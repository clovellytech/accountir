//! The statements a taxpayer receives, and where the money on them goes.
//!
//! # What this is for
//!
//! A personal return is mostly other people's paperwork: an employer's W-2, a
//! bank's 1099-INT, a partnership's K-1. Each is a set of boxes, and each box has
//! one place it belongs on the return. Recording a statement as figures keyed by
//! those boxes is what lets a return be computed from them rather than retyped
//! from a pile of PDFs — and a box code outside the form's own list is a figure
//! no line will ever pick up, so it is refused when it is recorded rather than
//! lost when the return is filed.
//!
//! # What the destinations are, and are not
//!
//! [`BoxDef::destination`] says where a box's amount is carried on a recent
//! Form 1040, in the words a preparer would use to find it. It is guidance for
//! the reader and for [`super::personal`]'s summary, not a line map: line numbers
//! move between years, and several boxes (a K-1's box 1, a 1098's interest) land
//! in different places depending on facts only the taxpayer knows. A Form 1040
//! builder will carry its own dated line map, as [`super::lines`] does for
//! Form 1065.
//!
//! # Coded boxes
//!
//! Some boxes hold several amounts under letter codes — K-1 boxes 11, 13, 15, 17
//! and 20. A statement records one total per box for now, with the detail in the
//! attached document, except where the parts go to different places on the
//! return: Schedule K line 13's parts and box 20 code Z's three Section 199A
//! figures have codes of their own here. Forms without a catalogue yet
//! ([`FormKind::is_open`]) accept any well-formed code.

use std::fmt;

/// A kind of statement somebody receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FormKind {
    W2,
    F1099Int,
    F1099Div,
    F1099B,
    F1099Misc,
    F1099Nec,
    F1099K,
    F1099R,
    F1099G,
    Ssa1099,
    F1098,
    K1Partnership,
    K1SCorporation,
    K1EstateOrTrust,
    IlK1P,
    PropertyTaxBill,
    Other,
}

/// One box on a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxDef {
    /// The code a statement's amounts are keyed by: the box number as printed,
    /// lowercase, or a spelled-out code where the box is split (`13_cash_contributions`).
    pub code: &'static str,
    pub label: &'static str,
    /// Where the amount goes on the return — see the module documentation.
    pub destination: &'static str,
    /// Whether this box adds up with the same box on other statements.
    ///
    /// False for a box that totals other boxes, a position rather than a year's
    /// amount (a capital account at year end), or a figure only reconciled
    /// against: adding those would count something twice or add unlike things.
    pub summed: bool,
}

const fn money(code: &'static str, label: &'static str, destination: &'static str) -> BoxDef {
    BoxDef {
        code,
        label,
        destination,
        summed: true,
    }
}

const fn info(code: &'static str, label: &'static str, destination: &'static str) -> BoxDef {
    BoxDef {
        code,
        label,
        destination,
        summed: false,
    }
}

const WITHHELD: &str = "Form 1040, line 25b";
const INTEREST: &str = "Schedule B, line 1 (Form 1040, line 2b)";
const ORDINARY_DIVIDENDS: &str = "Schedule B, line 5 (Form 1040, line 3b)";
const QUALIFIED_DIVIDENDS: &str = "Form 1040, line 3a";
const PASSIVE_RENTAL: &str = "Schedule E, Part II (passive — Form 8582)";
/// A coded box: what its amount is depends on the letter code, so the box is
/// shown but never added — box 11 code A and code C are not the same kind of
/// income, and a total of them is a number no line on the return wants.
const SEE_STATEMENT: &str = "Depends on the code — see the issuer's statement";
const BASIS_RECORDS: &str = "Your basis records — a capital account is not basis";
const AT_RISK: &str = "Basis, and at-risk limits (Form 6198)";
const PROPERTY: &str = "Schedule A, line 5b — or Schedule E or C for a rental or business property";

