//! Reading a Schedule K-1 package — the federal K-1 and the state K-1s that come
//! with it — out of a PDF.
//!
//! # What this is for
//!
//! A partnership with property in several states sends each partner one PDF: the
//! federal K-1, its supporting statements, the K-3, and a state K-1 for every
//! state the partnership files in. The federal figures go on the 1040; the state
//! ones decide which nonresident returns are owed and carry the tax the
//! partnership already paid on the partner's behalf. Typing all of that in from
//! thirty-odd pages is where figures go wrong.
//!
//! # Why every figure is offered, never applied
//!
//! A K-1 is laid out by whatever program the partnership's preparer used. This
//! reads by *position* — a figure belongs to the box whose label is printed just
//! above it — which holds across programs because the forms themselves are
//! fixed, but it can still misread an unfamiliar layout. So the result is an
//! [`K1Extraction`] the person reviews and accepts; nothing here writes to the
//! books. Where a page is recognised but a figure cannot be found, the result says
//! so in [`K1Extraction::warnings`] and leaves the figure out, rather than
//! guessing.
//!
//! # What it never keeps
//!
//! The partner's identifying number, name and address are on every page. None of
//! them are read: the result holds amounts, the partnership's name and EIN, and
//! which pages each figure came from.

use std::collections::BTreeMap;

use crate::documents::pdf_text::{Chunk, PageText, TextLine};

/// Everything read from one K-1 package.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct K1Extraction {
    pub tax_year: Option<i32>,
    /// The partnership, as Part I item B names it.
    pub issuer: Option<String>,
    pub issuer_ein: Option<String>,
    /// Federal boxes as [`crate::tax::information_returns::FormKind::K1Partnership`]
    /// codes, in cents. Empty when no federal K-1 page was found.
    pub federal: BTreeMap<String, i64>,
    /// The page the federal K-1 is on, when there is one.
    pub federal_page: Option<u32>,
    /// One per state K-1 in the package, in the order they appear.
    pub states: Vec<StateK1Extract>,
    pub warnings: Vec<String>,
}

/// One state's K-1, as read.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateK1Extract {
    /// Two-letter postal code.
    pub state: String,
    /// The form, in the state's words: "Maryland Schedule K-1 (510/511)".
    pub form: String,
    pub pages: Vec<u32>,
    /// [`state_codes`] to cents.
    pub amounts: BTreeMap<String, i64>,
    /// The state's apportionment percentage, in parts per million, where the K-1
    /// gives one.
    pub apportionment_ppm: Option<i64>,
    /// Figures this layout was expected to carry and did not yield.
    pub warnings: Vec<String>,
}

impl StateK1Extract {
    pub fn amount(&self, code: &str) -> Option<i64> {
        self.amounts.get(code).copied()
    }
}

/// The amounts a state K-1 is reduced to. Each state names them differently;
/// these are what the state returns and the filing-obligation summary need.
pub mod state_codes {
    /// The partner's whole distributive share, as the state K-1 restates it.
    pub const DISTRIBUTIVE_INCOME: &str = "distributive_income";
    /// What the state taxes a nonresident on: income allocated and apportioned
    /// to it.
    pub const SOURCE_INCOME: &str = "source_income";
    /// State additions to the partner's share (all of them).
    pub const ADDITIONS: &str = "additions";
    /// The part of [`ADDITIONS`] that is decoupling from federal depreciation —
    /// Maryland asks for it under its own code.
    pub const ADDITIONS_DECOUPLING: &str = "additions_decoupling";
    pub const SUBTRACTIONS: &str = "subtractions";
    pub const SUBTRACTIONS_DECOUPLING: &str = "subtractions_decoupling";
    /// Nonresident tax the partnership paid for the partner (Maryland Form 510).
    pub const NONRESIDENT_TAX_PAID: &str = "nonresident_tax_paid";
    /// Pass-through entity election tax paid on the partner's share (Maryland Form
    /// 511 and its equivalents). A credit, and an addback.
    pub const PTE_ELECTION_TAX: &str = "pte_election_tax";
    /// Income tax withheld for the partner (Virginia VK-1 line e).
    pub const WITHHOLDING: &str = "withholding";
    /// Virginia: income allocated to Virginia, apportionable income.
    pub const ALLOCATED: &str = "allocated";
    pub const APPORTIONABLE: &str = "apportionable";

    pub const ALL: &[&str] = &[
        DISTRIBUTIVE_INCOME,
        SOURCE_INCOME,
        ADDITIONS,
        ADDITIONS_DECOUPLING,
        SUBTRACTIONS,
        SUBTRACTIONS_DECOUPLING,
        NONRESIDENT_TAX_PAID,
        PTE_ELECTION_TAX,
        WITHHOLDING,
        ALLOCATED,
        APPORTIONABLE,
    ];

    /// What a code means, for a review screen.
    pub fn label(code: &str) -> &'static str {
        match code {
            DISTRIBUTIVE_INCOME => "Distributive share of income",
            SOURCE_INCOME => "Income sourced to the state",
            ADDITIONS => "State additions",
            ADDITIONS_DECOUPLING => "of which decoupling",
            SUBTRACTIONS => "State subtractions",
            SUBTRACTIONS_DECOUPLING => "of which decoupling",
            NONRESIDENT_TAX_PAID => "Nonresident tax paid for you",
            PTE_ELECTION_TAX => "Pass-through entity election tax",
            WITHHOLDING => "Tax withheld for you",
            ALLOCATED => "Income allocated to the state",
            APPORTIONABLE => "Apportionable income",
            _ => "Other",
        }
    }
}

use state_codes as sc;

/// The states this recognises a K-1 for, and how.
struct StateForm {
    code: &'static str,
    form: &'static str,
    /// Every phrase must appear on the page.
    markers: &'static [&'static str],
}

