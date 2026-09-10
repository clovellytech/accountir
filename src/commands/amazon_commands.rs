//! Amazon Business order-history CSV ingest: turns the "Order History Report"
//! you download from Amazon Business (Business Analytics → Reports) into
//! balanced double-entry journal entries that clear the Amazon liability
//! account your credit-card feed already posts to.
//!
//! Acquisition is manual today (download the report, run `amazon orders <file>`)
//! and will move to the browser extension later — same record/replay + CSV
//! interception machinery as bank and Square imports. The parser here is
//! content-driven (per-row dates), so it doesn't care how the file arrived.
//!
//! ## Report shape
//!
//! The report is **line-item level**: one row per item, with the order- and
//! payment-level columns repeated across every row of the same order. We read
//! columns *by name* (not position) because the report carries ~70 columns and
//! their order is not guaranteed.
//!
//! Three real-world quirks this handles:
//!   - `Payment Identifier` is Excel-escaped as `="…1234"` to stop spreadsheets
//!     mangling it. We strip the `="…"` wrapper and keep the last 4 digits.
//!   - **An order's item rows are repeated once per payment.** `Payment
//!     Reference ID` is Amazon's identifier for one settlement, and every item
//!     row of the shipment it paid for is listed again under it. So an order
//!     split across a card and a gift card carries its whole item list twice.
//!     Booking the rows as they come counts the purchase once per payment
//!     method — see [`charges_for_order`] for how they are folded back.
//!   - **Amazon restates a payment under a new reference id.** The same
//!     settlement comes back a day later with a different `Payment Reference
//!     ID`, as an extra charge that never happened. `Order Net Total` is what
//!     adjudicates: a restatement is dropped only when dropping it makes the
//!     order foot to Amazon's own total for it.
//!
//! Line items still don't always foot to what was paid (returns, partial
//! shipments), and where they don't the payments are authoritative — that is
//! what hit the card — so the difference goes to a reconciling line for review
//! rather than silently unbalancing the entry.
//!
//! ## Accounting model
//!
//! Your credit-card feed already posts each Amazon charge to a clearing/liability
//! account (the `amazon_clearing` mapped account). This import categorizes that
//! charge by booking the purchase detail against the same account, so the two
//! sides net to zero once matched:
//!
//! ```text
//!   Dr  Uncategorized expense   item net total   (one line per item; memo = Title)
//!   Dr  Uncategorized expense   reconciling diff (only if items != payments)
//!     Cr  Amazon clearing (amazon_clearing)   payment amount   (one per settlement)
//! ```
//!
//! Each item lands in **Uncategorized** so you can reassign it to the right
//! expense account in the app — that's the "categorize" step. One entry is
//! posted per *shipment*, carrying a clearing credit per settlement that funded
//! it, so the clearing account still matches the card feed charge for charge.
//! Idempotent on `amazon-<order>-<paydate>-<amount>-<last4>` of its largest
//! payment — the format the books already carry.

use crate::commands::account_commands::find_or_create_uncategorized;
use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use crate::commands::import_commands::{parse_amount, parse_date, parse_delimited_line};
use crate::commands::ingest_commands::{
    load_all_mappings, load_ingest_mappings, post_ingest_entry, IngestError,
};
use crate::events::types::JournalEntrySource;
use crate::store::event_store::EventStore;
use chrono::NaiveDate;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// The ingest mapping key for the Amazon clearing/liability account that the
/// card feed posts charges to and this import clears.
pub const AMAZON_CLEARING_KEY: &str = "amazon_clearing";

/// Column names we read from the Order History Report (case-insensitive).
mod columns {
    pub const ORDER_DATE: &str = "order date";
    pub const ORDER_ID: &str = "order id";
    pub const ORDER_STATUS: &str = "order status";
    pub const ORDER_NET_TOTAL: &str = "order net total";
    pub const PAYMENT_REFERENCE_ID: &str = "payment reference id";
    pub const PAYMENT_DATE: &str = "payment date";
    pub const PAYMENT_AMOUNT: &str = "payment amount";
    pub const PAYMENT_INSTRUMENT_TYPE: &str = "payment instrument type";
    pub const PAYMENT_IDENTIFIER: &str = "payment identifier";
    pub const ITEM_NET_TOTAL: &str = "item net total";
    pub const TITLE: &str = "title";
}

/// Outcome of an import.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AmazonImportSummary {
    pub entries_posted: usize,
    pub skipped_duplicates: usize,
    /// Charges (shipments) seen in the file — one per entry the import would
    /// post, not one per settlement.
    pub charges_seen: usize,
    /// Orders skipped because they were cancelled (never charged).
    pub cancelled_orders: usize,
    /// Orders skipped because they were still pending (not yet settled).
    pub pending_orders: usize,
    /// Charges whose line items didn't foot to what was paid (got a
    /// reconciling line — worth a human look).
    pub reconciled_charges: usize,
    /// Payment references dropped as Amazon restating a settlement it had
    /// already reported. Never a guess — see [`drop_restated_payments`].
    pub restated_payments: usize,
}

/// One settlement against an order.
///
/// `Payment Reference ID` is Amazon's own identity for it, and the unit the card
/// feed will show up as, so it is what the report is grouped by. Older exports
/// predate the column; those fall back to the date/amount/card triple that this
/// importer used to group on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Payment {
    /// Grouping key: the reference id, or the synthesized fallback.
    key: String,
    date: NaiveDate,
    /// What hit the card, in cents.
    amount: i64,
    card_type: String,
    card_last4: String,
}

