//! Form 4562, "Depreciation and Amortization", filled from the asset register.
//!
//! # What this form is for, and what the 1065 does with it
//!
//! Form 4562 is the working paper the depreciation deduction is computed on. Its
//! line 22 is the total, and the instruction on the form says to "enter here and
//! on the appropriate lines of your return" — plural, because for a partnership
//! the total splits in two:
//!
//! - **§179** (line 12) goes to Schedule K line 12 and K-1 box 12, separately
//!   stated, because the dollar limit and the taxable-income limit are applied on
//!   each partner's own return.
//! - **Everything else** (bonus on line 14, MACRS on lines 17, 19 and 20) goes to
//!   page 1 line 16a.
//!
//! So line 22 is deliberately *not* the figure that reaches line 16a, and
//! [`Filled::line_16a_cents`] is the one that does. A preparer who copies line 22
//! onto line 16a has deducted the §179 twice — once at the partnership and again
//! on every partner's return — and that is the single easiest mistake this form
//! invites, so it is warned about on every 4562 produced.
//!
//! # The 2025 revision's row layout
//!
//! Section B is not the layout older revisions had. Row 19h is **50-year
//! property**, and residential rental and nonresidential real property have moved
//! down to 19i and 19j, each with two rows. The field numbering follows, so a
//! constant that named the 39-year row in an earlier revision names something
//! else here. [`every_field_this_module_names_exists_in_the_vendored_form`]
//! catches a field that has gone; only reading the form catches one that has
//! changed meaning, which is what the note in `assets/irs/README.md` is for.
//!
//! # Where one row has to carry two methods
//!
//! Line 19e is "15-year property", one row — and 15-year property is two things
//! with two different methods: land improvements at 150% declining balance, and
//! qualified improvement property straight line. When a return has both, the row
//! carries their combined basis and deduction so line 22 still foots, the method
//! column reads both, and a warning says a supporting statement has to explain
//! the split. Splitting the total across a row that does not exist would be
//! worse: the form would not add up.

use std::collections::BTreeMap;

use chrono::Datelike;

use super::acroform::{field_map, set_text, strip_xfa, FormError};
use super::depreciation::YearSchedule;
use super::lines::{cents_to_dollars, format_dollars};
use crate::domain::{BusinessProfile, Convention, Method, PropertyClass, System};
use lopdf::Document;

const F4562: &[u8] = include_bytes!("../../assets/irs/f4562.pdf");

/// The tax year the vendored form is the revision for.
pub const FORM_TAX_YEAR: i32 = 2025;

/// One tax year's Form 4562 blank.
///
/// Carried per year for a reason this form has already demonstrated: the 2025
/// revision inserted **50-year property** at row 19h, pushing residential rental
/// to 19i and nonresidential real property to 19j, and every field number after
/// row 19g moved with it. Every name still existed, so no field check could see
/// it — a 2023 return built on the 2025 blank would put residential rental on
/// the 50-year row and read as a filled-in form.
pub struct Form4562Year {
    pub year: i32,
    pub form: &'static [u8],
    pub draft: bool,
    /// Every box on this revision, named for what it means. Standalone: it
    /// references no other revision's table.
    pub boxes: &'static Boxes,
}

/// The Form 4562 revisions carried, oldest first.
pub const FORM_4562_YEARS: &[Form4562Year] = &[
    Form4562Year {
        year: 2023,
        form: include_bytes!("../../assets/irs/2023/f4562.pdf"),
        draft: false,
        boxes: &BOXES_2023,
    },
    Form4562Year {
        year: 2024,
        form: include_bytes!("../../assets/irs/2024/f4562.pdf"),
        draft: false,
        boxes: &BOXES_2024,
    },
    Form4562Year {
        year: FORM_TAX_YEAR,
        form: F4562,
        draft: false,
        boxes: &BOXES_2025,
    },
];

/// The blank for a year, or `None` when none is carried.
pub fn form_4562_year(year: i32) -> Option<&'static Form4562Year> {
    FORM_4562_YEARS.iter().find(|f| f.year == year)
}

/// The §179 dollar limit and phase-out threshold for the tax year.
///
/// 2025's figures come from the July 2025 Act, which raised the limit to
/// $2,500,000 and the threshold to $4,000,000. Both are indexed, so a later year
/// needs its own row here rather than an assumption that these hold — which is
/// why an unknown year returns `None` and the form is left for the preparer
/// rather than filled with last year's law.
fn section_179_limits(year: i32) -> Option<(i64, i64)> {
    match year {
        2023 => Some((1_160_000_00, 2_890_000_00)),
        2024 => Some((1_220_000_00, 3_050_000_00)),
        2025 => Some((2_500_000_00, 4_000_000_00)),
        _ => None,
    }
}

/// One cell of a Section B or C row.
///
/// `None` means "do not write here", and it covers both reasons that arises:
/// the column does not exist on this revision, and the column exists but the
/// IRS preprints its value — 27.5 years mid-month straight line has nowhere
/// else to go. For a writer the two are the same instruction, and collapsing
/// them means a row cannot be half-described.
///
/// # Why this is not a list of writable column indices
///
/// It was: `writable: &[usize]` beside a fixed `[&str; 6]`. Two of the three
/// index sets were right and one was not — `NO_PERIOD_OR_METHOD` wrote the
/// *method* into a column the form preprints and left the *convention* blank,
/// because `[0, 1, 4, 5]` is one position off from what its own doc comment
/// said. The IRS leaves a one-point-wide stub behind where it preprints a
/// value, so the wrong write went into a box a millimetre across and vanished.
/// A cell that is either a box or nothing cannot express that mistake.
type Cell = Option<&'static str>;

/// One row of Section B or C, by column.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// (b) month and year placed in service.
    pub month_year: Cell,
    /// (c) basis for depreciation.
    pub basis: Cell,
    /// (d) recovery period.
    pub recovery: Cell,
    /// (e) convention.
    pub convention: Cell,
    /// (f) method.
    pub method: Cell,
    /// (g) depreciation deduction.
    pub deduction: Cell,
}

/// Section B — assets placed in service this year under the general system.
///
/// Keyed by property class rather than by row index. The index was a per-revision
/// fact pretending to be a shared one: residential rental is the eighth row on
/// the 2025 form and the seventh on the 2023 form, because 2025 inserted 50-year
/// property above it.
#[derive(Debug, Clone, Copy)]
pub struct SectionB {
    pub three_year: Row,
    pub five_year: Row,
    pub seven_year: Row,
    pub ten_year: Row,
    /// Both 15-year classes share this row, which is why it may carry two methods.
    pub fifteen_year: Row,
    pub twenty_year: Row,
    pub twenty_five_year: Row,
    /// Added by the 2025 revision. `None` on every earlier one.
    pub fifty_year: Option<Row>,
    /// The form prints two rows for each real-property class, because the
    /// mid-month convention makes two buildings bought in different months
    /// genuinely different rows.
    pub residential_rental: [Row; 2],
    pub nonresidential_real: [Row; 2],
}

/// Section C — the alternative depreciation system, organised by recovery period.
#[derive(Debug, Clone, Copy)]
pub struct SectionC {
    pub class_life: Row,
    pub twelve_year: Row,
    pub thirty_year: Row,
    pub forty_year: Row,
    /// Added by the 2025 revision, like Section B's.
    pub fifty_year: Option<Row>,
}

