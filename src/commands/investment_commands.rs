//! The taxable-brokerage ledger: a security master, purchase lots, sales with
//! lot selection and a realized gain, investment income, and account fees.
//!
//! INVESTMENTS-SPEC.md phase 1. Nothing here knows about retirement accounts,
//! Plaid, corporate actions, wash sales, options or short sales — those are later
//! phases or deliberately out of scope, and each is called out where it would
//! otherwise be tempting to half-build it.
//!
//! # Why a register beside the ledger, rather than an account per holding
//!
//! Securities are carried **at cost** in one Securities account per brokerage
//! (spec §3, and the open question §10 answers with "one"). So the ledger knows
//! the total cost of everything held and nothing else. A realized gain needs to
//! know *which shares* were sold — what they cost and when they were bought — and
//! a journal entry records neither. This module holds those facts, exactly as
//! `depreciation_commands` holds the inputs MACRS needs while the ledger holds
//! only the result.
//!
//! An account per purchase was the alternative, and it makes the chart of
//! accounts unreadable inside a year.
//!
//! # Why market value is never posted
//!
//! Marking to market needs a valuation account and an unrealized-gain line that
//! reverses itself every period. It churns the books daily and changes no tax
//! outcome. The gap between cost in the books and value at the broker is not an
//! error to be eliminated — it *is* the unrealized gain, and it is worth showing
//! as exactly that, from a holdings snapshot, in a report (spec §3). Nothing here
//! posts it.
//!
//! # Units
//!
//! Money is `i64` cents, as everywhere else in the ledger. Quantity is `i64` in
//! **millionths of a share** — micro-shares, 1e-6. Fractional shares are ordinary
//! now (dividend reinvestment, dollar-based buying), and six places is past every
//! brokerage's own precision, so nothing has to be rounded on the way in. A float
//! cannot represent 0.1, and a holding has to reconcile against a statement.
//!
//! # How the money moves (spec §2a)
//!
//! | Event | Debit | Credit |
//! |---|---|---|
//! | Buy | Securities (cost **including** commission) | Cash |
//! | Sell | Cash (proceeds − fee), the gain account if a loss | Securities (basis of the lots sold), the gain account if a gain |
//! | Dividend / interest | Cash | the income account |
//! | Fee | the fee expense account | Cash |
//!
//! A buy nets to zero across assets, which is the point: it changes the form the
//! money is in and not the amount. The two fee treatments are different on
//! purpose. A commission on a **purchase** capitalises into basis, and a fee on a
//! **sale** reduces proceeds, because that is how a 1099-B reports proceeds and
//! reconciling against that form is the point of all of this (spec §7). A
//! standalone account fee that is not part of a trade posts as an ordinary expense
//! — it appears on no 1099-B, and folding it into proceeds would put it on a form
//! that does not report it.
//!
//! # Every invariant is checked under the write lock
//!
//! Each command validates inside the append transaction, the pattern
//! `account_commands` and `bill_commands` use: a `build_*_in_txn` returns either
//! the events to append or a domain rejection, and
//! [`EventStore::append_checked_many`] runs it with the head compare and the
//! inserts in one transaction. This is not ceremony. "Does this lot have 5 shares
//! left" read before the transaction is a fact that a concurrent sale can
//! falsify, and the failure it produces is two sales of the same shares, each
//! claiming the same basis — a Form 8949 that deducts one purchase twice.
//!
//! The register event and its journal entry are appended as **one batch** for the
//! same reason `apply_payment` does: a lot whose purchase entry never landed, or
//! an entry relieving basis no lot gave up, is worse than either failing.

use chrono::{Months, NaiveDate};
use rusqlite::{params, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::commands::entry_commands::{check_entry_invariants_in_txn, EntryCommandError};
use crate::events::types::{
    Event, EventEnvelope, HoldingTerm, InvestmentIncomeKind, JournalEntrySource, JournalLineData,
    SaleLotData, SecurityDefinedData, SecuritySoldData, StoredEvent,
};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;

/// One share, in the units a quantity is stored in.
///
/// Public because a caller converting from a broker's decimal quantity needs it,
/// and a private constant would have every caller writing `1_000_000` from
/// memory.
pub const MICRO_SHARE: i64 = 1_000_000;

#[derive(Debug, Error)]
pub enum InvestmentError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("Could not post the entry: {0}")]
    Entry(#[from] EntryCommandError),
    #[error("No security with id {0}")]
    NoSuchSecurity(String),
    #[error("No account with id {0}")]
    NoSuchAccount(String),
    #[error(
        "{ticker} is already on the security master as {security_id}. One ticker is one security \
         in one book — two masters for it would split one holding into two that neither add up \
         on the balance sheet nor reconcile against the 1099-B."
    )]
    TickerTaken { ticker: String, security_id: String },
    #[error(
        "There are no lots of {security_id} in account {securities_account_id}, so there is \
         nothing to sell and no basis to sell it against"
    )]
    NoLots {
        security_id: String,
        securities_account_id: String,
    },
    #[error(
        "That sells {requested} shares of a holding of {held}. A sale cannot be larger than the \
         position; the shares are either in another account or were never bought."
    )]
    NotEnoughShares { requested: String, held: String },
    #[error("No lot with id {0} in this account")]
    NoSuchLot(String),
    #[error("Lot {lot_id} is a lot of {lot_security}, not of {sale_security}")]
    LotWrongSecurity {
        lot_id: String,
        lot_security: String,
        sale_security: String,
    },
    #[error("Lot {lot_id} sits in account {lot_account}, and the sale is out of {sale_account}")]
    LotWrongAccount {
        lot_id: String,
        lot_account: String,
        sale_account: String,
    },
    #[error("Lot {lot_id} has {remaining} shares left and the selection takes {requested}")]
    LotOverdrawn {
        lot_id: String,
        remaining: String,
        requested: String,
    },
    #[error("Lot {0} is named twice in the same selection")]
    LotNamedTwice(String),
    #[error(
        "The chosen lots come to {selected} shares and the sale is of {sale}. A specific-lot sale \
         has to say where every share came from."
    )]
    SelectionDoesNotSum { selected: String, sale: String },
    #[error("Invalid data: {0}")]
    Invalid(String),
}

impl From<EventStoreError> for InvestmentError {
    fn from(e: EventStoreError) -> Self {
        InvestmentError::Store(e.to_string())
    }
}

/// Micro-shares as a person reads them, for error messages: six places with
/// trailing zeroes trimmed, so a whole number of shares does not print as
/// "10.000000".
fn shares(micro: i64) -> String {
    let mut s = format!("{:.6}", micro as f64 / MICRO_SHARE as f64);
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

// ---------------------------------------------------------------------------
// Reading the register
// ---------------------------------------------------------------------------

/// A security on the master.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Security {
    pub security_id: String,
    pub ticker: String,
    pub name: String,
    pub kind: String,
    pub cusip: Option<String>,
    pub currency: String,
}

/// One purchase lot, with what is left of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lot {
    pub lot_id: String,
    pub security_id: String,
    pub securities_account_id: String,
    pub cash_account_id: String,
    /// Micro-shares bought.
    pub quantity: i64,
    /// The whole cost, commission included.
    pub total_cost_cents: i64,
    /// Micro-shares not yet sold.
    pub remaining_quantity: i64,
    /// The part of `total_cost_cents` not yet relieved by a sale. Carried rather
    /// than recomputed — see [`allocate_basis`].
    pub remaining_basis_cents: i64,
    pub trade_date: NaiveDate,
}

impl Lot {
    /// Whether the sale is long-term against this lot.
    ///
    /// **More than one year**, not one year: §1222 counts the day of acquisition
    /// out and the day of sale in, so a share bought on 1 January is long-term
    /// when sold on 2 January of the next year and short-term when sold on the
    /// 1st. "One year plus a day" is the whole rule, and it is worth 20 points of
    /// tax rate on the sale, so it is computed here rather than left to a caller's
    /// arithmetic.
    ///
    /// Computed per lot, which is why one sale can produce both terms.
    pub fn term_on(&self, sale_date: NaiveDate) -> HoldingTerm {
        // A date chrono cannot add a year to does not exist; if it ever did, the
        // safe answer is the one that taxes more.
        match self.trade_date.checked_add_months(Months::new(12)) {
            Some(one_year) if sale_date > one_year => HoldingTerm::Long,
            _ => HoldingTerm::Short,
        }
    }
}