const STATE_FORMS: &[StateForm] = &[
    StateForm {
        code: "AZ",
        form: "Arizona Form 165 Schedule K-1(NR)",
        markers: &["Arizona", "Schedule K-1(NR)"],
    },
    StateForm {
        code: "CA",
        form: "California Schedule K-1 (568)",
        markers: &["CALIFORNIA SCHEDULE", "K-1 (568)"],
    },
    StateForm {
        code: "CA",
        form: "California Schedule K-1 (565)",
        markers: &["CALIFORNIA SCHEDULE", "K-1 (565)"],
    },
    StateForm {
        code: "GA",
        form: "Georgia K-1",
        markers: &["Georgia K-1"],
    },
    StateForm {
        code: "MD",
        form: "Maryland Schedule K-1 (510/511)",
        markers: &["MARYLAND", "SCHEDULE K-1", "(510/511)"],
    },
    StateForm {
        code: "NJ",
        form: "New Jersey Schedule NJK-1",
        markers: &["NJK-1", "New Jersey"],
    },
    StateForm {
        code: "NC",
        form: "North Carolina K-1 (D-403)",
        markers: &["NC K-1", "North Carolina"],
    },
    StateForm {
        code: "OR",
        form: "Oregon Schedule OR-K-1",
        markers: &["Schedule OR-K-1"],
    },
    StateForm {
        code: "PA",
        form: "Pennsylvania Schedule NRK-1",
        markers: &["Schedule NRK-1"],
    },
    StateForm {
        code: "SC",
        form: "South Carolina SC1065 K-1",
        markers: &["SOUTH CAROLINA", "K-1"],
    },
    StateForm {
        code: "VA",
        form: "Virginia Schedule VK-1",
        markers: &["Schedule VK-1", "Virginia"],
    },
];

/// The state K-1s a package carries, by page — what the attachment's type says
/// it contains.
///
/// A page that names no state follows the state K-1 before it: a state's K-1
/// often runs to several pages and only the first carries its heading.
/// Continuation stops at a federal page, a K-3, or another state's K-1.
pub fn detect_states(pages: &[PageText]) -> Vec<(String, &'static str, Vec<u32>)> {
    let mut out: Vec<(String, &'static str, Vec<u32>)> = Vec::new();
    let mut current: Option<usize> = None;
    for page in pages {
        if let Some(form) = STATE_FORMS
            .iter()
            .find(|f| f.markers.iter().all(|m| page.mentions(m)))
        {
            let at = match out.iter().position(|(code, _, _)| code == form.code) {
                Some(i) => {
                    out[i].2.push(page.page);
                    i
                }
                None => {
                    out.push((form.code.to_string(), form.form, vec![page.page]));
                    out.len() - 1
                }
            };
            current = Some(at);
        } else if federal_page(std::slice::from_ref(page)).is_some()
            || page.mentions("Schedule K-3")
        {
            current = None;
        } else if let Some(i) = current {
            out[i].2.push(page.page);
        }
    }
    out
}

/// The page carrying the federal Schedule K-1 (Form 1065), if any.
pub fn federal_page(pages: &[PageText]) -> Option<&PageText> {
    pages.iter().find(|p| {
        p.mentions("Schedule K-1")
            && p.mentions("(Form 1065)")
            && p.mentions("Part III")
            && p.mentions("Ordinary business income")
    })
}

/// Read a K-1 package.
pub fn extract(pages: &[PageText]) -> K1Extraction {
    let mut out = K1Extraction::default();
    match federal_page(pages) {
        Some(page) => {
            out.federal_page = Some(page.page);
            read_federal(page, &mut out);
        }
        None => out.warnings.push(
            "No federal Schedule K-1 (Form 1065) page was recognised, so no federal box was \
             read."
                .to_string(),
        ),
    }
    if out.federal_page.is_some() {
        match pages.iter().find(|p| p.mentions("QBI Pass-through Entity Reporting")) {
            Some(page) => read_qbi_statement(page, &mut out),
            None if out.federal.contains_key("1") || out.federal.contains_key("2") => {
                out.warnings.push(
                    "No QBI statement (box 20, code Z) was recognised; the QBI figures were \
                     not read."
                        .to_string(),
                )
            }
            None => {}
        }
    }
    for (code, form, state_pages) in detect_states(pages) {
        let first = pages
            .iter()
            .find(|p| p.page == state_pages[0])
            .expect("a detected page exists");
        let mut st = StateK1Extract {
            state: code.clone(),
            form: form.to_string(),
            pages: state_pages.clone(),
            ..Default::default()
        };
        match code.as_str() {
            "MD" => read_maryland(first, &mut st),
            "VA" => read_virginia(first, &mut st),
            "GA" => {
                read_labelled_total(first, &mut st, "Partner's Share of Georgia Source Income");
                if st.amount(sc::SOURCE_INCOME).is_none() {
                    read_labelled_total(first, &mut st, "Total Georgia Source Income");
                }
            }
            "NC" => read_labelled_total(first, &mut st, "Attributable to North Carolina"),
            "AZ" => read_column_row(first, &mut st, "Source Income", "Add lines 1, 2, and 3"),
            "NJ" => read_column_first(first, &mut st, "New Jersey Source"),
            "SC" => read_rightmost_column(first, &mut st),
            "CA" => read_column_sum(pages, &state_pages, &mut st, "source amounts"),
            "OR" => read_column_sum(pages, &state_pages, &mut st, "Oregon column"),
            "PA" => read_pa(first, &mut st),
            _ => {}
        }
        if st.amount(sc::SOURCE_INCOME).is_none() {
            st.warnings.push(format!(
                "The {} was recognised but its source income was not read. Enter it from page \
                 {}.",
                st.form, state_pages[0]
            ));
        }
        out.states.push(st);
    }
    out
}

// ---------------------------------------------------------------------------
// Amounts
// ---------------------------------------------------------------------------

/// A printed amount, in cents: `7,914.`, `-9,520.`, `(1,234)`, `34,810.00`,
/// `1,422 00` (Maryland prints the cents in their own box), `.00`.
pub fn parse_money(text: &str) -> Option<i64> {
    let t = text.trim().trim_end_matches('*').trim();
    if t.is_empty() {
        return None;
    }
    let (negative, body) = if let Some(inner) = t.strip_prefix('(').and_then(|s| s.strip_suffix(')'))
    {
        (true, inner.trim())
    } else if let Some(rest) = t.strip_prefix('-') {
        (true, rest.trim())
    } else {
        (false, t)
    };
    // "1422 00": dollars, a space, two digits of cents.
    let body = match body.rsplit_once(' ') {
        Some((d, c)) if c.len() == 2 && c.bytes().all(|b| b.is_ascii_digit()) => {
            format!("{d}.{c}")
        }
        _ => body.to_string(),
    };
    let body = body.replace(',', "");
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    if !body.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    let (whole, frac) = body.split_once('.').unwrap_or((&body, ""));
    if frac.contains('.') {
        return None;
    }
    let dollars: i64 = if whole.is_empty() { 0 } else { whole.parse().ok()? };
    // More than two decimal places is a ratio or a percentage, not money.
    let cents: i64 = match frac.len() {
        0 => 0,
        1 => frac.parse::<i64>().ok()? * 10,
        2 => frac.parse().ok()?,
        _ => return None,
    };
    let v = dollars * 100 + cents;
    Some(if negative { -v } else { v })
}

/// Whether a chunk looks like an amount rather than a label: digits with the
/// punctuation amounts carry, and at least one digit.
fn is_money(text: &str) -> bool {
    let t = text.trim().trim_end_matches('*');
    !t.is_empty()
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | '-' | '(' | ')' | ' '))
        && parse_money(t).is_some()
}

