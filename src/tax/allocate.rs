//! Splitting Schedule K across the partners, for their Schedules K-1.
//!
//! # Which percentage applies
//!
//! A partner carries three shares — profit, loss and capital — and they are not
//! always equal. The rule here is the one the form's own wording implies:
//!
//! - an item that **is** income (a positive figure) is split on the **profit**
//!   share;
//! - an item that is a **loss** (a negative figure) is split on the **loss**
//!   share;
//! - the capital share is used only for capital-account figures, never for
//!   distributive share items.
//!
//! Split per item and on the item's own sign, not on whether the partnership had
//! a good year overall: a partnership can have ordinary income and a section 1231
//! loss in the same year, and those two travel on different percentages.
//!
//! Most partnerships set profit and loss to the same number, in which case none
//! of this is observable. When they differ it matters a great deal, so
//! [`allocate`] says so in a warning rather than letting the choice pass silently.
//!
//! # Why the shares are rounded by largest remainder
//!
//! Each partner's K-1 shows whole dollars, and the K-1s have to add back to the
//! Schedule K figure they came from. Rounding each share independently does not
//! do that: three partners at a third of $100 round to $33 each and lose a
//! dollar, and a return whose K-1s total $99 against a Schedule K of $100 is a
//! mismatch an examiner sees immediately.
//!
//! So the shares are apportioned by largest remainder — every partner gets the
//! floor of their exact share, and the dollars left over go to the partners with
//! the largest fractional parts, one each. The total is then exact by
//! construction, and the discrepancy is at most one dollar per partner rather
//! than accumulating.
//!
//! Ties are broken by the order the partners appear, which is stable because the
//! caller passes them in a fixed order. An arbitrary-but-stable rule is what
//! keeps regenerating the same return twice from producing two different sets of
//! K-1s.

use crate::domain::Partner;
use chrono::NaiveDate;

/// Parts per million of the whole; 100% is 1,000,000.
pub const PPM_WHOLE: i64 = 1_000_000;

/// Which of a partner's three shares an item travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Positive figures use the profit share, negative ones the loss share.
    ProfitOrLoss,
    /// Capital-account figures.
    Capital,
}

/// One partner's share of one figure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Share {
    /// Index into the partner slice this was allocated from.
    pub partner: usize,
    pub dollars: i64,
}

/// Split `total` across `partners`, exactly.
///
/// The returned shares are in the same order as `partners` and always sum to
/// `total` — see the module docs for why that is the whole point.
///
/// A partnership whose shares do not total 100% is not corrected here: the
/// figures are apportioned on the percentages as given, and the shortfall shows
/// up as a total that does not match. [`crate::tax::form1065`] already warns
/// about shares that do not add up, and silently scaling them here would hide
/// the fact that they do not.
pub fn allocate(total: i64, partners: &[&Partner], basis: Basis) -> Vec<Share> {
    allocate_as_of(total, partners, basis, None)
}

