//! MACRS: turning the asset register into a year's deduction.
//!
//! # Computed, not looked up
//!
//! The IRS publishes depreciation as percentage tables (Pub. 946, Appendix A) —
//! a grid of recovery year against class. This module computes the same numbers
//! from the rules the tables were built from instead of carrying the tables.
//!
//! Two reasons. The tables assume a full 12-month tax year and say nothing about
//! a disposal part-way through one, so a table-driven implementation needs the
//! computation anyway for every year an asset leaves. And a table is 300 numbers
//! with no structure a reader can check: a transposed digit is invisible, where a
//! wrong rate here fails a test against the published percentages.
//!
//! # Where the computed figures and the published ones differ
//!
//! By up to a hundredth of a percentage point, in some years, and it is the
//! *tables* that are adjusted rather than this. The published percentages are
//! rounded so each column sums to exactly 100.000, which takes a digit here and
//! there away from the arithmetic: 7-year property is 8.9249% in each of years
//! five, six and seven, and Table A-1 prints 8.93, 8.92, 8.93. The tests below
//! assert the computed schedule against the published tables to within that
//! hundredth, and assert separately that the schedule recovers the basis exactly
//! — which is the property the IRS was buying with the adjustment.
//!
//! On a $100,000 asset the difference is about ten dollars in a year and nothing
//! over the life, since both recover the same basis. A preparer reconciling this
//! against table-driven software should expect that and not hunt for it.
//!
//! # The order the three deductions come in
//!
//! An asset can be written off three ways in its first year, and the order is
//! fixed by statute, not by preference:
//!
//! 1. **§179** comes off the cost first.
//! 2. **Bonus** takes its percentage of what is left.
//! 3. **MACRS** runs on what remains after both.
//!
//! Doing this in any other order overstates the deduction, because each step
//! reduces the basis the next one works on. And only two of the three reach page
//! 1 line 16a: §179 is separately stated on Schedule K line 12, because the
//! dollar limit and the taxable-income limit are applied on each partner's own
//! return rather than here. [`YearSchedule::line_16a_cents`] and
//! [`YearSchedule::section_179_cents`] are deliberately separate for that reason,
//! and nothing in this module adds them together.
//!
//! # The declining-balance switch
//!
//! Declining balance would never finish — each year takes a fraction of what is
//! left, so something is always left. MACRS switches to straight line over the
//! *remaining* recovery period in the first year that gives a bigger deduction,
//! and stays there. That switch is not an election; it is how the published
//! tables are built, and reproducing them requires it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Datelike, NaiveDate};

use crate::domain::{
    BonusElection, Convention, DepreciableAsset, Method, PropertyClass, Section179Eligibility,
    System,
};

/// The share of a year's personal property that may arrive in the last quarter
/// before the mid-quarter convention is forced on all of it. §168(d)(3).
const MID_QUARTER_THRESHOLD: f64 = 0.40;

/// The bonus depreciation rate for an asset, as a fraction.
///
/// Two rules stacked, and the order matters:
///
/// The **phase-down** in §168(k)(6) steps the rate down by the year property is
/// placed in service — 80% for 2023, 60% for 2024, 40% for 2025, 20% for 2026,
/// nothing after.
///
/// The **restoration** enacted in July 2025 puts the rate back to 100% for
/// property *acquired* after 19 January 2025. Acquired, not placed in service:
/// a press ordered in March 2025 and first run in 2026 takes 100%, while one
/// ordered in 2024 and first run in 2025 takes the 40% its placed-in-service
/// year earns. That is why the register holds both dates — one field could not
/// answer this, and the difference on a large asset is most of its cost.
pub fn bonus_rate(asset: &DepreciableAsset) -> f64 {
    if !asset.class.bonus_eligible() || asset.bonus == BonusElection::Decline {
        return 0.0;
    }
    // The restoration, keyed to acquisition.
    let restored = NaiveDate::from_ymd_opt(2025, 1, 20).expect("20 January 2025 exists");
    if asset.acquired_on >= restored {
        return 1.0;
    }
    // Otherwise the phase-down, keyed to the year it was placed in service.
    match asset.placed_in_service.year() {
        y if y <= 2022 => 1.0,
        2023 => 0.80,
        2024 => 0.60,
        2025 => 0.40,
        2026 => 0.20,
        _ => 0.0,
    }
}

/// Whether the mid-quarter convention is forced on property placed in service in
/// `pis_year`.
///
/// §168(d)(3): if more than 40% of the year's *personal* property, by basis,
/// arrived in the last three months, then every piece of personal property placed
/// in service that year moves from half-year to mid-quarter. All of it — not only
/// the assets that arrived late.
///
/// Three details that are easy to get wrong and all change the answer:
///
/// - Basis is taken **after §179 and before bonus**. §179 reduces what goes into
///   the fraction; bonus does not.
/// - Real property is outside the test entirely — it is on mid-month, which this
///   convention never displaces — so it neither counts toward the 40% nor gets
///   moved by it.
/// - Property placed in service and disposed of in the **same year** is excluded,
///   so an asset that came and went cannot drag the rest of the year onto
///   mid-quarter behind it.
pub fn mid_quarter_applies(assets: &[DepreciableAsset], pis_year: i32) -> bool {
    let mut total = 0i64;
    let mut fourth_quarter = 0i64;

    for a in assets {
        if a.placed_in_service.year() != pis_year
            || !a.class.is_personal_property()
            || a.placed_and_disposed_same_year()
        {
            continue;
        }
        let basis = (a.cost_cents - a.section_179_cents).max(0);
        total += basis;
        if a.quarter_placed() == 4 {
            fourth_quarter += basis;
        }
    }

    total > 0 && (fourth_quarter as f64) > MID_QUARTER_THRESHOLD * (total as f64)
}

/// The fraction of the first year the asset counts for.
fn first_year_fraction(convention: Convention, placed: NaiveDate) -> f64 {
    match convention {
        Convention::HalfYear => 0.5,
        // Mid-quarter: from the middle of the quarter it arrived in. Q1 leaves
        // 3.5 quarters of the year, Q4 leaves half of one.
        Convention::MidQuarter => {
            let q = (placed.month() - 1) / 3 + 1;
            (4.0 - q as f64 + 0.5) / 4.0
        }
        // Mid-month: from the middle of the month it arrived in.
        Convention::MidMonth => (12.0 - placed.month() as f64 + 0.5) / 12.0,
    }
}

/// The fraction of the disposal year the asset counts for.
///
/// The mirror of [`first_year_fraction`]: the same convention that decided when
/// the asset started deciding when it stops. Half-year gives half the year
/// whenever it goes; mid-quarter and mid-month give it up to the middle of the
/// quarter or month it left in.
fn disposal_year_fraction(convention: Convention, disposed: NaiveDate) -> f64 {
    match convention {
        Convention::HalfYear => 0.5,
        Convention::MidQuarter => {
            let q = (disposed.month() - 1) / 3 + 1;
            (q as f64 - 0.5) / 4.0
        }
        Convention::MidMonth => (disposed.month() as f64 - 0.5) / 12.0,
    }
}

