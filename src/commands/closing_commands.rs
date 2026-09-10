//! Closing the books at year end: sweep revenue and expense into equity, and
//! fence the year against further posting.
//!
//! # The shape of a close
//!
//! One journal entry, dated the last day of the fiscal year, with a debit per
//! revenue account, a credit per expense account, and one balancing line to an
//! equity account named for the year — conventionally `Equity:Years:2023`. The
//! year's income statement is then zero going forward, and the balance sheet
//! carries the result as a real account rather than a figure the reports have to
//! keep recomputing.
//!
//! The textbook close routes everything through an "Income Summary" account
//! first. That exists because a bookkeeper cannot write a hundred-line entry by
//! hand. We can, and one entry is one thing to read, one thing to void and one
//! thing to audit.
//!
//! # Why the entry and the lock are one append
//!
//! The closing entry is dated inside the very year it closes, so the fence that
//! closing raises would refuse it. Posting first and locking second leaves a
//! window where the books are half-closed; locking first makes the entry
//! impossible. Both events therefore go through [`EventStore::append_checked_many`]
//! in a single transaction: the invariant check runs against the write-locked
//! state *before* either is applied, so the fence sees the year still open and
//! admits the entry, and the two land together or not at all.
//!
//! # Two different questions about a balance
//!
//! This module asks what an account's *ledger balance* is — closing entries
//! included — because prior years' closing entries are exactly what zeroed those
//! years. [`AccountQueries::period_movement`], used by the income statement,
//! asks the opposite: what an account did in a window *ignoring* closing
//! entries, because counting them would report a closed year as having earned
//! nothing.
//!
//! Both are right for their own question, and swapping one for the other breaks
//! something quietly. The guard in [`opening_balances_are_clear`] is what keeps
//! the first question answerable: it refuses to close a year whose revenue and
//! expense accounts did not start at zero, because the sweep would then
//! attribute an earlier year's earnings to this one.

use std::collections::HashMap;

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

use crate::commands::entry_commands::{
    build_post_entry_in_txn, EntryLine, PostEntryCommand, PostEntryStep, VoidEntryCommand,
};
use crate::commands::fiscal_year_commands::{boundaries_for, load_year, FiscalYearCommands};
use crate::domain::AccountType;
use crate::events::types::{Event, EventEnvelope, JournalEntrySource};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;

#[derive(Error, Debug)]
pub enum ClosingError {
    #[error("Event store error: {0}")]
    Store(#[from] EventStoreError),
    #[error("Query error: {0}")]
    Query(#[from] crate::queries::account_queries::AccountQueryError),
    #[error("Fiscal year error: {0}")]
    FiscalYear(#[from] crate::commands::fiscal_year_commands::FiscalYearCommandError),
    #[error("Posting the closing entry failed: {0}")]
    Entry(String),

    #[error("{year} has no revenue or expense activity — there is nothing to close")]
    NothingToClose { year: i32 },
    #[error(
        "The trial balance for {year} does not balance: debits {debits}, credits {credits}. \
         Closing a year that does not balance would carry the discrepancy into equity."
    )]
    Unbalanced {
        year: i32,
        debits: i64,
        credits: i64,
    },
    #[error(
        "{account} still holds {}{}.{:02} but has been deactivated. Reactivate it so the close \
         can sweep it, or the balance is stranded once {year} is fenced.",
        if *cents < 0 { "-" } else { "" }, cents.abs() / 100, cents.abs() % 100
    )]
    InactiveAccountHoldsBalance {
        year: i32,
        account: String,
        cents: i64,
    },
    #[error(
        "Revenue and expense did not start {year} at zero — {account} was carrying {}{}.{:02} \
         on {opening_date}. That is earlier activity which has never been closed, and sweeping it \
         now would record it as {year}'s. Close the earlier year first.",
        if *cents < 0 { "-" } else { "" }, cents.abs() / 100, cents.abs() % 100
    )]
    PriorActivityNotClosed {
        year: i32,
        account: String,
        cents: i64,
        opening_date: NaiveDate,
    },
    #[error("{year} is already closed by entry {entry_id}")]
    AlreadyClosed { year: i32, entry_id: String },
    #[error("{year} is not closed")]
    NotClosed { year: i32 },
    #[error("The account to close into does not exist")]
    EquityAccountMissing,
    #[error("{0} is not an equity account — a year's result has to land in equity")]
    EquityAccountWrongType(String),
}

/// One account the close will sweep.
#[derive(Debug, Clone)]
pub struct SweptAccount {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub account_type: AccountType,
    /// The ledger balance being swept, signed as the ledger holds it: negative
    /// for a credit balance (ordinary revenue), positive for a debit balance
    /// (ordinary expense).
    pub balance_cents: i64,
    pub is_active: bool,
}

impl SweptAccount {
    /// The line that takes this account to zero: the negation of its balance.
    fn closing_line(&self, currency: &str) -> EntryLine {
        EntryLine::signed(&self.account_id, -self.balance_cents, currency)
            .with_memo(&format!("Closing {}", self.account_name))
    }
}

/// What a close would do, computed without doing it.
#[derive(Debug, Clone)]
pub struct ClosingPreview {
    pub year: i32,
    pub year_start: NaiveDate,
    pub year_end: NaiveDate,
    pub revenue: Vec<SweptAccount>,
    pub expenses: Vec<SweptAccount>,
    pub draws: Vec<SweptAccount>,
    /// Positive for a profit, negative for a loss.
    pub net_income_cents: i64,
    pub trial_balance_debits: i64,
    pub trial_balance_credits: i64,
    pub trial_balance_ok: bool,
    /// The live closing entry, if this year is already closed.
    pub closed_by: Option<String>,
    /// Things worth saying that are not refusals.
    pub warnings: Vec<String>,
    /// The refusal this close would hit, rendered as a sentence. `None` if it
    /// would go through.
    pub blocker: Option<String>,
}

