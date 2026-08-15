use crate::commands::entry_commands::{check_entry_invariants_in_txn, check_reference_free_in_txn};
use crate::events::types::{
    Event, EventEnvelope, JournalEntrySource, JournalLineData, PlaidAccountInfo, StoredEvent,
};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::{ProjectionError, ProjectionStore, Projector};
use chrono::{NaiveDate, Utc};
use rusqlite::OptionalExtension;
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
    #[error("Invalid transfer: {0}")]
    InvalidTransfer(String),
    #[error("Transaction already imported: {0}")]
    AlreadyImported(String),
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Account error: {0}")]
    AccountError(#[from] crate::commands::account_commands::AccountCommandError),
    #[error("Entry invariant: {0}")]
    EntryError(#[from] crate::commands::entry_commands::EntryCommandError),
}

/// Outcome of a Plaid command's in-txn validation. Mirrors `AccountStep`: the
/// caller wraps the event, stamping identity as appropriate — the local
/// `user_id`, or the server-authenticated actor on the sync path.
pub(crate) enum PlaidStep {
    Append(Event),
    Reject(PlaidCommandError),
    /// Nothing to record. Not a refusal — the command ran, and found the log
    /// already says what it was going to say. Distinct from `Reject` because the
    /// caller should report success, and distinct from appending an event that
    /// changes nothing because a log is read by people.
    Nothing,
}

/// Check that the connection exists and the ledger account is real, then build
/// the `PlaidAccountMapped` event.
///
/// Inside the transaction, not before it. The previous version read
/// `SELECT 1 FROM plaid_items` on a plain connection and appended afterwards —
/// a read-then-append window in which the item could be disconnected, leaving a
/// mapping that points at a connection which no longer exists.
///
/// The `local_account_id` check is new. Without it a typo'd account id was
/// accepted and only surfaced later as a foreign-key failure inside the
/// projector, which reads as an internal error rather than "no such account".
pub(crate) fn build_map_account_in_txn(
    tx: &rusqlite::Transaction<'_>,
    item_id: &str,
    plaid_account_id: &str,
    local_account_id: &str,
) -> Result<PlaidStep, EventStoreError> {
    let item_exists: bool = tx
        .query_row("SELECT 1 FROM plaid_items WHERE id = ?1", [item_id], |_| {
            Ok(true)
        })
        .optional()?
        .unwrap_or(false);
    if !item_exists {
        return Ok(PlaidStep::Reject(PlaidCommandError::ItemNotFound(
            item_id.to_string(),
        )));
    }

    let account_exists: bool = tx
        .query_row(
            "SELECT 1 FROM accounts WHERE id = ?1",
            [local_account_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !account_exists {
        return Ok(PlaidStep::Reject(PlaidCommandError::AccountNotMapped(
            format!("no such ledger account: {local_account_id}"),
        )));
    }

    Ok(PlaidStep::Append(Event::PlaidAccountMapped {
        item_id: item_id.to_string(),
        plaid_account_id: plaid_account_id.to_string(),
        local_account_id: local_account_id.to_string(),
    }))
}

/// What a refresh would change, and the event to record it.
///
/// Read **inside** the transaction for the same reason the two builders around it
/// are: the decision is "is this list different from what we hold", and answering
/// it outside the write lock lets two refreshes of the same connection both
/// conclude "yes" and append two events for one change.
///
/// A refresh that finds nothing new appends nothing. Pressing refresh is the
/// thing somebody does when they are *not sure*, so it will be pressed often and
/// mostly find nothing; a log with an event per press is a log nobody can read.
pub(crate) fn build_refresh_accounts_in_txn(
    tx: &rusqlite::Transaction<'_>,
    item_id: &str,
    found: &[PlaidAccountInfo],
) -> Result<PlaidStep, EventStoreError> {
    let item_exists: bool = tx
        .query_row("SELECT 1 FROM plaid_items WHERE id = ?1", [item_id], |_| {
            Ok(true)
        })
        .optional()?
        .unwrap_or(false);
    if !item_exists {
        return Ok(PlaidStep::Reject(PlaidCommandError::ItemNotFound(
            item_id.to_string(),
        )));
    }
    if found.is_empty() {
        // The bank reporting nothing behind a login is not a refresh finding
        // nothing — it is an answer that would be wrong to record, and recording
        // it would say the connection had been checked and found empty.
        return Ok(PlaidStep::Reject(PlaidCommandError::ItemNotFound(format!(
            "{item_id}: the bank returned no accounts"
        ))));
    }

    let mut known: std::collections::HashMap<String, (String, String, Option<String>)> =
        std::collections::HashMap::new();
    let mut stmt = tx.prepare(
        "SELECT plaid_account_id, name, account_type, mask FROM plaid_local_accounts
          WHERE item_id = ?1",
    )?;
    let rows = stmt.query_map([item_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            (r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get(3)?),
        ))
    })?;
    for row in rows {
        let (id, rest) = row?;
        known.insert(id, rest);
    }

    // A renamed or re-masked account counts as a change too: the name is what a
    // person picks from when mapping, and one that silently disagrees with the
    // bank's is worse than one that is a day out of date.
    let differs = found.iter().any(|a| {
        known
            .get(&a.plaid_account_id)
            .map(|(name, kind, mask)| *name != a.name || *kind != a.account_type || *mask != a.mask)
            .unwrap_or(true)
    });
    if !differs {
        return Ok(PlaidStep::Nothing);
    }

    Ok(PlaidStep::Append(Event::PlaidAccountsRefreshed {
        item_id: item_id.to_string(),
        plaid_accounts: found.to_vec(),
    }))
}

/// Check the connection exists, then build the `PlaidItemDisconnected` event.
///
/// In-txn like its neighbours, and it was not before: `disconnect_item` read on a
/// plain connection and appended afterwards, so two disconnects of the same
/// connection could both find it present and append twice. Harmless-looking, but
/// it puts two contradictory-looking records of one action in a log people read
/// to work out what happened.
///
/// Nothing is deleted. The connection's accounts, their mappings and every
/// transaction imported through them stay exactly where they are — a disconnected
/// connection has stopped being a source of new transactions, which is not the
/// same as never having been one.
pub(crate) fn build_disconnect_item_in_txn(
    tx: &rusqlite::Transaction<'_>,
    item_id: &str,
    reason: &str,
) -> Result<PlaidStep, EventStoreError> {
    let active: bool = tx
        .query_row(
            "SELECT 1 FROM plaid_items WHERE id = ?1 AND status = 'active'",
            [item_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !active {
        return Ok(PlaidStep::Reject(PlaidCommandError::ItemNotFound(
            item_id.to_string(),
        )));
    }

    Ok(PlaidStep::Append(Event::PlaidItemDisconnected {
        item_id: item_id.to_string(),
        reason: reason.to_string(),
    }))
}

/// Check the mapping is actually there, then build the `PlaidAccountUnmapped`
/// event. Same in-txn reasoning as [`build_map_account_in_txn`]: read outside the
/// transaction and two concurrent unmaps both succeed, appending two events for
/// one removal.
pub(crate) fn build_unmap_account_in_txn(
    tx: &rusqlite::Transaction<'_>,
    item_id: &str,
    plaid_account_id: &str,
    local_account_id: &str,
) -> Result<PlaidStep, EventStoreError> {
    let mapped: bool = tx
        .query_row(
            "SELECT 1 FROM plaid_local_accounts
              WHERE item_id = ?1 AND plaid_account_id = ?2 AND local_account_id = ?3",
            rusqlite::params![item_id, plaid_account_id, local_account_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !mapped {
        return Ok(PlaidStep::Reject(PlaidCommandError::AccountNotMapped(
            format!("{item_id}:{plaid_account_id}"),
        )));
    }

    Ok(PlaidStep::Append(Event::PlaidAccountUnmapped {
        item_id: item_id.to_string(),
        plaid_account_id: plaid_account_id.to_string(),
        local_account_id: local_account_id.to_string(),
    }))
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
            proxy_item_id: Some(proxy_item_id.to_string()),
            institution_name: institution_name.to_string(),
            plaid_accounts: accounts,
        };

        let envelope = EventEnvelope::new(event, self.user_id.clone());
        let stored = self.store.append(envelope)?;
        self.store.apply_projection(&stored)?;
        Ok(stored)
    }

    /// Record the accounts a bank now reports behind a connection.
    ///
    /// `found` is the whole list from the bank, not a delta, and nothing is
    /// removed by it — see [`Event::PlaidAccountsRefreshed`].
    ///
    /// `Ok(None)` means the books already agreed with the bank. That is the
    /// common outcome and it appends nothing.
    pub fn refresh_accounts(
        &mut self,
        item_id: &str,
        found: Vec<PlaidAccountInfo>,
    ) -> Result<Option<StoredEvent>, PlaidCommandError> {
        self.append_optional_step(|tx| build_refresh_accounts_in_txn(tx, item_id, &found))
    }

    /// Map a Plaid account to a local account
    pub fn map_account(
        &mut self,
        item_id: &str,
        plaid_account_id: &str,
        local_account_id: &str,
    ) -> Result<StoredEvent, PlaidCommandError> {
        self.append_step(|tx| {
            build_map_account_in_txn(tx, item_id, plaid_account_id, local_account_id)
        })
    }

    /// Unmap a Plaid account from a local account
    pub fn unmap_account(
        &mut self,
        item_id: &str,
        plaid_account_id: &str,
        local_account_id: &str,
    ) -> Result<StoredEvent, PlaidCommandError> {
        self.append_step(|tx| {
            build_unmap_account_in_txn(tx, item_id, plaid_account_id, local_account_id)
        })
    }

    /// Append one event whose invariants are checked inside the transaction,
    /// retrying on a head move.
    ///
    /// The retry is what makes moving these checks in-txn free for the local
    /// caller: a concurrent append no longer means a lost command, it means one
    /// more attempt against the state that actually exists now.
    fn append_step(
        &mut self,
        build: impl Fn(&rusqlite::Transaction<'_>) -> Result<PlaidStep, EventStoreError>,
    ) -> Result<StoredEvent, PlaidCommandError> {
        self.append_optional_step(build)?
            .ok_or_else(|| PlaidCommandError::ItemNotFound("nothing to record".to_string()))
    }

    /// The same, for commands that may legitimately have nothing to say.
    ///
    /// `Ok(None)` is success with no event. Only [`build_refresh_accounts_in_txn`]
    /// uses it today; [`append_step`] keeps the simpler signature for the commands
    /// where "nothing to record" would be a bug.
    ///
    /// [`append_step`]: PlaidCommands::append_step
    fn append_optional_step(
        &mut self,
        build: impl Fn(&rusqlite::Transaction<'_>) -> Result<PlaidStep, EventStoreError>,
    ) -> Result<Option<StoredEvent>, PlaidCommandError> {
        let user_id = self.user_id.clone();
        let nothing = std::cell::Cell::new(false);
        loop {
            nothing.set(false);
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build(tx)? {
                    PlaidStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    PlaidStep::Reject(e) => Ok(Verdict::Reject(e)),
                    // Nothing to append, and not an error. Reported through
                    // `Reject` because that is the only way out of the closure
                    // that does not write, then turned back into `Ok(None)` below
                    // — the flag is what tells the two apart.
                    PlaidStep::Nothing => {
                        nothing.set(true);
                        Ok(Verdict::Reject(PlaidCommandError::ItemNotFound(
                            "nothing to record".to_string(),
                        )))
                    }
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;
            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(Some(stored)),
                CheckedOutcome::HeadMismatch { .. } => continue,
                CheckedOutcome::Rejected(_) if nothing.get() => return Ok(None),
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Import synced transactions from the proxy into journal entries.
    /// Returns (added_count, skipped_count).
    pub fn import_transactions(
        &mut self,
        item_id: &str,
        transactions: &[SyncedTransaction],
    ) -> Result<(u32, u32), PlaidCommandError> {
        // Get uncategorized account first (needs &mut store)
        let uncategorized_id =
            crate::commands::account_commands::find_or_create_uncategorized(self.store)?;

        // Pre-load all data we need into owned values, then drop the borrows
        let (mappings, already_imported_set) = {
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

            (mappings, already_imported)
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
            let currency = txn.currency.clone().unwrap_or_else(|| "USD".to_string());
            let memo = txn
                .merchant_name
                .as_deref()
                .unwrap_or(&txn.name)
                .to_string();
            let user_id = self.user_id.clone();
            let uncat = uncategorized_id.clone();
            let txn_ref = txn.transaction_id.clone();
            let item = item_id.to_string();

            // Post the entry, fold in its projection, and record the dedup row —
            // all in one append_checked transaction (retry on a head move). The
            // reference dedup and the fences (account active, period open) run
            // under the write lock. A duplicate reference (a concurrent import
            // that won the race) or a fence violation is counted as skipped
            // rather than posted.
            let posted = loop {
                let head = self.store.latest_id()?.unwrap_or(0);
                let outcome = self.store.append_checked(
                    head,
                    |tx| {
                        // Already imported under this bare txn id — including when
                        // it was consumed by a transfer (whose journal reference is
                        // `transfer:from:to`, so the reference check below won't see
                        // it). Checked in-txn so a concurrent transfer that won the
                        // race is caught, not double-posted.
                        if tx
                            .query_row(
                                "SELECT 1 FROM plaid_imported_transactions WHERE plaid_transaction_id = ?1",
                                [&txn_ref],
                                |_| Ok(true),
                            )
                            .optional()?
                            .unwrap_or(false)
                        {
                            return Ok(Verdict::Reject(PlaidCommandError::AlreadyImported(
                                txn_ref.clone(),
                            )));
                        }
                        if check_reference_free_in_txn(tx, &txn_ref)?.is_some() {
                            return Ok(Verdict::Reject(PlaidCommandError::AlreadyImported(
                                txn_ref.clone(),
                            )));
                        }
                        if let Some(e) = check_entry_invariants_in_txn(
                            tx,
                            &[local_account_id.as_str(), uncat.as_str()],
                            date,
                        )? {
                            return Ok(Verdict::Reject(PlaidCommandError::from(e)));
                        }
                        let entry_id = Uuid::new_v4().to_string();
                        let lines = vec![
                            JournalLineData {
                                line_id: format!("{}-line-1", entry_id),
                                account_id: local_account_id.clone(),
                                amount: -amount_cents,
                                currency: currency.clone(),
                                exchange_rate: None,
                                memo: None,
                            },
                            JournalLineData {
                                line_id: format!("{}-line-2", entry_id),
                                account_id: uncat.clone(),
                                amount: amount_cents,
                                currency: currency.clone(),
                                exchange_rate: None,
                                memo: None,
                            },
                        ];
                        let event = Event::JournalEntryPosted {
                            entry_id,
                            date,
                            memo: memo.clone(),
                            lines,
                            reference: Some(txn_ref.clone()),
                            source: Some(JournalEntrySource::Plaid),
                        };
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    },
                    |tx, stored| {
                        Projector::new(tx)
                            .apply(stored)
                            .map_err(|e| EventStoreError::Projection(e.to_string()))?;
                        let entry_id = match &stored.event {
                            Event::JournalEntryPosted { entry_id, .. } => entry_id.as_str(),
                            _ => "",
                        };
                        tx.execute(
                            "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                            rusqlite::params![txn_ref, item, entry_id],
                        )?;
                        Ok(())
                    },
                )?;
                match outcome {
                    CheckedOutcome::Appended(_) => break true,
                    CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                    CheckedOutcome::Rejected(_) => break false,      // duplicate or fence → skip
                }
            };
            if posted {
                added += 1;
            } else {
                skipped += 1;
            }
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
        self.store.apply_projection(&stored)?;

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
            let outcome =
                stage_transactions_in_conn(self.store.connection(), item_id, transactions)?;
            (outcome.staged, outcome.skipped())
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
        self.store.apply_projection(&stored)?;

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

        // Pick the source ("from", money leaving) and destination ("to") legs.
        //
        // Asset↔asset transfers (e.g. checking→savings) arrive equal-and-opposite:
        // Plaid amounts are positive when money leaves an account, so the positive
        // leg is the source.
        //
        // Asset↔liability transfers (e.g. a credit-card payment) arrive with the
        // SAME sign — both legs positive — so sign alone can't tell direction. In
        // that case the asset account is the source (cash leaving) and the
        // liability is paid down. Either way the journal below posts -abs to the
        // source and +abs to the destination, which nets to zero and moves both
        // balances correctly (the liability, carried negative, moves toward zero).
        let (from_txn, to_txn) = if txn1.amount_cents == -txn2.amount_cents {
            if txn1.amount_cents > 0 {
                (&txn1, &txn2)
            } else {
                (&txn2, &txn1)
            }
        } else {
            let txn1_is_asset =
                account_type_of(self.store.connection(), txn1.local_account_id.as_deref())
                    .as_deref()
                    == Some("asset");
            if txn1_is_asset {
                (&txn1, &txn2)
            } else {
                (&txn2, &txn1)
            }
        };

        let date = NaiveDate::parse_from_str(&from_txn.date, "%Y-%m-%d")
            .unwrap_or_else(|_| Utc::now().date_naive());
        let abs_amount = from_txn.amount_cents.unsigned_abs() as i64;
        let memo = format!(
            "Transfer: {}",
            from_txn.merchant_name.as_deref().unwrap_or(&from_txn.name)
        );

        let from_account = from_txn.local_account_id.clone().ok_or_else(|| {
            PlaidCommandError::AccountNotMapped(from_txn.plaid_account_id.clone())
        })?;
        let to_account = to_txn
            .local_account_id
            .clone()
            .ok_or_else(|| PlaidCommandError::AccountNotMapped(to_txn.plaid_account_id.clone()))?;

        let from_currency = from_txn.currency.clone();
        let to_currency = to_txn.currency.clone();
        let from_ref = from_txn.plaid_transaction_id.clone();
        let from_item = from_txn.item_id.clone();
        let to_ref = to_txn.plaid_transaction_id.clone();
        let to_item = to_txn.item_id.clone();
        let reference = format!("transfer:{}:{}", from_ref, to_ref);
        let staged1 = txn1.id.clone();
        let staged2 = txn2.id.clone();
        let cand = candidate_id.to_string();
        let user_id = self.user_id.clone();

        // Post the transfer entry, its projection, both dedup rows, and the
        // staged/candidate status updates in one append_checked transaction
        // (retry on a head move). The reference dedup and the fences (both
        // accounts active, period open) run under the write lock; a duplicate or
        // fence violation is a terminal error.
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    if check_reference_free_in_txn(tx, &reference)?.is_some() {
                        return Ok(Verdict::Reject(PlaidCommandError::AlreadyImported(
                            reference.clone(),
                        )));
                    }
                    if let Some(e) = check_entry_invariants_in_txn(
                        tx,
                        &[from_account.as_str(), to_account.as_str()],
                        date,
                    )? {
                        return Ok(Verdict::Reject(PlaidCommandError::from(e)));
                    }
                    let entry_id = Uuid::new_v4().to_string();
                    let lines = vec![
                        JournalLineData {
                            line_id: format!("{}-line-1", entry_id),
                            account_id: from_account.clone(),
                            amount: -abs_amount,
                            currency: from_currency.clone(),
                            exchange_rate: None,
                            memo: None,
                        },
                        JournalLineData {
                            line_id: format!("{}-line-2", entry_id),
                            account_id: to_account.clone(),
                            amount: abs_amount,
                            currency: to_currency.clone(),
                            exchange_rate: None,
                            memo: None,
                        },
                    ];
                    let event = Event::JournalEntryPosted {
                        entry_id,
                        date,
                        memo: memo.clone(),
                        lines,
                        reference: Some(reference.clone()),
                        source: Some(JournalEntrySource::Plaid),
                    };
                    Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))?;
                    let entry_id = match &stored.event {
                        Event::JournalEntryPosted { entry_id, .. } => entry_id.as_str(),
                        _ => "",
                    };
                    tx.execute(
                        "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                        rusqlite::params![from_ref, from_item, entry_id],
                    )?;
                    tx.execute(
                        "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                        rusqlite::params![to_ref, to_item, entry_id],
                    )?;
                    tx.execute(
                        "UPDATE plaid_staged_transactions SET status = 'imported' WHERE id IN (?1, ?2)",
                        rusqlite::params![staged1, staged2],
                    )?;
                    tx.execute(
                        "UPDATE plaid_transfer_candidates SET status = 'confirmed' WHERE id = ?1",
                        [cand.as_str()],
                    )?;
                    Ok(())
                },
            )?;
            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(stored),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
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

        let uncategorized_id =
            crate::commands::account_commands::find_or_create_uncategorized(self.store)?;

        let local_account_id = txn
            .local_account_id
            .clone()
            .unwrap_or_else(|| uncategorized_id.clone());

        let date = NaiveDate::parse_from_str(&txn.date, "%Y-%m-%d")
            .unwrap_or_else(|_| Utc::now().date_naive());
        let memo = txn
            .merchant_name
            .as_deref()
            .unwrap_or(&txn.name)
            .to_string();
        let currency = txn.currency.clone();
        let amount_cents = txn.amount_cents;
        let txn_ref = txn.plaid_transaction_id.clone();
        let item = txn.item_id.clone();
        let user_id = self.user_id.clone();
        let staged_id = staged_txn_id.to_string();

        // Post the entry, its projection, the dedup row, and the staged-status
        // update in one append_checked transaction (retry on a head move). The
        // reference dedup and the fences (account active, period open) run under
        // the write lock; a duplicate or fence violation is a terminal error.
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    // Already imported under this bare txn id — including when it
                    // was consumed by a transfer (whose journal reference is
                    // `transfer:from:to`, so the reference check below won't see
                    // it). Checked in-txn so a concurrent import is caught.
                    if tx
                        .query_row(
                            "SELECT 1 FROM plaid_imported_transactions WHERE plaid_transaction_id = ?1",
                            [&txn_ref],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false)
                    {
                        return Ok(Verdict::Reject(PlaidCommandError::AlreadyImported(
                            txn_ref.clone(),
                        )));
                    }
                    if check_reference_free_in_txn(tx, &txn_ref)?.is_some() {
                        return Ok(Verdict::Reject(PlaidCommandError::AlreadyImported(
                            txn_ref.clone(),
                        )));
                    }
                    if let Some(e) = check_entry_invariants_in_txn(
                        tx,
                        &[local_account_id.as_str(), uncategorized_id.as_str()],
                        date,
                    )? {
                        return Ok(Verdict::Reject(PlaidCommandError::from(e)));
                    }
                    let entry_id = Uuid::new_v4().to_string();
                    let lines = vec![
                        JournalLineData {
                            line_id: format!("{}-line-1", entry_id),
                            account_id: local_account_id.clone(),
                            amount: -amount_cents,
                            currency: currency.clone(),
                            exchange_rate: None,
                            memo: None,
                        },
                        JournalLineData {
                            line_id: format!("{}-line-2", entry_id),
                            account_id: uncategorized_id.clone(),
                            amount: amount_cents,
                            currency: currency.clone(),
                            exchange_rate: None,
                            memo: None,
                        },
                    ];
                    let event = Event::JournalEntryPosted {
                        entry_id,
                        date,
                        memo: memo.clone(),
                        lines,
                        reference: Some(txn_ref.clone()),
                        source: Some(JournalEntrySource::Plaid),
                    };
                    Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))?;
                    let entry_id = match &stored.event {
                        Event::JournalEntryPosted { entry_id, .. } => entry_id.as_str(),
                        _ => "",
                    };
                    tx.execute(
                        "INSERT INTO plaid_imported_transactions (plaid_transaction_id, item_id, entry_id) VALUES (?1, ?2, ?3)",
                        rusqlite::params![txn_ref, item, entry_id],
                    )?;
                    tx.execute(
                        "UPDATE plaid_staged_transactions SET status = 'imported' WHERE id = ?1",
                        [staged_id.as_str()],
                    )?;
                    Ok(())
                },
            )?;
            match outcome {
                CheckedOutcome::Appended(stored) => return Ok(stored),
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Import all: confirm all pending transfer candidates, then import remaining unmatched.
    /// Returns (transfers_imported, unmatched_imported).
    pub fn import_all_staged(&mut self) -> Result<(u32, u32), PlaidCommandError> {
        // Collect pending transfer candidate IDs
        let candidate_ids: Vec<String> = {
            let conn = self.store.connection();
            let mut stmt = conn.prepare(
                "SELECT id FROM plaid_transfer_candidates WHERE status IN ('pending', 'manual')",
            )?;
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
        self.append_step(|tx| build_disconnect_item_in_txn(tx, item_id, reason))
    }
}

/// What one staging run did with the transactions it was handed.
///
/// Counted four ways rather than summed into "staged N, skipped M", because the
/// outcomes call for different responses and lumping them together sends someone
/// hunting for hundreds of missing transactions that were mostly duplicates they
/// already had.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagedOutcome {
    /// Rows written to the review table. Includes [`Self::unmapped`].
    pub staged: u32,
    /// Of those, how many arrived for a bank account with no local account yet.
    /// They are staged all the same — see below — and the import path leaves them
    /// alone until their account is mapped.
    pub unmapped: u32,
    /// Already staged, or already imported. The bulk of any re-pull.
    pub duplicates: u32,
    /// Not yet settled at the bank. Skipped because the amount can still change;
    /// they arrive again, as themselves, once they post.
    pub still_pending: u32,
}

impl StagedOutcome {
    /// Everything that did not become a row.
    pub fn skipped(&self) -> u32 {
        self.duplicates + self.still_pending
    }
}

/// Stage transactions into the machine-local review table.
///
/// Takes a connection rather than `&mut self` because the delegated path has no
/// ledger to write to. On a group-hosted book a pull is not a fact about the
/// books — only what one member has fetched and not yet posted — so staging is
/// machine-local, and appending a `PlaidTransactionsSynced` event here would be a
/// local write that a replica must refuse.
///
/// # Why nothing is dropped
///
/// A transaction whose bank account is not mapped yet is staged with a null local
/// account rather than discarded. The provider advances this consumer's position
/// in the stream as it hands transactions over, so anything dropped here is not
/// coming back: the next pull starts after it. An unusable row costs a line in a
/// review list. A dropped one costs a transaction nobody will ever see again, and
/// nobody will know to look for.
pub fn stage_transactions_in_conn(
    conn: &rusqlite::Connection,
    item_id: &str,
    transactions: &[SyncedTransaction],
) -> Result<StagedOutcome, PlaidCommandError> {
    let mut stmt = conn.prepare(
        "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts WHERE item_id = ?1",
    )?;
    let mappings: HashMap<String, Option<String>> = stmt
        .query_map([item_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut outcome = StagedOutcome::default();
    for txn in transactions {
        if txn.pending {
            outcome.still_pending += 1;
            continue;
        }

        let local_account_id = mappings.get(&txn.account_id).and_then(|o| o.clone());

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
            outcome.duplicates += 1;
            continue;
        }

        let amount_cents = (txn.amount * 100.0).round() as i64;
        let currency = txn.iso_currency_code.as_deref().unwrap_or("USD");
        let id = Uuid::new_v4().to_string();

        let payment_meta_json = txn
            .payment_meta
            .as_ref()
            .filter(|pm| !pm.is_empty())
            .and_then(|pm| serde_json::to_string(pm).ok());

        conn.execute(
            "INSERT INTO plaid_staged_transactions
             (id, item_id, plaid_transaction_id, plaid_account_id, local_account_id,
              amount_cents, date, name, merchant_name, currency, status, payment_meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11)",
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
                currency,
                payment_meta_json
            ],
        )?;
        if local_account_id.is_none() {
            outcome.unmapped += 1;
        }
        outcome.staged += 1;
    }

    Ok(outcome)
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
    #[serde(default)]
    pub payment_meta: Option<PaymentMeta>,
}

/// Payment metadata from Plaid (card holder, reference number, etc.)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaymentMeta {
    pub by_order_of: Option<String>,
    pub payee: Option<String>,
    pub payer: Option<String>,
    pub payment_method: Option<String>,
    pub payment_processor: Option<String>,
    pub reason: Option<String>,
    pub reference_number: Option<String>,
}

impl PaymentMeta {
    /// Returns true if all fields are None (Plaid sometimes sends an object with all nulls)
    pub fn is_empty(&self) -> bool {
        self.by_order_of.is_none()
            && self.payee.is_none()
            && self.payer.is_none()
            && self.payment_method.is_none()
            && self.payment_processor.is_none()
            && self.reason.is_none()
            && self.reference_number.is_none()
    }
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
    pub payment_meta: Option<PaymentMeta>,
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
///
/// Matches transactions of equal magnitude within 5 days, across two different
/// mapped accounts, in either of two shapes:
///   * equal-and-opposite amounts — an asset↔asset transfer (checking→savings),
///     where Plaid reports the two legs with opposite signs; or
///   * equal-and-SAME-sign amounts across an asset and a liability account — a
///     credit-card payment, whose two legs Plaid reports with the same sign.
///
/// 5 days covers weekend lag on cross-bank payments (e.g. Thu checking debit →
/// Mon card post). Candidates always require user confirmation before import.
///
/// Manually-marked candidates (status `'manual'`) and their legs are preserved
/// across re-runs; only auto-generated (`'pending'`) candidates are recomputed.
pub fn detect_transfers(conn: &rusqlite::Connection) -> Result<u32, PlaidCommandError> {
    // Clear previous auto-generated candidates (leave 'manual'/'confirmed' alone)
    conn.execute(
        "DELETE FROM plaid_transfer_candidates WHERE status = 'pending'",
        [],
    )?;

    // Reset previously matched staged txns back to pending, except those still
    // held by a surviving manual candidate.
    conn.execute(
        "UPDATE plaid_staged_transactions SET status = 'pending'
         WHERE status = 'matched'
           AND id NOT IN (
               SELECT staged_txn_id_1 FROM plaid_transfer_candidates WHERE status = 'manual'
               UNION
               SELECT staged_txn_id_2 FROM plaid_transfer_candidates WHERE status = 'manual'
           )",
        [],
    )?;

    // Find pairs: equal magnitude, within 5 days, different mapped accounts —
    // either opposite-signed (asset↔asset) or same-signed across asset↔liability.
    let mut stmt = conn.prepare(
        "SELECT t1.id, t2.id,
                ABS(julianday(t1.date) - julianday(t2.date)) as date_diff
         FROM plaid_staged_transactions t1
         JOIN plaid_staged_transactions t2
           ON ABS(t1.amount_cents) = ABS(t2.amount_cents)
           AND t1.amount_cents != 0
           AND t1.id < t2.id
           AND t1.local_account_id IS NOT NULL
           AND t2.local_account_id IS NOT NULL
           AND t1.local_account_id != t2.local_account_id
           AND ABS(julianday(t1.date) - julianday(t2.date)) <= 5
         JOIN accounts a1 ON a1.id = t1.local_account_id
         JOIN accounts a2 ON a2.id = t2.local_account_id
         WHERE t1.status = 'pending' AND t2.status = 'pending'
           AND (
                 t1.amount_cents = -t2.amount_cents
              OR (a1.account_type = 'asset' AND a2.account_type = 'liability')
              OR (a1.account_type = 'liability' AND a2.account_type = 'asset')
               )
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

        // Confidence: 1.0 for same day, decreasing to ~0.17 at the 5-day edge.
        let confidence = 1.0 - (date_diff / 6.0);

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

/// Look up an account's type ("asset", "liability", …) from the projection.
fn account_type_of(conn: &rusqlite::Connection, account_id: Option<&str>) -> Option<String> {
    let id = account_id?;
    conn.query_row(
        "SELECT account_type FROM accounts WHERE id = ?1",
        [id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Manually pair two pending staged transactions as a transfer candidate.
///
/// Used when auto-detection misses a transfer — e.g. a credit-card payment whose
/// legs differ in some way the matcher can't see. The pair is recorded with
/// status `'manual'` so it survives re-syncs, and shows up as a candidate to
/// confirm or reject like any auto-detected one.
pub fn create_manual_transfer(
    conn: &rusqlite::Connection,
    staged_id_1: &str,
    staged_id_2: &str,
) -> Result<String, PlaidCommandError> {
    if staged_id_1 == staged_id_2 {
        return Err(PlaidCommandError::InvalidTransfer(
            "cannot pair a transaction with itself".to_string(),
        ));
    }

    let t1 = load_staged_transaction(conn, staged_id_1)?;
    let t2 = load_staged_transaction(conn, staged_id_2)?;

    if t1.local_account_id.is_none() || t2.local_account_id.is_none() {
        return Err(PlaidCommandError::InvalidTransfer(
            "both transactions must be linked to a local account first".to_string(),
        ));
    }
    if t1.local_account_id == t2.local_account_id {
        return Err(PlaidCommandError::InvalidTransfer(
            "both legs are on the same account — a transfer moves between two accounts".to_string(),
        ));
    }
    if t1.status != "pending" || t2.status != "pending" {
        return Err(PlaidCommandError::InvalidTransfer(
            "both transactions must be unmatched (pending)".to_string(),
        ));
    }
    if t1.amount_cents.abs() != t2.amount_cents.abs() {
        return Err(PlaidCommandError::InvalidTransfer(
            "the two legs must be the same amount".to_string(),
        ));
    }

    let candidate_id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO plaid_transfer_candidates (id, staged_txn_id_1, staged_txn_id_2, confidence, status)
         VALUES (?1, ?2, ?3, 1.0, 'manual')",
        rusqlite::params![candidate_id, staged_id_1, staged_id_2],
    )?;
    conn.execute(
        "UPDATE plaid_staged_transactions SET status = 'matched' WHERE id = ?1 OR id = ?2",
        rusqlite::params![staged_id_1, staged_id_2],
    )?;

    Ok(candidate_id)
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
fn parse_payment_meta(json: Option<String>) -> Option<PaymentMeta> {
    json.and_then(|s| serde_json::from_str(&s).ok())
}

fn load_staged_transaction(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<StagedTransaction, PlaidCommandError> {
    conn.query_row(
        "SELECT id, item_id, plaid_transaction_id, plaid_account_id, local_account_id,
                amount_cents, date, name, merchant_name, currency, status, payment_meta
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
                payment_meta: parse_payment_meta(row.get(11)?),
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
         WHERE tc.status IN ('pending', 'manual')
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
                amount_cents, date, name, merchant_name, currency, status, payment_meta
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
                payment_meta: parse_payment_meta(row.get(11)?),
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
        "SELECT COUNT(*) FROM plaid_transfer_candidates WHERE status IN ('pending', 'manual')",
        [],
        |row| row.get(0),
    )?;
    Ok((staged, transfers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{
        AccountCommands, CreateAccountCommand, DeactivateAccountCommand,
    };
    use crate::domain::AccountType;
    use crate::store::migrations::init_schema;

    fn mk_account(store: &mut EventStore, num: &str, name: &str, ty: AccountType) -> String {
        let stored = AccountCommands::new(store, "u".to_string())
            .create_account(CreateAccountCommand {
                account_type: ty,
                account_number: num.to_string(),
                name: name.to_string(),
                parent_id: None,
                currency: None,
                description: None,
            })
            .unwrap();
        match &stored.event {
            Event::AccountCreated { account_id, .. } => account_id.clone(),
            _ => panic!("expected AccountCreated"),
        }
    }

    fn acct(id: &str, name: &str) -> PlaidAccountInfo {
        PlaidAccountInfo {
            plaid_account_id: id.to_string(),
            name: name.to_string(),
            official_name: None,
            account_type: "depository".to_string(),
            mask: Some("0000".to_string()),
            persistent_account_id: None,
        }
    }

    /// An account shaped the way the reported Chase data was.
    fn chase(id: &str, name: &str, kind: &str, mask: &str) -> PlaidAccountInfo {
        PlaidAccountInfo {
            plaid_account_id: id.to_string(),
            name: name.to_string(),
            official_name: None,
            account_type: kind.to_string(),
            mask: Some(mask.to_string()),
            persistent_account_id: None,
        }
    }

    /// The same, with the id that survives a re-link.
    fn acct_persistent(id: &str, name: &str, persistent: &str) -> PlaidAccountInfo {
        PlaidAccountInfo {
            persistent_account_id: Some(persistent.to_string()),
            ..acct(id, name)
        }
    }

    fn recorded_accounts(store: &EventStore) -> Vec<(String, Option<String>)> {
        let conn = store.connection();
        let mut stmt = conn
            .prepare(
                "SELECT plaid_account_id, local_account_id FROM plaid_local_accounts
                  WHERE item_id = 'item1' ORDER BY plaid_account_id",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    /// The accounts a bank reports later are added to the ones already known.
    ///
    /// This is the repair path for connections made while the account list came
    /// from the browser — which for an OAuth bank could be most of the accounts
    /// missing, with nothing anywhere that could have noticed.
    #[test]
    fn a_refresh_adds_accounts_the_connection_did_not_have() {
        let (mut store, _local) = setup();

        let stored = PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![acct("pa1", "Checking"), acct("pa2", "Card 2")],
            )
            .expect("refresh")
            .expect("something to record");
        assert!(matches!(stored.event, Event::PlaidAccountsRefreshed { .. }));

        let rows = recorded_accounts(&store);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[1].0, "pa2");
    }

    /// The mapping survives.
    ///
    /// `local_account_id` lives on the same row as the bank's own fields, so a
    /// projection written as `INSERT OR REPLACE` — the obvious way, and what the
    /// neighbouring `PlaidItemConnected` arm does — would blank it. The visible
    /// result would be a connection that quietly stopped importing into the
    /// account it was mapped to, on the one action a user takes *because* they
    /// want the connection to be more complete.
    #[test]
    fn a_refresh_does_not_disturb_an_existing_mapping() {
        let (mut store, local) = setup();

        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![acct("pa1", "Checking renamed"), acct("pa2", "Card 2")],
            )
            .expect("refresh");

        let rows = recorded_accounts(&store);
        assert_eq!(
            rows[0],
            ("pa1".to_string(), Some(local)),
            "the mapping was lost: {rows:?}"
        );
        assert_eq!(rows[1], ("pa2".to_string(), None), "{rows:?}");

        // The bank's own fields do follow the bank.
        let name: String = store
            .connection()
            .query_row(
                "SELECT name FROM plaid_local_accounts WHERE plaid_account_id = 'pa1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(name, "Checking renamed");
    }

    /// The bug this was reported as: one bank account, listed twice.
    ///
    /// Reproduced from real data. A Chase login was linked three times; each link
    /// minted fresh Plaid ids for the same checking account and card, and the
    /// ledger kept a connection per link. Refreshing one of the older connections
    /// returned the *current* ids, which nothing recognised as accounts already
    /// held — so "BUS COMPLETE CHK 0908" appeared twice, under two ids, and so
    /// did the card.
    #[test]
    fn an_account_that_comes_back_with_a_new_id_is_not_a_second_account() {
        let (mut store, _local) = setup();

        // The connection as first linked: two accounts, the ids of that link.
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![
                    chase("KqZmpVXnk8HRr5", "BUS COMPLETE CHK", "depository", "0908"),
                    chase("k18b75e9a6fmpg", "Z. PATTERSON", "credit", "3082"),
                ],
            )
            .expect("first")
            .expect("something to record");

        // The same bank, re-linked: same two accounts, new ids.
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![
                    chase("by1BD60nJQi53b", "BUS COMPLETE CHK", "depository", "0908"),
                    chase("g0gD3qxJbEfAbD", "Z. PATTERSON", "credit", "3082"),
                ],
            )
            .expect("second");

        let rows = recorded_accounts(&store);
        let ids: Vec<&str> = rows.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids.len(),
            3,
            "the re-linked accounts were added instead of recognised: {rows:?}"
        );
        assert!(ids.contains(&"by1BD60nJQi53b"), "{rows:?}");
        assert!(ids.contains(&"g0gD3qxJbEfAbD"), "{rows:?}");
        assert!(
            !ids.contains(&"KqZmpVXnk8HRr5") && !ids.contains(&"k18b75e9a6fmpg"),
            "the old ids survived alongside the new ones: {rows:?}"
        );
    }

    /// And the mapping moves with it.
    ///
    /// The quieter half of the same bug, and the more expensive one: the ledger
    /// account a bank account is mapped to lives on that row. Treat a re-link as a
    /// new account and the mapping stays on an id the bank will never mention
    /// again — the connection looks healthy and imports nothing at all.
    #[test]
    fn a_re_linked_account_keeps_the_ledger_account_it_was_mapped_to() {
        let (mut store, local) = setup();

        // `setup` maps 'pa1' to a ledger account. Same account, new id.
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![PlaidAccountInfo {
                    plaid_account_id: "pa1-relinked".to_string(),
                    name: "Checking".to_string(),
                    official_name: None,
                    account_type: "depository".to_string(),
                    mask: None,
                    persistent_account_id: None,
                }],
            )
            .expect("refresh");

        let rows = recorded_accounts(&store);
        assert_eq!(
            rows,
            vec![("pa1-relinked".to_string(), Some(local))],
            "the mapping did not follow the account to its new id: {rows:?}"
        );
    }

    /// Plaid's own stable id is believed over name and mask.
    ///
    /// Two cards can share a holder's name and a last-4; the persistent id is the
    /// bank saying which account this is, and it wins.
    #[test]
    fn the_persistent_id_decides_when_there_is_one() {
        let (mut store, _local) = setup();
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![
                    acct_persistent("old-a", "Card", "stable-a"),
                    acct_persistent("old-b", "Card", "stable-b"),
                ],
            )
            .expect("first");

        // Both ids rotate, and both accounts look identical apart from the
        // persistent id. Name-and-mask matching could not tell them apart.
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![
                    acct_persistent("new-a", "Card", "stable-a"),
                    acct_persistent("new-b", "Card", "stable-b"),
                ],
            )
            .expect("second");

        let mut ids: Vec<String> = recorded_accounts(&store)
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| id != "pa1")
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["new-a", "new-b"], "ids were not migrated cleanly");
    }

    /// An ambiguous match is refused rather than guessed.
    ///
    /// Two accounts identical in name, type and mask, and no persistent id to
    /// separate them: merging one into the other would put a card's transactions
    /// into another card's ledger account, silently and permanently. Adding it as
    /// new is wrong too, but it is *visible* and somebody can fix it.
    #[test]
    fn two_indistinguishable_accounts_are_not_merged() {
        let (mut store, _local) = setup();
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![
                    acct("twin-1", "Employee Card"),
                    acct("twin-2", "Employee Card"),
                ],
            )
            .expect("first");

        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts("item1", vec![acct("twin-3", "Employee Card")])
            .expect("second");

        let ids: Vec<String> = recorded_accounts(&store)
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| id != "pa1")
            .collect();
        assert!(
            ids.contains(&"twin-1".to_string())
                && ids.contains(&"twin-2".to_string())
                && ids.contains(&"twin-3".to_string()),
            "an ambiguous match was guessed at rather than left alone: {ids:?}"
        );
    }

    /// A genuinely new account is still added.
    ///
    /// The matcher must not become so eager that a card opened at the bank gets
    /// absorbed into an existing row.
    #[test]
    fn an_account_that_is_actually_new_is_still_added() {
        let (mut store, _local) = setup();
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![
                    chase("chk-1", "BUS COMPLETE CHK", "depository", "0908"),
                    chase("card-1", "Z. PATTERSON", "credit", "3082"),
                ],
            )
            .expect("first");

        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts(
                "item1",
                vec![
                    chase("chk-1", "BUS COMPLETE CHK", "depository", "0908"),
                    chase("card-1", "Z. PATTERSON", "credit", "3082"),
                    chase("card-2", "A. OTHER", "credit", "4402"),
                ],
            )
            .expect("second");

        let ids: Vec<String> = recorded_accounts(&store)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert!(ids.contains(&"card-2".to_string()), "{ids:?}");
        assert_eq!(ids.len(), 4, "pa1 plus the three: {ids:?}");
    }

    /// A refresh that finds nothing new writes nothing.
    ///
    /// Refresh is what somebody presses when they are not sure, so it gets
    /// pressed a lot and mostly finds nothing. An event per press is a log nobody
    /// can read afterwards.
    #[test]
    fn a_refresh_that_finds_nothing_new_appends_nothing() {
        let (mut store, _local) = setup();
        let found = vec![acct("pa1", "Checking"), acct("pa2", "Card 2")];

        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts("item1", found.clone())
            .expect("first refresh")
            .expect("the first one has something to say");
        let settled = store.latest_id().unwrap();

        let outcome = PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts("item1", found)
            .expect("second refresh");

        assert!(outcome.is_none(), "an event was appended for no change");
        assert_eq!(store.latest_id().unwrap(), settled, "the log moved");
    }

    /// A renamed account is a change worth recording.
    ///
    /// The name is what a person picks from when mapping, so one that silently
    /// disagrees with the bank's is worse than one a day out of date — and a
    /// comparison on ids alone would never notice.
    #[test]
    fn a_renamed_account_counts_as_a_change() {
        let (mut store, _local) = setup();
        PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts("item1", vec![acct("pa1", "Checking")])
            .expect("first refresh");

        let outcome = PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts("item1", vec![acct("pa1", "Operating account")])
            .expect("second refresh");

        assert!(outcome.is_some(), "a rename went unrecorded");
    }

    /// An unknown connection is refused rather than creating one.
    #[test]
    fn a_refresh_of_an_unknown_connection_is_refused() {
        let (mut store, _local) = setup();
        let err = PlaidCommands::new(&mut store, "u".to_string())
            .refresh_accounts("no-such-item", vec![acct("pa9", "Ghost")])
            .expect_err("an unknown item must be refused");
        assert!(matches!(err, PlaidCommandError::ItemNotFound(_)), "{err:?}");
    }

    /// One bank login is one connection, however many times it is linked.
    ///
    /// The proxy keeps one connection per user and institution and reuses it on a
    /// reconnect, so the same handle identifies the same login. The local link
    /// path looks it up before minting anything — without that lookup a real
    /// Chase login became three connections, each holding its own generation of
    /// Plaid's per-Item ids for the same two accounts.
    ///
    /// This checks the lookup the handler performs; the handler itself needs an
    /// HTTP round trip to the proxy, which is not what is in question here.
    #[test]
    fn a_reconnect_is_found_by_its_proxy_handle_rather_than_creating_a_second() {
        let (store, _local) = setup();

        let found: Option<String> = store
            .connection()
            .query_row(
                "SELECT id FROM plaid_items WHERE proxy_item_id = ?1 AND status = 'active'",
                ["p1"],
                |r| r.get(0),
            )
            .optional()
            .expect("query");
        assert_eq!(
            found.as_deref(),
            Some("item1"),
            "a reconnect of this bank would have minted a second connection"
        );

        // And a genuinely different bank is not mistaken for it.
        let other: Option<String> = store
            .connection()
            .query_row(
                "SELECT id FROM plaid_items WHERE proxy_item_id = ?1 AND status = 'active'",
                ["p-someone-elses-bank"],
                |r| r.get(0),
            )
            .optional()
            .expect("query");
        assert_eq!(other, None);
    }

    /// Store with a local asset account mapped to Plaid account 'pa1' on item
    /// 'item1'. Returns (store, local_account_id).
    fn setup() -> (EventStore, String) {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let local = mk_account(&mut store, "1000", "Checking", AccountType::Asset);
        store
            .connection()
            .execute(
                "INSERT INTO plaid_items (id, proxy_item_id, institution_name) VALUES ('item1','p1','Bank')",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1','pa1','Checking','depository',?1)",
                [&local],
            )
            .unwrap();
        (store, local)
    }

    fn txn(id: &str, date: &str, amount: f64) -> SyncedTransaction {
        SyncedTransaction {
            transaction_id: id.to_string(),
            account_id: "pa1".to_string(),
            amount,
            date: date.to_string(),
            name: "Coffee".to_string(),
            merchant_name: None,
            pending: false,
            iso_currency_code: None,
            currency: None,
            payment_meta: None,
        }
    }

    fn count(store: &EventStore, sql: &str) -> i64 {
        store.connection().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn import_transactions_posts_entry_and_records_dedup() {
        let (mut store, _local) = setup();
        let (added, skipped) = PlaidCommands::new(&mut store, "u".to_string())
            .import_transactions("item1", &[txn("t1", "2026-03-04", 4.50)])
            .unwrap();
        assert_eq!((added, skipped), (1, 0));
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM journal_entries WHERE reference = 't1'"
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM plaid_imported_transactions WHERE plaid_transaction_id = 't1'"
            ),
            1,
            "the dedup row must be recorded in the same commit"
        );
    }

    #[test]
    fn import_transactions_skips_already_imported() {
        let (mut store, _local) = setup();
        let t = [txn("t1", "2026-03-04", 4.50)];
        PlaidCommands::new(&mut store, "u".to_string())
            .import_transactions("item1", &t)
            .unwrap();
        let (added, skipped) = PlaidCommands::new(&mut store, "u".to_string())
            .import_transactions("item1", &t)
            .unwrap();
        assert_eq!((added, skipped), (0, 1), "re-import is a no-op");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM journal_entries WHERE reference = 't1'"
            ),
            1,
            "no double-post"
        );
    }

    #[test]
    fn import_transactions_skips_transaction_in_closed_period() {
        let (mut store, _local) = setup();
        // Seed a closed fiscal period covering the transaction date. (There is no
        // command emitter for period-close yet, so seed the projection directly.)
        store
            .connection()
            .execute(
                "INSERT INTO fiscal_periods (year, period, start_date, end_date, status)
                 VALUES (2026, 3, '2026-03-01', '2026-03-31', 'closed')",
                [],
            )
            .unwrap();
        let (added, skipped) = PlaidCommands::new(&mut store, "u".to_string())
            .import_transactions("item1", &[txn("t1", "2026-03-04", 4.50)])
            .unwrap();
        assert_eq!((added, skipped), (0, 1), "closed-period txn is skipped");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 0);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM plaid_imported_transactions WHERE plaid_transaction_id = 't1'"
            ),
            0,
            "a skipped txn is not marked imported"
        );
    }

    #[test]
    fn import_transactions_skips_transaction_to_inactive_account() {
        let (mut store, local) = setup();
        AccountCommands::new(&mut store, "u".to_string())
            .deactivate_account(DeactivateAccountCommand {
                account_id: local,
                reason: None,
            })
            .unwrap();
        let (added, skipped) = PlaidCommands::new(&mut store, "u".to_string())
            .import_transactions("item1", &[txn("t1", "2026-03-04", 4.50)])
            .unwrap();
        assert_eq!((added, skipped), (0, 1), "inactive-account txn is skipped");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 0);
    }
}

