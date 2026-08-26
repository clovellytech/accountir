//! The "attach statement" pages.
//!
//! Several lines on the return are a single box with a note beside it telling
//! you to attach a statement saying what is in it. Line 21, "Other deductions",
//! is the one that matters most in practice: a chart of accounts has a dozen
//! expenses — advertising, insurance, professional fees, software, bank charges
//! — that all land there, and the return shows one number for the lot. Without
//! the statement the figure is unsupported, which is a correspondence letter
//! rather than a rejection, but a letter about the largest deduction on the page.
//!
//! # Why the statement is generated rather than left to the preparer
//!
//! The books already know exactly what went into the line — that is what the
//! mapping *is*. Asking somebody to retype it into a word processor invites the
//! one error the statement exists to rule out: a list that does not add up to
//! the box it supports. Here the list and the box are computed from the same
//! sum, so they agree by construction, and the total is printed on the statement
//! for a reader to check against the form.
//!
//! # Why it is drawn rather than filled
//!
//! Everything else in this module tree fills an existing IRS form and is careful
//! never to draw on one, because a drawn figure cannot be corrected. A statement
//! has no IRS form — it is a plain schedule the filer composes — so there is
//! nothing to preserve the fillability of. It is generated as text on a blank
//! page, appended after the return.

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Document, Object, Stream};

use super::acroform::FormError;
use super::lines::{cents_to_dollars, format_dollars, LineDetail, TaxLineDef};

/// US Letter, in PDF points.
const PAGE_W: f32 = 612.0;
const PAGE_H: f32 = 792.0;
const MARGIN: f32 = 54.0;

const TITLE_SIZE: f32 = 13.0;
const BODY_SIZE: f32 = 10.0;
const LINE_H: f32 = 14.0;

/// Rows per page, leaving room for the heading block and the total.
const ROWS_PER_PAGE: usize = 42;

/// What one statement is about.
pub struct StatementRequest<'a> {
    pub legal_name: &'a str,
    pub ein: &'a str,
    pub year: i32,
    pub line: &'static TaxLineDef,
    pub rows: &'a [LineDetail],
}

/// Build the statement pages for one line as their own document.
///
/// Returns `None` when the line has nothing on it — a statement supporting an
/// empty box is noise, and attaching one invites the reader to look for a figure
/// that is not there.
pub fn build(req: &StatementRequest) -> Result<Option<Document>, FormError> {
    if req.rows.is_empty() {
        return Ok(None);
    }

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();

    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        // The one font every PDF reader has without embedding. A statement is
        // plain text and has no reason to carry a font programme.
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let bold_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica-Bold",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id, "F2" => bold_id },
    });

    let chunks: Vec<&[LineDetail]> = req.rows.chunks(ROWS_PER_PAGE).collect();
    let page_count = chunks.len();
    let total_cents: i64 = req.rows.iter().map(|r| r.cents).sum();

    let mut page_ids: Vec<Object> = Vec::with_capacity(page_count);
    for (i, chunk) in chunks.iter().enumerate() {
        let last = i + 1 == page_count;
        let ops = page_ops(req, chunk, i + 1, page_count, last.then_some(total_cents));
        let content_id = doc.add_object(Stream::new(
            dictionary! {},
            Content { operations: ops }.encode()?,
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "MediaBox" => vec![0.into(), 0.into(), PAGE_W.into(), PAGE_H.into()],
            "Resources" => resources_id,
        });
        page_ids.push(page_id.into());
    }

    let count = page_ids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids,
            "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    Ok(Some(doc))
}

