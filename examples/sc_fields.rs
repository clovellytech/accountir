//! Dump a Schedule C revision's field names and rectangles.
//!
//! One-off, for checking a new revision against the names `schedule_c.rs` uses.
//! Names alone prove nothing — a box that kept its name can have moved — so the
//! rectangle is printed beside it and read against the printed labels.
//!
//! Usage: cargo run --release --example sc_fields -- <pdf> [name-filter]

use accountir::tax::acroform::{field_map, strip_xfa};
use lopdf::Document;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: sc_fields <pdf> [filter]");
    let filter = std::env::args().nth(2).unwrap_or_default();
    let mut doc = Document::load(&path).expect("load pdf");
    strip_xfa(&mut doc);
    let map = field_map(&doc);

    let mut names: Vec<&String> = map.names().collect();
    names.sort();
    for name in names {
        if !filter.is_empty() && !name.contains(&filter) {
            continue;
        }
        let rect = map
            .find(name)
            .and_then(|id| doc.get_object(id).ok())
            .and_then(|o| o.as_dict().ok())
            .and_then(|d| d.get(b"Rect").ok())
            .map(|r| format!("{r:?}"))
            .unwrap_or_default();
        println!("{name}  {rect}");
    }
}
