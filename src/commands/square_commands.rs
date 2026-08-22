//! Square CSV ingest: turns the two reports we pull from the Square dashboard
//! (sales summary and pay-period payroll) into balanced double-entry journal
//! entries.
//!
//! Acquisition is handled by the browser extension (record/replay + CSV-download
//! interception, same machinery as bank imports). The extension POSTs the
//! downloaded file to `/import/square-sales-file` or `/import/square-payroll-file`,
//! and those handlers call into the `ingest_square_*` functions here.
//!
//! ## Sales summary format
//!
//! The sales-summary export is a **vertical key→value report**, not a table:
//! column 1 is a label, column 2 is a dollar amount, e.g.
//! ```text
//!   "Net sales","$1,489.43"
//!   "Taxes","$152.68"
//!   "Fees","($44.26)"
//!   "Net total","$1,597.85"
//! ```
//! The whole file is ONE export covering the date range selected on the
//! dashboard. The dates are not in the content — they're in the filename
//! (`sales-summary-2026-06-26-2026-06-26.csv`), so we parse the period from
//! there.
//!
//! ## Accounting model
//!
//! Money from card sales lands in the **Square balance** (the `pos_square`
//! mapped account), not the bank. Employees are paid out of that balance, and
//! employer/withheld taxes are remitted from **checking**. Because checking is
//! already on the Plaid bank feed, the payroll entry credits a
//! `payroll_taxes_payable` liability that the real checking withdrawal clears —
//! avoiding double-counting.
//!
//! Sales (one entry per export period):
//! ```text
//!   Dr  Square balance (pos_square)     net = revenue + tax + tips - fees   (= "Net total")
//!   Dr  Processing fees (square_fees)   fees
//!     Cr  Sales revenue (pos_revenue)       revenue (net sales)
//!     Cr  Sales tax payable                 tax        (only if > 0)
//!     Cr  Tips payable                      tips       (only if > 0)
//! ```
//!
//! Payroll (one entry per pay period):
//! ```text
//!   Dr  Wages expense (payroll_wages_expense)       gross_wages
//!   Dr  Employer tax expense (payroll_tax_expense)  employer_taxes
//!     Cr  Square balance (pos_square)               net_pay
//!     Cr  Payroll taxes payable                     gross_wages - net_pay + employer_taxes
//! ```

use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use crate::commands::import_commands::{parse_amount, parse_delimited_line};
use crate::commands::ingest_commands::{
    check_idempotent, load_ingest_mappings, post_ingest_entry, IngestError,
};
use crate::events::types::JournalEntrySource;
use crate::store::event_store::EventStore;
use calamine::{open_workbook, Data, Reader, Xlsx};
use chrono::NaiveDate;
use rusqlite::Connection;
use std::collections::HashMap;

// ===========================================================================
// >>> COLUMN MAPPING SEAM — finalize these against the real Square exports <<<
//
// SALES labels match column 1 of the vertical summary report (case-insensitive,
// exact then substring). PAYROLL columns are still best-guess — replace once a
// real payroll export is available.
// ===========================================================================

mod columns {
    // ---- Sales summary export (vertical key→value rows) ----
    pub const SALES_NET_SALES: &[&str] = &["net sales"];
    pub const SALES_GROSS_SALES: &[&str] = &["gross sales"];
    pub const SALES_RETURNS: &[&str] = &["returns"];
    pub const SALES_DISCOUNTS: &[&str] = &["discounts & comps", "discounts"];
    pub const SALES_TAX: &[&str] = &["taxes"];
    pub const SALES_TIPS: &[&str] = &["tips"];
    pub const SALES_FEES: &[&str] = &["fees", "square fees"];
    pub const SALES_NET_TOTAL: &[&str] = &["net total"];

    // ---- Payroll "Company Totals" matrix (labels in the header row; the
    //      authoritative figures live in the "Total" row). Value for a label is
    //      the first numeric cell in the Total row at/after the label's column,
    //      up to the next labelled column (tax sections put the value one column
    //      right of the label). ----
    pub const PAY_GROSS: &[&str] = &["pay"];
    pub const PAY_EMPLOYEE_TAXES: &[&str] = &["employee taxes"];
    pub const PAY_EMPLOYER_TAXES: &[&str] = &["employer taxes"];
    pub const PAY_NET: &[&str] = &["net pay"];
}

/// Outcome of an import: how many entries were posted, and how many periods were
/// skipped because an entry with that reference already existed.
#[derive(Debug, Default)]
pub struct SquareImportSummary {
    pub entries_posted: usize,
    pub skipped_duplicates: usize,
    pub rows_parsed: usize,
}

