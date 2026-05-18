use chrono::{Datelike, NaiveDate};
use sqlx::{Acquire, PgPool, Row};
use uuid::Uuid;

use crate::error::AppResult;
use crate::store::event_store::set_tenant;

#[derive(Debug, Clone)]
pub struct AccountRow {
    pub id: Uuid,
    pub account_type: String,
    pub account_number: String,
    pub name: String,
    pub currency: Option<String>,
}

pub async fn list_accounts(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<AccountRow>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        r#"
        SELECT id, account_type::text, account_number, name, currency
        FROM accounts
        WHERE is_active = true
        ORDER BY account_number ASC
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rows
        .into_iter()
        .map(|r| AccountRow {
            id: r.get(0),
            account_type: r.get::<String, _>(1),
            account_number: r.get(2),
            name: r.get(3),
            currency: r.get(4),
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct EntryRow {
    pub id: Uuid,
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub total_debits_cents: i64,
}

impl EntryRow {
    pub fn total_display(&self) -> String {
        format_cents(self.total_debits_cents)
    }
}

pub async fn list_entries(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<EntryRow>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        r#"
        SELECT je.id,
               je.date,
               je.memo,
               je.reference,
               COALESCE(SUM(GREATEST(jl.amount, 0)), 0)::BIGINT AS total_debits_cents
        FROM journal_entries je
        LEFT JOIN journal_lines jl ON jl.entry_id = je.id
        WHERE je.is_void = false
        GROUP BY je.id, je.date, je.memo, je.reference
        ORDER BY je.date DESC, je.id DESC
        LIMIT 200
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rows
        .into_iter()
        .map(|r| EntryRow {
            id: r.get(0),
            date: r.get(1),
            memo: r.get::<Option<String>, _>(2).unwrap_or_default(),
            reference: r.get(3),
            total_debits_cents: r.get(4),
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct TrialBalanceRow {
    pub account_number: String,
    pub name: String,
    pub account_type: String,
    pub debit_cents: i64,
    pub credit_cents: i64,
}

impl TrialBalanceRow {
    pub fn debit_display(&self) -> String {
        if self.debit_cents > 0 { format_cents(self.debit_cents) } else { String::new() }
    }
    pub fn credit_display(&self) -> String {
        if self.credit_cents > 0 { format_cents(self.credit_cents) } else { String::new() }
    }
}

pub fn format_cents(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let frac = abs % 100;
    format!("{sign}{dollars}.{frac:02}")
}

pub async fn trial_balance(
    pool: &PgPool,
    company_id: Uuid,
) -> AppResult<(Vec<TrialBalanceRow>, i64, i64)> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        r#"
        SELECT a.account_number,
               a.name,
               a.account_type::text,
               COALESCE(SUM(CASE WHEN jl.amount > 0 THEN jl.amount ELSE 0 END), 0)::BIGINT AS debit,
               COALESCE(SUM(CASE WHEN jl.amount < 0 THEN -jl.amount ELSE 0 END), 0)::BIGINT AS credit
        FROM accounts a
        LEFT JOIN journal_lines jl ON jl.account_id = a.id
        LEFT JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
        WHERE a.is_active = true
        GROUP BY a.id, a.account_number, a.name, a.account_type
        HAVING COALESCE(SUM(ABS(jl.amount)), 0) > 0
        ORDER BY a.account_number ASC
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut total_debit = 0i64;
    let mut total_credit = 0i64;
    let trial: Vec<TrialBalanceRow> = rows
        .into_iter()
        .map(|r| {
            let debit: i64 = r.get(3);
            let credit: i64 = r.get(4);
            total_debit += debit;
            total_credit += credit;
            TrialBalanceRow {
                account_number: r.get(0),
                name: r.get(1),
                account_type: r.get(2),
                debit_cents: debit,
                credit_cents: credit,
            }
        })
        .collect();

    Ok((trial, total_debit, total_credit))
}

#[derive(Debug, Clone)]
pub struct PlaidItemRow {
    pub id: Uuid,
    pub institution_name: String,
    pub status: String,
    pub last_synced_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl PlaidItemRow {
    pub fn last_synced_display(&self) -> String {
        match self.last_synced_at {
            Some(ts) => ts.format("%Y-%m-%d %H:%M UTC").to_string(),
            None => "never".to_string(),
        }
    }
}

pub async fn list_plaid_items(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<PlaidItemRow>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        r#"
        SELECT id, institution_name, status::text, last_synced_at
        FROM plaid_items
        ORDER BY institution_name ASC
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rows
        .into_iter()
        .map(|r| PlaidItemRow {
            id: r.get(0),
            institution_name: r.get(1),
            status: r.get::<String, _>(2),
            last_synced_at: r.get(3),
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Dashboard / reports
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DashboardKpis {
    pub total_assets_cents: i64,
    pub total_liabilities_cents: i64,
    pub total_equity_cents: i64,
    pub net_income_ytd_cents: i64,
    pub net_income_mtd_cents: i64,
    pub bank_count: i64,
    pub recent_entries: Vec<EntryRow>,
}

impl DashboardKpis {
    pub fn assets_display(&self) -> String { format_cents(self.total_assets_cents) }
    pub fn liabilities_display(&self) -> String { format_cents(self.total_liabilities_cents) }
    pub fn equity_display(&self) -> String { format_cents(self.total_equity_cents) }
    pub fn net_income_ytd_display(&self) -> String { format_cents(self.net_income_ytd_cents) }
    pub fn net_income_mtd_display(&self) -> String { format_cents(self.net_income_mtd_cents) }
    pub fn net_ytd_class(&self) -> &'static str {
        if self.net_income_ytd_cents > 0 { "pos" } else if self.net_income_ytd_cents < 0 { "neg" } else { "muted" }
    }
}

/// Sum of journal_lines.amount per account_type, restricted to non-void entries.
/// Returns a HashMap<account_type, sum_cents>.
async fn sum_by_account_type(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    end: Option<NaiveDate>,
    start: Option<NaiveDate>,
) -> Result<std::collections::HashMap<String, i64>, sqlx::Error> {
    let q = r#"
        SELECT a.account_type::text, COALESCE(SUM(jl.amount), 0)::BIGINT
        FROM accounts a
        LEFT JOIN journal_lines jl ON jl.account_id = a.id
        LEFT JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
        WHERE a.is_active = true
          AND ($1::date IS NULL OR je.date <= $1)
          AND ($2::date IS NULL OR je.date >= $2)
        GROUP BY a.account_type
    "#;
    let rows = sqlx::query(q)
        .bind(end)
        .bind(start)
        .fetch_all(&mut **tx)
        .await?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        map.insert(r.get::<String, _>(0), r.get::<i64, _>(1));
    }
    Ok(map)
}

pub async fn dashboard_kpis(pool: &PgPool, company_id: Uuid) -> AppResult<DashboardKpis> {
    let today = chrono::Utc::now().date_naive();
    let year_start = NaiveDate::from_ymd_opt(today.year_ce().1 as i32, 1, 1).unwrap();
    let month_start = NaiveDate::from_ymd_opt(today.year_ce().1 as i32, today.month0() + 1, 1).unwrap();

    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    // All-time balances per type for asset/liability/equity (positive amounts = debit-normal).
    // Convention: assets/expenses are debit-normal so balance = sum(amount).
    // Liabilities/equity/revenue are credit-normal so balance = -sum(amount).
    let all_time = sum_by_account_type(&mut tx, None, None).await?;
    let assets = *all_time.get("asset").unwrap_or(&0);
    let liabs = -*all_time.get("liability").unwrap_or(&0);
    let equity = -*all_time.get("equity").unwrap_or(&0);

    // YTD net income = -(revenue + expense) where revenue is credit-normal and expense is debit-normal.
    // Net income increases equity, so positive net income = -sum(rev) - sum(exp) inverted:
    // revenue contributes -sum_amount (since credit-normal), expense contributes -sum_amount.
    // Net income = revenue - expense = (-sum_rev) - (sum_exp).
    let ytd = sum_by_account_type(&mut tx, Some(today), Some(year_start)).await?;
    let net_ytd = -ytd.get("revenue").copied().unwrap_or(0) - ytd.get("expense").copied().unwrap_or(0);

    let mtd = sum_by_account_type(&mut tx, Some(today), Some(month_start)).await?;
    let net_mtd = -mtd.get("revenue").copied().unwrap_or(0) - mtd.get("expense").copied().unwrap_or(0);

    let bank_count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plaid_items")
        .fetch_one(&mut *tx)
        .await?;

    let recent = sqlx::query(
        r#"
        SELECT je.id, je.date, je.memo, je.reference,
               COALESCE(SUM(GREATEST(jl.amount, 0)), 0)::BIGINT
        FROM journal_entries je
        LEFT JOIN journal_lines jl ON jl.entry_id = je.id
        WHERE je.is_void = false
        GROUP BY je.id, je.date, je.memo, je.reference
        ORDER BY je.date DESC, je.id DESC
        LIMIT 5
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;

    tx.commit().await?;

    let recent_entries = recent
        .into_iter()
        .map(|r| EntryRow {
            id: r.get(0),
            date: r.get(1),
            memo: r.get::<Option<String>, _>(2).unwrap_or_default(),
            reference: r.get(3),
            total_debits_cents: r.get(4),
        })
        .collect();

    Ok(DashboardKpis {
        total_assets_cents: assets,
        total_liabilities_cents: liabs,
        total_equity_cents: equity,
        net_income_ytd_cents: net_ytd,
        net_income_mtd_cents: net_mtd,
        bank_count: bank_count.0,
        recent_entries,
    })
}

#[derive(Debug, Clone)]
pub struct ReportLine {
    pub account_number: String,
    pub name: String,
    pub amount_cents: i64,
}
impl ReportLine {
    pub fn amount_display(&self) -> String { format_cents(self.amount_cents) }
}

#[derive(Debug, Clone)]
pub struct IncomeStatement {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub revenues: Vec<ReportLine>,
    pub expenses: Vec<ReportLine>,
    pub total_revenue_cents: i64,
    pub total_expense_cents: i64,
}
impl IncomeStatement {
    pub fn total_revenue_display(&self) -> String { format_cents(self.total_revenue_cents) }
    pub fn total_expense_display(&self) -> String { format_cents(self.total_expense_cents) }
    pub fn net_income_cents(&self) -> i64 { self.total_revenue_cents - self.total_expense_cents }
    pub fn net_income_display(&self) -> String { format_cents(self.net_income_cents()) }
    pub fn net_class(&self) -> &'static str {
        if self.net_income_cents() > 0 { "pos" } else if self.net_income_cents() < 0 { "neg" } else { "muted" }
    }
}

pub async fn income_statement(
    pool: &PgPool,
    company_id: Uuid,
    start: NaiveDate,
    end: NaiveDate,
) -> AppResult<IncomeStatement> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        r#"
        SELECT a.account_type::text, a.account_number, a.name,
               COALESCE(SUM(jl.amount), 0)::BIGINT AS sum_amount
        FROM accounts a
        LEFT JOIN journal_lines jl ON jl.account_id = a.id
        LEFT JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
            AND je.date BETWEEN $1 AND $2
        WHERE a.is_active = true AND a.account_type IN ('revenue', 'expense')
        GROUP BY a.id, a.account_type, a.account_number, a.name
        HAVING COALESCE(SUM(jl.amount), 0) <> 0
        ORDER BY a.account_number ASC
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let mut revenues = Vec::new();
    let mut expenses = Vec::new();
    let mut total_rev = 0i64;
    let mut total_exp = 0i64;
    for r in rows {
        let acct_type: String = r.get(0);
        let sum: i64 = r.get(3);
        match acct_type.as_str() {
            "revenue" => {
                let amount = -sum; // credit-normal → positive
                total_rev += amount;
                revenues.push(ReportLine {
                    account_number: r.get(1),
                    name: r.get(2),
                    amount_cents: amount,
                });
            }
            "expense" => {
                let amount = sum; // debit-normal → positive as expense
                total_exp += amount;
                expenses.push(ReportLine {
                    account_number: r.get(1),
                    name: r.get(2),
                    amount_cents: amount,
                });
            }
            _ => {}
        }
    }
    Ok(IncomeStatement {
        start,
        end,
        revenues,
        expenses,
        total_revenue_cents: total_rev,
        total_expense_cents: total_exp,
    })
}

#[derive(Debug, Clone)]
pub struct BalanceSheet {
    pub as_of: NaiveDate,
    pub assets: Vec<ReportLine>,
    pub liabilities: Vec<ReportLine>,
    pub equity: Vec<ReportLine>,
    pub net_income_cents: i64,
    pub total_assets_cents: i64,
    pub total_liab_cents: i64,
    pub total_equity_cents: i64,
}
impl BalanceSheet {
    pub fn total_assets_display(&self) -> String { format_cents(self.total_assets_cents) }
    pub fn total_liab_display(&self) -> String { format_cents(self.total_liab_cents) }
    pub fn total_equity_display(&self) -> String { format_cents(self.total_equity_cents) }
    pub fn net_income_display(&self) -> String { format_cents(self.net_income_cents) }
    pub fn liab_plus_equity_cents(&self) -> i64 { self.total_liab_cents + self.total_equity_cents + self.net_income_cents }
    pub fn liab_plus_equity_display(&self) -> String { format_cents(self.liab_plus_equity_cents()) }
    pub fn balance_class(&self) -> &'static str {
        if self.total_assets_cents == self.liab_plus_equity_cents() { "ok" } else { "err" }
    }
    pub fn balance_msg(&self) -> String {
        if self.total_assets_cents == self.liab_plus_equity_cents() {
            "Balanced".to_string()
        } else {
            format!("Off by {}", format_cents((self.total_assets_cents - self.liab_plus_equity_cents()).abs()))
        }
    }
}

pub async fn balance_sheet(pool: &PgPool, company_id: Uuid, as_of: NaiveDate) -> AppResult<BalanceSheet> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let rows = sqlx::query(
        r#"
        SELECT a.account_type::text, a.account_number, a.name,
               COALESCE(SUM(jl.amount), 0)::BIGINT AS sum_amount
        FROM accounts a
        LEFT JOIN journal_lines jl ON jl.account_id = a.id
        LEFT JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
            AND je.date <= $1
        WHERE a.is_active = true
          AND a.account_type IN ('asset', 'liability', 'equity')
        GROUP BY a.id, a.account_type, a.account_number, a.name
        HAVING COALESCE(SUM(jl.amount), 0) <> 0
        ORDER BY a.account_number ASC
        "#,
    )
    .bind(as_of)
    .fetch_all(&mut *tx)
    .await?;

    // Net income up through as_of for current calendar year.
    let year = as_of.format("%Y").to_string().parse::<i32>().unwrap_or(2026);
    let year_start = NaiveDate::from_ymd_opt(year, 1, 1).unwrap();
    let ni_row: (i64, i64) = sqlx::query_as(
        r#"
        SELECT
            COALESCE(SUM(CASE WHEN a.account_type = 'revenue' THEN -jl.amount ELSE 0 END), 0)::BIGINT,
            COALESCE(SUM(CASE WHEN a.account_type = 'expense' THEN jl.amount ELSE 0 END), 0)::BIGINT
        FROM journal_lines jl
        JOIN accounts a ON a.id = jl.account_id
        JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
        WHERE je.date BETWEEN $1 AND $2
        "#,
    )
    .bind(year_start)
    .bind(as_of)
    .fetch_one(&mut *tx)
    .await?;
    let net_income = ni_row.0 - ni_row.1;
    tx.commit().await?;

    let mut assets = Vec::new();
    let mut liabilities = Vec::new();
    let mut equity = Vec::new();
    let mut total_assets = 0i64;
    let mut total_liab = 0i64;
    let mut total_equity = 0i64;
    for r in rows {
        let t: String = r.get(0);
        let sum: i64 = r.get(3);
        match t.as_str() {
            "asset" => {
                total_assets += sum;
                assets.push(ReportLine { account_number: r.get(1), name: r.get(2), amount_cents: sum });
            }
            "liability" => {
                let amt = -sum;
                total_liab += amt;
                liabilities.push(ReportLine { account_number: r.get(1), name: r.get(2), amount_cents: amt });
            }
            "equity" => {
                let amt = -sum;
                total_equity += amt;
                equity.push(ReportLine { account_number: r.get(1), name: r.get(2), amount_cents: amt });
            }
            _ => {}
        }
    }

    Ok(BalanceSheet {
        as_of,
        assets,
        liabilities,
        equity,
        net_income_cents: net_income,
        total_assets_cents: total_assets,
        total_liab_cents: total_liab,
        total_equity_cents: total_equity,
    })
}

#[derive(Debug, Clone)]
pub struct CashFlow {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub opening_cash_cents: i64,
    pub closing_cash_cents: i64,
    pub change_cents: i64,
    pub by_other_account: Vec<ReportLine>,
}
impl CashFlow {
    pub fn opening_display(&self) -> String { format_cents(self.opening_cash_cents) }
    pub fn closing_display(&self) -> String { format_cents(self.closing_cash_cents) }
    pub fn change_display(&self) -> String { format_cents(self.change_cents) }
    pub fn change_class(&self) -> &'static str {
        if self.change_cents > 0 { "pos" } else if self.change_cents < 0 { "neg" } else { "muted" }
    }
}

/// Simplified cash flow: net change in all asset accounts whose name contains "cash" or "bank",
/// plus a breakdown by counterpart account. Real GAAP statement-of-cash-flows is v2.
pub async fn cash_flow(
    pool: &PgPool,
    company_id: Uuid,
    start: NaiveDate,
    end: NaiveDate,
) -> AppResult<CashFlow> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    // Sum cash account balances at end and start.
    let opening: (i64,) = sqlx::query_as(
        r#"
        SELECT COALESCE(SUM(jl.amount), 0)::BIGINT
        FROM journal_lines jl
        JOIN accounts a ON a.id = jl.account_id
        JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
        WHERE a.account_type = 'asset'
          AND (a.name ILIKE '%cash%' OR a.name ILIKE '%bank%' OR a.name ILIKE '%checking%' OR a.name ILIKE '%savings%')
          AND je.date < $1
        "#,
    )
    .bind(start)
    .fetch_one(&mut *tx)
    .await?;
    let closing: (i64,) = sqlx::query_as(
        r#"
        SELECT COALESCE(SUM(jl.amount), 0)::BIGINT
        FROM journal_lines jl
        JOIN accounts a ON a.id = jl.account_id
        JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
        WHERE a.account_type = 'asset'
          AND (a.name ILIKE '%cash%' OR a.name ILIKE '%bank%' OR a.name ILIKE '%checking%' OR a.name ILIKE '%savings%')
          AND je.date <= $1
        "#,
    )
    .bind(end)
    .fetch_one(&mut *tx)
    .await?;

    // Counterpart accounts movement during the period (entries that touch a cash account):
    let breakdown = sqlx::query(
        r#"
        WITH cash_entries AS (
            SELECT DISTINCT je.id AS entry_id
            FROM journal_entries je
            JOIN journal_lines jl ON jl.entry_id = je.id
            JOIN accounts a ON a.id = jl.account_id
            WHERE je.is_void = false
              AND je.date BETWEEN $1 AND $2
              AND a.account_type = 'asset'
              AND (a.name ILIKE '%cash%' OR a.name ILIKE '%bank%' OR a.name ILIKE '%checking%' OR a.name ILIKE '%savings%')
        )
        SELECT a.account_number, a.name, COALESCE(SUM(jl.amount), 0)::BIGINT AS amt
        FROM cash_entries ce
        JOIN journal_lines jl ON jl.entry_id = ce.entry_id
        JOIN accounts a ON a.id = jl.account_id
        WHERE NOT (a.account_type = 'asset'
                   AND (a.name ILIKE '%cash%' OR a.name ILIKE '%bank%' OR a.name ILIKE '%checking%' OR a.name ILIKE '%savings%'))
        GROUP BY a.id, a.account_number, a.name
        HAVING COALESCE(SUM(jl.amount), 0) <> 0
        ORDER BY a.account_number ASC
        "#,
    )
    .bind(start)
    .bind(end)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let by_other_account: Vec<ReportLine> = breakdown
        .into_iter()
        .map(|r| ReportLine {
            account_number: r.get(0),
            name: r.get(1),
            amount_cents: -r.get::<i64, _>(2), // sign-flip: cash inflow corresponds to credit on counterpart
        })
        .collect();

    Ok(CashFlow {
        start,
        end,
        opening_cash_cents: opening.0,
        closing_cash_cents: closing.0,
        change_cents: closing.0 - opening.0,
        by_other_account,
    })
}

// ---------------------------------------------------------------------------
// Transactions (line-level)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TransactionLine {
    pub line_id: Uuid,
    pub entry_id: Uuid,
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub account_id: Uuid,
    pub account_number: String,
    pub account_name: String,
    pub amount_cents: i64,
    pub currency: String,
    pub source: Option<String>,
    pub is_void: bool,
}

impl TransactionLine {
    pub fn amount_display(&self) -> String { format_cents(self.amount_cents) }
    pub fn debit_display(&self) -> String {
        if self.amount_cents > 0 { format_cents(self.amount_cents) } else { String::new() }
    }
    pub fn credit_display(&self) -> String {
        if self.amount_cents < 0 { format_cents(-self.amount_cents) } else { String::new() }
    }
    pub fn source_label(&self) -> &str {
        self.source.as_deref().unwrap_or("manual")
    }
}

#[derive(Debug, Clone, Default)]
pub struct TransactionFilter {
    pub start: Option<NaiveDate>,
    pub end: Option<NaiveDate>,
    pub account_id: Option<Uuid>,
    pub source: Option<String>,
    pub search: Option<String>,
    pub include_void: bool,
}

pub async fn list_transactions(
    pool: &PgPool,
    company_id: Uuid,
    filter: &TransactionFilter,
) -> AppResult<Vec<TransactionLine>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    // When no specific account is picked we show only the asset/liability "bank side"
    // of each entry — otherwise every Plaid transaction shows up twice (bank line +
    // Uncategorized counterpart). Filtering by a specific account always wins.
    let rows = sqlx::query(
        r#"
        SELECT jl.id, je.id, je.date, je.memo, je.reference,
               a.id, a.account_number, a.name,
               jl.amount, jl.currency,
               je.source::text, je.is_void
        FROM journal_lines jl
        JOIN journal_entries je ON je.id = jl.entry_id
        JOIN accounts a ON a.id = jl.account_id
        WHERE ($1::date IS NULL OR je.date >= $1)
          AND ($2::date IS NULL OR je.date <= $2)
          AND ($3::uuid IS NULL OR jl.account_id = $3)
          AND ($3::uuid IS NOT NULL OR a.account_type IN ('asset', 'liability'))
          AND ($4::text IS NULL OR je.source::text = $4)
          AND ($5::text IS NULL OR je.memo ILIKE '%' || $5 || '%')
          AND ($6::boolean = true OR je.is_void = false)
        ORDER BY je.date DESC, je.id DESC, jl.amount DESC
        LIMIT 500
        "#,
    )
    .bind(filter.start)
    .bind(filter.end)
    .bind(filter.account_id)
    .bind(filter.source.as_deref())
    .bind(filter.search.as_deref())
    .bind(filter.include_void)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(rows
        .into_iter()
        .map(|r| TransactionLine {
            line_id: r.get(0),
            entry_id: r.get(1),
            date: r.get(2),
            memo: r.get::<Option<String>, _>(3).unwrap_or_default(),
            reference: r.get(4),
            account_id: r.get(5),
            account_number: r.get(6),
            account_name: r.get(7),
            amount_cents: r.get(8),
            currency: r.get(9),
            source: r.get(10),
            is_void: r.get(11),
        })
        .collect())
}

