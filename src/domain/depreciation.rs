//! Depreciable assets: what the partnership owns, and the facts MACRS needs
//! about each one.
//!
//! # Why an asset register exists at all
//!
//! Every other figure on the return is a sum over the ledger. Depreciation is
//! not: the deduction for a kiln bought in 2023 depends on what it cost, when it
//! was placed in service, which recovery class it falls in, and which year of
//! that class this return is — none of which a journal entry records. The ledger
//! holds the *result* of the calculation, never its inputs, so the calculation
//! cannot be redone from the books alone. This module is those inputs.
//!
//! The register is the source; the ledger entry is the consequence. Computing a
//! year posts one journal entry (see
//! [`crate::commands::depreciation_commands`]), and from there the ordinary
//! account-to-tax-line mapping fills page 1 line 16a and Schedule L 9a/9b with
//! no special case at all. That is the whole reason to post rather than to
//! override: one source of truth for the lines, and it is the same one every
//! other line uses.
//!
//! # The trap this model exists to avoid: 15-year property is not one thing
//!
//! A recovery period does not determine a method. Two kinds of property both sit
//! at 15 years and depreciate completely differently:
//!
//! - **Land improvements** — a parking lot, a fence, site drainage — are 15-year
//!   property written off at **150% declining balance**.
//! - **Qualified improvement property** — the interior fit-out of a leased
//!   studio — is 15-year property written off **straight line**.
//!
//! A model that stored "15 years" and inferred the method would silently apply
//! declining balance to a leasehold fit-out and overstate the early years by
//! thousands. So [`PropertyClass`] is the unit here, and it carries the life,
//! the method and the convention together; a bare number of years is never
//! enough to depreciate anything.
//!
//! # Leasehold improvements in particular
//!
//! These are the ones people get wrong, and for a business that fits out a rented
//! studio they are usually the largest asset on the register. The fork is
//! [`PropertyClass::QualifiedImprovement`] against
//! [`PropertyClass::Nonresidential`], and it is worth 24 years of recovery
//! period:
//!
//! | | QIP | Nonresidential real |
//! |---|---|---|
//! | Recovery | 15 years | 39 years |
//! | Method | Straight line | Straight line |
//! | Convention | Half-year or mid-quarter | Mid-month |
//! | Bonus depreciation | Yes | No |
//! | §179 | Yes, as §179(f) real property | Only four named categories |
//!
//! An improvement is QIP under §168(e)(6) when it is made to the **interior** of
//! a **nonresidential** building, and placed in service **after** the building
//! first was. Three things are carved out however interior they look — an
//! **enlargement** of the building, an **elevator or escalator**, and the
//! **internal structural framework** — and those fall back to 39-year
//! nonresidential real property. [`PropertyClass::qip_exclusions`] carries that
//! list so the desktop can show it at the point the class is chosen, which is the
//! only moment anybody is in a position to answer it.
//!
//! One consequence worth knowing before the lease ends: improvements are
//! recovered over the class life and **not** over the lease term, however short
//! the lease. §168(i)(8) says so. What happens at the end is a disposition —
//! abandoning the improvements leaves the remaining basis deductible as a loss —
//! which is why [`DepreciableAsset::disposed_on`] exists rather than an
//! assumption that assets live out their recovery period.

use chrono::{Datelike, NaiveDate};

