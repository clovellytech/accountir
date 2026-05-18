use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum FiscalPeriodError {
    #[error("Period is closed")]
    PeriodClosed,
    #[error("Period is already open")]
    AlreadyOpen,
    #[error("Cannot close period with unbalanced trial balance")]
    UnbalancedTrialBalance,
    #[error("Cannot close year before all periods are closed")]
    PeriodsNotClosed,
    #[error("Invalid period: {0}")]
    InvalidPeriod(String),
    #[error("Date {0} is not within fiscal year {1}")]
    DateOutsideFiscalYear(NaiveDate, i32),
}

/// Status of a fiscal period
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodStatus {
    Open,
    Closed,
}

/// A fiscal period (typically a month)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiscalPeriod {
    pub year: i32,
    pub period: u8, // 1-12 for monthly, 1-4 for quarterly
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub status: PeriodStatus,
    pub closed_by_user_id: Option<String>,
    pub closed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl FiscalPeriod {
    pub fn new(year: i32, period: u8, start_date: NaiveDate, end_date: NaiveDate) -> Self {
        Self {
            year,
            period,
            start_date,
            end_date,
            status: PeriodStatus::Open,
            closed_by_user_id: None,
            closed_at: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.status == PeriodStatus::Open
    }

    pub fn is_closed(&self) -> bool {
        self.status == PeriodStatus::Closed
    }

    pub fn contains_date(&self, date: NaiveDate) -> bool {
        date >= self.start_date && date <= self.end_date
    }

    pub fn close(&mut self, user_id: String) -> Result<(), FiscalPeriodError> {
        if self.is_closed() {
            return Err(FiscalPeriodError::PeriodClosed);
        }
        self.status = PeriodStatus::Closed;
        self.closed_by_user_id = Some(user_id);
        self.closed_at = Some(chrono::Utc::now());
        Ok(())
    }

    pub fn reopen(&mut self) -> Result<(), FiscalPeriodError> {
        if self.is_open() {
            return Err(FiscalPeriodError::AlreadyOpen);
        }
        self.status = PeriodStatus::Open;
        self.closed_by_user_id = None;
        self.closed_at = None;
        Ok(())
    }
}

/// A fiscal year containing multiple periods
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiscalYear {
    pub year: i32,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub periods: Vec<FiscalPeriod>,
    pub is_closed: bool,
    pub retained_earnings_entry_id: Option<String>,
}

impl FiscalYear {
    /// Create a new fiscal year with monthly periods
    pub fn new_monthly(year: i32, start_month: u32) -> Self {
        let start_date = NaiveDate::from_ymd_opt(year, start_month, 1).unwrap();
        let end_date = if start_month == 1 {
            NaiveDate::from_ymd_opt(year, 12, 31).unwrap()
        } else {
            // Fiscal year spans two calendar years
            let end_year = year + 1;
            let end_month = if start_month == 1 {
                12
            } else {
                start_month - 1
            };
            // Last day of the end month
            let next_month_start = if end_month == 12 {
                NaiveDate::from_ymd_opt(end_year + 1, 1, 1).unwrap()
            } else {
                NaiveDate::from_ymd_opt(end_year, end_month + 1, 1).unwrap()
            };
            next_month_start.pred_opt().unwrap()
        };

        let mut periods = Vec::new();
        let mut current_date = start_date;
        let mut period_num = 1u8;

        while current_date <= end_date && period_num <= 12 {
            let period_end = {
                let next_month = if current_date.month() == 12 {
                    NaiveDate::from_ymd_opt(current_date.year() + 1, 1, 1).unwrap()
                } else {
                    NaiveDate::from_ymd_opt(current_date.year(), current_date.month() + 1, 1)
                        .unwrap()
                };
                next_month.pred_opt().unwrap()
            };

            periods.push(FiscalPeriod::new(
                year,
                period_num,
                current_date,
                period_end,
            ));

            current_date = period_end.succ_opt().unwrap();
            period_num += 1;
        }

        Self {
            year,
            start_date,
            end_date,
            periods,
            is_closed: false,
            retained_earnings_entry_id: None,
        }
    }

    /// Create a standard calendar year (Jan 1 - Dec 31)
    pub fn calendar_year(year: i32) -> Self {
        Self::new_monthly(year, 1)
    }

    /// Check if all periods are closed
    pub fn all_periods_closed(&self) -> bool {
        self.periods.iter().all(|p| p.is_closed())
    }

    /// Get the period containing a date
    pub fn get_period_for_date(&self, date: NaiveDate) -> Option<&FiscalPeriod> {
        self.periods.iter().find(|p| p.contains_date(date))
    }

    /// Get mutable period containing a date
    pub fn get_period_for_date_mut(&mut self, date: NaiveDate) -> Option<&mut FiscalPeriod> {
        self.periods.iter_mut().find(|p| p.contains_date(date))
    }

    /// Check if a date is in an open period
    pub fn is_date_in_open_period(&self, date: NaiveDate) -> bool {
        self.get_period_for_date(date)
            .map(|p| p.is_open())
            .unwrap_or(false)
    }

    /// Close the fiscal year
    pub fn close(&mut self, retained_earnings_entry_id: String) -> Result<(), FiscalPeriodError> {
        if !self.all_periods_closed() {
            return Err(FiscalPeriodError::PeriodsNotClosed);
        }
        self.is_closed = true;
        self.retained_earnings_entry_id = Some(retained_earnings_entry_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fiscal_year_calendar() {
        let fy = FiscalYear::calendar_year(2024);
        assert_eq!(fy.year, 2024);
        assert_eq!(fy.start_date, NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
        assert_eq!(fy.end_date, NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
        assert_eq!(fy.periods.len(), 12);
    }

    #[test]
    fn test_fiscal_period_dates() {
        let fy = FiscalYear::calendar_year(2024);

        // January
        let jan = &fy.periods[0];
        assert_eq!(jan.period, 1);
        assert_eq!(jan.start_date, NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
        assert_eq!(jan.end_date, NaiveDate::from_ymd_opt(2024, 1, 31).unwrap());

        // February (leap year)
        let feb = &fy.periods[1];
        assert_eq!(feb.period, 2);
        assert_eq!(feb.end_date, NaiveDate::from_ymd_opt(2024, 2, 29).unwrap());

        // December
        let dec = &fy.periods[11];
        assert_eq!(dec.period, 12);
        assert_eq!(dec.end_date, NaiveDate::from_ymd_opt(2024, 12, 31).unwrap());
    }

    #[test]
    fn test_period_contains_date() {
        let fy = FiscalYear::calendar_year(2024);
        let jan = &fy.periods[0];

        assert!(jan.contains_date(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()));
        assert!(jan.contains_date(NaiveDate::from_ymd_opt(2024, 1, 15).unwrap()));
        assert!(jan.contains_date(NaiveDate::from_ymd_opt(2024, 1, 31).unwrap()));
        assert!(!jan.contains_date(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap()));
    }

    #[test]
    fn test_close_period() {
        let mut fy = FiscalYear::calendar_year(2024);

        // Close January
        fy.periods[0].close("user-001".to_string()).unwrap();
        assert!(fy.periods[0].is_closed());
        assert!(!fy.all_periods_closed());

        // Try to close again - should fail
        let result = fy.periods[0].close("user-001".to_string());
        assert!(matches!(result, Err(FiscalPeriodError::PeriodClosed)));
    }

    #[test]
    fn test_get_period_for_date() {
        let fy = FiscalYear::calendar_year(2024);

        let period = fy.get_period_for_date(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap());
        assert!(period.is_some());
        assert_eq!(period.unwrap().period, 6); // June
    }
}
