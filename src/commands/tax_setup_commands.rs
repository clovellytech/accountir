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

/// Point an account at a Form 1065 line, from a tax year onward.
///
/// The key is validated in [`crate::events::validation`] rather than here, so
/// the same check guards a command from this machine and a command that arrived
/// over the sync transport.
///
/// # Why the year is not optional
///
/// It used to be absent, and an account was on one line for all time — so
/// remapping in 2026 silently changed the 2023 return too, one that had already
/// been filed on the old assignment. Making the caller name the year means the
/// question "which years does this change?" is answered where the change is
/// made rather than discovered afterwards.
///
/// # What it refuses
///
/// A retirement value-change account, for any line but
/// [`crate::tax::lines::OFF_RETURN`]. Growth inside a sheltered account is not
/// income to anybody (INVESTMENTS-SPEC.md §8), so putting it on a line is tax paid
/// on exempt money, on a signed return. Refused here so the person who tried gets
/// told why, at the moment they tried — `load_effective_mapping` is the fence that
/// actually holds, and it holds silently, which is the wrong way to learn this.
pub fn set_account_line(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
    line_key: &str,
    effective_from: i32,
) -> Result<StoredEvent, TaxSetupError> {
    if line_key != crate::tax::lines::OFF_RETURN
        && crate::commands::retirement_commands::is_value_change_account(
            store.connection(),
            account_id,
        )
    {
        return Err(TaxSetupError::InvalidData(format!(
            "account {account_id} is where a sheltered account's value change is recorded, and \
             nothing in a sheltered account is taxable — it cannot report on {line_key}. Growth \
             in a 401(k) or an IRA is not income to anybody, and a return that reported it would \
             pay tax on money the statute exempts."
        )));
    }
    append(
        store,
        user_id,
        Event::TaxLineMappingSet {
            account_id: account_id.to_string(),
            line_key: line_key.to_string(),
            effective_from,
        },
    )
}

/// Say how much of an account's balance the law lets you deduct.
///
/// 100 clears the limit rather than storing it: "all of it" is the absence of a
/// rule, and a row saying so is a row somebody has to maintain.
pub fn set_deduction_limit(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
    deductible_pct: u8,
    effective_from: i32,
) -> Result<StoredEvent, TaxSetupError> {
    if deductible_pct >= 100 {
        return clear_deduction_limit(store, user_id, account_id, effective_from);
    }
    append(
        store,
        user_id,
        Event::TaxDeductionLimitSet {
            account_id: account_id.to_string(),
            deductible_pct,
            effective_from,
        },
    )
}

/// Put an account back to fully deductible from a tax year onward.
///
/// Removes that year's row only. An earlier year keeps whatever it had, because
/// a return filed on it is not something a later decision may edit.
pub fn clear_deduction_limit(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
    effective_from: i32,
) -> Result<StoredEvent, TaxSetupError> {
    append(
        store,
        user_id,
        Event::TaxDeductionLimitCleared {
            account_id: account_id.to_string(),
            effective_from,
        },
    )
}

/// Print a parent account's children as one row on the statements attached to
/// the return, or go back to a row each, from a tax year onward.
///
/// A yes-or-no for the year rather than a set and a clear, so "not from 2025"
/// is recorded as such and a statement attached to an earlier return is left as
/// it was filed.
pub fn set_statement_grouping(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
    grouped: bool,
    effective_from: i32,
) -> Result<StoredEvent, TaxSetupError> {
    append(
        store,
        user_id,
        Event::TaxStatementGroupingSet {
            account_id: account_id.to_string(),
            grouped,
            effective_from,
        },
    )
}

/// Mark an account as holding Illinois income or replacement tax from a tax year
/// on — or stop — so IL-1065 line 16 adds back what the federal return deducted
/// from it and the accounts beneath it.
///
/// A yes-or-no for the year, like [`set_statement_grouping`], so a return filed
/// for an earlier year is left as it was.
pub fn set_illinois_tax_addback(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
    added_back: bool,
    effective_from: i32,
) -> Result<StoredEvent, TaxSetupError> {
    append(
        store,
        user_id,
        Event::IllinoisTaxAddbackSet {
            account_id: account_id.to_string(),
            added_back,
            effective_from,
        },
    )
}

