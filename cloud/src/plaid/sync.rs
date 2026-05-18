//! Reusable Plaid sync: pull /transactions/sync, import each added txn as a
//! journal entry against the mapped local account vs. Uncategorized.

use std::collections::HashMap;

use accountir_core::events::types::{Event, EventAccountType, JournalEntrySource, JournalLineData};
use chrono::NaiveDate;
use sqlx::Acquire;
use uuid::Uuid;

use crate::commands::account::{
    create_account_in_tx, find_or_create_uncategorized, next_account_number, CreateAccountInput,
};
use crate::error::{AppError, AppResult};
use crate::http::AppState;
use crate::plaid::client::PlaidClient;
use crate::plaid::crypto::TokenCipher;
use crate::store::event_store::{append_event, set_tenant};

/// Backfill local accounts + plaid_local_accounts mappings for an item that
/// was linked before auto-provisioning existed. Also resets sync_cursor so
/// the next sync re-pulls everything.
/// Returns the number of accounts provisioned (skipping ones already mapped).
pub async fn provision_existing_item(
    state: &AppState,
    company_id: Uuid,
    user_id: Uuid,
    item_uuid: Uuid,
) -> AppResult<u32> {
    // Read access token + institution name.
    let (access_token, institution_name) = {
        let mut conn = state.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        set_tenant(&mut tx, company_id).await?;
        let row: Option<(Vec<u8>, Vec<u8>, String)> = sqlx::query_as(
            "SELECT access_token_ciphertext, access_token_nonce, institution_name FROM plaid_items WHERE id = $1",
        )
        .bind(item_uuid)
        .fetch_optional(&mut *tx)
        .await?;
        let (ct, nonce, inst) = row.ok_or(AppError::NotFound)?;
        tx.commit().await?;
        let cipher = TokenCipher::new(&state.config.plaid.token_enc_key);
        let token = cipher.decrypt(&ct, &nonce).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
        (token, inst)
    };

    // Pull account metadata from Plaid.
    let plaid = PlaidClient::new(state.config.plaid.clone());
    let plaid_accounts = plaid
        .accounts_get(&access_token)
        .await
        .map_err(|e| AppError::BadRequest(format!("plaid: {e}")))?;

    // Provision local accounts + mapping rows in one tx.
    let mut conn = state.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let _ = find_or_create_uncategorized(&mut tx, company_id, user_id).await?;

    // Existing mappings, so we don't duplicate.
    let existing: HashMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts WHERE item_id = $1 AND local_account_id IS NOT NULL",
    )
    .bind(item_uuid)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();

    let mut provisioned = 0u32;
    for acct in &plaid_accounts {
        let plaid_account_id = match acct.get("account_id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if existing.contains_key(&plaid_account_id) {
            continue;
        }
        let name = acct
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();
        let mask = acct.get("mask").and_then(|v| v.as_str()).map(str::to_string);
        let typ_str = acct.get("type").and_then(|v| v.as_str()).unwrap_or("depository");

        let (acct_type, num_start, num_end) = match typ_str {
            "credit" | "loan" => (EventAccountType::Liability, 2000, 3000),
            _ => (EventAccountType::Asset, 1000, 2000),
        };
        let acct_num = next_account_number(&mut tx, company_id, num_start, num_end).await?;
        let display_name = match mask.as_deref() {
            Some(m) => format!("{}: {} ***{}", institution_name, name, m),
            None => format!("{}: {}", institution_name, name),
        };
        let local_id = create_account_in_tx(
            &mut tx,
            company_id,
            user_id,
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
        .bind(item_uuid)
        .bind(&plaid_account_id)
        .bind(&name)
        .bind(typ_str)
        .bind(mask.as_deref())
        .bind(local_id)
        .execute(&mut *tx)
        .await?;
        provisioned += 1;
    }

    // Reset cursor so the next sync re-pulls all transactions.
    sqlx::query("UPDATE plaid_items SET sync_cursor = NULL WHERE id = $1")
        .bind(item_uuid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(provisioned)
}

/// Run sync for an item. Returns (imported, skipped).
pub async fn run_sync_for_item(
    state: &AppState,
    company_id: Uuid,
    user_id: Uuid,
    item_uuid: Uuid,
) -> AppResult<(u32, u32)> {
    // Load access token + cursor
    let (access_token, cursor) = {
        let mut conn = state.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        set_tenant(&mut tx, company_id).await?;
        let row: Option<(Vec<u8>, Vec<u8>, Option<String>)> = sqlx::query_as(
            "SELECT access_token_ciphertext, access_token_nonce, sync_cursor
             FROM plaid_items WHERE id = $1 AND status = 'active'",
        )
        .bind(item_uuid)
        .fetch_optional(&mut *tx)
        .await?;
        let (ct, nonce, cursor) = row.ok_or(AppError::NotFound)?;
        tx.commit().await?;
        let cipher = TokenCipher::new(&state.config.plaid.token_enc_key);
        (cipher.decrypt(&ct, &nonce).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?, cursor)
    };

    // Pull from Plaid (paginate has_more)
    let plaid = PlaidClient::new(state.config.plaid.clone());
    let mut all_added: Vec<serde_json::Value> = Vec::new();
    let mut next_cursor = cursor;
    let mut has_more = true;
    while has_more {
        let result = plaid
            .transactions_sync(&access_token, next_cursor.as_deref())
            .await
            .map_err(|e| AppError::BadRequest(format!("plaid: {e}")))?;
        all_added.extend(result.added);
        next_cursor = Some(result.next_cursor);
        has_more = result.has_more;
    }

    // Import added txns as journal entries
    let mut conn = state.pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let uncategorized_id = find_or_create_uncategorized(&mut tx, company_id, user_id).await?;

    let mappings: HashMap<String, Uuid> = sqlx::query_as::<_, (String, Uuid)>(
        "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts WHERE item_id = $1 AND local_account_id IS NOT NULL",
    )
    .bind(item_uuid)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();

    let mut imported = 0u32;
    let mut skipped = 0u32;

    for txn in &all_added {
        let pending = txn.get("pending").and_then(|v| v.as_bool()).unwrap_or(false);
        if pending { skipped += 1; continue; }
        let plaid_acct_id = match txn.get("account_id").and_then(|v| v.as_str()) {
            Some(s) => s, None => { skipped += 1; continue; }
        };
        let plaid_txn_id = match txn.get("transaction_id").and_then(|v| v.as_str()) {
            Some(s) => s, None => { skipped += 1; continue; }
        };
        let local_account_id = match mappings.get(plaid_acct_id) {
            Some(id) => *id, None => { skipped += 1; continue; }
        };

        let already: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM plaid_imported_transactions WHERE company_id = $1 AND plaid_transaction_id = $2",
        )
        .bind(company_id)
        .bind(plaid_txn_id)
        .fetch_optional(&mut *tx)
        .await?;
        if already.is_some() { skipped += 1; continue; }

        let amount_dollars = txn.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let amount_cents = (amount_dollars * 100.0).round() as i64;
        if amount_cents == 0 { skipped += 1; continue; }
        let date_str = txn.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
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

        let bank_line = JournalLineData {
            line_id: Uuid::new_v4().to_string(),
            account_id: local_account_id.to_string(),
            amount: -amount_cents,
            currency: currency.clone(),
            exchange_rate: None,
            memo: None,
        };
        let counter_line = JournalLineData {
            line_id: Uuid::new_v4().to_string(),
            account_id: uncategorized_id.to_string(),
            amount: amount_cents,
            currency: currency.clone(),
            exchange_rate: None,
            memo: None,
        };
        let entry_id = Uuid::new_v4();
        let event = Event::JournalEntryPosted {
            entry_id: entry_id.to_string(),
            date,
            memo,
            lines: vec![bank_line, counter_line],
            reference: Some(plaid_txn_id.to_string()),
            source: Some(JournalEntrySource::Plaid),
        };
        append_event(&mut tx, company_id, user_id, &event).await?;

        sqlx::query(
            "INSERT INTO plaid_imported_transactions (company_id, plaid_transaction_id, item_id, entry_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(company_id)
        .bind(plaid_txn_id)
        .bind(item_uuid)
        .bind(entry_id)
        .execute(&mut *tx)
        .await?;

        imported += 1;
    }

    sqlx::query("UPDATE plaid_items SET sync_cursor = $1, last_synced_at = now() WHERE id = $2")
        .bind(&next_cursor)
        .bind(item_uuid)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok((imported, skipped))
}
