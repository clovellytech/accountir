//! Undoing a batch of events by appending their inverses.
//!
//! # Why not delete
//!
//! The obvious reading of "roll back" is to drop the events and rebuild. Nothing
//! structural forbids it — the hashes are per-event, not chained, so a truncated
//! log still verifies — but two things make it the wrong default.
//!
//! The log is replicated. On group-hosted books every replica holds the same
//! events, and a machine that deletes its tail has not undone anything; it has
//! disagreed with everyone else, and the next pull brings the events straight
//! back. An append is a fact the whole group receives.
//!
//! And a rewind is indiscriminate. The mistake that prompted this — 147 lines
//! moved off a credit card in one keystroke — was followed within four minutes
//! by twenty-eight *correct* reassignments. Rewinding past the mistake throws
//! those away too, and the user has no way to tell what they lost. Reverting a
//! batch touches exactly the batch.
//!
//! So the log only ever grows: a revert is new events saying the opposite thing,
//! and the history keeps both the mistake and its correction. That is the honest
//! record of what happened, and it is also the one that can itself be reverted.
//!
//! # What a batch is
//!
//! Not a stored concept — the log has no notion of a bulk operation, only of
//! events. A batch is recovered from the shape of the log: a maximal run of
//! consecutive ids with the same event type and writer, where no two neighbours
//! are more than [`BATCH_GAP`] apart.
//!
//! That threshold is what separates "one keystroke" from "several things a
//! person did in a row". A bulk reassign writes its events a few milliseconds
//! apart; somebody clicking through transactions one at a time takes seconds
//! between them. On the ledger this was built for the 147-line batch spans 375ms
//! end to end and the next action is 10.7 seconds later, which is not a close
//! call — but the gap is generous enough that a slow machine writing a large
//! batch stays one batch.
//!
//! # What can be reverted
//!
//! Three event types, which is what "I did the wrong thing to a lot of rows"
//! actually produces:
//!
//! | Event | Inverse | Refused when |
//! |---|---|---|
//! | `JournalLineReassigned` | reassign back | the line has moved again since |
//! | `JournalEntryPosted` | void | already voided |
//! | `JournalEntryVoided` | unvoid | not currently voided |
//!
//! Anything else is reported as not revertible rather than guessed at. There is
//! no safe generic inverse for "an account was renamed" without knowing what it
//! was called before, and inventing one would produce a revert that silently did
//! less than it claimed.
//!
//! # Superseded events are refused, not skipped
//!
//! A revert is all-or-nothing. If any event in the batch has been overtaken —
//! the line was moved again afterwards, the entry was voided by hand — the whole
//! revert is refused and says which ones. Reverting the rest would leave the
//! batch half-undone, which is a state nobody asked for and which no longer
//! corresponds to any point in the ledger's history.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use thiserror::Error;

use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;

/// How far apart two events can be and still count as one operation.
///
/// See the module docs: milliseconds within a batch, seconds between human
/// actions. Two seconds sits between those by a wide margin in both directions.
pub const BATCH_GAP: Duration = Duration::milliseconds(2000);

#[derive(Error, Debug)]
pub enum RevertError {
    #[error("no events in the range {lo}..={hi}")]
    EmptyRange { lo: i64, hi: i64 },
    #[error(
        "this batch is {kind} events, which cannot be reverted automatically. Only \
         reassignments, postings and voids have an inverse the ledger can derive."
    )]
    NotRevertible { kind: String },
    #[error(
        "{count} of the {total} events in this batch have been overtaken since, so \
         reverting would undo some of the batch and not the rest:\n{detail}"
    )]
    Superseded {
        count: usize,
        total: usize,
        detail: String,
    },
    #[error("the ledger moved while this revert was being prepared; try again")]
    Contended,
    #[error(transparent)]
    Store(#[from] EventStoreError),
    #[error(transparent)]
    Database(#[from] rusqlite::Error),
}

/// One run of events written together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub lo: i64,
    pub hi: i64,
    pub event_type: String,
    pub user_id: String,
    pub at: DateTime<Utc>,
    pub count: usize,
    /// What it did, in a line somebody can recognise their own action in.
    pub summary: String,
    /// Whether [`plan_revert`] would even attempt it. A batch can be revertible
    /// in kind and still fail on the state check.
    pub revertible: bool,
}

