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
//! # Constructive ownership, and where it stops
//!
//! The form's instructions apply the constructive-ownership rules of IRC §267(c),
//! so somebody at 40% can be treated as owning 60% through a spouse. The **family**
//! half of that — §267(c)(2), spouses, siblings, ancestors, lineal descendants — is
//! computed here, from the relationships the books record: every [`Owner`] arrives
//! with a `constructive` share that already includes it, and the 50% test and the
//! reported percentage both use that. Two spouses at 40% and 20% each land on the
//! schedule at 60%.
//!
//! What is **not** computed is attribution through **entities** — a partner that is
//! itself owned by someone who also holds a direct interest (§267(c)(1) and (c)(3)),
//! which needs ownership facts about those entities the books do not hold — and any
//! family tie nobody entered. [`CONSTRUCTIVE_OWNERSHIP_CAVEAT`] says so on every
//! schedule produced, because one that looks complete and silently omits an
//! attributed owner is worse than none.

use crate::domain::{Partner, Residency, Shares};

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
    /// The partner's ownership **after §267(c) family attribution** — what the
    /// 50% test is applied to and what column (v)/(iv) reports. It equals the
    /// partner's own direct shares when no recorded relationship touches them, and
    /// is larger when one does: two spouses at 40% and 20% each carry 60% here.
    ///
    /// Computed by the caller, which is the only place that holds both the full
    /// partner list and the relationships between them — see
    /// [`crate::tax::constructive::constructive_shares`]. Passing it in rather than
    /// recomputing it keeps this module about *laying out the form* and the tax
    /// rule in one place.
    pub constructive: Shares,
}

/// The largest of three shares, in ppm — the value the 50% test is applied to and
/// what column (v)/(iv) reports.
pub fn largest_ppm(s: &Shares) -> i64 {
    s.profit_ppm.max(s.loss_ppm).max(s.capital_ppm)
}

/// The largest of a partner's three **direct** shares, in ppm.
///
/// Kept for callers reasoning about direct ownership; the schedule itself works
/// from [`Owner::constructive`], because the form's own test is a constructive one.
pub fn largest_share_ppm(p: &Partner) -> i64 {
    largest_ppm(&p.shares)
}

/// Whether a partner crosses the threshold on any of their three **direct** shares.
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