/// Every security on the master, by ticker.
pub fn list_securities(conn: &rusqlite::Connection) -> Vec<Security> {
    let Ok(mut stmt) = conn
        .prepare("SELECT id, ticker, name, kind, cusip, currency FROM securities ORDER BY ticker")
    else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(Security {
            security_id: r.get(0)?,
            ticker: r.get(1)?,
            name: r.get(2)?,
            kind: r.get(3)?,
            cusip: r.get(4)?,
            currency: r.get(5)?,
        })
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

pub fn get_security(conn: &rusqlite::Connection, security_id: &str) -> Option<Security> {
    list_securities(conn)
        .into_iter()
        .find(|s| s.security_id == security_id)
}

/// The lots of one security in one account, **oldest first** — the order FIFO
/// consumes them in, and the order a holdings report reads best.
///
/// Fully consumed lots are included. They are the basis history: a Form 8949 row
/// for a lot sold in March still has to name the date it was acquired, and a lot
/// deleted when it emptied could not answer that.
pub fn lots_of(
    conn: &rusqlite::Connection,
    security_id: &str,
    securities_account_id: &str,
) -> Vec<Lot> {
    read_lots(
        conn,
        "SELECT id, security_id, securities_account_id, cash_account_id, quantity,
                total_cost_cents, remaining_quantity, remaining_basis_cents, trade_date
           FROM investment_lots
          WHERE security_id = ?1 AND securities_account_id = ?2
          ORDER BY trade_date, rowid",
        params![security_id, securities_account_id],
    )
}

/// The lots of one security in one account that still have shares in them, oldest
/// first.
///
/// What a lot picker offers. A fully consumed lot is deliberately excluded: it can
/// no longer contribute to a sale, and offering it would be offering a choice that
/// is refused under the write lock — while [`lots_of`] keeps them, because a Form
/// 8949 row for a lot sold in March still has to name the date it was acquired.
pub fn open_lots(
    conn: &rusqlite::Connection,
    security_id: &str,
    securities_account_id: &str,
) -> Vec<Lot> {
    lots_of(conn, security_id, securities_account_id)
        .into_iter()
        .filter(|l| l.remaining_quantity > 0)
        .collect()
}

/// Every lot in the register, oldest first.
pub fn list_lots(conn: &rusqlite::Connection) -> Vec<Lot> {
    read_lots(
        conn,
        "SELECT id, security_id, securities_account_id, cash_account_id, quantity,
                total_cost_cents, remaining_quantity, remaining_basis_cents, trade_date
           FROM investment_lots
          ORDER BY trade_date, rowid",
        params![],
    )
}

fn read_lots(conn: &rusqlite::Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Vec<Lot> {
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(args, |r| {
        let date: String = r.get(8)?;
        Ok((
            Lot {
                lot_id: r.get(0)?,
                security_id: r.get(1)?,
                securities_account_id: r.get(2)?,
                cash_account_id: r.get(3)?,
                quantity: r.get(4)?,
                total_cost_cents: r.get(5)?,
                remaining_quantity: r.get(6)?,
                remaining_basis_cents: r.get(7)?,
                trade_date: NaiveDate::default(),
            },
            date,
        ))
    });
    let Ok(rows) = rows else { return Vec::new() };
    // A row whose date this crate cannot parse is skipped rather than failing the
    // read — the choice `list_assets` makes, for the same reason: a register that
    // will not open is worse than one missing a row somebody can see is missing.
    rows.flatten()
        .filter_map(|(mut lot, date)| {
            lot.trade_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?;
            Some(lot)
        })
        .collect()
}

/// What is held of one security in one account: micro-shares and the cost they
/// are carried at.
///
/// This is the figure §7's reconciliation compares against a holdings snapshot,
/// and it is a sum over the lots rather than the Securities account balance —
/// that account holds every security together.
pub fn holding_of(
    conn: &rusqlite::Connection,
    security_id: &str,
    securities_account_id: &str,
) -> (i64, i64) {
    lots_of(conn, security_id, securities_account_id)
        .iter()
        .fold((0, 0), |(q, c), l| {
            (q + l.remaining_quantity, c + l.remaining_basis_cents)
        })
}

/// One lot a sale consumed, as a report reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumedLot {
    pub lot_id: String,
    pub quantity: i64,
    pub basis_cents: i64,
    pub term: HoldingTerm,
    /// The lot's purchase date — Form 8949's "date acquired". Joined from the lot
    /// rather than copied onto the sale row, because a lot's purchase date is one
    /// fact and copying it is how two answers to it appear.
    pub acquired_on: Option<NaiveDate>,
}

/// Which lots a sale consumed, in the order it consumed them.
pub fn consumed_lots(conn: &rusqlite::Connection, sale_id: &str) -> Vec<ConsumedLot> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT sl.lot_id, sl.quantity, sl.basis_cents, sl.term, l.trade_date
           FROM investment_sale_lots sl
           LEFT JOIN investment_lots l ON l.id = sl.lot_id
          WHERE sl.sale_id = ?1
          ORDER BY l.trade_date, sl.lot_id",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([sale_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    });
    let Ok(rows) = rows else { return Vec::new() };
    rows.flatten()
        .filter_map(|(lot_id, quantity, basis_cents, term, acquired)| {
            Some(ConsumedLot {
                lot_id,
                quantity,
                basis_cents,
                term: HoldingTerm::parse(&term)?,
                acquired_on: acquired.and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A security to put on the master. `security_id` is minted by the command.
#[derive(Debug, Clone)]
pub struct NewSecurity {
    pub ticker: String,
    pub name: String,
    /// "stock", "etf", "mutual fund"… free text; see [`SecurityDefinedData`].
    pub kind: String,
    pub cusip: Option<String>,
    /// "USD" unless something says otherwise. Multi-currency is out of scope
    /// (spec §10); the field is here so adding it later is not a migration of
    /// every lot.
    pub currency: String,
}

#[derive(Debug, Clone)]
pub struct BuySecurityCommand {
    pub security_id: String,
    pub securities_account_id: String,
    pub cash_account_id: String,
    /// Micro-shares.
    pub quantity: i64,
    /// The whole cost, **including** any commission — which capitalises into
    /// basis. There is no separate fee field on a purchase on purpose: a caller
    /// who passed the two apart would sooner or later pass the cost without them.
    pub total_cost_cents: i64,
    pub trade_date: NaiveDate,
    pub memo: Option<String>,
}

/// Which shares a sale sells.
///
/// FIFO by default (spec §4), which is also what the IRS assumes when nothing
/// else is specified. `Specific` is the override offered at the point of sale, and
/// it is the reason the consumed lots are recorded on the event: the choice made
/// on the day is the choice that was filed, and it must not be recomputed later
/// from whatever the default has become.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LotSelection {
    #[default]
    Fifo,
    /// `(lot_id, micro-shares)`, in the order they should be consumed.
    Specific(Vec<(String, i64)>),
}

#[derive(Debug, Clone)]
pub struct SellSecurityCommand {
    pub security_id: String,
    pub securities_account_id: String,
    pub cash_account_id: String,
    /// `Income:Investments:Realized gain` — one account for both directions, as
    /// spec §2a's chart has it: a loss is a debit to it, a gain a credit. Two
    /// accounts would make net gain a subtraction somebody has to remember to do.
    pub realized_gain_account_id: String,
    /// Micro-shares.
    pub quantity: i64,
    /// Gross, as a 1099-B reports it.
    pub proceeds_cents: i64,
    /// Taken out of the proceeds, not expensed. See the module docs.
    pub fee_cents: i64,
    pub trade_date: NaiveDate,
    pub selection: LotSelection,
    pub memo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RecordInvestmentIncomeCommand {
    pub kind: InvestmentIncomeKind,
    /// `None` for sweep interest, which belongs to the account and not to any
    /// holding.
    pub security_id: Option<String>,
    pub cash_account_id: String,
    /// `Income:Investments:Dividends` or `:Interest`. Named by the caller rather
    /// than derived from `kind`, because this module does not own the chart of
    /// accounts and guessing an account id from an enum is how a posting lands
    /// somewhere nobody chose.
    pub income_account_id: String,
    pub amount_cents: i64,
    pub received_on: NaiveDate,
    pub memo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChargeInvestmentFeeCommand {
    pub cash_account_id: String,
    pub expense_account_id: String,
    pub amount_cents: i64,
    pub charged_on: NaiveDate,
    pub security_id: Option<String>,
    pub memo: Option<String>,
}

/// What a purchase came to.
#[derive(Debug, Clone)]
pub struct Bought {
    pub lot_id: String,
    pub entry_id: String,
}

/// What a sale came to — everything a Form 8949 row and a 1099-B check need.
#[derive(Debug, Clone)]
pub struct Sold {
    pub sale_id: String,
    pub entry_id: String,
    pub proceeds_cents: i64,
    pub fee_cents: i64,
    /// The basis of the lots consumed, which is exactly what was credited to the
    /// Securities account.
    pub basis_cents: i64,
    /// `(proceeds − fee) − basis`. Negative is a loss.
    pub realized_gain_cents: i64,
    pub lots: Vec<SaleLotData>,
}

/// The outcome of an investment command's in-transaction validation: the events
/// to append as one unit, or a domain rejection. The shape
/// `AccountBatchStep`/`BillStep` have, and for the stated reason — bare [`Event`]s
/// rather than envelopes, because the local path stamps the operator and a future
/// server path would stamp the authenticated actor, and shared code must not pick
/// one.
///
/// `pub(crate)` along with the `build_*_in_txn` functions below, so that phase 4's
/// importer (`investment_import`) appends a trade, its journal entry **and** its
/// import record as one batch. That is not a convenience: an imported purchase
/// whose import record did not land is re-imported on the next rolling fetch, with
/// a freshly minted lot id that sails past the entry-reference fence, and the same
/// purchase is then deducted twice on a Form 8949. Re-running these builders from
/// the importer also means the importer cannot grow a second opinion about FIFO,
/// about whether a lot can be consumed twice, or about a closed year.
pub(crate) enum InvestmentStep {
    Append(Vec<Event>),
    Reject(InvestmentError),
}

/// The idempotency key a purchase entry carries.
///
/// One per lot, which also makes the register row and its journal entry findable
/// from each other without a foreign key either way — the link §7's
/// reconciliation walks. Migration 014's partial unique index is what actually
/// enforces it, rather than a check this module remembers to make.
pub fn buy_reference(lot_id: &str) -> String {
    format!("securities-buy-{lot_id}")
}

/// The same, for a sale.
pub fn sale_reference(sale_id: &str) -> String {
    format!("securities-sell-{sale_id}")
}

/// Put a security on the master. Returns its new id.
///
/// The ticker's uniqueness is checked **inside** the append transaction, so two
/// concurrent defines cannot both pass it and leave one holding split across two
/// masters.
pub fn define_security(
    store: &mut EventStore,
    user_id: &str,
    security: &NewSecurity,
) -> Result<(String, StoredEvent), InvestmentError> {
    let security_id = Uuid::new_v4().to_string();
    let stored = run(store, user_id, |tx| {
        build_define_security_in_txn(tx, &security_id, security)
    })?;
    Ok((
        security_id,
        stored.into_iter().next_back().expect("one event appended"),
    ))
}

/// Buy shares: one lot on the register, one entry debiting Securities and
/// crediting Cash for the whole cost.
pub fn buy_security(
    store: &mut EventStore,
    user_id: &str,
    cmd: &BuySecurityCommand,
) -> Result<Bought, InvestmentError> {
    let lot_id = Uuid::new_v4().to_string();
    let events = run(store, user_id, |tx| build_buy_in_txn(tx, &lot_id, cmd))?;
    Ok(Bought {
        lot_id,
        entry_id: entry_id_of(&events)?,
    })
}

/// Sell shares: relieve the basis of the lots the selection names, post the net
/// proceeds and the realized gain, and record which lots went and on what terms.
pub fn sell_security(
    store: &mut EventStore,
    user_id: &str,
    cmd: &SellSecurityCommand,
) -> Result<Sold, InvestmentError> {
    let sale_id = Uuid::new_v4().to_string();
    let events = run(store, user_id, |tx| build_sell_in_txn(tx, &sale_id, cmd))?;
    let entry_id = entry_id_of(&events)?;
    let sale = events
        .iter()
        .find_map(|e| match &e.event {
            Event::SecuritySold(d) => Some(d.clone()),
            _ => None,
        })
        .ok_or_else(|| InvestmentError::Store("the sale event did not land".to_string()))?;
    Ok(Sold {
        sale_id,
        entry_id,
        proceeds_cents: sale.proceeds_cents,
        fee_cents: sale.fee_cents,
        basis_cents: sale.lots.iter().map(|l| l.basis_cents).sum(),
        realized_gain_cents: sale.realized_gain_cents,
        lots: sale.lots.clone(),
    })
}

/// A dividend or interest payment: debit Cash, credit the income account.
/// Returns the journal entry's id.
pub fn record_income(
    store: &mut EventStore,
    user_id: &str,
    cmd: &RecordInvestmentIncomeCommand,
) -> Result<String, InvestmentError> {
    let events = run(store, user_id, |tx| build_income_in_txn(tx, cmd))?;
    entry_id_of(&events)
}

/// An account fee that is not part of a trade: debit the expense account, credit
/// Cash. Returns the journal entry's id.
pub fn charge_fee(
    store: &mut EventStore,
    user_id: &str,
    cmd: &ChargeInvestmentFeeCommand,
) -> Result<String, InvestmentError> {
    let events = run(store, user_id, |tx| build_fee_in_txn(tx, cmd))?;
    entry_id_of(&events)
}

/// The append-and-retry loop every command above shares.
///
/// One copy, because the difference between the commands is entirely in the
/// `build_*_in_txn` they run: the head compare, the stamping, the projection and
/// the retry on a head move are identical, and five copies of them is five places
/// for the retry to be forgotten.
fn run(
    store: &mut EventStore,
    user_id: &str,
    build: impl Fn(&rusqlite::Transaction<'_>) -> Result<InvestmentStep, EventStoreError>,
) -> Result<Vec<StoredEvent>, InvestmentError> {
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let outcome = store.append_checked_many(
            head,
            |tx| match build(tx)? {
                InvestmentStep::Append(events) => Ok(Verdict::Append(
                    events
                        .into_iter()
                        .map(|e| EventEnvelope::new(e, user_id.to_string()))
                        .collect(),
                )),
                InvestmentStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;
        match outcome {
            CheckedOutcome::Appended(events) => return Ok(events),
            // The log moved before the write lock; nothing was appended and the
            // checks did not run. Rebuild the whole command against fresh state —
            // which matters here more than most places, because a concurrent sale
            // changes which lots FIFO would pick.
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

/// The id of the `JournalEntryPosted` in a batch — read off the event rather than
/// minted by the caller, so the id reported is the one the ledger holds.
fn entry_id_of(events: &[StoredEvent]) -> Result<String, InvestmentError> {
    events
        .iter()
        .find_map(|e| match &e.event {
            Event::JournalEntryPosted { entry_id, .. } => Some(entry_id.clone()),
            _ => None,
        })
        .ok_or_else(|| InvestmentError::Store("no journal entry was posted".to_string()))
}

// ---------------------------------------------------------------------------
// Validation and event building, all of it inside the append transaction
// ---------------------------------------------------------------------------

pub(crate) fn build_define_security_in_txn(
    tx: &rusqlite::Transaction<'_>,
    security_id: &str,
    security: &NewSecurity,
) -> Result<InvestmentStep, EventStoreError> {
    let ticker = security.ticker.trim().to_uppercase();
    if ticker.is_empty() || security.name.trim().is_empty() || security.kind.trim().is_empty() {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "a security needs a ticker, a name and a kind".to_string(),
        )));
    }
    // Under the write lock, not before it: two concurrent defines of AAPL would
    // otherwise both pass and split the holding.
    if let Some(existing) = tx
        .query_row(
            "SELECT id FROM securities WHERE ticker = ?1",
            [&ticker],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(InvestmentStep::Reject(InvestmentError::TickerTaken {
            ticker,
            security_id: existing,
        }));
    }
    Ok(InvestmentStep::Append(vec![Event::SecurityDefined(
        Box::new(SecurityDefinedData {
            security_id: security_id.to_string(),
            ticker,
            name: security.name.trim().to_string(),
            kind: security.kind.trim().to_string(),
            cusip: security
                .cusip
                .as_ref()
                .map(|c| c.trim().to_string())
                .filter(|c| !c.is_empty()),
            currency: if security.currency.trim().is_empty() {
                "USD".to_string()
            } else {
                security.currency.trim().to_uppercase()
            },
        }),
    )]))
}

pub(crate) fn build_buy_in_txn(
    tx: &rusqlite::Transaction<'_>,
    lot_id: &str,
    cmd: &BuySecurityCommand,
) -> Result<InvestmentStep, EventStoreError> {
    if cmd.quantity <= 0 {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "a purchase of no shares is not a purchase".to_string(),
        )));
    }
    if cmd.total_cost_cents <= 0 {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "a lot has to cost something — a free lot comes from a corporate action, and those \
             are out of scope for now"
                .to_string(),
        )));
    }
    if !security_exists(tx, &cmd.security_id)? {
        return Ok(InvestmentStep::Reject(InvestmentError::NoSuchSecurity(
            cmd.security_id.clone(),
        )));
    }

    let currency = base_currency(tx)?;
    let memo = cmd.memo.clone().unwrap_or_else(|| {
        format!(
            "Bought {} of {}",
            shares(cmd.quantity),
            ticker_or_id(tx, &cmd.security_id)
        )
    });
    // A buy nets to zero across assets: the money changes form, not amount. The
    // debit is the whole cost including commission, which is what makes the lot's
    // basis and the account's balance the same number.
    let lines = vec![
        (
            cmd.securities_account_id.clone(),
            cmd.total_cost_cents,
            "Securities at cost",
        ),
        (cmd.cash_account_id.clone(), -cmd.total_cost_cents, "Cash"),
    ];
    let entry = match entry_or_reject(
        tx,
        cmd.trade_date,
        memo,
        Some(buy_reference(lot_id)),
        &lines,
        &currency,
    )? {
        Ok(entry) => entry,
        Err(e) => return Ok(InvestmentStep::Reject(e)),
    };

    Ok(InvestmentStep::Append(vec![
        entry,
        Event::SecurityBought {
            lot_id: lot_id.to_string(),
            security_id: cmd.security_id.clone(),
            securities_account_id: cmd.securities_account_id.clone(),
            cash_account_id: cmd.cash_account_id.clone(),
            quantity: cmd.quantity,
            total_cost_cents: cmd.total_cost_cents,
            trade_date: cmd.trade_date,
        },
    ]))
}

/// One lot's contribution to a sale, chosen but not yet priced.
struct Pick {
    lot: Lot,
    quantity: i64,
}

/// Which lots to consume, and how much of each — FIFO or the caller's list.
///
/// Reads under the write lock and refuses rather than clamps. A sale larger than
/// the position is a data error every time: the shares are in another account, or
/// the purchase was never entered. Clamping it would post a gain against a basis
/// nobody chose and reconcile against nothing.
fn pick_lots(
    tx: &rusqlite::Transaction<'_>,
    cmd: &SellSecurityCommand,
) -> Result<Result<Vec<Pick>, InvestmentError>, EventStoreError> {
    let lots = lots_of(tx, &cmd.security_id, &cmd.securities_account_id);
    if lots.is_empty() {
        return Ok(Err(InvestmentError::NoLots {
            security_id: cmd.security_id.clone(),
            securities_account_id: cmd.securities_account_id.clone(),
        }));
    }
    // A position of nothing is `NotEnoughShares` and not `NoLots`: the lots are
    // there, they are simply spent, and telling somebody their lots do not exist
    // when the register can show them is how a real bug gets looked for in the
    // wrong place.
    let held: i64 = lots.iter().map(|l| l.remaining_quantity).sum();
    if cmd.quantity > held {
        return Ok(Err(InvestmentError::NotEnoughShares {
            requested: shares(cmd.quantity),
            held: shares(held),
        }));
    }

    match &cmd.selection {
        // Oldest first, which is both the spec's default and what the IRS assumes
        // when a seller specifies nothing.
        LotSelection::Fifo => {
            let mut left = cmd.quantity;
            let mut picks = Vec::new();
            for lot in lots {
                if left == 0 {
                    break;
                }
                if lot.remaining_quantity <= 0 {
                    continue;
                }
                let take = left.min(lot.remaining_quantity);
                left -= take;
                picks.push(Pick {
                    lot,
                    quantity: take,
                });
            }
            // `held` already covers the total, so this cannot fail; it is asserted
            // rather than assumed because the alternative is a silent short sale.
            if left != 0 {
                return Ok(Err(InvestmentError::NotEnoughShares {
                    requested: shares(cmd.quantity),
                    held: shares(held),
                }));
            }
            Ok(Ok(picks))
        }
        LotSelection::Specific(chosen) => {
            let mut picks = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let mut total = 0i64;
            for (lot_id, quantity) in chosen {
                if !seen.insert(lot_id.clone()) {
                    return Ok(Err(InvestmentError::LotNamedTwice(lot_id.clone())));
                }
                if *quantity <= 0 {
                    return Ok(Err(InvestmentError::Invalid(format!(
                        "lot {lot_id} is named in the selection with no shares against it"
                    ))));
                }
                // Looked up across the whole register, not only within this
                // security and account, so naming a lot of the wrong security says
                // so instead of saying the lot does not exist.
                let Some(lot) = lookup_lot(tx, lot_id)? else {
                    return Ok(Err(InvestmentError::NoSuchLot(lot_id.clone())));
                };
                if lot.security_id != cmd.security_id {
                    return Ok(Err(InvestmentError::LotWrongSecurity {
                        lot_id: lot_id.clone(),
                        lot_security: lot.security_id,
                        sale_security: cmd.security_id.clone(),
                    }));
                }
                if lot.securities_account_id != cmd.securities_account_id {
                    return Ok(Err(InvestmentError::LotWrongAccount {
                        lot_id: lot_id.clone(),
                        lot_account: lot.securities_account_id,
                        sale_account: cmd.securities_account_id.clone(),
                    }));
                }
                // The remaining quantity is the fence that stops a lot being
                // consumed twice — there is no separate "closed" flag to fall out
                // of step with it.
                if *quantity > lot.remaining_quantity {
                    return Ok(Err(InvestmentError::LotOverdrawn {
                        lot_id: lot_id.clone(),
                        remaining: shares(lot.remaining_quantity),
                        requested: shares(*quantity),
                    }));
                }
                total += quantity;
                picks.push(Pick {
                    lot,
                    quantity: *quantity,
                });
            }
            if total != cmd.quantity {
                return Ok(Err(InvestmentError::SelectionDoesNotSum {
                    selected: shares(total),
                    sale: shares(cmd.quantity),
                }));
            }
            Ok(Ok(picks))
        }
    }
}

/// The part of a lot's remaining cost that goes with `quantity` shares of it.
///
/// `remaining_basis * quantity / remaining_quantity`, floored, in integer
/// arithmetic — **except** when the sale takes the whole remainder, where it is
/// the whole remainder. That exception is the entire point:
///
/// - A lot sold in pieces has each piece's cost floored, so the pieces would sum
///   to less than the lot cost by up to a cent per piece. Giving the closing sale
///   everything that is left puts those cents back exactly where they belong,
///   and no cent is ever lost or invented.
/// - Because what remains is carried on the lot rather than recomputed from the
///   original cost, repeated partial sales compose: three sales of a $10.00
///   three-share lot allocate 333, 333 and 334 cents, and 333 + 333 + 334 is
///   1000.
///
/// `i128` for the product: a large holding of a cheap security multiplies a cents
/// figure by a micro-share figure, and while the result fits in `i64` for any
/// plausible position, an overflow here would silently restate a basis rather than
/// fail.
fn allocate_basis(remaining_basis_cents: i64, remaining_quantity: i64, quantity: i64) -> i64 {
    if quantity >= remaining_quantity {
        return remaining_basis_cents;
    }
    let product = remaining_basis_cents as i128 * quantity as i128;
    (product / remaining_quantity as i128) as i64
}

pub(crate) fn build_sell_in_txn(
    tx: &rusqlite::Transaction<'_>,
    sale_id: &str,
    cmd: &SellSecurityCommand,
) -> Result<InvestmentStep, EventStoreError> {
    if cmd.quantity <= 0 {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "a sale of no shares is not a sale".to_string(),
        )));
    }
    if cmd.proceeds_cents < 0 || cmd.fee_cents < 0 {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "proceeds and fees are amounts, not directions".to_string(),
        )));
    }
    if cmd.fee_cents > cmd.proceeds_cents {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "a fee larger than the proceeds would make the sale pay out negative cash, which is \
             a margin call and not a sale"
                .to_string(),
        )));
    }
    if !security_exists(tx, &cmd.security_id)? {
        return Ok(InvestmentStep::Reject(InvestmentError::NoSuchSecurity(
            cmd.security_id.clone(),
        )));
    }

    let picks = match pick_lots(tx, cmd)? {
        Ok(picks) => picks,
        Err(e) => return Ok(InvestmentStep::Reject(e)),
    };

    // Price each pick against what is left of its lot, in the order the selection
    // gave them. The term is per lot, which is how one sale produces both.
    let mut lots = Vec::with_capacity(picks.len());
    let mut basis_cents = 0i64;
    for pick in &picks {
        let basis = allocate_basis(
            pick.lot.remaining_basis_cents,
            pick.lot.remaining_quantity,
            pick.quantity,
        );
        basis_cents += basis;
        lots.push(SaleLotData {
            lot_id: pick.lot.lot_id.clone(),
            quantity: pick.quantity,
            basis_cents: basis,
            term: pick.lot.term_on(cmd.trade_date),
        });
    }

    let net_proceeds = cmd.proceeds_cents - cmd.fee_cents;
    let realized_gain_cents = net_proceeds - basis_cents;

    // The entry, per spec §2a. The Securities credit is the basis just allocated
    // and nothing else, so what leaves the account is exactly what the lots gave
    // up — the invariant the whole allocation exists to keep.
    let currency = base_currency(tx)?;
    let memo = cmd.memo.clone().unwrap_or_else(|| {
        format!(
            "Sold {} of {}",
            shares(cmd.quantity),
            ticker_or_id(tx, &cmd.security_id)
        )
    });
    let mut lines: Vec<(String, i64, &str)> = Vec::with_capacity(3);
    if net_proceeds != 0 {
        lines.push((cmd.cash_account_id.clone(), net_proceeds, "Net proceeds"));
    }
    if basis_cents != 0 {
        lines.push((
            cmd.securities_account_id.clone(),
            -basis_cents,
            "Basis of the lots sold",
        ));
    }
    // One account for both directions, as the chart has it: a gain is a credit to
    // it and a loss a debit.
    if realized_gain_cents != 0 {
        lines.push((
            cmd.realized_gain_account_id.clone(),
            -realized_gain_cents,
            if realized_gain_cents < 0 {
                "Realized loss"
            } else {
                "Realized gain"
            },
        ));
    }
    let entry = match entry_or_reject(
        tx,
        cmd.trade_date,
        memo,
        Some(sale_reference(sale_id)),
        &lines,
        &currency,
    )? {
        Ok(entry) => entry,
        Err(e) => return Ok(InvestmentStep::Reject(e)),
    };

    Ok(InvestmentStep::Append(vec![
        entry,
        Event::SecuritySold(Box::new(SecuritySoldData {
            sale_id: sale_id.to_string(),
            security_id: cmd.security_id.clone(),
            securities_account_id: cmd.securities_account_id.clone(),
            cash_account_id: cmd.cash_account_id.clone(),
            quantity: cmd.quantity,
            proceeds_cents: cmd.proceeds_cents,
            fee_cents: cmd.fee_cents,
            trade_date: cmd.trade_date,
            lots,
            realized_gain_cents,
        })),
    ]))
}

