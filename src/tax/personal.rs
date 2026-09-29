//! What a personal return is built from: the statements recorded in a person's
//! books, gathered by where each amount goes.
//!
//! # A scaffold, deliberately
//!
//! This is the input side of Form 1040 and nothing more. It answers the question
//! somebody has in February with a folder of 1099s — what has arrived, what is
//! still missing, and where does each figure go — without yet computing a line.
//! Computing lines belongs to a dated line map like Form 1065's
//! ([`super::lines`]), and this summary is what that map will read.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::commands::{document_commands, tax_statement_commands};
use crate::domain::documents::{Document, K1Link, TaxStatement};
use crate::tax::information_returns::FormKind;

/// One box on one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution {
    pub statement_id: String,
    pub form: FormKind,
    pub issuer: String,
    pub box_code: String,
    /// The box's label, or empty for a code the form has no catalogue entry for.
    pub label: &'static str,
    /// In cents.
    pub cents: i64,
}

/// Everything headed for one place on the return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub destination: &'static str,
    /// In cents.
    pub cents: i64,
    pub contributions: Vec<Contribution>,
}

/// A tax year's inputs, as far as the books know them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonalInputs {
    pub tax_year: i32,
    pub statements: Vec<TaxStatement>,
    /// Summed boxes, gathered by destination. Interest from a bank's 1099-INT
    /// and from a partnership's K-1 land in the same entry, which is the point.
    pub destinations: Vec<Destination>,
    /// Boxes that are totals or positions — shown, never added. See
    /// [`crate::tax::information_returns::BoxDef::summed`].
    pub informational: Vec<Contribution>,
    /// Amounts on a form with no catalogue yet, which no destination picks up.
    pub unrouted: Vec<Contribution>,
    /// Partnerships linked to these books with no K-1 recorded for the year.
    pub missing_k1s: Vec<K1Link>,
    /// The year's documents that no statement was read from.
    pub unread_documents: Vec<Document>,
}

/// Gather a tax year's statements by destination, and say what is missing.
pub fn inputs_for_year(conn: &Connection, tax_year: i32) -> PersonalInputs {
    let statements = tax_statement_commands::list(conn, Some(tax_year));

    let mut by_destination: BTreeMap<&'static str, Destination> = BTreeMap::new();
    let mut informational = Vec::new();
    let mut unrouted = Vec::new();
    for statement in &statements {
        for (code, cents) in &statement.amounts {
            let def = statement.form.box_def(code);
            let contribution = Contribution {
                statement_id: statement.statement_id.clone(),
                form: statement.form,
                issuer: statement.issuer.clone(),
                box_code: code.clone(),
                label: def.map_or("", |d| d.label),
                cents: *cents,
            };
            match def {
                None => unrouted.push(contribution),
                Some(def) if !def.summed => informational.push(contribution),
                Some(def) => {
                    let entry =
                        by_destination
                            .entry(def.destination)
                            .or_insert_with(|| Destination {
                                destination: def.destination,
                                cents: 0,
                                contributions: Vec::new(),
                            });
                    entry.cents += cents;
                    entry.contributions.push(contribution);
                }
            }
        }
    }

    let missing_k1s = tax_statement_commands::list_k1_links(conn)
        .into_iter()
        .filter(|link| {
            let id = tax_statement_commands::k1_statement_id(link, tax_year);
            !statements.iter().any(|s| s.statement_id == id)
        })
        .collect();
    let unread_documents = document_commands::list(conn, Some(tax_year))
        .into_iter()
        .filter(|d| {
            !statements
                .iter()
                .any(|s| s.document_ids.contains(&d.document_id))
        })
        .collect();

    PersonalInputs {
        tax_year,
        statements,
        destinations: by_destination.into_values().collect(),
        informational,
        unrouted,
        missing_k1s,
        unread_documents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::partnership_commands::append_event_locally;
    use crate::domain::documents::StatementSource;
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;

    const YEAR: i32 = 2025;

    fn books() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut store).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES ('me', 'me', 'Me', 'USD', 1)",
                [],
            )
            .unwrap();
        store
    }

    fn entered(store: &mut EventStore, form: FormKind, issuer: &str, boxes: &[(&str, i64)]) {
        tax_statement_commands::record(
            store,
            "user",
            &TaxStatement {
                statement_id: tax_statement_commands::new_statement_id(),
                tax_year: YEAR,
                form,
                issuer: issuer.to_string(),
                amounts: boxes.iter().map(|(c, v)| (c.to_string(), *v)).collect(),
                document_ids: Vec::new(),
                source: StatementSource::Entered,
                note: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn interest_from_a_bank_and_from_a_partnership_land_in_one_place() {
        let mut store = books();
        entered(
            &mut store,
            FormKind::F1099Int,
            "First Bank",
            &[("1", 10_000)],
        );
        entered(
            &mut store,
            FormKind::K1Partnership,
            "Example Art House LLC",
            &[("5", 5_000), ("4a", 20_000), ("4c", 20_000)],
        );

        let inputs = inputs_for_year(store.connection(), YEAR);
        let interest = inputs
            .destinations
            .iter()
            .find(|d| d.destination == FormKind::F1099Int.box_def("1").unwrap().destination)
            .expect("interest is gathered");
        assert_eq!(interest.cents, 15_000);
        assert_eq!(interest.contributions.len(), 2);

        // Box 4c is the total of 4a and 4b: shown, not added a second time.
        assert_eq!(inputs.informational.len(), 1);
        assert_eq!(inputs.informational[0].box_code, "4c");
        let summed: i64 = inputs.destinations.iter().map(|d| d.cents).sum();
        assert_eq!(summed, 35_000);
    }

    #[test]
    fn a_linked_partnership_with_no_k1_yet_is_missing() {
        let mut store = books();
        append_event_locally(
            &mut store,
            "user",
            Event::K1SourceLinked {
                link_id: "partnership:partner".to_string(),
                ledger_id: "partnership".to_string(),
                ledger_name: "Example Art House LLC".to_string(),
                partner_id: "partner".to_string(),
                partner_name: "Me".to_string(),
            },
        )
        .unwrap();
        let inputs = inputs_for_year(store.connection(), YEAR);
        assert_eq!(inputs.missing_k1s.len(), 1);
        assert_eq!(inputs.missing_k1s[0].ledger_name, "Example Art House LLC");
    }

    #[test]
    fn a_document_nothing_was_read_from_is_listed_as_unread() {
        let mut store = books();
        let dir = tempfile::tempdir().unwrap();
        let blobs = crate::documents::LocalBlobStore::at(dir.path());
        document_commands::attach(
            &mut store,
            &blobs,
            "user",
            document_commands::AttachDocument {
                bytes: b"%PDF-1.7 a W-2",
                filename: "w2.pdf",
                title: None,
                tax_year: Some(YEAR),
                form: Some(FormKind::W2),
            },
        )
        .unwrap();
        let inputs = inputs_for_year(store.connection(), YEAR);
        assert_eq!(inputs.unread_documents.len(), 1);
        assert!(inputs.statements.is_empty());
    }
}
