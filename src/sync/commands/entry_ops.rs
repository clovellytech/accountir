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
//! Wired here: `void-entry` and `unvoid-entry`, which emit a single event each and
//! so fit the single-event `SubmitResponse`/`outcome_to_response` shape — and
//! `reassign-lines`, which does not: recategorising a bank import is dozens of
//! lines at once, so it follows `post-entries` instead and reports per-line skips
//! rather than failing the lot. See [`ReassignLinesRequest`].

use crate::commands::entry_commands::{
    build_reassign_line_in_txn, build_unvoid_entry_in_txn, build_void_entry_in_txn,
    EntryCommandError, PostEntryStep, ReassignLineCommand, ReassignLineStep, UnvoidEntryCommand,
    VoidEntryCommand,
};
use crate::events::types::Event;
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
        .route("/sync/commands/reassign-lines", post(submit_reassign_lines))
        .route("/sync/commands/void-entries", post(submit_void_entries))
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

    fn head_of(v: &serde_json::Value) -> i64 {
        v["head"].as_i64().unwrap()
    }

    /// Seed an account directly (local handler), returning its id.
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

    /// Seed two accounts + a balanced posted entry (local handlers). Returns the
    /// posted entry's id.
    pub(super) fn seed_posted_entry(store: &mut EventStore) -> String {
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

// ---------------------------------------------------------------------------
// Reassigning lines
// ---------------------------------------------------------------------------

/// One line to move, and where to.
#[derive(Serialize, Deserialize, Clone)]
pub struct LineAssignment {
    pub entry_id: String,
    pub line_id: String,
    pub new_account_id: String,
}

/// Move many posted lines to different accounts in one call.
///
/// # Why this exists
///
/// A bank import posts everything it cannot categorise to Uncategorized, on the
/// deliberate principle that a posted entry beats a staged one. Recategorising is
/// therefore not an edge case — it is the second half of every import, and it is
/// done in bulk. On group-hosted books it was refused outright ("Moving posted
/// lines isn't something the group server can do yet"), which left a member who
/// had imported a month of transactions with no way to file any of them.
///
/// # Why a batch, and why it is not all-or-nothing
///
/// Same reasoning as [`super::entries`]: the desktop's sync engine holds one
/// pending write, so forty moves would be forty round trips. And the rejections
/// here are individually meaningful and expected — a line somebody else already
/// moved, an account deactivated since the page was loaded — so failing all forty
/// because of one would make the common case unusable. Every line that passes its
/// fences moves, in ONE transaction; every line that does not is reported by index.
#[derive(Serialize, Deserialize)]
pub struct ReassignLinesRequest {
    pub expected_head_seq: i64,
    pub assignments: Vec<LineAssignment>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SkippedAssignment {
    /// Index into the submitted `assignments`, so the caller can leave exactly
    /// that row selected and clear the rest.
    pub index: usize,
    pub reason: String,
}

#[derive(Serialize, Deserialize)]
pub struct ReassignLinesResponse {
    pub head: i64,
    pub moved: usize,
    pub skipped: Vec<SkippedAssignment>,
}

/// The cap on one batch, matching `post-entries`: a bound rather than a limit
/// anyone should hit, so an unbounded request cannot hold the group's write lock
/// for as long as the caller likes.
const MAX_ASSIGNMENTS: usize = 1000;

async fn submit_reassign_lines(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<ReassignLinesRequest>,
) -> Result<Json<ReassignLinesResponse>, ApiError> {
    if req.assignments.is_empty() {
        return Err(ApiError::bad_request("no lines to reassign"));
    }
    if req.assignments.len() > MAX_ASSIGNMENTS {
        return Err(ApiError::bad_request(
            "too many lines in one batch; split it",
        ));
    }

    let expected = req.expected_head_seq;
    let assignments = req.assignments;
    let skips: std::cell::RefCell<Vec<SkippedAssignment>> = std::cell::RefCell::new(Vec::new());

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            |tx| {
                let mut events: Vec<Event> = Vec::new();
                // Lines already moved by an earlier assignment in THIS batch.
                //
                // The fences read the projection, and nothing in this batch is
                // projected until the whole closure has run — so two assignments
                // naming the same line would both read its original account, both
                // append, and leave the log holding two moves whose `old_account_id`
                // disagree with each other. The second is nonsense however it is
                // resolved, so it is refused here rather than reconciled later.
                //
                // The same blind spot cost a 368-entry import its entire batch on
                // `post-entries`; this is that lesson applied before it can happen
                // twice.
                let mut moved: std::collections::HashSet<&str> = std::collections::HashSet::new();
                for (index, a) in assignments.iter().enumerate() {
                    if !moved.insert(a.line_id.as_str()) {
                        skips.borrow_mut().push(SkippedAssignment {
                            index,
                            reason: "This line was already moved earlier in the same request"
                                .to_string(),
                        });
                        continue;
                    }
                    let cmd = ReassignLineCommand {
                        entry_id: a.entry_id.clone(),
                        line_id: a.line_id.clone(),
                        new_account_id: a.new_account_id.clone(),
                    };
                    match build_reassign_line_in_txn(tx, &cmd)? {
                        ReassignLineStep::Append(event) => events.push(event),
                        ReassignLineStep::Reject(e) => skips.borrow_mut().push(SkippedAssignment {
                            index,
                            reason: e.to_string(),
                        }),
                    }
                }
                Ok(Verdict::<Vec<_>, EntryCommandError>::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                ))
            },
            project,
        )
        .map_err(ApiError::store)?;

    // `moved` is the number of events actually appended, not
    // `assignments.len() - skipped.len()`. They should agree, and deriving it from
    // the append is what makes it true rather than hoped: `post-entries` shipped a
    // version that computed the count separately, reported `posted: 0` for a batch
    // that had appended, and left its caller re-importing the same rows forever.
    use crate::store::event_store::CheckedOutcome;
    let (head, moved) = match outcome {
        CheckedOutcome::Appended(stored) => {
            (stored.last().map_or(expected, |e| e.id), stored.len())
        }
        CheckedOutcome::HeadMismatch { actual, .. } => return Err(ApiError::conflict(actual)),
        CheckedOutcome::Rejected(e) => return Err(ApiError::domain(e)),
    };
    Ok(Json(ReassignLinesResponse {
        head,
        moved,
        skipped: skips.into_inner(),
    }))
}

