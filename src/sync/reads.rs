//! Projection read endpoints over the sync transport — so a client can *display*
//! the ledger (read-your-writes), not just append to it. These expose the group's
//! projection tables (chart of accounts, journal entries, trial balance) through
//! the existing `crate::queries` read structs — no SQL is reimplemented here. All
//! endpoints are bearer-authenticated (take the [`AuthedUser`] extractor) and
//! strictly read-only: they lock the store, run a query against
//! `store.connection()`, and serialize. No cross-tenant concern — one instance =
//! one group's data.

use crate::domain::AccountType;
use crate::queries::account_queries::AccountQueries;
use crate::queries::reports::Reports;
use crate::queries::search::Search;
use crate::sync::{ApiError, AuthedUser, SyncState};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// The read-only projection router, merged into the sync transport in
/// [`crate::sync::router`].
pub fn router() -> Router<SyncState> {
    Router::new()
        .route("/sync/accounts", get(get_accounts))
        .route("/sync/entries", get(get_entries))
        .route("/sync/trial-balance", get(get_trial_balance))
}

/// Map a projection-query failure (bad DB state, I/O) to a 500. Read endpoints
/// never touch domain invariants, so there is no 422/409 path here. Constructed
/// directly because `reads` is a child module of `sync` and so can see
/// `ApiError`'s private fields.
fn query_err<E: std::fmt::Display>(e: E) -> ApiError {
    // Don't leak internal detail (SQLite messages / paths) to the client; log it
    // server-side and return a generic 500. Mirrors ApiError::store (review L1).
    eprintln!("sync: internal read query error: {e}");
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        body: serde_json::json!({ "error": "internal error" }),
    }
}

// --- GET /sync/accounts : chart of accounts + balances ---

/// One account in the chart of accounts, with its current net balance (smallest
/// currency unit; positive = debit balance, negative = credit balance).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AccountDto {
    pub id: String,
    pub account_number: String,
    pub name: String,
    pub account_type: AccountType,
    pub is_active: bool,
    pub currency: String,
    /// Net balance across all non-void entries, as of now.
    pub balance: i64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AccountsResponse {
    pub accounts: Vec<AccountDto>,
}

/// The full chart of accounts (active *and* inactive), each with its running
/// balance. Reuses [`AccountQueries`] — `get_all_accounts` for the chart,
/// `get_account_balance` for each net balance.
async fn get_accounts(
    _user: AuthedUser,
    State(st): State<SyncState>,
) -> Result<Json<AccountsResponse>, ApiError> {
    let store = st.store.lock().unwrap();
    let q = AccountQueries::new(store.connection());
    let accounts = q.get_all_accounts().map_err(query_err)?;

    let mut out = Vec::with_capacity(accounts.len());
    for a in accounts {
        let balance = q
            .get_account_balance(&a.id, None)
            .map_err(query_err)?
            .balance;
        out.push(AccountDto {
            id: a.id,
            account_number: a.account_number,
            name: a.name,
            account_type: a.account_type,
            is_active: a.is_active,
            currency: a.currency.unwrap_or_else(|| "USD".to_string()),
            balance,
        });
    }
    Ok(Json(AccountsResponse { accounts: out }))
}

// --- GET /sync/entries : journal entries, newest first ---

#[derive(Deserialize)]
struct EntriesQuery {
    /// Cap the number of returned entries (paging). Entries are newest-first, so
    /// this returns the most recent `limit` entries.
    limit: Option<usize>,
    /// Include voided entries (default: false).
    #[serde(default)]
    include_void: bool,
}

/// A journal entry summary as shown in a register/list view.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EntryDto {
    pub entry_id: String,
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    /// Total entry amount (sum of debit legs), smallest currency unit.
    pub total_amount: i64,
    pub is_void: bool,
    pub source: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct EntriesResponse {
    pub entries: Vec<EntryDto>,
}

/// Journal entries, newest first (by date then id). Reuses [`Search::search_entries`]
/// with no filters; `?limit=N` pages the most recent N, `?include_void=true` keeps
/// voided entries.
async fn get_entries(
    _user: AuthedUser,
    State(st): State<SyncState>,
    Query(q): Query<EntriesQuery>,
) -> Result<Json<EntriesResponse>, ApiError> {
    // Bound the response: the client's `limit` is clamped to a hard max, and a
    // default applies when omitted, so a read can't materialize the whole ledger
    // under the store lock (review L3).
    const DEFAULT_LIMIT: usize = 200;
    const MAX_LIMIT: usize = 1000;
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT);

    let store = st.store.lock().unwrap();
    let search = Search::new(store.connection());
    let results = search
        .search_entries(None, None, None, None, q.include_void, Some(limit))
        .map_err(query_err)?;

    let entries = results
        .into_iter()
        .map(|r| EntryDto {
            entry_id: r.entry_id,
            date: r.date,
            memo: r.memo,
            reference: r.reference,
            total_amount: r.total_amount,
            is_void: r.is_void,
            source: r.source,
        })
        .collect();
    Ok(Json(EntriesResponse { entries }))
}

// --- GET /sync/trial-balance : per-account debit/credit balances ---

