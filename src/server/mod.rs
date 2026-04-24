use axum::{
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use crate::config::AppConfig;
use crate::store::event_store::EventStore;

/// Active database connection held by the server.
struct ActiveDb {
    store: EventStore,
    db_path: PathBuf,
}

/// Shared server state — the database may or may not be open.
struct SharedState {
    db: std::sync::Mutex<Option<ActiveDb>>,
    http_client: reqwest::Client,
}

/// Handle passed to the TUI so it can set/clear the active database.
#[derive(Clone)]
pub struct ServerDb {
    inner: Arc<SharedState>,
}

impl ServerDb {
    /// Open a database and make it available to the sync server.
    pub fn set(&self, path: &std::path::Path) {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match EventStore::open(&canonical) {
            Ok(store) => {
                let mut guard = self.inner.db.lock().unwrap();
                *guard = Some(ActiveDb {
                    store,
                    db_path: canonical,
                });
            }
            Err(_) => {
                // Silently ignore — the TUI has its own store; this is best-effort.
            }
        }
    }

    /// Close the server's database connection (e.g. when the TUI closes a file).
    pub fn clear(&self) {
        let mut guard = self.inner.db.lock().unwrap();
        *guard = None;
    }
}

/// Legacy application state for the standalone `serve` command.
pub struct AppState {
    pub store: std::sync::Mutex<EventStore>,
    pub db_path: PathBuf,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    company_id: Option<String>,
    company_name: Option<String>,
}

#[derive(Deserialize)]
pub struct ImportBankCsvRequest {
    pub company_id: String,
    pub bank_id: String,
    pub bank_name: String,
    pub filename: String,
    pub content: String,
    pub downloaded_at: String,
}

#[derive(Deserialize)]
pub struct ImportBankFileRequest {
    pub company_id: String,
    pub bank_id: String,
    pub bank_name: String,
    pub file_path: String,
    pub downloaded_at: String,
}

#[derive(Serialize)]
struct ImportBankCsvResponse {
    success: bool,
    transaction_count: usize,
}

#[derive(Serialize)]
struct ErrorResponse {
    success: bool,
    error: String,
}

#[derive(Serialize)]
struct AccountBankInfo {
    bank_id: String,
    bank_name: String,
}

#[derive(Serialize)]
struct AccountWithBanks {
    id: String,
    name: String,
    account_type: String,
    account_number: String,
    banks: Vec<AccountBankInfo>,
}

#[derive(Serialize)]
struct AccountBanksResponse {
    accounts: Vec<AccountWithBanks>,
}

#[derive(Deserialize)]
struct LinkBankRequest {
    bank_id: String,
    bank_name: String,
    account_id: String,
}

#[derive(Serialize)]
struct LinkBankResponse {
    success: bool,
}

// ---------------------------------------------------------------------------
// Handlers for the background server (shared state with optional DB)
// ---------------------------------------------------------------------------

async fn bg_health(
    State(state): State<Arc<SharedState>>,
) -> Result<Json<HealthResponse>, StatusCode> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let (company_id, company_name) = {
        let conn = active.store.connection();
        conn.query_row(
            "SELECT company_id, name FROM company WHERE id = 'default'",
            [],
            |row| Ok((row.get::<_, String>(0).ok(), row.get::<_, String>(1).ok())),
        )
        .unwrap_or((None, None))
    };

    Ok(Json(HealthResponse {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        company_id,
        company_name,
    }))
}

async fn bg_account_banks(
    State(state): State<Arc<SharedState>>,
) -> Result<Json<AccountBanksResponse>, StatusCode> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let conn = active.store.connection();

    let mut stmt = conn
        .prepare(
            "SELECT a.id, a.name, a.account_type, a.account_number,
                    ba.bank_id, ba.bank_name
             FROM accounts a
             LEFT JOIN bank_accounts ba ON a.id = ba.account_id
             WHERE a.account_type IN ('asset', 'liability') AND a.is_active = 1
             ORDER BY a.account_type, a.account_number",
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut accounts: Vec<AccountWithBanks> = Vec::new();
    let mut last_id: Option<String> = None;

    for row in rows {
        let (id, name, account_type, account_number, bank_id, bank_name) =
            row.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        if last_id.as_deref() != Some(&id) {
            accounts.push(AccountWithBanks {
                id: id.clone(),
                name,
                account_type,
                account_number,
                banks: Vec::new(),
            });
            last_id = Some(id);
        }

        if let (Some(bid), Some(bname)) = (bank_id, bank_name) {
            accounts.last_mut().unwrap().banks.push(AccountBankInfo {
                bank_id: bid,
                bank_name: bname,
            });
        }
    }

    Ok(Json(AccountBanksResponse { accounts }))
}

async fn bg_link_bank(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<LinkBankRequest>,
) -> Result<Json<LinkBankResponse>, (StatusCode, Json<ErrorResponse>)> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;
    let conn = active.store.connection();

    conn.execute(
        "INSERT OR REPLACE INTO bank_accounts (bank_id, bank_name, account_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![req.bank_id, req.bank_name, req.account_id],
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Failed to link bank account: {}", e),
            }),
        )
    })?;

    Ok(Json(LinkBankResponse { success: true }))
}

async fn bg_import_bank_csv(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<ImportBankCsvRequest>,
) -> Result<Json<ImportBankCsvResponse>, (StatusCode, Json<ErrorResponse>)> {
    let db_path = {
        let guard = state.db.lock().unwrap();
        let active = guard.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                success: false,
                error: "No database open".to_string(),
            }),
        ))?;

        // Validate company_id matches the open database
        let conn = active.store.connection();
        let db_company: Option<(String, String)> = conn
            .query_row(
                "SELECT company_id, name FROM company WHERE id = 'default'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        match db_company {
            Some((db_company_id, db_company_name)) => {
                if req.company_id != db_company_id {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(ErrorResponse {
                            success: false,
                            error: format!(
                                "CSV is for company '{}' but this server is serving '{}'",
                                req.company_id, db_company_name
                            ),
                        }),
                    ));
                }
            }
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        success: false,
                        error: "No company configured in this database".to_string(),
                    }),
                ));
            }
        }

        active.db_path.clone()
    };

    // Count data rows (non-empty lines after the header)
    let lines: Vec<&str> = req.content.lines().collect();
    let transaction_count = if lines.len() > 1 {
        lines[1..].iter().filter(|l| !l.trim().is_empty()).count()
    } else {
        0
    };

    // Determine imports directory next to the database file
    let imports_dir = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("imports");

    // Save CSV file
    let sanitized_bank = req
        .bank_name
        .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let csv_filename = format!("{}_{}.csv", sanitized_bank, timestamp);

    let csv_path = imports_dir.join(&csv_filename);

    // Write file using spawn_blocking since it's I/O
    let content = req.content.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        std::fs::create_dir_all(&imports_dir)
            .map_err(|e| format!("Failed to create imports directory: {}", e))?;
        std::fs::write(&csv_path, &content)
            .map_err(|e| format!("Failed to write CSV file: {}", e))?;
        Ok(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Task join error: {}", e),
            }),
        )
    })?;

    if let Err(msg) = result {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: msg,
            }),
        ));
    }

    Ok(Json(ImportBankCsvResponse {
        success: true,
        transaction_count,
    }))
}