/// How much of each tax year the asset is in service for, across the whole
/// recovery period.
///
/// The first entry is the convention's opening fraction; whole years follow; the
/// last is whatever is left. They sum to the recovery period, which is why a
/// 5-year asset appears on six returns and a 39-year building on forty.
fn year_fractions(first: f64, life: f64) -> Vec<f64> {
    let mut out = vec![first];
    let mut used = first;
    // A whole year at a time while a whole year still fits inside the period.
    while used + 1.0 <= life + 1e-9 {
        out.push(1.0);
        used += 1.0;
    }
    let remainder = life - used;
    if remainder > 1e-9 {
        out.push(remainder);
    }
    out
}

/// The undiscounted MACRS schedule for a basis, in cents, one entry per tax year
/// of the recovery period.
///
/// Declining balance switches to straight line over the *remaining* period in the
/// first year straight line wins, and never switches back — see the module docs.
/// Straight-line classes take the same path with the comparison skipped.
fn schedule_cents(basis_cents: i64, life: f64, method: Method, fractions: &[f64]) -> Vec<i64> {
    let mut remaining = basis_cents as f64;
    let mut consumed = 0.0;
    let mut switched = false;
    let mut exact: Vec<f64> = Vec::with_capacity(fractions.len());

    for &f in fractions {
        let remaining_life = life - consumed;
        let straight = if remaining_life > 1e-9 {
            remaining / remaining_life * f
        } else {
            remaining
        };

        let amount = match method {
            Method::StraightLine => straight,
            Method::DecliningBalance { factor } => {
                if switched {
                    straight
                } else {
                    let declining = remaining * (factor / life) * f;
                    // The switch happens the first year straight line is at
                    // least as good, and is permanent from there.
                    if straight >= declining {
                        switched = true;
                        straight
                    } else {
                        declining
                    }
                }
            }
        };

        let amount = amount.clamp(0.0, remaining);
        exact.push(amount);
        remaining -= amount;
        consumed += f;
    }

    // Round through the cumulative rather than each year on its own, so the
    // schedule adds up to the basis exactly and no cent is invented or lost.
    let mut out = Vec::with_capacity(exact.len());
    let mut running = 0.0;
    let mut placed = 0i64;
    for a in exact {
        running += a;
        let cumulative = running.round() as i64;
        out.push(cumulative - placed);
        placed = cumulative;
    }
    out
}

/// One asset's deduction for one tax year.
#[derive(Debug, Clone)]
pub struct AssetYear<'a> {
    pub asset: &'a DepreciableAsset,
    /// Which year of the recovery period this is, counting from 1.
    pub recovery_year: u32,
    pub convention: Convention,
    /// §179 elected — first year only, and never part of line 16a.
    pub section_179_cents: i64,
    /// Bonus depreciation — first year only.
    pub bonus_cents: i64,
    pub bonus_rate: f64,
    /// Cost less §179 less bonus: what MACRS actually runs on.
    pub macrs_basis_cents: i64,
    pub macrs_cents: i64,
    /// Everything written off against this asset from the start through the end
    /// of this year, §179 and bonus included. Schedule L line 9b reads this.
    pub accumulated_cents: i64,
    /// Whether the asset went during this year, so the deduction is a part year.
    pub disposed: bool,
}

impl AssetYear<'_> {
    /// The whole year's write-off, §179 included.
    ///
    /// Not what goes on line 16a — [`YearSchedule::line_16a_cents`] excludes the
    /// §179 part, because that is separately stated. This is the figure for a
    /// schedule a person reads.
    pub fn total_cents(&self) -> i64 {
        self.section_179_cents + self.bonus_cents + self.macrs_cents
    }

    /// Cost less everything written off — the asset's remaining tax basis.
    pub fn remaining_basis_cents(&self) -> i64 {
        self.asset.cost_cents - self.accumulated_cents
    }
}

/// A tax year's depreciation across the whole register.
#[derive(Debug, Clone)]
pub struct YearSchedule<'a> {
    pub tax_year: i32,
    pub rows: Vec<AssetYear<'a>>,
    /// The placed-in-service years the mid-quarter convention was forced on.
    pub mid_quarter_years: BTreeSet<i32>,
    pub warnings: Vec<String>,
}

impl YearSchedule<'_> {
    /// Page 1, line 16a — bonus and MACRS, and deliberately **not** §179.
    ///
    /// §179 is on Schedule K line 12 instead, because each partner applies their
    /// own dollar and taxable-income limits to it. Adding it here would deduct at
    /// the partnership level something the statute deducts at the partner level,
    /// and would double-count it against the K-1 box 12 the partner also gets.
    pub fn line_16a_cents(&self) -> i64 {
        self.rows
            .iter()
            .map(|r| r.bonus_cents + r.macrs_cents)
            .sum()
    }

    /// Schedule K line 12, and K-1 box 12: the §179 election, separately stated.
    pub fn section_179_cents(&self) -> i64 {
        self.rows.iter().map(|r| r.section_179_cents).sum()
    }

    /// Everything written off this year, however it is reported. The figure a
    /// person checking the schedule adds up to.
    pub fn total_cents(&self) -> i64 {
        self.rows.iter().map(|r| r.total_cents()).sum()
    }

    pub fn bonus_cents(&self) -> i64 {
        self.rows.iter().map(|r| r.bonus_cents).sum()
    }

    pub fn macrs_cents(&self) -> i64 {
        self.rows.iter().map(|r| r.macrs_cents).sum()
    }

    /// Schedule L line 9a: the gross cost of depreciable assets still held at the
    /// end of the year. Land is not here, and neither is anything disposed of.
    pub fn gross_cost_cents(&self) -> i64 {
        self.rows
            .iter()
            .filter(|r| !r.disposed)
            .map(|r| r.asset.cost_cents)
            .sum()
    }

    /// Schedule L line 9b: accumulated depreciation on the assets in line 9a.
    pub fn accumulated_cents(&self) -> i64 {
        self.rows
            .iter()
            .filter(|r| !r.disposed)
            .map(|r| r.accumulated_cents)
            .sum()
    }

    /// The rows placed in service during this tax year, which is what Form 4562
    /// Part III section B reports and the rest of Part III does not.
    pub fn placed_this_year(&self) -> impl Iterator<Item = &AssetYear<'_>> {
        self.rows.iter().filter(|r| r.recovery_year == 1)
    }
}

