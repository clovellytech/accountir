//! Phase-0 spike: the `expected_head_seq` compare-and-append handshake.
//!
//! Goal (SPEC §7 Phase 0): prove that `EventStore::append_expecting` gives us a
//! working optimistic-concurrency primitive for the server append path —
//! two online clients racing to append against the same head, one wins, the
//! loser is told to refetch and retry — and that this is enough to close the
//! read-then-append TOCTOU races catalogued in
//! `docs/multitenant-invariant-audit.md`.
//!
//! Three tests, increasing in fidelity:
//!   1. `deterministic_conflict_then_retry` — the handshake, hand-interleaved.
//!   2. `real_threads_serialize` — two OS threads, two connections, one WAL
//!      file: proves the IMMEDIATE-txn head check actually serializes writers.
//!   3. `double_payment_is_prevented` — the marquee invariant fix: two clients
//!      each apply a payment that alone is valid but together overpay a bill;
//!      the CAS conflict forces the loser to re-derive against the new state and
//!      correctly reject.
//!
//! Finding (recorded in the audit's "Phase 1 must build"): a *matching* head at
//! commit means no event landed since the caller read state, so a strict global
//! head CAS is by itself sufficient to preserve every invariant — at the cost of
//! a false conflict when two *unrelated* events race (test 2 shows the retries).
//! Fine-grained per-event acceptance is a Phase-3 optimization, not needed for
//! correctness in v1.

use crate::events::types::{Event, EventAccountType, EventEnvelope};
use crate::store::event_store::{AppendOutcome, EventStore};
use crate::store::migrations::init_schema;

