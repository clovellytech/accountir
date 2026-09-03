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

use super::acroform::{FormError, field_map, set_text, strip_xfa};
use super::depreciation::YearSchedule;
use super::lines::{cents_to_dollars, format_dollars};
use crate::domain::{BusinessProfile, Convention, Method, PropertyClass, System};
use lopdf::Document;

const F4562: &[u8] = include_bytes!("../../assets/irs/f4562.pdf");

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

mod field {
    /// Header: name(s) shown on return, the activity, the EIN.
    pub const NAME: &str = "f1_1[0]";
    pub const ACTIVITY: &str = "f1_2[0]";
    pub const EIN: &str = "f1_3[0]";

    // Part I — §179.
    pub const L1_MAXIMUM: &str = "f1_4[0]";
    pub const L2_TOTAL_COST: &str = "f1_5[0]";
    pub const L3_THRESHOLD: &str = "f1_6[0]";
    pub const L4_REDUCTION: &str = "f1_7[0]";
    pub const L5_DOLLAR_LIMIT: &str = "f1_8[0]";
    /// Line 6: description, cost, elected cost. Two printed rows.
    pub const L6_ROWS: [[&str; 3]; 2] = [
        ["f1_9[0]", "f1_10[0]", "f1_11[0]"],
        ["f1_12[0]", "f1_13[0]", "f1_14[0]"],
    ];
    pub const L8_TOTAL_ELECTED: &str = "f1_16[0]";
    pub const L9_TENTATIVE: &str = "f1_17[0]";
    pub const L12_DEDUCTION: &str = "f1_20[0]";

    // Part II — the special depreciation allowance.
    pub const L14_BONUS: &str = "f1_22[0]";

    // Part III — MACRS.
    pub const L17_PRIOR_YEARS: &str = "f1_25[0]";

    // Part IV — the summary, on page 2.
    pub const L22_TOTAL: &str = "f2_2[0]";
}