/// Everything written off against one asset from the start through the end of
/// `through_year`, §179 and bonus included.
///
/// Used for Schedule L's opening column, which needs the position a year earlier
/// than the one being computed.
fn accumulated_through(asset: &DepreciableAsset, through_year: i32, mid_quarter: bool) -> i64 {
    let Some(last) = asset.recovery_year(through_year) else {
        return 0;
    };
    if asset.placed_and_disposed_same_year() {
        return 0;
    }

    let convention = asset.convention(mid_quarter);
    let (section_179, bonus, macrs_basis) = first_year_splits(asset);

    let life = asset.class.recovery_years(asset.system);
    let fractions = year_fractions(
        first_year_fraction(convention, asset.placed_in_service),
        life,
    );
    let schedule = schedule_cents(
        macrs_basis,
        life,
        asset.class.method(asset.system),
        &fractions,
    );

    let mut total = section_179;
    for year in 1..=last {
        let index = (year - 1) as usize;
        let tax_year = asset.placed_in_service.year() + (year as i32 - 1);
        if asset.disposed_on.is_some_and(|d| d.year() < tax_year) {
            break;
        }
        // A year fixed by hand counts at the figure fixed: the books carry it, and
        // every later year's accumulated depreciation is built on it.
        if let Some(fixed) = asset.overrides.get(&tax_year) {
            total += fixed.amount_cents;
            if asset.disposed_during(tax_year) {
                break;
            }
            continue;
        }
        let Some(&amount) = schedule.get(index) else {
            break;
        };
        let first_year_bonus = if year == 1 { bonus } else { 0 };
        // A disposal year is a part year, on the same convention that opened the
        // asset; after it there is nothing left to take.
        let (amount, last_year) = match asset.disposed_on.filter(|d| d.year() == tax_year) {
            Some(disposed) => {
                let fraction = disposal_year_fraction(convention, disposed);
                ((amount as f64 * fraction).round() as i64, true)
            }
            None => (amount, false),
        };
        // Never past the basis — the same cap `compute_year` applies, so the two
        // cannot disagree about a year after one fixed above the table.
        total +=
            first_year_bonus + amount.min((asset.cost_cents - total - first_year_bonus).max(0));
        if last_year {
            break;
        }
    }
    total.min(asset.cost_cents)
}

/// §179, bonus and the basis MACRS is left with — in the statutory order.
///
/// §179 comes off the cost, bonus takes its rate of what remains, and MACRS runs
/// on the rest. Each step is clamped so an over-large §179 election cannot drive
/// the basis negative.
fn first_year_splits(asset: &DepreciableAsset) -> (i64, i64, i64) {
    let section_179 = asset.section_179_cents.clamp(0, asset.cost_cents);
    let after_179 = asset.cost_cents - section_179;
    let bonus = (after_179 as f64 * bonus_rate(asset)).round() as i64;
    let bonus = bonus.clamp(0, after_179);
    (section_179, bonus, after_179 - bonus)
}

/// Compute a tax year's depreciation over the whole register.
///
/// The mid-quarter test is run per placed-in-service year rather than once,
/// because an asset from 2023 keeps whatever convention 2023 forced on it for the
/// rest of its life. A register holding assets from several years can quite
/// properly have some on mid-quarter and some on half-year at the same time.
pub fn compute_year<'a>(assets: &'a [DepreciableAsset], tax_year: i32) -> YearSchedule<'a> {
    // Which placed-in-service years were mid-quarter years. Computed over the
    // whole register, since the test is about a year's acquisitions as a group.
    let mut mid_quarter_years = BTreeSet::new();
    let years: BTreeSet<i32> = assets.iter().map(|a| a.placed_in_service.year()).collect();
    for year in years {
        if mid_quarter_applies(assets, year) {
            mid_quarter_years.insert(year);
        }
    }

    let mut rows = Vec::new();
    let mut warnings = Vec::new();

    for asset in assets {
        if !asset.held_during(tax_year) {
            continue;
        }
        let Some(recovery_year) = asset.recovery_year(tax_year) else {
            continue;
        };

        let mid_quarter = mid_quarter_years.contains(&asset.placed_in_service.year());
        let convention = asset.convention(mid_quarter);

        // Placed in service and gone in the same year: no deduction at all. The
        // asset still appears, at zero, because a schedule that simply omitted it
        // would look like one where it was never entered.
        if asset.placed_and_disposed_same_year() {
            rows.push(AssetYear {
                asset,
                recovery_year,
                convention,
                section_179_cents: 0,
                bonus_cents: 0,
                bonus_rate: 0.0,
                macrs_basis_cents: 0,
                macrs_cents: 0,
                accumulated_cents: 0,
                disposed: true,
            });
            warnings.push(format!(
                "{}: placed in service and disposed of in the same tax year, so no depreciation \
                 is allowable on it at all. Its basis is recovered through the gain or loss on \
                 the disposal instead.",
                asset.description
            ));
            continue;
        }

        let (section_179, bonus, macrs_basis) = first_year_splits(asset);
        let life = asset.class.recovery_years(asset.system);
        let fractions = year_fractions(
            first_year_fraction(convention, asset.placed_in_service),
            life,
        );
        let schedule = schedule_cents(
            macrs_basis,
            life,
            asset.class.method(asset.system),
            &fractions,
        );

        let mut macrs = schedule
            .get((recovery_year - 1) as usize)
            .copied()
            .unwrap_or(0);

        let disposed = asset.disposed_during(tax_year);
        if disposed {
            let fraction = disposal_year_fraction(
                convention,
                asset.disposed_on.expect("disposed_during implies a date"),
            );
            macrs = (macrs as f64 * fraction).round() as i64;
        }

        // First-year-only deductions.
        let (section_179, bonus, bonus_rate_used) = if recovery_year == 1 {
            (section_179, bonus, bonus_rate(asset))
        } else {
            (0, 0, 0.0)
        };

        // Never past the basis. The table recovers exactly the basis over the
        // life, so this only bites after a year fixed above the table — and then
        // it is what stops the asset deducting more than it cost.
        let already = accumulated_through(asset, tax_year - 1, mid_quarter);
        macrs = macrs.min((asset.cost_cents - already - section_179 - bonus).max(0));

        // A year fixed by hand replaces bonus and MACRS with the figure fixed, and
        // says so wherever the schedule's warnings are read — the return included.
        let (bonus, macrs) = match asset.overrides.get(&tax_year) {
            Some(fixed) => {
                warnings.push(format!(
                    "{}: {tax_year} depreciation is fixed by hand at ${:.2}; the register computes                      ${:.2}. Reason given: {}",
                    asset.description,
                    fixed.amount_cents as f64 / 100.0,
                    (bonus + macrs) as f64 / 100.0,
                    fixed.note
                ));
                let kept_bonus = bonus.min(fixed.amount_cents);
                (kept_bonus, fixed.amount_cents - kept_bonus)
            }
            None => (bonus, macrs),
        };

        rows.push(AssetYear {
            asset,
            recovery_year,
            convention,
            section_179_cents: section_179,
            bonus_cents: bonus,
            bonus_rate: bonus_rate_used,
            macrs_basis_cents: macrs_basis,
            macrs_cents: macrs,
            accumulated_cents: accumulated_through(asset, tax_year, mid_quarter),
            disposed,
        });
    }

    warnings.extend(election_warnings(assets, tax_year, &rows));

    YearSchedule {
        tax_year,
        rows,
        mid_quarter_years,
        warnings,
    }
}

