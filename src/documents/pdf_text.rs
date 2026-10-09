//! Text from a PDF, with where it sits on the page.
//!
//! # Why not `lopdf::Document::extract_text`
//!
//! It returns the text in content-stream order with the positions thrown away.
//! On a printed tax form that order is "every label on the page, then every
//! figure", so a K-1's page 1 comes out as `…7,914.-9,520.73.369.35,974.…` — the
//! figures are all there and nothing says which box any of them is in. What a
//! form's figures mean is *where* they are, so this keeps the position of every
//! run of text and lets a reader ask "what is printed just below the label
//! `Ordinary business income`?".
//!
//! # What it does and does not model
//!
//! It tracks the graphics state's transformation matrix (`cm`, `q`/`Q`), the text
//! matrix and line matrix (`BT`, `Tm`, `Td`, `TD`, `T*`, `TL`, `'`, `"`), and
//! follows form XObjects (`Do`). It does not lay glyphs out one by one: each
//! string operand is placed at the point where it starts, which is all a form
//! reader needs — a figure is one string, and its first character says where it
//! is. Rotated text is placed but not turned.

use std::collections::BTreeMap;

use lopdf::content::Content;
use lopdf::{Document, Encoding, Object, ObjectId};

/// One run of text, placed on the page in PDF points from the bottom-left corner.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub x: f32,
    /// Where the text ends, from the font's glyph widths where it has them.
    pub x_end: f32,
    pub y: f32,
    /// The font size the text was set in, after the matrices — a rough height.
    pub size: f32,
    pub text: String,
}

/// The chunks that share a baseline, left to right.
#[derive(Debug, Clone, PartialEq)]
pub struct TextLine {
    pub y: f32,
    pub chunks: Vec<Chunk>,
}

impl TextLine {
    /// The line as one string, with gaps kept roughly to scale — what
    /// `pdftotext -layout` prints. One column per [`COLUMN_POINTS`].
    pub fn layout(&self) -> String {
        let mut out = String::new();
        for c in &self.chunks {
            let col = (c.x / COLUMN_POINTS).max(0.0) as usize;
            let len = out.chars().count();
            if col > len {
                out.extend(std::iter::repeat_n(' ', col - len));
            } else if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push_str(&c.text);
        }
        out
    }
}

/// How many points one character column of [`TextLine::layout`] stands for.
pub const COLUMN_POINTS: f32 = 4.5;

/// One page's text, as lines from the top of the page down.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PageText {
    pub page: u32,
    pub lines: Vec<TextLine>,
}

impl PageText {
    /// Every chunk on the page, in reading order.
    pub fn chunks(&self) -> impl Iterator<Item = &Chunk> {
        self.lines.iter().flat_map(|l| l.chunks.iter())
    }

    /// The page as text, one line per baseline — for matching words, not positions.
    pub fn layout(&self) -> String {
        self.lines
            .iter()
            .map(TextLine::layout)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether `needle` appears on the page, ignoring case and spacing — a label
    /// split across two text runs still matches.
    pub fn mentions(&self, needle: &str) -> bool {
        let squash = |s: &str| {
            s.chars()
                .filter(|c| !c.is_whitespace())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        squash(&self.layout()).contains(&squash(needle))
    }
}

/// Every page's text, in page order. A page whose content cannot be read comes
/// back empty rather than failing the document: one broken page of thirty-eight
/// should not cost the other thirty-seven.
pub fn pages(doc: &Document) -> Vec<PageText> {
    doc.get_pages()
        .into_iter()
        .map(|(number, id)| PageText {
            page: number,
            lines: page_lines(doc, id).unwrap_or_default(),
        })
        .collect()
}

/// Two chunks closer together vertically than this share a line.
const SAME_LINE: f32 = 2.0;

/// How a font sets text: how its bytes decode, and how wide each glyph is.
struct FontInfo<'a> {
    encoding: Encoding<'a>,
    /// `/FirstChar` and `/Widths`, in thousandths of the font size. Empty for
    /// fonts that do not carry them (the standard 14, composite fonts), which
    /// fall back to [`DEFAULT_WIDTH`].
    first_char: i64,
    widths: Vec<f32>,
    /// Two bytes per glyph (`Identity-H` and the like).
    two_byte: bool,
}

/// Glyph width when a font does not say: half an em, roughly a digit.
const DEFAULT_WIDTH: f32 = 500.0;

impl FontInfo<'_> {
    fn advance(&self, bytes: &[u8]) -> f32 {
        if self.two_byte {
            return (bytes.len() / 2) as f32 * DEFAULT_WIDTH;
        }
        bytes
            .iter()
            .map(|b| {
                let i = *b as i64 - self.first_char;
                if i >= 0 {
                    self.widths.get(i as usize).copied().unwrap_or(DEFAULT_WIDTH)
                } else {
                    DEFAULT_WIDTH
                }
            })
            .sum()
    }
}

