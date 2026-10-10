//! Maryland Form 505, the nonresident return, with Form 505NR — computed from the
//! person's federal return and the Maryland K-1s on their books, and filled for
//! paper filing.
//!
//! # How a nonresident is taxed, in the form's own steps
//!
//! Form 505 first computes a taxable net income as if all of federal AGI were
//! Maryland's (lines 1–31). Form 505NR then works out how much is Maryland's:
//! Maryland AGI over federal AGI is the *income factor*, which scales the
//! deduction and exemptions; the Maryland taxable net income that results, over
//! the line-31 figure, is the *nonresident factor*, which scales the tax.
//! On top of that a nonresident pays the 2.25% special nonresident tax on
//! Maryland taxable net income, in place of the county tax a resident pays.
//!
//! What the partnership already paid for the partner (Maryland Schedule K-1
//! (510/511) line D.1) is a payment on line 47.
//!
//! # Where a partnership's Maryland income goes
//!
//! The Maryland K-1 gives one figure — the partner's share allocable to
//! Maryland — not the lines it came from. It goes on line 10 (partnerships),
//! column 2. Line 10's non-Maryland column is then usually a loss, which line 18
//! adds back; that is the form's arithmetic, not a loss anybody had, and Maryland
//! AGI comes out the same whichever line the figure is put on.
//!
//! # What it does not do
//!
//! Itemized deductions (the standard deduction is used), Maryland-source wages
//! and other non-K-1 Maryland income, the capital gains surtax of Form 502CG, and
//! credits. Each is said in the warnings when the year's figures make it matter.
//! The name, SSN and address are left for the person to write in: personal books
//! do not hold them.

use rusqlite::Connection;

use crate::commands::{k1_import_commands, personal_tax_commands};
use crate::events::types::FilingStatus;
use crate::tax::acroform::{self, set_check, set_text, strip_xfa, FormError};
use crate::tax::form1040::{Form1040, ReturnLine};
use crate::tax::k1_extract::state_codes as sc;

/// One year's Maryland figures for a nonresident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MdParams {
    pub year: i32,
    /// Checked against the published instructions.
    pub verified: bool,
    pub source: &'static str,
    /// Single, married filing separately, dependent.
    pub standard_deduction_single_cents: i64,
    /// Joint, head of household, qualifying surviving spouse.
    pub standard_deduction_joint_cents: i64,
    pub exemption_cents: i64,
    /// Per box: 65 or older, blind.
    pub additional_exemption_cents: i64,
    /// Special nonresident tax, in basis points.
    pub special_nonresident_bp: i64,
    /// Below this taxable net income the tax comes from the tax table rather
    /// than the rate schedules.
    pub table_limit_cents: i64,
    pub form: &'static [u8],
    pub form_nr: &'static [u8],
}

const MD2025: MdParams = MdParams {
    year: 2025,
    verified: true,
    source: "2025 Maryland nonresident instructions: standard deduction $3,350/$6,700, \
             exemption $3,200, Tax Rate Schedules I and II, 2.25% special nonresident tax",
    standard_deduction_single_cents: 335_000,
    standard_deduction_joint_cents: 670_000,
    exemption_cents: 320_000,
    additional_exemption_cents: 100_000,
    special_nonresident_bp: 225,
    table_limit_cents: 5_000_000,
    form: include_bytes!("../../assets/state/md/2025/505.pdf"),
    form_nr: include_bytes!("../../assets/state/md/2025/505nr.pdf"),
};

pub fn params_for(year: i32) -> Option<&'static MdParams> {
    match year {
        2025 => Some(&MD2025),
        _ => None,
    }
}

/// Rate schedule brackets: (above, base tax on the amount below, rate in bp).
const SCHEDULE_I: &[(i64, i64, i64)] = &[
    (0, 0, 200),
    (1_000, 20_00, 300),
    (2_000, 50_00, 400),
    (3_000, 90_00, 475),
    (100_000, 4_697_50, 500),
    (125_000, 5_947_50, 525),
    (150_000, 7_260_00, 550),
    (250_000, 12_760_00, 575),
    (500_000, 27_135_00, 625),
    (1_000_000, 58_385_00, 650),
];
const SCHEDULE_II: &[(i64, i64, i64)] = &[
    (0, 0, 200),
    (1_000, 20_00, 300),
    (2_000, 50_00, 400),
    (3_000, 90_00, 475),
    (150_000, 7_072_50, 500),
    (175_000, 8_322_50, 525),
    (225_000, 10_947_50, 550),
    (300_000, 15_072_50, 575),
    (600_000, 32_322_50, 625),
    (1_200_000, 69_822_50, 650),
];

fn joint_like(status: FilingStatus) -> bool {
    matches!(
        status,
        FilingStatus::MarriedFilingJointly
            | FilingStatus::HeadOfHousehold
            | FilingStatus::QualifyingSurvivingSpouse
    )
}

