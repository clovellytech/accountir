//! Schedule B-1, "Information on Partners Owning 50% or More of the
//! Partnership".
//!
//! Required whenever Schedule B question 2a or 2b is answered Yes. Part I lists
//! the *entities* that own 50% or more; Part II lists the *individuals and
//! estates*. Those two parts are exactly what the two questions ask about, which
//! is why one schedule answers both.
//!
//! # Why this can be filled from the books at all
//!
//! Almost everything else Schedule B asks about concerns third parties the
//! ledger has never heard of. This one does not: the partners, their percentages
//! and their identifying numbers are already here, because a Schedule K-1 needs
//! all three. Working out who crosses 50% is then arithmetic on data we hold.
//!
//! # What "50% or more" means here
//!
//! The form says "an interest of 50% or more in the profit, loss, **or**
//! capital". Or, not and — a partner at 60% of capital and 10% of profit is on
//! this schedule. So the test is the *largest* of a partner's three shares, and
//! that largest share is what column (v) reports.
//!
//! # What this does not do
//!
//! Constructive ownership. The form's instructions attribute a partner's
//! interest from family members and related entities, so somebody at 30% can be
//! treated as owning 50% through relations the books know nothing about. Only
//! direct ownership is filled, and [`fill`] says so whenever the schedule is
//! produced — a Schedule B-1 that looks complete and silently omits an
//! attributed owner is worse than none.

use crate::domain::{Partner, Residency};

use super::acroform::{field_map, set_text, strip_xfa, FormError};
use super::allocate::PPM_WHOLE;
use lopdf::Document;

const F1065_SB1: &[u8] = include_bytes!("../../assets/irs/f1065sb1.pdf");

/// The threshold the form names, in parts per million.
const FIFTY_PERCENT: i64 = PPM_WHOLE / 2;

/// Rows the printed schedule has, per part. Beyond this the IRS expects a
/// continuation sheet, which this does not produce — see [`fill`].
const ROWS: usize = 7;

/// Part I — entities. Five columns per row: name, EIN, type, country, percentage.
const PART_I: [[&str; 5]; ROWS] = [
    ["f1_3[0]", "f1_4[0]", "f1_5[0]", "f1_6[0]", "f1_7[0]"],
    ["f1_8[0]", "f1_9[0]", "f1_10[0]", "f1_11[0]", "f1_12[0]"],
    ["f1_13[0]", "f1_14[0]", "f1_15[0]", "f1_16[0]", "f1_17[0]"],
    ["f1_18[0]", "f1_19[0]", "f1_20[0]", "f1_21[0]", "f1_22[0]"],
    ["f1_23[0]", "f1_24[0]", "f1_25[0]", "f1_26[0]", "f1_27[0]"],
    ["f1_28[0]", "f1_29[0]", "f1_30[0]", "f1_31[0]", "f1_32[0]"],
    ["f1_33[0]", "f1_34[0]", "f1_35[0]", "f1_36[0]", "f1_37[0]"],
];

/// Part II — individuals and estates. Four columns: name, identifying number,
/// country of citizenship, percentage. No "type" column, because the part is
/// itself the type.
const PART_II: [[&str; 4]; ROWS] = [
    ["f1_38[0]", "f1_39[0]", "f1_40[0]", "f1_41[0]"],
    ["f1_42[0]", "f1_43[0]", "f1_44[0]", "f1_45[0]"],
    ["f1_46[0]", "f1_47[0]", "f1_48[0]", "f1_49[0]"],
    ["f1_50[0]", "f1_51[0]", "f1_52[0]", "f1_53[0]"],
    ["f1_54[0]", "f1_55[0]", "f1_56[0]", "f1_57[0]"],
    ["f1_58[0]", "f1_59[0]", "f1_60[0]", "f1_61[0]"],
    ["f1_62[0]", "f1_63[0]", "f1_64[0]", "f1_65[0]"],
];

const PARTNERSHIP_NAME: &str = "f1_1[0]";
const PARTNERSHIP_EIN: &str = "f1_2[0]";

/// A partner as this schedule reports them.
pub struct Owner<'a> {
    pub partner: &'a Partner,
    pub tin: Option<&'a str>,
}

