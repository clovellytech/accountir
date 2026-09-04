//! Schedule C (Form 1040), "Profit or Loss From Business (Sole Proprietorship)".
//!
//! # What is different from Form 1065, and what is not
//!
//! Not different: the money. Both forms total the same income statement onto
//! numbered lines through the same account-to-line mapping, so this module shares
//! [`super::lines::sum_by_line`] with the partnership return rather than growing
//! a second implementation of the arithmetic. In particular it shares the check
//! that catches accounts reaching no line at all, which is the one that finds
//! money going missing from a return.
//!
//! Different: almost everything else.
//!
//! A partnership return is a return. Schedule C is an *attachment* to the
//! owner's Form 1040 — it has no filing of its own, no Schedule K-1, no balance
//! sheet, and the identifying number in its header is the owner's **social
//! security number** rather than the business's EIN. It ends at line 31 with one
//! net figure that goes onto the owner's 1040 and Schedule SE, where a
//! partnership's income fragments across partners and separately stated items.
//!
//! So there is no equivalent here of Schedule K: no line for charitable
//! contributions, none for section 179, none for investment interest. On a
//! Schedule C those are either an ordinary expense or the owner's own business on
//! their 1040, and a line catalogue that offered them would invite somebody to
//! map an account to a line that does not exist on this form.
//!
//! # Line keys are prefixed `sc`
//!
//! One `tax_line_mappings` table serves both forms — an account maps to one line,
//! and one set of books files one return, so there is no collision to arbitrate.
//! The `sc` prefix keeps the two vocabularies apart anyway, so that a book which
//! switches type has mappings that are *visibly* pointing at the wrong form
//! rather than silently landing on a same-numbered line of the other one.
//! [`super::lines::sum_by_line`] reports those as unrecognised keys, which is
//! exactly the right outcome: they are, for this form.
//!
//! # What is not filled, and why it says so
//!
//! Part III (cost of goods sold) and Part V (other expenses) have their own
//! lines here and are filled from the mapping like everything else. Part IV, the
//! vehicle questions, is not: mileage is not in a ledger. Line 30, business use of
//! the home, is not: it needs a Form 8829 or the simplified-method worksheet, and
//! neither is derivable from the books. Both are reported rather than left
//! silently blank.

use std::collections::BTreeMap;

use chrono::Datelike;

use lopdf::Document;

use super::acroform::{FormError, field_map, set_check, set_text, strip_xfa};
use super::lines::{
    Field, Sense, TaxLineDef, cents_to_dollars, format_dollars, sum_by_line,
};
use crate::domain::{AccountingMethod, BusinessProfile, SoleProprietor};
use crate::queries::reports::IncomeStatement;

const F1040SC: &[u8] = include_bytes!("../../assets/irs/f1040sc.pdf");

/// The tax year the vendored form is the revision for.
pub const FORM_TAX_YEAR: i32 = 2025;

/// Which part of the form a line belongs to — what a mapping editor groups by.
pub const INCOME: &str = "Income";
pub const EXPENSES: &str = "Expenses";
pub const COST_OF_GOODS: &str = "Cost of goods sold";
pub const OTHER_EXPENSES: &str = "Other expenses";

