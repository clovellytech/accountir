use crate::domain::AccountType;
use crate::events::types::{Event, EventAccountType, EventEnvelope, StoredEvent};
use crate::store::event_store::{EventStore, EventStoreError};
use crate::store::projections::Projector;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum AccountCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Projection error: {0}")]
    ProjectionError(#[from] crate::store::projections::ProjectionError),
    #[error("Account not found: {0}")]
    NotFound(String),
    #[error("Account already exists: {0}")]
    AlreadyExists(String),
    #[error("Invalid account data: {0}")]
    InvalidData(String),
    #[error("Account has balance, cannot deactivate")]
    HasBalance,
}

/// Command to create a new account
#[derive(Debug, Clone)]
pub struct CreateAccountCommand {
    pub account_type: AccountType,
    pub account_number: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub currency: Option<String>,
    pub description: Option<String>,
}

/// Command to update an account
#[derive(Debug, Clone)]
pub struct UpdateAccountCommand {
    pub account_id: String,
    pub account_number: Option<String>,
    pub name: Option<String>,
    pub parent_id: Option<Option<String>>, // Some(None) = clear parent, Some(Some(id)) = set parent, None = no change
    pub description: Option<String>,
}

/// Command to deactivate an account
#[derive(Debug, Clone)]
pub struct DeactivateAccountCommand {
    pub account_id: String,
    pub reason: Option<String>,
}

/// Command to reactivate an account
#[derive(Debug, Clone)]
pub struct ReactivateAccountCommand {
    pub account_id: String,
}

