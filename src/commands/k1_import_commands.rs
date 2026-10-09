//! Importing a K-1 package: reading its figures out of an attached PDF and, once
//! somebody has looked at them, recording them as statements.
//!
//! # Two steps, on purpose
//!
//! [`extract`] reads; nothing is written. [`plan_accept`] turns what was read into
//! the events that would record it, and [`accept`] appends them. The plan is a
//! separate step so the same decision — which statement is new, which already
//! exists and only gains the document — is made once and carried out either
//! here (books on this machine) or by a client submitting each event to a group
//! server (hosted books), with no second copy of the logic to drift.
//!
//! # A K-1 already recorded
//!
//! The federal K-1 may already be in the books: pulled from the partnership's own
//! books, or typed in. Accepting the PDF then attaches it to that statement and
//! leaves its figures alone — a pulled K-1 is the partnership's own computation,
//! and a typed one is somebody's checked entry. Where the PDF says something
//! different, the plan lists the differences for the person to resolve.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use thiserror::Error;

use crate::commands::document_commands::{self, DocumentError};
use crate::commands::partnership_commands as pc;
use crate::commands::tax_statement_commands;
use crate::documents::BlobStore;
use crate::domain::documents::{Document, StateTaxStatement};
use crate::events::types::{
    Event, StateTaxStatementData, StatementSourceData, StoredEvent, TaxStatementData,
};
use crate::store::event_store::EventStore;
use crate::tax::information_returns::FormKind;
use crate::tax::k1_extract::{self, state_codes, K1Extraction};
use crate::tax::lines::{cents_to_dollars, format_dollars};

#[derive(Debug, Error)]
pub enum ImportError {
    #[error(transparent)]
    Document(#[from] DocumentError),
    #[error("{0} is not a PDF, so there is nothing to read figures from")]
    NotPdf(String),
    #[error("{0} could not be opened as a PDF: {1}")]
    Unreadable(String, String),
    #[error("nothing on {0} was recognised as a K-1")]
    NotAK1(String),
    #[error(
        "the tax year could not be read from the K-1 and the document has none recorded; \
         say which year it is"
    )]
    NoYear,
    #[error("no state K-1 with id {0}")]
    NoSuchStatement(String),
    #[error("Store error: {0}")]
    Store(String),
}

/// Read a K-1 package out of an attached document. Writes nothing.
pub fn extract(
    conn: &Connection,
    blobs: &dyn BlobStore,
    document_id: &str,
) -> Result<K1Extraction, ImportError> {
    let doc = document_commands::get(conn, document_id)
        .ok_or_else(|| DocumentError::NoSuchDocument(document_id.to_string()))?;
    if doc.media_type != "application/pdf" {
        return Err(ImportError::NotPdf(doc.label().to_string()));
    }
    let bytes = document_commands::read(conn, blobs, document_id)?;
    extract_bytes(&bytes, doc.label())
}

/// [`extract`], for bytes in hand.
pub fn extract_bytes(bytes: &[u8], label: &str) -> Result<K1Extraction, ImportError> {
    let pdf = lopdf::Document::load_mem(bytes)
        .map_err(|e| ImportError::Unreadable(label.to_string(), e.to_string()))?;
    let pages = crate::documents::pdf_text::pages(&pdf);
    let x = k1_extract::extract(&pages);
    if x.federal.is_empty() && x.states.is_empty() {
        return Err(ImportError::NotAK1(label.to_string()));
    }
    Ok(x)
}

/// What accepting an extraction would record.
#[derive(Debug, Clone)]
pub struct AcceptPlan {
    pub tax_year: i32,
    pub issuer: String,
    /// In the order they are to be appended: the federal statement first, so the
    /// state statements can name it.
    pub events: Vec<Event>,
    pub federal_statement_id: Option<String>,
    /// The federal K-1 was already recorded; accepting attaches the document to it.
    pub linked_to_existing: bool,
    /// Where the PDF and an already-recorded K-1 disagree, box by box.
    pub differences: Vec<String>,
    /// Figures read but not recorded, because the statement catalogue has no box
    /// for them.
    pub dropped: Vec<String>,
    pub state_statement_ids: Vec<String>,
}

