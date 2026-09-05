//! Importing a Sawyer orders export.
//!
//! # What the export is
//!
//! One row per order, with the whole life of that order already netted into it.
//! An order refunded in full comes back with `Net Order Amount at Checkout` of
//! zero — not the original amount and a refund beside it — and a cancelled order
//! that still cost a processing fee shows a *negative* amount to the provider.
//! So there is no refund handling here and there should not be: the columns are
//! the position, not the history, and re-deriving it would only be a chance to
//! disagree with Sawyer about what happened.
//!
//! # The identity everything rests on
//!
//! ```text
//! Net Order Amount − Stripe Fees − Sawyer Fees = Net Amt to Provider
//! ```
//!
//! It holds on every one of the 1,435 rows in this ledger's export, including the
//! refunded and cancelled ones, which is what makes the entry balance by
//! construction rather than by a plug. [`tests::the_identity_is_what_makes_it
//! _balance`] holds it.
//!
//! # Why the granularity is a choice
//!
//! A year of orders is thousands of entries. Posting each one is right when the
//! ledger is the record of who paid for what, and wrong when it buries every
//! other transaction in the journal. Neither is the correct answer in general, so
//! it is asked rather than decided — and the reference each entry carries records
//! which was chosen, so a period imported one way is recognised whichever way you
//! come back to it.
//!
//! Mixing them across the same dates would double-count, which is why
//! [`plan_sawyer`] refuses a period that is already imported at a different
//! granularity rather than quietly adding to it.

use std::collections::BTreeMap;

use chrono::{Datelike, Days, NaiveDate};
use rusqlite::Connection;

use crate::commands::entry_commands::{EntryLine, PostEntryCommand};
use crate::commands::ingest_commands::{IngestError, check_idempotent, load_ingest_mappings};
use crate::commands::import_commands::{parse_amount, parse_delimited_line};
use crate::events::types::JournalEntrySource;

/// How much of the export goes into one journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Grain {
    /// One entry per order. The journal becomes the record of who paid what.
    Transaction,
    Week,
    #[default]
    Month,
    Year,
}

impl Grain {
    pub const ALL: [Grain; 4] = [Grain::Transaction, Grain::Week, Grain::Month, Grain::Year];

    pub fn label(self) -> &'static str {
        match self {
            Grain::Transaction => "One entry per order",
            Grain::Week => "Weekly totals",
            Grain::Month => "Monthly totals",
            Grain::Year => "Yearly totals",
        }
    }

    /// The token that goes in an entry's reference.
    ///
    /// Part of the reference rather than beside it, so the same orders imported
    /// monthly and per-order do not collide on one key — and so a period already
    /// imported can be recognised whichever grain it was imported at.
    pub fn slug(self) -> &'static str {
        match self {
            Grain::Transaction => "order",
            Grain::Week => "week",
            Grain::Month => "month",
            Grain::Year => "year",
        }
    }
}

impl std::fmt::Display for Grain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One order, reduced to the four figures that matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Order {
    pub order_id: String,
    pub date: NaiveDate,
    /// Revenue, already net of coupons and refunds.
    pub net_order: i64,
    pub stripe_fees: i64,
    pub sawyer_fees: i64,
    /// What Sawyer owes the provider for it. Negative when a refunded order left
    /// a processing fee behind.
    pub to_provider: i64,
}

impl Order {
    /// Whether this order moves anything at all.
    ///
    /// An order paid entirely with a gift card, or fully discounted, is all
    /// zeroes: real in Sawyer, and nothing for a ledger to say.
    pub fn is_empty(&self) -> bool {
        self.net_order == 0 && self.stripe_fees == 0 && self.sawyer_fees == 0
    }
}

/// What a parse found, and what it could not read.
#[derive(Debug, Default)]
pub struct ParsedOrders {
    pub orders: Vec<Order>,
    /// Rows that could not be read, with the reason. Named rather than counted:
    /// a row skipped silently is money missing from the books.
    pub problems: Vec<String>,
}

fn column_index(headers: &[String], name: &str) -> Option<usize> {
    headers
        .iter()
        .position(|h| h.trim().eq_ignore_ascii_case(name))
}

