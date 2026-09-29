//! Schedule D and Form 8949, built from the 1099-B — not from our lot register.
//!
//! # The 1099-B is the source of the filed numbers
//!
//! INVESTMENTS-SPEC.md §8, and it reverses the order that seems natural. Almost
//! everything in a brokerage account is a **covered** transaction: the broker
//! reports the basis to the IRS, has already applied the wash-sale rule across the
//! whole account, and knows about the corporate actions our importer held for
//! review. The IRS holds a copy of that form. A return built from our arithmetic
//! would therefore have to *win an argument* against a document the other side
//! already has, over figures that agree in the ordinary case anyway.
//!
//! So this module transcribes the form. The ledger's realized gains are the
//! cross-check, and they live in [`super::investment_reconciliation`], which
//! reports differences and adjusts nothing.
//!
//! # Why most brokerage years need no Form 8949 at all
//!
//! Form 8949 exists to *list* transactions the IRS cannot otherwise check. It does
//! not need to list a sale whose basis the broker already reported and did not
//! adjust. So categories **A** (short term) and **D** (long term), with no
//! adjustment on them, are entered on Schedule D lines **1a** and **8a** as
//! subtotals and no Form 8949 is filed. For an ordinary brokerage year that is the
//! entire capital-gains part of the return: six figures, not a transaction listing.
//!
//! Form 8949 is built here only for what the form actually requires it for:
//!
//! | Why | Categories |
//! |---|---|
//! | basis was not reported to the IRS | B, E |
//! | the sale was not reported on a 1099-B at all | C, F |
//! | the transaction carries an adjustment | any, including A and D |
//!
//! # What is computed and what is supplied
//!
//! A straight total of captured figures is computed: a category's gain from its
//! proceeds, basis and adjustment; a line from the categories that reach it; the
//! short-term and long-term nets. A judgement is not: which lot method the broker
//! used, whether shares were inherited, whether a sale was a wash sale and by how
//! much. Those come from the statement or from the person, and where the statement
//! says one thing and arithmetic says another this module **reports both** rather
//! than choosing (see [`ScheduleD::warnings`]).
//!
//! # What this module does not do
//!
//! Fill a PDF. There is no Schedule D or Form 8949 in `assets/irs`, so there is no
//! AcroForm to fill and inventing field names for a form nobody has vendored would
//! be guessing. The figures are produced the way [`super::form1065::K1Figures`] is
//! — keyed by line, ready for a dated line map — and filling the paper is a step
//! that can be added the day the assets land without any of this changing.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::commands::tax_statement_commands;
use crate::domain::documents::{StatementLine, TaxStatement};
use crate::events::types::HoldingTerm;
use crate::tax::information_returns::FormKind;

/// One of Form 8949's six categories.
///
/// The vocabulary lives here and nowhere else: the migration stores the letter,
/// the event's validation calls [`Category::parse`], and the box codes on the
/// 1099-B catalogue are built from [`Category::code`]. Two copies of a six-member
/// set is one copy too many.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    /// Short term, basis reported to the IRS.
    A,
    /// Short term, basis **not** reported to the IRS.
    B,
    /// Short term, not reported on a 1099-B at all.
    C,
    /// Long term, basis reported to the IRS.
    D,
    /// Long term, basis **not** reported to the IRS.
    E,
    /// Long term, not reported on a 1099-B at all.
    F,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::A,
        Category::B,
        Category::C,
        Category::D,
        Category::E,
        Category::F,
    ];

    /// The lowercase letter the log and the box codes use.
    pub fn code(self) -> &'static str {
        match self {
            Category::A => "a",
            Category::B => "b",
            Category::C => "c",
            Category::D => "d",
            Category::E => "e",
            Category::F => "f",
        }
    }

    pub fn parse(s: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.code() == s)
    }

    /// The letter the form's checkbox is labelled with.
    pub fn letter(self) -> char {
        match self {
            Category::A => 'A',
            Category::B => 'B',
            Category::C => 'C',
            Category::D => 'D',
            Category::E => 'E',
            Category::F => 'F',
        }
    }

    pub fn term(self) -> HoldingTerm {
        match self {
            Category::A | Category::B | Category::C => HoldingTerm::Short,
            Category::D | Category::E | Category::F => HoldingTerm::Long,
        }
    }

    /// Which part of Form 8949 it belongs to: 1 for short term, 2 for long.
    pub fn part(self) -> u8 {
        match self.term() {
            HoldingTerm::Short => 1,
            HoldingTerm::Long => 2,
        }
    }

    /// Whether the broker reported the basis to the IRS. True only for A and D,
    /// and that is the whole reason those two can be subtotalled.
    pub fn basis_reported_to_irs(self) -> bool {
        matches!(self, Category::A | Category::D)
    }

    /// Whether the sale appears on a 1099-B at all. False for C and F, which are
    /// the sales nobody filed a form about.
    pub fn on_a_1099b(self) -> bool {
        !matches!(self, Category::C | Category::F)
    }

    /// The Schedule D line a category may be entered on as a subtotal, skipping
    /// Form 8949 — and only when nothing in it was adjusted.
    pub fn subtotal_line(self) -> Option<&'static str> {
        match self {
            Category::A => Some("1a"),
            Category::D => Some("8a"),
            _ => None,
        }
    }

    /// The Schedule D line the category reaches through Form 8949.
    pub fn form8949_line(self) -> &'static str {
        match self {
            Category::A => "1b",
            Category::B => "2",
            Category::C => "3",
            Category::D => "8b",
            Category::E => "9",
            Category::F => "10",
        }
    }

    /// What a person reads.
    pub fn label(self) -> &'static str {
        match self {
            Category::A => "Box A — short term, basis reported to the IRS",
            Category::B => "Box B — short term, basis not reported to the IRS",
            Category::C => "Box C — short term, not reported on a 1099-B",
            Category::D => "Box D — long term, basis reported to the IRS",
            Category::E => "Box E — long term, basis not reported to the IRS",
            Category::F => "Box F — long term, not reported on a 1099-B",
        }
    }

    /// The four box codes this category's figures are recorded under on a 1099-B
    /// statement: proceeds, basis, adjustments, gain.
    pub fn box_codes(self) -> [String; 4] {
        let c = self.code();
        [
            format!("{c}_proceeds"),
            format!("{c}_basis"),
            format!("{c}_adjustments"),
            format!("{c}_gain"),
        ]
    }
}