/// Which MACRS class a property falls in.
///
/// The class — not the number of years — is the unit, because the number of years
/// does not determine the method: see the module docs on 15-year property. Each
/// variant answers life, method, convention and eligibility together, so there is
/// no way to hold a life without the method that goes with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyClass {
    /// 3-year: tractor units, certain tools, breeding hogs.
    ThreeYear,
    /// 5-year: computers, office machinery, cars and light trucks, R&D
    /// equipment. The commonest class in a small partnership.
    FiveYear,
    /// 7-year: office furniture and fixtures, and the catch-all for property
    /// with no class life assigned — which is where a kiln, a pottery wheel or a
    /// set of studio easels lands.
    SevenYear,
    /// 10-year: single-purpose agricultural structures, fruit-bearing trees,
    /// vessels.
    TenYear,
    /// 15-year land improvements: parking lots, fences, sidewalks, drainage,
    /// landscaping. **150% declining balance** — the distinction from
    /// [`Self::QualifiedImprovement`], which shares the 15 years and does not
    /// share the method.
    FifteenYearLandImprovement,
    /// Qualified improvement property under §168(e)(6): an interior improvement
    /// to a nonresidential building, made after the building was first placed in
    /// service. 15 years, **straight line**, and bonus-eligible because 15 is
    /// within the 20-year ceiling §168(k) sets.
    ///
    /// This is where a fit-out of leased premises belongs, unless one of
    /// [`Self::qip_exclusions`] applies.
    QualifiedImprovement,
    /// 20-year: farm buildings, municipal sewers.
    TwentyYear,
    /// 25-year water utility property. Straight line, and past the 20-year
    /// ceiling, so no bonus.
    TwentyFiveYear,
    /// 27.5-year residential rental property. Straight line, mid-month.
    ResidentialRental,
    /// 39-year nonresidential real property — a building, and any improvement to
    /// one that fails the QIP test. Straight line, mid-month, no bonus.
    Nonresidential,
}

/// Which depreciation system applies: the general one, or the alternative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum System {
    /// The general depreciation system — what almost everything uses.
    #[default]
    Gds,
    /// The alternative depreciation system: longer lives, always straight line.
    /// Required for property used predominantly outside the United States, for
    /// tax-exempt use property, and whenever it is elected.
    Ads,
}

impl System {
    /// The stable string the event log stores.
    pub fn as_str(self) -> &'static str {
        match self {
            System::Gds => "gds",
            System::Ads => "ads",
        }
    }

    pub fn parse(s: &str) -> Option<System> {
        match s {
            "gds" => Some(System::Gds),
            "ads" => Some(System::Ads),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            System::Gds => "GDS (general)",
            System::Ads => "ADS (alternative)",
        }
    }
}

/// The averaging convention: how much of the first year the property counts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Convention {
    /// Half a year in the first year and in the last, whatever month the asset
    /// arrived. The default for personal property.
    HalfYear,
    /// The asset counts from the middle of the quarter it arrived in. Forced on
    /// *all* of a year's personal property when too much of it arrived late —
    /// see [`crate::tax::depreciation::mid_quarter_applies`].
    MidQuarter,
    /// The asset counts from the middle of the month it arrived in. Real
    /// property only, and never displaced by the mid-quarter test.
    MidMonth,
}

/// How the deduction is computed within the recovery period.
///
/// `PartialEq` but not `Eq`: the declining-balance factor is a float, and 200%
/// against 150% is the only comparison anybody makes of it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Method {
    /// Declining balance at `factor` times the straight-line rate, switching to
    /// straight line in the first year straight line gives more. The switch is
    /// not optional — it is how the IRS tables are built.
    DecliningBalance { factor: f64 },
    /// Straight line over the recovery period.
    StraightLine,
}

/// Whether §179 can be elected on a class, and on what terms.
///
/// Three-valued rather than a bool because the honest answer for real property is
/// "sometimes, and only you know". §179(f) admits certain improvements to
/// nonresidential buildings — and only those — so the register cannot decide it
/// from the class alone, and refusing outright would be wrong as often as
/// allowing silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section179Eligibility {
    /// §1245 property, or real property §179(f) admits outright. Elect freely.
    Eligible,
    /// §1250 property outside §179(f). The election is not available.
    NotEligible,
    /// Available only for the property this names. Electing is allowed and
    /// carries a warning quoting the condition, because whether a given roof
    /// qualifies is a fact about the roof, not about the class.
    Conditional(&'static str),
}

