use accountir_core::events::types::{Event, EventAccountType};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::store::event_store::StoreError;

pub async fn apply<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    event_id: i64,
    event: &Event,
) -> Result<(), StoreError> {
    match event {
        Event::AccountCreated {
            account_id,
            account_type,
            account_number,
            name,
            parent_id,
            currency,
            description,
        } => {
            let id = Uuid::parse_str(account_id)
                .map_err(|e| StoreError::Payload(format!("account_id not uuid: {e}")))?;
            let parent = parent_id
                .as_ref()
                .map(|p| Uuid::parse_str(p))
                .transpose()
                .map_err(|e| StoreError::Payload(format!("parent_id not uuid: {e}")))?;
            sqlx::query(
                r#"
                INSERT INTO accounts
                    (id, company_id, account_type, account_number, name, parent_id,
                     currency, description, is_active, created_at_event)
                VALUES ($1, $2, $3::account_type, $4, $5, $6, $7, $8, true, $9)
                "#,
            )
            .bind(id)
            .bind(company_id)
            .bind(account_type_str(account_type))
            .bind(account_number)
            .bind(name)
            .bind(parent)
            .bind(currency.as_deref())
            .bind(description.as_deref())
            .bind(event_id)
            .execute(&mut **tx)
            .await?;
        }
        Event::JournalEntryPosted {
            entry_id,
            date,
            memo,
            lines,
            reference,
            source,
        } => {
            let entry_uuid = Uuid::parse_str(entry_id)
                .map_err(|e| StoreError::Payload(format!("entry_id not uuid: {e}")))?;
            let source_str = source
                .as_ref()
                .map(|s| match s {
                    accountir_core::events::types::JournalEntrySource::Manual => "manual",
                    accountir_core::events::types::JournalEntrySource::Import => "import",
                    accountir_core::events::types::JournalEntrySource::Recurring => "recurring",
                    accountir_core::events::types::JournalEntrySource::System => "system",
                    accountir_core::events::types::JournalEntrySource::Plaid => "plaid",
                });
            sqlx::query(
                r#"
                INSERT INTO journal_entries
                    (id, company_id, date, memo, reference, source, posted_at_event)
                VALUES ($1, $2, $3, $4, $5, $6::journal_entry_source, $7)
                "#,
            )
            .bind(entry_uuid)
            .bind(company_id)
            .bind(date)
            .bind(memo)
            .bind(reference.as_deref())
            .bind(source_str)
            .bind(event_id)
            .execute(&mut **tx)
            .await?;

            for line in lines {
                let line_uuid = Uuid::parse_str(&line.line_id)
                    .map_err(|e| StoreError::Payload(format!("line_id not uuid: {e}")))?;
                let acct_uuid = Uuid::parse_str(&line.account_id)
                    .map_err(|e| StoreError::Payload(format!("account_id not uuid: {e}")))?;
                sqlx::query(
                    r#"
                    INSERT INTO journal_lines
                        (id, company_id, entry_id, account_id, amount, currency,
                         exchange_rate, memo)
                    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                    "#,
                )
                .bind(line_uuid)
                .bind(company_id)
                .bind(entry_uuid)
                .bind(acct_uuid)
                .bind(line.amount)
                .bind(&line.currency)
                .bind(line.exchange_rate)
                .bind(line.memo.as_deref())
                .execute(&mut **tx)
                .await?;
            }
        }
        Event::JournalEntryVoided { entry_id, .. } => {
            let id = Uuid::parse_str(entry_id)
                .map_err(|e| StoreError::Payload(format!("entry_id not uuid: {e}")))?;
            sqlx::query("UPDATE journal_entries SET is_void = true WHERE id = $1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
            let _ = event_id;
        }
        Event::JournalEntryUnvoided { entry_id, .. } => {
            let id = Uuid::parse_str(entry_id)
                .map_err(|e| StoreError::Payload(format!("entry_id not uuid: {e}")))?;
            sqlx::query("UPDATE journal_entries SET is_void = false WHERE id = $1")
                .bind(id)
                .execute(&mut **tx)
                .await?;
        }
        Event::JournalLineReassigned {
            line_id,
            new_account_id,
            ..
        } => {
            let line_uuid = Uuid::parse_str(line_id)
                .map_err(|e| StoreError::Payload(format!("line_id not uuid: {e}")))?;
            let new_acct = Uuid::parse_str(new_account_id)
                .map_err(|e| StoreError::Payload(format!("new_account_id not uuid: {e}")))?;
            sqlx::query("UPDATE journal_lines SET account_id = $1 WHERE id = $2")
                .bind(new_acct)
                .bind(line_uuid)
                .execute(&mut **tx)
                .await?;
        }
        // Other event types: not needed for v1 webapp; cloud Plaid path uses its own writes.
        _ => {}
    }
    Ok(())
}

fn account_type_str(t: &EventAccountType) -> &'static str {
    match t {
        EventAccountType::Asset => "asset",
        EventAccountType::Liability => "liability",
        EventAccountType::Equity => "equity",
        EventAccountType::Revenue => "revenue",
        EventAccountType::Expense => "expense",
    }
}