fn font_info<'a>(doc: &'a Document, font: &'a lopdf::Dictionary) -> Option<FontInfo<'a>> {
    let encoding = font.get_font_encoding(doc).ok()?;
    let first_char = font
        .get(b"FirstChar")
        .ok()
        .and_then(|o| o.as_i64().ok())
        .unwrap_or(0);
    let widths = match font.get(b"Widths").ok() {
        Some(Object::Array(a)) => a.iter().map(|o| number(o).unwrap_or(DEFAULT_WIDTH)).collect(),
        Some(Object::Reference(id)) => match doc.get_object(*id) {
            Ok(Object::Array(a)) => a.iter().map(|o| number(o).unwrap_or(DEFAULT_WIDTH)).collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let two_byte = font
        .get(b"Subtype")
        .and_then(|s| s.as_name())
        .map(|s| s == b"Type0")
        .unwrap_or(false);
    Some(FontInfo {
        encoding,
        first_char,
        widths,
        two_byte,
    })
}

fn page_lines(doc: &Document, page_id: ObjectId) -> Option<Vec<TextLine>> {
    let fonts = doc.get_page_fonts(page_id).ok()?;
    let encodings: BTreeMap<Vec<u8>, FontInfo> = fonts
        .into_iter()
        .filter_map(|(name, font)| font_info(doc, font).map(|f| (name, f)))
        .collect();
    let content = doc.get_page_content(page_id);
    let mut chunks = Vec::new();
    let resources = page_resources(doc, page_id);
    walk(
        doc,
        &content,
        IDENTITY,
        &[&encodings],
        resources.as_ref(),
        &mut chunks,
        0,
    );
    Some(into_lines(chunks))
}

/// The page's /Resources dictionary, following one indirect reference.
fn page_resources(doc: &Document, page_id: ObjectId) -> Option<lopdf::Dictionary> {
    let page = doc.get_dictionary(page_id).ok()?;
    match page.get(b"Resources").ok()? {
        Object::Reference(id) => doc.get_dictionary(*id).ok().cloned(),
        Object::Dictionary(d) => Some(d.clone()),
        _ => None,
    }
}

type Matrix = [f32; 6];
const IDENTITY: Matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

/// `a × b`, PDF's row-vector convention: apply `a`, then `b`.
fn multiply(a: &Matrix, b: &Matrix) -> Matrix {
    [
        a[0] * b[0] + a[1] * b[2],
        a[0] * b[1] + a[1] * b[3],
        a[2] * b[0] + a[3] * b[2],
        a[2] * b[1] + a[3] * b[3],
        a[4] * b[0] + a[5] * b[2] + b[4],
        a[4] * b[1] + a[5] * b[3] + b[5],
    ]
}

fn number(o: &Object) -> Option<f32> {
    match o {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r),
        _ => None,
    }
}

fn numbers<const N: usize>(operands: &[Object]) -> Option<[f32; N]> {
    let mut out = [0.0; N];
    for (slot, o) in out.iter_mut().zip(operands) {
        *slot = number(o)?;
    }
    (operands.len() >= N).then_some(out)
}

/// Form XObjects can nest; this is how deep a page is followed before it is
/// treated as malformed rather than recursed into for ever.
const MAX_DEPTH: usize = 8;

fn walk(
    doc: &Document,
    content: &[u8],
    base: Matrix,
    // Innermost first: a form XObject's own fonts shadow the page's.
    encodings: &[&BTreeMap<Vec<u8>, FontInfo>],
    resources: Option<&lopdf::Dictionary>,
    out: &mut Vec<Chunk>,
    depth: usize,
) {
    let Ok(content) = Content::decode(content) else {
        return;
    };
    let mut ctm = base;
    let mut stack: Vec<Matrix> = Vec::new();
    let mut tm = IDENTITY;
    let mut tlm = IDENTITY;
    let mut leading = 0.0f32;
    let mut font: Option<Vec<u8>> = None;
    let mut font_size = 0.0f32;

    let next_line = |tlm: &mut Matrix, tm: &mut Matrix, tx: f32, ty: f32| {
        *tlm = multiply(&[1.0, 0.0, 0.0, 1.0, tx, ty], tlm);
        *tm = *tlm;
    };

    for op in &content.operations {
        let args = &op.operands;
        match op.operator.as_str() {
            "q" => stack.push(ctm),
            "Q" => {
                if let Some(m) = stack.pop() {
                    ctm = m;
                }
            }
            "cm" => {
                if let Some(m) = numbers::<6>(args) {
                    ctm = multiply(&m, &ctm);
                }
            }
            "BT" => {
                tm = IDENTITY;
                tlm = IDENTITY;
            }
            "Tf" => {
                font = args.first().and_then(|o| o.as_name().ok()).map(<[u8]>::to_vec);
                font_size = args.get(1).and_then(number).unwrap_or(0.0);
            }
            "TL" => leading = args.first().and_then(number).unwrap_or(leading),
            "Td" => {
                if let Some([tx, ty]) = numbers::<2>(args) {
                    next_line(&mut tlm, &mut tm, tx, ty);
                }
            }
            "TD" => {
                if let Some([tx, ty]) = numbers::<2>(args) {
                    leading = -ty;
                    next_line(&mut tlm, &mut tm, tx, ty);
                }
            }
            "Tm" => {
                if let Some(m) = numbers::<6>(args) {
                    tm = m;
                    tlm = m;
                }
            }
            "T*" => next_line(&mut tlm, &mut tm, 0.0, -leading),
            "Tj" | "TJ" | "'" | "\"" => {
                if matches!(op.operator.as_str(), "'" | "\"") {
                    next_line(&mut tlm, &mut tm, 0.0, -leading);
                }
                let Some(info) = font
                    .as_ref()
                    .and_then(|f| encodings.iter().find_map(|layer| layer.get(f)))
                else {
                    continue;
                };
                let mut text = String::new();
                // In thousandths of the font size, as glyph widths are.
                let mut advance = 0.0f32;
                for operand in args {
                    collect(&mut text, &mut advance, info, operand);
                }
                let width = advance / 1000.0 * font_size;
                let start = multiply(&tm, &ctm);
                tm = multiply(&[1.0, 0.0, 0.0, 1.0, width, 0.0], &tm);
                let end = multiply(&tm, &ctm);
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let scale = (start[2] * start[2] + start[3] * start[3]).sqrt();
                out.push(Chunk {
                    x: start[4],
                    x_end: end[4],
                    y: start[5],
                    size: font_size * scale,
                    text: trimmed.to_string(),
                });
            }
            "Do" if depth < MAX_DEPTH => {
                let Some(name) = args.first().and_then(|o| o.as_name().ok()) else {
                    continue;
                };
                let Some(stream_id) = resources
                    .and_then(|r| dict_entry(doc, r, b"XObject"))
                    .and_then(|x| x.get(name).ok())
                    .and_then(|o| o.as_reference().ok())
                else {
                    continue;
                };
                let Ok(Object::Stream(stream)) = doc.get_object(stream_id) else {
                    continue;
                };
                let is_form = stream
                    .dict
                    .get(b"Subtype")
                    .and_then(|s| s.as_name())
                    .map(|s| s == b"Form")
                    .unwrap_or(false);
                if !is_form {
                    continue;
                }
                let matrix = stream
                    .dict
                    .get(b"Matrix")
                    .ok()
                    .and_then(|m| m.as_array().ok())
                    .and_then(|a| numbers::<6>(a))
                    .unwrap_or(IDENTITY);
                let inner_resources = dict_entry(doc, &stream.dict, b"Resources").cloned();
                let mut inner_encodings: BTreeMap<Vec<u8>, FontInfo> = BTreeMap::new();
                if let Some(fonts) = inner_resources
                    .as_ref()
                    .and_then(|r| dict_entry(doc, r, b"Font"))
                {
                    for (fname, f) in fonts.iter() {
                        let font_dict = match f {
                            Object::Reference(id) => doc.get_dictionary(*id).ok(),
                            Object::Dictionary(d) => Some(d),
                            _ => None,
                        };
                        if let Some(info) = font_dict.and_then(|d| font_info(doc, d)) {
                            inner_encodings.insert(fname.clone(), info);
                        }
                    }
                }
                let data = stream
                    .decompressed_content()
                    .unwrap_or_else(|_| stream.content.clone());
                let mut layers: Vec<&BTreeMap<Vec<u8>, FontInfo>> = vec![&inner_encodings];
                layers.extend_from_slice(encodings);
                walk(
                    doc,
                    &data,
                    multiply(&matrix, &ctm),
                    &layers,
                    inner_resources.as_ref().or(resources),
                    out,
                    depth + 1,
                );
            }
            _ => {}
        }
    }
}

/// A dictionary entry that is itself a dictionary, directly or by reference.
fn dict_entry<'a>(
    doc: &'a Document,
    dict: &'a lopdf::Dictionary,
    key: &[u8],
) -> Option<&'a lopdf::Dictionary> {
    match dict.get(key).ok()? {
        Object::Dictionary(d) => Some(d),
        Object::Reference(id) => doc.get_dictionary(*id).ok(),
        _ => None,
    }
}

