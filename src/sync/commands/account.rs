//! Account command endpoints over the sync transport.
//!
//! Follows the same contract as `sync::submit_post_entry` (see `sync/mod.rs`):
//! each endpoint is bearer-authenticated (`AuthedUser`), honors the client's
//! `expected_head_seq`, and calls `append_checked` ONCE — no internal retry
//! loop, so a `HeadMismatch` surfaces as a `409` for the client to refetch and
//! retry. The command's real domain invariants run *inside* the append
//! transaction via the shared `build_*_in_txn` helpers in
//! `commands::account_commands` (the same code the local handlers use), the
//! server stamps identity via `stamp`, and the outcome maps to
//! 200 + new head / 409 stale head / 422 domain rejection.
//!
//! Wired here: `create-account` and `deactivate-account`. `update-account` is
//! NOT wired: it emits one `AccountUpdated` event per changed field via
//! `append_checked_many` (a batch), which does not fit the single-event
//! `SubmitResponse`/`outcome_to_response` shape this template is built around —
//! it needs the multi-event submit path, deferred to that work.

use crate::commands::account_commands::{
    build_create_account_in_txn, build_deactivate_account_in_txn, AccountCommandError, AccountStep,
    CreateAccountCommand, DeactivateAccountCommand,
};
use crate::domain::AccountType;
use crate::store::event_store::Verdict;
use crate::sync::{
    outcome_to_response, project, stamp, ApiError, AuthedUser, SubmitResponse, SyncState,
};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/commands/create-account", post(submit_create_account))
        .route(
            "/sync/commands/deactivate-account",
            post(submit_deactivate_account),
        )
}

/// Create an account over the wire. Serde-reusable DTO (the client half can
/// share it). `expected_head_seq` carries the log head the client last observed.
#[derive(Serialize, Deserialize)]
pub struct CreateAccountRequest {
    pub expected_head_seq: i64,
    pub account_type: AccountType,
    pub account_number: String,
    pub name: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Create an account, validated server-side. The server runs the SAME invariant
/// the local `create_account` handler uses — the account-number uniqueness check
/// via [`build_create_account_in_txn`], under the write lock — honoring the
/// client's `expected_head_seq`. A duplicate number is a `422`, a stale head a
/// `409`.
async fn submit_create_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<CreateAccountRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = CreateAccountCommand {
        account_type: req.account_type,
        account_number: req.account_number,
        name: req.name,
        parent_id: req.parent_id,
        currency: req.currency,
        description: req.description,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_create_account_in_txn(tx, &cmd)? {
                AccountStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                AccountStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<AccountCommandError>)
}

/// Deactivate an account over the wire. Serde-reusable DTO.
#[derive(Serialize, Deserialize)]
pub struct DeactivateAccountRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Deactivate an account, validated server-side. The server re-checks the SAME
/// fences the local `deactivate_account` handler uses — the account is active AND
/// has a zero net balance via [`build_deactivate_account_in_txn`], under the
/// write lock so a concurrent posting can't sneak a nonzero balance in — honoring
/// the client's `expected_head_seq`. A violated fence is a `422`, a stale head a
/// `409`.
async fn submit_deactivate_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<DeactivateAccountRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = DeactivateAccountCommand {
        account_id: req.account_id,
        reason: req.reason,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_deactivate_account_in_txn(tx, &cmd)? {
                AccountStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                AccountStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<AccountCommandError>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::AccountCommands;
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
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

    fn create_body(expected_head: i64, number: &str) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": expected_head,
            "account_type": "asset",
            "account_number": number,
            "name": format!("Acct {number}"),
        })
    }

    #[tokio::test]
    async fn create_account_command_validated_server_side() {
        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/create-account");

        // Missing token → 401, nothing appended.
        let unauth = http
            .post(&url)
            .json(&create_body(0, "1000"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Happy path: fresh number at head 0 → 200, head advances to 1.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&create_body(0, "1000"))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), 1);

        // Duplicate account number (in-txn uniqueness) → 422, nothing appended.
        let dup = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&create_body(1, "1000"))
            .send()
            .await
            .unwrap();
        assert_eq!(dup.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

        // Stale head (log is at 1) → 409 with current head 1.
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&create_body(0, "2000"))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
        let cur = stale.json::<serde_json::Value>().await.unwrap()["current_head"]
            .as_i64()
            .unwrap();
        assert_eq!(cur, 1);

        // Refetch + retry against the fresh head → success, head 2.
        let retry = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&create_body(cur, "2000"))
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&retry.json().await.unwrap()), 2);
    }

    #[tokio::test]
    async fn deactivate_account_command_validated_server_side() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        // Zero-balance account can be deactivated; one with a balance cannot.
        let cash = mk_account(&mut store, "1000", AccountType::Asset);
        let equity = mk_account(&mut store, "3000", AccountType::Equity);
        let head = store.latest_id().unwrap().unwrap_or(0); // 2 accounts seeded
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/deactivate-account");

        // Missing token → 401.
        let unauth = http
            .post(&url)
            .json(&serde_json::json!({ "expected_head_seq": head, "account_id": equity }))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Happy path: zero-balance account → 200, head advances.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": head, "account_id": equity }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), head + 1);

        // Stale head now (log advanced) → 409.
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": head, "account_id": cash }))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn deactivate_rejects_account_with_balance_server_side() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let cash = mk_account(&mut store, "1000", AccountType::Asset);
        let equity = mk_account(&mut store, "3000", AccountType::Equity);
        // Give `cash` a nonzero balance so the in-txn fence rejects deactivation.
        EntryCommands::new(&mut store, "seed".to_string())
            .post_entry(PostEntryCommand {
                date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                memo: "opening".to_string(),
                lines: vec![
                    EntryLine::debit(&cash, 5000, "USD"),
                    EntryLine::credit(&equity, 5000, "USD"),
                ],
                reference: None,
                source: None,
            })
            .unwrap();
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/deactivate-account"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": head, "account_id": cash }))
            .send()
            .await
            .unwrap();
        // The server enforces the fence: nonzero balance → 422, not a blind append.
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r.json::<serde_json::Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("balance"));
    }
    /// Deactivation is the one account command the desktop has no local fallback
    /// for on a replica, so this route is the whole feature. Same wiring
    /// regression as the other command families.
    #[tokio::test]
    async fn the_client_reaches_deactivate_account() {
        use crate::sync::client::SyncClient;

        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let account_id = mk_account(&mut store, "1000", AccountType::Asset);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        assert_eq!(
            client
                .deactivate_account(account_id, Some("closed".to_string()))
                .await
                .unwrap(),
            head + 1
        );
    }
}
