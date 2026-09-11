//! Building a Form 1065 return: the partnership's page one, then one Schedule
//! K-1 per partner, in a single PDF that is still a form.
//!
//! # What this fills in and what it does not
//!
//! Everything the books actually know: the partnership header, and each
//! partner's identity, dates, and shares. Not the income statement, not Schedule
//! K, not the capital accounts — those are the parts a return is *about*, and
//! putting a computed figure in one of them without the schedules that support
//! it produces a return that looks finished and is not. The fields are left
//! empty and editable, which is the honest state for a figure nobody has
//! prepared yet.
//!
//! # Why the field names are constants with a table behind them
//!
//! The IRS calls the EIN box `f1_14[0]`. Nothing about that name says so, so
//! every constant here is checked against `docs/form-1065-fields.md`, which is
//! generated from the XFA description inside the vendored PDF itself. The tests
//! at the bottom re-read the vendored file and assert the boxes still are what
//! these constants claim — the check that catches a new revision having
//! renumbered the form under us.

use super::acroform::{
    append_document, field_map, namespace_fields, set_check, set_text, strip_xfa, FieldMap,
    FormError,
};
use super::lines::{format_dollars, Form1065Lines};
use crate::commands::share_period_commands as spc;
use crate::domain::{format_ppm, BusinessProfile, Partner, PartnerType, Residency};
// Only the tests still name it directly; the checks that used to sum shares here
// now ask `share_period_commands` about a date instead.
#[cfg(test)]
use crate::domain::Shares;
use chrono::{Datelike, NaiveDate};
use lopdf::Document;

/// The blank forms, carried in the binary.
///
/// Embedded rather than fetched: this is a local-first program, and a return you
/// can only produce with a working connection to irs.gov is one you cannot
/// produce on the afternoon it is due.
const F1065: &[u8] = include_bytes!("../../assets/irs/f1065.pdf");
const F1065_SK1: &[u8] = include_bytes!("../../assets/irs/f1065sk1.pdf");

/// The tax year the *current* forms are for — the ones in `assets/irs` itself
/// rather than in a year directory.
pub const FORM_TAX_YEAR: i32 = 2025;

/// One tax year's blank forms.
pub struct FormYear {
    pub year: i32,
    pub f1065: &'static [u8],
    pub sk1: &'static [u8],
    /// A draft the IRS has published but not finalised. Filing one is not
    /// allowed, so anything built on it is a projection and has to say so.
    pub draft: bool,
    /// Schedule B's questions on this revision, or `None` where they have not
    /// been transcribed. Which questions the year asks, what it numbers them,
    /// and which box each answer goes in are all one fact about one form.
    pub schedule_b: Option<&'static [super::schedule_b::QuestionBoxes]>,
    /// Page one's boxes on this revision, or `None` where they have not been
    /// transcribed. The income block moves between revisions, so this is a
    /// complete table rather than a set of exceptions to another year's.
    pub page1: Option<&'static Page1>,
    /// Whether this revision's pages line up with the ones the field constants
    /// were written against.
    ///
    /// The 2026 draft inserts a Schedule A ahead of page 1, so every page shifts
    /// and `f5_*` — Schedule K in every prior revision — is Schedule B in it.
    /// Those names all still resolve, which is exactly the danger: the figures
    /// would land in real boxes on the wrong schedule. A revision that has not
    /// been mapped fills identity only, and says so.
    pub mapped: bool,
}

/// Every year this program can produce a return for, oldest first.
///
/// Each year gets its own blank because the IRS renumbers boxes between
/// revisions — see `assets/irs/README.md`. Carrying them all is the only way a
/// prior-year return is the prior year's *form* rather than this year's form
/// with last year's figures on it.
pub const FORM_YEARS: &[FormYear] = &[
    FormYear {
        year: 2023,
        page1: Some(&PAGE1_2023),
        schedule_b: Some(super::schedule_b::SCHEDULE_B_2023),
        f1065: include_bytes!("../../assets/irs/2023/f1065.pdf"),
        sk1: include_bytes!("../../assets/irs/2023/f1065sk1.pdf"),
        draft: false,
        // The paid-preparer block sits one label row higher than in 2025, so its
        // first box is f1_49 rather than f1_57. Confirmed by matching the field
        // rectangle against the position of the printed "preparer's name" label.
        // 2023 ends at question 31 and has neither the "reserved" 10e nor the
        // subchapter-K election, which the IRS added later.
        mapped: true,
    },
    FormYear {
        year: 2024,
        page1: Some(&PAGE1_2024),
        schedule_b: Some(super::schedule_b::SCHEDULE_B_2024),
        f1065: include_bytes!("../../assets/irs/2024/f1065.pdf"),
        sk1: include_bytes!("../../assets/irs/2024/f1065sk1.pdf"),
        draft: false,
        // Same box, same place, but this revision writes it as the second
        // widget of f1_49 rather than the first.
        mapped: true,
    },
    FormYear {
        year: FORM_TAX_YEAR,
        page1: Some(&PAGE1_2025),
        schedule_b: Some(super::schedule_b::SCHEDULE_B_2025),
        f1065: F1065,
        sk1: F1065_SK1,
        draft: false,
        mapped: true,
    },
    FormYear {
        year: 2026,
        // The draft inserts a Schedule A ahead of page 1; nothing has been
        // transcribed for it, and `mapped: false` refuses the year before this
        // is reached.
        page1: None,
        schedule_b: None,
        f1065: include_bytes!("../../assets/irs/2026/f1065.pdf"),
        sk1: include_bytes!("../../assets/irs/2026/f1065sk1.pdf"),
        draft: true,
        // Re-paginated: a new Schedule A takes page 1, so the income page is
        // page 2 (`f2_*`), Schedule K is page 6 (`f6_*`) and Analysis page 7.
        // Until each box is matched to its new number this cannot fill figures.
        mapped: false,
    },
];

/// The blanks for `year`, or `None` if no form is carried for it.
pub fn form_year(year: i32) -> Option<&'static FormYear> {
    FORM_YEARS.iter().find(|f| f.year == year)
}

/// Where [`FORM_TAX_YEAR`]'s blanks sit in [`FORM_YEARS`] — the fallback for a
/// year nothing is carried for, alongside a warning saying so.
// Only the test that pins `FORM_TAX_YEAR` to the table still names it: nothing
// falls back to "the current revision" any more, because a return built on
// another year's blank carries that year in pre-printed type.
#[cfg(test)]
const CURRENT_FORM_INDEX: usize = 2;

/// Roughly how many Schedule B boxes a prior revision renumbers, for the message
/// that has to explain why the schedule is blank. Measured, not guessed: 19 on
/// the 2023 form, 13 on 2024.
const UNMAPPED_SCHEDULE_B_BOXES: &str = "a dozen or more";

/// The years a return can be produced for, oldest first.
pub fn supported_years() -> Vec<i32> {
    FORM_YEARS.iter().map(|f| f.year).collect()
}

/// The 1065's own root subform, which keeps the name the IRS gave it.
pub const FORM_ROOT: &str = "topmostSubform[0]";

/// The namespace a bundle's nth Schedule K-1 lives under, numbered from one.
///
/// Public because it is how a caller reads a particular partner's boxes back out
/// of a finished bundle — `map.find_in(&k1_namespace(2), "f1_9[0]")` — without
/// depending on which subform the IRS currently nests that box in.
pub fn k1_namespace(n: usize) -> String {
    format!("K1_{n}")
}

// --- Form 1065, page 1 ------------------------------------------------------
// Descriptions are from docs/form-1065-fields.md.
/// Page one's boxes on one revision of the form.
///
/// # Why this is a table per revision rather than a set of constants
///
/// The income and deduction block moves. The 2025 form numbers gross receipts
/// `f1_19[0]`; the 2023 and 2024 forms number it `f1_15[0]`, because their
/// header uses four fewer boxes. Every one of the twenty-seven lines below is
/// displaced by the same four — and on the 2023 form by five from line 21 down,
/// because its energy-efficient-buildings line is a second widget of `f1_37`
/// rather than a new number.
///
/// Every one of those names exists on every revision, so nothing that checks
/// names could see it. What it produced was a 2023 return with gross receipts
/// printed on line 3, total deductions on "Other taxes", and the ordinary
/// business income on **line 28, Total balance due** — a page that totals to
/// nothing a reader could follow, on boxes that are all real.
pub struct Page1 {
    pub legal_name: &'static str,
    pub street: &'static str,
    /// The suite or room line. `None` on revisions that print no separate box
    /// for it — which is also the revisions that combine the address below.
    pub suite: Option<&'static str>,
    /// City. On a revision that prints one combined "City or town, state or
    /// province, country, and ZIP or foreign postal code" box, this is that box
    /// and the three below are `None`. The caller asks whether `state` is
    /// present rather than carrying a separate flag, so the two cannot disagree.
    pub city: &'static str,
    pub state: Option<&'static str>,
    pub country: Option<&'static str>,
    pub postal_code: Option<&'static str>,
    pub principal_activity: &'static str,
    pub principal_product: &'static str,
    pub naics: &'static str,
    pub ein: &'static str,
    pub date_started: &'static str,
    pub k1_count: &'static str,
    pub preparer_name: &'static str,
    pub lines: Page1Lines,
}

/// The income and deduction lines, 1a through 23.
pub struct Page1Lines {
    pub l1a_gross_receipts: &'static str,
    pub l1b_returns: &'static str,
    pub l1c_balance: &'static str,
    pub l2_cogs: &'static str,
    pub l3_gross_profit: &'static str,
    pub l4_other_partnerships: &'static str,
    pub l5_farm: &'static str,
    pub l6_form_4797: &'static str,
    pub l7_other_income: &'static str,
    pub l8_total_income: &'static str,
    pub l9_salaries: &'static str,
    pub l10_guaranteed: &'static str,
    pub l11_repairs: &'static str,
    pub l12_bad_debts: &'static str,
    pub l13_rent: &'static str,
    pub l14_taxes: &'static str,
    pub l15_interest: &'static str,
    pub l16a_depreciation: &'static str,
    pub l16b_depreciation_elsewhere: &'static str,
    pub l16c_depreciation_net: &'static str,
    pub l17_depletion: &'static str,
    pub l18_retirement: &'static str,
    pub l19_benefits: &'static str,
    pub l20_energy: &'static str,
    pub l21_other_deductions: &'static str,
    pub l22_total_deductions: &'static str,
    pub l23_ordinary_income: &'static str,
}

/// The 2023 revision. Header in the left column, one combined address line, and
/// the income block starting at `f1_15[0]`.
pub const PAGE1_2023: Page1 = Page1 {
    legal_name: "f1_04[0]",
    street: "f1_05[0]",
    suite: None,
    city: "f1_06[0]",
    state: None,
    country: None,
    postal_code: None,
    principal_activity: "f1_07[0]",
    principal_product: "f1_08[0]",
    naics: "f1_09[0]",
    ein: "f1_10[0]",
    date_started: "f1_11[0]",
    k1_count: "f1_14[0]",
    preparer_name: "f1_49[0]",
    lines: Page1Lines {
        l1a_gross_receipts: "f1_15[0]",
        l1b_returns: "f1_16[0]",
        l1c_balance: "f1_17[0]",
        l2_cogs: "f1_18[0]",
        l3_gross_profit: "f1_19[0]",
        l4_other_partnerships: "f1_20[0]",
        l5_farm: "f1_21[0]",
        l6_form_4797: "f1_22[0]",
        l7_other_income: "f1_23[0]",
        l8_total_income: "f1_24[0]",
        l9_salaries: "f1_25[0]",
        l10_guaranteed: "f1_26[0]",
        l11_repairs: "f1_27[0]",
        l12_bad_debts: "f1_28[0]",
        l13_rent: "f1_29[0]",
        l14_taxes: "f1_30[0]",
        l15_interest: "f1_31[0]",
        l16a_depreciation: "f1_32[0]",
        l16b_depreciation_elsewhere: "f1_33[0]",
        l16c_depreciation_net: "f1_34[0]",
        l17_depletion: "f1_35[0]",
        l18_retirement: "f1_36[0]",
        l19_benefits: "f1_37[0]",
        // A second widget of `f1_37`, not a number of its own — which is why the
        // three lines below it are displaced by five rather than four.
        l20_energy: "f1_37[1]",
        l21_other_deductions: "f1_38[0]",
        l22_total_deductions: "f1_39[0]",
        l23_ordinary_income: "f1_40[0]",
    },
};

/// The 2024 revision. Identical to 2023 down to line 19; from line 20 the energy
/// line gets a number of its own and everything below shifts by one.
pub const PAGE1_2024: Page1 = Page1 {
    legal_name: "f1_4[0]",
    street: "f1_5[0]",
    suite: None,
    city: "f1_6[0]",
    state: None,
    country: None,
    postal_code: None,
    principal_activity: "f1_7[0]",
    principal_product: "f1_8[0]",
    naics: "f1_9[0]",
    ein: "f1_10[0]",
    date_started: "f1_11[0]",
    k1_count: "f1_14[0]",
    preparer_name: "f1_49[1]",
    lines: Page1Lines {
        l1a_gross_receipts: "f1_15[0]",
        l1b_returns: "f1_16[0]",
        l1c_balance: "f1_17[0]",
        l2_cogs: "f1_18[0]",
        l3_gross_profit: "f1_19[0]",
        l4_other_partnerships: "f1_20[0]",
        l5_farm: "f1_21[0]",
        l6_form_4797: "f1_22[0]",
        l7_other_income: "f1_23[0]",
        l8_total_income: "f1_24[0]",
        l9_salaries: "f1_25[0]",
        l10_guaranteed: "f1_26[0]",
        l11_repairs: "f1_27[0]",
        l12_bad_debts: "f1_28[0]",
        l13_rent: "f1_29[0]",
        l14_taxes: "f1_30[0]",
        l15_interest: "f1_31[0]",
        l16a_depreciation: "f1_32[0]",
        l16b_depreciation_elsewhere: "f1_33[0]",
        l16c_depreciation_net: "f1_34[0]",
        l17_depletion: "f1_35[0]",
        l18_retirement: "f1_36[0]",
        l19_benefits: "f1_37[0]",
        l20_energy: "f1_38[0]",
        l21_other_deductions: "f1_39[0]",
        l22_total_deductions: "f1_40[0]",
        l23_ordinary_income: "f1_41[0]",
    },
};

/// The 2025 revision. The header splits the address into four boxes and takes
/// four more numbers than 2023's, which is what displaces everything below it.
pub const PAGE1_2025: Page1 = Page1 {
    legal_name: "f1_04[0]",
    street: "f1_05[0]",
    suite: Some("f1_06[0]"),
    city: "f1_07[0]",
    state: Some("f1_08[0]"),
    country: Some("f1_09[0]"),
    postal_code: Some("f1_10[0]"),
    principal_activity: "f1_11[0]",
    principal_product: "f1_12[0]",
    naics: "f1_13[0]",
    ein: "f1_14[0]",
    date_started: "f1_15[0]",
    k1_count: "f1_18[0]",
    preparer_name: "f1_57[0]",
    lines: Page1Lines {
        l1a_gross_receipts: "f1_19[0]",
        l1b_returns: "f1_20[0]",
        l1c_balance: "f1_21[0]",
        l2_cogs: "f1_22[0]",
        l3_gross_profit: "f1_23[0]",
        l4_other_partnerships: "f1_24[0]",
        l5_farm: "f1_25[0]",
        l6_form_4797: "f1_26[0]",
        l7_other_income: "f1_27[0]",
        l8_total_income: "f1_28[0]",
        l9_salaries: "f1_29[0]",
        l10_guaranteed: "f1_30[0]",
        l11_repairs: "f1_31[0]",
        l12_bad_debts: "f1_32[0]",
        l13_rent: "f1_33[0]",
        l14_taxes: "f1_34[0]",
        l15_interest: "f1_35[0]",
        l16a_depreciation: "f1_36[0]",
        l16b_depreciation_elsewhere: "f1_37[0]",
        l16c_depreciation_net: "f1_38[0]",
        l17_depletion: "f1_39[0]",
        l18_retirement: "f1_40[0]",
        l19_benefits: "f1_41[0]",
        l20_energy: "f1_42[0]",
        l21_other_deductions: "f1_43[0]",
        l22_total_deductions: "f1_44[0]",
        l23_ordinary_income: "f1_45[0]",
    },
};

/// What goes in the paid preparer's name box.
///
/// A return prepared by the partnership itself has no paid preparer, and the box
/// is not left blank: the IRS convention is to say so in words, and a blank box
/// on a return that somebody clearly prepared reads as an omission rather than as
/// an answer. Nothing about this program can produce a *paid* preparer — there is
/// no PTIN to enter and no firm to name — so the phrase is written unconditionally
/// rather than offered as a setting nobody could correctly turn off.
pub const SELF_PREPARED: &str = "SELF PREPARED";

// --- Schedule K, page 5 -----------------------------------------------------
//
// Only the *derived* boxes are named here. Every mapped Schedule K line carries
// its own field in `lines::MAPPABLE_LINES`, so there is one table rather than
// two lists that can drift apart.
mod sched_k {
    /// "1. Ordinary business income (loss) (page 1, line 23)."
    pub const L1_ORDINARY: &str = "f5_01[0]";
    /// "3c. Other net rental income (loss). Subtract line 3b from line 3a."
    pub const L3C_NET_RENTAL: &str = "f5_05[0]";
    /// "4c. Total. Add lines 4a and 4b."
    pub const L4C_TOTAL_GUARANTEED: &str = "f5_08[0]";
    /// "Analysis of Net Income (Loss) per Return, line 1."
    pub const ANALYSIS: &str = "f6_01[0]";
}

// --- Schedule K-1 -----------------------------------------------------------
mod k1 {
    /// "Final K-1."
    pub const FINAL: &str = "c1_1[0]";
    /// "A. Partnership's employer identification number."
    pub const PARTNERSHIP_EIN: &str = "f1_6[0]";
    /// "B. Partnership's name, address, city, state, and Z I P code."
    pub const PARTNERSHIP_ADDRESS: &str = "f1_7[0]";
    /// "E. Partner's S S N or T I N."
    pub const PARTNER_TIN: &str = "f1_9[0]";
    /// "F. Name, address, city, state, and Z I P code for partner entered in E."
    pub const PARTNER_ADDRESS: &str = "f1_10[0]";
    /// "G. General partner or L L C member-manager."
    pub const TYPE_GENERAL: &str = "c1_4[0]";
    /// "G. Limited partner or other L L C member."
    pub const TYPE_LIMITED: &str = "c1_4[1]";
    /// "H1. Domestic partner."
    pub const DOMESTIC: &str = "c1_5[0]";
    /// "H1. Foreign partner."
    pub const FOREIGN: &str = "c1_5[1]";
    /// "I1. What type of entity is this partner?"
    pub const ENTITY_TYPE: &str = "f1_13[0]";
    /// "J. ... Row: Profit. Column: Beginning. %."
    pub const PROFIT_BEGIN: &str = "f1_14[0]";
    pub const PROFIT_END: &str = "f1_15[0]";
    pub const LOSS_BEGIN: &str = "f1_16[0]";
    pub const LOSS_END: &str = "f1_17[0]";
    pub const CAPITAL_BEGIN: &str = "f1_18[0]";
    pub const CAPITAL_END: &str = "f1_19[0]";

    // --- Item L, "Partner's Capital Account Analysis" ---
    //
    // Matched by rectangle against the printed labels, not by the names in
    // `docs/form-1065-fields.md`. A name that resolves proves nothing about what
    // the box means — that is how the 2023 header ended up written into the wrong
    // boxes — and item L is six boxes in one column where being one row out is an
    // arithmetic error nobody can see, because the column still adds up.
    //
    // The evidence, identical on all four revisions carried in `assets/irs`
    // (2023, 2024, 2025, and the 2026 draft, where the K-1 is page 2 because a
    // Schedule A was inserted ahead of it): one column of six boxes at x=194.4,
    // width 108, on rows y=156, 144, 132, 120, 108, 96, each with its label
    // ending by x=170 and a printed "$" at x=189.5 immediately to its left.
    //
    //   y=156  "Beginning capital account"                      f1_26
    //   y=144  "Capital contributed during the year"            f1_27
    //   y=132  "Current year net income (loss)"                 f1_28
    //   y=120  "Other increase (decrease) (attach explanation)" f1_29
    //   y=108  "Withdrawals and distributions"                  f1_30
    //   y= 96  "Ending capital account"                         f1_31
    //
    // So no year needs an alias for item L. The 2026 draft rewords row 3 to
    // "Current-year net income (loss)" and moves nothing.
    pub const L_BEGIN: &str = "f1_26[0]";
    pub const L_CONTRIBUTED: &str = "f1_27[0]";
    pub const L_NET_INCOME: &str = "f1_28[0]";
    pub const L_OTHER: &str = "f1_29[0]";
    /// Row 5's box starts at x=197.7 rather than 194.4: the "$" beside it is
    /// followed by the opening parenthesis the form prints around a withdrawal,
    /// which is why the figure written here is a magnitude and not a negative.
    pub const L_WITHDRAWN: &str = "f1_30[0]";
    pub const L_ENDING: &str = "f1_31[0]";

    // --- Boxes this program deliberately leaves blank ---
    //
    // Named here rather than left unmentioned, because "there is no constant for
    // it" and "we decided not to fill it" look identical from outside, and the
    // second is the one that has been reviewed.
    //
    // The header's tax-year boxes (`ForCalendarYear[0].f1_1` through `f1_5`) sit
    // under "For calendar year 2025, or tax year beginning ... ending ...". They
    // are the *fiscal year* boxes: a calendar-year filer leaves them blank and
    // the pre-printed year on the form is their year. Every return this program
    // builds runs January to December on that year's own blank, so filling them
    // would turn a calendar-year return into a fiscal-year one. Page 1's
    // equivalent boxes are left blank for the same reason.
    //
    // Item K (`f1_20`-`f1_25`) is the partner's share of partnership liabilities,
    // split three ways — nonrecourse, qualified nonrecourse financing, recourse.
    // The books hold the liabilities but not that classification: which of the
    // three a loan is depends on who bears the economic risk of loss under
    // §752, which lives in the loan documents and the partnership agreement. A
    // total split on the profit percentage would land in real boxes, foot against
    // Schedule L, and be an assertion about guarantees nobody made.
    //
    // Item N (`f1_32`, `f1_33`) is net unrecognized §704(c) gain or loss, which
    // needs each contributed asset's basis *and* its fair market value on the day
    // it was contributed. A general ledger records the first and never the
    // second.
    //
    // Item J's "decrease due to sale or exchange" pair (`c1_8[0]`, `c1_8[1]`) is
    // a reason, not a fact: the percentages falling is visible, but a fall caused
    // by a sale, by a redemption, and by another partner being admitted look
    // identical in the ledger, and the box asks which.

    /// The appearance state these forms use for a ticked box. Not `Yes`, which
    /// is what most PDFs use and what guessing would produce.
    pub const ON: &str = "1";
    /// The second box of a pair — "limited", "foreign" — has its own state.
    pub const ON_SECOND: &str = "2";

    /// Part III: this partner's share of each Schedule K line.
    ///
    /// Pairs a Schedule K line key with the K-1 box that carries that partner's
    /// share of it. Derived Schedule K lines appear here too — a partner's share
    /// of line 1 is on their K-1 even though nothing is mapped to line 1.
    ///
    /// The lines the IRS reports by *code* (11, 13, 14, 17 through 20) are not in
    /// this table; see `CODED_BOXES`.
    pub const PART_III: &[(&str, &str)] = &[
        ("k1", "f1_34[0]"),
        ("k2", "f1_35[0]"),
        ("k3c", "f1_36[0]"),
        ("k4a", "f1_37[0]"),
        ("k4b", "f1_38[0]"),
        ("k4c", "f1_39[0]"),
        ("k5", "f1_40[0]"),
        ("k6a", "f1_41[0]"),
        ("k6b", "f1_42[0]"),
        ("k6c", "f1_43[0]"),
        ("k7", "f1_44[0]"),
        ("k8", "f1_45[0]"),
        ("k9a", "f1_46[0]"),
        ("k9b", "f1_47[0]"),
        ("k9c", "f1_48[0]"),
        ("k10", "f1_49[0]"),
        ("k12", "f1_54[0]"),
        ("k21", "f1_66[0]"),
    ];