/// Find every `YYYY-MM-DD` in a string (e.g. a filename) and return the first
/// and last as a (start, end) period.
fn extract_period(name: &str) -> Option<(NaiveDate, NaiveDate)> {
    let mut found = Vec::new();
    let bytes = name.as_bytes();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        let slice = &name[i..i + 10];
        if slice.as_bytes()[4] == b'-' && slice.as_bytes()[7] == b'-' {
            if let Ok(d) = NaiveDate::parse_from_str(slice, "%Y-%m-%d") {
                found.push(d);
                i += 10;
                continue;
            }
        }
        i += 1;
    }
    match (found.first(), found.last()) {
        (Some(&s), Some(&e)) => Some((s, e)),
        _ => None,
    }
}

// ===========================================================================
// Sales
// ===========================================================================

#[derive(Debug, Default, Clone)]
struct SalesSummary {
    revenue: i64, // net sales (after discounts/returns)
    tax: i64,
    tips: i64,
    fees: i64, // positive magnitude
}

/// Look up a value from the label→amount map by candidate labels: exact
/// (lowercased) match first, then substring.
fn pick(map: &HashMap<String, i64>, candidates: &[&str]) -> Option<i64> {
    for c in candidates {
        if let Some(v) = map.get(&c.to_lowercase()) {
            return Some(*v);
        }
    }
    for c in candidates {
        let cl = c.to_lowercase();
        if let Some((_, v)) = map.iter().find(|(k, _)| k.contains(&cl)) {
            return Some(*v);
        }
    }
    None
}

/// Parse the vertical sales-summary report into a single summary.
fn parse_sales_summary(content: &str) -> SalesSummary {
    let mut map: HashMap<String, i64> = HashMap::new();
    for line in content.lines() {
        let fields = parse_delimited_line(line, ',');
        if fields.len() < 2 {
            continue;
        }
        let label = fields[0].trim().to_lowercase();
        if label.is_empty() {
            continue;
        }
        if let Some(amount) = parse_amount(fields[1].trim()) {
            // First occurrence wins (top-level "Fees" before the breakdown rows).
            map.entry(label).or_insert(amount);
        }
    }

    let revenue = pick(&map, columns::SALES_NET_SALES).unwrap_or_else(|| {
        pick(&map, columns::SALES_GROSS_SALES).unwrap_or(0)
            - pick(&map, columns::SALES_RETURNS).unwrap_or(0)
            - pick(&map, columns::SALES_DISCOUNTS).unwrap_or(0).abs()
    });

    let summary = SalesSummary {
        revenue,
        tax: pick(&map, columns::SALES_TAX).unwrap_or(0),
        tips: pick(&map, columns::SALES_TIPS).unwrap_or(0),
        fees: pick(&map, columns::SALES_FEES).unwrap_or(0).abs(),
    };

    // Integrity check: our net-to-balance should equal the report's own
    // "Net total". A mismatch means a column label drifted — surface it loudly
    // rather than silently posting an unbalanced-looking deposit.
    if let Some(reported_net) = pick(&map, columns::SALES_NET_TOTAL) {
        let computed = summary.revenue + summary.tax + summary.tips - summary.fees;
        if computed != reported_net {
            eprintln!(
                "square-sales: computed net {} != reported 'Net total' {} — check column mapping",
                computed, reported_net
            );
        }
    }

    summary
}

