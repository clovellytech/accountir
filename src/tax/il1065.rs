//! Illinois Form IL-1065, Partnership Replacement Tax Return.
//!
//! Illinois taxes partnerships a **1.5% Personal Property Replacement Tax** on
//! their base income, and offers an elective **4.95% Pass-through Entity (PTE)
//! tax**. IL-1065 computes both. It is filed with the Illinois Department of
//! Revenue, separately from the federal Form 1065 — so this produces its own PDF
//! rather than appending to the federal bundle.
//!
//! # Why it can be filled from what the books already hold
//!
//! IL-1065 starts from federal figures: Step 2 copies the partnership's ordinary
//! income and other Schedule K items straight off the federal return, and Steps 3
//! onward adjust from there. Those federal figures are exactly what
//! [`crate::tax::lines`] computes from the ledger, and the partners — whom Illinois
//! Schedule B lists — are already recorded. So the return is arithmetic on data we
//! have, plus two standing choices the books cannot infer (see below).
//!
//! # The two choices that change the return
//!
//! Held in [`Il1065Settings`] because they are the partnership's position, not one
//! year's figures, and they differ between businesses:
//!
//! - **Apportionment.** A wholly-Illinois partnership checks "inside Illinois only"
//!   and carries base income straight to the tax (Step 6 blank). One with income
//!   elsewhere must apportion, which needs sales-by-state figures the ledger does
//!   not hold — so that path fills the structure and leaves the sales lines blank,
//!   with a warning.
//! - **PTE election.** When elected, box I is checked and the 4.95% entity tax is
//!   computed alongside the replacement tax.
//!
//! # What this does not do
//!
//! The Illinois-specific adjustments — additions for state/municipal interest and
//! Illinois taxes deducted, subtractions for U.S. Treasury interest, special
//! depreciation, related-party expenses, net loss deduction, credits, and
//! nonresident pass-through withholding — are things the books do not know. Those
//! lines are left blank and editable, each named in a warning, exactly as the
//! federal return leaves an unmapped line. A filled IL-1065 that silently treated
//! them as zero would be worse than one that says which boxes still need a person.

use crate::domain::{BonusElection, BusinessProfile, DepreciableAsset, Il1065Settings};

use super::acroform::{field_map, set_check, set_text, strip_xfa, FieldMap, FormError};
use super::form1065::{Bundle, PartnerFiling};
use super::lines::{format_dollars, Form1065Lines};
use lopdf::Document;

const IL1065: &[u8] = include_bytes!("../../assets/il/il1065.pdf");

/// One tax year's IL-1065 blank.
///
/// # Why a year is needed at all
///
/// The vendored form prints, on its own first page, "This form is for tax years
/// ending on or after December 31, 2025, and before December 31, 2026." Filling
/// it with an earlier year's figures produces a document that states on its face
/// that it is not for the year it carries — and Illinois renumbers its lines
/// between revisions exactly as the IRS does.
///
/// # Why every revision can share one set of box names
///
/// Illinois names its fields in plain language, and the names carry the line
/// arithmetic with them — `Add L36 - L37`, `Divide L47 - L50 - a - 1`. A
/// renumbering would therefore show up *in the names*, not silently behind them,
/// which is the opposite of the IRS forms where `f1_19[0]` is a position and
/// means whatever the current revision put there.
///
/// Checked rather than assumed: all 227 names are identical across the three
/// revisions carried, the printed line numbers on pages 1–3 match one for one,
/// and 2024's boxes are in exactly the same places as 2025's. The 2023 form
/// reflows 78 rows vertically — same column, same name, a few points up or down
/// — which is a page laid out afresh, not a form renumbered.
///
/// So what the year table gates is not the box map but the *paper*: each blank
/// prints its own year and the range of tax years it may be filed for.
pub struct Il1065Year {
    pub year: i32,
    pub form: &'static [u8],
}

/// The revisions carried, oldest first.
pub const IL1065_YEARS: &[Il1065Year] = &[
    Il1065Year {
        year: 2023,
        form: include_bytes!("../../assets/il/2023/il1065.pdf"),
    },
    Il1065Year {
        year: 2024,
        form: include_bytes!("../../assets/il/2024/il1065.pdf"),
    },
    Il1065Year {
        year: crate::tax::form1065::FORM_TAX_YEAR,
        form: IL1065,
    },
];

/// The blank for a year, or `None` when none is carried.
pub fn il1065_year(year: i32) -> Option<&'static Il1065Year> {
    IL1065_YEARS.iter().find(|f| f.year == year)
}

/// The years an IL-1065 can be produced for.
pub fn supported_years() -> Vec<i32> {
    IL1065_YEARS.iter().map(|f| f.year).collect()
}

/// Illinois' replacement-tax rate, 1.5%, as a numerator over 1000.
const REPLACEMENT_TAX_PER_MILLE: i64 = 15;
/// Illinois' PTE-tax rate, 4.95%, as a numerator over 10_000.
const PTE_TAX_PER_TEN_THOUSAND: i64 = 495;
/// Step 7, line 52: the standard exemption, in whole dollars, before line 51's
/// apportionment fraction.
const STANDARD_EXEMPTION: i64 = 1_000;
/// Unmodified base income (Step 3, line 13) above which the standard exemption
/// is $0.
const STANDARD_EXEMPTION_CEILING: i64 = 250_000;

/// Illinois Schedule B, Section B prints three members per page; more need a
/// continuation page, which this does not produce — see [`build`].
const SCHEDULE_B_ROWS: usize = 3;

// ---------------------------------------------------------------------------
// Field names — the form names them in plain language, so the constants read as
// the boxes do. Checked against the vendored PDF by the tests at the bottom.
// ---------------------------------------------------------------------------

mod f {
    // Step 1 — identify the partnership.
    pub const LEGAL_NAME: &str = "Name change";
    pub const MAILING_ADDRESS: &str = "Mailing address";
    pub const MAILING_CITY: &str = "Mailing city";
    pub const MAILING_STATE: &str = "Mailing state";
    pub const MAILING_ZIP: &str = "Mailing ZIP";
    pub const FEIN_2: &str = "Your-FEIN2";
    pub const FEIN_7: &str = "Your-FEIN7";
    pub const NAICS: &str = "NAICS";
    pub const RECORDS_CITY: &str = "Accounting records - city";
    pub const RECORDS_STATE: &str = "Accounting records - state";
    pub const RECORDS_ZIP: &str = "Accounting records - ZIP";
    pub const PTE_BOX: &str = "File/Pay pass-through entity tax";
    pub const PTE_BOX_ON: &str = "File/Pay Pass-through Entity Tax";

    // Step 2 — ordinary income or loss.
    pub const L1_ORDINARY: &str = "Ordinary income/loss";
    pub const L2_RENTAL_RE: &str = "Rental net income/loss";
    pub const L3_OTHER_RENTAL: &str = "Other net income/loss";
    pub const L4_PORTFOLIO: &str = "Portfolio income/loss";
    pub const L5_1231: &str = "IRC gain/loss";
    pub const L7_TOTAL_ORDINARY: &str = "Total ordinary income";

    // Step 3 — unmodified base income.
    pub const L8_CHARITABLE: &str = "Charitable contributions";
    pub const L9_SECTION179: &str = "Expense deduction";
    pub const L10_INVEST_INTEREST: &str = "Investment indebtness";
    pub const L12_ADD_8_11: &str = "Add L8 - L11";
    pub const L13_UNMODIFIED_BASE: &str = "Total base income/loss";

    // Step 4 — additions.
    pub const L14_FROM_L13: &str = "Amounts - L13";
    pub const L16_ILLINOIS_TAXES: &str = "Illinois taxes deducted";
    pub const L17_SPECIAL_DEPRECIATION: &str = "Illinois Special Depreciation";
    pub const L20_GUARANTEED: &str = "Guaranteed payments";
    pub const L23_INCOME: &str = "Income/loss";

    // Step 5 — subtractions.
    pub const L30_SPECIAL_DEPRECIATION: &str = "IL Special Depreciation";
    pub const L34_TOTAL_SUBTRACT: &str = "Ttl subtract";
    pub const L35_BASE_INCOME: &str = "Bse income/loss";

    // The STOP box: inside Illinois only, or apportion.
    pub const INSIDE_OUTSIDE: &str = "Inside/Outside Illinois";
    pub const INSIDE_ON: &str = "Inside Illinois";
    pub const OUTSIDE_ON: &str = "Outside Illinois";

    // Step 6 — income allocable to Illinois (apportionment).
    pub const L36_NONBUSINESS: &str = "Nonbusiness income/loss";
    pub const L37_NONUNITARY: &str = "Business income/loss";
    pub const L38_ADD_36_37: &str = "Add L36 - L37";
    pub const L39_BUSINESS: &str = "Subtract L38 - L35";

    // Step 7 — net income.
    pub const L47_BASE: &str = "Base income/loss";
    pub const L49_AFTER_NLD: &str = "Income after NLD";
    pub const L50_FROM_L35: &str = "Amount - L35 - S5";
    pub const L51_RATIO_WHOLE: &str = "Divide L47 - L50 - a - 1";
    pub const L51_RATIO_FRAC: &str = "Divide L47 - L50 - a - 2";
    pub const L52_EXEMPTION: &str = "Exemption allowance";
    pub const L53_NET_INCOME: &str = "Net income";

    // Step 8 — net replacement tax.
    pub const L54_REPLACEMENT: &str = "Replacement tax";
    pub const L56_BEFORE_CREDITS: &str = "Before replacement tax";
    pub const L58_NET_REPLACEMENT: &str = "Net replacement tax";

    // Step 9 — taxes, withholding, PTE.
    pub const L59_TOTAL_WITHHOLDING: &str = "Total withholding";
    pub const L60_PTE_INCOME: &str = "Pass-through entity income";
    pub const L61_PTE_TAX: &str = "Pass-through entity tax";
    pub const L62_TOTAL_TAX: &str = "Total net replacement tax";
    pub const L64_TOTAL: &str = "Total taxes, surcharge";

    // Step 10 — payments, and what is still owed.
    pub const L66_TOTAL_PAYMENTS: &str = "Ttl payments";
    pub const L71_TAX_DUE: &str = "Amount tax due - owe";

    // Step 1, box Q: Form IL-4562 is attached.
    pub const IL4562_ATTACHED: &str = "Form IL-452 chk box";
    pub const IL4562_ON: &str = "Form IL-4562";

    // Step 11, paid preparer: the firm's name box.
    pub const PREPARER_FIRM_NAME: &str = "Preparer firm name";