const W2: &[BoxDef] = &[
    money("1", "Wages, tips, other compensation", "Form 1040, line 1a"),
    money("2", "Federal income tax withheld", "Form 1040, line 25a"),
    money(
        "3",
        "Social security wages",
        "Excess social security tax check",
    ),
    money(
        "4",
        "Social security tax withheld",
        "Schedule 3, line 11, when more than one employer withheld past the wage base",
    ),
    money(
        "5",
        "Medicare wages and tips",
        "Form 8959, Additional Medicare Tax",
    ),
    money("6", "Medicare tax withheld", "Form 8959"),
    money("16", "State wages, tips, etc.", "State return"),
    money(
        "17",
        "State income tax",
        "Schedule A, line 5a, and the state return's withholding",
    ),
];

const F1099_INT: &[BoxDef] = &[
    money("1", "Interest income", INTEREST),
    money("2", "Early withdrawal penalty", "Schedule 1, line 18"),
    money(
        "3",
        "Interest on U.S. Savings Bonds and Treasury obligations",
        "Schedule B, line 1 (exempt on the state return)",
    ),
    money("4", "Federal income tax withheld", WITHHELD),
    money("8", "Tax-exempt interest", "Form 1040, line 2a"),
];

const F1099_DIV: &[BoxDef] = &[
    money("1a", "Total ordinary dividends", ORDINARY_DIVIDENDS),
    money("1b", "Qualified dividends", QUALIFIED_DIVIDENDS),
    money(
        "2a",
        "Total capital gain distributions",
        "Schedule D, line 13",
    ),
    money("4", "Federal income tax withheld", WITHHELD),
    money(
        "5",
        "Section 199A dividends",
        "Form 8995 or 8995-A, qualified REIT dividends",
    ),
    money("7", "Foreign tax paid", "Schedule 3, line 1, or Form 1116"),
];

const F1099_B: &[BoxDef] = &[
    money("1d", "Proceeds", "Form 8949"),
    money("1e", "Cost or other basis", "Form 8949"),
    money("4", "Federal income tax withheld", WITHHELD),
];

const F1099_MISC: &[BoxDef] = &[
    money("1", "Rents", "Schedule E, line 3"),
    money("2", "Royalties", "Schedule E, line 4"),
    money("3", "Other income", "Schedule 1, line 8z"),
    money("4", "Federal income tax withheld", WITHHELD),
];

const F1099_NEC: &[BoxDef] = &[
    money("1", "Nonemployee compensation", "Schedule C, line 1"),
    money("4", "Federal income tax withheld", WITHHELD),
];

const F1099_K: &[BoxDef] = &[
    info(
        "1a",
        "Gross amount of payment card and third party network transactions",
        "Schedule C, line 1 — reconciled to the books' gross receipts, not added to them",
    ),
    money("4", "Federal income tax withheld", WITHHELD),
];

const F1099_R: &[BoxDef] = &[
    money("1", "Gross distribution", "Form 1040, line 4a or 5a"),
    money("2a", "Taxable amount", "Form 1040, line 4b or 5b"),
    money("4", "Federal income tax withheld", WITHHELD),
];

const F1099_G: &[BoxDef] = &[
    money("1", "Unemployment compensation", "Schedule 1, line 7"),
    money(
        "2",
        "State or local income tax refunds, credits, or offsets",
        "Schedule 1, line 1 — only if deducted on an earlier Schedule A",
    ),
    money("4", "Federal income tax withheld", WITHHELD),
];

const SSA_1099: &[BoxDef] = &[
    money("5", "Net benefits", "Form 1040, line 6a"),
    money("6", "Voluntary federal income tax withheld", WITHHELD),
];

const F1098: &[BoxDef] = &[
    money(
        "1",
        "Mortgage interest received from payer(s)/borrower(s)",
        "Schedule A, line 8a — or Schedule E or C for a rental or business property",
    ),
    info(
        "2",
        "Outstanding mortgage principal",
        "Home mortgage interest limit (Publication 936)",
    ),
    money(
        "10",
        "Other (often real estate taxes paid from escrow)",
        PROPERTY,
    ),
];

