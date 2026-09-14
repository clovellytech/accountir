//! Form IL-4562, Special Depreciation.
//!
//! Illinois does not follow federal bonus depreciation. The bonus a return takes
//! is added back in the year it is taken (Step 2), and recovered through a share
//! of each later year's regular depreciation on the property (Step 3), with the
//! property's last year settling what is left. The per-property figures come from
//! [`super::il1065::special_depreciation`]; this module turns them into the
//! form's lines and fills the blank.
//!
//! # Why the lines are figured here, and the IL-1065 reads them
//!
//! The form multiplies each bonus rate's *total* regular depreciation by its
//! factor — line 9c times 0.667, line 12c times 1.5 — in whole dollars. Summing
//! each property's own product instead can land a dollar away, and IL-1065 lines
//! 17 and 30 must carry exactly what lines 4 and 19 say. So
//! [`super::il1065::SpecialDepreciation`]'s totals come from [`Lines`].

use super::acroform::{field_map, set_text, strip_xfa, FormError};
use super::il1065::SpecialDepreciation;
use super::lines::{cents_to_dollars, format_dollars};
use crate::domain::BusinessProfile;
use lopdf::Document;

/// The tax year the vendored blank (R-12/25) is for.
pub const FORM_YEAR: i32 = 2025;

const IL4562: &[u8] = include_bytes!("../../assets/il/il4562.pdf");

/// The blank's field names, exactly as the PDF spells them.
pub mod f {
    pub const MONTH: &str = "Enter the month your tax year ends";
    pub const YEAR: &str = "Enter the year your tax year ends";
    pub const NAME: &str = "Enter your name as shown on your return";
    pub const TIN: &str = "Enter your Social Security number or federal employer identification number";
    pub const L1: &str = "Step 2, Line 1. Enter the total amount claimed as a special depreciation allowance on federal Form 4562, Depreciation and Amortization, Line 14 or Line 25, for property acquired after September 10, 2001";
    pub const L3: &str = "Step 2, Line 3. Last year of regular depreciation. Enter the total amount of all Illinois depreciation subtractions claimed on prior year IL 4562 forms, Step 3, Line 8, for this property";
    pub const L4: &str = "Step 2, Line 4. Add Lines 1 through 3. This is your Illinois special depreciation addition. Enter the total here and see instructions for the list of Illinois form and line references to report this addition";
    pub const L7A: &str = "Step 3, Line 7 \"a\" . Enter the portion of depreciation allowance claimed on federal Form 4562, for property for which you claimed bonus depreciation equal to 30 percent of your basis in the property.  See instructions";
    pub const L7C: &str = "Step 3, Line 7 \"c\" . Add Lines 7a and 7b";
    pub const L8: &str = "Step 3, Line 8. Multiply Line 7c by 42.9% or (0.429)";
    pub const L9A: &str = "Step 3, Line 9 \"a\" . Enter the portion of depreciation allowance claimed on federal Form 4562, for property for which you claimed bonus depreciation equal to 40 percent of your basis in the property.  See instructions";
    pub const L9C: &str = "Step 3, Line 9 \"c\" . Add Lines 9a and 9b";
    pub const L10: &str = "Step 3, Line 10. Multiply Line 9c by 66.7% or (0.667)";
    pub const L11A: &str = "Step 3, Line 11 \"a\" . Enter the portion of depreciation allowance claimed on federal Form 4562,  for property for which you claimed bonus depreciation equal to 50 percent of your basis in the property.  See instructions";
    pub const L11C: &str = "Step 3, Line 11 \"c\" . Add Lines 11a and 11b";
    pub const L12A: &str = "Step 3, Line 12 \"a\" . Enter the portion of depreciation allowance claimed on federal Form 4562, for property for which you claimed bonus depreciation equal to 60 percent of your basis in the property.   See instructions";
    pub const L12C: &str = "Step 3, Line 12 \"c\" .  Add Lines 12a and 12b";
    pub const L13: &str = "Step 3, Line 13. Multiply Line 12c by one and one-half (1.5)";
    pub const L14A: &str = "Step 3, Line 14 \"a\" . Enter the portion of depreciation allowance claimed on federal Form 4562, for property for which you claimed bonus depreciation equal to 80 percent of your basis in the property.   See instructions";
    pub const L14C: &str = "Step 3, Line 14 \"c\" .  Add Lines 14a and 14b";
    pub const L15: &str = "Step 3, Line 15. Multiply Line 14c by four (4)";
    pub const L16: &str = "Step 3, Line 16. Enter the amount of federal depreciation you would have claimed if you elected not to claim bonus depreciation on your federal return";
    pub const L17: &str = "Step 3, Line 17. Add Lines 8, 11c ,13 ,15, and 16";
    pub const L18: &str = "Step 3, Line 18.  Last year of reqular depreciation :  Enter the Illinois special depreciation addition reported on any prior year Form IL-4562, Step 2, Line 1 plus Line 2, for each property.   See instructions";
    pub const L19: &str = "Step 3, Line 19. Add Lines 17 and 18.  This is your Illinois depreciation subtraction for this year.  Enter the total here and see instructions for the list of Illinois form and line references to report this subtractions";
}

