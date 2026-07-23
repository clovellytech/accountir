use crate::events::payload::{compute_event_hash, serialize_event};
use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::events::validation::validate_event;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EventStoreError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    /// Backend-neutral storage failure, for `EventLog` implementations that
    /// aren't SQLite (e.g. the Postgres group-server backend, SPEC §4.2). The
    /// SQLite store never produces this; it exists so the trait's error type is
    /// honestly implementable by a second backend without embedding a
    /// driver-specific error.
    #[error("Storage backend error: {0}")]
    Backend(String),
    #[error("Serialization error: {0}")]
    SerializationError(String),
    /// A caller-supplied in-transaction step (e.g. folding the event into
    /// projections inside `append_checked`) failed, rolling the append back.
    #[error("Projection error: {0}")]
    Projection(String),
    #[error("Validation error: {0}")]
    ValidationError(#[from] crate::events::validation::ValidationError),
    #[error("Event not found: {0}")]
    NotFound(i64),
    #[error("Duplicate event hash")]
    DuplicateHash,
    #[error("IO error: {0}")]
    IoError(String),
}

/// The event store manages the append-only event log
pub struct EventStore {
    conn: Connection,
}

/// Result of a compare-and-append (`append_expecting`).
///
/// This is the optimistic-concurrency outcome for the server append path (see
/// `MULTITENANT-SPEC.md` §6.2). A `HeadMismatch` is *not* an error — it is the
/// expected signal that another writer appended since the caller last read the
/// log, telling the caller to refetch the head, re-derive its event against the
/// new state, and retry.
#[derive(Debug)]
pub enum AppendOutcome {
    /// The event was appended; head is now `event.id`.
    Appended(StoredEvent),
    /// The log moved under the caller. `expected` was passed in; `actual` is the
    /// head observed inside the append transaction.
    HeadMismatch { expected: i64, actual: i64 },
}

/// A `check` closure's decision inside [`append_checked`](EventStore::append_checked)
/// / [`append_checked_many`](EventStore::append_checked_many), made after reading
/// current state under the append transaction's write lock.
///
/// `T` is what gets appended on success — a single [`EventEnvelope`] for
/// `append_checked`, a `Vec<EventEnvelope>` for `append_checked_many` (a command
/// that emits several events atomically). `E` is the caller's domain error type.
pub enum Verdict<T, E> {
    /// Invariants hold against the write-locked current state — append this.
    Append(T),
    /// An invariant failed (e.g. account inactive, period closed, would overpay).
    /// The transaction rolls back with this domain error; nothing is appended.
    Reject(E),
}

/// Outcome of [`append_checked`](EventStore::append_checked) /
/// [`append_checked_many`](EventStore::append_checked_many).
///
/// Unlike [`AppendOutcome`], the invariant check ran *inside* the append
/// transaction — atomically with the head compare and the insert(s) — so a command
/// can no longer read state, decide it is valid, and then have another writer
/// invalidate that decision before the append lands (the read-then-append TOCTOU
/// catalogued in `docs/multitenant-invariant-audit.md`). `T` is the appended
/// result ([`StoredEvent`] or `Vec<StoredEvent>`); `E` is the caller's domain
/// error type (e.g. `EntryCommandError`).
pub enum CheckedOutcome<T, E> {
    /// The check passed and the event(s) were appended.
    Appended(T),
    /// The log moved under the caller before the write lock was taken; nothing
    /// was appended and the check did not run. Refetch the head and retry the
    /// whole command against fresh state.
    HeadMismatch { expected: i64, actual: i64 },
    /// The caller's invariant check rejected the command. Nothing was appended;
    /// the transaction rolled back. This is a terminal domain error, not a retry.
    Rejected(E),
}

/// The raw column parts of an `events` row, in SELECT order:
/// `(id, payload, hash, user_id, timestamp_str, actor_id, received_at_str)`.
/// `actor_id`/`received_at_str` are nullable (NULL on legacy/solo rows).
type StoredEventParts = (
    i64,
    String,
    Vec<u8>,
    String,
    String,
    Option<String>,
    Option<String>,
);

