//! The fiscal year, and what it means for one to be closed.
//!
//! # Why there are no sub-year periods
//!
//! This module used to carry a `FiscalPeriod` as well — twelve monthly windows
//! per year, each independently closable, with the year closable only once all
//! twelve were. Nothing ever emitted them: no ledger in existence held a single
//! `fiscal_periods` row, so the posting fence they existed to raise had never
//! actually been raised.
//!
//! They were removed rather than wired up, because the year is the unit anyone
//! actually closes here and the period model made the year-end close strictly
//! worse. The closing entry is dated the last day of the year — inside the
//! twelfth period — so it could not be posted after that period was closed, and
//! the year could not be closed before it. The close had to thread the needle in
//! three separate appends with two half-closed states to recover from. Against
//! the year directly it is one atomic append of two events, and there is no
//! ordering to get wrong. See `commands/closing_commands.rs`.
//!
//! A monthly soft-close is a real thing to want, and nothing here forecloses
//! adding it back. It should be added when someone wants it, on top of a year
//! model that works, rather than kept as scaffolding nobody stands on.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FiscalYearError {
    #[error("Fiscal year is already closed")]
    AlreadyClosed,
    #[error("Fiscal year is already open")]
    AlreadyOpen,
    #[error("Cannot close a year whose trial balance does not balance")]
    UnbalancedTrialBalance,
    #[error("Date {0} is not within fiscal year {1}")]
    DateOutsideFiscalYear(NaiveDate, i32),
}

/// A fiscal year: its boundaries, and whether it has been closed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiscalYear {
    pub year: i32,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub is_closed: bool,
    /// The closing entry that swept revenue and expense into equity. `None`
    /// while the year is open.
    pub retained_earnings_entry_id: Option<String>,
}

impl FiscalYear {
    /// A fiscal year labelled `year` that begins in `start_month` and runs
    /// twelve months.
    ///
    /// A year starting in January is the calendar year; one starting in July
    /// 2023 ends 30 June 2024 and is still "fiscal 2023". The end is derived as
    /// the day before the same month a year on, which is the one formula that
    /// gets both cases — and February — right without a branch.
    pub fn for_year(year: i32, start_month: u32) -> Self {
        let start_date = NaiveDate::from_ymd_opt(year, start_month, 1)
            .unwrap_or_else(|| panic!("month {start_month} is not a month"));
        let end_date = NaiveDate::from_ymd_opt(year + 1, start_month, 1)
            .expect("the first of a month exists in every year")
            .pred_opt()
            .expect("no fiscal year begins at the dawn of the calendar");

        Self {
            year,
            start_date,
            end_date,
            is_closed: false,
            retained_earnings_entry_id: None,
        }
    }

    /// The ordinary case: 1 January to 31 December.
    pub fn calendar_year(year: i32) -> Self {
        Self::for_year(year, 1)
    }

    pub fn contains_date(&self, date: NaiveDate) -> bool {
        date >= self.start_date && date <= self.end_date
    }

    /// Close the year, recording the entry that carried its result to equity.
    pub fn close(&mut self, retained_earnings_entry_id: String) -> Result<(), FiscalYearError> {
        if self.is_closed {
            return Err(FiscalYearError::AlreadyClosed);
        }
        self.is_closed = true;
        self.retained_earnings_entry_id = Some(retained_earnings_entry_id);
        Ok(())
    }

    /// Reopen a closed year. The closing entry is voided by the caller — see
    /// `closing_commands::reopen_books`, which does both in one append so the
    /// books are never briefly locked with their closing entry missing.
    pub fn reopen(&mut self) -> Result<(), FiscalYearError> {
        if !self.is_closed {
            return Err(FiscalYearError::AlreadyOpen);
        }
        self.is_closed = false;
        self.retained_earnings_entry_id = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn calendar_year_runs_january_to_december() {
        let fy = FiscalYear::calendar_year(2024);
        assert_eq!(fy.year, 2024);
        assert_eq!(fy.start_date, day(2024, 1, 1));
        assert_eq!(fy.end_date, day(2024, 12, 31));
        assert!(!fy.is_closed);
    }

    /// A July start ends on 30 June of the *following* calendar year, and is
    /// still labelled with the year it began in.
    #[test]
    fn a_non_calendar_year_spans_two_calendar_years() {
        let fy = FiscalYear::for_year(2023, 7);
        assert_eq!(fy.start_date, day(2023, 7, 1));
        assert_eq!(fy.end_date, day(2024, 6, 30));
    }

    /// A March start ends in February — including a February that only has 29
    /// days in the year it lands in, which is why the end is derived rather
    /// than assembled from a day-of-month.
    #[test]
    fn a_year_ending_in_a_leap_february_gets_the_29th() {
        let fy = FiscalYear::for_year(2023, 3);
        assert_eq!(fy.end_date, day(2024, 2, 29));
        let fy = FiscalYear::for_year(2024, 3);
        assert_eq!(fy.end_date, day(2025, 2, 28));
    }

    #[test]
    fn contains_date_is_inclusive_of_both_ends() {
        let fy = FiscalYear::calendar_year(2024);
        assert!(fy.contains_date(day(2024, 1, 1)));
        assert!(fy.contains_date(day(2024, 6, 15)));
        assert!(fy.contains_date(day(2024, 12, 31)));
        assert!(!fy.contains_date(day(2023, 12, 31)));
        assert!(!fy.contains_date(day(2025, 1, 1)));
    }

    #[test]
    fn closing_records_the_entry_and_refuses_a_second_time() {
        let mut fy = FiscalYear::calendar_year(2024);
        fy.close("entry-1".to_string()).unwrap();
        assert!(fy.is_closed);
        assert_eq!(fy.retained_earnings_entry_id.as_deref(), Some("entry-1"));

        assert!(matches!(
            fy.close("entry-2".to_string()),
            Err(FiscalYearError::AlreadyClosed)
        ));
        assert_eq!(
            fy.retained_earnings_entry_id.as_deref(),
            Some("entry-1"),
            "a refused close must not overwrite the entry that did close it"
        );
    }

    #[test]
    fn reopening_clears_the_entry_and_refuses_on_an_open_year() {
        let mut fy = FiscalYear::calendar_year(2024);
        assert!(matches!(
            fy.reopen(),
            Err(FiscalYearError::AlreadyOpen)
        ));

        fy.close("entry-1".to_string()).unwrap();
        fy.reopen().unwrap();
        assert!(!fy.is_closed);
        assert_eq!(fy.retained_earnings_entry_id, None);
    }
}
