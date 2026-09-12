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
        limit: Option<usize>,
    ) -> Result<Vec<EntrySearchResult>, SearchError> {
        let one: Vec<String> = account_id.into_iter().map(str::to_string).collect();
        self.search_entries_in_accounts(query, start_date, end_date, &one, include_void, limit)
    }

    /// The same search, filtered to a *set* of accounts rather than one.
    ///
    /// # Why a set, and why it is not just a wider `IN`
    ///
    /// A parent account's ledger is only useful with its children in it —
    /// `Leasehold Improvements` means little when the architecture sits in a
    /// child account. But an entry can touch two accounts in the set at once
    /// (the parent *and* a child), and then the single-account shape is wrong in
    /// two ways at the same time: `SELECT DISTINCT` no longer collapses it,
    /// because the two rows differ in `jl.amount`, so the entry is listed twice,
    /// each time with half of its effect on the set. A running balance built
    /// from that double-counts and stops tying out.
    ///
    /// So this groups by entry and sums the lines that fall inside the set,
    /// which is the entry's actual effect on it. The "other account" is then the
    /// counterpart *outside* the set — an entry moving money from a parent to
    /// its own child has no outside account at all, and says so.
    ///
    /// An empty `accounts` means no account filter, exactly as `None` did.
    pub fn search_entries_in_accounts(
        &self,
        query: Option<&str>,
        start_date: Option<NaiveDate>,
        end_date: Option<NaiveDate>,
        accounts: &[String],
        include_void: bool,
        limit: Option<usize>,
    ) -> Result<Vec<EntrySearchResult>, SearchError> {
        let filtered = !accounts.is_empty();
        // Placeholders rather than interpolation: these are ids from the caller,
        // and the number of them is the only thing the SQL text should learn.
        // Numbered rather than positional: the list appears four times in the
        // statement and SQLite binds a numbered parameter once however often it
        // is referenced. Seeded into `param_values` first, so the index
        // arithmetic the rest of this builder does still lands after them.
        let inlist = (1..=accounts.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        // The entry's effect on the set, and what sits on the other side of it.
        let (select_account_amount, select_other_account, select_other_account_id) = if filtered {
            (
                " , SUM(jl.amount)".to_string(),
                format!(
                    ", (SELECT CASE
                        WHEN COUNT(DISTINCT jl3.account_id) = 0 THEN NULL
                        WHEN COUNT(DISTINCT jl3.account_id) = 1 THEN
                            (SELECT a.name FROM accounts a
                             JOIN journal_lines jl4 ON a.id = jl4.account_id
                             WHERE jl4.entry_id = je.id AND jl4.account_id NOT IN ({0})
                             LIMIT 1)
                        ELSE 'Multiple'
                        END
                        FROM journal_lines jl3
                        WHERE jl3.entry_id = je.id AND jl3.account_id NOT IN ({0}))",
                    inlist
                ),
                format!(
                    ", (SELECT CASE
                        WHEN COUNT(DISTINCT jl5.account_id) = 1 THEN
                            (SELECT jl6.account_id
                             FROM journal_lines jl6
                             WHERE jl6.entry_id = je.id AND jl6.account_id NOT IN ({0})
                             LIMIT 1)
                        ELSE NULL
                        END
                        FROM journal_lines jl5
                        WHERE jl5.entry_id = je.id AND jl5.account_id NOT IN ({0}))",
                    inlist
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
            "SELECT je.id, je.date, je.memo, je.reference,
                    (SELECT SUM(ABS(jl2.amount)) / 2 FROM journal_lines jl2 WHERE jl2.entry_id = je.id),
                    je.is_void, je.source{}{}{}
             FROM journal_entries je",
            select_account_amount, select_other_account, select_other_account_id
        );

        let mut conditions = Vec::new();
        let mut param_values: Vec<String> = accounts.to_vec();

        if filtered {
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

        if filtered {
            // Reuses the placeholders already bound above — no new parameters.
            conditions.push(format!("jl.account_id IN ({inlist})"));
        }

        if !include_void {
            conditions.push("je.is_void = 0".to_string());
        }

        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        // One row per entry. Without this an entry touching two accounts in the
        // set is listed once per line, each showing only part of its effect —
        // see this method's own doc.
        if filtered {
            sql.push_str(" GROUP BY je.id");
        }

        sql.push_str(" ORDER BY je.date DESC, je.id DESC");
        // Bound the result set in SQL when a caller asks (avoids materializing the
        // whole table then truncating). `limit` is a usize, so no injection.
        if let Some(n) = limit {
            sql.push_str(&format!(" LIMIT {n}"));
        }

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
            .search_entries(Some("supplies"), None, None, None, false, None)
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
                None,
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

/// Searching a *set* of accounts — a parent's ledger with its children in it.
#[cfg(test)]
mod subtree_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::AccountType;
    use crate::events::types::JournalEntrySource;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    /// Cash, a parent asset and a child of it.
    fn books() -> (EventStore, String, String, String) {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let mut store = store;
        for (ty, number, name) in [
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Asset, "1004", "Leasehold"),
            (AccountType::Expense, "6000", "Rent"),
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
        let id = |s: &EventStore, n: &str| -> String {
            s.connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [n],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let parent = id(&store, "1004");
        AccountCommands::new(&mut store, "u".to_string())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1015".to_string(),
                name: "Architecture".to_string(),
                parent_id: Some(parent.clone()),
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
        let (cash, child) = (id(&store, "1000"), id(&store, "1015"));
        (store, parent, child, cash)
    }

    fn post(store: &mut EventStore, date: NaiveDate, memo: &str, lines: Vec<EntryLine>) {
        EntryCommands::new(store, "u".to_string())
            .post_entry(PostEntryCommand {
                date,
                memo: memo.to_string(),
                lines,
                reference: None,
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();
    }

    /// The case the set query exists for: one entry touching the parent *and*
    /// a child. Listed once, carrying its whole effect on the set — not twice,
    /// carrying half each.
    #[test]
    fn an_entry_touching_two_accounts_in_the_set_is_listed_once() {
        let (mut store, parent, child, cash) = books();
        post(
            &mut store,
            day(2023, 5, 1),
            "Buildout invoice",
            vec![
                EntryLine::debit(&parent, 100_00, "USD"),
                EntryLine::debit(&child, 25_00, "USD"),
                EntryLine::credit(&cash, 125_00, "USD"),
            ],
        );

        let set = vec![parent.clone(), child.clone()];
        let rows = Search::new(store.connection())
            .search_entries_in_accounts(None, None, None, &set, false, None)
            .unwrap();
        assert_eq!(rows.len(), 1, "one entry, one row");
        assert_eq!(
            rows[0].account_amount,
            Some(125_00),
            "the entry's whole effect on the set, not one of its lines"
        );

        // The parent alone still sees only its own line.
        let just_parent = vec![parent.clone()];
        let rows = Search::new(store.connection())
            .search_entries_in_accounts(None, None, None, &just_parent, false, None)
            .unwrap();
        assert_eq!(rows[0].account_amount, Some(100_00));
    }

    /// Money moved from a parent to its own child nets to nothing across the
    /// set, and has no counterpart outside it to name.
    #[test]
    fn a_transfer_within_the_set_nets_to_zero_and_names_no_other_account() {
        let (mut store, parent, child, _cash) = books();
        post(
            &mut store,
            day(2023, 6, 1),
            "Reclassify to Architecture",
            vec![
                EntryLine::debit(&child, 40_00, "USD"),
                EntryLine::credit(&parent, 40_00, "USD"),
            ],
        );

        let set = vec![parent, child];
        let rows = Search::new(store.connection())
            .search_entries_in_accounts(None, None, None, &set, false, None)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].account_amount, Some(0), "it left the set unchanged");
        assert_eq!(rows[0].other_account, None, "there is no outside account");
    }

    /// The subtree's running total is the sum of its rows — the property a
    /// ledger's balance column depends on.
    #[test]
    fn the_rows_sum_to_the_subtree_balance() {
        let (mut store, parent, child, cash) = books();
        post(
            &mut store,
            day(2023, 1, 5),
            "a",
            vec![
                EntryLine::debit(&parent, 10_00, "USD"),
                EntryLine::credit(&cash, 10_00, "USD"),
            ],
        );
        post(
            &mut store,
            day(2023, 2, 5),
            "b",
            vec![
                EntryLine::debit(&child, 7_00, "USD"),
                EntryLine::credit(&cash, 7_00, "USD"),
            ],
        );
        post(
            &mut store,
            day(2023, 3, 5),
            "c",
            vec![
                EntryLine::debit(&parent, 3_00, "USD"),
                EntryLine::debit(&child, 1_00, "USD"),
                EntryLine::credit(&cash, 4_00, "USD"),
            ],
        );

        let set = vec![parent.clone(), child.clone()];
        let rows = Search::new(store.connection())
            .search_entries_in_accounts(None, None, None, &set, false, None)
            .unwrap();
        let from_rows: i64 = rows.iter().filter_map(|r| r.account_amount).sum();

        let q = crate::queries::account_queries::AccountQueries::new(store.connection());
        let from_ledger: i64 = [&parent, &child]
            .iter()
            .map(|a| q.get_account_balance(a, None).unwrap().balance)
            .sum();
        assert_eq!(from_rows, 21_00);
        assert_eq!(
            from_rows, from_ledger,
            "the column has to tie to the accounts"
        );
    }

    /// An empty set means no account filter, which is what `None` always meant.
    #[test]
    fn an_empty_set_filters_nothing() {
        let (mut store, parent, _child, cash) = books();
        post(
            &mut store,
            day(2023, 1, 5),
            "a",
            vec![
                EntryLine::debit(&parent, 10_00, "USD"),
                EntryLine::credit(&cash, 10_00, "USD"),
            ],
        );
        let rows = Search::new(store.connection())
            .search_entries_in_accounts(None, None, None, &[], false, None)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].account_amount, None, "no account to be relative to");
    }
}
