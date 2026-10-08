//! A file attached to a set of books, the tax statements read from such files, and
//! the links through which a K-1 reaches these books from other books managed here.
//!
//! The wire shape lives in [`crate::events::types`]; this is the shape the rest of the
//! program works with, read back from the projection. The bytes are in a
//! [`crate::documents::BlobStore`], under [`Document::sha256`].

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};

use crate::tax::information_returns::FormKind;
use crate::tax::schedule_d::Category;

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

/// What a received statement says, box by box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxStatement {
    pub statement_id: String,
    pub tax_year: i32,
    pub form: FormKind,
    /// Who sent it: the employer, the bank, the partnership.
    pub issuer: String,
    /// Box code to amount, in cents.
    pub amounts: BTreeMap<String, i64>,
    /// The attached documents it was read from.
    pub document_ids: Vec<String>,
    pub source: StatementSource,
    pub note: Option<String>,
}

impl TaxStatement {
    /// A box's amount, in cents. A box nothing was recorded in is zero.
    pub fn amount(&self, code: &str) -> i64 {
        self.amounts.get(code).copied().unwrap_or(0)
    }
}

/// Where a statement's figures came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StatementSource {
    /// Typed in from the paper, or from the attached document.
    #[default]
    Entered,
    /// Computed by another set of books managed here.
    Ledger(LedgerProvenance),
}

/// Which books produced a statement, and how far into their log they had got.
///
/// `through_event` and `event_hash` pin the figures to one state of the source
/// books. A return prepared from them can later be checked against those books
/// as they are now, and a difference is a K-1 that changed after it was pulled —
/// which is exactly the thing somebody filing a personal return needs to hear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerProvenance {
    pub ledger_id: String,
    pub ledger_name: String,
    pub partner_id: String,
    pub partner_name: String,
    pub through_event: i64,
    pub event_hash: String,
}

/// A partnership managed in accountir whose K-1 for one of its partners belongs
/// in these books.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct K1Link {
    pub link_id: String,
    pub ledger_id: String,
    pub ledger_name: String,
    pub partner_id: String,
    pub partner_name: String,
}

impl K1Link {
    /// The id a link to this partner in those books always has, so linking
    /// twice is one link rather than two K-1s for the same interest.
    pub fn id_for(ledger_id: &str, partner_id: &str) -> String {
        format!("{ledger_id}:{partner_id}")
    }
}

/// A sole proprietorship managed in accountir whose Schedule C belongs on these
/// books' Form 1040. The counterpart of [`K1Link`]; the id is the business's
/// ledger id, since a business files one Schedule C.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleCLink {
    pub link_id: String,
    pub ledger_id: String,
    pub ledger_name: String,
    /// The proprietor as those books named them when linked. Empty if they named
    /// nobody yet.
    pub proprietor_name: String,
}

/// One transaction on a received statement — a Form 8949 row.
///
/// Only a 1099-B has these, and only where the form requires a transaction to be
/// listed rather than subtotalled: the noncovered categories, anything not
/// reported on a 1099-B, and any transaction the broker adjusted. See
/// [`crate::tax::schedule_d`] for which is which, and
/// [`crate::events::types::TaxStatementLineData`] for why the gain is not stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementLine {
    pub statement_id: String,
    pub line_id: String,
    pub category: Category,
    /// Column (a).
    pub description: String,
    /// Column (b): a date, or the word that stands in its place.
    pub acquired: Acquired,
    /// Column (c).
    pub sold_on: NaiveDate,
    /// Column (d), in cents.
    pub proceeds_cents: i64,
    /// Column (e), in cents.
    pub basis_cents: i64,
    /// Column (f).
    pub adjustment_code: Option<String>,
    /// Column (g), in cents. Positive increases the gain.
    pub adjustment_cents: i64,
}

impl StatementLine {
    /// Column (h): proceeds less basis, plus the adjustment. Computed, never
    /// stored — see [`crate::events::types::TaxStatementLineData`].
    pub fn gain_cents(&self) -> i64 {
        self.proceeds_cents - self.basis_cents + self.adjustment_cents
    }
}

/// Form 8949 column (b): when the shares were acquired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquired {
    On(NaiveDate),
    /// One of the words the form takes in a date's place — `VARIOUS` for a sale
    /// across lots bought on different days, `INHERITED` for a holding whose basis
    /// is its value at death.
    Stated(String),
}

impl Acquired {
    /// What column (b) prints.
    pub fn as_printed(&self) -> String {
        match self {
            Acquired::On(date) => date.to_string(),
            Acquired::Stated(word) => word.clone(),
        }
    }
}
