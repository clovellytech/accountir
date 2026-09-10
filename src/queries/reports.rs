use std::collections::HashSet;

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
        let income = self.calculate_net_income(self.unclosed_from(as_of_date)?, Some(as_of_date))?;
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

    fn create_accounts_and_entries(store: &mut EventStore) {
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
    use crate::commands::closing_commands::{close_books, CloseBooksCommand};
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
                    equity_account_id: equity,
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

        assert!(bs.is_balanced, "assets {} vs L+E {}", bs.total_assets, bs.total_liabilities_and_equity);
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
                equity_account_id: equity_2022,
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
                .query_row("SELECT id FROM accounts WHERE account_number = ?1", [n], |r| {
                    r.get(0)
                })
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
        assert_eq!(is.expenses.total, 30_000, "the deactivated rent still happened");
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
        use crate::commands::closing_commands::{close_books, CloseBooksCommand, ClosingError};

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
                equity_account_id: equity,
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
