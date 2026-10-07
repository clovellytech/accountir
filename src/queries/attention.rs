//! What is waiting for somebody, counted in one place.
//!
//! # Why a dashboard needs this
//!
//! The figures a dashboard shows — assets, income, what is owed — are the result of
//! bookkeeping that is finished. None of them says anything about the bookkeeping that
//! is *not*, and that is what a person opening the program can act on.
//!
//! The failure this exists to end is silence. A bank feed that stopped returning
//! transactions three weeks ago shows up nowhere: the balance sheet still balances,
//! the income statement still totals, and the only symptom is a number that is too
//! small. Likewise an import that parked two thousand lines in Uncategorized leaves
//! every report quietly wrong about every category, while footing perfectly.
//!
//! # Counts, not opinions
//!
//! Each item is a count with somewhere to go and one sentence saying why it matters.
//! Nothing here decides what is urgent: a book mid-migration may *want* a thousand
//! uncategorised lines, and a program that nagged about it would teach its reader to
//! ignore the panel. An item with a count of zero is simply absent.
//!
//! # Every count is cheap
//!
//! This runs on every visit to the dashboard, so each query is a count over an
//! indexed column or a small projection, and a missing table is zero rather than an
//! error — a book old enough to predate the bank feed has no feed to be stale.

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};

/// How far behind a bank feed has to be before it is worth mentioning.
///
/// Plaid's own transactions arrive within a day or two, and a weekend plus a holiday
/// is three. Two weeks is long enough that nothing normal explains it and short
/// enough to catch a quarter before it closes.
pub const STALE_FEED_DAYS: i64 = 14;

/// One thing waiting, as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub kind: Kind,
    /// How many. Never zero: an item with nothing in it is not built.
    pub count: usize,
    /// The one clause that says what is wrong, with the figure in it.
    pub detail: String,
}

/// What kind of thing is waiting. The dashboard maps these to the page that fixes
/// each one, which is why it is an enum and not a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Journal lines parked in the landing account an import uses when it cannot
    /// classify a transaction.
    Uncategorized,
    /// Bank transactions staged and not yet imported.
    StagedTransactions,
    /// Brokerage activity held because something about it needs a person.
    InvestmentReview,
    /// A provider investment account nobody has configured, so its activity is
    /// imported nowhere at all.
    UnconfiguredInvestment,
    /// A bank connection whose last successful pull is older than
    /// [`STALE_FEED_DAYS`], or which is no longer active.
    StaleFeed,
    /// A reconciliation started and never finished.
    OpenReconciliation,
    /// A cash account that has never been reconciled against a statement.
    NeverReconciled,
}

impl Kind {
    /// A short label, in the words of what is waiting rather than of the table it is
    /// counted from.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Uncategorized => "Uncategorized",
            Kind::StagedTransactions => "Waiting to import",
            Kind::InvestmentReview => "Brokerage review",
            Kind::UnconfiguredInvestment => "Brokerage not set up",
            Kind::StaleFeed => "Bank feed behind",
            Kind::OpenReconciliation => "Reconciliation open",
            Kind::NeverReconciled => "Never reconciled",
        }
    }
}

