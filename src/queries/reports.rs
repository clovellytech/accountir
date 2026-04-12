use std::collections::HashSet;

use crate::domain::AccountType;
use crate::queries::account_queries::AccountQueries;
use chrono::NaiveDate;
use rusqlite::Connection;
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

        // Calculate income to date for equity section
        let income = self.calculate_net_income(None, Some(as_of_date))?;
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
        let accounts = queries.get_active_accounts()?;

        for account in accounts {
            if !account.account_type.is_income_statement() {
                continue;
            }

            // Calculate balance change during the period
            let start_balance = queries
                .get_account_balance(
                    &account.id,
                    Some(start_date.pred_opt().unwrap_or(start_date)),
                )?
                .balance;
            let end_balance = queries
                .get_account_balance(&account.id, Some(end_date))?
                .balance;
            let period_change = end_balance - start_balance;

            if period_change == 0 {
                continue;
            }

            let line = IncomeStatementLine {
                account_id: account.id.clone(),
                account_number: account.account_number,
                account_name: account.name,
                parent_id: account.parent_id,
                balance: period_change.abs(),
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
             WHERE je.is_void = 0 AND a.account_type IN ('revenue', 'expense')",
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
    use crate::store::projections::Projector;

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
            let projector = Projector::new(store.connection());
            projector.apply(&stored).unwrap();
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