/// Accumulated depreciation across the register at the end of `through_year` —
/// Schedule L line 9b, for either column.
pub fn accumulated_at(assets: &[DepreciableAsset], through_year: i32) -> i64 {
    let mut total = 0;
    for asset in assets {
        // An asset gone before the date is off the balance sheet, along with its
        // accumulated depreciation — both sides leave together.
        if asset.disposed_on.is_some_and(|d| d.year() <= through_year) {
            continue;
        }
        if !asset.held_during(through_year) {
            continue;
        }
        let mid_quarter = mid_quarter_applies(assets, asset.placed_in_service.year());
        total += accumulated_through(asset, through_year, mid_quarter);
    }
    total
}

/// Gross cost of depreciable assets held at the end of `through_year` —
/// Schedule L line 9a.
pub fn gross_cost_at(assets: &[DepreciableAsset], through_year: i32) -> i64 {
    assets
        .iter()
        .filter(|a| a.placed_in_service.year() <= through_year)
        .filter(|a| !a.disposed_on.is_some_and(|d| d.year() <= through_year))
        .map(|a| a.cost_cents)
        .sum()
}

/// Warnings about elections that are wrong in ways the arithmetic cannot refuse.
///
/// These are all cases where a figure can be computed but should not be filed,
/// so refusing would block a return over a judgement that is the filer's to make,
/// and staying silent would file it.
fn election_warnings(
    assets: &[DepreciableAsset],
    tax_year: i32,
    rows: &[AssetYear<'_>],
) -> Vec<String> {
    let mut out = Vec::new();

    for row in rows {
        let asset = row.asset;

        // §179 on a class that cannot take it, or can take it only sometimes.
        if asset.section_179_cents > 0 {
            match asset.class.section_179() {
                Section179Eligibility::Eligible => {}
                Section179Eligibility::NotEligible => out.push(format!(
                    "{}: §179 of {} is elected on {} property, which is not eligible for the \
                     election. Remove it, or the return claims a deduction the statute does not \
                     allow.",
                    asset.description,
                    dollars(asset.section_179_cents),
                    asset.class.label()
                )),
                Section179Eligibility::Conditional(why) => out.push(format!(
                    "{}: §179 of {} is elected on {} property. {} Check this asset is one of \
                     them — nothing in the books can.",
                    asset.description,
                    dollars(asset.section_179_cents),
                    asset.class.label(),
                    why
                )),
            }
            if asset.section_179_cents > asset.cost_cents {
                out.push(format!(
                    "{}: §179 of {} is more than the asset cost ({}), so it has been capped at \
                     cost.",
                    asset.description,
                    dollars(asset.section_179_cents),
                    dollars(asset.cost_cents)
                ));
            }
        }

        // Bonus elected on a class that cannot take it. Not an error the
        // arithmetic notices, because `bonus_rate` already returns zero — which
        // is exactly why it needs saying.
        if asset.bonus == BonusElection::Take
            && !asset.class.bonus_eligible()
            && row.recovery_year == 1
        {
            out.push(format!(
                "{}: bonus depreciation is switched on but {} property has a recovery period \
                 over 20 years, so §168(k) does not reach it and none was taken. If this is a \
                 leasehold improvement, check whether it is qualified improvement property — \
                 that is 15-year property and bonus-eligible.",
                asset.description,
                asset.class.label()
            ));
        }
    }

    // §168(k)(7) is elected per class per year, not per asset. A year where one
    // 5-year asset takes bonus and another declines is not an election the
    // statute offers, and the return would carry a position that cannot be made.
    let mut by_class: BTreeMap<(PropertyClass, System), (bool, bool)> = BTreeMap::new();
    for asset in assets {
        if asset.placed_in_service.year() != tax_year || !asset.class.bonus_eligible() {
            continue;
        }
        let entry = by_class.entry((asset.class, asset.system)).or_default();
        match asset.bonus {
            BonusElection::Take => entry.0 = true,
            BonusElection::Decline => entry.1 = true,
        }
    }
    for ((class, _), (takes, declines)) in by_class {
        if takes && declines {
            out.push(format!(
                "{} property placed in service in {tax_year} has some assets taking bonus \
                 depreciation and others electing out. §168(k)(7) makes that election for the \
                 whole class for the year, not asset by asset — so pick one for every asset in \
                 the class, or the return claims an election that was never validly made.",
                class.label()
            ));
        }
    }

    out
}

/// Where the register and the ledger disagree about the same figure.
///
/// # Why this check has to exist at all
///
/// The register does not fill line 16a. It produces a journal entry, and the
/// ordinary account-to-tax-line mapping fills line 16a from the ledger — which is
/// the whole design, and the reason the books and the return cannot drift apart
/// silently. But that only holds while the entry is posted and current. Three
/// things break it, all of them ordinary:
///
/// - the year has not been posted yet, so the register computes a deduction the
///   ledger has never heard of;
/// - the register was edited after posting, so the entry is stale;
/// - nobody mapped the depreciation account to line 16a, so the entry exists and
///   reaches nothing.
///
/// In every one of them the return is internally consistent and quietly wrong,
/// which is exactly the shape of error a person does not find by reading the
/// form. So the two are compared on every build, in dollars, because dollars are
/// what the return is filed in and a cent of rounding is not a disagreement.
///
/// Reported rather than corrected. Posting rewrites somebody's books, the entry
/// may sit in a closed period, and neither is a thing to do without being asked.
pub fn reconcile_with_ledger(
    schedule: &YearSchedule<'_>,
    lines: &super::lines::Form1065Lines,
    schedule_l: Option<&super::schedule_l::ScheduleL>,
) -> Vec<String> {
    use super::lines::cents_to_dollars;

    let mut out = Vec::new();
    if schedule.rows.is_empty() {
        return out;
    }

    let year = schedule.tax_year;

    // --- page 1 line 16a, and Schedule K line 12 ---
    for (key, where_, what, register) in [
        (
            "l16a",
            "page 1 line 16a",
            "depreciation",
            schedule.line_16a_cents(),
        ),
        (
            "k12",
            "Schedule K line 12",
            "§179",
            schedule.section_179_cents(),
        ),
    ] {
        let register_dollars = cents_to_dollars(register);
        let ledger = lines.get(key);

        if !lines.is_mapped(key) {
            if register != 0 {
                out.push(format!(
                    "The register computes {} of {what} for {year}, but no account is mapped to {where_}, so none of it reaches the return. Post {year}'s depreciation to the ledger and map the account it is posted to.",
                    dollars(register)
                ));
            }
            continue;
        }
        if ledger != register_dollars {
            out.push(format!(
                "{where_} carries ${ledger} from the ledger, and the register computes ${register_dollars} of {what} for {year}. Either {year} has not been posted since the register last changed, or something else is mapped to {where_}. The return is filed on the ledger figure."
            ));
        }
    }

    // --- Schedule L lines 9a and 9b ---
    //
    // Checked against the register's own view of the balance sheet, which is the
    // only thing that knows an asset was disposed of: the cost and the
    // accumulated depreciation leave together, and a ledger that kept either
    // shows a studio still holding a kiln it sold.
    let Some(l) = schedule_l else { return out };
    for (key, where_, what, register) in [
        (
            "sl9a",
            "Schedule L line 9a",
            "the cost of the assets on the register",
            schedule.gross_cost_cents(),
        ),
        (
            "sl9b",
            "Schedule L line 9b",
            "accumulated depreciation per the register",
            schedule.accumulated_cents(),
        ),
    ] {
        if !l.is_mapped(key) {
            if register != 0 {
                out.push(format!(
                    "{where_} has no account mapped to it, and {what} is {} at the end of {year}. The balance sheet will be short of it.",
                    dollars(register)
                ));
            }
            continue;
        }
        let ledger = l.get(key).end;
        let register_dollars = cents_to_dollars(register);
        if ledger != register_dollars {
            out.push(format!(
                "{where_} closes at ${ledger} per the books, and {what} is ${register_dollars}. A difference here is usually an asset bought and expensed rather than capitalised, or one disposed of in the register and still on the books."
            ));
        }
    }

    out
}

/// Cents as a dollar figure, for a warning a person reads.
fn dollars(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, (cents % 100).abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn asset(class: PropertyClass, placed: NaiveDate, cost: i64) -> DepreciableAsset {
        DepreciableAsset {
            asset_id: format!("{class:?}-{placed}"),
            description: format!("{} asset", class.label()),
            asset_account_id: "1500".into(),
            expense_account_id: "6500".into(),
            accumulated_account_id: "1590".into(),
            section_179_account_id: Some("6501".into()),
            acquired_on: placed,
            placed_in_service: placed,
            cost_cents: cost,
            class,
            system: System::Gds,
            section_179_cents: 0,
            // Bonus off by default in these tests, so the MACRS schedule itself
            // is what is being checked rather than a 100% write-off in year one.
            bonus: BonusElection::Decline,
            disposed_on: None,
            notes: None,
            overrides: Default::default(),
        }
    }

    /// The whole schedule as percentages of basis, to two decimals — the shape
    /// the IRS tables are published in, so they can be compared directly.
    fn percentages(class: PropertyClass, convention: Convention, month: u32) -> Vec<f64> {
        let life = class.recovery_years(System::Gds);
        let placed = date(2025, month, 15);
        let fractions = year_fractions(first_year_fraction(convention, placed), life);
        schedule_cents(100_000_000, life, class.method(System::Gds), &fractions)
            .into_iter()
            .map(|c| (c as f64 / 1_000_000.0 * 100.0).round() / 100.0)
            .collect()
    }

    // --- against the published tables ---

    /// The computed schedule against a published column, to within the hundredth
    /// of a point the IRS's own rounding moves — see the module docs. The
    /// tolerance is what the tables force; it is far tighter than any real error,
    /// since a wrong rate, a missed convention or a missing straight-line switch
    /// all move a year by whole points.
    fn assert_matches_table(got: &[f64], table: &[f64], what: &str) {
        assert_eq!(
            got.len(),
            table.len(),
            "{what}: wrong number of years — {got:?}"
        );
        for (year, (g, t)) in got.iter().zip(table).enumerate() {
            assert!(
                (g - t).abs() <= 0.011,
                "{what}: year {} computed {g}, table {t}",
                year + 1
            );
        }
        // The property the IRS bought with that rounding: the column recovers
        // the basis. Asserted here on the published figures, and separately on
        // the computed cents in `every_class_recovers_the_whole_basis_and_no_more`.
        let total: f64 = table.iter().sum();
        assert!(
            (total - 100.0).abs() < 1e-9,
            "{what}: table sums to {total}"
        );
    }

    /// Pub. 946 Table A-1, 5-year property, half-year convention.
    #[test]
    fn five_year_half_year_reproduces_table_a1() {
        assert_matches_table(
            &percentages(PropertyClass::FiveYear, Convention::HalfYear, 6),
            &[20.00, 32.00, 19.20, 11.52, 11.52, 5.76],
            "5-year half-year",
        );
    }

    /// Pub. 946 Table A-1, 7-year property — the class a kiln or a set of easels
    /// falls in, and the one with the visible declining-balance switch at year 5.
    /// Also the column where the IRS's rounding is most obvious: years five to
    /// seven are one number, printed as three.
    #[test]
    fn seven_year_half_year_reproduces_table_a1() {
        assert_matches_table(
            &percentages(PropertyClass::SevenYear, Convention::HalfYear, 6),
            &[14.29, 24.49, 17.49, 12.49, 8.93, 8.92, 8.93, 4.46],
            "7-year half-year",
        );
    }

    /// Pub. 946 Table A-1, 3-year property.
    #[test]
    fn three_year_half_year_reproduces_table_a1() {
        assert_matches_table(
            &percentages(PropertyClass::ThreeYear, Convention::HalfYear, 6),
            &[33.33, 44.45, 14.81, 7.41],
            "3-year half-year",
        );
    }

    /// Pub. 946 Table A-1, 15-year land improvements — 150% declining balance,
    /// the column that shares a life with qualified improvement property and
    /// nothing else.
    #[test]
    fn fifteen_year_land_improvements_reproduce_table_a1() {
        assert_matches_table(
            &percentages(
                PropertyClass::FifteenYearLandImprovement,
                Convention::HalfYear,
                6,
            ),
            &[
                5.00, 9.50, 8.55, 7.70, 6.93, 6.23, 5.90, 5.90, 5.91, 5.90, 5.91, 5.90, 5.91, 5.90,
                5.91, 2.95,
            ],
            "15-year land improvement",
        );
    }

    /// Pub. 946 Table A-5, 5-year property placed in service in the fourth
    /// quarter — the mid-quarter table, where year one collapses to 5%.
    #[test]
    fn five_year_fourth_quarter_reproduces_table_a5() {
        assert_matches_table(
            &percentages(PropertyClass::FiveYear, Convention::MidQuarter, 11),
            &[5.00, 38.00, 22.80, 13.68, 10.94, 9.58],
            "5-year mid-quarter Q4",
        );
    }

    /// Pub. 946 Table A-2, 5-year property placed in service in the first
    /// quarter — the other end of the mid-quarter range, where year one is 35%.
    #[test]
    fn five_year_first_quarter_reproduces_table_a2() {
        assert_matches_table(
            &percentages(PropertyClass::FiveYear, Convention::MidQuarter, 2),
            &[35.00, 26.00, 15.60, 11.01, 11.01, 1.38],
            "5-year mid-quarter Q1",
        );
    }

    /// The point of the whole model. Qualified improvement property shares the
    /// 15 years and takes straight line, so its first year is 3.33% where the
    /// parking lot's is 5.00% — and it stays flatter for the rest of the period.
    #[test]
    fn qualified_improvement_property_is_straight_line_over_the_same_fifteen_years() {
        let qip = percentages(PropertyClass::QualifiedImprovement, Convention::HalfYear, 6);
        let lot = percentages(
            PropertyClass::FifteenYearLandImprovement,
            Convention::HalfYear,
            6,
        );

        assert_eq!(qip[0], 3.33, "half of 1/15");
        assert_eq!(qip[1], 6.67, "a full year of 1/15");
        assert_eq!(lot[0], 5.00);
        assert_ne!(qip, lot, "same life, different schedule");
        assert_eq!(qip.len(), 16);
    }

    /// Mid-month real property: the last month of the year leaves half a month
    /// of the first year, which is the one figure in the 39-year table that is
    /// unambiguous enough to assert. The rest is checked structurally.
    #[test]
    fn thirty_nine_year_property_is_straight_line_mid_month() {
        let december = percentages(PropertyClass::Nonresidential, Convention::MidMonth, 12);
        assert_eq!(december[0], 0.11, "half a month of 1/39");

        let january = percentages(PropertyClass::Nonresidential, Convention::MidMonth, 1);
        assert_eq!(january.len(), 40, "39-year property spans 40 tax years");
        // Every middle year is a full year at 1/39, and the two ends split one
        // between them — 11.5 months and then half a month.
        assert_eq!(january[1], 2.56);
        assert!(january.iter().skip(1).take(38).all(|&p| p == 2.56));
        assert!(
            (january[0] + january[39] - 2.56).abs() <= 0.02,
            "the two part years make one whole one: {} + {}",
            january[0],
            january[39]
        );

        // That the basis is recovered exactly is asserted on the cents, not on
        // these percentages: forty figures rounded to a hundredth can drift a
        // fifth of a point in total, which says nothing about the schedule.
        // `every_class_recovers_the_whole_basis_and_no_more` is the real check.
    }

    /// Every class recovers exactly its basis and not a cent more.
    #[test]
    fn every_class_recovers_the_whole_basis_and_no_more() {
        for class in PropertyClass::ALL {
            for system in [System::Gds, System::Ads] {
                let life = class.recovery_years(system);
                let convention = if class.uses_mid_month() {
                    Convention::MidMonth
                } else {
                    Convention::HalfYear
                };
                let fractions =
                    year_fractions(first_year_fraction(convention, date(2025, 6, 15)), life);
                let schedule = schedule_cents(1_234_567, life, class.method(system), &fractions);
                let total: i64 = schedule.iter().sum();
                assert_eq!(total, 1_234_567, "{class:?} under {system:?}");
                assert!(
                    schedule.iter().all(|&c| c >= 0),
                    "{class:?} has a negative year"
                );
            }
        }
    }

    // --- the mid-quarter test ---

    #[test]
    fn late_purchases_over_forty_percent_force_mid_quarter_on_the_whole_year() {
        let early = asset(PropertyClass::FiveYear, date(2025, 2, 1), 100_000);
        let late = asset(PropertyClass::SevenYear, date(2025, 11, 1), 200_000);
        let assets = vec![early, late];

        assert!(
            mid_quarter_applies(&assets, 2025),
            "200k of 300k is over 40%"
        );

        // And it reaches the asset that arrived in February, not only the one
        // that arrived in November.
        let s = compute_year(&assets, 2025);
        for row in &s.rows {
            assert_eq!(
                row.convention,
                Convention::MidQuarter,
                "{}",
                row.asset.description
            );
        }
    }

    #[test]
    fn under_the_threshold_leaves_everything_on_half_year() {
        let early = asset(PropertyClass::FiveYear, date(2025, 2, 1), 300_000);
        let late = asset(PropertyClass::SevenYear, date(2025, 11, 1), 100_000);
        let assets = vec![early, late];

        assert!(!mid_quarter_applies(&assets, 2025));
        let s = compute_year(&assets, 2025);
        assert!(s.rows.iter().all(|r| r.convention == Convention::HalfYear));
    }

    /// Real property is outside the test on both sides: a building bought in
    /// December neither counts toward the 40% nor moves off mid-month.
    #[test]
    fn a_building_bought_in_december_cannot_force_mid_quarter_on_the_equipment() {
        let equipment = asset(PropertyClass::FiveYear, date(2025, 2, 1), 100_000);
        let building = asset(PropertyClass::Nonresidential, date(2025, 12, 1), 9_000_000);
        let assets = vec![equipment, building];

        assert!(
            !mid_quarter_applies(&assets, 2025),
            "the building is not personal property and is not in the fraction"
        );
        let s = compute_year(&assets, 2025);
        let b = s
            .rows
            .iter()
            .find(|r| r.asset.class == PropertyClass::Nonresidential)
            .unwrap();
        assert_eq!(b.convention, Convention::MidMonth);
    }

    /// §179 reduces the basis the 40% test is measured on; bonus does not. Here
    /// the late asset is over the line on cost and under it after its election.
    #[test]
    fn section_179_comes_off_the_basis_the_mid_quarter_test_measures() {
        let early = asset(PropertyClass::FiveYear, date(2025, 2, 1), 100_000);
        let mut late = asset(PropertyClass::FiveYear, date(2025, 11, 1), 100_000);

        let before = vec![early.clone(), late.clone()];
        assert!(
            mid_quarter_applies(&before, 2025),
            "100k of 200k is 50%, over the 40% line"
        );

        late.section_179_cents = 60_000;
        let after = vec![early, late];
        assert!(
            !mid_quarter_applies(&after, 2025),
            "40k of 140k is under 40%"
        );
    }

    /// An asset that arrived and left in the same year is out of the test, so it
    /// cannot drag the year's other property onto mid-quarter behind it.
    #[test]
    fn an_asset_that_came_and_went_is_left_out_of_the_test() {
        let kept = asset(PropertyClass::FiveYear, date(2025, 2, 1), 100_000);
        let mut fleeting = asset(PropertyClass::FiveYear, date(2025, 11, 1), 900_000);
        fleeting.disposed_on = Some(date(2025, 12, 1));

        let assets = vec![kept, fleeting];
        assert!(!mid_quarter_applies(&assets, 2025));
    }

    /// An asset keeps the convention its own placed-in-service year forced, for
    /// the rest of its life — so one register can hold both at once.
    #[test]
    fn the_convention_is_fixed_by_the_year_the_asset_arrived() {
        // 2024 is a mid-quarter year; 2025 is not.
        let mut old = asset(PropertyClass::FiveYear, date(2024, 11, 1), 100_000);
        old.asset_id = "old".into();
        let mut new = asset(PropertyClass::FiveYear, date(2025, 3, 1), 100_000);
        new.asset_id = "new".into();
        let assets = vec![old, new];

        let s = compute_year(&assets, 2025);
        let old_row = s.rows.iter().find(|r| r.asset.asset_id == "old").unwrap();
        let new_row = s.rows.iter().find(|r| r.asset.asset_id == "new").unwrap();
        assert_eq!(old_row.convention, Convention::MidQuarter);
        assert_eq!(new_row.convention, Convention::HalfYear);
    }

    // --- the order of the three deductions ---

    /// §179 first, bonus on what is left, MACRS on what is left after that.
    /// Any other order overstates the year.
    #[test]
    fn the_three_deductions_come_in_the_statutory_order() {
        let mut a = asset(PropertyClass::FiveYear, date(2025, 6, 1), 1_000_000);
        a.acquired_on = date(2025, 6, 1); // after 19 Jan 2025, so 100% bonus
        a.section_179_cents = 400_000;
        a.bonus = BonusElection::Take;

        let assets = [a];
        let s = compute_year(&assets, 2025);
        let row = &s.rows[0];

        assert_eq!(row.section_179_cents, 400_000);
        assert_eq!(row.bonus_cents, 600_000, "100% of the 600k left after §179");
        assert_eq!(row.macrs_basis_cents, 0);
        assert_eq!(row.macrs_cents, 0);
        assert_eq!(row.total_cents(), 1_000_000);
    }

    /// §179 never reaches line 16a — it is separately stated on Schedule K line
    /// 12, because the partner applies their own limits to it.
    #[test]
    fn section_179_is_kept_out_of_line_16a() {
        let mut a = asset(PropertyClass::FiveYear, date(2025, 6, 1), 1_000_000);
        a.section_179_cents = 400_000;
        a.bonus = BonusElection::Decline;

        let assets = [a];
        let s = compute_year(&assets, 2025);
        assert_eq!(s.section_179_cents(), 400_000);
        // 20% of the 600k left, half-year convention already in the rate.
        assert_eq!(s.macrs_cents(), 120_000);
        assert_eq!(s.line_16a_cents(), 120_000, "the §179 is not in here");
        assert_eq!(s.total_cents(), 520_000);
    }

    // --- bonus rates ---

    /// The 2025 split year. Same asset, same in-service date, two acquisition
    /// dates a week apart, and 60% of the cost between them.
    #[test]
    fn the_2025_split_year_turns_on_the_acquisition_date() {
        let mut before = asset(PropertyClass::FiveYear, date(2025, 6, 1), 1_000_000);
        before.acquired_on = date(2025, 1, 15);
        before.bonus = BonusElection::Take;

        let mut after = asset(PropertyClass::FiveYear, date(2025, 6, 1), 1_000_000);
        after.acquired_on = date(2025, 1, 22);
        after.bonus = BonusElection::Take;

        assert_eq!(bonus_rate(&before), 0.40);
        assert_eq!(bonus_rate(&after), 1.00);
    }

    /// Acquired in 2025 after the restoration, first used in 2026: 100%, because
    /// the restoration is keyed to acquisition and the phase-down never applies.
    #[test]
    fn acquisition_after_the_restoration_carries_its_rate_into_a_later_year() {
        let mut a = asset(PropertyClass::FiveYear, date(2026, 3, 1), 1_000_000);
        a.acquired_on = date(2025, 3, 1);
        a.bonus = BonusElection::Take;
        assert_eq!(
            bonus_rate(&a),
            1.00,
            "not the 20% 2026 would otherwise give"
        );
    }

    #[test]
    fn the_phase_down_applies_to_property_acquired_before_the_restoration() {
        for (pis_year, rate) in [(2022, 1.00), (2023, 0.80), (2024, 0.60), (2027, 0.0)] {
            let mut a = asset(PropertyClass::FiveYear, date(pis_year, 6, 1), 1_000_000);
            a.acquired_on = date(2021, 1, 1);
            a.bonus = BonusElection::Take;
            assert_eq!(bonus_rate(&a), rate, "placed in service {pis_year}");
        }
    }

    /// A 39-year building takes no bonus however it is flagged, and says so
    /// rather than silently taking none.
    #[test]
    fn bonus_on_a_building_is_refused_and_reported() {
        let mut a = asset(PropertyClass::Nonresidential, date(2025, 6, 1), 10_000_000);
        a.acquired_on = date(2025, 6, 1);
        a.bonus = BonusElection::Take;

        let assets = [a];
        let s = compute_year(&assets, 2025);
        assert_eq!(s.bonus_cents(), 0);
        assert!(
            s.warnings
                .iter()
                .any(|w| w.contains("recovery period")
                    && w.contains("qualified improvement property")),
            "{:?}",
            s.warnings
        );
    }

    /// The same improvement classed as QIP does take bonus — the difference the
    /// leasehold question decides.
    #[test]
    fn the_same_improvement_as_qip_takes_the_whole_hundred_percent() {
        let mut a = asset(
            PropertyClass::QualifiedImprovement,
            date(2025, 6, 1),
            10_000_000,
        );
        a.acquired_on = date(2025, 6, 1);
        a.bonus = BonusElection::Take;

        let assets = [a];
        let s = compute_year(&assets, 2025);
        assert_eq!(s.bonus_cents(), 10_000_000);
        assert_eq!(s.line_16a_cents(), 10_000_000);
    }

    // --- elections that need a person ---

    #[test]
    fn section_179_on_a_class_that_cannot_take_it_is_reported() {
        let mut a = asset(
            PropertyClass::FifteenYearLandImprovement,
            date(2025, 6, 1),
            500_000,
        );
        a.section_179_cents = 100_000;

        let assets = [a];
        let s = compute_year(&assets, 2025);
        assert!(
            s.warnings.iter().any(|w| w.contains("not eligible")),
            "{:?}",
            s.warnings
        );
    }

    #[test]
    fn section_179_on_a_building_is_allowed_with_the_condition_quoted() {
        let mut a = asset(PropertyClass::Nonresidential, date(2025, 6, 1), 500_000);
        a.section_179_cents = 100_000;

        let assets = [a];
        let s = compute_year(&assets, 2025);
        assert_eq!(s.section_179_cents(), 100_000, "allowed, not refused");
        assert!(
            s.warnings
                .iter()
                .any(|w| w.contains("§179(f)") && w.contains("security systems")),
            "{:?}",
            s.warnings
        );
    }

    /// §168(k)(7) is a class-wide election. Splitting it across two assets of one
    /// class in one year is a position that cannot be taken.
    #[test]
    fn a_class_split_between_taking_and_declining_bonus_is_reported() {
        let mut takes = asset(PropertyClass::FiveYear, date(2025, 2, 1), 100_000);
        takes.asset_id = "takes".into();
        takes.bonus = BonusElection::Take;
        let mut declines = asset(PropertyClass::FiveYear, date(2025, 3, 1), 100_000);
        declines.asset_id = "declines".into();
        declines.bonus = BonusElection::Decline;

        let assets = [takes, declines];
        let s = compute_year(&assets, 2025);
        assert!(
            s.warnings.iter().any(|w| w.contains("§168(k)(7)")),
            "{:?}",
            s.warnings
        );
    }

    /// Two different classes disagreeing is perfectly proper — the election is
    /// per class, so this must not warn.
    #[test]
    fn two_different_classes_may_disagree_about_bonus() {
        let mut five = asset(PropertyClass::FiveYear, date(2025, 2, 1), 100_000);
        five.bonus = BonusElection::Take;
        let mut seven = asset(PropertyClass::SevenYear, date(2025, 3, 1), 100_000);
        seven.bonus = BonusElection::Decline;

        let assets = [five, seven];
        let s = compute_year(&assets, 2025);
        assert!(
            !s.warnings.iter().any(|w| w.contains("§168(k)(7)")),
            "{:?}",
            s.warnings
        );
    }

    // --- disposals ---

    #[test]
    fn a_disposal_takes_half_a_year_under_the_half_year_convention() {
        let mut a = asset(PropertyClass::FiveYear, date(2023, 6, 1), 1_000_000);
        a.disposed_on = Some(date(2025, 8, 1));

        let assets = [a];
        let s = compute_year(&assets, 2025);
        let row = &s.rows[0];
        assert!(row.disposed);
        // Year 3 of a 5-year asset is 19.20%; half of it on disposal.
        assert_eq!(row.macrs_cents, 96_000);
        assert_eq!(s.gross_cost_cents(), 0, "off the balance sheet at year end");
        assert_eq!(s.accumulated_cents(), 0);
    }

    #[test]
    fn an_asset_placed_and_disposed_in_one_year_takes_nothing_and_says_so() {
        let mut a = asset(PropertyClass::FiveYear, date(2025, 2, 1), 1_000_000);
        a.disposed_on = Some(date(2025, 9, 1));

        let assets = [a];
        let s = compute_year(&assets, 2025);
        assert_eq!(s.total_cents(), 0);
        assert!(
            s.warnings
                .iter()
                .any(|w| w.contains("no depreciation is allowable")),
            "{:?}",
            s.warnings
        );
    }

    #[test]
    fn a_disposed_asset_is_gone_from_the_year_after() {
        let mut a = asset(PropertyClass::FiveYear, date(2023, 6, 1), 1_000_000);
        a.disposed_on = Some(date(2025, 8, 1));

        let assets = [a];
        let s = compute_year(&assets, 2026);
        assert!(s.rows.is_empty());
        assert_eq!(gross_cost_at(&assets, 2026), 0);
    }

    // --- the balance sheet ---

    /// Schedule L reads gross cost against accumulated depreciation, and the two
    /// have to move together as the years pass.
    #[test]
    fn accumulated_depreciation_builds_to_the_whole_cost_over_the_period() {
        let a = asset(PropertyClass::FiveYear, date(2025, 6, 1), 1_000_000);
        let assets = [a];

        assert_eq!(gross_cost_at(&assets, 2025), 1_000_000, "cost never moves");
        assert_eq!(accumulated_at(&assets, 2025), 200_000);
        assert_eq!(accumulated_at(&assets, 2026), 520_000);
        assert_eq!(accumulated_at(&assets, 2030), 1_000_000, "fully recovered");
        assert_eq!(
            accumulated_at(&assets, 2035),
            1_000_000,
            "and never more than cost"
        );
    }

    /// The register's own view of accumulated depreciation and the year rows'
    /// have to agree, or Schedule L 9b and the schedule behind it disagree.
    #[test]
    fn the_two_routes_to_accumulated_depreciation_agree() {
        let mut a = asset(PropertyClass::SevenYear, date(2023, 3, 1), 1_400_000);
        a.section_179_cents = 200_000;
        a.bonus = BonusElection::Take;
        let assets = [a];

        for year in 2023..=2027 {
            let s = compute_year(&assets, year);
            assert_eq!(
                s.accumulated_cents(),
                accumulated_at(&assets, year),
                "year {year}"
            );
        }
    }

    /// §179 and bonus are part of accumulated depreciation, not something beside
    /// it — the asset's basis is gone whichever provision took it.
    #[test]
    fn the_first_year_write_offs_are_part_of_accumulated_depreciation() {
        let mut a = asset(PropertyClass::FiveYear, date(2025, 6, 1), 1_000_000);
        a.acquired_on = date(2025, 6, 1);
        a.bonus = BonusElection::Take;

        let assets = [a];
        let s = compute_year(&assets, 2025);
        assert_eq!(s.accumulated_cents(), 1_000_000);
        assert_eq!(s.rows[0].remaining_basis_cents(), 0);
    }

    /// A year fixed by hand is the year's figure, later years build on it, and
    /// the asset still recovers exactly its cost — the case this exists for: a
    /// 2023 return filed at $1,448 on a fit-out the tables put at $667.
    #[test]
    fn an_override_replaces_the_year_and_the_life_still_recovers_the_cost() {
        let placed = NaiveDate::from_ymd_opt(2023, 10, 19).unwrap();
        let mut fitout = asset(PropertyClass::Nonresidential, placed, 12_481_600);
        let plain = vec![fitout.clone()];
        fitout.overrides.insert(
            2023,
            crate::domain::DepreciationOverride {
                amount_cents: 144_800,
                note: "As filed: 39.5-year life".into(),
            },
        );
        let fixed = vec![fitout];

        let y2023 = compute_year(&fixed, 2023);
        assert_eq!(y2023.rows[0].macrs_cents, 144_800);
        assert_eq!(y2023.rows[0].accumulated_cents, 144_800);
        assert!(
            y2023
                .warnings
                .iter()
                .any(|w| w.contains("As filed: 39.5-year life")),
            "{:?}",
            y2023.warnings
        );

        let y2024 = compute_year(&fixed, 2024);
        assert_eq!(
            y2024.rows[0].macrs_cents,
            compute_year(&plain, 2024).rows[0].macrs_cents,
            "2024 is the table's figure"
        );
        assert_eq!(
            y2024.rows[0].accumulated_cents,
            144_800 + y2024.rows[0].macrs_cents
        );

        let life: i64 = (2023..=2064)
            .map(|y| {
                compute_year(&fixed, y)
                    .rows
                    .first()
                    .map_or(0, |r| r.total_cents())
            })
            .sum();
        assert_eq!(
            life, 12_481_600,
            "the whole life recovers the cost, not more"
        );
    }
}
