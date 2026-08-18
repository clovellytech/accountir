//! How often a service's sales become journal entries.
//!
//! # Why this is a ledger fact and not a preference
//!
//! It looks like a display setting and it is not. A rollup's idempotency key
//! carries its period — `bugbear:rollup:daily:2026-08-17` against
//! `bugbear:rollup:monthly:2026-08` — so two members of a group syncing the same
//! service at different frequencies produce keys that do not collide. Nothing
//! catches the overlap, and August's sales post twice.
//!
//! So the frequency lives in the log, every member reads the same value, and
//! changing it is an event with a date from which it applies.
//!
//! # Why changing it is dated
//!
//! Switching from daily to monthly mid-month would otherwise re-aggregate days
//! that are already posted under a key that does not match them. A change takes
//! effect from a boundary, and everything before it keeps the shape it was
//! posted with.

use chrono::{Datelike, Duration, NaiveDate};
use serde::{Deserialize, Serialize};

/// How a service's sales reach the books.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReportingFrequency {
    /// One journal entry per sale — what every service did before rollups
    /// existed, and still the default so that registering a service changes
    /// nothing about how it behaves.
    #[default]
    PerEvent,
    Daily,
    Weekly,
    Monthly,
}

impl ReportingFrequency {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportingFrequency::PerEvent => "per_event",
            ReportingFrequency::Daily => "daily",
            ReportingFrequency::Weekly => "weekly",
            ReportingFrequency::Monthly => "monthly",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "per_event" => Some(ReportingFrequency::PerEvent),
            "daily" => Some(ReportingFrequency::Daily),
            "weekly" => Some(ReportingFrequency::Weekly),
            "monthly" => Some(ReportingFrequency::Monthly),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ReportingFrequency::PerEvent => "Every sale",
            ReportingFrequency::Daily => "Daily totals",
            ReportingFrequency::Weekly => "Weekly totals",
            ReportingFrequency::Monthly => "Monthly totals",
        }
    }

    pub const ALL: &'static [ReportingFrequency] = &[
        ReportingFrequency::PerEvent,
        ReportingFrequency::Daily,
        ReportingFrequency::Weekly,
        ReportingFrequency::Monthly,
    ];

    /// The period a date falls in, or `None` when every event stands alone.
    pub fn period_of(self, date: NaiveDate) -> Option<Period> {
        let (start, end) = match self {
            ReportingFrequency::PerEvent => return None,
            ReportingFrequency::Daily => (date, date),
            // ISO weeks, Monday to Sunday. Chosen over "seven days from whenever
            // the service was connected" because a week has to mean the same
            // thing to two members who connected on different days, and because
            // it is the week every other report in the world uses.
            ReportingFrequency::Weekly => {
                let back = date.weekday().num_days_from_monday() as i64;
                let start = date - Duration::days(back);
                (start, start + Duration::days(6))
            }
            ReportingFrequency::Monthly => {
                let start = date.with_day(1).expect("day 1 exists in every month");
                (start, last_day_of_month(date))
            }
        };
        Some(Period {
            start,
            end,
            frequency: self,
        })
    }
}

fn last_day_of_month(date: NaiveDate) -> NaiveDate {
    let (y, m) = (date.year(), date.month());
    let first_of_next = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1)
    }
    .expect("a first-of-month always exists");
    first_of_next - Duration::days(1)
}

/// One reporting window.
///
/// Ordered by start date so that grouping events into periods yields them
/// chronologically — a rollup run posts oldest first, which is the order somebody
/// reading the register expects and the order that makes a partial run resumable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Period {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub frequency: ReportingFrequency,
}

/// How long after a period ends before it is safe to total up.
///
/// A service can publish an event after the day it happened — a till reconciled
/// the next morning, a queued webhook, a clock an hour out. Totalling the moment
/// midnight passes would post a figure that is then wrong, and the correction
/// costs a second entry that somebody has to understand.
///
/// One day is not a guess at any particular service's behaviour; it is the
/// smallest delay that makes "the period is over" mean it for anyone in a
/// neighbouring timezone.
pub const SETTLING_DAYS: i64 = 1;

impl Period {
    /// The key this period's entry is stamped with.
    ///
    /// It carries the frequency as well as the dates, because `2026-08-17` as a
    /// daily key and as one day of a monthly key are different postings of the
    /// same money and must not be mistaken for each other.
    pub fn key(&self) -> String {
        match self.frequency {
            ReportingFrequency::PerEvent => String::new(),
            ReportingFrequency::Daily => format!("daily:{}", self.start),
            ReportingFrequency::Weekly => format!("weekly:{}", self.start),
            ReportingFrequency::Monthly => {
                format!("monthly:{}-{:02}", self.start.year(), self.start.month())
            }
        }
    }

    pub fn label(&self) -> String {
        match self.frequency {
            ReportingFrequency::PerEvent => String::new(),
            ReportingFrequency::Daily => self.start.to_string(),
            ReportingFrequency::Weekly => format!("week of {}", self.start),
            ReportingFrequency::Monthly => {
                format!("{}-{:02}", self.start.year(), self.start.month())
            }
        }
    }

