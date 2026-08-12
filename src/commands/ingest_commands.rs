use crate::commands::bill_commands::{BillCommandError, BillCommands, ReceiveBillCommand};
use crate::commands::entry_commands::{
    EntryCommandError, EntryCommands, EntryLine, PostEntryCommand,
};
use crate::domain::PaymentTerms;
use crate::events::types::JournalEntrySource;
use crate::store::event_store::EventStore;
use chrono::NaiveDate;
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum IngestError {
    #[error("Missing ingest account mappings: {0}")]
    MissingMapping(String),
    #[error("Failed to post entry: {0}")]
    EntryError(String),
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("No items provided")]
    EmptyItems,
    #[error("Net adjustment is zero")]
    ZeroAdjustment,
    #[error("Invalid date: {0}")]
    InvalidDate(String),
    #[error("No payment method provided")]
    MissingPayment,
    #[error("Payments total {got} cents but the transaction is {expected} cents")]
    PaymentMismatch { expected: i64, got: i64 },
}

pub struct IngestResult {
    pub entry_id: String,
    pub was_duplicate: bool,
}

/// A configurable account-mapping slot used by the ingest/import flows.
pub struct MappingDef {
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
}

/// Canonical list of ingest account-mapping keys — the single source of truth
/// shared by the server's validation and the desktop mapping editor.
pub const MAPPING_DEFS: &[MappingDef] = &[
    MappingDef { key: "pos_square", label: "Square balance (asset)", group: "Square" },
    MappingDef { key: "pos_stripe", label: "Stripe balance (asset)", group: "Stripe" },
    MappingDef { key: "pos_revenue", label: "Sales revenue", group: "Square sales" },
    MappingDef { key: "refunds", label: "Refunds / returns (contra-revenue)", group: "Square sales" },
    MappingDef { key: "square_fees", label: "Processing fees (expense)", group: "Square sales" },
    MappingDef { key: "sales_tax_payable", label: "Sales tax payable (liability)", group: "Square sales" },
    MappingDef { key: "tips_payable", label: "Tips payable (liability)", group: "Square sales" },
    MappingDef { key: "payroll_wages_expense", label: "Wages expense", group: "Square payroll" },
    MappingDef { key: "payroll_tax_expense", label: "Employer payroll taxes (expense)", group: "Square payroll" },
    MappingDef { key: "payroll_taxes_payable", label: "Payroll taxes payable (liability)", group: "Square payroll" },
    MappingDef { key: "pos_cash", label: "Cash (POS)", group: "Point of sale" },
    MappingDef { key: "cogs", label: "Cost of goods sold (expense)", group: "Inventory" },
    MappingDef { key: "inventory", label: "Inventory (asset)", group: "Inventory" },
    MappingDef { key: "inventory_adjustment", label: "Inventory adjustment", group: "Inventory" },
    MappingDef { key: "accounts_payable", label: "Accounts payable (liability)", group: "Inventory" },
    MappingDef { key: "amazon_clearing", label: "Amazon clearing/liability", group: "Amazon" },
];

/// All valid ingest mapping keys.
pub fn mapping_keys() -> Vec<&'static str> {
    MAPPING_DEFS.iter().map(|d| d.key).collect()
}

/// Load every saved ingest mapping as `key -> account_id`.
pub fn load_all_mappings(conn: &Connection) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT key, account_id FROM ingest_account_mappings") {
        if let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for row in rows.flatten() {
                out.insert(row.0, row.1);
            }
        }
    }
    out
}

/// Upsert a single ingest account mapping.
pub fn set_account_mapping(
    conn: &Connection,
    key: &str,
    account_id: &str,
) -> Result<(), rusqlite::Error> {
    conn.execute(
        "INSERT INTO ingest_account_mappings (key, account_id, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET account_id = ?2, updated_at = datetime('now')",
        rusqlite::params![key, account_id],
    )?;
    Ok(())
}

// --- Data types (matching remote API shapes) ---