async fn bg_import_bank_file(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<ImportBankFileRequest>,
) -> Result<Json<ImportBankCsvResponse>, (StatusCode, Json<ErrorResponse>)> {
    let db_path = {
        let guard = state.db.lock().unwrap();
        let active = guard.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                success: false,
                error: "No database open".to_string(),
            }),
        ))?;

        // Validate company_id matches the open database
        let conn = active.store.connection();
        let db_company: Option<(String, String)> = conn
            .query_row(
                "SELECT company_id, name FROM company WHERE id = 'default'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        match db_company {
            Some((db_company_id, db_company_name)) => {
                if !req.company_id.is_empty() && req.company_id != db_company_id {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(ErrorResponse {
                            success: false,
                            error: format!(
                                "File is for company '{}' but this server is serving '{}'",
                                req.company_id, db_company_name
                            ),
                        }),
                    ));
                }
            }
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        success: false,
                        error: "No company configured in this database".to_string(),
                    }),
                ));
            }
        }

        active.db_path.clone()
    };

    // Read the file from disk
    let source_path = std::path::PathBuf::from(&req.file_path);
    let content = tokio::task::spawn_blocking({
        let path = source_path.clone();
        move || std::fs::read_to_string(&path)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Task join error: {}", e),
            }),
        )
    })?
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                success: false,
                error: format!("Failed to read file '{}': {}", req.file_path, e),
            }),
        )
    })?;

    // Count data rows (non-empty lines after the header)
    let lines: Vec<&str> = content.lines().collect();
    let transaction_count = if lines.len() > 1 {
        lines[1..].iter().filter(|l| !l.trim().is_empty()).count()
    } else {
        0
    };

    // Determine imports directory next to the database file
    let imports_dir = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("imports");

    // Copy file to imports directory with standardized name
    let sanitized_bank = req
        .bank_name
        .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let extension = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("csv");
    let dest_filename = format!("{}_{}.{}", sanitized_bank, timestamp, extension);
    let dest_path = imports_dir.join(&dest_filename);

    let dest_path_clone = dest_path.clone();
    let dest_filename_clone = dest_filename.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        std::fs::create_dir_all(&imports_dir)
            .map_err(|e| format!("Failed to create imports directory: {}", e))?;
        std::fs::write(&dest_path, &content).map_err(|e| format!("Failed to write file: {}", e))?;
        Ok(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Task join error: {}", e),
            }),
        )
    })?;

    if let Err(msg) = result {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: msg,
            }),
        ));
    }

    // Insert record into pending_imports
    {
        let guard = state.db.lock().unwrap();
        if let Some(active) = guard.as_ref() {
            let conn = active.store.connection();
            let _ = conn.execute(
                "INSERT INTO pending_imports (file_path, file_name, bank_id, bank_name, transaction_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    dest_path_clone.to_string_lossy(),
                    dest_filename_clone,
                    req.bank_id,
                    req.bank_name,
                    transaction_count as i64
                ],
            );
        }
    }

    Ok(Json(ImportBankCsvResponse {
        success: true,
        transaction_count,
    }))
}

/// Start the background sync server and return a handle the TUI can use to
/// set/clear the active database.  Returns `None` if the port is already in use.
pub async fn start_server_task() -> Option<ServerDb> {
    let shared = Arc::new(SharedState {
        db: std::sync::Mutex::new(None),
        http_client: reqwest::Client::new(),
    });

    let cors = CorsLayer::very_permissive();

    let app = Router::new()
        .route("/health", get(bg_health))
        .route("/accounts/banks", get(bg_account_banks))
        .route("/accounts/link-bank", post(bg_link_bank))
        .route("/import/bank-csv", post(bg_import_bank_csv))
        .route("/import/bank-file", post(bg_import_bank_file))
        // Plaid integration routes
        .route("/plaid/config", get(plaid_config))
        .route("/plaid/link-token", post(plaid_link_token))
        .route("/plaid/exchange-token", post(plaid_exchange_token))
        .route("/plaid/sync", post(plaid_sync))
        .route("/plaid/staged", get(plaid_staged_list))
        .route("/plaid/staged/import-transfer", post(plaid_import_transfer))
        .route("/plaid/staged/reject-transfer", post(plaid_reject_transfer))
        .route("/plaid/staged/import-all", post(plaid_import_all))
        .route("/plaid/items", get(plaid_items))
        .route("/plaid/link", get(plaid_link_page))
        .layer(cors)
        .with_state(shared.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], 9876));

    let listener = tokio::net::TcpListener::bind(addr).await.ok()?;

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Some(ServerDb { inner: shared })
}

// ---------------------------------------------------------------------------
// Plaid integration handlers
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PlaidConfigResponse {
    configured: bool,
}

async fn plaid_config(State(_state): State<Arc<SharedState>>) -> Json<PlaidConfigResponse> {
    let config = AppConfig::load();
    Json(PlaidConfigResponse {
        configured: config.plaid.proxy_url.is_some(),
    })
}

#[derive(Serialize)]
struct PlaidLinkTokenResponse {
    link_token: String,
}

