use std::collections::{BTreeMap, HashSet};

use crate::domain::AccountType;
use crate::queries::account_queries::AccountQueries;
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ReportError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Query error: {0}")]
    QueryError(#[from] crate::queries::account_queries::AccountQueryError),
    #[error("Unbalanced trial balance: debits {0}, credits {1}")]
    UnbalancedTrialBalance(i64, i64),
    #[error(
        "the cash accounts moved by {movement} and the statement attributes {attributed}: the \
         two are read from the same entries, so a difference is a fault in the report rather \
         than a fact about the books"
    )]
    CashFlowDoesNotTie { movement: i64, attributed: i64 },
}

/// A line in the trial balance
#[derive(Debug, Clone)]
pub struct TrialBalanceLine {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub account_type: AccountType,
    pub debit: Option<i64>,
    pub credit: Option<i64>,
}

/// Trial balance report
#[derive(Debug, Clone)]
pub struct TrialBalance {
    pub as_of_date: Option<NaiveDate>,
    pub lines: Vec<TrialBalanceLine>,
    pub total_debits: i64,
    pub total_credits: i64,
    pub is_balanced: bool,
}

/// A line in the balance sheet
#[derive(Debug, Clone)]
pub struct BalanceSheetLine {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub account_type: AccountType,
    pub parent_id: Option<String>,
    pub balance: i64,
}

/// Balance sheet section
#[derive(Debug, Clone)]
pub struct BalanceSheetSection {
    pub name: String,
    pub lines: Vec<BalanceSheetLine>,
    pub total: i64,
}

/// Balance sheet report
#[derive(Debug, Clone)]
pub struct BalanceSheet {
    pub as_of_date: NaiveDate,
    pub assets: BalanceSheetSection,
    pub liabilities: BalanceSheetSection,
    pub equity: BalanceSheetSection,
    pub total_assets: i64,
    pub total_liabilities_and_equity: i64,
    pub is_balanced: bool,
}

/// A line in the income statement
#[derive(Debug, Clone)]
pub struct IncomeStatementLine {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub parent_id: Option<String>,
    pub balance: i64,
}

/// Income statement section
#[derive(Debug, Clone)]
pub struct IncomeStatementSection {
    pub name: String,
    pub lines: Vec<IncomeStatementLine>,
    pub total: i64,
}

/// Income statement (P&L) report
#[derive(Debug, Clone)]
pub struct IncomeStatement {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub revenue: IncomeStatementSection,
    pub expenses: IncomeStatementSection,
    pub net_income: i64,
}

// ---------------------------------------------------------------------------
// Cash flow
// ---------------------------------------------------------------------------

/// One account money came from or went to, over the period.
#[derive(Debug, Clone)]
pub struct CashFlowLine {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub parent_id: Option<String>,
    /// Signed as cash moved: positive is cash **in**.
    pub amount: i64,
}

/// One of the accounts the statement treats as cash.
#[derive(Debug, Clone)]
pub struct CashAccountMovement {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub opening: i64,
    pub closing: i64,
}

impl CashAccountMovement {
    pub fn net(&self) -> i64 {
        self.closing - self.opening
    }
}

/// Refuse a statement whose two halves disagree.
///
/// The identity that makes the report worth reading: the cash accounts' own movement,
/// read from their balances, and the movement the statement attributes to named
/// accounts, read from the entries. They hold by construction — the non-cash lines of
/// a balanced entry sum to the negative of its cash movement — so a difference means
/// the balances and the attribution were read under different filters, which is a
/// fault here and not a fact about the books. Shown figures would be wrong in a way
/// nobody could see, so they are not shown.
fn check_cash_flow_ties(flow: &CashFlow) -> Result<(), ReportError> {
    let movement: i64 = flow.cash_accounts.iter().map(|a| a.net()).sum();
    if movement != flow.net_change() {
        return Err(ReportError::CashFlowDoesNotTie {
            movement,
            attributed: flow.net_change(),
        });
    }
    Ok(())
}

/// Where cash came from and where it went, over a period.
///
/// # Direct method, and exact rather than apportioned
///
/// An entry is balanced, so its lines sum to zero. For any entry that touches a
/// cash account, the lines that are *not* on a cash account therefore sum to
/// exactly the negative of the cash movement — which means every dollar of cash
/// movement can be attributed to a named account with no apportioning and no
/// residual. That is what makes this report additive: `opening + in - out` is
/// `closing`, and [`Reports::cash_flow`] refuses to return a statement where it is
/// not.
///
/// The indirect method — net income plus non-cash adjustments — is not what this
/// is. It answers "why does profit differ from cash", which needs a working-capital
/// classification this chart does not carry. This answers "where did the money go",
/// which is the question somebody looking at a bank balance is asking.
///
/// # What a transfer between two cash accounts does
///
/// Nothing, correctly. Such an entry has no non-cash lines, so it contributes no
/// attribution, and the two cash movements cancel in the opening/closing figures.
/// A statement that showed it would report money both earned and spent.
#[derive(Debug, Clone)]
pub struct CashFlow {
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    /// The accounts treated as cash, with what they held at each end.
    pub cash_accounts: Vec<CashAccountMovement>,
    /// Accounts cash came from, largest first.
    pub inflows: Vec<CashFlowLine>,
    /// Accounts cash went to, largest first.
    pub outflows: Vec<CashFlowLine>,
    pub total_in: i64,
    pub total_out: i64,
}

impl CashFlow {
    pub fn opening_cash(&self) -> i64 {
        self.cash_accounts.iter().map(|a| a.opening).sum()
    }

    pub fn closing_cash(&self) -> i64 {
        self.cash_accounts.iter().map(|a| a.closing).sum()
    }

    /// The movement the attribution accounts for.
    pub fn net_change(&self) -> i64 {
        self.total_in - self.total_out
    }
}

/// Report generator
pub struct Reports<'a> {
    conn: &'a Connection,
}