/// Remove an account's own line assignment from a tax year onward.
///
/// That year's row only. The account then falls back to the most recent earlier
/// year's assignment, and to its parent's if there is none — which is what makes
/// a new tax year start with the chart already mapped instead of blank.
pub fn clear_account_line(
    store: &mut EventStore,
    user_id: &str,
    account_id: &str,
    effective_from: i32,
) -> Result<StoredEvent, TaxSetupError> {
    append(
        store,
        user_id,
        Event::TaxLineMappingCleared {
            account_id: account_id.to_string(),
            effective_from,
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
        // Either catalogue: a book adopting pre-log setup may be filing
        // either return, and dropping the other form's mappings here would
        // silently discard the setup this migration exists to rescue.
        if crate::tax::any_line_def(line_key).is_none() {
            continue;
        }
        // Adopted setup predates dated assignments, so it applies to every year
        // until something later supersedes it — exactly what it did before.
        set_account_line(
            store,
            user_id,
            account_id,
            line_key,
            crate::events::types::ANY_YEAR,
        )?;
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

    /// The year these tests assign for. Any year works: what they check is the
    /// command and the projection, not the resolution across years.
    const YEAR: i32 = 2025;
    use crate::store::migrations::SchemaStore;

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().expect("in-memory store");
        SchemaStore::init_schema(&mut s).unwrap();
        s
    }

    /// Grouping applies from its year forward, and stopping it later leaves the
    /// earlier years grouped.
    #[test]
    fn statement_grouping_is_dated_and_can_be_stopped_later() {
        let mut s = store();
        set_statement_grouping(&mut s, "u1", "6000", true, 2023).unwrap();
        set_statement_grouping(&mut s, "u1", "6000", false, 2025).unwrap();

        let on = |y| crate::tax::lines::load_statement_groups(s.connection(), y).contains("6000");
        assert!(!on(2022), "before it was set");
        assert!(on(2023));
        assert!(on(2024), "a year with no row of its own inherits 2023's");
        assert!(
            !on(2025),
            "the no from 2025 is a row, not a fall back to 2023"
        );
    }

    #[test]
    fn an_illinois_tax_addback_is_dated_and_can_be_stopped_later() {
        let mut s = store();
        set_illinois_tax_addback(&mut s, "u1", "6000", true, 2023).unwrap();
        set_illinois_tax_addback(&mut s, "u1", "6000", false, 2025).unwrap();

        let on = |y| crate::tax::lines::load_illinois_tax_addbacks(s.connection(), y).contains("6000");
        assert!(!on(2022), "before it was set");
        assert!(on(2023));
        assert!(on(2024), "a year with no row of its own inherits 2023's");
        assert!(
            !on(2025),
            "the no from 2025 is a row, not a fall back to 2023"
        );
    }

    #[test]
    fn a_mapping_round_trips_through_the_log() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21", YEAR).unwrap();
        let m = crate::tax::lines::load_mapping(s.connection(), YEAR);
        assert_eq!(m.get("6100").map(String::as_str), Some("l21"));

        clear_account_line(&mut s, "u1", "6100", YEAR).unwrap();
        assert!(crate::tax::lines::load_mapping(s.connection(), YEAR).is_empty());
    }

    /// The point of the whole change: a second machine replaying the log has to
    /// arrive at the same setup.
    #[test]
    fn replaying_the_log_reproduces_the_setup() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21", YEAR).unwrap();
        set_account_line(&mut s, "u1", "1000", "sl1", YEAR).unwrap();
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

        let m = crate::tax::lines::load_mapping(replayed.connection(), YEAR);
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
        set_account_line(&mut s, "u1", "6100", "l21", YEAR).unwrap();
        clear_account_line(&mut s, "u1", "6100", YEAR).unwrap();

        let events = s.get_all().unwrap();
        crate::store::projections::Projector::new(s.connection())
            .rebuild(&events)
            .unwrap();
        assert!(crate::tax::lines::load_mapping(s.connection(), YEAR).is_empty());
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
        assert!(set_account_line(&mut s, "u1", "6100", "not-a-line", YEAR).is_err());
        assert!(crate::tax::lines::load_mapping(s.connection(), YEAR).is_empty());
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
            crate::tax::lines::load_mapping(s.connection(), YEAR)
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

    /// A remap does not reach backwards into a year already filed.
    ///
    /// The whole point. An account mapped to Other Deductions and used to file
    /// 2023 stays there when 2026 moves it to Repairs, because a return that has
    /// been signed is not something a later decision may edit.
    #[test]
    fn remapping_an_account_leaves_the_years_before_it_alone() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21", 2023).unwrap();
        set_account_line(&mut s, "u1", "6100", "l11", 2026).unwrap();

        let line = |year| {
            crate::tax::lines::load_mapping(s.connection(), year)
                .get("6100")
                .cloned()
        };
        assert_eq!(
            line(2023).as_deref(),
            Some("l21"),
            "the year it was filed on"
        );
        assert_eq!(
            line(2024).as_deref(),
            Some("l21"),
            "and every year until the change"
        );
        assert_eq!(line(2025).as_deref(), Some("l21"));
        assert_eq!(
            line(2026).as_deref(),
            Some("l11"),
            "the change applies forward"
        );
        assert_eq!(line(2027).as_deref(), Some("l11"), "and keeps applying");
    }

    /// A year with no assignment of its own inherits the most recent earlier one.
    ///
    /// Without this a new tax year would begin with an unmapped chart and every
    /// account would have to be assigned again, which is the cost that made
    /// dating them look not worth it.
    #[test]
    fn a_new_year_inherits_rather_than_starting_blank() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21", 2023).unwrap();
        for year in [2024, 2025, 2026, 2099] {
            assert_eq!(
                crate::tax::lines::load_mapping(s.connection(), year)
                    .get("6100")
                    .map(String::as_str),
                Some("l21"),
                "{year} did not inherit"
            );
        }
        // And a year before the first assignment has none, rather than borrowing
        // one from the future.
        assert!(crate::tax::lines::load_mapping(s.connection(), 2022).is_empty());
    }

    /// An assignment made before assignments were dated applies to every year.
    ///
    /// Books that predate this behave exactly as they did — which is what makes
    /// the migration a no-op until somebody dates something.
    #[test]
    fn an_undated_assignment_still_applies_to_every_year() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21", crate::events::types::ANY_YEAR).unwrap();
        for year in [2019, 2023, 2025, 2099] {
            assert_eq!(
                crate::tax::lines::load_mapping(s.connection(), year)
                    .get("6100")
                    .map(String::as_str),
                Some("l21"),
                "{year}"
            );
        }
        // And a dated assignment supersedes it from its own year on.
        set_account_line(&mut s, "u1", "6100", "l11", 2025).unwrap();
        assert_eq!(
            crate::tax::lines::load_mapping(s.connection(), 2024)
                .get("6100")
                .map(String::as_str),
            Some("l21")
        );
        assert_eq!(
            crate::tax::lines::load_mapping(s.connection(), 2025)
                .get("6100")
                .map(String::as_str),
            Some("l11")
        );
    }

    /// Clearing a year removes that year's assignment and no other.
    #[test]
    fn clearing_a_year_falls_back_rather_than_wiping_the_history() {
        let mut s = store();
        set_account_line(&mut s, "u1", "6100", "l21", 2023).unwrap();
        set_account_line(&mut s, "u1", "6100", "l11", 2026).unwrap();
        clear_account_line(&mut s, "u1", "6100", 2026).unwrap();

        let line = |year| {
            crate::tax::lines::load_mapping(s.connection(), year)
                .get("6100")
                .cloned()
        };
        assert_eq!(
            line(2026).as_deref(),
            Some("l21"),
            "back to the earlier assignment"
        );
        assert_eq!(
            line(2023).as_deref(),
            Some("l21"),
            "which was never disturbed"
        );
    }

    /// Deduction limits are dated the same way, and for the same reason.
    #[test]
    fn a_deduction_limit_applies_from_its_year_forward() {
        let mut s = store();
        set_deduction_limit(&mut s, "u1", "3055", 50, 2023).unwrap();
        set_deduction_limit(&mut s, "u1", "3055", 0, 2026).unwrap();

        fn pct(s: &EventStore, year: i32) -> Option<u8> {
            crate::tax::lines::load_deduction_limits(s.connection(), year)
                .get("3055")
                .copied()
        }
        assert_eq!(pct(&s, 2023), Some(50));
        assert_eq!(pct(&s, 2025), Some(50));
        assert_eq!(pct(&s, 2026), Some(0), "fully disallowed from 2026");

        // Clearing 2026 puts it back to the 50% that 2023 set, not to no rule.
        clear_deduction_limit(&mut s, "u1", "3055", 2026).unwrap();
        assert_eq!(pct(&s, 2026), Some(50));
    }
}