impl std::fmt::Display for Category {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One category's figures, as the form prints them.
///
/// `adjustment_cents` follows Form 8949 column (g): **positive increases the
/// gain**, which is the direction a disallowed wash-sale loss goes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CategoryTotals {
    pub proceeds_cents: i64,
    pub basis_cents: i64,
    pub adjustment_cents: i64,
    /// The gain the broker printed, when it printed one. Kept beside
    /// [`computed_gain_cents`](CategoryTotals::computed_gain_cents) rather than
    /// replacing it: two answers that agree are a check passed, and two that
    /// disagree are something to read the paper about.
    pub reported_gain_cents: Option<i64>,
}

impl CategoryTotals {
    /// Column (h): proceeds less basis, plus the adjustment.
    pub fn computed_gain_cents(&self) -> i64 {
        self.proceeds_cents - self.basis_cents + self.adjustment_cents
    }

    /// Whether anything at all was captured here.
    pub fn is_empty(&self) -> bool {
        self.proceeds_cents == 0
            && self.basis_cents == 0
            && self.adjustment_cents == 0
            && self.reported_gain_cents.is_none()
    }

    fn add(&mut self, other: &CategoryTotals) {
        self.proceeds_cents += other.proceeds_cents;
        self.basis_cents += other.basis_cents;
        self.adjustment_cents += other.adjustment_cents;
        if let Some(gain) = other.reported_gain_cents {
            *self.reported_gain_cents.get_or_insert(0) += gain;
        }
    }
}

/// One broker's 1099-B for one year, read off the statement it was captured as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brokerage1099B {
    pub statement_id: String,
    /// Who sent it.
    pub broker: String,
    pub tax_year: i32,
    /// Box 4.
    pub withheld_cents: i64,
    /// The categories the statement says something about, in letter order.
    pub categories: BTreeMap<Category, CategoryTotals>,
    /// The transaction detail entered against it, in the order Form 8949 prints
    /// it. Empty for an ordinary covered-only year.
    pub lines: Vec<StatementLine>,
}

impl Brokerage1099B {
    /// Read a captured statement as a 1099-B.
    ///
    /// Returns `None` for a statement of any other form: a 1099-INT has no
    /// categories, and quietly reading one as an empty 1099-B would put a broker on
    /// Schedule D that never sold anything.
    pub fn from_statement(statement: &TaxStatement, lines: Vec<StatementLine>) -> Option<Self> {
        if statement.form != FormKind::F1099B {
            return None;
        }
        let mut categories = BTreeMap::new();
        for category in Category::ALL {
            let [proceeds, basis, adjustments, gain] = category.box_codes();
            let totals = CategoryTotals {
                proceeds_cents: statement.amount(&proceeds),
                basis_cents: statement.amount(&basis),
                adjustment_cents: statement.amount(&adjustments),
                reported_gain_cents: statement.amounts.get(&gain).copied(),
            };
            if !totals.is_empty() {
                categories.insert(category, totals);
            }
        }
        Some(Brokerage1099B {
            statement_id: statement.statement_id.clone(),
            broker: statement.issuer.clone(),
            tax_year: statement.tax_year,
            withheld_cents: statement.amount("4"),
            categories,
            lines,
        })
    }