/// Ingest a Square sales-summary CSV: one balanced journal entry for the export
/// period (parsed from `file_name`), idempotent on `square-sales-<start>[_<end>]`.
/// Decide what a Square sales export posts, without writing anything.
///
/// The deciding half of [`ingest_square_sales`], over a plain `&Connection` so it
/// runs on a group replica — which may not append, its event ids being the
/// server's. `Ok((None, summary))` means there is nothing to post: an empty
/// period, or one already imported.
pub fn plan_square_sales(
    conn: &Connection,
    content: &str,
    file_name: &str,
) -> Result<(Option<PostEntryCommand>, SquareImportSummary), IngestError> {
    let (start, end) = extract_period(file_name).ok_or_else(|| {
        IngestError::InvalidDate(format!(
            "no YYYY-MM-DD period found in filename '{}'",
            file_name
        ))
    })?;

    let s = parse_sales_summary(content);
    let mut summary = SquareImportSummary {
        rows_parsed: 1,
        ..Default::default()
    };

    if s.revenue == 0 && s.tax == 0 && s.tips == 0 && s.fees == 0 {
        return Ok((None, summary));
    }

    let reference = if start == end {
        format!("square-sales-{}", start.format("%Y-%m-%d"))
    } else {
        format!(
            "square-sales-{}_{}",
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        )
    };
    if check_idempotent(conn, &reference).is_some() {
        summary.skipped_duplicates += 1;
        return Ok((None, summary));
    }

    let mut required = vec!["pos_square", "pos_revenue", "square_fees"];
    if s.tax > 0 {
        required.push("sales_tax_payable");
    }
    if s.tips > 0 {
        required.push("tips_payable");
    }
    let mappings = load_ingest_mappings(conn, &required)?;

    // Net change to the Square balance (matches the report's "Net total").
    let net_to_balance = s.revenue + s.tax + s.tips - s.fees;

    let mut lines = vec![
        EntryLine::debit(&mappings["pos_square"], net_to_balance, "USD")
            .with_memo("Square net deposit"),
    ];
    if s.fees != 0 {
        lines.push(
            EntryLine::debit(&mappings["square_fees"], s.fees, "USD")
                .with_memo("Square processing fees"),
        );
    }
    lines.push(
        EntryLine::credit(&mappings["pos_revenue"], s.revenue, "USD").with_memo("Sales revenue"),
    );
    if s.tax > 0 {
        lines.push(
            EntryLine::credit(&mappings["sales_tax_payable"], s.tax, "USD")
                .with_memo("Sales tax collected"),
        );
    }
    if s.tips > 0 {
        lines.push(
            EntryLine::credit(&mappings["tips_payable"], s.tips, "USD").with_memo("Tips collected"),
        );
    }

    let memo = if start == end {
        format!("Square sales {}", start.format("%Y-%m-%d"))
    } else {
        format!(
            "Square sales {} – {}",
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        )
    };

    Ok((
        Some(PostEntryCommand {
            date: end,
            memo,
            lines,
            reference: Some(reference),
            source: Some(JournalEntrySource::Pos),
        }),
        summary,
    ))
}

pub fn ingest_square_sales(
    store: &mut EventStore,
    user_id: &str,
    content: &str,
    file_name: &str,
) -> Result<SquareImportSummary, IngestError> {
    let (cmd, mut summary) = plan_square_sales(store.connection(), content, file_name)?;
    let Some(cmd) = cmd else { return Ok(summary) };

    let mut commands = EntryCommands::new(store, user_id.to_string());
    // A concurrent import that won the race after our pre-check is rejected
    // in-txn as a duplicate; count it as skipped rather than erroring.
    let (_, was_duplicate) = post_ingest_entry(&mut commands, cmd)?;
    if was_duplicate {
        summary.skipped_duplicates += 1;
    } else {
        summary.entries_posted += 1;
    }
    Ok(summary)
}

// ===========================================================================
// Payroll  (Square "Company Totals" report — an .xlsx summary matrix)
// ===========================================================================

#[derive(Debug, Default, Clone)]
struct PayrollTotals {
    gross: i64,          // "Pay"
    employee_taxes: i64, // "Employee Taxes"
    employer_taxes: i64, // "Employer Taxes"
    net_pay: i64,        // "Net Pay"
}

