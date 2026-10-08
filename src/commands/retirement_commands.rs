//! The sheltered-account ledger: contributions in, distributions out, and one
//! entry per statement period saying what the account is now worth.
//!
//! INVESTMENTS-SPEC.md phase 2. Nothing here knows about securities, lots,
//! holding periods or realized gains, and that is the point rather than an
//! omission.
//!
//! # Why there are no lots and no securities in a sheltered account
//!
//! Because nothing inside one is taxable (spec §2b). A buy, a sale, a dividend and
//! a reinvestment inside a 401(k) have no tax consequence whatever: there is no
//! Form 8949 row to produce, no holding period that matters, and no basis anybody
//! will ever need. Lot accounting there is machinery that answers no question, and
//! a target-date fund would put four hundred meaningless trades a year into the
//! books for it.
//!
//! So a sheltered account is **one ledger account carried at value**:
//!
//! ```text
//! Assets:Retirement:<Institution ••5678>          one account, carried at VALUE
//! Income:Investments:Retirement value change      non-taxable; off every tax line
//! ```
//!
//! That is the opposite of the taxable rule in [`super::investment_commands`],
//! where cost is carried and value is only ever a report. The difference is not
//! inconsistency: cost is carried in a taxable account because cost is what a gain
//! is measured against, and here there is no gain to measure.
//!
//! # How the money moves (spec §2b)
//!
//! | Event | Debit | Credit |
//! |---|---|---|
//! | Contribution | the retirement account | the funding account (a transfer) |
//! | Distribution | the receiving account (gross − withheld), the prepaid-tax account (withheld) | the retirement account (gross) |
//! | Value update | the retirement account, **by the difference** | the value-change account |
//! | Value update, downward | the value-change account | the retirement account |
//!
//! # Employer plans funded through payroll are out of scope
//!
//! Deliberately, and this is the second opinion the spec asks nobody to grow. A
//! salary deferral reduces taxable wages and an employer match is not income to
//! the employee; both are facts about a payroll run, and payroll already owns
//! them. [`record_contribution`] is for money the owner *moves* — a transfer from a
//! bank account into an IRA, a after-tax contribution, a rollover landing as cash.
//! A deferral that arrived through payroll must not also arrive through here, or
//! the same contribution is in the books twice and the account's value stops
//! agreeing with the statement.
//!
//! If phase 4's importer ever sees a payroll contribution arriving at the
//! custodian, the answer is to reconcile it against what payroll already posted,
//! not to post it again.
//!
//! # Value updates post the difference, never the value
//!
//! [`set_value`] compares the statement's figure against what the books already say
//! the account held **on that date** and posts only the gap. Posting the value
//! itself would double the account every period.
//!
//! Comparing against the ledger rather than against the last figure the register
//! stored is what makes contributions and distributions come out right. A
//! statement of $102,000 following a statement of $100,000 with a $1,000
//! contribution in between is a gain of $1,000, not $2,000 — and the ledger is the
//! only thing that knows the contribution happened.
//!
//! It also makes a value update **naturally idempotent**: re-importing the same
//! statement finds the books already agree and posts nothing, so these entries need
//! no idempotency reference. A contribution and a distribution are not idempotent
//! that way, which is why both take an optional `reference` — migration 014's
//! partial unique index is then what stops the same one landing twice.
//!
//! # Every invariant is checked under the write lock
//!
//! Each command validates inside the append transaction and the register event
//! goes in the same batch as its journal entry, exactly as
//! [`super::investment_commands`] does and for the same reason: a register that
//! says the account is worth $102,000 with no entry behind it, or an entry with no
//! register row to explain it, is worse than either failing.

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeSet;
use thiserror::Error;

use crate::commands::entry_commands::EntryCommandError;
use crate::commands::investment_commands::{entry_or_reject, InvestmentError};
use crate::events::types::{
    Event, EventEnvelope, RetirementDistributionData, RetirementKind, StoredEvent,
};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;

#[derive(Debug, Error)]
pub enum RetirementError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("Could not post the entry: {0}")]
    Entry(#[from] EntryCommandError),
    #[error("No account with id {0}")]
    NoSuchAccount(String),
    #[error(
        "Account {0} is not on the retirement register. Register it first: whether a distribution \
         out of it is taxable depends on what kind of account it is, and nothing else in the books \
         records that."
    )]
    NotRegistered(String),
    #[error(
        "Account {account_id} is already on the retirement register as {kind}. One ledger account \
         is one retirement account; a second row would mean two answers to whether a distribution \
         out of it is taxable."
    )]
    AlreadyRegistered { account_id: String, kind: String },
    #[error(
        "Account {account_id} must be an asset account to be carried at value, and it is {found}"
    )]
    NotAnAsset { account_id: String, found: String },
    #[error(
        "The value-change account {account_id} must be a revenue account — growth is income, \
         non-taxable income, and a loss is a debit back against it — and it is {found}. Check the \
         two accounts are not the wrong way round."
    )]
    NotRevenue { account_id: String, found: String },
    #[error(
        "Account {0} already records another sheltered account's value change, so it cannot be a \
         retirement account itself"
    )]
    AlreadyAValueChangeAccount(String),
    #[error(
        "Account {0} is itself a retirement account, so it cannot be where another one's value \
         change is recorded"
    )]
    ValueChangeIsARetirementAccount(String),
    #[error(
        "The last value for {account_id} is as of {last}, and this one is as of {as_of}. A value \
         series that runs backwards posts a fictional loss: the difference would be measured \
         against a book value that already includes everything after {as_of}. Correct the later \
         statement instead."
    )]
    ValueOutOfOrder {
        account_id: String,
        as_of: NaiveDate,
        last: NaiveDate,
    },
    #[error(
        "A distribution posts no income, so there is nothing for the income account \
         {0} to receive. The account is carried at value: every dollar of growth was already \
         recognised when the value was set, and crediting income now would put the same dollar on \
         the income statement twice. The taxable figure a 1099-R reports is recorded on the event \
         instead — pass it as `taxable_cents`."
    )]
    NoIncomeAccountOnADistribution(String),
    #[error(
        "Account {account_id} is registered as {kind}, and the register cannot decide how much of \
         a distribution out of it is taxable — a 529 or an HSA is taxed on what the money was \
         spent on, which no ledger holds. Say the taxable amount."
    )]
    TaxableAmountNotStated { account_id: String, kind: String },
    #[error("Invalid data: {0}")]
    Invalid(String),
}

impl From<EventStoreError> for RetirementError {
    fn from(e: EventStoreError) -> Self {
        RetirementError::Store(e.to_string())
    }
}