/// Every line an account can be mapped to.
///
/// Reuses [`TaxLineDef`] because the shape is genuinely the same — a key, a
/// number, a box, and whether the line prints its accounts positive or negative
/// — and a parallel struct would mean two mapping editors. The `schedule` field
/// is unused here and the `field` is the Schedule C box; nothing reads a Schedule
/// C line through `lines::line_def`, which searches the 1065 catalogue only.
///
/// Derived lines are deliberately absent, exactly as they are on the 1065: line 3
/// is 1 less 2 and line 28 is the sum of 8 through 27b, so an account mapped to
/// one would be counted twice. [`ScheduleCLines`] computes them.
pub const SCHEDULE_C_LINES: &[TaxLineDef] = &[
    // --- Part I, income ---
    TaxLineDef { key: "sc1", number: "1", label: "Gross receipts or sales", group: INCOME, schedule: super::lines::Schedule::Page1, field: Field::One("f1_10[0]"), sense: Sense::Natural,
        instructions: "Everything the business took in for goods or services before any deduction. Income reported to you on a Form W-2 with the statutory employee box ticked belongs here too, and the box beside line 1 has to be ticked with it.",
        attachment: None },
    TaxLineDef { key: "sc2", number: "2", label: "Returns and allowances", group: INCOME, schedule: super::lines::Schedule::Page1, field: Field::One("f1_11[0]"), sense: Sense::Contra,
        instructions: "Refunds and price allowances given to customers. Enter as a positive figure; the form subtracts it from line 1.",
        attachment: None },
    TaxLineDef { key: "sc6", number: "6", label: "Other income", group: INCOME, schedule: super::lines::Schedule::Page1, field: Field::One("f1_15[0]"), sense: Sense::Natural,
        instructions: "Business income that is not from sales — a federal or state fuel tax credit or refund, recoveries of bad debts, interest on business bank accounts. Not the proceeds of selling business property, which is Form 4797.",
        attachment: None },

    // --- Part II, expenses ---
    TaxLineDef { key: "sc8", number: "8", label: "Advertising", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_17[0]"), sense: Sense::Natural,
        instructions: "Advertising and promotion.",
        attachment: None },
    TaxLineDef { key: "sc9", number: "9", label: "Car and truck expenses", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_18[0]"), sense: Sense::Natural,
        instructions: "Either actual costs or the standard mileage rate, but the choice is made in the first year and constrains later ones. Claiming anything here means completing Part IV, or Form 4562 if one is required for this business.",
        attachment: None },
    TaxLineDef { key: "sc10", number: "10", label: "Commissions and fees", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_19[0]"), sense: Sense::Natural,
        instructions: "Commissions and fees paid to others. Not payments to yourself — a sole proprietor cannot pay themselves a deductible wage or fee.",
        attachment: None },
    TaxLineDef { key: "sc11", number: "11", label: "Contract labor", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_20[0]"), sense: Sense::Natural,
        instructions: "Payments to independent contractors. These are what questions I and J are about: pay a contractor $600 or more and a Form 1099-NEC is required.",
        attachment: None },
    TaxLineDef { key: "sc12", number: "12", label: "Depletion", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_21[0]"), sense: Sense::Natural,
        instructions: "Depletion of mines, wells and other natural deposits.",
        attachment: None },
    TaxLineDef { key: "sc13", number: "13", label: "Depreciation and section 179 expense deduction", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_22[0]"), sense: Sense::Natural,
        instructions: "From Form 4562. Unlike a partnership, a sole proprietor's §179 is deducted here rather than separately stated — there are no partners to apply their own limits, so the whole of Form 4562 line 22 lands on this one line.",
        attachment: Some(FORM_4562) },
    TaxLineDef { key: "sc14", number: "14", label: "Employee benefit programs", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_23[0]"), sense: Sense::Natural,
        instructions: "Benefit programs for employees other than pension and profit-sharing, which is line 19. Not the proprietor's own health insurance — that is an adjustment on Form 1040, not a business deduction.",
        attachment: None },
    TaxLineDef { key: "sc15", number: "15", label: "Insurance (other than health)", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_24[0]"), sense: Sense::Natural,
        instructions: "Business insurance: liability, property, malpractice. Health insurance is not here.",
        attachment: None },
    TaxLineDef { key: "sc16a", number: "16a", label: "Interest — mortgage (paid to banks, etc.)", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_25[0]"), sense: Sense::Natural,
        instructions: "Mortgage interest on business real property, paid to a financial institution.",
        attachment: None },
    TaxLineDef { key: "sc16b", number: "16b", label: "Interest — other", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_26[0]"), sense: Sense::Natural,
        instructions: "Other business interest — a business credit card, an equipment loan.",
        attachment: None },
    TaxLineDef { key: "sc17", number: "17", label: "Legal and professional services", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_27[0]"), sense: Sense::Natural,
        instructions: "Legal, accounting and other professional fees for the business.",
        attachment: None },
    TaxLineDef { key: "sc18", number: "18", label: "Office expense", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_28[0]"), sense: Sense::Natural,
        instructions: "Office costs — postage, stationery, software subscriptions. Not the home office, which is line 30.",
        attachment: None },
    TaxLineDef { key: "sc19", number: "19", label: "Pension and profit-sharing plans", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_29[0]"), sense: Sense::Natural,
        instructions: "Contributions to plans for employees. The proprietor's own contribution to a SEP or SIMPLE is an adjustment on Form 1040 rather than a deduction here.",
        attachment: None },
    TaxLineDef { key: "sc20a", number: "20a", label: "Rent or lease — vehicles, machinery, equipment", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_30[0]"), sense: Sense::Natural,
        instructions: "Rent or lease of vehicles, machinery and equipment.",
        attachment: None },
    TaxLineDef { key: "sc20b", number: "20b", label: "Rent or lease — other business property", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_31[0]"), sense: Sense::Natural,
        instructions: "Rent for premises — a studio, a workshop, a shop.",
        attachment: None },
    TaxLineDef { key: "sc21", number: "21", label: "Repairs and maintenance", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_32[0]"), sense: Sense::Natural,
        instructions: "Work that neither adds to the value of the property nor appreciably lengthens its life. Work that does either is a capital improvement and is depreciated instead.",
        attachment: None },
    TaxLineDef { key: "sc22", number: "22", label: "Supplies", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_33[0]"), sense: Sense::Natural,
        instructions: "Supplies consumed in the business and not part of cost of goods sold — which is where materials that go into what you sell belong instead.",
        attachment: None },
    TaxLineDef { key: "sc23", number: "23", label: "Taxes and licenses", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_34[0]"), sense: Sense::Natural,
        instructions: "Business taxes and licences: the employer share of payroll taxes, sales tax paid on business purchases, licence fees. Not federal income tax, and not the self-employment tax.",
        attachment: None },
    TaxLineDef { key: "sc24a", number: "24a", label: "Travel", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_35[0]"), sense: Sense::Natural,
        instructions: "Business travel away from home. Not commuting, which is never deductible.",
        attachment: None },
    TaxLineDef { key: "sc24b", number: "24b", label: "Deductible meals", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_36[0]"), sense: Sense::Natural,
        instructions: "The deductible part of business meals — generally half. Map an account holding the full cost and the return claims twice what it should; either map an account that already carries the deductible half, or adjust before filing.",
        attachment: None },
    TaxLineDef { key: "sc25", number: "25", label: "Utilities", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_37[0]"), sense: Sense::Natural,
        instructions: "Utilities for business premises. Utilities for a home office go to line 30 through Form 8829, not here.",
        attachment: None },
    TaxLineDef { key: "sc26", number: "26", label: "Wages (less employment credits)", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_38[0]"), sense: Sense::Natural,
        instructions: "Wages paid to employees, reduced by any employment credits claimed. Never the proprietor's own draw — a sole proprietor is not an employee of their own business.",
        attachment: None },
    TaxLineDef { key: "sc27a", number: "27a", label: "Energy efficient commercial buildings deduction", group: EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f1_39[0]"), sense: Sense::Natural,
        instructions: "The §179D deduction, computed on Form 7205.",
        attachment: Some(FORM_7205) },

    // --- Part III, cost of goods sold ---
    TaxLineDef { key: "sc35", number: "35", label: "Inventory at beginning of year", group: COST_OF_GOODS, schedule: super::lines::Schedule::Page1, field: Field::One("f2_1[0]"), sense: Sense::Natural,
        instructions: "Opening inventory. If it differs from last year's closing inventory, an explanation has to be attached.",
        attachment: None },
    TaxLineDef { key: "sc36", number: "36", label: "Purchases less cost of items withdrawn for personal use", group: COST_OF_GOODS, schedule: super::lines::Schedule::Page1, field: Field::One("f2_2[0]"), sense: Sense::Natural,
        instructions: "Goods bought for resale, less anything taken for personal use.",
        attachment: None },
    TaxLineDef { key: "sc37", number: "37", label: "Cost of labor", group: COST_OF_GOODS, schedule: super::lines::Schedule::Page1, field: Field::One("f2_3[0]"), sense: Sense::Natural,
        instructions: "Labour that goes into producing what is sold. Never amounts paid to yourself.",
        attachment: None },
    TaxLineDef { key: "sc38", number: "38", label: "Materials and supplies", group: COST_OF_GOODS, schedule: super::lines::Schedule::Page1, field: Field::One("f2_4[0]"), sense: Sense::Natural,
        instructions: "Materials and supplies that become part of what is sold — clay and glazes for pots that are sold, not the studio's cleaning supplies.",
        attachment: None },
    TaxLineDef { key: "sc39", number: "39", label: "Other costs", group: COST_OF_GOODS, schedule: super::lines::Schedule::Page1, field: Field::One("f2_5[0]"), sense: Sense::Natural,
        instructions: "Other costs of producing what is sold — freight in, containers, overhead attributable to production.",
        attachment: None },
    TaxLineDef { key: "sc41", number: "41", label: "Inventory at end of year", group: COST_OF_GOODS, schedule: super::lines::Schedule::Page1, field: Field::One("f2_7[0]"), sense: Sense::Contra,
        instructions: "Closing inventory. Enter as a positive figure; the form subtracts it, because what is still on the shelf was not sold.",
        attachment: None },

    // --- Part V, other expenses, totalled onto line 27b ---
    TaxLineDef { key: "sc48", number: "48", label: "Other expenses", group: OTHER_EXPENSES, schedule: super::lines::Schedule::Page1, field: Field::One("f2_33[0]"), sense: Sense::Natural,
        instructions: "Business expenses that fit none of lines 8 through 27a — bank charges, dues and subscriptions, continuing education. Part V itemises them by account and the total carries to line 27b, so map each such account here and the list writes itself.",
        attachment: None },
];

/// Attachments a Schedule C line obliges, named here rather than in
/// [`super::lines`] because that catalogue's copies are about Form 1065's lines.
const FORM_4562: super::lines::Attachment = super::lines::Attachment {
    name: "Form 4562",
    url: "https://www.irs.gov/forms-pubs/about-form-4562",
    generated: false,
};
const FORM_7205: super::lines::Attachment = super::lines::Attachment {
    name: "Form 7205",
    url: "https://www.irs.gov/forms-pubs/about-form-7205",
    generated: false,
};

/// The definition of one Schedule C line, by key.
pub fn line_def(key: &str) -> Option<&'static TaxLineDef> {
    SCHEDULE_C_LINES.iter().find(|d| d.key == key)
}

/// Every valid Schedule C line key.
pub fn line_keys() -> Vec<&'static str> {
    SCHEDULE_C_LINES.iter().map(|d| d.key).collect()
}

