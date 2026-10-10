//! Virginia Form 763, the nonresident return — computed from the person's
//! federal return and the Virginia K-1s (Schedule VK-1) on their books, and
//! filled for paper filing.
//!
//! # How a nonresident is taxed
//!
//! Form 763 computes Virginia taxable income as if the person were a resident
//! (lines 1–15), then takes the share of it that the Nonresident Allocation
//! Percentage gives (line 16): Virginia-source income over income from all
//! sources, both read off the federal return's income lines on page 2. The tax
//! on that share is the tax (line 18). What a partnership withheld for the
//! partner (VK-1) is withholding, on line 19a.
//!
//! # What it does not do
//!
//! Itemized deductions, the age deduction, Schedule 763 ADJ itself (its totals
//! are carried to lines 2 and 7; the schedule is to be completed and enclosed),
//! credits, and Virginia-source income other than K-1s. The name, SSN and
//! address are left for the person to write in.

use rusqlite::Connection;

use crate::commands::{k1_import_commands, personal_tax_commands};
use crate::events::types::FilingStatus;
use crate::tax::acroform::{self, set_check, set_text, strip_xfa, FormError};
use crate::tax::form1040::{Form1040, ReturnLine};
use crate::tax::k1_extract::state_codes as sc;
use crate::tax::md505::{round_dollars, StateReturnError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaParams {
    pub year: i32,
    pub verified: bool,
    pub source: &'static str,
    pub standard_deduction_single_cents: i64,
    pub standard_deduction_joint_cents: i64,
    pub exemption_cents: i64,
    /// Per box: 65 or older, blind.
    pub additional_exemption_cents: i64,
    /// Below this VAGI no tax is due (single and married filing separately).
    pub threshold_single_cents: i64,
    pub threshold_joint_cents: i64,
    pub form: &'static [u8],
}

const VA2025: VaParams = VaParams {
    year: 2025,
    verified: true,
    source: "2025 Form 763 instructions: standard deduction $8,750/$17,500, exemption $930 \
             (+$800 for 65 or blind), filing threshold $11,950/$23,900, Tax Rate Schedule",
    standard_deduction_single_cents: 875_000,
    standard_deduction_joint_cents: 1_750_000,
    exemption_cents: 93_000,
    additional_exemption_cents: 80_000,
    threshold_single_cents: 1_195_000,
    threshold_joint_cents: 2_390_000,
    form: include_bytes!("../../assets/state/va/2025/763.pdf"),
};

pub fn params_for(year: i32) -> Option<&'static VaParams> {
    match year {
        2025 => Some(&VA2025),
        _ => None,
    }
}

/// The Tax Rate Schedule, rounded to the dollar as its own example rounds.
///
/// Line 18 asks for the Tax Table, which is published separately and was not
/// checked; the table's $50 bands can differ from the schedule by about a
/// dollar, which [`compute`] says.
pub fn tax(income_cents: i64) -> i64 {
    if income_cents <= 0 {
        return 0;
    }
    let cents = match income_cents {
        c if c <= 300_000 => c * 200 / 10_000,
        c if c <= 500_000 => 6_000 + (c - 300_000) * 300 / 10_000,
        c if c <= 1_700_000 => 12_000 + (c - 500_000) * 500 / 10_000,
        c => 72_000 + (c - 1_700_000) * 575 / 10_000,
    };
    round_dollars(cents) * 100
}

/// The Virginia filing status code: 1 single, 2 joint, 4 separate. (3 — married,
/// spouse with no income — is not one these books can tell.)
fn status_code(status: FilingStatus) -> &'static str {
    match status {
        FilingStatus::MarriedFilingJointly => "2",
        FilingStatus::MarriedFilingSeparately => "4",
        FilingStatus::Single
        | FilingStatus::HeadOfHousehold
        | FilingStatus::QualifyingSurvivingSpouse => "1",
    }
}

