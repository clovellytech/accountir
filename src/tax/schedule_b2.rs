//! Schedule B-2, "Election Out of the Centralized Partnership Audit Regime".
//!
//! Required whenever Schedule B question 31 is answered Yes. Part I lists every
//! eligible partner with their TIN and a one-letter type code; Part III totals
//! the Schedules K-1 the election covers, and Form 1065 question 31 carries that
//! total back onto page 4.
//!
//! # Why the election is worth getting right
//!
//! Without it the IRS audits and collects at the *partnership* level, which
//! means an adjustment years later is paid by whoever the partners happen to be
//! then rather than by whoever they were in the year adjusted. Electing out is
//! how a small partnership keeps that from happening — and the election is only
//! valid if this schedule is complete. A Yes on question 31 with no B-2 attached
//! is an election the IRS can simply decline to recognise.
//!
//! # Eligibility, and why this program does not decide it
//!
//! The election is open to partnerships with 100 or fewer Schedules K-1 where
//! *every* partner is an individual, a C corporation, a foreign entity that
//! would be a C corporation if domestic, an S corporation, or the estate of a
//! deceased partner. A partnership with a partnership as a partner is not
//! eligible at all. This module counts, checks the hundred, and reports what it
//! sees — but the entity types come from free text on each partner, so whether
//! the roster really is all-eligible is a judgement it names rather than makes.
//!
//! # What is left blank
//!
//! Part II lists the shareholders of any S corporation partner, and Part V
//! continues it. The books hold nothing about another company's shareholders, so
//! those parts stay empty and editable and [`build`] warns when an S corporation
//! is on the roster at all.

use crate::domain::{Partner, Residency};

use super::acroform::{field_map, set_text, strip_xfa, FormError};
use lopdf::Document;

const F1065_SB2: &[u8] = include_bytes!("../../assets/irs/f1065sb2.pdf");

/// Rows in Part I on page 1. Beyond this the schedule continues into Part IV,
/// which this does not fill — see [`build`].
const ROWS: usize = 15;

/// Part I — three columns per row: name, TIN, type code.
const PART_I: [[&str; 3]; ROWS] = [
    ["f1_3[0]", "f1_4[0]", "f1_5[0]"],
    ["f1_6[0]", "f1_7[0]", "f1_8[0]"],
    ["f1_9[0]", "f1_10[0]", "f1_11[0]"],
    ["f1_12[0]", "f1_13[0]", "f1_14[0]"],
    ["f1_15[0]", "f1_16[0]", "f1_17[0]"],
    ["f1_18[0]", "f1_19[0]", "f1_20[0]"],
    ["f1_21[0]", "f1_22[0]", "f1_23[0]"],
    ["f1_24[0]", "f1_25[0]", "f1_26[0]"],
    ["f1_27[0]", "f1_28[0]", "f1_29[0]"],
    ["f1_30[0]", "f1_31[0]", "f1_32[0]"],
    ["f1_33[0]", "f1_34[0]", "f1_35[0]"],
    ["f1_36[0]", "f1_37[0]", "f1_38[0]"],
    ["f1_39[0]", "f1_40[0]", "f1_41[0]"],
    ["f1_42[0]", "f1_43[0]", "f1_44[0]"],
    ["f1_45[0]", "f1_46[0]", "f1_47[0]"],
];

const PARTNERSHIP_NAME: &str = "f1_1[0]";
const PARTNERSHIP_EIN: &str = "f1_2[0]";

/// Part III. Line 1 is the partnership's own K-1s, line 2 the S corporation
/// shareholders', line 3 the total that Form 1065 question 31 reports.
const TOTAL_PARTNERSHIP: &str = "f1_86[0]";
const TOTAL_S_CORP_SHAREHOLDERS: &str = "f1_87[0]";
const TOTAL: &str = "f1_88[0]";

/// The most Schedules K-1 an electing-out partnership may have.
pub const MAX_ELIGIBLE_PARTNERS: usize = 100;

