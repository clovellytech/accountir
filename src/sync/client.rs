//! Auth-aware HTTP client for the sync transport — the client half of the seam
//! the desktop app repoints at a remote group server (SPEC §6.5). It owns the
//! optimistic-concurrency retry loop: submits carry the last-known head, and a
//! `409` (another member wrote first) is resolved by adopting the server's head
//! and retrying, so the caller doesn't hand-manage conflicts.

use super::commands::account::{CreateAccountRequest, DeactivateAccountRequest};
use super::commands::bill::{IssueInvoiceRequest, ReceiveBillRequest};
use super::commands::bill_ops::{
    ApplyBillPaymentRequest, ReceiveInvoicePaymentRequest, VoidBillRequest, VoidInvoiceRequest,
};
use super::commands::entry_ops::{UnvoidEntryRequest, VoidEntryRequest};
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
