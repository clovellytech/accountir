//! The year's federal figures: brackets, the standard deduction, the capital-gain
//! thresholds, and the handful of amounts Form 1040 phases things in and out by.
//!
//! # Where these come from, and how sure they are
//!
//! 2025: Rev. Proc. 2024-40, with the standard deduction, the child tax credit and
//! the senior deduction as Public Law 119-21 (the 2025 reconciliation act) changed
//! them. 2026: Rev. Proc. 2025-32.
//!
//! **They were entered by hand and have not been checked against the published
//! tables.** That is what [`YearParams::verified`] says, and a return built from an
//! unverified table carries a warning saying so. Checking a year is: open the
//! revenue procedure, compare every figure below, and flip the flag — nothing else
//! reads it. A year this module does not have is refused rather than guessed from
//! the nearest one, because last year's brackets produce a plausible, wrong tax.

use crate::events::types::FilingStatus;

/// Dollars to cents, for writing the tables in the units they are published in.
const fn d(dollars: i64) -> i64 {
    dollars * 100
}

/// Figures that differ by filing status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByStatus<T> {
    pub single: T,
    pub married_jointly: T,
    pub married_separately: T,
    pub head_of_household: T,
}

impl<T: Copy> ByStatus<T> {
    /// A qualifying surviving spouse uses the joint figures throughout.
    pub fn of(&self, status: FilingStatus) -> T {
        match status {
            FilingStatus::Single => self.single,
            FilingStatus::MarriedFilingJointly | FilingStatus::QualifyingSurvivingSpouse => {
                self.married_jointly
            }
            FilingStatus::MarriedFilingSeparately => self.married_separately,
            FilingStatus::HeadOfHousehold => self.head_of_household,
        }
    }
}

/// The ordinary brackets: the top of each of the 10, 12, 22, 24, 32 and 35% bands,
/// in cents. Everything above the last is 37%.
pub type Brackets = [i64; 6];

pub const RATES_BP: [i64; 7] = [1000, 1200, 2200, 2400, 3200, 3500, 3700];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct YearParams {
    pub year: i32,
    /// Whether every figure below has been checked against the published table.
    pub verified: bool,
    pub source: &'static str,
    pub brackets: ByStatus<Brackets>,
    pub standard_deduction: ByStatus<i64>,
    /// Per box checked (65 or older, blind), for a married filer or a surviving
    /// spouse.
    pub additional_standard_married: i64,
    /// Per box checked, for a single or head-of-household filer.
    pub additional_standard_unmarried: i64,
    /// The top of the 0% band for qualified dividends and long-term gain.
    pub capital_gain_zero_max: ByStatus<i64>,
    /// The top of the 15% band; above it, 20%.
    pub capital_gain_fifteen_max: ByStatus<i64>,
    pub child_tax_credit: i64,
    pub other_dependent_credit: i64,
    /// Where the child and dependent credits start to phase out: $50 per $1,000
    /// of modified AGI above it.
    pub child_credit_phaseout: ByStatus<i64>,
    /// The senior deduction (Schedule 1-A), per person 65 or older. Zero in a year
    /// it does not exist.
    pub senior_deduction: i64,
    /// Where it starts to phase out, at 6% of modified AGI above.
    pub senior_deduction_phaseout: ByStatus<i64>,
    /// The Social Security wage base: the most earnings, wages and
    /// self-employment together, that the 12.4% part of SE tax reaches.
    pub social_security_wage_base: i64,
    /// Taxable income (before the QBI deduction) up to which Form 8995's
    /// simplified computation applies — above it, Form 8995-A and its W-2 wage
    /// and property limits.
    pub qbi_threshold: ByStatus<i64>,
}

/// Fixed by statute rather than indexed: the net investment income tax's
/// thresholds (IRC §1411), the capital loss limit (§1211), and the Social Security
/// base amounts (§86).
pub const NIIT_RATE_BP: i64 = 380;

/// Schedule SE: net earnings are 92.35% of net profit (the employer-equivalent
/// half of the tax taken out), Social Security at 12.4% up to the wage base,
/// Medicare at 2.9% on all of it, and nothing at all below $400 of net earnings.
pub const SE_NET_EARNINGS_BP: i64 = 9_235;
pub const SE_SOCIAL_SECURITY_BP: i64 = 1_240;
pub const SE_MEDICARE_BP: i64 = 290;
pub const SE_MINIMUM_EARNINGS: i64 = d(400);
/// The Additional Medicare Tax (Form 8959): 0.9% of wages and self-employment
/// income above the threshold. Fixed in the statute, not indexed.
pub const ADDITIONAL_MEDICARE_THRESHOLD: ByStatus<i64> = ByStatus {
    single: d(200_000),
    married_jointly: d(250_000),
    married_separately: d(125_000),
    head_of_household: d(200_000),
};
/// The QBI deduction's rate, 20%.
pub const QBI_RATE_BP: i64 = 2_000;
pub const NIIT_THRESHOLD: ByStatus<i64> = ByStatus {
    single: d(200_000),
    married_jointly: d(250_000),
    married_separately: d(125_000),
    head_of_household: d(200_000),
};
pub const CAPITAL_LOSS_LIMIT: ByStatus<i64> = ByStatus {
    single: d(3_000),
    married_jointly: d(3_000),
    married_separately: d(1_500),
    head_of_household: d(3_000),
};
/// The first and second Social Security base amounts. Married filing separately is
/// zero for both — the case of spouses who lived together at any time in the year,
/// which is the usual one; one who lived apart all year uses the single figures,
/// and the return warns.
pub const SS_BASE_ONE: ByStatus<i64> = ByStatus {
    single: d(25_000),
    married_jointly: d(32_000),
    married_separately: 0,
    head_of_household: d(25_000),
};
pub const SS_BASE_TWO: ByStatus<i64> = ByStatus {
    single: d(34_000),
    married_jointly: d(44_000),
    married_separately: 0,
    head_of_household: d(34_000),
};

