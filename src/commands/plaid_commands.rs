use crate::events::types::{
    Event, EventEnvelope, JournalEntrySource, JournalLineData, PlaidAccountInfo, StoredEvent,
};
use crate::store::event_store::{EventStore, EventStoreError};
use crate::store::projections::{ProjectionError, Projector};
use chrono::{NaiveDate, Utc};
use std::collections::{HashMap, HashSet};
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

            // Skip if account is not mapped to a local account
            let local_account_id = match mappings.get(&txn.account_id).and_then(|opt| opt.clone()) {
                Some(id) => id,
                None => {
                    skipped += 1;
                    continue;
                }
            };

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

    /// Stage synced transactions for review instead of immediately importing.
    /// Returns (staged_count, skipped_count).
    pub fn stage_transactions(
        &mut self,
        item_id: &str,
        transactions: &[SyncedTransaction],
    ) -> Result<(u32, u32), PlaidCommandError> {
        // Pre-load mappings and do all staging with conn, then drop the borrow
        let (staged, skipped) = {
            let conn = self.store.connection();

            let mut stmt = conn.prepare(
                "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts WHERE item_id = ?1",
            )?;
            let mappings: HashMap<String, Option<String>> = stmt
                .query_map([item_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .filter_map(|r| r.ok())
                .collect();

            let mut staged = 0u32;
            let mut skipped = 0u32;

            for txn in transactions {
                if txn.pending {
                    skipped += 1;
                    continue;
                }

                // Skip if account is not mapped to a local account
                let local_account_id = mappings.get(&txn.account_id).and_then(|o| o.clone());
                if local_account_id.is_none() {
                    skipped += 1;
                    continue;
                }

                let already_exists: bool = conn
                    .query_row(
                        "SELECT 1 FROM plaid_staged_transactions WHERE plaid_transaction_id = ?1
                         UNION ALL
                         SELECT 1 FROM plaid_imported_transactions WHERE plaid_transaction_id = ?1
                         LIMIT 1",
                        [&txn.transaction_id],
                        |_| Ok(true),
                    )
                    .unwrap_or(false);

                if already_exists {
                    skipped += 1;
                    continue;
                }
                let amount_cents = (txn.amount * 100.0).round() as i64;
                let currency = txn.iso_currency_code.as_deref().unwrap_or("USD");
                let id = Uuid::new_v4().to_string();

                conn.execute(
                    "INSERT INTO plaid_staged_transactions
                     (id, item_id, plaid_transaction_id, plaid_account_id, local_account_id,
                      amount_cents, date, name, merchant_name, currency, status)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending')",
                    rusqlite::params![
                        id,
                        item_id,
                        txn.transaction_id,
                        txn.account_id,
                        local_account_id,
                        amount_cents,
                        txn.date,
                        txn.name,
                        txn.merchant_name,
                        currency
                    ],
                )?;
                staged += 1;
            }

            (staged, skipped)
        };
        // Borrow of self.store.connection() is now dropped

        // Record sync event
        let sync_event = Event::PlaidTransactionsSynced {
            item_id: item_id.to_string(),
            transactions_added: staged,
            transactions_modified: 0,
            transactions_removed: 0,
            sync_timestamp: Utc::now().to_rfc3339(),
        };
        let envelope = EventEnvelope::new(sync_event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        // Run transfer detection after staging
        detect_transfers(self.store.connection())?;

        Ok((staged, skipped))
    }

    /// Import a confirmed transfer pair as a single balanced journal entry.
    pub fn import_transfer(
        &mut self,
        candidate_id: &str,
    ) -> Result<StoredEvent, PlaidCommandError> {
        let (txn1, txn2) = {
            let conn = self.store.connection();
            load_transfer_pair(conn, candidate_id)?
        };

        // Plaid amounts are positive when money leaves the account, negative when it arrives.
        // The positive-amount side is the "from" account (money leaving), negative is "to".
        let (from_txn, to_txn) = if txn1.amount_cents > 0 {
            (&txn1, &txn2)
        } else {
            (&txn2, &txn1)
        };

        let date = NaiveDate::parse_from_str(&from_txn.date, "%Y-%m-%d")
            .unwrap_or_else(|_| Utc::now().date_naive());
        let abs_amount = from_txn.amount_cents.unsigned_abs() as i64;
        let entry_id = Uuid::new_v4().to_string();
        let memo = format!(
            "Transfer: {}",
            from_txn.merchant_name.as_deref().unwrap_or(&from_txn.name)
        );

        let from_account = from_txn.local_account_id.as_ref().ok_or_else(|| {
            PlaidCommandError::AccountNotMapped(from_txn.plaid_account_id.clone())
        })?;
        let to_account = to_txn
            .local_account_id
            .as_ref()
            .ok_or_else(|| PlaidCommandError::AccountNotMapped(to_txn.plaid_account_id.clone()))?;

        let lines = vec![
            JournalLineData {
                line_id: format!("{}-line-1", entry_id),
                account_id: from_account.clone(),
                amount: -abs_amount,
                currency: from_txn.currency.clone(),
                exchange_rate: None,
                memo: None,
            },
            JournalLineData {
                line_id: format!("{}-line-2", entry_id),
                account_id: to_account.clone(),
                amount: abs_amount,
                currency: to_txn.currency.clone(),
                exchange_rate: None,
                memo: None,
            },
        ];

        let event = Event::JournalEntryPosted {
            entry_id: entry_id.clone(),
            date,
            memo,
            lines,
            reference: Some(format!(
                "transfer:{}:{}",
                from_txn.plaid_transaction_id, to_txn.plaid_transaction_id
            )),
            source: Some(JournalEntrySource::Plaid),
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        // Record both in plaid_imported_transactions for dedup
        let conn = self.store.connection();
        conn.execute(
            "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![from_txn.plaid_transaction_id, from_txn.item_id, entry_id],
        )?;
        conn.execute(
            "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![to_txn.plaid_transaction_id, to_txn.item_id, entry_id],
        )?;

        // Update statuses
        conn.execute(
            "UPDATE plaid_staged_transactions SET status = 'imported' WHERE id IN (?1, ?2)",
            rusqlite::params![txn1.id, txn2.id],
        )?;
        conn.execute(
            "UPDATE plaid_transfer_candidates SET status = 'confirmed' WHERE id = ?1",
            [candidate_id],
        )?;

        Ok(stored)
    }

    /// Import a single unmatched staged transaction with Uncategorized counterpart.
    pub fn import_single_staged(
        &mut self,
        staged_txn_id: &str,
    ) -> Result<StoredEvent, PlaidCommandError> {
        let txn = {
            let conn = self.store.connection();
            load_staged_transaction(conn, staged_txn_id)?
        };

        let uncategorized_id = {
            let conn = self.store.connection();
            find_or_create_uncategorized(conn)?
        };

        let local_account_id = txn
            .local_account_id
            .clone()
            .unwrap_or_else(|| uncategorized_id.clone());

        let date = NaiveDate::parse_from_str(&txn.date, "%Y-%m-%d")
            .unwrap_or_else(|_| Utc::now().date_naive());
        let entry_id = Uuid::new_v4().to_string();
        let memo = txn
            .merchant_name
            .as_deref()
            .unwrap_or(&txn.name)
            .to_string();

        let lines = vec![
            JournalLineData {
                line_id: format!("{}-line-1", entry_id),
                account_id: local_account_id,
                amount: -txn.amount_cents,
                currency: txn.currency.clone(),
                exchange_rate: None,
                memo: None,
            },
            JournalLineData {
                line_id: format!("{}-line-2", entry_id),
                account_id: uncategorized_id,
                amount: txn.amount_cents,
                currency: txn.currency.clone(),
                exchange_rate: None,
                memo: None,
            },
        ];

        let event = Event::JournalEntryPosted {
            entry_id: entry_id.clone(),
            date,
            memo,
            lines,
            reference: Some(txn.plaid_transaction_id.clone()),
            source: Some(JournalEntrySource::Plaid),
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        let projector = Projector::new(self.store.connection());
        projector.apply(&stored)?;

        let conn = self.store.connection();
        conn.execute(
            "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![txn.plaid_transaction_id, txn.item_id, entry_id],
        )?;
        conn.execute(
            "UPDATE plaid_staged_transactions SET status = 'imported' WHERE id = ?1",
            [staged_txn_id],
        )?;

        Ok(stored)
    }

    /// Import all: confirm all pending transfer candidates, then import remaining unmatched.
    /// Returns (transfers_imported, unmatched_imported).
    pub fn import_all_staged(&mut self) -> Result<(u32, u32), PlaidCommandError> {
        // Collect pending transfer candidate IDs
        let candidate_ids: Vec<String> = {
            let conn = self.store.connection();
            let mut stmt =
                conn.prepare("SELECT id FROM plaid_transfer_candidates WHERE status = 'pending'")?;
            let ids: Vec<String> = stmt
                .query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };

        let mut transfers = 0u32;
        for cid in &candidate_ids {
            self.import_transfer(cid)?;
            transfers += 1;
        }

        // Collect remaining pending staged transaction IDs
        let pending_ids: Vec<String> = {
            let conn = self.store.connection();
            let mut stmt =
                conn.prepare("SELECT id FROM plaid_staged_transactions WHERE status = 'pending'")?;
            let ids: Vec<String> = stmt
                .query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();
            ids
        };

        let mut unmatched = 0u32;
        for sid in &pending_ids {
            self.import_single_staged(sid)?;
            unmatched += 1;
        }

        Ok((transfers, unmatched))
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

/// A staged Plaid transaction awaiting review/import.
#[derive(Debug, Clone)]
pub struct StagedTransaction {
    pub id: String,
    pub item_id: String,
    pub plaid_transaction_id: String,
    pub plaid_account_id: String,
    pub local_account_id: Option<String>,
    pub amount_cents: i64,
    pub date: String,
    pub name: String,
    pub merchant_name: Option<String>,
    pub currency: String,
    pub status: String,
}

/// A detected transfer candidate pair for display.
#[derive(Debug, Clone)]
pub struct TransferCandidate {
    pub id: String,
    pub txn1: StagedTransaction,
    pub txn2: StagedTransaction,
    pub confidence: f64,
    pub status: String,
}

/// Detect transfer pairs among pending staged transactions.
/// Matches transactions with equal-and-opposite amounts, within 3 days,
/// from different local accounts.
pub fn detect_transfers(conn: &rusqlite::Connection) -> Result<u32, PlaidCommandError> {
    // Clear previous unconfirmed candidates
    conn.execute(
        "DELETE FROM plaid_transfer_candidates WHERE status = 'pending'",
        [],
    )?;

    // Reset previously matched-but-not-confirmed staged txns back to pending
    conn.execute(
        "UPDATE plaid_staged_transactions SET status = 'pending' WHERE status = 'matched'",
        [],
    )?;

    // Find pairs: equal-and-opposite amounts, within 3 days, different mapped accounts
    let mut stmt = conn.prepare(
        "SELECT t1.id, t2.id,
                ABS(julianday(t1.date) - julianday(t2.date)) as date_diff
         FROM plaid_staged_transactions t1
         JOIN plaid_staged_transactions t2
           ON t1.amount_cents = -t2.amount_cents
           AND t1.amount_cents != 0
           AND t1.id < t2.id
           AND t1.local_account_id IS NOT NULL
           AND t2.local_account_id IS NOT NULL
           AND t1.local_account_id != t2.local_account_id
           AND ABS(julianday(t1.date) - julianday(t2.date)) <= 3
         WHERE t1.status = 'pending' AND t2.status = 'pending'
         ORDER BY date_diff ASC",
    )?;

    let candidates: Vec<(String, String, f64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .filter_map(|r| r.ok())
        .collect();

    let mut matched_ids: HashSet<String> = HashSet::new();
    let mut count = 0u32;

    for (id1, id2, date_diff) in candidates {
        if matched_ids.contains(&id1) || matched_ids.contains(&id2) {
            continue;
        }

        // Confidence: 1.0 for same day, decreasing for further apart
        let confidence = 1.0 - (date_diff / 4.0);

        let candidate_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO plaid_transfer_candidates (id, staged_txn_id_1, staged_txn_id_2, confidence, status)
             VALUES (?1, ?2, ?3, ?4, 'pending')",
            rusqlite::params![candidate_id, id1, id2, confidence],
        )?;

        conn.execute(
            "UPDATE plaid_staged_transactions SET status = 'matched' WHERE id = ?1 OR id = ?2",
            rusqlite::params![id1, id2],
        )?;

        matched_ids.insert(id1);
        matched_ids.insert(id2);
        count += 1;
    }

    Ok(count)
}

/// Reject a transfer candidate, unlinking the pair back to pending.
pub fn reject_transfer(
    conn: &rusqlite::Connection,
    candidate_id: &str,
) -> Result<(), PlaidCommandError> {
    // Reset the two staged transactions back to pending
    conn.execute(
        "UPDATE plaid_staged_transactions SET status = 'pending'
         WHERE id IN (SELECT staged_txn_id_1 FROM plaid_transfer_candidates WHERE id = ?1
                      UNION SELECT staged_txn_id_2 FROM plaid_transfer_candidates WHERE id = ?1)",
        [candidate_id],
    )?;
    conn.execute(
        "UPDATE plaid_transfer_candidates SET status = 'rejected' WHERE id = ?1",
        [candidate_id],
    )?;
    Ok(())
}

/// Load a transfer candidate pair from the database.
fn load_transfer_pair(
    conn: &rusqlite::Connection,
    candidate_id: &str,
) -> Result<(StagedTransaction, StagedTransaction), PlaidCommandError> {
    let (id1, id2): (String, String) = conn.query_row(
        "SELECT staged_txn_id_1, staged_txn_id_2 FROM plaid_transfer_candidates WHERE id = ?1",
        [candidate_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let txn1 = load_staged_transaction(conn, &id1)?;
    let txn2 = load_staged_transaction(conn, &id2)?;
    Ok((txn1, txn2))
}

/// Load a single staged transaction by ID.
fn load_staged_transaction(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<StagedTransaction, PlaidCommandError> {
    conn.query_row(
        "SELECT id, item_id, plaid_transaction_id, plaid_account_id, local_account_id,
                amount_cents, date, name, merchant_name, currency, status
         FROM plaid_staged_transactions WHERE id = ?1",
        [id],
        |row| {
            Ok(StagedTransaction {
                id: row.get(0)?,
                item_id: row.get(1)?,
                plaid_transaction_id: row.get(2)?,
                plaid_account_id: row.get(3)?,
                local_account_id: row.get(4)?,
                amount_cents: row.get(5)?,
                date: row.get(6)?,
                name: row.get(7)?,
                merchant_name: row.get(8)?,
                currency: row.get(9)?,
                status: row.get(10)?,
            })
        },
    )
    .map_err(PlaidCommandError::from)
}

/// Load all pending transfer candidates with their transaction details.
pub fn load_pending_transfers(
    conn: &rusqlite::Connection,
) -> Result<Vec<TransferCandidate>, PlaidCommandError> {
    let mut stmt = conn.prepare(
        "SELECT tc.id, tc.confidence, tc.status,
                tc.staged_txn_id_1, tc.staged_txn_id_2
         FROM plaid_transfer_candidates tc
         WHERE tc.status = 'pending'
         ORDER BY tc.confidence DESC",
    )?;

    let rows: Vec<(String, f64, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut candidates = Vec::new();
    for (id, confidence, status, txn_id_1, txn_id_2) in rows {
        let txn1 = load_staged_transaction(conn, &txn_id_1)?;
        let txn2 = load_staged_transaction(conn, &txn_id_2)?;
        candidates.push(TransferCandidate {
            id,
            txn1,
            txn2,
            confidence,
            status,
        });
    }

    Ok(candidates)
}

/// Load all pending (unmatched) staged transactions.
pub fn load_pending_staged(
    conn: &rusqlite::Connection,
) -> Result<Vec<StagedTransaction>, PlaidCommandError> {
    let mut stmt = conn.prepare(
        "SELECT id, item_id, plaid_transaction_id, plaid_account_id, local_account_id,
                amount_cents, date, name, merchant_name, currency, status
         FROM plaid_staged_transactions
         WHERE status = 'pending'
         ORDER BY date DESC",
    )?;

    let txns = stmt
        .query_map([], |row| {
            Ok(StagedTransaction {
                id: row.get(0)?,
                item_id: row.get(1)?,
                plaid_transaction_id: row.get(2)?,
                plaid_account_id: row.get(3)?,
                local_account_id: row.get(4)?,
                amount_cents: row.get(5)?,
                date: row.get(6)?,
                name: row.get(7)?,
                merchant_name: row.get(8)?,
                currency: row.get(9)?,
                status: row.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(txns)
}

/// Get counts of pending staged transactions and transfer candidates.
pub fn staged_counts(conn: &rusqlite::Connection) -> Result<(u32, u32), PlaidCommandError> {
    let staged: u32 = conn.query_row(
        "SELECT COUNT(*) FROM plaid_staged_transactions WHERE status IN ('pending', 'matched')",
        [],
        |row| row.get(0),
    )?;
    let transfers: u32 = conn.query_row(
        "SELECT COUNT(*) FROM plaid_transfer_candidates WHERE status = 'pending'",
        [],
        |row| row.get(0),
    )?;
    Ok((staged, transfers))
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
