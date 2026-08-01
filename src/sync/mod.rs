//! Prototype of the multi-tenant online-write transport (SPEC §4.1 / §6.5).
//!
//! Exposes the [`EventStore::append_checked`] concurrency primitive over HTTP so
//! we can de-risk the network boundary *before* the closed `accountir-server`
//! exists. The model the spec decided on:
//!
//!   - The server holds the canonical event log and serializes appends.
//!   - Every request is authenticated with a bearer token → an `actor_id`; the
//!     server stamps that identity on the events it writes (never trusting a
//!     client-supplied actor). [Prototype: tokens are an in-memory map; a real
//!     server uses `accountir-proxy`'s sessions/API-keys.]
//!   - A client submits a **command** with the log head it last observed
//!     (`expected_head_seq`). The server runs the command's real domain
//!     invariants *inside* the append transaction (the same in-txn checks the
//!     local command handlers use — it does not blind-append). On a head match it
//!     appends and returns the new head; on a mismatch it replies `409` with the
//!     current head so the client refetches (`GET /sync/events`) and retries; on a
//!     domain violation it replies `422`.
//!
//! `/sync/commands/post-entry` is the first real command wired end to end. The
//! generic `/sync/submit` remains as the raw primitive demo (it blind-appends
//! after `validate_event`, so it is NOT how real writes flow — real writes go
//! through a command endpoint that validates server-side).

use crate::commands::entry_commands::{
    build_post_entry_in_txn, check_entry_pure, EntryCommandError, EntryLine, PostEntryCommand,
    PostEntryStep,
};
use crate::events::payload::hash_to_hex;
use crate::events::types::{Event, EventEnvelope, JournalEntrySource, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use axum::{
    extract::{FromRequestParts, Query, State},
    http::{header::AUTHORIZATION, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub mod binding;
pub mod client;
pub mod commands;
pub mod reads;
pub mod replica;
pub use binding::GroupBinding;
pub use client::{SyncClient, SyncClientError};
pub use replica::{AppliedRange, ReplicaError};

/// Resolves a bearer token to an authenticated `actor_id` (or `None` if invalid).
/// This is the transport's auth seam: `accountir-app` ships the in-memory
/// [`StaticTokens`] impl for the prototype/tests, and a real deployment
/// (`accountir-server`) provides its own — e.g. one backed by `accountir-proxy`
/// sessions / API keys — and passes it to [`SyncState::with_auth`], reusing this
/// whole transport unchanged. Async so a real backend can validate against a
/// session store / network without blocking the request task.
pub trait AuthBackend: Send + Sync {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
}

/// In-memory bearer-token → actor_id map. The prototype/test auth backend; a real
/// server swaps this for an `accountir-proxy`-backed [`AuthBackend`].
pub struct StaticTokens(pub HashMap<String, String>);

impl AuthBackend for StaticTokens {
    fn authenticate<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        let actor = self.0.get(token).cloned();
        Box::pin(async move { actor })
    }
}

/// Shared server state: the single canonical event store behind a mutex, plus the
/// pluggable auth backend. Per SPEC §4.1 the server is single-writer, so one lock
/// serializes all appends; `append_checked`'s `IMMEDIATE` transaction is the real
/// serialization point, the mutex just keeps `&mut EventStore` sound.
#[derive(Clone)]
pub struct SyncState {
    pub store: Arc<Mutex<EventStore>>,
    pub auth: Arc<dyn AuthBackend>,
}

impl SyncState {
    /// Prototype/test convenience: authenticate against an in-memory
    /// bearer-token → actor_id map.
    pub fn new(store: EventStore, tokens: HashMap<String, String>) -> Self {
        Self::with_auth(store, Arc::new(StaticTokens(tokens)))
    }

    /// Construct with any [`AuthBackend`] — the seam `accountir-server` uses to
    /// plug in real (`accountir-proxy`) auth without forking the transport.
    pub fn with_auth(store: EventStore, auth: Arc<dyn AuthBackend>) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            auth,
        }
    }
}

