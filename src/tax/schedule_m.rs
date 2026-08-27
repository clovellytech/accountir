//! Schedules M-1 and M-2: reconciling the books to the return, and the partners'
//! capital accounts.
//!
//! # Why these are worth completing even when they are not required
//!
//! Schedule B question 4 excuses a small partnership from L, M-1 and M-2. That
//! excuses the *filing*, not the arithmetic — and the arithmetic is the only
//! check the return has on itself. M-1 says the book profit and the taxable
//! figure differ by an amount somebody can name; M-2 says the capital the balance
//! sheet claims at year end is the capital you started with, plus income, less
//! what was drawn. A return that fails either is wrong in a way page one cannot
//! show, because page one foots regardless.
//!
//! So they are completed by default, and the exemption becomes a note on the page
//! rather than a reason to leave it empty. See [`crate::tax::ReturnOptions`].
//!
//! # What M-1 can be derived from and what it cannot
//!
//! M-1 reconciles book income to the Analysis of Net Income:
//!
//! ```text
//!   line 1  net income (loss) per books
//! + line 3  guaranteed payments
//! + lines 2, 4   income/expense the books and the return disagree about
//! - lines 6, 7   ditto, the other direction
//! = line 9  Analysis of Net Income, line 1
//! ```
//!
//! Lines 1, 3 and 9 all come from figures already computed. Lines 2, 4, 6 and 7
//! are *book-to-tax differences* — a meal half-disallowed, depreciation on two
//! bases, tax-exempt interest — and nothing in a general ledger distinguishes
//! them from any other entry, because the difference lives in the tax code and
//! not in the books.
//!
//! Rather than plug the gap into whichever line makes it foot, [`reconcile`]
//! computes the residual and names it. A residual of nothing means the books and
//! the return agree and the schedule is complete. A residual of something is a
//! real quantity somebody has to itemize, and it is better seen than hidden in
//! line 4.

use super::acroform::{set_text, FieldMap, FormError};
use super::lines::{format_dollars, Form1065Lines};
use super::schedule_l::ScheduleL;
use lopdf::Document;

/// Schedule M-1.
mod m1 {
    pub const L1_BOOK_INCOME: &str = "f6_126[0]";
    pub const L2_ITEMIZE: &str = "f6_127[0]";
    pub const L2_AMOUNT: &str = "f6_128[0]";
    pub const L3_GUARANTEED: &str = "f6_129[0]";
    // Lines 4 and 7 are deliberately absent.
    //
    // The additions split across lines 2 and 4, and the subtractions across 6 and
    // 7, by whether the difference is an income item or an expense one. Nothing
    // in a general ledger distinguishes those — the difference lives in the tax
    // code — so the residual goes to whichever side it belongs on and to the one
    // free-text row on that side. Lines 4 and 7 are also pre-labelled on the
    // printed form ("Depreciation", "Travel and entertainment"), so an
    // unallocated figure written there would sit under a caption naming
    // something it may not be.
    pub const L5_TOTAL: &str = "f6_133[0]";
    pub const L6_ITEMIZE: &str = "f6_134[0]";
    pub const L6_AMOUNT: &str = "f6_136[0]";
    pub const L8_TOTAL: &str = "f6_140[0]";
    pub const L9_INCOME: &str = "f6_141[0]";
}

/// Schedule M-2.
mod m2 {
    pub const L1_BEGIN: &str = "f6_142[0]";
    pub const L3_NET_INCOME: &str = "f6_145[0]";
    pub const L4_ITEMIZE: &str = "f6_146[0]";
    pub const L4_AMOUNT: &str = "f6_147[0]";
    pub const L5_TOTAL: &str = "f6_148[0]";
    pub const L6A_CASH: &str = "f6_149[0]";
    pub const L6B_PROPERTY: &str = "f6_150[0]";
    pub const L7_ITEMIZE: &str = "f6_151[0]";
    pub const L7_AMOUNT: &str = "f6_153[0]";
    pub const L8_TOTAL: &str = "f6_154[0]";
    pub const L9_END: &str = "f6_155[0]";
}

