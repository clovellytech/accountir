use chrono::NaiveDate;
use rusqlite::{params, Connection};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SearchError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
}

/// Search result for an account
#[derive(Debug, Clone)]
pub struct AccountSearchResult {
    pub id: String,
    pub account_number: String,
    pub name: String,
    pub account_type: String,
    pub is_active: bool,
}

/// Search result for a journal entry
#[derive(Debug, Clone)]
pub struct EntrySearchResult {
    pub entry_id: String,
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub total_amount: i64,
    pub is_void: bool,
    /// Source of the entry (manual, import, reversal, etc.)
    pub source: Option<String>,
    /// Amount for the specific account when filtering by account (for ledger view)
    pub account_amount: Option<i64>,
    /// The other account(s) in the transaction (for ledger view)
    /// Shows the offsetting account name, or "Multiple" if there are multiple
    pub other_account: Option<String>,
    /// The ID of the other account (for jumping to that account's ledger)
    /// None if there are multiple other accounts
    pub other_account_id: Option<String>,
}

/// Search functionality for accounts and entries
pub struct Search<'a> {
    conn: &'a Connection,
}

impl<'a> Search<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Search accounts by name or number
    pub fn search_accounts(&self, query: &str) -> Result<Vec<AccountSearchResult>, SearchError> {
        let pattern = format!("%{}%", query);

        let mut stmt = self.conn.prepare(
            "SELECT id, account_number, name, account_type, is_active
             FROM accounts
             WHERE name LIKE ?1 OR account_number LIKE ?1
             ORDER BY account_number",
        )?;

        let results = stmt
            .query_map([&pattern], |row| {
                Ok(AccountSearchResult {
                    id: row.get(0)?,
                    account_number: row.get(1)?,
                    name: row.get(2)?,
                    account_type: row.get(3)?,
                    is_active: row.get::<_, i32>(4)? == 1,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Search entries by memo, reference, or amount
    pub fn search_entries(
        &self,
        query: Option<&str>,
        start_date: Option<NaiveDate>,
        end_date: Option<NaiveDate>,
        account_id: Option<&str>,
        include_void: bool,
    ) -> Result<Vec<EntrySearchResult>, SearchError> {
        // When filtering by account, include the account-specific amount and other account info
        let (select_account_amount, select_other_account, select_other_account_id) =
            if let Some(acc_id) = account_id {
                (
                    ", jl.amount".to_string(),
                    format!(
                        ", (SELECT CASE
                        WHEN COUNT(DISTINCT jl3.account_id) = 0 THEN
                            (SELECT a.name FROM accounts a WHERE a.id = '{0}')
                        WHEN COUNT(DISTINCT jl3.account_id) = 1 THEN
                            (SELECT a.name FROM accounts a
                             JOIN journal_lines jl4 ON a.id = jl4.account_id
                             WHERE jl4.entry_id = je.id AND jl4.account_id != '{0}'
                             LIMIT 1)
                        ELSE 'Multiple'
                        END
                        FROM journal_lines jl3
                        WHERE jl3.entry_id = je.id AND jl3.account_id != '{0}')",
                        acc_id
                    ),
                    // Get the other account ID (NULL if multiple accounts)
                    format!(
                        ", (SELECT CASE
                        WHEN COUNT(DISTINCT jl5.account_id) = 1 THEN
                            (SELECT jl6.account_id
                             FROM journal_lines jl6
                             WHERE jl6.entry_id = je.id AND jl6.account_id != '{0}'
                             LIMIT 1)
                        ELSE NULL
                        END
                        FROM journal_lines jl5
                        WHERE jl5.entry_id = je.id AND jl5.account_id != '{0}')",
                        acc_id
                    ),
                )
            } else {
                (
                    ", NULL".to_string(),
                    ", NULL".to_string(),
                    ", NULL".to_string(),
                )
            };

        let mut sql = format!(
            "SELECT DISTINCT je.id, je.date, je.memo, je.reference,
                    (SELECT SUM(ABS(jl2.amount)) / 2 FROM journal_lines jl2 WHERE jl2.entry_id = je.id),
                    je.is_void, je.source{}{}{}
             FROM journal_entries je",
            select_account_amount, select_other_account, select_other_account_id
        );

        let mut conditions = Vec::new();
        let mut param_values: Vec<String> = Vec::new();

        if account_id.is_some() {
            sql.push_str(" JOIN journal_lines jl ON je.id = jl.entry_id");
        }

        if let Some(q) = query {
            let idx = param_values.len() + 1;
            conditions.push(format!(
                "(je.memo LIKE ?{0} OR je.reference LIKE ?{0})",
                idx
            ));
            param_values.push(format!("%{}%", q));
        }

        if let Some(start) = start_date {
            let idx = param_values.len() + 1;
            conditions.push(format!("je.date >= ?{}", idx));
            param_values.push(start.to_string());
        }

        if let Some(end) = end_date {
            let idx = param_values.len() + 1;
            conditions.push(format!("je.date <= ?{}", idx));
            param_values.push(end.to_string());
        }

        if let Some(acc_id) = account_id {
            let idx = param_values.len() + 1;
            conditions.push(format!("jl.account_id = ?{}", idx));
            param_values.push(acc_id.to_string());
        }

        if !include_void {
            conditions.push("je.is_void = 0".to_string());
        }

        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        sql.push_str(" ORDER BY je.date DESC, je.id DESC");

        let mut stmt = self.conn.prepare(&sql)?;

        let params_refs: Vec<&dyn rusqlite::ToSql> = param_values
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();

        let results = stmt
            .query_map(params_refs.as_slice(), |row| {
                let date_str: String = row.get(1)?;
                let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                    .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());

                Ok(EntrySearchResult {
                    entry_id: row.get(0)?,
                    date,
                    memo: row.get(2)?,
                    reference: row.get(3)?,
                    total_amount: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    is_void: row.get::<_, i32>(5)? == 1,
                    source: row.get(6)?,
                    account_amount: row.get(7)?,
                    other_account: row.get(8)?,
                    other_account_id: row.get(9)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Find entries by reference number
    pub fn find_by_reference(
        &self,
        reference: &str,
    ) -> Result<Vec<EntrySearchResult>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT je.id, je.date, je.memo, je.reference,
                    (SELECT SUM(ABS(jl.amount)) / 2 FROM journal_lines jl WHERE jl.entry_id = je.id),
                    je.is_void, je.source
             FROM journal_entries je
             WHERE je.reference = ?1
             ORDER BY je.date DESC",
        )?;

        let results = stmt
            .query_map([reference], |row| {
                let date_str: String = row.get(1)?;
                let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                    .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());

                Ok(EntrySearchResult {
                    entry_id: row.get(0)?,
                    date,
                    memo: row.get(2)?,
                    reference: row.get(3)?,
                    total_amount: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    is_void: row.get::<_, i32>(5)? == 1,
                    source: row.get(6)?,
                    account_amount: None,
                    other_account: None,
                    other_account_id: None,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Find entries by amount (exact match)
    pub fn find_by_amount(&self, amount: i64) -> Result<Vec<EntrySearchResult>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT je.id, je.date, je.memo, je.reference,
                    ?1 as amount, je.is_void, je.source
             FROM journal_entries je
             JOIN journal_lines jl ON je.id = jl.entry_id
             WHERE ABS(jl.amount) = ?1 AND je.is_void = 0
             ORDER BY je.date DESC",
        )?;

        let results = stmt
            .query_map([amount], |row| {
                let date_str: String = row.get(1)?;
                let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                    .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());

                Ok(EntrySearchResult {
                    entry_id: row.get(0)?,
                    date,
                    memo: row.get(2)?,
                    reference: row.get(3)?,
                    total_amount: row.get(4)?,
                    is_void: row.get::<_, i32>(5)? == 1,
                    source: row.get(6)?,
                    account_amount: None,
                    other_account: None,
                    other_account_id: None,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Get recent entries
    pub fn recent_entries(&self, limit: u32) -> Result<Vec<EntrySearchResult>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT je.id, je.date, je.memo, je.reference,
                    (SELECT SUM(ABS(jl.amount)) / 2 FROM journal_lines jl WHERE jl.entry_id = je.id),
                    je.is_void, je.source
             FROM journal_entries je
             WHERE je.is_void = 0
             ORDER BY je.posted_at_event DESC
             LIMIT ?1",
        )?;

        let results = stmt
            .query_map([limit], |row| {
                let date_str: String = row.get(1)?;
                let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                    .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());

                Ok(EntrySearchResult {
                    entry_id: row.get(0)?,
                    date,
                    memo: row.get(2)?,
                    reference: row.get(3)?,
                    total_amount: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    is_void: row.get::<_, i32>(5)? == 1,
                    source: row.get(6)?,
                    account_amount: None,
                    other_account: None,
                    other_account_id: None,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
    }

    /// Get entries for a specific date range
    pub fn entries_in_range(
        &self,
        start_date: NaiveDate,
        end_date: NaiveDate,
    ) -> Result<Vec<EntrySearchResult>, SearchError> {
        let mut stmt = self.conn.prepare(
            "SELECT je.id, je.date, je.memo, je.reference,
                    (SELECT SUM(ABS(jl.amount)) / 2 FROM journal_lines jl WHERE jl.entry_id = je.id),
                    je.is_void, je.source
             FROM journal_entries je
             WHERE je.date >= ?1 AND je.date <= ?2 AND je.is_void = 0
             ORDER BY je.date, je.id",
        )?;

        let results = stmt
            .query_map(
                params![start_date.to_string(), end_date.to_string()],
                |row| {
                    let date_str: String = row.get(1)?;
                    let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                        .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());

                    Ok(EntrySearchResult {
                        entry_id: row.get(0)?,
                        date,
                        memo: row.get(2)?,
                        reference: row.get(3)?,
                        total_amount: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        is_void: row.get::<_, i32>(5)? == 1,
                        source: row.get(6)?,
                        account_amount: None,
                        other_account: None,
                        other_account_id: None,
                    })
                },
            )?
            .filter_map(|r| r.ok())
            .collect();

        Ok(results)
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

    fn create_test_data(store: &mut EventStore) {
        // Create accounts
        let cash = Event::AccountCreated {
            account_id: "cash".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        let expense = Event::AccountCreated {
            account_id: "expense".to_string(),
            account_type: EventAccountType::Expense,
            account_number: "5000".to_string(),
            name: "Office Supplies".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };

        append_and_project(store, cash, "user");
        append_and_project(store, expense, "user");

        // Create entries
        let entry1 = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Bought office supplies".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "l1-1".to_string(),
                    account_id: "expense".to_string(),
                    amount: 5000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "l1-2".to_string(),
                    account_id: "cash".to_string(),
                    amount: -5000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: Some("CHK-001".to_string()),
            source: None,
        };

        let entry2 = Event::JournalEntryPosted {
            entry_id: "entry-002".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 20).unwrap(),
            memo: "More supplies".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "l2-1".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "l2-2".to_string(),
                    account_id: "cash".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: Some("CHK-002".to_string()),
            source: None,
        };

        append_and_project(store, entry1, "user");
        append_and_project(store, entry2, "user");
    }

    #[test]
    fn test_search_accounts() {
        let mut store = setup();
        create_test_data(&mut store);

        let search = Search::new(store.connection());

        // Search by name
        let results = search.search_accounts("Cash").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Cash");

        // Search by number
        let results = search.search_accounts("5000").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "Office Supplies");

        // Search partial match
        let results = search.search_accounts("Off").unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_entries() {
        let mut store = setup();
        create_test_data(&mut store);

        let search = Search::new(store.connection());

        // Search by memo
        let results = search
            .search_entries(Some("supplies"), None, None, None, false)
            .unwrap();
        assert_eq!(results.len(), 2);

        // Search by date range
        let results = search
            .search_entries(
                None,
                Some(NaiveDate::from_ymd_opt(2024, 1, 18).unwrap()),
                Some(NaiveDate::from_ymd_opt(2024, 1, 25).unwrap()),
                None,
                false,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_id, "entry-002");
    }

    #[test]
    fn test_find_by_reference() {
        let mut store = setup();
        create_test_data(&mut store);

        let search = Search::new(store.connection());
        let results = search.find_by_reference("CHK-001").unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memo, "Bought office supplies");
    }

    #[test]
    fn test_find_by_amount() {
        let mut store = setup();
        create_test_data(&mut store);

        let search = Search::new(store.connection());
        let results = search.find_by_amount(10000).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry_id, "entry-002");
    }

    #[test]
    fn test_recent_entries() {
        let mut store = setup();
        create_test_data(&mut store);

        let search = Search::new(store.connection());
        let results = search.recent_entries(10).unwrap();

        assert_eq!(results.len(), 2);
    }
}