pub(crate) fn build_income_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &RecordInvestmentIncomeCommand,
) -> Result<InvestmentStep, EventStoreError> {
    if cmd.amount_cents <= 0 {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "income of nothing is not income".to_string(),
        )));
    }
    if let Some(security_id) = &cmd.security_id {
        if !security_exists(tx, security_id)? {
            return Ok(InvestmentStep::Reject(InvestmentError::NoSuchSecurity(
                security_id.clone(),
            )));
        }
    }

    let currency = base_currency(tx)?;
    // One label per kind, from the enum itself rather than from a match here: four
    // kinds since phase 5, and a fifth copy of the list is a fifth place for one of
    // them to be called something else.
    let label = cmd.kind.label();
    let memo = cmd.memo.clone().unwrap_or_else(|| match &cmd.security_id {
        Some(id) => format!("{label} from {}", ticker_or_id(tx, id)),
        None => format!("{label} on the brokerage account"),
    });
    let lines = vec![
        (cmd.cash_account_id.clone(), cmd.amount_cents, label),
        (cmd.income_account_id.clone(), -cmd.amount_cents, label),
    ];
    // No reference: a dividend has no natural idempotency key in phase 1. Phase 4
    // imports one (Plaid's `investment_transaction_id`) and can supply it then;
    // inventing one from the amount and the date would collide two real payments
    // of the same size on the same day, which is exactly what a quarterly
    // dividend across two holdings looks like.
    let entry = match entry_or_reject(tx, cmd.received_on, memo, None, &lines, &currency)? {
        Ok(entry) => entry,
        Err(e) => return Ok(InvestmentStep::Reject(e)),
    };

    Ok(InvestmentStep::Append(vec![
        entry,
        Event::InvestmentIncomeReceived {
            kind: cmd.kind,
            security_id: cmd.security_id.clone(),
            cash_account_id: cmd.cash_account_id.clone(),
            amount_cents: cmd.amount_cents,
            received_on: cmd.received_on,
        },
    ]))
}