/// The two schedules, in whole dollars.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScheduleM {
    // --- M-1 ---
    /// Line 1. Net income (loss) per books.
    pub book_income: i64,
    /// Line 3. Guaranteed payments — Schedule K line 4c.
    pub guaranteed_payments: i64,
    /// Line 9. The Analysis of Net Income figure this has to reconcile to.
    pub analysis: i64,
    /// What lines 2, 4, 6 and 7 have to account for between them.
    ///
    /// Positive means the return reports more than the books plus guaranteed
    /// payments do, so it belongs on line 2 or 4; negative, on line 6 or 7.
    pub book_tax_difference: i64,

    // --- M-2 ---
    /// Line 1. Partners' capital at the start of the year.
    pub capital_begin: i64,
    /// Line 6a. Cash distributions — Schedule K line 19a.
    pub distributions_cash: i64,
    /// Line 6b. Property distributions — Schedule K line 19b.
    pub distributions_property: i64,
    /// What the balance sheet says capital was at year end.
    ///
    /// M-2 line 9 is computed from lines 1 through 8; this is the independent
    /// figure it has to match, and the difference between them is the check.
    pub capital_end_per_books: i64,
    /// Whether a balance sheet was available at all. Without one, M-2 has no
    /// opening balance to start from and only its middle is computable.
    pub has_balance_sheet: bool,
}

impl ScheduleM {
    /// M-1 line 5. Add lines 1 through 4.
    ///
    /// The difference is placed on the *additions* side when it is positive and
    /// on the subtractions side when it is negative, so lines 5 and 8 foot to
    /// line 9 exactly. Which of lines 2 and 4 it belongs on is a question about
    /// the nature of the difference that the books cannot answer, so the figure
    /// is written to the itemize row with a label saying it is unallocated.
    pub fn m1_line_5(&self) -> i64 {
        self.book_income + self.guaranteed_payments + self.book_tax_difference.max(0)
    }

    /// M-1 line 8. Add lines 6 and 7.
    pub fn m1_line_8(&self) -> i64 {
        (-self.book_tax_difference).max(0)
    }

    /// M-2 line 5. Add lines 1 through 4.
    pub fn m2_line_5(&self) -> i64 {
        self.capital_begin + self.analysis + self.m2_other_increase()
    }

    /// M-2 line 8. Add lines 6 and 7.
    pub fn m2_line_8(&self) -> i64 {
        self.distributions_cash + self.distributions_property + self.m2_other_decrease()
    }

    /// M-2 line 9. Balance at end of year.
    pub fn m2_line_9(&self) -> i64 {
        self.m2_line_5() - self.m2_line_8()
    }

    /// What capital moved by that income and distributions do not explain.
    ///
    /// Contributions, draws recorded outside the distribution accounts, prior
    /// period adjustments. Split into an increase and a decrease so line 9 lands
    /// on the balance sheet's own year-end figure, which is the number a reader
    /// checks against Schedule L.
    fn unexplained(&self) -> i64 {
        if !self.has_balance_sheet {
            return 0;
        }
        let without_adjustment = self.capital_begin + self.analysis
            - self.distributions_cash
            - self.distributions_property;
        self.capital_end_per_books - without_adjustment
    }

    fn m2_other_increase(&self) -> i64 {
        self.unexplained().max(0)
    }

    fn m2_other_decrease(&self) -> i64 {
        (-self.unexplained()).max(0)
    }

    /// Whether M-1 reconciles with nothing left over.
    pub fn m1_reconciles(&self) -> bool {
        self.book_tax_difference == 0
    }

    /// Whether M-2's own arithmetic lands on the balance sheet's year-end
    /// capital.
    pub fn m2_ties_to_the_balance_sheet(&self) -> bool {
        !self.has_balance_sheet || self.m2_line_9() == self.capital_end_per_books
    }
}