/// One row of the Nonresident Allocation Percentage table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocationRow {
    pub line: &'static str,
    pub label: &'static str,
    pub all_sources_cents: i64,
    pub virginia_cents: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Va763 {
    pub tax_year: i32,
    pub filing_status: FilingStatus,
    /// Section 1 (you, spouse, dependents) and section 2 (65 or older, blind):
    /// (count, dollars).
    pub exemptions: [(i64, i64); 2],
    pub allocation: Vec<AllocationRow>,
    /// Line 16, in tenths of a percent.
    pub percentage_tenths: i64,
    pub lines: Vec<ReturnLine>,
    pub vagi_cents: i64,
    pub nonresident_taxable_cents: i64,
    pub tax_cents: i64,
    pub payments_cents: i64,
    /// Positive is an overpayment, negative tax owed.
    pub balance_cents: i64,
    /// Whether VAGI reaches the filing threshold.
    pub must_file: bool,
    pub warnings: Vec<String>,
}

impl Va763 {
    pub fn line(&self, key: &str) -> Option<&ReturnLine> {
        self.lines.iter().find(|l| l.key == key)
    }

    pub fn cents(&self, key: &str) -> i64 {
        self.line(key).map(|l| l.cents).unwrap_or(0)
    }

    /// Line 16 as the form wants it: "54.3".
    pub fn percentage_text(&self) -> String {
        format!("{}.{}", self.percentage_tenths / 10, self.percentage_tenths % 10)
    }
}