async fn plaid_link_token(
    State(state): State<Arc<SharedState>>,
) -> Result<Json<PlaidLinkTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    let plaid_cfg = get_plaid_config(&state)?;

    let mut req = state
        .http_client
        .post(format!("{}/plaid/create-link-token", plaid_cfg.proxy_url));
    if let Some(ref key) = plaid_cfg.api_key {
        req = req.bearer_auth(key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| proxy_error(format!("Failed to contact proxy: {}", e)))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(proxy_error(format!("Proxy error: {}", text)));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| proxy_error(format!("Parse error: {}", e)))?;

    let link_token = body["link_token"]
        .as_str()
        .ok_or_else(|| proxy_error("Missing link_token in response".to_string()))?
        .to_string();

    Ok(Json(PlaidLinkTokenResponse { link_token }))
}

#[derive(Deserialize)]
struct PlaidExchangeTokenRequest {
    public_token: String,
    institution: PlaidInstitutionInfo,
    accounts: Vec<PlaidLinkAccountInfo>,
}

#[derive(Deserialize, Serialize)]
struct PlaidInstitutionInfo {
    institution_id: String,
    name: String,
}

#[derive(Deserialize, Serialize, Clone)]
struct PlaidLinkAccountInfo {
    #[serde(alias = "account_id")]
    id: String,
    name: String,
    official_name: Option<String>,
    #[serde(rename = "type")]
    account_type: String,
    mask: Option<String>,
}

#[derive(Serialize)]
struct PlaidExchangeTokenResponse {
    success: bool,
    item_id: String,
}

async fn plaid_exchange_token(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<PlaidExchangeTokenRequest>,
) -> Result<Json<PlaidExchangeTokenResponse>, (StatusCode, Json<ErrorResponse>)> {
    let plaid_cfg = get_plaid_config(&state)?;

    // Forward to proxy
    let proxy_body = serde_json::json!({
        "public_token": req.public_token,
        "institution": {
            "institution_id": req.institution.institution_id,
            "name": req.institution.name,
        },
        "accounts": req.accounts.iter().map(|a| serde_json::json!({
            "account_id": a.id,
            "name": a.name,
            "official_name": a.official_name,
            "type": a.account_type,
            "mask": a.mask,
        })).collect::<Vec<_>>(),
    });

    let mut req_builder = state
        .http_client
        .post(format!("{}/plaid/exchange-token", plaid_cfg.proxy_url));
    if let Some(ref key) = plaid_cfg.api_key {
        req_builder = req_builder.bearer_auth(key);
    }
    let resp = req_builder
        .json(&proxy_body)
        .send()
        .await
        .map_err(|e| proxy_error(format!("Failed to contact proxy: {}", e)))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(proxy_error(format!("Proxy error: {}", text)));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| proxy_error(format!("Parse error: {}", e)))?;

    let proxy_item_id = body["item_id"]
        .as_str()
        .ok_or_else(|| proxy_error("Missing item_id".to_string()))?
        .to_string();

    // Create local event
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;

    let plaid_accounts: Vec<crate::events::types::PlaidAccountInfo> = req
        .accounts
        .iter()
        .map(|a| crate::events::types::PlaidAccountInfo {
            plaid_account_id: a.id.clone(),
            name: a.name.clone(),
            official_name: a.official_name.clone(),
            account_type: a.account_type.clone(),
            mask: a.mask.clone(),
        })
        .collect();

    // We can't use PlaidCommands because it needs &mut store but we're in a handler
    // with a Mutex guard. Instead, create the event directly.
    let item_id = uuid::Uuid::new_v4().to_string();
    let event = crate::events::types::Event::PlaidItemConnected {
        item_id: item_id.clone(),
        proxy_item_id: proxy_item_id.clone(),
        institution_name: req.institution.name.clone(),
        plaid_accounts,
    };
    let envelope = crate::events::types::EventEnvelope::new(event, "plaid-link".to_string());

    // We need mutable access — but ActiveDb.store is behind a shared ref.
    // The store is already opened exclusively; cast away immutability via
    // interior-mutable pattern that EventStore uses for the Connection.
    // Actually, EventStore::open stores a rusqlite::Connection which is not Sync.
    // The background server opens its own store — we can just use it directly.
    // The Mutex<Option<ActiveDb>> gives us exclusive access.
    // However, ActiveDb.store is not &mut. We need to refactor slightly.
    // For now, use raw SQL instead.

    let conn = active.store.connection();

    // Manually insert the event + projections
    let payload = serde_json::to_string(&envelope.event).unwrap();
    let event_type = envelope.event.event_type();
    let timestamp = envelope.timestamp.to_rfc3339();
    let hash_input = format!("{}{}{}", event_type, payload, timestamp);
    let hash = sha2_hash(hash_input.as_bytes());

    conn.execute(
        "INSERT INTO events (event_type, payload, hash, user_id, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![event_type, payload, hash, envelope.user_id, timestamp],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
        success: false,
        error: format!("DB error: {}", e),
    })))?;

    let event_id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
        .unwrap_or(0);

    // Project: insert plaid_items
    conn.execute(
        "INSERT OR REPLACE INTO plaid_items (id, proxy_item_id, institution_name, status, connected_at_event) VALUES (?1, ?2, ?3, 'active', ?4)",
        rusqlite::params![item_id, proxy_item_id, req.institution.name, event_id],
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
        success: false,
        error: format!("DB error: {}", e),
    })))?;

    for acct in &req.accounts {
        conn.execute(
            "INSERT OR REPLACE INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type, mask) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![item_id, acct.id, acct.name, acct.account_type, acct.mask],
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
            success: false,
            error: format!("DB error: {}", e),
        })))?;
    }

    Ok(Json(PlaidExchangeTokenResponse {
        success: true,
        item_id,
    }))
}

#[derive(Deserialize)]
struct PlaidSyncRequest {
    item_id: String,
}

#[derive(Serialize)]
struct PlaidSyncResponse {
    staged: u32,
    skipped: u32,
    transfer_candidates: u32,
}

