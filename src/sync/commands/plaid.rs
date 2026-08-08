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

use crate::events::types::{Event, PlaidAccountInfo};
use crate::store::event_store::{CheckedOutcome, Verdict};
use crate::sync::{project, stamp, ApiError, AuthedUser, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new().route(
        "/sync/commands/connect-plaid-item",
        post(submit_connect_plaid_item),
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
