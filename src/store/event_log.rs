//! Storage abstraction for the event log (SPEC §6.1 — first Phase-1 deliverable).
//!
//! `EventLog` is the backend-agnostic append/read contract every event-store
//! implementation must satisfy. Today the only implementation is the local
//! SQLite [`EventStore`]; the multi-tenant work adds a Postgres implementation
//! for the hosted group server (SPEC §4.2, the blocking Phase-1 port) behind
//! this *same* trait, so the engine above the store — command handlers,
//! projections, queries — can eventually run unchanged on either backend.
//!
//! This first cut introduces the seam without disturbing any caller: the
//! inherent methods on `EventStore` are untouched (so all existing call sites
//! keep compiling byte-for-byte), and `EventStore` additionally *implements*
//! this trait by delegating to them. Migrating consumers to be generic over
//! `EventLog` is incremental follow-up work — the trait existing is what
//! unblocks writing the Postgres backend against a defined surface.
//!
//! Deliberately **excluded** from the trait: connection accessors
//! (`connection`/`connection_mut`), constructors (`open`/`in_memory`), and
//! `backup_to`. Those are SQLite-specific construction and maintenance details,
//! not part of the append/read contract a Postgres backend has to honor. The
//! raw-`Connection` accessors are, today, how migrations/projections reach the
//! DB; unpicking that coupling is part of the Postgres port, tracked separately.
//!
//! Error type: [`EventStoreError`] still embeds `rusqlite::Error` via its
//! SQLite-only `DatabaseError` variant, but also carries a backend-neutral
//! `Backend(String)` variant so a non-SQLite backend (the Postgres group
//! server) can implement this trait without embedding a driver-specific error.
//! The SQLite store never produces `Backend`.

use crate::events::types::{EventEnvelope, StoredEvent};
use crate::store::event_store::{AppendOutcome, EventStore, EventStoreError};

/// The append-only event log: the source of truth the whole ledger folds over.
///
/// All read methods mirror the inherent `EventStore` API one-for-one so the
/// switch to trait-generic call sites is a pure find-and-replace, not a
/// semantic change. The two append methods are the write surface:
/// [`append`](EventLog::append) is the legacy single-writer path (solo,
/// local-first) and [`append_expecting`](EventLog::append_expecting) is the
/// optimistic compare-and-append used by the concurrent server path.
pub trait EventLog {
    /// Append a new event to the head of the log (legacy single-writer path).
    fn append(&mut self, envelope: EventEnvelope) -> Result<StoredEvent, EventStoreError>;

    /// Compare-and-append: append `envelope` iff the current head sequence still
    /// equals `expected_head_seq`, otherwise report [`AppendOutcome::HeadMismatch`].
    /// This is the optimistic-concurrency primitive for concurrent writers.
    fn append_expecting(
        &mut self,
        envelope: EventEnvelope,
        expected_head_seq: i64,
    ) -> Result<AppendOutcome, EventStoreError>;

    /// Fetch a single event by its sequence id.
    fn get(&self, id: i64) -> Result<StoredEvent, EventStoreError>;

    /// All events in ascending sequence order.
    fn get_all(&self) -> Result<Vec<StoredEvent>, EventStoreError>;

    /// All events of a given `event_type`, in ascending sequence order.
    fn get_by_type(&self, event_type: &str) -> Result<Vec<StoredEvent>, EventStoreError>;

    /// Events with id strictly greater than `after_id` (the sync/tail read).
    fn get_after(&self, after_id: i64) -> Result<Vec<StoredEvent>, EventStoreError>;

    /// Number of events in the log.
    fn count(&self) -> Result<i64, EventStoreError>;

    /// The id of the most recent event, or `None` for an empty log.
    fn latest_id(&self) -> Result<Option<i64>, EventStoreError>;

    /// Every event hash in sequence order (for Merkle-tree building).
    fn get_all_hashes(&self) -> Result<Vec<Vec<u8>>, EventStoreError>;

    /// The stored hash of a single event.
    fn get_hash(&self, id: i64) -> Result<Vec<u8>, EventStoreError>;

    /// Current head sequence — `latest_id` normalized so an empty log reads as
    /// `0`. This is exactly the value a caller passes as `expected_head_seq` to
    /// [`append_expecting`](EventLog::append_expecting), so having it on the
    /// trait keeps the compare-and-append handshake backend-agnostic.
    fn head_seq(&self) -> Result<i64, EventStoreError> {
        Ok(self.latest_id()?.unwrap_or(0))
    }
}

