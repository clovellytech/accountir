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
    build_post_entry_in_closed_year_in_txn, build_post_entry_in_txn, EntryLine, PostEntryCommand,
    PostEntryStep,
};
use crate::commands::fiscal_year_commands::{boundaries_for, load_year};
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
    #[error("{year}'s result is already allocated to the partners by entry {entry_id}")]
    AlreadyAllocated { year: i32, entry_id: String },
    #[error(
        "{year}'s closing entry has no single year account to allocate from — it was closed \
         straight into partner capital, or into more than one equity account"
    )]
    NoYearAccount { year: i32 },
    #[error("{year} closed at exactly zero, so there is nothing to allocate")]
    NothingToAllocate { year: i32 },
    #[error("{year} is not closed")]
    NotClosed { year: i32 },
    #[error("The account to close into does not exist")]
    EquityAccountMissing,
    #[error("{0} is not an equity account — a year's result has to land in equity")]
    EquityAccountWrongType(String),
    #[error(
        "These books are not a partnership, so there are no partner capital accounts to \
         allocate {year} to. Close into a single equity account instead."
    )]
    NotAPartnership { year: i32 },
    #[error("No partner held an interest during {year}, so there is nobody to allocate it to")]
    NoPartnersInYear { year: i32 },
    #[error(
        "{partner} has no capital account linked, so there is nowhere to put their share of \
         {year}. Link one on the Partners page — an account is tied to a partner deliberately, \
         because matching on the name would move the link the day somebody renames it."
    )]
    PartnerHasNoCapitalAccount { partner: String, year: i32 },
    #[error(
        "{partner} has {count} accounts linked in the contribution role, so it is not clear \
         which one their share of {year} belongs in. Leave one linked as the capital account."
    )]
    PartnerCapitalIsAmbiguous {
        partner: String,
        count: usize,
        year: i32,
    },
    #[error(
        "The partners' profit percentages do not total 100% for {year}: {}.{:02} of \
         {}.{:02} would be allocated and the rest would belong to nobody. Fix the percentages \
         on the Partners page — they are what the allocation runs on.",
        allocated.abs() / 100, allocated.abs() % 100,
        total.abs() / 100, total.abs() % 100
    )]
    SharesDoNotTotal {
        year: i32,
        allocated: i64,
        total: i64,
    },
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
    /// How the result is split across the partners, when closing to partner
    /// capital. Empty when closing into a single account.
    pub allocation: Vec<PartnerShare>,
    /// Positive for a profit, negative for a loss.
    pub net_income_cents: i64,
    pub trial_balance_debits: i64,
    pub trial_balance_credits: i64,
    pub trial_balance_ok: bool,
    /// The live closing entry, if this year is already closed.
    pub closed_by: Option<String>,
    /// The live allocation entry, if the year's result has been moved on to the
    /// partners' capital accounts.
    pub allocated_by: Option<String>,
    /// Why a closed year's result cannot be allocated now, if it cannot.
    pub allocation_blocker: Option<String>,
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

    pub fn is_allocated(&self) -> bool {
        self.allocated_by.is_some()
    }

    /// Every account the entry will touch, revenue then expense then draws.
    pub fn swept(&self) -> impl Iterator<Item = &SweptAccount> {
        self.revenue
            .iter()
            .chain(self.expenses.iter())
            .chain(self.draws.iter())
    }
}

/// Where a year's result goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosingTarget {
    /// One equity account, conventionally `Equity:Years:2023`. Resolved — and
    /// created, if the caller wants a path that does not exist yet — before the
    /// command is called.
    Account(String),
    /// The year's result into this account, and then — in a second entry, in the
    /// same append — from it to each partner's own capital account, split on the
    /// percentages in force across the year.
    ///
    /// This is what a partnership's books do. A partnership pays no tax itself;
    /// the year's result passes through to the partners, and each partner's
    /// capital account is what says whose it is. Closing to a single equity
    /// account records that the partnership earned something without recording
    /// whose it is — which the Schedule K-1s then have to compute separately,
    /// leaving two records of one fact and only one of them in the ledger.
    ///
    /// # Why two entries
    ///
    /// One entry that swept income straight into partner capital answered two
    /// questions at once: what the year came to, and whose it is. `close-2023`
    /// now answers the first, putting the result in the year account, and
    /// `close-2023-allocation` answers the second, moving it on. The figure is in
    /// the ledger before it is divided, and the division reads on its own.
    PartnerCapital(String),
}

