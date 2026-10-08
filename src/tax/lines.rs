//! Which Form 1065 line each ledger account is reported on, and what the lines
//! then add up to.
//!
//! # Why the mapping is keyed by account
//!
//! The ingest mappings ([`crate::commands::ingest_commands::MAPPING_DEFS`]) go
//! key → account, because there is exactly one Square balance account. A return
//! is the other shape: a chart of accounts has a dozen expense accounts that all
//! land on line 21, and one row per line could only ever name one of them.
//!
//! Keying by account also makes the question that matters answerable: *which
//! accounts have no line?* An account carrying a balance and no mapping is
//! income or expense that silently vanishes off the return — the return still
//! foots, it is just wrong — so [`compute`] enumerates them and says so.
//!
//! # Why the derived lines are computed and not mapped
//!
//! Lines 1c, 3, 8, 16c, 22 and 23 are arithmetic on the lines above them. Left
//! mappable, somebody eventually points an account at line 23 and the return
//! shows an ordinary business income that is not what lines 8 and 22 make it.
//! That return is worse than one with no figures at all, because it looks
//! finished and its own internal check passes only by not being made. So the
//! totals here are computed, always, from the same rounded figures the form
//! shows.
//!
//! # Rounding
//!
//! The ledger holds integer cents; the IRS permits whole dollars provided every
//! amount is rounded. Rounding happens **once per line**, after that line's
//! accounts are summed in cents — not per account, which would accumulate a
//! cent of error for every account in the chart — and every derived line is
//! computed from the already-rounded dollars. That ordering is what makes the
//! arithmetic on the printed page come out exactly: see
//! [`tests::the_totals_on_the_page_are_the_arithmetic_of_the_lines_on_the_page`].

use super::ReturnForm;
use crate::queries::reports::IncomeStatement;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

/// Whether a line reports its accounts as they stand, or as a figure subtracted
/// from the line above it.
///
/// The form's "less …" lines — 1b returns and allowances, 16b depreciation
/// reported elsewhere — print a *positive* number that is then taken away. The
/// accounts behind them are contra accounts, which carry the opposite sign to
/// their type, so their oriented balance is negative. Negating at the line makes
/// the box read the way the form means it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sense {
    /// The line shows the sum of its accounts as oriented.
    Natural,
    /// The line shows the sum negated — a "less …" line.
    Contra,
}

/// Which part of the return a line belongs to.
///
/// # Why one catalogue covers all three
///
/// An account reaches exactly one line, and that stays true across the
/// schedules because the sets are disjoint by construction:
///
/// - A profit-and-loss account is either an *ordinary* item, which nets into
///   page one's line 23, or a **separately stated** item, which bypasses page
///   one entirely and goes straight to Schedule K. Tax law makes those mutually
///   exclusive: charitable contributions, section 179, investment interest and
///   capital gains are deducted or reported by each partner on their own return,
///   at their own rates, so a partnership that also put them in page-one line 21
///   would deduct them twice.
/// - A balance-sheet account is neither. It has no place on page one or Schedule
///   K at all, and belongs to Schedule L.
///
/// So a single `account → line` mapping cannot double-count, and there is one
/// picker rather than three. The alternative — a separate mapping per schedule —
/// buys nothing and makes the double-deduction above expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Schedule {
    /// Page one: the ordinary trade-or-business computation ending at line 23.
    Page1,
    /// Schedule K: partners' distributive share items, separately stated.
    K,
    /// Schedule L: balance sheets per books.
    L,
}

impl Schedule {
    pub fn title(self) -> &'static str {
        match self {
            Schedule::Page1 => "Form 1065, page 1",
            Schedule::K => "Schedule K — partners' distributive share items",
            Schedule::L => "Schedule L — balance sheets per books",
        }
    }

    /// Whether this schedule is totalled from the income statement.
    ///
    /// Schedule L is not: a balance sheet is a position on two dates, not a
    /// period's activity, and feeding it through [`compute`] would report a
    /// year's *movement* in cash as the cash on hand.
    pub fn from_income_statement(self) -> bool {
        matches!(self, Schedule::Page1 | Schedule::K)
    }
}

/// Where a line's figure is written on the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// One box. Page one and Schedule K report a single period.
    One(&'static str),
    /// Two boxes: the balance at the start of the year and at the end.
    /// Schedule L reports both, side by side, and a return whose opening column
    /// does not match last year's closing one is the first thing an examiner
    /// looks at.
    Period {
        begin: &'static str,
        end: &'static str,
    },
}

/// A Form 1065 line that accounts can be pointed at.
///
/// Comparable because a picker holds one as its selected value and has to
/// recognise it again. Every field is `Copy` and the whole table is `const`, so
/// equality is identity here: two defs are equal exactly when they are the same
/// entry of [`MAPPABLE_LINES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaxLineDef {
    /// Stored in `tax_line_mappings.line_key`.
    pub key: &'static str,
    /// The line as the form numbers it, e.g. "1a", "16b".
    pub number: &'static str,
    /// The form's own wording, for a mapping editor to show.
    pub label: &'static str,
    /// The block the form prints this line under — "Income", "Deductions",
    /// "Assets", and so on. Used to group a picker the way the page reads.
    pub group: &'static str,
    pub schedule: Schedule,
    pub field: Field,
    pub sense: Sense,
    /// What the Instructions for Form 1065 say belongs on this line, condensed
    /// to what somebody deciding where an account goes actually needs.
    ///
    /// Carried here rather than left to the reader's memory because the whole
    /// failure this module guards against — money reaching the wrong line, or no
    /// line — is a failure of knowing what a line is *for*. "Other deductions"
    /// and "Other income" in particular are where everything ends up when nobody
    /// is sure, and both have real exclusions.
    pub instructions: &'static str,
    /// A form or statement this line obliges when it carries a figure.
    pub attachment: Option<Attachment>,
}

/// Something that has to travel with the return when a line is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attachment {
    pub name: &'static str,
    pub url: &'static str,
    /// True when this program produces it and appends it to the return.
    pub generated: bool,
}

/// Every line an account may be mapped to.
///
/// The single source of truth, shared by the mapping editor and by [`compute`].
/// Derived lines are deliberately absent — see the module docs.
// Attachments the form names beside a line.
const FORM_1125A: Attachment = Attachment {
    name: "Form 1125-A",
    url: "https://www.irs.gov/forms-pubs/about-form-1125-a",
    generated: true,
};
const FORM_4797: Attachment = Attachment {
    name: "Form 4797",
    url: "https://www.irs.gov/forms-pubs/about-form-4797",
    generated: false,
};
const FORM_4562: Attachment = Attachment {
    name: "Form 4562",
    url: "https://www.irs.gov/forms-pubs/about-form-4562",
    generated: false,
};
const FORM_7205: Attachment = Attachment {
    name: "Form 7205",
    url: "https://www.irs.gov/forms-pubs/about-form-7205",
    generated: false,
};
const FORM_8825: Attachment = Attachment {
    name: "Form 8825",
    url: "https://www.irs.gov/forms-pubs/about-form-8825",
    generated: false,
};
const SCHEDULE_D: Attachment = Attachment {
    name: "Schedule D (Form 1065)",
    url: "https://www.irs.gov/forms-pubs/about-schedule-d-form-1065",
    generated: false,
};
/// The one attachment this program produces itself — see [`crate::tax::statement`].
const LINE_21_STATEMENT: Attachment = Attachment {
    name: "Other deductions statement",
    url: "https://www.irs.gov/instructions/i1065",
    generated: true,
};
const OTHER_INCOME_STATEMENT: Attachment = Attachment {
    name: "Other income statement",
    url: "https://www.irs.gov/instructions/i1065",
    generated: true,
};
/// Line 18c's, which is produced twice over: once for the partnership's own
/// Schedule K box, and once per partner for the share on their K-1.
///
/// The per-partner half is the one the form actually demands. Box 18 code C and
/// item L row 4 both reach a partner as a single unexplained figure, and item L
/// row 4 is a row the K-1 instructions tell you to attach an explanation for.
/// See [`crate::tax::nondeductible`].
pub const NONDEDUCTIBLE_STATEMENT: Attachment = Attachment {
    name: "Nondeductible expenses statement",
    url: "https://www.irs.gov/instructions/i1065",
    generated: true,
};

/// The Instructions for Form 1065 themselves — the document every `instructions`
/// string on this table is condensed from, and where a preparer goes when the
/// condensation is not enough.
pub const FORM_1065_INSTRUCTIONS: Attachment = Attachment {
    name: "Instructions for Form 1065",
    url: "https://www.irs.gov/pub/irs-pdf/i1065.pdf",
    generated: false,
};