/// The rate schedule, in cents, on taxable net income in cents.
fn schedule_tax(status: FilingStatus, income_cents: i64) -> i64 {
    if income_cents <= 0 {
        return 0;
    }
    let table = if joint_like(status) {
        SCHEDULE_II
    } else {
        SCHEDULE_I
    };
    let (above, base, bp) = table
        .iter()
        .rev()
        .find(|(above, _, _)| income_cents > above * 100)
        .copied()
        .unwrap_or((0, 0, 200));
    base + (income_cents - above * 100) * bp / 10_000
}

/// Line 2 of Form 505NR: the tax table below the table limit, the rate
/// schedules above it.
///
/// The printed table steps in $50 bands and gives, for each, the schedule's tax
/// at the middle of the band rounded to the dollar — checked against its rows
/// (taxable income of $3,200 to $3,250 is $101). Computing it reproduces the
/// table without carrying its forty pages.
pub fn tax(params: &MdParams, status: FilingStatus, income_cents: i64) -> i64 {
    if income_cents <= 0 {
        return 0;
    }
    if income_cents < params.table_limit_cents {
        let band = income_cents / 5_000 * 5_000;
        return round_dollars(schedule_tax(status, band + 2_500)) * 100;
    }
    round_dollars(schedule_tax(status, income_cents)) * 100
}

/// Half a dollar and up rounds up.
pub fn round_dollars(cents: i64) -> i64 {
    if cents >= 0 {
        (cents + 50) / 100
    } else {
        -((-cents + 50) / 100)
    }
}

/// Cents rounded to a whole dollar, still in cents: the form is filled in
/// whole dollars, and its later lines are worked from the rounded figures.
fn to_dollar(cents: i64) -> i64 {
    round_dollars(cents) * 100
}

/// A factor, in parts per million: `num / den` held to `[0, 1]` under the rules
/// both forms print (zero or less is 0; a positive numerator over a denominator of
/// zero or less is 1).
fn factor_ppm(num: i64, den: i64) -> i64 {
    if num <= 0 {
        0
    } else if den <= 0 {
        1_000_000
    } else {
        ((num as i128 * 1_000_000) / den as i128).min(1_000_000) as i64
    }
}

fn apply_ppm(cents: i64, ppm: i64) -> i64 {
    ((cents as i128 * ppm as i128) / 1_000_000) as i64
}

/// The per-person exemption at a federal AGI (Exemption Amount Chart 10A).
fn exemption_each(params: &MdParams, status: FilingStatus, agi_cents: i64) -> i64 {
    let full = params.exemption_cents;
    let agi = agi_cents / 100;
    if joint_like(status) {
        match agi {
            a if a <= 150_000 => full,
            a if a <= 175_000 => full / 2,
            a if a <= 200_000 => full / 4,
            _ => 0,
        }
    } else {
        match agi {
            a if a <= 100_000 => full,
            a if a <= 125_000 => full / 2,
            a if a <= 150_000 => full / 4,
            _ => 0,
        }
    }
}

/// One row of Form 505's income section: federal, Maryland, and non-Maryland.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncomeRow {
    pub line: &'static str,
    pub label: &'static str,
    pub federal_cents: i64,
    pub maryland_cents: i64,
}

