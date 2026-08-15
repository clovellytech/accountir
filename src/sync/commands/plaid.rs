//! Recording a bank connection over the sync transport.
//!
//! Lets a member on group-hosted books link a bank without appending locally —
//! which a replica cannot do, since its event ids are the group server's.
//!
//! # What does and does not cross this boundary
//!
//! Nothing secret. The bank credential is exchanged directly between the member's
//! machine and the bank-sync proxy, under *their* proxy API key, and the resulting
//! access token is encrypted at rest there. This endpoint carries only what the
//! group needs in order to show the connection and map its accounts: the
//! institution's name, and each account's name, type and mask.
//!
//! `proxy_item_id` is deliberately **not** accepted. It is the proxy's handle for
//! the connection, inert without the owner's API key, and read only by the machine
//! that talks to the proxy — which on hosted books is the group's instance, using a
//! grant, and it already holds the handle in its own store. Putting it in a log
//! every member replicates would share something no member can use.
//!
//! # Why the server mints the item id
//!
//! The projector writes `INSERT OR REPLACE INTO plaid_items (id, ...)`. A
//! client-supplied id is therefore a way to *overwrite another member's
//! connection* — silently, and with a perfectly valid-looking event. So the id is
//! minted here and returned, exactly as `build_create_account_in_txn` mints an
//! account id rather than taking one.

use crate::commands::plaid_commands::{
    build_map_account_in_txn, build_refresh_accounts_in_txn, build_unmap_account_in_txn,
    PlaidCommandError, PlaidStep,
};
use crate::events::types::{Event, PlaidAccountInfo};
use crate::store::event_store::{CheckedOutcome, Verdict};
use crate::sync::{outcome_to_response, project, stamp, ApiError, AuthedUser, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new()
        .route(
            "/sync/commands/connect-plaid-item",
            post(submit_connect_plaid_item),
        )
        .route(
            "/sync/commands/refresh-plaid-accounts",
            post(submit_refresh_accounts),
        )
        .route("/sync/commands/map-plaid-account", post(submit_map_account))
        .route(
            "/sync/commands/unmap-plaid-account",
            post(submit_unmap_account),
        )
}

#[derive(Serialize, Deserialize)]
pub struct ConnectPlaidItemRequest {
    pub expected_head_seq: i64,
    pub institution_name: String,
    pub plaid_accounts: Vec<PlaidAccountInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct ConnectPlaidItemResponse {
    pub head: i64,
    /// Minted server-side. The caller needs it to attach a grant to this
    /// connection, and cannot choose it — see the module docs.
    pub item_id: String,
}

async fn submit_connect_plaid_item(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ConnectPlaidItemRequest>,
) -> Result<Json<ConnectPlaidItemResponse>, ApiError> {
    if req.institution_name.trim().is_empty() {
        return Err(ApiError::bad_request("institution_name is required"));
    }
    if req.plaid_accounts.is_empty() {
        // A connection with no accounts can never be mapped to anything, so it
        // would sit in the group's books as a permanent dead entry.
        return Err(ApiError::bad_request(
            "a connection must carry at least one account",
        ));
    }

    let item_id = uuid::Uuid::new_v4().to_string();
    let event = Event::PlaidItemConnected {
        item_id: item_id.clone(),
        // Omitted on purpose. See the module docs.
        proxy_item_id: None,
        institution_name: req.institution_name,
        plaid_accounts: req.plaid_accounts,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |_tx| {
                Ok(Verdict::<_, std::convert::Infallible>::Append(stamp(
                    event, &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;

    match outcome {
        CheckedOutcome::Appended(stored) => Ok(Json(ConnectPlaidItemResponse {
            head: stored.id,
            item_id,
        })),
        CheckedOutcome::HeadMismatch { actual, .. } => Err(ApiError::conflict(actual)),
        // `Infallible` — there is no state-dependent fence here, and saying so in
        // the type is better than an arm that can never run.
        CheckedOutcome::Rejected(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::router;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    async fn serve() -> String {
        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            crate::store::migrations::run_migrations(s.connection()).unwrap();
            s
        };
        let state = SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "alice@example.com".to_string())]),
        );
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn body(head: i64) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": head,
            "institution_name": "Chase",
            "plaid_accounts": [{
                "plaid_account_id": "acc-business",
                "name": "Business Checking",
                "official_name": null,
                "account_type": "depository",
                "mask": "1187",
            }],
        })
    }

    /// The whole point: a member on hosted books records a connection without a
    /// local append, and gets back an id they can attach a grant to.
    #[tokio::test]
    async fn a_connection_is_recorded_and_its_id_returned() {
        let base = serve().await;
        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/connect-plaid-item"))
            .bearer_auth(TOKEN)
            .json(&body(0))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["head"], 1);
        assert!(
            uuid::Uuid::parse_str(v["item_id"].as_str().unwrap()).is_ok(),
            "the server must mint a real id: {v}"
        );
    }