/// One shipment: the items bought, and every settlement that funded them.
///
/// This is the unit an entry is posted for. It is deliberately *not* the
/// settlement: an order paid part on a card and part on a gift card lists its
/// whole item set under each, and one entry per settlement books the purchase
/// twice.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Charge {
    order_id: String,
    date: NaiveDate,
    /// Every settlement funding these items, in the order the report listed them.
    payments: Vec<Payment>,
    /// (item title, item net total in cents)
    items: Vec<(String, i64)>,
}

impl Charge {
    fn items_total(&self) -> i64 {
        self.items.iter().map(|(_, a)| a).sum()
    }

    fn payments_total(&self) -> i64 {
        self.payments.iter().map(|p| p.amount).sum()
    }

    /// The settlement the idempotency key is built from: the largest, ties
    /// broken by the earliest date then the key, so it is stable across exports.
    ///
    /// Largest rather than first because a single-payment charge — which is
    /// almost all of them — then keeps the exact reference the books already
    /// carry. Changing that format would orphan 800 posted entries and re-import
    /// the lot.
    fn primary(&self) -> &Payment {
        self.payments
            .iter()
            .max_by(|a, b| {
                a.amount
                    .cmp(&b.amount)
                    .then_with(|| b.date.cmp(&a.date))
                    .then_with(|| b.key.cmp(&a.key))
            })
            .expect("a charge always has at least one payment")
    }
}

/// Result of parsing the report content, before anything touches the store.
#[derive(Debug, Default)]
struct AmazonParse {
    charges: Vec<Charge>,
    cancelled_orders: usize,
    pending_orders: usize,
    restated_payments: usize,
}

/// Strip Excel's `="…"` text-escaping wrapper from a cell.
fn clean_cell(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('=').unwrap_or(s);
    s.trim_matches('"').trim().to_string()
}

/// Keep the last 4 digits of a (possibly masked/escaped) payment identifier.
fn last4(s: &str) -> String {
    let digits: String = clean_cell(s)
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let n = digits.len();
    if n > 4 {
        digits[n - 4..].to_string()
    } else {
        digits
    }
}

/// Truncate a memo to a sane length (item titles can be very long).
fn memo_of(title: &str) -> String {
    let t = title.trim();
    if t.chars().count() > 180 {
        let cut: String = t.chars().take(179).collect();
        format!("{}…", cut)
    } else {
        t.to_string()
    }
}

/// Everything one order contributed to the report, before it becomes entries.
#[derive(Debug, Default)]
struct OrderRows {
    /// `Order Net Total` — Amazon's own statement of what the order cost. The
    /// tiebreaker when the payment rows disagree with themselves.
    net_total: Option<i64>,
    /// Payment keys in first-seen order, so the output does not depend on hash
    /// iteration order.
    keys: Vec<String>,
    /// key -> (the settlement, the item rows listed under it).
    by_key: HashMap<String, (Payment, Vec<(String, i64)>)>,
}

/// Drop payment references that are Amazon restating a settlement it already
/// reported, and say how many were dropped.
///
/// The same settlement comes back in a later export under a new `Payment
/// Reference ID`, usually a day later, and looks exactly like a second charge.
/// Guessing which is which would be a good way to delete a real double-charge,
/// so this does not guess: it collapses references that agree on amount, card
/// type and card number, and keeps the collapsed set **only if that makes the
/// order foot to `Order Net Total`**. Two of Amazon's own numbers agreeing is
/// the evidence; without it the payments are left alone and the caller's
/// reconciling line flags the order for a human.
///
/// This is what protects the opposite case: an order genuinely charged twice for
/// the same amount on the same card (two of one item, shipped separately) foots
/// to the net total *with* both, so collapsing would break it and is refused.
fn drop_restated_payments(order: &mut OrderRows) -> usize {
    let Some(net_total) = order.net_total else {
        return 0;
    };
    let total: i64 = order
        .keys
        .iter()
        .filter_map(|k| order.by_key.get(k))
        .map(|(p, _)| p.amount)
        .sum();
    if total == net_total {
        return 0;
    }

    let mut seen: HashSet<(i64, String, String)> = HashSet::new();
    let mut kept: Vec<String> = Vec::new();
    for key in &order.keys {
        let Some((payment, _)) = order.by_key.get(key) else {
            continue;
        };
        let signature = (
            payment.amount,
            payment.card_type.clone(),
            payment.card_last4.clone(),
        );
        if seen.insert(signature) {
            kept.push(key.clone());
        }
    }

    let kept_total: i64 = kept
        .iter()
        .filter_map(|k| order.by_key.get(k))
        .map(|(p, _)| p.amount)
        .sum();
    if kept_total != net_total {
        return 0;
    }

    let dropped = order.keys.len() - kept.len();
    order.by_key.retain(|k, _| kept.contains(k));
    order.keys = kept;
    dropped
}

