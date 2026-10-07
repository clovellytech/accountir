//! Recording the statements a set of books receives, and pulling K-1s from other
//! books managed here.
//!
//! # One way in for every figure
//!
//! A W-2 typed in from paper and a K-1 computed by a partnership's own books
//! arrive the same way: as a [`TaxStatement`], box by box. They differ only in
//! [`StatementSource`]. Whatever builds the personal return reads one list, and
//! never needs to know that part of it came from another ledger.
//!
//! # Why a K-1 is copied in rather than read live
//!
//! It would be simpler for the personal return to open the partnership's books
//! and ask. It would also mean:
//!
//! - a personal return that cannot be computed on a machine without the
//!   partnership's file — or, on a group server, by an instance that must never
//!   see another group's books at all (MULTITENANT-SPEC §2a);
//! - a filed return that silently changes whenever the partnership's books do.
//!
//! So [`pull_k1`] copies the figures into these books' log, stamped with the
//! event the partnership's books had reached. The personal return is computed
//! from its own log alone, and [`k1_freshness`] is how somebody who can see both
//! sets of books finds out a K-1 has changed since. Carrying the figures across
//! is the job of whoever holds access to both — this machine today, a client
//! signed in to both groups later — and never a link between servers.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::commands::{document_commands, partnership_commands as pc, sole_proprietor_commands};
use crate::documents;
use crate::domain::documents::{
    Acquired, K1Link, LedgerProvenance, StatementLine, StatementSource, TaxStatement,
};
use crate::domain::BusinessType;
use crate::events::types::{
    Event, StatementSourceData, StoredEvent, TaxStatementData, TaxStatementLineData,
    TaxStatementLinesData,
};
use crate::store::event_store::EventStore;
use crate::tax::information_returns::FormKind;
use crate::tax::schedule_d::Category;
use crate::tax::k1_package::{self, K1PackageError};

#[derive(Debug, Error)]
pub enum StatementError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("Invalid data: {0}")]
    Invalid(String),
    #[error("no statement with id {0}")]
    NoSuchStatement(String),
    #[error("no document with id {0}")]
    NoSuchDocument(String),
    #[error("no K-1 link with id {0}")]
    NoSuchLink(String),
    #[error("{0} have no ledger id, so no link to or from them can be recorded")]
    NoLedgerId(&'static str),
    #[error("a set of books cannot receive a K-1 from itself")]
    SameBooks,
    #[error("{name} files {form}, not Form 1065, so it issues no K-1s")]
    NotAPartnership { name: String, form: &'static str },
    #[error("no partner with id {0} in those books")]
    NoSuchPartner(String),
    #[error(
        "those are not the books this link points at (expected ledger {expected}, found {found})"
    )]
    WrongSource { expected: String, found: String },
    #[error(
        "{form} has no transactions to list. Only a 1099-B does: it reports subtotals per Form \
         8949 category, and the form requires the transactions behind some of those subtotals to \
         be listed one by one."
    )]
    NotA1099B { form: &'static str },
    #[error(transparent)]
    Package(#[from] K1PackageError),
}

/// A fresh id for a statement typed in by hand.
pub fn new_statement_id() -> String {
    Uuid::new_v4().to_string()
}

/// Record a statement, or replace the one with the same id.
pub fn record(
    store: &mut EventStore,
    user_id: &str,
    statement: &TaxStatement,
) -> Result<StoredEvent, StatementError> {
    if statement.issuer.trim().is_empty() {
        return Err(StatementError::Invalid(
            "a statement needs an issuer — who sent it".to_string(),
        ));
    }
    // Checked here as well as in validation, to say which boxes the form does
    // have: "no box 16" alone sends somebody back to the paper to guess.
    for code in statement.amounts.keys() {
        if !statement.form.accepts_box(code) {
            let known: Vec<&str> = statement.form.boxes().iter().map(|b| b.code).collect();
            return Err(StatementError::Invalid(format!(
                "{} has no box {code:?}; its boxes are {}",
                statement.form.label(),
                known.join(", ")
            )));
        }
    }
    for id in &statement.document_ids {
        if document_commands::get(store.connection(), id).is_none() {
            return Err(StatementError::NoSuchDocument(id.clone()));
        }
    }
    append(
        store,
        user_id,
        Event::TaxStatementRecorded(Box::new(to_data(statement))),
    )
}

pub fn remove(
    store: &mut EventStore,
    user_id: &str,
    statement_id: &str,
) -> Result<StoredEvent, StatementError> {
    if get(store.connection(), statement_id).is_none() {
        return Err(StatementError::NoSuchStatement(statement_id.to_string()));
    }
    append(
        store,
        user_id,
        Event::TaxStatementRemoved {
            statement_id: statement_id.to_string(),
        },
    )
}

/// Statements on the books — all of them, or one tax year's — by year, form and
/// issuer.
pub fn list(conn: &Connection, tax_year: Option<i32>) -> Vec<TaxStatement> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT statement_id, tax_year, form, issuer, amounts, document_ids, source, note
           FROM tax_statements
          WHERE ?1 IS NULL OR tax_year = ?1
          ORDER BY tax_year, form, issuer, statement_id",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![tax_year], raw_statement)
        .map(|rows| {
            rows.filter_map(Result::ok)
                .filter_map(to_statement)
                .collect()
        })
        .unwrap_or_default()
}