impl PropertyClass {
    /// Every class, for a picker.
    pub const ALL: [PropertyClass; 10] = [
        PropertyClass::ThreeYear,
        PropertyClass::FiveYear,
        PropertyClass::SevenYear,
        PropertyClass::TenYear,
        PropertyClass::FifteenYearLandImprovement,
        PropertyClass::QualifiedImprovement,
        PropertyClass::TwentyYear,
        PropertyClass::TwentyFiveYear,
        PropertyClass::ResidentialRental,
        PropertyClass::Nonresidential,
    ];

    /// The stable string the event log stores.
    ///
    /// Named by what the class *is*, never by its recovery period: a log that
    /// recorded `"15_year"` could not tell a parking lot from a shop fit-out, and
    /// those two depreciate differently. Changing one of these strings silently
    /// re-classes every asset already logged under it.
    pub fn as_str(self) -> &'static str {
        match self {
            PropertyClass::ThreeYear => "three_year",
            PropertyClass::FiveYear => "five_year",
            PropertyClass::SevenYear => "seven_year",
            PropertyClass::TenYear => "ten_year",
            PropertyClass::FifteenYearLandImprovement => "fifteen_year_land_improvement",
            PropertyClass::QualifiedImprovement => "qualified_improvement",
            PropertyClass::TwentyYear => "twenty_year",
            PropertyClass::TwentyFiveYear => "twenty_five_year",
            PropertyClass::ResidentialRental => "residential_rental",
            PropertyClass::Nonresidential => "nonresidential",
        }
    }

    pub fn parse(s: &str) -> Option<PropertyClass> {
        PropertyClass::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// What to call it on screen.
    pub fn label(self) -> &'static str {
        match self {
            PropertyClass::ThreeYear => "3-year",
            PropertyClass::FiveYear => "5-year",
            PropertyClass::SevenYear => "7-year",
            PropertyClass::TenYear => "10-year",
            PropertyClass::FifteenYearLandImprovement => "15-year land improvement",
            PropertyClass::QualifiedImprovement => "Qualified improvement property (15-year)",
            PropertyClass::TwentyYear => "20-year",
            PropertyClass::TwentyFiveYear => "25-year water utility",
            PropertyClass::ResidentialRental => "Residential rental (27.5-year)",
            PropertyClass::Nonresidential => "Nonresidential real (39-year)",
        }
    }

    /// Examples, for the desktop to show beside the class.
    pub fn examples(self) -> &'static str {
        match self {
            PropertyClass::ThreeYear => "Tractor units, certain special tools.",
            PropertyClass::FiveYear => {
                "Computers, printers, cameras, cars and light trucks, office machinery."
            }
            PropertyClass::SevenYear => {
                "Office furniture and fixtures, and anything with no class life of its own — \
                 kilns, pottery wheels, easels, studio equipment."
            }
            PropertyClass::TenYear => "Single-purpose agricultural structures, vessels.",
            PropertyClass::FifteenYearLandImprovement => {
                "Parking lots, fences, sidewalks, drainage, landscaping. Declining balance, \
                 unlike qualified improvement property at the same 15 years."
            }
            PropertyClass::QualifiedImprovement => {
                "Interior fit-out of leased or owned nonresidential premises, done after the \
                 building was first placed in service — partitions, lighting, flooring, \
                 plumbing serving the interior."
            }
            PropertyClass::TwentyYear => "Farm buildings, municipal sewers.",
            PropertyClass::TwentyFiveYear => "Water utility property.",
            PropertyClass::ResidentialRental => {
                "A building where 80% or more of the rent is from dwelling units."
            }
            PropertyClass::Nonresidential => {
                "A building, and any improvement to one that is not qualified improvement \
                 property — an enlargement, an elevator, the structural framework."
            }
        }
    }

    /// What is carved out of qualified improvement property, for the desktop to
    /// show at the moment somebody picks that class.
    ///
    /// Shown rather than enforced. Whether a wall is part of the internal
    /// structural framework is a fact about the building that the register has no
    /// way to hold, so the register asks the question at the one moment the
    /// person answering it is looking at the asset.
    pub fn qip_exclusions() -> &'static [&'static str] {
        &[
            "An enlargement of the building — added floor area is not QIP.",
            "An elevator or escalator.",
            "The internal structural framework of the building.",
            "Anything in a residential building — QIP is nonresidential only.",
            "Anything placed in service before the building itself was.",
        ]
    }

    /// The recovery period in years under the given system.
    ///
    /// Fractional for residential rental, which is why this is not an integer:
    /// 27.5 years is the statute's number, not a rounding of 27 or 28.
    pub fn recovery_years(self, system: System) -> f64 {
        match (self, system) {
            (PropertyClass::ThreeYear, System::Gds) => 3.0,
            (PropertyClass::ThreeYear, System::Ads) => 3.0,
            (PropertyClass::FiveYear, System::Gds) => 5.0,
            (PropertyClass::FiveYear, System::Ads) => 5.0,
            (PropertyClass::SevenYear, System::Gds) => 7.0,
            (PropertyClass::SevenYear, System::Ads) => 10.0,
            (PropertyClass::TenYear, System::Gds) => 10.0,
            (PropertyClass::TenYear, System::Ads) => 10.0,
            (PropertyClass::FifteenYearLandImprovement, System::Gds) => 15.0,
            (PropertyClass::FifteenYearLandImprovement, System::Ads) => 20.0,
            (PropertyClass::QualifiedImprovement, System::Gds) => 15.0,
            (PropertyClass::QualifiedImprovement, System::Ads) => 20.0,
            (PropertyClass::TwentyYear, System::Gds) => 20.0,
            (PropertyClass::TwentyYear, System::Ads) => 25.0,
            (PropertyClass::TwentyFiveYear, System::Gds) => 25.0,
            (PropertyClass::TwentyFiveYear, System::Ads) => 50.0,
            (PropertyClass::ResidentialRental, System::Gds) => 27.5,
            (PropertyClass::ResidentialRental, System::Ads) => 30.0,
            (PropertyClass::Nonresidential, System::Gds) => 39.0,
            (PropertyClass::Nonresidential, System::Ads) => 40.0,
        }
    }

    /// The method the class uses.
    ///
    /// ADS is always straight line. Under GDS the answer is per class, and the
    /// two 15-year classes disagree — which is the whole reason this is a method
    /// on the class rather than a function of the recovery period.
    pub fn method(self, system: System) -> Method {
        if system == System::Ads {
            return Method::StraightLine;
        }
        match self {
            PropertyClass::ThreeYear
            | PropertyClass::FiveYear
            | PropertyClass::SevenYear
            | PropertyClass::TenYear => Method::DecliningBalance { factor: 2.0 },
            PropertyClass::FifteenYearLandImprovement | PropertyClass::TwentyYear => {
                Method::DecliningBalance { factor: 1.5 }
            }
            PropertyClass::QualifiedImprovement
            | PropertyClass::TwentyFiveYear
            | PropertyClass::ResidentialRental
            | PropertyClass::Nonresidential => Method::StraightLine,
        }
    }

    /// Whether the class takes the mid-month convention.
    ///
    /// Real property does; everything else takes half-year or mid-quarter. This
    /// is also exactly the line the mid-quarter test does not cross — see
    /// [`Self::is_personal_property`].
    pub fn uses_mid_month(self) -> bool {
        matches!(
            self,
            PropertyClass::ResidentialRental | PropertyClass::Nonresidential
        )
    }

    /// Whether the class counts as personal property for the mid-quarter test.
    ///
    /// The test in §168(d)(3) looks at property to which the half-year convention
    /// would otherwise apply, so mid-month real property is outside it — neither
    /// counted in the 40% fraction nor pushed onto mid-quarter by it.
    ///
    /// Qualified improvement property *is* inside it, despite being an
    /// improvement to a building: §168(e)(6) makes it 15-year property, and
    /// 15-year property takes the half-year convention.
    pub fn is_personal_property(self) -> bool {
        !self.uses_mid_month()
    }

    /// Whether bonus depreciation is available.
    ///
    /// §168(k) requires a recovery period of 20 years or less, which is what
    /// admits qualified improvement property at 15 years and excludes the same
    /// improvement at 39 when it fails the QIP test. Judged on the **GDS** period
    /// whichever system is elected, as §168(k)(2)(A)(i) does.
    pub fn bonus_eligible(self) -> bool {
        self.recovery_years(System::Gds) <= 20.0
    }

    /// Whether §179 can be elected, and on what terms.
    pub fn section_179(self) -> Section179Eligibility {
        match self {
            // §1245 tangible personal property.
            PropertyClass::ThreeYear
            | PropertyClass::FiveYear
            | PropertyClass::SevenYear
            | PropertyClass::TenYear
            | PropertyClass::TwentyYear => Section179Eligibility::Eligible,
            // §179(f) names qualified improvement property outright.
            PropertyClass::QualifiedImprovement => Section179Eligibility::Eligible,
            // §1250 property that §179(f) does not reach.
            PropertyClass::FifteenYearLandImprovement
            | PropertyClass::TwentyFiveYear
            | PropertyClass::ResidentialRental => Section179Eligibility::NotEligible,
            // §179(f) admits four named improvements to nonresidential buildings
            // and nothing else, so the building itself never qualifies and a new
            // roof on it may.
            PropertyClass::Nonresidential => Section179Eligibility::Conditional(
                "§179(f) admits only roofs, heating, ventilation and air-conditioning, fire \
                 protection and alarm systems, and security systems — and only improvements to \
                 nonresidential real property placed in service after the building was. The \
                 building itself never qualifies.",
            ),
        }
    }
}

