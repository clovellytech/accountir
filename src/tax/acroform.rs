//! Filling and combining AcroForm PDFs, without flattening them.
//!
//! # Why the values are set rather than drawn
//!
//! A filled return has to stay editable: the figures this program knows are a
//! head start, not a filing, and whoever signs it will change some of them.
//! Drawing text onto the page — the easy way to "fill" a PDF — produces a
//! picture of a form that nobody can correct. So field values are set on the
//! form fields themselves and the widgets keep working.
//!
//! # Why the XFA packet is removed
//!
//! IRS forms are XFA hybrids: the same form twice over, once as AcroForm fields
//! and once as an XML packet. Acrobat prefers the XML and ignores AcroForm
//! values; every other viewer does the opposite. Filling one leaves the file
//! showing different content depending on who opens it, which for a tax return
//! is the worst available outcome. [`strip_xfa`] drops the XML so there is one
//! answer, and the surviving AcroForm renders the same everywhere.
//!
//! # Why appended copies get renamed
//!
//! Field names identify values, and every Schedule K-1 is the same form with the
//! same names. Appending three of them unchanged produces one `f1_9` that three
//! widgets share, so typing a TIN for one partner fills it in for all three.
//! [`namespace_fields`] gives each copy its own namespace before it is
//! appended.

use lopdf::{Dictionary, Document, Object, ObjectId};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum FormError {
    #[error("PDF error: {0}")]
    Pdf(#[from] lopdf::Error),
    #[error("Writing the PDF failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("The form has no field named {0}")]
    NoSuchField(String),
    #[error(
        "{0} names more than one field — a bundle has one per Schedule K-1, so pass a namespace"
    )]
    AmbiguousField(String),
    #[error("{field} holds at most {max} characters, but {value:?} is {len}")]
    ValueTooLong {
        field: String,
        value: String,
        len: usize,
        max: usize,
    },
    #[error("{0}")]
    Malformed(String),
}

