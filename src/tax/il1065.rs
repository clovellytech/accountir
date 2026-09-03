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

use crate::domain::{BusinessProfile, Il1065Settings};

use super::acroform::{field_map, set_check, set_text, strip_xfa, FieldMap, FormError};
use super::form1065::{Bundle, PartnerFiling};
use super::lines::{format_dollars, Form1065Lines};
use lopdf::Document;

const IL1065: &[u8] = include_bytes!("../../assets/il/il1065.pdf");

/// Illinois' replacement-tax rate, 1.5%, as a numerator over 1000.
const REPLACEMENT_TAX_PER_MILLE: i64 = 15;
/// Illinois' PTE-tax rate, 4.95%, as a numerator over 10_000.
const PTE_TAX_PER_TEN_THOUSAND: i64 = 495;

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
    pub const L20_GUARANTEED: &str = "Guaranteed payments";
    pub const L23_INCOME: &str = "Income/loss";

    // Step 5 — subtractions.
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

    // Schedule B header (Section B, page 5).
    pub const SCHB_NAME: &str =
        "Schedule B. Enter your name as shown on your Form IL-1065 or Form IL-1120-ST";
    pub const SCHB_FEIN_2: &str = "Schedule B. Enter the initial two digits of your FEIN";
    pub const SCHB_FEIN_7: &str = "Schedule B. Enter the last seven digits of your FEIN";

    // Schedule B header (Section A, page 4) — the same identity, echoed.
    pub const SCHA_NAME: &str = "PSI name";
    pub const SCHA_FEIN_2: &str = "PSI FEIN-2";
    pub const SCHA_FEIN_7: &str = "PSI FEIN-7";
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
// Column B is left blank (a one-character Illinois code); kept here so the tests
// still assert the box exists, hence the allow for non-test builds.
#[allow(dead_code)]
const M_COL_B_TYPE: &str = ", Column B - Partner or Shareholder type. See instructions";
const M_COL_C_TIN: &str = ", Column C - Social Security number or Federal Employer Identification \
                           Number of the partner or shareholder";
const M_COL_D_SUBJECT: &str = ", Column D - Check if your partner or shareholder is subject to \
                               Illinois replacment tax or is an ESOP";
const M_COL_D_ON: &str = "Yes";
const M_COL_E_SHARE: &str = ", Column E - Member's distributable amount of base income or loss";

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
    pub line20: i64,
    pub line23: i64,
    pub line35: i64,
    /// Base income the tax is figured on. Equals line 35 for an Illinois-only
    /// partnership; `None` when apportioning, because it depends on sales figures
    /// the books do not hold.
    pub line47: Option<i64>,
    pub line53: Option<i64>,
    pub line54: Option<i64>,
    pub line58: Option<i64>,
    pub line61: Option<i64>,
    pub line62: Option<i64>,
}

/// Compute the return's figures from the federal Schedule K totals and settings.
pub fn figures(federal: &Form1065Lines, settings: &Il1065Settings) -> Figures {
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

    // Step 4 — additions. Only the guaranteed payments are a federal figure; the
    // Illinois-specific additions are left blank.
    let line14 = line13;
    let line20 = federal.k_line_4c();
    let line23 = line14 + line20;

    // Step 5 — subtractions all Illinois-specific, so none are known here.
    let line34 = 0;
    let line35 = line23 - line34;

    // Steps 6–9 depend on apportionment. Only the Illinois-only path can be
    // carried through to the tax, because the apportioned path needs sales.
    if settings.apportions_outside_illinois {
        return Figures {
            line1, line2, line3, line4, line5, line7, line8, line9, line10, line12, line13,
            line20, line23, line35,
            line47: None, line53: None, line54: None, line58: None, line61: None, line62: None,
        };
    }

    // Illinois-only: base income flows straight through Step 7 (no NLD or
    // exemption for a partnership), and the replacement tax is 1.5% of it. A net
    // loss owes no tax.
    let line47 = line35;
    let line53 = line47; // line 48 NLD = 0, line 52 exemption = 0
    let taxable = line53.max(0);
    let line54 = round_rate(taxable, REPLACEMENT_TAX_PER_MILLE, 1000);
    let line58 = line54;

    let line61 = if settings.elects_pte_tax {
        round_rate(taxable, PTE_TAX_PER_TEN_THOUSAND, 10_000)
    } else {
        0
    };
    let line62 = line58 + line61; // line 59 withholding = 0 here

    Figures {
        line1, line2, line3, line4, line5, line7, line8, line9, line10, line12, line13,
        line20, line23, line35,
        line47: Some(line47),
        line53: Some(line53),
        line54: Some(line54),
        line58: Some(line58),
        line61: Some(line61),
        line62: Some(line62),
    }
}