/// One row of Section B or C: the six columns, and which of them the printed
/// form leaves blank for us.
///
/// The IRS preprints the recovery period, convention and method on the rows where
/// the law fixes them — 27.5 years mid-month straight line has nowhere else to go
/// — so writing those columns would stamp a value on top of the one already
/// printed there. `writable` says which columns are genuinely empty.
struct FormRow {
    /// (b) month and year, (c) basis, (d) recovery period, (e) convention,
    /// (f) method, (g) deduction.
    fields: [&'static str; 6],
    writable: &'static [usize],
}

/// Every column blank — rows 19a to 19f, where the class alone does not fix the
/// method.
const ALL: &[usize] = &[0, 1, 2, 3, 4, 5];
/// Recovery period and method preprinted; convention still ours.
const NO_PERIOD_OR_METHOD: &[usize] = &[0, 1, 4, 5];
/// Recovery period, convention and method all preprinted — the real-property
/// rows.
const AMOUNTS_ONLY: &[usize] = &[0, 1, 5];

/// Section B — assets placed in service this year under the general system.
///
/// The order is the form's own, and 19h is 50-year property in the 2025 revision:
/// residential rental is 19i and nonresidential real is 19j, each with two rows
/// for property placed in service in different months.
const SECTION_B: [FormRow; 12] = [
    // 19a 3-year
    FormRow { fields: ["f1_26[0]", "f1_27[0]", "f1_28[0]", "f1_29[0]", "f1_30[0]", "f1_31[0]"], writable: ALL },
    // 19b 5-year
    FormRow { fields: ["f1_32[0]", "f1_33[0]", "f1_34[0]", "f1_35[0]", "f1_36[0]", "f1_37[0]"], writable: ALL },
    // 19c 7-year
    FormRow { fields: ["f1_38[0]", "f1_39[0]", "f1_40[0]", "f1_41[0]", "f1_42[0]", "f1_43[0]"], writable: ALL },
    // 19d 10-year
    FormRow { fields: ["f1_44[0]", "f1_45[0]", "f1_46[0]", "f1_47[0]", "f1_48[0]", "f1_49[0]"], writable: ALL },
    // 19e 15-year — the row that may have to carry two methods.
    FormRow { fields: ["f1_50[0]", "f1_51[0]", "f1_52[0]", "f1_53[0]", "f1_54[0]", "f1_55[0]"], writable: ALL },
    // 19f 20-year
    FormRow { fields: ["f1_56[0]", "f1_57[0]", "f1_58[0]", "f1_59[0]", "f1_60[0]", "f1_61[0]"], writable: ALL },
    // 19g 25-year — "25 yrs." and "S/L" preprinted.
    FormRow { fields: ["f1_62[0]", "f1_63[0]", "f1_64[0]", "f1_65[0]", "f1_66[0]", "f1_67[0]"], writable: NO_PERIOD_OR_METHOD },
    // 19h 50-year — "50 yrs.", "MM", "S/L" preprinted. Nothing this register
    // models lands here; the row exists so the field check covers it.
    FormRow { fields: ["f1_68[0]", "f1_69[0]", "f1_70[0]", "f1_71[0]", "f1_72[0]", "f1_73[0]"], writable: AMOUNTS_ONLY },
    // 19i residential rental, two rows
    FormRow { fields: ["f1_74[0]", "f1_75[0]", "f1_76[0]", "f1_77[0]", "f1_78[0]", "f1_79[0]"], writable: AMOUNTS_ONLY },
    FormRow { fields: ["f1_80[0]", "f1_81[0]", "f1_82[0]", "f1_83[0]", "f1_84[0]", "f1_85[0]"], writable: AMOUNTS_ONLY },
    // 19j nonresidential real, two rows
    FormRow { fields: ["f1_86[0]", "f1_87[0]", "f1_88[0]", "f1_89[0]", "f1_90[0]", "f1_91[0]"], writable: AMOUNTS_ONLY },
    FormRow { fields: ["f1_92[0]", "f1_93[0]", "f1_94[0]", "f1_95[0]", "f1_96[0]", "f1_97[0]"], writable: AMOUNTS_ONLY },
];

/// Section C — the alternative depreciation system.
const SECTION_C: [FormRow; 5] = [
    // 20a class life — "S/L" preprinted, the period is ours.
    FormRow { fields: ["f1_98[0]", "f1_99[0]", "f1_100[0]", "f1_101[0]", "f1_102[0]", "f1_103[0]"], writable: &[0, 1, 2, 3, 5] },
    // 20b 12-year
    FormRow { fields: ["f1_104[0]", "f1_105[0]", "f1_106[0]", "f1_107[0]", "f1_108[0]", "f1_109[0]"], writable: NO_PERIOD_OR_METHOD },
    // 20c 30-year
    FormRow { fields: ["f1_110[0]", "f1_111[0]", "f1_112[0]", "f1_113[0]", "f1_114[0]", "f1_115[0]"], writable: AMOUNTS_ONLY },
    // 20d 40-year
    FormRow { fields: ["f1_116[0]", "f1_117[0]", "f1_118[0]", "f1_119[0]", "f1_120[0]", "f1_121[0]"], writable: AMOUNTS_ONLY },
    // 20e 50-year
    FormRow { fields: ["f1_122[0]", "f1_123[0]", "f1_124[0]", "f1_125[0]", "f1_126[0]", "f1_127[0]"], writable: AMOUNTS_ONLY },
];

/// Which Section B row a GDS class reports on.
fn section_b_row(class: PropertyClass) -> usize {
    match class {
        PropertyClass::ThreeYear => 0,
        PropertyClass::FiveYear => 1,
        PropertyClass::SevenYear => 2,
        PropertyClass::TenYear => 3,
        // Both 15-year classes, and the whole reason a row may carry two methods.
        PropertyClass::FifteenYearLandImprovement | PropertyClass::QualifiedImprovement => 4,
        PropertyClass::TwentyYear => 5,
        PropertyClass::TwentyFiveYear => 6,
        PropertyClass::ResidentialRental => 8,
        PropertyClass::Nonresidential => 10,
    }
}

/// Which Section C row an ADS class reports on.
///
/// Section C is organised by recovery period rather than by class, because ADS
/// flattens the classes into lives: everything without a row of its own goes to
/// "class life", with its period written in.
fn section_c_row(class: PropertyClass) -> usize {
    match class.recovery_years(System::Ads) as i64 {
        30 => 2,
        40 => 3,
        50 => 4,
        _ => 0,
    }
}

/// The second row available to a class, where the form prints one.
fn overflow_row(row: usize) -> Option<usize> {
    match row {
        8 => Some(9),
        10 => Some(11),
        _ => None,
    }
}

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
) -> Result<(Option<Filled>, Vec<String>), FormError> {
    let mut warnings = Vec::new();

    if schedule.rows.is_empty() {
        return Ok((None, warnings));
    }

    let mut doc = Document::load_mem(F4562)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    set_text(&mut doc, &map, field::NAME, &profile.legal_name)?;
    set_text(&mut doc, &map, field::ACTIVITY, activity)?;
    set_text(&mut doc, &map, field::EIN, &profile.ein)?;

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

    if !elected.is_empty() {
        // Line 2 is the cost of *all* §179 property placed in service, not only
        // the part elected — it is what the phase-out is measured against.
        let total_cost: i64 = schedule
            .placed_this_year()
            .filter(|r| r.asset.class.section_179() != crate::domain::Section179Eligibility::NotEligible)
            .map(|r| r.asset.cost_cents)
            .sum();

        match section_179_limits(schedule.tax_year) {
            Some((maximum, threshold)) => {
                let reduction = (total_cost - threshold).max(0);
                let dollar_limit = (maximum - reduction).max(0);
                let tentative = dollar_limit.min(total_elected);

                set_text(&mut doc, &map, field::L1_MAXIMUM, &money(maximum))?;
                set_text(&mut doc, &map, field::L2_TOTAL_COST, &money(total_cost))?;
                set_text(&mut doc, &map, field::L3_THRESHOLD, &money(threshold))?;
                set_text(&mut doc, &map, field::L4_REDUCTION, &money(reduction))?;
                set_text(&mut doc, &map, field::L5_DOLLAR_LIMIT, &money(dollar_limit))?;
                set_text(&mut doc, &map, field::L9_TENTATIVE, &money(tentative))?;
                set_text(&mut doc, &map, field::L12_DEDUCTION, &money(tentative))?;

                if tentative < total_elected {
                    warnings.push(format!(
                        "Form 4562: {} of §179 is elected across the register, but the dollar \
                         limit for {} allows {}. Line 12 carries the limit. The difference \
                         carries forward on line 13 — which needs the two figures the books do \
                         not hold, so it is left blank.",
                        money(total_elected),
                        schedule.tax_year,
                        money(tentative)
                    ));
                }
            }
            None => warnings.push(format!(
                "Form 4562: the §179 dollar limit and phase-out threshold for {} are not known \
                 to this program, so Part I lines 1 to 5 are blank and line 12 is unfilled. \
                 Both figures are indexed each year — fill them from the {} instructions.",
                schedule.tax_year, schedule.tax_year
            )),
        }

        set_text(&mut doc, &map, field::L8_TOTAL_ELECTED, &money(total_elected))?;

        for (row, (description, cost, elected_cost)) in elected.iter().take(2).enumerate() {
            let cols = field::L6_ROWS[row];
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

        // The two inputs a ledger cannot supply. Both cap line 12, so a return
        // filed without checking them can claim more than the statute allows.
        warnings.push(
            "Form 4562 line 10 (carryover of disallowed §179 from the prior year) and line 11 \
             (the business income limitation) are left blank — neither is in the books. Line 12 \
             is filled as the tentative deduction on line 9, which is correct only when there is \
             no carryover and business income covers it. Check both before filing."
                .to_string(),
        );
    }

    // --- Part II: bonus ---------------------------------------------------
    let bonus = schedule.bonus_cents();
    if bonus != 0 {
        set_text(&mut doc, &map, field::L14_BONUS, &money(bonus))?;
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
        set_text(&mut doc, &map, field::L17_PRIOR_YEARS, &money(prior))?;
    }

    // --- Part III Sections B and C: this year's acquisitions --------------
    let (section_b, section_c) = group_current_year(schedule);
    let mut current_year_total = 0i64;

    for (row_index, group) in &section_b {
        current_year_total += group.deduction_cents;
        warnings.extend(write_group(&mut doc, &map, &SECTION_B, *row_index, group, "19")?);
    }
    for (row_index, group) in &section_c {
        current_year_total += group.deduction_cents;
        warnings.extend(write_group(&mut doc, &map, &SECTION_C, *row_index, group, "20")?);
    }

    // --- Part IV: the summary --------------------------------------------
    //
    // Line 22 is what the form says it is: line 12 plus 14 through 17 plus the
    // (g) columns. It is *not* what page 1 line 16a takes, which is why the two
    // are reported separately below.
    let line_12 = if elected.is_empty() {
        0
    } else {
        schedule.section_179_cents()
    };
    let line_22 = line_12 + bonus + prior + current_year_total;
    set_text(&mut doc, &map, field::L22_TOTAL, &money(line_22))?;

    let line_16a = line_22 - line_12;
    if line_12 != 0 {
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
fn group_current_year(
    schedule: &YearSchedule<'_>,
) -> (Vec<(usize, Group)>, Vec<(usize, Group)>) {
    let mut b: BTreeMap<(usize, Option<u32>), Group> = BTreeMap::new();
    let mut c: BTreeMap<(usize, Option<u32>), Group> = BTreeMap::new();

    for row in schedule.placed_this_year() {
        let asset = row.asset;
        let (table, index) = match asset.system {
            System::Gds => (&mut b, section_b_row(asset.class)),
            System::Ads => (&mut c, section_c_row(asset.class)),
        };
        // Only the mid-month rows separate by month; everything else shares one
        // convention across the year and belongs on one row.
        let key_month = asset
            .class
            .uses_mid_month()
            .then(|| asset.placed_in_service.month());

        let group = table.entry((index, key_month)).or_default();
        group.basis_cents += row.macrs_basis_cents;
        group.deduction_cents += row.macrs_cents;

        let placed = (asset.placed_in_service.year(), asset.placed_in_service.month());
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

    // Collapse the (row, month) keys back onto the rows the form actually has,
    // spilling to the second printed row where there is one.
    (assign_rows(b, &SECTION_B), assign_rows(c, &SECTION_C))
}

/// Place grouped figures on the printed rows, merging what does not fit.
fn assign_rows(
    grouped: BTreeMap<(usize, Option<u32>), Group>,
    table: &[FormRow],
) -> Vec<(usize, Group)> {
    let mut out: Vec<(usize, Group)> = Vec::new();
    let mut used: Vec<usize> = Vec::new();

    for ((base_row, _), group) in grouped {
        // The row itself, then its overflow row, then merge into whatever is
        // already there — a figure merged still totals; a figure dropped does not.
        let target = if !used.contains(&base_row) {
            Some(base_row)
        } else {
            overflow_row(base_row).filter(|r| !used.contains(r) && *r < table.len())
        };

        match target {
            Some(row) => {
                used.push(row);
                out.push((row, group));
            }
            None => {
                let existing = out
                    .iter_mut()
                    .find(|(r, _)| *r == base_row)
                    .expect("the base row was used, so it is in the output");
                existing.1.basis_cents += group.basis_cents;
                existing.1.deduction_cents += group.deduction_cents;
                for m in group.methods {
                    if !existing.1.methods.contains(&m) {
                        existing.1.methods.push(m);
                    }
                }
            }
        }
    }
    out
}

/// Write one grouped row, skipping the columns the form has already printed.
fn write_group(
    doc: &mut Document,
    map: &super::acroform::FieldMap,
    table: &[FormRow],
    row_index: usize,
    group: &Group,
    line: &str,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();
    let row = &table[row_index];

    let month = group
        .month
        .map(|(y, m)| format!("{m:02}/{y}"))
        .unwrap_or_default();
    let values = [
        month,
        money(group.basis_cents),
        group.recovery.clone(),
        group.convention.clone(),
        group.methods.join("/"),
        money(group.deduction_cents),
    ];

    for column in row.writable {
        set_text(doc, map, row.fields[*column], &values[*column])?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Address, BonusElection, DepreciableAsset, PropertyClass, System,
    };
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
        }
    }

    fn value(f: &Filled, name: &str) -> Option<String> {
        let map = field_map(&f.document);
        get_value(&f.document, &map, name)
    }

    #[test]
    fn an_empty_register_produces_no_form() {
        let assets: Vec<DepreciableAsset> = Vec::new();
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts instruction").unwrap();
        assert!(form.is_none());
    }

    #[test]
    fn the_header_and_a_seven_year_asset_reach_their_boxes() {
        let assets = [asset("Kiln", PropertyClass::SevenYear, date(2025, 3, 1), 1_000_000)];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts instruction").unwrap();
        let f = form.expect("a form");

        assert_eq!(value(&f, field::NAME).as_deref(), Some("Bunny Ears Art House LLC"));
        assert_eq!(value(&f, field::EIN).as_deref(), Some("12-3456789"));
        assert_eq!(value(&f, field::ACTIVITY).as_deref(), Some("Fine arts instruction"));

        // Row 19c is 7-year property: month, basis, period, convention, method,
        // deduction.
        let row = &SECTION_B[2];
        assert_eq!(value(&f, row.fields[0]).as_deref(), Some("03/2025"));
        assert_eq!(value(&f, row.fields[1]).as_deref(), Some("10,000"));
        assert_eq!(value(&f, row.fields[2]).as_deref(), Some("7"));
        assert_eq!(value(&f, row.fields[3]).as_deref(), Some("HY"));
        assert_eq!(value(&f, row.fields[4]).as_deref(), Some("200DB"));
        assert_eq!(value(&f, row.fields[5]).as_deref(), Some("1,429"));
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
        let (form, _) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        let row = &SECTION_B[10]; // 19j, first row
        assert_eq!(value(&f, row.fields[0]).as_deref(), Some("04/2025"));
        assert_eq!(value(&f, row.fields[1]).as_deref(), Some("390,000"));
        // The period, convention and method are printed on the form already and
        // must be left empty rather than stamped over.
        assert!(value(&f, row.fields[2]).unwrap_or_default().is_empty());
        assert!(value(&f, row.fields[3]).unwrap_or_default().is_empty());
        assert!(value(&f, row.fields[4]).unwrap_or_default().is_empty());
        assert!(!value(&f, row.fields[5]).unwrap_or_default().is_empty());
    }

    /// Two buildings from different months take the two printed rows, because
    /// mid-month makes the month part of the computation.
    #[test]
    fn two_months_of_real_property_take_the_two_printed_rows() {
        let assets = [
            asset("Building A", PropertyClass::Nonresidential, date(2025, 2, 1), 10_000_000),
            asset("Building B", PropertyClass::Nonresidential, date(2025, 9, 1), 10_000_000),
        ];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        assert_eq!(value(&f, SECTION_B[10].fields[0]).as_deref(), Some("02/2025"));
        assert_eq!(value(&f, SECTION_B[11].fields[0]).as_deref(), Some("09/2025"));
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
        let (form, warnings) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        let row = &SECTION_B[4]; // 19e
        assert_eq!(value(&f, row.fields[1]).as_deref(), Some("20,000"), "combined basis");
        let method = value(&f, row.fields[4]).unwrap_or_default();
        assert!(method.contains("150DB") && method.contains("S/L"), "{method}");

        assert!(
            warnings.iter().any(|w| w.contains("19e") || w.contains("two different ways")),
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
            asset("Old kiln", PropertyClass::SevenYear, date(2023, 3, 1), 1_000_000),
            asset("New kiln", PropertyClass::SevenYear, date(2025, 3, 1), 1_000_000),
        ];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        // 7-year year 3 is 17.49%.
        assert_eq!(value(&f, field::L17_PRIOR_YEARS).as_deref(), Some("1,749"));
        // Section B row 19c carries only the new one's basis.
        assert_eq!(value(&f, SECTION_B[2].fields[1]).as_deref(), Some("10,000"));
    }

    /// The mistake the form invites: line 22 includes §179 and line 16a must not.
    #[test]
    fn line_22_includes_section_179_and_line_16a_does_not() {
        let mut a = asset("Kiln", PropertyClass::SevenYear, date(2025, 3, 1), 1_000_000);
        a.section_179_cents = 400_000;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, warnings) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        assert_eq!(f.section_179_cents, 400_000);
        assert_eq!(f.line_22_cents, f.line_16a_cents + 400_000);
        assert_eq!(f.line_16a_cents, s.line_16a_cents());
        assert!(
            warnings.iter().any(|w| w.contains("deducts the §179 twice")),
            "{warnings:?}"
        );
    }

    /// Part I's arithmetic, including the phase-out, against 2025's figures.
    #[test]
    fn part_one_computes_the_2025_limit_and_phase_out() {
        let mut a = asset("Press", PropertyClass::SevenYear, date(2025, 3, 1), 500_000_00);
        a.section_179_cents = 400_000_00;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        assert_eq!(value(&f, field::L1_MAXIMUM).as_deref(), Some("2,500,000"));
        assert_eq!(value(&f, field::L3_THRESHOLD).as_deref(), Some("4,000,000"));
        assert_eq!(value(&f, field::L2_TOTAL_COST).as_deref(), Some("500,000"));
        // Well under the threshold, so no reduction and the full limit stands.
        assert_eq!(value(&f, field::L4_REDUCTION).as_deref(), Some("0"));
        assert_eq!(value(&f, field::L5_DOLLAR_LIMIT).as_deref(), Some("2,500,000"));
        assert_eq!(value(&f, field::L8_TOTAL_ELECTED).as_deref(), Some("400,000"));
        assert_eq!(value(&f, field::L12_DEDUCTION).as_deref(), Some("400,000"));
    }

    /// The two figures the books cannot supply are named rather than guessed.
    #[test]
    fn the_carryover_and_income_limitation_are_reported_as_unfilled() {
        let mut a = asset("Kiln", PropertyClass::SevenYear, date(2025, 3, 1), 1_000_000);
        a.section_179_cents = 400_000;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (_, warnings) = build(&profile(), &s, "Fine arts").unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("line 10") && w.contains("line 11")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_year_with_no_known_section_179_limit_says_so_rather_than_using_last_years() {
        let mut a = asset("Kiln", PropertyClass::SevenYear, date(2030, 3, 1), 1_000_000);
        a.section_179_cents = 400_000;
        let assets = [a];
        let s = compute_year(&assets, 2030);
        let (form, warnings) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        assert!(value(&f, field::L1_MAXIMUM).unwrap_or_default().is_empty());
        assert!(
            warnings.iter().any(|w| w.contains("indexed each year")),
            "{warnings:?}"
        );
    }

    #[test]
    fn bonus_reaches_line_fourteen() {
        let mut a = asset("Press", PropertyClass::FiveYear, date(2025, 6, 1), 1_000_000);
        a.acquired_on = date(2025, 6, 1);
        a.bonus = BonusElection::Take;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        assert_eq!(value(&f, field::L14_BONUS).as_deref(), Some("10,000"));
        assert_eq!(f.line_22_cents, 1_000_000);
    }

    /// Line 22 has to equal what the schedule says the year came to, or the form
    /// and the return disagree about the same number.
    #[test]
    fn line_22_equals_the_whole_year_the_register_computed() {
        let mut press = asset("Press", PropertyClass::FiveYear, date(2025, 6, 1), 2_000_000);
        press.acquired_on = date(2025, 6, 1);
        press.bonus = BonusElection::Take;
        let assets = [
            asset("Old kiln", PropertyClass::SevenYear, date(2023, 3, 1), 1_000_000),
            asset("New kiln", PropertyClass::SevenYear, date(2025, 3, 1), 1_000_000),
            press,
            asset("Fit-out", PropertyClass::QualifiedImprovement, date(2025, 5, 1), 5_000_000),
        ];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        assert_eq!(f.line_22_cents, s.total_cents());
        assert_eq!(f.line_16a_cents, s.line_16a_cents());
    }

    /// ADS property reports in Section C, and anything without a printed row of
    /// its own goes to "class life" with the period written in.
    #[test]
    fn ads_property_reports_in_section_c() {
        let mut a = asset("Press", PropertyClass::SevenYear, date(2025, 6, 1), 1_000_000);
        a.system = System::Ads;
        let assets = [a];
        let s = compute_year(&assets, 2025);
        let (form, _) = build(&profile(), &s, "Fine arts").unwrap();
        let f = form.expect("a form");

        // 20a class life, with the ADS 10-year period.
        assert_eq!(value(&f, SECTION_C[0].fields[2]).as_deref(), Some("10"));
        assert_eq!(value(&f, SECTION_C[0].fields[1]).as_deref(), Some("10,000"));
        // And nothing in Section B.
        assert!(value(&f, SECTION_B[2].fields[1]).unwrap_or_default().is_empty());
    }

    /// Every field this module names has to exist, or a revision has renumbered
    /// the form under us — the check the other form modules carry.
    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_form() {
        let mut doc = Document::load_mem(F4562).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);

        for name in [
            field::NAME,
            field::ACTIVITY,
            field::EIN,
            field::L1_MAXIMUM,
            field::L2_TOTAL_COST,
            field::L3_THRESHOLD,
            field::L4_REDUCTION,
            field::L5_DOLLAR_LIMIT,
            field::L8_TOTAL_ELECTED,
            field::L9_TENTATIVE,
            field::L12_DEDUCTION,
            field::L14_BONUS,
            field::L17_PRIOR_YEARS,
            field::L22_TOTAL,
        ] {
            assert!(map.find(name).is_some(), "f4562.pdf has no field {name}");
        }
        for row in field::L6_ROWS {
            for f in row {
                assert!(map.find(f).is_some(), "f4562.pdf has no line 6 field {f}");
            }
        }
        for (table, label) in [(&SECTION_B[..], "Section B"), (&SECTION_C[..], "Section C")] {
            for row in table {
                for f in row.fields {
                    assert!(map.find(f).is_some(), "f4562.pdf has no {label} field {f}");
                }
            }
        }
    }

    /// Every class this program models has to land on a row that exists.
    #[test]
    fn every_property_class_maps_to_a_printed_row() {
        for class in PropertyClass::ALL {
            assert!(section_b_row(class) < SECTION_B.len(), "{class:?} in Section B");
            assert!(section_c_row(class) < SECTION_C.len(), "{class:?} in Section C");
        }
    }
}
