//! Investment and retirement command endpoints over the sync transport.
//!
//! The contract is `sync::commands::account`'s, unchanged: each endpoint is
//! bearer-authenticated (`AuthedUser`), honors the client's `expected_head_seq`,
//! calls `append_checked_many` **once** with no internal retry so a `HeadMismatch`
//! surfaces as a `409`, and maps the outcome to 200 + new head / 409 stale head /
//! 422 domain rejection.
//!
//! # Why these exist at all
//!
//! INVESTMENTS-SPEC.md §10 left "does the group-hosted case matter for
//! investments?" open, and the answer turned out to be yes: the books that needed
//! phases 1–5 first are hosted ones. Until this module there was no honest way to
//! use them there, and the two that looked available were both worse than the
//! refusal the desktop was showing:
//!
//! * appending locally forks the replica's log, because the event would take a
//!   sequence number the server will also hand out;
//! * posting the *entry* through `post-entry` would put a purchase in the books
//!   with **no lot** behind it, so the sale of it years later would have no basis
//!   to relieve — a trial balance that balances and a Form 8949 that is wrong.
//!
//! # Not one invariant is re-implemented here
//!
//! Every handler below calls the same `build_*_in_txn` the local command calls, so
//! FIFO, specific-lot validation, the empty-lot and over-sale refusals, the
//! account-type checks, the out-of-order-statement refusal, the reference
//! uniqueness and the closed-year fence all run **inside the server's append
//! transaction** against locked state. `hosted_and_local_paths_post_the_same_entries`
//! is what holds that to be true rather than merely intended.
//!
//! # Why the responses carry more than a head
//!
//! A buy mints a lot id, a sale a sale id and a realized gain, a define a security
//! id — and the desktop shows them. A client that had to re-read the log to learn
//! what it just did would be racing its own replica pull: our write comes back
//! through the same pull path as everybody else's, so for up to a tick the replica
//! does not contain the thing the server just created. So the ids are minted by the
//! handler (or read off the events it appended) and returned in the response.

use crate::commands::investment_commands::{
    build_buy_in_txn, build_define_security_in_txn, build_fee_in_txn, build_income_in_txn,
    build_sell_in_txn, BuySecurityCommand, ChargeInvestmentFeeCommand, InvestmentError,
    InvestmentStep, LotSelection, NewSecurity, RecordInvestmentIncomeCommand, SellSecurityCommand,
};
use crate::commands::investment_import::{
    build_configure_in_txn, build_import_in_txn, build_resolve_security_in_txn,
    build_snapshot_in_txn, classify_subtype, ConfigureInvestmentAccountCommand, ImportError,
    ImportRecord, ImportStep, MasterSecurity, PlannedWrite,
};
use crate::commands::retirement_commands::{
    build_contribution_in_txn, build_distribution_in_txn, build_registration_in_txn,
    build_value_in_txn, RegisterRetirementAccountCommand, RetirementContributionCommand,
    RetirementDistributionCommand, RetirementError, RetirementStep, SetRetirementValueCommand,
};
use crate::events::types::{
    Event, HoldingsSnapshotData, InvestmentIncomeKind, InvestmentPostingAccounts, RetirementKind,
    SaleLotData,
};
use crate::store::event_store::{EventStoreError, Verdict};
use crate::sync::{
    outcome_to_response_many, project, stamp, ApiError, AuthedUser, SubmitResponse, SyncState,
};
use axum::{extract::State, routing::post, Json, Router};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/define-security", post(submit_define_security))
        .route("/sync/commands/buy-security", post(submit_buy_security))
        .route("/sync/commands/sell-security", post(submit_sell_security))
        .route(
            "/sync/commands/record-investment-income",
            post(submit_record_income),
        )
        .route(
            "/sync/commands/charge-investment-fee",
            post(submit_charge_fee),
        )
        .route(
            "/sync/commands/configure-investment-account",
            post(submit_configure_account),
        )
        .route(
            "/sync/commands/register-retirement-account",
            post(submit_register_retirement_account),
        )
        .route(
            "/sync/commands/set-retirement-value",
            post(submit_set_retirement_value),
        )
        .route(
            "/sync/commands/record-retirement-contribution",
            post(submit_record_contribution),
        )
        .route(
            "/sync/commands/record-retirement-distribution",
            post(submit_record_distribution),
        )
        // The importer's own three (phase 4 over the transport). Everything above is
        // a command a person gives; these three are what a provider payload becomes,
        // and they exist because an import that appended locally would fork a
        // replica's log exactly as a hand-entered trade would.
        .route(
            "/sync/commands/resolve-plaid-security",
            post(submit_resolve_plaid_security),
        )
        .route(
            "/sync/commands/import-investment-activity",
            post(submit_import_investment_activity),
        )
        .route(
            "/sync/commands/record-holdings-snapshot",
            post(submit_record_holdings_snapshot),
        )
}

// ---------------------------------------------------------------------------
// Reading what the transaction decided
// ---------------------------------------------------------------------------

/// A slot the append closure fills and the handler reads afterwards.
///
/// The shape `submit_ensure_account_path` uses, and for the same reason: the facts
/// a response needs — which entry was posted, what a value update came to — are
/// decided *inside* the transaction, and the handler cannot recompute them
/// afterwards without reading state another writer may already have moved.
type Sink<T> = std::sync::Arc<std::sync::Mutex<Option<T>>>;

fn sink<T>() -> Sink<T> {
    Default::default()
}

/// Take what the closure recorded, or fail loudly.
///
/// An empty sink after a successful append means the closure appended without
/// recording, which is a bug in this module and not a client error — so it is a
/// 500 with a sentence, not a response with a plausible-looking zero in it.
fn taken<T>(slot: &Sink<T>, what: &str) -> Result<T, ApiError> {
    slot.lock()
        .ok()
        .and_then(|mut s| s.take())
        .ok_or_else(|| {
            ApiError::store(EventStoreError::Backend(format!(
                "the append landed without recording {what}"
            )))
        })
}

/// The id of the `JournalEntryPosted` in a batch, read off the event rather than
/// minted by the caller — so the id reported is the one the ledger holds.
fn entry_id_of(events: &[Event]) -> Option<String> {
    events.iter().find_map(|e| match e {
        Event::JournalEntryPosted { entry_id, .. } => Some(entry_id.clone()),
        _ => None,
    })
}

/// What a batch posted to one account. `retirement_commands::value_posted`'s
/// arithmetic, over bare events rather than stored ones.
fn posted_to(events: &[Event], account_id: &str) -> i64 {
    events
        .iter()
        .filter_map(|e| match e {
            Event::JournalEntryPosted { lines, .. } => Some(lines),
            _ => None,
        })
        .flatten()
        .filter(|l| l.account_id == account_id)
        .map(|l| l.amount)
        .sum()
}

// ---------------------------------------------------------------------------
// define-security
// ---------------------------------------------------------------------------

/// Put a security on the group's master.
#[derive(Serialize, Deserialize)]
pub struct DefineSecurityRequest {
    pub expected_head_seq: i64,
    pub ticker: String,
    pub name: String,
    /// "stock", "etf", "mutual fund"… free text, as the event carries it.
    pub kind: String,
    #[serde(default)]
    pub cusip: Option<String>,
    /// `None` means USD, which is what the builder also decides. Sent rather than
    /// defaulted here so a future multi-currency client needs no new endpoint.
    #[serde(default)]
    pub currency: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct DefineSecurityResponse {
    pub head: i64,
    /// Minted by the server. The client needs it to reference the security in the
    /// very next command, before its replica has pulled the event.
    pub security_id: String,
}

/// Define a security, validated server-side.
///
/// The ticker's uniqueness is re-checked inside the append transaction, under the
/// write lock, which is exactly why this cannot be a local append followed by a
/// push: two members defining AAPL at the same moment would otherwise both pass
/// and split one holding across two masters that neither add up on the balance
/// sheet nor reconcile against a 1099-B. A taken ticker is a `422` naming the
/// security that already has it.
async fn submit_define_security(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<DefineSecurityRequest>,
) -> Result<Json<DefineSecurityResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let security_id = Uuid::new_v4().to_string();
    let security = NewSecurity {
        ticker: req.ticker,
        name: req.name,
        kind: req.kind,
        cusip: req.cusip,
        currency: req.currency.unwrap_or_else(|| "USD".to_string()),
    };
    let id = security_id.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_define_security_in_txn(tx, &id, &security)? {
                InvestmentStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                InvestmentStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<InvestmentError>)?
        .0
        .head;
    Ok(Json(DefineSecurityResponse { head, security_id }))
}

// ---------------------------------------------------------------------------
// buy-security
// ---------------------------------------------------------------------------