/// Fold one order's settlements into the shipments they paid for.
///
/// A settlement whose item rows foot to its own amount paid for exactly what it
/// lists, and stands alone. One that does not is funding something bigger than
/// itself — the other half of a split tender — so it is pooled with the other
/// short settlements listing the identical item set, and that pool books its
/// items **once** with a clearing credit per settlement.
///
/// Footing is what separates the two cases, and it has to be, because they look
/// the same otherwise: two settlements of \$42.38 on one card can be one item
/// bought twice (each foots, two charges) or a \$84.76 shipment paid half and
/// half (neither foots, one charge).
fn charges_for_order(order_id: &str, order: OrderRows) -> Vec<Charge> {
    let mut charges: Vec<Charge> = Vec::new();
    // Pooled short settlements, keyed by their item set, in first-seen order.
    let mut pool_keys: Vec<String> = Vec::new();
    let mut pools: HashMap<String, Charge> = HashMap::new();

    for key in &order.keys {
        let Some((payment, items)) = order.by_key.get(key) else {
            continue;
        };
        let items_sum: i64 = items.iter().map(|(_, a)| a).sum();
        if items_sum == payment.amount {
            charges.push(Charge {
                order_id: order_id.to_string(),
                date: payment.date,
                payments: vec![payment.clone()],
                items: items.clone(),
            });
            continue;
        }
        let signature = item_signature(items);
        match pools.get_mut(&signature) {
            Some(charge) => {
                charge.date = charge.date.min(payment.date);
                charge.payments.push(payment.clone());
            }
            None => {
                pool_keys.push(signature.clone());
                pools.insert(
                    signature,
                    Charge {
                        order_id: order_id.to_string(),
                        date: payment.date,
                        payments: vec![payment.clone()],
                        items: items.clone(),
                    },
                );
            }
        }
    }

    for signature in pool_keys {
        if let Some(charge) = pools.remove(&signature) {
            charges.push(charge);
        }
    }
    charges
}

/// A stable key for an item set, so the settlements funding one shipment find
/// each other. Sorted, because the report does not promise a row order.
fn item_signature(items: &[(String, i64)]) -> String {
    // Control characters as separators: an item title can contain anything a
    // seller typed, and a separator a title could contain would let two
    // different item sets collide into one shipment.
    let mut parts: Vec<String> = items
        .iter()
        .map(|(title, amount)| format!("{}\u{1f}{}", title, amount))
        .collect();
    parts.sort();
    parts.join("\u{1e}")
}

/// Parse the report into charges, skipping cancelled and pending orders.
/// Pure (no store access) so it can be unit-tested directly.
fn parse_amazon_orders(content: &str) -> AmazonParse {
    // Drop a leading UTF-8 BOM so the first header name matches cleanly.
    let content = content.trim_start_matches('\u{feff}');

    let mut lines = content.lines();
    let header = match lines.next() {
        Some(h) => parse_delimited_line(h, ','),
        None => return AmazonParse::default(),
    };
    let index: HashMap<String, usize> = header
        .iter()
        .enumerate()
        .map(|(i, name)| (name.trim().to_lowercase(), i))
        .collect();

    let get = |fields: &[String], name: &str| -> String {
        index
            .get(name)
            .and_then(|&i| fields.get(i))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    let mut order_ids: Vec<String> = Vec::new();
    let mut orders: HashMap<String, OrderRows> = HashMap::new();
    let mut cancelled: HashSet<String> = HashSet::new();
    let mut pending: HashSet<String> = HashSet::new();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields = parse_delimited_line(line, ',');

        let order_id = get(&fields, columns::ORDER_ID);
        if order_id.is_empty() {
            continue;
        }

        let status = get(&fields, columns::ORDER_STATUS).to_lowercase();
        if status == "cancelled" || status == "canceled" {
            cancelled.insert(order_id);
            continue;
        }
        if status == "pending" {
            pending.insert(order_id);
            continue;
        }

        // A charge needs a settled payment amount. No amount → not yet charged.
        let payment_amount = match parse_amount(&get(&fields, columns::PAYMENT_AMOUNT)) {
            Some(a) if a != 0 => a,
            _ => continue,
        };

        let payment_date_raw = get(&fields, columns::PAYMENT_DATE);
        let date = match parse_date(&payment_date_raw)
            .or_else(|| parse_date(&get(&fields, columns::ORDER_DATE)))
        {
            Some(d) => d,
            None => continue,
        };

        let card_type = get(&fields, columns::PAYMENT_INSTRUMENT_TYPE);
        let card_last4 = last4(&get(&fields, columns::PAYMENT_IDENTIFIER));
        let title = get(&fields, columns::TITLE);
        let item_total = parse_amount(&get(&fields, columns::ITEM_NET_TOTAL)).unwrap_or(0);

        // Amazon's own id for this settlement. Exports that predate the column
        // fall back to the key this importer used to group on, so an old file
        // still parses the way it always did.
        let reference_id = clean_cell(&get(&fields, columns::PAYMENT_REFERENCE_ID));
        let key = if reference_id.is_empty() {
            format!("{}|{}|{}", payment_date_raw, payment_amount, card_last4)
        } else {
            reference_id
        };

        let entry = orders.entry(order_id.clone()).or_insert_with(|| {
            order_ids.push(order_id.clone());
            OrderRows::default()
        });
        entry.net_total = entry
            .net_total
            .or_else(|| parse_amount(&get(&fields, columns::ORDER_NET_TOTAL)));
        let slot = entry.by_key.entry(key.clone()).or_insert_with(|| {
            entry.keys.push(key.clone());
            (
                Payment {
                    key: key.clone(),
                    date,
                    amount: payment_amount,
                    card_type: card_type.clone(),
                    card_last4: card_last4.clone(),
                },
                Vec::new(),
            )
        });
        if item_total != 0 || !title.is_empty() {
            slot.1.push((memo_of(&title), item_total));
        }
    }

    let mut charges: Vec<Charge> = Vec::new();
    let mut restated_payments = 0;
    for order_id in &order_ids {
        let Some(mut order) = orders.remove(order_id) else {
            continue;
        };
        restated_payments += drop_restated_payments(&mut order);
        charges.extend(charges_for_order(order_id, order));
    }

    AmazonParse {
        charges,
        cancelled_orders: cancelled.len(),
        pending_orders: pending.len(),
        restated_payments,
    }
}