/// Work out both schedules from what has already been computed.
///
/// Takes the figures rather than the ledger so the arithmetic is testable
/// without a set of books, and so it cannot disagree with the pages it
/// reconciles — every input here is the same value those pages carry.
pub fn reconcile(
    book_income_cents: i64,
    lines: &Form1065Lines,
    schedule_l: Option<&ScheduleL>,
) -> ScheduleM {
    let book_income = super::lines::cents_to_dollars(book_income_cents);
    let guaranteed_payments = lines.k_line_4c();
    let analysis = lines.k_analysis();

    let (capital_begin, capital_end_per_books, has_balance_sheet) = match schedule_l {
        Some(l) if !l.is_empty() => {
            let p = l.get("sl21");
            (p.begin, p.end, true)
        }
        _ => (0, 0, false),
    };

    ScheduleM {
        book_income,
        guaranteed_payments,
        analysis,
        book_tax_difference: analysis - book_income - guaranteed_payments,
        capital_begin,
        distributions_cash: lines.get("k19a"),
        distributions_property: lines.get("k19b"),
        capital_end_per_books,
        has_balance_sheet,
    }
}

/// The label written beside an amount nobody has itemized.
///
/// Says what the figure is rather than inventing a category for it. A reader who
/// sees "unallocated" knows there is work left; one who sees a plausible-looking
/// "Depreciation" does not.
const UNALLOCATED: &str = "Book-to-tax difference — itemize before filing";
const UNEXPLAINED_CAPITAL: &str = "Not explained by income or distributions — itemize";

