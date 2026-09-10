//! The dated history of who holds what percentage.
//!
//! # What this is for
//!
//! A partnership's split changes. Three partners at a third each; one leaves;
//! the two who stay go to 51/49. Held as one set of numbers per partner — which
//! is how [`partners`] holds it — the *current* split is the only one the books
//! can describe, so a Schedule K-1 for the three-partner year shows 51/49 in
//! item J. The return foots perfectly and describes a partnership that did not
//! exist in the year being filed.
//!
//! `partner_share_periods` records what the percentages were and from when.
//! [`attach_history`] hangs that series on the [`Partner`] records the tax code
//! already loads, and [`Partner::shares_on`] answers the question the K-1 asks:
//! not "what is the split" but "what was the split, on this day".
//!
//! # Why the current shares stay where they are
//!
//! `partners.profit_ppm` and its two neighbours remain the live snapshot, and
//! the projection keeps them equal to the newest period. Nine or so readers want
//! today's split and nothing else; making all of them walk a series to get it
//! would be a lot of code paying for a case they do not have. Books that have
//! never recorded a change carry no periods at all, and every one of those
//! readers — and every prior year of a partnership that never changed — keeps
//! giving the same answer it always did.
//!
//! [`partners`]: crate::commands::partnership_commands::list_partners

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};

use crate::commands::partnership_commands::{
    self as pc, append_event_locally, list_partners, PartnershipError,
};
use crate::domain::{Partner, SharePeriod, Shares, FULL_SHARE};
use crate::events::Event;
use crate::store::EventStore;
use crate::StoredEvent;

/// Read every partner's dated series, keyed by partner id.
///
/// Unreadable rows are skipped rather than failing the read: a date this crate
/// did not write is a row no reader can place, and the alternative — refusing to
/// build a return at all — is worse than falling back to the current split for
/// the partner it belongs to.
pub fn load_share_periods(conn: &Connection) -> Vec<(String, SharePeriod)> {
    let mut stmt = match conn.prepare(
        "SELECT partner_id, effective_from, profit_ppm, loss_ppm, capital_ppm
         FROM partner_share_periods ORDER BY partner_id, effective_from",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
        ))
    });
    let Ok(rows) = rows else { return Vec::new() };
    rows.flatten()
        .filter_map(|(id, from, profit_ppm, loss_ppm, capital_ppm)| {
            let effective_from = NaiveDate::parse_from_str(&from, "%Y-%m-%d").ok()?;
            Some((
                id,
                SharePeriod {
                    effective_from,
                    shares: Shares {
                        profit_ppm,
                        loss_ppm,
                        capital_ppm,
                    },
                },
            ))
        })
        .collect()
}

/// Hang each partner's dated series on their record.
///
/// One query for the whole list rather than one per partner, because the caller
/// is usually building a return and has every partner in hand already.
pub fn attach_history(conn: &Connection, partners: &mut [Partner]) {
    let periods = load_share_periods(conn);
    for partner in partners.iter_mut() {
        partner.history = periods
            .iter()
            .filter(|(id, _)| *id == partner.partner_id)
            .map(|(_, p)| *p)
            .collect();
    }
}

/// The partners, each carrying whatever history the books hold for them.
///
/// What anything preparing a *filing* should call. [`list_partners`] gives the
/// present-day split, which is the right answer for a screen showing who the
/// partners are and the wrong one for a K-1 covering a year that has ended.
pub fn list_partners_with_history(conn: &Connection) -> Vec<Partner> {
    let mut partners = list_partners(conn);
    attach_history(conn, &mut partners);
    partners
}

/// The partners on a year's return, each carrying their dated history.
///
/// The variant anything building a *return* should call. The one in
/// `partnership_commands` filters the same way but leaves `history` empty, which
/// makes every partner look as though their current split held all year — the
/// exact thing this module exists to stop.
pub fn partners_for_year(conn: &Connection, year: i32) -> Vec<Partner> {
    partners_for_year_with_problems(conn, year).0
}