/// Split `total` on the percentages that were in force on `on`.
///
/// # Why a date belongs here
///
/// [`allocate`] splits on `Partner::shares`, which is the split as it stands
/// *now*. Rebuilding a 2023 return in 2026 therefore allocated 2023's income on
/// 2026's percentages — and once item J started reading the dated series, the
/// two halves of the same K-1 disagreed: the header said a third and box 1 said
/// 51%. Passing the year's own date keeps the whole page describing one
/// partnership.
///
/// `None` means "as they stand now", which is what a projection of the year in
/// progress wants and what books with no recorded history have always given.
///
/// # What this is not
///
/// One date, so one split for the whole year. A partnership whose percentages
/// changed mid-year needs the year divided at the change and each segment
/// allocated on its own split — §706(d), by interim closing of the books or by
/// proration if elected. That is a larger piece of work; until it lands,
/// `split_across_partners` warns when a year contains a change rather than
/// letting a single-split allocation pass for a segmented one.
pub fn allocate_as_of(
    total: i64,
    partners: &[&Partner],
    basis: Basis,
    on: Option<NaiveDate>,
) -> Vec<Share> {
    if partners.is_empty() {
        return Vec::new();
    }

    let ppm_of = |p: &Partner| -> i64 {
        let shares = match on {
            Some(day) => p.shares_on(day),
            None => p.shares,
        };
        match basis {
            Basis::Capital => shares.capital_ppm,
            Basis::ProfitOrLoss => {
                if total < 0 {
                    shares.loss_ppm
                } else {
                    shares.profit_ppm
                }
            }
        }
    };

    // Exact share in millionths of a dollar, so the floor and the remainder are
    // both integers and no float ever touches a figure on a tax return.
    let mut floors: Vec<i64> = Vec::with_capacity(partners.len());
    let mut remainders: Vec<(i64, usize)> = Vec::with_capacity(partners.len());
    for (i, p) in partners.iter().enumerate() {
        let exact = total * ppm_of(p);
        // Truncating division rounds toward zero, which for a negative total
        // means the floor is the *larger* value and the remainder is negative.
        // Taking the magnitude of the remainder keeps "who is owed the next
        // dollar" the same question in both directions.
        let floor = exact / PPM_WHOLE;
        let rem = (exact - floor * PPM_WHOLE).abs();
        floors.push(floor);
        remainders.push((rem, i));
    }

    let assigned: i64 = floors.iter().sum();
    // What the percentages *as given* come to, before rounding. On a split that
    // totals the whole this is `total`; on one that does not it is the smaller
    // figure the percentages actually describe, and the difference between it
    // and `total` is the shortfall — which is not this loop's to hand out.
    let sum_ppm: i128 = partners.iter().map(|p| ppm_of(p) as i128).sum();
    let intended = (total as i128 * sum_ppm / PPM_WHOLE as i128) as i64;
    let mut leftover = intended - assigned;

    // Hand the *rounding* leftover out a dollar at a time, largest fractional
    // part first. `sort_by` is stable, so equal remainders keep partner order
    // and the same books produce the same return every time.
    //
    // # Why the loop is bounded, and why it skips zero shares
    //
    // Splitting on percentages that do not total 100% leaves a shortfall that is
    // not rounding — 80% of a thousand dollars leaves two hundred — and the
    // unbounded version of this loop handed that shortfall out too, cycling
    // until the figures footed. The result was neither the percentages as given
    // nor a proportional scaling of them but an arbitrary round robin, and the
    // worst case was silent: two partners with a **0% loss share** were each
    // allocated half of a ten-thousand-dollar loss, out of nothing.
    //
    // So the leftover handed out here is measured against what the percentages
    // come to, not against `total`. That difference is under one dollar per
    // partner by construction, which is what rounding leftover means — and each
    // partner can receive at most one of it, so the loop cannot cycle. The
    // shortfall is left unallocated, so the total does not foot: what the doc
    // comment above promises, and what `form1065::check` warns about, rather
    // than being papered over by a return that adds up and is wrong.
    remainders.sort_by_key(|r| std::cmp::Reverse(r.0));
    remainders.retain(|(_, i)| ppm_of(partners[*i]) != 0);
    let step = if leftover < 0 { -1 } else { 1 };
    let mut idx = 0;
    while leftover != 0 && idx < remainders.len() {
        let (_, who) = remainders[idx];
        floors[who] += step;
        leftover -= step;
        idx += 1;
    }

    floors
        .into_iter()
        .enumerate()
        .map(|(partner, dollars)| Share { partner, dollars })
        .collect()
}

/// Split `total` on percentages given directly.
///
/// The same exact arithmetic as [`allocate`], but taking the percentages rather
/// than reading them off partners — for [`crate::tax::varying`], which computes
/// an effective split over a year divided at each change of interest. Sharing
/// the loop rather than copying it keeps the guarantee that the shares sum to
/// `total` in one place.
pub fn allocate_on_ppm(total: i64, ppm: &[i64]) -> Vec<Share> {
    if ppm.is_empty() {
        return Vec::new();
    }
    let mut floors: Vec<i64> = Vec::with_capacity(ppm.len());
    let mut remainders: Vec<(i64, usize)> = Vec::with_capacity(ppm.len());
    for (i, share) in ppm.iter().enumerate() {
        let exact = total * share;
        let floor = exact / PPM_WHOLE;
        remainders.push(((exact - floor * PPM_WHOLE).abs(), i));
        floors.push(floor);
    }
    let assigned: i64 = floors.iter().sum();
    let sum_ppm: i128 = ppm.iter().map(|p| *p as i128).sum();
    let intended = (total as i128 * sum_ppm / PPM_WHOLE as i128) as i64;
    let mut leftover = intended - assigned;

    remainders.sort_by_key(|r| std::cmp::Reverse(r.0));
    remainders.retain(|(_, i)| ppm[*i] != 0);
    let step = if leftover < 0 { -1 } else { 1 };
    let mut idx = 0;
    while leftover != 0 && idx < remainders.len() {
        floors[remainders[idx].1] += step;
        leftover -= step;
        idx += 1;
    }
    floors
        .into_iter()
        .enumerate()
        .map(|(partner, dollars)| Share { partner, dollars })
        .collect()
}