/// Stringify a cell so the same numeric/text parsing (`parse_amount`) applies.
fn data_to_string(d: &Data) -> String {
    match d {
        Data::String(s) => s.clone(),
        Data::Float(f) => format!("{}", f),
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Parse the "Company Totals" xlsx: locate the header row and the "Total" row,
/// then read the company-wide figures we post.
fn parse_company_totals_xlsx(path: &str) -> Result<PayrollTotals, IngestError> {
    let mut wb: Xlsx<_> = open_workbook(path)
        .map_err(|e| IngestError::EntryError(format!("open xlsx '{}': {}", path, e)))?;
    let range = wb
        .worksheet_range_at(0)
        .ok_or_else(|| IngestError::MissingMapping("payroll xlsx: no worksheets".to_string()))?
        .map_err(|e| IngestError::EntryError(format!("read xlsx sheet: {}", e)))?;

    let rows: Vec<Vec<String>> = range
        .rows()
        .map(|r| r.iter().map(data_to_string).collect())
        .collect();

    let header = rows
        .iter()
        .find(|r| r.iter().any(|c| c.trim().eq_ignore_ascii_case("net pay")))
        .ok_or_else(|| {
            IngestError::MissingMapping("payroll xlsx: no header row with 'Net Pay'".to_string())
        })?;
    let total = rows
        .iter()
        .find(|r| r.iter().any(|c| c.trim().eq_ignore_ascii_case("total")))
        .ok_or_else(|| IngestError::MissingMapping("payroll xlsx: no 'Total' row".to_string()))?;

    // Columns that carry a label in the header row, in order.
    let header_cols: Vec<usize> = header
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.trim().is_empty())
        .map(|(i, _)| i)
        .collect();

    let col_of = |candidates: &[&str]| -> Option<usize> {
        candidates.iter().find_map(|cand| {
            header
                .iter()
                .position(|c| c.trim().eq_ignore_ascii_case(cand))
        })
    };

    // The Total-row value for a label: first parseable amount at/after the
    // label's column, up to the next labelled column.
    let value_for = |candidates: &[&str]| -> i64 {
        let Some(start) = col_of(candidates) else {
            return 0;
        };
        let next = header_cols
            .iter()
            .copied()
            .find(|&c| c > start)
            .unwrap_or(total.len());
        for cell in total.iter().take(next.min(total.len())).skip(start) {
            let t = cell.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(cents) = parse_amount(t) {
                return cents;
            }
        }
        0
    };

    Ok(PayrollTotals {
        gross: value_for(columns::PAY_GROSS),
        employee_taxes: value_for(columns::PAY_EMPLOYEE_TAXES),
        employer_taxes: value_for(columns::PAY_EMPLOYER_TAXES),
        net_pay: value_for(columns::PAY_NET),
    })
}

/// Ingest a Square payroll "Company Totals" xlsx: one balanced journal entry for
/// the report period (parsed from the filename), idempotent on
/// `square-payroll-<start>[_<end>]`.
/// Decide what a Square payroll export posts, without writing anything. The
/// deciding half of [`ingest_square_payroll`]; see [`plan_square_sales`].
pub fn plan_square_payroll(
    conn: &Connection,
    file_path: &str,
) -> Result<(Option<PostEntryCommand>, SquareImportSummary), IngestError> {
    let (start, end) = extract_period(file_path).ok_or_else(|| {
        IngestError::InvalidDate(format!(
            "no YYYY-MM-DD period found in filename '{}'",
            file_path
        ))
    })?;

    let t = parse_company_totals_xlsx(file_path)?;
    let mut summary = SquareImportSummary {
        rows_parsed: 1,
        ..Default::default()
    };

    if t.gross == 0 && t.net_pay == 0 {
        return Ok((None, summary));
    }

    let reference = if start == end {
        format!("square-payroll-{}", start.format("%Y-%m-%d"))
    } else {
        format!(
            "square-payroll-{}_{}",
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        )
    };
    if check_idempotent(conn, &reference).is_some() {
        summary.skipped_duplicates += 1;
        return Ok((None, summary));
    }

    let mappings = load_ingest_mappings(
        conn,
        &[
            "payroll_wages_expense",
            "payroll_tax_expense",
            "pos_square",
            "payroll_taxes_payable",
        ],
    )?;

    // What leaves checking for the IRS/state = employee withholdings + employer
    // taxes. The derived form (gross - net + employer) keeps the entry balanced
    // even if a future export carries post-tax deductions/benefits — but those
    // aren't taxes, so warn if the two disagree.
    let other = (t.gross - t.net_pay) - t.employee_taxes;
    if other != 0 {
        eprintln!(
            "square-payroll {}: {} cents of non-tax deductions/benefits detected and folded into \
             payroll taxes payable — review this entry",
            reference, other
        );
    }
    let taxes_payable = t.gross - t.net_pay + t.employer_taxes;

    let mut lines = vec![
        EntryLine::debit(&mappings["payroll_wages_expense"], t.gross, "USD")
            .with_memo("Gross wages"),
    ];
    if t.employer_taxes != 0 {
        lines.push(
            EntryLine::debit(&mappings["payroll_tax_expense"], t.employer_taxes, "USD")
                .with_memo("Employer payroll taxes"),
        );
    }
    lines.push(
        EntryLine::credit(&mappings["pos_square"], t.net_pay, "USD")
            .with_memo("Net pay (from Square balance)"),
    );
    if taxes_payable != 0 {
        lines.push(
            EntryLine::credit(&mappings["payroll_taxes_payable"], taxes_payable, "USD")
                .with_memo("Payroll taxes payable (remitted from checking)"),
        );
    }

    let memo = if start == end {
        format!("Square payroll {}", start.format("%Y-%m-%d"))
    } else {
        format!(
            "Square payroll {} – {}",
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        )
    };

    Ok((
        Some(PostEntryCommand {
            date: end,
            memo,
            lines,
            reference: Some(reference),
            source: Some(JournalEntrySource::Import),
        }),
        summary,
    ))
}

pub fn ingest_square_payroll(
    store: &mut EventStore,
    user_id: &str,
    file_path: &str,
) -> Result<SquareImportSummary, IngestError> {
    let (cmd, mut summary) = plan_square_payroll(store.connection(), file_path)?;
    let Some(cmd) = cmd else { return Ok(summary) };

    let mut commands = EntryCommands::new(store, user_id.to_string());
    // A concurrent import that won the race after our pre-check is rejected
    // in-txn as a duplicate; count it as skipped rather than erroring.
    let (_, was_duplicate) = post_ingest_entry(&mut commands, cmd)?;
    if was_duplicate {
        summary.skipped_duplicates += 1;
    } else {
        summary.entries_posted += 1;
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_sales_summary() {
        let csv = "\"Sales summary - Summary\nAll day (12:00 AM-11:59 PM CT)\",\" \"\n\
\"Gross sales\",\"$1,498.28\"\n\
\"Returns\",\"$0.00\"\n\
\"Discounts & comps\",\"($8.85)\"\n\
\"Net sales\",\"$1,489.43\"\n\
\"Taxes\",\"$152.68\"\n\
\"Tips\",\"$0.00\"\n\
\"Card\",\"$1,642.11\"\n\
\"Fees\",\"($44.26)\"\n\
\"Square fees\",\"($44.26)\"\n\
\"Net total\",\"$1,597.85\"\n";
        let s = parse_sales_summary(csv);
        assert_eq!(s.revenue, 148943);
        assert_eq!(s.tax, 15268);
        assert_eq!(s.tips, 0);
        assert_eq!(s.fees, 4426);
        // Net to Square balance must equal the report's "Net total".
        assert_eq!(s.revenue + s.tax + s.tips - s.fees, 159785);
    }

    #[test]
    fn extracts_period_from_filename() {
        let (start, end) = extract_period("sales-summary-2026-06-26-2026-06-26.csv").unwrap();
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 6, 26).unwrap());
        assert_eq!(end, NaiveDate::from_ymd_opt(2026, 6, 26).unwrap());

        let (s2, e2) = extract_period("Company-Totals-2026-06-01-2026-06-30-.xlsx").unwrap();
        assert_eq!(s2, NaiveDate::from_ymd_opt(2026, 6, 1).unwrap());
        assert_eq!(e2, NaiveDate::from_ymd_opt(2026, 6, 30).unwrap());
    }

    #[test]
    fn parses_real_company_totals_xlsx() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sampledata/Company-Totals-2026-06-01-2026-06-30-.xlsx"
        );
        if !std::path::Path::new(path).exists() {
            return; // sample not present in this checkout — skip
        }
        let t = parse_company_totals_xlsx(path).unwrap();
        assert_eq!(t.gross, 525385);
        assert_eq!(t.employee_taxes, 90140);
        assert_eq!(t.employer_taxes, 46489);
        assert_eq!(t.net_pay, 435245);
        // Net pay = gross - employee taxes (no other deductions in this sample).
        assert_eq!(t.gross - t.employee_taxes, t.net_pay);
        // Taxes remitted from checking = employee + employer taxes.
        assert_eq!(
            t.gross - t.net_pay + t.employer_taxes,
            t.employee_taxes + t.employer_taxes
        );
    }
}

