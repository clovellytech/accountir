//! Auth-aware HTTP client for the sync transport — the client half of the seam
//! the desktop app repoints at a remote group server (SPEC §6.5). It owns the
//! optimistic-concurrency retry loop: submits carry the last-known head, and a
//! `409` (another member wrote first) is resolved by adopting the server's head
//! and retrying, so the caller doesn't hand-manage conflicts.

use super::commands::account::{
    CreateAccountRequest, DeactivateAccountRequest, SeedDefaultAccountsRequest,
    UpdateAccountRequest,
};
use super::commands::bill::{IssueInvoiceRequest, ReceiveBillRequest};
use super::commands::bill_ops::{
    ApplyBillPaymentRequest, ReceiveInvoicePaymentRequest, VoidBillRequest, VoidInvoiceRequest,
};
use super::commands::entries::{BatchEntry, PostEntriesRequest, PostEntriesResponse};
use super::commands::entry_ops::{
    LineAssignment, ReassignLinesRequest, ReassignLinesResponse, UnvoidEntryRequest,
    VoidEntryRequest,
};
use super::commands::event_service::{
    RecordEventServiceSyncRequest, RegisterEventServiceRequest, RegisterEventServiceResponse,
    RemoveEventServiceRequest,
};
use super::commands::plaid::{
    ConnectPlaidItemRequest, ConnectPlaidItemResponse, DisconnectPlaidItemRequest,
    MapPlaidAccountRequest, RefreshPlaidAccountsRequest, RefreshPlaidAccountsResponse,
};
use super::{EventsResponse, HeadResponse, PostEntryLine, PostEntryRequest, SubmitResponse};
use crate::domain::{AccountType, PaymentTerms};
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
    /// The group server has no such command endpoint.
    ///
    /// Its own diagnosis rather than a bare `Unexpected(404, …)`, because there is
    /// exactly one thing this means and it is actionable: the instance is running
    /// an older build than this app. It happens on a perfectly healthy deployment —
    /// pushing a new image does NOT restart running containers, so a group
    /// provisioned before the deploy keeps serving the old ledger until it is
    /// re-provisioned. "unexpected status 404" sent the last person who hit this
    /// looking at auth, DNS and TLS.
    #[error(
        "this group's server doesn't support {0} yet — it is running an older build \
         than this app. Ask whoever administers it to update the group's instance; \
         nothing was written."
    )]
    ServerTooOld(String),
    #[error("unexpected status {0}: {1}")]
    Unexpected(u16, String),
}

/// What a chunked import did, including when it did not finish.
///
/// A large import is several appends, so "it failed" and "nothing happened" are
/// different answers and this is the type that can tell them apart. Everything in
/// `posted` and `skipped` is already in the ledger whatever `stopped_by` says.
pub struct BatchOutcome {
    /// The log head after the last chunk that landed.
    pub head: i64,
    pub posted: usize,
    /// Indices are positions in the batch the caller passed, not in any chunk.
    pub skipped: Vec<crate::sync::commands::entries::SkippedEntry>,
    /// Set when a chunk failed and the run stopped there. The entries after it
    /// were never sent, and are the caller's to retry.
    pub stopped_by: Option<SyncClientError>,
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

    /// Construct with a head the caller already knows.
    ///
    /// A replica's cursor *is* `MAX(events.id)` (see [`super::replica`]), so the
    /// desktop always has the right `expected_head_seq` before it opens a
    /// connection. Starting at `0` and calling `refresh_head` would spend a round
    /// trip re-learning something already on disk — and, worse, would make the
    /// first write race a head the client never actually applied.
    pub fn with_head(base_url: impl Into<String>, token: impl Into<String>, head: i64) -> Self {
        let mut c = Self::new(base_url, token);
        c.head = head;
        c
    }

    /// Swap in a freshly issued token without discarding the cached head.
    ///
    /// Reconnecting after an expiry is an auth event, not a log event: the log the
    /// client had followed is still the log it has. Rebuilding the client would
    /// silently reset `head` to 0 and turn the next write into a guaranteed 409.
    pub fn set_token(&mut self, token: impl Into<String>) {
        self.token = token.into();
    }

