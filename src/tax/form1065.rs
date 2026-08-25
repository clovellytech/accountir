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
    let computed = super::lines::compute(&statement, &super::lines::load_mapping(conn));
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

    // --- one K-1 per partner ---
    for (i, filing) in filed.iter().enumerate() {
        let mut sched = Document::load_mem(F1065_SK1)?;
        strip_xfa(&mut sched);
        // Namespace this copy before anything is written into it, so partner
        // two's boxes are not partner one's under another name.
        namespace_fields(&mut sched, &k1_namespace(i + 1));
        let smap = field_map(&sched);
        fill_k1(
            &mut sched,
            &smap,
            &req.profile,
            filing,
            year_start,
            year_end,
        )?;
        append_document(&mut doc, sched)?;
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

fn fill_k1(
    doc: &mut Document,
    map: &FieldMap,
    profile: &BusinessProfile,
    filing: &PartnerFiling,
    year_start: NaiveDate,
    year_end: NaiveDate,
) -> Result<(), FormError> {
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
    Ok(())
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
        let get = |n: &str| {
            acroform::get_value_in(&doc, &map, FORM_ROOT, n)
                .unwrap_or_default()
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

        assert_eq!(get(f1065::L1A_GROSS_RECEIPTS), "987654321");
        assert_eq!(get(f1065::L9_SALARIES), "123456789");
        assert_eq!(
            get(f1065::L23_ORDINARY_INCOME),
            "864197532",
            "987654321 - 123456789"
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