#[derive(Debug, Deserialize)]
pub struct IngestSaleData {
    pub date: String,
    pub reference: Option<String>,
    pub memo: Option<String>,
    pub items: Vec<IngestSaleItem>,
    /// Split tender: one payment per method used (they must sum to revenue +
    /// tax). Preferred over the legacy single `payment_method`.
    #[serde(default)]
    pub payments: Vec<IngestPayment>,
    /// Legacy single payment method (pre-split-tender). Still accepted and
    /// treated as one payment for the full amount; ignored when `payments` is
    /// present. Optional so new producers can send `payments` instead.
    #[serde(default)]
    pub payment_method: Option<IngestPaymentMethod>,
    pub tax_collected_cents: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct IngestSaleItem {
    pub name: String,
    pub qty: u32,
    pub unit_price_cents: i64,
    pub unit_cost_cents: i64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestPaymentMethod {
    Cash,
    Square,
    Stripe,
}

impl IngestPaymentMethod {
    /// The ingest mapping key for this method's clearing/balance account.
    fn mapping_key(self) -> &'static str {
        match self {
            IngestPaymentMethod::Cash => "pos_cash",
            IngestPaymentMethod::Square => "pos_square",
            IngestPaymentMethod::Stripe => "pos_stripe",
        }
    }
}

/// One tender in a split-tender sale or refund (e.g. a Stripe deposit followed
/// by the balance on Square in store).
#[derive(Debug, Deserialize)]
pub struct IngestPayment {
    pub method: IngestPaymentMethod,
    pub amount_cents: i64,
}

/// Resolve a transaction's tenders into aggregated `(mapping_key, amount)` pairs
/// (one line per payment account), validating that they sum to `expected_total`.
/// Accepts the new `payments` list or the legacy single `payment_method` (booked
/// as one payment for the whole amount).
fn resolve_tenders(
    payments: &[IngestPayment],
    legacy: Option<IngestPaymentMethod>,
    expected_total: i64,
) -> Result<Vec<(&'static str, i64)>, IngestError> {
    let tenders: Vec<(IngestPaymentMethod, i64)> = if !payments.is_empty() {
        payments.iter().map(|p| (p.method, p.amount_cents)).collect()
    } else if let Some(m) = legacy {
        vec![(m, expected_total)]
    } else {
        return Err(IngestError::MissingPayment);
    };

    let sum: i64 = tenders.iter().map(|(_, a)| *a).sum();
    if sum != expected_total {
        return Err(IngestError::PaymentMismatch {
            expected: expected_total,
            got: sum,
        });
    }

    // Aggregate by account so an entry has at most one line per payment method.
    let mut by_key: std::collections::BTreeMap<&'static str, i64> =
        std::collections::BTreeMap::new();
    for (method, amount) in tenders {
        *by_key.entry(method.mapping_key()).or_insert(0) += amount;
    }
    Ok(by_key.into_iter().collect())
}

fn default_true() -> bool {
    true
}

/// A refund / return — the reverse of a sale. Mirrors [`IngestSaleData`], but the
/// revenue is booked against the `refunds` contra-revenue account and the money
/// flows back out. Returned items with a unit cost are optionally restocked.
#[derive(Debug, Deserialize)]
pub struct IngestRefundData {
    pub date: String,
    pub reference: Option<String>,
    pub memo: Option<String>,
    pub items: Vec<IngestSaleItem>,
    /// Split tender: how the refund is returned across methods (must sum to
    /// revenue + tax). Preferred over the legacy single `payment_method`.
    #[serde(default)]
    pub payments: Vec<IngestPayment>,
    /// Legacy single method the refund is returned via; ignored when `payments`
    /// is present.
    #[serde(default)]
    pub payment_method: Option<IngestPaymentMethod>,
    pub tax_refunded_cents: Option<i64>,
    /// Whether the returned items go back into inventory (reversing COGS).
    /// Defaults to true when the producer omits it.
    #[serde(default = "default_true")]
    pub restock: bool,
}

#[derive(Debug, Deserialize)]
pub struct IngestPurchaseOrderData {
    pub date: String,
    pub reference: Option<String>,
    pub memo: Option<String>,
    pub supplier: Option<String>,
    pub items: Vec<IngestPurchaseItem>,
    pub payment: Option<IngestPurchasePayment>,
}

#[derive(Debug, Deserialize)]
pub struct IngestPurchaseItem {
    pub name: String,
    pub qty: u32,
    pub unit_cost_cents: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestPurchasePayment {
    Cash,
    OnCredit,
}

#[derive(Debug, Deserialize)]
pub struct IngestInventoryAdjustmentData {
    pub date: String,
    pub reference: Option<String>,
    pub memo: Option<String>,
    pub items: Vec<IngestAdjustmentItem>,
}

#[derive(Debug, Deserialize)]
pub struct IngestAdjustmentItem {
    pub name: String,
    pub qty_delta: i32,
    pub unit_cost_cents: i64,
    pub reason: Option<String>,
}

// -- Goods Received (procurement flow) --

#[derive(Debug, Deserialize)]
pub struct IngestGoodsReceivedData {
    pub date: String,
    pub reference: Option<String>,
    pub memo: Option<String>,
    pub supplier: Option<String>,
    pub items: Vec<IngestPurchaseItem>,
    pub purchase_order_reference: Option<String>,
    pub payment_terms: Option<String>,
}

// --- Shared helpers ---

/// Parse an event's date field. Accepts a plain `YYYY-MM-DD`, a full ISO-8601 /
/// RFC-3339 timestamp (`2026-07-03T14:30:00.000Z`, with `Z` or an offset), or
/// any string whose leading 10 characters are a valid date — taking the date
/// component in every case. POS/event producers commonly send full timestamps.
pub fn parse_ingest_date(s: &str) -> Result<NaiveDate, IngestError> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .or_else(|| chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.date_naive()))
        .or_else(|| {
            s.get(..10)
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        })
        .ok_or_else(|| IngestError::InvalidDate(s.to_string()))
}

