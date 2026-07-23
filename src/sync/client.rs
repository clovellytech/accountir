//! Auth-aware HTTP client for the sync transport — the client half of the seam
//! the desktop app repoints at a remote group server (SPEC §6.5). It owns the
//! optimistic-concurrency retry loop: submits carry the last-known head, and a
//! `409` (another member wrote first) is resolved by adopting the server's head
//! and retrying, so the caller doesn't hand-manage conflicts.

use super::{EventsResponse, HeadResponse, PostEntryLine, PostEntryRequest, SubmitResponse};
use chrono::NaiveDate;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SyncClientError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unauthorized")]
    Unauthorized,
    #[error("rejected by server: {0}")]
    Rejected(String),
    #[error("still conflicting after {0} retries")]
    ConflictExhausted(u32),
    #[error("unexpected status {0}: {1}")]
    Unexpected(u16, String),
}

/// A client for one group server, authenticated with a bearer token. Caches the
/// last-known log head so submits carry the right `expected_head_seq`.
pub struct SyncClient {
    base: String,
    token: String,
    http: reqwest::Client,
    head: i64,
}

impl SyncClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: base_url.into(),
            token: token.into(),
            http: reqwest::Client::new(),
            head: 0,
        }
    }

    /// The last head this client knows about (advanced by every successful call).
    pub fn head(&self) -> i64 {
        self.head
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Fetch and cache the current canonical head.
    pub async fn refresh_head(&mut self) -> Result<i64, SyncClientError> {
        let resp = self
            .http
            .get(self.url("/sync/head"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SyncClientError::Unauthorized);
        }
        let h: HeadResponse = resp.error_for_status()?.json().await?;
        self.head = h.head;
        Ok(self.head)
    }

    /// Fetch events after `since` (catch-up / projection rebuild); caches the head.
    pub async fn events_since(&mut self, since: i64) -> Result<EventsResponse, SyncClientError> {
        let resp = self
            .http
            .get(self.url(&format!("/sync/events?since={since}")))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SyncClientError::Unauthorized);
        }
        let e: EventsResponse = resp.error_for_status()?.json().await?;
        self.head = e.head;
        Ok(e)
    }

    /// Post a journal entry, auto-resolving stale-head conflicts: on `409` it
    /// adopts the server's current head and retries (up to 5 times). A `422` is a
    /// terminal domain rejection. On success the cached head advances.
    ///
    /// Blindly retrying the same command is correct **for `post_entry`** — it is
    /// self-contained and the server re-checks the fences + reference idempotency
    /// on each attempt. A command whose payload was *derived from projections*
    /// (e.g. "pay the remaining balance") must instead, on `409`, refetch
    /// projections and REBUILD before retrying; that is the caller's job, not this
    /// loop's.
    pub async fn post_entry(
        &mut self,
        date: NaiveDate,
        memo: impl Into<String>,
        lines: Vec<PostEntryLine>,
        reference: Option<String>,
    ) -> Result<i64, SyncClientError> {
        let memo = memo.into();
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = PostEntryRequest {
                expected_head_seq: self.head,
                date,
                memo: memo.clone(),
                lines: lines.clone(),
                reference: reference.clone(),
            };
            let resp = self
                .http
                .post(self.url("/sync/commands/post-entry"))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await?;
            match resp.status() {
                reqwest::StatusCode::OK => {
                    let r: SubmitResponse = resp.json().await?;
                    self.head = r.head;
                    return Ok(r.head);
                }
                reqwest::StatusCode::CONFLICT => {
                    // Stale head: adopt the server's and retry.
                    let v: serde_json::Value = resp.json().await?;
                    self.head = v["current_head"].as_i64().unwrap_or(self.head);
                    continue;
                }
                reqwest::StatusCode::UNAUTHORIZED => return Err(SyncClientError::Unauthorized),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
                    let v: serde_json::Value = resp.json().await?;
                    return Err(SyncClientError::Rejected(
                        v["error"].as_str().unwrap_or_default().to_string(),
                    ));
                }
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SyncClientError::Unexpected(s.as_u16(), body));
                }
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }
}