// ---------------------------------------------------------------------------
// The monthly settlement, when the POS already booked the sales
// ---------------------------------------------------------------------------

/// What a Square summary says, against what the books already hold for the
/// period.
///
/// The two arrive from different directions and neither is the other's check by
/// accident: the POS knows what was sold, Square knows what it settled and what
/// it charged for doing so. Showing both is what makes a disagreement findable —
/// a till left open, a sale voided at one end and not the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquareSettlement {
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// Gross sales Square reports for the period.
    pub reported_revenue: i64,
    pub reported_tax: i64,
    pub reported_tips: i64,
    pub fees: i64,
    /// Everything Square collected: revenue + tax + tips, before it kept its cut.
    pub reported_gross: i64,
    /// What the books show arriving in the Square balance over the period —
    /// debits only, so this crate's own fee credit does not net against it.
    pub books_square_in: i64,
    /// `reported_gross - books_square_in`. Zero means the two agree.
    ///
    /// Compared on the **tender** side rather than on revenue, because revenue
    /// cannot be attributed to a payment method: every sale credits one revenue
    /// account whatever it was paid with, and a split-tender sale credits it once
    /// for a total that arrived partly in cash. Comparing Square's report against
    /// that account showed every cash sale as a discrepancy.
    pub difference: i64,
}