const K1_PARTNERSHIP: &[BoxDef] = &[
    money(
        "1",
        "Ordinary business income (loss)",
        "Schedule E, Part II (nonpassive if you materially participate)",
    ),
    money("2", "Net rental real estate income (loss)", PASSIVE_RENTAL),
    money("3", "Other net rental income (loss)", PASSIVE_RENTAL),
    money(
        "4a",
        "Guaranteed payments for services",
        "Schedule E, Part II, and Schedule SE",
    ),
    money(
        "4b",
        "Guaranteed payments for capital",
        "Schedule E, Part II",
    ),
    info(
        "4c",
        "Total guaranteed payments",
        "Total of boxes 4a and 4b — not added again",
    ),
    money("5", "Interest income", INTEREST),
    money("6a", "Ordinary dividends", ORDINARY_DIVIDENDS),
    money("6b", "Qualified dividends", QUALIFIED_DIVIDENDS),
    money("6c", "Dividend equivalents", ORDINARY_DIVIDENDS),
    money("7", "Royalties", "Schedule E, line 4"),
    money(
        "8",
        "Net short-term capital gain (loss)",
        "Schedule D, line 5",
    ),
    money(
        "9a",
        "Net long-term capital gain (loss)",
        "Schedule D, line 12",
    ),
    money(
        "9b",
        "Collectibles (28%) gain (loss)",
        "28% Rate Gain Worksheet (Schedule D instructions)",
    ),
    money(
        "9c",
        "Unrecaptured section 1250 gain",
        "Unrecaptured Section 1250 Gain Worksheet (Schedule D instructions)",
    ),
    money("10", "Net section 1231 gain (loss)", "Form 4797"),
    info("11", "Other income (loss)", SEE_STATEMENT),
    money(
        "12",
        "Section 179 deduction",
        "Schedule E, Part II (Form 4562 limits)",
    ),
    money(
        "13_cash_contributions",
        "Cash contributions",
        "Schedule A, line 11",
    ),
    money(
        "13_noncash_contributions",
        "Noncash contributions",
        "Schedule A, line 12 (Form 8283 over $500)",
    ),
    money(
        "13_investment_interest",
        "Investment interest expense",
        "Form 4952",
    ),
    money(
        "13_section_59e2",
        "Section 59(e)(2) expenditures",
        "Schedule E, Part II, or amortized by election",
    ),
    info("13_other", "Other deductions", SEE_STATEMENT),
    money(
        "14a",
        "Net earnings (loss) from self-employment",
        "Schedule SE, line 2",
    ),
    money(
        "14b",
        "Gross farming or fishing income",
        "Schedule SE, farm optional method",
    ),
    money(
        "14c",
        "Gross nonfarm income",
        "Schedule SE, nonfarm optional method",
    ),
    info("15", "Credits", SEE_STATEMENT),
    money("17", "Alternative minimum tax (AMT) items", "Form 6251"),
    money(
        "18a",
        "Tax-exempt interest income",
        "Form 1040, line 2a, and basis",
    ),
    money(
        "18b",
        "Other tax-exempt income",
        "Basis only — not income on the return",
    ),
    money(
        "18c",
        "Nondeductible expenses",
        "Basis only — not deductible",
    ),
    money(
        "19a",
        "Distributions of cash and marketable securities",
        "Basis — a gain on Schedule D only past your basis",
    ),
    money(
        "19b",
        "Distributions of other property",
        "Basis — a gain on Schedule D only past your basis",
    ),
    money("20a", "Investment income", "Form 4952, line 4a"),
    money("20b", "Investment expenses", "Form 4952, line 5"),
    money(
        "20z_qbi",
        "Section 199A qualified business income",
        "Form 8995 or 8995-A, qualified business income",
    ),
    money(
        "20z_w2_wages",
        "Section 199A W-2 wages",
        "Form 8995-A, W-2 wages",
    ),
    money(
        "20z_ubia",
        "Section 199A UBIA of qualified property",
        "Form 8995-A, UBIA of qualified property",
    ),
    money(
        "21",
        "Foreign taxes paid or accrued",
        "Schedule 3, line 1, or Form 1116",
    ),
    info(
        "L_beginning",
        "Capital account, beginning of year",
        BASIS_RECORDS,
    ),
    info(
        "L_contributed",
        "Capital contributed during the year",
        BASIS_RECORDS,
    ),
    info("L_income", "Current year net income (loss)", BASIS_RECORDS),
    info("L_other", "Other increase (decrease)", BASIS_RECORDS),
    info(
        "L_withdrawals",
        "Withdrawals and distributions",
        BASIS_RECORDS,
    ),
    info("L_ending", "Capital account, end of year", BASIS_RECORDS),
    info(
        "K_nonrecourse_beginning",
        "Share of nonrecourse liabilities, beginning",
        AT_RISK,
    ),
    info(
        "K_nonrecourse_ending",
        "Share of nonrecourse liabilities, ending",
        AT_RISK,
    ),
    info(
        "K_qualified_nonrecourse_beginning",
        "Share of qualified nonrecourse financing, beginning",
        AT_RISK,
    ),
    info(
        "K_qualified_nonrecourse_ending",
        "Share of qualified nonrecourse financing, ending",
        AT_RISK,
    ),
    info(
        "K_recourse_beginning",
        "Share of recourse liabilities, beginning",
        AT_RISK,
    ),
    info(
        "K_recourse_ending",
        "Share of recourse liabilities, ending",
        AT_RISK,
    ),
];