    /// Every 1099-B recorded for `year`, with its transaction detail.
    pub fn for_year(conn: &Connection, year: i32) -> Vec<Brokerage1099B> {
        tax_statement_commands::list(conn, Some(year))
            .into_iter()
            .filter(|s| s.form == FormKind::F1099B)
            .filter_map(|s| {
                let lines = tax_statement_commands::lines_of(conn, &s.statement_id);
                Brokerage1099B::from_statement(&s, lines)
            })
            .collect()
    }

    /// The detail rows in one category.
    pub fn lines_in(&self, category: Category) -> Vec<&StatementLine> {
        self.lines
            .iter()
            .filter(|l| l.category == category)
            .collect()
    }

    /// Whether this form's `category` has to be listed on Form 8949 rather than
    /// subtotalled onto Schedule D.
    ///
    /// Three reasons, and any one is enough: the basis was never reported to the
    /// IRS, the sale was never reported at all, or something in it was adjusted.
    pub fn needs_form8949(&self, category: Category) -> bool {
        if category.subtotal_line().is_none() {
            return true;
        }
        let adjusted_in_total = self
            .categories
            .get(&category)
            .is_some_and(|t| t.adjustment_cents != 0);
        adjusted_in_total
            || self
                .lines_in(category)
                .iter()
                .any(|l| l.adjustment_cents != 0 || l.adjustment_code.is_some())
    }
}

/// One Form 8949 row, ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form8949Row {
    pub category: Category,
    /// Which 1099-B it came off, so a row can always be traced back to paper.
    pub statement_id: String,
    pub broker: String,
    /// Column (a).
    pub description: String,
    /// Column (b), as printed.
    pub acquired: String,
    /// Column (c).
    pub sold_on: String,
    /// Column (d), in cents.
    pub proceeds_cents: i64,
    /// Column (e), in cents.
    pub basis_cents: i64,
    /// Column (f).
    pub adjustment_code: Option<String>,
    /// Column (g), in cents.
    pub adjustment_cents: i64,
    /// Column (h), in cents — computed from (d), (e) and (g).
    pub gain_cents: i64,
}

/// One part of Form 8949: one category's rows and their totals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form8949Part {
    pub category: Category,
    pub rows: Vec<Form8949Row>,
    /// The rows added up. What Schedule D takes from the part.
    pub totals: CategoryTotals,
}

/// One line of Schedule D, in the columns the form has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleDLine {
    pub key: &'static str,
    pub label: &'static str,
    /// Column (d). Zero on a line that has no proceeds column.
    pub proceeds_cents: i64,
    /// Column (e).
    pub basis_cents: i64,
    /// Column (g).
    pub adjustment_cents: i64,
    /// Column (h).
    pub gain_cents: i64,
}

/// A year's Schedule D, with the Form 8949 parts it needs and nothing it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleD {
    pub tax_year: i32,
    /// The lines that carry a figure, by line key: `1a`, `1b`, `2`, `3`, `5`, `7`,
    /// `8a`, `8b`, `9`, `10`, `12`, `13`, `15`, `16`. A line that came to nothing
    /// is absent.
    pub lines: BTreeMap<&'static str, ScheduleDLine>,
    /// The Form 8949 parts this return files. **Empty for an ordinary covered-only
    /// year**, which is the point of the whole module.
    pub parts: Vec<Form8949Part>,
    /// Line 7.
    pub short_term_cents: i64,
    /// Line 15.
    pub long_term_cents: i64,
    /// Line 16.
    pub net_cents: i64,
    /// What the 1099-Bs withheld — not a Schedule D line, but it has to reach
    /// Form 1040 line 25b and this is where it is read.
    pub withheld_cents: i64,
    /// Things worth reading the paper about before filing: a broker's own gain
    /// that does not match its columns, a category that has to be listed and was
    /// not, detail rows that do not add up to the subtotal they belong to.
    pub warnings: Vec<String>,
}

/// What the ledger contributes, which on Schedule D is one line and one line only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerContributions {
    /// Capital gain distributions, read from the account they are posted to
    /// (Schedule D line 13). A fund's capital gain distribution is not a sale and
    /// never reaches Form 8949 or Schedule B — see
    /// [`super::personal_schedule_b`].
    pub capital_gain_distributions_cents: i64,
}

const fn line(key: &'static str, label: &'static str) -> ScheduleDLine {
    ScheduleDLine {
        key,
        label,
        proceeds_cents: 0,
        basis_cents: 0,
        adjustment_cents: 0,
        gain_cents: 0,
    }
}

