//! Changing the Form 1065 setup: which account reports where, and what Schedule
//! B says.
//!
//! # Why these are commands and not writes
//!
//! Until migration 027 both were plain local tables written by direct SQL. That
//! meant opening the same books on a second machine showed none of it — the work
//! existed on exactly one laptop, silently. These are facts about the
//! partnership, in the same sense `business_profile` and `partners` are, so they
//! travel the same way: an event each, projected into the same tables, replicated
//! like everything else.
//!
//! A partner's TIN deliberately does *not* travel this way. The distinction is
//! secrecy, not preparation: a TIN is a secret and belongs on one machine, while
//! "account 6100 reports on line 21" is something every member preparing this
//! return needs to agree about.
//!
//! # Adoption
//!
//! Databases that predate migration 027 hold rows nothing in the log accounts
//! for. [`adopt_pending`] turns them into events, once, on the next writable
//! open. See the migration for why it is staged rather than done in place.

use crate::events::types::{Event, StoredEvent};
use crate::store::event_store::EventStore;

pub use crate::commands::partnership_commands::PartnershipError as TaxSetupError;

/// Point an account at a Form 1065 line.
///
/// The key is validated in [`crate::events::validation`] rather than here, so
/// the same check guards a command from this machine and a command that arrived
/// over the sync transport.
pub fn set_account_line(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
    line_key: &str,
) -> Result<StoredEvent, TaxSetupError> {
    append(
        store,
        user_id,
        Event::TaxLineMappingSet {
            account_id: account_id.to_string(),
            line_key: line_key.to_string(),
        },
    )
}

/// Take an account off the return.
pub fn clear_account_line(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
) -> Result<StoredEvent, TaxSetupError> {
    append(
        store,
        user_id,
        Event::TaxLineMappingCleared {
            account_id: account_id.to_string(),
        },
    )
}

/// Answer one Schedule B question for one tax year.
///
/// An empty value clears the answer, because "unanswered" is a real state on
/// this form and distinct from "No" — the caller says which by what they pass,
/// and the two produce different events.
pub fn set_schedule_b_answer(
    store: &mut EventStore,
    user_id: &str,
    tax_year: i32,
    answer_key: &str,
    value: &str,
) -> Result<StoredEvent, TaxSetupError> {
    let value = value.trim();
    let event = if value.is_empty() {
        Event::ScheduleBAnswerCleared {
            tax_year,
            answer_key: answer_key.to_string(),
        }
    } else {
        Event::ScheduleBAnswerSet {
            tax_year,
            answer_key: answer_key.to_string(),
            value: value.to_string(),
        }
    };
    append(store, user_id, event)
}

/// Copy every answer from one year to another, skipping any the target year
/// already has.
///
/// One event per answer copied, rather than one "copied 2024 to 2025" event:
/// each answer is independently editable afterwards, and a single event would
/// make "what does 2025 say about question 7" a question you answer by
/// replaying a bulk operation and then every edit since.
///
/// Returns how many were copied.
pub fn copy_schedule_b_year(
    store: &mut EventStore,
    user_id: &str,
    from: i32,
    to: i32,
) -> Result<usize, TaxSetupError> {
    let source = crate::tax::schedule_b::load(store.connection(), from);
    let target = crate::tax::schedule_b::load(store.connection(), to);

    let mut copied = 0;
    for (key, value) in source.answers() {
        if target.get(key).is_some() {
            continue;
        }
        set_schedule_b_answer(store, user_id, to, key, value)?;
        copied += 1;
    }
    Ok(copied)
}

/// What [`adopt_pending`] did, for the caller to report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Adopted {
    pub mappings: usize,
    pub answers: usize,
}

impl Adopted {
    pub fn is_empty(self) -> bool {
        self.mappings == 0 && self.answers == 0
    }
}