/// Work out the events that record an extraction, without appending them.
///
/// `tax_year` overrides the year read from the K-1, for a package whose year
/// could not be read; otherwise the K-1's own year, then the document's.
pub fn plan_accept(
    conn: &Connection,
    document: &Document,
    x: &K1Extraction,
    tax_year: Option<i32>,
) -> Result<AcceptPlan, ImportError> {
    let year = tax_year
        .or(x.tax_year)
        .or(document.tax_year)
        .ok_or(ImportError::NoYear)?;
    let issuer = x
        .issuer
        .clone()
        .filter(|i| !i.trim().is_empty())
        .unwrap_or_else(|| format!("Partnership ({})", document.label()));

    let mut plan = AcceptPlan {
        tax_year: year,
        issuer: issuer.clone(),
        events: Vec::new(),
        federal_statement_id: None,
        linked_to_existing: false,
        differences: Vec::new(),
        dropped: Vec::new(),
        state_statement_ids: Vec::new(),
    };

    if !x.federal.is_empty() {
        let form = FormKind::K1Partnership;
        let mut amounts: BTreeMap<String, i64> = BTreeMap::new();
        for (code, cents) in &x.federal {
            if form.accepts_box(code) {
                amounts.insert(code.clone(), *cents);
            } else {
                plan.dropped.push(format!("box {code} {}", money(*cents)));
            }
        }
        let existing = tax_statement_commands::list(conn, Some(year))
            .into_iter()
            .find(|s| s.form == form && same_issuer(&s.issuer, &issuer));
        match existing {
            Some(existing) => {
                plan.linked_to_existing = true;
                plan.federal_statement_id = Some(existing.statement_id.clone());
                let codes: BTreeSet<&String> =
                    existing.amounts.keys().chain(amounts.keys()).collect();
                for code in codes {
                    let was = existing.amounts.get(code).copied().unwrap_or(0);
                    let read = amounts.get(code).copied().unwrap_or(0);
                    if was != read {
                        plan.differences.push(format!(
                            "box {code}: recorded {}, the PDF says {}",
                            money(was),
                            money(read)
                        ));
                    }
                }
                if !existing.document_ids.iter().any(|d| d == &document.document_id) {
                    let mut data = tax_statement_commands::statement_data(&existing);
                    data.document_ids.push(document.document_id.clone());
                    plan.events.push(Event::TaxStatementRecorded(Box::new(data)));
                }
            }
            None => {
                let id = format!("doc:{}", document.document_id);
                plan.federal_statement_id = Some(id.clone());
                plan.events
                    .push(Event::TaxStatementRecorded(Box::new(TaxStatementData {
                        statement_id: id,
                        tax_year: year,
                        form: form.as_str().to_string(),
                        issuer: issuer.clone(),
                        amounts,
                        document_ids: vec![document.document_id.clone()],
                        source: StatementSourceData::Entered,
                        note: Some(format!("Read from {}", document.label())),
                    })));
            }
        }
    }

    for st in &x.states {
        let id = format!("doc:{}:{}", document.document_id, st.state);
        let amounts: BTreeMap<String, i64> = st
            .amounts
            .iter()
            .filter(|(c, _)| state_codes::ALL.contains(&c.as_str()))
            .map(|(c, v)| (c.clone(), *v))
            .collect();
        plan.state_statement_ids.push(id.clone());
        plan.events
            .push(Event::StateTaxStatementRecorded(Box::new(StateTaxStatementData {
                statement_id: id,
                tax_year: year,
                state: st.state.clone(),
                issuer: issuer.clone(),
                form: st.form.clone(),
                amounts,
                apportionment_ppm: st.apportionment_ppm,
                federal_statement_id: plan.federal_statement_id.clone(),
                document_ids: vec![document.document_id.clone()],
                note: Some(format!(
                    "Read from {}, page {}",
                    document.label(),
                    st.pages
                        .first()
                        .map(u32::to_string)
                        .unwrap_or_default()
                )),
            })));
    }
    Ok(plan)
}

