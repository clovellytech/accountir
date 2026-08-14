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
//! Two real-world quirks this handles:
//!   - `Payment Identifier` is Excel-escaped as `="…1234"` to stop spreadsheets
//!     mangling it. We strip the `="…"` wrapper and keep the last 4 digits.
//!   - Line items don't always foot to the order/payment total (returns,
//!     partial shipments). The **`Payment Amount` is authoritative** — it's what
//!     hit the card — so any difference goes to a reconciling line for review
//!     rather than silently unbalancing the entry.
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
//!   Dr  Uncategorized expense   reconciling diff (only if items != payment)
//!     Cr  Amazon clearing (amazon_clearing)   payment amount  (the card charge)
//! ```
//!
//! Each item lands in **Uncategorized** so you can reassign it to the right
//! expense account in the app — that's the "categorize" step. One entry is
//! posted per card charge, idempotent on
//! `amazon-<order>-<paydate>-<amount>-<last4>`.

use crate::commands::account_commands::find_or_create_uncategorized;
use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use crate::commands::import_commands::{parse_amount, parse_date, parse_delimited_line};
use crate::commands::ingest_commands::{
    check_idempotent, load_all_mappings, load_ingest_mappings, post_ingest_entry, IngestError,
};
use crate::events::types::JournalEntrySource;
use crate::store::event_store::EventStore;
use rusqlite::Connection;
use chrono::NaiveDate;
use std::collections::{HashMap, HashSet};

/// The ingest mapping key for the Amazon clearing/liability account that the
/// card feed posts charges to and this import clears.
pub const AMAZON_CLEARING_KEY: &str = "amazon_clearing";

/// Column names we read from the Order History Report (case-insensitive).
mod columns {
    pub const ORDER_DATE: &str = "order date";
    pub const ORDER_ID: &str = "order id";
    pub const ORDER_STATUS: &str = "order status";
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
    /// Card charges (payment groups) seen in the file.
    pub charges_seen: usize,
    /// Orders skipped because they were cancelled (never charged).
    pub cancelled_orders: usize,
    /// Orders skipped because they were still pending (not yet settled).
    pub pending_orders: usize,
    /// Charges whose line items didn't foot to the payment total (got a
    /// reconciling line — worth a human look).
    pub reconciled_charges: usize,
}

/// One card charge: the unit that reconciles against a single credit-card line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Charge {
    order_id: String,
    date: NaiveDate,
    /// What actually hit the card, in cents. Authoritative.
    payment_amount: i64,
    card_type: String,
    card_last4: String,
    /// (item title, item net total in cents)
    items: Vec<(String, i64)>,
}

/// Result of parsing the report content, before anything touches the store.
#[derive(Debug, Default)]
struct AmazonParse {
    charges: Vec<Charge>,
    cancelled_orders: usize,
    pending_orders: usize,
}

/// Strip Excel's `="…"` text-escaping wrapper from a cell.
fn clean_cell(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('=').unwrap_or(s);
    s.trim_matches('"').trim().to_string()
}

