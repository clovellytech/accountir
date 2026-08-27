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
    FieldMap, FormError, append_document, field_map, namespace_fields, set_check, set_text,
    strip_xfa,
};
use super::lines::{Form1065Lines, format_dollars};
use crate::domain::{BusinessProfile, Partner, PartnerType, Residency, Shares, format_ppm};
use chrono::{Datelike, NaiveDate};
use lopdf::Document;

/// The blank forms, carried in the binary.
///
/// Embedded rather than fetched: this is a local-first program, and a return you
/// can only produce with a working connection to irs.gov is one you cannot
/// produce on the afternoon it is due.
const F1065: &[u8] = include_bytes!("../../assets/irs/f1065.pdf");
const F1065_SK1: &[u8] = include_bytes!("../../assets/irs/f1065sk1.pdf");

/// The tax year the vendored forms are for.
///
/// Checked against the year being filed so that filing 2024 with the 2025 form
/// is a message rather than a quietly wrong return.
pub const FORM_TAX_YEAR: i32 = 2025;

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
mod f1065 {
    /// "Name of partnership."
    pub const LEGAL_NAME: &str = "f1_04[0]";
    /// "Number and street."
    pub const STREET: &str = "f1_05[0]";
    /// "Room or suite no."
    pub const SUITE: &str = "f1_06[0]";
    /// "City or town."
    pub const CITY: &str = "f1_07[0]";
    /// "State or province."
    pub const STATE: &str = "f1_08[0]";
    /// "Country."
    pub const COUNTRY: &str = "f1_09[0]";
    /// "Z I P or foreign postal code."
    pub const POSTAL_CODE: &str = "f1_10[0]";
    /// "A. Principal business activity."
    pub const PRINCIPAL_ACTIVITY: &str = "f1_11[0]";
    /// "B. Principal product or service."
    pub const PRINCIPAL_PRODUCT: &str = "f1_12[0]";
    /// "C. Business code number." — the NAICS code.
    pub const NAICS: &str = "f1_13[0]";
    /// "D. Employer identification number."
    pub const EIN: &str = "f1_14[0]";
    /// "E. Date business started."
    pub const DATE_STARTED: &str = "f1_15[0]";
    /// "I. Number of Schedules K-1."
    pub const K1_COUNT: &str = "f1_18[0]";

    // --- Income, lines 1a-8 ---
    /// "1a. Gross receipts or sales."
    pub const L1A_GROSS_RECEIPTS: &str = "f1_19[0]";
    /// "1b. Less returns and allowances."
    pub const L1B_RETURNS: &str = "f1_20[0]";
    /// "1c. Balance." — derived.
    pub const L1C_BALANCE: &str = "f1_21[0]";
    /// "2. Cost of goods sold (attach Form 1125-A)."
    pub const L2_COGS: &str = "f1_22[0]";
    /// "3. Gross profit. Subtract line 2 from line 1c." — derived.
    pub const L3_GROSS_PROFIT: &str = "f1_23[0]";
    /// "4. Ordinary income (loss) from other partnerships, estates, and trusts."
    pub const L4_OTHER_PARTNERSHIPS: &str = "f1_24[0]";
    /// "5. Net farm profit (loss)."
    pub const L5_FARM: &str = "f1_25[0]";
    /// "6. Net gain (loss) from Form 4797, Part II, line 17."
    pub const L6_FORM_4797: &str = "f1_26[0]";
    /// "7. Other income (loss)."
    pub const L7_OTHER_INCOME: &str = "f1_27[0]";
    /// "8. Total income (loss). Combine lines 3 through 7." — derived.
    pub const L8_TOTAL_INCOME: &str = "f1_28[0]";