// Display for the three stored enums, so a picker can show them without every
// caller reaching for `label()`. All three print the label a person reads, never
// the string the log stores — those are deliberately different, and a picker
// showing `fifteen_year_land_improvement` would be a bug you could see.
impl std::fmt::Display for PropertyClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl std::fmt::Display for System {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Whether an asset takes bonus depreciation.
///
/// Per asset here, though §168(k)(7) makes the election **per property class per
/// year**: elect out of 5-year property for a year and every 5-year asset that
/// year is out. Modelling it per asset is what a register can actually record —
/// the class-wide election is a consequence — so
/// [`crate::tax::depreciation::compute_year`] reports a class where some assets
/// take bonus and others decline, rather than letting the return carry an
/// election that was never validly made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BonusElection {
    /// Take bonus at whatever rate the acquisition date earns.
    #[default]
    Take,
    /// Elect out under §168(k)(7).
    Decline,
}

impl std::fmt::Display for BonusElection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl BonusElection {
    pub fn label(self) -> &'static str {
        match self {
            BonusElection::Take => "Take bonus",
            BonusElection::Decline => "Elect out",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            BonusElection::Take => "take",
            BonusElection::Decline => "decline",
        }
    }

    pub fn parse(s: &str) -> Option<BonusElection> {
        match s {
            "take" => Some(BonusElection::Take),
            "decline" => Some(BonusElection::Decline),
            _ => None,
        }
    }
}