/// Every line an account may be mapped to, across page one, Schedule K and
/// Schedule L.
///
/// The single source of truth, shared by the mapping editor and by [`compute`].
/// Derived lines are deliberately absent — see the module docs.
pub const MAPPABLE_LINES: &[TaxLineDef] = &[
    // --- Page 1: income ---------------------------------------------------
    TaxLineDef { key: "l1a", number: "1a", label: "Gross receipts or sales", group: "Income", schedule: Schedule::Page1, field: Field::One("f1_19[0]"), sense: Sense::Natural,
        instructions: "Gross receipts or sales from all trade or business operations, before returns and allowances. Do not include rental income (Schedule K, line 2 or 3a), portfolio income such as interest, dividends or royalties (Schedule K, lines 5-7), or gains from selling business property (line 6).",
        attachment: None },
    TaxLineDef { key: "l1b", number: "1b", label: "Less returns and allowances", group: "Income", schedule: Schedule::Page1, field: Field::One("f1_20[0]"), sense: Sense::Contra,
        instructions: "Cash or credit refunds made to customers for returned goods, and allowances off the sale price. Enter as a positive figure; the form subtracts it from line 1a.",
        attachment: None },
    TaxLineDef { key: "l2", number: "2", label: "Cost of goods sold", group: "Income", schedule: Schedule::Page1, field: Field::One("f1_22[0]"), sense: Sense::Natural,
        instructions: "The total from Form 1125-A, line 8. A partnership carrying inventory, or producing or buying goods for resale, has to compute cost of goods sold on that form and bring the total here.",
        attachment: Some(FORM_1125A) },
    TaxLineDef { key: "l4", number: "4", label: "Ordinary income (loss) from other partnerships, estates, and trusts", group: "Income", schedule: Schedule::Page1, field: Field::One("f1_24[0]"), sense: Sense::Natural,
        instructions: "Ordinary income or loss shown on a Schedule K-1 this partnership received from another partnership, or on a Schedule K-1 from an estate or trust. Do not include portfolio or other separately stated items from that K-1 — those belong on the matching Schedule K line here.",
        attachment: None },
    TaxLineDef { key: "l5", number: "5", label: "Net farm profit (loss)", group: "Income", schedule: Schedule::Page1, field: Field::One("f1_25[0]"), sense: Sense::Natural,
        instructions: "Net profit or loss from farming, as computed on Schedule F (Form 1040).",
        attachment: None },
    TaxLineDef { key: "l6", number: "6", label: "Net gain (loss) from Form 4797, Part II, line 17", group: "Income", schedule: Schedule::Page1, field: Field::One("f1_26[0]"), sense: Sense::Natural,
        instructions: "Ordinary gain or loss from the sale or exchange of property used in the trade or business, from Form 4797, Part II, line 17. Capital gains do not belong here — they are separately stated on Schedule K, lines 8 and 9a.",
        attachment: Some(FORM_4797) },
    TaxLineDef { key: "l7", number: "7", label: "Other income (loss)", group: "Income", schedule: Schedule::Page1, field: Field::One("f1_27[0]"), sense: Sense::Natural,
        instructions: "Trade or business income that fits none of lines 1a through 6 — recoveries of bad debts deducted in an earlier year, section 481 adjustments, taxable interest actually earned in the trade or business. A statement itemising what makes up this line has to be attached.",
        attachment: Some(OTHER_INCOME_STATEMENT) },

    // --- Page 1: deductions -----------------------------------------------
    TaxLineDef { key: "l9", number: "9", label: "Salaries and wages (other than to partners)", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_29[0]"), sense: Sense::Natural,
        instructions: "Salaries and wages paid to employees, less any employment credits claimed. Payments to partners are never here: guaranteed payments go on line 10, and a partner's distributive share is not a wage at all.",
        attachment: None },
    TaxLineDef { key: "l10", number: "10", label: "Guaranteed payments to partners", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_30[0]"), sense: Sense::Natural,
        instructions: "Payments to a partner for services or for the use of capital that are determined without regard to partnership income. The same figures are separately stated on Schedule K, line 4, so each partner reports what they received.",
        attachment: None },
    TaxLineDef { key: "l11", number: "11", label: "Repairs and maintenance", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_31[0]"), sense: Sense::Natural,
        instructions: "Repairs and maintenance that neither add to the value of the property nor appreciably lengthen its life. Work that does either is a capital improvement and is depreciated instead.",
        attachment: None },
    TaxLineDef { key: "l12", number: "12", label: "Bad debts", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_32[0]"), sense: Sense::Natural,
        instructions: "Business debts that became worthless during the year, in whole or in part. A cash-basis partnership has no deduction for an unpaid receivable it never took into income.",
        attachment: None },
    TaxLineDef { key: "l13", number: "13", label: "Rent", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_33[0]"), sense: Sense::Natural,
        instructions: "Rent paid or incurred for business property. Rent for a partner's own property is included, provided it is reasonable.",
        attachment: None },
    TaxLineDef { key: "l14", number: "14", label: "Taxes and licenses", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_34[0]"), sense: Sense::Natural,
        instructions: "State and local taxes, payroll taxes, and licence fees paid or incurred in the trade or business. Federal income taxes are not deductible, and taxes assessed against local benefits that increase a property's value are capitalised.",
        attachment: None },
    TaxLineDef { key: "l15", number: "15", label: "Interest (see instructions)", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_35[0]"), sense: Sense::Natural,
        instructions: "Interest on debts incurred in the trade or business, subject to the section 163(j) limitation. Investment interest expense is not here — it is separately stated on Schedule K, line 13c, because the limit that applies to it is computed on each partner's own return.",
        attachment: None },
    TaxLineDef { key: "l16a", number: "16a", label: "Depreciation (if required, attach Form 4562)", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_36[0]"), sense: Sense::Natural,
        instructions: "Depreciation claimed on assets used in the trade or business, from Form 4562. The section 179 deduction is not here — it is separately stated on Schedule K, line 12, because each partner's dollar limit is their own.",
        attachment: Some(FORM_4562) },
    TaxLineDef { key: "l16b", number: "16b", label: "Less depreciation reported on Form 1125-A and elsewhere", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_37[0]"), sense: Sense::Contra,
        instructions: "The part of line 16a already deducted through cost of goods sold or claimed elsewhere on the return. Enter as a positive figure; the form subtracts it, and omitting it deducts the same depreciation twice.",
        attachment: None },
    TaxLineDef { key: "l17", number: "17", label: "Depletion (do not deduct oil and gas depletion)", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_39[0]"), sense: Sense::Natural,
        instructions: "Depletion other than on oil and gas. Oil and gas depletion is computed by each partner separately and must not be deducted here.",
        attachment: None },
    TaxLineDef { key: "l18", number: "18", label: "Retirement plans, etc.", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_40[0]"), sense: Sense::Natural,
        instructions: "Contributions to employee retirement plans — pension, profit-sharing, annuity, SEP and SIMPLE. Contributions on behalf of a partner are not deductible here; they are reported on Schedule K, line 13e, and taken on that partner's own return.",
        attachment: None },
    TaxLineDef { key: "l19", number: "19", label: "Employee benefit programs", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_41[0]"), sense: Sense::Natural,
        instructions: "Employee benefits not part of a retirement plan — health and accident insurance, life insurance, dependent care. Benefits for a partner are not here; they are guaranteed payments or Schedule K items.",
        attachment: None },
    TaxLineDef { key: "l20", number: "20", label: "Energy efficient commercial buildings deduction", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_42[0]"), sense: Sense::Natural,
        instructions: "The section 179D deduction for energy efficient commercial building property, computed on Form 7205.",
        attachment: Some(FORM_7205) },
    TaxLineDef { key: "l21", number: "21", label: "Other deductions", group: "Deductions", schedule: Schedule::Page1, field: Field::One("f1_43[0]"), sense: Sense::Natural,
        instructions: "Ordinary and necessary trade or business expenses that fit none of lines 9 through 20 — advertising, insurance, professional fees, office supplies, utilities, bank charges. A statement itemising what makes up this line has to be attached. Separately stated items never belong here: charitable contributions, section 179, investment interest and the like go to Schedule K instead.",
        attachment: Some(LINE_21_STATEMENT) },

    // --- Schedule K: income (loss) ----------------------------------------
    TaxLineDef { key: "k2", number: "2", label: "Net rental real estate income (loss)", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_02[0]"), sense: Sense::Natural,
        instructions: "Net income or loss from renting real estate, computed on Form 8825. Rental activity is passive to the partners regardless of what they do, which is why it is separately stated rather than netted into ordinary income.",
        attachment: Some(FORM_8825) },
    TaxLineDef { key: "k3a", number: "3a", label: "Other gross rental income (loss)", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_03[0]"), sense: Sense::Natural,
        instructions: "Gross income from rental activities other than real estate — equipment hire, vehicle hire.",
        attachment: None },
    TaxLineDef { key: "k3b", number: "3b", label: "Expenses from other rental activities", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_04[0]"), sense: Sense::Contra,
        instructions: "Expenses of the rental activities on line 3a. Enter as a positive figure; the form subtracts it to reach line 3c.",
        attachment: None },
    TaxLineDef { key: "k4a", number: "4a", label: "Guaranteed payments: services", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_06[0]"), sense: Sense::Natural,
        instructions: "Guaranteed payments to partners for services. The same amounts are deducted on page 1, line 10; here they are reported so each partner takes them into income.",
        attachment: None },
    TaxLineDef { key: "k4b", number: "4b", label: "Guaranteed payments: capital", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_07[0]"), sense: Sense::Natural,
        instructions: "Guaranteed payments to partners for the use of capital, determined without regard to partnership income.",
        attachment: None },
    TaxLineDef { key: "k5", number: "5", label: "Interest income", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_09[0]"), sense: Sense::Natural,
        instructions: "Portfolio interest — bank accounts, notes, bonds. Separately stated because it is investment income to the partners, not trade or business income. Interest genuinely earned in the trade or business goes on page 1, line 7 instead. Tax-exempt interest goes on line 18a.",
        attachment: None },
    TaxLineDef { key: "k6a", number: "6a", label: "Ordinary dividends", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_10[0]"), sense: Sense::Natural,
        instructions: "Total ordinary dividends received from domestic and qualified foreign corporations. Portfolio income to the partners, so it is separately stated rather than netted into ordinary income.",
        attachment: None },
    TaxLineDef { key: "k6b", number: "6b", label: "Qualified dividends", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_11[0]"), sense: Sense::Natural,
        instructions: "The part of line 6a that is qualified dividend income, taxed to the partners at capital gain rates. Included in line 6a, not added to it.",
        attachment: None },
    TaxLineDef { key: "k6c", number: "6c", label: "Dividend equivalents", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_12[0]"), sense: Sense::Natural,
        instructions: "Payments treated as dividend equivalents under section 871(m).",
        attachment: None },
    TaxLineDef { key: "k7", number: "7", label: "Royalties", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_13[0]"), sense: Sense::Natural,
        instructions: "Royalty income. Separately stated as portfolio income unless the partnership is in the business of producing royalties.",
        attachment: None },
    TaxLineDef { key: "k8", number: "8", label: "Net short-term capital gain (loss)", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_14[0]"), sense: Sense::Natural,
        instructions: "Net short-term capital gain or loss from Schedule D (Form 1065). Capital gains keep their character all the way to each partner's return, which is why they never net into ordinary income.",
        attachment: Some(SCHEDULE_D) },
    TaxLineDef { key: "k9a", number: "9a", label: "Net long-term capital gain (loss)", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_15[0]"), sense: Sense::Natural,
        instructions: "Net long-term capital gain or loss from Schedule D (Form 1065).",
        attachment: Some(SCHEDULE_D) },
    TaxLineDef { key: "k9b", number: "9b", label: "Collectibles (28%) gain (loss)", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_16[0]"), sense: Sense::Natural,
        instructions: "The part of line 9a from collectibles, taxed at 28%. Included in line 9a, not added to it.",
        attachment: None },
    TaxLineDef { key: "k9c", number: "9c", label: "Unrecaptured section 1250 gain", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_17[0]"), sense: Sense::Natural,
        instructions: "The part of line 9a that is unrecaptured section 1250 gain. A statement has to be attached.",
        attachment: None },
    TaxLineDef { key: "k10", number: "10", label: "Net section 1231 gain (loss)", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_18[0]"), sense: Sense::Natural,
        instructions: "Net gain or loss on business property held more than a year, from Form 4797. Separately stated because each partner nets it against their own section 1231 transactions.",
        attachment: Some(FORM_4797) },
    TaxLineDef { key: "k11", number: "11", label: "Other income (loss)", group: "Income (loss)", schedule: Schedule::K, field: Field::One("f5_20[0]"), sense: Sense::Natural,
        instructions: "Separately stated income that fits no other line — cancellation of debt, section 951A inclusions, gambling gains. The type has to be entered beside the amount.",
        attachment: None },

    // --- Schedule K: deductions -------------------------------------------
    TaxLineDef { key: "k12", number: "12", label: "Section 179 deduction", group: "Deductions", schedule: Schedule::K, field: Field::One("f5_21[0]"), sense: Sense::Natural,
        instructions: "The section 179 expense election, from Form 4562. Never on page 1: the dollar limit and the taxable-income limit are applied on each partner's own return, so the partnership only reports the amount.",
        attachment: Some(FORM_4562) },
    TaxLineDef { key: "k13a", number: "13a", label: "Cash contributions", group: "Deductions", schedule: Schedule::K, field: Field::One("f5_22[0]"), sense: Sense::Natural,
        instructions: "Cash charitable contributions. A partnership takes no charitable deduction itself — each partner does, subject to their own limit — so these must not appear in page 1, line 21.",
        attachment: None },
    TaxLineDef { key: "k13b", number: "13b", label: "Noncash contributions", group: "Deductions", schedule: Schedule::K, field: Field::One("f5_23[0]"), sense: Sense::Natural,
        instructions: "Charitable contributions of property rather than cash. Contributions over $500 need Form 8283 attached.",
        attachment: None },
    TaxLineDef { key: "k13c", number: "13c", label: "Investment interest expense", group: "Deductions", schedule: Schedule::K, field: Field::One("f5_24[0]"), sense: Sense::Natural,
        instructions: "Interest on debt used to buy or carry investment property. Separately stated because the deduction is limited to each partner's own net investment income.",
        attachment: None },
    TaxLineDef { key: "k13d", number: "13d", label: "Section 59(e)(2) expenditures", group: "Deductions", schedule: Schedule::K, field: Field::One("f5_26[0]"), sense: Sense::Natural,
        instructions: "Expenditures a partner may elect to amortise over 10 years rather than deduct at once — circulation, research, mining development. The type has to be entered beside the amount.",
        attachment: None },
    TaxLineDef { key: "k13e", number: "13e", label: "Other deductions", group: "Deductions", schedule: Schedule::K, field: Field::One("f5_28[0]"), sense: Sense::Natural,
        instructions: "Separately stated deductions fitting no other line — partner retirement plan contributions, penalty on early withdrawal, certain portfolio expenses. The type has to be entered beside the amount.",
        attachment: None },

    // --- Schedule K: self-employment --------------------------------------
    TaxLineDef { key: "k14a", number: "14a", label: "Net earnings (loss) from self-employment", group: "Self-employment", schedule: Schedule::K, field: Field::One("f5_29[0]"), sense: Sense::Natural,
        instructions: "Net earnings from self-employment, which each general partner takes onto their own Schedule SE. Not the same as ordinary business income: it excludes rental income and portfolio items, and it includes guaranteed payments for services.",
        attachment: None },
    TaxLineDef { key: "k14b", number: "14b", label: "Gross farming or fishing income", group: "Self-employment", schedule: Schedule::K, field: Field::One("f5_30[0]"), sense: Sense::Natural,
        instructions: "Gross income from farming or fishing, needed by partners who use the farm income averaging or optional SE method.",
        attachment: None },
    TaxLineDef { key: "k14c", number: "14c", label: "Gross nonfarm income", group: "Self-employment", schedule: Schedule::K, field: Field::One("f5_31[0]"), sense: Sense::Natural,
        instructions: "Gross nonfarm income, needed by partners using the nonfarm optional method on Schedule SE.",
        attachment: None },

    // --- Schedule K: other information ------------------------------------
    TaxLineDef { key: "k18a", number: "18a", label: "Tax-exempt interest income", group: "Other information", schedule: Schedule::K, field: Field::One("f5_47[0]"), sense: Sense::Natural,
        instructions: "Interest excluded from income, typically municipal bond interest. Reported so each partner can add it to basis and report it on their own return, not because it is taxed.",
        attachment: None },
    TaxLineDef { key: "k18b", number: "18b", label: "Other tax-exempt income", group: "Other information", schedule: Schedule::K, field: Field::One("f5_48[0]"), sense: Sense::Natural,
        instructions: "Income excluded from gross income other than tax-exempt interest — life insurance proceeds, certain forgiven loans. Increases each partner's basis.",
        attachment: None },
    TaxLineDef { key: "k18c", number: "18c", label: "Nondeductible expenses", group: "Other information", schedule: Schedule::K, field: Field::One("f5_49[0]"), sense: Sense::Natural,
        instructions: "Expenses paid that are not deductible and not capitalised — the disallowed half of meals, fines and penalties, political contributions. Reduces each partner's basis and their tax-basis capital account, which is why it is reported rather than ignored. A statement itemising what makes up this line is attached, and each partner's K-1 carries their own share of the same list.",
        attachment: Some(NONDEDUCTIBLE_STATEMENT) },
    TaxLineDef { key: "k19a", number: "19a", label: "Distributions of cash and marketable securities", group: "Other information", schedule: Schedule::K, field: Field::One("f5_50[0]"), sense: Sense::Natural,
        instructions: "Cash and marketable securities distributed to partners during the year. A distribution is not a deduction — it reduces the partner's capital account and basis.",
        attachment: None },
    TaxLineDef { key: "k19b", number: "19b", label: "Distributions of other property", group: "Other information", schedule: Schedule::K, field: Field::One("f5_51[0]"), sense: Sense::Natural,
        instructions: "Property other than cash and marketable securities distributed to partners during the year.",
        attachment: None },
    TaxLineDef { key: "k20a", number: "20a", label: "Investment income", group: "Other information", schedule: Schedule::K, field: Field::One("f5_52[0]"), sense: Sense::Natural,
        instructions: "Investment income included in lines 5, 6a, 7 and 11 — what each partner needs to compute their own investment interest limit.",
        attachment: None },
    TaxLineDef { key: "k20b", number: "20b", label: "Investment expenses", group: "Other information", schedule: Schedule::K, field: Field::One("f5_53[0]"), sense: Sense::Natural,
        instructions: "Expenses included on line 13e that are investment expenses.",
        attachment: None },
    TaxLineDef { key: "k21", number: "21", label: "Total foreign taxes paid or accrued", group: "Other information", schedule: Schedule::K, field: Field::One("f5_55[0]"), sense: Sense::Natural,
        instructions: "Foreign taxes paid or accrued, which each partner may take as a credit or a deduction on their own return. Subtracted in the Analysis of Net Income on page 6.",
        attachment: None },

    // --- Schedule L: assets -----------------------------------------------
    // Single-value rows report in columns (b) and (d). The paired rows below —
    // a gross figure and the "less" that reduces it — report the two halves in
    // (a) and (c), which is where the form prints them.
    TaxLineDef { key: "sl1", number: "1", label: "Cash", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_15[0]", end: "f6_17[0]" }, sense: Sense::Natural,
        instructions: "Cash on hand and in bank accounts at the start and end of the year.",
        attachment: None },
    TaxLineDef { key: "sl2a", number: "2a", label: "Trade notes and accounts receivable", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_18[0]", end: "f6_20[0]" }, sense: Sense::Natural,
        instructions: "Receivables from trade, gross, before any allowance for bad debts.",
        attachment: None },
    TaxLineDef { key: "sl2b", number: "2b", label: "Less allowance for bad debts", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_22[0]", end: "f6_24[0]" }, sense: Sense::Contra,
        instructions: "The allowance for doubtful accounts. Enter as a positive figure; the form subtracts it from line 2a.",
        attachment: None },
    TaxLineDef { key: "sl3", number: "3", label: "Inventories", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_27[0]", end: "f6_29[0]" }, sense: Sense::Natural,
        instructions: "Inventory on hand, valued the same way it is valued on Form 1125-A.",
        attachment: None },
    TaxLineDef { key: "sl4", number: "4", label: "U.S. Government obligations", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_31[0]", end: "f6_33[0]" }, sense: Sense::Natural,
        instructions: "Treasury and other U.S. Government obligations held.",
        attachment: None },
    TaxLineDef { key: "sl5", number: "5", label: "Tax-exempt securities", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_35[0]", end: "f6_37[0]" }, sense: Sense::Natural,
        instructions: "State and municipal obligations whose interest is excluded from income.",
        attachment: None },
    TaxLineDef { key: "sl6", number: "6", label: "Other current assets", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_39[0]", end: "f6_41[0]" }, sense: Sense::Natural,
        instructions: "Current assets fitting none of lines 1 through 5 — prepaid expenses, short-term deposits. A statement itemising them has to be attached.",
        attachment: None },
    TaxLineDef { key: "sl7a", number: "7a", label: "Loans to partners (or persons related to partners)", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_43[0]", end: "f6_45[0]" }, sense: Sense::Natural,
        instructions: "Amounts lent to partners or their relations and still outstanding.",
        attachment: None },
    TaxLineDef { key: "sl7b", number: "7b", label: "Mortgage and real estate loans", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_47[0]", end: "f6_49[0]" }, sense: Sense::Natural,
        instructions: "Mortgage and real estate loans held as assets of the partnership.",
        attachment: None },
    TaxLineDef { key: "sl8", number: "8", label: "Other investments", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_51[0]", end: "f6_53[0]" }, sense: Sense::Natural,
        instructions: "Investments fitting none of lines 4, 5 or 7b. A statement itemising them has to be attached.",
        attachment: None },
    TaxLineDef { key: "sl9a", number: "9a", label: "Buildings and other depreciable assets", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_54[0]", end: "f6_56[0]" }, sense: Sense::Natural,
        instructions: "Depreciable assets at cost, gross, before accumulated depreciation.",
        attachment: None },
    TaxLineDef { key: "sl9b", number: "9b", label: "Less accumulated depreciation", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_58[0]", end: "f6_60[0]" }, sense: Sense::Contra,
        instructions: "Accumulated depreciation on the assets in line 9a. Enter as a positive figure; the form subtracts it.",
        attachment: None },
    TaxLineDef { key: "sl10a", number: "10a", label: "Depletable assets", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_62[0]", end: "f6_64[0]" }, sense: Sense::Natural,
        instructions: "Depletable natural resource assets at cost, gross.",
        attachment: None },
    TaxLineDef { key: "sl10b", number: "10b", label: "Less accumulated depletion", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_66[0]", end: "f6_68[0]" }, sense: Sense::Contra,
        instructions: "Accumulated depletion on the assets in line 10a. Enter as a positive figure.",
        attachment: None },
    TaxLineDef { key: "sl11", number: "11", label: "Land (net of any amortization)", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_71[0]", end: "f6_73[0]" }, sense: Sense::Natural,
        instructions: "Land held, net of amortisation. Land is not depreciated.",
        attachment: None },
    TaxLineDef { key: "sl12a", number: "12a", label: "Intangible assets (amortizable only)", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_74[0]", end: "f6_76[0]" }, sense: Sense::Natural,
        instructions: "Amortisable intangibles at cost, gross — goodwill acquired, covenants, organisation costs.",
        attachment: None },
    TaxLineDef { key: "sl12b", number: "12b", label: "Less accumulated amortization", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_78[0]", end: "f6_80[0]" }, sense: Sense::Contra,
        instructions: "Accumulated amortisation on the intangibles in line 12a. Enter as a positive figure.",
        attachment: None },
    TaxLineDef { key: "sl13", number: "13", label: "Other assets", group: "Assets", schedule: Schedule::L, field: Field::Period { begin: "f6_83[0]", end: "f6_85[0]" }, sense: Sense::Natural,
        instructions: "Assets fitting none of lines 1 through 12. A statement itemising them has to be attached.",
        attachment: None },

    // --- Schedule L: liabilities and capital -------------------------------
    TaxLineDef { key: "sl15", number: "15", label: "Accounts payable", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_91[0]", end: "f6_93[0]" }, sense: Sense::Natural,
        instructions: "Trade payables outstanding at the start and end of the year.",
        attachment: None },
    TaxLineDef { key: "sl16", number: "16", label: "Mortgages, notes, bonds payable in less than 1 year", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_95[0]", end: "f6_97[0]" }, sense: Sense::Natural,
        instructions: "Short-term borrowings — those maturing within a year of the balance sheet date.",
        attachment: None },
    TaxLineDef { key: "sl17", number: "17", label: "Other current liabilities", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_99[0]", end: "f6_101[0]" }, sense: Sense::Natural,
        instructions: "Current liabilities fitting none of lines 15, 16 or 19a — accrued expenses, payroll taxes withheld, deferred revenue. A statement itemising them has to be attached.",
        attachment: None },
    TaxLineDef { key: "sl18", number: "18", label: "All nonrecourse loans", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_103[0]", end: "f6_105[0]" }, sense: Sense::Natural,
        instructions: "Loans for which no partner bears the economic risk of loss. The distinction matters to each partner's basis and at-risk amount.",
        attachment: None },
    TaxLineDef { key: "sl19a", number: "19a", label: "Loans from partners (or persons related to partners)", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_107[0]", end: "f6_109[0]" }, sense: Sense::Natural,
        instructions: "Amounts owed to partners or their relations. A partner's loan is debt, not capital, and belongs here rather than in line 21.",
        attachment: None },
    TaxLineDef { key: "sl19b", number: "19b", label: "Mortgages, notes, bonds payable in 1 year or more", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_111[0]", end: "f6_113[0]" }, sense: Sense::Natural,
        instructions: "Long-term borrowings — those maturing a year or more after the balance sheet date.",
        attachment: None },
    TaxLineDef { key: "sl20", number: "20", label: "Other liabilities", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_115[0]", end: "f6_117[0]" }, sense: Sense::Natural,
        instructions: "Liabilities fitting none of lines 15 through 19b. A statement itemising them has to be attached.",
        attachment: None },
    TaxLineDef { key: "sl21", number: "21", label: "Partners' capital accounts", group: "Liabilities and capital", schedule: Schedule::L, field: Field::Period { begin: "f6_119[0]", end: "f6_121[0]" }, sense: Sense::Natural,
        instructions: "Total partners' capital. Schedule L is kept on the books-and-records basis, so this is book capital, which is not the same as each partner's tax capital reported in item L of their K-1.",
        attachment: None },
];

