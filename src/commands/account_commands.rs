use crate::domain::AccountType;
use crate::events::types::{Event, EventAccountType, EventEnvelope, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use rusqlite::OptionalExtension;
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

/// Outcome of an account command's in-txn validation: the invariants held (append
/// this event) or a domain invariant was violated (reject). Mirrors
/// [`crate::commands::entry_commands::PostEntryStep`]. The caller wraps the event
/// in an envelope, stamping identity as appropriate (local `user_id` vs. the
/// server-authenticated actor on the sync path).
pub(crate) enum AccountStep {
    /// All invariants hold under the write lock; append this event.
    Append(Event),
    /// A domain invariant was violated.
    Reject(AccountCommandError),
}

/// Run `create_account`'s state-dependent invariant inside the append
/// transaction — the account-number uniqueness check — and, if it holds, build
/// the `AccountCreated` event. Shared by [`AccountCommands::create_account`] and
/// the server-side sync submit path so both enforce the SAME uniqueness fence
/// under the write lock (the read-then-append TOCTOU on the HIGH-risk
/// `AccountCreated` variant, SPEC §6.2 / invariant audit #4).
pub(crate) fn build_create_account_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &CreateAccountCommand,
) -> Result<AccountStep, EventStoreError> {
    // Uniqueness: no existing account already holds this number.
    let exists: bool = tx
        .query_row(
            "SELECT 1 FROM accounts WHERE account_number = ?1",
            [&cmd.account_number],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if exists {
        return Ok(AccountStep::Reject(AccountCommandError::AlreadyExists(
            cmd.account_number.clone(),
        )));
    }

    let event = Event::AccountCreated {
        account_id: Uuid::new_v4().to_string(),
        account_type: EventAccountType::from(cmd.account_type),
        account_number: cmd.account_number.clone(),
        name: cmd.name.clone(),
        parent_id: cmd.parent_id.clone(),
        currency: cmd.currency.clone(),
        description: cmd.description.clone(),
    };
    Ok(AccountStep::Append(event))
}

/// Run `deactivate_account`'s state-dependent invariants inside the append
/// transaction — the account is active AND has a zero net balance — and, if they
/// hold, build the `AccountDeactivated` event. Shared by
/// [`AccountCommands::deactivate_account`] and the server-side sync submit path
/// so both enforce the SAME fences under the write lock (audit
/// `AccountDeactivated`, HIGH — a concurrent posting must not sneak a nonzero
/// balance in after the check).
pub(crate) fn build_deactivate_account_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &DeactivateAccountCommand,
) -> Result<AccountStep, EventStoreError> {
    let is_active: Option<bool> = tx
        .query_row(
            "SELECT is_active = 1 FROM accounts WHERE id = ?1",
            [&cmd.account_id],
            |row| row.get(0),
        )
        .optional()?;
    match is_active {
        None => {
            return Ok(AccountStep::Reject(AccountCommandError::NotFound(
                cmd.account_id.clone(),
            )))
        }
        Some(false) => {
            return Ok(AccountStep::Reject(AccountCommandError::InvalidData(
                "Account is already inactive".to_string(),
            )))
        }
        Some(true) => {}
    }

    let balance: i64 = tx
        .query_row(
            "SELECT COALESCE(SUM(jl.amount), 0)
             FROM journal_lines jl
             JOIN journal_entries je ON jl.entry_id = je.id
             WHERE jl.account_id = ?1 AND je.is_void = 0",
            [&cmd.account_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if balance != 0 {
        return Ok(AccountStep::Reject(AccountCommandError::HasBalance));
    }

    let event = Event::AccountDeactivated {
        account_id: cmd.account_id.clone(),
        reason: cmd.reason.clone(),
    };
    Ok(AccountStep::Append(event))
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

    /// Create a new account.
    ///
    /// The account-number uniqueness check runs *inside* the append transaction
    /// via [`EventStore::append_checked`], so two concurrent creates can't both
    /// pass the duplicate check and then both append the same number (the
    /// read-then-append TOCTOU on the HIGH-risk `AccountCreated` variant, SPEC
    /// §6.2 / invariant audit #4). On a head move we retry against fresh state.
    pub fn create_account(
        &mut self,
        cmd: CreateAccountCommand,
    ) -> Result<StoredEvent, AccountCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_create_account_in_txn(tx, &cmd)? {
                    AccountStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    AccountStep::Reject(e) => Ok(Verdict::Reject(e)),
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

    /// Update an existing account.
    ///
    /// Emits one `AccountUpdated` event per changed field (number, name, parent,
    /// description) — a batch that must land atomically — via
    /// [`EventStore::append_checked_many`]. The account-number rename uniqueness
    /// check runs inside the append transaction (audit `AccountUpdated`), so a
    /// concurrent create/rename can't collide the number. Retries on a head move.
    /// Returns the events for the fields that actually changed (empty if none).
    pub fn update_account(
        &mut self,
        cmd: UpdateAccountCommand,
    ) -> Result<Vec<StoredEvent>, AccountCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked_many(
                head,
                |tx| {
                    let exists: bool = tx
                        .query_row(
                            "SELECT 1 FROM accounts WHERE id = ?1",
                            [&cmd.account_id],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if !exists {
                        return Ok(Verdict::Reject(AccountCommandError::NotFound(
                            cmd.account_id.clone(),
                        )));
                    }

                    let mut envelopes = Vec::new();

                    // account_number — with rename-uniqueness check
                    if let Some(new_number) = &cmd.account_number {
                        let old_number: String = tx.query_row(
                            "SELECT account_number FROM accounts WHERE id = ?1",
                            [&cmd.account_id],
                            |row| row.get(0),
                        )?;
                        if &old_number != new_number {
                            let duplicate: bool = tx
                                .query_row(
                                    "SELECT 1 FROM accounts WHERE account_number = ?1 AND id != ?2",
                                    [new_number, &cmd.account_id],
                                    |_| Ok(true),
                                )
                                .optional()?
                                .unwrap_or(false);
                            if duplicate {
                                return Ok(Verdict::Reject(AccountCommandError::AlreadyExists(
                                    new_number.clone(),
                                )));
                            }
                            envelopes.push(EventEnvelope::new(
                                Event::AccountUpdated {
                                    account_id: cmd.account_id.clone(),
                                    field: "account_number".to_string(),
                                    old_value: old_number,
                                    new_value: new_number.clone(),
                                },
                                user_id.clone(),
                            ));
                        }
                    }

                    // name
                    if let Some(new_name) = &cmd.name {
                        let old_name: String = tx.query_row(
                            "SELECT name FROM accounts WHERE id = ?1",
                            [&cmd.account_id],
                            |row| row.get(0),
                        )?;
                        if &old_name != new_name {
                            envelopes.push(EventEnvelope::new(
                                Event::AccountUpdated {
                                    account_id: cmd.account_id.clone(),
                                    field: "name".to_string(),
                                    old_value: old_name,
                                    new_value: new_name.clone(),
                                },
                                user_id.clone(),
                            ));
                        }
                    }

                    // parent_id (Option<Option<String>>: Some(x) => set to x)
                    if let Some(new_parent) = &cmd.parent_id {
                        let old_parent: Option<String> = tx.query_row(
                            "SELECT parent_id FROM accounts WHERE id = ?1",
                            [&cmd.account_id],
                            |row| row.get(0),
                        )?;
                        let old_parent_str = old_parent.unwrap_or_default();
                        let new_parent_str = new_parent.clone().unwrap_or_default();
                        if old_parent_str != new_parent_str {
                            envelopes.push(EventEnvelope::new(
                                Event::AccountUpdated {
                                    account_id: cmd.account_id.clone(),
                                    field: "parent_id".to_string(),
                                    old_value: old_parent_str,
                                    new_value: new_parent_str,
                                },
                                user_id.clone(),
                            ));
                        }
                    }

                    // description
                    if let Some(new_desc) = &cmd.description {
                        let old_desc: Option<String> = tx.query_row(
                            "SELECT description FROM accounts WHERE id = ?1",
                            [&cmd.account_id],
                            |row| row.get(0),
                        )?;
                        let old_desc_str = old_desc.unwrap_or_default();
                        if &old_desc_str != new_desc {
                            envelopes.push(EventEnvelope::new(
                                Event::AccountUpdated {
                                    account_id: cmd.account_id.clone(),
                                    field: "description".to_string(),
                                    old_value: old_desc_str,
                                    new_value: new_desc.clone(),
                                },
                                user_id.clone(),
                            ));
                        }
                    }

                    Ok(Verdict::Append(envelopes))
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                CheckedOutcome::Appended(events) => return Ok(events),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Deactivate an account.
    ///
    /// Re-checks, inside the append transaction, that the account is active and
    /// has a zero net balance (audit `AccountDeactivated`, HIGH — a concurrent
    /// posting must not sneak a nonzero balance in after the check). Retries on a
    /// head move.
    pub fn deactivate_account(
        &mut self,
        cmd: DeactivateAccountCommand,
    ) -> Result<StoredEvent, AccountCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_deactivate_account_in_txn(tx, &cmd)? {
                    AccountStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    AccountStep::Reject(e) => Ok(Verdict::Reject(e)),
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

    /// Reactivate an account.
    ///
    /// Re-checks the "exists and is currently inactive" guard inside the append
    /// transaction. Retries on a head move.
    pub fn reactivate_account(
        &mut self,
        cmd: ReactivateAccountCommand,
    ) -> Result<StoredEvent, AccountCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    let is_active: Option<bool> = tx
                        .query_row(
                            "SELECT is_active = 1 FROM accounts WHERE id = ?1",
                            [&cmd.account_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match is_active {
                        None => {
                            return Ok(Verdict::Reject(AccountCommandError::NotFound(
                                cmd.account_id.clone(),
                            )))
                        }
                        Some(true) => {
                            return Ok(Verdict::Reject(AccountCommandError::InvalidData(
                                "Account is already active".to_string(),
                            )))
                        }
                        Some(false) => {}
                    }

                    let event = Event::AccountReactivated {
                        account_id: cmd.account_id.clone(),
                    };
                    Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
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
///
/// The singleton invariant (at most one `company` row, keyed `id = 'default'`) is
/// enforced *inside* the append transaction via [`EventStore::append_checked`]:
/// the "does a company already exist" check runs under the write lock, so two
/// concurrent bootstraps can't both pass the check and both append a
/// `CompanyCreated` (the projection uses `INSERT OR REPLACE` and would otherwise
/// silently clobber). If a concurrent writer wins the race the loser is rejected
/// and this returns `None` (idempotent no-op), matching the pre-existing
/// "already have a company" fast path. On a head move we retry against fresh
/// state.
pub fn ensure_company(store: &mut EventStore, db_path: &std::path::Path) -> Option<String> {
    // Fast path: a company already exists ⇒ nothing to do. Correctness does not
    // rely on this read (the in-txn check below is authoritative); it just skips
    // the append loop in the common case.
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

    loop {
        let head = match store.latest_id() {
            Ok(h) => h.unwrap_or(0),
            Err(e) => return Some(format!("Failed to create company: {}", e)),
        };
        let outcome = store.append_checked(
            head,
            |tx| {
                // Singleton: a company row already exists ⇒ reject (treated as an
                // idempotent no-op by the caller). Checked under the write lock so
                // a concurrent bootstrap can't slip in between check and append.
                let exists: bool = tx
                    .query_row(
                        "SELECT 1 FROM company WHERE id = 'default'",
                        [],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false);
                if exists {
                    return Ok(Verdict::Reject(AccountCommandError::AlreadyExists(
                        "company".to_string(),
                    )));
                }

                let event = Event::CompanyCreated {
                    company_id: Uuid::new_v4().to_string(),
                    name: company_name.clone(),
                    base_currency: "USD".to_string(),
                    fiscal_year_start: 1,
                };
                Ok(Verdict::Append(EventEnvelope::new(event, "system".to_string())))
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        );

        match outcome {
            Ok(CheckedOutcome::Appended(_)) => {
                return Some(format!("Company '{}' created for sync", company_name))
            }
            Ok(CheckedOutcome::HeadMismatch { .. }) => continue, // refetch & retry
            // A concurrent writer created the company first: idempotent no-op.
            Ok(CheckedOutcome::Rejected(_)) => return None,
            Err(e) => return Some(format!("Failed to create company: {}", e)),
        }
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

        // Per-account reference (not a shared "opening-balance" literal): each
        // account gets exactly one opening-balance entry, so this doubles as an
        // idempotency key and keeps the reference unique (idx_journal_entries_
        // reference_unique). Re-running for the same account is a no-op.
        let _ = commands.post_entry(PostEntryCommand {
            date,
            memo: format!("Opening balance: {}", account_name),
            lines,
            reference: Some(format!("opening-balance:{}", account_id)),
            source: Some(JournalEntrySource::System),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations::SchemaStore;

    fn setup() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        store.init_schema().unwrap();
        store
    }

    #[test]
    fn concurrent_create_same_number_only_one_wins() {
        // The AccountCreated uniqueness TOCTOU (audit #4), exercised across TWO
        // connections — the UI + in-process-sync-server topology the WAL setup is
        // built for. Because the dup-check AND the projection now commit in one
        // transaction (append_checked), exactly one create lands and the other
        // observes the committed `accounts` row and is rejected AlreadyExists.
        // Before projections were folded into the txn this raced (both could pass
        // the check against a not-yet-projected log and append a duplicate).
        let dir = std::env::temp_dir().join(format!("accountir-acct-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("log.db");
        {
            let mut store = EventStore::open(&db).unwrap();
            store.init_schema().unwrap();
        }

        // A barrier lines both threads up at the create so they genuinely
        // contend at the critical section (rather than one finishing first).
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let worker = |tag: &'static str,
                      path: std::path::PathBuf,
                      barrier: std::sync::Arc<std::sync::Barrier>| {
            move || {
                let mut store = EventStore::open(&path).unwrap();
                let mut commands = AccountCommands::new(&mut store, tag.to_string());
                barrier.wait();
                commands.create_account(CreateAccountCommand {
                    account_type: AccountType::Asset,
                    account_number: "1000".to_string(),
                    name: format!("Cash {tag}"),
                    parent_id: None,
                    currency: None,
                    description: None,
                })
            }
        };

        let t1 = std::thread::spawn(worker("t1", db.clone(), barrier.clone()));
        let t2 = std::thread::spawn(worker("t2", db.clone(), barrier.clone()));
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(oks, 1, "exactly one create must win (r1={r1:?}, r2={r2:?})");
        for r in [&r1, &r2] {
            if let Err(e) = r {
                assert!(
                    matches!(e, AccountCommandError::AlreadyExists(_)),
                    "the loser must be rejected AlreadyExists, got {e:?}"
                );
            }
        }

        // Exactly one account row and one event landed — no duplicate.
        let store = EventStore::open(&db).unwrap();
        let rows: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE account_number = '1000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1, "exactly one account with number 1000");
        assert_eq!(
            store.count().unwrap(),
            1,
            "exactly one AccountCreated event"
        );
        std::fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn deactivate_rejected_when_account_has_balance() {
        use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
        let mut store = setup();

        let mk = |store: &mut EventStore, num: &str, ty: AccountType| {
            let e = AccountCommands::new(store, "u".to_string())
                .create_account(CreateAccountCommand {
                    account_type: ty,
                    account_number: num.to_string(),
                    name: format!("A{num}"),
                    parent_id: None,
                    currency: None,
                    description: None,
                })
                .unwrap();
            match e.event {
                Event::AccountCreated { account_id, .. } => account_id,
                _ => panic!("expected AccountCreated"),
            }
        };
        let cash = mk(&mut store, "1000", AccountType::Asset);
        let equity = mk(&mut store, "3000", AccountType::Equity);

        // Give cash a nonzero balance.
        EntryCommands::new(&mut store, "u".to_string())
            .post_entry(PostEntryCommand {
                date: chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                memo: "opening".to_string(),
                lines: vec![
                    EntryLine::debit(&cash, 5000, "USD"),
                    EntryLine::credit(&equity, 5000, "USD"),
                ],
                reference: None,
                source: None,
            })
            .unwrap();

        let err = AccountCommands::new(&mut store, "u".to_string())
            .deactivate_account(DeactivateAccountCommand {
                account_id: cash,
                reason: None,
            })
            .unwrap_err();
        assert!(matches!(err, AccountCommandError::HasBalance));
    }

    #[test]
    fn update_account_rename_to_existing_number_rejected() {
        let mut store = setup();
        let mk = |store: &mut EventStore, num: &str| {
            let e = AccountCommands::new(store, "u".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Asset,
                    account_number: num.to_string(),
                    name: format!("A{num}"),
                    parent_id: None,
                    currency: None,
                    description: None,
                })
                .unwrap();
            match e.event {
                Event::AccountCreated { account_id, .. } => account_id,
                _ => panic!("expected AccountCreated"),
            }
        };
        let _a = mk(&mut store, "1000");
        let b = mk(&mut store, "2000");

        // Renaming b to 1000 collides with a — rejected in-txn.
        let err = AccountCommands::new(&mut store, "u".to_string())
            .update_account(UpdateAccountCommand {
                account_id: b,
                account_number: Some("1000".to_string()),
                name: None,
                parent_id: None,
                description: None,
            })
            .unwrap_err();
        assert!(matches!(err, AccountCommandError::AlreadyExists(_)));
    }

    #[test]
    fn ensure_company_creates_once_then_is_a_noop() {
        let mut store = setup();
        let path = std::path::Path::new("/tmp/Acme.db");

        // First call creates the singleton company row.
        let msg = ensure_company(&mut store, path);
        assert!(msg.is_some(), "first ensure_company should create a company");
        let rows: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM company", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        let after_create = store.count().unwrap();

        // Second call is an idempotent no-op: no message, no new event, still one row.
        let msg2 = ensure_company(&mut store, path);
        assert!(msg2.is_none(), "second ensure_company must be a no-op");
        let rows2: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM company", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows2, 1, "still exactly one company row");
        assert_eq!(
            store.count().unwrap(),
            after_create,
            "a no-op ensure_company appends nothing"
        );
    }
}