/// Post only Square's fees for a period, leaving the sales to the POS.
///
/// # Why this exists
///
/// [`plan_square_sales`] posts the whole picture — revenue, tax, tips, fees and
/// the net to the Square balance. That is right when Square is the only source.
/// It is wrong the moment a POS is publishing daily sales totals into the same
/// books, because then the revenue is already there and posting it again doubles
/// it.
///
/// So this posts the one thing the POS cannot know: what Square kept.
///
/// ```text
///   Dr  Processing fees      fees
///   Cr  Square balance       fees
/// ```
///
/// The Square balance is credited because the POS rollups debited it with the
/// gross, and Square only ever deposited the net. After both, the balance equals
/// what Square actually holds — which is the figure a reconciliation against
/// their statement can then be run on.
///
/// Returns the entry and the comparison. A period with no fees posts nothing and
/// still reports the comparison: "Square charged nothing" is an answer, and the
/// revenue check is the reason to look.
pub fn plan_square_fees(
    conn: &Connection,
    content: &str,
    file_name: &str,
) -> Result<(Option<PostEntryCommand>, SquareSettlement), IngestError> {
    let (start, end) = extract_period(file_name).ok_or_else(|| {
        IngestError::InvalidDate(format!(
            "no YYYY-MM-DD period found in filename '{}'",
            file_name
        ))
    })?;
    let s = parse_sales_summary(content);

    let required = ["pos_square", "square_fees"];
    let mappings = load_ingest_mappings(conn, &required)?;

    // What arrived in the Square balance over the period, from the books.
    //
    // Debits only, and deliberately: a credit to this account is money leaving
    // it — this crate's own fee entry, or a transfer out to the bank — and
    // netting those against what came in would compare Square's gross to
    // something else entirely. Voided entries are excluded; they are not money.
    let books_square_in: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(jl.amount), 0)
               FROM journal_lines jl
               JOIN journal_entries je ON jl.entry_id = je.id
              WHERE jl.account_id = ?1 AND jl.amount > 0 AND je.is_void = 0
                AND je.date >= ?2 AND je.date <= ?3",
            rusqlite::params![&mappings["pos_square"], start.to_string(), end.to_string()],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Everything Square collected before its cut — which is what the POS booked
    // as arriving in the Square balance.
    let reported_gross = s.revenue + s.tax + s.tips;

    let settlement = SquareSettlement {
        start,
        end,
        reported_revenue: s.revenue,
        reported_tax: s.tax,
        reported_tips: s.tips,
        fees: s.fees,
        reported_gross,
        books_square_in,
        difference: reported_gross - books_square_in,
    };

    if s.fees == 0 {
        return Ok((None, settlement));
    }

    // Its own key space. A fees-only entry and a full summary for the same
    // period are different postings, and sharing a reference would let one be
    // mistaken for the other — silently leaving the revenue unposted or posted
    // twice, depending which came first.
    let reference = format!(
        "square-fees-{}_{}",
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d")
    );
    if check_idempotent(conn, &reference).is_some() {
        return Ok((None, settlement));
    }

    let lines = vec![
        EntryLine::debit(&mappings["square_fees"], s.fees, "USD")
            .with_memo("Square processing fees"),
        EntryLine::credit(&mappings["pos_square"], s.fees, "USD")
            .with_memo("Kept by Square from settlements"),
    ];

    Ok((
        Some(PostEntryCommand {
            date: end,
            memo: format!(
                "Square fees {} – {}",
                start.format("%Y-%m-%d"),
                end.format("%Y-%m-%d")
            ),
            lines,
            reference: Some(reference),
            source: Some(JournalEntrySource::Pos),
        }),
        settlement,
    ))
}

/// The monthly settlement, when a POS is already posting the sales.
#[cfg(test)]
mod settlement_tests {
    use super::*;
    use crate::commands::event_service_commands::planning_tests::books;
    use crate::commands::ingest_commands::set_account_mapping;

    const SUMMARY: &str = "\"Sales summary - Summary\",\" \"\n\
\"Gross sales\",\"$1,498.28\"\n\
\"Net sales\",\"$1,489.43\"\n\
\"Taxes\",\"$152.68\"\n\
\"Tips\",\"$0.00\"\n\
\"Fees\",\"($44.26)\"\n\
\"Net total\",\"$1,597.85\"\n";