fn collect(text: &mut String, advance: &mut f32, font: &FontInfo, operand: &Object) {
    match operand {
        Object::String(bytes, _) => {
            let _ = font.encoding.write_to_string(bytes, text);
            *advance += font.advance(bytes);
        }
        Object::Array(items) => {
            for item in items {
                match item {
                    Object::Integer(_) | Object::Real(_) => {
                        let kern = number(item).unwrap_or(0.0);
                        // A large negative kern is a space the layout program
                        // drew as a gap rather than a character.
                        if kern < -200.0 {
                            text.push(' ');
                        }
                        *advance -= kern;
                    }
                    other => collect(text, advance, font, other),
                }
            }
        }
        _ => {}
    }
}

/// Group chunks into lines by baseline, top of the page first.
fn into_lines(mut chunks: Vec<Chunk>) -> Vec<TextLine> {
    chunks.sort_by(|a, b| b.y.total_cmp(&a.y).then(a.x.total_cmp(&b.x)));
    let mut lines: Vec<TextLine> = Vec::new();
    for c in chunks {
        match lines.last_mut() {
            Some(line) if (line.y - c.y).abs() <= SAME_LINE => line.chunks.push(c),
            _ => lines.push(TextLine {
                y: c.y,
                chunks: vec![c],
            }),
        }
    }
    for line in &mut lines {
        line.chunks.sort_by(|a, b| a.x.total_cmp(&b.x));
        line.chunks = merge_runs(std::mem::take(&mut line.chunks));
    }
    lines
}