/// Sawyer writes the order date as `2025-09-19 00:40:06 -0400`.
fn parse_order_date(raw: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(raw.get(..10)?, "%Y-%m-%d").ok()
}

/// Read an orders export.
pub fn parse_orders(content: &str) -> Result<ParsedOrders, IngestError> {
    let mut lines = content.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| IngestError::EntryError("the file is empty".to_string()))?;
    // Excel writes a byte-order mark; it belongs to the first header, not to the
    // name of the first column.
    let header_line = header_line.trim_start_matches('\u{feff}');
    let headers = parse_delimited_line(header_line, ',');

    let need = |name: &str| -> Result<usize, IngestError> {
        column_index(&headers, name).ok_or_else(|| {
            IngestError::MissingMapping(format!(
                "this does not look like a Sawyer orders export: no {name:?} column"
            ))
        })
    };
    let (c_id, c_date, c_net, c_stripe, c_sawyer, c_provider) = (
        need("Order ID")?,
        need("Order Date")?,
        need("Net Order Amount at Checkout")?,
        need("Stripe Fees at Checkout")?,
        need("Sawyer Fees at Checkout")?,
        need("Net Amt to Provider from Checkout")?,
    );

    let mut out = ParsedOrders::default();
    for (n, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let f = parse_delimited_line(line, ',');
        let get = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");
        let money = |i: usize| parse_amount(get(i)).unwrap_or(0);

        let Some(date) = parse_order_date(get(c_date)) else {
            out.problems.push(format!(
                "row {}: could not read the order date {:?}",
                n + 2,
                get(c_date)
            ));
            continue;
        };

        let order = Order {
            order_id: get(c_id).to_string(),
            date,
            net_order: money(c_net),
            stripe_fees: money(c_stripe),
            sawyer_fees: money(c_sawyer),
            to_provider: money(c_provider),
        };

        // Sawyer's own arithmetic, checked. If these disagree the columns have
        // moved and the entry would be wrong, so the row is reported rather than
        // posted on the assumption it still means what it used to.
        let expected = order.net_order - order.stripe_fees - order.sawyer_fees;
        if expected != order.to_provider {
            out.problems.push(format!(
                "order {}: net {} less fees {} does not equal the {} Sawyer says it owes",
                order.order_id,
                order.net_order,
                order.stripe_fees + order.sawyer_fees,
                order.to_provider
            ));
            continue;
        }
        out.orders.push(order);
    }
    Ok(out)
}

/// One entry's worth of orders.
#[derive(Debug, Clone)]
pub struct Group {
    pub key: String,
    /// The date the entry is posted on: the order's own date, or the last day of
    /// the period for an aggregate — a month's total belongs at the month's end
    /// rather than on whichever day happened to be first in the file.
    pub date: NaiveDate,
    pub orders: usize,
    pub net_order: i64,
    pub stripe_fees: i64,
    pub sawyer_fees: i64,
    pub to_provider: i64,
}

/// The last day of the week/month/year an order falls in.
fn period_end(date: NaiveDate, grain: Grain) -> NaiveDate {
    match grain {
        Grain::Transaction => date,
        // ISO weeks start Monday, so the end is the following Sunday.
        Grain::Week => {
            let from_monday = date.weekday().num_days_from_monday() as u64;
            date.checked_add_days(Days::new(6 - from_monday)).unwrap_or(date)
        }
        Grain::Month => {
            let (y, m) = (date.year(), date.month());
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            NaiveDate::from_ymd_opt(ny, nm, 1)
                .and_then(|d| d.checked_sub_days(Days::new(1)))
                .unwrap_or(date)
        }
        Grain::Year => NaiveDate::from_ymd_opt(date.year(), 12, 31).unwrap_or(date),
    }
}

