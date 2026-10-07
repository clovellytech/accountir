//! One tax year's filing facts — see [`PersonalTaxProfileData`].
//!
//! Recorded rather than inferred: filing status, a household's ages and
//! dependents, the state of residence and the estimated tax paid are facts about a
//! person on December 31 that no ledger holds. A return computed without them would
//! be computed for somebody else.

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

use crate::commands::partnership_commands as pc;
use crate::events::types::{Event, PersonalTaxProfileData, StoredEvent};
use crate::store::event_store::EventStore;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("{0}")]
    Invalid(String),
}

/// Record a year's profile, replacing any the year already had.
///
/// Every account a rental property or an extra income account names has to exist:
/// a figure read from an account that is not there is a zero that looks like an
/// answer.
pub fn set_profile(
    store: &mut EventStore,
    user_id: &str,
    profile: &PersonalTaxProfileData,
) -> Result<StoredEvent, ProfileError> {
    check_accounts(store.connection(), profile)?;
    pc::append_event_locally(
        store,
        user_id,
        Event::PersonalTaxProfileSet(Box::new(profile.clone())),
    )
    .map_err(|e| ProfileError::Store(e.to_string()))
}

/// The accounts a profile names, each checked to exist. Shared with the hosted route,
/// which checks against the group's books before appending.
pub fn check_accounts(
    conn: &Connection,
    profile: &PersonalTaxProfileData,
) -> Result<(), ProfileError> {
    let named = profile
        .rental_properties
        .iter()
        .flat_map(|p| p.income_account_ids.iter().chain(&p.expense_account_ids))
        .chain(&profile.extra_interest_account_ids)
        .chain(&profile.extra_dividend_account_ids);
    for id in named {
        let exists: bool = conn
            .query_row("SELECT 1 FROM accounts WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .map_err(|e| ProfileError::Store(e.to_string()))?
            .is_some();
        if !exists {
            return Err(ProfileError::Invalid(format!(
                "the profile names account {id}, which these books do not have"
            )));
        }
    }
    Ok(())
}

/// The profile recorded for a year, if there is one.
pub fn get_profile(conn: &Connection, tax_year: i32) -> Option<PersonalTaxProfileData> {
    let json: String = conn
        .query_row(
            "SELECT profile FROM personal_tax_profiles WHERE tax_year = ?1",
            [tax_year],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()?;
    serde_json::from_str(&json).ok()
}

/// Every year with a profile, newest first.
pub fn profile_years(conn: &Connection) -> Vec<i32> {
    let Ok(mut stmt) =
        conn.prepare("SELECT tax_year FROM personal_tax_profiles ORDER BY tax_year DESC")
    else {
        return Vec::new();
    };
    stmt.query_map([], |r| r.get(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::events::types::{FilingStatus, RentalPropertyData};
    use crate::store::migrations::SchemaStore;

    fn books() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut store).unwrap();
        store
    }

    pub(crate) fn profile(year: i32, status: FilingStatus) -> PersonalTaxProfileData {
        PersonalTaxProfileData {
            tax_year: year,
            filing_status: status,
            taxpayer_65_or_older: false,
            taxpayer_blind: false,
            spouse_65_or_older: false,
            spouse_blind: false,
            qualifying_children: 0,
            other_dependents: 0,
            state: Some("IL".to_string()),
            federal_estimated_payments_cents: 0,
            state_estimated_payments_cents: 0,
            short_term_loss_carryover_cents: 0,
            long_term_loss_carryover_cents: 0,
            rental_properties: Vec::new(),
            extra_interest_account_ids: Vec::new(),
            extra_dividend_account_ids: Vec::new(),
        }
    }

    /// A year's profile is replaced whole, and each year keeps its own.
    #[test]
    fn each_year_has_one_profile_and_setting_it_again_replaces_it() {
        let mut store = books();
        set_profile(&mut store, "u", &profile(2025, FilingStatus::Single)).unwrap();
        set_profile(
            &mut store,
            "u",
            &profile(2026, FilingStatus::MarriedFilingJointly),
        )
        .unwrap();
        let mut again = profile(2025, FilingStatus::HeadOfHousehold);
        again.qualifying_children = 1;
        set_profile(&mut store, "u", &again).unwrap();

        let conn = store.connection();
        assert_eq!(get_profile(conn, 2025), Some(again));
        assert_eq!(
            get_profile(conn, 2026).map(|p| p.filing_status),
            Some(FilingStatus::MarriedFilingJointly)
        );
        assert_eq!(profile_years(conn), vec![2026, 2025]);
        assert_eq!(get_profile(conn, 2024), None);
    }

    /// An account that is not in the books is refused, not read as zero.
    #[test]
    fn a_property_naming_a_missing_account_is_refused() {
        let mut store = books();
        let mut p = profile(2025, FilingStatus::Single);
        p.rental_properties.push(RentalPropertyData {
            name: "4541 N Lincoln".to_string(),
            income_account_ids: vec!["no-such-account".to_string()],
            expense_account_ids: Vec::new(),
        });
        let err = set_profile(&mut store, "u", &p).unwrap_err();
        assert!(err.to_string().contains("no-such-account"), "{err}");
    }

    /// A spouse's age raises the standard deduction only on a joint return.
    #[test]
    fn a_spouse_flag_off_a_joint_return_is_refused() {
        let mut store = books();
        let mut p = profile(2025, FilingStatus::Single);
        p.spouse_65_or_older = true;
        assert!(set_profile(&mut store, "u", &p).is_err());
        p.filing_status = FilingStatus::MarriedFilingJointly;
        assert!(set_profile(&mut store, "u", &p).is_ok());
    }

    #[test]
    fn filing_status_reads_the_short_forms() {
        assert_eq!(FilingStatus::parse("MFJ"), Some(FilingStatus::MarriedFilingJointly));
        assert_eq!(FilingStatus::parse("head of household"), Some(FilingStatus::HeadOfHousehold));
        assert_eq!(FilingStatus::parse("married_filing_separately"), Some(FilingStatus::MarriedFilingSeparately));
        assert_eq!(FilingStatus::parse("x"), None);
    }
}