/// Resolve the user's first company membership.
pub async fn resolve_company_id(pool: &PgPool, user_id: Uuid) -> AppResult<Option<Uuid>> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT company_id FROM memberships WHERE user_id = $1 ORDER BY created_at ASC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

#[derive(Debug, Clone)]
pub struct CompanyRow {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub role: String,
}

pub async fn list_companies_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<Vec<CompanyRow>> {
    let rows = sqlx::query(
        r#"
        SELECT c.id, c.name, c.slug, m.role::text
        FROM memberships m
        JOIN companies c ON c.id = m.company_id
        WHERE m.user_id = $1
        ORDER BY c.name ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CompanyRow {
            id: r.get(0),
            name: r.get(1),
            slug: r.get(2),
            role: r.get(3),
        })
        .collect())
}

pub async fn user_has_membership(pool: &PgPool, user_id: Uuid, company_id: Uuid) -> AppResult<bool> {
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM memberships WHERE user_id = $1 AND company_id = $2",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

#[derive(Debug, Clone)]
pub struct MemberRow {
    pub user_id: Uuid,
    pub email: String,
    pub name: Option<String>,
    pub role: String,
}

pub async fn list_members(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<MemberRow>> {
    let rows = sqlx::query(
        r#"
        SELECT u.id, u.email, u.name, m.role::text
        FROM memberships m
        JOIN auth_users u ON u.id = m.user_id
        WHERE m.company_id = $1
        ORDER BY m.role ASC, u.email ASC
        "#,
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| MemberRow {
            user_id: r.get(0),
            email: r.get(1),
            name: r.get(2),
            role: r.get(3),
        })
        .collect())
}

/// Create a new company with the given name, owned by user_id who becomes 'owner'.
pub async fn create_company(pool: &PgPool, user_id: Uuid, name: &str) -> AppResult<Uuid> {
    let mut tx = pool.begin().await?;
    let slug_base: String = name
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c.is_whitespace() || c == '-' || c == '_' {
                Some('-')
            } else {
                None
            }
        })
        .collect();
    let slug_base = slug_base.trim_matches('-').to_string();
    let slug_base = if slug_base.is_empty() { "company".to_string() } else { slug_base };
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    let slug = format!("{}-{}", slug_base.chars().take(31).collect::<String>(), suffix);
    let row: (Uuid,) = sqlx::query_as(
        "INSERT INTO companies (slug, name, owner_user_id) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(&slug)
    .bind(name)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO memberships (user_id, company_id, role) VALUES ($1, $2, 'owner')")
        .bind(user_id)
        .bind(row.0)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(row.0)
}