/// Carry a posting rejection across from [`entry_or_reject`], which phase 1 owns.
///
/// The alternative was a second copy of the entry fences — the balance check, the
/// reference uniqueness, the account-exists-and-is-active check, the closed-year
/// fence — and a second copy is a second place for one of them to go missing.
/// `InvestmentError` only ever produces two shapes from that function, and this
/// maps both without inventing a third.
fn posting_error(e: InvestmentError) -> RetirementError {
    match e {
        InvestmentError::Entry(e) => RetirementError::Entry(e),
        other => RetirementError::Invalid(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Reading the register
// ---------------------------------------------------------------------------

/// One sheltered account on the register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetirementAccount {
    /// The ledger account carried at value.
    pub account_id: String,
    pub institution: String,
    pub kind: RetirementKind,
    pub value_change_account_id: String,
    /// What the most recent statement said, and as of when. `None` until a first
    /// value is set — distinct from zero, because "nobody has told us yet" and
    /// "the statement says it is empty" are different facts and the out-of-order
    /// fence has nothing to compare against in the first case.
    pub last_value_cents: Option<i64>,
    pub last_value_as_of: Option<NaiveDate>,
}

/// Every sheltered account, by ledger account id.
pub fn list_accounts(conn: &Connection) -> Vec<RetirementAccount> {
    read_accounts(conn, "", &[])
}

pub fn get_account(conn: &Connection, account_id: &str) -> Option<RetirementAccount> {
    read_accounts(conn, "WHERE account_id = ?1", &[&account_id])
        .into_iter()
        .next()
}

fn read_accounts(
    conn: &Connection,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Vec<RetirementAccount> {
    let sql = format!(
        "SELECT account_id, institution, kind, value_change_account_id,
                last_value_cents, last_value_as_of
           FROM retirement_accounts {filter} ORDER BY account_id"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(args, |r| {
        let kind: String = r.get(2)?;
        let as_of: Option<String> = r.get(5)?;
        Ok(RetirementAccount {
            account_id: r.get(0)?,
            institution: r.get(1)?,
            // An unparseable kind falls to `Other`, the one that decides nothing:
            // it makes the register decline to compute a taxable amount rather
            // than guess one, which is the safe direction for a row written by
            // something newer than this code.
            kind: RetirementKind::parse(&kind).unwrap_or(RetirementKind::Other),
            value_change_account_id: r.get(3)?,
            last_value_cents: r.get(4)?,
            last_value_as_of: as_of.and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()),
        })
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Every account that records a sheltered account's value change.
///
/// Read by [`crate::tax::lines::load_effective_mapping`], which forces every one of
/// them onto `OFF_RETURN`. That is the whole non-taxable fence (spec §8), so the
/// query is kept as narrow as it can be: one column, one table, no joins, nothing
/// that can be confused by a deactivated account or a gap in the chart.
///
/// Infallible because its callers are. The only way it can fail is
/// `retirement_accounts` not existing, which `init_schema` and `run_migrations`
/// both create and which `init_schema_has_the_retirement_register` asserts.
pub fn value_change_account_ids(conn: &Connection) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT DISTINCT value_change_account_id FROM retirement_accounts")
    else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) {
        out.extend(rows.flatten());
    }
    out
}

/// Whether this account records a sheltered account's value change.
pub fn is_value_change_account(conn: &Connection, account_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM retirement_accounts WHERE value_change_account_id = ?1",
        [account_id],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// What the books say the account held on a date.
///
/// The comparison a value update is measured against. Positive is a debit, which
/// for an asset account is what it holds. Void entries are excluded, because a
/// voided contribution is money that did not move and counting it would make the
/// next statement look like a loss of exactly that much.
///
/// `date <= as_of` rather than the whole balance, so a value as of 31 January is
/// compared against what the books said on 31 January — not against a February
/// contribution that has already been entered. The adjustment is dated `as_of`
/// too, so the two stay consistent as later periods are added.
pub fn book_value_cents(conn: &Connection, account_id: &str, as_of: NaiveDate) -> i64 {
    sum_to_date(conn, account_id, as_of).unwrap_or(0)
}

fn sum_to_date(
    conn: &Connection,
    account_id: &str,
    as_of: NaiveDate,
) -> Result<i64, rusqlite::Error> {
    conn.query_row(
        "SELECT COALESCE(SUM(jl.amount), 0)
           FROM journal_lines jl
           JOIN journal_entries je ON je.id = jl.entry_id
          WHERE jl.account_id = ?1 AND je.is_void = 0 AND je.date <= ?2",
        rusqlite::params![account_id, as_of.to_string()],
        |r| r.get(0),
    )
}

/// One distribution, read back out of the log.
///
/// The 1099-R's figures (spec §8): box 1 gross, box 2a taxable, box 4 withheld.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distribution {
    pub account_id: String,
    pub receiving_account_id: String,
    pub gross_cents: i64,
    pub taxable_cents: i64,
    pub withheld_cents: i64,
    pub withheld_account_id: String,
    pub on: NaiveDate,
}

/// Every distribution the log holds, oldest first.
///
/// Read from the log rather than from a projection, and deliberately. What the
/// ledger cannot hold is the taxable amount — a distribution posts no income (see
/// [`RetirementDistributionData`]), so no account's balance is it — and that
/// figure is carried on the event exactly as `SecuritySold` carries the lots a
/// sale consumed. This is the reader phase 6's 1099-R will be built on. A
/// projection table would be one nothing else reads and nothing keeps honest; when
/// the form arrives and wants to query by year, it can bring one.
pub fn list_distributions(store: &EventStore) -> Result<Vec<Distribution>, RetirementError> {
    Ok(store
        .get_by_type("retirement_distribution_recorded")?
        .into_iter()
        .filter_map(|e| match e.event {
            Event::RetirementDistributionRecorded(d) => Some(Distribution {
                account_id: d.account_id,
                receiving_account_id: d.receiving_account_id,
                gross_cents: d.gross_cents,
                taxable_cents: d.taxable_cents,
                withheld_cents: d.withheld_cents,
                withheld_account_id: d.withheld_account_id,
                on: d.on,
            }),
            _ => None,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RegisterRetirementAccountCommand {
    /// An existing asset account, which becomes the account carried at value.
    pub account_id: String,
    /// "Fidelity ••5678".
    pub institution: String,
    pub kind: RetirementKind,
    /// An existing revenue account: `Income:Investments:Retirement value change`.
    /// One shared across every sheltered account in the book is normal — spec
    /// §2b's chart has exactly one.
    pub value_change_account_id: String,
}

#[derive(Debug, Clone)]
pub struct SetRetirementValueCommand {
    pub account_id: String,
    /// The statement date. Must not precede the last one recorded.
    pub as_of: NaiveDate,
    /// What the statement says it is worth. The **difference** from the book value
    /// is what posts.
    pub value_cents: i64,
    pub memo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RetirementContributionCommand {
    pub account_id: String,
    /// The bank account the money came from.
    pub funding_account_id: String,
    pub amount_cents: i64,
    pub on: NaiveDate,
    pub memo: Option<String>,
    /// An idempotency key, when the caller has one — phase 4's importer will pass
    /// Plaid's `investment_transaction_id`. `None` for a contribution entered by
    /// hand: there is no natural key, and one invented from the amount and the
    /// date would collide two real contributions of the same size on the same day.
    pub reference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RetirementDistributionCommand {
    pub account_id: String,
    /// Where the net lands.
    pub receiving_account_id: String,
    /// Box 1: everything that left the retirement account.
    pub gross_cents: i64,
    /// Box 4: what the payer withheld and sent to the Treasury.
    pub withheld_cents: i64,
    /// A **prepaid-tax asset** account, named by the caller. Withholding is money
    /// paid toward a tax bill that is not settled yet — it comes back as a refund
    /// or reduces what is owed in April — so it is an asset, not an expense.
    /// Expensing it would both overstate expenses and lose a payment already made.
    pub withheld_account_id: String,
    /// **Always [`None`], and refused when it is not.**
    ///
    /// It is on the command rather than left off it because the intuition that a
    /// taxable distribution needs an income credit is a reasonable one to arrive
    /// at, and this is where it gets corrected with a reason instead of being
    /// silently ignored. A sheltered account is carried at value: every dollar of
    /// growth was recognised as `Income:Investments:Retirement value change` when
    /// the value was set, and every dollar of contribution as the transfer it was.
    /// Crediting income again at distribution would put the same dollar on the
    /// income statement twice. See [`RetirementDistributionData`] for the whole
    /// argument, and `taxable_cents` for where the 1099-R figure goes instead.
    pub taxable_income_account_id: Option<String>,
    /// Box 2a, when the caller knows better than the register does.
    ///
    /// `None` takes the answer from the account's kind: the whole gross for a
    /// [`RetirementKind::Traditional`] account, nothing for a
    /// [`RetirementKind::Roth`] one, and a refusal for
    /// [`RetirementKind::Other`] — a 529 or an HSA is taxed on what the money was
    /// spent on, which no ledger holds.
    ///
    /// `Some` covers the cases the kind cannot: after-tax basis in a traditional
    /// account (Form 8606), and the earnings part of a non-qualified Roth
    /// withdrawal. Both are real and neither needs a schema change to record.
    pub taxable_cents: Option<i64>,
    pub on: NaiveDate,
    pub memo: Option<String>,
    /// As on a contribution.
    pub reference: Option<String>,
}

/// What a value update came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSet {
    /// What the books said the account held on `as_of`, before this.
    pub book_value_cents: i64,
    /// What posted to the value-change account: positive for growth, negative for
    /// a fall, zero when the statement confirmed what the books already said.
    pub change_cents: i64,
    /// `None` when nothing changed. A statement that confirms no change is not a
    /// journal entry.
    pub entry_id: Option<String>,
}

/// What a distribution came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distributed {
    pub entry_id: String,
    /// Gross less withholding — what reached the receiving account.
    pub net_cents: i64,
    /// Box 2a, as recorded on the event. Recoverable from the log for as long as
    /// the log exists, which is what a 1099-R filed three years from now needs.
    pub taxable_cents: i64,
}

/// Put a ledger account on the retirement register.
///
/// Posts nothing — it says what an account *is*. It does append one thing besides
/// the registration, in the same batch: a `TaxLineMappingSet` putting the
/// value-change account on [`crate::tax::lines::OFF_RETURN`], so the account is
/// excluded from the return from the moment it exists rather than when somebody
/// remembers. See [`check_registration_in_txn`] for why that is one of three
/// fences and not the only one.
pub fn register_account(
    store: &mut EventStore,
    user_id: &str,
    cmd: &RegisterRetirementAccountCommand,
) -> Result<StoredEvent, RetirementError> {
    let events = run(store, user_id, |tx| build_registration_in_txn(tx, cmd))?;
    events
        .into_iter()
        .find(|e| matches!(e.event, Event::RetirementAccountRegistered { .. }))
        .ok_or_else(|| RetirementError::Store("the registration did not land".to_string()))
}

/// Record what a statement says the account is worth, and post the difference.
pub fn set_value(
    store: &mut EventStore,
    user_id: &str,
    cmd: &SetRetirementValueCommand,
) -> Result<ValueSet, RetirementError> {
    let events = run(store, user_id, |tx| build_value_in_txn(tx, cmd))?;
    let book_value_cents = book_value_cents(store.connection(), &cmd.account_id, cmd.as_of)
        - value_posted(&events, &cmd.account_id);
    Ok(ValueSet {
        book_value_cents,
        change_cents: value_posted(&events, &cmd.account_id),
        entry_id: entry_id_of(&events),
    })
}

/// Money in: a plain transfer from the funding account into the sheltered one.
///
/// Not for a payroll deferral or an employer match — see the module docs. Returns
/// the journal entry's id.
pub fn record_contribution(
    store: &mut EventStore,
    user_id: &str,
    cmd: &RetirementContributionCommand,
) -> Result<String, RetirementError> {
    let events = run(store, user_id, |tx| build_contribution_in_txn(tx, cmd))?;
    entry_id_of(&events)
        .ok_or_else(|| RetirementError::Store("no journal entry was posted".to_string()))
}

/// Money out, with tax withheld: gross leaves the retirement account, the net
/// lands in the receiving account and the withholding becomes prepaid tax.
///
/// Posts **no income**, whatever the account's kind. The taxable figure is recorded
/// on the event — see [`RetirementDistributionData`].
pub fn record_distribution(
    store: &mut EventStore,
    user_id: &str,
    cmd: &RetirementDistributionCommand,
) -> Result<Distributed, RetirementError> {
    let events = run(store, user_id, |tx| build_distribution_in_txn(tx, cmd))?;
    let taxable_cents = events
        .iter()
        .find_map(|e| match &e.event {
            Event::RetirementDistributionRecorded(d) => Some(d.taxable_cents),
            _ => None,
        })
        .ok_or_else(|| RetirementError::Store("the distribution did not land".to_string()))?;
    Ok(Distributed {
        entry_id: entry_id_of(&events)
            .ok_or_else(|| RetirementError::Store("no journal entry was posted".to_string()))?,
        net_cents: cmd.gross_cents - cmd.withheld_cents,
        taxable_cents,
    })
}

/// The outcome of a retirement command's in-transaction validation: the events to
/// append as one unit, or a domain rejection. The shape `InvestmentStep` has, for
/// the reason it gives.
///
/// `pub(crate)` along with the `build_*_in_txn` functions below, so the sync
/// command endpoints in `sync::commands::investments` run these very builders
/// inside the server's own append transaction. That is the whole point of the
/// hosted path: a retirement account on a group's books must be fenced by the same
/// out-of-order-statement refusal, the same account-type checks and the same
/// closed-year fence as one in a local file, and a second implementation of them
/// would be a second place for one to be left out.
pub(crate) enum RetirementStep {
    Append(Vec<Event>),
    Reject(RetirementError),
}

/// The append-and-retry loop every command above shares. One copy, exactly as in
/// [`super::investment_commands`].
fn run(
    store: &mut EventStore,
    user_id: &str,
    build: impl Fn(&rusqlite::Transaction<'_>) -> Result<RetirementStep, EventStoreError>,
) -> Result<Vec<StoredEvent>, RetirementError> {
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let outcome = store.append_checked_many(
            head,
            |tx| match build(tx)? {
                RetirementStep::Append(events) => Ok(Verdict::Append(
                    events
                        .into_iter()
                        .map(|e| EventEnvelope::new(e, user_id.to_string()))
                        .collect(),
                )),
                RetirementStep::Reject(e) => Ok(Verdict::Reject(e)),
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
            // checks did not run. Rebuild against fresh state — which matters
            // especially here, because a concurrent contribution changes the book
            // value a value update is measured against.
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

/// The id of the `JournalEntryPosted` in a batch, if there was one.
///
/// `Option`, unlike phase 1's, because a value update that changed nothing posts
/// no entry and that is a success rather than a fault.
fn entry_id_of(events: &[StoredEvent]) -> Option<String> {
    events.iter().find_map(|e| match &e.event {
        Event::JournalEntryPosted { entry_id, .. } => Some(entry_id.clone()),
        _ => None,
    })
}

/// What the batch posted to the retirement account itself.
fn value_posted(events: &[StoredEvent], account_id: &str) -> i64 {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::JournalEntryPosted { lines, .. } => Some(lines),
            _ => None,
        })
        .flatten()
        .filter(|l| l.account_id == account_id)
        .map(|l| l.amount)
        .sum()
}

// ---------------------------------------------------------------------------
// Validation and event building, all of it inside the append transaction
// ---------------------------------------------------------------------------

/// Whether an account exists, and what type it is. `None` when there is no such
/// account.
fn account_type_of(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> Result<Option<String>, EventStoreError> {
    Ok(tx
        .query_row(
            "SELECT account_type FROM accounts WHERE id = ?1",
            [account_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

/// Everything that has to be true before an account becomes a sheltered one.
///
/// Checked under the write lock rather than before it, so two concurrent
/// registrations of the same account cannot both pass and leave the register with
/// two opinions about whether a distribution out of it is taxable.
///
/// The account types are checked because getting the two arguments the wrong way
/// round is an easy mistake and an invisible one: the resulting entries balance
/// perfectly, and the books are nonsense — an income account accumulating the
/// account's whole value and an asset account holding its growth.
fn check_registration_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &RegisterRetirementAccountCommand,
) -> Result<Option<RetirementError>, EventStoreError> {
    if cmd.institution.trim().is_empty() {
        return Ok(Some(RetirementError::Invalid(
            "a sheltered account needs an institution to name it by".to_string(),
        )));
    }
    if cmd.account_id == cmd.value_change_account_id {
        return Ok(Some(RetirementError::Invalid(
            "the retirement account and its value-change account cannot be the same account: \
             every value update would post to itself and change nothing"
                .to_string(),
        )));
    }

    match account_type_of(tx, &cmd.account_id)? {
        None => {
            return Ok(Some(RetirementError::NoSuchAccount(cmd.account_id.clone())));
        }
        Some(t) if t != "asset" => {
            return Ok(Some(RetirementError::NotAnAsset {
                account_id: cmd.account_id.clone(),
                found: t,
            }));
        }
        Some(_) => {}
    }
    match account_type_of(tx, &cmd.value_change_account_id)? {
        None => {
            return Ok(Some(RetirementError::NoSuchAccount(
                cmd.value_change_account_id.clone(),
            )));
        }
        Some(t) if t != "revenue" => {
            return Ok(Some(RetirementError::NotRevenue {
                account_id: cmd.value_change_account_id.clone(),
                found: t,
            }));
        }
        Some(_) => {}
    }

    if let Some(kind) = tx
        .query_row(
            "SELECT kind FROM retirement_accounts WHERE account_id = ?1",
            [&cmd.account_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(Some(RetirementError::AlreadyRegistered {
            account_id: cmd.account_id.clone(),
            kind,
        }));
    }
    // The two roles cannot cross. An account carried at value that is also
    // somebody's value-change account would be forced off every tax line by the
    // non-taxable fence — harmless for an asset account, but it would also make
    // "which accounts are excluded" unanswerable — and, the other way round, a
    // value-change account registered as a retirement account would be carried at
    // value while still receiving another account's growth.
    if tx
        .query_row(
            "SELECT 1 FROM retirement_accounts WHERE value_change_account_id = ?1",
            [&cmd.account_id],
            |_| Ok(true),
        )
        .optional()?
        .is_some()
    {
        return Ok(Some(RetirementError::AlreadyAValueChangeAccount(
            cmd.account_id.clone(),
        )));
    }
    if tx
        .query_row(
            "SELECT 1 FROM retirement_accounts WHERE account_id = ?1",
            [&cmd.value_change_account_id],
            |_| Ok(true),
        )
        .optional()?
        .is_some()
    {
        return Ok(Some(RetirementError::ValueChangeIsARetirementAccount(
            cmd.value_change_account_id.clone(),
        )));
    }
    Ok(None)
}

pub(crate) fn build_registration_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &RegisterRetirementAccountCommand,
) -> Result<RetirementStep, EventStoreError> {
    if let Some(e) = check_registration_in_txn(tx, cmd)? {
        return Ok(RetirementStep::Reject(e));
    }

    Ok(RetirementStep::Append(vec![
        Event::RetirementAccountRegistered {
            account_id: cmd.account_id.clone(),
            institution: cmd.institution.trim().to_string(),
            kind: cmd.kind,
            value_change_account_id: cmd.value_change_account_id.clone(),
        },
        // The first of three fences keeping a non-taxable gain off a tax return
        // (spec §8). This one is the visible record: an explicit `OFF_RETURN`
        // assignment, from `effective_from = 0` — as far back as these books go —
        // so the mapping editor shows the account as deliberately off the return
        // rather than as one nobody has ruled on, and `compute` does not warn
        // about it every year forever.
        //
        // It is not sufficient on its own, which is why the other two exist:
        // `set_account_line` refuses to move it, and
        // `tax::lines::load_effective_mapping` forces it off every line regardless
        // of what the table says. That last one is what defeats inheritance from a
        // mapped parent, a mapping written before the account was registered, and
        // a row that arrived over the sync transport from older code.
        //
        // In the same append batch as the registration, because the two are one
        // fact: an account that is sheltered is an account whose growth is not on
        // the return, and a registration that landed without its exclusion would
        // be a window in which a return could be built wrong.
        Event::TaxLineMappingSet {
            account_id: cmd.value_change_account_id.clone(),
            line_key: crate::tax::lines::OFF_RETURN.to_string(),
            effective_from: 0,
            form: None,
        },
    ]))
}

pub(crate) fn build_value_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &SetRetirementValueCommand,
) -> Result<RetirementStep, EventStoreError> {
    if cmd.value_cents < 0 {
        return Ok(RetirementStep::Reject(RetirementError::Invalid(format!(
            "a retirement account cannot be worth {} cents",
            cmd.value_cents
        ))));
    }
    let Some(account) = registered_in_txn(tx, &cmd.account_id)? else {
        return Ok(RetirementStep::Reject(RetirementError::NotRegistered(
            cmd.account_id.clone(),
        )));
    };
    // A value series that runs backwards posts a fictional loss. The difference
    // would be measured against a book value that already contains everything
    // after `as_of` — including the adjustment the *later* statement posted — so
    // an out-of-order statement subtracts the growth it has already recorded. The
    // same date is allowed: a corrected statement for the period just closed is an
    // ordinary thing, and it measures against the same book value the first one
    // did plus whatever the first one posted, which is exactly right.
    if let Some(last) = account.last_value_as_of {
        if cmd.as_of < last {
            return Ok(RetirementStep::Reject(RetirementError::ValueOutOfOrder {
                account_id: cmd.account_id.clone(),
                as_of: cmd.as_of,
                last,
            }));
        }
    }

    let book = sum_to_date(tx, &cmd.account_id, cmd.as_of)?;
    let change = cmd.value_cents - book;

    let register = Event::RetirementValueSet {
        account_id: cmd.account_id.clone(),
        as_of: cmd.as_of,
        value_cents: cmd.value_cents,
    };

    // Nothing changed. The register still records that the statement was seen —
    // that is what the as-of date is for, and a period nobody confirmed is worth
    // being able to tell from one that was — but there is no entry, because a
    // statement confirming no change is not a journal entry and an entry of zero
    // is a line everybody has to read past forever.
    if change == 0 {
        return Ok(RetirementStep::Append(vec![register]));
    }

    let currency = base_currency(tx)?;
    let memo = cmd.memo.clone().unwrap_or_else(|| {
        format!(
            "{} of {} in {} as of {}",
            if change > 0 { "Growth" } else { "Decline" },
            money(change.abs()),
            account.institution,
            cmd.as_of
        )
    });
    // Signed lines, so one construction covers both directions: growth debits the
    // account and credits the value change, a fall does the reverse. Two branches
    // would be two places for the sign to be wrong, and the sign is the whole
    // content of this entry.
    let lines = vec![
        (cmd.account_id.clone(), change, "Retirement value change"),
        (
            account.value_change_account_id.clone(),
            -change,
            "Retirement value change",
        ),
    ];
    // No reference. A value update is naturally idempotent — re-importing the same
    // statement finds the books already agree and posts nothing — so there is
    // nothing for a reference to protect against, and one keyed on the as-of date
    // would refuse a corrected statement for a period that is legitimately being
    // restated.
    let entry = match entry_or_reject(tx, cmd.as_of, memo, None, &lines, &currency)? {
        Ok(entry) => entry,
        Err(e) => return Ok(RetirementStep::Reject(posting_error(e))),
    };
    Ok(RetirementStep::Append(vec![entry, register]))
}

pub(crate) fn build_contribution_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &RetirementContributionCommand,
) -> Result<RetirementStep, EventStoreError> {
    if cmd.amount_cents <= 0 {
        return Ok(RetirementStep::Reject(RetirementError::Invalid(
            "a contribution of nothing is not a contribution; money coming back out is a \
             distribution"
                .to_string(),
        )));
    }
    if cmd.account_id == cmd.funding_account_id {
        return Ok(RetirementStep::Reject(RetirementError::Invalid(
            "a contribution from an account to itself moves no money".to_string(),
        )));
    }
    let Some(account) = registered_in_txn(tx, &cmd.account_id)? else {
        return Ok(RetirementStep::Reject(RetirementError::NotRegistered(
            cmd.account_id.clone(),
        )));
    };

    let currency = base_currency(tx)?;
    let memo = cmd.memo.clone().unwrap_or_else(|| {
        format!(
            "Contribution of {} to {}",
            money(cmd.amount_cents),
            account.institution
        )
    });
    // A transfer, and nothing more. Both sides are assets, so it nets to zero
    // across the balance sheet: the money changes which account holds it, not how
    // much there is. Whether the contribution is deductible is a question for a
    // personal return and not a posting in these books — and a payroll deferral is
    // not this event at all, see the module docs.
    let lines = vec![
        (cmd.account_id.clone(), cmd.amount_cents, "Contribution"),
        (
            cmd.funding_account_id.clone(),
            -cmd.amount_cents,
            "Contribution",
        ),
    ];
    let entry = match entry_or_reject(tx, cmd.on, memo, cmd.reference.clone(), &lines, &currency)? {
        Ok(entry) => entry,
        Err(e) => return Ok(RetirementStep::Reject(posting_error(e))),
    };

    Ok(RetirementStep::Append(vec![
        entry,
        Event::RetirementContributionRecorded {
            account_id: cmd.account_id.clone(),
            funding_account_id: cmd.funding_account_id.clone(),
            amount_cents: cmd.amount_cents,
            on: cmd.on,
        },
    ]))
}

pub(crate) fn build_distribution_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &RetirementDistributionCommand,
) -> Result<RetirementStep, EventStoreError> {
    if cmd.gross_cents <= 0 {
        return Ok(RetirementStep::Reject(RetirementError::Invalid(
            "a distribution of nothing is not a distribution".to_string(),
        )));
    }
    if cmd.withheld_cents < 0 {
        return Ok(RetirementStep::Reject(RetirementError::Invalid(
            "withholding is an amount, not a direction".to_string(),
        )));
    }
    if cmd.withheld_cents > cmd.gross_cents {
        return Ok(RetirementStep::Reject(RetirementError::Invalid(format!(
            "{} withheld out of a distribution of {}",
            money(cmd.withheld_cents),
            money(cmd.gross_cents)
        ))));
    }
    // The refusal the type documents. A caller who names an income account has
    // reasoned that a taxable distribution needs an income credit, which is a
    // reasonable thing to conclude and wrong here; the error says why rather than
    // ignoring the field.
    if let Some(id) = &cmd.taxable_income_account_id {
        return Ok(RetirementStep::Reject(
            RetirementError::NoIncomeAccountOnADistribution(id.clone()),
        ));
    }
    let Some(account) = registered_in_txn(tx, &cmd.account_id)? else {
        return Ok(RetirementStep::Reject(RetirementError::NotRegistered(
            cmd.account_id.clone(),
        )));
    };

    // Box 2a. From the kind unless the caller knows better: the whole gross out of
    // pre-tax money, nothing out of a qualified Roth, and a refusal for 'other',
    // because whether a 529 or HSA distribution is taxable turns on what the money
    // was spent on and no ledger holds that. Guessing zero there would understate
    // income on a return; guessing the gross would overstate it. Neither is a
    // guess worth making silently.
    let taxable_cents = match cmd.taxable_cents {
        Some(n) => n,
        None => match account.kind {
            RetirementKind::Traditional => cmd.gross_cents,
            RetirementKind::Roth => 0,
            RetirementKind::Other => {
                return Ok(RetirementStep::Reject(
                    RetirementError::TaxableAmountNotStated {
                        account_id: cmd.account_id.clone(),
                        kind: account.kind.as_str().to_string(),
                    },
                ))
            }
        },
    };
    if taxable_cents < 0 || taxable_cents > cmd.gross_cents {
        return Ok(RetirementStep::Reject(RetirementError::Invalid(format!(
            "a taxable amount of {} does not fit inside a distribution of {}",
            money(taxable_cents),
            money(cmd.gross_cents)
        ))));
    }

    let currency = base_currency(tx)?;
    let memo = cmd.memo.clone().unwrap_or_else(|| {
        format!(
            "Distribution of {} from {}",
            money(cmd.gross_cents),
            account.institution
        )
    });
    // Assets only, on both sides, and that is the whole argument for why no income
    // is posted: the gross leaves the retirement account and arrives as a bank
    // balance plus a prepaid tax. Nothing is earned here — it was earned, and
    // recognised, when the value was set (see `RetirementDistributionData`).
    //
    // The receiving line is built even when it is zero, which happens on a
    // distribution taken entirely to cover tax: the entry then has three lines
    // and one of them is nothing, which is a truer record than two lines that
    // hide where the money was meant to go.
    let mut lines = vec![
        (
            cmd.receiving_account_id.clone(),
            cmd.gross_cents - cmd.withheld_cents,
            "Distribution received",
        ),
        (cmd.account_id.clone(), -cmd.gross_cents, "Distribution"),
    ];
    if cmd.withheld_cents > 0 {
        lines.insert(
            1,
            (
                cmd.withheld_account_id.clone(),
                cmd.withheld_cents,
                "Income tax withheld",
            ),
        );
    }
    let entry = match entry_or_reject(tx, cmd.on, memo, cmd.reference.clone(), &lines, &currency)? {
        Ok(entry) => entry,
        Err(e) => return Ok(RetirementStep::Reject(posting_error(e))),
    };

    Ok(RetirementStep::Append(vec![
        entry,
        Event::RetirementDistributionRecorded(Box::new(RetirementDistributionData {
            account_id: cmd.account_id.clone(),
            receiving_account_id: cmd.receiving_account_id.clone(),
            gross_cents: cmd.gross_cents,
            withheld_cents: cmd.withheld_cents,
            withheld_account_id: cmd.withheld_account_id.clone(),
            taxable_cents,
            on: cmd.on,
        })),
    ]))
}

// ---------------------------------------------------------------------------
// Shared in-transaction helpers
// ---------------------------------------------------------------------------

/// The register row for an account, read under the write lock.
fn registered_in_txn(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> Result<Option<RetirementAccount>, EventStoreError> {
    Ok(get_account(tx, account_id))
}

fn base_currency(tx: &rusqlite::Transaction<'_>) -> Result<String, EventStoreError> {
    Ok(tx
        .query_row("SELECT base_currency FROM company LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .unwrap_or_else(|| "USD".to_string()))
}

/// Cents as a person reads them, for memos and messages.
fn money(cents: i64) -> String {
    format!("${:.2}", cents as f64 / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AccountType;
    use crate::store::migrations::SchemaStore;

    // The chart spec §2b describes, plus the two accounts a distribution needs.
    const CHECKING: &str = "1000";
    const PREPAID_TAX: &str = "1400";
    const IRA: &str = "1500";
    const ROTH: &str = "1510";
    const FIVE29: &str = "1520";
    const VALUE_CHANGE: &str = "4130";
    // A taxable investment income account, for the mapping tests: it is what a
    // value-change account must never be treated as.
    const DIVIDENDS: &str = "4100";
    const INVESTMENT_INCOME: &str = "4000";

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Dollars as cents, so the tests read in dollars and assert in cents.
    fn usd(dollars: i64) -> i64 {
        dollars * 100
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
        for (id, name, kind, parent) in [
            (CHECKING, "Checking", AccountType::Asset, None),
            (PREPAID_TAX, "Prepaid income tax", AccountType::Asset, None),
            (IRA, "Fidelity IRA ••5678", AccountType::Asset, None),
            (ROTH, "Vanguard Roth ••9012", AccountType::Asset, None),
            (FIVE29, "Bright Start 529", AccountType::Asset, None),
            (
                INVESTMENT_INCOME,
                "Investment income",
                AccountType::Revenue,
                None,
            ),
            (
                DIVIDENDS,
                "Dividends",
                AccountType::Revenue,
                Some(INVESTMENT_INCOME),
            ),
            (
                VALUE_CHANGE,
                "Retirement value change",
                AccountType::Revenue,
                Some(INVESTMENT_INCOME),
            ),
        ] {
            crate::commands::partnership_commands::append_event_locally(
                &mut store,
                "u",
                Event::AccountCreated {
                    account_id: id.into(),
                    account_type: kind.into(),
                    account_number: id.into(),
                    name: name.into(),
                    parent_id: parent.map(|p: &str| p.to_string()),
                    currency: Some("USD".into()),
                    description: None,
                },
            )
            .expect("account");
        }
        store
    }

    fn register(store: &mut EventStore, account_id: &str, kind: RetirementKind) -> StoredEvent {
        register_account(
            store,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: account_id.to_string(),
                institution: format!("Custodian for {account_id}"),
                kind,
                value_change_account_id: VALUE_CHANGE.to_string(),
            },
        )
        .expect("registered")
    }

    fn set(store: &mut EventStore, account_id: &str, value: i64, as_of: NaiveDate) -> ValueSet {
        set_value(
            store,
            "u",
            &SetRetirementValueCommand {
                account_id: account_id.to_string(),
                as_of,
                value_cents: value,
                memo: None,
            },
        )
        .expect("value set")
    }

    fn contribute(store: &mut EventStore, account_id: &str, amount: i64, on: NaiveDate) -> String {
        record_contribution(
            store,
            "u",
            &RetirementContributionCommand {
                account_id: account_id.to_string(),
                funding_account_id: CHECKING.to_string(),
                amount_cents: amount,
                on,
                memo: None,
                reference: None,
            },
        )
        .expect("contributed")
    }

    fn distribution(
        account_id: &str,
        gross: i64,
        withheld: i64,
        on: NaiveDate,
    ) -> RetirementDistributionCommand {
        RetirementDistributionCommand {
            account_id: account_id.to_string(),
            receiving_account_id: CHECKING.to_string(),
            gross_cents: gross,
            withheld_cents: withheld,
            withheld_account_id: PREPAID_TAX.to_string(),
            taxable_income_account_id: None,
            taxable_cents: None,
            on,
            memo: None,
            reference: None,
        }
    }

    /// The net amount one entry posted to one account. Positive is a debit.
    fn amount_on(store: &EventStore, entry_id: &str, account: &str) -> i64 {
        store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(amount), 0) FROM journal_lines
                  WHERE entry_id = ?1 AND account_id = ?2",
                rusqlite::params![entry_id, account],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// An account's whole balance. Positive is a debit.
    fn balance(store: &EventStore, account: &str) -> i64 {
        store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(amount), 0) FROM journal_lines WHERE account_id = ?1",
                [account],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Every entry in the books sums to zero, which is the one thing that must be
    /// true of all of them however they were built.
    fn every_entry_balances(store: &EventStore) {
        let mut stmt = store
            .connection()
            .prepare(
                "SELECT entry_id, SUM(amount) FROM journal_lines
                  GROUP BY entry_id HAVING SUM(amount) != 0",
            )
            .unwrap();
        let bad: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .flatten()
            .collect();
        assert!(bad.is_empty(), "these entries do not balance: {bad:?}");
    }

    fn entry_count(store: &EventStore) -> i64 {
        store
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_entries", [], |r| r.get(0))
            .unwrap()
    }

    // -----------------------------------------------------------------------
    // Registering
    // -----------------------------------------------------------------------

    #[test]
    fn registering_an_account_puts_it_on_the_register_with_no_value_yet() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);

        let account = get_account(s.connection(), IRA).expect("on the register");
        assert_eq!(account.institution, format!("Custodian for {IRA}"));
        assert_eq!(account.kind, RetirementKind::Traditional);
        assert_eq!(account.value_change_account_id, VALUE_CHANGE);
        assert_eq!(
            (account.last_value_cents, account.last_value_as_of),
            (None, None),
            "a fresh account has no statement value, which is not the same as zero"
        );
        assert_eq!(entry_count(&s), 0, "a registration posts nothing");
    }

    #[test]
    fn registering_the_same_account_twice_is_refused() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        let again = register_account(
            &mut s,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: IRA.to_string(),
                institution: "Fidelity".into(),
                kind: RetirementKind::Roth,
                value_change_account_id: VALUE_CHANGE.to_string(),
            },
        );
        assert!(
            matches!(again, Err(RetirementError::AlreadyRegistered { .. })),
            "got {again:?}"
        );
        assert_eq!(
            get_account(s.connection(), IRA).unwrap().kind,
            RetirementKind::Traditional,
            "the refused registration must not have changed the kind"
        );
    }

    /// Two sheltered accounts sharing one value-change account is spec §2b's own
    /// chart, not an error.
    #[test]
    fn two_accounts_may_share_one_value_change_account() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        register(&mut s, ROTH, RetirementKind::Roth);
        assert_eq!(list_accounts(s.connection()).len(), 2);
        assert_eq!(
            value_change_account_ids(s.connection()),
            BTreeSet::from([VALUE_CHANGE.to_string()])
        );
    }

    /// Swapping the two arguments produces entries that balance perfectly and books
    /// that are nonsense, so the types are checked.
    #[test]
    fn the_account_types_have_to_be_the_right_way_round() {
        let mut s = store();
        let swapped = register_account(
            &mut s,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: VALUE_CHANGE.to_string(),
                institution: "Fidelity".into(),
                kind: RetirementKind::Traditional,
                value_change_account_id: IRA.to_string(),
            },
        );
        assert!(
            matches!(swapped, Err(RetirementError::NotAnAsset { .. })),
            "got {swapped:?}"
        );

        let no_such = register_account(
            &mut s,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: "9999".to_string(),
                institution: "Fidelity".into(),
                kind: RetirementKind::Roth,
                value_change_account_id: VALUE_CHANGE.to_string(),
            },
        );
        assert!(
            matches!(no_such, Err(RetirementError::NoSuchAccount(_))),
            "got {no_such:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Value updates
    // -----------------------------------------------------------------------

    #[test]
    fn a_value_increase_debits_the_account_and_credits_the_value_change() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));

        let set = set(&mut s, IRA, usd(102_500), day(2026, 1, 31));
        assert_eq!(set.book_value_cents, 10_000_000);
        assert_eq!(set.change_cents, 250_000, "$2,500 of growth");
        let entry = set.entry_id.expect("an entry was posted");
        assert_eq!(amount_on(&s, &entry, IRA), 250_000, "debit the account");
        assert_eq!(
            amount_on(&s, &entry, VALUE_CHANGE),
            -250_000,
            "credit the value change"
        );
        assert_eq!(balance(&s, IRA), 10_250_000, "carried at value");
        every_entry_balances(&s);

        let account = get_account(s.connection(), IRA).unwrap();
        assert_eq!(account.last_value_cents, Some(10_250_000));
        assert_eq!(account.last_value_as_of, Some(day(2026, 1, 31)));
    }

    #[test]
    fn a_value_decrease_credits_the_account_and_debits_the_value_change() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));
        set(&mut s, IRA, usd(102_500), day(2026, 1, 31));

        let set = set(&mut s, IRA, usd(99_000), day(2026, 2, 28));
        assert_eq!(set.book_value_cents, 10_250_000);
        assert_eq!(set.change_cents, -350_000, "$3,500 lost");
        let entry = set.entry_id.expect("an entry was posted");
        assert_eq!(amount_on(&s, &entry, IRA), -350_000, "credit the account");
        assert_eq!(
            amount_on(&s, &entry, VALUE_CHANGE),
            350_000,
            "debit the value change — a loss is a debit back against it"
        );
        assert_eq!(balance(&s, IRA), 9_900_000);
        every_entry_balances(&s);
    }

    /// A statement that confirms nothing changed is not a journal entry.
    #[test]
    fn an_unchanged_value_records_the_statement_and_posts_nothing() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));
        set(&mut s, IRA, usd(102_500), day(2026, 1, 31));
        let before = entry_count(&s);

        let again = set(&mut s, IRA, usd(102_500), day(2026, 2, 28));
        assert_eq!(again.change_cents, 0);
        assert_eq!(again.entry_id, None, "no entry for no change");
        assert_eq!(entry_count(&s), before, "and none appeared");
        // The register still records that the statement was seen.
        assert_eq!(
            get_account(s.connection(), IRA).unwrap().last_value_as_of,
            Some(day(2026, 2, 28))
        );
    }

    /// The reason the book value comes from the ledger and not from the last figure
    /// the register stored: a contribution in between is not growth.
    #[test]
    fn a_contribution_between_statements_is_not_counted_as_growth() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));
        set(&mut s, IRA, usd(100_000), day(2026, 1, 31));
        contribute(&mut s, IRA, usd(1_000), day(2026, 2, 15));

        let set = set(&mut s, IRA, usd(102_000), day(2026, 2, 28));
        assert_eq!(
            set.change_cents, 100_000,
            "$1,000 of growth, not $2,000: the other $1,000 was contributed"
        );
        assert_eq!(balance(&s, VALUE_CHANGE), -100_000);
        every_entry_balances(&s);
    }

    #[test]
    fn a_value_dated_before_the_last_one_is_refused() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));
        set(&mut s, IRA, usd(102_500), day(2026, 3, 31));
        let before = entry_count(&s);

        let backwards = set_value(
            &mut s,
            "u",
            &SetRetirementValueCommand {
                account_id: IRA.to_string(),
                as_of: day(2026, 2, 28),
                value_cents: usd(101_000),
                memo: None,
            },
        );
        assert!(
            matches!(backwards, Err(RetirementError::ValueOutOfOrder { .. })),
            "got {backwards:?}"
        );
        assert_eq!(entry_count(&s), before, "and nothing was posted");
        assert_eq!(
            get_account(s.connection(), IRA).unwrap().last_value_as_of,
            Some(day(2026, 3, 31)),
            "the register is untouched"
        );

        // The same date is fine: a corrected statement for the period just closed.
        let corrected = set(&mut s, IRA, usd(103_000), day(2026, 3, 31));
        assert_eq!(corrected.change_cents, 50_000);
        every_entry_balances(&s);
    }

    #[test]
    fn a_value_for_an_unregistered_account_is_refused() {
        let mut s = store();
        let orphan = set_value(
            &mut s,
            "u",
            &SetRetirementValueCommand {
                account_id: IRA.to_string(),
                as_of: day(2026, 1, 31),
                value_cents: usd(1_000),
                memo: None,
            },
        );
        assert!(
            matches!(orphan, Err(RetirementError::NotRegistered(_))),
            "got {orphan:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Contributions
    // -----------------------------------------------------------------------

    #[test]
    fn a_contribution_is_a_transfer_and_nets_to_zero_across_assets() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        let entry = contribute(&mut s, IRA, usd(7_000), day(2026, 4, 10));

        assert_eq!(amount_on(&s, &entry, IRA), 700_000, "debit the IRA");
        assert_eq!(
            amount_on(&s, &entry, CHECKING),
            -700_000,
            "credit the funding account"
        );
        assert_eq!(
            balance(&s, VALUE_CHANGE),
            0,
            "a contribution is not growth and touches no income account"
        );
        every_entry_balances(&s);
    }

    #[test]
    fn a_contribution_of_nothing_is_refused() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        let nothing = record_contribution(
            &mut s,
            "u",
            &RetirementContributionCommand {
                account_id: IRA.to_string(),
                funding_account_id: CHECKING.to_string(),
                amount_cents: 0,
                on: day(2026, 4, 10),
                memo: None,
                reference: None,
            },
        );
        assert!(
            matches!(nothing, Err(RetirementError::Invalid(_))),
            "got {nothing:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Distributions
    // -----------------------------------------------------------------------

    #[test]
    fn a_roth_distribution_with_withholding_moves_assets_and_taxes_nothing() {
        let mut s = store();
        register(&mut s, ROTH, RetirementKind::Roth);
        contribute(&mut s, ROTH, usd(50_000), day(2026, 1, 5));

        let out = record_distribution(
            &mut s,
            "u",
            &distribution(ROTH, usd(10_000), usd(1_000), day(2026, 6, 1)),
        )
        .expect("distributed");

        assert_eq!(out.net_cents, 900_000);
        assert_eq!(
            out.taxable_cents, 0,
            "a qualified Roth distribution is not income"
        );
        assert_eq!(amount_on(&s, &out.entry_id, CHECKING), 900_000);
        assert_eq!(
            amount_on(&s, &out.entry_id, PREPAID_TAX),
            100_000,
            "withholding is a prepaid tax asset, not an expense"
        );
        assert_eq!(amount_on(&s, &out.entry_id, ROTH), -1_000_000, "gross out");
        assert_eq!(balance(&s, ROTH), 4_000_000);
        assert_eq!(
            balance(&s, VALUE_CHANGE),
            0,
            "a distribution posts no income at all"
        );
        every_entry_balances(&s);
    }

    /// The whole of the Traditional question: the taxable amount is recorded, not
    /// posted, and it comes back out of the log for the 1099-R.
    #[test]
    fn a_traditional_distribution_records_its_taxable_amount_and_posts_no_income() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));
        set(&mut s, IRA, usd(110_000), day(2026, 5, 31));

        let out = record_distribution(
            &mut s,
            "u",
            &distribution(IRA, usd(20_000), usd(4_000), day(2026, 6, 1)),
        )
        .expect("distributed");

        assert_eq!(out.taxable_cents, 2_000_000, "box 2a is the whole gross");
        assert_eq!(out.net_cents, 1_600_000);
        assert_eq!(amount_on(&s, &out.entry_id, CHECKING), 1_600_000);
        assert_eq!(amount_on(&s, &out.entry_id, PREPAID_TAX), 400_000);
        assert_eq!(amount_on(&s, &out.entry_id, IRA), -2_000_000);

        // No income anywhere. The $10,000 of growth was recognised when the value
        // was set, and that is the only place it appears.
        assert_eq!(
            balance(&s, VALUE_CHANGE),
            -1_000_000,
            "the growth, recognised once"
        );
        assert_eq!(balance(&s, DIVIDENDS), 0);
        let income_lines: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM journal_lines jl
                   JOIN accounts a ON a.id = jl.account_id
                  WHERE jl.entry_id = ?1 AND a.account_type = 'revenue'",
                [&out.entry_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            income_lines, 0,
            "a distribution entry touches no income account"
        );
        every_entry_balances(&s);

        // And the 1099-R figure is recoverable from the log alone.
        let filed = list_distributions(&s).expect("read back");
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].gross_cents, 2_000_000);
        assert_eq!(filed[0].taxable_cents, 2_000_000);
        assert_eq!(filed[0].withheld_cents, 400_000);
        assert_eq!(filed[0].on, day(2026, 6, 1));
        assert_eq!(filed[0].withheld_account_id, PREPAID_TAX);
    }

    /// The refusal the type documents: naming an income account would double-count
    /// the same dollar.
    #[test]
    fn a_distribution_may_not_name_an_income_account() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));

        let mut cmd = distribution(IRA, usd(20_000), 0, day(2026, 6, 1));
        cmd.taxable_income_account_id = Some(DIVIDENDS.to_string());
        let refused = record_distribution(&mut s, "u", &cmd);
        assert!(
            matches!(
                refused,
                Err(RetirementError::NoIncomeAccountOnADistribution(_))
            ),
            "got {refused:?}"
        );
        assert_eq!(entry_count(&s), 1, "only the contribution was posted");
    }

    /// A 529 or an HSA is taxed on what the money was spent on, which no ledger
    /// holds, so the register refuses to guess.
    #[test]
    fn an_other_kind_distribution_makes_the_caller_state_the_taxable_amount() {
        let mut s = store();
        register(&mut s, FIVE29, RetirementKind::Other);
        contribute(&mut s, FIVE29, usd(30_000), day(2026, 1, 5));

        let guessed = record_distribution(
            &mut s,
            "u",
            &distribution(FIVE29, usd(5_000), 0, day(2026, 8, 1)),
        );
        assert!(
            matches!(guessed, Err(RetirementError::TaxableAmountNotStated { .. })),
            "got {guessed:?}"
        );

        let mut stated = distribution(FIVE29, usd(5_000), 0, day(2026, 8, 1));
        stated.taxable_cents = Some(usd(1_200));
        let out = record_distribution(&mut s, "u", &stated).expect("distributed");
        assert_eq!(out.taxable_cents, 120_000, "the earnings part, as stated");
        assert_eq!(amount_on(&s, &out.entry_id, FIVE29), -500_000);
        every_entry_balances(&s);
    }

    #[test]
    fn withholding_larger_than_the_distribution_is_refused() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));

        let bad = record_distribution(
            &mut s,
            "u",
            &distribution(IRA, usd(1_000), usd(1_500), day(2026, 6, 1)),
        );
        assert!(
            matches!(bad, Err(RetirementError::Invalid(_))),
            "got {bad:?}"
        );

        // Equal is legitimate: a distribution taken entirely to cover tax.
        let all_tax = record_distribution(
            &mut s,
            "u",
            &distribution(IRA, usd(1_000), usd(1_000), day(2026, 6, 1)),
        )
        .expect("a distribution taken entirely as withholding");
        assert_eq!(all_tax.net_cents, 0);
        assert_eq!(amount_on(&s, &all_tax.entry_id, PREPAID_TAX), 100_000);
        assert_eq!(amount_on(&s, &all_tax.entry_id, CHECKING), 0);
        every_entry_balances(&s);
    }

    // -----------------------------------------------------------------------
    // The non-taxable fence (spec §8) — the most important thing in phase 2
    // -----------------------------------------------------------------------

    /// Registering an account excludes its value-change account from the return,
    /// explicitly and from as far back as the books go.
    #[test]
    fn registering_excludes_the_value_change_account_from_the_return() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        let stored = crate::tax::lines::load_mapping(s.connection(), 2026);
        assert_eq!(
            stored.get(VALUE_CHANGE).map(String::as_str),
            Some(crate::tax::lines::OFF_RETURN),
            "the exclusion is a row somebody can see, not only a filter"
        );
    }

    /// The fence itself. Every route onto a tax line is closed: mapping it
    /// directly, inheriting it from a mapped parent, and a row written before the
    /// account was registered.
    #[test]
    fn a_value_change_account_cannot_end_up_on_a_tax_line() {
        let mut s = store();

        // A mapping written *before* the account is a value-change account — which
        // is the ordering no command-level check can catch.
        crate::commands::tax_setup_commands::set_account_line(
            &mut s,
            "u",
            VALUE_CHANGE,
            "l7",
            2026,
        )
        .expect("nothing says otherwise yet");
        // And the parent mapped too, so inheritance would reach the child even if
        // the row above were removed.
        crate::commands::tax_setup_commands::set_account_line(
            &mut s,
            "u",
            INVESTMENT_INCOME,
            "l7",
            2026,
        )
        .expect("an ordinary mapping");

        register(&mut s, IRA, RetirementKind::Traditional);

        // Mapping it directly is now refused, with a reason.
        let refused = crate::commands::tax_setup_commands::set_account_line(
            &mut s,
            "u",
            VALUE_CHANGE,
            "l7",
            2026,
        );
        assert!(refused.is_err(), "mapping it must be refused");
        // Taking it off the return is still allowed: that is not a mapping.
        crate::commands::tax_setup_commands::set_account_line(
            &mut s,
            "u",
            VALUE_CHANGE,
            crate::tax::lines::OFF_RETURN,
            2026,
        )
        .expect("saying no again is allowed");

        // And whatever the table holds, the return sees it off every line.
        let effective = crate::tax::lines::load_effective_mapping(s.connection(), 2026);
        assert_eq!(
            effective.get(VALUE_CHANGE).map(String::as_str),
            Some(crate::tax::lines::OFF_RETURN),
            "a non-taxable gain reaching a tax return is a filed error"
        );
        // The sibling that *is* taxable still inherits line 7, so the fence is
        // narrow: it excludes this account and not the branch it sits in.
        assert_eq!(
            effective.get(DIVIDENDS).map(String::as_str),
            Some("l7"),
            "an ordinary income account beside it is unaffected"
        );
    }

    /// The nastiest route, on its own: nobody ever names the value-change account,
    /// and its parent's mapping would carry it onto line 7 by inheritance.
    #[test]
    fn inheritance_from_a_mapped_parent_cannot_reach_a_value_change_account() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        crate::commands::tax_setup_commands::set_account_line(
            &mut s,
            "u",
            INVESTMENT_INCOME,
            "l7",
            2026,
        )
        .unwrap();
        // Remove the explicit exclusion, so inheritance is the only thing left.
        crate::commands::tax_setup_commands::clear_account_line(&mut s, "u", VALUE_CHANGE, 0)
            .unwrap();
        assert_eq!(
            crate::tax::lines::load_mapping(s.connection(), 2026).get(VALUE_CHANGE),
            None,
            "the fixture really has no row of its own"
        );

        let effective = crate::tax::lines::load_effective_mapping(s.connection(), 2026);
        assert_eq!(
            effective.get(VALUE_CHANGE).map(String::as_str),
            Some(crate::tax::lines::OFF_RETURN),
            "inherited onto line 7 is exactly the silent failure §8 exists to stop"
        );
    }

    /// Non-taxable growth must not reach the income the return is built from.
    #[test]
    fn retirement_growth_reaches_no_line_of_the_return() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));
        set(&mut s, IRA, usd(140_000), day(2026, 12, 31));
        // A real, taxable dividend beside it, so the test proves exclusion rather
        // than an empty return.
        crate::commands::tax_setup_commands::set_account_line(
            &mut s,
            "u",
            INVESTMENT_INCOME,
            "l7",
            2026,
        )
        .unwrap();

        let mapping = crate::tax::lines::load_effective_mapping(s.connection(), 2026);
        assert_eq!(
            mapping.get(VALUE_CHANGE).map(String::as_str),
            Some(crate::tax::lines::OFF_RETURN)
        );
        assert_eq!(
            balance(&s, VALUE_CHANGE),
            -4_000_000,
            "$40,000 of growth in the books…"
        );
        assert!(
            !mapping
                .iter()
                .any(|(a, k)| a == VALUE_CHANGE && k != crate::tax::lines::OFF_RETURN),
            "…and on no line of the return"
        );
    }

    // -----------------------------------------------------------------------
    // The log is the truth
    // -----------------------------------------------------------------------

    /// The point of event-sourcing the register: a second machine replaying the log
    /// arrives at the same register, including the exclusion.
    #[test]
    fn the_register_rebuilds_from_the_log_identically() {
        let mut s = store();
        register(&mut s, IRA, RetirementKind::Traditional);
        register(&mut s, ROTH, RetirementKind::Roth);
        contribute(&mut s, IRA, usd(100_000), day(2026, 1, 5));
        set(&mut s, IRA, usd(102_500), day(2026, 1, 31));
        set(&mut s, ROTH, 0, day(2026, 1, 31));
        record_distribution(
            &mut s,
            "u",
            &distribution(IRA, usd(20_000), usd(4_000), day(2026, 6, 1)),
        )
        .unwrap();

        let before = list_accounts(s.connection());
        let mapping_before = crate::tax::lines::load_effective_mapping(s.connection(), 2026);

        let events = s.get_all().unwrap();
        Projector::new(s.connection()).rebuild(&events).unwrap();

        assert_eq!(
            list_accounts(s.connection()),
            before,
            "a replay must reproduce the register exactly"
        );
        assert_eq!(
            crate::tax::lines::load_effective_mapping(s.connection(), 2026),
            mapping_before,
            "including the exclusion, which a replay that dropped it would turn into a filed error"
        );
        every_entry_balances(&s);
        assert_eq!(
            list_distributions(&s).unwrap().len(),
            1,
            "and the 1099-R figures are still in the log"
        );
    }
}
