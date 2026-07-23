//! Commands that close and reopen fiscal periods, and close a fiscal year.
//!
//! The [`Event`] log already carries `PeriodClosed`, `PeriodReopened` and
//! `YearEndClosed`, the domain rules live in [`crate::domain::fiscal_period`],
//! and every journal-entry handler enforces a *closed-period fence*
//! (`check_entry_invariants_in_txn` rejects a `JournalEntryPosted` whose date
//! falls in a `status='closed'` row of `fiscal_periods`). But nothing emitted
//! those events, so the fence could never actually be raised. This module is the
//! missing command layer.
//!
//! Each handler follows the [`EventStore::append_checked`] head-CAS retry
//! pattern used across the command layer (see `reconciliation_commands.rs`): the
//! state-dependent invariant (period not already closed / currently closed / all
//! periods in the year closed) is re-checked **inside** the append transaction,
//! against the write-locked projection, so a concurrent writer cannot invalidate
//! the decision between the read and the append. The invariant logic itself is
//! delegated to the domain types [`FiscalPeriod`] / [`FiscalPeriodError`].
//!
//! ## Period identity
//! A fiscal period is identified by `(year, period)` — the primary key of the
//! `fiscal_periods` projection table and the fields carried by the events.
//!
//! ## Where `fiscal_periods` rows come from
//! Closing/reopening a period operates on a **pre-existing** row; it never
//! creates one. Rows are seeded by the `FiscalYearOpened` projection (12 monthly
//! periods, all `open`). Because no command emitted `FiscalYearOpened` either,
//! [`FiscalPeriodCommands::open_fiscal_year`] is provided here so the feature is
//! usable (and testable) end to end. See the decision notes in the task summary.

use crate::domain::fiscal_period::{FiscalPeriod, FiscalPeriodError, PeriodStatus};
use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use chrono::NaiveDate;
use rusqlite::OptionalExtension;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FiscalPeriodCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Projection error: {0}")]
    ProjectionError(#[from] crate::store::projections::ProjectionError),
    #[error("Fiscal period {year}-{period} does not exist")]
    PeriodNotFound { year: i32, period: u8 },
    #[error("Fiscal period {year}-{period} is already closed")]
    AlreadyClosed { year: i32, period: u8 },
    #[error("Fiscal period {year}-{period} is not closed")]
    NotClosed { year: i32, period: u8 },
    #[error("Fiscal year {year} does not exist")]
    YearNotFound { year: i32 },
    #[error("Fiscal year {year} is already open")]
    YearAlreadyOpen { year: i32 },
    #[error("Fiscal year {year} is already closed")]
    YearAlreadyClosed { year: i32 },
    #[error("Cannot close fiscal year {year}: not all periods are closed")]
    PeriodsNotClosed { year: i32 },
}

/// Command to open a fiscal year, seeding its (monthly) periods as `open`.
///
/// Emits `FiscalYearOpened`; its existing projection inserts the `fiscal_years`
/// row and the monthly `fiscal_periods` rows the close/reopen commands operate
/// on. Provided so the period-close feature is usable end to end (no other
/// command emits this event).
#[derive(Debug, Clone)]
pub struct OpenFiscalYearCommand {
    pub year: i32,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
}

/// Command to close a fiscal period. Establishes the closed-period fence for
/// dates in `(year, period)`.
#[derive(Debug, Clone)]
pub struct ClosePeriodCommand {
    pub year: i32,
    pub period: u8,
}

/// Command to reopen a previously closed fiscal period.
#[derive(Debug, Clone)]
pub struct ReopenPeriodCommand {
    pub year: i32,
    pub period: u8,
    pub reason: String,
}

/// Command to close a fiscal year. Requires every period in the year closed.
#[derive(Debug, Clone)]
pub struct CloseYearCommand {
    pub year: i32,
    /// The journal entry that rolls net income into retained earnings. Treated
    /// as an opaque identifier here (the domain does not require it to reference
    /// an existing entry).
    pub retained_earnings_entry_id: String,
}