pub fn get(conn: &Connection, statement_id: &str) -> Option<TaxStatement> {
    conn.query_row(
        "SELECT statement_id, tax_year, form, issuer, amounts, document_ids, source, note
           FROM tax_statements WHERE statement_id = ?1",
        [statement_id],
        raw_statement,
    )
    .optional()
    .ok()
    .flatten()
    .and_then(to_statement)
}

// ---------------------------------------------------------------------------
// A statement's transaction detail: the Form 8949 rows
// ---------------------------------------------------------------------------

/// A fresh id for a transaction entered against a statement.
pub fn new_line_id() -> String {
    Uuid::new_v4().to_string()
}

/// Record the transactions behind a 1099-B's subtotals, replacing whatever the
/// statement had.
///
/// # Why only a 1099-B
///
/// Because no other statement has transactions. A W-2 or a 1099-INT is a set of
/// boxes and nothing else; a 1099-B reports subtotals per Form 8949 category and
/// the form requires some of those categories to be *listed*. Which ones is
/// [`crate::tax::schedule_d::Brokerage1099B::needs_form8949`]'s answer, not this
/// function's: entering detail for a covered category is allowed, because a person
/// transcribing a consolidated statement should not have to know the rule before
/// they start typing, and Schedule D will still subtotal it if nothing in it was
/// adjusted.
///
/// # Why the whole list at once
///
/// The same reason [`record`] replaces a statement rather than merging into it: a
/// corrected consolidated 1099-B is re-entered from the paper, and merging the new
/// rows into the old ones would leave the return listing the sales the correction
/// removed. An empty list clears the detail without removing the statement.
pub fn record_lines(
    store: &mut EventStore,
    user_id: &str,
    statement_id: &str,
    lines: &[StatementLine],
) -> Result<StoredEvent, StatementError> {
    let Some(statement) = get(store.connection(), statement_id) else {
        return Err(StatementError::NoSuchStatement(statement_id.to_string()));
    };
    if statement.form != FormKind::F1099B {
        return Err(StatementError::NotA1099B {
            form: statement.form.label(),
        });
    }
    append(
        store,
        user_id,
        Event::TaxStatementLinesRecorded(Box::new(TaxStatementLinesData {
            statement_id: statement_id.to_string(),
            lines: lines.iter().map(to_line_data).collect(),
        })),
    )
}

/// One statement's transactions, in the order Form 8949 prints them.
pub fn lines_of(conn: &Connection, statement_id: &str) -> Vec<StatementLine> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT statement_id, line_id, category, description, acquired_on, acquired_label,
                sold_on, proceeds_cents, basis_cents, adjustment_code, adjustment_cents
           FROM tax_statement_lines
          WHERE statement_id = ?1
          ORDER BY position, line_id",
    ) else {
        return Vec::new();
    };
    stmt.query_map([statement_id], raw_line)
        .map(|rows| rows.filter_map(Result::ok).filter_map(to_line).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// K-1s from other books managed here
// ---------------------------------------------------------------------------

/// Say that these books receive `partner_id`'s K-1 from the partnership whose
/// books are `source`.
///
/// Linking the same partner twice is one link — see [`K1Link::id_for`].
pub fn link_k1_source(
    store: &mut EventStore,
    user_id: &str,
    source: &Connection,
    partner_id: &str,
) -> Result<K1Link, StatementError> {
    let own = documents::ledger_id(store.connection())
        .ok_or(StatementError::NoLedgerId("these books"))?;
    let ledger_id = documents::ledger_id(source)
        .ok_or(StatementError::NoLedgerId("the partnership's books"))?;
    if own == ledger_id {
        return Err(StatementError::SameBooks);
    }
    let ledger_name = pc::get_profile(source)
        .map(|p| p.legal_name)
        .or_else(|| documents::ledger_name(source))
        .unwrap_or_else(|| ledger_id.clone());
    let kind = sole_proprietor_commands::business_type(source);
    if kind != BusinessType::Partnership {
        return Err(StatementError::NotAPartnership {
            name: ledger_name,
            form: kind.form_name(),
        });
    }
    let partner = pc::list_partners(source)
        .into_iter()
        .find(|p| p.partner_id == partner_id)
        .ok_or_else(|| StatementError::NoSuchPartner(partner_id.to_string()))?;

    let link = K1Link {
        link_id: K1Link::id_for(&ledger_id, &partner.partner_id),
        ledger_id,
        ledger_name,
        partner_id: partner.partner_id,
        partner_name: partner.name,
    };
    append(
        store,
        user_id,
        Event::K1SourceLinked {
            link_id: link.link_id.clone(),
            ledger_id: link.ledger_id.clone(),
            ledger_name: link.ledger_name.clone(),
            partner_id: link.partner_id.clone(),
            partner_name: link.partner_name.clone(),
        },
    )?;
    Ok(link)
}

/// Stop receiving a K-1 through a link. K-1s already pulled through it stay:
/// they are figures a return may already have been filed on.
pub fn unlink_k1_source(
    store: &mut EventStore,
    user_id: &str,
    link_id: &str,
) -> Result<StoredEvent, StatementError> {
    if get_k1_link(store.connection(), link_id).is_none() {
        return Err(StatementError::NoSuchLink(link_id.to_string()));
    }
    append(
        store,
        user_id,
        Event::K1SourceUnlinked {
            link_id: link_id.to_string(),
        },
    )
}

pub fn list_k1_links(conn: &Connection) -> Vec<K1Link> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT link_id, ledger_id, ledger_name, partner_id, partner_name
           FROM k1_links ORDER BY ledger_name, partner_name",
    ) else {
        return Vec::new();
    };
    stmt.query_map([], row_to_link)
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

