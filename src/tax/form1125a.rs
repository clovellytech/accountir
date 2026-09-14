//! Form 1125-A, "Cost of Goods Sold".
//!
//! Attached whenever page 1 line 2 carries a figure, and whenever the books hold
//! inventory at either end of the year. A partnership that bought stock for
//! resale owes the form even in a year nothing sold — which is the year its
//! inventory is easiest to lose track of.
//!
//! # Where the figures come from
//!
//! Nowhere new. Line 8 is page 1 line 2, and lines 1 and 7 are Schedule L line 3
//! at the start and end of the year: three figures the return already prints.
//! Line 2 is what makes them agree —
//!
//! ```text
//! purchases = cost of goods sold + ending inventory − beginning inventory
//! ```
//!
//! That identity holds whichever way the books track stock. Purchases expensed
//! and inventory adjusted at year end, or purchases capitalised and relieved to
//! cost of goods sold as they sell — both arrive at the same line 2. Deriving it,
//! rather than summing some other set of accounts, is what stops this form from
//! disagreeing with the page it supports.
//!
//! # What is left blank
//!
//! Lines 3 to 5 — labour, section 263A costs and other costs. When the books have
//! any, they are inside the derived purchases figure, and the ledger cannot tell
//! them apart from it.
//!
//! # Question 9
//!
//! Methods and elections: how closing inventory is valued, LIFO, section 263A, a
//! change in method. None of that is in the ledger, and a ticked box is a
//! statement about the business, so it is answered by a person and kept with the
//! Schedule B answers for the year — the same per-year store, the same events,
//! the same page — under keys of its own ([`QUESTION_9`]). Nothing is ticked that
//! nobody answered; what is missing is named in the warnings.
//!
//! # Revisions
//!
//! Two are carried. The November 2018 revision serves tax years through 2023 and
//! gives every amount a separate cents box. The November 2024 revision serves
//! 2024 onward: it drops the cents boxes and adds three valuation methods and a
//! LIFO reserve line to question 9, which renumbers every check box after 9a.
//! Each revision names only its own boxes.

use super::acroform::{field_map, set_check, set_text, strip_xfa, FieldMap, FormError};
use super::lines::{format_dollars, Form1065Lines};
use super::schedule_b::{
    Asked, Control, FollowUp, FollowUpWhen, FormRef, InputKind, Question, QuestionBoxes,
    ScheduleB, NO, YES,
};
use super::schedule_l::ScheduleL;
use lopdf::Document;

/// One revision's blank and the boxes this module fills on it.
pub struct Revision {
    /// The first tax year this revision is used for.
    pub from_year: i32,
    pub label: &'static str,
    pub form: &'static [u8],
    pub boxes: Boxes,
}