const K1_S_CORPORATION: &[BoxDef] = &[
    money(
        "1",
        "Ordinary business income (loss)",
        "Schedule E, Part II",
    ),
    money("2", "Net rental real estate income (loss)", PASSIVE_RENTAL),
    money("3", "Other net rental income (loss)", PASSIVE_RENTAL),
    money("4", "Interest income", INTEREST),
    money("5a", "Ordinary dividends", ORDINARY_DIVIDENDS),
    money("5b", "Qualified dividends", QUALIFIED_DIVIDENDS),
    money("6", "Royalties", "Schedule E, line 4"),
    money(
        "7",
        "Net short-term capital gain (loss)",
        "Schedule D, line 5",
    ),
    money(
        "8a",
        "Net long-term capital gain (loss)",
        "Schedule D, line 12",
    ),
    money("9", "Net section 1231 gain (loss)", "Form 4797"),
    info("10", "Other income (loss)", SEE_STATEMENT),
    money(
        "11",
        "Section 179 deduction",
        "Schedule E, Part II (Form 4562 limits)",
    ),
    info("12", "Other deductions", SEE_STATEMENT),
    money(
        "16a",
        "Tax-exempt interest income",
        "Form 1040, line 2a, and basis",
    ),
    money(
        "16c",
        "Nondeductible expenses",
        "Stock basis (Form 7203) — not deductible",
    ),
    money(
        "16d",
        "Distributions",
        "Stock basis (Form 7203) — a gain only past your basis",
    ),
    money(
        "17v_qbi",
        "Section 199A qualified business income",
        "Form 8995 or 8995-A, qualified business income",
    ),
    money(
        "17v_w2_wages",
        "Section 199A W-2 wages",
        "Form 8995-A, W-2 wages",
    ),
    money(
        "17v_ubia",
        "Section 199A UBIA of qualified property",
        "Form 8995-A, UBIA of qualified property",
    ),
];

