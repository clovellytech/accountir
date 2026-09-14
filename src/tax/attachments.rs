//! What travels with the return, and whether this program produces it.
//!
//! # Why this is one list computed in one place
//!
//! A return is not one document. Answering Schedule B question 2a Yes obliges a
//! Schedule B-1; question 31 obliges a B-2; a figure on line 21 obliges a
//! statement; a depreciation figure obliges a Form 4562 this program has never
//! seen. Some of those we produce, and some the filer has to fetch and fill
//! themselves — and the difference is invisible from inside the PDF, which looks
//! equally finished either way.
//!
//! So the list is computed once, from the answers and the figures, and used
//! twice: the desktop shows it beside the Generate button so somebody knows what
//! they are about to get *before* they get it, and the build folds the
//! not-produced ones into its warnings. Two separate lists would eventually
//! disagree, and the disagreement would be a form somebody was told they did not
//! need.

use super::lines::{Form1065Lines, Schedule, MAPPABLE_LINES};
use super::schedule_b::ScheduleB;

/// Whether the return carries this attachment already, or somebody has to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Produced here and appended to the PDF.
    Generated,
    /// Obliged, but not something this program can produce. The filer fetches it.
    YourJob,
}

/// One thing that travels with the return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: &'static str,
    /// What made it necessary, in the terms the person reading it decided in.
    pub because: String,
    pub provenance: Provenance,
    /// Where to read about it, for the ones we do not produce.
    pub url: &'static str,
}

