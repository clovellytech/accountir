//! Applying a group server's log into a local ledger — the read half of
//! connected mode.
//!
//! SPEC §4.1 settled the multi-tenant model as **server-authoritative,
//! online-first**: the group server owns the log, clients follow it. This module
//! is the client's follow path, and its whole job is to make one invariant true
//! by construction:
//!
//! > **A connected ledger is a strict replica: local `events.id` == server `seq`,
//! > always, and the only writer to a replica's log is this mirror path.**
//!
//! Everything else falls out of that. The sync cursor is `MAX(events.id)` — no
//! separate column, so no way for the cursor to claim progress the data doesn't
//! have. A batch that doesn't continue the log exactly is refused whole. A file
//! that already holds local events can't be bound in the first place
//! ([`super::binding`]).
//!
//! # The failure this prevents
//!
//! A client silently accepting a log that isn't a prefix of the server's — a
//! dropped event, a re-ordered page, a payload mangled by a proxy or a bug — and
//! then reporting balances the rest of the group cannot reproduce. In accounting
//! that is not a glitch that resolves itself on the next refresh; it is two sets
//! of books that disagree, discovered at audit. So every divergence is surfaced
//! as an error the user must resolve ([`reset`]), never repaired by guessing.
//!
//! # What this module deliberately does not do
//!
//! No merge, no rebase, no CRDT, no quarantine of "local-only" events. Those are
//! the offline-write strategies §4.1 rejected. If the local copy has diverged,
//! the only honest recoveries are "throw away the local copy and re-pull" and
//! "stop syncing"; both are offered, and neither is taken automatically.

