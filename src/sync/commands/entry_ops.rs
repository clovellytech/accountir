//! Journal-entry operation endpoints over the sync transport: void-entry and
//! unvoid-entry.
//!
//! Follows the same contract as `sync::commands::account` (see `sync/mod.rs`):
//! each endpoint is bearer-authenticated (`AuthedUser`), honors the client's
//! `expected_head_seq`, and calls `append_checked` ONCE — no internal retry
//! loop, so a `HeadMismatch` surfaces as a `409` for the client to refetch and
//! retry. The command's real domain invariants run *inside* the append
//! transaction via the shared `build_*_in_txn` helpers in
//! `commands::entry_commands` (the same code the local handlers use), the server
//! stamps identity via `stamp`, and the outcome maps to 200 + new head / 409
//! stale head / 422 domain rejection.
//!
//! Wired here: `void-entry` and `unvoid-entry`. Both emit a single event, so
//! they fit the single-event `SubmitResponse`/`outcome_to_response` shape.

use crate::commands::entry_commands::{
    build_unvoid_entry_in_txn, build_void_entry_in_txn, EntryCommandError, PostEntryStep,
    UnvoidEntryCommand, VoidEntryCommand,
};
use crate::store::event_store::Verdict;
use crate::sync::{
    outcome_to_response, project, stamp, ApiError, AuthedUser, SubmitResponse, SyncState,
};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/void-entry", post(submit_void_entry))
        .route("/sync/commands/unvoid-entry", post(submit_unvoid_entry))
}

/// Void a journal entry over the wire. Serde-reusable DTO (the client half can
/// share it). `expected_head_seq` carries the log head the client last observed.
#[derive(Serialize, Deserialize)]
pub struct VoidEntryRequest {
    pub expected_head_seq: i64,
    pub entry_id: String,
    pub reason: String,
}

/// Void a journal entry, validated server-side. The server runs the SAME
/// invariant the local `void_entry` handler uses — the entry exists and is not
/// already voided via [`build_void_entry_in_txn`], under the write lock —
/// honoring the client's `expected_head_seq`. An already-voided (or missing)
/// entry is a `422`, a stale head a `409`.
async fn submit_void_entry(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<VoidEntryRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = VoidEntryCommand {
        entry_id: req.entry_id,
        reason: req.reason,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_void_entry_in_txn(tx, &cmd)? {
                PostEntryStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PostEntryStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<EntryCommandError>)
}

/// Unvoid a journal entry over the wire. Serde-reusable DTO.
#[derive(Serialize, Deserialize)]
pub struct UnvoidEntryRequest {
    pub expected_head_seq: i64,
    pub entry_id: String,
    pub reason: String,
}

/// Unvoid a journal entry, validated server-side. The server re-checks the SAME
/// invariants the local `unvoid_entry` handler uses — the entry exists AND is
/// currently voided, PLUS the reference-reclamation guard — via
/// [`build_unvoid_entry_in_txn`], under the write lock so a concurrent claim of
/// the freed reference can't slip in — honoring the client's `expected_head_seq`.
/// A non-voided entry or a reclaimed reference is a `422`, a stale head a `409`.
async fn submit_unvoid_entry(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<UnvoidEntryRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = UnvoidEntryCommand {
        entry_id: req.entry_id,
        reason: req.reason,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_unvoid_entry_in_txn(tx, &cmd)? {
                PostEntryStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PostEntryStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<EntryCommandError>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{
        EntryCommands, EntryLine, PostEntryCommand, VoidEntryCommand,
    };
    use crate::domain::AccountType;
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

    fn head_of(v: &serde_json::Value) -> i64 {
        v["head"].as_i64().unwrap()
    }

    /// Seed an account directly (local handler), returning its id.
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

    /// Seed two accounts + a balanced posted entry (local handlers). Returns the
    /// posted entry's id.
    fn seed_posted_entry(store: &mut EventStore) -> String {
        let cash = mk_account(store, "1000", AccountType::Asset);
        let expense = mk_account(store, "5000", AccountType::Expense);
        let stored = EntryCommands::new(store, "seed".to_string())
            .post_entry(PostEntryCommand {
                date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                memo: "seed".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 10000, "USD"),
                    EntryLine::credit(&cash, 10000, "USD"),
                ],
                reference: None,
                source: None,
            })
            .unwrap();
        match stored.event {
            Event::JournalEntryPosted { entry_id, .. } => entry_id,
            _ => unreachable!(),
        }
    }

    #[tokio::test]
    async fn void_entry_command_validated_server_side() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let entry_id = seed_posted_entry(&mut store);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/void-entry");

        let body = |expected_head: i64| {
            serde_json::json!({
                "expected_head_seq": expected_head,
                "entry_id": entry_id,
                "reason": "mistake",
            })
        };

        // Missing token → 401.
        let unauth = http.post(&url).json(&body(head)).send().await.unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Stale head (log is at `head`, client claims 0) → 409 with current head.
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body(0))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        let cur = stale.json::<serde_json::Value>().await.unwrap()["current_head"]
            .as_i64()
            .unwrap();
        assert_eq!(cur, head);

        // Happy path: void at the fresh head → 200, head advances.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body(head))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), head + 1);

        // Voiding the already-voided entry (in-txn invariant) → 422.
        let dup = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&body(head + 1))
            .send()
            .await
            .unwrap();
        assert_eq!(dup.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn unvoid_entry_command_validated_server_side() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let entry_id = seed_posted_entry(&mut store);
        // Void it locally so there is something to unvoid over the wire.
        EntryCommands::new(&mut store, "seed".to_string())
            .void_entry(VoidEntryCommand {
                entry_id: entry_id.clone(),
                reason: "oops".to_string(),
            })
            .unwrap();
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/unvoid-entry");

        // Happy path: unvoid at the fresh head → 200, head advances.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "entry_id": entry_id,
                "reason": "restore",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), head + 1);
    }
    /// The wiring the desktop's "void" button actually travels: client path +
    /// DTO against the real router. A drift in either is a button that reports a
    /// server problem for a command the server implements perfectly well.
    #[tokio::test]
    async fn the_client_reaches_void_and_unvoid_entry() {
        use crate::sync::client::SyncClient;

        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let entry_id = seed_posted_entry(&mut store);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        assert_eq!(
            client
                .void_entry(entry_id.clone(), "mistake")
                .await
                .unwrap(),
            head + 1
        );
        // The client adopted the new head, so this second call only succeeds if it
        // read the first reply rather than guessing.
        assert_eq!(
            client.unvoid_entry(entry_id, "restored").await.unwrap(),
            head + 2
        );
    }
}