/// A partner as this schedule reports them.
pub struct Eligible<'a> {
    pub partner: &'a Partner,
    pub tin: Option<&'a str>,
}

/// The one-letter code Part I asks for.
///
/// `None` where the partner's `entity_type` does not match anything the election
/// allows — a partnership, a trust, a tax-exempt organisation. That is not a
/// formatting gap: a partner of that kind makes the partnership *ineligible*, so
/// the right response is an empty box and a warning, never a letter chosen to
/// make the row look finished.
pub fn type_code(p: &Partner) -> Option<&'static str> {
    let t = p.entity_type.trim().to_ascii_lowercase();
    // Checked before "corporation" so an S corporation is not read as a C one.
    if t.starts_with('s') && t.contains("corp") {
        return Some("S");
    }
    if t.contains("estate") {
        return Some("E");
    }
    if t.contains("individual") || t.contains("person") {
        // A foreign individual is still an individual; the eligible-foreign-entity
        // code is for entities, not people.
        return Some("I");
    }
    if t.contains("corp") {
        return Some(match p.residency {
            Residency::Foreign => "F",
            Residency::Domestic => "C",
        });
    }
    None
}

/// Whether this schedule has to be attached, given the Schedule B answers.
pub fn is_required(answers: &super::schedule_b::ScheduleB) -> bool {
    answers.get("b31") == Some(super::schedule_b::YES)
}