/// Extract the raw column parts from an `events` row (used by the list queries,
/// which all select the same columns in the same order).
fn row_to_stored_parts(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEventParts> {
    Ok((
        row.get(0)?, // id
        row.get(2)?, // payload
        row.get(3)?, // hash
        row.get(4)?, // user_id
        row.get(5)?, // timestamp
        row.get(6)?, // actor_id
        row.get(7)?, // received_at
    ))
}

/// Parse a nullable server-stamped `received_at` string into an optional
/// `DateTime<Utc>`. `None` in ⇒ `None` out (legacy/solo rows).
fn parse_received_at(
    received_at_str: Option<&str>,
) -> Result<Option<DateTime<Utc>>, EventStoreError> {
    match received_at_str {
        Some(s) => Ok(Some(
            DateTime::parse_from_rfc3339(s)
                .map_err(|e| EventStoreError::SerializationError(e.to_string()))?
                .with_timezone(&Utc),
        )),
        None => Ok(None),
    }
}

/// Hydrate raw row parts into a `StoredEvent`, returning `None` if the payload or
/// a timestamp fails to parse (matching the lenient `filter_map` behavior of the
/// list queries, which skip corrupt rows rather than aborting the whole read).
fn hydrate_stored_event(parts: StoredEventParts) -> Option<StoredEvent> {
    let (id, payload, hash, user_id, timestamp_str, actor_id, received_at_str) = parts;
    let event: Event = serde_json::from_str(&payload).ok()?;
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .ok()?
        .with_timezone(&Utc);
    let received_at = match received_at_str {
        Some(s) => Some(DateTime::parse_from_rfc3339(&s).ok()?.with_timezone(&Utc)),
        None => None,
    };
    Some(StoredEvent::with_identity(
        id,
        event,
        hash,
        user_id,
        timestamp,
        actor_id,
        received_at,
    ))
}

impl EventStore {
    /// Open an existing database or create a new one.
    ///
    /// Sets `journal_mode=WAL` so that the Tauri-side connection and the
    /// in-process sync server can read and write concurrently without
    /// blocking each other. `busy_timeout` is the retry budget on the rare
    /// writer-vs-writer overlap.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, EventStoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; \
             PRAGMA foreign_keys=ON; \
             PRAGMA busy_timeout=5000;",
        )?;
        Ok(Self { conn })
    }

    /// Create an in-memory event store (for testing). In-memory DBs don't
    /// support WAL, so we skip that pragma here.
    pub fn in_memory() -> Result<Self, EventStoreError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        Ok(Self { conn })
    }

    /// Write a transaction-consistent snapshot of the database to `dest` using
    /// SQLite's online backup API — safe to call while the database is open and
    /// being written. The snapshot is a single self-contained file (no `-wal`
    /// sidecar), written to a temp path and atomically renamed so a file-sync
    /// daemon watching `dest` never observes a half-written database.
    pub fn backup_to(&self, dest: &Path) -> Result<(), EventStoreError> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EventStoreError::IoError(e.to_string()))?;
        }
        let tmp = dest.with_extension("db.tmp");
        let _ = std::fs::remove_file(&tmp);
        // A checkpoint first folds pending WAL pages into the snapshot promptly.
        let _ = self
            .conn
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        self.conn.backup("main", &tmp, None)?;
        std::fs::rename(&tmp, dest).map_err(|e| EventStoreError::IoError(e.to_string()))?;
        Ok(())
    }

    /// Get the underlying connection (for migrations, etc.)
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Get a mutable reference to the connection
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Append a new event to the store
    pub fn append(&mut self, envelope: EventEnvelope) -> Result<StoredEvent, EventStoreError> {
        // Validate the event
        validate_event(&envelope.event)?;

        // Serialize the event
        let payload = serialize_event(&envelope.event)
            .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;

        // Compute the hash.
        // NOTE: `actor_id`/`received_at` are deliberately NOT hash inputs — they
        // are server-stamped and may be absent, so hashing them would make legacy
        // hashes unstable and appends non-deterministic. See `compute_event_hash`.
        let timestamp_str = envelope.timestamp.to_rfc3339();
        let received_at_str = envelope.received_at.map(|t| t.to_rfc3339());
        let hash = compute_event_hash(&envelope.event, &timestamp_str, &envelope.user_id)
            .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;

        // Insert the event
        let result = self.conn.execute(
            "INSERT INTO events (event_type, payload, hash, user_id, timestamp, actor_id, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                envelope.event.event_type(),
                payload,
                hash.as_slice(),
                envelope.user_id,
                timestamp_str,
                envelope.actor_id,
                received_at_str,
            ],
        );

        match result {
            Ok(_) => {
                let id = self.conn.last_insert_rowid();
                Ok(StoredEvent::with_identity(
                    id,
                    envelope.event,
                    hash.to_vec(),
                    envelope.user_id,
                    envelope.timestamp,
                    envelope.actor_id,
                    envelope.received_at,
                ))
            }
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(EventStoreError::DuplicateHash)
            }
            Err(e) => Err(EventStoreError::DatabaseError(e)),
        }
    }

    /// Compare-and-append: append `envelope` **iff** the current head sequence
    /// equals `expected_head_seq`, otherwise report a `HeadMismatch`.
    ///
    /// This is the optimistic-concurrency primitive that makes the log safe for
    /// concurrent writers (the server append path in the multi-tenant work).
    /// The head read and the insert run inside a single `IMMEDIATE`
    /// transaction, which takes SQLite's write lock up front — so for any given
    /// head value **at most one** concurrent caller can win the append; every
    /// other caller reads the post-commit head and gets `HeadMismatch`.
    ///
    /// `expected_head_seq` is the id of the last event the caller saw, or `0`
    /// for an empty log (ids start at 1). On `HeadMismatch` the caller should
    /// refetch (`latest_id`), re-derive its event against the new state
    /// (re-running any invariant checks — a matching head guarantees no event
    /// landed since the caller's read, so its view is current), and retry.
    ///
    /// Note: `append` (the legacy single-writer path) is left untouched so solo
    /// local-first behavior is byte-for-byte unchanged.
    pub fn append_expecting(
        &mut self,
        envelope: EventEnvelope,
        expected_head_seq: i64,
    ) -> Result<AppendOutcome, EventStoreError> {
        // Validate + serialize + hash outside the txn (pure, no DB state).
        validate_event(&envelope.event)?;
        let payload = serialize_event(&envelope.event)
            .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;
        let timestamp_str = envelope.timestamp.to_rfc3339();
        let received_at_str = envelope.received_at.map(|t| t.to_rfc3339());
        // See `append`: server-stamped identity fields are not hash inputs.
        let hash = compute_event_hash(&envelope.event, &timestamp_str, &envelope.user_id)
            .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;

        // IMMEDIATE grabs the write lock before we read the head, so the
        // compare-and-append is atomic against other writers.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let actual_head: i64 =
            tx.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| row.get(0))?;

        if actual_head != expected_head_seq {
            // Drop the txn (rollback) without inserting — this is a normal
            // optimistic-concurrency outcome, not an error.
            return Ok(AppendOutcome::HeadMismatch {
                expected: expected_head_seq,
                actual: actual_head,
            });
        }

        let insert = tx.execute(
            "INSERT INTO events (event_type, payload, hash, user_id, timestamp, actor_id, received_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                envelope.event.event_type(),
                payload,
                hash.as_slice(),
                envelope.user_id,
                timestamp_str,
                envelope.actor_id,
                received_at_str,
            ],
        );

        match insert {
            Ok(_) => {
                let id = tx.last_insert_rowid();
                tx.commit()?;
                Ok(AppendOutcome::Appended(StoredEvent::with_identity(
                    id,
                    envelope.event,
                    hash.to_vec(),
                    envelope.user_id,
                    envelope.timestamp,
                    envelope.actor_id,
                    envelope.received_at,
                )))
            }
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(EventStoreError::DuplicateHash)
            }
            Err(e) => Err(EventStoreError::DatabaseError(e)),
        }
    }

    /// Compare-and-append with a caller-supplied invariant check **and**
    /// projection step that both run **inside** the append transaction.
    ///
    /// This is the productized form of `append_expecting` for command handlers
    /// whose validity depends on current ledger state (SPEC §6.2, Phase 1). The
    /// `IMMEDIATE` transaction takes SQLite's write lock up front, then, in one
    /// atomic critical section:
    ///   1. compares the head against `expected_head_seq` (bails with
    ///      [`CheckedOutcome::HeadMismatch`] if the log moved — neither closure
    ///      runs);
    ///   2. runs `check`, which reads the now write-locked current state (the
    ///      projection tables) and returns either the event to append
    ///      ([`Verdict::Append`]) or a domain rejection ([`Verdict::Reject`]);
    ///   3. inserts the event, then runs `project` to fold that event into the
    ///      projection tables — in the *same* transaction — and commits.
    ///
    /// Folding the projection into the transaction is what makes state-dependent
    /// invariants safe across *concurrent connections* (the app runs the UI and
    /// the in-process sync server on separate connections, see [`open`]): the
    /// event and its projection become visible atomically, so another writer's
    /// `check` can never observe a log that leads its projections. Combined with
    /// the head compare and the shared write lock, this fully closes the
    /// read-then-append TOCTOU that single-writer previously hid — with no
    /// reliance on an app-level append mutex.
    ///
    /// On [`CheckedOutcome::HeadMismatch`] the caller should refetch the head and
    /// retry the whole command; [`CheckedOutcome::Rejected`] is terminal. If
    /// `project` errors, the whole append (event included) rolls back.
    ///
    /// [`open`]: EventStore::open
    pub fn append_checked<E>(
        &mut self,
        expected_head_seq: i64,
        check: impl FnOnce(
            &rusqlite::Transaction<'_>,
        ) -> Result<Verdict<EventEnvelope, E>, EventStoreError>,
        project: impl Fn(&rusqlite::Transaction<'_>, &StoredEvent) -> Result<(), EventStoreError>,
    ) -> Result<CheckedOutcome<StoredEvent, E>, EventStoreError> {
        // Single-event case: wrap the one envelope in a batch of one and unwrap
        // the one result. All the transactional work lives in `append_checked_many`.
        let outcome = self.append_checked_many(
            expected_head_seq,
            |tx| {
                Ok(match check(tx)? {
                    Verdict::Append(envelope) => Verdict::Append(vec![envelope]),
                    Verdict::Reject(e) => Verdict::Reject(e),
                })
            },
            project,
        )?;
        Ok(match outcome {
            CheckedOutcome::Appended(mut events) => {
                CheckedOutcome::Appended(events.pop().expect("exactly one event appended"))
            }
            CheckedOutcome::HeadMismatch { expected, actual } => {
                CheckedOutcome::HeadMismatch { expected, actual }
            }
            CheckedOutcome::Rejected(e) => CheckedOutcome::Rejected(e),
        })
    }

    /// Compare-and-append for a command that emits **several events atomically**,
    /// with the invariant check and each event's projection all inside one
    /// transaction.
    ///
    /// This is the multi-event generalization of [`append_checked`] — the same
    /// primitive, but `check` returns a `Vec<EventEnvelope>` (e.g. a bill payment
    /// that emits both a `JournalEntryPosted` and a `BillPaymentApplied`, which
    /// must land together or not at all). The `IMMEDIATE` transaction, in one
    /// atomic critical section:
    ///   1. compares the head against `expected_head_seq` (bails with
    ///      [`CheckedOutcome::HeadMismatch`] if the log moved — neither closure runs);
    ///   2. runs `check` against the write-locked current state, yielding the
    ///      events to append ([`Verdict::Append`]) or a domain rejection
    ///      ([`Verdict::Reject`]);
    ///   3. inserts each event in order and runs `project` on each — folding them
    ///      into the projection tables — then commits.
    ///
    /// Every insert and projection shares the one write-locked transaction, so the
    /// whole batch (and the projections it drives) becomes visible atomically:
    /// another writer's `check` can never observe a partial batch or a log that
    /// leads its projections. See [`append_checked`] for the full concurrency
    /// rationale (two writer connections, no app-level mutex needed). Any error —
    /// including from `project` — rolls the entire batch back.
    pub fn append_checked_many<E>(
        &mut self,
        expected_head_seq: i64,
        check: impl FnOnce(
            &rusqlite::Transaction<'_>,
        ) -> Result<Verdict<Vec<EventEnvelope>, E>, EventStoreError>,
        project: impl Fn(&rusqlite::Transaction<'_>, &StoredEvent) -> Result<(), EventStoreError>,
    ) -> Result<CheckedOutcome<Vec<StoredEvent>, E>, EventStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let actual_head: i64 =
            tx.query_row("SELECT COALESCE(MAX(id), 0) FROM events", [], |row| row.get(0))?;
        if actual_head != expected_head_seq {
            return Ok(CheckedOutcome::HeadMismatch {
                expected: expected_head_seq,
                actual: actual_head,
            });
        }

        let envelopes = match check(&tx)? {
            Verdict::Append(envelopes) => envelopes,
            // Dropping `tx` rolls back — nothing was written.
            Verdict::Reject(e) => return Ok(CheckedOutcome::Rejected(e)),
        };

        let mut stored_events = Vec::with_capacity(envelopes.len());
        for envelope in envelopes {
            // Validate + serialize + hash inside the txn: a failure anywhere rolls
            // the whole batch back.
            validate_event(&envelope.event)?;
            let payload = serialize_event(&envelope.event)
                .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;
            let timestamp_str = envelope.timestamp.to_rfc3339();
            let received_at_str = envelope.received_at.map(|t| t.to_rfc3339());
            // See `append`: server-stamped identity fields are not hash inputs.
            let hash = compute_event_hash(&envelope.event, &timestamp_str, &envelope.user_id)
                .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;

            match tx.execute(
                "INSERT INTO events (event_type, payload, hash, user_id, timestamp, actor_id, received_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    envelope.event.event_type(),
                    payload,
                    hash.as_slice(),
                    envelope.user_id,
                    timestamp_str,
                    envelope.actor_id,
                    received_at_str,
                ],
            ) {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(EventStoreError::DuplicateHash)
                }
                Err(e) => return Err(EventStoreError::DatabaseError(e)),
            }

            let stored = StoredEvent::with_identity(
                tx.last_insert_rowid(),
                envelope.event,
                hash.to_vec(),
                envelope.user_id,
                envelope.timestamp,
                envelope.actor_id,
                envelope.received_at,
            );

            // Fold each event into projections in the same transaction, so the log
            // and the projections it drives commit atomically.
            project(&tx, &stored)?;
            stored_events.push(stored);
        }

        tx.commit()?;
        Ok(CheckedOutcome::Appended(stored_events))
    }

    /// Get an event by ID
    pub fn get(&self, id: i64) -> Result<StoredEvent, EventStoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, event_type, payload, hash, user_id, timestamp, actor_id, received_at
                 FROM events WHERE id = ?1",
                [id],
                |row| {
                    let payload: String = row.get(2)?;
                    let hash: Vec<u8> = row.get(3)?;
                    let user_id: String = row.get(4)?;
                    let timestamp_str: String = row.get(5)?;
                    let actor_id: Option<String> = row.get(6)?;
                    let received_at_str: Option<String> = row.get(7)?;

                    Ok((payload, hash, user_id, timestamp_str, actor_id, received_at_str))
                },
            )
            .optional()?;

        match row {
            Some((payload, hash, user_id, timestamp_str, actor_id, received_at_str)) => {
                let event: Event = serde_json::from_str(&payload)
                    .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;
                let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
                    .map_err(|e| EventStoreError::SerializationError(e.to_string()))?
                    .with_timezone(&Utc);
                let received_at = parse_received_at(received_at_str.as_deref())?;

                Ok(StoredEvent::with_identity(
                    id, event, hash, user_id, timestamp, actor_id, received_at,
                ))
            }
            None => Err(EventStoreError::NotFound(id)),
        }
    }

    /// Get all events in order
    pub fn get_all(&self) -> Result<Vec<StoredEvent>, EventStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, payload, hash, user_id, timestamp, actor_id, received_at
             FROM events ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([], row_to_stored_parts)?
            .filter_map(|r| r.ok())
            .filter_map(hydrate_stored_event)
            .collect();

        Ok(events)
    }

    /// Get events by type
    pub fn get_by_type(&self, event_type: &str) -> Result<Vec<StoredEvent>, EventStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, payload, hash, user_id, timestamp, actor_id, received_at
             FROM events WHERE event_type = ?1 ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([event_type], row_to_stored_parts)?
            .filter_map(|r| r.ok())
            .filter_map(hydrate_stored_event)
            .collect();

        Ok(events)
    }

    /// Get events after a specific ID (for sync)
    pub fn get_after(&self, after_id: i64) -> Result<Vec<StoredEvent>, EventStoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_type, payload, hash, user_id, timestamp, actor_id, received_at
             FROM events WHERE id > ?1 ORDER BY id ASC",
        )?;

        let events = stmt
            .query_map([after_id], row_to_stored_parts)?
            .filter_map(|r| r.ok())
            .filter_map(hydrate_stored_event)
            .collect();

        Ok(events)
    }

    /// Get the count of events
    pub fn count(&self) -> Result<i64, EventStoreError> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        Ok(count)
    }

    /// Get the latest event ID
    pub fn latest_id(&self) -> Result<Option<i64>, EventStoreError> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT MAX(id) FROM events", [], |row| row.get(0))?;
        Ok(id)
    }

    /// Get all event hashes (for Merkle tree building)
    pub fn get_all_hashes(&self) -> Result<Vec<Vec<u8>>, EventStoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT hash FROM events ORDER BY id ASC")?;

        let hashes: Vec<Vec<u8>> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(hashes)
    }

    /// Get event hash by ID
    pub fn get_hash(&self, id: i64) -> Result<Vec<u8>, EventStoreError> {
        let hash: Vec<u8> = self
            .conn
            .query_row("SELECT hash FROM events WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .optional()?
            .ok_or(EventStoreError::NotFound(id))?;
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventAccountType, JournalEntrySource, JournalLineData};
    use crate::store::migrations::init_schema;
    use chrono::NaiveDate;

    fn setup_store() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    /// A validation-passing event whose content (hence hash) is unique per
    /// `(tag, n)`, so concurrent appends never collide on `UNIQUE(hash)`.
    fn sample_account(tag: &str, n: usize) -> Event {
        Event::AccountCreated {
            account_id: format!("{tag}-{n}"),
            account_type: EventAccountType::Asset,
            account_number: format!("{tag}-{n}"),
            name: format!("acct {tag} {n}"),
            parent_id: None,
            currency: None,
            description: None,
        }
    }

    #[test]
    fn append_checked_appends_when_head_matches_and_check_passes() {
        let mut store = setup_store();
        let env = EventEnvelope::new(sample_account("a", 1), "u".into());
        match store
            .append_checked::<()>(0, |_tx| Ok(Verdict::Append(env)), |_tx, _stored| Ok(()))
            .unwrap()
        {
            CheckedOutcome::Appended(se) => assert_eq!(se.id, 1),
            _ => panic!("expected Appended"),
        }
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn append_checked_rolls_back_event_if_projection_fails() {
        // The event and its projection commit atomically: if the in-txn project
        // step fails, the event must not persist in the log either.
        let mut store = setup_store();
        let outcome = store.append_checked::<()>(
            0,
            |_tx| {
                Ok(Verdict::Append(EventEnvelope::new(
                    sample_account("a", 1),
                    "u".into(),
                )))
            },
            |_tx, _stored| Err(EventStoreError::Projection("boom".into())),
        );
        assert!(matches!(outcome, Err(EventStoreError::Projection(_))));
        assert_eq!(
            store.count().unwrap(),
            0,
            "event must not persist when its projection fails"
        );
    }

    #[test]
    fn append_checked_many_is_all_or_nothing() {
        let mut store = setup_store();

        // Happy path: two events append together, gaplessly.
        match store
            .append_checked_many::<()>(
                0,
                |_tx| {
                    Ok(Verdict::Append(vec![
                        EventEnvelope::new(sample_account("a", 1), "u".into()),
                        EventEnvelope::new(sample_account("b", 2), "u".into()),
                    ]))
                },
                |_tx, _stored| Ok(()),
            )
            .unwrap()
        {
            CheckedOutcome::Appended(evs) => {
                assert_eq!(evs.len(), 2);
                assert_eq!((evs[0].id, evs[1].id), (1, 2));
            }
            _ => panic!("expected Appended"),
        }
        assert_eq!(store.count().unwrap(), 2);

        // Atomicity: if the SECOND event's projection fails, NEITHER of the
        // batch's events persists — the whole transaction rolls back.
        let calls = std::cell::Cell::new(0);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let outcome = store.append_checked_many::<()>(
            head,
            |_tx| {
                Ok(Verdict::Append(vec![
                    EventEnvelope::new(sample_account("c", 3), "u".into()),
                    EventEnvelope::new(sample_account("d", 4), "u".into()),
                ]))
            },
            |_tx, _stored| {
                let n = calls.get();
                calls.set(n + 1);
                if n == 1 {
                    Err(EventStoreError::Projection("boom on 2nd".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(outcome, Err(EventStoreError::Projection(_))));
        assert_eq!(
            store.count().unwrap(),
            2,
            "the whole batch must roll back — still only the first two events"
        );
    }

    #[test]
    fn append_checked_head_mismatch_does_not_run_check() {
        let mut store = setup_store();
        // Seed one event so the head is 1.
        store
            .append(EventEnvelope::new(sample_account("a", 1), "u".into()))
            .unwrap();

        let ran = std::cell::Cell::new(false);
        let outcome = store
            .append_checked::<()>(
                0,
                |_tx| {
                    ran.set(true);
                    Ok(Verdict::Append(EventEnvelope::new(
                        sample_account("b", 2),
                        "u".into(),
                    )))
                },
                |_tx, _stored| Ok(()),
            )
            .unwrap();

        match outcome {
            CheckedOutcome::HeadMismatch { expected, actual } => {
                assert_eq!((expected, actual), (0, 1))
            }
            _ => panic!("expected HeadMismatch"),
        }
        assert!(!ran.get(), "check must not run when the head has moved");
        assert_eq!(store.count().unwrap(), 1, "nothing appended on mismatch");
    }

    #[test]
    fn append_checked_reject_rolls_back() {
        let mut store = setup_store();
        let outcome = store
            .append_checked::<&str>(0, |_tx| Ok(Verdict::Reject("nope")), |_tx, _stored| Ok(()))
            .unwrap();
        match outcome {
            CheckedOutcome::Rejected(e) => assert_eq!(e, "nope"),
            _ => panic!("expected Rejected"),
        }
        assert_eq!(store.count().unwrap(), 0, "a rejected check appends nothing");
    }

    #[test]
    fn append_checked_capacity_invariant_holds_under_racing_threads() {
        // An invariant enforced *inside* the txn: the log may hold at most CAP
        // events. Two threads race to fill it; because the count-check and the
        // insert share one write-locked transaction (plus head-CAS retry), the
        // cap can never be exceeded. A read-then-append version of this check
        // would let both threads append the CAP-th event (count = CAP + 1).
        const CAP: i64 = 40;
        let dir = std::env::temp_dir().join(format!("accountir-checked-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("log.db");
        {
            let store = EventStore::open(&db).unwrap();
            init_schema(store.connection()).unwrap();
        }

        let worker = |tag: &'static str, path: std::path::PathBuf| {
            move || {
                let mut store = EventStore::open(&path).unwrap();
                let mut i = 0usize;
                loop {
                    let head = store.latest_id().unwrap().unwrap_or(0);
                    let env = EventEnvelope::new(sample_account(tag, i), tag.into());
                    let outcome = store
                        .append_checked::<()>(
                            head,
                            |tx| {
                                let n: i64 =
                                    tx.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
                                if n >= CAP {
                                    Ok(Verdict::Reject(()))
                                } else {
                                    Ok(Verdict::Append(env))
                                }
                            },
                            |_tx, _stored| Ok(()),
                        )
                        .unwrap();
                    match outcome {
                        CheckedOutcome::Appended(_) => i += 1,
                        CheckedOutcome::HeadMismatch { .. } => {} // refetch & retry
                        CheckedOutcome::Rejected(()) => break,    // cap reached
                    }
                }
            }
        };

        let t1 = std::thread::spawn(worker("t1", db.clone()));
        let t2 = std::thread::spawn(worker("t2", db.clone()));
        t1.join().unwrap();
        t2.join().unwrap();

        let store = EventStore::open(&db).unwrap();
        assert_eq!(
            store.count().unwrap(),
            CAP,
            "the capacity invariant must hold exactly — never exceeded under contention"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backup_creates_consistent_snapshot() {
        let dir = std::env::temp_dir().join(format!("accountir-bk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("src.db");
        let dst = dir.join("snap.backup.db");
        {
            let store = EventStore::open(&src).unwrap();
            store
                .connection()
                .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1),(2),(3);")
                .unwrap();
            store.backup_to(&dst).unwrap();
            // A second backup over an existing snapshot must succeed (atomic replace).
            store.backup_to(&dst).unwrap();
        }
        // The snapshot is a complete, standalone database — no -wal needed.
        let snap = Connection::open(&dst).unwrap();
        let n: i64 = snap
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 3);
        assert!(!dst.with_extension("db.tmp").exists(), "temp file left behind");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_append_and_get() {
        let mut store = setup_store();

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };

        let envelope = EventEnvelope::new(event, "user-001".to_string());
        let stored = store.append(envelope).unwrap();

        assert_eq!(stored.id, 1);
        assert_eq!(stored.user_id, "user-001");

        let retrieved = store.get(1).unwrap();
        assert_eq!(retrieved.id, stored.id);
    }

    #[test]
    fn actor_identity_round_trips_through_store() {
        // An envelope carrying server identity (actor_id + received_at) must
        // persist and hydrate those fields unchanged, via both get() and the
        // list read paths (get_all / get_after).
        let mut store = setup_store();

        let received = Utc::now();
        let envelope = EventEnvelope::new(sample_account("actor", 1), "u".into())
            .with_actor(Some("actor-42".into()))
            .with_received_at(Some(received));

        let stored = store.append(envelope).unwrap();
        assert_eq!(stored.actor_id.as_deref(), Some("actor-42"));
        assert_eq!(stored.received_at, Some(received));

        // get()
        let got = store.get(stored.id).unwrap();
        assert_eq!(got.actor_id.as_deref(), Some("actor-42"));
        assert_eq!(got.received_at, Some(received));
        // user_id / timestamp still carried as before.
        assert_eq!(got.user_id, "u");

        // list path (get_all)
        let all = store.get_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].actor_id.as_deref(), Some("actor-42"));
        assert_eq!(all[0].received_at, Some(received));

        // list path (get_after)
        let after = store.get_after(0).unwrap();
        assert_eq!(after[0].actor_id.as_deref(), Some("actor-42"));
        assert_eq!(after[0].received_at, Some(received));
    }

    #[test]
    fn legacy_envelope_stores_and_reads_with_null_identity() {
        // The unchanged EventEnvelope::new path (the ~200 existing call sites)
        // must persist and read back with actor_id / received_at as NULL/None —
        // solo/local-first behavior unchanged.
        let mut store = setup_store();

        let stored = store
            .append(EventEnvelope::new(sample_account("legacy", 1), "u".into()))
            .unwrap();
        assert_eq!(stored.actor_id, None);
        assert_eq!(stored.received_at, None);

        let got = store.get(stored.id).unwrap();
        assert_eq!(got.actor_id, None);
        assert_eq!(got.received_at, None);

        // The columns are actually NULL in the row (not empty strings).
        let (actor_null, recv_null): (bool, bool) = store
            .connection()
            .query_row(
                "SELECT actor_id IS NULL, received_at IS NULL FROM events WHERE id = ?1",
                [stored.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(actor_null && recv_null, "legacy identity columns must be NULL");

        let all = store.get_all().unwrap();
        assert_eq!(all[0].actor_id, None);
        assert_eq!(all[0].received_at, None);
    }

    #[test]
    fn test_duplicate_hash_rejected() {
        let mut store = setup_store();

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };

        let timestamp = Utc::now();
        let envelope1 =
            EventEnvelope::with_timestamp(event.clone(), "user-001".to_string(), timestamp);
        let envelope2 = EventEnvelope::with_timestamp(event, "user-001".to_string(), timestamp);

        store.append(envelope1).unwrap();
        let result = store.append(envelope2);

        assert!(matches!(result, Err(EventStoreError::DuplicateHash)));
    }

    #[test]
    fn test_get_all() {
        let mut store = setup_store();

        // Add multiple events
        for i in 1..=5 {
            let event = Event::AccountCreated {
                account_id: format!("acc-{:03}", i),
                account_type: EventAccountType::Asset,
                account_number: format!("{}", 1000 + i),
                name: format!("Account {}", i),
                parent_id: None,
                currency: None,
                description: None,
            };
            let envelope = EventEnvelope::new(event, "user-001".to_string());
            store.append(envelope).unwrap();
        }

        let all = store.get_all().unwrap();
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_get_by_type() {
        let mut store = setup_store();

        // Add account events
        let event1 = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        store
            .append(EventEnvelope::new(event1, "user-001".to_string()))
            .unwrap();

        // Add journal entry event
        let event2 = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "acc-001".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "acc-002".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: Some(JournalEntrySource::Manual),
        };
        store
            .append(EventEnvelope::new(event2, "user-001".to_string()))
            .unwrap();

        let accounts = store.get_by_type("account_created").unwrap();
        assert_eq!(accounts.len(), 1);

        let entries = store.get_by_type("journal_entry_posted").unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_get_after() {
        let mut store = setup_store();

        // Add 5 events
        for i in 1..=5 {
            let event = Event::AccountCreated {
                account_id: format!("acc-{:03}", i),
                account_type: EventAccountType::Asset,
                account_number: format!("{}", 1000 + i),
                name: format!("Account {}", i),
                parent_id: None,
                currency: None,
                description: None,
            };
            store
                .append(EventEnvelope::new(event, "user-001".to_string()))
                .unwrap();
        }

        let after_3 = store.get_after(3).unwrap();
        assert_eq!(after_3.len(), 2);
        assert_eq!(after_3[0].id, 4);
        assert_eq!(after_3[1].id, 5);
    }

    #[test]
    fn test_count() {
        let mut store = setup_store();

        assert_eq!(store.count().unwrap(), 0);

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        store
            .append(EventEnvelope::new(event, "user-001".to_string()))
            .unwrap();

        assert_eq!(store.count().unwrap(), 1);
    }
}