const Y2025: YearParams = YearParams {
    year: 2025,
    verified: false,
    source: "Rev. Proc. 2024-40, as amended by P.L. 119-21",
    brackets: ByStatus {
        single: [
            d(11_925),
            d(48_475),
            d(103_350),
            d(197_300),
            d(250_525),
            d(626_350),
        ],
        married_jointly: [
            d(23_850),
            d(96_950),
            d(206_700),
            d(394_600),
            d(501_050),
            d(751_600),
        ],
        married_separately: [
            d(11_925),
            d(48_475),
            d(103_350),
            d(197_300),
            d(250_525),
            d(375_800),
        ],
        head_of_household: [
            d(17_000),
            d(64_850),
            d(103_350),
            d(197_300),
            d(250_500),
            d(626_350),
        ],
    },
    standard_deduction: ByStatus {
        single: d(15_750),
        married_jointly: d(31_500),
        married_separately: d(15_750),
        head_of_household: d(23_625),
    },
    additional_standard_married: d(1_600),
    additional_standard_unmarried: d(2_000),
    capital_gain_zero_max: ByStatus {
        single: d(48_350),
        married_jointly: d(96_700),
        married_separately: d(48_350),
        head_of_household: d(64_750),
    },
    capital_gain_fifteen_max: ByStatus {
        single: d(533_400),
        married_jointly: d(600_050),
        married_separately: d(300_000),
        head_of_household: d(566_700),
    },
    child_tax_credit: d(2_200),
    other_dependent_credit: d(500),
    child_credit_phaseout: ByStatus {
        single: d(200_000),
        married_jointly: d(400_000),
        married_separately: d(200_000),
        head_of_household: d(200_000),
    },
    senior_deduction: d(6_000),
    senior_deduction_phaseout: ByStatus {
        single: d(75_000),
        married_jointly: d(150_000),
        married_separately: d(75_000),
        head_of_household: d(75_000),
    },
    // SSA's 2025 contribution and benefit base.
    social_security_wage_base: d(176_100),
    // Rev. Proc. 2024-40 §2.25: $197,300, joint $394,600.
    qbi_threshold: ByStatus {
        single: d(197_300),
        married_jointly: d(394_600),
        married_separately: d(197_300),
        head_of_household: d(197_300),
    },
};

const Y2026: YearParams = YearParams {
    year: 2026,
    verified: false,
    source: "Rev. Proc. 2025-32",
    brackets: ByStatus {
        single: [
            d(12_400),
            d(50_400),
            d(105_700),
            d(201_775),
            d(256_225),
            d(640_600),
        ],
        married_jointly: [
            d(24_800),
            d(100_800),
            d(211_400),
            d(403_550),
            d(512_450),
            d(768_700),
        ],
        married_separately: [
            d(12_400),
            d(50_400),
            d(105_700),
            d(201_775),
            d(256_225),
            d(384_350),
        ],
        head_of_household: [
            d(17_700),
            d(67_450),
            d(105_700),
            d(201_750),
            d(256_200),
            d(640_600),
        ],
    },
    standard_deduction: ByStatus {
        single: d(16_100),
        married_jointly: d(32_200),
        married_separately: d(16_100),
        head_of_household: d(24_150),
    },
    additional_standard_married: d(1_650),
    additional_standard_unmarried: d(2_050),
    capital_gain_zero_max: ByStatus {
        single: d(49_450),
        married_jointly: d(98_900),
        married_separately: d(49_450),
        head_of_household: d(66_200),
    },
    capital_gain_fifteen_max: ByStatus {
        single: d(545_500),
        married_jointly: d(613_700),
        married_separately: d(306_850),
        head_of_household: d(579_600),
    },
    child_tax_credit: d(2_200),
    other_dependent_credit: d(500),
    child_credit_phaseout: ByStatus {
        single: d(200_000),
        married_jointly: d(400_000),
        married_separately: d(200_000),
        head_of_household: d(200_000),
    },
    senior_deduction: d(6_000),
    senior_deduction_phaseout: ByStatus {
        single: d(75_000),
        married_jointly: d(150_000),
        married_separately: d(75_000),
        head_of_household: d(75_000),
    },
    // SSA's announced 2026 base.
    social_security_wage_base: d(184_500),
    // Rev. Proc. 2025-32: $201,775, joint $403,550 — the same figures as the top of
    // the 24% bracket, as in every year since 2018.
    qbi_threshold: ByStatus {
        single: d(201_775),
        married_jointly: d(403_550),
        married_separately: d(201_775),
        head_of_household: d(201_775),
    },
};