/// Form IL-4562's lines, in whole dollars. The "individuals only" lines (2, 7b,
/// 9b, 11b, 12b, 14b) are never a business's, so the "c" lines equal the "a" ones.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Lines {
    pub l1: i64,
    pub l3: i64,
    pub l4: i64,
    pub l7a: i64,
    pub l8: i64,
    pub l9a: i64,
    pub l10: i64,
    pub l11a: i64,
    pub l12a: i64,
    pub l13: i64,
    pub l14a: i64,
    pub l15: i64,
    pub l16: i64,
    pub l17: i64,
    pub l18: i64,
    pub l19: i64,
}

/// `dollars` times `num / den`, rounded half up — the form's products are never
/// negative.
fn times(dollars: i64, num: i64, den: i64) -> i64 {
    (dollars * num + den / 2) / den
}

impl Lines {
    /// The form's lines from the year's per-property figures.
    ///
    /// A rate the form prints no line for (anything but 30, 40, 50, 60, 80 or
    /// 100 percent) has its subtraction carried on line 16 with the 100% property,
    /// the one Step 3 line that takes an amount rather than a product.
    pub fn from_special(special: &SpecialDepreciation) -> Self {
        let (mut addition, mut last_add, mut last_sub) = (0i64, 0i64, 0i64);
        let mut regular = [0i64; 5]; // 30, 40, 50, 60, 80
        let mut line16 = 0i64;
        for r in &special.rows {
            addition += r.addition_cents;
            last_add += r.last_year_addition_cents;
            last_sub += r.last_year_subtraction_cents;
            match (r.bonus_rate * 100.0).round() as i64 {
                30 => regular[0] += r.regular_cents,
                40 => regular[1] += r.regular_cents,
                50 => regular[2] += r.regular_cents,
                60 => regular[3] += r.regular_cents,
                80 => regular[4] += r.regular_cents,
                _ => line16 += r.subtraction_cents,
            }
        }
        let [r30, r40, r50, r60, r80] = regular.map(cents_to_dollars);
        let mut l = Lines {
            l1: cents_to_dollars(addition),
            l3: cents_to_dollars(last_add),
            l7a: r30,
            l8: times(r30, 429, 1000),
            l9a: r40,
            l10: times(r40, 667, 1000),
            l11a: r50,
            l12a: r60,
            l13: times(r60, 3, 2),
            l14a: r80,
            l15: r80 * 4,
            l16: cents_to_dollars(line16),
            l18: cents_to_dollars(last_sub),
            ..Default::default()
        };
        l.l4 = l.l1 + l.l3;
        l.l17 = l.l8 + l.l10 + l.l11a + l.l13 + l.l15 + l.l16;
        l.l19 = l.l17 + l.l18;
        l
    }
}

