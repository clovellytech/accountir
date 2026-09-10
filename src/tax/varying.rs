//! Allocating a year whose partners' percentages did not stay still — §706(d).
//!
//! # The problem
//!
//! A partner who held 20% from April to September and then left is entitled to a
//! share of what the partnership earned while they held it. Splitting the year's
//! total on the percentages in force at 31 December gives them **nothing**: by
//! then they held nothing. The K-1 foots to Schedule K, is marked Final, states
//! a real 20% interest in item J — and carries a zero in box 1.
//!
//! Splitting on the percentages in force at 1 January is the same failure
//! pointing the other way, and any single day inside the year is arbitrary.
//!
//! # What §706(d) requires
//!
//! The year is divided into segments at each date a partner's interest changed,
//! and each segment is allocated on the percentages in force during it. The
//! statute gives two ways to decide what each segment earned:
//!
//! - **Interim closing of the books** — the default. The books are closed at
//!   each change and each segment's actual income is used.
//! - **Proration** — available by election. The year's total is spread across
//!   the segments in proportion to their length in days.
//!
//! Both are implemented here. Interim closing is used whenever the caller has a
//! ledger to read segment figures from, which is the default it should be;
//! proration is the fallback for a caller building a return from figures alone,
//! and it says which one it used.
//!
//! # How the arithmetic is arranged
//!
//! Rather than allocating each segment separately and adding up — which lets
//! each segment's rounding leftover through, so the K-1s no longer foot to
//! Schedule K — this converts the segments into one **effective percentage** per
//! partner per line, and hands that to the ordinary allocator. The exactness
//! guarantee in [`super::allocate`] then applies unchanged: the shares still sum
//! to the figure on Schedule K, to the dollar.
//!
//! A partner's effective percentage on a line is their claim divided by the
//! total claim, where a claim is the sum over segments of that segment's figure
//! times their percentage during it. Which of the profit and loss percentages
//! applies is decided **per segment**, on that segment's own sign — a
//! partnership that made money to June and lost it after allocates the first
//! part on profit percentages and the second on loss percentages, which is what
//! the two figures are for and what one annual sign cannot express.

use chrono::NaiveDate;

use crate::domain::Partner;
use crate::tax::allocate::Basis;
use crate::tax::lines::Form1065Lines;

/// How the year's figures were divided among its segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// The books closed at each change; each segment's actual figures used.
    /// The default under §706(d)(1).
    InterimClosing,
    /// The year's total spread across the segments by their length in days.
    /// Permitted by election.
    Proration,
}

impl Method {
    pub fn label(self) -> &'static str {
        match self {
            Method::InterimClosing => "interim closing of the books",
            Method::Proration => "proration by days",
        }
    }
}

/// One stretch of a year over which nobody's percentages changed.
#[derive(Debug, Clone)]
pub struct Segment {
    /// First day of the segment.
    pub from: NaiveDate,
    /// Last day of the segment, inclusive.
    pub to: NaiveDate,
    /// The figures for this segment. Under interim closing these come from the
    /// ledger; under proration they are the year's, and [`Segment::days`]
    /// carries the weight instead.
    pub lines: Option<Form1065Lines>,
}

impl Segment {
    pub fn days(&self) -> i64 {
        (self.to - self.from).num_days() + 1
    }
}

/// Divide a year at every date a partner's percentages changed.
///
/// The boundaries are the change dates themselves: a change effective 1 July
/// ends the segment on 30 June. A year with no changes comes back as one
/// segment, which is the ordinary case and which the caller can recognise by its
/// length rather than by asking again.
pub fn segments(
    partners: &[Partner],
    year_start: NaiveDate,
    year_end: NaiveDate,
) -> Vec<(NaiveDate, NaiveDate)> {
    let mut boundaries =
        crate::commands::share_period_commands::days_to_check(partners, year_start, year_end);
    let Some(&first) = boundaries.first() else {
        return Vec::new();
    };
    boundaries.retain(|d| *d > first);
    let mut out = Vec::new();
    let mut from = first;
    for change in boundaries {
        if change > year_end {
            break;
        }
        // The day before the change is the last day of the segment it ends.
        // `days_to_check` puts `year_end` in the list too, and that is a segment
        // boundary in the same sense: the last segment ends there.
        let to = if change == year_end {
            year_end
        } else {
            change.pred_opt().unwrap_or(change)
        };
        if to >= from {
            out.push((from, to));
        }
        if change == year_end {
            return out;
        }
        from = change;
    }
    if from <= year_end {
        out.push((from, year_end));
    }
    out
}