/// Everything this return needs beside Form 1065 itself.
///
/// Ordered: what we produce first, then what the filer owes. A list that
/// interleaved them would make the second group easy to skim past, and the
/// second group is the one with a deadline attached.
pub fn required(
    answers: &ScheduleB,
    lines: &Form1065Lines,
    partner_count: usize,
    schedule_l_mapped: bool,
    asset_register_size: usize,
) -> Vec<Attachment> {
    let mut generated = Vec::new();
    let mut owed = Vec::new();

    // --- Schedules K-1: one per partner, always ---
    if partner_count > 0 {
        generated.push(Attachment {
            name: "Schedule K-1 (Form 1065)",
            because: format!(
                "{partner_count} partner(s) held an interest during the year — one each."
            ),
            provenance: Provenance::Generated,
            url: "https://www.irs.gov/forms-pubs/about-schedule-k-1-form-1065",
        });
    }

    // --- Schedule B-1 ---
    if super::schedule_b1::is_required(answers) {
        let which = match (
            answers.get("b2a") == Some(super::schedule_b::YES),
            answers.get("b2b") == Some(super::schedule_b::YES),
        ) {
            (true, true) => "questions 2a and 2b are Yes",
            (true, false) => "question 2a is Yes",
            _ => "question 2b is Yes",
        };
        generated.push(Attachment {
            name: "Schedule B-1 (Form 1065)",
            because: format!("Schedule B {which} — partners owning 50% or more."),
            provenance: Provenance::Generated,
            url: "https://www.irs.gov/pub/irs-pdf/f1065sb1.pdf",
        });
    }

    // --- Schedule B-2 ---
    if super::schedule_b2::is_required(answers) {
        generated.push(Attachment {
            name: "Schedule B-2 (Form 1065)",
            because:
                "Schedule B question 31 is Yes — electing out of the centralized audit regime."
                    .to_string(),
            provenance: Provenance::Generated,
            url: "https://www.irs.gov/instructions/i1065sb2",
        });
    }

    // --- Schedule L ---
    //
    // The exemption is question 4, not the absence of a mapping: a partnership
    // that answered 4 No owes the schedule whether or not anybody mapped the
    // accounts for it, and that gap is the whole reason to say so here.
    let exempt = answers.get("b4") == Some(super::schedule_b::YES);
    if !exempt {
        if schedule_l_mapped {
            generated.push(Attachment {
                name: "Schedule L — balance sheets per books",
                because: "Schedule B question 4 is not Yes, so the balance sheet is required."
                    .to_string(),
                provenance: Provenance::Generated,
                url: "https://www.irs.gov/pub/irs-pdf/i1065.pdf",
            });
        } else {
            owed.push(Attachment {
                name: "Schedule L — balance sheets per books",
                because: "Schedule B question 4 is not Yes, so it is required — but no \
                          balance-sheet account is mapped to a Schedule L line."
                    .to_string(),
                provenance: Provenance::YourJob,
                url: "https://www.irs.gov/pub/irs-pdf/i1065.pdf",
            });
        }
    }

    // --- The line-by-line obligations ---
    //
    // Driven off the catalogue rather than a second list here, so a line that
    // gains or loses an attachment in a future revision changes in one place.
    for def in MAPPABLE_LINES {
        let Some(a) = def.attachment else { continue };
        let carries = match def.schedule {
            Schedule::L => schedule_l_mapped && lines.is_mapped(def.key),
            _ => lines.is_mapped(def.key),
        };
        if !carries {
            continue;
        }
        let where_ = match def.schedule {
            Schedule::Page1 => format!("Page 1, line {}", def.number),
            Schedule::K => format!("Schedule K, line {}", def.number),
            Schedule::L => format!("Schedule L, line {}", def.number),
        };
        // Form 4562 is the one attachment whose provenance is not a property of
        // the line. It is produced here when there is an asset register to
        // produce it from, and owed to the filer when there is not — the same
        // form, and which of the two it is depends on whether anybody has
        // entered the assets.
        let is_4562 = a.name == "Form 4562";
        let produced = a.generated || (is_4562 && asset_register_size > 0);
        // The nondeductible-expenses statement is the one attachment that is not
        // a single page supporting a single box. The entity's Schedule K figure
        // gets one, and so does every partner's share of it, because item L row
        // 4 and box 18 code C reach each partner as a bare figure on a row the
        // K-1 instructions say to attach an explanation for. Said here because a
        // filer counting pages against this list would otherwise find more of
        // them than they were told to expect.
        let is_nondeductible = a.name == super::lines::NONDEDUCTIBLE_STATEMENT.name;
        let because = if is_4562 && produced {
            format!(
                "{where_} carries a figure, and the register holds {asset_register_size} asset(s) — the schedule is computed from them."
            )
        } else if is_4562 {
            format!(
                "{where_} carries a figure, and no depreciable asset is on the register. Enter the assets to have this computed, or fill it in yourself."
            )
        } else if is_nondeductible {
            format!(
                "{where_} carries a figure. One statement itemises it for the partnership, and \
                 each of the {partner_count} partner(s) with a share of it gets their own share \
                 of the same list behind their Schedule K-1 — it is the explanation item L, row 4 \
                 asks for. A partner the line allocates nothing to gets no page, so there may be \
                 fewer of these than there are partners."
            )
        } else {
            format!("{where_} carries a figure.")
        };
        let entry = Attachment {
            name: a.name,
            because,
            provenance: if produced {
                Provenance::Generated
            } else {
                Provenance::YourJob
            },
            url: a.url,
        };
        if produced {
            generated.push(entry);
        } else {
            owed.push(entry);
        }
    }

    // --- The ones a Yes obliges but nothing here can produce ---
    for (key, name, url, because) in YES_OBLIGES {
        if answers.get(key) == Some(super::schedule_b::YES) {
            owed.push(Attachment {
                name,
                because: because.to_string(),
                provenance: Provenance::YourJob,
                url,
            });
        }
    }

    generated.extend(owed);
    generated
}