/// `amount × num / den`, rounded half up. Only called on non-negative amounts —
/// tax on a loss is zero, and the caller clamps before dividing.
fn round_rate(amount: i64, num: i64, den: i64) -> i64 {
    (amount * num + den / 2) / den
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
) -> Result<Bundle, FormError> {
    let mut warnings = Vec::new();
    let figs = figures(federal, settings);

    let mut doc = Document::load_mem(IL1065)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    fill_identity(&mut doc, &map, profile, settings)?;
    fill_income(&mut doc, &map, &figs, &mut warnings)?;
    fill_tax(&mut doc, &map, &figs, settings, &mut warnings)?;
    fill_schedule_b(&mut doc, &map, profile, partners, &figs, &mut warnings)?;

    warnings.extend(caveats(profile, settings, partners.len()));

    let mut pdf = Vec::new();
    doc.save_to(&mut pdf)?;
    let page_count = doc.get_pages().len();
    Ok(Bundle { pdf, warnings, page_count })
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
    let mapping = super::lines::load_mapping(conn);
    let federal = super::lines::compute(&statement, &mapping).lines;

    // The partners who held an interest during the year — one Schedule B row each —
    // with the TIN this machine holds, exactly as the federal return assembles them.
    let (partners, problems) = pc::partners_for_year_with_problems(conn, year);
    let filings: Vec<PartnerFiling> = partners
        .into_iter()
        .map(|partner| PartnerFiling {
            tin: pc::get_tin(conn, &partner.partner_id),
            partner,
        })
        .collect();

    let mut bundle = build(&profile, &filings, &federal, settings)?;
    bundle.warnings.extend(problems);
    Ok(bundle)
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
        (f::L20_GUARANTEED, figs.line20),
        (f::L23_INCOME, figs.line23),
        (f::L34_TOTAL_SUBTRACT, 0),
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
    write_money(doc, map, f::L52_EXEMPTION, 0, warnings)?; // no exemption for a partnership
    write_money(doc, map, f::L53_NET_INCOME, line53, warnings)?;

    write_money(doc, map, f::L54_REPLACEMENT, line54, warnings)?;
    write_money(doc, map, f::L56_BEFORE_CREDITS, line54, warnings)?;
    write_money(doc, map, f::L58_NET_REPLACEMENT, line58, warnings)?;

    if settings.elects_pte_tax {
        let line61 = figs.line61.expect("il-only figures are complete");
        write_money(doc, map, f::L60_PTE_INCOME, line53.max(0), warnings)?;
        write_money(doc, map, f::L61_PTE_TAX, line61, warnings)?;
    }
    write_money(doc, map, f::L59_TOTAL_WITHHOLDING, 0, warnings)?;
    let line62 = figs.line62.expect("il-only figures are complete");
    write_money(doc, map, f::L62_TOTAL_TAX, line62, warnings)?;
    write_money(doc, map, f::L64_TOTAL, line62, warnings)?; // line 63 penalty = 0
    Ok(())
}

fn fill_schedule_b(
    doc: &mut Document,
    map: &FieldMap,
    profile: &BusinessProfile,
    partners: &[PartnerFiling],
    figs: &Figures,
    warnings: &mut Vec<String>,
) -> Result<(), FormError> {
    let (fein2, fein7) = split_fein(&profile.ein);
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
        // Column B is a one-character Illinois partner-type code (see the form's
        // instructions), not the federal free-text entity type — so it is left
        // blank rather than filled with a code we might get wrong. Named in the
        // caveat below.
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
        if !super::schedule_b1::is_individual_or_estate(p) {
            set_check(doc, map, &member(n, M_COL_D_SUBJECT), M_COL_D_ON)?;
        }

        // Column E — the member's share of base income (line 35).
        let share = share_of(figs.line35, p.shares.profit_ppm);
        write_money(doc, map, &member(n, M_COL_E_SHARE), share, warnings)?;

        if filing.tin.is_none() {
            warnings.push(format!(
                "Illinois Schedule B: no identifying number is held on this machine for {}, so \
                 column C is blank.",
                p.name
            ));
        }
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
fn caveats(profile: &BusinessProfile, settings: &Il1065Settings, partner_count: usize) -> Vec<String> {
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
         municipal interest, Illinois taxes deducted, special depreciation, related-party \
         expenses) and subtractions (U.S. Treasury interest, and the rest of Step 5) are left \
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
        "Illinois Schedule B, Section B: column B (the one-character partner-type code) is left \
         blank — enter it from the instructions. Column D (subject to replacement tax) is checked \
         for entity partners and clear for individuals; verify each. Columns F–L (pass-through \
         withholding and credits) and Section A totals are left blank."
            .to_string(),
    );

    if partner_count == 0 {
        out.push("No partners are recorded, so Illinois Schedule B is blank.".to_string());
    }

    out
}