/// A PDF text string: either PDFDocEncoded bytes or UTF-16BE behind a byte-order
/// mark. IRS field names are the latter, which is why reading them as UTF-8
/// yields nothing that matches.
pub fn decode_pdf_string(b: &[u8]) -> String {
    if b.len() >= 2 && b[0] == 0xFE && b[1] == 0xFF {
        let units: Vec<u16> = b[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        b.iter().map(|&c| c as char).collect()
    }
}

/// Encode a value for a field, as UTF-16BE when it needs to be.
///
/// Plain bytes for plain ASCII keeps the file diffable and small; anything with
/// an accent in it — a partner's name, most obviously — needs the wide form or
/// it renders as mojibake.
fn encode_pdf_string(s: &str) -> Object {
    if s.is_ascii() {
        Object::string_literal(s.as_bytes().to_vec())
    } else {
        let mut bytes = vec![0xFE, 0xFF];
        for unit in s.encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        Object::string_literal(bytes)
    }
}

fn acroform_ref(doc: &Document) -> Option<ObjectId> {
    doc.catalog().ok()?.get(b"AcroForm").ok()?.as_reference().ok()
}

fn acroform_dict(doc: &Document) -> Option<Dictionary> {
    let obj = doc.catalog().ok()?.get(b"AcroForm").ok()?;
    match doc.dereference(obj) {
        Ok((_, Object::Dictionary(d))) => Some(d.clone()),
        _ => None,
    }
}

fn root_fields(doc: &Document) -> Vec<Object> {
    acroform_dict(doc)
        .and_then(|a| a.get(b"Fields").ok().cloned())
        .and_then(|f| doc.dereference(&f).ok().map(|(_, o)| o.clone()))
        .and_then(|o| o.as_array().ok().cloned())
        .unwrap_or_default()
}

/// Every terminal field in the document, keyed by its fully qualified name
/// (`topmostSubform[0].Page1[0].f1_14[0]`).
///
/// Qualified rather than leaf names because a merged bundle has one `f1_9` per
/// partner and they must stay distinguishable; [`FieldMap::find`] takes the
/// short name back for callers that do not care.
pub fn field_map(doc: &Document) -> FieldMap {
    let mut out = BTreeMap::new();
    for f in root_fields(doc) {
        walk_field(doc, &f, "", &mut out);
    }
    FieldMap(out)
}

fn walk_field(doc: &Document, obj: &Object, prefix: &str, out: &mut BTreeMap<String, ObjectId>) {
    let id = match obj {
        Object::Reference(r) => Some(*r),
        _ => None,
    };
    let dict = match doc.dereference(obj) {
        Ok((_, Object::Dictionary(d))) => d.clone(),
        _ => return,
    };

    let own = dict
        .get(b"T")
        .ok()
        .and_then(|o| o.as_str().ok())
        .map(decode_pdf_string);
    let name = match (&own, prefix.is_empty()) {
        (Some(t), true) => t.clone(),
        (Some(t), false) => format!("{prefix}.{t}"),
        (None, _) => prefix.to_string(),
    };

    // Kids that carry their own /T are sub-fields; kids without are the widgets
    // that draw this field, and descending into them would invent names.
    let kids = dict
        .get(b"Kids")
        .ok()
        .and_then(|k| doc.dereference(k).ok())
        .and_then(|(_, o)| o.as_array().ok().cloned());
    if let Some(kids) = kids {
        let has_named = kids.iter().any(|k| {
            matches!(doc.dereference(k), Ok((_, Object::Dictionary(d))) if d.has(b"T"))
        });
        if has_named {
            for k in &kids {
                walk_field(doc, k, &name, out);
            }
            return;
        }
    }

    if let Some(id) = id {
        if dict.has(b"FT") || dict.has(b"T") {
            out.insert(name, id);
        }
    }
}

/// Terminal fields by fully qualified name.
pub struct FieldMap(BTreeMap<String, ObjectId>);

impl FieldMap {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    /// Resolve a leaf name within one namespace — one K-1 of a bundle, or the
    /// `topmostSubform[0]` of the 1065 itself.
    ///
    /// This is how a caller asks for "partner two's TIN box" without writing out
    /// `K1_2.Page1[0].LeftCol[0].f1_9[0]` and having to change it the next time
    /// the IRS moves a box between subforms.
    pub fn find_in(&self, namespace: &str, leaf: &str) -> Option<ObjectId> {
        let root = format!("{namespace}.");
        let suffix = format!(".{leaf}");
        let mut hits = self
            .0
            .iter()
            .filter(|(k, _)| k.starts_with(&root) && k.ends_with(&suffix));
        let first = hits.next().map(|(_, v)| *v);
        match hits.next() {
            Some(_) => None,
            None => first,
        }
    }

    /// Resolve a name, accepting either the full path or the leaf (`f1_14[0]`).
    ///
    /// The suffix match is anchored on a dot so that `f1_1[0]` cannot match
    /// `f1_14[0]` — the kind of near-miss that fills a neighbouring box and is
    /// then very hard to see on a printed form.
    pub fn find(&self, name: &str) -> Option<ObjectId> {
        self.resolve(name).ok()
    }

    /// Resolve a name, saying *why* when it cannot be.
    ///
    /// "No field named `f1_9[0]`" is actively misleading when the truth is that
    /// a bundle has three of them, one per Schedule K-1 — it sends a reader
    /// looking for a renamed box instead of reaching for a namespace.
    pub fn resolve(&self, name: &str) -> Result<ObjectId, FormError> {
        if let Some(id) = self.0.get(name) {
            return Ok(*id);
        }
        let suffix = format!(".{name}");
        let mut hits = self.0.iter().filter(|(k, _)| k.ends_with(&suffix));
        let first = hits.next().map(|(_, v)| *v);
        match (first, hits.next()) {
            (Some(_), Some(_)) => Err(FormError::AmbiguousField(name.to_string())),
            (Some(id), None) => Ok(id),
            (None, _) => Err(FormError::NoSuchField(name.to_string())),
        }
    }
}

/// The character limit a field declares, if it declares one.
///
/// `/MaxLen` is inheritable in principle, so ancestors are consulted before
/// concluding a field is unbounded — a limit read as absent is a limit not
/// enforced.
pub fn max_len(doc: &Document, map: &FieldMap, name: &str) -> Option<usize> {
    let id = map.find(name)?;
    let mut dict = doc.get_dictionary(id).ok()?.clone();
    for _ in 0..8 {
        if let Ok(n) = dict.get(b"MaxLen").and_then(|o| o.as_i64()) {
            return usize::try_from(n).ok();
        }
        match dict.get(b"Parent").and_then(|p| doc.dereference(p)) {
            Ok((_, Object::Dictionary(d))) => dict = d.clone(),
            _ => return None,
        }
    }
    None
}

/// Put a value in a text field.
///
/// Refuses a value longer than the field's `/MaxLen` rather than writing it.
/// An over-long value is the worst of the available failures: some viewers
/// truncate it on load and others hold it but will not display it, so a figure
/// can disappear from a printed return with nothing reporting an error
/// anywhere. Refusing here turns that into something a caller can act on.
pub fn set_text(doc: &mut Document, map: &FieldMap, name: &str, value: &str) -> Result<(), FormError> {
    let id = map.find(name).ok_or_else(|| FormError::NoSuchField(name.into()))?;
    // Counted in UTF-16 code units, which is what /MaxLen bounds — a name with
    // an accent in it costs the same as its ASCII neighbour, but an emoji does
    // not, and the field is the one that decides.
    if let Some(max) = max_len(doc, map, name) {
        let len = value.encode_utf16().count();
        if len > max {
            return Err(FormError::ValueTooLong {
                field: name.to_string(),
                value: value.to_string(),
                len,
                max,
            });
        }
    }
    let dict = doc.get_object_mut(id)?.as_dict_mut()?;
    dict.set("V", encode_pdf_string(value));
    // Drop the cached appearance so the viewer draws the new value. Kept
    // together with the document-level NeedAppearances that `strip_xfa` sets;
    // one without the other leaves fields that are filled but look empty.
    dict.remove(b"AP");
    Ok(())
}

/// Tick a checkbox or select a radio button.
///
/// `on_state` is the appearance state the widget was built with — `1` and `2` on
/// these forms, not the `Yes` that most PDFs use. [`on_states`] reads them out
/// of a document rather than guessing.
pub fn set_check(doc: &mut Document, map: &FieldMap, name: &str, on_state: &str) -> Result<(), FormError> {
    let id = map.resolve(name)?;
    let state = Object::Name(on_state.trim_start_matches('/').as_bytes().to_vec());
    let dict = doc.get_object_mut(id)?.as_dict_mut()?;
    dict.set("V", state.clone());
    dict.set("AS", state);
    Ok(())
}

/// The appearance states a field's widgets accept, excluding `/Off`.
///
/// Used by the tests to assert that the constants in `form1065.rs` still match
/// the vendored PDF, which is the check that catches a new form revision having
/// renumbered everything.
pub fn on_states(doc: &Document, map: &FieldMap, name: &str) -> Vec<String> {
    let Some(id) = map.find(name) else {
        return Vec::new();
    };
    let Ok(dict) = doc.get_dictionary(id) else {
        return Vec::new();
    };

    let widgets: Vec<Dictionary> = match dict
        .get(b"Kids")
        .ok()
        .and_then(|k| doc.dereference(k).ok())
        .and_then(|(_, o)| o.as_array().ok().cloned())
    {
        Some(kids) => kids
            .iter()
            .filter_map(|k| match doc.dereference(k) {
                Ok((_, Object::Dictionary(d))) => Some(d.clone()),
                _ => None,
            })
            .collect(),
        None => vec![dict.clone()],
    };

    let mut out = Vec::new();
    for w in widgets {
        let normal = w
            .get(b"AP")
            .ok()
            .and_then(|ap| doc.dereference(ap).ok())
            .and_then(|(_, o)| o.as_dict().ok().cloned())
            .and_then(|ap| ap.get(b"N").ok().cloned())
            .and_then(|n| doc.dereference(&n).ok().map(|(_, o)| o.clone()));
        if let Some(Object::Dictionary(states)) = normal {
            for (k, _) in states.iter() {
                let s = String::from_utf8_lossy(k).to_string();
                if s != "Off" && !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    }
    out
}

/// Drop the XFA packet and the signature that vouched for the original bytes,
/// leaving a plain AcroForm that renders the same in every viewer. See the module
/// docs for why the XFA half is not optional.
///
/// # Why the usage-rights signature has to go with it
///
/// IRS forms ship *signed*: the catalog carries `/Perms /UR3`, a real
/// `adbe.pkcs7.detached` signature by which Adobe grants Reader extra rights on
/// this document — saving a filled copy, most usefully. Its `ByteRange` covers
/// the file as the IRS built it.
///
/// Every byte of that is stale the moment we write a field, and appending a
/// Schedule K-1 moves the offsets it names besides. Leaving it in place does not
/// preserve anything: Reader validates it, finds it broken, and greets whoever
/// opens the return with "the document has been changed since it was created and
/// use of extended features is no longer available" — on a tax return handed to a
/// partner or an accountant, the first thing they see.
///
/// So the grant is dropped rather than invalidated. The rights it conferred are
/// not ones this bundle needs: it is a plain AcroForm that any viewer can fill,
/// which is the whole point of [`strip_xfa`] in the first place.
pub fn strip_xfa(doc: &mut Document) {
    // The signature lives on the catalog, not on the form dictionary.
    if let Ok(catalog) = doc.catalog_mut() {
        catalog.remove(b"Perms");
    }
    let Some(af) = acroform_ref(doc) else { return };
    if let Ok(dict) = doc.get_object_mut(af).and_then(|o| o.as_dict_mut()) {
        dict.remove(b"XFA");
        // The vendored forms ship appearance streams, but the values we write
        // have none until a viewer builds them.
        dict.set("NeedAppearances", Object::Boolean(true));
        // Nothing in the bundle is signed any more — see above — so the flag
        // would advertise a signature field that is no longer there to fill.
        dict.remove(b"SigFlags");
    }
}

/// Put a whole copy of a form into its own namespace.
///
/// The root field's name is *replaced* rather than prefixed — these forms have a
/// single root, `topmostSubform`, which carries no meaning worth keeping — so
/// partner two's TIN box becomes `K1_2.Page1[0].LeftCol[0].f1_9[0]` and is no
/// longer the same field as partner one's.
///
/// Applied to a Schedule K-1 before anything is written into it.
pub fn namespace_fields(doc: &mut Document, namespace: &str) {
    let roots = root_fields(doc);
    for f in roots {
        if let Object::Reference(id) = f {
            if let Ok(dict) = doc.get_object_mut(id).and_then(|o| o.as_dict_mut()) {
                dict.set("T", encode_pdf_string(namespace));
            }
        }
    }
}

/// Append every page of `other` to `base`, carrying its form fields across.
///
/// The caller is expected to have run [`namespace_fields`] on `other` first;
/// nothing here can tell two identical field names apart once they are in the
/// same document.
pub fn append_document(base: &mut Document, mut other: Document) -> Result<(), FormError> {
    // Move `other`'s object ids clear of `base`'s so nothing collides.
    other.renumber_objects_with(base.max_id + 1);
    base.max_id = other.max_id;

    let pages: Vec<ObjectId> = other.get_pages().values().copied().collect();
    let other_acro = acroform_dict(&other);
    let fields = other_acro
        .as_ref()
        .and_then(|af| af.get(b"Fields").ok().cloned())
        .and_then(|f| other.dereference(&f).ok().map(|(_, o)| o.clone()))
        .and_then(|o| o.as_array().ok().cloned())
        .unwrap_or_default();
    // The fonts the incoming fields name in their /DA strings. Without these the
    // merged form asks for a font the document does not have, and a viewer
    // either substitutes one or draws nothing — the Schedule K-1 names
    // `HelveticaLTStd-Roman`, which the 1065 does not carry.
    let other_fonts = other_acro
        .as_ref()
        .and_then(|af| af.get(b"DR").ok().cloned())
        .and_then(|dr| other.dereference(&dr).ok().map(|(_, o)| o.clone()))
        .and_then(|dr| dr.as_dict().ok().cloned())
        .and_then(|dr| dr.get(b"Font").ok().cloned())
        .and_then(|f| other.dereference(&f).ok().map(|(_, o)| o.clone()))
        .and_then(|o| o.as_dict().ok().cloned());

    base.objects.extend(other.objects);

    // Re-parent the incoming pages onto this document's page tree; a page whose
    // /Parent still points into the old tree is one viewers refuse to render.
    let pages_root = base.catalog()?.get(b"Pages")?.as_reference()?;
    let mut kids = base.get_dictionary(pages_root)?.get(b"Kids")?.as_array()?.clone();
    for p in &pages {
        kids.push(Object::Reference(*p));
        if let Ok(d) = base.get_object_mut(*p).and_then(|o| o.as_dict_mut()) {
            d.set("Parent", Object::Reference(pages_root));
        }
    }
    let count = kids.len() as i64;
    let tree = base.get_object_mut(pages_root)?.as_dict_mut()?;
    tree.set("Kids", Object::Array(kids));
    tree.set("Count", Object::Integer(count));

    // Union the field lists, or the appended pages show widgets that the form
    // does not consider part of itself.
    let af = acroform_ref(base).ok_or_else(|| FormError::Malformed("no AcroForm".into()))?;
    let mut all = base.get_dictionary(af)?.get(b"Fields")?.as_array()?.clone();
    all.extend(fields);
    let dict = base.get_object_mut(af)?.as_dict_mut()?;
    dict.set("Fields", Object::Array(all));
    dict.set("NeedAppearances", Object::Boolean(true));

    if let Some(fonts) = other_fonts {
        merge_dr_fonts(base, af, fonts)?;
    }
    Ok(())
}

/// Read back a field's value within one namespace.
pub fn get_value_in(doc: &Document, map: &FieldMap, namespace: &str, leaf: &str) -> Option<String> {
    read_value(doc, map.find_in(namespace, leaf)?)
}

/// Read a field's current value back, for tests and for checking a bundle.
pub fn get_value(doc: &Document, map: &FieldMap, name: &str) -> Option<String> {
    read_value(doc, map.find(name)?)
}

fn read_value(doc: &Document, id: ObjectId) -> Option<String> {
    let dict = doc.get_dictionary(id).ok()?;
    match dict.get(b"V").ok()? {
        Object::String(b, _) => Some(decode_pdf_string(b)),
        Object::Name(n) => Some(format!("/{}", String::from_utf8_lossy(n))),
        _ => None,
    }
}

/// Add an appended form's fonts to the merged AcroForm's default resources.
///
/// Existing entries win: where both forms define `/Helv` they mean the same
/// font, and replacing the base's would repoint every field already using it at
/// an object from another document for no gain.
fn merge_dr_fonts(base: &mut Document, af: ObjectId, incoming: Dictionary) -> Result<(), FormError> {
    let dr_ref = base.get_dictionary(af)?.get(b"DR").ok().cloned();
    let dr = match dr_ref.as_ref().map(|dr| base.dereference(dr)) {
        Some(Ok((id, Object::Dictionary(d)))) => Some((id, d.clone())),
        _ => None,
    };

    let (dr_id, mut dr_dict) = match dr {
        Some((Some(id), d)) => (Some(id), d),
        Some((None, d)) => (None, d),
        None => (None, Dictionary::new()),
    };

    let mut fonts = match dr_dict.get(b"Font").ok().map(|f| base.dereference(f)) {
        Some(Ok((id, Object::Dictionary(d)))) => (id, d.clone()),
        _ => (None, Dictionary::new()),
    };
    for (name, obj) in incoming.iter() {
        if !fonts.1.has(name) {
            fonts.1.set(name.to_vec(), obj.clone());
        }
    }

    match fonts.0 {
        // The font dictionary is its own object: update it in place, so every
        // /DR that points at it sees the additions.
        Some(id) => {
            *base.get_object_mut(id)? = Object::Dictionary(fonts.1);
        }
        None => dr_dict.set("Font", Object::Dictionary(fonts.1)),
    }

    match dr_id {
        Some(id) => {
            if base.get_object(id).is_ok() {
                *base.get_object_mut(id)? = Object::Dictionary(dr_dict);
            }
        }
        None => {
            let dict = base.get_object_mut(af)?.as_dict_mut()?;
            dict.set("DR", Object::Dictionary(dr_dict));
        }
    }
    Ok(())
}