/// Schedule B answers that oblige a form this program does not produce.
///
/// Kept beside the questions rather than inside them because a question's
/// `refs` are things to *read* while answering — several questions link a form
/// they only mention. This list is narrower: a Yes here means a filing is owed.
const YES_OBLIGES: &[(&str, &str, &str, &str)] = &[
    (
        "b8",
        "FinCEN Form 114 (FBAR)",
        "https://www.irs.gov/businesses/small-businesses-self-employed/report-of-foreign-bank-and-financial-accounts-fbar",
        "Schedule B question 8 is Yes — a foreign financial account. Filed with FinCEN, separately from this return.",
    ),
    (
        "b10b",
        "Section 743(b) basis adjustment statement",
        "https://www.irs.gov/pub/irs-pdf/i1065.pdf",
        "Schedule B question 10b is Yes — the computation and allocation of each adjustment.",
    ),
    (
        "b10c",
        "Section 734(b) basis adjustment statement",
        "https://www.irs.gov/pub/irs-pdf/i1065.pdf",
        "Schedule B question 10c is Yes — the computation and allocation of each adjustment.",
    ),
    (
        "b10d",
        "Substantial built-in loss statement",
        "https://www.irs.gov/pub/irs-pdf/i1065.pdf",
        "Schedule B question 10d is Yes — the computation and allocation of the basis adjustment.",
    ),
    (
        "b24",
        "Form 8990",
        "https://www.irs.gov/forms-pubs/about-form-8990",
        "Schedule B question 24 is Yes — the business interest expense limitation.",
    ),
    (
        "b25",
        "Form 8996",
        "https://www.irs.gov/forms-pubs/about-form-8996",
        "Schedule B question 25 is Yes — self-certifying as a qualified opportunity fund.",
    ),
    (
        "b29a",
        "Form 7208",
        "https://www.irs.gov/forms-pubs/about-form-7208",
        "Schedule B question 29a is Yes — excise tax on repurchase of corporate stock.",
    ),
    (
        "b29b",
        "Form 7208",
        "https://www.irs.gov/forms-pubs/about-form-7208",
        "Schedule B question 29b is Yes — covered surrogate foreign corporation rules.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::schedule_b::{NO, YES};

    fn names(list: &[Attachment]) -> Vec<&str> {
        list.iter().map(|a| a.name).collect()
    }

    /// Form 4562 is the one attachment that changes hands: owed to the filer
    /// with no register, produced here with one — the same line, the same figure,
    /// and the difference is whether anybody entered the assets.
    #[test]
    fn form_4562_is_owed_without_a_register_and_produced_with_one() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l16a", 8_420);

        let owed = required(&a, &lines, 1, false, 0);
        let f = owed.iter().find(|x| x.name == "Form 4562").expect("listed");
        assert_eq!(f.provenance, Provenance::YourJob);
        assert!(f.because.contains("no depreciable asset"), "{}", f.because);

        let produced = required(&a, &lines, 1, false, 3);
        let f = produced
            .iter()
            .find(|x| x.name == "Form 4562")
            .expect("listed");
        assert_eq!(f.provenance, Provenance::Generated);
        assert!(f.because.contains("3 asset(s)"), "{}", f.because);
    }

    /// A figure on line 18c obliges a statement, and the list has to say the
    /// statement is one page per partner as well as one for the partnership —
    /// somebody counting pages against this list would otherwise find more of
    /// them than they were promised.
    #[test]
    fn line_18c_obliges_a_statement_the_list_says_is_per_partner() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        let mut lines = Form1065Lines::default();
        lines.set_for_test("k18c", 140);

        let list = required(&a, &lines, 2, false, 0);
        let s = list
            .iter()
            .find(|x| x.name == "Nondeductible expenses statement")
            .unwrap_or_else(|| panic!("not listed: {:?}", names(&list)));
        assert_eq!(s.provenance, Provenance::Generated);
        assert!(s.because.contains("Schedule K, line 18c"), "{}", s.because);
        assert!(s.because.contains("2 partner(s)"), "{}", s.because);
        assert!(s.because.contains("item L"), "{}", s.because);
    }

    /// And nothing on the line obliges nothing.
    #[test]
    fn no_nondeductible_expenses_oblige_no_statement() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        let list = required(&a, &Form1065Lines::default(), 2, false, 0);
        assert!(!names(&list).contains(&"Nondeductible expenses statement"));
    }

    #[test]
    fn a_bare_return_carries_only_its_k1s() {
        let mut a = ScheduleB::default();
        // Question 4 Yes takes Schedule L off the list.
        a.set("b4", YES);
        let list = required(&a, &Form1065Lines::default(), 2, false, 0);
        assert_eq!(names(&list), vec!["Schedule K-1 (Form 1065)"]);
    }

    #[test]
    fn question_2a_brings_schedule_b1_and_says_which_question_did_it() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        a.set("b2a", YES);
        let list = required(&a, &Form1065Lines::default(), 1, false, 0);
        let b1 = list
            .iter()
            .find(|x| x.name.starts_with("Schedule B-1"))
            .unwrap();
        assert_eq!(b1.provenance, Provenance::Generated);
        assert!(b1.because.contains("question 2a"), "{}", b1.because);

        a.set("b2b", YES);
        let list = required(&a, &Form1065Lines::default(), 1, false, 0);
        let b1 = list
            .iter()
            .find(|x| x.name.starts_with("Schedule B-1"))
            .unwrap();
        assert!(b1.because.contains("2a and 2b"), "{}", b1.because);
    }

    #[test]
    fn question_31_brings_schedule_b2() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        a.set("b31", YES);
        let list = required(&a, &Form1065Lines::default(), 1, false, 0);
        assert!(names(&list).iter().any(|n| n.starts_with("Schedule B-2")));

        a.set("b31", NO);
        let list = required(&a, &Form1065Lines::default(), 1, false, 0);
        assert!(!names(&list).iter().any(|n| n.starts_with("Schedule B-2")));
    }

    /// Question 4 is the exemption, not the mapping. A partnership that owes the
    /// balance sheet and has not mapped it must still be told it owes one.
    #[test]
    fn schedule_l_is_owed_when_question_4_is_not_yes_even_with_nothing_mapped() {
        let a = ScheduleB::default();
        let list = required(&a, &Form1065Lines::default(), 1, false, 0);
        let l = list
            .iter()
            .find(|x| x.name.starts_with("Schedule L"))
            .unwrap();
        assert_eq!(l.provenance, Provenance::YourJob);

        let list = required(&a, &Form1065Lines::default(), 1, true, 0);
        let l = list
            .iter()
            .find(|x| x.name.starts_with("Schedule L"))
            .unwrap();
        assert_eq!(l.provenance, Provenance::Generated);
    }

    #[test]
    fn line_21_brings_a_statement_and_line_2_brings_form_1125a_both_produced_here() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l21", 5000);
        lines.set_for_test("l2", 1000);

        let list = required(&a, &lines, 1, false, 0);
        let stmt = list
            .iter()
            .find(|x| x.name.contains("Other deductions"))
            .unwrap();
        assert_eq!(stmt.provenance, Provenance::Generated);
        let f1125a = list.iter().find(|x| x.name == "Form 1125-A").unwrap();
        assert_eq!(f1125a.provenance, Provenance::Generated);
    }

    /// What we produce comes first, so the list that has a deadline attached is
    /// not the one somebody skims past.
    #[test]
    fn generated_attachments_are_listed_before_the_ones_you_owe() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        a.set("b8", YES);
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l21", 5000);

        let list = required(&a, &lines, 2, false, 0);
        let first_owed = list
            .iter()
            .position(|x| x.provenance == Provenance::YourJob)
            .unwrap();
        assert!(
            list[..first_owed]
                .iter()
                .all(|x| x.provenance == Provenance::Generated),
            "{:?}",
            names(&list)
        );
    }

    #[test]
    fn a_yes_that_obliges_a_separate_filing_appears_on_the_list() {
        let mut a = ScheduleB::default();
        a.set("b4", YES);
        a.set("b24", YES);
        let list = required(&a, &Form1065Lines::default(), 1, false, 0);
        let f = list.iter().find(|x| x.name == "Form 8990").unwrap();
        assert_eq!(f.provenance, Provenance::YourJob);
        assert!(f.because.contains("question 24"), "{}", f.because);
    }

    /// Every URL has to be reachable from the panel that shows it.
    #[test]
    fn every_attachment_names_somewhere_to_read_about_it() {
        let mut a = ScheduleB::default();
        for q in [
            "b2a", "b2b", "b8", "b10b", "b10c", "b10d", "b24", "b25", "b29a", "b29b", "b31",
        ] {
            a.set(q, YES);
        }
        let mut lines = Form1065Lines::default();
        for def in MAPPABLE_LINES {
            lines.set_for_test(def.key, 100);
        }
        let list = required(&a, &lines, 3, true, 0);
        assert!(!list.is_empty());
        for at in &list {
            assert!(
                at.url.starts_with("https://www.irs.gov/"),
                "{} has a non-IRS url {}",
                at.name,
                at.url
            );
            assert!(!at.because.is_empty(), "{} says nothing about why", at.name);
        }
    }
}
