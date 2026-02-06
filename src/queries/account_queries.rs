use crate::domain::{Account, AccountType};
use chrono::NaiveDate;
use rusqlite::{params, Connection};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AccountQueryError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Account not found: {0}")]
    NotFound(String),
}

/// Account balance information
#[derive(Debug, Clone)]
pub struct AccountBalance {
    pub account_id: String,
    pub account_number: String,
    pub account_name: String,
    pub account_type: AccountType,
    /// Balance in smallest currency unit (positive = debit balance, negative = credit balance)
    pub balance: i64,
    pub currency: String,
}

/// A ledger entry (single line from a journal entry)
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    pub entry_id: String,
    pub line_id: String,
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub debit: Option<i64>,
    pub credit: Option<i64>,
    pub running_balance: i64,
    pub is_void: bool,
    pub is_cleared: bool,
}

/// Queries for accounts and balances
pub struct AccountQueries<'a> {
    conn: &'a Connection,
}

impl<'a> AccountQueries<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Get an account by ID
    pub fn get_account(&self, account_id: &str) -> Result<Account, AccountQueryError> {
        let row = self.conn.query_row(
            "SELECT id, account_type, account_number, name, parent_id, currency, description, is_active
             FROM accounts WHERE id = ?1",
            [account_id],
            |row| {
                let type_str: String = row.get(1)?;
                let account_type = match type_str.as_str() {
                    "asset" => AccountType::Asset,
                    "liability" => AccountType::Liability,
                    "equity" => AccountType::Equity,
                    "revenue" => AccountType::Revenue,
                    "expense" => AccountType::Expense,
                    _ => AccountType::Asset,
                };

                Ok(Account {
                    id: row.get(0)?,
                    account_type,
                    account_number: row.get(2)?,
                    name: row.get(3)?,
                    parent_id: row.get(4)?,
                    currency: row.get(5)?,
                    description: row.get(6)?,
                    is_active: row.get::<_, i32>(7)? == 1,
                })
            },
        );

        row.map_err(|_| AccountQueryError::NotFound(account_id.to_string()))
    }

    /// Get all accounts
    pub fn get_all_accounts(&self) -> Result<Vec<Account>, AccountQueryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account_type, account_number, name, parent_id, currency, description, is_active
             FROM accounts ORDER BY account_number",
        )?;

        let accounts = stmt
            .query_map([], |row| {
                let type_str: String = row.get(1)?;
                let account_type = match type_str.as_str() {
                    "asset" => AccountType::Asset,
                    "liability" => AccountType::Liability,
                    "equity" => AccountType::Equity,
                    "revenue" => AccountType::Revenue,
                    "expense" => AccountType::Expense,
                    _ => AccountType::Asset,
                };

                Ok(Account {
                    id: row.get(0)?,
                    account_type,
                    account_number: row.get(2)?,
                    name: row.get(3)?,
                    parent_id: row.get(4)?,
                    currency: row.get(5)?,
                    description: row.get(6)?,
                    is_active: row.get::<_, i32>(7)? == 1,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(accounts)
    }

    /// Get active accounts only
    pub fn get_active_accounts(&self) -> Result<Vec<Account>, AccountQueryError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account_type, account_number, name, parent_id, currency, description, is_active
             FROM accounts WHERE is_active = 1 ORDER BY account_number",
        )?;

        let accounts = stmt
            .query_map([], |row| {
                let type_str: String = row.get(1)?;
                let account_type = match type_str.as_str() {
                    "asset" => AccountType::Asset,
                    "liability" => AccountType::Liability,
                    "equity" => AccountType::Equity,
                    "revenue" => AccountType::Revenue,
                    "expense" => AccountType::Expense,
                    _ => AccountType::Asset,
                };

                Ok(Account {
                    id: row.get(0)?,
                    account_type,
                    account_number: row.get(2)?,
                    name: row.get(3)?,
                    parent_id: row.get(4)?,
                    currency: row.get(5)?,
                    description: row.get(6)?,
                    is_active: row.get::<_, i32>(7)? == 1,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(accounts)
    }

    /// Get accounts by type
    pub fn get_accounts_by_type(
        &self,
        account_type: AccountType,
    ) -> Result<Vec<Account>, AccountQueryError> {
        let type_str = match account_type {
            AccountType::Asset => "asset",
            AccountType::Liability => "liability",
            AccountType::Equity => "equity",
            AccountType::Revenue => "revenue",
            AccountType::Expense => "expense",
        };

        let mut stmt = self.conn.prepare(
            "SELECT id, account_type, account_number, name, parent_id, currency, description, is_active
             FROM accounts WHERE account_type = ?1 AND is_active = 1 ORDER BY account_number",
        )?;

        let accounts = stmt
            .query_map([type_str], |row| {
                Ok(Account {
                    id: row.get(0)?,
                    account_type,
                    account_number: row.get(2)?,
                    name: row.get(3)?,
                    parent_id: row.get(4)?,
                    currency: row.get(5)?,
                    description: row.get(6)?,
                    is_active: row.get::<_, i32>(7)? == 1,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(accounts)
    }

    /// Calculate the balance of an account as of a date
    pub fn get_account_balance(
        &self,
        account_id: &str,
        as_of_date: Option<NaiveDate>,
    ) -> Result<AccountBalance, AccountQueryError> {
        let account = self.get_account(account_id)?;

        let balance: i64 = if let Some(date) = as_of_date {
            self.conn.query_row(
                "SELECT COALESCE(SUM(jl.amount), 0)
                 FROM journal_lines jl
                 JOIN journal_entries je ON jl.entry_id = je.id
                 WHERE jl.account_id = ?1 AND je.date <= ?2 AND je.is_void = 0",
                params![account_id, date.to_string()],
                |row| row.get(0),
            )?
        } else {
            self.conn.query_row(
                "SELECT COALESCE(SUM(jl.amount), 0)
                 FROM journal_lines jl
                 JOIN journal_entries je ON jl.entry_id = je.id
                 WHERE jl.account_id = ?1 AND je.is_void = 0",
                [account_id],
                |row| row.get(0),
            )?
        };

        Ok(AccountBalance {
            account_id: account.id,
            account_number: account.account_number,
            account_name: account.name,
            account_type: account.account_type,
            balance,
            currency: account.currency.unwrap_or_else(|| "USD".to_string()),
        })
    }

    /// Get all account balances
    pub fn get_all_balances(
        &self,
        as_of_date: Option<NaiveDate>,
    ) -> Result<Vec<AccountBalance>, AccountQueryError> {
        let accounts = self.get_active_accounts()?;
        let mut balances = Vec::new();

        for account in accounts {
            let balance = self.get_account_balance(&account.id, as_of_date)?;
            balances.push(balance);
        }

        Ok(balances)
    }

    /// Get the ledger for an account (all transactions affecting it)
    pub fn get_account_ledger(
        &self,
        account_id: &str,
        start_date: Option<NaiveDate>,
        end_date: Option<NaiveDate>,
    ) -> Result<Vec<LedgerEntry>, AccountQueryError> {
        let mut sql = String::from(
            "SELECT jl.id, jl.entry_id, je.date, je.memo, je.reference, jl.amount, je.is_void, jl.is_cleared
             FROM journal_lines jl
             JOIN journal_entries je ON jl.entry_id = je.id
             WHERE jl.account_id = ?1",
        );

        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(account_id.to_string())];

        if let Some(start) = start_date {
            sql.push_str(" AND je.date >= ?2");
            params_vec.push(Box::new(start.to_string()));
        }

        if let Some(end) = end_date {
            let param_num = params_vec.len() + 1;
            sql.push_str(&format!(" AND je.date <= ?{}", param_num));
            params_vec.push(Box::new(end.to_string()));
        }

        sql.push_str(" ORDER BY je.date, je.id");

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let mut running_balance: i64 = 0;

        // Get opening balance if start_date is specified
        if let Some(start) = start_date {
            let opening: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(jl.amount), 0)
                 FROM journal_lines jl
                 JOIN journal_entries je ON jl.entry_id = je.id
                 WHERE jl.account_id = ?1 AND je.date < ?2 AND je.is_void = 0",
                params![account_id, start.to_string()],
                |row| row.get(0),
            )?;
            running_balance = opening;
        }

        let entries: Vec<LedgerEntry> = stmt
            .query_map(params_refs.as_slice(), |row| {
                let line_id: String = row.get(0)?;
                let entry_id: String = row.get(1)?;
                let date_str: String = row.get(2)?;
                let memo: String = row.get(3)?;
                let reference: Option<String> = row.get(4)?;
                let amount: i64 = row.get(5)?;
                let is_void: i32 = row.get(6)?;
                let is_cleared: i32 = row.get(7)?;

                let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                    .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());

                let (debit, credit) = if amount > 0 {
                    (Some(amount), None)
                } else if amount < 0 {
                    (None, Some(-amount))
                } else {
                    (None, None)
                };

                Ok((
                    line_id,
                    entry_id,
                    date,
                    memo,
                    reference,
                    debit,
                    credit,
                    amount,
                    is_void == 1,
                    is_cleared == 1,
                ))
            })?
            .filter_map(|r| r.ok())
            .map(
                |(
                    line_id,
                    entry_id,
                    date,
                    memo,
                    reference,
                    debit,
                    credit,
                    amount,
                    is_void,
                    is_cleared,
                )| {
                    if !is_void {
                        running_balance += amount;
                    }
                    LedgerEntry {
                        entry_id,
                        line_id,
                        date,
                        memo,
                        reference,
                        debit,
                        credit,
                        running_balance,
                        is_void,
                        is_cleared,
                    }
                },
            )
            .collect();

        Ok(entries)
    }

    /// Get uncleared transactions for an account (for reconciliation)
    pub fn get_uncleared_transactions(
        &self,
        account_id: &str,
    ) -> Result<Vec<LedgerEntry>, AccountQueryError> {
        let mut stmt = self.conn.prepare(
            "SELECT jl.id, jl.entry_id, je.date, je.memo, je.reference, jl.amount, je.is_void
             FROM journal_lines jl
             JOIN journal_entries je ON jl.entry_id = je.id
             WHERE jl.account_id = ?1 AND jl.is_cleared = 0 AND je.is_void = 0
             ORDER BY je.date, je.id",
        )?;

        let entries: Vec<LedgerEntry> = stmt
            .query_map([account_id], |row| {
                let line_id: String = row.get(0)?;
                let entry_id: String = row.get(1)?;
                let date_str: String = row.get(2)?;
                let memo: String = row.get(3)?;
                let reference: Option<String> = row.get(4)?;
                let amount: i64 = row.get(5)?;

                let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                    .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());

                let (debit, credit) = if amount > 0 {
                    (Some(amount), None)
                } else if amount < 0 {
                    (None, Some(-amount))
                } else {
                    (None, None)
                };

                Ok(LedgerEntry {
                    entry_id,
                    line_id,
                    date,
                    memo,
                    reference,
                    debit,
                    credit,
                    running_balance: 0, // Not calculated for uncleared list
                    is_void: false,
                    is_cleared: false,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{Event, EventAccountType, EventEnvelope, JournalLineData};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::store::projections::Projector;

    fn setup() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    fn append_and_project(store: &mut EventStore, event: Event, user_id: &str) {
        let stored = store
            .append(EventEnvelope::new(event, user_id.to_string()))
            .unwrap();
        {
            let projector = Projector::new(store.connection());
            projector.apply(&stored).unwrap();
        }
    }

    fn create_test_accounts(store: &mut EventStore) {
        let cash = Event::AccountCreated {
            account_id: "cash".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };

        let expense = Event::AccountCreated {
            account_id: "expense".to_string(),
            account_type: EventAccountType::Expense,
            account_number: "5000".to_string(),
            name: "Supplies Expense".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };

        append_and_project(store, cash, "user");
        append_and_project(store, expense, "user");
    }

    #[test]
    fn test_get_account() {
        let mut store = setup();
        create_test_accounts(&mut store);

        let queries = AccountQueries::new(store.connection());
        let account = queries.get_account("cash").unwrap();

        assert_eq!(account.name, "Cash");
        assert_eq!(account.account_number, "1000");
        assert!(matches!(account.account_type, AccountType::Asset));
    }

    #[test]
    fn test_get_account_balance() {
        let mut store = setup();
        create_test_accounts(&mut store);

        // Post an entry
        let entry = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Bought supplies".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000, // $100 debit
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "cash".to_string(),
                    amount: -10000, // $100 credit
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };

        append_and_project(&mut store, entry, "user");

        let queries = AccountQueries::new(store.connection());

        // Cash should have credit balance of -10000
        let cash_balance = queries.get_account_balance("cash", None).unwrap();
        assert_eq!(cash_balance.balance, -10000);

        // Expense should have debit balance of 10000
        let expense_balance = queries.get_account_balance("expense", None).unwrap();
        assert_eq!(expense_balance.balance, 10000);
    }

    #[test]
    fn test_get_account_ledger() {
        let mut store = setup();
        create_test_accounts(&mut store);

        // Post two entries
        for i in 1..=2 {
            let entry = Event::JournalEntryPosted {
                entry_id: format!("entry-{:03}", i),
                date: NaiveDate::from_ymd_opt(2024, 1, i as u32 * 5).unwrap(),
                memo: format!("Entry {}", i),
                lines: vec![
                    JournalLineData {
                        line_id: format!("line-{:03}-1", i),
                        account_id: "expense".to_string(),
                        amount: 5000 * i as i64,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    },
                    JournalLineData {
                        line_id: format!("line-{:03}-2", i),
                        account_id: "cash".to_string(),
                        amount: -5000 * i as i64,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    },
                ],
                reference: None,
                source: None,
            };

            append_and_project(&mut store, entry, "user");
        }

        let queries = AccountQueries::new(store.connection());
        let ledger = queries.get_account_ledger("cash", None, None).unwrap();

        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0].credit, Some(5000));
        assert_eq!(ledger[0].running_balance, -5000);
        assert_eq!(ledger[1].credit, Some(10000));
        assert_eq!(ledger[1].running_balance, -15000);
    }
}