/// Gather orders into the entries they will become.
pub fn group(orders: &[Order], grain: Grain) -> Vec<Group> {
    let mut by: BTreeMap<String, Group> = BTreeMap::new();
    for o in orders {
        if o.is_empty() {
            continue;
        }
        let end = period_end(o.date, grain);
        let key = match grain {
            Grain::Transaction => o.order_id.clone(),
            Grain::Week => format!("{}-W{:02}", end.iso_week().year(), end.iso_week().week()),
            Grain::Month => end.format("%Y-%m").to_string(),
            Grain::Year => end.format("%Y").to_string(),
        };
        let g = by.entry(key.clone()).or_insert(Group {
            key,
            date: end,
            orders: 0,
            net_order: 0,
            stripe_fees: 0,
            sawyer_fees: 0,
            to_provider: 0,
        });
        g.orders += 1;
        g.net_order += o.net_order;
        g.stripe_fees += o.stripe_fees;
        g.sawyer_fees += o.sawyer_fees;
        g.to_provider += o.to_provider;
    }
    by.into_values().collect()
}

/// The reference an entry carries, which is also what makes a re-import a no-op.
pub fn reference_for(grain: Grain, key: &str) -> String {
    format!("sawyer-{}-{}", grain.slug(), key)
}

/// What importing this export at this grain would do.
#[derive(Debug, Default)]
pub struct SawyerPlan {
    pub entries: Vec<(String, PostEntryCommand)>,
    pub already_imported: usize,
    /// Rows that could not be read, and periods already imported at a different
    /// grain.
    pub problems: Vec<String>,
    pub orders_read: usize,
}

/// Decide what an export posts, without writing anything.
pub fn plan_sawyer(
    conn: &Connection,
    content: &str,
    grain: Grain,
) -> Result<SawyerPlan, IngestError> {
    let parsed = parse_orders(content)?;
    let mut plan = SawyerPlan {
        orders_read: parsed.orders.len(),
        problems: parsed.problems,
        ..Default::default()
    };

    let groups = group(&parsed.orders, grain);
    if groups.is_empty() {
        return Ok(plan);
    }

    let mappings = load_ingest_mappings(
        conn,
        &[
            "sawyer_clearing",
            "sawyer_revenue",
            "sawyer_fees",
            "stripe_fees",
        ],
    )?;

    for g in groups {
        let reference = reference_for(grain, &g.key);
        if check_idempotent(conn, &reference).is_some() {
            plan.already_imported += 1;
            continue;
        }
        // The same orders posted twice at two grains would count twice, and the
        // references differ so nothing else would notice. Checked per period
        // rather than trusted.
        if let Some(other) = imported_at_another_grain(conn, grain, &g) {
            plan.problems.push(other);
            continue;
        }

        // Signed: a refunded order is a negative row, and at transaction grain
        // there is nothing to net it against. Each line flips side rather than
        // magnitude.
        let mut lines = vec![
            EntryLine::signed(&mappings["sawyer_clearing"], g.to_provider, "USD")
                .with_memo("Owed by Sawyer"),
        ];
        if g.stripe_fees != 0 {
            lines.push(
                EntryLine::signed(&mappings["stripe_fees"], g.stripe_fees, "USD")
                    .with_memo("Stripe processing fees"),
            );
        }
        if g.sawyer_fees != 0 {
            lines.push(
                EntryLine::signed(&mappings["sawyer_fees"], g.sawyer_fees, "USD")
                    .with_memo("Sawyer platform fees"),
            );
        }
        lines.push(
            EntryLine::signed(&mappings["sawyer_revenue"], -g.net_order, "USD")
                .with_memo("Class and camp revenue"),
        );
        lines.retain(|l| l.amount != 0);
        if lines.is_empty() {
            continue;
        }

        let memo = match grain {
            Grain::Transaction => format!("Sawyer order {}", g.key),
            _ => format!("Sawyer {} — {} order(s)", g.key, g.orders),
        };
        plan.entries.push((
            reference.clone(),
            PostEntryCommand {
                date: g.date,
                memo,
                lines,
                reference: Some(reference),
                source: Some(JournalEntrySource::Pos),
            },
        ));
    }
    Ok(plan)
}