async fn plaid_sync(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<PlaidSyncRequest>,
) -> Result<Json<PlaidSyncResponse>, (StatusCode, Json<ErrorResponse>)> {
    let plaid_cfg = get_plaid_config(&state)?;

    // Look up proxy_item_id from local DB
    let proxy_item_id = {
        let guard = state.db.lock().unwrap();
        let active = guard.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                success: false,
                error: "No database open".to_string(),
            }),
        ))?;
        let conn = active.store.connection();
        conn.query_row(
            "SELECT proxy_item_id FROM plaid_items WHERE id = ?1 AND status = 'active'",
            [&req.item_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    success: false,
                    error: "Item not found".to_string(),
                }),
            )
        })?
    };

    // Call proxy sync
    let mut req_builder = state
        .http_client
        .post(format!("{}/plaid/sync", plaid_cfg.proxy_url));
    if let Some(ref key) = plaid_cfg.api_key {
        req_builder = req_builder.bearer_auth(key);
    }
    let resp = req_builder
        .json(&serde_json::json!({ "item_id": proxy_item_id }))
        .send()
        .await
        .map_err(|e| proxy_error(format!("Failed to contact proxy: {}", e)))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(proxy_error(format!("Proxy sync error: {}", text)));
    }

    let sync_body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| proxy_error(format!("Parse error: {}", e)))?;

    let added_txns: Vec<crate::commands::plaid_commands::SyncedTransaction> =
        serde_json::from_value(sync_body["added"].clone()).unwrap_or_default();

    // Stage transactions instead of directly importing
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;
    let conn = active.store.connection();

    // Load account mappings for this item
    let mappings: std::collections::HashMap<String, Option<String>> = conn
        .prepare(
            "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts WHERE item_id = ?1",
        )
        .and_then(|mut stmt| {
            stmt.query_map([&req.item_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let mut staged = 0u32;
    let mut skipped = 0u32;

    for txn in &added_txns {
        if txn.pending {
            skipped += 1;
            continue;
        }

        // Skip if account is not mapped to a local account
        let local_account_id = mappings.get(&txn.account_id).and_then(|o| o.clone());
        if local_account_id.is_none() {
            skipped += 1;
            continue;
        }

        // Skip if already staged or already imported
        let already_exists: bool = conn
            .query_row(
                "SELECT 1 FROM plaid_staged_transactions WHERE plaid_transaction_id = ?1
                 UNION ALL
                 SELECT 1 FROM plaid_imported_transactions WHERE plaid_transaction_id = ?1
                 LIMIT 1",
                [&txn.transaction_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if already_exists {
            skipped += 1;
            continue;
        }

        let amount_cents = (txn.amount * 100.0).round() as i64;
        let currency = txn.iso_currency_code.as_deref().unwrap_or("USD");
        let id = uuid::Uuid::new_v4().to_string();
        let payment_meta_json = txn
            .payment_meta
            .as_ref()
            .filter(|pm| !pm.is_empty())
            .and_then(|pm| serde_json::to_string(pm).ok());

        conn.execute(
            "INSERT INTO plaid_staged_transactions
             (id, item_id, plaid_transaction_id, plaid_account_id, local_account_id,
              amount_cents, date, name, merchant_name, currency, status, payment_meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11)",
            rusqlite::params![
                id,
                req.item_id,
                txn.transaction_id,
                txn.account_id,
                local_account_id,
                amount_cents,
                txn.date,
                txn.name,
                txn.merchant_name,
                currency,
                payment_meta_json
            ],
        )
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    success: false,
                    error: format!("DB error: {}", e),
                }),
            )
        })?;

        staged += 1;
    }

    // Update last_synced_at
    let now = chrono::Utc::now().to_rfc3339();
    let _ = conn.execute(
        "UPDATE plaid_items SET last_synced_at = ?1 WHERE id = ?2",
        rusqlite::params![now, req.item_id],
    );

    // Run transfer detection
    let transfer_candidates = crate::commands::plaid_commands::detect_transfers(conn).unwrap_or(0);

    Ok(Json(PlaidSyncResponse {
        staged,
        skipped,
        transfer_candidates,
    }))
}

// ---------------------------------------------------------------------------
// Staged transaction review endpoints
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PlaidStagedListResponse {
    transfer_candidates: Vec<TransferCandidateJson>,
    unmatched: Vec<StagedTransactionJson>,
}

#[derive(Serialize)]
struct TransferCandidateJson {
    id: String,
    confidence: f64,
    txn1: StagedTransactionJson,
    txn2: StagedTransactionJson,
}

#[derive(Serialize)]
struct StagedTransactionJson {
    id: String,
    date: String,
    name: String,
    merchant_name: Option<String>,
    amount_cents: i64,
    local_account_id: Option<String>,
    local_account_name: Option<String>,
    currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    payment_meta: Option<crate::commands::plaid_commands::PaymentMeta>,
}

async fn plaid_staged_list(
    State(state): State<Arc<SharedState>>,
) -> Result<Json<PlaidStagedListResponse>, (StatusCode, Json<ErrorResponse>)> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;
    let conn = active.store.connection();

    let candidates =
        crate::commands::plaid_commands::load_pending_transfers(conn).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    success: false,
                    error: format!("DB error: {}", e),
                }),
            )
        })?;

    let unmatched = crate::commands::plaid_commands::load_pending_staged(conn).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("DB error: {}", e),
            }),
        )
    })?;

    fn resolve_account_name(conn: &rusqlite::Connection, id: &Option<String>) -> Option<String> {
        id.as_ref().and_then(|aid| {
            conn.query_row("SELECT name FROM accounts WHERE id = ?1", [aid], |row| {
                row.get(0)
            })
            .ok()
        })
    }

    fn to_json(
        conn: &rusqlite::Connection,
        t: &crate::commands::plaid_commands::StagedTransaction,
    ) -> StagedTransactionJson {
        StagedTransactionJson {
            id: t.id.clone(),
            date: t.date.clone(),
            name: t.name.clone(),
            merchant_name: t.merchant_name.clone(),
            amount_cents: t.amount_cents,
            local_account_id: t.local_account_id.clone(),
            local_account_name: resolve_account_name(conn, &t.local_account_id),
            currency: t.currency.clone(),
            payment_meta: t.payment_meta.clone(),
        }
    }

    let transfer_candidates: Vec<TransferCandidateJson> = candidates
        .iter()
        .map(|c| TransferCandidateJson {
            id: c.id.clone(),
            confidence: c.confidence,
            txn1: to_json(conn, &c.txn1),
            txn2: to_json(conn, &c.txn2),
        })
        .collect();

    let unmatched_json: Vec<StagedTransactionJson> =
        unmatched.iter().map(|t| to_json(conn, t)).collect();

    Ok(Json(PlaidStagedListResponse {
        transfer_candidates,
        unmatched: unmatched_json,
    }))
}

#[derive(Deserialize)]
struct ImportTransferRequest {
    candidate_id: String,
}