pub(crate) fn build_fee_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ChargeInvestmentFeeCommand,
) -> Result<InvestmentStep, EventStoreError> {
    if cmd.amount_cents <= 0 {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            "a fee of nothing is not a fee".to_string(),
        )));
    }
    if let Some(security_id) = &cmd.security_id {
        if !security_exists(tx, security_id)? {
            return Ok(InvestmentStep::Reject(InvestmentError::NoSuchSecurity(
                security_id.clone(),
            )));
        }
    }

    let currency = base_currency(tx)?;
    let memo = cmd
        .memo
        .clone()
        .unwrap_or_else(|| "Investment account fee".to_string());
    // An expense, not a reduction of proceeds: this fee is on no 1099-B, and
    // netting it against a sale would put it on a form that does not report it.
    let lines = vec![
        (cmd.expense_account_id.clone(), cmd.amount_cents, "Fee"),
        (cmd.cash_account_id.clone(), -cmd.amount_cents, "Cash"),
    ];
    let entry = match entry_or_reject(tx, cmd.charged_on, memo, None, &lines, &currency)? {
        Ok(entry) => entry,
        Err(e) => return Ok(InvestmentStep::Reject(e)),
    };

    Ok(InvestmentStep::Append(vec![
        entry,
        Event::InvestmentFeeCharged {
            cash_account_id: cmd.cash_account_id.clone(),
            expense_account_id: cmd.expense_account_id.clone(),
            amount_cents: cmd.amount_cents,
            charged_on: cmd.charged_on,
            security_id: cmd.security_id.clone(),
        },
    ]))
}

// ---------------------------------------------------------------------------
// Shared in-transaction helpers
// ---------------------------------------------------------------------------