use super::binding;
use super::SyncEvent;
use crate::events::types::StoredEvent;
use crate::store::event_store::{EventStore, EventStoreError, MirrorEvent};
use crate::store::projections::{ProjectionError, Projector};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ReplicaError {
    /// The batch does not start at `local head + 1`, or skips a seq inside
    /// itself. Recoverable by re-requesting from the real cursor — unless the
    /// local head is genuinely past what the server has, which is [`LocalAhead`].
    ///
    /// [`LocalAhead`]: ReplicaError::LocalAhead
    #[error("the server's events don't continue this copy (expected #{expected}, got #{got}); re-syncing from #{expected}")]
    Gap { expected: i64, got: i64 },
    /// An event's payload did not re-derive the hash the server sent. Terminal:
    /// this is either tampering in transit or a genuine fork, and neither is
    /// something a retry fixes.
    #[error(
        "event #{seq} from the server doesn't match its own checksum — this copy can't be trusted"
    )]
    HashMismatch { seq: i64 },
    /// The server answered without the timestamp/hash a replica needs to verify
    /// what it is being given. Fail closed: a server that cannot prove its events
    /// does not get to fill our log.
    #[error("the group server is too old to sync with this app (event #{seq} arrived without a {missing})")]
    ServerTooOld { seq: i64, missing: &'static str },
    /// The local copy holds events the server does not. Under a
    /// server-authoritative model this cannot happen by syncing, so it means the
    /// file was written locally, or restored from a different group, or the
    /// server was itself restored from an older backup. There is no merge; the
    /// user is told.
    #[error("this copy has {local} events but the server only has {server} — it has diverged and must be reset")]
    LocalAhead { local: i64, server: i64 },
    #[error("storage error while applying the group's events: {0}")]
    Store(#[from] EventStoreError),
    #[error("could not rebuild the local views from the group's events: {0}")]
    Projection(#[from] ProjectionError),
}

impl ReplicaError {
    /// Whether the local copy is now known-bad, so the UI should stop syncing and
    /// offer "reset local copy" rather than a retry button.
    ///
    /// A [`Gap`] is *not* divergence — the usual cause is a page requested from a
    /// stale cursor, which the next tick fixes by asking from `local_cursor`
    /// again. Treating it as fatal would offer people a destructive reset for a
    /// transient paging error.
    ///
    /// [`Gap`]: ReplicaError::Gap
    pub fn is_divergence(&self) -> bool {
        matches!(
            self,
            ReplicaError::HashMismatch { .. } | ReplicaError::LocalAhead { .. }
        )
    }
}

/// The range a batch actually applied. `from`/`to` are inclusive server seqs;
/// an empty batch reports `applied == 0` and the caller's cursor is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedRange {
    pub from: i64,
    pub to: i64,
    pub applied: usize,
}

/// How far this replica has followed the server: the id of the last event it
/// holds, `0` for an empty ledger.
///
/// This is *the* cursor. It is read from the data rather than kept alongside it
/// precisely so the two cannot disagree — a stored cursor that survives a
/// rolled-back batch would make the client skip events forever, and the symptom
/// (a missing entry) would appear months later with nothing pointing at the
/// cause.
pub fn local_cursor(store: &EventStore) -> Result<i64, ReplicaError> {
    Ok(store.latest_id()?.unwrap_or(0))
}

/// Apply one page of server events to the local log, atomically.
///
/// The batch is validated before anything is written — every event must carry the
/// `timestamp` and `hash` needed to verify it — and then handed to
/// [`EventStore::append_mirrored`], which re-reads the head under the write lock,
/// enforces contiguity, recomputes each hash, inserts with the server's id and
/// projects each event **in the same transaction**. Any failure rolls the whole
/// page back: there is no partial apply, so the log never ends up half a page
/// ahead of the projections or vice versa.
pub fn apply_batch(
    store: &mut EventStore,
    events: &[SyncEvent],
) -> Result<AppliedRange, ReplicaError> {
    if events.is_empty() {
        let cursor = local_cursor(store)?;
        return Ok(AppliedRange {
            from: cursor,
            to: cursor,
            applied: 0,
        });
    }

    let mut mirrored = Vec::with_capacity(events.len());
    for e in events {
        mirrored.push(to_mirror_event(e)?);
    }

    let stored = store
        .append_mirrored(&mirrored, super::project)
        .map_err(map_store_error)?;

    Ok(AppliedRange {
        from: stored.first().map(|s| s.id).unwrap_or_default(),
        to: stored.last().map(|s| s.id).unwrap_or_default(),
        applied: stored.len(),
    })
}

/// Confirm the local copy really is a prefix of the server's, by comparing the
/// event at the local head with the server's event at the same seq.
///
/// Run on every business open. It is one request and one row, and it catches a
/// forked or tampered replica *before* it compounds: without it, a copy that
/// diverged at seq 40 keeps happily appending seq 41, 42, … and only reveals
/// itself as a balance nobody else can reproduce.
///
/// `at` is the server's event at [`local_cursor`]; an empty local log has nothing
/// to verify and passes trivially.
pub fn verify_prefix(store: &EventStore, at: Option<&SyncEvent>) -> Result<(), ReplicaError> {
    let cursor = local_cursor(store)?;
    if cursor == 0 {
        return Ok(());
    }
    let Some(server) = at else {
        // We hold events the server has never heard of.
        return Err(ReplicaError::LocalAhead {
            local: cursor,
            server: cursor - 1,
        });
    };
    if server.seq != cursor {
        return Err(ReplicaError::Gap {
            expected: cursor,
            got: server.seq,
        });
    }
    let Some(server_hash) = server.hash.as_deref() else {
        return Err(ReplicaError::ServerTooOld {
            seq: server.seq,
            missing: "checksum",
        });
    };
    let local = store.get_hash(cursor)?;
    if crate::events::payload::hash_to_hex(&local) != server_hash.to_ascii_lowercase() {
        return Err(ReplicaError::HashMismatch { seq: cursor });
    }
    Ok(())
}

/// Throw the local copy away and start following the server from zero.
///
/// The only offered recovery from divergence, and safe *only* because the server
/// is authoritative: everything deleted here exists on the server and comes back
/// on the next pull with identical ids (`events.id` is a plain
/// `INTEGER PRIMARY KEY`, so there is no `AUTOINCREMENT` sequence to leave a
/// gap). Refused on an unbound ledger, where the local log is the only copy and
/// this would be data loss with no source to restore from.
pub fn reset(store: &mut EventStore) -> Result<(), ReplicaError> {
    if !binding::is_bound(store).map_err(|e| ReplicaError::Store(store_err(e)))? {
        return Err(ReplicaError::Store(EventStoreError::Backend(
            "refusing to clear a ledger that is not a group replica — \
             its events exist nowhere else"
                .into(),
        )));
    }
    let tx = store
        .connection_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(EventStoreError::from)?;
    // Projections first, events second, and both in one transaction. Order,
    // because every projection row points back at the event that produced it
    // (`created_at_event REFERENCES events(id)`), so deleting the log first is a
    // foreign-key violation. Atomicity, because a half-done reset — a cleared log
    // with live projections — would show accounts and balances that no event
    // supports, which is the most convincing possible form of wrong.
    Projector::new(&tx).rebuild(&[])?;
    tx.execute("DELETE FROM events", [])
        .map_err(EventStoreError::from)?;
    tx.commit().map_err(EventStoreError::from)?;
    let _ = binding::record_sync(store.connection(), 0);
    Ok(())
}

/// Rebuild every projection table by replaying the local log.
///
/// For after a migration changes a projection's shape: the event log is the
/// source of truth and is left untouched, only the derived tables are recomputed.
/// Reuses [`Projector::rebuild`] rather than re-deriving the replay order, so a
/// projection added tomorrow is picked up here for free.
pub fn rebuild_projections(store: &mut EventStore) -> Result<(), ReplicaError> {
    let events: Vec<StoredEvent> = store.get_all()?;
    // One transaction: a rebuild that failed halfway would otherwise leave the app
    // reading tables that had been truncated and only partly replayed.
    let tx = store
        .connection_mut()
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(EventStoreError::from)?;
    Projector::new(&tx).rebuild(&events)?;
    tx.commit().map_err(EventStoreError::from)?;
    Ok(())
}

/// Convert a wire event into something we are willing to write, refusing anything
/// we could not verify. The `Option`s exist for wire compatibility with an older
/// server; a replica treats their absence as a hard stop, because an event we
/// cannot check is an event we cannot honestly claim came from the group.
fn to_mirror_event(e: &SyncEvent) -> Result<MirrorEvent, ReplicaError> {
    let timestamp = e.timestamp.ok_or(ReplicaError::ServerTooOld {
        seq: e.seq,
        missing: "timestamp",
    })?;
    let hash_hex = e.hash.as_deref().ok_or(ReplicaError::ServerTooOld {
        seq: e.seq,
        missing: "checksum",
    })?;
    let hash = hex::decode(hash_hex).map_err(|_| ReplicaError::HashMismatch { seq: e.seq })?;
    Ok(MirrorEvent {
        seq: e.seq,
        event: e.event.clone(),
        user_id: e.user_id.clone(),
        timestamp,
        actor_id: e.actor_id.clone(),
        received_at: e.received_at,
        hash,
    })
}

/// Translate the store's mirror errors into the replica vocabulary, so callers
/// match on one enum and the "is this fatal?" question has one answer.
fn map_store_error(e: EventStoreError) -> ReplicaError {
    match e {
        EventStoreError::MirrorGap { expected, got } => ReplicaError::Gap { expected, got },
        EventStoreError::MirrorHashMismatch { seq } => ReplicaError::HashMismatch { seq },
        other => ReplicaError::Store(other),
    }
}

fn store_err(e: binding::BindingError) -> EventStoreError {
    match e {
        binding::BindingError::Database(e) => EventStoreError::DatabaseError(e),
        other => EventStoreError::Backend(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::payload::hash_to_hex;
    use crate::events::types::{Event, EventEnvelope};
    use crate::store::migrations::SchemaStore;
    use chrono::{TimeZone, Utc};

    /// A stand-in for the group server: its own EventStore, written through the
    /// normal append path, then read back as wire events exactly as
    /// `GET /sync/events` would serve them.
    fn server_with(n: usize) -> EventStore {
        let mut s = store();
        for i in 0..n {
            s.append(account_event(i)).unwrap();
        }
        s
    }

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        s.init_schema().unwrap();
        s.run_migrations().unwrap();
        s
    }

    fn replica() -> EventStore {
        let s = store();
        binding::bind(s.connection(), "acme", "https://i", "https://cp").unwrap();
        s
    }

    fn account_event(i: usize) -> EventEnvelope {
        EventEnvelope::new(
            Event::AccountCreated {
                account_id: format!("a{i}"),
                account_number: format!("1{i:03}"),
                name: format!("Account {i}"),
                account_type: crate::events::types::EventAccountType::Asset,
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            },
            "server-user".into(),
        )
    }

    fn wire(server: &EventStore, since: i64) -> Vec<SyncEvent> {
        server
            .get_after(since)
            .unwrap()
            .into_iter()
            .map(SyncEvent::from)
            .collect()
    }

    /// The core invariant: after mirroring, the replica's row ids ARE the
    /// server's seqs. Everything else in this module (the cursor, contiguity,
    /// prefix verification) is meaningless if this drifts.
    #[test]
    fn mirroring_preserves_server_sequence_numbers_exactly() {
        let server = server_with(3);
        let mut r = replica();
        let batch = wire(&server, 0);
        let applied = apply_batch(&mut r, &batch).unwrap();
        assert_eq!(applied.applied, 3);
        assert_eq!((applied.from, applied.to), (1, 3));

        let server_ids: Vec<i64> = server.get_all().unwrap().iter().map(|e| e.id).collect();
        let local_ids: Vec<i64> = r.get_all().unwrap().iter().map(|e| e.id).collect();
        assert_eq!(server_ids, local_ids);
        assert_eq!(local_cursor(&r).unwrap(), 3);

        // And the hashes match byte for byte, which is what makes prefix
        // verification meaningful.
        for (s, l) in server.get_all().unwrap().iter().zip(r.get_all().unwrap()) {
            assert_eq!(s.hash, l.hash);
        }
    }

    /// The regression: a dropped page leaving a hole in the local log that
    /// nothing ever notices, because the cursor jumped past it.
    #[test]
    fn mirrored_batch_with_a_gap_is_rejected_whole() {
        let server = server_with(4);
        let mut r = replica();
        let mut batch = wire(&server, 0);
        batch.remove(2); // 1, 2, 4
        let err = apply_batch(&mut r, &batch).unwrap_err();
        assert!(
            matches!(
                err,
                ReplicaError::Gap {
                    expected: 3,
                    got: 4
                }
            ),
            "{err}"
        );
        // Whole batch or nothing: the two events before the hole must not survive.
        assert_eq!(local_cursor(&r).unwrap(), 0);
        assert_eq!(r.count().unwrap(), 0);
    }

    /// The regression: accepting a payload that was altered in transit (a broken
    /// proxy, a mangled encoding, an attacker) and baking it into the ledger.
    #[test]
    fn mirrored_event_whose_hash_does_not_recompute_is_refused() {
        let server = server_with(2);
        let mut r = replica();
        let mut batch = wire(&server, 0);
        // Same hash, different payload — exactly what a tampered response looks
        // like to a client that trusts what it is told.
        if let Event::AccountCreated { name, .. } = &mut batch[1].event {
            *name = "Attacker's account".into();
        }
        let err = apply_batch(&mut r, &batch).unwrap_err();
        assert!(
            matches!(err, ReplicaError::HashMismatch { seq: 2 }),
            "{err}"
        );
        assert!(err.is_divergence());
        assert_eq!(r.count().unwrap(), 0);
    }

    /// Hash verification rests entirely on `timestamp.to_rfc3339()` producing the
    /// same bytes after a JSON round trip. If chrono ever renders a round-tripped
    /// timestamp differently, every pull would fail as a hash mismatch — so this
    /// pins the assumption rather than letting it fail in the field.
    #[test]
    fn round_tripped_chrono_timestamp_recomputes_the_stored_hash() {
        let server = server_with(1);
        let stored = &server.get_all().unwrap()[0];
        let json = serde_json::to_string(&SyncEvent::from(stored.clone())).unwrap();
        let back: SyncEvent = serde_json::from_str(&json).unwrap();
        let recomputed = crate::events::payload::compute_event_hash(
            &back.event,
            &back.timestamp.unwrap().to_rfc3339(),
            &back.user_id,
        )
        .unwrap();
        assert_eq!(hash_to_hex(&recomputed), back.hash.unwrap());

        // Also pin a sub-second, non-UTC-offset timestamp, since that is where
        // RFC3339 renderings differ.
        let ts = Utc.with_ymd_and_hms(2026, 7, 25, 13, 45, 6).unwrap()
            + chrono::Duration::milliseconds(789);
        let round_tripped: chrono::DateTime<Utc> =
            serde_json::from_str(&serde_json::to_string(&ts).unwrap()).unwrap();
        assert_eq!(ts.to_rfc3339(), round_tripped.to_rfc3339());
    }

    /// Fail closed against an older server: without a timestamp or a hash the
    /// client cannot verify what it is being handed, and "can't verify" must mean
    /// "won't write", not "write it anyway".
    #[test]
    fn mirror_path_refuses_when_the_server_omits_timestamp_or_hash() {
        let server = server_with(1);
        let mut r = replica();

        let mut no_ts = wire(&server, 0);
        no_ts[0].timestamp = None;
        assert!(matches!(
            apply_batch(&mut r, &no_ts).unwrap_err(),
            ReplicaError::ServerTooOld {
                missing: "timestamp",
                ..
            }
        ));

        let mut no_hash = wire(&server, 0);
        no_hash[0].hash = None;
        assert!(matches!(
            apply_batch(&mut r, &no_hash).unwrap_err(),
            ReplicaError::ServerTooOld {
                missing: "checksum",
                ..
            }
        ));
        assert_eq!(r.count().unwrap(), 0);
    }

    /// The log must never lead the projections: if projecting event N fails, the
    /// events for the whole batch have to go back too, or the next pull starts
    /// from a cursor whose projections were never built.
    #[test]
    fn applying_a_batch_projects_in_the_same_transaction_so_a_projection_failure_rolls_back_the_events(
    ) {
        let server = server_with(2);
        let mut r = replica();
        let batch: Vec<MirrorEvent> = wire(&server, 0)
            .iter()
            .map(|e| to_mirror_event(e).unwrap())
            .collect();

        let err = r
            .append_mirrored(&batch, |_tx, stored| {
                if stored.id == 2 {
                    Err(EventStoreError::Projection("boom".into()))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
        assert!(matches!(err, EventStoreError::Projection(_)), "{err}");
        assert_eq!(r.count().unwrap(), 0, "event 1 must not survive");
    }

    /// Pulling in pages must land in exactly the same place as pulling in one go —
    /// this is what makes the `limit` on `/sync/events` safe to use.
    #[test]
    fn applying_the_log_in_pages_reaches_the_same_state_as_one_batch() {
        let server = server_with(5);
        let mut paged = replica();
        loop {
            let cursor = local_cursor(&paged).unwrap();
            let page: Vec<SyncEvent> = wire(&server, cursor).into_iter().take(2).collect();
            if page.is_empty() {
                break;
            }
            apply_batch(&mut paged, &page).unwrap();
        }
        let mut whole = replica();
        apply_batch(&mut whole, &wire(&server, 0)).unwrap();

        assert_eq!(
            paged.get_all_hashes().unwrap(),
            whole.get_all_hashes().unwrap()
        );
        assert_eq!(local_cursor(&paged).unwrap(), 5);
    }

    #[test]
    fn verifying_the_prefix_accepts_a_faithful_copy_and_rejects_a_forked_one() {
        let server = server_with(3);
        let mut r = replica();
        apply_batch(&mut r, &wire(&server, 0)).unwrap();

        let at_head = wire(&server, 2).into_iter().next().unwrap();
        verify_prefix(&r, Some(&at_head)).unwrap();

        // A server whose event #3 is not ours: the copy forked somewhere.
        let other = server_with(3);
        let mut theirs = SyncEvent::from(other.get(3).unwrap());
        theirs.seq = 3;
        let err = verify_prefix(&r, Some(&theirs)).unwrap_err();
        assert!(
            matches!(err, ReplicaError::HashMismatch { seq: 3 }),
            "{err}"
        );
        assert!(err.is_divergence());
    }

    /// An empty replica has nothing to verify — asking it to would make the very
    /// first sync of a new ledger fail.
    #[test]
    fn verifying_the_prefix_of_an_empty_copy_is_trivially_ok() {
        let r = replica();
        verify_prefix(&r, None).unwrap();
    }

    /// Holding events the server has never heard of is the one thing this design
    /// has no answer for, so it must be named rather than papered over.
    #[test]
    fn a_local_copy_ahead_of_the_server_is_reported_as_divergence() {
        let server = server_with(2);
        let mut r = replica();
        apply_batch(&mut r, &wire(&server, 0)).unwrap();
        let err = verify_prefix(&r, None).unwrap_err();
        assert!(
            matches!(err, ReplicaError::LocalAhead { local: 2, .. }),
            "{err}"
        );
        assert!(err.is_divergence());
    }

    #[test]
    fn reset_clears_events_and_projections_and_repull_reproduces_identical_ids() {
        let server = server_with(3);
        let mut r = replica();
        apply_batch(&mut r, &wire(&server, 0)).unwrap();
        let before: Vec<Vec<u8>> = r.get_all_hashes().unwrap();
        let accounts_before: i64 = r
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |x| x.get(0))
            .unwrap();
        assert_eq!(accounts_before, 3);

        reset(&mut r).unwrap();
        assert_eq!(r.count().unwrap(), 0);
        assert_eq!(local_cursor(&r).unwrap(), 0);
        let accounts_after: i64 = r
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |x| x.get(0))
            .unwrap();
        assert_eq!(accounts_after, 0, "projections must be cleared too");

        // Re-pull: ids come back identical because `events.id` has no
        // AUTOINCREMENT sequence to leave a gap.
        apply_batch(&mut r, &wire(&server, 0)).unwrap();
        assert_eq!(r.get_all_hashes().unwrap(), before);
        assert_eq!(
            r.get_all()
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    /// Reset is data loss with no source to restore from unless the ledger really
    /// is a replica. The binding is the check, and it has to be enforced here
    /// rather than only in the UI.
    #[test]
    fn reset_refuses_on_a_ledger_that_is_not_a_replica() {
        let mut solo = store();
        solo.append(account_event(0)).unwrap();
        let err = reset(&mut solo).unwrap_err();
        assert!(err.to_string().contains("not a group replica"), "{err}");
        assert_eq!(solo.count().unwrap(), 1);
    }

    #[test]
    fn rebuilding_projections_replays_the_local_log_without_touching_it() {
        let server = server_with(3);
        let mut r = replica();
        apply_batch(&mut r, &wire(&server, 0)).unwrap();
        // Simulate a projection table that a migration has just changed shape of.
        r.connection().execute("DELETE FROM accounts", []).unwrap();

        rebuild_projections(&mut r).unwrap();
        let accounts: i64 = r
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |x| x.get(0))
            .unwrap();
        assert_eq!(accounts, 3);
        assert_eq!(r.count().unwrap(), 3, "the event log is untouched");
    }

    /// A page that re-sends events we already hold (a retry, a duplicated tick)
    /// must be refused as non-contiguous rather than half-applied — and it must
    /// not corrupt what we already have.
    #[test]
    fn re_applying_an_already_applied_page_is_refused_and_changes_nothing() {
        let server = server_with(2);
        let mut r = replica();
        let batch = wire(&server, 0);
        apply_batch(&mut r, &batch).unwrap();
        let err = apply_batch(&mut r, &batch).unwrap_err();
        assert!(
            matches!(
                err,
                ReplicaError::Gap {
                    expected: 3,
                    got: 1
                }
            ),
            "{err}"
        );
        assert!(!err.is_divergence(), "a stale page is not a forked ledger");
        assert_eq!(r.count().unwrap(), 2);
    }
}
