//! Recording that a set of books is a sole proprietorship, and who owns it.
//!
//! # Where the social security number lives
//!
//! Everything here goes into the event log except one field, and it is the same
//! exception `partnership_commands` makes for a partner's TIN: the number is
//! written to `sole_proprietor_tin`, an ordinary local table, and never to an
//! event.
//!
//! The log is replicated in full to every member's machine and is append-only, so
//! an SSN written into it is that SSN on every other machine forever with no way
//! to take it back. For a sole proprietorship the argument is stronger than for a
//! partnership: a partnership's identifying number is usually an EIN, a number
//! about a business, where a sole proprietor's is their own social security
//! number and the whole return is about one person.
//!
//! The cost is the same one accepted there and worth naming again: SSNs do not
//! sync. A member who has not entered one locally produces a Schedule C with the
//! number box blank — a form you can see is incomplete — rather than one carrying
//! a number that reached them by a route nobody intended.

use chrono::Datelike;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::domain::{is_valid_tin, AccountingMethod, BusinessType, SoleProprietor};
use crate::events::types::{Event, SoleProprietorData, StoredEvent};
use crate::store::event_store::EventStore;

#[derive(Debug, Error)]
pub enum SoleProprietorError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("Invalid data: {0}")]
    Invalid(String),
    #[error("These books are a {0}, so there is no sole proprietor to record")]
    NotASoleProprietorship(&'static str),
}

// ---------------------------------------------------------------------------
// Which return these books file
// ---------------------------------------------------------------------------