/// The amount a chunk carries: all of it, or its first word when a figure ran
/// into the text beside it (`-1,399 Line 21`).
fn chunk_money(c: &Chunk) -> Option<i64> {
    if is_money(&c.text) {
        return parse_money(&c.text);
    }
    c.text
        .split_whitespace()
        .next()
        .filter(|t| is_money(t))
        .and_then(parse_money)
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn starts_like(text: &str, prefix: &str) -> bool {
    squash(text).starts_with(&squash(prefix))
}

// ---------------------------------------------------------------------------
// Federal K-1
// ---------------------------------------------------------------------------

/// Part III's boxes, by the start of each label.
const FEDERAL_BOXES: &[(&str, &str)] = &[
    ("1", "Ordinary business income"),
    ("2", "Net rental real estate"),
    ("3", "Other net rental"),
    ("4a", "Guaranteed payments for services"),
    ("4b", "Guaranteed payments for capital"),
    ("4c", "Total guaranteed"),
    ("5", "Interest income"),
    ("6a", "Ordinary dividends"),
    ("6b", "Qualified dividends"),
    ("6c", "Dividend equivalents"),
    ("7", "Royalties"),
    ("8", "Net short-term"),
    ("9a", "Net long-term"),
    ("9b", "Collectibles"),
    ("9c", "Unrecaptured section 1250"),
    ("10", "Net section 1231"),
    ("11", "Other income"),
    ("12", "Section 179"),
    ("13", "Other deductions"),
    ("14", "Self-employment"),
    ("15", "Credits"),
    ("16", "Schedule K-3"),
    ("17", "Alternative minimum"),
    ("18", "Tax-exempt income"),
    ("19", "Distributions"),
    ("20", "Other information"),
    ("21", "Foreign taxes"),
    // Checkboxes, listed so their numbers are never read as figures.
    ("22", "More than one activity for at-risk"),
    ("23", "More than one activity for passive"),
];

/// A box's printed position.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    box_no: &'static str,
    x: f32,
    y: f32,
    right_column: bool,
}

fn federal_anchors(page: &PageText) -> Vec<Anchor> {
    let mut anchors = Vec::new();
    for line in &page.lines {
        for (i, c) in line.chunks.iter().enumerate() {
            let Some(next) = line.chunks.get(i + 1) else {
                continue;
            };
            if let Some((box_no, _)) = FEDERAL_BOXES
                .iter()
                .find(|(n, label)| c.text == *n && starts_like(&next.text, label))
            {
                if !anchors.iter().any(|a: &Anchor| a.box_no == *box_no) {
                    anchors.push(Anchor {
                        box_no,
                        x: c.x,
                        y: line.y,
                        right_column: false,
                    });
                }
            }
        }
    }
    // The right column is boxes 14 to 21. Its boundary is where its numbers sit.
    let boundary = anchors
        .iter()
        .filter(|a| a.box_no.parse::<u32>().map(|n| (14..=21).contains(&n)).unwrap_or(false))
        .map(|a| a.x)
        .fold(f32::INFINITY, f32::min);
    for a in &mut anchors {
        a.right_column = a.x >= boundary - 1.0;
    }
    anchors
}

fn read_federal(page: &PageText, out: &mut K1Extraction) {
    let text = page.layout();
    out.tax_year = find_year(&text);
    read_partnership(page, out);

    let anchors = federal_anchors(page);
    if anchors.len() < 10 {
        out.warnings.push(format!(
            "The federal K-1 on page {} did not lay out as expected ({} of its 21 boxes were \
             found), so its figures were not read.",
            page.page,
            anchors.len()
        ));
        return;
    }
    let left_x = anchors
        .iter()
        .filter(|a| !a.right_column)
        .map(|a| a.x)
        .fold(f32::INFINITY, f32::min);
    let right_x = anchors
        .iter()
        .filter(|a| a.right_column)
        .map(|a| a.x)
        .fold(f32::INFINITY, f32::min);

    let mut skipped: Vec<String> = Vec::new();
    for line in &page.lines {
        for (i, c) in line.chunks.iter().enumerate() {
            // Part III only: Parts I and II sit to the left of the box numbers.
            // A box code (`AC*`) can sit a little left of its column's numbers.
            if c.x < left_x - 10.0 {
                continue;
            }
            // A box's own number is not a figure.
            if anchors
                .iter()
                .any(|a| a.box_no == c.text.trim() && (a.x - c.x).abs() < 4.0)
            {
                continue;
            }
            let statement = c.text.trim() == "STMT";
            if !statement && !is_money(&c.text) {
                continue;
            }
            let right = c.x >= right_x - 12.0;
            // The box whose label is nearest above the figure, in its column.
            let Some(anchor) = anchors
                .iter()
                .filter(|a| a.right_column == right && a.y > line.y + 0.5)
                .min_by(|a, b| a.y.total_cmp(&b.y))
            else {
                continue;
            };
            let code = line.chunks[..i]
                .iter()
                .rev()
                .find(|k| {
                    let t = k.text.trim_end_matches('*').trim();
                    k.x >= (if right { right_x - 12.0 } else { left_x - 10.0 })
                        && !t.is_empty()
                        && t.len() <= 2
                        && t.chars().all(|ch| ch.is_ascii_uppercase())
                })
                .map(|k| k.text.trim_end_matches('*').trim().to_string());
            if statement {
                skipped.push(format!(
                    "box {}{} (\"see statement\")",
                    anchor.box_no,
                    code.as_deref().map(|c| format!(" code {c}")).unwrap_or_default()
                ));
                continue;
            }
            let cents = parse_money(&c.text).expect("is_money checked it");
            match catalogue_code(anchor.box_no, code.as_deref()) {
                Some(key) => *out.federal.entry(key).or_insert(0) += cents,
                None => skipped.push(format!(
                    "box {}{} {}",
                    anchor.box_no,
                    code.as_deref().map(|c| format!(" code {c}")).unwrap_or_default(),
                    crate::tax::lines::format_dollars(crate::tax::lines::cents_to_dollars(cents))
                )),
            }
        }
    }
    if !skipped.is_empty() {
        out.warnings.push(format!(
            "Read from the federal K-1 but not carried, because nothing on the 1040 computed \
             here takes them: {}. Check them against the K-1's statements.",
            skipped.join("; ")
        ));
    }
}

