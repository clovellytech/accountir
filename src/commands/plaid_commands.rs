use crate::events::types::{
    Event, EventEnvelope, JournalEntrySource, JournalLineData, PlaidAccountInfo, StoredEvent,
};
use crate::store::event_store::{EventStore, EventStoreError};
use crate::store::projections::{ProjectionError, Projector};
use chrono::{NaiveDate, Utc};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum PlaidCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Projection error: {0}")]
    ProjectionError(#[from] ProjectionError),
    #[error("Item not found: {0}")]
    ItemNotFound(String),
    #[error("Account not mapped: {0}")]
    AccountNotMapped(String),
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
}

pub struct PlaidCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> PlaidCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Record a newly connected Plaid item
    pub fn connect_item(
        &mut self,
        proxy_item_id: &str,
        institution_name: &str,
        accounts: Vec<PlaidAccountInfo>,
    ) -> Result<StoredEvent, PlaidCommandError> {
        let item_id = Uuid::new_v4().to_string();

        let event = Event::PlaidItemConnected {
            item_id,
            proxy_item_id: proxy_item_id.to_string(),
            institution_name: institution_name.to_string(),
            plaid_accounts: accounts,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;
        Ok(stored)
    }

    /// Map a Plaid account to a local account
    pub fn map_account(
        &mut self,
        item_id: &str,
        plaid_account_id: &str,
        local_account_id: &str,
    ) -> Result<StoredEvent, PlaidCommandError> {
        // Verify item exists
        let exists: bool = self
            .store
            .connection()
            .query_row("SELECT 1 FROM plaid_items WHERE id = ?1", [item_id], |_| {
                Ok(true)
            })
            .unwrap_or(false);

        if !exists {
            return Err(PlaidCommandError::ItemNotFound(item_id.to_string()));
        }

        let event = Event::PlaidAccountMapped {
            item_id: item_id.to_string(),
            plaid_account_id: plaid_account_id.to_string(),
            local_account_id: local_account_id.to_string(),
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;
        Ok(stored)
    }

    /// Unmap a Plaid account from a local account
    pub fn unmap_account(
        &mut self,
        item_id: &str,
        plaid_account_id: &str,
        local_account_id: &str,
    ) -> Result<StoredEvent, PlaidCommandError> {
        // Verify item exists and mapping exists
        let mapped: bool = self
            .store
            .connection()
            .query_row(
                "SELECT 1 FROM plaid_local_accounts WHERE item_id = ?1 AND plaid_account_id = ?2 AND local_account_id = ?3",
                rusqlite::params![item_id, plaid_account_id, local_account_id],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !mapped {
            return Err(PlaidCommandError::AccountNotMapped(format!(
                "{}:{}",
                item_id, plaid_account_id
            )));
        }

        let event = Event::PlaidAccountUnmapped {
            item_id: item_id.to_string(),
            plaid_account_id: plaid_account_id.to_string(),
            local_account_id: local_account_id.to_string(),
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;
        Ok(stored)
    }

    /// Import synced transactions from the proxy into journal entries.
    /// Returns (added_count, skipped_count).
    pub fn import_transactions(
        &mut self,
        item_id: &str,
        transactions: &[SyncedTransaction],
    ) -> Result<(u32, u32), PlaidCommandError> {
        // Pre-load all data we need into owned values, then drop the borrows
        let (mappings, uncategorized_id, already_imported_set) = {
            let conn = self.store.connection();

            let mut stmt = conn.prepare(
                "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts WHERE item_id = ?1",
            )?;
            let mappings: std::collections::HashMap<String, Option<String>> = stmt
                .query_map([item_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .filter_map(|r| r.ok())
                .collect();

            let uncategorized_id = find_or_create_uncategorized(conn)?;

            // Pre-check all transaction IDs for dedup
            let mut already_imported = std::collections::HashSet::new();
            for txn in transactions {
                let exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM plaid_imported_transactions WHERE plaid_transaction_id = ?1",
                        [&txn.transaction_id],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);
                if exists {
                    already_imported.insert(txn.transaction_id.clone());
                }
            }

            (mappings, uncategorized_id, already_imported)
        };
        // All borrows of self.store.connection() are now dropped

        let mut added = 0u32;
        let mut skipped = 0u32;

        for txn in transactions {
            if already_imported_set.contains(&txn.transaction_id) {
                skipped += 1;
                continue;
            }

            let local_account_id = mappings
                .get(&txn.account_id)
                .and_then(|opt| opt.clone())
                .unwrap_or_else(|| uncategorized_id.clone());

            let date = NaiveDate::parse_from_str(&txn.date, "%Y-%m-%d")
                .unwrap_or_else(|_| Utc::now().date_naive());

            let amount_cents = (txn.amount * 100.0).round() as i64;

            let entry_id = Uuid::new_v4().to_string();
            let memo = txn
                .merchant_name
                .as_deref()
                .unwrap_or(&txn.name)
                .to_string();

            let lines = vec![
                JournalLineData {
                    line_id: format!("{}-line-1", entry_id),
                    account_id: local_account_id.clone(),
                    amount: -amount_cents,
                    currency: txn.currency.clone().unwrap_or_else(|| "USD".to_string()),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: format!("{}-line-2", entry_id),
                    account_id: uncategorized_id.clone(),
                    amount: amount_cents,
                    currency: txn.currency.clone().unwrap_or_else(|| "USD".to_string()),
                    exchange_rate: None,
                    memo: None,
                },
            ];

            let event = Event::JournalEntryPosted {
                entry_id: entry_id.clone(),
                date,
                memo,
                lines,
                reference: Some(txn.transaction_id.clone()),
                source: Some(JournalEntrySource::Plaid),
            };

            let envelope = EventEnvelope::new(event, self.user_id.clone());
            let stored = self.store.append(envelope)?;
            let projector = Projector::new(self.store.connection());
            projector.apply(&stored)?;

            // Record for dedup
            self.store.connection().execute(
                "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![txn.transaction_id, item_id, entry_id],
            )?;

            added += 1;
        }

        // Record sync event
        let sync_event = Event::PlaidTransactionsSynced {
            item_id: item_id.to_string(),
            transactions_added: added,
            transactions_modified: 0,
            transactions_removed: 0,
            sync_timestamp: Utc::now().to_rfc3339(),
        };
        let envelope = EventEnvelope::new(sync_event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        Ok((added, skipped))
    }

    /// Disconnect a Plaid item
    pub fn disconnect_item(
        &mut self,
        item_id: &str,
        reason: &str,
    ) -> Result<StoredEvent, PlaidCommandError> {
        let exists: bool = self
            .store
            .connection()
            .query_row("SELECT 1 FROM plaid_items WHERE id = ?1", [item_id], |_| {
                Ok(true)
            })
            .unwrap_or(false);

        if !exists {
            return Err(PlaidCommandError::ItemNotFound(item_id.to_string()));
        }

        let event = Event::PlaidItemDisconnected {
            item_id: item_id.to_string(),
            reason: reason.to_string(),
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;
        Ok(stored)
    }
}

/// A transaction received from the proxy's sync endpoint
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SyncedTransaction {
    pub transaction_id: String,
    pub account_id: String,
    pub amount: f64,
    pub date: String,
    pub name: String,
    pub merchant_name: Option<String>,
    pub pending: bool,
    pub iso_currency_code: Option<String>,
    #[serde(skip)]
    pub currency: Option<String>,
}

fn find_or_create_uncategorized(conn: &rusqlite::Connection) -> Result<String, PlaidCommandError> {
    // Check if "Uncategorized" account already exists
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM accounts WHERE name = 'Uncategorized' AND is_active = 1",
            [],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        return Ok(id);
    }

    // Find next available account number in 9000 range
    let max_number: Option<String> = conn
        .query_row(
            "SELECT MAX(account_number) FROM accounts WHERE account_number LIKE '9%'",
            [],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    let next_number = match max_number {
        Some(n) => {
            let num: u32 = n.parse().unwrap_or(8999);
            format!("{}", num + 1)
        }
        None => "9000".to_string(),
    };

    let account_id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO accounts (id, account_type, account_number, name, is_active) VALUES (?1, 'expense', ?2, 'Uncategorized', 1)",
        rusqlite::params![account_id, next_number],
    )?;

    Ok(account_id)
}
