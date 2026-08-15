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

/// A bank connection that has been established with the proxy but not yet
/// recorded in any ledger.
///
/// Exists for group-hosted books. The OAuth flow has to finish *somewhere* — the
/// browser posts back to this server — but the replica is deliberately not
/// attached to it (see [`attachable`]), so there is nothing here to append to.
/// The result is parked instead, and the desktop drains it and submits it to the
/// group server as a command under the member's own session.
///
/// `proxy_item_id` is in here and goes no further: the desktop needs it to mint a
/// grant at the proxy, and that is a conversation between the owner's machine and
/// their own proxy account. It is not part of what reaches the group.
#[derive(Debug, Clone)]
pub struct PendingLink {
    pub institution_name: String,
    pub proxy_item_id: String,
    pub accounts: Vec<crate::events::types::PlaidAccountInfo>,
    /// `"plaid"` when `accounts` is the Item's real list, `"link"` when the proxy
    /// could not reach Plaid and fell back to what the browser reported — which
    /// may be a subset. The desktop says so rather than presenting a possibly
    /// short list as complete.
    pub account_source: String,
}

/// The account list the proxy read back from Plaid, if it managed to.
///
/// `None` when the response carries no usable list at all — an older proxy, or a
/// shape we do not recognise. The caller falls back to the browser's list, which
/// is what this whole function exists to avoid depending on.
fn plaid_accounts_from_proxy(body: &serde_json::Value) -> Option<Vec<crate::events::types::PlaidAccountInfo>> {
    let accounts = body["accounts"].as_array()?;
    Some(
        accounts
            .iter()
            .filter_map(|a| {
                Some(crate::events::types::PlaidAccountInfo {
                    plaid_account_id: a["plaid_account_id"].as_str()?.to_string(),
                    name: a["name"].as_str().unwrap_or_default().to_string(),
                    official_name: a["official_name"].as_str().map(str::to_string),
                    account_type: a["account_type"]
                        .as_str()
                        .unwrap_or("depository")
                        .to_string(),
                    mask: a["mask"].as_str().map(str::to_string),
                })
            })
            .collect(),
    )
}

/// Shared server state — the database may or may not be open.
struct SharedState {
    db: std::sync::Mutex<Option<ActiveDb>>,
    http_client: reqwest::Client,
    /// One slot, not a queue: a person links one bank at a time, and a queue
    /// would let a forgotten result surface long after the flow it belonged to.
    pending_link: std::sync::Mutex<Option<PendingLink>>,
}

/// Handle passed to the TUI so it can set/clear the active database.
#[derive(Clone)]
pub struct ServerDb {
    inner: Arc<SharedState>,
}

/// Whether this local server may serve the given ledger at all.
///
/// The failure this prevents: every write handler on this server appends a
/// **locally authored** event through the ordinary path, and the server holds its
/// own `EventStore` on the file, so none of it passes through whatever gate the
/// caller uses to keep replicas read-only. On a group replica `events.id` *are*
/// the group server's sequence numbers, so one `POST /api/ingest/sale` or one
/// Plaid Link callback mints an id the group server is also about to mint, for a
/// different event. The two logs then silently stop being the same log, and the
/// first anyone notices is a trial balance nobody else can reproduce.
///
/// It is refused here rather than at each of the dozen write handlers, and rather
/// than only in the caller, because this is the single place the file becomes
/// reachable — and because this server is unauthenticated and CORS-permissive, so
/// "no caller would do that" is not a property anyone can enforce.
///
/// Erring toward refusal: a binding we cannot read is treated as present. A
/// wrongly refused local ledger costs an error message the user can act on; a
/// wrongly accepted replica costs the ledger.
fn attachable(store: &EventStore) -> Result<(), String> {
    let bound = crate::sync::binding::get_for(store).map_or(true, |b| b.is_some());
    if bound {
        return Err(
            "these books are a copy of a group server's — the group server keeps the \
             authoritative copy, so imports and Plaid have to go through it rather than \
             being written here"
                .to_string(),
        );
    }
    Ok(())
}

impl ServerDb {
    /// Take a completed bank link that had nowhere to be written.
    ///
    /// Draining rather than reading: the result is a one-shot, and leaving it in
    /// place would have the desktop record the same connection again on the next
    /// poll. `None` is the ordinary answer — the desktop asks repeatedly while a
    /// link is in flight.
    pub fn take_pending_link(&self) -> Option<PendingLink> {
        self.inner.pending_link.lock().ok()?.take()
    }

    /// Forget any parked link. Called when a link flow is abandoned, so a result
    /// the user walked away from cannot attach itself to a later attempt.
    pub fn clear_pending_link(&self) {
        if let Ok(mut slot) = self.inner.pending_link.lock() {
            *slot = None;
        }
    }