/// Command to close a fiscal year.
#[derive(Debug, Clone)]
pub struct CloseBooksCommand {
    pub year: i32,
    pub target: ClosingTarget,
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

/// The Schedule L line a year-result equity account belongs on: 21, partners'
/// capital accounts.
const YEAR_ACCOUNT_TAX_LINE: &str = "sl21";

/// Whether the target account already reaches a Schedule L line for `year`.
///
/// An account with no mapping is dropped from Schedule L entirely — the return
/// warns and names it, so it fails loudly rather than quietly, but a balance
/// sheet missing the year's own result is a return nobody can file. Since the
/// close is what puts a balance there, the close is what should say where it
/// goes.
///
/// Resolution matches `tax::lines::load_mapping`: the greatest `effective_from`
/// at or before the year.
fn already_mapped_for_tax(tx: &Connection, account_id: &str, year: i32) -> bool {
    tx.query_row(
        "SELECT 1 FROM tax_line_mappings t
          WHERE t.account_id = ?1 AND t.form = '1065' AND t.effective_from <= ?2
            AND t.effective_from = (
                SELECT MAX(u.effective_from) FROM tax_line_mappings u
                 WHERE u.account_id = t.account_id AND u.form = t.form
                   AND u.effective_from <= ?2)",
        rusqlite::params![account_id, year],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
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

/// One partner's share of a year, and where it goes.
#[derive(Debug, Clone)]
pub struct PartnerShare {
    pub partner_id: String,
    pub partner_name: String,
    pub account_id: String,
    pub account_label: String,
    /// Positive for a share of profit, negative for a share of a loss.
    pub cents: i64,
}

/// Split a year's result across the partners, and say which account each share
/// goes to.
///
/// # Why `allocate_over_year` and not `allocate_as_of`
///
/// A partner's percentage can change mid-year, and §706(d) then wants the year
/// divided at the change with each part allocated on its own split.
/// `allocate_as_of` applies one split to the whole year and warns; this is the
/// one `tax::capital` uses for Schedule K-1 item L, and using anything else here
/// would put a different figure in the ledger from the one on the K-1 — which is
/// exactly the divergence item L row 3 and box 1 hit before that module was
/// changed.
///
/// # Cents, not dollars
///
/// `allocate_on_ppm` is unit-agnostic and exact by construction — every partner
/// gets the floor of their share and the remainders go one each to the largest
/// fractional parts — so passing cents gives cents that sum to the cents given.
/// The K-1 passes dollars for the same reason: whole dollars are what the form
/// prints. A ledger entry needs the cents, and rounding to dollars here would
/// leave the closing entry unbalanced by the difference.
fn partner_capital_lines(
    tx: &Connection,
    year: i32,
    net_income_cents: i64,
) -> Result<Vec<PartnerShare>, ClosingError> {
    if crate::commands::sole_proprietor_commands::business_type(tx).is_sole_proprietorship() {
        return Err(ClosingError::NotAPartnership { year });
    }

    let partners = crate::commands::partnership_commands::partners_for_year(tx, year);
    if partners.is_empty() {
        return Err(ClosingError::NoPartnersInYear { year });
    }

    // One capital account each, in the contribution role. A partner may own
    // several linked accounts — contributions kept apart from draws is ordinary
    // bookkeeping — but a share of income has exactly one place to go, and
    // guessing between two would put a partner's earnings somewhere nobody
    // chose.
    let links = crate::tax::capital::load_partner_equity_accounts(tx);
    let mut targets: Vec<(String, String)> = Vec::new();
    for p in &partners {
        let mine: Vec<&crate::tax::capital::EquityAccount> = links
            .iter()
            .filter(|l| {
                l.partner_id == p.partner_id && l.role == crate::tax::capital::Role::Contribution
            })
            .collect();
        match mine.len() {
            0 => {
                return Err(ClosingError::PartnerHasNoCapitalAccount {
                    partner: p.name.clone(),
                    year,
                })
            }
            1 => targets.push((p.partner_id.clone(), mine[0].account_id.clone())),
            count => {
                return Err(ClosingError::PartnerCapitalIsAmbiguous {
                    partner: p.name.clone(),
                    count,
                    year,
                })
            }
        }
    }

    let refs: Vec<&crate::domain::Partner> = partners.iter().collect();
    // A year divided in fixed amounts closes on those amounts, in cents, so the
    // ledger carries what the K-1s say.
    let fixed = crate::commands::partnership_commands::list_fixed_allocations(tx);
    let shares = match crate::tax::allocate::split_fixed(net_income_cents, &refs, year, &fixed, true)
    {
        Some(shares) => shares,
        None => crate::tax::varying::allocate_over_year(
            tx,
            year,
            net_income_cents,
            &refs,
            crate::tax::allocate::Basis::ProfitOrLoss,
            // The whole of Schedule K, because that is what is being split.
            crate::tax::varying::ANALYSIS,
        ),
    };

    // The percentages are apportioned as given — a partnership whose shares sum
    // to 90% gets 90% allocated and the rest belongs to nobody. That is the right
    // answer for a return, which reports what the records say; it is not a thing
    // a journal entry can do, because the missing tenth would leave it
    // unbalanced. So it is refused here, naming the shortfall.
    let allocated: i64 = shares.iter().map(|s| s.dollars).sum();
    if allocated != net_income_cents {
        return Err(ClosingError::SharesDoNotTotal {
            year,
            allocated,
            total: net_income_cents,
        });
    }

    let mut out = Vec::new();
    for (i, p) in partners.iter().enumerate() {
        let cents = shares.get(i).map(|s| s.dollars).unwrap_or(0);
        let (_, account_id) = &targets[i];
        let label: String = tx
            .query_row(
                "SELECT account_number || ' ' || name FROM accounts WHERE id = ?1",
                [account_id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_else(|| account_id.clone());
        out.push(PartnerShare {
            partner_id: p.partner_id.clone(),
            partner_name: p.name.clone(),
            account_id: account_id.clone(),
            account_label: label,
            cents,
        });
    }
    Ok(out)
}

/// The equity accounts linked to a partner in the "draw" role.
fn draw_account_ids(conn: &Connection) -> Vec<String> {
    let mut stmt =
        match conn.prepare("SELECT account_id FROM partner_equity_accounts WHERE role = 'draw'") {
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
    target: &ClosingTarget,
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
    if let Some(stale) = crate::commands::depreciation_commands::posting_is_stale(conn, year) {
        warnings.push(stale);
    }

    let closed_by = closing_entry_for(conn, year);
    let allocated_by = allocation_entry_for(conn, year);

    // Computed for the preview even when it would refuse, so the page can show
    // the split it *would* post beside the reason it cannot. For a closed year it
    // is the split an allocation would post now, if none has been.
    let (allocation, allocation_blocker) = if closed_by.is_some() {
        if allocated_by.is_some() {
            (Vec::new(), None)
        } else {
            match allocation_plan(conn, year) {
                Ok(plan) => (plan.shares, None),
                Err(e) => (Vec::new(), Some(e.to_string())),
            }
        }
    } else {
        match target {
            ClosingTarget::PartnerCapital(_) => (
                partner_capital_lines(conn, year, net_income_cents).unwrap_or_default(),
                None,
            ),
            ClosingTarget::Account(_) => (Vec::new(), None),
        }
    };
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
        .or_else(|| match target {
            // The allocation's own refusals — an unlinked partner, an ambiguous
            // capital account, percentages that do not total — belong in the
            // blocker too, or the page would offer a Close button that fails.
            ClosingTarget::PartnerCapital(_) => {
                partner_capital_lines(conn, year, net_income_cents).err()
            }
            ClosingTarget::Account(_) => None,
        })
        .map(|e| e.to_string()),
    };

    Ok(ClosingPreview {
        year,
        year_start,
        year_end,
        revenue,
        expenses,
        draws,
        allocation,
        net_income_cents,
        trial_balance_debits: debits,
        trial_balance_credits: credits,
        trial_balance_ok: debits == credits,
        closed_by,
        allocated_by,
        allocation_blocker,
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

/// Build every event a close appends, under the write lock.
///
/// Shared by [`close_books`] and the group server's `close-books` endpoint, so
/// the invariants are enforced identically whichever door the command came
/// through — the same reason `build_post_entry_in_txn` exists.
///
/// Everything is re-derived here rather than passed in: another writer may have
/// posted into the year between the preview a user was looking at and this
/// append, and on a group server the two are on different machines.
pub(crate) fn build_close_books_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &CloseBooksCommand,
) -> Result<Verdict<Vec<Event>, ClosingError>, EventStoreError> {
    let year_account = match &cmd.target {
        ClosingTarget::Account(id) | ClosingTarget::PartnerCapital(id) => id.clone(),
    };
    // Both targets pass the result through the year account, so both check it.
    {
        let account_id = &year_account;
        let equity: Option<(String, bool)> = tx
            .query_row(
                "SELECT account_type, is_active = 1 FROM accounts WHERE id = ?1",
                [account_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match equity {
            None => return Ok(Verdict::Reject(ClosingError::EquityAccountMissing)),
            Some((ty, _)) if ty != "equity" => {
                return Ok(Verdict::Reject(ClosingError::EquityAccountWrongType(
                    account_id.clone(),
                )))
            }
            Some(_) => {}
        }
    }

    // The year may never have been opened — which is the ordinary case, not the
    // edge one: no ledger has ever had a `fiscal_years` row. Opening it rides in
    // this same batch rather than in an append of its own, so a close is exactly
    // one atomic unit and a failure cannot leave a year opened but not closed.
    let existing = load_year(tx, cmd.year)?;
    let fy = match &existing {
        Some(fy) => fy.clone(),
        None => boundaries_for(tx, cmd.year),
    };

    if let Some(entry_id) = closing_entry_for(tx, cmd.year) {
        return Ok(Verdict::Reject(ClosingError::AlreadyClosed {
            year: cmd.year,
            entry_id,
        }));
    }

    let revenue = match accounts_with_balances(tx, &[AccountType::Revenue], fy.end_date, None) {
        Ok(v) => v,
        Err(e) => return Ok(Verdict::Reject(e)),
    };
    let expenses = match accounts_with_balances(tx, &[AccountType::Expense], fy.end_date, None) {
        Ok(v) => v,
        Err(e) => return Ok(Verdict::Reject(e)),
    };
    let draws = if cmd.include_draws {
        let ids = draw_account_ids(tx);
        match accounts_with_balances(tx, &[AccountType::Equity], fy.end_date, Some(&ids)) {
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
    if let Err(e) = check_can_close(tx, cmd.year, fy.start_date, &revenue, &expenses, tb) {
        return Ok(Verdict::Reject(e));
    }

    // Lines: each account back to zero, then the balancing figure to equity. The
    // equity line is the sum of what the others removed, so the entry sums to
    // zero by construction.
    let currency = base_currency(tx);
    let mut lines: Vec<EntryLine> = Vec::new();
    let mut sweep_total: i64 = 0;
    for account in revenue.iter().chain(expenses.iter()).chain(draws.iter()) {
        sweep_total += account.balance_cents;
        lines.push(account.closing_line(&currency));
    }
    let net_income_cents = -sweep_total;
    // The other side: one line to the year account. Moving it on to the partners
    // is a second entry, built below.
    if sweep_total != 0 {
        lines.push(
            EntryLine::signed(&year_account, sweep_total, &currency)
                .with_memo(&format!("Net result for {}", cmd.year)),
        );
    }
    // Worked out before anything is built, so a partner with no capital account
    // refuses the close rather than leaving a year closed and unallocated.
    let allocation = match &cmd.target {
        ClosingTarget::PartnerCapital(_) => {
            match partner_capital_lines(tx, cmd.year, net_income_cents) {
                Ok(a) => Some(a),
                Err(e) => return Ok(Verdict::Reject(e)),
            }
        }
        ClosingTarget::Account(_) => None,
    };

    let swept_count = revenue.len() + expenses.len() + draws.len();
    let post = PostEntryCommand {
        date: fy.end_date,
        memo: memo_for(cmd.year, net_income_cents, swept_count),
        lines,
        reference: Some(reference_for(cmd.year)),
        source: Some(JournalEntrySource::Closing),
    };

    let mut events = Vec::new();
    if existing.is_none() {
        events.push(Event::FiscalYearOpened {
            year: cmd.year,
            start_date: fy.start_date,
            end_date: fy.end_date,
        });
    }

    let entry_event = match build_post_entry_in_txn(tx, &post)? {
        PostEntryStep::Append(event) => event,
        PostEntryStep::Reject(e) => return Ok(Verdict::Reject(ClosingError::Entry(e.to_string()))),
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

    events.push(entry_event);

    // The allocation, dated the same day. Built here, before the lock event, for
    // the reason the closing entry is: the fence is checked against the state
    // before this batch, so it sees the year still open and admits both.
    if let Some(allocation) = &allocation {
        if net_income_cents != 0 {
            let post = allocation_post(
                cmd.year,
                fy.end_date,
                &year_account,
                net_income_cents,
                allocation,
                &currency,
            );
            match build_post_entry_in_txn(tx, &post)? {
                PostEntryStep::Append(event) => events.push(event),
                PostEntryStep::Reject(e) => {
                    return Ok(Verdict::Reject(ClosingError::Entry(e.to_string())))
                }
            }
        }
    }

    events.push(Event::YearEndClosed {
        year: cmd.year,
        retained_earnings_entry_id: entry_id,
    });

    // Point the year account at Schedule L line 21, unless it already reaches a
    // line. The close has just given this account a balance-sheet balance;
    // leaving it on no line means the year's own result is missing from Schedule
    // L. Only for a partnership — line 21 is a Form 1065 line, and a sole
    // proprietorship files no balance sheet at all.
    //
    // Not done for partner capital accounts: those are not accounts this command
    // created, they already carried balances, and where they belong on the return
    // is a decision their owner has already made.
    {
        let account_id = &year_account;
        let partnership =
            !crate::commands::sole_proprietor_commands::business_type(tx).is_sole_proprietorship();
        if partnership && !already_mapped_for_tax(tx, account_id, cmd.year) {
            events.push(Event::TaxLineMappingSet {
                account_id: account_id.clone(),
                line_key: YEAR_ACCOUNT_TAX_LINE.to_string(),
                effective_from: cmd.year,
                form: Some(crate::tax::ReturnForm::Form1065.as_str().to_string()),
            });
        }
    }

    Ok(Verdict::Append(events))
}

/// Build every event a reopen appends, under the write lock. Shared with the
/// group server's `reopen-year` endpoint.
pub(crate) fn build_reopen_books_in_txn(
    tx: &rusqlite::Transaction<'_>,
    year: i32,
    reason: &str,
    user_id: &str,
) -> Result<Verdict<Vec<Event>, ClosingError>, EventStoreError> {
    let is_closed = load_year(tx, year)?.map(|fy| fy.is_closed).unwrap_or(false);
    let entry_id = closing_entry_for(tx, year);
    if !is_closed && entry_id.is_none() {
        return Ok(Verdict::Reject(ClosingError::NotClosed { year }));
    }

    let mut events = Vec::new();
    // The allocation first. It moved the result the closing entry put in the year
    // account, and voiding the close alone would leave the partners holding shares
    // of a year that no longer has a result.
    if let Some(allocation_id) = allocation_entry_for(tx, year) {
        events.push(Event::JournalEntryVoided {
            entry_id: allocation_id,
            reason: format!("Reopening {year}: {reason}"),
        });
    }
    if let Some(entry_id) = entry_id {
        // Built directly rather than through `build_void_entry_in_txn`, which
        // refuses to void a closing entry while its year is closed — the state
        // this very batch is undoing. Its other check, that the entry is live,
        // `closing_entry_for` has already made: it only returns entries with
        // `is_void = 0`, and nothing else can touch them under this write lock.
        events.push(Event::JournalEntryVoided {
            entry_id,
            reason: format!("Reopening {year}: {reason}"),
        });
    }
    events.push(Event::YearEndReopened {
        year,
        reason: reason.to_string(),
        reopened_by_user_id: user_id.to_string(),
    });
    Ok(Verdict::Append(events))
}

/// Close the books for a year: post the closing entry and fence the year, in one
/// atomic append.
pub fn close_books(
    store: &mut EventStore,
    user_id: &str,
    cmd: CloseBooksCommand,
) -> Result<Closed, ClosingError> {
    let year = cmd.year;

    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let user_id = user_id.to_string();
        let cmd = cmd.clone();

        let outcome = store.append_checked_many(
            head,
            move |tx| {
                Ok(match build_close_books_in_txn(tx, &cmd)? {
                    Verdict::Append(events) => Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    ),
                    Verdict::Reject(e) => Verdict::Reject(e),
                })
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
                let (net, swept) = result_of(store.connection(), &entry_id);
                return Ok(Closed {
                    year,
                    entry_id,
                    net_income_cents: net,
                    accounts_swept: swept,
                });
            }
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

/// The net result and how many accounts were swept, read back off the posted
/// entry — so what is reported is what the ledger actually holds rather than
/// what the caller computed.
///
/// Counted from the income-statement lines rather than from whatever the other
/// side turned out to be, which is one line to an equity account or one per
/// partner depending on the target.
fn result_of(conn: &Connection, entry_id: &str) -> (i64, usize) {
    conn.query_row(
        "SELECT COALESCE(SUM(jl.amount), 0), COUNT(*)
           FROM journal_lines jl
           JOIN accounts a ON a.id = jl.account_id
          WHERE jl.entry_id = ?1 AND a.account_type IN ('revenue', 'expense')",
        [entry_id],
        // The lines are already the negation of the balances they clear, so their
        // sum *is* the net result: a credit-balance revenue account contributes a
        // debit line, and income comes out positive.
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as usize)),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or((0, 0))
}

fn memo_for(year: i32, net_income_cents: i64, swept: usize) -> String {
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
                Ok(
                    match build_reopen_books_in_txn(tx, year, &reason, &user_id)? {
                        Verdict::Append(events) => Verdict::Append(
                            events
                                .into_iter()
                                .map(|e| EventEnvelope::new(e, user_id.clone()))
                                .collect(),
                        ),
                        Verdict::Reject(e) => Verdict::Reject(e),
                    },
                )
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

/// The idempotency key a year's allocation entry carries. Voiding it — which
/// reopening does — frees it, exactly as for the closing entry.
pub fn allocation_reference_for(year: i32) -> String {
    format!("close-{year}-allocation")
}

/// The live allocation entry for a year, if its result has been allocated.
pub fn allocation_entry_for(conn: &Connection, year: i32) -> Option<String> {
    conn.query_row(
        "SELECT id FROM journal_entries WHERE reference = ?1 AND is_void = 0",
        [allocation_reference_for(year)],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// The entry that moves a year's result from the year account to the partners.
///
/// The year account line is the negation of the close's own line to it, so the
/// account ends the year at zero; each partner's line is the negation of their
/// share, a share of income being a credit to capital. The shares add back to
/// the result exactly, so the entry balances by construction.
fn allocation_post(
    year: i32,
    date: NaiveDate,
    year_account: &str,
    net_income_cents: i64,
    shares: &[PartnerShare],
    currency: &str,
) -> PostEntryCommand {
    let mut lines = vec![EntryLine::signed(year_account, net_income_cents, currency)
        .with_memo(&format!("{year} allocated to the partners"))];
    for share in shares.iter().filter(|s| s.cents != 0) {
        lines.push(
            EntryLine::signed(&share.account_id, -share.cents, currency)
                .with_memo(&format!("{}'s share of {year}", share.partner_name)),
        );
    }
    let magnitude = format!(
        "{}.{:02}",
        net_income_cents.abs() / 100,
        net_income_cents.abs() % 100
    );
    PostEntryCommand {
        date,
        memo: format!(
            "Allocation of {year}'s net {} {magnitude} to the partners",
            if net_income_cents < 0 {
                "loss"
            } else {
                "income"
            }
        ),
        lines,
        reference: Some(allocation_reference_for(year)),
        source: Some(JournalEntrySource::Closing),
    }
}

/// What allocating a closed year would post.
struct AllocationPlan {
    year_account: String,
    date: NaiveDate,
    shares: Vec<PartnerShare>,
    net_income_cents: i64,
}

/// Work out the allocation for a year that is already closed.
///
/// The year account is read off the closing entry itself — its one equity line
/// that is neither a partner's draw nor a partner's capital account — rather
/// than taken from the caller, so the allocation always empties the account the
/// close actually filled. The amount is the closed result, read the same way.
fn allocation_plan(conn: &Connection, year: i32) -> Result<AllocationPlan, ClosingError> {
    let Some(entry_id) = closing_entry_for(conn, year) else {
        return Err(ClosingError::NotClosed { year });
    };
    if let Some(entry_id) = allocation_entry_for(conn, year) {
        return Err(ClosingError::AlreadyAllocated { year, entry_id });
    }

    let date = conn
        .query_row(
            "SELECT date FROM journal_entries WHERE id = ?1",
            [&entry_id],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|d| NaiveDate::parse_from_str(d.get(..10).unwrap_or(&d), "%Y-%m-%d").ok())
        .ok_or(ClosingError::NotClosed { year })?;

    let excluded: Vec<String> = draw_account_ids(conn)
        .into_iter()
        .chain(
            crate::tax::capital::load_partner_equity_accounts(conn)
                .into_iter()
                .map(|l| l.account_id),
        )
        .collect();
    let equity_lines: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT jl.account_id FROM journal_lines jl
               JOIN accounts a ON a.id = jl.account_id
              WHERE jl.entry_id = ?1 AND a.account_type = 'equity'",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map([&entry_id], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let candidates: Vec<String> = equity_lines
        .into_iter()
        .filter(|a| !excluded.contains(a))
        .collect();
    let [year_account] = candidates.as_slice() else {
        return Err(ClosingError::NoYearAccount { year });
    };

    let (net_income_cents, _) = result_of(conn, &entry_id);
    if net_income_cents == 0 {
        return Err(ClosingError::NothingToAllocate { year });
    }
    let shares = partner_capital_lines(conn, year, net_income_cents)?;
    Ok(AllocationPlan {
        year_account: year_account.clone(),
        date,
        shares,
        net_income_cents,
    })
}

/// Build the allocation of a closed year, under the write lock. Shared with the
/// group server's `allocate-year` endpoint.
pub(crate) fn build_allocate_in_txn(
    tx: &rusqlite::Transaction<'_>,
    year: i32,
) -> Result<Verdict<Vec<Event>, ClosingError>, EventStoreError> {
    let plan = match allocation_plan(tx, year) {
        Ok(plan) => plan,
        Err(e) => return Ok(Verdict::Reject(e)),
    };
    let currency = base_currency(tx);
    let post = allocation_post(
        year,
        plan.date,
        &plan.year_account,
        plan.net_income_cents,
        &plan.shares,
        &currency,
    );
    // The year is closed, so the ordinary post refuses anything dated inside it.
    // This is the one entry the fence lets through: equity to equity, out of the
    // account the close filled, dated the day it closed.
    Ok(match build_post_entry_in_closed_year_in_txn(tx, &post)? {
        PostEntryStep::Append(event) => Verdict::Append(vec![event]),
        PostEntryStep::Reject(e) => Verdict::Reject(ClosingError::Entry(e.to_string())),
    })
}

/// What an allocation did.
#[derive(Debug, Clone)]
pub struct Allocated {
    pub year: i32,
    pub entry_id: String,
    pub net_income_cents: i64,
}

/// Allocate a closed year's result to the partners' capital accounts, in its own
/// entry dated the day the year closed.
///
/// For a year closed into its account without the allocation — closed before the
/// partners' capital accounts were linked, say. A close to partner capital
/// already posts it, in the same append as the close.
pub fn allocate_to_partners(
    store: &mut EventStore,
    user_id: &str,
    year: i32,
) -> Result<Allocated, ClosingError> {
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let user_id = user_id.to_string();

        let outcome = store.append_checked_many(
            head,
            move |tx| {
                Ok(match build_allocate_in_txn(tx, year)? {
                    Verdict::Append(events) => Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    ),
                    Verdict::Reject(e) => Verdict::Reject(e),
                })
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;

        match outcome {
            CheckedOutcome::Appended(_) => {
                let conn = store.connection();
                let entry_id = allocation_entry_for(conn, year).ok_or_else(|| {
                    ClosingError::Entry("the allocation appended no entry".to_string())
                })?;
                let net_income_cents = closing_entry_for(conn, year)
                    .map(|e| result_of(conn, &e).0)
                    .unwrap_or(0);
                return Ok(Allocated {
                    year,
                    entry_id,
                    net_income_cents,
                });
            }
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
                    target: ClosingTarget::Account(equity),
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
            .execute("UPDATE accounts SET is_active = 0 WHERE id = ?1", [&b.rent])
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
                target: ClosingTarget::Account("no-such-account".to_string()),
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
                target: ClosingTarget::Account(cash),
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
        crate::commands::fiscal_year_commands::FiscalYearCommands::new(
            &mut b.store,
            "user".to_string(),
        )
        .ensure_year_open(2023)
        .unwrap();
        let before = b.head();

        let _ = close_books(
            &mut b.store,
            "user",
            CloseBooksCommand {
                year: 2023,
                target: ClosingTarget::Account("no-such-account".to_string()),
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
            let rows = stmt.query_map([before], |r| r.get::<_, String>(0)).unwrap();
            rows.filter_map(|r| r.ok()).collect()
        };
        // `fiscal_year_opened` precedes them: the year had never been opened.
        // The tax-line assignment rides along in the same batch — books that have
        // never said what they file are treated as a partnership, which is what
        // `sole_proprietor_commands::business_type` defaults to.
        assert_eq!(
            types,
            vec![
                "fiscal_year_opened",
                "journal_entry_posted",
                "year_end_closed",
                "tax_line_mapping_set",
            ],
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
        assert!(
            matches!(err, EntryCommandError::YearClosed(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn reopening_voids_the_entry_lifts_the_fence_and_allows_a_re_close() {
        let mut b = books();
        b.ordinary_year(2023);
        let first = b.close(2023).unwrap();

        reopen_books(&mut b.store, "user", 2023, "found a missing invoice").unwrap();

        assert!(
            load_year(b.store.connection(), 2023)
                .unwrap()
                .unwrap()
                .is_closed
                == false
        );
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

    /// Voiding the closing entry on its own would put the books somewhere
    /// nothing can get them out of: the balances come back, the fence stays up,
    /// and the entry that would put them away again cannot be posted.
    #[test]
    fn the_closing_entry_cannot_be_voided_while_the_year_is_closed() {
        use crate::commands::entry_commands::VoidEntryCommand;

        let mut b = books();
        b.ordinary_year(2023);
        let closed = b.close(2023).unwrap();

        let err = EntryCommands::new(&mut b.store, "user".to_string())
            .void_entry(VoidEntryCommand {
                entry_id: closed.entry_id.clone(),
                reason: "changed my mind".to_string(),
            })
            .unwrap_err();
        assert!(
            matches!(err, EntryCommandError::ClosingEntryFenced { year: 2023 }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("Reopen"),
            "the refusal has to name the way out: {err}"
        );

        // Still intact, and still closed.
        assert_eq!(
            closing_entry_for(b.store.connection(), 2023).as_deref(),
            Some(closed.entry_id.as_str())
        );
        assert!(
            load_year(b.store.connection(), 2023)
                .unwrap()
                .unwrap()
                .is_closed
        );

        // Reopening does void it — that is the door.
        reopen_books(&mut b.store, "user", 2023, "correcting an invoice").unwrap();
        assert!(closing_entry_for(b.store.connection(), 2023).is_none());
    }

    /// Once the year is open again, its old closing entry is an ordinary voided
    /// entry and the fence has nothing to say about a later one.
    #[test]
    fn an_ordinary_entry_is_still_voidable_in_a_closed_year() {
        let mut b = books();
        b.ordinary_year(2023);
        // A live 2024 entry, and 2023 closed. The fence keys on the entry's own
        // year, so a 2024 entry is unaffected by 2023 being shut.
        let (cash, sales) = (b.cash.clone(), b.sales.clone());
        b.post(day(2024, 2, 1), &cash, &sales, 10_000);
        b.close(2023).unwrap();

        let entry_2024: String = b
            .store
            .connection()
            .query_row(
                "SELECT id FROM journal_entries WHERE date = '2024-02-01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        EntryCommands::new(&mut b.store, "user".to_string())
            .void_entry(crate::commands::entry_commands::VoidEntryCommand {
                entry_id: entry_2024,
                reason: "duplicate".to_string(),
            })
            .unwrap();
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

        let target = ClosingTarget::Account(b.equity.clone());
        let p = preview(b.store.connection(), 2023, false, &target).unwrap();
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

        let after = preview(b.store.connection(), 2023, false, &target).unwrap();
        assert!(after.is_closed());
        assert_eq!(after.closed_by.as_deref(), Some(closed.entry_id.as_str()));
        assert!(
            after.blocker.is_some(),
            "a closed year reports why it cannot close again"
        );
    }

    #[test]
    fn preview_reports_the_blocker_rather_than_failing() {
        let mut b = books();
        b.ordinary_year(2022);
        b.ordinary_year(2023);

        let target = ClosingTarget::Account(b.equity.clone());
        let p = preview(b.store.connection(), 2023, false, &target).unwrap();
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

        let p = preview(
            b.store.connection(),
            2023,
            true,
            &ClosingTarget::Account(b.equity.clone()),
        )
        .unwrap();
        assert!(p.draws.is_empty());
        assert!(
            p.warnings.iter().any(|w| w.contains("draw")),
            "the checkbox must not silently do nothing: {:?}",
            p.warnings
        );
    }

    fn set_business_type(store: &EventStore, kind: &str) {
        store
            .connection()
            .execute(
                "INSERT INTO business_profile
                     (id, legal_name, street, city, state, postal_code, ein, naics_code,
                      formation_date, business_type)
                 VALUES ('default', 'Co', '1 St', 'Town', 'IL', '60000', '00-0000000',
                         '451120', '2020-01-01', ?1)
                 ON CONFLICT(id) DO UPDATE SET business_type = excluded.business_type",
                [kind],
            )
            .unwrap();
    }

    fn tax_line_for(store: &EventStore, account_id: &str) -> Option<String> {
        store
            .connection()
            .query_row(
                "SELECT line_key FROM tax_line_mappings WHERE account_id = ?1",
                [account_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }

    /// The close gives the year account a balance-sheet balance, so it also says
    /// where that balance goes on Schedule L. Left unmapped, the year's own
    /// result is simply missing from the return's balance sheet.
    #[test]
    fn closing_points_the_year_account_at_schedule_l_line_21() {
        let mut b = books();
        set_business_type(&b.store, "partnership");
        b.ordinary_year(2023);

        assert_eq!(tax_line_for(&b.store, &b.equity), None);
        b.close(2023).unwrap();
        assert_eq!(tax_line_for(&b.store, &b.equity).as_deref(), Some("sl21"));
    }

    /// An account that already reaches a line keeps the one it has — a
    /// partnership closing into an existing capital account has already said
    /// where it goes, and the close must not overrule them.
    #[test]
    fn an_account_that_is_already_mapped_is_left_alone() {
        let mut b = books();
        set_business_type(&b.store, "partnership");
        b.ordinary_year(2023);
        b.store
            .connection()
            .execute(
                "INSERT INTO tax_line_mappings (account_id, line_key, effective_from)
                 VALUES (?1, 'sl19a', 0)",
                [&b.equity],
            )
            .unwrap();

        b.close(2023).unwrap();
        assert_eq!(tax_line_for(&b.store, &b.equity).as_deref(), Some("sl19a"));
    }

    /// Line 21 is a Form 1065 line. A sole proprietorship files no balance sheet
    /// at all, so the assignment would be noise on books that can never show it.
    #[test]
    fn a_sole_proprietorship_gets_no_schedule_l_assignment() {
        let mut b = books();
        set_business_type(&b.store, "sole_proprietorship");
        b.ordinary_year(2023);

        b.close(2023).unwrap();
        assert_eq!(tax_line_for(&b.store, &b.equity), None);
    }

    // --- closing to partner capital ----------------------------------------

    /// The books above, made into a partnership with two partners splitting
    /// 60/40, each with a contribution and a draw account linked.
    fn as_a_partnership(b: &mut Books, split: (f64, f64)) -> (String, String) {
        use crate::commands::partnership_commands::{
            admit_partner, link_equity_account, set_profile, AdmitPartner,
        };
        use crate::domain::{Address, BusinessProfile, PartnerType, Residency, Shares};

        let address = || Address {
            street: "1 Example Street".into(),
            suite: None,
            city: "Chicago".into(),
            state: "IL".into(),
            postal_code: "60600".into(),
            country: None,
        };
        set_profile(
            &mut b.store,
            "u",
            &BusinessProfile {
                legal_name: "Two Partners LLC".into(),
                address: address(),
                ein: "88-1234567".into(),
                naics_code: "541511".into(),
                formation_date: day(2020, 1, 1),
                principal_activity: None,
                principal_product: None,
            },
        )
        .unwrap();

        let mut admit = |name: &str, pct: f64| -> String {
            admit_partner(
                &mut b.store,
                "u",
                &AdmitPartner {
                    name: name.into(),
                    partner_type: PartnerType::General,
                    residency: Residency::Domestic,
                    entity_type: "Individual".into(),
                    address: address(),
                    start_date: Some(day(2020, 1, 1)),
                    shares: Shares::from_percents(pct, pct, pct),
                    tin: None,
                },
            )
            .unwrap()
            .0
        };
        let one = admit("Ada", split.0);
        let two = admit("Bo", split.1);

        // Their capital accounts, and a draw account each so the roles are not
        // trivially unambiguous.
        for (number, name) in [
            ("3101", "Ada capital"),
            ("3102", "Ada draws"),
            ("3201", "Bo capital"),
            ("3202", "Bo draws"),
        ] {
            AccountCommands::new(&mut b.store, "u".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Equity,
                    account_number: number.to_string(),
                    name: name.to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
        }
        let id = |b: &Books, n: &str| -> String {
            b.store
                .connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [n],
                    |r| r.get(0),
                )
                .unwrap()
        };
        for (partner, number, role) in [
            (&one, "3101", "contribution"),
            (&one, "3102", "draw"),
            (&two, "3201", "contribution"),
            (&two, "3202", "draw"),
        ] {
            let account = id(b, number);
            link_equity_account(&mut b.store, "u", partner, &account, role).unwrap();
        }
        (id(b, "3101"), id(b, "3201"))
    }

    fn close_to_partners(b: &mut Books, year: i32) -> Result<Closed, ClosingError> {
        let year_account = b.equity.clone();
        close_books(
            &mut b.store,
            "user",
            CloseBooksCommand {
                year,
                target: ClosingTarget::PartnerCapital(year_account),
                include_draws: false,
            },
        )
    }

    /// The point of the whole phase: the year's result reaches the partners'
    /// own capital accounts, in their shares, rather than sitting in one bucket.
    #[test]
    fn the_year_is_split_across_the_partners_capital_accounts() {
        let mut b = books();
        let (ada, bo) = as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);

        let closed = close_to_partners(&mut b, 2023).unwrap();
        assert_eq!(closed.net_income_cents, 180_000);

        let end = day(2023, 12, 31);
        assert_eq!(b.balance(&ada, end), -108_000, "60% of 1,800");
        assert_eq!(b.balance(&bo, end), -72_000, "40% of 1,800");
        assert_eq!(
            b.balance(&ada, end) + b.balance(&bo, end),
            -180_000,
            "the shares have to add back to the year"
        );
        // And nothing was left in the year account.
        assert_eq!(b.balance(&b.equity, end), 0);
    }

    /// Rounding is by largest remainder, so thirds of an odd number still add
    /// back exactly — the property that stops a K-1 set totalling one cent short
    /// of Schedule K.
    #[test]
    fn an_indivisible_result_still_adds_back_exactly() {
        let mut b = books();
        let (ada, bo) = as_a_partnership(&mut b, (50.0, 50.0));
        // 1,000.01 of sales and nothing else: an odd number of cents.
        let (cash, sales) = (b.cash.clone(), b.sales.clone());
        b.post(day(2023, 5, 1), &cash, &sales, 100_001);

        let closed = close_to_partners(&mut b, 2023).unwrap();
        assert_eq!(closed.net_income_cents, 100_001);

        let end = day(2023, 12, 31);
        let (a, o) = (b.balance(&ada, end), b.balance(&bo, end));
        assert_eq!(a + o, -100_001, "not a cent may go missing");
        assert!(
            (a - o).abs() == 1,
            "and the odd cent goes to one of them: {a} {o}"
        );
    }

    /// A loss is a debit to capital, in each partner's share.
    #[test]
    fn a_loss_is_split_too() {
        let mut b = books();
        let (ada, bo) = as_a_partnership(&mut b, (60.0, 40.0));
        let (cash, sales, rent) = (b.cash.clone(), b.sales.clone(), b.rent.clone());
        b.post(day(2023, 3, 1), &cash, &sales, 100_000);
        b.post(day(2023, 9, 1), &rent, &cash, 250_000);

        let closed = close_to_partners(&mut b, 2023).unwrap();
        assert_eq!(closed.net_income_cents, -150_000);

        let end = day(2023, 12, 31);
        assert_eq!(b.balance(&ada, end), 90_000, "a debit — capital went down");
        assert_eq!(b.balance(&bo, end), 60_000);
    }

    /// Percentages that do not total 100% are apportioned as given by the
    /// return, which reports what the records say. A journal entry cannot do
    /// that — the missing part would leave it unbalanced — so it is refused.
    #[test]
    fn shares_that_do_not_total_are_refused_rather_than_posted_short() {
        let mut b = books();
        as_a_partnership(&mut b, (50.0, 40.0));
        b.ordinary_year(2023);

        let err = close_to_partners(&mut b, 2023).unwrap_err();
        match err {
            ClosingError::SharesDoNotTotal { year, total, .. } => {
                assert_eq!(year, 2023);
                assert_eq!(total, 180_000);
            }
            other => panic!("expected SharesDoNotTotal, got {other:?}"),
        }
        assert!(closing_entry_for(b.store.connection(), 2023).is_none());
    }

    #[test]
    fn a_partner_with_no_capital_account_blocks_the_close() {
        use crate::commands::partnership_commands::unlink_equity_account;

        let mut b = books();
        let (ada, _bo) = as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);

        let partner: String = b
            .store
            .connection()
            .query_row(
                "SELECT partner_id FROM partner_equity_accounts WHERE account_id = ?1",
                [&ada],
                |r| r.get(0),
            )
            .unwrap();
        unlink_equity_account(&mut b.store, "u", &partner, &ada).unwrap();

        let err = close_to_partners(&mut b, 2023).unwrap_err();
        match err {
            ClosingError::PartnerHasNoCapitalAccount { partner, .. } => {
                assert_eq!(partner, "Ada");
            }
            other => panic!("expected PartnerHasNoCapitalAccount, got {other:?}"),
        }
    }

    #[test]
    fn a_sole_proprietorship_cannot_close_to_partner_capital() {
        let mut b = books();
        set_business_type(&b.store, "sole_proprietorship");
        b.ordinary_year(2023);

        let err = close_to_partners(&mut b, 2023).unwrap_err();
        assert!(
            matches!(err, ClosingError::NotAPartnership { year: 2023 }),
            "got {err:?}"
        );
    }

    /// The preview shows the split it would post, and agrees with what the close
    /// actually does.
    #[test]
    fn the_preview_shows_the_split() {
        let mut b = books();
        as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);

        let p = preview(
            b.store.connection(),
            2023,
            false,
            &ClosingTarget::PartnerCapital(b.equity.clone()),
        )
        .unwrap();
        assert!(p.blocker.is_none());
        assert_eq!(p.allocation.len(), 2);
        assert_eq!(
            p.allocation.iter().map(|a| a.cents).sum::<i64>(),
            p.net_income_cents
        );
        let ada = p
            .allocation
            .iter()
            .find(|a| a.partner_name == "Ada")
            .unwrap();
        assert_eq!(ada.cents, 108_000);
        assert!(ada.account_label.starts_with("3101"));

        let closed = close_to_partners(&mut b, 2023).unwrap();
        assert_eq!(closed.net_income_cents, p.net_income_cents);
    }

    /// The reason `capital.rs` had to change: the allocation lands in the
    /// contribution account, and item L must not then report it as capital the
    /// partner paid in *and* as their share of income.
    #[test]
    fn item_l_does_not_count_the_allocation_twice() {
        use crate::commands::partnership_commands::partners_for_year;

        let mut b = books();
        as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);
        close_to_partners(&mut b, 2023).unwrap();

        let partners = partners_for_year(b.store.connection(), 2023);
        let refs: Vec<&crate::domain::Partner> = partners.iter().collect();
        let capital =
            // No nondeductible expenses in this scenario, so item L row 4 is
            // zero and the identity below is the same one it always was.
            crate::tax::capital::compute(b.store.connection(), 2023, &refs, 1_800, 0).unwrap();

        let ada = capital
            .accounts
            .iter()
            .find(|a| a.partner_name == "Ada")
            .unwrap();
        assert_eq!(
            ada.contributed, 0,
            "the closing allocation is not a contribution"
        );
        assert_eq!(ada.net_income, 1_080, "it is her share of income, once");
        assert_eq!(
            ada.beginning + ada.contributed + ada.net_income - ada.withdrawals,
            1_080,
            "ending capital ties to the ledger"
        );
    }

    /// Two entries: the close puts the year's result in the year account, and the
    /// allocation moves it on to the partners — leaving nothing behind.
    #[test]
    fn closing_to_partners_posts_the_result_then_its_allocation() {
        let mut b = books();
        let (ada, bo) = as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);
        close_to_partners(&mut b, 2023).unwrap();

        let conn = b.store.connection();
        let close = closing_entry_for(conn, 2023).expect("closed");
        let allocation = allocation_entry_for(conn, 2023).expect("allocated");
        let lines = |entry: &str| -> Vec<(String, i64)> {
            let mut stmt = conn
                .prepare("SELECT account_id, amount FROM journal_lines WHERE entry_id = ?1")
                .unwrap();
            let rows: Vec<(String, i64)> = stmt
                .query_map([entry], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .flatten()
                .collect();
            rows
        };

        let close_lines = lines(&close);
        assert!(
            close_lines
                .iter()
                .any(|(a, amt)| *a == b.equity && *amt == -180_000),
            "the result lands in the year account: {close_lines:?}"
        );
        assert!(
            !close_lines.iter().any(|(a, _)| *a == ada || *a == bo),
            "and not straight in partner capital"
        );

        let mut moved = lines(&allocation);
        moved.sort();
        let mut expected = vec![
            (b.equity.clone(), 180_000),
            (ada.clone(), -108_000),
            (bo.clone(), -72_000),
        ];
        expected.sort();
        assert_eq!(moved, expected);
        assert_eq!(b.balance(&b.equity, day(2023, 12, 31)), 0);
    }

    /// A year closed into its account can be allocated afterwards, through the
    /// fence, exactly once — and the fence still refuses everything else.
    #[test]
    fn a_closed_year_can_be_allocated_afterwards_and_only_once() {
        let mut b = books();
        let (ada, bo) = as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);
        b.close(2023).unwrap();
        assert!(allocation_entry_for(b.store.connection(), 2023).is_none());

        let p = preview(
            b.store.connection(),
            2023,
            false,
            &ClosingTarget::Account(b.equity.clone()),
        )
        .unwrap();
        assert!(p.is_closed() && !p.is_allocated());
        assert!(p.allocation_blocker.is_none(), "{:?}", p.allocation_blocker);
        assert_eq!(p.allocation.iter().map(|s| s.cents).sum::<i64>(), 180_000);

        let done = allocate_to_partners(&mut b.store, "user", 2023).unwrap();
        assert_eq!(done.net_income_cents, 180_000);
        let end = day(2023, 12, 31);
        assert_eq!(b.balance(&ada, end), -108_000);
        assert_eq!(b.balance(&bo, end), -72_000);
        assert_eq!(b.balance(&b.equity, end), 0);

        assert!(
            matches!(
                allocate_to_partners(&mut b.store, "user", 2023),
                Err(ClosingError::AlreadyAllocated { year: 2023, .. })
            ),
            "only once"
        );

        let (cash, sales) = (b.cash.clone(), b.sales.clone());
        let late =
            EntryCommands::new(&mut b.store, "user".to_string()).post_entry(PostEntryCommand {
                date: day(2023, 11, 1),
                memo: "late".to_string(),
                lines: vec![
                    EntryLine::debit(&cash, 100, "USD"),
                    EntryLine::credit(&sales, 100, "USD"),
                ],
                reference: None,
                source: Some(JournalEntrySource::Manual),
            });
        assert!(
            late.is_err(),
            "an ordinary entry is still refused in the closed year"
        );
    }

    #[test]
    fn an_open_year_cannot_be_allocated() {
        let mut b = books();
        as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);
        assert!(matches!(
            allocate_to_partners(&mut b.store, "user", 2023),
            Err(ClosingError::NotClosed { year: 2023 })
        ));
    }

    /// Reopening takes the allocation with the close, so a reopened year never
    /// keeps a stale split — and it closes and allocates again cleanly.
    #[test]
    fn reopening_voids_the_allocation_with_the_close() {
        let mut b = books();
        let (ada, _bo) = as_a_partnership(&mut b, (60.0, 40.0));
        b.ordinary_year(2023);
        close_to_partners(&mut b, 2023).unwrap();

        reopen_books(&mut b.store, "user", 2023, "found a missing invoice").unwrap();
        let end = day(2023, 12, 31);
        assert!(allocation_entry_for(b.store.connection(), 2023).is_none());
        assert_eq!(b.balance(&ada, end), 0);
        assert_eq!(b.balance(&b.equity, end), 0);

        close_to_partners(&mut b, 2023).unwrap();
        assert_eq!(b.balance(&ada, end), -108_000);
        assert_eq!(b.balance(&b.equity, end), 0);
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
