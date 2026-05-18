use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::PlaidConfig;

#[derive(Debug, thiserror::Error)]
pub enum PlaidError {
    #[error("plaid http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("plaid api error ({status}, {error_code}): {error_message}")]
    Api {
        status: u16,
        error_code: String,
        error_message: String,
    },
    #[error("plaid response missing field: {0}")]
    MissingField(&'static str),
}

#[derive(Clone)]
pub struct PlaidClient {
    config: PlaidConfig,
    http: reqwest::Client,
}

impl PlaidClient {
    pub fn new(config: PlaidConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    async fn post(&self, path: &str, mut body: Value) -> Result<Value, PlaidError> {
        if let Value::Object(ref mut map) = body {
            map.insert("client_id".into(), json!(self.config.client_id));
            map.insert("secret".into(), json!(self.config.secret));
        }
        let url = format!("{}{}", self.config.env.base_url(), path);
        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        let value: Value = resp.json().await?;
        if !status.is_success() {
            return Err(PlaidError::Api {
                status: status.as_u16(),
                error_code: value["error_code"].as_str().unwrap_or("unknown").to_string(),
                error_message: value["error_message"]
                    .as_str()
                    .unwrap_or("(no message)")
                    .to_string(),
            });
        }
        Ok(value)
    }

    /// `/link/token/create` — returns a link_token suitable for Plaid Link's `token` field.
    pub async fn create_link_token(&self, user_id: &str) -> Result<String, PlaidError> {
        let mut body = json!({
            "user": { "client_user_id": user_id },
            "client_name": "Accountir",
            "products": ["transactions"],
            "country_codes": ["US"],
            "language": "en",
        });
        if let Some(ref redirect) = self.config.redirect_uri {
            body["redirect_uri"] = json!(redirect);
        }
        if let Some(ref webhook) = self.config.webhook_url {
            body["webhook"] = json!(webhook);
        }
        let v = self.post("/link/token/create", body).await?;
        v["link_token"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or(PlaidError::MissingField("link_token"))
    }

    /// `/item/public_token/exchange` — returns (access_token, plaid_item_id).
    pub async fn exchange_public_token(
        &self,
        public_token: &str,
    ) -> Result<ExchangeResult, PlaidError> {
        let v = self
            .post(
                "/item/public_token/exchange",
                json!({ "public_token": public_token }),
            )
            .await?;
        Ok(ExchangeResult {
            access_token: v["access_token"]
                .as_str()
                .ok_or(PlaidError::MissingField("access_token"))?
                .to_string(),
            item_id: v["item_id"]
                .as_str()
                .ok_or(PlaidError::MissingField("item_id"))?
                .to_string(),
        })
    }

    /// `/transactions/sync` — incremental cursor-based sync.
    pub async fn transactions_sync(
        &self,
        access_token: &str,
        cursor: Option<&str>,
    ) -> Result<TransactionsSyncResult, PlaidError> {
        let mut body = json!({ "access_token": access_token });
        if let Some(c) = cursor {
            body["cursor"] = json!(c);
        }
        let v = self.post("/transactions/sync", body).await?;
        let next_cursor = v["next_cursor"]
            .as_str()
            .ok_or(PlaidError::MissingField("next_cursor"))?
            .to_string();
        let has_more = v["has_more"].as_bool().unwrap_or(false);
        let added: Vec<Value> = v["added"].as_array().cloned().unwrap_or_default();
        let modified: Vec<Value> = v["modified"].as_array().cloned().unwrap_or_default();
        let removed: Vec<Value> = v["removed"].as_array().cloned().unwrap_or_default();
        Ok(TransactionsSyncResult {
            added,
            modified,
            removed,
            next_cursor,
            has_more,
        })
    }

    /// `/statements/list` — returns statement metadata if the Statements product is
    /// enabled on the access token. Errors with PRODUCT_NOT_READY or INVALID_PRODUCT
    /// otherwise.
    pub async fn statements_list(&self, access_token: &str) -> Result<Value, PlaidError> {
        self.post("/statements/list", json!({ "access_token": access_token }))
            .await
    }

    /// `/transactions/get` — historical pull with explicit date range. Used for backfills
    /// older than the cursor-based `/transactions/sync` returns. Plaid's max look-back is
    /// up to 24 months depending on institution.
    pub async fn transactions_get(
        &self,
        access_token: &str,
        start_date: chrono::NaiveDate,
        end_date: chrono::NaiveDate,
    ) -> Result<Vec<Value>, PlaidError> {
        let mut all_txns: Vec<Value> = Vec::new();
        let mut offset = 0u32;
        let count = 500u32;
        loop {
            let v = self
                .post(
                    "/transactions/get",
                    json!({
                        "access_token": access_token,
                        "start_date": start_date.format("%Y-%m-%d").to_string(),
                        "end_date": end_date.format("%Y-%m-%d").to_string(),
                        "options": { "count": count, "offset": offset }
                    }),
                )
                .await?;
            let total = v["total_transactions"].as_u64().unwrap_or(0) as u32;
            let batch: Vec<Value> = v["transactions"].as_array().cloned().unwrap_or_default();
            let got = batch.len() as u32;
            all_txns.extend(batch);
            offset += got;
            if got == 0 || offset >= total {
                break;
            }
        }
        Ok(all_txns)
    }

    /// `/accounts/get` — returns metadata for every account on the item (no live balances fetch).
    pub async fn accounts_get(&self, access_token: &str) -> Result<Vec<Value>, PlaidError> {
        let v = self
            .post("/accounts/get", json!({ "access_token": access_token }))
            .await?;
        Ok(v["accounts"].as_array().cloned().unwrap_or_default())
    }

    /// `/accounts/balance/get` — returns live balances. Note: Plaid charges per call in production.
    pub async fn accounts_balance_get(&self, access_token: &str) -> Result<Vec<Value>, PlaidError> {
        let v = self
            .post(
                "/accounts/balance/get",
                json!({ "access_token": access_token }),
            )
            .await?;
        Ok(v["accounts"].as_array().cloned().unwrap_or_default())
    }

    /// `/item/remove` — invalidate the access_token on Plaid's side.
    pub async fn item_remove(&self, access_token: &str) -> Result<(), PlaidError> {
        self.post("/item/remove", json!({ "access_token": access_token }))
            .await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeResult {
    pub access_token: String,
    pub item_id: String,
}

#[derive(Debug, Clone)]
pub struct TransactionsSyncResult {
    pub added: Vec<Value>,
    pub modified: Vec<Value>,
    pub removed: Vec<Value>,
    pub next_cursor: String,
    pub has_more: bool,
}