/// Build Schedule B-2, and the total that Form 1065 question 31 should carry.
///
/// Returns `None` when there are no partners at all — a schedule listing nobody
/// asserts an election over an empty roster.
pub fn build(
    legal_name: &str,
    ein: &str,
    partners: &[Eligible<'_>],
) -> Result<(Option<Document>, usize, Vec<String>), FormError> {
    let mut warnings = Vec::new();
    if partners.is_empty() {
        return Ok((None, 0, warnings));
    }

    let mut doc = Document::load_mem(F1065_SB2)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    set_text(&mut doc, &map, PARTNERSHIP_NAME, legal_name)?;
    set_text(&mut doc, &map, PARTNERSHIP_EIN, ein)?;

    let mut ineligible: Vec<&str> = Vec::new();
    let mut missing_tin: Vec<&str> = Vec::new();
    let mut s_corps: Vec<&str> = Vec::new();

    for (row, e) in partners.iter().take(ROWS).enumerate() {
        let p = e.partner;
        let cols = PART_I[row];
        set_text(&mut doc, &map, cols[0], &p.name)?;
        set_text(&mut doc, &map, cols[1], e.tin.unwrap_or(""))?;
        match type_code(p) {
            Some(code) => {
                set_text(&mut doc, &map, cols[2], code)?;
                if code == "S" {
                    s_corps.push(&p.name);
                }
            }
            None => ineligible.push(&p.name),
        }
        if e.tin.is_none() {
            missing_tin.push(&p.name);
        }
    }

    // Line 1 counts every partner, not just the rows that fit — the total has to
    // be the truth even when the printed rows are not all of it.
    let count = partners.len();
    set_text(&mut doc, &map, TOTAL_PARTNERSHIP, &count.to_string())?;
    // Line 2 is S corporation shareholders, which the books do not hold. Left
    // blank rather than zeroed: a printed 0 claims somebody checked.
    set_text(&mut doc, &map, TOTAL_S_CORP_SHAREHOLDERS, "")?;
    set_text(&mut doc, &map, TOTAL, &count.to_string())?;

    if count > ROWS {
        warnings.push(format!(
            "Schedule B-2 Part I has {ROWS} printed rows and this partnership has {count} \
             partners. The first {ROWS} were filled; the rest belong on Part IV, which this \
             program does not produce."
        ));
    }
    if count > MAX_ELIGIBLE_PARTNERS {
        warnings.push(format!(
            "Schedule B-2 line 3 is {count}, which is more than {MAX_ELIGIBLE_PARTNERS}. A \
             partnership over that limit cannot elect out under section 6221(b) — question 31 \
             should be No."
        ));
    }
    if !ineligible.is_empty() {
        warnings.push(format!(
            "Schedule B-2: no eligible-partner code fits {} — the election is only open when every \
             partner is an individual, a C corporation, an eligible foreign entity, an S \
             corporation or a deceased partner's estate. A partnership or trust as a partner makes \
             the election unavailable.",
            ineligible.join(", ")
        ));
    }
    if !s_corps.is_empty() {
        warnings.push(format!(
            "Schedule B-2: {} is an S corporation, so Part II has to list its shareholders and \
             they count toward the 100. The books hold nothing about another company's \
             shareholders, so Part II and line 2 are blank.",
            s_corps.join(", ")
        ));
    }
    if !missing_tin.is_empty() {
        warnings.push(format!(
            "Schedule B-2: no taxpayer identification number is held on this machine for {}, so \
             those rows are incomplete. An incomplete Part I is grounds for the IRS to treat the \
             election as invalid.",
            missing_tin.join(", ")
        ));
    }

    Ok((Some(doc), count, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Shares};
    use crate::tax::acroform::get_value;
    use chrono::NaiveDate;

    fn partner(name: &str, entity_type: &str, residency: Residency) -> Partner {
        Partner {
            partner_id: name.to_lowercase(),
            name: name.to_string(),
            partner_type: PartnerType::General,
            residency,
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
            shares: Shares::from_percents(50.0, 50.0, 50.0),
        }
    }

    #[test]
    fn the_schedule_is_required_only_when_question_31_says_yes() {
        use crate::tax::schedule_b::{ScheduleB, NO, YES};
        let mut a = ScheduleB::default();
        assert!(!is_required(&a));
        a.set("b31", NO);
        assert!(!is_required(&a));
        a.set("b31", YES);
        assert!(is_required(&a));
    }

    /// An S corporation must not be read as a C corporation — they are different
    /// codes and the S one drags Part II along with it.
    #[test]
    fn each_eligible_kind_gets_its_own_code() {
        assert_eq!(type_code(&partner("A", "Individual", Residency::Domestic)), Some("I"));
        assert_eq!(type_code(&partner("B", "Estate of Deceased Partner", Residency::Domestic)), Some("E"));
        assert_eq!(type_code(&partner("C", "S Corporation", Residency::Domestic)), Some("S"));
        assert_eq!(type_code(&partner("D", "C Corporation", Residency::Domestic)), Some("C"));
        assert_eq!(type_code(&partner("E", "Corporation", Residency::Foreign)), Some("F"));
        // A foreign individual is still an individual.
        assert_eq!(type_code(&partner("F", "Individual", Residency::Foreign)), Some("I"));
    }

    /// A partner the election does not allow gets no code — and no guess.
    #[test]
    fn an_ineligible_partner_gets_no_code_and_a_warning() {
        let p = partner("Holdings LP", "Partnership", Residency::Domestic);
        assert_eq!(type_code(&p), None);

        let ps = vec![Eligible { partner: &p, tin: Some("98-7654321") }];
        let (doc, _, warnings) = build("Acme LLP", "12-3456789", &ps).unwrap();
        let doc = doc.unwrap();
        let map = field_map(&doc);
        assert_eq!(get_value(&doc, &map, PART_I[0][2]), None, "no code may be invented");
        assert!(warnings.iter().any(|w| w.contains("Holdings LP")), "{warnings:?}");
    }

    #[test]
    fn part_one_carries_each_partner_and_part_three_totals_them() {
        let a = partner("Alice Reyes", "Individual", Residency::Domestic);
        let b = partner("Ben Osei", "Individual", Residency::Domestic);
        let ps = vec![
            Eligible { partner: &a, tin: Some("111-22-3333") },
            Eligible { partner: &b, tin: Some("444-55-6666") },
        ];
        let (doc, count, _) = build("Acme LLP", "12-3456789", &ps).unwrap();
        let doc = doc.unwrap();
        let map = field_map(&doc);

        assert_eq!(count, 2);
        assert_eq!(get_value(&doc, &map, PART_I[0][0]).as_deref(), Some("Alice Reyes"));
        assert_eq!(get_value(&doc, &map, PART_I[0][1]).as_deref(), Some("111-22-3333"));
        assert_eq!(get_value(&doc, &map, PART_I[0][2]).as_deref(), Some("I"));
        assert_eq!(get_value(&doc, &map, PART_I[1][0]).as_deref(), Some("Ben Osei"));
        assert_eq!(get_value(&doc, &map, TOTAL_PARTNERSHIP).as_deref(), Some("2"));
        assert_eq!(get_value(&doc, &map, TOTAL).as_deref(), Some("2"));
    }

    /// The total counts every partner, not only the ones that fit on the page —
    /// a line 3 that matched the printed rows would understate the election.
    #[test]
    fn the_total_counts_partners_beyond_the_printed_rows() {
        let ps: Vec<Partner> = (0..20)
            .map(|i| partner(&format!("Partner {i}"), "Individual", Residency::Domestic))
            .collect();
        let es: Vec<Eligible> = ps.iter().map(|p| Eligible { partner: p, tin: Some("111-22-3333") }).collect();
        let (doc, count, warnings) = build("Acme LLP", "12-3456789", &es).unwrap();
        let doc = doc.unwrap();
        let map = field_map(&doc);

        assert_eq!(count, 20);
        assert_eq!(get_value(&doc, &map, TOTAL).as_deref(), Some("20"));
        assert!(warnings.iter().any(|w| w.contains("Part IV")), "{warnings:?}");
    }

    /// Over a hundred and the election is not available at all.
    #[test]
    fn more_than_a_hundred_partners_is_called_out_as_ineligible() {
        let ps: Vec<Partner> = (0..101)
            .map(|i| partner(&format!("Partner {i}"), "Individual", Residency::Domestic))
            .collect();
        let es: Vec<Eligible> = ps.iter().map(|p| Eligible { partner: p, tin: None }).collect();
        let (_, count, warnings) = build("Acme LLP", "12-3456789", &es).unwrap();
        assert_eq!(count, 101);
        assert!(
            warnings.iter().any(|w| w.contains("cannot elect out")),
            "{warnings:?}"
        );
    }

    #[test]
    fn an_s_corporation_partner_drags_part_two_along_and_says_so() {
        let p = partner("Osei S Corp", "S Corporation", Residency::Domestic);
        let ps = vec![Eligible { partner: &p, tin: Some("98-7654321") }];
        let (_, _, warnings) = build("Acme LLP", "12-3456789", &ps).unwrap();
        assert!(warnings.iter().any(|w| w.contains("shareholders")), "{warnings:?}");
    }

    /// An incomplete Part I can invalidate the election, so a missing TIN is not
    /// a cosmetic gap.
    #[test]
    fn a_missing_tin_warns_that_the_election_may_be_invalid() {
        let p = partner("Alice Reyes", "Individual", Residency::Domestic);
        let ps = vec![Eligible { partner: &p, tin: None }];
        let (_, _, warnings) = build("Acme LLP", "12-3456789", &ps).unwrap();
        assert!(warnings.iter().any(|w| w.contains("invalid")), "{warnings:?}");
    }

    #[test]
    fn no_partners_produces_no_schedule() {
        let (doc, count, _) = build("Acme LLP", "12-3456789", &[]).unwrap();
        assert!(doc.is_none());
        assert_eq!(count, 0);
    }

    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_schedule() {
        let mut doc = Document::load_mem(F1065_SB2).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        for name in [
            PARTNERSHIP_NAME,
            PARTNERSHIP_EIN,
            TOTAL_PARTNERSHIP,
            TOTAL_S_CORP_SHAREHOLDERS,
            TOTAL,
        ] {
            assert!(map.find(name).is_some(), "f1065sb2.pdf has no field {name}");
        }
        for row in PART_I {
            for f in row {
                assert!(map.find(f).is_some(), "f1065sb2.pdf has no Part I field {f}");
            }
        }
    }
}
