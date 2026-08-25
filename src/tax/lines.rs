//! Which Form 1065 line each ledger account is reported on, and what the lines
//! then add up to.
//!
//! # Why the mapping is keyed by account
//!
//! The ingest mappings ([`crate::commands::ingest_commands::MAPPING_DEFS`]) go
//! key → account, because there is exactly one Square balance account. A return
//! is the other shape: a chart of accounts has a dozen expense accounts that all
//! land on line 21, and one row per line could only ever name one of them.
//!
//! Keying by account also makes the question that matters answerable: *which
//! accounts have no line?* An account carrying a balance and no mapping is
//! income or expense that silently vanishes off the return — the return still
//! foots, it is just wrong — so [`compute`] enumerates them and says so.
//!
//! # Why the derived lines are computed and not mapped
//!
//! Lines 1c, 3, 8, 16c, 22 and 23 are arithmetic on the lines above them. Left
//! mappable, somebody eventually points an account at line 23 and the return
//! shows an ordinary business income that is not what lines 8 and 22 make it.
//! That return is worse than one with no figures at all, because it looks
//! finished and its own internal check passes only by not being made. So the
//! totals here are computed, always, from the same rounded figures the form
//! shows.
//!
//! # Rounding
//!
//! The ledger holds integer cents; the IRS permits whole dollars provided every
//! amount is rounded. Rounding happens **once per line**, after that line's
//! accounts are summed in cents — not per account, which would accumulate a
//! cent of error for every account in the chart — and every derived line is
//! computed from the already-rounded dollars. That ordering is what makes the
//! arithmetic on the printed page come out exactly: see
//! [`tests::the_totals_on_the_page_are_the_arithmetic_of_the_lines_on_the_page`].

use crate::queries::reports::IncomeStatement;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

/// Whether a line reports its accounts as they stand, or as a figure subtracted
/// from the line above it.
///
/// The form's "less …" lines — 1b returns and allowances, 16b depreciation
/// reported elsewhere — print a *positive* number that is then taken away. The
/// accounts behind them are contra accounts, which carry the opposite sign to
/// their type, so their oriented balance is negative. Negating at the line makes
/// the box read the way the form means it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sense {
    /// The line shows the sum of its accounts as oriented.
    Natural,
    /// The line shows the sum negated — a "less …" line.
    Contra,
}

/// A Form 1065 line that accounts can be pointed at.
///
/// Comparable because a picker holds one as its selected value and has to
/// recognise it again. Every field is `Copy` and the whole table is `const`, so
/// equality is identity here: two defs are equal exactly when they are the same
/// entry of [`MAPPABLE_LINES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaxLineDef {
    /// Stored in `tax_line_mappings.line_key`.
    pub key: &'static str,
    /// The line as the form numbers it, e.g. "1a", "16b".
    pub number: &'static str,
    /// The form's own wording, for a mapping editor to show.
    pub label: &'static str,
    /// "Income" or "Deductions" — the form's two page-1 blocks.
    pub group: &'static str,
    pub sense: Sense,
}

/// Every line an account may be mapped to.
///
/// The single source of truth, shared by the mapping editor and by [`compute`].
/// Derived lines are deliberately absent — see the module docs.
pub const MAPPABLE_LINES: &[TaxLineDef] = &[
    TaxLineDef { key: "l1a", number: "1a", label: "Gross receipts or sales", group: "Income", sense: Sense::Natural },
    TaxLineDef { key: "l1b", number: "1b", label: "Less returns and allowances", group: "Income", sense: Sense::Contra },
    TaxLineDef { key: "l2", number: "2", label: "Cost of goods sold", group: "Income", sense: Sense::Natural },
    TaxLineDef { key: "l4", number: "4", label: "Ordinary income (loss) from other partnerships, estates, and trusts", group: "Income", sense: Sense::Natural },
    TaxLineDef { key: "l5", number: "5", label: "Net farm profit (loss)", group: "Income", sense: Sense::Natural },
    TaxLineDef { key: "l6", number: "6", label: "Net gain (loss) from Form 4797", group: "Income", sense: Sense::Natural },
    TaxLineDef { key: "l7", number: "7", label: "Other income (loss)", group: "Income", sense: Sense::Natural },
    TaxLineDef { key: "l9", number: "9", label: "Salaries and wages (other than to partners)", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l10", number: "10", label: "Guaranteed payments to partners", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l11", number: "11", label: "Repairs and maintenance", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l12", number: "12", label: "Bad debts", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l13", number: "13", label: "Rent", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l14", number: "14", label: "Taxes and licenses", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l15", number: "15", label: "Interest", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l16a", number: "16a", label: "Depreciation (if required, attach Form 4562)", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l16b", number: "16b", label: "Less depreciation reported on Form 1125-A and elsewhere", group: "Deductions", sense: Sense::Contra },
    TaxLineDef { key: "l17", number: "17", label: "Depletion (do not deduct oil and gas depletion)", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l18", number: "18", label: "Retirement plans, etc.", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l19", number: "19", label: "Employee benefit programs", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l20", number: "20", label: "Energy efficient commercial buildings deduction", group: "Deductions", sense: Sense::Natural },
    TaxLineDef { key: "l21", number: "21", label: "Other deductions", group: "Deductions", sense: Sense::Natural },
];

/// Look a line definition up by its stored key.
pub fn line_def(key: &str) -> Option<&'static TaxLineDef> {
    MAPPABLE_LINES.iter().find(|d| d.key == key)
}

