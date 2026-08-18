//! Reconciliation workflow endpoints over the sync transport (start, clear /
//! unclear a transaction, complete, abandon).
//!
//! Follows the same contract as `sync::submit_post_entry` (see `sync/mod.rs`):
//! each endpoint is bearer-authenticated (`AuthedUser`), honors the client's
//! `expected_head_seq`, and calls `append_checked` ONCE — no internal retry loop,
//! so a `HeadMismatch` surfaces as a `409` for the client to refetch and retry.
//! The command's real domain invariants run *inside* the append transaction via
//! the shared `build_*_in_txn` helpers in `commands::reconciliation_commands` (the
//! same code the local handlers use), the server stamps identity via `stamp`, and
//! the outcome maps to 200 + new head / 409 stale head / 422 domain rejection.
//!
//! Every reconciliation command emits a single event, so all five use the
//! single-event `SubmitResponse` / `outcome_to_response` shape. There is no pure
//! (state-independent) pre-check to run: all invariants are state-dependent and
//! live in-txn.

use crate::commands::reconciliation_commands::{
    build_abandon_reconciliation_in_txn, build_clear_transaction_in_txn,
    build_complete_reconciliation_in_txn, build_start_reconciliation_in_txn,
    build_unclear_transaction_in_txn, AbandonReconciliationCommand, ClearTransactionCommand,
    CompleteReconciliationCommand, ReconciliationCommandError, ReconciliationStep,
    StartReconciliationCommand, UnclearTransactionCommand,
};
use crate::store::event_store::Verdict;
use crate::sync::{
    outcome_to_response, project, stamp, ApiError, AuthedUser, SubmitResponse, SyncState,
};
use axum::{extract::State, routing::post, Json, Router};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route(
            "/sync/commands/start-reconciliation",
            post(submit_start_reconciliation),
        )
        .route(
            "/sync/commands/clear-transaction",
            post(submit_clear_transaction),
        )
        .route(
            "/sync/commands/unclear-transaction",
            post(submit_unclear_transaction),
        )
        .route(
            "/sync/commands/complete-reconciliation",
            post(submit_complete_reconciliation),
        )
        .route(
            "/sync/commands/abandon-reconciliation",
            post(submit_abandon_reconciliation),
        )
}

// --- start-reconciliation ---

/// Start a reconciliation over the wire. Serde-reusable DTO. `expected_head_seq`
/// carries the log head the client last observed.
#[derive(Serialize, Deserialize)]
pub struct StartReconciliationRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    pub statement_date: NaiveDate,
    pub statement_ending_balance: i64,
}