/// Look a line definition up by its stored key.
pub fn line_def(key: &str) -> Option<&'static TaxLineDef> {
    MAPPABLE_LINES.iter().find(|d| d.key == key)
}

/// Every valid line key.
pub fn line_keys() -> Vec<&'static str> {
    MAPPABLE_LINES.iter().map(|d| d.key).collect()
}

/// Page one's income and deduction lines, in whole dollars.
///
/// Whole dollars because that is what reaches the form; carrying cents here and
/// rounding at the point of printing would let the derived totals be computed
/// from figures the reader never sees, which is the exact failure the module
/// docs describe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form1065Lines {
    /// Mapped lines, by key, in whole dollars. Absent means no account was
    /// pointed at that line — distinct from mapped-and-zero.
    mapped: BTreeMap<&'static str, i64>,
}

impl Form1065Lines {
    /// Build from explicit figures. Tests and segment arithmetic.
    pub fn from_pairs(pairs: &[(&'static str, i64)]) -> Self {
        Form1065Lines {
            mapped: pairs.iter().copied().collect(),
        }
    }

    /// The same lines scaled to a fraction of a period — `days` out of `total`.
    ///
    /// For §706(d) proration, which spreads one year's figures across the
    /// segments its partners' changes divide it into. Every line is scaled by
    /// the same fraction, so the relationships between them survive; the
    /// rounding is per line and toward zero, which means the segments can come
    /// to a dollar or two less than the year. That is why the caller turns these
    /// into percentages of the annual figure rather than filing them: the shares
    /// are then allocated out of the year's own total and foot to it exactly.
    pub fn scaled(&self, days: i64, total: i64) -> Self {
        if total == 0 {
            return Form1065Lines::default();
        }
        Form1065Lines {
            mapped: self
                .mapped
                .iter()
                .map(|(k, v)| (*k, (*v as i128 * days as i128 / total as i128) as i64))
                .collect(),
        }
    }

    /// A mapped line's figure, or zero when nothing was mapped to it.
    ///
    /// Zero rather than `None` because every arithmetic use of a line wants it
    /// to behave as nothing, and forcing each caller to unwrap invites one of
    /// them to unwrap it differently.
    pub fn get(&self, key: &str) -> i64 {
        self.mapped.get(key).copied().unwrap_or(0)
    }

    /// Whether an account was actually pointed at this line.
    pub fn is_mapped(&self, key: &str) -> bool {
        self.mapped.contains_key(key)
    }

    // --- derived lines: arithmetic on the rounded dollars above ---

    /// 1c. Balance — gross receipts less returns and allowances.
    pub fn line_1c(&self) -> i64 {
        self.get("l1a") - self.get("l1b")
    }

    /// 3. Gross profit — line 1c less cost of goods sold.
    pub fn line_3(&self) -> i64 {
        self.line_1c() - self.get("l2")
    }

    /// 8. Total income (loss) — combine lines 3 through 7.
    pub fn line_8(&self) -> i64 {
        self.line_3() + self.get("l4") + self.get("l5") + self.get("l6") + self.get("l7")
    }

    /// 16c. Depreciation claimed here — 16a less what is reported elsewhere.
    pub fn line_16c(&self) -> i64 {
        self.get("l16a") - self.get("l16b")
    }

    /// 22. Total deductions — lines 9 through 21, taking 16c rather than 16a.
    ///
    /// 16c and not 16a: 16a is depreciation in total and 16b is the part already
    /// deducted through cost of goods sold. Adding 16a would deduct that part
    /// twice, which is the single easiest way to overstate a deduction on this
    /// page.
    pub fn line_22(&self) -> i64 {
        self.get("l9")
            + self.get("l10")
            + self.get("l11")
            + self.get("l12")
            + self.get("l13")
            + self.get("l14")
            + self.get("l15")
            + self.line_16c()
            + self.get("l17")
            + self.get("l18")
            + self.get("l19")
            + self.get("l20")
            + self.get("l21")
    }

    /// 23. Ordinary business income (loss) — total income less total deductions.
    pub fn line_23(&self) -> i64 {
        self.line_8() - self.line_22()
    }

    // --- Schedule K: the derived lines -------------------------------------

    /// Schedule K, line 1. Ordinary business income (loss).
    ///
    /// The same figure as page one's line 23, and deliberately not a separate
    /// mapping: this is the repetition between the two pages, and the only safe
    /// way to render it is to compute it once. A line 1 that could be mapped
    /// independently is a return whose two pages disagree about the same number.
    pub fn k_line_1(&self) -> i64 {
        self.line_23()
    }

    /// Schedule K, line 3c. Other net rental income — 3a less 3b.
    pub fn k_line_3c(&self) -> i64 {
        self.get("k3a") - self.get("k3b")
    }

    /// Schedule K, line 4c. Total guaranteed payments — services plus capital.
    pub fn k_line_4c(&self) -> i64 {
        self.get("k4a") + self.get("k4b")
    }

    /// Analysis of Net Income (Loss), page 6, line 1.
    ///
    /// Schedule K lines 1 through 11, less the sum of lines 12 through 13e and
    /// 21, exactly as the form words it.
    ///
    /// Lines 6b, 9b and 9c are *not* added: each is a subset of the line above it
    /// — qualified dividends are part of ordinary dividends, collectibles and
    /// unrecaptured 1250 gain are parts of long-term capital gain — and adding
    /// them would count the same money twice.
    pub fn k_analysis(&self) -> i64 {
        let income = self.k_line_1()
            + self.get("k2")
            + self.k_line_3c()
            + self.k_line_4c()
            + self.get("k5")
            + self.get("k6a")
            + self.get("k6c")
            + self.get("k7")
            + self.get("k8")
            + self.get("k9a")
            + self.get("k10")
            + self.get("k11");
        let deductions = self.get("k12")
            + self.get("k13a")
            + self.get("k13b")
            + self.get("k13c")
            + self.get("k13d")
            + self.get("k13e")
            + self.get("k21");
        income - deductions
    }

    /// Whether any Schedule K line carries a figure.
    ///
    /// Line 1 alone does not count: it is page one's result restated, so a
    /// partnership with nothing separately stated still has a line 1, and
    /// treating that as "Schedule K is in use" would suppress the warning that
    /// says nothing was mapped to it.
    pub fn any_schedule_k(&self) -> bool {
        MAPPABLE_LINES
            .iter()
            .filter(|d| d.schedule == Schedule::K)
            .any(|d| self.is_mapped(d.key))
    }

    /// The same lines with one figure put in place of whatever it was.
    ///
    /// For a line whose figure is a fact the ledger's mapping cannot express —
    /// Schedule K line 19a, which is each partner's draws rather than an account
    /// balance. A key the form does not have is ignored rather than inserted.
    pub fn with_line(&self, key: &str, dollars: i64) -> Self {
        let mut out = self.clone();
        if let Some(def) = line_def(key) {
            out.mapped.insert(def.key, dollars);
        }
        out
    }

    /// Set a line directly. Tests only — the real path is [`compute`], which is
    /// the only thing that knows the rounding order the totals depend on.
    #[cfg(test)]
    pub fn set_for_test(&mut self, key: &str, dollars: i64) {
        let def = line_def(key).expect("test used a line key the form does not have");
        self.mapped.insert(def.key, dollars);
    }

    /// Whether anything at all was mapped.
    ///
    /// An unmapped ledger produces an identity-only return rather than a page of
    /// zeros, which is the honest rendering of "nobody has done this yet".
    pub fn is_empty(&self) -> bool {
        self.mapped.is_empty()
    }
}

/// Cents → whole dollars, rounded half away from zero.
///
/// Away from zero rather than to-even so that a loss and a profit of the same
/// magnitude round to the same magnitude; banker's rounding would make the sign
/// of a figure change how big it is, which is impossible to explain to anybody
/// reading the return.
pub fn cents_to_dollars(cents: i64) -> i64 {
    let (sign, abs) = if cents < 0 { (-1, -cents) } else { (1, cents) };
    sign * ((abs + 50) / 100)
}

/// A figure as it should appear in a form box or on a statement.
///
/// Thousands are separated by commas, which is how the IRS prints every figure
/// in its own instructions and what every reader of a return expects. A
/// seven-figure deduction written as a bare run of digits has to be counted off
/// by eye to be read at all, and a misread figure on a tax return is a
/// correction letter.
///
/// Losses carry a leading minus rather than parentheses: the boxes the form
/// pre-prints parentheses around already have them printed, and adding a second
/// pair inside one of those reads as a nested negative.
///
/// This is the one place a dollar figure becomes text — page one, Schedule K,
/// Schedule L, every Schedule K-1 and every attached statement all come through
/// here. That is deliberate: two formatters would eventually disagree, and a
/// statement whose figures are punctuated differently from the box they support
/// invites the reader to wonder which one is the real number.
pub fn format_dollars(dollars: i64) -> String {
    let digits = dollars.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if dollars < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        // A separator before every group of three that is not the first.
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One account's contribution to a line, in cents, already oriented the way the
/// line prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineDetail {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    /// Signed cents as the line reports them — a "less" line's accounts appear
    /// positive here, because that is how they print.
    pub cents: i64,
}

/// What [`compute`] found that somebody should see before filing.
pub struct ComputedLines {
    pub lines: Form1065Lines,
    /// Which accounts made up each line, by line key.
    ///
    /// Kept rather than discarded because two things need it and neither can
    /// reconstruct it: the "attach statement" lines have to itemise what they
    /// contain, and a mapping editor is much easier to trust when it shows the
    /// figures a mapping is about to produce.
    pub detail: BTreeMap<&'static str, Vec<LineDetail>>,
    /// Accounts carrying a balance that no line claims — income and expense that
    /// would otherwise leave the return without trace.
    pub warnings: Vec<String>,
}

/// What a line key resolves to: the key the engine buckets under, and whether
/// the line prints its accounts positive or negative.
///
/// A closure rather than a catalogue slice, so a form with a different line type
/// can use the same engine without either form's line definition growing a
/// variant for the other's. Schedule C's lines are not Form 1065 lines and there
/// is no useful sense in which they could be.
pub type ResolveLine<'a> = &'a dyn Fn(&str) -> Option<(&'static str, Sense)>;

/// Total a period's income statement onto whatever lines `resolve` recognises.
///
/// The whole of the arithmetic every form needs and none of the knowledge of any
/// particular form: bucket the accounts, sum in cents so rounding happens once
/// per line, and report what reached no line at all. That last part is the reason
/// this is shared rather than copied — "which accounts are missing from the
/// return" is the check that catches money going astray, and a second
/// implementation of it would eventually disagree with this one.
///
/// `form` names the return in the warning, because "on no line" is only useful
/// when the reader knows which form's lines were being looked at.
pub fn sum_by_line(
    statement: &IncomeStatement,
    mapping: &BTreeMap<String, String>,
    // How much of each account is deductible, inherited down the tree. An
    // account absent from it is fully deductible.
    limits: &BTreeMap<String, u8>,
    // Where the disallowed part goes. `Some` on Form 1065, which reports it on
    // Schedule K line 18c so it reaches each partner's basis; `None` on
    // Schedule C, which has nowhere to put it and simply does not deduct it.
    disallowed_line: Option<&'static str>,
    resolve: ResolveLine<'_>,
    form: &str,
) -> (
    BTreeMap<&'static str, i64>,
    BTreeMap<&'static str, Vec<LineDetail>>,
    Vec<String>,
) {
    let mut cents: BTreeMap<&'static str, i64> = BTreeMap::new();
    let mut detail: BTreeMap<&'static str, Vec<LineDetail>> = BTreeMap::new();
    let mut unmapped: Vec<(String, String, i64)> = Vec::new();
    let mut unknown_keys: BTreeSet<String> = BTreeSet::new();
    let mut warnings_from_limits: Vec<(String, String, i64, u8)> = Vec::new();

    let all = statement
        .revenue
        .lines
        .iter()
        .chain(statement.expenses.lines.iter());

    for line in all {
        // Backfilled ancestors come through with a zero balance, and a genuinely
        // zero account has nothing to report either way. Neither is worth
        // warning about — an unmapped account matters because money is going
        // missing, and no money is.
        if line.balance == 0 {
            continue;
        }
        match mapping.get(&line.account_id) {
            // Deliberately on no line. Not summed, and — unlike an account
            // nobody has ruled on — not warned about either.
            Some(key) if key == OFF_RETURN => continue,
            Some(key) => match resolve(key) {
                Some((canonical, sense)) => {
                    let signed = match sense {
                        Sense::Natural => line.balance,
                        Sense::Contra => -line.balance,
                    };
                    // The law's share of it. Meals are the standing example: the
                    // books hold what the meal cost, the return deducts half,
                    // and the other half is reported rather than forgotten.
                    let pct = limits.get(&line.account_id).copied().unwrap_or(100);
                    let (allowed, disallowed) = split_deductible(signed, pct);

                    *cents.entry(canonical).or_insert(0) += allowed;
                    detail.entry(canonical).or_default().push(LineDetail {
                        account_id: line.account_id.clone(),
                        account_number: line.account_number.clone(),
                        account_name: line.account_name.clone(),
                        cents: allowed,
                    });

                    if disallowed != 0 {
                        match disallowed_line.and_then(|k| resolve(k)) {
                            Some((nd_line, _)) => {
                                *cents.entry(nd_line).or_insert(0) += disallowed;
                                detail.entry(nd_line).or_default().push(LineDetail {
                                    account_id: line.account_id.clone(),
                                    account_number: line.account_number.clone(),
                                    account_name: format!(
                                        "{} — {}% disallowed",
                                        line.account_name,
                                        100 - pct
                                    ),
                                    cents: disallowed,
                                });
                            }
                            // Nowhere to report it on this form. Not deducted,
                            // and said out loud: a limit that silently evaporated
                            // would look exactly like a full deduction.
                            None => warnings_from_limits.push((
                                line.account_number.clone(),
                                line.account_name.clone(),
                                disallowed,
                                pct,
                            )),
                        }
                    }
                }
                // A key this form does not have — a revision dropped the line,
                // the row was hand-edited, or the books changed which return
                // they file and the mapping still points at the old form.
                // Treated as unmapped, because that is what it is, and named
                // separately so the cause is visible.
                None => {
                    unknown_keys.insert(key.clone());
                    unmapped.push((
                        line.account_number.clone(),
                        line.account_name.clone(),
                        line.balance,
                    ));
                }
            },
            None => unmapped.push((
                line.account_number.clone(),
                line.account_name.clone(),
                line.balance,
            )),
        }
    }

    let mut warnings = Vec::new();
    for (number, name, disallowed, pct) in &warnings_from_limits {
        warnings.push(format!(
            "{number} {name} is {pct}% deductible, so {} is not deducted — but {form} has no line \
             for a disallowed expense, so it is simply left out. Check that is what the form \
             expects for this deduction.",
            format_dollars(cents_to_dollars(*disallowed))
        ));
    }
    if !unmapped.is_empty() {
        let total: i64 = unmapped.iter().map(|(_, _, c)| *c).sum();
        let named: Vec<String> = unmapped
            .iter()
            .map(|(num, name, c)| {
                format!("{num} {name} ({})", format_dollars(cents_to_dollars(*c)))
            })
            .collect();
        warnings.push(format!(
            "{} account(s) carrying {} are on no {form} line and are missing from the return: {}.",
            unmapped.len(),
            format_dollars(cents_to_dollars(total)),
            named.join(", ")
        ));
    }
    for key in unknown_keys {
        warnings.push(format!(
            "Mapping refers to line key {key:?}, which {form} does not have."
        ));
    }

    // Largest first: a statement of thirty accounts is read from the top, and
    // the figures that matter are the big ones.
    for rows in detail.values_mut() {
        rows.sort_by(|a, b| {
            b.cents
                .abs()
                .cmp(&a.cents.abs())
                .then(a.account_number.cmp(&b.account_number))
        });
    }

    (cents, detail, warnings)
}

/// Total a period's income statement onto page one's lines.
///
/// `mapping` is account id → line key, as [`load_mapping`] returns it.
pub fn compute(
    statement: &IncomeStatement,
    mapping: &BTreeMap<String, String>,
    limits: &BTreeMap<String, u8>,
) -> ComputedLines {
    let (cents, detail, warnings) = sum_by_line(
        statement,
        mapping,
        limits,
        Some(NONDEDUCTIBLE_LINE),
        &|key| line_def(key).map(|d| (d.key, d.sense)),
        "Form 1065",
    );
    let mapped: BTreeMap<&'static str, i64> = cents
        .into_iter()
        .map(|(k, c)| (k, cents_to_dollars(c)))
        .collect();
    ComputedLines {
        lines: Form1065Lines { mapped },
        detail,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Line key meaning "deliberately on no line".
///
/// Absence from the mapping means "nobody has said yet", and the page shouts
/// about an account in that state carrying a balance — money that would go
/// missing from the return. Once a parent is mapped its children inherit, so
/// there has to be a way to say *no* for one of them that is louder than
/// silence: this key is that, and unlike silence it draws no warning, because
/// somebody decided it.
pub const OFF_RETURN: &str = "off";

/// Every account's parent, for walking a mapping up the tree.
///
/// Read straight from the table rather than from a list of active accounts: a
/// deactivated parent still sits between a live child and the line it inherits,
/// and dropping it out of the chain would silently take the child off the
/// return.
pub fn load_parents(conn: &Connection) -> BTreeMap<String, Option<String>> {
    let mut out = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, parent_id FROM accounts") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        }) {
            for (id, parent) in rows.flatten() {
                out.insert(id, parent);
            }
        }
    }
    out
}

/// How far up a parent chain to walk before deciding it is not a chain.
///
/// A chart of accounts is a handful of levels deep. This is not a limit on
/// nesting anybody will meet — it is what stops a corrupt `parent_id` cycle
/// from hanging the tax page.
const MAX_TREE_DEPTH: usize = 32;

/// Expand a mapping so every account carries the line of its nearest mapped
/// ancestor.
///
/// # Why this is the shape of the feature
///
/// A chart of accounts already says that Squarespace, Canva and Flodesk are
/// software; repeating that on a tax page by pointing all three at line 20, one
/// dropdown at a time, is work the tree has already done. Mapping
/// `Expenses:Software` is the same statement made once. A child that belongs
/// somewhere else says so itself and its own key wins, because the nearest
/// answer is the most specific one.
///
/// # Why it is expanded here rather than looked up at each use
///
/// Three forms and a preview all ask "which line does this account reach", and
/// the warning about accounts reaching no line is the check that catches money
/// going astray. If inheritance lived in the callers, one of them would
/// eventually walk the tree differently, and the symptom would be a return that
/// foots and is wrong. Expanding once, here, means every caller is asking the
/// same question of the same answer.
pub fn inherit_through_tree(
    stored: &BTreeMap<String, String>,
    parent_of: &BTreeMap<String, Option<String>>,
) -> BTreeMap<String, String> {
    // Anything mapped whose account the tree does not know about is kept as it
    // is: a mapping is not worth dropping because an account row is missing.
    let mut out = stored.clone();
    for account_id in parent_of.keys() {
        if out.contains_key(account_id) {
            continue;
        }
        let mut cursor = parent_of.get(account_id).cloned().flatten();
        for _ in 0..MAX_TREE_DEPTH {
            let Some(id) = cursor else { break };
            if let Some(key) = stored.get(&id) {
                out.insert(account_id.clone(), key.clone());
                break;
            }
            cursor = parent_of.get(&id).cloned().flatten();
        }
    }
    out
}

/// Schedule K's line for expenses paid that the law does not let you deduct.
///
/// Where the disallowed part of a limited deduction goes on Form 1065. It is not
/// a deduction anywhere: it reduces each partner's outside basis and capital
/// account, which is why dropping it silently would overstate basis and
/// understate a later gain.
pub const NONDEDUCTIBLE_LINE: &str = "k18c";

/// How much of each account's balance the law lets you deduct, as a percentage.
///
/// An account with no row is fully deductible. Stored sparsely for that reason:
/// a 100 written against every account is a hundred rows that mean "nothing
/// unusual here" and one more thing to keep right.
pub fn load_deduction_limits(conn: &Connection, year: i32) -> BTreeMap<String, u8> {
    load_dated(conn, "tax_deduction_limits", "deductible_pct", year)
        .into_iter()
        .filter_map(|(account, pct)| pct.parse::<u8>().ok().map(|p| (account, p.min(100))))
        .collect()
}

/// The same, inherited down the account tree like a line assignment.
///
/// A limit set on "Meals" is a statement about everything filed under it, in
/// exactly the way a line assignment is — and a child that is limited
/// differently says so itself and wins, being the nearer answer.
pub fn load_effective_limits(conn: &Connection, year: i32) -> BTreeMap<String, u8> {
    let stored = load_deduction_limits(conn, year);
    let parent_of = load_parents(conn);
    let as_text: BTreeMap<String, String> = stored
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    inherit_through_tree(&as_text, &parent_of)
        .into_iter()
        .filter_map(|(k, v)| v.parse::<u8>().ok().map(|p| (k, p)))
        .collect()
}

/// Which parent accounts print as one row on the attached statements, for a year.
///
/// Stored as a dated yes or no — see migration 040 — so only the yeses in force
/// for the year come back.
pub fn load_statement_groups(conn: &Connection, year: i32) -> BTreeSet<String> {
    load_dated(conn, "tax_statement_groups", "grouped", year)
        .into_iter()
        .filter(|(_, grouped)| grouped == "1")
        .map(|(account, _)| account)
        .collect()
}

/// The accounts marked as Illinois income or replacement tax for a year — the
/// ones IL-1065 line 16 adds back. Dated yeses and nos, like statement groups;
/// the tree is walked by the reader, since a mark covers what is filed beneath.
pub fn load_illinois_tax_addbacks(conn: &Connection, year: i32) -> BTreeSet<String> {
    load_dated(conn, "il_tax_addbacks", "added_back", year)
        .into_iter()
        .filter(|(_, on)| on == "1")
        .map(|(account, _)| account)
        .collect()
}

/// Every account's number and name, by id.
///
/// For labelling a grouped statement row: the parent a group is named for often
/// carries no balance of its own, so it is not among the rows being grouped.
/// Read from the table rather than the active accounts, for the reason
/// [`load_parents`] is.
pub fn load_account_labels(conn: &Connection) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, account_number, name FROM accounts") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
            ))
        }) {
            for (id, label) in rows.flatten() {
                out.insert(id, label);
            }
        }
    }
    out
}