/// Every valid line key.
pub fn line_keys() -> Vec<&'static str> {
    MAPPABLE_LINES.iter().map(|d| d.key).collect()
}

/// Page one's income and deduction lines, in whole dollars.
///
/// Whole dollars because that is what reaches the form; carrying cents here and
/// rounding at the point of printing would let the derived totals be computed
/// from figures the reader never sees, which is the exact failure the module
/// docs describe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form1065Lines {
    /// Mapped lines, by key, in whole dollars. Absent means no account was
    /// pointed at that line — distinct from mapped-and-zero.
    mapped: BTreeMap<&'static str, i64>,
}

impl Form1065Lines {
    /// A mapped line's figure, or zero when nothing was mapped to it.
    ///
    /// Zero rather than `None` because every arithmetic use of a line wants it
    /// to behave as nothing, and forcing each caller to unwrap invites one of
    /// them to unwrap it differently.
    pub fn get(&self, key: &str) -> i64 {
        self.mapped.get(key).copied().unwrap_or(0)
    }

    /// Whether an account was actually pointed at this line.
    pub fn is_mapped(&self, key: &str) -> bool {
        self.mapped.contains_key(key)
    }

    // --- derived lines: arithmetic on the rounded dollars above ---

    /// 1c. Balance — gross receipts less returns and allowances.
    pub fn line_1c(&self) -> i64 {
        self.get("l1a") - self.get("l1b")
    }

    /// 3. Gross profit — line 1c less cost of goods sold.
    pub fn line_3(&self) -> i64 {
        self.line_1c() - self.get("l2")
    }

    /// 8. Total income (loss) — combine lines 3 through 7.
    pub fn line_8(&self) -> i64 {
        self.line_3() + self.get("l4") + self.get("l5") + self.get("l6") + self.get("l7")
    }

    /// 16c. Depreciation claimed here — 16a less what is reported elsewhere.
    pub fn line_16c(&self) -> i64 {
        self.get("l16a") - self.get("l16b")
    }

    /// 22. Total deductions — lines 9 through 21, taking 16c rather than 16a.
    ///
    /// 16c and not 16a: 16a is depreciation in total and 16b is the part already
    /// deducted through cost of goods sold. Adding 16a would deduct that part
    /// twice, which is the single easiest way to overstate a deduction on this
    /// page.
    pub fn line_22(&self) -> i64 {
        self.get("l9")
            + self.get("l10")
            + self.get("l11")
            + self.get("l12")
            + self.get("l13")
            + self.get("l14")
            + self.get("l15")
            + self.line_16c()
            + self.get("l17")
            + self.get("l18")
            + self.get("l19")
            + self.get("l20")
            + self.get("l21")
    }

    /// 23. Ordinary business income (loss) — total income less total deductions.
    pub fn line_23(&self) -> i64 {
        self.line_8() - self.line_22()
    }

    /// Set a line directly. Tests only — the real path is [`compute`], which is
    /// the only thing that knows the rounding order the totals depend on.
    #[cfg(test)]
    pub fn set_for_test(&mut self, key: &str, dollars: i64) {
        let def = line_def(key).expect("test used a line key the form does not have");
        self.mapped.insert(def.key, dollars);
    }

    /// Whether anything at all was mapped.
    ///
    /// An unmapped ledger produces an identity-only return rather than a page of
    /// zeros, which is the honest rendering of "nobody has done this yet".
    pub fn is_empty(&self) -> bool {
        self.mapped.is_empty()
    }
}