impl<'a> Reports<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Generate trial balance
    pub fn trial_balance(
        &self,
        as_of_date: Option<NaiveDate>,
    ) -> Result<TrialBalance, ReportError> {
        let queries = AccountQueries::new(self.conn);
        let balances = queries.get_all_balances(as_of_date)?;

        let mut lines = Vec::new();
        let mut total_debits: i64 = 0;
        let mut total_credits: i64 = 0;

        for balance in balances {
            if balance.balance == 0 {
                continue; // Skip zero balances
            }

            let (debit, credit) = if balance.account_type.is_normal_debit() {
                // Asset/Expense: positive = debit balance
                if balance.balance > 0 {
                    total_debits += balance.balance;
                    (Some(balance.balance), None)
                } else {
                    total_credits += -balance.balance;
                    (None, Some(-balance.balance))
                }
            } else {
                // Liability/Equity/Revenue: negative = credit balance (normal)
                if balance.balance < 0 {
                    total_credits += -balance.balance;
                    (None, Some(-balance.balance))
                } else {
                    total_debits += balance.balance;
                    (Some(balance.balance), None)
                }
            };

            lines.push(TrialBalanceLine {
                account_id: balance.account_id,
                account_number: balance.account_number,
                account_name: balance.account_name,
                account_type: balance.account_type,
                debit,
                credit,
            });
        }

        // Sort by account number
        lines.sort_by(|a, b| a.account_number.cmp(&b.account_number));

        Ok(TrialBalance {
            as_of_date,
            lines,
            total_debits,
            total_credits,
            is_balanced: total_debits == total_credits,
        })
    }

    /// Generate balance sheet
    pub fn balance_sheet(&self, as_of_date: NaiveDate) -> Result<BalanceSheet, ReportError> {
        let queries = AccountQueries::new(self.conn);
        let balances = queries.get_all_balances(Some(as_of_date))?;

        let mut assets = Vec::new();
        let mut liabilities = Vec::new();
        let mut equity = Vec::new();

        for balance in balances {
            if balance.balance == 0 {
                continue;
            }

            let account = queries.get_account(&balance.account_id)?;
            let line = BalanceSheetLine {
                account_id: balance.account_id,
                account_number: balance.account_number,
                account_name: balance.account_name,
                account_type: balance.account_type,
                parent_id: account.parent_id,
                balance: balance.balance,
            };

            match balance.account_type {
                AccountType::Asset => assets.push(line),
                AccountType::Liability => liabilities.push(line),
                AccountType::Equity => equity.push(line),
                _ => {} // Revenue/Expense not on balance sheet directly
            }
        }

        // Income not yet swept into equity by a close, for the equity section.
        // Starts the day after the last closed year rather than at the beginning
        // of time: once a year is closed its result sits in a real equity
        // account, and counting it here as well would show it twice and take the
        // sheet out of balance.
        let income =
            self.calculate_net_income(self.unclosed_from(as_of_date)?, Some(as_of_date))?;
        if income != 0 {
            equity.push(BalanceSheetLine {
                account_id: "__net_income__".to_string(),
                account_number: "".to_string(),
                account_name: "Current Year Net Income".to_string(),
                account_type: AccountType::Equity,
                parent_id: None,
                balance: -income, // Credit balance
            });
        }

        // Backfill ancestor accounts so the tree is complete
        self.backfill_bs_ancestors(&queries, &mut assets)?;
        self.backfill_bs_ancestors(&queries, &mut liabilities)?;
        self.backfill_bs_ancestors(&queries, &mut equity)?;

        let total_assets: i64 = assets.iter().map(|l| l.balance).sum();
        // For credit-normal accounts, negate balance to convert:
        // - Credit balances (negative) → positive (normal)
        // - Debit balances (positive) → negative (reduces total, e.g., net loss)
        let total_liabilities: i64 = liabilities.iter().map(|l| -l.balance).sum();
        let total_equity: i64 = equity.iter().map(|l| -l.balance).sum();

        Ok(BalanceSheet {
            as_of_date,
            assets: BalanceSheetSection {
                name: "Assets".to_string(),
                lines: assets,
                total: total_assets,
            },
            liabilities: BalanceSheetSection {
                name: "Liabilities".to_string(),
                lines: liabilities,
                total: total_liabilities,
            },
            equity: BalanceSheetSection {
                name: "Equity".to_string(),
                lines: equity,
                total: total_equity,
            },
            total_assets,
            total_liabilities_and_equity: total_liabilities + total_equity,
            is_balanced: total_assets == (total_liabilities + total_equity),
        })
    }

    /// Generate income statement (P&L)
    pub fn income_statement(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<IncomeStatement, ReportError> {
        let mut revenue_lines = Vec::new();
        let mut expense_lines = Vec::new();

        // Get balances for the period
        let queries = AccountQueries::new(self.conn);
        // Every account, not just the active ones: an expense account
        // deactivated in July still holds what was posted to it in June, and a
        // P&L that leaves it out understates the year and does not tie to the
        // trial balance.
        let accounts = queries.get_all_accounts()?;

        for account in accounts {
            if !account.account_type.is_income_statement() {
                continue;
            }

            // Movement during the period, with year-end closing entries left
            // out — see `AccountQueries::period_movement` for why that matters.
            let period_change = queries.period_movement(&account.id, start_date, end_date)?;

            if period_change == 0 {
                continue;
            }

            // Orient to the account's normal side (revenue is credit-normal,
            // expense debit-normal) rather than taking the magnitude. This keeps
            // contra activity signed correctly — e.g. a refunds account inside
            // Revenue carries a debit balance and must *subtract* from revenue,
            // not add to it.
            let balance = match account.account_type {
                AccountType::Revenue => -period_change,
                _ => period_change, // Expense
            };

            let line = IncomeStatementLine {
                account_id: account.id.clone(),
                account_number: account.account_number,
                account_name: account.name,
                parent_id: account.parent_id,
                balance,
            };

            match account.account_type {
                AccountType::Revenue => revenue_lines.push(line),
                AccountType::Expense => expense_lines.push(line),
                _ => {}
            }
        }

        // Backfill ancestor accounts so the tree is complete
        self.backfill_is_ancestors(&queries, &mut revenue_lines)?;
        self.backfill_is_ancestors(&queries, &mut expense_lines)?;

        let total_revenue: i64 = revenue_lines.iter().map(|l| l.balance).sum();
        let total_expenses: i64 = expense_lines.iter().map(|l| l.balance).sum();
        let net_income = total_revenue - total_expenses;

        Ok(IncomeStatement {
            start_date,
            end_date,
            revenue: IncomeStatementSection {
                name: "Revenue".to_string(),
                lines: revenue_lines,
                total: total_revenue,
            },
            expenses: IncomeStatementSection {
                name: "Expenses".to_string(),
                lines: expense_lines,
                total: total_expenses,
            },
            net_income,
        })
    }

    /// Ensure all ancestor accounts are present in a balance sheet section.
    /// Parent accounts that have no direct balance are added with balance 0
    /// so the tree structure is complete for display.
    fn backfill_bs_ancestors(
        &self,
        queries: &AccountQueries,
        lines: &mut Vec<BalanceSheetLine>,
    ) -> Result<(), ReportError> {
        let existing_ids: HashSet<String> = lines.iter().map(|l| l.account_id.clone()).collect();
        let mut to_add: Vec<BalanceSheetLine> = Vec::new();
        let mut seen: HashSet<String> = existing_ids.clone();

        for line in lines.iter() {
            let mut parent_id = line.parent_id.clone();
            while let Some(pid) = parent_id {
                if seen.contains(&pid) {
                    break;
                }
                seen.insert(pid.clone());
                let parent = queries.get_account(&pid)?;
                parent_id = parent.parent_id.clone();
                to_add.push(BalanceSheetLine {
                    account_id: parent.id,
                    account_number: parent.account_number,
                    account_name: parent.name,
                    account_type: parent.account_type,
                    parent_id: parent.parent_id,
                    balance: 0,
                });
            }
        }

        lines.extend(to_add);
        Ok(())
    }

    /// Ensure all ancestor accounts are present in an income statement section.
    fn backfill_is_ancestors(
        &self,
        queries: &AccountQueries,
        lines: &mut Vec<IncomeStatementLine>,
    ) -> Result<(), ReportError> {
        let existing_ids: HashSet<String> = lines.iter().map(|l| l.account_id.clone()).collect();
        let mut to_add: Vec<IncomeStatementLine> = Vec::new();
        let mut seen: HashSet<String> = existing_ids.clone();

        for line in lines.iter() {
            let mut parent_id = line.parent_id.clone();
            while let Some(pid) = parent_id {
                if seen.contains(&pid) {
                    break;
                }
                seen.insert(pid.clone());
                let parent = queries.get_account(&pid)?;
                parent_id = parent.parent_id.clone();
                to_add.push(IncomeStatementLine {
                    account_id: parent.id,
                    account_number: parent.account_number,
                    account_name: parent.name,
                    parent_id: parent.parent_id,
                    balance: 0,
                });
            }
        }

        lines.extend(to_add);
        Ok(())
    }

    /// Calculate net income for a period
    /// The day after the latest fiscal year that is closed as of `as_of` — the
    /// point from which income has *not* yet been swept into equity.
    ///
    /// `None` when nothing is closed, which is every ledger that has never run a
    /// year-end close and is the behaviour this report has always had.
    fn unclosed_from(&self, as_of: NaiveDate) -> Result<Option<NaiveDate>, ReportError> {
        let last_closed: Option<String> = self
            .conn
            .query_row(
                "SELECT MAX(end_date) FROM fiscal_years
                 WHERE is_closed = 1 AND end_date <= ?1",
                [as_of.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();

        Ok(match last_closed {
            None => None,
            Some(d) => NaiveDate::parse_from_str(&d, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.succ_opt()),
        })
    }

    fn calculate_net_income(
        &self,
        start_date: Option<NaiveDate>,
        end_date: Option<NaiveDate>,
    ) -> Result<i64, ReportError> {
        // Revenue is credit balance (negative in our system)
        // Expense is debit balance (positive)
        // Net income = Revenue - Expenses

        let mut sql = String::from(
            "SELECT COALESCE(SUM(CASE WHEN a.account_type = 'revenue' THEN -jl.amount ELSE 0 END), 0) -
                    COALESCE(SUM(CASE WHEN a.account_type = 'expense' THEN jl.amount ELSE 0 END), 0)
             FROM journal_lines jl
             JOIN journal_entries je ON jl.entry_id = je.id
             JOIN accounts a ON jl.account_id = a.id
             WHERE je.is_void = 0 AND a.account_type IN ('revenue', 'expense')
               AND (je.source IS NULL OR je.source != 'closing')",
        );

        if start_date.is_some() || end_date.is_some() {
            if let Some(start) = start_date {
                sql.push_str(&format!(" AND je.date >= '{}'", start));
            }
            if let Some(end) = end_date {
                sql.push_str(&format!(" AND je.date <= '{}'", end));
            }
        }

        let net_income: i64 = self.conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(net_income)
    }

    /// Get account activity summary
    /// The accounts a cash flow statement covers unless somebody says otherwise.
    ///
    /// Asset accounts the bank feed maps to an account the provider itself calls
    /// `depository` — a chequing or savings account. Derived from what the provider
    /// said rather than guessed from a name, because "Cash" in an account's name is
    /// not evidence and a chart is free to call a bank account anything.
    ///
    /// Credit cards are deliberately absent. Paying one is cash leaving; what was
    /// bought with it left cash when the card was paid, not when it was swiped, and
    /// a statement that counted both would double every card purchase.
    pub fn default_cash_accounts(&self) -> Result<Vec<String>, ReportError> {
        // A book old enough to predate the bank feed has no such table, and the
        // report still has to open — with no default, which reads as "choose the
        // accounts" rather than as an error.
        let has_links: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'plaid_local_accounts'",
                [],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !has_links {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT a.id
               FROM plaid_local_accounts pa
               JOIN accounts a ON a.id = pa.local_account_id
              WHERE lower(pa.account_type) = 'depository'
                AND a.account_type = 'asset'
              ORDER BY a.account_number",
        )?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Where cash came from and where it went, between two dates inclusive.
    ///
    /// See [`CashFlow`] for why the attribution is exact rather than apportioned,
    /// and why a transfer between two cash accounts contributes nothing.
    pub fn cash_flow(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
        cash_account_ids: &[String],
    ) -> Result<CashFlow, ReportError> {
        let queries = AccountQueries::new(self.conn);
        let cash: HashSet<&str> = cash_account_ids.iter().map(|s| s.as_str()).collect();

        let mut cash_accounts = Vec::new();
        for id in cash_account_ids {
            let account = queries.get_account(id)?;
            // The day before the period starts: an opening balance is what was there
            // before anything in the window happened.
            let before = start_date.pred_opt().unwrap_or(start_date);
            cash_accounts.push(CashAccountMovement {
                account_id: account.id.clone(),
                account_number: account.account_number.clone(),
                account_name: account.name.clone(),
                opening: queries.get_account_balance(id, Some(before))?.balance,
                closing: queries.get_account_balance(id, Some(end_date))?.balance,
            });
        }

        // Every line of every live entry that touches a cash account, cash lines
        // included — they are filtered here rather than in SQL so the attribution and
        // the movement are read from one result set and cannot disagree.
        let mut by_account: BTreeMap<String, i64> = BTreeMap::new();
        if !cash.is_empty() {
            // Every index is written out. A bare `?` takes one more than the largest
            // index seen so far, so mixing the two forms made the account list bind
            // *after* the dates and the statement ask for a parameter nobody passed.
            let placeholders = (3..3 + cash_account_ids.len())
                .map(|n| format!("?{n}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT jl.account_id, jl.amount
                   FROM journal_lines jl
                   JOIN journal_entries je ON je.id = jl.entry_id
                  WHERE je.is_void = 0
                    AND je.date >= ?1 AND je.date <= ?2
                    AND jl.entry_id IN (
                        SELECT entry_id FROM journal_lines WHERE account_id IN ({placeholders})
                    )"
            );
            let mut params: Vec<String> = vec![start_date.to_string(), end_date.to_string()];
            params.extend(cash_account_ids.iter().cloned());
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (account_id, amount) = row?;
                if cash.contains(account_id.as_str()) {
                    continue;
                }
                // Negated: a line crediting Income by 100 is 100 of cash coming in.
                *by_account.entry(account_id).or_insert(0) -= amount;
            }
        }

        let mut inflows = Vec::new();
        let mut outflows = Vec::new();
        for (account_id, amount) in by_account {
            if amount == 0 {
                continue;
            }
            let account = queries.get_account(&account_id)?;
            let line = CashFlowLine {
                account_id,
                account_number: account.account_number,
                account_name: account.name,
                parent_id: account.parent_id,
                amount,
            };
            if amount > 0 {
                inflows.push(line);
            } else {
                outflows.push(line);
            }
        }
        // Largest first on both sides: the biggest thing is why anybody opened this.
        inflows.sort_by_key(|l| -l.amount);
        outflows.sort_by_key(|l| l.amount);

        let total_in: i64 = inflows.iter().map(|l| l.amount).sum();
        let total_out: i64 = outflows.iter().map(|l| -l.amount).sum();

        let flow = CashFlow {
            start_date,
            end_date,
            cash_accounts,
            inflows,
            outflows,
            total_in,
            total_out,
        };
        check_cash_flow_ties(&flow)?;
        Ok(flow)
    }

    pub fn account_activity_summary(
        &self,
        account_id: &str,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<AccountActivitySummary, ReportError> {
        let queries = AccountQueries::new(self.conn);
        let account = queries.get_account(account_id)?;

        let opening_balance = queries
            .get_account_balance(
                account_id,
                Some(start_date.pred_opt().unwrap_or(start_date)),
            )?
            .balance;

        let closing_balance = queries
            .get_account_balance(account_id, Some(end_date))?
            .balance;

        // Get debit and credit totals for the period
        let (total_debits, total_credits): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(CASE WHEN jl.amount > 0 THEN jl.amount ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN jl.amount < 0 THEN -jl.amount ELSE 0 END), 0)
             FROM journal_lines jl
             JOIN journal_entries je ON jl.entry_id = je.id
             WHERE jl.account_id = ?1 AND je.date >= ?2 AND je.date <= ?3 AND je.is_void = 0",
            rusqlite::params![account_id, start_date.to_string(), end_date.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let transaction_count: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT je.id)
             FROM journal_lines jl
             JOIN journal_entries je ON jl.entry_id = je.id
             WHERE jl.account_id = ?1 AND je.date >= ?2 AND je.date <= ?3 AND je.is_void = 0",
            rusqlite::params![account_id, start_date.to_string(), end_date.to_string()],
            |row| row.get(0),
        )?;

        Ok(AccountActivitySummary {
            account_id: account_id.to_string(),
            account_name: account.name,
            account_type: account.account_type,
            start_date,
            end_date,
            opening_balance,
            total_debits,
            total_credits,
            closing_balance,
            transaction_count: transaction_count as u32,
        })
    }
}

/// Summary of account activity for a period
#[derive(Debug, Clone)]
pub struct AccountActivitySummary {
    pub account_id: String,
    pub account_name: String,
    pub account_type: AccountType,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub opening_balance: i64,
    pub total_debits: i64,
    pub total_credits: i64,
    pub closing_balance: i64,
    pub transaction_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{
        Event, EventAccountType, EventEnvelope, JournalLineData, StoredEvent,
    };
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::store::projections::ProjectionStore;

    fn setup() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    fn append_and_project(store: &mut EventStore, event: Event, user_id: &str) -> StoredEvent {
        let stored = store
            .append(EventEnvelope::new(event, user_id.to_string()))
            .unwrap();
        {
            store.apply_projection(&stored).unwrap();
        }
        stored
    }

    pub(crate) fn create_accounts_and_entries(store: &mut EventStore) {
        // Create accounts
        let accounts = vec![
            ("cash", EventAccountType::Asset, "1000", "Cash"),
            ("ar", EventAccountType::Asset, "1100", "Accounts Receivable"),
            (
                "ap",
                EventAccountType::Liability,
                "2000",
                "Accounts Payable",
            ),
            ("equity", EventAccountType::Equity, "3000", "Owner's Equity"),
            (
                "revenue",
                EventAccountType::Revenue,
                "4000",
                "Sales Revenue",
            ),
            (
                "expense",
                EventAccountType::Expense,
                "5000",
                "Supplies Expense",
            ),
        ];

        for (id, acc_type, number, name) in accounts {
            let event = Event::AccountCreated {
                account_id: id.to_string(),
                account_type: acc_type,
                account_number: number.to_string(),
                name: name.to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            };
            append_and_project(store, event, "user");
        }

        // Initial investment: Cash DR, Equity CR
        let entry1 = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            memo: "Initial investment".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "l001-1".to_string(),
                    account_id: "cash".to_string(),
                    amount: 100000, // $1000 DR
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "l001-2".to_string(),
                    account_id: "equity".to_string(),
                    amount: -100000, // $1000 CR
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        append_and_project(store, entry1, "user");

        // Sale: AR DR, Revenue CR
        let entry2 = Event::JournalEntryPosted {
            entry_id: "entry-002".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Sales".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "l002-1".to_string(),
                    account_id: "ar".to_string(),
                    amount: 50000, // $500 DR
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "l002-2".to_string(),
                    account_id: "revenue".to_string(),
                    amount: -50000, // $500 CR
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        append_and_project(store, entry2, "user");

        // Expense: Expense DR, Cash CR
        let entry3 = Event::JournalEntryPosted {
            entry_id: "entry-003".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 20).unwrap(),
            memo: "Supplies purchased".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "l003-1".to_string(),
                    account_id: "expense".to_string(),
                    amount: 20000, // $200 DR
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "l003-2".to_string(),
                    account_id: "cash".to_string(),
                    amount: -20000, // $200 CR
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        append_and_project(store, entry3, "user");
    }

    #[test]
    fn test_trial_balance() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);

        let reports = Reports::new(store.connection());
        let tb = reports.trial_balance(None).unwrap();

        assert!(tb.is_balanced);
        assert_eq!(tb.total_debits, tb.total_credits);

        // Total should be $1000 + $500 + $200 = $1700 (debits) = $1000 + $500 + $200 = $1700 (credits)
        // Actually: Cash DR 80000, AR DR 50000, Expense DR 20000 = 150000
        // Equity CR 100000, Revenue CR 50000 = 150000
        assert_eq!(tb.total_debits, 150000);
    }

    #[test]
    fn test_income_statement() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);

        let reports = Reports::new(store.connection());
        let pl = reports
            .income_statement(
                NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            )
            .unwrap();

        assert_eq!(pl.revenue.total, 50000); // $500 revenue
        assert_eq!(pl.expenses.total, 20000); // $200 expense
        assert_eq!(pl.net_income, 30000); // $300 net income
    }

    #[test]
    fn income_statement_nets_contra_revenue() {
        let mut store = setup();
        for (id, ty, num, name) in [
            ("cash", EventAccountType::Asset, "1000", "Cash"),
            ("sales", EventAccountType::Revenue, "4000", "Sales"),
            ("refunds", EventAccountType::Revenue, "4900", "Refunds"),
        ] {
            append_and_project(
                &mut store,
                Event::AccountCreated {
                    account_id: id.to_string(),
                    account_type: ty,
                    account_number: num.to_string(),
                    name: name.to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                },
                "user",
            );
        }

        // $15,000 of sales: Cash DR / Sales CR.
        append_and_project(
            &mut store,
            Event::JournalEntryPosted {
                entry_id: "s1".to_string(),
                date: NaiveDate::from_ymd_opt(2024, 1, 10).unwrap(),
                memo: "sales".to_string(),
                lines: vec![
                    JournalLineData {
                        line_id: "s1a".to_string(),
                        account_id: "cash".to_string(),
                        amount: 1_500_000,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    },
                    JournalLineData {
                        line_id: "s1b".to_string(),
                        account_id: "sales".to_string(),
                        amount: -1_500_000,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    },
                ],
                reference: None,
                source: None,
            },
            "user",
        );

        // $4,000 of refunds: Refunds DR (contra-revenue) / Cash CR.
        append_and_project(
            &mut store,
            Event::JournalEntryPosted {
                entry_id: "r1".to_string(),
                date: NaiveDate::from_ymd_opt(2024, 1, 20).unwrap(),
                memo: "refund".to_string(),
                lines: vec![
                    JournalLineData {
                        line_id: "r1a".to_string(),
                        account_id: "refunds".to_string(),
                        amount: 400_000,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    },
                    JournalLineData {
                        line_id: "r1b".to_string(),
                        account_id: "cash".to_string(),
                        amount: -400_000,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    },
                ],
                reference: None,
                source: None,
            },
            "user",
        );

        let reports = Reports::new(store.connection());
        let pl = reports
            .income_statement(
                NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            )
            .unwrap();

        // $15,000 − $4,000 = $11,000 net revenue — NOT $19,000.
        assert_eq!(pl.revenue.total, 1_100_000);
        assert_eq!(pl.net_income, 1_100_000);
        // The refunds line itself reads negative (contra-revenue).
        let refunds = pl
            .revenue
            .lines
            .iter()
            .find(|l| l.account_id == "refunds")
            .unwrap();
        assert_eq!(refunds.balance, -400_000);
    }

    #[test]
    fn test_balance_sheet() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);

        let reports = Reports::new(store.connection());
        let bs = reports
            .balance_sheet(NaiveDate::from_ymd_opt(2024, 1, 31).unwrap())
            .unwrap();

        // Assets: Cash $800 + AR $500 = $1300
        assert_eq!(bs.total_assets, 130000);

        // L&E: Equity $1000 + Net Income $300 = $1300
        assert_eq!(bs.total_liabilities_and_equity, 130000);
        assert!(bs.is_balanced);
    }
}