pub fn get_k1_link(conn: &Connection, link_id: &str) -> Option<K1Link> {
    conn.query_row(
        "SELECT link_id, ledger_id, ledger_name, partner_id, partner_name
           FROM k1_links WHERE link_id = ?1",
        [link_id],
        row_to_link,
    )
    .optional()
    .ok()
    .flatten()
}

/// The id a K-1 pulled through `link` for `year` is recorded under, so pulling
/// again replaces it rather than adding a second K-1 for the same interest.
pub fn k1_statement_id(link: &K1Link, year: i32) -> String {
    format!("k1:{}:{year}", link.link_id)
}

/// What a pull recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PulledK1 {
    pub statement: TaxStatement,
    /// Whether it replaced a K-1 pulled earlier.
    pub replaced: bool,
    /// The partnership return's warnings, for the person relying on its K-1.
    pub warnings: Vec<String>,
}

/// Compute `link`'s K-1 for `year` from the partnership's books, and record it
/// in these books with where it came from.
pub fn pull_k1(
    store: &mut EventStore,
    user_id: &str,
    source: &Connection,
    link: &K1Link,
    year: i32,
) -> Result<PulledK1, StatementError> {
    check_source(source, link)?;
    let package = k1_package::for_partner(source, year, &link.partner_id)?;
    let statement_id = k1_statement_id(link, year);
    let existing = get(store.connection(), &statement_id);

    let statement = TaxStatement {
        statement_id,
        tax_year: year,
        form: FormKind::K1Partnership,
        issuer: package.partnership_name.clone(),
        amounts: package.amounts,
        // The K-1 PDF somebody attached to the last pull, and their note, are
        // theirs rather than the partnership's: a re-pull keeps them.
        document_ids: existing
            .as_ref()
            .map(|s| s.document_ids.clone())
            .unwrap_or_default(),
        note: existing.as_ref().and_then(|s| s.note.clone()),
        source: StatementSource::Ledger(LedgerProvenance {
            ledger_id: package.ledger_id,
            ledger_name: package.partnership_name,
            partner_id: package.partner_id,
            partner_name: package.partner_name,
            through_event: package.through_event,
            event_hash: package.event_hash,
        }),
    };
    record(store, user_id, &statement)?;
    Ok(PulledK1 {
        statement,
        replaced: existing.is_some(),
        warnings: package.warnings,
    })
}

/// How a pulled K-1 compares with what the partnership's books say now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum K1Freshness {
    /// Nothing has been pulled through this link for the year.
    NotPulled,
    /// A statement is recorded under the link's id but was typed in, so there is
    /// nothing to compare it with.
    EnteredByHand,
    /// The partnership's books still produce exactly these figures.
    Current,
    /// They produce different ones now.
    Changed(Vec<BoxChange>),
}

/// One box whose figure moved since the K-1 was pulled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxChange {
    pub code: String,
    /// In cents.
    pub recorded: i64,
    /// In cents.
    pub now: i64,
}