/// What a grouped statement row adds to its parent's name, so a reader tallying
/// the column knows the row is a subtotal. The reports' wording, deliberately.
pub const WITH_CHILDREN: &str = " (with children)";

/// Collapse a statement's rows under the parents set to group their children.
///
/// Each row goes to its outermost grouped ancestor — itself included — and rows
/// that share one are summed; a row with no grouped ancestor stays as it is.
/// Outermost, because grouping "Office" is a statement about everything under
/// it, a grouped child of it included. Groups keep the position of their first
/// row, so the statement keeps the order the books gave it.
///
/// The total cannot move: grouping relocates figures between rows, and a
/// statement that no longer footed to its line would be worse than a long one.
pub fn group_statement_rows(
    rows: &[LineDetail],
    grouped: &BTreeSet<String>,
    parent_of: &BTreeMap<String, Option<String>>,
    labels: &BTreeMap<String, (String, String)>,
) -> Vec<LineDetail> {
    if grouped.is_empty() {
        return rows.to_vec();
    }
    let mut out: Vec<LineDetail> = Vec::with_capacity(rows.len());
    // Where each group's row is in `out`, and whether anything under the parent
    // has been folded into it.
    let mut groups: BTreeMap<String, (usize, bool)> = BTreeMap::new();

    for row in rows {
        let mut outermost = None;
        let mut cursor = Some(row.account_id.clone());
        for _ in 0..MAX_TREE_DEPTH {
            let Some(id) = cursor else { break };
            if grouped.contains(&id) {
                outermost = Some(id.clone());
            }
            cursor = parent_of.get(&id).cloned().flatten();
        }
        let Some(parent) = outermost else {
            out.push(row.clone());
            continue;
        };
        let is_child = parent != row.account_id;
        match groups.get_mut(&parent) {
            Some((i, has_children)) => {
                out[*i].cents += row.cents;
                *has_children |= is_child;
            }
            None => {
                let (account_number, account_name) = labels
                    .get(&parent)
                    .cloned()
                    .unwrap_or_else(|| (row.account_number.clone(), row.account_name.clone()));
                groups.insert(parent.clone(), (out.len(), is_child));
                out.push(LineDetail {
                    account_id: parent,
                    account_number,
                    account_name,
                    cents: row.cents,
                });
            }
        }
    }

    for (i, has_children) in groups.into_values() {
        if has_children {
            out[i].account_name.push_str(WITH_CHILDREN);
        }
    }
    out
}

