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
//! Wired here: `create-account`, `deactivate-account`, `update-account` and
//! `seed-default-accounts`. The last two append a **batch** — one `AccountUpdated`
//! per changed field, one `AccountCreated` per default account — so they go
//! through `append_checked_many` and `outcome_to_response_many` instead. Same
//! contract otherwise: one attempt, no internal retry, `409` on a stale head.

use crate::commands::account_commands::{
    build_create_account_in_txn, build_deactivate_account_in_txn, build_delete_account_in_txn,
    build_ensure_account_path_in_txn, build_seed_default_accounts_in_txn,
    build_update_account_in_txn, AccountBatchStep, AccountCommandError, AccountStep,
    CreateAccountCommand, DeactivateAccountCommand, EnsurePathStep, UpdateAccountCommand,
};
use crate::domain::AccountType;
use crate::store::event_store::Verdict;
use crate::sync::{
    outcome_to_response, outcome_to_response_many, project, stamp, ApiError, AuthedUser,
    SubmitResponse, SyncState,
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
        .route("/sync/commands/update-account", post(submit_update_account))
        .route("/sync/commands/delete-account", post(submit_delete_account))
        .route(
            "/sync/commands/seed-default-accounts",
            post(submit_seed_default_accounts),
        )
        .route(
            "/sync/commands/ensure-account-path",
            post(submit_ensure_account_path),
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

#[derive(Serialize, Deserialize)]
pub struct DeleteAccountRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
}

/// Remove an account that nothing points at, over the wire.
///
/// # Why the check has to happen in here
///
/// Deletion is refused when anything at all references the account — a journal
/// line including a voided one, a child account, a tax line mapping, a bank
/// account, a staged import row. On a group's books two members can be looking
/// at the same empty account while one of them posts to it, and a check made
/// before the write lock would pass for the member who read first. So the
/// refusal is evaluated inside the transaction against locked state, and the
/// loser gets a 422 naming what now points at it rather than a deleted account
/// with entries hanging off it.
async fn submit_delete_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<DeleteAccountRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    if req.account_id.trim().is_empty() {
        return Err(ApiError::bad_request("account_id is required"));
    }
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked(
            req.expected_head_seq,
            move |tx| match build_delete_account_in_txn(tx, &req.account_id)? {
                AccountStep::Append(event) => Ok(Verdict::Append(stamp(event, &actor))),
                AccountStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response(outcome, ApiError::domain::<AccountCommandError>)
}

/// Edit an account over the wire. Serde-reusable DTO.
///
/// Every field is optional and `None` means "leave alone" — so a client that only
/// renames sends only `name`. `parent_id` is doubly wrapped because it has three
/// states, and collapsing them would make "clear the parent" unexpressible:
/// absent = no change, `null` = clear it, a string = set it.
#[derive(Serialize, Deserialize)]
pub struct UpdateAccountRequest {
    pub expected_head_seq: i64,
    pub account_id: String,
    #[serde(default)]
    pub account_number: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// `Some(None)` clears the parent, `Some(Some(id))` sets it, `None` leaves it.
    ///
    /// Both attributes are load-bearing. `deserialize_with` is what makes an
    /// explicit `null` mean `Some(None)` — plain serde collapses `null` and
    /// "absent" both to `None`, so "clear the parent" would be unexpressible.
    /// `skip_serializing_if` is the other half: without it the client's `None`
    /// would go on the wire as `"parent_id": null`, which the fixed deserializer
    /// then reads back as `Some(None)`, and every rename would silently
    /// **un-parent the account**. See
    /// `an_omitted_parent_is_not_the_same_as_a_null_one`.
    #[serde(
        default,
        deserialize_with = "explicit_null_is_a_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_id: Option<Option<String>>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Distinguish an explicit JSON `null` from an absent field.
///
/// Serde folds both into `None` by default. Wrapping whatever we deserialize in
/// `Some` means a present-but-null field arrives as `Some(None)`, while an absent
/// one is left to `#[serde(default)]` and stays `None`. This is the standard
/// "double option" trick; it is spelled out here rather than pulled from
/// `serde_with` so the ledger crate does not gain a dependency the server would
/// have to build.
fn explicit_null_is_a_value<'de, D, T>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

/// Edit an account, validated server-side. Runs the SAME diff the local
/// `update_account` handler uses via [`build_update_account_in_txn`], under the
/// write lock, so the `old_value` recorded on each `AccountUpdated` is true at the
/// instant it is appended. A rename onto a taken number is a `422`, a missing
/// account a `422`, a stale head a `409`.
///
/// An edit that changes nothing appends nothing and returns the unchanged head —
/// see [`outcome_to_response_many`].
async fn submit_update_account(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<UpdateAccountRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let cmd = UpdateAccountCommand {
        account_id: req.account_id,
        account_number: req.account_number,
        name: req.name,
        parent_id: req.parent_id,
        description: req.description,
    };

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_update_account_in_txn(tx, &cmd)? {
                AccountBatchStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                AccountBatchStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response_many(outcome, expected, ApiError::domain::<AccountCommandError>)
}

/// Lay down the default chart of accounts. Serde-reusable DTO.
///
/// Carries no chart of its own — [`crate::commands::account_commands::DEFAULT_CHART`]
/// is the definition, and it lives server-side. A client-supplied chart would let
/// one replica seed a group with accounts another replica's build has never heard
/// of.
#[derive(Serialize, Deserialize)]
pub struct SeedDefaultAccountsRequest {
    pub expected_head_seq: i64,
}

/// Seed the default chart, validated server-side and **all-or-nothing**.
///
/// This is the command that makes "Seed defaults" work on group-hosted books at
/// all. The desktop cannot do it by appending locally — on a replica the event ids
/// are the server's sequence numbers — and it cannot do it as N `create-account`
/// calls either, because the sync engine carries one pending write at a time and
/// there is no transaction spanning them: a failure partway would leave some
/// accounts created and some not, with nothing recording which.
///
/// So the whole chart is one `append_checked_many` batch. If any number is already
/// taken the batch is rejected as a `422` and nothing is appended — seeding is a
/// "this ledger is new" action, and a half-seeded chart is worse than none.
async fn submit_seed_default_accounts(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<SeedDefaultAccountsRequest>,
) -> Result<Json<SubmitResponse>, ApiError> {
    let expected = req.expected_head_seq;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_seed_default_accounts_in_txn(tx)? {
                AccountBatchStep::Append(events) => Ok(Verdict::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                )),
                AccountBatchStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    outcome_to_response_many(outcome, expected, ApiError::domain::<AccountCommandError>)
}

/// Create the account a path names, and any ancestors it does not have yet.
#[derive(Serialize, Deserialize)]
pub struct EnsureAccountPathRequest {
    pub expected_head_seq: i64,
    /// Colon-separated, as typed: `Expenses:Software:SaaS`.
    pub path: String,
    /// Every account created along the way takes this type, the way the local
    /// modal does it — children conventionally share their parent's type.
    pub account_type: AccountType,
}

/// What the group's ledger holds for that path now.
#[derive(Serialize, Deserialize)]
pub struct EnsureAccountPathResponse {
    pub head: i64,
    /// The account the path names — newly created, or the one already there.
    pub account_id: String,
    /// How many accounts this call had to create. Zero when the path already
    /// existed, which is a success: the caller wanted the account, not the work.
    pub created: usize,
}

/// Ensure a whole account path exists, as one append.
///
/// The refusal this removes: the desktop's "create the account I just typed" modal
/// may need several accounts (`Expenses:Software:SaaS` can be three), and the
/// single-account command takes them one at a time with no all-or-nothing across
/// them — so on hosted books the modal could only say no, and the person was sent
/// to the Accounts page to build the path by hand, one level at a time.
///
/// Here the whole path is one `append_checked_many`: every missing account or none.
/// A duplicate account number is a 422 and nothing is written; a stale head is a 409
/// for the client to refetch and retry, exactly like every other command.
async fn submit_ensure_account_path(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<EnsureAccountPathRequest>,
) -> Result<Json<EnsureAccountPathResponse>, ApiError> {
    let expected = req.expected_head_seq;
    // Filled inside the append transaction, read after it: the leaf id is decided
    // there (it may be an account this batch mints), and the response cannot be
    // built without it.
    let found: std::sync::Arc<std::sync::Mutex<Option<(String, usize)>>> = Default::default();
    let sink = found.clone();
    let path = req.path.clone();
    let account_type = req.account_type;
    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            move |tx| match build_ensure_account_path_in_txn(tx, &path, account_type)? {
                EnsurePathStep::Ensured { events, leaf_id } => {
                    if let Ok(mut slot) = sink.lock() {
                        *slot = Some((leaf_id, events.len()));
                    }
                    Ok(Verdict::Append(
                        events.into_iter().map(|e| stamp(e, &actor)).collect(),
                    ))
                }
                EnsurePathStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            project,
        )
        .map_err(ApiError::store)?;
    let head = outcome_to_response_many(outcome, expected, ApiError::domain::<AccountCommandError>)?
        .0
        .head;
    let (account_id, created) = found
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .ok_or_else(|| {
            ApiError::store(crate::store::event_store::EventStoreError::Backend(
                "the account path was appended without recording which account it named"
                    .to_string(),
            ))
        })?;
    Ok(Json(EnsureAccountPathResponse {
        head,
        account_id,
        created,
    }))
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
    /// The whole point of the batch: seven accounts or none.
    #[tokio::test]
    async fn seeding_the_default_chart_is_one_atomic_batch() {
        use crate::commands::account_commands::DEFAULT_CHART;

        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/seed-default-accounts");

        // Missing token → 401, nothing appended.
        let unauth = http
            .post(&url)
            .json(&serde_json::json!({ "expected_head_seq": 0 }))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Happy path: the head advances by exactly one per default account. The
        // local seeder needs extra AccountUpdated events to re-parent; this one
        // does not, because it knows the ids while it is still building the batch.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": 0 }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        let head = head_of(&ok.json().await.unwrap());
        assert_eq!(
            head,
            DEFAULT_CHART.len() as i64,
            "one AccountCreated per default account, parents included"
        );

        // Seeding twice → 422 on the duplicate number, and — the property that
        // matters — the log does not move. A partial re-seed would leave a chart
        // nobody could reconcile.
        let again = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": head }))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

        let after = http
            .get(format!("{base}/sync/head"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(
            after["head"].as_i64().unwrap(),
            head,
            "a rejected seed must append NOTHING, not a partial chart"
        );
    }

    /// A single pre-existing account number is enough to refuse the whole chart —
    /// the case where a human created `1000` by hand before anyone pressed Seed.
    #[tokio::test]
    async fn one_taken_number_rejects_the_entire_seed() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        mk_account(&mut store, "4000", AccountType::Equity);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/seed-default-accounts"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": head }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

        // Not even the accounts BEFORE the collision (1000, 1001, 2000, 3000) may
        // survive — they are built before `4000` is reached.
        let after = reqwest::Client::new()
            .get(format!("{base}/sync/head"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(after["head"].as_i64().unwrap(), head);
    }

    #[tokio::test]
    async fn update_account_command_validated_server_side() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let a = mk_account(&mut store, "1000", AccountType::Asset);
        let _b = mk_account(&mut store, "2000", AccountType::Asset);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/update-account");

        // Missing token → 401.
        let unauth = http
            .post(&url)
            .json(&serde_json::json!({ "expected_head_seq": head, "account_id": a, "name": "X" }))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Two fields changed → two AccountUpdated events, one batch.
        let ok = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head, "account_id": a,
                "name": "Cash", "description": "petty cash",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), reqwest::StatusCode::OK);
        let head = head_of(&ok.json().await.unwrap());

        // Renaming onto a number another account holds → 422, nothing appended.
        let dup = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head, "account_id": a, "account_number": "2000",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(dup.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

        // An account that isn't there → 422, not a silent success.
        let missing = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head, "account_id": "nope", "name": "X",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);

        // Stale head → 409.
        let stale = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": 0, "account_id": a, "name": "Other",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    }

    /// An edit that changes nothing must append nothing AND return the head the
    /// client already had. Returning 0, or erroring, would hand the client a bogus
    /// `expected_head_seq` and make its next write fail with an unexplainable 409.
    #[tokio::test]
    async fn an_edit_that_changes_nothing_appends_nothing_and_keeps_the_head() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let a = mk_account(&mut store, "1000", AccountType::Asset);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/update-account"))
            .bearer_auth(TOKEN)
            // `mk_account` names it "Acct 1000" — this is a no-op rename.
            .json(&serde_json::json!({
                "expected_head_seq": head, "account_id": a, "name": "Acct 1000",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        assert_eq!(
            head_of(&r.json().await.unwrap()),
            head,
            "no fields changed, so the head must not move"
        );
    }

    /// The `parent_id` round-trip trap, pinned.
    ///
    /// `Option<Option<String>>` needs a custom deserializer for `null` to mean
    /// "clear it" — and once that exists, a client serializing `None` as
    /// `"parent_id": null` would have every plain rename silently un-parent the
    /// account. The DTO is shared by both halves, so this asserts on the wire form
    /// the client actually produces.
    #[tokio::test]
    async fn an_omitted_parent_is_not_the_same_as_a_null_one() {
        let omitted = UpdateAccountRequest {
            expected_head_seq: 1,
            account_id: "a".into(),
            account_number: None,
            name: Some("Cash".into()),
            parent_id: None,
            description: None,
        };
        let wire = serde_json::to_value(&omitted).unwrap();
        assert!(
            wire.get("parent_id").is_none(),
            "a client leaving the parent alone must not send `parent_id` at all, \
             or the server reads it as `clear the parent`: {wire}"
        );

        // …and the three states survive a round trip through that wire form.
        let clear = serde_json::from_value::<UpdateAccountRequest>(serde_json::json!({
            "expected_head_seq": 1, "account_id": "a", "parent_id": null,
        }))
        .unwrap();
        assert_eq!(clear.parent_id, Some(None), "explicit null means clear it");

        let leave = serde_json::from_value::<UpdateAccountRequest>(serde_json::json!({
            "expected_head_seq": 1, "account_id": "a",
        }))
        .unwrap();
        assert_eq!(leave.parent_id, None, "absent means leave it alone");

        let set = serde_json::from_value::<UpdateAccountRequest>(serde_json::json!({
            "expected_head_seq": 1, "account_id": "a", "parent_id": "p",
        }))
        .unwrap();
        assert_eq!(set.parent_id, Some(Some("p".to_string())));
    }

    /// The seed must produce the account `find_or_create_uncategorized` looks for.
    ///
    /// This is the pairing the whole workflow rests on for group books. A member
    /// who cannot classify a bank transaction parks it in Uncategorized so anyone
    /// can recategorise it later from the journal — but on a replica no local
    /// append is allowed, so the "create" half of find-or-create cannot run. If the
    /// seed and the finder ever disagree about the name, hosted books get an import
    /// that fails on the first unrecognised transaction, and the escape hatch
    /// silently is not one.
    #[tokio::test]
    async fn the_seed_lays_down_the_account_uncategorised_imports_land_in() {
        use crate::commands::account_commands::find_or_create_uncategorized;

        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        let base = serve(SyncState::new(store, tokens())).await;

        let seeded = reqwest::Client::new()
            .post(format!("{base}/sync/commands/seed-default-accounts"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": 0 }))
            .send()
            .await
            .unwrap();
        assert_eq!(seeded.status(), reqwest::StatusCode::OK);

        // Replay the same chart into a local store and ask the finder for it. It
        // must FIND rather than create — the head must not move.
        let mut local = EventStore::in_memory().unwrap();
        init_schema(local.connection()).unwrap();
        crate::commands::account_commands::create_default_accounts(&mut local).unwrap();
        let before = local.latest_id().unwrap().unwrap_or(0);

        let id = find_or_create_uncategorized(&mut local).expect("uncategorized");
        assert!(!id.is_empty());
        assert_eq!(
            local.latest_id().unwrap().unwrap_or(0),
            before,
            "the seed already provides Uncategorized, so find_or_create must FIND \
             it — creating would be a local append, which a replica cannot do"
        );
    }

    /// The skew message, pinned.
    ///
    /// This is the failure a healthy deployment produces: pushing a new image does
    /// not restart running containers, so a group provisioned before a deploy keeps
    /// serving the old ledger. The client used to report that as
    /// "unexpected status 404", which names neither the cause nor the fix.
    #[tokio::test]
    async fn a_missing_command_endpoint_reads_as_a_stale_server_not_a_mystery() {
        use crate::sync::client::{SyncClient, SyncClientError};

        // A server with the sync routes but NOT the account command routes — i.e.
        // exactly what an instance one deploy behind looks like from out here.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::<()>::new()).await.unwrap();
        });

        let mut client = SyncClient::with_head(format!("http://{addr}"), TOKEN, 0);
        let err = client.seed_default_accounts().await.unwrap_err();

        let SyncClientError::ServerTooOld(what) = &err else {
            panic!("a 404 on a command endpoint must be diagnosed, got: {err:?}");
        };
        assert_eq!(
            what, "seed default accounts",
            "name the action, not the path"
        );

        let message = err.to_string();
        assert!(
            message.contains("older build"),
            "the message must say the server is behind: {message}"
        );
        assert!(
            message.contains("nothing was written"),
            "a user who just pressed Save needs to know it did not half-happen: \
             {message}"
        );
    }

    /// Wiring regression for both new routes, driven through the client half.
    #[tokio::test]
    async fn the_client_reaches_the_batch_account_commands() {
        use crate::commands::account_commands::DEFAULT_CHART;
        use crate::sync::client::SyncClient;

        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        let base = serve(SyncState::new(store, tokens())).await;
        let mut client = SyncClient::with_head(base, TOKEN, 0);

        let head = client.seed_default_accounts().await.unwrap();
        assert_eq!(head, DEFAULT_CHART.len() as i64);
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

#[cfg(test)]
mod ensure_path_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::domain::AccountType;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::router;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    async fn serve(store: EventStore) -> String {
        let state =
            SyncState::new(store, HashMap::from([(TOKEN.to_string(), "u1".to_string())]));
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        init_schema(s.connection()).unwrap();
        let _ = &mut s;
        s
    }

    async fn ensure(base: &str, head: i64, path: &str) -> (u16, serde_json::Value) {
        let resp = reqwest::Client::new()
            .post(format!("{base}/sync/commands/ensure-account-path"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "path": path,
                "account_type": "expense",
            }))
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body)),
        )
    }

    /// Every account this ledger holds, as (name, number, parent id), read from the
    /// event stream rather than `/sync/accounts` — the chart DTO carries no parent,
    /// and parentage is exactly what these tests are about. It is also what a replica
    /// reads, so this is the view the desktop will build.
    async fn accounts(base: &str) -> Vec<(String, String, Option<String>)> {
        let resp = reqwest::Client::new()
            .get(format!("{base}/sync/events?since=0"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let v: serde_json::Value = resp.json().await.unwrap();
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| {
                // Internally tagged, snake_case: {"type":"account_created", ...}.
                let created = &e["event"];
                (created["type"] == "account_created").then(|| {
                    (
                        created["name"].as_str().unwrap_or_default().to_string(),
                        created["account_number"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        created["parent_id"].as_str().map(|s| s.to_string()),
                    )
                })
            })
            .collect()
    }

    /// The whole point: a path of accounts none of which exist becomes one append,
    /// parented correctly, and the caller is told which account it asked for.
    #[tokio::test]
    async fn a_missing_path_is_created_whole_and_names_its_leaf() {
        let base = serve(store()).await;

        let (status, body) = ensure(&base, 0, "Expenses:Software:SaaS").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["created"], 3, "{body}");
        let leaf = body["account_id"].as_str().unwrap().to_string();
        assert!(!leaf.is_empty());

        let rows = accounts(&base).await;
        assert_eq!(rows.len(), 3, "{rows:?}");
        let names: Vec<&str> = rows.iter().map(|(n, _, _)| n.as_str()).collect();
        for expected in ["Expenses", "Software", "SaaS"] {
            assert!(names.contains(&expected), "{rows:?}");
        }
        // Parented as a path, not created flat: only the root has no parent.
        assert_eq!(
            rows.iter().filter(|(_, _, parent)| parent.is_none()).count(),
            1,
            "{rows:?}"
        );
    }

    /// Asked again, it creates nothing and returns the same account. The caller
    /// wanted the account, not the work.
    #[tokio::test]
    async fn an_existing_path_is_returned_without_appending_anything() {
        let base = serve(store()).await;
        let (_, first) = ensure(&base, 0, "Expenses:Software").await;
        let head = first["head"].as_i64().unwrap();
        let id = first["account_id"].as_str().unwrap().to_string();

        let (status, again) = ensure(&base, head, "Expenses:Software").await;
        assert_eq!(status, 200, "{again}");
        assert_eq!(again["created"], 0, "{again}");
        assert_eq!(again["account_id"].as_str().unwrap(), id);
        assert_eq!(again["head"].as_i64().unwrap(), head, "nothing was appended");
    }

    /// Only the missing part is created when the path is half there — and the new
    /// account hangs off the one that already existed.
    #[tokio::test]
    async fn only_the_missing_segments_are_created() {
        let mut s = store();
        let parent = AccountCommands::new(&mut s, "seed".to_string())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Expense,
                account_number: "5000".to_string(),
                name: "Expenses".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            })
            .unwrap();
        let parent_id = match &parent.event {
            crate::events::types::Event::AccountCreated { account_id, .. } => account_id.clone(),
            _ => unreachable!(),
        };
        let head = parent.id;
        let base = serve(s).await;

        let (status, body) = ensure(&base, head, "Expenses:Software").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["created"], 1, "{body}");

        let rows = accounts(&base).await;
        let created = rows.iter().find(|(n, _, _)| n == "Software").unwrap();
        assert_eq!(
            created.2.as_deref(),
            Some(parent_id.as_str()),
            "the new account hangs off the one that was already there: {rows:?}"
        );
        assert_ne!(created.1, "5000", "and took a free number: {rows:?}");
    }

    /// Two accounts may share a name under different parents, so matching is by the
    /// whole path. Matching by name would hand back the wrong account — silently, and
    /// with entries then posted against it.
    #[tokio::test]
    async fn a_name_that_exists_under_another_parent_is_not_the_same_account() {
        let base = serve(store()).await;
        let (_, a) = ensure(&base, 0, "Expenses:Taxes").await;
        let head = a["head"].as_i64().unwrap();
        let first = a["account_id"].as_str().unwrap().to_string();

        let (status, b) = ensure(&base, head, "Expenses:Payroll:Taxes").await;
        assert_eq!(status, 200, "{b}");
        assert_eq!(b["created"], 2, "Payroll and its own Taxes: {b}");
        assert_ne!(b["account_id"].as_str().unwrap(), first);
    }

    #[tokio::test]
    async fn an_empty_path_is_refused_and_a_stale_head_is_a_conflict() {
        let base = serve(store()).await;
        let (status, body) = ensure(&base, 0, "   :  ").await;
        assert_eq!(status, 422, "{body}");

        let (_, made) = ensure(&base, 0, "Expenses").await;
        let stale = made["head"].as_i64().unwrap() - 1;
        let (status, body) = ensure(&base, stale, "Expenses:Other").await;
        assert_eq!(status, 409, "{body}");
    }
}