/// Ingest an Amazon Business Order History Report CSV: one balanced journal
/// entry per card charge, clearing the `amazon_clearing` account. Idempotent —
/// re-importing the same file skips charges already posted.
/// Decide what an Amazon order history posts, without writing anything.
///
/// The deciding half of [`ingest_amazon_orders`], over a plain `&Connection` so a
/// member on group-hosted books can run it against their replica and submit the
/// result — a replica may not append, its event ids belonging to the server.
///
/// Distinct from [`plan_amazon_orders`], which answers "what would this do?" for
/// the preview panel in the shape the UI wants. This one produces the entries
/// themselves, and the two are deliberately separate: the preview is allowed to
/// be approximate about a missing mapping, while this must refuse.
pub fn plan_amazon_entries(
    conn: &Connection,
    content: &str,
) -> Result<(Vec<PostEntryCommand>, AmazonImportSummary), IngestError> {
    let parsed = parse_amazon_orders(content);

    let mut summary = AmazonImportSummary {
        charges_seen: parsed.charges.len(),
        cancelled_orders: parsed.cancelled_orders,
        pending_orders: parsed.pending_orders,
        restated_payments: parsed.restated_payments,
        ..Default::default()
    };

    if parsed.charges.is_empty() {
        return Ok((Vec::new(), summary));
    }

    // Pass 1 (immutable store borrow): drop charges already imported.
    let references = assign_references(&parsed.charges);
    let mut to_post: Vec<(Charge, String)> = Vec::new();
    for (charge, reference) in parsed.charges.into_iter().zip(references) {
        if charge_already_imported(conn, &charge) {
            summary.skipped_duplicates += 1;
        } else {
            to_post.push((charge, reference));
        }
    }
    if to_post.is_empty() {
        return Ok((Vec::new(), summary));
    }

    // Both accounts must already exist. On hosted books nothing here may create
    // one, and on a standalone ledger `ingest_amazon_orders` has already made the
    // parking account before calling in.
    let uncategorized_id =
        crate::commands::account_commands::find_uncategorized(conn).ok_or_else(|| {
            IngestError::EntryError(
                crate::commands::account_commands::missing_uncategorized_refusal(),
            )
        })?;
    let mappings = load_ingest_mappings(conn, &[AMAZON_CLEARING_KEY])?;
    let clearing_id = mappings[AMAZON_CLEARING_KEY].clone();

    // Pass 2: build.
    let mut entries = Vec::new();
    for (charge, reference) in to_post {
        let mut lines: Vec<EntryLine> = Vec::new();
        let mut items_sum = 0i64;
        for (title, amount) in &charge.items {
            if *amount == 0 {
                continue;
            }
            items_sum += amount;
            lines.push(
                EntryLine::debit(&uncategorized_id, *amount, "USD").with_memo(&memo_of(title)),
            );
        }

        // What was paid is authoritative; book any shortfall/overage so the
        // entry balances and the discrepancy is visible.
        let discrepancy = charge.payments_total() - items_sum;
        if discrepancy != 0 {
            lines.push(EntryLine {
                account_id: uncategorized_id.clone(),
                amount: discrepancy, // positive = debit, negative = credit
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: Some(
                    "Amazon line-item vs payment reconciling difference — review".to_string(),
                ),
            });
            summary.reconciled_charges += 1;
        }

        // Clear each parked card charge separately. One line per settlement,
        // not one for their total: the card feed posts them one at a time, and a
        // combined credit would have nothing to reconcile against.
        for payment in &charge.payments {
            lines.push(
                EntryLine::credit(&clearing_id, payment.amount, "USD").with_memo(&format!(
                    "Amazon order {} ({})",
                    charge.order_id,
                    card_label(payment)
                )),
            );
        }

        entries.push(PostEntryCommand {
            date: charge.date,
            memo: format!("Amazon order {}", charge.order_id),
            lines,
            reference: Some(reference),
            source: Some(JournalEntrySource::Import),
        });
    }

    Ok((entries, summary))
}

pub fn ingest_amazon_orders(
    store: &mut EventStore,
    user_id: &str,
    content: &str,
) -> Result<AmazonImportSummary, IngestError> {
    // A standalone ledger may mint the parking account; a replica may not, which
    // is why the planner only looks for it.
    find_or_create_uncategorized(store).map_err(|e| IngestError::EntryError(e.to_string()))?;
    let (entries, mut summary) = plan_amazon_entries(store.connection(), content)?;

    let mut commands = EntryCommands::new(store, user_id.to_string());
    for cmd in entries {
        // A concurrent import that won the race after our pre-check is rejected
        // in-txn as a duplicate; count it as skipped rather than erroring.
        let (_, was_duplicate) = post_ingest_entry(&mut commands, cmd)?;
        if was_duplicate {
            summary.skipped_duplicates += 1;
        } else {
            summary.entries_posted += 1;
        }
    }

    Ok(summary)
}

/// Idempotency key for a charge — one ledger entry per shipment.
///
/// Built from the charge's largest settlement, which for the single-settlement
/// charges that are almost all of them is the only one, so the key is byte for
/// byte the one the books already carry.
fn charge_reference(c: &Charge) -> String {
    let p = c.primary();
    format!(
        "amazon-{}-{}-{}-{}",
        c.order_id,
        p.date.format("%Y%m%d"),
        p.amount,
        p.card_last4
    )
}