/// The transport router.
pub fn router(state: SyncState) -> Router {
    let router = Router::new()
        .route("/sync/head", get(get_head))
        .route("/sync/events", get(get_events))
        .route("/sync/commands/post-entry", post(submit_post_entry))
        .merge(commands::router())
        .merge(reads::router());
    // The raw blind-append primitive bypasses every domain invariant, so it must
    // never be reachable in a real deployment — an authenticated member could
    // otherwise forge arbitrary events (unbalanced entries, privilege-shaped
    // payloads). It is a test-only affordance for exercising the head-CAS
    // transport directly; real writes go through the validated command endpoints.
    #[cfg(test)]
    let router = router.route("/sync/submit", post(submit));
    router.with_state(state)
}

/// The authenticated principal, resolved from the `Authorization: Bearer <token>`
/// header via the state's [`AuthBackend`].
pub struct AuthedUser(pub String);

impl FromRequestParts<SyncState> for AuthedUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SyncState,
    ) -> Result<Self, Self::Rejection> {
        let unauthorized = || ApiError {
            status: StatusCode::UNAUTHORIZED,
            body: serde_json::json!({ "error": "unauthorized" }),
        };
        let token = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(unauthorized)?;
        match state.auth.authenticate(token).await {
            Some(actor) => Ok(AuthedUser(actor)),
            None => Err(unauthorized()),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct HeadResponse {
    pub head: i64,
}

async fn get_head(
    _user: AuthedUser,
    State(st): State<SyncState>,
) -> Result<Json<HeadResponse>, ApiError> {
    let store = st.store.lock().unwrap();
    let head = store.latest_id().map_err(ApiError::store)?.unwrap_or(0);
    Ok(Json(HeadResponse { head }))
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    /// Page size. A replica catching up from zero must not be handed the entire
    /// log in one response — that is an unbounded allocation on both ends driven
    /// by a client-chosen `since`. Clamped in [`EventsQuery::page_limit`].
    limit: Option<u32>,
}

/// Default and maximum page sizes for `GET /sync/events`.
///
/// The default is what a catching-up replica gets when it doesn't ask; the
/// maximum is the ceiling a client cannot talk the server past, because the
/// response is materialized in memory before it is serialized.
const EVENTS_DEFAULT_LIMIT: u32 = 500;
const EVENTS_MAX_LIMIT: u32 = 1000;

impl EventsQuery {
    /// Clamp the requested page size into `1..=EVENTS_MAX_LIMIT`. A `0` or absent
    /// limit means "use the default" rather than "return nothing": a client that
    /// mis-serializes its limit should catch up slowly, not stall forever on an
    /// empty page it reads as "already up to date".
    fn page_limit(&self) -> usize {
        self.limit
            .filter(|l| *l > 0)
            .unwrap_or(EVENTS_DEFAULT_LIMIT)
            .min(EVENTS_MAX_LIMIT) as usize
    }
}

/// A log entry as seen by a client catching up. `seq` is the canonical order.
///
/// `timestamp` / `received_at` / `hash` are the fields a **replica** needs to
/// mirror the row faithfully — without the timestamp and the hash a client
/// cannot re-derive `compute_event_hash` and therefore cannot tell a genuine
/// server log from a mangled one. They are `Option` + `#[serde(default)]` purely
/// for wire compatibility with an older peer; the replica path fails closed when
/// they are absent (see [`replica::apply_batch`]) rather than trusting a log it
/// cannot verify.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SyncEvent {
    pub seq: i64,
    pub actor_id: Option<String>,
    pub user_id: String,
    pub event: Event,
    /// Client wall-clock time, RFC3339 on the wire. A hash input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    /// Server-stamped receive time. Not a hash input; mirrored for audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_at: Option<DateTime<Utc>>,
    /// Hex-encoded SHA-256 of the stored row, as the server holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EventsResponse {
    pub head: i64,
    pub events: Vec<SyncEvent>,
}

/// Fetch events after `since` (default 0), up to `limit`, plus the current head,
/// so a client can rebuild its view before retrying a conflicted submit or catch
/// a replica up.
///
/// `head` is the *canonical* head, not the last seq in this page: that is what
/// tells a paging replica whether to come back for more. Truncation is therefore
/// visible to the client (`last seq < head`) rather than silent.
async fn get_events(
    _user: AuthedUser,
    State(st): State<SyncState>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<EventsResponse>, ApiError> {
    let store = st.store.lock().unwrap();
    let stored = store
        .get_after_limited(q.since.unwrap_or(0), q.page_limit())
        .map_err(ApiError::store)?;
    let head = store.latest_id().map_err(ApiError::store)?.unwrap_or(0);
    let events = stored.into_iter().map(SyncEvent::from).collect();
    Ok(Json(EventsResponse { head, events }))
}

impl From<StoredEvent> for SyncEvent {
    fn from(s: StoredEvent) -> Self {
        SyncEvent {
            seq: s.id,
            actor_id: s.actor_id,
            user_id: s.user_id,
            timestamp: Some(s.timestamp),
            received_at: s.received_at,
            hash: Some(hash_to_hex(&s.hash)),
            event: s.event,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct SubmitResponse {
    /// The log head after the append — the client's new `expected_head_seq`.
    pub head: i64,
}

#[cfg(test)]
#[derive(Deserialize)]
struct SubmitRequest {
    expected_head_seq: i64,
    event: Event,
}

/// Raw primitive demo (test-only — see `router`): append a client-provided event
/// under optimistic concurrency, stamping the authenticated actor. Blind-appends
/// (no per-command domain invariants), so it is gated out of production builds;
/// real writes go through the validated command endpoints. Exercises head-CAS.
#[cfg(test)]
async fn submit(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SubmitRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let mut store = st.store.lock().unwrap();
    let event = req.event;
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |_tx| {
                Ok(Verdict::<EventEnvelope, ()>::Append(
                    stamp(event.clone(), &actor),
                ))
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, |()| ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        body: serde_json::json!({ "error": "rejected" }),
    })
}

// --- Real command: post a journal entry, validated server-side ---

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PostEntryLine {
    pub account_id: String,
    /// Smallest currency unit. Positive = debit, negative = credit.
    pub amount: i64,
    pub currency: String,
    #[serde(default)]
    pub memo: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PostEntryRequest {
    pub expected_head_seq: i64,
    pub date: NaiveDate,
    pub memo: String,
    pub lines: Vec<PostEntryLine>,
    #[serde(default)]
    pub reference: Option<String>,
}

/// Post a journal entry over the wire. The server runs the SAME invariants the
/// local `post_entry` handler uses — pure balance/line checks, then in-txn
/// reference dedup + accounts-active / period-open fences via
/// [`build_post_entry_in_txn`] — honoring the client's `expected_head_seq`. It
/// does not trust the client: a bad entry is a `422`, a stale head is a `409`.
async fn submit_post_entry(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<PostEntryRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let cmd = PostEntryCommand {
        date: req.date,
        memo: req.memo,
        lines: req
            .lines
            .into_iter()
            .map(|l| EntryLine {
                account_id: l.account_id,
                amount: l.amount,
                currency: l.currency,
                exchange_rate: None,
                memo: l.memo,
            })
            .collect(),
        reference: req.reference,
        source: Some(JournalEntrySource::Manual),
    };

    // Pure (state-independent) validation → 422 without touching the store.
    check_entry_pure(&cmd).map_err(ApiError::domain)?;

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_post_entry_in_txn(tx, &cmd)? {
                PostEntryStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                PostEntryStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<EntryCommandError>)
}

// --- shared helpers ---

/// Stamp server identity on a client-originated event: `user_id` and `actor_id`
/// both the authenticated principal, `received_at` = now (canonical order).
pub(crate) fn stamp(event: Event, actor: &str) -> EventEnvelope {
    EventEnvelope::new(event, actor.to_string())
        .with_actor(Some(actor.to_string()))
        .with_received_at(Some(Utc::now()))
}

/// The `append_checked` project closure: fold the event into projections in the
/// same transaction.
pub(crate) fn project(tx: &rusqlite::Transaction<'_>, stored: &crate::events::types::StoredEvent) -> Result<(), EventStoreError> {
    Projector::new(tx)
        .apply(stored)
        .map_err(|e| EventStoreError::Projection(e.to_string()))
}

/// Map an append outcome to an HTTP response: `Appended` → 200 + new head,
/// `HeadMismatch` → 409 + current head, `Rejected(e)` → caller-supplied error.
pub(crate) fn outcome_to_response<E>(
    outcome: CheckedOutcome<crate::events::types::StoredEvent, E>,
    reject: impl FnOnce(E) -> ApiError,
) -> Result<Json<SubmitResponse>, ApiError> {
    match outcome {
        CheckedOutcome::Appended(stored) => Ok(Json(SubmitResponse { head: stored.id })),
        CheckedOutcome::HeadMismatch { actual, .. } => Err(ApiError::conflict(actual)),
        CheckedOutcome::Rejected(e) => Err(reject(e)),
    }
}

/// A JSON error response carrying an HTTP status.
pub struct ApiError {
    status: StatusCode,
    body: serde_json::Value,
}

impl ApiError {
    pub(crate) fn store(e: EventStoreError) -> Self {
        // Don't leak internal detail (SQLite messages / paths) to the client; log
        // it server-side and return a generic 500. (Review finding L1.)
        eprintln!("sync: internal store error: {e}");
        ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: serde_json::json!({ "error": "internal error" }),
        }
    }

    /// A domain-invariant violation (unbalanced, inactive account, closed period,
    /// duplicate reference, ...): `422` with the error message.
    pub(crate) fn domain<E: std::fmt::Display>(e: E) -> Self {
        ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: serde_json::json!({ "error": e.to_string() }),
        }
    }

    /// A stale-head conflict: the log moved to `current_head`. Client refetches.
    pub(crate) fn conflict(current_head: i64) -> Self {
        ApiError {
            status: StatusCode::CONFLICT,
            body: serde_json::json!({ "error": "head_mismatch", "current_head": current_head }),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::domain::AccountType;
    use crate::events::types::{Event, EventAccountType};
    use crate::store::migrations::init_schema;

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

    // --- raw-primitive transport test (unchanged semantics, now authed) ---

    #[tokio::test]
    async fn expected_head_seq_conflict_then_refetch_and_retry() {
        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();

        let acct = |n: &str| {
            serde_json::json!({
                "expected_head_seq": 0,
                "event": Event::AccountCreated {
                    account_id: format!("a-{n}"),
                    account_type: EventAccountType::Asset,
                    account_number: n.to_string(),
                    name: format!("A{n}"),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                },
            })
        };

        // Missing token → 401.
        let unauth = http
            .post(format!("{base}/sync/submit"))
            .json(&acct("1000"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Client B wins the race at head 0.
        let win = http
            .post(format!("{base}/sync/submit"))
            .bearer_auth(TOKEN)
            .json(&acct("1000"))
            .send()
            .await
            .unwrap();
        assert_eq!(win.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&win.json().await.unwrap()), 1);

        // Client A, stale head 0 → 409 with current head 1.
        let conflict = http
            .post(format!("{base}/sync/submit"))
            .bearer_auth(TOKEN)
            .json(&acct("2000"))
            .send()
            .await
            .unwrap();
        assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);
        let cur = conflict.json::<serde_json::Value>().await.unwrap()["current_head"]
            .as_i64()
            .unwrap();
        assert_eq!(cur, 1);

        // Refetch + retry against fresh head → success.
        let mut body = acct("2000");
        body["expected_head_seq"] = serde_json::json!(cur);
        let retry = http
            .post(format!("{base}/sync/submit"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(retry.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&retry.json().await.unwrap()), 2);
    }

    // --- real command test: post_entry validated server-side ---

    async fn serve_with_accounts() -> (String, String, String) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let asset = mk_account(&mut store, "1000", AccountType::Asset);
        let expense = mk_account(&mut store, "5000", AccountType::Expense);
        let base = serve(SyncState::new(store, tokens())).await;
        (base, asset, expense)
    }

    fn entry_body(expected_head: i64, debit: &str, credit: &str, amount: i64) -> serde_json::Value {
        serde_json::json!({
            "expected_head_seq": expected_head,
            "date": "2026-03-04",
            "memo": "test entry",
            "lines": [
                { "account_id": debit, "amount": amount, "currency": "USD" },
                { "account_id": credit, "amount": -amount, "currency": "USD" },
            ],
        })
    }

    #[tokio::test]
    async fn post_entry_command_validated_server_side() {
        let (base, asset, expense) = serve_with_accounts().await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/post-entry");
        // Two AccountCreated events were seeded, so head starts at 2.

        // Happy path: balanced entry, active accounts → 200, head 3.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&entry_body(2, &expense, &asset, 5000))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        assert_eq!(head_of(&ok.json().await.unwrap()), 3);

        // Server-side pure validation: unbalanced → 422, nothing appended.
        let unbalanced = serde_json::json!({
            "expected_head_seq": 3, "date": "2026-03-04", "memo": "bad",
            "lines": [
                { "account_id": expense, "amount": 5000, "currency": "USD" },
                { "account_id": asset, "amount": -4000, "currency": "USD" },
            ],
        });
        let r = http.post(&url).bearer_auth(TOKEN).json(&unbalanced).send().await.unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

        // Stale head → 409 (log is at 3 after the happy path).
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&entry_body(2, &expense, &asset, 100))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn post_entry_rejects_inactive_account_server_side() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let asset = mk_account(&mut store, "1000", AccountType::Asset);
        let expense = mk_account(&mut store, "5000", AccountType::Expense);
        // Deactivate the expense account (zero balance) so the in-txn fence rejects.
        AccountCommands::new(&mut store, "seed".to_string())
            .deactivate_account(crate::commands::account_commands::DeactivateAccountCommand {
                account_id: expense.clone(),
                reason: None,
            })
            .unwrap();
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/post-entry"))
            .bearer_auth(TOKEN)
            .json(&entry_body(head, &expense, &asset, 5000))
            .send()
            .await
            .unwrap();
        // The server enforces the fence: inactive account → 422, not a blind append.
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(r
            .json::<serde_json::Value>()
            .await
            .unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("inactive"));
    }

    // --- SyncClient (client half of the seam) ---

    fn line(account_id: &str, amount: i64) -> PostEntryLine {
        PostEntryLine {
            account_id: account_id.to_string(),
            amount,
            currency: "USD".to_string(),
            memo: None,
        }
    }

    fn balanced(debit: &str, credit: &str, amount: i64) -> Vec<PostEntryLine> {
        vec![line(debit, amount), line(credit, -amount)]
    }

    /// The client owns the optimistic-concurrency loop: client A starts with a
    /// head that goes stale (client B writes first), yet A's post succeeds because
    /// the client auto-adopts the server head and retries — the caller never sees
    /// the 409.
    #[tokio::test]
    async fn sync_client_auto_resolves_stale_head() {
        let (base, asset, expense) = serve_with_accounts().await; // head starts at 2
        let date = NaiveDate::from_ymd_opt(2026, 3, 4).unwrap();

        let mut a = SyncClient::new(&base, TOKEN);
        let mut b = SyncClient::new(&base, TOKEN);
        assert_eq!(a.refresh_head().await.unwrap(), 2);
        b.refresh_head().await.unwrap();

        // B posts first → log at 3. A's cached head (2) is now stale.
        assert_eq!(b.post_entry(date, "b", balanced(&expense, &asset, 1000), None).await.unwrap(), 3);

        // A posts against its stale head; the client resolves the 409 internally.
        let ha = a
            .post_entry(date, "a", balanced(&expense, &asset, 2000), None)
            .await
            .unwrap();
        assert_eq!(ha, 4, "client auto-resolved the stale head");
        assert_eq!(a.head(), 4);
    }

    #[tokio::test]
    async fn sync_client_surfaces_domain_rejection() {
        let (base, asset, expense) = serve_with_accounts().await;
        let date = NaiveDate::from_ymd_opt(2026, 3, 4).unwrap();
        let mut c = SyncClient::new(&base, TOKEN);
        c.refresh_head().await.unwrap();

        // Unbalanced entry → the server rejects; the client surfaces it as Rejected.
        let err = c
            .post_entry(date, "bad", vec![line(&expense, 5000), line(&asset, -4000)], None)
            .await
            .unwrap_err();
        assert!(matches!(err, SyncClientError::Rejected(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn sync_client_rejects_bad_token() {
        let (base, _asset, _expense) = serve_with_accounts().await;
        let mut c = SyncClient::new(&base, "wrong-token");
        assert!(matches!(
            c.refresh_head().await.unwrap_err(),
            SyncClientError::Unauthorized
        ));
    }

    /// Demonstrates the AuthBackend seam accountir-server relies on: a bespoke
    /// backend replaces the in-memory token map via `SyncState::with_auth`, and
    /// the whole transport is reused unchanged.
    #[tokio::test]
    async fn custom_auth_backend_plugs_in_via_with_auth() {
        struct PrefixAuth;
        impl AuthBackend for PrefixAuth {
            fn authenticate<'a>(
                &'a self,
                token: &'a str,
            ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
                // Any token prefixed "grp-" authenticates as "member".
                let actor = token.strip_prefix("grp-").map(|_| "member".to_string());
                Box::pin(async move { actor })
            }
        }

        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        let base = serve(SyncState::with_auth(store, Arc::new(PrefixAuth))).await;
        let http = reqwest::Client::new();

        let ok = http
            .get(format!("{base}/sync/head"))
            .bearer_auth("grp-anything")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);

        let rejected = http
            .get(format!("{base}/sync/head"))
            .bearer_auth("no-prefix")
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    // --- replica catch-up over the real transport ---

    /// The whole point of the read path, end to end: a fresh empty ledger follows
    /// a live server over HTTP and ends up holding the same log, byte for byte,
    /// with the same ids. If this passes, `local_cursor` is a valid cursor.
    #[tokio::test]
    async fn a_replica_catches_up_to_the_server_over_http_and_matches_it_exactly() {
        let mut server_store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        mk_account(&mut server_store, "1000", AccountType::Asset);
        mk_account(&mut server_store, "2000", AccountType::Liability);
        mk_account(&mut server_store, "3000", AccountType::Equity);
        let expected_hashes = server_store.get_all_hashes().unwrap();

        let base = serve(SyncState::new(server_store, tokens())).await;

        let mut replica_store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            crate::store::migrations::run_migrations(s.connection()).unwrap();
            binding::bind(s.connection(), "acme", &base, "https://cp").unwrap();
            s
        };

        // Page deliberately smaller than the log so the paging loop is exercised.
        let mut client = SyncClient::with_head(&base, TOKEN, 0);
        loop {
            let cursor = replica::local_cursor(&replica_store).unwrap();
            let page = client.events_page(cursor, 2).await.unwrap();
            if page.events.is_empty() {
                assert_eq!(cursor, page.head, "an empty page means we are caught up");
                break;
            }
            replica::apply_batch(&mut replica_store, &page.events).unwrap();
        }

        assert_eq!(replica_store.get_all_hashes().unwrap(), expected_hashes);
        assert_eq!(replica::local_cursor(&replica_store).unwrap(), 3);
        // The projections came with them — a replica is usable, not just archived.
        let accounts: i64 = replica_store
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accounts, 3);

        // And the prefix check the desktop runs on every open agrees.
        let at_head = client.events_page(2, 1).await.unwrap();
        replica::verify_prefix(&replica_store, at_head.events.first()).unwrap();
    }

    /// A client must not be able to talk the server into materializing an
    /// arbitrarily large response, and a `limit` of zero must not mean "no events"
    /// — a replica reading an empty page as "caught up" would stall forever.
    #[tokio::test]
    async fn the_events_page_limit_is_clamped_and_head_still_reports_the_whole_log() {
        let mut store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        for i in 0..5 {
            mk_account(&mut store, &format!("{}000", i + 1), AccountType::Asset);
        }
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();

        let page: EventsResponse = http
            .get(format!("{base}/sync/events?since=0&limit=2"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2);
        // Truncation has to be visible, or a replica stops one page short and
        // believes it is up to date.
        assert_eq!(page.head, 5);

        for query in ["limit=0", "limit=100000", ""] {
            let page: EventsResponse = http
                .get(format!("{base}/sync/events?since=0&{query}"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(page.events.len(), 5, "query {query:?}");
        }
    }

    /// Old clients pin the shape they rely on; new fields must be additive.
    #[test]
    fn a_sync_event_still_deserializes_without_the_replica_fields() {
        let json = serde_json::json!({
            "seq": 1,
            "actor_id": null,
            "user_id": "u",
            "event": Event::AccountCreated {
                account_id: "a".into(),
                account_number: "1000".into(),
                name: "Cash".into(),
                account_type: EventAccountType::Asset,
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            },
        });
        let e: SyncEvent = serde_json::from_value(json).unwrap();
        assert!(e.timestamp.is_none() && e.hash.is_none());
    }
}