/// What these books file. Defaults to a partnership, which is what every book
/// that has never been asked has always filed.
pub fn business_type(conn: &Connection) -> BusinessType {
    conn.query_row(
        "SELECT business_type FROM business_profile WHERE id = 'default'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|s| BusinessType::parse(&s))
    .unwrap_or_default()
}

/// Say which return these books file.
///
/// Does **not** touch the account-to-line mappings. Each return keeps its own
/// (see [`crate::tax::ReturnForm`]), so switching changes which set the books
/// are read through and leaves both as they were — the Form 1065 assignments
/// are still there, intact, on the day the books file a partnership return
/// again.
pub fn set_business_type(
    store: &mut EventStore,
    user_id: &str,
    business_type: BusinessType,
) -> Result<StoredEvent, SoleProprietorError> {
    append(
        store,
        user_id,
        Event::BusinessTypeSet {
            business_type: business_type.as_str().to_string(),
        },
    )
}

// ---------------------------------------------------------------------------
// The owner
// ---------------------------------------------------------------------------

pub fn get_proprietor(conn: &Connection) -> Option<SoleProprietor> {
    let row = conn
        .query_row(
            "SELECT name, accounting_method, accounting_method_other
               FROM sole_proprietor WHERE id = 'default'",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .ok()
        .flatten()?;

    Some(SoleProprietor {
        name: row.0,
        accounting_method: AccountingMethod::parse(&row.1)?,
        accounting_method_other: row.2,
    })
}

/// Record who owns the business.
pub fn set_proprietor(
    store: &mut EventStore,
    user_id: &str,
    proprietor: &SoleProprietor,
) -> Result<StoredEvent, SoleProprietorError> {
    if proprietor.name.trim().is_empty() {
        return Err(SoleProprietorError::Invalid(
            "the proprietor's name is required — Schedule C is attached to that person's Form \
             1040 and the IRS pairs the two by name and number"
                .to_string(),
        ));
    }
    if proprietor.accounting_method == AccountingMethod::Other
        && proprietor
            .accounting_method_other
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
    {
        return Err(SoleProprietorError::Invalid(
            "line F(3) asks which other accounting method — name it".to_string(),
        ));
    }

    append(
        store,
        user_id,
        Event::SoleProprietorSet(Box::new(SoleProprietorData {
            name: proprietor.name.trim().to_string(),
            accounting_method: proprietor.accounting_method.as_str().to_string(),
            accounting_method_other: proprietor
                .accounting_method_other
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        })),
    )
}

// ---------------------------------------------------------------------------
// The number that is not in the log
// ---------------------------------------------------------------------------

/// The proprietor's SSN, if this machine holds one.
pub fn get_ssn(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT ssn FROM sole_proprietor_tin WHERE id = 'default'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Store the proprietor's SSN on this machine only.
///
/// A plain SQL write with no event behind it, which is the whole point — see the
/// module docs. Validated in the same shape a partner's TIN is, so a
/// transposition is caught here rather than on the form.
pub fn set_ssn(conn: &Connection, ssn: &str) -> Result<(), SoleProprietorError> {
    let ssn = ssn.trim();
    if ssn.is_empty() {
        return clear_ssn(conn);
    }
    if !is_valid_tin(ssn) {
        return Err(SoleProprietorError::Invalid(format!(
            "{ssn:?} is not a social security number — nine digits, usually written \
             NNN-NN-NNNN"
        )));
    }
    conn.execute(
        "INSERT OR REPLACE INTO sole_proprietor_tin (id, ssn, updated_at)
         VALUES ('default', ?1, datetime('now'))",
        params![ssn],
    )
    .map_err(|e| SoleProprietorError::Store(e.to_string()))?;
    Ok(())
}

pub fn clear_ssn(conn: &Connection) -> Result<(), SoleProprietorError> {
    conn.execute("DELETE FROM sole_proprietor_tin WHERE id = 'default'", [])
        .map_err(|e| SoleProprietorError::Store(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Schedule C's own questions
// ---------------------------------------------------------------------------

/// Answer one Schedule C question for one tax year.
///
/// An empty value clears the answer rather than storing one, because unanswered
/// and "No" are different states on this form.
pub fn set_answer(
    store: &mut EventStore,
    user_id: &str,
    tax_year: i32,
    answer_key: &str,
    value: &str,
) -> Result<StoredEvent, SoleProprietorError> {
    if crate::tax::schedule_c::question(answer_key).is_none() {
        return Err(SoleProprietorError::Invalid(format!(
            "no Schedule C question has key {answer_key:?}"
        )));
    }
    let event = if value.trim().is_empty() {
        Event::ScheduleCAnswerCleared {
            tax_year,
            answer_key: answer_key.to_string(),
        }
    } else {
        Event::ScheduleCAnswerSet {
            tax_year,
            answer_key: answer_key.to_string(),
            value: value.trim().to_string(),
        }
    };
    append(store, user_id, event)
}

// ---------------------------------------------------------------------------
// Building the return
// ---------------------------------------------------------------------------

/// What a Schedule C needs that no ledger holds, as typed on the Schedule C page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScheduleCInputs {
    /// Line 30, business use of the home. `None` is "not worked out yet", which
    /// the build reports; it is not the same as zero.
    pub home_office_dollars: Option<i64>,
    /// Wages and taxable income from the owner's *other* active trades or
    /// businesses, for the §179 business income limit (Form 4562 line 11). `None`
    /// means nobody has worked it out, and the limit is then this business's
    /// income alone — the cautious reading, and said out loud.
    pub other_business_income_dollars: Option<i64>,
    /// §179 disallowed last year and carried into this one (Form 4562 line 10).
    pub section_179_carryover_dollars: Option<i64>,
}

/// The year's figures as the form will carry them, before any PDF is drawn.
pub struct Figures {
    pub computed: crate::tax::schedule_c::Computed,
    /// Part I of Form 4562, when there is any §179 to apply.
    pub section_179: Option<crate::tax::form4562::Section179Outcome>,
    /// The inputs Part I was applied with, for the Form 4562 that shows it.
    pub section_179_limit: crate::tax::form4562::Section179Limit,
    /// What the depreciation register and the ledger disagree about, and what
    /// the §179 limit did to line 13.
    pub warnings: Vec<String>,
}

/// Compute a year's Schedule C figures: the ledger through Schedule C's own
/// mapping, checked against the depreciation register, with the §179 business
/// income limit applied to line 13.
///
/// The one computation the Schedule C page's preview and the PDF both read, so
/// the two cannot disagree about a figure.
///
/// # Why line 13 can differ from the ledger
///
/// The register posts the whole §179 election to the books. On a partnership
/// return that is right — the limits are applied on each partner's return — but a
/// sole proprietor applies them here, on Form 4562, and only line 12 of that form
/// is deducted. The rest carries to next year. So line 13 is the ledger's
/// depreciation with the §179 part replaced by what Part I allows, and a warning
/// says by how much. It is only done when line 13 and the register agree: if they
/// do not, the ledger's §179 is not known to be the register's, and adjusting it
/// would be guessing.
pub fn figures(
    conn: &Connection,
    year: i32,
    inputs: &ScheduleCInputs,
) -> Result<Figures, SoleProprietorError> {
    use crate::tax::form4562::{section_179_outcome, Section179Limit};
    use crate::tax::lines::{cents_to_dollars, format_dollars};

    let (start, end) = (
        chrono::NaiveDate::from_ymd_opt(year, 1, 1).expect("January 1 exists in every year"),
        chrono::NaiveDate::from_ymd_opt(year, 12, 31).expect("December 31 exists in every year"),
    );
    let statement = crate::queries::reports::Reports::new(conn)
        .income_statement(start, end)
        .map_err(|e| SoleProprietorError::Store(format!("income statement: {e}")))?;

    // Schedule C's own assignments, inherited down the account tree: a parent
    // mapped to a line carries every child that does not say otherwise.
    let mapping = crate::tax::lines::load_effective_mapping_for(
        conn,
        crate::tax::ReturnForm::ScheduleC,
        year,
    );
    let limits = crate::tax::lines::load_effective_limits(conn, year);
    let mut computed = crate::tax::schedule_c::compute(&statement, &mapping, &limits);
    let mut warnings = Vec::new();

    // --- the register against the ledger --------------------------------
    //
    // The register computes the year; the ledger is what line 13 is filled from.
    // They agree only once the year is posted, and stop agreeing the moment an
    // asset is edited afterwards. Without this a year nobody posted files a
    // Schedule C with its depreciation quietly missing.
    let assets = crate::commands::depreciation_commands::list_assets(conn);
    let schedule = crate::tax::depreciation::compute_year(&assets, year);
    warnings.extend(schedule.warnings.iter().cloned());
    let register_179 = schedule.section_179_cents();
    let register_dollars = cents_to_dollars(schedule.line_16a_cents() + register_179);
    let ledger_13 = computed.lines.get("sc13");
    let mut reconciled = true;
    if !schedule.rows.is_empty() {
        if !computed.lines.is_mapped("sc13") {
            if register_dollars != 0 {
                reconciled = false;
                warnings.push(format!(
                    "The depreciation register computes ${} of depreciation and §179 for {year}, \
                     but no account is mapped to line 13, so none of it reaches this return. \
                     Post {year} on the Depreciation page, then map the depreciation expense and \
                     §179 accounts to line 13 on this page.",
                    format_dollars(register_dollars)
                ));
            }
        } else if ledger_13 != register_dollars {
            reconciled = false;
            warnings.push(format!(
                "Line 13 carries ${} from the ledger, and the depreciation register computes ${} \
                 of depreciation and §179 for {year}. Either {year} has not been posted since the \
                 register last changed, or something else is mapped to line 13. The form is \
                 filled with the ledger's figure.",
                format_dollars(ledger_13),
                format_dollars(register_dollars)
            ));
        }
        if let Some(stale) = crate::commands::depreciation_commands::posting_is_stale(conn, year) {
            warnings.push(stale);
        }
    }

    // --- §179, limited by business income ---------------------------------
    let section_179_limit = Section179Limit {
        carryover_in_cents: inputs.section_179_carryover_dollars.unwrap_or(0).max(0) * 100,
        // Filled in below, once there is a profit to measure.
        business_income_cents: 0,
    };
    let mut figures = Figures {
        computed,
        section_179: None,
        section_179_limit,
        warnings,
    };
    if register_179 == 0 && section_179_limit.carryover_in_cents == 0 {
        return Ok(figures);
    }
    if !reconciled {
        figures.warnings.push(
            "The §179 business income limit was not applied to line 13, because line 13 and the \
             depreciation register disagree — the ledger's §179 is not known to be the \
             register's. Post the year and check line 13 first."
                .to_string(),
        );
        return Ok(figures);
    }

    // Line 11 is figured without the §179 deduction: this business's profit with
    // the posted §179 added back, plus whatever else the owner earns actively.
    let lines = &figures.computed.lines;
    let profit_before_179 = lines.line_31(
        lines.get("sc48"),
        inputs.home_office_dollars.unwrap_or(0),
    ) + cents_to_dollars(register_179);
    let business_income =
        profit_before_179 + inputs.other_business_income_dollars.unwrap_or(0);
    figures.section_179_limit.business_income_cents = business_income * 100;
    let outcome = section_179_outcome(&schedule, figures.section_179_limit);

    let delta = cents_to_dollars(outcome.allowed_cents) - cents_to_dollars(register_179);
    if delta != 0 {
        figures.computed.lines.adjust("sc13", delta);
        figures.warnings.push(format!(
            "Line 13 is ${} rather than the ledger's ${}: Form 4562 allows ${} of §179 this year \
             (line 12) against ${} posted{}, and ${} carries to next year (line 13). Enter that \
             carryover on next year's Schedule C page.",
            format_dollars(ledger_13 + delta),
            format_dollars(ledger_13),
            format_dollars(cents_to_dollars(outcome.allowed_cents)),
            format_dollars(cents_to_dollars(register_179)),
            if outcome.carryover_in_cents > 0 {
                format!(
                    " plus ${} carried in",
                    format_dollars(cents_to_dollars(outcome.carryover_in_cents))
                )
            } else {
                String::new()
            },
            format_dollars(cents_to_dollars(outcome.carryover_out_cents))
        ));
    }
    if inputs.other_business_income_dollars.is_none() && outcome.carryover_out_cents > 0 {
        figures.warnings.push(
            "The §179 business income limit counts this business alone, because no wages or \
             other business income were entered. The limit includes wages and the profit of \
             every active trade or business — enter them on the Schedule C page and more of \
             the §179 may be deductible this year."
                .to_string(),
        );
    }
    figures.section_179 = Some(outcome);
    Ok(figures)
}

/// Build a Schedule C for `year` from the books, with Form 4562 behind it when
/// the depreciation register has anything in it.
///
/// Refuses on a partnership rather than producing a Schedule C for one. A
/// partnership that filed this form would be filing the wrong return entirely,
/// and the form gives no hint of it — every box would be filled and plausible.
pub fn build_from_ledger(
    conn: &Connection,
    year: i32,
    inputs: &ScheduleCInputs,
) -> Result<crate::tax::schedule_c::Bundle, SoleProprietorError> {
    let kind = business_type(conn);
    if kind != BusinessType::SoleProprietorship {
        return Err(SoleProprietorError::NotASoleProprietorship(kind.label()));
    }

    // Named where they are, because the last version of this message said only
    // that they were missing. One set of business details serves both returns
    // and they are edited in one place; a person filling in a Schedule C has no
    // reason to guess that place is Settings, still less that a panel headed
    // "partnership details" was the one they wanted.
    let profile = crate::commands::partnership_commands::get_profile(conn).ok_or_else(|| {
        SoleProprietorError::Invalid(
            "the business details have not been set. Schedule C takes its header from them — \
             the business name (line C), the address (line E), the business code (line B), the \
             EIN (line D) and the principal business (line A). They are on the Settings page, \
             under Business details, and are the same details Form 1065 uses."
                .to_string(),
        )
    })?;

    let figures = figures(conn, year, inputs)?;
    let proprietor = get_proprietor(conn);
    let ssn = get_ssn(conn);
    let answers = crate::tax::schedule_c::load(conn, year);

    let req = crate::tax::schedule_c::ScheduleCRequest {
        year,
        profile: &profile,
        proprietor: proprietor.as_ref(),
        ssn: ssn.as_deref(),
        answers: &answers,
        home_office_dollars: inputs.home_office_dollars,
    };
    let mut bundle = crate::tax::schedule_c::build(&req, &figures.computed)
        .map_err(|e| SoleProprietorError::Store(e.to_string()))?;
    bundle.warnings.extend(figures.warnings.iter().cloned());

    attach_form_4562(&mut bundle, conn, year, &profile, proprietor.as_ref(), ssn.as_deref(), &figures)?;

    // A year that has not been answered at all is worth saying once, rather than
    // producing a form whose questions are silently all blank.
    if answers.is_empty() {
        bundle.warnings.push(format!(
            "None of Schedule C's questions are answered for {year} — material participation, \
             whether the business started this year, and the two about Forms 1099. They are on \
             the form and a blank answer is not a No."
        ));
    }

    Ok(bundle)
}


/// Put Form 4562 — and the basis statement, when an asset's basis moved — behind
/// the Schedule C, headed with the owner's name and SSN as the 1040 is.
///
/// Built from the same register and the same Part I the figures used, so line 22
/// of the 4562 is line 13 of the Schedule C whenever the two are reconciled.
fn attach_form_4562(
    bundle: &mut crate::tax::schedule_c::Bundle,
    conn: &Connection,
    year: i32,
    profile: &crate::domain::BusinessProfile,
    proprietor: Option<&SoleProprietor>,
    ssn: Option<&str>,
    figures: &Figures,
) -> Result<(), SoleProprietorError> {
    use crate::tax::acroform::{append_document, namespace_fields};
    use crate::tax::form4562::{self, Filer};

    let store_err = |e: crate::tax::acroform::FormError| SoleProprietorError::Store(e.to_string());
    let assets = crate::commands::depreciation_commands::list_assets(conn);
    let schedule = crate::tax::depreciation::compute_year(&assets, year);
    if schedule.rows.is_empty() {
        return Ok(());
    }

    // Only when the Schedule C instructions require one: property placed in
    // service this year, or §179 — elected now or carried in. A year of nothing
    // but depreciation continuing on older assets is entered on line 13 with no
    // Form 4562, and filing one anyway is a form the IRS did not ask for.
    let required = schedule.placed_this_year().next().is_some()
        || schedule.section_179_cents() > 0
        || figures.section_179_limit.carryover_in_cents > 0;
    if !required {
        // Listed property is the one trigger the register cannot see: it
        // records no business-use percentage, so it cannot tell a delivery van
        // from a lathe.
        bundle.warnings.push(format!(
            "No Form 4562 is attached: nothing was placed in service in {year} and there is no \
             §179, so the depreciation goes on line 13 without one. If any asset is listed \
             property — a car, a truck, or other property used partly for personal purposes — \
             Form 4562 is required anyway, for Part V."
        ));
        return Ok(());
    }
    let name = proprietor
        .map(|p| p.name.as_str())
        .unwrap_or(profile.legal_name.as_str());
    let activity = crate::tax::schedule_c::form_4562_activity(profile, proprietor);
    let (filled, warnings) = form4562::build(
        profile,
        &schedule,
        &activity,
        year,
        Filer::SoleProprietor {
            name,
            ssn,
            limit: figures.section_179_limit,
        },
    )
    .map_err(store_err)?;
    bundle.warnings.extend(warnings);

    let statement = form4562::basis_statement(&schedule, name, ssn.unwrap_or(""))
        .map_err(store_err)?;
    if filled.is_none() && statement.is_none() {
        return Ok(());
    }

    let mut doc = lopdf::Document::load_mem(&bundle.pdf)
        .map_err(|e| SoleProprietorError::Store(e.to_string()))?;
    if let Some(mut filled) = filled {
        namespace_fields(&mut filled.document, crate::tax::form1065::F4562_NAMESPACE);
        append_document(&mut doc, filled.document).map_err(store_err)?;
    }
    if let Some(statement) = statement {
        append_document(&mut doc, statement).map_err(store_err)?;
    }
    let mut buf = Vec::new();
    doc.save_to(&mut buf)
        .map_err(|e| SoleProprietorError::Store(e.to_string()))?;
    bundle.pdf = buf;
    Ok(())
}

fn append(
    store: &mut EventStore,
    user_id: &str,
    event: Event,
) -> Result<StoredEvent, SoleProprietorError> {
    crate::commands::partnership_commands::append_event_locally(store, user_id, event)
        .map_err(|e| SoleProprietorError::Store(e.to_string()))
}

/// The year a return is being prepared for: last year, because a return is filed
/// for a year that has finished.
pub fn default_year() -> i32 {
    chrono::Local::now().date_naive().year() - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{AddressData, BusinessProfileData};

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        crate::store::migrations::SchemaStore::init_schema(&mut s).unwrap();
        s
    }

    fn with_profile() -> EventStore {
        let mut s = store();
        append(
            &mut s,
            "u",
            Event::BusinessProfileSet(Box::new(BusinessProfileData {
                legal_name: "Bunny Ears Art House".into(),
                address: AddressData {
                    street: "1808 W Summerdale Ave".into(),
                    suite: None,
                    city: "Chicago".into(),
                    state: "IL".into(),
                    postal_code: "60640".into(),
                    country: None,
                },
                ein: "12-3456789".into(),
                naics_code: "611610".into(),
                formation_date: chrono::NaiveDate::from_ymd_opt(2023, 4, 13).unwrap(),
                principal_activity: Some("Fine arts instruction".into()),
                principal_product: Some("Art classes".into()),
            })),
        )
        .unwrap();
        s
    }

    fn proprietor() -> SoleProprietor {
        SoleProprietor {
            name: "Jinny Choi".into(),
            accounting_method: AccountingMethod::Cash,
            accounting_method_other: None,
        }
    }

    /// A book that has never been asked files what it has always filed.
    #[test]
    fn the_default_is_a_partnership() {
        let s = store();
        assert_eq!(business_type(s.connection()), BusinessType::Partnership);
    }

    #[test]
    fn the_business_type_round_trips_through_the_log() {
        let mut s = store();
        set_business_type(&mut s, "u", BusinessType::SoleProprietorship).unwrap();
        assert_eq!(
            business_type(s.connection()),
            BusinessType::SoleProprietorship
        );
    }

    /// The type can be chosen before the header is ever filled in — somebody
    /// opening fresh books says what they are before they say where they are —
    /// so the write must not depend on a profile row existing.
    #[test]
    fn the_type_can_be_set_before_the_business_details_are() {
        let mut s = store();
        set_business_type(&mut s, "u", BusinessType::SoleProprietorship).unwrap();
        assert_eq!(
            business_type(s.connection()),
            BusinessType::SoleProprietorship
        );

        // And filling the header afterwards must not quietly undo it.
        let mut s2 = with_profile();
        set_business_type(&mut s2, "u", BusinessType::SoleProprietorship).unwrap();
        append(
            &mut s2,
            "u",
            Event::BusinessProfileSet(Box::new(BusinessProfileData {
                legal_name: "Bunny Ears Art House LLC".into(),
                address: AddressData {
                    street: "2 New Street".into(),
                    suite: None,
                    city: "Chicago".into(),
                    state: "IL".into(),
                    postal_code: "60640".into(),
                    country: None,
                },
                ein: "12-3456789".into(),
                naics_code: "611610".into(),
                formation_date: chrono::NaiveDate::from_ymd_opt(2023, 4, 13).unwrap(),
                principal_activity: None,
                principal_product: None,
            })),
        )
        .unwrap();
        assert_eq!(
            business_type(s2.connection()),
            BusinessType::SoleProprietorship,
            "correcting the address turned the books back into a partnership"
        );
    }

    #[test]
    fn the_proprietor_round_trips() {
        let mut s = store();
        set_proprietor(&mut s, "u", &proprietor()).unwrap();
        let back = get_proprietor(s.connection()).expect("recorded");
        assert_eq!(back.name, "Jinny Choi");
        assert_eq!(back.accounting_method, AccountingMethod::Cash);
    }

    #[test]
    fn a_nameless_proprietor_is_refused() {
        let mut s = store();
        let mut p = proprietor();
        p.name = "  ".into();
        assert!(set_proprietor(&mut s, "u", &p).is_err());
    }

    /// Line F(3) makes you name the method; a ticked "Other" with nothing beside
    /// it is incomplete on its face.
    #[test]
    fn an_other_accounting_method_has_to_be_named() {
        let mut s = store();
        let mut p = proprietor();
        p.accounting_method = AccountingMethod::Other;
        assert!(set_proprietor(&mut s, "u", &p).is_err());

        p.accounting_method_other = Some("Hybrid".into());
        assert!(set_proprietor(&mut s, "u", &p).is_ok());
    }

    /// The number is on this machine and nowhere else — no event carries it.
    #[test]
    fn the_ssn_is_stored_locally_and_never_reaches_the_log() {
        let mut s = with_profile();
        set_business_type(&mut s, "u", BusinessType::SoleProprietorship).unwrap();
        set_proprietor(&mut s, "u", &proprietor()).unwrap();
        set_ssn(s.connection(), "123-45-6789").unwrap();

        assert_eq!(get_ssn(s.connection()).as_deref(), Some("123-45-6789"));

        let events: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE payload LIKE '%123-45-6789%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0, "the SSN reached the event log");
    }

    #[test]
    fn a_malformed_ssn_is_refused_and_an_empty_one_clears() {
        let s = store();
        assert!(set_ssn(s.connection(), "12345").is_err());
        set_ssn(s.connection(), "123-45-6789").unwrap();
        set_ssn(s.connection(), "").unwrap();
        assert!(get_ssn(s.connection()).is_none());
    }

    /// A partnership must never produce a Schedule C: every box would be filled
    /// and plausible, and the return would be the wrong one entirely.
    #[test]
    fn a_partnership_cannot_build_a_schedule_c() {
        let s = with_profile();
        assert!(matches!(
            build_from_ledger(s.connection(), 2025, &ScheduleCInputs::default()),
            Err(SoleProprietorError::NotASoleProprietorship(_))
        ));
    }

    #[test]
    fn a_sole_proprietorship_builds_one() {
        let mut s = with_profile();
        set_business_type(&mut s, "u", BusinessType::SoleProprietorship).unwrap();
        set_proprietor(&mut s, "u", &proprietor()).unwrap();
        set_ssn(s.connection(), "123-45-6789").unwrap();

        let bundle = build_from_ledger(s.connection(), 2025, &ScheduleCInputs { home_office_dollars: Some(0), ..Default::default() }).expect("a Schedule C");
        assert!(bundle.pdf.len() > 1000);
        // No answers given, so the form says so rather than looking answered.
        assert!(
            bundle
                .warnings
                .iter()
                .any(|w| w.contains("blank answer is not a No")),
            "{:?}",
            bundle.warnings
        );
    }

    /// The order somebody actually works in, and the one that produced a
    /// confusing error: say what the books are first, fill the business details
    /// afterwards.
    ///
    /// Setting the type before any header exists writes a stub row — the type has
    /// to live somewhere and it lives on the profile row — whose formation date
    /// is empty and therefore unreadable. `get_profile` returns `None` for that,
    /// which is the right answer (the details genuinely are not set) but reached
    /// by a route worth pinning down: the stub must not survive as a half-header,
    /// and filling the form afterwards must produce a complete one without
    /// undoing the type.
    #[test]
    fn setting_the_type_before_the_details_leaves_a_buildable_book_once_they_are_filled() {
        let mut s = store();

        set_business_type(&mut s, "u", BusinessType::SoleProprietorship).unwrap();
        assert_eq!(
            business_type(s.connection()),
            BusinessType::SoleProprietorship
        );
        assert!(
            crate::commands::partnership_commands::get_profile(s.connection()).is_none(),
            "a stub row must not read as a filled-in header"
        );

        // And the failure a person meets at that point says where to go.
        let err = match build_from_ledger(s.connection(), 2025, &ScheduleCInputs::default()) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("built a Schedule C with no business details"),
        };
        assert!(err.contains("Settings"), "{err}");
        assert!(err.contains("Business details"), "{err}");

        // Filling the header afterwards completes it and leaves the type alone.
        append(
            &mut s,
            "u",
            Event::BusinessProfileSet(Box::new(BusinessProfileData {
                legal_name: "Bunny Ears Art House".into(),
                address: AddressData {
                    street: "1808 W Summerdale Ave".into(),
                    suite: None,
                    city: "Chicago".into(),
                    state: "IL".into(),
                    postal_code: "60640".into(),
                    country: None,
                },
                ein: "12-3456789".into(),
                naics_code: "611610".into(),
                formation_date: chrono::NaiveDate::from_ymd_opt(2023, 4, 13).unwrap(),
                principal_activity: Some("Fine arts instruction".into()),
                principal_product: None,
            })),
        )
        .unwrap();

        assert!(crate::commands::partnership_commands::get_profile(s.connection()).is_some());
        assert_eq!(
            business_type(s.connection()),
            BusinessType::SoleProprietorship,
            "filling the header turned the books back into a partnership"
        );
        set_proprietor(&mut s, "u", &proprietor()).unwrap();
        assert!(build_from_ledger(s.connection(), 2025, &ScheduleCInputs { home_office_dollars: Some(0), ..Default::default() }).is_ok());
    }

    /// Many sole proprietors have never applied for an EIN, and the return is
    /// identified by the owner's SSN regardless. Demanding one made the business
    /// details unsaveable for exactly the businesses this feature is for.
    ///
    /// What is optional is *having* an EIN, not reporting one you have: an EIN
    /// the business holds goes on Schedule C line D, which
    /// `tax::schedule_c::tests::a_sole_proprietors_ein_is_reported_on_line_d`
    /// covers.
    #[test]
    fn a_sole_proprietor_can_file_with_no_ein_at_all() {
        let mut s = store();
        set_business_type(&mut s, "u", BusinessType::SoleProprietorship).unwrap();
        append(
            &mut s,
            "u",
            Event::BusinessProfileSet(Box::new(BusinessProfileData {
                legal_name: "Bunny Ears Art House".into(),
                address: AddressData {
                    street: "1808 W Summerdale Ave".into(),
                    suite: None,
                    city: "Chicago".into(),
                    state: "IL".into(),
                    postal_code: "60640".into(),
                    country: None,
                },
                ein: String::new(),
                naics_code: "611610".into(),
                formation_date: chrono::NaiveDate::from_ymd_opt(2023, 4, 13).unwrap(),
                principal_activity: Some("Fine arts instruction".into()),
                principal_product: None,
            })),
        )
        .expect("an absent EIN is a legitimate profile");
        set_proprietor(&mut s, "u", &proprietor()).unwrap();

        let bundle = build_from_ledger(s.connection(), 2025, &ScheduleCInputs { home_office_dollars: Some(0), ..Default::default() }).expect("a Schedule C");
        assert!(bundle.pdf.len() > 1000);
        // And nothing complains about the missing number, because nothing should.
        assert!(
            !bundle.warnings.iter().any(|w| w.contains("EIN")),
            "{:?}",
            bundle.warnings
        );
    }

    /// A malformed EIN is still refused — absence and a typo are different.
    #[test]
    fn an_ein_that_is_present_and_malformed_is_still_refused() {
        use crate::commands::partnership_commands::check_set_profile_pure;
        use crate::domain::{Address, BusinessProfile};

        let profile = |ein: &str| BusinessProfile {
            legal_name: "Bunny Ears Art House".into(),
            address: Address {
                street: "1808 W Summerdale Ave".into(),
                suite: None,
                city: "Chicago".into(),
                state: "IL".into(),
                postal_code: "60640".into(),
                country: None,
            },
            ein: ein.into(),
            naics_code: "611610".into(),
            formation_date: chrono::NaiveDate::from_ymd_opt(2023, 4, 13).unwrap(),
            principal_activity: None,
            principal_product: None,
        };

        assert!(
            check_set_profile_pure(&profile("")).is_ok(),
            "absent is allowed"
        );
        assert!(check_set_profile_pure(&profile("12-3456789")).is_ok());
        assert!(
            check_set_profile_pure(&profile("123456789")).is_err(),
            "no hyphen"
        );
        assert!(
            check_set_profile_pure(&profile("12-345678")).is_err(),
            "too short"
        );
    }

    #[test]
    fn an_unknown_question_key_is_refused() {
        let mut s = store();
        assert!(set_answer(&mut s, "u", 2025, "not-a-question", "yes").is_err());
        assert!(set_answer(&mut s, "u", 2025, "g", "yes").is_ok());
    }

    /// An empty value clears rather than storing one, because unanswered and
    /// "No" are different states on this form.
    #[test]
    fn clearing_an_answer_is_not_the_same_as_answering_no() {
        let mut s = store();
        set_answer(&mut s, "u", 2025, "g", "yes").unwrap();
        assert_eq!(
            crate::tax::schedule_c::load(s.connection(), 2025).get("g"),
            Some("yes")
        );
        set_answer(&mut s, "u", 2025, "g", "").unwrap();
        assert_eq!(
            crate::tax::schedule_c::load(s.connection(), 2025).get("g"),
            None
        );
    }
}
