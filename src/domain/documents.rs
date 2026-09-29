//! Documents attached to a set of books, the tax statements read from them, and
//! the links through which a K-1 reaches these books from other books managed
//! here.
//!
//! The wire shapes live in [`crate::events::types`]; these are the shapes the
//! rest of the program works with, read back from the projections.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};

use crate::tax::information_returns::FormKind;
use crate::tax::schedule_d::Category;

/// A file attached to the books.
///
/// The bytes are not here: they are in a [`crate::documents::BlobStore`], under
/// [`sha256`](Document::sha256).
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
    /// What kind of statement it is, when it is one.
    pub form: Option<FormKind>,
    pub attached_at: DateTime<Utc>,
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