/// One line of the trial balance. Exactly one of `debit`/`credit` is set per line
/// (a zero-balance account is omitted entirely).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TrialBalanceLineDto {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub account_type: AccountType,
    pub debit: Option<i64>,
    pub credit: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TrialBalanceResponse {
    pub as_of_date: Option<NaiveDate>,
    pub lines: Vec<TrialBalanceLineDto>,
    pub total_debits: i64,
    pub total_credits: i64,
    /// `total_debits == total_credits` — always true for a consistent ledger.
    pub is_balanced: bool,
}

/// The trial balance as of now. Reuses [`Reports::trial_balance`].
async fn get_trial_balance(
    _user: AuthedUser,
    State(st): State<SyncState>,
) -> Result<Json<TrialBalanceResponse>, ApiError> {
    let store = st.store.lock().unwrap();
    let reports = Reports::new(store.connection());
    let tb = reports.trial_balance(None).map_err(query_err)?;

    let lines = tb
        .lines
        .into_iter()
        .map(|l| TrialBalanceLineDto {
            account_id: l.account_id,
            account_number: l.account_number,
            account_name: l.account_name,
            account_type: l.account_type,
            debit: l.debit,
            credit: l.credit,
        })
        .collect();
    Ok(Json(TrialBalanceResponse {
        as_of_date: tb.as_of_date,
        lines,
        total_debits: tb.total_debits,
        total_credits: tb.total_credits,
        is_balanced: tb.is_balanced,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use std::collections::HashMap;

    const TOKEN: &str = "tok-1";
    const ACTOR: &str = "user-1";

    fn tokens() -> HashMap<String, String> {
        HashMap::from([(TOKEN.to_string(), ACTOR.to_string())])
    }

    async fn serve(state: SyncState) -> String {
        let app = crate::sync::router(state);
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

    /// Seed two accounts and one balanced entry ($50.00 expense DR / asset CR)
    /// through the real command handlers (which project), then serve.
    async fn serve_seeded() -> (String, String, String) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let asset = mk_account(&mut store, "1000", AccountType::Asset);
        let expense = mk_account(&mut store, "5000", AccountType::Expense);

        EntryCommands::new(&mut store, "seed".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2026, 3, 4).unwrap(),
                memo: "supplies".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 5000, "USD"),
                    EntryLine::credit(&asset, 5000, "USD"),
                ],
                reference: Some("INV-1".to_string()),
                source: None,
            })
            .unwrap();

        let base = serve(SyncState::new(store, tokens())).await;
        (base, asset, expense)
    }

    #[tokio::test]
    async fn accounts_endpoint_returns_chart_with_balances() {
        let (base, asset, expense) = serve_seeded().await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/sync/accounts"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: AccountsResponse = resp.json().await.unwrap();

        // Both seeded accounts are present.
        assert_eq!(body.accounts.len(), 2);
        let by_id = |id: &str| body.accounts.iter().find(|a| a.id == id).unwrap();

        // Expense was debited $50.00 → positive (debit) balance.
        let e = by_id(&expense);
        assert_eq!(e.account_number, "5000");
        assert!(matches!(e.account_type, AccountType::Expense));
        assert!(e.is_active);
        assert_eq!(e.balance, 5000);

        // Asset was credited $50.00 → negative (credit) balance.
        let a = by_id(&asset);
        assert_eq!(a.balance, -5000);
    }

    #[tokio::test]
    async fn entries_endpoint_returns_journal_entries() {
        let (base, _asset, _expense) = serve_seeded().await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/sync/entries"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: EntriesResponse = resp.json().await.unwrap();

        assert_eq!(body.entries.len(), 1);
        let entry = &body.entries[0];
        assert_eq!(entry.memo, "supplies");
        assert_eq!(entry.reference.as_deref(), Some("INV-1"));
        assert_eq!(entry.total_amount, 5000);
        assert!(!entry.is_void);

        // Paging: limit=0 yields nothing.
        let empty: EntriesResponse = reqwest::Client::new()
            .get(format!("{base}/sync/entries?limit=0"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(empty.entries.is_empty());
    }

    #[tokio::test]
    async fn trial_balance_endpoint_nets_to_zero() {
        let (base, _asset, _expense) = serve_seeded().await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/sync/trial-balance"))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: TrialBalanceResponse = resp.json().await.unwrap();

        assert!(body.is_balanced);
        assert_eq!(body.total_debits, body.total_credits);
        assert_eq!(body.total_debits, 5000);
        // Two non-zero accounts, one debit line and one credit line.
        assert_eq!(body.lines.len(), 2);
        assert_eq!(body.lines.iter().filter(|l| l.debit.is_some()).count(), 1);
        assert_eq!(body.lines.iter().filter(|l| l.credit.is_some()).count(), 1);
    }

    #[tokio::test]
    async fn reads_require_bearer_token() {
        let (base, _asset, _expense) = serve_seeded().await;
        let http = reqwest::Client::new();
        for path in ["/sync/accounts", "/sync/entries", "/sync/trial-balance"] {
            let resp = http.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(
                resp.status(),
                reqwest::StatusCode::UNAUTHORIZED,
                "{path} should reject an unauthenticated request"
            );
        }
    }
}
