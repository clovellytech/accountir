use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use chrono::NaiveDate;
use rusqlite::OptionalExtension;
use thiserror::Error;
use uuid::Uuid;

/// In-txn guard that a reconciliation exists and is still in progress. Returns
/// `Some(err)` (`NotFound` / `AlreadyCompleted` / `Abandoned`) or `None` if it is
/// in progress and safe to mutate.
fn recon_in_progress_in_txn(
    tx: &rusqlite::Transaction<'_>,
    reconciliation_id: &str,
) -> Result<Option<ReconciliationCommandError>, EventStoreError> {
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM reconciliations WHERE id = ?1",
            [reconciliation_id],
            |row| row.get(0),
        )
        .optional()?;
    match status.as_deref() {
        None => Ok(Some(ReconciliationCommandError::NotFound(
            reconciliation_id.to_string(),
        ))),
        Some("completed") => Ok(Some(ReconciliationCommandError::AlreadyCompleted)),
        Some("abandoned") => Ok(Some(ReconciliationCommandError::Abandoned)),
        Some(_) => Ok(None),
    }
}

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
    #[error("Account {0} already has a reconciliation in progress")]
    AlreadyInProgress(String),
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

/// Outcome of a reconciliation command's in-txn validation: the invariants held
/// (append this event) or a domain invariant was violated (reject). Mirrors
/// [`crate::commands::account_commands::AccountStep`]. The caller wraps the event
/// in an envelope, stamping identity as appropriate (local `user_id` vs. the
/// server-authenticated actor on the sync path).
pub(crate) enum ReconciliationStep {
    /// All invariants hold under the write lock; append this event.
    Append(Event),
    /// A domain invariant was violated.
    Reject(ReconciliationCommandError),
}