/// A Part III box and its letter code, as the statement catalogue names it.
/// `None` for a box or code nothing here reads.
fn catalogue_code(box_no: &str, code: Option<&str>) -> Option<String> {
    let c = code.unwrap_or("");
    let key = match (box_no, c) {
        ("16" | "22" | "23", _) => return None,
        ("13", "A" | "B" | "G") => "13_cash_contributions",
        ("13", "C" | "D" | "E" | "F") => "13_noncash_contributions",
        ("13", "H") => "13_investment_interest",
        ("13", "J") => "13_section_59e2",
        ("13", _) => "13_other",
        ("14", "A") => "14a",
        ("14", "B") => "14b",
        ("14", "C") => "14c",
        ("14", _) => return None,
        ("18", "A") => "18a",
        ("18", "B") => "18b",
        ("18", "C") => "18c",
        ("18", _) => return None,
        ("19", "A") => "19a",
        ("19", "B" | "C") => "19b",
        ("19", _) => return None,
        ("20", "A") => "20a",
        ("20", "B") => "20b",
        ("20", _) => return None,
        ("17", _) => "17",
        (n, _) => n,
    };
    Some(key.to_string())
}

fn find_year(text: &str) -> Option<i32> {
    let squashed = text.replace(char::is_whitespace, " ");
    for marker in ["For calendar year ", "(Form 1065) "] {
        let mut rest = squashed.as_str();
        while let Some(i) = rest.find(marker) {
            let after = &rest[i + marker.len()..];
            let digits: String = after.chars().take(4).collect();
            if digits.len() == 4 && digits.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(y) = digits.parse::<i32>() {
                    if (2000..2100).contains(&y) {
                        return Some(y);
                    }
                }
            }
            rest = after;
        }
    }
    None
}

/// Part I: item A (the EIN) and item B (the name), each printed on the line
/// below its label.
fn read_partnership(page: &PageText, out: &mut K1Extraction) {
    let below = |label: &str| -> Option<&Chunk> {
        let (li, label_chunk) = page.lines.iter().enumerate().find_map(|(li, l)| {
            l.chunks
                .iter()
                .find(|c| starts_like(&c.text, label))
                .map(|c| (li, c))
        })?;
        page.lines[li + 1..]
            .iter()
            .take(2)
            .flat_map(|l: &TextLine| l.chunks.iter())
            .find(|c| c.x < label_chunk.x + 120.0 && c.x >= label_chunk.x - 40.0)
    };
    if let Some(c) = below("Partnership's employer identification number") {
        let t = c.text.trim();
        if t.len() == 10 && t.as_bytes()[2] == b'-' {
            out.issuer_ein = Some(t.to_string());
        }
    }
    if let Some(c) = below("Partnership's name, address") {
        out.issuer = Some(c.text.trim().to_string());
    }
    if out.issuer.is_none() {
        out.warnings
            .push("The partnership's name (Part I, item B) was not read.".to_string());
    }
}

/// Statement A: the QBI items, summed across the columns the partnership
/// reports them in.
fn read_qbi_statement(page: &PageText, out: &mut K1Extraction) {
    let row = |label: &str| -> Option<i64> {
        page.lines.iter().find_map(|l| {
            let at = l.chunks.iter().position(|c| starts_like(&c.text, label))?;
            let sum: i64 = l.chunks[at + 1..]
                .iter()
                .filter(|c| is_money(&c.text))
                .filter_map(|c| parse_money(&c.text))
                .sum();
            Some(sum)
        })
    };
    let ordinary = row("Ordinary business income").unwrap_or(0);
    let rental = row("Rental income").unwrap_or(0);
    let royalty = row("Royalty income").unwrap_or(0);
    let other_income = row("Other income").unwrap_or(0);
    let s179 = row("Section 179 deduction").unwrap_or(0);
    let other_ded = row("Other deductions").unwrap_or(0);
    let s1231 = row("Section 1231 gain").unwrap_or(0);
    // A net §1231 gain is treated as a capital gain, which is not QBI; a §1231
    // loss is ordinary and is. Without the partner's other §1231 items that
    // netting cannot be done here, so a gain is left out and said.
    let qbi = ordinary + rental + royalty + other_income - s179 - other_ded + s1231.min(0);
    out.federal.insert("20z_qbi".to_string(), qbi);
    if s1231 > 0 {
        out.warnings.push(format!(
            "The QBI statement lists {} of §1231 gain. It is left out of qualified business \
             income here, because a net §1231 gain is taxed as a capital gain and capital \
             gains are not QBI.",
            crate::tax::lines::format_dollars(crate::tax::lines::cents_to_dollars(s1231))
        ));
    }
    if let Some(w2) = row("W-2 wages") {
        out.federal.insert("20z_w2_wages".to_string(), w2);
    }
    if let Some(ubia) = row("UBIA of qualified property") {
        out.federal.insert("20z_ubia".to_string(), ubia);
    }
}