/// Split a balance into the part that is deducted and the part that is not.
///
/// Rounding is done once, on the deductible part, and the remainder is whatever
/// is left — so the two halves always add back to the balance exactly. Rounding
/// both independently is how a return ends up a cent out from its own books.
pub fn split_deductible(balance: i64, pct: u8) -> (i64, i64) {
    let pct = pct.min(100) as i64;
    // Round half away from zero, so a negative balance (a credit sitting in an
    // expense account) splits the same way its positive twin would.
    let scaled = balance * pct;
    let allowed = if scaled >= 0 {
        (scaled + 50) / 100
    } else {
        (scaled - 50) / 100
    };
    (allowed, balance - allowed)
}

/// Every saved mapping, expanded down the account tree — what the forms ask.
///
/// [`load_mapping`] is what a *mapping editor* wants: the choices somebody
/// actually made. This is what a *return* wants: the line every account ends up
/// on once those choices are inherited.
pub fn load_effective_mapping(conn: &Connection, year: i32) -> BTreeMap<String, String> {
    load_effective_mapping_for(conn, ReturnForm::Form1065, year)
}

/// [`load_effective_mapping`] for one return's assignments.
///
/// Each return is inherited through the tree on its own: a child carrying a
/// Form 1065 line must not stop a parent's Schedule C line reaching it, which it
/// did while both returns shared one row per account.
pub fn load_effective_mapping_for(
    conn: &Connection,
    form: ReturnForm,
    year: i32,
) -> BTreeMap<String, String> {
    let mut mapping =
        inherit_through_tree(&load_mapping_for(conn, form, year), &load_parents(conn));
    // The non-taxable fence, applied here and not left to the mapping table
    // (INVESTMENTS-SPEC.md §8, phase 2).
    //
    // Growth inside a sheltered account is not income to anybody, ever. It
    // reaching a tax line is not a cosmetic bug — it is a filed error, tax paid
    // on money the statute exempts, on a return somebody signed. So the exclusion
    // is enforced where every return reads the mapping, rather than trusted to the
    // three things that could each let one through on their own:
    //
    //   * **inheritance**, which is the sneaky one. Nobody need map the
    //     value-change account at all: mapping its parent `Income:Investments` to
    //     line 7 is enough, and the child inherits it silently. That is why this
    //     runs *after* `inherit_through_tree` and not before.
    //   * **a mapping that predates the register**, written while the account was
    //     an ordinary income account and still sitting in the table afterwards.
    //   * **a row that arrived some other way** — over the sync transport from a
    //     machine running older code, or out of a hand-edited database.
    //
    // `OFF_RETURN` rather than removal, because removal means "nobody has ruled on
    // this yet" and makes `compute` warn about an account carrying a balance,
    // every year, forever. Somebody did rule on it: the statute did.
    for account_id in crate::commands::retirement_commands::value_change_account_ids(conn) {
        mapping.insert(account_id, OFF_RETURN.to_string());
    }
    mapping
}