/// Where the Schedule B answers and the partner data disagree about who owns 50%
/// or more.
///
/// [`is_required`] reads the answers and nothing else, which is right: the two
/// questions are the filer's claim, and a Yes can rest on an owner the books have
/// never met — an interest held through a related entity, a family tie nobody
/// entered. [`build`] already warns about that direction, where the answer says
/// Yes and no partner here crosses the line.
///
/// The other direction is not symmetric, and it is the dangerous one. When the
/// books *do* hold a partner over the line and the question says No, one of the
/// two is wrong — and nothing in this module can notice, because [`is_required`]
/// never looks at a partner and [`build`] is only reached once the answer already
/// says Yes. The whole B-1 path is skipped, the schedule is silently left off, and
/// the return is short a form it may owe. So this is checked on every build,
/// independently of the answers, from the same constructive shares the schedule
/// itself would report.
///
/// It names the partners and their percentages rather than only the question,
/// because "somebody crosses 50%" sends you back through the partner list to work
/// out who — and the cause is usually one share or one relationship typed wrong.
pub fn contradictions(answers: &super::schedule_b::ScheduleB, owners: &[Owner<'_>]) -> Vec<String> {
    // (key, number, part, whom the question asks about, which part they land in)
    const QUESTIONS: [(&str, &str, &str, &str, bool); 2] = [
        (
            "b2a",
            "2a",
            "I",
            "corporation, partnership, trust, tax-exempt organization or foreign government",
            false,
        ),
        ("b2b", "2b", "II", "individual or estate", true),
    ];

    let mut out = Vec::new();

    for (key, number, part, asks_about, part_ii) in QUESTIONS {
        if answers.get(key) == Some(super::schedule_b::YES) {
            continue;
        }

        let over: Vec<&Owner> = owners
            .iter()
            .filter(|o| is_individual_or_estate(o.partner) == part_ii)
            .filter(|o| largest_ppm(&o.constructive) >= FIFTY_PERCENT)
            .collect();
        if over.is_empty() {
            continue;
        }

        let named = over
            .iter()
            .map(|o| {
                format!(
                    "{} at {}%",
                    o.partner.name,
                    percent(largest_ppm(&o.constructive))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");

        // An unanswered question is the same defect as a wrong one — the schedule
        // is equally absent — but it is a different mistake to go and fix, so it
        // is worth saying which of the two happened.
        let stance = if answers.get(key) == Some(super::schedule_b::NO) {
            format!("Schedule B question {number} is answered No")
        } else {
            format!("Schedule B question {number} has not been answered")
        };

        out.push(format!(
            "{stance}, but the books put somebody over the line: {named}. The question asks \
             whether any {asks_about} owns, directly or indirectly, 50% or more of the profit, \
             loss, or capital — any one of the three, not all of them — and those percentages \
             already carry the §267(c) family attribution from the relationships on file, which \
             is what the question's own instructions call for. Either the answer or the partner \
             data is wrong. While it stands, no Schedule B-1 Part {part} is produced and the \
             return goes out without a schedule it owes."
        ));
    }

    out
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
        .filter(|o| largest_ppm(&o.constructive) >= FIFTY_PERCENT)
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
        set_text(
            &mut doc,
            &map,
            cols[4],
            &percent(largest_ppm(&o.constructive)),
        )?;
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
        set_text(
            &mut doc,
            &map,
            cols[3],
            &percent(largest_ppm(&o.constructive)),
        )?;
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
    "Schedule B-1 applies §267(c) family attribution from the partner relationships on file \
     (spouse, sibling, parent/child). It does not attribute ownership held through related \
     entities, and it only knows the relationships you have entered — check both before filing.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Shares};
    use crate::tax::acroform::get_value;
    use chrono::NaiveDate;

    fn partner(name: &str, entity_type: &str, profit: f64, loss: f64, capital: f64) -> Partner {
        Partner {
            history: Vec::new(),
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

    /// An owner whose constructive share is just their direct share — the common
    /// case, with no relationship attributing anything to them.
    fn owner<'a>(p: &'a Partner, tin: Option<&'a str>) -> Owner<'a> {
        Owner {
            partner: p,
            tin,
            constructive: p.shares,
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
        assert!(is_individual_or_estate(&partner(
            "A",
            "Individual",
            50.0,
            50.0,
            50.0
        )));
        assert!(is_individual_or_estate(&partner(
            "B",
            "Estate of Deceased Partner",
            50.0,
            50.0,
            50.0
        )));
        assert!(!is_individual_or_estate(&partner(
            "C",
            "S Corporation",
            50.0,
            50.0,
            50.0
        )));
        assert!(!is_individual_or_estate(&partner(
            "D", "Trust", 50.0, 50.0, 50.0
        )));
        // Unrecognised text is treated as an entity — see the doc comment.
        assert!(!is_individual_or_estate(&partner(
            "E",
            "Grantor Vehicle",
            50.0,
            50.0,
            50.0
        )));
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
        let owners = vec![owner(&p, None)];
        let (doc, _) = build("Acme LLP", "12-3456789", &owners).unwrap();
        assert!(doc.is_none());
    }

    /// The scenario this feature exists for: 40% and 20% married. Under §267(c)
    /// family attribution each owns the couple's 60%, so **both** appear on the
    /// schedule at 60 — neither owns 50% directly, and without the relationship
    /// neither would be here at all.
    #[test]
    fn two_spouses_below_the_line_each_land_on_the_schedule_at_their_combined_share() {
        use crate::domain::PartnerRelationship;
        use crate::domain::RelationshipKind::Spouse;
        use crate::tax::constructive::constructive_shares;

        let mut me = partner("Zak", "Individual", 40.0, 40.0, 40.0);
        me.partner_id = "me".into();
        let mut wife = partner("Wife", "Individual", 20.0, 20.0, 20.0);
        wife.partner_id = "wife".into();
        let all = [me.clone(), wife.clone()];
        let rels = [PartnerRelationship::new("me", "wife", Spouse)];

        let owners: Vec<Owner> = all
            .iter()
            .map(|p| Owner {
                partner: p,
                tin: None,
                constructive: constructive_shares(p, &all, &rels),
            })
            .collect();

        let (doc, _) = build("Acme LLP", "12-3456789", &owners).unwrap();
        let doc = doc.expect("both spouses cross 50% by attribution");
        let map = field_map(&doc);

        // Both in Part II (individuals), both reported at 60.
        assert_eq!(get_value(&doc, &map, PART_II[0][0]).as_deref(), Some("Zak"));
        assert_eq!(get_value(&doc, &map, PART_II[0][3]).as_deref(), Some("60"));
        assert_eq!(
            get_value(&doc, &map, PART_II[1][0]).as_deref(),
            Some("Wife")
        );
        assert_eq!(get_value(&doc, &map, PART_II[1][3]).as_deref(), Some("60"));
    }

    /// The control: the exact same two partners, with no relationship recorded,
    /// produce no schedule — the attribution is what puts them on it, and it is
    /// only ever applied to ties the books actually hold.
    #[test]
    fn the_same_two_partners_unrelated_are_not_on_the_schedule() {
        let me = partner("Zak", "Individual", 40.0, 40.0, 40.0);
        let wife = partner("Wife", "Individual", 20.0, 20.0, 20.0);
        let owners = vec![owner(&me, None), owner(&wife, None)];
        let (doc, _) = build("Acme LLP", "12-3456789", &owners).unwrap();
        assert!(doc.is_none(), "nobody owns 50% directly");
    }

    #[test]
    fn an_entity_and_an_individual_land_in_their_own_parts() {
        let e = partner("Holdings LLC", "Partnership", 60.0, 60.0, 60.0);
        let i = partner("Dana Whitlock", "Individual", 55.0, 55.0, 55.0);
        let owners = vec![
            owner(&e, Some("98-7654321")),
            owner(&i, Some("111-22-3333")),
        ];
        let (doc, _) = build("Acme LLP", "12-3456789", &owners).unwrap();
        let doc = doc.unwrap();
        let map = field_map(&doc);

        assert_eq!(
            get_value(&doc, &map, PARTNERSHIP_NAME).as_deref(),
            Some("Acme LLP")
        );
        assert_eq!(
            get_value(&doc, &map, PARTNERSHIP_EIN).as_deref(),
            Some("12-3456789")
        );

        // Part I row 1: the entity.
        assert_eq!(
            get_value(&doc, &map, PART_I[0][0]).as_deref(),
            Some("Holdings LLC")
        );
        assert_eq!(
            get_value(&doc, &map, PART_I[0][1]).as_deref(),
            Some("98-7654321")
        );
        assert_eq!(
            get_value(&doc, &map, PART_I[0][2]).as_deref(),
            Some("Partnership")
        );
        assert_eq!(get_value(&doc, &map, PART_I[0][4]).as_deref(), Some("60"));

        // Part II row 1: the individual, with no type column.
        assert_eq!(
            get_value(&doc, &map, PART_II[0][0]).as_deref(),
            Some("Dana Whitlock")
        );
        assert_eq!(
            get_value(&doc, &map, PART_II[0][1]).as_deref(),
            Some("111-22-3333")
        );
        assert_eq!(get_value(&doc, &map, PART_II[0][3]).as_deref(), Some("55"));
    }

    /// A missing TIN leaves a visibly empty box and says so, the same rule the
    /// K-1 follows.
    #[test]
    fn a_missing_identifying_number_is_reported_rather_than_invented() {
        let i = partner("Dana Whitlock", "Individual", 55.0, 55.0, 55.0);
        let owners = vec![owner(&i, None)];
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
        let owners: Vec<Owner> = ps.iter().map(|p| owner(p, None)).collect();
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
                assert!(
                    map.find(f).is_some(),
                    "f1065sb1.pdf has no Part I field {f}"
                );
            }
        }
        for row in PART_II {
            for f in row {
                assert!(
                    map.find(f).is_some(),
                    "f1065sb1.pdf has no Part II field {f}"
                );
            }
        }
    }

    // --- the answers against the books ---

    /// The gap this check exists for: the question says No, a partner in the books
    /// owns 60%, and every other part of the module is happy — `is_required` says
    /// no schedule and `build` is never called, so nothing else in a build would
    /// ever mention it.
    #[test]
    fn an_answer_of_no_over_a_partner_who_crosses_the_line_is_reported() {
        use crate::tax::schedule_b::{ScheduleB, NO};
        let p = partner("Dana Whitlock", "Individual", 60.0, 10.0, 10.0);
        let owners = vec![owner(&p, None)];

        let mut a = ScheduleB::default();
        a.set("b2a", NO);
        a.set("b2b", NO);

        let w = contradictions(&a, &owners);
        assert_eq!(w.len(), 1, "only 2b is contradicted: {w:?}");
        assert!(w[0].contains("2b is answered No"), "{w:?}");
        assert!(w[0].contains("Dana Whitlock at 60%"), "{w:?}");
        assert!(!is_required(&a), "the answers alone still say no schedule");
    }

    /// Two spouses at 51% and 49%. Neither number looks like 50% or more on the
    /// partner screen, which is exactly why answering 2b No is an easy mistake —
    /// §267(c) puts both at 100%, and both belong on the schedule.
    #[test]
    fn married_partners_below_the_line_individually_still_contradict_a_no() {
        use crate::domain::PartnerRelationship;
        use crate::domain::RelationshipKind::Spouse;
        use crate::tax::constructive::constructive_shares;
        use crate::tax::schedule_b::{ScheduleB, NO};

        let mut a_p = partner("Jinny", "Individual", 51.0, 51.0, 51.0);
        a_p.partner_id = "jinny".into();
        let mut b_p = partner("Zak", "Individual", 49.0, 49.0, 49.0);
        b_p.partner_id = "zak".into();
        let all = [a_p.clone(), b_p.clone()];
        let rels = [PartnerRelationship::new("jinny", "zak", Spouse)];

        let owners: Vec<Owner> = all
            .iter()
            .map(|p| Owner {
                partner: p,
                tin: None,
                constructive: constructive_shares(p, &all, &rels),
            })
            .collect();

        let mut answers = ScheduleB::default();
        answers.set("b2b", NO);

        let w = contradictions(&answers, &owners);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("Jinny at 100%"), "{w:?}");
        assert!(w[0].contains("Zak at 100%"), "{w:?}");
    }

    /// A Yes is the filer's claim and the schedule is built from it — there is
    /// nothing to reconcile, and a warning here would fire on every correct return.
    #[test]
    fn a_yes_is_never_a_contradiction() {
        use crate::tax::schedule_b::{ScheduleB, YES};
        let p = partner("Dana Whitlock", "Individual", 60.0, 60.0, 60.0);
        let owners = vec![owner(&p, None)];

        let mut a = ScheduleB::default();
        a.set("b2b", YES);
        assert!(contradictions(&a, &owners).is_empty());
    }

    /// An unanswered question leaves the schedule off just as surely as a wrong
    /// one, so it is reported — but as the different mistake it is.
    #[test]
    fn an_unanswered_question_over_the_line_says_so() {
        use crate::tax::schedule_b::ScheduleB;
        let p = partner("Dana Whitlock", "Individual", 60.0, 60.0, 60.0);
        let owners = vec![owner(&p, None)];

        let w = contradictions(&ScheduleB::default(), &owners);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("2b has not been answered"), "{w:?}");
    }

    /// The two questions are about different kinds of owner, and each is checked
    /// against the part it feeds: an entity over the line contradicts 2a, and
    /// leaves a No on 2b entirely alone.
    #[test]
    fn an_entity_contradicts_2a_and_an_individual_2b() {
        use crate::tax::schedule_b::{ScheduleB, NO, YES};
        let e = partner("Holdings LLC", "Partnership", 60.0, 60.0, 60.0);
        let owners = vec![owner(&e, None)];

        let mut a = ScheduleB::default();
        a.set("b2a", NO);
        a.set("b2b", NO);

        let w = contradictions(&a, &owners);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("2a is answered No"), "{w:?}");
        assert!(w[0].contains("Part I"), "{w:?}");

        // And the mirror image: answering 2a Yes clears it, while 2b stays quiet
        // because no individual is over the line.
        a.set("b2a", YES);
        assert!(contradictions(&a, &owners).is_empty());
    }

    /// Nobody over the line is the ordinary return — no warning, whatever the
    /// answers say.
    #[test]
    fn nobody_over_the_line_never_contradicts_anything() {
        use crate::tax::schedule_b::{ScheduleB, NO};
        let p = partner("Small", "Individual", 10.0, 10.0, 10.0);
        let owners = vec![owner(&p, None)];

        let mut a = ScheduleB::default();
        a.set("b2a", NO);
        a.set("b2b", NO);
        assert!(contradictions(&a, &owners).is_empty());
    }

    /// Every cell sits in the column its heading names, and on its own row.
    ///
    /// # Why a name check is not enough here either
    ///
    /// This schedule is a grid, and a grid is where an off-by-one hides best:
    /// shift `PART_I` by one and every partner's EIN prints in the "type of
    /// entity" column while their name prints where the row number goes. Every
    /// name still exists, every row still has five of them, and the page looks
    /// filled in. The Form 4562 had exactly this shape of defect and carried it
    /// through two revisions.
    ///
    /// The bands come off the printed headings: (i) name at x=36, (ii) EIN at
    /// 238, (iii) type at 325, (iv) country at 411, (v) percentage at 497. Part
    /// II has no type column, so its country box spans (iii) and (iv).
    #[test]
    fn every_cell_is_in_the_column_its_heading_names() {
        let mut doc = Document::load_mem(F1065_SB1).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);

        let left = |name: &str| -> (f64, f64) {
            let id = map
                .find(name)
                .unwrap_or_else(|| panic!("the schedule has no {name}"));
            let d = doc.get_object(id).and_then(lopdf::Object::as_dict).unwrap();
            let r = d.get(b"Rect").and_then(lopdf::Object::as_array).unwrap();
            let n = |i: usize| {
                r[i].as_float()
                    .map(f64::from)
                    .unwrap_or_else(|_| r[i].as_i64().unwrap() as f64)
            };
            (n(0), n(1))
        };

        // Part I: five columns, seven rows, each row strictly below the last.
        let mut previous_row: Option<f64> = None;
        for (r, row) in PART_I.iter().enumerate() {
            let xs = [36.0, 238.0, 325.0, 411.0, 497.0];
            let mut y = None;
            for (c, (name, want_x)) in row.iter().zip(xs).enumerate() {
                let (x, at) = left(name);
                assert!(
                    (x - want_x).abs() < 4.0,
                    "Part I row {r} column {c} ({name}) is at x={x:.0}, not the {want_x:.0} \
                     column its heading is over"
                );
                match y {
                    None => y = Some(at),
                    Some(first) => assert!(
                        (at - first).abs() < 3.0,
                        "Part I row {r} column {c} ({name}) is on a different row from its \
                         own row's first cell"
                    ),
                }
            }
            if let (Some(previous), Some(this)) = (previous_row, y) {
                assert!(this < previous, "Part I row {r} is not below row {}", r - 1);
            }
            previous_row = y;
        }

        // Part II: four columns — no "type of entity", because the part is the
        // type — so its third box spans what Part I splits into (iii) and (iv).
        let mut previous_row: Option<f64> = None;
        for (r, row) in PART_II.iter().enumerate() {
            let xs = [36.0, 238.0, 325.0, 497.0];
            let mut y = None;
            for (c, (name, want_x)) in row.iter().zip(xs).enumerate() {
                let (x, at) = left(name);
                assert!(
                    (x - want_x).abs() < 4.0,
                    "Part II row {r} column {c} ({name}) is at x={x:.0}, not the {want_x:.0} \
                     column its heading is over"
                );
                match y {
                    None => y = Some(at),
                    Some(first) => assert!((at - first).abs() < 3.0),
                }
            }
            if let (Some(previous), Some(this)) = (previous_row, y) {
                assert!(
                    this < previous,
                    "Part II row {r} is not below row {}",
                    r - 1
                );
            }
            previous_row = y;
        }

        // Part II must sit below Part I: they are different parts of the page,
        // and a table that overlapped them would put an individual on an
        // entity's row.
        assert!(
            left(PART_II[0][0]).1 < left(PART_I[ROWS - 1][0]).1,
            "Part II starts above the end of Part I"
        );
    }

    /// No two cells write the same box.
    #[test]
    fn no_two_cells_share_a_box() {
        let mut seen = std::collections::HashSet::new();
        for row in PART_I.iter() {
            for f in row {
                assert!(seen.insert(*f), "{f} is used twice");
            }
        }
        for row in PART_II.iter() {
            for f in row {
                assert!(seen.insert(*f), "{f} is used twice");
            }
        }
        assert!(seen.insert(PARTNERSHIP_NAME));
        assert!(seen.insert(PARTNERSHIP_EIN));
    }
}