fn page_ops(
    req: &StatementRequest,
    rows: &[LineDetail],
    page_no: usize,
    page_count: usize,
    total: Option<i64>,
) -> Vec<Operation> {
    let mut ops = Vec::new();
    let mut y = PAGE_H - MARGIN;

    let heading = format!(
        "Form 1065 ({}) — {} statement",
        req.year,
        match req.line.schedule {
            super::lines::Schedule::Page1 => format!("Page 1, line {}", req.line.number),
            super::lines::Schedule::K => format!("Schedule K, line {}", req.line.number),
            super::lines::Schedule::L => format!("Schedule L, line {}", req.line.number),
        }
    );
    text(&mut ops, "F2", TITLE_SIZE, MARGIN, y, &heading);
    y -= LINE_H * 1.4;
    text(&mut ops, "F2", BODY_SIZE, MARGIN, y, req.line.label);
    y -= LINE_H;

    // Who this belongs to, on every page. A statement that comes adrift from its
    // return has to be able to name the return it supports.
    text(
        &mut ops,
        "F1",
        BODY_SIZE,
        MARGIN,
        y,
        &format!("{}  ·  EIN {}", req.legal_name, req.ein),
    );
    y -= LINE_H * 1.6;

    text(&mut ops, "F2", BODY_SIZE, MARGIN, y, "Account");
    right(&mut ops, "F2", BODY_SIZE, PAGE_W - MARGIN, y, "Amount");
    y -= 4.0;
    rule(&mut ops, MARGIN, y, PAGE_W - MARGIN);
    y -= LINE_H;

    for r in rows {
        let label = if r.account_number.is_empty() {
            r.account_name.clone()
        } else {
            format!("{}  {}", r.account_number, r.account_name)
        };
        text(&mut ops, "F1", BODY_SIZE, MARGIN, y, &truncate(&label, 62));
        right(
            &mut ops,
            "F1",
            BODY_SIZE,
            PAGE_W - MARGIN,
            y,
            &format_dollars(cents_to_dollars(r.cents)),
        );
        y -= LINE_H;
    }

    if let Some(total_cents) = total {
        y -= 4.0;
        rule(&mut ops, MARGIN, y, PAGE_W - MARGIN);
        y -= LINE_H;
        text(
            &mut ops,
            "F2",
            BODY_SIZE,
            MARGIN,
            y,
            &format!("Total — line {}", req.line.number),
        );
        right(
            &mut ops,
            "F2",
            BODY_SIZE,
            PAGE_W - MARGIN,
            y,
            // Rounded once, from the summed cents — the same order the form's own
            // figure is computed in, so the statement and the box agree exactly.
            // Rounding each row and adding those would differ by a dollar or two
            // and make the statement contradict the return it supports.
            &format_dollars(cents_to_dollars(total_cents)),
        );
    }

    if page_count > 1 {
        right(
            &mut ops,
            "F1",
            8.0,
            PAGE_W - MARGIN,
            MARGIN * 0.6,
            &format!("Page {page_no} of {page_count}"),
        );
    }

    ops
}

fn text(ops: &mut Vec<Operation>, font: &str, size: f32, x: f32, y: f32, s: &str) {
    ops.push(Operation::new("BT", vec![]));
    ops.push(Operation::new("Tf", vec![font.into(), size.into()]));
    ops.push(Operation::new("Td", vec![x.into(), y.into()]));
    ops.push(Operation::new("Tj", vec![Object::string_literal(encode(s))]));
    ops.push(Operation::new("ET", vec![]));
}

/// Right-aligned text, positioned from an estimate of its width.
///
/// Helvetica's real widths are per-glyph and live in the font's metrics, which
/// are not embedded here. For a column of digits an average works: every figure
/// on the page is a number, digits are uniform width in this face, and being a
/// point or two out on a right margin is invisible.
fn right(ops: &mut Vec<Operation>, font: &str, size: f32, right_x: f32, y: f32, s: &str) {
    let width = s.chars().count() as f32 * size * 0.55;
    text(ops, font, size, right_x - width, y, s);
}

fn rule(ops: &mut Vec<Operation>, x0: f32, y: f32, x1: f32) {
    ops.push(Operation::new("q", vec![]));
    ops.push(Operation::new("w", vec![0.5.into()]));
    ops.push(Operation::new("m", vec![x0.into(), y.into()]));
    ops.push(Operation::new("l", vec![x1.into(), y.into()]));
    ops.push(Operation::new("S", vec![]));
    ops.push(Operation::new("Q", vec![]));
}