/// Write both schedules onto the form.
pub fn fill(
    doc: &mut Document,
    map: &FieldMap,
    m: &ScheduleM,
    required: bool,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();
    let money = |d: i64| format_dollars(d);

    // --- M-1 ---
    set_text(doc, map, m1::L1_BOOK_INCOME, &money(m.book_income))?;
    if m.guaranteed_payments != 0 {
        set_text(doc, map, m1::L3_GUARANTEED, &money(m.guaranteed_payments))?;
    }
    if m.book_tax_difference > 0 {
        set_text(doc, map, m1::L2_ITEMIZE, UNALLOCATED)?;
        set_text(doc, map, m1::L2_AMOUNT, &money(m.book_tax_difference))?;
    } else if m.book_tax_difference < 0 {
        set_text(doc, map, m1::L6_ITEMIZE, UNALLOCATED)?;
        set_text(doc, map, m1::L6_AMOUNT, &money(-m.book_tax_difference))?;
    }
    set_text(doc, map, m1::L5_TOTAL, &money(m.m1_line_5()))?;
    set_text(doc, map, m1::L8_TOTAL, &money(m.m1_line_8()))?;
    // Line 9 is written from the Analysis figure, not from 5 - 8. They are equal
    // by construction, and writing the one the rest of the return already carries
    // means the two pages cannot disagree even if this arithmetic is wrong.
    set_text(doc, map, m1::L9_INCOME, &money(m.analysis))?;

    if !m.m1_reconciles() {
        warnings.push(format!(
            "Schedule M-1: book income ({}) plus guaranteed payments ({}) differs from the \
             Analysis of Net Income ({}) by {}. That difference is real — a disallowed expense, \
             depreciation on two bases, tax-exempt income — and the form wants it itemized on \
             lines 2, 4, 6 and 7. It has been placed on one unallocated row so the page foots; \
             break it out before filing.",
            money(m.book_income),
            money(m.guaranteed_payments),
            money(m.analysis),
            money(m.book_tax_difference.abs()),
        ));
    }

    // --- M-2 ---
    if m.has_balance_sheet {
        set_text(doc, map, m2::L1_BEGIN, &money(m.capital_begin))?;
    }
    set_text(doc, map, m2::L3_NET_INCOME, &money(m.analysis))?;
    if m.distributions_cash != 0 {
        set_text(doc, map, m2::L6A_CASH, &money(m.distributions_cash))?;
    }
    if m.distributions_property != 0 {
        set_text(doc, map, m2::L6B_PROPERTY, &money(m.distributions_property))?;
    }
    let increase = m.m2_other_increase();
    let decrease = m.m2_other_decrease();
    if increase != 0 {
        set_text(doc, map, m2::L4_ITEMIZE, UNEXPLAINED_CAPITAL)?;
        set_text(doc, map, m2::L4_AMOUNT, &money(increase))?;
    }
    if decrease != 0 {
        set_text(doc, map, m2::L7_ITEMIZE, UNEXPLAINED_CAPITAL)?;
        set_text(doc, map, m2::L7_AMOUNT, &money(decrease))?;
    }
    set_text(doc, map, m2::L5_TOTAL, &money(m.m2_line_5()))?;
    set_text(doc, map, m2::L8_TOTAL, &money(m.m2_line_8()))?;
    set_text(doc, map, m2::L9_END, &money(m.m2_line_9()))?;

    if !m.has_balance_sheet {
        warnings.push(
            "Schedule M-2 has no opening capital: nothing is mapped to Schedule L line 21, so \
             there is no balance sheet to take it from. Lines 1 and 9 are the ones a reader checks \
             against Schedule L, and both are guesswork without it."
                .to_string(),
        );
    } else if increase != 0 || decrease != 0 {
        warnings.push(format!(
            "Schedule M-2: capital moved by {} that income and distributions do not explain — \
             contributions, draws posted outside the distribution accounts, a prior-period \
             adjustment. Placed on one unallocated row so line 9 lands on the balance sheet's \
             year-end capital; itemize it before filing.",
            money(increase.max(decrease)),
        ));
    }

    if !required {
        warnings.push(
            "Schedule B question 4 is Yes, so Schedules L, M-1 and M-2 were not required. They \
             have been completed from the books anyway — the arithmetic is the only check the \
             return has on itself. Turn this off under \"Generate the return\" to leave them blank."
                .to_string(),
        );
    }

    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::acroform::{field_map, get_value, strip_xfa};

    const F1065: &[u8] = include_bytes!("../../assets/irs/f1065.pdf");

    fn form() -> (Document, FieldMap) {
        let mut doc = Document::load_mem(F1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        (doc, map)
    }

    /// A partnership with no book-tax differences: book income plus guaranteed
    /// payments is the Analysis figure, and M-1 has nothing to itemize.
    #[test]
    fn a_clean_reconciliation_leaves_nothing_to_itemize() {
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 100_000);
        lines.set_for_test("l10", 30_000); // guaranteed payments, deducted on page 1
        lines.set_for_test("k4a", 30_000); // and reported on Schedule K

        // Books: 100,000 revenue less the 30,000 of guaranteed payments.
        let m = reconcile(70_000_00, &lines, None);
        assert_eq!(m.book_income, 70_000);
        assert_eq!(m.guaranteed_payments, 30_000);
        assert_eq!(m.analysis, lines.k_analysis());
        assert!(m.m1_reconciles(), "difference was {}", m.book_tax_difference);
        assert_eq!(m.m1_line_5(), m.analysis);
        assert_eq!(m.m1_line_8(), 0);
    }

    /// A real difference is named rather than plugged into a plausible line.
    #[test]
    fn a_book_tax_difference_is_reported_and_not_disguised() {
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 100_000);
        // Books say 60,000 but the return computes 100,000 — a 40,000 difference,
        // say half a year of meals disallowed.
        let m = reconcile(60_000_00, &lines, None);
        assert_eq!(m.book_tax_difference, 40_000);

        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, true).unwrap();

        assert_eq!(
            get_value(&doc, &map, m1::L2_AMOUNT).as_deref(),
            Some("40,000")
        );
        assert!(
            get_value(&doc, &map, m1::L2_ITEMIZE)
                .unwrap_or_default()
                .contains("itemize"),
            "the row has to say it is unfinished"
        );
        assert!(
            warnings.iter().any(|w| w.contains("break it out")),
            "{warnings:?}"
        );
    }

    /// The page has to foot as printed, in both directions.
    #[test]
    fn m1_foots_whichever_way_the_difference_runs() {
        for (book_cents, expect_side) in [(60_000_00i64, "additions"), (140_000_00, "subtractions")]
        {
            let mut lines = Form1065Lines::default();
            lines.set_for_test("l1a", 100_000);
            let m = reconcile(book_cents, &lines, None);
            assert_eq!(
                m.m1_line_5() - m.m1_line_8(),
                m.analysis,
                "line 5 less line 8 must equal line 9 ({expect_side})"
            );
        }
    }

    /// M-2's whole value: line 9 has to land on the balance sheet's own year-end
    /// capital, so the two pages agree.
    #[test]
    fn m2_line_9_lands_on_the_balance_sheets_year_end_capital() {
        let mut l = ScheduleL::default();
        l.set_for_test("sl21", 100_000, 118_000);

        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 30_000);
        lines.set_for_test("k19a", 12_000); // cash distributions

        let m = reconcile(30_000_00, &lines, Some(&l));
        assert_eq!(m.capital_begin, 100_000);
        assert_eq!(m.distributions_cash, 12_000);
        // 100,000 + 30,000 - 12,000 = 118,000, which is what the books say.
        assert_eq!(m.m2_line_9(), 118_000);
        assert!(m.m2_ties_to_the_balance_sheet());
    }

    /// Capital that moved for a reason the books do not record — a contribution —
    /// still has to land on the right year-end figure, and be called out.
    #[test]
    fn an_unexplained_capital_movement_is_placed_and_reported() {
        let mut l = ScheduleL::default();
        // 25,000 more than income and distributions explain.
        l.set_for_test("sl21", 100_000, 143_000);

        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 30_000);
        lines.set_for_test("k19a", 12_000);

        let m = reconcile(30_000_00, &lines, Some(&l));
        assert_eq!(m.m2_line_9(), 143_000, "line 9 must still tie");
        assert!(m.m2_ties_to_the_balance_sheet());

        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(get_value(&doc, &map, m2::L4_AMOUNT).as_deref(), Some("25,000"));
        assert!(
            warnings.iter().any(|w| w.contains("do not explain")),
            "{warnings:?}"
        );
    }

    /// A withdrawal beyond distributions runs the other way.
    #[test]
    fn capital_that_fell_further_than_distributions_explain_goes_to_line_7() {
        let mut l = ScheduleL::default();
        l.set_for_test("sl21", 100_000, 100_000);

        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 30_000);
        let m = reconcile(30_000_00, &lines, Some(&l));

        let (mut doc, map) = form();
        fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(get_value(&doc, &map, m2::L7_AMOUNT).as_deref(), Some("30,000"));
        assert_eq!(get_value(&doc, &map, m2::L9_END).as_deref(), Some("100,000"));
    }

    /// Without a balance sheet, M-2's opening balance is not knowable and the
    /// page must say so rather than start from zero as though that were a fact.
    #[test]
    fn no_balance_sheet_means_m2_says_it_cannot_open() {
        let m = reconcile(30_000_00, &Form1065Lines::default(), None);
        assert!(!m.has_balance_sheet);

        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(get_value(&doc, &map, m2::L1_BEGIN), None);
        assert!(
            warnings.iter().any(|w| w.contains("no opening capital")),
            "{warnings:?}"
        );
    }

    /// Completed under the exemption, with a note — not left blank.
    #[test]
    fn the_exemption_completes_the_pages_and_says_they_were_optional() {
        let m = reconcile(30_000_00, &Form1065Lines::default(), None);
        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, false).unwrap();

        assert!(
            get_value(&doc, &map, m1::L1_BOOK_INCOME).is_some(),
            "the page is completed even when not required"
        );
        assert!(
            warnings.iter().any(|w| w.contains("not required")),
            "{warnings:?}"
        );
    }

    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_form() {
        let (doc, map) = form();
        for name in [
            m1::L1_BOOK_INCOME,
            m1::L2_ITEMIZE,
            m1::L2_AMOUNT,
            m1::L3_GUARANTEED,
            m1::L5_TOTAL,
            m1::L6_ITEMIZE,
            m1::L6_AMOUNT,
            m1::L8_TOTAL,
            m1::L9_INCOME,
            m2::L1_BEGIN,
            m2::L3_NET_INCOME,
            m2::L4_ITEMIZE,
            m2::L4_AMOUNT,
            m2::L5_TOTAL,
            m2::L6A_CASH,
            m2::L6B_PROPERTY,
            m2::L7_ITEMIZE,
            m2::L7_AMOUNT,
            m2::L8_TOTAL,
            m2::L9_END,
        ] {
            assert!(map.find(name).is_some(), "f1065.pdf has no field {name}");
        }
        let _ = doc;
    }
}