/// Every saved Form 1065 mapping as account id → line key, exactly as stored.
pub fn load_mapping(conn: &Connection, year: i32) -> BTreeMap<String, String> {
    load_mapping_for(conn, ReturnForm::Form1065, year)
}

/// One return's saved mappings as account id → line key, exactly as stored.
pub fn load_mapping_for(
    conn: &Connection,
    form: ReturnForm,
    year: i32,
) -> BTreeMap<String, String> {
    load_dated_where(
        conn,
        "tax_line_mappings",
        "line_key",
        year,
        Some(("form", form.as_str())),
    )
}

/// The rows of a dated assignment table that are in force for a tax year.
///
/// # How a year picks its row
///
/// The greatest `effective_from` at or before the year. So an assignment made in
/// 2026 does not reach back into a 2023 return that was filed on the old one,
/// and a year with no assignment of its own inherits the most recent earlier one
/// rather than starting with an unmapped chart. `effective_from = 0` is the
/// sentinel for an assignment made before assignments were dated: it is at or
/// before every year, so books that predate the change behave exactly as they
/// did.
fn load_dated(
    conn: &Connection,
    table: &str,
    value_column: &str,
    year: i32,
) -> BTreeMap<String, String> {
    load_dated_where(conn, table, value_column, year, None)
}

/// [`load_dated`], narrowed to the rows whose `column` holds `value` — the
/// resolution by year happening within that slice, so one return's assignment
/// for 2026 never hides another return's from 2024.
fn load_dated_where(
    conn: &Connection,
    table: &str,
    value_column: &str,
    year: i32,
    filter: Option<(&str, &str)>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let (outer, inner) = match filter {
        Some((column, _)) => (
            format!("AND t.{column} = ?2"),
            format!("AND u.{column} = t.{column}"),
        ),
        None => (String::new(), String::new()),
    };
    // `CAST(... AS TEXT)` because one of the two value columns is an INTEGER,
    // and reading an integer as a string makes `query_map` yield an error per
    // row — which `flatten` then drops, so every deduction limit silently
    // vanished rather than failing loudly.
    let sql = format!(
        "SELECT account_id, CAST({value_column} AS TEXT) FROM {table} t
          WHERE effective_from <= ?1 {outer}
            AND effective_from = (
                SELECT MAX(effective_from) FROM {table} u
                 WHERE u.account_id = t.account_id AND u.effective_from <= ?1 {inner}
            )"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let read = |r: &rusqlite::Row<'_>| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?));
    let rows = match filter {
        Some((_, value)) => stmt.query_map(rusqlite::params![year, value], read),
        None => stmt.query_map(rusqlite::params![year], read),
    };
    let Ok(rows) = rows else {
        return out;
    };
    for (account_id, value) in rows.flatten() {
        out.insert(account_id, value);
    }
    out
}

/// Where the assignment in force for a year came from.
///
/// The desktop needs this to say "set here" rather than "inherited from 2024" —
/// and a reviewer needs it to see at a glance that the year they are looking at
/// is one somebody actually decided, rather than one that drifted in from a
/// neighbour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Assigned for this very year.
    SetHere,
    /// Inherited from an assignment made in an earlier year.
    From(i32),
    /// Made before assignments were dated, so it applies to every year.
    Undated,
    /// No assignment at any year at or before this one.
    None,
}

/// Where an account's assignment for a year came from.
pub fn provenance(conn: &Connection, account_id: &str, year: i32) -> Provenance {
    provenance_for(conn, ReturnForm::Form1065, account_id, year)
}

/// [`provenance`] for one return's assignments.
pub fn provenance_for(
    conn: &Connection,
    form: ReturnForm,
    account_id: &str,
    year: i32,
) -> Provenance {
    let years = assignment_years_for(conn, form, account_id);
    match years.iter().filter(|y| **y <= year).max() {
        None => Provenance::None,
        Some(&y) if y == year => Provenance::SetHere,
        Some(&y) if y == crate::events::types::ANY_YEAR => Provenance::Undated,
        Some(&y) => Provenance::From(y),
    }
}

/// Every year an assignment was made in, for one account, oldest first.
///
/// What the desktop needs to say "set here" rather than "inherited from 2024",
/// and what a reviewer needs to see that a filed year has not been disturbed.
pub fn assignment_years(conn: &Connection, account_id: &str) -> Vec<i32> {
    assignment_years_for(conn, ReturnForm::Form1065, account_id)
}

/// [`assignment_years`] for one return's assignments.
pub fn assignment_years_for(conn: &Connection, form: ReturnForm, account_id: &str) -> Vec<i32> {
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT effective_from FROM tax_line_mappings
          WHERE account_id = ?1 AND form = ?2 ORDER BY effective_from",
    ) {
        if let Ok(rows) = stmt.query_map([account_id, form.as_str()], |r| r.get::<_, i32>(0)) {
            out.extend(rows.flatten());
        }
    }
    out
}

#[derive(Debug, thiserror::Error)]
pub enum MappingError {
    #[error("No Form 1065 line has key {0:?}")]
    UnknownLine(String),
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
}

/// Point an account at a line, writing straight to the table.
///
/// **Not the command path.** Since migration 027 the mapping is event-sourced, so
/// the way to change it is
/// [`crate::commands::tax_setup_commands::set_account_line`], which appends an
/// event that reaches every member. This remains only for the projector and for
/// tests that build a mapping without a log; calling it from a UI writes a row
/// that the next rebuild deletes and that no colleague ever sees.
///
/// The key is checked against [`MAPPABLE_LINES`] here rather than trusted,
/// because a row with a key nothing recognises is an account whose balance
/// quietly stops reaching the return.
pub fn set_account_line(
    conn: &Connection,
    account_id: &str,
    line_key: &str,
    effective_from: i32,
) -> Result<(), MappingError> {
    if line_def(line_key).is_none() {
        return Err(MappingError::UnknownLine(line_key.to_string()));
    }
    conn.execute(
        "INSERT INTO tax_line_mappings (account_id, form, effective_from, line_key, updated_at)
         VALUES (?1, '1065', ?2, ?3, datetime('now'))
         ON CONFLICT(account_id, form, effective_from)
           DO UPDATE SET line_key = ?3, updated_at = datetime('now')",
        rusqlite::params![account_id, effective_from, line_key],
    )?;
    Ok(())
}

/// Take an account off the return, writing straight to the table.
///
/// **Not the command path** — see [`set_account_line`].
pub fn clear_account_line(
    conn: &Connection,
    account_id: &str,
    effective_from: i32,
) -> Result<(), MappingError> {
    conn.execute(
        "DELETE FROM tax_line_mappings
          WHERE account_id = ?1 AND form = '1065' AND effective_from = ?2",
        rusqlite::params![account_id, effective_from],
    )?;
    Ok(())
}

#[cfg(test)]
mod inheritance_tests {
    use super::*;

    fn tree(pairs: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        pairs
            .iter()
            .map(|(id, parent)| (id.to_string(), parent.map(str::to_string)))
            .collect()
    }

