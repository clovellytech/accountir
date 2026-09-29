//! Attaching files to a set of books.
//!
//! # Bytes first, then the event
//!
//! Attaching is two writes to two places — the bytes to a [`BlobStore`], the
//! [`Event::DocumentAttached`] to the log — and they cannot be one transaction.
//! The order is chosen so the failure in between is the harmless one. Bytes
//! stored with no event naming them are an orphan a sweep can collect; an event
//! naming bytes that were never stored is a document every replica believes in
//! and nobody can open.
//!
//! # Who can see a document
//!
//! Everyone with access to a set of books sees its documents, as they see its
//! accounts — access within a group is flat (MULTITENANT-SPEC §2a). A personal
//! statement therefore belongs in personal books, and a business's books should
//! only hold what every member of that business may read. Nothing here can
//! soften that, so the places a file is attached say it.

use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::documents::{self, BlobError, BlobStore, LocalBlobStore};
use crate::domain::documents::Document;
use crate::events::types::{DocumentAttachedData, Event, StoredEvent};
use crate::store::event_store::EventStore;
use crate::tax::information_returns::FormKind;

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("Store error: {0}")]
    Store(String),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("these books have no ledger id yet, so there is nowhere to keep their documents")]
    NoLedgerId,
    #[error("no document with id {0}")]
    NoSuchDocument(String),
    #[error("document {document_id} is what {} was read from; remove or re-record that first", statements.join(", "))]
    InUse {
        document_id: String,
        statements: Vec<String>,
    },
}

/// A file to attach, already in memory.
#[derive(Debug, Clone)]
pub struct AttachDocument<'a> {
    pub bytes: &'a [u8],
    /// The name it arrived under. Only its last component is recorded.
    pub filename: &'a str,
    pub title: Option<String>,
    pub tax_year: Option<i32>,
    pub form: Option<FormKind>,
}

/// The blob store these books' documents live in on this machine.
pub fn local_store(conn: &Connection) -> Result<LocalBlobStore, DocumentError> {
    let ledger_id = documents::ledger_id(conn).ok_or(DocumentError::NoLedgerId)?;
    Ok(LocalBlobStore::for_ledger(&ledger_id)?)
}

/// Attach a file: store its bytes, then record that it was attached.
pub fn attach(
    store: &mut EventStore,
    blobs: &dyn BlobStore,
    user_id: &str,
    doc: AttachDocument<'_>,
) -> Result<Document, DocumentError> {
    documents::check_size(doc.bytes.len() as u64)?;
    let filename = documents::display_filename(doc.filename);
    let media_type = documents::sniff_media_type(doc.bytes, &filename).to_string();
    let title = doc
        .title
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    let stored = blobs.put(doc.bytes)?;
    let document_id = Uuid::new_v4().to_string();
    append(
        store,
        user_id,
        Event::DocumentAttached(Box::new(DocumentAttachedData {
            document_id: document_id.clone(),
            sha256: stored.sha256,
            size_bytes: stored.size_bytes,
            media_type,
            filename,
            title,
            tax_year: doc.tax_year,
            form: doc.form.map(|f| f.as_str().to_string()),
        })),
    )?;
    get(store.connection(), &document_id).ok_or_else(|| {
        DocumentError::Store("the attached document did not reach the projection".to_string())
    })
}

/// Attach a file from disk to these books, keeping its bytes in the local store.
pub fn attach_file(
    store: &mut EventStore,
    user_id: &str,
    path: &Path,
    title: Option<String>,
    tax_year: Option<i32>,
    form: Option<FormKind>,
) -> Result<Document, DocumentError> {
    let read_error = |source| DocumentError::Read {
        path: path.display().to_string(),
        source,
    };
    // Sized before it is read, so the wrong file is refused rather than loaded.
    let size = std::fs::metadata(path).map_err(read_error)?.len();
    documents::check_size(size)?;
    let bytes = std::fs::read(path).map_err(read_error)?;
    let blobs = local_store(store.connection())?;
    attach(
        store,
        &blobs,
        user_id,
        AttachDocument {
            bytes: &bytes,
            filename: &path.to_string_lossy(),
            title,
            tax_year,
            form,
        },
    )
}