    fn ready() -> EventStore {
        let mut store = books();
        // A Square balance account of its own. Mapping it onto the cash account —
        // which `books()` already uses for `pos_cash` — would make a cash sale and
        // a Square sale land in the same place, and every test about telling the
        // two apart would pass for the wrong reason.
        let created = crate::events::types::Event::AccountCreated {
            account_id: "sq".to_string(),
            account_type: crate::events::types::EventAccountType::Asset,
            account_number: "1100".to_string(),
            name: "Square balance".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        let stored = store
            .append(crate::events::types::EventEnvelope::new(
                created,
                "test".to_string(),
            ))
            .unwrap();
        crate::store::projections::ProjectionStore::apply_projection(&mut store, &stored).unwrap();

        let conn = store.connection();
        set_account_mapping(conn, "pos_square", "sq").unwrap();
        set_account_mapping(conn, "square_fees", "cogs").unwrap();
        // The full-summary planner needs these two as well; the fees-only one
        // deliberately does not, which is part of what makes it lighter to run.
        set_account_mapping(conn, "sales_tax_payable", "ap").unwrap();
        set_account_mapping(conn, "tips_payable", "ap").unwrap();
        store
    }

    /// **The point of the whole fees-only mode.**
    ///
    /// The POS rollups already posted the revenue. A full Square summary would
    /// post it again, and a month's sales would appear twice with nothing on
    /// either entry to say the other existed. Fees-only posts the one figure the
    /// POS cannot know.
    #[test]
    fn a_fees_only_entry_does_not_touch_revenue() {
        let store = ready();
        let (cmd, settlement) = plan_square_fees(
            store.connection(),
            SUMMARY,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .expect("plan");

        let cmd = cmd.expect("fees were charged, so there is an entry");
        assert_eq!(cmd.lines.len(), 2, "fees and the balance, nothing else");
        assert_eq!(settlement.fees, 4426);

        // The revenue account must not appear at all.
        let revenue_account = crate::commands::ingest_commands::load_ingest_mappings(
            store.connection(),
            &["pos_revenue"],
        )
        .unwrap()["pos_revenue"]
            .clone();
        assert!(
            cmd.lines.iter().all(|l| l.account_id != revenue_account),
            "a fees-only entry posted revenue, which the POS already booked"
        );

        // And it balances, which a two-line entry only does if the signs are right.
        assert_eq!(cmd.lines.iter().map(|l| l.amount).sum::<i64>(), 0);
    }

    /// The Square balance is credited, not debited.
    ///
    /// The POS rollups debited it with the gross; Square only ever deposited the
    /// net. Crediting the difference is what leaves the account equal to what
    /// Square actually holds — get the sign wrong and the balance is off by twice
    /// the fees, which reconciles against nothing.
    #[test]
    fn the_fees_come_out_of_the_square_balance() {
        let store = ready();
        let (cmd, _) = plan_square_fees(
            store.connection(),
            SUMMARY,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .unwrap();
        let cmd = cmd.unwrap();

        let square = crate::commands::ingest_commands::load_ingest_mappings(
            store.connection(),
            &["pos_square"],
        )
        .unwrap()["pos_square"]
            .clone();
        let square_line = cmd
            .lines
            .iter()
            .find(|l| l.account_id == square)
            .expect("the Square balance must be on the entry");
        assert!(
            square_line.amount < 0,
            "the Square balance must be credited: {square_line:?}"
        );
        assert_eq!(square_line.amount, -4426);
    }

    /// It reports what Square says against what the books hold, which is the
    /// check the user actually runs each month.
    #[test]
    fn the_settlement_compares_square_against_the_books() {
        let store = ready();
        let (_, settlement) = plan_square_fees(
            store.connection(),
            SUMMARY,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .unwrap();

        assert_eq!(settlement.reported_revenue, 148943);
        assert_eq!(
            settlement.reported_gross,
            148943 + 15268,
            "revenue plus tax"
        );
        // Nothing posted in this period yet, so the whole of it is the gap — and
        // saying so is the point: it is how a missing day of POS totals shows up.
        assert_eq!(settlement.books_square_in, 0);
        assert_eq!(settlement.difference, 164211);
    }

    /// **A shop that also takes cash must still reconcile.**
    ///
    /// The first version of this compared Square's reported revenue against the
    /// whole revenue account — and every sale credits that account whatever it
    /// was paid with. So a shop taking cash saw its cash sales as a discrepancy,
    /// every month, with nothing wrong.
    ///
    /// Revenue cannot be attributed to a payment method at all: a split-tender
    /// sale credits revenue once for a total that arrived partly in cash. The
    /// tender side can, because that is where the split lives — so the comparison
    /// is Square's gross against what actually landed in the Square balance.
    #[test]
    fn cash_sales_are_not_counted_as_a_square_discrepancy() {
        let mut store = ready();
        let conn_accounts = crate::commands::ingest_commands::load_ingest_mappings(
            store.connection(),
            &["pos_square", "pos_cash", "pos_revenue"],
        )
        .unwrap();

        // A day's takings: 1,000.00 through Square and 500.00 in cash. One
        // revenue credit for the lot, as a real rollup posts it.
        crate::commands::entry_commands::EntryCommands::new(&mut store, "t".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2026, 6, 15).unwrap(),
                memo: "POS daily total".to_string(),
                lines: vec![
                    EntryLine::debit(&conn_accounts["pos_square"], 100_000, "USD"),
                    EntryLine::debit(&conn_accounts["pos_cash"], 50_000, "USD"),
                    EntryLine::credit(&conn_accounts["pos_revenue"], 150_000, "USD"),
                ],
                reference: Some("pos:2026-06-15".to_string()),
                source: None,
            })
            .unwrap();

        // Square reports only what went through Square: 1,000.00, no tax or tips.
        let summary = "\"Sales summary - Summary\",\" \"\n\
\"Net sales\",\"$1,000.00\"\n\
\"Taxes\",\"$0.00\"\n\
\"Tips\",\"$0.00\"\n\
\"Fees\",\"($30.00)\"\n";

        let (_, settlement) = plan_square_fees(
            store.connection(),
            summary,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .unwrap();

        assert_eq!(
            settlement.books_square_in, 100_000,
            "only the Square tender"
        );
        assert_eq!(settlement.reported_gross, 100_000);
        assert_eq!(
            settlement.difference, 0,
            "the cash sales were counted as a Square discrepancy"
        );
    }

    /// The fee entry does not net against what came in.
    ///
    /// It credits the Square balance, so summing the account's movement would
    /// subtract the fees from the gross and show a difference of exactly the
    /// fees — every month, once the entry had posted.
    #[test]
    fn the_fee_entry_does_not_skew_the_comparison() {
        let mut store = ready();
        let accounts = crate::commands::ingest_commands::load_ingest_mappings(
            store.connection(),
            &["pos_square", "pos_revenue", "square_fees"],
        )
        .unwrap();

        crate::commands::entry_commands::EntryCommands::new(&mut store, "t".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2026, 6, 15).unwrap(),
                memo: "POS daily total".to_string(),
                lines: vec![
                    EntryLine::debit(&accounts["pos_square"], 100_000, "USD"),
                    EntryLine::credit(&accounts["pos_revenue"], 100_000, "USD"),
                ],
                reference: Some("pos:2026-06-15".to_string()),
                source: None,
            })
            .unwrap();
        // And the fees, as this module would post them.
        crate::commands::entry_commands::EntryCommands::new(&mut store, "t".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
                memo: "Square fees".to_string(),
                lines: vec![
                    EntryLine::debit(&accounts["square_fees"], 3_000, "USD"),
                    EntryLine::credit(&accounts["pos_square"], 3_000, "USD"),
                ],
                reference: Some("square-fees-already".to_string()),
                source: None,
            })
            .unwrap();

        let summary = "\"Sales summary - Summary\",\" \"\n\
\"Net sales\",\"$1,000.00\"\n\
\"Taxes\",\"$0.00\"\n\
\"Tips\",\"$0.00\"\n\
\"Fees\",\"($30.00)\"\n";
        let (_, settlement) = plan_square_fees(
            store.connection(),
            summary,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .unwrap();

        assert_eq!(settlement.books_square_in, 100_000, "debits only");
        assert_eq!(settlement.difference, 0);
    }

    /// A fees-only entry and a full summary must not be mistaken for each other.
    ///
    /// They post different things for the same period. Sharing an idempotency key
    /// would mean whichever ran second was silently skipped — leaving the revenue
    /// either unposted or posted twice, depending on the order.
    #[test]
    fn fees_and_a_full_summary_do_not_share_a_key() {
        let store = ready();
        let (fees, _) = plan_square_fees(
            store.connection(),
            SUMMARY,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .unwrap();
        let (full, _) = plan_square_sales(
            store.connection(),
            SUMMARY,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .unwrap();

        let fees_ref = fees.unwrap().reference.unwrap();
        let full_ref = full.unwrap().reference.unwrap();
        assert_ne!(fees_ref, full_ref, "one would swallow the other");
    }

    /// A month Square charged nothing for posts nothing, and still reports.
    #[test]
    fn a_period_with_no_fees_posts_nothing_but_still_compares() {
        let store = ready();
        let no_fees = SUMMARY.replace("\"Fees\",\"($44.26)\"", "\"Fees\",\"$0.00\"");
        let (cmd, settlement) = plan_square_fees(
            store.connection(),
            &no_fees,
            "sales-summary-2026-06-01-2026-06-30.csv",
        )
        .unwrap();

        assert!(cmd.is_none(), "no fees, no entry");
        assert_eq!(settlement.reported_revenue, 148943, "still worth comparing");
    }
}