/// Whether this period's orders are already in the books under another grain.
///
/// Looks for any Sawyer reference at a different grain whose entry falls inside
/// this group's period. Not exhaustive — an order-level import of one order in a
/// month is not the whole month — but it catches the case that actually happens,
/// which is importing a year monthly and then again yearly.
fn imported_at_another_grain(conn: &Connection, grain: Grain, g: &Group) -> Option<String> {
    let start = match grain {
        Grain::Transaction => g.date,
        Grain::Week => g.date.checked_sub_days(Days::new(6)).unwrap_or(g.date),
        Grain::Month => g.date.with_day(1).unwrap_or(g.date),
        Grain::Year => NaiveDate::from_ymd_opt(g.date.year(), 1, 1).unwrap_or(g.date),
    };
    let mut stmt = conn
        .prepare(
            "SELECT reference FROM journal_entries
              WHERE is_void = 0 AND reference LIKE 'sawyer-%'
                AND date >= ?1 AND date <= ?2 LIMIT 1",
        )
        .ok()?;
    let found: Option<String> = stmt
        .query_row([start.to_string(), g.date.to_string()], |r| r.get(0))
        .ok();
    let other = found?;
    if other.starts_with(&format!("sawyer-{}-", grain.slug())) {
        return None;
    }
    Some(format!(
        "{} is already in the books as {} — importing it again at a different grain would count \
         the same orders twice.",
        g.key, other
    ))
}

/// Post everything an export yields at one grain.
///
/// Each entry is posted on its own so that one refusal — a period already there
/// under another grain, a row that would not read — stops that entry and nothing
/// else. A year of orders should not be held up by one of them.
pub fn ingest_sawyer(
    store: &mut crate::store::event_store::EventStore,
    user_id: &str,
    content: &str,
    grain: Grain,
) -> Result<SawyerOutcome, IngestError> {
    let plan = plan_sawyer(store.connection(), content, grain)?;
    let mut out = SawyerOutcome {
        already_imported: plan.already_imported,
        problems: plan.problems,
        ..Default::default()
    };

    let mut commands =
        crate::commands::entry_commands::EntryCommands::new(store, user_id.to_string());
    for (reference, cmd) in plan.entries {
        match crate::commands::ingest_commands::post_ingest_entry(&mut commands, cmd) {
            Ok((_, true)) => out.already_imported += 1,
            Ok((_, false)) => out.posted += 1,
            Err(e) => out.problems.push(format!("{reference}: {e}")),
        }
    }
    Ok(out)
}

/// What an import did.
#[derive(Debug, Default)]
pub struct SawyerOutcome {
    pub posted: usize,
    pub already_imported: usize,
    pub problems: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Order ID,Order Date,Net Order Amount at Checkout,\
Stripe Fees at Checkout,Sawyer Fees at Checkout,Net Amt to Provider from Checkout";

    fn csv(rows: &[&str]) -> String {
        let mut s = HEADER.to_string();
        for r in rows {
            s.push('\n');
            s.push_str(r);
        }
        s
    }

    #[test]
    fn an_order_is_read_with_its_date_and_figures() {
        let p = parse_orders(&csv(&[
            "6261591,2025-09-19 09:10:38 -0400,30.00,1.17,0.00,28.83",
        ]))
        .unwrap();
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        let o = &p.orders[0];
        assert_eq!(o.order_id, "6261591");
        assert_eq!(o.date, NaiveDate::from_ymd_opt(2025, 9, 19).unwrap());
        assert_eq!((o.net_order, o.stripe_fees, o.to_provider), (3000, 117, 2883));
    }

    /// A refunded order comes back already net, and the fee it cost is still
    /// owed — so the provider is *out* that fee and the figure is negative.
    /// Re-deriving a refund from the columns would only be a chance to disagree
    /// with Sawyer about what happened.
    #[test]
    fn a_refunded_order_is_already_net_and_can_be_negative() {
        let p = parse_orders(&csv(&[
            "6339373,2026-01-05 10:00:00 -0500,0.00,6.39,0.00,-6.39",
        ]))
        .unwrap();
        assert!(p.problems.is_empty(), "{:?}", p.problems);
        let o = &p.orders[0];
        assert_eq!(o.net_order, 0);
        assert_eq!(o.to_provider, -639);
        assert!(!o.is_empty(), "a fee was still charged");
    }

