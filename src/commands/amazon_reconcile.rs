//! Pair the Amazon clearing account's two sides against each other and report
//! what is left over.
//!
//! The clearing account is a hinge between two independent sources. The card
//! feed posts what Amazon actually charged the card; [`amazon_commands`] posts
//! what the Business order history says was bought. Every purchase should arrive
//! from both directions and net to nothing.
//!
//! What is left when they are paired is the interesting part, and the two
//! leftovers mean different things:
//!
//! - **A card charge with no order.** The card was charged by Amazon for
//!   something the Business account never ordered — a purchase made on a
//!   personal Amazon account with the business card, or a charge that is not an
//!   order at all (a subscription, AWS, a Prime renewal).
//! - **An order with no card charge.** The Business account ordered it and
//!   something paid for it, but no card feed in these books ever shows the
//!   money leaving — a personal card on the business Amazon account, or simply a
//!   card whose statement was never imported.
//!
//! Neither is proof of a mistake on its own; both are exactly where to look for
//! one. The report also breaks the orders down by payment instrument, because a
//! card that appears a handful of times among hundreds is worth a second look
//! whether or not it reconciles.
//!
//! [`amazon_commands`]: crate::commands::amazon_commands

use std::collections::HashMap;

use chrono::NaiveDate;
use rusqlite::Connection;

use crate::commands::amazon_commands::AMAZON_CLEARING_KEY;
use crate::commands::ingest_commands::load_all_mappings;

/// How far apart a card charge and its order may sit and still be the same
/// purchase.
///
/// Generous on purpose. The two dates come from different places — Amazon's
/// settlement date and the bank's posting date — and a statement imported months
/// later still carries its own date. Amounts do the real matching; this only
/// stops a recurring charge of the same size in a different year from pairing
/// with the wrong one.
const MATCH_WINDOW_DAYS: i64 = 90;

/// One line on the clearing account, from either direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearingLine {
    pub entry_id: String,
    pub date: NaiveDate,
    /// The line's own memo, falling back to the entry's.
    pub memo: String,
    /// Always positive — the two sides are opposite signs, and the size is what
    /// pairs them.
    pub amount_cents: i64,
    /// The payment instrument, for lines that came from the order import.
    pub card: Option<String>,
}

/// What one payment instrument accounts for across the imported orders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardTotal {
    pub card: String,
    pub charges: usize,
    pub total_cents: i64,
    pub first: NaiveDate,
    pub last: NaiveDate,
    /// Charges on this card that no card feed in these books pays for.
    pub unmatched: usize,
}

/// The result of pairing the clearing account's two sides.
#[derive(Debug, Default)]
pub struct AmazonReconciliation {
    /// Amazon charges on the card feed that no imported order explains.
    pub charges_without_orders: Vec<ClearingLine>,
    /// Imported orders that no card feed pays for.
    pub orders_without_charges: Vec<ClearingLine>,
    /// Every payment instrument the orders were settled on, commonest first.
    pub cards: Vec<CardTotal>,
    pub matched: usize,
    pub matched_cents: i64,
    /// The clearing account's balance — what the two sides fail to net out by.
    pub clearing_balance_cents: i64,
    /// Set when the books carry no `amazon_clearing` mapping to reconcile.
    pub missing_mapping: bool,
}

impl AmazonReconciliation {
    pub fn charges_without_orders_cents(&self) -> i64 {
        self.charges_without_orders
            .iter()
            .map(|l| l.amount_cents)
            .sum()
    }

    pub fn orders_without_charges_cents(&self) -> i64 {
        self.orders_without_charges
            .iter()
            .map(|l| l.amount_cents)
            .sum()
    }
}

/// Read the clearing account's lines and pair them.
pub fn reconcile_amazon(conn: &Connection) -> AmazonReconciliation {
    let Some(clearing_id) = load_all_mappings(conn).get(AMAZON_CLEARING_KEY).cloned() else {
        return AmazonReconciliation {
            missing_mapping: true,
            ..Default::default()
        };
    };

    let (orders, feed, balance) = load_clearing_lines(conn, &clearing_id);
    let cards = summarize_cards(&orders);
    let (matched, matched_cents, orders_left, feed_left) = pair(orders, feed);

    let mut cards = cards;
    for card in &mut cards {
        card.unmatched = orders_left
            .iter()
            .filter(|l| l.card.as_deref() == Some(card.card.as_str()))
            .count();
    }

    AmazonReconciliation {
        charges_without_orders: feed_left,
        orders_without_charges: orders_left,
        cards,
        matched,
        matched_cents,
        clearing_balance_cents: balance,
        missing_mapping: false,
    }
}