/// Assign every charge its ledger reference, disambiguating the rare collision.
///
/// The format stays the legacy one — [`charge_reference`] — because eight
/// hundred posted entries are keyed by it. It is not quite unique, though: one
/// order can settle twice for the same amount on the same card on the same day
/// (one item bought twice and shipped separately), and the unique index on
/// `reference` would reject the second entry. Only those collisions get Amazon's
/// own settlement id appended, so every other entry keeps the key the books
/// already know it by.
fn assign_references(charges: &[Charge]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    charges
        .iter()
        .map(|c| {
            let base = charge_reference(c);
            if seen.insert(base.clone()) {
                base
            } else {
                format!("{}#{}", base, c.primary().key)
            }
        })
        .collect()
}

/// Whether this charge is already in the ledger, ignoring the payment date.
///
/// # Why the date is written but not matched on
///
/// Amazon moves a charge's payment date between exports — a settlement that read
/// as the 5th on Monday reads as the 6th a week later. The date is part of
/// [`charge_reference`], so the same charge came back under a new key and posted
/// a second time; nine orders on this ledger were imported twice that way, and
/// the reference index could not see it because the two references genuinely
/// differed.
///
/// The date stays in the written reference: every entry already in the books
/// carries it, and changing the format would orphan all of them and re-import
/// the lot. So the format is kept and the *lookup* drops the date, which fixes
/// re-imports going forward and stays compatible with everything already posted.
///
/// The obvious worry is a collision — one order settling twice for the same
/// amount on the same card, which this would wrongly treat as a duplicate.
/// Amazon's own per-charge `Payment Reference ID` is 1:1 with the charges in
/// this export (209 for 209), and across it no order has two charges differing
/// only by date. A genuine repeat would need the same order, amount and card
/// twice, which is what a re-export looks like anyway.
///
/// # Why *any* settlement counts
///
/// A charge can carry several settlements, and the books hold entries from
/// before this importer grouped them — one per settlement, under that
/// settlement's own reference. So a shipment already posted the old way is
/// recognised by any of its payments matching, not just the one the new
/// reference is built from. Re-importing an old file must not post the same
/// purchase a second time in a new shape.
fn charge_already_imported(conn: &Connection, c: &Charge) -> bool {
    c.payments.iter().any(|p| {
        let pattern = format!("amazon-{}-%-{}-{}", c.order_id, p.amount, p.card_last4);
        conn.query_row(
            "SELECT 1 FROM journal_entries WHERE reference LIKE ?1 AND is_void = 0 LIMIT 1",
            [&pattern],
            |_| Ok(true),
        )
        .unwrap_or(false)
    })
}

/// Human-readable card label, e.g. "Mastercard ••1234".
fn card_label(p: &Payment) -> String {
    if p.card_last4.is_empty() {
        p.card_type.clone()
    } else {
        format!("{} ••{}", p.card_type, p.card_last4)
    }
}

/// Every card a charge was settled on, e.g. "Visa ••1174 + Gift Certificate/Card".
fn charge_card_label(c: &Charge) -> String {
    let mut seen: Vec<String> = Vec::new();
    for p in &c.payments {
        let label = card_label(p);
        if !seen.contains(&label) {
            seen.push(label);
        }
    }
    seen.join(" + ")
}

// ===========================================================================
// Preview / dry-run
// ===========================================================================

/// One charge as it would be imported — for the preview panel.
#[derive(Debug, Clone)]
pub struct PlannedCharge {
    pub order_id: String,
    pub date: NaiveDate,
    /// What will be credited to the clearing account, in cents.
    pub amount_cents: i64,
    pub item_count: usize,
    /// e.g. "Mastercard ••1234", or "Visa ••1174 + Gift Certificate/Card" when
    /// one shipment was settled across several.
    pub card: String,
    /// How many settlements funded it — more than one is a split tender.
    pub payment_count: usize,
    /// Already in the ledger (same reference) — will be skipped.
    pub already_imported: bool,
    /// payments - sum(items); nonzero means a reconciling line will be added.
    pub reconciling_diff_cents: i64,
}

/// A non-mutating preview of what an Amazon import will do. Lets the UI show the
/// exact effect — entries, dollar total, what's skipped — before committing.
#[derive(Debug, Default)]
pub struct AmazonPlan {
    pub charges: Vec<PlannedCharge>,
    pub cancelled_orders: usize,
    pub pending_orders: usize,
    /// Settlements Amazon restated under a new reference id, dropped because
    /// dropping them made the order foot to its own net total.
    pub restated_payments: usize,
    /// Resolved Amazon clearing account id, or None if the mapping isn't set yet.
    pub clearing_account_id: Option<String>,
}

impl AmazonPlan {
    /// Charges that will actually post (not already imported).
    pub fn new_charges(&self) -> usize {
        self.charges.iter().filter(|c| !c.already_imported).count()
    }
    /// Charges that will be skipped because they're already in the ledger.
    pub fn duplicate_charges(&self) -> usize {
        self.charges.iter().filter(|c| c.already_imported).count()
    }
    /// New charges whose items don't foot to the payment (get a reconciling line).
    pub fn reconciling_charges(&self) -> usize {
        self.charges
            .iter()
            .filter(|c| !c.already_imported && c.reconciling_diff_cents != 0)
            .count()
    }
    /// Total credited to the clearing account by this import (new charges), cents.
    pub fn total_to_post_cents(&self) -> i64 {
        self.charges
            .iter()
            .filter(|c| !c.already_imported)
            .map(|c| c.amount_cents)
            .sum()
    }
}