/// Build a `JournalEntryPosted` after re-running the ledger's own fences inside
/// this transaction: the reference is free, every account exists and is active,
/// and the date is not in a closed year.
///
/// The balance check is here too, and it is not belt-and-braces: every caller
/// above computes its credit side from figures it derived, and an entry that does
/// not balance means one of those derivations is wrong. Refusing it is how that
/// never reaches the books.
///
/// `pub(crate)` so `retirement_commands` (phase 2) uses this very function rather
/// than its own copy. There is exactly one set of fences an investment posting has
/// to clear, and a second implementation of them is a second place for the
/// closed-year check to be left out.
pub(crate) fn entry_or_reject(
    tx: &rusqlite::Transaction<'_>,
    date: NaiveDate,
    memo: String,
    reference: Option<String>,
    lines: &[(String, i64, &str)],
    currency: &str,
) -> Result<Result<Event, InvestmentError>, EventStoreError> {
    if lines.len() < 2 {
        return Ok(Err(InvestmentError::Invalid(
            "a posting needs at least two lines".to_string(),
        )));
    }
    let sum = lines.iter().try_fold(0i64, |acc, l| acc.checked_add(l.1));
    match sum {
        Some(0) => {}
        Some(sum) => {
            return Ok(Err(InvestmentError::Invalid(format!(
                "the entry does not balance: the lines sum to {sum} rather than zero"
            ))))
        }
        None => {
            return Ok(Err(InvestmentError::Invalid(
                "the line amounts overflow".to_string(),
            )))
        }
    }

    if let Some(reference) = &reference {
        if let Some(existing) =
            crate::commands::entry_commands::check_reference_free_in_txn(tx, reference)?
        {
            return Ok(Err(InvestmentError::Entry(
                EntryCommandError::DuplicateReference {
                    reference: reference.clone(),
                    existing_entry_id: existing,
                },
            )));
        }
    }

    let account_ids: Vec<&str> = lines.iter().map(|l| l.0.as_str()).collect();
    if let Some(e) = check_entry_invariants_in_txn(tx, &account_ids, date)? {
        return Ok(Err(InvestmentError::Entry(e)));
    }

    let entry_id = Uuid::new_v4().to_string();
    let lines = lines
        .iter()
        .enumerate()
        .map(|(i, (account_id, amount, memo))| JournalLineData {
            line_id: format!("{entry_id}-line-{}", i + 1),
            account_id: account_id.clone(),
            amount: *amount,
            currency: currency.to_string(),
            exchange_rate: None,
            memo: Some((*memo).to_string()),
        })
        .collect();
    Ok(Ok(Event::JournalEntryPosted {
        entry_id,
        date,
        memo,
        lines,
        reference,
        // `System`, like the depreciation posting: this entry was computed from a
        // register rather than typed by anybody, and that is what the source field
        // is for.
        source: Some(JournalEntrySource::System),
    }))
}