    /// Open a database and make it available to the sync server. Returns the
    /// canonical path on success. Errors are returned to the caller and also
    /// logged to stderr so they're visible in `tauri dev` output.
    ///
    /// Refuses a group replica outright — see [`attachable`].
    pub fn set(&self, path: &std::path::Path) -> Result<std::path::PathBuf, String> {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match EventStore::open(&canonical) {
            Ok(store) => {
                // Run migrations to ensure schema is up to date
                if let Err(e) = crate::store::migrations::run_migrations(store.connection()) {
                    let msg = format!(
                        "sync-server: migrations failed for {}: {}",
                        canonical.display(),
                        e
                    );
                    eprintln!("{}", msg);
                    return Err(msg);
                }
                if let Err(why) = attachable(&store) {
                    let msg = format!("sync-server: refusing {}: {}", canonical.display(), why);
                    eprintln!("{}", msg);
                    // Whatever was attached before is not this file, and leaving it
                    // in place would silently write somewhere else.
                    *self.inner.db.lock().unwrap() = None;
                    return Err(why);
                }
                let mut guard = self.inner.db.lock().unwrap();
                *guard = Some(ActiveDb {
                    store,
                    db_path: canonical.clone(),
                });
                eprintln!("sync-server: db set to {}", canonical.display());
                Ok(canonical)
            }
            Err(e) => {
                let msg = format!("sync-server: failed to open {}: {}", canonical.display(), e);
                eprintln!("{}", msg);
                Err(msg)
            }
        }
    }

    /// Close the server's database connection.
    pub fn clear(&self) {
        let mut guard = self.inner.db.lock().unwrap();
        *guard = None;
        eprintln!("sync-server: db cleared");
    }

    /// Path of the currently-open database, if any. Useful for diagnostics.
    pub fn current_path(&self) -> Option<std::path::PathBuf> {
        self.inner
            .db
            .lock()
            .unwrap()
            .as_ref()
            .map(|a| a.db_path.clone())
    }

    /// Whether the server currently has a database open.
    pub fn is_open(&self) -> bool {
        self.inner.db.lock().unwrap().is_some()
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
        pending_link: std::sync::Mutex::new(None),
    });

    let cors = CorsLayer::very_permissive();

    let app = Router::new()
        .route("/health", get(bg_health))
        .route("/accounts/banks", get(bg_account_banks))
        .route("/accounts/link-bank", post(bg_link_bank))
        .route("/import/bank-csv", post(bg_import_bank_csv))
        .route("/import/bank-file", post(bg_import_bank_file))
        .route("/import/square-sales-file", post(bg_import_square_sales))
        .route(
            "/import/square-payroll-file",
            post(bg_import_square_payroll),
        )
        // Plaid integration routes
        .route("/plaid/config", get(plaid_config))
        .route("/plaid/link-token", post(plaid_link_token))
        .route("/plaid/exchange-token", post(plaid_exchange_token))
        .route("/plaid/refresh-accounts", post(plaid_refresh_accounts))
        .route("/plaid/sync", post(plaid_sync))
        .route("/plaid/balances", post(plaid_balances))
        .route("/plaid/staged", get(plaid_staged_list))
        .route("/plaid/staged/import-transfer", post(plaid_import_transfer))
        .route("/plaid/staged/reject-transfer", post(plaid_reject_transfer))
        .route("/plaid/staged/import-all", post(plaid_import_all))
        .route("/plaid/items", get(plaid_items))
        .route("/plaid/link", get(plaid_link_page))
        // Ingest API routes
        .route(
            "/api/ingest/mappings",
            get(bg_ingest_get_mappings).put(bg_ingest_set_mappings),
        )
        .route("/api/ingest/sale", post(bg_ingest_sale))
        .route("/api/ingest/purchase-order", post(bg_ingest_purchase_order))
        .route(
            "/api/ingest/inventory-adjustment",
            post(bg_ingest_inventory_adjustment),
        )
        .layer(cors)
        .with_state(shared.clone());

    let addr = SocketAddr::from(([127, 0, 0, 1], 9876));

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "sync-server: failed to bind to {}: {} — another accountir \
                 process is probably already running. Plaid sync from this \
                 process will not work.",
                addr, e
            );
            return None;
        }
    };

    eprintln!("sync-server: listening on http://{}", addr);

    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("sync-server: axum serve loop exited: {}", e);
        }
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

    // The proxy's list, not the browser's.
    //
    // `req.accounts` is Plaid Link's `onSuccess` metadata: what the *browser* was
    // handed at the end of the flow. For an OAuth institution the account choice
    // happens at the bank, and that metadata has been seen to carry a subset — a
    // Chase login with a checking account and three employee cards arrived with
    // two of the four, and the two it omitted then existed nowhere. The proxy now
    // reads the Item's real account list from Plaid and returns it, so that is
    // what goes into the ledger.
    //
    // Falls back to the browser's list only when the proxy could not verify one
    // either (`account_source == "link"`), which it reports rather than hides.
    let discovered = plaid_accounts_from_proxy(&body);
    let account_source = body["account_source"].as_str().unwrap_or("link").to_string();
    let plaid_accounts = match discovered {
        Some(accounts) if !accounts.is_empty() => accounts,
        _ => req
            .accounts
            .iter()
            .map(|a| crate::events::types::PlaidAccountInfo {
                plaid_account_id: a.id.clone(),
                name: a.name.clone(),
                official_name: a.official_name.clone(),
                account_type: a.account_type.clone(),
                mask: a.mask.clone(),
            })
            .collect(),
    };

    // No ledger attached means these are group-hosted books: the replica is
    // deliberately kept off this server (see `attachable`), so there is nothing
    // here to append to. Park the result for the desktop, which submits it to the
    // group server under the member's own session.
    //
    // Checked before taking the write path rather than after failing it, so the
    // browser gets a clean success instead of "No database open" at the end of an
    // OAuth flow the user cannot repeat without re-authenticating at their bank.
    if state.db.lock().unwrap().is_none() {
        *state.pending_link.lock().unwrap() = Some(PendingLink {
            institution_name: req.institution.name.clone(),
            proxy_item_id: proxy_item_id.clone(),
            accounts: plaid_accounts,
            account_source: account_source.clone(),
        });
        // The id the ledger will use is minted by the group server when the
        // desktop submits, so there is none to report yet. The browser only needs
        // to know the flow succeeded.
        return Ok(Json(PlaidExchangeTokenResponse {
            success: true,
            item_id: String::new(),
        }));
    }

    // Record the connection through the ledger's own append path.
    //
    // This block used to hand-roll the insert — raw SQL, its own hash formula
    // (`event_type + payload + timestamp`, with no separators and no `user_id`),
    // hand-written projection statements, and no validation. The comments said it
    // was a workaround for needing `&mut EventStore` behind a `Mutex` guard, which
    // `as_mut()` gives directly.
    //
    // What it actually cost: every bank connection ever made through Plaid Link
    // wrote an event whose hash cannot be re-derived, so it fails chain
    // verification forever. On a ledger whose tamper-evidence IS the hash chain,
    // an event that cannot be re-derived is indistinguishable from one that was
    // altered. Four ledgers were checked and each had exactly one bad event: this
    // one. See `examples/verify_real_ledger.rs`.
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;

    let item_id = uuid::Uuid::new_v4().to_string();
    let event = crate::events::types::Event::PlaidItemConnected {
        item_id: item_id.clone(),
        proxy_item_id: Some(proxy_item_id.clone()),
        institution_name: req.institution.name.clone(),
        plaid_accounts,
    };

    // `append` hashes with `compute_event_hash`, validates, and folds the event
    // into projections through `Projector` — so `plaid_items` and
    // `plaid_local_accounts` are written by the same code that would rebuild them
    // from the log, rather than by a second copy that can drift from it.
    active
        .store
        .append(crate::events::types::EventEnvelope::new(
            event,
            "plaid-link".to_string(),
        ))
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    success: false,
                    error: format!("recording the connection: {}", e),
                }),
            )
        })?;

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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    balance_discrepancies: Vec<BalanceDiscrepancy>,
}