    fn stored(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(id, key)| (id.to_string(), key.to_string()))
            .collect()
    }

    /// The point of the feature: say it once on the parent.
    #[test]
    fn a_child_takes_its_parents_line() {
        let t = tree(&[
            ("expenses", None),
            ("software", Some("expenses")),
            ("canva", Some("software")),
        ]);
        let e = inherit_through_tree(&stored(&[("software", "l20")]), &t);
        assert_eq!(e.get("canva").map(String::as_str), Some("l20"));
        assert_eq!(e.get("software").map(String::as_str), Some("l20"));
        // Nothing above the mapped account is dragged onto the line with it.
        assert_eq!(e.get("expenses"), None);
    }

    /// The nearest answer is the most specific one, so a child that says
    /// something else says it for itself and not for its parent.
    #[test]
    fn the_nearest_mapped_ancestor_wins() {
        let t = tree(&[
            ("expenses", None),
            ("software", Some("expenses")),
            ("canva", Some("software")),
        ]);
        let e = inherit_through_tree(&stored(&[("expenses", "l20"), ("software", "l13")]), &t);
        assert_eq!(
            e.get("canva").map(String::as_str),
            Some("l13"),
            "nearest, not topmost"
        );
        assert_eq!(e.get("software").map(String::as_str), Some("l13"));
        assert_eq!(e.get("expenses").map(String::as_str), Some("l20"));
    }

    /// Without this there is no way out of an inherited line, and mapping a
    /// parent becomes a trap.
    #[test]
    fn a_child_can_be_held_off_a_line_its_parent_is_on() {
        let t = tree(&[("expenses", None), ("personal", Some("expenses"))]);
        let e = inherit_through_tree(
            &stored(&[("expenses", "l20"), ("personal", OFF_RETURN)]),
            &t,
        );
        assert_eq!(e.get("personal").map(String::as_str), Some(OFF_RETURN));
    }

    /// An account under nothing mapped is still simply unmapped, which is the
    /// state the page warns about.
    #[test]
    fn an_account_under_no_mapped_ancestor_stays_unmapped() {
        let t = tree(&[("expenses", None), ("rent", Some("expenses"))]);
        let e = inherit_through_tree(&stored(&[]), &t);
        assert!(e.is_empty());
    }

    /// A corrupt `parent_id` must not hang the tax page.
    #[test]
    fn a_cycle_in_the_tree_terminates() {
        let t = tree(&[("a", Some("b")), ("b", Some("a"))]);
        let e = inherit_through_tree(&stored(&[]), &t);
        assert!(e.is_empty());
    }

    /// A mapping whose account the tree does not know about is kept, not
    /// dropped — losing one silently takes money off the return.
    #[test]
    fn a_mapping_for_an_unknown_account_survives() {
        let e = inherit_through_tree(&stored(&[("ghost", "l20")]), &tree(&[("a", None)]));
        assert_eq!(e.get("ghost").map(String::as_str), Some("l20"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every line in the catalogue must name a box the vendored form actually
    /// has. This is the check that catches a revision renumbering the schedules:
    /// without it, a moved box is a figure written into whichever field inherited
    /// the old name, on a return that still adds up.
    #[test]
    fn every_line_names_a_box_the_form_has() {
        use crate::tax::acroform::{field_map, strip_xfa};
        let mut doc = lopdf::Document::load_mem(include_bytes!("../../assets/irs/f1065.pdf"))
            .expect("the vendored 1065 loads");
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        for def in MAPPABLE_LINES {
            match def.field {
                Field::One(f) => assert!(
                    map.find(f).is_some(),
                    "{:?} line {} names {f}, which the form does not have",
                    def.schedule,
                    def.number
                ),
                Field::Period { begin, end } => {
                    assert!(
                        map.find(begin).is_some(),
                        "{:?} line {} names {begin} for the opening column, which the form does not have",
                        def.schedule,
                        def.number
                    );
                    assert!(
                        map.find(end).is_some(),
                        "{:?} line {} names {end} for the closing column, which the form does not have",
                        def.schedule,
                        def.number
                    );
                }
            }
        }
    }

    /// Two lines sharing a key would make one mapping mean two places; two
    /// sharing a box would make one figure overwrite another.
    #[test]
    fn line_keys_and_boxes_are_unique() {
        let mut keys = std::collections::HashSet::new();
        let mut boxes = std::collections::HashSet::new();
        for def in MAPPABLE_LINES {
            assert!(keys.insert(def.key), "duplicate line key {}", def.key);
            match def.field {
                Field::One(f) => {
                    assert!(boxes.insert(f), "duplicate box {f} at line {}", def.number)
                }
                Field::Period { begin, end } => {
                    assert!(
                        boxes.insert(begin),
                        "duplicate box {begin} at line {}",
                        def.number
                    );
                    assert!(
                        boxes.insert(end),
                        "duplicate box {end} at line {}",
                        def.number
                    );
                }
            }
        }
    }

    /// Page one and Schedule K are totalled from the income statement; Schedule L
    /// is a position on two dates and must never be. A line that drifted into the
    /// wrong schedule would report a year's movement in cash as the cash on hand.
    #[test]
    fn only_the_profit_and_loss_schedules_are_totalled_from_activity() {
        for def in MAPPABLE_LINES {
            let period = matches!(def.field, Field::Period { .. });
            assert_eq!(
                period,
                !def.schedule.from_income_statement(),
                "line {} {:?} mixes a period box with an activity schedule",
                def.number,
                def.schedule
            );
        }
    }

    /// Every line has to say what belongs on it. An empty tooltip is a line a
    /// preparer has to guess at, and guessing is what puts a charitable
    /// contribution in "other deductions".
    #[test]
    fn every_line_explains_itself() {
        for def in MAPPABLE_LINES {
            assert!(
                def.instructions.len() > 40,
                "line {} {:?} has no useful instructions",
                def.number,
                def.schedule
            );
            if let Some(a) = def.attachment {
                assert!(
                    a.url.starts_with("https://www.irs.gov/"),
                    "line {} attachment {} has a non-IRS url",
                    def.number,
                    a.name
                );
            }
        }
    }
    use crate::queries::reports::{IncomeStatementLine, IncomeStatementSection};
    use chrono::NaiveDate;

    fn line(id: &str, number: &str, name: &str, balance: i64) -> IncomeStatementLine {
        IncomeStatementLine {
            account_id: id.into(),
            account_number: number.into(),
            account_name: name.into(),
            parent_id: None,
            balance,
        }
    }

    fn statement(
        revenue: Vec<IncomeStatementLine>,
        expenses: Vec<IncomeStatementLine>,
    ) -> IncomeStatement {
        let rt = revenue.iter().map(|l| l.balance).sum();
        let et = expenses.iter().map(|l| l.balance).sum();
        IncomeStatement {
            start_date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
            end_date: NaiveDate::from_ymd_opt(2025, 12, 31).unwrap(),
            revenue: IncomeStatementSection {
                name: "Revenue".into(),
                lines: revenue,
                total: rt,
            },
            expenses: IncomeStatementSection {
                name: "Expenses".into(),
                lines: expenses,
                total: et,
            },
            net_income: rt - et,
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect()
    }

    fn limits(pairs: &[(&str, u8)]) -> BTreeMap<String, u8> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// The two halves must add back to the balance exactly. Rounding each
    /// independently is how a return ends up a cent away from its own books.
    #[test]
    fn a_limited_deduction_splits_without_losing_a_cent() {
        for balance in [0, 1, 99, 100, 101, 12_345, 1_320_91, -4_567, -1] {
            for pct in [0u8, 25, 50, 75, 100] {
                let (allowed, disallowed) = split_deductible(balance, pct);
                assert_eq!(
                    allowed + disallowed,
                    balance,
                    "{balance} at {pct}% split into {allowed} + {disallowed}"
                );
            }
        }
        // An odd cent rounds away from zero, and the same way for a credit.
        assert_eq!(split_deductible(101, 50), (51, 50));
        assert_eq!(split_deductible(-101, 50), (-51, -50));
        assert_eq!(split_deductible(1_320_91, 50), (66_046, 66_045));
        // The ends are exact.
        assert_eq!(split_deductible(999, 100), (999, 0));
        assert_eq!(split_deductible(999, 0), (0, 999));
    }

    /// The meals case, end to end: half is deducted on the line it is mapped
    /// to, and the other half is *reported* on Schedule K line 18c rather than
    /// disappearing. Dropping it would overstate every partner's basis.
    #[test]
    fn the_disallowed_half_of_a_meal_reaches_the_nondeductible_line() {
        let s = statement(vec![], vec![line("meals", "3045", "Staff meals", 1_320_91)]);
        let m = map(&[("meals", "l21")]);
        let c = compute(&s, &m, &limits(&[("meals", 50)]));

        assert_eq!(c.lines.get("l21"), 660, "660.46 rounds to 660");
        assert_eq!(
            c.lines.get(NONDEDUCTIBLE_LINE),
            660,
            "the disallowed half is reported, not dropped"
        );
        // And it is attributed, so the line 18c statement can say what it was.
        let detail = c.detail.get(NONDEDUCTIBLE_LINE).expect("18c has detail");
        assert!(
            detail
                .iter()
                .any(|d| d.account_name.contains("50% disallowed")),
            "{detail:?}"
        );
    }

    /// No limit means no split and nothing on 18c — the overwhelmingly common
    /// case has to stay exactly as it was.
    #[test]
    fn an_unlimited_account_is_untouched_and_reports_nothing_as_nondeductible() {
        let s = statement(vec![], vec![line("rent", "3002", "Rent", 100_000)]);
        let c = compute(&s, &map(&[("rent", "l13")]), &BTreeMap::new());
        assert_eq!(c.lines.get("l13"), 1_000);
        assert_eq!(c.lines.get(NONDEDUCTIBLE_LINE), 0);
    }

    /// A limit set on a parent is a statement about everything under it, in the
    /// same way a line assignment is.
    #[test]
    fn a_deduction_limit_inherits_down_the_tree() {
        let tree = [
            ("meals".to_string(), None),
            ("lunches".to_string(), Some("meals".to_string())),
            ("parties".to_string(), Some("meals".to_string())),
        ]
        .into_iter()
        .collect();
        let stored = [("meals".to_string(), "50".to_string())]
            .into_iter()
            .collect();
        let effective = inherit_through_tree(&stored, &tree);
        assert_eq!(effective.get("lunches").map(String::as_str), Some("50"));

        // …and a child that is limited differently wins, being the nearer answer.
        let stored = [
            ("meals".to_string(), "50".to_string()),
            ("parties".to_string(), "100".to_string()),
        ]
        .into_iter()
        .collect();
        let effective = inherit_through_tree(&stored, &tree);
        assert_eq!(effective.get("parties").map(String::as_str), Some("100"));
        assert_eq!(effective.get("lunches").map(String::as_str), Some("50"));
    }

    /// The property the whole module exists for: what the reader adds up on the
    /// printed page must be what the printed page says.
    #[test]
    fn the_totals_on_the_page_are_the_arithmetic_of_the_lines_on_the_page() {
        // Cents chosen so that every line rounds, and so that summing the cents
        // first and summing the rounded dollars first give different answers —
        // if the code ever rounds in the wrong order, this catches it.
        let s = statement(
            vec![
                line("a1", "4000", "Sales", 10_050),
                line("a2", "4100", "Consulting", 20_050),
            ],
            vec![
                line("b1", "6000", "Wages", 3_050),
                line("b2", "6100", "Rent", 1_050),
            ],
        );
        let m = map(&[("a1", "l1a"), ("a2", "l7"), ("b1", "l9"), ("b2", "l13")]);
        let c = compute(&s, &m, &BTreeMap::new());
        let l = &c.lines;

        assert_eq!(l.get("l1a"), 101, "100.50 rounds away from zero");
        assert_eq!(l.get("l7"), 201);
        assert_eq!(l.get("l9"), 31);
        assert_eq!(l.get("l13"), 11);

        // Every derived line, recomputed by hand from the *printed* figures.
        assert_eq!(l.line_1c(), 101 - 0);
        assert_eq!(l.line_3(), 101);
        assert_eq!(l.line_8(), 101 + 201);
        assert_eq!(l.line_22(), 31 + 11);
        assert_eq!(l.line_23(), 302 - 42);

        // And the identity the form itself asserts.
        assert_eq!(l.line_23(), l.line_8() - l.line_22());
        assert_eq!(
            l.line_8(),
            l.line_3() + l.get("l4") + l.get("l5") + l.get("l6") + l.get("l7")
        );
    }

    /// Rounding each account before summing accumulates a cent per account. This
    /// is the same figures totalled the right way and the wrong way.
    #[test]
    fn a_line_is_rounded_once_not_once_per_account() {
        // Four accounts at 50 cents each: rounded individually they are 4 × $1;
        // summed first they are $2.
        let s = statement(
            vec![],
            vec![
                line("b1", "6000", "A", 50),
                line("b2", "6001", "B", 50),
                line("b3", "6002", "C", 50),
                line("b4", "6003", "D", 50),
            ],
        );
        let m = map(&[("b1", "l21"), ("b2", "l21"), ("b3", "l21"), ("b4", "l21")]);
        let c = compute(&s, &m, &BTreeMap::new());
        assert_eq!(
            c.lines.get("l21"),
            2,
            "$2.00 of expense must not become $4 by rounding each account"
        );
    }

    /// A contra-revenue account carries a debit balance, so it arrives negative.
    /// Line 1b prints the figure that is *taken away*, which is positive.
    #[test]
    fn a_less_line_prints_the_amount_it_takes_away_as_a_positive_figure() {
        let s = statement(
            vec![
                line("a1", "4000", "Sales", 100_000),
                line("a2", "4900", "Refunds", -5_000),
            ],
            vec![],
        );
        let m = map(&[("a1", "l1a"), ("a2", "l1b")]);
        let c = compute(&s, &m, &BTreeMap::new());

        assert_eq!(c.lines.get("l1a"), 1000);
        assert_eq!(
            c.lines.get("l1b"),
            50,
            "refunds print positive on a 'less' line"
        );
        assert_eq!(c.lines.line_1c(), 950, "and are subtracted, not added");
    }

    /// 16b is the depreciation already taken through cost of goods sold.
    /// Line 22 must use 16c or that depreciation is deducted twice.
    #[test]
    fn depreciation_reported_elsewhere_is_not_deducted_a_second_time() {
        let s = statement(
            vec![],
            vec![
                line("b1", "6500", "Depreciation", 10_000),
                line("b2", "6501", "Depreciation in COGS", -4_000),
            ],
        );
        let m = map(&[("b1", "l16a"), ("b2", "l16b")]);
        let c = compute(&s, &m, &BTreeMap::new());

        assert_eq!(c.lines.get("l16a"), 100);
        assert_eq!(c.lines.get("l16b"), 40);
        assert_eq!(c.lines.line_16c(), 60);
        assert_eq!(
            c.lines.line_22(),
            60,
            "line 22 takes 16c; taking 16a would deduct the COGS share twice"
        );
    }

    /// An unmapped account is money leaving the return without trace. It must be
    /// named, and named with its balance, or nobody can find it.
    #[test]
    fn an_account_with_a_balance_and_no_line_is_named_in_the_warnings() {
        let s = statement(
            vec![line("a1", "4000", "Sales", 100_000)],
            vec![line("b9", "6999", "Mystery expense", 12_345)],
        );
        let m = map(&[("a1", "l1a")]);
        let c = compute(&s, &m, &BTreeMap::new());

        let joined = c.warnings.join(" ");
        assert!(joined.contains("6999"), "the account number: {joined}");
        assert!(joined.contains("Mystery expense"), "and its name: {joined}");
        assert!(joined.contains("123"), "and its balance: {joined}");
        assert!(
            !c.lines.is_mapped("l21"),
            "an unmapped account must not be quietly swept into other deductions"
        );
    }

    /// Backfilled ancestors arrive with a zero balance and are not a problem to
    /// report — warning about them would bury the accounts that are.
    #[test]
    fn an_account_with_no_balance_is_not_reported_as_missing() {
        let s = statement(
            vec![
                line("a1", "4000", "Sales", 100_000),
                line("parent", "4", "Revenue", 0),
            ],
            vec![],
        );
        let m = map(&[("a1", "l1a")]);
        let c = compute(&s, &m, &BTreeMap::new());
        assert!(
            c.warnings.is_empty(),
            "a zero-balance ancestor is not missing money: {:?}",
            c.warnings
        );
    }

    #[test]
    fn a_mapping_naming_a_line_the_form_does_not_have_is_reported_not_ignored() {
        let s = statement(vec![line("a1", "4000", "Sales", 100_000)], vec![]);
        let m = map(&[("a1", "l99")]);
        let c = compute(&s, &m, &BTreeMap::new());

        assert!(c.lines.is_empty(), "nothing was reportable");
        let joined = c.warnings.join(" ");
        assert!(joined.contains("l99"), "got {joined}");
        assert!(
            joined.contains("4000"),
            "and the account is still missing money: {joined}"
        );
    }

    #[test]
    fn rounding_is_away_from_zero_and_symmetric_about_it() {
        assert_eq!(cents_to_dollars(150), 2);
        assert_eq!(cents_to_dollars(-150), -2, "a loss rounds like a profit");
        assert_eq!(cents_to_dollars(149), 1);
        assert_eq!(cents_to_dollars(-149), -1);
        assert_eq!(cents_to_dollars(0), 0);
        assert_eq!(cents_to_dollars(50), 1);
        assert_eq!(cents_to_dollars(-50), -1);
    }

    /// A loss is a real outcome and must survive to the bottom line.
    #[test]
    fn a_partnership_that_lost_money_reports_a_negative_line_23() {
        let s = statement(
            vec![line("a1", "4000", "Sales", 100_000)],
            vec![line("b1", "6000", "Wages", 250_000)],
        );
        let m = map(&[("a1", "l1a"), ("b1", "l9")]);
        let c = compute(&s, &m, &BTreeMap::new());

        assert_eq!(c.lines.line_8(), 1000);
        assert_eq!(c.lines.line_22(), 2500);
        assert_eq!(c.lines.line_23(), -1500);
        assert_eq!(format_dollars(c.lines.line_23()), "-1,500");
    }

    /// Every figure on the return and on every statement comes through here, so
    /// the grouping, the sign and the boundary cases are worth pinning.
    #[test]
    fn figures_are_grouped_in_threes_and_keep_their_sign() {
        assert_eq!(format_dollars(0), "0");
        assert_eq!(format_dollars(7), "7");
        assert_eq!(format_dollars(999), "999");
        assert_eq!(format_dollars(1_000), "1,000");
        assert_eq!(format_dollars(12_345), "12,345");
        assert_eq!(format_dollars(999_999), "999,999");
        assert_eq!(format_dollars(1_234_567), "1,234,567");
        assert_eq!(format_dollars(-1_500), "-1,500");
        assert_eq!(format_dollars(-999), "-999");
        // A loss big enough to matter still reads as one number.
        assert_eq!(format_dollars(-12_345_678), "-12,345,678");
        // `unsigned_abs`, not `-x`: negating this would overflow.
        assert_eq!(format_dollars(i64::MIN).starts_with('-'), true);
    }

    #[test]
    fn every_mappable_line_key_is_unique_and_resolvable() {
        let mut seen = BTreeSet::new();
        for def in MAPPABLE_LINES {
            assert!(seen.insert(def.key), "duplicate line key {}", def.key);
            assert!(line_def(def.key).is_some());
            // The group names the block the form prints the line under, so the
            // set depends on which schedule the line is on. Asserted per
            // schedule rather than against one flat list: a Schedule L line
            // labelled "Income" is a line in the wrong table, and a flat list
            // would accept it.
            let allowed: &[&str] = match def.schedule {
                Schedule::Page1 => &["Income", "Deductions"],
                Schedule::K => &[
                    "Income (loss)",
                    "Deductions",
                    "Self-employment",
                    "Other information",
                ],
                Schedule::L => &["Assets", "Liabilities and capital"],
            };
            assert!(
                allowed.contains(&def.group),
                "{} is on {:?} but has group {}",
                def.key,
                def.schedule,
                def.group
            );
        }
    }

    /// Provenance tells a year that decided something from a year that inherited.
    #[test]
    fn provenance_says_whether_a_year_decided_or_inherited() {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::migrations::init_schema(&conn).unwrap();
        set_account_line(&conn, "6100", "l21", 2023).unwrap();
        set_account_line(&conn, "6100", "l11", 2026).unwrap();

        assert_eq!(provenance(&conn, "6100", 2022), Provenance::None);
        assert_eq!(provenance(&conn, "6100", 2023), Provenance::SetHere);
        assert_eq!(provenance(&conn, "6100", 2024), Provenance::From(2023));
        assert_eq!(provenance(&conn, "6100", 2026), Provenance::SetHere);
        assert_eq!(provenance(&conn, "6100", 2027), Provenance::From(2026));
        assert_eq!(provenance(&conn, "9999", 2025), Provenance::None);
    }

    /// An assignment made before assignments were dated says so, rather than
    /// claiming to come from "year 0".
    #[test]
    fn an_undated_assignment_is_named_as_such() {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::migrations::init_schema(&conn).unwrap();
        set_account_line(&conn, "6100", "l21", crate::events::types::ANY_YEAR).unwrap();

        assert_eq!(provenance(&conn, "6100", 2025), Provenance::Undated);
        // A dated assignment then takes over from its own year.
        set_account_line(&conn, "6100", "l11", 2026).unwrap();
        assert_eq!(provenance(&conn, "6100", 2025), Provenance::Undated);
        assert_eq!(provenance(&conn, "6100", 2026), Provenance::SetHere);
    }
}

/// Form 1065 page one, across a year-end close.
///
/// The assertion that matters most in the whole closing feature. Everything else
/// a bad close could do is recoverable; a return built on a year that reports
/// itself as having earned nothing is filed, and then it is the IRS's problem
/// and the partners'.
///
/// A closing entry is dated inside the year it closes and debits every revenue
/// account. If the income statement counted it, line 8 and line 22 would both
/// collapse to zero and line 23 with them — and the form would still foot, still
/// print, and still look like a return.
#[cfg(test)]
mod closing_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::closing_commands::{close_books, CloseBooksCommand, ClosingTarget};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::AccountType;
    use crate::events::types::JournalEntrySource;
    use crate::queries::reports::Reports;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use chrono::NaiveDate;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Gross receipts of 500,000 against salaries of 120,000 and rent of 80,000,
    /// so line 8 is 500,000, line 22 is 200,000 and line 23 is 300,000.
    fn books_for_2023() -> (EventStore, BTreeMap<String, String>, String) {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let mut store = store;
        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES ('c', 'c', 'Co', 'USD', 1)",
                [],
            )
            .unwrap();

        for (ty, number, name) in [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Revenue, "4000", "Gross receipts"),
            (AccountType::Expense, "6000", "Salaries"),
            (AccountType::Expense, "6100", "Rent"),
            (AccountType::Equity, "3023", "2023"),
        ] {
            AccountCommands::new(&mut store, "user".to_string())
                .create_account(CreateAccountCommand {
                    account_type: ty,
                    account_number: number.to_string(),
                    name: name.to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
        }
        let id = |store: &EventStore, n: &str| -> String {
            store
                .connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [n],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let (cash, receipts, salaries, rent, equity) = (
            id(&store, "1000"),
            id(&store, "4000"),
            id(&store, "6000"),
            id(&store, "6100"),
            id(&store, "3023"),
        );

        let mut post = |date: NaiveDate, debit: &str, credit: &str, cents: i64| {
            EntryCommands::new(&mut store, "user".to_string())
                .post_entry(PostEntryCommand {
                    date,
                    memo: "test".to_string(),
                    lines: vec![
                        EntryLine::debit(debit, cents, "USD"),
                        EntryLine::credit(credit, cents, "USD"),
                    ],
                    reference: None,
                    source: Some(JournalEntrySource::Manual),
                })
                .unwrap();
        };
        post(day(2023, 4, 1), &cash, &receipts, 50_000_000);
        post(day(2023, 7, 1), &salaries, &cash, 12_000_000);
        post(day(2023, 10, 1), &rent, &cash, 8_000_000);

        // 1a gross receipts; 9 salaries and wages; 13 rent.
        let mapping: BTreeMap<String, String> = [
            (receipts, "l1a".to_string()),
            (salaries, "l9".to_string()),
            (rent, "l13".to_string()),
        ]
        .into_iter()
        .collect();

        (store, mapping, equity)
    }

    fn page_one(store: &EventStore, mapping: &BTreeMap<String, String>) -> (i64, i64, i64) {
        let statement = Reports::new(store.connection())
            .income_statement(day(2023, 1, 1), day(2023, 12, 31))
            .unwrap();
        let computed = compute(&statement, mapping, &BTreeMap::new());
        (
            computed.lines.line_8(),
            computed.lines.line_22(),
            computed.lines.line_23(),
        )
    }

    #[test]
    fn closing_2023_does_not_change_the_2023_return() {
        let (mut store, mapping, equity) = books_for_2023();

        let before = page_one(&store, &mapping);
        assert_eq!(
            before,
            (500_000, 200_000, 300_000),
            "the fixture should produce a return worth protecting"
        );

        close_books(
            &mut store,
            "user",
            CloseBooksCommand {
                year: 2023,
                target: ClosingTarget::Account(equity),
                include_draws: false,
            },
        )
        .unwrap();

        let after = page_one(&store, &mapping);
        assert_eq!(
            after, before,
            "closing the books must leave the filed figures exactly as they were"
        );
    }
}

#[cfg(test)]
mod statement_group_tests {
    use super::*;

    fn stmt_row(id: &str, cents: i64) -> LineDetail {
        LineDetail {
            account_id: id.into(),
            account_number: format!("n{id}"),
            account_name: format!("row {id}"),
            cents,
        }
    }

    /// Office (P) holds Supplies (C1) and Postage (C2); Paper (G) sits under
    /// Supplies. Travel (U) is unrelated.
    fn tree() -> (
        BTreeMap<String, Option<String>>,
        BTreeMap<String, (String, String)>,
    ) {
        let parents = [
            ("P", None),
            ("C1", Some("P")),
            ("C2", Some("P")),
            ("G", Some("C1")),
            ("U", None),
        ]
        .into_iter()
        .map(|(id, p)| (id.to_string(), p.map(str::to_string)))
        .collect();
        let labels = [("P", "6000", "Office"), ("C1", "6010", "Supplies")]
            .into_iter()
            .map(|(id, n, name)| (id.to_string(), (n.to_string(), name.to_string())))
            .collect();
        (parents, labels)
    }

    fn rows() -> Vec<LineDetail> {
        vec![
            stmt_row("C1", 100),
            stmt_row("G", 5),
            stmt_row("U", 7),
            stmt_row("C2", 20),
        ]
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn total(rows: &[LineDetail]) -> i64 {
        rows.iter().map(|r| r.cents).sum()
    }

    #[test]
    fn nothing_grouped_leaves_the_rows_alone() {
        let (parents, labels) = tree();
        assert_eq!(
            group_statement_rows(&rows(), &set(&[]), &parents, &labels),
            rows()
        );
    }

    #[test]
    fn a_grouped_parent_takes_its_whole_subtree_under_its_own_name() {
        let (parents, labels) = tree();
        let out = group_statement_rows(&rows(), &set(&["P"]), &parents, &labels);
        let shown: Vec<(&str, &str, i64)> = out
            .iter()
            .map(|r| (r.account_number.as_str(), r.account_name.as_str(), r.cents))
            .collect();
        assert_eq!(
            shown,
            vec![("6000", "Office (with children)", 125), ("nU", "row U", 7)]
        );
        assert_eq!(total(&out), total(&rows()), "grouping moved the total");
    }

    #[test]
    fn a_grouped_child_groups_only_beneath_itself() {
        let (parents, labels) = tree();
        let out = group_statement_rows(&rows(), &set(&["C1"]), &parents, &labels);
        let shown: Vec<(&str, i64)> = out
            .iter()
            .map(|r| (r.account_name.as_str(), r.cents))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("Supplies (with children)", 105),
                ("row U", 7),
                ("row C2", 20)
            ]
        );
    }

    #[test]
    fn the_outermost_grouped_ancestor_wins() {
        let (parents, labels) = tree();
        assert_eq!(
            group_statement_rows(&rows(), &set(&["P", "C1"]), &parents, &labels),
            group_statement_rows(&rows(), &set(&["P"]), &parents, &labels)
        );
    }

    /// A parent that posted to itself and has nothing beneath it on this line is
    /// not a subtotal, and is not labelled as one.
    #[test]
    fn a_parent_with_only_its_own_balance_is_not_called_a_subtotal() {
        let (parents, labels) = tree();
        let out = group_statement_rows(&[stmt_row("P", 40)], &set(&["P"]), &parents, &labels);
        assert_eq!(out[0].account_name, "Office");
        assert_eq!(out[0].cents, 40);
    }
}