/// The largest of a partner's three shares, in ppm — what the 50% test is
/// applied to and what column (v) reports.
pub fn largest_share_ppm(p: &Partner) -> i64 {
    p.shares
        .profit_ppm
        .max(p.shares.loss_ppm)
        .max(p.shares.capital_ppm)
}

/// Whether a partner crosses the threshold on any of their three shares.
pub fn owns_fifty_percent_or_more(p: &Partner) -> bool {
    largest_share_ppm(p) >= FIFTY_PERCENT
}

/// Whether a partner belongs in Part II rather than Part I.
///
/// Part II is "individuals or estates"; Part I is everything else — corporations,
/// partnerships, trusts, tax-exempt organisations, foreign governments. The
/// partner's `entity_type` is free text because the form's own answer is, so this
/// matches loosely and treats anything it does not recognise as an entity. That
/// direction is deliberate: Part I asks for an EIN and a type, so a misfiled
/// individual is visibly odd on the page, where a misfiled entity in Part II
/// silently loses the two columns that identify it.
pub fn is_individual_or_estate(p: &Partner) -> bool {
    let t = p.entity_type.trim().to_ascii_lowercase();
    t.is_empty() || t.contains("individual") || t.contains("estate") || t.contains("person")
}

/// Whether this schedule has to be attached, given the Schedule B answers.
pub fn is_required(answers: &super::schedule_b::ScheduleB) -> bool {
    let yes = |k: &str| answers.get(k) == Some(super::schedule_b::YES);
    yes("b2a") || yes("b2b")
}

/// A percentage as the form prints it, from parts per million.
fn percent(ppm: i64) -> String {
    let whole = ppm / 10_000;
    let frac = (ppm % 10_000) / 100;
    if frac == 0 {
        format!("{whole}")
    } else {
        format!("{whole}.{frac:02}")
    }
}