/// What a year-end close does to the reports that read the year it closed.
///
/// These are regression tests for a failure that is silent rather than loud: a
/// closing entry is dated inside the year it closes and zeroes every revenue and
/// expense account, so a report that counts it reports the year as having earned
/// nothing — and produces a *plausible, wrong* tax return rather than an error.
#[cfg(test)]
mod closing_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::closing_commands::{close_books, CloseBooksCommand, ClosingTarget};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::events::types::JournalEntrySource;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    struct Books {
        store: EventStore,
        cash: String,
        sales: String,
        rent: String,
        equity: String,
    }

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

        for (ty, number, name) in [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Revenue, "4000", "Sales"),
            (AccountType::Expense, "6100", "Rent"),
            (AccountType::Equity, "3023", "2023"),
        ] {
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
            rent: id(&store, "6100"),
            equity: id(&store, "3023"),
            store,
        }
    }

    impl Books {
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

        /// 5,000 of sales against 3,000 of rent: net income 2,000.
        fn year(&mut self, year: i32) {
            let (cash, sales, rent) = (self.cash.clone(), self.sales.clone(), self.rent.clone());
            self.post(day(year, 3, 1), &cash, &sales, 500_000);
            self.post(day(year, 9, 1), &rent, &cash, 300_000);
        }

        fn close(&mut self, year: i32) {
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
            .unwrap();
        }
    }

    /// The one that protects the tax return: a closed year still reports the
    /// revenue, expenses and net income it actually had.
    #[test]
    fn a_closed_year_still_reports_its_own_income_statement() {
        let mut b = books();
        b.year(2023);

        let before = Reports::new(b.store.connection())
            .income_statement(day(2023, 1, 1), day(2023, 12, 31))
            .unwrap();
        assert_eq!(before.revenue.total, 500_000);
        assert_eq!(before.expenses.total, 300_000);
        assert_eq!(before.net_income, 200_000);

        b.close(2023);

        let after = Reports::new(b.store.connection())
            .income_statement(day(2023, 1, 1), day(2023, 12, 31))
            .unwrap();
        assert_eq!(
            (after.revenue.total, after.expenses.total, after.net_income),
            (500_000, 300_000, 200_000),
            "closing the year must not erase the year"
        );
    }

    /// The other half: after the close the *equity* holds the result, and the
    /// synthetic "current year net income" line must not report it a second time.
    #[test]
    fn the_balance_sheet_balances_after_a_close_without_double_counting() {
        let mut b = books();
        b.year(2023);
        b.close(2023);

        let bs = Reports::new(b.store.connection())
            .balance_sheet(day(2023, 12, 31))
            .unwrap();

        assert!(
            bs.is_balanced,
            "assets {} vs L+E {}",
            bs.total_assets, bs.total_liabilities_and_equity
        );
        assert_eq!(bs.total_assets, 200_000);
        assert_eq!(bs.equity.total, 200_000);
        assert!(
            !bs.equity
                .lines
                .iter()
                .any(|l| l.account_id == "__net_income__"),
            "the year's result is in a real account now; the synthetic line would double it"
        );
        assert!(
            bs.equity
                .lines
                .iter()
                .any(|l| l.account_number == "3023" && l.balance == -200_000),
            "the year account carries the result: {:?}",
            bs.equity.lines
        );
    }

    /// A balance sheet drawn part-way through the *next* year shows only that
    /// year's activity as unswept income.
    #[test]
    fn income_after_a_close_belongs_to_the_year_that_earned_it() {
        let mut b = books();
        b.year(2023);
        b.close(2023);

        let (cash, sales) = (b.cash.clone(), b.sales.clone());
        b.post(day(2024, 2, 1), &cash, &sales, 90_000);

        let bs = Reports::new(b.store.connection())
            .balance_sheet(day(2024, 6, 30))
            .unwrap();
        assert!(bs.is_balanced);

        let synthetic = bs
            .equity
            .lines
            .iter()
            .find(|l| l.account_id == "__net_income__")
            .expect("2024 is not closed, so its income shows as the current-year line");
        assert_eq!(
            -synthetic.balance, 90_000,
            "only 2024's income — 2023's is in the year account"
        );
        assert_eq!(bs.equity.total, 290_000);
    }

    /// A year still open is unaffected by any of this — the behaviour every
    /// ledger has today, which the fix must not disturb.
    #[test]
    fn an_open_year_reports_exactly_as_it_always_did() {
        let mut b = books();
        b.year(2023);

        let bs = Reports::new(b.store.connection())
            .balance_sheet(day(2023, 12, 31))
            .unwrap();
        assert!(bs.is_balanced);
        let synthetic = bs
            .equity
            .lines
            .iter()
            .find(|l| l.account_id == "__net_income__")
            .expect("an open year's income shows as the current-year line");
        assert_eq!(-synthetic.balance, 200_000);
    }

    /// Two closed years each keep their own result, and neither leaks into the
    /// other's income statement.
    #[test]
    fn consecutive_closed_years_each_keep_their_own_result() {
        let mut b = books();
        b.year(2022);
        b.year(2023);

        // A second year account, so each close has its own home.
        AccountCommands::new(&mut b.store, "user".to_string())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Equity,
                account_number: "3022".to_string(),
                name: "2022".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
        let equity_2022: String = b
            .store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '3022'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        close_books(
            &mut b.store,
            "user",
            CloseBooksCommand {
                year: 2022,
                target: ClosingTarget::Account(equity_2022),
                include_draws: false,
            },
        )
        .unwrap();
        b.close(2023);

        let reports = Reports::new(b.store.connection());
        for year in [2022, 2023] {
            let is = reports
                .income_statement(day(year, 1, 1), day(year, 12, 31))
                .unwrap();
            assert_eq!(
                is.net_income, 200_000,
                "{year} should still report its own 2,000"
            );
        }

        let bs = reports.balance_sheet(day(2023, 12, 31)).unwrap();
        assert!(bs.is_balanced);
        assert_eq!(bs.equity.total, 400_000, "both years' results, once each");
    }
}

