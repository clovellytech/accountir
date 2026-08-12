//! Recurring transfer rules and their runner.
//!
//! A rule says "on day N of each month, move `source`'s balance (or a fixed
//! amount) to `dest`". The classic case is a business credit card whose employee
//! sub-card is paid through the parent account: every month the employee card's
//! balance is shifted onto the parent, which is then paid down from the bank.
//!
//! Nothing here posts automatically. [`due_transfers`] computes the periods that
//! are due but not yet posted (walking from the rule's `start_month` to today),
//! and the UI shows them for the user to confirm. [`post_proposed`] then posts
//! one as an ordinary balanced journal entry (source `Recurring`) with a
//! deterministic reference `recurring:<rule_id>:<YYYY-MM>` — so confirming the
//! same period twice, or re-running a backfill, is a no-op.
//!
//! `full_balance` mode zeroes the source into the destination. Because each
//! period reads the source balance *as of that period's run date*, posting the
//! periods oldest-first moves only that month's incremental change — which is
//! exactly the monthly "shift the new spend to the parent" behaviour. The
//! previews returned by [`due_transfers`] simulate that same oldest-first
//! sequence, so what you see is what posts.

use chrono::{Datelike, NaiveDate};
use rusqlite::Connection;
use uuid::Uuid;

use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use crate::events::types::{JournalEntrySource, StoredEvent};
use crate::queries::account_queries::AccountQueries;
use crate::store::event_store::EventStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AmountMode {
    /// Zero the source account into the destination (move its whole balance).
    #[default]
    FullBalance,
    /// Move a fixed amount every period.
    Fixed,
}

impl AmountMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AmountMode::FullBalance => "full_balance",
            AmountMode::Fixed => "fixed",
        }
    }
    fn from_str(s: &str) -> Self {
        match s {
            "fixed" => AmountMode::Fixed,
            _ => AmountMode::FullBalance,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecurringTransferRule {
    pub id: String,
    pub source_account_id: String,
    pub dest_account_id: String,
    pub day_of_month: u32,
    pub amount_mode: AmountMode,
    pub fixed_amount_cents: Option<i64>,
    pub memo: String,
    /// First period considered, inclusive, as "YYYY-MM".
    pub start_month: String,
    pub active: bool,
}

/// One period's worth of transfer, computed but not yet posted.
#[derive(Debug, Clone)]
pub struct ProposedTransfer {
    pub rule_id: String,
    /// "YYYY-MM".
    pub period: String,
    pub date: NaiveDate,
    pub source_account_id: String,
    pub dest_account_id: String,
    /// Debit-positive amount on the *source* line; the destination line is its
    /// negation. Positive here debits the source (e.g. pays down a liability).
    pub source_amount_cents: i64,
    pub memo: String,
    pub reference: String,
}

impl ProposedTransfer {
    /// Magnitude of money moved, for display.
    pub fn magnitude_cents(&self) -> i64 {
        self.source_amount_cents.abs()
    }
}

// ---------------------------------------------------------------------------
// Rule CRUD (plain config table, like vendor_account_rules)
// ---------------------------------------------------------------------------

pub fn list_rules(conn: &Connection) -> Vec<RecurringTransferRule> {
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT id, source_account_id, dest_account_id, day_of_month, amount_mode,
                fixed_amount_cents, memo, start_month, active
         FROM recurring_transfer_rules
         ORDER BY created_at",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok(RecurringTransferRule {
                id: r.get(0)?,
                source_account_id: r.get(1)?,
                dest_account_id: r.get(2)?,
                day_of_month: r.get::<_, i64>(3)? as u32,
                amount_mode: AmountMode::from_str(&r.get::<_, String>(4)?),
                fixed_amount_cents: r.get(5)?,
                memo: r.get(6)?,
                start_month: r.get(7)?,
                active: r.get::<_, i64>(8)? != 0,
            })
        }) {
            out.extend(rows.flatten());
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub fn create_rule(
    conn: &Connection,
    source_account_id: &str,
    dest_account_id: &str,
    day_of_month: u32,
    amount_mode: AmountMode,
    fixed_amount_cents: Option<i64>,
    memo: &str,
    start_month: &str,
) -> Result<String, rusqlite::Error> {
    let id = Uuid::new_v4().to_string();
    let day = day_of_month.clamp(1, 31) as i64;
    conn.execute(
        "INSERT INTO recurring_transfer_rules
            (id, source_account_id, dest_account_id, day_of_month, amount_mode,
             fixed_amount_cents, memo, start_month, active, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, datetime('now'))",
        rusqlite::params![
            id,
            source_account_id,
            dest_account_id,
            day,
            amount_mode.as_str(),
            fixed_amount_cents,
            memo.trim(),
            start_month,
        ],
    )?;
    Ok(id)
}

pub fn set_active(conn: &Connection, id: &str, active: bool) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE recurring_transfer_rules SET active = ?2 WHERE id = ?1",
        rusqlite::params![id, active as i64],
    )?;
    Ok(())
}