const K1_ESTATE_OR_TRUST: &[BoxDef] = &[
    money("1", "Interest income", INTEREST),
    money("2a", "Ordinary dividends", ORDINARY_DIVIDENDS),
    money("2b", "Qualified dividends", QUALIFIED_DIVIDENDS),
    money("3", "Net short-term capital gain", "Schedule D, line 5"),
    money("4a", "Net long-term capital gain", "Schedule D, line 12"),
    money(
        "5",
        "Other portfolio and nonbusiness income",
        "Schedule E, Part III",
    ),
    money("6", "Ordinary business income", "Schedule E, Part III"),
    money("7", "Net rental real estate income", "Schedule E, Part III"),
    money("8", "Other rental income", "Schedule E, Part III"),
    info("14", "Other information", SEE_STATEMENT),
];

const PROPERTY_TAX_BILL: &[BoxDef] = &[money("paid", "Real estate taxes paid", PROPERTY)];

impl FormKind {
    pub const ALL: [FormKind; 17] = [
        FormKind::W2,
        FormKind::F1099Int,
        FormKind::F1099Div,
        FormKind::F1099B,
        FormKind::F1099Misc,
        FormKind::F1099Nec,
        FormKind::F1099K,
        FormKind::F1099R,
        FormKind::F1099G,
        FormKind::Ssa1099,
        FormKind::F1098,
        FormKind::K1Partnership,
        FormKind::K1SCorporation,
        FormKind::K1EstateOrTrust,
        FormKind::IlK1P,
        FormKind::PropertyTaxBill,
        FormKind::Other,
    ];

    /// The stable code the event log stores.
    pub fn as_str(self) -> &'static str {
        match self {
            FormKind::W2 => "w2",
            FormKind::F1099Int => "1099_int",
            FormKind::F1099Div => "1099_div",
            FormKind::F1099B => "1099_b",
            FormKind::F1099Misc => "1099_misc",
            FormKind::F1099Nec => "1099_nec",
            FormKind::F1099K => "1099_k",
            FormKind::F1099R => "1099_r",
            FormKind::F1099G => "1099_g",
            FormKind::Ssa1099 => "ssa_1099",
            FormKind::F1098 => "1098",
            FormKind::K1Partnership => "k1_1065",
            FormKind::K1SCorporation => "k1_1120s",
            FormKind::K1EstateOrTrust => "k1_1041",
            FormKind::IlK1P => "il_k1_p",
            FormKind::PropertyTaxBill => "property_tax_bill",
            FormKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> Option<FormKind> {
        FormKind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            FormKind::W2 => "Form W-2",
            FormKind::F1099Int => "Form 1099-INT",
            FormKind::F1099Div => "Form 1099-DIV",
            FormKind::F1099B => "Form 1099-B",
            FormKind::F1099Misc => "Form 1099-MISC",
            FormKind::F1099Nec => "Form 1099-NEC",
            FormKind::F1099K => "Form 1099-K",
            FormKind::F1099R => "Form 1099-R",
            FormKind::F1099G => "Form 1099-G",
            FormKind::Ssa1099 => "Form SSA-1099",
            FormKind::F1098 => "Form 1098",
            FormKind::K1Partnership => "Schedule K-1 (Form 1065)",
            FormKind::K1SCorporation => "Schedule K-1 (Form 1120-S)",
            FormKind::K1EstateOrTrust => "Schedule K-1 (Form 1041)",
            FormKind::IlK1P => "Illinois Schedule K-1-P",
            FormKind::PropertyTaxBill => "Property tax bill",
            FormKind::Other => "Other statement",
        }
    }