    // Schedule B header (Section B, page 5).
    pub const SCHB_NAME: &str =
        "Schedule B. Enter your name as shown on your Form IL-1065 or Form IL-1120-ST";
    pub const SCHB_FEIN_2: &str = "Schedule B. Enter the initial two digits of your FEIN";
    pub const SCHB_FEIN_7: &str = "Schedule B. Enter the last seven digits of your FEIN";

    // Schedule B header (Section A, page 4) — the same identity, echoed.
    pub const SCHA_NAME: &str = "PSI name";
    pub const SCHA_FEIN_2: &str = "PSI FEIN-2";
    pub const SCHA_FEIN_7: &str = "PSI FEIN-7";
    /// Section A, line 3: column E totalled over the members whose column D is
    /// checked. The form's own field name, misleading as it is.
    pub const SCHA_L3_SUBJECT_SHARE: &str = "PSI nonresidents shareholder amounts";
}

/// The Schedule B, Section B field for member `n` (1-based) with a shared suffix.
fn member(n: usize, suffix: &str) -> String {
    format!("Schedule B, Section B, Member {n}{suffix}")
}

/// Member `n`'s address line 1. The form's own field name embeds "Member 1" in
/// every row's label — a copy-paste in the PDF, not our mistake — so the suffix is
/// constant and only the "Member {n}" prefix changes.
fn member_address1(n: usize) -> String {
    member(
        n,
        " - Identify your partners or shareholders. Enter the address Member 1 information here",
    )
}

// Suffixes shared by every member column. The spacing and misspellings are the
// form's own; they are transcribed exactly and the tests verify each one exists.
const M_NAME: &str =
    " - Identify your partners or shareholders.  Enter the name of the partner or shareholder";
const M_ADDR2: &str =
    " - Identify your partners or shareholders. Enter the address line 2 information here";
const M_CITY: &str = " - Identify your partners or shareholders. Enter the city";
const M_STATE: &str = " - Identify your partners or shareholders. Enter the state";
const M_ZIP: &str = " - Identify your partners or shareholders. Enter the zip code";
const M_COL_B_TYPE: &str = ", Column B - Partner or Shareholder type. See instructions";
const M_COL_C_TIN: &str = ", Column C - Social Security number or Federal Employer Identification \
                           Number of the partner or shareholder";
const M_COL_D_SUBJECT: &str = ", Column D - Check if your partner or shareholder is subject to \
                               Illinois replacment tax or is an ESOP";
const M_COL_D_ON: &str = "Yes";
const M_COL_E_SHARE: &str = ", Column E - Member's distributable amount of base income or loss";
const M_COL_F_EXCLUDED: &str =
    ", Column F -  Excluded from pass-through withholding payments.  See instructions";

// ---------------------------------------------------------------------------
// The figures IL-1065 computes, so a test can check the arithmetic without
// reading them back out of a PDF.
// ---------------------------------------------------------------------------

/// Every whole-dollar line this return computes from the federal figures and the
/// settings. Lines the books cannot know (Illinois adjustments, apportionment
/// sales, credits, withholding) are absent here and left blank on the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Figures {
    pub line1: i64,
    pub line2: i64,
    pub line3: i64,
    pub line4: i64,
    pub line5: i64,
    pub line7: i64,
    pub line8: i64,
    pub line9: i64,
    pub line10: i64,
    pub line12: i64,
    pub line13: i64,
    /// Illinois income and replacement tax the federal return deducted.
    pub line16: i64,
    /// Illinois special depreciation addition — Form IL-4562, Step 2, line 4.
    pub line17: i64,
    pub line20: i64,
    pub line23: i64,
    /// Illinois special depreciation subtraction — Form IL-4562, Step 3, line 19.
    pub line30: i64,
    pub line35: i64,
    /// Base income the tax is figured on. Equals line 35 for an Illinois-only
    /// partnership; `None` when apportioning, because it depends on sales figures
    /// the books do not hold.
    pub line47: Option<i64>,
    /// The standard exemption: $1,000 times line 51, or $0 when unmodified base
    /// income (line 13) is over $250,000.
    pub line52: Option<i64>,
    pub line53: Option<i64>,
    pub line54: Option<i64>,
    pub line58: Option<i64>,
    pub line61: Option<i64>,
    pub line62: Option<i64>,
}

/// Compute the return's figures from the federal Schedule K totals and settings.
pub fn figures(federal: &Form1065Lines, settings: &Il1065Settings) -> Figures {
    figures_with(federal, settings, &SpecialDepreciation::default(), 0)
}

/// [`figures`], with the Illinois special depreciation adjustments from the asset
/// register carried onto lines 17 and 30.
pub fn figures_with(
    federal: &Form1065Lines,
    settings: &Il1065Settings,
    special: &SpecialDepreciation,
    illinois_taxes: i64,
) -> Figures {
    // Step 2 — straight off federal Schedule K. Portfolio income is interest,
    // dividends, royalties and net capital gains; §1231 is its own line 5.
    let line1 = federal.k_line_1();
    let line2 = federal.get("k2");
    let line3 = federal.k_line_3c();
    let line4 = federal.get("k5")
        + federal.get("k6a")
        + federal.get("k7")
        + federal.get("k8")
        + federal.get("k9a");
    let line5 = federal.get("k10");
    let line7 = line1 + line2 + line3 + line4 + line5;

    // Step 3 — federal deductions added back to reach the base.
    let line8 = federal.get("k13a") + federal.get("k13b");
    let line9 = federal.get("k12");
    let line10 = federal.get("k13c");
    let line12 = line8 + line9 + line10;
    let line13 = line7 - line12;

    // Step 4 — additions. Guaranteed payments are a federal figure and special
    // depreciation comes from the asset register; the other Illinois-specific
    // additions are left blank.
    let line14 = line13;
    // Line 16: Illinois income and replacement tax the federal return deducted —
    // see `illinois_taxes_deducted`.
    let line16 = illinois_taxes;
    let line17 = special.addition_dollars();
    let line20 = federal.k_line_4c();
    let line23 = line14 + line16 + line17 + line20;

    // Step 5 — subtractions. Only special depreciation is known here.
    let line30 = special.subtraction_dollars();
    let line34 = line30;
    let line35 = line23 - line34;

    // Steps 6–9 depend on apportionment. Only the Illinois-only path can be
    // carried through to the tax, because the apportioned path needs sales.
    if settings.apportions_outside_illinois {
        return Figures {
            line1,
            line2,
            line3,
            line4,
            line5,
            line7,
            line8,
            line9,
            line10,
            line12,
            line13,
            line16,
            line17,
            line20,
            line23,
            line30,
            line35,
            line47: None,
            line52: None,
            line53: None,
            line54: None,
            line58: None,
            line61: None,
            line62: None,
        };
    }

    // Illinois-only: base income flows through Step 7, and the replacement tax is
    // 1.5% of it after the standard exemption. Line 51 is exactly one, so the
    // exemption is the whole $1,000 — or nothing when unmodified base income is
    // over $250,000 — and it never deepens a loss. A first or final short year
    // keeps the full exemption, per the instructions; a change of year end,
    // which would prorate it, is not something this program models. A net loss
    // owes no tax.
    let line47 = line35; // line 48 NLD = 0, so line 49 is line 47
    let line52 = if line13 > STANDARD_EXEMPTION_CEILING {
        0
    } else {
        STANDARD_EXEMPTION
    };
    let line53 = if line47 <= 0 {
        line47
    } else {
        (line47 - line52).max(0)
    };
    let line54 = round_rate(line53.max(0), REPLACEMENT_TAX_PER_MILLE, 1000);
    // The PTE tax is figured on base income, not on the replacement tax's net
    // income: the exemption belongs to the replacement tax alone.
    let taxable = line47.max(0);
    let line58 = line54;

    let line61 = if settings.elects_pte_tax {
        round_rate(taxable, PTE_TAX_PER_TEN_THOUSAND, 10_000)
    } else {
        0
    };
    let line62 = line58 + line61; // line 59 withholding = 0 here

    Figures {
        line1,
        line2,
        line3,
        line4,
        line5,
        line7,
        line8,
        line9,
        line10,
        line12,
        line13,
        line16,
        line17,
        line20,
        line23,
        line30,
        line35,
        line47: Some(line47),
        line52: Some(line52),
        line53: Some(line53),
        line54: Some(line54),
        line58: Some(line58),
        line61: Some(line61),
        line62: Some(line62),
    }
}

/// Illinois special depreciation for a year: Form IL-4562's addition and
/// subtraction, one row per property that took federal bonus depreciation.
///
/// # What Illinois does with bonus depreciation
///
/// Illinois does not follow federal bonus depreciation. In the year it is taken
/// the bonus is added back (Step 2, line 1). It is then recovered over the
/// property's life: each year, including the first, a share of the federal
/// regular depreciation on that property is subtracted — the depreciation the
/// added-back amount would itself have earned. Form IL-4562 prints the share as a
/// factor of the regular depreciation for each bonus rate: 42.9% for 30% bonus,
/// 66.7% for 40%, 100% for 50%, 150% for 60%, 400% for 80%; for 100% bonus it is
/// the depreciation that would have been taken without bonus (line 16). In the
/// last year of regular depreciation the property settles up: the original
/// addition is subtracted (line 18) and every subtraction already taken on it is
/// added back (line 3), so over its life the subtractions equal the addition.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpecialDepreciation {
    pub rows: Vec<SpecialRow>,
}

/// One property's Illinois special depreciation adjustments for the year, in cents.
#[derive(Debug, Clone, PartialEq)]
pub struct SpecialRow {
    pub description: String,
    pub placed_in_service: chrono::NaiveDate,
    pub bonus_rate: f64,
    /// Step 2, line 1: the federal bonus taken this year.
    pub addition_cents: i64,
    /// This year's federal regular depreciation on the property.
    pub regular_cents: i64,
    /// Step 3's factor for the bonus rate; `None` for 100% bonus (line 16).
    pub factor: Option<f64>,
    /// Step 3's subtraction for the year before any last-year settlement.
    pub subtraction_cents: i64,
    /// Step 2, line 3 — last year only: the subtractions already taken.
    pub last_year_addition_cents: i64,
    /// Step 3, line 18 — last year only: the original addition.
    pub last_year_subtraction_cents: i64,
}

impl SpecialDepreciation {
    /// Line 17 — Form IL-4562, Step 2, line 4, figured as the form figures it.
    pub fn addition_dollars(&self) -> i64 {
        super::il4562::Lines::from_special(self).l4
    }

    /// Line 30 — Form IL-4562, Step 3, line 19, figured as the form figures it.
    pub fn subtraction_dollars(&self) -> i64 {
        super::il4562::Lines::from_special(self).l19
    }
}