/// One year's depreciation on one asset, fixed by hand.
///
/// For the year the register cannot reproduce — a return already filed on a
/// figure the statute's tables do not give. The books have to carry what the
/// return claimed, so the override replaces that year's bonus and MACRS, and the
/// note says why, because an override with no reason cannot be told apart from a
/// mistake. §179 is an election of its own and is not touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepreciationOverride {
    pub amount_cents: i64,
    pub note: String,
}

/// A change to an asset's basis after it was bought, with the reason.
///
/// For a grant or rebate that reimburses what an asset cost, a casualty loss, or
/// anything else that moves the basis the register depreciates. Negative reduces
/// the basis. From `effective_year` on, depreciation runs on the adjusted basis
/// less what has already been allowed, over what is left of the recovery period;
/// an adjustment in the year the asset was placed in service is simply part of
/// its opening basis. The note is required, because an adjusted basis nobody can
/// explain is a deduction nobody can defend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasisAdjustment {
    pub adjustment_id: String,
    pub effective_year: i32,
    pub amount_cents: i64,
    pub note: String,
}

/// One depreciable asset, as the register holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct DepreciableAsset {
    pub asset_id: String,
    /// What it is, in the owner's words. Appears on the depreciation schedule
    /// and in the posted entry's memo.
    pub description: String,
    /// The fixed-asset account the cost sits in, so Schedule L line 9a can be
    /// reconciled against the register rather than assumed to agree.
    pub asset_account_id: String,
    /// The expense account the year's deduction is debited to.
    pub expense_account_id: String,
    /// The contra-asset account it is credited to.
    pub accumulated_account_id: String,
    /// Where a §179 election is expensed, when one is made.
    ///
    /// A different account from [`Self::expense_account_id`] because the two
    /// reach different lines: ordinary depreciation is page 1 line 16a, and §179
    /// is separately stated on Schedule K line 12. Sharing one account would put
    /// the §179 into the partnership's ordinary deduction, where each partner's
    /// own dollar limit can never be applied to it.
    pub section_179_account_id: Option<String>,
    /// When it was **acquired** — which is not when it was placed in service,
    /// and the difference decides the bonus rate. 2025 is a split year: property
    /// acquired before 20 January 2025 earns 40%, and property acquired on or
    /// after earns 100%.
    pub acquired_on: NaiveDate,
    /// When it was **placed in service** — available and ready for its intended
    /// use. This starts the recovery period and fixes the convention; a kiln
    /// bought in November and first fired in January depreciates from January.
    pub placed_in_service: NaiveDate,
    /// Cost or other basis, in cents.
    pub cost_cents: i64,
    pub class: PropertyClass,
    pub system: System,
    /// §179 elected against this asset, in cents. Zero for no election.
    pub section_179_cents: i64,
    pub bonus: BonusElection,
    /// When it left the business, if it has. Stops depreciation, and takes the
    /// asset off Schedule L.
    pub disposed_on: Option<NaiveDate>,
    /// Free text — a serial number, a room, an invoice reference.
    pub notes: Option<String>,
    /// Years whose depreciation is fixed by hand, by tax year. Not part of the
    /// asset's own events: it arrives with its own, and a correction to the asset
    /// leaves it alone.
    pub overrides: std::collections::BTreeMap<i32, DepreciationOverride>,
    /// Changes to the basis after purchase, oldest first. Like the overrides,
    /// they arrive with events of their own.
    pub basis_adjustments: Vec<BasisAdjustment>,
}