pub fn load_ingest_mappings(
    conn: &Connection,
    required_keys: &[&str],
) -> Result<HashMap<String, String>, IngestError> {
    let mut mappings = HashMap::new();
    let mut missing = Vec::new();

    for key in required_keys {
        match conn.query_row(
            "SELECT account_id FROM ingest_account_mappings WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        ) {
            Ok(account_id) => {
                mappings.insert(key.to_string(), account_id);
            }
            Err(_) => {
                missing.push(*key);
            }
        }
    }

    if !missing.is_empty() {
        return Err(IngestError::MissingMapping(missing.join(", ")));
    }

    Ok(mappings)
}

/// Check if a non-voided journal entry with this reference already exists.
pub fn check_idempotent(conn: &Connection, reference: &str) -> Option<String> {
    conn.query_row(
        "SELECT id FROM journal_entries WHERE reference = ?1 AND is_void = 0",
        [reference],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// The "we already have this one" answer, for an ingest about to be posted
/// locally.
///
/// A pre-transaction check, so the common re-import case costs a read rather than
/// a rejected append. It is not the correctness boundary — that is the in-txn
/// duplicate-reference check plus the unique index behind it, which is what
/// catches a concurrent import that won the race after this returned `None`.
fn already_posted(conn: &Connection, reference: Option<&str>) -> Option<IngestResult> {
    let existing_id = check_idempotent(conn, reference?)?;
    Some(IngestResult {
        entry_id: existing_id,
        was_duplicate: true,
    })
}

fn extract_entry_id(stored: &crate::events::types::StoredEvent) -> String {
    if let crate::events::types::Event::JournalEntryPosted { entry_id, .. } = &stored.event {
        entry_id.clone()
    } else {
        String::new()
    }
}

/// Post an ingest journal entry through the idempotent path, returning
/// `(entry_id, was_duplicate)`. The pre-transaction [`check_idempotent`] handles
/// the common re-import case; this additionally maps `post_entry`'s *in-txn*
/// duplicate-reference rejection — a concurrent import that won the race after
/// our pre-check — to `was_duplicate = true` instead of erroring. Correctness
/// against the race comes from the in-txn check plus the
/// `idx_journal_entries_reference_unique` backstop; this keeps the duplicate
/// outcome graceful.
pub(crate) fn post_ingest_entry(
    commands: &mut EntryCommands,
    cmd: PostEntryCommand,
) -> Result<(String, bool), IngestError> {
    match commands.post_entry(cmd) {
        Ok(stored) => Ok((extract_entry_id(&stored), false)),
        Err(EntryCommandError::DuplicateReference {
            existing_entry_id, ..
        }) => Ok((existing_entry_id, true)),
        Err(e) => Err(IngestError::EntryError(e.to_string())),
    }
}

/// Write a planned entry to standalone books: skip it if its reference is
/// already there, otherwise post it.
///
/// The counterpart of [`plan_sale`] and friends, and the only thing standing
/// between a plan and the log. On hosted books this half is the group server's
/// job instead — same plan, different writer.
pub fn post_planned_entry(
    store: &mut EventStore,
    user_id: &str,
    cmd: PostEntryCommand,
) -> Result<IngestResult, IngestError> {
    if let Some(result) = already_posted(store.connection(), cmd.reference.as_deref()) {
        return Ok(result);
    }
    let mut commands = EntryCommands::new(store, user_id.to_string());
    let (entry_id, was_duplicate) = post_ingest_entry(&mut commands, cmd)?;
    Ok(IngestResult {
        entry_id,
        was_duplicate,
    })
}

/// Write a planned bill to standalone books. See [`post_planned_entry`].
///
/// A concurrent import for the same source event that won the race after the
/// pre-check is rejected in-txn as a duplicate; that maps to a graceful skip
/// rather than an error, because it means the work is done, not that it failed.
pub fn post_planned_bill(
    store: &mut EventStore,
    user_id: &str,
    cmd: ReceiveBillCommand,
) -> Result<IngestResult, IngestError> {
    if let Some(result) = already_posted(store.connection(), cmd.reference.as_deref()) {
        return Ok(result);
    }
    let mut bill_cmds = BillCommands::new(store, user_id.to_string());
    let (entry_id, was_duplicate) = match bill_cmds.receive_bill(cmd) {
        Ok(stored) => {
            let entry_id = if let crate::events::types::Event::BillReceived { entry_id, .. } =
                &stored.event
            {
                entry_id.clone()
            } else {
                String::new()
            };
            (entry_id, false)
        }
        Err(BillCommandError::DuplicateReference {
            existing_entry_id, ..
        }) => (existing_entry_id, true),
        Err(e) => return Err(IngestError::EntryError(e.to_string())),
    };
    Ok(IngestResult {
        entry_id,
        was_duplicate,
    })
}

// --- Planning ---
//
// Every ingest below is two separable halves: *decide* what to post — which
// accounts, which lines, which memo — and then *write* it. The deciding half
// needs only a read of the mappings and the vendor rules; the writing half needs
// a `&mut EventStore`, which group-hosted books do not have, because their event
// ids are the group server's to mint.
//
// So the deciding half lives in these `plan_*` functions, over a plain
// `&Connection`. A member on hosted books plans against their replica's
// projection and submits the result to the group server's command endpoints,
// which is the only route by which anything reaches those books.
//
// The `ingest_*` functions keep their signatures and call straight through, so
// there is exactly one description of what a sale posts to. The alternative —
// the desktop assembling entries of its own for hosted books — is two copies of
// the chart-of-accounts logic that would drift, and the symptom of that drift is
// two members' books disagreeing about the same sale.

/// The entry a sale posts, decided but not written.
pub fn plan_sale(
    conn: &Connection,
    data: IngestSaleData,
    source: JournalEntrySource,
) -> Result<PostEntryCommand, IngestError> {
    if data.items.is_empty() {
        return Err(IngestError::EmptyItems);
    }

    let date = parse_ingest_date(&data.date)?;
    let tax = data.tax_collected_cents.unwrap_or(0);

    let total_revenue: i64 = data.items.iter().map(|i| i.qty as i64 * i.unit_price_cents).sum();
    let total_cogs: i64 = data.items.iter().map(|i| i.qty as i64 * i.unit_cost_cents).sum();
    let amount_received = total_revenue + tax;

    // Split tender: one debit per payment method (or the legacy single method),
    // validated to sum to what was received.
    let tenders = resolve_tenders(&data.payments, data.payment_method, amount_received)?;

    let mut required: Vec<&str> = tenders.iter().map(|(k, _)| *k).collect();
    required.extend(["pos_revenue", "cogs", "inventory"]);
    if tax > 0 {
        required.push("sales_tax_payable");
    }

    let mappings = load_ingest_mappings(conn, &required)?;

    let items_desc: String = data
        .items
        .iter()
        .map(|i| format!("{}x {}", i.qty, i.name))
        .collect::<Vec<_>>()
        .join(", ");

    let memo = data.memo.unwrap_or_else(|| format!("POS Sale: {}", items_desc));

    let mut lines = vec![EntryLine::credit(&mappings["pos_revenue"], total_revenue, "USD")
        .with_memo("Sales revenue")];
    for (key, amount) in &tenders {
        lines.push(
            EntryLine::debit(&mappings[*key], *amount, "USD").with_memo("Payment received"),
        );
    }

    if tax > 0 {
        lines.push(
            EntryLine::credit(&mappings["sales_tax_payable"], tax, "USD")
                .with_memo("Sales tax collected"),
        );
    }

    if total_cogs > 0 {
        lines.push(
            EntryLine::debit(&mappings["cogs"], total_cogs, "USD")
                .with_memo("Cost of goods sold"),
        );
        lines.push(
            EntryLine::credit(&mappings["inventory"], total_cogs, "USD")
                .with_memo("Inventory reduction"),
        );
    }

    Ok(PostEntryCommand {
        date,
        memo,
        lines,
        reference: data.reference,
        source: Some(source),
    })
}

pub fn ingest_sale(
    store: &mut EventStore,
    user_id: &str,
    data: IngestSaleData,
    source: JournalEntrySource,
) -> Result<IngestResult, IngestError> {
    let cmd = plan_sale(store.connection(), data, source)?;
    post_planned_entry(store, user_id, cmd)
}

/// Ingest a refund / return — the reverse of [`ingest_sale`]. Revenue is booked
/// against the `refunds` contra-revenue account (so it subtracts from income),
/// the money flows back out of the payment account, sales tax is reversed, and —
/// when `restock` is set and the returned items carry a cost — inventory is
/// restocked and COGS reversed.
pub fn plan_refund(
    conn: &Connection,
    data: IngestRefundData,
    source: JournalEntrySource,
) -> Result<PostEntryCommand, IngestError> {
    if data.items.is_empty() {
        return Err(IngestError::EmptyItems);
    }

    let date = parse_ingest_date(&data.date)?;
    let tax = data.tax_refunded_cents.unwrap_or(0);

    let total_revenue: i64 = data.items.iter().map(|i| i.qty as i64 * i.unit_price_cents).sum();
    let total_cost: i64 = data.items.iter().map(|i| i.qty as i64 * i.unit_cost_cents).sum();
    let amount_returned = total_revenue + tax;
    // Only restock when asked *and* the items carry a cost — otherwise there's
    // nothing to move and no need for the inventory/cogs mappings.
    let restock = data.restock && total_cost > 0;

    // Split tender: the refund can be returned across several methods.
    let tenders = resolve_tenders(&data.payments, data.payment_method, amount_returned)?;

    let mut required: Vec<&str> = tenders.iter().map(|(k, _)| *k).collect();
    required.push("refunds");
    if tax > 0 {
        required.push("sales_tax_payable");
    }
    if restock {
        required.push("cogs");
        required.push("inventory");
    }

    let mappings = load_ingest_mappings(conn, &required)?;

    let items_desc: String = data
        .items
        .iter()
        .map(|i| format!("{}x {}", i.qty, i.name))
        .collect::<Vec<_>>()
        .join(", ");
    let memo = data.memo.unwrap_or_else(|| format!("POS Refund: {}", items_desc));

    let mut lines = vec![EntryLine::debit(&mappings["refunds"], total_revenue, "USD")
        .with_memo("Refund of sales revenue")];
    for (key, amount) in &tenders {
        lines.push(
            EntryLine::credit(&mappings[*key], *amount, "USD").with_memo("Refund issued"),
        );
    }

    if tax > 0 {
        lines.push(
            EntryLine::debit(&mappings["sales_tax_payable"], tax, "USD")
                .with_memo("Sales tax refunded"),
        );
    }

    if restock {
        lines.push(
            EntryLine::debit(&mappings["inventory"], total_cost, "USD")
                .with_memo("Inventory restocked"),
        );
        lines.push(
            EntryLine::credit(&mappings["cogs"], total_cost, "USD")
                .with_memo("Cost of goods sold reversed"),
        );
    }

    Ok(PostEntryCommand {
        date,
        memo,
        lines,
        reference: data.reference,
        source: Some(source),
    })
}

pub fn ingest_refund(
    store: &mut EventStore,
    user_id: &str,
    data: IngestRefundData,
    source: JournalEntrySource,
) -> Result<IngestResult, IngestError> {
    let cmd = plan_refund(store.connection(), data, source)?;
    post_planned_entry(store, user_id, cmd)
}

pub fn plan_purchase_order(
    conn: &Connection,
    data: IngestPurchaseOrderData,
    source: JournalEntrySource,
) -> Result<PostEntryCommand, IngestError> {
    if data.items.is_empty() {
        return Err(IngestError::EmptyItems);
    }

    let date = parse_ingest_date(&data.date)?;

    let payment = data.payment.unwrap_or(IngestPurchasePayment::OnCredit);
    let inventory_account = load_ingest_mappings(conn, &["inventory"])?["inventory"].clone();
    // On-credit purchases route to a vendor-specific payable if a rule matches
    // the supplier, else the generic accounts_payable mapping. Cash uses pos_cash.
    let credit_account = match payment {
        IngestPurchasePayment::Cash => {
            load_ingest_mappings(conn, &["pos_cash"])?["pos_cash"].clone()
        }
        IngestPurchasePayment::OnCredit => {
            let supplier = data.supplier.as_deref().unwrap_or("supplier");
            match crate::commands::vendor_rules::match_account(conn, supplier) {
                Some(id) => id,
                None => {
                    load_ingest_mappings(conn, &["accounts_payable"])?["accounts_payable"].clone()
                }
            }
        }
    };

    let total_cost: i64 = data.items.iter().map(|i| i.qty as i64 * i.unit_cost_cents).sum();

    let items_desc: String = data
        .items
        .iter()
        .map(|i| format!("{}x {}", i.qty, i.name))
        .collect::<Vec<_>>()
        .join(", ");

    let memo = data.memo.unwrap_or_else(|| {
        let supplier_str = data.supplier.as_deref().unwrap_or("supplier");
        format!("PO from {}: {}", supplier_str, items_desc)
    });

    let lines = vec![
        EntryLine::debit(&inventory_account, total_cost, "USD")
            .with_memo("Inventory received"),
        EntryLine::credit(&credit_account, total_cost, "USD")
            .with_memo(match payment {
                IngestPurchasePayment::Cash => "Cash payment",
                IngestPurchasePayment::OnCredit => "Accounts payable",
            }),
    ];

    Ok(PostEntryCommand {
        date,
        memo,
        lines,
        reference: data.reference,
        source: Some(source),
    })
}

pub fn ingest_purchase_order(
    store: &mut EventStore,
    user_id: &str,
    data: IngestPurchaseOrderData,
    source: JournalEntrySource,
) -> Result<IngestResult, IngestError> {
    let cmd = plan_purchase_order(store.connection(), data, source)?;
    post_planned_entry(store, user_id, cmd)
}

pub fn plan_inventory_adjustment(
    conn: &Connection,
    data: IngestInventoryAdjustmentData,
    source: JournalEntrySource,
) -> Result<PostEntryCommand, IngestError> {
    if data.items.is_empty() {
        return Err(IngestError::EmptyItems);
    }

    let date = parse_ingest_date(&data.date)?;

    let mappings = load_ingest_mappings(conn, &["inventory", "inventory_adjustment"])?;

    let net: i64 = data.items.iter().map(|i| i.qty_delta as i64 * i.unit_cost_cents).sum();

    if net == 0 {
        return Err(IngestError::ZeroAdjustment);
    }

    let items_desc: String = data
        .items
        .iter()
        .map(|i| {
            let reason = i.reason.as_deref().unwrap_or("adjustment");
            format!("{}x {} ({})", i.qty_delta, i.name, reason)
        })
        .collect::<Vec<_>>()
        .join(", ");

    let memo = data.memo.unwrap_or_else(|| format!("Inventory adjustment: {}", items_desc));

    let abs_net = net.unsigned_abs() as i64;

    let lines = if net < 0 {
        vec![
            EntryLine::debit(&mappings["inventory_adjustment"], abs_net, "USD")
                .with_memo("Inventory adjustment expense"),
            EntryLine::credit(&mappings["inventory"], abs_net, "USD")
                .with_memo("Inventory reduction"),
        ]
    } else {
        vec![
            EntryLine::debit(&mappings["inventory"], abs_net, "USD")
                .with_memo("Inventory increase"),
            EntryLine::credit(&mappings["inventory_adjustment"], abs_net, "USD")
                .with_memo("Inventory adjustment credit"),
        ]
    };

    Ok(PostEntryCommand {
        date,
        memo,
        lines,
        reference: data.reference,
        source: Some(source),
    })
}

pub fn ingest_inventory_adjustment(
    store: &mut EventStore,
    user_id: &str,
    data: IngestInventoryAdjustmentData,
    source: JournalEntrySource,
) -> Result<IngestResult, IngestError> {
    let cmd = plan_inventory_adjustment(store.connection(), data, source)?;
    post_planned_entry(store, user_id, cmd)
}

/// The bill a goods-received event raises, decided but not written.
///
/// A bill rather than a plain entry on purpose: goods arriving creates a debt to
/// the supplier that somebody has to pay, and the payables list is where that is
/// tracked. Posting the same two lines as a bare journal entry would balance the
/// books and lose the obligation.
pub fn plan_goods_received(
    conn: &Connection,
    data: IngestGoodsReceivedData,
) -> Result<ReceiveBillCommand, IngestError> {
    if data.items.is_empty() {
        return Err(IngestError::EmptyItems);
    }

    let date = parse_ingest_date(&data.date)?;

    // Inventory is always required; the payable account is resolved per-vendor
    // (see below), so the generic accounts_payable mapping is only needed as a
    // fallback.
    let inventory_account = load_ingest_mappings(conn, &["inventory"])?["inventory"].clone();

    let total_cost: i64 = data
        .items
        .iter()
        .map(|i| i.qty as i64 * i.unit_cost_cents)
        .sum();

    let supplier = data.supplier.as_deref().unwrap_or("supplier");

    // Route the payable leg to a vendor-specific account if a rule matches the
    // supplier name; otherwise fall back to the generic accounts_payable mapping.
    let ap_account_id = match crate::commands::vendor_rules::match_account(conn, supplier) {
        Some(id) => id,
        None => load_ingest_mappings(conn, &["accounts_payable"])?["accounts_payable"].clone(),
    };

    let items_desc: String = data
        .items
        .iter()
        .map(|i| format!("{}x {}", i.qty, i.name))
        .collect::<Vec<_>>()
        .join(", ");

    let memo = data
        .memo
        .clone()
        .unwrap_or_else(|| format!("Received from {}: {}", supplier, items_desc));

    let terms = data
        .payment_terms
        .as_deref()
        .map(PaymentTerms::parse)
        .unwrap_or(PaymentTerms::Net { days: 30 });

    Ok(ReceiveBillCommand {
        vendor: supplier.to_string(),
        amount: total_cost,
        currency: "USD".to_string(),
        issue_date: date,
        terms,
        memo: Some(memo),
        expense_account_id: inventory_account,
        ap_account_id,
        // Carry the source event's reference so a re-sync is idempotent.
        reference: data.reference.clone(),
    })
}

/// Ingest a goods received event: creates inventory journal entry + AP bill.
/// This is the proper procurement flow — goods arrive, inventory increases,
/// and a bill is created in the AP system for tracking payment.
pub fn ingest_goods_received(
    store: &mut EventStore,
    user_id: &str,
    data: IngestGoodsReceivedData,
) -> Result<IngestResult, IngestError> {
    if let Some(result) = already_posted(store.connection(), data.reference.as_deref()) {
        return Ok(result);
    }
    let cmd = plan_goods_received(store.connection(), data)?;

    // Create the bill via BillCommands (this creates the journal entry
    // internally). A concurrent goods-received for the same source event that
    // won the race after our pre-check is rejected in-txn as a duplicate; map
    // that to a graceful skip rather than an error.
    let mut bill_cmds = BillCommands::new(store, user_id.to_string());
    let (entry_id, was_duplicate) = match bill_cmds.receive_bill(cmd) {
        Ok(stored) => {
            let entry_id = if let crate::events::types::Event::BillReceived { entry_id, .. } =
                &stored.event
            {
                entry_id.clone()
            } else {
                String::new()
            };
            (entry_id, false)
        }
        Err(BillCommandError::DuplicateReference {
            existing_entry_id, ..
        }) => (existing_entry_id, true),
        Err(e) => return Err(IngestError::EntryError(e.to_string())),
    };

    Ok(IngestResult {
        entry_id,
        was_duplicate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{Event, EventAccountType, EventEnvelope};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::store::projections::ProjectionStore;
    use chrono::NaiveDate;

    fn mk_account(store: &mut EventStore, id: &str, ty: EventAccountType, num: &str, name: &str) {
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

    fn refund_data() -> IngestRefundData {
        IngestRefundData {
            date: "2026-07-05".to_string(),
            reference: Some("Bugbear pos:refund-1".to_string()),
            memo: None,
            items: vec![IngestSaleItem {
                name: "Bar tape".to_string(),
                qty: 1,
                unit_price_cents: 5000,
                unit_cost_cents: 2000,
            }],
            payments: vec![],
            payment_method: Some(IngestPaymentMethod::Square),
            tax_refunded_cents: Some(300),
            restock: true,
        }
    }

    /// Collect account_id -> signed (debit-positive) amount for an entry.
    fn entry_amounts(store: &EventStore, entry_id: &str) -> std::collections::HashMap<String, i64> {
        let mut amt = std::collections::HashMap::new();
        let conn = store.connection();
        let mut stmt = conn
            .prepare("SELECT account_id, amount FROM journal_lines WHERE entry_id = ?1")
            .unwrap();
        for row in stmt
            .query_map([entry_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .flatten()
        {
            *amt.entry(row.0).or_insert(0) += row.1;
        }
        amt
    }

    #[test]
    fn sale_splits_across_multiple_payment_methods() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();

        mk_account(&mut store, "cash", EventAccountType::Asset, "1000", "Cash");
        mk_account(&mut store, "stripe", EventAccountType::Asset, "1060", "Stripe");
        mk_account(&mut store, "square", EventAccountType::Asset, "1050", "Square");
        mk_account(&mut store, "rev", EventAccountType::Revenue, "4000", "Sales");
        mk_account(&mut store, "inv", EventAccountType::Asset, "1200", "Inventory");
        mk_account(&mut store, "cogs", EventAccountType::Expense, "5000", "COGS");
        {
            let conn = store.connection();
            set_account_mapping(conn, "pos_cash", "cash").unwrap();
            set_account_mapping(conn, "pos_stripe", "stripe").unwrap();
            set_account_mapping(conn, "pos_square", "square").unwrap();
            set_account_mapping(conn, "pos_revenue", "rev").unwrap();
            set_account_mapping(conn, "inventory", "inv").unwrap();
            set_account_mapping(conn, "cogs", "cogs").unwrap();
        }

        // A $100 sale (no tax): $30 Stripe deposit + $70 balance on Square.
        let data = IngestSaleData {
            date: "2026-07-05".to_string(),
            reference: Some("pos:sale-splt".to_string()),
            memo: None,
            items: vec![IngestSaleItem {
                name: "Wheelset".to_string(),
                qty: 1,
                unit_price_cents: 10000,
                unit_cost_cents: 6000,
            }],
            payments: vec![
                IngestPayment { method: IngestPaymentMethod::Stripe, amount_cents: 3000 },
                IngestPayment { method: IngestPaymentMethod::Square, amount_cents: 7000 },
            ],
            payment_method: None,
            tax_collected_cents: None,
        };
        let res = ingest_sale(&mut store, "test", data, JournalEntrySource::EventService).unwrap();
        let amt = entry_amounts(&store, &res.entry_id);

        assert_eq!(amt["stripe"], 3000, "stripe deposit debited");
        assert_eq!(amt["square"], 7000, "square balance debited");
        assert_eq!(amt.get("cash"), None, "cash not involved");
        assert_eq!(amt["rev"], -10000, "revenue credited in full");
        assert_eq!(amt["inv"], -6000);
        assert_eq!(amt["cogs"], 6000);
        assert_eq!(amt.values().sum::<i64>(), 0, "entry balances");

        // Tenders that don't add up to the sale are rejected.
        let bad = IngestSaleData {
            date: "2026-07-05".to_string(),
            reference: Some("pos:sale-bad".to_string()),
            memo: None,
            items: vec![IngestSaleItem {
                name: "Tube".to_string(),
                qty: 1,
                unit_price_cents: 1000,
                unit_cost_cents: 400,
            }],
            payments: vec![IngestPayment {
                method: IngestPaymentMethod::Cash,
                amount_cents: 900,
            }],
            payment_method: None,
            tax_collected_cents: None,
        };
        let err = ingest_sale(&mut store, "test", bad, JournalEntrySource::EventService);
        assert!(matches!(err, Err(IngestError::PaymentMismatch { .. })));
    }

    #[test]
    fn refund_posts_reverse_of_sale_with_restock() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();

        mk_account(&mut store, "refunds", EventAccountType::Revenue, "4900", "Refunds");
        mk_account(&mut store, "square", EventAccountType::Asset, "1050", "Square");
        mk_account(&mut store, "inv", EventAccountType::Asset, "1200", "Inventory");
        mk_account(&mut store, "cogs", EventAccountType::Expense, "5000", "COGS");
        mk_account(&mut store, "tax", EventAccountType::Liability, "2200", "Sales tax");
        {
            let conn = store.connection();
            set_account_mapping(conn, "refunds", "refunds").unwrap();
            set_account_mapping(conn, "pos_square", "square").unwrap();
            set_account_mapping(conn, "inventory", "inv").unwrap();
            set_account_mapping(conn, "cogs", "cogs").unwrap();
            set_account_mapping(conn, "sales_tax_payable", "tax").unwrap();
        }

        let res =
            ingest_refund(&mut store, "test", refund_data(), JournalEntrySource::EventService)
                .unwrap();
        assert!(!res.was_duplicate);

        // account_id -> signed (debit-positive) amount for the posted entry.
        let mut amt = std::collections::HashMap::new();
        {
            let conn = store.connection();
            let mut stmt = conn
                .prepare("SELECT account_id, amount FROM journal_lines WHERE entry_id = ?1")
                .unwrap();
            for row in stmt
                .query_map([&res.entry_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })
                .unwrap()
                .flatten()
            {
                *amt.entry(row.0).or_insert(0) += row.1;
            }
        }

        assert_eq!(amt["refunds"], 5000, "refunds debited by revenue");
        assert_eq!(amt["square"], -5300, "payment credited revenue+tax");
        assert_eq!(amt["tax"], 300, "sales tax reversed (debit)");
        assert_eq!(amt["inv"], 2000, "inventory restocked");
        assert_eq!(amt["cogs"], -2000, "COGS reversed");
        assert_eq!(amt.values().sum::<i64>(), 0, "entry must balance");

        // Same reference re-ingests as a no-op duplicate.
        let dup =
            ingest_refund(&mut store, "test", refund_data(), JournalEntrySource::EventService)
                .unwrap();
        assert!(dup.was_duplicate, "same reference must dedupe");
    }

    #[test]
    fn parse_ingest_date_accepts_plain_and_iso8601() {
        let expected = NaiveDate::from_ymd_opt(2026, 7, 3).unwrap();
        // Plain date
        assert_eq!(parse_ingest_date("2026-07-03").unwrap(), expected);
        // Full ISO-8601 / RFC-3339 with millis + Z (what the POS sends)
        assert_eq!(parse_ingest_date("2026-07-03T14:30:00.000Z").unwrap(), expected);
        // With an explicit offset
        assert_eq!(parse_ingest_date("2026-07-03T09:30:00-05:00").unwrap(), expected);
        // Datetime without offset (leading-10 fallback)
        assert_eq!(parse_ingest_date("2026-07-03T14:30:00").unwrap(), expected);
        // Whitespace tolerated
        assert_eq!(parse_ingest_date("  2026-07-03  ").unwrap(), expected);
        // Genuinely invalid still errors
        assert!(parse_ingest_date("not-a-date").is_err());
        assert!(parse_ingest_date("").is_err());
    }
}