    /// Whether this period is over and settled enough to total.
    ///
    /// The alternative — totalling a period still in progress — posts a figure
    /// that changes, and a ledger entry that changes is one that has to be
    /// corrected by another entry every time a sale lands.
    pub fn is_closed_on(&self, today: NaiveDate) -> bool {
        self.end + Duration::days(SETTLING_DAYS) <= today
    }

    pub fn contains(&self, date: NaiveDate) -> bool {
        date >= self.start && date <= self.end
    }
}

/// The full idempotency key for a period's entry.
///
/// Scoped by service for the same reason a single event's key is: two services
/// both reporting daily would otherwise share `daily:2026-08-17` and the second
/// would be swallowed as a duplicate of the first.
pub fn rollup_reference(service_name: &str, period: &Period) -> String {
    format!("{}:rollup:{}", service_name, period.key())
}

/// The key for a top-up entry covering events that arrived after a period was
/// already posted.
///
/// `revision` counts from 2 — the original posting is revision 1 and carries the
/// bare key. A top-up is a separate entry on purpose: the ledger records what was
/// known when, and rewriting the first entry would erase the fact that the books
/// were correct on the evidence available.
pub fn rollup_revision_reference(service_name: &str, period: &Period, revision: u32) -> String {
    format!("{}:r{revision}", rollup_reference(service_name, period))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn a_week_runs_monday_to_sunday_whoever_asks() {
        // Every day of one ISO week must land on the same period, or two members
        // connecting on different days would total different weeks.
        let expected = Period {
            frequency: ReportingFrequency::Weekly,
            start: day(2026, 8, 17),
            end: day(2026, 8, 23),
        };
        for d in 17..=23 {
            assert_eq!(
                ReportingFrequency::Weekly.period_of(day(2026, 8, d)),
                Some(expected),
                "2026-08-{d} landed in a different week"
            );
        }
        assert_ne!(
            ReportingFrequency::Weekly.period_of(day(2026, 8, 24)),
            Some(expected)
        );
    }

    #[test]
    fn a_month_ends_on_its_last_day_including_february() {
        let feb = ReportingFrequency::Monthly
            .period_of(day(2026, 2, 14))
            .unwrap();
        assert_eq!(feb.start, day(2026, 2, 1));
        assert_eq!(feb.end, day(2026, 2, 28));

        let leap = ReportingFrequency::Monthly
            .period_of(day(2024, 2, 14))
            .unwrap();
        assert_eq!(leap.end, day(2024, 2, 29), "2024 is a leap year");

        let dec = ReportingFrequency::Monthly
            .period_of(day(2026, 12, 3))
            .unwrap();
        assert_eq!(dec.end, day(2026, 12, 31), "December must not wrap wrong");
    }

    /// A period still running is not totalled.
    ///
    /// Posting a figure that then changes means a correcting entry for every sale
    /// that lands afterwards, which is how a ledger fills with noise.
    #[test]
    fn a_period_is_not_closed_until_it_has_settled() {
        let today = day(2026, 8, 18);
        let d17 = ReportingFrequency::Daily
            .period_of(day(2026, 8, 17))
            .unwrap();
        let d18 = ReportingFrequency::Daily.period_of(today).unwrap();

        assert!(d17.is_closed_on(today), "yesterday has settled");
        assert!(!d18.is_closed_on(today), "today is still happening");
        assert!(
            !d17.is_closed_on(day(2026, 8, 17)),
            "a day is not closed on the day itself, whatever the clock says"
        );
    }

    /// The same dates under two frequencies must never share a key.
    ///
    /// This is the property that stops one member's daily totals and another's
    /// monthly totals from both posting: they would be the same money, and only a
    /// distinct key makes the collision visible instead of silent.
    #[test]
    fn two_frequencies_over_the_same_days_produce_different_keys() {
        let d = ReportingFrequency::Daily
            .period_of(day(2026, 8, 1))
            .unwrap();
        let w = ReportingFrequency::Weekly
            .period_of(day(2026, 8, 1))
            .unwrap();
        let m = ReportingFrequency::Monthly
            .period_of(day(2026, 8, 1))
            .unwrap();

        let keys = [
            rollup_reference("bugbear", &d),
            rollup_reference("bugbear", &w),
            rollup_reference("bugbear", &m),
        ];
        let unique: std::collections::HashSet<&String> = keys.iter().collect();
        assert_eq!(unique.len(), 3, "keys collided: {keys:?}");
    }

    #[test]
    fn two_services_reporting_daily_do_not_share_a_key() {
        let p = ReportingFrequency::Daily
            .period_of(day(2026, 8, 17))
            .unwrap();
        assert_ne!(
            rollup_reference("bugbear", &p),
            rollup_reference("othershop", &p)
        );
    }

    /// A top-up is its own entry, not a rewrite of the first.
    #[test]
    fn a_revision_key_differs_from_the_original() {
        let p = ReportingFrequency::Daily
            .period_of(day(2026, 8, 17))
            .unwrap();
        assert_ne!(
            rollup_reference("bugbear", &p),
            rollup_revision_reference("bugbear", &p, 2)
        );
    }

    #[test]
    fn per_event_has_no_period_at_all() {
        assert_eq!(
            ReportingFrequency::PerEvent.period_of(day(2026, 8, 17)),
            None
        );
    }
}
