//! Dump every filled AcroForm field in a generated return.
//!
//! A one-off for checking that a return says what it is meant to say: the values
//! are read straight out of the document, by name, so what this prints is what a
//! filer would see in the boxes.
//!
//! Usage: cargo run --release --example k1_fields -- <bundle.pdf> [name-filter]

use accountir::tax::acroform::field_map;
use lopdf::Document;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: k1_fields <pdf> [filter]");
    let filter = std::env::args().nth(2).unwrap_or_default();
    let doc = Document::load(&path).expect("load pdf");
    let map = field_map(&doc);

    let mut names: Vec<&String> = map.names().collect();
    names.sort();
    for name in names {
        if !filter.is_empty() && !name.contains(&filter) {
            continue;
        }
        let Some(v) = accountir::tax::acroform::get_value(&doc, &map, name) else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        println!("{name} = {v}");
    }
}
