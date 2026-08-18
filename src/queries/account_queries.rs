use crate::domain::{Account, AccountType};
use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AccountQueryError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Account not found: {0}")]
    NotFound(String),
}

/// A reconciliation that is open on an account.
#[derive(Debug, Clone)]
pub struct OpenReconciliation {
    pub id: String,
    pub statement_date: NaiveDate,
    pub statement_ending_balance: i64,
}

/// Where a reconciliation stands, in the terms the completion event records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationProgress {
    /// Lines cleared by earlier reconciliations on this account.
    pub beginning_balance: i64,
    /// Lines cleared by this one.
    pub cleared_total: i64,
    /// What the statement says the account should hold.
    pub statement_balance: i64,
    /// `statement_balance - (beginning_balance + cleared_total)`. Zero is done.
    pub difference: i64,
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

    /// The reconciliation currently open on an account, if any.
    ///
    /// How a client learns the id of a reconciliation it just started: the server
    /// mints it and returns only the new log head, so the id arrives with the next
    /// pull rather than in the response. There is at most one — enforced in the
    /// append transaction and by a partial unique index — so this is a lookup and
    /// not a choice.
    ///
    /// It is also how a screen finds out that a reconciliation is *already* open,
    /// which it must do before offering to start another.
    pub fn in_progress_reconciliation(
        &self,
        account_id: &str,
    ) -> Result<Option<OpenReconciliation>, AccountQueryError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, statement_date, statement_ending_balance
                   FROM reconciliations
                  WHERE account_id = ?1 AND status = 'in_progress'",
                [account_id],
                |row| {
                    Ok(OpenReconciliation {
                        id: row.get(0)?,
                        statement_date: NaiveDate::parse_from_str(
                            &row.get::<_, String>(1)?,
                            "%Y-%m-%d",
                        )
                        .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
                        statement_ending_balance: row.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// What a reconciliation's difference would be if it were completed now.
    ///
    /// **The same arithmetic `build_complete_reconciliation_in_txn` runs**, and
    /// deliberately the same SQL: the number a screen shows while somebody ticks
    /// lines has to be the number the server records when they finish, or the
    /// reconciliation "balances" on screen and completes with a residual.
    ///
    /// Advisory all the same — the server recomputes it under the write lock, so a
    /// colleague clearing a line in between moves it. That is a reason to keep the
    /// formula identical, not a reason to skip showing it.
    pub fn reconciliation_progress(
        &self,
        reconciliation_id: &str,
    ) -> Result<ReconciliationProgress, AccountQueryError> {
        let (account_id, statement_balance): (String, i64) = self.conn.query_row(
            "SELECT account_id, statement_ending_balance FROM reconciliations WHERE id = ?1",
            [reconciliation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let cleared_total: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(cleared_amount), 0) FROM cleared_transactions
                  WHERE reconciliation_id = ?1",
                [reconciliation_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);

        // Lines cleared by *earlier* reconciliations on this account. Excluding
        // this one's is what makes the two sums disjoint rather than overlapping.
        let beginning_balance: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(jl.amount), 0)
                   FROM journal_lines jl
                   JOIN journal_entries je ON jl.entry_id = je.id
                  WHERE jl.account_id = ?1 AND jl.is_cleared = 1
                    AND jl.id NOT IN (SELECT line_id FROM cleared_transactions WHERE reconciliation_id = ?2)
                    AND je.is_void = 0",
                params![&account_id, reconciliation_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);

        Ok(ReconciliationProgress {
            beginning_balance,
            cleared_total,
            statement_balance,
            difference: statement_balance - (beginning_balance + cleared_total),
        })
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
    use crate::store::projections::ProjectionStore;

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
            store.apply_projection(&stored).unwrap();
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