/// Fill Form IL-4562 for the year, or `None` when there is no special
/// depreciation or the year is not the one the blank is for.
pub fn build(
    profile: &BusinessProfile,
    year: i32,
    special: &SpecialDepreciation,
) -> Result<Option<Document>, FormError> {
    if special.rows.is_empty() || year != FORM_YEAR {
        return Ok(None);
    }
    let lines = Lines::from_special(special);
    let mut doc = Document::load_mem(IL4562)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    set_text(&mut doc, &map, f::MONTH, "12")?;
    set_text(&mut doc, &map, f::YEAR, &year.to_string())?;
    set_text(&mut doc, &map, f::NAME, &profile.legal_name)?;
    set_text(&mut doc, &map, f::TIN, &profile.ein)?;

    // The totals always; the rate lines only where property of that rate exists.
    for (field, amount, always) in [
        (f::L1, lines.l1, true),
        (f::L3, lines.l3, false),
        (f::L4, lines.l4, true),
        (f::L7A, lines.l7a, false),
        (f::L7C, lines.l7a, false),
        (f::L8, lines.l8, false),
        (f::L9A, lines.l9a, false),
        (f::L9C, lines.l9a, false),
        (f::L10, lines.l10, false),
        (f::L11A, lines.l11a, false),
        (f::L11C, lines.l11a, false),
        (f::L12A, lines.l12a, false),
        (f::L12C, lines.l12a, false),
        (f::L13, lines.l13, false),
        (f::L14A, lines.l14a, false),
        (f::L14C, lines.l14a, false),
        (f::L15, lines.l15, false),
        (f::L16, lines.l16, false),
        (f::L17, lines.l17, true),
        (f::L18, lines.l18, false),
        (f::L19, lines.l19, true),
    ] {
        if always || amount != 0 {
            set_text(&mut doc, &map, field, &format_dollars(amount))?;
        }
    }
    Ok(Some(doc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::acroform::get_value;
    use crate::tax::il1065::SpecialRow;
    use chrono::NaiveDate;

    fn row(rate: f64, addition: i64, regular: i64, subtraction: i64) -> SpecialRow {
        SpecialRow {
            description: format!("{rate}"),
            placed_in_service: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
            bonus_rate: rate,
            addition_cents: addition,
            regular_cents: regular,
            factor: None,
            subtraction_cents: subtraction,
            last_year_addition_cents: 0,
            last_year_subtraction_cents: 0,
        }
    }

    /// Bunny Ears' 2025 register: each rate's regular depreciation times the
    /// form's factor, in whole dollars, and the 100% property on line 16.
    #[test]
    fn the_lines_follow_the_forms_own_arithmetic() {
        let special = SpecialDepreciation {
            rows: vec![
                row(0.8, 0, 5_997, 23_988),
                row(0.6, 0, 21_547, 32_321),
                row(0.6, 0, 82_237, 123_356),
                row(0.6, 0, 13_491, 20_237),
                row(0.4, 14_906, 3_194, 2_130),
                row(1.0, 1_662_057, 0, 55_398),
                row(1.0, 21_389, 0, 3_056),
                row(1.0, 143_007, 0, 20_430),
                row(1.0, 1_165_120, 0, 38_837),
            ],
        };
        let l = Lines::from_special(&special);
        assert_eq!(l.l1, 30_065);
        assert_eq!(l.l4, 30_065);
        assert_eq!((l.l9a, l.l10), (32, 21));
        assert_eq!((l.l12a, l.l13), (1_173, 1_760));
        assert_eq!((l.l14a, l.l15), (60, 240));
        assert_eq!(l.l16, 1_177);
        assert_eq!(l.l17, 21 + 1_760 + 240 + 1_177);
        assert_eq!(l.l19, l.l17);
    }

    #[test]
    fn every_field_named_is_on_the_blank_and_the_totals_are_written() {
        let mut doc = Document::load_mem(IL4562).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        for name in [
            f::MONTH, f::YEAR, f::NAME, f::TIN, f::L1, f::L3, f::L4, f::L7A, f::L7C, f::L8,
            f::L9A, f::L9C, f::L10, f::L11A, f::L11C, f::L12A, f::L12C, f::L13, f::L14A,
            f::L14C, f::L15, f::L16, f::L17, f::L18, f::L19,
        ] {
            assert!(map.find(name).is_some(), "no field {name:?}");
        }

        let special = SpecialDepreciation {
            rows: vec![row(0.6, 600_000, 57_140, 85_710)],
        };
        let profile = BusinessProfile {
            legal_name: "Prairie Partners LLC".into(),
            ein: "37-1234567".into(),
            ..Default::default()
        };
        let doc = build(&profile, FORM_YEAR, &special).unwrap().expect("a form");
        let map = field_map(&doc);
        assert_eq!(get_value(&doc, &map, f::L1).as_deref(), Some("6,000"));
        assert_eq!(get_value(&doc, &map, f::L12A).as_deref(), Some("571"));
        assert_eq!(get_value(&doc, &map, f::L13).as_deref(), Some("857"));
        assert_eq!(get_value(&doc, &map, f::L19).as_deref(), Some("857"));
        assert!(build(&profile, FORM_YEAR - 1, &special).unwrap().is_none());
    }
}