/// Tests for the staging policy that the delegated (group) pull depends on.
///
/// Kept apart from the module's other tests because they are about one property:
/// a pull is destructive, so staging must not silently discard anything. The
/// provider has already moved this consumer's position in the stream by the time
/// these rows are written.
#[cfg(test)]
mod staging_policy_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::domain::AccountType;
    use crate::store::migrations::init_schema;
    use crate::store::EventStore;

    fn store_with_one_mapped_account() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let local = AccountCommands::new(&mut store, "u".to_string())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Asset,
                account_number: "1000".to_string(),
                name: "Checking".to_string(),
                parent_id: None,
                currency: None,
                description: None,
            })
            .unwrap();
        let local = match &local.event {
            Event::AccountCreated { account_id, .. } => account_id.clone(),
            _ => unreachable!(),
        };
        store
            .connection()
            .execute(
                "INSERT INTO plaid_items (id, proxy_item_id, institution_name)
                 VALUES ('item1','p1','Bank')",
                [],
            )
            .unwrap();
        // 'pa1' is mapped; 'pa2' is deliberately not.
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                     (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1','pa1','Checking','depository',?1)",
                [&local],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                     (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1','pa2','Savings','depository',NULL)",
                [],
            )
            .unwrap();
        store
    }

    fn txn_on(account: &str, id: &str) -> SyncedTransaction {
        SyncedTransaction {
            transaction_id: id.to_string(),
            account_id: account.to_string(),
            amount: 4.50,
            date: "2026-03-04".to_string(),
            name: "Coffee".to_string(),
            merchant_name: None,
            pending: false,
            iso_currency_code: None,
            currency: None,
            payment_meta: None,
        }
    }

    /// The regression that cost 728 transactions: a transaction for an unmapped
    /// bank account used to be dropped. The pull that fetched it does not happen
    /// twice, so dropping it means nobody ever sees it again.
    #[test]
    fn a_transaction_for_an_unmapped_account_is_kept_not_dropped() {
        let store = store_with_one_mapped_account();
        let outcome = stage_transactions_in_conn(
            store.connection(),
            "item1",
            &[txn_on("pa1", "t1"), txn_on("pa2", "t2")],
        )
        .unwrap();

        assert_eq!(outcome.staged, 2, "both must survive the pull");
        assert_eq!(outcome.unmapped, 1);
        assert_eq!(outcome.skipped(), 0);

        let held: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM plaid_staged_transactions
                 WHERE plaid_transaction_id = 't2' AND local_account_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(held, 1, "held for review, with no account guessed at");
    }

    /// Staging must not touch the ledger: on a group-hosted book there is no
    /// local write to make, and a pull is not a fact about the books.
    #[test]
    fn staging_appends_no_events() {
        let store = store_with_one_mapped_account();
        let before: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        stage_transactions_in_conn(store.connection(), "item1", &[txn_on("pa1", "t1")]).unwrap();
        let after: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after, "a replica must be able to run this");
    }

    /// Re-staging the same transactions is a no-op, and says so as duplicates
    /// rather than as a failure.
    #[test]
    fn the_same_transaction_twice_is_counted_as_a_duplicate() {
        let store = store_with_one_mapped_account();
        let t = [txn_on("pa1", "t1")];
        assert_eq!(
            stage_transactions_in_conn(store.connection(), "item1", &t)
                .unwrap()
                .staged,
            1
        );
        let second = stage_transactions_in_conn(store.connection(), "item1", &t).unwrap();
        assert_eq!((second.staged, second.duplicates), (0, 1));
    }

    /// Unsettled transactions are the one thing it is safe not to keep: the bank
    /// sends them again, as themselves, once they post.
    #[test]
    fn an_unsettled_transaction_is_left_for_the_bank_to_send_again() {
        let store = store_with_one_mapped_account();
        let mut t = txn_on("pa1", "t1");
        t.pending = true;
        let outcome = stage_transactions_in_conn(store.connection(), "item1", &[t]).unwrap();
        assert_eq!((outcome.staged, outcome.still_pending), (0, 1));
    }
}