// ---------------------------------------------------------------------------
// State K-1s
// ---------------------------------------------------------------------------

/// The amount printed to the right of `chunks[from..]`, if any.
fn first_money(chunks: &[Chunk]) -> Option<i64> {
    // A figure printed as "34,810" ".00" or "709." ".00" can be one chunk or two;
    // the first token carries the dollars, and a lone ".00" is a zero.
    let tokens: Vec<&str> = chunks.iter().flat_map(|c| c.text.split_whitespace()).collect();
    tokens
        .iter()
        .find(|t| **t != ".00" && is_money(t))
        .and_then(|t| parse_money(t))
        .or_else(|| tokens.iter().find(|t| **t == ".00").map(|_| 0))
}

/// Maryland Schedule K-1 (510/511): sections A to E, numbered lines, each with
/// its amount at the right margin.
fn read_maryland(page: &PageText, st: &mut StateK1Extract) {
    let mut section: Option<char> = None;
    let mut lines: BTreeMap<String, i64> = BTreeMap::new();
    for line in &page.lines {
        let first = line.chunks.first().map(|c| c.text.trim()).unwrap_or("");
        let mut chars = first.chars();
        if let (Some(letter), Some('.')) = (chars.next(), chars.next()) {
            if letter.is_ascii_uppercase() && first.len() > 3 {
                section = Some(letter);
                continue;
            }
        }
        let Some(letter) = section else { continue };
        // A line number "N." at the left margin, its amount as the last chunk.
        let Some(number) = first
            .strip_suffix('.')
            .filter(|n| n.len() == 1 && n.chars().all(|c| c.is_ascii_digit()))
        else {
            continue;
        };
        if let Some(cents) = line.chunks.iter().skip(1).rev().find_map(|c| {
            let t = c.text.trim();
            (t.ends_with(" 00") || t == "00").then(|| parse_money(t)).flatten()
        }) {
            lines.insert(format!("{letter}{number}"), cents);
        }
    }
    // The 2025 layout puts a two-line label's amount on its second line, which
    // carries no number; those lines (D2, D4, D5) are read where they land.
    for line in &page.lines {
        let label = line.layout();
        for (key, phrase) in [
            ("D2", "by this PTE (Form 511)"),
            ("D4", "by other PTEs for this entity"),
            ("D5", "of the credit total on Line 2 and 4"),
        ] {
            if squash(&label).contains(&squash(phrase)) {
                if let Some(cents) = line.chunks.iter().rev().find_map(|c| {
                    let t = c.text.trim();
                    (t.ends_with(" 00") || t == "00").then(|| parse_money(t)).flatten()
                }) {
                    lines.insert(key.to_string(), cents);
                }
            }
        }
    }
    let get = |k: &str| lines.get(k).copied();
    let sum = |ks: &[&str]| ks.iter().filter_map(|k| get(k)).sum::<i64>();
    if let Some(v) = get("A1") {
        st.amounts.insert(sc::DISTRIBUTIVE_INCOME.into(), v);
    }
    if let Some(v) = get("A2") {
        st.amounts.insert(sc::SOURCE_INCOME.into(), v);
    }
    if ["B1", "B2", "B3", "B4", "B5"].iter().any(|k| get(k).is_some()) {
        st.amounts
            .insert(sc::ADDITIONS.into(), sum(&["B1", "B2", "B3", "B4", "B5"]));
        st.amounts
            .insert(sc::ADDITIONS_DECOUPLING.into(), sum(&["B3", "B4"]));
    }
    if ["C1", "C2", "C3", "C4", "C5"].iter().any(|k| get(k).is_some()) {
        st.amounts
            .insert(sc::SUBTRACTIONS.into(), sum(&["C1", "C2", "C3", "C4", "C5"]));
        st.amounts
            .insert(sc::SUBTRACTIONS_DECOUPLING.into(), sum(&["C3", "C4"]));
    }
    if let Some(v) = get("D1") {
        st.amounts.insert(sc::NONRESIDENT_TAX_PAID.into(), v);
    }
    let pte = sum(&["D2", "D4"]);
    if get("D2").is_some() || get("D4").is_some() {
        st.amounts.insert(sc::PTE_ELECTION_TAX.into(), pte);
    }
    for (key, what) in [("A2", "income allocable to Maryland (A.2)"), ("D1", "nonresident tax paid (D.1)")] {
        if get(key).is_none() {
            st.warnings.push(format!("Maryland K-1: {what} was not read."));
        }
    }
}