/// Take a document off the books. Its bytes stay in storage — see
/// [`Event::DocumentRemoved`].
///
/// Refused while a recorded statement names it as its source, because removing
/// it would leave figures on the return with nothing behind them.
pub fn remove(
    store: &mut EventStore,
    user_id: &str,
    document_id: &str,
) -> Result<StoredEvent, DocumentError> {
    if get(store.connection(), document_id).is_none() {
        return Err(DocumentError::NoSuchDocument(document_id.to_string()));
    }
    let statements = statements_citing(store.connection(), document_id);
    if !statements.is_empty() {
        return Err(DocumentError::InUse {
            document_id: document_id.to_string(),
            statements,
        });
    }
    append(
        store,
        user_id,
        Event::DocumentRemoved {
            document_id: document_id.to_string(),
        },
    )
}

/// Documents on the books, newest first — all of them, or one tax year's.
pub fn list(conn: &Connection, tax_year: Option<i32>) -> Vec<Document> {
    let sql = "SELECT document_id, sha256, size_bytes, media_type, filename, title, tax_year,
                      form, attached_at
                 FROM documents
                WHERE ?1 IS NULL OR tax_year = ?1
                ORDER BY attached_at DESC, document_id";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    stmt.query_map(params![tax_year], row_to_document)
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

pub fn get(conn: &Connection, document_id: &str) -> Option<Document> {
    conn.query_row(
        "SELECT document_id, sha256, size_bytes, media_type, filename, title, tax_year,
                form, attached_at
           FROM documents WHERE document_id = ?1",
        [document_id],
        row_to_document,
    )
    .optional()
    .ok()
    .flatten()
}

/// A document's bytes, verified against the digest the log recorded.
pub fn read(
    conn: &Connection,
    blobs: &dyn BlobStore,
    document_id: &str,
) -> Result<Vec<u8>, DocumentError> {
    let doc = get(conn, document_id)
        .ok_or_else(|| DocumentError::NoSuchDocument(document_id.to_string()))?;
    Ok(blobs.get(&doc.sha256)?)
}

/// Whether a document's bytes can be opened on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Present,
    /// The log names it, but its bytes are not here — attached on another
    /// machine, or not yet fetched.
    Missing,
    /// Bytes are here but do not match the digest, or could not be read.
    Damaged(String),
}

/// Every document on the books, and whether its bytes are here and intact.
///
/// Reads and hashes every file, so it is a check to run on request, not on
/// every screen that lists documents — [`BlobStore::contains`] is the cheap
/// version of the question.
pub fn check(conn: &Connection, blobs: &dyn BlobStore) -> Vec<(Document, Availability)> {
    list(conn, None)
        .into_iter()
        .map(|doc| {
            let availability = match blobs.get(&doc.sha256) {
                Ok(_) => Availability::Present,
                Err(BlobError::NotFound(_)) => Availability::Missing,
                Err(e) => Availability::Damaged(e.to_string()),
            };
            (doc, availability)
        })
        .collect()
}

/// The statements that name a document as what they were read from.
fn statements_citing(conn: &Connection, document_id: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare("SELECT statement_id, document_ids FROM tax_statements") else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)));
    let Ok(rows) = rows else {
        return Vec::new();
    };
    rows.filter_map(Result::ok)
        .filter(|(_, ids)| {
            serde_json::from_str::<Vec<String>>(ids)
                .map(|ids| ids.iter().any(|id| id == document_id))
                .unwrap_or(false)
        })
        .map(|(statement_id, _)| statement_id)
        .collect()
}