/// Read a `(year, period)` row from the `fiscal_periods` projection under the
/// write lock, reconstructing the domain [`FiscalPeriod`] so the invariant
/// checks can be delegated to [`FiscalPeriod::close`] / [`FiscalPeriod::reopen`].
/// Returns `None` if no such row exists.
fn load_period_in_txn(
    tx: &rusqlite::Transaction<'_>,
    year: i32,
    period: u8,
) -> Result<Option<FiscalPeriod>, EventStoreError> {
    let row: Option<(String, String, String)> = tx
        .query_row(
            "SELECT start_date, end_date, status FROM fiscal_periods
             WHERE year = ?1 AND period = ?2",
            rusqlite::params![year, period],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let (start_s, end_s, status_s) = match row {
        Some(r) => r,
        None => return Ok(None),
    };

    // Dates are stored as ISO-8601 `YYYY-MM-DD` (chrono's `NaiveDate` Display).
    let parse = |s: &str| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|e| EventStoreError::Projection(format!("bad fiscal_periods date {s:?}: {e}")))
    };
    let mut fp = FiscalPeriod::new(year, period, parse(&start_s)?, parse(&end_s)?);
    if status_s == "closed" {
        fp.status = PeriodStatus::Closed;
    }
    Ok(Some(fp))
}