#[derive(Serialize)]
struct BalanceDiscrepancy {
    account_name: String,
    plaid_balance_cents: i64,
    ledger_balance_cents: i64,
    difference_cents: i64,
}

#[derive(Serialize)]
struct PlaidRefreshAccountsResponse {
    /// Every account the bank reports behind this connection.
    accounts: Vec<crate::events::types::PlaidAccountInfo>,
    /// How many of them the books had never seen.
    added: usize,
    /// Whether anything was written. `false` means the books already agreed.
    recorded: bool,
}

/// Re-read a connection's accounts from the bank and record what is new.
///
/// The reason this exists: an account list captured when the connection was made
/// goes stale in two ways. A connection made before discovery was authoritative
/// is short of whatever Plaid Link's metadata omitted — which for an OAuth bank
/// can be most of them — and an account opened at the bank afterwards never
/// appears at all, because nothing was asking.
///
/// Re-linking is not the answer to either: it means re-authenticating at the bank
/// and it resets the transaction cursor.
///
/// Books held by a group are refused here, as linking is, and for the same
/// mechanical reason: this appends, and on a replica the event ids belong to the
/// group's server. The desktop submits those through
/// `/sync/commands/refresh-plaid-accounts` instead, having asked the proxy itself.
async fn plaid_refresh_accounts(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<PlaidSyncRequest>,
) -> Result<Json<PlaidRefreshAccountsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let plaid_cfg = get_plaid_config(&state)?;

    let proxy_item_id = {
        let guard = state.db.lock().unwrap();
        let active = guard.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                success: false,
                error: "No database open".to_string(),
            }),
        ))?;
        active
            .store
            .connection()
            .query_row(
                "SELECT proxy_item_id FROM plaid_items WHERE id = ?1 AND status = 'active'",
                [&req.item_id],
                |row| row.get::<_, Option<String>>(0),
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
            .ok_or((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    success: false,
                    error: "This connection has no proxy handle, so these books \
                            cannot refresh it themselves."
                        .to_string(),
                }),
            ))?
    };

    let mut req_builder = state.http_client.post(format!(
        "{}/plaid/items/{}/accounts/refresh",
        plaid_cfg.proxy_url, proxy_item_id
    ));
    if let Some(ref key) = plaid_cfg.api_key {
        req_builder = req_builder.bearer_auth(key);
    }
    let resp = req_builder
        .send()
        .await
        .map_err(|e| proxy_error(format!("Failed to contact proxy: {}", e)))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(proxy_error(
            "The bank-sync proxy does not support refreshing a connection's \
             accounts yet. Nothing was changed."
                .to_string(),
        ));
    }
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(proxy_error(format!("Proxy error: {}", text)));
    }
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| proxy_error(format!("Parse error: {}", e)))?;

    let accounts = plaid_accounts_from_proxy(&body).unwrap_or_default();
    if accounts.is_empty() {
        return Err(proxy_error(
            "The bank reported no accounts behind this connection. Nothing was \
             changed — an empty answer is far more likely to be a fault than a \
             bank with nothing in it."
                .to_string(),
        ));
    }
    let added = body["added"].as_u64().unwrap_or(0) as usize;

    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;
    let recorded = crate::commands::plaid_commands::PlaidCommands::new(
        &mut active.store,
        "plaid-refresh".to_string(),
    )
    .refresh_accounts(&req.item_id, accounts.clone())
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("recording the refresh: {}", e),
            }),
        )
    })?
    .is_some();

    Ok(Json(PlaidRefreshAccountsResponse {
        accounts,
        added,
        recorded,
    }))
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
    let (staged, skipped, transfer_candidates) = {
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
        let transfer_candidates =
            crate::commands::plaid_commands::detect_transfers(conn).unwrap_or(0);

        (staged, skipped, transfer_candidates)
    }; // guard dropped here

    // Fetch and store current Plaid balances, then compute discrepancies
    let mut balance_discrepancies = Vec::new();

    let mut balance_req = state
        .http_client
        .post(format!("{}/plaid/balances", plaid_cfg.proxy_url));
    if let Some(ref key) = plaid_cfg.api_key {
        balance_req = balance_req.bearer_auth(key);
    }
    let balance_result = balance_req
        .json(&serde_json::json!({ "item_id": proxy_item_id }))
        .send()
        .await;

    if let Err(ref e) = balance_result {
        eprintln!("Balance fetch failed: {}", e);
    }

    if let Ok(balance_resp) = balance_result {
        if let Ok(body) = balance_resp.json::<serde_json::Value>().await {
            if let Some(accounts) = body["accounts"].as_array() {
                let guard = state.db.lock().unwrap();
                if let Some(ref active) = *guard {
                    let conn = active.store.connection();
                    let now = chrono::Utc::now().to_rfc3339();
                    for acct in accounts {
                        if let (Some(plaid_id), Some(current)) =
                            (acct["account_id"].as_str(), acct["current"].as_f64())
                        {
                            let balance_cents = (current * 100.0).round() as i64;
                            let _ = conn.execute(
                                "UPDATE plaid_local_accounts SET plaid_balance_cents = ?1, balance_updated_at = ?2
                                 WHERE item_id = ?3 AND plaid_account_id = ?4",
                                rusqlite::params![balance_cents, &now, &req.item_id, plaid_id],
                            );

                            // Check for discrepancy with ledger balance
                            let mapping: Option<(String, String, String)> = conn
                                .query_row(
                                    "SELECT pla.local_account_id, a.name, pla.account_type
                                     FROM plaid_local_accounts pla
                                     JOIN accounts a ON pla.local_account_id = a.id
                                     WHERE pla.item_id = ?1 AND pla.plaid_account_id = ?2
                                       AND pla.local_account_id IS NOT NULL",
                                    rusqlite::params![&req.item_id, plaid_id],
                                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                                )
                                .ok();

                            if let Some((local_id, account_name, account_type)) = mapping {
                                // Convert Plaid balance to our sign convention
                                let plaid_in_ours = if account_type == "credit" {
                                    -balance_cents
                                } else {
                                    balance_cents
                                };

                                let ledger_balance: i64 = conn
                                    .query_row(
                                        "SELECT COALESCE(SUM(jl.amount), 0)
                                         FROM journal_lines jl
                                         JOIN journal_entries je ON jl.entry_id = je.id
                                         WHERE jl.account_id = ?1 AND je.is_void = 0",
                                        [&local_id],
                                        |row| row.get(0),
                                    )
                                    .unwrap_or(0);

                                let diff = plaid_in_ours - ledger_balance;
                                if diff != 0 {
                                    balance_discrepancies.push(BalanceDiscrepancy {
                                        account_name,
                                        plaid_balance_cents: plaid_in_ours,
                                        ledger_balance_cents: ledger_balance,
                                        difference_cents: diff,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(Json(PlaidSyncResponse {
        staged,
        skipped,
        transfer_candidates,
        balance_discrepancies,
    }))
}

// ---------------------------------------------------------------------------
// Plaid balances endpoint
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PlaidBalanceEntry {
    plaid_account_id: String,
    name: String,
    current: Option<f64>,
    available: Option<f64>,
    iso_currency_code: Option<String>,
}

#[derive(Serialize)]
struct PlaidBalancesResponse {
    accounts: Vec<PlaidBalanceEntry>,
}

async fn plaid_balances(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<PlaidSyncRequest>,
) -> Result<Json<PlaidBalancesResponse>, (StatusCode, Json<ErrorResponse>)> {
    let plaid_cfg = get_plaid_config(&state)?;

    let proxy_item_id: String = {
        let guard = state.db.lock().unwrap();
        let active = guard.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                success: false,
                error: "No database open".to_string(),
            }),
        ))?;
        active
            .store
            .connection()
            .query_row(
                "SELECT proxy_item_id FROM plaid_items WHERE id = ?1",
                [&req.item_id],
                |row| row.get(0),
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

    let mut req_builder = state
        .http_client
        .post(format!("{}/plaid/balances", plaid_cfg.proxy_url));
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
        return Err(proxy_error(format!("Proxy balance error: {}", text)));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| proxy_error(format!("Parse error: {}", e)))?;

    let accounts: Vec<PlaidBalanceEntry> = body["accounts"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|a| PlaidBalanceEntry {
            plaid_account_id: a["account_id"].as_str().unwrap_or("").to_string(),
            name: a["name"].as_str().unwrap_or("").to_string(),
            current: a["current"].as_f64(),
            available: a["available"].as_f64(),
            iso_currency_code: a["iso_currency_code"].as_str().map(String::from),
        })
        .collect();

    Ok(Json(PlaidBalancesResponse { accounts }))
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
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;

    let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
        &mut active.store,
        "plaid-sync".to_string(),
    );

    match commands.import_transfer(&req.candidate_id) {
        Ok(stored) => {
            let entry_id =
                if let crate::events::types::Event::JournalEntryPosted { entry_id, .. } =
                    &stored.event
                {
                    entry_id.clone()
                } else {
                    String::new()
                };
            Ok(Json(
                serde_json::json!({ "success": true, "entry_id": entry_id }),
            ))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Import failed: {}", e),
            }),
        )),
    }
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
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            success: false,
            error: "No database open".to_string(),
        }),
    ))?;

    let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
        &mut active.store,
        "plaid-sync".to_string(),
    );

    match commands.import_all_staged() {
        Ok((transfers_imported, unmatched_imported)) => Ok(Json(serde_json::json!({
            "success": true,
            "transfers_imported": transfers_imported,
            "unmatched_imported": unmatched_imported
        }))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                success: false,
                error: format!("Import failed: {}", e),
            }),
        )),
    }
}

