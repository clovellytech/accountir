//! Schedule B, "Other Information": the questions, the answers, and getting
//! them onto the form.
//!
//! # Why the questions are a table and not a struct
//!
//! Schedule B is thirty-odd numbered questions that are almost all yes/no, plus
//! a handful of counts and amounts that only matter when the answer is yes.
//! Modelled as a struct with a field per question, every consumer — storage, the
//! desktop form, the PDF filler — has to name all thirty separately, and the
//! next form revision that inserts a question renumbers the lot. As a table,
//! each of those consumers is a loop, and a revision is an edit to this file
//! alone.
//!
//! # Why the full question text lives here
//!
//! The words are the question. "Q7" means nothing to somebody deciding whether
//! to tick yes, and a paraphrase means something subtly different from what they
//! are signing — these are legal representations, and "did you make any payments
//! that would require you to file Form(s) 1099" is not "did you send any 1099s".
//! So the text is carried verbatim from the form and shown verbatim, and
//! `docs/form-1065-fields.md` (generated from the PDF itself) is where it came
//! from.
//!
//! # Why answers are strings keyed by question
//!
//! See `migrations/026_schedule_b_answers.sql`. Briefly: the shapes move between
//! revisions, and a typed column per question buys a migration every time the
//! IRS renumbers.
//!
//! # What this does not fill in
//!
//! Questions 3a and 3b each carry a five-row table naming the *other* entities
//! involved, and 2a/2b want Schedule B-1 attached. Those are lists of third
//! parties, not answers about this partnership, and nothing in the books knows
//! them. Answering yes therefore produces a warning pointing at the table or the
//! attachment rather than a silently blank one — see [`fill`].

use std::collections::BTreeMap;

use rusqlite::Connection;

use super::acroform::{set_check, set_text, FieldMap, FormError};
use lopdf::Document;

// ---------------------------------------------------------------------------
// The catalogue
// ---------------------------------------------------------------------------

/// A form the question points somebody at, and where to read about it.
///
/// The URL is part of the question, not decoration: every one of these is a
/// separate filing obligation that a yes answer triggers, and the difference
/// between "yes" and "yes, and I owe a Form 8865" is a penalty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormRef {
    pub name: &'static str,
    pub url: &'static str,
}

/// One choice in a pick-one question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChoiceOpt {
    /// Stored value.
    pub key: &'static str,
    pub label: &'static str,
    /// The checkbox this option ticks, and the appearance state that ticks it.
    pub field: &'static str,
    pub on: &'static str,
}

/// What kind of input a follow-up wants. Only affects how it is presented and
/// what counts as a sane value; everything is stored as text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Text,
    /// A whole number of forms, partners, and so on.
    Count,
    /// A dollar amount, entered as the form wants it.
    Money,
    /// A date, entered as the preparer wants it printed.
    Date,
    /// A percentage.
    Percent,
}

/// An extra box that only means anything once the question is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowUp {
    /// Storage key. Namespaced under the question, e.g. `b10a_date`.
    pub key: &'static str,
    pub label: &'static str,
    pub field: &'static str,
    pub kind: InputKind,
}

/// How a question is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Two checkboxes, one of which gets ticked.
    YesNo {
        yes: &'static str,
        no: &'static str,
    },
    /// Mutually exclusive checkboxes. Exclusivity is enforced here because the
    /// XFA logic that enforced it on the original form is stripped out — see
    /// [`super::acroform`] — so nothing but this code stops two boxes being
    /// ticked at once.
    Choice(&'static [ChoiceOpt]),
    /// A lone box that is either ticked or not. Not a yes/no: the form offers no
    /// "no" box, and an unticked box *is* the negative answer.
    Check { field: &'static str },
    /// A question whose whole answer is a number in a box — "enter the number of
    /// Forms 8865 attached". No checkbox exists for these on the form.
    Entry {
        field: &'static str,
        kind: InputKind,
    },
}

/// When a follow-up is worth showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUpWhen {
    Yes,
    No,
    /// A specific choice key.
    Choice(&'static str),
    Always,
}

pub struct Question {
    /// Stable storage key. Not the number: the IRS renumbers, and a renumbering
    /// must not silently re-point an answer at a different question.
    pub key: &'static str,
    /// As printed, e.g. "10b".
    pub number: &'static str,
    /// Which page of the form it is on, so the desktop can group them the way
    /// the paper does.
    pub page: u8,
    /// Verbatim from the form.
    pub text: &'static str,
    pub control: Control,
    pub follow_ups: &'static [(FollowUpWhen, FollowUp)],
    pub refs: &'static [FormRef],
    /// Said when the answer is yes and this program cannot fill in what a yes
    /// obliges. Empty when a yes needs nothing further.
    pub yes_warning: &'static str,
    /// A number the IRS is holding for a question it has not written yet.
    ///
    /// Kept in the catalogue rather than deleted: the boxes exist on the form and
    /// the guard test has to keep proving that the *other* questions still line up
    /// with their fields, which means the numbering has to stay intact. But it is
    /// never shown, never counted as unanswered, and never written to — there is
    /// nothing to answer, and a form with a tick beside "reserved for future use"
    /// is a form somebody answered a question that does not exist.
    pub reserved: bool,
    /// A question that only applies when an earlier one was answered a
    /// particular way.
    ///
    /// The form says so in words — question 16b opens "If 'Yes' to question 16a"
    /// — and the consequence is real: 16b answered No beside a 16a of No reads as
    /// "we had 1099s to file and did not file them", which is the opposite of
    /// what happened. So an answer is only carried onto the form while its
    /// condition holds.
    pub depends_on: Option<Dependency>,
}

/// An earlier answer this question hangs off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dependency {
    /// The question key that governs this one.
    pub question: &'static str,
    /// The value it has to hold.
    pub value: &'static str,
    /// How to say the condition to somebody looking at the page.
    pub label: &'static str,
}

