use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::store::event_store::{EventStore, EventStoreError};
use crate::store::projections::Projector;
use chrono::NaiveDate;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum ReconciliationCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Projection error: {0}")]
    ProjectionError(#[from] crate::store::projections::ProjectionError),
    #[error("Reconciliation not found: {0}")]
    NotFound(String),
    #[error("Account not found: {0}")]
    AccountNotFound(String),
    #[error("Reconciliation already completed")]
    AlreadyCompleted,
    #[error("Reconciliation was abandoned")]
    Abandoned,
    #[error("Transaction already cleared")]
    AlreadyCleared,
    #[error("Transaction not cleared")]
    NotCleared,
    #[error("Entry not found: {0}")]
    EntryNotFound(String),
    #[error("Line not found: {0}")]
    LineNotFound(String),
}

/// Command to start a reconciliation
#[derive(Debug, Clone)]
pub struct StartReconciliationCommand {
    pub account_id: String,
    pub statement_date: NaiveDate,
    pub statement_ending_balance: i64,
}

/// Command to clear a transaction
#[derive(Debug, Clone)]
pub struct ClearTransactionCommand {
    pub reconciliation_id: String,
    pub entry_id: String,
    pub line_id: String,
}

/// Command to unclear a transaction
#[derive(Debug, Clone)]
pub struct UnclearTransactionCommand {
    pub reconciliation_id: String,
    pub entry_id: String,
    pub line_id: String,
}

/// Command to complete a reconciliation
#[derive(Debug, Clone)]
pub struct CompleteReconciliationCommand {
    pub reconciliation_id: String,
}

/// Command to abandon a reconciliation
#[derive(Debug, Clone)]
pub struct AbandonReconciliationCommand {
    pub reconciliation_id: String,
}

