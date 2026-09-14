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
//! them apart from it. Question 9 is methods and elections: how closing inventory
//! is valued, LIFO, section 263A, a change in method. None of that is in the
//! books, and a ticked box is a statement about the business, so the boxes stay
//! empty and the warnings say so.
//!
//! # Revisions
//!
//! Two are carried. The November 2018 revision serves tax years through 2023 and
//! gives every amount a separate cents box. The November 2024 revision serves
//! 2024 onward: it drops the cents boxes and adds three valuation methods and a
//! LIFO reserve line to question 9, which renumbers every check box after 9a.
//! Each revision names only its own boxes.

use super::acroform::{field_map, set_text, strip_xfa, FieldMap, FormError};
use super::lines::{format_dollars, Form1065Lines};
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

    warnings.push(
        "Form 1125-A: lines 3 to 5 are blank — any labor, section 263A or other costs in the \
         books are inside line 2, which is derived as line 8 plus line 7 less line 1 — and \
         question 9 is unanswered. How closing inventory is valued (9a), whether section 263A \
         applies (9e) and whether the method changed (9f) are not in the books; answer them \
         before filing."
            .to_string(),
    );

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
) -> Result<(Option<Document>, Vec<String>), FormError> {
    if !is_required(lines, schedule_l) {
        return Ok((None, Vec::new()));
    }
    let revision = revision_for(year);
    let (f, warnings) = figures(lines, schedule_l);

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

    Ok((Some(doc), warnings))
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
    use crate::tax::acroform::get_value;
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
        let (doc, _) = build("P", "12-3456789", 2025, &Form1065Lines::default(), None).unwrap();
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

    #[test]
    fn question_9_is_never_answered_for_the_filer() {
        let (_, warnings) = figures(&cogs(10), Some(&inventory(0, 0)));
        assert!(warnings
            .iter()
            .any(|w| w.contains("question 9 is unanswered")));
    }

    #[test]
    fn the_2023_form_carries_the_figures_in_their_boxes() {
        let (doc, _) = build(
            "Bunny Ears Art House LLC",
            "92-3497029",
            2023,
            &cogs(300),
            Some(&inventory(759, 1_200)),
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