/// Each partner's effective percentage on one line, over a segmented year.
///
/// Returned in the same order as `partners`, in parts per million, and summing
/// to the whole except where the underlying percentages themselves do not — a
/// shortfall passes through rather than being scaled away, for the reason given
/// in [`super::allocate`].
///
/// `None` when the segments carry nothing to weight by: every segment figure is
/// zero, so there is no basis on which to prefer one partner's percentage to
/// another's, and the caller should fall back to a single split rather than
/// invent one.
pub fn effective_ppm(
    partners: &[&Partner],
    segments: &[Segment],
    line: &str,
    basis: Basis,
) -> Option<Vec<i64>> {
    // Each segment's weight: its own figure under interim closing, its figure
    // times its length under proration — the multiplication by days is what
    // spreads a single annual figure across segments in proportion.
    let weights: Vec<i128> = segments
        .iter()
        .map(|s| match &s.lines {
            Some(l) => figure_for(l, line) as i128,
            None => 0,
        })
        .collect();
    let total: i128 = weights.iter().sum();
    if total == 0 {
        return None;
    }

    let mut claims: Vec<i128> = vec![0; partners.len()];
    for (seg, weight) in segments.iter().zip(&weights) {
        if *weight == 0 {
            continue;
        }
        for (i, p) in partners.iter().enumerate() {
            // The percentages in force *during* the segment, read on its first
            // day: nothing changes inside a segment, by construction.
            let shares = p.shares_on(seg.from);
            let ppm = match basis {
                Basis::Capital => shares.capital_ppm,
                // Per segment, on that segment's own sign. A year that earned to
                // June and lost after allocates each part on the percentage that
                // part calls for.
                Basis::ProfitOrLoss => {
                    if *weight < 0 {
                        shares.loss_ppm
                    } else {
                        shares.profit_ppm
                    }
                }
            };
            claims[i] += weight * ppm as i128;
        }
    }

    // Truncating each claim independently loses up to a millionth per partner,
    // and that is enough: three partners each a millionth short make the
    // effective split total 999,997ppm, the allocator treats the missing three
    // millionths as a shortfall rather than as rounding, and a K-1 comes out a
    // dollar light against Schedule K. So the remainder is handed out the same
    // way the dollars are — largest fractional part first, stable, so the same
    // books give the same return every time.
    let target = claims.iter().sum::<i128>() / total;
    let mut floors: Vec<i128> = claims.iter().map(|c| c / total).collect();
    let mut remainders: Vec<(i128, usize)> = claims
        .iter()
        .enumerate()
        .map(|(i, c)| ((c - (c / total) * total).abs(), i))
        .collect();
    remainders.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut leftover = target - floors.iter().sum::<i128>();
    let step = if leftover < 0 { -1 } else { 1 };
    let mut idx = 0;
    while leftover != 0 && idx < remainders.len() {
        floors[remainders[idx].1] += step;
        leftover -= step;
        idx += 1;
    }

    Some(
        floors
            .iter()
            .map(|f| (*f).clamp(i64::MIN as i128, i64::MAX as i128) as i64)
            .collect(),
    )
}

/// The figure a line carries, derived lines included.
///
/// `Form1065Lines::get` answers for mapped lines; line 1 and the two `c` lines
/// are computed, and a segment's share of them is what the K-1 actually splits.
fn figure_for(lines: &Form1065Lines, key: &str) -> i64 {
    match key {
        "k1" => lines.k_line_1(),
        "k3c" => lines.k_line_3c(),
        "k4c" => lines.k_line_4c(),
        // Not a line on the form: the whole of Schedule K, which item L row 3 is
        // a share of. Named here so a segment can be weighted by it.
        ANALYSIS => lines.k_analysis(),
        other => lines.get(other),
    }
}