/// Account command handler
pub struct AccountCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> AccountCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Create a new account
    pub fn create_account(
        &mut self,
        cmd: CreateAccountCommand,
    ) -> Result<StoredEvent, AccountCommandError> {
        // Check for duplicate account number
        let exists: bool = self
            .store
            .connection()
            .query_row(
                "SELECT 1 FROM accounts WHERE account_number = ?1",
                [&cmd.account_number],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if exists {
            return Err(AccountCommandError::AlreadyExists(cmd.account_number));
        }

        let account_id = Uuid::new_v4().to_string();
        let account_type = EventAccountType::from(cmd.account_type);

        let event = Event::AccountCreated {
            account_id,
            account_type,
            account_number: cmd.account_number,
            name: cmd.name,
            parent_id: cmd.parent_id,
            currency: cmd.currency,
            description: cmd.description,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        // Apply projection
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Update an existing account
    pub fn update_account(
        &mut self,
        cmd: UpdateAccountCommand,
    ) -> Result<Vec<StoredEvent>, AccountCommandError> {
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
            return Err(AccountCommandError::NotFound(cmd.account_id));
        }

        let mut events = Vec::new();

        // Update account_number
        if let Some(new_number) = cmd.account_number {
            let old_number: String = self
                .store
                .connection()
                .query_row(
                    "SELECT account_number FROM accounts WHERE id = ?1",
                    [&cmd.account_id],
                    |row| row.get(0),
                )
                .map_err(|_| AccountCommandError::NotFound(cmd.account_id.clone()))?;

            if old_number != new_number {
                // Check for duplicate account number (excluding current account)
                let duplicate: bool = self
                    .store
                    .connection()
                    .query_row(
                        "SELECT 1 FROM accounts WHERE account_number = ?1 AND id != ?2",
                        [&new_number, &cmd.account_id],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);

                if duplicate {
                    return Err(AccountCommandError::AlreadyExists(new_number));
                }

                let event = Event::AccountUpdated {
                    account_id: cmd.account_id.clone(),
                    field: "account_number".to_string(),
                    old_value: old_number,
                    new_value: new_number,
                };

                let envelope = EventEnvelope::new(event, self.user_id.clone());
                let stored = self.store.append(envelope)?;

                let projector = Projector::new(self.store.connection());
                projector.apply(&stored)?;

                events.push(stored);
            }
        }

        // Update name
        if let Some(new_name) = cmd.name {
            let old_name: String = self
                .store
                .connection()
                .query_row(
                    "SELECT name FROM accounts WHERE id = ?1",
                    [&cmd.account_id],
                    |row| row.get(0),
                )
                .map_err(|_| AccountCommandError::NotFound(cmd.account_id.clone()))?;

            if old_name != new_name {
                let event = Event::AccountUpdated {
                    account_id: cmd.account_id.clone(),
                    field: "name".to_string(),
                    old_value: old_name,
                    new_value: new_name,
                };

                let envelope = EventEnvelope::new(event, self.user_id.clone());
                let stored = self.store.append(envelope)?;

                let projector = Projector::new(self.store.connection());
                projector.apply(&stored)?;

                events.push(stored);
            }
        }

        // Update parent_id
        if let Some(new_parent) = cmd.parent_id {
            let old_parent: Option<String> = self
                .store
                .connection()
                .query_row(
                    "SELECT parent_id FROM accounts WHERE id = ?1",
                    [&cmd.account_id],
                    |row| row.get(0),
                )
                .unwrap_or(None);

            let old_parent_str = old_parent.unwrap_or_default();
            let new_parent_str = new_parent.unwrap_or_default();

            if old_parent_str != new_parent_str {
                let event = Event::AccountUpdated {
                    account_id: cmd.account_id.clone(),
                    field: "parent_id".to_string(),
                    old_value: old_parent_str,
                    new_value: new_parent_str,
                };

                let envelope = EventEnvelope::new(event, self.user_id.clone());
                let stored = self.store.append(envelope)?;

                let projector = Projector::new(self.store.connection());
                projector.apply(&stored)?;

                events.push(stored);
            }
        }

        // Update description
        if let Some(new_desc) = cmd.description {
            let old_desc: Option<String> = self
                .store
                .connection()
                .query_row(
                    "SELECT description FROM accounts WHERE id = ?1",
                    [&cmd.account_id],
                    |row| row.get(0),
                )
                .unwrap_or(None);

            let old_desc_str = old_desc.unwrap_or_default();
            if old_desc_str != new_desc {
                let event = Event::AccountUpdated {
                    account_id: cmd.account_id.clone(),
                    field: "description".to_string(),
                    old_value: old_desc_str,
                    new_value: new_desc,
                };

                let envelope = EventEnvelope::new(event, self.user_id.clone());
                let stored = self.store.append(envelope)?;

                let projector = Projector::new(self.store.connection());
                projector.apply(&stored)?;

                events.push(stored);
            }
        }

        Ok(events)
    }

    /// Deactivate an account
    pub fn deactivate_account(
        &mut self,
        cmd: DeactivateAccountCommand,
    ) -> Result<StoredEvent, AccountCommandError> {
        // Verify account exists and is active
        let is_active: bool = self
            .store
            .connection()
            .query_row(
                "SELECT is_active = 1 FROM accounts WHERE id = ?1",
                [&cmd.account_id],
                |row| row.get(0),
            )
            .map_err(|_| AccountCommandError::NotFound(cmd.account_id.clone()))?;

        if !is_active {
            return Err(AccountCommandError::InvalidData(
                "Account is already inactive".to_string(),
            ));
        }

        // Check if account has balance
        let balance: i64 = self
            .store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(jl.amount), 0)
                 FROM journal_lines jl
                 JOIN journal_entries je ON jl.entry_id = je.id
                 WHERE jl.account_id = ?1 AND je.is_void = 0",
                [&cmd.account_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if balance != 0 {
            return Err(AccountCommandError::HasBalance);
        }

        let event = Event::AccountDeactivated {
            account_id: cmd.account_id,
            reason: cmd.reason,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;

        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok(stored)
    }

    /// Reactivate an account
    pub fn reactivate_account(
        &mut self,
        cmd: ReactivateAccountCommand,
    ) -> Result<StoredEvent, AccountCommandError> {
        // Verify account exists and is inactive
        let is_active: bool = self
            .store
            .connection()
            .query_row(
                "SELECT is_active = 1 FROM accounts WHERE id = ?1",
                [&cmd.account_id],
                |row| row.get(0),
            )
            .map_err(|_| AccountCommandError::NotFound(cmd.account_id.clone()))?;

        if is_active {
            return Err(AccountCommandError::InvalidData(
                "Account is already active".to_string(),
            ));
        }

        let event = Event::AccountReactivated {
            account_id: cmd.account_id,
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
    use crate::store::migrations::init_schema;

    fn setup() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    #[test]
    fn test_create_account() {
        let mut store = setup();
        let mut commands = AccountCommands::new(&mut store, "user-001".to_string());

        let cmd = CreateAccountCommand {
            account_type: AccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: Some("Main cash account".to_string()),
        };

        let result = commands.create_account(cmd);
        assert!(result.is_ok());

        // Verify account was created
        let name: String = store
            .connection()
            .query_row(
                "SELECT name FROM accounts WHERE account_number = '1000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Cash");
    }

    #[test]
    fn test_create_duplicate_account_number() {
        let mut store = setup();

        // Create first account
        {
            let mut commands = AccountCommands::new(&mut store, "user-001".to_string());
            let cmd = CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            };
            commands.create_account(cmd).unwrap();
        }

        // Try to create duplicate
        {
            let mut commands = AccountCommands::new(&mut store, "user-001".to_string());
            let cmd = CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Another Cash".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            };
            let result = commands.create_account(cmd);
            assert!(matches!(result, Err(AccountCommandError::AlreadyExists(_))));
        }
    }

    #[test]
    fn test_update_account() {
        let mut store = setup();

        // Create account
        let account_id: String;
        {
            let mut commands = AccountCommands::new(&mut store, "user-001".to_string());
            let cmd = CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            };
            let result = commands.create_account(cmd).unwrap();
            if let Event::AccountCreated { account_id: id, .. } = result.event {
                account_id = id;
            } else {
                panic!("Wrong event type");
            }
        }

        // Update account
        {
            let mut commands = AccountCommands::new(&mut store, "user-001".to_string());
            let cmd = UpdateAccountCommand {
                account_id: account_id.clone(),
                account_number: None, // No change
                name: Some("Petty Cash".to_string()),
                parent_id: None, // No change
                description: Some("Updated description".to_string()),
            };
            let events = commands.update_account(cmd).unwrap();
            assert_eq!(events.len(), 2); // name and description updates
        }

        // Verify updates
        let name: String = store
            .connection()
            .query_row(
                "SELECT name FROM accounts WHERE id = ?1",
                [&account_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "Petty Cash");
    }

    #[test]
    fn test_deactivate_account() {
        let mut store = setup();

        // Create account
        let account_id: String;
        {
            let mut commands = AccountCommands::new(&mut store, "user-001".to_string());
            let cmd = CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Cash".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            };
            let result = commands.create_account(cmd).unwrap();
            if let Event::AccountCreated { account_id: id, .. } = result.event {
                account_id = id;
            } else {
                panic!("Wrong event type");
            }
        }

        // Deactivate account
        {
            let mut commands = AccountCommands::new(&mut store, "user-001".to_string());
            let cmd = DeactivateAccountCommand {
                account_id: account_id.clone(),
                reason: Some("No longer used".to_string()),
            };
            commands.deactivate_account(cmd).unwrap();
        }

        // Verify deactivation
        let is_active: i32 = store
            .connection()
            .query_row(
                "SELECT is_active FROM accounts WHERE id = ?1",
                [&account_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(is_active, 0);
    }
}
