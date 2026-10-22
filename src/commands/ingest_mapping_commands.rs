//! Setting an event-service account mapping as an event.
//!
//! The mappings say which ledger account each part of a shop sale books to —
//! revenue, cost of goods, the Square clearing account. They used to be a local
//! table, so on group-hosted books every member's machine kept its own and a
//! second machine pulling the same feed found nothing mapped. See migration 060.
//!
//! The account check lives here so a local command and the group server's
//! `set-ingest-mapping` refuse the same things. The key catalogue is checked
//! only by the local command: the server accepts any well-formed key, so a new
//! importer key ships with the app and desktop without a server deploy.

use crate::commands::ingest_commands::mapping_keys;
use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum IngestMappingError {
    #[error("Unknown mapping key '{0}'")]
    UnknownKey(String),
    #[error("Malformed mapping key '{0}'")]
    MalformedKey(String),
    #[error("No account with id {0}")]
    NoSuchAccount(String),
    #[error("Account {0} is inactive")]
    InactiveAccount(String),
    #[error("Store error: {0}")]
    Store(#[from] EventStoreError),
}

/// Whether a key has the shape of a catalogue key: 1–64 of `[a-z0-9_]`.
fn is_well_formed_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Refuse a malformed key, or an account that is missing or inactive. Run
/// inside the append transaction, against the write-locked state.
///
/// Whether the catalogue knows the key is deliberately not checked here: the
/// group server runs this with whatever catalogue its pinned app rev had, and a
/// key added since (`pos_zelle`) would be refused until the server was bumped.
/// Storing a key no importer reads is harmless; the local command still checks.
pub fn check_set_ingest_mapping(
    conn: &Connection,
    key: &str,
    account_id: &str,
) -> Result<Result<(), IngestMappingError>, EventStoreError> {
    if !is_well_formed_key(key) {
        return Ok(Err(IngestMappingError::MalformedKey(key.to_string())));
    }
    let active: Option<bool> = conn
        .query_row(
            "SELECT is_active = 1 FROM accounts WHERE id = ?1",
            [account_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match active {
        None => Err(IngestMappingError::NoSuchAccount(account_id.to_string())),
        Some(false) => Err(IngestMappingError::InactiveAccount(account_id.to_string())),
        Some(true) => Ok(()),
    })
}

/// Point a mapping key at an account, on standalone books.
pub fn set_ingest_mapping(
    store: &mut EventStore,
    user_id: &str,
    key: &str,
    account_id: &str,
) -> Result<StoredEvent, IngestMappingError> {
    if !mapping_keys().contains(&key) {
        return Err(IngestMappingError::UnknownKey(key.to_string()));
    }
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let outcome = store.append_checked(
            head,
            |tx| {
                Ok(match check_set_ingest_mapping(tx, key, account_id)? {
                    Ok(()) => Verdict::Append(EventEnvelope::new(
                        Event::IngestMappingSet {
                            key: key.to_string(),
                            account_id: account_id.to_string(),
                        },
                        user_id.to_string(),
                    )),
                    Err(e) => Verdict::Reject(e),
                })
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;
        match outcome {
            CheckedOutcome::Appended(stored) => return Ok(stored),
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

/// Mappings this machine made before they were events, as `(key, account_id)`.
///
/// Still honoured here, but no other member has them. On hosted books the
/// Services page offers to publish them, and only those whose account the
/// group's books still have and whose key the catalogue still knows.
pub fn unpublished_mappings(conn: &Connection) -> Vec<(String, String)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT m.key, m.account_id FROM ingest_account_mappings m
           JOIN accounts a ON a.id = m.account_id AND a.is_active = 1
          WHERE m.updated_at_event IS NULL
          ORDER BY m.key",
    ) else {
        return Vec::new();
    };
    let keys = mapping_keys();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map(|rows| {
            rows.filter_map(Result::ok)
                .filter(|(k, _): &(String, String)| keys.contains(&k.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ingest_commands::{load_all_mappings, set_account_mapping};
    use crate::store::event_store::MirrorEvent;
    use crate::store::migrations::SchemaStore;

    fn store_with_accounts() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut s).unwrap();
        s.connection()
            .execute_batch(
                "INSERT INTO accounts (id, account_type, account_number, name, is_active)
                 VALUES ('rev', 'revenue', '4000', 'Sales', 1),
                        ('old', 'revenue', '4900', 'Old sales', 0);",
            )
            .unwrap();
        s
    }

    /// The reported problem: a mapping set on one machine never reached the
    /// other. As an event it is in the log, and a replica replaying the log gets
    /// the same row.
    #[test]
    fn a_mapping_is_an_event_a_replica_replays() {
        let mut origin = store_with_accounts();
        set_ingest_mapping(&mut origin, "user", "pos_revenue", "rev").unwrap();
        assert_eq!(
            load_all_mappings(origin.connection()).get("pos_revenue"),
            Some(&"rev".to_string())
        );

        let mirrored: Vec<MirrorEvent> = origin
            .get_all()
            .unwrap()
            .into_iter()
            .map(|e| MirrorEvent {
                seq: e.id,
                event: e.event,
                user_id: e.user_id,
                timestamp: e.timestamp,
                actor_id: None,
                received_at: None,
                hash: e.hash,
            })
            .collect();
        let mut replica = store_with_accounts();
        replica
            .append_mirrored(&mirrored, crate::sync::project)
            .unwrap();
        assert_eq!(
            load_all_mappings(replica.connection()).get("pos_revenue"),
            Some(&"rev".to_string()),
            "the second machine has the mapping without setting it"
        );
        assert!(unpublished_mappings(replica.connection()).is_empty());
    }

    #[test]
    fn unknown_keys_and_inactive_accounts_are_refused() {
        let mut s = store_with_accounts();
        assert!(matches!(
            set_ingest_mapping(&mut s, "user", "not_a_key", "rev"),
            Err(IngestMappingError::UnknownKey(_))
        ));
        assert!(matches!(
            set_ingest_mapping(&mut s, "user", "pos_revenue", "old"),
            Err(IngestMappingError::InactiveAccount(_))
        ));
        assert!(matches!(
            set_ingest_mapping(&mut s, "user", "pos_revenue", "gone"),
            Err(IngestMappingError::NoSuchAccount(_))
        ));
        assert!(s.get_all().unwrap().is_empty(), "nothing appended");
    }

    /// The group server runs the shared check with its own, possibly older,
    /// catalogue. A key it has never heard of still passes; a malformed one
    /// or a bad account does not.
    #[test]
    fn the_shared_check_takes_keys_the_catalogue_lacks() {
        let s = store_with_accounts();
        let conn = s.connection();
        assert!(check_set_ingest_mapping(conn, "pos_added_later", "rev").unwrap().is_ok());
        for bad in ["", "Pos_Zelle", "pos zelle", "pos-zelle", &"k".repeat(65)] {
            assert!(
                matches!(
                    check_set_ingest_mapping(conn, bad, "rev").unwrap(),
                    Err(IngestMappingError::MalformedKey(_))
                ),
                "{bad:?}"
            );
        }
        assert!(matches!(
            check_set_ingest_mapping(conn, "pos_added_later", "old").unwrap(),
            Err(IngestMappingError::InactiveAccount(_))
        ));
    }

    /// A mapping written the old way is offered for publishing until an event
    /// covers its key, and one pointing at an inactive account never is.
    #[test]
    fn mappings_made_before_events_are_offered_for_publishing() {
        let mut s = store_with_accounts();
        set_account_mapping(s.connection(), "pos_revenue", "rev").unwrap();
        set_account_mapping(s.connection(), "cogs", "old").unwrap();
        assert_eq!(
            unpublished_mappings(s.connection()),
            vec![("pos_revenue".to_string(), "rev".to_string())]
        );

        set_ingest_mapping(&mut s, "user", "pos_revenue", "rev").unwrap();
        assert!(unpublished_mappings(s.connection()).is_empty());
    }
}