/// Every box one revision of Form 4562 offers, named for what it means.
///
/// One of these per revision, each naming only its own PDF. No revision is
/// expressed as a difference from another: the 2023 form calls the first column
/// of row 19a `R4[0]` and the 2025 form calls it `f1_26[0]`, and there is no
/// rename table that turns one into the other without also knowing that the
/// `f1_*` numbering downstream shifts by one because `R4` does not consume a
/// number.
#[derive(Debug, Clone, Copy)]
pub struct Boxes {
    pub name: &'static str,
    pub activity: &'static str,
    pub ein: &'static str,
    pub l1_maximum: &'static str,
    pub l2_total_cost: &'static str,
    pub l3_threshold: &'static str,
    pub l4_reduction: &'static str,
    pub l5_dollar_limit: &'static str,
    /// Line 6: description, cost, elected cost. Two printed rows.
    pub l6_rows: [[&'static str; 3]; 2],
    pub l8_total_elected: &'static str,
    pub l9_tentative: &'static str,
    /// Line 10, carryover of disallowed §179 from the prior year. Filled only for
    /// a sole proprietor: a partnership's limits are applied on each partner's
    /// return, so its own line 10 has nothing to say.
    pub l10_carryover_in: &'static str,
    /// Line 11, the business income limitation. Sole proprietor only, as above.
    pub l11_income_limit: &'static str,
    pub l12_deduction: &'static str,
    /// Line 13, carryover of disallowed §179 to next year — the inner column,
    /// left of the amounts. Sole proprietor only.
    pub l13_carryover_out: &'static str,
    pub l14_bonus: &'static str,
    pub l17_prior_years: &'static str,
    /// Part IV's total. On page 2 from the 2025 revision; on page 1 before it,
    /// which is why this is a box name and not a page-and-number.
    pub l22_total: &'static str,
    pub section_b: SectionB,
    pub section_c: SectionC,
}

/// Which Section B row a class reports on, and the overflow row where the form
/// prints one.
fn section_b_rows(boxes: &SectionB, class: PropertyClass) -> &[Row] {
    match class {
        PropertyClass::ThreeYear => std::slice::from_ref(&boxes.three_year),
        PropertyClass::FiveYear => std::slice::from_ref(&boxes.five_year),
        PropertyClass::SevenYear => std::slice::from_ref(&boxes.seven_year),
        PropertyClass::TenYear => std::slice::from_ref(&boxes.ten_year),
        PropertyClass::FifteenYearLandImprovement | PropertyClass::QualifiedImprovement => {
            std::slice::from_ref(&boxes.fifteen_year)
        }
        PropertyClass::TwentyYear => std::slice::from_ref(&boxes.twenty_year),
        PropertyClass::TwentyFiveYear => std::slice::from_ref(&boxes.twenty_five_year),
        PropertyClass::ResidentialRental => &boxes.residential_rental,
        PropertyClass::Nonresidential => &boxes.nonresidential_real,
    }
}

/// Which Section C row an ADS class reports on.
///
/// Section C is organised by recovery period rather than by class, because ADS
/// flattens the classes into lives: everything without a row of its own goes to
/// "class life", with its period written in. A period this revision has no row
/// for comes back `None` rather than falling to class life, because a 50-year
/// asset on a form printed before the 50-year row existed is something the filer
/// has to be told about.
fn section_c_row(boxes: &SectionC, class: PropertyClass) -> Option<&Row> {
    match class.recovery_years(System::Ads) as i64 {
        12 => Some(&boxes.twelve_year),
        30 => Some(&boxes.thirty_year),
        40 => Some(&boxes.forty_year),
        50 => boxes.fifty_year.as_ref(),
        _ => Some(&boxes.class_life),
    }
}

pub const BOXES_2025: Boxes = Boxes {
    name: "f1_1[0]",
    activity: "f1_2[0]",
    ein: "f1_3[0]",
    l1_maximum: "f1_4[0]",
    l2_total_cost: "f1_5[0]",
    l3_threshold: "f1_6[0]",
    l4_reduction: "f1_7[0]",
    l5_dollar_limit: "f1_8[0]",
    l8_total_elected: "f1_16[0]",
    l9_tentative: "f1_17[0]",
    l10_carryover_in: "f1_18[0]",
    l11_income_limit: "f1_19[0]",
    l12_deduction: "f1_20[0]",
    l13_carryover_out: "f1_21[0]",
    l14_bonus: "f1_22[0]",
    l17_prior_years: "f1_25[0]",
    l6_rows: [
        ["f1_9[0]", "f1_10[0]", "f1_11[0]"],
        ["f1_12[0]", "f1_13[0]", "f1_14[0]"],
    ],
    l22_total: "f2_2[0]",
    section_b: SectionB {
        three_year: Row {
            month_year: Some("f1_26[0]"),
            basis: Some("f1_27[0]"),
            recovery: Some("f1_28[0]"),
            convention: Some("f1_29[0]"),
            method: Some("f1_30[0]"),
            deduction: Some("f1_31[0]"),
        },
        five_year: Row {
            month_year: Some("f1_32[0]"),
            basis: Some("f1_33[0]"),
            recovery: Some("f1_34[0]"),
            convention: Some("f1_35[0]"),
            method: Some("f1_36[0]"),
            deduction: Some("f1_37[0]"),
        },
        seven_year: Row {
            month_year: Some("f1_38[0]"),
            basis: Some("f1_39[0]"),
            recovery: Some("f1_40[0]"),
            convention: Some("f1_41[0]"),
            method: Some("f1_42[0]"),
            deduction: Some("f1_43[0]"),
        },
        ten_year: Row {
            month_year: Some("f1_44[0]"),
            basis: Some("f1_45[0]"),
            recovery: Some("f1_46[0]"),
            convention: Some("f1_47[0]"),
            method: Some("f1_48[0]"),
            deduction: Some("f1_49[0]"),
        },
        fifteen_year: Row {
            month_year: Some("f1_50[0]"),
            basis: Some("f1_51[0]"),
            recovery: Some("f1_52[0]"),
            convention: Some("f1_53[0]"),
            method: Some("f1_54[0]"),
            deduction: Some("f1_55[0]"),
        },
        twenty_year: Row {
            month_year: Some("f1_56[0]"),
            basis: Some("f1_57[0]"),
            recovery: Some("f1_58[0]"),
            convention: Some("f1_59[0]"),
            method: Some("f1_60[0]"),
            deduction: Some("f1_61[0]"),
        },
        twenty_five_year: Row {
            month_year: Some("f1_62[0]"),
            basis: Some("f1_63[0]"),
            recovery: None,
            convention: Some("f1_65[0]"),
            method: None,
            deduction: Some("f1_67[0]"),
        },
        fifty_year: Some(Row {
            month_year: Some("f1_68[0]"),
            basis: Some("f1_69[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_73[0]"),
        }),
        residential_rental: [
            Row {
                month_year: Some("f1_74[0]"),
                basis: Some("f1_75[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_79[0]"),
            },
            Row {
                month_year: Some("f1_80[0]"),
                basis: Some("f1_81[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_85[0]"),
            },
        ],
        nonresidential_real: [
            Row {
                month_year: Some("f1_86[0]"),
                basis: Some("f1_87[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_91[0]"),
            },
            Row {
                month_year: Some("f1_92[0]"),
                basis: Some("f1_93[0]"),
                recovery: Some("f1_94[0]"),
                convention: None,
                method: None,
                deduction: Some("f1_97[0]"),
            },
        ],
    },
    section_c: SectionC {
        class_life: Row {
            month_year: Some("f1_98[0]"),
            basis: Some("f1_99[0]"),
            recovery: Some("f1_100[0]"),
            convention: Some("f1_101[0]"),
            method: None,
            deduction: Some("f1_103[0]"),
        },
        twelve_year: Row {
            month_year: Some("f1_104[0]"),
            basis: Some("f1_105[0]"),
            recovery: None,
            convention: Some("f1_107[0]"),
            method: None,
            deduction: Some("f1_109[0]"),
        },
        thirty_year: Row {
            month_year: Some("f1_110[0]"),
            basis: Some("f1_111[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_115[0]"),
        },
        forty_year: Row {
            month_year: Some("f1_116[0]"),
            basis: Some("f1_117[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_121[0]"),
        },
        fifty_year: Some(Row {
            month_year: Some("f1_122[0]"),
            basis: Some("f1_123[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_127[0]"),
        }),
    },
};

pub const BOXES_2023: Boxes = Boxes {
    name: "f1_1[0]",
    activity: "f1_2[0]",
    ein: "f1_3[0]",
    l1_maximum: "f1_4[0]",
    l2_total_cost: "f1_5[0]",
    l3_threshold: "f1_6[0]",
    l4_reduction: "f1_7[0]",
    l5_dollar_limit: "f1_8[0]",
    l8_total_elected: "f1_16[0]",
    l9_tentative: "f1_17[0]",
    l10_carryover_in: "f1_18[0]",
    l11_income_limit: "f1_19[0]",
    l12_deduction: "f1_20[0]",
    l13_carryover_out: "f1_21[0]",
    l14_bonus: "f1_22[0]",
    l17_prior_years: "f1_25[0]",
    l6_rows: [
        ["f1_9[0]", "f1_10[0]", "f1_11[0]"],
        ["f1_12[0]", "f1_13[0]", "f1_14[0]"],
    ],
    l22_total: "f1_108[0]",
    section_b: SectionB {
        three_year: Row {
            month_year: Some("R4[0]"),
            basis: Some("f1_26[0]"),
            recovery: Some("f1_27[0]"),
            convention: Some("f1_28[0]"),
            method: Some("f1_29[0]"),
            deduction: Some("f1_30[0]"),
        },
        five_year: Row {
            month_year: Some("R5[0]"),
            basis: Some("f1_31[0]"),
            recovery: Some("f1_32[0]"),
            convention: Some("f1_33[0]"),
            method: Some("f1_34[0]"),
            deduction: Some("f1_35[0]"),
        },
        seven_year: Row {
            month_year: Some("R6[0]"),
            basis: Some("f1_36[0]"),
            recovery: Some("f1_37[0]"),
            convention: Some("f1_38[0]"),
            method: Some("f1_39[0]"),
            deduction: Some("f1_40[0]"),
        },
        ten_year: Row {
            month_year: Some("R7[0]"),
            basis: Some("f1_41[0]"),
            recovery: Some("f1_42[0]"),
            convention: Some("f1_43[0]"),
            method: Some("f1_44[0]"),
            deduction: Some("f1_45[0]"),
        },
        fifteen_year: Row {
            month_year: Some("R8[0]"),
            basis: Some("f1_46[0]"),
            recovery: Some("f1_47[0]"),
            convention: Some("f1_48[0]"),
            method: Some("f1_49[0]"),
            deduction: Some("f1_50[0]"),
        },
        twenty_year: Row {
            month_year: Some("R9[0]"),
            basis: Some("f1_51[0]"),
            recovery: Some("f1_52[0]"),
            convention: Some("f1_53[0]"),
            method: Some("f1_54[0]"),
            deduction: Some("f1_55[0]"),
        },
        twenty_five_year: Row {
            month_year: Some("R10[0]"),
            basis: Some("f1_56[0]"),
            recovery: None,
            convention: Some("f1_58[0]"),
            method: None,
            deduction: Some("f1_60[0]"),
        },
        fifty_year: None,
        residential_rental: [
            Row {
                month_year: Some("f1_61[0]"),
                basis: Some("f1_62[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_66[0]"),
            },
            Row {
                month_year: Some("f1_67[0]"),
                basis: Some("f1_68[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_72[0]"),
            },
        ],
        nonresidential_real: [
            Row {
                month_year: Some("f1_73[0]"),
                basis: Some("f1_74[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_78[0]"),
            },
            Row {
                month_year: Some("f1_79[0]"),
                basis: Some("f1_80[0]"),
                recovery: Some("f1_81[0]"),
                convention: None,
                method: None,
                deduction: Some("f1_84[0]"),
            },
        ],
    },
    section_c: SectionC {
        class_life: Row {
            month_year: Some("R11[0]"),
            basis: Some("f1_85[0]"),
            recovery: Some("f1_86[0]"),
            convention: Some("f1_87[0]"),
            method: None,
            deduction: Some("f1_89[0]"),
        },
        twelve_year: Row {
            month_year: Some("R12[0]"),
            basis: Some("f1_90[0]"),
            recovery: None,
            convention: Some("f1_92[0]"),
            method: None,
            deduction: Some("f1_94[0]"),
        },
        thirty_year: Row {
            month_year: Some("f1_95[0]"),
            basis: Some("f1_96[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_100[0]"),
        },
        forty_year: Row {
            month_year: Some("f1_101[0]"),
            basis: Some("f1_102[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_106[0]"),
        },
        fifty_year: None,
    },
};

pub const BOXES_2024: Boxes = Boxes {
    name: "f1_1[0]",
    activity: "f1_2[0]",
    ein: "f1_3[0]",
    l1_maximum: "f1_4[0]",
    l2_total_cost: "f1_5[0]",
    l3_threshold: "f1_6[0]",
    l4_reduction: "f1_7[0]",
    l5_dollar_limit: "f1_8[0]",
    l8_total_elected: "f1_16[0]",
    l9_tentative: "f1_17[0]",
    l10_carryover_in: "f1_18[0]",
    l11_income_limit: "f1_19[0]",
    l12_deduction: "f1_20[0]",
    l13_carryover_out: "f1_21[0]",
    l14_bonus: "f1_22[0]",
    l17_prior_years: "f1_25[0]",
    l6_rows: [
        ["f1_9[0]", "f1_10[0]", "f1_11[0]"],
        ["f1_12[0]", "f1_13[0]", "f1_14[0]"],
    ],
    l22_total: "f1_108[0]",
    section_b: SectionB {
        three_year: Row {
            month_year: Some("R4[0]"),
            basis: Some("f1_26[0]"),
            recovery: Some("f1_27[0]"),
            convention: Some("f1_28[0]"),
            method: Some("f1_29[0]"),
            deduction: Some("f1_30[0]"),
        },
        five_year: Row {
            month_year: Some("R5[0]"),
            basis: Some("f1_31[0]"),
            recovery: Some("f1_32[0]"),
            convention: Some("f1_33[0]"),
            method: Some("f1_34[0]"),
            deduction: Some("f1_35[0]"),
        },
        seven_year: Row {
            month_year: Some("R6[0]"),
            basis: Some("f1_36[0]"),
            recovery: Some("f1_37[0]"),
            convention: Some("f1_38[0]"),
            method: Some("f1_39[0]"),
            deduction: Some("f1_40[0]"),
        },
        ten_year: Row {
            month_year: Some("R7[0]"),
            basis: Some("f1_41[0]"),
            recovery: Some("f1_42[0]"),
            convention: Some("f1_43[0]"),
            method: Some("f1_44[0]"),
            deduction: Some("f1_45[0]"),
        },
        fifteen_year: Row {
            month_year: Some("R8[0]"),
            basis: Some("f1_46[0]"),
            recovery: Some("f1_47[0]"),
            convention: Some("f1_48[0]"),
            method: Some("f1_49[0]"),
            deduction: Some("f1_50[0]"),
        },
        twenty_year: Row {
            month_year: Some("R9[0]"),
            basis: Some("f1_51[0]"),
            recovery: Some("f1_52[0]"),
            convention: Some("f1_53[0]"),
            method: Some("f1_54[0]"),
            deduction: Some("f1_55[0]"),
        },
        twenty_five_year: Row {
            month_year: Some("R10[0]"),
            basis: Some("f1_56[0]"),
            recovery: None,
            convention: Some("f1_58[0]"),
            method: None,
            deduction: Some("f1_60[0]"),
        },
        fifty_year: None,
        residential_rental: [
            Row {
                month_year: Some("f1_61[0]"),
                basis: Some("f1_62[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_66[0]"),
            },
            Row {
                month_year: Some("f1_67[0]"),
                basis: Some("f1_68[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_72[0]"),
            },
        ],
        nonresidential_real: [
            Row {
                month_year: Some("f1_73[0]"),
                basis: Some("f1_74[0]"),
                recovery: None,
                convention: None,
                method: None,
                deduction: Some("f1_78[0]"),
            },
            Row {
                month_year: Some("f1_79[0]"),
                basis: Some("f1_80[0]"),
                recovery: Some("f1_81[0]"),
                convention: None,
                method: None,
                deduction: Some("f1_84[0]"),
            },
        ],
    },
    section_c: SectionC {
        class_life: Row {
            month_year: Some("R11[0]"),
            basis: Some("f1_85[0]"),
            recovery: Some("f1_86[0]"),
            convention: Some("f1_87[0]"),
            method: None,
            deduction: Some("f1_89[0]"),
        },
        twelve_year: Row {
            month_year: Some("R12[0]"),
            basis: Some("f1_90[0]"),
            recovery: None,
            convention: Some("f1_92[0]"),
            method: None,
            deduction: Some("f1_94[0]"),
        },
        thirty_year: Row {
            month_year: Some("f1_95[0]"),
            basis: Some("f1_96[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_100[0]"),
        },
        forty_year: Row {
            month_year: Some("f1_101[0]"),
            basis: Some("f1_102[0]"),
            recovery: None,
            convention: None,
            method: None,
            deduction: Some("f1_106[0]"),
        },
        fifty_year: None,
    },
};

/// One line of Section B or C as it will be printed.
#[derive(Debug, Clone, Default)]
struct Group {
    basis_cents: i64,
    deduction_cents: i64,
    /// The earliest month any asset in the group was placed in service. Only
    /// meaningful for the mid-month rows, where it is the whole point.
    month: Option<(i32, u32)>,
    recovery: String,
    convention: String,
    methods: Vec<&'static str>,
}

fn convention_label(c: Convention) -> &'static str {
    match c {
        Convention::HalfYear => "HY",
        Convention::MidQuarter => "MQ",
        Convention::MidMonth => "MM",
    }
}

fn method_label(m: Method) -> &'static str {
    match m {
        Method::DecliningBalance { factor } if factor >= 2.0 => "200DB",
        Method::DecliningBalance { .. } => "150DB",
        Method::StraightLine => "S/L",
    }
}