/// Join runs that are one piece of text.
///
/// Plenty of form software draws its labels a glyph at a time, so "Ordinary" is
/// eight chunks. Two runs whose gap is under a space wide are one word; under
/// about two spaces, one phrase with a space between. Anything wider is a
/// different cell of the form and stays its own chunk — a label and the figure
/// beside it must not become one string.
fn merge_runs(chunks: Vec<Chunk>) -> Vec<Chunk> {
    let mut out: Vec<Chunk> = Vec::new();
    for c in chunks {
        if let Some(prev) = out.last_mut() {
            let size = prev.size.max(c.size).max(1.0);
            let gap = c.x - prev.x_end;
            if gap < 0.12 * size {
                prev.text.push_str(&c.text);
                prev.x_end = prev.x_end.max(c.x_end);
                continue;
            }
            if gap < 0.6 * size {
                prev.text.push(' ');
                prev.text.push_str(&c.text);
                prev.x_end = prev.x_end.max(c.x_end);
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// A PDF whose text is placed at known points, one page per entry — for tests
/// that need a form laid out like a real one without carrying anybody's real
/// form. Helvetica with no /Widths, so glyphs advance half an em.
#[cfg(test)]
pub(crate) fn fixture_pdf(pages: &[Vec<(f32, f32, String)>], size: f32) -> Document {
    use lopdf::{dictionary, Stream};
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let mut kids = Vec::new();
    for text in pages {
        let mut ops = String::new();
        for (x, y, t) in text {
            ops.push_str(&format!(
                "BT /F1 {size} Tf 1 0 0 1 {x} {y} Tm ({}) Tj ET\n",
                t.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)")
            ));
        }
        let content_id = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        kids.push(page_id.into());
    }
    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;
    use lopdf::{Object, Stream};

    /// A one-page PDF whose text is placed at known points.
    pub(crate) fn pdf_with(text: &[(f32, f32, &str)]) -> Document {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let mut ops = String::new();
        for (x, y, t) in text {
            ops.push_str(&format!(
                "BT /F1 9 Tf 1 0 0 1 {x} {y} Tm ({}) Tj ET\n",
                t.replace('(', "\\(").replace(')', "\\)")
            ));
        }
        let content_id = doc.add_object(Stream::new(dictionary! {}, ops.into_bytes()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc
    }

    /// Positions survive, which is the whole reason this module exists: two
    /// figures emitted after all the labels still land beside their labels.
    #[test]
    fn text_keeps_its_place_on_the_page() {
        let doc = pdf_with(&[
            (50.0, 700.0, "1 Ordinary business income"),
            (50.0, 680.0, "2 Net rental real estate income"),
            (300.0, 690.0, "7,914."),
            (300.0, 670.0, "-9,520."),
        ]);
        let pages = pages(&doc);
        assert_eq!(pages.len(), 1);
        let lines = &pages[0].lines;
        assert_eq!(lines[0].chunks[0].text, "1 Ordinary business income");
        assert_eq!(lines[1].chunks[0].text, "7,914.");
        assert!((lines[1].chunks[0].x - 300.0).abs() < 0.01);
        assert!(lines[1].y < lines[0].y, "top of the page first");
        assert!(pages[0].mentions("ORDINARY  business income"));
    }

    /// Two runs on one baseline are one line, ordered left to right however the
    /// content stream ordered them.
    #[test]
    fn runs_on_one_baseline_are_one_line_left_to_right() {
        let doc = pdf_with(&[(300.0, 500.0, "16,438"), (50.0, 500.5, "2. Allocable")]);
        let lines = &pages(&doc)[0].lines;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chunks[0].text, "2. Allocable");
        assert!(lines[0].layout().contains("2. Allocable"));
        assert!(lines[0].layout().ends_with("16,438"));
    }
}