/// Virginia Schedule VK-1: lettered owner lines, then numbered lines 1 to 18.
fn read_virginia(page: &PageText, st: &mut StateK1Extract) {
    let row = |prefix: &str| -> Option<(i64, String)> {
        page.lines.iter().find_map(|l| {
            let at = l.chunks.iter().position(|c| starts_like(&c.text, prefix))?;
            // The line's own number repeats at the right ("1."), then the amount.
            let rest = &l.chunks[at + 1..];
            let amount_from = rest
                .iter()
                .position(|c| {
                    let t = c.text.trim();
                    t.ends_with('.') && t.len() <= 4 && t[..t.len() - 1].chars().all(|ch| ch.is_ascii_alphanumeric())
                })
                .map(|i| i + 1)
                .unwrap_or(0);
            let tail = &rest[amount_from..];
            let joined: String = tail.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join(" ");
            first_money(tail).map(|v| (v, joined))
        })
    };
    let mut put = |code: &str, prefix: &str| -> Option<i64> {
        let v = row(prefix).map(|(v, _)| v);
        if let Some(v) = v {
            st.amounts.insert(code.to_string(), v);
        }
        v
    };
    put(sc::WITHHOLDING, "e. Amount withheld by PTE");
    put(sc::DISTRIBUTIVE_INCOME, "1. Total taxable income");
    let allocated = put(sc::ALLOCATED, "4. Income allocated to Virginia");
    let apportionable = put(sc::APPORTIONABLE, "6. Apportionable income");
    put(sc::ADDITIONS, "13. Total Additions");
    put(sc::SUBTRACTIONS, "18. Total Subtractions");
    // Line 7: "100.000000" "%".
    let pct = page.lines.iter().find_map(|l| {
        let at = l
            .chunks
            .iter()
            .position(|c| starts_like(&c.text, "7. Virginia apportionment percentage"))?;
        l.chunks[at + 1..].iter().find_map(|c| {
            let t = c.text.trim();
            (t.contains('.') && t.len() > 3)
                .then(|| t.parse::<f64>().ok())
                .flatten()
        })
    });
    if let Some(p) = pct {
        st.apportionment_ppm = Some((p * 10_000.0).round() as i64);
    }
    match (allocated, apportionable, st.apportionment_ppm) {
        (Some(a), Some(b), Some(ppm)) => {
            let source = a + (b as i128 * ppm as i128 / 1_000_000) as i64;
            st.amounts.insert(sc::SOURCE_INCOME.into(), source);
        }
        _ => st.warnings.push(
            "Virginia VK-1: lines 4, 6 and 7 are needed to work out Virginia source income and \
             not all of them were read."
                .to_string(),
        ),
    }
    if st.amount(sc::WITHHOLDING).is_none() {
        st.warnings
            .push("Virginia VK-1: the amount withheld (line e) was not read.".to_string());
    }
}

/// A total printed beside or just below its label.
fn read_labelled_total(page: &PageText, st: &mut StateK1Extract, label: &str) {
    for (li, line) in page.lines.iter().enumerate() {
        let Some(at) = line.chunks.iter().position(|c| squash(&c.text).contains(&squash(label)))
        else {
            continue;
        };
        let label_x = line.chunks[at].x;
        let same = line.chunks[at + 1..]
            .iter()
            .find(|c| is_money(&c.text))
            .and_then(|c| parse_money(&c.text));
        // Beside the label, else on the next line down, else the one above.
        let near = |l: &TextLine| {
            l.chunks
                .iter()
                .filter(|c| c.x > label_x + 100.0 && is_money(&c.text))
                .find_map(|c| parse_money(&c.text))
        };
        let found = same
            .or_else(|| page.lines.get(li + 1).and_then(near))
            .or_else(|| li.checked_sub(1).and_then(|i| page.lines.get(i)).and_then(near));
        if let Some(v) = found {
            st.amounts.insert(sc::SOURCE_INCOME.into(), v);
            return;
        }
    }
}

/// The column headed by a chunk containing `header`: its x range.
/// Arizona: the "source income" column, on the total line. The column is the
/// heading nearest above that line — the phrase also appears in the form's prose.
fn read_column_row(page: &PageText, st: &mut StateK1Extract, header: &str, row: &str) {
    let Some(line) = page
        .lines
        .iter()
        .find(|l| l.chunks.iter().any(|c| squash(&c.text).contains(&squash(row))))
    else {
        return;
    };
    let Some(head) = page
        .chunks()
        .filter(|c| c.y > line.y && squash(&c.text).contains(&squash(header)))
        .min_by(|a, b| a.y.total_cmp(&b.y))
    else {
        return;
    };
    let (lo, hi) = (head.x - 25.0, head.x_end.max(head.x + 40.0) + 60.0);
    let v = line
        .chunks
        .iter()
        .filter(|c| c.x >= lo && c.x <= hi)
        .filter_map(chunk_money)
        .next_back();
    st.amounts.insert(sc::SOURCE_INCOME.into(), v.unwrap_or(0));
}

/// New Jersey: the first amount in the "New Jersey Source Amounts" column.
fn read_column_first(page: &PageText, st: &mut StateK1Extract, header: &str) {
    let Some(head) = page.chunks().find(|c| squash(&c.text).contains(&squash(header))) else {
        return;
    };
    let (hx, hy) = (head.x, head.y);
    if let Some(v) = page
        .chunks()
        .filter(|c| c.y < hy && (c.x - hx).abs() < 140.0 && c.x > hx - 40.0 && is_money(&c.text))
        .max_by(|a, b| a.y.total_cmp(&b.y))
        .and_then(|c| parse_money(&c.text))
    {
        st.amounts.insert(sc::SOURCE_INCOME.into(), v);
    }
}

/// South Carolina: income lines allocated or apportioned to the state, in the
/// rightmost column.
fn read_rightmost_column(page: &PageText, st: &mut StateK1Extract) {
    let amounts: Vec<&Chunk> = page.chunks().filter(|c| is_money(&c.text)).collect();
    let Some(right) = amounts.iter().map(|c| c.x).fold(None, |m: Option<f32>, x| {
        Some(m.map_or(x, |m| m.max(x)))
    }) else {
        return;
    };
    // Income lines only: those whose row starts with a line number from 1 to 11.
    let mut total = 0;
    let mut any = false;
    for line in &page.lines {
        let first = line.chunks.first().map(|c| c.text.trim()).unwrap_or("");
        let number: Option<u32> = first.split_whitespace().next().and_then(|n| n.parse().ok());
        if !matches!(number, Some(1..=11)) {
            continue;
        }
        for c in &line.chunks {
            if c.x >= right - 40.0 && is_money(&c.text) {
                total += parse_money(&c.text).unwrap_or(0);
                any = true;
            }
        }
    }
    if any {
        st.amounts.insert(sc::SOURCE_INCOME.into(), total);
    }
}