/// SQLite implementation: pure delegation to the inherent `EventStore` methods.
///
/// Method-call syntax resolves to the inherent method (Rust prefers inherent
/// methods over trait methods of the same name), so these bodies call the real
/// implementations rather than recursing into the trait.
impl EventLog for EventStore {
    fn append(&mut self, envelope: EventEnvelope) -> Result<StoredEvent, EventStoreError> {
        EventStore::append(self, envelope)
    }

    fn append_expecting(
        &mut self,
        envelope: EventEnvelope,
        expected_head_seq: i64,
    ) -> Result<AppendOutcome, EventStoreError> {
        EventStore::append_expecting(self, envelope, expected_head_seq)
    }

    fn get(&self, id: i64) -> Result<StoredEvent, EventStoreError> {
        EventStore::get(self, id)
    }

    fn get_all(&self) -> Result<Vec<StoredEvent>, EventStoreError> {
        EventStore::get_all(self)
    }

    fn get_by_type(&self, event_type: &str) -> Result<Vec<StoredEvent>, EventStoreError> {
        EventStore::get_by_type(self, event_type)
    }

    fn get_after(&self, after_id: i64) -> Result<Vec<StoredEvent>, EventStoreError> {
        EventStore::get_after(self, after_id)
    }

    fn count(&self) -> Result<i64, EventStoreError> {
        EventStore::count(self)
    }

    fn latest_id(&self) -> Result<Option<i64>, EventStoreError> {
        EventStore::latest_id(self)
    }

    fn get_all_hashes(&self) -> Result<Vec<Vec<u8>>, EventStoreError> {
        EventStore::get_all_hashes(self)
    }

    fn get_hash(&self, id: i64) -> Result<Vec<u8>, EventStoreError> {
        EventStore::get_hash(self, id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{Event, EventAccountType};
    use crate::store::migrations::init_schema;

    fn unique_event(n: usize) -> Event {
        Event::AccountCreated {
            account_id: format!("acc-{n}"),
            account_type: EventAccountType::Asset,
            account_number: format!("{}", 1000 + n),
            name: format!("Account {n}"),
            parent_id: None,
            currency: None,
            description: None,
        }
    }

    /// Exercises the append/read/compare-and-append surface entirely through the
    /// `EventLog` trait (`&mut dyn EventLog`), proving the SQLite store satisfies
    /// the abstraction a Postgres backend will also have to satisfy. If this
    /// compiles and passes, the seam is real — a consumer written against
    /// `EventLog` needs no knowledge of the concrete backend.
    #[test]
    fn sqlite_store_satisfies_event_log_trait() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();

        // Drive it as a trait object so only the trait surface is in play.
        let log: &mut dyn EventLog = &mut store;

        assert_eq!(log.head_seq().unwrap(), 0);
        assert_eq!(log.count().unwrap(), 0);
        assert_eq!(log.latest_id().unwrap(), None);

        // Legacy append advances the head.
        let a = log
            .append(EventEnvelope::new(unique_event(1), "alice".into()))
            .unwrap();
        assert_eq!(a.id, 1);
        assert_eq!(log.head_seq().unwrap(), 1);

        // Compare-and-append against the stale head 0 conflicts...
        match log
            .append_expecting(EventEnvelope::new(unique_event(2), "bob".into()), 0)
            .unwrap()
        {
            AppendOutcome::HeadMismatch { expected, actual } => {
                assert_eq!((expected, actual), (0, 1));
            }
            other => panic!("expected HeadMismatch, got {other:?}"),
        }

        // ...and against the current head lands.
        let h = log.head_seq().unwrap();
        match log
            .append_expecting(EventEnvelope::new(unique_event(2), "bob".into()), h)
            .unwrap()
        {
            AppendOutcome::Appended(ev) => assert_eq!(ev.id, 2),
            other => panic!("expected Appended, got {other:?}"),
        }

        // Reads round-trip through the trait.
        assert_eq!(log.count().unwrap(), 2);
        assert_eq!(log.get(2).unwrap().id, 2);
        assert_eq!(log.get_after(1).unwrap().len(), 1);
        assert_eq!(log.get_all().unwrap().len(), 2);
        assert_eq!(log.get_all_hashes().unwrap().len(), 2);
        assert_eq!(log.get_hash(1).unwrap(), a.hash);
    }
}
