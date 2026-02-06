use crate::events::types::{
    Event, EventEnvelope, JournalEntrySource, JournalLineData, StoredEvent,
};
use crate::store::event_store::{EventStore, EventStoreError};
use crate::store::projections::Projector;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum EntryCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Projection error: {0}")]
    ProjectionError(#[from] crate::store::projections::ProjectionError),
    #[error("Entry not found: {0}")]
    NotFound(String),
    #[error("Entry is not balanced: sum is {0}")]
    NotBalanced(i64),
    #[error("Entry must have at least two lines")]
    InsufficientLines,
    #[error("Account not found: {0}")]
    AccountNotFound(String),
    #[error("Account is inactive: {0}")]
    AccountInactive(String),
    #[error("Entry already voided")]
    AlreadyVoided,
    #[error("Entry is not voided")]
    NotVoided,
    #[error("Period is closed for date: {0}")]
    PeriodClosed(NaiveDate),
    #[error("Invalid data: {0}")]
    InvalidData(String),
}

/// A line in a journal entry command
#[derive(Debug, Clone)]
pub struct EntryLine {
    pub account_id: String,
    /// Amount in smallest currency unit. Positive = debit, negative = credit
    pub amount: i64,
    pub currency: String,
    pub exchange_rate: Option<Decimal>,
    pub memo: Option<String>,
}

impl EntryLine {
    pub fn debit(account_id: &str, amount: i64, currency: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            amount: amount.abs(),
            currency: currency.to_string(),
            exchange_rate: None,
            memo: None,
        }
    }

    pub fn credit(account_id: &str, amount: i64, currency: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            amount: -amount.abs(),
            currency: currency.to_string(),
            exchange_rate: None,
            memo: None,
        }
    }

    pub fn with_exchange_rate(mut self, rate: Decimal) -> Self {
        self.exchange_rate = Some(rate);
        self
    }

    pub fn with_memo(mut self, memo: &str) -> Self {
        self.memo = Some(memo.to_string());
        self
    }
}

/// Command to post a journal entry
#[derive(Debug, Clone)]
pub struct PostEntryCommand {
    pub date: NaiveDate,
    pub memo: String,
    pub lines: Vec<EntryLine>,
    pub reference: Option<String>,
    pub source: Option<JournalEntrySource>,
}

/// Command to void a journal entry
#[derive(Debug, Clone)]
pub struct VoidEntryCommand {
    pub entry_id: String,
    pub reason: String,
}

/// Command to unvoid a journal entry
#[derive(Debug, Clone)]
pub struct UnvoidEntryCommand {
    pub entry_id: String,
    pub reason: String,
}

/// Command to add an annotation to a journal entry
#[derive(Debug, Clone)]
pub struct AnnotateEntryCommand {
    pub entry_id: String,
    pub annotation: String,
}

/// Command to reassign a journal line to a different account
#[derive(Debug, Clone)]
pub struct ReassignLineCommand {
    pub entry_id: String,
    pub line_id: String,
    pub new_account_id: String,
}