/// Schedule C's lines, in whole dollars, as computed from the books.
#[derive(Debug, Clone, Default)]
pub struct ScheduleCLines {
    mapped: BTreeMap<&'static str, i64>,
}

impl ScheduleCLines {
    pub fn get(&self, key: &str) -> i64 {
        self.mapped.get(key).copied().unwrap_or(0)
    }

    pub fn is_mapped(&self, key: &str) -> bool {
        self.mapped.contains_key(key)
    }

    pub fn is_empty(&self) -> bool {
        self.mapped.is_empty()
    }

    /// Set a line directly. Tests only — the real path is [`compute`], which is
    /// the only thing that knows the sign conventions.
    #[cfg(test)]
    pub fn set_for_test(&mut self, key: &'static str, dollars: i64) {
        self.mapped.insert(key, dollars);
    }

    // --- derived lines: arithmetic on the rounded dollars above ---
    //
    // Computed from the printed dollars rather than from unrounded cents, the
    // same rule `lines` follows: a total derived from cents does not always equal
    // the sum of the figures printed above it, and the page has to add up as
    // read.

    /// Line 3. Gross receipts less returns and allowances.
    pub fn line_3(&self) -> i64 {
        self.get("sc1") - self.get("sc2")
    }

    /// Line 40. Opening inventory plus purchases, labour, materials and other
    /// costs.
    pub fn line_40(&self) -> i64 {
        ["sc35", "sc36", "sc37", "sc38", "sc39"]
            .iter()
            .map(|k| self.get(k))
            .sum()
    }

    /// Line 42, and line 4. Cost of goods sold — line 40 less closing inventory.
    ///
    /// Zero when nothing in Part III is mapped, so a business that sells services
    /// and holds no stock gets a blank Part III and a line 4 of nothing, rather
    /// than a zero that looks like an answer.
    pub fn cost_of_goods_sold(&self) -> i64 {
        if !self.has_cost_of_goods() {
            return 0;
        }
        self.line_40() - self.get("sc41")
    }

    /// Whether any Part III line is mapped at all.
    pub fn has_cost_of_goods(&self) -> bool {
        SCHEDULE_C_LINES
            .iter()
            .filter(|d| d.group == COST_OF_GOODS)
            .any(|d| self.is_mapped(d.key))
    }

    /// Line 5. Gross profit — line 3 less cost of goods sold.
    pub fn line_5(&self) -> i64 {
        self.line_3() - self.cost_of_goods_sold()
    }

    /// Line 7. Gross income — gross profit plus other income.
    pub fn line_7(&self) -> i64 {
        self.line_5() + self.get("sc6")
    }

    /// Line 28. Total expenses before business use of the home.
    ///
    /// Every Part II line, with "less" lines subtracting. Line 27b (other
    /// expenses, from Part V) is included through [`Self::line_27b`].
    pub fn line_28(&self, other_expenses: i64) -> i64 {
        let part_ii: i64 = SCHEDULE_C_LINES
            .iter()
            .filter(|d| d.group == EXPENSES)
            .map(|d| match d.sense {
                Sense::Natural => self.get(d.key),
                Sense::Contra => -self.get(d.key),
            })
            .sum();
        part_ii + other_expenses
    }

    /// Line 29. Tentative profit or loss — gross income less total expenses.
    pub fn line_29(&self, other_expenses: i64) -> i64 {
        self.line_7() - self.line_28(other_expenses)
    }

    /// Line 31. Net profit or loss, after business use of the home.
    ///
    /// This is the figure that leaves the form: it goes to Form 1040 Schedule 1
    /// line 3 and to Schedule SE. Everything above it exists to produce it.
    pub fn line_31(&self, other_expenses: i64, home_office: i64) -> i64 {
        self.line_29(other_expenses) - home_office
    }
}

/// What [`compute`] found.
pub struct Computed {
    pub lines: ScheduleCLines,
    /// Which accounts made up each line, by key.
    pub detail: BTreeMap<&'static str, Vec<super::lines::LineDetail>>,
    pub warnings: Vec<String>,
}

/// Total a year's income statement onto Schedule C's lines.
pub fn compute(
    statement: &IncomeStatement,
    mapping: &BTreeMap<String, String>,
) -> Computed {
    let (cents, detail, warnings) = sum_by_line(
        statement,
        mapping,
        &|key| line_def(key).map(|d| (d.key, d.sense)),
        "Schedule C",
    );
    let mapped: BTreeMap<&'static str, i64> = cents
        .into_iter()
        .map(|(k, c)| (k, cents_to_dollars(c)))
        .collect();
    Computed {
        lines: ScheduleCLines { mapped },
        detail,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// The questions the books cannot answer
// ---------------------------------------------------------------------------

pub const YES: &str = "yes";
pub const NO: &str = "no";

/// How a question is answered, and what value ticks the box.
///
/// The on-state is carried per question rather than assumed, because this form
/// is not consistent about it: questions G, I and J tick with `Yes`/`No`, while
/// 32 and 34 tick with `1`/`2` — on the same page, in the same revision. A
/// checkbox written with the wrong state silently ticks nothing while every
/// field-existence check still passes, so the values live beside the boxes and
/// [`tests::the_checkbox_states_are_the_ones_the_form_was_built_with`] holds them
/// to the vendored PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// A yes/no pair of boxes, with the value each is ticked by.
    YesNo {
        yes: &'static str,
        no: &'static str,
        yes_on: &'static str,
        no_on: &'static str,
    },
    /// A single box, ticked or not.
    Check { on: &'static str, on_state: &'static str },
}

/// One of Schedule C's questions.
pub struct Question {
    pub key: &'static str,
    pub number: &'static str,
    pub text: &'static str,
    pub control: Control,
    /// What a Yes obliges, for the screen to say before the answer is given.
    pub yes_warning: &'static str,
}

/// The questions on Schedule C that no ledger can answer.
///
/// Kept apart from the lines because they are a different kind of thing: a line
/// is a sum over accounts, and these are facts about how the business was run.
/// Keyed by tax year in `schedule_c_answers`, because every one of them asks
/// about a particular year.
pub const QUESTIONS: &[Question] = &[
    Question {
        key: "g",
        number: "G",
        text: "Did you \u{201c}materially participate\u{201d} in the operation of this business during the year?",
        control: Control::YesNo { yes: "c1_2[0]", no: "c1_2[1]", yes_on: "Yes", no_on: "No" },
        yes_warning: "",
    },
    Question {
        key: "h",
        number: "H",
        text: "Did you start or acquire this business during the year?",
        control: Control::Check { on: "c1_3[0]", on_state: "1" },
        yes_warning: "",
    },
    Question {
        key: "i",
        number: "I",
        text: "Did you make any payments during the year that would require you to file Form(s) 1099?",
        control: Control::YesNo { yes: "c1_4[0]", no: "c1_4[1]", yes_on: "Yes", no_on: "No" },
        yes_warning: "Answering Yes obliges question J as well. Paying a contractor $600 or more generally requires a Form 1099-NEC, filed separately from this return — the amount on line 11 is the usual reason this is Yes.",
    },
    Question {
        key: "j",
        number: "J",
        text: "If \u{201c}Yes\u{201d} to question I, did you or will you file the required Form(s) 1099?",
        control: Control::YesNo { yes: "c1_5[0]", no: "c1_5[1]", yes_on: "Yes", no_on: "No" },
        yes_warning: "",
    },
    Question {
        key: "line32",
        number: "32",
        text: "If you have a loss, is all of your investment in this activity at risk?",
        control: Control::YesNo { yes: "c1_7[0]", no: "c1_7[1]", yes_on: "1", no_on: "2" },
        yes_warning: "",
    },
    Question {
        key: "line34",
        number: "34",
        text: "Was there any change in determining quantities, costs, or valuations between opening and closing inventory?",
        control: Control::YesNo { yes: "c2_4[0]", no: "c2_4[1]", yes_on: "1", no_on: "2" },
        yes_warning: "Answering Yes obliges an explanation attached to the return, which this program does not produce.",
    },
];

pub fn question(key: &str) -> Option<&'static Question> {
    QUESTIONS.iter().find(|q| q.key == key)
}