impl IncomeRow {
    pub fn non_maryland_cents(&self) -> i64 {
        self.federal_cents - self.maryland_cents
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Md505 {
    pub tax_year: i32,
    pub filing_status: FilingStatus,
    /// Lines 1–17.
    pub income: Vec<IncomeRow>,
    /// Form 505 lines 18 onward, then Form 505NR's lines keyed `nr1`…`nr17`.
    pub lines: Vec<ReturnLine>,
    /// Exemptions: (count, dollars) for A, B, C and D.
    pub exemptions: [(i64, i64); 4],
    pub agi_factor_ppm: i64,
    pub income_factor_ppm: i64,
    pub nonresident_factor_ppm: i64,
    /// Line 19's and line 23's code letters.
    pub addition_codes: Vec<&'static str>,
    pub subtraction_codes: Vec<&'static str>,
    /// Maryland AGI (505NR line 8): the income Maryland taxes.
    pub maryland_agi_cents: i64,
    /// Maryland taxable net income (505NR line 13).
    pub maryland_taxable_cents: i64,
    /// Line 32a: Maryland's own tax.
    pub state_tax_cents: i64,
    /// Line 32b: the special nonresident tax.
    pub special_tax_cents: i64,
    /// Line 43.
    pub total_tax_cents: i64,
    /// Line 49.
    pub payments_cents: i64,
    /// Positive is an overpayment (line 51), negative a balance due (line 50).
    pub balance_cents: i64,
    pub warnings: Vec<String>,
}

impl Md505 {
    pub fn line(&self, key: &str) -> Option<&ReturnLine> {
        self.lines.iter().find(|l| l.key == key)
    }

    pub fn cents(&self, key: &str) -> i64 {
        self.line(key).map(|l| l.cents).unwrap_or(0)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StateReturnError {
    UnsupportedYear(&'static str, i32),
    NoProfile(i32),
    NoStateIncome(&'static str, i32),
}

impl std::fmt::Display for StateReturnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateReturnError::UnsupportedYear(form, year) => write!(
                f,
                "{year} is not a year this version carries {form} for; it is refused rather \
                 than filled on another year's form"
            ),
            StateReturnError::NoProfile(year) => {
                write!(f, "no {year} tax profile (filing status) is recorded")
            }
            StateReturnError::NoStateIncome(state, year) => {
                write!(f, "no {year} {state} K-1 is recorded, so there is no {state} income to report")
            }
        }
    }
}

impl std::error::Error for StateReturnError {}

/// Compute a year's Form 505 and 505NR.
pub fn compute(conn: &Connection, federal: &Form1040) -> Result<Md505, StateReturnError> {
    let year = federal.tax_year;
    let params =
        params_for(year).ok_or(StateReturnError::UnsupportedYear("Maryland Form 505", year))?;
    let profile =
        personal_tax_commands::get_profile(conn, year).ok_or(StateReturnError::NoProfile(year))?;
    let md = k1_import_commands::state_totals(conn, year, "MD");
    if md.is_empty() {
        return Err(StateReturnError::NoStateIncome("Maryland", year));
    }
    let status = federal.filing_status;
    let mut warnings = Vec::new();
    if !params.verified {
        warnings.push(format!(
            "The {year} Maryland figures ({}) have not been checked against the published \
             instructions.",
            params.source
        ));
    }
    let get = |code: &str| md.get(code).copied().unwrap_or(0);
    let md_source = get(sc::SOURCE_INCOME);

    // Lines 1–17. Column 1 from the federal return; the Maryland K-1's figure on
    // line 10 of column 2.
    let total_income = federal.line("9").map(|l| l.cents).unwrap_or(federal.agi_cents);
    let adjustments = federal.line("10").map(|l| l.cents).unwrap_or(0);
    let listed = [
        federal.wages_cents,
        federal.taxable_interest_cents,
        federal.ordinary_dividends_cents,
        federal.business_income_cents,
        federal.capital_gain_cents,
        federal.other_gains_cents,
        federal.retirement_taxable_cents,
        federal.schedule_e.total_cents,
        federal.social_security_taxable_cents,
    ];
    let other_income = total_income - listed.iter().sum::<i64>();
    let row = |line, label, federal_cents, maryland_cents| IncomeRow {
        line,
        label,
        federal_cents,
        maryland_cents,
    };
    let mut income = vec![
        row("1", "Wages, salaries, tips", federal.wages_cents, 0),
        row("2", "Taxable interest", federal.taxable_interest_cents, 0),
        row("3", "Dividends", federal.ordinary_dividends_cents, 0),
        row("6", "Business income or loss", federal.business_income_cents, 0),
        row("7", "Capital gain or loss", federal.capital_gain_cents, 0),
        row("8", "Other gains or losses (Form 4797)", federal.other_gains_cents, 0),
        row("9", "Pensions, IRA distributions, annuities", federal.retirement_taxable_cents, 0),
        row(
            "10",
            "Rents, royalties, partnerships, estates, trusts",
            federal.schedule_e.total_cents,
            md_source,
        ),
        row("13", "Taxable Social Security", federal.social_security_taxable_cents, 0),
        row("14", "Other income", other_income, 0),
    ];
    let md_total: i64 = income.iter().map(|r| r.maryland_cents).sum();
    income.push(row("15", "Total income", total_income, md_total));
    income.push(row("16", "Adjustments to income", adjustments, 0));
    income.push(row("17", "Adjusted gross income", federal.agi_cents, md_total));

    if federal.wages_cents > 0 {
        warnings.push(
            "Wages are all treated as non-Maryland. If any were earned working in Maryland, \
             enter them in column 2 of line 1 by hand."
                .to_string(),
        );
    }

    // Line 18: non-Maryland losses and adjustments, added back.
    let rows_1_14 = &income[..income.len() - 3];
    let non_md_losses: i64 = rows_1_14
        .iter()
        .map(|r| (-r.non_maryland_cents()).max(0))
        .sum::<i64>()
        + adjustments;
    let non_md_income: i64 = rows_1_14.iter().map(|r| r.non_maryland_cents().max(0)).sum();
    let additions = get(sc::ADDITIONS) + get(sc::PTE_ELECTION_TAX);
    let mut addition_codes = Vec::new();
    if get(sc::ADDITIONS) != 0 {
        addition_codes.push("a");
    }
    if get(sc::PTE_ELECTION_TAX) != 0 {
        addition_codes.push("r");
        warnings.push(
            "The pass-through entity election tax paid on your share (Form 511) is added back \
             on line 19 (code r) and claimed as a credit on Form 502CR, which is not prepared \
             here."
                .to_string(),
        );
    }
    let l20 = non_md_losses + additions;
    let l21 = federal.agi_cents + l20;
    let subtractions = get(sc::SUBTRACTIONS);
    let mut subtraction_codes = Vec::new();
    if get(sc::SUBTRACTIONS_DECOUPLING) != 0 {
        subtraction_codes.push("dp");
    }
    if subtractions - get(sc::SUBTRACTIONS_DECOUPLING) != 0 {
        subtraction_codes.push("b");
    }
    if subtractions != 0 {
        warnings.push(
            "Line 23's subtractions are itemised on Form 505SU, which has to be attached; it \
             is not prepared here."
                .to_string(),
        );
    }
    let l25 = l21 - subtractions;

    // Deduction and exemptions, scaled by the AGI factor.
    let standard = if joint_like(status) {
        params.standard_deduction_joint_cents
    } else {
        params.standard_deduction_single_cents
    };
    if federal.deduction_cents > crate::tax::federal_params::for_year(year)
        .map(|p| p.standard_deduction.of(status))
        .unwrap_or(i64::MAX)
    {
        warnings.push(
            "The federal return itemizes, and this uses Maryland's standard deduction. Compare \
             with Maryland itemized deductions (lines 26b–26e) by hand."
                .to_string(),
        );
    }
    let agi_factor = factor_ppm(l25, federal.agi_cents);
    let deduction = to_dollar(apply_ppm(standard, agi_factor));
    let l27 = l25 - deduction;
    let each = exemption_each(params, status, federal.agi_cents);
    let people_a = 1 + i64::from(status == FilingStatus::MarriedFilingJointly);
    let boxes_b = [
        profile.taxpayer_65_or_older,
        profile.taxpayer_blind,
        status.has_spouse() && profile.spouse_65_or_older,
        status.has_spouse() && profile.spouse_blind,
    ]
    .iter()
    .filter(|b| **b)
    .count() as i64;
    let dependents = (profile.qualifying_children + profile.other_dependents) as i64;
    let exemptions = [
        (people_a, people_a * each),
        (boxes_b, boxes_b * params.additional_exemption_cents),
        (dependents, dependents * each),
        (
            people_a + boxes_b + dependents,
            people_a * each + boxes_b * params.additional_exemption_cents + dependents * each,
        ),
    ];
    let l28 = exemptions[3].1;
    let l30 = to_dollar(apply_ppm(l28, agi_factor));
    let l31 = (l27 - l30).max(0);

    // Form 505NR.
    let nr2 = tax(params, status, l31);
    let earned = federal.wages_cents + federal.business_income_cents.max(0);
    let nr7 = subtractions + non_md_income;
    let nr8 = l21 - nr7;
    let income_factor = factor_ppm(nr8, federal.agi_cents);
    let nr10a = to_dollar(apply_ppm(standard, income_factor));
    let nr11 = nr8 - nr10a;
    let nr12 = to_dollar(apply_ppm(l28, income_factor));
    let nr13 = nr11 - nr12;
    let nonresident_factor = factor_ppm(nr13, l31);
    let nr16 = round_dollars(apply_ppm(nr2, nonresident_factor)) * 100;
    let nr17 = if nr13 > 0 {
        round_dollars(nr13 * params.special_nonresident_bp / 10_000) * 100
    } else {
        0
    };
    if federal.agi_cents > 35_000_000 && federal.capital_gain_cents > 0 {
        warnings.push(
            "Federal AGI is over $350,000 with a capital gain: the additional 2% tax on net \
             capital gain (Form 502CG, line 32d) is not computed."
                .to_string(),
        );
    }

    let l32e = nr16 + nr17;
    let l43 = l32e;
    let withheld = 0;
    let pte_paid = get(sc::NONRESIDENT_TAX_PAID);
    let l49 = withheld + pte_paid;
    let balance = l49 - l43;
    if pte_paid > 0 {
        warnings.push(
            "Attach the Maryland Schedule K-1 (510/511): line 47's payment is claimed from it."
                .to_string(),
        );
    }
    warnings.push(
        "Your name, Social Security number and address are not in these books; write them \
         at the top of Form 505 and Form 505NR."
            .to_string(),
    );

    let line = |key: &'static str, label: &'static str, cents: i64| ReturnLine {
        key,
        label,
        cents,
        note: None,
    };
    let mut lines = vec![
        line("18", "Non-Maryland loss and adjustments", non_md_losses),
        line("19", "Other additions", additions),
        line("20", "Total additions", l20),
        line("21", "Federal AGI and Maryland additions", l21),
        line("23", "Other subtractions (Form 505SU)", subtractions),
        line("24", "Total subtractions", subtractions),
        line("25", "Maryland AGI before subtraction of non-Maryland income", l25),
        line("26a", "Standard deduction", standard),
        line("26", "Deduction × AGI factor", deduction),
        line("27", "Net income", l27),
        line("28", "Total exemption amount", l28),
        line("30", "Exemption allowance × AGI factor", l30),
        line("31", "Taxable net income", l31),
        line("nr1", "505NR 1: taxable net income", l31),
        line("nr2", "505NR 2: tax", nr2),
        line("nr3", "505NR 3: federal AGI", federal.agi_cents),
        line("nr3a", "505NR 3a: earned income", earned),
        line("nr4", "505NR 4: federal AGI plus additions", l21),
        line("nr6a", "505NR 6a: subtractions", subtractions),
        line("nr6b", "505NR 6b: non-Maryland income", non_md_income),
        line("nr7", "505NR 7: lines 5 through 6b", nr7),
        line("nr8", "505NR 8: Maryland adjusted gross income", nr8),
        line("nr10a", "505NR 10a: deduction × income factor", nr10a),
        line("nr11", "505NR 11: net income", nr11),
        line("nr12", "505NR 12: exemptions × income factor", nr12),
        line("nr13", "505NR 13: Maryland taxable net income", nr13),
        line("nr14", "505NR 14: tax from line 2", nr2),
        line("nr16", "505NR 16: Maryland tax", nr16),
        line("nr17", "505NR 17: special nonresident tax (2.25%)", nr17),
        line("32a", "Maryland tax (505NR line 16)", nr16),
        line("32b", "Special nonresident tax (505NR line 17)", nr17),
        line("32e", "Total Maryland tax", l32e),
        line("37", "Maryland tax after credits", l32e),
        line("43", "Total Maryland income tax", l43),
        line("44", "Maryland tax withheld", withheld),
        line("47", "Nonresident tax paid by pass-through entities", pte_paid),
        line("49", "Total payments and credits", l49),
    ];
    if balance >= 0 {
        lines.push(line("51", "Overpayment", balance));
        lines.push(line("53", "Refund", balance));
    } else {
        lines.push(line("50", "Balance due", -balance));
        lines.push(line("55", "Total amount due", -balance));
    }

    Ok(Md505 {
        tax_year: year,
        filing_status: status,
        income,
        lines,
        exemptions,
        agi_factor_ppm: agi_factor,
        income_factor_ppm: income_factor,
        nonresident_factor_ppm: nonresident_factor,
        addition_codes,
        subtraction_codes,
        maryland_agi_cents: nr8,
        maryland_taxable_cents: nr13,
        state_tax_cents: nr16,
        special_tax_cents: nr17,
        total_tax_cents: l43,
        payments_cents: l49,
        balance_cents: balance,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// The forms
// ---------------------------------------------------------------------------

/// Where a box is: page, and a point inside it, in PDF points.
type At = (u32, f32, f32);

/// Form 505's boxes, by position on the 2025 revision.
mod at_505 {
    use super::At;
    pub const RESIDENCE_STATE: At = (1, 284.0, 258.0);
    /// Exemptions A to D: (count box, dollar box).
    pub const EXEMPTIONS: [(At, At); 4] = [
        ((1, 310.0, 149.0), (1, 461.0, 150.0)),
        ((1, 309.0, 102.0), (1, 461.0, 102.0)),
        ((1, 310.0, 78.0), (1, 461.0, 78.0)),
        ((1, 309.0, 66.0), (1, 461.0, 66.0)),
    ];
    pub const SELF: At = (1, 79.0, 149.0);
    pub const SPOUSE: At = (1, 151.0, 149.0);
    pub const YOU_65: At = (1, 79.0, 125.0);
    pub const SPOUSE_65: At = (1, 151.0, 126.0);
    pub const YOU_BLIND: At = (1, 79.0, 102.0);
    pub const SPOUSE_BLIND: At = (1, 152.0, 102.0);
    /// Lines 1 to 17: the y of each row; columns at these x.
    pub const COLUMNS: [f32; 3] = [317.0, 418.0, 519.0];
    pub const ROWS: &[(&str, f32)] = &[
        ("1", 666.0),
        ("2", 654.0),
        ("3", 642.0),
        ("4", 618.0),
        ("5", 606.0),
        ("6", 594.0),
        ("7", 582.0),
        ("8", 570.0),
        ("9", 546.0),
        ("10", 522.0),
        ("11", 510.0),
        ("12", 498.0),
        ("13", 474.0),
        ("14", 450.0),
        ("15", 438.0),
        ("16", 414.0),
        ("17", 402.0),
    ];
    /// The right-hand column, lines 18 to 31.
    pub const RIGHT: &[(&str, At)] = &[
        ("18", (2, 518.0, 378.0)),
        ("19", (2, 518.0, 366.0)),
        ("20", (2, 518.0, 354.0)),
        ("21", (2, 518.0, 342.0)),
        ("23", (2, 518.0, 306.0)),
        ("24", (2, 518.0, 294.0)),
        ("25", (2, 518.0, 282.0)),
        ("26a", (2, 417.0, 258.0)),
        ("26", (2, 518.0, 186.0)),
        ("27", (2, 518.0, 174.0)),
        ("28", (2, 518.0, 162.0)),
        ("30", (2, 518.0, 138.0)),
        ("31", (2, 518.0, 126.0)),
    ];
    pub const ADDITION_CODES: [At; 4] = [
        (2, 295.0, 365.0),
        (2, 316.0, 365.0),
        (2, 338.0, 365.0),
        (2, 359.0, 365.0),
    ];
    pub const SUBTRACTION_CODES: [At; 4] = [
        (2, 295.0, 305.0),
        (2, 316.0, 305.0),
        (2, 338.0, 305.0),
        (2, 359.0, 305.0),
    ];
    pub const STANDARD_CHECK: At = (2, 331.0, 258.0);
    /// The AGI factor: the digit before the decimal point, and the six after.
    pub const FACTOR_26E: (At, At) = ((2, 295.0, 186.0), (2, 331.0, 186.0));
    pub const FACTOR_29: (At, At) = ((2, 489.0, 150.0), (2, 538.0, 150.0));
    /// Page 3: the tax lines are whole dollars; payments carry cents.
    pub const TAX: &[(&str, At)] = &[
        ("32a", (3, 518.0, 678.0)),
        ("32b", (3, 518.0, 667.0)),
        ("32e", (3, 518.0, 631.0)),
        ("37", (3, 518.0, 571.0)),
        ("43", (3, 518.0, 499.0)),
    ];
    /// (line, dollars, cents).
    pub const PAYMENTS: &[(&str, At, At)] = &[
        ("44", (3, 518.0, 487.0), (3, 569.0, 487.0)),
        ("47", (3, 518.0, 451.0), (3, 569.0, 451.0)),
        ("49", (3, 518.0, 427.0), (3, 569.0, 427.0)),
        ("50", (3, 518.0, 415.0), (3, 569.0, 415.0)),
        ("51", (3, 518.0, 403.0), (3, 569.0, 403.0)),
        ("53", (3, 518.0, 379.0), (3, 569.0, 379.0)),
        ("55", (3, 518.0, 331.0), (3, 569.0, 331.0)),
    ];
}

/// Form 505NR's boxes.
mod at_505nr {
    use super::At;
    pub const LINES: &[(&str, At)] = &[
        ("nr1", (1, 518.0, 581.0)),
        ("nr2", (1, 518.0, 569.0)),
        ("nr3", (1, 411.0, 534.0)),
        ("nr3a", (1, 411.0, 522.0)),
        ("nr4", (1, 518.0, 511.0)),
        ("nr6a", (1, 518.0, 486.0)),
        ("nr6b", (1, 518.0, 463.0)),
        ("nr7", (1, 518.0, 450.0)),
        ("nr8", (1, 518.0, 438.0)),
        ("nr10a", (1, 411.0, 366.0)),
        ("nr11", (1, 518.0, 318.0)),
        ("nr12", (1, 518.0, 294.0)),
        ("nr13", (1, 518.0, 282.0)),
        ("nr14", (1, 518.0, 270.0)),
        ("nr16", (1, 518.0, 222.0)),
        ("nr17", (1, 518.0, 198.0)),
    ];
    pub const FACTOR_9: (At, At) = ((1, 489.0, 403.0), (1, 525.0, 403.0));
    pub const FACTOR_15: (At, At) = ((1, 489.0, 246.0), (1, 525.0, 246.0));
}

/// The name of the field at a position, or an error naming the position — a
/// box this module expects and the form does not have is a form that changed.
fn field(widgets: &[acroform::Widget], at: At) -> Result<String, FormError> {
    acroform::field_at(widgets, at.0, at.1, at.2)
        .map(str::to_string)
        .ok_or_else(|| FormError::NoSuchField(format!("page {} at ({}, {})", at.0, at.1, at.2)))
}

fn whole(cents: i64) -> String {
    round_dollars(cents).to_string()
}

fn put(
    doc: &mut lopdf::Document,
    map: &acroform::FieldMap,
    widgets: &[acroform::Widget],
    at: At,
    value: &str,
) -> Result<(), FormError> {
    let name = field(widgets, at)?;
    set_text(doc, map, &name, value)
}

fn put_factor(
    doc: &mut lopdf::Document,
    map: &acroform::FieldMap,
    widgets: &[acroform::Widget],
    at: (At, At),
    ppm: i64,
) -> Result<(), FormError> {
    put(doc, map, widgets, at.0, &(ppm / 1_000_000).to_string())?;
    put(doc, map, widgets, at.1, &format!("{:06}", ppm % 1_000_000))
}

fn tick(
    doc: &mut lopdf::Document,
    map: &acroform::FieldMap,
    widgets: &[acroform::Widget],
    at: At,
    on: &str,
) -> Result<(), FormError> {
    let name = field(widgets, at)?;
    set_check(doc, map, &name, on)
}

/// Fill Form 505 and Form 505NR, as two documents in that order.
pub fn fill(r: &Md505) -> Result<(lopdf::Document, lopdf::Document), FormError> {
    let params = params_for(r.tax_year).ok_or(FormError::NoFormForYear {
        form: "Maryland Form 505",
        year: r.tax_year,
        available: "2025".to_string(),
    })?;
    let mut doc = lopdf::Document::load_mem(params.form)?;
    strip_xfa(&mut doc);
    let map = acroform::field_map(&doc);
    let w = acroform::widgets(&doc, &map);

    let status_state = match r.filing_status {
        FilingStatus::Single => "Single",
        FilingStatus::MarriedFilingJointly => "Married filing joint return",
        FilingStatus::MarriedFilingSeparately => "Married filing separately",
        FilingStatus::HeadOfHousehold => "Head of Household",
        FilingStatus::QualifyingSurvivingSpouse => "Surviving Spouse",
    };
    set_check(&mut doc, &map, "Check Box 1", status_state)?;
    set_check(&mut doc, &map, "Check Box 100", "Yes")?;
    put(&mut doc, &map, &w, at_505::RESIDENCE_STATE, "IL")?;
    tick(&mut doc, &map, &w, at_505::SELF, "Yes")?;
    if r.filing_status == FilingStatus::MarriedFilingJointly {
        tick(&mut doc, &map, &w, at_505::SPOUSE, "Yes")?;
    }
    let _ = (at_505::YOU_65, at_505::SPOUSE_65, at_505::YOU_BLIND, at_505::SPOUSE_BLIND);
    for (i, (count, dollars)) in r.exemptions.iter().enumerate() {
        if *count > 0 || i == 3 {
            put(&mut doc, &map, &w, at_505::EXEMPTIONS[i].0, &count.to_string())?;
            put(&mut doc, &map, &w, at_505::EXEMPTIONS[i].1, &whole(*dollars))?;
        }
    }

    for row in &r.income {
        let Some((_, y)) = at_505::ROWS.iter().find(|(l, _)| *l == row.line) else {
            continue;
        };
        let cols = [row.federal_cents, row.maryland_cents, row.non_maryland_cents()];
        for (i, cents) in cols.iter().enumerate() {
            if *cents == 0 && row.line != "15" && row.line != "17" {
                continue;
            }
            put(&mut doc, &map, &w, (2, at_505::COLUMNS[i], *y), &whole(*cents))?;
        }
    }
    for (key, at) in at_505::RIGHT {
        let cents = r.cents(key);
        if cents != 0 || matches!(*key, "21" | "25" | "27" | "31") {
            put(&mut doc, &map, &w, *at, &whole(cents))?;
        }
    }
    for (code, at) in r.addition_codes.iter().zip(at_505::ADDITION_CODES) {
        put(&mut doc, &map, &w, at, code)?;
    }
    for (code, at) in r.subtraction_codes.iter().zip(at_505::SUBTRACTION_CODES) {
        put(&mut doc, &map, &w, at, code)?;
    }
    tick(&mut doc, &map, &w, at_505::STANDARD_CHECK, "Yes")?;
    put_factor(&mut doc, &map, &w, at_505::FACTOR_26E, r.agi_factor_ppm)?;
    put_factor(&mut doc, &map, &w, at_505::FACTOR_29, r.agi_factor_ppm)?;
    for (key, at) in at_505::TAX {
        put(&mut doc, &map, &w, *at, &whole(r.cents(key)))?;
    }
    for (key, dollars, cents) in at_505::PAYMENTS {
        let Some(line) = r.line(key) else { continue };
        if line.cents == 0 && *key != "49" {
            continue;
        }
        put(&mut doc, &map, &w, *dollars, &(line.cents / 100).to_string())?;
        put(&mut doc, &map, &w, *cents, &format!("{:02}", line.cents.abs() % 100))?;
    }

    let mut nr = lopdf::Document::load_mem(params.form_nr)?;
    strip_xfa(&mut nr);
    let nmap = acroform::field_map(&nr);
    let nw = acroform::widgets(&nr, &nmap);
    for (key, at) in at_505nr::LINES {
        put(&mut nr, &nmap, &nw, *at, &whole(r.cents(key)))?;
    }
    put_factor(&mut nr, &nmap, &nw, at_505nr::FACTOR_9, r.income_factor_ppm)?;
    put_factor(&mut nr, &nmap, &nw, at_505nr::FACTOR_15, r.nonresident_factor_ppm)?;
    Ok((doc, nr))
}

/// Form 505 with Form 505NR behind it, as one PDF.
pub fn build_pdf(r: &Md505) -> Result<Vec<u8>, FormError> {
    let (mut doc, mut nr) = fill(r)?;
    acroform::namespace_fields(&mut nr, "MD505NR");
    acroform::append_document(&mut doc, nr)?;
    let mut out = Vec::new();
    doc.save_to(&mut out)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tax table is the schedule at the middle of each $50 band, rounded —
    /// checked against rows of the printed 2025 table.
    #[test]
    fn the_tax_table_is_reproduced_from_the_schedule() {
        let p = &MD2025;
        let s = FilingStatus::Single;
        // "3,200 – 3,250 → 101", "6,200 – 6,250 → 243", "12,450 – 12,500 → 540".
        assert_eq!(tax(p, s, 320_000), 10_100);
        assert_eq!(tax(p, s, 324_999), 10_100);
        assert_eq!(tax(p, s, 620_000), 24_300);
        assert_eq!(tax(p, s, 1_245_000), 54_000);
        // Above the table, the schedule: $90 + 4.75% of the excess over $3,000.
        assert_eq!(tax(p, s, 6_000_000), 279_800); // $2,797.50, rounded
        assert_eq!(tax(p, s, 0), 0);
    }

    #[test]
    fn factors_follow_the_forms_rules() {
        assert_eq!(factor_ppm(0, 100), 0);
        assert_eq!(factor_ppm(-5, 100), 0);
        assert_eq!(factor_ppm(5, 0), 1_000_000);
        assert_eq!(factor_ppm(50, 100), 500_000);
        assert_eq!(factor_ppm(150, 100), 1_000_000);
    }

    #[test]
    fn the_exemption_shrinks_with_income_as_the_chart_says() {
        let p = &MD2025;
        assert_eq!(exemption_each(p, FilingStatus::Single, 9_000_000), 320_000);
        assert_eq!(exemption_each(p, FilingStatus::Single, 11_000_000), 160_000);
        assert_eq!(exemption_each(p, FilingStatus::Single, 20_000_000), 0);
        assert_eq!(exemption_each(p, FilingStatus::MarriedFilingJointly, 16_000_000), 160_000);
    }

    /// Every position this module fills lands on a field of the vendored forms —
    /// the check that a new revision moved a box.
    #[test]
    fn every_box_this_module_names_is_on_the_vendored_forms() {
        let doc = lopdf::Document::load_mem(MD2025.form).unwrap();
        let map = acroform::field_map(&doc);
        let w = acroform::widgets(&doc, &map);
        let mut points: Vec<At> = vec![
            at_505::RESIDENCE_STATE,
            at_505::SELF,
            at_505::SPOUSE,
            at_505::YOU_65,
            at_505::SPOUSE_65,
            at_505::YOU_BLIND,
            at_505::SPOUSE_BLIND,
            at_505::STANDARD_CHECK,
            at_505::FACTOR_26E.0,
            at_505::FACTOR_26E.1,
            at_505::FACTOR_29.0,
            at_505::FACTOR_29.1,
        ];
        for (a, b) in at_505::EXEMPTIONS {
            points.push(a);
            points.push(b);
        }
        for (_, y) in at_505::ROWS {
            // Lines 4, 9, 12 and 13 have no Maryland column: those incomes are
            // never Maryland-source for a nonresident.
            for x in at_505::COLUMNS {
                if (x - 418.0).abs() < 1.0 && [618.0, 546.0, 498.0, 474.0].contains(y) {
                    continue;
                }
                points.push((2, x, *y));
            }
        }
        points.extend(at_505::RIGHT.iter().map(|(_, a)| *a));
        points.extend(at_505::ADDITION_CODES);
        points.extend(at_505::SUBTRACTION_CODES);
        points.extend(at_505::TAX.iter().map(|(_, a)| *a));
        for (_, d, c) in at_505::PAYMENTS {
            points.push(*d);
            points.push(*c);
        }
        for at in points {
            assert!(field(&w, at).is_ok(), "Form 505 has no box at {at:?}");
        }
        // The two the names get wrong: line 47 is "43 Enter Dollars 5".
        assert_eq!(field(&w, (3, 518.0, 451.0)).unwrap(), "43 Enter Dollars 5");

        let nr = lopdf::Document::load_mem(MD2025.form_nr).unwrap();
        let nmap = acroform::field_map(&nr);
        let nw = acroform::widgets(&nr, &nmap);
        for (_, at) in at_505nr::LINES {
            assert!(field(&nw, *at).is_ok(), "Form 505NR has no box at {at:?}");
        }
        for at in [at_505nr::FACTOR_9, at_505nr::FACTOR_15] {
            assert!(field(&nw, at.0).is_ok() && field(&nw, at.1).is_ok());
        }
    }

    #[test]
    fn a_year_without_a_form_is_refused() {
        assert!(params_for(2024).is_none());
        assert!(params_for(2026).is_none());
    }
}