/// Form IL-4562's Step 3 factor for a bonus rate, or `None` for 100% bonus.
///
/// A rate the form does not print is given the ratio its printed factors round:
/// the bonus over what is left after it.
fn illinois_factor(rate: f64) -> Option<f64> {
    match (rate * 100.0).round() as i64 {
        30 => Some(0.429),
        40 => Some(0.667),
        50 => Some(1.0),
        60 => Some(1.5),
        80 => Some(4.0),
        100 => None,
        _ => Some(rate / (1.0 - rate)),
    }
}

/// Figure Illinois special depreciation for `year` from the asset register.
pub fn special_depreciation(assets: &[DepreciableAsset], year: i32) -> SpecialDepreciation {
    use super::depreciation::compute_year;
    use chrono::Datelike;

    let row_of = |schedule: &super::depreciation::YearSchedule<'_>, id: &str| {
        schedule
            .rows
            .iter()
            .find(|r| r.asset.asset_id == id)
            .map(|r| (r.bonus_cents, r.bonus_rate, r.macrs_cents))
    };

    let mut rows = Vec::new();
    for asset in assets {
        let placed = asset.placed_in_service.year();
        if placed > year || !asset.held_during(year) {
            continue;
        }
        let Some((bonus, rate, _)) = row_of(&compute_year(assets, placed), &asset.asset_id) else {
            continue;
        };
        if bonus <= 0 {
            continue;
        }
        let factor = illinois_factor(rate);

        // The Step 3 amount for one year: the factor times that year's federal
        // regular depreciation, or for 100% bonus the depreciation the property
        // would have earned without it.
        let step_three = |y: i32| -> i64 {
            match factor {
                Some(f) => row_of(&compute_year(assets, y), &asset.asset_id)
                    .map(|(_, _, macrs)| (macrs as f64 * f).round() as i64)
                    .unwrap_or(0),
                None => {
                    let alternative: Vec<DepreciableAsset> = assets
                        .iter()
                        .map(|a| {
                            let mut a = a.clone();
                            if a.asset_id == asset.asset_id {
                                a.bonus = BonusElection::Decline;
                            }
                            a
                        })
                        .collect();
                    row_of(&compute_year(&alternative, y), &asset.asset_id)
                        .map(|(_, _, macrs)| macrs)
                        .unwrap_or(0)
                }
            }
        };

        let regular = row_of(&compute_year(assets, year), &asset.asset_id)
            .map(|(_, _, macrs)| macrs)
            .unwrap_or(0);
        let subtraction = step_three(year);
        let addition = if year == placed { bonus } else { 0 };
        let last_year = asset.disposed_during(year) || (subtraction != 0 && step_three(year + 1) == 0);
        let (last_add, last_sub) = if last_year {
            let taken: i64 = (placed..year).map(step_three).sum();
            (taken + subtraction, bonus)
        } else {
            (0, 0)
        };
        if addition == 0 && subtraction == 0 && last_add == 0 && last_sub == 0 {
            continue;
        }
        rows.push(SpecialRow {
            description: asset.description.clone(),
            placed_in_service: asset.placed_in_service,
            bonus_rate: rate,
            addition_cents: addition,
            regular_cents: regular,
            factor,
            subtraction_cents: subtraction,
            last_year_addition_cents: last_add,
            last_year_subtraction_cents: last_sub,
        });
    }
    SpecialDepreciation { rows }
}

/// The Form IL-4562 figures, property by property, behind the return.
fn special_statement(
    profile: &BusinessProfile,
    year: i32,
    special: &SpecialDepreciation,
) -> Result<Option<Document>, FormError> {
    use super::lines::cents_to_dollars;
    use super::statement::{build_table, Column, TableLine, TableStatement};

    if special.rows.is_empty() {
        return Ok(None);
    }
    let dollars = |c: i64| format_dollars(cents_to_dollars(c));
    let column = |title: &str, x: f32, right: bool| Column {
        title: title.to_string(),
        x,
        right,
    };
    let mut lines: Vec<TableLine> = special
        .rows
        .iter()
        .flat_map(|r| {
            // The whole description: as much as the column holds, and the rest on
            // the line beneath rather than cut off.
            let (head, rest) = fit_words(&r.description, PROPERTY_WIDTH);
            let cells = TableLine::Cells(vec![
                head,
                r.placed_in_service.to_string(),
                format!("{:.0}%", r.bonus_rate * 100.0),
                dollars(r.addition_cents),
                dollars(r.regular_cents),
                r.factor
                    .map(|f| format!("x {f}"))
                    .unwrap_or_else(|| "line 16".to_string()),
                dollars(r.subtraction_cents),
                dollars(r.last_year_addition_cents),
                dollars(r.last_year_subtraction_cents),
            ]);
            std::iter::once(cells).chain(rest.map(TableLine::Note))
        })
        .collect();
    let sum = |f: fn(&SpecialRow) -> i64| special.rows.iter().map(f).sum::<i64>();
    lines.push(TableLine::Cells(vec![
        "Total".to_string(),
        String::new(),
        String::new(),
        dollars(sum(|r| r.addition_cents)),
        String::new(),
        String::new(),
        dollars(sum(|r| r.subtraction_cents)),
        dollars(sum(|r| r.last_year_addition_cents)),
        dollars(sum(|r| r.last_year_subtraction_cents)),
    ]));

    build_table(&TableStatement {
        legal_name: &profile.legal_name,
        ein: &profile.ein,
        heading: format!("Form IL-1065 ({year}) — Illinois special depreciation (Form IL-4562)"),
        subheading: format!(
            "Line 17 addition {}   ·   Line 30 subtraction {}",
            format_dollars(special.addition_dollars()),
            format_dollars(special.subtraction_dollars())
        ),
        columns: vec![
            column("Property", 54.0, false),
            column("In service", 250.0, false),
            column("Bonus", 330.0, true),
            column("Bonus added back", 410.0, true),
            column("Regular depr.", 480.0, true),
            column("Factor", 492.0, false),
            column("Subtraction", 600.0, true),
            column("Last yr: add", 672.0, true),
            column("Last yr: subtract", 744.0, true),
        ],
        lines,
        footnotes: vec![
            "Step 2, line 1 is the federal bonus (Form 4562, line 14) in the year it was taken. \
             Step 3 subtracts the factor Form IL-4562 prints for the bonus rate times the year's \
             federal regular depreciation on the property, or for 100% bonus the depreciation \
             that would have been taken without it. In the last year of regular depreciation \
             line 18 subtracts the original addition and line 3 adds back the subtractions \
             already taken."
                .to_string(),
        ],
    })
}

/// `amount × num / den`, rounded half up. Only called on non-negative amounts —
/// tax on a loss is zero, and the caller clamps before dividing.
fn round_rate(amount: i64, num: i64, den: i64) -> i64 {
    (amount * num + den / 2) / den
}

/// Characters of a property description the special depreciation statement's
/// first column holds.
const PROPERTY_WIDTH: usize = 40;

/// Split text at the last space that keeps the first part within `width`
/// characters, returning the remainder when there is one.
fn fit_words(s: &str, width: usize) -> (String, Option<String>) {
    if s.chars().count() <= width {
        return (s.to_string(), None);
    }
    let mut head = String::new();
    let mut rest = Vec::new();
    for word in s.split_whitespace() {
        if rest.is_empty() && head.chars().count() + 1 + word.chars().count() <= width {
            if !head.is_empty() {
                head.push(' ');
            }
            head.push_str(word);
        } else {
            rest.push(word);
        }
    }
    if head.is_empty() {
        // One word longer than the column: it prints whole.
        return (s.to_string(), None);
    }
    (head, Some(rest.join(" ")).filter(|r| !r.is_empty()))
}

/// One partner's share of the return, in whole dollars: what their Schedule K-1-P
/// carries and what Illinois Schedule B column E adds up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberShares {
    /// Line 1 — ordinary income, divided as the federal K-1s divide it: by the
    /// year's fixed or preferred shares where the agreement sets them, otherwise
    /// on the profit percentages.
    pub ordinary: i64,
    /// Line 20 — guaranteed payments.
    pub guaranteed: i64,
    /// Line 16 — Illinois income and replacement tax added back.
    pub illinois_taxes: i64,
    /// Line 17 — special depreciation addition.
    pub special_addition: i64,
    /// Line 30 — special depreciation subtraction.
    pub special_subtraction: i64,
    /// Schedule B column E: ordinary income plus a profit-percentage share of
    /// everything else between line 1 and line 35. The column adds back to line 35.
    pub base_income: i64,
    /// Schedule K-1-P line 8: the year-end profit percentage, which the Step 5
    /// amounts follow.
    pub share_ppm: i64,
}

/// Every partner's [`MemberShares`], in the order given.
///
/// The Step 5 amounts are each line of the return times the partner's
/// percentage, as the K-1-P instructions direct; under a preferred or fixed
/// division of ordinary income that is the split every dollar past the fixed
/// amounts follows.
pub fn member_shares(
    figs: &Figures,
    partners: &[PartnerFiling],
    year: i32,
    fixed: &[crate::domain::FixedAllocation],
) -> Vec<MemberShares> {
    use super::allocate::{allocate_as_of, split_fixed, Basis};
    let members: Vec<&crate::domain::Partner> = partners.iter().map(|f| &f.partner).collect();
    let (_, year_end) = crate::commands::partnership_commands::calendar_year(year);
    let by_profit = |total: i64| -> Vec<i64> {
        let mut out = vec![0; members.len()];
        if total != 0 {
            for s in allocate_as_of(total, &members, Basis::ProfitOrLoss, Some(year_end)) {
                out[s.partner] = s.dollars;
            }
        }
        out
    };
    let ordinary = match split_fixed(figs.line1, &members, year, fixed, false) {
        Some(shares) => {
            let mut out = vec![0; members.len()];
            for s in shares {
                out[s.partner] = s.dollars;
            }
            out
        }
        None => by_profit(figs.line1),
    };
    let guaranteed = by_profit(figs.line20);
    let taxes = by_profit(figs.line16);
    let addition = by_profit(figs.line17);
    let subtraction = by_profit(figs.line30);
    let rest = by_profit(figs.line35 - figs.line1);
    members
        .iter()
        .enumerate()
        .map(|(i, p)| MemberShares {
            ordinary: ordinary[i],
            guaranteed: guaranteed[i],
            illinois_taxes: taxes[i],
            special_addition: addition[i],
            special_subtraction: subtraction[i],
            base_income: ordinary[i] + rest[i],
            share_ppm: p.shares_on(year_end).profit_ppm,
        })
        .collect()
}

