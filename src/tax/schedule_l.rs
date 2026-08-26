//! Schedule L, "Balance Sheets per Books": the two columns, from the books.
//!
//! # Why this is not [`super::lines::compute`]
//!
//! Page one and Schedule K total a *period's activity*, so one income statement
//! answers them. Schedule L reports a *position on two dates* — the first day of
//! the tax year and the last — and the two columns are read against each other:
//! an opening column that does not match last year's closing one is the first
//! thing an examiner asks about. Running a balance sheet through the activity
//! path would report the year's movement in cash as the cash on hand, which is
//! wrong in a way that still foots.
//!
//! So this module asks the ledger for two balance sheets, at `year_start - 1
//! day` and at `year_end`, and fills both columns from them.
//!
//! # Why the opening column is the day *before* the year starts
//!
//! "Beginning of tax year" means the position before the year's first entry, and
//! [`crate::queries::reports::Reports::balance_sheet`] is inclusive of its date.
//! Asking as of January 1 would include January 1's transactions in the opening
//! column and then again in the year's activity.
//!
//! # Signs
//!
//! The ledger holds liabilities and equity credit-normal, which is to say
//! negative. Schedule L prints all three sections as positive figures, so those
//! two are negated on the way out. Getting this wrong produces a balance sheet
//! whose totals are equal and opposite, which looks like a sign bug and is one.

use std::collections::BTreeMap;

use chrono::{Duration, NaiveDate};
use rusqlite::Connection;

use super::acroform::{set_text, FieldMap, FormError};
use super::lines::{cents_to_dollars, format_dollars, Field, Schedule, Sense, MAPPABLE_LINES};
use crate::queries::reports::{BalanceSheet, Reports};
use lopdf::Document;

/// The boxes the form computes rather than takes from an account.
mod derived {
    /// "14. Total assets." Beginning, then end.
    pub const TOTAL_ASSETS: (&str, &str) = ("f6_87[0]", "f6_89[0]");
    /// "22. Total liabilities and capital." Beginning, then end.
    pub const TOTAL_LIABS_CAPITAL: (&str, &str) = ("f6_123[0]", "f6_125[0]");
}

/// One line's two figures, in whole dollars.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Period {
    pub begin: i64,
    pub end: i64,
}

/// Schedule L as computed from the books.
#[derive(Debug, Clone, Default)]
pub struct ScheduleL {
    lines: BTreeMap<&'static str, Period>,
    /// Accounts with a balance that reach no Schedule L line, at either date.
    pub unmapped: Vec<(String, String, i64)>,
}

impl ScheduleL {
    pub fn get(&self, key: &str) -> Period {
        self.lines.get(key).copied().unwrap_or_default()
    }

