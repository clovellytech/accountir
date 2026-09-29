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
/// Does **not** touch the existing account-to-line mappings, deliberately. They
/// are still there, still pointing at the other form's lines, and
/// [`stale_mappings`] is what reports them — because throwing away somebody's
/// mapping work on a click is not recoverable, and a mapping that is visibly
/// wrong is easier to live with than one that silently vanished.
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

/// Account mappings that point at the other return's lines.
///
/// Switching what a set of books files leaves every mapping aimed at a form this
/// book no longer produces. Those accounts then reach no line, which
/// `lines::sum_by_line` reports as money missing from the return — correct, but
/// after the fact and one warning for all of them. This says it up front, per
/// account, at the point somebody can fix it.
pub fn stale_mappings(conn: &Connection) -> Vec<(String, String, &'static str)> {
    let want = match business_type(conn) {
        BusinessType::Partnership => "Form 1065",
        BusinessType::SoleProprietorship => "Schedule C",
        BusinessType::Individual => "Form 1040",
    };
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT m.account_id, COALESCE(a.name, m.account_id), m.line_key
           FROM tax_line_mappings m
           LEFT JOIN accounts a ON a.id = m.account_id
          ORDER BY a.account_number",
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    }) {
        for (account_id, name, key) in rows.flatten() {
            match crate::tax::line_key_form(&key) {
                Some(form) if form != want => out.push((account_id, name, form)),
                _ => {}
            }
        }
    }
    out
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

/// Build a Schedule C for `year` from the books.
///
/// Refuses on a partnership rather than producing a Schedule C for one. A
/// partnership that filed this form would be filing the wrong return entirely,
/// and the form gives no hint of it — every box would be filled and plausible.
pub fn build_from_ledger(
    conn: &Connection,
    year: i32,
    home_office_dollars: Option<i64>,
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

    let (start, end) = (
        chrono::NaiveDate::from_ymd_opt(year, 1, 1).expect("January 1 exists in every year"),
        chrono::NaiveDate::from_ymd_opt(year, 12, 31).expect("December 31 exists in every year"),
    );
    let statement = crate::queries::reports::Reports::new(conn)
        .income_statement(start, end)
        .map_err(|e| SoleProprietorError::Store(format!("income statement: {e}")))?;

    let mapping = crate::tax::lines::load_effective_mapping(conn, year);
    let limits = crate::tax::lines::load_effective_limits(conn, year);
    let computed = crate::tax::schedule_c::compute(&statement, &mapping, &limits);
    let proprietor = get_proprietor(conn);
    let ssn = get_ssn(conn);
    let answers = crate::tax::schedule_c::load(conn, year);

    let req = crate::tax::schedule_c::ScheduleCRequest {
        year,
        profile: &profile,
        proprietor: proprietor.as_ref(),
        ssn: ssn.as_deref(),
        answers: &answers,
        home_office_dollars,
    };
    let mut bundle = crate::tax::schedule_c::build(&req, &computed)
        .map_err(|e| SoleProprietorError::Store(e.to_string()))?;

    // Mappings aimed at the other form, reported per account. `sum_by_line`
    // already says money is missing; this says which form it went looking on,
    // which is the part that tells somebody what to do about it.
    let stale = stale_mappings(conn);
    if !stale.is_empty() {
        let named: Vec<String> = stale
            .iter()
            .map(|(_, name, form)| format!("{name} ({form})"))
            .collect();
        bundle.warnings.push(format!(
            "{} account(s) are still mapped to lines of a form these books no longer file, so \
             none of their balances reach this return: {}. Remap them to Schedule C lines.",
            stale.len(),
            named.join(", ")
        ));
    }

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
            build_from_ledger(s.connection(), 2025, None),
            Err(SoleProprietorError::NotASoleProprietorship(_))
        ));
    }

    #[test]
    fn a_sole_proprietorship_builds_one() {
        let mut s = with_profile();
        set_business_type(&mut s, "u", BusinessType::SoleProprietorship).unwrap();
        set_proprietor(&mut s, "u", &proprietor()).unwrap();
        set_ssn(s.connection(), "123-45-6789").unwrap();

        let bundle = build_from_ledger(s.connection(), 2025, Some(0)).expect("a Schedule C");
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
        let err = match build_from_ledger(s.connection(), 2025, None) {
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
        assert!(build_from_ledger(s.connection(), 2025, Some(0)).is_ok());
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

        let bundle = build_from_ledger(s.connection(), 2025, Some(0)).expect("a Schedule C");
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
