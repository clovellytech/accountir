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

/// Find or create the "Uncategorized" expense account.
/// Uses the event store so the creation is properly event-sourced.
pub fn find_or_create_uncategorized(store: &mut EventStore) -> Result<String, AccountCommandError> {
    let conn = store.connection();

    // Check if it already exists
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM accounts WHERE LOWER(name) = 'uncategorized' AND is_active = 1",
            [],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        return Ok(id);
    }

    // Find next available account number in 9000 range
    let next_number: String = conn
        .query_row(
            "SELECT MAX(CAST(account_number AS INTEGER)) FROM accounts WHERE account_number LIKE '9%'",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .map(|n| format!("{}", n + 1))
        .unwrap_or_else(|| "9000".to_string());

    let mut commands = AccountCommands::new(store, "system".to_string());
    let stored = commands.create_account(CreateAccountCommand {
        account_type: AccountType::Expense,
        account_number: next_number,
        name: "Uncategorized".to_string(),
        parent_id: None,
        currency: Some("USD".to_string()),
        description: Some("Uncategorized transactions".to_string()),
    })?;

    if let Event::AccountCreated { account_id, .. } = &stored.event {
        Ok(account_id.clone())
    } else {
        Err(AccountCommandError::InvalidData(
            "Unexpected event type".to_string(),
        ))
    }
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

/// Check if the database has any active accounts.
pub fn has_no_accounts(store: &EventStore) -> bool {
    let count: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE is_active = 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    count == 0
}

/// Ensure a default company exists. Returns a status message if one was created.
pub fn ensure_company(store: &mut EventStore, db_path: &std::path::Path) -> Option<String> {
    let has_company: bool = store
        .connection()
        .query_row(
            "SELECT COUNT(*) > 0 FROM company WHERE id = 'default'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if has_company {
        return None;
    }

    let company_name = db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("My Company")
        .to_string();
    let company_id = Uuid::new_v4().to_string();
    let envelope = crate::events::types::EventEnvelope::new(
        Event::CompanyCreated {
            company_id,
            name: company_name.clone(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 1,
        },
        "system".to_string(),
    );
    match store.append(envelope) {
        Ok(stored) => {
            let projector = crate::store::projections::Projector::new(store.connection());
            if let Err(e) = projector.apply(&stored) {
                return Some(format!("Failed to project company: {}", e));
            }
            Some(format!("Company '{}' created for sync", company_name))
        }
        Err(e) => Some(format!("Failed to create company: {}", e)),
    }
}

/// Create the default chart of accounts. Returns the count of accounts created.
pub fn create_default_accounts(store: &mut EventStore) -> Result<usize, String> {
    let defaults: Vec<(&str, &str, AccountType, Option<&str>)> = vec![
        ("1000", "Assets", AccountType::Asset, None),
        (
            "1001",
            "Business Checking",
            AccountType::Asset,
            Some("1000"),
        ),
        ("2000", "Income", AccountType::Revenue, None),
        ("3000", "Expenses", AccountType::Expense, None),
        ("4000", "Equity", AccountType::Equity, None),
        (
            "4001",
            "Opening Balances",
            AccountType::Equity,
            Some("4000"),
        ),
        ("5000", "Liabilities", AccountType::Liability, None),
    ];

    let mut account_ids: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut created = 0;

    for (number, name, account_type, _parent_number) in &defaults {
        let mut commands = AccountCommands::new(store, "system".to_string());
        let cmd = CreateAccountCommand {
            account_type: *account_type,
            account_number: number.to_string(),
            name: name.to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };
        match commands.create_account(cmd) {
            Ok(stored) => {
                if let Event::AccountCreated { account_id, .. } = &stored.event {
                    account_ids.insert(number.to_string(), account_id.clone());
                }
                created += 1;
            }
            Err(e) => return Err(format!("Failed to create account {}: {}", number, e)),
        }
    }

    for (number, _name, _account_type, parent_number) in &defaults {
        if let Some(parent_num) = parent_number {
            let account_id = account_ids.get(*number).cloned();
            let parent_id = account_ids.get(*parent_num).cloned();
            if let (Some(aid), Some(pid)) = (account_id, parent_id) {
                let mut commands = AccountCommands::new(store, "system".to_string());
                let cmd = UpdateAccountCommand {
                    account_id: aid,
                    account_number: None,
                    name: None,
                    parent_id: Some(Some(pid)),
                    description: None,
                };
                if let Err(e) = commands.update_account(cmd) {
                    return Err(format!("Failed to set parent for {}: {}", number, e));
                }
            }
        }
    }

    Ok(created)
}

/// Create opening balance journal entries for accounts.
pub fn create_opening_balance_entries(
    store: &mut EventStore,
    entries: &[(String, String, i64, i32)], // (account_id, account_name, amount_cents, year)
) {
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::events::types::JournalEntrySource;

    // Find or create an "Opening Balances" equity account
    let equity_account_id: String = store
        .connection()
        .query_row(
            "SELECT id FROM accounts WHERE LOWER(name) = 'opening balances' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| {
            let mut acct_commands = AccountCommands::new(store, "system".to_string());
            match acct_commands.create_account(CreateAccountCommand {
                account_type: AccountType::Equity,
                account_number: "3000".to_string(),
                name: "Opening Balances".to_string(),
                parent_id: None,
                currency: None,
                description: Some("Equity account for opening balance entries".to_string()),
            }) {
                Ok(stored) => {
                    if let Event::AccountCreated { account_id, .. } = &stored.event {
                        account_id.clone()
                    } else {
                        Uuid::new_v4().to_string()
                    }
                }
                Err(_) => Uuid::new_v4().to_string(),
            }
        });

    let mut commands = EntryCommands::new(store, "system".to_string());

    for (account_id, account_name, amount_cents, year) in entries {
        let date = chrono::NaiveDate::from_ymd_opt(*year, 1, 1)
            .unwrap_or_else(|| chrono::Utc::now().date_naive());

        let lines = vec![
            EntryLine {
                account_id: account_id.clone(),
                amount: *amount_cents,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
            EntryLine {
                account_id: equity_account_id.clone(),
                amount: -*amount_cents,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
        ];

        let _ = commands.post_entry(PostEntryCommand {
            date,
            memo: format!("Opening balance: {}", account_name),
            lines,
            reference: Some("opening-balance".to_string()),
            source: Some(JournalEntrySource::System),
        });
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