impl DepreciableAsset {
    /// Cost plus every basis adjustment in effect by the end of `tax_year`.
    pub fn adjusted_cost_through(&self, tax_year: i32) -> i64 {
        self.cost_cents
            + self
                .basis_adjustments
                .iter()
                .filter(|a| a.effective_year <= tax_year)
                .map(|a| a.amount_cents)
                .sum::<i64>()
    }

    /// The basis the recovery period starts from: cost plus any adjustment made
    /// in the year the asset was placed in service. §179, bonus and the tables
    /// all run on this.
    pub fn opening_basis_cents(&self) -> i64 {
        self.adjusted_cost_through(self.placed_in_service.year())
    }

    /// Whether an adjustment made after the placed-in-service year has taken
    /// effect by `tax_year` — from which point the tables no longer apply and
    /// the adjusted basis is spread over what is left of the recovery period.
    pub fn basis_adjusted_after_placement_by(&self, tax_year: i32) -> bool {
        let placed = self.placed_in_service.year();
        self.basis_adjustments
            .iter()
            .any(|a| a.effective_year > placed && a.effective_year <= tax_year)
    }

    /// The convention this asset takes, given whether the mid-quarter test
    /// caught the year it was placed in service.
    ///
    /// Real property is never caught: it is on mid-month, which the test does not
    /// reach.
    pub fn convention(&self, mid_quarter_year: bool) -> Convention {
        if self.class.uses_mid_month() {
            Convention::MidMonth
        } else if mid_quarter_year {
            Convention::MidQuarter
        } else {
            Convention::HalfYear
        }
    }