/// Buy shares on the group's books: one lot on the register, one entry.
#[derive(Serialize, Deserialize)]
pub struct BuySecurityRequest {
    pub expected_head_seq: i64,
    pub security_id: String,
    pub securities_account_id: String,
    pub cash_account_id: String,
    /// Micro-shares (millionths), as the register stores them.
    pub quantity: i64,
    /// The whole cost, commission **included** — which is what makes the lot's
    /// basis and the Securities account's balance the same number. There is no
    /// separate fee field for the reason [`BuySecurityCommand`] gives.
    pub total_cost_cents: i64,
    pub trade_date: NaiveDate,
    #[serde(default)]
    pub memo: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct BuySecurityResponse {
    pub head: i64,
    /// The lot this purchase opened. A specific-lot sale later names it, and the
    /// desktop's lot picker has to be able to show it before the next pull.
    pub lot_id: String,
    pub entry_id: String,
}

/// Buy shares, validated server-side: the security exists, the entry balances,
/// every account is active and the trade date is not in a closed year — all of it
/// via [`build_buy_in_txn`], the same function the local `buy_security` runs.
async fn submit_buy_security(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<BuySecurityRequest>,
) -> Result<Json<BuySecurityResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let lot_id = Uuid::new_v4().to_string();
    let cmd = BuySecurityCommand {
        security_id: req.security_id,
        securities_account_id: req.securities_account_id,
        cash_account_id: req.cash_account_id,
        quantity: req.quantity,
        total_cost_cents: req.total_cost_cents,
        trade_date: req.trade_date,
        memo: req.memo,
    };
    let posted: Sink<String> = sink();
    let recorder = posted.clone();
    let lot = lot_id.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_buy_in_txn(tx, &lot, &cmd)? {
                InvestmentStep::Append(events) => {
                    if let (Ok(mut slot), Some(id)) = (recorder.lock(), entry_id_of(&events)) {
                        *slot = Some(id);
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                InvestmentStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<InvestmentError>)?
        .0
        .head;
    let entry_id = taken(&posted, "the purchase's journal entry")?;
    Ok(Json(BuySecurityResponse {
        head,
        lot_id,
        entry_id,
    }))
}

// ---------------------------------------------------------------------------
// sell-security
// ---------------------------------------------------------------------------

/// Sell shares on the group's books.
#[derive(Serialize, Deserialize)]
pub struct SellSecurityRequest {
    pub expected_head_seq: i64,
    pub security_id: String,
    pub securities_account_id: String,
    pub cash_account_id: String,
    pub realized_gain_account_id: String,
    /// Micro-shares.
    pub quantity: i64,
    /// Gross, as a 1099-B reports it.
    pub proceeds_cents: i64,
    pub fee_cents: i64,
    pub trade_date: NaiveDate,
    /// **Which shares are being sold**, and the reason this endpoint could not be
    /// a generic posting: FIFO and a specific-lot list produce different bases and
    /// different holding terms out of the same trade, and the choice made on the
    /// day is the one that gets filed. It is the ledger's own [`LotSelection`]
    /// rather than a wire twin of it, so the two cannot drift apart.
    #[serde(default)]
    pub selection: LotSelection,
    #[serde(default)]
    pub memo: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SellSecurityResponse {
    pub head: i64,
    pub sale_id: String,
    pub entry_id: String,
    pub proceeds_cents: i64,
    pub fee_cents: i64,
    /// The basis of the lots consumed — exactly what was credited to Securities.
    pub basis_cents: i64,
    /// `(proceeds − fee) − basis`. Negative is a loss.
    pub realized_gain_cents: i64,
    /// Per lot: which lot, how much of it, what basis went with it and on what
    /// term. Returned rather than left to be re-read, because it is what a Form
    /// 8949 row is built from and one sale can split across both terms.
    pub lots: Vec<SaleLotData>,
}

/// Sell shares, validated server-side.
///
/// Every refusal `pick_lots` can produce is reached over the wire and comes back as
/// a `422`: no lots at all, a sale larger than the position, a lot of the wrong
/// security or in the wrong account, a lot named twice, a lot overdrawn, and a
/// selection that does not sum to the sale. None of them is a partial sale — the
/// whole batch is one `append_checked_many`, so an over-sale writes nothing rather
/// than selling what it can find.
async fn submit_sell_security(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SellSecurityRequest>,
) -> Result<Json<SellSecurityResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let sale_id = Uuid::new_v4().to_string();
    let cmd = SellSecurityCommand {
        security_id: req.security_id,
        securities_account_id: req.securities_account_id,
        cash_account_id: req.cash_account_id,
        realized_gain_account_id: req.realized_gain_account_id,
        quantity: req.quantity,
        proceeds_cents: req.proceeds_cents,
        fee_cents: req.fee_cents,
        trade_date: req.trade_date,
        selection: req.selection,
        memo: req.memo,
    };
    let sold: Sink<(String, Vec<SaleLotData>, i64)> = sink();
    let recorder = sold.clone();
    let sale = sale_id.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_sell_in_txn(tx, &sale, &cmd)? {
                InvestmentStep::Append(events) => {
                    let detail = events.iter().find_map(|e| match e {
                        Event::SecuritySold(d) => Some((d.lots.clone(), d.realized_gain_cents)),
                        _ => None,
                    });
                    if let (Ok(mut slot), Some(id), Some((lots, gain))) =
                        (recorder.lock(), entry_id_of(&events), detail)
                    {
                        *slot = Some((id, lots, gain));
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                InvestmentStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<InvestmentError>)?
        .0
        .head;
    let (entry_id, lots, realized_gain_cents) = taken(&sold, "the sale it appended")?;
    Ok(Json(SellSecurityResponse {
        head,
        sale_id,
        entry_id,
        proceeds_cents: req.proceeds_cents,
        fee_cents: req.fee_cents,
        basis_cents: lots.iter().map(|l| l.basis_cents).sum(),
        realized_gain_cents,
        lots,
    }))
}

// ---------------------------------------------------------------------------
// record-investment-income
// ---------------------------------------------------------------------------

/// A dividend or interest payment into the group's brokerage account.
#[derive(Serialize, Deserialize)]
pub struct RecordInvestmentIncomeRequest {
    pub expected_head_seq: i64,
    pub kind: InvestmentIncomeKind,
    /// `None` for sweep interest, which belongs to the account and not a holding.
    #[serde(default)]
    pub security_id: Option<String>,
    pub cash_account_id: String,
    /// Named by the client, never derived from `kind` — this module does not own
    /// the chart of accounts, and guessing an account id from an enum is how a
    /// posting lands somewhere nobody chose.
    pub income_account_id: String,
    pub amount_cents: i64,
    pub received_on: NaiveDate,
    #[serde(default)]
    pub memo: Option<String>,
}

/// One posted entry, for the commands that mint nothing else.
#[derive(Serialize, Deserialize)]
pub struct PostedEntryResponse {
    pub head: i64,
    pub entry_id: String,
}

async fn submit_record_income(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordInvestmentIncomeRequest>,
) -> Result<Json<PostedEntryResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let cmd = RecordInvestmentIncomeCommand {
        kind: req.kind,
        security_id: req.security_id,
        cash_account_id: req.cash_account_id,
        income_account_id: req.income_account_id,
        amount_cents: req.amount_cents,
        received_on: req.received_on,
        memo: req.memo,
    };
    let posted: Sink<String> = sink();
    let recorder = posted.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_income_in_txn(tx, &cmd)? {
                InvestmentStep::Append(events) => {
                    if let (Ok(mut slot), Some(id)) = (recorder.lock(), entry_id_of(&events)) {
                        *slot = Some(id);
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                InvestmentStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<InvestmentError>)?
        .0
        .head;
    Ok(Json(PostedEntryResponse {
        head,
        entry_id: taken(&posted, "the income entry")?,
    }))
}

// ---------------------------------------------------------------------------
// charge-investment-fee
// ---------------------------------------------------------------------------

/// An account fee that is not part of a trade.
#[derive(Serialize, Deserialize)]
pub struct ChargeInvestmentFeeRequest {
    pub expected_head_seq: i64,
    pub cash_account_id: String,
    pub expense_account_id: String,
    pub amount_cents: i64,
    pub charged_on: NaiveDate,
    #[serde(default)]
    pub security_id: Option<String>,
    #[serde(default)]
    pub memo: Option<String>,
}

async fn submit_charge_fee(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ChargeInvestmentFeeRequest>,
) -> Result<Json<PostedEntryResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let cmd = ChargeInvestmentFeeCommand {
        cash_account_id: req.cash_account_id,
        expense_account_id: req.expense_account_id,
        amount_cents: req.amount_cents,
        charged_on: req.charged_on,
        security_id: req.security_id,
        memo: req.memo,
    };
    let posted: Sink<String> = sink();
    let recorder = posted.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_fee_in_txn(tx, &cmd)? {
                InvestmentStep::Append(events) => {
                    if let (Ok(mut slot), Some(id)) = (recorder.lock(), entry_id_of(&events)) {
                        *slot = Some(id);
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                InvestmentStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<InvestmentError>)?
        .0
        .head;
    Ok(Json(PostedEntryResponse {
        head,
        entry_id: taken(&posted, "the fee entry")?,
    }))
}

// ---------------------------------------------------------------------------
// configure-investment-account
// ---------------------------------------------------------------------------

/// Say how a provider investment account is imported into the group's books.
#[derive(Serialize, Deserialize)]
pub struct ConfigureInvestmentAccountRequest {
    pub expected_head_seq: i64,
    pub item_id: String,
    pub plaid_account_id: String,
    pub accounts: InvestmentPostingAccounts,
    /// What the provider calls the account, when the client holds it. The
    /// recognised flag is derived from it **here**, by
    /// [`classify_subtype`](crate::commands::investment_import::classify_subtype),
    /// so a client cannot set the flag to whatever suits it.
    #[serde(default)]
    pub plaid_subtype: Option<String>,
}

/// Configure an investment account, validated server-side: the connection exists,
/// every ledger account named exists and is of a type that can play its part, and
/// a sheltered one is already on the retirement register.
///
/// Returns only the head. Nothing is minted — the configuration is keyed by
/// `(item_id, plaid_account_id)`, which the client already holds.
async fn submit_configure_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ConfigureInvestmentAccountRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let recognised = classify_subtype(req.plaid_subtype.as_deref()).recognised;
    let cmd = ConfigureInvestmentAccountCommand {
        item_id: req.item_id,
        plaid_account_id: req.plaid_account_id,
        accounts: req.accounts,
        plaid_subtype: req.plaid_subtype,
    };
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_configure_in_txn(tx, &cmd, recognised)? {
                ImportStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                ImportStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response_many(outcome, expected, ApiError::domain::<ImportError>)
}

// ---------------------------------------------------------------------------
// register-retirement-account
// ---------------------------------------------------------------------------

/// Put a ledger account on the group's retirement register.
#[derive(Serialize, Deserialize)]
pub struct RegisterRetirementAccountRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    pub institution: String,
    pub kind: RetirementKind,
    pub value_change_account_id: String,
}

/// Register a sheltered account, validated server-side.
///
/// Appends two events as one unit — the registration and the `TaxLineMappingSet`
/// putting its value-change account off the return — because they are one fact, and
/// a registration that landed without its exclusion would be a window in which a
/// return could be built wrong. Hence `append_checked_many`, not `append_checked`.
async fn submit_register_retirement_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RegisterRetirementAccountRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let cmd = RegisterRetirementAccountCommand {
        account_id: req.account_id,
        institution: req.institution,
        kind: req.kind,
        value_change_account_id: req.value_change_account_id,
    };
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_registration_in_txn(tx, &cmd)? {
                RetirementStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                RetirementStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response_many(outcome, expected, ApiError::domain::<RetirementError>)
}

// ---------------------------------------------------------------------------
// set-retirement-value
// ---------------------------------------------------------------------------

/// What a statement says a sheltered account is worth.
#[derive(Serialize, Deserialize)]
pub struct SetRetirementValueRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    /// The statement date. Must not precede the last one recorded.
    pub as_of: NaiveDate,
    pub value_cents: i64,
    #[serde(default)]
    pub memo: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct SetRetirementValueResponse {
    pub head: i64,
    /// What the books said the account held on `as_of`, before this.
    pub book_value_cents: i64,
    /// What posted to the value-change account: positive for growth, negative for
    /// a fall, zero when the statement confirmed what the books already said.
    pub change_cents: i64,
    /// `None` when nothing changed. A statement that confirms no change is not a
    /// journal entry, and that is a success rather than a fault.
    pub entry_id: Option<String>,
}

/// Record a statement value, validated server-side.
///
/// The out-of-order refusal is the one that most needs the write lock: the
/// difference is measured against a book value that already contains everything
/// after `as_of`, so a statement applied out of order posts a fictional loss. On a
/// group's books two members can be holding two statements, so the comparison is
/// made against locked state and the loser gets a `422` naming the date already
/// recorded.
///
/// `book_value_cents` is derived from what the batch posted rather than re-read
/// afterwards — `value_cents − change` is the book value before it, and a second
/// read could see another member's contribution and report a figure this update was
/// never measured against.
async fn submit_set_retirement_value(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SetRetirementValueRequest>,
) -> Result<Json<SetRetirementValueResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let value_cents = req.value_cents;
    let cmd = SetRetirementValueCommand {
        account_id: req.account_id,
        as_of: req.as_of,
        value_cents: req.value_cents,
        memo: req.memo,
    };
    let account_id = cmd.account_id.clone();
    let applied: Sink<(i64, Option<String>)> = sink();
    let recorder = applied.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_value_in_txn(tx, &cmd)? {
                RetirementStep::Append(events) => {
                    if let Ok(mut slot) = recorder.lock() {
                        *slot = Some((posted_to(&events, &account_id), entry_id_of(&events)));
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                RetirementStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<RetirementError>)?
        .0
        .head;
    let (change_cents, entry_id) = taken(&applied, "what the value update came to")?;
    Ok(Json(SetRetirementValueResponse {
        head,
        book_value_cents: value_cents - change_cents,
        change_cents,
        entry_id,
    }))
}

// ---------------------------------------------------------------------------
// record-retirement-contribution
// ---------------------------------------------------------------------------

/// Money into a sheltered account on the group's books.
#[derive(Serialize, Deserialize)]
pub struct RecordRetirementContributionRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    pub funding_account_id: String,
    pub amount_cents: i64,
    pub on: NaiveDate,
    #[serde(default)]
    pub memo: Option<String>,
    /// An idempotency key, when the client has one. The desktop sends
    /// `investment_import::resolution_reference(provider_txn_id)` when it is
    /// resolving a held row, which is what makes the "post here, flip the local
    /// status there" pair safe to interrupt: a repeat is refused as a duplicate
    /// reference rather than posted twice.
    #[serde(default)]
    pub reference: Option<String>,
}

async fn submit_record_contribution(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordRetirementContributionRequest>,
) -> Result<Json<PostedEntryResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let cmd = RetirementContributionCommand {
        account_id: req.account_id,
        funding_account_id: req.funding_account_id,
        amount_cents: req.amount_cents,
        on: req.on,
        memo: req.memo,
        reference: req.reference,
    };
    let posted: Sink<String> = sink();
    let recorder = posted.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_contribution_in_txn(tx, &cmd)? {
                RetirementStep::Append(events) => {
                    if let (Ok(mut slot), Some(id)) = (recorder.lock(), entry_id_of(&events)) {
                        *slot = Some(id);
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                RetirementStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<RetirementError>)?
        .0
        .head;
    Ok(Json(PostedEntryResponse {
        head,
        entry_id: taken(&posted, "the contribution entry")?,
    }))
}

// ---------------------------------------------------------------------------
// record-retirement-distribution
// ---------------------------------------------------------------------------

/// Money out of a sheltered account, with tax withheld.
#[derive(Serialize, Deserialize)]
pub struct RecordRetirementDistributionRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    pub receiving_account_id: String,
    /// Box 1: everything that left the account.
    pub gross_cents: i64,
    /// Box 4: what the payer withheld.
    pub withheld_cents: i64,
    /// The prepaid-tax **asset** account the withholding becomes.
    pub withheld_account_id: String,
    /// On the wire although it must always be absent, exactly as it is on
    /// [`RetirementDistributionCommand`] and for the same reason: a client that
    /// reasons "a taxable distribution needs an income credit" has reached a
    /// reasonable conclusion that is wrong here, and this is where it is corrected
    /// with a `422` that explains why rather than silently ignored.
    #[serde(default)]
    pub taxable_income_account_id: Option<String>,
    /// Box 2a, when the client knows better than the register does. `None` takes
    /// the answer from the account's kind.
    #[serde(default)]
    pub taxable_cents: Option<i64>,
    pub on: NaiveDate,
    #[serde(default)]
    pub memo: Option<String>,
    /// As on a contribution.
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct RecordRetirementDistributionResponse {
    pub head: i64,
    pub entry_id: String,
    /// Gross less withholding — what reached the receiving account.
    pub net_cents: i64,
    /// Box 2a as recorded on the event, which is what a 1099-R will report.
    pub taxable_cents: i64,
}

async fn submit_record_distribution(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordRetirementDistributionRequest>,
) -> Result<Json<RecordRetirementDistributionResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let net_cents = req.gross_cents - req.withheld_cents;
    let cmd = RetirementDistributionCommand {
        account_id: req.account_id,
        receiving_account_id: req.receiving_account_id,
        gross_cents: req.gross_cents,
        withheld_cents: req.withheld_cents,
        withheld_account_id: req.withheld_account_id,
        taxable_income_account_id: req.taxable_income_account_id,
        taxable_cents: req.taxable_cents,
        on: req.on,
        memo: req.memo,
        reference: req.reference,
    };
    let posted: Sink<(String, i64)> = sink();
    let recorder = posted.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_distribution_in_txn(tx, &cmd)? {
                RetirementStep::Append(events) => {
                    let taxable = events.iter().find_map(|e| match e {
                        Event::RetirementDistributionRecorded(d) => Some(d.taxable_cents),
                        _ => None,
                    });
                    if let (Ok(mut slot), Some(id), Some(taxable)) =
                        (recorder.lock(), entry_id_of(&events), taxable)
                    {
                        *slot = Some((id, taxable));
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                RetirementStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<RetirementError>)?
        .0
        .head;
    let (entry_id, taxable_cents) = taken(&posted, "the distribution it appended")?;
    Ok(Json(RecordRetirementDistributionResponse {
        head,
        entry_id,
        net_cents,
        taxable_cents,
    }))
}


// ---------------------------------------------------------------------------
// resolve-plaid-security
// ---------------------------------------------------------------------------

/// Which of the group's securities a provider's security is — finding it, or putting
/// it on the master.
#[derive(Serialize, Deserialize)]
pub struct ResolvePlaidSecurityRequest {
    pub expected_head_seq: i64,
    /// The provider's own id, which is what the mapping is keyed on and what
    /// survives a ticker change.
    pub plaid_security_id: String,
    /// The security as our master would hold it, normalised by
    /// [`crate::commands::investment_import::provider_security_as_new`] on the client. The same [`NewSecurity`] a local
    /// import would have written, so the group's master gets the same row either way.
    pub security: NewSecurity,
}

#[derive(Serialize, Deserialize)]
pub struct ResolvePlaidSecurityResponse {
    pub head: i64,
    /// The id the **server** chose, and the kind its master holds. The client uses
    /// both from here on: a locally invented id would be a second master for one
    /// holding the moment the group's log came back with the real one, and a kind
    /// read out of a replica that has not pulled the definition yet would file a
    /// stock under "other securities".
    pub master: MasterSecurity,
    /// Whether a master was minted, which is the one thing the id alone cannot say
    /// and the number the import report shows.
    pub created: bool,
}

/// Resolve a provider security against the group's master.
///
/// The whole decision — the mapping, then the CUSIP, then the ticker, then minting
/// one — runs inside the append transaction, which is exactly why the importer's
/// hosted path cannot do it locally and push the answer: two members importing at
/// once would both miss the mapping, both mint a master for the same CUSIP, and split
/// one holding across two masters that neither add up on the balance sheet nor
/// reconcile against a 1099-B.
///
/// A security already mapped appends nothing and answers with the head it was sent.
async fn submit_resolve_plaid_security(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ResolvePlaidSecurityRequest>,
) -> Result<Json<ResolvePlaidSecurityResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let plaid_security_id = req.plaid_security_id;
    let security = req.security;
    let chosen: Sink<(MasterSecurity, bool)> = sink();
    let recorder = chosen.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| {
                let resolved = build_resolve_security_in_txn(tx, &plaid_security_id, &security)?;
                if let Ok(mut slot) = recorder.lock() {
                    *slot = Some((resolved.master, resolved.created));
                }
                match resolved.step {
                    ImportStep::Append(events) => Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    )),
                    ImportStep::Reject(e) => Ok(Verdict::Reject(e)),
                }
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<ImportError>)?
        .0
        .head;
    let (master, created) = taken(&chosen, "which security the provider's security is")?;
    Ok(Json(ResolvePlaidSecurityResponse {
        head,
        master,
        created,
    }))
}

// ---------------------------------------------------------------------------
// import-investment-activity
// ---------------------------------------------------------------------------

/// One provider transaction, imported into the group's books.
///
/// Not five endpoints. A purchase, a sale, income, a fee and a cash movement differ
/// in what they post and agree in everything that makes them an *import*: the dedup
/// fence and the register event that carries it. Those are the reason this exists at
/// all, so they are what the endpoint is about, and [`PlannedWrite`] carries the
/// difference.
#[derive(Serialize, Deserialize)]
pub struct ImportInvestmentActivityRequest {
    pub expected_head_seq: i64,
    /// Which provider transaction this is the import of.
    pub record: ImportRecord,
    /// What the importer decided, with nothing left to decide — the same value the
    /// local path hands to [`build_import_in_txn`].
    pub write: PlannedWrite,
}

#[derive(Serialize, Deserialize)]
pub struct ImportInvestmentActivityResponse {
    pub head: i64,
    /// The entry the import posted, read off the event rather than minted here.
    pub entry_id: String,
}

/// Import one provider transaction, validated server-side.
///
/// [`build_import_in_txn`] is the same function the local importer runs, so the
/// batch is the same batch: the trade or the entry, its phase-1 register event, and
/// the `InvestmentActivityImported` that fences it — appended as one unit inside the
/// server's write lock. That atomicity is the point of the endpoint. A posting whose
/// import record did not land is re-imported on the next rolling fetch with a freshly
/// minted lot id, and the same purchase is then deducted twice on a Form 8949.
///
/// A second import of the same provider transaction is a `422` whose wording the
/// client recognises, because on a replica it means only that the client's register
/// had not pulled yet.
async fn submit_import_investment_activity(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ImportInvestmentActivityRequest>,
) -> Result<Json<ImportInvestmentActivityResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let (write, record) = (req.write, req.record);
    let posted: Sink<String> = sink();
    let recorder = posted.clone();
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_import_in_txn(tx, &write, &record)? {
                ImportStep::Append(events) => {
                    if let (Ok(mut slot), Some(id)) = (recorder.lock(), entry_id_of(&events)) {
                        *slot = Some(id);
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                ImportStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<ImportError>)?
        .0
        .head;
    let entry_id = taken(&posted, "the imported transaction's journal entry")?;
    Ok(Json(ImportInvestmentActivityResponse { head, entry_id }))
}

// ---------------------------------------------------------------------------
// record-holdings-snapshot
// ---------------------------------------------------------------------------

/// What the broker said one account held, on a date.
#[derive(Serialize, Deserialize)]
pub struct RecordHoldingsSnapshotRequest {
    pub expected_head_seq: i64,
    /// The snapshot as the event carries it, including the `snapshot_id` the client
    /// minted — so the id in the group's log is the id the client already reported.
    pub snapshot: HoldingsSnapshotData,
}

#[derive(Serialize, Deserialize)]
pub struct RecordHoldingsSnapshotResponse {
    pub head: i64,
    /// `false` when the group already held this exact snapshot for this date, so
    /// nothing was appended. That is a success, and it is what makes re-importing a
    /// holdings payload append literally nothing.
    pub recorded: bool,
}

/// Record a holdings snapshot on the group's books.
///
/// The snapshot has no journal entry behind it and posts no money, which is what
/// makes it tempting to leave local — and it must not be. The reconciliation
/// (spec §7) and the market-value report both read it, so a snapshot only one machine
/// held would make one member's reconciliation disagree with another's about what the
/// broker said; and a snapshot appended locally on a replica would take a sequence
/// number the server is also going to hand out.
///
/// Whether this snapshot is new is decided **inside** the transaction, compared line
/// by line, so two members reading the same holdings at the same moment record one
/// snapshot between them rather than two.
async fn submit_record_holdings_snapshot(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordHoldingsSnapshotRequest>,
) -> Result<Json<RecordHoldingsSnapshotResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let snapshot = req.snapshot;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_snapshot_in_txn(tx, &snapshot)? {
                ImportStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                ImportStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    // Nothing appended means the head did not move, which is exactly how the client
    // is told the snapshot was already on file.
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<ImportError>)?
        .0
        .head;
    Ok(Json(RecordHoldingsSnapshotResponse {
        head,
        recorded: head != expected,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::investment_commands::{
        buy_security, charge_fee, define_security, lots_of, record_income, sell_security,
        BuySecurityCommand, ChargeInvestmentFeeCommand, NewSecurity, RecordInvestmentIncomeCommand,
        SellSecurityCommand, MICRO_SHARE,
    };
    use crate::commands::investment_import::{configure_account, ConfigureInvestmentAccountCommand};
    use crate::commands::retirement_commands::{
        record_contribution, record_distribution, register_account, set_value,
        RegisterRetirementAccountCommand, RetirementContributionCommand,
        RetirementDistributionCommand, SetRetirementValueCommand,
    };
    use crate::domain::AccountType;
    use crate::events::types::{StoredEvent, TaxableBrokerageAccounts};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;
    use crate::sync::router;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const TOKEN: &str = "tok-1";
    const ACTOR: &str = "user-1";

    // The chart spec §2a and §2b describe, with stable ids so a test reads as the
    // accounts it names rather than as a list of freshly minted uuids.
    const BANK: &str = "1000";
    const CASH: &str = "1101";
    const SECURITIES: &str = "1102";
    const PREPAID_TAX: &str = "1300";
    const IRA: &str = "1500";
    /// A securities slot distinct from the stocks one, so a test can see which slot a
    /// position was filed in. See `importing_accounts`.
    const OTHER_SECURITIES: &str = "1103";
    /// Where the other leg of a brokerage transfer waits for the bank feed.
    const CLEARING: &str = "1200";
    const DIVIDENDS: &str = "4100";
    const INTEREST: &str = "4110";
    const GAIN: &str = "4120";
    const VALUE_CHANGE: &str = "4130";
    const FEES: &str = "6600";

    fn tokens() -> HashMap<String, String> {
        HashMap::from([(TOKEN.to_string(), ACTOR.to_string())])
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Shares as micro-shares, so the tests read in shares.
    fn sh(n: i64) -> i64 {
        n * MICRO_SHARE
    }

    /// A book with the chart both models need and one provider connection.
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
            (BANK, "Checking", AccountType::Asset),
            (CASH, "Brokerage cash", AccountType::Asset),
            (SECURITIES, "Securities at cost", AccountType::Asset),
            (OTHER_SECURITIES, "Other securities at cost", AccountType::Asset),
            (CLEARING, "Cash in transit", AccountType::Asset),
            (PREPAID_TAX, "Prepaid tax", AccountType::Asset),
            (IRA, "Fidelity IRA ••5678", AccountType::Asset),
            (DIVIDENDS, "Dividends", AccountType::Revenue),
            (INTEREST, "Interest", AccountType::Revenue),
            (GAIN, "Realized gain", AccountType::Revenue),
            (VALUE_CHANGE, "Retirement value change", AccountType::Revenue),
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
        // Machine-local, and inserted directly for the same reason the importer's
        // own tests do it: connecting an item is another command's business.
        store
            .connection()
            .execute(
                "INSERT INTO plaid_items (id, proxy_item_id, institution_name)
                 VALUES ('item1','p1','Fidelity')",
                [],
            )
            .unwrap();
        store
    }

    fn taxable_accounts() -> InvestmentPostingAccounts {
        InvestmentPostingAccounts::Taxable(Box::new(TaxableBrokerageAccounts {
            stocks_account_id: SECURITIES.into(),
            mutual_funds_account_id: None,
            other_securities_account_id: None,
            cash_account_id: CASH.into(),
            dividend_income_account_id: DIVIDENDS.into(),
            interest_income_account_id: INTEREST.into(),
            tax_exempt_interest_account_id: None,
            capital_gain_distribution_account_id: None,
            realized_gain_account_id: GAIN.into(),
            fee_expense_account_id: FEES.into(),
            transfer_clearing_account_id: None,
        }))
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

    fn a_lot(store: &mut EventStore, security_id: &str, shares: i64, cost: i64, on: NaiveDate) {
        buy_security(
            store,
            "u",
            &BuySecurityCommand {
                security_id: security_id.to_string(),
                securities_account_id: SECURITIES.into(),
                cash_account_id: CASH.into(),
                quantity: shares,
                total_cost_cents: cost,
                trade_date: on,
                memo: None,
            },
        )
        .expect("bought");
    }

    /// One loopback server over `store`, plus the handle its own log is read back
    /// through — `SyncState::store` is an `Arc`, so a test can check what the
    /// endpoint actually wrote instead of only what it answered.
    async fn serve(store: EventStore) -> (Wire, Arc<Mutex<EventStore>>) {
        let state = SyncState::new(store, tokens());
        let handle = state.store.clone();
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            Wire {
                http: reqwest::Client::new(),
                base: format!("http://{addr}"),
            },
            handle,
        )
    }

    /// A bearer-authenticated caller, so each test reads as the exchange it is.
    struct Wire {
        http: reqwest::Client,
        base: String,
    }

    impl Wire {
        async fn head(&self) -> i64 {
            self.http
                .get(format!("{}/sync/head", self.base))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["head"]
                .as_i64()
                .unwrap()
        }

        async fn post(
            &self,
            path: &str,
            body: serde_json::Value,
        ) -> (reqwest::StatusCode, serde_json::Value) {
            let resp = self
                .http
                .post(format!("{}/sync/commands/{path}", self.base))
                .bearer_auth(TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = resp.status();
            (status, resp.json().await.unwrap_or(serde_json::Value::Null))
        }

        /// The happy path, with the status asserted so a test that meant to succeed
        /// cannot quietly assert things about an error body.
        async fn ok(&self, path: &str, body: serde_json::Value) -> serde_json::Value {
            let (status, v) = self.post(path, body).await;
            assert_eq!(status, reqwest::StatusCode::OK, "{path}: {v}");
            v
        }

        async fn status(&self, path: &str, body: serde_json::Value) -> reqwest::StatusCode {
            self.post(path, body).await.0
        }
    }

    /// Every event after `from`, as the ledger holds it.
    fn events_after(handle: &Arc<Mutex<EventStore>>, from: i64) -> Vec<StoredEvent> {
        handle.lock().unwrap().get_after(from).unwrap()
    }

    // -----------------------------------------------------------------------
    // define-security
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn define_security_mints_an_id_and_refuses_a_ticker_already_on_the_master() {
        let (wire, handle) = serve(store()).await;
        let head = wire.head().await;
        let body = |expected: i64, ticker: &str| {
            serde_json::json!({
                "expected_head_seq": expected,
                "ticker": ticker,
                "name": "Acme Corp",
                "kind": "stock",
            })
        };

        // Unauthenticated: refused before the handler body runs, nothing appended.
        let unauth = wire
            .http
            .post(format!("{}/sync/commands/define-security", wire.base))
            .json(&body(head, "ACME"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        let ok = wire.ok("define-security", body(head, "ACME")).await;
        assert_eq!(ok["head"].as_i64().unwrap(), head + 1);
        let security_id = ok["security_id"].as_str().unwrap().to_string();
        assert!(!security_id.is_empty());

        // Stale head: the log moved, so nothing is written and the client is told
        // where it moved to.
        let (status, v) = wire.post("define-security", body(head, "BETA")).await;
        assert_eq!(status, reqwest::StatusCode::CONFLICT);
        assert_eq!(v["current_head"].as_i64().unwrap(), head + 1);

        // The in-transaction ticker check, which is the whole reason this is a
        // server command and not a local append pushed afterwards.
        let (status, v) = wire.post("define-security", body(head + 1, "acme")).await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            v["error"].as_str().unwrap().contains(&security_id),
            "the refusal should name the security that has the ticker: {v}"
        );
        assert_eq!(events_after(&handle, head).len(), 1, "only the one define");
    }

    // -----------------------------------------------------------------------
    // buy-security
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn buy_security_returns_its_lot_and_refuses_a_security_that_is_not_on_the_master() {
        let mut store = store();
        let security_id = acme(&mut store);
        let (wire, handle) = serve(store).await;
        let head = wire.head().await;
        let body = |expected: i64, security: &str| {
            serde_json::json!({
                "expected_head_seq": expected,
                "security_id": security,
                "securities_account_id": SECURITIES,
                "cash_account_id": CASH,
                "quantity": sh(10),
                "total_cost_cents": 100_000,
                "trade_date": "2025-03-01",
            })
        };

        let ok = wire.ok("buy-security", body(head, &security_id)).await;
        let lot_id = ok["lot_id"].as_str().unwrap().to_string();
        assert!(!lot_id.is_empty());
        assert!(!ok["entry_id"].as_str().unwrap().is_empty());

        // The lot is on the register, with the whole cost as its basis.
        let lots = lots_of(
            handle.lock().unwrap().connection(),
            &security_id,
            SECURITIES,
        );
        assert_eq!(lots.len(), 1);
        assert_eq!(lots[0].lot_id, lot_id);
        assert_eq!(lots[0].remaining_quantity, sh(10));
        assert_eq!(lots[0].remaining_basis_cents, 100_000);

        assert_eq!(
            wire.status("buy-security", body(head, &security_id)).await,
            reqwest::StatusCode::CONFLICT
        );

        let (status, v) = wire
            .post("buy-security", body(ok["head"].as_i64().unwrap(), "nope"))
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(v["error"].as_str().unwrap().contains("nope"), "{v}");
    }

    // -----------------------------------------------------------------------
    // sell-security
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sell_security_returns_the_gain_per_lot_and_refuses_an_empty_position() {
        let mut store = store();
        let security_id = acme(&mut store);
        // Two lots, a year apart, so one sale crosses the term boundary — the case
        // a per-lot response exists for.
        a_lot(&mut store, &security_id, sh(10), 100_000, day(2023, 1, 10));
        a_lot(&mut store, &security_id, sh(10), 140_000, day(2025, 1, 10));
        let empty = define_security(
            &mut store,
            "u",
            &NewSecurity {
                ticker: "NONE".into(),
                name: "Never bought".into(),
                kind: "stock".into(),
                cusip: None,
                currency: "USD".into(),
            },
        )
        .expect("defined")
        .0;
        let (wire, _handle) = serve(store).await;
        let head = wire.head().await;
        let body = |expected: i64, security: &str, quantity: i64| {
            serde_json::json!({
                "expected_head_seq": expected,
                "security_id": security,
                "securities_account_id": SECURITIES,
                "cash_account_id": CASH,
                "realized_gain_account_id": GAIN,
                "quantity": quantity,
                "proceeds_cents": 200_000,
                "fee_cents": 995,
                "trade_date": "2025-06-01",
                "selection": "fifo",
            })
        };

        let ok = wire.ok("sell-security", body(head, &security_id, sh(15))).await;
        assert!(!ok["sale_id"].as_str().unwrap().is_empty());
        assert!(!ok["entry_id"].as_str().unwrap().is_empty());
        // FIFO: the whole 2023 lot, then half the 2025 one.
        let lots = ok["lots"].as_array().unwrap();
        assert_eq!(lots.len(), 2);
        assert_eq!(lots[0]["basis_cents"].as_i64().unwrap(), 100_000);
        assert_eq!(lots[0]["term"].as_str().unwrap(), "long");
        assert_eq!(lots[1]["quantity"].as_i64().unwrap(), sh(5));
        assert_eq!(lots[1]["basis_cents"].as_i64().unwrap(), 70_000);
        assert_eq!(lots[1]["term"].as_str().unwrap(), "short");
        assert_eq!(ok["basis_cents"].as_i64().unwrap(), 170_000);
        // (200_000 − 995) − 170_000.
        assert_eq!(ok["realized_gain_cents"].as_i64().unwrap(), 29_005);

        assert_eq!(
            wire.status("sell-security", body(head, &security_id, sh(1)))
                .await,
            reqwest::StatusCode::CONFLICT
        );

        // No lots at all is its own refusal, and not "not enough shares": the
        // register can show nothing, so saying the position is short would send
        // somebody looking in the wrong place.
        let (status, v) = wire
            .post(
                "sell-security",
                body(ok["head"].as_i64().unwrap(), &empty, sh(1)),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            v["error"].as_str().unwrap().contains("nothing to sell"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn a_specific_lot_selection_sent_over_the_wire_consumes_the_lots_it_names() {
        let mut store = store();
        let security_id = acme(&mut store);
        a_lot(&mut store, &security_id, sh(10), 100_000, day(2023, 1, 10));
        a_lot(&mut store, &security_id, sh(10), 200_000, day(2024, 1, 10));
        a_lot(&mut store, &security_id, sh(10), 300_000, day(2025, 1, 10));
        let ids: Vec<String> = lots_of(store.connection(), &security_id, SECURITIES)
            .into_iter()
            .map(|l| l.lot_id)
            .collect();
        let (first, second, third) = (ids[0].clone(), ids[1].clone(), ids[2].clone());

        let (wire, handle) = serve(store).await;
        let head = wire.head().await;
        // Deliberately not the FIFO answer: four shares out of the middle lot and
        // six out of the newest, which is a basis of 80_000 + 180_000 = 260_000
        // where FIFO would have relieved 100_000.
        let ok = wire
            .ok(
                "sell-security",
                serde_json::json!({
                    "expected_head_seq": head,
                    "security_id": security_id,
                    "securities_account_id": SECURITIES,
                    "cash_account_id": CASH,
                    "realized_gain_account_id": GAIN,
                    "quantity": sh(10),
                    "proceeds_cents": 400_000,
                    "fee_cents": 0,
                    "trade_date": "2025-06-01",
                    "selection": { "specific": [[second, sh(4)], [third, sh(6)]] },
                }),
            )
            .await;

        let lots = ok["lots"].as_array().unwrap();
        assert_eq!(lots.len(), 2);
        assert_eq!(lots[0]["lot_id"].as_str().unwrap(), second);
        assert_eq!(lots[0]["basis_cents"].as_i64().unwrap(), 80_000);
        assert_eq!(lots[1]["lot_id"].as_str().unwrap(), third);
        assert_eq!(lots[1]["basis_cents"].as_i64().unwrap(), 180_000);
        assert_eq!(ok["basis_cents"].as_i64().unwrap(), 260_000);
        assert_eq!(ok["realized_gain_cents"].as_i64().unwrap(), 140_000);

        // And the register agrees: the oldest lot was not touched, which is what
        // proves the selection crossed the wire rather than being re-derived.
        let remaining: HashMap<String, i64> =
            lots_of(handle.lock().unwrap().connection(), &security_id, SECURITIES)
                .into_iter()
                .map(|l| (l.lot_id, l.remaining_quantity))
                .collect();
        assert_eq!(remaining[&first], sh(10));
        assert_eq!(remaining[&second], sh(6));
        assert_eq!(remaining[&third], sh(4));
    }

    #[tokio::test]
    async fn over_selling_over_the_wire_is_refused_whole_rather_than_sold_in_part() {
        let mut store = store();
        let security_id = acme(&mut store);
        a_lot(&mut store, &security_id, sh(10), 100_000, day(2025, 1, 10));
        a_lot(&mut store, &security_id, sh(10), 150_000, day(2025, 2, 10));
        let lot_id = lots_of(store.connection(), &security_id, SECURITIES)[0]
            .lot_id
            .clone();
        let (wire, handle) = serve(store).await;
        let head = wire.head().await;
        let sale = |selection: serde_json::Value, quantity: i64| {
            serde_json::json!({
                "expected_head_seq": head,
                "security_id": security_id,
                "securities_account_id": SECURITIES,
                "cash_account_id": CASH,
                "realized_gain_account_id": GAIN,
                "quantity": quantity,
                "proceeds_cents": 300_000,
                "fee_cents": 0,
                "trade_date": "2025-06-01",
                "selection": selection,
            })
        };

        // FIFO, larger than the whole position of 20.
        let (status, v) = wire
            .post("sell-security", sale(serde_json::json!("fifo"), sh(25)))
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(v["error"].as_str().unwrap().contains("cannot be larger"), "{v}");

        // A named lot overdrawn, inside a position that *would* cover the sale —
        // which is the failure a selection can make and FIFO cannot, and the one a
        // clamping implementation would silently fill from the next lot along.
        let (status, v) = wire
            .post(
                "sell-security",
                sale(
                    serde_json::json!({ "specific": [[lot_id, sh(15)]] }),
                    sh(15),
                ),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(v["error"].as_str().unwrap().contains("shares left"), "{v}");

        // Nothing was written by either: no head movement, no partial sale, and both
        // lots are exactly as they were. A partial sale here would post a gain
        // against a basis nobody chose and reconcile against nothing.
        assert_eq!(wire.head().await, head);
        assert!(events_after(&handle, head).is_empty());
        let lots = lots_of(handle.lock().unwrap().connection(), &security_id, SECURITIES);
        assert_eq!(lots.len(), 2);
        assert_eq!(lots[0].remaining_quantity, sh(10));
        assert_eq!(lots[0].remaining_basis_cents, 100_000);
        assert_eq!(lots[1].remaining_quantity, sh(10));
        assert_eq!(lots[1].remaining_basis_cents, 150_000);
    }

    // -----------------------------------------------------------------------
    // record-investment-income / charge-investment-fee
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn record_investment_income_posts_the_payment_and_refuses_income_of_nothing() {
        let mut store = store();
        let security_id = acme(&mut store);
        let (wire, _handle) = serve(store).await;
        let head = wire.head().await;
        let body = |expected: i64, amount: i64| {
            serde_json::json!({
                "expected_head_seq": expected,
                "kind": "dividend",
                "security_id": security_id,
                "cash_account_id": CASH,
                "income_account_id": DIVIDENDS,
                "amount_cents": amount,
                "received_on": "2025-04-15",
            })
        };

        let ok = wire.ok("record-investment-income", body(head, 1_234)).await;
        assert!(!ok["entry_id"].as_str().unwrap().is_empty());
        assert_eq!(ok["head"].as_i64().unwrap(), head + 2, "entry + register event");

        assert_eq!(
            wire.status("record-investment-income", body(head, 1_234))
                .await,
            reqwest::StatusCode::CONFLICT
        );
        assert_eq!(
            wire.status("record-investment-income", body(head + 2, 0))
                .await,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[tokio::test]
    async fn charge_investment_fee_posts_an_expense_and_refuses_a_fee_of_nothing() {
        let (wire, _handle) = serve(store()).await;
        let head = wire.head().await;
        let body = |expected: i64, amount: i64| {
            serde_json::json!({
                "expected_head_seq": expected,
                "cash_account_id": CASH,
                "expense_account_id": FEES,
                "amount_cents": amount,
                "charged_on": "2025-04-15",
            })
        };

        let ok = wire.ok("charge-investment-fee", body(head, 500)).await;
        assert!(!ok["entry_id"].as_str().unwrap().is_empty());

        assert_eq!(
            wire.status("charge-investment-fee", body(head, 500)).await,
            reqwest::StatusCode::CONFLICT
        );
        assert_eq!(
            wire.status("charge-investment-fee", body(head + 2, -1))
                .await,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    // -----------------------------------------------------------------------
    // configure-investment-account
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn configure_investment_account_checks_the_accounts_and_refuses_an_unknown_connection() {
        let (wire, _handle) = serve(store()).await;
        let head = wire.head().await;
        let body = |expected: i64, item: &str, accounts: InvestmentPostingAccounts| {
            serde_json::json!({
                "expected_head_seq": expected,
                "item_id": item,
                "plaid_account_id": "acct-1",
                "accounts": accounts,
                "plaid_subtype": "brokerage",
            })
        };

        let ok = wire
            .ok(
                "configure-investment-account",
                body(head, "item1", taxable_accounts()),
            )
            .await;
        assert_eq!(ok["head"].as_i64().unwrap(), head + 1);

        assert_eq!(
            wire.status(
                "configure-investment-account",
                body(head, "item1", taxable_accounts())
            )
            .await,
            reqwest::StatusCode::CONFLICT
        );

        let (status, v) = wire
            .post(
                "configure-investment-account",
                body(head + 1, "nope", taxable_accounts()),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(v["error"].as_str().unwrap().contains("nope"), "{v}");

        // A sheltered configuration against an account that is not on the
        // retirement register: the refusal phase 4 exists to make, reached over the
        // wire.
        let (status, v) = wire
            .post(
                "configure-investment-account",
                body(
                    head + 1,
                    "item1",
                    InvestmentPostingAccounts::Sheltered {
                        retirement_account_id: IRA.into(),
                    },
                ),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(v["error"].as_str().unwrap().contains("register"), "{v}");
    }

    // -----------------------------------------------------------------------
    // register-retirement-account
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_retirement_account_excludes_its_value_change_and_checks_both_types() {
        let (wire, handle) = serve(store()).await;
        let head = wire.head().await;
        let body = |expected: i64, account: &str| {
            serde_json::json!({
                "expected_head_seq": expected,
                "account_id": account,
                "institution": "Fidelity ••5678",
                "kind": "traditional",
                "value_change_account_id": VALUE_CHANGE,
            })
        };

        let ok = wire.ok("register-retirement-account", body(head, IRA)).await;
        // Two events as one unit: the registration and the off-the-return mapping.
        assert_eq!(ok["head"].as_i64().unwrap(), head + 2);
        let appended = events_after(&handle, head);
        assert!(appended
            .iter()
            .any(|e| matches!(e.event, Event::TaxLineMappingSet { .. })));

        assert_eq!(
            wire.status("register-retirement-account", body(head, IRA))
                .await,
            reqwest::StatusCode::CONFLICT
        );

        // A revenue account cannot be the one carried at value — the two arguments
        // the wrong way round balance perfectly and make the books nonsense.
        let (status, v) = wire
            .post("register-retirement-account", body(head + 2, DIVIDENDS))
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(v["error"].as_str().unwrap().contains("asset account"), "{v}");
    }

    // -----------------------------------------------------------------------
    // set-retirement-value
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn set_retirement_value_posts_the_difference_and_refuses_a_statement_out_of_order() {
        let mut store = store();
        register_account(
            &mut store,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: IRA.into(),
                institution: "Fidelity ••5678".into(),
                kind: RetirementKind::Traditional,
                value_change_account_id: VALUE_CHANGE.into(),
            },
        )
        .expect("registered");
        let (wire, _handle) = serve(store).await;
        let head = wire.head().await;
        let body = |expected: i64, as_of: &str, value: i64| {
            serde_json::json!({
                "expected_head_seq": expected,
                "account_id": IRA,
                "as_of": as_of,
                "value_cents": value,
            })
        };

        let ok = wire
            .ok("set-retirement-value", body(head, "2025-06-30", 100_000))
            .await;
        assert_eq!(ok["book_value_cents"].as_i64().unwrap(), 0);
        assert_eq!(ok["change_cents"].as_i64().unwrap(), 100_000);
        assert!(ok["entry_id"].as_str().is_some());

        assert_eq!(
            wire.status("set-retirement-value", body(head, "2025-09-30", 110_000))
                .await,
            reqwest::StatusCode::CONFLICT
        );

        let head = wire.head().await;
        // A statement that confirms what the books already say posts nothing, and
        // that is a success rather than a fault.
        let same = wire
            .ok("set-retirement-value", body(head, "2025-06-30", 100_000))
            .await;
        assert_eq!(same["change_cents"].as_i64().unwrap(), 0);
        assert!(same["entry_id"].is_null());

        let (status, v) = wire
            .post(
                "set-retirement-value",
                body(wire.head().await, "2025-03-31", 90_000),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            v["error"].as_str().unwrap().contains("fictional loss"),
            "{v}"
        );
    }

    // -----------------------------------------------------------------------
    // record-retirement-contribution / -distribution
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn record_retirement_contribution_posts_a_transfer_and_refuses_an_unregistered_account() {
        let mut store = store();
        register_account(
            &mut store,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: IRA.into(),
                institution: "Fidelity ••5678".into(),
                kind: RetirementKind::Traditional,
                value_change_account_id: VALUE_CHANGE.into(),
            },
        )
        .expect("registered");
        let (wire, handle) = serve(store).await;
        let head = wire.head().await;
        let body = |expected: i64, account: &str, reference: Option<&str>| {
            serde_json::json!({
                "expected_head_seq": expected,
                "account_id": account,
                "funding_account_id": BANK,
                "amount_cents": 50_000,
                "on": "2025-04-01",
                "reference": reference,
            })
        };

        let ok = wire
            .ok(
                "record-retirement-contribution",
                body(head, IRA, Some("investment-activity-txn-1")),
            )
            .await;
        assert!(!ok["entry_id"].as_str().unwrap().is_empty());
        // Both sides assets: the money changed which account holds it, not how much
        // there is.
        let lines: Vec<(String, i64)> = events_after(&handle, head)
            .iter()
            .filter_map(|e| match &e.event {
                Event::JournalEntryPosted { lines, .. } => Some(lines.clone()),
                _ => None,
            })
            .flatten()
            .map(|l| (l.account_id, l.amount))
            .collect();
        assert!(lines.contains(&(IRA.to_string(), 50_000)));
        assert!(lines.contains(&(BANK.to_string(), -50_000)));

        assert_eq!(
            wire.status("record-retirement-contribution", body(head, IRA, None))
                .await,
            reqwest::StatusCode::CONFLICT
        );

        // The reference is the fence a resolution's two-step shape relies on: the
        // same provider transaction cannot be contributed twice.
        let (status, v) = wire
            .post(
                "record-retirement-contribution",
                body(
                    wire.head().await,
                    IRA,
                    Some("investment-activity-txn-1"),
                ),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(v["error"].as_str().unwrap().contains("investment-activity-txn-1"), "{v}");

        // An account that is not on the register at all. Not `BANK`, which is the
        // funding account here — that would trip the self-transfer refusal first
        // and test the wrong fence.
        let (status, v) = wire
            .post(
                "record-retirement-contribution",
                body(wire.head().await, CASH, None),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("retirement register"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn record_retirement_distribution_reports_box_2a_and_refuses_an_income_account() {
        let mut store = store();
        register_account(
            &mut store,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: IRA.into(),
                institution: "Fidelity ••5678".into(),
                kind: RetirementKind::Traditional,
                value_change_account_id: VALUE_CHANGE.into(),
            },
        )
        .expect("registered");
        let (wire, _handle) = serve(store).await;
        let head = wire.head().await;
        let body = |expected: i64, income: Option<&str>| {
            serde_json::json!({
                "expected_head_seq": expected,
                "account_id": IRA,
                "receiving_account_id": BANK,
                "gross_cents": 20_000,
                "withheld_cents": 2_000,
                "withheld_account_id": PREPAID_TAX,
                "taxable_income_account_id": income,
                "on": "2025-05-01",
            })
        };

        let ok = wire.ok("record-retirement-distribution", body(head, None)).await;
        assert_eq!(ok["net_cents"].as_i64().unwrap(), 18_000);
        // The whole gross, from the account's kind: pre-tax money coming out.
        assert_eq!(ok["taxable_cents"].as_i64().unwrap(), 20_000);

        assert_eq!(
            wire.status("record-retirement-distribution", body(head, None))
                .await,
            reqwest::StatusCode::CONFLICT
        );

        // Named an income account: corrected with a reason rather than ignored.
        let (status, v) = wire
            .post(
                "record-retirement-distribution",
                body(wire.head().await, Some(DIVIDENDS)),
            )
            .await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            v["error"].as_str().unwrap().contains("posts no income"),
            "{v}"
        );
    }

    // -----------------------------------------------------------------------
    // The point of all of it
    // -----------------------------------------------------------------------

    /// Replace every UUID-shaped run with a placeholder.
    ///
    /// Two books that ran the same commands mint different ids for the lot, the
    /// entry, the sale and the security, and comparing the logs is only possible
    /// once those are out of the way. Everything else — the dates, the memos
    /// (including the ticker a memo is built from), the account ids, the signed line
    /// amounts, the references' shapes, the source, the register events' figures —
    /// is compared exactly, which is the assertion worth making.
    fn scrub_ids(json: &str) -> String {
        fn is_uuid(bytes: &[u8]) -> bool {
            if bytes.len() < 36 {
                return false;
            }
            bytes[..36].iter().enumerate().all(|(i, b)| match i {
                8 | 13 | 18 | 23 => *b == b'-',
                _ => b.is_ascii_hexdigit(),
            })
        }
        let bytes = json.as_bytes();
        let mut out = String::with_capacity(json.len());
        let mut i = 0;
        while i < bytes.len() {
            if is_uuid(&bytes[i..]) {
                out.push_str("<id>");
                i += 36;
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        out
    }

    /// Whatever else these endpoints do, this is what they are for: a hosted book
    /// and a local book that ran the same commands hold the same log.
    ///
    /// Not "the same balances" and not "the same entries" — the same **events**, in
    /// the same order, scrubbed only of the ids each book mints for itself. That
    /// catches the failures a balance comparison would miss: a register event left
    /// off the hosted path, a memo built from something the server does not have, a
    /// missing `System` source, an entry whose reference is shaped differently so a
    /// re-import would not be deduplicated against it.
    #[tokio::test]
    async fn hosted_and_local_paths_post_the_same_entries() {
        // --- the local book ---
        let mut local = store();
        let baseline = local.latest_id().unwrap().unwrap_or(0);
        let security_id = acme(&mut local);
        a_lot(&mut local, &security_id, sh(10), 100_000, day(2023, 1, 10));
        a_lot(&mut local, &security_id, sh(10), 140_000, day(2025, 1, 10));
        sell_security(
            &mut local,
            "u",
            &SellSecurityCommand {
                security_id: security_id.clone(),
                securities_account_id: SECURITIES.into(),
                cash_account_id: CASH.into(),
                realized_gain_account_id: GAIN.into(),
                quantity: sh(15),
                proceeds_cents: 200_000,
                fee_cents: 995,
                trade_date: day(2025, 6, 1),
                selection: LotSelection::Fifo,
                memo: None,
            },
        )
        .expect("sold");
        record_income(
            &mut local,
            "u",
            &RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Dividend,
                security_id: Some(security_id.clone()),
                cash_account_id: CASH.into(),
                income_account_id: DIVIDENDS.into(),
                amount_cents: 1_234,
                received_on: day(2025, 4, 15),
                memo: None,
            },
        )
        .expect("dividend");
        charge_fee(
            &mut local,
            "u",
            &ChargeInvestmentFeeCommand {
                cash_account_id: CASH.into(),
                expense_account_id: FEES.into(),
                amount_cents: 500,
                charged_on: day(2025, 4, 30),
                security_id: None,
                memo: None,
            },
        )
        .expect("fee");
        register_account(
            &mut local,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: IRA.into(),
                institution: "Fidelity ••5678".into(),
                kind: RetirementKind::Traditional,
                value_change_account_id: VALUE_CHANGE.into(),
            },
        )
        .expect("registered");
        set_value(
            &mut local,
            "u",
            &SetRetirementValueCommand {
                account_id: IRA.into(),
                as_of: day(2025, 6, 30),
                value_cents: 100_000,
                memo: None,
            },
        )
        .expect("valued");
        record_contribution(
            &mut local,
            "u",
            &RetirementContributionCommand {
                account_id: IRA.into(),
                funding_account_id: BANK.into(),
                amount_cents: 50_000,
                on: day(2025, 7, 1),
                memo: None,
                reference: Some("investment-activity-txn-1".into()),
            },
        )
        .expect("contributed");
        record_distribution(
            &mut local,
            "u",
            &RetirementDistributionCommand {
                account_id: IRA.into(),
                receiving_account_id: BANK.into(),
                gross_cents: 20_000,
                withheld_cents: 2_000,
                withheld_account_id: PREPAID_TAX.into(),
                taxable_income_account_id: None,
                taxable_cents: None,
                on: day(2025, 8, 1),
                memo: None,
                reference: Some("investment-activity-txn-2".into()),
            },
        )
        .expect("distributed");
        configure_account(
            &mut local,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: "item1".into(),
                plaid_account_id: "acct-1".into(),
                accounts: taxable_accounts(),
                plaid_subtype: Some("brokerage".into()),
            },
        )
        .expect("configured");
        let local_log = local.get_after(baseline).unwrap();

        // --- the hosted book, over the wire, in the same order ---
        let (wire, handle) = serve(store()).await;
        let mut head = wire.head().await;
        assert_eq!(head, baseline, "the two books start from the same chart");
        let define = wire
            .ok(
                "define-security",
                serde_json::json!({
                    "expected_head_seq": head,
                    "ticker": "ACME",
                    "name": "Acme Corp",
                    "kind": "stock",
                    "cusip": "037833100",
                }),
            )
            .await;
        head = define["head"].as_i64().unwrap();
        let hosted_security = define["security_id"].as_str().unwrap().to_string();
        for (shares, cost, on) in [
            (sh(10), 100_000, "2023-01-10"),
            (sh(10), 140_000, "2025-01-10"),
        ] {
            head = wire
                .ok(
                    "buy-security",
                    serde_json::json!({
                        "expected_head_seq": head,
                        "security_id": hosted_security,
                        "securities_account_id": SECURITIES,
                        "cash_account_id": CASH,
                        "quantity": shares,
                        "total_cost_cents": cost,
                        "trade_date": on,
                    }),
                )
                .await["head"]
                .as_i64()
                .unwrap();
        }
        head = wire
            .ok(
                "sell-security",
                serde_json::json!({
                    "expected_head_seq": head,
                    "security_id": hosted_security,
                    "securities_account_id": SECURITIES,
                    "cash_account_id": CASH,
                    "realized_gain_account_id": GAIN,
                    "quantity": sh(15),
                    "proceeds_cents": 200_000,
                    "fee_cents": 995,
                    "trade_date": "2025-06-01",
                    "selection": "fifo",
                }),
            )
            .await["head"]
            .as_i64()
            .unwrap();
        head = wire
            .ok(
                "record-investment-income",
                serde_json::json!({
                    "expected_head_seq": head,
                    "kind": "dividend",
                    "security_id": hosted_security,
                    "cash_account_id": CASH,
                    "income_account_id": DIVIDENDS,
                    "amount_cents": 1_234,
                    "received_on": "2025-04-15",
                }),
            )
            .await["head"]
            .as_i64()
            .unwrap();
        head = wire
            .ok(
                "charge-investment-fee",
                serde_json::json!({
                    "expected_head_seq": head,
                    "cash_account_id": CASH,
                    "expense_account_id": FEES,
                    "amount_cents": 500,
                    "charged_on": "2025-04-30",
                }),
            )
            .await["head"]
            .as_i64()
            .unwrap();
        head = wire
            .ok(
                "register-retirement-account",
                serde_json::json!({
                    "expected_head_seq": head,
                    "account_id": IRA,
                    "institution": "Fidelity ••5678",
                    "kind": "traditional",
                    "value_change_account_id": VALUE_CHANGE,
                }),
            )
            .await["head"]
            .as_i64()
            .unwrap();
        head = wire
            .ok(
                "set-retirement-value",
                serde_json::json!({
                    "expected_head_seq": head,
                    "account_id": IRA,
                    "as_of": "2025-06-30",
                    "value_cents": 100_000,
                }),
            )
            .await["head"]
            .as_i64()
            .unwrap();
        head = wire
            .ok(
                "record-retirement-contribution",
                serde_json::json!({
                    "expected_head_seq": head,
                    "account_id": IRA,
                    "funding_account_id": BANK,
                    "amount_cents": 50_000,
                    "on": "2025-07-01",
                    "reference": "investment-activity-txn-1",
                }),
            )
            .await["head"]
            .as_i64()
            .unwrap();
        head = wire
            .ok(
                "record-retirement-distribution",
                serde_json::json!({
                    "expected_head_seq": head,
                    "account_id": IRA,
                    "receiving_account_id": BANK,
                    "gross_cents": 20_000,
                    "withheld_cents": 2_000,
                    "withheld_account_id": PREPAID_TAX,
                    "on": "2025-08-01",
                    "reference": "investment-activity-txn-2",
                }),
            )
            .await["head"]
            .as_i64()
            .unwrap();
        wire.ok(
            "configure-investment-account",
            serde_json::json!({
                "expected_head_seq": head,
                "item_id": "item1",
                "plaid_account_id": "acct-1",
                "accounts": taxable_accounts(),
                "plaid_subtype": "brokerage",
            }),
        )
        .await;

        let hosted_log = events_after(&handle, baseline);

        assert_eq!(
            local_log.len(),
            hosted_log.len(),
            "the two books appended a different number of events"
        );
        for (i, (l, h)) in local_log.iter().zip(hosted_log.iter()).enumerate() {
            let left = scrub_ids(&serde_json::to_string_pretty(&l.event).unwrap());
            let right = scrub_ids(&serde_json::to_string_pretty(&h.event).unwrap());
            assert_eq!(left, right, "event {i} differs between local and hosted");
        }
    }

    // -----------------------------------------------------------------------
    // The importer, hosted
    // -----------------------------------------------------------------------
    //
    // One planner, two sinks: everything below is about the sinks agreeing. The
    // planner's own decisions — which subtype becomes what, which rows are held —
    // are `investment_import`'s tests and are not repeated here.

    use crate::commands::investment_import as imp;
    use crate::sync::client::SyncClient;
    use crate::sync::replica;
    use rusqlite::OptionalExtension;

    /// A provider connection, as the importer's local tables hold it. Machine-local,
    /// so it does not arrive with the log and every copy inserts its own.
    fn connect_item(store: &EventStore) {
        store
            .connection()
            .execute(
                "INSERT OR IGNORE INTO plaid_items (id, proxy_item_id, institution_name)
                 VALUES ('item1','p1','Fidelity')",
                [],
            )
            .unwrap();
    }

    /// A replica of the group's books, arrived at the only way a replica ever arrives
    /// at anything: by pulling the server's log.
    fn replica_of(handle: &Arc<Mutex<EventStore>>) -> EventStore {
        let mut replica = EventStore::in_memory().unwrap();
        replica.init_schema().unwrap();
        connect_item(&replica);
        pull(handle, &mut replica);
        replica
    }

    /// Bring a replica up to the server's head, as the sync tick does.
    fn pull(handle: &Arc<Mutex<EventStore>>, replica: &mut EventStore) {
        let from = replica::local_cursor(replica).unwrap();
        let events: Vec<crate::sync::SyncEvent> = handle
            .lock()
            .unwrap()
            .get_after(from)
            .unwrap()
            .into_iter()
            .map(Into::into)
            .collect();
        replica::apply_batch(replica, &events).unwrap();
    }

    /// A client pointed at the loopback server, with the head it currently has.
    async fn client_for(wire: &Wire) -> SyncClient {
        SyncClient::with_head(wire.base.clone(), TOKEN, wire.head().await)
    }

    /// The taxable configuration the import tests use.
    ///
    /// `other_securities_account_id` is a **different account** from the stocks slot
    /// on purpose: which slot a position is carried in comes from the kind our master
    /// holds, and a hosted import that read that kind out of a replica which has not
    /// pulled the definition yet would find nothing and file a stock in here. With
    /// both slots pointing at one account no test could see the difference.
    fn importing_accounts() -> InvestmentPostingAccounts {
        InvestmentPostingAccounts::Taxable(Box::new(TaxableBrokerageAccounts {
            stocks_account_id: SECURITIES.into(),
            mutual_funds_account_id: None,
            other_securities_account_id: Some(OTHER_SECURITIES.into()),
            cash_account_id: CASH.into(),
            dividend_income_account_id: DIVIDENDS.into(),
            interest_income_account_id: INTEREST.into(),
            tax_exempt_interest_account_id: None,
            capital_gain_distribution_account_id: None,
            realized_gain_account_id: GAIN.into(),
            fee_expense_account_id: FEES.into(),
            transfer_clearing_account_id: Some(CLEARING.into()),
        }))
    }

    /// The chart, the connection, the retirement register and both account
    /// configurations — everything an import needs before it starts, appended the
    /// same way into whichever book is about to run one.
    fn ready_to_import() -> EventStore {
        let mut store = store();
        register_account(
            &mut store,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: IRA.into(),
                institution: "Fidelity ••5678".into(),
                kind: RetirementKind::Traditional,
                value_change_account_id: VALUE_CHANGE.into(),
            },
        )
        .expect("registered");
        configure_account(
            &mut store,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: "item1".into(),
                plaid_account_id: "acct-1".into(),
                accounts: importing_accounts(),
                plaid_subtype: Some("brokerage".into()),
            },
        )
        .expect("configured the brokerage");
        configure_account(
            &mut store,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: "item1".into(),
                plaid_account_id: "acct-2".into(),
                accounts: InvestmentPostingAccounts::Sheltered {
                    retirement_account_id: IRA.into(),
                },
                plaid_subtype: Some("ira".into()),
            },
        )
        .expect("configured the IRA");
        store
    }

    fn provider_accounts() -> Vec<imp::ProviderAccount> {
        vec![
            imp::ProviderAccount {
                account_id: "acct-1".into(),
                name: "Brokerage".into(),
                subtype: Some("brokerage".into()),
                mask: Some("1234".into()),
            },
            imp::ProviderAccount {
                account_id: "acct-2".into(),
                name: "Rollover IRA".into(),
                subtype: Some("ira".into()),
                mask: Some("5678".into()),
            },
        ]
    }

    fn acme_security() -> imp::ProviderSecurity {
        imp::ProviderSecurity {
            security_id: "sec-acme".into(),
            ticker: Some("ACME".into()),
            name: Some("Acme Corp".into()),
            security_type: Some("stock".into()),
            cusip: Some("037833100".into()),
            ..Default::default()
        }
    }

    fn fund_security() -> imp::ProviderSecurity {
        imp::ProviderSecurity {
            security_id: "sec-fund".into(),
            ticker: Some("TDF2050".into()),
            name: Some("Target 2050".into()),
            security_type: Some("mutual fund".into()),
            ..Default::default()
        }
    }

    /// One payload exercising every branch that writes: a purchase, a sale out of
    /// it, a dividend, a fee, a cash transfer, a corporate action that is held, and a
    /// trade inside the sheltered account that is ignored.
    fn provider_transactions() -> Vec<imp::ProviderInvestmentTransaction> {
        let on_acme = |id: &str, ty: &str, sub: &str| imp::ProviderInvestmentTransaction {
            investment_transaction_id: id.into(),
            account_id: "acct-1".into(),
            security_id: Some("sec-acme".into()),
            security: Some(acme_security()),
            date: "2025-03-10".into(),
            name: format!("ACME {sub}"),
            transaction_type: ty.into(),
            subtype: sub.into(),
            ..Default::default()
        };
        vec![
            imp::ProviderInvestmentTransaction {
                quantity: 10.0,
                price: 100.0,
                amount: 1_000.0,
                ..on_acme("tx-buy", "buy", "buy")
            },
            imp::ProviderInvestmentTransaction {
                date: "2025-04-01".into(),
                quantity: -4.0,
                price: 150.0,
                fees: Some(9.95),
                amount: -590.05,
                ..on_acme("tx-sell", "sell", "sell")
            },
            imp::ProviderInvestmentTransaction {
                date: "2025-04-15".into(),
                amount: -12.34,
                ..on_acme("tx-dividend", "cash", "dividend")
            },
            imp::ProviderInvestmentTransaction {
                investment_transaction_id: "tx-fee".into(),
                account_id: "acct-1".into(),
                date: "2025-04-30".into(),
                name: "Advisory fee".into(),
                transaction_type: "fee".into(),
                subtype: "management fee".into(),
                amount: 5.0,
                ..Default::default()
            },
            imp::ProviderInvestmentTransaction {
                investment_transaction_id: "tx-deposit".into(),
                account_id: "acct-1".into(),
                date: "2025-05-02".into(),
                name: "Transfer in".into(),
                transaction_type: "cash".into(),
                subtype: "deposit".into(),
                amount: -500.0,
                ..Default::default()
            },
            // Never guessed, on either path: a split applied wrongly restates every
            // gain on the security for ever.
            imp::ProviderInvestmentTransaction {
                date: "2025-05-10".into(),
                quantity: 10.0,
                ..on_acme("tx-split", "cash", "split")
            },
            // Inside the sheltered account: ignored by design, and recorded nowhere.
            imp::ProviderInvestmentTransaction {
                investment_transaction_id: "tx-ira-buy".into(),
                account_id: "acct-2".into(),
                security_id: Some("sec-fund".into()),
                security: Some(fund_security()),
                date: "2025-05-15".into(),
                name: "TDF2050 buy".into(),
                transaction_type: "buy".into(),
                subtype: "buy".into(),
                quantity: 12.0,
                amount: 480.0,
                ..Default::default()
            },
        ]
    }

    fn provider_holdings() -> Vec<imp::ProviderHolding> {
        vec![
            imp::ProviderHolding {
                account_id: "acct-1".into(),
                security_id: "sec-acme".into(),
                security: Some(acme_security()),
                quantity: 6.0,
                cost_basis: Some(600.0),
                institution_value: Some(720.0),
                ..Default::default()
            },
            imp::ProviderHolding {
                account_id: "acct-2".into(),
                security_id: "sec-fund".into(),
                security: Some(fund_security()),
                quantity: 100.0,
                cost_basis: Some(1_000.0),
                institution_value: Some(1_234.56),
                ..Default::default()
            },
        ]
    }

    fn as_of() -> NaiveDate {
        day(2025, 6, 30)
    }

    /// The property the whole hosted path exists to have: the same payload imported
    /// into a local book and into a group's produces the same log.
    ///
    /// Not the same balances and not the same entries — the same **events**, in the
    /// same order, scrubbed only of the ids each book mints for itself. That is what
    /// catches an importer that grew a second opinion on the way over the wire: a
    /// missing `InvestmentActivityImported` (so the next fetch imports the purchase
    /// again and a Form 8949 deducts it twice), a position filed in the wrong
    /// securities slot because the kind was read off a replica that had not pulled the
    /// definition, a memo built from something the server does not have, a snapshot
    /// left local, a sheltered account's value never set.
    #[tokio::test]
    async fn a_hosted_import_and_a_local_import_of_one_payload_produce_the_same_log() {
        let accounts = provider_accounts();
        let transactions = provider_transactions();
        let holdings = provider_holdings();

        // --- the local book ---
        let mut local = ready_to_import();
        let baseline = local.latest_id().unwrap().unwrap_or(0);
        let local_report =
            imp::import_transactions(&mut local, "u", "item1", &accounts, &transactions)
                .expect("imported locally");
        let local_holdings =
            imp::import_holdings(&mut local, "u", "item1", as_of(), &holdings, &accounts)
                .expect("holdings locally");
        let local_log = local.get_after(baseline).unwrap();

        // --- the group's books, over the wire ---
        let (wire, handle) = serve(ready_to_import()).await;
        assert_eq!(
            wire.head().await,
            baseline,
            "the two books start from the same chart and the same configuration"
        );
        let replica = replica_of(&handle);
        let mut client = client_for(&wire).await;
        let hosted_report =
            imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &transactions)
                .await
                .expect("imported to the group");
        let hosted_holdings = imp::import_holdings_hosted(
            &replica,
            &mut client,
            "item1",
            as_of(),
            &holdings,
            &accounts,
        )
        .await
        .expect("holdings to the group");
        let hosted_log = events_after(&handle, baseline);

        assert_eq!(
            local_log.len(),
            hosted_log.len(),
            "the two books appended a different number of events"
        );
        for (i, (l, h)) in local_log.iter().zip(hosted_log.iter()).enumerate() {
            assert_eq!(
                scrub_ids(&serde_json::to_string_pretty(&l.event).unwrap()),
                scrub_ids(&serde_json::to_string_pretty(&h.event).unwrap()),
                "event {i} of the import differs between a local book and a hosted one"
            );
        }

        // And the reports agree about everything a person is shown, bar the
        // reconciliation the hosted path leaves to the caller (both halves of it are
        // one pull behind).
        assert_eq!(local_report, hosted_report);
        assert_eq!(
            (local_holdings.recorded, local_holdings.unchanged),
            (hosted_holdings.recorded, hosted_holdings.unchanged)
        );
        assert_eq!(
            local_holdings.securities_created,
            hosted_holdings.securities_created
        );
        assert_eq!(
            local_holdings
                .values_set
                .iter()
                .map(|(a, v)| (a.clone(), v.change_cents))
                .collect::<Vec<_>>(),
            hosted_holdings
                .values_set
                .iter()
                .map(|(a, v)| (a.clone(), v.change_cents))
                .collect::<Vec<_>>(),
            "the sheltered account's value update has to come to the same thing"
        );
        assert_eq!(local_report.bought, 1, "the payload did post something");
        assert_eq!(local_report.held, 1, "and held the corporate action");
        assert!(!local_holdings.reconciliations.is_empty());
        assert!(
            hosted_holdings.reconciliations.is_empty(),
            "a hosted reconciliation would be computed from a replica that is one pull behind"
        );
    }

    /// The pull is what brings a hosted import home: the register, the security
    /// master and the mapping all arrive through the mirror path, under the ids the
    /// **server** chose.
    #[tokio::test]
    async fn a_security_minted_by_the_server_reaches_the_local_mapping_under_the_servers_id() {
        let accounts = provider_accounts();
        let transactions = provider_transactions();
        let (wire, handle) = serve(ready_to_import()).await;
        let mut replica = replica_of(&handle);
        let mut client = client_for(&wire).await;

        let report =
            imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &transactions)
                .await
                .expect("imported");
        assert_eq!(report.securities_created, 1, "ACME was minted server-side");

        // The id the server chose, read off the group's own log.
        let server_id: String = handle
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT security_id FROM plaid_securities WHERE plaid_security_id = 'sec-acme'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Nothing locally yet: the importer appended nothing to the replica, which is
        // the whole point of the hosted path.
        let mapped: Option<String> = replica
            .connection()
            .query_row(
                "SELECT security_id FROM plaid_securities WHERE plaid_security_id = 'sec-acme'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(mapped, None, "a replica's projections have one writer");

        pull(&handle, &mut replica);
        let mapped: String = replica
            .connection()
            .query_row(
                "SELECT security_id FROM plaid_securities WHERE plaid_security_id = 'sec-acme'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            mapped, server_id,
            "the local mapping must hold the id the server minted, never one of its own"
        );
        // And the lot the purchase opened is carried in the stocks slot, because the
        // kind came back with the id rather than being read out of a replica that had
        // never heard of the security.
        let account: String = replica
            .connection()
            .query_row(
                "SELECT securities_account_id FROM investment_lots WHERE security_id = ?1",
                [&mapped],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(account, SECURITIES);
    }

    /// Interrupted **before** the server accepted: nothing is recorded as imported,
    /// anywhere, and the next run posts the transaction exactly once.
    ///
    /// The interruption is a server that is not there, which is the honest shape of
    /// one: the submit fails, the run fails, and the question is what was left behind.
    #[tokio::test]
    async fn an_import_interrupted_before_the_server_accepted_records_nothing() {
        let accounts = provider_accounts();
        let transactions = provider_transactions();
        let (wire, handle) = serve(ready_to_import()).await;
        let head_before = wire.head().await;
        let mut replica = replica_of(&handle);

        // A client pointed at nothing at all.
        let mut broken = SyncClient::with_head("http://127.0.0.1:1", TOKEN, head_before);
        let outcome =
            imp::import_transactions_hosted(&replica, &mut broken, "item1", &accounts, &transactions)
                .await;
        assert!(
            outcome.is_err(),
            "a transport that is down must fail the run, not hold every row as refused"
        );

        assert_eq!(wire.head().await, head_before, "the group's log did not move");
        let registered: i64 = replica
            .connection()
            .query_row("SELECT COUNT(*) FROM investment_imports", [], |r| r.get(0))
            .unwrap();
        assert_eq!(registered, 0, "nothing was recorded as imported");
        let held: i64 = replica
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM investment_staged_activity",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(held, 0, "and nothing was held either");

        // And the run that follows posts each transaction once.
        let mut client = client_for(&wire).await;
        let report =
            imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &transactions)
                .await
                .expect("imported");
        assert_eq!((report.bought, report.sold, report.duplicates), (1, 1, 0));
        pull(&handle, &mut replica);
        let buys: i64 = replica
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM investment_imports WHERE outcome = 'buy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(buys, 1);
    }

    /// Interrupted **after** the server accepted: the register is not lost. It is in
    /// the group's log, it arrives at the next pull, and a re-import that beats the
    /// pull is refused by the server's own fence and counted as the duplicate it is —
    /// not posted twice, and not parked in the review list.
    #[tokio::test]
    async fn an_import_interrupted_after_the_server_accepted_leaves_the_register_consistent() {
        let accounts = provider_accounts();
        let one = vec![provider_transactions()[0].clone()];
        let (wire, handle) = serve(ready_to_import()).await;
        let mut replica = replica_of(&handle);
        let mut client = client_for(&wire).await;

        let report = imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &one)
            .await
            .expect("imported");
        assert_eq!(report.bought, 1);

        // The interruption: the process stopped here, so this copy never pulled. The
        // fence is nonetheless in the group's log.
        let fenced: i64 = handle
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM investment_imports WHERE provider_transaction_id = 'tx-buy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fenced, 1, "the group holds the record the local copy is missing");
        let locally: i64 = replica
            .connection()
            .query_row("SELECT COUNT(*) FROM investment_imports", [], |r| r.get(0))
            .unwrap();
        assert_eq!(locally, 0, "and the local copy does not know yet");

        // The next run, still before any pull: the planner cannot see the fence, the
        // server can.
        let head = wire.head().await;
        let mut client = client_for(&wire).await;
        let again = imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &one)
            .await
            .expect("imported again");
        assert_eq!(
            (again.bought, again.duplicates, again.held),
            (0, 1, 0),
            "a re-import that beat the pull is a duplicate, not a posting and not a hold"
        );
        assert_eq!(wire.head().await, head, "and nothing was appended for it");

        // Then the pull, and the register is consistent with no repair.
        pull(&handle, &mut replica);
        let locally: i64 = replica
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM investment_imports WHERE provider_transaction_id = 'tx-buy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(locally, 1);
        let third = imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &one)
            .await
            .expect("imported a third time");
        assert_eq!((third.bought, third.duplicates), (0, 1));
    }

    /// A `422` is the books refusing this transaction, and the row is held carrying
    /// the server's own wording — exactly as a local refusal is.
    ///
    /// The refusal here is a sale of more shares than the position holds, which is a
    /// judgement only the write lock can make.
    #[tokio::test]
    async fn a_refusal_from_the_server_holds_the_row_with_the_servers_wording() {
        let accounts = provider_accounts();
        let oversold = imp::ProviderInvestmentTransaction {
            investment_transaction_id: "tx-oversell".into(),
            account_id: "acct-1".into(),
            security_id: Some("sec-acme".into()),
            security: Some(acme_security()),
            date: "2025-04-01".into(),
            name: "ACME sell".into(),
            transaction_type: "sell".into(),
            subtype: "sell".into(),
            quantity: -400.0,
            amount: -60_000.0,
            ..Default::default()
        };
        let (wire, handle) = serve(ready_to_import()).await;
        let replica = replica_of(&handle);
        let mut client = client_for(&wire).await;

        let report = imp::import_transactions_hosted(
            &replica,
            &mut client,
            "item1",
            &accounts,
            &[oversold],
        )
        .await
        .expect("the run survives a refusal");
        assert_eq!((report.sold, report.held), (0, 1));

        let (reason, detail): (String, String) = replica
            .connection()
            .query_row(
                "SELECT reason, detail FROM investment_staged_activity
                  WHERE provider_transaction_id = 'tx-oversell'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(reason, "rejected");
        assert!(
            detail.contains("There are no lots of"),
            "the held row has to carry the server's own sentence, not ours: {detail}"
        );
        assert!(
            detail.contains("The books refused this one"),
            "and the guidance a person acts on: {detail}"
        );
        // Nothing posted, and the row is not fenced as imported — it is waiting for a
        // person, which is what makes importing it again after they act possible.
        assert_eq!(
            events_after(&handle, wire.head().await).len(),
            0,
            "a refusal appends nothing"
        );
    }

    /// Re-importing the same payload on a group's books changes nothing: not a
    /// transaction, not a snapshot, not a sheltered account's value.
    #[tokio::test]
    async fn re_importing_the_same_payload_on_hosted_books_changes_nothing() {
        let accounts = provider_accounts();
        let transactions = provider_transactions();
        let holdings = provider_holdings();
        let (wire, handle) = serve(ready_to_import()).await;
        let mut replica = replica_of(&handle);
        let mut client = client_for(&wire).await;

        imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &transactions)
            .await
            .expect("imported");
        imp::import_holdings_hosted(&replica, &mut client, "item1", as_of(), &holdings, &accounts)
            .await
            .expect("holdings");
        pull(&handle, &mut replica);
        let settled = wire.head().await;

        let report =
            imp::import_transactions_hosted(&replica, &mut client, "item1", &accounts, &transactions)
                .await
                .expect("imported again");
        let holdings_report = imp::import_holdings_hosted(
            &replica,
            &mut client,
            "item1",
            as_of(),
            &holdings,
            &accounts,
        )
        .await
        .expect("holdings again");

        assert_eq!(
            wire.head().await,
            settled,
            "a second import of one payload appended {} event(s)",
            wire.head().await - settled
        );
        assert_eq!(report.posted(), 0);
        assert_eq!(report.duplicates, 6, "every row was already dealt with");
        assert_eq!(
            (holdings_report.recorded, holdings_report.unchanged),
            (0, 2),
            "both snapshots were already on file"
        );
        assert!(holdings_report.values_set.is_empty());
    }
}