/// Recover the batches in the log, newest first.
///
/// `limit` counts batches, not events: a screen wants the last twenty things
/// that happened, and one of those things may be two thousand events.
pub fn batches(conn: &Connection, limit: usize) -> Result<Vec<Batch>, RevertError> {
    let mut stmt = conn.prepare(
        "SELECT id, event_type, user_id, timestamp, payload FROM events ORDER BY id DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;

    let mut out: Vec<Batch> = Vec::new();
    let mut current: Option<(Batch, DateTime<Utc>, Vec<String>)> = None;

    for row in rows {
        let (id, event_type, user_id, ts, payload) = row?;
        let at = DateTime::parse_from_rfc3339(&ts)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        // Walking backwards, so `oldest` is the earlier end of the run so far and
        // the new event joins it only if it is close enough *below* it.
        let joins = match &current {
            Some((b, oldest, _)) => {
                b.event_type == event_type
                    && b.user_id == user_id
                    && b.lo == id + 1
                    && *oldest - at <= BATCH_GAP
            }
            None => false,
        };

        if joins {
            let (b, oldest, payloads) = current.as_mut().expect("joins implies some");
            b.lo = id;
            b.count += 1;
            *oldest = at;
            payloads.push(payload);
            continue;
        }

        if let Some((b, _, payloads)) = current.take() {
            out.push(finish(conn, b, &payloads));
            if out.len() >= limit {
                return Ok(out);
            }
        }
        current = Some((
            Batch {
                lo: id,
                hi: id,
                event_type: event_type.clone(),
                user_id,
                at,
                count: 1,
                summary: String::new(),
                revertible: revertible_kind(&event_type),
            },
            at,
            vec![payload],
        ));
    }
    if let Some((b, _, payloads)) = current.take() {
        out.push(finish(conn, b, &payloads));
    }
    out.truncate(limit);
    Ok(out)
}

fn revertible_kind(event_type: &str) -> bool {
    matches!(
        event_type,
        "journal_line_reassigned" | "journal_entry_posted" | "journal_entry_voided"
    )
}

/// Fill in a batch's human summary from the payloads it collected.
fn finish(conn: &Connection, mut batch: Batch, payloads: &[String]) -> Batch {
    batch.summary = summarise(conn, &batch, payloads);
    batch
}

/// Name the accounts a run of reassignments moved lines between.
///
/// The account pair is the whole point of recognising your own mistake in a
/// list: "147 reassignments" could be anything, and "147 lines: 5003 USBank →
/// 6001 Amazon" is the thing you remember doing.
fn summarise(conn: &Connection, batch: &Batch, payloads: &[String]) -> String {
    let n = batch.count;
    // Only the two nouns this function actually uses. A general pluraliser would
    // be wrong more often than it was right, and "185 entrys posted" is the kind
    // of thing that makes a screen look untrustworthy.
    fn plural<'a>(n: usize, singular: &'a str, many: &'a str) -> &'a str {
        if n == 1 { singular } else { many }
    }
    match batch.event_type.as_str() {
        "journal_line_reassigned" => {
            let mut pairs: Vec<(String, String)> = Vec::new();
            for p in payloads {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(p) {
                    let old = v["old_account_id"].as_str().unwrap_or("").to_string();
                    let new = v["new_account_id"].as_str().unwrap_or("").to_string();
                    if !pairs.contains(&(old.clone(), new.clone())) {
                        pairs.push((old, new));
                    }
                }
            }
            let names = account_names(conn);
            let name = |id: &String| names.get(id).cloned().unwrap_or_else(|| "?".to_string());
            match pairs.len() {
                1 => format!(
                    "{n} {}: {} \u{2192} {}",
                    plural(n, "line", "lines"),
                    name(&pairs[0].0),
                    name(&pairs[0].1)
                ),
                // More than one pair in a single keystroke means rows from
                // several accounts were selected together; the destination is
                // still the thing that was chosen, so lead with it.
                _ => {
                    let mut dests: Vec<String> = Vec::new();
                    for (_, d) in &pairs {
                        let d = name(d);
                        if !dests.contains(&d) {
                            dests.push(d);
                        }
                    }
                    format!("{n} {} \u{2192} {}", plural(n, "line", "lines"), dests.join(", "))
                }
            }
        }
        "journal_entry_posted" => format!("{n} {} posted", plural(n, "entry", "entries")),
        "journal_entry_voided" => format!("{n} {} voided", plural(n, "entry", "entries")),
        // No inverse and no noun worth guessing at, so the event type carries
        // itself. "3 x plaid account mapped" rather than a wrong plural.
        other if n == 1 => other.replace('_', " "),
        other => format!("{n} \u{d7} {}", other.replace('_', " ")),
    }
}

fn account_names(conn: &Connection) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, account_number, name FROM accounts") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                format!("{} {}", r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            ))
        }) {
            for (id, label) in rows.flatten() {
                out.insert(id, label);
            }
        }
    }
    out
}

