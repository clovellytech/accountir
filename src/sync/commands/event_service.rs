//! Connecting an event service to group-hosted books.
//!
//! An event service is an app that publishes an accountir feed — a bike shop's
//! point of sale, say — which the ledger pulls sales, purchases, goods receipts
//! and stock adjustments from. On standalone books a member registers one and
//! syncs it locally. On group-hosted books they could do neither, because a
//! replica may not append: its event ids are the group server's to mint. These
//! endpoints are the route that was missing.
//!
//! # What does and does not cross this boundary
//!
//! **Not the API key.** This log is replicated in full to every member's laptop
//! and into every backup they take, so a key written here is a key on all of them,
//! permanently, recoverable only by rotating it at the service. It goes where the
//! bank-grant token goes: the group's instance, in a database beside this one, one
//! copy and one audit point — see `accountir-server/src/servicekeys.rs`.
//!
//! What the group's books do carry is the *fact* of the connection: its name and
//! its root URL. That is not a secret and the group needs it — every member sees
//! the entries this service produces, so every member should be able to see where
//! they came from.
//!
//! # Why the server mints the service id
//!
//! The projector writes `INSERT OR REPLACE INTO event_services (id, ...)`, so a
//! client-supplied id is a way to overwrite another member's service with a
//! perfectly valid-looking event. The id is minted here and returned, exactly as
//! `connect-plaid-item` mints an item id rather than accepting one.

use crate::events::types::Event;
use crate::store::event_store::{CheckedOutcome, Verdict};
use crate::sync::{outcome_to_response, project, stamp, ApiError, AuthedUser, SyncState};
use axum::{extract::State, routing::post, Json, Router};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub fn router() -> Router<SyncState> {
    Router::new()
        .route(
            "/sync/commands/register-event-service",
            post(submit_register),
        )
        .route("/sync/commands/remove-event-service", post(submit_remove))
        .route(
            "/sync/commands/record-event-service-sync",
            post(submit_record_sync),
        )
}

/// The state-dependent refusals, checked under the write lock.
#[derive(Error, Debug)]
pub enum EventServiceCommandError {
    #[error("A service is already connected for {0}")]
    AlreadyRegistered(String),
    #[error("No connected service with id {0}")]
    NotFound(String),
}

/// Normalize a service URL to its app root.
///
/// Accepts either the bare root or the full published feed URL, because both are
/// what people have in front of them when they set this up, and stores the root
/// either way so the endpoint path is appended exactly once. It also makes the
/// uniqueness check mean something: without it the same service registered as
/// `https://x.com` and `https://x.com/` is two connections double-posting every
/// event it publishes.
pub fn normalize_root_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/api/accounting/events")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

#[derive(Serialize, Deserialize)]
pub struct RegisterEventServiceRequest {
    pub expected_head_seq: i64,
    pub name: String,
    pub root_url: String,
}

#[derive(Serialize, Deserialize)]
pub struct RegisterEventServiceResponse {
    pub head: i64,
    /// Minted server-side. The caller needs it to file the service's key with the
    /// instance, and cannot choose it — see the module docs.
    pub service_id: String,
}

/// Connect a service to the group's books.
///
/// The uniqueness fence — no *active* service already registered for the same
/// normalized root URL — runs inside the append transaction, so two members
/// connecting the same shop at the same moment cannot both pass it. Getting that
/// wrong means the shop's every sale posted twice.
async fn submit_register(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RegisterEventServiceRequest>,
) -> Result<Json<RegisterEventServiceResponse>, ApiError> {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }
    let root_url = normalize_root_url(&req.root_url);
    if root_url.is_empty() {
        return Err(ApiError::bad_request("root_url is required"));
    }
    // The desktop defaults a bare host to https, so anything arriving without a
    // scheme is a client that did not. Refusing here rather than storing it means
    // the failure is "that URL is not a URL" instead of a fetch error weeks later.
    if !root_url.starts_with("http://") && !root_url.starts_with("https://") {
        return Err(ApiError::bad_request(
            "root_url must start with http:// or https://",
        ));
    }

    let service_id = uuid::Uuid::new_v4().to_string();
    let event = Event::EventServiceRegistered {
        service_id: service_id.clone(),
        name,
        root_url: root_url.clone(),
        // Omitted on purpose. See the module docs.
        api_key: None,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                let taken: bool = tx
                    .query_row(
                        "SELECT 1 FROM event_services WHERE root_url = ?1 AND status = 'active'",
                        [&root_url],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if taken {
                    return Ok(Verdict::Reject(
                        EventServiceCommandError::AlreadyRegistered(root_url.clone()),
                    ));
                }
                Ok(Verdict::Append(stamp(event, &actor)))
            },
            project,
        )
        .map_err(ApiError::store)?;

    match outcome {
        CheckedOutcome::Appended(stored) => Ok(Json(RegisterEventServiceResponse {
            head: stored.id,
            service_id,
        })),
        CheckedOutcome::HeadMismatch { actual, .. } => Err(ApiError::conflict(actual)),
        CheckedOutcome::Rejected(e) => Err(ApiError::domain(e)),
    }
}