/// Split an EIN `NN-NNNNNNN` into its two-digit and seven-digit halves, the way
/// the form's two boxes want it. A value without the hyphen is split by position
/// rather than refused — the return is more use with the number in it.
fn split_fein(ein: &str) -> (String, String) {
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
    use chrono::NaiveDate;

    /// Every text box this module writes to, so a form revision that renamed one
    /// fails a test rather than silently dropping a figure.
    fn all_text_fields() -> Vec<String> {
        let mut v: Vec<String> = [
            f::LEGAL_NAME, f::MAILING_ADDRESS, f::MAILING_CITY, f::MAILING_STATE, f::MAILING_ZIP,
            f::FEIN_2, f::FEIN_7, f::NAICS, f::RECORDS_CITY, f::RECORDS_STATE, f::RECORDS_ZIP,
            f::L1_ORDINARY, f::L2_RENTAL_RE, f::L3_OTHER_RENTAL, f::L4_PORTFOLIO, f::L5_1231,
            f::L7_TOTAL_ORDINARY, f::L8_CHARITABLE, f::L9_SECTION179, f::L10_INVEST_INTEREST,
            f::L12_ADD_8_11, f::L13_UNMODIFIED_BASE, f::L14_FROM_L13, f::L20_GUARANTEED,
            f::L23_INCOME, f::L34_TOTAL_SUBTRACT, f::L35_BASE_INCOME, f::L36_NONBUSINESS,
            f::L37_NONUNITARY, f::L38_ADD_36_37, f::L39_BUSINESS, f::L47_BASE, f::L49_AFTER_NLD,
            f::L50_FROM_L35, f::L51_RATIO_WHOLE, f::L51_RATIO_FRAC, f::L52_EXEMPTION,
            f::L53_NET_INCOME, f::L54_REPLACEMENT, f::L56_BEFORE_CREDITS, f::L58_NET_REPLACEMENT,
            f::L59_TOTAL_WITHHOLDING, f::L60_PTE_INCOME, f::L61_PTE_TAX, f::L62_TOTAL_TAX,
            f::L64_TOTAL, f::SCHB_NAME, f::SCHB_FEIN_2, f::SCHB_FEIN_7, f::SCHA_NAME, f::SCHA_FEIN_2,
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
        }
        v
    }

    /// Every checkbox/radio this module ticks, with the on-state it ticks it to.
    fn all_check_fields() -> Vec<(String, String)> {
        let mut v = vec![
            (f::PTE_BOX.to_string(), f::PTE_BOX_ON.to_string()),
            (f::INSIDE_OUTSIDE.to_string(), f::INSIDE_ON.to_string()),
            (f::INSIDE_OUTSIDE.to_string(), f::OUTSIDE_ON.to_string()),
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
            assert!(map.find(&name).is_some(), "il1065.pdf has no text field {name:?}");
        }
        for (name, _) in all_check_fields() {
            assert!(map.find(&name).is_some(), "il1065.pdf has no checkbox {name:?}");
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
        // 1.5% of 100,000 = 1,500.
        assert_eq!(figs.line54, Some(1_500));
        assert_eq!(figs.line58, Some(1_500));
        assert_eq!(figs.line61, Some(0));
        assert_eq!(figs.line62, Some(1_500));
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
        let s = Il1065Settings { apportions_outside_illinois: false, elects_pte_tax: true };
        let figs = figures(&fed, &s);
        assert_eq!(figs.line54, Some(3_000)); // 1.5%
        assert_eq!(figs.line61, Some(9_900)); // 4.95% of 200,000
        assert_eq!(figs.line62, Some(12_900));
    }

    #[test]
    fn apportioning_leaves_the_tax_for_a_person() {
        let fed = federal_ordinary(100_000);
        let s = Il1065Settings { apportions_outside_illinois: true, elects_pte_tax: false };
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
        let partners = vec![PartnerFiling { partner: p, tin: Some("123-45-6789".into()) }];
        let bundle = build(&profile(), &partners, &fed, &Il1065Settings::default()).unwrap();

        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(get_value(&doc, &map, f::LEGAL_NAME).as_deref(), Some("Prairie Partners LLC"));
        assert_eq!(get_value(&doc, &map, f::FEIN_2).as_deref(), Some("37"));
        assert_eq!(get_value(&doc, &map, f::FEIN_7).as_deref(), Some("1234567"));
        assert_eq!(get_value(&doc, &map, f::L1_ORDINARY).as_deref(), Some("100,000"));
        assert_eq!(get_value(&doc, &map, f::L35_BASE_INCOME).as_deref(), Some("100,000"));
        assert_eq!(get_value(&doc, &map, f::L54_REPLACEMENT).as_deref(), Some("1,500"));

        // Schedule B row 1: the partner and their 60% share of base income.
        assert_eq!(get_value(&doc, &map, &member(1, M_NAME)).as_deref(), Some("Dana Individual"));
        assert_eq!(get_value(&doc, &map, &member(1, M_COL_C_TIN)).as_deref(), Some("123456789"));
        assert_eq!(get_value(&doc, &map, &member(1, M_COL_E_SHARE)).as_deref(), Some("60,000"));
    }

    #[test]
    fn an_entity_partner_is_flagged_subject_to_replacement_tax() {
        let fed = federal_ordinary(100_000);
        let individual = PartnerFiling { partner: partner("Al", "Individual", 50.0), tin: None };
        let corp = PartnerFiling { partner: partner("Holdings LLC", "Partnership", 50.0), tin: None };
        let bundle = build(&profile(), &[individual, corp], &fed, &Il1065Settings::default()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        // Member 1 individual: box D clear (unticked → no value). Member 2 entity:
        // box D ticked to its "/Yes" on-state.
        assert_eq!(get_value(&doc, &map, &member(1, M_COL_D_SUBJECT)), None);
        assert_eq!(get_value(&doc, &map, &member(2, M_COL_D_SUBJECT)).as_deref(), Some("/Yes"));
    }

    #[test]
    fn more_than_three_partners_warns_about_a_continuation_page() {
        let fed = federal_ordinary(100_000);
        let partners: Vec<PartnerFiling> = (0..4)
            .map(|i| PartnerFiling { partner: partner(&format!("P{i}"), "Individual", 25.0), tin: None })
            .collect();
        let bundle = build(&profile(), &partners, &fed, &Il1065Settings::default()).unwrap();
        assert!(bundle.warnings.iter().any(|w| w.contains("continuation page")), "{:?}", bundle.warnings);
    }

    #[test]
    fn a_non_illinois_address_is_flagged() {
        let fed = federal_ordinary(100_000);
        let mut prof = profile();
        prof.address.state = "TX".into();
        let bundle = build(&prof, &[], &fed, &Il1065Settings::default()).unwrap();
        assert!(bundle.warnings.iter().any(|w| w.contains("not IL")), "{:?}", bundle.warnings);
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
                JournalLineData { line_id: "e1-0".into(), account_id: "cash".into(), amount: 10_000_000, currency: "USD".into(), exchange_rate: None, memo: None },
                JournalLineData { line_id: "e1-1".into(), account_id: "sales".into(), amount: -10_000_000, currency: "USD".into(), exchange_rate: None, memo: None },
            ],
            reference: None,
            source: None,
        };
        let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
        store.apply_projection(&stored).unwrap();

        // Map sales to gross receipts, with no expenses — ordinary income = 100,000.
        set_account_line(store.connection(), "sales", "l1a").unwrap();

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

        let bundle =
            build_from_ledger(store.connection(), FORM_TAX_YEAR, &Il1065Settings::default()).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();
        let map = field_map(&doc);
        assert_eq!(get_value(&doc, &map, f::L1_ORDINARY).as_deref(), Some("100,000"));
        assert_eq!(get_value(&doc, &map, f::L35_BASE_INCOME).as_deref(), Some("100,000"));
        assert_eq!(get_value(&doc, &map, f::L54_REPLACEMENT).as_deref(), Some("1,500"));
        // The sole partner's 100% share of base income lands on Schedule B.
        assert_eq!(get_value(&doc, &map, &member(1, M_COL_E_SHARE)).as_deref(), Some("100,000"));
    }
}