/// Everything waiting, in the order it is worth dealing with.
///
/// The order is not a severity ranking — it is the order the work runs in. A feed
/// that is behind is first because every other count is computed from data it has not
/// delivered yet; importing comes before categorising; reconciling comes last, because
/// it is the check on everything above it.
pub fn whats_waiting(conn: &Connection, today: NaiveDate) -> Vec<Item> {
    let mut items = Vec::new();
    let mut push = |kind: Kind, count: usize, detail: String| {
        if count > 0 {
            items.push(Item {
                kind,
                count,
                detail,
            });
        }
    };

    let stale = stale_feeds(conn, today);
    push(
        Kind::StaleFeed,
        stale.len(),
        match stale.first() {
            Some((name, Some(days))) if stale.len() == 1 => {
                format!("{name} last pulled {days} days ago. Transactions since then are not in the books.")
            }
            Some((name, None)) if stale.len() == 1 => {
                format!("{name} has never been pulled successfully.")
            }
            _ => format!(
                "{} connections have not been pulled in {STALE_FEED_DAYS} days.",
                stale.len()
            ),
        },
    );

    let staged = count_of(conn, "plaid_staged_transactions", "status = 'pending'");
    push(
        Kind::StagedTransactions,
        staged,
        format!("{staged} bank transactions are staged and not yet in the books."),
    );

    let held = count_of(conn, "investment_staged_activity", "status = 'pending'");
    push(
        Kind::InvestmentReview,
        held,
        format!("{held} pieces of brokerage activity are held for review and post nothing."),
    );

    let unconfigured = unconfigured_investment_accounts(conn);
    push(
        Kind::UnconfiguredInvestment,
        unconfigured,
        format!(
            "{unconfigured} investment accounts have no configuration, so their activity is \
             imported nowhere."
        ),
    );

    let uncategorized = uncategorized_lines(conn);
    push(
        Kind::Uncategorized,
        uncategorized,
        format!(
            "{uncategorized} lines are in Uncategorized. Every report that groups by account is \
             wrong by this much."
        ),
    );

    let open = count_of(conn, "reconciliations", "status = 'in_progress'");
    push(
        Kind::OpenReconciliation,
        open,
        format!("{open} reconciliations were started and not finished."),
    );

    let never = never_reconciled(conn);
    push(
        Kind::NeverReconciled,
        never,
        format!("{never} accounts a statement arrives for have never been reconciled against one."),
    );

    items
}

/// `SELECT COUNT(*)`, or zero where the table does not exist.
fn count_of(conn: &Connection, table: &str, predicate: &str) -> usize {
    if !has_table(conn, table) {
        return 0;
    }
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as usize)
    .unwrap_or(0)
}

fn has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Live journal lines on the landing account imports use.
///
/// Matched on the account the seeder creates for it, by name under Expenses — the same
/// account `find_uncategorized` resolves. Void entries are left out: a voided import is
/// not work waiting.
fn uncategorized_lines(conn: &Connection) -> usize {
    conn.query_row(
        "SELECT COUNT(*)
           FROM journal_lines jl
           JOIN journal_entries je ON je.id = jl.entry_id
           JOIN accounts a ON a.id = jl.account_id
          WHERE je.is_void = 0 AND a.name = 'Uncategorized'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as usize)
    .unwrap_or(0)
}

/// Provider accounts the bank feed skips and the Investments page has no configuration
/// for — so their activity reaches the books through neither.
fn unconfigured_investment_accounts(conn: &Connection) -> usize {
    if !has_table(conn, "plaid_local_accounts") || !has_table(conn, "investment_account_config") {
        return 0;
    }
    conn.query_row(
        "SELECT COUNT(*)
           FROM plaid_local_accounts pa
          WHERE lower(pa.account_type) IN ('investment', 'brokerage')
            AND NOT EXISTS (
                SELECT 1 FROM investment_account_config c
                 WHERE c.item_id = pa.item_id AND c.plaid_account_id = pa.plaid_account_id
            )",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as usize)
    .unwrap_or(0)
}

/// Connections that have not delivered in a while, as `(institution, days ago)`.
///
/// `None` days means it has never been pulled at all, which is a different sentence:
/// a connection that was linked and never worked.
fn stale_feeds(conn: &Connection, today: NaiveDate) -> Vec<(String, Option<i64>)> {
    if !has_table(conn, "plaid_items") {
        return Vec::new();
    }
    let Ok(mut stmt) = conn.prepare(
        "SELECT institution_name, last_synced_at, status FROM plaid_items ORDER BY institution_name",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
        ))
    });
    let Ok(rows) = rows else { return Vec::new() };
    rows.flatten()
        .filter(|(_, _, status)| status == "active")
        .filter_map(
            |(name, last, _)| match last.as_deref().and_then(parse_day) {
                None => Some((name, None)),
                Some(day) => {
                    let days = (today - day).num_days();
                    (days >= STALE_FEED_DAYS).then_some((name, Some(days)))
                }
            },
        )
        .collect()
}

/// A date as the feed records it, which may carry a time.
fn parse_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10).unwrap_or(s), "%Y-%m-%d").ok()
}

