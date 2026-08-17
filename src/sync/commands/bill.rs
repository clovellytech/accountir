//! Bill & invoice command endpoints over the sync transport (receive-bill,
//! issue-invoice — composite commands that emit several events atomically).
//!
//! Like `post-entry` (see `sync/mod.rs`), each endpoint is bearer-authenticated,
//! runs the command's real domain invariants *inside* the append transaction
//! under the client's `expected_head_seq`, and returns 200 + new head / 409 stale
//! head / 422 domain rejection. Unlike `post-entry` these are *composite*: they
//! emit several events atomically via [`EventStore::append_checked_many`], so the
//! shared in-txn helpers (`build_receive_bill_in_txn` /
//! `build_issue_invoice_in_txn`) return the raw events and this endpoint stamps
//! the authenticated actor on each (the local handlers stamp their own user).
//!
//! The request DTOs derive `Serialize` as well as `Deserialize` so the client half
//! ([`crate::sync::client::SyncClient`]) builds its bodies from the *same* structs
//! the server parses. The failure that prevents: a hand-rolled `json!` body on the
//! client drifting one field name away from the server's DTO, which serde answers
//! with a silent default or a 422 nobody can explain from either side of the wire.

use crate::commands::bill_commands::{
    build_receive_bill_in_txn, check_receive_bill_pure, BillStep, ReceiveBillCommand,
};
use crate::commands::invoice_commands::{
    build_issue_invoice_in_txn, check_issue_invoice_pure, InvoiceStep, IssueInvoiceCommand,
};
use crate::domain::PaymentTerms;
use crate::events::types::StoredEvent;
use crate::store::event_store::{CheckedOutcome, Verdict};
use crate::sync::{project, stamp, ApiError, AuthedUser, SubmitResponse, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/receive-bill", post(submit_receive_bill))
        .route("/sync/commands/issue-invoice", post(submit_issue_invoice))
}

/// Map a composite (`append_checked_many`) outcome to an HTTP response. The new
/// head is the LAST appended event's seq (the batch is appended in order, so the
/// final row is the log head). Mirrors `sync::outcome_to_response`, which only
/// handles the single-event `append_checked` outcome.
fn many_outcome_to_response<E: std::fmt::Display>(
    outcome: CheckedOutcome<Vec<StoredEvent>, E>,
) -> Result<Json<SubmitResponse>, ApiError> {
    match outcome {
        CheckedOutcome::Appended(events) => Ok(Json(SubmitResponse {
            head: events
                .last()
                .map(|s| s.id)
                .expect("a composite command appends at least one event"),
        })),
        CheckedOutcome::HeadMismatch { actual, .. } => Err(ApiError::conflict(actual)),
        CheckedOutcome::Rejected(e) => Err(ApiError::domain(e)),
    }
}

// --- receive-bill ---

#[derive(Serialize, Deserialize)]
pub struct ReceiveBillRequest {
    pub expected_head_seq: i64,
    pub vendor: String,
    pub amount: i64,
    pub currency: String,
    pub issue_date: NaiveDate,
    pub terms: PaymentTerms,
    #[serde(default)]
    pub memo: Option<String>,
    /// The debit side: what the bill was for. An expense, or an asset such as
    /// inventory — see [`ReceiveBillCommand::debit_account_id`].
    ///
    /// Still `expense_account_id` on the wire. Renaming it would mean a desktop
    /// and an instance disagreeing about a field name for however long it takes
    /// every group to upgrade, which is a real outage to buy a better spelling.
    ///
    /// [`ReceiveBillCommand::debit_account_id`]:
    /// crate::commands::bill_commands::ReceiveBillCommand::debit_account_id
    #[serde(rename = "expense_account_id")]
    pub debit_account_id: String,
    pub ap_account_id: String,
    #[serde(default)]
    pub reference: Option<String>,
}