/// Compare the K-1 recorded through `link` for `year` with the one the
/// partnership's books produce now.
///
/// Figures, not event heads: the partnership's books gain events all year that
/// change nothing on a closed year's K-1, and "your K-1 is stale" on every one
/// of them would be a warning people learn to ignore.
pub fn k1_freshness(
    conn: &Connection,
    source: &Connection,
    link: &K1Link,
    year: i32,
) -> Result<K1Freshness, StatementError> {
    let Some(recorded) = get(conn, &k1_statement_id(link, year)) else {
        return Ok(K1Freshness::NotPulled);
    };
    if !matches!(recorded.source, StatementSource::Ledger(_)) {
        return Ok(K1Freshness::EnteredByHand);
    }
    check_source(source, link)?;
    let package = k1_package::for_partner(source, year, &link.partner_id)?;
    let codes: BTreeSet<&String> = recorded
        .amounts
        .keys()
        .chain(package.amounts.keys())
        .collect();
    let changes: Vec<BoxChange> = codes
        .into_iter()
        .filter_map(|code| {
            let before = recorded.amount(code);
            let now = package.amounts.get(code).copied().unwrap_or(0);
            (before != now).then(|| BoxChange {
                code: code.clone(),
                recorded: before,
                now,
            })
        })
        .collect();
    Ok(if changes.is_empty() {
        K1Freshness::Current
    } else {
        K1Freshness::Changed(changes)
    })
}

/// Refuse to read a K-1 out of books other than the ones the link names — a
/// restored copy of a different partnership has partners too.
fn check_source(source: &Connection, link: &K1Link) -> Result<(), StatementError> {
    let found = documents::ledger_id(source).unwrap_or_default();
    if found != link.ledger_id {
        return Err(StatementError::WrongSource {
            expected: link.ledger_id.clone(),
            found,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Conversions between the log's shapes, the projection's and the domain's
// ---------------------------------------------------------------------------

fn to_data(s: &TaxStatement) -> TaxStatementData {
    TaxStatementData {
        statement_id: s.statement_id.clone(),
        tax_year: s.tax_year,
        form: s.form.as_str().to_string(),
        issuer: s.issuer.trim().to_string(),
        amounts: s.amounts.clone(),
        document_ids: s.document_ids.clone(),
        source: match &s.source {
            StatementSource::Entered => StatementSourceData::Entered,
            StatementSource::Ledger(p) => StatementSourceData::Ledger {
                ledger_id: p.ledger_id.clone(),
                ledger_name: p.ledger_name.clone(),
                partner_id: p.partner_id.clone(),
                partner_name: p.partner_name.clone(),
                through_event: p.through_event,
                event_hash: p.event_hash.clone(),
            },
        },
        note: s
            .note
            .as_ref()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty()),
    }
}

type RawStatement = (
    String,
    i32,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

fn raw_statement(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawStatement> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
    ))
}

/// A projected row as a statement. `None` for a form code this version does not
/// know, which only a newer version's events can have put there.
fn to_statement(raw: RawStatement) -> Option<TaxStatement> {
    let (statement_id, tax_year, form, issuer, amounts, document_ids, source, note) = raw;
    let source = match serde_json::from_str::<StatementSourceData>(&source).ok()? {
        StatementSourceData::Entered => StatementSource::Entered,
        StatementSourceData::Ledger {
            ledger_id,
            ledger_name,
            partner_id,
            partner_name,
            through_event,
            event_hash,
        } => StatementSource::Ledger(LedgerProvenance {
            ledger_id,
            ledger_name,
            partner_id,
            partner_name,
            through_event,
            event_hash,
        }),
    };
    Some(TaxStatement {
        statement_id,
        tax_year,
        form: FormKind::parse(&form)?,
        issuer,
        amounts: serde_json::from_str::<BTreeMap<String, i64>>(&amounts).ok()?,
        document_ids: serde_json::from_str(&document_ids).ok()?,
        source,
        note,
    })
}

fn to_line_data(l: &StatementLine) -> TaxStatementLineData {
    let (acquired_on, acquired_label) = match &l.acquired {
        Acquired::On(date) => (Some(*date), None),
        Acquired::Stated(word) => (None, Some(word.trim().to_uppercase())),
    };
    TaxStatementLineData {
        line_id: l.line_id.clone(),
        category: l.category.code().to_string(),
        description: l.description.trim().to_string(),
        acquired_on,
        acquired_label,
        sold_on: l.sold_on,
        proceeds_cents: l.proceeds_cents,
        basis_cents: l.basis_cents,
        adjustment_code: l
            .adjustment_code
            .as_ref()
            .map(|c| c.trim().to_uppercase())
            .filter(|c| !c.is_empty()),
        adjustment_cents: l.adjustment_cents,
    }
}

type RawLine = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    i64,
    i64,
    Option<String>,
    i64,
);

fn raw_line(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawLine> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get(9)?,
        r.get(10)?,
    ))
}