/// Start a reconciliation, validated server-side. The server runs the SAME
/// invariants the local `start_reconciliation` handler uses — the account exists
/// AND has no other in-progress reconciliation via
/// [`build_start_reconciliation_in_txn`], under the write lock so the
/// ≤1-in-progress-per-account fence can't be raced — honoring the client's
/// `expected_head_seq`. A second in-progress reconciliation is a `422`, a stale
/// head a `409`.
async fn submit_start_reconciliation(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<StartReconciliationRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = StartReconciliationCommand {
        account_id: req.account_id,
        statement_date: req.statement_date,
        statement_ending_balance: req.statement_ending_balance,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_start_reconciliation_in_txn(tx, &cmd)? {
                ReconciliationStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ReconciliationCommandError>)
}

// --- clear-transaction ---

/// Clear a transaction in a reconciliation over the wire. Serde-reusable DTO.
#[derive(Serialize, Deserialize)]
pub struct ClearTransactionRequest {
    pub expected_head_seq: i64,
    pub reconciliation_id: String,
    pub entry_id: String,
    pub line_id: String,
}

/// Clear a transaction, validated server-side. The server re-checks the SAME
/// fences the local `clear_transaction` handler uses — the reconciliation is in
/// progress, the line exists, and it is not already cleared via
/// [`build_clear_transaction_in_txn`] — honoring the client's `expected_head_seq`.
/// A violated fence is a `422`, a stale head a `409`.
async fn submit_clear_transaction(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ClearTransactionRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = ClearTransactionCommand {
        reconciliation_id: req.reconciliation_id,
        entry_id: req.entry_id,
        line_id: req.line_id,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_clear_transaction_in_txn(tx, &cmd)? {
                ReconciliationStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ReconciliationCommandError>)
}

// --- unclear-transaction ---

/// Unclear a transaction in a reconciliation over the wire. Serde-reusable DTO.
#[derive(Serialize, Deserialize)]
pub struct UnclearTransactionRequest {
    pub expected_head_seq: i64,
    pub reconciliation_id: String,
    pub entry_id: String,
    pub line_id: String,
}

/// Unclear a transaction, validated server-side. The server re-checks the SAME
/// fences the local `unclear_transaction` handler uses — the reconciliation is in
/// progress and the line is actually cleared via
/// [`build_unclear_transaction_in_txn`] — honoring the client's
/// `expected_head_seq`. A violated fence is a `422`, a stale head a `409`.
async fn submit_unclear_transaction(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<UnclearTransactionRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = UnclearTransactionCommand {
        reconciliation_id: req.reconciliation_id,
        entry_id: req.entry_id,
        line_id: req.line_id,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_unclear_transaction_in_txn(tx, &cmd)? {
                ReconciliationStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ReconciliationCommandError>)
}

// --- complete-reconciliation ---

/// Complete a reconciliation over the wire. Serde-reusable DTO.
#[derive(Serialize, Deserialize)]
pub struct CompleteReconciliationRequest {
    pub expected_head_seq: i64,
    pub reconciliation_id: String,
}

/// Complete a reconciliation, validated server-side. The server runs the SAME
/// logic the local `complete_reconciliation` handler uses — the reconciliation is
/// in progress and the `difference` snapshot is computed from the cleared set and
/// beginning balance *inside* the append transaction via
/// [`build_complete_reconciliation_in_txn`], so a concurrent clear/unclear can't
/// make the stored difference wrong — honoring the client's `expected_head_seq`.
/// An already-completed/abandoned reconciliation is a `422`, a stale head a `409`.
async fn submit_complete_reconciliation(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<CompleteReconciliationRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = CompleteReconciliationCommand {
        reconciliation_id: req.reconciliation_id,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_complete_reconciliation_in_txn(tx, &cmd)? {
                ReconciliationStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ReconciliationCommandError>)
}

// --- abandon-reconciliation ---

/// Abandon a reconciliation over the wire. Serde-reusable DTO.
#[derive(Serialize, Deserialize)]
pub struct AbandonReconciliationRequest {
    pub expected_head_seq: i64,
    pub reconciliation_id: String,
}

/// Abandon a reconciliation, validated server-side. The server re-checks the SAME
/// in-progress fence the local `abandon_reconciliation` handler uses via
/// [`build_abandon_reconciliation_in_txn`], freeing the account's in-progress slot
/// — honoring the client's `expected_head_seq`. A not-in-progress reconciliation
/// is a `422`, a stale head a `409`.
async fn submit_abandon_reconciliation(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<AbandonReconciliationRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = AbandonReconciliationCommand {
        reconciliation_id: req.reconciliation_id,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_abandon_reconciliation_in_txn(tx, &cmd)? {
                ReconciliationStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<ReconciliationCommandError>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::AccountType;
    use crate::events::types::{Event, JournalEntrySource};
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

    fn head_of(v: &serde_json::Value) -> i64 {
        v["head"].as_i64().unwrap()
    }

    /// Seed (via the local command handlers) a checking account, an expense
    /// account, and one balanced journal entry between them. Returns the checking
    /// account id, the entry id, and the checking-side line id (the line a
    /// reconciliation clears). Head starts at 3 (2 accounts + 1 entry).
    fn seed(store: &mut EventStore) -> (String, String, String) {
        let checking = {
            let stored = AccountCommands::new(store, "seed".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Asset,
                    account_number: "1010".to_string(),
                    name: "Checking".to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
            match stored.event {
                Event::AccountCreated { account_id, .. } => account_id,
                _ => unreachable!(),
            }
        };
        let expense = {
            let stored = AccountCommands::new(store, "seed".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Expense,
                    account_number: "5000".to_string(),
                    name: "Expense".to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
            match stored.event {
                Event::AccountCreated { account_id, .. } => account_id,
                _ => unreachable!(),
            }
        };

        let entry = EntryCommands::new(store, "seed".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
                memo: "Test expense".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 10000, "USD"),
                    EntryLine::credit(&checking, 10000, "USD"),
                ],
                reference: Some("CHK-001".to_string()),
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();
        let entry_id = match entry.event {
            Event::JournalEntryPosted { entry_id, .. } => entry_id,
            _ => unreachable!(),
        };
        // The checking line is the second line (credit) of the entry.
        let line_id = format!("{entry_id}-line-2");
        (checking, entry_id, line_id)
    }

    /// Serve a freshly seeded store, returning the base URL, a probe handle to the
    /// same store (to read the server-generated reconciliation id), and the seeded
    /// ids + starting head.
    pub(super) async fn serve_seeded() -> (String, SyncState, String, String, String, i64) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (checking, entry_id, line_id) = seed(&mut store);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let state = SyncState::new(store, tokens());
        let probe = state.clone();
        let base = serve(state).await;
        (base, probe, checking, entry_id, line_id, head)
    }

    /// The single in-progress reconciliation id for an account, read from the
    /// projected `reconciliations` table via the probe handle.
    pub(super) fn recon_id_of(st: &SyncState, account_id: &str) -> String {
        st.store
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT id FROM reconciliations WHERE account_id = ?1 AND status = 'in_progress'",
                [account_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn start_clear_complete_happy_path() {
        let (base, probe, checking, entry_id, line_id, head) = serve_seeded().await;
        let http = reqwest::Client::new();

        // Missing token → 401, nothing appended.
        let unauth = http
            .post(format!("{base}/sync/commands/start-reconciliation"))
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "account_id": checking,
                "statement_date": "2024-01-31",
                "statement_ending_balance": -10000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Start → 200, head advances by 1.
        let start = http
            .post(format!("{base}/sync/commands/start-reconciliation"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "account_id": checking,
                "statement_date": "2024-01-31",
                "statement_ending_balance": -10000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(start.status(), reqwest::StatusCode::OK);
        let head = head_of(&start.json().await.unwrap());

        let recon_id = recon_id_of(&probe, &checking);

        // Clear the checking-side transaction → 200, head advances.
        let clear = http
            .post(format!("{base}/sync/commands/clear-transaction"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "reconciliation_id": recon_id,
                "entry_id": entry_id,
                "line_id": line_id,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(clear.status(), reqwest::StatusCode::OK);
        let head = head_of(&clear.json().await.unwrap());

        // Complete → 200, head advances, projected status is completed.
        let complete = http
            .post(format!("{base}/sync/commands/complete-reconciliation"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "reconciliation_id": recon_id,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(complete.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&complete.json().await.unwrap()), head + 1);

        let status: String = probe
            .store
            .lock()
            .unwrap()
            .connection()
            .query_row(
                "SELECT status FROM reconciliations WHERE id = ?1",
                [&recon_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[tokio::test]
    async fn second_start_on_same_account_rejected_422() {
        let (base, _probe, checking, _entry_id, _line_id, head) = serve_seeded().await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/start-reconciliation");

        let body = |expected_head: i64| {
            serde_json::json!({
                "expected_head_seq": expected_head,
                "account_id": checking,
                "statement_date": "2024-01-31",
                "statement_ending_balance": 100000,
            })
        };

        // First start lands.
        let first = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body(head))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), reqwest::StatusCode::OK);
        let head = head_of(&first.json().await.unwrap());

        // A second start on the same account (in-txn ≤1-in-progress fence) → 422.
        let second = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body(head))
            .send()
            .await
            .unwrap();
        assert_eq!(second.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn stale_head_conflicts_409() {
        let (base, _probe, checking, _entry_id, _line_id, head) = serve_seeded().await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/start-reconciliation");

        // First start lands, moving the log past `head`.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "account_id": checking,
                "statement_date": "2024-01-31",
                "statement_ending_balance": 100000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        let new_head = head_of(&ok.json().await.unwrap());

        // A submit against the now-stale `head` → 409 with the current head.
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "account_id": checking,
                "statement_date": "2024-02-28",
                "statement_ending_balance": 120000,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        let cur = stale.json::<serde_json::Value>().await.unwrap()["current_head"]
            .as_i64()
            .unwrap();
        assert_eq!(cur, new_head);
    }

    #[tokio::test]
    async fn complete_requires_token_401() {
        let (base, _probe, _checking, _entry_id, _line_id, head) = serve_seeded().await;
        // Missing bearer token → 401, before any command runs.
        let unauth = reqwest::Client::new()
            .post(format!("{base}/sync/commands/complete-reconciliation"))
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "reconciliation_id": "does-not-matter",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
}

/// The client half, and the number it shows while you work.
///
/// A group-hosted book could not be reconciled at all until these existed: a
/// replica may not append, so the endpoints above had no caller. Which is the
/// wrong way round — a shared book is the one most likely to need reconciling.
#[cfg(test)]
mod client_round_trip {
    use super::tests::{recon_id_of, serve_seeded};
    use super::*;
    use crate::queries::account_queries::AccountQueries;
    use crate::sync::client::SyncClient;

    const TOKEN: &str = "tok-1";

    /// Start, clear, complete — from the client, against the real router.
    #[tokio::test]
    async fn a_replica_can_run_a_reconciliation_end_to_end() {
        let (base, probe, checking, entry_id, line_id, head) = serve_seeded().await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        // The seeded entry credits checking 10000, so a statement saying -10000
        // is a reconciliation that balances exactly once that line is cleared.
        client
            .start_reconciliation(
                &checking,
                NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                -10000,
            )
            .await
            .expect("start");

        // The id was minted server-side and is not in the response; it is read
        // back from the projection, which is what the desktop does after its pull.
        let recon_id = recon_id_of(&probe, &checking);
        {
            let store = probe.store.lock().unwrap();
            let found = AccountQueries::new(store.connection())
                .in_progress_reconciliation(&checking)
                .expect("lookup")
                .expect("a reconciliation is open");
            assert_eq!(
                found.id, recon_id,
                "the shared lookup found a different one"
            );
            assert_eq!(found.statement_ending_balance, -10000);
        }

        client
            .clear_transaction(&recon_id, &entry_id, &line_id)
            .await
            .expect("clear");
        client
            .complete_reconciliation(&recon_id)
            .await
            .expect("complete");

        let store = probe.store.lock().unwrap();
        let status: String = store
            .connection()
            .query_row(
                "SELECT status FROM reconciliations WHERE id = ?1",
                [&recon_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    /// The advisory number on screen and the one the server records must agree.
    ///
    /// They are computed in two places — `reconciliation_progress` for the view,
    /// `build_complete_reconciliation_in_txn` for the event — and a formula that
    /// drifts means a reconciliation that reads as balanced and completes with a
    /// residual, which is exactly the thing a reconciliation exists to rule out.
    #[tokio::test]
    async fn the_difference_on_screen_is_the_difference_recorded() {
        let (base, probe, checking, entry_id, line_id, head) = serve_seeded().await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        // A statement 2500 away from what clearing that one line accounts for, so
        // a difference of zero cannot pass by accident.
        client
            .start_reconciliation(
                &checking,
                NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                -7500,
            )
            .await
            .expect("start");
        let recon_id = recon_id_of(&probe, &checking);
        client
            .clear_transaction(&recon_id, &entry_id, &line_id)
            .await
            .expect("clear");

        let advisory = {
            let store = probe.store.lock().unwrap();
            AccountQueries::new(store.connection())
                .reconciliation_progress(&recon_id)
                .expect("progress")
        };
        assert_eq!(advisory.cleared_total, -10000);
        assert_eq!(advisory.beginning_balance, 0, "nothing was cleared before");
        assert_eq!(advisory.difference, 2500, "{advisory:?}");

        client
            .complete_reconciliation(&recon_id)
            .await
            .expect("complete");

        // What the ledger actually recorded.
        let store = probe.store.lock().unwrap();
        let recorded: i64 = store
            .connection()
            .query_row(
                "SELECT json_extract(payload, '$.difference') FROM events
                  WHERE event_type = 'reconciliation_completed'
                  ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            recorded, advisory.difference,
            "the screen said {} and the ledger recorded {recorded}",
            advisory.difference
        );
    }
}
