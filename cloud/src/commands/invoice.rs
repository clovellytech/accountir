//! Invoice commands: create draft, issue (post to ledger), record payment, void.
//!
//! Posting model (US accrual):
//!   Issue:   DR Accounts Receivable (total),
//!            CR Revenue (per-line subtotal),
//!            CR Sales Tax Payable (sum of line tax)
//!   Pay:     DR Cash/Bank (paid amount),
//!            CR Accounts Receivable (paid amount)

use accountir_core::events::types::{EventAccountType, JournalEntrySource};
use chrono::{Local, NaiveDate};
use rand::{distributions::Alphanumeric, Rng};
use sqlx::{Acquire, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::commands::account::{create_account_in_tx, CreateAccountInput};
use crate::commands::entry::{post_entry_in_tx, EntryLineInput, PostEntryInput};
use crate::error::{AppError, AppResult};
use crate::store::event_store::set_tenant;

pub const AR_ACCOUNT_NUMBER: &str = "1200";
pub const SALES_TAX_ACCOUNT_NUMBER: &str = "2200";

pub struct DraftInvoiceInput {
    pub customer_id: Uuid,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    pub terms: String,
    pub memo: Option<String>,
    pub customer_notes: Option<String>,
    pub lines: Vec<DraftLine>,
}

pub struct DraftLine {
    pub description: String,
    pub quantity: f64,
    pub unit_price_cents: i64,
    pub tax_rate_pct: f64,
    pub revenue_account_id: Uuid,
}

pub async fn create_draft(
    pool: &PgPool,
    company_id: Uuid,
    input: DraftInvoiceInput,
) -> AppResult<Uuid> {
    if input.lines.is_empty() {
        return Err(AppError::BadRequest("invoice needs at least one line".into()));
    }

    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let invoice_id = Uuid::new_v4();
    let invoice_number = next_invoice_number(&mut tx, company_id).await?;
    let public_token: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();

    let mut subtotal: i64 = 0;
    let mut tax_sum: i64 = 0;
    let mut prepared: Vec<(Uuid, i32, String, f64, i64, i64, f64, i64, Uuid)> = Vec::new();
    for (idx, l) in input.lines.iter().enumerate() {
        let line_amount = (l.quantity * l.unit_price_cents as f64).round() as i64;
        let line_tax = ((line_amount as f64) * (l.tax_rate_pct / 100.0)).round() as i64;
        subtotal += line_amount;
        tax_sum += line_tax;
        prepared.push((
            Uuid::new_v4(),
            idx as i32,
            l.description.clone(),
            l.quantity,
            l.unit_price_cents,
            line_amount,
            l.tax_rate_pct,
            line_tax,
            l.revenue_account_id,
        ));
    }
    let total = subtotal + tax_sum;

    sqlx::query(
        r#"
        INSERT INTO invoices
            (id, company_id, customer_id, invoice_number, status, issue_date, due_date,
             terms, currency, subtotal_cents, tax_cents, total_cents, paid_cents,
             memo, customer_notes, public_token)
        VALUES ($1, $2, $3, $4, 'draft', $5, $6, $7, 'USD', $8, $9, $10, 0, $11, $12, $13)
        "#,
    )
    .bind(invoice_id)
    .bind(company_id)
    .bind(input.customer_id)
    .bind(&invoice_number)
    .bind(input.issue_date)
    .bind(input.due_date)
    .bind(&input.terms)
    .bind(subtotal)
    .bind(tax_sum)
    .bind(total)
    .bind(&input.memo)
    .bind(&input.customer_notes)
    .bind(&public_token)
    .execute(&mut *tx)
    .await?;

    for (lid, ord, desc, qty, unit, amt, tax_rate, tax, acct) in prepared {
        sqlx::query(
            r#"
            INSERT INTO invoice_lines
                (id, company_id, invoice_id, sort_order, description, quantity,
                 unit_price_cents, amount_cents, tax_rate_pct, tax_cents, revenue_account_id)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(lid)
        .bind(company_id)
        .bind(invoice_id)
        .bind(ord)
        .bind(&desc)
        .bind(qty)
        .bind(unit)
        .bind(amt)
        .bind(tax_rate)
        .bind(tax)
        .bind(acct)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(invoice_id)
}

/// Post the invoice to the ledger and flip status to 'sent'.
pub async fn issue_invoice(
    pool: &PgPool,
    company_id: Uuid,
    user_id: Uuid,
    invoice_id: Uuid,
) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let inv: Option<(String, String, NaiveDate, i64, i64, i64)> = sqlx::query_as(
        "SELECT status::text, invoice_number, issue_date, subtotal_cents, tax_cents, total_cents
         FROM invoices WHERE id = $1",
    )
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (status, number, issue_date, subtotal, tax, total) =
        inv.ok_or(AppError::NotFound)?;
    if status != "draft" {
        return Err(AppError::Conflict(format!(
            "invoice is {status}; only drafts can be issued"
        )));
    }

    let ar_id = find_or_create_ar(&mut tx, company_id, user_id).await?;
    let tax_payable_id = if tax > 0 {
        Some(find_or_create_sales_tax(&mut tx, company_id, user_id).await?)
    } else {
        None
    };

    // Aggregate revenue per account.
    let line_rows: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT revenue_account_id, amount_cents FROM invoice_lines WHERE invoice_id = $1",
    )
    .bind(invoice_id)
    .fetch_all(&mut *tx)
    .await?;

    let mut acct_totals: std::collections::BTreeMap<Uuid, i64> = std::collections::BTreeMap::new();
    for (acct, amt) in line_rows {
        *acct_totals.entry(acct).or_insert(0) += amt;
    }
    let revenue_sum: i64 = acct_totals.values().sum();
    if revenue_sum != subtotal {
        return Err(AppError::Internal(anyhow::anyhow!(
            "invoice subtotal {subtotal} != sum of line amounts {revenue_sum}"
        )));
    }

    let mut lines = Vec::with_capacity(2 + acct_totals.len());
    lines.push(EntryLineInput {
        account_id: ar_id,
        amount: total,
        currency: "USD".into(),
        memo: Some(format!("Invoice {number}")),
    });
    for (acct, amt) in acct_totals {
        lines.push(EntryLineInput {
            account_id: acct,
            amount: -amt,
            currency: "USD".into(),
            memo: Some(format!("Invoice {number}")),
        });
    }
    if let Some(tid) = tax_payable_id {
        lines.push(EntryLineInput {
            account_id: tid,
            amount: -tax,
            currency: "USD".into(),
            memo: Some(format!("Invoice {number} sales tax")),
        });
    }

    let entry_id = post_entry_in_tx(
        &mut tx,
        company_id,
        user_id,
        PostEntryInput {
            date: issue_date,
            memo: format!("Invoice {number}"),
            reference: Some(number.clone()),
            lines,
        },
        JournalEntrySource::System,
    )
    .await?;

    sqlx::query(
        "UPDATE invoices SET status = 'sent', posted_entry_id = $1, updated_at = now()
         WHERE id = $2",
    )
    .bind(entry_id)
    .bind(invoice_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

pub struct PaymentInput {
    pub payment_date: NaiveDate,
    pub amount_cents: i64,
    pub method: String,
    pub deposit_account_id: Uuid,
    pub reference: Option<String>,
    pub memo: Option<String>,
}

pub async fn record_payment(
    pool: &PgPool,
    company_id: Uuid,
    user_id: Uuid,
    invoice_id: Uuid,
    input: PaymentInput,
) -> AppResult<()> {
    if input.amount_cents <= 0 {
        return Err(AppError::BadRequest("payment amount must be positive".into()));
    }

    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let inv: Option<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT status::text, invoice_number, total_cents, paid_cents
         FROM invoices WHERE id = $1",
    )
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (status, number, total, paid) = inv.ok_or(AppError::NotFound)?;
    if status == "void" || status == "draft" {
        return Err(AppError::Conflict(format!(
            "cannot record payment on {status} invoice"
        )));
    }
    let new_paid = paid + input.amount_cents;
    if new_paid > total {
        return Err(AppError::BadRequest(format!(
            "payment exceeds balance ({} > {})",
            new_paid - paid,
            total - paid
        )));
    }

    let ar_id = find_or_create_ar(&mut tx, company_id, user_id).await?;

    let lines = vec![
        EntryLineInput {
            account_id: input.deposit_account_id,
            amount: input.amount_cents,
            currency: "USD".into(),
            memo: Some(format!("Payment for {number}")),
        },
        EntryLineInput {
            account_id: ar_id,
            amount: -input.amount_cents,
            currency: "USD".into(),
            memo: Some(format!("Payment for {number}")),
        },
    ];

    let entry_id = post_entry_in_tx(
        &mut tx,
        company_id,
        user_id,
        PostEntryInput {
            date: input.payment_date,
            memo: format!("Payment for invoice {number}"),
            reference: Some(number.clone()),
            lines,
        },
        JournalEntrySource::System,
    )
    .await?;

    let method = match input.method.as_str() {
        "cash" | "check" | "ach" | "wire" | "card" | "other" => input.method.clone(),
        _ => "other".into(),
    };

    sqlx::query(
        r#"
        INSERT INTO invoice_payments
            (company_id, invoice_id, payment_date, amount_cents, method,
             reference, deposit_account_id, entry_id, memo)
        VALUES ($1, $2, $3, $4, $5::invoice_payment_method, $6, $7, $8, $9)
        "#,
    )
    .bind(company_id)
    .bind(invoice_id)
    .bind(input.payment_date)
    .bind(input.amount_cents)
    .bind(&method)
    .bind(&input.reference)
    .bind(input.deposit_account_id)
    .bind(entry_id)
    .bind(&input.memo)
    .execute(&mut *tx)
    .await?;

    let new_status = if new_paid >= total { "paid" } else { "partial" };
    sqlx::query(
        "UPDATE invoices SET paid_cents = $1, status = $2::invoice_status, updated_at = now()
         WHERE id = $3",
    )
    .bind(new_paid)
    .bind(new_status)
    .bind(invoice_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(())
}

pub async fn void_invoice(
    pool: &PgPool,
    company_id: Uuid,
    user_id: Uuid,
    invoice_id: Uuid,
    reason: String,
) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let row: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT status::text, posted_entry_id FROM invoices WHERE id = $1",
    )
    .bind(invoice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (status, posted) = row.ok_or(AppError::NotFound)?;
    if status == "void" {
        return Ok(());
    }
    if let Some(entry_id) = posted {
        let _ = crate::commands::mutations::void_entry_in_tx(
            &mut tx,
            company_id,
            user_id,
            entry_id,
            reason.clone(),
        )
        .await?;
    }
    sqlx::query("UPDATE invoices SET status = 'void', updated_at = now() WHERE id = $1")
        .bind(invoice_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn mark_sent(
    pool: &PgPool,
    company_id: Uuid,
    invoice_id: Uuid,
    sent_to: &str,
) -> AppResult<()> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    sqlx::query(
        "UPDATE invoices SET sent_at = now(), last_sent_to = $1, updated_at = now()
         WHERE id = $2",
    )
    .bind(sent_to)
    .bind(invoice_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---------- helpers --------------------------------------------------------

async fn next_invoice_number<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
) -> AppResult<String> {
    sqlx::query(
        "INSERT INTO invoice_number_seq (company_id) VALUES ($1) ON CONFLICT DO NOTHING",
    )
    .bind(company_id)
    .execute(&mut **tx)
    .await?;
    let row: (String, i64) = sqlx::query_as(
        "UPDATE invoice_number_seq SET next_number = next_number + 1
         WHERE company_id = $1
         RETURNING prefix, next_number - 1",
    )
    .bind(company_id)
    .fetch_one(&mut **tx)
    .await?;
    Ok(format!("{}{}", row.0, row.1))
}

async fn find_or_create_ar<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    user_id: Uuid,
) -> AppResult<Uuid> {
    if let Some((id,)) = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM accounts WHERE company_id = $1 AND account_number = $2 AND is_active = true",
    )
    .bind(company_id)
    .bind(AR_ACCOUNT_NUMBER)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(id);
    }
    create_account_in_tx(
        tx,
        company_id,
        user_id,
        CreateAccountInput {
            account_type: EventAccountType::Asset,
            account_number: AR_ACCOUNT_NUMBER.into(),
            name: "Accounts Receivable".into(),
            currency: Some("USD".into()),
            description: Some("Money owed by customers".into()),
        },
    )
    .await
}

async fn find_or_create_sales_tax<'a>(
    tx: &mut Transaction<'a, Postgres>,
    company_id: Uuid,
    user_id: Uuid,
) -> AppResult<Uuid> {
    if let Some((id,)) = sqlx::query_as::<_, (Uuid,)>(
        "SELECT id FROM accounts WHERE company_id = $1 AND account_number = $2 AND is_active = true",
    )
    .bind(company_id)
    .bind(SALES_TAX_ACCOUNT_NUMBER)
    .fetch_optional(&mut **tx)
    .await?
    {
        return Ok(id);
    }
    create_account_in_tx(
        tx,
        company_id,
        user_id,
        CreateAccountInput {
            account_type: EventAccountType::Liability,
            account_number: SALES_TAX_ACCOUNT_NUMBER.into(),
            name: "Sales Tax Payable".into(),
            currency: Some("USD".into()),
            description: Some("Sales tax collected from customers".into()),
        },
    )
    .await
}

pub fn default_due_date(issue_date: NaiveDate, terms: &str) -> NaiveDate {
    let days: i64 = match terms {
        "due_on_receipt" => 0,
        "net_15" => 15,
        "net_30" => 30,
        "net_45" => 45,
        "net_60" => 60,
        _ => 30,
    };
    issue_date + chrono::Duration::days(days)
}

pub fn today_local() -> NaiveDate {
    Local::now().date_naive()
}
