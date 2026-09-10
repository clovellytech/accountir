//! Opening a fiscal year, and reading back whether one is closed.
//!
//! Closing a year is not here — it is in [`crate::commands::closing_commands`],
//! because a year is never closed on its own. It is closed *by* the entry that
//! sweeps its revenue and expense into equity, and the two have to land
//! together; splitting them across two modules would invite closing a year with
//! nothing to show for it.
//!
//! What is here is the other half: `FiscalYearOpened`, which creates the
//! `fiscal_years` row that the closed-year fence in
//! [`crate::commands::entry_commands`] and everything in `closing_commands`
//! read. Nothing else in the app emits it.
//!
//! ## The fence only knows about years someone opened
//!
//! `fiscal_years` starts empty and stays empty until something opens a year. The
//! fence reads `.optional()?.unwrap_or(false)`, so a date in no row at all is
//! unfenced — which is the behaviour you want for a ledger that has never closed
//! anything, and is why `close_books` opens the year itself rather than making
//! the user do it first.

use crate::domain::fiscal_year::FiscalYear;
use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FiscalYearCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Fiscal year {year} does not exist")]
    YearNotFound { year: i32 },
    #[error("Fiscal year {year} is already open")]
    YearAlreadyOpen { year: i32 },
}

/// Command to open a fiscal year, creating its `fiscal_years` row.
#[derive(Debug, Clone)]
pub struct OpenFiscalYearCommand {
    pub year: i32,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
}

/// Read a `fiscal_years` row into the domain type, or `None` if the year was
/// never opened.
///
/// Takes a bare `&Connection` so it serves both the read path and the write
/// path — `closing_commands` calls it with the transaction's own handle, so its
/// checks see the write-locked state rather than a snapshot taken before the
/// lock.
pub fn load_year(conn: &Connection, year: i32) -> Result<Option<FiscalYear>, EventStoreError> {
    let row: Option<(String, String, bool, Option<String>)> = conn
        .query_row(
            "SELECT start_date, end_date, is_closed = 1, retained_earnings_entry_id
             FROM fiscal_years WHERE year = ?1",
            [year],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    let (start_s, end_s, is_closed, entry_id) = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    // Dates are stored as ISO-8601 `YYYY-MM-DD` (chrono's `NaiveDate` Display).
    let parse = |s: &str| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|e| EventStoreError::Projection(format!("bad fiscal_years date {s:?}: {e}")))
    };

    Ok(Some(FiscalYear {
        year,
        start_date: parse(&start_s)?,
        end_date: parse(&end_s)?,
        is_closed,
        retained_earnings_entry_id: entry_id,
    }))
}