/// A revision's boxes, named for what they mean.
pub struct Boxes {
    pub name: &'static str,
    pub ein: &'static str,
    /// The dollar box of lines 1 through 8, in order. Lines 3 to 5 are named so
    /// the tests hold them to the form, although nothing is written there.
    pub lines: [&'static str; 8],
    /// Question 9 as this revision prints it: its numbers and its boxes. The
    /// meaning of each key is in [`QUESTION_9`].
    pub question_9: &'static [QuestionBoxes],
}

const fn q9(key: &'static str, number: &'static str, control: Control) -> QuestionBoxes {
    QuestionBoxes {
        key,
        number,
        control,
        reserved: false,
    }
}

/// Question 9 on the November 2018 revision: three valuation methods and one
/// LIFO figure.
const QUESTION_9_2018: &[QuestionBoxes] = &[
    q9("a9a_cost", "9a(i)", Control::Check { field: "c1_1[0]" }),
    q9("a9a_lcm", "9a(ii)", Control::Check { field: "c1_2[0]" }),
    q9("a9a_other", "9a(iii)", Control::Check { field: "c1_3[0]" }),
    q9("a9b", "9b", Control::Check { field: "c1_4[0]" }),
    q9("a9c", "9c", Control::Check { field: "c1_5[0]" }),
    q9(
        "a9d",
        "9d",
        Control::Entry {
            field: "f1_20[0]",
            kind: InputKind::Money,
        },
    ),
    q9(
        "a9e",
        "9e",
        Control::YesNo {
            yes: "c1_6[0]",
            no: "c1_6[1]",
        },
    ),
    q9(
        "a9f",
        "9f",
        Control::YesNo {
            yes: "c1_7[0]",
            no: "c1_7[1]",
        },
    ),
];

/// Question 9 on the November 2024 revision, which adds the small business
/// taxpayer methods to 9a and the LIFO reserve as 9d(ii) — renumbering every box
/// after 9a(iii).
const QUESTION_9_2024: &[QuestionBoxes] = &[
    q9("a9a_cost", "9a(i)", Control::Check { field: "c1_1[0]" }),
    q9("a9a_lcm", "9a(ii)", Control::Check { field: "c1_2[0]" }),
    q9("a9a_other", "9a(iii)", Control::Check { field: "c1_3[0]" }),
    q9("a9a_nims", "9a(iv)", Control::Check { field: "c1_4[0]" }),
    q9("a9a_afs", "9a(v)", Control::Check { field: "c1_5[0]" }),
    q9("a9a_non_afs", "9a(vi)", Control::Check { field: "c1_6[0]" }),
    q9("a9b", "9b", Control::Check { field: "c1_7[0]" }),
    q9("a9c", "9c", Control::Check { field: "c1_8[0]" }),
    q9(
        "a9d",
        "9d(i)",
        Control::Entry {
            field: "f1_20[0]",
            kind: InputKind::Money,
        },
    ),
    q9(
        "a9d_reserve",
        "9d(ii)",
        Control::Entry {
            field: "f1_22[0]",
            kind: InputKind::Money,
        },
    ),
    q9(
        "a9e",
        "9e",
        Control::YesNo {
            yes: "c1_9[0]",
            no: "c1_9[1]",
        },
    ),
    q9(
        "a9f",
        "9f",
        Control::YesNo {
            yes: "c1_10[0]",
            no: "c1_10[1]",
        },
    ),
];

/// The appearance state every question 9 box is ticked with, on both revisions:
/// `Yes` for a check box and the Yes half of a pair, `No` for the No half.
const ON_YES: &str = "Yes";
const ON_NO: &str = "No";

const FORM_970: FormRef = FormRef {
    name: "Form 970",
    url: "https://www.irs.gov/forms-pubs/about-form-970",
};

const OTHER_METHOD: FollowUp = FollowUp {
    key: "a9a_other_method",
    label: "Method used",
    field: "f1_19[0]",
    kind: InputKind::Text,
};

const fn meaning(key: &'static str, text: &'static str) -> Question {
    Question {
        key,
        page: 1,
        text,
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        depends_on: None,
    }
}

/// What each question 9 key asks, in the form's words. Stored with the Schedule
/// B answers for the year; see the module docs.
pub const QUESTION_9: &[Question] = &[
    meaning(
        "a9a_cost",
        "Check all methods used for valuing closing inventory: Cost.",
    ),
    meaning(
        "a9a_lcm",
        "Check all methods used for valuing closing inventory: Lower of cost or market.",
    ),
    Question {
        follow_ups: &[(FollowUpWhen::Yes, OTHER_METHOD)],
        ..meaning(
            "a9a_other",
            "Check all methods used for valuing closing inventory: Other (specify method used \
             and attach explanation).",
        )
    },
    meaning(
        "a9a_nims",
        "For certain small business taxpayers, alternative methods of accounting for \
         inventories: Non-incidental materials and supplies method.",
    ),
    meaning(
        "a9a_afs",
        "For certain small business taxpayers, alternative methods of accounting for \
         inventories: AFS method.",
    ),
    meaning(
        "a9a_non_afs",
        "For certain small business taxpayers, alternative methods of accounting for \
         inventories: Non-AFS method.",
    ),
    meaning("a9b", "Check if there was a writedown of subnormal goods."),
    Question {
        refs: &[FORM_970],
        ..meaning(
            "a9c",
            "Check if the LIFO inventory method was adopted this tax year for any goods (if \
             checked, attach Form 970).",
        )
    },
    meaning(
        "a9d",
        "If the LIFO inventory method was used for this tax year, enter amount of closing \
         inventory figured under LIFO.",
    ),
    meaning(
        "a9d_reserve",
        "If the LIFO inventory method was used for this tax year, enter amount of the closing \
         LIFO Reserve.",
    ),
    meaning(
        "a9e",
        "If property is produced or acquired for resale, do the rules of section 263A apply to \
         the entity? See instructions.",
    ),
    meaning(
        "a9f",
        "Was there any change in determining quantities, cost, or valuations between opening \
         and closing inventory? If \u{201c}Yes,\u{201d} attach explanation.",
    ),
];

/// The question 9 items a tax year's revision asks, in the order it prints them.
pub fn asked(year: i32) -> Vec<Asked> {
    revision_for(year)
        .boxes
        .question_9
        .iter()
        .filter_map(|boxes| {
            QUESTION_9
                .iter()
                .find(|q| q.key == boxes.key)
                .map(|meaning| Asked { meaning, boxes })
        })
        .collect()
}

/// Whether a key is a question 9 answer or one of its follow-ups.
pub fn known_key(key: &str) -> bool {
    QUESTION_9
        .iter()
        .any(|q| q.key == key || q.follow_ups.iter().any(|(_, f)| f.key == key))
}

/// What question 9 still needs, as printed numbers: 9a when no valuation method
/// is checked, and each Yes/No left unanswered. A lone box is not listed — an
/// unticked box is its negative answer — nor are the LIFO figures, which are
/// correctly blank without LIFO.
pub fn unanswered(answers: &ScheduleB, year: i32) -> Vec<&'static str> {
    let asked = asked(year);
    let mut out = Vec::new();
    if !asked
        .iter()
        .any(|q| q.key().starts_with("a9a_") && answers.get(q.key()) == Some(YES))
    {
        out.push("9a");
    }
    for q in &asked {
        if matches!(q.control(), Control::YesNo { .. }) && answers.get(q.key()).is_none() {
            out.push(q.number());
        }
    }
    out
}