/// The partners on a year's return with history, and every unreadable row.
pub fn partners_for_year_with_problems(
    conn: &Connection,
    year: i32,
) -> (Vec<Partner>, Vec<String>) {
    let (mut partners, problems) = pc::partners_for_year_with_problems(conn, year);
    attach_history(conn, &mut partners);
    (partners, problems)
}

/// The whole partnership's split as it stood on one day.
///
/// Partners who had not joined, or had already left, are absent — not present
/// with zeroes — so the totals this feeds are over the partners who actually
/// held something.
pub fn shares_on(partners: &[Partner], on: NaiveDate) -> Vec<(String, Shares)> {
    partners
        .iter()
        // `end_date` is the partner's last day, not the day after it — so a
        // partner who left on the 30th still held their interest on the 30th.
        .filter(|p| p.start_date <= on && !p.end_date.is_some_and(|e| on > e))
        .map(|p| (p.partner_id.clone(), p.shares_on(on)))
        .collect()
}

/// What is wrong with the split on a particular day, in words, if anything.
///
/// # Why this checks a day and not the table
///
/// The old check summed the current percentages across all partners and
/// complained when they did not reach 100%. On a partnership whose membership
/// changed mid-year that check is *always* wrong: the partner who left still has
/// their percentages on file, so the sum includes somebody who was gone, and the
/// warning fires on books that are perfectly correct. Asking about a specific
/// day is the question that has an answer.
pub fn problems_on(partners: &[Partner], on: NaiveDate) -> Vec<String> {
    let held = shares_on(partners, on);
    if held.is_empty() {
        return vec![format!("Nobody held an interest on {on}.")];
    }
    let mut problems = Vec::new();
    for (id, shares) in &held {
        if let Some((what, ppm)) = shares.out_of_range() {
            problems.push(format!(
                "On {on}, partner {id} has a {what} share of {:.4}%.",
                ppm as f64 / 10_000.0
            ));
        }
    }
    let totals = Shares::sums_to_whole(&held.iter().map(|(_, s)| *s).collect::<Vec<_>>());
    for (what, got) in [
        ("Profit", totals.profit_ppm),
        ("Loss", totals.loss_ppm),
        ("Capital", totals.capital_ppm),
    ] {
        if got != FULL_SHARE {
            problems.push(format!(
                "On {on}, the {} percentages add up to {:.4}%, not 100%.",
                what.to_lowercase(),
                got as f64 / 10_000.0
            ));
        }
    }
    problems
}