/// Cents → whole dollars, rounded half away from zero.
///
/// Away from zero rather than to-even so that a loss and a profit of the same
/// magnitude round to the same magnitude; banker's rounding would make the sign
/// of a figure change how big it is, which is impossible to explain to anybody
/// reading the return.
pub fn cents_to_dollars(cents: i64) -> i64 {
    let (sign, abs) = if cents < 0 { (-1, -cents) } else { (1, cents) };
    sign * ((abs + 50) / 100)
}

/// A figure as it should appear in a form box.
///
/// Losses carry a leading minus rather than parentheses: the boxes the form
/// pre-prints parentheses around already have them printed, and adding a second
/// pair inside one of those reads as a nested negative.
pub fn format_dollars(dollars: i64) -> String {
    dollars.to_string()
}

/// What [`compute`] found that somebody should see before filing.
pub struct ComputedLines {
    pub lines: Form1065Lines,
    /// Accounts carrying a balance that no line claims — income and expense that
    /// would otherwise leave the return without trace.
    pub warnings: Vec<String>,
}

/// Total a period's income statement onto page one's lines.
///
/// `mapping` is account id → line key, as [`load_mapping`] returns it.
pub fn compute(
    statement: &IncomeStatement,
    mapping: &BTreeMap<String, String>,
) -> ComputedLines {
    // Sum in cents, one bucket per line, so rounding happens once per line.
    let mut cents: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut unmapped: Vec<(String, String, i64)> = Vec::new();
    let mut unknown_keys: BTreeSet<String> = BTreeSet::new();

    let all = statement
        .revenue
        .lines
        .iter()
        .chain(statement.expenses.lines.iter());

    for line in all {
        // Backfilled ancestors come through with a zero balance, and a genuinely
        // zero account has nothing to report either way. Neither is worth
        // warning about — an unmapped account matters because money is going
        // missing, and no money is.
        if line.balance == 0 {
            continue;
        }
        match mapping.get(&line.account_id) {
            Some(key) => match line_def(key) {
                Some(def) => {
                    let signed = match def.sense {
                        Sense::Natural => line.balance,
                        Sense::Contra => -line.balance,
                    };
                    *cents.entry(def.key).or_insert(0) += signed;
                }
                // A key the code no longer knows — a form revision dropped the
                // line, or the row was hand-edited. Treated as unmapped, because
                // that is what it is, and named separately so the cause is
                // visible.
                None => {
                    unknown_keys.insert(key.clone());
                    unmapped.push((
                        line.account_number.clone(),
                        line.account_name.clone(),
                        line.balance,
                    ));
                }
            },
            None => unmapped.push((
                line.account_number.clone(),
                line.account_name.clone(),
                line.balance,
            )),
        }
    }

    let mapped: BTreeMap<&'static str, i64> = cents
        .into_iter()
        .map(|(k, c)| (k, cents_to_dollars(c)))
        .collect();

    let mut warnings = Vec::new();
    if !unmapped.is_empty() {
        let total: i64 = unmapped.iter().map(|(_, _, c)| *c).sum();
        let named: Vec<String> = unmapped
            .iter()
            .map(|(num, name, c)| format!("{num} {name} ({})", format_dollars(cents_to_dollars(*c))))
            .collect();
        warnings.push(format!(
            "{} account(s) carrying {} are on no Form 1065 line and are missing from the return: {}.",
            unmapped.len(),
            format_dollars(cents_to_dollars(total)),
            named.join(", ")
        ));
    }
    for key in unknown_keys {
        warnings.push(format!(
            "Mapping refers to line key {key:?}, which this version of the form does not have."
        ));
    }

    ComputedLines {
        lines: Form1065Lines { mapped },
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Every saved mapping as account id → line key.
pub fn load_mapping(conn: &Connection) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT account_id, line_key FROM tax_line_mappings") {
        if let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (account_id, key) in rows.flatten() {
                out.insert(account_id, key);
            }
        }
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum MappingError {
    #[error("No Form 1065 line has key {0:?}")]
    UnknownLine(String),
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
}

/// Point an account at a line.
///
/// The key is checked against [`MAPPABLE_LINES`] here rather than trusted,
/// because a row with a key nothing recognises is an account whose balance
/// quietly stops reaching the return.
pub fn set_account_line(
    conn: &Connection,
    account_id: &str,
    line_key: &str,
) -> Result<(), MappingError> {
    if line_def(line_key).is_none() {
        return Err(MappingError::UnknownLine(line_key.to_string()));
    }
    conn.execute(
        "INSERT INTO tax_line_mappings (account_id, line_key, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(account_id) DO UPDATE SET line_key = ?2, updated_at = datetime('now')",
        rusqlite::params![account_id, line_key],
    )?;
    Ok(())
}

/// Take an account off the return.
pub fn clear_account_line(conn: &Connection, account_id: &str) -> Result<(), MappingError> {
    conn.execute(
        "DELETE FROM tax_line_mappings WHERE account_id = ?1",
        [account_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::reports::{IncomeStatementLine, IncomeStatementSection};
    use chrono::NaiveDate;

    fn line(id: &str, number: &str, name: &str, balance: i64) -> IncomeStatementLine {
        IncomeStatementLine {
            account_id: id.into(),
            account_number: number.into(),
            account_name: name.into(),
            parent_id: None,
            balance,
        }
    }

    fn statement(revenue: Vec<IncomeStatementLine>, expenses: Vec<IncomeStatementLine>) -> IncomeStatement {
        let rt = revenue.iter().map(|l| l.balance).sum();
        let et = expenses.iter().map(|l| l.balance).sum();
        IncomeStatement {
            start_date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
            end_date: NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
            revenue: IncomeStatementSection { name: "Revenue".into(), lines: revenue, total: rt },
            expenses: IncomeStatementSection { name: "Expenses".into(), lines: expenses, total: et },
            net_income: rt - et,
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(a, k)| (a.to_string(), k.to_string())).collect()
    }

    /// The property the whole module exists for: what the reader adds up on the
    /// printed page must be what the printed page says.
    #[test]
    fn the_totals_on_the_page_are_the_arithmetic_of_the_lines_on_the_page() {
        // Cents chosen so that every line rounds, and so that summing the cents
        // first and summing the rounded dollars first give different answers —
        // if the code ever rounds in the wrong order, this catches it.
        let s = statement(
            vec![line("a1", "4000", "Sales", 10_050), line("a2", "4100", "Consulting", 20_050)],
            vec![line("b1", "6000", "Wages", 3_050), line("b2", "6100", "Rent", 1_050)],
        );
        let m = map(&[("a1", "l1a"), ("a2", "l7"), ("b1", "l9"), ("b2", "l13")]);
        let c = compute(&s, &m);
        let l = &c.lines;

        assert_eq!(l.get("l1a"), 101, "100.50 rounds away from zero");
        assert_eq!(l.get("l7"), 201);
        assert_eq!(l.get("l9"), 31);
        assert_eq!(l.get("l13"), 11);

        // Every derived line, recomputed by hand from the *printed* figures.
        assert_eq!(l.line_1c(), 101 - 0);
        assert_eq!(l.line_3(), 101);
        assert_eq!(l.line_8(), 101 + 201);
        assert_eq!(l.line_22(), 31 + 11);
        assert_eq!(l.line_23(), 302 - 42);

        // And the identity the form itself asserts.
        assert_eq!(l.line_23(), l.line_8() - l.line_22());
        assert_eq!(l.line_8(), l.line_3() + l.get("l4") + l.get("l5") + l.get("l6") + l.get("l7"));
    }

    /// Rounding each account before summing accumulates a cent per account. This
    /// is the same figures totalled the right way and the wrong way.
    #[test]
    fn a_line_is_rounded_once_not_once_per_account() {
        // Four accounts at 50 cents each: rounded individually they are 4 × $1;
        // summed first they are $2.
        let s = statement(
            vec![],
            vec![
                line("b1", "6000", "A", 50),
                line("b2", "6001", "B", 50),
                line("b3", "6002", "C", 50),
                line("b4", "6003", "D", 50),
            ],
        );
        let m = map(&[("b1", "l21"), ("b2", "l21"), ("b3", "l21"), ("b4", "l21")]);
        let c = compute(&s, &m);
        assert_eq!(
            c.lines.get("l21"),
            2,
            "$2.00 of expense must not become $4 by rounding each account"
        );
    }

    /// A contra-revenue account carries a debit balance, so it arrives negative.
    /// Line 1b prints the figure that is *taken away*, which is positive.
    #[test]
    fn a_less_line_prints_the_amount_it_takes_away_as_a_positive_figure() {
        let s = statement(
            vec![line("a1", "4000", "Sales", 100_000), line("a2", "4900", "Refunds", -5_000)],
            vec![],
        );
        let m = map(&[("a1", "l1a"), ("a2", "l1b")]);
        let c = compute(&s, &m);

        assert_eq!(c.lines.get("l1a"), 1000);
        assert_eq!(c.lines.get("l1b"), 50, "refunds print positive on a 'less' line");
        assert_eq!(c.lines.line_1c(), 950, "and are subtracted, not added");
    }

    /// 16b is the depreciation already taken through cost of goods sold.
    /// Line 22 must use 16c or that depreciation is deducted twice.
    #[test]
    fn depreciation_reported_elsewhere_is_not_deducted_a_second_time() {
        let s = statement(
            vec![],
            vec![
                line("b1", "6500", "Depreciation", 10_000),
                line("b2", "6501", "Depreciation in COGS", -4_000),
            ],
        );
        let m = map(&[("b1", "l16a"), ("b2", "l16b")]);
        let c = compute(&s, &m);

        assert_eq!(c.lines.get("l16a"), 100);
        assert_eq!(c.lines.get("l16b"), 40);
        assert_eq!(c.lines.line_16c(), 60);
        assert_eq!(
            c.lines.line_22(),
            60,
            "line 22 takes 16c; taking 16a would deduct the COGS share twice"
        );
    }

    /// An unmapped account is money leaving the return without trace. It must be
    /// named, and named with its balance, or nobody can find it.
    #[test]
    fn an_account_with_a_balance_and_no_line_is_named_in_the_warnings() {
        let s = statement(
            vec![line("a1", "4000", "Sales", 100_000)],
            vec![line("b9", "6999", "Mystery expense", 12_345)],
        );
        let m = map(&[("a1", "l1a")]);
        let c = compute(&s, &m);

        let joined = c.warnings.join(" ");
        assert!(joined.contains("6999"), "the account number: {joined}");
        assert!(joined.contains("Mystery expense"), "and its name: {joined}");
        assert!(joined.contains("123"), "and its balance: {joined}");
        assert!(
            !c.lines.is_mapped("l21"),
            "an unmapped account must not be quietly swept into other deductions"
        );
    }

    /// Backfilled ancestors arrive with a zero balance and are not a problem to
    /// report — warning about them would bury the accounts that are.
    #[test]
    fn an_account_with_no_balance_is_not_reported_as_missing() {
        let s = statement(
            vec![line("a1", "4000", "Sales", 100_000), line("parent", "4", "Revenue", 0)],
            vec![],
        );
        let m = map(&[("a1", "l1a")]);
        let c = compute(&s, &m);
        assert!(
            c.warnings.is_empty(),
            "a zero-balance ancestor is not missing money: {:?}",
            c.warnings
        );
    }

    #[test]
    fn a_mapping_naming_a_line_the_form_does_not_have_is_reported_not_ignored() {
        let s = statement(vec![line("a1", "4000", "Sales", 100_000)], vec![]);
        let m = map(&[("a1", "l99")]);
        let c = compute(&s, &m);

        assert!(c.lines.is_empty(), "nothing was reportable");
        let joined = c.warnings.join(" ");
        assert!(joined.contains("l99"), "got {joined}");
        assert!(joined.contains("4000"), "and the account is still missing money: {joined}");
    }

    #[test]
    fn rounding_is_away_from_zero_and_symmetric_about_it() {
        assert_eq!(cents_to_dollars(150), 2);
        assert_eq!(cents_to_dollars(-150), -2, "a loss rounds like a profit");
        assert_eq!(cents_to_dollars(149), 1);
        assert_eq!(cents_to_dollars(-149), -1);
        assert_eq!(cents_to_dollars(0), 0);
        assert_eq!(cents_to_dollars(50), 1);
        assert_eq!(cents_to_dollars(-50), -1);
    }

    /// A loss is a real outcome and must survive to the bottom line.
    #[test]
    fn a_partnership_that_lost_money_reports_a_negative_line_23() {
        let s = statement(
            vec![line("a1", "4000", "Sales", 100_000)],
            vec![line("b1", "6000", "Wages", 250_000)],
        );
        let m = map(&[("a1", "l1a"), ("b1", "l9")]);
        let c = compute(&s, &m);

        assert_eq!(c.lines.line_8(), 1000);
        assert_eq!(c.lines.line_22(), 2500);
        assert_eq!(c.lines.line_23(), -1500);
        assert_eq!(format_dollars(c.lines.line_23()), "-1500");
    }

    #[test]
    fn every_mappable_line_key_is_unique_and_resolvable() {
        let mut seen = BTreeSet::new();
        for def in MAPPABLE_LINES {
            assert!(seen.insert(def.key), "duplicate line key {}", def.key);
            assert!(line_def(def.key).is_some());
            assert!(
                def.group == "Income" || def.group == "Deductions",
                "{} has group {}",
                def.key,
                def.group
            );
        }
    }
}
