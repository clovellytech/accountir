//! Tool definitions exposed to the AI chat: read accounts, list entries,
//! create accounts, post journal entries, get balances.
//!
//! Each tool returns a JSON Value that becomes the `content` of a
//! tool_result block sent back to Claude.

use accountir_core::events::types::EventAccountType;
use chrono::NaiveDate;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::commands::{
    account::{create_account, CreateAccountInput},
    entry::{post_entry, EntryLineInput, PostEntryInput},
};
use crate::queries;

pub fn schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "list_accounts",
            "description": "List the company's chart of accounts. Returns id, account_number, name, type, currency for each.",
            "input_schema": {
                "type": "object",
                "properties": {},
            }
        }),
        json!({
            "name": "list_recent_entries",
            "description": "List the most recent journal entries (date, memo, total, reference). Default limit 20, max 100.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
                }
            }
        }),
        json!({
            "name": "get_account_balance",
            "description": "Return the running debit-positive balance for an account (looked up by account_number, e.g. '1000').",
            "input_schema": {
                "type": "object",
                "properties": {
                    "account_number": {"type": "string"}
                },
                "required": ["account_number"]
            }
        }),
        json!({
            "name": "create_account",
            "description": "Create a new account in the chart. Returns the new account id.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "account_number": {"type": "string", "description": "e.g. '1000'"},
                    "name": {"type": "string", "description": "e.g. 'Cash'"},
                    "type": {"type": "string", "enum": ["asset", "liability", "equity", "revenue", "expense"]},
                    "currency": {"type": "string", "default": "USD"}
                },
                "required": ["account_number", "name", "type"]
            }
        }),
        json!({
            "name": "post_journal_entry",
            "description": "Post a balanced double-entry journal entry. Lines are referred to by account_number. Positive amount = debit, negative = credit. Sum must be zero. Provide amounts in dollars (e.g. 100.00).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "date": {"type": "string", "description": "YYYY-MM-DD"},
                    "memo": {"type": "string"},
                    "reference": {"type": "string"},
                    "lines": {
                        "type": "array",
                        "minItems": 2,
                        "items": {
                            "type": "object",
                            "properties": {
                                "account_number": {"type": "string"},
                                "amount": {"type": "number", "description": "Dollars; positive=debit, negative=credit"},
                                "currency": {"type": "string", "default": "USD"},
                                "memo": {"type": "string"}
                            },
                            "required": ["account_number", "amount"]
                        }
                    }
                },
                "required": ["date", "memo", "lines"]
            }
        }),
    ]
}

pub struct ToolContext<'a> {
    pub pool: &'a PgPool,
    pub company_id: Uuid,
    pub user_id: Uuid,
}

pub async fn execute(name: &str, input: &Value, ctx: &ToolContext<'_>) -> Value {
    match name {
        "list_accounts" => list_accounts_tool(ctx).await,
        "list_recent_entries" => list_recent_entries_tool(ctx, input).await,
        "get_account_balance" => get_account_balance_tool(ctx, input).await,
        "create_account" => create_account_tool(ctx, input).await,
        "post_journal_entry" => post_entry_tool(ctx, input).await,
        other => json!({ "error": format!("unknown tool: {other}") }),
    }
}

async fn list_accounts_tool(ctx: &ToolContext<'_>) -> Value {
    match queries::list_accounts(ctx.pool, ctx.company_id).await {
        Ok(rows) => json!({
            "accounts": rows.iter().map(|a| json!({
                "id": a.id,
                "account_number": a.account_number,
                "name": a.name,
                "type": a.account_type,
                "currency": a.currency,
            })).collect::<Vec<_>>()
        }),
        Err(e) => json!({ "error": format!("{e}") }),
    }
}

async fn list_recent_entries_tool(ctx: &ToolContext<'_>, input: &Value) -> Value {
    let limit = input
        .get("limit")
        .and_then(|v| v.as_i64())
        .unwrap_or(20)
        .clamp(1, 100);
    match queries::list_entries(ctx.pool, ctx.company_id).await {
        Ok(rows) => {
            let truncated: Vec<_> = rows
                .into_iter()
                .take(limit as usize)
                .map(|e| {
                    json!({
                        "id": e.id,
                        "date": e.date,
                        "memo": e.memo,
                        "reference": e.reference,
                        "total_debits": format!("{:.2}", e.total_debits_cents as f64 / 100.0),
                    })
                })
                .collect();
            json!({ "entries": truncated })
        }
        Err(e) => json!({ "error": format!("{e}") }),
    }
}