    pub fn is_mapped(&self, key: &str) -> bool {
        self.lines.contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Set a line directly. Tests only — the real path is [`fold`], which is the
    /// only thing that knows the sign conventions the two columns depend on.
    #[cfg(test)]
    pub fn set_for_test(&mut self, key: &'static str, begin: i64, end: i64) {
        self.lines.insert(key, Period { begin, end });
    }

    /// Line 14. Total assets — every asset line, with the "less" lines
    /// subtracting.
    ///
    /// Computed from the same rounded dollars the boxes show, for the reason
    /// [`super::lines`] gives: a total derived from unrounded cents does not
    /// equal the sum of the printed figures, and the page has to add up as read.
    pub fn total_assets(&self) -> Period {
        self.total_of("Assets")
    }

    /// Line 22. Total liabilities and capital.
    pub fn total_liabilities_and_capital(&self) -> Period {
        self.total_of("Liabilities and capital")
    }

    fn total_of(&self, group: &str) -> Period {
        let mut out = Period::default();
        for def in MAPPABLE_LINES
            .iter()
            .filter(|d| d.schedule == Schedule::L && d.group == group)
        {
            let p = self.get(def.key);
            // A contra line prints positive and reduces its section — the same
            // rule the "less" lines follow on page one.
            let sign = match def.sense {
                Sense::Natural => 1,
                Sense::Contra => -1,
            };
            out.begin += sign * p.begin;
            out.end += sign * p.end;
        }
        out
    }

    /// Whether the two sides agree, on each date.
    ///
    /// Surfaced rather than asserted: a balance sheet that does not balance is a
    /// real state of somebody's books, and refusing to build the return would
    /// leave them with no way to see the figures that prove it.
    pub fn balances(&self) -> (bool, bool) {
        let a = self.total_assets();
        let lc = self.total_liabilities_and_capital();
        (a.begin == lc.begin, a.end == lc.end)
    }
}

/// Total the books onto Schedule L for `year`.
pub fn compute(
    conn: &Connection,
    year: i32,
    mapping: &BTreeMap<String, String>,
) -> Result<ScheduleL, rusqlite::Error> {
    let year_start = NaiveDate::from_ymd_opt(year, 1, 1).expect("January 1 exists in every year");
    let year_end = NaiveDate::from_ymd_opt(year, 12, 31).expect("December 31 exists in every year");
    // The day before the year opens — see the module docs.
    let opening = year_start - Duration::days(1);

    let reports = Reports::new(conn);
    let begin = reports
        .balance_sheet(opening)
        .map_err(|_| rusqlite::Error::QueryReturnedNoRows)?;
    let end = reports
        .balance_sheet(year_end)
        .map_err(|_| rusqlite::Error::QueryReturnedNoRows)?;

    Ok(fold(&begin, &end, mapping))
}

/// Fold two balance sheets onto the schedule's lines.
///
/// Split from [`compute`] so the arithmetic is testable without a ledger.
pub fn fold(
    begin: &BalanceSheet,
    end: &BalanceSheet,
    mapping: &BTreeMap<String, String>,
) -> ScheduleL {
    let mut cents: BTreeMap<&'static str, (i64, i64)> = BTreeMap::new();
    // Keyed by account so an account unmapped at both dates is reported once,
    // with the larger of its two balances — one row per account, not two.
    let mut unmapped: BTreeMap<String, (String, String, i64)> = BTreeMap::new();

    for (sheet, is_end) in [(begin, false), (end, true)] {
        // Assets are debit-normal and print as they stand. Liabilities and
        // equity are credit-normal — held negative — and print positive.
        let sections = [
            (&sheet.assets, 1i64),
            (&sheet.liabilities, -1),
            (&sheet.equity, -1),
        ];
        for (section, orient) in sections {
            for line in &section.lines {
                // Backfilled ancestors come through at zero, and a genuinely
                // empty account has nothing to report either way.
                if line.balance == 0 {
                    continue;
                }
                let oriented = orient * line.balance;
                match mapping.get(&line.account_id).and_then(|k| lookup(k)) {
                    Some(def) => {
                        let signed = match def.sense {
                            Sense::Natural => oriented,
                            Sense::Contra => -oriented,
                        };
                        let slot = cents.entry(def.key).or_insert((0, 0));
                        if is_end {
                            slot.1 += signed;
                        } else {
                            slot.0 += signed;
                        }
                    }
                    None => {
                        let e = unmapped.entry(line.account_id.clone()).or_insert((
                            line.account_number.clone(),
                            line.account_name.clone(),
                            0,
                        ));
                        if oriented.abs() > e.2.abs() {
                            e.2 = oriented;
                        }
                    }
                }
            }
        }
    }

    ScheduleL {
        lines: cents
            .into_iter()
            .map(|(k, (b, e))| {
                (
                    k,
                    Period {
                        begin: cents_to_dollars(b),
                        end: cents_to_dollars(e),
                    },
                )
            })
            .collect(),
        unmapped: unmapped.into_values().collect(),
    }
}

/// A Schedule L line by key. Page-one and Schedule K keys resolve to `None`
/// here, so a balance-sheet account pointed at an income line is treated as
/// unmapped rather than silently added to the wrong schedule.
fn lookup(key: &str) -> Option<&'static super::lines::TaxLineDef> {
    MAPPABLE_LINES
        .iter()
        .find(|d| d.key == key && d.schedule == Schedule::L)
}