/// The clearing account's lines, split by which side posted them, plus the
/// account's balance.
///
/// The order import is the side that writes an `amazon-…` reference, so that is
/// what tells the two apart — not the sign, which a refund inverts.
fn load_clearing_lines(
    conn: &Connection,
    clearing_id: &str,
) -> (Vec<ClearingLine>, Vec<ClearingLine>, i64) {
    let mut orders = Vec::new();
    let mut feed = Vec::new();
    let mut balance = 0i64;

    let Ok(mut stmt) = conn.prepare(
        "SELECT jl.entry_id, je.date, COALESCE(NULLIF(jl.memo, ''), je.memo), jl.amount, \
                COALESCE(je.reference, '') \
         FROM journal_lines jl JOIN journal_entries je ON je.id = jl.entry_id \
         WHERE jl.account_id = ?1 AND je.is_void = 0 \
         ORDER BY je.date, jl.id",
    ) else {
        return (orders, feed, balance);
    };
    let rows = stmt.query_map([clearing_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    });
    let Ok(rows) = rows else {
        return (orders, feed, balance);
    };

    for (entry_id, date, memo, amount, reference) in rows.flatten() {
        balance += amount;
        let Ok(date) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
            continue;
        };
        if amount == 0 {
            continue;
        }
        let line = ClearingLine {
            entry_id,
            date,
            card: reference
                .starts_with("amazon-")
                .then(|| card_from_memo(&memo))
                .flatten(),
            memo,
            amount_cents: amount.abs(),
        };
        if reference.starts_with("amazon-") {
            orders.push(line);
        } else {
            feed.push(line);
        }
    }
    (orders, feed, balance)
}

/// Pull the payment instrument out of an order line's memo, which the import
/// writes as `Amazon order <id> (<card>)`.
fn card_from_memo(memo: &str) -> Option<String> {
    let open = memo.rfind('(')?;
    let close = memo.rfind(')')?;
    if close <= open + 1 {
        return None;
    }
    Some(memo[open + 1..close].trim().to_string())
}

/// Totals per payment instrument across the imported orders, commonest first.
fn summarize_cards(orders: &[ClearingLine]) -> Vec<CardTotal> {
    let mut by_card: HashMap<String, CardTotal> = HashMap::new();
    for line in orders {
        let Some(card) = line.card.as_ref() else {
            continue;
        };
        let entry = by_card.entry(card.clone()).or_insert_with(|| CardTotal {
            card: card.clone(),
            charges: 0,
            total_cents: 0,
            first: line.date,
            last: line.date,
            unmatched: 0,
        });
        entry.charges += 1;
        entry.total_cents += line.amount_cents;
        entry.first = entry.first.min(line.date);
        entry.last = entry.last.max(line.date);
    }
    let mut cards: Vec<CardTotal> = by_card.into_values().collect();
    // Commonest first, then by name so the report does not shuffle run to run.
    cards.sort_by(|a, b| {
        b.charges
            .cmp(&a.charges)
            .then_with(|| b.total_cents.cmp(&a.total_cents))
            .then_with(|| a.card.cmp(&b.card))
    });
    cards
}