/// A recovery period as the column prints it: "7" or "27.5".
fn recovery_label(years: f64) -> String {
    if (years - years.round()).abs() < 1e-9 {
        format!("{}", years.round() as i64)
    } else {
        format!("{years}")
    }
}

/// Form 4562, and the figures the rest of the return needs from it.
pub struct Filled {
    pub document: Document,
    /// Line 12 — the §179 deduction, for Schedule K line 12 and K-1 box 12.
    pub section_179_cents: i64,
    /// Line 22 as the form computes it, §179 included. Not the figure for page
    /// 1 line 16a — see [`Self::line_16a_cents`].
    pub line_22_cents: i64,
    /// What page 1 line 16a should carry: line 22 less the §179 on line 12.
    pub line_16a_cents: i64,
}

/// Who files the return this Form 4562 is attached to.
///
/// It decides two things the form cannot decide for itself: whose name and number
/// head it, and whether the §179 limits are applied here at all.
#[derive(Debug, Clone, Copy)]
pub enum Filer<'a> {
    /// Form 1065. The dollar and income limits on §179 are applied on each
    /// partner's own return, so lines 10, 11 and 13 stay blank here and line 12
    /// is reported separately on Schedule K.
    Partnership,
    /// Schedule C, attached to the owner's Form 1040. The form is headed with the
    /// owner's name and SSN — the numbers the IRS pairs the return by — and the
    /// whole of Part I is applied here, because there is nobody downstream to
    /// apply it.
    SoleProprietor {
        name: &'a str,
        ssn: Option<&'a str>,
        limit: Section179Limit,
    },
}

/// The two Part I inputs a ledger cannot supply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Section179Limit {
    /// Line 10: §179 disallowed last year and carried into this one.
    pub carryover_in_cents: i64,
    /// Line 11: taxable income from every active trade or business, wages
    /// included, figured *without* the §179 deduction.
    pub business_income_cents: i64,
}

/// Part I worked through, line by line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section179Outcome {
    /// Line 8: elected across the register.
    pub elected_cents: i64,
    /// Line 9: the elected amount within the dollar limit.
    pub tentative_cents: i64,
    /// Line 10.
    pub carryover_in_cents: i64,
    /// Line 11.
    pub business_income_cents: i64,
    /// Line 12: what is deducted this year.
    pub allowed_cents: i64,
    /// Line 13: what carries to next year.
    pub carryover_out_cents: i64,
}

/// Lines 1 to 5 for a year: the maximum, the cost of §179 property placed in
/// service, the threshold, the reduction, and the dollar limit. `None` for a year
/// whose figures this program does not carry.
fn dollar_limit(schedule: &YearSchedule<'_>) -> Option<(i64, i64, i64, i64, i64)> {
    let (maximum, threshold) = section_179_limits(schedule.tax_year)?;
    // Line 2 is the cost of *all* §179 property placed in service, not only the
    // part elected — it is what the phase-out is measured against.
    let total_cost: i64 = schedule
        .placed_this_year()
        .filter(|r| r.asset.class.section_179() != crate::domain::Section179Eligibility::NotEligible)
        .map(|r| r.asset.cost_cents)
        .sum();
    let reduction = (total_cost - threshold).max(0);
    Some((maximum, total_cost, threshold, reduction, (maximum - reduction).max(0)))
}

/// Part I of Form 4562 for a sole proprietor: how much of the §179 elected is
/// deducted this year, and how much waits.
///
/// Line 12 is the lesser of what is available (line 9 plus last year's
/// carryover) and the business income limit, which can never be negative — §179
/// cannot make a loss, or add to one. The rest is line 13. Where the year's
/// dollar limit is not known, line 9 is the whole election and the form says so.
pub fn section_179_outcome(
    schedule: &YearSchedule<'_>,
    limit: Section179Limit,
) -> Section179Outcome {
    let elected = schedule.section_179_cents();
    let tentative = match dollar_limit(schedule) {
        Some((.., limit)) => limit.min(elected),
        None => elected,
    };
    let available = tentative + limit.carryover_in_cents.max(0);
    let allowed = available.min(limit.business_income_cents.max(0));
    Section179Outcome {
        elected_cents: elected,
        tentative_cents: tentative,
        carryover_in_cents: limit.carryover_in_cents.max(0),
        business_income_cents: limit.business_income_cents,
        allowed_cents: allowed,
        carryover_out_cents: available - allowed,
    }
}