#[derive(Serialize, Deserialize)]
pub struct RemoveEventServiceRequest {
    pub expected_head_seq: i64,
    pub service_id: String,
}

/// Disconnect a service from the group's books.
///
/// Removing something that is not there is a `422` rather than a silent success:
/// a stale list would otherwise report a disconnection that never happened, and
/// the service would keep being synced by whoever refreshes next.
///
/// This does **not** delete the key the instance holds — that is a separate call
/// to `/servicefeed`, deliberately, because the two live in different databases
/// and no transaction spans them. The desktop makes both; a key left behind for a
/// service nobody can see is inert, whereas a service visible with no key is a
/// button that cannot work.
async fn submit_remove(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RemoveEventServiceRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                let active: bool = tx
                    .query_row(
                        "SELECT 1 FROM event_services WHERE id = ?1 AND status = 'active'",
                        [&req.service_id],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if !active {
                    return Ok(Verdict::Reject(EventServiceCommandError::NotFound(
                        req.service_id.clone(),
                    )));
                }
                Ok(Verdict::Append(stamp(
                    Event::EventServiceRemoved {
                        service_id: req.service_id.clone(),
                    },
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<EventServiceCommandError>)
}

#[derive(Serialize, Deserialize)]
pub struct RecordEventServiceSyncRequest {
    pub expected_head_seq: i64,
    pub service_id: String,
    pub events_processed: u32,
    pub entries_created: u32,
    pub errors: u32,
}

/// Record that somebody synced this service, and with what result.
///
/// Bookkeeping rather than accounting — it moves no money, it updates the "last
/// synced / events / entries" columns the Services page shows. It is a shared
/// event rather than a local note because the question it answers is a group
/// question: *has anyone pulled the shop's sales this week?* On hosted books
/// nobody can see anyone else's machine, so a local note would leave every member
/// assuming somebody else had done it.
async fn submit_record_sync(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<RecordEventServiceSyncRequest>,
) -> Result<Json<crate::sync::SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| {
                let active: bool = tx
                    .query_row(
                        "SELECT 1 FROM event_services WHERE id = ?1 AND status = 'active'",
                        [&req.service_id],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if !active {
                    return Ok(Verdict::Reject(EventServiceCommandError::NotFound(
                        req.service_id.clone(),
                    )));
                }
                Ok(Verdict::Append(stamp(
                    Event::EventServiceSynced {
                        service_id: req.service_id.clone(),
                        events_processed: req.events_processed,
                        entries_created: req.entries_created,
                        errors: req.errors,
                    },
                    &actor,
                )))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<EventServiceCommandError>)
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

    async fn register(base: &str, name: &str, url: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{base}/sync/commands/register-event-service"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(base).await,
                "name": name,
                "root_url": url,
            }))
            .send()
            .await
            .unwrap()
    }

    /// The thing that was blocked: a member on hosted books connects a service and
    /// gets back an id they can file a key against.
    #[tokio::test]
    async fn a_service_is_connected_and_its_id_returned() {
        let base = serve().await;
        let r = register(&base, "Bugbear Bikes", "https://bugbearbikes.com").await;
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        let v: serde_json::Value = r.json().await.unwrap();
        assert!(
            uuid::Uuid::parse_str(v["service_id"].as_str().unwrap()).is_ok(),
            "the server must mint a real id: {v}"
        );
        assert_eq!(v["head"], 1);
    }

    /// The whole reason the key is not a field here. Every member replicates this
    /// log, so a key in it is a key on every member's laptop — and the endpoint
    /// must not even accept one, or a client could smuggle it in.
    #[tokio::test]
    async fn no_api_key_can_reach_the_shared_log() {
        let base = serve().await;
        let with_key = serde_json::json!({
            "expected_head_seq": 0,
            "name": "Bugbear Bikes",
            "root_url": "https://bugbearbikes.com",
            "api_key": "sk-live-do-not-share",
        });
        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/register-event-service"))
            .bearer_auth(TOKEN)
            .json(&with_key)
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
            !dump.contains("sk-live-do-not-share"),
            "a client-supplied key reached the shared log: {dump}"
        );
        assert!(
            !dump.contains("api_key"),
            "the field must be absent entirely, not null: {dump}"
        );
    }

    /// A client-chosen id would overwrite someone else's service — the projector
    /// does INSERT OR REPLACE on `event_services(id)`. Two registrations must
    /// produce two distinct services, not one that ate the other.
    #[tokio::test]
    async fn a_client_cannot_choose_an_id_and_clobber_someone_elses_service() {
        let base = serve().await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/register-event-service");

        let first: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": 0,
                "name": "Shop A",
                "root_url": "https://a.test",
                "service_id": "victim-service",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let second: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "name": "Shop B",
                "root_url": "https://b.test",
                "service_id": "victim-service",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_ne!(first["service_id"], second["service_id"]);
        assert_ne!(first["service_id"], "victim-service");
    }

    /// Registering the same shop twice would post its every sale twice. The URL is
    /// normalized first, so the trailing slash and the full feed path are the same
    /// service — which is exactly how a person would retype it.
    #[tokio::test]
    async fn the_same_service_cannot_be_connected_twice_however_its_url_is_spelled() {
        let base = serve().await;
        assert_eq!(
            register(&base, "Bugbear", "https://bugbearbikes.com")
                .await
                .status(),
            reqwest::StatusCode::OK
        );

        for spelling in [
            "https://bugbearbikes.com/",
            "https://bugbearbikes.com/api/accounting/events",
            "  https://bugbearbikes.com  ",
        ] {
            assert_eq!(
                register(&base, "Bugbear again", spelling).await.status(),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "{spelling} was accepted as a second connection to the same service, \
                 which would double-post every event it publishes"
            );
        }
    }

    /// Disconnecting frees the URL — otherwise a service removed by mistake could
    /// never be reconnected, and the only fix would be an administrator editing
    /// the group's database by hand.
    #[tokio::test]
    async fn a_service_can_be_disconnected_and_then_reconnected() {
        let base = serve().await;
        let v: serde_json::Value = register(&base, "Bugbear", "https://bugbearbikes.com")
            .await
            .json()
            .await
            .unwrap();
        let id = v["service_id"].as_str().unwrap().to_string();

        let http = reqwest::Client::new();
        let remove = |id: String, head: i64| {
            http.post(format!("{base}/sync/commands/remove-event-service"))
                .bearer_auth(TOKEN)
                .json(&serde_json::json!({ "expected_head_seq": head, "service_id": id }))
                .send()
        };

        assert_eq!(
            remove(id.clone(), head_of(&base).await)
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::OK
        );
        // Twice is a refusal, not a silent success: a stale list would otherwise
        // report a removal that never happened.
        assert_eq!(
            remove(id, head_of(&base).await).await.unwrap().status(),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            register(&base, "Bugbear", "https://bugbearbikes.com")
                .await
                .status(),
            reqwest::StatusCode::OK,
            "a disconnected service's URL must be free again"
        );
    }

    /// The counters the Services page shows are group state — on hosted books
    /// nobody can see anyone else's machine, so "has anyone pulled this week?"
    /// has to be answerable from the books.
    #[tokio::test]
    async fn a_sync_is_recorded_against_the_group_books() {
        let base = serve().await;
        let v: serde_json::Value = register(&base, "Bugbear", "https://bugbearbikes.com")
            .await
            .json()
            .await
            .unwrap();
        let id = v["service_id"].as_str().unwrap();

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/record-event-service-sync"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "service_id": id,
                "events_processed": 12,
                "entries_created": 10,
                "errors": 2,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK);

        // …and recording against a service that is not connected is refused, so a
        // stale client cannot resurrect counters for something nobody can see.
        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/record-event-service-sync"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head_of(&base).await,
                "service_id": "no-such-service",
                "events_processed": 1,
                "entries_created": 1,
                "errors": 0,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn a_nonsense_url_is_refused_and_auth_is_required() {
        let base = serve().await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/register-event-service");
        let body = serde_json::json!({
            "expected_head_seq": 0,
            "name": "Bugbear",
            "root_url": "https://bugbearbikes.com",
        });

        assert_eq!(
            http.post(&url).json(&body).send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        for bad in ["", "   ", "bugbearbikes.com", "ftp://bugbearbikes.com"] {
            let mut b = body.clone();
            b["root_url"] = serde_json::json!(bad);
            assert_eq!(
                http.post(&url)
                    .bearer_auth(TOKEN)
                    .json(&b)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::BAD_REQUEST,
                "{bad:?} was accepted as a service URL"
            );
        }

        let mut b = body.clone();
        b["name"] = serde_json::json!("  ");
        assert_eq!(
            http.post(&url)
                .bearer_auth(TOKEN)
                .json(&b)
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
    }
}
