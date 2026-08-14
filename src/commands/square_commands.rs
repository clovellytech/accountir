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
use rusqlite::Connection;
use calamine::{open_workbook, Data, Reader, Xlsx};
use chrono::NaiveDate;
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

    let mut lines = vec![EntryLine::debit(&mappings["pos_square"], net_to_balance, "USD")
        .with_memo("Square net deposit")];
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
            EntryLine::credit(&mappings["tips_payable"], s.tips, "USD")
                .with_memo("Tips collected"),
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

    Ok((Some(PostEntryCommand {
        date: end,
        memo,
        lines,
        reference: Some(reference),
        source: Some(JournalEntrySource::Pos),
    }), summary))
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
        .ok_or_else(|| {
            IngestError::MissingMapping("payroll xlsx: no 'Total' row".to_string())
        })?;

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
        EntryLine::debit(&mappings["payroll_wages_expense"], t.gross, "USD").with_memo("Gross wages"),
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
        let (start, end) =
            extract_period("sales-summary-2026-06-26-2026-06-26.csv").unwrap();
        assert_eq!(start, NaiveDate::from_ymd_opt(2026, 6, 26).unwrap());
        assert_eq!(end, NaiveDate::from_ymd_opt(2026, 6, 26).unwrap());

        let (s2, e2) =
            extract_period("Company-Totals-2026-06-01-2026-06-30-.xlsx").unwrap();
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