/// Deactivated accounts that still carry a balance.
///
/// Deactivating an account means "do not post to this again". It does not
/// unwrite what was posted to it, so a report that filters on the active flag
/// silently loses that balance — and then does not foot, with nothing on the
/// page to say which account went missing or why the totals disagree.
#[cfg(test)]
mod deactivated_account_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::events::types::JournalEntrySource;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Cash, a revenue account and an expense account; 500 of sales and 300 of
    /// rent in 2023. The expense account is then deactivated, still holding its
    /// 300.
    fn books_with_a_deactivated_expense() -> (EventStore, String) {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let mut store = store;

        for (ty, number, name) in [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Revenue, "4000", "Sales"),
            (AccountType::Expense, "6100", "Rent"),
        ] {
            AccountCommands::new(&mut store, "u".to_string())
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
        let (cash, sales, rent) = (id(&store, "1000"), id(&store, "4000"), id(&store, "6100"));

        for (date, debit, credit, cents) in [
            (day(2023, 3, 1), &cash, &sales, 50_000i64),
            (day(2023, 6, 1), &rent, &cash, 30_000),
        ] {
            EntryCommands::new(&mut store, "u".to_string())
                .post_entry(PostEntryCommand {
                    date,
                    memo: "t".to_string(),
                    lines: vec![
                        EntryLine::debit(debit, cents, "USD"),
                        EntryLine::credit(credit, cents, "USD"),
                    ],
                    reference: None,
                    source: Some(JournalEntrySource::Manual),
                })
                .unwrap();
        }

        store
            .connection()
            .execute("UPDATE accounts SET is_active = 0 WHERE id = ?1", [&rent])
            .unwrap();

        (store, rent)
    }

    #[test]
    fn the_trial_balance_still_foots() {
        let (store, rent) = books_with_a_deactivated_expense();
        let tb = Reports::new(store.connection())
            .trial_balance(Some(day(2023, 12, 31)))
            .unwrap();

        assert!(
            tb.is_balanced,
            "debits {} credits {} — a deactivated account took its balance out of one side",
            tb.total_debits, tb.total_credits
        );
        assert!(
            tb.lines.iter().any(|l| l.account_id == rent),
            "the deactivated account has a balance and belongs on the trial balance"
        );
    }

    #[test]
    fn the_income_statement_still_counts_it() {
        let (store, _) = books_with_a_deactivated_expense();
        let is = Reports::new(store.connection())
            .income_statement(day(2023, 1, 1), day(2023, 12, 31))
            .unwrap();

        assert_eq!(is.revenue.total, 50_000);
        assert_eq!(
            is.expenses.total, 30_000,
            "the deactivated rent still happened"
        );
        assert_eq!(is.net_income, 20_000);
    }

    #[test]
    fn the_balance_sheet_still_balances() {
        let (store, _) = books_with_a_deactivated_expense();
        let bs = Reports::new(store.connection())
            .balance_sheet(day(2023, 12, 31))
            .unwrap();

        assert!(bs.is_balanced);
        assert_eq!(bs.total_assets, 20_000);
        assert_eq!(bs.equity.total, 20_000);
    }

    /// The close still refuses rather than sweeping into an account nothing can
    /// post to — the reports seeing the account does not make it postable.
    #[test]
    fn closing_still_refuses_while_the_account_is_deactivated() {
        use crate::commands::closing_commands::{
            close_books, CloseBooksCommand, ClosingError, ClosingTarget,
        };

        let (mut store, _) = books_with_a_deactivated_expense();
        AccountCommands::new(&mut store, "u".to_string())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Equity,
                account_number: "3023".to_string(),
                name: "2023".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
        let equity: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '3023'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let err = close_books(
            &mut store,
            "u",
            CloseBooksCommand {
                year: 2023,
                target: ClosingTarget::Account(equity),
                include_draws: false,
            },
        )
        .unwrap_err();
        assert!(
            matches!(err, ClosingError::InactiveAccountHoldsBalance { .. }),
            "got {err:?}"
        );
    }
}