    /// Lines the K-1 reports as a code plus an amount.
    ///
    /// `code` is the letter the IRS assigns, or `None` where the letter depends
    /// on facts this program does not have — which of the charitable-contribution
    /// limits applies, what kind of "other" item it is. A guessed code on a
    /// signed return is worse than a blank one: blank is visibly unfinished,
    /// wrong is not. Every `None` produces a warning naming the box, so nobody
    /// has to notice the gap themselves.
    pub const CODED_BOXES: &[CodedBox] = &[
        CodedBox {
            line_key: "k11",
            number: "11",
            code: None,
            code_field: "f1_50[0]",
            amount_field: "f1_51[0]",
        },
        CodedBox {
            line_key: "k13a",
            number: "13a",
            code: None,
            code_field: "Line13[0]",
            amount_field: "f1_55[0]",
        },
        CodedBox {
            line_key: "k13b",
            number: "13b",
            code: None,
            code_field: "f1_56[0]",
            amount_field: "f1_57[0]",
        },
        CodedBox {
            line_key: "k13c",
            number: "13c",
            code: None,
            code_field: "f1_58[0]",
            amount_field: "f1_59[0]",
        },
        CodedBox {
            line_key: "k14a",
            number: "14a",
            code: Some("A"),
            code_field: "Line14[0]",
            amount_field: "f1_60[0]",
        },
        CodedBox {
            line_key: "k14b",
            number: "14b",
            code: Some("B"),
            code_field: "f1_61[0]",
            amount_field: "f1_62[0]",
        },
        CodedBox {
            line_key: "k18a",
            number: "18a",
            code: Some("A"),
            code_field: "Line18[0]",
            amount_field: "f1_84[0]",
        },
        CodedBox {
            line_key: "k18b",
            number: "18b",
            code: Some("B"),
            code_field: "f1_85[0]",
            amount_field: "f1_86[0]",
        },
        CodedBox {
            line_key: "k18c",
            number: "18c",
            code: Some("C"),
            code_field: "f1_87[0]",
            amount_field: "f1_88[0]",
        },
        CodedBox {
            line_key: "k19a",
            number: "19a",
            code: Some("A"),
            code_field: "Line19[0]",
            amount_field: "f1_89[0]",
        },
        CodedBox {
            line_key: "k19b",
            number: "19b",
            code: None,
            code_field: "f1_90[0]",
            amount_field: "f1_91[0]",
        },
        CodedBox {
            line_key: "k20a",
            number: "20a",
            code: Some("A"),
            code_field: "Line20[0]",
            amount_field: "f1_92[0]",
        },
        CodedBox {
            line_key: "k20b",
            number: "20b",
            code: Some("B"),
            code_field: "f1_93[0]",
            amount_field: "f1_94[0]",
        },
    ];

    pub struct CodedBox {
        pub line_key: &'static str,
        pub number: &'static str,
        pub code: Option<&'static str>,
        pub code_field: &'static str,
        pub amount_field: &'static str,
    }
}

/// A partner and the TIN this machine holds for them, if any.
///
/// Separate from [`Partner`] because the TIN is not part of the partner record —
/// it never enters the event log. See [`crate::commands::partnership_commands`].
#[derive(Debug, Clone)]
pub struct PartnerFiling {
    pub partner: Partner,
    pub tin: Option<String>,
}

/// What to build a return from.
#[derive(Debug, Clone)]
pub struct ReturnRequest {
    pub year: i32,
    pub profile: BusinessProfile,
    pub partners: Vec<PartnerFiling>,
    /// What to do about the schedules a small partnership may skip.
    pub options: ReturnOptions,
    /// Schedule B, as answered for `year`. Defaulted rather than optional: an
    /// unanswered schedule and an absent one produce the same blank boxes, and
    /// the warnings say which questions were left.
    pub schedule_b: super::schedule_b::ScheduleB,
    /// Which accounts made up each line, from [`super::lines::compute`].
    ///
    /// Only used to build the "attach statement" pages, so a caller with no
    /// ledger leaves it empty and gets a return with no statements — which is
    /// correct, because it also has no figures to support.
    pub detail: std::collections::BTreeMap<&'static str, Vec<super::lines::LineDetail>>,
    /// Net income per the books for `year`, in cents.
    ///
    /// Schedule M-1 line 1, and nothing else uses it. In cents because it comes
    /// straight off the income statement and is rounded once, here, the same way
    /// every other figure on the return is.
    pub book_income_cents: i64,
    /// Schedule L, when the books were read for it.
    ///
    /// Optional, unlike Schedule B, because a balance sheet needs two dates of
    /// ledger history and [`build_return`] has no ledger to ask. `None` means
    /// "nobody computed one", which leaves the page blank and editable; a
    /// present-but-empty one means "computed, and nothing was mapped", which is
    /// worth a warning.
    pub schedule_l: Option<super::schedule_l::ScheduleL>,
    /// Each partner's Schedule K-1 item L, when the books were read for it.
    ///
    /// Default-empty rather than optional, unlike [`schedule_l`]: an item L that
    /// nobody computed and an item L over a partnership with no equity accounts
    /// linked both leave the same six blank boxes, and [`capital::Capital`] says
    /// which in its own warnings rather than making the absence of a value mean
    /// it. Filled by [`build_return_from_ledger`] when the caller leaves it
    /// empty, the way Schedule L and the asset register are.
    ///
    /// [`schedule_l`]: ReturnRequest::schedule_l
    /// [`capital::Capital`]: super::capital::Capital
    pub capital: super::capital::Capital,
    /// The year's figures cut at each date a partner's percentages changed.
    ///
    /// Empty is the ordinary case — either nothing changed, or the caller has no
    /// ledger to close the books against. When it is empty and the year *did*
    /// contain a change, `split_across_partners` prorates the annual figures by
    /// days instead, and says which method it used. Filled by
    /// [`build_return_from_ledger`], the only entry point with books to read.
    pub segments: Vec<super::varying::Segment>,
    /// Family ties between the partners, for Schedule B-1's §267(c) constructive
    /// ownership test. Empty is the ordinary case — no relationships recorded, so
    /// every partner is attributed only their own direct share. A spouse pair here
    /// is what puts two partners under 50% each onto the schedule at their combined
    /// percentage. See [`super::constructive`].
    pub relationships: Vec<crate::domain::PartnerRelationship>,
    /// Each partner's share of Schedule K line 18c, itemised account by account.
    ///
    /// The statement that goes behind their own K-1, and the explanation item L
    /// row 4 asks for. Default-empty rather than optional, like [`capital`]: a
    /// return with no nondeductible expenses and one whose caller had no ledger
    /// to split them with both produce no statement, and the difference is
    /// already visible in whether line 18c carries a figure at all. Filled by
    /// [`build_return_from_ledger`], the only entry point with books to read.
    ///
    /// [`capital`]: ReturnRequest::capital
    pub nondeductible: Vec<super::nondeductible::PartnerStatement>,
    /// The depreciable asset register, for Form 4562 and for checking the
    /// ledger's depreciation against what the assets actually earn.
    ///
    /// Empty is a real state and not a missing input: a partnership can post
    /// depreciation by hand and keep no register, in which case no 4562 is
    /// produced and nothing is reconciled. [`build_return_from_ledger`] reads it
    /// from the books when the caller leaves it empty, the same way it reads
    /// Schedule L.
    pub assets: Vec<crate::domain::DepreciableAsset>,
}

/// Choices about the return that are not facts about the partnership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReturnOptions {
    /// Complete Schedules L, M-1 and M-2 even when Schedule B question 4 excuses
    /// them.
    ///
    /// On by default, and that is the considered position rather than a
    /// convenience. Question 4 excuses the *filing*; it does not make the
    /// arithmetic less true. M-1 is the only check the return has that the book
    /// profit and the taxable figure differ by an amount somebody can name, and
    /// M-2 is the only check that year-end capital is opening capital plus income
    /// less draws. A return that fails either is wrong in a way page one cannot
    /// show, because page one foots regardless.
    ///
    /// Turning it off leaves all three blank, which is what the exemption
    /// permits — but it should be a decision somebody made, not the default.
    pub complete_optional_schedules: bool,
}

impl Default for ReturnOptions {
    fn default() -> Self {
        Self {
            complete_optional_schedules: true,
        }
    }
}

/// A built return, and everything about it somebody should see before filing.
pub struct Bundle {
    pub pdf: Vec<u8>,
    /// Things that are wrong or missing but not worth refusing over — shares
    /// that do not total 100%, a partner with no TIN on this machine. Surfaced
    /// rather than swallowed, because each one is a rejected return later.
    pub warnings: Vec<String>,
    pub page_count: usize,
}

/// Build the 1065 and one K-1 per partner into a single fillable PDF, filling
/// identity only.
///
/// The income and deduction lines are left blank. Use
/// [`build_return_from_ledger`] to fill them from the books.
pub fn build_return(req: &ReturnRequest) -> Result<Bundle, FormError> {
    build_return_inner(req, &Form1065Lines::default(), Vec::new())
}

/// Build the return with page one's income and deduction lines totalled from the
/// ledger.
///
/// Separate from [`build_return`] rather than a flag, because "no figures yet"
/// and "figures that came to zero" are different returns and a caller has to say
/// which it means.
pub fn build_return_from_ledger(
    conn: &rusqlite::Connection,
    req: &ReturnRequest,
) -> Result<Bundle, FormError> {
    let (year_start, year_end) = (
        NaiveDate::from_ymd_opt(req.year, 1, 1).expect("January 1 exists in every year"),
        NaiveDate::from_ymd_opt(req.year, 12, 31).expect("December 31 exists in every year"),
    );
    let statement = crate::queries::reports::Reports::new(conn)
        .income_statement(year_start, year_end)
        .map_err(|e| FormError::Malformed(format!("income statement: {e}")))?;
    let mapping = super::lines::load_effective_mapping(conn, req.year);
    let limits = super::lines::load_effective_limits(conn, req.year);
    let computed = super::lines::compute(&statement, &mapping, &limits);

    // Schedule L comes from the ledger too, and this is the only entry point
    // that has one. Computed here rather than demanded from the caller: every
    // caller with a connection would write the same three lines, and the one
    // that forgot would ship a return with a blank balance sheet and no warning.
    // An explicitly-supplied schedule wins, so a caller can still override it.
    let mut owned = req.clone();
    if owned.schedule_l.is_none() {
        owned.schedule_l = super::schedule_l::compute(conn, req.year, &mapping).ok();
    }
    // The asset register, read here for the reason Schedule L is: this is the
    // only entry point with a connection, and a caller that forgot would ship a
    // return with no Form 4562 and nothing checking line 16a against the assets
    // that justify it.
    if owned.assets.is_empty() {
        owned.assets = crate::commands::depreciation_commands::list_assets(conn);
    }
    // Item L, read here for the reason Schedule L is: this is the only entry
    // point with a ledger, and a caller that forgot would ship K-1s whose capital
    // accounts are blank with nothing saying why.
    //
    // Split from the Analysis of Net Income and not from page one's line 23:
    // item L's "current year net income (loss)" is the partner's share of the
    // *whole* of Schedule K, so a partnership with capital gains or charitable
    // contributions would otherwise close the year on a capital account short by
    // exactly those.
    if owned.capital.is_empty() {
        owned.capital = super::capital::for_return(
            conn,
            req.year,
            &req.partners,
            computed.lines.k_analysis(),
            // Item L row 4. Positive here — it is Schedule K line 18c as the form
            // prints it — and `capital` negates it, because row 4 is a decrease.
            computed.lines.get(super::lines::NONDEDUCTIBLE_LINE),
        );
    }
    // The itemisation behind row 4 and behind box 18 code C. Read here for the
    // reason item L is: splitting the components needs the same ledger the
    // percentages come from, and a caller that skipped it would ship K-1s
    // carrying a figure that reduces a partner's capital with nothing anywhere
    // saying what it was spent on.
    if owned.nondeductible.is_empty() {
        if let Some(components) = computed.detail.get(super::lines::NONDEDUCTIBLE_LINE) {
            owned.nondeductible =
                super::nondeductible::for_return(conn, req.year, &req.partners, components);
        }
    }
    // §706(d) interim closing: the year cut at each change of interest, with each
    // part's figures read from the books for those dates. Done here for the
    // reason Schedule L is — this is the only entry point with a ledger, and the
    // alternative when it is absent is proration, which is an election the filer
    // has to have actually made.
    if owned.segments.is_empty() {
        // Only the dated series comes from the books — not the whole record.
        // Replacing it would overwrite whatever the caller passed in, and a
        // projection built on hypothetical percentages would quietly be built on
        // the stored ones instead.
        let periods = crate::commands::share_period_commands::load_share_periods(conn);
        let on_return: Vec<crate::domain::Partner> = req
            .partners
            .iter()
            .map(|f| {
                let mut p = f.partner.clone();
                if p.history.is_empty() {
                    p.history = periods
                        .iter()
                        .filter(|(id, _)| *id == p.partner_id)
                        .map(|(_, period)| *period)
                        .collect();
                }
                p
            })
            .collect();
        let spans = super::varying::segments(&on_return, year_start, year_end);
        if spans.len() > 1 {
            owned.segments = spans
                .iter()
                .map(|(from, to)| {
                    let lines = crate::queries::reports::Reports::new(conn)
                        .income_statement(*from, *to)
                        .ok()
                        .map(|s| super::lines::compute(&s, &mapping, &limits).lines);
                    super::varying::Segment {
                        from: *from,
                        to: *to,
                        lines,
                    }
                })
                .collect();
        }
    }

    // The statements are built from this, and only this path knows it.
    owned.detail = computed.detail;
    // Schedule M-1 line 1. Read here rather than demanded from the caller, for
    // the same reason Schedule L is: this is the only entry point with a ledger,
    // and a caller that forgot would ship an M-1 opening at zero.
    owned.book_income_cents = statement.net_income;
    let req = &owned;

    let mut bundle = build_return_inner(req, &computed.lines, computed.warnings)?;

    // Shares carry no effective date, so a partner edited since the year ended is
    // shown here at today's split rather than that year's. See
    // `partners_changed_after` for why this is a warning and not yet a fix.
    let changed = crate::commands::partnership_commands::partners_changed_after(conn, year_end);
    if !changed.is_empty() {
        bundle.warnings.push(format!(
            "Changed after {year_end}, so item J shows their shares as they stand today, \
             not as they stood during {}: {}. Check every percentage before filing.",
            req.year,
            changed.join(", ")
        ));
    }
    Ok(bundle)
}

/// Build a return from figures supplied directly, bypassing the ledger.
///
/// Exists for previews and for eyeballing a form revision without standing up a
/// set of books. Not the filing path: [`build_return_from_ledger`] is, and it is
/// the only one that computes the figures from anything real.
#[doc(hidden)]
pub fn build_for_preview(req: &ReturnRequest, lines: &Form1065Lines) -> Result<Bundle, FormError> {
    build_return_inner(req, lines, Vec::new())
}