pub fn delete_rule(conn: &Connection, id: &str) -> Result<(), rusqlite::Error> {
    conn.execute("DELETE FROM recurring_transfer_rules WHERE id = ?1", [id])?;
    Ok(())
}

/// The month ("YYYY-MM") of the earliest non-void entry touching `account_id`,
/// a sensible default `start_month` for a new rule over that account.
pub fn earliest_activity_month(conn: &Connection, account_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT substr(MIN(je.date), 1, 7)
         FROM journal_lines jl JOIN journal_entries je ON je.id = jl.entry_id
         WHERE jl.account_id = ?1 AND je.is_void = 0",
        [account_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// Last calendar day of the given year/month (28–31).
fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let first_next = NaiveDate::from_ymd_opt(ny, nm, 1).unwrap();
    first_next.pred_opt().unwrap().day()
}

/// The run date for a period: `day_of_month` clamped to the month's length.
fn run_date(year: i32, month: u32, day_of_month: u32) -> NaiveDate {
    let day = day_of_month.min(last_day_of_month(year, month)).max(1);
    NaiveDate::from_ymd_opt(year, month, day).unwrap()
}

/// Whether a non-void entry with this reference already exists.
fn reference_posted(conn: &Connection, reference: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM journal_entries WHERE reference = ?1 AND is_void = 0 LIMIT 1",
        [reference],
        |_| Ok(()),
    )
    .is_ok()
}

fn account_balance_as_of(conn: &Connection, account_id: &str, date: NaiveDate) -> i64 {
    AccountQueries::new(conn)
        .get_account_balance(account_id, Some(date))
        .map(|b| b.balance)
        .unwrap_or(0)
}

/// Parse "YYYY-MM" into (year, month).
fn parse_month(s: &str) -> Option<(i32, u32)> {
    let mut it = s.split('-');
    let y = it.next()?.parse::<i32>().ok()?;
    let m = it.next()?.parse::<u32>().ok()?;
    if (1..=12).contains(&m) {
        Some((y, m))
    } else {
        None
    }
}

/// Compute the transfers a rule is due for but hasn't posted yet, from its
/// `start_month` through the current month, in oldest-first order.
///
/// `today` bounds the run — periods whose run date is in the future are skipped.
/// Zero-movement periods (no change to the source) are omitted.
pub fn due_transfers(
    conn: &Connection,
    rule: &RecurringTransferRule,
    today: NaiveDate,
) -> Vec<ProposedTransfer> {
    let Some((sy, sm)) = parse_month(&rule.start_month) else {
        return Vec::new();
    };
    let mut proposals = Vec::new();
    // Sum of source-line amounts we've proposed this pass but not yet posted, so
    // full-balance periods after an unposted one still net to the right amount.
    let mut pending: i64 = 0;

    let (mut y, mut m) = (sy, sm);
    loop {
        // Stop once we pass the current month.
        if (y, m) > (today.year(), today.month()) {
            break;
        }
        let date = run_date(y, m, rule.day_of_month);
        if date <= today {
            // Keyed on the account pair + period, NOT the rule id — so the same
            // monthly shift has one stable reference no matter which rule (or
            // which machine, or a historical backfill) produces it. That makes
            // the whole thing idempotent across databases: a period booked by a
            // one-off backfill won't be re-proposed by a rule created later.
            let reference = format!(
                "recurring:{}:{}:{:04}-{:02}",
                rule.source_account_id, rule.dest_account_id, y, m
            );
            if reference_posted(conn, &reference) {
                // Already booked; its effect is in the ledger, don't re-propose
                // and don't touch `pending`.
            } else {
                let source_amount = match rule.amount_mode {
                    AmountMode::FullBalance => {
                        // Zero the source as of the run date, accounting for
                        // earlier unposted proposals in this pass.
                        let bal = account_balance_as_of(conn, &rule.source_account_id, date);
                        -(bal + pending)
                    }
                    AmountMode::Fixed => rule.fixed_amount_cents.unwrap_or(0),
                };
                if source_amount != 0 {
                    let memo = if rule.memo.trim().is_empty() {
                        format!("Recurring transfer {:04}-{:02}", y, m)
                    } else {
                        rule.memo.trim().to_string()
                    };
                    proposals.push(ProposedTransfer {
                        rule_id: rule.id.clone(),
                        period: format!("{:04}-{:02}", y, m),
                        date,
                        source_account_id: rule.source_account_id.clone(),
                        dest_account_id: rule.dest_account_id.clone(),
                        source_amount_cents: source_amount,
                        memo,
                        reference,
                    });
                    pending += source_amount;
                }
            }
        }
        // Advance one month.
        if m == 12 {
            y += 1;
            m = 1;
        } else {
            m += 1;
        }
    }
    proposals
}

