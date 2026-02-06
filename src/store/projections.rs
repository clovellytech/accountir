use crate::events::types::{Event, EventAccountType, StoredEvent};
use chrono::Datelike;
use rusqlite::{params, Connection};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ProjectionError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Entity not found: {0}")]
    NotFound(String),
    #[error("Invalid state: {0}")]
    InvalidState(String),
}

/// Projects events into materialized tables
pub struct Projector<'a> {
    conn: &'a Connection,
}

impl<'a> Projector<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Apply a single event to update projections
    pub fn apply(&self, stored_event: &StoredEvent) -> Result<(), ProjectionError> {
        match &stored_event.event {
            Event::CompanyCreated {
                company_id,
                name,
                base_currency,
                fiscal_year_start,
            } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO company (id, company_id, name, base_currency, fiscal_year_start_month, created_at_event)
                     VALUES ('default', ?1, ?2, ?3, ?4, ?5)",
                    params![company_id, name, base_currency, fiscal_year_start, stored_event.id],
                )?;
            }
            Event::CompanySettingsUpdated {
                field,
                old_value: _,
                new_value,
            } => {
                // Update the specific field
                let sql = format!("UPDATE company SET {} = ?1 WHERE id = 'default'", field);
                self.conn.execute(&sql, [new_value])?;
            }
            Event::UserAdded {
                user_id,
                username,
                role,
            } => {
                let role_str = match role {
                    crate::events::types::UserRole::Admin => "admin",
                    crate::events::types::UserRole::Accountant => "accountant",
                    crate::events::types::UserRole::Viewer => "viewer",
                };
                self.conn.execute(
                    "INSERT INTO users (id, username, role, is_active, created_at_event)
                     VALUES (?1, ?2, ?3, 1, ?4)",
                    params![user_id, username, role_str, stored_event.id],
                )?;
            }
            Event::UserModified {
                user_id,
                field,
                old_value: _,
                new_value,
            } => {
                let sql = format!("UPDATE users SET {} = ?1 WHERE id = ?2", field);
                self.conn.execute(&sql, params![new_value, user_id])?;
            }
            Event::UserRemoved { user_id } => {
                self.conn
                    .execute("UPDATE users SET is_active = 0 WHERE id = ?1", [user_id])?;
            }
            Event::AccountCreated {
                account_id,
                account_type,
                account_number,
                name,
                parent_id,
                currency,
                description,
            } => {
                let type_str = match account_type {
                    EventAccountType::Asset => "asset",
                    EventAccountType::Liability => "liability",
                    EventAccountType::Equity => "equity",
                    EventAccountType::Revenue => "revenue",
                    EventAccountType::Expense => "expense",
                };
                self.conn.execute(
                    "INSERT INTO accounts (id, account_type, account_number, name, parent_id, currency, description, is_active, created_at_event, updated_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?8)",
                    params![
                        account_id,
                        type_str,
                        account_number,
                        name,
                        parent_id,
                        currency,
                        description,
                        stored_event.id,
                    ],
                )?;
            }
            Event::AccountUpdated {
                account_id,
                field,
                old_value: _,
                new_value,
            } => {
                let sql = format!(
                    "UPDATE accounts SET {} = ?1, updated_at_event = ?2 WHERE id = ?3",
                    field
                );
                self.conn
                    .execute(&sql, params![new_value, stored_event.id, account_id])?;
            }
            Event::AccountDeactivated {
                account_id,
                reason: _,
            } => {
                self.conn.execute(
                    "UPDATE accounts SET is_active = 0, updated_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, account_id],
                )?;
            }
            Event::AccountReactivated { account_id } => {
                self.conn.execute(
                    "UPDATE accounts SET is_active = 1, updated_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, account_id],
                )?;
            }
            Event::JournalEntryPosted {
                entry_id,
                date,
                memo,
                lines,
                reference,
                source,
            } => {
                let source_str = source.as_ref().map(|s| match s {
                    crate::events::types::JournalEntrySource::Manual => "manual",
                    crate::events::types::JournalEntrySource::Import => "import",
                    crate::events::types::JournalEntrySource::Recurring => "recurring",
                    crate::events::types::JournalEntrySource::System => "system",
                    crate::events::types::JournalEntrySource::Plaid => "plaid",
                });

                self.conn.execute(
                    "INSERT INTO journal_entries (id, date, memo, reference, source, is_void, posted_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
                    params![
                        entry_id,
                        date.to_string(),
                        memo,
                        reference,
                        source_str,
                        stored_event.id,
                    ],
                )?;

                for line in lines {
                    self.conn.execute(
                        "INSERT INTO journal_lines (id, entry_id, account_id, amount, currency, exchange_rate, memo, is_cleared)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
                        params![
                            line.line_id,
                            entry_id,
                            line.account_id,
                            line.amount,
                            line.currency,
                            line.exchange_rate.map(|r| r.to_string()),
                            line.memo,
                        ],
                    )?;
                }
            }
            Event::JournalEntryVoided {
                entry_id,
                reason: _,
            } => {
                self.conn.execute(
                    "UPDATE journal_entries SET is_void = 1 WHERE id = ?1",
                    params![entry_id],
                )?;
            }
            Event::JournalEntryUnvoided {
                entry_id,
                reason: _,
            } => {
                self.conn.execute(
                    "UPDATE journal_entries SET is_void = 0 WHERE id = ?1",
                    params![entry_id],
                )?;
            }
            Event::JournalEntryAnnotated {
                entry_id: _,
                annotation: _,
            } => {
                // Annotations could be stored in a separate table
                // For now, we'll skip this
            }
            Event::JournalLineReassigned {
                entry_id: _,
                line_id,
                old_account_id: _,
                new_account_id,
            } => {
                self.conn.execute(
                    "UPDATE journal_lines SET account_id = ?1 WHERE id = ?2",
                    params![new_account_id, line_id],
                )?;
            }
            Event::FiscalYearOpened {
                year,
                start_date,
                end_date,
            } => {
                self.conn.execute(
                    "INSERT INTO fiscal_years (year, start_date, end_date, is_closed)
                     VALUES (?1, ?2, ?3, 0)",
                    params![year, start_date.to_string(), end_date.to_string()],
                )?;

                // Create monthly periods
                let mut current = *start_date;
                let mut period = 1u8;
                while current <= *end_date && period <= 12 {
                    let period_end = {
                        let next_month = if current.month() == 12 {
                            chrono::NaiveDate::from_ymd_opt(current.year() + 1, 1, 1).unwrap()
                        } else {
                            chrono::NaiveDate::from_ymd_opt(current.year(), current.month() + 1, 1)
                                .unwrap()
                        };
                        next_month.pred_opt().unwrap().min(*end_date)
                    };

                    self.conn.execute(
                        "INSERT INTO fiscal_periods (year, period, start_date, end_date, status)
                         VALUES (?1, ?2, ?3, ?4, 'open')",
                        params![year, period, current.to_string(), period_end.to_string()],
                    )?;

                    current = period_end.succ_opt().unwrap_or(period_end);
                    period += 1;
                }
            }
            Event::PeriodClosed {
                year,
                period,
                closed_by_user_id,
            } => {
                self.conn.execute(
                    "UPDATE fiscal_periods SET status = 'closed', closed_by_user_id = ?1, closed_at = datetime('now')
                     WHERE year = ?2 AND period = ?3",
                    params![closed_by_user_id, year, period],
                )?;
            }
            Event::PeriodReopened {
                year,
                period,
                reason: _,
                reopened_by_user_id: _,
            } => {
                self.conn.execute(
                    "UPDATE fiscal_periods SET status = 'open', closed_by_user_id = NULL, closed_at = NULL
                     WHERE year = ?1 AND period = ?2",
                    params![year, period],
                )?;
            }
            Event::YearEndClosed {
                year,
                retained_earnings_entry_id,
            } => {
                self.conn.execute(
                    "UPDATE fiscal_years SET is_closed = 1, retained_earnings_entry_id = ?1 WHERE year = ?2",
                    params![retained_earnings_entry_id, year],
                )?;
            }
            Event::CurrencyEnabled {
                code,
                name,
                symbol,
                decimal_places,
            } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO currencies (code, name, symbol, decimal_places)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![code, name, symbol, decimal_places],
                )?;
            }
            Event::ExchangeRateRecorded {
                from_currency,
                to_currency,
                rate,
                effective_date,
            } => {
                self.conn.execute(
                    "INSERT INTO exchange_rates (from_currency, to_currency, rate, effective_date, recorded_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        from_currency,
                        to_currency,
                        rate.to_string().parse::<f64>().unwrap_or(0.0),
                        effective_date.to_string(),
                        stored_event.id,
                    ],
                )?;
            }
            Event::PlaidItemConnected {
                item_id,
                proxy_item_id,
                institution_name,
                plaid_accounts,
            } => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO plaid_items (id, proxy_item_id, institution_name, status, connected_at_event)
                     VALUES (?1, ?2, ?3, 'active', ?4)",
                    params![item_id, proxy_item_id, institution_name, stored_event.id],
                )?;

                for acct in plaid_accounts {
                    self.conn.execute(
                        "INSERT OR REPLACE INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type, mask)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![item_id, acct.plaid_account_id, acct.name, acct.account_type, acct.mask],
                    )?;
                }
            }
            Event::PlaidItemDisconnected { item_id, reason: _ } => {
                self.conn.execute(
                    "UPDATE plaid_items SET status = 'disconnected' WHERE id = ?1",
                    params![item_id],
                )?;
            }
            Event::PlaidAccountMapped {
                item_id,
                plaid_account_id,
                local_account_id,
            } => {
                self.conn.execute(
                    "UPDATE plaid_local_accounts SET local_account_id = ?1 WHERE item_id = ?2 AND plaid_account_id = ?3",
                    params![local_account_id, item_id, plaid_account_id],
                )?;
            }
            Event::PlaidAccountUnmapped {
                item_id,
                plaid_account_id,
                local_account_id: _,
            } => {
                self.conn.execute(
                    "UPDATE plaid_local_accounts SET local_account_id = NULL WHERE item_id = ?1 AND plaid_account_id = ?2",
                    params![item_id, plaid_account_id],
                )?;
            }
            Event::PlaidTransactionsSynced {
                item_id,
                transactions_added: _,
                transactions_modified: _,
                transactions_removed: _,
                sync_timestamp,
            } => {
                self.conn.execute(
                    "UPDATE plaid_items SET last_synced_at = ?1 WHERE id = ?2",
                    params![sync_timestamp, item_id],
                )?;
            }
            Event::ReconciliationStarted {
                reconciliation_id,
                account_id,
                statement_date,
                statement_ending_balance,
            } => {
                self.conn.execute(
                    "INSERT INTO reconciliations (id, account_id, statement_date, statement_ending_balance, status, started_at_event)
                     VALUES (?1, ?2, ?3, ?4, 'in_progress', ?5)",
                    params![
                        reconciliation_id,
                        account_id,
                        statement_date.to_string(),
                        statement_ending_balance,
                        stored_event.id,
                    ],
                )?;
            }
            Event::TransactionCleared {
                reconciliation_id,
                entry_id,
                line_id,
                cleared_amount,
            } => {
                self.conn.execute(
                    "INSERT INTO cleared_transactions (reconciliation_id, entry_id, line_id, cleared_amount, cleared_at_event)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        reconciliation_id,
                        entry_id,
                        line_id,
                        cleared_amount,
                        stored_event.id,
                    ],
                )?;
                self.conn.execute(
                    "UPDATE journal_lines SET is_cleared = 1, cleared_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, line_id],
                )?;
            }
            Event::TransactionUncleared {
                reconciliation_id,
                entry_id,
                line_id,
            } => {
                self.conn.execute(
                    "DELETE FROM cleared_transactions WHERE reconciliation_id = ?1 AND entry_id = ?2 AND line_id = ?3",
                    params![reconciliation_id, entry_id, line_id],
                )?;
                self.conn.execute(
                    "UPDATE journal_lines SET is_cleared = 0, cleared_at_event = NULL WHERE id = ?1",
                    [line_id],
                )?;
            }
            Event::ReconciliationCompleted {
                reconciliation_id,
                difference: _,
            } => {
                self.conn.execute(
                    "UPDATE reconciliations SET status = 'completed', completed_at_event = ?1 WHERE id = ?2",
                    params![stored_event.id, reconciliation_id],
                )?;
            }
            Event::ReconciliationAbandoned { reconciliation_id } => {
                self.conn.execute(
                    "UPDATE reconciliations SET status = 'abandoned' WHERE id = ?1",
                    [reconciliation_id],
                )?;
                // Remove cleared transactions for this reconciliation
                self.conn.execute(
                    "DELETE FROM cleared_transactions WHERE reconciliation_id = ?1",
                    [reconciliation_id],
                )?;
            }
        }

        Ok(())
    }

    /// Replay all events to rebuild projections
    pub fn rebuild(&self, events: &[StoredEvent]) -> Result<(), ProjectionError> {
        // Clear all projections
        self.conn.execute_batch(
            "DELETE FROM plaid_imported_transactions;
             DELETE FROM plaid_local_accounts;
             DELETE FROM plaid_items;
             DELETE FROM cleared_transactions;
             DELETE FROM reconciliations;
             DELETE FROM exchange_rates;
             DELETE FROM currencies;
             DELETE FROM fiscal_periods;
             DELETE FROM fiscal_years;
             DELETE FROM journal_lines;
             DELETE FROM journal_entries;
             DELETE FROM accounts;
             DELETE FROM users;
             DELETE FROM company;",
        )?;

        // Replay all events
        for event in events {
            self.apply(event)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventEnvelope, JournalLineData};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use chrono::NaiveDate;

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

    #[test]
    fn test_project_account_created() {
        let mut store = setup();

        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: Some("Main cash account".to_string()),
        };

        append_and_project(&mut store, event, "user-001");

        // Verify projection
        let (name, is_active): (String, i32) = store
            .connection()
            .query_row(
                "SELECT name, is_active FROM accounts WHERE id = ?1",
                ["acc-001"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();

        assert_eq!(name, "Cash");
        assert_eq!(is_active, 1);
    }

    #[test]
    fn test_project_journal_entry() {
        let mut store = setup();

        // First create accounts
        let acc1 = Event::AccountCreated {
            account_id: "expense".to_string(),
            account_type: EventAccountType::Expense,
            account_number: "5000".to_string(),
            name: "Supplies".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        let acc2 = Event::AccountCreated {
            account_id: "cash".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };

        append_and_project(&mut store, acc1, "user-001");
        append_and_project(&mut store, acc2, "user-001");

        // Now create journal entry
        let entry = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Bought supplies".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "cash".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: Some("CHK-001".to_string()),
            source: None,
        };

        append_and_project(&mut store, entry, "user-001");

        // Verify entry
        let memo: String = store
            .connection()
            .query_row(
                "SELECT memo FROM journal_entries WHERE id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(memo, "Bought supplies");

        // Verify lines
        let line_count: i32 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM journal_lines WHERE entry_id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(line_count, 2);

        // Verify balance
        let sum: i64 = store
            .connection()
            .query_row(
                "SELECT SUM(amount) FROM journal_lines WHERE entry_id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sum, 0); // Balanced
    }

    #[test]
    fn test_project_void_entry() {
        let mut store = setup();

        // Create accounts and entry
        let acc1 = Event::AccountCreated {
            account_id: "expense".to_string(),
            account_type: EventAccountType::Expense,
            account_number: "5000".to_string(),
            name: "Supplies".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        let acc2 = Event::AccountCreated {
            account_id: "cash".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };

        append_and_project(&mut store, acc1, "user-001");
        append_and_project(&mut store, acc2, "user-001");

        let entry = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Original entry".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "cash".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };

        append_and_project(&mut store, entry, "user-001");

        // Void the entry
        let void_event = Event::JournalEntryVoided {
            entry_id: "entry-001".to_string(),
            reason: "Error".to_string(),
        };

        append_and_project(&mut store, void_event, "user-001");

        // Verify void status
        let is_void: i32 = store
            .connection()
            .query_row(
                "SELECT is_void FROM journal_entries WHERE id = ?1",
                ["entry-001"],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(is_void, 1);
    }

    #[test]
    fn test_rebuild_projections() {
        let mut store = setup();

        // Create some events
        let events_data = vec![
            Event::AccountCreated {
                account_id: "acc-001".to_string(),
                account_type: EventAccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            },
            Event::AccountCreated {
                account_id: "acc-002".to_string(),
                account_type: EventAccountType::Expense,
                account_number: "5000".to_string(),
                name: "Supplies".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            },
        ];

        let mut stored_events = Vec::new();
        for event in events_data {
            let stored = append_and_project(&mut store, event, "user-001");
            stored_events.push(stored);
        }

        // Verify initial state
        let count: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Rebuild from scratch
        {
            let projector = Projector::new(store.connection());
            projector.rebuild(&stored_events).unwrap();
        }

        // Verify same state after rebuild
        let count_after: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 2);
    }
}