async fn plaid_import_transfer(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<ImportTransferRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;
    let conn = active.store.connection();

    // Load the transfer pair
    let (txn1_id, txn2_id): (String, String) = conn.query_row(
        "SELECT staged_txn_id_1, staged_txn_id_2 FROM plaid_transfer_candidates WHERE id = ?1 AND status = 'pending'",
        [&req.candidate_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(|_| (StatusCode::NOT_FOUND, Json(ErrorResponse {
        success: false,
        error: "Transfer candidate not found".to_string(),
    })))?;

    // Load both transactions
    let txn1 = load_staged_sync(conn, &txn1_id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("DB error: {}", e),
            }),
        )
    })?;
    let txn2 = load_staged_sync(conn, &txn2_id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("DB error: {}", e),
            }),
        )
    })?;

    // Plaid amounts are positive when money leaves the account, negative when it arrives.
    // The positive-amount side is the "from" account (money leaving), negative is "to".
    let (from_txn, to_txn) = if txn1.4 > 0 {
        (&txn1, &txn2)
    } else {
        (&txn2, &txn1)
    };
    let from_account = from_txn.3.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            success: false,
            error: "Source account not mapped".to_string(),
        }),
    ))?;
    let to_account = to_txn.3.as_ref().ok_or((
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            success: false,
            error: "Destination account not mapped".to_string(),
        }),
    ))?;

    let date = chrono::NaiveDate::parse_from_str(&from_txn.5, "%Y-%m-%d")
        .unwrap_or_else(|_| chrono::Utc::now().date_naive());
    let abs_amount = (from_txn.4 as i64).unsigned_abs() as i64;
    let entry_id = uuid::Uuid::new_v4().to_string();
    let memo = format!("Transfer: {}", from_txn.7.as_deref().unwrap_or(&from_txn.6));
    let currency = &from_txn.8;

    let entry_event = crate::events::types::Event::JournalEntryPosted {
        entry_id: entry_id.clone(),
        date,
        memo: memo.clone(),
        lines: vec![
            crate::events::types::JournalLineData {
                line_id: format!("{}-line-1", entry_id),
                account_id: from_account.clone(),
                amount: -abs_amount,
                currency: currency.clone(),
                exchange_rate: None,
                memo: None,
            },
            crate::events::types::JournalLineData {
                line_id: format!("{}-line-2", entry_id),
                account_id: to_account.clone(),
                amount: abs_amount,
                currency: currency.clone(),
                exchange_rate: None,
                memo: None,
            },
        ],
        reference: Some(format!("transfer:{}:{}", from_txn.2, to_txn.2)),
        source: Some(crate::events::types::JournalEntrySource::Plaid),
    };

    let payload = serde_json::to_string(&entry_event).unwrap();
    let event_type = entry_event.event_type();
    let timestamp = chrono::Utc::now().to_rfc3339();
    let hash = sha2_hash(format!("{}{}{}", event_type, payload, timestamp).as_bytes());

    conn.execute(
        "INSERT INTO events (event_type, payload, hash, user_id, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![event_type, payload, hash, "plaid-sync", timestamp],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { success: false, error: format!("DB error: {}", e) })))?;

    let ev_id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
        .unwrap_or(0);

    conn.execute(
        "INSERT INTO journal_entries (id, date, memo, reference, source, is_void, posted_at_event) VALUES (?1, ?2, ?3, ?4, 'plaid', 0, ?5)",
        rusqlite::params![entry_id, date.to_string(), memo, format!("transfer:{}:{}", from_txn.2, to_txn.2), ev_id],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { success: false, error: format!("DB error: {}", e) })))?;

    conn.execute(
        "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, is_cleared) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![format!("{}-line-1", entry_id), entry_id, from_account, -abs_amount, currency],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { success: false, error: format!("DB error: {}", e) })))?;

    conn.execute(
        "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, is_cleared) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        rusqlite::params![format!("{}-line-2", entry_id), entry_id, to_account, abs_amount, currency],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { success: false, error: format!("DB error: {}", e) })))?;

    // Dedup records for both transactions
    conn.execute(
        "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![from_txn.2, from_txn.1, entry_id],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { success: false, error: format!("DB error: {}", e) })))?;
    conn.execute(
        "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![to_txn.2, to_txn.1, entry_id],
    ).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse { success: false, error: format!("DB error: {}", e) })))?;

    // Update statuses
    conn.execute(
        "UPDATE plaid_staged_transactions SET status = 'imported' WHERE id IN (?1, ?2)",
        rusqlite::params![txn1_id, txn2_id],
    )
    .ok();
    conn.execute(
        "UPDATE plaid_transfer_candidates SET status = 'confirmed' WHERE id = ?1",
        [&req.candidate_id],
    )
    .ok();

    Ok(Json(
        serde_json::json!({ "success": true, "entry_id": entry_id }),
    ))
}

#[derive(Deserialize)]
struct RejectTransferRequest {
    candidate_id: String,
}

async fn plaid_reject_transfer(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<RejectTransferRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;
    let conn = active.store.connection();

    crate::commands::plaid_commands::reject_transfer(conn, &req.candidate_id).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("DB error: {}", e),
            }),
        )
    })?;

    Ok(Json(serde_json::json!({ "success": true })))
}