/// Post one proposed transfer as a balanced journal entry (source `Recurring`).
/// The deterministic reference makes this idempotent: a period already posted
/// comes back as an `AlreadyExists`-style error from the command, which the
/// caller can treat as "already done".
pub fn post_proposed(
    store: &mut EventStore,
    user_id: &str,
    p: &ProposedTransfer,
) -> Result<StoredEvent, crate::commands::entry_commands::EntryCommandError> {
    let mut cmds = EntryCommands::new(store, user_id.to_string());
    cmds.post_entry(PostEntryCommand {
        date: p.date,
        memo: p.memo.clone(),
        lines: vec![
            EntryLine {
                account_id: p.source_account_id.clone(),
                amount: p.source_amount_cents,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
            EntryLine {
                account_id: p.dest_account_id.clone(),
                amount: -p.source_amount_cents,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
        ],
        reference: Some(p.reference.clone()),
        source: Some(JournalEntrySource::Recurring),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    fn mk_account(store: &mut EventStore, id: &str, number: &str, name: &str, ty: &str) {
        store
            .connection()
            .execute(
                "INSERT INTO accounts (id, account_number, name, account_type, is_active)
                 VALUES (?1, ?2, ?3, ?4, 1)",
                rusqlite::params![id, number, name, ty],
            )
            .unwrap();
    }

    fn post(store: &mut EventStore, date: &str, memo: &str, lines: Vec<EntryLine>, reference: Option<String>) {
        let mut cmds = EntryCommands::new(store, "test".to_string());
        cmds.post_entry(PostEntryCommand {
            date: NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
            memo: memo.to_string(),
            lines,
            reference,
            source: None,
        })
        .unwrap();
    }

    #[test]
    fn full_balance_shifts_monthly_increment_oldest_first() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        migrations_ok(&store);
        mk_account(&mut store, "card", "2002", "Employee card", "liability");
        mk_account(&mut store, "parent", "2001", "Parent card", "liability");
        mk_account(&mut store, "exp", "5000", "Expense", "expense");

        // Two months of employee-card charges (credit the liability, debit exp).
        post(&mut store, "2026-01-10", "jan charge",
            vec![EntryLine::debit("exp", 10_000, "USD"), EntryLine::credit("card", 10_000, "USD")], None);
        post(&mut store, "2026-02-05", "feb charge",
            vec![EntryLine::debit("exp", 4_000, "USD"), EntryLine::credit("card", 4_000, "USD")], None);

        let rule = RecurringTransferRule {
            id: "r1".into(),
            source_account_id: "card".into(),
            dest_account_id: "parent".into(),
            day_of_month: 22,
            amount_mode: AmountMode::FullBalance,
            fixed_amount_cents: None,
            memo: "Shift employee card to parent".into(),
            start_month: "2026-01".into(),
            active: true,
        };

        let today = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        let due = due_transfers(store.connection(), &rule, today);
        // Jan run date 2026-01-22 sees 10_000; Feb run date 2026-02-22 sees the
        // additional 4_000. Each source line is debit-positive (pays the card down).
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].period, "2026-01");
        assert_eq!(due[0].source_amount_cents, 10_000);
        assert_eq!(due[1].period, "2026-02");
        assert_eq!(due[1].source_amount_cents, 4_000);

        // Post them; the card zeroes and the parent carries the balance.
        for p in &due {
            post_proposed(&mut store, "test", p).unwrap();
        }
        let conn = store.connection();
        assert_eq!(account_balance_as_of(conn, "card", today), 0);
        assert_eq!(account_balance_as_of(conn, "parent", today), -14_000);

        // Re-running proposes nothing (idempotent via reference).
        assert!(due_transfers(conn, &rule, today).is_empty());
    }

    fn migrations_ok(store: &EventStore) {
        // The rules table lives in a migration, not init_schema; create it here.
        store
            .connection()
            .execute_batch(include_str!("../../migrations/019_recurring_transfers.sql"))
            .unwrap();
    }
}
