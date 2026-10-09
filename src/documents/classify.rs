//! What kind of document a file is, from what is in it.
//!
//! The media type ([`super::sniff_media_type`]) says how to open a file. This
//! says what it *is* — a K-1 package, and which state K-1s are in it — which is
//! what decides whether figures can be read out of it and where they would go.
//!
//! Recognising is cheap and safe to repeat, so it is done when a file is
//! attached and the answer recorded with it ([`crate::events::types::DocumentAttachedData::kind`]):
//! a replica that has the log but not the bytes still knows what each document
//! is. A file attached before this existed is typed afterwards by
//! [`crate::events::types::Event::DocumentClassified`].

use crate::documents::pdf_text;
use crate::tax::k1_extract;

/// A Schedule K-1 (Form 1065) package: the federal K-1, possibly with state K-1s.
pub const K1_1065: &str = "k1_1065";
/// A package of state K-1s with no federal K-1 page recognised.
pub const STATE_K1S: &str = "state_k1s";

/// What a file was recognised as.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Classification {
    pub kind: Option<String>,
    /// For a K-1 package, the postal codes of the state K-1s in it, in the order
    /// they appear.
    pub parts: Vec<String>,
}

impl Classification {
    /// Whether figures can be read out of a document of this kind.
    pub fn extractable(&self) -> bool {
        matches!(self.kind.as_deref(), Some(K1_1065 | STATE_K1S))
    }
}

/// Recognise a file. Anything that is not a PDF, or a PDF this does not know,
/// is an unclassified document — still attached, just not read.
pub fn classify(bytes: &[u8], media_type: &str) -> Classification {
    if media_type != "application/pdf" {
        return Classification::default();
    }
    let Ok(doc) = lopdf::Document::load_mem(bytes) else {
        return Classification::default();
    };
    classify_pages(&pdf_text::pages(&doc))
}

/// [`classify`], for pages already read.
pub fn classify_pages(pages: &[pdf_text::PageText]) -> Classification {
    let states: Vec<String> = k1_extract::detect_states(pages)
        .into_iter()
        .map(|(code, _, _)| code)
        .collect();
    let kind = if k1_extract::federal_page(pages).is_some() {
        Some(K1_1065)
    } else if !states.is_empty() {
        Some(STATE_K1S)
    } else {
        None
    };
    Classification {
        kind: kind.map(str::to_string),
        parts: if kind.is_some() { states } else { Vec::new() },
    }
}

/// What a kind is called, for a list of documents.
pub fn kind_label(kind: &str) -> &'static str {
    match kind {
        K1_1065 => "Schedule K-1 (Form 1065)",
        STATE_K1S => "State K-1s",
        _ => "Document",
    }
}

/// What a document is, in a line: "Schedule K-1 (Form 1065) with MD, VA K-1s".
pub fn describe(kind: Option<&str>, parts: &[String]) -> Option<String> {
    let kind = kind?;
    Some(match (kind, parts.is_empty()) {
        (K1_1065, false) => format!("{} with {} K-1s", kind_label(kind), parts.join(", ")),
        (STATE_K1S, false) => format!("State K-1s: {}", parts.join(", ")),
        _ => kind_label(kind).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample package is a federal K-1 with Maryland and Virginia K-1s, and
    /// says so; a PDF with nothing recognisable in it is a plain document.
    #[test]
    fn a_k1_package_is_recognised_with_its_states() {
        let mut bytes = Vec::new();
        crate::tax::k1_extract::tests::sample_package()
            .save_to(&mut bytes)
            .unwrap();
        let c = classify(&bytes, "application/pdf");
        assert_eq!(c.kind.as_deref(), Some(K1_1065));
        assert_eq!(c.parts, vec!["MD".to_string(), "VA".to_string()]);
        assert!(c.extractable());
        assert_eq!(
            describe(c.kind.as_deref(), &c.parts).as_deref(),
            Some("Schedule K-1 (Form 1065) with MD, VA K-1s")
        );

        let mut plain = Vec::new();
        crate::documents::pdf_text::fixture_pdf(
            &[vec![(50.0, 700.0, "Monthly statement".to_string())]],
            9.0,
        )
        .save_to(&mut plain)
        .unwrap();
        assert_eq!(classify(&plain, "application/pdf"), Classification::default());
        assert_eq!(classify(b"a,b\n1,2\n", "text/csv"), Classification::default());
    }
}