/// Receive a bill over the wire. Emits the bill's journal entry and `BillReceived`
/// atomically. Runs the SAME invariants the local `receive_bill` handler uses —
/// pure amount check, then in-txn reference dedup + accounts-active / period-open
/// fences via [`build_receive_bill_in_txn`] — honoring the client's
/// `expected_head_seq`. A bad bill is a `422`, a stale head a `409`.
async fn submit_receive_bill(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ReceiveBillRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = ReceiveBillCommand {
        vendor: req.vendor,
        amount: req.amount,
        currency: req.currency,
        issue_date: req.issue_date,
        terms: req.terms,
        memo: req.memo,
        debit_account_id: req.debit_account_id,
        ap_account_id: req.ap_account_id,
        reference: req.reference,
    };

    // Pure (state-independent) validation → 422 without touching the store.
    check_receive_bill_pure(&cmd).map_err(ApiError::domain)?;

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            req.expected_head_seq,
            move |tx| match build_receive_bill_in_txn(tx, &cmd)? {
                // Sync path: stamp the authenticated actor on each event.
                BillStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                BillStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    many_outcome_to_response(outcome)
}

// --- issue-invoice ---

#[derive(Serialize, Deserialize)]
pub struct IssueInvoiceRequest {
    pub expected_head_seq: i64,
    pub customer: String,
    pub amount: i64,
    pub currency: String,
    pub issue_date: NaiveDate,
    pub terms: PaymentTerms,
    #[serde(default)]
    pub memo: Option<String>,
    pub revenue_account_id: String,
    pub ar_account_id: String,
}

/// Issue an invoice over the wire. Emits the invoice's journal entry and
/// `InvoiceIssued` atomically. Runs the SAME invariants the local `issue_invoice`
/// handler uses — pure amount check, then in-txn accounts-active / period-open
/// fences via [`build_issue_invoice_in_txn`] — honoring the client's
/// `expected_head_seq`. A bad invoice is a `422`, a stale head a `409`.
async fn submit_issue_invoice(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<IssueInvoiceRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = IssueInvoiceCommand {
        customer: req.customer,
        amount: req.amount,
        currency: req.currency,
        issue_date: req.issue_date,
        terms: req.terms,
        memo: req.memo,
        revenue_account_id: req.revenue_account_id,
        ar_account_id: req.ar_account_id,
    };

    // Pure (state-independent) validation → 422 without touching the store.
    check_issue_invoice_pure(&cmd).map_err(ApiError::domain)?;

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            req.expected_head_seq,
            move |tx| match build_issue_invoice_in_txn(tx, &cmd)? {
                // Sync path: stamp the authenticated actor on each event.
                InvoiceStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                InvoiceStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    many_outcome_to_response(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{
        AccountCommands, CreateAccountCommand, DeactivateAccountCommand,
    };
    use crate::domain::AccountType;
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::router;
    use std::collections::HashMap;

    pub(super) const TOKEN: &str = "tok-1";
    const ACTOR: &str = "user-1";

    pub(super) fn tokens() -> HashMap<String, String> {
        HashMap::from([(TOKEN.to_string(), ACTOR.to_string())])
    }

    pub(super) async fn serve(state: SyncState) -> String {
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    pub(super) fn mk_account(store: &mut EventStore, num: &str, ty: AccountType) -> String {
        let stored = AccountCommands::new(store, "seed".to_string())
            .create_account(CreateAccountCommand {
                account_type: ty,
                account_number: num.to_string(),
                name: format!("Acct {num}"),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
        match &stored.event {
            Event::AccountCreated { account_id, .. } => account_id.clone(),
            _ => unreachable!(),
        }
    }

    fn head_of(v: &serde_json::Value) -> i64 {
        v["head"].as_i64().unwrap()
    }

    /// A store seeded (via the command handlers) with the accounts both commands
    /// need: expense/AP for bills, revenue/AR for invoices. Head starts at 4.
    struct Accounts {
        expense: String,
        ap: String,
        revenue: String,
        ar: String,
    }

    fn seed_accounts(store: &mut EventStore) -> Accounts {
        Accounts {
            expense: mk_account(store, "5000", AccountType::Expense),
            ap: mk_account(store, "2000", AccountType::Liability),
            revenue: mk_account(store, "4000", AccountType::Revenue),
            ar: mk_account(store, "1100", AccountType::Asset),
        }
    }

    async fn serve_with_accounts() -> (String, Accounts) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let accts = seed_accounts(&mut store);
        let base = serve(SyncState::new(store, tokens())).await;
        (base, accts)
    }

    fn bill_body(expected_head: i64, expense: &str, ap: &str) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": expected_head,
            "vendor": "V",
            "amount": 10_000,
            "currency": "USD",
            "issue_date": "2026-07-03",
            "terms": { "type": "net", "days": 30 },
            "expense_account_id": expense,
            "ap_account_id": ap,
        })
    }

    fn invoice_body(expected_head: i64, revenue: &str, ar: &str) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": expected_head,
            "customer": "C",
            "amount": 10_000,
            "currency": "USD",
            "issue_date": "2026-07-03",
            "terms": { "type": "net", "days": 30 },
            "revenue_account_id": revenue,
            "ar_account_id": ar,
        })
    }

    #[tokio::test]
    async fn receive_bill_command_happy_path_advances_head_by_two() {
        let (base, a) = serve_with_accounts().await; // head starts at 4
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/receive-bill");

        // Happy path: emits JournalEntryPosted + BillReceived → head 4 + 2 = 6.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&bill_body(4, &a.expense, &a.ap))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), 6);
    }

    #[tokio::test]
    async fn issue_invoice_command_happy_path_advances_head_by_two() {
        let (base, a) = serve_with_accounts().await; // head starts at 4
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/issue-invoice");

        // Happy path: emits JournalEntryPosted + InvoiceIssued → head 4 + 2 = 6.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&invoice_body(4, &a.revenue, &a.ar))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), 6);
    }

    #[tokio::test]
    async fn receive_bill_rejects_inactive_account_server_side() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let a = seed_accounts(&mut store);
        // Deactivate the expense account (zero balance) so the in-txn fence rejects.
        AccountCommands::new(&mut store, "seed".to_string())
            .deactivate_account(DeactivateAccountCommand {
                account_id: a.expense.clone(),
                reason: None,
            })
            .unwrap();
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/receive-bill"))
            .bearer_auth(TOKEN)
            .json(&bill_body(head, &a.expense, &a.ap))
            .send()
            .await
            .unwrap();
        // The server enforces the fence: inactive account → 422, not a blind append.
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("inactive"));
    }

    #[tokio::test]
    async fn receive_bill_stale_head_conflicts() {
        let (base, a) = serve_with_accounts().await; // head starts at 4
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/receive-bill");

        // First bill lands, moving the log to 6.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&bill_body(4, &a.expense, &a.ap))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);

        // A second submit against the now-stale head 4 → 409 with current head 6.
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&bill_body(4, &a.expense, &a.ap))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        let cur = stale.json::<serde_json::Value>().await.unwrap()["current_head"]
            .as_i64()
            .unwrap();
        assert_eq!(cur, 6);
    }

    #[tokio::test]
    async fn receive_bill_requires_token() {
        let (base, a) = serve_with_accounts().await;
        // Missing bearer token → 401, before any command runs.
        let unauth = reqwest::Client::new()
            .post(format!("{base}/sync/commands/receive-bill"))
            .json(&bill_body(4, &a.expense, &a.ap))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
    /// Same wiring regression as `bill_ops`: the desktop's only route to these two
    /// commands is [`SyncClient`], and a path or field-name drift between the two
    /// halves is invisible until a user reports a bill that "didn't save".
    ///
    /// [`SyncClient`]: crate::sync::client::SyncClient
    #[tokio::test]
    async fn the_client_reaches_receive_bill_and_issue_invoice() {
        use crate::sync::client::SyncClient;

        let (base, a) = serve_with_accounts().await;
        let head = 4;
        let mut client = SyncClient::with_head(base, TOKEN, head);
        let date = NaiveDate::from_ymd_opt(2026, 7, 3).unwrap();

        let after_bill = client
            .receive_bill(
                "V".to_string(),
                10_000,
                "USD".to_string(),
                date,
                PaymentTerms::Net { days: 30 },
                None,
                a.expense.clone(),
                a.ap.clone(),
                None,
            )
            .await
            .expect("receive-bill");
        assert_eq!(
            after_bill,
            head + 2,
            "a bill appends its entry and the event"
        );

        let after_invoice = client
            .issue_invoice(
                "C".to_string(),
                10_000,
                "USD".to_string(),
                date,
                PaymentTerms::Net { days: 30 },
                None,
                a.revenue.clone(),
                a.ar.clone(),
            )
            .await
            .expect("issue-invoice");
        assert_eq!(after_invoice, head + 4);
    }
}