/// The figures for a year, or `None` for a year this module does not have.
pub fn for_year(year: i32) -> Option<&'static YearParams> {
    match year {
        2025 => Some(&Y2025),
        2026 => Some(&Y2026),
        _ => None,
    }
}

pub fn supported_years() -> &'static [i32] {
    &[2025, 2026]
}

/// `cents × rate` in basis points, rounded half away from zero.
pub fn apply_bp(cents: i64, bp: i64) -> i64 {
    let n = cents as i128 * bp as i128;
    let q = n / 10_000;
    let r = n % 10_000;
    let rounded = if r.abs() * 2 >= 10_000 { q + n.signum() } else { q };
    rounded as i64
}

/// Tax on an amount by the brackets, exactly — the Tax Computation Worksheet's
/// arithmetic, used at $100,000 and above.
pub fn bracket_tax(params: &YearParams, status: FilingStatus, taxable_cents: i64) -> i64 {
    if taxable_cents <= 0 {
        return 0;
    }
    let tops = params.brackets.of(status);
    let mut tax = 0i64;
    let mut floor = 0i64;
    for (i, &rate) in RATES_BP.iter().enumerate() {
        let top = tops.get(i).copied().unwrap_or(i64::MAX);
        if taxable_cents <= floor {
            break;
        }
        let band = taxable_cents.min(top) - floor;
        tax += apply_bp(band, rate);
        floor = top;
    }
    tax
}

/// Tax on an amount the way the return computes it: from the Tax Table below
/// $100,000 — the tax on the midpoint of the row the amount falls in, to the whole
/// dollar — and by the brackets at and above it.
///
/// The table matters below $100,000 because it is what the IRS checks: computing
/// those amounts exactly gives a figure a few dollars off the one it expects.
pub fn tax_on(params: &YearParams, status: FilingStatus, taxable_cents: i64) -> i64 {
    if taxable_cents <= 0 {
        return 0;
    }
    if taxable_cents >= d(100_000) {
        return bracket_tax(params, status, taxable_cents);
    }
    // The table's rows: $0–5, $5–15, $15–25, then $25 wide to $3,000, then $50.
    let dollars = taxable_cents / 100;
    let midpoint_cents = if dollars < 5 {
        return 0;
    } else if dollars < 15 {
        d(10)
    } else if dollars < 25 {
        d(20)
    } else if dollars < 3_000 {
        (dollars / 25) * d(25) + 1_250
    } else {
        (dollars / 50) * d(50) + d(25)
    };
    let exact = bracket_tax(params, status, midpoint_cents);
    // To the whole dollar, half up.
    ((exact + 50) / 100) * 100
}

#[cfg(test)]
mod tests {
    use super::*;
    use FilingStatus::*;

    #[test]
    fn the_brackets_add_up_band_by_band() {
        let p = for_year(2025).unwrap();
        // 10% of 11,925 + 12% of 36,550 + 22% of 54,875 + 24% of 93,950 + 32% of 2,700.
        assert_eq!(bracket_tax(p, Single, d(200_000)), 4_106_300);
        assert_eq!(bracket_tax(p, Single, 0), 0);
        // A joint return's 10% band is twice as wide.
        assert_eq!(bracket_tax(p, MarriedFilingJointly, d(23_850)), d(2_385));
    }

    /// Below $100,000 the return reads the Tax Table: the tax on the row's midpoint,
    /// to the dollar.
    #[test]
    fn below_a_hundred_thousand_the_table_is_used() {
        let p = for_year(2025).unwrap();
        // $50,000–$50,050 → tax on $50,025: 1,192.50 + 4,386 + 22% of 1,550 = 5,919.50.
        assert_eq!(tax_on(p, Single, d(50_000)), d(5_920));
        assert_eq!(tax_on(p, Single, d(50_049)), d(5_920));
        assert_eq!(tax_on(p, Single, 300), 0);
    }

    #[test]
    fn a_year_not_in_the_tables_is_refused() {
        assert!(for_year(2024).is_none());
        assert!(for_year(2027).is_none());
    }

    #[test]
    fn rates_round_half_away_from_zero() {
        assert_eq!(apply_bp(5, 1000), 1); // 0.5 → 1
        assert_eq!(apply_bp(4, 1000), 0);
        assert_eq!(apply_bp(-5, 1000), -1);
    }
}