/// Journal entry command handler
pub struct EntryCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> EntryCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Post a new journal entry
    pub fn post_entry(&mut self, cmd: PostEntryCommand) -> Result<StoredEvent, EntryCommandError> {
        // Validate entry has at least 2 lines
        if cmd.lines.len() < 2 {
            return Err(EntryCommandError::InsufficientLines);
        }

        // Validate entry is balanced
        let sum: i64 = cmd.lines.iter().map(|l| l.amount).sum();
        if sum != 0 {
            return Err(EntryCommandError::NotBalanced(sum));
        }

        // Validate all accounts exist and are active
        for line in &cmd.lines {
            let result: Result<(bool,), _> = self.store.connection().query_row(
                "SELECT is_active = 1 FROM accounts WHERE id = ?1",
                [&line.account_id],
                |row| Ok((row.get(0)?,)),
            );

            match result {
                Ok((true,)) => {} // Account exists and is active
                Ok((false,)) => {
                    return Err(EntryCommandError::AccountInactive(line.account_id.clone()))
                }
                Err(_) => return Err(EntryCommandError::AccountNotFound(line.account_id.clone())),
            }
        }

        // Check if period is open (if fiscal periods exist)
        let period_closed: bool = self
            .store
            .connection()
            .query_row(
                "SELECT status = 'closed' FROM fiscal_periods
                 WHERE ?1 BETWEEN start_date AND end_date",
                [cmd.date.to_string()],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if period_closed {
            return Err(EntryCommandError::PeriodClosed(cmd.date));
        }

        let entry_id = Uuid::new_v4().to_string();

        let lines: Vec<JournalLineData> = cmd
            .lines
            .iter()
            .enumerate()
            .map(|(i, line)| JournalLineData {
                line_id: format!("{}-line-{}", entry_id, i + 1),
                account_id: line.account_id.clone(),
                amount: line.amount,
                currency: line.currency.clone(),
                exchange_rate: line.exchange_rate,
                memo: line.memo.clone(),
            })
            .collect();

        let event = Event::JournalEntryPosted {
            entry_id,
            date: cmd.date,
            memo: cmd.memo,
            lines,
            reference: cmd.reference,
            source: cmd.source,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Void an existing journal entry
    pub fn void_entry(&mut self, cmd: VoidEntryCommand) -> Result<StoredEvent, EntryCommandError> {
        // Verify the entry exists and is not already voided
        let is_void: i32 = self
            .store
            .connection()
            .query_row(
                "SELECT is_void FROM journal_entries WHERE id = ?1",
                [&cmd.entry_id],
                |row| row.get(0),
            )
            .map_err(|_| EntryCommandError::NotFound(cmd.entry_id.clone()))?;

        if is_void == 1 {
            return Err(EntryCommandError::AlreadyVoided);
        }

        // Mark the entry as voided
        let void_event = Event::JournalEntryVoided {
            entry_id: cmd.entry_id,
            reason: cmd.reason,
        };

        let void_envelope = EventEnvelope::new(void_event, self.user_id.clone());
        let void_stored = self.store.append(void_envelope)?;

        {
            let projector = Projector::new(self.store.connection());
            projector.apply(&void_stored)?;
        }

        Ok(void_stored)
    }

    /// Unvoid a voided journal entry
    pub fn unvoid_entry(
        &mut self,
        cmd: UnvoidEntryCommand,
    ) -> Result<StoredEvent, EntryCommandError> {
        // Verify the entry exists and is voided
        let is_void: i32 = self
            .store
            .connection()
            .query_row(
                "SELECT is_void FROM journal_entries WHERE id = ?1",
                [&cmd.entry_id],
                |row| row.get(0),
            )
            .map_err(|_| EntryCommandError::NotFound(cmd.entry_id.clone()))?;

        if is_void == 0 {
            return Err(EntryCommandError::NotVoided);
        }

        // Mark the entry as unvoided
        let unvoid_event = Event::JournalEntryUnvoided {
            entry_id: cmd.entry_id,
            reason: cmd.reason,
        };

        let unvoid_envelope = EventEnvelope::new(unvoid_event, self.user_id.clone());
        let unvoid_stored = self.store.append(unvoid_envelope)?;

        {
            let projector = Projector::new(self.store.connection());
            projector.apply(&unvoid_stored)?;
        }

        Ok(unvoid_stored)
    }

    /// Add an annotation to a journal entry
    pub fn annotate_entry(
        &mut self,
        cmd: AnnotateEntryCommand,
    ) -> Result<StoredEvent, EntryCommandError> {
        // Verify entry exists
        let exists: bool = self
            .store
            .connection()
            .query_row(
                "SELECT 1 FROM journal_entries WHERE id = ?1",
                [&cmd.entry_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !exists {
            return Err(EntryCommandError::NotFound(cmd.entry_id));
        }

        let event = Event::JournalEntryAnnotated {
            entry_id: cmd.entry_id,
            annotation: cmd.annotation,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Reassign a journal line to a different account
    pub fn reassign_line(
        &mut self,
        cmd: ReassignLineCommand,
    ) -> Result<StoredEvent, EntryCommandError> {
        // Verify entry exists and is not voided
        let is_void: bool = self
            .store
            .connection()
            .query_row(
                "SELECT is_void = 1 FROM journal_entries WHERE id = ?1",
                [&cmd.entry_id],
                |row| row.get(0),
            )
            .map_err(|_| EntryCommandError::NotFound(cmd.entry_id.clone()))?;

        if is_void {
            return Err(EntryCommandError::AlreadyVoided);
        }

        // Verify line exists and get old account
        let old_account_id: String = self
            .store
            .connection()
            .query_row(
                "SELECT account_id FROM journal_lines WHERE id = ?1 AND entry_id = ?2",
                [&cmd.line_id, &cmd.entry_id],
                |row| row.get(0),
            )
            .map_err(|_| {
                EntryCommandError::NotFound(format!(
                    "Line {} in entry {}",
                    cmd.line_id, cmd.entry_id
                ))
            })?;

        // Verify new account exists and is active
        let new_account_active: bool = self
            .store
            .connection()
            .query_row(
                "SELECT is_active = 1 FROM accounts WHERE id = ?1",
                [&cmd.new_account_id],
                |row| row.get(0),
            )
            .map_err(|_| EntryCommandError::AccountNotFound(cmd.new_account_id.clone()))?;

        if !new_account_active {
            return Err(EntryCommandError::AccountInactive(cmd.new_account_id));
        }

        // Don't do anything if the account isn't changing
        if old_account_id == cmd.new_account_id {
            return Err(EntryCommandError::InvalidData(
                "New account is the same as current account".to_string(),
            ));
        }

        let event = Event::JournalLineReassigned {
            entry_id: cmd.entry_id,
            line_id: cmd.line_id,
            old_account_id,
            new_account_id: cmd.new_account_id,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::domain::AccountType;
    use crate::store::migrations::init_schema;

    fn setup() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    fn create_test_accounts(store: &mut EventStore) {
        let mut commands = AccountCommands::new(store, "user".to_string());

        commands
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();

        commands
            .create_account(CreateAccountCommand {
                account_type: AccountType::Expense,
                account_number: "5000".to_string(),
                name: "Supplies".to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
    }

    #[test]
    fn test_post_entry() {
        let mut store = setup();
        create_test_accounts(&mut store);

        // Get account IDs
        let cash_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '1000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let expense_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '5000'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        let mut commands = EntryCommands::new(&mut store, "user".to_string());

        let cmd = PostEntryCommand {
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Bought supplies".to_string(),
            lines: vec![
                EntryLine::debit(&expense_id, 10000, "USD"),
                EntryLine::credit(&cash_id, 10000, "USD"),
            ],
            reference: Some("CHK-001".to_string()),
            source: Some(JournalEntrySource::Manual),
        };

        let result = commands.post_entry(cmd);
        assert!(result.is_ok());

        // Verify entry was created
        let count: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Verify lines
        let line_count: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_lines", [], |row| row.get(0))
            .unwrap();
        assert_eq!(line_count, 2);

        // Verify balance
        let sum: i64 = store
            .connection()
            .query_row("SELECT SUM(amount) FROM journal_lines", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sum, 0);
    }

    #[test]
    fn test_unbalanced_entry_rejected() {
        let mut store = setup();
        create_test_accounts(&mut store);

        let cash_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '1000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let expense_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '5000'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        let mut commands = EntryCommands::new(&mut store, "user".to_string());

        let cmd = PostEntryCommand {
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Unbalanced".to_string(),
            lines: vec![
                EntryLine::debit(&expense_id, 10000, "USD"),
                EntryLine::credit(&cash_id, 5000, "USD"), // Not balanced!
            ],
            reference: None,
            source: None,
        };

        let result = commands.post_entry(cmd);
        assert!(matches!(result, Err(EntryCommandError::NotBalanced(5000))));
    }

    #[test]
    fn test_void_entry() {
        let mut store = setup();
        create_test_accounts(&mut store);

        let cash_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '1000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let expense_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '5000'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Post an entry
        let entry_id: String;
        {
            let mut commands = EntryCommands::new(&mut store, "user".to_string());
            let cmd = PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
                memo: "Original entry".to_string(),
                lines: vec![
                    EntryLine::debit(&expense_id, 10000, "USD"),
                    EntryLine::credit(&cash_id, 10000, "USD"),
                ],
                reference: None,
                source: None,
            };
            let result = commands.post_entry(cmd).unwrap();
            if let Event::JournalEntryPosted { entry_id: id, .. } = result.event {
                entry_id = id;
            } else {
                panic!("Wrong event type");
            }
        }

        // Void the entry
        {
            let mut commands = EntryCommands::new(&mut store, "user".to_string());
            let cmd = VoidEntryCommand {
                entry_id: entry_id.clone(),
                reason: "Error in entry".to_string(),
            };
            commands.void_entry(cmd).unwrap();
        }

        // Verify original is voided
        let is_void: i32 = store
            .connection()
            .query_row(
                "SELECT is_void FROM journal_entries WHERE id = ?1",
                [&entry_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_void, 1);

        // Verify there is still only 1 entry (no reversal created)
        let count: i32 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Verify net balance is zero (voided entries excluded)
        let net: Option<i64> = store
            .connection()
            .query_row(
                "SELECT SUM(jl.amount)
                 FROM journal_lines jl
                 JOIN journal_entries je ON jl.entry_id = je.id
                 WHERE je.is_void = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(net, None); // No non-voided entries
    }

    #[test]
    fn test_inactive_account_rejected() {
        let mut store = setup();
        create_test_accounts(&mut store);

        let cash_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '1000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let expense_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '5000'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // Deactivate expense account
        store
            .connection()
            .execute(
                "UPDATE accounts SET is_active = 0 WHERE id = ?1",
                [&expense_id],
            )
            .unwrap();

        let mut commands = EntryCommands::new(&mut store, "user".to_string());

        let cmd = PostEntryCommand {
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test".to_string(),
            lines: vec![
                EntryLine::debit(&expense_id, 10000, "USD"),
                EntryLine::credit(&cash_id, 10000, "USD"),
            ],
            reference: None,
            source: None,
        };

        let result = commands.post_entry(cmd);
        assert!(matches!(result, Err(EntryCommandError::AccountInactive(_))));
    }
}