/// Build a non-mutating preview of an Amazon order import: what would post, the
/// dollar total, what would be skipped (already imported / cancelled / pending),
/// and whether the clearing mapping is set. Does not touch the ledger.
pub fn plan_amazon_orders(store: &EventStore, content: &str) -> AmazonPlan {
    let parsed = parse_amazon_orders(content);
    let conn = store.connection();
    let clearing_account_id = load_all_mappings(conn).get(AMAZON_CLEARING_KEY).cloned();

    let charges = parsed
        .charges
        .iter()
        .map(|c| {
            let already_imported = charge_already_imported(conn, c);
            PlannedCharge {
                order_id: c.order_id.clone(),
                date: c.date,
                amount_cents: c.payments_total(),
                item_count: c.items.len(),
                card: charge_card_label(c),
                payment_count: c.payments.len(),
                already_imported,
                reconciling_diff_cents: c.payments_total() - c.items_total(),
            }
        })
        .collect();

    AmazonPlan {
        charges,
        cancelled_orders: parsed.cancelled_orders,
        pending_orders: parsed.pending_orders,
        restated_payments: parsed.restated_payments,
        clearing_account_id,
    }
}

#[cfg(test)]
mod reimport_tests {
    use super::*;
    use crate::store::migrations::init_schema;

    fn charge(order: &str, date: (i32, u32, u32), cents: i64, card: &str) -> Charge {
        let date = chrono::NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap();
        Charge {
            order_id: order.to_string(),
            date,
            payments: vec![Payment {
                key: format!("{}-{}-{}", order, cents, card),
                date,
                amount: cents,
                card_type: "Visa".to_string(),
                card_last4: card.to_string(),
            }],
            items: Vec::new(),
        }
    }

    /// The reported defect: Amazon moved a charge's payment date between
    /// exports, the date is part of the reference, and the same charge posted a
    /// second time. Nine orders on the real ledger arrived twice this way.
    #[test]
    fn a_charge_whose_payment_date_moved_is_recognised_as_already_imported() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let first = charge("114-2102713-7939431", (2026, 4, 5), 806, "1174");
        conn.execute(
            "INSERT INTO journal_entries (id, date, memo, reference, is_void) \
             VALUES ('e1', '2026-04-05', 'Amazon', ?1, 0)",
            [charge_reference(&first)],
        )
        .unwrap();
        assert!(
            charge_already_imported(&conn, &first),
            "the same export again"
        );