/// California and Oregon: the state-source column, summed over the K-1's income
/// lines (1 to 11) less its deductions (12 and 13). An empty column is a K-1
/// reporting nothing sourced to the state.
fn read_column_sum(pages: &[PageText], state_pages: &[u32], st: &mut StateK1Extract, header: &str) {
    let mut total = 0;
    let mut found_header = false;
    for page in pages.iter().filter(|p| state_pages.contains(&p.page)) {
        let Some(head) = page.chunks().find(|c| squash(&c.text).contains(&squash(header))) else {
            continue;
        };
        found_header = true;
        let (lo, hi, head_y) = (head.x - 25.0, head.x_end.max(head.x + 40.0) + 40.0, head.y);
        for line in page.lines.iter().filter(|l| l.y < head_y) {
            let Some(number) = line_number(line) else {
                continue;
            };
            let sign = match number {
                1..=11 => 1,
                12 | 13 => -1,
                _ => continue,
            };
            for c in &line.chunks {
                if c.x >= lo && c.x <= hi && is_money(&c.text) {
                    total += sign * parse_money(&c.text).unwrap_or(0);
                }
            }
        }
    }
    if found_header {
        st.amounts.insert(sc::SOURCE_INCOME.into(), total);
    }
}

/// The line number a row of a state form starts with — `1`, `4 a`, `10 a` —
/// looking at the first two chunks, since a number and its label are sometimes
/// one run and sometimes two.
fn line_number(line: &TextLine) -> Option<u32> {
    line.chunks.iter().take(2).find_map(|c| {
        let digits: String = c.text.trim().chars().take_while(|ch| ch.is_ascii_digit()).collect();
        (!digits.is_empty() && digits.len() <= 2).then(|| digits.parse().ok()).flatten()
    })
}

