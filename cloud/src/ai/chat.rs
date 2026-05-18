//! Chat orchestration: load conversation, run Claude with tools, persist messages.

use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

use crate::ai::anthropic::{AnthropicClient, AnthropicError, Message};
use crate::ai::tools::{self, ToolContext};

const SYSTEM_PROMPT: &str = r#"You are an AI accounting assistant for a small-business double-entry accounting system called Accountir.

You can read the chart of accounts, list recent journal entries, and post new entries on behalf of the user. Always:
- Confirm what you're about to do before posting an entry, unless the user is unambiguously asking you to act now.
- Use the user's account_numbers (e.g. '1000', '4000') when posting; if the chart is empty, propose what to create.
- Remember: positive amounts are debits, negative are credits, and lines must sum to zero.
- Format money in dollars (e.g. $100.00) when speaking to the user; use dollar amounts when calling tools (the system multiplies by 100).
- Be concise and bookkeeping-precise.
- If a tool returns an error, explain it plainly and propose a fix; never silently retry without explaining.

You do not have access to bank feeds or external services beyond the tools provided. If asked about syncing Plaid, point the user at the Banks page in the UI."#;

const MAX_ITERATIONS: u32 = 5;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("anthropic: {0}")]
    Anthropic(#[from] AnthropicError),
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
    #[error("config: ANTHROPIC_API_KEY not set")]
    NoApiKey,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub role: String,
    pub content: Value,
}

pub async fn load_history(
    pool: &PgPool,
    user_id: Uuid,
    company_id: Uuid,
) -> Result<Vec<StoredMessage>, ChatError> {
    let rows = sqlx::query_as::<_, (String, Value)>(
        "SELECT role, content FROM chat_messages WHERE user_id = $1 AND company_id = $2 ORDER BY created_at ASC, id ASC",
    )
    .bind(user_id)
    .bind(company_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(role, content)| StoredMessage { role, content })
        .collect())
}

pub async fn append_message(
    pool: &PgPool,
    user_id: Uuid,
    company_id: Uuid,
    role: &str,
    content: &Value,
) -> Result<(), ChatError> {
    sqlx::query(
        "INSERT INTO chat_messages (user_id, company_id, role, content) VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id)
    .bind(company_id)
    .bind(role)
    .bind(content)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn clear_history(pool: &PgPool, user_id: Uuid, company_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM chat_messages WHERE user_id = $1 AND company_id = $2")
        .bind(user_id)
        .bind(company_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Send a user message, run the tool-use loop, persist everything,
/// return a list of new assistant messages produced (so UI can render them).
pub async fn send_message(
    pool: &PgPool,
    api_key: &str,
    user_id: Uuid,
    company_id: Uuid,
    user_text: String,
) -> Result<Vec<StoredMessage>, ChatError> {
    let client = AnthropicClient::new(api_key.to_string());

    // Persist user message.
    let user_msg_content = json!(user_text);
    append_message(pool, user_id, company_id, "user", &user_msg_content).await?;

    // Build the conversation for the API call from full history.
    let history = load_history(pool, user_id, company_id).await?;
    let mut messages: Vec<Message> = history
        .iter()
        .map(|m| Message {
            role: m.role.clone(),
            content: m.content.clone(),
        })
        .collect();

    let tools_json = tools::schemas();
    let ctx = ToolContext { pool, company_id, user_id };

    let mut new_assistant_messages: Vec<StoredMessage> = Vec::new();

    for _ in 0..MAX_ITERATIONS {
        let resp = client
            .create_message(SYSTEM_PROMPT, &messages, Some(&tools_json))
            .await?;

        // Persist assistant message verbatim — Anthropic requires we send back the
        // exact content blocks (with tool_use ids) on follow-up turns.
        let assistant_content = Value::Array(resp.content.clone());
        append_message(pool, user_id, company_id, "assistant", &assistant_content).await?;
        new_assistant_messages.push(StoredMessage {
            role: "assistant".to_string(),
            content: assistant_content.clone(),
        });
        messages.push(Message {
            role: "assistant".to_string(),
            content: assistant_content,
        });

        let stop = resp.stop_reason.as_deref().unwrap_or("");
        if stop != "tool_use" {
            break;
        }

        // Execute every tool_use block and collect tool_result blocks for the next turn.
        let mut tool_result_blocks: Vec<Value> = Vec::new();
        for block in resp.content {
            if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let tool_name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let tool_use_id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or(json!({}));
                let result = tools::execute(tool_name, &input, &ctx).await;
                tool_result_blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": result.to_string(),
                }));
            }
        }

        let user_tool_result = Value::Array(tool_result_blocks);
        append_message(pool, user_id, company_id, "user", &user_tool_result).await?;
        messages.push(Message {
            role: "user".to_string(),
            content: user_tool_result,
        });
    }

    Ok(new_assistant_messages)
}