/// Get current user's role in a given company.
pub async fn user_role_in(pool: &PgPool, user_id: Uuid, company_id: Uuid) -> AppResult<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT role::text FROM memberships WHERE user_id = $1 AND company_id = $2",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(r,)| r))
}

/// True when role grants write/admin authority over membership/settings.
pub fn role_can_admin(role: &str) -> bool {
    matches!(role, "owner" | "admin")
}

pub async fn update_company_settings(
    pool: &PgPool,
    company_id: Uuid,
    name: &str,
    base_currency: &str,
    fiscal_year_start_month: i16,
) -> AppResult<()> {
    sqlx::query(
        "UPDATE companies SET name = $1, base_currency = $2, fiscal_year_start_month = $3, updated_at = now() WHERE id = $4",
    )
    .bind(name.trim())
    .bind(base_currency.trim())
    .bind(fiscal_year_start_month)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_company(pool: &PgPool, company_id: Uuid) -> AppResult<Option<(String, String, i16)>> {
    let row: Option<(String, String, i16)> = sqlx::query_as(
        "SELECT name, base_currency, fiscal_year_start_month FROM companies WHERE id = $1",
    )
    .bind(company_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn remove_member(pool: &PgPool, company_id: Uuid, user_id: Uuid) -> AppResult<()> {
    // Prevent removing the last owner.
    let owner_count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM memberships WHERE company_id = $1 AND role = 'owner'",
    )
    .bind(company_id)
    .fetch_one(pool)
    .await?;
    let target_role: Option<(String,)> = sqlx::query_as(
        "SELECT role::text FROM memberships WHERE company_id = $1 AND user_id = $2",
    )
    .bind(company_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    if let Some((r,)) = &target_role {
        if r == "owner" && owner_count.0 <= 1 {
            return Err(crate::error::AppError::BadRequest(
                "cannot remove the last owner".into(),
            ));
        }
    }
    sqlx::query("DELETE FROM memberships WHERE company_id = $1 AND user_id = $2")
        .bind(company_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn change_member_role(
    pool: &PgPool,
    company_id: Uuid,
    user_id: Uuid,
    new_role: &str,
) -> AppResult<()> {
    let new_role = match new_role {
        "owner" | "admin" | "accountant" | "viewer" => new_role,
        _ => return Err(crate::error::AppError::BadRequest("invalid role".into())),
    };
    // Prevent demoting last owner.
    if new_role != "owner" {
        let target_role: Option<(String,)> = sqlx::query_as(
            "SELECT role::text FROM memberships WHERE company_id = $1 AND user_id = $2",
        )
        .bind(company_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;
        if let Some((r,)) = target_role {
            if r == "owner" {
                let owner_count: (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM memberships WHERE company_id = $1 AND role = 'owner'",
                )
                .bind(company_id)
                .fetch_one(pool)
                .await?;
                if owner_count.0 <= 1 {
                    return Err(crate::error::AppError::BadRequest(
                        "cannot demote the last owner".into(),
                    ));
                }
            }
        }
    }
    sqlx::query(&format!(
        "UPDATE memberships SET role = '{new_role}'::membership_role WHERE company_id = $1 AND user_id = $2"
    ))
    .bind(company_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct InvitationRow {
    pub id: Uuid,
    pub token: String,
    pub role: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub accepted_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn list_invitations(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<InvitationRow>> {
    let rows = sqlx::query(
        r#"
        SELECT id, token, role::text, created_at, expires_at, accepted_at
        FROM company_invitations
        WHERE company_id = $1
        ORDER BY created_at DESC
        LIMIT 50
        "#,
    )
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| InvitationRow {
            id: r.get(0),
            token: r.get(1),
            role: r.get(2),
            created_at: r.get(3),
            expires_at: r.get(4),
            accepted_at: r.get(5),
        })
        .collect())
}

pub async fn create_invitation(
    pool: &PgPool,
    company_id: Uuid,
    invited_by: Uuid,
    role: &str,
    ttl_days: i64,
) -> AppResult<String> {
    use rand::RngCore;
    let role = match role {
        "owner" | "admin" | "accountant" | "viewer" => role,
        _ => return Err(crate::error::AppError::BadRequest("invalid role".into())),
    };
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let expires_at = chrono::Utc::now() + chrono::Duration::days(ttl_days);
    sqlx::query(&format!(
        "INSERT INTO company_invitations (token, company_id, role, invited_by, expires_at) VALUES ($1, $2, '{role}'::membership_role, $3, $4)"
    ))
    .bind(&token)
    .bind(company_id)
    .bind(invited_by)
    .bind(expires_at)
    .execute(pool)
    .await?;
    Ok(token)
}

pub async fn accept_invitation(
    pool: &PgPool,
    token: &str,
    user_id: Uuid,
) -> AppResult<Uuid> {
    let row: Option<(Uuid, Uuid, String, chrono::DateTime<chrono::Utc>, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as(
            "SELECT id, company_id, role::text, expires_at, accepted_at FROM company_invitations WHERE token = $1",
        )
        .bind(token)
        .fetch_optional(pool)
        .await?;
    let (inv_id, company_id, role, expires_at, accepted_at) = row
        .ok_or_else(|| crate::error::AppError::NotFound)?;
    if accepted_at.is_some() {
        return Err(crate::error::AppError::Conflict("invitation already used".into()));
    }
    if chrono::Utc::now() > expires_at {
        return Err(crate::error::AppError::BadRequest("invitation expired".into()));
    }

    let mut tx = pool.begin().await?;
    sqlx::query(&format!(
        "INSERT INTO memberships (user_id, company_id, role) VALUES ($1, $2, '{role}'::membership_role) ON CONFLICT DO NOTHING"
    ))
    .bind(user_id)
    .bind(company_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE company_invitations SET accepted_at = now(), accepted_by = $1 WHERE id = $2")
        .bind(user_id)
        .bind(inv_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(company_id)
}

/// Add an existing user (by email) as a member of the company.
pub async fn add_member_by_email(
    pool: &PgPool,
    company_id: Uuid,
    email: &str,
    role: &str,
) -> AppResult<()> {
    let normalized = email.trim().to_lowercase();
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM auth_users WHERE email_normalized = $1",
    )
    .bind(&normalized)
    .fetch_optional(pool)
    .await?;
    let user_id = row.ok_or_else(|| {
        crate::error::AppError::BadRequest(format!("no user found with email {email}"))
    })?.0;
    let role_clause = match role {
        "owner" | "admin" | "accountant" | "viewer" => role,
        _ => return Err(crate::error::AppError::BadRequest("invalid role".into())),
    };
    // ON CONFLICT DO NOTHING in case already a member.
    sqlx::query(&format!(
        "INSERT INTO memberships (user_id, company_id, role) VALUES ($1, $2, '{role_clause}'::membership_role) ON CONFLICT DO NOTHING"
    ))
    .bind(user_id)
    .bind(company_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ===========================================================================
// Invoicing
// ===========================================================================

#[derive(Debug, Clone)]
pub struct CustomerRow {
    pub id: Uuid,
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub default_terms: String,
    pub is_active: bool,
}

pub async fn list_customers(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<CustomerRow>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        "SELECT id, name, email, phone, city, state, default_terms, is_active
         FROM customers WHERE company_id = $1 ORDER BY name ASC",
    )
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|r| CustomerRow {
            id: r.get(0),
            name: r.get(1),
            email: r.get(2),
            phone: r.get(3),
            city: r.get(4),
            state: r.get(5),
            default_terms: r.get(6),
            is_active: r.get(7),
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct CustomerDetail {
    pub id: Uuid,
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country: String,
    pub default_terms: String,
    pub notes: Option<String>,
}

pub async fn get_customer(pool: &PgPool, company_id: Uuid, id: Uuid) -> AppResult<Option<CustomerDetail>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let row = sqlx::query(
        "SELECT id, name, email, phone, address_line1, address_line2, city, state,
                postal_code, country, default_terms, notes
         FROM customers WHERE company_id = $1 AND id = $2",
    )
    .bind(company_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.map(|r| CustomerDetail {
        id: r.get(0),
        name: r.get(1),
        email: r.get(2),
        phone: r.get(3),
        address_line1: r.get(4),
        address_line2: r.get(5),
        city: r.get(6),
        state: r.get(7),
        postal_code: r.get(8),
        country: r.get(9),
        default_terms: r.get(10),
        notes: r.get(11),
    }))
}

pub struct CreateCustomerInput {
    pub name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub postal_code: Option<String>,
    pub country: String,
    pub default_terms: String,
    pub notes: Option<String>,
}

pub async fn create_customer(
    pool: &PgPool,
    company_id: Uuid,
    input: CreateCustomerInput,
) -> AppResult<Uuid> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO customers
            (id, company_id, name, email, phone, address_line1, address_line2,
             city, state, postal_code, country, default_terms, notes)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(id)
    .bind(company_id)
    .bind(&input.name)
    .bind(&input.email)
    .bind(&input.phone)
    .bind(&input.address_line1)
    .bind(&input.address_line2)
    .bind(&input.city)
    .bind(&input.state)
    .bind(&input.postal_code)
    .bind(&input.country)
    .bind(&input.default_terms)
    .bind(&input.notes)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

#[derive(Debug, Clone)]
pub struct InvoiceRow {
    pub id: Uuid,
    pub invoice_number: String,
    pub customer_id: Uuid,
    pub customer_name: String,
    pub status: String,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    pub total_cents: i64,
    pub paid_cents: i64,
}

impl InvoiceRow {
    pub fn total_display(&self) -> String { format_cents(self.total_cents) }
    pub fn balance_cents(&self) -> i64 { self.total_cents - self.paid_cents }
    pub fn balance_display(&self) -> String { format_cents(self.balance_cents()) }
    pub fn status_badge(&self) -> &'static str {
        match self.status.as_str() {
            "paid" => "ok",
            "void" => "err",
            "overdue" => "err",
            "partial" => "warn",
            "sent" => "warn",
            _ => "",
        }
    }
    pub fn issue_us(&self) -> String { self.issue_date.format("%m/%d/%Y").to_string() }
    pub fn due_us(&self) -> String { self.due_date.format("%m/%d/%Y").to_string() }
}

pub async fn list_invoices(
    pool: &PgPool,
    company_id: Uuid,
    status_filter: Option<&str>,
) -> AppResult<Vec<InvoiceRow>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        "SELECT i.id, i.invoice_number, i.customer_id, c.name,
                i.status::text, i.issue_date, i.due_date, i.total_cents, i.paid_cents
         FROM invoices i JOIN customers c ON c.id = i.customer_id
         WHERE i.company_id = $1
         AND ($2::text IS NULL OR i.status::text = $2)
         ORDER BY i.issue_date DESC, i.invoice_number DESC",
    )
    .bind(company_id)
    .bind(status_filter)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|r| InvoiceRow {
        id: r.get(0), invoice_number: r.get(1), customer_id: r.get(2),
        customer_name: r.get(3), status: r.get(4), issue_date: r.get(5),
        due_date: r.get(6), total_cents: r.get(7), paid_cents: r.get(8),
    }).collect())
}

#[derive(Debug, Clone)]
pub struct InvoiceDetail {
    pub id: Uuid,
    pub invoice_number: String,
    pub status: String,
    pub issue_date: NaiveDate,
    pub due_date: NaiveDate,
    pub terms: String,
    pub currency: String,
    pub subtotal_cents: i64,
    pub tax_cents: i64,
    pub total_cents: i64,
    pub paid_cents: i64,
    pub memo: Option<String>,
    pub customer_notes: Option<String>,
    pub public_token: String,
    pub sent_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_sent_to: Option<String>,
    pub customer: CustomerDetail,
    pub lines: Vec<InvoiceLineRow>,
    pub payments: Vec<InvoicePaymentRow>,
}

impl InvoiceDetail {
    pub fn subtotal_display(&self) -> String { format_cents(self.subtotal_cents) }
    pub fn tax_display(&self) -> String { format_cents(self.tax_cents) }
    pub fn total_display(&self) -> String { format_cents(self.total_cents) }
    pub fn paid_display(&self) -> String { format_cents(self.paid_cents) }
    pub fn balance_cents(&self) -> i64 { self.total_cents - self.paid_cents }
    pub fn balance_display(&self) -> String { format_cents(self.balance_cents()) }
    pub fn issue_us(&self) -> String { self.issue_date.format("%m/%d/%Y").to_string() }
    pub fn due_us(&self) -> String { self.due_date.format("%m/%d/%Y").to_string() }
    pub fn is_draft(&self) -> bool { self.status == "draft" }
    pub fn is_void(&self) -> bool { self.status == "void" }
    pub fn is_paid(&self) -> bool { self.status == "paid" }
    pub fn status_badge(&self) -> &'static str {
        match self.status.as_str() {
            "paid" => "ok",
            "void" => "err",
            "partial" | "sent" => "warn",
            _ => "",
        }
    }
}

#[derive(Debug, Clone)]
pub struct InvoiceLineRow {
    pub description: String,
    pub quantity: rust_decimal::Decimal,
    pub unit_price_cents: i64,
    pub amount_cents: i64,
    pub tax_rate_pct: rust_decimal::Decimal,
    pub tax_cents: i64,
    pub revenue_account_id: Uuid,
    pub revenue_account_name: String,
}

impl InvoiceLineRow {
    pub fn unit_price_display(&self) -> String { format_cents(self.unit_price_cents) }
    pub fn amount_display(&self) -> String { format_cents(self.amount_cents) }
    pub fn tax_display(&self) -> String { format_cents(self.tax_cents) }
    pub fn quantity_display(&self) -> String {
        let s = self.quantity.to_string();
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else { s }
    }
}

#[derive(Debug, Clone)]
pub struct InvoicePaymentRow {
    pub payment_date: NaiveDate,
    pub amount_cents: i64,
    pub method: String,
    pub reference: Option<String>,
    pub deposit_account_name: String,
}

impl InvoicePaymentRow {
    pub fn amount_display(&self) -> String { format_cents(self.amount_cents) }
    pub fn date_us(&self) -> String { self.payment_date.format("%m/%d/%Y").to_string() }
}

pub async fn get_invoice(
    pool: &PgPool,
    company_id: Uuid,
    id: Uuid,
) -> AppResult<Option<InvoiceDetail>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;

    let inv_row = sqlx::query(
        "SELECT i.id, i.invoice_number, i.status::text, i.issue_date, i.due_date,
                i.terms, i.currency, i.subtotal_cents, i.tax_cents, i.total_cents,
                i.paid_cents, i.memo, i.customer_notes, i.public_token,
                i.sent_at, i.last_sent_to,
                c.id, c.name, c.email, c.phone, c.address_line1, c.address_line2,
                c.city, c.state, c.postal_code, c.country, c.default_terms, c.notes
         FROM invoices i JOIN customers c ON c.id = i.customer_id
         WHERE i.company_id = $1 AND i.id = $2",
    )
    .bind(company_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(r) = inv_row else {
        tx.commit().await?;
        return Ok(None);
    };
    let customer = CustomerDetail {
        id: r.get(16),
        name: r.get(17),
        email: r.get(18),
        phone: r.get(19),
        address_line1: r.get(20),
        address_line2: r.get(21),
        city: r.get(22),
        state: r.get(23),
        postal_code: r.get(24),
        country: r.get(25),
        default_terms: r.get(26),
        notes: r.get(27),
    };

    let line_rows = sqlx::query(
        "SELECT il.description, il.quantity, il.unit_price_cents, il.amount_cents,
                il.tax_rate_pct, il.tax_cents, il.revenue_account_id, a.name
         FROM invoice_lines il JOIN accounts a ON a.id = il.revenue_account_id
         WHERE il.invoice_id = $1
         ORDER BY il.sort_order ASC",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let lines = line_rows.into_iter().map(|l| InvoiceLineRow {
        description: l.get(0),
        quantity: l.get(1),
        unit_price_cents: l.get(2),
        amount_cents: l.get(3),
        tax_rate_pct: l.get(4),
        tax_cents: l.get(5),
        revenue_account_id: l.get(6),
        revenue_account_name: l.get(7),
    }).collect();

    let pay_rows = sqlx::query(
        "SELECT p.payment_date, p.amount_cents, p.method::text, p.reference, a.name
         FROM invoice_payments p JOIN accounts a ON a.id = p.deposit_account_id
         WHERE p.invoice_id = $1 ORDER BY p.payment_date DESC, p.created_at DESC",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let payments = pay_rows.into_iter().map(|p| InvoicePaymentRow {
        payment_date: p.get(0),
        amount_cents: p.get(1),
        method: p.get(2),
        reference: p.get(3),
        deposit_account_name: p.get(4),
    }).collect();

    tx.commit().await?;

    Ok(Some(InvoiceDetail {
        id: r.get(0),
        invoice_number: r.get(1),
        status: r.get(2),
        issue_date: r.get(3),
        due_date: r.get(4),
        terms: r.get(5),
        currency: r.get(6),
        subtotal_cents: r.get(7),
        tax_cents: r.get(8),
        total_cents: r.get(9),
        paid_cents: r.get(10),
        memo: r.get(11),
        customer_notes: r.get(12),
        public_token: r.get(13),
        sent_at: r.get(14),
        last_sent_to: r.get(15),
        customer,
        lines,
        payments,
    }))
}

/// Resolve a public token to the invoice's id + company + company name.
/// Uses a transaction-scoped setting that the public_token policy reads.
pub async fn get_invoice_by_token(
    pool: &PgPool,
    token: &str,
) -> AppResult<Option<(Uuid, Uuid, String)>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT set_config('app.invoice_public_token', $1, true)")
        .bind(token)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT i.id, i.company_id, c.name
         FROM invoices i JOIN companies c ON c.id = i.company_id
         WHERE i.public_token = $1",
    )
    .bind(token)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.map(|r| (r.get::<Uuid, _>(0), r.get::<Uuid, _>(1), r.get::<String, _>(2))))
}

/// Re-fetch an invoice using the public token for RLS rather than tenant scope.
/// Used by the public preview page.
pub async fn get_invoice_public(
    pool: &PgPool,
    token: &str,
    invoice_id: Uuid,
) -> AppResult<Option<InvoiceDetail>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT set_config('app.invoice_public_token', $1, true)")
        .bind(token)
        .execute(&mut *tx)
        .await?;

    let inv_row = sqlx::query(
        "SELECT i.id, i.invoice_number, i.status::text, i.issue_date, i.due_date,
                i.terms, i.currency, i.subtotal_cents, i.tax_cents, i.total_cents,
                i.paid_cents, i.memo, i.customer_notes, i.public_token,
                i.sent_at, i.last_sent_to,
                c.id, c.name, c.email, c.phone, c.address_line1, c.address_line2,
                c.city, c.state, c.postal_code, c.country, c.default_terms, c.notes
         FROM invoices i JOIN customers c ON c.id = i.customer_id
         WHERE i.id = $1 AND i.public_token = $2",
    )
    .bind(invoice_id)
    .bind(token)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(r) = inv_row else { tx.commit().await?; return Ok(None) };
    let customer = CustomerDetail {
        id: r.get(16), name: r.get(17), email: r.get(18), phone: r.get(19),
        address_line1: r.get(20), address_line2: r.get(21), city: r.get(22),
        state: r.get(23), postal_code: r.get(24), country: r.get(25),
        default_terms: r.get(26), notes: r.get(27),
    };
    let line_rows = sqlx::query(
        "SELECT description, quantity, unit_price_cents, amount_cents,
                tax_rate_pct, tax_cents, revenue_account_id
         FROM invoice_lines
         WHERE invoice_id = $1
         ORDER BY sort_order ASC",
    )
    .bind(invoice_id)
    .fetch_all(&mut *tx)
    .await?;
    let lines = line_rows.into_iter().map(|l| InvoiceLineRow {
        description: l.get(0), quantity: l.get(1), unit_price_cents: l.get(2),
        amount_cents: l.get(3), tax_rate_pct: l.get(4), tax_cents: l.get(5),
        revenue_account_id: l.get(6),
        revenue_account_name: String::new(),
    }).collect();
    let pay_rows = sqlx::query(
        "SELECT payment_date, amount_cents, method::text, reference
         FROM invoice_payments
         WHERE invoice_id = $1 ORDER BY payment_date DESC, created_at DESC",
    )
    .bind(invoice_id)
    .fetch_all(&mut *tx)
    .await?;
    let payments = pay_rows.into_iter().map(|p| InvoicePaymentRow {
        payment_date: p.get(0), amount_cents: p.get(1), method: p.get(2),
        reference: p.get(3),
        deposit_account_name: String::new(),
    }).collect();
    tx.commit().await?;
    Ok(Some(InvoiceDetail {
        id: r.get(0), invoice_number: r.get(1), status: r.get(2),
        issue_date: r.get(3), due_date: r.get(4), terms: r.get(5),
        currency: r.get(6), subtotal_cents: r.get(7), tax_cents: r.get(8),
        total_cents: r.get(9), paid_cents: r.get(10),
        memo: r.get(11), customer_notes: r.get(12), public_token: r.get(13),
        sent_at: r.get(14), last_sent_to: r.get(15),
        customer, lines, payments,
    }))
}

#[derive(Debug, Clone)]
pub struct DepositAccountOption {
    pub id: Uuid,
    pub display: String,
}

/// Asset accounts customers can deposit INTO (Cash/Bank), excluding A/R itself.
pub async fn list_deposit_accounts(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<DepositAccountOption>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        "SELECT id, account_number, name FROM accounts
         WHERE company_id = $1
           AND account_type = 'asset'
           AND is_active = true
           AND account_number <> '1200'
         ORDER BY account_number ASC",
    )
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|r| DepositAccountOption {
        id: r.get(0),
        display: format!("{} — {}", r.get::<String, _>(1), r.get::<String, _>(2)),
    }).collect())
}

pub async fn list_revenue_accounts(pool: &PgPool, company_id: Uuid) -> AppResult<Vec<DepositAccountOption>> {
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    set_tenant(&mut tx, company_id).await?;
    let rows = sqlx::query(
        "SELECT id, account_number, name FROM accounts
         WHERE company_id = $1 AND account_type = 'revenue' AND is_active = true
         ORDER BY account_number ASC",
    )
    .bind(company_id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|r| DepositAccountOption {
        id: r.get(0),
        display: format!("{} — {}", r.get::<String, _>(1), r.get::<String, _>(2)),
    }).collect())
}
