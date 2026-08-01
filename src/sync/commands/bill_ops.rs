//! Bill & invoice operation endpoints over the sync transport: apply-bill-payment,
//! void-bill, receive-invoice-payment, void-invoice.
//!
//! Like `receive-bill` / `issue-invoice` (see `sync/commands/bill.rs`), each of
//! these is a bearer-authenticated *composite* command: it runs the command's
//! real domain invariants *inside* one [`EventStore::append_checked_many`]
//! transaction under the client's `expected_head_seq`, and emits two events
//! atomically (a payment/void `JournalEntryPosted`/`JournalEntryVoided` plus the
//! bill/invoice event). The shared in-txn helpers
//! (`build_apply_payment_in_txn`, `build_void_bill_in_txn`,
//! `build_receive_payment_in_txn`, `build_void_invoice_in_txn`) return the raw
//! events and this endpoint stamps the authenticated actor on each (the local
//! handlers stamp their own user). None of these commands have a pure
//! (state-independent) pre-check — the guards all read live projections under the
//! write lock — so there is no `check_*_pure` call here. A domain rejection is a
//! `422`, a stale head a `409`.
//!
//! The request DTOs derive `Serialize` as well as `Deserialize` so the client half
//! ([`crate::sync::client::SyncClient`]) builds its bodies from the *same* structs
//! the server parses. The failure that prevents: a hand-rolled `json!` body on the
//! client drifting one field name away from the server's DTO, which serde answers
//! with a silent default or a 422 nobody can explain from either side of the wire.

use crate::commands::bill_commands::{
    build_apply_payment_in_txn, build_void_bill_in_txn, ApplyBillPaymentCommand, BillStep,
    VoidBillCommand,
};
use crate::commands::invoice_commands::{
    build_receive_payment_in_txn, build_void_invoice_in_txn, InvoiceStep,
    ReceiveInvoicePaymentCommand, VoidInvoiceCommand,
};
use crate::events::types::StoredEvent;
use crate::store::event_store::{CheckedOutcome, Verdict};
use crate::sync::{project, stamp, ApiError, AuthedUser, SubmitResponse, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route(
            "/sync/commands/apply-bill-payment",
            post(submit_apply_bill_payment),
        )
        .route("/sync/commands/void-bill", post(submit_void_bill))
        .route(
            "/sync/commands/receive-invoice-payment",
            post(submit_receive_invoice_payment),
        )
        .route("/sync/commands/void-invoice", post(submit_void_invoice))
}

/// Map a composite (`append_checked_many`) outcome to an HTTP response. The new
/// head is the LAST appended event's seq (the batch is appended in order, so the
/// final row is the log head). Private mirror of the same helper in
/// `sync/commands/bill.rs`.
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

// --- apply-bill-payment ---

#[derive(Serialize, Deserialize)]
pub struct ApplyBillPaymentRequest {
    pub expected_head_seq: i64,
    pub bill_id: String,
    pub payment_date: NaiveDate,
    pub amount_applied: i64,
    pub payment_account_id: String,
    pub ap_account_id: String,
    #[serde(default)]
    pub memo: Option<String>,
}