/// Every line this module can fill, so a label is written once.
fn line_label(key: &str) -> &'static str {
    match key {
        "1a" => "Short-term totals from a 1099-B with basis reported and no adjustment (no Form 8949)",
        "1b" => "Short-term, basis reported to the IRS — Form 8949 Part I, box A",
        "2" => "Short-term, basis not reported to the IRS — Form 8949 Part I, box B",
        "3" => "Short-term, not reported on a 1099-B — Form 8949 Part I, box C",
        "5" => "Net short-term gain or loss from partnerships, S corporations, estates and trusts",
        "7" => "Net short-term capital gain or loss",
        "8a" => "Long-term totals from a 1099-B with basis reported and no adjustment (no Form 8949)",
        "8b" => "Long-term, basis reported to the IRS — Form 8949 Part II, box D",
        "9" => "Long-term, basis not reported to the IRS — Form 8949 Part II, box E",
        "10" => "Long-term, not reported on a 1099-B — Form 8949 Part II, box F",
        "12" => "Net long-term gain or loss from partnerships, S corporations, estates and trusts",
        "13" => "Capital gain distributions",
        "15" => "Net long-term capital gain or loss",
        "16" => "Total — combine lines 7 and 15",
        _ => "",
    }
}

/// Build a year's Schedule D from the 1099-Bs recorded for it, the other
/// statements that reach it, and the one figure the ledger supplies.
pub fn build(conn: &Connection, year: i32, ledger: LedgerContributions) -> ScheduleD {
    let forms = Brokerage1099B::for_year(conn, year);
    let statements = tax_statement_commands::list(conn, Some(year));
    from_parts(year, &forms, &statements, ledger)
}

/// The same, from figures already in hand.
///
/// Separate from [`build`] so that the assembly can be exercised without a
/// database, and so that a caller holding the forms already does not read them
/// twice.
pub fn from_parts(
    year: i32,
    forms: &[Brokerage1099B],
    statements: &[TaxStatement],
    ledger: LedgerContributions,
) -> ScheduleD {
    let mut warnings = Vec::new();
    let mut subtotals: BTreeMap<&'static str, CategoryTotals> = BTreeMap::new();
    let mut parts: BTreeMap<Category, Form8949Part> = BTreeMap::new();
    let mut withheld_cents = 0;

    for form in forms {
        withheld_cents += form.withheld_cents;
        for (&category, totals) in &form.categories {
            // The broker's own gain against the columns it came from. Reported
            // both ways rather than resolved: the columns are what the form is
            // filed from, and a mismatch is a transcription to check.
            if let Some(reported) = totals.reported_gain_cents {
                let computed = totals.computed_gain_cents();
                if reported != computed {
                    warnings.push(format!(
                        "The {} from {} reports {} of gain in {}, and its proceeds less basis \
                         plus adjustments comes to {}. The columns are what the return is filed \
                         from; check the transcription.",
                        FormKind::F1099B.label(),
                        form.broker,
                        dollars(reported),
                        category.label(),
                        dollars(computed),
                    ));
                }
            }

            if form.needs_form8949(category) {
                let rows = rows_for(form, category);
                if rows.is_empty() {
                    warnings.push(format!(
                        "{} on the {} from {} has to be listed transaction by transaction on \
                         Form 8949 — {} — and no transactions were entered against it. The \
                         return carries the subtotal, which is what the form says, but Form 8949 \
                         cannot be printed from it.",
                        category.label(),
                        FormKind::F1099B.label(),
                        form.broker,
                        why_listed(category, totals),
                    ));
                } else {
                    let row_totals = total_rows(&rows);
                    if row_totals.proceeds_cents != totals.proceeds_cents
                        || row_totals.basis_cents != totals.basis_cents
                        || row_totals.adjustment_cents != totals.adjustment_cents
                    {
                        warnings.push(format!(
                            "The Form 8949 rows entered for {} on the {} from {} come to {} of \
                             proceeds, {} of basis and {} of adjustments, against the form's {}, \
                             {} and {}. Form 8949 has to add up to the 1099-B it is listing.",
                            category.label(),
                            FormKind::F1099B.label(),
                            form.broker,
                            dollars(row_totals.proceeds_cents),
                            dollars(row_totals.basis_cents),
                            dollars(row_totals.adjustment_cents),
                            dollars(totals.proceeds_cents),
                            dollars(totals.basis_cents),
                            dollars(totals.adjustment_cents),
                        ));
                    }
                    let part = parts.entry(category).or_insert_with(|| Form8949Part {
                        category,
                        rows: Vec::new(),
                        totals: CategoryTotals::default(),
                    });
                    part.rows.extend(rows);
                }
                // The figures the return carries are the form's, not the rows'.
                subtotals
                    .entry(category.form8949_line())
                    .or_default()
                    .add(totals);
            } else {
                subtotals
                    .entry(category.subtotal_line().expect("checked by needs_form8949"))
                    .or_default()
                    .add(totals);
            }
        }
    }

    // A part's totals are its rows', which is what makes the tie-out above worth
    // printing: the return's line and the part's footer can be compared on paper.
    for part in parts.values_mut() {
        part.totals = total_rows(&part.rows);
    }

    let mut lines: BTreeMap<&'static str, ScheduleDLine> = BTreeMap::new();
    for (key, totals) in subtotals {
        if totals.is_empty() {
            continue;
        }
        let mut l = line(key, line_label(key));
        l.proceeds_cents = totals.proceeds_cents;
        l.basis_cents = totals.basis_cents;
        l.adjustment_cents = totals.adjustment_cents;
        l.gain_cents = totals.computed_gain_cents();
        lines.insert(key, l);
    }

    // Lines 5 and 12: a partnership's, an S corporation's or a trust's share of
    // capital gain, which arrives on a K-1 and not on a 1099-B. Read from the
    // catalogue's own destinations rather than from a second list of box codes.
    let (short_flow, long_flow) = flow_through(statements);
    if short_flow != 0 {
        let mut l = line("5", line_label("5"));
        l.gain_cents = short_flow;
        lines.insert("5", l);
    }
    if long_flow != 0 {
        let mut l = line("12", line_label("12"));
        l.gain_cents = long_flow;
        lines.insert("12", l);
    }

    // Line 13. Read from the account it is posted to, because a capital gain
    // distribution cannot be told apart from an ordinary dividend by looking at a
    // posting — they are both cash arriving from a fund — and inferring it is how
    // a Schedule D line and a Schedule B line end up holding the same money.
    if ledger.capital_gain_distributions_cents != 0 {
        let mut l = line("13", line_label("13"));
        l.gain_cents = ledger.capital_gain_distributions_cents;
        lines.insert("13", l);
        let reported: i64 = statements
            .iter()
            .filter(|s| s.form == FormKind::F1099Div)
            .map(|s| s.amount("2a"))
            .sum();
        if reported != 0 && reported != ledger.capital_gain_distributions_cents {
            warnings.push(format!(
                "The books show {} of capital gain distributions and the year's 1099-DIVs report \
                 {} in box 2a. Schedule D line 13 carries the books' figure; the difference is a \
                 distribution posted somewhere else, or one the fund reported and the books never \
                 received.",
                dollars(ledger.capital_gain_distributions_cents),
                dollars(reported),
            ));
        }
    }

    let short_term_cents = ["1a", "1b", "2", "3", "5"]
        .iter()
        .filter_map(|k| lines.get(k))
        .map(|l| l.gain_cents)
        .sum();
    let long_term_cents = ["8a", "8b", "9", "10", "12", "13"]
        .iter()
        .filter_map(|k| lines.get(k))
        .map(|l| l.gain_cents)
        .sum();

    if short_term_cents != 0 {
        let mut l = line("7", line_label("7"));
        l.gain_cents = short_term_cents;
        lines.insert("7", l);
    }
    if long_term_cents != 0 {
        let mut l = line("15", line_label("15"));
        l.gain_cents = long_term_cents;
        lines.insert("15", l);
    }
    let net_cents = short_term_cents + long_term_cents;
    if net_cents != 0 {
        let mut l = line("16", line_label("16"));
        l.gain_cents = net_cents;
        lines.insert("16", l);
    }

    ScheduleD {
        tax_year: year,
        lines,
        parts: parts.into_values().collect(),
        short_term_cents,
        long_term_cents,
        net_cents,
        withheld_cents,
        warnings,
    }
}