fn security_exists(
    tx: &rusqlite::Transaction<'_>,
    security_id: &str,
) -> Result<bool, EventStoreError> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM securities WHERE id = ?1",
            [security_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

fn lookup_lot(
    tx: &rusqlite::Transaction<'_>,
    lot_id: &str,
) -> Result<Option<Lot>, EventStoreError> {
    Ok(read_lots(
        tx,
        "SELECT id, security_id, securities_account_id, cash_account_id, quantity,
                total_cost_cents, remaining_quantity, remaining_basis_cents, trade_date
           FROM investment_lots WHERE id = ?1",
        params![lot_id],
    )
    .into_iter()
    .next())
}

/// The ticker, for a memo, falling back to the id when the master has not been
/// read yet. A memo is not worth failing a posting over.
fn ticker_or_id(tx: &rusqlite::Transaction<'_>, security_id: &str) -> String {
    tx.query_row(
        "SELECT ticker FROM securities WHERE id = ?1",
        [security_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or_else(|| security_id.to_string())
}

fn base_currency(tx: &rusqlite::Transaction<'_>) -> Result<String, EventStoreError> {
    Ok(tx
        .query_row("SELECT base_currency FROM company LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .unwrap_or_else(|| "USD".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AccountType;
    use crate::store::migrations::SchemaStore;

    // The chart spec §2a describes, per account.
    const CASH: &str = "1101";
    const SECURITIES: &str = "1102";
    const DIVIDENDS: &str = "4100";
    const INTEREST: &str = "4110";
    const GAIN: &str = "4120";
    const FEES: &str = "6600";

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Shares as micro-shares, so the tests read in shares.
    fn sh(n: i64) -> i64 {
        n * MICRO_SHARE
    }

    fn store() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        store.init_schema().unwrap();
        crate::commands::partnership_commands::append_event_locally(
            &mut store,
            "u",
            Event::CompanyCreated {
                company_id: "c".into(),
                name: "Books".into(),
                base_currency: "USD".into(),
                fiscal_year_start: 1,
            },
        )
        .expect("company");
        for (id, name, kind) in [
            (CASH, "Brokerage cash", AccountType::Asset),
            (SECURITIES, "Securities at cost", AccountType::Asset),
            (DIVIDENDS, "Dividends", AccountType::Revenue),
            (INTEREST, "Interest", AccountType::Revenue),
            (GAIN, "Realized gain", AccountType::Revenue),
            (FEES, "Investment fees", AccountType::Expense),
        ] {
            crate::commands::partnership_commands::append_event_locally(
                &mut store,
                "u",
                Event::AccountCreated {
                    account_id: id.into(),
                    account_type: kind.into(),
                    account_number: id.into(),
                    name: name.into(),
                    parent_id: None,
                    currency: Some("USD".into()),
                    description: None,
                },
            )
            .expect("account");
        }
        store
    }

    fn acme(store: &mut EventStore) -> String {
        define_security(
            store,
            "u",
            &NewSecurity {
                ticker: "ACME".into(),
                name: "Acme Corp".into(),
                kind: "stock".into(),
                cusip: Some("037833100".into()),
                currency: "USD".into(),
            },
        )
        .expect("defined")
        .0
    }

    fn buy(
        store: &mut EventStore,
        security_id: &str,
        quantity: i64,
        total_cost_cents: i64,
        trade_date: NaiveDate,
    ) -> Bought {
        buy_security(
            store,
            "u",
            &BuySecurityCommand {
                security_id: security_id.to_string(),
                securities_account_id: SECURITIES.into(),
                cash_account_id: CASH.into(),
                quantity,
                total_cost_cents,
                trade_date,
                memo: None,
            },
        )
        .expect("bought")
    }

    fn sale(
        security_id: &str,
        quantity: i64,
        proceeds_cents: i64,
        trade_date: NaiveDate,
    ) -> SellSecurityCommand {
        SellSecurityCommand {
            security_id: security_id.to_string(),
            securities_account_id: SECURITIES.into(),
            cash_account_id: CASH.into(),
            realized_gain_account_id: GAIN.into(),
            quantity,
            proceeds_cents,
            fee_cents: 0,
            trade_date,
            selection: LotSelection::Fifo,
            memo: None,
        }
    }

    /// The net amount posted to one account by one entry. Positive is a debit.
    fn amount_on(store: &EventStore, entry_id: &str, account: &str) -> i64 {
        store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(amount), 0) FROM journal_lines
                  WHERE entry_id = ?1 AND account_id = ?2",
                params![entry_id, account],
                |r| r.get(0),
            )
            .expect("sum")
    }

    /// Every live entry in the books balances, and there is at least one.
    fn assert_every_entry_balances(store: &EventStore) {
        let unbalanced: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM (
                     SELECT entry_id, SUM(amount) AS s FROM journal_lines GROUP BY entry_id
                 ) WHERE s != 0",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(unbalanced, 0, "an entry in the books does not balance");
        let entries: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_entries", [], |r| r.get(0))
            .expect("count");
        assert!(entries > 0, "nothing was posted, so nothing was checked");
    }

    // --- the security master ---

    #[test]
    fn a_security_round_trips_through_the_log_with_its_currency() {
        let mut s = store();
        let id = acme(&mut s);
        let back = get_security(s.connection(), &id).expect("on the master");
        assert_eq!(back.ticker, "ACME");
        assert_eq!(back.name, "Acme Corp");
        assert_eq!(back.kind, "stock");
        assert_eq!(back.cusip.as_deref(), Some("037833100"));
        assert_eq!(back.currency, "USD");
    }

    /// Two masters for one ticker would split one holding into two that neither
    /// add up on the balance sheet nor reconcile against the 1099-B.
    #[test]
    fn a_ticker_already_on_the_master_is_refused() {
        let mut s = store();
        acme(&mut s);
        match define_security(
            &mut s,
            "u",
            &NewSecurity {
                ticker: "acme".into(), // case is not a second security either
                name: "Acme again".into(),
                kind: "stock".into(),
                cusip: None,
                currency: "USD".into(),
            },
        ) {
            Err(InvestmentError::TickerTaken { ticker, .. }) => assert_eq!(ticker, "ACME"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(list_securities(s.connection()).len(), 1);
    }

    // --- buying ---

    /// A buy changes the form the money is in, not the amount — and the
    /// commission is part of the basis, not an expense.
    #[test]
    fn a_buy_debits_securities_and_credits_cash_for_the_whole_cost_including_fees() {
        let mut s = store();
        let id = acme(&mut s);
        // 10 shares at $12.34 plus a $4.95 commission.
        let bought = buy(&mut s, &id, sh(10), 123_400 + 495, day(2025, 3, 3));

        assert_eq!(amount_on(&s, &bought.entry_id, SECURITIES), 123_895);
        assert_eq!(amount_on(&s, &bought.entry_id, CASH), -123_895);

        let lots = lots_of(s.connection(), &id, SECURITIES);
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].quantity, sh(10));
        assert_eq!(lots[0].total_cost_cents, 123_895);
        assert_eq!(lots[0].remaining_quantity, sh(10));
        assert_eq!(lots[0].remaining_basis_cents, 123_895);
        assert_eq!(
            holding_of(s.connection(), &id, SECURITIES),
            (sh(10), 123_895)
        );
        assert_every_entry_balances(&s);
    }

    #[test]
    fn a_buy_of_a_security_that_is_not_on_the_master_is_refused() {
        let mut s = store();
        match buy_security(
            &mut s,
            "u",
            &BuySecurityCommand {
                security_id: "nobody".into(),
                securities_account_id: SECURITIES.into(),
                cash_account_id: CASH.into(),
                quantity: sh(1),
                total_cost_cents: 100,
                trade_date: day(2025, 1, 2),
                memo: None,
            },
        ) {
            Err(InvestmentError::NoSuchSecurity(id)) => assert_eq!(id, "nobody"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(list_lots(s.connection()).is_empty());
    }

    #[test]
    fn a_zero_quantity_buy_or_sale_is_refused() {
        let mut s = store();
        let id = acme(&mut s);
        assert!(matches!(
            buy_security(
                &mut s,
                "u",
                &BuySecurityCommand {
                    security_id: id.clone(),
                    securities_account_id: SECURITIES.into(),
                    cash_account_id: CASH.into(),
                    quantity: 0,
                    total_cost_cents: 100,
                    trade_date: day(2025, 1, 2),
                    memo: None,
                },
            ),
            Err(InvestmentError::Invalid(_))
        ));
        // And a free lot, which only a corporate action produces.
        assert!(matches!(
            buy_security(
                &mut s,
                "u",
                &BuySecurityCommand {
                    security_id: id.clone(),
                    securities_account_id: SECURITIES.into(),
                    cash_account_id: CASH.into(),
                    quantity: sh(1),
                    total_cost_cents: 0,
                    trade_date: day(2025, 1, 2),
                    memo: None,
                },
            ),
            Err(InvestmentError::Invalid(_))
        ));

        buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 2));
        assert!(matches!(
            sell_security(&mut s, "u", &sale(&id, 0, 5_000, day(2025, 6, 1))),
            Err(InvestmentError::Invalid(_))
        ));
        assert!(list_lots(s.connection())[0].remaining_quantity == sh(10));
    }

    // --- selling: one lot ---

    /// Proceeds less the sale fee, basis out of the lot, and the difference to the
    /// gain account. The sale fee reduces proceeds rather than posting as an
    /// expense, because that is how a 1099-B reports proceeds.
    #[test]
    fn a_single_lot_sale_posts_the_net_gain_and_closes_the_lot() {
        let mut s = store();
        let id = acme(&mut s);
        let bought = buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));

        let mut cmd = sale(&id, sh(10), 150_000, day(2025, 9, 8));
        cmd.fee_cents = 995;
        let sold = sell_security(&mut s, "u", &cmd).expect("sold");

        assert_eq!(sold.basis_cents, 100_000);
        assert_eq!(sold.realized_gain_cents, 150_000 - 995 - 100_000);
        assert_eq!(sold.realized_gain_cents, 49_005);
        assert_eq!(sold.lots.len(), 1);
        assert_eq!(sold.lots[0].lot_id, bought.lot_id);
        assert_eq!(sold.lots[0].quantity, sh(10));
        assert_eq!(sold.lots[0].basis_cents, 100_000);
        assert_eq!(sold.lots[0].term, HoldingTerm::Short);

        assert_eq!(amount_on(&s, &sold.entry_id, CASH), 149_005);
        assert_eq!(amount_on(&s, &sold.entry_id, SECURITIES), -100_000);
        assert_eq!(amount_on(&s, &sold.entry_id, GAIN), -49_005);
        // Nothing of a sale fee reaches the expense account.
        assert_eq!(amount_on(&s, &sold.entry_id, FEES), 0);

        assert_eq!(holding_of(s.connection(), &id, SECURITIES), (0, 0));
        assert_every_entry_balances(&s);
    }

    /// A loss is a debit to the one gain account, which is what makes net gain a
    /// balance rather than a subtraction somebody has to remember.
    #[test]
    fn a_sale_below_basis_records_a_loss_as_a_debit_to_the_gain_account() {
        let mut s = store();
        let id = acme(&mut s);
        buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));

        let sold =
            sell_security(&mut s, "u", &sale(&id, sh(10), 62_500, day(2025, 11, 3))).expect("sold");
        assert_eq!(sold.realized_gain_cents, -37_500);
        assert_eq!(amount_on(&s, &sold.entry_id, CASH), 62_500);
        assert_eq!(amount_on(&s, &sold.entry_id, SECURITIES), -100_000);
        assert_eq!(
            amount_on(&s, &sold.entry_id, GAIN),
            37_500,
            "a loss debits the gain account"
        );
        assert_every_entry_balances(&s);
    }

    // --- selling: across lots ---

    /// FIFO takes the oldest shares first, and the partially consumed lot keeps
    /// exactly the cost that did not go with the shares sold.
    #[test]
    fn a_sale_spanning_two_lots_takes_the_oldest_first() {
        let mut s = store();
        let id = acme(&mut s);
        let first = buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));
        let second = buy(&mut s, &id, sh(10), 200_000, day(2025, 4, 6));

        let sold =
            sell_security(&mut s, "u", &sale(&id, sh(15), 400_000, day(2025, 8, 6))).expect("sold");

        assert_eq!(sold.lots.len(), 2);
        assert_eq!(sold.lots[0].lot_id, first.lot_id);
        assert_eq!(sold.lots[0].quantity, sh(10));
        assert_eq!(sold.lots[0].basis_cents, 100_000, "all of the first lot");
        assert_eq!(sold.lots[1].lot_id, second.lot_id);
        assert_eq!(sold.lots[1].quantity, sh(5));
        assert_eq!(sold.lots[1].basis_cents, 100_000, "half of the second");
        assert_eq!(sold.basis_cents, 200_000);
        assert_eq!(sold.realized_gain_cents, 200_000);

        let lots = lots_of(s.connection(), &id, SECURITIES);
        assert_eq!(lots[0].remaining_quantity, 0);
        assert_eq!(lots[0].remaining_basis_cents, 0);
        assert_eq!(lots[1].remaining_quantity, sh(5));
        assert_eq!(lots[1].remaining_basis_cents, 100_000);
        assert_eq!(
            holding_of(s.connection(), &id, SECURITIES),
            (sh(5), 100_000)
        );

        // And the register's own detail answers "which lots did this sale take".
        let consumed = consumed_lots(s.connection(), &sold.sale_id);
        assert_eq!(consumed.len(), 2);
        assert_eq!(consumed[0].lot_id, first.lot_id);
        assert_eq!(consumed[0].acquired_on, Some(day(2025, 1, 6)));
        assert_eq!(consumed[1].basis_cents, 100_000);
        assert_every_entry_balances(&s);
    }

    /// The whole reason a selection exists: picking the *newest* lot, which FIFO
    /// would never have chosen, and proving by the basis relieved that it did not.
    #[test]
    fn a_specific_selection_can_sell_the_newest_lot_and_leave_the_oldest_alone() {
        let mut s = store();
        let id = acme(&mut s);
        let first = buy(&mut s, &id, sh(10), 100_000, day(2024, 1, 8));
        let newest = buy(&mut s, &id, sh(10), 300_000, day(2025, 6, 2));

        let mut cmd = sale(&id, sh(10), 320_000, day(2025, 9, 2));
        cmd.selection = LotSelection::Specific(vec![(newest.lot_id.clone(), sh(10))]);
        let sold = sell_security(&mut s, "u", &cmd).expect("sold");

        assert_eq!(sold.lots.len(), 1);
        assert_eq!(sold.lots[0].lot_id, newest.lot_id);
        assert_eq!(
            sold.basis_cents, 300_000,
            "FIFO would have relieved 100,000 out of the 2024 lot"
        );
        assert_eq!(sold.realized_gain_cents, 20_000);
        assert_eq!(
            sold.lots[0].term,
            HoldingTerm::Short,
            "and the term follows the lot actually sold, not the oldest one"
        );

        let lots = lots_of(s.connection(), &id, SECURITIES);
        assert_eq!(lots[0].lot_id, first.lot_id);
        assert_eq!(
            lots[0].remaining_quantity,
            sh(10),
            "the oldest lot is untouched"
        );
        assert_eq!(lots[0].remaining_basis_cents, 100_000);
        assert_eq!(lots[1].remaining_quantity, 0);
        assert_every_entry_balances(&s);
    }

    /// Three partial sales of one $10.00 three-share lot: 333, 333 and 334 cents.
    /// Not a cent lost, not a cent invented, and the lot closes at exactly zero.
    #[test]
    fn repeated_partial_sales_of_one_lot_allocate_its_basis_to_the_cent() {
        let mut s = store();
        let id = acme(&mut s);
        let lot = buy(&mut s, &id, sh(3), 1_000, day(2025, 2, 2));

        let first = sell_security(&mut s, "u", &sale(&id, sh(1), 500, day(2025, 3, 2)))
            .expect("first third");
        assert_eq!(first.basis_cents, 333);
        assert_eq!(first.realized_gain_cents, 167);
        let after_first = lots_of(s.connection(), &id, SECURITIES);
        assert_eq!(after_first[0].remaining_quantity, sh(2));
        assert_eq!(after_first[0].remaining_basis_cents, 667);

        let second = sell_security(&mut s, "u", &sale(&id, sh(1), 500, day(2025, 4, 2)))
            .expect("second third");
        assert_eq!(second.basis_cents, 333, "667 * 1 / 2 floors to 333");
        let after_second = lots_of(s.connection(), &id, SECURITIES);
        assert_eq!(after_second[0].remaining_quantity, sh(1));
        assert_eq!(after_second[0].remaining_basis_cents, 334);

        let third = sell_security(&mut s, "u", &sale(&id, sh(1), 500, day(2025, 5, 2)))
            .expect("last third");
        assert_eq!(
            third.basis_cents, 334,
            "the sale that closes the lot takes the whole remainder"
        );

        assert_eq!(
            first.basis_cents + second.basis_cents + third.basis_cents,
            1_000,
            "the allocations sum to the lot's cost exactly"
        );
        assert_eq!(holding_of(s.connection(), &id, SECURITIES), (0, 0));

        // And the Securities account was relieved of exactly the lot's cost — the
        // same 1,000 cents it was debited, with nothing stranded in it.
        let securities_balance: i64 = s
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(amount), 0) FROM journal_lines WHERE account_id = ?1",
                [SECURITIES],
                |r| r.get(0),
            )
            .expect("balance");
        assert_eq!(securities_balance, 0);
        assert_eq!(
            lot.lot_id,
            lots_of(s.connection(), &id, SECURITIES)[0].lot_id
        );
        assert_every_entry_balances(&s);
    }

    /// Whatever the sale consumed, the Securities account gives up exactly that
    /// and no more — across a sale that spans lots and leaves one part-sold.
    #[test]
    fn the_securities_account_is_credited_exactly_the_basis_removed() {
        let mut s = store();
        let id = acme(&mut s);
        buy(&mut s, &id, sh(7), 3_333, day(2025, 1, 2));
        buy(&mut s, &id, sh(11), 7_777, day(2025, 2, 2));

        let sold =
            sell_security(&mut s, "u", &sale(&id, sh(9), 9_999, day(2025, 3, 2))).expect("sold");
        let per_lot: i64 = sold.lots.iter().map(|l| l.basis_cents).sum();
        assert_eq!(per_lot, sold.basis_cents);
        assert_eq!(
            amount_on(&s, &sold.entry_id, SECURITIES),
            -sold.basis_cents,
            "the credit is the basis the lots gave up"
        );

        // And the account's remaining balance is the register's remaining basis.
        let (_, remaining_basis) = holding_of(s.connection(), &id, SECURITIES);
        let securities_balance: i64 = s
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(amount), 0) FROM journal_lines WHERE account_id = ?1",
                [SECURITIES],
                |r| r.get(0),
            )
            .expect("balance");
        assert_eq!(securities_balance, remaining_basis);
        assert_eq!(securities_balance, 3_333 + 7_777 - sold.basis_cents);
        assert_every_entry_balances(&s);
    }

    // --- holding period ---

    /// One year **plus a day**: a lot bought on 1 January 2024 and sold on
    /// 1 January 2025 is short-term, and one bought a day earlier is long. So one
    /// sale straddling the boundary produces both rows.
    #[test]
    fn a_sale_straddling_one_year_plus_a_day_splits_short_and_long_term() {
        let mut s = store();
        let id = acme(&mut s);
        let long = buy(&mut s, &id, sh(5), 50_000, day(2023, 12, 31));
        let short = buy(&mut s, &id, sh(5), 60_000, day(2024, 1, 1));

        let sold =
            sell_security(&mut s, "u", &sale(&id, sh(10), 200_000, day(2025, 1, 1))).expect("sold");

        assert_eq!(sold.lots.len(), 2);
        assert_eq!(sold.lots[0].lot_id, long.lot_id);
        assert_eq!(
            sold.lots[0].term,
            HoldingTerm::Long,
            "2023-12-31 to 2025-01-01 is a year and a day"
        );
        assert_eq!(sold.lots[1].lot_id, short.lot_id);
        assert_eq!(
            sold.lots[1].term,
            HoldingTerm::Short,
            "2024-01-01 to 2025-01-01 is exactly a year, which is not more than one"
        );

        // The same split comes back out of the register, which is what Form 8949
        // is filled from.
        let consumed = consumed_lots(s.connection(), &sold.sale_id);
        assert_eq!(
            consumed
                .iter()
                .filter(|c| c.term == HoldingTerm::Long)
                .map(|c| c.basis_cents)
                .sum::<i64>(),
            50_000
        );
        assert_eq!(
            consumed
                .iter()
                .filter(|c| c.term == HoldingTerm::Short)
                .map(|c| c.basis_cents)
                .sum::<i64>(),
            60_000
        );
        assert_every_entry_balances(&s);
    }

    /// The day after that boundary is long-term, and the day of the purchase's
    /// anniversary is not — asserted directly on the lot, because this is the one
    /// rule in the module worth 20 points of tax rate.
    #[test]
    fn the_holding_period_turns_long_the_day_after_the_anniversary() {
        let lot = Lot {
            lot_id: "l".into(),
            security_id: "s".into(),
            securities_account_id: SECURITIES.into(),
            cash_account_id: CASH.into(),
            quantity: sh(1),
            total_cost_cents: 100,
            remaining_quantity: sh(1),
            remaining_basis_cents: 100,
            trade_date: day(2024, 3, 15),
        };
        assert_eq!(lot.term_on(day(2025, 3, 14)), HoldingTerm::Short);
        assert_eq!(lot.term_on(day(2025, 3, 15)), HoldingTerm::Short);
        assert_eq!(lot.term_on(day(2025, 3, 16)), HoldingTerm::Long);
        // Sold the day it was bought: short, obviously, and not a panic.
        assert_eq!(lot.term_on(day(2024, 3, 15)), HoldingTerm::Short);

        // 29 February has no anniversary; chrono lands on the 28th, and the day
        // after that is long-term.
        let leap = Lot {
            trade_date: day(2024, 2, 29),
            ..lot
        };
        assert_eq!(leap.term_on(day(2025, 2, 28)), HoldingTerm::Short);
        assert_eq!(leap.term_on(day(2025, 3, 1)), HoldingTerm::Long);
    }

    // --- refusals ---

    /// Refused, not clamped. A sale larger than the position means the shares are
    /// somewhere else or the purchase was never entered, and clamping it posts a
    /// gain against a basis nobody chose.
    #[test]
    fn selling_more_than_is_held_is_refused_rather_than_clamped() {
        let mut s = store();
        let id = acme(&mut s);
        buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));

        match sell_security(&mut s, "u", &sale(&id, sh(11), 200_000, day(2025, 6, 6))) {
            Err(InvestmentError::NotEnoughShares { requested, held }) => {
                assert_eq!(requested, "11");
                assert_eq!(held, "10");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        // Nothing moved: no entry, and the lot is whole.
        assert_eq!(
            holding_of(s.connection(), &id, SECURITIES),
            (sh(10), 100_000)
        );
        let entries: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_entries", [], |r| r.get(0))
            .expect("count");
        assert_eq!(entries, 1, "only the purchase");
    }

    /// The remaining quantity is the fence: a lot already sold has nothing left to
    /// give, whether FIFO or a selection asks for it.
    #[test]
    fn a_lot_cannot_be_consumed_twice() {
        let mut s = store();
        let id = acme(&mut s);
        let lot = buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));
        sell_security(&mut s, "u", &sale(&id, sh(10), 150_000, day(2025, 6, 6))).expect("sold");

        let mut cmd = sale(&id, sh(10), 150_000, day(2025, 7, 6));
        cmd.selection = LotSelection::Specific(vec![(lot.lot_id.clone(), sh(10))]);
        match sell_security(&mut s, "u", &cmd) {
            Err(InvestmentError::NotEnoughShares { held, .. }) => assert_eq!(held, "0"),
            other => panic!("expected a refusal, got {other:?}"),
        }

        // And even a single share out of it, with the holding fence out of the way
        // by a second purchase, is refused against that lot specifically.
        buy(&mut s, &id, sh(10), 100_000, day(2025, 8, 6));
        let mut cmd = sale(&id, sh(1), 15_000, day(2025, 9, 6));
        cmd.selection = LotSelection::Specific(vec![(lot.lot_id.clone(), sh(1))]);
        match sell_security(&mut s, "u", &cmd) {
            Err(InvestmentError::LotOverdrawn { remaining, .. }) => assert_eq!(remaining, "0"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_every_entry_balances(&s);
    }

    /// A specific-lot sale has to say where every share came from.
    #[test]
    fn a_specific_selection_whose_quantities_do_not_sum_is_refused() {
        let mut s = store();
        let id = acme(&mut s);
        let a = buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));
        let b = buy(&mut s, &id, sh(10), 200_000, day(2025, 2, 6));

        let mut cmd = sale(&id, sh(10), 150_000, day(2025, 6, 6));
        cmd.selection = LotSelection::Specific(vec![(a.lot_id.clone(), sh(4))]);
        match sell_security(&mut s, "u", &cmd) {
            Err(InvestmentError::SelectionDoesNotSum { selected, sale }) => {
                assert_eq!(selected, "4");
                assert_eq!(sale, "10");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        // Too many is refused the same way, not silently truncated.
        cmd.selection =
            LotSelection::Specific(vec![(a.lot_id.clone(), sh(10)), (b.lot_id.clone(), sh(2))]);
        assert!(matches!(
            sell_security(&mut s, "u", &cmd),
            Err(InvestmentError::SelectionDoesNotSum { .. })
        ));

        // Naming a lot twice is refused rather than added together — the second
        // row is a mistake, and summing it would consume shares the caller did not
        // mean to.
        cmd.selection =
            LotSelection::Specific(vec![(a.lot_id.clone(), sh(5)), (a.lot_id.clone(), sh(5))]);
        assert!(matches!(
            sell_security(&mut s, "u", &cmd),
            Err(InvestmentError::LotNamedTwice(_))
        ));

        assert_eq!(
            holding_of(s.connection(), &id, SECURITIES),
            (sh(20), 300_000),
            "nothing was consumed by any of the refusals"
        );
    }

    /// A selection must name lots of the right security **and** the right account;
    /// otherwise one brokerage's basis is relieved by another's sale.
    #[test]
    fn a_specific_selection_must_name_lots_of_the_right_security_and_account() {
        let mut s = store();
        let id = acme(&mut s);
        let (other_id, _) = define_security(
            &mut s,
            "u",
            &NewSecurity {
                ticker: "OTHR".into(),
                name: "Other Inc".into(),
                kind: "stock".into(),
                cusip: None,
                currency: "USD".into(),
            },
        )
        .expect("defined");
        buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));
        let wrong_security = buy(&mut s, &other_id, sh(10), 100_000, day(2025, 1, 6));

        let mut cmd = sale(&id, sh(10), 150_000, day(2025, 6, 6));
        cmd.selection = LotSelection::Specific(vec![(wrong_security.lot_id.clone(), sh(10))]);
        assert!(matches!(
            sell_security(&mut s, "u", &cmd),
            Err(InvestmentError::LotWrongSecurity { .. })
        ));

        // A second Securities account, and a lot in it: the sale is out of the
        // first, so the lot in the second is not available to it.
        crate::commands::partnership_commands::append_event_locally(
            &mut s,
            "u",
            Event::AccountCreated {
                account_id: "1202".into(),
                account_type: AccountType::Asset.into(),
                account_number: "1202".into(),
                name: "Securities at the other broker".into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            },
        )
        .expect("account");
        let elsewhere = buy_security(
            &mut s,
            "u",
            &BuySecurityCommand {
                security_id: id.clone(),
                securities_account_id: "1202".into(),
                cash_account_id: CASH.into(),
                quantity: sh(10),
                total_cost_cents: 100_000,
                trade_date: day(2025, 2, 6),
                memo: None,
            },
        )
        .expect("bought");
        cmd.selection = LotSelection::Specific(vec![(elsewhere.lot_id.clone(), sh(10))]);
        assert!(matches!(
            sell_security(&mut s, "u", &cmd),
            Err(InvestmentError::LotWrongAccount { .. })
        ));

        // A lot that does not exist at all says so.
        cmd.selection = LotSelection::Specific(vec![("no-such-lot".to_string(), sh(10))]);
        assert!(matches!(
            sell_security(&mut s, "u", &cmd),
            Err(InvestmentError::NoSuchLot(_))
        ));

        // And FIFO out of the first account never reaches into the second.
        let sold =
            sell_security(&mut s, "u", &sale(&id, sh(10), 150_000, day(2025, 6, 6))).expect("sold");
        assert_eq!(sold.lots.len(), 1);
        assert_eq!(
            holding_of(s.connection(), &id, "1202"),
            (sh(10), 100_000),
            "the other broker's lot is untouched"
        );
        assert_every_entry_balances(&s);
    }

    /// A clear domain rejection, not a panic and not a sale with no basis behind
    /// it — which would report the whole proceeds as gain.
    #[test]
    fn selling_a_security_with_no_lots_is_refused() {
        let mut s = store();
        let id = acme(&mut s);
        match sell_security(&mut s, "u", &sale(&id, sh(1), 10_000, day(2025, 6, 6))) {
            Err(InvestmentError::NoLots { security_id, .. }) => assert_eq!(security_id, id),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let entries: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_entries", [], |r| r.get(0))
            .expect("count");
        assert_eq!(entries, 0, "no entry was posted");

        // A security that is not on the master at all is a different refusal.
        assert!(matches!(
            sell_security(&mut s, "u", &sale("nobody", sh(1), 10_000, day(2025, 6, 6))),
            Err(InvestmentError::NoSuchSecurity(_))
        ));
    }

    // --- income and fees ---

    #[test]
    fn a_dividend_debits_cash_and_credits_the_dividend_account() {
        let mut s = store();
        let id = acme(&mut s);
        let entry_id = record_income(
            &mut s,
            "u",
            &RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Dividend,
                security_id: Some(id.clone()),
                cash_account_id: CASH.into(),
                income_account_id: DIVIDENDS.into(),
                amount_cents: 4_237,
                received_on: day(2025, 3, 14),
                memo: None,
            },
        )
        .expect("recorded");

        assert_eq!(amount_on(&s, &entry_id, CASH), 4_237);
        assert_eq!(amount_on(&s, &entry_id, DIVIDENDS), -4_237);
        assert_eq!(amount_on(&s, &entry_id, INTEREST), 0);
        assert_every_entry_balances(&s);
    }

    /// Sweep interest belongs to the account and not to any holding, so it is
    /// recordable without a security.
    #[test]
    fn interest_credits_the_interest_account_and_needs_no_security() {
        let mut s = store();
        let entry_id = record_income(
            &mut s,
            "u",
            &RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Interest,
                security_id: None,
                cash_account_id: CASH.into(),
                income_account_id: INTEREST.into(),
                amount_cents: 112,
                received_on: day(2025, 3, 31),
                memo: None,
            },
        )
        .expect("recorded");

        assert_eq!(amount_on(&s, &entry_id, CASH), 112);
        assert_eq!(amount_on(&s, &entry_id, INTEREST), -112);
        assert_eq!(amount_on(&s, &entry_id, DIVIDENDS), 0);

        assert!(matches!(
            record_income(
                &mut s,
                "u",
                &RecordInvestmentIncomeCommand {
                    kind: InvestmentIncomeKind::Interest,
                    security_id: None,
                    cash_account_id: CASH.into(),
                    income_account_id: INTEREST.into(),
                    amount_cents: 0,
                    received_on: day(2025, 3, 31),
                    memo: None,
                },
            ),
            Err(InvestmentError::Invalid(_))
        ));
        assert_every_entry_balances(&s);
    }

    /// A standalone fee is an expense, not a reduction of anybody's proceeds — it
    /// appears on no 1099-B.
    #[test]
    fn a_standalone_fee_debits_the_fee_expense_account() {
        let mut s = store();
        let entry_id = charge_fee(
            &mut s,
            "u",
            &ChargeInvestmentFeeCommand {
                cash_account_id: CASH.into(),
                expense_account_id: FEES.into(),
                amount_cents: 2_500,
                charged_on: day(2025, 12, 31),
                security_id: None,
                memo: None,
            },
        )
        .expect("charged");

        assert_eq!(amount_on(&s, &entry_id, FEES), 2_500);
        assert_eq!(amount_on(&s, &entry_id, CASH), -2_500);
        assert_every_entry_balances(&s);
    }

    // --- the whole thing, and the ledger's own fences ---

    /// Every command in the module, one after another, and every entry the books
    /// end up holding balances.
    #[test]
    fn every_posted_entry_balances_across_a_whole_year_of_activity() {
        let mut s = store();
        let id = acme(&mut s);
        buy(&mut s, &id, sh(10), 123_456, day(2024, 1, 10));
        buy(&mut s, &id, sh(7), 98_765, day(2024, 7, 19));
        record_income(
            &mut s,
            "u",
            &RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Dividend,
                security_id: Some(id.clone()),
                cash_account_id: CASH.into(),
                income_account_id: DIVIDENDS.into(),
                amount_cents: 1_337,
                received_on: day(2024, 9, 30),
                memo: None,
            },
        )
        .expect("dividend");
        record_income(
            &mut s,
            "u",
            &RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Interest,
                security_id: None,
                cash_account_id: CASH.into(),
                income_account_id: INTEREST.into(),
                amount_cents: 43,
                received_on: day(2024, 9, 30),
                memo: None,
            },
        )
        .expect("interest");
        charge_fee(
            &mut s,
            "u",
            &ChargeInvestmentFeeCommand {
                cash_account_id: CASH.into(),
                expense_account_id: FEES.into(),
                amount_cents: 999,
                charged_on: day(2024, 12, 31),
                security_id: Some(id.clone()),
                memo: None,
            },
        )
        .expect("fee");
        let mut cmd = sale(&id, sh(13), 250_001, day(2025, 2, 11));
        cmd.fee_cents = 87;
        let sold = sell_security(&mut s, "u", &cmd).expect("sold");
        // A gain straddling the boundary, and a part-sold lot left behind.
        assert_eq!(sold.lots.len(), 2);
        assert_eq!(sold.lots[0].term, HoldingTerm::Long);
        assert_eq!(sold.lots[1].term, HoldingTerm::Short);
        assert_eq!(
            sold.basis_cents,
            123_456 + 98_765 * 3 / 7,
            "all of the first lot and three sevenths of the second"
        );
        assert_eq!(
            sold.realized_gain_cents,
            250_001 - 87 - sold.basis_cents,
            "gain is net proceeds less basis"
        );

        assert_every_entry_balances(&s);

        // Seven entries: two buys, two income, one fee, one sale — and the
        // register agrees with the account it fills.
        let (_, remaining_basis) = holding_of(s.connection(), &id, SECURITIES);
        let securities_balance: i64 = s
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(amount), 0) FROM journal_lines WHERE account_id = ?1",
                [SECURITIES],
                |r| r.get(0),
            )
            .expect("balance");
        assert_eq!(securities_balance, remaining_basis);
    }

    /// The ledger's own fences apply to these postings like any other: a closed
    /// year refuses them, and nothing lands half-way.
    #[test]
    fn a_trade_in_a_closed_year_is_refused_and_leaves_no_lot_behind() {
        let mut s = store();
        let id = acme(&mut s);
        for event in [
            Event::FiscalYearOpened {
                year: 2024,
                start_date: day(2024, 1, 1),
                end_date: day(2024, 12, 31),
            },
            Event::YearEndClosed {
                year: 2024,
                retained_earnings_entry_id: "made-up".into(),
            },
        ] {
            crate::commands::partnership_commands::append_event_locally(&mut s, "u", event)
                .expect("year");
        }

        match buy_security(
            &mut s,
            "u",
            &BuySecurityCommand {
                security_id: id.clone(),
                securities_account_id: SECURITIES.into(),
                cash_account_id: CASH.into(),
                quantity: sh(1),
                total_cost_cents: 1_000,
                trade_date: day(2024, 6, 1),
                memo: None,
            },
        ) {
            Err(InvestmentError::Entry(EntryCommandError::YearClosed(d))) => {
                assert_eq!(d, day(2024, 6, 1))
            }
            other => panic!("expected the closed-year fence, got {other:?}"),
        }
        assert!(
            list_lots(s.connection()).is_empty(),
            "the lot and its entry land together or not at all"
        );
    }

    /// The register survives a replay, because it is derived from the log and not
    /// merged into it — the fence that stops a lot no event justifies from going
    /// on relieving basis.
    #[test]
    fn the_register_is_rebuilt_from_the_log_and_comes_back_identical() {
        let mut s = store();
        let id = acme(&mut s);
        buy(&mut s, &id, sh(10), 100_000, day(2025, 1, 6));
        buy(&mut s, &id, sh(10), 200_000, day(2025, 4, 6));
        let sold =
            sell_security(&mut s, "u", &sale(&id, sh(15), 400_000, day(2025, 8, 6))).expect("sold");

        let before_lots = list_lots(s.connection());
        let before_consumed = consumed_lots(s.connection(), &sold.sale_id);

        let events = s.get_all().expect("events");
        let tx = s.connection_mut().transaction().expect("txn");
        Projector::new(&tx).rebuild(&events).expect("rebuilt");
        tx.commit().expect("commit");

        assert_eq!(list_lots(s.connection()), before_lots);
        assert_eq!(
            consumed_lots(s.connection(), &sold.sale_id),
            before_consumed
        );
        assert_eq!(list_securities(s.connection()).len(), 1);
        assert_eq!(
            holding_of(s.connection(), &id, SECURITIES),
            (sh(5), 100_000)
        );
    }
}