/// The answers given for one tax year.
#[derive(Debug, Clone, Default)]
pub struct ScheduleC {
    answers: BTreeMap<String, String>,
}

impl ScheduleC {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.answers.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.answers.insert(key.to_string(), value.to_string());
    }

    pub fn is_empty(&self) -> bool {
        self.answers.is_empty()
    }
}

/// Read a year's answers from the books.
pub fn load(conn: &rusqlite::Connection, tax_year: i32) -> ScheduleC {
    let mut out = ScheduleC::default();
    let Ok(mut stmt) = conn.prepare(
        "SELECT answer_key, value FROM schedule_c_answers WHERE tax_year = ?1",
    ) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([tax_year], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) {
        for (k, v) in rows.flatten() {
            out.answers.insert(k, v);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Filling the form
// ---------------------------------------------------------------------------

/// Boxes the form computes rather than takes from an account.
mod field {
    /// The header: the person, then the business.
    pub const PROPRIETOR: &str = "f1_1[0]";
    /// The owner's SSN. Blank when this machine holds none — see the module
    /// docs on why it is not in the log.
    pub const SSN: &str = "f1_2[0]";
    /// A, principal business or profession.
    pub const PRINCIPAL_BUSINESS: &str = "f1_3[0]";
    /// B, the business code.
    pub const BUSINESS_CODE: &str = "f1_4[0]";
    /// C, business name.
    pub const BUSINESS_NAME: &str = "f1_5[0]";
    /// D, the business's EIN. Reported whenever the business has one — it is not
    /// "the partnership box" and not a place to repeat the SSN, which the
    /// instructions forbid outright. What is optional is *having* an EIN, not
    /// entering one you have.
    pub const EIN: &str = "f1_6[0]";
    /// E, address.
    pub const STREET: &str = "f1_7[0]";
    pub const CITY_STATE_ZIP: &str = "f1_8[0]";
    /// F(3), the method named when it is neither cash nor accrual.
    pub const METHOD_OTHER: &str = "f1_9[0]";
    /// F, the three method boxes.
    pub const METHOD_CASH: &str = "c1_1[0]";
    pub const METHOD_ACCRUAL: &str = "c1_1[1]";
    pub const METHOD_OTHER_BOX: &str = "c1_1[2]";

    /// Part I derived boxes: 3, 4, 5, 7.
    pub const L3_BALANCE: &str = "f1_12[0]";
    pub const L4_COST_OF_GOODS: &str = "f1_13[0]";
    pub const L5_GROSS_PROFIT: &str = "f1_14[0]";
    pub const L7_GROSS_INCOME: &str = "f1_16[0]";

    /// Part II derived: 27b from Part V, then 28 and 29.
    pub const L27B_OTHER: &str = "f1_40[0]";
    pub const L28_TOTAL_EXPENSES: &str = "f1_41[0]";
    pub const L29_TENTATIVE: &str = "f1_42[0]";
    /// 30, business use of the home, and 31, the figure that leaves the form.
    pub const L30_HOME: &str = "f1_45[0]";
    pub const L31_NET: &str = "f1_46[0]";

    /// Part III derived: 40 and 42.
    pub const L40_TOTAL: &str = "f2_6[0]";
    pub const L42_COST_OF_GOODS: &str = "f2_8[0]";

    /// Part V: nine printed rows of description and amount, then the total.
    pub const PART_V_ROWS: [[&str; 2]; 9] = [
        ["f2_15[0]", "f2_16[0]"],
        ["f2_17[0]", "f2_18[0]"],
        ["f2_19[0]", "f2_20[0]"],
        ["f2_21[0]", "f2_22[0]"],
        ["f2_23[0]", "f2_24[0]"],
        ["f2_25[0]", "f2_26[0]"],
        ["f2_27[0]", "f2_28[0]"],
        ["f2_29[0]", "f2_30[0]"],
        ["f2_31[0]", "f2_32[0]"],
    ];
    pub const L48_TOTAL: &str = "f2_33[0]";
}

/// What a caller has to supply that the books cannot answer.
pub struct ScheduleCRequest<'a> {
    pub year: i32,
    /// The business — name, address, EIN, business code. Shared with the
    /// partnership return, because a sole proprietorship's books describe one
    /// business exactly as a partnership's do.
    pub profile: &'a BusinessProfile,
    /// The owner. `None` produces a form with the header blank and says so.
    pub proprietor: Option<&'a SoleProprietor>,
    /// The owner's SSN, from the local table. `None` leaves the box empty,
    /// which is the visible incompleteness the design intends.
    pub ssn: Option<&'a str>,
    pub answers: &'a ScheduleC,
    /// Line 30, business use of the home, in whole dollars. Not derivable from
    /// a ledger — it comes off Form 8829 or the simplified-method worksheet — so
    /// it is supplied, and its absence is reported rather than assumed to be nil.
    pub home_office_dollars: Option<i64>,
}

/// A finished Schedule C.
pub struct Bundle {
    pub pdf: Vec<u8>,
    pub warnings: Vec<String>,
    /// Line 31 — the figure that goes to Form 1040 Schedule 1 and Schedule SE.
    pub net_profit_dollars: i64,
}

/// Build a Schedule C from the books.
pub fn build(req: &ScheduleCRequest<'_>, computed: &Computed) -> Result<Bundle, FormError> {
    let mut warnings = computed.warnings.clone();
    let lines = &computed.lines;

    let mut doc = Document::load_mem(F1040SC)?;
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    if req.year != FORM_TAX_YEAR {
        warnings.push(format!(
            "The bundled form is the {FORM_TAX_YEAR} revision and this is a {} return. The line              numbering moves between years — check every figure against the {} form before              filing.",
            req.year, req.year
        ));
    }

    // --- the header ---
    match req.proprietor {
        Some(p) => {
            set_text(&mut doc, &map, field::PROPRIETOR, &p.name)?;
            match p.accounting_method {
                AccountingMethod::Cash => set_check(&mut doc, &map, field::METHOD_CASH, "1")?,
                AccountingMethod::Accrual => {
                    set_check(&mut doc, &map, field::METHOD_ACCRUAL, "2")?
                }
                AccountingMethod::Other => {
                    set_check(&mut doc, &map, field::METHOD_OTHER_BOX, "3")?;
                    set_text(&mut doc, &map, field::METHOD_OTHER, p.method_description())?;
                }
            }
        }
        None => warnings.push(
            "No sole proprietor is recorded, so the name at the top of the form is blank.              Schedule C is attached to that person's Form 1040 and the IRS pairs the two by name              and number — a form with neither identifies nobody."
                .to_string(),
        ),
    }

    match req.ssn {
        Some(ssn) => set_text(&mut doc, &map, field::SSN, ssn)?,
        None => warnings.push(
            "No social security number is held on this machine, so the number box is blank. It              is deliberately not in the event log — see the sole proprietor page — so it has to              be entered on the machine the return is prepared on."
                .to_string(),
        ),
    }

    set_text(&mut doc, &map, field::BUSINESS_NAME, &req.profile.legal_name)?;
    set_text(&mut doc, &map, field::BUSINESS_CODE, &req.profile.naics_code)?;
    // The EIN box is a nine-character comb — one digit per cell — so the hyphen
    // an EIN is written with does not fit and would be refused outright. Digits
    // only, which is what the comb is drawn for.
    let ein_digits: String = req.profile.ein.chars().filter(|c| c.is_ascii_digit()).collect();
    set_text(&mut doc, &map, field::EIN, &ein_digits)?;
    set_text(&mut doc, &map, field::STREET, &address_line(req.profile))?;
    set_text(&mut doc, &map, field::CITY_STATE_ZIP, &city_line(req.profile))?;
    if let Some(activity) = &req.profile.principal_activity {
        set_text(&mut doc, &map, field::PRINCIPAL_BUSINESS, activity)?;
    }

    // --- the questions ---
    for q in QUESTIONS {
        let Some(answer) = req.answers.get(q.key) else {
            continue;
        };
        match q.control {
            Control::YesNo {
                yes,
                no,
                yes_on,
                no_on,
            } => match answer {
                YES => set_check(&mut doc, &map, yes, yes_on)?,
                NO => set_check(&mut doc, &map, no, no_on)?,
                _ => {}
            },
            Control::Check { on, on_state } => {
                if answer == YES {
                    set_check(&mut doc, &map, on, on_state)?;
                }
            }
        }
    }

    // --- the mapped lines ---
    for def in SCHEDULE_C_LINES {
        if !lines.is_mapped(def.key) {
            continue;
        }
        if let Field::One(name) = def.field {
            set_text(&mut doc, &map, name, &format_dollars(lines.get(def.key)))?;
        }
    }

    // --- Part V, itemised from the accounts mapped to line 48 ---
    let other = computed.detail.get("sc48").cloned().unwrap_or_default();
    for (row, item) in other.iter().take(field::PART_V_ROWS.len()).enumerate() {
        let cols = field::PART_V_ROWS[row];
        set_text(&mut doc, &map, cols[0], &item.account_name)?;
        set_text(
            &mut doc,
            &map,
            cols[1],
            &format_dollars(cents_to_dollars(item.cents)),
        )?;
    }
    if other.len() > field::PART_V_ROWS.len() {
        warnings.push(format!(
            "Part V has {} printed rows and {} accounts are mapped to line 48. The first {} are              listed; the rest need a continuation statement, which this program does not produce.              The total on line 48 includes all of them.",
            field::PART_V_ROWS.len(),
            other.len(),
            field::PART_V_ROWS.len()
        ));
    }
    let other_expenses = lines.get("sc48");

    // --- the derived boxes ---
    let home_office = req.home_office_dollars.unwrap_or(0);
    let net = lines.line_31(other_expenses, home_office);

    for (name, value) in [
        (field::L3_BALANCE, lines.line_3()),
        (field::L5_GROSS_PROFIT, lines.line_5()),
        (field::L7_GROSS_INCOME, lines.line_7()),
        (field::L28_TOTAL_EXPENSES, lines.line_28(other_expenses)),
        (field::L29_TENTATIVE, lines.line_29(other_expenses)),
        (field::L31_NET, net),
    ] {
        set_text(&mut doc, &map, name, &format_dollars(value))?;
    }
    if other_expenses != 0 {
        set_text(&mut doc, &map, field::L27B_OTHER, &format_dollars(other_expenses))?;
        set_text(&mut doc, &map, field::L48_TOTAL, &format_dollars(other_expenses))?;
    }
    if home_office != 0 {
        set_text(&mut doc, &map, field::L30_HOME, &format_dollars(home_office))?;
    }

    // Part III prints only when there is a Part III. A business that sells
    // services and holds no stock should get a blank page, not a column of
    // zeroes that reads as an answered question.
    if lines.has_cost_of_goods() {
        set_text(&mut doc, &map, field::L40_TOTAL, &format_dollars(lines.line_40()))?;
        let cogs = lines.cost_of_goods_sold();
        set_text(&mut doc, &map, field::L42_COST_OF_GOODS, &format_dollars(cogs))?;
        set_text(&mut doc, &map, field::L4_COST_OF_GOODS, &format_dollars(cogs))?;
    }

    // --- what the books *can* answer, and the form asks anyway ---
    //
    // The date the business started has no box on Schedule C, which made it look
    // like a Form 1065 field a sole proprietor was being asked for out of habit.
    // It is not: question H asks whether the business started or was acquired
    // during the year, and the business details already know. Checked rather
    // than filled in, because "started" and "acquired" are not the same event and
    // only the filer knows which this was.
    let started_this_year = req.profile.formation_date.year() == req.year;
    match (started_this_year, req.answers.get("h")) {
        (true, Some(YES)) | (false, None) | (false, Some(NO)) => {}
        (true, _) => warnings.push(format!(
            "The business details say this business started on {}, and question H asks whether \
             it started or was acquired during {}. H is not ticked. Tick it, or correct the date \
             on the Settings page — they cannot both be right.",
            req.profile.formation_date, req.year
        )),
        (false, Some(YES)) => warnings.push(format!(
            "Question H says the business started or was acquired during {}, but the business \
             details give {} as the date it started. One of the two is wrong.",
            req.year, req.profile.formation_date
        )),
        (false, Some(_)) => {}
    }

    // --- what the books cannot answer ---
    if req.home_office_dollars.is_none() {
        warnings.push(
            "Line 30, business use of the home, is blank. It is not in the books — it comes off              Form 8829, or the simplified-method worksheet in the instructions — so it has to be              worked out and entered. If no part of a home is used for the business, that is              correct as it stands."
                .to_string(),
        );
    }
    if lines.is_mapped("sc9") {
        warnings.push(
            "Line 9 carries car and truck expenses, so Part IV has to be completed — the date              the vehicle went into service, the miles split between business, commuting and              other, and four questions about its use. None of that is in a ledger, so Part IV is              blank. If a Form 4562 is required for this business, Part IV is answered there              instead."
                .to_string(),
        );
    }
    // Wages mean employees, and employees mean an EIN. You cannot file a Form 941
    // or issue a W-2 without one, so a Schedule C reporting wages with line D
    // blank is describing a business that could not have paid them.
    if lines.is_mapped("sc26") && req.profile.ein.trim().is_empty() {
        warnings.push(
            "Line 26 reports wages, so this business has employees — and an employer must have \
             an EIN, because Forms 941 and W-2 are filed under it. Line D is blank. If the \
             business has an EIN, it belongs in the business details on the Settings page; if it \
             has none, it cannot have employees."
                .to_string(),
        );
    }
    if lines.is_mapped("sc24b") {
        warnings.push(
            "Line 24b is deductible meals, which is generally half of what was spent. Check the              account mapped to it carries the deductible half rather than the whole — the form              does not halve it."
                .to_string(),
        );
    }

    let pdf = {
        let mut buf = Vec::new();
        doc.save_to(&mut buf)?;
        buf
    };
    Ok(Bundle {
        pdf,
        warnings,
        net_profit_dollars: net,
    })
}

fn address_line(p: &BusinessProfile) -> String {
    match &p.address.suite {
        Some(s) if !s.trim().is_empty() => format!("{}, {}", p.address.street, s),
        _ => p.address.street.clone(),
    }
}

fn city_line(p: &BusinessProfile) -> String {
    format!(
        "{}, {} {}",
        p.address.city, p.address.state, p.address.postal_code
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(pairs: &[(&'static str, i64)]) -> ScheduleCLines {
        let mut l = ScheduleCLines::default();
        for (k, v) in pairs {
            l.set_for_test(k, *v);
        }
        l
    }

    /// Every key is unique, or two accounts mapped to "the same line" would land
    /// in different buckets depending on which definition was found first.
    #[test]
    fn every_line_key_is_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for d in SCHEDULE_C_LINES {
            assert!(seen.insert(d.key), "duplicate line key {}", d.key);
        }
    }

    /// The prefix is what keeps a Form 1065 mapping from silently landing on a
    /// same-numbered Schedule C line when a book changes what it files.
    #[test]
    fn every_key_is_prefixed_and_none_collides_with_a_1065_line() {
        for d in SCHEDULE_C_LINES {
            assert!(d.key.starts_with("sc"), "{} is not prefixed", d.key);
            assert!(
                super::super::lines::line_def(d.key).is_none(),
                "{} is also a Form 1065 line key",
                d.key
            );
        }
    }

    /// Derived lines must not be mappable: an account on line 3 would be counted
    /// once in line 1 and again in the subtraction.
    #[test]
    fn no_derived_line_is_in_the_catalogue() {
        for derived in ["sc3", "sc4", "sc5", "sc7", "sc28", "sc29", "sc31", "sc40", "sc42"] {
            assert!(line_def(derived).is_none(), "{derived} is mappable and derived");
        }
    }

    #[test]
    fn gross_profit_subtracts_returns_and_cost_of_goods() {
        let l = lines(&[("sc1", 100_000), ("sc2", 4_000), ("sc35", 10_000), ("sc36", 30_000), ("sc41", 12_000)]);
        assert_eq!(l.line_3(), 96_000);
        assert_eq!(l.line_40(), 40_000);
        assert_eq!(l.cost_of_goods_sold(), 28_000);
        assert_eq!(l.line_5(), 68_000);
    }

    /// A business that holds no stock gets a blank Part III, not a zero that
    /// looks like somebody answered it.
    #[test]
    fn a_business_with_no_inventory_has_no_cost_of_goods_at_all() {
        let l = lines(&[("sc1", 100_000)]);
        assert!(!l.has_cost_of_goods());
        assert_eq!(l.cost_of_goods_sold(), 0);
        assert_eq!(l.line_5(), 100_000, "gross profit is just the receipts");
    }

    /// Closing inventory is a contra line: it prints positive and reduces the
    /// cost of what was sold, because what is still on the shelf was not sold.
    #[test]
    fn closing_inventory_reduces_cost_of_goods_sold() {
        let with = lines(&[("sc35", 10_000), ("sc36", 30_000), ("sc41", 12_000)]);
        let without = lines(&[("sc35", 10_000), ("sc36", 30_000)]);
        assert_eq!(with.cost_of_goods_sold(), 28_000);
        assert_eq!(without.cost_of_goods_sold(), 40_000);
    }

    #[test]
    fn the_net_figure_walks_down_the_form() {
        let l = lines(&[
            ("sc1", 200_000), ("sc2", 5_000), ("sc6", 1_000),
            ("sc8", 3_000), ("sc20b", 24_000), ("sc25", 6_000), ("sc26", 40_000),
        ]);
        assert_eq!(l.line_3(), 195_000);
        assert_eq!(l.line_7(), 196_000);
        assert_eq!(l.line_28(2_000), 75_000, "73,000 of Part II plus 2,000 of other");
        assert_eq!(l.line_29(2_000), 121_000);
        assert_eq!(l.line_31(2_000, 5_000), 116_000, "less the home office");
    }

    /// Schedule C has no equivalent of Schedule K, and offering one would invite
    /// a mapping to a line this form does not have.
    #[test]
    fn there_is_no_separately_stated_section_on_this_form() {
        for absent in ["charitable", "section_179", "investment_interest"] {
            assert!(
                !SCHEDULE_C_LINES.iter().any(|d| d.key.contains(absent)),
                "{absent} has no place on a Schedule C"
            );
        }
        // §179 is on line 13 with depreciation, not stated separately.
        let l13 = line_def("sc13").unwrap();
        assert!(l13.label.contains("section 179"), "{}", l13.label);
    }

    // --- against the vendored form ---

    use crate::domain::Address;
    use crate::tax::acroform::{get_value, on_states};
    use chrono::NaiveDate;

    fn profile() -> BusinessProfile {
        BusinessProfile {
            legal_name: "Bunny Ears Art House".into(),
            address: Address {
                street: "1808 W Summerdale Ave".into(),
                suite: None,
                city: "Chicago".into(),
                state: "IL".into(),
                postal_code: "60640".into(),
                country: None,
            },
            ein: "12-3456789".into(),
            naics_code: "611610".into(),
            formation_date: NaiveDate::from_ymd_opt(2023, 4, 13).unwrap(),
            principal_activity: Some("Fine arts instruction".into()),
            principal_product: Some("Art classes".into()),
        }
    }

    fn proprietor() -> SoleProprietor {
        SoleProprietor {
            name: "Jinny Choi".into(),
            accounting_method: AccountingMethod::Cash,
            accounting_method_other: None,
        }
    }

    fn computed(pairs: &[(&'static str, i64)]) -> Computed {
        Computed {
            lines: lines(pairs),
            detail: BTreeMap::new(),
            warnings: Vec::new(),
        }
    }

    fn built(c: &Computed, p: Option<&SoleProprietor>, ssn: Option<&str>) -> (Document, Vec<String>) {
        let answers = ScheduleC::default();
        let profile = profile();
        let req = ScheduleCRequest {
            year: FORM_TAX_YEAR,
            profile: &profile,
            proprietor: p,
            ssn,
            answers: &answers,
            home_office_dollars: Some(0),
        };
        let bundle = build(&req, c).unwrap();
        (Document::load_mem(&bundle.pdf).unwrap(), bundle.warnings)
    }

    fn box_of(doc: &Document, name: &str) -> String {
        let map = field_map(doc);
        get_value(doc, &map, name).unwrap_or_default()
    }

    /// Every field this module names has to exist, or a revision has renumbered
    /// the form under us — the check every other form module carries.
    #[test]
    fn every_field_this_module_names_exists_in_the_vendored_form() {
        let mut doc = Document::load_mem(F1040SC).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);

        for name in [
            field::PROPRIETOR, field::SSN, field::PRINCIPAL_BUSINESS, field::BUSINESS_CODE,
            field::BUSINESS_NAME, field::EIN, field::STREET, field::CITY_STATE_ZIP,
            field::METHOD_OTHER, field::METHOD_CASH, field::METHOD_ACCRUAL,
            field::METHOD_OTHER_BOX, field::L3_BALANCE, field::L4_COST_OF_GOODS,
            field::L5_GROSS_PROFIT, field::L7_GROSS_INCOME, field::L27B_OTHER,
            field::L28_TOTAL_EXPENSES, field::L29_TENTATIVE, field::L30_HOME, field::L31_NET,
            field::L40_TOTAL, field::L42_COST_OF_GOODS, field::L48_TOTAL,
        ] {
            assert!(map.find(name).is_some(), "f1040sc.pdf has no field {name}");
        }
        for row in field::PART_V_ROWS {
            for f in row {
                assert!(map.find(f).is_some(), "f1040sc.pdf has no Part V field {f}");
            }
        }
        for def in SCHEDULE_C_LINES {
            if let Field::One(name) = def.field {
                assert!(map.find(name).is_some(), "line {} names missing field {name}", def.number);
            }
        }
        for q in QUESTIONS {
            match q.control {
                Control::YesNo { yes, no, .. } => {
                    assert!(map.find(yes).is_some(), "question {} yes box", q.number);
                    assert!(map.find(no).is_some(), "question {} no box", q.number);
                }
                Control::Check { on, .. } => {
                    assert!(map.find(on).is_some(), "question {} box", q.number);
                }
            }
        }
    }

    /// A checkbox whose on-state changed between revisions ticks nothing while
    /// looking like it worked, which no field-existence check can catch.
    #[test]
    fn the_checkbox_states_are_the_ones_the_form_was_built_with() {
        let mut doc = Document::load_mem(F1040SC).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);

        // Each question against the state recorded beside it. The form is not
        // consistent — G, I and J tick with "Yes"/"No" and 32 and 34 with
        // "1"/"2" — which is exactly why the value is carried per question
        // rather than assumed once.
        for q in QUESTIONS {
            match q.control {
                Control::YesNo {
                    yes,
                    no,
                    yes_on,
                    no_on,
                } => {
                    assert_eq!(on_states(&doc, &map, yes), vec![yes_on], "{} yes", q.number);
                    assert_eq!(on_states(&doc, &map, no), vec![no_on], "{} no", q.number);
                }
                Control::Check { on, on_state } => {
                    assert_eq!(on_states(&doc, &map, on), vec![on_state], "{}", q.number);
                }
            }
        }
        assert_eq!(on_states(&doc, &map, field::METHOD_CASH), vec!["1"]);
        assert_eq!(on_states(&doc, &map, field::METHOD_ACCRUAL), vec!["2"]);
        assert_eq!(on_states(&doc, &map, field::METHOD_OTHER_BOX), vec!["3"]);
    }

    /// The header comes from two places: the person from the proprietor record,
    /// the business from the profile the partnership return already uses.
    #[test]
    fn the_header_takes_the_person_and_the_business_from_their_own_records() {
        let c = computed(&[("sc1", 100_000)]);
        let (doc, _) = built(&c, Some(&proprietor()), Some("123-45-6789"));

        assert_eq!(box_of(&doc, field::PROPRIETOR), "Jinny Choi");
        assert_eq!(box_of(&doc, field::SSN), "123-45-6789");
        assert_eq!(box_of(&doc, field::BUSINESS_NAME), "Bunny Ears Art House");
        assert_eq!(box_of(&doc, field::BUSINESS_CODE), "611610");
        assert_eq!(box_of(&doc, field::EIN), "123456789", "the comb takes digits only");
        assert_eq!(box_of(&doc, field::PRINCIPAL_BUSINESS), "Fine arts instruction");
        assert_eq!(box_of(&doc, field::CITY_STATE_ZIP), "Chicago, IL 60640");
    }

    /// The number box is left visibly empty rather than filled from anywhere
    /// else, and the form says why.
    #[test]
    fn a_missing_ssn_leaves_the_box_blank_and_says_so() {
        let c = computed(&[("sc1", 100_000)]);
        let (doc, warnings) = built(&c, Some(&proprietor()), None);

        assert_eq!(box_of(&doc, field::SSN), "");
        assert!(
            warnings.iter().any(|w| w.contains("not in the event log")),
            "{warnings:?}"
        );
    }

    #[test]
    fn no_proprietor_is_reported_rather_than_producing_an_anonymous_form() {
        let c = computed(&[("sc1", 100_000)]);
        let (_, warnings) = built(&c, None, None);
        assert!(
            warnings.iter().any(|w| w.contains("identifies nobody")),
            "{warnings:?}"
        );
    }

    /// The arithmetic that walks down the page has to reach the boxes, because
    /// line 31 is the only figure anybody carries off this form.
    #[test]
    fn the_derived_boxes_carry_the_arithmetic_down_to_line_31() {
        let c = computed(&[
            ("sc1", 200_000), ("sc2", 5_000), ("sc6", 1_000),
            ("sc8", 3_000), ("sc20b", 24_000), ("sc26", 40_000),
        ]);
        let answers = ScheduleC::default();
        let profile = profile();
        let p = proprietor();
        let req = ScheduleCRequest {
            year: FORM_TAX_YEAR,
            profile: &profile,
            proprietor: Some(&p),
            ssn: Some("123-45-6789"),
            answers: &answers,
            home_office_dollars: Some(5_000),
        };
        let bundle = build(&req, &c).unwrap();
        let doc = Document::load_mem(&bundle.pdf).unwrap();

        assert_eq!(box_of(&doc, field::L3_BALANCE), "195,000");
        assert_eq!(box_of(&doc, field::L7_GROSS_INCOME), "196,000");
        assert_eq!(box_of(&doc, field::L28_TOTAL_EXPENSES), "67,000");
        assert_eq!(box_of(&doc, field::L29_TENTATIVE), "129,000");
        assert_eq!(box_of(&doc, field::L30_HOME), "5,000");
        assert_eq!(box_of(&doc, field::L31_NET), "124,000");
        assert_eq!(bundle.net_profit_dollars, 124_000);
    }

    /// A business that holds no stock gets a blank Part III rather than a
    /// column of zeroes that reads as an answered question.
    #[test]
    fn part_three_is_blank_when_nothing_is_mapped_to_it() {
        let c = computed(&[("sc1", 100_000)]);
        let (doc, _) = built(&c, Some(&proprietor()), None);
        assert_eq!(box_of(&doc, field::L42_COST_OF_GOODS), "");
        assert_eq!(box_of(&doc, field::L4_COST_OF_GOODS), "");

        let c = computed(&[("sc1", 100_000), ("sc35", 10_000), ("sc36", 30_000), ("sc41", 12_000)]);
        let (doc, _) = built(&c, Some(&proprietor()), None);
        assert_eq!(box_of(&doc, field::L40_TOTAL), "40,000");
        assert_eq!(box_of(&doc, field::L42_COST_OF_GOODS), "28,000");
        assert_eq!(box_of(&doc, field::L4_COST_OF_GOODS), "28,000", "line 42 carries to line 4");
    }

    /// Part V writes itself from the accounts mapped to line 48, and the total
    /// carries to line 27b.
    #[test]
    fn part_five_itemises_the_accounts_mapped_to_line_48() {
        let mut c = computed(&[("sc1", 100_000), ("sc48", 1_500)]);
        c.detail.insert(
            "sc48",
            vec![
                super::super::lines::LineDetail {
                    account_id: "a".into(),
                    account_number: "6900".into(),
                    account_name: "Bank charges".into(),
                    cents: 100_000,
                },
                super::super::lines::LineDetail {
                    account_id: "b".into(),
                    account_number: "6910".into(),
                    account_name: "Dues and subscriptions".into(),
                    cents: 50_000,
                },
            ],
        );
        let (doc, _) = built(&c, Some(&proprietor()), None);

        assert_eq!(box_of(&doc, field::PART_V_ROWS[0][0]), "Bank charges");
        assert_eq!(box_of(&doc, field::PART_V_ROWS[0][1]), "1,000");
        assert_eq!(box_of(&doc, field::PART_V_ROWS[1][0]), "Dues and subscriptions");
        assert_eq!(box_of(&doc, field::L48_TOTAL), "1,500");
        assert_eq!(box_of(&doc, field::L27B_OTHER), "1,500", "and on to line 27b");
    }

    #[test]
    fn more_other_expenses_than_printed_rows_are_reported() {
        let mut c = computed(&[("sc48", 10_000)]);
        c.detail.insert(
            "sc48",
            (0..12)
                .map(|i| super::super::lines::LineDetail {
                    account_id: format!("a{i}"),
                    account_number: format!("69{i:02}"),
                    account_name: format!("Expense {i}"),
                    cents: 100_000,
                })
                .collect(),
        );
        let (_, warnings) = built(&c, Some(&proprietor()), None);
        assert!(
            warnings.iter().any(|w| w.contains("continuation statement")),
            "{warnings:?}"
        );
    }

    /// The three things this form asks that a ledger cannot answer.
    #[test]
    fn the_parts_a_ledger_cannot_fill_are_reported_rather_than_left_silent() {
        let c = computed(&[("sc1", 100_000), ("sc9", 4_000), ("sc24b", 800)]);
        let answers = ScheduleC::default();
        let profile = profile();
        let p = proprietor();
        let req = ScheduleCRequest {
            year: FORM_TAX_YEAR,
            profile: &profile,
            proprietor: Some(&p),
            ssn: Some("123-45-6789"),
            answers: &answers,
            // Not supplied, which is the case worth reporting.
            home_office_dollars: None,
        };
        let warnings = build(&req, &c).unwrap().warnings;

        assert!(warnings.iter().any(|w| w.contains("Form 8829")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("Part IV")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("deductible half")), "{warnings:?}");
    }

    /// The accounting method ticks one box, and line F(3) is written only for
    /// the box that asks for it.
    #[test]
    fn the_accounting_method_ticks_its_own_box() {
        let c = computed(&[("sc1", 100_000)]);

        let mut p = proprietor();
        p.accounting_method = AccountingMethod::Accrual;
        let (doc, _) = built(&c, Some(&p), None);
        // A ticked box reads back as a PDF name, so it carries its leading slash.
        assert_eq!(box_of(&doc, field::METHOD_ACCRUAL), "/2");
        assert_eq!(box_of(&doc, field::METHOD_OTHER), "");

        p.accounting_method = AccountingMethod::Other;
        p.accounting_method_other = Some("Hybrid".into());
        let (doc, _) = built(&c, Some(&p), None);
        assert_eq!(box_of(&doc, field::METHOD_OTHER_BOX), "/3");
        assert_eq!(box_of(&doc, field::METHOD_OTHER), "Hybrid");
    }

    /// A year the vendored form is not the revision for is reported, because the
    /// line numbering moves.
    #[test]
    fn a_return_for_another_year_says_the_form_is_the_wrong_revision() {
        let c = computed(&[("sc1", 100_000)]);
        let answers = ScheduleC::default();
        let profile = profile();
        let p = proprietor();
        let req = ScheduleCRequest {
            year: FORM_TAX_YEAR + 1,
            profile: &profile,
            proprietor: Some(&p),
            ssn: None,
            answers: &answers,
            home_office_dollars: Some(0),
        };
        let warnings = build(&req, &c).unwrap().warnings;
        assert!(warnings.iter().any(|w| w.contains("revision")), "{warnings:?}");
    }

    /// The date the business started has no box on this form, which made it look
    /// like a Form 1065 field a sole proprietor was asked for out of habit. It
    /// is not — question H asks the same thing, and the two have to agree.
    #[test]
    fn the_date_the_business_started_is_checked_against_question_h() {
        let c = computed(&[("sc1", 100_000)]);
        let p = proprietor();
        let profile = profile(); // started 13 April 2023

        let build_for = |year: i32, h: Option<&str>| {
            let mut answers = ScheduleC::default();
            if let Some(v) = h {
                answers.set("h", v);
            }
            let req = ScheduleCRequest {
                year,
                profile: &profile,
                proprietor: Some(&p),
                ssn: None,
                answers: &answers,
                home_office_dollars: Some(0),
            };
            build(&req, &c).unwrap().warnings
        };

        // Filing 2023, the year it started, with H unticked.
        let w = build_for(2023, None);
        assert!(w.iter().any(|w| w.contains("H is not ticked")), "{w:?}");

        // Same year, H ticked — nothing to say.
        let w = build_for(2023, Some(YES));
        assert!(!w.iter().any(|w| w.contains("question H")), "{w:?}");

        // A later year with H ticked contradicts the date.
        let w = build_for(2025, Some(YES));
        assert!(w.iter().any(|w| w.contains("One of the two is wrong")), "{w:?}");

        // A later year, H unanswered or No — the ordinary case, silent.
        for h in [None, Some(NO)] {
            let w = build_for(2025, h);
            assert!(!w.iter().any(|x| x.contains("question H")), "{h:?}: {w:?}");
        }
    }

    /// An EIN a sole proprietor has is reported on line D — it is not the
    /// partnership's box, and the digits go in without the hyphen because the box
    /// is a nine-character comb.
    #[test]
    fn a_sole_proprietors_ein_is_reported_on_line_d() {
        let c = computed(&[("sc1", 100_000)]);
        let (doc, warnings) = built(&c, Some(&proprietor()), Some("123-45-6789"));

        assert_eq!(box_of(&doc, field::EIN), "123456789");
        // The SSN is the owner's and belongs at the top, never on line D.
        assert_eq!(box_of(&doc, field::SSN), "123-45-6789");
        assert_ne!(box_of(&doc, field::EIN), box_of(&doc, field::SSN));
        assert!(!warnings.iter().any(|w| w.contains("Line 26")), "{warnings:?}");
    }

    /// Wages mean employees, and an employer must have an EIN — Forms 941 and
    /// W-2 are filed under it. A Schedule C reporting wages with line D blank
    /// describes a business that could not have paid them.
    #[test]
    fn wages_with_no_ein_are_reported() {
        let c = computed(&[("sc1", 100_000), ("sc26", 40_000)]);
        let answers = ScheduleC::default();
        let mut profile = profile();
        profile.ein = String::new();
        let p = proprietor();
        let req = ScheduleCRequest {
            year: FORM_TAX_YEAR,
            profile: &profile,
            proprietor: Some(&p),
            ssn: Some("123-45-6789"),
            answers: &answers,
            home_office_dollars: Some(0),
        };
        let warnings = build(&req, &c).unwrap().warnings;
        assert!(
            warnings.iter().any(|w| w.contains("must have") && w.contains("EIN")),
            "{warnings:?}"
        );

        // With an EIN, nothing to say — and no wages, nothing to say either.
        let (_, w) = built(&c, Some(&p), None);
        assert!(!w.iter().any(|x| x.contains("Line 26")), "{w:?}");
    }

    #[test]
    fn every_question_key_is_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for q in QUESTIONS {
            assert!(seen.insert(q.key), "duplicate question key {}", q.key);
        }
    }
}