/// Build Form 4562 from a computed year, or `None` when the register is empty.
///
/// An empty register is not the same as no depreciation: a partnership can have
/// depreciation posted straight to the ledger by hand and no assets entered here.
/// Returning `None` says only that this module found nothing to report, and the
/// caller decides what that means against what the ledger says.
pub fn build(
    profile: &BusinessProfile,
    schedule: &YearSchedule<'_>,
    activity: &str,
    year: i32,
    filer: Filer<'_>,
) -> Result<(Option<Filled>, Vec<String>), FormError> {
    let mut warnings = Vec::new();

    if schedule.rows.is_empty() {
        return Ok((None, warnings));
    }

    // No form rather than the wrong year's form.
    //
    // Between the 2024 and 2025 revisions 186 of this form's 277 boxes changed
    // position and 65 stopped existing — the 2025 revision inserted 50-year
    // property at row 19h and everything after row 19g moved down with it. A
    // 2023 Form 4562 filled on the 2025 blank puts residential rental property
    // on the 50-year row, totals correctly, and looks finished.
    //
    // The depreciation figure still reaches the return: page 1 line 16 comes
    // from the ledger, not from this form. What is missing is the schedule
    // behind it, and the warning says so plainly enough to act on.
    // The year's own blank and the year's own boxes. A revision this program has
    // not transcribed is not filled on a neighbouring year's form: the boxes move
    // — Part IV's total is on page 1 before the 2025 revision and page 2 after it
    // — so the figures would land in real boxes on the wrong part and the form
    // would total correctly while being wrong.
    //
    // The return itself is unaffected either way: page 1 line 16 comes from the
    // ledger, not from this form.
    let Some(revision) = form_4562_year(year) else {
        warnings.push(format!(
            "No Form 4562 is attached: this program does not carry the {year} revision of it. \
             The depreciation figure on the return comes from the ledger and is unaffected — \
             fill the {year} Form 4562 by hand and attach it. Revisions carried: {}.",
            FORM_4562_YEARS
                .iter()
                .map(|r| r.year.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return Ok((None, warnings));
    };
    let boxes = revision.boxes;
    if revision.draft {
        warnings.push(format!(
            "The {} Form 4562 is an IRS draft, which may not be filed. Anything produced on it \
             is a projection until the final form is published.",
            revision.year
        ));
    }
    let mut doc = Document::load_mem(revision.form)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    // The name and number of the return this is attached to: the partnership's,
    // or — on a Schedule C — the owner's, as on their Form 1040.
    match filer {
        Filer::Partnership => {
            set_text(&mut doc, &map, boxes.name, &profile.legal_name)?;
            set_text(&mut doc, &map, boxes.ein, &profile.ein)?;
        }
        Filer::SoleProprietor { name, ssn, .. } => {
            set_text(&mut doc, &map, boxes.name, name)?;
            if let Some(ssn) = ssn {
                set_text(&mut doc, &map, boxes.ein, ssn)?;
            }
        }
    }
    set_text(&mut doc, &map, boxes.activity, activity)?;

    // --- Part I: §179 -----------------------------------------------------
    let elected: Vec<(&str, i64, i64)> = schedule
        .rows
        .iter()
        .filter(|r| r.section_179_cents > 0)
        .map(|r| {
            (
                r.asset.description.as_str(),
                r.asset.cost_cents,
                r.section_179_cents,
            )
        })
        .collect();

    let total_elected: i64 = elected.iter().map(|(_, _, e)| e).sum();
    let sole_limit = match filer {
        Filer::SoleProprietor { limit, .. } => Some(limit),
        Filer::Partnership => None,
    };
    // A sole proprietor with last year's carryover fills Part I even in a year
    // that elects nothing new: lines 10 to 13 are where the carryover is used.
    let part_i = !elected.is_empty() || sole_limit.is_some_and(|l| l.carryover_in_cents > 0);
    let outcome = sole_limit.map(|l| section_179_outcome(schedule, l));

    if part_i {
        match dollar_limit(schedule) {
            Some((maximum, total_cost, threshold, reduction, dollar_limit)) => {
                let tentative = dollar_limit.min(total_elected);

                set_text(&mut doc, &map, boxes.l1_maximum, &money(maximum))?;
                set_text(&mut doc, &map, boxes.l2_total_cost, &money(total_cost))?;
                set_text(&mut doc, &map, boxes.l3_threshold, &money(threshold))?;
                set_text(&mut doc, &map, boxes.l4_reduction, &money(reduction))?;
                set_text(&mut doc, &map, boxes.l5_dollar_limit, &money(dollar_limit))?;
                set_text(&mut doc, &map, boxes.l9_tentative, &money(tentative))?;
                if outcome.is_none() {
                    set_text(&mut doc, &map, boxes.l12_deduction, &money(tentative))?;
                }

                if tentative < total_elected {
                    warnings.push(format!(
                        "Form 4562: {} of §179 is elected across the register, but the dollar \
                         limit for {} allows {}. Line 9 carries the limit, and the difference \
                         is not deductible this year.",
                        money(total_elected),
                        schedule.tax_year,
                        money(tentative)
                    ));
                }
            }
            None => warnings.push(format!(
                "Form 4562: the §179 dollar limit and phase-out threshold for {} are not known \
                 to this program, so Part I lines 1 to 5 are blank and line 9 is the whole \
                 election. Both figures are indexed each year — fill them from the {} \
                 instructions.",
                schedule.tax_year, schedule.tax_year
            )),
        }

        set_text(
            &mut doc,
            &map,
            boxes.l8_total_elected,
            &money(total_elected),
        )?;

        for (row, (description, cost, elected_cost)) in elected.iter().take(2).enumerate() {
            let cols = boxes.l6_rows[row];
            set_text(&mut doc, &map, cols[0], description)?;
            set_text(&mut doc, &map, cols[1], &money(*cost))?;
            set_text(&mut doc, &map, cols[2], &money(*elected_cost))?;
        }
        if elected.len() > 2 {
            warnings.push(format!(
                "Form 4562: {} assets elect §179 and line 6 has two printed rows. The first two \
                 are listed; the rest need a supporting statement, which this program does not \
                 produce. The totals on lines 8 and 12 include all of them.",
                elected.len()
            ));
        }

        match outcome {
            // A sole proprietor's limits are applied here, so the lines that apply
            // them are filled rather than left to a partner.
            Some(o) => {
                if o.carryover_in_cents != 0 {
                    set_text(&mut doc, &map, boxes.l10_carryover_in, &money(o.carryover_in_cents))?;
                }
                set_text(
                    &mut doc,
                    &map,
                    boxes.l11_income_limit,
                    &money(o.business_income_cents.max(0)),
                )?;
                set_text(&mut doc, &map, boxes.l12_deduction, &money(o.allowed_cents))?;
                if o.carryover_out_cents != 0 {
                    set_text(&mut doc, &map, boxes.l13_carryover_out, &money(o.carryover_out_cents))?;
                }
            }
            // The two inputs a ledger cannot supply. Both cap line 12, so a return
            // filed without checking them can claim more than the statute allows.
            None => warnings.push(
                "Form 4562 line 10 (carryover of disallowed §179 from the prior year) and line \
                 11 (the business income limitation) are left blank — neither is in the books. \
                 Line 12 is filled as the tentative deduction on line 9, which is correct only \
                 when there is no carryover and business income covers it. Check both before \
                 filing."
                    .to_string(),
            ),
        }
    }

    // --- Part II: bonus ---------------------------------------------------
    let bonus = schedule.bonus_cents();
    if bonus != 0 {
        set_text(&mut doc, &map, boxes.l14_bonus, &money(bonus))?;
    }

    // --- Part III Section A: assets from earlier years --------------------
    //
    // Everything not in its first recovery year. Section B is current-year
    // acquisitions only, so without this line the deduction on assets bought in
    // earlier years would vanish from the form while still being claimed.
    let prior: i64 = schedule
        .rows
        .iter()
        .filter(|r| r.recovery_year > 1)
        .map(|r| r.macrs_cents)
        .sum();
    if prior != 0 {
        set_text(&mut doc, &map, boxes.l17_prior_years, &money(prior))?;
    }

    // --- Part III Sections B and C: this year's acquisitions --------------
    let (section_b, section_c) = group_current_year(schedule, boxes);
    let mut current_year_total = 0i64;

    for (class, slot, group) in &section_b {
        current_year_total += group.deduction_cents;
        let row = &section_b_rows(&boxes.section_b, *class)[*slot];
        warnings.extend(write_group(&mut doc, &map, row, group, "19")?);
    }
    for (class, group) in &section_c {
        current_year_total += group.deduction_cents;
        match section_c_row(&boxes.section_c, *class) {
            Some(row) => warnings.extend(write_group(&mut doc, &map, row, group, "20")?),
            // A recovery period this revision prints no row for. The deduction
            // still counts toward line 22 — it is real money the ledger computed
            // — but the row that would show it does not exist on this form, so
            // it has to be said rather than silently dropped into class life.
            None => warnings.push(format!(
                "Form 4562 line 20: {} of ADS depreciation has a {}-year recovery period, and \
                 the {} revision of this form prints no row for it. The amount is included in \
                 line 22; add the row by hand.",
                money(group.deduction_cents),
                class.recovery_years(System::Ads),
                revision.year
            )),
        }
    }

    // --- Part IV: the summary --------------------------------------------
    //
    // Line 22 is what the form says it is: line 12 plus 14 through 17 plus the
    // (g) columns. It is *not* what page 1 line 16a takes, which is why the two
    // are reported separately below.
    let line_12 = match outcome {
        Some(o) if part_i => o.allowed_cents,
        _ if elected.is_empty() => 0,
        _ => schedule.section_179_cents(),
    };
    let line_22 = line_12 + bonus + prior + current_year_total;
    set_text(&mut doc, &map, boxes.l22_total, &money(line_22))?;

    let line_16a = line_22 - line_12;
    // A sole proprietor's line 22 is exactly what Schedule C line 13 takes, so
    // the warning below — about a partnership splitting it — would be wrong there.
    if line_12 != 0 && matches!(filer, Filer::Partnership) {
        warnings.push(format!(
            "Form 4562 line 22 is {}, and that is not the figure for page 1 line 16a. It \
             includes the {} of §179 on line 12, which a partnership reports separately on \
             Schedule K line 12 and K-1 box 12 — each partner applies their own dollar and \
             taxable-income limits to it. Line 16a takes {}. Copying line 22 onto line 16a \
             deducts the §179 twice.",
            money(line_22),
            money(line_12),
            money(line_16a)
        ));
    }

    Ok((
        Some(Filled {
            document: doc,
            section_179_cents: line_12,
            line_22_cents: line_22,
            line_16a_cents: line_16a,
        }),
        warnings,
    ))
}

/// Group this year's acquisitions into the rows the form prints.
///
/// Grouped by class and system, because that is what a row is. Real property is
/// grouped by month as well: those rows take the mid-month convention, so the
/// month placed in service is part of the computation rather than a label, and
/// two buildings bought in different months genuinely belong on different rows —
/// which is why the form prints two of each.
///
/// Keyed by class and slot rather than by row index. The index was a fact about
/// one revision's page layout, and using it as the shared key is what made
/// residential rental land on the 50-year row when the form was not the one the
/// indices were counted on.
fn group_current_year(
    schedule: &YearSchedule<'_>,
    boxes: &Boxes,
) -> (
    Vec<(PropertyClass, usize, Group)>,
    Vec<(PropertyClass, Group)>,
) {
    let mut b: BTreeMap<(u8, Option<u32>), (PropertyClass, Group)> = BTreeMap::new();
    let mut c: BTreeMap<(u8, Option<u32>), (PropertyClass, Group)> = BTreeMap::new();

    for row in schedule.placed_this_year() {
        let asset = row.asset;
        // Property the §179 deduction or bonus depreciation took in full leaves no
        // basis for a Section B or C row to depreciate. Its deduction is on line 12
        // or 14; a row of zeros beside it says nothing and reads as an asset left
        // undepreciated.
        if row.macrs_basis_cents == 0 && row.macrs_cents == 0 {
            continue;
        }
        let table = match asset.system {
            System::Gds => &mut b,
            System::Ads => &mut c,
        };
        // Only the mid-month rows separate by month; everything else shares one
        // convention across the year and belongs on one row.
        let key_month = asset
            .class
            .uses_mid_month()
            .then(|| asset.placed_in_service.month());

        let entry = table
            .entry((class_ord(asset.class), key_month))
            .or_insert_with(|| (asset.class, Group::default()));
        let group = &mut entry.1;
        group.basis_cents += row.macrs_basis_cents;
        group.deduction_cents += row.macrs_cents;

        let placed = (
            asset.placed_in_service.year(),
            asset.placed_in_service.month(),
        );
        group.month = Some(match group.month {
            Some(existing) if existing <= placed => existing,
            _ => placed,
        });
        group.recovery = recovery_label(asset.class.recovery_years(asset.system));
        group.convention = convention_label(row.convention).to_string();

        let method = method_label(asset.class.method(asset.system));
        if !group.methods.contains(&method) {
            group.methods.push(method);
        }
    }

    // Collapse the (class, month) keys onto the rows this revision actually
    // prints, spilling to the second printed row where there is one.
    let mut section_b: Vec<(PropertyClass, usize, Group)> = Vec::new();
    for (_, (class, group)) in b {
        let available = section_b_rows(&boxes.section_b, class).len();
        let used = section_b.iter().filter(|(c, _, _)| *c == class).count();
        if used < available {
            section_b.push((class, used, group));
        } else {
            // A figure merged still totals; a figure dropped does not.
            let existing = section_b
                .iter_mut()
                .find(|(c, slot, _)| *c == class && *slot == 0)
                .expect("the first slot was filled, so it is in the output");
            merge(&mut existing.2, group);
        }
    }

    let mut section_c: Vec<(PropertyClass, Group)> = Vec::new();
    for (_, (class, group)) in c {
        match section_c
            .iter_mut()
            .find(|(existing, _)| *existing == class)
        {
            Some(slot) => merge(&mut slot.1, group),
            None => section_c.push((class, group)),
        }
    }

    (section_b, section_c)
}

/// Fold one group's figures into another, keeping both methods named.
fn merge(into: &mut Group, from: Group) {
    into.basis_cents += from.basis_cents;
    into.deduction_cents += from.deduction_cents;
    for m in from.methods {
        if !into.methods.contains(&m) {
            into.methods.push(m);
        }
    }
}

/// A stable order for grouping, so the same books produce the same form.
///
/// The form's own order, which is what a reader compares against.
fn class_ord(class: PropertyClass) -> u8 {
    match class {
        PropertyClass::ThreeYear => 0,
        PropertyClass::FiveYear => 1,
        PropertyClass::SevenYear => 2,
        PropertyClass::TenYear => 3,
        // One ordinal, because the form prints one row: land improvements are
        // 150% declining balance and qualified improvement property is straight
        // line, and both report on 19e. Giving them separate ordinals split them
        // into two groups competing for a single row, and one of the two figures
        // was overwritten by the other.
        PropertyClass::FifteenYearLandImprovement | PropertyClass::QualifiedImprovement => 4,
        PropertyClass::TwentyYear => 5,
        PropertyClass::TwentyFiveYear => 6,
        PropertyClass::ResidentialRental => 7,
        PropertyClass::Nonresidential => 8,
    }
}

/// Write one grouped row into the boxes this revision leaves blank.
fn write_group(
    doc: &mut Document,
    map: &super::acroform::FieldMap,
    row: &Row,
    group: &Group,
    line: &str,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();

    let month = group
        .month
        .map(|(y, m)| format!("{m:02}/{y}"))
        .unwrap_or_default();
    // A `None` cell is a column the form has already printed, or one this
    // revision does not have. Either way there is nothing to write.
    for (cell, value) in [
        (row.month_year, month),
        (row.basis, money(group.basis_cents)),
        (row.recovery, group.recovery.clone()),
        (row.convention, group.convention.clone()),
        (row.method, group.methods.join("/")),
        (row.deduction, money(group.deduction_cents)),
    ] {
        if let Some(field) = cell {
            set_text(doc, map, field, &value)?;
        }
    }

    // One printed row, two methods — the 15-year case. The figures are combined
    // so the form foots; the split needs a statement.
    if group.methods.len() > 1 {
        warnings.push(format!(
            "Form 4562 line {line}: this row carries property depreciated two different ways \
             ({}), and the form prints one method column for it. The basis and the deduction are \
             combined so line 22 still adds up, and the column names both — but a supporting \
             statement has to set out the split. 15-year property is the usual cause: land \
             improvements are 150% declining balance and qualified improvement property is \
             straight line, and both report on line 19e.",
            group.methods.join(" and ")
        ));
    }

    Ok(warnings)
}

/// Cents as the form prints them: whole dollars, with thousands separated.
fn money(cents: i64) -> String {
    format_dollars(cents_to_dollars(cents))
}

/// The depreciation schedule behind Form 4562: every asset on the register, with
/// its basis, what was taken before this year and in it, and what is left.
///
/// # Why every asset, not only the ones the form has a row for
///
/// Form 4562 itemises only property placed in service this year. Everything
/// older is one figure on line 17 — "MACRS deductions for assets placed in
/// service in tax years beginning before" this one — with nothing on the form
/// saying which assets it is or how much of each is left to recover. This page is
/// that missing detail, the schedule a preparer would otherwise keep beside the
/// return: one row per asset, so line 17 and the year's own rows can be traced
/// to the property behind them.
///
/// It also carries what used to be a statement of its own: an asset whose basis
/// moved after purchase — a grant that reimbursed a fit-out, a rebate — has a
/// note under its row saying by how much, from when, and why, and so does a year
/// whose depreciation was fixed by hand. `None` when the register has nothing for
/// the year.
pub fn depreciation_statement(
    schedule: &YearSchedule<'_>,
    name: &str,
    id_label: &str,
    identifying_number: &str,
) -> Result<Option<Document>, FormError> {
    use super::statement::{build_table, Column, TableLine, TableStatement};

    let year = schedule.tax_year;
    if schedule.rows.is_empty() {
        return Ok(None);
    }

    let dollars = |cents: i64| {
        let whole = super::lines::format_dollars(super::lines::cents_to_dollars(cents.abs()));
        if cents < 0 {
            format!("({whole})")
        } else {
            whole
        }
    };
    let column = |title: &str, x: f32, right: bool| Column {
        title: title.to_string(),
        x,
        right,
    };
    let columns = vec![
        column("Asset", 54.0, false),
        column("In service", 210.0, false),
        column("Basis", 336.0, true),
        column("Method", 346.0, false),
        column("Prior years", 520.0, true),
        column(&year.to_string(), 590.0, true),
        column("Accumulated", 664.0, true),
        column("Remaining", 738.0, true),
    ];

    let mut lines = Vec::new();
    let (mut basis_total, mut prior_total, mut year_total, mut accumulated_total) = (0, 0, 0, 0);
    for r in &schedule.rows {
        let a = r.asset;
        let this_year = r.total_cents();
        let prior = r.accumulated_cents - this_year;
        basis_total += r.adjusted_cost_cents;
        prior_total += prior;
        year_total += this_year;
        accumulated_total += r.accumulated_cents;
        lines.push(TableLine::Cells(vec![
            a.description.chars().take(30).collect(),
            a.placed_in_service.to_string(),
            dollars(r.adjusted_cost_cents),
            format!(
                "{} {} {}",
                method_label(a.class.method(a.system)),
                convention_label(r.convention),
                recovery_label(a.class.recovery_years(a.system))
            ),
            dollars(prior),
            dollars(this_year),
            dollars(r.accumulated_cents),
            dollars(r.adjusted_cost_cents - r.accumulated_cents),
        ]));
        // How this year's figure is made up, when it is more than the tables'
        // MACRS: §179 and bonus are taken once, in the first year, and a reader
        // checking the row against a percentage table needs to see them apart.
        if r.section_179_cents != 0 || r.bonus_cents != 0 {
            let mut parts = Vec::new();
            if r.section_179_cents != 0 {
                parts.push(format!("§179 {}", dollars(r.section_179_cents)));
            }
            if r.bonus_cents != 0 {
                parts.push(format!("special depreciation allowance {}", dollars(r.bonus_cents)));
            }
            parts.push(format!("MACRS {}", dollars(r.macrs_cents)));
            lines.push(TableLine::Note(format!("{year}: {}", parts.join(", "))));
        }
        let adjustments: Vec<_> = a
            .basis_adjustments
            .iter()
            .filter(|b| b.effective_year <= year)
            .collect();
        if !adjustments.is_empty() {
            lines.push(TableLine::Note(format!("Cost {}", dollars(a.cost_cents))));
        }
        for adjustment in adjustments {
            lines.push(TableLine::Note(format!(
                "Basis {} by {} from {}: {}",
                if adjustment.amount_cents < 0 {
                    "reduced"
                } else {
                    "increased"
                },
                dollars(adjustment.amount_cents.abs()),
                adjustment.effective_year,
                adjustment.note
            )));
        }
        if let Some(fixed) = a.overrides.get(&year) {
            lines.push(TableLine::Note(format!(
                "{year} depreciation fixed at {}: {}",
                dollars(fixed.amount_cents),
                fixed.note
            )));
        }
        if let Some(d) = a.disposed_on.filter(|d| d.year() == year) {
            lines.push(TableLine::Note(format!("Disposed of {d}")));
        }
    }
    lines.push(TableLine::Cells(vec![
        "Total".to_string(),
        String::new(),
        dollars(basis_total),
        String::new(),
        dollars(prior_total),
        dollars(year_total),
        dollars(accumulated_total),
        dollars(basis_total - accumulated_total),
    ]));

    build_table(&TableStatement {
        legal_name: name,
        id_label,
        ein: identifying_number,
        heading: format!("Form 4562 ({year}) — depreciation schedule"),
        subheading: "Every depreciable asset: its basis, the depreciation taken, and what remains"
            .to_string(),
        columns,
        lines,
        footnotes: vec![
            "Basis is cost plus or minus any adjustment in effect by the end of the year. The \
             year's column includes §179 and the special depreciation allowance where taken; \
             remaining is basis less accumulated depreciation. Land is not depreciated and is \
             not listed."
                .to_string(),
            "From the year a basis adjustment takes effect, depreciation is figured on the \
             adjusted basis less the depreciation already allowed, over the rest of the \
             recovery period."
                .to_string(),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, BonusElection, DepreciableAsset, PropertyClass, System};
    use crate::tax::acroform::get_value;
    use crate::tax::depreciation::compute_year;
    use chrono::NaiveDate;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn profile() -> BusinessProfile {
        BusinessProfile {
            legal_name: "Bunny Ears Art House LLC".into(),
            address: Address {
                street: "1 Studio Lane".into(),
                suite: None,
                city: "Chicago".into(),
                state: "IL".into(),
                postal_code: "60640".into(),
                country: None,
            },
            ein: "12-3456789".into(),
            naics_code: "611610".into(),
            formation_date: date(2023, 4, 13),
            principal_activity: Some("Fine arts instruction".into()),
            principal_product: Some("Art classes".into()),
        }
    }

    fn asset(
        description: &str,
        class: PropertyClass,
        placed: NaiveDate,
        cost: i64,
    ) -> DepreciableAsset {
        DepreciableAsset {
            asset_id: description.to_lowercase(),
            description: description.into(),
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
            bonus: BonusElection::Decline,
            disposed_on: None,
            notes: None,
            overrides: Default::default(),
            basis_adjustments: Vec::new(),
        }
    }

    /// The same asset, elected onto the alternative depreciation system.
    fn ads_asset(
        description: &str,
        class: PropertyClass,
        placed: NaiveDate,
        cost: i64,
    ) -> DepreciableAsset {
        let mut a = asset(description, class, placed, cost);
        a.system = System::Ads;
        a
    }

    fn value(f: &Filled, name: &str) -> Option<String> {
        let map = field_map(&f.document);
        get_value(&f.document, &map, name)
    }

    fn statement_text(doc: &Document) -> String {
        doc.get_pages()
            .keys()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The schedule lists every asset — the building bought this year and the
    /// improvement written off whole — with its basis, the year's depreciation,
    /// what has accumulated and what remains, and a total row.
    #[test]
    fn the_depreciation_schedule_lists_every_asset_and_what_remains() {
        let mut fitout = asset(
            "5113 N Lincoln Improvements",
            PropertyClass::QualifiedImprovement,
            date(2025, 12, 29),
            1_980_600,
        );
        fitout.bonus = BonusElection::Take;
        let assets = vec![
            asset(
                "5113 N Lincoln Ave",
                PropertyClass::Nonresidential,
                date(2025, 9, 12),
                17_899_300,
            ),
            fitout,
        ];
        let s = compute_year(&assets, 2025);
        let doc = depreciation_statement(&s, "Zak Patterson", "SSN", "123-45-6789")
            .unwrap()
            .expect("a schedule");
        let text = statement_text(&doc);

        for r in &s.rows {
            assert!(text.contains(&r.asset.description), "{} missing", r.asset.description);
            let remaining = money(r.adjusted_cost_cents - r.accumulated_cents);
            assert!(text.contains(&remaining), "remaining {remaining} missing:\n{text}");
        }
        assert!(text.contains("178,993"), "the building's basis");
        assert!(
            text.contains("special depreciation allowance 19,806"),
            "how the improvement's year is made up:\n{text}"
        );
        assert!(text.contains("Total"));
        assert!(text.contains("depreciation schedule"));
    }

    /// An asset from an earlier year is on the schedule even though the form
    /// has no row for it — it is the detail behind line 17.
    #[test]
    fn an_older_asset_is_on_the_schedule_with_its_prior_years() {
        let assets = vec![asset(
            "Kiln",
            PropertyClass::SevenYear,
            date(2023, 3, 1),
            1_000_000,
        )];
        let s = compute_year(&assets, 2025);
        let row = &s.rows[0];
        let prior = row.accumulated_cents - row.total_cents();
        assert!(prior > 0);
        let text =
            statement_text(&depreciation_statement(&s, "Studio", "EIN", "12-3456789").unwrap().unwrap());
        assert!(text.contains("Kiln"));
        assert!(text.contains(&money(prior)), "prior years {}:\n{text}", money(prior));
    }

    /// A basis that moved after purchase is still explained under its row.
    #[test]
    fn a_basis_adjustment_is_noted_under_its_asset() {
        let mut kiln = asset("Kiln", PropertyClass::SevenYear, date(2024, 3, 1), 1_000_000);
        kiln.basis_adjustments.push(crate::domain::BasisAdjustment {
            adjustment_id: "a".into(),
            effective_year: 2025,
            amount_cents: -200_000,
            note: "Grant reimbursed part of the cost".into(),
        });
        let assets = [kiln];
        let s = compute_year(&assets, 2025);
        let text =
            statement_text(&depreciation_statement(&s, "Studio", "EIN", "12-3456789").unwrap().unwrap());
        assert!(text.contains("Cost 10,000"), "{text}");
        assert!(text.contains("Grant reimbursed part of the cost"), "{text}");
    }

    #[test]
    fn an_empty_register_has_no_schedule() {
        let assets: [DepreciableAsset; 0] = [];
        let s = compute_year(&assets, 2025);
        assert!(depreciation_statement(&s, "Studio", "EIN", "12-3456789").unwrap().is_none());
    }

    #[test]
    fn an_empty_register_produces_no_form() {
        let assets: Vec<DepreciableAsset> = Vec::new();
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts instruction", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        assert!(form.is_none());
    }

    /// A year this program carries no revision for attaches nothing and says so.
    ///
    /// Not filled on a neighbouring year's blank. Between the 2024 and 2025
    /// revisions the first column of every Section B row went from `R4[0]` to
    /// `f1_26[0]`, the `f1_*` numbering downstream shifted by one, and Part IV's
    /// total moved from page 1 to page 2. Every name still resolved on both
    /// forms — the basis would simply have printed in the recovery-period column
    /// and the form would have totalled correctly.
    ///
    /// The return itself is unharmed either way: page 1 line 16 comes from the
    /// ledger, not from this form.
    #[test]
    fn a_year_with_no_carried_revision_attaches_no_form_and_says_why() {
        let assets = [asset(
            "Kiln",
            PropertyClass::SevenYear,
            date(2019, 3, 1),
            1_000_000,
        )];
        let s = compute_year(&assets, 2019);
        let (form, warnings) = build(&profile(), &s, "Fine arts instruction", 2019, Filer::Partnership).unwrap();
        assert!(form.is_none(), "no form rather than another year's form");
        assert!(
            warnings.iter().any(|w| {
                w.contains("No Form 4562 is attached")
                    && w.contains("2019")
                    && w.contains("by hand")
            }),
            "{warnings:?}"
        );
    }

    /// 25-year property gets its convention written, not its method.
    ///
    /// The form preprints "25 yrs." and "S/L" on row 19g and leaves the
    /// convention column blank. The old `writable: &[usize]` set for this row
    /// was `[0, 1, 4, 5]` — one position off from what its own doc comment said
    /// — so it wrote the method into the one-point-wide stub the IRS leaves
    /// where it preprints a value, and the convention came out empty.
    #[test]
    fn a_preprinted_row_gets_the_column_the_form_actually_left_blank() {
        let assets = [asset(
            "Reservoir",
            PropertyClass::TwentyFiveYear,
            date(FORM_TAX_YEAR, 3, 1),
            1_000_000,
        )];
        let s = compute_year(&assets, FORM_TAX_YEAR);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        let row = &BOXES_2025.section_b.twenty_five_year;
        assert!(row.recovery.is_none(), "25 yrs. is preprinted");
        assert!(row.method.is_none(), "S/L is preprinted");
        let convention =
            value(&f, row.convention.expect("the convention column is ours")).unwrap_or_default();
        assert!(
            !convention.is_empty(),
            "the one column the form leaves blank on this row came out blank"
        );
    }

    /// Every revision carried is a real form with a table behind it.
    #[test]
    fn every_carried_revision_is_a_real_form() {
        assert!(!FORM_4562_YEARS.is_empty());
        for r in FORM_4562_YEARS {
            let doc = Document::load_mem(r.form).expect("the blank loads");
            assert!(doc.get_pages().len() > 1, "{}: not a real form", r.year);
        }
        assert!(
            form_4562_year(FORM_TAX_YEAR).is_some(),
            "the current year must be carried"
        );
    }

    #[test]
    fn the_header_and_a_seven_year_asset_reach_their_boxes() {
        let assets = [asset(
            "Kiln",
            PropertyClass::SevenYear,
            date(2025, 3, 1),
            1_000_000,
        )];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts instruction", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        assert_eq!(
            value(&f, BOXES_2025.name).as_deref(),
            Some("Bunny Ears Art House LLC")
        );
        assert_eq!(value(&f, BOXES_2025.ein).as_deref(), Some("12-3456789"));
        assert_eq!(
            value(&f, BOXES_2025.activity).as_deref(),
            Some("Fine arts instruction")
        );

        // Row 19c is 7-year property: month, basis, period, convention, method,
        // deduction.
        let row = &BOXES_2025.section_b.seven_year;
        assert_eq!(
            value(&f, row.month_year.unwrap()).as_deref(),
            Some("03/2025")
        );
        assert_eq!(value(&f, row.basis.unwrap()).as_deref(), Some("10,000"));
        assert_eq!(value(&f, row.recovery.unwrap()).as_deref(), Some("7"));
        assert_eq!(value(&f, row.convention.unwrap()).as_deref(), Some("HY"));
        assert_eq!(value(&f, row.method.unwrap()).as_deref(), Some("200DB"));
        assert_eq!(value(&f, row.deduction.unwrap()).as_deref(), Some("1,429"));
    }

    /// The 2025 revision moved these rows. A 39-year building belongs on 19j,
    /// which is where the old 39-year row is *not*.
    #[test]
    fn real_property_reports_on_the_2025_rows_with_the_preprinted_columns_left_alone() {
        let assets = [asset(
            "Studio building",
            PropertyClass::Nonresidential,
            date(2025, 4, 1),
            39_000_000,
        )];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        let row = &BOXES_2025.section_b.nonresidential_real[0]; // 19j, first row
        assert_eq!(
            value(&f, row.month_year.unwrap()).as_deref(),
            Some("04/2025")
        );
        assert_eq!(value(&f, row.basis.unwrap()).as_deref(), Some("390,000"));
        // The period, convention and method are printed on the form already, so
        // the table has no box for them at all — which is stronger than leaving
        // them empty, because there is nothing to write into by mistake.
        assert!(row.recovery.is_none(), "27.5 yrs. is preprinted");
        assert!(row.convention.is_none(), "MM is preprinted");
        assert!(row.method.is_none(), "S/L is preprinted");
        assert!(!value(&f, row.deduction.unwrap())
            .unwrap_or_default()
            .is_empty());
    }

    /// Two buildings from different months take the two printed rows, because
    /// mid-month makes the month part of the computation.
    #[test]
    fn two_months_of_real_property_take_the_two_printed_rows() {
        let assets = [
            asset(
                "Building A",
                PropertyClass::Nonresidential,
                date(2025, 2, 1),
                10_000_000,
            ),
            asset(
                "Building B",
                PropertyClass::Nonresidential,
                date(2025, 9, 1),
                10_000_000,
            ),
        ];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        assert_eq!(
            value(
                &f,
                BOXES_2025.section_b.nonresidential_real[0]
                    .month_year
                    .unwrap()
            )
            .as_deref(),
            Some("02/2025")
        );
        assert_eq!(
            value(
                &f,
                BOXES_2025.section_b.nonresidential_real[1]
                    .month_year
                    .unwrap()
            )
            .as_deref(),
            Some("09/2025")
        );
    }

    /// The 15-year collision: a parking lot and a shop fit-out share line 19e and
    /// do not share a method. The row must still foot, and must say so.
    #[test]
    fn the_two_fifteen_year_classes_share_a_row_and_the_split_is_reported() {
        let assets = [
            asset(
                "Parking lot",
                PropertyClass::FifteenYearLandImprovement,
                date(2025, 3, 1),
                1_000_000,
            ),
            asset(
                "Studio fit-out",
                PropertyClass::QualifiedImprovement,
                date(2025, 3, 1),
                1_000_000,
            ),
        ];
        let s = compute_year(&assets, 2025);
        let (form, warnings) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        let row = &BOXES_2025.section_b.fifteen_year;
        assert_eq!(
            value(&f, row.basis.unwrap()).as_deref(),
            Some("20,000"),
            "combined basis"
        );
        let method = value(&f, row.method.unwrap()).unwrap_or_default();
        assert!(
            method.contains("150DB") && method.contains("S/L"),
            "{method}"
        );

        assert!(
            warnings
                .iter()
                .any(|w| w.contains("19e") || w.contains("two different ways")),
            "{warnings:?}"
        );

        // And the deduction is the sum of the two schedules, not one of them.
        assert_eq!(f.line_16a_cents, s.line_16a_cents());
    }

    /// Assets from earlier years belong on line 17, not in Section B — Section B
    /// is this year's acquisitions.
    #[test]
    fn earlier_years_go_to_line_seventeen_and_not_into_section_b() {
        let assets = [
            asset(
                "Old kiln",
                PropertyClass::SevenYear,
                date(2023, 3, 1),
                1_000_000,
            ),
            asset(
                "New kiln",
                PropertyClass::SevenYear,
                date(2025, 3, 1),
                1_000_000,
            ),
        ];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        // 7-year year 3 is 17.49%.
        assert_eq!(
            value(&f, BOXES_2025.l17_prior_years).as_deref(),
            Some("1,749")
        );
        // Section B row 19c carries only the new one's basis.
        assert_eq!(
            value(&f, BOXES_2025.section_b.seven_year.basis.unwrap()).as_deref(),
            Some("10,000")
        );
    }

    /// Property bonus depreciation took in full leaves no Section B row: its
    /// deduction is on line 14, and a row of zeros would say nothing.
    #[test]
    fn fully_expensed_property_leaves_no_section_b_row() {
        let mut fitout = asset(
            "Fit-out",
            PropertyClass::QualifiedImprovement,
            date(2025, 6, 1),
            2_000_000,
        );
        fitout.bonus = BonusElection::Take;
        let kiln = asset("Kiln", PropertyClass::SevenYear, date(2025, 3, 1), 1_000_000);
        let assets = [fitout, kiln];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        let fifteen = &BOXES_2025.section_b.fifteen_year;
        assert_eq!(value(&f, fifteen.basis.unwrap()), None, "no 19e row");
        assert_eq!(value(&f, fifteen.recovery.unwrap()), None, "not even its period");
        assert_eq!(
            value(&f, BOXES_2025.section_b.seven_year.basis.unwrap()).as_deref(),
            Some("10,000"),
            "a row with basis is still written"
        );
    }

    /// The mistake the form invites: line 22 includes §179 and line 16a must not.
    #[test]
    fn line_22_includes_section_179_and_line_16a_does_not() {
        let mut a = asset(
            "Kiln",
            PropertyClass::SevenYear,
            date(2025, 3, 1),
            1_000_000,
        );
        a.section_179_cents = 400_000;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, warnings) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        assert_eq!(f.section_179_cents, 400_000);
        assert_eq!(f.line_22_cents, f.line_16a_cents + 400_000);
        assert_eq!(f.line_16a_cents, s.line_16a_cents());
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("deducts the §179 twice")),
            "{warnings:?}"
        );
    }

    /// Part I's arithmetic, including the phase-out, against 2025's figures.
    #[test]
    fn part_one_computes_the_2025_limit_and_phase_out() {
        let mut a = asset(
            "Press",
            PropertyClass::SevenYear,
            date(2025, 3, 1),
            500_000_00,
        );
        a.section_179_cents = 400_000_00;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        assert_eq!(
            value(&f, BOXES_2025.l1_maximum).as_deref(),
            Some("2,500,000")
        );
        assert_eq!(
            value(&f, BOXES_2025.l3_threshold).as_deref(),
            Some("4,000,000")
        );
        assert_eq!(
            value(&f, BOXES_2025.l2_total_cost).as_deref(),
            Some("500,000")
        );
        // Well under the threshold, so no reduction and the full limit stands.
        assert_eq!(value(&f, BOXES_2025.l4_reduction).as_deref(), Some("0"));
        assert_eq!(
            value(&f, BOXES_2025.l5_dollar_limit).as_deref(),
            Some("2,500,000")
        );
        assert_eq!(
            value(&f, BOXES_2025.l8_total_elected).as_deref(),
            Some("400,000")
        );
        assert_eq!(
            value(&f, BOXES_2025.l12_deduction).as_deref(),
            Some("400,000")
        );
    }

    /// The two figures the books cannot supply are named rather than guessed.
    #[test]
    fn the_carryover_and_income_limitation_are_reported_as_unfilled() {
        let mut a = asset(
            "Kiln",
            PropertyClass::SevenYear,
            date(2025, 3, 1),
            1_000_000,
        );
        a.section_179_cents = 400_000;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (_, warnings) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("line 10") && w.contains("line 11")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_year_with_no_known_section_179_limit_says_so_rather_than_using_last_years() {
        let mut a = asset(
            "Kiln",
            PropertyClass::SevenYear,
            date(2030, 3, 1),
            1_000_000,
        );
        a.section_179_cents = 400_000;
        let assets = [a];
        let s = compute_year(&assets, 2030);
        let (form, warnings) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        assert!(value(&f, BOXES_2025.l1_maximum)
            .unwrap_or_default()
            .is_empty());
        assert!(
            warnings.iter().any(|w| w.contains("indexed each year")),
            "{warnings:?}"
        );
    }

    #[test]
    fn bonus_reaches_line_fourteen() {
        let mut a = asset(
            "Press",
            PropertyClass::FiveYear,
            date(2025, 6, 1),
            1_000_000,
        );
        a.acquired_on = date(2025, 6, 1);
        a.bonus = BonusElection::Take;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        assert_eq!(value(&f, BOXES_2025.l14_bonus).as_deref(), Some("10,000"));
        assert_eq!(f.line_22_cents, 1_000_000);
    }

    /// Line 22 has to equal what the schedule says the year came to, or the form
    /// and the return disagree about the same number.
    #[test]
    fn line_22_equals_the_whole_year_the_register_computed() {
        let mut press = asset(
            "Press",
            PropertyClass::FiveYear,
            date(2025, 6, 1),
            2_000_000,
        );
        press.acquired_on = date(2025, 6, 1);
        press.bonus = BonusElection::Take;
        let assets = [
            asset(
                "Old kiln",
                PropertyClass::SevenYear,
                date(2023, 3, 1),
                1_000_000,
            ),
            asset(
                "New kiln",
                PropertyClass::SevenYear,
                date(2025, 3, 1),
                1_000_000,
            ),
            press,
            asset(
                "Fit-out",
                PropertyClass::QualifiedImprovement,
                date(2025, 5, 1),
                5_000_000,
            ),
        ];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        assert_eq!(f.line_22_cents, s.total_cents());
        assert_eq!(f.line_16a_cents, s.line_16a_cents());
    }

    /// ADS property reports in Section C, and anything without a printed row of
    /// its own goes to "class life" with the period written in.
    #[test]
    fn ads_property_reports_in_section_c() {
        let mut a = asset(
            "Press",
            PropertyClass::SevenYear,
            date(2025, 6, 1),
            1_000_000,
        );
        a.system = System::Ads;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts", FORM_TAX_YEAR, Filer::Partnership).unwrap();
        let f = form.expect("a form");

        // 20a class life, with the ADS 10-year period.
        assert_eq!(
            value(&f, BOXES_2025.section_c.class_life.recovery.unwrap()).as_deref(),
            Some("10")
        );
        assert_eq!(
            value(&f, BOXES_2025.section_c.class_life.basis.unwrap()).as_deref(),
            Some("10,000")
        );
        // And nothing in Section B.
        assert!(value(&f, BOXES_2025.section_b.seven_year.basis.unwrap())
            .unwrap_or_default()
            .is_empty());
    }

    /// Every field this module names has to exist, or a revision has renumbered
    /// the form under us — the check the other form modules carry.
    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_form() {
        let mut doc = Document::load_mem(F4562).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);

        for name in [
            BOXES_2025.name,
            BOXES_2025.activity,
            BOXES_2025.ein,
            BOXES_2025.l1_maximum,
            BOXES_2025.l2_total_cost,
            BOXES_2025.l3_threshold,
            BOXES_2025.l4_reduction,
            BOXES_2025.l5_dollar_limit,
            BOXES_2025.l8_total_elected,
            BOXES_2025.l9_tentative,
            BOXES_2025.l12_deduction,
            BOXES_2025.l14_bonus,
            BOXES_2025.l17_prior_years,
            BOXES_2025.l22_total,
        ] {
            assert!(map.find(name).is_some(), "f4562.pdf has no field {name}");
        }
        for row in BOXES_2025.l6_rows {
            for f in row {
                assert!(map.find(f).is_some(), "f4562.pdf has no line 6 field {f}");
            }
        }
        for (label, field) in row_boxes(&BOXES_2025) {
            assert!(
                map.find(field).is_some(),
                "f4562.pdf has no {label} field {field}"
            );
        }
    }

    /// Every box every revision names exists in *that revision's* PDF.
    ///
    /// Run over the whole table rather than the current year, because a
    /// revision offered in the picker and never checked is the shape of every
    /// wrong return this module has produced.
    #[test]
    fn every_revision_names_only_boxes_its_own_pdf_has() {
        for revision in FORM_4562_YEARS {
            let mut doc = Document::load_mem(revision.form).expect("the blank loads");
            strip_xfa(&mut doc);
            let map = field_map(&doc);
            for (label, field) in all_boxes(revision.boxes) {
                assert!(
                    map.find(field).is_some(),
                    "the {} form has no field {field} ({label})",
                    revision.year
                );
            }
        }
    }

    /// No table names a box the form has already printed into.
    ///
    /// Where the IRS preprints a value — "27.5 yrs.", "MM", "S/L" — it leaves a
    /// one-point-wide stub behind. Writing into one puts the text nowhere a
    /// reader can see, and the column it belonged in comes out blank. That is
    /// exactly what the old `writable: &[usize]` did for 25-year and 12-year
    /// property: it wrote the method into the stub and left the convention empty.
    #[test]
    fn no_table_names_a_box_too_small_to_hold_anything() {
        for revision in FORM_4562_YEARS {
            let mut doc = Document::load_mem(revision.form).expect("the blank loads");
            strip_xfa(&mut doc);
            let map = field_map(&doc);
            for (label, field) in all_boxes(revision.boxes) {
                let Some(id) = map.find(field) else { continue };
                let width = doc
                    .get_object(id)
                    .ok()
                    .and_then(|o| o.as_dict().ok())
                    .and_then(|d| d.get(b"Rect").ok())
                    .and_then(|r| r.as_array().ok())
                    .map(|a| {
                        let n: Vec<f64> = a
                            .iter()
                            .filter_map(|o| {
                                o.as_float()
                                    .map(f64::from)
                                    .or_else(|_| o.as_i64().map(|i| i as f64))
                                    .ok()
                            })
                            .collect();
                        if n.len() == 4 {
                            n[2] - n[0]
                        } else {
                            0.0
                        }
                    })
                    .unwrap_or(0.0);
                assert!(
                    width >= 10.0,
                    "{}: {label} points at {field}, which is {width:.0}pt wide — the form \
                     preprints that column and left a stub",
                    revision.year
                );
            }
        }
    }

    /// No two slots point at the same box.
    ///
    /// A duplicate means one of the two values is silently overwritten by the
    /// other, and which one depends on write order.
    #[test]
    fn no_two_slots_share_a_box() {
        for revision in FORM_4562_YEARS {
            let mut seen: Vec<(&str, &str)> = Vec::new();
            for (label, field) in all_boxes(revision.boxes) {
                if let Some((other, _)) = seen.iter().find(|(_, f)| *f == field) {
                    panic!("{}: {label} and {other} both write {field}", revision.year);
                }
                seen.push((label, field));
            }
        }
    }

    /// Every class this program models lands on a Section B row, on every
    /// revision. Section B is organised by class, so a missing row would mean a
    /// class the form cannot report at all.
    #[test]
    fn every_property_class_has_a_section_b_row_on_every_revision() {
        for revision in FORM_4562_YEARS {
            for class in PropertyClass::ALL {
                assert!(
                    !section_b_rows(&revision.boxes.section_b, class).is_empty(),
                    "{class:?} has no Section B row on the {} form",
                    revision.year
                );
            }
        }
    }

    /// Section C is organised by recovery period, and the periods it prints
    /// changed: the 2025 revision added a 50-year row.
    ///
    /// 25-year GDS property has a **50-year ADS life**, so a partnership electing
    /// ADS on it has no row to report on before 2025. That is a fact about the
    /// paper, not a gap in this program — what matters is that it is said out
    /// loud rather than folded into "class life", which is what the old
    /// row-index model did.
    #[test]
    fn section_c_covers_every_class_on_the_current_form_and_says_what_older_ones_lack() {
        let current = form_4562_year(FORM_TAX_YEAR).expect("the current revision");
        for class in PropertyClass::ALL {
            assert!(
                section_c_row(&current.boxes.section_c, class).is_some(),
                "{class:?} has no Section C row on the {FORM_TAX_YEAR} form"
            );
        }

        for revision in FORM_4562_YEARS
            .iter()
            .filter(|r| r.boxes.section_c.fifty_year.is_none())
        {
            assert!(
                section_c_row(&revision.boxes.section_c, PropertyClass::TwentyFiveYear).is_none(),
                "the {} form has no 50-year row, so this must not resolve to one",
                revision.year
            );

            // And the builder says so rather than writing it somewhere else.
            let assets = [ads_asset(
                "Reservoir",
                PropertyClass::TwentyFiveYear,
                date(revision.year, 3, 1),
                1_000_000,
            )];
            let s = compute_year(&assets, revision.year);
            let (_, warnings) = build(&profile(), &s, "Fine arts", revision.year, Filer::Partnership).unwrap();
            assert!(
                warnings
                    .iter()
                    .any(|w| w.contains("50-year") || w.contains("50 ")),
                "{}: {warnings:?}",
                revision.year
            );
        }
    }

    /// Every box sits in the column its printed heading names.
    ///
    /// The check no field-name test can make. Between the 2024 and 2025
    /// revisions the first column of every Section B row went from `R4[0]` to
    /// `f1_26[0]` and the `f1_*` numbering downstream shifted by one — every
    /// name still resolved on both forms, and the basis would have printed in
    /// the recovery-period column.
    #[test]
    fn every_row_box_sits_in_the_column_its_heading_names() {
        // The six column bands, read off the printed headings: (b) month and
        // year, (c) basis, (d) recovery, (e) convention, (f) method,
        // (g) deduction. Identical on every revision carried — it is the field
        // *names* that move, not the grid.
        const BANDS: [(f64, f64, &str); 6] = [
            (130.0, 190.0, "month/year"),
            (190.0, 275.0, "basis"),
            (275.0, 331.0, "recovery"),
            (331.0, 396.0, "convention"),
            (396.0, 482.0, "method"),
            (482.0, 580.0, "deduction"),
        ];

        for revision in FORM_4562_YEARS {
            let mut doc = Document::load_mem(revision.form).expect("the blank loads");
            strip_xfa(&mut doc);
            let map = field_map(&doc);

            for (label, field) in row_boxes(revision.boxes) {
                let Some(id) = map.find(field) else { continue };
                let Some(rect) = doc
                    .get_object(id)
                    .ok()
                    .and_then(|o| o.as_dict().ok())
                    .and_then(|d| d.get(b"Rect").ok())
                    .and_then(|r| r.as_array().ok())
                    .map(|a| {
                        a.iter()
                            .filter_map(|o| {
                                o.as_float()
                                    .map(f64::from)
                                    .or_else(|_| o.as_i64().map(|i| i as f64))
                                    .ok()
                            })
                            .collect::<Vec<f64>>()
                    })
                else {
                    continue;
                };
                if rect.len() != 4 {
                    continue;
                }
                let column = label.rsplit('.').next().unwrap_or(label);
                let band = BANDS
                    .iter()
                    .find(|(lo, hi, _)| rect[0] >= *lo && rect[0] < *hi)
                    .map(|(_, _, name)| *name);
                assert_eq!(
                    band,
                    Some(column),
                    "{}: {label} points at {field} at x={:.0}, which is the {:?} column",
                    revision.year,
                    rect[0],
                    band
                );
            }
        }
    }

    /// Every box a revision names, as `(what it is, the field)`.
    fn all_boxes(b: &'static Boxes) -> Vec<(&'static str, &'static str)> {
        let mut out = vec![
            ("name", b.name),
            ("activity", b.activity),
            ("ein", b.ein),
            ("l1_maximum", b.l1_maximum),
            ("l2_total_cost", b.l2_total_cost),
            ("l3_threshold", b.l3_threshold),
            ("l4_reduction", b.l4_reduction),
            ("l5_dollar_limit", b.l5_dollar_limit),
            ("l8_total_elected", b.l8_total_elected),
            ("l9_tentative", b.l9_tentative),
            ("l10_carryover_in", b.l10_carryover_in),
            ("l11_income_limit", b.l11_income_limit),
            ("l12_deduction", b.l12_deduction),
            ("l13_carryover_out", b.l13_carryover_out),
            ("l14_bonus", b.l14_bonus),
            ("l17_prior_years", b.l17_prior_years),
            ("l22_total", b.l22_total),
        ];
        for row in b.l6_rows {
            for f in row {
                out.push(("l6", f));
            }
        }
        out.extend(row_boxes(b));
        out
    }

    /// Only the Section B and C row cells, which are the ones with columns.
    fn row_boxes(b: &'static Boxes) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        let mut add = |label: &'static str, row: &Row| {
            for (col, cell) in [
                ("month/year", row.month_year),
                ("basis", row.basis),
                ("recovery", row.recovery),
                ("convention", row.convention),
                ("method", row.method),
                ("deduction", row.deduction),
            ] {
                if let Some(f) = cell {
                    out.push((
                        Box::leak(format!("{label}.{col}").into_boxed_str()) as &str,
                        f,
                    ));
                }
            }
        };
        let sb = &b.section_b;
        add("19a", &sb.three_year);
        add("19b", &sb.five_year);
        add("19c", &sb.seven_year);
        add("19d", &sb.ten_year);
        add("19e", &sb.fifteen_year);
        add("19f", &sb.twenty_year);
        add("19g", &sb.twenty_five_year);
        if let Some(r) = &sb.fifty_year {
            add("19h", r);
        }
        add("19-res-1", &sb.residential_rental[0]);
        add("19-res-2", &sb.residential_rental[1]);
        add("19-nonres-1", &sb.nonresidential_real[0]);
        add("19-nonres-2", &sb.nonresidential_real[1]);
        let sc = &b.section_c;
        add("20a", &sc.class_life);
        add("20b", &sc.twelve_year);
        add("20c", &sc.thirty_year);
        add("20d", &sc.forty_year);
        if let Some(r) = &sc.fifty_year {
            add("20e", r);
        }
        out
    }
}