/// Fiscal period command handler.
pub struct FiscalPeriodCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> FiscalPeriodCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Open a fiscal year and seed its monthly periods (all `open`).
    ///
    /// Rejects if the year is already open (its `fiscal_years` row exists),
    /// checked inside the append transaction. Retries on a head move.
    pub fn open_fiscal_year(
        &mut self,
        cmd: OpenFiscalYearCommand,
    ) -> Result<StoredEvent, FiscalPeriodCommandError> {
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
                        return Ok(Verdict::Reject(FiscalPeriodCommandError::YearAlreadyOpen {
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

    /// Close a fiscal period.
    ///
    /// Re-checks, inside the append transaction, that the period exists and is
    /// not already closed (delegated to [`FiscalPeriod::close`]). This is what
    /// raises the closed-period fence read by `check_entry_invariants_in_txn`.
    /// Retries on a head move.
    pub fn close_period(
        &mut self,
        cmd: ClosePeriodCommand,
    ) -> Result<StoredEvent, FiscalPeriodCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    let mut period = match load_period_in_txn(tx, cmd.year, cmd.period)? {
                        Some(p) => p,
                        None => {
                            return Ok(Verdict::Reject(FiscalPeriodCommandError::PeriodNotFound {
                                year: cmd.year,
                                period: cmd.period,
                            }))
                        }
                    };
                    // Domain rule: a period cannot be closed twice.
                    if let Err(FiscalPeriodError::PeriodClosed) = period.close(user_id.clone()) {
                        return Ok(Verdict::Reject(FiscalPeriodCommandError::AlreadyClosed {
                            year: cmd.year,
                            period: cmd.period,
                        }));
                    }
                    let event = Event::PeriodClosed {
                        year: cmd.year,
                        period: cmd.period,
                        closed_by_user_id: user_id.clone(),
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

    /// Reopen a fiscal period.
    ///
    /// Re-checks, inside the append transaction, that the period exists and is
    /// currently closed (delegated to [`FiscalPeriod::reopen`]). Retries on a
    /// head move.
    pub fn reopen_period(
        &mut self,
        cmd: ReopenPeriodCommand,
    ) -> Result<StoredEvent, FiscalPeriodCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    let mut period = match load_period_in_txn(tx, cmd.year, cmd.period)? {
                        Some(p) => p,
                        None => {
                            return Ok(Verdict::Reject(FiscalPeriodCommandError::PeriodNotFound {
                                year: cmd.year,
                                period: cmd.period,
                            }))
                        }
                    };
                    // Domain rule: only a closed period can be reopened.
                    if let Err(FiscalPeriodError::AlreadyOpen) = period.reopen() {
                        return Ok(Verdict::Reject(FiscalPeriodCommandError::NotClosed {
                            year: cmd.year,
                            period: cmd.period,
                        }));
                    }
                    let event = Event::PeriodReopened {
                        year: cmd.year,
                        period: cmd.period,
                        reason: cmd.reason.clone(),
                        reopened_by_user_id: user_id.clone(),
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

    /// Close a fiscal year.
    ///
    /// Re-checks, inside the append transaction, that the year exists, is not
    /// already closed, and that **every** period in the year is closed (the
    /// domain rule [`crate::domain::fiscal_period::FiscalYear::all_periods_closed`],
    /// mirrored here as a SQL count so we do not have to rehydrate the whole
    /// year). Does **not** flip period statuses — they are already closed by
    /// precondition. Retries on a head move.
    pub fn close_year(
        &mut self,
        cmd: CloseYearCommand,
    ) -> Result<StoredEvent, FiscalPeriodCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    // The fiscal year must exist and not already be closed.
                    let is_closed: Option<bool> = tx
                        .query_row(
                            "SELECT is_closed = 1 FROM fiscal_years WHERE year = ?1",
                            [cmd.year],
                            |r| r.get(0),
                        )
                        .optional()?;
                    match is_closed {
                        None => {
                            return Ok(Verdict::Reject(
                                FiscalPeriodCommandError::YearNotFound { year: cmd.year },
                            ))
                        }
                        Some(true) => {
                            return Ok(Verdict::Reject(
                                FiscalPeriodCommandError::YearAlreadyClosed { year: cmd.year },
                            ))
                        }
                        Some(false) => {}
                    }

                    // Domain rule (FiscalYear::all_periods_closed): every period
                    // in the year must be closed. There must also be at least one
                    // period, else the year was never seeded with periods.
                    let total: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM fiscal_periods WHERE year = ?1",
                        [cmd.year],
                        |r| r.get(0),
                    )?;
                    let open: i64 = tx.query_row(
                        "SELECT COUNT(*) FROM fiscal_periods WHERE year = ?1 AND status != 'closed'",
                        [cmd.year],
                        |r| r.get(0),
                    )?;
                    if total == 0 || open > 0 {
                        return Ok(Verdict::Reject(
                            FiscalPeriodCommandError::PeriodsNotClosed { year: cmd.year },
                        ));
                    }

                    let event = Event::YearEndClosed {
                        year: cmd.year,
                        retained_earnings_entry_id: cmd.retained_earnings_entry_id.clone(),
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

    /// Seed calendar fiscal year 2024 with its 12 monthly `open` periods.
    fn open_2024(store: &mut EventStore) {
        FiscalPeriodCommands::new(store, "user".to_string())
            .open_fiscal_year(OpenFiscalYearCommand {
                year: 2024,
                start_date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                end_date: NaiveDate::from_ymd_opt(2024, 12, 31).unwrap(),
            })
            .unwrap();
    }

    fn period_status(store: &EventStore, year: i32, period: u8) -> String {
        store
            .connection()
            .query_row(
                "SELECT status FROM fiscal_periods WHERE year = ?1 AND period = ?2",
                rusqlite::params![year, period],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn create_accounts(store: &mut EventStore) -> (String, String) {
        let mut commands = AccountCommands::new(store, "user".to_string());
        commands
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
        commands
            .create_account(CreateAccountCommand {
                account_type: AccountType::Expense,
                account_number: "5000".to_string(),
                name: "Supplies".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
        let cash: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '1000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let expense: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '5000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (cash, expense)
    }

    #[test]
    fn open_fiscal_year_seeds_twelve_open_periods() {
        let mut store = setup();
        open_2024(&mut store);

        let count: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM fiscal_periods WHERE year = 2024 AND status = 'open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 12);

        // Reopening the same year is rejected.
        let err = FiscalPeriodCommands::new(&mut store, "user".to_string())
            .open_fiscal_year(OpenFiscalYearCommand {
                year: 2024,
                start_date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                end_date: NaiveDate::from_ymd_opt(2024, 12, 31).unwrap(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalPeriodCommandError::YearAlreadyOpen { year: 2024 }
        ));
    }

    #[test]
    fn closing_a_period_sets_status_and_raises_the_fence() {
        let mut store = setup();
        open_2024(&mut store);
        let (cash, expense) = create_accounts(&mut store);

        FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_period(ClosePeriodCommand {
                year: 2024,
                period: 1,
            })
            .unwrap();

        assert_eq!(period_status(&store, 2024, 1), "closed");

        // A journal entry dated inside the now-closed January period is rejected
        // by the existing closed-period fence in post_entry.
        let err = EntryCommands::new(&mut store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
                memo: "In closed period".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 10000, "USD"),
                    EntryLine::credit(&cash, 10000, "USD"),
                ],
                reference: None,
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap_err();
        assert!(
            matches!(err, EntryCommandError::PeriodClosed(d) if d == NaiveDate::from_ymd_opt(2024, 1, 15).unwrap()),
            "expected PeriodClosed fence, got {err:?}"
        );

        // An entry in a still-open period (February) posts fine.
        EntryCommands::new(&mut store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2024, 2, 15).unwrap(),
                memo: "In open period".to_string(),
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
    fn double_close_is_rejected() {
        let mut store = setup();
        open_2024(&mut store);

        FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_period(ClosePeriodCommand {
                year: 2024,
                period: 3,
            })
            .unwrap();
        let err = FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_period(ClosePeriodCommand {
                year: 2024,
                period: 3,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalPeriodCommandError::AlreadyClosed {
                year: 2024,
                period: 3
            }
        ));
    }

    #[test]
    fn close_period_rejects_unknown_period() {
        let mut store = setup();
        open_2024(&mut store);
        // Year 2099 has no fiscal_periods rows at all.
        let err = FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_period(ClosePeriodCommand {
                year: 2099,
                period: 1,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalPeriodCommandError::PeriodNotFound {
                year: 2099,
                period: 1
            }
        ));
    }

    #[test]
    fn reopen_requires_closed_period() {
        let mut store = setup();
        open_2024(&mut store);

        // Reopening an open period is rejected.
        let err = FiscalPeriodCommands::new(&mut store, "user".to_string())
            .reopen_period(ReopenPeriodCommand {
                year: 2024,
                period: 5,
                reason: "oops".to_string(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalPeriodCommandError::NotClosed {
                year: 2024,
                period: 5
            }
        ));

        // Close then reopen restores 'open' status.
        FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_period(ClosePeriodCommand {
                year: 2024,
                period: 5,
            })
            .unwrap();
        assert_eq!(period_status(&store, 2024, 5), "closed");

        FiscalPeriodCommands::new(&mut store, "user".to_string())
            .reopen_period(ReopenPeriodCommand {
                year: 2024,
                period: 5,
                reason: "correcting entry".to_string(),
            })
            .unwrap();
        assert_eq!(period_status(&store, 2024, 5), "open");

        // closed_by_user_id / closed_at cleared on reopen.
        let (closed_by, closed_at): (Option<String>, Option<String>) = store
            .connection()
            .query_row(
                "SELECT closed_by_user_id, closed_at FROM fiscal_periods WHERE year = 2024 AND period = 5",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(closed_by, None);
        assert_eq!(closed_at, None);
    }

    #[test]
    fn close_year_requires_all_periods_closed() {
        let mut store = setup();
        open_2024(&mut store);

        // With periods still open, closing the year is rejected.
        let err = FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_year(CloseYearCommand {
                year: 2024,
                retained_earnings_entry_id: "re-entry-1".to_string(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalPeriodCommandError::PeriodsNotClosed { year: 2024 }
        ));

        // Close all 12 periods.
        for period in 1..=12u8 {
            FiscalPeriodCommands::new(&mut store, "user".to_string())
                .close_period(ClosePeriodCommand { year: 2024, period })
                .unwrap();
        }

        // Now the year can be closed.
        FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_year(CloseYearCommand {
                year: 2024,
                retained_earnings_entry_id: "re-entry-1".to_string(),
            })
            .unwrap();

        let (is_closed, re): (i64, String) = store
            .connection()
            .query_row(
                "SELECT is_closed, retained_earnings_entry_id FROM fiscal_years WHERE year = 2024",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(is_closed, 1);
        assert_eq!(re, "re-entry-1");

        // Double year-close is rejected.
        let err = FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_year(CloseYearCommand {
                year: 2024,
                retained_earnings_entry_id: "re-entry-2".to_string(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalPeriodCommandError::YearAlreadyClosed { year: 2024 }
        ));
    }

    #[test]
    fn close_year_rejects_unknown_year() {
        let mut store = setup();
        open_2024(&mut store);
        let err = FiscalPeriodCommands::new(&mut store, "user".to_string())
            .close_year(CloseYearCommand {
                year: 2099,
                retained_earnings_entry_id: "re".to_string(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            FiscalPeriodCommandError::YearNotFound { year: 2099 }
        ));
    }
}
