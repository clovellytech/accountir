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
                // References claimed by earlier entries *in this batch*.
                //
                // `build_post_entry_in_txn` asks the projection whether a reference
                // is free, and the projection does not exist yet for anything in
                // this batch: `append_checked_many` runs this whole closure before
                // it appends or projects a single event. So two entries carrying the
                // same reference both pass the fence, both get appended, and the
                // second one's projection trips
                // `idx_journal_entries_reference_unique` — which is not a rejection
                // but a store error, so the WHOLE batch rolls back and the caller
                // gets a 500 with nothing imported.
                //
                // That is not hypothetical. An event feed's `since` cursor is
                // usually inclusive, so consecutive pages overlap by one event, and
                // a 368-event import died on it: 367 good entries lost to one
                // repeat. The fence has to cover the batch as well as the books.
                let mut claimed: std::collections::HashSet<&str> = std::collections::HashSet::new();
                for (index, cmd) in &candidates {
                    if let Some(reference) = cmd.reference.as_deref() {
                        if !claimed.insert(reference) {
                            // Worded exactly like the persisted-duplicate rejection
                            // above, because it means the same thing to the caller —
                            // this entry is already accounted for — and clients
                            // classify skips by that wording.
                            in_txn_skips.borrow_mut().push(SkippedEntry {
                                index: *index,
                                reason: format!(
                                    "An entry with reference {reference} already exists"
                                ),
                            });
                            continue;
                        }
                    }
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

    pub(super) async fn serve(state: SyncState) -> String {
        let app = router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

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

    /// The failure that cost a real import all 368 of its entries.
    ///
    /// `build_post_entry_in_txn` asks the *projection* whether a reference is free,
    /// and `append_checked_many` runs the whole check closure before it projects
    /// anything — so two entries in one batch carrying the same reference both
    /// passed, both appended, and the second one's projection tripped
    /// `idx_journal_entries_reference_unique`. That is a store error, not a
    /// rejection: the entire transaction rolled back and the caller got a 500 with
    /// nothing imported.
    ///
    /// It is not an exotic input. An event feed's `since` cursor is usually
    /// inclusive, so consecutive pages overlap by one event, and every import of
    /// more than one page carried a repeat.
    #[tokio::test]
    async fn a_reference_repeated_inside_one_batch_is_skipped_not_fatal() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let cash = mk_account(&mut store, "1000", AccountType::Asset);
        let income = mk_account(&mut store, "2000", AccountType::Revenue);
        let base = serve(SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "user-1".to_string())]),
        ))
        .await;

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/post-entries"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({
                "expected_head_seq": 2,
                "entries": [
                    entry(&cash, &income, "feed:e-1", 100),
                    entry(&cash, &income, "feed:e-2", 200),
                    // The overlap: page 2 re-served the last row of page 1.
                    entry(&cash, &income, "feed:e-2", 200),
                    entry(&cash, &income, "feed:e-3", 300),
                ],
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(
            r.status(),
            reqwest::StatusCode::OK,
            "a repeated reference inside the batch killed the whole import"
        );
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["posted"], 3, "the three distinct entries must land: {v}");
        assert_eq!(v["skipped"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["skipped"][0]["index"], 2,
            "the SECOND copy is the one dropped: {v}"
        );
        assert!(
            v["skipped"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("already exists"),
            "clients classify duplicates by this wording: {v}"
        );

        // And the books hold each reference exactly once.
        let entries: serde_json::Value = reqwest::Client::new()
            .get(format!("{base}/sync/events?since=0&limit=50"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        // Count the REFERENCE field specifically — the test fixture also puts the
        // reference in the memo, so a bare substring count sees each entry twice.
        let dump = entries.to_string();
        assert_eq!(
            dump.matches(r#""reference":"feed:e-2""#).count(),
            1,
            "feed:e-2 was posted twice: {dump}"
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

/// Importing more entries than one request may carry.
///
/// The server caps a batch to bound how long one append holds the group's write
/// lock, which is right — but a first sync of a year's bank history is thousands
/// of transactions, and the cap surfaced as "too many entries in one batch; split
/// it" to somebody looking at a list they would have had to tick one row at a
/// time. The client splits it instead.
#[cfg(test)]
mod chunked_import {
    use super::tests::{mk_account, serve};
    use super::*;
    use crate::domain::AccountType;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::sync::client::SyncClient;
    use crate::sync::{PostEntryLine, SyncState};
    use std::collections::HashMap;

    const TOKEN: &str = "tok-import";

    fn batch_entry(cash: &str, income: &str, reference: &str, amount: i64) -> BatchEntry {
        BatchEntry {
            date: chrono::NaiveDate::from_ymd_opt(2026, 3, 4).unwrap(),
            memo: format!("Import {reference}"),
            lines: vec![
                PostEntryLine {
                    account_id: cash.to_string(),
                    amount,
                    currency: "USD".to_string(),
                    memo: None,
                },
                PostEntryLine {
                    account_id: income.to_string(),
                    amount: -amount,
                    currency: "USD".to_string(),
                    memo: None,
                },
            ],
            reference: Some(reference.to_string()),
        }
    }

    async fn fixture(entries: usize) -> (SyncClient, Vec<BatchEntry>) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let cash = mk_account(&mut store, "1000", AccountType::Asset);
        let income = mk_account(&mut store, "4000", AccountType::Revenue);
        let base = serve(SyncState::new(
            store,
            HashMap::from([(TOKEN.to_string(), "member".to_string())]),
        ))
        .await;

        let batch = (0..entries)
            .map(|i| batch_entry(&cash, &income, &format!("feed:{i}"), 100 + i as i64))
            .collect();
        (SyncClient::new(base, TOKEN), batch)
    }

    /// Two thousand transactions, which is what a real first import looked like.
    #[tokio::test]
    async fn an_import_larger_than_one_request_still_posts_all_of_it() {
        let (mut client, batch) = fixture(2_000).await;

        let mut seen: Vec<(usize, usize)> = Vec::new();
        let out = client
            .post_entries_reporting(batch, |done, total| seen.push((done, total)))
            .await;

        assert!(out.stopped_by.is_none(), "the import did not finish");
        assert_eq!(out.posted, 2_000, "skipped: {:?}", out.skipped);
        assert!(out.skipped.is_empty());
        assert_eq!(
            seen.last(),
            Some(&(2_000, 2_000)),
            "progress never reached the end: {seen:?}"
        );
        assert!(seen.len() > 1, "an import this size reported no progress");
    }

    /// The half of chunking that fails silently if it is wrong.
    ///
    /// A skip's index is how the caller decides which of *its* rows was not
    /// imported. Report a chunk-local index and the caller marks whichever row
    /// happened to sit at that position — so a bank transaction that never posted
    /// is recorded as imported, and one that did posts again on the next run.
    #[tokio::test]
    async fn a_skip_reports_its_position_in_the_batch_the_caller_passed() {
        let (mut client, mut batch) = fixture(600).await;
        // Duplicate references, chosen to land in the second and third chunks:
        // the server skips the later of the pair.
        batch[300].reference = batch[7].reference.clone();
        batch[550].reference = batch[9].reference.clone();

        let out = client.post_entries_reporting(batch, |_, _| {}).await;

        assert!(out.stopped_by.is_none());
        assert_eq!(out.posted, 598);
        let mut skipped: Vec<usize> = out.skipped.iter().map(|s| s.index).collect();
        skipped.sort();
        assert_eq!(
            skipped,
            vec![300, 550],
            "indices were reported per chunk, not per batch — the caller would \
             mark the wrong rows as imported"
        );
    }

    /// Filing an import is the same size as the import.
    ///
    /// Everything a bank feed brings in posts to Uncategorized, so the reassign
    /// that follows carries one line per transaction — the same thousands, and
    /// the same cap. Fixing one and leaving the other would move the wall by one
    /// step.
    #[tokio::test]
    async fn filing_a_large_import_is_chunked_too() {
        let (mut client, batch) = fixture(1_500).await;
        let out = client.post_entries_reporting(batch, |_, _| {}).await;
        assert!(out.stopped_by.is_none());
        assert_eq!(out.posted, 1_500);

        // Every line that landed in the revenue account, moved somewhere else.
        let base = client.base_url().to_string();
        let http = reqwest::Client::new();
        let mut assignments = Vec::new();
        let mut target = String::new();
        // Paged, because the events endpoint caps a page — the same bound this
        // whole test is about, seen from the read side.
        let mut since = 0i64;
        loop {
            let page: serde_json::Value = http
                .get(format!("{base}/sync/events?since={since}&limit=500"))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let events = page["events"].as_array().unwrap().clone();
            if events.is_empty() {
                break;
            }
            for e in &events {
                since = e["seq"].as_i64().unwrap();
                let p = &e["event"];
                if p["type"] == "account_created" && p["account_number"] == "4000" {
                    target = p["account_id"].as_str().unwrap().to_string();
                }
                if p["type"] == "journal_entry_posted" {
                    let line = &p["lines"][0];
                    assignments.push(crate::sync::commands::entry_ops::LineAssignment {
                        entry_id: p["entry_id"].as_str().unwrap().to_string(),
                        line_id: line["line_id"].as_str().unwrap().to_string(),
                        new_account_id: target.clone(),
                    });
                }
            }
        }
        assert_eq!(
            assignments.len(),
            1_500,
            "the fixture did not post what it said"
        );

        let r = client
            .reassign_lines(assignments)
            .await
            .expect("filing 1500 lines must not be refused as one oversized batch");
        assert_eq!(r.moved + r.skipped.len(), 1_500, "skipped: {:?}", r.skipped);
    }

    /// The server's own cap is still the cap.
    ///
    /// The client splits below it; nothing here relaxes what the server accepts,
    /// because the reason for the cap — one append, one write lock, held for as
    /// long as the batch takes — is unchanged.
    #[tokio::test]
    async fn the_server_still_refuses_an_oversized_single_request() {
        let (client, batch) = fixture(1).await;
        let base = client.base_url().to_string();
        let entries: Vec<serde_json::Value> = (0..MAX_BATCH + 1)
            .map(|i| {
                serde_json::json!({
                    "date": "2026-03-04",
                    "memo": format!("e{i}"),
                    "lines": batch[0].lines.iter().map(|l| serde_json::json!({
                        "account_id": l.account_id,
                        "amount": l.amount,
                        "currency": l.currency,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();

        let r = reqwest::Client::new()
            .post(format!("{base}/sync/commands/post-entries"))
            .bearer_auth(TOKEN)
            .json(&serde_json::json!({"expected_head_seq": 2, "entries": entries}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    }
}