/// Split a year's figure on the split that actually held, reading the ledger.
///
/// The one entry point for "who gets what share of this year", used by both the
/// K-1s' Part III and item L so the two cannot drift apart. Where the year
/// contained a change of interest it is divided there and each part read from
/// the books (§706(d), interim closing); where it did not, the year-end
/// percentages apply and this is the ordinary allocation.
///
/// Falls back to the year-end split when the segments carry nothing to weight
/// by, which is the same fallback `split_across_partners` makes and names.
pub fn allocate_over_year(
    conn: &rusqlite::Connection,
    year: i32,
    total: i64,
    partners: &[&crate::domain::Partner],
    basis: Basis,
) -> Vec<crate::tax::allocate::Share> {
    let (year_start, year_end) = crate::commands::partnership_commands::calendar_year(year);
    // Only the dated series is taken from the books. Replacing the whole record
    // would silently overwrite whatever the caller passed — their percentages,
    // their dates — with the stored ones, and a caller that hands in a partner on
    // purpose (a projection, a what-if, a test) would find the ledger's figures
    // used instead of theirs.
    let periods = crate::commands::share_period_commands::load_share_periods(conn);
    let owned: Vec<crate::domain::Partner> = partners
        .iter()
        .map(|p| {
            let mut owned = (*p).clone();
            if owned.history.is_empty() {
                owned.history = periods
                    .iter()
                    .filter(|(id, _)| *id == owned.partner_id)
                    .map(|(_, period)| *period)
                    .collect();
            }
            owned
        })
        .collect();

    let spans = segments(&owned, year_start, year_end);
    if spans.len() > 1 {
        let mapping = crate::tax::lines::load_effective_mapping(conn, year);
        let limits = crate::tax::lines::load_effective_limits(conn, year);
        let segs: Vec<Segment> = spans
            .iter()
            .map(|(from, to)| Segment {
                from: *from,
                to: *to,
                lines: crate::queries::reports::Reports::new(conn)
                    .income_statement(*from, *to)
                    .ok()
                    .map(|st| crate::tax::lines::compute(&st, &mapping, &limits).lines),
            })
            .collect();
        // Referenced against the partners *as passed*, whose `history` the caller
        // may not have filled — so the borrowed slice is rebuilt from `owned`.
        let with_history: Vec<&crate::domain::Partner> = owned.iter().collect();
        if let Some(ppm) = effective_ppm(&with_history, &segs, ANALYSIS, basis) {
            return crate::tax::allocate::allocate_on_ppm(total, &ppm);
        }
    }
    let with_history: Vec<&crate::domain::Partner> = owned.iter().collect();
    crate::tax::allocate::allocate_as_of(total, &with_history, basis, Some(year_end))
}

/// The line key standing for "the whole of Schedule K", which item L row 3 and
/// the Analysis of Net Income are both a share of.
const ANALYSIS: &str = "k_analysis";