async fn get_account_balance_tool(ctx: &ToolContext<'_>, input: &Value) -> Value {
    let acct_num = match input.get("account_number").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return json!({ "error": "missing account_number" }),
    };
    let row: Option<(Uuid, String, i64)> = sqlx::query_as(
        r#"
        SELECT a.id, a.name,
               COALESCE(SUM(jl.amount), 0)::BIGINT
        FROM accounts a
        LEFT JOIN journal_lines jl ON jl.account_id = a.id
        LEFT JOIN journal_entries je ON je.id = jl.entry_id AND je.is_void = false
        WHERE a.company_id = $1 AND a.account_number = $2 AND a.is_active = true
        GROUP BY a.id, a.name
        "#,
    )
    .bind(ctx.company_id)
    .bind(acct_num)
    .fetch_optional(ctx.pool)
    .await
    .ok()
    .flatten();
    match row {
        Some((id, name, cents)) => json!({
            "account_id": id,
            "account_number": acct_num,
            "name": name,
            "balance": format!("{:.2}", cents as f64 / 100.0),
            "balance_cents": cents,
        }),
        None => json!({ "error": format!("no active account with number '{acct_num}'") }),
    }
}

async fn create_account_tool(ctx: &ToolContext<'_>, input: &Value) -> Value {
    let acct_num = input
        .get("account_number")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = input.get("name").and_then(|v| v.as_str()).map(str::to_string);
    let typ_str = input.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let currency = input
        .get("currency")
        .and_then(|v| v.as_str())
        .map(str::to_uppercase);
    let (acct_num, name) = match (acct_num, name) {
        (Some(a), Some(n)) => (a, n),
        _ => return json!({ "error": "account_number and name required" }),
    };
    let typ = match typ_str {
        "asset" => EventAccountType::Asset,
        "liability" => EventAccountType::Liability,
        "equity" => EventAccountType::Equity,
        "revenue" => EventAccountType::Revenue,
        "expense" => EventAccountType::Expense,
        _ => return json!({ "error": "type must be one of: asset, liability, equity, revenue, expense" }),
    };
    match create_account(
        ctx.pool,
        ctx.company_id,
        ctx.user_id,
        CreateAccountInput {
            account_type: typ,
            account_number: acct_num.clone(),
            name: name.clone(),
            currency,
            description: None,
        },
    )
    .await
    {
        Ok(id) => json!({ "ok": true, "account_id": id, "account_number": acct_num, "name": name }),
        Err(e) => json!({ "error": format!("{e}") }),
    }
}

async fn post_entry_tool(ctx: &ToolContext<'_>, input: &Value) -> Value {
    let date_str = input.get("date").and_then(|v| v.as_str()).unwrap_or("");
    let memo = input
        .get("memo")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let reference = input
        .get("reference")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let date = match NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        Ok(d) => d,
        Err(_) => return json!({ "error": "date must be YYYY-MM-DD" }),
    };
    let lines_value = input.get("lines").and_then(|v| v.as_array());
    let lines_value = match lines_value {
        Some(l) => l,
        None => return json!({ "error": "lines required" }),
    };
    if lines_value.len() < 2 {
        return json!({ "error": "need at least 2 lines" });
    }

    // Resolve account_numbers → uuids and convert dollar amounts to cents
    let mut input_lines: Vec<EntryLineInput> = Vec::with_capacity(lines_value.len());
    let mut sum_cents = 0i64;
    for line in lines_value {
        let acct_num = match line.get("account_number").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return json!({ "error": "each line needs account_number" }),
        };
        let amount = match line.get("amount").and_then(|v| v.as_f64()) {
            Some(a) => a,
            None => return json!({ "error": "each line needs numeric amount" }),
        };
        let amount_cents = (amount * 100.0).round() as i64;
        sum_cents += amount_cents;
        let currency = line
            .get("currency")
            .and_then(|v| v.as_str())
            .unwrap_or("USD")
            .to_uppercase();
        let memo_l = line.get("memo").and_then(|v| v.as_str()).map(str::to_string);
        let acct_uuid: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM accounts WHERE company_id = $1 AND account_number = $2 AND is_active = true",
        )
        .bind(ctx.company_id)
        .bind(acct_num)
        .fetch_optional(ctx.pool)
        .await
        .ok()
        .flatten();
        let account_id = match acct_uuid {
            Some((id,)) => id,
            None => return json!({ "error": format!("no active account with number '{acct_num}'") }),
        };
        input_lines.push(EntryLineInput {
            account_id,
            amount: amount_cents,
            currency,
            memo: memo_l,
        });
    }
    if sum_cents != 0 {
        return json!({
            "error": format!("lines must balance to zero (got {} cents off)", sum_cents),
        });
    }
    match post_entry(
        ctx.pool,
        ctx.company_id,
        ctx.user_id,
        PostEntryInput {
            date,
            memo,
            reference,
            lines: input_lines,
        },
    )
    .await
    {
        Ok(id) => json!({ "ok": true, "entry_id": id }),
        Err(e) => json!({ "error": format!("{e}") }),
    }
}