/// How many rows are still waiting to be adopted.
///
/// A replica cannot append locally — the instance owns the writes — so
/// [`adopt_pending`] cannot run there. The desktop asks this instead and offers
/// to publish them over the sync transport, which is the only route a replica
/// has. Zero means there is nothing outstanding.
pub fn pending_adoption(conn: &rusqlite::Connection) -> Adopted {
    let count = |sql: &str| -> usize {
        conn.query_row(sql, [], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
            .max(0) as usize
    };
    Adopted {
        mappings: count("SELECT COUNT(*) FROM tax_line_mappings_pending_adoption"),
        answers: count("SELECT COUNT(*) FROM schedule_b_answers_pending_adoption"),
    }
}

/// One staged mapping: account id, line key.
pub type StagedMapping = (String, String);
/// One staged answer: tax year, question key, value.
pub type StagedAnswer = (i32, String, String);

/// The staged rows themselves, for a replica to submit one at a time.
pub fn staged_rows(conn: &rusqlite::Connection) -> (Vec<StagedMapping>, Vec<StagedAnswer>) {
    let mappings = conn
        .prepare("SELECT account_id, line_key FROM tax_line_mappings_pending_adoption")
        .and_then(|mut st| {
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default();
    let answers = conn
        .prepare("SELECT tax_year, answer_key, value FROM schedule_b_answers_pending_adoption")
        .and_then(|mut st| {
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map(|rows| rows.flatten().collect::<Vec<_>>())
        })
        .unwrap_or_default();
    (mappings, answers)
}

/// Forget the staged rows once a replica has published them over sync.
///
/// Separate from [`adopt_pending`] because on a replica the events are appended
/// by the *instance*, not here — this side only has to stop offering to publish
/// them again.
pub fn clear_staged(conn: &rusqlite::Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "DELETE FROM tax_line_mappings_pending_adoption;
         DELETE FROM schedule_b_answers_pending_adoption;",
    )
}

/// Turn rows that predate migration 027 into events, once.
///
/// Runs on open, before anything reads the tables, so there is no window in
/// which the setup looks lost. Rows whose line key or answer key the catalogue
/// no longer recognises are dropped rather than adopted: validation would refuse
/// the event anyway, and a staged row nobody can turn into an event would be
/// retried on every open forever.
///
/// Idempotent by construction — the staging tables are emptied as part of the
/// same append, so a second call finds nothing.
pub fn adopt_pending(store: &mut EventStore, user_id: &str) -> Result<Adopted, TaxSetupError> {
    let db = |e: rusqlite::Error| TaxSetupError::StoreError(e.to_string());

    let mappings: Vec<(String, String)> = {
        let mut stmt = store
            .connection()
            .prepare("SELECT account_id, line_key FROM tax_line_mappings_pending_adoption")
            .map_err(db)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db)?;
        rows.flatten().collect()
    };
    let answers: Vec<(i32, String, String)> = {
        let mut stmt = store
            .connection()
            .prepare("SELECT tax_year, answer_key, value FROM schedule_b_answers_pending_adoption")
            .map_err(db)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(db)?;
        rows.flatten().collect()
    };

    if mappings.is_empty() && answers.is_empty() {
        return Ok(Adopted::default());
    }

    let mut out = Adopted::default();
    for (account_id, line_key) in &mappings {
        if crate::tax::lines::line_def(line_key).is_none() {
            continue;
        }
        set_account_line(store, user_id, account_id, line_key)?;
        out.mappings += 1;
    }
    for (tax_year, answer_key, value) in &answers {
        if !crate::tax::schedule_b::known_key(answer_key) {
            continue;
        }
        set_schedule_b_answer(store, user_id, *tax_year, answer_key, value)?;
        out.answers += 1;
    }

    // Cleared only once every event is on disk. A crash midway leaves the
    // staging rows in place and re-adopts on the next open — which produces a
    // duplicate event for the ones that made it, and duplicates are harmless
    // here: both project to the same row. Losing a row is not harmless, so the
    // asymmetry runs this way deliberately.
    store
        .connection()
        .execute_batch(
            "DELETE FROM tax_line_mappings_pending_adoption;
             DELETE FROM schedule_b_answers_pending_adoption;",
        )
        .map_err(db)?;

    Ok(out)
}

fn append(
    store: &mut EventStore,
    user_id: &str,
    event: Event,
) -> Result<StoredEvent, TaxSetupError> {
    crate::commands::partnership_commands::append_event_locally(store, user_id, event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations::SchemaStore;

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().expect("in-memory store");
        SchemaStore::init_schema(&mut s).unwrap();
        s
    }

    #[test]
    fn a_mapping_round_trips_through_the_log() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21").unwrap();
        let m = crate::tax::lines::load_mapping(s.connection());
        assert_eq!(m.get("6100").map(String::as_str), Some("l21"));

        clear_account_line(&mut s, "u1", "6100").unwrap();
        assert!(crate::tax::lines::load_mapping(s.connection()).is_empty());
    }

    /// The point of the whole change: a second machine replaying the log has to
    /// arrive at the same setup.
    #[test]
    fn replaying_the_log_reproduces_the_setup() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21").unwrap();
        set_account_line(&mut s, "u1", "1000", "sl1").unwrap();
        set_schedule_b_answer(&mut s, "u1", 2025, "b5", "no").unwrap();

        // A second machine receives the events and appends them, which is what
        // the sync transport does. Projecting into a store whose `events` table
        // is empty would violate `updated_at_event`'s foreign key — and rightly
        // so: a projection row pointing at an event the store does not have is
        // exactly the inconsistency that key exists to prevent.
        let events = s.get_all().unwrap();
        let mut replayed = store();
        for e in &events {
            replayed
                .append(crate::events::types::EventEnvelope::new(
                    e.event.clone(),
                    "u1".to_string(),
                ))
                .unwrap();
        }
        // Appending stores; projecting is the separate step that builds the
        // tables the return is read from.
        let stored = replayed.get_all().unwrap();
        crate::store::projections::Projector::new(replayed.connection())
            .rebuild(&stored)
            .unwrap();

        let m = crate::tax::lines::load_mapping(replayed.connection());
        assert_eq!(m.get("6100").map(String::as_str), Some("l21"));
        assert_eq!(m.get("1000").map(String::as_str), Some("sl1"));
        assert_eq!(
            crate::tax::schedule_b::load(replayed.connection(), 2025).get("b5"),
            Some("no")
        );
    }

    /// A rebuild truncates these tables now, so a clear has to survive replay —
    /// otherwise an account taken off the return quietly comes back.
    #[test]
    fn a_cleared_mapping_stays_cleared_through_a_rebuild() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21").unwrap();
        clear_account_line(&mut s, "u1", "6100").unwrap();

        let events = s.get_all().unwrap();
        crate::store::projections::Projector::new(s.connection())
            .rebuild(&events)
            .unwrap();
        assert!(crate::tax::lines::load_mapping(s.connection()).is_empty());
    }

    /// Unanswered and No are different states, and a clear must replay as the
    /// first rather than the second.
    #[test]
    fn clearing_an_answer_is_its_own_event_and_survives_replay() {
        let mut s = store();
        set_schedule_b_answer(&mut s, "u1", 2025, "b5", "no").unwrap();
        set_schedule_b_answer(&mut s, "u1", 2025, "b5", "").unwrap();

        let events = s.get_all().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e.event, Event::ScheduleBAnswerCleared { .. })),
            "clearing must be its own event"
        );

        crate::store::projections::Projector::new(s.connection())
            .rebuild(&events)
            .unwrap();
        assert_eq!(
            crate::tax::schedule_b::load(s.connection(), 2025).get("b5"),
            None
        );
    }

    #[test]
    fn a_line_key_the_catalogue_does_not_have_is_refused() {
        let mut s = store();
        assert!(set_account_line(&mut s, "u1", "6100", "not-a-line").is_err());
        assert!(crate::tax::lines::load_mapping(s.connection()).is_empty());
    }

    #[test]
    fn a_schedule_b_key_the_catalogue_does_not_have_is_refused() {
        let mut s = store();
        assert!(set_schedule_b_answer(&mut s, "u1", 2025, "b99", "yes").is_err());
    }

    #[test]
    fn copying_a_year_emits_one_event_per_answer_and_skips_what_is_answered() {
        let mut s = store();
        set_schedule_b_answer(&mut s, "u1", 2024, "b5", "no").unwrap();
        set_schedule_b_answer(&mut s, "u1", 2024, "b6", "no").unwrap();
        set_schedule_b_answer(&mut s, "u1", 2025, "b5", "yes").unwrap();

        let copied = copy_schedule_b_year(&mut s, "u1", 2024, 2025).unwrap();
        assert_eq!(copied, 1);

        let y = crate::tax::schedule_b::load(s.connection(), 2025);
        assert_eq!(y.get("b5"), Some("yes"), "the answer already given wins");
        assert_eq!(y.get("b6"), Some("no"));
    }

    /// The migration path: rows that predate the log become events, once, and
    /// then survive a rebuild.
    #[test]
    fn pending_rows_are_adopted_into_the_log_and_survive_a_rebuild() {
        let mut s = store();
        s.connection()
            .execute_batch(
                "INSERT INTO tax_line_mappings (account_id, line_key) VALUES ('6100','l21');
                 INSERT INTO tax_line_mappings_pending_adoption (account_id, line_key)
                     VALUES ('6100','l21');
                 INSERT INTO schedule_b_answers (tax_year, answer_key, value)
                     VALUES (2025,'b5','no');
                 INSERT INTO schedule_b_answers_pending_adoption (tax_year, answer_key, value)
                     VALUES (2025,'b5','no');",
            )
            .unwrap();

        let adopted = adopt_pending(&mut s, "u1").unwrap();
        assert_eq!(adopted.mappings, 1);
        assert_eq!(adopted.answers, 1);

        // A rebuild would have destroyed these before adoption; now it does not.
        let events = s.get_all().unwrap();
        crate::store::projections::Projector::new(s.connection())
            .rebuild(&events)
            .unwrap();
        assert_eq!(
            crate::tax::lines::load_mapping(s.connection())
                .get("6100")
                .map(String::as_str),
            Some("l21")
        );
        assert_eq!(
            crate::tax::schedule_b::load(s.connection(), 2025).get("b5"),
            Some("no")
        );
    }

    #[test]
    fn adoption_runs_once_and_is_a_no_op_thereafter() {
        let mut s = store();
        s.connection()
            .execute("INSERT INTO tax_line_mappings_pending_adoption (account_id, line_key) VALUES ('6100','l21')", [])
            .unwrap();
        assert_eq!(adopt_pending(&mut s, "u1").unwrap().mappings, 1);
        assert!(adopt_pending(&mut s, "u1").unwrap().is_empty());
    }

    /// A staged row the catalogue no longer recognises would be refused by
    /// validation forever. Dropped instead, so adoption always completes.
    #[test]
    fn a_staged_row_with_an_unknown_key_is_dropped_rather_than_retried() {
        let mut s = store();
        s.connection()
            .execute_batch(
                "INSERT INTO tax_line_mappings_pending_adoption (account_id, line_key)
                     VALUES ('6100','a-line-that-was-removed');
                 INSERT INTO schedule_b_answers_pending_adoption (tax_year, answer_key, value)
                     VALUES (2025,'b99','yes');",
            )
            .unwrap();

        let adopted = adopt_pending(&mut s, "u1").unwrap();
        assert!(adopted.is_empty());
        // And the staging tables are empty, so it does not retry on every open.
        assert!(adopt_pending(&mut s, "u1").unwrap().is_empty());
        let n: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM tax_line_mappings_pending_adoption",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