/// Encode text as WinAnsi **bytes**, which is what the font declares.
///
/// # Why this returns bytes and not a `String`
///
/// A Rust `String` is UTF-8, and a PDF string literal is a byte sequence read
/// through whatever encoding the font declares — `WinAnsiEncoding` here. Handing
/// back a `String` means every character above U+007F is emitted as its *two or
/// three* UTF-8 bytes and then read as that many separate WinAnsi characters. A
/// middle dot comes out as `Â·`, an é as `Ã©`. One byte per character is the
/// whole fix, and it cannot be expressed in a type that guarantees UTF-8.
///
/// # Why the punctuation is mapped rather than replaced
///
/// WinAnsi has an em dash, curly quotes, a bullet and an ellipsis — they just do
/// not sit at their Unicode code points, but in the 0x80–0x9F range that Latin-1
/// leaves as control characters. Replacing them with `?` threw away characters
/// the font could draw perfectly well, which is why "Page 1, line 21 statement"
/// arrived with a question mark where its dash should be.
///
/// Anything genuinely outside WinAnsi still becomes `?`. Account names come from
/// the books and can hold any script somebody typed; a `?` is visibly wrong,
/// where a byte the reader draws as some unrelated glyph is not.
fn encode(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| match c {
            // The 0x80-0x9F block, where WinAnsi and Latin-1 disagree.
            '\u{20AC}' => 0x80, // €
            '\u{201A}' => 0x82, // ‚
            '\u{0192}' => 0x83, // ƒ
            '\u{201E}' => 0x84, // „
            '\u{2026}' => 0x85, // …
            '\u{2020}' => 0x86, // †
            '\u{2021}' => 0x87, // ‡
            '\u{02C6}' => 0x88, // ˆ
            '\u{2030}' => 0x89, // ‰
            '\u{0160}' => 0x8A, // Š
            '\u{2039}' => 0x8B, // ‹
            '\u{0152}' => 0x8C, // Œ
            '\u{017D}' => 0x8E, // Ž
            '\u{2018}' => 0x91, // ‘
            '\u{2019}' => 0x92, // ’
            '\u{201C}' => 0x93, // “
            '\u{201D}' => 0x94, // ”
            '\u{2022}' => 0x95, // •
            '\u{2013}' => 0x96, // –
            '\u{2014}' => 0x97, // —
            '\u{02DC}' => 0x98, // ˜
            '\u{2122}' => 0x99, // ™
            '\u{0161}' => 0x9A, // š
            '\u{203A}' => 0x9B, // ›
            '\u{0153}' => 0x9C, // œ
            '\u{017E}' => 0x9E, // ž
            '\u{0178}' => 0x9F, // Ÿ
            // ASCII and the Latin-1 supplement, which WinAnsi carries unchanged.
            // The 0x80-0x9F gap is excluded: nothing legitimate lands there, and
            // a raw control byte is exactly what this function exists to avoid.
            c if (c as u32) <= 0x7E || (0xA0..=0xFF).contains(&(c as u32)) => c as u8,
            _ => b'?',
        })
        .collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::lines::line_def;

    fn detail(num: &str, name: &str, cents: i64) -> LineDetail {
        LineDetail {
            account_id: num.to_string(),
            account_number: num.to_string(),
            account_name: name.to_string(),
            cents,
        }
    }

    fn req<'a>(rows: &'a [LineDetail]) -> StatementRequest<'a> {
        StatementRequest {
            legal_name: "Acme Trading LLP",
            ein: "12-3456789",
            year: 2025,
            line: line_def("l21").unwrap(),
            rows,
        }
    }

    #[test]
    fn an_empty_line_gets_no_statement() {
        assert!(build(&req(&[])).unwrap().is_none());
    }

    #[test]
    fn a_statement_is_one_page_for_a_short_list() {
        let rows = vec![
            detail("6100", "Advertising", 120_00),
            detail("6200", "Insurance", 340_50),
        ];
        let doc = build(&req(&rows)).unwrap().unwrap();
        assert_eq!(doc.get_pages().len(), 1);
    }

    /// A long list has to keep going rather than silently stop at the bottom of
    /// the first page — a truncated statement supports the wrong total.
    #[test]
    fn a_long_list_runs_onto_further_pages() {
        let rows: Vec<LineDetail> = (0..100)
            .map(|i| detail(&format!("6{i:03}"), &format!("Expense {i}"), 100_00))
            .collect();
        let doc = build(&req(&rows)).unwrap().unwrap();
        assert!(doc.get_pages().len() >= 3, "got {}", doc.get_pages().len());
    }

    /// The whole point: the statement's total is the same figure the form's box
    /// carries. Rounded once from summed cents, not summed from rounded rows.
    #[test]
    fn the_total_is_rounded_the_same_way_the_form_box_is() {
        // Three rows that each round down, and whose cents carry.
        let rows = vec![
            detail("6100", "A", 33_33),
            detail("6200", "B", 33_33),
            detail("6300", "C", 33_34),
        ];
        let summed: i64 = rows.iter().map(|r| r.cents).sum();
        let statement_total = cents_to_dollars(summed);
        let row_by_row: i64 = rows.iter().map(|r| cents_to_dollars(r.cents)).sum();

        assert_eq!(statement_total, 100);
        assert_eq!(row_by_row, 99, "this is the mistake the module avoids");

        let doc = build(&req(&rows)).unwrap().unwrap();
        let text = page_text(&doc);
        assert!(text.contains("100"), "total missing from {text:?}");
    }

    #[test]
    fn the_statement_names_the_return_it_supports() {
        let rows = vec![detail("6100", "Advertising", 120_00)];
        let doc = build(&req(&rows)).unwrap().unwrap();
        let text = page_text(&doc);
        assert!(text.contains("Acme Trading LLP"), "{text:?}");
        assert!(text.contains("12-3456789"), "{text:?}");
        assert!(text.contains("2025"), "{text:?}");
        assert!(text.contains("line 21"), "{text:?}");
    }

    /// An account name with characters outside WinAnsi must not corrupt the
    /// stream — the books hold whatever somebody typed.
    #[test]
    fn an_unencodable_account_name_does_not_break_the_page() {
        let rows = vec![detail("6100", "Café — 日本 supplies", 100_00)];
        let doc = build(&req(&rows)).unwrap().unwrap();
        assert_eq!(doc.get_pages().len(), 1);
    }

    /// One byte per character, always.
    ///
    /// This is the bug that shipped: `encode` returned a `String`, so every
    /// character above U+007F went out as its UTF-8 bytes and came back as that
    /// many WinAnsi characters — a middle dot rendering as `Â·`. Asserting the
    /// byte length is the only way to catch it, because the source text looks
    /// perfectly correct either way.
    #[test]
    fn every_character_encodes_to_exactly_one_byte() {
        for s in ["plain ascii", "Café", "a · b", "an — dash", "…", "€5", "naïve", "Ÿ"] {
            assert_eq!(
                encode(s).len(),
                s.chars().count(),
                "{s:?} did not encode one byte per character"
            );
        }
    }

    /// The characters WinAnsi actually has must reach it, not the `?` fallback.
    /// The em dash in the heading and the middle dot in the address line are both
    /// on this list, and both came out wrong.
    #[test]
    fn punctuation_winansi_has_is_not_replaced_with_a_question_mark() {
        for (c, byte) in [
            ('\u{00B7}', 0xB7u8), // · middle dot
            ('\u{2014}', 0x97),   // — em dash
            ('\u{2013}', 0x96),   // – en dash
            ('\u{2026}', 0x85),   // … ellipsis
            ('\u{2019}', 0x92),   // ’ right single quote
            ('\u{201C}', 0x93),   // " left double quote
            ('\u{00E9}', 0xE9),   // é
        ] {
            assert_eq!(
                encode(&c.to_string()),
                vec![byte],
                "{c:?} should encode to {byte:#04x}"
            );
        }
    }

    /// Anything WinAnsi genuinely cannot draw still becomes a visible `?` rather
    /// than a byte the reader renders as some unrelated glyph.
    #[test]
    fn characters_outside_winansi_become_a_visible_question_mark() {
        assert_eq!(encode("日本"), b"??".to_vec());
        // The 0x80-0x9F gap holds control codes in Latin-1; nothing may land there
        // by falling through the range check.
        assert_eq!(encode("\u{0081}"), b"?".to_vec());
    }

    /// End to end: the strings this module composes itself have to survive it.
    #[test]
    fn the_heading_and_address_line_round_trip_intact() {
        let rows = vec![detail("6100", "Insurance", 120_00)];
        let doc = build(&req(&rows)).unwrap().unwrap();
        let text = page_text(&doc);
        assert!(!text.contains('\u{00C2}'), "stray Â in {text:?}");
        assert!(!text.contains('?'), "a character was dropped: {text:?}");
    }

    fn page_text(doc: &Document) -> String {
        let pages: Vec<u32> = doc.get_pages().keys().copied().collect();
        pages
            .iter()
            .filter_map(|p| doc.extract_text(&[*p]).ok())
            .collect::<Vec<_>>()
            .join(" ")
    }
}