#[cfg(test)]
mod cash_flow_tests {
    use super::*;
    use crate::events::types::{
        Event, EventAccountType, EventEnvelope, JournalLineData, StoredEvent,
    };
    use crate::queries::reports::tests::create_accounts_and_entries;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::store::projections::ProjectionStore;

    fn setup() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    fn append_and_project(store: &mut EventStore, event: Event, user_id: &str) -> StoredEvent {
        let stored = store
            .append(EventEnvelope::new(event, user_id.to_string()))
            .unwrap();
        store.apply_projection(&stored).unwrap();
        stored
    }

    // -----------------------------------------------------------------------
    // Cash flow
    // -----------------------------------------------------------------------

    fn entry(id: &str, date: NaiveDate, lines: &[(&str, i64)]) -> Event {
        Event::JournalEntryPosted {
            entry_id: id.to_string(),
            date,
            memo: id.to_string(),
            lines: lines
                .iter()
                .enumerate()
                .map(|(i, (account, amount))| JournalLineData {
                    line_id: format!("{id}-{i}"),
                    account_id: account.to_string(),
                    amount: *amount,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                })
                .collect(),
            reference: None,
            source: None,
        }
    }

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Cash in from revenue, cash out to an expense, and the receivable sale left
    /// out: it moved no cash, which is the whole difference between this report and
    /// the income statement.
    #[test]
    fn the_cash_flow_names_where_the_money_came_from_and_went() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        let reports = Reports::new(store.connection());
        let flow = reports
            .cash_flow(day(2024, 1, 1), day(2024, 1, 31), &["cash".to_string()])
            .expect("ties");