async fn plaid_import_all(
    State(state): State<Arc<SharedState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;
    let conn = active.store.connection();

    // Import all pending transfer candidates
    let candidate_ids: Vec<String> = conn
        .prepare("SELECT id FROM plaid_transfer_candidates WHERE status = 'pending'")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get(0))
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    let mut transfers_imported = 0u32;
    // For each candidate, we'd need full import logic - for the server path,
    // the TUI handles this via PlaidCommands which has &mut EventStore.
    // The server endpoint is primarily for the browser extension.
    // For now, return the counts and let the TUI handle actual imports.

    let uncategorized = find_or_create_uncategorized_sync(conn).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("DB error: {}", e),
            }),
        )
    })?;

    let mut unmatched_imported = 0u32;

    // First import all transfer candidates
    for cid in &candidate_ids {
        // Load pair
        if let Ok((tid1, tid2)) = conn.query_row(
            "SELECT staged_txn_id_1, staged_txn_id_2 FROM plaid_transfer_candidates WHERE id = ?1",
            [cid],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ) {
            let txn1 = load_staged_sync(conn, &tid1);
            let txn2 = load_staged_sync(conn, &tid2);
            if let (Ok(t1), Ok(t2)) = (txn1, txn2) {
                let (from_t, to_t) = if t1.4 > 0 { (&t1, &t2) } else { (&t2, &t1) };
                if let (Some(from_acct), Some(to_acct)) = (&from_t.3, &to_t.3) {
                    let date = chrono::NaiveDate::parse_from_str(&from_t.5, "%Y-%m-%d")
                        .unwrap_or_else(|_| chrono::Utc::now().date_naive());
                    let abs_amount = (from_t.4 as i64).unsigned_abs() as i64;
                    let entry_id = uuid::Uuid::new_v4().to_string();
                    let memo = format!("Transfer: {}", from_t.7.as_deref().unwrap_or(&from_t.6));

                    let entry_event = crate::events::types::Event::JournalEntryPosted {
                        entry_id: entry_id.clone(),
                        date,
                        memo: memo.clone(),
                        lines: vec![
                            crate::events::types::JournalLineData {
                                line_id: format!("{}-line-1", entry_id),
                                account_id: from_acct.clone(),
                                amount: -abs_amount,
                                currency: from_t.8.clone(),
                                exchange_rate: None,
                                memo: None,
                            },
                            crate::events::types::JournalLineData {
                                line_id: format!("{}-line-2", entry_id),
                                account_id: to_acct.clone(),
                                amount: abs_amount,
                                currency: to_t.8.clone(),
                                exchange_rate: None,
                                memo: None,
                            },
                        ],
                        reference: Some(format!("transfer:{}:{}", from_t.2, to_t.2)),
                        source: Some(crate::events::types::JournalEntrySource::Plaid),
                    };

                    let payload = serde_json::to_string(&entry_event).unwrap();
                    let event_type = entry_event.event_type();
                    let timestamp = chrono::Utc::now().to_rfc3339();
                    let hash =
                        sha2_hash(format!("{}{}{}", event_type, payload, timestamp).as_bytes());

                    if conn.execute(
                        "INSERT INTO events (event_type, payload, hash, user_id, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![event_type, payload, hash, "plaid-sync", timestamp],
                    ).is_ok() {
                        let ev_id: i64 = conn.query_row("SELECT last_insert_rowid()", [], |row| row.get(0)).unwrap_or(0);
                        let _ = conn.execute(
                            "INSERT INTO journal_entries (id, date, memo, reference, source, is_void, posted_at_event) VALUES (?1, ?2, ?3, ?4, 'plaid', 0, ?5)",
                            rusqlite::params![entry_id, date.to_string(), memo, format!("transfer:{}:{}", from_t.2, to_t.2), ev_id],
                        );
                        let _ = conn.execute(
                            "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, is_cleared) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                            rusqlite::params![format!("{}-line-1", entry_id), entry_id, from_acct, -abs_amount, from_t.8],
                        );
                        let _ = conn.execute(
                            "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, is_cleared) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                            rusqlite::params![format!("{}-line-2", entry_id), entry_id, to_acct, abs_amount, to_t.8],
                        );
                        let _ = conn.execute("INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                            rusqlite::params![from_t.2, from_t.1, entry_id]);
                        let _ = conn.execute("INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                            rusqlite::params![to_t.2, to_t.1, entry_id]);
                        let _ = conn.execute("UPDATE plaid_staged_transactions SET status = 'imported' WHERE id IN (?1, ?2)",
                            rusqlite::params![tid1, tid2]);
                        let _ = conn.execute("UPDATE plaid_transfer_candidates SET status = 'confirmed' WHERE id = ?1", [cid]);
                        transfers_imported += 1;
                    }
                }
            }
        }
    }

    // Now import remaining pending as uncategorized
    // Re-query since some may have been imported as transfers
    let remaining: Vec<(String, String, String, Option<String>, i64, String, String, Option<String>, String)> = conn
        .prepare(
            "SELECT id, item_id, plaid_transaction_id, local_account_id, amount_cents, date, name, merchant_name, currency
             FROM plaid_staged_transactions WHERE status = 'pending'"
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?))
            }).map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    for (
        id,
        item_id,
        plaid_txn_id,
        local_account_id,
        amount_cents,
        date_str,
        name,
        merchant_name,
        currency,
    ) in &remaining
    {
        let bank_account = local_account_id.as_deref().unwrap_or(&uncategorized);
        let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
            .unwrap_or_else(|_| chrono::Utc::now().date_naive());
        let entry_id = uuid::Uuid::new_v4().to_string();
        let memo = merchant_name.as_deref().unwrap_or(name);

        let entry_event = crate::events::types::Event::JournalEntryPosted {
            entry_id: entry_id.clone(),
            date,
            memo: memo.to_string(),
            lines: vec![
                crate::events::types::JournalLineData {
                    line_id: format!("{}-line-1", entry_id),
                    account_id: bank_account.to_string(),
                    amount: -amount_cents,
                    currency: currency.clone(),
                    exchange_rate: None,
                    memo: None,
                },
                crate::events::types::JournalLineData {
                    line_id: format!("{}-line-2", entry_id),
                    account_id: uncategorized.clone(),
                    amount: *amount_cents,
                    currency: currency.clone(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: Some(plaid_txn_id.clone()),
            source: Some(crate::events::types::JournalEntrySource::Plaid),
        };

        let payload = serde_json::to_string(&entry_event).unwrap();
        let event_type = entry_event.event_type();
        let timestamp = chrono::Utc::now().to_rfc3339();
        let hash = sha2_hash(format!("{}{}{}", event_type, payload, timestamp).as_bytes());

        if conn.execute(
            "INSERT INTO events (event_type, payload, hash, user_id, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![event_type, payload, hash, "plaid-sync", timestamp],
        ).is_ok() {
            let ev_id: i64 = conn.query_row("SELECT last_insert_rowid()", [], |row| row.get(0)).unwrap_or(0);
            let _ = conn.execute(
                "INSERT INTO journal_entries (id, date, memo, reference, source, is_void, posted_at_event) VALUES (?1, ?2, ?3, ?4, 'plaid', 0, ?5)",
                rusqlite::params![entry_id, date.to_string(), memo, plaid_txn_id, ev_id],
            );
            let _ = conn.execute(
                "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, is_cleared) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                rusqlite::params![format!("{}-line-1", entry_id), entry_id, bank_account, -amount_cents, currency],
            );
            let _ = conn.execute(
                "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, is_cleared) VALUES (?1, ?2, ?3, ?4, ?5, 0)",
                rusqlite::params![format!("{}-line-2", entry_id), entry_id, uncategorized, *amount_cents, currency],
            );
            let _ = conn.execute(
                "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![plaid_txn_id, item_id, entry_id],
            );
            let _ = conn.execute("UPDATE plaid_staged_transactions SET status = 'imported' WHERE id = ?1", [id]);
            unmatched_imported += 1;
        }
    }

    Ok(Json(serde_json::json!({
        "success": true,
        "transfers_imported": transfers_imported,
        "unmatched_imported": unmatched_imported
    })))
}

/// Load a staged transaction as a tuple for server-side processing.
/// Returns (id, item_id, plaid_txn_id, local_account_id, amount_cents, date, name, merchant_name, currency)
fn load_staged_sync(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<
    (
        String,
        String,
        String,
        Option<String>,
        i64,
        String,
        String,
        Option<String>,
        String,
    ),
    rusqlite::Error,
> {
    conn.query_row(
        "SELECT id, item_id, plaid_transaction_id, local_account_id, amount_cents, date, name, merchant_name, currency
         FROM plaid_staged_transactions WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?)),
    )
}

#[derive(Serialize)]
struct PlaidItemInfo {
    id: String,
    institution_name: String,
    status: String,
    last_synced_at: Option<String>,
    accounts: Vec<PlaidLocalAccountInfo>,
}

#[derive(Serialize)]
struct PlaidLocalAccountInfo {
    plaid_account_id: String,
    name: String,
    account_type: String,
    mask: Option<String>,
    local_account_id: Option<String>,
    local_account_name: Option<String>,
}

#[derive(Serialize)]
struct PlaidItemsResponse {
    items: Vec<PlaidItemInfo>,
}

async fn plaid_items(
    State(state): State<Arc<SharedState>>,
) -> Result<Json<PlaidItemsResponse>, StatusCode> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let conn = active.store.connection();

    let mut stmt = conn
        .prepare("SELECT id, institution_name, status, last_synced_at FROM plaid_items ORDER BY rowid DESC")
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let items: Vec<(String, String, String, Option<String>)> = stmt
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .filter_map(|r| r.ok())
        .collect();

    let mut result = Vec::new();
    for (id, institution_name, status, last_synced_at) in items {
        let mut acct_stmt = conn
            .prepare(
                "SELECT pa.plaid_account_id, pa.name, pa.account_type, pa.mask, pa.local_account_id, a.name
                 FROM plaid_local_accounts pa
                 LEFT JOIN accounts a ON pa.local_account_id = a.id
                 WHERE pa.item_id = ?1",
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let accounts: Vec<PlaidLocalAccountInfo> = acct_stmt
            .query_map([&id], |row| {
                Ok(PlaidLocalAccountInfo {
                    plaid_account_id: row.get(0)?,
                    name: row.get(1)?,
                    account_type: row.get(2)?,
                    mask: row.get(3)?,
                    local_account_id: row.get(4)?,
                    local_account_name: row.get(5)?,
                })
            })
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .filter_map(|r| r.ok())
            .collect();

        result.push(PlaidItemInfo {
            id,
            institution_name,
            status,
            last_synced_at,
            accounts,
        });
    }

    Ok(Json(PlaidItemsResponse { items: result }))
}

async fn plaid_link_page() -> Html<&'static str> {
    Html(include_str!("plaid_link.html"))
}

