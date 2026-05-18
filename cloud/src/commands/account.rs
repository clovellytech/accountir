use accountir_core::events::types::{Event, EventAccountType};
use accountir_core::events::validation::validate_event;
use sqlx::{Acquire, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::store::event_store::{append_event, set_tenant};

pub struct CreateAccountInput {
    pub account_type: EventAccountType,
    pub account_number: String,
    pub name: String,
    pub currency: Option<String>,
    pub description: Option<String>,
}

pub async fn create_account(
    pool: &PgPool,
    company_id: Uuid,
    user_id: Uuid,
    input: CreateAccountInput,
) -> AppResult<Uuid> {
    let account_id = Uuid::new_v4();
    let event = Event::AccountCreated {
        account_id: account_id.to_string(),
        account_type: input.account_type,
        account_number: input.account_number,
        name: input.name,
        parent_id: None,
        currency: input.currency,
        description: input.description,
    };
    validate_event(&event)
        .map_err(|e| AppError::BadRequest(format!("invalid account: {e}")))?;

    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    if let Err(e) = append_event(&mut tx, company_id, user_id, &event).await {
        return Err(map_account_unique(e));
    }
    tx.commit().await?;
    Ok(account_id)
}

/// Append an AccountCreated event inside an already-active transaction.
/// Caller must have called set_tenant first.
pub async fn create_account_in_tx<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    user_id: Uuid,
    input: CreateAccountInput,
) -> AppResult<Uuid> {
    let account_id = Uuid::new_v4();
    let event = Event::AccountCreated {
        account_id: account_id.to_string(),
        account_type: input.account_type,
        account_number: input.account_number,
        name: input.name,
        parent_id: None,
        currency: input.currency,
        description: input.description,
    };
    validate_event(&event)
        .map_err(|e| AppError::BadRequest(format!("invalid account: {e}")))?;
    if let Err(e) = crate::store::event_store::append_event(tx, company_id, user_id, &event).await {
        return Err(map_account_unique(e));
    }
    Ok(account_id)
}

/// Find next free account number with the given numeric prefix range.
/// `prefix_start` is inclusive (e.g. 1000), `prefix_end` is exclusive (e.g. 2000).
/// Returns the lowest unused number, padded as 4-digit string.
pub async fn next_account_number<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    prefix_start: i32,
    prefix_end: i32,
) -> AppResult<String> {
    // Pull all numeric account numbers in range, find the smallest gap.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT account_number FROM accounts WHERE company_id = $1 AND account_number ~ '^[0-9]+$'",
    )
    .bind(company_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut used: std::collections::BTreeSet<i32> = rows
        .into_iter()
        .filter_map(|(s,)| s.parse::<i32>().ok())
        .filter(|n| *n >= prefix_start && *n < prefix_end)
        .collect();
    let mut candidate = prefix_start;
    while used.contains(&candidate) {
        candidate += 10;
        if candidate >= prefix_end {
            return Err(AppError::Conflict(format!(
                "no free account number in {prefix_start}-{prefix_end} range"
            )));
        }
    }
    used.insert(candidate);
    Ok(format!("{:04}", candidate))
}

/// Find or create the catch-all "Uncategorized" expense account at 9999.
pub async fn find_or_create_uncategorized<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    user_id: Uuid,
) -> AppResult<Uuid> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM accounts WHERE company_id = $1 AND account_number = '9999' AND is_active = true",
    )
    .bind(company_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((id,)) = row {
        return Ok(id);
    }
    create_account_in_tx(
        tx,
        company_id,
        user_id,
        CreateAccountInput {
            account_type: EventAccountType::Expense,
            account_number: "9999".to_string(),
            name: "Uncategorized".to_string(),
            currency: Some("USD".to_string()),
            description: Some("Catch-all for un-classified Plaid transactions".to_string()),
        },
    )
    .await
}

fn map_account_unique(e: crate::store::event_store::StoreError) -> AppError {
    use crate::store::event_store::StoreError;
    match e {
        StoreError::Database(sqlx::Error::Database(db_err))
            if db_err.code().as_deref() == Some("23505") =>
        {
            AppError::Conflict("account number already exists".into())
        }
        other => other.into(),
    }
}