/// Run `start_reconciliation`'s state-dependent invariants inside the append
/// transaction — the account exists AND has no other in-progress reconciliation —
/// and, if they hold, build the `ReconciliationStarted` event. Shared by
/// [`ReconciliationCommands::start_reconciliation`] and the server-side sync submit
/// path so both enforce the SAME ≤1-in-progress-per-account fence under the write
/// lock (audit `ReconciliationStarted`, HIGH).
pub(crate) fn build_start_reconciliation_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &StartReconciliationCommand,
) -> Result<ReconciliationStep, EventStoreError> {
    let account_exists: bool = tx
        .query_row(
            "SELECT 1 FROM accounts WHERE id = ?1",
            [&cmd.account_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !account_exists {
        return Ok(ReconciliationStep::Reject(
            ReconciliationCommandError::AccountNotFound(cmd.account_id.clone()),
        ));
    }

    // At most one in-progress reconciliation per account.
    let in_progress: bool = tx
        .query_row(
            "SELECT 1 FROM reconciliations
             WHERE account_id = ?1 AND status = 'in_progress'",
            [&cmd.account_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if in_progress {
        return Ok(ReconciliationStep::Reject(
            ReconciliationCommandError::AlreadyInProgress(cmd.account_id.clone()),
        ));
    }

    let event = Event::ReconciliationStarted {
        reconciliation_id: Uuid::new_v4().to_string(),
        account_id: cmd.account_id.clone(),
        statement_date: cmd.statement_date,
        statement_ending_balance: cmd.statement_ending_balance,
    };
    Ok(ReconciliationStep::Append(event))
}

/// Run `clear_transaction`'s state-dependent invariants inside the append
/// transaction — the reconciliation is in progress, the line exists, and it is not
/// already cleared — and, if they hold, build the `TransactionCleared` event.
/// Shared by [`ReconciliationCommands::clear_transaction`] and the server-side sync
/// submit path.
pub(crate) fn build_clear_transaction_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ClearTransactionCommand,
) -> Result<ReconciliationStep, EventStoreError> {
    if let Some(e) = recon_in_progress_in_txn(tx, &cmd.reconciliation_id)? {
        return Ok(ReconciliationStep::Reject(e));
    }

    // The entry/line must exist.
    let amount: Option<i64> = tx
        .query_row(
            "SELECT amount FROM journal_lines WHERE id = ?1 AND entry_id = ?2",
            rusqlite::params![&cmd.line_id, &cmd.entry_id],
            |row| row.get(0),
        )
        .optional()?;
    let amount = match amount {
        Some(a) => a,
        None => {
            return Ok(ReconciliationStep::Reject(
                ReconciliationCommandError::LineNotFound(cmd.line_id.clone()),
            ))
        }
    };

    // Not already cleared in this reconciliation.
    let already_cleared: bool = tx
        .query_row(
            "SELECT 1 FROM cleared_transactions
             WHERE reconciliation_id = ?1 AND entry_id = ?2 AND line_id = ?3",
            rusqlite::params![&cmd.reconciliation_id, &cmd.entry_id, &cmd.line_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if already_cleared {
        return Ok(ReconciliationStep::Reject(
            ReconciliationCommandError::AlreadyCleared,
        ));
    }

    let event = Event::TransactionCleared {
        reconciliation_id: cmd.reconciliation_id.clone(),
        entry_id: cmd.entry_id.clone(),
        line_id: cmd.line_id.clone(),
        cleared_amount: amount,
    };
    Ok(ReconciliationStep::Append(event))
}

/// Run `unclear_transaction`'s state-dependent invariants inside the append
/// transaction — the reconciliation is in progress and the line is actually cleared
/// — and, if they hold, build the `TransactionUncleared` event. Shared by
/// [`ReconciliationCommands::unclear_transaction`] and the server-side sync submit
/// path.
pub(crate) fn build_unclear_transaction_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &UnclearTransactionCommand,
) -> Result<ReconciliationStep, EventStoreError> {
    if let Some(e) = recon_in_progress_in_txn(tx, &cmd.reconciliation_id)? {
        return Ok(ReconciliationStep::Reject(e));
    }

    let is_cleared: bool = tx
        .query_row(
            "SELECT 1 FROM cleared_transactions
             WHERE reconciliation_id = ?1 AND entry_id = ?2 AND line_id = ?3",
            rusqlite::params![&cmd.reconciliation_id, &cmd.entry_id, &cmd.line_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !is_cleared {
        return Ok(ReconciliationStep::Reject(
            ReconciliationCommandError::NotCleared,
        ));
    }

    let event = Event::TransactionUncleared {
        reconciliation_id: cmd.reconciliation_id.clone(),
        entry_id: cmd.entry_id.clone(),
        line_id: cmd.line_id.clone(),
    };
    Ok(ReconciliationStep::Append(event))
}

/// Run `complete_reconciliation`'s state-dependent logic inside the append
/// transaction — the reconciliation is in progress, and the `difference` snapshot
/// is computed from the cleared set and beginning balance under the write lock —
/// and, if it holds, build the `ReconciliationCompleted` event. Shared by
/// [`ReconciliationCommands::complete_reconciliation`] and the server-side sync
/// submit path so a concurrent `TransactionCleared`/`Uncleared` can't make the
/// stored difference wrong (audit `ReconciliationCompleted`, HIGH).
pub(crate) fn build_complete_reconciliation_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &CompleteReconciliationCommand,
) -> Result<ReconciliationStep, EventStoreError> {
    let recon: Option<(String, i64, String)> = tx
        .query_row(
            "SELECT status, statement_ending_balance, account_id
             FROM reconciliations WHERE id = ?1",
            [&cmd.reconciliation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (status, statement_balance, account_id) = match recon {
        Some(r) => r,
        None => {
            return Ok(ReconciliationStep::Reject(
                ReconciliationCommandError::NotFound(cmd.reconciliation_id.clone()),
            ))
        }
    };
    if status == "completed" {
        return Ok(ReconciliationStep::Reject(
            ReconciliationCommandError::AlreadyCompleted,
        ));
    }
    if status == "abandoned" {
        return Ok(ReconciliationStep::Reject(
            ReconciliationCommandError::Abandoned,
        ));
    }

    // Cleared balance for this reconciliation.
    let cleared_total: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(cleared_amount), 0) FROM cleared_transactions
             WHERE reconciliation_id = ?1",
            [&cmd.reconciliation_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);

    // Account's beginning balance (already-cleared lines from prior
    // reconciliations).
    let beginning_balance: i64 = tx
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
        .optional()?
        .unwrap_or(0);

    let difference = statement_balance - (beginning_balance + cleared_total);
    let event = Event::ReconciliationCompleted {
        reconciliation_id: cmd.reconciliation_id.clone(),
        difference,
    };
    Ok(ReconciliationStep::Append(event))
}

/// Run `abandon_reconciliation`'s state-dependent invariant inside the append
/// transaction — the reconciliation is in progress — and, if it holds, build the
/// `ReconciliationAbandoned` event. Shared by
/// [`ReconciliationCommands::abandon_reconciliation`] and the server-side sync
/// submit path.
pub(crate) fn build_abandon_reconciliation_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &AbandonReconciliationCommand,
) -> Result<ReconciliationStep, EventStoreError> {
    if let Some(e) = recon_in_progress_in_txn(tx, &cmd.reconciliation_id)? {
        return Ok(ReconciliationStep::Reject(e));
    }
    let event = Event::ReconciliationAbandoned {
        reconciliation_id: cmd.reconciliation_id.clone(),
    };
    Ok(ReconciliationStep::Append(event))
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

    /// Start a new reconciliation.
    ///
    /// Enforces, inside the append transaction, that the account exists and has
    /// **no other in-progress reconciliation** (audit `ReconciliationStarted`,
    /// HIGH — previously unenforced). A partial unique index
    /// `reconciliations(account_id) WHERE status='in_progress'` is the DB-level
    /// backstop. Retries on a head move.
    pub fn start_reconciliation(
        &mut self,
        cmd: StartReconciliationCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_start_reconciliation_in_txn(tx, &cmd)? {
                    ReconciliationStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(stored),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Clear a transaction in a reconciliation.
    ///
    /// Re-checks, in the append transaction, that the reconciliation is in
    /// progress, the line exists, and it is not already cleared. Retries on a
    /// head move.
    pub fn clear_transaction(
        &mut self,
        cmd: ClearTransactionCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_clear_transaction_in_txn(tx, &cmd)? {
                    ReconciliationStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(stored),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Unclear a transaction in a reconciliation.
    ///
    /// Re-checks, in the append transaction, that the reconciliation is in
    /// progress and the line is actually cleared. Retries on a head move.
    pub fn unclear_transaction(
        &mut self,
        cmd: UnclearTransactionCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_unclear_transaction_in_txn(tx, &cmd)? {
                    ReconciliationStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(stored),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Complete a reconciliation.
    ///
    /// The `difference` snapshot is computed from the cleared set and beginning
    /// balance **inside** the append transaction (audit `ReconciliationCompleted`,
    /// HIGH), so a concurrent `TransactionCleared`/`Uncleared` can't make the
    /// stored difference wrong. Retries on a head move.
    pub fn complete_reconciliation(
        &mut self,
        cmd: CompleteReconciliationCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_complete_reconciliation_in_txn(tx, &cmd)? {
                    ReconciliationStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(stored),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Abandon a reconciliation.
    ///
    /// Re-checks in-progress status inside the append transaction, then frees the
    /// account's in-progress slot. Retries on a head move.
    pub fn abandon_reconciliation(
        &mut self,
        cmd: AbandonReconciliationCommand,
    ) -> Result<StoredEvent, ReconciliationCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_abandon_reconciliation_in_txn(tx, &cmd)? {
                    ReconciliationStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    ReconciliationStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(stored),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
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

    #[test]
    fn second_start_on_same_account_rejected_until_freed() {
        let mut store = setup();
        let (checking_id, _, _) = create_test_data(&mut store);

        let first = ReconciliationCommands::new(&mut store, "user".to_string())
            .start_reconciliation(StartReconciliationCommand {
                account_id: checking_id.clone(),
                statement_date: NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                statement_ending_balance: 100000,
            })
            .unwrap();
        let first_id = match first.event {
            Event::ReconciliationStarted {
                reconciliation_id, ..
            } => reconciliation_id,
            _ => panic!("expected ReconciliationStarted"),
        };

        // A second start while the first is in progress is rejected.
        let err = ReconciliationCommands::new(&mut store, "user".to_string())
            .start_reconciliation(StartReconciliationCommand {
                account_id: checking_id.clone(),
                statement_date: NaiveDate::from_ymd_opt(2024, 2, 28).unwrap(),
                statement_ending_balance: 120000,
            })
            .unwrap_err();
        assert!(matches!(
            err,
            ReconciliationCommandError::AlreadyInProgress(_)
        ));

        // Abandoning the first frees the account's slot.
        ReconciliationCommands::new(&mut store, "user".to_string())
            .abandon_reconciliation(AbandonReconciliationCommand {
                reconciliation_id: first_id,
            })
            .unwrap();
        ReconciliationCommands::new(&mut store, "user".to_string())
            .start_reconciliation(StartReconciliationCommand {
                account_id: checking_id,
                statement_date: NaiveDate::from_ymd_opt(2024, 2, 28).unwrap(),
                statement_ending_balance: 120000,
            })
            .unwrap();
    }

    #[test]
    fn concurrent_starts_same_account_only_one_wins() {
        // The ≤1-in-progress-per-account invariant (audit ReconciliationStarted,
        // previously unenforced), across two connections. The in-txn check + the
        // partial unique index let exactly one start land; the other is rejected
        // AlreadyInProgress.
        let dir = std::env::temp_dir().join(format!("accountir-recon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("log.db");
        let checking_id = {
            let mut store = EventStore::open(&db).unwrap();
            init_schema(store.connection()).unwrap();
            let (checking_id, _, _) = create_test_data(&mut store);
            checking_id
        };

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn_start = |db: std::path::PathBuf,
                           account_id: String,
                           barrier: std::sync::Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                let mut store = EventStore::open(&db).unwrap();
                let mut cmds = ReconciliationCommands::new(&mut store, "user".to_string());
                barrier.wait();
                cmds.start_reconciliation(StartReconciliationCommand {
                    account_id,
                    statement_date: NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                    statement_ending_balance: 100000,
                })
            })
        };

        let t1 = spawn_start(db.clone(), checking_id.clone(), barrier.clone());
        let t2 = spawn_start(db.clone(), checking_id.clone(), barrier.clone());
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, 1,
            "only one reconciliation may start per account (r1={r1:?}, r2={r2:?})"
        );
        for r in [&r1, &r2] {
            if let Err(e) = r {
                assert!(
                    matches!(e, ReconciliationCommandError::AlreadyInProgress(_)),
                    "the loser must be rejected AlreadyInProgress, got {e:?}"
                );
            }
        }

        let store = EventStore::open(&db).unwrap();
        let n: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM reconciliations
                 WHERE account_id = ?1 AND status = 'in_progress'",
                [&checking_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 1,
            "exactly one in-progress reconciliation for the account"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migration_path_creates_enforcing_partial_index() {
        // Production initializes with init_schema followed by run_migrations;
        // confirm that sequence is clean (migration 013's IF NOT EXISTS index
        // co-exists with init_schema's) and that the resulting partial unique
        // index actually rejects a second in-progress reconciliation.
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        crate::store::migrations::run_migrations(store.connection()).unwrap();

        let acc = AccountCommands::new(&mut store, "u".to_string())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1010".to_string(),
                name: "Checking".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            })
            .unwrap();
        let account_id = match acc.event {
            Event::AccountCreated { account_id, .. } => account_id,
            _ => panic!("expected AccountCreated"),
        };

        let conn = store.connection();
        conn.execute(
            "INSERT INTO reconciliations (id, account_id, statement_date, statement_ending_balance, status)
             VALUES ('r1', ?1, '2024-01-01', 0, 'in_progress')",
            [&account_id],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO reconciliations (id, account_id, statement_date, statement_ending_balance, status)
             VALUES ('r2', ?1, '2024-02-01', 0, 'in_progress')",
            [&account_id],
        );
        assert!(
            dup.is_err(),
            "the partial unique index must reject a second in-progress reconciliation"
        );
    }
}