        // The same charge, one day later — a different reference entirely.
        let moved = charge("114-2102713-7939431", (2026, 4, 6), 806, "1174");
        assert_ne!(charge_reference(&first), charge_reference(&moved));
        assert!(
            charge_already_imported(&conn, &moved),
            "a shifted payment date is the same charge, not a new one"
        );
    }

    /// The date is the only thing ignored. A different order, amount or card is
    /// a different charge, and treating one as a duplicate would silently drop
    /// a real purchase.
    #[test]
    fn only_the_date_is_ignored_when_matching() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let base = charge("114-2102713-7939431", (2026, 4, 5), 806, "1174");
        conn.execute(
            "INSERT INTO journal_entries (id, date, memo, reference, is_void) \
             VALUES ('e1', '2026-04-05', 'Amazon', ?1, 0)",
            [charge_reference(&base)],
        )
        .unwrap();
        for other in [
            charge("114-0000000-0000000", (2026, 4, 5), 806, "1174"),
            charge("114-2102713-7939431", (2026, 4, 5), 807, "1174"),
            charge("114-2102713-7939431", (2026, 4, 5), 806, "1007"),
        ] {
            assert!(
                !charge_already_imported(&conn, &other),
                "{:?}",
                charge_reference(&other)
            );
        }
    }

    /// Two real charges that differ only by Amazon's settlement id would write
    /// the same reference twice, and the unique index would reject the second —
    /// the whole shipment silently missing from the books.
    #[test]
    fn charges_that_collide_on_the_legacy_key_still_get_distinct_references() {
        let parsed = super::parse_amazon_orders(super::tests::SAMPLE);
        let refs = assign_references(&parsed.charges);
        let mut sorted = refs.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            before,
            "two charges share a reference: {refs:?}"
        );

        // …and the first of a colliding pair keeps the bare legacy key, so the
        // entry already in the books is still recognised by it.
        let ggg: Vec<&String> = refs.iter().filter(|r| r.contains("111-GGG")).collect();
        assert_eq!(ggg.len(), 2);
        assert!(ggg.iter().any(|r| !r.contains('#')), "{ggg:?}");
        assert!(ggg.iter().any(|r| r.contains('#')), "{ggg:?}");
    }

    /// A voided entry does not block a re-import: voiding is how a bad import is
    /// undone, and the charge has to be able to come back.
    #[test]
    fn a_voided_entry_does_not_count_as_imported() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let c = charge("114-2102713-7939431", (2026, 4, 5), 806, "1174");
        conn.execute(
            "INSERT INTO journal_entries (id, date, memo, reference, is_void) \
             VALUES ('e1', '2026-04-05', 'Amazon', ?1, 1)",
            [charge_reference(&c)],
        )
        .unwrap();
        assert!(!charge_already_imported(&conn, &c));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal report carrying only the columns the parser reads, exercising:
    // a clean 2-item order, a cancelled order (skip), a pending order (skip),
    // a split order (two charges on the same order id), an Excel-escaped card
    // identifier, and an order whose items don't foot to the payment total.
    pub(super) const SAMPLE: &str = "\u{feff}Order Date,Order ID,Order Status,Order Net Total,Payment Reference ID,Payment Date,Payment Amount,Payment Instrument Type,Payment Identifier,Item Net Total,Title\n\
06/25/2026,111-AAA,Closed,$30.00,REF-AAA,06/26/2026,$30.00,Mastercard,\"=\"\"1111\"\"\",$10.00,Widget A\n\
06/25/2026,111-AAA,Closed,$30.00,REF-AAA,06/26/2026,$30.00,Mastercard,\"=\"\"1111\"\"\",$20.00,Widget B\n\
06/01/2026,111-BBB,Cancelled,$0.00,,,,N/A,,$0.00,Cancelled thing\n\
06/02/2026,111-CCC,Pending,$5.00,REF-CCC,06/29/2026,$5.00,Visa,\"=\"\"2222\"\"\",$5.00,Pending thing\n\
06/10/2026,111-DDD,Closed,$40.00,REF-DDD-1,06/11/2026,$15.00,Visa,\"=\"\"3333\"\"\",$15.00,Ship one\n\
06/10/2026,111-DDD,Closed,$40.00,REF-DDD-2,06/12/2026,$25.00,Visa,\"=\"\"3333\"\"\",$25.00,Ship two\n\
06/20/2026,111-EEE,Closed,$50.00,REF-EEE,06/21/2026,$50.00,Mastercard,\"=\"\"4444\"\"\",$20.00,Only itemized part\n\
07/01/2026,111-FFF,Closed,$60.00,REF-FFF-CARD,07/02/2026,$45.00,Visa,\"=\"\"5555\"\"\",$25.00,Split item A\n\
07/01/2026,111-FFF,Closed,$60.00,REF-FFF-CARD,07/02/2026,$45.00,Visa,\"=\"\"5555\"\"\",$35.00,Split item B\n\
07/01/2026,111-FFF,Closed,$60.00,REF-FFF-GIFT,07/02/2026,$15.00,Gift Certificate/Card,,$25.00,Split item A\n\
07/01/2026,111-FFF,Closed,$60.00,REF-FFF-GIFT,07/02/2026,$15.00,Gift Certificate/Card,,$35.00,Split item B\n\
07/05/2026,111-GGG,Closed,$24.00,REF-GGG-1,07/06/2026,$12.00,Visa,\"=\"\"6666\"\"\",$12.00,Bought two of these\n\
07/05/2026,111-GGG,Closed,$24.00,REF-GGG-2,07/06/2026,$12.00,Visa,\"=\"\"6666\"\"\",$12.00,Bought two of these\n\
07/09/2026,111-HHH,Closed,$18.00,REF-HHH-A,07/10/2026,$18.00,Visa,\"=\"\"7777\"\"\",$18.00,Restated once\n\
07/09/2026,111-HHH,Closed,$18.00,REF-HHH-B,07/11/2026,$18.00,Visa,\"=\"\"7777\"\"\",$18.00,Restated once\n";

    #[test]
    fn skips_cancelled_and_pending() {
        let p = parse_amazon_orders(SAMPLE);
        assert_eq!(p.cancelled_orders, 1);
        assert_eq!(p.pending_orders, 1);
    }

    fn only(p: &AmazonParse, order: &str) -> Charge {
        let mut found: Vec<&Charge> = p.charges.iter().filter(|c| c.order_id == order).collect();
        assert_eq!(found.len(), 1, "{order} produced {} charges", found.len());
        found.remove(0).clone()
    }

    #[test]
    fn groups_items_into_one_charge() {
        let p = parse_amazon_orders(SAMPLE);
        let aaa = only(&p, "111-AAA");
        assert_eq!(aaa.payments_total(), 3000);
        assert_eq!(aaa.items.len(), 2);
        assert_eq!(
            aaa.payments[0].card_last4, "1111",
            "Excel ==\"…\" wrapper stripped"
        );
    }

    #[test]
    fn splits_one_order_into_separate_charges() {
        let p = parse_amazon_orders(SAMPLE);
        let ddd: Vec<_> = p
            .charges
            .iter()
            .filter(|c| c.order_id == "111-DDD")
            .collect();
        assert_eq!(
            ddd.len(),
            2,
            "two shipments, each paid for in full → two charges"
        );
        let amounts: Vec<i64> = ddd.iter().map(|c| c.payments_total()).collect();
        assert!(amounts.contains(&1500) && amounts.contains(&2500));
    }

    /// The bug this grouping exists for. An order settled part on a card and
    /// part on a gift card lists its *whole* item set under each settlement, so
    /// one entry per settlement books the purchase twice and needs a reconciling
    /// line the size of the second copy to balance.
    #[test]
    fn a_split_tender_order_books_its_items_once() {
        let p = parse_amazon_orders(SAMPLE);
        let fff = only(&p, "111-FFF");
        assert_eq!(fff.items.len(), 2, "the two items, listed once");
        assert_eq!(fff.items_total(), 6000);
        assert_eq!(fff.payments.len(), 2, "a clearing credit per settlement");
        assert_eq!(fff.payments_total(), 6000);
        assert_eq!(
            fff.payments_total() - fff.items_total(),
            0,
            "nothing left over, so no reconciling line"
        );
    }

    /// The case the footing rule protects: two identical settlements that are
    /// two real charges, not one restated. Each pays for its own item in full,
    /// and together they make the order's net total — so both stand.
    #[test]
    fn two_real_charges_of_the_same_amount_are_not_collapsed() {
        let p = parse_amazon_orders(SAMPLE);
        let ggg: Vec<_> = p
            .charges
            .iter()
            .filter(|c| c.order_id == "111-GGG")
            .collect();
        assert_eq!(ggg.len(), 2, "one item bought twice, shipped separately");
        assert_eq!(ggg.iter().map(|c| c.payments_total()).sum::<i64>(), 2400);
    }

    /// The same settlement re-reported a day later under a new reference id.
    /// Dropping it is what makes the order foot to Amazon's own net total, which
    /// is the only reason it may be dropped.
    #[test]
    fn a_settlement_restated_under_a_new_reference_is_dropped() {
        let p = parse_amazon_orders(SAMPLE);
        let hhh = only(&p, "111-HHH");
        assert_eq!(p.restated_payments, 1);
        assert_eq!(
            hhh.payments.len(),
            1,
            "the restatement is not a second charge"
        );
        assert_eq!(hhh.payments_total(), 1800);
        assert_eq!(hhh.items_total(), 1800);
    }

    /// A report with no `Payment Reference ID` column at all — every export
    /// before Amazon added it — still parses on the old date/amount/card key.
    #[test]
    fn a_report_without_payment_references_still_parses() {
        let legacy = SAMPLE
            .lines()
            .map(|l| {
                let f: Vec<&str> = l.split(',').collect();
                // Drop the reference column (index 4) the sample carries.
                let mut kept = f.clone();
                kept.remove(4);
                kept.join(",")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let p = parse_amazon_orders(&legacy);
        let ddd: Vec<_> = p
            .charges
            .iter()
            .filter(|c| c.order_id == "111-DDD")
            .collect();
        assert_eq!(
            ddd.len(),
            2,
            "distinct payments still separate without a reference id"
        );
    }

    #[test]
    fn every_charge_is_at_least_one_settlement() {
        let p = parse_amazon_orders(SAMPLE);
        assert!(!p.charges.is_empty());
        for c in &p.charges {
            assert!(!c.payments.is_empty(), "{} has no settlement", c.order_id);
        }
    }

    #[test]
    fn last4_handles_escaping_and_short_values() {
        assert_eq!(last4("=\"1234\""), "1234");
        assert_eq!(last4("=\"xxxx5678\""), "5678");
        assert_eq!(last4("99"), "99");
        assert_eq!(last4("N/A"), "");
    }

    #[test]
    fn underfooted_order_keeps_payment_authoritative() {
        let p = parse_amazon_orders(SAMPLE);
        let eee = only(&p, "111-EEE");
        assert_eq!(eee.payments_total(), 5000);
        assert_eq!(
            eee.items_total(),
            2000,
            "items under-foot; reconciling line covers the 30.00 gap"
        );
    }
}

#[cfg(test)]
mod plan_and_post_agree {
    use super::*;
    use crate::commands::ingest_commands::set_account_mapping;
    use crate::events::types::{Event, EventAccountType, EventEnvelope};
    use crate::store::migrations::SchemaStore;
    use crate::store::projections::ProjectionStore;

    /// Books with the Amazon clearing mapping and a parking account, as a real
    /// import needs.
    fn books() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        store.init_schema().unwrap();
        for (id, ty, num, name) in [
            (
                "clearing",
                EventAccountType::Liability,
                "2100",
                "Amazon clearing",
            ),
            ("uncat", EventAccountType::Expense, "9000", "Uncategorized"),
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
        set_account_mapping(store.connection(), AMAZON_CLEARING_KEY, "clearing").unwrap();
        store
    }

    /// The property the split exists for: what a replica plans is exactly what a
    /// standalone ledger posts. Two descriptions of what an Amazon charge becomes
    /// would drift, and the symptom is two members' books disagreeing about the
    /// same order.
    #[test]
    fn the_planner_produces_exactly_what_the_local_import_posts() {
        // Plan against a read-only view.
        let planning = books();
        let (planned, plan_summary) =
            plan_amazon_entries(planning.connection(), super::tests::SAMPLE).unwrap();

        // Post through the local path on identical books.
        let mut posting = books();
        let post_summary =
            ingest_amazon_orders(&mut posting, "test", super::tests::SAMPLE).unwrap();

        assert_eq!(
            planned.len(),
            post_summary.entries_posted,
            "the planner and the local import disagree on how many entries an \
             order history produces"
        );
        assert_eq!(plan_summary.charges_seen, post_summary.charges_seen);
        assert_eq!(plan_summary.cancelled_orders, post_summary.cancelled_orders);
        assert_eq!(plan_summary.pending_orders, post_summary.pending_orders);

        // …and line for line, against what actually landed.
        let conn = posting.connection();
        for cmd in &planned {
            let reference = cmd
                .reference
                .as_deref()
                .expect("every charge is idempotent");
            let entry_id: String = conn
                .query_row(
                    "SELECT id FROM journal_entries WHERE reference = ?1",
                    [reference],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| panic!("planned {reference} never posted"));
            let mut stmt = conn
                .prepare("SELECT account_id, amount FROM journal_lines WHERE entry_id = ?1 ORDER BY account_id, amount")
                .unwrap();
            let posted: Vec<(String, i64)> = stmt
                .query_map([&entry_id], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            let mut expected: Vec<(String, i64)> = cmd
                .lines
                .iter()
                .map(|l| (l.account_id.clone(), l.amount))
                .collect();
            expected.sort();
            assert_eq!(posted, expected, "lines differ for {reference}");
        }
    }

    /// Books with no parking account cannot be planned against, and the refusal
    /// has to name the fix — on hosted books nothing here may create one.
    #[test]
    fn missing_uncategorized_is_refused_with_something_to_do_about_it() {
        let store = books();
        store
            .connection()
            .execute("UPDATE accounts SET is_active = 0 WHERE id = 'uncat'", [])
            .unwrap();
        let err = plan_amazon_entries(store.connection(), super::tests::SAMPLE).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Accounts page"), "no route out: {msg}");
        assert!(msg.contains("Uncategorized"), "{msg}");
    }
}