/// Why a category has to be listed, in the words the warning needs.
fn why_listed(category: Category, totals: &CategoryTotals) -> &'static str {
    if !category.on_a_1099b() {
        "these sales were not reported on a 1099-B at all"
    } else if !category.basis_reported_to_irs() {
        "the broker did not report the basis to the IRS"
    } else if totals.adjustment_cents != 0 {
        "a transaction in it carries an adjustment, which rules out the subtotal line"
    } else {
        "a transaction in it carries an adjustment"
    }
}

fn rows_for(form: &Brokerage1099B, category: Category) -> Vec<Form8949Row> {
    form.lines_in(category)
        .into_iter()
        .map(|l| Form8949Row {
            category,
            statement_id: form.statement_id.clone(),
            broker: form.broker.clone(),
            description: l.description.clone(),
            acquired: l.acquired.as_printed(),
            sold_on: l.sold_on.to_string(),
            proceeds_cents: l.proceeds_cents,
            basis_cents: l.basis_cents,
            adjustment_code: l.adjustment_code.clone(),
            adjustment_cents: l.adjustment_cents,
            gain_cents: l.gain_cents(),
        })
        .collect()
}

fn total_rows(rows: &[Form8949Row]) -> CategoryTotals {
    let mut totals = CategoryTotals::default();
    for row in rows {
        totals.proceeds_cents += row.proceeds_cents;
        totals.basis_cents += row.basis_cents;
        totals.adjustment_cents += row.adjustment_cents;
    }
    totals
}