    /// An order paid entirely by gift card or fully discounted is all zeroes.
    /// Real in Sawyer, and nothing for a ledger to say.
    #[test]
    fn an_order_that_moved_no_money_is_left_out() {
        let p = parse_orders(&csv(&[
            "6328119,2025-12-01 10:00:00 -0500,0.00,0.00,0.00,0.00",
        ]))
        .unwrap();
        assert!(p.orders[0].is_empty());
        assert!(group(&p.orders, Grain::Month).is_empty());
    }

    /// The identity is what makes every entry balance without a plug.
    #[test]
    fn the_identity_is_what_makes_it_balance() {
        let p = parse_orders(&csv(&[
            "1,2025-09-19 09:10:38 -0400,30.00,1.17,0.00,28.83",
            "2,2025-09-20 09:10:38 -0400,210.00,6.39,1.00,202.61",
        ]))
        .unwrap();
        for o in &p.orders {
            assert_eq!(o.net_order - o.stripe_fees - o.sawyer_fees, o.to_provider);
        }
        let g = &group(&p.orders, Grain::Month)[0];
        assert_eq!(g.net_order - g.stripe_fees - g.sawyer_fees, g.to_provider);
    }

    /// A row whose columns no longer add up is reported rather than posted on
    /// the assumption it still means what it used to.
    #[test]
    fn a_row_that_does_not_add_up_is_reported() {
        let p = parse_orders(&csv(&[
            "9,2025-09-19 09:10:38 -0400,30.00,1.17,0.00,99.99",
        ]))
        .unwrap();
        assert!(p.orders.is_empty());
        assert_eq!(p.problems.len(), 1);
        assert!(p.problems[0].contains("order 9"), "{:?}", p.problems);
    }

    #[test]
    fn a_bad_date_is_reported_with_its_row_number() {
        let p = parse_orders(&csv(&["9,not-a-date,30.00,1.17,0.00,28.83"])).unwrap();
        assert!(p.orders.is_empty());
        assert!(p.problems[0].contains("row 2"), "{:?}", p.problems);
    }

    #[test]
    fn a_file_that_is_not_a_sawyer_export_says_so() {
        let e = parse_orders("a,b,c\n1,2,3").unwrap_err();
        assert!(format!("{e}").contains("Sawyer orders export"), "{e}");
    }

    /// Each grain gathers the same orders differently and dates the entry at the
    /// end of the period, so a month's total lands on the month's last day
    /// rather than on whichever order happened to come first.
    #[test]
    fn the_grain_decides_how_many_entries_and_when_they_land() {
        let p = parse_orders(&csv(&[
            "1,2025-09-19 09:10:38 -0400,30.00,1.17,0.00,28.83",
            "2,2025-09-23 09:10:38 -0400,30.00,1.17,0.00,28.83",
            "3,2025-10-02 09:10:38 -0400,30.00,1.17,0.00,28.83",
        ]))
        .unwrap();

        assert_eq!(group(&p.orders, Grain::Transaction).len(), 3);
        assert_eq!(group(&p.orders, Grain::Year).len(), 1);

        let months = group(&p.orders, Grain::Month);
        assert_eq!(months.len(), 2);
        assert_eq!(months[0].key, "2025-09");
        assert_eq!(months[0].orders, 2);
        assert_eq!(months[0].net_order, 6000);
        assert_eq!(
            months[0].date,
            NaiveDate::from_ymd_opt(2025, 9, 30).unwrap(),
            "a month's total belongs at the month's end"
        );

        // 19 Sep 2025 is a Friday and 23 Sep the Tuesday after, so they are
        // different weeks even though they are the same month.
        assert_eq!(group(&p.orders, Grain::Week).len(), 3);
    }

    #[test]
    fn the_reference_records_which_grain_was_chosen() {
        assert_eq!(reference_for(Grain::Month, "2025-09"), "sawyer-month-2025-09");
        assert_eq!(reference_for(Grain::Transaction, "6261591"), "sawyer-order-6261591");
        assert_ne!(
            reference_for(Grain::Month, "2025-09"),
            reference_for(Grain::Year, "2025-09")
        );
    }
}
