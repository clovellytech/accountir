use axum::{extract::State, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Acquire;
use uuid::Uuid;

use crate::{
    auth::AuthenticatedUser,
    error::{AppError, AppResult},
    http::AppState,
    plaid::{client::PlaidClient, crypto::TokenCipher, PlaidError},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plaid/create-link-token", post(create_link_token))
        .route("/plaid/exchange-token", post(exchange_token))
        .route("/plaid/sync", post(sync))
        .route("/plaid/balances", post(balances))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve which company this user is acting as.
/// For now: the user's first membership (alphabetical by created_at).
/// A future enhancement is an X-Company-Id header.
async fn resolve_company_id(state: &AppState, user_id: Uuid) -> AppResult<Uuid> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT company_id FROM memberships WHERE user_id = $1 ORDER BY created_at ASC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;

    match row {
        Some((id,)) => Ok(id),
        None => Err(AppError::Forbidden),
    }
}

fn plaid_client(state: &AppState) -> PlaidClient {
    PlaidClient::new(state.config.plaid.clone())
}

fn token_cipher(state: &AppState) -> TokenCipher {
    TokenCipher::new(&state.config.plaid.token_enc_key)
}

impl From<PlaidError> for AppError {
    fn from(e: PlaidError) -> Self {
        match e {
            PlaidError::Api {
                status,
                error_code,
                error_message,
            } => {
                tracing::warn!(status, %error_code, %error_message, "plaid api error");
                AppError::BadRequest(format!("plaid: {error_code}: {error_message}"))
            }
            other => {
                tracing::error!(error = %other, "plaid client error");
                AppError::Internal(anyhow::anyhow!(other))
            }
        }
    }
}

impl From<crate::plaid::crypto::CryptoError> for AppError {
    fn from(e: crate::plaid::crypto::CryptoError) -> Self {
        tracing::error!(error = %e, "token crypto error");
        AppError::Internal(anyhow::anyhow!(e))
    }
}

// ---------------------------------------------------------------------------
// POST /plaid/create-link-token
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct LinkTokenResponse {
    link_token: String,
}

async fn create_link_token(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> AppResult<Json<LinkTokenResponse>> {
    let token = plaid_client(&state)
        .create_link_token(&user.id.to_string())
        .await?;
    Ok(Json(LinkTokenResponse { link_token: token }))
}

// ---------------------------------------------------------------------------
// POST /plaid/exchange-token
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ExchangeTokenRequest {
    public_token: String,
    institution: InstitutionInfo,
    #[serde(default)]
    accounts: Vec<LinkedAccountInfo>,
}

#[derive(Deserialize)]
struct InstitutionInfo {
    institution_id: String,
    name: String,
}

#[derive(Deserialize, Clone)]
struct LinkedAccountInfo {
    account_id: String,
    name: String,
    #[serde(rename = "type", default)]
    account_type: Option<String>,
    #[serde(default)]
    mask: Option<String>,
}

#[derive(Serialize)]
struct ExchangeTokenResponse {
    item_id: String,
}

async fn exchange_token(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<ExchangeTokenRequest>,
) -> AppResult<Json<ExchangeTokenResponse>> {
    let company_id = resolve_company_id(&state, user.id).await?;

    let exchange = plaid_client(&state)
        .exchange_public_token(&req.public_token)
        .await?;

    let (ciphertext, nonce) = token_cipher(&state).encrypt(&exchange.access_token)?;

    let our_id = Uuid::new_v4();

    let mut conn = state.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT set_config('app.company_id', $1, true)")
        .bind(company_id.to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO plaid_items
            (id, company_id, plaid_item_id, access_token_ciphertext, access_token_nonce,
             institution_name, institution_id, status)
        VALUES ($1, $2, $3, $4, $5, $6, $7, 'active')
        "#,
    )
    .bind(our_id)
    .bind(company_id)
    .bind(&exchange.item_id)
    .bind(&ciphertext)
    .bind(&nonce[..])
    .bind(&req.institution.name)
    .bind(&req.institution.institution_id)
    .execute(&mut *tx)
    .await?;

    // Auto-provision: for each Plaid account, create a local account and a mapping row.
    // Asset for depository, Liability for credit cards, Asset otherwise.
    use crate::commands::account::{
        create_account_in_tx, find_or_create_uncategorized, next_account_number, CreateAccountInput,
    };
    use accountir_core::events::types::EventAccountType;
    let _ = find_or_create_uncategorized(&mut tx, company_id, user.id).await?;

    for plaid_acct in &req.accounts {
        let typ_str = plaid_acct.account_type.as_deref().unwrap_or("depository");
        let (acct_type, num_start, num_end) = match typ_str {
            "credit" | "loan" => (EventAccountType::Liability, 2000, 3000),
            _ => (EventAccountType::Asset, 1000, 2000),
        };
        let acct_num = next_account_number(&mut tx, company_id, num_start, num_end).await?;
        let display_name = match plaid_acct.mask.as_deref() {
            Some(m) => format!("{}: {} ***{}", req.institution.name, plaid_acct.name, m),
            None => format!("{}: {}", req.institution.name, plaid_acct.name),
        };
        let local_id = create_account_in_tx(
            &mut tx,
            company_id,
            user.id,
            CreateAccountInput {
                account_type: acct_type,
                account_number: acct_num,
                name: display_name,
                currency: Some("USD".to_string()),
                description: None,
            },
        )
        .await?;
        sqlx::query(
            r#"
            INSERT INTO plaid_local_accounts
                (company_id, item_id, plaid_account_id, name, account_type, mask, local_account_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (item_id, plaid_account_id) DO UPDATE
              SET local_account_id = EXCLUDED.local_account_id
            "#,
        )
        .bind(company_id)
        .bind(our_id)
        .bind(&plaid_acct.account_id)
        .bind(&plaid_acct.name)
        .bind(typ_str)
        .bind(plaid_acct.mask.as_deref())
        .bind(local_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    Ok(Json(ExchangeTokenResponse {
        item_id: our_id.to_string(),
    }))
}

// ---------------------------------------------------------------------------
// POST /plaid/sync
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SyncRequest {
    /// Our DB UUID for the plaid_items row (returned from /plaid/exchange-token).
    item_id: String,
}

#[derive(Serialize)]
struct SyncResponse {
    added: Vec<Value>,
    modified: Vec<Value>,
    removed: Vec<Value>,
    has_more: bool,
    /// Newly created journal entries from this sync.
    imported_count: u32,
    /// Transactions skipped (pending, unmapped, or duplicate).
    skipped_count: u32,
}

async fn sync(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<SyncRequest>,
) -> AppResult<Json<SyncResponse>> {
    let company_id = resolve_company_id(&state, user.id).await?;
    let item_uuid = Uuid::parse_str(&req.item_id)
        .map_err(|_| AppError::BadRequest("item_id must be a uuid".into()))?;

    let mut conn = state.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT set_config('app.company_id', $1, true)")
        .bind(company_id.to_string())
        .execute(&mut *tx)
        .await?;

    let row: Option<(Vec<u8>, Vec<u8>, Option<String>)> = sqlx::query_as(
        "SELECT access_token_ciphertext, access_token_nonce, sync_cursor
         FROM plaid_items WHERE id = $1 AND status = 'active'",
    )
    .bind(item_uuid)
    .fetch_optional(&mut *tx)
    .await?;
    let (ciphertext, nonce, cursor) = row.ok_or(AppError::NotFound)?;

    let access_token = token_cipher(&state).decrypt(&ciphertext, &nonce)?;

    // Drop the tx so we don't hold a connection during the Plaid round-trip.
    tx.commit().await?;
    drop(conn);

    let mut all_added = Vec::new();
    let mut all_modified = Vec::new();
    let mut all_removed = Vec::new();
    let mut next_cursor = cursor;
    let mut has_more = true;

    while has_more {
        let result = plaid_client(&state)
            .transactions_sync(&access_token, next_cursor.as_deref())
            .await?;
        all_added.extend(result.added);
        all_modified.extend(result.modified);
        all_removed.extend(result.removed);
        next_cursor = Some(result.next_cursor);
        has_more = result.has_more;
    }

    // Import each added txn as a journal entry, persist cursor.
    let mut conn = state.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT set_config('app.company_id', $1, true)")
        .bind(company_id.to_string())
        .execute(&mut *tx)
        .await?;

    use crate::commands::account::find_or_create_uncategorized;
    let uncategorized_id = find_or_create_uncategorized(&mut tx, company_id, user.id).await?;

    // Load mapping from plaid_account_id → local_account_id.
    let mappings: std::collections::HashMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts WHERE item_id = $1 AND local_account_id IS NOT NULL",
    )
    .bind(item_uuid)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();

    let mut imported_count: u32 = 0;
    let mut skipped_count: u32 = 0;

    for txn in &all_added {
        let pending = txn.get("pending").and_then(|v| v.as_bool()).unwrap_or(false);
        if pending {
            skipped_count += 1;
            continue;
        }
        let plaid_acct_id = match txn.get("account_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => { skipped_count += 1; continue; }
        };
        let plaid_txn_id = match txn.get("transaction_id").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => { skipped_count += 1; continue; }
        };
        let local_account_id = match mappings.get(plaid_acct_id) {
            Some(id) => *id,
            None => { skipped_count += 1; continue; }
        };

        // Dedup
        let already: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM plaid_imported_transactions WHERE company_id = $1 AND plaid_transaction_id = $2",
        )
        .bind(company_id)
        .bind(plaid_txn_id)
        .fetch_optional(&mut *tx)
        .await?;
        if already.is_some() {
            skipped_count += 1;
            continue;
        }

        let amount_dollars = txn.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let amount_cents = (amount_dollars * 100.0).round() as i64;
        if amount_cents == 0 { skipped_count += 1; continue; }
        let date_str = txn.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
            .unwrap_or_else(|_| chrono::Utc::now().date_naive());
        let memo = txn
            .get("merchant_name").and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| txn.get("name").and_then(|v| v.as_str()))
            .unwrap_or("Plaid transaction")
            .to_string();
        let currency = txn
            .get("iso_currency_code").and_then(|v| v.as_str())
            .unwrap_or("USD").to_string();

        // Sign convention: Plaid amount > 0 means money LEFT the account.
        // → bank line is CREDIT (negative in our convention)
        // → counterpart (Uncategorized) is DEBIT (positive)
        let bank_line = accountir_core::events::types::JournalLineData {
            line_id: Uuid::new_v4().to_string(),
            account_id: local_account_id.to_string(),
            amount: -amount_cents,
            currency: currency.clone(),
            exchange_rate: None,
            memo: None,
        };
        let counter_line = accountir_core::events::types::JournalLineData {
            line_id: Uuid::new_v4().to_string(),
            account_id: uncategorized_id.to_string(),
            amount: amount_cents,
            currency: currency.clone(),
            exchange_rate: None,
            memo: None,
        };
        let entry_id = Uuid::new_v4();
        let event = accountir_core::events::types::Event::JournalEntryPosted {
            entry_id: entry_id.to_string(),
            date,
            memo,
            lines: vec![bank_line, counter_line],
            reference: Some(plaid_txn_id.to_string()),
            source: Some(accountir_core::events::types::JournalEntrySource::Plaid),
        };
        crate::store::event_store::append_event(&mut tx, company_id, user.id, &event).await?;

        sqlx::query(
            "INSERT INTO plaid_imported_transactions (company_id, plaid_transaction_id, item_id, entry_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(company_id)
        .bind(plaid_txn_id)
        .bind(item_uuid)
        .bind(entry_id)
        .execute(&mut *tx)
        .await?;

        imported_count += 1;
    }

    sqlx::query("UPDATE plaid_items SET sync_cursor = $1, last_synced_at = now() WHERE id = $2")
        .bind(&next_cursor)
        .bind(item_uuid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Json(SyncResponse {
        added: all_added,
        modified: all_modified,
        removed: all_removed,
        has_more: false,
        imported_count,
        skipped_count,
    }))
}

// ---------------------------------------------------------------------------
// POST /plaid/balances
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct BalancesResponse {
    accounts: Vec<Value>,
}

async fn balances(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(req): Json<SyncRequest>,
) -> AppResult<Json<BalancesResponse>> {
    let company_id = resolve_company_id(&state, user.id).await?;
    let item_uuid = Uuid::parse_str(&req.item_id)
        .map_err(|_| AppError::BadRequest("item_id must be a uuid".into()))?;

    let mut conn = state.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT set_config('app.company_id', $1, true)")
        .bind(company_id.to_string())
        .execute(&mut *tx)
        .await?;
    let row: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT access_token_ciphertext, access_token_nonce
         FROM plaid_items WHERE id = $1 AND status = 'active'",
    )
    .bind(item_uuid)
    .fetch_optional(&mut *tx)
    .await?;
    let (ciphertext, nonce) = row.ok_or(AppError::NotFound)?;
    tx.commit().await?;
    drop(conn);

    let access_token = token_cipher(&state).decrypt(&ciphertext, &nonce)?;
    let accounts = plaid_client(&state).accounts_balance_get(&access_token).await?;

    Ok(Json(BalancesResponse { accounts }))
}