/// Build Schedule B-1 as its own document, or `None` when nobody crosses 50%.
///
/// Returning `None` on an empty schedule is not the same as it not being
/// required: a partnership can answer 2a Yes because of an owner the books do
/// not know about. [`fill`] is where that distinction is turned into a warning;
/// this only reports what it found.
pub fn build(
    legal_name: &str,
    ein: &str,
    owners: &[Owner<'_>],
) -> Result<(Option<Document>, Vec<String>), FormError> {
    let mut warnings = Vec::new();

    let (individuals, entities): (Vec<&Owner>, Vec<&Owner>) = owners
        .iter()
        .filter(|o| owns_fifty_percent_or_more(o.partner))
        .partition(|o| is_individual_or_estate(o.partner));

    if individuals.is_empty() && entities.is_empty() {
        return Ok((None, warnings));
    }

    let mut doc = Document::load_mem(F1065_SB1)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    set_text(&mut doc, &map, PARTNERSHIP_NAME, legal_name)?;
    set_text(&mut doc, &map, PARTNERSHIP_EIN, ein)?;

    for (row, o) in entities.iter().take(ROWS).enumerate() {
        let p = o.partner;
        let cols = PART_I[row];
        set_text(&mut doc, &map, cols[0], &p.name)?;
        set_text(&mut doc, &map, cols[1], o.tin.unwrap_or(""))?;
        set_text(&mut doc, &map, cols[2], &p.entity_type)?;
        set_text(&mut doc, &map, cols[3], &country_of(p))?;
        set_text(&mut doc, &map, cols[4], &percent(largest_share_ppm(p)))?;
        if o.tin.is_none() {
            warnings.push(format!(
                "Schedule B-1: no identifying number is held on this machine for {}, so column \
                 (ii) is blank.",
                p.name
            ));
        }
    }

    for (row, o) in individuals.iter().take(ROWS).enumerate() {
        let p = o.partner;
        let cols = PART_II[row];
        set_text(&mut doc, &map, cols[0], &p.name)?;
        set_text(&mut doc, &map, cols[1], o.tin.unwrap_or(""))?;
        set_text(&mut doc, &map, cols[2], &country_of(p))?;
        set_text(&mut doc, &map, cols[3], &percent(largest_share_ppm(p)))?;
        if o.tin.is_none() {
            warnings.push(format!(
                "Schedule B-1: no identifying number is held on this machine for {}, so column \
                 (ii) is blank.",
                p.name
            ));
        }
    }

    for (part, n) in [("I", entities.len()), ("II", individuals.len())] {
        if n > ROWS {
            warnings.push(format!(
                "Schedule B-1 Part {part} has {n} owners and the printed schedule has {ROWS} rows. \
                 The first {ROWS} were filled; the rest need a continuation sheet, which this \
                 program does not produce."
            ));
        }
    }

    Ok((Some(doc), warnings))
}

/// Country of organisation or citizenship, as the form asks for it.
///
/// The books hold an address and a domestic/foreign flag rather than a
/// nationality. A domestic partner is United States; a foreign one is whatever
/// their address says, and blank when it says nothing — a guessed nationality on
/// a return is worse than an empty box somebody has to fill.
fn country_of(p: &Partner) -> String {
    match p.residency {
        Residency::Domestic => "United States".to_string(),
        Residency::Foreign => p.address.country.clone().unwrap_or_default(),
    }
}

/// The caveat that goes with every Schedule B-1 this program produces.
pub const CONSTRUCTIVE_OWNERSHIP_CAVEAT: &str =
    "Schedule B-1 lists direct ownership only. The instructions also attribute interests held by \
     family members and related entities, which the books do not know about — check whether \
     anybody reaches 50% that way before filing.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Shares};
    use crate::tax::acroform::get_value;
    use chrono::NaiveDate;

    fn partner(name: &str, entity_type: &str, profit: f64, loss: f64, capital: f64) -> Partner {
        Partner {
            partner_id: name.to_lowercase(),
            name: name.to_string(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: entity_type.to_string(),
            address: Address {
                street: "1 Main".into(),
                suite: None,
                city: "Town".into(),
                state: "TX".into(),
                postal_code: "70000".into(),
                country: None,
            },
            start_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            end_date: None,
            shares: Shares::from_percents(profit, loss, capital),
        }
    }

    /// "Profit, loss, **or** capital" — a partner over the line on any one of the
    /// three is on the schedule, even at a small profit share.
    #[test]
    fn the_test_is_the_largest_of_the_three_shares_not_the_profit_share() {
        let p = partner("Big Capital LLC", "Partnership", 10.0, 10.0, 60.0);
        assert!(owns_fifty_percent_or_more(&p));
        assert_eq!(largest_share_ppm(&p), 600_000);

        let q = partner("Small", "Individual", 10.0, 10.0, 10.0);
        assert!(!owns_fifty_percent_or_more(&q));
    }

    /// Exactly 50% is "50% or more".
    #[test]
    fn exactly_fifty_percent_is_included() {
        let p = partner("Half", "Individual", 50.0, 50.0, 50.0);
        assert!(owns_fifty_percent_or_more(&p));
        let q = partner("Just under", "Individual", 49.9999, 49.9999, 49.9999);
        assert!(!owns_fifty_percent_or_more(&q));
    }

    #[test]
    fn individuals_and_estates_go_to_part_two_and_everything_else_to_part_one() {
        assert!(is_individual_or_estate(&partner("A", "Individual", 50.0, 50.0, 50.0)));
        assert!(is_individual_or_estate(&partner(
            "B",
            "Estate of Deceased Partner",
            50.0,
            50.0,
            50.0
        )));
        assert!(!is_individual_or_estate(&partner("C", "S Corporation", 50.0, 50.0, 50.0)));
        assert!(!is_individual_or_estate(&partner("D", "Trust", 50.0, 50.0, 50.0)));
        // Unrecognised text is treated as an entity — see the doc comment.
        assert!(!is_individual_or_estate(&partner("E", "Grantor Vehicle", 50.0, 50.0, 50.0)));
    }

    #[test]
    fn the_schedule_is_required_when_either_question_says_yes() {
        use crate::tax::schedule_b::{ScheduleB, NO, YES};
        let mut a = ScheduleB::default();
        assert!(!is_required(&a));
        a.set("b2a", NO);
        a.set("b2b", NO);
        assert!(!is_required(&a));
        a.set("b2b", YES);
        assert!(is_required(&a));
    }

    #[test]
    fn nobody_over_the_threshold_produces_no_schedule() {
        let p = partner("Small", "Individual", 10.0, 10.0, 10.0);
        let owners = vec![Owner { partner: &p, tin: None }];
        let (doc, _) = build("Acme LLP", "12-3456789", &owners).unwrap();
        assert!(doc.is_none());
    }

    #[test]
    fn an_entity_and_an_individual_land_in_their_own_parts() {
        let e = partner("Holdings LLC", "Partnership", 60.0, 60.0, 60.0);
        let i = partner("Dana Whitlock", "Individual", 55.0, 55.0, 55.0);
        let owners = vec![
            Owner { partner: &e, tin: Some("98-7654321") },
            Owner { partner: &i, tin: Some("111-22-3333") },
        ];
        let (doc, _) = build("Acme LLP", "12-3456789", &owners).unwrap();
        let doc = doc.unwrap();
        let map = field_map(&doc);

        assert_eq!(get_value(&doc, &map, PARTNERSHIP_NAME).as_deref(), Some("Acme LLP"));
        assert_eq!(get_value(&doc, &map, PARTNERSHIP_EIN).as_deref(), Some("12-3456789"));

        // Part I row 1: the entity.
        assert_eq!(get_value(&doc, &map, PART_I[0][0]).as_deref(), Some("Holdings LLC"));
        assert_eq!(get_value(&doc, &map, PART_I[0][1]).as_deref(), Some("98-7654321"));
        assert_eq!(get_value(&doc, &map, PART_I[0][2]).as_deref(), Some("Partnership"));
        assert_eq!(get_value(&doc, &map, PART_I[0][4]).as_deref(), Some("60"));

        // Part II row 1: the individual, with no type column.
        assert_eq!(get_value(&doc, &map, PART_II[0][0]).as_deref(), Some("Dana Whitlock"));
        assert_eq!(get_value(&doc, &map, PART_II[0][1]).as_deref(), Some("111-22-3333"));
        assert_eq!(get_value(&doc, &map, PART_II[0][3]).as_deref(), Some("55"));
    }

    /// A missing TIN leaves a visibly empty box and says so, the same rule the
    /// K-1 follows.
    #[test]
    fn a_missing_identifying_number_is_reported_rather_than_invented() {
        let i = partner("Dana Whitlock", "Individual", 55.0, 55.0, 55.0);
        let owners = vec![Owner { partner: &i, tin: None }];
        let (doc, warnings) = build("Acme LLP", "12-3456789", &owners).unwrap();
        assert!(doc.is_some());
        assert!(
            warnings.iter().any(|w| w.contains("Dana Whitlock")),
            "{warnings:?}"
        );
    }

    /// More owners than the printed schedule has rows must not be silently
    /// dropped — that is a return that omits an owner it declared.
    #[test]
    fn more_owners_than_rows_are_reported() {
        let ps: Vec<Partner> = (0..9)
            .map(|i| partner(&format!("Owner {i}"), "Individual", 60.0, 60.0, 60.0))
            .collect();
        let owners: Vec<Owner> = ps.iter().map(|p| Owner { partner: p, tin: None }).collect();
        let (doc, warnings) = build("Acme LLP", "12-3456789", &owners).unwrap();
        assert!(doc.is_some());
        assert!(
            warnings.iter().any(|w| w.contains("continuation sheet")),
            "{warnings:?}"
        );
    }

    #[test]
    fn percentages_print_the_way_the_form_expects() {
        assert_eq!(percent(1_000_000), "100");
        assert_eq!(percent(500_000), "50");
        assert_eq!(percent(333_333), "33.33");
        assert_eq!(percent(605_000), "60.50");
    }

    /// Every field this module names has to exist, or a revision has renumbered
    /// the schedule under us.
    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_schedule() {
        let mut doc = Document::load_mem(F1065_SB1).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        for name in [PARTNERSHIP_NAME, PARTNERSHIP_EIN] {
            assert!(map.find(name).is_some(), "f1065sb1.pdf has no field {name}");
        }
        for row in PART_I {
            for f in row {
                assert!(map.find(f).is_some(), "f1065sb1.pdf has no Part I field {f}");
            }
        }
        for row in PART_II {
            for f in row {
                assert!(map.find(f).is_some(), "f1065sb1.pdf has no Part II field {f}");
            }
        }
    }
}