/// What reverting a range would append, without appending it.
#[derive(Debug, Clone)]
pub struct RevertPlan {
    /// The inverse events, in the order they will be written.
    pub events: Vec<Event>,
    /// One line per inverse, for a confirmation the user can actually read.
    pub description: Vec<String>,
}

/// Derive the inverse of every event in `lo..=hi`, refusing if any has been
/// overtaken.
pub fn plan_revert(conn: &Connection, lo: i64, hi: i64) -> Result<RevertPlan, RevertError> {
    let mut stmt = conn.prepare(
        "SELECT id, event_type, payload FROM events WHERE id BETWEEN ?1 AND ?2 ORDER BY id DESC",
    )?;
    let rows: Vec<(i64, String, String)> = stmt
        .query_map([lo, hi], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?
        .collect::<Result<_, _>>()?;

    if rows.is_empty() {
        return Err(RevertError::EmptyRange { lo, hi });
    }
    if let Some((_, kind, _)) = rows.iter().find(|(_, k, _)| !revertible_kind(k)) {
        return Err(RevertError::NotRevertible { kind: kind.clone() });
    }

    let names = account_names(conn);
    let label = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_string());

    let total = rows.len();
    let mut events = Vec::with_capacity(total);
    let mut description = Vec::with_capacity(total);
    let mut superseded: Vec<String> = Vec::new();

    // Newest first: undoing a run means walking back up it, and where one batch
    // touched a line twice only that order returns it to where it started.
    for (id, kind, payload) in rows {
        let v: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;
        match kind.as_str() {
            "journal_line_reassigned" => {
                let entry_id = v["entry_id"].as_str().unwrap_or_default().to_string();
                let line_id = v["line_id"].as_str().unwrap_or_default().to_string();
                let old = v["old_account_id"].as_str().unwrap_or_default().to_string();
                let new = v["new_account_id"].as_str().unwrap_or_default().to_string();

                let now: Option<String> = conn
                    .query_row(
                        "SELECT account_id FROM journal_lines WHERE id = ?1",
                        [&line_id],
                        |r| r.get(0),
                    )
                    .ok();
                match now {
                    Some(ref a) if *a == new => {
                        description.push(format!(
                            "put line {line_id} back on {} (from {})",
                            label(&old),
                            label(&new)
                        ));
                        events.push(Event::JournalLineReassigned {
                            entry_id,
                            line_id,
                            old_account_id: new,
                            new_account_id: old,
                        });
                    }
                    Some(a) => superseded.push(format!(
                        "  event {id}: line {line_id} is on {} now, not {}",
                        label(&a),
                        label(&new)
                    )),
                    None => superseded
                        .push(format!("  event {id}: line {line_id} no longer exists")),
                }
            }
            "journal_entry_posted" => {
                let entry_id = v["entry_id"].as_str().unwrap_or_default().to_string();
                let voided: Option<i64> = conn
                    .query_row(
                        "SELECT COALESCE(is_void, 0) FROM journal_entries WHERE id = ?1",
                        [&entry_id],
                        |r| r.get(0),
                    )
                    .ok();
                match voided {
                    Some(0) => {
                        description.push(format!("void entry {entry_id}"));
                        events.push(Event::JournalEntryVoided {
                            entry_id,
                            reason: format!("reverted events {lo}\u{2013}{hi}"),
                        });
                    }
                    Some(_) => superseded
                        .push(format!("  event {id}: entry {entry_id} is already voided")),
                    None => superseded
                        .push(format!("  event {id}: entry {entry_id} no longer exists")),
                }
            }
            "journal_entry_voided" => {
                let entry_id = v["entry_id"].as_str().unwrap_or_default().to_string();
                let voided: Option<i64> = conn
                    .query_row(
                        "SELECT COALESCE(is_void, 0) FROM journal_entries WHERE id = ?1",
                        [&entry_id],
                        |r| r.get(0),
                    )
                    .ok();
                match voided {
                    Some(0) => superseded
                        .push(format!("  event {id}: entry {entry_id} is not voided")),
                    Some(_) => {
                        description.push(format!("un-void entry {entry_id}"));
                        events.push(Event::JournalEntryUnvoided {
                            entry_id,
                            reason: format!("reverted events {lo}\u{2013}{hi}"),
                        });
                    }
                    None => superseded
                        .push(format!("  event {id}: entry {entry_id} no longer exists")),
                }
            }
            other => return Err(RevertError::NotRevertible { kind: other.to_string() }),
        }
    }

    if !superseded.is_empty() {
        return Err(RevertError::Superseded {
            count: superseded.len(),
            total,
            detail: superseded.join("\n"),
        });
    }
    Ok(RevertPlan { events, description })
}