    /// The boxes this version knows for the form. Empty for a form with no
    /// catalogue yet.
    pub fn boxes(self) -> &'static [BoxDef] {
        match self {
            FormKind::W2 => W2,
            FormKind::F1099Int => F1099_INT,
            FormKind::F1099Div => F1099_DIV,
            FormKind::F1099B => F1099_B,
            FormKind::F1099Misc => F1099_MISC,
            FormKind::F1099Nec => F1099_NEC,
            FormKind::F1099K => F1099_K,
            FormKind::F1099R => F1099_R,
            FormKind::F1099G => F1099_G,
            FormKind::Ssa1099 => SSA_1099,
            FormKind::F1098 => F1098,
            FormKind::K1Partnership => K1_PARTNERSHIP,
            FormKind::K1SCorporation => K1_S_CORPORATION,
            FormKind::K1EstateOrTrust => K1_ESTATE_OR_TRUST,
            FormKind::PropertyTaxBill => PROPERTY_TAX_BILL,
            FormKind::IlK1P | FormKind::Other => &[],
        }
    }

    /// Whether the form has no catalogue, so any well-formed box code is taken.
    pub fn is_open(self) -> bool {
        self.boxes().is_empty()
    }

    pub fn box_def(self, code: &str) -> Option<&'static BoxDef> {
        self.boxes().iter().find(|b| b.code == code)
    }

    /// Whether a statement of this kind can carry an amount under `code`.
    pub fn accepts_box(self, code: &str) -> bool {
        if self.is_open() {
            is_box_code(code)
        } else {
            self.box_def(code).is_some()
        }
    }
}

impl fmt::Display for FormKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A code in the shape every catalogued code has: letters, digits and
/// underscores, at most 40 of them.
pub fn is_box_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 40
        && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_form_code_reads_back_as_its_form() {
        for kind in FormKind::ALL {
            assert_eq!(FormKind::parse(kind.as_str()), Some(kind));
        }
        let mut codes: Vec<_> = FormKind::ALL.iter().map(|k| k.as_str()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), FormKind::ALL.len(), "form codes are distinct");
    }

    /// A duplicated box code would make one of the two unreachable, and a code
    /// in the wrong shape would be refused by the check that admits it.
    #[test]
    fn every_catalogued_box_is_distinct_and_well_formed() {
        for kind in FormKind::ALL {
            let mut seen = std::collections::BTreeSet::new();
            for b in kind.boxes() {
                assert!(is_box_code(b.code), "{kind}: {:?} is malformed", b.code);
                assert!(seen.insert(b.code), "{kind}: box {:?} twice", b.code);
                assert!(!b.label.is_empty() && !b.destination.is_empty());
            }
        }
    }

    #[test]
    fn a_catalogued_form_refuses_a_box_it_does_not_have() {
        assert!(FormKind::F1099Int.accepts_box("1"));
        assert!(!FormKind::F1099Int.accepts_box("99"));
        assert!(
            !FormKind::K1Partnership.accepts_box("16"),
            "box 16 is Schedule K-3 now"
        );
    }

    #[test]
    fn an_open_form_takes_any_well_formed_code_and_nothing_else() {
        assert!(FormKind::Other.is_open());
        assert!(FormKind::Other.accepts_box("anything_7"));
        assert!(!FormKind::Other.accepts_box(""));
        assert!(!FormKind::Other.accepts_box("no spaces"));
    }

    /// Figures that mean different things must not be added into one total: a
    /// coded box's amounts, and the three Section 199A figures, which the form
    /// takes on separate lines.
    #[test]
    fn unlike_figures_are_never_gathered_into_one_total() {
        let k1 = FormKind::K1Partnership;
        for code in ["11", "13_other", "15"] {
            assert!(!k1.box_def(code).unwrap().summed, "box {code} is coded");
        }
        let qbi = k1.box_def("20z_qbi").unwrap().destination;
        let wages = k1.box_def("20z_w2_wages").unwrap().destination;
        let ubia = k1.box_def("20z_ubia").unwrap().destination;
        assert!(qbi != wages && wages != ubia && qbi != ubia);
        assert_ne!(FormKind::F1099Div.box_def("5").unwrap().destination, qbi);
    }

    /// A total and its parts must not both be summed toward one destination.
    #[test]
    fn a_total_box_is_not_summed() {
        let total = FormKind::K1Partnership.box_def("4c").unwrap();
        assert!(!total.summed);
        assert!(FormKind::K1Partnership.box_def("4a").unwrap().summed);
    }
}