/// Short-term and long-term capital gain arriving on somebody else's return:
/// Schedule D lines 5 and 12.
///
/// Found by destination rather than by a list of box codes, so a K-1 from a
/// partnership, from an S corporation and from a trust are all picked up by the one
/// rule — and so adding a form to the catalogue does not mean remembering to add it
/// here too.
fn flow_through(statements: &[TaxStatement]) -> (i64, i64) {
    let mut short = 0;
    let mut long = 0;
    for statement in statements {
        if statement.form == FormKind::F1099B {
            continue;
        }
        for (code, cents) in &statement.amounts {
            match statement.form.box_def(code) {
                Some(def) if def.summed && def.destination == "Schedule D, line 5" => short += cents,
                Some(def) if def.summed && def.destination == "Schedule D, line 12" => {
                    long += cents
                }
                _ => {}
            }
        }
    }
    (short, long)
}

/// Cents as a signed dollar amount, for a warning somebody reads.
fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.unsigned_abs();
    format!("{sign}${}.{:02}", cents / 100, cents % 100)
}

/// The categories a form says something about, for a caller that wants them
/// without walking the map.
pub fn categories_present(forms: &[Brokerage1099B]) -> BTreeSet<Category> {
    forms.iter().flat_map(|f| f.categories.keys().copied()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::documents::{Acquired, StatementSource};
    use chrono::NaiveDate;

    const YEAR: i32 = 2025;

    fn statement(broker: &str, boxes: &[(&str, i64)]) -> TaxStatement {
        TaxStatement {
            statement_id: format!("s-{broker}"),
            tax_year: YEAR,
            form: FormKind::F1099B,
            issuer: broker.to_string(),
            amounts: boxes.iter().map(|(c, v)| (c.to_string(), *v)).collect(),
            document_ids: Vec::new(),
            source: StatementSource::Entered,
            note: None,
        }
    }

    fn detail(
        statement_id: &str,
        line_id: &str,
        category: Category,
        description: &str,
        proceeds: i64,
        basis: i64,
        adjustment: Option<(&str, i64)>,
    ) -> StatementLine {
        StatementLine {
            statement_id: statement_id.to_string(),
            line_id: line_id.to_string(),
            category,
            description: description.to_string(),
            acquired: Acquired::On(NaiveDate::from_ymd_opt(2024, 3, 4).unwrap()),
            sold_on: NaiveDate::from_ymd_opt(YEAR, 6, 5).unwrap(),
            proceeds_cents: proceeds,
            basis_cents: basis,
            adjustment_code: adjustment.map(|(c, _)| c.to_string()),
            adjustment_cents: adjustment.map_or(0, |(_, a)| a),
        }
    }

    /// The common case, and the one the whole module is shaped around.
    #[test]
    fn a_covered_only_year_fills_lines_1a_and_8a_and_files_no_form_8949() {
        let s = statement(
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 1_250_000),
                ("a_basis", 1_100_000),
                ("a_gain", 150_000),
                ("d_proceeds", 4_000_000),
                ("d_basis", 3_250_000),
                ("d_gain", 750_000),
                ("4", 0),
            ],
        );
        let form = Brokerage1099B::from_statement(&s, Vec::new()).unwrap();
        let d = from_parts(YEAR, &[form], &[s], LedgerContributions::default());

        assert!(d.parts.is_empty(), "no Form 8949 for a covered-only year");
        let l1a = d.lines.get("1a").expect("line 1a carries box A");
        assert_eq!(l1a.proceeds_cents, 1_250_000);
        assert_eq!(l1a.basis_cents, 1_100_000);
        assert_eq!(l1a.adjustment_cents, 0);
        assert_eq!(l1a.gain_cents, 150_000);
        let l8a = d.lines.get("8a").expect("line 8a carries box D");
        assert_eq!(l8a.proceeds_cents, 4_000_000);
        assert_eq!(l8a.basis_cents, 3_250_000);
        assert_eq!(l8a.gain_cents, 750_000);
        assert!(!d.lines.contains_key("1b"));
        assert!(!d.lines.contains_key("8b"));
        assert_eq!(d.short_term_cents, 150_000);
        assert_eq!(d.long_term_cents, 750_000);
        assert_eq!(d.net_cents, 900_000);
        assert_eq!(d.lines.get("16").unwrap().gain_cents, 900_000);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    /// An adjustment is what turns a covered category into a listed one: the
    /// subtotal line cannot carry column (g).
    #[test]
    fn a_wash_sale_adjustment_moves_box_a_off_line_1a_and_onto_form_8949() {
        let s = statement(
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 500_000),
                ("a_basis", 560_000),
                ("a_adjustments", 20_000),
                ("a_gain", -40_000),
            ],
        );
        let lines = vec![
            detail(&s.statement_id, "l1", Category::A, "50 sh. ACME", 200_000, 190_000, None),
            detail(
                &s.statement_id,
                "l2",
                Category::A,
                "80 sh. ACME",
                300_000,
                370_000,
                Some(("W", 20_000)),
            ),
        ];
        let form = Brokerage1099B::from_statement(&s, lines).unwrap();
        assert!(form.needs_form8949(Category::A));
        let d = from_parts(YEAR, &[form], &[s], LedgerContributions::default());

        assert!(!d.lines.contains_key("1a"), "an adjusted category cannot be subtotalled");
        let l1b = d.lines.get("1b").expect("line 1b carries box A through Form 8949");
        assert_eq!(l1b.proceeds_cents, 500_000);
        assert_eq!(l1b.basis_cents, 560_000);
        assert_eq!(l1b.adjustment_cents, 20_000);
        assert_eq!(l1b.gain_cents, -40_000);

        assert_eq!(d.parts.len(), 1);
        let part = &d.parts[0];
        assert_eq!(part.category, Category::A);
        assert_eq!(part.rows.len(), 2);
        assert_eq!(part.rows[1].adjustment_code.as_deref(), Some("W"));
        assert_eq!(part.rows[1].adjustment_cents, 20_000);
        // Column (h) on the adjusted row: 300,000 - 370,000 + 20,000.
        assert_eq!(part.rows[1].gain_cents, -50_000);
        assert_eq!(part.rows[0].gain_cents, 10_000);
        assert_eq!(part.totals.proceeds_cents, 500_000);
        assert_eq!(part.totals.basis_cents, 560_000);
        assert_eq!(part.totals.adjustment_cents, 20_000);
        assert_eq!(d.short_term_cents, -40_000);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    /// Box E: the broker reported the sale but not the basis, so the transaction
    /// has to be listed even though nothing about it was adjusted.
    #[test]
    fn a_noncovered_category_is_listed_even_with_no_adjustment() {
        let s = statement(
            "Old Mutual Trust",
            &[
                ("e_proceeds", 900_000),
                ("e_basis", 400_000),
                ("e_gain", 500_000),
            ],
        );
        let lines = vec![detail(
            &s.statement_id,
            "l1",
            Category::E,
            "300 sh. HERITAGE CO",
            900_000,
            400_000,
            None,
        )];
        let form = Brokerage1099B::from_statement(&s, lines).unwrap();
        assert!(form.needs_form8949(Category::E));
        let d = from_parts(YEAR, &[form], &[s], LedgerContributions::default());

        assert_eq!(d.parts.len(), 1);
        assert_eq!(d.parts[0].category, Category::E);
        assert_eq!(d.lines.get("9").unwrap().gain_cents, 500_000);
        assert!(!d.lines.contains_key("8a"));
        assert_eq!(d.long_term_cents, 500_000);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    /// A category that must be listed and was not: the return still carries the
    /// form's subtotal, and says Form 8949 cannot be printed.
    #[test]
    fn a_listed_category_with_no_rows_entered_warns_and_still_carries_the_subtotal() {
        let s = statement("Old Mutual Trust", &[("b_proceeds", 100_000), ("b_basis", 60_000)]);
        let form = Brokerage1099B::from_statement(&s, Vec::new()).unwrap();
        let d = from_parts(YEAR, &[form], &[s], LedgerContributions::default());

        assert!(d.parts.is_empty());
        assert_eq!(d.lines.get("2").unwrap().gain_cents, 40_000);
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("did not report the basis to the IRS"));
        crate::tax::warning_shape::assert_all(&d.warnings);
    }

    #[test]
    fn detail_rows_that_do_not_add_up_to_the_form_are_reported() {
        let s = statement("Old Mutual Trust", &[("b_proceeds", 100_000), ("b_basis", 60_000)]);
        let lines = vec![detail(
            &s.statement_id,
            "l1",
            Category::B,
            "10 sh. HERITAGE CO",
            90_000,
            60_000,
            None,
        )];
        let form = Brokerage1099B::from_statement(&s, lines).unwrap();
        let d = from_parts(YEAR, &[form], &[s], LedgerContributions::default());
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("$900.00 of proceeds"));
        assert!(d.warnings[0].contains("the form's $1000.00"));
        // The line is the form's, not the rows'.
        assert_eq!(d.lines.get("2").unwrap().proceeds_cents, 100_000);
        crate::tax::warning_shape::assert_all(&d.warnings);
    }

    /// The books' capital gain distributions against the 1099-DIVs' box 2a. Line
    /// 13 carries the books' figure, because a distribution is read from the
    /// account it was posted to and not inferred.
    #[test]
    fn capital_gain_distributions_that_differ_from_box_2a_are_reported() {
        let div = TaxStatement {
            statement_id: "div".to_string(),
            tax_year: YEAR,
            form: FormKind::F1099Div,
            issuer: "Old Mutual Trust".to_string(),
            amounts: [("2a".to_string(), 30_000)].into_iter().collect(),
            document_ids: Vec::new(),
            source: StatementSource::Entered,
            note: None,
        };
        let d = from_parts(
            YEAR,
            &[],
            &[div],
            LedgerContributions {
                capital_gain_distributions_cents: 25_000,
            },
        );
        assert_eq!(d.lines.get("13").unwrap().gain_cents, 25_000);
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("$250.00 of capital gain distributions"));
        assert!(d.warnings[0].contains("$300.00 in box 2a"));
        crate::tax::warning_shape::assert_all(&d.warnings);
    }

    /// An adjusted covered category with no rows entered: the warning has to say
    /// the adjustment is what rules out the subtotal line, not that the basis went
    /// unreported — box A's basis was reported.
    #[test]
    fn an_adjusted_covered_category_with_no_rows_says_the_adjustment_is_why() {
        let s = statement(
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 100_000),
                ("a_basis", 120_000),
                ("a_adjustments", 5_000),
            ],
        );
        let form = Brokerage1099B::from_statement(&s, Vec::new()).unwrap();
        let d = from_parts(YEAR, &[form], &[s], LedgerContributions::default());
        assert_eq!(d.warnings.len(), 1);
        assert!(
            d.warnings[0].contains("rules out the subtotal line"),
            "{}",
            d.warnings[0]
        );
        assert_eq!(d.lines.get("1b").unwrap().gain_cents, -15_000);
        crate::tax::warning_shape::assert_all(&d.warnings);
    }

    #[test]
    fn a_brokers_own_gain_that_does_not_match_its_columns_is_reported() {
        let s = statement(
            "Broad Street Brokerage",
            &[("a_proceeds", 100_000), ("a_basis", 60_000), ("a_gain", 45_000)],
        );
        let form = Brokerage1099B::from_statement(&s, Vec::new()).unwrap();
        let d = from_parts(YEAR, &[form], &[s], LedgerContributions::default());
        assert_eq!(d.warnings.len(), 1);
        assert!(d.warnings[0].contains("$450.00 of gain"));
        assert!(d.warnings[0].contains("comes to $400.00"));
        // The columns win: the line is filed from them.
        assert_eq!(d.lines.get("1a").unwrap().gain_cents, 40_000);
        crate::tax::warning_shape::assert_all(&d.warnings);
    }

    /// Two brokers' box A add into one subtotal, because line 1a is one line.
    #[test]
    fn two_brokers_covered_totals_add_into_one_line() {
        let a = statement("Broad Street Brokerage", &[("a_proceeds", 100_000), ("a_basis", 70_000)]);
        let b = statement("Second Street Securities", &[("a_proceeds", 50_000), ("a_basis", 20_000)]);
        let forms = vec![
            Brokerage1099B::from_statement(&a, Vec::new()).unwrap(),
            Brokerage1099B::from_statement(&b, Vec::new()).unwrap(),
        ];
        let d = from_parts(YEAR, &forms, &[a, b], LedgerContributions::default());
        let l1a = d.lines.get("1a").unwrap();
        assert_eq!(l1a.proceeds_cents, 150_000);
        assert_eq!(l1a.basis_cents, 90_000);
        assert_eq!(l1a.gain_cents, 60_000);
        assert_eq!(d.parts.len(), 0);
    }

    #[test]
    fn a_k1s_capital_gain_reaches_lines_5_and_12() {
        let k1 = TaxStatement {
            statement_id: "k1".to_string(),
            tax_year: YEAR,
            form: FormKind::K1Partnership,
            issuer: "Example Art House LLC".to_string(),
            amounts: [("8".to_string(), 30_000), ("9a".to_string(), 70_000)]
                .into_iter()
                .collect(),
            document_ids: Vec::new(),
            source: StatementSource::Entered,
            note: None,
        };
        let d = from_parts(YEAR, &[], &[k1], LedgerContributions::default());
        assert_eq!(d.lines.get("5").unwrap().gain_cents, 30_000);
        assert_eq!(d.lines.get("12").unwrap().gain_cents, 70_000);
        assert_eq!(d.short_term_cents, 30_000);
        assert_eq!(d.long_term_cents, 70_000);
        assert_eq!(d.net_cents, 100_000);
    }

    #[test]
    fn every_category_knows_its_term_its_part_and_where_it_goes() {
        for c in Category::ALL {
            assert_eq!(Category::parse(c.code()), Some(c));
            assert_eq!(c.box_codes()[0], format!("{}_proceeds", c.code()));
            for code in c.box_codes() {
                assert!(
                    FormKind::F1099B.accepts_box(&code),
                    "the 1099-B catalogue is missing {code}"
                );
            }
            assert_eq!(c.part(), if c.term() == HoldingTerm::Short { 1 } else { 2 });
            assert_eq!(c.subtotal_line().is_some(), c.basis_reported_to_irs());
        }
        assert!(!Category::C.on_a_1099b() && !Category::F.on_a_1099b());
    }

    /// A 1099-INT read as a 1099-B would put a bank on Schedule D.
    #[test]
    fn only_a_1099b_reads_as_a_1099b() {
        let mut s = statement("First Bank", &[]);
        s.form = FormKind::F1099Int;
        s.amounts = [("1".to_string(), 10_000)].into_iter().collect();
        assert!(Brokerage1099B::from_statement(&s, Vec::new()).is_none());
    }
}