/// The revisions carried, oldest first.
pub const REVISIONS: &[Revision] = &[
    Revision {
        from_year: 0,
        label: "Rev. November 2018",
        form: include_bytes!("../../assets/irs/2023/f1125a.pdf"),
        boxes: Boxes {
            name: "f1_1[0]",
            ein: "f1_2[0]",
            // Each dollar box has a three-character cents box beside it, the
            // even-numbered field. Returns are in whole dollars; those stay empty.
            lines: [
                "f1_3[0]", "f1_5[0]", "f1_7[0]", "f1_9[0]", "f1_11[0]", "f1_13[0]", "f1_15[0]",
                "f1_17[0]",
            ],
            question_9: QUESTION_9_2018,
        },
    },
    Revision {
        from_year: 2024,
        label: "Rev. November 2024",
        form: include_bytes!("../../assets/irs/f1125a.pdf"),
        boxes: Boxes {
            name: "f1_1[0]",
            ein: "f1_2[0]",
            lines: [
                "f1_3[0]", "f1_5[0]", "f1_7[0]", "f1_9[0]", "f1_11[0]", "f1_13[0]", "f1_15[0]",
                "f1_17[0]",
            ],
            question_9: QUESTION_9_2024,
        },
    },
];

/// The revision a tax year is filed on.
pub fn revision_for(year: i32) -> &'static Revision {
    REVISIONS
        .iter()
        .rev()
        .find(|r| year >= r.from_year)
        .expect("the oldest revision starts at year 0")
}