/// Compute a year's Form 763.
pub fn compute(conn: &Connection, federal: &Form1040) -> Result<Va763, StateReturnError> {
    let year = federal.tax_year;
    let params =
        params_for(year).ok_or(StateReturnError::UnsupportedYear("Virginia Form 763", year))?;
    let profile =
        personal_tax_commands::get_profile(conn, year).ok_or(StateReturnError::NoProfile(year))?;
    let va = k1_import_commands::state_totals(conn, year, "VA");
    if va.is_empty() {
        return Err(StateReturnError::NoStateIncome("Virginia", year));
    }
    let status = federal.filing_status;
    let get = |code: &str| va.get(code).copied().unwrap_or(0);
    let mut warnings = Vec::new();
    if !params.verified {
        warnings.push(format!(
            "The {year} Virginia figures ({}) have not been checked against the published \
             instructions.",
            params.source
        ));
    }

    // Page 2's allocation table: column A is the federal return's income lines,
    // before adjustments; column B the Virginia-source part — the K-1s'.
    let total_income = federal.line("9").map(|l| l.cents).unwrap_or(federal.agi_cents);
    let listed = [
        federal.wages_cents,
        federal.taxable_interest_cents,
        federal.ordinary_dividends_cents,
        federal.business_income_cents,
        federal.capital_gain_cents,
        federal.other_gains_cents,
        federal.retirement_taxable_cents,
        federal.schedule_e.total_cents,
    ];
    let other = total_income - listed.iter().sum::<i64>();
    let row = |line, label, all_sources_cents, virginia_cents| AllocationRow {
        line,
        label,
        all_sources_cents,
        virginia_cents,
    };
    let va_source = get(sc::SOURCE_INCOME);
    let mut allocation = vec![
        row("1", "Wages, salaries, tips", federal.wages_cents, 0),
        row("2", "Interest income", federal.taxable_interest_cents, 0),
        row("3", "Dividends", federal.ordinary_dividends_cents, 0),
        row("5", "Business income or loss", federal.business_income_cents, 0),
        row("6", "Capital gain or loss", federal.capital_gain_cents, 0),
        row("7", "Other gains or losses", federal.other_gains_cents, 0),
        row("8", "Taxable pensions, annuities, IRA distributions", federal.retirement_taxable_cents, 0),
        row(
            "9",
            "Rents, royalties, partnerships, estates, trusts, S corporations",
            federal.schedule_e.total_cents,
            va_source,
        ),
        row("11", "Other income", other, 0),
    ];
    let total_a: i64 = allocation.iter().map(|r| round_dollars(r.all_sources_cents) * 100).sum();
    let total_b: i64 = allocation.iter().map(|r| round_dollars(r.virginia_cents) * 100).sum();
    allocation.push(row("14", "Total", total_a, total_b));
    let percentage_tenths = if total_b <= 0 {
        0
    } else if total_a <= 0 {
        1_000
    } else {
        // One decimal place, rounded.
        ((total_b as i128 * 10_000 / total_a as i128 + 5) / 10).min(1_000) as i64
    };
    if federal.wages_cents > 0 {
        warnings.push(
            "Wages are all treated as earned outside Virginia. If any were earned working in \
             Virginia, enter them in column B of line 1 by hand."
                .to_string(),
        );
    }

    // Page 1.
    let l1 = federal.agi_cents;
    let l2 = get(sc::ADDITIONS);
    let l3 = l1 + l2;
    let l5 = federal.social_security_taxable_cents;
    let l7 = get(sc::SUBTRACTIONS);
    if l2 != 0 || l7 != 0 {
        warnings.push(
            "Lines 2 and 7 carry the VK-1's additions and subtractions; complete Schedule 763 \
             ADJ with them and enclose it — it is not prepared here."
                .to_string(),
        );
    }
    if profile.taxpayer_65_or_older || (status.has_spouse() && profile.spouse_65_or_older) {
        warnings.push(
            "The age deduction (lines 4a and 4b) is not computed; see the Age Deduction \
             Worksheet."
                .to_string(),
        );
    }
    if get(sc::PTE_ELECTION_TAX) != 0 {
        warnings.push(
            "The partnership paid Virginia's pass-through entity tax on your share: that is a \
             credit claimed on Schedule CR (line 25), which is not prepared here."
                .to_string(),
        );
    }
    let l8 = l5 + l7;
    let l9 = l3 - l8;
    let joint = status == FilingStatus::MarriedFilingJointly;
    let l11 = if joint {
        params.standard_deduction_joint_cents
    } else {
        params.standard_deduction_single_cents
    };
    let section1 = 1
        + i64::from(joint)
        + (profile.qualifying_children + profile.other_dependents) as i64;
    let section2 = [
        profile.taxpayer_65_or_older,
        profile.taxpayer_blind,
        joint && profile.spouse_65_or_older,
        joint && profile.spouse_blind,
    ]
    .iter()
    .filter(|b| **b)
    .count() as i64;
    let exemptions = [
        (section1, section1 * params.exemption_cents),
        (section2, section2 * params.additional_exemption_cents),
    ];
    let l12 = exemptions[0].1 + exemptions[1].1;
    let l14 = l11 + l12;
    let l15 = (l9 - l14).max(0);
    let l17 = round_dollars(l15 * percentage_tenths / 1_000) * 100;
    let threshold = if joint {
        params.threshold_joint_cents
    } else {
        params.threshold_single_cents
    };
    let must_file = l9 >= threshold;
    let l18 = if must_file { tax(l17) } else { 0 };
    if !must_file {
        warnings.push(format!(
            "Virginia adjusted gross income is under the ${} filing threshold: no tax is due, and \
             a return is needed only to claim back the ${} withheld.",
            threshold / 100,
            round_dollars(get(sc::WITHHOLDING))
        ));
    } else {
        warnings.push(
            "Line 18 is computed from the Tax Rate Schedule; the published Tax Table can differ \
             by about a dollar — check it against the table."
                .to_string(),
        );
    }
    let l19a = get(sc::WITHHOLDING);
    if l19a > 0 {
        warnings.push("Enclose the Schedule VK-1: line 19a's withholding is claimed from it.".into());
    }
    let l26 = l19a;
    let balance = l26 - l18;
    warnings.push(
        "Your name, Social Security number, address and birth date are not in these books; \
         write them at the top of Form 763 and of page 2."
            .to_string(),
    );

    let line = |key: &'static str, label: &'static str, cents: i64| ReturnLine {
        key,
        label,
        cents,
        note: None,
    };
    let mut lines = vec![
        line("1", "Federal adjusted gross income", l1),
        line("2", "Additions (Schedule 763 ADJ line 3)", l2),
        line("3", "Lines 1 and 2", l3),
        line("5", "Social Security benefits", l5),
        line("7", "Subtractions (Schedule 763 ADJ line 7)", l7),
        line("8", "Total subtractions", l8),
        line("9", "Virginia adjusted gross income", l9),
        line("11", "Standard deduction", l11),
        line("12", "Exemptions", l12),
        line("14", "Deductions and exemptions", l14),
        line("15", "Taxable income computed as a resident", l15),
        line("17", "Nonresident taxable income", l17),
        line("18", "Income tax", l18),
        line("19a", "Virginia income tax withheld", l19a),
        line("26", "Total payments and credits", l26),
    ];
    if balance < 0 {
        lines.push(line("27", "Income tax you owe", -balance));
        lines.push(line("35", "Amount you owe", -balance));
    } else if balance > 0 {
        lines.push(line("28", "Overpayment", balance));
        lines.push(line("36", "Refund", balance));
    }

    Ok(Va763 {
        tax_year: year,
        filing_status: status,
        exemptions,
        allocation,
        percentage_tenths,
        lines,
        vagi_cents: l9,
        nonresident_taxable_cents: l17,
        tax_cents: l18,
        payments_cents: l26,
        balance_cents: balance,
        must_file,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

type At = (u32, f32, f32);

/// Form 763's boxes on the 2025 revision: a point inside each, and the name
/// the field there carries. Both are checked by the tests; the fill goes by
/// position.
mod at {
    use super::At;
    pub const FILING_STATUS: (At, &str) = ((1, 56.0, 477.0), "Filing Status");
    pub const HOH: (At, &str) = ((1, 259.0, 496.0), "Federal head of household YES");
    pub const SPOUSE: (At, &str) = ((1, 407.0, 473.0), "Spouse");
    pub const DEPENDENTS: (At, &str) = ((1, 442.0, 473.0), "Dependents");
    pub const SECTION1: (At, At) = ((1, 481.0, 472.0), (1, 552.0, 473.0));
    pub const SECTION2: (At, At) = ((1, 481.0, 434.0), (1, 552.0, 434.0));
    pub const SECTION2_BOXES: [At; 4] = [
        (1, 373.0, 434.0),
        (1, 399.0, 434.0),
        (1, 427.0, 434.0),
        (1, 454.0, 434.0),
    ];
    pub const STATE_OF_RESIDENCE: (At, &str) = ((1, 100.0, 584.0), "State of Residence");
    pub const LINES: &[(&str, At, &str)] = &[
        ("1", (1, 521.0, 404.0), "Line 1 Adjusted Gross Income"),
        ("2", (1, 521.0, 386.0), "Line 2 Additions"),
        ("3", (1, 521.0, 368.0), "Line 3 Add Lines 1 and 2"),
        ("5", (1, 521.0, 314.0), "Line 5 Soc Sec and Rail benefits"),
        ("7", (1, 521.0, 279.0), "Line 7 Subtractions"),
        ("8", (1, 521.0, 261.0), "Line 8 Add Lines 4a, 4b, 5, 6 and 7"),
        ("9", (1, 521.0, 243.0), "Line 9 Virginia Adjusted Gross Income"),
        ("11", (1, 521.0, 207.0), "Line 11 Standard Deduction"),
        ("12", (1, 521.0, 189.0), "Line 12 Total Exemptions from Sections 1 and 2 above"),
        ("14", (1, 521.0, 153.0), "Line 14 Add Lines 10, 11, 12 and 13"),
        ("15", (1, 521.0, 135.0), "Line 15 Virginia Resident Taxable Income"),
        ("17", (1, 521.0, 99.0), "Line 17 Nonresident Taxable Income"),
        ("18", (1, 521.0, 81.0), "Line 18 Income Tax from Tax Table or Tax Rate Schedule"),
        ("19a", (1, 521.0, 63.0), "Line 19a Your Virginia Income tax withheld"),
        ("26", (2, 521.0, 614.0), "Line 26 Total payments and credits"),
        ("27", (2, 521.0, 600.0), "Line 27 Income Tax you owe"),
        ("28", (2, 521.0, 585.0), "Line 28 Overpayment amount"),
        ("35", (2, 521.0, 446.0), "Line 35 Amount You Owe"),
        ("36", (2, 521.0, 428.0), "Line 36 Amount refunded to you"),
    ];
    pub const PERCENTAGE_16: (At, &str) =
        ((1, 521.0, 117.0), "Line 16 Percentage Nonresident Allocation");
    pub const PERCENTAGE_15: (At, &str) =
        ((2, 512.0, 140.0), "Nonresident Allocation Percentage");
    /// Allocation rows: line, y; column A at x 401, B at x 512. Lines 8 and 12
    /// have no column B.
    pub const ALLOCATION_ROWS: &[(&str, f32)] = &[
        ("1", 343.0),
        ("2", 329.0),
        ("3", 315.0),
        ("4", 302.0),
        ("5", 288.0),
        ("6", 274.0),
        ("7", 260.0),
        ("8", 246.0),
        ("9", 232.0),
        ("10", 219.0),
        ("11", 205.0),
        ("12", 191.0),
        ("13", 175.0),
        ("14", 160.0),
    ];
    pub const COLUMN_A: f32 = 401.0;
    pub const COLUMN_B: f32 = 512.0;
}

fn field(widgets: &[acroform::Widget], at: At) -> Result<String, FormError> {
    acroform::field_at(widgets, at.0, at.1, at.2)
        .map(str::to_string)
        .ok_or_else(|| FormError::NoSuchField(format!("page {} at ({}, {})", at.0, at.1, at.2)))
}

/// Whole dollars, as the instructions ask.
fn whole(cents: i64) -> String {
    round_dollars(cents).to_string()
}

/// Fill Form 763.
pub fn fill(r: &Va763) -> Result<lopdf::Document, FormError> {
    let params = params_for(r.tax_year).ok_or(FormError::NoFormForYear {
        form: "Virginia Form 763",
        year: r.tax_year,
        available: "2025".to_string(),
    })?;
    let mut doc = lopdf::Document::load_mem(params.form)?;
    strip_xfa(&mut doc);
    let map = acroform::field_map(&doc);
    let w = acroform::widgets(&doc, &map);
    let put = |doc: &mut lopdf::Document, at: At, value: &str| -> Result<(), FormError> {
        let name = field(&w, at)?;
        set_text(doc, &map, &name, value)
    };

    put(&mut doc, at::FILING_STATUS.0, status_code(r.filing_status))?;
    put(&mut doc, at::STATE_OF_RESIDENCE.0, "Illinois")?;
    let (s1, s2) = (r.exemptions[0], r.exemptions[1]);
    if r.filing_status == FilingStatus::MarriedFilingJointly {
        put(&mut doc, at::SPOUSE.0, "1")?;
    }
    let dependents = s1.0 - 1 - i64::from(r.filing_status == FilingStatus::MarriedFilingJointly);
    if dependents > 0 {
        put(&mut doc, at::DEPENDENTS.0, &dependents.to_string())?;
    }
    put(&mut doc, at::SECTION1.0, &s1.0.to_string())?;
    put(&mut doc, at::SECTION1.1, &whole(s1.1))?;
    if s2.0 > 0 {
        put(&mut doc, at::SECTION2.0, &s2.0.to_string())?;
        put(&mut doc, at::SECTION2.1, &whole(s2.1))?;
    }
    for (key, at, _) in at::LINES {
        if let Some(line) = r.line(key) {
            put(&mut doc, *at, &whole(line.cents))?;
        }
    }
    put(&mut doc, at::PERCENTAGE_16.0, &r.percentage_text())?;
    put(&mut doc, at::PERCENTAGE_15.0, &r.percentage_text())?;
    for row in &r.allocation {
        let Some((_, y)) = at::ALLOCATION_ROWS.iter().find(|(l, _)| *l == row.line) else {
            continue;
        };
        if row.all_sources_cents != 0 || row.line == "14" {
            put(&mut doc, (2, at::COLUMN_A, *y), &whole(row.all_sources_cents))?;
        }
        if row.virginia_cents != 0 || row.line == "14" {
            put(&mut doc, (2, at::COLUMN_B, *y), &whole(row.virginia_cents))?;
        }
    }

    if r.filing_status == FilingStatus::HeadOfHousehold {
        let name = field(&w, at::HOH.0)?;
        set_check(&mut doc, &map, &name, "On")?;
    }
    Ok(doc)
}

pub fn build_pdf(r: &Va763) -> Result<Vec<u8>, FormError> {
    let mut doc = fill(r)?;
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_schedule_matches_the_instructions_example() {
        // "$90,000 … $720 + $4,197.50 = $4,917.50 which should be rounded to $4,918."
        assert_eq!(tax(9_000_000), 491_800);
        assert_eq!(tax(300_000), 6_000);
        assert_eq!(tax(400_000), 9_000);
        assert_eq!(tax(1_700_000), 72_000);
        assert_eq!(tax(0), 0);
    }

    /// Each box is found by position, and the field there is the one its name
    /// says — so a revision that moves or renames a box fails here.
    #[test]
    fn every_box_is_where_and_what_this_module_expects() {
        let doc = lopdf::Document::load_mem(VA2025.form).unwrap();
        let map = acroform::field_map(&doc);
        let w = acroform::widgets(&doc, &map);
        let named = [
            at::FILING_STATUS,
            at::HOH,
            at::SPOUSE,
            at::DEPENDENTS,
            at::STATE_OF_RESIDENCE,
            at::PERCENTAGE_16,
            at::PERCENTAGE_15,
        ];
        for (at, name) in named {
            assert_eq!(field(&w, at).unwrap(), name, "at {at:?}");
        }
        for (key, at, name) in at::LINES {
            assert_eq!(field(&w, *at).unwrap(), *name, "line {key}");
        }
        assert_eq!(field(&w, at::SECTION1.0).unwrap(), "Total Exemptions Section 1");
        assert_eq!(field(&w, at::SECTION1.1).unwrap(), "Exemption Dollar Amount Section 1");
        assert_eq!(field(&w, at::SECTION2.0).unwrap(), "Total Exemptions Section 2");
        assert_eq!(field(&w, at::SECTION2.1).unwrap(), "Exemption Dollar Amount Section 2");
        for at in at::SECTION2_BOXES {
            assert!(field(&w, at).is_ok());
        }
        for (line, y) in at::ALLOCATION_ROWS {
            assert!(field(&w, (2, at::COLUMN_A, *y)).is_ok(), "line {line} column A");
            if !matches!(*line, "8" | "12") {
                assert!(field(&w, (2, at::COLUMN_B, *y)).is_ok(), "line {line} column B");
            }
        }
        assert_eq!(
            field(&w, (2, at::COLUMN_B, 232.0)).unwrap(),
            "Virginia Rents, Royalties, Partnerships, Estates, Trusts, S corps"
        );
    }

    #[test]
    fn only_2025_is_carried() {
        assert!(params_for(2025).is_some());
        assert!(params_for(2024).is_none());
    }
}