        assert_eq!(flow.opening_cash(), 0);
        assert_eq!(flow.closing_cash(), 80_000);
        assert_eq!(flow.total_in, 100_000, "the owner's investment");
        assert_eq!(flow.total_out, 20_000, "the supplies");
        assert_eq!(flow.net_change(), 80_000);
        assert_eq!(
            flow.inflows
                .iter()
                .map(|l| l.account_id.as_str())
                .collect::<Vec<_>>(),
            vec!["equity"]
        );
        assert_eq!(
            flow.outflows
                .iter()
                .map(|l| l.account_id.as_str())
                .collect::<Vec<_>>(),
            vec!["expense"]
        );
        assert!(
            flow.inflows.iter().all(|l| l.account_id != "revenue"),
            "the sale was on credit: it earned $500 and moved no cash"
        );
    }

    /// A transfer between two cash accounts is not income and not spending. The
    /// report has to say nothing about it at all — anything else reports the same
    /// money as both.
    #[test]
    fn a_transfer_between_two_cash_accounts_is_not_a_flow() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        append_and_project(
            &mut store,
            Event::AccountCreated {
                account_id: "savings".to_string(),
                account_type: EventAccountType::Asset,
                account_number: "1001".to_string(),
                name: "Savings".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            },
            "user",
        );
        append_and_project(
            &mut store,
            entry(
                "transfer",
                day(2024, 1, 25),
                &[("savings", 30_000), ("cash", -30_000)],
            ),
            "user",
        );
        let reports = Reports::new(store.connection());
        let flow = reports
            .cash_flow(
                day(2024, 1, 1),
                day(2024, 1, 31),
                &["cash".to_string(), "savings".to_string()],
            )
            .expect("ties");

        assert_eq!(flow.total_in, 100_000);
        assert_eq!(flow.total_out, 20_000);
        assert!(
            flow.inflows
                .iter()
                .chain(&flow.outflows)
                .all(|l| { l.account_id != "cash" && l.account_id != "savings" }),
            "a cash account is never a line of its own statement"
        );
        assert_eq!(
            flow.closing_cash(),
            80_000,
            "the transfer moved no cash out"
        );
    }

    /// With only one of the two in the cash set, the same transfer *is* a flow — the
    /// statement is about the accounts it was asked about, and money leaving them for
    /// an account it was not asked about has left.
    #[test]
    fn a_transfer_out_of_the_cash_set_is_a_flow() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        append_and_project(
            &mut store,
            Event::AccountCreated {
                account_id: "savings".to_string(),
                account_type: EventAccountType::Asset,
                account_number: "1001".to_string(),
                name: "Savings".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            },
            "user",
        );
        append_and_project(
            &mut store,
            entry(
                "transfer",
                day(2024, 1, 25),
                &[("savings", 30_000), ("cash", -30_000)],
            ),
            "user",
        );
        let reports = Reports::new(store.connection());
        let flow = reports
            .cash_flow(day(2024, 1, 1), day(2024, 1, 31), &["cash".to_string()])
            .expect("ties");
        assert_eq!(flow.total_out, 50_000, "supplies and the transfer out");
        assert!(flow.outflows.iter().any(|l| l.account_id == "savings"));
        assert_eq!(flow.closing_cash(), 50_000);
    }

    /// A three-line entry: one cash line and two counter lines. Every dollar is
    /// attributed and nothing is apportioned, because a balanced entry's non-cash
    /// lines already sum to its cash movement.
    #[test]
    fn a_split_entry_attributes_every_dollar_to_a_named_account() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        append_and_project(
            &mut store,
            entry(
                "split",
                day(2024, 1, 28),
                &[("expense", 7_000), ("ap", 3_000), ("cash", -10_000)],
            ),
            "user",
        );
        let reports = Reports::new(store.connection());
        let flow = reports
            .cash_flow(day(2024, 1, 1), day(2024, 1, 31), &["cash".to_string()])
            .expect("ties");
        let out = |id: &str| {
            flow.outflows
                .iter()
                .find(|l| l.account_id == id)
                .map(|l| -l.amount)
                .unwrap_or(0)
        };
        assert_eq!(out("expense"), 27_000, "20,000 earlier plus 7,000 here");
        assert_eq!(out("ap"), 3_000);
        assert_eq!(flow.total_out, 30_000);
        assert_eq!(flow.net_change(), 70_000);
    }

    /// The window is the window. An entry outside it is not in the attribution, and
    /// the opening balance is what the account held the day before it starts.
    #[test]
    fn the_period_bounds_the_attribution_and_sets_the_opening_balance() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        let reports = Reports::new(store.connection());
        let flow = reports
            .cash_flow(day(2024, 1, 16), day(2024, 1, 31), &["cash".to_string()])
            .expect("ties");
        assert_eq!(
            flow.opening_cash(),
            100_000,
            "the investment landed on the 1st, before this window"
        );
        assert_eq!(flow.total_in, 0);
        assert_eq!(flow.total_out, 20_000);
        assert_eq!(flow.closing_cash(), 80_000);
    }

    /// A void entry moved no money, and the balances it is checked against exclude
    /// it. Counting it would break the identity the report refuses to publish
    /// without.
    #[test]
    fn a_voided_entry_is_not_a_cash_flow() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        append_and_project(
            &mut store,
            entry(
                "mistake",
                day(2024, 1, 29),
                &[("expense", 99_000), ("cash", -99_000)],
            ),
            "user",
        );
        append_and_project(
            &mut store,
            Event::JournalEntryVoided {
                entry_id: "mistake".to_string(),
                reason: "not ours".to_string(),
            },
            "user",
        );
        let reports = Reports::new(store.connection());
        let flow = reports
            .cash_flow(day(2024, 1, 1), day(2024, 1, 31), &["cash".to_string()])
            .expect("ties");
        assert_eq!(flow.total_out, 20_000);
        assert_eq!(flow.closing_cash(), 80_000);
    }

    /// No cash accounts is not an error: it is a statement with nothing in it, which
    /// is what a book whose bank accounts nobody has named should show.
    #[test]
    fn a_statement_over_no_accounts_is_empty_and_ties() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        let reports = Reports::new(store.connection());
        let flow = reports
            .cash_flow(day(2024, 1, 1), day(2024, 1, 31), &[])
            .expect("ties");
        assert_eq!(flow.opening_cash(), 0);
        assert_eq!(flow.closing_cash(), 0);
        assert_eq!(flow.net_change(), 0);
        assert!(flow.inflows.is_empty() && flow.outflows.is_empty());
    }

    /// The guard that stops a statement whose halves disagree being published. It
    /// cannot fire while the balances and the attribution are read under the same
    /// filters, which is the point — but an unreachable check whose arithmetic was
    /// never run is a check that will be wrong on the day it is reached.
    #[test]
    fn a_statement_whose_halves_disagree_is_refused() {
        let mut flow = CashFlow {
            start_date: day(2024, 1, 1),
            end_date: day(2024, 1, 31),
            cash_accounts: vec![CashAccountMovement {
                account_id: "cash".to_string(),
                account_number: "1000".to_string(),
                account_name: "Cash".to_string(),
                opening: 0,
                closing: 80_000,
            }],
            inflows: Vec::new(),
            outflows: Vec::new(),
            total_in: 100_000,
            total_out: 20_000,
        };
        assert!(
            super::check_cash_flow_ties(&flow).is_ok(),
            "80,000 either way"
        );

        flow.total_out = 30_000;
        let refused = super::check_cash_flow_ties(&flow).expect_err("refused");
        let said = refused.to_string();
        assert!(said.contains("80000"), "{said}");
        assert!(said.contains("70000"), "{said}");
        assert!(
            said.contains("fault in the report"),
            "it says whose fault it is: {said}"
        );
    }

    /// The default set is what the provider called a chequing account, not what the
    /// chart calls one — and never a credit card, because paying one is the cash
    /// movement and the purchase was not.
    #[test]
    fn the_default_cash_set_is_the_banks_own_depository_accounts() {
        let mut store = setup();
        create_accounts_and_entries(&mut store);
        append_and_project(
            &mut store,
            Event::AccountCreated {
                account_id: "card".to_string(),
                account_type: EventAccountType::Liability,
                account_number: "2100".to_string(),
                name: "Visa".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            },
            "user",
        );
        let conn = store.connection();
        conn.execute(
            "INSERT INTO plaid_items (id, proxy_item_id, institution_name, status)
             VALUES ('item1', 'p1', 'Bank', 'active')",
            [],
        )
        .unwrap();
        for (pa, kind, local) in [
            ("pa1", "depository", "cash"),
            ("pa2", "credit", "card"),
            ("pa3", "investment", "ar"),
        ] {
            conn.execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', ?1, ?1, ?2, ?3)",
                rusqlite::params![pa, kind, local],
            )
            .unwrap();
        }
        let reports = Reports::new(conn);
        assert_eq!(
            reports.default_cash_accounts().expect("read"),
            vec!["cash".to_string()]
        );
    }
}
