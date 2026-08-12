use crate::commands::bill_commands::ReceiveBillCommand;
use crate::commands::entry_commands::PostEntryCommand;
use crate::commands::ingest_commands::{
    self as ingest_commands, IngestError, IngestGoodsReceivedData,
    IngestInventoryAdjustmentData, IngestPurchaseOrderData, IngestRefundData, IngestSaleData,
};
use crate::events::types::{Event, EventEnvelope, JournalEntrySource, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::{ProjectionStore, Projector};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EventServiceError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Event store error: {0}")]
    StoreError(String),
    #[error("Service not found: {0}")]
    NotFound(String),
    #[error("A service is already registered for URL: {0}")]
    AlreadyExists(String),
    #[error("HTTP error: {0}")]
    HttpError(String),
    #[error("Parse error: {0}")]
    ParseError(String),
}

// --- Data types for remote API responses ---

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteEventsResponse {
    pub events: Vec<RemoteEvent>,
    pub cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub data: serde_json::Value,
    pub timestamp: String,
}

pub struct SyncResult {
    pub events_processed: u32,
    pub entries_created: u32,
    pub errors: u32,
    pub new_cursor: Option<String>,
    pub event_results: Vec<SyncEventResult>,
}

#[derive(Debug, Clone)]
pub struct SyncEventResult {
    pub event_id: String,
    pub event_type: String,
    pub status: SyncEventStatus,
}

#[derive(Debug, Clone)]
pub enum SyncEventStatus {
    Created { entry_id: String },
    Skipped { reason: String },
    Error { message: String },
}

/// Info needed to connect to a service for syncing
pub struct ServiceRecord {
    pub id: String,
    pub name: String,
    pub root_url: String,
    /// `None` on group-hosted books: the key is held by the group's instance and
    /// this machine is not meant to have it. Callers there fetch through the
    /// instance's `/servicefeed` relay instead of talking to the service directly.
    pub api_key: Option<String>,
    pub cursor: Option<String>,
}

/// Display info for the TUI
pub struct ServiceDisplay {
    pub id: String,
    pub name: String,
    pub root_url: String,
    pub status: String,
    pub last_synced_at: Option<String>,
    pub events_processed: u32,
    pub entries_created: u32,
}

// --- Queries ---