/// Record an extraction on books held on this machine.
pub fn accept(
    store: &mut EventStore,
    user_id: &str,
    document_id: &str,
    x: &K1Extraction,
    tax_year: Option<i32>,
) -> Result<AcceptPlan, ImportError> {
    let doc = document_commands::get(store.connection(), document_id)
        .ok_or_else(|| DocumentError::NoSuchDocument(document_id.to_string()))?;
    let plan = plan_accept(store.connection(), &doc, x, tax_year)?;
    for event in &plan.events {
        append(store, user_id, event.clone())?;
    }
    Ok(plan)
}

/// Whether two names are the same partnership: case, punctuation and spacing aside.
fn same_issuer(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && (a == b || a.starts_with(&b) || b.starts_with(&a))
}

fn money(cents: i64) -> String {
    format!("${}", format_dollars(cents_to_dollars(cents)))
}

// ---------------------------------------------------------------------------
// State K-1s on the books
// ---------------------------------------------------------------------------

/// The state K-1s recorded — all of them, or one tax year's — by year and state.
pub fn list_state_statements(conn: &Connection, tax_year: Option<i32>) -> Vec<StateTaxStatement> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT statement_id, tax_year, state, issuer, form, amounts, apportionment_ppm,
                federal_statement_id, document_ids, note
           FROM state_tax_statements
          WHERE ?1 IS NULL OR tax_year = ?1
          ORDER BY tax_year, state, issuer",
    ) else {
        return Vec::new();
    };
    stmt.query_map([tax_year], |r| {
        let amounts: String = r.get(5)?;
        let document_ids: String = r.get(8)?;
        Ok(StateTaxStatement {
            statement_id: r.get(0)?,
            tax_year: r.get(1)?,
            state: r.get(2)?,
            issuer: r.get(3)?,
            form: r.get(4)?,
            amounts: serde_json::from_str(&amounts).unwrap_or_default(),
            apportionment_ppm: r.get(6)?,
            federal_statement_id: r.get(7)?,
            document_ids: serde_json::from_str(&document_ids).unwrap_or_default(),
            note: r.get(9)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// One state's K-1s for a year, summed: what that state's return reads.
pub fn state_totals(conn: &Connection, tax_year: i32, state: &str) -> BTreeMap<String, i64> {
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    for s in list_state_statements(conn, Some(tax_year))
        .into_iter()
        .filter(|s| s.state == state)
    {
        for (code, cents) in s.amounts {
            *out.entry(code).or_insert(0) += cents;
        }
    }
    out
}

pub fn remove_state_statement(
    store: &mut EventStore,
    user_id: &str,
    statement_id: &str,
) -> Result<StoredEvent, ImportError> {
    if !list_state_statements(store.connection(), None)
        .iter()
        .any(|s| s.statement_id == statement_id)
    {
        return Err(ImportError::NoSuchStatement(statement_id.to_string()));
    }
    append(
        store,
        user_id,
        Event::StateTaxStatementRemoved {
            statement_id: statement_id.to_string(),
        },
    )
}

fn append(store: &mut EventStore, user_id: &str, event: Event) -> Result<StoredEvent, ImportError> {
    pc::append_event_locally(store, user_id, event).map_err(|e| ImportError::Store(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::document_commands::AttachDocument;
    use crate::documents::LocalBlobStore;
    use crate::store::migrations::SchemaStore;

    fn books() -> (EventStore, tempfile::TempDir, LocalBlobStore) {
        let mut s = EventStore::in_memory().unwrap();
        s.init_schema().unwrap();
        s.run_migrations().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let blobs = LocalBlobStore::at(dir.path());
        (s, dir, blobs)
    }

    fn attach_sample(store: &mut EventStore, blobs: &LocalBlobStore) -> Document {
        let mut bytes = Vec::new();
        crate::tax::k1_extract::tests::sample_package()
            .save_to(&mut bytes)
            .unwrap();
        document_commands::attach(
            store,
            blobs,
            "u",
            AttachDocument {
                bytes: &bytes,
                filename: "k1-package.pdf",
                title: None,
                tax_year: None,
                form: None,
                subject: None,
            },
        )
        .unwrap()
    }

    /// The attachment knows what it is the moment it is attached; reading it
    /// writes nothing; accepting records the federal K-1 and one statement per
    /// state, all citing the document — which can then not be removed out from
    /// under them.
    #[test]
    fn a_k1_package_is_typed_read_reviewed_and_recorded() {
        let (mut store, _dir, blobs) = books();
        let doc = attach_sample(&mut store, &blobs);
        assert_eq!(doc.kind.as_deref(), Some(crate::documents::classify::K1_1065));
        assert_eq!(doc.parts, vec!["MD".to_string(), "VA".to_string()]);

        let head = store.connection().query_row("SELECT MAX(id) FROM events", [], |r| r.get::<_, i64>(0)).unwrap();
        let x = extract(store.connection(), &blobs, &doc.document_id).unwrap();
        let after = store.connection().query_row("SELECT MAX(id) FROM events", [], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(head, after, "reading wrote nothing");

        let plan = accept(&mut store, "u", &doc.document_id, &x, None).unwrap();
        assert_eq!(plan.tax_year, 2025);
        assert!(!plan.linked_to_existing);
        let federal = tax_statement_commands::get(
            store.connection(),
            plan.federal_statement_id.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(federal.form, FormKind::K1Partnership);
        assert_eq!(federal.amount("1"), 1_200_000);
        assert_eq!(federal.document_ids, vec![doc.document_id.clone()]);

        let states = list_state_statements(store.connection(), Some(2025));
        assert_eq!(states.len(), 2);
        let md = state_totals(store.connection(), 2025, "MD");
        assert_eq!(md.get(state_codes::NONRESIDENT_TAX_PAID), Some(&90_000));
        let va = states.iter().find(|s| s.state == "VA").unwrap();
        assert_eq!(va.apportionment_ppm, Some(500_000));
        assert_eq!(va.federal_statement_id, plan.federal_statement_id);

        assert!(matches!(
            document_commands::remove(&mut store, "u", &doc.document_id),
            Err(DocumentError::InUse { .. })
        ));
    }

    /// A K-1 already on the books gains the document; its figures are left alone
    /// and the disagreements are listed.
    #[test]
    fn a_k1_already_recorded_gains_the_document_and_hears_the_differences() {
        let (mut store, _dir, blobs) = books();
        let doc = attach_sample(&mut store, &blobs);
        let typed = crate::domain::documents::TaxStatement {
            statement_id: "typed".into(),
            tax_year: 2025,
            form: FormKind::K1Partnership,
            issuer: "Example Property Fund, LP".into(),
            amounts: BTreeMap::from([("1".to_string(), 1_100_000)]),
            document_ids: Vec::new(),
            source: Default::default(),
            note: None,
        };
        tax_statement_commands::record(&mut store, "u", &typed).unwrap();

        let x = extract(store.connection(), &blobs, &doc.document_id).unwrap();
        let plan = accept(&mut store, "u", &doc.document_id, &x, None).unwrap();
        assert!(plan.linked_to_existing);
        assert_eq!(plan.federal_statement_id.as_deref(), Some("typed"));
        assert!(plan.differences.iter().any(|d| d.starts_with("box 1:")), "{:?}", plan.differences);
        let kept = tax_statement_commands::get(store.connection(), "typed").unwrap();
        assert_eq!(kept.amount("1"), 1_100_000, "the typed figure stands");
        assert_eq!(kept.document_ids, vec![doc.document_id.clone()]);
    }

    /// A document attached before files were typed is typed on request, once.
    #[test]
    fn an_untyped_document_is_classified_afterwards() {
        let (mut store, _dir, blobs) = books();
        let doc = attach_sample(&mut store, &blobs);
        store
            .connection()
            .execute("UPDATE documents SET kind = NULL, kind_parts = ''", [])
            .unwrap();
        let typed = document_commands::classify(&mut store, &blobs, "u", &doc.document_id)
            .unwrap()
            .expect("it changed");
        assert_eq!(typed.kind.as_deref(), Some(crate::documents::classify::K1_1065));
        assert!(document_commands::classify(&mut store, &blobs, "u", &doc.document_id)
            .unwrap()
            .is_none());
    }
}