impl ClosingPreview {
    pub fn is_closed(&self) -> bool {
        self.closed_by.is_some()
    }

    /// Every account the entry will touch, revenue then expense then draws.
    pub fn swept(&self) -> impl Iterator<Item = &SweptAccount> {
        self.revenue
            .iter()
            .chain(self.expenses.iter())
            .chain(self.draws.iter())
    }
}

/// Command to close a fiscal year.
#[derive(Debug, Clone)]
pub struct CloseBooksCommand {
    pub year: i32,
    /// Where the year's result lands. Resolved — and created, if the caller
    /// wants a path like `Equity:Years:2023` that does not exist yet — before
    /// this command is called.
    pub equity_account_id: String,
    /// Also sweep partner draw accounts into the year's result. Off by default:
    /// leaving draws as their own equity line is the more readable balance
    /// sheet, and for a partnership they feed Schedule K-1 item L.
    pub include_draws: bool,
}

/// What a close did.
#[derive(Debug, Clone)]
pub struct Closed {
    pub year: i32,
    pub entry_id: String,
    pub net_income_cents: i64,
    pub accounts_swept: usize,
}

/// The idempotency key a year's closing entry carries.
///
/// Migration 014's partial unique index over live references is what actually
/// stops a year being closed twice — not a check this module has to remember to
/// make. Voiding the entry frees the reference, which is what lets a reopened
/// year be closed again.
pub fn reference_for(year: i32) -> String {
    format!("close-{year}")
}

/// The live closing entry for a year, if one is posted.
pub fn closing_entry_for(conn: &Connection, year: i32) -> Option<String> {
    conn.query_row(
        "SELECT id FROM journal_entries WHERE reference = ?1 AND is_void = 0",
        [reference_for(year)],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

fn base_currency(conn: &Connection) -> String {
    conn.query_row("SELECT base_currency FROM company LIMIT 1", [], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
    .unwrap_or_else(|| "USD".to_string())
}

/// Every account of the given types, with its ledger balance as of `as_of`.
///
/// Deliberately **not** filtered to active accounts. An account deactivated
/// part-way through a year still holds whatever was posted to it, and a close
/// built on `get_active_accounts` would fence the year with that balance
/// stranded inside it — unreachable, because the fence then refuses every entry
/// that could clear it.
fn accounts_with_balances(
    conn: &Connection,
    types: &[AccountType],
    as_of: NaiveDate,
    only: Option<&[String]>,
) -> Result<Vec<SweptAccount>, ClosingError> {
    let type_names: Vec<&str> = types
        .iter()
        .map(|t| match t {
            AccountType::Asset => "asset",
            AccountType::Liability => "liability",
            AccountType::Equity => "equity",
            AccountType::Revenue => "revenue",
            AccountType::Expense => "expense",
        })
        .collect();
    let placeholders = type_names
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");

    // The balance includes closing entries: a prior year's closing entry is what
    // took that year's revenue back to zero, and excluding it would resurrect it.
    let sql = format!(
        "SELECT a.id, a.account_number, a.name, a.account_type, a.is_active,
                COALESCE((SELECT SUM(jl.amount) FROM journal_lines jl
                          JOIN journal_entries je ON jl.entry_id = je.id
                          WHERE jl.account_id = a.id AND je.is_void = 0
                            AND je.date <= ?1), 0) AS balance
           FROM accounts a
          WHERE a.account_type IN ({placeholders})
          ORDER BY a.account_number"
    );

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(as_of.to_string())];
    for name in &type_names {
        params.push(Box::new(name.to_string()));
    }
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn.prepare(&sql).map_err(EventStoreError::from)?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            let type_str: String = row.get(3)?;
            Ok(SweptAccount {
                account_id: row.get(0)?,
                account_number: row.get(1)?,
                account_name: row.get(2)?,
                account_type: match type_str.as_str() {
                    "asset" => AccountType::Asset,
                    "liability" => AccountType::Liability,
                    "equity" => AccountType::Equity,
                    "revenue" => AccountType::Revenue,
                    _ => AccountType::Expense,
                },
                is_active: row.get::<_, i64>(4)? == 1,
                balance_cents: row.get(5)?,
            })
        })
        .map_err(EventStoreError::from)?;

    let mut out = Vec::new();
    for row in rows {
        let account = row.map_err(EventStoreError::from)?;
        if let Some(ids) = only {
            if !ids.contains(&account.account_id) {
                continue;
            }
        }
        if account.balance_cents != 0 {
            out.push(account);
        }
    }
    Ok(out)
}