    /// The proxy handle must never reach the group's log. It is inert without the
    /// owner's API key and no member can use it, so sharing it is cost with no
    /// benefit — and the endpoint does not even accept one, so a client cannot
    /// smuggle it in by adding a field.
    #[tokio::test]
    async fn the_proxy_handle_never_reaches_the_shared_log() {
        let base = serve().await;
        let mut with_handle = body(0);
        with_handle["proxy_item_id"] = serde_json::json!("p-secret-handle");

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/connect-plaid-item"))
            .bearer_auth(TOKEN)
            .json(&with_handle)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK);

        let events: serde_json::Value = reqwest::Client::new()
            .get(format!("{base}/sync/events?since=0&limit=10"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let dump = events.to_string();
        assert!(
            !dump.contains("p-secret-handle"),
            "a client-supplied proxy handle reached the shared log: {dump}"
        );
        assert!(
            !dump.contains("proxy_item_id"),
            "the field must be absent entirely, not null: {dump}"
        );
    }

    /// A client-chosen id would be a way to overwrite another member's connection
    /// — the projector does INSERT OR REPLACE on `plaid_items(id)`. Two identical
    /// requests must therefore produce two distinct connections, not one that ate
    /// the other.
    #[tokio::test]
    async fn a_client_cannot_choose_an_id_and_clobber_someone_elses_connection() {
        let base = serve().await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/connect-plaid-item");

        let mut attacker = body(0);
        attacker["item_id"] = serde_json::json!("victim-item");
        let first: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&attacker)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let head = first["head"].as_i64().unwrap();

        let mut again = body(head);
        again["item_id"] = serde_json::json!("victim-item");
        let second: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&again)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_ne!(
            first["item_id"], second["item_id"],
            "the server reused a client-supplied id, so one connection would \
             REPLACE the other in the projection"
        );
        assert_ne!(first["item_id"], "victim-item");
    }

    /// Set up a connection and a ledger account, and return `(item_id, account_id)`.
    async fn connected(base: &str) -> (String, String) {
        let http = reqwest::Client::new();
        let v: serde_json::Value = http
            .post(format!("{base}/sync/commands/connect-plaid-item"))
            .bearer_auth(TOKEN)
            .json(&body(0))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let item = v["item_id"].as_str().unwrap().to_string();
        let head = v["head"].as_i64().unwrap();

        let acct: serde_json::Value = http
            .post(format!("{base}/sync/commands/create-account"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "account_type": "asset",
                "account_number": "1001",
                "name": "Business Checking",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let _ = acct;

        let accounts: serde_json::Value = http
            .get(format!("{base}/sync/accounts"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let account_id = accounts["accounts"][0]["id"]
            .as_str()
            .expect("the created account")
            .to_string();
        (item, account_id)
    }

    async fn head_of(base: &str) -> i64 {
        reqwest::Client::new()
            .get(format!("{base}/sync/head"))
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

    /// The thing that was blocked: a member on hosted books links a bank account
    /// to a ledger account, and can unlink it again.
    #[tokio::test]
    async fn a_bank_account_can_be_linked_and_unlinked_on_hosted_books() {
        let base = serve().await;
        let (item, account) = connected(&base).await;
        let http = reqwest::Client::new();

        let map = http
            .post(format!("{base}/sync/commands/map-plaid-account"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "item_id": item,
                "plaid_account_id": "acc-business",
                "local_account_id": account,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(map.status(), reqwest::StatusCode::OK);

        let unmap = http
            .post(format!("{base}/sync/commands/unmap-plaid-account"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "item_id": item,
                "plaid_account_id": "acc-business",
                "local_account_id": account,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(unmap.status(), reqwest::StatusCode::OK);
    }

    /// A member finds accounts the group's books never had, and records them.
    ///
    /// The situation this is for: a connection made when the account list came
    /// from Plaid Link's browser metadata, which for an OAuth bank can omit most
    /// of the accounts. The member's own machine asks the proxy — it holds the
    /// API key, the group never does — and hands the answer here.
    #[tokio::test]
    async fn a_member_can_record_accounts_the_group_did_not_know_about() {
        let base = serve().await;
        let (item, _account) = connected(&base).await;
        let http = reqwest::Client::new();

        let found = serde_json::json!([
            {
                "plaid_account_id": "acc-business",
                "name": "Business Checking",
                "official_name": null,
                "account_type": "depository",
                "mask": "1187",
            },
            {
                "plaid_account_id": "acc-card-2",
                "name": "Employee Card 2",
                "official_name": null,
                "account_type": "credit",
                "mask": "4402",
            },
        ]);

        let r = http
            .post(format!("{base}/sync/commands/refresh-plaid-accounts"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "item_id": item,
                "plaid_accounts": found.clone(),
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["recorded"], true, "{v}");

        // Pressed again with the same answer: success, and the log does not move.
        // Refresh is what somebody presses when they are unsure, so it gets
        // pressed repeatedly and must not leave a trail of identical events.
        let head = head_of(&base).await;
        let again = http
            .post(format!("{base}/sync/commands/refresh-plaid-accounts"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "item_id": item,
                "plaid_accounts": found,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::OK);
        let v: serde_json::Value = again.json().await.unwrap();
        assert_eq!(v["recorded"], false, "a second identical refresh wrote: {v}");
        assert_eq!(head_of(&base).await, head, "the shared log moved for nothing");
    }

    /// An empty list is refused rather than recorded.
    ///
    /// "The bank has nothing behind this login" is far more likely to be a fault
    /// upstream than the truth, and recording it would say the connection had
    /// been checked and found empty.
    #[tokio::test]
    async fn a_refresh_carrying_no_accounts_is_refused() {
        let base = serve().await;
        let (item, _account) = connected(&base).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/refresh-plaid-accounts"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "item_id": item,
                "plaid_accounts": [],
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    }

    /// A typo'd ledger account used to be accepted and then blow up inside the
    /// projector as a foreign-key failure — an internal error where the honest
    /// answer is "no such account".
    #[tokio::test]
    async fn mapping_to_an_account_that_does_not_exist_is_a_domain_rejection() {
        let base = serve().await;
        let (item, _account) = connected(&base).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/map-plaid-account"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "item_id": item,
                "plaid_account_id": "acc-business",
                "local_account_id": "no-such-account",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Unmapping something that is not mapped must not report success — a stale
    /// UI would otherwise show a removal that never happened.
    #[tokio::test]
    async fn unmapping_what_was_never_mapped_is_refused() {
        let base = serve().await;
        let (item, account) = connected(&base).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/unmap-plaid-account"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "item_id": item,
                "plaid_account_id": "acc-business",
                "local_account_id": account,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn an_empty_connection_is_refused_and_auth_is_required() {
        let base = serve().await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/connect-plaid-item");

        assert_eq!(
            http.post(&url)
                .json(&body(0))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let mut empty = body(0);
        empty["plaid_accounts"] = serde_json::json!([]);
        assert_eq!(
            http.post(&url)
                .bearer_auth(TOKEN)
                .json(&empty)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::BAD_REQUEST,
            "a connection with no accounts can never be mapped and would sit in \
             the books as a permanent dead entry"
        );
    }
}

/// Link or unlink one of a connection's bank accounts to a ledger account.
///
/// Both shapes are identical, so one DTO. Which endpoint it is sent to decides
/// the direction — a `mapped: bool` field would be a way to get the wrong one by
/// typo, on a request that otherwise looks correct.
#[derive(Serialize, Deserialize)]
pub struct MapPlaidAccountRequest {
    pub expected_head_seq: i64,
    pub item_id: String,
    pub plaid_account_id: String,
    pub local_account_id: String,
}

/// Map a bank account to a ledger account, validated server-side.
///
/// Runs the SAME in-txn fences the local handler does — the connection exists and
/// the ledger account is real — under the write lock, so a mapping cannot be
/// created against a connection that was disconnected a moment earlier.
async fn submit_map_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<MapPlaidAccountRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_map_account_in_txn(
                tx,
                &req.item_id,
                &req.plaid_account_id,
                &req.local_account_id,
            )? {
                PlaidStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PlaidStep::Reject(e) => Ok(Verdict::Reject(e)),
                // Unreachable for map/unmap: both decide, they never abstain.
                PlaidStep::Nothing => unreachable!("mapping always appends or refuses"),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<PlaidCommandError>)
}

#[derive(Serialize, Deserialize)]
pub struct RefreshPlaidAccountsRequest {
    pub expected_head_seq: i64,
    pub item_id: String,
    /// Every account the bank reports behind this connection.
    pub plaid_accounts: Vec<PlaidAccountInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct RefreshPlaidAccountsResponse {
    pub head: i64,
    /// `false` when the group's books already agreed with the bank, in which case
    /// `head` did not move. Success either way — a member pressing refresh on a
    /// connection nobody has touched should not see a failure.
    pub recorded: bool,
}

/// Record what a member's bank reports behind a shared connection.
///
/// The member's machine does the talking to the proxy — it holds the API key, and
/// the group never does — so the account list arrives here already fetched. What
/// the server keeps for itself is the decision about whether it is news: the
/// same in-txn comparison the local command makes, under the write lock, so two
/// members refreshing at once append one event rather than two.
async fn submit_refresh_accounts(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RefreshPlaidAccountsRequest>,
) -> Result<Json<RefreshPlaidAccountsResponse>, ApiError> {
    if req.plaid_accounts.is_empty() {
        return Err(ApiError::bad_request(
            "a refresh must carry the accounts the bank reported",
        ));
    }

    let nothing = std::cell::Cell::new(false);
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            |tx| match build_refresh_accounts_in_txn(tx, &req.item_id, &req.plaid_accounts)? {
                PlaidStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PlaidStep::Reject(e) => Ok(Verdict::Reject(e)),
                PlaidStep::Nothing => {
                    nothing.set(true);
                    Ok(Verdict::Reject(PlaidCommandError::ItemNotFound(
                        "nothing to record".to_string(),
                    )))
                }
            },
            project,
        )
        .map_err(ApiError::store)?;

    match outcome {
        CheckedOutcome::Appended(stored) => Ok(Json(RefreshPlaidAccountsResponse {
            head: stored.id,
            recorded: true,
        })),
        CheckedOutcome::HeadMismatch { actual, .. } => Err(ApiError::conflict(actual)),
        CheckedOutcome::Rejected(_) if nothing.get() => {
            // The head the caller sent is still the head — nothing was appended.
            Ok(Json(RefreshPlaidAccountsResponse {
                head: req.expected_head_seq,
                recorded: false,
            }))
        }
        CheckedOutcome::Rejected(e) => Err(ApiError::domain(e)),
    }
}

/// Unmap a bank account, validated server-side. A mapping that is not there is a
/// `422` rather than a silent success, so a stale UI cannot report a removal that
/// never happened.
async fn submit_unmap_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<MapPlaidAccountRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_unmap_account_in_txn(
                tx,
                &req.item_id,
                &req.plaid_account_id,
                &req.local_account_id,
            )? {
                PlaidStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PlaidStep::Reject(e) => Ok(Verdict::Reject(e)),
                // Unreachable for map/unmap: both decide, they never abstain.
                PlaidStep::Nothing => unreachable!("mapping always appends or refuses"),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<PlaidCommandError>)
}