/// Reconciliation command handler
pub struct ReconciliationCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> ReconciliationCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Start a new reconciliation
    pub fn start_reconciliation(
        &mut self,
        cmd: StartReconciliationCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        // Verify account exists
        let exists: bool = self
            .store
            .connection()
            .query_row(
                "SELECT 1 FROM accounts WHERE id = ?1",
                [&cmd.account_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !exists {
            return Err(ReconciliationCommandError::AccountNotFound(cmd.account_id));
        }

        let reconciliation_id = Uuid::new_v4().to_string();

        let event = Event::ReconciliationStarted {
            reconciliation_id,
            account_id: cmd.account_id,
            statement_date: cmd.statement_date,
            statement_ending_balance: cmd.statement_ending_balance,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Clear a transaction in a reconciliation
    pub fn clear_transaction(
        &mut self,
        cmd: ClearTransactionCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        // Verify reconciliation exists and is in progress
        let status: String = self
            .store
            .connection()
            .query_row(
                "SELECT status FROM reconciliations WHERE id = ?1",
                [&cmd.reconciliation_id],
                |row| row.get(0),
            )
            .map_err(|_| ReconciliationCommandError::NotFound(cmd.reconciliation_id.clone()))?;

        if status == "completed" {
            return Err(ReconciliationCommandError::AlreadyCompleted);
        }
        if status == "abandoned" {
            return Err(ReconciliationCommandError::Abandoned);
        }

        // Verify entry and line exist
        let amount: i64 = self
            .store
            .connection()
            .query_row(
                "SELECT amount FROM journal_lines WHERE id = ?1 AND entry_id = ?2",
                rusqlite::params![&cmd.line_id, &cmd.entry_id],
                |row| row.get(0),
            )
            .map_err(|_| ReconciliationCommandError::LineNotFound(cmd.line_id.clone()))?;

        // Check if already cleared in this reconciliation
        let already_cleared: bool = self
            .store
            .connection()
            .query_row(
                "SELECT 1 FROM cleared_transactions
                 WHERE reconciliation_id = ?1 AND entry_id = ?2 AND line_id = ?3",
                rusqlite::params![&cmd.reconciliation_id, &cmd.entry_id, &cmd.line_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if already_cleared {
            return Err(ReconciliationCommandError::AlreadyCleared);
        }

        let event = Event::TransactionCleared {
            reconciliation_id: cmd.reconciliation_id,
            entry_id: cmd.entry_id,
            line_id: cmd.line_id,
            cleared_amount: amount,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Unclear a transaction in a reconciliation
    pub fn unclear_transaction(
        &mut self,
        cmd: UnclearTransactionCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        // Verify reconciliation exists and is in progress
        let status: String = self
            .store
            .connection()
            .query_row(
                "SELECT status FROM reconciliations WHERE id = ?1",
                [&cmd.reconciliation_id],
                |row| row.get(0),
            )
            .map_err(|_| ReconciliationCommandError::NotFound(cmd.reconciliation_id.clone()))?;

        if status == "completed" {
            return Err(ReconciliationCommandError::AlreadyCompleted);
        }
        if status == "abandoned" {
            return Err(ReconciliationCommandError::Abandoned);
        }

        // Check if actually cleared
        let is_cleared: bool = self
            .store
            .connection()
            .query_row(
                "SELECT 1 FROM cleared_transactions
                 WHERE reconciliation_id = ?1 AND entry_id = ?2 AND line_id = ?3",
                rusqlite::params![&cmd.reconciliation_id, &cmd.entry_id, &cmd.line_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !is_cleared {
            return Err(ReconciliationCommandError::NotCleared);
        }

        let event = Event::TransactionUncleared {
            reconciliation_id: cmd.reconciliation_id,
            entry_id: cmd.entry_id,
            line_id: cmd.line_id,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Complete a reconciliation
    pub fn complete_reconciliation(
        &mut self,
        cmd: CompleteReconciliationCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        // Verify reconciliation exists and is in progress
        let (status, statement_balance, account_id): (String, i64, String) = self
            .store
            .connection()
            .query_row(
                "SELECT status, statement_ending_balance, account_id FROM reconciliations WHERE id = ?1",
                [&cmd.reconciliation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|_| ReconciliationCommandError::NotFound(cmd.reconciliation_id.clone()))?;

        if status == "completed" {
            return Err(ReconciliationCommandError::AlreadyCompleted);
        }
        if status == "abandoned" {
            return Err(ReconciliationCommandError::Abandoned);
        }

        // Calculate cleared balance
        let cleared_total: i64 = self
            .store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(cleared_amount), 0) FROM cleared_transactions
                 WHERE reconciliation_id = ?1",
                [&cmd.reconciliation_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        // Get account's beginning balance (transactions before the reconciliation started)
        let beginning_balance: i64 = self
            .store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(jl.amount), 0)
                 FROM journal_lines jl
                 JOIN journal_entries je ON jl.entry_id = je.id
                 WHERE jl.account_id = ?1 AND jl.is_cleared = 1
                   AND jl.id NOT IN (SELECT line_id FROM cleared_transactions WHERE reconciliation_id = ?2)
                   AND je.is_void = 0",
                rusqlite::params![&account_id, &cmd.reconciliation_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let book_balance = beginning_balance + cleared_total;
        let difference = statement_balance - book_balance;

        let event = Event::ReconciliationCompleted {
            reconciliation_id: cmd.reconciliation_id,
            difference,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Abandon a reconciliation
    pub fn abandon_reconciliation(
        &mut self,
        cmd: AbandonReconciliationCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        // Verify reconciliation exists and is in progress
        let status: String = self
            .store
            .connection()
            .query_row(
                "SELECT status FROM reconciliations WHERE id = ?1",
                [&cmd.reconciliation_id],
                |row| row.get(0),
            )
            .map_err(|_| ReconciliationCommandError::NotFound(cmd.reconciliation_id.clone()))?;

        if status == "completed" {
            return Err(ReconciliationCommandError::AlreadyCompleted);
        }
        if status == "abandoned" {
            return Err(ReconciliationCommandError::Abandoned);
        }

        let event = Event::ReconciliationAbandoned {
            reconciliation_id: cmd.reconciliation_id,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Get reconciliation status
    pub fn get_reconciliation_status(
        &self,
        reconciliation_id: &str,
    ) -> Result<ReconciliationStatus, ReconciliationCommandError> {
        let (status, statement_balance, account_id, statement_date): (String, i64, String, String) =
            self.store
                .connection()
                .query_row(
                    "SELECT status, statement_ending_balance, account_id, statement_date
                 FROM reconciliations WHERE id = ?1",
                    [reconciliation_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|_| ReconciliationCommandError::NotFound(reconciliation_id.to_string()))?;

        let cleared_total: i64 = self
            .store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(cleared_amount), 0) FROM cleared_transactions
                 WHERE reconciliation_id = ?1",
                [reconciliation_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let cleared_count: i32 = self
            .store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM cleared_transactions WHERE reconciliation_id = ?1",
                [reconciliation_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        Ok(ReconciliationStatus {
            reconciliation_id: reconciliation_id.to_string(),
            account_id,
            statement_date: NaiveDate::parse_from_str(&statement_date, "%Y-%m-%d")
                .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap()),
            statement_ending_balance: statement_balance,
            status,
            cleared_total,
            cleared_count: cleared_count as u32,
            difference: statement_balance - cleared_total,
        })
    }
}

/// Reconciliation status summary
#[derive(Debug, Clone)]
pub struct ReconciliationStatus {
    pub reconciliation_id: String,
    pub account_id: String,
    pub statement_date: NaiveDate,
    pub statement_ending_balance: i64,
    pub status: String,
    pub cleared_total: i64,
    pub cleared_count: u32,
    pub difference: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::AccountType;
    use crate::events::types::JournalEntrySource;
    use crate::store::migrations::init_schema;

    fn setup() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    fn create_test_data(store: &mut EventStore) -> (String, String, String) {
        // Create accounts
        let mut acc_cmd = AccountCommands::new(store, "user".to_string());
        let checking_event = acc_cmd
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1010".to_string(),
                name: "Checking".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();

        let checking_id = if let Event::AccountCreated { account_id, .. } = checking_event.event {
            account_id
        } else {
            panic!("Wrong event");
        };

        acc_cmd
            .create_account(CreateAccountCommand {
                account_type: AccountType::Expense,
                account_number: "5000".to_string(),
                name: "Expense".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();

        let expense_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '5000'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Create a journal entry
        let mut entry_cmd = EntryCommands::new(store, "user".to_string());
        let entry_event = entry_cmd
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
                memo: "Test expense".to_string(),
                lines: vec![
                    EntryLine::debit(&expense_id, 10000, "USD"),
                    EntryLine::credit(&checking_id, 10000, "USD"),
                ],
                reference: Some("CHK-001".to_string()),
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();

        let entry_id = if let Event::JournalEntryPosted { entry_id, .. } = entry_event.event {
            entry_id
        } else {
            panic!("Wrong event");
        };

        let line_id = format!("{}-line-2", entry_id);
        (checking_id, entry_id, line_id)
    }

    #[test]
    fn test_start_reconciliation() {
        let mut store = setup();
        let (checking_id, _, _) = create_test_data(&mut store);

        let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
        let result = cmd.start_reconciliation(StartReconciliationCommand {
            account_id: checking_id,
            statement_date: NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            statement_ending_balance: 100000,
        });

        assert!(result.is_ok());

        // Verify reconciliation was created
        let count: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM reconciliations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_clear_transaction() {
        let mut store = setup();
        let (checking_id, entry_id, line_id) = create_test_data(&mut store);

        // Start reconciliation
        let recon_id: String;
        {
            let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
            let event = cmd
                .start_reconciliation(StartReconciliationCommand {
                    account_id: checking_id,
                    statement_date: NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                    statement_ending_balance: -10000, // Credit balance
                })
                .unwrap();

            if let Event::ReconciliationStarted {
                reconciliation_id, ..
            } = event.event
            {
                recon_id = reconciliation_id;
            } else {
                panic!("Wrong event");
            }
        }

        // Clear the transaction
        {
            let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
            cmd.clear_transaction(ClearTransactionCommand {
                reconciliation_id: recon_id.clone(),
                entry_id,
                line_id,
            })
            .unwrap();
        }

        // Verify transaction was cleared
        let cleared_count: i32 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM cleared_transactions WHERE reconciliation_id = ?1",
                [&recon_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(cleared_count, 1);
    }

    #[test]
    fn test_complete_reconciliation() {
        let mut store = setup();
        let (checking_id, entry_id, line_id) = create_test_data(&mut store);

        // Start and complete reconciliation
        let recon_id: String;
        {
            let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
            let event = cmd
                .start_reconciliation(StartReconciliationCommand {
                    account_id: checking_id,
                    statement_date: NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                    statement_ending_balance: -10000,
                })
                .unwrap();

            if let Event::ReconciliationStarted {
                reconciliation_id, ..
            } = event.event
            {
                recon_id = reconciliation_id;
            } else {
                panic!("Wrong event");
            }
        }

        {
            let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
            cmd.clear_transaction(ClearTransactionCommand {
                reconciliation_id: recon_id.clone(),
                entry_id,
                line_id,
            })
            .unwrap();
        }

        {
            let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
            cmd.complete_reconciliation(CompleteReconciliationCommand {
                reconciliation_id: recon_id.clone(),
            })
            .unwrap();
        }

        // Verify reconciliation was completed
        let status: String = store
            .connection()
            .query_row(
                "SELECT status FROM reconciliations WHERE id = ?1",
                [&recon_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[test]
    fn test_abandon_reconciliation() {
        let mut store = setup();
        let (checking_id, _, _) = create_test_data(&mut store);

        let recon_id: String;
        {
            let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
            let event = cmd
                .start_reconciliation(StartReconciliationCommand {
                    account_id: checking_id,
                    statement_date: NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                    statement_ending_balance: 100000,
                })
                .unwrap();

            if let Event::ReconciliationStarted {
                reconciliation_id, ..
            } = event.event
            {
                recon_id = reconciliation_id;
            } else {
                panic!("Wrong event");
            }
        }

        {
            let mut cmd = ReconciliationCommands::new(&mut store, "user".to_string());
            cmd.abandon_reconciliation(AbandonReconciliationCommand {
                reconciliation_id: recon_id.clone(),
            })
            .unwrap();
        }

        // Verify reconciliation was abandoned
        let status: String = store
            .connection()
            .query_row(
                "SELECT status FROM reconciliations WHERE id = ?1",
                [&recon_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "abandoned");
    }
}