    /// Which year of the recovery period `tax_year` is, counting from 1.
    ///
    /// `None` before it was placed in service, so a return for 2024 sees nothing
    /// of an asset first used in 2025.
    pub fn recovery_year(&self, tax_year: i32) -> Option<u32> {
        let placed = self.placed_in_service.year();
        if tax_year < placed {
            return None;
        }
        Some((tax_year - placed + 1) as u32)
    }

    /// Whether the asset was still held at any point during the tax year.
    ///
    /// An asset disposed of during the year is still held for part of it and
    /// still earns a part-year deduction, so the test is against the year it was
    /// disposed in, not against whether a disposal exists.
    pub fn held_during(&self, tax_year: i32) -> bool {
        if self.placed_in_service.year() > tax_year {
            return false;
        }
        match self.disposed_on {
            Some(d) => d.year() >= tax_year,
            None => true,
        }
    }

    /// Whether it was disposed of during this tax year.
    pub fn disposed_during(&self, tax_year: i32) -> bool {
        self.disposed_on.is_some_and(|d| d.year() == tax_year)
    }

    /// Placed in service and disposed of in the same tax year.
    ///
    /// No depreciation is allowed at all in that case, and — separately — the
    /// asset is left out of the mid-quarter test's arithmetic, so it cannot drag
    /// a year's other assets onto mid-quarter on its way through.
    pub fn placed_and_disposed_same_year(&self) -> bool {
        self.disposed_on
            .is_some_and(|d| d.year() == self.placed_in_service.year())
    }

