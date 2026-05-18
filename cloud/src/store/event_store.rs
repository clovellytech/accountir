use accountir_core::events::payload::compute_event_hash;
use accountir_core::events::types::Event;
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::error::AppError;
use crate::store::projections;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("hash collision (event already exists)")]
    DuplicateHash,
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("payload error: {0}")]
    Payload(String),
}

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::DuplicateHash => AppError::Conflict("duplicate event".into()),
            StoreError::Database(e) => AppError::Database(e),
            StoreError::Payload(m) => AppError::Internal(anyhow::anyhow!(m)),
        }
    }
}

/// Append an event for the current tenant inside an existing transaction.
/// Caller is responsible for setting `app.company_id` (RLS) before calling this.
/// Returns the new event's row id.
pub async fn append_event<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    user_id: Uuid,
    event: &Event,
) -> Result<i64, StoreError> {
    let timestamp: DateTime<Utc> = Utc::now();
    let timestamp_str = timestamp.to_rfc3339();
    let hash = compute_event_hash(event, &timestamp_str, &user_id.to_string())
        .map_err(|e| StoreError::Payload(e.to_string()))?;
    let payload = serde_json::to_value(event)
        .map_err(|e| StoreError::Payload(e.to_string()))?;
    let event_type = event.event_type();

    // Per-tenant monotonic sequence. Locks via UPDATE/INSERT pattern is overkill;
    // SELECT MAX inside a transaction is fine because we're in REPEATABLE READ-equivalent
    // territory under the SET LOCAL company_id. UNIQUE(company_id, company_seq_id) guards.
    let next_seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(company_seq_id), 0) + 1 FROM events WHERE company_id = $1",
    )
    .bind(company_id)
    .fetch_one(&mut **tx)
    .await?;

    let row: Result<(i64,), sqlx::Error> = sqlx::query_as(
        r#"
        INSERT INTO events
            (company_id, company_seq_id, event_type, payload, hash, user_id, timestamp)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#,
    )
    .bind(company_id)
    .bind(next_seq)
    .bind(event_type)
    .bind(payload)
    .bind(&hash[..])
    .bind(user_id)
    .bind(timestamp)
    .fetch_one(&mut **tx)
    .await;

    let event_id = match row {
        Ok((id,)) => id,
        Err(sqlx::Error::Database(db_err)) if db_err.code().as_deref() == Some("23505") => {
            return Err(StoreError::DuplicateHash);
        }
        Err(e) => return Err(e.into()),
    };

    projections::apply(tx, company_id, event_id, event).await?;

    Ok(event_id)
}

/// Set the per-tx tenant scoping that RLS reads from.
pub async fn set_tenant<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT set_config('app.company_id', $1, true)")
        .bind(company_id.to_string())
        .execute(&mut **tx)
        .await?;
    Ok(())
}