#[cfg(test)]
mod reassign_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::AccountType;
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::router;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

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

    /// A bank import as it actually lands: the bank side on a real account, the
    /// other side parked in Uncategorized awaiting somebody to file it.
    struct Imported {
        base: String,
        /// (entry_id, line_id) of each Uncategorized line, in order.
        parked: Vec<(String, String)>,
        groceries: String,
        fuel: String,
    }

    async fn imported(count: usize) -> Imported {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let bank = mk_account(&mut store, "1000", AccountType::Asset);
        let uncat = mk_account(&mut store, "9999", AccountType::Expense);
        let groceries = mk_account(&mut store, "5001", AccountType::Expense);
        let fuel = mk_account(&mut store, "5002", AccountType::Expense);

        let mut parked = Vec::new();
        for i in 0..count {
            let stored = EntryCommands::new(&mut store, "seed".to_string())
                .post_entry(PostEntryCommand {
                    date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                    memo: format!("txn {i}"),
                    lines: vec![
                        EntryLine::debit(&uncat, 1000, "USD"),
                        EntryLine::credit(&bank, 1000, "USD"),
                    ],
                    reference: Some(format!("plaid-{i}")),
                    source: None,
                })
                .unwrap();
            let (entry_id, line_id) = match &stored.event {
                Event::JournalEntryPosted {
                    entry_id, lines, ..
                } => (
                    entry_id.clone(),
                    lines
                        .iter()
                        .find(|l| l.account_id == uncat)
                        .expect("the uncategorised line")
                        .line_id
                        .clone(),
                ),
                _ => unreachable!(),
            };
            parked.push((entry_id, line_id));
        }

        let state = SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "alice@example.com".to_string())]),
        );
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        Imported {
            base: format!("http://{addr}"),
            parked,
            groceries,
            fuel,
        }
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

    async fn reassign(base: &str, body: serde_json::Value) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{base}/sync/commands/reassign-lines"))
            .bearer_auth(TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// The thing that was blocked: a member on hosted books files a batch of
    /// imported transactions. Before this endpoint the desktop refused outright —
    /// "Moving posted lines isn't something the group server can do yet" — which
    /// left a month of bank transactions sitting in Uncategorized with no way out.
    #[tokio::test]
    async fn a_batch_of_imported_lines_can_be_filed_on_hosted_books() {
        let fx = imported(3).await;
        let assignments: Vec<_> = fx
            .parked
            .iter()
            .enumerate()
            .map(|(i, (entry_id, line_id))| {
                serde_json::json!({
                    "entry_id": entry_id,
                    "line_id": line_id,
                    "new_account_id": if i == 0 { &fx.fuel } else { &fx.groceries },
                })
            })
            .collect();

        let r = reassign(
            &fx.base,
            serde_json::json!({
                "expected_head_seq": head_of(&fx.base).await,
                "assignments": assignments,
            }),
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["moved"], 3, "{v}");
        assert!(v["skipped"].as_array().unwrap().is_empty(), "{v}");

        // The head must match what actually appended, or the caller's next write
        // gets a 409 it cannot explain.
        assert_eq!(v["head"].as_i64().unwrap(), head_of(&fx.base).await);
    }

    /// One bad line must not cost the other two. These rejections are expected and
    /// individually meaningful — the same reasoning `post-entries` is built on.
    #[tokio::test]
    async fn one_unfileable_line_is_skipped_and_the_rest_still_move() {
        let fx = imported(3).await;
        let (good_entry, good_line) = fx.parked[0].clone();
        let (other_entry, other_line) = fx.parked[1].clone();

        let r = reassign(
            &fx.base,
            serde_json::json!({
                "expected_head_seq": head_of(&fx.base).await,
                "assignments": [
                    {"entry_id": good_entry, "line_id": good_line, "new_account_id": fx.groceries},
                    // No such account.
                    {"entry_id": other_entry, "line_id": other_line, "new_account_id": "no-such-account"},
                    // No such line.
                    {"entry_id": fx.parked[2].0, "line_id": "no-such-line", "new_account_id": fx.fuel},
                ],
            }),
        )
        .await;
        assert_eq!(r.status(), reqwest::StatusCode::OK);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["moved"], 1, "{v}");
        let skipped = v["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 2, "{v}");
        assert_eq!(skipped[0]["index"], 1);
        assert_eq!(skipped[1]["index"], 2);
    }

    /// The blind spot that cost a 368-entry import its whole batch on
    /// `post-entries`, in its reassignment form: the fences read the projection,
    /// and nothing in this batch is projected until the closure has finished. Two
    /// assignments naming the same line would both read its ORIGINAL account and
    /// append two moves whose `old_account_id` disagree.
    #[tokio::test]
    async fn the_same_line_named_twice_moves_once() {
        let fx = imported(1).await;
        let (entry_id, line_id) = fx.parked[0].clone();

        let r = reassign(
            &fx.base,
            serde_json::json!({
                "expected_head_seq": head_of(&fx.base).await,
                "assignments": [
                    {"entry_id": entry_id, "line_id": line_id, "new_account_id": fx.groceries},
                    {"entry_id": entry_id, "line_id": line_id, "new_account_id": fx.fuel},
                ],
            }),
        )
        .await;
        assert_eq!(
            r.status(),
            reqwest::StatusCode::OK,
            "naming a line twice must not fail the batch"
        );
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["moved"], 1, "{v}");
        assert_eq!(v["skipped"][0]["index"], 1, "{v}");

        // Exactly one move reached the log.
        let events: serde_json::Value = reqwest::Client::new()
            .get(format!("{}/sync/events?since=0&limit=50", fx.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            events
                .to_string()
                .matches("journal_line_reassigned")
                .count(),
            1,
            "the line moved twice: {events}"
        );
    }

    /// Re-sending a batch that already landed is safe: each line reads as "same as
    /// current account" and is skipped. That is what makes the client's 409 retry
    /// loop sound.
    #[tokio::test]
    async fn re_sending_a_landed_batch_moves_nothing() {
        let fx = imported(2).await;
        let body = |head: i64| {
            serde_json::json!({
                "expected_head_seq": head,
                "assignments": fx.parked.iter().map(|(e, l)| serde_json::json!({
                    "entry_id": e, "line_id": l, "new_account_id": fx.groceries,
                })).collect::<Vec<_>>(),
            })
        };
        let first: serde_json::Value = reassign(&fx.base, body(head_of(&fx.base).await))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(first["moved"], 2);

        let again: serde_json::Value = reassign(&fx.base, body(head_of(&fx.base).await))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(again["moved"], 0, "{again}");
        assert_eq!(again["skipped"].as_array().unwrap().len(), 2, "{again}");
    }

    #[tokio::test]
    async fn an_empty_batch_is_a_bad_request_and_auth_is_required() {
        let fx = imported(1).await;
        let (entry_id, line_id) = fx.parked[0].clone();

        assert_eq!(
            reqwest::Client::new()
                .post(format!("{}/sync/commands/reassign-lines", fx.base))
                .json(&serde_json::json!({"expected_head_seq": 0, "assignments": []}))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "auth is checked before the body"
        );
        assert_eq!(
            reassign(
                &fx.base,
                serde_json::json!({"expected_head_seq": 0, "assignments": []})
            )
            .await
            .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
        // A stale head is a 409 the client resolves by refetching, not a failure.
        assert_eq!(
            reassign(
                &fx.base,
                serde_json::json!({
                    "expected_head_seq": 0,
                    "assignments": [{
                        "entry_id": entry_id, "line_id": line_id,
                        "new_account_id": fx.groceries,
                    }],
                })
            )
            .await
            .status(),
            reqwest::StatusCode::CONFLICT
        );
    }
}

// ---------------------------------------------------------------------------
// Voiding many entries
// ---------------------------------------------------------------------------

/// Void many entries in one call.
///
/// # Why this exists
///
/// The desktop's sync engine holds one pending write at a time, so voiding a
/// selection of entries on hosted books was refused outright: "each void has to be
/// sent and confirmed on its own. Select one entry and void it, then the next."
/// Correct about the engine and useless to anyone who had just imported a month of
/// transactions and wanted a bad batch gone.
///
/// Firing N single voids instead would have been worse than the refusal — a
/// partial failure halfway through leaves a selection nobody can reconstruct, some
/// voided and some not, with no record of which.
///
/// # Why it is not all-or-nothing
///
/// Same reasoning as [`ReassignLinesRequest`]: the rejections are individually
/// meaningful and expected. An entry somebody else voided a second ago, or one
/// inside a period that has since closed, should not cost the other thirty-nine
/// their void. Every entry that passes its fences is voided, in ONE transaction;
/// every entry that does not is reported by index.
#[derive(Serialize, Deserialize)]
pub struct VoidEntriesRequest {
    pub expected_head_seq: i64,
    pub entry_ids: Vec<String>,
    /// One reason for the batch. Voiding a selection is one decision, and asking
    /// for a reason per entry would produce forty copies of the same sentence.
    pub reason: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SkippedVoid {
    /// Index into the submitted `entry_ids`, so the caller can leave exactly that
    /// row selected and clear the rest.
    pub index: usize,
    pub reason: String,
}

#[derive(Serialize, Deserialize)]
pub struct VoidEntriesResponse {
    pub head: i64,
    pub voided: usize,
    pub skipped: Vec<SkippedVoid>,
}

/// The cap on one batch, matching its neighbours: a bound rather than a limit
/// anyone should hit, so an unbounded request cannot hold the group's write lock
/// for as long as the caller likes. The client splits below it.
const MAX_VOIDS: usize = 1000;

async fn submit_void_entries(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<VoidEntriesRequest>,
) -> Result<Json<VoidEntriesResponse>, ApiError> {
    if req.entry_ids.is_empty() {
        return Err(ApiError::bad_request("no entries to void"));
    }
    if req.entry_ids.len() > MAX_VOIDS {
        return Err(ApiError::bad_request(
            "too many entries in one batch; split it",
        ));
    }
    if req.reason.trim().is_empty() {
        // Required for the same purpose a single void's is: six months on, the
        // only person who knew why has left.
        return Err(ApiError::bad_request("a reason is required"));
    }

    let expected = req.expected_head_seq;
    let entry_ids = req.entry_ids;
    let reason = req.reason;
    let skips: std::cell::RefCell<Vec<SkippedVoid>> = std::cell::RefCell::new(Vec::new());

    let mut store = st.store.lock().unwrap();
    let outcome = store
        .append_checked_many(
            expected,
            |tx| {
                let mut events: Vec<Event> = Vec::new();
                // Entries already voided by an earlier position in THIS batch.
                //
                // The fence reads the projection, and nothing in this batch is
                // projected until the whole closure has run — so the same id twice
                // would pass `check_entry_not_voided_in_txn` twice and append two
                // voids for one entry. A selection can contain a duplicate for
                // ordinary reasons, so this is refused here rather than left to be
                // puzzled over in the log.
                //
                // The same blind spot cost a 368-entry import its entire batch on
                // `post-entries`.
                let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
                for (index, entry_id) in entry_ids.iter().enumerate() {
                    if !seen.insert(entry_id.as_str()) {
                        skips.borrow_mut().push(SkippedVoid {
                            index,
                            reason: "This entry was already voided earlier in the same request"
                                .to_string(),
                        });
                        continue;
                    }
                    let cmd = VoidEntryCommand {
                        entry_id: entry_id.clone(),
                        reason: reason.clone(),
                    };
                    match build_void_entry_in_txn(tx, &cmd)? {
                        PostEntryStep::Append(event) => events.push(event),
                        PostEntryStep::Reject(e) => skips.borrow_mut().push(SkippedVoid {
                            index,
                            reason: e.to_string(),
                        }),
                    }
                }
                Ok(Verdict::<Vec<_>, EntryCommandError>::Append(
                    events.into_iter().map(|e| stamp(e, &actor)).collect(),
                ))
            },
            project,
        )
        .map_err(ApiError::store)?;

    // Derived from the append, not from `entry_ids.len() - skipped.len()`. They
    // should agree, and taking it from what was actually appended is what makes it
    // true rather than hoped — see the same note on `reassign-lines`.
    use crate::store::event_store::CheckedOutcome;
    let (head, voided) = match outcome {
        CheckedOutcome::Appended(stored) => {
            (stored.last().map_or(expected, |e| e.id), stored.len())
        }
        CheckedOutcome::HeadMismatch { actual, .. } => return Err(ApiError::conflict(actual)),
        CheckedOutcome::Rejected(e) => return Err(ApiError::domain(e)),
    };

    Ok(Json(VoidEntriesResponse {
        head,
        voided,
        skipped: skips.into_inner(),
    }))
}

/// Voiding a selection on group-hosted books.
///
/// Refused outright before this — "each void has to be sent and confirmed on its
/// own" — because the desktop's sync engine holds one pending write and firing N
/// single voids would leave a partial failure nobody could reconstruct. Correct
/// about the engine, and useless to somebody looking at a bad import they wanted
/// gone.
#[cfg(test)]
mod batch_void {
    use super::tests::{mk_account, seed_posted_entry, serve, tokens, TOKEN};
    use super::*;
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::AccountType;
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::client::SyncClient;

    /// `count` posted entries, plus the accounts they use.
    fn seed_many(store: &mut EventStore, count: usize) -> Vec<String> {
        let cash = mk_account(store, "1000", AccountType::Asset);
        let expense = mk_account(store, "5000", AccountType::Expense);
        (0..count)
            .map(|i| {
                let stored = EntryCommands::new(store, "seed".to_string())
                    .post_entry(PostEntryCommand {
                        date: chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                        memo: format!("seed {i}"),
                        lines: vec![
                            EntryLine::debit(&expense, 100 + i as i64, "USD"),
                            EntryLine::credit(&cash, 100 + i as i64, "USD"),
                        ],
                        reference: Some(format!("seed-{i}")),
                        source: None,
                    })
                    .unwrap();
                match stored.event {
                    Event::JournalEntryPosted { entry_id, .. } => entry_id,
                    _ => unreachable!(),
                }
            })
            .collect()
    }

    /// The thing the desktop could not do.
    #[tokio::test]
    async fn a_selection_is_voided_in_one_call() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let ids = seed_many(&mut store, 40);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        let r = client
            .void_entries(ids.clone(), "bulk void from UI")
            .await
            .expect("voiding a selection must not be refused");

        assert_eq!(r.voided, 40, "skipped: {:?}", r.skipped);
        assert!(r.skipped.is_empty());
        assert_eq!(r.head, head + 40, "one event per void");
    }

    /// One entry already voided does not cost the rest theirs.
    ///
    /// The reason this is a batch of skips rather than a batch that fails: a
    /// colleague voiding one row a second before you press the button is ordinary,
    /// and it must not undo the other thirty-nine.
    #[tokio::test]
    async fn an_entry_that_cannot_be_voided_is_skipped_by_index() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let ids = seed_many(&mut store, 5);
        // Somebody got to the third one first.
        EntryCommands::new(&mut store, "colleague".to_string())
            .void_entry(crate::commands::entry_commands::VoidEntryCommand {
                entry_id: ids[2].clone(),
                reason: "already dealt with".to_string(),
            })
            .unwrap();
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        let r = client.void_entries(ids, "bulk void").await.unwrap();

        assert_eq!(r.voided, 4);
        assert_eq!(r.skipped.len(), 1, "{:?}", r.skipped);
        assert_eq!(
            r.skipped[0].index, 2,
            "the caller uses this to leave exactly that row selected: {:?}",
            r.skipped
        );
    }

    /// The same id twice in one selection.
    ///
    /// The fence reads the projection, which is not updated until the whole
    /// closure has run — so without an in-batch guard both positions pass and the
    /// log gets two voids for one entry. This is the failure that cost a 368-entry
    /// import everything on `post-entries`, applied before it can happen again.
    #[tokio::test]
    async fn the_same_entry_twice_in_one_selection_is_voided_once() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let ids = seed_many(&mut store, 3);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        let mut with_dup = ids.clone();
        with_dup.push(ids[1].clone());

        let r = client.void_entries(with_dup, "bulk void").await.unwrap();

        assert_eq!(r.voided, 3, "an entry was voided twice: {:?}", r.skipped);
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(r.skipped[0].index, 3);
    }

    /// A selection larger than one request still goes through.
    #[tokio::test]
    async fn a_selection_larger_than_one_batch_is_split() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let ids = seed_many(&mut store, 1_200);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;
        let mut client = SyncClient::with_head(base, TOKEN, head);

        let r = client.void_entries(ids, "bulk void").await.unwrap();
        assert_eq!(r.voided, 1_200, "skipped: {:?}", r.skipped);
    }

    /// A reason is required, as it is for a single void.
    #[tokio::test]
    async fn a_batch_void_without_a_reason_is_refused() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let entry_id = seed_posted_entry(&mut store);
        let head = store.latest_id().unwrap().unwrap_or(0);
        let base = serve(SyncState::new(store, tokens())).await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/void-entries"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head,
                "entry_ids": [entry_id],
                "reason": "  ",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    }
}