    // --- Deductions, lines 9-23 ---
    /// "9. Salaries and wages (other than to partners) (less employment credits)."
    pub const L9_SALARIES: &str = "f1_29[0]";
    /// "10. Guaranteed payments to partners."
    pub const L10_GUARANTEED: &str = "f1_30[0]";
    /// "11. Repairs and maintenance."
    pub const L11_REPAIRS: &str = "f1_31[0]";
    /// "12. Bad debts."
    pub const L12_BAD_DEBTS: &str = "f1_32[0]";
    /// "13. Rent."
    pub const L13_RENT: &str = "f1_33[0]";
    /// "14. Taxes and licenses."
    pub const L14_TAXES: &str = "f1_34[0]";
    /// "15. Interest (see instructions)."
    pub const L15_INTEREST: &str = "f1_35[0]";
    /// "16a. Depreciation (if required, attach Form 4562)."
    pub const L16A_DEPRECIATION: &str = "f1_36[0]";
    /// "16b. Less depreciation reported on Form 1125-A and elsewhere on return."
    pub const L16B_DEPRECIATION_ELSEWHERE: &str = "f1_37[0]";
    /// "16c. Amount." — derived, 16a less 16b.
    pub const L16C_DEPRECIATION_NET: &str = "f1_38[0]";
    /// "17. Depletion (Do not deduct oil and gas depletion.)."
    pub const L17_DEPLETION: &str = "f1_39[0]";
    /// "18. Retirement plans, etc."
    pub const L18_RETIREMENT: &str = "f1_40[0]";
    /// "19. Employee benefit programs."
    pub const L19_BENEFITS: &str = "f1_41[0]";
    /// "20. Energy efficient commercial buildings deduction (attach Form 7205)."
    pub const L20_ENERGY: &str = "f1_42[0]";
    /// "21. Other deductions (attach statement)."
    pub const L21_OTHER_DEDUCTIONS: &str = "f1_43[0]";
    /// "22. Total deductions. Add the amounts shown ... for lines 9 through 21." — derived.
    pub const L22_TOTAL_DEDUCTIONS: &str = "f1_44[0]";
    /// "23. Ordinary business income (loss). Subtract line 22 from line 8." — derived.
    pub const L23_ORDINARY_INCOME: &str = "f1_45[0]";

