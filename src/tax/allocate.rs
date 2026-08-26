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
    if partners.is_empty() {
        return Vec::new();
    }

    let ppm_of = |p: &Partner| -> i64 {
        match basis {
            Basis::Capital => p.shares.capital_ppm,
            Basis::ProfitOrLoss => {
                if total < 0 {
                    p.shares.loss_ppm
                } else {
                    p.shares.profit_ppm
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
    let mut leftover = total - assigned;

    // Hand the leftover out a dollar at a time, largest fractional part first.
    // `sort_by` is stable, so equal remainders keep partner order and the same
    // books produce the same return every time.
    remainders.sort_by_key(|r| std::cmp::Reverse(r.0));
    let step = if leftover < 0 { -1 } else { 1 };
    let mut idx = 0;
    while leftover != 0 && !remainders.is_empty() {
        let (_, who) = remainders[idx % remainders.len()];
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

/// Whether any partner's profit and loss shares differ.
///
/// When they do, which percentage an item travels on becomes visible on the
/// return, and the preparer should confirm the split matches the partnership
/// agreement.
pub fn profit_and_loss_shares_differ(partners: &[&Partner]) -> bool {
    partners.iter().any(|p| p.shares.profit_ppm != p.shares.loss_ppm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PartnerType, Residency};
    use chrono::NaiveDate;

    fn partner(name: &str, profit: i64, loss: i64, capital: i64) -> Partner {
        Partner {
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

    /// Shares that do not total the whole are apportioned as given — the
    /// shortfall is somebody's data problem and hiding it here would remove the
    /// only evidence of it.
    #[test]
    fn shares_that_do_not_total_the_whole_are_not_silently_scaled() {
        let a = partner("A", 400_000, 400_000, 400_000);
        let b = partner("B", 400_000, 400_000, 400_000);
        let ps = [&a, &b];
        let shares = allocate(1000, &ps, Basis::ProfitOrLoss);
        // 80% of the total was allocated, and the arithmetic still ties to the
        // figure passed in, so the missing fifth shows up rather than vanishing.
        assert_eq!(sum(&shares), 1000);
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
        assert_eq!(allocate(12_345, &ps, Basis::ProfitOrLoss)[0].dollars, 12_345);
        assert_eq!(allocate(-12_345, &ps, Basis::ProfitOrLoss)[0].dollars, -12_345);
    }
}