// The forms Schedule B points at. Every URL is an IRS landing page carrying the
// form and its instructions, except the two Form 1065 schedules that have no
// such page and the FBAR, which is filed with FinCEN and not the IRS at all.
const SCHEDULE_B1: FormRef = FormRef {
    name: "Schedule B-1 (Form 1065)",
    url: "https://www.irs.gov/pub/irs-pdf/f1065sb1.pdf",
};
const SCHEDULE_B2: FormRef = FormRef {
    name: "Schedule B-2 (Form 1065)",
    url: "https://www.irs.gov/instructions/i1065sb2",
};
const SCHEDULE_M3: FormRef = FormRef {
    name: "Schedule M-3 (Form 1065)",
    url: "https://www.irs.gov/instructions/i1065sm3",
};
const FORM_8918: FormRef = FormRef {
    name: "Form 8918",
    url: "https://www.irs.gov/forms-pubs/about-form-8918",
};
const FBAR_114: FormRef = FormRef {
    name: "FinCEN Form 114 (FBAR)",
    url: "https://www.irs.gov/businesses/small-businesses-self-employed/report-of-foreign-bank-and-financial-accounts-fbar",
};
const FORM_3520: FormRef = FormRef {
    name: "Form 3520",
    url: "https://www.irs.gov/forms-pubs/about-form-3520",
};
const FORM_8858: FormRef = FormRef {
    name: "Form 8858",
    url: "https://www.irs.gov/forms-pubs/about-form-8858",
};
const FORM_8805: FormRef = FormRef {
    name: "Form 8805",
    url: "https://www.irs.gov/forms-pubs/about-form-8805",
};
const FORM_8865: FormRef = FormRef {
    name: "Form 8865",
    url: "https://www.irs.gov/forms-pubs/about-form-8865",
};
const FORM_1099: FormRef = FormRef {
    name: "Form 1099",
    url: "https://www.irs.gov/businesses/small-businesses-self-employed/am-i-required-to-file-a-form-1099-or-other-information-return",
};
const FORM_5471: FormRef = FormRef {
    name: "Form 5471",
    url: "https://www.irs.gov/forms-pubs/about-form-5471",
};
const FORM_1042: FormRef = FormRef {
    name: "Form 1042",
    url: "https://www.irs.gov/forms-pubs/about-form-1042",
};
const FORM_1042S: FormRef = FormRef {
    name: "Form 1042-S",
    url: "https://www.irs.gov/forms-pubs/about-form-1042-s",
};
const FORM_8938: FormRef = FormRef {
    name: "Form 8938",
    url: "https://www.irs.gov/forms-pubs/about-form-8938",
};
const FORM_8990: FormRef = FormRef {
    name: "Form 8990",
    url: "https://www.irs.gov/forms-pubs/about-form-8990",
};
const FORM_8996: FormRef = FormRef {
    name: "Form 8996",
    url: "https://www.irs.gov/forms-pubs/about-form-8996",
};
const FORM_7208: FormRef = FormRef {
    name: "Form 7208",
    url: "https://www.irs.gov/forms-pubs/about-form-7208",
};

const ENTITY_TYPES: &[ChoiceOpt] = &[
    ChoiceOpt { key: "general",  label: "Domestic general partnership",         field: "c2_1[0]", on: "1" },
    ChoiceOpt { key: "lp",       label: "Domestic limited partnership",         field: "c2_1[1]", on: "2" },
    ChoiceOpt { key: "llc",      label: "Domestic limited liability company",   field: "c2_1[2]", on: "3" },
    ChoiceOpt { key: "llp",      label: "Domestic limited liability partnership", field: "c2_1[3]", on: "4" },
    ChoiceOpt { key: "foreign",  label: "Foreign partnership",                  field: "c2_1[4]", on: "5" },
    ChoiceOpt { key: "other",    label: "Other",                                field: "c2_1[5]", on: "6" },
];