/// Whether any partner's profit and loss shares differ.
///
/// When they do, which percentage an item travels on becomes visible on the
/// return, and the preparer should confirm the split matches the partnership
/// agreement.
pub fn profit_and_loss_shares_differ(partners: &[&Partner]) -> bool {
    partners
        .iter()
        .any(|p| p.shares.profit_ppm != p.shares.loss_ppm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PartnerType, Residency};
    use chrono::NaiveDate;

    fn partner(name: &str, profit: i64, loss: i64, capital: i64) -> Partner {
        Partner {
            history: Vec::new(),
            partner_id: name.to_string(),
            name: name.to_string(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: "Individual".to_string(),
            address: crate::domain::Address {
                street: "1 Main".into(),
                suite: None,
                city: "Town".into(),
                state: "TX".into(),
                postal_code: "70000".into(),
                country: None,
            },
            start_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            end_date: None,
            shares: crate::domain::Shares {
                profit_ppm: profit,
                loss_ppm: loss,
                capital_ppm: capital,
            },
        }
    }

    fn sum(shares: &[Share]) -> i64 {
        shares.iter().map(|s| s.dollars).sum()
    }

    /// The whole reason this module exists: three ways of $100 must still be
    /// $100 on the return.
    #[test]
    fn thirds_of_a_hundred_still_add_to_a_hundred() {
        let a = partner("A", 333_333, 333_333, 333_333);
        let b = partner("B", 333_333, 333_333, 333_333);
        let c = partner("C", 333_334, 333_334, 333_334);
        let ps = [&a, &b, &c];

        let shares = allocate(100, &ps, Basis::ProfitOrLoss);
        assert_eq!(sum(&shares), 100, "{shares:?}");
        // Nobody is more than a dollar off their exact share.
        for s in &shares {
            assert!((s.dollars - 33).abs() <= 1, "{s:?}");
        }
    }

    #[test]
    fn a_loss_uses_the_loss_share_and_income_uses_the_profit_share() {
        // A carries the losses; B takes most of the profit.
        let a = partner("A", 100_000, 900_000, 500_000);
        let b = partner("B", 900_000, 100_000, 500_000);
        let ps = [&a, &b];

        let income = allocate(1000, &ps, Basis::ProfitOrLoss);
        assert_eq!(income[0].dollars, 100);
        assert_eq!(income[1].dollars, 900);

        let loss = allocate(-1000, &ps, Basis::ProfitOrLoss);
        assert_eq!(loss[0].dollars, -900);
        assert_eq!(loss[1].dollars, -100);
    }

    /// A loss must not lose or gain a dollar in rounding either.
    #[test]
    fn a_loss_allocates_exactly_too() {
        let a = partner("A", 333_333, 333_333, 0);
        let b = partner("B", 333_333, 333_333, 0);
        let c = partner("C", 333_334, 333_334, 0);
        let ps = [&a, &b, &c];
        let shares = allocate(-100, &ps, Basis::ProfitOrLoss);
        assert_eq!(sum(&shares), -100, "{shares:?}");
        for s in &shares {
            assert!(s.dollars <= 0, "a loss must not hand anybody income: {s:?}");
        }
    }

    #[test]
    fn the_capital_share_is_used_for_capital_figures() {
        let a = partner("A", 500_000, 500_000, 250_000);
        let b = partner("B", 500_000, 500_000, 750_000);
        let ps = [&a, &b];
        let shares = allocate(1000, &ps, Basis::Capital);
        assert_eq!(shares[0].dollars, 250);
        assert_eq!(shares[1].dollars, 750);
    }

    /// Regenerating the same return twice must produce the same K-1s, including
    /// which partner got the odd dollar.
    #[test]
    fn the_odd_dollar_lands_in_the_same_place_every_time() {
        let a = partner("A", 333_333, 333_333, 0);
        let b = partner("B", 333_333, 333_333, 0);
        let c = partner("C", 333_334, 333_334, 0);
        let ps = [&a, &b, &c];
        let first = allocate(100, &ps, Basis::ProfitOrLoss);
        for _ in 0..25 {
            assert_eq!(allocate(100, &ps, Basis::ProfitOrLoss), first);
        }
    }

    #[test]
    fn zero_splits_to_zero_and_no_partners_splits_to_nothing() {
        let a = partner("A", 500_000, 500_000, 500_000);
        let ps = [&a];
        assert_eq!(sum(&allocate(0, &ps, Basis::ProfitOrLoss)), 0);
        assert!(allocate(100, &[], Basis::ProfitOrLoss).is_empty());
    }

    /// Shares that do not total the whole are apportioned as given.
    ///
    /// This test used to assert `sum == 1000` and comment that "80% of the total
    /// was allocated" — while the code allocated 100% of it. The name said the
    /// right thing, the assertion passed, and the behaviour was the opposite of
    /// both: the leftover loop handed the missing fifth out a dollar at a time
    /// until the figures footed, so 40/40 became 50/50 on the return.
    #[test]
    fn shares_that_do_not_total_the_whole_are_not_silently_scaled() {
        let a = partner("A", 400_000, 400_000, 400_000);
        let b = partner("B", 400_000, 400_000, 400_000);
        let ps = [&a, &b];
        let shares = allocate(1000, &ps, Basis::ProfitOrLoss);
        assert_eq!(shares[0].dollars, 400, "40% of a thousand is four hundred");
        assert_eq!(shares[1].dollars, 400);
        assert_eq!(
            sum(&shares),
            800,
            "the missing fifth stays missing, which is the only evidence of it"
        );
    }

    /// The worst shape the old leftover loop produced.
    ///
    /// Two partners with a **0% loss share** and a ten-thousand-dollar loss: the
    /// loop gave them five thousand each, allocated out of nothing, on a K-1
    /// that footed perfectly to Schedule K.
    #[test]
    fn a_zero_share_is_allocated_nothing_however_large_the_loss() {
        let a = partner("A", 500_000, 0, 500_000);
        let b = partner("B", 500_000, 0, 500_000);
        let shares = allocate(-10_000, &[&a, &b], Basis::ProfitOrLoss);
        assert_eq!(shares[0].dollars, 0);
        assert_eq!(shares[1].dollars, 0);
    }

    /// A shortfall leaks nothing at all, not even the first dollar.
    ///
    /// The bounded loop still handed out one dollar per partner before it
    /// stopped, so a sole partner at 50% of ten thousand got 5,001. The leftover
    /// is now measured against what the percentages come to rather than against
    /// the total, so on a shortfall there is no rounding leftover to hand out.
    #[test]
    fn a_shortfall_does_not_leak_even_a_single_dollar() {
        let a = partner("A", 500_000, 500_000, 500_000);
        assert_eq!(
            allocate(10_000, &[&a], Basis::ProfitOrLoss)[0].dollars,
            5_000
        );
        let b = partner("B", 100_000, 100_000, 100_000);
        let shares = allocate(10_000, &[&a, &b], Basis::ProfitOrLoss);
        assert_eq!(sum(&shares), 6_000, "60% of ten thousand, and no more");
    }

    /// The percentages the real book uses, so the ordinary case stays exact.
    #[test]
    fn a_whole_split_still_foots_to_the_figure_it_was_given() {
        let a = partner("A", 510_000, 0, 0);
        let b = partner("B", 490_000, 1_000_000, 1_000_000);
        for total in [287_925_i64, -287_925, 1, 0, 999_999] {
            let shares = allocate(total, &[&a, &b], Basis::ProfitOrLoss);
            assert_eq!(sum(&shares), total, "total {total} did not foot");
        }
    }

    #[test]
    fn differing_profit_and_loss_shares_are_detectable() {
        let same = partner("A", 500_000, 500_000, 500_000);
        let diff = partner("B", 500_000, 400_000, 500_000);
        assert!(!profit_and_loss_shares_differ(&[&same]));
        assert!(profit_and_loss_shares_differ(&[&same, &diff]));
    }

    /// A single partner takes the whole figure, exactly, with no rounding
    /// artefact.
    #[test]
    fn a_sole_partner_takes_everything() {
        let a = partner("A", PPM_WHOLE, PPM_WHOLE, PPM_WHOLE);
        let ps = [&a];
        assert_eq!(
            allocate(12_345, &ps, Basis::ProfitOrLoss)[0].dollars,
            12_345
        );
        assert_eq!(
            allocate(-12_345, &ps, Basis::ProfitOrLoss)[0].dollars,
            -12_345
        );
    }
}