/// Load a staged transaction as a tuple for server-side processing.
/// Returns (id, item_id, plaid_txn_id, local_account_id, amount_cents, date, name, merchant_name, currency)

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
    #[serde(skip_serializing_if = "Option::is_none")]
    plaid_balance_cents: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ledger_balance_cents: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    balance_updated_at: Option<String>,
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
                "SELECT pa.plaid_account_id, pa.name, pa.account_type, pa.mask,
                        pa.local_account_id, a.name, pa.plaid_balance_cents, pa.balance_updated_at
                 FROM plaid_local_accounts pa
                 LEFT JOIN accounts a ON pa.local_account_id = a.id
                 WHERE pa.item_id = ?1",
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        let accounts: Vec<PlaidLocalAccountInfo> = acct_stmt
            .query_map([&id], |row| {
                let local_account_id: Option<String> = row.get(4)?;
                let account_type: String = row.get(2)?;
                let plaid_balance_raw: Option<i64> = row.get(6)?;

                // Convert Plaid balance to our sign convention
                let plaid_balance_cents =
                    plaid_balance_raw.map(|pb| if account_type == "credit" { -pb } else { pb });

                let ledger_balance_cents = local_account_id.as_ref().and_then(|aid| {
                    conn.query_row(
                        "SELECT COALESCE(SUM(jl.amount), 0)
                         FROM journal_lines jl
                         JOIN journal_entries je ON jl.entry_id = je.id
                         WHERE jl.account_id = ?1 AND je.is_void = 0",
                        [aid],
                        |r| r.get::<_, i64>(0),
                    )
                    .ok()
                });

                Ok(PlaidLocalAccountInfo {
                    plaid_account_id: row.get(0)?,
                    name: row.get(1)?,
                    account_type,
                    mask: row.get(3)?,
                    local_account_id,
                    local_account_name: row.get(5)?,
                    plaid_balance_cents,
                    ledger_balance_cents,
                    balance_updated_at: row.get(7)?,
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

// ---------------------------------------------------------------------------
// Ingest API (POS, inventory, purchase orders)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct IngestMappingEntry {
    key: String,
    account_id: String,
    account_name: Option<String>,
}

#[derive(Serialize)]
struct IngestMappingsResponse {
    mappings: Vec<IngestMappingEntry>,
}

#[derive(Deserialize)]
struct SetIngestMappingsRequest {
    mappings: Vec<SetMappingEntry>,
}

#[derive(Deserialize)]
struct SetMappingEntry {
    key: String,
    account_id: String,
}

#[derive(Serialize)]
struct SetIngestMappingsResponse {
    success: bool,
    updated: usize,
}

#[derive(Deserialize)]
struct IngestSaleRequest {
    date: String,
    reference: Option<String>,
    memo: Option<String>,
    items: Vec<SaleItem>,
    payment_method: PaymentMethod,
    tax_collected_cents: Option<i64>,
}

#[derive(Deserialize)]
struct SaleItem {
    name: String,
    qty: u32,
    unit_price_cents: i64,
    unit_cost_cents: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PaymentMethod {
    Cash,
    Square,
}

#[derive(Serialize)]
struct IngestSaleResponse {
    success: bool,
    entry_id: String,
    total_revenue_cents: i64,
    total_cogs_cents: i64,
}

#[derive(Deserialize)]
struct IngestPurchaseOrderRequest {
    date: String,
    reference: Option<String>,
    memo: Option<String>,
    supplier: Option<String>,
    items: Vec<PurchaseItem>,
    payment: PurchasePayment,
}

#[derive(Deserialize)]
struct PurchaseItem {
    name: String,
    qty: u32,
    unit_cost_cents: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum PurchasePayment {
    Cash,
    OnCredit,
}

#[derive(Serialize)]
struct IngestPurchaseOrderResponse {
    success: bool,
    entry_id: String,
    total_cost_cents: i64,
}

#[derive(Deserialize)]
struct IngestInventoryAdjustmentRequest {
    date: String,
    reference: Option<String>,
    memo: Option<String>,
    items: Vec<AdjustmentItem>,
}

#[derive(Deserialize)]
struct AdjustmentItem {
    name: String,
    qty_delta: i32,
    unit_cost_cents: i64,
    reason: Option<String>,
}

#[derive(Serialize)]
struct IngestInventoryAdjustmentResponse {
    success: bool,
    entry_id: String,
    net_adjustment_cents: i64,
}

fn ingest_err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            success: false,
            error: msg.into(),
        }),
    )
}

fn ingest_no_db() -> (StatusCode, Json<ErrorResponse>) {
    ingest_err(StatusCode::SERVICE_UNAVAILABLE, "No database open")
}

async fn bg_ingest_get_mappings(
    State(state): State<Arc<SharedState>>,
) -> Result<Json<IngestMappingsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or_else(ingest_no_db)?;
    let conn = active.store.connection();

    let mut stmt = conn
        .prepare(
            "SELECT m.key, m.account_id, a.name
             FROM ingest_account_mappings m
             LEFT JOIN accounts a ON m.account_id = a.id
             ORDER BY m.key",
        )
        .map_err(|e| ingest_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let mappings: Vec<IngestMappingEntry> = stmt
        .query_map([], |row| {
            Ok(IngestMappingEntry {
                key: row.get(0)?,
                account_id: row.get(1)?,
                account_name: row.get(2)?,
            })
        })
        .map_err(|e| ingest_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(IngestMappingsResponse { mappings }))
}

async fn bg_ingest_set_mappings(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<SetIngestMappingsRequest>,
) -> Result<Json<SetIngestMappingsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let guard = state.db.lock().unwrap();
    let active = guard.as_ref().ok_or_else(ingest_no_db)?;
    let conn = active.store.connection();

    let valid_keys = crate::commands::ingest_commands::mapping_keys();
    for entry in &req.mappings {
        if !valid_keys.contains(&entry.key.as_str()) {
            return Err(ingest_err(
                StatusCode::BAD_REQUEST,
                format!(
                    "Unknown mapping key '{}'. Valid keys: {}",
                    entry.key,
                    valid_keys.join(", ")
                ),
            ));
        }

        let active_account: bool = conn
            .query_row(
                "SELECT is_active = 1 FROM accounts WHERE id = ?1",
                [&entry.account_id],
                |row| row.get(0),
            )
            .map_err(|_| {
                ingest_err(
                    StatusCode::BAD_REQUEST,
                    format!("Account not found: {}", entry.account_id),
                )
            })?;

        if !active_account {
            return Err(ingest_err(
                StatusCode::BAD_REQUEST,
                format!("Account is inactive: {}", entry.account_id),
            ));
        }
    }

    let mut updated = 0;
    for entry in &req.mappings {
        conn.execute(
            "INSERT INTO ingest_account_mappings (key, account_id, updated_at)
             VALUES (?1, ?2, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET account_id = ?2, updated_at = datetime('now')",
            rusqlite::params![entry.key, entry.account_id],
        )
        .map_err(|e| ingest_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        updated += 1;
    }

    Ok(Json(SetIngestMappingsResponse {
        success: true,
        updated,
    }))
}

async fn bg_ingest_sale(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<IngestSaleRequest>,
) -> Result<Json<IngestSaleResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or_else(ingest_no_db)?;

    let total_revenue: i64 = req
        .items
        .iter()
        .map(|i| i.qty as i64 * i.unit_price_cents)
        .sum();
    let total_cogs: i64 = req
        .items
        .iter()
        .map(|i| i.qty as i64 * i.unit_cost_cents)
        .sum();

    let data = crate::commands::ingest_commands::IngestSaleData {
        date: req.date,
        reference: req.reference,
        memo: req.memo,
        items: req
            .items
            .into_iter()
            .map(|i| crate::commands::ingest_commands::IngestSaleItem {
                name: i.name,
                qty: i.qty,
                unit_price_cents: i.unit_price_cents,
                unit_cost_cents: i.unit_cost_cents,
            })
            .collect(),
        payments: Vec::new(),
        payment_method: Some(match req.payment_method {
            PaymentMethod::Cash => crate::commands::ingest_commands::IngestPaymentMethod::Cash,
            PaymentMethod::Square => crate::commands::ingest_commands::IngestPaymentMethod::Square,
        }),
        tax_collected_cents: req.tax_collected_cents,
    };

    let result = crate::commands::ingest_commands::ingest_sale(
        &mut active.store,
        "ingest-api",
        data,
        crate::events::types::JournalEntrySource::Pos,
    )
    .map_err(|e| ingest_err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;

    Ok(Json(IngestSaleResponse {
        success: true,
        entry_id: result.entry_id,
        total_revenue_cents: total_revenue,
        total_cogs_cents: total_cogs,
    }))
}

async fn bg_ingest_purchase_order(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<IngestPurchaseOrderRequest>,
) -> Result<Json<IngestPurchaseOrderResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or_else(ingest_no_db)?;

    let total_cost: i64 = req
        .items
        .iter()
        .map(|i| i.qty as i64 * i.unit_cost_cents)
        .sum();

    let data = crate::commands::ingest_commands::IngestPurchaseOrderData {
        date: req.date,
        reference: req.reference,
        memo: req.memo,
        supplier: req.supplier,
        items: req
            .items
            .into_iter()
            .map(|i| crate::commands::ingest_commands::IngestPurchaseItem {
                name: i.name,
                qty: i.qty,
                unit_cost_cents: i.unit_cost_cents,
            })
            .collect(),
        payment: Some(match req.payment {
            PurchasePayment::Cash => crate::commands::ingest_commands::IngestPurchasePayment::Cash,
            PurchasePayment::OnCredit => {
                crate::commands::ingest_commands::IngestPurchasePayment::OnCredit
            }
        }),
    };

    let result = crate::commands::ingest_commands::ingest_purchase_order(
        &mut active.store,
        "ingest-api",
        data,
        crate::events::types::JournalEntrySource::PurchaseOrder,
    )
    .map_err(|e| ingest_err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;

    Ok(Json(IngestPurchaseOrderResponse {
        success: true,
        entry_id: result.entry_id,
        total_cost_cents: total_cost,
    }))
}

async fn bg_ingest_inventory_adjustment(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<IngestInventoryAdjustmentRequest>,
) -> Result<Json<IngestInventoryAdjustmentResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or_else(ingest_no_db)?;

    let net: i64 = req
        .items
        .iter()
        .map(|i| i.qty_delta as i64 * i.unit_cost_cents)
        .sum();

    let data = crate::commands::ingest_commands::IngestInventoryAdjustmentData {
        date: req.date,
        reference: req.reference,
        memo: req.memo,
        items: req
            .items
            .into_iter()
            .map(|i| crate::commands::ingest_commands::IngestAdjustmentItem {
                name: i.name,
                qty_delta: i.qty_delta,
                unit_cost_cents: i.unit_cost_cents,
                reason: i.reason,
            })
            .collect(),
    };

    let result = crate::commands::ingest_commands::ingest_inventory_adjustment(
        &mut active.store,
        "ingest-api",
        data,
        crate::events::types::JournalEntrySource::InventoryAdjustment,
    )
    .map_err(|e| ingest_err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;

    Ok(Json(IngestInventoryAdjustmentResponse {
        success: true,
        entry_id: result.entry_id,
        net_adjustment_cents: net,
    }))
}

// ---------------------------------------------------------------------------
// Square CSV import (sales activity + pay-period payroll)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImportSquareFileRequest {
    #[serde(default)]
    company_id: String,
    #[serde(default)]
    source_name: Option<String>,
    file_path: String,
    #[serde(default)]
    #[allow(dead_code)]
    downloaded_at: Option<String>,
}

#[derive(Serialize)]
struct ImportSquareResponse {
    success: bool,
    entries_posted: usize,
    skipped_duplicates: usize,
    rows_parsed: usize,
}

/// Read a file the extension already downloaded to disk.
async fn read_import_file(file_path: &str) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let path = std::path::PathBuf::from(file_path);
    let file_path = file_path.to_string();
    tokio::task::spawn_blocking(move || std::fs::read_to_string(&path))
        .await
        .map_err(|e| {
            ingest_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Task join error: {}", e),
            )
        })?
        .map_err(|e| {
            ingest_err(
                StatusCode::BAD_REQUEST,
                format!("Failed to read file '{}': {}", file_path, e),
            )
        })
}