/// Pennsylvania NRK-1: lines 1 to 5 are its classes of income.
fn read_pa(page: &PageText, st: &mut StateK1Extract) {
    let mut total = 0;
    let mut read = 0;
    for n in 1..=5u32 {
        let label = n.to_string();
        // The NRK-1 prints each line's number again inside the amount box, then
        // the amount — so the last money-like chunk on the line is the amount.
        if let Some(line) = page.lines.iter().find(|l| {
            l.chunks.first().map(|c| c.text.trim() == label).unwrap_or(false)
        }) {
            if let Some(v) = line
                .chunks
                .iter()
                .skip(1)
                .filter(|c| is_money(&c.text))
                .next_back()
                .and_then(|c| parse_money(&c.text))
            {
                if line.chunks.len() > 2 {
                    total += v;
                    read += 1;
                }
            }
        }
    }
    if read > 0 {
        st.amounts.insert(sc::SOURCE_INCOME.into(), total);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A made-up package laid out the way the forms are: a federal K-1, a QBI
    /// statement, and Maryland's and Virginia's K-1s. Nothing in it belongs to
    /// anybody.
    pub(crate) fn sample_package() -> lopdf::Document {
        let t = |x: f32, y: f32, s: &str| (x, y, s.to_string());
        // Federal page: box numbers and labels, with figures just below.
        let mut federal = vec![
            t(36.0, 756.0, "Schedule K-1"),
            t(36.0, 746.0, "(Form 1065)"),
            t(316.0, 734.0, "Part III"),
            t(187.0, 726.0, "For calendar year 2025, or tax year"),
            t(61.0, 651.0, "Partnership's employer identification number"),
            t(38.0, 640.0, "00-0000000"),
            t(61.0, 627.0, "Partnership's name, address, city, state, and ZIP code"),
            t(38.0, 616.0, "EXAMPLE PROPERTY FUND LP"),
        ];
        let left: Vec<&(&str, &str)> = FEDERAL_BOXES
            .iter()
            .filter(|(n, _)| n.parse::<u32>().map(|n| !(14..=21).contains(&n)).unwrap_or(true))
            .collect();
        for (i, (n, label)) in left.iter().enumerate() {
            let y = 712.0 - i as f32 * 24.0;
            federal.push(t(315.0, y, n));
            federal.push(t(331.0, y, label));
        }
        for (i, (n, label)) in FEDERAL_BOXES
            .iter()
            .filter(|(n, _)| n.parse::<u32>().map(|n| (14..=21).contains(&n)).unwrap_or(false))
            .enumerate()
        {
            let y = 712.0 - i as f32 * 48.0;
            federal.push(t(446.0, y, n));
            federal.push(t(464.0, y, label));
        }
        let below = |box_no: &str| -> f32 {
            let i = left.iter().position(|(n, _)| *n == box_no).unwrap();
            712.0 - i as f32 * 24.0 - 12.0
        };
        federal.push(t(398.0, below("1"), "12,000."));
        federal.push(t(392.0, below("2"), "-3,000."));
        federal.push(t(416.0, below("5"), "50."));
        federal.push(t(392.0, below("10"), "20,000."));
        federal.push(t(306.0, below("13"), "AE*"));
        federal.push(t(410.0, below("13"), "100."));
        // Box 19 is the sixth right-column box: 712 - 5 × 48.
        federal.push(t(440.0, 712.0 - 5.0 * 48.0 - 14.0, "A"));
        federal.push(t(538.0, 712.0 - 5.0 * 48.0 - 14.0, "4,000."));

        let qbi = vec![
            t(22.0, 575.0, "Statement A - QBI Pass-through Entity Reporting"),
            t(90.0, 388.0, "Ordinary business income (loss)"),
            t(437.0, 388.0, "12,000."),
            t(90.0, 364.0, "Rental income (loss)"),
            t(360.0, 364.0, "-3,000."),
            t(90.0, 244.0, "Other deductions"),
            t(372.0, 244.0, "100."),
            t(25.0, 207.0, "UBIA of qualified property"),
            t(360.0, 207.0, "50,000."),
        ];
        let maryland = vec![
            t(75.0, 750.0, "MARYLAND"),
            t(69.0, 740.0, "SCHEDULE K-1"),
            t(67.0, 724.0, "(510/511)"),
            t(35.0, 469.0, "A. Member's Income"),
            t(47.0, 458.0, "1."),
            t(60.0, 458.0, "Distributive or pro rata share of income from federal Schedule K-1"),
            t(525.0, 458.0, "29000 00"),
            t(47.0, 446.0, "2."),
            t(60.0, 446.0, "Distributive or pro rata share allocable to Maryland"),
            t(525.0, 446.0, "10000 00"),
            t(35.0, 362.0, "C. Subtractions"),
            t(47.0, 314.0, "4."),
            t(60.0, 314.0, "Net decoupling modification from another PTE"),
            t(538.0, 314.0, "200 00"),
            t(35.0, 290.0, "D. Nonresident/Resident Tax - Enter the member's share"),
            t(47.0, 278.0, "1."),
            t(60.0, 278.0, "Nonresident tax paid on member's behalf by this PTE (Form 510)"),
            t(531.0, 278.0, "900 00"),
        ];
        let virginia = vec![
            t(36.0, 733.0, "2025 Form 502"),
            t(163.0, 733.0, "Virginia Pass-Through Entity"),
            t(36.0, 719.0, "Schedule VK-1"),
            t(45.0, 483.0, "e. Amount withheld by PTE for the owner"),
            t(461.0, 483.0, "e."),
            t(527.0, 483.0, "400."),
            t(547.0, 483.0, ".00"),
            t(44.0, 447.0, "1. Total taxable income amounts"),
            t(459.0, 447.0, "1."),
            t(515.0, 447.0, "29,000"),
            t(547.0, 447.0, ".00"),
            t(44.0, 398.0, "4. Income allocated to Virginia"),
            t(459.0, 398.0, "4."),
            t(547.0, 398.0, ".00"),
            t(44.0, 375.0, "6. Apportionable income"),
            t(459.0, 375.0, "6."),
            t(515.0, 375.0, "8,000"),
            t(547.0, 375.0, ".00"),
            t(44.0, 363.0, "7. Virginia apportionment percentage"),
            t(459.0, 363.0, "7."),
            t(499.0, 363.0, "50.000000"),
            t(552.0, 363.0, "%"),
            t(40.0, 135.0, "18. Total Subtractions."),
            t(454.0, 135.0, "18."),
            t(527.0, 135.0, "100"),
            t(547.0, 135.0, ".00"),
        ];
        crate::documents::pdf_text::fixture_pdf(&[federal, qbi, maryland, virginia], 5.0)
    }

    /// The whole package read back: federal boxes by catalogue code, the QBI
    /// statement, and each state K-1's figures.
    #[test]
    fn a_k1_package_reads_back_box_by_box_and_state_by_state() {
        let pages = crate::documents::pdf_text::pages(&sample_package());
        let x = extract(&pages);
        assert_eq!(x.tax_year, Some(2025));
        assert_eq!(x.issuer.as_deref(), Some("EXAMPLE PROPERTY FUND LP"));
        assert_eq!(x.issuer_ein.as_deref(), Some("00-0000000"));
        let f = |k: &str| x.federal.get(k).copied();
        assert_eq!(f("1"), Some(1_200_000), "{:?}", x.federal);
        assert_eq!(f("2"), Some(-300_000));
        assert_eq!(f("5"), Some(5_000));
        assert_eq!(f("10"), Some(2_000_000));
        assert_eq!(f("13_other"), Some(10_000));
        assert_eq!(f("19a"), Some(400_000));
        assert_eq!(f("20z_qbi"), Some(1_200_000 - 300_000 - 10_000));
        assert_eq!(f("20z_ubia"), Some(5_000_000));

        let md = x.states.iter().find(|s| s.state == "MD").expect("Maryland");
        assert_eq!(md.amount(sc::SOURCE_INCOME), Some(1_000_000));
        assert_eq!(md.amount(sc::SUBTRACTIONS_DECOUPLING), Some(20_000));
        assert_eq!(md.amount(sc::NONRESIDENT_TAX_PAID), Some(90_000));

        let va = x.states.iter().find(|s| s.state == "VA").expect("Virginia");
        assert_eq!(va.amount(sc::WITHHOLDING), Some(40_000));
        assert_eq!(va.apportionment_ppm, Some(500_000));
        // Nothing allocated, half of 8,000 apportioned.
        assert_eq!(va.amount(sc::SOURCE_INCOME), Some(400_000));
        assert_eq!(va.amount(sc::SUBTRACTIONS), Some(10_000));
    }

    /// A page set that is not a K-1 says so instead of producing figures.
    #[test]
    fn a_document_that_is_not_a_k1_yields_nothing_and_says_so() {
        let doc = crate::documents::pdf_text::fixture_pdf(
            &[vec![(50.0, 700.0, "Monthly statement".to_string())]],
            9.0,
        );
        let x = extract(&crate::documents::pdf_text::pages(&doc));
        assert!(x.federal.is_empty());
        assert!(x.states.is_empty());
        assert!(x.warnings.iter().any(|w| w.contains("No federal Schedule K-1")));
    }

    #[test]
    fn amounts_in_every_way_forms_print_them() {
        assert_eq!(parse_money("7,914."), Some(791_400));
        assert_eq!(parse_money("-9,520."), Some(-952_000));
        assert_eq!(parse_money("(1,234)"), Some(-123_400));
        assert_eq!(parse_money("34,810.00"), Some(3_481_000));
        assert_eq!(parse_money("1422 00"), Some(142_200));
        assert_eq!(parse_money(".00"), Some(0));
        assert_eq!(parse_money("-363.00"), Some(-36_300));
        assert_eq!(parse_money("559.*"), Some(55_900));
        assert_eq!(parse_money("STMT"), None);
        assert_eq!(parse_money("4a"), None);
        assert_eq!(parse_money("1.2.3"), None);
    }

    #[test]
    fn the_year_comes_from_the_forms_own_heading() {
        assert_eq!(find_year("For calendar year 2025, or tax year"), Some(2025));
        assert_eq!(find_year("Schedule K-1 (Form 1065) 2024 Created"), Some(2024));
        assert_eq!(find_year("nothing here"), None);
    }

    #[test]
    fn letter_codes_reach_the_catalogue_boxes_they_mean() {
        assert_eq!(catalogue_code("1", None).as_deref(), Some("1"));
        assert_eq!(catalogue_code("13", Some("AC")).as_deref(), Some("13_other"));
        assert_eq!(catalogue_code("13", Some("H")).as_deref(), Some("13_investment_interest"));
        assert_eq!(catalogue_code("19", Some("A")).as_deref(), Some("19a"));
        assert_eq!(catalogue_code("20", Some("A")).as_deref(), Some("20a"));
        assert_eq!(catalogue_code("20", Some("N")), None);
        assert_eq!(catalogue_code("16", None), None);
    }
}
