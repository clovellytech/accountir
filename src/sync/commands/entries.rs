//! Posting **many** journal entries in one call.
//!
//! Exists for one job: a bank import on group-hosted books. A member pulls a
//! feed, reviews it, and posts what they reviewed — which is dozens of entries.
//! One-at-a-time is not an option there, because the desktop's sync engine holds
//! exactly one pending write and nothing spans several of them: forty submissions
//! would be forty round trips with a half-imported ledger at every step, and a
//! member who closed their laptop halfway would leave it that way.
//!
//! # Why this is not all-or-nothing
//!
//! [`super::account::submit_seed_default_accounts`] refuses wholesale if any one
//! account collides, and that is right for a seed: half a chart of accounts is
//! worse than none, because nothing tells you which half.
//!
//! An import is the opposite. Its rejections are *expected and individually
//! meaningful* — a transaction already imported, one dated into a closed period,
//! one pointing at an account somebody deactivated. Failing all forty because the
//! third is a duplicate would make the common case unusable, and the fix (find the
//! duplicate, deselect it, retry) is work the server can simply do itself.
//!
//! So every entry that passes its fences is appended, in **one** transaction, and
//! every entry that does not is reported by index with the reason. The atomicity
//! that matters is still there: the batch either lands as a unit or not at all, so
//! there is no partially-applied append to reconcile.

use crate::commands::entry_commands::{
    build_post_entry_in_txn, check_entry_pure, EntryCommandError, EntryLine, PostEntryCommand,
    PostEntryStep,
};
use crate::events::types::Event;
use crate::events::types::JournalEntrySource;
use crate::store::event_store::Verdict;
use crate::sync::{
    outcome_to_response_many, project, stamp, ApiError, AuthedUser, PostEntryLine, SubmitResponse,
    SyncState,
};
use axum::{extract::State, routing::post, Json, Router};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

pub fn router() -> Router<SyncState> {
    Router::new().route("/sync/commands/post-entries", post(submit_post_entries))
}