/// Whether the return needs the form: a figure on line 2, or inventory on
/// Schedule L at either end of the year.
pub fn is_required(lines: &Form1065Lines, schedule_l: Option<&ScheduleL>) -> bool {
    let inventory = schedule_l.map(|s| s.get("sl3")).unwrap_or_default();
    lines.get("l2") != 0 || inventory.begin != 0 || inventory.end != 0
}

/// Lines 1, 2, 6, 7 and 8 as the form prints them, in whole dollars.
///
/// `None` is a box left blank because the figure cannot be known, which is not
/// the same as a figure of zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Figures {
    pub beginning_inventory: Option<i64>,
    pub purchases: Option<i64>,
    pub total: Option<i64>,
    pub ending_inventory: Option<i64>,
    pub cost_of_goods_sold: i64,
}

/// Figure the form from line 2 and Schedule L line 3, and say what needs a
/// person.
pub fn figures(lines: &Form1065Lines, schedule_l: Option<&ScheduleL>) -> (Figures, Vec<String>) {
    let cogs = lines.get("l2");
    let mut warnings = Vec::new();

    let Some(sched) = schedule_l else {
        warnings.push(
            "Form 1125-A: Schedule L was not computed, so beginning and ending inventory (lines 1 \
             and 7) and purchases (line 2) are blank. Line 8 carries page 1 line 2."
                .to_string(),
        );
        return (
            Figures {
                beginning_inventory: None,
                purchases: None,
                total: None,
                ending_inventory: None,
                cost_of_goods_sold: cogs,
            },
            warnings,
        );
    };

    let inventory = sched.get("sl3");
    let (begin, end) = (inventory.begin, inventory.end);
    let derived = cogs + end - begin;

    let purchases = if derived < 0 {
        warnings.push(format!(
            "Form 1125-A: inventory fell from {} to {}, which is {} more than the {} of cost of \
             goods sold on page 1 line 2 accounts for, so purchases would be negative. Stock left \
             inventory without its cost reaching cost of goods sold — lines 2 and 6 are blank \
             until the books say where it went.",
            format_dollars(begin),
            format_dollars(end),
            format_dollars(-derived),
            format_dollars(cogs)
        ));
        None
    } else {
        Some(derived)
    };

    if cogs == 0 && end > begin {
        warnings.push(format!(
            "Form 1125-A: inventory rose from {} to {} and page 1 line 2 carries no cost of goods \
             sold. If any of that stock was sold or used up during the year, the entry moving its \
             cost out of inventory has not been posted, and the return overstates income by it.",
            format_dollars(begin),
            format_dollars(end)
        ));
    }

    (
        Figures {
            beginning_inventory: Some(begin),
            purchases,
            total: purchases.map(|p| begin + p),
            ending_inventory: Some(end),
            cost_of_goods_sold: cogs,
        },
        warnings,
    )
}

/// Build Form 1125-A for the tax year, or `None` when the return does not need
/// one.
pub fn build(
    legal_name: &str,
    ein: &str,
    year: i32,
    lines: &Form1065Lines,
    schedule_l: Option<&ScheduleL>,
    answers: &ScheduleB,
) -> Result<(Option<Document>, Vec<String>), FormError> {
    if !is_required(lines, schedule_l) {
        return Ok((None, Vec::new()));
    }
    let revision = revision_for(year);
    let (f, mut warnings) = figures(lines, schedule_l);

    let mut doc = Document::load_mem(revision.form)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    set_text(&mut doc, &map, revision.boxes.name, legal_name)?;
    set_text(&mut doc, &map, revision.boxes.ein, ein)?;

    let [l1, l2, _, _, _, l6, l7, l8] = revision.boxes.lines;
    write_money(&mut doc, &map, l1, f.beginning_inventory)?;
    write_money(&mut doc, &map, l2, f.purchases)?;
    write_money(&mut doc, &map, l6, f.total)?;
    write_money(&mut doc, &map, l7, f.ending_inventory)?;
    write_money(&mut doc, &map, l8, Some(f.cost_of_goods_sold))?;

    warnings.extend(fill_question_9(&mut doc, &map, answers, year)?);

    Ok((Some(doc), warnings))
}