/// The fiscal year boundaries this company uses for `year`, from
/// `company.fiscal_year_start_month`.
///
/// Falls back to the calendar year: the column defaults to 1, nothing in the app
/// currently sets it, and a company row may not exist at all in a ledger that
/// has only ever been written to by the importer.
pub fn boundaries_for(conn: &Connection, year: i32) -> FiscalYear {
    let start_month: u32 = conn
        .query_row(
            "SELECT fiscal_year_start_month FROM company LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
        .filter(|m| (1..=12).contains(m))
        .unwrap_or(1);
    FiscalYear::for_year(year, start_month)
}

/// Fiscal year command handler.
pub struct FiscalYearCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> FiscalYearCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Open a fiscal year.
    ///
    /// Rejects if the year already exists, checked inside the append transaction
    /// against the write-locked projection so a concurrent writer cannot
    /// invalidate the decision between the read and the append. Retries on a
    /// head move.
    pub fn open_fiscal_year(
        &mut self,
        cmd: OpenFiscalYearCommand,
    ) -> Result<StoredEvent, FiscalYearCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    let exists: bool = tx
                        .query_row(
                            "SELECT 1 FROM fiscal_years WHERE year = ?1",
                            [cmd.year],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if exists {
                        return Ok(Verdict::Reject(FiscalYearCommandError::YearAlreadyOpen {
                            year: cmd.year,
                        }));
                    }
                    let event = Event::FiscalYearOpened {
                        year: cmd.year,
                        start_date: cmd.start_date,
                        end_date: cmd.end_date,
                    };
                    Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
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

    /// Open `year` if it is not open already, using this company's fiscal
    /// boundaries. Idempotent — an already-open year is success, not an error.
    ///
    /// This is what `close_books` calls: no ledger in existence has ever had a
    /// `fiscal_years` row, so "the year has not been opened" is the normal case
    /// rather than the edge one, and making the user open a year by hand before
    /// they may close it would be ceremony with nothing behind it.
    pub fn ensure_year_open(&mut self, year: i32) -> Result<(), FiscalYearCommandError> {
        let fy = boundaries_for(self.store.connection(), year);
        match self.open_fiscal_year(OpenFiscalYearCommand {
            year,
            start_date: fy.start_date,
            end_date: fy.end_date,
        }) {
            Ok(_) | Err(FiscalYearCommandError::YearAlreadyOpen { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{
        EntryCommandError, EntryCommands, EntryLine, PostEntryCommand,
    };
    use crate::domain::AccountType;
    use crate::events::types::JournalEntrySource;
    use crate::store::migrations::init_schema;

    fn setup() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn open_2024(store: &mut EventStore) {
        FiscalYearCommands::new(store, "user".to_string())
            .ensure_year_open(2024)
            .unwrap();
    }

    fn create_accounts(store: &mut EventStore) -> (String, String) {
        let mut commands = AccountCommands::new(store, "user".to_string());
        for (ty, number, name) in [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Expense, "5000", "Supplies"),
        ] {
            commands
                .create_account(CreateAccountCommand {
                    account_type: ty,
                    account_number: number.to_string(),
                    name: name.to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
        }
        let id = |n: &str| -> String {
            store
                .connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [n],
                    |r| r.get(0),
                )
                .unwrap()
        };
        (id("1000"), id("5000"))
    }

    #[test]
    fn opening_a_year_creates_one_row_and_no_periods_table_is_consulted() {
        let mut store = setup();
        open_2024(&mut store);

        let (start, end, closed): (String, String, i64) = store
            .connection()
            .query_row(
                "SELECT start_date, end_date, is_closed FROM fiscal_years WHERE year = 2024",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(start, "2024-01-01");
        assert_eq!(end, "2024-12-31");
        assert_eq!(closed, 0);
    }

    #[test]
    fn opening_the_same_year_twice_is_rejected_but_ensure_is_idempotent() {
        let mut store = setup();
        open_2024(&mut store);

        let err = FiscalYearCommands::new(&mut store, "user".to_string())
            .open_fiscal_year(OpenFiscalYearCommand {
                year: 2024,
                start_date: day(2024, 1, 1),
                end_date: day(2024, 12, 31),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalYearCommandError::YearAlreadyOpen { year: 2024 }
        ));

        // ensure_year_open swallows exactly that rejection.
        FiscalYearCommands::new(&mut store, "user".to_string())
            .ensure_year_open(2024)
            .unwrap();
    }

    #[test]
    fn an_open_year_does_not_fence_anything() {
        let mut store = setup();
        open_2024(&mut store);
        let (cash, expense) = create_accounts(&mut store);

        EntryCommands::new(&mut store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: day(2024, 6, 15),
                memo: "In an open year".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 10000, "USD"),
                    EntryLine::credit(&cash, 10000, "USD"),
                ],
                reference: None,
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();
    }

    /// The fence reads `fiscal_years`, so a closed year refuses entries dated
    /// anywhere inside it — including the last day, which is where a closing
    /// entry lands. (`closing_commands` posts that entry in the same append that
    /// closes the year, which is the only reason it can exist at all.)
    #[test]
    fn a_closed_year_fences_every_date_it_contains() {
        let mut store = setup();
        open_2024(&mut store);
        let (cash, expense) = create_accounts(&mut store);

        store
            .connection()
            .execute("UPDATE fiscal_years SET is_closed = 1 WHERE year = 2024", [])
            .unwrap();

        for date in [day(2024, 1, 1), day(2024, 6, 15), day(2024, 12, 31)] {
            let err = EntryCommands::new(&mut store, "user".to_string())
                .post_entry(PostEntryCommand {
                    date,
                    memo: "In a closed year".to_string(),
                    lines: vec![
                        EntryLine::debit(&expense, 10000, "USD"),
                        EntryLine::credit(&cash, 10000, "USD"),
                    ],
                    reference: None,
                    source: Some(JournalEntrySource::Manual),
                })
                .unwrap_err();
            assert!(
                matches!(err, EntryCommandError::YearClosed(d) if d == date),
                "expected the year fence at {date}, got {err:?}"
            );
        }

        // A date outside the closed year is untouched.
        EntryCommands::new(&mut store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: day(2025, 1, 2),
                memo: "The next year".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 10000, "USD"),
                    EntryLine::credit(&cash, 10000, "USD"),
                ],
                reference: None,
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();
    }

    /// A ledger that has never opened a year fences nothing — the table is
    /// empty, and an empty table must not mean "everything is closed".
    #[test]
    fn a_ledger_with_no_fiscal_years_fences_nothing() {
        let mut store = setup();
        let (cash, expense) = create_accounts(&mut store);

        EntryCommands::new(&mut store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: day(1999, 3, 4),
                memo: "No fiscal years exist".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 10000, "USD"),
                    EntryLine::credit(&cash, 10000, "USD"),
                ],
                reference: None,
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();
    }

    #[test]
    fn boundaries_follow_the_companys_fiscal_year_start_month() {
        let store = setup();
        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES ('c', 'c', 'Co', 'USD', 7)",
                [],
            )
            .unwrap();

        let fy = boundaries_for(store.connection(), 2023);
        assert_eq!(fy.start_date, day(2023, 7, 1));
        assert_eq!(fy.end_date, day(2024, 6, 30));
    }

    #[test]
    fn boundaries_fall_back_to_the_calendar_year_with_no_company_row() {
        let store = setup();
        let fy = boundaries_for(store.connection(), 2023);
        assert_eq!(fy.start_date, day(2023, 1, 1));
        assert_eq!(fy.end_date, day(2023, 12, 31));
    }

    #[test]
    fn load_year_reads_back_what_was_opened() {
        let mut store = setup();
        open_2024(&mut store);

        let fy = load_year(store.connection(), 2024).unwrap().unwrap();
        assert_eq!(fy.year, 2024);
        assert_eq!(fy.start_date, day(2024, 1, 1));
        assert!(!fy.is_closed);
        assert_eq!(fy.retained_earnings_entry_id, None);

        assert!(load_year(store.connection(), 2099).unwrap().is_none());
    }
}