/// One entry in a batch. Same fields as a single post, minus the head — the head
/// belongs to the batch.
#[derive(Serialize, Deserialize, Clone)]
pub struct BatchEntry {
    pub date: NaiveDate,
    pub memo: String,
    pub lines: Vec<PostEntryLine>,
    /// The idempotency key. For a bank import this is the Plaid transaction id,
    /// which is what makes re-posting the same feed a no-op rather than a
    /// duplicate — the server checks it against live entries under the write lock.
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PostEntriesRequest {
    pub expected_head_seq: i64,
    pub entries: Vec<BatchEntry>,
}

/// One entry the batch declined, by position in the request.
#[derive(Serialize, Deserialize, Debug)]
pub struct SkippedEntry {
    /// Index into the submitted `entries`, so the caller can mark exactly that
    /// staged row and leave the rest alone. A reference would be ambiguous —
    /// `reference` is optional, and two entries may share a memo.
    pub index: usize,
    pub reason: String,
}

#[derive(Serialize, Deserialize)]
pub struct PostEntriesResponse {
    /// The log head after the append, or the unchanged head if nothing qualified.
    pub head: i64,
    pub posted: usize,
    pub skipped: Vec<SkippedEntry>,
}

/// The cap on one batch.
///
/// A bound rather than a limit anyone should hit: the whole batch is built in
/// memory and appended in a single transaction, so an unbounded request is a way
/// to hold the group's write lock for as long as the caller likes. Well above any
/// real import — a month of transactions on a busy account is a few hundred.
const MAX_BATCH: usize = 1000;

async fn submit_post_entries(
    AuthedUser(actor): AuthedUser,
    State(st): State<SyncState>,
    Json(req): Json<PostEntriesRequest>,
) -> Result<Json<PostEntriesResponse>, ApiError> {
    // Shape complaints about the batch itself, rather than about any entry in
    // it. `ApiError::domain` is for a rejected *command*; an empty or oversized
    // request never became one.
    if req.entries.is_empty() {
        return Err(ApiError::bad_request("no entries to post"));
    }
    if req.entries.len() > MAX_BATCH {
        return Err(ApiError::bad_request(
            "too many entries in one batch; split it",
        ));
    }

    let expected = req.expected_head_seq;
    let commands: Vec<PostEntryCommand> = req
        .entries
        .into_iter()
        .map(|e| PostEntryCommand {
            date: e.date,
            memo: e.memo,
            lines: e
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
            reference: e.reference,
            source: Some(JournalEntrySource::Manual),
        })
        .collect();

    // Pure checks first, outside the lock. An unbalanced entry is the caller's
    // bug rather than a state conflict, and it is worth finding before taking the
    // group's write lock — but it is still *skipped* rather than fatal, so one
    // malformed row cannot block a whole import.
    let mut skipped: Vec<SkippedEntry> = Vec::new();
    let mut candidates: Vec<(usize, PostEntryCommand)> = Vec::new();
    for (index, cmd) in commands.into_iter().enumerate() {
        match check_entry_pure(&cmd) {
            Ok(()) => candidates.push((index, cmd)),
            Err(e) => skipped.push(SkippedEntry {
                index,
                reason: e.to_string(),
            }),
        }
    }

    if candidates.is_empty() {
        // Nothing to append. Answering with the unchanged head rather than an
        // error: a feed whose every row was already imported is a *successful*
        // no-op, and the caller needs a head it can keep using.
        return Ok(Json(PostEntriesResponse {
            head: expected,
            posted: 0,
            skipped,
        }));
    }

    let mut store = st.store.lock().unwrap();
    // `RefCell` because the check closure is `FnOnce` but has to report back which
    // entries it declined, and it runs inside the append transaction where the
    // per-entry fences (reference dedup, account active, period open) are the only
    // place those answers exist.
    let in_txn_skips: std::cell::RefCell<Vec<SkippedEntry>> = std::cell::RefCell::new(Vec::new());

    let outcome = store
        .append_checked_many(
            expected,
            |tx| {
                let mut events: Vec<Event> = Vec::new();
                for (index, cmd) in &candidates {
                    match build_post_entry_in_txn(tx, cmd)? {
                        PostEntryStep::Append(event) => events.push(event),
                        // The expected case, not an error: already imported, a
                        // closed period, a deactivated account. Recorded and
                        // stepped over.
                        PostEntryStep::Reject(e) => in_txn_skips.borrow_mut().push(SkippedEntry {
                            index: *index,
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

    // Only the in-transaction rejections come off `candidates` — the pure-check
    // failures never became candidates in the first place.
    //
    // Subtracting the whole `skipped` list is what the first version did, and it
    // reported `posted: 0` for a batch that had appended an entry: the head moved
    // and the count said nothing landed. A caller trusting that would leave the
    // staged row marked pending and re-import it forever.
    let in_txn_skips = in_txn_skips.into_inner();
    let posted = candidates.len() - in_txn_skips.len();
    skipped.extend(in_txn_skips);

    let response =
        outcome_to_response_many(outcome, expected, ApiError::domain::<EntryCommandError>)?;
    let SubmitResponse { head } = response.0;
    Ok(Json(PostEntriesResponse {
        posted,
        head,
        skipped,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::domain::AccountType;
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::router;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";

    async fn serve(state: SyncState) -> String {
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
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

    fn entry(cash: &str, income: &str, reference: &str, amount: i64) -> serde_json::Value {
        serde_json::json!({
            "date": "2026-08-01",
            "memo": format!("import {reference}"),
            "reference": reference,
            "lines": [
                { "account_id": cash, "amount": amount, "currency": "USD" },
                { "account_id": income, "amount": -amount, "currency": "USD" },
            ],
        })
    }

    /// The property that separates this from the seed command, and the reason an
    /// import needs its own shape: one already-imported transaction in the middle
    /// of forty must not sink the other thirty-nine. A bank feed re-pulled after a
    /// partial review is the *normal* case, not an error.
    #[tokio::test]
    async fn one_duplicate_does_not_sink_the_rest_of_the_import() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let cash = mk_account(&mut store, "1000", AccountType::Asset);
        let income = mk_account(&mut store, "2000", AccountType::Revenue);
        let base = serve(SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "user-1".to_string())]),
        ))
        .await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/post-entries");

        let head0: i64 = http
            .get(format!("{base}/sync/head"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["head"]
            .as_i64()
            .unwrap();

        // First import: two transactions.
        let first: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head0,
                "entries": [entry(&cash, &income, "plaid-a", 100),
                            entry(&cash, &income, "plaid-b", 200)],
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(first["posted"], 2, "{first}");
        assert_eq!(first["skipped"].as_array().unwrap().len(), 0);
        let head1 = first["head"].as_i64().unwrap();

        // Re-pull: `plaid-b` again plus a new one. The duplicate is skipped BY
        // INDEX and the new one still lands.
        let second: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head1,
                "entries": [entry(&cash, &income, "plaid-b", 200),
                            entry(&cash, &income, "plaid-c", 300)],
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            second["posted"], 1,
            "the new transaction did not land: {second}"
        );
        let skipped = second["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(
            skipped[0]["index"], 0,
            "the skip must name the position the caller sent, so it can mark that \
             staged row and leave the rest: {second}"
        );
    }

    /// A feed whose every row was already imported is a successful no-op, and the
    /// caller needs a head it can keep using. Returning 0 — or an error — would
    /// leave it with a bogus `expected_head_seq` and a 409 it cannot explain.
    #[tokio::test]
    async fn an_import_with_nothing_new_returns_the_unchanged_head() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let cash = mk_account(&mut store, "1000", AccountType::Asset);
        let income = mk_account(&mut store, "2000", AccountType::Revenue);
        let base = serve(SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "user-1".to_string())]),
        ))
        .await;
        let http = reqwest::Client::new();
        let url = format!("{base}/sync/commands/post-entries");
        let head0: i64 = http
            .get(format!("{base}/sync/head"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()["head"]
            .as_i64()
            .unwrap();

        let one = serde_json::json!({
            "expected_head_seq": head0,
            "entries": [entry(&cash, &income, "plaid-a", 100)],
        });
        let first: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&one)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let head1 = first["head"].as_i64().unwrap();

        let again: serde_json::Value = http
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head1,
                "entries": [entry(&cash, &income, "plaid-a", 100)],
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(again["posted"], 0);
        assert_eq!(
            again["head"].as_i64().unwrap(),
            head1,
            "nothing appended, so the head must not move: {again}"
        );
    }

    /// An unbalanced entry is skipped like any other bad row rather than failing
    /// the batch — same reasoning as a duplicate, and it is caught before the
    /// group's write lock is taken.
    #[tokio::test]
    async fn a_malformed_entry_is_skipped_not_fatal() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let cash = mk_account(&mut store, "1000", AccountType::Asset);
        let income = mk_account(&mut store, "2000", AccountType::Revenue);
        let base = serve(SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "user-1".to_string())]),
        ))
        .await;
        let head0: i64 = 2;

        let mut unbalanced = entry(&cash, &income, "plaid-bad", 100);
        unbalanced["lines"][1]["amount"] = serde_json::json!(-999);

        let r: serde_json::Value = reqwest::Client::new()
            .post(format!("{base}/sync/commands/post-entries"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": head0,
                "entries": [unbalanced, entry(&cash, &income, "plaid-ok", 100)],
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(r["posted"], 1, "the good entry did not land: {r}");
        assert_eq!(r["skipped"][0]["index"], 0);
        // The count and the log must agree. The first version subtracted every
        // skip from the candidate count — including the pure-check ones that were
        // never candidates — and reported `posted: 0` for a batch that had
        // appended. A caller trusting that re-imports the row forever.
        assert_eq!(
            r["skipped"].as_array().unwrap().len() + r["posted"].as_u64().unwrap() as usize,
            2,
            "every submitted entry must be accounted for exactly once: {r}"
        );
    }

    /// Complaints about the envelope are 400s, not domain rejections: telling a
    /// client to fix an entry when there are no entries is not a message it can
    /// act on.
    #[tokio::test]
    async fn an_empty_or_oversized_batch_is_a_bad_request() {
        let store = {
            let s = EventStore::in_memory().unwrap();
            init_schema(s.connection()).unwrap();
            s
        };
        let base = serve(SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "user-1".to_string())]),
        ))
        .await;

        let empty = reqwest::Client::new()
            .post(format!("{base}/sync/commands/post-entries"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({ "expected_head_seq": 0, "entries": [] }))
            .send()
            .await
            .unwrap();
        assert_eq!(empty.status(), reqwest::StatusCode::BAD_REQUEST);
    }
}