    /// The quarter of the tax year the asset was placed in service, 1 through 4.
    pub fn quarter_placed(&self) -> u32 {
        (self.placed_in_service.month() - 1) / 3 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn asset(class: PropertyClass, placed: NaiveDate) -> DepreciableAsset {
        DepreciableAsset {
            asset_id: "a".into(),
            description: "Thing".into(),
            asset_account_id: "1500".into(),
            expense_account_id: "6500".into(),
            accumulated_account_id: "1590".into(),
            section_179_account_id: None,
            acquired_on: placed,
            placed_in_service: placed,
            cost_cents: 100_000,
            class,
            system: System::Gds,
            section_179_cents: 0,
            bonus: BonusElection::Take,
            disposed_on: None,
            notes: None,
            overrides: Default::default(),
            basis_adjustments: Vec::new(),
        }
    }

    /// The distinction the whole model exists for. Same 15 years, different
    /// method — a register that stored the life alone could not tell them apart.
    #[test]
    fn the_two_fifteen_year_classes_share_a_life_and_not_a_method() {
        let lot = PropertyClass::FifteenYearLandImprovement;
        let fitout = PropertyClass::QualifiedImprovement;

        assert_eq!(lot.recovery_years(System::Gds), 15.0);
        assert_eq!(fitout.recovery_years(System::Gds), 15.0);

        assert_eq!(
            lot.method(System::Gds),
            Method::DecliningBalance { factor: 1.5 }
        );
        assert_eq!(fitout.method(System::Gds), Method::StraightLine);
    }

    /// A leasehold fit-out that fails the QIP test is 39-year property, and the
    /// consequences run past the recovery period: it loses bonus entirely and
    /// moves to the mid-month convention.
    #[test]
    fn failing_the_qip_test_costs_bonus_and_twenty_four_years() {
        let qip = PropertyClass::QualifiedImprovement;
        let not_qip = PropertyClass::Nonresidential;

        assert_eq!(qip.recovery_years(System::Gds), 15.0);
        assert_eq!(not_qip.recovery_years(System::Gds), 39.0);

        assert!(
            qip.bonus_eligible(),
            "15 years is inside the 20-year ceiling"
        );
        assert!(!not_qip.bonus_eligible(), "39 years is not");

        assert!(
            !qip.uses_mid_month(),
            "15-year property is half-year or mid-quarter"
        );
        assert!(not_qip.uses_mid_month());

        assert_eq!(qip.section_179(), Section179Eligibility::Eligible);
        assert!(matches!(
            not_qip.section_179(),
            Section179Eligibility::Conditional(_)
        ));
    }

    /// Bonus is judged on the GDS period even when ADS is elected, so electing
    /// ADS on 7-year property does not quietly forfeit bonus.
    #[test]
    fn bonus_eligibility_reads_the_gds_period_whichever_system_is_elected() {
        assert_eq!(PropertyClass::SevenYear.recovery_years(System::Ads), 10.0);
        assert!(PropertyClass::SevenYear.bonus_eligible());

        // And the ceiling genuinely excludes: 25-year water utility is past it.
        assert!(!PropertyClass::TwentyFiveYear.bonus_eligible());
    }

    /// ADS is straight line whatever the class would otherwise do.
    #[test]
    fn ads_is_always_straight_line() {
        for class in PropertyClass::ALL {
            assert_eq!(class.method(System::Ads), Method::StraightLine, "{class:?}");
        }
    }

    /// Real property is on mid-month and stays there — the mid-quarter test
    /// cannot reach it, whatever else happened that year.
    #[test]
    fn real_property_keeps_mid_month_through_a_mid_quarter_year() {
        let building = asset(PropertyClass::Nonresidential, date(2025, 11, 1));
        assert_eq!(building.convention(true), Convention::MidMonth);
        assert_eq!(building.convention(false), Convention::MidMonth);

        let kiln = asset(PropertyClass::SevenYear, date(2025, 11, 1));
        assert_eq!(kiln.convention(true), Convention::MidQuarter);
        assert_eq!(kiln.convention(false), Convention::HalfYear);
    }

    /// Qualified improvement property is personal property for the mid-quarter
    /// test, despite being an improvement to a building — it is 15-year property,
    /// and 15-year property is on the half-year convention.
    #[test]
    fn qip_is_inside_the_mid_quarter_test_and_the_building_is_not() {
        assert!(PropertyClass::QualifiedImprovement.is_personal_property());
        assert!(!PropertyClass::Nonresidential.is_personal_property());
    }

    /// The class strings are what the log stores, so every one of them has to
    /// survive a round trip — a rename silently re-classes assets already logged.
    #[test]
    fn every_class_round_trips_through_its_stored_string() {
        for class in PropertyClass::ALL {
            assert_eq!(
                PropertyClass::parse(class.as_str()),
                Some(class),
                "{class:?}"
            );
        }
        assert_eq!(
            PropertyClass::parse("fifteen_year"),
            None,
            "no bare life key"
        );
    }

    #[test]
    fn recovery_year_counts_from_the_year_placed_in_service() {
        let a = asset(PropertyClass::FiveYear, date(2023, 6, 1));
        assert_eq!(a.recovery_year(2022), None);
        assert_eq!(a.recovery_year(2023), Some(1));
        assert_eq!(a.recovery_year(2025), Some(3));
    }

    #[test]
    fn a_disposal_ends_the_years_the_asset_is_held_for() {
        let mut a = asset(PropertyClass::FiveYear, date(2023, 6, 1));
        a.disposed_on = Some(date(2025, 4, 1));

        assert!(a.held_during(2024));
        assert!(
            a.held_during(2025),
            "still held for part of the year it went"
        );
        assert!(!a.held_during(2026));
        assert!(a.disposed_during(2025));
        assert!(!a.placed_and_disposed_same_year());
    }

    #[test]
    fn quarters_are_read_from_the_placed_in_service_month() {
        for (month, quarter) in [
            (1, 1),
            (3, 1),
            (4, 2),
            (6, 2),
            (7, 3),
            (9, 3),
            (10, 4),
            (12, 4),
        ] {
            let a = asset(PropertyClass::FiveYear, date(2025, month, 15));
            assert_eq!(a.quarter_placed(), quarter, "month {month}");
        }
    }
}