fn row_to_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    let attached_at: String = row.get(8)?;
    Ok(Document {
        document_id: row.get(0)?,
        sha256: row.get(1)?,
        size_bytes: row.get::<_, i64>(2)?.max(0) as u64,
        media_type: row.get(3)?,
        filename: row.get(4)?,
        title: row.get(5)?,
        tax_year: row.get(6)?,
        form: row
            .get::<_, Option<String>>(7)?
            .and_then(|code| FormKind::parse(&code)),
        attached_at: DateTime::parse_from_rfc3339(&attached_at)
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_default(),
    })
}

fn append(
    store: &mut EventStore,
    user_id: &str,
    event: Event,
) -> Result<StoredEvent, DocumentError> {
    crate::commands::partnership_commands::append_event_locally(store, user_id, event)
        .map_err(|e| DocumentError::Store(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations::SchemaStore;

    pub(crate) fn books(ledger_id: &str) -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut store).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES (?1, ?1, 'Personal', 'USD', 1)",
                [ledger_id],
            )
            .unwrap();
        store
    }

    fn blobs() -> (LocalBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (LocalBlobStore::at(dir.path()), dir)
    }

    fn pdf(store: &mut EventStore, blobs: &LocalBlobStore, bytes: &[u8]) -> Document {
        attach(
            store,
            blobs,
            "user",
            AttachDocument {
                bytes,
                filename: "/scans/2025/1099-INT.pdf",
                title: Some("  Bank interest  ".to_string()),
                tax_year: Some(2025),
                form: Some(FormKind::F1099Int),
            },
        )
        .unwrap()
    }

    #[test]
    fn an_attached_document_is_listed_and_opens_to_its_bytes() {
        let mut store = books("personal");
        let (blobs, _dir) = blobs();
        let doc = pdf(&mut store, &blobs, b"%PDF-1.7 interest");

        assert_eq!(doc.filename, "1099-INT.pdf", "the name, not the path");
        assert_eq!(doc.title.as_deref(), Some("Bank interest"));
        assert_eq!(doc.media_type, "application/pdf");
        assert_eq!(doc.form, Some(FormKind::F1099Int));
        assert_eq!(list(store.connection(), Some(2025)), vec![doc.clone()]);
        assert!(list(store.connection(), Some(2024)).is_empty());
        assert_eq!(
            read(store.connection(), &blobs, &doc.document_id).unwrap(),
            b"%PDF-1.7 interest"
        );
        assert_eq!(
            check(store.connection(), &blobs),
            vec![(doc, Availability::Present)]
        );
    }

    /// The log says the document exists; this machine does not have it. That is
    /// a state to report, not an error to hide.
    #[test]
    fn a_document_whose_bytes_are_elsewhere_is_reported_missing() {
        let mut store = books("personal");
        let (here, _a) = blobs();
        let (elsewhere, _b) = blobs();
        let doc = pdf(&mut store, &elsewhere, b"%PDF-1.7 attached on the laptop");
        assert_eq!(
            check(store.connection(), &here),
            vec![(doc, Availability::Missing)]
        );
    }

    #[test]
    fn removing_a_document_keeps_its_bytes() {
        let mut store = books("personal");
        let (blobs, _dir) = blobs();
        let doc = pdf(&mut store, &blobs, b"%PDF-1.7 keep me");
        remove(&mut store, "user", &doc.document_id).unwrap();
        assert!(list(store.connection(), None).is_empty());
        assert!(blobs.contains(&doc.sha256), "retention outlives a removal");
        assert!(matches!(
            remove(&mut store, "user", &doc.document_id),
            Err(DocumentError::NoSuchDocument(_))
        ));
    }

    #[test]
    fn nothing_is_recorded_for_a_file_that_could_not_be_stored() {
        let mut store = books("personal");
        let (blobs, _dir) = blobs();
        let result = attach(
            &mut store,
            &blobs,
            "user",
            AttachDocument {
                bytes: b"",
                filename: "empty.pdf",
                title: None,
                tax_year: None,
                form: None,
            },
        );
        assert!(matches!(result, Err(DocumentError::Blob(BlobError::Empty))));
        assert!(list(store.connection(), None).is_empty());
    }
}
