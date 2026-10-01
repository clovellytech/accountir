//! A file attached to a set of books.
//!
//! The wire shape lives in [`crate::events::types`]; this is the shape the rest of the
//! program works with, read back from the projection. The bytes are in a
//! [`crate::documents::BlobStore`], under [`Document::sha256`].

use chrono::{DateTime, Utc};

/// A file attached to the books.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub document_id: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub media_type: String,
    /// The name it was attached under. Display only.
    pub filename: String,
    pub title: Option<String>,
    pub tax_year: Option<i32>,
    /// A form code, where the document is a statement: `1099-B`, `W-2`.
    pub form: Option<String>,
    /// What it is about, where it is about one thing.
    pub subject: Option<DocumentSubject>,
    pub attached_at: DateTime<Utc>,
}

impl Document {
    /// What to show in a list: the title somebody gave it, or the file's own name.
    pub fn label(&self) -> &str {
        match self.title.as_deref().map(str::trim) {
            Some(title) if !title.is_empty() => title,
            _ => &self.filename,
        }
    }

    /// The size, in the units a person reads.
    pub fn size_human(&self) -> String {
        match self.size_bytes {
            n if n >= 1024 * 1024 => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
            n if n >= 1024 => format!("{:.0} kB", n as f64 / 1024.0),
            n => format!("{n} bytes"),
        }
    }
}

/// What a document is about.
///
/// Deliberately a small closed set. "Anything can point at anything" is how a link
/// ends up pointing at a row that no longer exists, and each of these three is a thing
/// the program can take somebody to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentSubject {
    /// A receipt or an invoice for one journal entry.
    Entry { entry_id: String },
    /// Something about an account as a whole — a brokerage's year-end statement.
    Account { account_id: String },
    /// The statement a reconciliation was done against, which is the one document that
    /// makes a completed reconciliation checkable afterwards.
    Reconciliation { reconciliation_id: String },
}

impl DocumentSubject {
    /// The pair the projection stores, which is also what a lookup is keyed by.
    pub fn as_columns(&self) -> (&'static str, &str) {
        match self {
            DocumentSubject::Entry { entry_id } => ("entry", entry_id),
            DocumentSubject::Account { account_id } => ("account", account_id),
            DocumentSubject::Reconciliation { reconciliation_id } => {
                ("reconciliation", reconciliation_id)
            }
        }
    }

    /// Back from the columns. `None` for a kind this version does not know, which is
    /// how a document attached by a later build still reads as a document — it simply
    /// has no subject this one can follow.
    pub fn from_columns(kind: &str, id: &str) -> Option<Self> {
        match kind {
            "entry" => Some(DocumentSubject::Entry {
                entry_id: id.to_string(),
            }),
            "account" => Some(DocumentSubject::Account {
                account_id: id.to_string(),
            }),
            "reconciliation" => Some(DocumentSubject::Reconciliation {
                reconciliation_id: id.to_string(),
            }),
            _ => None,
        }
    }
}