/// Every day inside a year on which anybody's percentages changed.
///
/// The boundaries a varying-interest allocation would divide the year at — see
/// §706(d) — and, short of that, the dates a reviewer should look at before
/// signing a return that assumes one split held all year.
pub fn change_dates_in(
    partners: &[Partner],
    year_start: NaiveDate,
    year_end: NaiveDate,
) -> Vec<NaiveDate> {
    let began = first_day(partners);
    let mut out: Vec<NaiveDate> = partners
        .iter()
        .flat_map(|p| p.change_dates_in(year_start, year_end))
        // The day the partnership came into existence is not a day its split
        // changed. Counting it warned every first-year return that its
        // percentages had moved mid-year — on books where they never had.
        .filter(|d| began.is_none_or(|b| *d > b))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The day the partnership began, as the partner records tell it.
///
/// The earliest day anybody held an interest. Not read from the business
/// profile, because the formation date there is what somebody typed on the
/// settings page and this is what the return will actually be built from; when
/// they disagree, the partners are the ones with K-1s.
pub fn first_day(partners: &[Partner]) -> Option<NaiveDate> {
    partners.iter().map(|p| p.start_date).min()
}

/// The days inside a year on which the split has to be checked.
///
/// The first day the partnership existed in that year, the last day of it, and
/// every day in between on which anybody's percentages moved. Between two
/// changes nothing moves, so these are every distinct split the year contained.
///
/// Clamped to the partnership's own first day, because a partnership formed in
/// April did not have a split on 1 January and asking about one produced a
/// warning that the books could never satisfy.
pub fn days_to_check(
    partners: &[Partner],
    year_start: NaiveDate,
    year_end: NaiveDate,
) -> Vec<NaiveDate> {
    let began = first_day(partners).unwrap_or(year_start).max(year_start);
    if began > year_end {
        return Vec::new();
    }
    let mut days = vec![began];
    days.extend(change_dates_in(partners, year_start, year_end));
    days.push(year_end);
    days.retain(|d| *d >= began && *d <= year_end);
    days.sort();
    days.dedup();
    days
}

/// Record what a partner's percentages became, and from when.
///
/// Writing the same `effective_from` twice replaces that step rather than adding
/// a second: two rows claiming the same day is a state no reader could resolve,
/// and the primary key would refuse it anyway.
pub fn set_partner_shares(
    store: &mut EventStore,
    user_id: &str,
    partner_id: &str,
    effective_from: NaiveDate,
    shares: Shares,
) -> Result<StoredEvent, PartnershipError> {
    let partner = list_partners(store.connection())
        .into_iter()
        .find(|p| p.partner_id == partner_id)
        .ok_or_else(|| PartnershipError::NoSuchPartner(partner_id.to_string()))?;
    check_set_partner_shares_pure(&partner.name, partner.start_date, effective_from, shares)?;
    // # Capturing what came before
    //
    // A partner with no recorded history who records only the *change* would be
    // left with a series whose earliest entry is that change — and
    // `Partner::shares_on` answers a date before the earliest entry with that
    // entry, so every prior year would silently take the new figure. The whole
    // point of recording the change is that prior years do not move.
    //
    // So the split that was in force until now is written down first, dated from
    // the day the partner joined. It is not a guess: it is what the books have
    // been saying all along, which is exactly the figure the prior years were
    // filed on.
    let periods = load_share_periods(store.connection());
    let has_history = periods.iter().any(|(id, _)| id == partner_id);
    if !has_history && effective_from > partner.start_date {
        append_event_locally(
            store,
            user_id,
            Event::PartnerSharesChanged {
                partner_id: partner_id.to_string(),
                effective_from: partner.start_date,
                profit_ppm: partner.shares.profit_ppm,
                loss_ppm: partner.shares.loss_ppm,
                capital_ppm: partner.shares.capital_ppm,
            },
        )?;
    }

    append_event_locally(
        store,
        user_id,
        Event::PartnerSharesChanged {
            partner_id: partner_id.to_string(),
            effective_from,
            profit_ppm: shares.profit_ppm,
            loss_ppm: shares.loss_ppm,
            capital_ppm: shares.capital_ppm,
        },
    )
}

/// What is wrong with a proposed change that can be decided without the books.
///
/// Split out so the group server can refuse the same things this does before it
/// takes the write lock — the pattern `check_set_relationship_pure` follows, and
/// for the same reason: a refusal that needs no locked state should not queue
/// behind one that does.
pub fn check_set_partner_shares_pure(
    partner_name: &str,
    start_date: NaiveDate,
    effective_from: NaiveDate,
    shares: Shares,
) -> Result<(), PartnershipError> {
    if effective_from < start_date {
        return Err(PartnershipError::InvalidData(format!(
            "{partner_name} joined on {start_date}, so their percentages cannot start on \
             {effective_from}, before that."
        )));
    }
    if let Some((what, ppm)) = shares.out_of_range() {
        return Err(PartnershipError::InvalidData(format!(
            "A {what} share of {:.4}% is not between nothing and the whole.",
            ppm as f64 / 10_000.0
        )));
    }
    Ok(())
}

/// Record a change against write-locked state.
///
/// The group server's path. Reads the partner inside the transaction, so two
/// members recording changes at once cannot both pass a check made against a
/// partner one of them has since withdrawn. The opening period is captured here
/// too, for the reason [`set_partner_shares`] captures it: without it every
/// prior year silently takes the new figure.
pub fn build_set_partner_shares_in_txn(
    tx: &rusqlite::Transaction<'_>,
    partner_id: &str,
    effective_from: NaiveDate,
    shares: Shares,
) -> Result<Vec<Event>, PartnershipError> {
    let row: Option<(String, String, i64, i64, i64)> = tx
        .query_row(
            "SELECT name, start_date, profit_ppm, loss_ppm, capital_ppm
               FROM partners WHERE id = ?1",
            [partner_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(|e| PartnershipError::StoreError(e.to_string()))?;
    let Some((name, start_raw, profit_ppm, loss_ppm, capital_ppm)) = row else {
        return Err(PartnershipError::NoSuchPartner(partner_id.to_string()));
    };
    let start_date = NaiveDate::parse_from_str(&start_raw, "%Y-%m-%d").map_err(|_| {
        PartnershipError::InvalidData(format!(
            "{name} has an unreadable start date {start_raw:?}. Fix the record and try again."
        ))
    })?;
    check_set_partner_shares_pure(&name, start_date, effective_from, shares)?;

    let has_history: bool = tx
        .query_row(
            "SELECT 1 FROM partner_share_periods WHERE partner_id = ?1",
            [partner_id],
            |_| Ok(true),
        )
        .optional()
        .map_err(|e| PartnershipError::StoreError(e.to_string()))?
        .unwrap_or(false);

    let mut events = Vec::new();
    if !has_history && effective_from > start_date {
        events.push(Event::PartnerSharesChanged {
            partner_id: partner_id.to_string(),
            effective_from: start_date,
            profit_ppm,
            loss_ppm,
            capital_ppm,
        });
    }
    events.push(Event::PartnerSharesChanged {
        partner_id: partner_id.to_string(),
        effective_from,
        profit_ppm: shares.profit_ppm,
        loss_ppm: shares.loss_ppm,
        capital_ppm: shares.capital_ppm,
    });
    Ok(events)
}

/// Warn when a change lands in a year whose return has already been built.
///
/// Not a refusal. Amending is legitimate and sometimes required, and a change
/// dated into a closed year is exactly what an amendment looks like. But it is
/// also exactly what a typo looks like — the wrong year in a date field — and
/// the difference between the two is something only the person typing knows. So
/// this hands them the sentence and lets them decide.
pub fn retrospective_warning(conn: &Connection, effective_from: NaiveDate) -> Option<String> {
    let year = effective_from.format("%Y").to_string();
    let closed: Result<i64, _> = conn.query_row(
        "SELECT COUNT(*) FROM fiscal_periods
         WHERE strftime('%Y', start_date) <= ?1 AND status = 'closed'",
        [&year],
        |r| r.get(0),
    );
    match closed {
        Ok(n) if n > 0 => Some(format!(
            "{effective_from} falls in or before a closed period. If a return has already been \
             filed for that year, changing the percentages behind it means amending it — the \
             Schedule K-1s it produced will no longer match what was filed."
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Residency};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn partner(id: &str, start: NaiveDate, end: Option<NaiveDate>, pct: f64) -> Partner {
        Partner {
            partner_id: id.to_string(),
            name: id.to_string(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: String::new(),
            address: Address::default(),
            start_date: start,
            end_date: end,
            shares: Shares::from_percents(pct, pct, pct),
            history: Vec::new(),
        }
    }

    /// End to end through the log: command, event, projection, read back.
    ///
    /// The unit tests above build `Partner` values by hand, which proves the
    /// arithmetic and nothing about whether a change actually survives being
    /// written. This one goes through the store.
    #[test]
    fn a_recorded_change_survives_the_round_trip_through_the_log() {
        use crate::commands::partnership_commands::{admit_partner, set_profile, AdmitPartner};
        use crate::domain::BusinessProfile;

        let mut s = crate::store::EventStore::in_memory().unwrap();
        crate::store::migrations::SchemaStore::init_schema(&mut s).unwrap();
        set_profile(
            &mut s,
            "u",
            &BusinessProfile {
                legal_name: "Test LLC".into(),
                address: Address {
                    street: "1 Example Street".into(),
                    suite: None,
                    city: "Cape Town".into(),
                    state: "WC".into(),
                    postal_code: "8001".into(),
                    country: None,
                },
                ein: "88-1234567".into(),
                naics_code: "541511".into(),
                formation_date: day(2020, 1, 1),
                principal_activity: None,
                principal_product: None,
            },
        )
        .unwrap();
        let admit = |name: &str, pct: f64| AdmitPartner {
            name: name.into(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: "Individual".into(),
            address: Address {
                street: "1 Example Street".into(),
                suite: None,
                city: "Cape Town".into(),
                state: "WC".into(),
                postal_code: "8001".into(),
                country: None,
            },
            start_date: Some(day(2020, 1, 1)),
            shares: Shares::from_percents(pct, pct, pct),
            tin: None,
        };
        let (a, _) = admit_partner(&mut s, "u", &admit("A", 50.0)).unwrap();
        let (b, _) = admit_partner(&mut s, "u", &admit("B", 50.0)).unwrap();

        set_partner_shares(
            &mut s,
            "u",
            &a,
            day(2024, 7, 1),
            Shares::from_percents(70.0, 70.0, 70.0),
        )
        .unwrap();
        set_partner_shares(
            &mut s,
            "u",
            &b,
            day(2024, 7, 1),
            Shares::from_percents(30.0, 30.0, 30.0),
        )
        .unwrap();

        let partners = list_partners_with_history(s.connection());
        let first = partners.iter().find(|p| p.partner_id == a).unwrap();
        assert_eq!(
            first.shares_on(day(2023, 6, 1)).profit_ppm,
            500_000,
            "2023 still reads 50% — recording only the change must capture what \
             was in force before it, or every prior year silently takes the new \
             figure and the change accomplishes nothing"
        );
        assert_eq!(
            first.history.len(),
            2,
            "the opening period was captured alongside the change: {:?}",
            first.history
        );
        assert_eq!(first.shares_on(day(2024, 12, 31)).profit_ppm, 700_000);
        assert_eq!(
            first.shares,
            first.shares_on(day(2024, 12, 31)),
            "the snapshot on `partners` follows the newest period"
        );

        // The prior year has to be clean, and the year of the change has to
        // report where it was cut.
        assert!(
            problems_on(&partners, day(2023, 6, 1)).is_empty(),
            "{:?}",
            problems_on(&partners, day(2023, 6, 1))
        );
        assert_eq!(
            change_dates_in(&partners, day(2024, 1, 1), day(2024, 12, 31)),
            vec![day(2024, 7, 1)]
        );

        // A correction to an earlier period must not move today's figures.
        set_partner_shares(
            &mut s,
            "u",
            &a,
            day(2020, 1, 1),
            Shares::from_percents(55.0, 55.0, 55.0),
        )
        .unwrap();
        let partners = list_partners_with_history(s.connection());
        let first = partners.iter().find(|p| p.partner_id == a).unwrap();
        assert_eq!(first.shares.profit_ppm, 700_000, "still 70% today");
        assert_eq!(first.shares_on(day(2021, 1, 1)).profit_ppm, 550_000);
    }

    /// The case that started all of this: three partners at a third each, one
    /// leaves, the remaining two go to 51/49. The 2023 K-1s must say a third.
    #[test]
    fn a_prior_year_k1_shows_the_split_that_year_not_todays() {
        let changed = day(2024, 7, 1);
        let third = Shares::from_percents(33.3334, 33.3334, 33.3334);
        let mut a = partner("a", day(2020, 1, 1), None, 51.0);
        a.history = vec![
            SharePeriod {
                effective_from: day(2020, 1, 1),
                shares: third,
            },
            SharePeriod {
                effective_from: changed,
                shares: Shares::from_percents(51.0, 51.0, 51.0),
            },
        ];

        let (beginning, ending) = a.shares_over(day(2023, 1, 1), day(2023, 12, 31));
        assert_eq!(beginning, third, "beginning of 2023 was a third");
        assert_eq!(ending, third, "end of 2023 was still a third");

        // And 2024, the year it changed, shows both ends honestly.
        let (b24, e24) = a.shares_over(day(2024, 1, 1), day(2024, 12, 31));
        assert_eq!(b24, third);
        assert_eq!(e24.profit_ppm, 510_000);
    }

    /// Books that have never recorded a change must behave exactly as before.
    #[test]
    fn no_recorded_history_means_the_current_split_held_throughout() {
        let a = partner("a", day(2020, 1, 1), None, 50.0);
        assert_eq!(a.shares_on(day(2021, 6, 1)), a.shares);
        assert_eq!(
            a.shares_over(day(2021, 1, 1), day(2021, 12, 31)),
            (a.shares, a.shares)
        );
    }

    /// A date before the first recorded step falls back to that step, not to
    /// today's numbers — a history missing its opening entry is still a better
    /// guide to the past than the present is.
    #[test]
    fn a_date_before_the_first_step_uses_the_oldest_split_known() {
        let mut a = partner("a", day(2019, 1, 1), None, 51.0);
        a.history = vec![SharePeriod {
            effective_from: day(2024, 7, 1),
            shares: Shares::from_percents(51.0, 51.0, 51.0),
        }];
        // Nothing recorded for 2020, but 51% is today's split, not 2020's.
        assert_eq!(a.shares_on(day(2020, 1, 1)).profit_ppm, 510_000);
    }

    /// The warning that used to fire on every correct set of books.
    ///
    /// Three partners at a third; one leaves mid-2024. Summing the current
    /// percentages across all three rows gives 100% only because the departed
    /// partner's numbers are still on file — and on the day after they left, the
    /// two who remain must reach 100% between them.
    #[test]
    fn a_partner_leaving_does_not_make_the_percentages_wrong() {
        let left = day(2024, 6, 30);
        let mut a = partner("a", day(2020, 1, 1), None, 51.0);
        let mut b = partner("b", day(2020, 1, 1), None, 49.0);
        let c = partner("c", day(2020, 1, 1), Some(left), 33.3332);
        let third_a = Shares::from_percents(33.3334, 33.3334, 33.3334);
        let third_b = Shares::from_percents(33.3334, 33.3334, 33.3334);
        a.history = vec![
            SharePeriod {
                effective_from: day(2020, 1, 1),
                shares: third_a,
            },
            SharePeriod {
                effective_from: day(2024, 7, 1),
                shares: Shares::from_percents(51.0, 51.0, 51.0),
            },
        ];
        b.history = vec![
            SharePeriod {
                effective_from: day(2020, 1, 1),
                shares: third_b,
            },
            SharePeriod {
                effective_from: day(2024, 7, 1),
                shares: Shares::from_percents(49.0, 49.0, 49.0),
            },
        ];
        let partners = vec![a, b, c];

        assert!(
            problems_on(&partners, day(2024, 1, 1)).is_empty(),
            "three at a third: {:?}",
            problems_on(&partners, day(2024, 1, 1))
        );
        assert!(
            problems_on(&partners, day(2024, 12, 31)).is_empty(),
            "two at 51/49: {:?}",
            problems_on(&partners, day(2024, 12, 31))
        );
        assert_eq!(
            change_dates_in(&partners, day(2024, 1, 1), day(2024, 12, 31)),
            vec![day(2024, 7, 1)],
            "the day the split changed is the day the departure took effect"
        );
    }

    /// A gap is a real problem and must still be reported.
    #[test]
    fn percentages_that_do_not_reach_a_hundred_are_reported() {
        let a = partner("a", day(2020, 1, 1), None, 40.0);
        let b = partner("b", day(2020, 1, 1), None, 40.0);
        let problems = problems_on(&[a, b], day(2021, 1, 1));
        assert_eq!(problems.len(), 3, "profit, loss and capital: {problems:?}");
        assert!(problems[0].contains("80.0000%"), "{problems:?}");
    }
}