/// Spread one year's figures across its segments in proportion to their length.
///
/// The proration method: no ledger is read, so every segment gets the same
/// `Form1065Lines` and the weighting comes from the day counts. Used when the
/// caller has figures but no books to close.
pub fn prorate(annual: &Form1065Lines, spans: &[(NaiveDate, NaiveDate)]) -> Vec<Segment> {
    let total_days: i64 = spans.iter().map(|(f, t)| (*t - *f).num_days() + 1).sum();
    if total_days == 0 {
        return Vec::new();
    }
    spans
        .iter()
        .map(|(from, to)| {
            let days = (*to - *from).num_days() + 1;
            Segment {
                from: *from,
                to: *to,
                lines: Some(annual.scaled(days, total_days)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Residency, SharePeriod, Shares};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn partner(id: &str, start: NaiveDate, end: Option<NaiveDate>) -> Partner {
        Partner {
            partner_id: id.to_string(),
            name: id.to_string(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: String::new(),
            address: Address::default(),
            start_date: start,
            end_date: end,
            shares: Shares::default(),
            history: Vec::new(),
        }
    }

    fn step(from: NaiveDate, pct: f64) -> SharePeriod {
        SharePeriod {
            effective_from: from,
            shares: Shares::from_percents(pct, pct, pct),
        }
    }

    #[test]
    fn a_year_with_no_changes_is_one_segment() {
        let mut a = partner("a", day(2020, 1, 1), None);
        a.history = vec![step(day(2020, 1, 1), 100.0)];
        let spans = segments(&[a], day(2025, 1, 1), day(2025, 12, 31));
        assert_eq!(spans, vec![(day(2025, 1, 1), day(2025, 12, 31))]);
    }

    /// A change on 1 July ends the first segment on 30 June, not on 1 July.
    #[test]
    fn a_change_ends_the_segment_the_day_before_it() {
        let mut a = partner("a", day(2020, 1, 1), None);
        a.history = vec![step(day(2020, 1, 1), 50.0), step(day(2025, 7, 1), 60.0)];
        let mut b = partner("b", day(2020, 1, 1), None);
        b.history = vec![step(day(2020, 1, 1), 50.0), step(day(2025, 7, 1), 40.0)];
        assert_eq!(
            segments(&[a, b], day(2025, 1, 1), day(2025, 12, 31)),
            vec![
                (day(2025, 1, 1), day(2025, 6, 30)),
                (day(2025, 7, 1), day(2025, 12, 31)),
            ]
        );
    }

    /// The case this module exists for: a partner in for part of the year is
    /// allocated a share of it, not zero.
    #[test]
    fn a_partner_who_left_midyear_gets_the_share_they_held_while_they_held_it() {
        let mut a = partner("a", day(2020, 1, 1), None);
        a.history = vec![step(day(2020, 1, 1), 50.0), step(day(2025, 7, 1), 100.0)];
        let mut gone = partner("gone", day(2020, 1, 1), Some(day(2025, 6, 30)));
        gone.history = vec![step(day(2020, 1, 1), 50.0)];
        let ps = [&a, &gone];

        // Equal halves, so an equal split of the first half and all of the
        // second: 75% and 25%.
        let spans = segments(
            &[a.clone(), gone.clone()],
            day(2025, 1, 1),
            day(2025, 12, 31),
        );
        let segs: Vec<Segment> = spans
            .iter()
            .map(|(f, t)| Segment {
                from: *f,
                to: *t,
                lines: Some(Form1065Lines::from_pairs(&[("l1a", 1_000)])),
            })
            .collect();

        let ppm = effective_ppm(&ps, &segs, "l1a", Basis::ProfitOrLoss).unwrap();
        assert_eq!(
            ppm[0], 750_000,
            "half of the first part plus all the second"
        );
        assert_eq!(ppm[1], 250_000, "half of the first part and nothing after");
    }

    /// The effective split has to total the whole, or the K-1s come out a dollar
    /// light against Schedule K.
    ///
    /// Three partners over two segments is the shape that exposed it: each
    /// partner's share truncated independently, the effective split totalled
    /// 999,997ppm, and the allocator correctly treated the missing three
    /// millionths as a shortfall to leave unallocated.
    #[test]
    fn an_effective_split_totals_the_whole() {
        let mut a = partner("a", day(2020, 1, 1), None);
        a.history = vec![step(day(2020, 1, 1), 60.0), step(day(2025, 4, 1), 48.0)];
        let mut b = partner("b", day(2020, 1, 1), None);
        b.history = vec![step(day(2020, 1, 1), 40.0), step(day(2025, 4, 1), 32.0)];
        let mut c = partner("c", day(2025, 4, 1), None);
        c.history = vec![step(day(2025, 4, 1), 20.0)];
        let ps = [&a, &b, &c];

        let spans = vec![
            (day(2025, 1, 1), day(2025, 3, 31)),
            (day(2025, 4, 1), day(2025, 12, 31)),
        ];
        let segs: Vec<Segment> = spans
            .iter()
            .map(|(f, t)| Segment {
                from: *f,
                to: *t,
                lines: Some(Form1065Lines::from_pairs(&[("l1a", 100_000)])),
            })
            .collect();

        let ppm = effective_ppm(&ps, &segs, "l1a", Basis::ProfitOrLoss).unwrap();
        assert_eq!(
            ppm.iter().sum::<i64>(),
            1_000_000,
            "effective split must total the whole, got {ppm:?}"
        );

        // And the dollars it produces foot exactly.
        let shares = crate::tax::allocate::allocate_on_ppm(287_925, &ppm);
        assert_eq!(shares.iter().map(|s| s.dollars).sum::<i64>(), 287_925);
    }

    /// Segments carrying nothing give no basis for preferring anybody, and say
    /// so rather than inventing one.
    #[test]
    fn segments_that_carry_nothing_produce_no_effective_split() {
        let mut a = partner("a", day(2020, 1, 1), None);
        a.history = vec![step(day(2020, 1, 1), 100.0)];
        let ps = [&a];
        let segs = vec![Segment {
            from: day(2025, 1, 1),
            to: day(2025, 12, 31),
            lines: Some(Form1065Lines::default()),
        }];
        assert!(effective_ppm(&ps, &segs, "k1", Basis::ProfitOrLoss).is_none());
    }

    /// Proration weights by days, so an unequal split of the year is an unequal
    /// split of the figures.
    #[test]
    fn proration_weights_the_segments_by_their_length() {
        let annual = Form1065Lines::from_pairs(&[("l1a", 36_500)]);
        let spans = vec![
            (day(2025, 1, 1), day(2025, 3, 31)),
            (day(2025, 4, 1), day(2025, 12, 31)),
        ];
        let segs = prorate(&annual, &spans);
        assert_eq!(segs[0].days(), 90);
        assert_eq!(segs[1].days(), 275);
        assert_eq!(segs[0].lines.as_ref().unwrap().get("l1a"), 9_000);
        assert_eq!(segs[1].lines.as_ref().unwrap().get("l1a"), 27_500);
    }
}
