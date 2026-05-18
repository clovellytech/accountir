use accountir_core::events::types::{Event, JournalEntrySource, JournalLineData};
use accountir_core::events::validation::validate_event;
use chrono::NaiveDate;
use sqlx::{Acquire, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::store::event_store::{append_event, set_tenant};

pub struct PostEntryInput {
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub lines: Vec<EntryLineInput>,
}

pub struct EntryLineInput {
    pub account_id: Uuid,
    pub amount: i64,
    pub currency: String,
    pub memo: Option<String>,
}

pub async fn post_entry(
    pool: &PgPool,
    company_id: Uuid,
    user_id: Uuid,
    input: PostEntryInput,
) -> AppResult<Uuid> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let id = post_entry_in_tx(
        &mut tx,
        company_id,
        user_id,
        input,
        JournalEntrySource::Manual,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// Append a JournalEntryPosted event in an active transaction. Caller must have
/// invoked `set_tenant` first. Lets callers chain other DB writes against the
/// new entry's id inside the same atomic step.
pub async fn post_entry_in_tx<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    user_id: Uuid,
    input: PostEntryInput,
    source: JournalEntrySource,
) -> AppResult<Uuid> {
    if input.lines.len() < 2 {
        return Err(AppError::BadRequest(
            "an entry needs at least two lines".into(),
        ));
    }
    let sum: i64 = input.lines.iter().map(|l| l.amount).sum();
    if sum != 0 {
        return Err(AppError::BadRequest(format!(
            "lines must balance to zero (got {sum})"
        )));
    }

    let entry_id = Uuid::new_v4();
    let lines: Vec<JournalLineData> = input
        .lines
        .into_iter()
        .map(|l| JournalLineData {
            line_id: Uuid::new_v4().to_string(),
            account_id: l.account_id.to_string(),
            amount: l.amount,
            currency: l.currency,
            exchange_rate: None,
            memo: l.memo,
        })
        .collect();

    let event = Event::JournalEntryPosted {
        entry_id: entry_id.to_string(),
        date: input.date,
        memo: input.memo,
        lines,
        reference: input.reference,
        source: Some(source),
    };
    validate_event(&event)
        .map_err(|e| AppError::BadRequest(format!("invalid entry: {e}")))?;

    let _ = company_id;
    append_event(tx, company_id, user_id, &event).await?;
    Ok(entry_id)
}