/// A partner's share of a whole-dollar figure, in ppm, rounded to the dollar.
fn share_of(dollars: i64, ppm: i64) -> i64 {
    let n = dollars * ppm;
    let half = 500_000;
    if n >= 0 {
        (n + half) / 1_000_000
    } else {
        -((-n + half) / 1_000_000)
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

/// Build a filled IL-1065 (with Illinois Schedule B) as its own PDF.
///
/// Fills identity, the income and base-income steps from the federal figures, the
/// replacement tax (and PTE tax when elected), and Schedule B Section B from the
/// partners. Leaves every Illinois-specific adjustment, apportionment sales figure,
/// credit and withholding line blank and editable, each named in a warning.
///
/// More than [`SCHEDULE_B_ROWS`] partners fill the first three and warn: the
/// printed Section B has three rows and Illinois wants a continuation page for the
/// rest, which this does not produce — the same rule federal Schedule B-1 follows.
pub fn build(
    profile: &BusinessProfile,
    partners: &[PartnerFiling],
    federal: &Form1065Lines,
    settings: &Il1065Settings,
    year: i32,
) -> Result<Bundle, FormError> {
    build_with_special(
        profile,
        partners,
        federal,
        settings,
        year,
        &SpecialDepreciation::default(),
        &[],
        0,
    )
}

/// [`build`], with Illinois special depreciation from the asset register on lines
/// 17 and 30 and the Form IL-4562 figures behind the return.
pub fn build_with_special(
    profile: &BusinessProfile,
    partners: &[PartnerFiling],
    federal: &Form1065Lines,
    settings: &Il1065Settings,
    year: i32,
    special: &SpecialDepreciation,
    fixed: &[crate::domain::FixedAllocation],
    illinois_taxes: i64,
) -> Result<Bundle, FormError> {
    // The year's own blank, or none. Refused rather than substituted, for the
    // reason the federal forms are: Illinois renumbers between revisions, and
    // this one says on its first page which years it is for.
    let revision = il1065_year(year).ok_or_else(|| FormError::NoFormForYear {
        form: "Form IL-1065",
        year,
        available: supported_years()
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    })?;

    let mut warnings = Vec::new();
    let figs = figures_with(federal, settings, special, illinois_taxes);

    let mut doc = Document::load_mem(revision.form)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    fill_identity(&mut doc, &map, profile, settings)?;
    // A return the partnership prepares itself has no paid preparer, and the firm
    // box says so rather than being left blank — as on the federal return.
    set_text(
        &mut doc,
        &map,
        f::PREPARER_FIRM_NAME,
        super::form1065::SELF_PREPARED,
    )?;
    if !special.rows.is_empty() {
        set_check(&mut doc, &map, f::IL4562_ATTACHED, f::IL4562_ON)?;
    }
    fill_income(&mut doc, &map, &figs, &mut warnings)?;
    fill_tax(&mut doc, &map, &figs, settings, &mut warnings)?;
    fill_schedule_b(
        &mut doc,
        &map,
        profile,
        partners,
        &figs,
        year,
        fixed,
        &mut warnings,
    )?;

    warnings.extend(caveats(profile, settings, partners.len()));

    if !special.rows.is_empty() {
        warnings.push(format!(
            "Illinois special depreciation: line 17 adds back {} and line 30 subtracts {}, as \
             Form IL-4562 figures them from the asset register — the federal bonus depreciation \
             added back in the year taken, recovered through a share of each later year's \
             regular depreciation, and settled in the property's last year. The figures are on \
             the statement behind Form IL-4562, and box Q on page 1 is checked.",
            format_dollars(figs.line17),
            format_dollars(figs.line30)
        ));
        match super::il4562::build(profile, year, special)? {
            Some(mut form) => {
                super::acroform::namespace_fields(&mut form, "IL4562");
                super::acroform::append_document(&mut doc, form)?;
            }
            None => warnings.push(format!(
                "Form IL-4562 is not filled for {year}: this program carries the {} blank only. \
                 Complete it by hand from the statement behind the return.",
                super::il4562::FORM_YEAR
            )),
        }
        if let Some(page) = special_statement(profile, year, special)? {
            super::acroform::append_document(&mut doc, page)?;
        }
    }

    // Schedule K-1-P for each partner, behind everything that is filed.
    if year == super::il_k1p::FORM_YEAR {
        let shares = member_shares(&figs, partners, year, fixed);
        for (i, (filing, share)) in partners.iter().zip(&shares).enumerate() {
            let (mut k1p, k1p_warnings) = super::il_k1p::build(
                profile,
                year,
                filing,
                share,
                settings.apportions_outside_illinois,
            )?;
            super::acroform::namespace_fields(&mut k1p, &format!("K1P_{}", i + 1));
            super::acroform::append_document(&mut doc, k1p)?;
            warnings.extend(k1p_warnings);
        }
        if !partners.is_empty() {
            warnings.push(format!(
                "Schedule K-1-P for each of the {} partners follows the return. Give each partner \
                 theirs with Schedule K-1-P(2) by the IL-1065's due date; they are not mailed with \
                 the return. Line 8 is the year-end profit percentage, which Step 5 follows; where \
                 ordinary income on line 20 is divided in fixed or preferred amounts instead, the \
                 instructions ask for that allocation to be explained on a sheet attached to the \
                 schedule.{}",
                partners.len(),
                if settings.apportions_outside_illinois {
                    " Line 4 and Column B are blank: the apportionment factor is not in the books."
                } else {
                    ""
                }
            ));
        }
    } else if !partners.is_empty() {
        warnings.push(format!(
            "Schedule K-1-P is not produced for {year}: this program carries the {} blank only.",
            super::il_k1p::FORM_YEAR
        ));
    }

    let mut pdf = Vec::new();
    doc.save_to(&mut pdf)?;
    let page_count = doc.get_pages().len();
    Ok(Bundle {
        pdf,
        warnings,
        page_count,
        k1s: Vec::new(),
    })
}

/// Build an IL-1065 from the ledger: read the year's federal figures the same way
/// the federal return does, load the partners and their TINs, and fill the form.
///
/// The Illinois entry point that a filer actually uses. Mirrors
/// [`crate::tax::form1065::build_return_from_ledger`] — the federal figures come
/// from one income statement through [`crate::tax::lines::compute`], so the two
/// returns cannot disagree about the same number.
pub fn build_from_ledger(
    conn: &rusqlite::Connection,
    year: i32,
    settings: &Il1065Settings,
) -> Result<Bundle, FormError> {
    use crate::commands::partnership_commands as pc;

    let profile = pc::get_profile(conn).ok_or_else(|| {
        FormError::Malformed(
            "the partnership's details have not been set — no legal name or FEIN for the return"
                .to_string(),
        )
    })?;

    let (year_start, year_end) = (
        chrono::NaiveDate::from_ymd_opt(year, 1, 1).expect("January 1 exists in every year"),
        chrono::NaiveDate::from_ymd_opt(year, 12, 31).expect("December 31 exists in every year"),
    );
    let statement = crate::queries::reports::Reports::new(conn)
        .income_statement(year_start, year_end)
        .map_err(|e| FormError::Malformed(format!("income statement: {e}")))?;
    let mapping = super::lines::load_effective_mapping(conn, year);
    let limits = super::lines::load_effective_limits(conn, year);
    let federal = super::lines::compute(&statement, &mapping, &limits).lines;

    // The partners who held an interest during the year — one Schedule B row each —
    // with the TIN this machine holds, exactly as the federal return assembles them.
    let (partners, problems) =
        crate::commands::share_period_commands::partners_for_year_with_problems(conn, year);
    let filings: Vec<PartnerFiling> = partners
        .into_iter()
        .map(|partner| PartnerFiling {
            tin: pc::get_tin(conn, &partner.partner_id),
            partner,
        })
        .collect();

    let assets = crate::commands::depreciation_commands::list_assets(conn);
    let special = special_depreciation(&assets, year);
    let fixed = pc::list_fixed_allocations(conn);
    let (illinois_taxes, addback_warnings) =
        illinois_taxes_deducted(conn, year, &statement, &mapping, &limits);
    let mut bundle = build_with_special(
        &profile,
        &filings,
        &federal,
        settings,
        year,
        &special,
        &fixed,
        illinois_taxes,
    )?;
    bundle.warnings.extend(addback_warnings);
    bundle.warnings.extend(problems);
    Ok(bundle)
}

/// IL-1065 line 16: the Illinois income and replacement tax the federal return
/// deducted.
///
/// Read from the accounts marked as Illinois tax for the year
/// ([`super::lines::load_illinois_tax_addbacks`]) and every account beneath them,
/// and only as much as reached a federal deduction: an account on no line, or
/// limited to a share of its balance, adds back what the return actually took.
fn illinois_taxes_deducted(
    conn: &rusqlite::Connection,
    year: i32,
    statement: &crate::queries::reports::IncomeStatement,
    mapping: &std::collections::BTreeMap<String, String>,
    limits: &std::collections::BTreeMap<String, u8>,
) -> (i64, Vec<String>) {
    let marked = super::lines::load_illinois_tax_addbacks(conn, year);
    if marked.is_empty() {
        return (0, Vec::new());
    }
    let parents = super::lines::load_parents(conn);
    let is_marked = |id: &str| {
        let mut cursor = Some(id.to_string());
        while let Some(current) = cursor {
            if marked.contains(&current) {
                return true;
            }
            cursor = parents.get(&current).cloned().flatten();
        }
        false
    };

    let (mut cents, mut deducted, mut not_deducted) = (0i64, Vec::new(), Vec::new());
    for line in &statement.expenses.lines {
        if line.balance == 0 || !is_marked(&line.account_id) {
            continue;
        }
        let label = format!("{} {}", line.account_number, line.account_name);
        let reaches_a_deduction = mapping
            .get(&line.account_id)
            .filter(|k| k.as_str() != super::lines::OFF_RETURN)
            .and_then(|k| super::lines::line_def(k))
            .is_some_and(|d| d.schedule != super::lines::Schedule::L);
        if reaches_a_deduction {
            let pct = i64::from(limits.get(&line.account_id).copied().unwrap_or(100));
            cents += line.balance * pct / 100;
            deducted.push(label);
        } else {
            not_deducted.push(label);
        }
    }

    let dollars = super::lines::cents_to_dollars(cents);
    let mut warnings = Vec::new();
    if dollars != 0 {
        warnings.push(format!(
            "IL-1065 line 16 adds back {} of Illinois income and replacement tax the federal \
             return deducted for {year}, from {}. The tax on this return is deductible when it \
             is paid, and added back on the return for that year.",
            format_dollars(dollars),
            deducted.join(", ")
        ));
    }
    if !not_deducted.is_empty() {
        warnings.push(format!(
            "{} {} marked as Illinois tax to add back, but reach{} no deduction on the federal \
             return, so nothing of {} is added back on IL-1065 line 16.",
            not_deducted.join(", "),
            if not_deducted.len() == 1 { "is" } else { "are" },
            if not_deducted.len() == 1 { "es" } else { "" },
            if not_deducted.len() == 1 { "its" } else { "theirs" }
        ));
    }
    (dollars, warnings)
}

fn fill_identity(
    doc: &mut Document,
    map: &FieldMap,
    profile: &BusinessProfile,
    settings: &Il1065Settings,
) -> Result<(), FormError> {
    set_text(doc, map, f::LEGAL_NAME, &profile.legal_name)?;

    let a = &profile.address;
    let street = match a.suite.as_deref().filter(|s| !s.trim().is_empty()) {
        Some(suite) => format!("{} {}", a.street, suite),
        None => a.street.clone(),
    };
    set_text(doc, map, f::MAILING_ADDRESS, &street)?;
    set_text(doc, map, f::MAILING_CITY, &a.city)?;
    set_text(doc, map, f::MAILING_STATE, &a.state)?;
    set_text(doc, map, f::MAILING_ZIP, &a.postal_code)?;

    let (fein2, fein7) = split_fein(&profile.ein);
    set_text(doc, map, f::FEIN_2, &fein2)?;
    set_text(doc, map, f::FEIN_7, &fein7)?;
    set_text(doc, map, f::NAICS, &profile.naics_code)?;

    // "Where your accounting records are kept" defaults to the business address —
    // the common case, and a box someone must otherwise retype.
    set_text(doc, map, f::RECORDS_CITY, &a.city)?;
    set_text(doc, map, f::RECORDS_STATE, &a.state)?;
    set_text(doc, map, f::RECORDS_ZIP, &a.postal_code)?;

    if settings.elects_pte_tax {
        set_check(doc, map, f::PTE_BOX, f::PTE_BOX_ON)?;
    }
    Ok(())
}

fn fill_income(
    doc: &mut Document,
    map: &FieldMap,
    figs: &Figures,
    warnings: &mut Vec<String>,
) -> Result<(), FormError> {
    // Step 2 / Step 3 / Step 4 / Step 5 — the lines computed from federal figures.
    for (field, amount) in [
        (f::L1_ORDINARY, figs.line1),
        (f::L2_RENTAL_RE, figs.line2),
        (f::L3_OTHER_RENTAL, figs.line3),
        (f::L4_PORTFOLIO, figs.line4),
        (f::L5_1231, figs.line5),
        (f::L7_TOTAL_ORDINARY, figs.line7),
        (f::L8_CHARITABLE, figs.line8),
        (f::L9_SECTION179, figs.line9),
        (f::L10_INVEST_INTEREST, figs.line10),
        (f::L12_ADD_8_11, figs.line12),
        (f::L13_UNMODIFIED_BASE, figs.line13),
        (f::L14_FROM_L13, figs.line13),
        (f::L16_ILLINOIS_TAXES, figs.line16),
        (f::L17_SPECIAL_DEPRECIATION, figs.line17),
        (f::L20_GUARANTEED, figs.line20),
        (f::L23_INCOME, figs.line23),
        (f::L30_SPECIAL_DEPRECIATION, figs.line30),
        (f::L34_TOTAL_SUBTRACT, figs.line30),
        (f::L35_BASE_INCOME, figs.line35),
    ] {
        write_money(doc, map, field, amount, warnings)?;
    }
    Ok(())
}

fn fill_tax(
    doc: &mut Document,
    map: &FieldMap,
    figs: &Figures,
    settings: &Il1065Settings,
    warnings: &mut Vec<String>,
) -> Result<(), FormError> {
    if settings.apportions_outside_illinois {
        // Multi-state: check "outside Illinois", fill Step 6 as far as the books
        // reach (business income before apportionment), and stop — the sales
        // figures, factor, apportioned income and the whole tax below it need
        // data the ledger does not have.
        set_check(doc, map, f::INSIDE_OUTSIDE, f::OUTSIDE_ON)?;
        write_money(doc, map, f::L36_NONBUSINESS, 0, warnings)?;
        write_money(doc, map, f::L37_NONUNITARY, 0, warnings)?;
        write_money(doc, map, f::L38_ADD_36_37, 0, warnings)?;
        write_money(doc, map, f::L39_BUSINESS, figs.line35, warnings)?;
        return Ok(());
    }

    // Illinois-only: carry base income through Step 7 and figure the tax.
    set_check(doc, map, f::INSIDE_OUTSIDE, f::INSIDE_ON)?;
    let (line47, line53, line54, line58) = (
        figs.line47.expect("il-only figures are complete"),
        figs.line53.expect("il-only figures are complete"),
        figs.line54.expect("il-only figures are complete"),
        figs.line58.expect("il-only figures are complete"),
    );
    write_money(doc, map, f::L47_BASE, line47, warnings)?;
    write_money(doc, map, f::L49_AFTER_NLD, line47, warnings)?; // NLD = 0
    write_money(doc, map, f::L50_FROM_L35, figs.line35, warnings)?;
    // Line 51 = line 47 ÷ line 50, six decimals, never above one. Illinois-only
    // makes 47 and 50 the same figure, so the ratio is exactly one.
    set_text(doc, map, f::L51_RATIO_WHOLE, "1")?;
    set_text(doc, map, f::L51_RATIO_FRAC, "000000")?;
    write_money(
        doc,
        map,
        f::L52_EXEMPTION,
        figs.line52.expect("il-only figures are complete"),
        warnings,
    )?;
    write_money(doc, map, f::L53_NET_INCOME, line53, warnings)?;

    write_money(doc, map, f::L54_REPLACEMENT, line54, warnings)?;
    write_money(doc, map, f::L56_BEFORE_CREDITS, line54, warnings)?;
    write_money(doc, map, f::L58_NET_REPLACEMENT, line58, warnings)?;

    if settings.elects_pte_tax {
        let line61 = figs.line61.expect("il-only figures are complete");
        write_money(doc, map, f::L60_PTE_INCOME, line47.max(0), warnings)?;
        write_money(doc, map, f::L61_PTE_TAX, line61, warnings)?;
    }
    write_money(doc, map, f::L59_TOTAL_WITHHOLDING, 0, warnings)?;
    let line62 = figs.line62.expect("il-only figures are complete");
    write_money(doc, map, f::L62_TOTAL_TAX, line62, warnings)?;
    write_money(doc, map, f::L64_TOTAL, line62, warnings)?; // line 63 penalty = 0
    // Step 10. The books hold no payment toward this return — an extension
    // payment is made after the year the ledger covers — so line 66 is zero and
    // line 71 is the whole of line 64.
    if line62 > 0 {
        write_money(doc, map, f::L66_TOTAL_PAYMENTS, 0, warnings)?;
        write_money(doc, map, f::L71_TAX_DUE, line62, warnings)?;
        warnings.push(format!(
            "IL-1065 line 71 is the whole {} of line 64, with line 66 at zero. Payments made \
             before filing (line 65b, such as an extension payment) and credits from an earlier \
             overpayment (65a) are not in the books; enter any and re-figure lines 66 to 71.",
            format_dollars(line62)
        ));
    }
    Ok(())
}

fn fill_schedule_b(
    doc: &mut Document,
    map: &FieldMap,
    profile: &BusinessProfile,
    partners: &[PartnerFiling],
    figs: &Figures,
    year: i32,
    fixed: &[crate::domain::FixedAllocation],
    warnings: &mut Vec<String>,
) -> Result<(), FormError> {
    let (fein2, fein7) = split_fein(&profile.ein);

    // Column E divides base income the way the partnership divides its income:
    // by the year's fixed or preferred shares where the agreement sets them — the
    // federal K-1s' own split — and otherwise on the profit percentages. Either
    // way the column adds back to line 35, as the instructions require.
    let shares = member_shares(figs, partners, year, fixed);
    let mut subject_total = 0i64;
    for (name, f2, f7) in [
        (f::SCHB_NAME, f::SCHB_FEIN_2, f::SCHB_FEIN_7),
        (f::SCHA_NAME, f::SCHA_FEIN_2, f::SCHA_FEIN_7),
    ] {
        set_text(doc, map, name, &profile.legal_name)?;
        set_text(doc, map, f2, &fein2)?;
        set_text(doc, map, f7, &fein7)?;
    }

    for (i, filing) in partners.iter().take(SCHEDULE_B_ROWS).enumerate() {
        let n = i + 1;
        let p = &filing.partner;
        let a = &p.address;
        set_text(doc, map, &member(n, M_NAME), &p.name)?;
        set_text(doc, map, &member_address1(n), &a.street)?;
        set_text(
            doc,
            map,
            &member(n, M_ADDR2),
            a.suite.as_deref().unwrap_or(""),
        )?;
        set_text(doc, map, &member(n, M_CITY), &a.city)?;
        set_text(doc, map, &member(n, M_STATE), &a.state)?;
        set_text(doc, map, &member(n, M_ZIP), &a.postal_code)?;
        // Column B — the one-character Illinois partner type, read from the
        // federal entity type. One that does not say plainly which code it is
        // (an LLC, which may be disregarded; an exempt organization, which is
        // trust or corporation) is left blank and named.
        match illinois_member_type(p) {
            Some(code) => set_text(doc, map, &member(n, M_COL_B_TYPE), code)?,
            None => warnings.push(format!(
                "Illinois Schedule B: {}'s entity type {:?} does not say which Illinois partner \
                 type it is, so column B is blank. Enter I, P, M, T, C, S, A or N from the \
                 instructions.",
                p.name, p.entity_type
            )),
        }
        // Column C holds nine digits with no punctuation (the box is that wide),
        // so an SSN's or EIN's hyphens are stripped.
        let tin_digits: String = filing
            .tin
            .as_deref()
            .unwrap_or("")
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        set_text(doc, map, &member(n, M_COL_C_TIN), &tin_digits)?;

        // Column D — the member is itself subject to Illinois replacement tax when
        // it is an entity (another partnership, a corporation, a trust), not an
        // individual or estate. A best-effort default; the caveat says to check it.
        let subject = !super::schedule_b1::is_individual_or_estate(p);
        if subject {
            set_check(doc, map, &member(n, M_COL_D_SUBJECT), M_COL_D_ON)?;
        }

        // Column E — the member's share of base income (line 35).
        let share = shares[i].base_income;
        write_money(doc, map, &member(n, M_COL_E_SHARE), share, warnings)?;
        if subject {
            subject_total += share;
        }

        // Column F — why no pass-through withholding is owed: "R" for an Illinois
        // resident. Read from an individual's Illinois address, which is the usual
        // case and not proof of residency; the caveat says so.
        if illinois_member_type(p) == Some("I")
            && matches!(p.residency, crate::domain::Residency::Domestic)
            && p.address.state.trim().eq_ignore_ascii_case("IL")
        {
            set_text(doc, map, &member(n, M_COL_F_EXCLUDED), "R")?;
        }

        if filing.tin.is_none() {
            warnings.push(format!(
                "Illinois Schedule B: no identifying number is held on this machine for {}, so \
                 column C is blank.",
                p.name
            ));
        }
    }

    // Section A, line 3: column E over the members subject to replacement tax.
    write_money(doc, map, f::SCHA_L3_SUBJECT_SHARE, subject_total, warnings)?;
    if subject_total != 0 {
        warnings.push(format!(
            "Illinois Schedule B, Section A line 3 is {}: the base income distributable to \
             partners subject to replacement tax. Form IL-1065 subtracts it on line 27 (or, as a \
             loss, adds it back on line 21), which this program does not do — enter it there and \
             re-add Steps 4, 5 and 7.",
            format_dollars(subject_total)
        ));
    }

    if partners.len() > SCHEDULE_B_ROWS {
        warnings.push(format!(
            "Illinois Schedule B, Section B has {} partners and the printed page has {SCHEDULE_B_ROWS} \
             rows. The first {SCHEDULE_B_ROWS} were filled; the rest need a continuation page, which \
             this program does not produce.",
            partners.len()
        ));
    }
    Ok(())
}

/// The advisories that go with every IL-1065 this program produces.
fn caveats(
    profile: &BusinessProfile,
    settings: &Il1065Settings,
    partner_count: usize,
) -> Vec<String> {
    let mut out = Vec::new();

    if profile.address.state.trim().to_ascii_uppercase() != "IL" {
        out.push(format!(
            "This is an Illinois IL-1065, but the partnership's address is in {:?}, not IL. Confirm \
             it actually has an Illinois filing obligation before filing.",
            profile.address.state
        ));
    }

    out.push(
        "IL-1065 fills only the lines the books can compute. The Illinois additions (state and \
         municipal interest, related-party expenses; line 16 comes from the accounts marked as Illinois tax) and subtractions \
         (U.S. Treasury interest, and the rest of Step 5 other than special depreciation) are left \
         blank — enter any that apply and re-add the Step 4, 5 and 7 totals."
            .to_string(),
    );

    if settings.apportions_outside_illinois {
        out.push(
            "Apportioning outside Illinois: Step 6's total sales everywhere and inside Illinois, \
             the apportionment factor, and everything from line 40 down (including the replacement \
             tax) are left blank — the books hold no sales-by-state figures. Complete Step 6 and \
             the tax by hand."
                .to_string(),
        );
    }

    if settings.elects_pte_tax {
        out.push(
            "PTE tax elected: line 60 is set to net income and line 61 to 4.95% of it, which is \
             right for an Illinois-only partnership with resident partners. Adjust for any \
             nonresident or apportioned share before filing."
                .to_string(),
        );
    }

    out.push(
        "Illinois Schedule B, Section B: column D (subject to replacement tax) is checked for \
         entity partners and clear for individuals, and column F is R for individuals with an \
         Illinois address; verify each partner's residency. Columns G–L (pass-through \
         withholding and credits) and Section A lines 1, 2 and 4 to 7 are left blank."
            .to_string(),
    );

    if partner_count == 0 {
        out.push("No partners are recorded, so Illinois Schedule B is blank.".to_string());
    }

    out
}

/// The Illinois Schedule B partner-type code for a partner's federal entity
/// type, when the entity type says plainly which it is.
pub(crate) fn illinois_member_type(p: &crate::domain::Partner) -> Option<&'static str> {
    let t = p.entity_type.trim().to_ascii_lowercase();
    if t.contains("exempt") || t.contains("llc") || t.contains("disregarded") {
        None
    } else if t.is_empty() || t.contains("individual") || t.contains("person") {
        Some("I")
    } else if t.contains("estate") {
        Some("M")
    } else if t.contains("trust") {
        Some("T")
    } else if t.contains("s corp") || t.contains("s-corp") {
        Some("S")
    } else if t.contains("corp") {
        Some("C")
    } else if t.contains("partnership") {
        Some("P")
    } else {
        None
    }
}

/// Split an EIN `NN-NNNNNNN` into its two-digit and seven-digit halves, the way
/// the form's two boxes want it. A value without the hyphen is split by position
/// rather than refused — the return is more use with the number in it.
pub(crate) fn split_fein(ein: &str) -> (String, String) {
    match ein.split_once('-') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => {
            let digits: String = ein.chars().filter(|c| c.is_ascii_digit()).collect();
            (
                digits.chars().take(2).collect(),
                digits.chars().skip(2).collect(),
            )
        }
    }
}