fn build_return_inner(
    req: &ReturnRequest,
    lines: &Form1065Lines,
    line_warnings: Vec<String>,
) -> Result<Bundle, FormError> {
    let (year_start, year_end) = (
        NaiveDate::from_ymd_opt(req.year, 1, 1).expect("January 1 exists in every year"),
        NaiveDate::from_ymd_opt(req.year, 12, 31).expect("December 31 exists in every year"),
    );

    // A K-1 goes to everyone who held an interest during the year, and to nobody
    // else. Enforced here rather than trusted to the caller: passing an
    // unfiltered partner list is the easy mistake, and it does not fail — it
    // produces a K-1 for somebody who left years ago, marked Final, with nothing
    // in either column of item J. That is a form you would have to already
    // suspect in order to notice.
    let (filed, dropped): (Vec<&PartnerFiling>, Vec<&PartnerFiling>) = req
        .partners
        .iter()
        .partition(|f| f.partner.was_partner_during(year_start, year_end));

    let mut warnings = check(req, &filed);
    warnings.extend(line_warnings);
    if lines.is_empty() {
        warnings.push(
            "No accounts are mapped to Form 1065 lines, so every income and deduction line is \
             blank. Map them and regenerate."
                .to_string(),
        );
    }
    // Said out loud. Dropping the right partners silently is how a genuinely
    // missing K-1 goes unnoticed for a year.
    if !dropped.is_empty() {
        warnings.push(format!(
            "Left off this return, having held no interest during {}: {}.",
            req.year,
            dropped
                .iter()
                .map(|f| f.partner.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // --- page one ---
    // The year's own blank, not this year's. A prior-year return on the current
    // revision is a form whose boxes have moved under the figures written into
    // them, and it foots perfectly while being wrong.
    //
    // Refused rather than substituted. This used to fall back to the current
    // revision with a warning, which produced a complete, plausible PDF whose
    // page 1 and every K-1 carried the *current* year in pre-printed type. A
    // 2022 return that says 2025 at the top is not a return with a caveat; it is
    // the wrong form, and the caveat scrolls past in a list of a dozen others.
    let blanks = form_year(req.year).ok_or_else(|| FormError::NoFormForYear {
        form: "Form 1065",
        year: req.year,
        available: supported_years()
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    })?;
    if !blanks.mapped {
        return Err(FormError::UnmappedRevision(blanks.year));
    }
    // The revision's own page-one table. A revision with none is refused above,
    // so this is an invariant of `mapped` rather than a case to handle.
    let page1 = blanks
        .page1
        .ok_or(FormError::UnmappedRevision(blanks.year))?;
    let mut doc = Document::load_mem(blanks.f1065)?;
    strip_xfa(&mut doc);
    let mut map = field_map(&doc);
    let map = map;
    warnings.extend(fill_1065(
        &mut doc,
        &map,
        page1,
        &req.profile,
        filed.len(),
        lines,
    )?);
    warnings.extend(fill_schedule_k(&mut doc, &map, lines)?);
    // The revision's own question table. A year with none has no Schedule B —
    // there is no "mapped" flag to keep in step with a list of exceptions.
    match blanks.schedule_b {
        Some(table) => {
            warnings.extend(super::schedule_b::fill(
                &mut doc,
                &map,
                &req.schedule_b,
                table,
            )?);
        }
        None if !req.schedule_b.is_empty() => warnings.push(format!(
            "Schedule B is left blank: this program does not carry the {} revision's question \
             table. The answers on file are kept.",
            blanks.year
        )),
        None => {}
    }

    // Schedules L, M-1 and M-2. Question 4 excuses them; the option decides
    // whether to take the excuse, and it defaults to no — see `ReturnOptions`.
    let exempt = req.schedule_b.get("b4") == Some(super::schedule_b::YES);
    let do_optional = !exempt || req.options.complete_optional_schedules;

    if do_optional {
        match req.schedule_l.as_ref() {
            Some(sched_l) => {
                warnings.extend(super::schedule_l::fill(&mut doc, &map, sched_l, !exempt)?)
            }
            // Nobody computed one. Previously this arm was silent, so a Schedule
            // L that never ran and a Schedule L with nothing mapped produced the
            // same blank page and the same absence of explanation.
            None => warnings.push(
                "Schedule L is blank because no balance sheet was computed for this return. \
                 `build_return_from_ledger` reads one from the books; `build_return` has no ledger \
                 to read."
                    .to_string(),
            ),
        }

        // Line 18c is handed over rather than left inside M-1's residual: the
        // disallowed half of a meal is an expense the books bear and the return
        // does not deduct, which is precisely what M-1 line 4 is for, and it is
        // the one component of that residual this program can name.
        let m = super::schedule_m::reconcile(
            req.book_income_cents,
            lines,
            req.schedule_l.as_ref(),
            lines.get(super::lines::NONDEDUCTIBLE_LINE),
        );
        warnings.extend(super::schedule_m::fill(&mut doc, &map, &m, !exempt)?);
    } else {
        warnings.push(
            "Schedules L, M-1 and M-2 are blank: question 4 excuses them, and completing them \
             anyway is switched off. Nothing then checks that the books and the return agree."
                .to_string(),
        );
    }

    // Said once for the return rather than once per K-1: what a beginning capital
    // account here does and does not include is a fact about the books, not about
    // any one partner. The per-partner ones — an unsupported loss, a partner with
    // no accounts linked — come out of `fill_k1` beside the K-1 they concern.
    warnings.extend(req.capital.warnings());

    // Split Schedule K before any K-1 is built, so every partner's share comes
    // out of one apportionment and the shares add back to the totals above.
    let (shares, split_warnings) = split_across_partners(lines, &filed, req.year, &req.segments);
    warnings.extend(split_warnings);

    // --- one K-1 per partner ---
    for (i, filing) in filed.iter().enumerate() {
        let mut sched = Document::load_mem(blanks.sk1)?;
        strip_xfa(&mut sched);
        // Namespace this copy before anything is written into it, so partner
        // two's boxes are not partner one's under another name.
        namespace_fields(&mut sched, &k1_namespace(i + 1));
        let smap = field_map(&sched);
        warnings.extend(fill_k1(
            &mut sched,
            &smap,
            &req.profile,
            filing,
            &shares[i],
            req.capital.for_partner(&filing.partner.partner_id),
            year_start,
            year_end,
        )?);
        append_document(&mut doc, sched)?;

        // --- and, behind it, their nondeductible-expenses statement ---
        //
        // Out of order on purpose. Everything else this program composes is
        // appended after the IRS schedules, so the bundle reads form, K-1s,
        // official schedules, then our supporting pages. This one page names a
        // partner and supports two boxes on the schedule immediately in front of
        // it — box 18 code C, and item L row 4, which the K-1 instructions tell
        // you to attach an explanation for. Filed at the back it is a loose sheet
        // somebody has to match to a partner by reading it; filed here it is
        // attached to the K-1 it explains, which is what "attach" means.
        warnings.extend(nondeductible_statement(
            &mut doc, req, filing, &filed, &shares[i],
        )?);
    }

    // --- Schedule B-1 and B-2 ---
    //
    // Before the statements, because they are IRS schedules and the statements
    // are ours: the return reads form, K-1s, official schedules, then the
    // supporting pages we composed.
    // Attribute ownership under §267(c) from the recorded family ties, over
    // *every* partner (not only those filing this year), because a relative who
    // has left still attributes their interest for the 50% test.
    //
    // Computed before the question is consulted, because it is needed either way:
    // the schedule uses it when 2a or 2b says Yes, and the consistency check below
    // uses it precisely when they do not.
    let all_partners: Vec<crate::domain::Partner> =
        req.partners.iter().map(|f| f.partner.clone()).collect();
    let owners: Vec<super::schedule_b1::Owner> = req
        .partners
        .iter()
        .map(|f| super::schedule_b1::Owner {
            partner: &f.partner,
            tin: f.tin.as_deref(),
            constructive: super::constructive::constructive_shares(
                &f.partner,
                &all_partners,
                &req.relationships,
            ),
        })
        .collect();

    // Checked whether or not the schedule is required, because the case that
    // needs catching is the one where it is *not*: an answer of No over a partner
    // the books put at 50% or more drops the whole B-1 path silently, and a
    // return that is short a schedule looks exactly like one that never owed it.
    warnings.extend(super::schedule_b1::contradictions(&req.schedule_b, &owners));

    if super::schedule_b1::is_required(&req.schedule_b) {
        let (sched, b1_warnings) =
            super::schedule_b1::build(&req.profile.legal_name, &req.profile.ein, &owners)?;
        warnings.extend(b1_warnings);
        match sched {
            Some(sched) => {
                append_document(&mut doc, sched)?;
                warnings.push(super::schedule_b1::CONSTRUCTIVE_OWNERSHIP_CAVEAT.to_string());
            }
            // Declared on Schedule B but nobody in the books crosses 50%. The
            // two are not the same claim — the form's own instructions attribute
            // ownership from family and related entities — so this is a mismatch
            // to resolve, not a schedule to quietly omit.
            None => warnings.push(
                "Schedule B question 2a or 2b is Yes, but no partner in the books owns 50% or \
                 more, so no Schedule B-1 was produced. Either the answer is wrong or the owner \
                 holds their interest indirectly — the schedule has to be attached by hand in that \
                 case."
                    .to_string(),
            ),
        }
    }

    // --- Form 4562 ---
    //
    // Built from the register rather than from the ledger, because the ledger
    // holds only the result: a journal entry records the deduction and none of
    // the facts — cost, class, date placed in service, recovery year — the form
    // asks for. The entry and the form come from the same computation, which is
    // what makes them agree.
    let year_schedule = super::depreciation::compute_year(&req.assets, req.year);
    warnings.extend(year_schedule.warnings.iter().cloned());

    // Checked whether or not a 4562 is produced: the disagreements worth
    // catching are the ones where the register says something the ledger does
    // not, and an unposted year is exactly that.
    warnings.extend(super::depreciation::reconcile_with_ledger(
        &year_schedule,
        lines,
        req.schedule_l.as_ref(),
    ));

    let activity = req
        .profile
        .principal_activity
        .clone()
        .unwrap_or_else(|| req.profile.legal_name.clone());
    let (form_4562, f4562_warnings) =
        super::form4562::build(&req.profile, &year_schedule, &activity, req.year)?;
    warnings.extend(f4562_warnings);
    if let Some(filled) = form_4562 {
        append_document(&mut doc, filled.document)?;
    }

    if super::schedule_b2::is_required(&req.schedule_b) {
        let eligible: Vec<super::schedule_b2::Eligible> = filed
            .iter()
            .map(|f| super::schedule_b2::Eligible {
                partner: &f.partner,
                tin: f.tin.as_deref(),
            })
            .collect();
        let (sched, count, b2_warnings) =
            super::schedule_b2::build(&req.profile.legal_name, &req.profile.ein, &eligible)?;
        warnings.extend(b2_warnings);
        if let Some(sched) = sched {
            append_document(&mut doc, sched)?;
        }

        // Question 31's follow-up is the total from Schedule B-2, Part III, line
        // 3. Checked rather than assumed: a hand-typed figure that disagrees with
        // the schedule attached behind it is the kind of mismatch that invalidates
        // the election.
        match req.schedule_b.get("b31_total") {
            Some(typed) if typed != count.to_string() => warnings.push(format!(
                "Question 31 says the Schedule B-2 total is {typed}, but the schedule produced \
                 lists {count} partner(s). The two have to agree."
            )),
            _ => {}
        }
    }

    // --- "attach statement" pages ---
    //
    // After the K-1s, so the return reads front to back: the form, then each
    // partner's schedule, then the schedules that support a box on the form.
    for def in super::lines::MAPPABLE_LINES
        .iter()
        .filter(|d| d.attachment.is_some_and(|a| a.generated))
    {
        let Some(rows) = req.detail.get(def.key) else {
            continue;
        };
        let statement = super::statement::build(&super::statement::StatementRequest {
            legal_name: &req.profile.legal_name,
            ein: &req.profile.ein,
            year: req.year,
            line: def,
            rows,
            // The entity's own page: these are Schedule K's figures, not any one
            // partner's share of them.
            partner: None,
        })?;
        if let Some(statement) = statement {
            append_document(&mut doc, statement)?;
        }
    }

    // A line that needs a statement, carries a figure, and has no detail to
    // build one from — a caller that skipped the ledger. Said out loud, because
    // an unsupported "other deductions" figure is what draws a letter.
    for def in super::lines::MAPPABLE_LINES
        .iter()
        .filter(|d| d.attachment.is_some_and(|a| a.generated))
    {
        if lines.is_mapped(def.key) && req.detail.get(def.key).is_none_or(|r| r.is_empty()) {
            warnings.push(format!(
                "Line {} carries a figure and the form asks for a statement of what is in it, but \
                 no account detail was supplied, so none was produced. Attach one before filing.",
                def.number
            ));
        }
    }

    let page_count = doc.get_pages().len();
    let mut pdf = Vec::new();
    doc.save_to(&mut pdf)?;

    if filed.is_empty() {
        warnings.push("No partners, so the return has no Schedules K-1.".to_string());
    }
    Ok(Bundle {
        pdf,
        warnings,
        page_count,
    })
}

/// Everything worth saying about a return before it is filed.
fn check(req: &ReturnRequest, filed: &[&PartnerFiling]) -> Vec<String> {
    let mut out = Vec::new();

    // An absent EIN is allowed when the profile is saved, because a sole
    // proprietorship may genuinely have none — Schedule C line D is optional and
    // the return goes under the owner's own SSN. A partnership may not: the
    // return is matched to the entity by that number. The event cannot tell the
    // two apart, because it does not carry the business type; this can, because
    // by here the form being filled is a Form 1065.
    if req.profile.ein.trim().is_empty() {
        out.push(
            "The partnership has no EIN, so the box at the top of page 1 and the same box on \
             every Schedule K-1 are blank. A partnership return is matched to the entity by that \
             number — it is on the Settings page, under Business details."
                .to_string(),
        );
    }

    match form_year(req.year) {
        // Refused in `build_return_inner`, which is where the blank is chosen.
        // Nothing reaches a filed return on the wrong year's form.
        None => {}
        Some(f) if f.draft => out.push(format!(
            "The {} Form 1065 is an IRS draft, which may not be filed. This is a projection of \
             what the year is heading for, not a return — check for the final form before \
             filing anything on it.",
            f.year
        )),
        Some(_) => {}
    }

    // Shares are checked here rather than when a partner is saved: a partnership
    // passes through states where they do not total the whole, and this is the
    // point at which they have to.
    //
    // # Why this asks about days rather than summing the list
    //
    // It used to sum the current percentages across everybody on the return and
    // complain when they did not reach 100%. On a partnership whose membership
    // changed during the year that check is wrong in both directions at once: a
    // partner who left in June is still on the return and still has percentages
    // on file, so the sum counts somebody who was gone for half of it, and the
    // warning fires on books that are entirely correct — while a genuine gap in
    // one half of the year hides inside a total that happens to reach 100%.
    //
    // The split has to add up on each day it was in force. Checking the first
    // day, the last, and every day it changed covers every distinct split the
    // year contained, because between two changes nothing moves.
    let owned: Vec<Partner> = filed.iter().map(|p| p.partner.clone()).collect();
    let (year_start, year_end) = crate::commands::partnership_commands::calendar_year(req.year);
    let mut said: Vec<String> = Vec::new();
    for day in spc::days_to_check(&owned, year_start, year_end) {
        for problem in spc::problems_on(&owned, day) {
            if !said.contains(&problem) {
                said.push(problem.clone());
                out.push(format!(
                    "{problem} Every K-1 covering that date will be filed with these figures."
                ));
            }
        }
    }

    for filing in filed {
        if filing.tin.is_none() {
            out.push(format!(
                "No TIN on this machine for '{}', so item E of their K-1 is blank.",
                filing.partner.name
            ));
        }
    }
    out
}

/// City, state, country and ZIP as the older forms want them: one line.
///
/// Comma-separated the way an address is written, and empty parts dropped, so a
/// partnership with no country entered does not get a stray comma on its return.
fn one_line_address(addr: &crate::domain::Address) -> String {
    // Punctuated the way an address is written rather than as a comma-joined
    // list: "Chicago, IL 60625", not "Chicago, IL, 60625". This prints on a
    // filed form, so it should read like an address.
    let commas: Vec<&str> = [
        Some(addr.city.as_str()),
        Some(addr.state.as_str()),
        addr.country.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .collect();
    let head = commas.join(", ");
    let zip = addr.postal_code.trim();
    match (head.is_empty(), zip.is_empty()) {
        (true, _) => zip.to_string(),
        (false, true) => head,
        (false, false) => format!("{head} {zip}"),
    }
}

fn fill_1065(
    doc: &mut Document,
    map: &FieldMap,
    page1: &Page1,
    profile: &BusinessProfile,
    k1_count: usize,
    lines: &Form1065Lines,
) -> Result<Vec<String>, FormError> {
    let addr = &profile.address;
    set_text(doc, map, page1.legal_name, &profile.legal_name)?;
    set_text(doc, map, page1.street, &addr.street)?;
    // A revision with no separate state box prints one line labelled "City or
    // town, state or province, country, and ZIP or foreign postal code", and the
    // whole address goes in it. Asked of the table rather than carried as a
    // flag beside it, so the two cannot disagree about which form this is.
    match (page1.state, page1.country, page1.postal_code) {
        (Some(state), country, Some(postal)) => {
            if let Some(suite) = page1.suite {
                set_text(doc, map, suite, addr.suite.as_deref().unwrap_or(""))?;
            }
            set_text(doc, map, page1.city, &addr.city)?;
            set_text(doc, map, state, &addr.state)?;
            if let Some(country) = country {
                set_text(doc, map, country, addr.country.as_deref().unwrap_or(""))?;
            }
            set_text(doc, map, postal, &addr.postal_code)?;
        }
        _ => set_text(doc, map, page1.city, &one_line_address(addr))?,
    }
    set_text(doc, map, page1.naics, &profile.naics_code)?;
    set_text(doc, map, page1.ein, &profile.ein)?;
    set_text(
        doc,
        map,
        page1.date_started,
        &us_date(profile.formation_date),
    )?;
    set_text(doc, map, page1.k1_count, &k1_count.to_string())?;

    if let Some(a) = profile.principal_activity.as_deref() {
        set_text(doc, map, page1.principal_activity, a)?;
    }
    if let Some(p) = profile.principal_product.as_deref() {
        set_text(doc, map, page1.principal_product, p)?;
    }

    // The tax-year boxes at the top are deliberately left blank. The form reads
    // "For calendar year 2025, or tax year beginning ___", so a calendar-year
    // filer fills in nothing; writing the dates in would assert a fiscal year
    // that was never chosen.

    set_text(doc, map, page1.preparer_name, SELF_PREPARED)?;

    // The PTIN, firm name, firm EIN, firm address and phone beside it stay blank,
    // and the "check if self-employed" box stays unticked: all of them describe a
    // paid preparer, and there is not one. So does "May the IRS discuss this
    // return with the preparer shown below?" — a question about somebody who does
    // not exist here, and one whose answer is the signer's to give.

    fill_income_lines(doc, map, page1, lines)
}

/// Write page one's income and deduction lines.
///
/// A mapped line is written only when it is not zero: a box left empty says "no
/// such item", which is what a partnership with no farm income means, whereas a
/// printed 0 is a positive claim that somebody looked. The running totals are
/// the exception — line 23 is always written, because the bottom line of the
/// page is a figure a reader goes looking for and its absence reads as an
/// unfinished return rather than as a nil result.
fn fill_income_lines(
    doc: &mut Document,
    map: &FieldMap,
    page1: &Page1,
    lines: &Form1065Lines,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();
    let mapped = [
        (page1.lines.l1a_gross_receipts, lines.get("l1a")),
        (page1.lines.l1b_returns, lines.get("l1b")),
        (page1.lines.l2_cogs, lines.get("l2")),
        (page1.lines.l4_other_partnerships, lines.get("l4")),
        (page1.lines.l5_farm, lines.get("l5")),
        (page1.lines.l6_form_4797, lines.get("l6")),
        (page1.lines.l7_other_income, lines.get("l7")),
        (page1.lines.l9_salaries, lines.get("l9")),
        (page1.lines.l10_guaranteed, lines.get("l10")),
        (page1.lines.l11_repairs, lines.get("l11")),
        (page1.lines.l12_bad_debts, lines.get("l12")),
        (page1.lines.l13_rent, lines.get("l13")),
        (page1.lines.l14_taxes, lines.get("l14")),
        (page1.lines.l15_interest, lines.get("l15")),
        (page1.lines.l16a_depreciation, lines.get("l16a")),
        (page1.lines.l16b_depreciation_elsewhere, lines.get("l16b")),
        (page1.lines.l17_depletion, lines.get("l17")),
        (page1.lines.l18_retirement, lines.get("l18")),
        (page1.lines.l19_benefits, lines.get("l19")),
        (page1.lines.l20_energy, lines.get("l20")),
        (page1.lines.l21_other_deductions, lines.get("l21")),
        // Derived. Written on the same non-zero rule so a page with no COGS does
        // not carry a gross-profit line restating gross receipts.
        (page1.lines.l1c_balance, lines.line_1c()),
        (page1.lines.l3_gross_profit, lines.line_3()),
        (page1.lines.l8_total_income, lines.line_8()),
        (page1.lines.l16c_depreciation_net, lines.line_16c()),
        (page1.lines.l22_total_deductions, lines.line_22()),
    ];

    for (field, dollars) in mapped {
        if dollars != 0 {
            write_money(doc, map, field, dollars, &mut warnings)?;
        }
    }

    // Always: the figure every reader of this page is looking for, and the one
    // every Schedule K-1 is an allocation of.
    write_money(
        doc,
        map,
        page1.lines.l23_ordinary_income,
        lines.line_23(),
        &mut warnings,
    )?;
    Ok(warnings)
}

/// Write a dollar figure, or say why it could not be written.
///
/// A figure too long for its box leaves the box **empty** and adds a warning
/// naming the line and the amount. Truncating is the one unacceptable option: a
/// return showing 12,345,678 where 123,456,789 belongs is wrong and looks
/// right, where an empty box with a warning beside it is wrong and looks wrong.
/// Same principle as an unmapped account.
fn write_money(
    doc: &mut Document,
    map: &FieldMap,
    field: &str,
    dollars: i64,
    warnings: &mut Vec<String>,
) -> Result<(), FormError> {
    let text = format_dollars(dollars);
    match set_text(doc, map, field, &text) {
        Ok(()) => Ok(()),
        Err(FormError::ValueTooLong { max, len, .. }) => {
            warnings.push(format!(
                "{dollars} does not fit the box for {field} ({len} characters, limit {max}), \
                 so that line is blank. Enter it by hand."
            ));
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Write Schedule K — the partnership's totals, before they are split.
///
/// The mapped lines come straight from the catalogue; the derived ones are
/// computed here for the same reason page one's are, and line 1 in particular is
/// never mappable: it *is* page one's line 23, and a line 1 somebody could map
/// separately is a return whose two pages disagree about one number.
fn fill_schedule_k(
    doc: &mut Document,
    map: &FieldMap,
    lines: &Form1065Lines,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();

    for def in super::lines::MAPPABLE_LINES
        .iter()
        .filter(|d| d.schedule == super::lines::Schedule::K)
    {
        let super::lines::Field::One(field) = def.field else {
            continue;
        };
        if !lines.is_mapped(def.key) {
            continue;
        }
        write_money(doc, map, field, lines.get(def.key), &mut warnings)?;
    }

    // Line 1 is always written, even at zero: it is the figure every reader of a
    // K-1 reconciles against, and a blank reads as an unfinished return rather
    // than as a nil result. The other derived lines follow page one's rule and
    // are written only when they carry something.
    set_text(
        doc,
        map,
        sched_k::L1_ORDINARY,
        &super::lines::format_dollars(lines.k_line_1()),
    )?;
    write_money(
        doc,
        map,
        sched_k::L3C_NET_RENTAL,
        lines.k_line_3c(),
        &mut warnings,
    )?;
    write_money(
        doc,
        map,
        sched_k::L4C_TOTAL_GUARANTEED,
        lines.k_line_4c(),
        &mut warnings,
    )?;
    set_text(
        doc,
        map,
        sched_k::ANALYSIS,
        &super::lines::format_dollars(lines.k_analysis()),
    )?;

    // The credits (15a-15f) and AMT items (17a-17f) are left blank and editable.
    // Neither is an account balance: a credit is computed on its own form and an
    // AMT item is a recomputation of a figure already reported, so there is
    // nothing in the chart of accounts to point at them.
    if !lines.any_schedule_k() {
        warnings.push(
            "Nothing is mapped to a Schedule K line, so every separately stated item is blank. \
             Charitable contributions, section 179, investment interest and capital gains belong \
             there rather than in page 1, line 21."
                .to_string(),
        );
    }

    Ok(warnings)
}

/// Draw one partner's share of Schedule K line 18c behind their own K-1.
///
/// Nothing to draw is the ordinary case — most partnerships have no limited
/// deduction at all — and it is not worth a word: an absent statement for an
/// absent figure is correct, and [`super::statement::build`] already declines to
/// draw a page for an empty list. A partner whose *box* carries a figure and
/// whose statement is missing is a different thing entirely, and is said.
///
/// # The two checks, and why they are still here
///
/// Box 18 code C, item L row 4 and this page are all one partner's share of one
/// line, split by one allocator on one weighting — line 18c's own history over
/// the year. They agree by construction, and through
/// [`build_return_from_ledger`] neither check below can fire.
///
/// They are kept for the path that does not go through it. A caller may build a
/// [`ReturnRequest`] by hand — a projection, a what-if, a restored bundle — and
/// hand in figures that were never split together. The mismatch these catch used
/// to be reachable from the books: weighting row 4 and the statement by the whole
/// of Schedule K instead put a statement of 140 beside a box of 180 on a $200
/// line. A statement quietly contradicting the box above it is the kind of thing
/// only ever found by whoever is being audited.
fn nondeductible_statement(
    doc: &mut Document,
    req: &ReturnRequest,
    filing: &PartnerFiling,
    filed: &[&PartnerFiling],
    shares: &PartnerShares,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();
    let boxed = shares.get(super::lines::NONDEDUCTIBLE_LINE);
    let Some(mine) = req
        .nondeductible
        .iter()
        .find(|s| s.partner_id == filing.partner.partner_id)
    else {
        // Silence here used to be indistinguishable from "no nondeductible
        // expenses": a partner with a figure in box 18 code C got no page
        // explaining it and nothing saying a page was missing.
        if boxed != 0 {
            warnings.push(format!(
                "{}: box 18 code C on their Schedule K-1 carries {}, and no statement of what \
                 makes it up was produced, so their K-1 reports a figure that reduces their \
                 capital account with nothing behind it. `build_return_from_ledger` splits the \
                 statement from the books; a return built from figures alone has to have one \
                 attached by hand.",
                filing.partner.name,
                super::lines::format_dollars(boxed),
            ));
        }
        return Ok(warnings);
    };
    // The catalogue has carried this line since the deduction limits landed; the
    // `else` is unreachable and is a `return` rather than an `expect` because a
    // missing statement is not worth failing somebody's return over.
    let Some(def) = super::lines::line_def(super::lines::NONDEDUCTIBLE_LINE) else {
        return Ok(warnings);
    };

    // Two partners of the same name is not a hypothetical — a father and son, a
    // trust named after its settlor — and their two statements would otherwise be
    // one page printed twice with different figures on it, with nothing saying
    // which K-1 either belongs behind. So the heading falls back to what item E
    // of the K-1 in front of it carries, and to the books' own id when even that
    // is absent. Only when it is needed: an unambiguous name reads better than a
    // name with an identifier stapled to it, and most pages are unambiguous.
    let shares_a_name = filed
        .iter()
        .filter(|f| f.partner.name == filing.partner.name)
        .count()
        > 1;
    let heading_name = if shares_a_name {
        match filing.tin.as_deref() {
            Some(tin) => format!("{} · {}", filing.partner.name, tin),
            None => format!("{} · {}", filing.partner.name, filing.partner.partner_id),
        }
    } else {
        filing.partner.name.clone()
    };

    if let Some(page) = super::statement::build(&super::statement::StatementRequest {
        legal_name: &req.profile.legal_name,
        ein: &req.profile.ein,
        year: req.year,
        line: def,
        rows: &mine.rows,
        partner: Some(&heading_name),
    })? {
        append_document(doc, page)?;
    }

    // A negative row. Largest-remainder is not monotone, so adding a component to
    // the running total can move the dollar it rounds up from one partner to
    // another and leave this partner's share of that component at −1. It is a
    // real dollar in the right place — the column and the row both still foot —
    // and it reads on the page as an expense that came back. Named rather than
    // smoothed away, because the alternatives are worse: see
    // [`super::nondeductible`].
    for row in mine.rows.iter().filter(|r| r.cents < 0) {
        warnings.push(format!(
            "{}: on their nondeductible expenses statement, {} {} shows {}. That is rounding, \
             not a credit — their share of the line is apportioned a component at a time, and \
             where the dollar left over moves from one partner to another between two components \
             one of them comes out a dollar short on the second. The statement still totals their \
             box 18 code C and the partners still total Schedule K line 18c. Re-label the row by \
             hand if it would puzzle a reader.",
            filing.partner.name,
            row.account_number,
            row.account_name,
            super::lines::format_dollars(super::lines::cents_to_dollars(row.cents)),
        ));
    }

    if mine.total() != boxed {
        warnings.push(format!(
            "{}: the nondeductible expenses statement behind their Schedule K-1 totals {}, but \
             box 18 code C on that K-1 says {}. Both are their share of the same Schedule K line \
             18c, so the two were not split together — usually because the request was built from \
             figures rather than from the books, and the statement and the box came from different \
             readings of the year. Settle which the partnership agreement means and correct the \
             other before filing.",
            filing.partner.name,
            super::lines::format_dollars(mine.total()),
            super::lines::format_dollars(boxed),
        ));
    }

    Ok(warnings)
}

/// One partner's share of every Schedule K figure, in whole dollars.
struct PartnerShares {
    by_line: std::collections::BTreeMap<&'static str, i64>,
}

impl PartnerShares {
    fn get(&self, key: &str) -> i64 {
        self.by_line.get(key).copied().unwrap_or(0)
    }
}

/// Split every Schedule K figure across the partners.
///
/// Returns one [`PartnerShares`] per entry of `filed`, in the same order, and a
/// warning when the profit and loss percentages differ — at which point *which*
/// share an item travelled on becomes visible on the return and is worth
/// checking against the partnership agreement.
fn split_across_partners(
    lines: &Form1065Lines,
    filed: &[&PartnerFiling],
    year: i32,
    segments: &[super::varying::Segment],
) -> (Vec<PartnerShares>, Vec<String>) {
    use super::allocate::{allocate_as_of, profit_and_loss_shares_differ, Basis};

    let partners: Vec<&Partner> = filed.iter().map(|f| &f.partner).collect();
    let (year_start, year_end) = crate::commands::partnership_commands::calendar_year(year);
    let mut out: Vec<PartnerShares> = (0..filed.len())
        .map(|_| PartnerShares {
            by_line: std::collections::BTreeMap::new(),
        })
        .collect();

    // Every figure a K-1 carries, derived ones included. Line 1 is here because
    // a partner's share of ordinary business income is the single most important
    // number on their K-1, and it is derived rather than mapped.
    let mut figures: Vec<(&'static str, i64)> = vec![
        ("k1", lines.k_line_1()),
        ("k3c", lines.k_line_3c()),
        ("k4c", lines.k_line_4c()),
    ];
    for def in super::lines::MAPPABLE_LINES
        .iter()
        .filter(|d| d.schedule == super::lines::Schedule::K)
    {
        if lines.is_mapped(def.key) {
            figures.push((def.key, lines.get(def.key)));
        }
    }

    // §706(d): a year whose percentages moved cannot honestly be split on any one
    // day's figures. `segments` divides it at each change and turns the parts
    // into one effective percentage per partner per line, which the ordinary
    // allocator then applies to the year's own total — so the shares still foot
    // to Schedule K exactly while describing who held what, when.
    let owned: Vec<Partner> = partners.iter().map(|p| (*p).clone()).collect();
    let spans = super::varying::segments(&owned, year_start, year_end);
    let changed = spans.len() > 1;
    let (segs, method) = if !changed {
        (Vec::new(), None)
    } else if segments.is_empty() {
        (
            super::varying::prorate(lines, &spans),
            Some(super::varying::Method::Proration),
        )
    } else {
        (
            segments.to_vec(),
            Some(super::varying::Method::InterimClosing),
        )
    };

    let mut fell_back: Vec<&'static str> = Vec::new();
    for (key, total) in figures {
        if total == 0 {
            continue;
        }
        let effective = if segs.is_empty() {
            None
        } else {
            super::varying::effective_ppm(&partners, &segs, key, Basis::ProfitOrLoss)
        };
        let shares = match effective {
            Some(ppm) => allocate_with(total, &ppm),
            // A line every segment carries nothing on gives no basis for
            // preferring one partner's percentage to another's. The year-end
            // split is the fallback, and it is named rather than assumed.
            None => {
                if changed {
                    fell_back.push(key);
                }
                allocate_as_of(total, &partners, Basis::ProfitOrLoss, Some(year_end))
            }
        };
        for share in shares {
            out[share.partner].by_line.insert(key, share.dollars);
        }
    }

    let mut warnings = Vec::new();
    // Named, with their percentages. The unnamed version of this fired on every
    // build for any partnership with a special allocation — which is a permanent,
    // deliberate, correctly-recorded state — and said nothing a preparer could
    // act on. A warning that can never be resolved teaches people to skip the
    // panel, and the panel also carries the ones that matter.
    let special: Vec<String> = partners
        .iter()
        .filter(|p| {
            let s = p.shares_on(year_end);
            s.profit_ppm != s.loss_ppm
        })
        .map(|p| {
            let s = p.shares_on(year_end);
            format!(
                "{} takes {} of profit but {} of loss",
                p.name,
                format_ppm(s.profit_ppm),
                format_ppm(s.loss_ppm)
            )
        })
        .collect();
    if !special.is_empty() {
        warnings.push(format!(
            "Special allocation: {}. Income items and loss items were split on different \
             percentages — confirm once that this matches the partnership agreement.",
            special.join("; ")
        ));
    }

    // The method has to be stated, because §706(d) offers two and the return does
    // not say on its face which one produced the figures. Proration in
    // particular is available only by election, so a preparer who did not know
    // it had been used would be filing an election they never made.
    if let Some(method) = method {
        let dates = spans
            .iter()
            .skip(1)
            .map(|(from, _)| from.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        warnings.push(match method {
            super::varying::Method::InterimClosing => format!(
                "The partners' percentages changed during {year} ({dates}), so the year was \
                 divided there and each part allocated on the percentages in force during it — \
                 §706(d), by interim closing of the books, which is the default method. Each \
                 part's figures came from the ledger for those dates."
            ),
            super::varying::Method::Proration => format!(
                "The partners' percentages changed during {year} ({dates}), so the year was \
                 divided there and each part allocated on the percentages in force during it. \
                 This return was built from figures rather than from the books, so each part's \
                 share of the year was set by its length in days — the proration method, which \
                 §706(d) allows only by election. Either make that election or rebuild the \
                 return from the ledger, which closes the books at each change instead."
            ),
        });
    }
    if !fell_back.is_empty() {
        warnings.push(format!(
            "Line(s) {} carried nothing in any part of {year} taken separately, although the year \
             as a whole does. They were split on the percentages in force at 31 December rather \
             than over the year — check those figures on each K-1 by hand.",
            fell_back.join(", ")
        ));
    }
    let _ = profit_and_loss_shares_differ(&partners);
    (out, warnings)
}

/// Split `total` on percentages given directly, rather than read from partners.
///
/// The same exact largest-remainder arithmetic as [`super::allocate::allocate`],
/// applied to an effective split computed over a segmented year. Written as a
/// thin adapter rather than by duplicating the loop: the guarantee that matters
/// — the shares sum to `total` — lives in one place, and a second copy of it is
/// a second place for it to stop being true.
fn allocate_with(total: i64, ppm: &[i64]) -> Vec<super::allocate::Share> {
    super::allocate::allocate_on_ppm(total, ppm)
}

fn fill_k1(
    doc: &mut Document,
    map: &FieldMap,
    profile: &BusinessProfile,
    filing: &PartnerFiling,
    shares: &PartnerShares,
    capital: Option<&super::capital::CapitalAccount>,
    year_start: NaiveDate,
    year_end: NaiveDate,
) -> Result<Vec<String>, FormError> {
    let p = &filing.partner;

    set_text(doc, map, k1::PARTNERSHIP_EIN, &profile.ein)?;
    set_text(
        doc,
        map,
        k1::PARTNERSHIP_ADDRESS,
        &profile.address.as_block(&profile.legal_name),
    )?;

    // Blank rather than absent when this machine holds no TIN: a visibly empty
    // box is a form somebody notices, which a plausible-looking wrong one is not.
    set_text(
        doc,
        map,
        k1::PARTNER_TIN,
        filing.tin.as_deref().unwrap_or(""),
    )?;
    set_text(doc, map, k1::PARTNER_ADDRESS, &p.address.as_block(&p.name))?;
    set_text(doc, map, k1::ENTITY_TYPE, &p.entity_type)?;

    match p.partner_type {
        PartnerType::General => set_check(doc, map, k1::TYPE_GENERAL, k1::ON)?,
        PartnerType::Limited => set_check(doc, map, k1::TYPE_LIMITED, k1::ON_SECOND)?,
    }
    match p.residency {
        Residency::Domestic => set_check(doc, map, k1::DOMESTIC, k1::ON)?,
        Residency::Foreign => set_check(doc, map, k1::FOREIGN, k1::ON_SECOND)?,
    }

    if p.is_final_for(year_end) {
        set_check(doc, map, k1::FINAL, k1::ON)?;
    }

    let (begin, end) = p.shares_over(year_start, year_end);
    for (field, ppm) in [
        (k1::PROFIT_BEGIN, begin.profit_ppm),
        (k1::PROFIT_END, end.profit_ppm),
        (k1::LOSS_BEGIN, begin.loss_ppm),
        (k1::LOSS_END, end.loss_ppm),
        (k1::CAPITAL_BEGIN, begin.capital_ppm),
        (k1::CAPITAL_END, end.capital_ppm),
    ] {
        set_text(doc, map, field, &format_ppm(ppm))?;
    }

    // --- Part III: this partner's share of each Schedule K line ---
    let mut warnings = Vec::new();

    // --- Item L: the partner's capital account ---
    //
    // All six rows or none of them. Item L is an identity — opening, plus what
    // went in, plus this year's result, less what came out, equals closing — and
    // a reader checks it by adding the column. Writing only the rows that carry a
    // figure leaves a column that does not add up unless you know a blank means
    // zero, and one that does not add up is the first thing an examiner asks
    // about. `None` is the return built without a ledger, where every row is left
    // blank and editable, exactly as page one's figures are.
    if let Some(cap) = capital {
        for (field, dollars) in [
            (k1::L_BEGIN, cap.beginning),
            (k1::L_CONTRIBUTED, cap.contributed),
            (k1::L_NET_INCOME, cap.net_income),
            (k1::L_OTHER, cap.other),
            // The magnitude: this box's parentheses are printed on the form, and
            // a minus sign inside them reads as the opposite of what it is.
            (k1::L_WITHDRAWN, cap.withdrawals),
            (k1::L_ENDING, cap.ending()),
        ] {
            write_money(doc, map, field, dollars, &mut warnings)?;
        }
        warnings.extend(cap.warnings());
    }

    for (line_key, field) in k1::PART_III {
        // Line 1 is written even at zero, matching Schedule K: it is the figure
        // the partner reconciles their own return against.
        let amount = shares.get(line_key);
        if *line_key == "k1" {
            set_text(doc, map, field, &super::lines::format_dollars(amount))?;
        } else {
            write_money(doc, map, field, amount, &mut warnings)?;
        }
    }

    let mut needs_a_code: Vec<&str> = Vec::new();
    for b in k1::CODED_BOXES {
        let amount = shares.get(b.line_key);
        if amount == 0 {
            continue;
        }
        write_money(doc, map, b.amount_field, amount, &mut warnings)?;
        match b.code {
            Some(code) => set_text(doc, map, b.code_field, code)?,
            None => needs_a_code.push(b.number),
        }
    }
    if !needs_a_code.is_empty() {
        warnings.push(format!(
            "{}: box(es) {} carry an amount with no code. Which letter applies depends on facts the \
             books do not hold — which charitable limit, what kind of \"other\" item — so the code \
             is left for you to enter rather than guessed.",
            p.name,
            needs_a_code.join(", ")
        ));
    }

    Ok(warnings)
}

/// The date format the IRS forms use.
fn us_date(d: NaiveDate) -> String {
    format!("{:02}/{:02}/{}", d.month(), d.day(), d.year())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Address;
    use crate::tax::acroform;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn profile() -> BusinessProfile {
        BusinessProfile {
            legal_name: "Clovelly Technology Partners LLC".into(),
            address: Address {
                street: "1 Example Street".into(),
                suite: Some("Suite 4".into()),
                city: "Cape Town".into(),
                state: "WC".into(),
                postal_code: "8001".into(),
                country: None,
            },
            ein: "88-1234567".into(),
            naics_code: "541511".into(),
            formation_date: day(2021, 7, 1),
            principal_activity: Some("Software".into()),
            principal_product: Some("Accounting software".into()),
        }
    }

    fn partner(name: &str, t: PartnerType, r: Residency, pct: f64) -> Partner {
        Partner {
            history: Vec::new(),
            partner_id: name.to_lowercase(),
            name: name.into(),
            partner_type: t,
            residency: r,
            entity_type: "Individual".into(),
            address: Address {
                street: "2 Other Road".into(),
                suite: None,
                city: "Cape Town".into(),
                state: "WC".into(),
                postal_code: "8001".into(),
                country: None,
            },
            start_date: day(2021, 7, 1),
            end_date: None,
            shares: Shares::from_percents(pct, pct, pct),
        }
    }

    fn two_partner_request() -> ReturnRequest {
        ReturnRequest {
            segments: Vec::new(),
            year: FORM_TAX_YEAR,
            profile: profile(),
            partners: vec![
                PartnerFiling {
                    partner: partner("Alice", PartnerType::General, Residency::Domestic, 50.0),
                    tin: Some("123-45-6789".into()),
                },
                PartnerFiling {
                    partner: partner("Bob", PartnerType::Limited, Residency::Foreign, 50.0),
                    tin: Some("987-65-4321".into()),
                },
            ],
            schedule_b: Default::default(),
            relationships: Vec::new(),
            assets: Vec::new(),
            schedule_l: None,
            capital: Default::default(),
            nondeductible: Vec::new(),
            detail: Default::default(),
            options: Default::default(),
            book_income_cents: 0,
        }
    }

    /// The constants above name boxes by number, and nothing about `f1_14[0]`
    /// says "EIN". This is the check that they still are what they claim: the
    /// vendored PDF is re-read and every constant must resolve in it.
    ///
    /// It fails the day somebody drops in a new revision of the form, which is
    /// exactly when it should — the numbering shifts between tax years, and a
    /// stale constant fills a neighbouring box in silence.
    /// Run against **every** year carried, not just the current one. A prior
    /// year whose boxes moved is not a compile error and not a runtime error —
    /// it is a return with figures in the wrong boxes, and this is the only
    /// thing standing between that and a filing.
    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_forms() {
        for blanks in FORM_YEARS {
            // An unmapped revision is one whose boxes are known *not* to line up
            // — checking its names would pass, because they all exist and mean
            // something else. `mapped` records that, and
            // `an_unmapped_revision_fills_identity_only` covers it instead.
            if blanks.mapped {
                check_year(blanks);
            }
        }
    }

    /// Every `fN_M[i]` / `cN_M[i]` field name a module's source names.
    ///
    /// Scanned rather than listed. The alternative — each module exporting a
    /// hand-written array — is a second place to remember, and the whole reason
    /// this check exists is that somebody did not remember.
    fn field_literals(src: &str) -> Vec<String> {
        let mut out = Vec::new();
        let bytes: Vec<char> = src.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] != '"' {
                i += 1;
                continue;
            }
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && bytes[j] != '"' {
                j += 1;
            }
            let lit: String = bytes[start..j].iter().collect();
            // fN_MM[i] or cN_MM[i], and nothing else.
            let looks_like_field = {
                let mut cs = lit.chars();
                matches!(cs.next(), Some('f' | 'c'))
                    && lit.contains('_')
                    && lit.ends_with(']')
                    && lit[1..]
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == '_' || c == '[' || c == ']')
            };
            if looks_like_field && !out.contains(&lit) {
                out.push(lit);
            }
            i = j + 1;
        }
        out
    }

    fn check_year(blanks: &FormYear) {
        let year = blanks.year;
        let Some(page1) = blanks.page1 else { return };
        let doc = Document::load_mem(blanks.f1065).unwrap();
        let mut map = field_map(&doc);
        let mut names: Vec<&str> = vec![
            page1.legal_name,
            page1.street,
            page1.city,
            page1.principal_activity,
            page1.principal_product,
            page1.naics,
            page1.ein,
            page1.date_started,
            page1.k1_count,
            page1.lines.l1a_gross_receipts,
            page1.lines.l1b_returns,
            page1.lines.l1c_balance,
            page1.lines.l2_cogs,
            page1.lines.l3_gross_profit,
            page1.lines.l4_other_partnerships,
            page1.lines.l5_farm,
            page1.lines.l6_form_4797,
            page1.lines.l7_other_income,
            page1.lines.l8_total_income,
            page1.lines.l9_salaries,
            page1.lines.l10_guaranteed,
            page1.lines.l11_repairs,
            page1.lines.l12_bad_debts,
            page1.lines.l13_rent,
            page1.lines.l14_taxes,
            page1.lines.l15_interest,
            page1.lines.l16a_depreciation,
            page1.lines.l16b_depreciation_elsewhere,
            page1.lines.l16c_depreciation_net,
            page1.lines.l17_depletion,
            page1.lines.l18_retirement,
            page1.lines.l19_benefits,
            page1.lines.l20_energy,
            page1.lines.l21_other_deductions,
            page1.lines.l22_total_deductions,
            page1.lines.l23_ordinary_income,
            // Written by `fill_1065` and `fill_schedule_k` but missing from this
            // list until a 2023 build failed on the preparer box — a name that
            // nothing checked, in a revision nothing had opened.
            page1.preparer_name,
            sched_k::L1_ORDINARY,
            sched_k::L3C_NET_RENTAL,
            sched_k::L4C_TOTAL_GUARANTEED,
            sched_k::ANALYSIS,
        ];
        names.extend(
            [page1.suite, page1.state, page1.country, page1.postal_code]
                .into_iter()
                .flatten(),
        );
        for name in names {
            assert!(
                map.find(name).is_some(),
                "the {year} Form 1065 has no field {name}"
            );
        }

        // The schedules that write into this same document. Their field names live
        // in their own modules, and *not checking them here* is how a 2023 return
        // got as far as a user before failing on `c4_1[1]`: this test proved page
        // one and Schedule K on every year and said nothing about the rest of the
        // form.
        //
        // Read out of the source rather than from a list kept by hand, because a
        // list kept by hand is what drifted.
        for (module, src) in [
            ("Schedule L", include_str!("schedule_l.rs")),
            ("Schedule M", include_str!("schedule_m.rs")),
        ] {
            for name in field_literals(src) {
                assert!(
                    map.find(&name).is_some(),
                    "the {year} Form 1065 has no {module} field {name}"
                );
            }
        }

        // Schedule B, against what filling it actually needs: every question the
        // year *asks* must have its control boxes, or an answer would land in a
        // neighbouring question's box. Follow-ups are allowed to be missing —
        // older forms lay the partnership-representative block out differently,
        // and `fill` reports that rather than failing.
        if let Some(table) = blanks.schedule_b {
            use crate::tax::schedule_b::Control;
            for q in table {
                let boxes: Vec<&str> = match &q.control {
                    Control::YesNo { yes, no } => vec![yes, no],
                    Control::Choice(opts) => opts.iter().map(|o| o.field).collect(),
                    Control::Check { field } | Control::Entry { field, .. } => vec![field],
                };
                for b in boxes {
                    assert!(
                        map.find(b).is_some(),
                        "the {year} Form 1065 asks question {} but has no box {b} for it",
                        q.number
                    );
                }
            }
        }

        let sched = Document::load_mem(blanks.sk1).unwrap();
        let smap = field_map(&sched);

        // Part III: every box a partner's share is written into. Catches the
        // revision that renumbers the K-1 independently of the 1065, which has
        // happened before and which nothing else here would notice.
        for (line_key, field) in k1::PART_III {
            assert!(
                smap.find(field).is_some(),
                "the {year} Schedule K-1 has no field {field} for Schedule K line {line_key}"
            );
        }
        for b in k1::CODED_BOXES {
            assert!(
                smap.find(b.amount_field).is_some(),
                "the {year} Schedule K-1 has no amount box {} for line {}",
                b.amount_field,
                b.number
            );
            assert!(
                smap.find(b.code_field).is_some(),
                "the {year} Schedule K-1 has no code box {} for line {}",
                b.code_field,
                b.number
            );
        }
        for name in [
            k1::FINAL,
            k1::PARTNERSHIP_EIN,
            k1::PARTNERSHIP_ADDRESS,
            k1::PARTNER_TIN,
            k1::PARTNER_ADDRESS,
            k1::TYPE_GENERAL,
            k1::TYPE_LIMITED,
            k1::DOMESTIC,
            k1::FOREIGN,
            k1::ENTITY_TYPE,
            k1::PROFIT_BEGIN,
            k1::PROFIT_END,
            k1::LOSS_BEGIN,
            k1::LOSS_END,
            k1::CAPITAL_BEGIN,
            k1::CAPITAL_END,
            k1::L_BEGIN,
            k1::L_CONTRIBUTED,
            k1::L_NET_INCOME,
            k1::L_OTHER,
            k1::L_WITHDRAWN,
            k1::L_ENDING,
        ] {
            assert!(
                smap.find(name).is_some(),
                "the {year} Schedule K-1 has no field {name}"
            );
        }
    }

    /// Ticking a box with the wrong appearance state leaves it looking unticked.
    #[test]
    fn the_checkbox_states_are_the_ones_the_form_was_built_with() {
        let sched = Document::load_mem(F1065_SK1).unwrap();
        let map = field_map(&sched);
        assert_eq!(
            acroform::on_states(&sched, &map, k1::TYPE_GENERAL),
            [k1::ON]
        );
        assert_eq!(
            acroform::on_states(&sched, &map, k1::TYPE_LIMITED),
            [k1::ON_SECOND]
        );
        assert_eq!(acroform::on_states(&sched, &map, k1::DOMESTIC), [k1::ON]);
        assert_eq!(
            acroform::on_states(&sched, &map, k1::FOREIGN),
            [k1::ON_SECOND]
        );
        assert_eq!(acroform::on_states(&sched, &map, k1::FINAL), [k1::ON]);
    }

    #[test]
    fn a_return_carries_the_partnership_header() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);

        let get = |n: &str| acroform::get_value_in(&doc, &map, FORM_ROOT, n).unwrap_or_default();
        assert_eq!(
            get(PAGE1_2025.legal_name),
            "Clovelly Technology Partners LLC"
        );
        assert_eq!(get(PAGE1_2025.ein), "88-1234567");
        assert_eq!(get(PAGE1_2025.naics), "541511");
        assert_eq!(get(PAGE1_2025.date_started), "07/01/2021");
        assert_eq!(get(PAGE1_2025.city), "Cape Town");
        assert_eq!(get(PAGE1_2025.suite.unwrap()), "Suite 4");
        assert_eq!(get(PAGE1_2025.k1_count), "2", "one K-1 per partner");
    }

    /// The bug this whole design exists to prevent: two K-1s sharing a field
    /// name means typing one partner's TIN fills it in for everybody.
    #[test]
    fn each_partners_k1_holds_that_partners_details_and_not_another_s() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);

        let tin = |n: usize| {
            acroform::get_value_in(&doc, &map, &k1_namespace(n), k1::PARTNER_TIN)
                .unwrap_or_default()
        };
        assert_eq!(tin(1), "123-45-6789");
        assert_eq!(tin(2), "987-65-4321");
        assert_ne!(tin(1), tin(2), "the two K-1s share a field");

        // And a bare leaf name is now ambiguous rather than silently one of them.
        assert!(
            map.find(k1::PARTNER_TIN).is_none(),
            "a bare name resolved to one of two K-1s"
        );
    }

    #[test]
    fn a_general_domestic_and_a_limited_foreign_partner_get_different_boxes() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        let get = |i: usize, f: &str| {
            acroform::get_value_in(&doc, &map, &k1_namespace(i), f).unwrap_or_default()
        };
        assert_eq!(get(1, k1::TYPE_GENERAL), "/1", "Alice is general");
        assert_eq!(get(1, k1::DOMESTIC), "/1", "Alice is domestic");
        assert_eq!(get(2, k1::TYPE_LIMITED), "/2", "Bob is limited");
        assert_eq!(get(2, k1::FOREIGN), "/2", "Bob is foreign");
    }

    /// A partner who left years ago must not receive a K-1 for this year.
    ///
    /// `build_return` used to fill one for whatever it was handed. Passing an
    /// unfiltered partner list — the obvious mistake for any new caller, and the
    /// desktop is about to become one — did not fail: it produced a K-1 for
    /// somebody who left in 2019, ticked **Final**, with nothing in either
    /// column of item J. Every figure on it is defensible in isolation, which is
    /// exactly why nobody would look twice.
    #[test]
    fn a_partner_who_left_years_ago_gets_no_k1_however_the_caller_asks() {
        let mut req = two_partner_request();
        let mut gone = partner("Long Gone", PartnerType::General, Residency::Domestic, 0.0);
        gone.start_date = day(2015, 1, 1);
        gone.end_date = Some(day(2019, 6, 30));
        req.partners.push(PartnerFiling {
            partner: gone,
            tin: Some("111-22-3333".into()),
        });

        let two_only = build_return(&two_partner_request()).unwrap();
        let with_stale = build_return(&req).unwrap();

        assert_eq!(
            with_stale.page_count, two_only.page_count,
            "the departed partner was given a K-1 page"
        );

        let doc = Document::load_mem(&with_stale.pdf).unwrap();
        let map = field_map(&doc);
        assert!(
            acroform::get_value_in(&doc, &map, &k1_namespace(3), k1::PARTNER_TIN).is_none(),
            "a third K-1 exists"
        );

        // Page one must agree with the pages behind it.
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, PAGE1_2025.k1_count),
            Some("2".into()),
            "the K-1 count still counted the departed partner"
        );

        // And the omission is stated, not silent.
        assert!(
            with_stale
                .warnings
                .iter()
                .any(|w| w.contains("Long Gone") && w.contains("held no interest")),
            "dropping a partner went unmentioned: {:?}",
            with_stale.warnings
        );
    }

    /// A prior-year partner's share must not drag the totals off 100%.
    #[test]
    fn share_totals_are_taken_over_the_partners_actually_on_the_return() {
        let mut req = two_partner_request(); // two halves, totalling the whole
        let mut gone = partner("Long Gone", PartnerType::General, Residency::Domestic, 40.0);
        gone.start_date = day(2015, 1, 1);
        gone.end_date = Some(day(2019, 6, 30));
        req.partners.push(PartnerFiling {
            partner: gone,
            tin: None,
        });

        let bundle = build_return(&req).unwrap();
        assert!(
            !bundle.warnings.iter().any(|w| w.contains("not 100%")),
            "a partner who is not on the return was counted into its shares: {:?}",
            bundle.warnings
        );
        assert!(
            !bundle.warnings.iter().any(|w| w.contains("No TIN")),
            "warned about a missing TIN for a partner who gets no K-1"
        );
    }

    #[test]
    fn one_page_is_added_per_partner() {
        let one = ReturnRequest {
            partners: vec![two_partner_request().partners[0].clone()],
            ..two_partner_request()
        };
        let base = build_return(&one).unwrap().page_count;
        let two = build_return(&two_partner_request()).unwrap().page_count;
        assert_eq!(two, base + 1, "a second partner adds exactly one K-1 page");
    }

    /// The filled return must still be a form, or the figures nobody computed
    /// can never be typed in.
    #[test]
    fn the_bundle_is_still_fillable() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);

        assert!(
            map.len() > 500,
            "expected the 1065's fields plus two K-1s, got {}",
            map.len()
        );

        let acro = doc
            .catalog()
            .unwrap()
            .get(b"AcroForm")
            .and_then(|o| doc.dereference(o).map(|(_, d)| d.clone()))
            .unwrap();
        let acro = acro.as_dict().unwrap();
        assert!(!acro.has(b"XFA"), "the XFA packet survived");
        assert!(
            acro.get(b"NeedAppearances")
                .ok()
                .and_then(|o| o.as_bool().ok())
                .unwrap_or(false),
            "without NeedAppearances the values are set but invisible"
        );
    }

    /// The IRS ships these forms carrying a usage-rights signature over the bytes
    /// as they built them. We rewrite those bytes and append pages, so the
    /// signature cannot still be valid — and a broken one is not inert. Reader
    /// checks it and tells whoever opens the return that "the document has been
    /// changed since it was created and use of extended features is no longer
    /// available", which on a return going to a partner or an accountant is the
    /// first thing they read.
    #[test]
    fn no_stale_signature_greets_whoever_opens_the_return() {
        // The blank form really is signed — otherwise this test proves nothing.
        let blank = Document::load_mem(F1065).unwrap();
        assert!(
            blank.catalog().unwrap().has(b"Perms"),
            "the vendored form is unsigned, so this test has stopped testing anything"
        );

        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        assert!(
            !doc.catalog().unwrap().has(b"Perms"),
            "the usage-rights signature survived into the bundle, where it is invalid"
        );
    }

    /// A prior-year return attaches that year's Form 4562, not this year's.
    ///
    /// The wiring test for the per-revision tables: `build_return_inner` passes
    /// `req.year` through, `form4562` picks that year's blank, and the boxes it
    /// writes are that revision's. Before the tables existed this attached the
    /// 2025 blank to a 2023 return, where the first Section B column is named
    /// `R4[0]` rather than `f1_26[0]` and every `f1_` number after it is off by
    /// one — so the basis printed in the recovery-period column.
    #[test]
    fn a_prior_year_return_attaches_that_years_form_4562() {
        use crate::domain::{BonusElection, DepreciableAsset, PropertyClass, System};

        let mut req = two_partner_request();
        req.year = 2023;
        req.assets = vec![DepreciableAsset {
            asset_id: "kiln".into(),
            description: "Kiln".into(),
            asset_account_id: "1500".into(),
            expense_account_id: "6500".into(),
            accumulated_account_id: "1590".into(),
            section_179_account_id: None,
            acquired_on: day(2023, 3, 1),
            placed_in_service: day(2023, 3, 1),
            cost_cents: 1_000_000,
            class: PropertyClass::SevenYear,
            system: System::Gds,
            section_179_cents: 0,
            bonus: BonusElection::Decline,
            disposed_on: None,
            notes: None,
        }];

        let bundle = build_return(&req).unwrap();
        assert!(
            !bundle
                .warnings
                .iter()
                .any(|w| w.contains("No Form 4562 is attached")),
            "2023 is carried, so the form must be attached: {:?}",
            bundle.warnings
        );

        // The 2023 blank names its first Section B column `R4[0]`; the 2025
        // blank has no such field at all. Finding it proves which blank went in.
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert!(
            map.names().any(|n| n.ends_with("R6[0]")),
            "the attached 4562 is not the 2023 revision"
        );
    }

    /// A merged form must carry the fonts its fields ask for.
    ///
    /// The Schedule K-1's fields name `HelveticaLTStd-Roman` in their /DA
    /// strings and the 1065 does not carry it, so appending one without merging
    /// resources leaves every K-1 field pointing at a font the document does not
    /// have. Viewers then substitute or draw nothing, and the K-1 pages come out
    /// blank in exactly the places that were filled in.
    #[test]
    fn a_bundle_carries_the_fonts_its_k1_fields_reference() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();

        let acro = doc
            .catalog()
            .unwrap()
            .get(b"AcroForm")
            .and_then(|o| doc.dereference(o).map(|(_, d)| d.clone()))
            .unwrap();
        let fonts = acro
            .as_dict()
            .unwrap()
            .get(b"DR")
            .and_then(|dr| doc.dereference(dr).map(|(_, o)| o.clone()))
            .unwrap();
        let fonts = fonts
            .as_dict()
            .unwrap()
            .get(b"Font")
            .and_then(|f| doc.dereference(f).map(|(_, o)| o.clone()))
            .unwrap();
        let fonts = fonts.as_dict().unwrap();

        // Every font any field asks for must be resolvable in /DR.
        let map = field_map(&doc);
        for name in map.names() {
            let Some(id) = map.find(name) else { continue };
            let Ok(dict) = doc.get_dictionary(id) else {
                continue;
            };
            let Ok(da) = dict.get(b"DA").and_then(|o| o.as_str()) else {
                continue;
            };
            let da = acroform::decode_pdf_string(da);
            // A /DA reads like "/HelveticaLTStd-Roman 9 Tf 0 g".
            let Some(font) = da
                .split_whitespace()
                .next()
                .and_then(|t| t.strip_prefix('/'))
            else {
                continue;
            };
            assert!(
                fonts.has(font.as_bytes()),
                "field {name} asks for /{font}, which the merged form does not carry"
            );
        }
    }

    /// A departing partner's K-1 is final, and item J states what they held.
    ///
    /// The ending column used to read 0%, on the reasoning that they held
    /// nothing by 31 December. The instruction for item J is the percentages
    /// *immediately before termination* — and the old rule produced a K-1
    /// asserting a 0% interest beside a Part III allocating real income to it.
    #[test]
    fn a_partner_who_left_gets_a_final_k1_stating_what_they_held() {
        let mut req = two_partner_request();
        req.partners[1].partner.end_date = Some(day(FORM_TAX_YEAR, 6, 30));

        let bundle = build_return(&req).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        let get =
            |f: &str| acroform::get_value_in(&doc, &map, &k1_namespace(2), f).unwrap_or_default();
        assert_eq!(get(k1::FINAL), "/1", "a departing partner's K-1 is final");
        assert_eq!(get(k1::PROFIT_BEGIN), "50");
        assert_eq!(
            get(k1::PROFIT_END),
            "50",
            "what they held on 30 June, not what they held on 31 December"
        );
    }

    /// Shares that do not add up are the classic silently-wrong return.
    #[test]
    fn shares_that_do_not_total_the_whole_are_reported_rather_than_swallowed() {
        let mut req = two_partner_request();
        req.partners[1].partner.shares = Shares::from_percents(30.0, 30.0, 30.0);

        let bundle = build_return(&req).unwrap();
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("add up to 80.0000%, not 100%")),
            "got {:?}",
            bundle.warnings
        );

        // Still built, and still carrying what it was told to carry.
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, &k1_namespace(2), k1::PROFIT_END),
            Some("30".into())
        );
    }

    #[test]
    fn a_missing_tin_leaves_the_box_blank_and_says_so() {
        let mut req = two_partner_request();
        req.partners[1].tin = None;

        let bundle = build_return(&req).unwrap();
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("No TIN") && w.contains("Bob")),
            "got {:?}",
            bundle.warnings
        );

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, &k1_namespace(2), k1::PARTNER_TIN),
            Some(String::new()),
            "an absent TIN must be an empty box, not somebody else's number"
        );
    }

    // --- the ledger-backed path -------------------------------------------

    /// Seed a ledger whose income statement is known by hand, so the figures on
    /// the finished page can be checked against arithmetic done on paper.
    /// Editing a partner after the year ended must be said out loud.
    ///
    /// Shares are one current figure with no effective date, so regenerating an
    /// earlier year prints today's split. Two partners at 50/50 through the year
    /// who move to 70/30 afterwards get K-1s for that year showing 70/30 — and
    /// because the two still total 100%, every other check passes. Until shares
    /// are dated, the only defence is saying so.
    #[test]
    fn a_partner_edited_since_the_year_ended_is_flagged_as_showing_todays_shares() {
        use crate::commands::partnership_commands as pc;
        use crate::domain::{Address, PartnerType as PT, Residency as R, Shares};

        let mut store = seeded_ledger();
        pc::set_profile(&mut store, "u", &profile()).unwrap();
        let (id, _) = pc::admit_partner(
            &mut store,
            "u",
            &pc::AdmitPartner {
                name: "Alice Example".into(),
                partner_type: PT::General,
                residency: R::Domestic,
                entity_type: "Individual".into(),
                address: Address {
                    street: "2 Other Road".into(),
                    suite: None,
                    city: "Cape Town".into(),
                    state: "WC".into(),
                    postal_code: "8001".into(),
                    country: None,
                },
                start_date: Some(day(2021, 7, 1)),
                shares: Shares::from_percents(50.0, 50.0, 50.0),
                tin: None,
            },
        )
        .unwrap();

        let req = ReturnRequest {
            segments: Vec::new(),
            year: FORM_TAX_YEAR,
            profile: profile(),
            partners: pc::partners_for_year(store.connection(), FORM_TAX_YEAR)
                .into_iter()
                .map(|partner| PartnerFiling { partner, tin: None })
                .collect(),
            schedule_b: Default::default(),
            relationships: Vec::new(),
            assets: Vec::new(),
            schedule_l: None,
            capital: Default::default(),
            nondeductible: Vec::new(),
            detail: Default::default(),
            options: Default::default(),
            book_income_cents: 0,
        };

        // Admitted during the year, so nothing to say yet.
        let before = build_return_from_ledger(store.connection(), &req).unwrap();
        assert!(
            !before.warnings.iter().any(|w| w.contains("stand today")),
            "warned before anything was edited: {:?}",
            before.warnings
        );

        // Now move their shares, as a person would the following spring.
        pc::update_partner(
            &mut store,
            "u",
            &pc::UpdatePartner {
                partner_id: id,
                name: "Alice Example".into(),
                partner_type: PT::General,
                residency: R::Domestic,
                entity_type: "Individual".into(),
                address: Address {
                    street: "2 Other Road".into(),
                    suite: None,
                    city: "Cape Town".into(),
                    state: "WC".into(),
                    postal_code: "8001".into(),
                    country: None,
                },
                shares: Shares::from_percents(70.0, 70.0, 70.0),
            },
        )
        .unwrap();
        store
            .connection()
            .execute(
                "UPDATE events SET timestamp = ?1 WHERE id = (SELECT MAX(id) FROM events)",
                [format!("{}-03-15T09:00:00Z", FORM_TAX_YEAR + 1)],
            )
            .unwrap();

        let after = build_return_from_ledger(store.connection(), &req).unwrap();
        assert!(
            after
                .warnings
                .iter()
                .any(|w| w.contains("stand today") && w.contains("Alice Example")),
            "a share change after the year end went unmentioned: {:?}",
            after.warnings
        );
    }

    fn seeded_ledger() -> crate::store::event_store::EventStore {
        use crate::events::types::{Event, EventAccountType, EventEnvelope, JournalLineData};
        use crate::store::event_store::EventStore;
        use crate::store::projections::ProjectionStore;

        let mut store = EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();

        let accounts = [
            ("cash", EventAccountType::Asset, "1000", "Cash"),
            ("sales", EventAccountType::Revenue, "4000", "Sales"),
            ("refunds", EventAccountType::Revenue, "4900", "Refunds"),
            (
                "cogs",
                EventAccountType::Expense,
                "5000",
                "Cost of goods sold",
            ),
            ("wages", EventAccountType::Expense, "6000", "Wages"),
            ("rent", EventAccountType::Expense, "6100", "Rent"),
            (
                "mystery",
                EventAccountType::Expense,
                "6999",
                "Unmapped expense",
            ),
        ];
        for (id, ty, number, name) in accounts {
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

        // Amounts in cents. Debits positive, credits negative.
        let mut post = |id: &str, day: u32, pairs: Vec<(&str, i64)>| {
            let lines: Vec<JournalLineData> = pairs
                .into_iter()
                .enumerate()
                .map(|(i, (acct, amount))| JournalLineData {
                    line_id: format!("{id}-{i}"),
                    account_id: acct.into(),
                    amount,
                    currency: "USD".into(),
                    exchange_rate: None,
                    memo: None,
                })
                .collect();
            let e = Event::JournalEntryPosted {
                entry_id: id.into(),
                date: NaiveDate::from_ymd_opt(FORM_TAX_YEAR, 6, day).unwrap(),
                memo: "seed".into(),
                lines,
                reference: None,
                source: None,
            };
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        };

        // Sales $4,000.50 (credit revenue)
        post("e1", 1, vec![("cash", 400_050), ("sales", -400_050)]);
        // Refunds $100.50 — contra-revenue, a debit inside Revenue
        post("e2", 2, vec![("refunds", 10_050), ("cash", -10_050)]);
        // COGS $1,000.50
        post("e3", 3, vec![("cogs", 100_050), ("cash", -100_050)]);
        // Wages $800.50
        post("e4", 4, vec![("wages", 80_050), ("cash", -80_050)]);
        // Rent $200.50
        post("e5", 5, vec![("rent", 20_050), ("cash", -20_050)]);
        // And one expense nobody mapped: $50.00
        post("e6", 6, vec![("mystery", 5_000), ("cash", -5_000)]);

        store
    }

    fn map_seeded_accounts(conn: &rusqlite::Connection) {
        use crate::tax::lines::set_account_line;
        set_account_line(conn, "sales", "l1a", 0).unwrap();
        set_account_line(conn, "refunds", "l1b", 0).unwrap();
        set_account_line(conn, "cogs", "l2", 0).unwrap();
        set_account_line(conn, "wages", "l9", 0).unwrap();
        set_account_line(conn, "rent", "l13", 0).unwrap();
        // "mystery" deliberately left unmapped.
    }

    /// The end-to-end property: figures posted to the ledger reach the finished
    /// page, and the totals printed on that page are the arithmetic of the other
    /// figures printed on it.
    #[test]
    fn a_return_built_from_the_ledger_carries_income_lines_that_add_up() {
        let store = seeded_ledger();
        map_seeded_accounts(store.connection());

        let bundle = build_return_from_ledger(store.connection(), &two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        // The separators come off before parsing: the box carries "4,001" now, and
        // reading the arithmetic back is what this test is about, not the
        // punctuation — which `lines::figures_are_grouped_in_threes_and_keep_their_sign`
        // covers on its own.
        let get = |n: &str| {
            acroform::get_value_in(&doc, &map, FORM_ROOT, n)
                .unwrap_or_default()
                .replace(',', "")
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("{n} is not a number"))
        };

        // Rounded once per line, away from zero.
        assert_eq!(get(PAGE1_2025.lines.l1a_gross_receipts), 4001, "$4,000.50");
        assert_eq!(
            get(PAGE1_2025.lines.l1b_returns),
            101,
            "refunds print positive"
        );
        assert_eq!(get(PAGE1_2025.lines.l2_cogs), 1001);
        assert_eq!(get(PAGE1_2025.lines.l9_salaries), 801);
        assert_eq!(get(PAGE1_2025.lines.l13_rent), 201);

        // Every total, recomputed from what the page itself shows.
        let (l1a, l1b, l2) = (
            get(PAGE1_2025.lines.l1a_gross_receipts),
            get(PAGE1_2025.lines.l1b_returns),
            get(PAGE1_2025.lines.l2_cogs),
        );
        let (l9, l13) = (
            get(PAGE1_2025.lines.l9_salaries),
            get(PAGE1_2025.lines.l13_rent),
        );

        assert_eq!(get(PAGE1_2025.lines.l1c_balance), l1a - l1b);
        assert_eq!(get(PAGE1_2025.lines.l3_gross_profit), (l1a - l1b) - l2);
        assert_eq!(get(PAGE1_2025.lines.l8_total_income), (l1a - l1b) - l2);
        assert_eq!(get(PAGE1_2025.lines.l22_total_deductions), l9 + l13);
        assert_eq!(
            get(PAGE1_2025.lines.l23_ordinary_income),
            get(PAGE1_2025.lines.l8_total_income) - get(PAGE1_2025.lines.l22_total_deductions),
            "the bottom line must be the page's own arithmetic"
        );
        assert_eq!(get(PAGE1_2025.lines.l23_ordinary_income), 2899 - 1002);
    }

    /// An expense with no line is money missing from the return. It must be
    /// named on the way past, not swept into a line it was never assigned.
    #[test]
    fn an_unmapped_account_is_reported_and_not_silently_absorbed() {
        let store = seeded_ledger();
        map_seeded_accounts(store.connection());

        let bundle = build_return_from_ledger(store.connection(), &two_partner_request()).unwrap();
        let joined = bundle.warnings.join(" ");
        assert!(joined.contains("6999"), "got {joined}");
        assert!(joined.contains("Unmapped expense"), "got {joined}");

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, PAGE1_2025.lines.l21_other_deductions),
            None,
            "the unmapped $50 must not have landed on other deductions"
        );
    }

    /// A ledger nobody has mapped yet produces an identity-only return, and says
    /// so — rather than a page of zeros that reads as a completed nil return.
    #[test]
    fn an_unmapped_ledger_leaves_the_money_lines_blank_and_says_why() {
        let store = seeded_ledger();
        let bundle = build_return_from_ledger(store.connection(), &two_partner_request()).unwrap();

        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("No accounts are mapped")),
            "got {:?}",
            bundle.warnings
        );

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, PAGE1_2025.lines.l1a_gross_receipts),
            None,
            "no figure was known, so no figure is claimed"
        );
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, PAGE1_2025.lines.l23_ordinary_income),
            Some("0".into()),
            "the bottom line is always written"
        );
    }

    /// Identity-only remains available and unchanged.
    #[test]
    fn build_return_still_fills_identity_and_leaves_the_money_blank() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, PAGE1_2025.legal_name),
            Some("Clovelly Technology Partners LLC".into())
        );
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, PAGE1_2025.lines.l1a_gross_receipts),
            None
        );
    }

    /// A large but entirely ordinary partnership must not lose a digit.
    ///
    /// Nine-figure gross receipts is a mid-sized business, not an edge case, and
    /// a return that drops the leading digit of one is wrong by an order of
    /// magnitude while looking perfectly well-formed.
    #[test]
    fn a_nine_figure_figure_survives_the_round_trip_to_the_page() {
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 987_654_321);
        lines.set_for_test("l9", 123_456_789);

        let mut doc = Document::load_mem(F1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        let warnings = fill_income_lines(&mut doc, &map, &PAGE1_2025, &lines).unwrap();
        assert!(
            warnings.is_empty(),
            "nothing should have been refused: {warnings:?}"
        );

        // Round-trip through a real save/load, not just the in-memory dict.
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let doc = Document::load_mem(&bytes).unwrap();
        let map = field_map(&doc);
        let get = |n: &str| acroform::get_value_in(&doc, &map, FORM_ROOT, n).unwrap_or_default();

        assert_eq!(get(PAGE1_2025.lines.l1a_gross_receipts), "987,654,321");
        assert_eq!(get(PAGE1_2025.lines.l9_salaries), "123,456,789");
        assert_eq!(
            get(PAGE1_2025.lines.l23_ordinary_income),
            "864,197,532",
            "987,654,321 - 123,456,789"
        );
    }

    /// The money boxes declare no `/MaxLen`, which is why a nine-figure figure
    /// fits. If a future revision of the form adds one, this fails and whoever
    /// dropped the new PDF in finds out here rather than from a clipped return.
    #[test]
    fn the_money_boxes_declare_no_length_limit() {
        let doc = Document::load_mem(F1065).unwrap();
        let map = field_map(&doc);
        for field in [
            PAGE1_2025.lines.l1a_gross_receipts,
            PAGE1_2025.lines.l1b_returns,
            PAGE1_2025.lines.l1c_balance,
            PAGE1_2025.lines.l2_cogs,
            PAGE1_2025.lines.l3_gross_profit,
            PAGE1_2025.lines.l8_total_income,
            PAGE1_2025.lines.l9_salaries,
            PAGE1_2025.lines.l16c_depreciation_net,
            PAGE1_2025.lines.l21_other_deductions,
            PAGE1_2025.lines.l22_total_deductions,
            PAGE1_2025.lines.l23_ordinary_income,
        ] {
            assert_eq!(
                acroform::max_len(&doc, &map, field),
                None,
                "{field} has grown a /MaxLen; check every figure still fits"
            );
        }
    }

    /// The identity boxes do declare limits, and what we write fits them exactly
    /// — by validation, not by luck. This is the test that notices if either the
    /// validation or the form's limit moves.
    #[test]
    fn the_identity_boxes_that_declare_a_limit_are_filled_to_within_it() {
        let doc = Document::load_mem(F1065).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::max_len(&doc, &map, PAGE1_2025.ein),
            Some(10),
            "an EIN is NN-NNNNNNN"
        );

        let sched = Document::load_mem(F1065_SK1).unwrap();
        let smap = field_map(&sched);
        assert_eq!(
            acroform::max_len(&sched, &smap, k1::PARTNERSHIP_EIN),
            Some(10)
        );
        assert_eq!(
            acroform::max_len(&sched, &smap, k1::PARTNER_TIN),
            Some(11),
            "an SSN is NNN-NN-NNNN, one longer than an EIN"
        );
    }

    /// An over-long value must never be silently shortened.
    #[test]
    fn a_value_too_long_for_its_box_is_refused_rather_than_truncated() {
        let mut doc = Document::load_mem(F1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);

        let err = set_text(&mut doc, &map, PAGE1_2025.ein, "88-1234567-EXTRA").unwrap_err();
        match err {
            FormError::ValueTooLong { len, max, .. } => {
                assert_eq!(max, 10);
                assert_eq!(len, 16);
            }
            other => panic!("expected ValueTooLong, got {other:?}"),
        }
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, PAGE1_2025.ein),
            None,
            "the box must be untouched, not holding a shortened EIN"
        );
    }

    /// Scratch: writes a sample return to $SAMPLE_OUT for eyeballing. Ignored,
    /// so it only runs when asked for by name.
    #[test]
    #[ignore]
    fn zz_write_sample_return() {
        use crate::tax::lines::LineDetail;
        use crate::tax::schedule_b::{self, ScheduleB};

        let mut sb = ScheduleB::default();
        sb.set("b1", "llp");
        for q in [
            "b3a", "b3b", "b5", "b6", "b7", "b8", "b9", "b12", "b16a", "b19", "b20", "b21", "b23",
            "b27", "b30", "b4",
        ] {
            sb.set(q, schedule_b::NO);
        }
        // Both attachments, so the sample shows them.
        sb.set("b2b", schedule_b::YES);
        sb.set("b31", schedule_b::YES);
        sb.set("b31_total", "2");
        sb.set("pr_first", "Dana");
        sb.set("pr_last", "Whitlock");
        sb.set("pr_street", "1200 Harbor Way");
        sb.set("pr_city", "Corpus Christi");
        sb.set("pr_state", "TX");
        sb.set("pr_zip", "78401");
        sb.set("pr_phone", "361-555-0142");

        let mut req = two_partner_request();
        req.schedule_b = sb;
        req.detail.insert(
            "l21",
            vec![
                LineDetail {
                    account_id: "1".into(),
                    account_number: "6100".into(),
                    account_name: "Advertising and promotion".into(),
                    cents: 12_450_00,
                },
                LineDetail {
                    account_id: "2".into(),
                    account_number: "6200".into(),
                    account_name: "Professional fees".into(),
                    cents: 8_900_00,
                },
                LineDetail {
                    account_id: "3".into(),
                    account_number: "6300".into(),
                    account_name: "Software subscriptions".into(),
                    cents: 4_215_00,
                },
                LineDetail {
                    account_id: "4".into(),
                    account_number: "6400".into(),
                    account_name: "Bank and merchant charges".into(),
                    cents: 1_980_50,
                },
                LineDetail {
                    account_id: "5".into(),
                    account_number: "6500".into(),
                    account_name: "Office supplies".into(),
                    cents: 2_104_50,
                },
            ],
        );

        // A Schedule L with both columns and a paired gross/contra row, which is
        // the placement worth looking at on paper.
        let mut sl = crate::tax::schedule_l::ScheduleL::default();
        sl.set_for_test("sl1", 84_300, 96_150);
        sl.set_for_test("sl2a", 41_000, 52_400);
        sl.set_for_test("sl2b", 3_000, 4_200);
        sl.set_for_test("sl9a", 220_000, 220_000);
        sl.set_for_test("sl9b", 66_000, 88_000);
        sl.set_for_test("sl15", 19_300, 24_150);
        sl.set_for_test("sl21", 257_000, 272_200);
        req.schedule_l = Some(sl);
        req.book_income_cents = 133_950_00;

        let mut lines = crate::tax::lines::Form1065Lines::default();
        for (k, v) in [
            ("l1a", 480_000i64),
            ("l2", 150_000),
            ("l9", 120_000),
            ("l13", 36_000),
            ("l14", 18_400),
            ("l16a", 22_000),
            ("l21", 29_650),
            ("k5", 3_200),
            ("k13a", 5_000),
            ("k12", 14_000),
            ("k19a", 60_000),
        ] {
            lines.set_for_test(k, v);
        }

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        let out = std::env::var("SAMPLE_OUT").unwrap_or_else(|_| "sample-1065.pdf".into());
        std::fs::write(&out, &bundle.pdf).unwrap();
        println!("WROTE {out} ({} pages)", bundle.page_count);
        for w in &bundle.warnings {
            println!("WARN {w}");
        }
    }

    /// A Yes on 2a has to produce a real Schedule B-1 in the bundle, filled from
    /// the partners the books already hold.
    #[test]
    fn question_2a_puts_a_filled_schedule_b1_in_the_bundle() {
        use crate::domain::Shares;
        use crate::tax::schedule_b::{ScheduleB, YES};

        let mut req = two_partner_request();
        let mut owner = partner(
            "Holdings LLC",
            PartnerType::General,
            Residency::Domestic,
            60.0,
        );
        owner.entity_type = "Partnership".to_string();
        owner.shares = Shares::from_percents(60.0, 60.0, 60.0);
        req.partners = vec![PartnerFiling {
            partner: owner,
            tin: Some("98-7654321".into()),
        }];

        let mut sb = ScheduleB::default();
        sb.set("b2a", YES);
        req.schedule_b = sb;

        let before = build_return_inner(&two_partner_request(), &Default::default(), Vec::new())
            .unwrap()
            .page_count;
        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        assert!(
            bundle.page_count > before,
            "the bundle gained no pages: {} vs {before}",
            bundle.page_count
        );

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("49842K"), "Schedule B-1 is not in the bundle");
        // And the constructive-ownership caveat travels with it.
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("family attribution")),
            "{:?}",
            bundle.warnings
        );
    }

    /// The whole feature, end to end: two spouses under 50% each, with a spouse
    /// relationship on the request, produce a Schedule B-1 in the bundle they
    /// would not produce without it. A `partner` in these tests is already an
    /// individual, so both land in Part II.
    /// Every warning a return can produce has to read as a sentence.
    ///
    /// This drives the warning-producing paths across the whole return — the
    /// schedules, the elections, the asset register, the reconciliations — and
    /// puts each message through [`crate::tax::warning_shape`], which catches the
    /// line break or the run of spaces a mis-written string literal leaves in the
    /// middle of a message. That defect has reached this crate fifteen times; it
    /// survives review because the source looks right and every `contains` test
    /// still passes, and it only shows in the warnings panel.
    ///
    /// Driven through `build_return_inner` rather than asserted against a list of
    /// literals on purpose: a scan of the source cannot tell a swallowed
    /// continuation from a column of deliberately aligned CLI output, and the
    /// rendered string is where both forms of the defect look identical.
    #[test]
    fn every_warning_a_return_produces_reads_as_a_sentence() {
        use crate::domain::{BonusElection, DepreciableAsset, PropertyClass, Shares, System};
        use crate::tax::schedule_b::{ScheduleB, NO, YES};
        use crate::tax::warning_shape;

        fn asset(
            description: &str,
            class: PropertyClass,
            placed: NaiveDate,
            cost: i64,
        ) -> DepreciableAsset {
            DepreciableAsset {
                asset_id: description.to_lowercase(),
                description: description.into(),
                asset_account_id: "1500".into(),
                expense_account_id: "6500".into(),
                accumulated_account_id: "1590".into(),
                section_179_account_id: Some("6501".into()),
                acquired_on: placed,
                placed_in_service: placed,
                cost_cents: cost,
                class,
                system: System::Gds,
                section_179_cents: 0,
                bonus: BonusElection::Decline,
                disposed_on: None,
                notes: None,
            }
        }

        let mut all: Vec<String> = Vec::new();

        // 1. A bare return: no balance sheet, nothing on Schedule K, no answers.
        all.extend(
            build_return_inner(&two_partner_request(), &Default::default(), Vec::new())
                .unwrap()
                .warnings,
        );

        // 2. Question 2a Yes with nobody in the books over 50% — the schedule is
        //    declared and cannot be produced.
        let mut req = two_partner_request();
        let mut sb = ScheduleB::default();
        sb.set("b2a", YES);
        req.schedule_b = sb;
        all.extend(
            build_return_inner(&req, &Default::default(), Vec::new())
                .unwrap()
                .warnings,
        );

        // 3. Question 2b No over a partner who owns 60%, and a partner with no
        //    TIN, and profit and loss split on different percentages.
        let mut req = two_partner_request();
        let mut owner = partner("Dana", PartnerType::General, Residency::Domestic, 60.0);
        owner.shares = Shares::from_percents(60.0, 40.0, 60.0);
        req.partners = vec![PartnerFiling {
            partner: owner,
            tin: None,
        }];
        let mut sb = ScheduleB::default();
        sb.set("b2b", NO);
        // 4. …and question 31 Yes with a total that disagrees with the schedule.
        sb.set("b31", YES);
        sb.set("b31_total", "17");
        req.schedule_b = sb;
        all.extend(
            build_return_inner(&req, &Default::default(), Vec::new())
                .unwrap()
                .warnings,
        );

        // 5. The asset register: an unposted year, a §179 election on property
        //    that only conditionally allows one, bonus on property too long-lived
        //    to take it, the two 15-year classes colliding on line 19e, and an
        //    asset that came and went inside one year.
        let mut req = two_partner_request();
        let mut building = asset(
            "Studio building",
            PropertyClass::Nonresidential,
            NaiveDate::from_ymd_opt(FORM_TAX_YEAR, 4, 1).unwrap(),
            50_000_000,
        );
        building.section_179_cents = 1_000_000;
        building.bonus = BonusElection::Take;

        let mut fleeting = asset(
            "Borrowed press",
            PropertyClass::FiveYear,
            NaiveDate::from_ymd_opt(FORM_TAX_YEAR, 2, 1).unwrap(),
            400_000,
        );
        fleeting.disposed_on = Some(NaiveDate::from_ymd_opt(FORM_TAX_YEAR, 9, 1).unwrap());

        let mut lot = asset(
            "Parking lot",
            PropertyClass::FifteenYearLandImprovement,
            NaiveDate::from_ymd_opt(FORM_TAX_YEAR, 3, 1).unwrap(),
            1_000_000,
        );
        lot.section_179_cents = 200_000;

        req.assets = vec![
            building,
            fleeting,
            lot,
            asset(
                "Studio fit-out",
                PropertyClass::QualifiedImprovement,
                NaiveDate::from_ymd_opt(FORM_TAX_YEAR, 3, 1).unwrap(),
                1_000_000,
            ),
        ];
        let mut lines = crate::tax::lines::Form1065Lines::default();
        lines.set_for_test("l16a", 0);
        lines.set_for_test("k12", 0);
        all.extend(
            build_return_inner(&req, &lines, Vec::new())
                .unwrap()
                .warnings,
        );

        // 6. A balance sheet that *was* computed and has nothing mapped to it —
        //    a different warning from "nobody computed one", and reached only
        //    when the schedule is present and empty.
        let mut req = two_partner_request();
        req.schedule_l = Some(crate::tax::schedule_l::ScheduleL::default());
        all.extend(
            build_return_inner(&req, &Default::default(), Vec::new())
                .unwrap()
                .warnings,
        );

        // 7. The ledger path, which reaches the warnings the request-only path
        //    cannot: accounts carrying a balance that no tax line claims, and the
        //    balance sheet read from the books rather than handed in.
        let store = seeded_ledger();
        all.extend(
            build_return_from_ledger(store.connection(), &two_partner_request())
                .unwrap()
                .warnings,
        );

        // 8. A year whose percentages changed mid-year, which cannot be
        //    allocated on one split.
        let mut req = two_partner_request();
        let mid = day(FORM_TAX_YEAR, 7, 1);
        req.partners[0].partner.history = vec![
            crate::domain::SharePeriod {
                effective_from: day(2020, 1, 1),
                shares: crate::domain::Shares::from_percents(50.0, 50.0, 50.0),
            },
            crate::domain::SharePeriod {
                effective_from: mid,
                shares: crate::domain::Shares::from_percents(60.0, 60.0, 60.0),
            },
        ];
        req.partners[1].partner.history = vec![
            crate::domain::SharePeriod {
                effective_from: day(2020, 1, 1),
                shares: crate::domain::Shares::from_percents(50.0, 50.0, 50.0),
            },
            crate::domain::SharePeriod {
                effective_from: mid,
                shares: crate::domain::Shares::from_percents(40.0, 40.0, 40.0),
            },
        ];
        all.extend(
            build_return_inner(&req, &Default::default(), Vec::new())
                .unwrap()
                .warnings,
        );

        // The scenarios above have to have actually exercised the paths — a
        // handful of warnings would mean this passes by not reaching them.
        assert!(
            all.len() >= 20,
            "only {} warning(s) reached the check; the scenarios are not covering the \
             warning paths any more: {all:#?}",
            all.len()
        );
        warning_shape::assert_all(&all);
    }

    /// The mirror of the sole proprietor's case: absence is allowed when the
    /// profile is saved, and refused at the point a partnership return is built,
    /// because that is where which return is being filed is finally known.
    #[test]
    fn a_partnership_with_no_ein_is_told_before_it_files() {
        let mut req = two_partner_request();
        req.profile.ein = String::new();
        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).expect("a return");
        assert!(
            bundle.warnings.iter().any(|w| w.contains("no EIN")),
            "{:?}",
            bundle.warnings
        );

        // And a partnership that has one is not nagged about it.
        let bundle =
            build_return_inner(&two_partner_request(), &Default::default(), Vec::new()).unwrap();
        assert!(!bundle.warnings.iter().any(|w| w.contains("no EIN")));
    }

    /// The register reaches the bundle as a Form 4562, and the reconciliation
    /// notices that the year was never posted to the ledger — the state a return
    /// is most likely to be built in, and the one where it is quietly wrong.
    #[test]
    fn the_asset_register_produces_a_4562_and_an_unposted_year_is_reported() {
        use crate::domain::{BonusElection, DepreciableAsset, PropertyClass, System};

        let mut req = two_partner_request();
        req.assets = vec![DepreciableAsset {
            asset_id: "kiln".into(),
            description: "Kiln".into(),
            asset_account_id: "1500".into(),
            expense_account_id: "6500".into(),
            accumulated_account_id: "1590".into(),
            section_179_account_id: None,
            acquired_on: NaiveDate::from_ymd_opt(2025, 3, 1).unwrap(),
            placed_in_service: NaiveDate::from_ymd_opt(2025, 3, 1).unwrap(),
            cost_cents: 1_000_000,
            class: PropertyClass::SevenYear,
            system: System::Gds,
            section_179_cents: 0,
            bonus: BonusElection::Decline,
            disposed_on: None,
            notes: None,
        }];

        let mut lines = crate::tax::lines::Form1065Lines::default();
        // The books carry nothing on 16a, because the year was never posted.
        lines.set_for_test("l16a", 0);

        let before = build_return_inner(&two_partner_request(), &lines, Vec::new())
            .unwrap()
            .page_count;
        let bundle = build_return_inner(&req, &lines, Vec::new()).expect("a return");
        assert!(
            bundle.page_count > before,
            "the bundle gained no Form 4562: {} vs {before}",
            bundle.page_count
        );
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("line 16a") && w.contains("register computes")),
            "{:?}",
            bundle.warnings
        );
    }

    #[test]
    fn a_spouse_relationship_on_the_request_puts_both_partners_on_schedule_b1() {
        use crate::domain::{PartnerRelationship, RelationshipKind::Spouse, Shares};
        use crate::tax::schedule_b::{ScheduleB, YES};

        let mut req = two_partner_request();
        req.partners[0].partner.partner_id = "me".into();
        req.partners[0].partner.shares = Shares::from_percents(40.0, 40.0, 40.0);
        req.partners[1].partner.partner_id = "wife".into();
        req.partners[1].partner.shares = Shares::from_percents(20.0, 20.0, 20.0);

        let mut sb = ScheduleB::default();
        sb.set("b2b", YES);
        req.schedule_b = sb;

        // Without the relationship: 40 and 20, nobody at 50%, no B-1.
        let without = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        let doc = Document::load_mem(&without.pdf).unwrap();
        let text: String = doc
            .get_pages()
            .keys()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect();
        assert!(
            !text.contains("49842K"),
            "a B-1 was produced with no relationship on file"
        );

        // With it: each spouse owns 60%, both on the schedule.
        req.relationships = vec![PartnerRelationship::new("me", "wife", Spouse)];
        let with = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        let doc = Document::load_mem(&with.pdf).unwrap();
        assert!(
            with.page_count > without.page_count,
            "the spouse attribution added no Schedule B-1 page"
        );
        let text: String = doc
            .get_pages()
            .keys()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect();
        assert!(text.contains("49842K"), "Schedule B-1 is not in the bundle");
    }

    /// Declared on Schedule B but nobody in the books crosses the threshold. The
    /// two claims are different and the mismatch has to surface.
    #[test]
    fn a_declared_owner_the_books_do_not_have_is_reported_not_ignored() {
        use crate::tax::schedule_b::{ScheduleB, YES};
        let mut req = two_partner_request();
        // Both partners are at 50%… which is over the line. Push them under it.
        for f in &mut req.partners {
            f.partner.shares = crate::domain::Shares::from_percents(25.0, 25.0, 25.0);
        }
        let mut sb = ScheduleB::default();
        sb.set("b2b", YES);
        req.schedule_b = sb;

        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("no partner in the books owns 50%")),
            "{:?}",
            bundle.warnings
        );
    }

    /// A Yes on 31 produces Schedule B-2, and question 31's own figure has to
    /// agree with the schedule behind it.
    #[test]
    fn question_31_puts_schedule_b2_in_the_bundle_and_checks_its_total() {
        use crate::tax::schedule_b::{ScheduleB, YES};

        let mut req = two_partner_request();
        for f in &mut req.partners {
            f.partner.entity_type = "Individual".to_string();
        }
        let mut sb = ScheduleB::default();
        sb.set("b31", YES);
        sb.set("b31_total", "7"); // wrong on purpose: there are two partners
        req.schedule_b = sb;

        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("69658K"), "Schedule B-2 is not in the bundle");
        assert!(
            bundle.warnings.iter().any(|w| w.contains("have to agree")),
            "the mismatch must be reported: {:?}",
            bundle.warnings
        );
    }

    /// Each partner's share of line 18c travels behind their own K-1.
    ///
    /// Box 18 code C and item L row 4 both reach a partner as a bare figure, and
    /// row 4 is a row the instructions say to attach an explanation for. This is
    /// that explanation, and it has to name the partner it belongs to — two
    /// identical pages with different figures are not filable.
    #[test]
    fn each_partner_gets_the_statement_behind_their_own_k1() {
        use crate::tax::lines::LineDetail;
        use crate::tax::nondeductible::PartnerStatement;

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 140);

        let mut req = two_partner_request();
        let without = build_return_inner(&req, &lines, Vec::new()).unwrap();

        // 50/50, so $70 each and nothing for the cross-check to complain about.
        req.nondeductible = ["alice", "bob"]
            .iter()
            .zip(["Alice", "Bob"])
            .map(|(id, name)| PartnerStatement {
                partner_id: (*id).to_string(),
                partner_name: name.to_string(),
                rows: vec![LineDetail {
                    account_id: "3055".into(),
                    account_number: "3055".into(),
                    account_name: "Partner meals — 50% disallowed".into(),
                    cents: 70_00,
                }],
            })
            .collect();

        let with = build_return_inner(&req, &lines, Vec::new()).unwrap();
        assert_eq!(
            with.page_count,
            without.page_count + 2,
            "one statement page per partner"
        );

        let doc = Document::load_mem(&with.pdf).unwrap();
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("Partner: Alice"), "{text:?}");
        assert!(text.contains("Partner: Bob"), "{text:?}");
        assert!(
            !with
                .warnings
                .iter()
                .any(|w| w.contains("box 18 code C on that K-1")),
            "the shares agree with the box: {:?}",
            with.warnings
        );
    }

    /// **What the attachment list promises is an upper bound, not a count.**
    ///
    /// `attachments::required` tells a filer that each of the N partners gets
    /// their own page. A partner whose share of line 18c rounds to nothing gets
    /// no page — correctly, a statement of zeros is worse than none — so a
    /// two-partner return can come back with one partner page against a list
    /// that named two. Pinned here so the gap between the promise and the bundle
    /// is a known one.
    #[test]
    fn a_partner_allocated_nothing_gets_no_page_though_the_list_named_them() {
        use crate::tax::lines::LineDetail;
        use crate::tax::nondeductible::PartnerStatement;

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 140);

        let mut req = two_partner_request();
        let without = build_return_inner(&req, &lines, Vec::new()).unwrap();
        req.nondeductible = vec![
            PartnerStatement {
                partner_id: "alice".into(),
                partner_name: "Alice".into(),
                rows: vec![LineDetail {
                    account_id: "3055".into(),
                    account_number: "3055".into(),
                    account_name: "Partner meals — 50% disallowed".into(),
                    cents: 140_00,
                }],
            },
            // The whole line went to Alice; Bob's share was nothing.
            PartnerStatement {
                partner_id: "bob".into(),
                partner_name: "Bob".into(),
                rows: Vec::new(),
            },
        ];
        let with = build_return_inner(&req, &lines, Vec::new()).unwrap();
        assert_eq!(
            with.page_count,
            without.page_count + 1,
            "one page, not one per partner"
        );

        // And the list still says two, which is the overclaim.
        let mut answers = crate::tax::schedule_b::ScheduleB::default();
        answers.set("b4", crate::tax::schedule_b::YES);
        let listed = crate::tax::attachments::required(&answers, &lines, 2, false, 0);
        let entry = listed
            .iter()
            .find(|a| a.name == crate::tax::lines::NONDEDUCTIBLE_STATEMENT.name)
            .expect("line 18c obliges a statement");
        assert!(entry.because.contains("2 partner(s)"), "{}", entry.because);
    }

    /// The one way that page and the schedule in front of it can disagree. Both
    /// split the same figure on the same percentages, but over a year whose
    /// interests moved they weight the year's parts differently — so the two are
    /// compared rather than assumed equal.
    #[test]
    fn a_statement_that_contradicts_box_18c_is_reported() {
        use crate::tax::lines::LineDetail;
        use crate::tax::nondeductible::PartnerStatement;

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 140);

        let mut req = two_partner_request();
        // A statement saying $90 against a box that says $70.
        req.nondeductible = vec![PartnerStatement {
            partner_id: "alice".into(),
            partner_name: "Alice".into(),
            rows: vec![LineDetail {
                account_id: "3055".into(),
                account_number: "3055".into(),
                account_name: "Partner meals — 50% disallowed".into(),
                cents: 90_00,
            }],
        }];

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        let complaint = bundle
            .warnings
            .iter()
            .find(|w| w.contains("box 18 code C on that K-1"))
            .unwrap_or_else(|| panic!("no mismatch reported: {:?}", bundle.warnings));
        assert!(complaint.contains("Alice"), "{complaint}");
        assert!(complaint.contains("90"), "{complaint}");
        assert!(complaint.contains("70"), "{complaint}");
        crate::tax::warning_shape::assert_all(&bundle.warnings);
    }

    /// A negative row on a statement is rounding, and the return says so rather
    /// than leaving a reader to work out how an expense came back.
    #[test]
    fn a_negative_row_on_a_statement_is_explained_rather_than_left_to_puzzle() {
        use crate::tax::lines::LineDetail;
        use crate::tax::nondeductible::PartnerStatement;

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 2);

        let row = |number: &str, name: &str, cents: i64| LineDetail {
            account_id: number.into(),
            account_number: number.into(),
            account_name: name.into(),
            cents,
        };
        let mut req = two_partner_request();
        req.nondeductible = vec![
            PartnerStatement {
                partner_id: "alice".into(),
                partner_name: "Alice".into(),
                rows: vec![row("6100", "Meals", 2_00), row("6200", "Fines", -1_00)],
            },
            PartnerStatement {
                partner_id: "bob".into(),
                partner_name: "Bob".into(),
                rows: vec![row("6200", "Fines", 1_00)],
            },
        ];

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        let said = bundle
            .warnings
            .iter()
            .find(|w| w.contains("That is rounding, not a credit"))
            .unwrap_or_else(|| panic!("{:?}", bundle.warnings));
        assert!(said.starts_with("Alice"), "{said}");
        assert!(said.contains("6200"), "the row has to be named: {said}");
        assert!(said.contains("-1"), "and its figure: {said}");
        assert!(
            !bundle
                .warnings
                .iter()
                .any(|w| w.contains("box 18 code C on that K-1")),
            "the totals agree, negative row and all: {:?}",
            bundle.warnings
        );
        crate::tax::warning_shape::assert_all(&bundle.warnings);
    }

    /// A figure in box 18 code C with no statement behind it. Silence here read
    /// exactly like "this partnership had no nondeductible expenses", which is
    /// the one thing it does not mean.
    #[test]
    fn a_box_18c_figure_with_no_statement_is_reported() {
        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 140);

        let req = two_partner_request();
        assert!(req.nondeductible.is_empty(), "the case under test");

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        let missing: Vec<&String> = bundle
            .warnings
            .iter()
            .filter(|w| w.contains("no statement of what makes it up"))
            .collect();
        assert_eq!(missing.len(), 2, "one per partner: {:?}", bundle.warnings);
        assert!(missing.iter().any(|w| w.starts_with("Alice")));
        assert!(missing.iter().any(|w| w.contains("70")), "{missing:?}");
        crate::tax::warning_shape::assert_all(&bundle.warnings);
    }

    /// Two partners of the same name — a father and son, a trust named after its
    /// settlor. Their statements would otherwise be one page printed twice with
    /// different figures on it and nothing saying which K-1 either sits behind.
    #[test]
    fn two_partners_of_one_name_get_statements_that_can_be_told_apart() {
        use crate::tax::lines::LineDetail;
        use crate::tax::nondeductible::PartnerStatement;

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 140);

        let mut req = two_partner_request();
        req.partners[1].partner.name = req.partners[0].partner.name.clone();
        req.partners[0].tin = Some("123-45-6789".into());
        req.partners[1].tin = Some("987-65-4321".into());
        req.nondeductible = req
            .partners
            .iter()
            .map(|f| PartnerStatement {
                partner_id: f.partner.partner_id.clone(),
                partner_name: f.partner.name.clone(),
                rows: vec![LineDetail {
                    account_id: "3055".into(),
                    account_number: "3055".into(),
                    account_name: "Partner meals — 50% disallowed".into(),
                    cents: 70_00,
                }],
            })
            .collect();

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let pages: Vec<String> = doc
            .get_pages()
            .keys()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect();
        assert!(
            pages.iter().any(|p| p.contains("123-45-6789")),
            "the first partner's page names no identifier"
        );
        assert!(
            pages.iter().any(|p| p.contains("987-65-4321")),
            "nor the second's"
        );
    }

    /// And an ordinary partnership's pages are not cluttered with one.
    #[test]
    fn a_partner_whose_name_is_their_own_is_named_and_nothing_else() {
        use crate::tax::lines::LineDetail;
        use crate::tax::nondeductible::PartnerStatement;

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 140);
        let mut req = two_partner_request();
        req.nondeductible = req
            .partners
            .iter()
            .map(|f| PartnerStatement {
                partner_id: f.partner.partner_id.clone(),
                partner_name: f.partner.name.clone(),
                rows: vec![LineDetail {
                    account_id: "3055".into(),
                    account_number: "3055".into(),
                    account_name: "Partner meals — 50% disallowed".into(),
                    cents: 70_00,
                }],
            })
            .collect();

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let pages: Vec<String> = doc
            .get_pages()
            .keys()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect();
        let hers = pages
            .iter()
            .find(|p| p.contains("Partner: Alice"))
            .unwrap_or_else(|| panic!("no statement page for Alice"));
        assert!(
            !hers.contains("123-45-6789"),
            "an unambiguous name needs no identifier: {hers}"
        );
    }

    /// No Yes, no extra schedules — the common case must not gain pages.
    #[test]
    fn a_return_with_no_b_schedule_answers_gains_no_b_schedules() {
        let req = two_partner_request();
        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        // Matched on catalogue number, not on wording: Form 1065's own question
        // 2a says "Owning 50% or More" in the course of asking, so the phrase is
        // no evidence the schedule is attached.
        assert!(
            !text.contains("49842K"),
            "an unrequested Schedule B-1 was attached"
        );
        assert!(
            !text.contains("69658K"),
            "an unrequested Schedule B-2 was attached"
        );
    }

    /// The default: question 4 excuses L, M-1 and M-2, and they are completed
    /// regardless — because the exemption is about filing, not about whether the
    /// arithmetic is true.
    #[test]
    fn the_optional_schedules_are_completed_under_the_exemption_by_default() {
        use crate::tax::schedule_b::{ScheduleB, YES};

        let mut req = two_partner_request();
        let mut sb = ScheduleB::default();
        sb.set("b4", YES);
        req.schedule_b = sb;
        req.book_income_cents = 40_000_00;
        assert!(req.options.complete_optional_schedules, "on by default");

        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);

        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, "f6_126[0]").as_deref(),
            Some("40,000"),
            "M-1 line 1 should carry book income"
        );
        assert!(
            bundle.warnings.iter().any(|w| w.contains("not required")),
            "the exemption should be noted rather than acted on: {:?}",
            bundle.warnings
        );
    }

    /// Switched off, they are left blank — which is what the exemption permits,
    /// and the return says nothing is checking it.
    #[test]
    fn switching_the_option_off_leaves_the_optional_schedules_blank() {
        use crate::tax::schedule_b::{ScheduleB, YES};

        let mut req = two_partner_request();
        let mut sb = ScheduleB::default();
        sb.set("b4", YES);
        req.schedule_b = sb;
        req.book_income_cents = 40_000_00;
        req.options.complete_optional_schedules = false;

        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);

        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, "f6_126[0]"),
            None,
            "M-1 must be blank when the option is off"
        );
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("Nothing then checks")),
            "{:?}",
            bundle.warnings
        );
    }

    /// Question 4 unanswered or No means they are required, and the option is
    /// irrelevant — they get completed either way.
    #[test]
    fn the_option_cannot_skip_a_schedule_that_is_actually_required() {
        let mut req = two_partner_request();
        req.book_income_cents = 40_000_00;
        req.options.complete_optional_schedules = false;

        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, "f6_126[0]").as_deref(),
            Some("40,000"),
            "the option only applies where the exemption does"
        );
    }

    /// A Schedule L that was never computed used to produce the same blank page
    /// as one with nothing mapped, and said nothing either way.
    #[test]
    fn a_schedule_l_that_was_never_computed_says_so() {
        let mut req = two_partner_request();
        req.schedule_l = None;
        let bundle = build_return_inner(&req, &Default::default(), Vec::new()).unwrap();
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("no balance sheet was computed")),
            "{:?}",
            bundle.warnings
        );
    }

    /// End to end: a ledger with several accounts on line 21 produces a bundle
    /// that actually carries the statement page supporting it, itemising them,
    /// and totalling to the figure in the box.
    #[test]
    fn line_21_gets_a_statement_page_listing_what_is_in_it() {
        use crate::tax::lines::LineDetail;

        let mut req = two_partner_request();
        req.detail.insert(
            "l21",
            vec![
                LineDetail {
                    account_id: "a".into(),
                    account_number: "6100".into(),
                    account_name: "Advertising".into(),
                    cents: 1_200_00,
                },
                LineDetail {
                    account_id: "b".into(),
                    account_number: "6200".into(),
                    account_name: "Professional fees".into(),
                    cents: 3_400_00,
                },
                LineDetail {
                    account_id: "c".into(),
                    account_number: "6300".into(),
                    account_name: "Software subscriptions".into(),
                    cents: 900_00,
                },
            ],
        );
        let mut lines = crate::tax::lines::Form1065Lines::default();
        lines.set_for_test("l21", 5500);

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();

        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(
            text.contains("Advertising"),
            "statement page missing from the bundle"
        );
        assert!(text.contains("Professional fees"));
        assert!(text.contains("Software subscriptions"));
        assert!(
            text.contains("5,500"),
            "the statement must total to the box"
        );
    }

    /// A figure on line 21 with no detail behind it cannot be supported, and has
    /// to say so rather than ship an unsupported deduction quietly.
    #[test]
    fn a_line_21_figure_with_no_detail_warns_instead_of_going_quiet() {
        let req = two_partner_request();
        let mut lines = crate::tax::lines::Form1065Lines::default();
        lines.set_for_test("l21", 5500);

        let bundle = build_return_inner(&req, &lines, Vec::new()).unwrap();
        assert!(
            bundle.warnings.iter().any(|w| w.contains("statement")),
            "{:?}",
            bundle.warnings
        );
    }

    /// The invariant the whole allocation exists for: what the K-1s say the
    /// partners got must equal what Schedule K says the partnership had. A
    /// mismatch here is the first thing an examiner sees, and rounding each
    /// share independently produces one.
    #[test]
    fn the_k1_shares_add_back_to_the_schedule_k_totals() {
        use crate::tax::lines::Form1065Lines;

        // Thirds, which is where independent rounding loses a dollar.
        let mut req = two_partner_request();
        req.partners = vec![
            PartnerFiling {
                partner: partner("Alice", PartnerType::General, Residency::Domestic, 33.3333),
                tin: None,
            },
            PartnerFiling {
                partner: partner("Bob", PartnerType::General, Residency::Domestic, 33.3333),
                tin: None,
            },
            PartnerFiling {
                partner: partner("Carol", PartnerType::General, Residency::Domestic, 33.3334),
                tin: None,
            },
        ];

        let mut lines = Form1065Lines::default();
        // An income item and a loss item, both awkward to divide.
        lines.set_for_test("l1a", 100);
        lines.set_for_test("k5", 100);
        lines.set_for_test("k13a", -101);

        let filed: Vec<&PartnerFiling> = req.partners.iter().collect();
        let (shares, _) = split_across_partners(&lines, &filed, 2025, &[]);

        for key in ["k1", "k5", "k13a"] {
            let total: i64 = shares.iter().map(|s| s.get(key)).sum();
            assert_eq!(
                total,
                match key {
                    "k1" => lines.k_line_1(),
                    other => lines.get(other),
                },
                "the three K-1s do not add back to Schedule K line {key}"
            );
        }
    }

    /// Income travels on the profit share and losses on the loss share, per item
    /// and on the item's own sign — so one return can split two figures two ways.
    #[test]
    fn income_and_loss_items_travel_on_different_percentages() {
        use crate::domain::Shares;
        use crate::tax::lines::Form1065Lines;

        let mut a = partner("Alice", PartnerType::General, Residency::Domestic, 50.0);
        a.shares = Shares {
            profit_ppm: 100_000,
            loss_ppm: 900_000,
            capital_ppm: 500_000,
        };
        let mut b = partner("Bob", PartnerType::General, Residency::Domestic, 50.0);
        b.shares = Shares {
            profit_ppm: 900_000,
            loss_ppm: 100_000,
            capital_ppm: 500_000,
        };

        let filings = vec![
            PartnerFiling {
                partner: a,
                tin: None,
            },
            PartnerFiling {
                partner: b,
                tin: None,
            },
        ];
        let filed: Vec<&PartnerFiling> = filings.iter().collect();

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k5", 1000); // income
        lines.set_for_test("k10", -1000); // loss

        let (shares, warnings) = split_across_partners(&lines, &filed, 2025, &[]);
        assert_eq!(shares[0].get("k5"), 100, "Alice takes 10% of the income");
        assert_eq!(shares[1].get("k5"), 900);
        assert_eq!(shares[0].get("k10"), -900, "Alice takes 90% of the loss");
        assert_eq!(shares[1].get("k10"), -100);
        assert!(
            warnings.iter().any(|w| w.contains("differ")),
            "differing percentages must be called out: {warnings:?}"
        );
    }

    /// Schedule K line 1 is page one's line 23, not a second mappable figure.
    /// Two pages disagreeing about one number is the failure this prevents.
    #[test]
    fn schedule_k_line_1_is_page_ones_bottom_line() {
        use crate::tax::lines::Form1065Lines;
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 5000);
        lines.set_for_test("l13", 2000);
        assert_eq!(lines.k_line_1(), lines.line_23());
        assert_eq!(lines.k_line_1(), 3000);

        let mut doc = Document::load_mem(F1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        fill_schedule_k(&mut doc, &map, &lines).unwrap();
        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, sched_k::L1_ORDINARY).as_deref(),
            Some("3,000")
        );
    }

    /// A separately stated item must reach Schedule K and stay off page one's
    /// deductions — the double-deduction the catalogue is arranged to prevent.
    #[test]
    fn a_charitable_contribution_reaches_schedule_k_and_not_line_21() {
        use crate::tax::lines::Form1065Lines;
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 10_000);
        lines.set_for_test("k13a", 500);

        // Page one's total deductions are untouched by the contribution.
        assert_eq!(lines.line_22(), 0);
        assert_eq!(lines.line_23(), 10_000);

        let mut doc = Document::load_mem(F1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        fill_schedule_k(&mut doc, &map, &lines).unwrap();

        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "f5_22[0]").as_deref(),
            Some("500"),
            "13a cash contributions"
        );
        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, PAGE1_2025.lines.l21_other_deductions),
            None,
            "line 21 must stay empty"
        );
    }

    #[test]
    fn filing_a_year_the_bundled_forms_are_not_for_is_flagged() {
        // A year that is carried and mapped gets its own form, and says nothing
        // about the form it used.
        for blanks in FORM_YEARS.iter().filter(|f| f.mapped) {
            let req = ReturnRequest {
                year: blanks.year,
                ..two_partner_request()
            };
            let bundle = build_return(&req).unwrap();
            assert!(
                !bundle
                    .warnings
                    .iter()
                    .any(|w| w.contains("No Form 1065 is carried")),
                "{} is carried but was reported as missing: {:?}",
                blanks.year,
                bundle.warnings
            );
        }

        // A year that is not is refused, not filled on another year's blank.
        //
        // This used to fall back to the current revision with a warning, and the
        // result was a complete, plausible return whose page 1 and every K-1
        // carried the current year in pre-printed type — a form that says one
        // year and is filed as another, with the only evidence one line in a
        // list of a dozen warnings.
        let oldest = supported_years().first().copied().unwrap();
        let req = ReturnRequest {
            year: oldest - 1,
            ..two_partner_request()
        };
        match build_return(&req) {
            Err(FormError::NoFormForYear { year, .. }) => assert_eq!(year, oldest - 1),
            Err(e) => panic!("refused, but for the wrong reason: {e}"),
            Ok(b) => panic!(
                "a year with no blank was filled on another year's form, with {} warning(s)",
                b.warnings.len()
            ),
        }
    }

    /// The older forms take one address line, and it has to read like one.
    #[test]
    fn a_one_line_address_reads_like_an_address() {
        use crate::domain::Address;
        let mut a = Address {
            street: "4541 N Lincoln Ave".into(),
            suite: None,
            city: "Chicago".into(),
            state: "IL".into(),
            postal_code: "60625".into(),
            country: None,
        };
        assert_eq!(one_line_address(&a), "Chicago, IL 60625");
        a.country = Some("United States".into());
        assert_eq!(one_line_address(&a), "Chicago, IL, United States 60625");
        // Nothing entered leaves no stray punctuation behind.
        a.country = None;
        a.postal_code = String::new();
        assert_eq!(one_line_address(&a), "Chicago, IL");
        a.city = String::new();
        a.state = String::new();
        a.postal_code = "60625".into();
        assert_eq!(one_line_address(&a), "60625");
    }

    /// Every box on page one sits where its printed label says it does.
    ///
    /// # The two failures this exists to catch, both of which happened
    ///
    /// The header: on the 2023 form `f1_07[0]` is the *principal business
    /// activity* box, not the city. Every name resolved, every test passed, and
    /// the city, state, ZIP and business code came out blank on the paper while
    /// their values sat in boxes A and B.
    ///
    /// The income block: the 2023 and 2024 forms number gross receipts
    /// `f1_15[0]` where the 2025 form numbers it `f1_19[0]`, because their
    /// header uses four fewer boxes. Filled with the 2025 names, a 2023 return
    /// printed gross receipts on line 3, total deductions on "Other taxes", and
    /// the ordinary business income on **line 28, Total balance due**. Every one
    /// of those names exists on the 2023 form. Only the geometry said otherwise.
    #[test]
    fn every_page_one_box_sits_where_its_label_says() {
        for blanks in FORM_YEARS.iter().filter(|f| f.mapped) {
            let Some(page1) = blanks.page1 else { continue };
            let year = blanks.year;
            let doc = Document::load_mem(blanks.f1065).unwrap();
            let mut map = field_map(&doc);

            use lopdf::Object;
            let rect = |name: &str| -> (f64, f64) {
                let id = map
                    .find(name)
                    .unwrap_or_else(|| panic!("the {year} form has no {name}"));
                let d = doc.get_object(id).and_then(Object::as_dict).unwrap();
                let r = d.get(b"Rect").and_then(Object::as_array).unwrap();
                let num = |i: usize| {
                    r[i].as_float()
                        .map(f64::from)
                        .unwrap_or_else(|_| r[i].as_i64().unwrap() as f64)
                };
                (num(0), num(1))
            };

            // --- the header, by column ---
            //
            // A/B/C run down the left margin, the name and address occupy the
            // middle, and the EIN and date started sit on the right. A box that
            // has drifted between them is in the wrong place whatever it is called.
            for (name, what) in [
                (page1.principal_activity, "box A principal activity"),
                (page1.principal_product, "box B principal product"),
                (page1.naics, "box C NAICS code"),
            ] {
                let x = rect(name).0;
                assert!(
                    x < 80.0,
                    "on the {year} form the {what} box is at x={x:.0}, not in the left \
                     margin where its label is — it is pointing at another box"
                );
            }
            for (name, what) in [
                (page1.legal_name, "partnership name"),
                (page1.street, "street"),
                (page1.city, "city"),
            ] {
                let x = rect(name).0;
                assert!(
                    (100.0..420.0).contains(&x),
                    "on the {year} form the {what} box is at x={x:.0}, outside the column \
                     its label is in"
                );
            }
            for (name, what) in [(page1.ein, "EIN"), (page1.date_started, "date started")] {
                let x = rect(name).0;
                assert!(
                    x > 420.0,
                    "on the {year} form the {what} box is at x={x:.0}, not in the right \
                     column where its label is"
                );
            }

            // --- the income and deduction block, by row ---
            //
            // Each line's box must sit on the printed row that carries its
            // number, read off the page rather than tabulated here — a table
            // written here is one more thing that can disagree with the paper,
            // which is the whole failure being guarded against.
            let rows = printed_line_rows(blanks.f1065);
            assert!(
                rows.len() > 15,
                "{year}: read {} numbered rows; is pdftotext installed?",
                rows.len()
            );
            let l = &page1.lines;
            for (name, number) in [
                (l.l1a_gross_receipts, "1a"),
                (l.l2_cogs, "2"),
                (l.l3_gross_profit, "3"),
                (l.l4_other_partnerships, "4"),
                (l.l5_farm, "5"),
                (l.l6_form_4797, "6"),
                (l.l7_other_income, "7"),
                (l.l8_total_income, "8"),
                (l.l9_salaries, "9"),
                (l.l10_guaranteed, "10"),
                (l.l11_repairs, "11"),
                (l.l12_bad_debts, "12"),
                (l.l13_rent, "13"),
                (l.l14_taxes, "14"),
                (l.l15_interest, "15"),
                (l.l16a_depreciation, "16a"),
                (l.l17_depletion, "17"),
                (l.l18_retirement, "18"),
                (l.l19_benefits, "19"),
                (l.l20_energy, "20"),
                (l.l21_other_deductions, "21"),
                (l.l22_total_deductions, "22"),
                (l.l23_ordinary_income, "23"),
            ] {
                let y = rect(name).1;
                // A `Rect` names the bottom of the box; the printed number is
                // centred in the row, about six points above it.
                let printed: Vec<&str> = rows
                    .iter()
                    .filter(|(at, _)| (*at - (y + 6.0)).abs() < 5.0)
                    .map(|(_, n)| n.as_str())
                    .collect();
                assert!(
                    printed.contains(&number),
                    "on the {year} form line {number} is written to {name} at y={y:.0}, \
                     where the page prints line(s) {printed:?}"
                );
            }
        }
    }

    /// The line numbers printed down the left of page one, by the y they sit at.
    ///
    /// A line number is a token that starts with a digit in the narrow column the
    /// form reserves for it, which is what distinguishes "16a" the label from
    /// "16a" appearing inside another line's wording.
    fn printed_line_rows(bytes: &[u8]) -> Vec<(f64, String)> {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("accountir-1065-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f1065.pdf");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        let out = std::process::Command::new("pdftotext")
            .args(["-f", "1", "-l", "1", "-bbox-layout"])
            .arg(&path)
            .arg("-")
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(out) = out else { return Vec::new() };
        let text = String::from_utf8_lossy(&out.stdout);
        let height: f64 = text
            .split("height=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(792.0);

        let mut rows = Vec::new();
        for chunk in text.split("<word ").skip(1) {
            let num = |key: &str| -> Option<f64> {
                chunk
                    .split(key)
                    .nth(1)?
                    .split('"')
                    .next()?
                    .parse::<f64>()
                    .ok()
            };
            let (Some(x_min), Some(y_min), Some(y_max)) =
                (num("xMin=\""), num("yMin=\""), num("yMax=\""))
            else {
                continue;
            };
            let Some(word) = chunk.split('>').nth(1).and_then(|s| s.split('<').next()) else {
                continue;
            };
            // The number column. The form reserves the left margin for it and
            // right-aligns, so a two-character number starts a few points left
            // of a one-character one — 55 to 70 covers both on every revision.
            // Anything further right is a number inside a sentence.
            if !(50.0..75.0).contains(&x_min) {
                continue;
            }
            let w = word.trim();
            if w.len() > 3 || !w.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            rows.push((height - (y_min + y_max) / 2.0, w.to_string()));
        }
        rows
    }

    /// No two Schedule B questions may resolve to the same box.
    ///
    /// The failure this catches, found the hard way: aliasing a prior year's
    /// yes/no rows into place is not enough on its own, because the questions
    /// that are *not* yes/no — a lone "check this box", a "how many Forms 8865"
    /// — sit outside those columns and shift with the page just the same. Leave
    /// one un-aliased and it lands on a neighbour's box. Both halves of a yes/no
    /// pair then come back ticked, which on a filed return is an answer nobody
    /// gave.
    #[test]
    fn no_two_schedule_b_questions_share_a_box() {
        use crate::tax::schedule_b::Control;
        for blanks in FORM_YEARS.iter() {
            let Some(table) = blanks.schedule_b else {
                continue;
            };
            let doc = Document::load_mem(blanks.f1065).unwrap();
            let mut map = field_map(&doc);
            let mut owner: std::collections::HashMap<lopdf::ObjectId, &str> =
                std::collections::HashMap::new();
            for q in table {
                let boxes: Vec<&str> = match &q.control {
                    Control::YesNo { yes, no } => vec![yes, no],
                    Control::Choice(opts) => opts.iter().map(|o| o.field).collect(),
                    Control::Check { field } | Control::Entry { field, .. } => vec![field],
                };
                for b in boxes {
                    let Some(id) = map.find(b) else { continue };
                    if let Some(previous) = owner.insert(id, q.key) {
                        assert_eq!(
                            previous, q.key,
                            "on the {} form, questions {previous} and {} both write {b}",
                            blanks.year, q.key
                        );
                    }
                }
            }
        }
    }

    /// Every Schedule B answer box sits on the row whose number the table claims.
    ///
    /// The yes and no columns are at fixed x on every revision, and the question
    /// number is printed in the left margin — so a box's row can be read off the
    /// page and compared with what the table says it is. This is the check that
    /// makes a per-revision table trustworthy without deriving it from another
    /// year's: each one is asserted against its own PDF.
    #[test]
    fn every_schedule_b_box_sits_on_the_row_its_number_is_printed_on() {
        use crate::tax::schedule_b::Control;
        for blanks in FORM_YEARS {
            let Some(table) = blanks.schedule_b else {
                continue;
            };
            let year = blanks.year;
            let doc = Document::load_mem(blanks.f1065).unwrap();
            let map = field_map(&doc);

            // The margin as it reads: numbers in one column, sub-letters just
            // right of them, walked top to bottom. A number sets the current
            // question; a letter sets its part. That is how a person reads the
            // page, and it is the only way to tell question 2's "a" from
            // question 3's.
            let mut printed: Vec<(u8, f64, String)> = Vec::new();
            for page in 2u8..=4 {
                let mut margin: Vec<(f64, bool, String)> = Vec::new();
                for (y, x, word) in page_words(blanks.f1065, page) {
                    if (40.0..=48.0).contains(&x)
                        && word.len() <= 2
                        && word.chars().all(|c| c.is_ascii_digit())
                    {
                        margin.push((y, true, word));
                    } else if (48.0..=56.0).contains(&x)
                        && word.len() == 1
                        && word.chars().all(|c| c.is_ascii_lowercase())
                    {
                        margin.push((y, false, word));
                    }
                }
                margin.sort_by(|a, b| b.0.total_cmp(&a.0));
                let mut number = String::new();
                for (y, is_number, word) in margin {
                    if is_number {
                        number = word;
                        printed.push((page, y, number.clone()));
                    } else if !number.is_empty() {
                        printed.push((page, y, format!("{number}{word}")));
                    }
                }
            }

            for q in table {
                let Control::YesNo { yes, .. } = q.control else {
                    continue;
                };
                let Some(id) = map.find(yes) else { continue };
                // Which page the box is on, from its qualified name — the printed
                // numbers restart on each page, so a row can only be matched
                // against its own.
                let suffix = format!(".{yes}");
                let Some(page) = map
                    .names()
                    .find(|n| n.ends_with(&suffix) || *n == yes)
                    .and_then(|n| n.split(".Page").nth(1))
                    .and_then(|n| n.split('[').next())
                    .and_then(|n| n.parse::<u8>().ok())
                else {
                    continue;
                };
                let d = doc.get_object(id).and_then(lopdf::Object::as_dict).unwrap();
                let r = d.get(b"Rect").and_then(lopdf::Object::as_array).unwrap();
                let y = r[1]
                    .as_float()
                    .map(f64::from)
                    .unwrap_or_else(|_| r[1].as_i64().unwrap() as f64);

                // The number for a row is the nearest one printed at or above it,
                // because a question's number sits on its first line and its
                // checkboxes on its last.
                let nearest = printed
                    .iter()
                    .filter(|(pg, py, _)| *pg == page && *py >= y - 3.0)
                    .min_by(|a, b| a.1.total_cmp(&b.1))
                    .map(|(_, _, n)| n.as_str());

                if let Some(found) = nearest {
                    assert!(
                        found.starts_with(q.number) || q.number.starts_with(found),
                        "on the {year} form question {} is written to {yes}, which sits on the \
                         row the page numbers {found}",
                        q.number
                    );
                }
            }
        }
    }

    /// Every word on a page, as `(y from the bottom, x, text)`.
    fn page_words(bytes: &[u8], page: u8) -> Vec<(f64, f64, String)> {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("accountir-sb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f1065.pdf");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
        let out = std::process::Command::new("pdftotext")
            .args([
                "-f",
                &page.to_string(),
                "-l",
                &page.to_string(),
                "-bbox-layout",
            ])
            .arg(&path)
            .arg("-")
            .output();
        let _ = std::fs::remove_dir_all(&dir);
        let Ok(out) = out else { return Vec::new() };
        let text = String::from_utf8_lossy(&out.stdout);
        let height: f64 = text
            .split("height=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(792.0);
        let mut out = Vec::new();
        for chunk in text.split("<word ").skip(1) {
            let num = |key: &str| -> Option<f64> {
                chunk
                    .split(key)
                    .nth(1)?
                    .split('"')
                    .next()?
                    .parse::<f64>()
                    .ok()
            };
            let (Some(x), Some(y0), Some(y1)) = (num("xMin=\""), num("yMin=\""), num("yMax=\""))
            else {
                continue;
            };
            let Some(w) = chunk.split('>').nth(1).and_then(|s| s.split('<').next()) else {
                continue;
            };
            if w.trim().is_empty() {
                continue;
            }
            out.push((height - (y0 + y1) / 2.0, x, w.trim().to_string()));
        }
        out
    }

    /// A draft may not be filed, and a page that does not say so invites
    /// somebody to post it to the IRS.
    #[test]
    fn a_draft_year_says_it_is_a_projection_and_not_a_return() {
        let draft = FORM_YEARS
            .iter()
            .find(|f| f.draft && f.mapped)
            .map(|f| f.year);
        let Some(year) = draft else {
            // Every draft carried is currently unmapped, which
            // `an_unmapped_revision_is_refused_rather_than_half_filled` covers.
            return;
        };
        let bundle = build_return(&ReturnRequest {
            year,
            ..two_partner_request()
        })
        .unwrap();
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("draft") && w.contains("may not be filed")),
            "got {:?}",
            bundle.warnings
        );
    }

    /// The trap this whole per-year apparatus exists for: on a re-paginated
    /// revision every field name still resolves, so a build that went ahead
    /// would fill real boxes on the wrong schedules and foot perfectly.
    #[test]
    fn an_unmapped_revision_is_refused_rather_than_half_filled() {
        for blanks in FORM_YEARS.iter().filter(|f| !f.mapped) {
            let msg = match build_return(&ReturnRequest {
                year: blanks.year,
                ..two_partner_request()
            }) {
                Err(e) => e.to_string(),
                Ok(_) => panic!(
                    "the {} revision is not mapped but produced a form anyway",
                    blanks.year
                ),
            };
            assert!(msg.contains("wrong schedule"), "{msg}");
            assert!(msg.contains("preview"), "no route to the figures: {msg}");
        }
    }

    /// The fallback index has to point at the current year's blanks, or a year
    /// nothing is carried for silently gets some other year's form.
    #[test]
    fn the_fallback_form_is_the_current_years() {
        assert_eq!(FORM_YEARS[CURRENT_FORM_INDEX].year, FORM_TAX_YEAR);
    }

    #[test]
    fn thirds_reach_the_form_with_their_digits_intact() {
        let mut req = two_partner_request();
        req.partners[0].partner.shares = Shares::from_percents(33.3333, 33.3333, 33.3333);

        let bundle = build_return(&req).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, &k1_namespace(1), k1::PROFIT_END),
            Some("33.3333".into())
        );
        assert_eq!(
            crate::domain::FULL_SHARE,
            1_000_000,
            "the unit shares are held in"
        );
    }

    // --- item L, the partner's capital account ------------------------------

    /// The names above were matched to the printed labels by rectangle, on every
    /// revision carried. This is the standing check that they are still there —
    /// and that no revision needed an alias for them, which is the claim the
    /// comment in `mod k1` makes and the one that would rot silently.
    ///
    /// Run over the 2026 draft too, unlike `check_year`. The draft re-paginates
    /// the 1065 and moves the K-1 onto page 2 of its own file, so its `f5_*`
    /// names mean something else — but item L's six boxes are not on the 1065 at
    /// all, and the whole point of a geometric match is that it survives a
    /// repagination that a name-based one would not notice.
    #[test]
    fn item_l_is_the_same_six_boxes_on_every_revision_carried() {
        for blanks in FORM_YEARS {
            let year = blanks.year;
            let sched = Document::load_mem(blanks.sk1).unwrap();
            let map = field_map(&sched);
            let rows = [
                (k1::L_BEGIN, "beginning capital account"),
                (k1::L_CONTRIBUTED, "capital contributed"),
                (k1::L_NET_INCOME, "current year net income"),
                (k1::L_OTHER, "other increase (decrease)"),
                (k1::L_WITHDRAWN, "withdrawals and distributions"),
                (k1::L_ENDING, "ending capital account"),
            ];
            for (name, _) in rows {
                assert!(
                    map.find(name).is_some(),
                    "the {year} Schedule K-1 has no item L box {name}"
                );
            }

            // And they are still the boxes in that order. Existence alone would
            // pass with row 4 pointing at row 3's box on a repaginated revision,
            // and now that row 4 carries a figure that mistake is a capital
            // account off by that partner's nondeductible expenses — on a page
            // that would still foot, because `ending()` is computed and not read
            // back off the form. Item L prints top to bottom, so the boxes have
            // to descend.
            use lopdf::Object;
            let top = |name: &str| -> f64 {
                let id = map.find(name).expect("checked above");
                let d = sched.get_object(id).and_then(Object::as_dict).unwrap();
                let r = d.get(b"Rect").and_then(Object::as_array).unwrap();
                // The higher of the two y coordinates: a box's top edge.
                let num = |i: usize| {
                    r[i].as_float()
                        .map(f64::from)
                        .unwrap_or_else(|_| r[i].as_i64().unwrap() as f64)
                };
                num(1).max(num(3))
            };
            for pair in rows.windows(2) {
                let (above, what_above) = pair[0];
                let (below, what_below) = pair[1];
                assert!(
                    top(above) > top(below),
                    "on the {year} Schedule K-1, {what_above} ({above}, y={:.0}) is not above                      {what_below} ({below}, y={:.0}) — one of them is pointing at the other's                      row",
                    top(above),
                    top(below)
                );
            }
        }
    }

    /// A ledger with two partners' capital in four accounts, and the entries that
    /// make each row of item L a different number — so a row written into its
    /// neighbour's box is visible rather than hidden behind two equal figures.
    fn ledger_with_capital_accounts() -> (crate::store::event_store::EventStore, String, String) {
        use crate::commands::partnership_commands as pc;
        use crate::events::types::{Event, EventAccountType, EventEnvelope, JournalLineData};
        use crate::store::projections::ProjectionStore;

        let mut store = seeded_ledger();
        map_seeded_accounts(store.connection());
        pc::set_profile(&mut store, "u", &profile()).unwrap();

        for (id, number, name) in [
            ("alice-in", "4002", "Alice contributions"),
            ("alice-out", "4005", "Alice draws"),
            ("bob-in", "4003", "Bob contributions"),
            ("bob-out", "4006", "Bob draws"),
        ] {
            let e = Event::AccountCreated {
                account_id: id.into(),
                account_type: EventAccountType::Equity,
                account_number: number.into(),
                name: name.into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            };
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        }

        let mut admit = |name: &str| {
            let who = pc::AdmitPartner {
                name: name.into(),
                partner_type: PartnerType::General,
                residency: Residency::Domestic,
                entity_type: "Individual".into(),
                address: Address {
                    street: "2 Other Road".into(),
                    suite: None,
                    city: "Cape Town".into(),
                    state: "WC".into(),
                    postal_code: "8001".into(),
                    country: None,
                },
                start_date: Some(day(2021, 7, 1)),
                shares: Shares::from_percents(50.0, 50.0, 50.0),
                tin: None,
            };
            pc::admit_partner(&mut store, "u", &who).unwrap().0
        };
        let alice = admit("Alice");
        let bob = admit("Bob");

        for (partner, account, role) in [
            (&alice, "alice-in", "contribution"),
            (&alice, "alice-out", "draw"),
            (&bob, "bob-in", "contribution"),
            (&bob, "bob-out", "draw"),
        ] {
            pc::link_equity_account(&mut store, "u", partner, account, role).unwrap();
        }

        let mut post = |id: &str, on: NaiveDate, pairs: &[(&str, i64)]| {
            let lines: Vec<JournalLineData> = pairs
                .iter()
                .enumerate()
                .map(|(i, (acct, amount))| JournalLineData {
                    line_id: format!("{id}-{i}"),
                    account_id: (*acct).into(),
                    amount: *amount,
                    currency: "USD".into(),
                    exchange_rate: None,
                    memo: None,
                })
                .collect();
            let e = Event::JournalEntryPosted {
                entry_id: id.into(),
                date: on,
                memo: "capital".into(),
                lines,
                reference: None,
                source: None,
            };
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        };

        // Alice: $20,000 in before the year, $3,000 in during it, $700 out.
        post(
            "a-prior",
            day(FORM_TAX_YEAR - 1, 5, 1),
            &[("cash", 2_000_000), ("alice-in", -2_000_000)],
        );
        post(
            "a-in",
            day(FORM_TAX_YEAR, 3, 1),
            &[("cash", 300_000), ("alice-in", -300_000)],
        );
        post(
            "a-out",
            day(FORM_TAX_YEAR, 11, 1),
            &[("alice-out", 70_000), ("cash", -70_000)],
        );

        (store, alice, bob)
    }

    /// The end-to-end check: figures posted to a partner's equity accounts reach
    /// their K-1's item L, in the right rows, and the column on the paper adds up
    /// the way the form says it does.
    #[test]
    fn item_l_reaches_the_k1_in_the_right_rows_and_the_column_foots() {
        let (store, _alice, _bob) = ledger_with_capital_accounts();
        let partners: Vec<PartnerFiling> =
            crate::commands::partnership_commands::list_partners(store.connection())
                .into_iter()
                .map(|partner| PartnerFiling { partner, tin: None })
                .collect();
        let alice_first = partners[0].partner.name == "Alice";
        assert!(
            alice_first,
            "the assertions below read the first K-1 as Alice's"
        );

        let req = ReturnRequest {
            partners,
            ..two_partner_request()
        };
        let bundle = build_return_from_ledger(store.connection(), &req).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);

        let get = |field: &str| -> i64 {
            acroform::get_value_in(&doc, &map, &k1_namespace(1), field)
                .unwrap_or_else(|| panic!("{field} is not on the first K-1"))
                .replace(',', "")
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("{field} is not a number"))
        };

        assert_eq!(get(k1::L_BEGIN), 20_000, "the prior year's contribution");
        assert_eq!(get(k1::L_CONTRIBUTED), 3_000);
        assert_eq!(get(k1::L_OTHER), 0);
        assert_eq!(
            get(k1::L_WITHDRAWN),
            700,
            "a magnitude: the box's parentheses are printed on the form"
        );
        assert_eq!(
            get(k1::L_ENDING),
            get(k1::L_BEGIN) + get(k1::L_CONTRIBUTED) + get(k1::L_NET_INCOME) + get(k1::L_OTHER)
                - get(k1::L_WITHDRAWN),
            "the six rows have to add up as printed"
        );

        // Row 3 is the partner's share of the same figure Part III box 1 comes
        // from, so a preparer reading down one K-1 sees one partnership.
        let ordinary: i64 = acroform::get_value_in(&doc, &map, &k1_namespace(1), "f1_34[0]")
            .unwrap()
            .replace(',', "")
            .parse()
            .unwrap();
        assert_eq!(
            get(k1::L_NET_INCOME),
            ordinary,
            "on books whose only Schedule K figure is ordinary income, the two agree"
        );
    }

    /// A ledger with a half-deductible meals account, revenue in both halves of
    /// the year, and the meal itself in the second half only.
    ///
    /// `moves_mid_year` sets Alice from a half to nine tenths on 1 July, which
    /// is what makes this a §706(d) year. Everything else is identical between
    /// the two, so the only thing a difference in the output can be attributed
    /// to is the change of interest.
    fn meals_ledger(moves_mid_year: bool) -> crate::store::event_store::EventStore {
        use crate::commands::partnership_commands as pc;
        use crate::commands::share_period_commands as spc;
        use crate::commands::tax_setup_commands as tsc;
        use crate::events::types::{Event, EventAccountType, EventEnvelope, JournalLineData};
        use crate::store::event_store::EventStore;
        use crate::store::projections::ProjectionStore;

        let mut store = EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        for (id, ty, number, name) in [
            ("cash", EventAccountType::Asset, "1000", "Cash"),
            ("sales", EventAccountType::Revenue, "4000", "Sales"),
            ("meals", EventAccountType::Expense, "3055", "Partner meals"),
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
        pc::set_profile(&mut store, "u", &profile()).unwrap();
        crate::tax::lines::set_account_line(store.connection(), "sales", "l1a", 0).unwrap();
        crate::tax::lines::set_account_line(store.connection(), "meals", "l21", 0).unwrap();
        tsc::set_deduction_limit(&mut store, "u", "meals", 50, FORM_TAX_YEAR).unwrap();

        let mut post = |id: &str, on: NaiveDate, pairs: &[(&str, i64)]| {
            let lines: Vec<JournalLineData> = pairs
                .iter()
                .enumerate()
                .map(|(i, (acct, amount))| JournalLineData {
                    line_id: format!("{id}-{i}"),
                    account_id: (*acct).into(),
                    amount: *amount,
                    currency: "USD".into(),
                    exchange_rate: None,
                    memo: None,
                })
                .collect();
            let e = Event::JournalEntryPosted {
                entry_id: id.into(),
                date: on,
                memo: "seed".into(),
                lines,
                reference: None,
                source: None,
            };
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        };
        // Half the year's revenue either side of 1 July, so the two halves carry
        // the same weight on the whole of Schedule K...
        post(
            "h1",
            day(FORM_TAX_YEAR, 3, 1),
            &[("cash", 100_000_00), ("sales", -100_000_00)],
        );
        post(
            "h2",
            day(FORM_TAX_YEAR, 9, 1),
            &[("cash", 100_000_00), ("sales", -100_000_00)],
        );
        // ...and the whole of the meal in the second half, so line 18c's own
        // weighting is nothing like it.
        post(
            "meal",
            day(FORM_TAX_YEAR, 9, 2),
            &[("meals", 400_00), ("cash", -400_00)],
        );

        for name in ["Alice", "Bob"] {
            let who = pc::AdmitPartner {
                name: name.into(),
                partner_type: PartnerType::General,
                residency: Residency::Domestic,
                entity_type: "Individual".into(),
                address: Address {
                    street: "2 Other Road".into(),
                    suite: None,
                    city: "Cape Town".into(),
                    state: "WC".into(),
                    postal_code: "8001".into(),
                    country: None,
                },
                start_date: Some(day(2021, 7, 1)),
                shares: Shares::from_percents(50.0, 50.0, 50.0),
                tin: None,
            };
            pc::admit_partner(&mut store, "u", &who).unwrap();
        }
        if moves_mid_year {
            for (name, pct) in [("Alice", 90.0), ("Bob", 10.0)] {
                let id = spc::list_partners_with_history(store.connection())
                    .into_iter()
                    .find(|p| p.name == name)
                    .expect("admitted above")
                    .partner_id;
                spc::set_partner_shares(
                    &mut store,
                    "u",
                    &id,
                    day(FORM_TAX_YEAR, 7, 1),
                    Shares::from_percents(pct, pct, pct),
                )
                .unwrap();
            }
        }
        store
    }

    fn meals_request(store: &crate::store::event_store::EventStore) -> ReturnRequest {
        let partners: Vec<PartnerFiling> =
            crate::commands::share_period_commands::list_partners_with_history(store.connection())
                .into_iter()
                .map(|partner| PartnerFiling { partner, tin: None })
                .collect();
        ReturnRequest {
            partners,
            ..two_partner_request()
        }
    }

    /// **The §706(d) conflict, now closed.** Box 18 code C is weighted by what
    /// line 18c itself carried in each part of the year. Item L row 4 and the
    /// statement behind the K-1 once followed the whole of Schedule K instead,
    /// and on this ledger — half the revenue either side of 1 July, the whole
    /// meal after it, and the interests moving on the same day — that put a
    /// statement of about 140 and an item L row 4 of −140 on the same page as a
    /// box of 180. Not a rounding artefact: 20% of the line, unbounded.
    ///
    /// All three are weighted by line 18c now, so they agree by construction and
    /// this books' worst case is the proof.
    #[test]
    fn a_segmented_year_splits_row_four_the_box_and_the_statement_alike() {
        let store = meals_ledger(true);
        let req = meals_request(&store);
        let bundle = build_return_from_ledger(store.connection(), &req).unwrap();

        assert!(
            !bundle
                .warnings
                .iter()
                .any(|w| w.contains("box 18 code C on that K-1")),
            "nothing is left to disagree about: {:?}",
            bundle.warnings
        );

        // Schedule K line 18c is $200: half of a $400 meal, all of it bought in
        // the segment Alice held nine tenths of.
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        let money = |which: usize, field: &str| -> i64 {
            acroform::get_value_in(&doc, &map, &k1_namespace(which), field)
                .unwrap_or_else(|| panic!("{field} is not on K-1 {which}"))
                .replace(',', "")
                .parse()
                .unwrap_or_else(|_| panic!("{field} on K-1 {which} is not a number"))
        };
        assert_eq!(money(1, "f1_88[0]"), 180, "Alice's box 18 code C");
        assert_eq!(money(1, k1::L_OTHER), -180, "and her item L row 4");
        assert_eq!(money(2, "f1_88[0]"), 20, "Bob's box 18 code C");
        assert_eq!(money(2, k1::L_OTHER), -20);

        // And her statement, which is the page explaining both of them. Read per
        // page, because "180" appears all over a return and what matters is that
        // it is on *her* page.
        let pages: Vec<String> = doc
            .get_pages()
            .keys()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect();
        let hers = pages
            .iter()
            .find(|p| p.contains("Partner: Alice"))
            .unwrap_or_else(|| panic!("no statement page for Alice"));
        assert!(
            hers.contains("180"),
            "her statement says something else: {hers}"
        );
        crate::tax::warning_shape::assert_all(&bundle.warnings);
    }

    /// And on a settled year the same books produce no such complaint. Without
    /// this the test above passes just as well against a check that always
    /// fires, which would put a false contradiction on every return that has a
    /// meal on it.
    #[test]
    fn a_settled_year_never_reports_the_statement_disagreeing_with_box_18c() {
        let store = meals_ledger(false);
        let req = meals_request(&store);
        let bundle = build_return_from_ledger(store.connection(), &req).unwrap();

        assert!(
            !bundle
                .warnings
                .iter()
                .any(|w| w.contains("box 18 code C on that K-1")),
            "{:?}",
            bundle.warnings
        );
        crate::tax::warning_shape::assert_all(&bundle.warnings);
    }

    /// The whole chain on one set of books: a meal in the ledger, half of it
    /// disallowed, reaching Schedule K line 18c, each partner's box, each
    /// partner's item L row 4 as a decrease, and a statement page behind each
    /// K-1 — all of them the same figure, split the same way.
    #[test]
    fn a_meal_in_the_ledger_reaches_every_place_line_18c_belongs() {
        let store = meals_ledger(false);
        let req = meals_request(&store);
        let bundle = build_return_from_ledger(store.connection(), &req).unwrap();

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value(&doc, &map, "f5_49[0]").as_deref(),
            Some("200"),
            "Schedule K line 18c"
        );
        // Read off the paper rather than out of the struct: what is filed is the
        // paper, and a figure that never reached a box is the failure.
        let money = |which: usize, field: &str| -> i64 {
            acroform::get_value_in(&doc, &map, &k1_namespace(which), field)
                .unwrap_or_else(|| panic!("{field} is not on K-1 {which}"))
                .replace(',', "")
                .parse()
                .unwrap_or_else(|_| panic!("{field} on K-1 {which} is not a number"))
        };
        let mut row_four = 0i64;
        for k1 in 1..=2 {
            assert_eq!(
                money(k1, k1::L_OTHER),
                -100,
                "item L row 4 on K-1 {k1} is a decrease of half of the line"
            );
            row_four += money(k1, k1::L_OTHER);
            assert_eq!(
                money(k1, k1::L_ENDING),
                money(k1, k1::L_BEGIN)
                    + money(k1, k1::L_CONTRIBUTED)
                    + money(k1, k1::L_NET_INCOME)
                    + money(k1, k1::L_OTHER)
                    - money(k1, k1::L_WITHDRAWN),
                "item L on K-1 {k1} does not foot as printed"
            );
            assert_eq!(
                money(k1, "f1_88[0]"),
                100,
                "box 18 code C on K-1 {k1}, positive as the form prints an expense"
            );
            assert_eq!(
                acroform::get_value_in(&doc, &map, &k1_namespace(k1), "f1_87[0]").as_deref(),
                Some("C"),
                "and it is coded C"
            );
        }
        assert_eq!(-row_four, 200, "the two rows are the whole of line 18c");

        // And the pages behind them.
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("Partner: Alice"), "{text:?}");
        assert!(text.contains("Partner: Bob"), "{text:?}");
        assert!(
            text.contains("50% disallowed"),
            "the statement names the part of the account it is about: {text:?}"
        );
    }

    /// A partner admitted halfway through the year gets a K-1, so they get a
    /// statement; one who left before it started gets neither. Allocating over a
    /// partner with no K-1 in the bundle is the failure this guards: the shares
    /// would still foot and one partner's page would be missing.
    #[test]
    fn the_statement_set_is_exactly_the_k1_set() {
        use crate::commands::partnership_commands as pc;

        let store = meals_ledger(false);
        let mut partners: Vec<Partner> =
            crate::commands::share_period_commands::list_partners_with_history(store.connection());
        // A third partner who left before the year began — a K-1 they must not
        // get, and a statement they must not get either.
        let gone = partner("Gone", PartnerType::General, Residency::Domestic, 0.0);
        let mut gone = gone;
        gone.start_date = day(2019, 1, 1);
        gone.end_date = Some(day(FORM_TAX_YEAR - 1, 12, 31));
        partners.push(gone);
        let filings: Vec<PartnerFiling> = partners
            .into_iter()
            .map(|partner| PartnerFiling { partner, tin: None })
            .collect();
        let req = ReturnRequest {
            partners: filings,
            ..two_partner_request()
        };
        let bundle = build_return_from_ledger(store.connection(), &req).unwrap();

        // The two sets the bundle is built from, taken the way it takes them.
        let components = {
            let (start, end) = pc::calendar_year(FORM_TAX_YEAR);
            let mapping =
                crate::tax::lines::load_effective_mapping(store.connection(), FORM_TAX_YEAR);
            let limits =
                crate::tax::lines::load_effective_limits(store.connection(), FORM_TAX_YEAR);
            let statement = crate::queries::reports::Reports::new(store.connection())
                .income_statement(start, end)
                .unwrap();
            crate::tax::lines::compute(&statement, &mapping, &limits)
                .detail
                .remove(crate::tax::lines::NONDEDUCTIBLE_LINE)
                .expect("the meal is on 18c")
        };
        let statements = crate::tax::nondeductible::for_return(
            store.connection(),
            FORM_TAX_YEAR,
            &req.partners,
            &components,
        );
        let ids: Vec<&str> = statements.iter().map(|s| s.partner_id.as_str()).collect();
        assert_eq!(
            ids.len(),
            2,
            "the departed partner must not be allocated anything: {ids:?}"
        );
        assert!(!ids.iter().any(|id| id.contains("gone")), "{ids:?}");
        assert_eq!(
            statements.iter().map(|s| s.total()).sum::<i64>(),
            200,
            "and the two who remain still carry the whole line: {statements:?}"
        );

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.contains("Partner: Gone"), "{text:?}");
    }

    /// A partner admitted halfway through the year gets a K-1, so they get a
    /// statement page too — and the three statements still come to the whole of
    /// line 18c. Allocating over a set the K-1s are not drawn over is the
    /// failure: the figures would foot and one partner's explanation would be
    /// missing from the bundle.
    #[test]
    fn a_partner_admitted_mid_year_gets_a_statement_like_everybody_else() {
        use crate::commands::partnership_commands as pc;
        use crate::commands::share_period_commands as spc;

        let mut store = meals_ledger(false);
        let who = pc::AdmitPartner {
            name: "Carol".into(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: "Individual".into(),
            address: Address {
                street: "2 Other Road".into(),
                suite: None,
                city: "Cape Town".into(),
                state: "WC".into(),
                postal_code: "8001".into(),
                country: None,
            },
            start_date: Some(day(FORM_TAX_YEAR, 7, 1)),
            shares: Shares::from_percents(20.0, 20.0, 20.0),
            tin: None,
        };
        pc::admit_partner(&mut store, "u", &who).unwrap();
        for name in ["Alice", "Bob"] {
            let id = spc::list_partners_with_history(store.connection())
                .into_iter()
                .find(|p| p.name == name)
                .unwrap()
                .partner_id;
            spc::set_partner_shares(
                &mut store,
                "u",
                &id,
                day(FORM_TAX_YEAR, 7, 1),
                Shares::from_percents(40.0, 40.0, 40.0),
            )
            .unwrap();
        }

        let req = meals_request(&store);
        assert_eq!(req.partners.len(), 3, "all three file");
        let bundle = build_return_from_ledger(store.connection(), &req).unwrap();

        let components = {
            let (start, end) = pc::calendar_year(FORM_TAX_YEAR);
            let mapping =
                crate::tax::lines::load_effective_mapping(store.connection(), FORM_TAX_YEAR);
            let limits =
                crate::tax::lines::load_effective_limits(store.connection(), FORM_TAX_YEAR);
            let statement = crate::queries::reports::Reports::new(store.connection())
                .income_statement(start, end)
                .unwrap();
            crate::tax::lines::compute(&statement, &mapping, &limits)
                .detail
                .remove(crate::tax::lines::NONDEDUCTIBLE_LINE)
                .expect("the meal is on 18c")
        };
        let statements = crate::tax::nondeductible::for_return(
            store.connection(),
            FORM_TAX_YEAR,
            &req.partners,
            &components,
        );
        assert_eq!(statements.len(), 3, "{statements:?}");
        assert_eq!(
            statements.iter().map(|s| s.total()).sum::<i64>(),
            200,
            "the three shares are still the whole line: {statements:?}"
        );

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        for name in ["Alice", "Bob", "Carol"] {
            assert!(
                text.contains(&format!("Partner: {name}")),
                "{name} has no statement page"
            );
        }
        crate::tax::warning_shape::assert_all(&bundle.warnings);
    }

    /// Nothing on line 18c is nothing everywhere: no statement page, no row 4,
    /// and — the one that is easy to get wrong — no warning saying a statement
    /// nobody produced disagrees with a box carrying nothing.
    #[test]
    fn no_nondeductible_expenses_produce_no_statement_and_no_complaint() {
        let store = seeded_ledger();
        map_seeded_accounts(store.connection());
        let bundle = build_return_from_ledger(store.connection(), &two_partner_request()).unwrap();

        assert!(
            !bundle
                .warnings
                .iter()
                .any(|w| w.contains("box 18 code C on that K-1")),
            "{:?}",
            bundle.warnings
        );
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        for k1 in 1..=2 {
            assert_eq!(
                acroform::get_value_in(&doc, &map, &k1_namespace(k1), k1::L_OTHER).as_deref(),
                Some("0"),
                "row 4 stays at nothing on K-1 {k1}"
            );
        }
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        let text: String = pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.contains("Partner:"), "{text:?}");
    }

    /// The identity-only path has no ledger, so item L is left blank and
    /// editable — the same treatment page one's figures get, and distinguishable
    /// from an item L computed as zero.
    #[test]
    fn item_l_is_blank_on_a_return_built_without_a_ledger() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        for field in [k1::L_BEGIN, k1::L_ENDING, k1::L_WITHDRAWN] {
            assert_eq!(
                acroform::get_value_in(&doc, &map, &k1_namespace(1), field),
                None,
                "{field} must be untouched when nothing computed a capital account"
            );
        }
    }

    /// The caveat about what a beginning capital account leaves out has to reach
    /// the person filing, not only the module that knows it.
    #[test]
    fn a_ledger_built_return_carries_the_capital_account_caveat() {
        let (store, _alice, _bob) = ledger_with_capital_accounts();
        let partners: Vec<PartnerFiling> =
            crate::commands::partnership_commands::list_partners(store.connection())
                .into_iter()
                .map(|partner| PartnerFiling { partner, tin: None })
                .collect();
        let req = ReturnRequest {
            partners,
            ..two_partner_request()
        };
        let bundle = build_return_from_ledger(store.connection(), &req).unwrap();
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w == crate::tax::capital::BEGINNING_CAPITAL_CAVEAT),
            "{:?}",
            bundle.warnings
        );
        crate::tax::warning_shape::assert_all(&bundle.warnings);
    }
}
