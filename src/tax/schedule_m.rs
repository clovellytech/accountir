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
//! bases, tax-exempt interest — and for most of them nothing in a general ledger
//! distinguishes them from any other entry, because the difference lives in the
//! tax code and not in the books.
//!
//! Rather than plug the gap into whichever line makes it foot, [`reconcile`]
//! computes the residual and names it. A residual of nothing means the books and
//! the return agree and the schedule is complete. A residual of something is a
//! real quantity somebody has to itemize, and it is better seen than hidden in
//! line 4.
//!
//! # Which of those differences this program can now name
//!
//! One of them: the disallowed part of a limited deduction. The books hold what
//! a meal cost, the mapping holds what share of it the law allows, and
//! [`crate::tax::lines`] computes the difference and reports it on Schedule K
//! line 18c. That is not a quantity nobody can see — it is a quantity this
//! program derived, and it belongs on M-1 line 4 (an expense on the books that
//! Schedule K does not deduct) and among M-2's other decreases (capital the
//! partners spent and get no deduction for). So it is broken out of the residual
//! and labelled, and the residual carries only what is left.
//!
//! The rest of the list still cannot be derived here and is not guessed at.
//! Depreciation on two bases needs a tax-basis register this program has for the
//! assets somebody entered and not for the ledger as a whole; tax-exempt
//! interest and the §263A adjustments are facts about accounts that nothing in
//! the chart of accounts records. Those stay inside the residual, and the
//! residual is still reported as unexplained rather than captioned with a
//! plausible name.
//!
//! # Why naming a component cannot move a total
//!
//! It must not, and [`ScheduleM`]'s totals are written so it cannot. M-2 line 9
//! has to land on Schedule L's own year-end capital, which is book basis, while
//! line 3 is the tax Analysis figure — the residual is what absorbs that
//! mismatch, and it absorbs it whether or not part of it has a name. So the
//! named component is carved *out of* the residual and never added beside it:
//! the two always sum to what the residual alone used to be. The same holds on
//! M-1, where line 5 less line 8 is line 9 by construction.

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
    /// Line 4's own money column — "Expenses recorded on books this year not
    /// included on Schedule K, lines 1 through 13d, and 21".
    ///
    /// Only the total. Line 4's two pre-printed sub-rows, 4a "Depreciation" and
    /// 4b "Travel and entertainment", are deliberately left alone: nondeductible
    /// expenses are usually the disallowed half of a meal and would sit happily
    /// under 4b, but line 18c also collects fines and political contributions,
    /// and a figure under a caption naming something it may not be is worse than
    /// a figure with its itemization attached behind the return — which is where
    /// this one's is. See [`crate::tax::statement`].
    ///
    /// # Why this one looks like 4b's box and is not
    ///
    /// `f6_132` follows `f6_130` (4a) and `f6_131` (4b) in the numbering and sits
    /// on the same baseline as 4b, so by ordinal or by row it reads as "the
    /// amount for 4b" — and the next person to grep the form will think this
    /// constant is wrong. It is not. The two sub-row boxes sit inset, ending at
    /// x=223; this one is at x=230–302, the money column lines 1, 2, 3 and 5 all
    /// write into. It is line 4's column total. Checked against the widget
    /// rectangles in the vendored form, and
    /// `every_field_this_module_names_exists_in_the_vendored_form` keeps the name
    /// honest across revisions.
    pub const L4_AMOUNT: &str = "f6_132[0]";
    // Line 7 is deliberately absent.
    //
    // The additions split across lines 2 and 4, and the subtractions across 6 and
    // 7, by whether the difference is an income item or an expense one. Nothing
    // in a general ledger distinguishes those for the differences this program
    // cannot derive — the difference lives in the tax code — so what is left over
    // goes to whichever side it belongs on and to the one free-text row on that
    // side. Line 7 is pre-labelled on the printed form ("Depreciation"), so an
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
    /// The continuation row under line 7's caption. Used when the decrease has
    /// both a named part and an unexplained one, because line 7 has one money box
    /// and two rows of text to say what is in it.
    pub const L7_ITEMIZE_CONT: &str = "f6_152[0]";
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
    ///
    /// The **whole** difference, named part included. [`ScheduleM::m1_named`]
    /// and [`ScheduleM::m1_unallocated`] divide it; nothing adds to it.
    pub book_tax_difference: i64,
    /// Schedule K line 18c — nondeductible expenses, positive as the form prints
    /// them.
    ///
    /// Not a total on either schedule. It is the part of the differences above
    /// that has a name, and both schedules use it only to caption a figure they
    /// were going to carry anyway. See the module docs.
    pub nondeductible: i64,

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

    /// M-1 line 4: the part of the additions this program can name.
    ///
    /// Nondeductible expenses are an expense the books bear that Schedule K does
    /// not deduct, which is line 4's own description of itself, so the figure is
    /// captioned rather than left inside line 2's unallocated row.
    ///
    /// Clamped to the additions, and that is the whole subtlety. The difference
    /// is a *net* of every book-to-tax adjustment: a year with $200 of
    /// nondeductible expenses and $5,000 of the differences that run the other
    /// way nets to a subtraction, and writing $200 on line 4 would then oblige
    /// another $200 on line 7 to keep line 9 where it is — inventing a
    /// subtraction nobody computed, to caption an addition. So the naming stops
    /// where the additions stop. What is named is always true; what is left is
    /// always still reported.
    pub fn m1_named(&self) -> i64 {
        self.nondeductible.clamp(0, self.book_tax_difference.max(0))
    }

    /// M-1 line 2: the additions nobody has itemized yet.
    ///
    /// With [`ScheduleM::m1_named`] this sums to `book_tax_difference.max(0)`,
    /// which is what line 5 adds — so naming a component never moves a total.
    pub fn m1_unallocated(&self) -> i64 {
        self.book_tax_difference.max(0) - self.m1_named()
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

    /// The part of M-2's other decreases that has a name.
    ///
    /// Nondeductible expenses are money the partnership spent out of the
    /// partners' capital and got no deduction for, so capital falls by them with
    /// nothing on lines 3 or 6 to show it — which is exactly what line 7 is for.
    ///
    /// Clamped to the decrease for [`ScheduleM::m1_named`]'s reason, and for one
    /// more that is specific to M-2: the decrease is derived backwards from
    /// Schedule L's year-end capital, so it already contains every other
    /// movement the books made and the return did not explain. Naming more of it
    /// than it holds would put a figure on line 7 that line 9 then has to
    /// contradict.
    pub fn m2_named_decrease(&self) -> i64 {
        self.nondeductible.clamp(0, self.m2_other_decrease())
    }

    /// The rest of M-2's other decreases: still nobody's to explain but the
    /// filer's. With [`ScheduleM::m2_named_decrease`] this sums to the decrease
    /// line 8 was always going to carry.
    pub fn m2_unexplained_decrease(&self) -> i64 {
        self.m2_other_decrease() - self.m2_named_decrease()
    }

    /// Whether M-1 reconciles with nothing left over.
    ///
    /// Nothing on the fill path asks any more: what decides whether the page
    /// carries an "itemize before filing" row is now the part of the difference
    /// that has no name, not whether there is a difference at all — a year whose
    /// whole difference is the disallowed half of its meals reconciles in the
    /// sense that matters while this still returns false. Kept because it is the
    /// plain statement of M-1's own identity, and because the test that pins the
    /// totals against naming a component checks it alongside them.
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
/// `nondeductible` is Schedule K line 18c in whole dollars, positive. Passed in
/// rather than read off `lines` so a caller can reconcile figures that never came
/// from a mapping — the same reason every other input here is a figure.
pub fn reconcile(
    book_income_cents: i64,
    lines: &Form1065Lines,
    schedule_l: Option<&ScheduleL>,
    nondeductible: i64,
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
        nondeductible,
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
/// The opposite kind of label: this one says what the figure *is*, because for
/// once the program knows. See the module docs.
const NONDEDUCTIBLE_CAPITAL: &str = "Nondeductible expenses (Schedule K, line 18c)";

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
    // The additions in two parts: what has a name on line 4, what does not on
    // line 2. They sum to the same figure line 2 alone used to carry, so line 5
    // is untouched — see the module docs on why that is a hard constraint and
    // not merely a nicety.
    if m.m1_named() != 0 {
        set_text(doc, map, m1::L4_AMOUNT, &money(m.m1_named()))?;
    }
    if m.m1_unallocated() > 0 {
        set_text(doc, map, m1::L2_ITEMIZE, UNALLOCATED)?;
        set_text(doc, map, m1::L2_AMOUNT, &money(m.m1_unallocated()))?;
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

    // Only what is left unexplained is worth saying. A year whose whole
    // book-to-tax difference is the disallowed half of the meals is now a
    // finished M-1 with a caption on line 4, and telling its preparer to go and
    // break out a figure that is already broken out is how a warnings panel
    // teaches people to stop reading it.
    let left_over = if m.book_tax_difference < 0 {
        m.m1_line_8()
    } else {
        m.m1_unallocated()
    };
    if left_over != 0 {
        // The whole difference is still named first, because that is the figure a
        // reader can check against the page. What follows is how much of it is
        // still theirs to explain.
        let accounted = if m.m1_named() == 0 {
            String::new()
        } else {
            format!(
                "{} of it is nondeductible expenses, captioned on line 4, and ",
                money(m.m1_named())
            )
        };
        warnings.push(format!(
            "Schedule M-1: book income ({}) plus guaranteed payments ({}) differs from the \
             Analysis of Net Income ({}) by {}. {accounted}{} is a difference nothing here can \
             name — depreciation on two bases, tax-exempt income, a section 263A adjustment. The \
             form wants it itemized on lines 2, 4, 6 and 7, and it has been placed on one \
             unallocated row so the page foots; break it out before filing.",
            money(m.book_income),
            money(m.guaranteed_payments),
            money(m.analysis),
            money(m.book_tax_difference.abs()),
            money(left_over),
        ));
    }
    if m.m1_named() != 0 {
        warnings.push(format!(
            "Schedule M-1 line 4 carries {} of nondeductible expenses — the part of a limited \
             deduction the law disallows, already reported on Schedule K, line 18c. The books \
             bear the whole expense and the return deducts only the allowed part, so the \
             difference is an add-back. The statement behind the return itemizes it account by \
             account; line 4's own sub-rows are left blank rather than captioned \"Travel and \
             entertainment\", which line 18c is not only made of.",
            money(m.m1_named()),
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
    // Line 7 has one money box and two rows of text. The amount is the whole
    // decrease, unchanged — line 9 has to land on the balance sheet's own
    // year-end capital and nothing here may move it — and the two text rows say
    // how much of it has a name.
    let named_decrease = m.m2_named_decrease();
    let unexplained_decrease = m.m2_unexplained_decrease();
    if decrease != 0 {
        set_text(doc, map, m2::L7_AMOUNT, &money(decrease))?;
        match (named_decrease, unexplained_decrease) {
            (0, _) => set_text(doc, map, m2::L7_ITEMIZE, UNEXPLAINED_CAPITAL)?,
            (named, 0) => set_text(
                doc,
                map,
                m2::L7_ITEMIZE,
                &format!("{NONDEDUCTIBLE_CAPITAL} {}", money(named)),
            )?,
            (named, rest) => {
                set_text(
                    doc,
                    map,
                    m2::L7_ITEMIZE,
                    &format!("{NONDEDUCTIBLE_CAPITAL} {}", money(named)),
                )?;
                set_text(
                    doc,
                    map,
                    m2::L7_ITEMIZE_CONT,
                    &format!("{UNEXPLAINED_CAPITAL} {}", money(rest)),
                )?;
            }
        }
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
    } else if increase != 0 || unexplained_decrease != 0 {
        warnings.push(format!(
            "Schedule M-2: capital moved by {} that income, distributions and nondeductible \
             expenses do not explain — contributions, draws posted outside the distribution \
             accounts, a prior-period adjustment. Placed on one unallocated row so line 9 lands \
             on the balance sheet's year-end capital; itemize it before filing.",
            money(increase.max(unexplained_decrease)),
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
        let m = reconcile(70_000_00, &lines, None, 0);
        assert_eq!(m.book_income, 70_000);
        assert_eq!(m.guaranteed_payments, 30_000);
        assert_eq!(m.analysis, lines.k_analysis());
        assert!(
            m.m1_reconciles(),
            "difference was {}",
            m.book_tax_difference
        );
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
        let m = reconcile(60_000_00, &lines, None, 0);
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
            let m = reconcile(book_cents, &lines, None, 0);
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

        let m = reconcile(30_000_00, &lines, Some(&l), 0);
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

        let m = reconcile(30_000_00, &lines, Some(&l), 0);
        assert_eq!(m.m2_line_9(), 143_000, "line 9 must still tie");
        assert!(m.m2_ties_to_the_balance_sheet());

        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(
            get_value(&doc, &map, m2::L4_AMOUNT).as_deref(),
            Some("25,000")
        );
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
        let m = reconcile(30_000_00, &lines, Some(&l), 0);

        let (mut doc, map) = form();
        fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(
            get_value(&doc, &map, m2::L7_AMOUNT).as_deref(),
            Some("30,000")
        );
        assert_eq!(
            get_value(&doc, &map, m2::L9_END).as_deref(),
            Some("100,000")
        );
    }

    /// Without a balance sheet, M-2's opening balance is not knowable and the
    /// page must say so rather than start from zero as though that were a fact.
    #[test]
    fn no_balance_sheet_means_m2_says_it_cannot_open() {
        let m = reconcile(30_000_00, &Form1065Lines::default(), None, 0);
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
        let m = reconcile(30_000_00, &Form1065Lines::default(), None, 0);
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

    /// The hard constraint, pinned: naming a component inside a residual must not
    /// move a single total on either schedule.
    ///
    /// M-2 line 9 has to land on Schedule L's own year-end capital, which is book
    /// basis, while line 3 is the tax Analysis figure — the residual is what
    /// absorbs that mismatch. Attributing part of it is safe; changing the
    /// arithmetic is not, and would break the tie-out to the balance sheet. This
    /// runs the whole grid of inputs and compares every total against the same
    /// schedule reconciled with no named component at all.
    #[test]
    fn naming_the_nondeductible_component_moves_no_total() {
        // The three book-income figures put the M-1 difference above, on and
        // below zero; the capital figures do the same to M-2's, including the
        // two boundaries the clamp turns on — a decrease exactly equal to the
        // named figure, and a decrease of nothing — and the *increase* side,
        // which has no named part at all. `None` is the no-balance-sheet case,
        // where M-2 has no residual to carve anything out of.
        //
        // 188,000 is what M-2 lands on with nothing unexplained: 100,000 of
        // opening capital plus a 100,000 Analysis less 12,000 of distributions.
        let (mut doc, map) = form();
        for book_cents in [60_000_00i64, 100_000_00, 140_000_00] {
            for end_capital in [
                Some(80_000i64),
                Some(100_000),
                Some(143_000),
                Some(187_860), // a decrease of exactly 140 — the clamp boundary
                Some(188_000), // no decrease at all
                Some(250_000), // an increase, which nothing here may name
                None,          // no balance sheet
            ] {
                for nondeductible in [-40_000i64, -1, 0, 140, 39_999, 40_000, 40_001, 500_000] {
                    let mut l = ScheduleL::default();
                    if let Some(end) = end_capital {
                        l.set_for_test("sl21", 100_000, end);
                    }
                    let sheet = end_capital.map(|_| &l);
                    let mut lines = Form1065Lines::default();
                    lines.set_for_test("l1a", 100_000);
                    lines.set_for_test("k19a", 12_000);
                    lines.set_for_test("k18c", nondeductible);

                    let named = reconcile(book_cents, &lines, sheet, nondeductible);
                    let anonymous = reconcile(book_cents, &lines, sheet, 0);
                    let case =
                        format!("books {book_cents}, capital {end_capital:?}, 18c {nondeductible}");

                    assert_eq!(
                        named.m1_line_5(),
                        anonymous.m1_line_5(),
                        "M-1 line 5: {case}"
                    );
                    assert_eq!(
                        named.m1_line_8(),
                        anonymous.m1_line_8(),
                        "M-1 line 8: {case}"
                    );
                    assert_eq!(
                        named.m1_line_5() - named.m1_line_8(),
                        named.analysis,
                        "M-1 line 9: {case}"
                    );
                    assert_eq!(
                        named.m1_reconciles(),
                        anonymous.m1_reconciles(),
                        "M-1 reconciles: {case}"
                    );
                    assert_eq!(
                        named.m2_line_5(),
                        anonymous.m2_line_5(),
                        "M-2 line 5: {case}"
                    );
                    assert_eq!(
                        named.m2_line_8(),
                        anonymous.m2_line_8(),
                        "M-2 line 8: {case}"
                    );
                    assert_eq!(
                        named.m2_line_9(),
                        anonymous.m2_line_9(),
                        "M-2 line 9: {case}"
                    );
                    assert!(named.m2_ties_to_the_balance_sheet(), "M-2 ties: {case}");

                    // And the two halves of each residual are exactly the residual.
                    assert_eq!(
                        named.m1_named() + named.m1_unallocated(),
                        named.book_tax_difference.max(0),
                        "M-1 additions: {case}"
                    );
                    assert_eq!(
                        named.m2_named_decrease() + named.m2_unexplained_decrease(),
                        anonymous.m2_line_8()
                            - named.distributions_cash
                            - named.distributions_property,
                        "M-2 decreases: {case}"
                    );
                    // Neither named figure ever exceeds its own side, or runs
                    // negative, or claims more than line 18c itself carries —
                    // the three ways a clamp can be wrong.
                    for part in [named.m1_named(), named.m2_named_decrease()] {
                        assert!(part >= 0, "a named part went negative: {case}");
                        assert!(
                            part <= nondeductible.max(0),
                            "more was named than line 18c carries: {case}"
                        );
                    }
                    assert!(
                        named.m1_named() <= named.book_tax_difference.max(0),
                        "M-1 named more than the additions: {case}"
                    );
                    assert!(
                        named.m2_named_decrease()
                            <= anonymous.m2_line_8()
                                - named.distributions_cash
                                - named.distributions_property,
                        "M-2 named more than the decrease: {case}"
                    );
                    assert_eq!(
                        named.has_balance_sheet,
                        end_capital.is_some(),
                        "balance sheet: {case}"
                    );
                    // The page itself, not only the arithmetic: filling the form
                    // must not fail, and whatever it says about the figure has to
                    // read as a sentence. One blank form, reused — loading the
                    // vendored PDF a hundred and sixty-eight times is a minute of
                    // test run for nothing, and nothing here reads a field back.
                    let warnings = fill(&mut doc, &map, &named, true).unwrap();
                    if !warnings.is_empty() {
                        crate::tax::warning_shape::assert_all(&warnings);
                    }
                }
            }
        }
    }

    /// The disallowed half of a meal is an expense the books bear that Schedule K
    /// does not deduct, which is what M-1 line 4 is. Captioned there rather than
    /// swept into line 2's "itemize before filing" row — and the page still foots.
    #[test]
    fn the_disallowed_half_of_a_meal_lands_on_m1_line_4_by_name() {
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 100_000);
        lines.set_for_test("k18c", 140);
        // Books bear the whole meal; the return adds back the disallowed half,
        // and nothing else differs.
        let m = reconcile(99_860_00, &lines, None, 140);

        assert_eq!(m.book_tax_difference, 140);
        assert_eq!(m.m1_named(), 140);
        assert_eq!(m.m1_unallocated(), 0, "nothing is left unexplained");

        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(get_value(&doc, &map, m1::L4_AMOUNT).as_deref(), Some("140"));
        assert_eq!(
            get_value(&doc, &map, m1::L2_AMOUNT),
            None,
            "the unallocated row is empty because nothing is unallocated"
        );
        assert_eq!(
            m.m1_line_5() - m.m1_line_8(),
            m.analysis,
            "the page still foots"
        );
        assert!(
            !warnings.iter().any(|w| w.contains("break it out")),
            "nothing is left to break out: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("line 18c")),
            "the named figure is still worth saying out loud: {warnings:?}"
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }

    /// A difference bigger than the part that has a name: line 4 takes what it
    /// can explain, line 2 keeps the rest and keeps saying so.
    #[test]
    fn what_the_named_component_does_not_cover_stays_unallocated() {
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 100_000);
        lines.set_for_test("k18c", 140);
        let m = reconcile(60_000_00, &lines, None, 140);

        assert_eq!(m.book_tax_difference, 40_000);
        assert_eq!(m.m1_named(), 140);
        assert_eq!(m.m1_unallocated(), 39_860);

        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(get_value(&doc, &map, m1::L4_AMOUNT).as_deref(), Some("140"));
        assert_eq!(
            get_value(&doc, &map, m1::L2_AMOUNT).as_deref(),
            Some("39,860")
        );
        assert!(
            warnings.iter().any(|w| w.contains("break it out")),
            "{warnings:?}"
        );
        // The residual warning now names both halves, so it is the one most
        // likely to have picked up a swallowed line continuation.
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("39,860") && w.contains("captioned on line 4")),
            "{warnings:?}"
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }

    /// The clamp. A year whose differences net to a subtraction cannot carry a
    /// named addition: writing one would oblige an invented subtraction beside it
    /// to keep line 9 where it is.
    #[test]
    fn nothing_is_named_when_the_difference_runs_the_other_way() {
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 100_000);
        lines.set_for_test("k18c", 140);
        let m = reconcile(140_000_00, &lines, None, 140);

        assert!(m.book_tax_difference < 0);
        assert_eq!(m.m1_named(), 0, "the additions side is empty");
        assert_eq!(m.m1_line_5() - m.m1_line_8(), m.analysis);

        let (mut doc, map) = form();
        fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(get_value(&doc, &map, m1::L4_AMOUNT), None);
    }

    /// M-2's other decreases, same treatment: the amount box is untouched — line
    /// 9 must still land on the balance sheet — and the caption says how much of
    /// it is explained.
    #[test]
    fn m2_line_7_names_the_nondeductible_part_of_the_decrease() {
        let mut l = ScheduleL::default();
        l.set_for_test("sl21", 100_000, 100_000);
        let mut lines = Form1065Lines::default();
        lines.set_for_test("l1a", 30_000);
        lines.set_for_test("k18c", 140);

        let m = reconcile(30_000_00, &lines, Some(&l), 140);
        assert_eq!(m.m2_named_decrease(), 140);
        assert_eq!(m.m2_unexplained_decrease(), 29_860);

        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &m, true).unwrap();
        assert_eq!(
            get_value(&doc, &map, m2::L7_AMOUNT).as_deref(),
            Some("30,000"),
            "the amount box carries the whole decrease, as it always did"
        );
        assert!(
            get_value(&doc, &map, m2::L7_ITEMIZE)
                .unwrap_or_default()
                .contains("18c"),
            "{:?}",
            get_value(&doc, &map, m2::L7_ITEMIZE)
        );
        assert!(
            get_value(&doc, &map, m2::L7_ITEMIZE_CONT)
                .unwrap_or_default()
                .contains("itemize"),
            "the rest is still nobody's to explain"
        );
        assert_eq!(
            get_value(&doc, &map, m2::L9_END).as_deref(),
            Some("100,000")
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }

    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_form() {
        let (doc, map) = form();
        for name in [
            m1::L1_BOOK_INCOME,
            m1::L2_ITEMIZE,
            m1::L2_AMOUNT,
            m1::L3_GUARANTEED,
            m1::L4_AMOUNT,
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
            m2::L7_ITEMIZE_CONT,
            m2::L7_AMOUNT,
            m2::L8_TOTAL,
            m2::L9_END,
        ] {
            assert!(map.find(name).is_some(), "f1065.pdf has no field {name}");
        }
        let _ = doc;
    }
}