/// Write Schedule L onto the form, returning what somebody should know first.
///
/// `required` is false when Schedule B question 4 was answered Yes — the
/// small-partnership exemption. The schedule is still filled in that case,
/// because figures the books already know are worth having on the page, and a
/// warning says it was not required. Blanking it instead would throw away work
/// nobody asked to throw away.
pub fn fill(
    doc: &mut Document,
    map: &FieldMap,
    sched: &ScheduleL,
    required: bool,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();

    if sched.is_empty() {
        if required {
            warnings.push(
                "Schedule L is blank: no balance-sheet account is mapped to a Schedule L line, and \
                 Schedule B question 4 does not exempt this partnership. Map them and regenerate."
                    .to_string(),
            );
        }
        return Ok(warnings);
    }

    for def in MAPPABLE_LINES
        .iter()
        .filter(|d| d.schedule == Schedule::L)
    {
        let Field::Period { begin, end } = def.field else {
            // Guarded by `only_the_profit_and_loss_schedules_are_totalled_from_activity`
            // in `super::lines`, so this is unreachable rather than merely unlikely.
            continue;
        };
        if !sched.is_mapped(def.key) {
            continue;
        }
        let p = sched.get(def.key);
        write_money(doc, map, begin, p.begin)?;
        write_money(doc, map, end, p.end)?;
    }

    // The two totals are always written, even at zero: they are the figures a
    // reader goes looking for, and a blank total reads as an unfinished page
    // rather than as a nil balance sheet.
    let assets = sched.total_assets();
    set_text(doc, map, derived::TOTAL_ASSETS.0, &format_dollars(assets.begin))?;
    set_text(doc, map, derived::TOTAL_ASSETS.1, &format_dollars(assets.end))?;
    let lc = sched.total_liabilities_and_capital();
    set_text(doc, map, derived::TOTAL_LIABS_CAPITAL.0, &format_dollars(lc.begin))?;
    set_text(doc, map, derived::TOTAL_LIABS_CAPITAL.1, &format_dollars(lc.end))?;

    if !required {
        warnings.push(
            "Schedule B question 4 is Yes, so Schedule L was not required. It has been filled from \
             the books anyway; delete the page before filing if you would rather rely on the \
             exemption."
                .to_string(),
        );
    }

    let (begins, ends) = sched.balances();
    if !begins || !ends {
        warnings.push(format!(
            "Schedule L does not balance ({}). Total assets {} / {} against liabilities and capital \
             {} / {}. The books say so — this is not a rounding artefact of the return.",
            match (begins, ends) {
                (false, false) => "at both dates",
                (false, true) => "at the start of the year",
                _ => "at the end of the year",
            },
            format_dollars(assets.begin),
            format_dollars(assets.end),
            format_dollars(lc.begin),
            format_dollars(lc.end),
        ));
    }

    if !sched.unmapped.is_empty() {
        let named: Vec<String> = sched
            .unmapped
            .iter()
            .map(|(num, name, c)| format!("{num} {name} ({})", format_dollars(cents_to_dollars(*c))))
            .collect();
        warnings.push(format!(
            "{} balance-sheet account(s) are on no Schedule L line and are missing from it: {}.",
            sched.unmapped.len(),
            named.join(", ")
        ));
    }

    Ok(warnings)
}