/// Every question on Schedule B, in the order the form asks them.
///
/// Checked against the vendored PDF by the tests at the bottom: a revision that
/// renumbers a box fails there rather than silently ticking its neighbour.
pub const QUESTIONS: &[Question] = &[
    Question {
        key: "b1",
        number: "1",
        page: 2,
        text: "What type of entity is filing this return? Check the applicable box.",
        control: Control::Choice(ENTITY_TYPES),
        follow_ups: &[(
            FollowUpWhen::Choice("other"),
            FollowUp { key: "b1_other", label: "Other — describe", field: "f2_01[0]", kind: InputKind::Text },
        )],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b2a",
        number: "2a",
        page: 2,
        text: "At the end of the tax year: Did any foreign or domestic corporation, partnership \
               (including any entity treated as a partnership), trust, or tax-exempt organization, \
               or any foreign government own, directly or indirectly, an interest of 50% or more in \
               the profit, loss, or capital of the partnership? For rules of constructive ownership, \
               see instructions. If \u{201c}Yes,\u{201d} attach Schedule B-1, Information on Partners \
               Owning 50% or More of the Partnership.",
        control: Control::YesNo { yes: "c2_2[0]", no: "c2_2[1]" },
        follow_ups: &[],
        refs: &[SCHEDULE_B1],
        yes_warning: "Question 2a is Yes, so Schedule B-1 is attached, listing every partner in the \
                      books who owns 50% or more. Check it against anybody who reaches 50% through \
                      family or related entities, which the books cannot see.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b2b",
        number: "2b",
        page: 2,
        text: "Did any individual or estate own, directly or indirectly, an interest of 50% or more \
               in the profit, loss, or capital of the partnership? For rules of constructive \
               ownership, see instructions. If \u{201c}Yes,\u{201d} attach Schedule B-1.",
        control: Control::YesNo { yes: "c2_3[0]", no: "c2_3[1]" },
        follow_ups: &[],
        refs: &[SCHEDULE_B1],
        yes_warning: "Question 2b is Yes, so Schedule B-1 is attached, listing every partner in the \
                      books who owns 50% or more. Check it against anybody who reaches 50% through \
                      family or related entities, which the books cannot see.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b3a",
        number: "3a",
        page: 2,
        text: "At the end of the tax year, did the partnership own directly 20% or more, or own, \
               directly or indirectly, 50% or more of the total voting power of all classes of stock \
               entitled to vote of any foreign or domestic corporation? For rules of constructive \
               ownership, see instructions. If \u{201c}Yes,\u{201d} complete (i) through (iv) below.",
        control: Control::YesNo { yes: "c2_4[0]", no: "c2_4[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "Question 3a is Yes, so the table under it — name, EIN, country and percentage \
                      for each corporation — has to be filled in. Those are facts about other \
                      companies that the books do not hold, so the rows are left blank and editable \
                      in the PDF.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b3b",
        number: "3b",
        page: 2,
        text: "At the end of the tax year, did the partnership own directly an interest of 20% or \
               more, or own, directly or indirectly, an interest of 50% or more, in the profit, loss, \
               or capital in any foreign or domestic partnership (including an entity treated as a \
               partnership) or in the beneficial interest of a trust? For rules of constructive \
               ownership, see instructions. If \u{201c}Yes,\u{201d} complete (i) through (v) below.",
        control: Control::YesNo { yes: "c2_5[0]", no: "c2_5[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "Question 3b is Yes, so the table under it — name, EIN, type, country and \
                      percentage for each entity — has to be filled in. Those are facts about other \
                      entities that the books do not hold, so the rows are left blank and editable \
                      in the PDF.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b4",
        number: "4",
        page: 2,
        text: "Does the partnership satisfy all four of the following conditions? \
               (a) The partnership's total receipts for the tax year were less than $250,000. \
               (b) The partnership's total assets at the end of the tax year were less than $1 million. \
               (c) Schedules K-1 are filed with the return and furnished to the partners on or before \
               the due date (including extensions) for the partnership return. \
               (d) The partnership is not filing and is not required to file Schedule M-3. \
               If \u{201c}Yes,\u{201d} the partnership is not required to complete Schedules L, M-1, \
               and M-2; item F on page 1 of Form 1065; or item L on Schedule K-1.",
        control: Control::YesNo { yes: "c2_6[0]", no: "c2_6[1]" },
        follow_ups: &[],
        refs: &[SCHEDULE_M3],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b5",
        number: "5",
        page: 2,
        text: "Is this partnership a publicly traded partnership, as defined in section 469(k)(2)?",
        control: Control::YesNo { yes: "c2_7[0]", no: "c2_7[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b6",
        number: "6",
        page: 2,
        text: "During the tax year, did the partnership have any debt that was canceled, was \
               forgiven, or had the terms modified so as to reduce the principal amount of the debt?",
        control: Control::YesNo { yes: "c2_8[0]", no: "c2_8[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b7",
        number: "7",
        page: 2,
        text: "Has this partnership filed, or is it required to file, Form 8918, Material Advisor \
               Disclosure Statement, to provide information on any reportable transaction?",
        control: Control::YesNo { yes: "c2_9[0]", no: "c2_9[1]" },
        follow_ups: &[],
        refs: &[FORM_8918],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b8",
        number: "8",
        page: 2,
        text: "At any time during calendar year 2025, did the partnership have an interest in or a \
               signature or other authority over a financial account in a foreign country (such as a \
               bank account, securities account, or other financial account)? See instructions for \
               exceptions and filing requirements for FinCEN Form 114, Report of Foreign Bank and \
               Financial Accounts (FBAR). If \u{201c}Yes,\u{201d} enter the name of the foreign country.",
        control: Control::YesNo { yes: "c2_10[0]", no: "c2_10[1]" },
        follow_ups: &[(
            FollowUpWhen::Yes,
            FollowUp { key: "b8_country", label: "Name of the foreign country", field: "f2_47[0]", kind: InputKind::Text },
        )],
        refs: &[FBAR_114],
        yes_warning: "Question 8 is Yes, which usually means an FBAR is owed. It is filed with \
                      FinCEN, separately from this return, and this program does not produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b9",
        number: "9",
        page: 2,
        text: "At any time during the tax year, did the partnership receive a distribution from, or \
               was it the grantor of, or transferor to, a foreign trust? If \u{201c}Yes,\u{201d} the \
               partnership may have to file Form 3520, Annual Return To Report Transactions With \
               Foreign Trusts and Receipt of Certain Foreign Gifts. See instructions.",
        control: Control::YesNo { yes: "c2_11[0]", no: "c2_11[1]" },
        follow_ups: &[],
        refs: &[FORM_3520],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b10a",
        number: "10a",
        page: 2,
        text: "Is the partnership making, or had it previously made (and not revoked), a section 754 \
               election? If \u{201c}Yes,\u{201d} enter the effective date of the election.",
        control: Control::YesNo { yes: "c2_12[0]", no: "c2_12[1]" },
        follow_ups: &[(
            FollowUpWhen::Yes,
            FollowUp { key: "b10a_date", label: "Effective date of the election", field: "f2_48[0]", kind: InputKind::Date },
        )],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b10b",
        number: "10b",
        page: 2,
        text: "For this tax year, did the partnership make an optional basis adjustment under section \
               743(b)? If \u{201c}Yes,\u{201d} enter the total aggregate net positive and net negative \
               amounts of such section 743(b) adjustments for all partners made in the tax year. The \
               partnership must also attach a statement showing the computation and allocation of each \
               basis adjustment.",
        control: Control::YesNo { yes: "c2_13[0]", no: "c2_13[1]" },
        follow_ups: &[
            (FollowUpWhen::Yes, FollowUp { key: "b10b_positive", label: "Total aggregate net positive amount", field: "f2_49[0]", kind: InputKind::Money }),
            (FollowUpWhen::Yes, FollowUp { key: "b10b_negative", label: "Total aggregate net negative amount", field: "f2_50[0]", kind: InputKind::Money }),
        ],
        refs: &[],
        yes_warning: "Question 10b is Yes, so a statement showing the computation and allocation of \
                      each basis adjustment has to be attached. This program does not produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b10c",
        number: "10c",
        page: 3,
        text: "For this tax year, did the partnership make an optional basis adjustment under section \
               734(b)? If \u{201c}Yes,\u{201d} enter the total aggregate net positive and net negative \
               amounts of such section 734(b) adjustments for all partnership property made in the tax \
               year. The partnership must also attach a statement showing the computation and \
               allocation of each basis adjustment.",
        control: Control::YesNo { yes: "c3_1[0]", no: "c3_1[1]" },
        follow_ups: &[
            (FollowUpWhen::Yes, FollowUp { key: "b10c_positive", label: "Total aggregate net positive amount", field: "f3_1[0]", kind: InputKind::Money }),
            (FollowUpWhen::Yes, FollowUp { key: "b10c_negative", label: "Total aggregate net negative amount", field: "f3_2[0]", kind: InputKind::Money }),
        ],
        refs: &[],
        yes_warning: "Question 10c is Yes, so a statement showing the computation and allocation of \
                      each basis adjustment has to be attached. This program does not produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b10d",
        number: "10d",
        page: 3,
        text: "For this tax year, is the partnership required to adjust the basis of partnership \
               property under section 743(b) or 734(b) because of a substantial built-in loss (as \
               defined under section 743(d)) or substantial basis reduction (as defined under section \
               734(d))? If \u{201c}Yes,\u{201d} enter the total aggregate amount of such section 743(b) \
               adjustments and/or section 734(b) adjustments for all partners and/or partnership \
               property made in the tax year. The partnership must also attach a statement showing the \
               computation and allocation of the basis adjustment.",
        control: Control::YesNo { yes: "c3_2[0]", no: "c3_2[1]" },
        follow_ups: &[(
            FollowUpWhen::Yes,
            FollowUp { key: "b10d_amount", label: "Total aggregate amount", field: "f3_3[0]", kind: InputKind::Money },
        )],
        refs: &[],
        yes_warning: "Question 10d is Yes, so a statement showing the computation and allocation of \
                      the basis adjustment has to be attached. This program does not produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b10e",
        number: "10e",
        page: 3,
        text: "Reserved for future use.",
        control: Control::YesNo { yes: "c3_3[0]", no: "c3_3[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: true,
        depends_on: None,
    },
    Question {
        key: "b11",
        number: "11",
        page: 3,
        text: "Check this box if, during the current or prior tax year, the partnership distributed \
               any property received in a like-kind exchange or contributed such property to another \
               entity (other than disregarded entities wholly owned by the partnership throughout the \
               tax year).",
        control: Control::Check { field: "c3_4[0]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b12",
        number: "12",
        page: 3,
        text: "At any time during the tax year, did the partnership distribute to any partner a \
               tenancy-in-common or other undivided interest in partnership property?",
        control: Control::YesNo { yes: "c3_5[0]", no: "c3_5[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b13a",
        number: "13a",
        page: 3,
        text: "If the partnership is required to file Form 8858, Information Return of U.S. Persons \
               With Respect to Foreign Disregarded Entities (FDEs) and Foreign Branches (FBs), enter \
               the number of Forms 8858 attached. See instructions.",
        control: Control::Entry { field: "f3_4[0]", kind: InputKind::Count },
        follow_ups: &[],
        refs: &[FORM_8858],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b14",
        number: "14",
        page: 3,
        text: "Does the partnership have any foreign partners? If \u{201c}Yes,\u{201d} enter the number \
               of Forms 8805, Foreign Partner\u{2019}s Information Statement of Section 1446 \
               Withholding Tax, filed for this partnership.",
        control: Control::YesNo { yes: "c3_6[0]", no: "c3_6[1]" },
        follow_ups: &[(
            FollowUpWhen::Yes,
            FollowUp { key: "b14_count", label: "Number of Forms 8805 filed", field: "f3_5[0]", kind: InputKind::Count },
        )],
        refs: &[FORM_8805],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b15",
        number: "15",
        page: 3,
        text: "Enter the number of Forms 8865, Return of U.S. Persons With Respect to Certain Foreign \
               Partnerships, attached to this return.",
        control: Control::Entry { field: "f3_6[0]", kind: InputKind::Count },
        follow_ups: &[],
        refs: &[FORM_8865],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b16a",
        number: "16a",
        page: 3,
        text: "Did you make any payments in 2025 that would require you to file Form(s) 1099? See \
               instructions.",
        control: Control::YesNo { yes: "c3_7[0]", no: "c3_7[1]" },
        follow_ups: &[],
        refs: &[FORM_1099],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b16b",
        number: "16b",
        page: 3,
        text: "If \u{201c}Yes\u{201d} to question 16a, did you or will you file required Form(s) 1099?",
        control: Control::YesNo { yes: "c3_8[0]", no: "c3_8[1]" },
        follow_ups: &[],
        refs: &[FORM_1099],
        yes_warning: "",
        reserved: false,
        depends_on: Some(Dependency {
            question: "b16a",
            value: YES,
            label: "only when question 16a is Yes",
        }),
    },
    Question {
        key: "b17",
        number: "17",
        page: 3,
        text: "Enter the number of Forms 5471, Information Return of U.S. Persons With Respect to \
               Certain Foreign Corporations, attached to this return.",
        control: Control::Entry { field: "f3_7[0]", kind: InputKind::Count },
        follow_ups: &[],
        refs: &[FORM_5471],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b18",
        number: "18",
        page: 3,
        text: "Enter the number of partners that are foreign governments under section 892.",
        control: Control::Entry { field: "f3_8[0]", kind: InputKind::Count },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b19",
        number: "19",
        page: 3,
        text: "During the partnership\u{2019}s tax year, did the partnership make any payments, or \
               receive any payments allocable to foreign partners, that would require it to file Forms \
               1042 and 1042-S under chapter 3 (sections 1441 through 1464) or chapter 4 (sections \
               1471 through 1474)?",
        control: Control::YesNo { yes: "c3_9[0]", no: "c3_9[1]" },
        follow_ups: &[],
        refs: &[FORM_1042, FORM_1042S],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b20",
        number: "20",
        page: 3,
        text: "Was the partnership a specified domestic entity required to file Form 8938 for the tax \
               year? See the Instructions for Form 8938.",
        control: Control::YesNo { yes: "c3_10[0]", no: "c3_10[1]" },
        follow_ups: &[],
        refs: &[FORM_8938],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b21",
        number: "21",
        page: 3,
        text: "Is the partnership a section 721(c) partnership, as defined in Regulations section \
               1.721(c)-1(b)(14)?",
        control: Control::YesNo { yes: "c3_11[0]", no: "c3_11[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b22",
        number: "22",
        page: 3,
        text: "During the tax year, did the partnership pay or accrue any interest or royalty for \
               which one or more partners are not allowed a deduction under section 267A? See \
               instructions. If \u{201c}Yes,\u{201d} enter the total amount of the disallowed deductions.",
        control: Control::YesNo { yes: "c3_12[0]", no: "c3_12[1]" },
        follow_ups: &[(
            FollowUpWhen::Yes,
            FollowUp { key: "b22_amount", label: "Total disallowed deductions", field: "f3_9[0]", kind: InputKind::Money },
        )],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b23",
        number: "23",
        page: 3,
        text: "Did the partnership have an election under section 163(j) for any real property trade or \
               business or any farming business in effect during the tax year? See instructions.",
        control: Control::YesNo { yes: "c3_13[0]", no: "c3_13[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b24",
        number: "24",
        page: 3,
        text: "Does the partnership satisfy one or more of the following? See instructions. \
               (a) The partnership owns a pass-through entity with current, or prior year carryover, \
               excess business interest expense. \
               (b) The partnership\u{2019}s aggregate average annual gross receipts (determined under \
               section 448(c)) for the 3 tax years preceding the current tax year are more than $31 \
               million and the partnership has business interest expense. \
               (c) The partnership is a tax shelter (see instructions) and the partnership has business \
               interest expense. \
               If \u{201c}Yes\u{201d} to any, complete and attach Form 8990.",
        control: Control::YesNo { yes: "c3_14[0]", no: "c3_14[1]" },
        follow_ups: &[],
        refs: &[FORM_8990],
        yes_warning: "Question 24 is Yes, so Form 8990 has to be completed and attached. This program \
                      does not produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b25",
        number: "25",
        page: 3,
        text: "Does the partnership intend to self-certify as a qualified opportunity fund? If \
               \u{201c}Yes,\u{201d} complete and attach Form 8996, Qualified Opportunity Fund, and \
               enter the amount (if any) from Form 8996, line 15.",
        control: Control::YesNo { yes: "c3_15[0]", no: "c3_15[1]" },
        follow_ups: &[(
            FollowUpWhen::Yes,
            FollowUp { key: "b25_amount", label: "Amount from Form 8996, line 15", field: "f3_10[0]", kind: InputKind::Money },
        )],
        refs: &[FORM_8996],
        yes_warning: "Question 25 is Yes, so Form 8996 has to be completed and attached. This program \
                      does not produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b26",
        number: "26",
        page: 3,
        text: "Enter the number of foreign partners subject to section 864(c)(8) as a result of \
               transferring all or a portion of an interest in the partnership or of receiving a \
               distribution from the partnership.",
        control: Control::Entry { field: "f3_11[0]", kind: InputKind::Count },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b27",
        number: "27",
        page: 3,
        text: "At any time during the tax year, were there any transfers between the partnership and \
               its partners subject to the disclosure requirements of Regulations section 1.707-8?",
        control: Control::YesNo { yes: "c3_16[0]", no: "c3_16[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b28",
        number: "28",
        page: 4,
        text: "Since December 22, 2017, did a foreign corporation directly or indirectly acquire \
               substantially all of the properties constituting a trade or business of your \
               partnership, and was the ownership percentage (by vote or value) for purposes of \
               section 7874 greater than 50% (for example, the partners held more than 50% of the \
               stock of the foreign corporation)? If \u{201c}Yes,\u{201d} list the ownership percentage \
               by vote and by value. See instructions.",
        control: Control::YesNo { yes: "c4_1[0]", no: "c4_1[1]" },
        follow_ups: &[
            (FollowUpWhen::Yes, FollowUp { key: "b28_vote", label: "Ownership percentage by vote", field: "f4_01[0]", kind: InputKind::Percent }),
            (FollowUpWhen::Yes, FollowUp { key: "b28_value", label: "Ownership percentage by value", field: "f4_02[0]", kind: InputKind::Percent }),
        ],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b29a",
        number: "29a",
        page: 4,
        text: "Is the partnership required to file Form 7208, Excise Tax on Repurchase of Corporate \
               Stock, under the applicable foreign corporation rules? See instructions.",
        control: Control::YesNo { yes: "c4_2[0]", no: "c4_2[1]" },
        follow_ups: &[],
        refs: &[FORM_7208],
        yes_warning: "Question 29a is Yes, so Form 7208 has to be completed. This program does not \
                      produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b29b",
        number: "29b",
        page: 4,
        text: "Is the partnership required to file Form 7208 under the covered surrogate foreign \
               corporation rules? If \u{201c}Yes\u{201d} to either (a) or (b), complete Form 7208. See \
               the Instructions for Form 7208.",
        control: Control::YesNo { yes: "c4_3[0]", no: "c4_3[1]" },
        follow_ups: &[],
        refs: &[FORM_7208],
        yes_warning: "Question 29b is Yes, so Form 7208 has to be completed. This program does not \
                      produce it.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b30",
        number: "30",
        page: 4,
        text: "At any time during this tax year, did the partnership (a) receive (as a reward, award, \
               or payment for property or services); or (b) sell, exchange, or otherwise dispose of a \
               digital asset (or financial interest in a digital asset)? See instructions.",
        control: Control::YesNo { yes: "c4_4[0]", no: "c4_4[1]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b31",
        number: "31",
        page: 4,
        text: "Is the partnership electing out of the centralized partnership audit regime under \
               section 6221(b)? See instructions. If \u{201c}Yes,\u{201d} the partnership must complete \
               Schedule B-2 (Form 1065); enter the total from Schedule B-2, Part III, line 3. If \
               \u{201c}No,\u{201d} complete the Designation of Partnership Representative below.",
        control: Control::YesNo { yes: "c4_6[0]", no: "c4_6[1]" },
        follow_ups: &[(
            FollowUpWhen::Yes,
            FollowUp { key: "b31_total", label: "Total from Schedule B-2, Part III, line 3", field: "f4_03[0]", kind: InputKind::Count },
        )],
        refs: &[SCHEDULE_B2],
        yes_warning: "Question 31 is Yes, so Schedule B-2 is attached, listing every partner as an \
                      eligible partner. Check each type code and TIN — an incomplete Part I is \
                      grounds for the IRS to treat the election as invalid.",
        reserved: false,
        depends_on: None,
    },
    Question {
        key: "b32",
        number: "32",
        page: 4,
        text: "Check this box if an election out of subchapter K under section 761 is being made. See \
               instructions.",
        control: Control::Check { field: "c4_5[0]" },
        follow_ups: &[],
        refs: &[],
        yes_warning: "",
        reserved: false,
        depends_on: None,
    },
];

/// The Designation of Partnership Representative block that closes Schedule B.
///
/// Its own list rather than a [`Question`], because it is not a question: it is
/// a name and address, and every partnership that has not elected out of the
/// centralized audit regime must give one. Keyed and stored exactly like the
/// answers so the desktop and the filler treat both the same way.
pub const PARTNERSHIP_REP: &[FollowUp] = &[
    FollowUp { key: "pr_first",    label: "First name of PR (or entity name)", field: "f4_04[0]", kind: InputKind::Text },
    FollowUp { key: "pr_last",     label: "Last name of PR",                   field: "f4_05[0]", kind: InputKind::Text },
    FollowUp { key: "pr_street",   label: "U.S. address of PR — street",       field: "f4_06[0]", kind: InputKind::Text },
    FollowUp { key: "pr_city",     label: "City",                              field: "f4_07[0]", kind: InputKind::Text },
    FollowUp { key: "pr_state",    label: "State",                             field: "f4_08[0]", kind: InputKind::Text },
    FollowUp { key: "pr_zip",      label: "ZIP code",                          field: "f4_09[0]", kind: InputKind::Text },
    FollowUp { key: "pr_phone",    label: "U.S. phone number of PR",           field: "f4_10[0]", kind: InputKind::Text },
    FollowUp { key: "di_first",    label: "First name of DI (if PR is an entity)", field: "f4_11[0]", kind: InputKind::Text },
    FollowUp { key: "di_last",     label: "Last name of DI",                   field: "f4_12[0]", kind: InputKind::Text },
    FollowUp { key: "di_street",   label: "U.S. address of DI — street",       field: "f4_13[0]", kind: InputKind::Text },
    FollowUp { key: "di_city",     label: "City",                              field: "f4_14[0]", kind: InputKind::Text },
    FollowUp { key: "di_state",    label: "State",                             field: "f4_15[0]", kind: InputKind::Text },
    FollowUp { key: "di_zip",      label: "ZIP code",                          field: "f4_16[0]", kind: InputKind::Text },
    FollowUp { key: "di_phone",    label: "U.S. phone number of DI",           field: "f4_17[0]", kind: InputKind::Text },
];

/// Look a question up by its storage key.
pub fn question(key: &str) -> Option<&'static Question> {
    QUESTIONS.iter().find(|q| q.key == key)
}

// ---------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------

/// The answers given for one tax year.
///
/// A missing key is an unanswered question, which is a different thing from
/// "No" and is left blank on the form. The IRS reads a blank as unanswered too,
/// so nothing is invented here — [`fill`] warns about them instead.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleB {
    answers: BTreeMap<String, String>,
}

/// The value stored for a ticked yes/no or a ticked lone box.
pub const YES: &str = "yes";
pub const NO: &str = "no";

impl ScheduleB {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.answers.get(key).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.answers.is_empty()
    }

    /// Every answer given, as (key, value).
    pub fn answers(&self) -> impl Iterator<Item = (&str, &str)> {
        self.answers.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.answers.len()
    }

    /// Set an answer, or clear it when the value is empty.
    ///
    /// Clearing on empty is what makes "I typed a number and then deleted it"
    /// mean *unanswered* rather than *answered with the empty string* — the
    /// latter would print nothing on the form and count as answered in the
    /// warnings, which is the combination nobody could debug from the page.
    pub fn set(&mut self, key: &str, value: &str) {
        if value.trim().is_empty() {
            self.answers.remove(key);
        } else {
            self.answers.insert(key.to_string(), value.trim().to_string());
        }
    }

    /// Whether a question's condition is satisfied, so it applies at all.
    ///
    /// An unconditional question always applies. A conditional one applies only
    /// while the question it hangs off holds the value it names — question 16b
    /// is a question about the 1099s you owed, and if you owed none it is not a
    /// question you can answer.
    pub fn applies(&self, q: &Question) -> bool {
        if q.reserved {
            return false;
        }
        match q.depends_on {
            None => true,
            Some(d) => self.get(d.question) == Some(d.value),
        }
    }

    /// The questions with no answer, as printed numbers.
    ///
    /// [`Control::Entry`] questions are excluded: "enter the number of Forms
    /// 8865 attached" is correctly blank when there are none, and warning about
    /// it every time would train the reader to ignore the panel. So are reserved
    /// numbers and questions whose condition does not hold — neither is something
    /// anybody failed to do.
    pub fn unanswered(&self) -> Vec<&'static str> {
        QUESTIONS
            .iter()
            .filter(|q| self.applies(q))
            .filter(|q| !matches!(q.control, Control::Entry { .. }))
            // A lone checkbox has no unanswered state — see `Control::Check`.
            .filter(|q| !matches!(q.control, Control::Check { .. }))
            .filter(|q| self.get(q.key).is_none())
            .map(|q| q.number)
            .collect()
    }
}

/// Read one year's answers.
pub fn load(conn: &Connection, tax_year: i32) -> ScheduleB {
    let mut answers = BTreeMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT answer_key, value FROM schedule_b_answers WHERE tax_year = ?1")
    {
        if let Ok(rows) = stmt.query_map([tax_year], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            for (k, v) in rows.flatten() {
                answers.insert(k, v);
            }
        }
    }
    ScheduleB { answers }
}

#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    #[error("Schedule B has no answer keyed {0:?}")]
    UnknownKey(String),
    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),
}

/// Whether a key is one this catalogue knows — a question, one of its
/// follow-ups, or a partnership-representative box.
///
/// Checked on the way in rather than trusted, for the same reason
/// [`super::lines::set_account_line`] checks its line key: a stored key nothing
/// recognises is an answer that will never reach the form, and it looks saved.
pub fn known_key(key: &str) -> bool {
    QUESTIONS.iter().any(|q| {
        q.key == key || q.follow_ups.iter().any(|(_, f)| f.key == key)
    }) || PARTNERSHIP_REP.iter().any(|f| f.key == key)
}

/// Save one answer for one year, writing straight to the table.
///
/// **Not the command path.** Since migration 027 the answers are event-sourced;
/// [`crate::commands::tax_setup_commands::set_schedule_b_answer`] is what a UI
/// calls. This remains for the projector and for tests — a row written here is
/// deleted by the next rebuild and never reaches a colleague.
pub fn set_answer(
    conn: &Connection,
    tax_year: i32,
    key: &str,
    value: &str,
) -> Result<(), AnswerError> {
    if !known_key(key) {
        return Err(AnswerError::UnknownKey(key.to_string()));
    }
    let value = value.trim();
    if value.is_empty() {
        conn.execute(
            "DELETE FROM schedule_b_answers WHERE tax_year = ?1 AND answer_key = ?2",
            rusqlite::params![tax_year, key],
        )?;
        return Ok(());
    }
    conn.execute(
        "INSERT INTO schedule_b_answers (tax_year, answer_key, value, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(tax_year, answer_key)
         DO UPDATE SET value = ?3, updated_at = datetime('now')",
        rusqlite::params![tax_year, key, value],
    )?;
    Ok(())
}

/// Copy every answer from one year to another, writing straight to the table.
///
/// **Not the command path** — see [`set_answer`] and
/// [`crate::commands::tax_setup_commands::copy_schedule_b_year`].
///
/// Skips any the target year has already answered.
///
/// Offered because most of Schedule B is the same year on year for a small
/// partnership, and retyping thirty answers invites the mistake this is all
/// meant to prevent. It is deliberately an explicit act with a year named on
/// each side, never an automatic fallback: a carried-over answer that nobody
/// chose to carry is last year's fact on this year's signed return.
///
/// Returns how many answers were copied.
pub fn copy_year(conn: &Connection, from: i32, to: i32) -> Result<usize, AnswerError> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO schedule_b_answers (tax_year, answer_key, value, updated_at)
         SELECT ?2, answer_key, value, datetime('now')
           FROM schedule_b_answers
          WHERE tax_year = ?1",
        rusqlite::params![from, to],
    )?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Filling
// ---------------------------------------------------------------------------

/// Write the answers onto the form, returning what somebody should know before
/// filing.
///
/// A follow-up whose question was not answered yes is *not* written, even when a
/// value is stored for it: an amount sitting beside a "No" is a contradiction on
/// a signed return, and the stored value is kept only so that flipping the answer
/// back does not lose what was typed.
pub fn fill(
    doc: &mut Document,
    map: &FieldMap,
    answers: &ScheduleB,
) -> Result<Vec<String>, FormError> {
    let mut warnings = Vec::new();

    for q in QUESTIONS {
        // A reserved number has nothing to answer, and a question whose condition
        // does not hold is not asking. Neither is written, whatever is stored:
        // the stored value is kept only so that flipping the governing answer
        // back does not lose what was already decided.
        if !answers.applies(q) {
            if let (Some(d), Some(_)) = (q.depends_on, answers.get(q.key)) {
                warnings.push(format!(
                    "Question {} has an answer but {}, so it was left blank on the form.",
                    q.number, d.label
                ));
            }
            continue;
        }
        let given = answers.get(q.key);
        match q.control {
            Control::YesNo { yes, no } => match given {
                Some(YES) => set_check(doc, map, yes, "1")?,
                Some(NO) => set_check(doc, map, no, "2")?,
                Some(other) => warnings.push(format!(
                    "Question {} is stored as {other:?}, which is neither yes nor no, so it was \
                     left blank. Answer it again.",
                    q.number
                )),
                None => {}
            },
            Control::Choice(options) => {
                if let Some(v) = given {
                    match options.iter().find(|o| o.key == v) {
                        // Exactly one box is ticked and the others are left
                        // alone; they start off, and nothing here ever turns one
                        // on without turning the rest off first, because only one
                        // is ever written.
                        Some(opt) => set_check(doc, map, opt.field, opt.on)?,
                        None => warnings.push(format!(
                            "Question {} is stored as {v:?}, which is not one of its choices, so it \
                             was left blank. Answer it again.",
                            q.number
                        )),
                    }
                }
            }
            Control::Check { field } => {
                if given == Some(YES) {
                    set_check(doc, map, field, "1")?;
                }
            }
            Control::Entry { field, .. } => {
                if let Some(v) = given {
                    set_text(doc, map, field, v)?;
                }
            }
        }

        for (when, f) in q.follow_ups {
            let show = match when {
                FollowUpWhen::Always => true,
                FollowUpWhen::Yes => given == Some(YES),
                FollowUpWhen::No => given == Some(NO),
                FollowUpWhen::Choice(k) => given == Some(*k),
            };
            if !show {
                continue;
            }
            match answers.get(f.key) {
                Some(v) => set_text(doc, map, f.field, v)?,
                None => warnings.push(format!(
                    "Question {} needs {} and none was given, so that box is blank.",
                    q.number,
                    f.label.to_lowercase()
                )),
            }
        }

        if given == Some(YES) && !q.yes_warning.is_empty() {
            warnings.push(q.yes_warning.to_string());
        }
    }

    // The partnership representative is owed by everyone who did not elect out.
    // Written whatever question 31 says, because a designation that was filled in
    // and then silently dropped is worse than one that is redundant.
    let mut rep_given = 0usize;
    for f in PARTNERSHIP_REP {
        if let Some(v) = answers.get(f.key) {
            set_text(doc, map, f.field, v)?;
            rep_given += 1;
        }
    }
    if answers.get("b31") != Some(YES) && rep_given == 0 {
        warnings.push(
            "No partnership representative is designated. Every partnership that has not elected \
             out of the centralized audit regime must name one, and question 31 is not Yes."
                .to_string(),
        );
    }

    let missing = answers.unanswered();
    if !missing.is_empty() {
        warnings.push(format!(
            "Schedule B question(s) {} have no answer and are blank on the form. The IRS reads a \
             blank as unanswered.",
            missing.join(", ")
        ));
    }

    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::acroform::{field_map, on_states, strip_xfa};

    const F1065: &[u8] = include_bytes!("../../assets/irs/f1065.pdf");

    fn form() -> (Document, FieldMap) {
        let mut doc = Document::load_mem(F1065).unwrap();
        strip_xfa(&mut doc);
        let map = field_map(&doc);
        (doc, map)
    }

    /// The check that catches a new revision having renumbered the schedule: every
    /// field this catalogue names must exist, and every appearance state it writes
    /// must be one the widget actually has. Without it, a renumbered form ticks the
    /// neighbouring box and the return is wrong in a way nobody reads off the page.
    #[test]
    fn every_field_in_the_catalogue_is_in_the_vendored_form() {
        let (doc, map) = form();
        for q in QUESTIONS {
            match q.control {
                Control::YesNo { yes, no } => {
                    assert!(map.find(yes).is_some(), "q{} yes box {yes} missing", q.number);
                    assert!(map.find(no).is_some(), "q{} no box {no} missing", q.number);
                    assert_eq!(on_states(&doc, &map, yes), vec!["1"], "q{} yes state", q.number);
                    assert_eq!(on_states(&doc, &map, no), vec!["2"], "q{} no state", q.number);
                }
                Control::Choice(opts) => {
                    for o in opts {
                        assert!(map.find(o.field).is_some(), "q{} {} box missing", q.number, o.key);
                        assert_eq!(
                            on_states(&doc, &map, o.field),
                            vec![o.on.to_string()],
                            "q{} {} state",
                            q.number,
                            o.key
                        );
                    }
                }
                Control::Check { field } => {
                    assert!(map.find(field).is_some(), "q{} box {field} missing", q.number);
                    assert_eq!(on_states(&doc, &map, field), vec!["1"], "q{} state", q.number);
                }
                Control::Entry { field, .. } => {
                    assert!(map.find(field).is_some(), "q{} entry {field} missing", q.number);
                }
            }
            for (_, f) in q.follow_ups {
                assert!(map.find(f.field).is_some(), "q{} follow-up {} missing", q.number, f.key);
            }
        }
        for f in PARTNERSHIP_REP {
            assert!(map.find(f.field).is_some(), "PR field {} missing", f.key);
        }
    }

    /// Two questions sharing a storage key would silently answer both from one
    /// click; two sharing a PDF field would tick a box the reader never chose.
    #[test]
    fn keys_and_fields_are_unique() {
        let mut keys = std::collections::HashSet::new();
        let mut fields = std::collections::HashSet::new();
        for q in QUESTIONS {
            assert!(keys.insert(q.key), "duplicate question key {}", q.key);
            for (_, f) in q.follow_ups {
                assert!(keys.insert(f.key), "duplicate follow-up key {}", f.key);
                assert!(fields.insert(f.field), "duplicate field {}", f.field);
            }
            match q.control {
                Control::YesNo { yes, no } => {
                    assert!(fields.insert(yes), "duplicate field {yes}");
                    assert!(fields.insert(no), "duplicate field {no}");
                }
                Control::Choice(opts) => {
                    for o in opts {
                        assert!(fields.insert(o.field), "duplicate field {}", o.field);
                    }
                }
                Control::Check { field } | Control::Entry { field, .. } => {
                    assert!(fields.insert(field), "duplicate field {field}");
                }
            }
        }
        for f in PARTNERSHIP_REP {
            assert!(keys.insert(f.key), "duplicate PR key {}", f.key);
            assert!(fields.insert(f.field), "duplicate PR field {}", f.field);
        }
    }

    /// Every reference is a live-looking IRS link. Checked as a shape, not fetched:
    /// a typo here sends somebody to a 404 while they are deciding whether they owe
    /// a separate filing.
    #[test]
    fn every_form_reference_points_at_irs_or_fincen() {
        for q in QUESTIONS {
            for r in q.refs {
                assert!(
                    r.url.starts_with("https://www.irs.gov/"),
                    "q{} reference {} has a non-IRS url {}",
                    q.number,
                    r.name,
                    r.url
                );
                assert!(!r.name.is_empty(), "q{} has a nameless reference", q.number);
            }
        }
    }

    /// An answer keyed to nothing is an answer that never reaches the form.
    #[test]
    fn an_unknown_key_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::migrations::init_schema(&conn).unwrap();
        let err = set_answer(&conn, 2025, "b99", YES).unwrap_err();
        assert!(matches!(err, AnswerError::UnknownKey(_)), "{err:?}");
    }

    #[test]
    fn answers_round_trip_per_year() {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::migrations::init_schema(&conn).unwrap();
        set_answer(&conn, 2025, "b5", NO).unwrap();
        set_answer(&conn, 2024, "b5", YES).unwrap();

        assert_eq!(load(&conn, 2025).get("b5"), Some(NO));
        assert_eq!(load(&conn, 2024).get("b5"), Some(YES));
        // A year nobody has answered starts empty rather than inheriting.
        assert!(load(&conn, 2023).is_empty());
    }

    /// Clearing an answer must leave the question unanswered, not answered with
    /// the empty string — the two look identical on the form and differ in the
    /// warnings.
    #[test]
    fn clearing_an_answer_makes_the_question_unanswered_again() {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::migrations::init_schema(&conn).unwrap();
        set_answer(&conn, 2025, "b5", YES).unwrap();
        set_answer(&conn, 2025, "b5", "").unwrap();
        assert_eq!(load(&conn, 2025).get("b5"), None);
    }

    #[test]
    fn copying_a_year_never_overwrites_an_answer_already_given() {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::migrations::init_schema(&conn).unwrap();
        set_answer(&conn, 2024, "b5", YES).unwrap();
        set_answer(&conn, 2024, "b6", YES).unwrap();
        set_answer(&conn, 2025, "b5", NO).unwrap();

        let copied = copy_year(&conn, 2024, 2025).unwrap();
        assert_eq!(copied, 1, "only the unanswered question should come across");

        let y = load(&conn, 2025);
        assert_eq!(y.get("b5"), Some(NO), "the answer already given must win");
        assert_eq!(y.get("b6"), Some(YES));
    }

    #[test]
    fn a_yes_and_a_no_tick_the_boxes_the_form_expects() {
        let (mut doc, map) = form();
        let mut a = ScheduleB::default();
        a.set("b5", YES);
        a.set("b6", NO);
        fill(&mut doc, &map, &a).unwrap();

        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "c2_7[0]").as_deref(),
            Some("/1"),
            "question 5 Yes"
        );
        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "c2_8[1]").as_deref(),
            Some("/2"),
            "question 6 No"
        );
    }

    /// The contradiction this prevents: an amount printed beside a "No".
    #[test]
    fn a_follow_up_is_not_printed_when_the_answer_is_no() {
        let (mut doc, map) = form();
        let mut a = ScheduleB::default();
        a.set("b22", NO);
        a.set("b22_amount", "1234");
        fill(&mut doc, &map, &a).unwrap();

        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "f3_9[0]"),
            None,
            "the disallowed-deduction box must stay blank beside a No"
        );
    }

    #[test]
    fn a_follow_up_is_printed_when_the_answer_is_yes() {
        let (mut doc, map) = form();
        let mut a = ScheduleB::default();
        a.set("b22", YES);
        a.set("b22_amount", "1234");
        fill(&mut doc, &map, &a).unwrap();

        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "f3_9[0]").as_deref(),
            Some("1234")
        );
    }

    #[test]
    fn the_entity_type_choice_ticks_exactly_one_box() {
        let (mut doc, map) = form();
        let mut a = ScheduleB::default();
        a.set("b1", "llc");
        fill(&mut doc, &map, &a).unwrap();

        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "c2_1[2]").as_deref(),
            Some("/3"),
            "1c domestic LLC"
        );
        // The other five read back as the off state they were built with. Asserted
        // rather than ignored: two ticked boxes on a pick-one question is a return
        // the IRS rejects, and the XFA logic that used to enforce exclusivity is
        // stripped out of every document this program produces.
        for other in ["c2_1[0]", "c2_1[1]", "c2_1[3]", "c2_1[4]", "c2_1[5]"] {
            assert_eq!(
                crate::tax::acroform::get_value(&doc, &map, other).as_deref(),
                Some("/Off"),
                "{other} must be left alone"
            );
        }
    }

    /// A yes that obliges a separate filing has to say so. Silence here is the
    /// failure mode the whole panel exists for.
    #[test]
    fn a_yes_that_needs_another_form_warns_about_it() {
        let (mut doc, map) = form();
        let mut a = ScheduleB::default();
        a.set("b24", YES);
        let warnings = fill(&mut doc, &map, &a).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("Form 8990")),
            "{warnings:?}"
        );
    }

    /// A reserved number is not a question. Nothing may tick it, and nobody may
    /// be told they failed to answer it.
    #[test]
    fn a_reserved_number_is_never_shown_answered_or_written() {
        let q = question("b10e").expect("10e is in the catalogue");
        assert!(q.reserved);

        let mut a = ScheduleB::default();
        assert!(!a.applies(q), "a reserved number never applies");
        assert!(
            !a.unanswered().contains(&"10e"),
            "nobody failed to answer a question that does not exist"
        );

        // Even with a value forced in, nothing reaches the form.
        a.set("b10e", YES);
        let (mut doc, map) = form();
        fill(&mut doc, &map, &a).unwrap();
        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "c3_3[0]").as_deref(),
            Some("/Off"),
            "a reserved box must stay untouched"
        );
    }

    /// 16b hangs off 16a. Answered while 16a is No it would read as "we had
    /// 1099s to file and did not file them", which is the opposite of the truth.
    #[test]
    fn a_conditional_question_only_applies_once_its_condition_holds() {
        let q = question("b16b").expect("16b is in the catalogue");
        let d = q.depends_on.expect("16b hangs off 16a");
        assert_eq!(d.question, "b16a");
        assert_eq!(d.value, YES);

        let mut a = ScheduleB::default();
        assert!(!a.applies(q), "unanswered 16a leaves 16b inapplicable");
        assert!(!a.unanswered().contains(&"16b"));

        a.set("b16a", NO);
        assert!(!a.applies(q), "16a No leaves 16b inapplicable");
        assert!(!a.unanswered().contains(&"16b"));

        a.set("b16a", YES);
        assert!(a.applies(q), "16a Yes brings 16b into play");
        assert!(a.unanswered().contains(&"16b"), "and now it is owed");
    }

    /// An answer stored against a condition that no longer holds is kept but not
    /// printed, and the mismatch is said out loud rather than silently dropped.
    #[test]
    fn a_stale_conditional_answer_is_withheld_from_the_form_and_reported() {
        let mut a = ScheduleB::default();
        a.set("b16a", YES);
        a.set("b16b", YES);

        let (mut doc, map) = form();
        fill(&mut doc, &map, &a).unwrap();
        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "c3_8[0]").as_deref(),
            Some("/1"),
            "16b prints while 16a is Yes"
        );

        // 16a flips to No; 16b's stored answer must not reach the page.
        a.set("b16a", NO);
        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &a).unwrap();
        assert_eq!(
            crate::tax::acroform::get_value(&doc, &map, "c3_8[0]").as_deref(),
            Some("/Off"),
            "16b must be blank once 16a is No"
        );
        assert!(
            warnings.iter().any(|w| w.contains("16b")),
            "the withheld answer has to be reported: {warnings:?}"
        );
        // The answer is kept, so flipping 16a back does not lose it.
        assert_eq!(a.get("b16b"), Some(YES));
    }

    #[test]
    fn unanswered_questions_are_named() {
        let mut a = ScheduleB::default();
        for q in QUESTIONS {
            a.set(q.key, YES);
        }
        assert!(a.unanswered().is_empty(), "{:?}", a.unanswered());
        // 10e is reserved and 16b hangs off 16a, so neither can ever be nagged
        // about even when every other question is blank.
        let empty = ScheduleB::default();
        assert!(!empty.unanswered().contains(&"10e"));
        assert!(!empty.unanswered().contains(&"16b"));

        let empty = ScheduleB::default();
        let missing = empty.unanswered();
        assert!(missing.contains(&"5"), "{missing:?}");
        // Counts and lone checkboxes are correctly blank when there is nothing to
        // report, so they are not nagged about.
        assert!(!missing.contains(&"15"), "{missing:?}");
        assert!(!missing.contains(&"11"), "{missing:?}");
    }

    /// Nobody designated a representative and question 31 is not Yes — the one
    /// omission on this schedule that is always wrong.
    #[test]
    fn a_missing_partnership_representative_is_called_out() {
        let (mut doc, map) = form();
        let warnings = fill(&mut doc, &map, &ScheduleB::default()).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("partnership representative")),
            "{warnings:?}"
        );
    }

    #[test]
    fn electing_out_does_not_demand_a_representative() {
        let (mut doc, map) = form();
        let mut a = ScheduleB::default();
        a.set("b31", YES);
        let warnings = fill(&mut doc, &map, &a).unwrap();
        assert!(
            !warnings.iter().any(|w| w.contains("No partnership representative")),
            "{warnings:?}"
        );
    }
}