/// The debit side of a bill is not always an expense.
#[cfg(test)]
mod bill_debit_side {
    use super::tests::{mk_account, serve, tokens, TOKEN};
    use super::*;
    use crate::domain::AccountType;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    /// A stock purchase: the money owed is a payable, and what arrived is an
    /// asset.
    ///
    /// The ledger has always allowed this — the only fences on either account are
    /// that it exists, is active, and the period is open. What stopped it was the
    /// desktop filtering its picker to expense accounts, which it did because the
    /// field was called `expense_account_id`. A shop could not enter a bill for
    /// inventory at all.
    #[tokio::test]
    async fn a_bill_can_debit_an_asset_such_as_inventory() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let inventory = mk_account(&mut store, "1300", AccountType::Asset);
        let ap = mk_account(&mut store, "2000", AccountType::Liability);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/receive-bill"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "vendor": "Quality Bicycle Products",
                "amount": 125_000,
                "currency": "USD",
                "issue_date": "2026-07-03",
                "terms": { "type": "net", "days": 30 },
                "expense_account_id": inventory,
                "ap_account_id": ap,
            }))
            .send()
            .await
            .unwrap();
        assert!(
            r.status().is_success(),
            "a bill for inventory was refused: HTTP {}",
            r.status()
        );
    }

    /// The wire field is still spelled `expense_account_id`.
    ///
    /// Only the *code* was renamed. A desktop sending a new spelling to an
    /// instance that has not been upgraded — or the reverse — is an outage lasting
    /// as long as the slowest group takes to update, bought for a better field
    /// name. Asserted so a future rename has to be deliberate.
    #[test]
    fn the_wire_still_says_expense_account_id() {
        let body = serde_json::json!({
            "expected_head_seq": 0,
            "vendor": "V",
            "amount": 1,
            "currency": "USD",
            "issue_date": "2026-07-03",
            "terms": { "type": "net", "days": 30 },
            "expense_account_id": "acct-1",
            "ap_account_id": "acct-2",
        });
        let parsed: ReceiveBillRequest =
            serde_json::from_value(body).expect("the server must still accept the old spelling");
        assert_eq!(parsed.debit_account_id, "acct-1");
    }
}