pub fn list_services(conn: &Connection) -> Result<Vec<ServiceDisplay>, EventServiceError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, root_url, status, last_synced_at, events_processed, entries_created
         FROM event_services WHERE status = 'active' ORDER BY name",
    )?;

    let services = stmt
        .query_map([], |row| {
            Ok(ServiceDisplay {
                id: row.get(0)?,
                name: row.get(1)?,
                root_url: row.get(2)?,
                status: row.get(3)?,
                last_synced_at: row.get(4)?,
                events_processed: row.get::<_, i64>(5)? as u32,
                entries_created: row.get::<_, i64>(6)? as u32,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(services)
}

pub fn get_service(conn: &Connection, service_id: &str) -> Result<ServiceRecord, EventServiceError> {
    conn.query_row(
        "SELECT id, name, root_url, api_key, cursor FROM event_services WHERE id = ?1 AND status = 'active'",
        [service_id],
        |row| {
            Ok(ServiceRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                root_url: row.get(2)?,
                api_key: row.get(3)?,
                cursor: row.get(4)?,
            })
        },
    )
    .map_err(|_| EventServiceError::NotFound(service_id.to_string()))
}

// --- Commands ---

/// Register an external event service.
///
/// The uniqueness invariant — no *active* service is already registered for the
/// same (normalized) root URL — is enforced *inside* the append transaction via
/// [`EventStore::append_checked`]. The `event_services` primary key is a fresh
/// UUID, so it never collides; the real duplicate risk is registering the same
/// remote service twice (which would double-sync it). Checking under the write
/// lock stops two concurrent registrations of the same URL from both passing the
/// check and both appending. Retries on a head move.
pub fn register_service(
    store: &mut EventStore,
    user_id: &str,
    name: &str,
    root_url: &str,
    api_key: &str,
) -> Result<StoredEvent, EventServiceError> {
    let url = root_url.trim_end_matches('/').to_string();
    let user_id = user_id.to_string();
    let name = name.to_string();
    let api_key = api_key.to_string();

    loop {
        let head = store
            .latest_id()
            .map_err(|e| EventServiceError::StoreError(e.to_string()))?
            .unwrap_or(0);
        let outcome = store
            .append_checked(
                head,
                |tx| {
                    // Uniqueness: no active service already registered for this URL.
                    let exists: bool = tx
                        .query_row(
                            "SELECT 1 FROM event_services WHERE root_url = ?1 AND status = 'active'",
                            [&url],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if exists {
                        return Ok(Verdict::Reject(EventServiceError::AlreadyExists(
                            url.clone(),
                        )));
                    }

                    let event = Event::EventServiceRegistered {
                        service_id: uuid::Uuid::new_v4().to_string(),
                        name: name.clone(),
                        root_url: url.clone(),
                        api_key: Some(api_key.clone()),
                    };
                    Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )
            .map_err(|e| EventServiceError::StoreError(e.to_string()))?;

        match outcome {
            CheckedOutcome::Appended(stored) => return Ok(stored),
            CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

pub fn remove_service(
    store: &mut EventStore,
    user_id: &str,
    service_id: &str,
) -> Result<StoredEvent, EventServiceError> {
    let event = Event::EventServiceRemoved {
        service_id: service_id.to_string(),
    };

    let envelope = EventEnvelope::new(event, user_id.to_string());
    let stored = store
        .append(envelope)
        .map_err(|e| EventServiceError::StoreError(e.to_string()))?;
        store
        .apply_projection(&stored)
        .map_err(|e| EventServiceError::StoreError(e.to_string()))?;
    Ok(stored)
}

// --- Planning a remote event ---

/// What one remote event turns into, decided against the books but not yet
/// written to them.
///
/// This is the seam that lets group-hosted books ingest at all. A replica cannot
/// append — its event ids belong to the group server — so a member there plans
/// against their local projection and submits the result to the server's command
/// endpoints. Standalone books plan and post in one step, but plan through the
/// *same* function, so there is one answer to "what does a sale post to" rather
/// than one per deployment shape.
#[derive(Debug)]
pub enum PlannedIngest {
    /// A journal entry, ready for `post_entry` locally or `post-entries` over the
    /// sync transport.
    Entry(Box<PostEntryCommand>),
    /// A bill: goods arrived, so inventory rises and the supplier is owed. Kept
    /// distinct from `Entry` because the payable has to be trackable, not merely
    /// balanced.
    Bill(Box<ReceiveBillCommand>),
    /// Nothing to post, and that is the correct outcome — a purchase-order
    /// commitment is a promise, not a transaction. The reason is carried so the
    /// sync report can say why rather than showing a silent gap.
    Nothing { reason: String },
}

/// Why a remote event could not be turned into a plan.
///
/// Separate from [`IngestError`] because the first two are about the *event* —
/// a producer sending a shape we do not understand — while an `Ingest` failure
/// is about these books: a mapping the user has not set yet. They read
/// differently in the sync report and they have different fixes.
#[derive(Error, Debug)]
pub enum PlanError {
    #[error("Unexpected {event_type} payload shape: {source}")]
    Payload {
        event_type: String,
        source: serde_json::Error,
    },
    #[error("Unknown event type: {0}")]
    UnknownType(String),
    #[error("{0}")]
    Ingest(#[from] IngestError),
}

/// The idempotency key stamped on everything one service's event produces.
///
/// Scoped by service name so two services that both number their events from 1
/// cannot collide and silently swallow each other's records as duplicates.
pub fn event_reference(service_name: &str, remote_event_id: &str) -> String {
    format!("{}:{}", service_name, remote_event_id)
}

/// Decide what a remote event posts, without writing anything.
///
/// `conn` supplies the account mappings and vendor rules; on a replica that is a
/// perfectly good read of books the server owns. Nothing here appends, so it is
/// safe on hosted books, which is the whole point.
pub fn plan_remote_event(
    conn: &Connection,
    service_name: &str,
    remote_event: &RemoteEvent,
) -> Result<PlannedIngest, PlanError> {
    let reference = event_reference(service_name, &remote_event.id);

    macro_rules! parsed {
        ($ty:ty) => {
            serde_json::from_value::<$ty>(remote_event.data.clone()).map_err(|e| {
                PlanError::Payload {
                    event_type: remote_event.event_type.clone(),
                    source: e,
                }
            })?
        };
    }

    match remote_event.event_type.as_str() {
        "sale" => {
            let mut data: IngestSaleData = parsed!(IngestSaleData);
            data.reference = Some(reference);
            Ok(PlannedIngest::Entry(Box::new(ingest_commands::plan_sale(
                conn,
                data,
                JournalEntrySource::EventService,
            )?)))
        }
        "refund" => {
            let mut data: IngestRefundData = parsed!(IngestRefundData);
            data.reference = Some(reference);
            Ok(PlannedIngest::Entry(Box::new(ingest_commands::plan_refund(
                conn,
                data,
                JournalEntrySource::EventService,
            )?)))
        }
        "purchase_order" => {
            let mut data: IngestPurchaseOrderData = parsed!(IngestPurchaseOrderData);
            data.reference = Some(reference);
            // Legacy detection, matching `process_remote_events`: a `payment`
            // field means the money moved, so it posts. Without one the PO is a
            // commitment — an intention to buy, which is not yet a transaction.
            if remote_event.data.get("payment").is_some() {
                Ok(PlannedIngest::Entry(Box::new(
                    ingest_commands::plan_purchase_order(
                        conn,
                        data,
                        JournalEntrySource::EventService,
                    )?,
                )))
            } else {
                Ok(PlannedIngest::Nothing {
                    reason: "Commitment recorded (no journal entry)".to_string(),
                })
            }
        }
        "goods_received" => {
            let mut data: IngestGoodsReceivedData = parsed!(IngestGoodsReceivedData);
            data.reference = Some(reference);
            Ok(PlannedIngest::Bill(Box::new(
                ingest_commands::plan_goods_received(conn, data)?,
            )))
        }
        "inventory_adjustment" => {
            let mut data: IngestInventoryAdjustmentData = parsed!(IngestInventoryAdjustmentData);
            data.reference = Some(reference);
            Ok(PlannedIngest::Entry(Box::new(
                ingest_commands::plan_inventory_adjustment(
                    conn,
                    data,
                    JournalEntrySource::EventService,
                )?,
            )))
        }
        other => Err(PlanError::UnknownType(other.to_string())),
    }
}

/// Fetch events from a remote event service. Designed to run on a background thread.
pub fn fetch_all_remote_events(
    root_url: &str,
    api_key: &str,
    initial_cursor: Option<&str>,
) -> Result<(Vec<RemoteEvent>, Option<String>), String> {
    let client = reqwest::blocking::Client::new();
    let events_url = format!("{}/api/accounting/events", root_url);

    let mut all_events = Vec::new();
    let mut cursor = initial_cursor.map(|s| s.to_string());

    loop {
        let mut url = events_url.clone();
        let mut params = Vec::new();
        if let Some(ref c) = cursor {
            params.push(format!("since={}", c));
        }
        params.push("limit=100".to_string());
        if !params.is_empty() {
            url = format!("{}?{}", url, params.join("&"));
        }

        let resp = client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .map_err(|e| format!("Request failed: {}", e))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().unwrap_or_default();
            return Err(format!("Service returned {} - {}", status, text));
        }

        let body: RemoteEventsResponse = resp
            .json()
            .map_err(|e| format!("Invalid response: {}", e))?;

        let has_more = body.has_more;
        let new_cursor = body.cursor.clone();
        all_events.extend(body.events);

        cursor = new_cursor;

        if !has_more {
            break;
        }
    }

    Ok((all_events, cursor))
}

/// Process fetched remote events into journal entries. Runs on main thread with &mut EventStore.
pub fn process_remote_events(
    store: &mut EventStore,
    service_id: &str,
    service_name: &str,
    events: Vec<RemoteEvent>,
    new_cursor: Option<String>,
) -> Result<SyncResult, EventServiceError> {
    let mut entries_created: u32 = 0;
    let mut errors: u32 = 0;
    let events_processed = events.len() as u32;
    let mut event_results: Vec<SyncEventResult> = Vec::new();

    for remote_event in &events {
        let event_id = remote_event.id.clone();
        let event_type = remote_event.event_type.clone();

        // Plan first, then write. Both halves are shared with the group-hosted
        // path (see `plan_remote_event`), so a sale posts to the same accounts
        // whichever kind of books it lands in.
        //
        // A failure here is per-event: one malformed record, or one event needing
        // a mapping nobody has set yet, must not stop the rest of the batch. The
        // reason travels into the sync report instead of being swallowed.
        let planned = match plan_remote_event(store.connection(), service_name, remote_event) {
            Ok(planned) => planned,
            Err(e) => {
                errors += 1;
                event_results.push(SyncEventResult {
                    event_id,
                    event_type,
                    status: SyncEventStatus::Error {
                        message: e.to_string(),
                    },
                });
                continue;
            }
        };

        let result = match planned {
            PlannedIngest::Entry(cmd) => {
                ingest_commands::post_planned_entry(store, "event-service-sync", *cmd)
            }
            PlannedIngest::Bill(cmd) => {
                ingest_commands::post_planned_bill(store, "event-service-sync", *cmd)
            }
            PlannedIngest::Nothing { reason } => {
                event_results.push(SyncEventResult {
                    event_id,
                    event_type,
                    status: SyncEventStatus::Skipped { reason },
                });
                continue;
            }
        };

        match result {
            Ok(r) if r.was_duplicate => event_results.push(SyncEventResult {
                event_id,
                event_type,
                status: SyncEventStatus::Skipped {
                    reason: "Duplicate".to_string(),
                },
            }),
            Ok(r) => {
                entries_created += 1;
                event_results.push(SyncEventResult {
                    event_id,
                    event_type,
                    status: SyncEventStatus::Created {
                        entry_id: r.entry_id,
                    },
                });
            }
            Err(e) => {
                errors += 1;
                event_results.push(SyncEventResult {
                    event_id,
                    event_type,
                    status: SyncEventStatus::Error {
                        message: e.to_string(),
                    },
                });
            }
        }
    }

    // Only advance the cursor when every event was handled cleanly. If any
    // errored (e.g. a missing account mapping), keep the previous position so
    // the next sync re-fetches and retries them — events already posted are
    // skipped as duplicates via their reference, so retrying is safe.
    if errors == 0 {
        if let Some(ref c) = new_cursor {
            store.connection().execute(
                "UPDATE event_services SET cursor = ?1 WHERE id = ?2",
                params![c, service_id],
            )?;
        }
    }

    // Record sync event
    let sync_event = Event::EventServiceSynced {
        service_id: service_id.to_string(),
        events_processed,
        entries_created,
        errors,
    };
    let envelope = EventEnvelope::new(sync_event, "event-service-sync".to_string());
    let stored = store
        .append(envelope)
        .map_err(|e| EventServiceError::StoreError(e.to_string()))?;
        store
        .apply_projection(&stored)
        .map_err(|e| EventServiceError::StoreError(e.to_string()))?;

    Ok(SyncResult {
        events_processed,
        entries_created,
        errors,
        new_cursor,
        event_results,
    })
}

// --- Staged Events ---

#[derive(Debug, Clone)]
pub struct StagedEventDisplay {
    pub id: String,
    pub remote_event_id: String,
    pub event_type: String,
    pub timestamp: String,
    pub data: serde_json::Value,
    pub status: String,
    pub error_message: Option<String>,
    pub readiness: StagedEventReadiness,
    pub description: String,
    pub amount_cents: Option<i64>,
}

#[derive(Debug, Clone)]
pub enum StagedEventReadiness {
    Ready,
    NeedsMapping(Vec<String>),
}

/// Determine which ingest mapping keys are required for an event type
pub fn required_mapping_keys(event_type: &str, data: &serde_json::Value) -> Vec<&'static str> {
    match event_type {
        "sale" => {
            let method = data
                .get("payment_method")
                .and_then(|v| v.as_str())
                .unwrap_or("cash");
            let mut keys = vec![
                if method == "square" {
                    "pos_square"
                } else {
                    "pos_cash"
                },
                "pos_revenue",
                "cogs",
                "inventory",
            ];
            if data
                .get("tax_collected_cents")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                > 0
            {
                keys.push("sales_tax_payable");
            }
            keys
        }
        "purchase_order" => {
            if data.get("payment").is_some() {
                let payment = data
                    .get("payment")
                    .and_then(|v| v.as_str())
                    .unwrap_or("on_credit");
                vec![
                    "inventory",
                    if payment == "cash" {
                        "pos_cash"
                    } else {
                        "accounts_payable"
                    },
                ]
            } else {
                // Commitment-only PO — no mappings needed, no journal entry
                vec![]
            }
        }
        "goods_received" => vec!["inventory", "accounts_payable"],
        "inventory_adjustment" => vec!["inventory", "inventory_adjustment"],
        _ => vec![],
    }
}

/// Stage fetched events into the staging table. Returns count of newly staged events.
pub fn stage_events(
    conn: &Connection,
    service_id: &str,
    events: Vec<RemoteEvent>,
) -> Result<usize, EventServiceError> {
    let mut staged = 0;
    for event in &events {
        let id = uuid::Uuid::new_v4().to_string();
        let data_str = event.data.to_string();
        let result = conn.execute(
            "INSERT OR IGNORE INTO staged_service_events (id, service_id, remote_event_id, event_type, data, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, service_id, event.id, event.event_type, data_str, event.timestamp],
        )?;
        if result > 0 {
            staged += 1;
        }
    }
    Ok(staged)
}

/// Load pending staged events for a service, with readiness computed
pub fn load_staged_events(
    conn: &Connection,
    service_id: &str,
) -> Result<Vec<StagedEventDisplay>, EventServiceError> {
    // Load existing mappings
    let mut existing_mappings = std::collections::HashSet::new();
    let mut stmt = conn.prepare("SELECT key FROM ingest_account_mappings")?;
    let keys = stmt.query_map([], |row| row.get::<_, String>(0))?;
    for key in keys {
        if let Ok(k) = key {
            existing_mappings.insert(k);
        }
    }

    let mut stmt = conn.prepare(
        "SELECT id, remote_event_id, event_type, data, timestamp, status, error_message
         FROM staged_service_events
         WHERE service_id = ?1 AND status IN ('pending', 'error')
         ORDER BY timestamp ASC",
    )?;

    let rows = stmt
        .query_map([service_id], |row| {
            let id: String = row.get(0)?;
            let remote_event_id: String = row.get(1)?;
            let event_type: String = row.get(2)?;
            let data_str: String = row.get(3)?;
            let timestamp: String = row.get(4)?;
            let status: String = row.get(5)?;
            let error_message: Option<String> = row.get(6)?;
            Ok((
                id,
                remote_event_id,
                event_type,
                data_str,
                timestamp,
                status,
                error_message,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut events = Vec::new();
    for (id, remote_event_id, event_type, data_str, timestamp, status, error_message) in rows {
        let data: serde_json::Value =
            serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);

        let required = required_mapping_keys(&event_type, &data);
        let missing: Vec<String> = required
            .iter()
            .filter(|k| !existing_mappings.contains(**k))
            .map(|k| k.to_string())
            .collect();

        let readiness = if missing.is_empty() {
            StagedEventReadiness::Ready
        } else {
            StagedEventReadiness::NeedsMapping(missing)
        };

        // Extract description and amount for display
        let description = extract_description(&event_type, &data);
        let amount_cents = extract_amount(&event_type, &data);

        events.push(StagedEventDisplay {
            id,
            remote_event_id,
            event_type,
            timestamp,
            data,
            status,
            error_message,
            readiness,
            description,
            amount_cents,
        });
    }

    Ok(events)
}

fn extract_description(event_type: &str, data: &serde_json::Value) -> String {
    let supplier_or_customer = data
        .get("supplier")
        .or_else(|| data.get("customer"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let memo = data.get("memo").and_then(|v| v.as_str()).unwrap_or("");

    let items_desc = data
        .get("items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| {
                    let name = i.get("name")?.as_str()?;
                    let qty = i.get("qty").and_then(|v| v.as_u64()).unwrap_or(1);
                    Some(format!("{}x {}", qty, name))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    if !memo.is_empty() {
        memo.to_string()
    } else if !supplier_or_customer.is_empty() {
        if items_desc.is_empty() {
            format!("{} ({})", supplier_or_customer, event_type)
        } else {
            format!("{}: {}", supplier_or_customer, items_desc)
        }
    } else if !items_desc.is_empty() {
        items_desc
    } else {
        event_type.to_string()
    }
}

fn extract_amount(event_type: &str, data: &serde_json::Value) -> Option<i64> {
    let items = data.get("items")?.as_array()?;
    match event_type {
        "sale" => {
            let revenue: i64 = items
                .iter()
                .map(|i| {
                    let qty = i.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                    let price = i
                        .get("unit_price_cents")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    qty * price
                })
                .sum();
            Some(revenue)
        }
        "purchase_order" | "goods_received" => {
            let cost: i64 = items
                .iter()
                .map(|i| {
                    let qty = i.get("qty").and_then(|v| v.as_i64()).unwrap_or(0);
                    let cost = i
                        .get("unit_cost_cents")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    qty * cost
                })
                .sum();
            Some(cost)
        }
        "inventory_adjustment" => {
            let net: i64 = items
                .iter()
                .map(|i| {
                    let qty = i.get("qty_delta").and_then(|v| v.as_i64()).unwrap_or(0);
                    let cost = i
                        .get("unit_cost_cents")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    qty * cost
                })
                .sum();
            Some(net.abs())
        }
        _ => None,
    }
}

/// Import a single staged event by processing it through the ingest system
pub fn import_staged_event(
    store: &mut EventStore,
    staged_id: &str,
    service_name: &str,
) -> Result<String, EventServiceError> {
    // Load the staged event
    let (remote_event_id, event_type, data_str): (String, String, String) = store
        .connection()
        .query_row(
            "SELECT remote_event_id, event_type, data FROM staged_service_events WHERE id = ?1 AND status IN ('pending', 'error')",
            [staged_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|_| EventServiceError::NotFound(staged_id.to_string()))?;

    let data: serde_json::Value = serde_json::from_str(&data_str)
        .map_err(|e| EventServiceError::ParseError(e.to_string()))?;

    let reference = format!("{}:{}", service_name, remote_event_id);

    // Dispatch to the appropriate ingest function
    let result = match event_type.as_str() {
        "sale" => {
            let mut ingest_data: crate::commands::ingest_commands::IngestSaleData =
                serde_json::from_value(data)
                    .map_err(|e| EventServiceError::ParseError(e.to_string()))?;
            ingest_data.reference = Some(reference);
            crate::commands::ingest_commands::ingest_sale(
                store,
                "event-service-sync",
                ingest_data,
                crate::events::types::JournalEntrySource::EventService,
            )
            .map(|r| r.entry_id)
            .map_err(|e| EventServiceError::StoreError(e.to_string()))
        }
        "purchase_order" => {
            if data_str.contains("\"payment\"") {
                let mut ingest_data: crate::commands::ingest_commands::IngestPurchaseOrderData =
                    serde_json::from_str(&data_str)
                        .map_err(|e| EventServiceError::ParseError(e.to_string()))?;
                ingest_data.reference = Some(reference);
                crate::commands::ingest_commands::ingest_purchase_order(
                    store,
                    "event-service-sync",
                    ingest_data,
                    crate::events::types::JournalEntrySource::EventService,
                )
                .map(|r| r.entry_id)
                .map_err(|e| EventServiceError::StoreError(e.to_string()))
            } else {
                // Commitment-only, no journal entry
                Ok(String::new())
            }
        }
        "goods_received" => {
            let mut ingest_data: crate::commands::ingest_commands::IngestGoodsReceivedData =
                serde_json::from_str(&data_str)
                    .map_err(|e| EventServiceError::ParseError(e.to_string()))?;
            ingest_data.reference = Some(reference);
            crate::commands::ingest_commands::ingest_goods_received(
                store,
                "event-service-sync",
                ingest_data,
            )
            .map(|r| r.entry_id)
            .map_err(|e| EventServiceError::StoreError(e.to_string()))
        }
        "inventory_adjustment" => {
            let mut ingest_data: crate::commands::ingest_commands::IngestInventoryAdjustmentData =
                serde_json::from_str(&data_str)
                    .map_err(|e| EventServiceError::ParseError(e.to_string()))?;
            ingest_data.reference = Some(reference);
            crate::commands::ingest_commands::ingest_inventory_adjustment(
                store,
                "event-service-sync",
                ingest_data,
                crate::events::types::JournalEntrySource::EventService,
            )
            .map(|r| r.entry_id)
            .map_err(|e| EventServiceError::StoreError(e.to_string()))
        }
        other => Err(EventServiceError::ParseError(format!(
            "Unknown event type: {}",
            other
        ))),
    };

    match result {
        Ok(entry_id) => {
            store.connection().execute(
                "UPDATE staged_service_events SET status = 'imported', error_message = NULL WHERE id = ?1",
                [staged_id],
            )?;
            Ok(entry_id)
        }
        Err(e) => {
            let msg = e.to_string();
            store.connection().execute(
                "UPDATE staged_service_events SET status = 'error', error_message = ?1 WHERE id = ?2",
                params![msg, staged_id],
            )?;
            Err(e)
        }
    }
}

/// Save an ingest account mapping (upsert)
pub fn save_ingest_mapping(
    conn: &Connection,
    key: &str,
    account_id: &str,
) -> Result<(), EventServiceError> {
    conn.execute(
        "INSERT INTO ingest_account_mappings (key, account_id, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET account_id = ?2, updated_at = datetime('now')",
        params![key, account_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations::SchemaStore;

    fn setup() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        store.init_schema().unwrap();
        store
    }

    #[test]
    fn register_service_happy_path() {
        let mut store = setup();
        register_service(&mut store, "u", "Acme", "https://acme.test/", "key-1").unwrap();

        // URL is normalized (trailing slash trimmed) and the row is active.
        let (name, url, status): (String, String, String) = store
            .connection()
            .query_row(
                "SELECT name, root_url, status FROM event_services",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "Acme");
        assert_eq!(url, "https://acme.test");
        assert_eq!(status, "active");
    }

    #[test]
    fn register_duplicate_url_rejected_appends_nothing() {
        let mut store = setup();
        register_service(&mut store, "u", "Acme", "https://acme.test", "key-1").unwrap();

        let before = store.count().unwrap();
        // Same URL (trailing slash normalizes to the same value) ⇒ rejected.
        let err = register_service(&mut store, "u", "Acme Dup", "https://acme.test/", "key-2")
            .unwrap_err();
        assert!(matches!(err, EventServiceError::AlreadyExists(_)));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected registration appends nothing"
        );
        let rows: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM event_services WHERE root_url = 'https://acme.test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "only one active service for the URL");
    }
}

#[cfg(test)]
mod planning_tests {
    use super::*;
    use crate::commands::ingest_commands::set_account_mapping;
    use crate::events::types::{EventAccountType, EventEnvelope};
    use crate::store::migrations::SchemaStore;
    use crate::store::projections::ProjectionStore;

    /// A ledger with the chart and mappings a POS sale and a goods receipt need.
    fn books() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        store.init_schema().unwrap();
        for (id, ty, num, name) in [
            ("cash", EventAccountType::Asset, "1000", "Cash"),
            ("inv", EventAccountType::Asset, "1200", "Inventory"),
            ("rev", EventAccountType::Revenue, "4000", "Sales"),
            ("cogs", EventAccountType::Expense, "5000", "COGS"),
            ("ap", EventAccountType::Liability, "2000", "Accounts payable"),
            ("adj", EventAccountType::Expense, "5100", "Inventory adjustment"),
            ("ref", EventAccountType::Revenue, "4100", "Refunds"),
        ] {
            let ev = Event::AccountCreated {
                account_id: id.to_string(),
                account_type: ty,
                account_number: num.to_string(),
                name: name.to_string(),
                parent_id: None,
                currency: None,
                description: None,
            };
            let stored = store
                .append(EventEnvelope::new(ev, "test".to_string()))
                .unwrap();
            store.apply_projection(&stored).unwrap();
        }
        let conn = store.connection();
        for (key, account) in [
            ("pos_cash", "cash"),
            ("pos_revenue", "rev"),
            ("inventory", "inv"),
            ("cogs", "cogs"),
            ("accounts_payable", "ap"),
            ("inventory_adjustment", "adj"),
            ("refunds", "ref"),
        ] {
            set_account_mapping(conn, key, account).unwrap();
        }
        store
    }

    fn remote(id: &str, event_type: &str, data: serde_json::Value) -> RemoteEvent {
        RemoteEvent {
            id: id.to_string(),
            event_type: event_type.to_string(),
            data,
            timestamp: "2026-08-01T00:00:00Z".to_string(),
        }
    }

    fn sale() -> RemoteEvent {
        remote(
            "e-1",
            "sale",
            serde_json::json!({
                "date": "2026-08-01",
                "payment_method": "cash",
                "items": [{"name": "Tube", "qty": 2, "unit_price_cents": 800, "unit_cost_cents": 300}],
            }),
        )
    }

    /// The property the whole split exists for: planning reads, and only reads.
    /// If it appended anything, a member on group-hosted books could not run it —
    /// their event ids belong to the group server, and one locally minted id makes
    /// the two logs stop being the same log.
    #[test]
    fn planning_writes_nothing_to_the_books() {
        let store = books();
        let before = store.count().unwrap();
        let entries: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_entries", [], |r| r.get(0))
            .unwrap();

        plan_remote_event(store.connection(), "Bugbear", &sale()).expect("a sale must plan");

        assert_eq!(store.count().unwrap(), before, "planning appended an event");
        assert_eq!(
            store
                .connection()
                .query_row::<i64, _, _>("SELECT COUNT(*) FROM journal_entries", [], |r| r.get(0))
                .unwrap(),
            entries,
            "planning posted an entry"
        );
    }

    /// Every event type the local sync handles has to plan, because the hosted
    /// path has no fallback: an event that will not plan simply never reaches the
    /// group's books.
    #[test]
    fn every_supported_event_type_plans() {
        let store = books();
        let conn = store.connection();

        let cases = [
            sale(),
            remote(
                "e-2",
                "refund",
                serde_json::json!({
                    "date": "2026-08-02",
                    "payment_method": "cash",
                    "restock": false,
                    "items": [{"name": "Tube", "qty": 1, "unit_price_cents": 800, "unit_cost_cents": 300}],
                }),
            ),
            remote(
                "e-3",
                "purchase_order",
                serde_json::json!({
                    "date": "2026-08-03",
                    "payment": "on_credit",
                    "supplier": "QBP",
                    "items": [{"name": "Tube", "qty": 10, "unit_cost_cents": 300}],
                }),
            ),
            remote(
                "e-5",
                "inventory_adjustment",
                serde_json::json!({
                    "date": "2026-08-05",
                    "items": [{"name": "Tube", "qty_delta": -2, "unit_cost_cents": 300}],
                }),
            ),
        ];
        for ev in &cases {
            match plan_remote_event(conn, "Bugbear", ev) {
                Ok(PlannedIngest::Entry(cmd)) => assert_eq!(
                    cmd.reference.as_deref(),
                    Some(format!("Bugbear:{}", ev.id).as_str()),
                    "the plan must carry the idempotency reference, or a re-sync \
                     posts everything twice"
                ),
                other => panic!(
                    "{} planned as something other than an entry: {}",
                    ev.event_type,
                    match other {
                        Ok(PlannedIngest::Bill(_)) => "a bill".to_string(),
                        Ok(PlannedIngest::Nothing { reason }) => format!("nothing ({reason})"),
                        Err(e) => e.to_string(),
                        _ => unreachable!(),
                    }
                ),
            }
        }

        // Goods received raises a bill, not a bare entry — the supplier is owed,
        // and a plain entry would balance the books while losing the obligation.
        let gr = remote(
            "e-4",
            "goods_received",
            serde_json::json!({
                "date": "2026-08-04",
                "supplier": "QBP",
                "items": [{"name": "Tube", "qty": 10, "unit_cost_cents": 300}],
            }),
        );
        match plan_remote_event(conn, "Bugbear", &gr) {
            Ok(PlannedIngest::Bill(cmd)) => {
                assert_eq!(cmd.amount, 3000);
                assert_eq!(cmd.reference.as_deref(), Some("Bugbear:e-4"));
                assert_eq!(cmd.expense_account_id, "inv");
                assert_eq!(cmd.ap_account_id, "ap");
            }
            _ => panic!("goods_received must plan as a bill"),
        }
    }

    /// A purchase order with no payment is an intention to buy. Planning it as
    /// `Nothing` — with a reason — is what keeps it out of the books while still
    /// accounting for it in the sync report, rather than leaving a silent gap.
    #[test]
    fn a_commitment_only_purchase_order_plans_to_nothing() {
        let store = books();
        let po = remote(
            "e-6",
            "purchase_order",
            serde_json::json!({
                "date": "2026-08-06",
                "supplier": "QBP",
                "items": [{"name": "Tube", "qty": 10, "unit_cost_cents": 300}],
            }),
        );
        match plan_remote_event(store.connection(), "Bugbear", &po) {
            Ok(PlannedIngest::Nothing { reason }) => assert!(!reason.is_empty()),
            _ => panic!("a PO with no payment must not post"),
        }
    }

    /// The three ways planning fails have three different fixes — update the
    /// producer, wait for support, set a mapping — so they must not collapse into
    /// one message.
    #[test]
    fn the_ways_planning_fails_stay_distinguishable() {
        let store = books();
        let conn = store.connection();

        let unknown = remote("e-7", "loyalty_points_issued", serde_json::json!({}));
        assert!(matches!(
            plan_remote_event(conn, "Bugbear", &unknown),
            Err(PlanError::UnknownType(_))
        ));

        let malformed = remote("e-8", "sale", serde_json::json!({"date": "2026-08-01"}));
        assert!(matches!(
            plan_remote_event(conn, "Bugbear", &malformed),
            Err(PlanError::Payload { .. })
        ));

        // A mapping these books have not set: the fix is on the Services page, and
        // the message has to name the key so the user knows which row to fill in.
        conn.execute("DELETE FROM ingest_account_mappings WHERE key = 'cogs'", [])
            .unwrap();
        let err = plan_remote_event(conn, "Bugbear", &sale()).unwrap_err();
        assert!(matches!(err, PlanError::Ingest(IngestError::MissingMapping(_))));
        assert!(err.to_string().contains("cogs"), "{err}");
    }
}