/// Accounts a statement arrives for, with no completed reconciliation.
///
/// Chequing, savings **and credit cards**: a card statement is reconciled exactly as a
/// bank statement is, and leaving cards out would quietly excuse the accounts where
/// double-counted feeds do the most damage. Investment accounts are excluded, because
/// what reconciles one is the holdings comparison on the Investments page rather than
/// a statement ticked off line by line.
///
/// Only *mapped* accounts, so an account nobody banks at — petty cash, a clearing
/// account — is not counted. There is no statement for it, and a count nobody can ever
/// clear is a warning readers learn to ignore.
fn never_reconciled(conn: &Connection) -> usize {
    if !has_table(conn, "plaid_local_accounts") || !has_table(conn, "reconciliations") {
        return 0;
    }
    conn.query_row(
        "SELECT COUNT(*)
           FROM (SELECT DISTINCT pa.local_account_id AS id
                   FROM plaid_local_accounts pa
                  WHERE lower(pa.account_type) IN ('depository', 'credit')
                    AND pa.local_account_id IS NOT NULL) bank
          WHERE NOT EXISTS (
                SELECT 1 FROM reconciliations r
                 WHERE r.account_id = bank.id AND r.status = 'completed'
            )",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n as usize)
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{
        create_default_accounts, AccountCommands, CreateAccountCommand,
    };
    use crate::domain::AccountType;
    use crate::events::types::{Event, EventEnvelope, JournalLineData};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::store::projections::ProjectionStore;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn store() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let stored = store
            .append(EventEnvelope::new(
                Event::CompanyCreated {
                    company_id: "c".into(),
                    name: "Books".into(),
                    base_currency: "USD".into(),
                    fiscal_year_start: 1,
                },
                "u".to_string(),
            ))
            .unwrap();
        store.apply_projection(&stored).unwrap();
        create_default_accounts(&mut store).expect("the default chart");
        store
    }

    fn kinds(items: &[Item]) -> Vec<Kind> {
        items.iter().map(|i| i.kind).collect()
    }

    fn find(items: &[Item], kind: Kind) -> Option<&Item> {
        items.iter().find(|i| i.kind == kind)
    }

    /// Nothing waiting is an empty panel, not a panel of zeroes. A dashboard that
    /// always shows seven rows is one nobody reads.
    #[test]
    fn a_book_with_nothing_waiting_has_nothing_to_say() {
        let store = store();
        assert_eq!(whats_waiting(store.connection(), day(2026, 10, 1)), vec![]);
    }

    /// The count that matters most in practice: an import parks its counter-leg in
    /// Uncategorized, and every report that groups by account is wrong by exactly
    /// this much while footing perfectly.
    #[test]
    fn lines_parked_in_uncategorized_are_counted_and_voids_are_not() {
        let mut store = store();
        let uncategorized: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE name = 'Uncategorized'",
                [],
                |r| r.get(0),
            )
            .expect("the seeded landing account");
        let checking: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE name = 'Business Checking'",
                [],
                |r| r.get(0),
            )
            .expect("seeded");
        for (id, void) in [("e1", false), ("e2", false), ("e3", true)] {
            let stored = store
                .append(EventEnvelope::new(
                    Event::JournalEntryPosted {
                        entry_id: id.to_string(),
                        date: day(2026, 9, 1),
                        memo: id.to_string(),
                        lines: vec![
                            JournalLineData {
                                line_id: format!("{id}-1"),
                                account_id: uncategorized.clone(),
                                amount: 1_000,
                                currency: "USD".into(),
                                exchange_rate: None,
                                memo: None,
                            },
                            JournalLineData {
                                line_id: format!("{id}-2"),
                                account_id: checking.clone(),
                                amount: -1_000,
                                currency: "USD".into(),
                                exchange_rate: None,
                                memo: None,
                            },
                        ],
                        reference: None,
                        source: None,
                    },
                    "u".to_string(),
                ))
                .unwrap();
            store.apply_projection(&stored).unwrap();
            if void {
                let stored = store
                    .append(EventEnvelope::new(
                        Event::JournalEntryVoided {
                            entry_id: id.to_string(),
                            reason: "no".into(),
                        },
                        "u".to_string(),
                    ))
                    .unwrap();
                store.apply_projection(&stored).unwrap();
            }
        }
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        let item = find(&items, Kind::Uncategorized).expect("counted");
        assert_eq!(item.count, 2, "the voided one is not work waiting");
        assert!(item.detail.contains("2 lines"), "{}", item.detail);
    }

    fn link(store: &EventStore, item: &str, institution: &str, last: Option<&str>) {
        store
            .connection()
            .execute(
                "INSERT INTO plaid_items (id, proxy_item_id, institution_name, status, last_synced_at)
                 VALUES (?1, ?1, ?2, 'active', ?3)",
                rusqlite::params![item, institution, last],
            )
            .expect("item");
    }

    /// A feed that has stopped shows up nowhere else: the balance sheet still
    /// balances and the only symptom is a figure that is too small.
    #[test]
    fn a_feed_that_has_not_delivered_in_a_fortnight_is_named() {
        let store = store();
        link(&store, "fresh", "Chase", Some("2026-09-30"));
        link(&store, "stale", "Merrill", Some("2026-09-01"));
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        let item = find(&items, Kind::StaleFeed).expect("counted");
        assert_eq!(item.count, 1);
        assert!(item.detail.contains("Merrill"), "{}", item.detail);
        assert!(item.detail.contains("30 days"), "{}", item.detail);
    }

    /// Exactly on the threshold counts. A fortnight is the point at which nothing
    /// normal explains it, so the day it arrives is the day to say so.
    #[test]
    fn the_threshold_is_inclusive() {
        let on_the_day = store();
        link(&on_the_day, "a", "Bank", Some("2026-09-17"));
        let items = whats_waiting(on_the_day.connection(), day(2026, 10, 1));
        assert_eq!(find(&items, Kind::StaleFeed).map(|i| i.count), Some(1));

        let a_day_short = store();
        link(&a_day_short, "a", "Bank", Some("2026-09-18"));
        let items = whats_waiting(a_day_short.connection(), day(2026, 10, 1));
        assert_eq!(find(&items, Kind::StaleFeed), None, "13 days is not behind");
    }

    /// Never pulled at all is its own sentence: a connection that was linked and has
    /// never worked is a different problem from one that has fallen behind.
    #[test]
    fn a_connection_that_has_never_pulled_says_so() {
        let store = store();
        link(&store, "a", "Schwab", None);
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        let item = find(&items, Kind::StaleFeed).expect("counted");
        assert!(item.detail.contains("never been pulled"), "{}", item.detail);
    }

    /// A disconnected connection is not behind — it is off. Counting it would leave a
    /// warning nobody can clear.
    #[test]
    fn a_disconnected_feed_is_not_a_feed_that_is_behind() {
        let store = store();
        link(&store, "a", "Bank", Some("2026-01-01"));
        store
            .connection()
            .execute("UPDATE plaid_items SET status = 'disconnected'", [])
            .unwrap();
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        assert_eq!(find(&items, Kind::StaleFeed), None);
    }

    /// An investment account nobody has configured imports its activity nowhere at
    /// all — the bank feed skips it and the Investments page has nothing to post to.
    #[test]
    fn an_investment_account_with_no_configuration_is_counted() {
        let store = store();
        link(&store, "item1", "Merrill", Some("2026-09-30"));
        for (pa, kind) in [("pa1", "investment"), ("pa2", "depository")] {
            store
                .connection()
                .execute(
                    "INSERT INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type)
                     VALUES ('item1', ?1, ?1, ?2)",
                    rusqlite::params![pa, kind],
                )
                .unwrap();
        }
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        assert_eq!(
            find(&items, Kind::UnconfiguredInvestment).map(|i| i.count),
            Some(1),
            "the chequing account is not an investment account"
        );
    }

    /// Only accounts a statement actually arrives for. An account nobody banks at
    /// cannot be reconciled against anything, and counting it would leave the panel
    /// permanently wrong.
    #[test]
    fn only_bank_accounts_are_counted_as_never_reconciled() {
        let mut store = store();
        let petty = AccountCommands::new(&mut store, "u".into())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1050".into(),
                name: "Petty cash".into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            })
            .expect("account");
        let _ = petty;
        let checking: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE name = 'Business Checking'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        link(&store, "item1", "Bank", Some("2026-09-30"));
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', 'pa1', 'Checking', 'depository', ?1)",
                [&checking],
            )
            .unwrap();
        // A card, which a statement does arrive for, and an investment account, whose
        // reconciliation is the holdings comparison and not a statement.
        let card = AccountCommands::new(&mut store, "u".into())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Liability,
                account_number: "2100".into(),
                name: "Visa".into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            })
            .expect("account");
        let card_id = match &card.event {
            Event::AccountCreated { account_id, .. } => account_id.clone(),
            _ => unreachable!(),
        };
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', 'pa2', 'Visa', 'credit', ?1)",
                [&card_id],
            )
            .unwrap();
        let brokerage = AccountCommands::new(&mut store, "u".into())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1200".into(),
                name: "Brokerage".into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            })
            .expect("account");
        let brokerage_id = match &brokerage.event {
            Event::AccountCreated { account_id, .. } => account_id.clone(),
            _ => unreachable!(),
        };
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', 'pa3', 'Brokerage', 'investment', ?1)",
                [&brokerage_id],
            )
            .unwrap();

        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        assert_eq!(
            find(&items, Kind::NeverReconciled).map(|i| i.count),
            Some(2),
            "the chequing account and the card; not the petty cash nobody banks at"
        );

        // Once one is completed, it stops being counted; an abandoned one does not.
        store
            .connection()
            .execute(
                "INSERT INTO reconciliations
                    (id, account_id, statement_date, statement_ending_balance, status)
                 VALUES ('r1', ?1, '2026-09-30', 0, 'completed')",
                [&checking],
            )
            .unwrap();
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        assert_eq!(
            find(&items, Kind::NeverReconciled).map(|i| i.count),
            Some(1),
            "the card is still waiting"
        );

        // And one merely *started* does not count: the figure it was started to check
        // has not been checked, which is exactly the state this item is about.
        store
            .connection()
            .execute(
                "INSERT INTO reconciliations
                    (id, account_id, statement_date, statement_ending_balance, status)
                 VALUES ('r2', ?1, '2026-09-30', 0, 'in_progress')",
                [&card_id],
            )
            .unwrap();
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        assert_eq!(
            find(&items, Kind::NeverReconciled).map(|i| i.count),
            Some(1),
            "started is not reconciled"
        );
    }

    /// A reconciliation left open holds a lock on the account and hides the figure it
    /// was started to check.
    #[test]
    fn a_reconciliation_left_open_is_waiting() {
        let store = store();
        let checking: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE name = 'Business Checking'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO reconciliations
                    (id, account_id, statement_date, statement_ending_balance, status)
                 VALUES ('r1', ?1, '2026-09-30', 0, 'in_progress')",
                [&checking],
            )
            .unwrap();
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        assert_eq!(
            find(&items, Kind::OpenReconciliation).map(|i| i.count),
            Some(1)
        );
    }

    /// The order is the order the work runs in, not a severity ranking: a feed that is
    /// behind comes first because every count below it is computed from data it has
    /// not delivered, and reconciling comes last because it is the check on the rest.
    #[test]
    fn the_items_come_in_the_order_the_work_runs_in() {
        let store = store();
        link(&store, "item1", "Bank", None);
        let checking: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE name = 'Business Checking'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', 'pa1', 'Checking', 'depository', ?1)",
                [&checking],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO plaid_staged_transactions
                    (id, item_id, plaid_transaction_id, plaid_account_id, amount_cents, date, name)
                 VALUES ('s1', 'item1', 't1', 'pa1', 100, '2026-09-01', 'Shop')",
                [],
            )
            .unwrap();
        let items = whats_waiting(store.connection(), day(2026, 10, 1));
        assert_eq!(
            kinds(&items),
            vec![
                Kind::StaleFeed,
                Kind::StagedTransactions,
                Kind::NeverReconciled
            ]
        );
    }

    /// A book from before the bank feed existed has no feed tables. Every count has to
    /// be zero rather than an error, or the dashboard cannot open at all.
    #[test]
    fn a_book_without_the_feed_tables_is_simply_quiet() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id TEXT PRIMARY KEY, name TEXT);
             CREATE TABLE journal_entries (id TEXT PRIMARY KEY, is_void INTEGER);
             CREATE TABLE journal_lines (id TEXT, entry_id TEXT, account_id TEXT);",
        )
        .unwrap();
        assert_eq!(whats_waiting(&conn, day(2026, 10, 1)), vec![]);
    }
}