/// Append the inverse of every event in `lo..=hi`, atomically.
///
/// The plan is rebuilt inside the append transaction rather than trusted from a
/// previous call: between a user reading a confirmation and pressing it, the
/// line it was about can have moved, and a plan computed outside the lock would
/// happily write an inverse for a state that no longer holds.
pub fn revert(store: &mut EventStore, user_id: &str, lo: i64, hi: i64) -> Result<usize, RevertError> {
    let user_id = user_id.to_string();
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let outcome = store.append_checked_many(
            head,
            |tx| {
                let plan = match plan_revert(tx, lo, hi) {
                    Ok(p) => p,
                    Err(e) => return Ok(Verdict::Reject(e)),
                };
                Ok(Verdict::Append(
                    plan.events
                        .into_iter()
                        .map(|event| EventEnvelope::new(event, user_id.clone()))
                        .collect(),
                ))
            },
            |tx, stored: &StoredEvent| {
                Projector::new(tx).apply(stored).map_err(|e| {
                    EventStoreError::SerializationError(format!("projection failed: {e}"))
                })
            },
        )?;
        match outcome {
            CheckedOutcome::Appended(events) => return Ok(events.len()),
            CheckedOutcome::Rejected(e) => return Err(e),
            CheckedOutcome::HeadMismatch { .. } => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{
        EntryCommands, EntryLine, PostEntryCommand, ReassignLineCommand,
    };
    use crate::domain::AccountType;
    use crate::store::migrations::init_schema;

    /// Three accounts and `n` two-line entries between the first two.
    fn books(n: usize) -> (EventStore, Vec<String>, Vec<(String, String)>) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let mut ids = Vec::new();
        for (t, num, name) in [
            (AccountType::Liability, "5003", "USBank"),
            (AccountType::Liability, "6001", "Amazon"),
            (AccountType::Expense, "9000", "Uncategorized"),
        ] {
            let ev = AccountCommands::new(&mut store, "user".into())
                .create_account(CreateAccountCommand {
                    account_type: t,
                    account_number: num.into(),
                    name: name.into(),
                    parent_id: None,
                    currency: Some("USD".into()),
                    description: None,
                })
                .unwrap();
            let id = store
                .connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [num],
                    |r| r.get::<_, String>(0),
                )
                .unwrap();
            let _ = ev;
            ids.push(id);
        }
        // entry: debit Uncategorized, credit USBank — a card purchase.
        let mut entries = Vec::new();
        for i in 0..n {
            let stored = EntryCommands::new(&mut store, "user".into())
                .post_entry(PostEntryCommand {
                    date: chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
                    memo: format!("Amazon {i}"),
                    lines: vec![
                        EntryLine::debit(&ids[2], 1000, "USD"),
                        EntryLine::credit(&ids[0], 1000, "USD"),
                    ],
                    reference: None,
                    source: None,
                })
                .unwrap();
            let entry_id = match &stored.event {
                Event::JournalEntryPosted { entry_id, .. } => entry_id.clone(),
                _ => unreachable!(),
            };
            let card_line: String = store
                .connection()
                .query_row(
                    "SELECT id FROM journal_lines WHERE entry_id = ?1 AND account_id = ?2",
                    [&entry_id, &ids[0]],
                    |r| r.get(0),
                )
                .unwrap();
            entries.push((entry_id, card_line));
        }
        (store, ids, entries)
    }

    fn account_of(store: &EventStore, line_id: &str) -> String {
        store
            .connection()
            .query_row(
                "SELECT account_id FROM journal_lines WHERE id = ?1",
                [line_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// The reported incident, in miniature: a bulk move takes the card's own leg
    /// off every selected row, and the revert puts all of them back.
    #[test]
    fn reverting_a_bulk_reassign_puts_every_line_back() {
        let (mut store, ids, entries) = books(5);
        let before = store.latest_id().unwrap().unwrap();
        for (entry_id, line_id) in &entries {
            EntryCommands::new(&mut store, "user".into())
                .reassign_line(ReassignLineCommand {
                    entry_id: entry_id.clone(),
                    line_id: line_id.clone(),
                    new_account_id: ids[1].clone(),
                })
                .unwrap();
        }
        for (_, line_id) in &entries {
            assert_eq!(account_of(&store, line_id), ids[1], "moved off the card");
        }

        let n = revert(&mut store, "user", before + 1, before + 5).unwrap();
        assert_eq!(n, 5, "one inverse per event");
        for (_, line_id) in &entries {
            assert_eq!(account_of(&store, line_id), ids[0], "back on the card");
        }

        // Appended, not deleted: the mistake and its correction both stand.
        let total: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE event_type = 'journal_line_reassigned'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 10, "5 wrong + 5 undoing them");
    }

    /// All-or-nothing. Undoing the rest of a batch while leaving one row where
    /// somebody deliberately put it produces a state that matches no point in
    /// the ledger's history, so it is refused and says which row.
    #[test]
    fn a_line_moved_again_since_blocks_the_whole_revert() {
        let (mut store, ids, entries) = books(3);
        let before = store.latest_id().unwrap().unwrap();
        for (entry_id, line_id) in &entries {
            EntryCommands::new(&mut store, "user".into())
                .reassign_line(ReassignLineCommand {
                    entry_id: entry_id.clone(),
                    line_id: line_id.clone(),
                    new_account_id: ids[1].clone(),
                })
                .unwrap();
        }
        // Someone files one of them somewhere else on purpose.
        EntryCommands::new(&mut store, "user".into())
            .reassign_line(ReassignLineCommand {
                entry_id: entries[1].0.clone(),
                line_id: entries[1].1.clone(),
                new_account_id: ids[2].clone(),
            })
            .unwrap();

        let err = revert(&mut store, "user", before + 1, before + 3)
            .expect_err("the batch has been overtaken");
        let msg = err.to_string();
        assert!(msg.contains("1 of the 3"), "{msg}");
        assert!(msg.contains("9000 Uncategorized"), "names where it is now: {msg}");

        // And nothing was written — the other two are still where the bad batch
        // put them, rather than half-reverted.
        assert_eq!(account_of(&store, &entries[0].1), ids[1]);
        assert_eq!(account_of(&store, &entries[2].1), ids[1]);
    }

    /// A revert is ordinary history, so it can be reverted in turn. Without
    /// this, "undo" would be a one-way door with no way back from a mistaken
    /// undo.
    #[test]
    fn a_revert_can_itself_be_reverted() {
        let (mut store, ids, entries) = books(2);
        let before = store.latest_id().unwrap().unwrap();
        for (entry_id, line_id) in &entries {
            EntryCommands::new(&mut store, "user".into())
                .reassign_line(ReassignLineCommand {
                    entry_id: entry_id.clone(),
                    line_id: line_id.clone(),
                    new_account_id: ids[1].clone(),
                })
                .unwrap();
        }
        revert(&mut store, "user", before + 1, before + 2).unwrap();
        assert_eq!(account_of(&store, &entries[0].1), ids[0]);

        revert(&mut store, "user", before + 3, before + 4).unwrap();
        assert_eq!(account_of(&store, &entries[0].1), ids[1], "the undo undone");
    }

    /// Posting is undone by voiding, which is the only inverse a posted entry
    /// has — the entry stays in the log and stops counting.
    #[test]
    fn reverting_an_import_voids_what_it_posted() {
        let (mut store, _ids, entries) = books(3);
        let hi = store.latest_id().unwrap().unwrap();
        let n = revert(&mut store, "user", hi - 2, hi).unwrap();
        assert_eq!(n, 3);
        for (entry_id, _) in &entries {
            let voided: i64 = store
                .connection()
                .query_row(
                    "SELECT is_void FROM journal_entries WHERE id = ?1",
                    [entry_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(voided, 1, "{entry_id} should be voided");
        }
    }

    /// The summary is the thing somebody scans to find the action they regret,
    /// so it has to read like a sentence. "185 entrys posted" is the kind of
    /// wrongness that makes a screen look untrustworthy.
    #[test]
    fn the_summary_counts_things_in_english() {
        let (mut store, ids, entries) = books(1);
        let one = store.latest_id().unwrap().unwrap();
        let found = batches(store.connection(), 40).unwrap();
        let posted = found
            .iter()
            .find(|b| b.event_type == "journal_entry_posted")
            .unwrap();
        assert_eq!(posted.summary, "1 entry posted");
        assert_eq!(one, posted.hi);

        // An event type with no inverse carries itself rather than a guessed noun.
        let created = found
            .iter()
            .find(|b| b.event_type == "account_created")
            .unwrap();
        assert_eq!(created.summary, "3 \u{d7} account created");
        assert!(!created.revertible);

        EntryCommands::new(&mut store, "user".into())
            .reassign_line(ReassignLineCommand {
                entry_id: entries[0].0.clone(),
                line_id: entries[0].1.clone(),
                new_account_id: ids[1].clone(),
            })
            .unwrap();
        let found = batches(store.connection(), 40).unwrap();
        assert_eq!(found[0].summary, "1 line: 5003 USBank \u{2192} 6001 Amazon");
    }

    /// Guessing an inverse is worse than declining to have one: a revert that
    /// silently did less than it claimed would be trusted.
    #[test]
    fn an_event_type_with_no_inverse_says_so() {
        let (store, _, _) = books(0);
        // Event 1 is company_created.
        let err = plan_revert(store.connection(), 1, 1).expect_err("no inverse");
        assert!(err.to_string().contains("cannot be reverted"), "{err}");
    }

    /// A batch is one keystroke, not one sitting. The gap is what tells them
    /// apart, and getting it wrong in either direction is bad: too small and a
    /// bulk operation fragments into rows that must each be undone; too large
    /// and reverting one action silently undoes the three before it.
    #[test]
    fn a_bulk_run_is_one_batch_and_separate_actions_are_not() {
        let (mut store, ids, entries) = books(4);
        for (entry_id, line_id) in &entries {
            EntryCommands::new(&mut store, "user".into())
                .reassign_line(ReassignLineCommand {
                    entry_id: entry_id.clone(),
                    line_id: line_id.clone(),
                    new_account_id: ids[1].clone(),
                })
                .unwrap();
        }
        // The four reassignments went in back to back; push the last one far
        // enough away that it reads as a separate action.
        let last = store.latest_id().unwrap().unwrap();
        store
            .connection()
            .execute(
                "UPDATE events SET timestamp = ?1 WHERE id = ?2",
                rusqlite::params![
                    (Utc::now() + Duration::seconds(30)).to_rfc3339(),
                    last
                ],
            )
            .unwrap();

        let found = batches(store.connection(), 10).unwrap();
        let reassigns: Vec<_> = found
            .iter()
            .filter(|b| b.event_type == "journal_line_reassigned")
            .collect();
        assert_eq!(reassigns.len(), 2, "the stray one is its own batch");
        assert_eq!(reassigns[0].count, 1, "newest first");
        assert_eq!(reassigns[1].count, 3);

        // And the summary names the pair, which is how somebody recognises the
        // action they regret.
        assert!(
            reassigns[1].summary.contains("5003 USBank")
                && reassigns[1].summary.contains("6001 Amazon"),
            "{}",
            reassigns[1].summary
        );
        assert!(reassigns[1].revertible);
    }
}