/// Do the books balance as of `as_of`? Returns `(debits, credits)`.
///
/// Deliberately not [`Reports::trial_balance`], which builds itself from
/// `get_active_accounts` and so drops any account that has been deactivated
/// while still holding a balance — making perfectly sound books report as
/// unbalanced, and reporting the *wrong reason* for refusing the close. Debits
/// equalling credits is a property of the journal lines themselves, so it is
/// read from them directly and the chart's flags cannot affect the answer.
fn trial_balance_at(conn: &Connection, as_of: NaiveDate) -> Result<(i64, i64), ClosingError> {
    let (debits, credits): (i64, i64) = conn
        .query_row(
            "SELECT COALESCE(SUM(CASE WHEN jl.amount > 0 THEN jl.amount ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN jl.amount < 0 THEN -jl.amount ELSE 0 END), 0)
               FROM journal_lines jl
               JOIN journal_entries je ON jl.entry_id = je.id
              WHERE je.is_void = 0 AND je.date <= ?1",
            [as_of.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(EventStoreError::from)?;
    Ok((debits, credits))
}

/// The equity accounts linked to a partner in the "draw" role.
fn draw_account_ids(conn: &Connection) -> Vec<String> {
    let mut stmt = match conn
        .prepare("SELECT account_id FROM partner_equity_accounts WHERE role = 'draw'")
    {
        Ok(s) => s,
        // The table is absent on a ledger predating migration 037; no draws.
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// Refuse if revenue or expense did not start the year at zero.
///
/// See the module docs: the sweep takes each account's whole ledger balance, so
/// an account carrying an earlier year's earnings into this one would have them
/// recorded as this year's. Names the first offender rather than listing them —
/// the fix (close the earlier year) is the same for all of them.
fn opening_balances_are_clear(
    conn: &Connection,
    year: i32,
    year_start: NaiveDate,
) -> Result<(), ClosingError> {
    let Some(day_before) = year_start.pred_opt() else {
        return Ok(());
    };
    let opening = accounts_with_balances(
        conn,
        &[AccountType::Revenue, AccountType::Expense],
        day_before,
        None,
    )?;
    if let Some(a) = opening.first() {
        return Err(ClosingError::PriorActivityNotClosed {
            year,
            account: format!("{} {}", a.account_number, a.account_name),
            cents: a.balance_cents,
            opening_date: day_before,
        });
    }
    Ok(())
}

/// Compute what closing `year` would do. A pure read — safe on a replica, and
/// safe to call from a view function.
pub fn preview(
    conn: &Connection,
    year: i32,
    include_draws: bool,
) -> Result<ClosingPreview, ClosingError> {
    let fy = load_year(conn, year)?.unwrap_or_else(|| boundaries_for(conn, year));
    let (year_start, year_end) = (fy.start_date, fy.end_date);

    let revenue = accounts_with_balances(conn, &[AccountType::Revenue], year_end, None)?;
    let expenses = accounts_with_balances(conn, &[AccountType::Expense], year_end, None)?;
    let draws = if include_draws {
        let ids = draw_account_ids(conn);
        accounts_with_balances(conn, &[AccountType::Equity], year_end, Some(&ids))?
    } else {
        Vec::new()
    };

    // Net income is the negation of the swept balances: revenue sits credit
    // (negative), expense debit (positive), so `-(sum)` is revenue less expenses.
    let sweep_total: i64 = revenue
        .iter()
        .chain(expenses.iter())
        .map(|a| a.balance_cents)
        .sum();
    let net_income_cents = -sweep_total;

    let (debits, credits) = trial_balance_at(conn, year_end)?;

    let mut warnings = Vec::new();
    if include_draws && draws.is_empty() {
        warnings.push(
            "Closing draws was asked for, but no account is linked to a partner in the \"draw\" \
             role, so none will be swept. Link them on the Partners page first."
                .to_string(),
        );
    }
    for a in revenue.iter().chain(expenses.iter()) {
        if !a.is_active {
            warnings.push(format!(
                "{} {} is deactivated but still holds a balance; it has to be reactivated \
                 before the year can be closed.",
                a.account_number, a.account_name
            ));
        }
    }
    if let Some(stale) =
        crate::commands::depreciation_commands::posting_is_stale(conn, year)
    {
        warnings.push(stale);
    }

    let closed_by = closing_entry_for(conn, year);
    let blocker = match closed_by {
        Some(ref entry_id) => Some(
            ClosingError::AlreadyClosed {
                year,
                entry_id: entry_id.clone(),
            }
            .to_string(),
        ),
        None => check_can_close(
            conn,
            year,
            year_start,
            &revenue,
            &expenses,
            (debits, credits),
        )
        .err()
        .map(|e| e.to_string()),
    };

    Ok(ClosingPreview {
        year,
        year_start,
        year_end,
        revenue,
        expenses,
        draws,
        net_income_cents,
        trial_balance_debits: debits,
        trial_balance_credits: credits,
        trial_balance_ok: debits == credits,
        closed_by,
        warnings,
        blocker,
    })
}

/// Every refusal a close can hit, in one place, so the preview and the command
/// cannot drift apart about what is allowed.
fn check_can_close(
    conn: &Connection,
    year: i32,
    year_start: NaiveDate,
    revenue: &[SweptAccount],
    expenses: &[SweptAccount],
    (debits, credits): (i64, i64),
) -> Result<(), ClosingError> {
    if revenue.is_empty() && expenses.is_empty() {
        return Err(ClosingError::NothingToClose { year });
    }
    // Before the trial balance, because a deactivated account holding a balance
    // is both the more specific diagnosis and the more actionable one.
    for a in revenue.iter().chain(expenses.iter()) {
        if !a.is_active {
            return Err(ClosingError::InactiveAccountHoldsBalance {
                year,
                account: format!("{} {}", a.account_number, a.account_name),
                cents: a.balance_cents,
            });
        }
    }
    if debits != credits {
        return Err(ClosingError::Unbalanced {
            year,
            debits,
            credits,
        });
    }
    opening_balances_are_clear(conn, year, year_start)
}

/// Close the books for a year: post the closing entry and fence the year, in one
/// atomic append.
pub fn close_books(
    store: &mut EventStore,
    user_id: &str,
    cmd: CloseBooksCommand,
) -> Result<Closed, ClosingError> {
    // The year must have a `fiscal_years` row for the fence to key off. No
    // ledger has ever had one, so this is the ordinary path, not the edge case.
    FiscalYearCommands::new(store, user_id.to_string()).ensure_year_open(cmd.year)?;

    let currency = base_currency(store.connection());

    let equity_account_id = cmd.equity_account_id.clone();

    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let user_id = user_id.to_string();
        let cmd = cmd.clone();
        let currency = currency.clone();

        let outcome = store.append_checked_many(
            head,
            move |tx| {
                // Everything is re-derived under the write lock: another writer
                // may have posted into the year between the preview the user saw
                // and this append.
                let equity: Option<(String, bool)> = tx
                    .query_row(
                        "SELECT account_type, is_active = 1 FROM accounts WHERE id = ?1",
                        [&cmd.equity_account_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                match equity {
                    None => return Ok(Verdict::Reject(ClosingError::EquityAccountMissing)),
                    Some((ty, _)) if ty != "equity" => {
                        return Ok(Verdict::Reject(ClosingError::EquityAccountWrongType(
                            cmd.equity_account_id.clone(),
                        )))
                    }
                    Some(_) => {}
                }

                let fy = match load_year(tx, cmd.year)? {
                    Some(fy) => fy,
                    None => boundaries_for(tx, cmd.year),
                };
                if let Some(entry_id) = closing_entry_for(tx, cmd.year) {
                    return Ok(Verdict::Reject(ClosingError::AlreadyClosed {
                        year: cmd.year,
                        entry_id,
                    }));
                }

                let revenue =
                    match accounts_with_balances(tx, &[AccountType::Revenue], fy.end_date, None) {
                        Ok(v) => v,
                        Err(e) => return Ok(Verdict::Reject(e)),
                    };
                let expenses =
                    match accounts_with_balances(tx, &[AccountType::Expense], fy.end_date, None) {
                        Ok(v) => v,
                        Err(e) => return Ok(Verdict::Reject(e)),
                    };
                let draws = if cmd.include_draws {
                    let ids = draw_account_ids(tx);
                    match accounts_with_balances(tx, &[AccountType::Equity], fy.end_date, Some(&ids))
                    {
                        Ok(v) => v,
                        Err(e) => return Ok(Verdict::Reject(e)),
                    }
                } else {
                    Vec::new()
                };

                let tb = match trial_balance_at(tx, fy.end_date) {
                    Ok(tb) => tb,
                    Err(e) => return Ok(Verdict::Reject(e)),
                };
                if let Err(e) =
                    check_can_close(tx, cmd.year, fy.start_date, &revenue, &expenses, tb)
                {
                    return Ok(Verdict::Reject(e));
                }

                // Lines: each account back to zero, then the balancing figure to
                // equity. The equity line is the sum of what the others removed,
                // so the entry sums to zero by construction.
                let mut lines: Vec<EntryLine> = Vec::new();
                let mut sweep_total: i64 = 0;
                for account in revenue.iter().chain(expenses.iter()).chain(draws.iter()) {
                    sweep_total += account.balance_cents;
                    lines.push(account.closing_line(&currency));
                }
                let net_income_cents = -sweep_total;
                if sweep_total != 0 {
                    lines.push(
                        EntryLine::signed(&cmd.equity_account_id, sweep_total, &currency)
                            .with_memo(&format!("Net result for {}", cmd.year)),
                    );
                }

                let post = PostEntryCommand {
                    date: fy.end_date,
                    memo: memo_for(cmd.year, net_income_cents, lines.len()),
                    lines,
                    reference: Some(reference_for(cmd.year)),
                    source: Some(JournalEntrySource::Closing),
                };
                let entry_event = match build_post_entry_in_txn(tx, &post)? {
                    PostEntryStep::Append(event) => event,
                    PostEntryStep::Reject(e) => {
                        return Ok(Verdict::Reject(ClosingError::Entry(e.to_string())))
                    }
                };
                let entry_id = match &entry_event {
                    Event::JournalEntryPosted { entry_id, .. } => entry_id.clone(),
                    other => {
                        return Ok(Verdict::Reject(ClosingError::Entry(format!(
                            "building the closing entry produced a {}",
                            other.event_type()
                        ))))
                    }
                };

                // The entry, then the lock that names it. One transaction, so the
                // fence above saw the year open and this one closes it.
                Ok(Verdict::Append(vec![
                    EventEnvelope::new(entry_event, user_id.clone()),
                    EventEnvelope::new(
                        Event::YearEndClosed {
                            year: cmd.year,
                            retained_earnings_entry_id: entry_id,
                        },
                        user_id.clone(),
                    ),
                ]))
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;

        match outcome {
            CheckedOutcome::Appended(events) => {
                let entry_id = events
                    .iter()
                    .find_map(|s| match &s.event {
                        Event::JournalEntryPosted { entry_id, .. } => Some(entry_id.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        ClosingError::Entry("the close appended no journal entry".to_string())
                    })?;
                let lines = events
                    .iter()
                    .find_map(|s| match &s.event {
                        Event::JournalEntryPosted { lines, .. } => Some(lines.len()),
                        _ => None,
                    })
                    .unwrap_or(0);
                let net = net_income_from(store.connection(), &entry_id, &equity_account_id);
                return Ok(Closed {
                    year: cmd.year,
                    entry_id,
                    net_income_cents: net,
                    // The equity line is not a swept account.
                    accounts_swept: lines.saturating_sub(1),
                });
            }
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

/// Read the net result back off the posted entry, so what is reported is what
/// the ledger actually holds rather than what the caller computed.
fn net_income_from(conn: &Connection, entry_id: &str, equity_account_id: &str) -> i64 {
    conn.query_row(
        "SELECT COALESCE(SUM(amount), 0) FROM journal_lines
          WHERE entry_id = ?1 AND account_id = ?2",
        rusqlite::params![entry_id, equity_account_id],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
    .map(|equity_line| -equity_line)
    .unwrap_or(0)
}

fn memo_for(year: i32, net_income_cents: i64, lines: usize) -> String {
    let swept = lines.saturating_sub(1);
    let magnitude = format!(
        "{}.{:02}",
        net_income_cents.abs() / 100,
        net_income_cents.abs() % 100
    );
    if net_income_cents < 0 {
        format!("Closing entries for {year} — net loss {magnitude}, {swept} account(s) swept")
    } else {
        format!("Closing entries for {year} — net income {magnitude}, {swept} account(s) swept")
    }
}

/// Reopen a closed year: void the closing entry and lift the fence, in one
/// atomic append.
///
/// Voiding is what frees the `close-<year>` reference, so the year can be closed
/// again once whatever needed correcting has been corrected. The two events are
/// batched for the same reason the close batches its own: either state on its
/// own is wrong, and one of them — fence up, closing entry gone — is a year
/// nobody can fix.
pub fn reopen_books(
    store: &mut EventStore,
    user_id: &str,
    year: i32,
    reason: &str,
) -> Result<(), ClosingError> {
    let reason = reason.trim().to_string();
    if reason.is_empty() {
        return Err(ClosingError::Entry(
            "reopening a closed year needs a reason".to_string(),
        ));
    }

    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let user_id = user_id.to_string();
        let reason = reason.clone();

        let outcome = store.append_checked_many(
            head,
            move |tx| {
                let is_closed = load_year(tx, year)?.map(|fy| fy.is_closed).unwrap_or(false);
                let entry_id = closing_entry_for(tx, year);
                if !is_closed && entry_id.is_none() {
                    return Ok(Verdict::Reject(ClosingError::NotClosed { year }));
                }

                let mut events = Vec::new();
                if let Some(entry_id) = entry_id {
                    let void = VoidEntryCommand {
                        entry_id,
                        reason: format!("Reopening {year}: {reason}"),
                    };
                    match crate::commands::entry_commands::build_void_entry_in_txn(tx, &void)? {
                        PostEntryStep::Append(event) => {
                            events.push(EventEnvelope::new(event, user_id.clone()))
                        }
                        PostEntryStep::Reject(e) => {
                            return Ok(Verdict::Reject(ClosingError::Entry(e.to_string())))
                        }
                    }
                }
                events.push(EventEnvelope::new(
                    Event::YearEndReopened {
                        year,
                        reason: reason.clone(),
                        reopened_by_user_id: user_id.clone(),
                    },
                    user_id.clone(),
                ));
                Ok(Verdict::Append(events))
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;

        match outcome {
            CheckedOutcome::Appended(_) => return Ok(()),
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

/// The years this ledger has activity in, newest first, each with whether it is
/// closed. What the desktop's year picker lists.
pub fn years_with_activity(conn: &Connection) -> Vec<(i32, bool)> {
    let mut closed: HashMap<i32, bool> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT year, is_closed = 1 FROM fiscal_years") {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i32>(0)?, r.get::<_, bool>(1)?))) {
            closed.extend(rows.filter_map(|r| r.ok()));
        }
    }

    let mut years: Vec<i32> = Vec::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT CAST(strftime('%Y', date) AS INTEGER) AS y
           FROM journal_entries WHERE is_void = 0 ORDER BY y DESC",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| r.get::<_, i32>(0)) {
            years.extend(rows.filter_map(|r| r.ok()));
        }
    }
    for year in closed.keys() {
        if !years.contains(year) {
            years.push(*year);
        }
    }
    years.sort_unstable_by(|a, b| b.cmp(a));
    years
        .into_iter()
        .map(|y| (y, closed.get(&y).copied().unwrap_or(false)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommandError, EntryCommands};
    use crate::store::migrations::init_schema;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    struct Books {
        store: EventStore,
        cash: String,
        sales: String,
        refunds: String,
        rent: String,
        equity: String,
    }

    /// A ledger with one of each account the closing entry cares about.
    ///
    /// `refunds` is a *revenue* account that carries a debit balance — a contra
    /// account. It is here in the fixture rather than in one test because
    /// getting its side wrong is the failure that unbalances a closing entry,
    /// and it should be under every test that posts one.
    fn books() -> Books {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let mut store = store;

        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES ('c', 'c', 'Co', 'USD', 1)",
                [],
            )
            .unwrap();

        let specs = [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Revenue, "4000", "Sales"),
            (AccountType::Revenue, "4900", "Refunds"),
            (AccountType::Expense, "6100", "Rent"),
            (AccountType::Equity, "3023", "2023"),
        ];
        for (ty, number, name) in specs {
            AccountCommands::new(&mut store, "user".to_string())
                .create_account(CreateAccountCommand {
                    account_type: ty,
                    account_number: number.to_string(),
                    name: name.to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
        }
        let id = |store: &EventStore, n: &str| -> String {
            store
                .connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [n],
                    |r| r.get(0),
                )
                .unwrap()
        };
        Books {
            cash: id(&store, "1000"),
            sales: id(&store, "4000"),
            refunds: id(&store, "4900"),
            rent: id(&store, "6100"),
            equity: id(&store, "3023"),
            store,
        }
    }

    impl Books {
        /// Post a two-line entry: `debit` gains, `credit` gives up.
        fn post(&mut self, date: NaiveDate, debit: &str, credit: &str, cents: i64) {
            EntryCommands::new(&mut self.store, "user".to_string())
                .post_entry(PostEntryCommand {
                    date,
                    memo: "test".to_string(),
                    lines: vec![
                        EntryLine::debit(debit, cents, "USD"),
                        EntryLine::credit(credit, cents, "USD"),
                    ],
                    reference: None,
                    source: Some(JournalEntrySource::Manual),
                })
                .unwrap();
        }

        /// A year with 5,000 of sales, 200 of refunds against them, and 3,000 of
        /// rent: net income 1,800.
        fn ordinary_year(&mut self, year: i32) {
            let (cash, sales, refunds, rent) = (
                self.cash.clone(),
                self.sales.clone(),
                self.refunds.clone(),
                self.rent.clone(),
            );
            self.post(day(year, 3, 1), &cash, &sales, 500_000);
            self.post(day(year, 6, 1), &refunds, &cash, 20_000);
            self.post(day(year, 9, 1), &rent, &cash, 300_000);
        }

        fn balance(&self, account: &str, as_of: NaiveDate) -> i64 {
            self.store
                .connection()
                .query_row(
                    "SELECT COALESCE(SUM(jl.amount), 0) FROM journal_lines jl
                     JOIN journal_entries je ON jl.entry_id = je.id
                     WHERE jl.account_id = ?1 AND je.is_void = 0 AND je.date <= ?2",
                    rusqlite::params![account, as_of.to_string()],
                    |r| r.get(0),
                )
                .unwrap()
        }

        fn head(&self) -> i64 {
            self.store.latest_id().unwrap().unwrap_or(0)
        }

        fn close(&mut self, year: i32) -> Result<Closed, ClosingError> {
            let equity = self.equity.clone();
            close_books(
                &mut self.store,
                "user",
                CloseBooksCommand {
                    year,
                    equity_account_id: equity,
                    include_draws: false,
                },
            )
        }
    }

    #[test]
    fn a_close_takes_every_income_statement_account_to_zero() {
        let mut b = books();
        b.ordinary_year(2023);

        let closed = b.close(2023).unwrap();

        let end = day(2023, 12, 31);
        assert_eq!(b.balance(&b.sales, end), 0, "sales must be swept to zero");
        assert_eq!(b.balance(&b.refunds, end), 0, "refunds too");
        assert_eq!(b.balance(&b.rent, end), 0, "and rent");

        // Net income 5,000 - 200 - 3,000 = 1,800, credited to equity.
        assert_eq!(closed.net_income_cents, 180_000);
        assert_eq!(b.balance(&b.equity, end), -180_000);
        assert_eq!(closed.accounts_swept, 3);

        // The asset side is untouched: closing moves nothing real.
        assert_eq!(b.balance(&b.cash, end), 180_000);
    }

    /// A contra revenue account carries a debit balance. Sweeping it has to
    /// credit, not debit — the mistake that would leave the entry unbalanced and
    /// the account non-zero.
    #[test]
    fn a_contra_account_is_swept_on_its_own_side() {
        let mut b = books();
        b.ordinary_year(2023);
        assert_eq!(
            b.balance(&b.refunds, day(2023, 12, 31)),
            20_000,
            "refunds should be carrying a debit balance before the close"
        );

        let closed = b.close(2023).unwrap();

        let line: i64 = b
            .store
            .connection()
            .query_row(
                "SELECT amount FROM journal_lines WHERE entry_id = ?1 AND account_id = ?2",
                rusqlite::params![closed.entry_id, b.refunds],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(line, -20_000, "a debit balance is closed with a credit");

        let sum: i64 = b
            .store
            .connection()
            .query_row(
                "SELECT SUM(amount) FROM journal_lines WHERE entry_id = ?1",
                [&closed.entry_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sum, 0, "the closing entry must balance");
    }

    #[test]
    fn a_loss_lands_as_a_debit_to_equity() {
        let mut b = books();
        let (cash, sales, rent) = (b.cash.clone(), b.sales.clone(), b.rent.clone());
        b.post(day(2023, 3, 1), &cash, &sales, 100_000);
        b.post(day(2023, 9, 1), &rent, &cash, 250_000);

        let closed = b.close(2023).unwrap();

        assert_eq!(closed.net_income_cents, -150_000);
        assert_eq!(
            b.balance(&b.equity, day(2023, 12, 31)),
            150_000,
            "a loss leaves the year account carrying a debit balance"
        );
    }

    #[test]
    fn closing_twice_is_refused() {
        let mut b = books();
        b.ordinary_year(2023);
        let first = b.close(2023).unwrap();

        let err = b.close(2023).unwrap_err();
        match err {
            ClosingError::AlreadyClosed { year, entry_id } => {
                assert_eq!(year, 2023);
                assert_eq!(entry_id, first.entry_id);
            }
            other => panic!("expected AlreadyClosed, got {other:?}"),
        }
    }

    #[test]
    fn a_year_with_no_activity_has_nothing_to_close() {
        let mut b = books();
        b.ordinary_year(2023);
        b.close(2023).unwrap();

        // 2023's balances are now swept, so 2024 genuinely carries nothing.
        let err = b.close(2024).unwrap_err();
        assert!(
            matches!(err, ClosingError::NothingToClose { year: 2024 }),
            "got {err:?}"
        );
    }

    /// Closing 2024 while 2023 is still open is refused for the *earlier* year,
    /// not reported as an empty 2024 — 2023's balances are still sitting in the
    /// revenue and expense accounts and would be swept into 2024's result.
    #[test]
    fn a_later_year_cannot_be_closed_first() {
        let mut b = books();
        b.ordinary_year(2023);

        let err = b.close(2024).unwrap_err();
        assert!(
            matches!(err, ClosingError::PriorActivityNotClosed { year: 2024, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_deactivated_account_still_holding_a_balance_blocks_the_close() {
        let mut b = books();
        b.ordinary_year(2023);
        b.store
            .connection()
            .execute(
                "UPDATE accounts SET is_active = 0 WHERE id = ?1",
                [&b.rent],
            )
            .unwrap();

        let err = b.close(2023).unwrap_err();
        match err {
            ClosingError::InactiveAccountHoldsBalance { account, cents, .. } => {
                assert!(account.contains("6100"), "the message names the account");
                assert_eq!(cents, 300_000);
            }
            other => panic!("expected InactiveAccountHoldsBalance, got {other:?}"),
        }
        assert!(
            closing_entry_for(b.store.connection(), 2023).is_none(),
            "nothing may be posted when the close is refused"
        );
    }

    /// The sweep takes each account's whole ledger balance, so an earlier year
    /// that was never closed would have its earnings recorded as this year's.
    #[test]
    fn an_unclosed_earlier_year_blocks_the_close() {
        let mut b = books();
        b.ordinary_year(2022);
        b.ordinary_year(2023);

        let err = b.close(2023).unwrap_err();
        match err {
            ClosingError::PriorActivityNotClosed {
                year, opening_date, ..
            } => {
                assert_eq!(year, 2023);
                assert_eq!(opening_date, day(2022, 12, 31));
            }
            other => panic!("expected PriorActivityNotClosed, got {other:?}"),
        }

        // Closing them in order works, and each year keeps its own result.
        b.close(2022).unwrap();
        let y2023 = b.close(2023).unwrap();
        assert_eq!(y2023.net_income_cents, 180_000);
    }

    #[test]
    fn an_unbalanced_trial_balance_blocks_the_close() {
        let mut b = books();
        b.ordinary_year(2023);

        // Corrupt the projection directly: no command can produce this, which is
        // the point — if the books ever do get into this state, closing them
        // would carry the discrepancy into equity for good.
        b.store
            .connection()
            .execute(
                "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency)
                 SELECT 'orphan', je.id, ?1, 12345, 'USD' FROM journal_entries je LIMIT 1",
                [&b.cash],
            )
            .unwrap();

        let err = b.close(2023).unwrap_err();
        assert!(
            matches!(err, ClosingError::Unbalanced { year: 2023, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn the_equity_account_has_to_exist_and_be_equity() {
        let mut b = books();
        b.ordinary_year(2023);

        let err = close_books(
            &mut b.store,
            "user",
            CloseBooksCommand {
                year: 2023,
                equity_account_id: "no-such-account".to_string(),
                include_draws: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ClosingError::EquityAccountMissing));

        let cash = b.cash.clone();
        let err = close_books(
            &mut b.store,
            "user",
            CloseBooksCommand {
                year: 2023,
                equity_account_id: cash,
                include_draws: false,
            },
        )
        .unwrap_err();
        assert!(matches!(err, ClosingError::EquityAccountWrongType(_)));
    }

    /// The property `append_checked_many` is here for: a refused close leaves
    /// the log exactly where it was, with no half-closed year behind it.
    #[test]
    fn a_refused_close_appends_nothing() {
        let mut b = books();
        b.ordinary_year(2023);
        // Open the year first, so the head does not move for that reason.
        FiscalYearCommands::new(&mut b.store, "user".to_string())
            .ensure_year_open(2023)
            .unwrap();
        let before = b.head();

        let _ = close_books(
            &mut b.store,
            "user",
            CloseBooksCommand {
                year: 2023,
                equity_account_id: "no-such-account".to_string(),
                include_draws: false,
            },
        )
        .unwrap_err();

        assert_eq!(b.head(), before, "a rejection must not move the log");
        assert_eq!(
            b.balance(&b.sales, day(2023, 12, 31)),
            -500_000,
            "and must not have swept anything"
        );
    }

    /// The two events land together, adjacent, in the order that lets the fence
    /// admit the entry.
    #[test]
    fn the_entry_and_the_lock_are_one_append() {
        let mut b = books();
        b.ordinary_year(2023);
        let before = b.head();

        b.close(2023).unwrap();

        let types: Vec<String> = {
            let conn = b.store.connection();
            let mut stmt = conn
                .prepare("SELECT event_type FROM events WHERE id > ?1 ORDER BY id")
                .unwrap();
            let rows = stmt
                .query_map([before], |r| r.get::<_, String>(0))
                .unwrap();
            rows.filter_map(|r| r.ok()).collect()
        };
        // `fiscal_year_opened` precedes them: the year had never been opened.
        assert_eq!(
            types,
            vec!["fiscal_year_opened", "journal_entry_posted", "year_end_closed"],
        );
    }

    #[test]
    fn a_closed_year_refuses_further_entries() {
        let mut b = books();
        b.ordinary_year(2023);
        b.close(2023).unwrap();

        let (cash, rent) = (b.cash.clone(), b.rent.clone());
        let err = EntryCommands::new(&mut b.store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: day(2023, 7, 4),
                memo: "late".to_string(),
                lines: vec![
                    EntryLine::debit(&rent, 1000, "USD"),
                    EntryLine::credit(&cash, 1000, "USD"),
                ],
                reference: None,
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap_err();
        assert!(matches!(err, EntryCommandError::YearClosed(_)), "got {err:?}");
    }

    #[test]
    fn reopening_voids_the_entry_lifts_the_fence_and_allows_a_re_close() {
        let mut b = books();
        b.ordinary_year(2023);
        let first = b.close(2023).unwrap();

        reopen_books(&mut b.store, "user", 2023, "found a missing invoice").unwrap();

        assert!(load_year(b.store.connection(), 2023).unwrap().unwrap().is_closed == false);
        assert!(closing_entry_for(b.store.connection(), 2023).is_none());
        assert_eq!(
            b.balance(&b.sales, day(2023, 12, 31)),
            -500_000,
            "voiding the closing entry puts the swept balances back"
        );
        assert_eq!(b.balance(&b.equity, day(2023, 12, 31)), 0);

        // The correction that reopening was for.
        let (cash, sales) = (b.cash.clone(), b.sales.clone());
        b.post(day(2023, 7, 4), &cash, &sales, 100_000);

        let second = b.close(2023).unwrap();
        assert_ne!(second.entry_id, first.entry_id);
        assert_eq!(second.net_income_cents, 280_000);
    }

    #[test]
    fn reopening_a_year_that_is_not_closed_is_refused_and_needs_a_reason() {
        let mut b = books();
        b.ordinary_year(2023);

        let err = reopen_books(&mut b.store, "user", 2023, "why not").unwrap_err();
        assert!(matches!(err, ClosingError::NotClosed { year: 2023 }));

        b.close(2023).unwrap();
        let err = reopen_books(&mut b.store, "user", 2023, "   ").unwrap_err();
        assert!(matches!(err, ClosingError::Entry(_)), "got {err:?}");
    }

    #[test]
    fn the_year_is_opened_on_the_companys_own_boundaries() {
        let mut b = books();
        b.store
            .connection()
            .execute("UPDATE company SET fiscal_year_start_month = 7", [])
            .unwrap();

        // Activity inside fiscal 2023 = July 2023 through June 2024.
        let (cash, sales, rent) = (b.cash.clone(), b.sales.clone(), b.rent.clone());
        b.post(day(2023, 8, 1), &cash, &sales, 500_000);
        b.post(day(2024, 5, 1), &rent, &cash, 200_000);

        let closed = b.close(2023).unwrap();
        assert_eq!(closed.net_income_cents, 300_000);

        let (start, end): (String, String) = b
            .store
            .connection()
            .query_row(
                "SELECT start_date, end_date FROM fiscal_years WHERE year = 2023",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((start.as_str(), end.as_str()), ("2023-07-01", "2024-06-30"));

        let date: String = b
            .store
            .connection()
            .query_row(
                "SELECT date FROM journal_entries WHERE id = ?1",
                [&closed.entry_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(date, "2024-06-30", "the entry is dated the fiscal year end");
    }

    #[test]
    fn preview_agrees_with_what_the_close_does() {
        let mut b = books();
        b.ordinary_year(2023);

        let p = preview(b.store.connection(), 2023, false).unwrap();
        assert_eq!(p.year_end, day(2023, 12, 31));
        assert_eq!(p.net_income_cents, 180_000);
        assert_eq!(p.revenue.len(), 2, "sales and refunds");
        assert_eq!(p.expenses.len(), 1);
        assert!(p.trial_balance_ok);
        assert!(p.blocker.is_none());
        assert!(!p.is_closed());

        let closed = b.close(2023).unwrap();
        assert_eq!(closed.net_income_cents, p.net_income_cents);
        assert_eq!(closed.accounts_swept, p.swept().count());

        let after = preview(b.store.connection(), 2023, false).unwrap();
        assert!(after.is_closed());
        assert_eq!(after.closed_by.as_deref(), Some(closed.entry_id.as_str()));
        assert!(after.blocker.is_some(), "a closed year reports why it cannot close again");
    }

    #[test]
    fn preview_reports_the_blocker_rather_than_failing() {
        let mut b = books();
        b.ordinary_year(2022);
        b.ordinary_year(2023);

        let p = preview(b.store.connection(), 2023, false).unwrap();
        let blocker = p.blocker.expect("2023 cannot be closed while 2022 is open");
        assert!(
            blocker.contains("earlier activity"),
            "the blocker explains itself: {blocker}"
        );
    }

    #[test]
    fn asking_to_close_draws_with_none_linked_says_so() {
        let mut b = books();
        b.ordinary_year(2023);

        let p = preview(b.store.connection(), 2023, true).unwrap();
        assert!(p.draws.is_empty());
        assert!(
            p.warnings.iter().any(|w| w.contains("draw")),
            "the checkbox must not silently do nothing: {:?}",
            p.warnings
        );
    }

    #[test]
    fn years_with_activity_lists_newest_first_with_their_state() {
        let mut b = books();
        b.ordinary_year(2022);
        b.ordinary_year(2023);
        b.close(2022).unwrap();

        let years = years_with_activity(b.store.connection());
        assert_eq!(years, vec![(2023, false), (2022, true)]);
    }
}