/// A projected row as a transaction. `None` for a row this version cannot read —
/// a category it does not know, or a date it cannot parse — which is the choice
/// [`to_statement`] makes, and for the same reason: a register that will not open
/// is worse than one visibly missing a row.
fn to_line(raw: RawLine) -> Option<StatementLine> {
    let (
        statement_id,
        line_id,
        category,
        description,
        acquired_on,
        acquired_label,
        sold_on,
        proceeds_cents,
        basis_cents,
        adjustment_code,
        adjustment_cents,
    ) = raw;
    let acquired = match (acquired_on, acquired_label) {
        (Some(date), _) => Acquired::On(chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?),
        (None, Some(word)) => Acquired::Stated(word),
        (None, None) => return None,
    };
    Some(StatementLine {
        statement_id,
        line_id,
        category: Category::parse(&category)?,
        description,
        acquired,
        sold_on: chrono::NaiveDate::parse_from_str(&sold_on, "%Y-%m-%d").ok()?,
        proceeds_cents,
        basis_cents,
        adjustment_code,
        adjustment_cents,
    })
}

fn row_to_link(r: &rusqlite::Row<'_>) -> rusqlite::Result<K1Link> {
    Ok(K1Link {
        link_id: r.get(0)?,
        ledger_id: r.get(1)?,
        ledger_name: r.get(2)?,
        partner_id: r.get(3)?,
        partner_name: r.get(4)?,
    })
}