/// The name we hand to the Square sales parser to recover the export period.
/// Square names the download `sales-summary-<start>-<end>.csv`, so the basename
/// carries the dates; we append the source name as a fallback hint.
fn import_file_name(file_path: &str, source_name: Option<&str>) -> String {
    let base = std::path::Path::new(file_path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(file_path);
    match source_name {
        Some(name) => format!("{} {}", base, name),
        None => base.to_string(),
    }
}

/// Guard that the posted file belongs to the company this server is serving.
fn validate_company(
    conn: &rusqlite::Connection,
    company_id: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let db_company: Option<(String, String)> = conn
        .query_row(
            "SELECT company_id, name FROM company WHERE id = 'default'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    match db_company {
        Some((db_id, db_name)) => {
            if !company_id.is_empty() && company_id != db_id {
                return Err(ingest_err(
                    StatusCode::FORBIDDEN,
                    format!(
                        "File is for company '{}' but this server is serving '{}'",
                        company_id, db_name
                    ),
                ));
            }
            Ok(())
        }
        None => Err(ingest_err(
            StatusCode::BAD_REQUEST,
            "No company configured in this database".to_string(),
        )),
    }
}

async fn bg_import_square_sales(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<ImportSquareFileRequest>,
) -> Result<Json<ImportSquareResponse>, (StatusCode, Json<ErrorResponse>)> {
    let content = read_import_file(&req.file_path).await?;
    let file_name = import_file_name(&req.file_path, req.source_name.as_deref());
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or_else(ingest_no_db)?;
    validate_company(active.store.connection(), &req.company_id)?;

    let summary = crate::commands::square_commands::ingest_square_sales(
        &mut active.store,
        "square-sync",
        &content,
        &file_name,
    )
    .map_err(|e| ingest_err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;

    Ok(Json(ImportSquareResponse {
        success: true,
        entries_posted: summary.entries_posted,
        skipped_duplicates: summary.skipped_duplicates,
        rows_parsed: summary.rows_parsed,
    }))
}

async fn bg_import_square_payroll(
    State(state): State<Arc<SharedState>>,
    Json(req): Json<ImportSquareFileRequest>,
) -> Result<Json<ImportSquareResponse>, (StatusCode, Json<ErrorResponse>)> {
    // The payroll "Company Totals" report is a binary .xlsx; the parser opens
    // the file directly from disk rather than reading it as text.
    let mut guard = state.db.lock().unwrap();
    let active = guard.as_mut().ok_or_else(ingest_no_db)?;
    validate_company(active.store.connection(), &req.company_id)?;

    let summary = crate::commands::square_commands::ingest_square_payroll(
        &mut active.store,
        "square-sync",
        &req.file_path,
    )
    .map_err(|e| ingest_err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;

    Ok(Json(ImportSquareResponse {
        success: true,
        entries_posted: summary.entries_posted,
        skipped_duplicates: summary.skipped_duplicates,
        rows_parsed: summary.rows_parsed,
    }))
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

async fn import_square_sales(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportSquareFileRequest>,
) -> Result<Json<ImportSquareResponse>, (StatusCode, Json<ErrorResponse>)> {
    let content = read_import_file(&req.file_path).await?;
    let file_name = import_file_name(&req.file_path, req.source_name.as_deref());
    let mut store = state.store.lock().unwrap();
    validate_company(store.connection(), &req.company_id)?;
    let summary = crate::commands::square_commands::ingest_square_sales(
        &mut store,
        "square-sync",
        &content,
        &file_name,
    )
    .map_err(|e| ingest_err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    Ok(Json(ImportSquareResponse {
        success: true,
        entries_posted: summary.entries_posted,
        skipped_duplicates: summary.skipped_duplicates,
        rows_parsed: summary.rows_parsed,
    }))
}

async fn import_square_payroll(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ImportSquareFileRequest>,
) -> Result<Json<ImportSquareResponse>, (StatusCode, Json<ErrorResponse>)> {
    let mut store = state.store.lock().unwrap();
    validate_company(store.connection(), &req.company_id)?;
    let summary = crate::commands::square_commands::ingest_square_payroll(
        &mut store,
        "square-sync",
        &req.file_path,
    )
    .map_err(|e| ingest_err(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    Ok(Json(ImportSquareResponse {
        success: true,
        entries_posted: summary.entries_posted,
        skipped_duplicates: summary.skipped_duplicates,
        rows_parsed: summary.rows_parsed,
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
        .route("/import/square-sales-file", post(import_square_sales))
        .route("/import/square-payroll-file", post(import_square_payroll))
        .layer(cors)
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 9876));
    println!("Accountir sync server listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations::SchemaStore;

    fn ledger() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        s.init_schema().unwrap();
        s.run_migrations().unwrap();
        s
    }

    /// The regression this guards: a group replica being served by this local,
    /// unauthenticated, CORS-permissive HTTP server. Its write handlers append
    /// locally authored events on its own connection to the file, so nothing the
    /// caller does to keep a replica read-only can see them — and an event
    /// authored at a seq the group server is also about to use forks the log
    /// silently and permanently.
    #[test]
    fn a_group_replica_is_never_served_by_the_local_write_server() {
        let local = ledger();
        assert!(
            attachable(&local).is_ok(),
            "a plain local ledger is what this server exists for"
        );

        let replica = ledger();
        crate::sync::binding::bind(
            replica.connection(),
            "acme",
            "https://acme.app.accountir.com",
            "https://app.accountir.com",
        )
        .unwrap();
        let err = attachable(&replica).unwrap_err();
        assert!(
            err.contains("group server"),
            "the refusal has to say where the writes belong instead: {err}"
        );
    }
}
#[cfg(test)]
mod pending_link_tests {
    use super::*;

    /// The parked result is a one-shot.
    ///
    /// The failure this prevents: leaving it in place means the desktop, which
    /// polls while a link is in flight, records the same bank connection over and
    /// over — one new `PlaidItemConnected` per poll, in the group's shared log,
    /// with nothing to say they are the same bank.
    #[test]
    fn draining_a_pending_link_yields_it_exactly_once() {
        let sdb = ServerDb {
            inner: Arc::new(SharedState {
                db: std::sync::Mutex::new(None),
                http_client: reqwest::Client::new(),
                pending_link: std::sync::Mutex::new(Some(PendingLink {
                    institution_name: "Chase".into(),
                    proxy_item_id: "p-1".into(),
                    accounts: vec![],
                    account_source: "plaid".into(),
                })),
            }),
        };

        let first = sdb.take_pending_link().expect("the parked link");
        assert_eq!(first.institution_name, "Chase");
        assert!(
            sdb.take_pending_link().is_none(),
            "a second drain returned the link again — the desktop polls, so this \
             would record the same connection once per poll"
        );
    }

    /// An abandoned flow must not attach itself to a later one. Someone who
    /// starts a link, changes their mind, and links a different bank an hour
    /// later should not get the first bank recorded.
    #[test]
    fn an_abandoned_link_can_be_discarded() {
        let sdb = ServerDb {
            inner: Arc::new(SharedState {
                db: std::sync::Mutex::new(None),
                http_client: reqwest::Client::new(),
                pending_link: std::sync::Mutex::new(Some(PendingLink {
                    institution_name: "Abandoned".into(),
                    proxy_item_id: "p-x".into(),
                    accounts: vec![],
                    account_source: "plaid".into(),
                })),
            }),
        };
        sdb.clear_pending_link();
        assert!(sdb.take_pending_link().is_none());
    }
}