/// A distinct, validation-passing event whose content (and therefore hash) is
/// unique per `(tag, n)`, so concurrent appends never collide on `UNIQUE(hash)`.
fn unique_event(tag: &str, n: usize) -> Event {
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

fn head(store: &EventStore) -> i64 {
    store.latest_id().unwrap().unwrap_or(0)
}

#[test]
fn deterministic_conflict_then_retry() {
    let mut store = EventStore::in_memory().unwrap();
    init_schema(store.connection()).unwrap();

    // Both clients observe the same head (empty log → 0).
    let h = head(&store);
    assert_eq!(h, 0);

    // Client A wins the race.
    let a = store
        .append_expecting(EventEnvelope::new(unique_event("A", 1), "alice".into()), h)
        .unwrap();
    let a_id = match a {
        AppendOutcome::Appended(ev) => ev.id,
        other => panic!("A should have appended, got {other:?}"),
    };
    assert_eq!(a_id, 1);

    // Client B submits against the now-stale head and is told to retry.
    match store
        .append_expecting(EventEnvelope::new(unique_event("B", 1), "bob".into()), h)
        .unwrap()
    {
        AppendOutcome::HeadMismatch { expected, actual } => {
            assert_eq!(expected, 0);
            assert_eq!(actual, 1);
        }
        other => panic!("B should have conflicted, got {other:?}"),
    }

    // B refetches the head and retries — this time it lands.
    let h2 = head(&store);
    match store
        .append_expecting(EventEnvelope::new(unique_event("B", 1), "bob".into()), h2)
        .unwrap()
    {
        AppendOutcome::Appended(ev) => assert_eq!(ev.id, 2),
        other => panic!("B retry should have appended, got {other:?}"),
    }

    assert_eq!(store.count().unwrap(), 2);
}

#[test]
fn real_threads_serialize() {
    // Two connections to one on-disk WAL database, one per thread, each racing
    // to append PER_THREAD events with retry-on-conflict. If the CAS were not
    // atomic we'd see lost updates (count < 2*PER_THREAD) or a panic on a
    // duplicate id; instead every append lands exactly once, gaplessly.
    const PER_THREAD: usize = 50;

    let dir = std::env::temp_dir().join(format!("accountir-cas-spike-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("log.db");

    // Initialize the schema once up front.
    {
        let store = EventStore::open(&db).unwrap();
        init_schema(store.connection()).unwrap();
    }

    let worker = |tag: &'static str, path: std::path::PathBuf| {
        move || {
            let mut store = EventStore::open(&path).unwrap();
            let mut appended = 0usize;
            let mut conflicts = 0usize;
            let mut i = 0usize;
            while appended < PER_THREAD {
                let h = head(&store);
                let env = EventEnvelope::new(unique_event(tag, i), tag.into());
                match store.append_expecting(env, h).unwrap() {
                    AppendOutcome::Appended(_) => {
                        appended += 1;
                        i += 1;
                    }
                    AppendOutcome::HeadMismatch { .. } => conflicts += 1,
                }
            }
            conflicts
        }
    };

    let t1 = std::thread::spawn(worker("t1", db.clone()));
    let t2 = std::thread::spawn(worker("t2", db.clone()));
    let c1 = t1.join().unwrap();
    let c2 = t2.join().unwrap();

    let store = EventStore::open(&db).unwrap();
    let total = store.count().unwrap();
    assert_eq!(
        total,
        2 * PER_THREAD as i64,
        "every append must land exactly once (no lost updates)"
    );
    // Gapless 1..=2N: max id equals count, so no id was skipped or reused.
    assert_eq!(store.latest_id().unwrap(), Some(2 * PER_THREAD as i64));

    // Sanity: with two racing writers we expect at least some conflicts to have
    // occurred — otherwise the test isn't exercising contention at all.
    assert!(
        c1 + c2 > 0,
        "expected the writers to actually contend (got 0 conflicts)"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn double_payment_is_prevented() {
    // Audit finding #2: two concurrent partial payments each read the same
    // remaining balance and both apply, overpaying the bill. This test shows
    // the CAS closing that TOCTOU: the invariant ("sum of payments <= bill
    // amount") is re-checked by the client after a HeadMismatch, against the
    // freshly-observed log.
    let mut store = EventStore::in_memory().unwrap();
    init_schema(store.connection()).unwrap();

    const BILL: &str = "bill-1";
    const BILL_AMOUNT: i64 = 100;

    // Total already applied to BILL, folded from the authoritative log.
    let applied = |store: &EventStore| -> i64 {
        store
            .get_all()
            .unwrap()
            .iter()
            .filter_map(|se| match &se.event {
                Event::BillPaymentApplied {
                    bill_id,
                    amount_applied,
                    ..
                } if bill_id == BILL => Some(*amount_applied),
                _ => None,
            })
            .sum()
    };

    let payment = |n: usize, amt: i64| Event::BillPaymentApplied {
        bill_id: BILL.into(),
        payment_entry_id: format!("pay-{n}"),
        amount_applied: amt,
    };

    // Both clients read head=0 and each independently decides a 60 payment is
    // fine (0 applied so far, 60 <= 100).
    let h = head(&store);
    assert_eq!(applied(&store), 0);

    // Client A's 60 lands.
    match store
        .append_expecting(EventEnvelope::new(payment(1, 60), "alice".into()), h)
        .unwrap()
    {
        AppendOutcome::Appended(_) => {}
        other => panic!("A payment should land, got {other:?}"),
    }

    // Client B's 60, computed against the stale head, conflicts.
    match store
        .append_expecting(EventEnvelope::new(payment(2, 60), "bob".into()), h)
        .unwrap()
    {
        AppendOutcome::HeadMismatch { .. } => {}
        other => panic!("B payment should conflict, got {other:?}"),
    }

    // On conflict B refetches head AND re-derives: now 60 is applied, so a
    // second 60 would overpay (120 > 100). B rejects it *before* resubmitting —
    // the invariant holds because the head match would have guaranteed a
    // current view, and here the head *didn't* match so B re-read the truth.
    let h2 = head(&store);
    let would_be_total = applied(&store) + 60;
    let overpays = would_be_total > BILL_AMOUNT;
    assert!(overpays, "re-derivation must see the overpay");

    if !overpays {
        // (Not reached in this scenario; shown for symmetry — a non-overpaying
        // retry would resubmit here against h2.)
        let _ = store.append_expecting(EventEnvelope::new(payment(2, 60), "bob".into()), h2);
    }

    // Exactly one payment landed; the bill is not overpaid.
    assert_eq!(applied(&store), 60);
    assert!(applied(&store) <= BILL_AMOUNT);
}