/// Pair the two sides on amount, nearest date first, and hand back what is left.
///
/// Amount is the key because it is the one thing both sources agree on exactly.
/// Within an amount, the nearest dates are paired first: two $12.99 charges a
/// year apart should each find their own order, not swap.
fn pair(
    orders: Vec<ClearingLine>,
    feed: Vec<ClearingLine>,
) -> (usize, i64, Vec<ClearingLine>, Vec<ClearingLine>) {
    let mut feed_by_amount: HashMap<i64, Vec<ClearingLine>> = HashMap::new();
    for line in feed {
        feed_by_amount
            .entry(line.amount_cents)
            .or_default()
            .push(line);
    }

    let mut matched = 0usize;
    let mut matched_cents = 0i64;
    let mut orders_left = Vec::new();

    // Candidate pairs, closest in time first, so a near-exact match is never
    // consumed by a distant one that happened to be considered earlier.
    let mut orders = orders;
    orders.sort_by(|a, b| {
        a.date
            .cmp(&b.date)
            .then_with(|| a.entry_id.cmp(&b.entry_id))
    });

    for order in orders {
        let candidates = feed_by_amount.get_mut(&order.amount_cents);
        let best = candidates.as_ref().and_then(|lines| {
            lines
                .iter()
                .enumerate()
                .map(|(i, l)| (i, (l.date - order.date).num_days().abs()))
                .filter(|(_, days)| *days <= MATCH_WINDOW_DAYS)
                .min_by_key(|(_, days)| *days)
                .map(|(i, _)| i)
        });
        match (candidates, best) {
            (Some(lines), Some(i)) => {
                lines.remove(i);
                matched += 1;
                matched_cents += order.amount_cents;
            }
            _ => orders_left.push(order),
        }
    }

    let mut feed_left: Vec<ClearingLine> = feed_by_amount.into_values().flatten().collect();
    feed_left.sort_by(|a, b| {
        a.date
            .cmp(&b.date)
            .then_with(|| a.entry_id.cmp(&b.entry_id))
    });
    (matched, matched_cents, orders_left, feed_left)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(date: (i32, u32, u32), cents: i64, memo: &str, card: Option<&str>) -> ClearingLine {
        ClearingLine {
            entry_id: format!("{}-{}-{}", date.2, cents, memo),
            date: NaiveDate::from_ymd_opt(date.0, date.1, date.2).unwrap(),
            memo: memo.to_string(),
            amount_cents: cents,
            card: card.map(str::to_string),
        }
    }

    #[test]
    fn a_charge_and_its_order_cancel_out() {
        let orders = vec![line(
            (2026, 4, 10),
            4238,
            "Amazon order X (Visa ••1174)",
            Some("Visa ••1174"),
        )];
        let feed = vec![line((2026, 4, 12), 4238, "AMAZON MARKEPLACE", None)];
        let (matched, cents, orders_left, feed_left) = pair(orders, feed);
        assert_eq!(matched, 1);
        assert_eq!(cents, 4238);
        assert!(orders_left.is_empty() && feed_left.is_empty());
    }

    /// The question the report exists to answer, in both directions.
    #[test]
    fn each_side_reports_what_the_other_cannot_explain() {
        let orders = vec![
            line(
                (2025, 5, 23),
                26180,
                "Amazon order P (Mastercard ••8922)",
                Some("Mastercard ••8922"),
            ),
            line(
                (2026, 4, 10),
                4238,
                "Amazon order X (Visa ••1174)",
                Some("Visa ••1174"),
            ),
        ];
        let feed = vec![
            line((2026, 4, 12), 4238, "AMAZON MARKEPLACE", None),
            line((2026, 1, 3), 1599, "AMZN Mktp US", None),
        ];
        let (matched, _, orders_left, feed_left) = pair(orders, feed);
        assert_eq!(matched, 1);
        assert_eq!(orders_left.len(), 1, "the order nothing paid for");
        assert_eq!(orders_left[0].card.as_deref(), Some("Mastercard ••8922"));
        assert_eq!(feed_left.len(), 1, "the charge no order explains");
        assert_eq!(feed_left[0].amount_cents, 1599);
    }

    /// Two charges of the same size in different years must each find their own
    /// order rather than swapping — which is the whole reason for the window.
    #[test]
    fn equal_amounts_far_apart_do_not_pair() {
        let orders = vec![line(
            (2024, 3, 1),
            999,
            "Amazon order A (Visa ••1174)",
            Some("Visa ••1174"),
        )];
        let feed = vec![line((2026, 3, 1), 999, "AMZN", None)];
        let (matched, _, orders_left, feed_left) = pair(orders, feed);
        assert_eq!(matched, 0, "two years apart is not the same purchase");
        assert_eq!(orders_left.len(), 1);
        assert_eq!(feed_left.len(), 1);
    }

    #[test]
    fn repeated_amounts_pair_with_their_nearest_date() {
        let orders = vec![
            line(
                (2026, 1, 5),
                1299,
                "Amazon order A (Visa ••1174)",
                Some("Visa ••1174"),
            ),
            line(
                (2026, 3, 5),
                1299,
                "Amazon order B (Visa ••1174)",
                Some("Visa ••1174"),
            ),
        ];
        let feed = vec![
            line((2026, 3, 6), 1299, "AMZN later", None),
            line((2026, 1, 6), 1299, "AMZN earlier", None),
        ];
        let (matched, _, orders_left, feed_left) = pair(orders, feed);
        assert_eq!(matched, 2);
        assert!(orders_left.is_empty() && feed_left.is_empty());
    }

    #[test]
    fn the_card_comes_out_of_the_import_memo() {
        assert_eq!(
            card_from_memo("Amazon order 114-0354356-0182612 (Visa ••1174)").as_deref(),
            Some("Visa ••1174")
        );
        assert_eq!(
            card_from_memo("Amazon order 111-1 (Business Credit Account)").as_deref(),
            Some("Business Credit Account")
        );
        assert_eq!(card_from_memo("AMAZON MARKEPLACE NA PA"), None);
        assert_eq!(card_from_memo("Amazon order 111-1 ()"), None);
    }

    #[test]
    fn cards_are_ranked_and_dated() {
        let orders = vec![
            line(
                (2026, 8, 18),
                1888,
                "o (Mastercard ••1549)",
                Some("Mastercard ••1549"),
            ),
            line(
                (2026, 9, 3),
                1758,
                "o (Mastercard ••1549)",
                Some("Mastercard ••1549"),
            ),
            line((2024, 1, 1), 5000, "o (Visa ••1174)", Some("Visa ••1174")),
            line((2024, 2, 1), 5000, "o (Visa ••1174)", Some("Visa ••1174")),
            line((2024, 3, 1), 5000, "o (Visa ••1174)", Some("Visa ••1174")),
        ];
        let cards = summarize_cards(&orders);
        assert_eq!(cards[0].card, "Visa ••1174");
        assert_eq!(cards[0].charges, 3);
        assert_eq!(cards[0].total_cents, 15000);
        assert_eq!(cards[1].card, "Mastercard ••1549");
        assert_eq!(
            cards[1].first,
            NaiveDate::from_ymd_opt(2026, 8, 18).unwrap()
        );
        assert_eq!(cards[1].last, NaiveDate::from_ymd_opt(2026, 9, 3).unwrap());
    }
}