fn append(
    store: &mut EventStore,
    user_id: &str,
    event: Event,
) -> Result<StoredEvent, StatementError> {
    pc::append_event_locally(store, user_id, event)
        .map_err(|e| StatementError::Store(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::documents::LocalBlobStore;
    use crate::domain::{AccountType, Address, BusinessProfile, PartnerType, Residency, Shares};
    use crate::events::types::JournalEntrySource;
    use crate::store::migrations::SchemaStore;
    use chrono::NaiveDate;

    const YEAR: i32 = 2025;

    fn books(ledger_id: &str) -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut store).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES (?1, ?1, ?1, 'USD', 1)",
                [ledger_id],
            )
            .unwrap();
        store
    }

    fn personal() -> EventStore {
        let mut store = books("personal-ledger");
        sole_proprietor_commands::set_business_type(&mut store, "user", BusinessType::Individual)
            .unwrap();
        store
    }

    fn address() -> Address {
        Address {
            street: "1 Main St".to_string(),
            suite: None,
            city: "Chicago".to_string(),
            state: "IL".to_string(),
            postal_code: "60601".to_string(),
            country: None,
        }
    }

    fn account_id(store: &EventStore, number: &str) -> String {
        store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = ?1",
                [number],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn receive(store: &mut EventStore, dollars: i64) {
        let (cash, receipts) = (account_id(store, "1000"), account_id(store, "4000"));
        EntryCommands::new(store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(YEAR, 3, 1).unwrap(),
                memo: "sales".to_string(),
                lines: vec![
                    EntryLine::debit(&cash, dollars * 100, "USD"),
                    EntryLine::credit(&receipts, dollars * 100, "USD"),
                ],
                reference: None,
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();
    }

    /// Two equal general partners and $10,000 of gross receipts: $5,000 each on
    /// box 1. Returns the books and the first partner's id.
    fn partnership() -> (EventStore, String) {
        let mut store = books("partnership-ledger");
        pc::set_profile(
            &mut store,
            "user",
            &BusinessProfile {
                legal_name: "Example Art House LLC".to_string(),
                address: address(),
                ein: "12-3456789".to_string(),
                naics_code: "711300".to_string(),
                formation_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                principal_activity: None,
                principal_product: None,
            },
        )
        .unwrap();
        let mut ids = Vec::new();
        for name in ["Partner A", "Partner B"] {
            let (id, _) = pc::admit_partner(
                &mut store,
                "user",
                &pc::AdmitPartner {
                    name: name.to_string(),
                    partner_type: PartnerType::General,
                    residency: Residency::Domestic,
                    entity_type: "Individual".to_string(),
                    address: address(),
                    start_date: None,
                    shares: Shares::from_percents(50.0, 50.0, 50.0),
                    tin: None,
                },
            )
            .unwrap();
            ids.push(id);
        }
        for (ty, number, name) in [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Revenue, "4000", "Gross receipts"),
        ] {
            AccountCommands::new(&mut store, "user".to_string())
                .create_account(CreateAccountCommand {
                    account_type: ty,
                    account_number: number.to_string(),
                    name: name.to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
        }
        let receipts = account_id(&store, "4000");
        crate::commands::tax_setup_commands::set_account_line(
            &mut store, "user", &receipts, "l1a", YEAR,
        )
        .unwrap();
        receive(&mut store, 10_000);
        (store, ids.remove(0))
    }

    #[test]
    fn a_pulled_k1_arrives_as_a_statement_that_says_where_it_came_from() {
        let (source, partner) = partnership();
        let mut me = personal();
        let link = link_k1_source(&mut me, "user", source.connection(), &partner).unwrap();
        assert_eq!(link.ledger_name, "Example Art House LLC");
        assert_eq!(link.partner_name, "Partner A");

        let pulled = pull_k1(&mut me, "user", source.connection(), &link, YEAR).unwrap();
        assert!(!pulled.replaced);

        let statements = list(me.connection(), Some(YEAR));
        assert_eq!(statements.len(), 1);
        let k1 = &statements[0];
        assert_eq!(k1.form, FormKind::K1Partnership);
        assert_eq!(k1.issuer, "Example Art House LLC");
        assert_eq!(k1.amount("1"), 500_000, "half of $10,000, in cents");
        match &k1.source {
            StatementSource::Ledger(p) => {
                assert_eq!(p.ledger_id, "partnership-ledger");
                assert_eq!(p.partner_id, partner);
                assert_eq!(Some(p.through_event), source.latest_id().unwrap());
            }
            other => panic!("expected ledger provenance, got {other:?}"),
        }
        assert_eq!(
            k1_freshness(me.connection(), source.connection(), &link, YEAR).unwrap(),
            K1Freshness::Current
        );
    }

    /// The case the provenance exists for: the partnership's books change after
    /// the personal return was prepared.
    #[test]
    fn a_k1_that_changed_since_it_was_pulled_says_so_and_a_repull_replaces_it() {
        let (mut source, partner) = partnership();
        let mut me = personal();
        let link = link_k1_source(&mut me, "user", source.connection(), &partner).unwrap();
        pull_k1(&mut me, "user", source.connection(), &link, YEAR).unwrap();

        receive(&mut source, 2_000);
        match k1_freshness(me.connection(), source.connection(), &link, YEAR).unwrap() {
            K1Freshness::Changed(changes) => assert!(
                changes.contains(&BoxChange {
                    code: "1".to_string(),
                    recorded: 500_000,
                    now: 600_000,
                }),
                "{changes:?}"
            ),
            other => panic!("expected a change, got {other:?}"),
        }

        let again = pull_k1(&mut me, "user", source.connection(), &link, YEAR).unwrap();
        assert!(again.replaced);
        let statements = list(me.connection(), Some(YEAR));
        assert_eq!(statements.len(), 1, "replaced, not added to");
        assert_eq!(statements[0].amount("1"), 600_000);
    }

    #[test]
    fn a_repull_keeps_the_document_and_note_attached_to_the_last_one() {
        let (source, partner) = partnership();
        let mut me = personal();
        let link = link_k1_source(&mut me, "user", source.connection(), &partner).unwrap();
        let mut k1 = pull_k1(&mut me, "user", source.connection(), &link, YEAR)
            .unwrap()
            .statement;

        let dir = tempfile::tempdir().unwrap();
        let blobs = LocalBlobStore::at(dir.path());
        let pdf = document_commands::attach(
            &mut me,
            &blobs,
            "user",
            document_commands::AttachDocument {
                bytes: b"%PDF-1.7 the signed K-1",
                filename: "k1.pdf",
                title: None,
                tax_year: Some(YEAR),
                form: Some(FormKind::K1Partnership.as_str().to_string()),
                subject: None,
            },
        )
        .unwrap();
        k1.document_ids = vec![pdf.document_id.clone()];
        k1.note = Some("as mailed".to_string());
        record(&mut me, "user", &k1).unwrap();

        let again = pull_k1(&mut me, "user", source.connection(), &link, YEAR).unwrap();
        assert_eq!(again.statement.document_ids, vec![pdf.document_id.clone()]);
        assert_eq!(again.statement.note.as_deref(), Some("as mailed"));

        // And the document cannot now be removed out from under it.
        assert!(matches!(
            document_commands::remove(&mut me, "user", &pdf.document_id),
            Err(document_commands::DocumentError::InUse { .. })
        ));
    }

    #[test]
    fn linking_twice_is_one_link() {
        let (source, partner) = partnership();
        let mut me = personal();
        link_k1_source(&mut me, "user", source.connection(), &partner).unwrap();
        link_k1_source(&mut me, "user", source.connection(), &partner).unwrap();
        assert_eq!(list_k1_links(me.connection()).len(), 1);
    }

    #[test]
    fn a_link_is_refused_to_itself_to_books_that_are_not_a_partnership_and_to_a_stranger() {
        let (source, _) = partnership();
        let mut me = personal();
        let other = personal_named("someone-elses-personal-ledger");

        assert!(matches!(
            link_k1_source(&mut me, "user", source.connection(), "no-such-partner"),
            Err(StatementError::NoSuchPartner(_))
        ));
        assert!(matches!(
            link_k1_source(&mut me, "user", other.connection(), "anyone"),
            Err(StatementError::NotAPartnership { .. })
        ));
        let mut source = source;
        let copy = books("partnership-ledger");
        assert!(matches!(
            link_k1_source(&mut source, "user", copy.connection(), "anyone"),
            Err(StatementError::SameBooks)
        ));
    }

    fn personal_named(ledger_id: &str) -> EventStore {
        let mut store = books(ledger_id);
        sole_proprietor_commands::set_business_type(&mut store, "user", BusinessType::Individual)
            .unwrap();
        store
    }

    /// A restored copy of a different partnership has partners too, and maybe
    /// even one with the same id.
    #[test]
    fn a_k1_is_not_pulled_from_books_other_than_the_linked_ones() {
        let (source, partner) = partnership();
        let mut me = personal();
        let link = link_k1_source(&mut me, "user", source.connection(), &partner).unwrap();
        let impostor = books("a-different-partnership");
        assert!(matches!(
            pull_k1(&mut me, "user", impostor.connection(), &link, YEAR),
            Err(StatementError::WrongSource { .. })
        ));
    }

    #[test]
    fn an_entered_statement_is_refused_a_box_its_form_lacks_or_a_document_that_is_not_there() {
        let mut me = personal();
        let mut statement = TaxStatement {
            statement_id: new_statement_id(),
            tax_year: YEAR,
            form: FormKind::F1099Int,
            issuer: "First Bank".to_string(),
            amounts: BTreeMap::from([("1".to_string(), 12_345)]),
            document_ids: Vec::new(),
            source: StatementSource::Entered,
            note: None,
        };
        record(&mut me, "user", &statement).unwrap();
        assert_eq!(
            get(me.connection(), &statement.statement_id),
            Some(statement.clone())
        );

        statement.amounts.insert("16".to_string(), 1);
        let refused = record(&mut me, "user", &statement).unwrap_err().to_string();
        assert!(refused.contains("its boxes are 1, 2, 3, 4, 8"), "{refused}");

        statement.amounts.remove("16");
        statement.document_ids = vec!["nope".to_string()];
        assert!(matches!(
            record(&mut me, "user", &statement),
            Err(StatementError::NoSuchDocument(_))
        ));

        remove(&mut me, "user", &statement.statement_id).unwrap();
        assert!(list(me.connection(), None).is_empty());
    }

    // -----------------------------------------------------------------------
    // A 1099-B's transaction detail
    // -----------------------------------------------------------------------

    fn a_1099b(store: &mut EventStore) -> String {
        let id = new_statement_id();
        record(
            store,
            "user",
            &TaxStatement {
                statement_id: id.clone(),
                tax_year: YEAR,
                form: FormKind::F1099B,
                issuer: "Broad Street Brokerage".to_string(),
                amounts: BTreeMap::from([
                    ("b_proceeds".to_string(), 500_000),
                    ("b_basis".to_string(), 300_000),
                ]),
                document_ids: Vec::new(),
                source: StatementSource::Entered,
                note: None,
            },
        )
        .unwrap();
        id
    }

    fn line(statement_id: &str, line_id: &str, proceeds: i64) -> StatementLine {
        StatementLine {
            statement_id: statement_id.to_string(),
            line_id: line_id.to_string(),
            category: Category::B,
            description: "100 sh. ACME CORP".to_string(),
            acquired: Acquired::On(NaiveDate::from_ymd_opt(2023, 4, 5).unwrap()),
            sold_on: NaiveDate::from_ymd_opt(YEAR, 8, 9).unwrap(),
            proceeds_cents: proceeds,
            basis_cents: 300_000,
            adjustment_code: None,
            adjustment_cents: 0,
        }
    }

    #[test]
    fn transactions_recorded_against_a_1099b_read_back_in_the_order_they_were_entered() {
        let mut me = personal();
        let id = a_1099b(&mut me);
        record_lines(
            &mut me,
            "user",
            &id,
            &[line(&id, "second", 200_000), line(&id, "first", 300_000)],
        )
        .unwrap();

        let read = lines_of(me.connection(), &id);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].line_id, "second", "entry order, not id order");
        assert_eq!(read[0].proceeds_cents, 200_000);
        assert_eq!(read[1].line_id, "first");
        assert_eq!(read[0].gain_cents(), -100_000);
        assert_eq!(read[1].gain_cents(), 0);
    }

    /// Re-recording replaces. A corrected consolidated statement is re-entered
    /// from the paper, and merging would leave the sales the correction removed.
    #[test]
    fn recording_transactions_again_replaces_them_rather_than_adding_to_them() {
        let mut me = personal();
        let id = a_1099b(&mut me);
        record_lines(
            &mut me,
            "user",
            &id,
            &[line(&id, "a", 100_000), line(&id, "b", 200_000)],
        )
        .unwrap();
        record_lines(&mut me, "user", &id, &[line(&id, "a", 150_000)]).unwrap();

        let read = lines_of(me.connection(), &id);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].proceeds_cents, 150_000);

        // And an empty list withdraws the detail without removing the statement.
        record_lines(&mut me, "user", &id, &[]).unwrap();
        assert!(lines_of(me.connection(), &id).is_empty());
        assert!(get(me.connection(), &id).is_some());
    }

    #[test]
    fn removing_a_statement_removes_the_transactions_entered_against_it() {
        let mut me = personal();
        let id = a_1099b(&mut me);
        record_lines(&mut me, "user", &id, &[line(&id, "a", 100_000)]).unwrap();
        remove(&mut me, "user", &id).unwrap();
        assert!(
            lines_of(me.connection(), &id).is_empty(),
            "rows without their 1099-B would put sales on a Form 8949 the log no longer holds"
        );
    }

    #[test]
    fn a_replay_reproduces_the_transactions_exactly() {
        let mut me = personal();
        let id = a_1099b(&mut me);
        record_lines(
            &mut me,
            "user",
            &id,
            &[line(&id, "a", 100_000), line(&id, "b", 200_000)],
        )
        .unwrap();
        let before = lines_of(me.connection(), &id);

        let events = me.get_all().unwrap();
        crate::store::projections::Projector::new(me.connection())
            .rebuild(&events)
            .unwrap();

        assert_eq!(lines_of(me.connection(), &id), before);
    }

    #[test]
    fn only_a_1099b_takes_transactions() {
        let mut me = personal();
        let id = new_statement_id();
        record(
            &mut me,
            "user",
            &TaxStatement {
                statement_id: id.clone(),
                tax_year: YEAR,
                form: FormKind::F1099Int,
                issuer: "First Bank".to_string(),
                amounts: BTreeMap::from([("1".to_string(), 1_000)]),
                document_ids: Vec::new(),
                source: StatementSource::Entered,
                note: None,
            },
        )
        .unwrap();
        let refused = record_lines(&mut me, "user", &id, &[line(&id, "a", 1)])
            .unwrap_err()
            .to_string();
        assert!(refused.contains("Form 1099-INT has no transactions to list"), "{refused}");

        assert!(matches!(
            record_lines(&mut me, "user", "nope", &[]),
            Err(StatementError::NoSuchStatement(_))
        ));
    }

    /// Form 8949 column (b) takes a date or one of the form's own words, and a row
    /// with neither is a row the form cannot print.
    #[test]
    fn a_stated_acquisition_is_kept_in_capitals_and_a_row_needs_one_answer_or_the_other() {
        let mut me = personal();
        let id = a_1099b(&mut me);
        let mut inherited = line(&id, "a", 100_000);
        inherited.acquired = Acquired::Stated("inherited".to_string());
        record_lines(&mut me, "user", &id, &[inherited]).unwrap();
        assert_eq!(
            lines_of(me.connection(), &id)[0].acquired,
            Acquired::Stated("INHERITED".to_string())
        );
        assert_eq!(
            lines_of(me.connection(), &id)[0].acquired.as_printed(),
            "INHERITED"
        );

        let mut blank = line(&id, "a", 100_000);
        blank.acquired = Acquired::Stated(String::new());
        assert!(record_lines(&mut me, "user", &id, &[blank]).is_err());
    }

    /// An amount in column (g) with no letter in column (f) is an adjustment the
    /// form cannot say the reason for.
    #[test]
    fn an_adjustment_amount_without_its_code_is_refused_and_a_code_alone_is_not() {
        let mut me = personal();
        let id = a_1099b(&mut me);

        let mut amount_only = line(&id, "a", 100_000);
        amount_only.adjustment_cents = 5_000;
        let refused = record_lines(&mut me, "user", &id, &[amount_only])
            .unwrap_err()
            .to_string();
        assert!(refused.contains("column (f)"), "{refused}");

        // Code M with nothing in column (g) is a real row.
        let mut code_only = line(&id, "a", 100_000);
        code_only.adjustment_code = Some("m".to_string());
        record_lines(&mut me, "user", &id, &[code_only]).unwrap();
        assert_eq!(
            lines_of(me.connection(), &id)[0].adjustment_code.as_deref(),
            Some("M"),
            "column (f) is capital letters"
        );
    }

    #[test]
    fn two_rows_cannot_share_an_id_and_a_holding_period_cannot_run_backwards() {
        let mut me = personal();
        let id = a_1099b(&mut me);
        let refused = record_lines(
            &mut me,
            "user",
            &id,
            &[line(&id, "same", 100_000), line(&id, "same", 200_000)],
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("twice"), "{refused}");

        let mut backwards = line(&id, "a", 100_000);
        backwards.acquired = Acquired::On(NaiveDate::from_ymd_opt(YEAR, 12, 31).unwrap());
        let refused = record_lines(&mut me, "user", &id, &[backwards])
            .unwrap_err()
            .to_string();
        assert!(refused.contains("backwards"), "{refused}");
    }
}