/// Tick and fill question 9 from the year's answers, and say what it still
/// needs or obliges.
fn fill_question_9(
    doc: &mut Document,
    map: &FieldMap,
    answers: &ScheduleB,
    year: i32,
) -> Result<Vec<String>, FormError> {
    let revision = revision_for(year);
    let asked = asked(year);
    let mut warnings = Vec::new();

    // An answer for an item this revision does not print — a small business
    // method answered for a year filed on the 2018 revision.
    for q in QUESTION_9 {
        if answers.get(q.key).is_some() && !asked.iter().any(|a| a.key() == q.key) {
            warnings.push(format!(
                "Form 1125-A: an answer is on file for \"{}\", which the {} revision does not \
                 ask, so it was not written.",
                q.text, revision.label
            ));
        }
    }

    for q in &asked {
        let given = answers.get(q.key());
        match q.control() {
            Control::Check { field } => {
                if given == Some(YES) {
                    set_check(doc, map, field, ON_YES)?;
                }
            }
            Control::YesNo { yes, no } => match given {
                Some(YES) => set_check(doc, map, yes, ON_YES)?,
                Some(NO) => set_check(doc, map, no, ON_NO)?,
                Some(other) => warnings.push(format!(
                    "Form 1125-A: question {} is stored as {other:?}, which is neither yes nor \
                     no, so it was left blank. Answer it again.",
                    q.number()
                )),
                None => {}
            },
            Control::Entry { field, .. } => {
                if let Some(v) = given {
                    match parse_dollars(v) {
                        Some(d) => write_money(doc, map, field, Some(d))?,
                        None => warnings.push(format!(
                            "Form 1125-A: question {} is stored as {v:?}, which is not an amount, \
                             so it was left blank.",
                            q.number()
                        )),
                    }
                }
            }
            // Question 9 has no pick-one items.
            Control::Choice(_) => {}
        }
        for (when, f) in q.meaning.follow_ups {
            let shown = match when {
                FollowUpWhen::Always => true,
                FollowUpWhen::Yes => given == Some(YES),
                FollowUpWhen::No => given == Some(NO),
                FollowUpWhen::Choice(k) => given == Some(*k),
            };
            if let (true, Some(v)) = (shown, answers.get(f.key)) {
                set_text(doc, map, f.field, v)?;
            }
        }
    }

    let missing = unanswered(answers, year);
    if !missing.is_empty() {
        warnings.push(format!(
            "Form 1125-A: question 9 is incomplete — {} unanswered. How closing inventory is \
             valued, whether section 263A applies and whether the method changed are not in the \
             books; answer them with the Schedule B answers for {year}.",
            missing
                .iter()
                .map(|n| if *n == "9a" {
                    "9a (no valuation method checked)".to_string()
                } else {
                    n.to_string()
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if answers.get("a9a_other") == Some(YES) {
        warnings.push(if answers.get("a9a_other_method").is_none() {
            "Form 1125-A: question 9a(iii), another valuation method, is checked with no method \
             named. Name it, and attach the explanation the form asks for."
                .to_string()
        } else {
            "Form 1125-A: question 9a(iii) is checked, so attach an explanation of the valuation \
             method."
                .to_string()
        });
    }
    if answers.get("a9c") == Some(YES) {
        warnings.push(
            "Form 1125-A: question 9c is checked — LIFO was adopted this year — so attach Form \
             970."
                .to_string(),
        );
    }
    if answers.get("a9e") == Some(YES) {
        warnings.push(
            "Form 1125-A: question 9e is Yes, so section 263A costs belong on line 4, but lines 3 \
             to 5 are blank — any labor, section 263A or other costs in the books are inside line \
             2, which is derived as line 8 plus line 7 less line 1. Figure them and enter them by \
             hand."
                .to_string(),
        );
    }
    if answers.get("a9f") == Some(YES) {
        warnings.push(
            "Form 1125-A: question 9f is Yes, so attach an explanation of the change in \
             determining quantities, cost or valuations."
                .to_string(),
        );
    }
    Ok(warnings)
}

/// A dollar amount as typed — `$1,234` or `1234.50` — in whole dollars.
fn parse_dollars(s: &str) -> Option<i64> {
    let cleaned: String = s.chars().filter(|c| !matches!(c, '$' | ',' | ' ')).collect();
    cleaned.parse::<f64>().ok().map(|d| d.round() as i64)
}

/// Write a figure, leaving the box empty for zero as the rest of the return does.
fn write_money(
    doc: &mut Document,
    map: &FieldMap,
    field: &str,
    dollars: Option<i64>,
) -> Result<(), FormError> {
    match dollars {
        Some(d) if d != 0 => set_text(doc, map, field, &format_dollars(d)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::acroform::{get_value, on_states};
    use lopdf::Object;

    fn inventory(begin: i64, end: i64) -> ScheduleL {
        let mut s = ScheduleL::default();
        s.set_for_test("sl3", begin, end);
        s
    }

    fn cogs(dollars: i64) -> Form1065Lines {
        let mut l = Form1065Lines::default();
        l.set_for_test("l2", dollars);
        l
    }

    #[test]
    fn each_tax_year_gets_the_revision_in_force_for_it() {
        assert_eq!(revision_for(2023).label, "Rev. November 2018");
        assert_eq!(revision_for(2019).label, "Rev. November 2018");
        assert_eq!(revision_for(2024).label, "Rev. November 2024");
        assert_eq!(revision_for(2026).label, "Rev. November 2024");
    }

    /// Every box a revision names exists in that revision's own PDF.
    #[test]
    fn every_revision_names_only_boxes_its_own_pdf_has() {
        for r in REVISIONS {
            let mut doc = Document::load_mem(r.form).expect("the blank loads");
            strip_xfa(&mut doc);
            let map = field_map(&doc);
            for name in [r.boxes.name, r.boxes.ein]
                .iter()
                .chain(r.boxes.lines.iter())
            {
                assert!(map.find(name).is_some(), "{} has no field {name}", r.label);
            }
        }
    }

    /// Lines 1 to 8 run down the right-hand column in order, on every revision.
    ///
    /// The check a name check cannot make: every name still resolving is exactly
    /// what a renumbered revision looks like.
    #[test]
    fn the_line_boxes_run_down_the_page_in_line_order() {
        for r in REVISIONS {
            let mut doc = Document::load_mem(r.form).expect("the blank loads");
            strip_xfa(&mut doc);
            let map = field_map(&doc);
            let rects: Vec<Vec<f64>> = r
                .boxes
                .lines
                .iter()
                .map(|name| {
                    let id = map.find(name).expect("named box exists");
                    doc.get_dictionary(id)
                        .and_then(|d| d.get(b"Rect"))
                        .and_then(Object::as_array)
                        .expect("a widget has a rectangle")
                        .iter()
                        .map(|o| {
                            o.as_float()
                                .map(f64::from)
                                .or_else(|_| o.as_i64().map(|i| i as f64))
                                .unwrap()
                        })
                        .collect()
                })
                .collect();
            for (i, pair) in rects.windows(2).enumerate() {
                assert!(
                    pair[1][1] < pair[0][1],
                    "{}: line {} sits above line {}",
                    r.label,
                    i + 2,
                    i + 1
                );
            }
            for (i, rect) in rects.iter().enumerate() {
                assert!(
                    rect[0] > 400.0,
                    "{}: line {} is not in the amount column",
                    r.label,
                    i + 1
                );
            }
        }
    }

    #[test]
    fn nothing_on_line_2_and_no_inventory_needs_no_form() {
        assert!(!is_required(
            &Form1065Lines::default(),
            Some(&inventory(0, 0))
        ));
        assert!(!is_required(&Form1065Lines::default(), None));
        let (doc, _) = build(
            "P",
            "12-3456789",
            2025,
            &Form1065Lines::default(),
            None,
            &ScheduleB::default(),
        )
        .unwrap();
        assert!(doc.is_none());
    }

    /// Purchases are what make the form foot to the page it supports.
    #[test]
    fn purchases_are_derived_so_line_8_is_line_2() {
        let (f, _) = figures(&cogs(3_000), Some(&inventory(1_000, 1_500)));
        assert_eq!(f.beginning_inventory, Some(1_000));
        assert_eq!(f.purchases, Some(3_500));
        assert_eq!(f.total, Some(4_500));
        assert_eq!(f.ending_inventory, Some(1_500));
        assert_eq!(f.cost_of_goods_sold, 3_000);
        assert_eq!(
            f.total.unwrap() - f.ending_inventory.unwrap(),
            f.cost_of_goods_sold
        );
    }

    /// Inventory held with nothing sold still owes the form — and a warning,
    /// because stock that never leaves inventory is usually an entry never posted.
    #[test]
    fn inventory_that_only_grows_is_filed_and_questioned() {
        assert!(is_required(
            &Form1065Lines::default(),
            Some(&inventory(759, 2_375))
        ));
        let (f, warnings) = figures(&Form1065Lines::default(), Some(&inventory(759, 2_375)));
        assert_eq!(f.purchases, Some(1_616));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("rose from 759 to 2,375")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_negative_purchases_figure_is_left_blank_and_said() {
        let (f, warnings) = figures(&cogs(100), Some(&inventory(1_000, 200)));
        assert_eq!(f.purchases, None);
        assert_eq!(f.total, None);
        assert_eq!(f.cost_of_goods_sold, 100);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("purchases would be negative")),
            "{warnings:?}"
        );
    }

    /// Nothing in question 9 is ticked for the filer: unanswered, it is left
    /// blank and every missing item is named.
    #[test]
    fn an_unanswered_question_9_is_left_blank_and_named() {
        let (doc, warnings) = build(
            "P",
            "12-3456789",
            2025,
            &cogs(10),
            Some(&inventory(0, 0)),
            &ScheduleB::default(),
        )
        .unwrap();
        let doc = doc.expect("a form");
        let map = field_map(&doc);
        assert_ne!(get_value(&doc, &map, "c1_1[0]").as_deref(), Some("/Yes"));
        let w = warnings
            .iter()
            .find(|w| w.contains("question 9 is incomplete"))
            .expect("a warning naming what is missing");
        assert!(w.contains("9a (no valuation method checked), 9e, 9f"), "{w}");
    }

    /// Every question 9 box a revision names is in its own PDF, ticks with the
    /// state it is given, and sits where the item is printed: the 9a methods in
    /// the left margin, everything else in the right-hand column.
    #[test]
    fn every_question_9_box_is_on_its_own_revision() {
        for r in REVISIONS {
            let mut doc = Document::load_mem(r.form).expect("the blank loads");
            strip_xfa(&mut doc);
            let map = field_map(&doc);
            let x = |name: &str| -> f64 {
                let id = map.find(name).unwrap_or_else(|| panic!("{} has no {name}", r.label));
                let rect = doc.get_dictionary(id).unwrap().get(b"Rect").unwrap();
                let first = &rect.as_array().unwrap()[0];
                first
                    .as_float()
                    .map(f64::from)
                    .or_else(|_| first.as_i64().map(|i| i as f64))
                    .unwrap()
            };
            for b in r.boxes.question_9 {
                assert!(
                    QUESTION_9.iter().any(|q| q.key == b.key),
                    "{} has no meaning",
                    b.key
                );
                match b.control {
                    Control::Check { field } => {
                        assert_eq!(on_states(&doc, &map, field), vec![ON_YES], "{}", b.number);
                        assert_eq!(
                            x(field) < 100.0,
                            b.key.starts_with("a9a_"),
                            "{} {} is in the wrong column",
                            r.label,
                            b.number
                        );
                    }
                    Control::YesNo { yes, no } => {
                        assert_eq!(on_states(&doc, &map, yes), vec![ON_YES], "{}", b.number);
                        assert_eq!(on_states(&doc, &map, no), vec![ON_NO], "{}", b.number);
                        assert!(x(yes) < x(no), "{} {}: Yes is left of No", r.label, b.number);
                    }
                    Control::Entry { field, .. } => assert!(x(field) > 400.0, "{}", b.number),
                    Control::Choice(_) => panic!("question 9 has no pick-one items"),
                }
            }
            assert!(map.find(OTHER_METHOD.field).is_some());
        }
    }

    /// Answers reach their boxes on each revision, and the ones that oblige an
    /// attachment say so.
    #[test]
    fn answers_tick_question_9_on_each_revision() {
        let mut answers = ScheduleB::default();
        answers.set("a9a_cost", YES);
        answers.set("a9e", NO);
        answers.set("a9f", NO);

        for (year, e_no, f_no) in [(2023, "c1_6[1]", "c1_7[1]"), (2025, "c1_9[1]", "c1_10[1]")] {
            let (doc, warnings) = build(
                "P",
                "12-3456789",
                year,
                &cogs(10),
                Some(&inventory(0, 0)),
                &answers,
            )
            .unwrap();
            let doc = doc.expect("a form");
            let map = field_map(&doc);
            assert_eq!(get_value(&doc, &map, "c1_1[0]").as_deref(), Some("/Yes"), "{year}");
            assert_eq!(get_value(&doc, &map, e_no).as_deref(), Some("/No"), "{year}");
            assert_eq!(get_value(&doc, &map, f_no).as_deref(), Some("/No"), "{year}");
            assert_ne!(get_value(&doc, &map, "c1_2[0]").as_deref(), Some("/Yes"), "{year}");
            assert!(
                !warnings.iter().any(|w| w.contains("question 9")),
                "{year}: {warnings:?}"
            );
        }

        answers.set("a9a_other", YES);
        answers.set("a9a_other_method", "Specific identification");
        answers.set("a9c", YES);
        answers.set("a9d", "$1,234");
        answers.set("a9a_nims", YES);
        let (doc, warnings) = build(
            "P",
            "12-3456789",
            2023,
            &cogs(10),
            Some(&inventory(0, 0)),
            &answers,
        )
        .unwrap();
        let doc = doc.expect("a form");
        let map = field_map(&doc);
        assert_eq!(
            get_value(&doc, &map, "f1_19[0]").as_deref(),
            Some("Specific identification")
        );
        assert_eq!(get_value(&doc, &map, "f1_20[0]").as_deref(), Some("1,234"));
        for said in ["attach Form 970", "explanation of the valuation", "does not ask"] {
            assert!(warnings.iter().any(|w| w.contains(said)), "{said}: {warnings:?}");
        }
    }

    #[test]
    fn the_2023_form_carries_the_figures_in_their_boxes() {
        let (doc, _) = build(
            "Bunny Ears Art House LLC",
            "92-3497029",
            2023,
            &cogs(300),
            Some(&inventory(759, 1_200)),
            &ScheduleB::default(),
        )
        .unwrap();
        let doc = doc.expect("a form");
        let map = field_map(&doc);
        let [l1, l2, _, _, _, l6, l7, l8] = revision_for(2023).boxes.lines;
        assert_eq!(
            get_value(&doc, &map, "f1_1[0]").as_deref(),
            Some("Bunny Ears Art House LLC")
        );
        assert_eq!(
            get_value(&doc, &map, "f1_2[0]").as_deref(),
            Some("92-3497029")
        );
        assert_eq!(get_value(&doc, &map, l1).as_deref(), Some("759"));
        assert_eq!(get_value(&doc, &map, l2).as_deref(), Some("741"));
        assert_eq!(get_value(&doc, &map, l6).as_deref(), Some("1,500"));
        assert_eq!(get_value(&doc, &map, l7).as_deref(), Some("1,200"));
        assert_eq!(get_value(&doc, &map, l8).as_deref(), Some("300"));
    }
}