// Helper functions

struct PlaidProxyConfig {
    proxy_url: String,
    api_key: Option<String>,
}

fn get_plaid_config(
    _state: &SharedState,
) -> Result<PlaidProxyConfig, (StatusCode, Json<ErrorResponse>)> {
    // Re-read config from disk so changes made via the TUI config modal are picked up
    let config = AppConfig::load();
    let proxy_url = config.plaid.proxy_url.ok_or((
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            success: false,
            error: "Plaid proxy not configured. Set the proxy URL in the Plaid config (C key)."
                .to_string(),
        }),
    ))?;
    Ok(PlaidProxyConfig {
        proxy_url,
        api_key: config.plaid.api_key,
    })
}

fn proxy_error(msg: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse {
            success: false,
            error: msg,
        }),
    )
}

fn sha2_hash(input: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input);
    hasher.finalize().to_vec()
}

fn find_or_create_uncategorized_sync(
    conn: &rusqlite::Connection,
) -> Result<String, rusqlite::Error> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM accounts WHERE name = 'Uncategorized' AND is_active = 1",
            [],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        return Ok(id);
    }

    let max_number: Option<String> = conn
        .query_row(
            "SELECT MAX(account_number) FROM accounts WHERE account_number LIKE '9%'",
            [],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let next_number = match max_number {
        Some(n) => {
            let num: u32 = n.parse().unwrap_or(8999);
            format!("{}", num + 1)
        }
        None => "9000".to_string(),
    };

    let account_id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO accounts (id, account_type, account_number, name, is_active) VALUES (?1, 'expense', ?2, 'Uncategorized', 1)",
        rusqlite::params![account_id, next_number],
    )?;

    Ok(account_id)
}

// ---------------------------------------------------------------------------
// Standalone `serve` command (keeps the old interface)
// ---------------------------------------------------------------------------

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let (company_id, company_name) = {
        let store = state.store.lock().unwrap();
        let conn = store.connection();
        conn.query_row(
            "SELECT company_id, name FROM company WHERE id = 'default'",
            [],
            |row| Ok((row.get::<_, String>(0).ok(), row.get::<_, String>(1).ok())),
        )
        .unwrap_or((None, None))
    };

    Json(HealthResponse {
        status: "ok".to_string(),
        version: "0.1.0".to_string(),
        company_id,
        company_name,
    })
}