    /// Where this client points. For tests that need to reach the same server by
    /// hand.
    #[cfg(test)]
    pub(crate) fn base_url(&self) -> &str {
        &self.base
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

    /// Fetch one bounded page of events after `since`.
    ///
    /// Separate from [`events_since`] rather than a parameter on it because
    /// changing that signature would break `accountir-server`, which builds
    /// against this crate. The paged form is what a replica catching up from an
    /// empty log needs: `events_since` on a year-old ledger materializes the whole
    /// log in memory twice (server and client) for one request.
    ///
    /// The response's `head` is the canonical head, not the last seq in the page,
    /// so `last_seq < head` is how the caller knows to come back for more.
    ///
    /// [`events_since`]: SyncClient::events_since
    pub async fn events_page(
        &mut self,
        since: i64,
        limit: u32,
    ) -> Result<EventsResponse, SyncClientError> {
        let resp = self
            .http
            .get(self.url(&format!("/sync/events?since={since}&limit={limit}")))
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

    /// Create an account over the wire, with the same stale-head retry loop as
    /// [`post_entry`].
    ///
    /// Blind retry is correct here for the same reason: the command is
    /// self-contained (nothing in it was derived from projections the retry would
    /// invalidate) and the server re-runs the account-number uniqueness check
    /// inside each attempt's append transaction, so a genuine duplicate comes back
    /// as a terminal `422` rather than being created twice.
    ///
    /// [`post_entry`]: SyncClient::post_entry
    #[allow(clippy::too_many_arguments)]
    pub async fn create_account(
        &mut self,
        account_type: AccountType,
        account_number: impl Into<String>,
        name: impl Into<String>,
        parent_id: Option<String>,
        currency: Option<String>,
        description: Option<String>,
    ) -> Result<i64, SyncClientError> {
        let account_number = account_number.into();
        let name = name.into();
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = CreateAccountRequest {
                expected_head_seq: self.head,
                account_type,
                account_number: account_number.clone(),
                name: name.clone(),
                parent_id: parent_id.clone(),
                currency: currency.clone(),
                description: description.clone(),
            };
            match self.submit("/sync/commands/create-account", &body).await? {
                Submitted::Head(head) => return Ok(head),
                Submitted::Retry => continue,
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Edit an account on the group's ledger.
    ///
    /// Same retry contract as [`create_account`]: a `409` means the log moved under
    /// us, so we adopt the server's head and try again. That is safe here for the
    /// same reason it is there — the server re-diffs the account against its
    /// *current* state inside each attempt's transaction, so a retry never replays
    /// a stale `old_value`.
    ///
    /// `parent_id` is `Option<Option<String>>`: `None` leaves the parent alone,
    /// `Some(None)` clears it, `Some(Some(id))` sets it.
    ///
    /// [`create_account`]: SyncClient::create_account
    pub async fn update_account(
        &mut self,
        account_id: impl Into<String>,
        account_number: Option<String>,
        name: Option<String>,
        parent_id: Option<Option<String>>,
        description: Option<String>,
    ) -> Result<i64, SyncClientError> {
        let account_id = account_id.into();
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = UpdateAccountRequest {
                expected_head_seq: self.head,
                account_id: account_id.clone(),
                account_number: account_number.clone(),
                name: name.clone(),
                parent_id: parent_id.clone(),
                description: description.clone(),
            };
            match self.submit("/sync/commands/update-account", &body).await? {
                Submitted::Head(head) => return Ok(head),
                Submitted::Retry => continue,
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Lay down the default chart of accounts on the group's ledger, atomically.
    ///
    /// Takes no arguments: the chart is the server's
    /// [`DEFAULT_CHART`](crate::commands::account_commands::DEFAULT_CHART), so two
    /// replicas on different builds cannot seed two different charts.
    ///
    /// A `422` here means the ledger already has one of those account numbers —
    /// i.e. it has been seeded, or someone created `1000` by hand. Nothing is
    /// appended in that case; it is not a partial seed to be cleaned up.
    pub async fn seed_default_accounts(&mut self) -> Result<i64, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = SeedDefaultAccountsRequest {
                expected_head_seq: self.head,
            };
            match self
                .submit("/sync/commands/seed-default-accounts", &body)
                .await?
            {
                Submitted::Head(head) => return Ok(head),
                Submitted::Retry => continue,
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// How many entries go in one request.
    ///
    /// Below the server's own cap, not at it. The server builds the whole batch in
    /// memory and appends it in a single transaction, so the batch size is how
    /// long one import holds the group's write lock — every other member is
    /// waiting behind it. Smaller chunks also mean a failure part-way costs less
    /// and progress is reportable.
    const CHUNK: usize = 250;

    /// Post many entries — a reviewed bank import, typically.
    ///
    /// Split into requests of [`CHUNK`] behind the caller's back, because a real
    /// import is routinely larger than any single request should be: a first sync
    /// of a year's bank history is thousands of transactions, and the alternative
    /// on offer was "split it" to somebody looking at a list they would have to
    /// tick one row at a time.
    ///
    /// Each chunk is its own append, so a large import is not atomic. That is the
    /// honest trade and it is safe to make here: every entry carries its bank
    /// transaction id as an idempotency `reference`, so re-running an import that
    /// stopped half way skips what already landed rather than doubling it. What
    /// would not be safe is the other direction — one transaction spanning
    /// thousands of entries, holding the write lock for as long as it takes.
    ///
    /// Indices in [`PostEntriesResponse::skipped`] are remapped to positions in
    /// the batch **the caller passed**, so the result is indistinguishable from a
    /// single request that succeeded. Callers match those indices against their
    /// own rows; chunk-local indices would silently mark the wrong ones.
    ///
    /// Entries that fail their fences are skipped and reported, not fatal. See
    /// [`crate::sync::commands::entries`] for why an import wants that.
    ///
    /// [`CHUNK`]: SyncClient::CHUNK
    pub async fn post_entries(
        &mut self,
        entries: Vec<BatchEntry>,
    ) -> Result<PostEntriesResponse, SyncClientError> {
        let out = self.post_entries_reporting(entries, |_, _| {}).await;
        match out.stopped_by {
            // Callers of this simpler form have nowhere to put a partial result,
            // so a failure is a failure. Retrying is safe — every entry carries an
            // idempotency reference — and what landed is skipped on the way back
            // through. Use `post_entries_reporting` where the partial matters.
            Some(e) => Err(e),
            None => Ok(PostEntriesResponse {
                head: out.head,
                posted: out.posted,
                skipped: out.skipped,
            }),
        }
    }

    /// The same, reporting progress and surviving a failure part-way.
    ///
    /// Never returns `Err`. A chunk that fails stops the run and is reported in
    /// [`BatchOutcome::stopped_by`], with everything the earlier chunks did
    /// already accounted for — because it *is* already in the ledger, and throwing
    /// that away is how a caller marks nothing as imported, retries, and gets its
    /// rows back as "already exists" skips it will never resolve. The rows that
    /// landed are the caller's to record before it retries the rest.
    ///
    /// `progress(done, total)` is called after each chunk. An import of a few
    /// thousand entries takes long enough that silence reads as a hang.
    pub async fn post_entries_reporting(
        &mut self,
        entries: Vec<BatchEntry>,
        mut progress: impl FnMut(usize, usize),
    ) -> BatchOutcome {
        let total = entries.len();
        let mut out = BatchOutcome {
            head: self.head,
            posted: 0,
            skipped: Vec::new(),
            stopped_by: None,
        };

        for (chunk_index, chunk) in entries.chunks(Self::CHUNK).enumerate() {
            let offset = chunk_index * Self::CHUNK;
            match self.post_one_batch(chunk.to_vec()).await {
                Ok(one) => {
                    out.head = one.head;
                    out.posted += one.posted;
                    // Back into the caller's numbering. A chunk-local index would
                    // point at whichever of the caller's rows happened to sit
                    // there, and the caller uses these to decide which of its rows
                    // were imported.
                    out.skipped.extend(one.skipped.into_iter().map(|mut s| {
                        s.index += offset;
                        s
                    }));
                    progress(out.posted + out.skipped.len(), total);
                }
                Err(e) => {
                    out.stopped_by = Some(e);
                    return out;
                }
            }
        }
        out
    }

    /// One request's worth, with the 409 retry.
    ///
    /// Retries are safe because the batch is atomic and every entry carries an
    /// idempotency `reference`: whatever landed before the conflict is seen as a
    /// duplicate on the way back through and skipped.
    async fn post_one_batch(
        &mut self,
        entries: Vec<BatchEntry>,
    ) -> Result<PostEntriesResponse, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = PostEntriesRequest {
                expected_head_seq: self.head,
                entries: entries.clone(),
            };
            let resp = self
                .http
                .post(self.url("/sync/commands/post-entries"))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await?;
            match resp.status() {
                reqwest::StatusCode::OK => {
                    let r: PostEntriesResponse = resp.json().await?;
                    self.head = r.head;
                    return Ok(r);
                }
                reqwest::StatusCode::CONFLICT => {
                    let v: serde_json::Value = resp.json().await?;
                    self.head = v["current_head"].as_i64().unwrap_or(self.head);
                    continue;
                }
                reqwest::StatusCode::UNAUTHORIZED => return Err(SyncClientError::Unauthorized),
                reqwest::StatusCode::NOT_FOUND => {
                    return Err(SyncClientError::ServerTooOld("post entries".to_string()))
                }
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SyncClientError::Unexpected(s.as_u16(), body));
                }
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Move many posted lines to different accounts in one call.
    ///
    /// The second half of a bank import: everything uncategorised posts to
    /// Uncategorized, and filing it is done in bulk. Like [`post_entries`], lines
    /// that fail their fences are **skipped and reported**, not fatal — a line
    /// somebody else already moved should not cost the other thirty-nine.
    ///
    /// Retries on 409. Safe to retry: a line already moved to the target reads as
    /// "same as current account" on the way back through and is skipped.
    ///
    /// [`post_entries`]: SyncClient::post_entries
    pub async fn reassign_lines(
        &mut self,
        assignments: Vec<LineAssignment>,
    ) -> Result<ReassignLinesResponse, SyncClientError> {
        // Chunked for the same reason `post_entries` is, and it is the same
        // import: everything a bank feed brings in lands in Uncategorized, so
        // filing a first import is a reassignment of however many transactions
        // that import had. Skip indices are remapped to the caller's numbering —
        // they are how it decides which lines it still has to file.
        let mut combined = ReassignLinesResponse {
            head: self.head,
            moved: 0,
            skipped: Vec::new(),
        };
        for (chunk_index, chunk) in assignments.chunks(Self::CHUNK).enumerate() {
            let offset = chunk_index * Self::CHUNK;
            let one = self.reassign_one_batch(chunk.to_vec()).await?;
            combined.head = one.head;
            combined.moved += one.moved;
            combined
                .skipped
                .extend(one.skipped.into_iter().map(|mut s| {
                    s.index += offset;
                    s
                }));
        }
        Ok(combined)
    }

    async fn reassign_one_batch(
        &mut self,
        assignments: Vec<LineAssignment>,
    ) -> Result<ReassignLinesResponse, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = ReassignLinesRequest {
                expected_head_seq: self.head,
                assignments: assignments.clone(),
            };
            let resp = self
                .http
                .post(self.url("/sync/commands/reassign-lines"))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await?;
            match resp.status() {
                reqwest::StatusCode::OK => {
                    let r: ReassignLinesResponse = resp.json().await?;
                    self.head = r.head;
                    return Ok(r);
                }
                reqwest::StatusCode::CONFLICT => {
                    let v: serde_json::Value = resp.json().await?;
                    self.head = v["current_head"].as_i64().unwrap_or(self.head);
                    continue;
                }
                reqwest::StatusCode::UNAUTHORIZED => return Err(SyncClientError::Unauthorized),
                reqwest::StatusCode::NOT_FOUND => {
                    return Err(SyncClientError::ServerTooOld(
                        "move posted lines to other accounts".to_string(),
                    ))
                }
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SyncClientError::Unexpected(s.as_u16(), body));
                }
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Connect an event service to the group's books.
    ///
    /// Carries no API key — see [`crate::sync::commands::event_service`]. Returns
    /// the server-minted `service_id`, which the caller needs to file the key
    /// with the group's instance.
    pub async fn register_event_service(
        &mut self,
        name: String,
        root_url: String,
    ) -> Result<RegisterEventServiceResponse, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = RegisterEventServiceRequest {
                expected_head_seq: self.head,
                name: name.clone(),
                root_url: root_url.clone(),
            };
            let resp = self
                .http
                .post(self.url("/sync/commands/register-event-service"))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await?;
            match resp.status() {
                reqwest::StatusCode::OK => {
                    let r: RegisterEventServiceResponse = resp.json().await?;
                    self.head = r.head;
                    return Ok(r);
                }
                reqwest::StatusCode::CONFLICT => {
                    let v: serde_json::Value = resp.json().await?;
                    self.head = v["current_head"].as_i64().unwrap_or(self.head);
                    continue;
                }
                reqwest::StatusCode::UNAUTHORIZED => return Err(SyncClientError::Unauthorized),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY | reqwest::StatusCode::BAD_REQUEST => {
                    let v: serde_json::Value = resp.json().await?;
                    return Err(SyncClientError::Rejected(
                        v["error"].as_str().unwrap_or_default().to_string(),
                    ));
                }
                reqwest::StatusCode::NOT_FOUND => {
                    return Err(SyncClientError::ServerTooOld(
                        "connect an event service".to_string(),
                    ))
                }
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SyncClientError::Unexpected(s.as_u16(), body));
                }
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Disconnect an event service from the group's books.
    pub async fn remove_event_service(
        &mut self,
        service_id: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        let service_id = service_id.into();
        for _ in 0..=MAX_RETRIES {
            let body = RemoveEventServiceRequest {
                expected_head_seq: self.head,
                service_id: service_id.clone(),
            };
            match self
                .submit("/sync/commands/remove-event-service", &body)
                .await?
            {
                Submitted::Head(head) => return Ok(head),
                Submitted::Retry => continue,
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Record a completed sync of an event service against the group's books, so
    /// every member can see that someone pulled it and what came of it.
    pub async fn record_event_service_sync(
        &mut self,
        service_id: impl Into<String>,
        events_processed: u32,
        entries_created: u32,
        errors: u32,
    ) -> Result<i64, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        let service_id = service_id.into();
        for _ in 0..=MAX_RETRIES {
            let body = RecordEventServiceSyncRequest {
                expected_head_seq: self.head,
                service_id: service_id.clone(),
                events_processed,
                entries_created,
                errors,
            };
            match self
                .submit("/sync/commands/record-event-service-sync", &body)
                .await?
            {
                Submitted::Head(head) => return Ok(head),
                Submitted::Retry => continue,
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Record a bank connection on the group's ledger.
    ///
    /// Carries no credential and no proxy handle — see
    /// [`crate::sync::commands::plaid`]. Returns the server-minted `item_id`,
    /// which the caller needs to attach a grant to this connection.
    pub async fn connect_plaid_item(
        &mut self,
        institution_name: String,
        plaid_accounts: Vec<crate::events::types::PlaidAccountInfo>,
    ) -> Result<ConnectPlaidItemResponse, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = ConnectPlaidItemRequest {
                expected_head_seq: self.head,
                institution_name: institution_name.clone(),
                plaid_accounts: plaid_accounts.clone(),
            };
            let resp = self
                .http
                .post(self.url("/sync/commands/connect-plaid-item"))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await?;
            match resp.status() {
                reqwest::StatusCode::OK => {
                    let r: ConnectPlaidItemResponse = resp.json().await?;
                    self.head = r.head;
                    return Ok(r);
                }
                reqwest::StatusCode::CONFLICT => {
                    let v: serde_json::Value = resp.json().await?;
                    self.head = v["current_head"].as_i64().unwrap_or(self.head);
                    continue;
                }
                reqwest::StatusCode::UNAUTHORIZED => return Err(SyncClientError::Unauthorized),
                reqwest::StatusCode::NOT_FOUND => {
                    return Err(SyncClientError::ServerTooOld("connect a bank".to_string()))
                }
                s => {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(SyncClientError::Unexpected(s.as_u16(), body));
                }
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Tell the group what the bank now reports behind a connection.
    ///
    /// The member's own machine has already asked the proxy — it holds the API
    /// key — so this carries the answer rather than a request to go and find out.
    /// The server decides whether it is news, under its write lock, which is what
    /// makes two members refreshing at once append one event instead of two.
    ///
    /// Blind retry on `409` is safe here for the reason `submit_retrying`
    /// requires: the payload is the bank's list and the ids in it, none of it
    /// derived from a projection that a competing write could have moved.
    pub async fn refresh_plaid_accounts(
        &mut self,
        item_id: impl Into<String>,
        plaid_accounts: Vec<crate::events::types::PlaidAccountInfo>,
    ) -> Result<RefreshPlaidAccountsResponse, SyncClientError> {
        let item_id = item_id.into();
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = RefreshPlaidAccountsRequest {
                expected_head_seq: self.head,
                item_id: item_id.clone(),
                plaid_accounts: plaid_accounts.clone(),
            };
            let resp = self
                .http
                .post(self.url("/sync/commands/refresh-plaid-accounts"))
                .bearer_auth(&self.token)
                .json(&body)
                .send()
                .await?;
            match resp.status() {
                reqwest::StatusCode::OK => {
                    let r: RefreshPlaidAccountsResponse = resp.json().await?;
                    self.head = r.head;
                    return Ok(r);
                }
                reqwest::StatusCode::CONFLICT => {
                    let v: serde_json::Value = resp.json().await?;
                    self.head = v["current_head"].as_i64().unwrap_or(self.head);
                    continue;
                }
                reqwest::StatusCode::UNAUTHORIZED => return Err(SyncClientError::Unauthorized),
                reqwest::StatusCode::NOT_FOUND => {
                    return Err(SyncClientError::ServerTooOld(
                        "refresh a connection's accounts".to_string(),
                    ))
                }
                reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
                    let v: serde_json::Value = resp.json().await?;
                    return Err(SyncClientError::Rejected(
                        v["error"].as_str().unwrap_or("refused").to_string(),
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

    /// Stop a bank connection on the group's books.
    ///
    /// Nothing is deleted — its accounts, their mappings and everything imported
    /// through them remain. Safe to retry blindly on a `409`: the item id and the
    /// reason are the caller's own, not derived from a projection a competing
    /// write could have moved, and the server re-checks that the connection is
    /// still active inside the append.
    pub async fn disconnect_plaid_item(
        &mut self,
        item_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        let (item_id, reason) = (item_id.into(), reason.into());
        self.submit_retrying("/sync/commands/disconnect-plaid-item", |head| {
            DisconnectPlaidItemRequest {
                expected_head_seq: head,
                item_id: item_id.clone(),
                reason: reason.clone(),
            }
        })
        .await
    }

    /// Link a bank account to a ledger account on the group's books.
    pub async fn map_plaid_account(
        &mut self,
        item_id: impl Into<String>,
        plaid_account_id: impl Into<String>,
        local_account_id: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        self.plaid_mapping(
            "/sync/commands/map-plaid-account",
            item_id.into(),
            plaid_account_id.into(),
            local_account_id.into(),
        )
        .await
    }

    /// Unlink one. Same shape, different route — see `MapPlaidAccountRequest`
    /// for why the direction is the endpoint rather than a field.
    pub async fn unmap_plaid_account(
        &mut self,
        item_id: impl Into<String>,
        plaid_account_id: impl Into<String>,
        local_account_id: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        self.plaid_mapping(
            "/sync/commands/unmap-plaid-account",
            item_id.into(),
            plaid_account_id.into(),
            local_account_id.into(),
        )
        .await
    }

    async fn plaid_mapping(
        &mut self,
        path: &str,
        item_id: String,
        plaid_account_id: String,
        local_account_id: String,
    ) -> Result<i64, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = MapPlaidAccountRequest {
                expected_head_seq: self.head,
                item_id: item_id.clone(),
                plaid_account_id: plaid_account_id.clone(),
                local_account_id: local_account_id.clone(),
            };
            match self.submit(path, &body).await? {
                Submitted::Head(head) => return Ok(head),
                Submitted::Retry => continue,
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// One command POST, with the status mapping every command endpoint shares:
    /// 200 → new head, 409 → adopt the server's head and tell the caller to retry,
    /// 401 → unauthorized, 422 → terminal domain rejection.
    async fn submit<B: serde::Serialize>(
        &mut self,
        path: &str,
        body: &B,
    ) -> Result<Submitted, SyncClientError> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        match resp.status() {
            reqwest::StatusCode::OK => {
                let r: SubmitResponse = resp.json().await?;
                self.head = r.head;
                Ok(Submitted::Head(r.head))
            }
            reqwest::StatusCode::CONFLICT => {
                let v: serde_json::Value = resp.json().await?;
                self.head = v["current_head"].as_i64().unwrap_or(self.head);
                Ok(Submitted::Retry)
            }
            reqwest::StatusCode::UNAUTHORIZED => Err(SyncClientError::Unauthorized),
            reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
                let v: serde_json::Value = resp.json().await?;
                Err(SyncClientError::Rejected(
                    v["error"].as_str().unwrap_or_default().to_string(),
                ))
            }
            // A command endpoint that isn't there is a version skew, not a
            // mystery. Naming the command rather than the path: the user is being
            // told which action is unavailable, and "/sync/commands/…" is our
            // plumbing, not their vocabulary.
            reqwest::StatusCode::NOT_FOUND => Err(SyncClientError::ServerTooOld(
                path.rsplit('/').next().unwrap_or(path).replace('-', " "),
            )),
            s => {
                let body = resp.text().await.unwrap_or_default();
                Err(SyncClientError::Unexpected(s.as_u16(), body))
            }
        }
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
            match self.submit("/sync/commands/post-entry", &body).await? {
                // Stale head: `submit` already adopted the server's, so retry.
                Submitted::Retry => continue,
                Submitted::Head(head) => return Ok(head),
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    // -----------------------------------------------------------------------
    // The rest of the command surface
    // -----------------------------------------------------------------------

    /// The stale-head retry loop, written once.
    ///
    /// `build` is called **per attempt** with the head this client currently
    /// believes in, so a retry re-serializes the command against the head the
    /// server just told us about rather than resending a body already known to be
    /// stale. Hand-rolling that loop per command is how one of them ends up
    /// resending `expected_head_seq: 0` forever.
    ///
    /// Sound only for **self-contained** commands: every field is either typed by
    /// the user or an id they picked, and every invariant is re-run by the server
    /// *inside* the append transaction. A command whose payload was derived from a
    /// projection ("pay whatever is left on this bill") must not use this — the
    /// projection moved when the other writer won, so the retry would apply an
    /// amount computed against a world that no longer exists. Every command below
    /// takes an explicit amount and explicit ids from the caller, which is exactly
    /// what makes blind retry safe for them.
    async fn submit_retrying<B: serde::Serialize>(
        &mut self,
        path: &str,
        build: impl Fn(i64) -> B,
    ) -> Result<i64, SyncClientError> {
        const MAX_RETRIES: u32 = 5;
        for _ in 0..=MAX_RETRIES {
            let body = build(self.head);
            match self.submit(path, &body).await? {
                Submitted::Head(head) => return Ok(head),
                // Stale head: `submit` already adopted the server's, so rebuild
                // the body against it and try again.
                Submitted::Retry => continue,
            }
        }
        Err(SyncClientError::ConflictExhausted(MAX_RETRIES))
    }

    /// Void a journal entry. The server re-checks that the entry exists and is not
    /// already voided, so a double-click comes back as a terminal `422` rather
    /// than two void events.
    pub async fn void_entry(
        &mut self,
        entry_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        let (entry_id, reason) = (entry_id.into(), reason.into());
        self.submit_retrying("/sync/commands/void-entry", |head| VoidEntryRequest {
            expected_head_seq: head,
            entry_id: entry_id.clone(),
            reason: reason.clone(),
        })
        .await
    }

    /// Unvoid a journal entry. The server re-runs the reference-reclamation guard
    /// under its write lock, so an entry whose reference was claimed by someone
    /// else in the meantime is refused rather than resurrected into a duplicate.
    pub async fn unvoid_entry(
        &mut self,
        entry_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        let (entry_id, reason) = (entry_id.into(), reason.into());
        self.submit_retrying("/sync/commands/unvoid-entry", |head| UnvoidEntryRequest {
            expected_head_seq: head,
            entry_id: entry_id.clone(),
            reason: reason.clone(),
        })
        .await
    }

    /// Deactivate an account. The zero-balance fence is re-checked server-side
    /// inside the append transaction, so a posting that lands first turns this into
    /// a `422` instead of deactivating an account that now holds money.
    pub async fn deactivate_account(
        &mut self,
        account_id: impl Into<String>,
        reason: Option<String>,
    ) -> Result<i64, SyncClientError> {
        let account_id = account_id.into();
        self.submit_retrying("/sync/commands/deactivate-account", |head| {
            DeactivateAccountRequest {
                expected_head_seq: head,
                account_id: account_id.clone(),
                reason: reason.clone(),
            }
        })
        .await
    }

    /// Receive a bill: the bill's journal entry and `BillReceived`, appended
    /// atomically by the server.
    #[allow(clippy::too_many_arguments)]
    pub async fn receive_bill(
        &mut self,
        vendor: String,
        amount: i64,
        currency: String,
        issue_date: NaiveDate,
        terms: PaymentTerms,
        memo: Option<String>,
        expense_account_id: String,
        ap_account_id: String,
        reference: Option<String>,
    ) -> Result<i64, SyncClientError> {
        self.submit_retrying("/sync/commands/receive-bill", |head| ReceiveBillRequest {
            expected_head_seq: head,
            vendor: vendor.clone(),
            amount,
            currency: currency.clone(),
            issue_date,
            terms: terms.clone(),
            memo: memo.clone(),
            expense_account_id: expense_account_id.clone(),
            ap_account_id: ap_account_id.clone(),
            reference: reference.clone(),
        })
        .await
    }

    /// Issue an invoice: the invoice's journal entry and `InvoiceIssued`, appended
    /// atomically by the server.
    #[allow(clippy::too_many_arguments)]
    pub async fn issue_invoice(
        &mut self,
        customer: String,
        amount: i64,
        currency: String,
        issue_date: NaiveDate,
        terms: PaymentTerms,
        memo: Option<String>,
        revenue_account_id: String,
        ar_account_id: String,
    ) -> Result<i64, SyncClientError> {
        self.submit_retrying("/sync/commands/issue-invoice", |head| IssueInvoiceRequest {
            expected_head_seq: head,
            customer: customer.clone(),
            amount,
            currency: currency.clone(),
            issue_date,
            terms: terms.clone(),
            memo: memo.clone(),
            revenue_account_id: revenue_account_id.clone(),
            ar_account_id: ar_account_id.clone(),
        })
        .await
    }

    /// Apply a payment to a bill.
    ///
    /// `amount_applied` is the figure the *user typed*, never a remaining balance
    /// this client computed — see [`submit_retrying`] for why that distinction is
    /// what makes the retry safe. The server re-runs the "cumulative payments ≤
    /// amount" guard, so a colleague's payment landing first turns an overpayment
    /// into a `422` the user is shown, not into a bill paid twice.
    ///
    /// [`submit_retrying`]: SyncClient::submit_retrying
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_bill_payment(
        &mut self,
        bill_id: String,
        payment_date: NaiveDate,
        amount_applied: i64,
        payment_account_id: String,
        ap_account_id: String,
        memo: Option<String>,
    ) -> Result<i64, SyncClientError> {
        self.submit_retrying("/sync/commands/apply-bill-payment", |head| {
            ApplyBillPaymentRequest {
                expected_head_seq: head,
                bill_id: bill_id.clone(),
                payment_date,
                amount_applied,
                payment_account_id: payment_account_id.clone(),
                ap_account_id: ap_account_id.clone(),
                memo: memo.clone(),
            }
        })
        .await
    }

    /// Receive a payment against an invoice. Same rule as
    /// [`apply_bill_payment`]: an explicit amount, an explicit invoice.
    ///
    /// [`apply_bill_payment`]: SyncClient::apply_bill_payment
    #[allow(clippy::too_many_arguments)]
    pub async fn receive_invoice_payment(
        &mut self,
        invoice_id: String,
        payment_date: NaiveDate,
        amount_applied: i64,
        payment_account_id: String,
        ar_account_id: String,
        memo: Option<String>,
    ) -> Result<i64, SyncClientError> {
        self.submit_retrying("/sync/commands/receive-invoice-payment", |head| {
            ReceiveInvoicePaymentRequest {
                expected_head_seq: head,
                invoice_id: invoice_id.clone(),
                payment_date,
                amount_applied,
                payment_account_id: payment_account_id.clone(),
                ar_account_id: ar_account_id.clone(),
                memo: memo.clone(),
            }
        })
        .await
    }

    /// Void a bill. The server re-runs the no-payments guard, so a bill someone
    /// paid a second ago is refused rather than voided out from under the payment.
    pub async fn void_bill(
        &mut self,
        bill_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        let (bill_id, reason) = (bill_id.into(), reason.into());
        self.submit_retrying("/sync/commands/void-bill", |head| VoidBillRequest {
            expected_head_seq: head,
            bill_id: bill_id.clone(),
            reason: reason.clone(),
        })
        .await
    }

    /// Void an invoice. Same no-payments guard as [`void_bill`], re-run
    /// server-side.
    ///
    /// [`void_bill`]: SyncClient::void_bill
    pub async fn void_invoice(
        &mut self,
        invoice_id: impl Into<String>,
        reason: impl Into<String>,
    ) -> Result<i64, SyncClientError> {
        let (invoice_id, reason) = (invoice_id.into(), reason.into());
        self.submit_retrying("/sync/commands/void-invoice", |head| VoidInvoiceRequest {
            expected_head_seq: head,
            invoice_id: invoice_id.clone(),
            reason: reason.clone(),
        })
        .await
    }
}

/// The outcome of one command POST, before the retry loop decides what to do.
enum Submitted {
    /// Accepted; the log head is now this.
    Head(i64),
    /// Stale head — the client's cached head has been updated, try again.
    Retry,
}