/// Write a whole-dollar figure, or leave the box blank and warn if it does not
/// fit — the same rule the federal return follows, for the same reason.
fn write_money(
    doc: &mut Document,
    map: &FieldMap,
    field: &str,
    dollars: i64,
    warnings: &mut Vec<String>,
) -> Result<(), FormError> {
    match set_text(doc, map, field, &format_dollars(dollars)) {
        Ok(()) => Ok(()),
        Err(FormError::ValueTooLong { max, len, .. }) => {
            warnings.push(format!(
                "{dollars} does not fit the box for {field:?} ({len} characters, limit {max}), so \
                 that line is blank. Enter it by hand."
            ));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Residency, Shares};
    use crate::tax::acroform::{get_value, on_states};
    use crate::tax::form1065::FORM_TAX_YEAR;
    use chrono::NaiveDate;

    /// Every text box this module writes to, so a form revision that renamed one
    /// fails a test rather than silently dropping a figure.
    fn all_text_fields() -> Vec<String> {
        let mut v: Vec<String> = [
            f::LEGAL_NAME,
            f::MAILING_ADDRESS,
            f::MAILING_CITY,
            f::MAILING_STATE,
            f::MAILING_ZIP,
            f::FEIN_2,
            f::FEIN_7,
            f::NAICS,
            f::RECORDS_CITY,
            f::RECORDS_STATE,
            f::RECORDS_ZIP,
            f::L1_ORDINARY,
            f::L2_RENTAL_RE,
            f::L3_OTHER_RENTAL,
            f::L4_PORTFOLIO,
            f::L5_1231,
            f::L7_TOTAL_ORDINARY,
            f::L8_CHARITABLE,
            f::L9_SECTION179,
            f::L10_INVEST_INTEREST,
            f::L12_ADD_8_11,
            f::L13_UNMODIFIED_BASE,
            f::L14_FROM_L13,
            f::L17_SPECIAL_DEPRECIATION,
            f::L20_GUARANTEED,
            f::L23_INCOME,
            f::L30_SPECIAL_DEPRECIATION,
            f::L34_TOTAL_SUBTRACT,
            f::L35_BASE_INCOME,
            f::L36_NONBUSINESS,
            f::L37_NONUNITARY,
            f::L38_ADD_36_37,
            f::L39_BUSINESS,
            f::L47_BASE,
            f::L49_AFTER_NLD,
            f::L50_FROM_L35,
            f::L51_RATIO_WHOLE,
            f::L51_RATIO_FRAC,
            f::L52_EXEMPTION,
            f::L53_NET_INCOME,
            f::L54_REPLACEMENT,
            f::L56_BEFORE_CREDITS,
            f::L58_NET_REPLACEMENT,
            f::L59_TOTAL_WITHHOLDING,
            f::L60_PTE_INCOME,
            f::L61_PTE_TAX,
            f::L62_TOTAL_TAX,
            f::L64_TOTAL,
            f::L66_TOTAL_PAYMENTS,
            f::L16_ILLINOIS_TAXES,
            f::PREPARER_FIRM_NAME,
            f::L71_TAX_DUE,
            f::SCHB_NAME,
            f::SCHB_FEIN_2,
            f::SCHB_FEIN_7,
            f::SCHA_NAME,
            f::SCHA_FEIN_2,
            f::SCHA_FEIN_7,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for n in 1..=SCHEDULE_B_ROWS {
            v.push(member(n, M_NAME));
            v.push(member_address1(n));
            v.push(member(n, M_ADDR2));
            v.push(member(n, M_CITY));
            v.push(member(n, M_STATE));
            v.push(member(n, M_ZIP));
            v.push(member(n, M_COL_B_TYPE));
            v.push(member(n, M_COL_C_TIN));
            v.push(member(n, M_COL_E_SHARE));
            v.push(member(n, M_COL_F_EXCLUDED));
        }
        v
    }

    /// Every checkbox/radio this module ticks, with the on-state it ticks it to.
    fn all_check_fields() -> Vec<(String, String)> {
        let mut v = vec![
            (f::PTE_BOX.to_string(), f::PTE_BOX_ON.to_string()),
            (f::INSIDE_OUTSIDE.to_string(), f::INSIDE_ON.to_string()),
            (f::INSIDE_OUTSIDE.to_string(), f::OUTSIDE_ON.to_string()),
            (f::IL4562_ATTACHED.to_string(), f::IL4562_ON.to_string()),
        ];
        for n in 1..=SCHEDULE_B_ROWS {
            v.push((member(n, M_COL_D_SUBJECT), M_COL_D_ON.to_string()));
        }
        v
    }

    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_form() {
        let mut doc = Document::load_mem(IL1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        for name in all_text_fields() {
            assert!(
                map.find(&name).is_some(),
                "il1065.pdf has no text field {name:?}"
            );
        }
        for (name, _) in all_check_fields() {
            assert!(
                map.find(&name).is_some(),
                "il1065.pdf has no checkbox {name:?}"
            );
        }
    }

    #[test]
    fn the_checkbox_states_are_the_ones_the_form_was_built_with() {
        let mut doc = Document::load_mem(IL1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        for (name, on) in all_check_fields() {
            let states = on_states(&doc, &map, &name);
            assert!(
                states.iter().any(|s| s == &on),
                "{name:?} accepts {states:?}, not {on:?}"
            );
        }
    }

    fn profile() -> BusinessProfile {
        BusinessProfile {
            legal_name: "Prairie Partners LLC".into(),
            address: Address {
                street: "1 State St".into(),
                suite: None,
                city: "Chicago".into(),
                state: "IL".into(),
                postal_code: "60601".into(),
                country: None,
            },
            ein: "37-1234567".into(),
            naics_code: "541511".into(),
            formation_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            principal_activity: None,
            principal_product: None,
        }
    }

    fn partner(name: &str, entity_type: &str, profit: f64) -> Partner {
        Partner {
            history: Vec::new(),
            partner_id: name.to_lowercase(),
            name: name.into(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: entity_type.into(),
            address: Address {
                street: "2 Oak Ave".into(),
                suite: None,
                city: "Chicago".into(),
                state: "IL".into(),
                postal_code: "60602".into(),
                country: None,
            },
            start_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            end_date: None,
            shares: Shares::from_percents(profit, profit, profit),
        }
    }

    use crate::domain::Partner;

    /// A minimal federal figure set: ordinary income only. `l7` (other income)
    /// flows straight into total income with no offsetting deduction, so
    /// `k_line_1` (page-1 line 23) equals `dollars`.
    fn federal_ordinary(dollars: i64) -> Form1065Lines {
        let mut fed = Form1065Lines::default();
        fed.set_for_test("l7", dollars);
        fed
    }

    #[test]
    fn illinois_only_carries_base_income_to_the_replacement_tax() {
        let fed = federal_ordinary(100_000);
        let s = Il1065Settings::default();
        let figs = figures(&fed, &s);
        assert_eq!(figs.line1, 100_000);
        assert_eq!(figs.line7, 100_000);
        assert_eq!(figs.line35, 100_000);
        assert_eq!(figs.line47, Some(100_000));
        assert_eq!(figs.line52, Some(1_000), "the standard exemption");
        assert_eq!(figs.line53, Some(99_000));
        // 1.5% of 99,000 = 1,485.
        assert_eq!(figs.line54, Some(1_485));
        assert_eq!(figs.line58, Some(1_485));
        assert_eq!(figs.line61, Some(0));
        assert_eq!(figs.line62, Some(1_485));
    }

    /// The standard exemption is $1,000, is gone when unmodified base income is
    /// over $250,000, and never turns a small profit into a loss or deepens one.
    #[test]
    fn the_standard_exemption_follows_the_instructions() {
        let over = figures(&federal_ordinary(250_001), &Il1065Settings::default());
        assert_eq!(over.line52, Some(0));
        assert_eq!(over.line53, Some(250_001));

        let at = figures(&federal_ordinary(250_000), &Il1065Settings::default());
        assert_eq!(at.line52, Some(1_000));
        assert_eq!(at.line53, Some(249_000));

        let small = figures(&federal_ordinary(600), &Il1065Settings::default());
        assert_eq!(small.line53, Some(0));
        assert_eq!(small.line54, Some(0));

        let loss = figures(&federal_ordinary(-40_000), &Il1065Settings::default());
        assert_eq!(loss.line53, Some(-40_000), "a loss is not increased");
    }

    #[test]
    fn a_net_loss_owes_no_replacement_tax() {
        let fed = federal_ordinary(-40_000);
        let figs = figures(&fed, &Il1065Settings::default());
        assert_eq!(figs.line35, -40_000);
        assert_eq!(figs.line54, Some(0), "tax on a loss is zero");
    }

    #[test]
    fn electing_pte_adds_the_four_point_nine_five_percent_tax() {
        let fed = federal_ordinary(200_000);
        let s = Il1065Settings {
            apportions_outside_illinois: false,
            elects_pte_tax: true,
        };
        let figs = figures(&fed, &s);
        assert_eq!(figs.line54, Some(2_985)); // 1.5% of 199,000, after the exemption
        assert_eq!(figs.line61, Some(9_900)); // 4.95% of 200,000 of base income
        assert_eq!(figs.line62, Some(12_885));
    }

    #[test]
    fn apportioning_leaves_the_tax_for_a_person() {
        let fed = federal_ordinary(100_000);
        let s = Il1065Settings {
            apportions_outside_illinois: true,
            elects_pte_tax: false,
        };
        let figs = figures(&fed, &s);
        // Base income is still known; the tax below it is not.
        assert_eq!(figs.line35, 100_000);
        assert_eq!(figs.line47, None);
        assert_eq!(figs.line54, None);
    }

    #[test]
    fn the_illinois_only_form_shows_identity_income_and_the_replacement_tax() {
        let fed = federal_ordinary(100_000);
        let p = partner("Dana Individual", "Individual", 60.0);
        let partners = vec![PartnerFiling {
            partner: p,
            tin: Some("123-45-6789".into()),
        }];
        let bundle = build(
            &profile(),
            &partners,
            &fed,
            &Il1065Settings::default(),
            FORM_TAX_YEAR,
        )
        .unwrap();

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            get_value(&doc, &map, f::LEGAL_NAME).as_deref(),
            Some("Prairie Partners LLC")
        );
        assert_eq!(get_value(&doc, &map, f::FEIN_2).as_deref(), Some("37"));
        assert_eq!(get_value(&doc, &map, f::FEIN_7).as_deref(), Some("1234567"));
        assert_eq!(
            get_value(&doc, &map, f::L1_ORDINARY).as_deref(),
            Some("100,000")
        );
        assert_eq!(
            get_value(&doc, &map, f::L35_BASE_INCOME).as_deref(),
            Some("100,000")
        );
        assert_eq!(
            get_value(&doc, &map, f::L52_EXEMPTION).as_deref(),
            Some("1,000")
        );
        assert_eq!(get_value(&doc, &map, f::L66_TOTAL_PAYMENTS).as_deref(), Some("0"));
        assert_eq!(get_value(&doc, &map, f::L71_TAX_DUE).as_deref(), Some("1,485"));
        assert_eq!(
            get_value(&doc, &map, f::PREPARER_FIRM_NAME).as_deref(),
            Some("SELF PREPARED")
        );
        // Box A shows ticked: the widget for "inside Illinois" is drawn on, not
        // just the field's value set.
        let parent = map.find(f::INSIDE_OUTSIDE).unwrap();
        let states: Vec<Vec<u8>> = doc
            .get_dictionary(parent)
            .unwrap()
            .get(b"Kids")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|k| {
                doc.get_dictionary(k.as_reference().unwrap())
                    .unwrap()
                    .get(b"AS")
                    .unwrap()
                    .as_name()
                    .unwrap()
                    .to_vec()
            })
            .collect();
        assert!(states.contains(&b"Inside Illinois".to_vec()), "{states:?}");
        assert!(states.contains(&b"Off".to_vec()), "{states:?}");
        // An Illinois individual is excluded from withholding as a resident.
        assert_eq!(
            get_value(&doc, &map, &member(1, M_COL_F_EXCLUDED)).as_deref(),
            Some("R")
        );
        assert_eq!(
            get_value(&doc, &map, f::L54_REPLACEMENT).as_deref(),
            Some("1,485")
        );

        // Schedule B row 1: the partner and their 60% share of base income.
        assert_eq!(
            get_value(&doc, &map, &member(1, M_NAME)).as_deref(),
            Some("Dana Individual")
        );
        assert_eq!(
            get_value(&doc, &map, &member(1, M_COL_C_TIN)).as_deref(),
            Some("123456789")
        );
        assert_eq!(
            get_value(&doc, &map, &member(1, M_COL_E_SHARE)).as_deref(),
            Some("60,000")
        );
    }

    #[test]
    fn an_entity_partner_is_flagged_subject_to_replacement_tax() {
        let fed = federal_ordinary(100_000);
        let individual = PartnerFiling {
            partner: partner("Al", "Individual", 50.0),
            tin: None,
        };
        let corp = PartnerFiling {
            partner: partner("Holdings LLC", "Partnership", 50.0),
            tin: None,
        };
        let bundle = build(
            &profile(),
            &[individual, corp],
            &fed,
            &Il1065Settings::default(),
            FORM_TAX_YEAR,
        )
        .unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        // Member 1 individual: box D clear (unticked → no value). Member 2 entity:
        // box D ticked to its "/Yes" on-state.
        assert_eq!(get_value(&doc, &map, &member(1, M_COL_D_SUBJECT)), None);
        assert_eq!(
            get_value(&doc, &map, &member(2, M_COL_D_SUBJECT)).as_deref(),
            Some("/Yes")
        );
        // Column B carries each one's Illinois type code, and Section A line 3
        // totals column E over the entity alone.
        assert_eq!(get_value(&doc, &map, &member(1, M_COL_B_TYPE)).as_deref(), Some("I"));
        assert_eq!(get_value(&doc, &map, &member(2, M_COL_B_TYPE)).as_deref(), Some("P"));
        assert_eq!(get_value(&doc, &map, &member(2, M_COL_F_EXCLUDED)), None, "an entity is not R");
        assert_eq!(
            get_value(&doc, &map, f::SCHA_L3_SUBJECT_SHARE).as_deref(),
            Some("50,000")
        );
        assert!(bundle.warnings.iter().any(|w| w.contains("Section A line 3 is 50,000")));
    }

    /// Column E follows a preferred share where the year has one — the split the
    /// federal K-1s use — not the bare profit percentages, and still adds back to
    /// base income.
    #[test]
    fn column_e_follows_a_preferred_share() {
        let fed = federal_ordinary(132_464);
        let partners = vec![
            PartnerFiling {
                partner: partner("Active", "Individual", 51.0),
                tin: None,
            },
            PartnerFiling {
                partner: partner("Investor", "Individual", 49.0),
                tin: None,
            },
        ];
        let fixed = vec![crate::domain::FixedAllocation {
            tax_year: FORM_TAX_YEAR,
            partner_id: "active".into(),
            amount_cents: Some(9_000_000),
            preferred: true,
            note: "First $90,000".into(),
        }];
        let bundle = build_with_special(
            &profile(),
            &partners,
            &fed,
            &Il1065Settings::default(),
            FORM_TAX_YEAR,
            &SpecialDepreciation::default(),
            &fixed,
            0,
        )
        .unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        // 90,000 first, then 42,464 split 51/49.
        assert_eq!(
            get_value(&doc, &map, &member(1, M_COL_E_SHARE)).as_deref(),
            Some("111,657")
        );
        assert_eq!(
            get_value(&doc, &map, &member(2, M_COL_E_SHARE)).as_deref(),
            Some("20,807")
        );
        assert_eq!(get_value(&doc, &map, f::SCHA_L3_SUBJECT_SHARE).as_deref(), Some("0"));
    }

    #[test]
    fn more_than_three_partners_warns_about_a_continuation_page() {
        let fed = federal_ordinary(100_000);
        let partners: Vec<PartnerFiling> = (0..4)
            .map(|i| PartnerFiling {
                partner: partner(&format!("P{i}"), "Individual", 25.0),
                tin: None,
            })
            .collect();
        let bundle = build(
            &profile(),
            &partners,
            &fed,
            &Il1065Settings::default(),
            FORM_TAX_YEAR,
        )
        .unwrap();
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("continuation page")),
            "{:?}",
            bundle.warnings
        );
    }

    #[test]
    fn a_non_illinois_address_is_flagged() {
        let fed = federal_ordinary(100_000);
        let mut prof = profile();
        prof.address.state = "TX".into();
        let bundle = build(&prof, &[], &fed, &Il1065Settings::default(), FORM_TAX_YEAR).unwrap();
        assert!(
            bundle.warnings.iter().any(|w| w.contains("not IL")),
            "{:?}",
            bundle.warnings
        );
    }

    /// End to end from the ledger: $100,000 of ordinary income posted and mapped
    /// reaches IL line 1, carries through to the 1.5% replacement tax, and a
    /// partner's share reaches Illinois Schedule B — the whole path the filer uses,
    /// not just the arithmetic.
    #[test]
    fn a_return_built_from_the_ledger_carries_income_to_the_replacement_tax() {
        use crate::commands::partnership_commands::{self as pc, AdmitPartner};
        use crate::events::types::{Event, EventAccountType, EventEnvelope, JournalLineData};
        use crate::store::event_store::EventStore;
        use crate::store::projections::ProjectionStore;
        use crate::tax::form1065::FORM_TAX_YEAR;
        use crate::tax::lines::set_account_line;

        let mut store = EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();

        for (id, ty, number, name) in [
            ("cash", EventAccountType::Asset, "1000", "Cash"),
            ("sales", EventAccountType::Revenue, "4000", "Sales"),
        ] {
            let e = Event::AccountCreated {
                account_id: id.into(),
                account_type: ty,
                account_number: number.into(),
                name: name.into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            };
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        }

        // $100,000 of sales (credit revenue, debit cash), in cents.
        let e = Event::JournalEntryPosted {
            entry_id: "e1".into(),
            date: NaiveDate::from_ymd_opt(FORM_TAX_YEAR, 6, 1).unwrap(),
            memo: "seed".into(),
            lines: vec![
                JournalLineData {
                    line_id: "e1-0".into(),
                    account_id: "cash".into(),
                    amount: 10_000_000,
                    currency: "USD".into(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "e1-1".into(),
                    account_id: "sales".into(),
                    amount: -10_000_000,
                    currency: "USD".into(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
        store.apply_projection(&stored).unwrap();

        // Map sales to gross receipts, with no expenses — ordinary income = 100,000.
        set_account_line(store.connection(), "sales", "l1a", 0).unwrap();

        pc::set_profile(&mut store, "u", &profile()).unwrap();
        pc::admit_partner(
            &mut store,
            "u",
            &AdmitPartner {
                name: "Zak".into(),
                partner_type: PartnerType::General,
                residency: Residency::Domestic,
                entity_type: "Individual".into(),
                address: profile().address,
                start_date: None,
                shares: Shares::from_percents(100.0, 100.0, 100.0),
                tin: Some("123-45-6789".into()),
            },
        )
        .unwrap();

        let bundle = build_from_ledger(
            store.connection(),
            FORM_TAX_YEAR,
            &Il1065Settings::default(),
        )
        .unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            get_value(&doc, &map, f::L1_ORDINARY).as_deref(),
            Some("100,000")
        );
        assert_eq!(
            get_value(&doc, &map, f::L35_BASE_INCOME).as_deref(),
            Some("100,000")
        );
        assert_eq!(
            get_value(&doc, &map, f::L54_REPLACEMENT).as_deref(),
            Some("1,485")
        );
        // The sole partner's 100% share of base income lands on Schedule B.
        assert_eq!(
            get_value(&doc, &map, &member(1, M_COL_E_SHARE)).as_deref(),
            Some("100,000")
        );
    }

    /// A year Illinois publishes no carried form for is refused, not substituted.
    ///
    /// The vendored blank says on its own first page that it is for tax years
    /// ending on or after 31 December 2025 and before 31 December 2026. Filled
    /// with 2023's figures it produces a document that contradicts itself about
    /// which year it is — and Illinois renumbers between revisions exactly as the
    /// IRS does, so the figures would not even be on the lines the labels name.
    ///
    /// Only the current revision is carried: Illinois publishes prior years but
    /// not at a URL this program could fetch, and a blank nobody has read against
    /// its own boxes is one nothing should be written to.
    #[test]
    fn a_year_with_no_carried_revision_is_refused() {
        let fed = Form1065Lines::default();
        let settings = Il1065Settings::default();

        // Every carried year builds.
        for year in supported_years() {
            assert!(
                build(&profile(), &[], &fed, &settings, year).is_ok(),
                "{year} is carried and must build"
            );
        }

        for year in [2019, 2022, FORM_TAX_YEAR + 1] {
            match build(&profile(), &[], &fed, &settings, year) {
                Err(FormError::NoFormForYear {
                    form,
                    year: got,
                    available,
                }) => {
                    assert_eq!(form, "Form IL-1065", "the refusal names the right form");
                    assert_eq!(got, year);
                    assert!(
                        available.contains(&FORM_TAX_YEAR.to_string()),
                        "the refusal has to name what it does have: {available:?}"
                    );
                }
                other => panic!("{year} must be refused, got ok={}", other.is_ok()),
            }
        }
    }

    /// Every revision carried is a real form, and the current year is one of them.
    #[test]
    fn the_current_year_has_a_carried_revision() {
        assert!(!IL1065_YEARS.is_empty());
        for r in IL1065_YEARS {
            let doc = Document::load_mem(r.form).expect("the blank loads");
            assert!(
                doc.get_pages().len() >= 5,
                "{}: not the full bundle",
                r.year
            );
        }
        assert!(il1065_year(FORM_TAX_YEAR).is_some());
    }

    /// The blank says which years it is for, and that is the year carried.
    ///
    /// Read off the paper rather than trusted to the table: the whole reason this
    /// form is year-gated is the sentence printed on its first page, and a table
    /// that drifted from it would gate the wrong year.
    #[test]
    fn the_carried_blank_is_for_the_year_the_table_claims() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("accountir-il-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("il1065.pdf");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(IL1065)
            .unwrap();
        let out = std::process::Command::new("pdftotext")
            .args(["-f", "1", "-l", "1", "-layout"])
            .arg(&path)
            .arg("-")
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(out) = out else { return };
        let text = String::from_utf8_lossy(&out.stdout);

        let year = crate::tax::form1065::FORM_TAX_YEAR;
        assert!(
            text.contains(&format!("{year} Form IL-1065")),
            "the blank does not call itself the {year} form"
        );
        assert!(
            text.contains(&format!("ending on or after December 31, {year}")),
            "the blank does not say it is for tax years ending in {year}"
        );
    }

    fn furniture(placed: NaiveDate, cost: i64, class: crate::domain::PropertyClass) -> DepreciableAsset {
        DepreciableAsset {
            asset_id: format!("{class:?}"),
            description: "Tables".into(),
            asset_account_id: "1500".into(),
            expense_account_id: "6500".into(),
            accumulated_account_id: "1590".into(),
            section_179_account_id: None,
            acquired_on: placed,
            placed_in_service: placed,
            cost_cents: cost,
            class,
            system: crate::domain::System::Gds,
            section_179_cents: 0,
            bonus: BonusElection::Take,
            disposed_on: None,
            notes: None,
            overrides: Default::default(),
            basis_adjustments: Vec::new(),
        }
    }

    /// The bonus taken is added back in its year, a share of each year's regular
    /// depreciation comes off, and base income carries both.
    #[test]
    fn special_depreciation_adds_back_bonus_and_subtracts_the_illinois_share() {
        let assets = vec![furniture(
            NaiveDate::from_ymd_opt(2024, 7, 1).unwrap(),
            1_000_000,
            crate::domain::PropertyClass::SevenYear,
        )];
        let special = special_depreciation(&assets, 2024);
        let schedule = crate::tax::depreciation::compute_year(&assets, 2024);
        let row = &schedule.rows[0];
        assert_eq!(special.addition_dollars(), 6_000, "60% bonus added back");
        // The form multiplies the rate's whole-dollar regular depreciation by 1.5.
        let regular = crate::tax::lines::cents_to_dollars(row.macrs_cents);
        assert_eq!(special.subtraction_dollars(), (regular * 3 + 1) / 2);

        let figs = figures_with(&federal_ordinary(50_000), &Il1065Settings::default(), &special, 0);
        assert_eq!(figs.line17, 6_000);
        assert_eq!(figs.line23, 50_000 + 6_000);
        assert_eq!(figs.line35, 56_000 - figs.line30);
    }

    /// Over the property's life the subtractions come back to exactly the
    /// addition, because the last year settles whatever is left.
    #[test]
    fn over_its_life_the_subtractions_equal_the_addition() {
        let assets = vec![furniture(
            NaiveDate::from_ymd_opt(2024, 7, 1).unwrap(),
            1_000_000,
            crate::domain::PropertyClass::ThreeYear,
        )];
        let (mut added, mut subtracted) = (0i64, 0i64);
        for year in 2024..=2030 {
            for r in special_depreciation(&assets, year).rows {
                added += r.addition_cents + r.last_year_addition_cents;
                subtracted += r.subtraction_cents + r.last_year_subtraction_cents;
            }
        }
        assert_eq!(subtracted - added, 0, "net over life");
        // The addition itself is the 60% bonus; the net subtraction beyond the
        // add-backs brings the Illinois basis back to the federal one.
        let bonus = crate::tax::depreciation::compute_year(&assets, 2024).rows[0].bonus_cents;
        assert!(added >= bonus);
    }

    #[test]
    fn a_long_description_splits_at_a_word_and_keeps_the_rest() {
        assert_eq!(fit_words("Computer", 40), ("Computer".to_string(), None));
        let (head, rest) = fit_words("Leasehold improvements - 2025 April build-out of the studio", 40);
        assert_eq!(head, "Leasehold improvements - 2025 April");
        assert_eq!(rest.as_deref(), Some("build-out of the studio"));
    }

    /// Illinois tax the federal return deducted is added back on line 16 and
    /// carried into base income.
    #[test]
    fn illinois_taxes_deducted_are_added_back_on_line_16() {
        let figs = figures_with(
            &federal_ordinary(100_000),
            &Il1065Settings::default(),
            &SpecialDepreciation::default(),
            639,
        );
        assert_eq!(figs.line16, 639);
        assert_eq!(figs.line23, 100_639);
        assert_eq!(figs.line35, 100_639);
    }
}