    /// "Paid Preparer Use Only. Enter preparer's name."
    pub const PREPARER_NAME: &str = "f1_57[0]";
}

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
        CodedBox { line_key: "k11",  number: "11",  code: None,      code_field: "f1_50[0]",  amount_field: "f1_51[0]" },
        CodedBox { line_key: "k13a", number: "13a", code: None,      code_field: "Line13[0]", amount_field: "f1_55[0]" },
        CodedBox { line_key: "k13b", number: "13b", code: None,      code_field: "f1_56[0]",  amount_field: "f1_57[0]" },
        CodedBox { line_key: "k13c", number: "13c", code: None,      code_field: "f1_58[0]",  amount_field: "f1_59[0]" },
        CodedBox { line_key: "k14a", number: "14a", code: Some("A"), code_field: "Line14[0]", amount_field: "f1_60[0]" },
        CodedBox { line_key: "k14b", number: "14b", code: Some("B"), code_field: "f1_61[0]",  amount_field: "f1_62[0]" },
        CodedBox { line_key: "k18a", number: "18a", code: Some("A"), code_field: "Line18[0]", amount_field: "f1_84[0]" },
        CodedBox { line_key: "k18b", number: "18b", code: Some("B"), code_field: "f1_85[0]",  amount_field: "f1_86[0]" },
        CodedBox { line_key: "k18c", number: "18c", code: Some("C"), code_field: "f1_87[0]",  amount_field: "f1_88[0]" },
        CodedBox { line_key: "k19a", number: "19a", code: Some("A"), code_field: "Line19[0]", amount_field: "f1_89[0]" },
        CodedBox { line_key: "k19b", number: "19b", code: None,      code_field: "f1_90[0]",  amount_field: "f1_91[0]" },
        CodedBox { line_key: "k20a", number: "20a", code: Some("A"), code_field: "Line20[0]", amount_field: "f1_92[0]" },
        CodedBox { line_key: "k20b", number: "20b", code: Some("B"), code_field: "f1_93[0]",  amount_field: "f1_94[0]" },
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
    let mapping = super::lines::load_mapping(conn);
    let computed = super::lines::compute(&statement, &mapping);

    // Schedule L comes from the ledger too, and this is the only entry point
    // that has one. Computed here rather than demanded from the caller: every
    // caller with a connection would write the same three lines, and the one
    // that forgot would ship a return with a blank balance sheet and no warning.
    // An explicitly-supplied schedule wins, so a caller can still override it.
    let mut owned = req.clone();
    if owned.schedule_l.is_none() {
        owned.schedule_l = super::schedule_l::compute(conn, req.year, &mapping).ok();
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
pub fn build_for_preview(
    req: &ReturnRequest,
    lines: &Form1065Lines,
) -> Result<Bundle, FormError> {
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
    let mut doc = Document::load_mem(F1065)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);
    warnings.extend(fill_1065(&mut doc, &map, &req.profile, filed.len(), lines)?);
    warnings.extend(fill_schedule_k(&mut doc, &map, lines)?);
    warnings.extend(super::schedule_b::fill(&mut doc, &map, &req.schedule_b)?);

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
                "Schedule L is blank because no balance sheet was computed for this return.                  `build_return_from_ledger` reads one from the books; `build_return` has no                  ledger to read."
                    .to_string(),
            ),
        }

        let m = super::schedule_m::reconcile(
            req.book_income_cents,
            lines,
            req.schedule_l.as_ref(),
        );
        warnings.extend(super::schedule_m::fill(&mut doc, &map, &m, !exempt)?);
    } else {
        warnings.push(
            "Schedules L, M-1 and M-2 are blank: question 4 excuses them, and completing them \
             anyway is switched off. Nothing then checks that the books and the return agree."
                .to_string(),
        );
    }

    // Split Schedule K before any K-1 is built, so every partner's share comes
    // out of one apportionment and the shares add back to the totals above.
    let (shares, split_warnings) = split_across_partners(lines, &filed);
    warnings.extend(split_warnings);

    // --- one K-1 per partner ---
    for (i, filing) in filed.iter().enumerate() {
        let mut sched = Document::load_mem(F1065_SK1)?;
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
            year_start,
            year_end,
        )?);
        append_document(&mut doc, sched)?;
    }

    // --- Schedule B-1 and B-2 ---
    //
    // Before the statements, because they are IRS schedules and the statements
    // are ours: the return reads form, K-1s, official schedules, then the
    // supporting pages we composed.
    if super::schedule_b1::is_required(&req.schedule_b) {
        let owners: Vec<super::schedule_b1::Owner> = req
            .partners
            .iter()
            .map(|f| super::schedule_b1::Owner {
                partner: &f.partner,
                tin: f.tin.as_deref(),
            })
            .collect();
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
                "Schedule B question 2a or 2b is Yes, but no partner in the books owns 50% or                  more, so no Schedule B-1 was produced. Either the answer is wrong or the owner                  holds their interest indirectly — the schedule has to be attached by hand in                  that case."
                    .to_string(),
            ),
        }
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
                "Question 31 says the Schedule B-2 total is {typed}, but the schedule produced                  lists {count} partner(s). The two have to agree."
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
                "Line {} carries a figure and the form asks for a statement of what is in it, but                  no account detail was supplied, so none was produced. Attach one before filing.",
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

    if req.year != FORM_TAX_YEAR {
        out.push(format!(
            "The bundled forms are the {FORM_TAX_YEAR} revision, but this is a {} return. \
             Replace assets/irs/*.pdf with that year's forms and regenerate \
             docs/form-1065-fields.md.",
            req.year
        ));
    }

    // Shares are checked here rather than when a partner is saved: a partnership
    // passes through states where they do not total the whole, and this is the
    // point at which they have to.
    // Over the partners actually on this return. A partner who left in a prior
    // year still holds a share in the books, and counting theirs would report a
    // split that does not add up for a return they are not on.
    let shares: Vec<Shares> = filed.iter().map(|p| p.partner.shares).collect();
    if !shares.is_empty() {
        let totals = Shares::sums_to_whole(&shares);
        if !totals.is_whole() {
            out.push(format!(
                "Partner shares do not total 100%: {}. Every K-1 will be filed with these figures.",
                totals.discrepancies().join(", ")
            ));
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

fn fill_1065(
    doc: &mut Document,
    map: &FieldMap,
    profile: &BusinessProfile,
    k1_count: usize,
    lines: &Form1065Lines,
) -> Result<Vec<String>, FormError> {
    let addr = &profile.address;
    set_text(doc, map, f1065::LEGAL_NAME, &profile.legal_name)?;
    set_text(doc, map, f1065::STREET, &addr.street)?;
    set_text(doc, map, f1065::SUITE, addr.suite.as_deref().unwrap_or(""))?;
    set_text(doc, map, f1065::CITY, &addr.city)?;
    set_text(doc, map, f1065::STATE, &addr.state)?;
    set_text(doc, map, f1065::COUNTRY, addr.country.as_deref().unwrap_or(""))?;
    set_text(doc, map, f1065::POSTAL_CODE, &addr.postal_code)?;
    set_text(doc, map, f1065::NAICS, &profile.naics_code)?;
    set_text(doc, map, f1065::EIN, &profile.ein)?;
    set_text(doc, map, f1065::DATE_STARTED, &us_date(profile.formation_date))?;
    set_text(doc, map, f1065::K1_COUNT, &k1_count.to_string())?;

    if let Some(a) = profile.principal_activity.as_deref() {
        set_text(doc, map, f1065::PRINCIPAL_ACTIVITY, a)?;
    }
    if let Some(p) = profile.principal_product.as_deref() {
        set_text(doc, map, f1065::PRINCIPAL_PRODUCT, p)?;
    }

    // The tax-year boxes at the top are deliberately left blank. The form reads
    // "For calendar year 2025, or tax year beginning ___", so a calendar-year
    // filer fills in nothing; writing the dates in would assert a fiscal year
    // that was never chosen.

    set_text(doc, map, f1065::PREPARER_NAME, SELF_PREPARED)?;

    // The PTIN, firm name, firm EIN, firm address and phone beside it stay blank,
    // and the "check if self-employed" box stays unticked: all of them describe a
    // paid preparer, and there is not one. So does "May the IRS discuss this
    // return with the preparer shown below?" — a question about somebody who does
    // not exist here, and one whose answer is the signer's to give.

    fill_income_lines(doc, map, lines)
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
    lines: &Form1065Lines,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();
    let mapped = [
        (f1065::L1A_GROSS_RECEIPTS, lines.get("l1a")),
        (f1065::L1B_RETURNS, lines.get("l1b")),
        (f1065::L2_COGS, lines.get("l2")),
        (f1065::L4_OTHER_PARTNERSHIPS, lines.get("l4")),
        (f1065::L5_FARM, lines.get("l5")),
        (f1065::L6_FORM_4797, lines.get("l6")),
        (f1065::L7_OTHER_INCOME, lines.get("l7")),
        (f1065::L9_SALARIES, lines.get("l9")),
        (f1065::L10_GUARANTEED, lines.get("l10")),
        (f1065::L11_REPAIRS, lines.get("l11")),
        (f1065::L12_BAD_DEBTS, lines.get("l12")),
        (f1065::L13_RENT, lines.get("l13")),
        (f1065::L14_TAXES, lines.get("l14")),
        (f1065::L15_INTEREST, lines.get("l15")),
        (f1065::L16A_DEPRECIATION, lines.get("l16a")),
        (f1065::L16B_DEPRECIATION_ELSEWHERE, lines.get("l16b")),
        (f1065::L17_DEPLETION, lines.get("l17")),
        (f1065::L18_RETIREMENT, lines.get("l18")),
        (f1065::L19_BENEFITS, lines.get("l19")),
        (f1065::L20_ENERGY, lines.get("l20")),
        (f1065::L21_OTHER_DEDUCTIONS, lines.get("l21")),
        // Derived. Written on the same non-zero rule so a page with no COGS does
        // not carry a gross-profit line restating gross receipts.
        (f1065::L1C_BALANCE, lines.line_1c()),
        (f1065::L3_GROSS_PROFIT, lines.line_3()),
        (f1065::L8_TOTAL_INCOME, lines.line_8()),
        (f1065::L16C_DEPRECIATION_NET, lines.line_16c()),
        (f1065::L22_TOTAL_DEDUCTIONS, lines.line_22()),
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
        f1065::L23_ORDINARY_INCOME,
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
    write_money(doc, map, sched_k::L3C_NET_RENTAL, lines.k_line_3c(), &mut warnings)?;
    write_money(doc, map, sched_k::L4C_TOTAL_GUARANTEED, lines.k_line_4c(), &mut warnings)?;
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
            "Nothing is mapped to a Schedule K line, so every separately stated item is blank.              Charitable contributions, section 179, investment interest and capital gains belong              there rather than in page 1, line 21."
                .to_string(),
        );
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
) -> (Vec<PartnerShares>, Vec<String>) {
    use super::allocate::{allocate, profit_and_loss_shares_differ, Basis};

    let partners: Vec<&Partner> = filed.iter().map(|f| &f.partner).collect();
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

    for (key, total) in figures {
        if total == 0 {
            continue;
        }
        for share in allocate(total, &partners, Basis::ProfitOrLoss) {
            out[share.partner].by_line.insert(key, share.dollars);
        }
    }

    let mut warnings = Vec::new();
    if profit_and_loss_shares_differ(&partners) {
        warnings.push(
            "Profit and loss percentages differ for at least one partner, so income items and              loss items were split on different percentages. Check each K-1 against the              partnership agreement."
                .to_string(),
        );
    }
    (out, warnings)
}

fn fill_k1(
    doc: &mut Document,
    map: &FieldMap,
    profile: &BusinessProfile,
    filing: &PartnerFiling,
    shares: &PartnerShares,
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
    set_text(doc, map, k1::PARTNER_TIN, filing.tin.as_deref().unwrap_or(""))?;
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
            schedule_l: None,
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
    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_forms() {
        let doc = Document::load_mem(F1065).unwrap();
        let map = field_map(&doc);
        for name in [
            f1065::LEGAL_NAME,
            f1065::STREET,
            f1065::SUITE,
            f1065::CITY,
            f1065::STATE,
            f1065::COUNTRY,
            f1065::POSTAL_CODE,
            f1065::PRINCIPAL_ACTIVITY,
            f1065::PRINCIPAL_PRODUCT,
            f1065::NAICS,
            f1065::EIN,
            f1065::DATE_STARTED,
            f1065::K1_COUNT,
            f1065::L1A_GROSS_RECEIPTS,
            f1065::L1B_RETURNS,
            f1065::L1C_BALANCE,
            f1065::L2_COGS,
            f1065::L3_GROSS_PROFIT,
            f1065::L4_OTHER_PARTNERSHIPS,
            f1065::L5_FARM,
            f1065::L6_FORM_4797,
            f1065::L7_OTHER_INCOME,
            f1065::L8_TOTAL_INCOME,
            f1065::L9_SALARIES,
            f1065::L10_GUARANTEED,
            f1065::L11_REPAIRS,
            f1065::L12_BAD_DEBTS,
            f1065::L13_RENT,
            f1065::L14_TAXES,
            f1065::L15_INTEREST,
            f1065::L16A_DEPRECIATION,
            f1065::L16B_DEPRECIATION_ELSEWHERE,
            f1065::L16C_DEPRECIATION_NET,
            f1065::L17_DEPLETION,
            f1065::L18_RETIREMENT,
            f1065::L19_BENEFITS,
            f1065::L20_ENERGY,
            f1065::L21_OTHER_DEDUCTIONS,
            f1065::L22_TOTAL_DEDUCTIONS,
            f1065::L23_ORDINARY_INCOME,
        ] {
            assert!(map.find(name).is_some(), "f1065.pdf has no field {name}");
        }

        let sched = Document::load_mem(F1065_SK1).unwrap();
        let smap = field_map(&sched);

        // Part III: every box a partner's share is written into. Catches the
        // revision that renumbers the K-1 independently of the 1065, which has
        // happened before and which nothing else here would notice.
        for (line_key, field) in k1::PART_III {
            assert!(
                smap.find(field).is_some(),
                "f1065sk1.pdf has no field {field} for Schedule K line {line_key}"
            );
        }
        for b in k1::CODED_BOXES {
            assert!(
                smap.find(b.amount_field).is_some(),
                "f1065sk1.pdf has no amount box {} for line {}",
                b.amount_field,
                b.number
            );
            assert!(
                smap.find(b.code_field).is_some(),
                "f1065sk1.pdf has no code box {} for line {}",
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
        ] {
            assert!(smap.find(name).is_some(), "f1065sk1.pdf has no field {name}");
        }
    }

    /// Ticking a box with the wrong appearance state leaves it looking unticked.
    #[test]
    fn the_checkbox_states_are_the_ones_the_form_was_built_with() {
        let sched = Document::load_mem(F1065_SK1).unwrap();
        let map = field_map(&sched);
        assert_eq!(acroform::on_states(&sched, &map, k1::TYPE_GENERAL), [k1::ON]);
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

        let get = |n: &str| {
            acroform::get_value_in(&doc, &map, FORM_ROOT, n).unwrap_or_default()
        };
        assert_eq!(get(f1065::LEGAL_NAME), "Clovelly Technology Partners LLC");
        assert_eq!(get(f1065::EIN), "88-1234567");
        assert_eq!(get(f1065::NAICS), "541511");
        assert_eq!(get(f1065::DATE_STARTED), "07/01/2021");
        assert_eq!(get(f1065::CITY), "Cape Town");
        assert_eq!(get(f1065::SUITE), "Suite 4");
        assert_eq!(get(f1065::K1_COUNT), "2", "one K-1 per partner");
    }

    /// The bug this whole design exists to prevent: two K-1s sharing a field
    /// name means typing one partner's TIN fills it in for everybody.
    #[test]
    fn each_partners_k1_holds_that_partners_details_and_not_another_s() {
        let bundle = build_return(&two_partner_request()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);

        let tin = |n: usize| {
            acroform::get_value_in(&doc, &map, &k1_namespace(n), k1::PARTNER_TIN).unwrap_or_default()
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
            acroform::get_value_in(&doc, &map, FORM_ROOT, f1065::K1_COUNT),
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
            !bundle
                .warnings
                .iter()
                .any(|w| w.contains("do not total 100%")),
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
            let Some(font) = da.split_whitespace().next().and_then(|t| t.strip_prefix('/')) else {
                continue;
            };
            assert!(
                fonts.has(font.as_bytes()),
                "field {name} asks for /{font}, which the merged form does not carry"
            );
        }
    }

    #[test]
    fn a_partner_who_left_gets_a_final_k1_and_an_ending_share_of_nothing() {
        let mut req = two_partner_request();
        req.partners[1].partner.end_date = Some(day(FORM_TAX_YEAR, 6, 30));

        let bundle = build_return(&req).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        let get = |f: &str| {
            acroform::get_value_in(&doc, &map, &k1_namespace(2), f).unwrap_or_default()
        };
        assert_eq!(get(k1::FINAL), "/1", "a departing partner's K-1 is final");
        assert_eq!(get(k1::PROFIT_BEGIN), "50");
        assert_eq!(get(k1::PROFIT_END), "0");
    }

    /// Shares that do not add up are the classic silently-wrong return.
    #[test]
    fn shares_that_do_not_total_the_whole_are_reported_rather_than_swallowed() {
        let mut req = two_partner_request();
        req.partners[1].partner.shares = Shares::from_percents(30.0, 30.0, 30.0);

        let bundle = build_return(&req).unwrap();
        assert!(
            bundle.warnings.iter().any(|w| w.contains("do not total 100%")),
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
            bundle.warnings.iter().any(|w| w.contains("No TIN") && w.contains("Bob")),
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
            year: FORM_TAX_YEAR,
            profile: profile(),
            partners: pc::partners_for_year(store.connection(), FORM_TAX_YEAR)
                .into_iter()
                .map(|partner| PartnerFiling { partner, tin: None })
                .collect(),
            schedule_b: Default::default(),
            schedule_l: None,
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
            ("cogs", EventAccountType::Expense, "5000", "Cost of goods sold"),
            ("wages", EventAccountType::Expense, "6000", "Wages"),
            ("rent", EventAccountType::Expense, "6100", "Rent"),
            ("mystery", EventAccountType::Expense, "6999", "Unmapped expense"),
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
        set_account_line(conn, "sales", "l1a").unwrap();
        set_account_line(conn, "refunds", "l1b").unwrap();
        set_account_line(conn, "cogs", "l2").unwrap();
        set_account_line(conn, "wages", "l9").unwrap();
        set_account_line(conn, "rent", "l13").unwrap();
        // "mystery" deliberately left unmapped.
    }

    /// The end-to-end property: figures posted to the ledger reach the finished
    /// page, and the totals printed on that page are the arithmetic of the other
    /// figures printed on it.
    #[test]
    fn a_return_built_from_the_ledger_carries_income_lines_that_add_up() {
        let store = seeded_ledger();
        map_seeded_accounts(store.connection());

        let bundle =
            build_return_from_ledger(store.connection(), &two_partner_request()).unwrap();
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
        assert_eq!(get(f1065::L1A_GROSS_RECEIPTS), 4001, "$4,000.50");
        assert_eq!(get(f1065::L1B_RETURNS), 101, "refunds print positive");
        assert_eq!(get(f1065::L2_COGS), 1001);
        assert_eq!(get(f1065::L9_SALARIES), 801);
        assert_eq!(get(f1065::L13_RENT), 201);

        // Every total, recomputed from what the page itself shows.
        let (l1a, l1b, l2) = (get(f1065::L1A_GROSS_RECEIPTS), get(f1065::L1B_RETURNS), get(f1065::L2_COGS));
        let (l9, l13) = (get(f1065::L9_SALARIES), get(f1065::L13_RENT));

        assert_eq!(get(f1065::L1C_BALANCE), l1a - l1b);
        assert_eq!(get(f1065::L3_GROSS_PROFIT), (l1a - l1b) - l2);
        assert_eq!(get(f1065::L8_TOTAL_INCOME), (l1a - l1b) - l2);
        assert_eq!(get(f1065::L22_TOTAL_DEDUCTIONS), l9 + l13);
        assert_eq!(
            get(f1065::L23_ORDINARY_INCOME),
            get(f1065::L8_TOTAL_INCOME) - get(f1065::L22_TOTAL_DEDUCTIONS),
            "the bottom line must be the page's own arithmetic"
        );
        assert_eq!(get(f1065::L23_ORDINARY_INCOME), 2899 - 1002);
    }

    /// An expense with no line is money missing from the return. It must be
    /// named on the way past, not swept into a line it was never assigned.
    #[test]
    fn an_unmapped_account_is_reported_and_not_silently_absorbed() {
        let store = seeded_ledger();
        map_seeded_accounts(store.connection());

        let bundle =
            build_return_from_ledger(store.connection(), &two_partner_request()).unwrap();
        let joined = bundle.warnings.join(" ");
        assert!(joined.contains("6999"), "got {joined}");
        assert!(joined.contains("Unmapped expense"), "got {joined}");

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, f1065::L21_OTHER_DEDUCTIONS),
            None,
            "the unmapped $50 must not have landed on other deductions"
        );
    }

    /// A ledger nobody has mapped yet produces an identity-only return, and says
    /// so — rather than a page of zeros that reads as a completed nil return.
    #[test]
    fn an_unmapped_ledger_leaves_the_money_lines_blank_and_says_why() {
        let store = seeded_ledger();
        let bundle =
            build_return_from_ledger(store.connection(), &two_partner_request()).unwrap();

        assert!(
            bundle.warnings.iter().any(|w| w.contains("No accounts are mapped")),
            "got {:?}",
            bundle.warnings
        );

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, f1065::L1A_GROSS_RECEIPTS),
            None,
            "no figure was known, so no figure is claimed"
        );
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, f1065::L23_ORDINARY_INCOME),
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
            acroform::get_value_in(&doc, &map, FORM_ROOT, f1065::LEGAL_NAME),
            Some("Clovelly Technology Partners LLC".into())
        );
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, f1065::L1A_GROSS_RECEIPTS),
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
        let warnings = fill_income_lines(&mut doc, &map, &lines).unwrap();
        assert!(warnings.is_empty(), "nothing should have been refused: {warnings:?}");

        // Round-trip through a real save/load, not just the in-memory dict.
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let doc = Document::load_mem(&bytes).unwrap();
        let map = field_map(&doc);
        let get = |n: &str| acroform::get_value_in(&doc, &map, FORM_ROOT, n).unwrap_or_default();

        assert_eq!(get(f1065::L1A_GROSS_RECEIPTS), "987,654,321");
        assert_eq!(get(f1065::L9_SALARIES), "123,456,789");
        assert_eq!(
            get(f1065::L23_ORDINARY_INCOME),
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
            f1065::L1A_GROSS_RECEIPTS,
            f1065::L1B_RETURNS,
            f1065::L1C_BALANCE,
            f1065::L2_COGS,
            f1065::L3_GROSS_PROFIT,
            f1065::L8_TOTAL_INCOME,
            f1065::L9_SALARIES,
            f1065::L16C_DEPRECIATION_NET,
            f1065::L21_OTHER_DEDUCTIONS,
            f1065::L22_TOTAL_DEDUCTIONS,
            f1065::L23_ORDINARY_INCOME,
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
            acroform::max_len(&doc, &map, f1065::EIN),
            Some(10),
            "an EIN is NN-NNNNNNN"
        );

        let sched = Document::load_mem(F1065_SK1).unwrap();
        let smap = field_map(&sched);
        assert_eq!(acroform::max_len(&sched, &smap, k1::PARTNERSHIP_EIN), Some(10));
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

        let err = set_text(&mut doc, &map, f1065::EIN, "88-1234567-EXTRA").unwrap_err();
        match err {
            FormError::ValueTooLong { len, max, .. } => {
                assert_eq!(max, 10);
                assert_eq!(len, 16);
            }
            other => panic!("expected ValueTooLong, got {other:?}"),
        }
        assert_eq!(
            acroform::get_value_in(&doc, &map, FORM_ROOT, f1065::EIN),
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
        for q in ["b3a","b3b","b5","b6","b7","b8","b9","b12","b16a","b19","b20","b21","b23","b27","b30","b4"] {
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
        req.detail.insert("l21", vec![
            LineDetail { account_id:"1".into(), account_number:"6100".into(), account_name:"Advertising and promotion".into(), cents: 12_450_00 },
            LineDetail { account_id:"2".into(), account_number:"6200".into(), account_name:"Professional fees".into(), cents: 8_900_00 },
            LineDetail { account_id:"3".into(), account_number:"6300".into(), account_name:"Software subscriptions".into(), cents: 4_215_00 },
            LineDetail { account_id:"4".into(), account_number:"6400".into(), account_name:"Bank and merchant charges".into(), cents: 1_980_50 },
            LineDetail { account_id:"5".into(), account_number:"6500".into(), account_name:"Office supplies".into(), cents: 2_104_50 },
        ]);

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
        for (k, v) in [("l1a", 480_000i64), ("l2", 150_000), ("l9", 120_000), ("l13", 36_000),
                       ("l14", 18_400), ("l16a", 22_000), ("l21", 29_650),
                       ("k5", 3_200), ("k13a", 5_000), ("k12", 14_000), ("k19a", 60_000)] {
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
        let mut owner = partner("Holdings LLC", PartnerType::General, Residency::Domestic, 60.0);
        owner.entity_type = "Partnership".to_string();
        owner.shares = Shares::from_percents(60.0, 60.0, 60.0);
        req.partners = vec![PartnerFiling { partner: owner, tin: Some("98-7654321".into()) }];

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
            bundle.warnings.iter().any(|w| w.contains("family members")),
            "{:?}",
            bundle.warnings
        );
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
            bundle.warnings.iter().any(|w| w.contains("no partner in the books owns 50%")),
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
        assert!(!text.contains("49842K"), "an unrequested Schedule B-1 was attached");
        assert!(!text.contains("69658K"), "an unrequested Schedule B-2 was attached");
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
            bundle.warnings.iter().any(|w| w.contains("Nothing then checks")),
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
                LineDetail { account_id: "a".into(), account_number: "6100".into(), account_name: "Advertising".into(), cents: 1_200_00 },
                LineDetail { account_id: "b".into(), account_number: "6200".into(), account_name: "Professional fees".into(), cents: 3_400_00 },
                LineDetail { account_id: "c".into(), account_number: "6300".into(), account_name: "Software subscriptions".into(), cents: 900_00 },
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

        assert!(text.contains("Advertising"), "statement page missing from the bundle");
        assert!(text.contains("Professional fees"));
        assert!(text.contains("Software subscriptions"));
        assert!(text.contains("5,500"), "the statement must total to the box");
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
        let (shares, _) = split_across_partners(&lines, &filed);

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
        use crate::tax::lines::Form1065Lines;
        use crate::domain::Shares;

        let mut a = partner("Alice", PartnerType::General, Residency::Domestic, 50.0);
        a.shares = Shares { profit_ppm: 100_000, loss_ppm: 900_000, capital_ppm: 500_000 };
        let mut b = partner("Bob", PartnerType::General, Residency::Domestic, 50.0);
        b.shares = Shares { profit_ppm: 900_000, loss_ppm: 100_000, capital_ppm: 500_000 };

        let filings = vec![
            PartnerFiling { partner: a, tin: None },
            PartnerFiling { partner: b, tin: None },
        ];
        let filed: Vec<&PartnerFiling> = filings.iter().collect();

        let mut lines = Form1065Lines::default();
        lines.set_for_test("k5", 1000);      // income
        lines.set_for_test("k10", -1000);    // loss

        let (shares, warnings) = split_across_partners(&lines, &filed);
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
            crate::tax::acroform::get_value(&doc, &map, f1065::L21_OTHER_DEDUCTIONS),
            None,
            "line 21 must stay empty"
        );
    }

    #[test]
    fn filing_a_year_the_bundled_forms_are_not_for_is_flagged() {
        let req = ReturnRequest {
            year: FORM_TAX_YEAR - 1,
            ..two_partner_request()
        };
        let bundle = build_return(&req).unwrap();
        assert!(
            bundle.warnings.iter().any(|w| w.contains("revision")),
            "got {:?}",
            bundle.warnings
        );
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
}