/// Keep the last 4 digits of a (possibly masked/escaped) payment identifier.
fn last4(s: &str) -> String {
    let digits: String = clean_cell(s).chars().filter(|c| c.is_ascii_digit()).collect();
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

/// Parse the report into card charges, skipping cancelled and pending orders.
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

    let mut charges: Vec<Charge> = Vec::new();
    let mut charge_index: HashMap<String, usize> = HashMap::new();
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

        // Group by the actual card charge: an order can be split across
        // multiple shipments/cards, each a separate charge.
        let key = format!(
            "{}|{}|{}|{}",
            order_id, payment_date_raw, payment_amount, card_last4
        );
        let idx = *charge_index.entry(key).or_insert_with(|| {
            charges.push(Charge {
                order_id: order_id.clone(),
                date,
                payment_amount,
                card_type: card_type.clone(),
                card_last4: card_last4.clone(),
                items: Vec::new(),
            });
            charges.len() - 1
        });
        if item_total != 0 || !title.is_empty() {
            charges[idx].items.push((title, item_total));
        }
    }

    AmazonParse {
        charges,
        cancelled_orders: cancelled.len(),
        pending_orders: pending.len(),
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
        ..Default::default()
    };

    if parsed.charges.is_empty() {
        return Ok((Vec::new(), summary));
    }

    // Pass 1 (immutable store borrow): drop charges already imported.
    let mut to_post: Vec<(Charge, String)> = Vec::new();
    for charge in parsed.charges {
        let reference = charge_reference(&charge);
        if check_idempotent(conn, &reference).is_some() {
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
    let uncategorized_id = crate::commands::account_commands::find_uncategorized(conn)
        .ok_or_else(|| {
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
            lines.push(EntryLine::debit(&uncategorized_id, *amount, "USD").with_memo(&memo_of(title)));
        }

        // The payment amount is authoritative; book any shortfall/overage so the
        // entry balances and the discrepancy is visible.
        let discrepancy = charge.payment_amount - items_sum;
        if discrepancy != 0 {
            lines.push(
                EntryLine {
                    account_id: uncategorized_id.clone(),
                    amount: discrepancy, // positive = debit, negative = credit
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: Some("Amazon line-item vs payment reconciling difference — review".to_string()),
                },
            );
            summary.reconciled_charges += 1;
        }

        // Clear the parked card charge.
        let card = card_label(&charge);
        lines.push(
            EntryLine::credit(&clearing_id, charge.payment_amount, "USD")
                .with_memo(&format!("Amazon order {} ({})", charge.order_id, card)),
        );

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

/// Idempotency key for a charge — one ledger entry per card charge.
fn charge_reference(c: &Charge) -> String {
    format!(
        "amazon-{}-{}-{}-{}",
        c.order_id,
        c.date.format("%Y%m%d"),
        c.payment_amount,
        c.card_last4
    )
}

/// Human-readable card label, e.g. "Mastercard ••1234".
fn card_label(c: &Charge) -> String {
    if c.card_last4.is_empty() {
        c.card_type.clone()
    } else {
        format!("{} ••{}", c.card_type, c.card_last4)
    }
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
    /// e.g. "Mastercard ••1234".
    pub card: String,
    /// Already in the ledger (same reference) — will be skipped.
    pub already_imported: bool,
    /// payment - sum(items); nonzero means a reconciling line will be added.
    pub reconciling_diff_cents: i64,
}

/// A non-mutating preview of what an Amazon import will do. Lets the UI show the
/// exact effect — entries, dollar total, what's skipped — before committing.
#[derive(Debug, Default)]
pub struct AmazonPlan {
    pub charges: Vec<PlannedCharge>,
    pub cancelled_orders: usize,
    pub pending_orders: usize,
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
            let already_imported = check_idempotent(conn, &charge_reference(c)).is_some();
            let items_sum: i64 = c.items.iter().map(|(_, a)| a).sum();
            PlannedCharge {
                order_id: c.order_id.clone(),
                date: c.date,
                amount_cents: c.payment_amount,
                item_count: c.items.len(),
                card: card_label(c),
                already_imported,
                reconciling_diff_cents: c.payment_amount - items_sum,
            }
        })
        .collect();

    AmazonPlan {
        charges,
        cancelled_orders: parsed.cancelled_orders,
        pending_orders: parsed.pending_orders,
        clearing_account_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal report carrying only the columns the parser reads, exercising:
    // a clean 2-item order, a cancelled order (skip), a pending order (skip),
    // a split order (two charges on the same order id), an Excel-escaped card
    // identifier, and an order whose items don't foot to the payment total.
    pub(super) const SAMPLE: &str = "\u{feff}Order Date,Order ID,Order Status,Payment Date,Payment Amount,Payment Instrument Type,Payment Identifier,Item Net Total,Title\n\
06/25/2026,111-AAA,Closed,06/26/2026,$30.00,Mastercard,\"=\"\"1111\"\"\",$10.00,Widget A\n\
06/25/2026,111-AAA,Closed,06/26/2026,$30.00,Mastercard,\"=\"\"1111\"\"\",$20.00,Widget B\n\
06/01/2026,111-BBB,Cancelled,,,N/A,,$0.00,Cancelled thing\n\
06/02/2026,111-CCC,Pending,06/29/2026,$5.00,Visa,\"=\"\"2222\"\"\",$5.00,Pending thing\n\
06/10/2026,111-DDD,Closed,06/11/2026,$15.00,Visa,\"=\"\"3333\"\"\",$15.00,Ship one\n\
06/10/2026,111-DDD,Closed,06/12/2026,$25.00,Visa,\"=\"\"3333\"\"\",$25.00,Ship two\n\
06/20/2026,111-EEE,Closed,06/21/2026,$50.00,Mastercard,\"=\"\"4444\"\"\",$20.00,Only itemized part\n";

    #[test]
    fn skips_cancelled_and_pending() {
        let p = parse_amazon_orders(SAMPLE);
        assert_eq!(p.cancelled_orders, 1);
        assert_eq!(p.pending_orders, 1);
    }

    #[test]
    fn groups_items_into_one_charge() {
        let p = parse_amazon_orders(SAMPLE);
        let aaa: Vec<_> = p.charges.iter().filter(|c| c.order_id == "111-AAA").collect();
        assert_eq!(aaa.len(), 1, "two line items, one charge");
        assert_eq!(aaa[0].payment_amount, 3000);
        assert_eq!(aaa[0].items.len(), 2);
        assert_eq!(aaa[0].card_last4, "1111", "Excel ==\"…\" wrapper stripped");
    }

    #[test]
    fn splits_one_order_into_separate_charges() {
        let p = parse_amazon_orders(SAMPLE);
        let ddd: Vec<_> = p.charges.iter().filter(|c| c.order_id == "111-DDD").collect();
        assert_eq!(ddd.len(), 2, "two payment dates/amounts → two charges");
        let amounts: Vec<i64> = ddd.iter().map(|c| c.payment_amount).collect();
        assert!(amounts.contains(&1500) && amounts.contains(&2500));
    }

    #[test]
    fn charge_count_matches_distinct_payments() {
        let p = parse_amazon_orders(SAMPLE);
        // AAA(1) + DDD(2) + EEE(1) = 4 charges; BBB cancelled, CCC pending.
        assert_eq!(p.charges.len(), 4);
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
        let eee = p.charges.iter().find(|c| c.order_id == "111-EEE").unwrap();
        let items_sum: i64 = eee.items.iter().map(|(_, a)| a).sum();
        assert_eq!(eee.payment_amount, 5000);
        assert_eq!(items_sum, 2000, "items under-foot; reconciling line covers the 30.00 gap");
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
            ("clearing", EventAccountType::Liability, "2100", "Amazon clearing"),
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
        let post_summary = ingest_amazon_orders(&mut posting, "test", super::tests::SAMPLE).unwrap();

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
            let reference = cmd.reference.as_deref().expect("every charge is idempotent");
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