/// A Schedule L box, written only when it carries something.
///
/// Same rule as page one: an empty box says "no such item", a printed 0 is a
/// claim that somebody looked and found nothing. The two totals are written
/// unconditionally by the caller, which is the deliberate exception.
fn write_money(
    doc: &mut Document,
    map: &FieldMap,
    field: &str,
    dollars: i64,
) -> Result<(), FormError> {
    if dollars != 0 {
        set_text(doc, map, field, &format_dollars(dollars))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::reports::{BalanceSheetLine, BalanceSheetSection};
    use crate::domain::AccountType;

    fn line(id: &str, num: &str, name: &str, t: AccountType, balance: i64) -> BalanceSheetLine {
        BalanceSheetLine {
            account_id: id.to_string(),
            account_number: num.to_string(),
            account_name: name.to_string(),
            account_type: t,
            parent_id: None,
            balance,
        }
    }

    fn sheet(
        date: NaiveDate,
        assets: Vec<BalanceSheetLine>,
        liabilities: Vec<BalanceSheetLine>,
        equity: Vec<BalanceSheetLine>,
    ) -> BalanceSheet {
        let ta: i64 = assets.iter().map(|l| l.balance).sum();
        let tl: i64 = liabilities.iter().map(|l| -l.balance).sum();
        let te: i64 = equity.iter().map(|l| -l.balance).sum();
        BalanceSheet {
            as_of_date: date,
            assets: BalanceSheetSection { name: "Assets".into(), lines: assets, total: ta },
            liabilities: BalanceSheetSection { name: "Liabilities".into(), lines: liabilities, total: tl },
            equity: BalanceSheetSection { name: "Equity".into(), lines: equity, total: te },
            total_assets: ta,
            total_liabilities_and_equity: tl + te,
            is_balanced: ta == tl + te,
        }
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn mapping(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect()
    }

    /// Liabilities and capital are credit-normal in the ledger and positive on
    /// the form. A sign slip here produces a page whose two halves are equal and
    /// opposite.
    #[test]
    fn credit_normal_sections_print_positive() {
        let begin = sheet(
            d(2024, 12, 31),
            vec![line("cash", "1000", "Checking", AccountType::Asset, 500_00)],
            vec![line("ap", "2000", "Accounts Payable", AccountType::Liability, -200_00)],
            vec![line("cap", "3000", "Partners Capital", AccountType::Equity, -300_00)],
        );
        let end = sheet(
            d(2025, 12, 31),
            vec![line("cash", "1000", "Checking", AccountType::Asset, 800_00)],
            vec![line("ap", "2000", "Accounts Payable", AccountType::Liability, -300_00)],
            vec![line("cap", "3000", "Partners Capital", AccountType::Equity, -500_00)],
        );
        let m = mapping(&[("cash", "sl1"), ("ap", "sl15"), ("cap", "sl21")]);
        let s = fold(&begin, &end, &m);

        assert_eq!(s.get("sl1"), Period { begin: 500, end: 800 });
        assert_eq!(s.get("sl15"), Period { begin: 200, end: 300 });
        assert_eq!(s.get("sl21"), Period { begin: 300, end: 500 });
        assert_eq!(s.total_assets(), Period { begin: 500, end: 800 });
        assert_eq!(
            s.total_liabilities_and_capital(),
            Period { begin: 500, end: 800 }
        );
        assert_eq!(s.balances(), (true, true));
    }

    /// The opening column is a position, not the year's movement. This is the
    /// mistake the module docs are about.
    #[test]
    fn the_opening_column_is_last_years_closing_position() {
        let begin = sheet(
            d(2024, 12, 31),
            vec![line("cash", "1000", "Checking", AccountType::Asset, 1_000_00)],
            vec![],
            vec![line("cap", "3000", "Capital", AccountType::Equity, -1_000_00)],
        );
        let end = sheet(
            d(2025, 12, 31),
            vec![line("cash", "1000", "Checking", AccountType::Asset, 1_500_00)],
            vec![],
            vec![line("cap", "3000", "Capital", AccountType::Equity, -1_500_00)],
        );
        let s = fold(&begin, &end, &mapping(&[("cash", "sl1"), ("cap", "sl21")]));
        // 1000 opening, not the 500 the year moved.
        assert_eq!(s.get("sl1").begin, 1000);
        assert_eq!(s.get("sl1").end, 1500);
    }

    /// A "less" line prints positive and reduces its section.
    #[test]
    fn accumulated_depreciation_reduces_total_assets() {
        let bs = |cash: i64, gross: i64, accum: i64, date: NaiveDate| {
            sheet(
                date,
                vec![
                    line("cash", "1000", "Checking", AccountType::Asset, cash),
                    line("bldg", "1500", "Buildings", AccountType::Asset, gross),
                    // Accumulated depreciation is a contra asset: credit balance
                    // on an asset account, so it comes through negative.
                    line("accum", "1590", "Accum. Depreciation", AccountType::Asset, accum),
                ],
                vec![],
                vec![],
            )
        };
        let m = mapping(&[("cash", "sl1"), ("bldg", "sl9a"), ("accum", "sl9b")]);
        let s = fold(
            &bs(100_00, 1_000_00, -400_00, d(2024, 12, 31)),
            &bs(100_00, 1_000_00, -500_00, d(2025, 12, 31)),
            &m,
        );

        // 9b prints as a positive number...
        assert_eq!(s.get("sl9b"), Period { begin: 400, end: 500 });
        // ...and is taken away from the section.
        assert_eq!(
            s.total_assets(),
            Period {
                begin: 100 + 1000 - 400,
                end: 100 + 1000 - 500
            }
        );
    }

    #[test]
    fn a_balance_sheet_account_on_no_line_is_reported_once_not_twice() {
        let bs = |date: NaiveDate, bal: i64| {
            sheet(
                date,
                vec![line("mystery", "1900", "Suspense", AccountType::Asset, bal)],
                vec![],
                vec![],
            )
        };
        let s = fold(&bs(d(2024, 12, 31), 100_00), &bs(d(2025, 12, 31), 900_00), &BTreeMap::new());
        assert_eq!(s.unmapped.len(), 1, "{:?}", s.unmapped);
        // Reported at its larger balance, so the figure names the exposure.
        assert_eq!(s.unmapped[0].2, 900_00);
    }

    /// An account pointed at a page-one line has no business on Schedule L, and
    /// must not be quietly added to whichever line shares its key space.
    #[test]
    fn an_income_line_key_does_not_land_on_schedule_l() {
        let bs = |date: NaiveDate| {
            sheet(
                date,
                vec![line("cash", "1000", "Checking", AccountType::Asset, 500_00)],
                vec![],
                vec![],
            )
        };
        let s = fold(
            &bs(d(2024, 12, 31)),
            &bs(d(2025, 12, 31)),
            &mapping(&[("cash", "l21")]),
        );
        assert!(s.is_empty(), "nothing should have been placed");
        assert_eq!(s.unmapped.len(), 1);
    }

    #[test]
    fn a_sheet_that_does_not_balance_says_so_rather_than_refusing() {
        let begin = sheet(
            d(2024, 12, 31),
            vec![line("cash", "1000", "Checking", AccountType::Asset, 500_00)],
            vec![],
            vec![line("cap", "3000", "Capital", AccountType::Equity, -100_00)],
        );
        let end = begin.clone();
        let s = fold(&begin, &end, &mapping(&[("cash", "sl1"), ("cap", "sl21")]));
        assert_eq!(s.balances(), (false, false));

        let mut doc =
            lopdf::Document::load_mem(include_bytes!("../../assets/irs/f1065.pdf")).unwrap();
        crate::tax::acroform::strip_xfa(&mut doc);
        let map = crate::tax::acroform::field_map(&doc);
        let warnings = fill(&mut doc, &map, &s, true).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("does not balance")),
            "{warnings:?}"
        );
    }

    #[test]
    fn the_exemption_fills_the_page_and_says_it_was_not_required() {
        let bs = |date: NaiveDate| {
            sheet(
                date,
                vec![line("cash", "1000", "Checking", AccountType::Asset, 500_00)],
                vec![],
                vec![line("cap", "3000", "Capital", AccountType::Equity, -500_00)],
            )
        };
        let s = fold(
            &bs(d(2024, 12, 31)),
            &bs(d(2025, 12, 31)),
            &mapping(&[("cash", "sl1"), ("cap", "sl21")]),
        );
        let mut doc =
            lopdf::Document::load_mem(include_bytes!("../../assets/irs/f1065.pdf")).unwrap();
        crate::tax::acroform::strip_xfa(&mut doc);
        let map = crate::tax::acroform::field_map(&doc);
        let warnings = fill(&mut doc, &map, &s, false).unwrap();

        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "f6_15[0]").as_deref(),
            Some("500"),
            "the page is filled even when not required"
        );
        assert!(
            warnings.iter().any(|w| w.contains("not required")),
            "{warnings:?}"
        );
    }

    /// The totals are the arithmetic of the printed figures, so the page adds up
    /// as read.
    #[test]
    fn the_totals_are_the_sum_of_the_boxes_on_the_page() {
        let bs = |date: NaiveDate| {
            sheet(
                date,
                vec![
                    line("a", "1000", "Checking", AccountType::Asset, 333_33),
                    line("b", "1100", "Savings", AccountType::Asset, 333_33),
                    line("c", "1200", "Petty cash", AccountType::Asset, 333_34),
                ],
                vec![],
                vec![],
            )
        };
        let m = mapping(&[("a", "sl1"), ("b", "sl4"), ("c", "sl6")]);
        let s = fold(&bs(d(2024, 12, 31)), &bs(d(2025, 12, 31)), &m);
        let t = s.total_assets();
        assert_eq!(
            t.end,
            s.get("sl1").end + s.get("sl4").end + s.get("sl6").end,
            "the total must equal the figures the reader can see"
        );
    }
}