async fn account_banks(
    State(state): State<Arc<AppState>>,
) -> Result<Json<AccountBanksResponse>, StatusCode> {
    let store = state.store.lock().unwrap();
    let conn = store.connection();

    let mut stmt = conn
        .prepare(
            "SELECT a.id, a.name, a.account_type, a.account_number,
                    ba.bank_id, ba.bank_name
             FROM accounts a
             LEFT JOIN bank_accounts ba ON a.id = ba.account_id
             WHERE a.account_type IN ('asset', 'liability') AND a.is_active = 1
             ORDER BY a.account_type, a.account_number",
        )
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut accounts: Vec<AccountWithBanks> = Vec::new();
    let mut last_id: Option<String> = None;

    for row in rows {
        let (id, name, account_type, account_number, bank_id, bank_name) =
            row.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        if last_id.as_deref() != Some(&id) {
            accounts.push(AccountWithBanks {
                id: id.clone(),
                name,
                account_type,
                account_number,
                banks: Vec::new(),
            });
            last_id = Some(id);
        }

        if let (Some(bid), Some(bname)) = (bank_id, bank_name) {
            accounts.last_mut().unwrap().banks.push(AccountBankInfo {
                bank_id: bid,
                bank_name: bname,
            });
        }
    }

    Ok(Json(AccountBanksResponse { accounts }))
}

async fn link_bank(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LinkBankRequest>,
) -> Result<Json<LinkBankResponse>, (StatusCode, Json<ErrorResponse>)> {
    let store = state.store.lock().unwrap();
    let conn = store.connection();

    conn.execute(
        "INSERT OR REPLACE INTO bank_accounts (bank_id, bank_name, account_id) VALUES (?1, ?2, ?3)",
        rusqlite::params![req.bank_id, req.bank_name, req.account_id],
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Failed to link bank account: {}", e),
            }),
        )
    })?;

    Ok(Json(LinkBankResponse { success: true }))
}

async fn import_bank_csv(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportBankCsvRequest>,
) -> Result<Json<ImportBankCsvResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Validate company_id matches the open database
    {
        let store = state.store.lock().unwrap();
        let conn = store.connection();
        let db_company: Option<(String, String)> = conn
            .query_row(
                "SELECT company_id, name FROM company WHERE id = 'default'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        match db_company {
            Some((db_company_id, db_company_name)) => {
                if req.company_id != db_company_id {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(ErrorResponse {
                            success: false,
                            error: format!(
                                "CSV is for company '{}' but this server is serving '{}'",
                                req.company_id, db_company_name
                            ),
                        }),
                    ));
                }
            }
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        success: false,
                        error: "No company configured in this database".to_string(),
                    }),
                ));
            }
        }
    }

    // Count data rows (non-empty lines after the header)
    let lines: Vec<&str> = req.content.lines().collect();
    let transaction_count = if lines.len() > 1 {
        lines[1..].iter().filter(|l| !l.trim().is_empty()).count()
    } else {
        0
    };

    // Determine imports directory next to the database file
    let imports_dir = state
        .db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("imports");

    // Save CSV file
    let sanitized_bank = req
        .bank_name
        .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let csv_filename = format!("{}_{}.csv", sanitized_bank, timestamp);

    let csv_path = imports_dir.join(&csv_filename);

    // Write file using spawn_blocking since it's I/O
    let content = req.content.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        std::fs::create_dir_all(&imports_dir)
            .map_err(|e| format!("Failed to create imports directory: {}", e))?;
        std::fs::write(&csv_path, &content)
            .map_err(|e| format!("Failed to write CSV file: {}", e))?;
        Ok(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Task join error: {}", e),
            }),
        )
    })?;

    if let Err(msg) = result {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: msg,
            }),
        ));
    }

    Ok(Json(ImportBankCsvResponse {
        success: true,
        transaction_count,
    }))
}

async fn import_bank_file(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportBankFileRequest>,
) -> Result<Json<ImportBankCsvResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Validate company_id matches the open database
    {
        let store = state.store.lock().unwrap();
        let conn = store.connection();
        let db_company: Option<(String, String)> = conn
            .query_row(
                "SELECT company_id, name FROM company WHERE id = 'default'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        match db_company {
            Some((db_company_id, db_company_name)) => {
                if !req.company_id.is_empty() && req.company_id != db_company_id {
                    return Err((
                        StatusCode::FORBIDDEN,
                        Json(ErrorResponse {
                            success: false,
                            error: format!(
                                "File is for company '{}' but this server is serving '{}'",
                                req.company_id, db_company_name
                            ),
                        }),
                    ));
                }
            }
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        success: false,
                        error: "No company configured in this database".to_string(),
                    }),
                ));
            }
        }
    }

    // Read the file from disk
    let source_path = std::path::PathBuf::from(&req.file_path);
    let content = tokio::task::spawn_blocking({
        let path = source_path.clone();
        move || std::fs::read_to_string(&path)
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Task join error: {}", e),
            }),
        )
    })?
    .map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                success: false,
                error: format!("Failed to read file '{}': {}", req.file_path, e),
            }),
        )
    })?;

    // Count data rows (non-empty lines after the header)
    let lines: Vec<&str> = content.lines().collect();
    let transaction_count = if lines.len() > 1 {
        lines[1..].iter().filter(|l| !l.trim().is_empty()).count()
    } else {
        0
    };

    // Determine imports directory next to the database file
    let imports_dir = state
        .db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("imports");

    // Copy file to imports directory with standardized name
    let sanitized_bank = req
        .bank_name
        .replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let extension = source_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("csv");
    let dest_filename = format!("{}_{}.{}", sanitized_bank, timestamp, extension);
    let dest_path = imports_dir.join(&dest_filename);

    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        std::fs::create_dir_all(&imports_dir)
            .map_err(|e| format!("Failed to create imports directory: {}", e))?;
        std::fs::write(&dest_path, &content).map_err(|e| format!("Failed to write file: {}", e))?;
        Ok(())
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Task join error: {}", e),
            }),
        )
    })?;

    if let Err(msg) = result {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: msg,
            }),
        ));
    }

    Ok(Json(ImportBankCsvResponse {
        success: true,
        transaction_count,
    }))
}

/// Start the HTTP sync server on localhost:9876 (standalone mode).
pub async fn run_server(store: EventStore, db_path: PathBuf) -> anyhow::Result<()> {
    let state = Arc::new(AppState {
        store: std::sync::Mutex::new(store),
        db_path,
    });

    let cors = CorsLayer::very_permissive();

    let app = Router::new()
        .route("/health", get(health))
        .route("/accounts/banks", get(account_banks))
        .route("/accounts/link-bank", post(link_bank))
        .route("/import/bank-csv", post(import_bank_csv))
        .route("/import/bank-file", post(import_bank_file))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 9876));
    println!("Accountir sync server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