/// Apply a payment to a bill over the wire. Emits the payment journal entry and
/// `BillPaymentApplied` atomically. Runs the SAME invariants the local
/// `apply_payment` handler uses — the "cumulative payments ≤ amount" guard +
/// status checks, then the payment entry's accounts-active / period-open fences,
/// via [`build_apply_payment_in_txn`] — honoring the client's `expected_head_seq`.
/// An overpayment/void/paid bill is a `422`, a stale head a `409`.
async fn submit_apply_bill_payment(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ApplyBillPaymentRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = ApplyBillPaymentCommand {
        bill_id: req.bill_id,
        payment_date: req.payment_date,
        amount_applied: req.amount_applied,
        payment_account_id: req.payment_account_id,
        ap_account_id: req.ap_account_id,
        memo: req.memo,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            req.expected_head_seq,
            move |tx| match build_apply_payment_in_txn(tx, &cmd)? {
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

// --- void-bill ---

#[derive(Serialize, Deserialize)]
pub struct VoidBillRequest {
    pub expected_head_seq: i64,
    pub bill_id: String,
    pub reason: String,
}

/// Void a bill over the wire. Emits `JournalEntryVoided` and `BillVoided`
/// atomically. Runs the SAME invariants the local `void_bill` handler uses — the
/// no-payments guard + entry-not-already-voided guard via
/// [`build_void_bill_in_txn`] — honoring the client's `expected_head_seq`. A bill
/// with payments (or already void) is a `422`, a stale head a `409`.
async fn submit_void_bill(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<VoidBillRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = VoidBillCommand {
        bill_id: req.bill_id,
        reason: req.reason,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            req.expected_head_seq,
            move |tx| match build_void_bill_in_txn(tx, &cmd)? {
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

// --- receive-invoice-payment ---

#[derive(Serialize, Deserialize)]
pub struct ReceiveInvoicePaymentRequest {
    pub expected_head_seq: i64,
    pub invoice_id: String,
    pub payment_date: NaiveDate,
    pub amount_applied: i64,
    pub payment_account_id: String,
    pub ar_account_id: String,
    #[serde(default)]
    pub memo: Option<String>,
}

/// Receive a payment against an invoice over the wire. Emits the payment journal
/// entry and `InvoicePaymentReceived` atomically. Runs the SAME invariants the
/// local `receive_payment` handler uses — the "cumulative payments ≤ amount"
/// guard + status checks, then the payment entry's accounts-active / period-open
/// fences, via [`build_receive_payment_in_txn`] — honoring the client's
/// `expected_head_seq`. An overpayment/void/paid invoice is a `422`, a stale head
/// a `409`.
async fn submit_receive_invoice_payment(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ReceiveInvoicePaymentRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = ReceiveInvoicePaymentCommand {
        invoice_id: req.invoice_id,
        payment_date: req.payment_date,
        amount_applied: req.amount_applied,
        payment_account_id: req.payment_account_id,
        ar_account_id: req.ar_account_id,
        memo: req.memo,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            req.expected_head_seq,
            move |tx| match build_receive_payment_in_txn(tx, &cmd)? {
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

// --- void-invoice ---

#[derive(Serialize, Deserialize)]
pub struct VoidInvoiceRequest {
    pub expected_head_seq: i64,
    pub invoice_id: String,
    pub reason: String,
}

/// Void an invoice over the wire. Emits `JournalEntryVoided` and `InvoiceVoided`
/// atomically. Runs the SAME invariants the local `void_invoice` handler uses —
/// the no-payments guard + entry-not-already-voided guard via
/// [`build_void_invoice_in_txn`] — honoring the client's `expected_head_seq`. An
/// invoice with payments (or already void) is a `422`, a stale head a `409`.
async fn submit_void_invoice(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<VoidInvoiceRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = VoidInvoiceCommand {
        invoice_id: req.invoice_id,
        reason: req.reason,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            req.expected_head_seq,
            move |tx| match build_void_invoice_in_txn(tx, &cmd)? {
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
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::bill_commands::{BillCommands, ReceiveBillCommand};
    use crate::commands::invoice_commands::{InvoiceCommands, IssueInvoiceCommand};
    use crate::domain::{AccountType, PaymentTerms};
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::router;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";
    const ACTOR: &str = "user-1";

    fn tokens() -> HashMap<String, String> {
        HashMap::from([(TOKEN.to_string(), ACTOR.to_string())])
    }

    async fn serve(state: SyncState) -> String {
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn mk_account(store: &mut EventStore, num: &str, ty: AccountType) -> String {
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

    /// A store seeded (via the command handlers) with the accounts, a bill and an
    /// invoice all four endpoints operate on. `head` is the log head after seeding.
    struct Seed {
        ap: String,
        ar: String,
        cash: String,
        bill_id: String,
        invoice_id: String,
        head: i64,
    }

    fn seed(store: &mut EventStore) -> Seed {
        let expense = mk_account(store, "5000", AccountType::Expense);
        let ap = mk_account(store, "2000", AccountType::Liability);
        let revenue = mk_account(store, "4000", AccountType::Revenue);
        let ar = mk_account(store, "1100", AccountType::Asset);
        let cash = mk_account(store, "1000", AccountType::Asset);

        let date = NaiveDate::from_ymd_opt(2026, 7, 3).unwrap();
        let bill = BillCommands::new(store, "seed".to_string())
            .receive_bill(ReceiveBillCommand {
                vendor: "V".to_string(),
                amount: 10_000,
                currency: "USD".to_string(),
                issue_date: date,
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                expense_account_id: expense,
                ap_account_id: ap.clone(),
                reference: None,
            })
            .unwrap();
        let bill_id = match &bill.event {
            Event::BillReceived { bill_id, .. } => bill_id.clone(),
            _ => unreachable!(),
        };
        let invoice = InvoiceCommands::new(store, "seed".to_string())
            .issue_invoice(IssueInvoiceCommand {
                customer: "C".to_string(),
                amount: 10_000,
                currency: "USD".to_string(),
                issue_date: date,
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                revenue_account_id: revenue,
                ar_account_id: ar.clone(),
            })
            .unwrap();
        let invoice_id = match &invoice.event {
            Event::InvoiceIssued { invoice_id, .. } => invoice_id.clone(),
            _ => unreachable!(),
        };
        let head = store.latest_id().unwrap().unwrap_or(0);
        Seed {
            ap,
            ar,
            cash,
            bill_id,
            invoice_id,
            head,
        }
    }

    async fn serve_with_seed() -> (String, Seed) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let s = seed(&mut store);
        let base = serve(SyncState::new(store, tokens())).await;
        (base, s)
    }

    // --- apply-bill-payment ---

    fn bill_pay_body(
        head: i64,
        bill_id: &str,
        ap: &str,
        cash: &str,
        amt: i64,
    ) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": head,
            "bill_id": bill_id,
            "payment_date": "2026-07-10",
            "amount_applied": amt,
            "payment_account_id": cash,
            "ap_account_id": ap,
        })
    }

    #[tokio::test]
    async fn apply_bill_payment_happy_path_advances_head_by_two() {
        let (base, s) = serve_with_seed().await;
        let url = format!("{base}/sync/commands/apply-bill-payment");
        let ok = reqwest::Client::new()
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&bill_pay_body(s.head, &s.bill_id, &s.ap, &s.cash, 6_000))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        // JournalEntryPosted + BillPaymentApplied → head advances by 2.
        assert_eq!(head_of(&ok.json().await.unwrap()), s.head + 2);
    }

    #[tokio::test]
    async fn apply_bill_payment_overpayment_rejected_server_side() {
        let (base, s) = serve_with_seed().await;
        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/apply-bill-payment"))
            .bearer_auth(TOKEN)
            .json(&bill_pay_body(s.head, &s.bill_id, &s.ap, &s.cash, 20_000))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("exceeds"));
    }

    #[tokio::test]
    async fn apply_bill_payment_stale_head_conflicts() {
        let (base, s) = serve_with_seed().await;
        let url = format!("{base}/sync/commands/apply-bill-payment");
        let http = reqwest::Client::new();
        // First payment lands, moving the log forward by 2.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&bill_pay_body(s.head, &s.bill_id, &s.ap, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        // Second submit against the now-stale head → 409 with the current head.
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&bill_pay_body(s.head, &s.bill_id, &s.ap, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        let cur = stale.json::<serde_json::Value>().await.unwrap()["current_head"]
            .as_i64()
            .unwrap();
        assert_eq!(cur, s.head + 2);
    }

    #[tokio::test]
    async fn apply_bill_payment_requires_token() {
        let (base, s) = serve_with_seed().await;
        let unauth = reqwest::Client::new()
            .post(format!("{base}/sync/commands/apply-bill-payment"))
            .json(&bill_pay_body(s.head, &s.bill_id, &s.ap, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    // --- void-bill ---

    fn void_bill_body(head: i64, bill_id: &str) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": head,
            "bill_id": bill_id,
            "reason": "oops",
        })
    }

    #[tokio::test]
    async fn void_bill_happy_path_advances_head_by_two() {
        let (base, s) = serve_with_seed().await;
        let ok = reqwest::Client::new()
            .post(format!("{base}/sync/commands/void-bill"))
            .bearer_auth(TOKEN)
            .json(&void_bill_body(s.head, &s.bill_id))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        // JournalEntryVoided + BillVoided → head advances by 2.
        assert_eq!(head_of(&ok.json().await.unwrap()), s.head + 2);
    }

    #[tokio::test]
    async fn void_bill_with_payments_rejected_server_side() {
        let (base, s) = serve_with_seed().await;
        let http = reqwest::Client::new();
        // Apply a payment so the bill can no longer be voided.
        let paid = http
            .post(format!("{base}/sync/commands/apply-bill-payment"))
            .bearer_auth(TOKEN)
            .json(&bill_pay_body(s.head, &s.bill_id, &s.ap, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(paid.status(), reqwest::StatusCode::OK);
        // Void against the advanced head → HasPayments → 422.
        let r = http
            .post(format!("{base}/sync/commands/void-bill"))
            .bearer_auth(TOKEN)
            .json(&void_bill_body(s.head + 2, &s.bill_id))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("payments"));
    }

    #[tokio::test]
    async fn void_bill_stale_head_conflicts() {
        let (base, s) = serve_with_seed().await;
        let http = reqwest::Client::new();
        // A bill payment moves the head; the void's expected head is now stale.
        let paid = http
            .post(format!("{base}/sync/commands/apply-bill-payment"))
            .bearer_auth(TOKEN)
            .json(&bill_pay_body(s.head, &s.bill_id, &s.ap, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(paid.status(), reqwest::StatusCode::OK);
        let stale = http
            .post(format!("{base}/sync/commands/void-bill"))
            .bearer_auth(TOKEN)
            .json(&void_bill_body(s.head, &s.bill_id))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn void_bill_requires_token() {
        let (base, s) = serve_with_seed().await;
        let unauth = reqwest::Client::new()
            .post(format!("{base}/sync/commands/void-bill"))
            .json(&void_bill_body(s.head, &s.bill_id))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    // --- receive-invoice-payment ---

    fn inv_pay_body(
        head: i64,
        invoice_id: &str,
        ar: &str,
        cash: &str,
        amt: i64,
    ) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": head,
            "invoice_id": invoice_id,
            "payment_date": "2026-07-10",
            "amount_applied": amt,
            "payment_account_id": cash,
            "ar_account_id": ar,
        })
    }

    #[tokio::test]
    async fn receive_invoice_payment_happy_path_advances_head_by_two() {
        let (base, s) = serve_with_seed().await;
        let ok = reqwest::Client::new()
            .post(format!("{base}/sync/commands/receive-invoice-payment"))
            .bearer_auth(TOKEN)
            .json(&inv_pay_body(s.head, &s.invoice_id, &s.ar, &s.cash, 6_000))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), s.head + 2);
    }

    #[tokio::test]
    async fn receive_invoice_payment_overpayment_rejected_server_side() {
        let (base, s) = serve_with_seed().await;
        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/receive-invoice-payment"))
            .bearer_auth(TOKEN)
            .json(&inv_pay_body(s.head, &s.invoice_id, &s.ar, &s.cash, 20_000))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("exceeds"));
    }

    #[tokio::test]
    async fn receive_invoice_payment_stale_head_conflicts() {
        let (base, s) = serve_with_seed().await;
        let url = format!("{base}/sync/commands/receive-invoice-payment");
        let http = reqwest::Client::new();
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&inv_pay_body(s.head, &s.invoice_id, &s.ar, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&inv_pay_body(s.head, &s.invoice_id, &s.ar, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        let cur = stale.json::<serde_json::Value>().await.unwrap()["current_head"]
            .as_i64()
            .unwrap();
        assert_eq!(cur, s.head + 2);
    }

    #[tokio::test]
    async fn receive_invoice_payment_requires_token() {
        let (base, s) = serve_with_seed().await;
        let unauth = reqwest::Client::new()
            .post(format!("{base}/sync/commands/receive-invoice-payment"))
            .json(&inv_pay_body(s.head, &s.invoice_id, &s.ar, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    // --- void-invoice ---

    fn void_invoice_body(head: i64, invoice_id: &str) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": head,
            "invoice_id": invoice_id,
            "reason": "oops",
        })
    }

    #[tokio::test]
    async fn void_invoice_happy_path_advances_head_by_two() {
        let (base, s) = serve_with_seed().await;
        let ok = reqwest::Client::new()
            .post(format!("{base}/sync/commands/void-invoice"))
            .bearer_auth(TOKEN)
            .json(&void_invoice_body(s.head, &s.invoice_id))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), s.head + 2);
    }

    #[tokio::test]
    async fn void_invoice_with_payments_rejected_server_side() {
        let (base, s) = serve_with_seed().await;
        let http = reqwest::Client::new();
        let paid = http
            .post(format!("{base}/sync/commands/receive-invoice-payment"))
            .bearer_auth(TOKEN)
            .json(&inv_pay_body(s.head, &s.invoice_id, &s.ar, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(paid.status(), reqwest::StatusCode::OK);
        let r = http
            .post(format!("{base}/sync/commands/void-invoice"))
            .bearer_auth(TOKEN)
            .json(&void_invoice_body(s.head + 2, &s.invoice_id))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("payments"));
    }

    #[tokio::test]
    async fn void_invoice_stale_head_conflicts() {
        let (base, s) = serve_with_seed().await;
        let http = reqwest::Client::new();
        let paid = http
            .post(format!("{base}/sync/commands/receive-invoice-payment"))
            .bearer_auth(TOKEN)
            .json(&inv_pay_body(s.head, &s.invoice_id, &s.ar, &s.cash, 1_000))
            .send()
            .await
            .unwrap();
        assert_eq!(paid.status(), reqwest::StatusCode::OK);
        let stale = http
            .post(format!("{base}/sync/commands/void-invoice"))
            .bearer_auth(TOKEN)
            .json(&void_invoice_body(s.head, &s.invoice_id))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn void_invoice_requires_token() {
        let (base, s) = serve_with_seed().await;
        let unauth = reqwest::Client::new()
            .post(format!("{base}/sync/commands/void-invoice"))
            .json(&void_invoice_body(s.head, &s.invoice_id))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    /// The regression this guards is a wiring one, and it is invisible to either
    /// half alone: the client builds a path and a body, the server owns the route
    /// and the DTO, and nothing in the type system connects them. A typo'd path is
    /// a 404 the desktop reports as "the server had a problem", and a drifted field
    /// name is a serde default the server silently accepts. Driving the real
    /// [`SyncClient`] against the real router is the only place that shows up.
    ///
    /// [`SyncClient`]: crate::sync::client::SyncClient
    #[tokio::test]
    async fn the_client_reaches_every_bill_and_invoice_command_it_offers() {
        use crate::sync::client::{SyncClient, SyncClientError};

        let (base, s) = serve_with_seed().await;
        let mut client = SyncClient::with_head(base, TOKEN, s.head);

        // Each call advances the head, and the client adopts it — so the *next*
        // call succeeding is itself evidence that the previous one's reply was
        // understood rather than guessed at.
        let head = client
            .apply_bill_payment(
                s.bill_id.clone(),
                NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                6_000,
                s.cash.clone(),
                s.ap.clone(),
                None,
            )
            .await
            .expect("apply-bill-payment");
        assert_eq!(
            head,
            s.head + 2,
            "a payment appends its entry and the event"
        );

        let head = client
            .receive_invoice_payment(
                s.invoice_id.clone(),
                NaiveDate::from_ymd_opt(2026, 7, 11).unwrap(),
                4_000,
                s.cash.clone(),
                s.ar.clone(),
                None,
            )
            .await
            .expect("receive-invoice-payment");
        assert_eq!(head, s.head + 4);

        // A part-paid bill may not be voided — and the refusal has to arrive as the
        // server's own words, not as a transport failure, or the desktop cannot
        // tell the user what to fix.
        match client.void_bill(s.bill_id.clone(), "changed my mind").await {
            Err(SyncClientError::Rejected(why)) => assert!(!why.is_empty(), "a 422 says why"),
            other => panic!("voiding a paid bill must be a terminal 422, got {other:?}"),
        }

        // …and one with no payments against it still voids, so the refusal above
        // was the domain guard and not the wiring.
        let (base2, s2) = serve_with_seed().await;
        let mut client2 = SyncClient::with_head(base2, TOKEN, s2.head);
        let head = client2
            .void_bill(s2.bill_id.clone(), "duplicate")
            .await
            .expect("void-bill");
        assert_eq!(head, s2.head + 2);
        let head = client2
            .void_invoice(s2.invoice_id.clone(), "duplicate")
            .await
            .expect("void-invoice");
        assert_eq!(head, s2.head + 4);
    }
}
