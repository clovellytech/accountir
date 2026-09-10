use crate::events::types::{
    Event, EventEnvelope, JournalEntrySource, JournalLineData, StoredEvent,
};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use chrono::NaiveDate;
use rusqlite::OptionalExtension;
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
    #[error("The books are closed for the year containing {0}")]
    YearClosed(NaiveDate),
    #[error(
        "This is the closing entry for {year}, and {year} is closed. Voiding it on its own would \
         leave the year locked with nothing to show what it earned, and no way to post the \
         correction. Reopen {year} from the Year end page instead — that voids this entry and \
         lifts the lock together."
    )]
    ClosingEntryFenced { year: i32 },
    #[error("An entry with reference {reference} already exists")]
    DuplicateReference {
        reference: String,
        existing_entry_id: String,
    },
    #[error(
        "Cannot unvoid: reference {reference} has been reclaimed by entry {existing_entry_id}"
    )]
    ReferenceReclaimed {
        reference: String,
        existing_entry_id: String,
    },
    #[error("Invalid data: {0}")]
    InvalidData(String),
}

/// Re-check a journal entry's *state-dependent* invariants inside an append
/// transaction: every referenced account exists and is active, and the entry's
/// date is not in a closed fiscal period. Returns `Some(err)` describing the
/// first violation, or `None` if all hold.
///
/// This is the reusable core of `post_entry`'s check, shared with composite
/// commands that emit a journal entry alongside other events (e.g. bill/invoice
/// payments) so they enforce the same fences inside their own append
/// transaction. Pure checks (debits==credits, line count) are the caller's
/// responsibility, done before the transaction.
pub(crate) fn check_entry_invariants_in_txn(
    tx: &rusqlite::Transaction<'_>,
    account_ids: &[&str],
    date: NaiveDate,
) -> Result<Option<EntryCommandError>, EventStoreError> {
    for account_id in account_ids {
        let active: Option<bool> = tx
            .query_row(
                "SELECT is_active = 1 FROM accounts WHERE id = ?1",
                [account_id],
                |row| row.get(0),
            )
            .optional()?;
        match active {
            Some(true) => {}
            Some(false) => {
                return Ok(Some(EntryCommandError::AccountInactive(
                    account_id.to_string(),
                )))
            }
            None => {
                return Ok(Some(EntryCommandError::AccountNotFound(
                    account_id.to_string(),
                )))
            }
        }
    }

    // The closed-year fence. A date in no fiscal year at all is unfenced —
    // `fiscal_years` only has rows for years someone opened, and a ledger that
    // has never closed anything should not be refusing entries.
    let year_closed: bool = tx
        .query_row(
            "SELECT is_closed = 1 FROM fiscal_years
             WHERE ?1 BETWEEN start_date AND end_date",
            [date.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if year_closed {
        return Ok(Some(EntryCommandError::YearClosed(date)));
    }

    Ok(None)
}

/// The fiscal year this entry closes, if it is a live closing entry and that
/// year is still closed. `None` for every ordinary entry.
fn closing_entry_in_a_closed_year(
    tx: &rusqlite::Transaction<'_>,
    entry_id: &str,
) -> Result<Option<i32>, EventStoreError> {
    Ok(tx
        .query_row(
            "SELECT fy.year FROM journal_entries je
               JOIN fiscal_years fy
                 ON je.date BETWEEN fy.start_date AND fy.end_date
              WHERE je.id = ?1 AND je.source = 'closing' AND fy.is_closed = 1",
            [entry_id],
            |r| r.get::<_, i32>(0),
        )
        .optional()?)
}

/// Re-check, inside an append transaction, that a journal entry exists and is not
/// already voided — the `void_entry` invariant. Returns `Some(err)` (`NotFound`
/// or `AlreadyVoided`) or `None` if it is safe to void. Shared with composite
/// commands that void an entry alongside another event (invoice/bill void).
pub(crate) fn check_entry_not_voided_in_txn(
    tx: &rusqlite::Transaction<'_>,
    entry_id: &str,
) -> Result<Option<EntryCommandError>, EventStoreError> {
    let is_void: Option<i32> = tx
        .query_row(
            "SELECT is_void FROM journal_entries WHERE id = ?1",
            [entry_id],
            |row| row.get(0),
        )
        .optional()?;
    match is_void {
        None => Ok(Some(EntryCommandError::NotFound(entry_id.to_string()))),
        Some(1) => Ok(Some(EntryCommandError::AlreadyVoided)),
        Some(_) => Ok(None),
    }
}

/// Re-check, inside an append transaction, that `reference` is not already in
/// use by a live (non-voided) journal entry — the ingest idempotency guard.
/// Returns `Some(existing_entry_id)` if the reference is taken (so the caller
/// can treat the command as a duplicate), or `None` if it is free. Mirrors the
/// pre-transaction [`check_idempotent`](crate::commands::ingest_commands::check_idempotent)
/// read, but under the write lock, so two concurrent imports of the same source
/// event can't both pass. The partial unique index
/// `idx_journal_entries_reference_unique` is the DB-level backstop.
pub(crate) fn check_reference_free_in_txn(
    tx: &rusqlite::Transaction<'_>,
    reference: &str,
) -> Result<Option<String>, EventStoreError> {
    Ok(tx
        .query_row(
            "SELECT id FROM journal_entries WHERE reference = ?1 AND is_void = 0",
            [reference],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// One note somebody added to an entry, newest last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub annotation: String,
    /// The event that recorded it — also the order they were made in.
    pub event_id: i64,
    pub created_at: String,
}

/// Every note on one entry, oldest first.
///
/// All of them, not just the latest. A note that was corrected is part of the
/// record — the reader who needs to know a cheque was described as one thing and
/// later as another is the reader this exists for. Callers wanting one line to
/// print take [`latest_annotation`].
pub fn annotations_for(conn: &rusqlite::Connection, entry_id: &str) -> Vec<Annotation> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT annotation, event_id, created_at
           FROM journal_entry_annotations
          WHERE entry_id = ?1
          ORDER BY event_id",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([entry_id], |r| {
        Ok(Annotation {
            annotation: r.get(0)?,
            event_id: r.get(1)?,
            created_at: r.get(2)?,
        })
    });
    rows.map(|r| r.flatten().collect()).unwrap_or_default()
}

/// The most recent note on an entry, which is what a one-line view shows.
pub fn latest_annotation(conn: &rusqlite::Connection, entry_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT annotation FROM journal_entry_annotations
          WHERE entry_id = ?1 ORDER BY event_id DESC LIMIT 1",
        [entry_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// The latest note for each of many entries, for a list that shows one per row.
///
/// One query rather than one per entry: the journal renders hundreds of rows and
/// a per-row lookup is what makes a list scroll badly.
pub fn latest_annotations(
    conn: &rusqlite::Connection,
    entry_ids: &[String],
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if entry_ids.is_empty() {
        return out;
    }
    // Built rather than bound in one go because rusqlite has no array binding;
    // the ids are uuids this crate minted, and `?` placeholders keep them out of
    // the SQL text regardless.
    let placeholders = std::iter::repeat_n("?", entry_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT entry_id, annotation FROM journal_entry_annotations
          WHERE event_id IN (
                SELECT MAX(event_id) FROM journal_entry_annotations
                 WHERE entry_id IN ({placeholders}) GROUP BY entry_id)"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return out;
    };
    let params = rusqlite::params_from_iter(entry_ids.iter());
    if let Ok(rows) = stmt.query_map(params, |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    }) {
        for (id, note) in rows.flatten() {
            out.insert(id, note);
        }
    }
    out
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
    /// A debit of `amount`, which must not be negative.
    ///
    /// The `abs()` is a normalisation for callers that know the side they want
    /// and hold the figure with whatever sign a report printed it. It is *not*
    /// a way to pass a signed figure: a negative here silently becomes a debit
    /// of the magnitude rather than the credit the caller meant, which unbalances
    /// the entry. Use [`EntryLine::signed`] when the sign is data.
    pub fn debit(account_id: &str, amount: i64, currency: &str) -> Self {
        debug_assert!(
            amount >= 0,
            "debit({account_id}, {amount}) — a negative debit is a credit, and `abs()` will not \
             make it one. Use EntryLine::signed when the sign comes from the data."
        );
        Self {
            account_id: account_id.to_string(),
            amount: amount.abs(),
            currency: currency.to_string(),
            exchange_rate: None,
            memo: None,
        }
    }

    /// A credit of `amount`, which must not be negative. See [`EntryLine::debit`].
    pub fn credit(account_id: &str, amount: i64, currency: &str) -> Self {
        debug_assert!(
            amount >= 0,
            "credit({account_id}, {amount}) — a negative credit is a debit, and `abs()` will not \
             make it one. Use EntryLine::signed when the sign comes from the data."
        );
        Self {
            account_id: account_id.to_string(),
            amount: -amount.abs(),
            currency: currency.to_string(),
            exchange_rate: None,
            memo: None,
        }
    }

    /// A line whose side is carried by the sign: positive debits, negative
    /// credits.
    ///
    /// For figures that come from data rather than from the caller's intent. A
    /// month of Square sales can have negative tips (a tip refunded), negative
    /// net deposits (more redeemed than taken) and a refunds column that is
    /// always negative — and each of those has to flip the side of its line, not
    /// just its magnitude.
    ///
    /// The convention for a credit-natured figure is `signed(account, -x)`: it
    /// credits when `x` is positive and debits when it is not, which is what
    /// keeps an entry balanced whatever the report says. A zero-amount line is
    /// still produced; callers that do not want one drop it themselves.
    pub fn signed(account_id: &str, amount: i64, currency: &str) -> Self {
        Self {
            account_id: account_id.to_string(),
            amount,
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

/// Pure (state-independent) validation for a journal entry: at least two lines
/// and debits == credits. Run before opening the append transaction.
pub(crate) fn check_entry_pure(cmd: &PostEntryCommand) -> Result<(), EntryCommandError> {
    if cmd.lines.len() < 2 {
        return Err(EntryCommandError::InsufficientLines);
    }
    // Fold with checked_add: line amounts are client-supplied over the sync
    // transport, so an unchecked `.sum()` could overflow — wrapping past a false
    // balance of 0 in release, or panicking in debug. Reject overflow outright.
    let sum = cmd
        .lines
        .iter()
        .try_fold(0i64, |acc, l| acc.checked_add(l.amount))
        .ok_or_else(|| {
            EntryCommandError::InvalidData("line amount sum overflows i64".to_string())
        })?;
    if sum != 0 {
        return Err(EntryCommandError::NotBalanced(sum));
    }
    Ok(())
}

/// Outcome of the in-txn journal-entry validation.
pub(crate) enum PostEntryStep {
    /// All invariants hold under the write lock; append this event.
    Append(Event),
    /// A domain invariant was violated.
    Reject(EntryCommandError),
}

/// Run a journal entry's state-dependent invariants inside the append
/// transaction — reference idempotency, then accounts-active / period-open fences
/// — and, if they hold, build the `JournalEntryPosted` event. Shared by
/// [`EntryCommands::post_entry`] and the server-side sync submit path so both
/// enforce the SAME invariants under the write lock; the caller wraps the event
/// in an envelope (stamping identity as appropriate). Pure checks are the
/// caller's responsibility ([`check_entry_pure`]).
/// The outcome of a reassignment's in-transaction fences. Same shape as
/// [`PostEntryStep`] and kept separate for the same reason its name is specific:
/// an enum called "post entry" appearing in a reassignment reads as a mistake.
pub(crate) enum ReassignLineStep {
    Append(Event),
    Reject(EntryCommandError),
}

/// Run a reassignment's state-dependent guards inside the append transaction and,
/// if they hold, build the `JournalLineReassigned` event.
///
/// Shared by [`EntryCommands::reassign_line`] and the server-side sync path, so a
/// member on group-hosted books is held to exactly the same fences as a standalone
/// ledger: the entry exists and is live, the line exists, and the *target* account
/// exists and is active — that last one under the write lock, closing the TOCTOU
/// where a concurrent writer deactivates it between check and append.
pub(crate) fn build_reassign_line_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ReassignLineCommand,
) -> Result<ReassignLineStep, EventStoreError> {
    // Entry exists and is not voided.
    let is_void: Option<bool> = tx
        .query_row(
            "SELECT is_void = 1 FROM journal_entries WHERE id = ?1",
            [&cmd.entry_id],
            |row| row.get(0),
        )
        .optional()?;
    match is_void {
        None => {
            return Ok(ReassignLineStep::Reject(EntryCommandError::NotFound(
                cmd.entry_id.clone(),
            )))
        }
        Some(true) => return Ok(ReassignLineStep::Reject(EntryCommandError::AlreadyVoided)),
        Some(false) => {}
    }

    // Line exists — read its current account.
    let old_account_id: Option<String> = tx
        .query_row(
            "SELECT account_id FROM journal_lines WHERE id = ?1 AND entry_id = ?2",
            [&cmd.line_id, &cmd.entry_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(old_account_id) = old_account_id else {
        return Ok(ReassignLineStep::Reject(EntryCommandError::NotFound(
            format!("Line {} in entry {}", cmd.line_id, cmd.entry_id),
        )));
    };

    // New account exists and is active — checked under the write lock so a
    // concurrent deactivation can't slip in.
    let new_account_active: Option<bool> = tx
        .query_row(
            "SELECT is_active = 1 FROM accounts WHERE id = ?1",
            [&cmd.new_account_id],
            |row| row.get(0),
        )
        .optional()?;
    match new_account_active {
        None => {
            return Ok(ReassignLineStep::Reject(
                EntryCommandError::AccountNotFound(cmd.new_account_id.clone()),
            ))
        }
        Some(false) => {
            return Ok(ReassignLineStep::Reject(
                EntryCommandError::AccountInactive(cmd.new_account_id.clone()),
            ))
        }
        Some(true) => {}
    }

    // No-op if the account isn't changing.
    if old_account_id == cmd.new_account_id {
        return Ok(ReassignLineStep::Reject(EntryCommandError::InvalidData(
            "New account is the same as current account".to_string(),
        )));
    }

    Ok(ReassignLineStep::Append(Event::JournalLineReassigned {
        entry_id: cmd.entry_id.clone(),
        line_id: cmd.line_id.clone(),
        old_account_id,
        new_account_id: cmd.new_account_id.clone(),
    }))
}

pub(crate) fn build_post_entry_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &PostEntryCommand,
) -> Result<PostEntryStep, EventStoreError> {
    // Idempotency: a live entry with this reference already exists ⇒ duplicate.
    if let Some(ref reference) = cmd.reference {
        if let Some(existing_entry_id) = check_reference_free_in_txn(tx, reference)? {
            return Ok(PostEntryStep::Reject(
                EntryCommandError::DuplicateReference {
                    reference: reference.clone(),
                    existing_entry_id,
                },
            ));
        }
    }

    // State-dependent fences (accounts active, period open), under the write lock.
    let account_ids: Vec<&str> = cmd.lines.iter().map(|l| l.account_id.as_str()).collect();
    if let Some(e) = check_entry_invariants_in_txn(tx, &account_ids, cmd.date)? {
        return Ok(PostEntryStep::Reject(e));
    }

    // Invariants hold — build the event.
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
    Ok(PostEntryStep::Append(Event::JournalEntryPosted {
        entry_id,
        date: cmd.date,
        memo: cmd.memo.clone(),
        lines,
        reference: cmd.reference.clone(),
        source: cmd.source.clone(),
    }))
}

/// Re-check the `void_entry` invariant inside the append transaction — the entry
/// exists and is not already voided — and, if it holds, build the
/// `JournalEntryVoided` event. Mirrors [`build_post_entry_in_txn`]: returns the
/// raw [`Event`] so the caller stamps identity as appropriate. Shared by
/// [`EntryCommands::void_entry`] and the server-side sync submit path so both
/// enforce the SAME invariant under the write lock.
pub(crate) fn build_void_entry_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &VoidEntryCommand,
) -> Result<PostEntryStep, EventStoreError> {
    if let Some(e) = check_entry_not_voided_in_txn(tx, &cmd.entry_id)? {
        return Ok(PostEntryStep::Reject(e));
    }
    // Voiding a closing entry while its year is still closed reaches a state
    // nothing can get out of by ordinary means: the balances come back, the
    // fence stays up, and the entry that would put them away again cannot be
    // posted. Reopening is the operation that does both, in one append.
    //
    // `closing_commands::reopen_books` builds its void event directly rather
    // than through here, for exactly this reason — it is lifting the same fence
    // in the same transaction.
    if let Some(year) = closing_entry_in_a_closed_year(tx, &cmd.entry_id)? {
        return Ok(PostEntryStep::Reject(
            EntryCommandError::ClosingEntryFenced { year },
        ));
    }
    Ok(PostEntryStep::Append(Event::JournalEntryVoided {
        entry_id: cmd.entry_id.clone(),
        reason: cmd.reason.clone(),
    }))
}

/// Re-check the `unvoid_entry` invariant inside the append transaction — the
/// entry exists AND is currently voided — plus the reference-reclamation guard
/// (reject `ReferenceReclaimed` if a *different* live entry now holds the entry's
/// reference), and, if all hold, build the `JournalEntryUnvoided` event. Mirrors
/// [`build_post_entry_in_txn`]: returns the raw [`Event`] so the caller stamps
/// identity. Shared by [`EntryCommands::unvoid_entry`] and the server-side sync
/// submit path so both enforce the SAME invariants under the write lock. See
/// [`EntryCommands::unvoid_entry`] for why the reference guard must run in-txn.
pub(crate) fn build_unvoid_entry_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &UnvoidEntryCommand,
) -> Result<PostEntryStep, EventStoreError> {
    let entry: Option<(i32, Option<String>)> = tx
        .query_row(
            "SELECT is_void, reference FROM journal_entries WHERE id = ?1",
            [&cmd.entry_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let reference = match entry {
        None => {
            return Ok(PostEntryStep::Reject(EntryCommandError::NotFound(
                cmd.entry_id.clone(),
            )))
        }
        Some((0, _)) => return Ok(PostEntryStep::Reject(EntryCommandError::NotVoided)),
        Some((_, reference)) => reference,
    };

    // Reference-reclamation guard: if this entry carried a reference and a
    // *different* live entry has since claimed it, unvoiding would re-take the
    // reference and violate the partial unique index at projection time. Reject
    // cleanly instead.
    if let Some(ref reference) = reference {
        if let Some(existing_entry_id) = check_reference_free_in_txn(tx, reference)? {
            if existing_entry_id != cmd.entry_id {
                return Ok(PostEntryStep::Reject(
                    EntryCommandError::ReferenceReclaimed {
                        reference: reference.clone(),
                        existing_entry_id,
                    },
                ));
            }
        }
    }

    Ok(PostEntryStep::Append(Event::JournalEntryUnvoided {
        entry_id: cmd.entry_id.clone(),
        reason: cmd.reason.clone(),
    }))
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

    /// Post a new journal entry.
    ///
    /// The state-dependent invariants (every referenced account exists and is
    /// active; the entry's date is not in a closed fiscal period) are checked
    /// *inside* the append transaction via [`EventStore::append_checked`], so a
    /// concurrent writer cannot deactivate an account or close the period between
    /// the check and the append (the read-then-append TOCTOU, SPEC §6.2). If the
    /// log head moves under us we retry the whole command against fresh state.
    pub fn post_entry(&mut self, cmd: PostEntryCommand) -> Result<StoredEvent, EntryCommandError> {
        // Pure validation (independent of ledger state) — do it once up front.
        check_entry_pure(&cmd)?;

        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_post_entry_in_txn(tx, &cmd)? {
                    PostEntryStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    PostEntryStep::Reject(e) => Ok(Verdict::Reject(e)),
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

    /// Void an existing journal entry.
    ///
    /// The "exists and not already voided" invariant is re-checked inside the
    /// append transaction via [`EventStore::append_checked`], so two concurrent
    /// voids can't both pass and double-void. Retries on a head move.
    pub fn void_entry(&mut self, cmd: VoidEntryCommand) -> Result<StoredEvent, EntryCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_void_entry_in_txn(tx, &cmd)? {
                    PostEntryStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    PostEntryStep::Reject(e) => Ok(Verdict::Reject(e)),
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

    /// Unvoid a voided journal entry.
    ///
    /// The "exists and is currently voided" invariant (the inverse of
    /// `void_entry`) is re-checked inside the append transaction via
    /// [`EventStore::append_checked`], so a concurrent unvoid can't double-unvoid
    /// and an entry that was concurrently unvoided is rejected `NotVoided`.
    /// Retries on a head move.
    ///
    /// It also guards the *reference-reclamation split-brain*: voiding an entry
    /// frees its journal reference (the partial unique index
    /// `idx_journal_entries_reference_unique` only covers live rows), so another
    /// live entry may legitimately claim that reference while this one is voided.
    /// Unvoiding flips `is_void` back to 0 and RE-TAKES the reference — which
    /// would hit the UNIQUE index at projection time. Under the old
    /// append-then-project path the event committed before the projection failed,
    /// leaving a permanent log/projection split-brain (and breaking
    /// `rebuild_projections`). Checking `check_reference_free_in_txn` under the
    /// write lock turns that into a clean `ReferenceReclaimed` rejection with
    /// nothing appended.
    pub fn unvoid_entry(
        &mut self,
        cmd: UnvoidEntryCommand,
    ) -> Result<StoredEvent, EntryCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_unvoid_entry_in_txn(tx, &cmd)? {
                    PostEntryStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    PostEntryStep::Reject(e) => Ok(Verdict::Reject(e)),
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

    /// Add an annotation to a journal entry.
    ///
    /// The "entry exists" guard is re-checked inside the append transaction via
    /// [`EventStore::append_checked`], so an annotation can't be appended against
    /// an entry that never existed under the write lock. Retries on a head move.
    pub fn annotate_entry(
        &mut self,
        cmd: AnnotateEntryCommand,
    ) -> Result<StoredEvent, EntryCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| {
                    let exists: bool = tx
                        .query_row(
                            "SELECT 1 FROM journal_entries WHERE id = ?1",
                            [&cmd.entry_id],
                            |_| Ok(true),
                        )
                        .optional()?
                        .unwrap_or(false);
                    if !exists {
                        return Ok(Verdict::Reject(EntryCommandError::NotFound(
                            cmd.entry_id.clone(),
                        )));
                    }

                    let event = Event::JournalEntryAnnotated {
                        entry_id: cmd.entry_id.clone(),
                        annotation: cmd.annotation.clone(),
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

    /// Reassign a journal line to a different account.
    ///
    /// All state-dependent guards are re-checked inside the append transaction
    /// via [`EventStore::append_checked`]: the entry exists and is not voided, the
    /// line exists (yielding its current account), and — critically — the *new*
    /// target account exists and is active. Moving the new-account-active check
    /// under the write lock closes the read-then-append TOCTOU where a concurrent
    /// writer deactivates the target account between the check and the append.
    /// Retries on a head move.
    pub fn reassign_line(
        &mut self,
        cmd: ReassignLineCommand,
    ) -> Result<StoredEvent, EntryCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked(
                head,
                |tx| match build_reassign_line_in_txn(tx, &cmd)? {
                    ReassignLineStep::Append(event) => {
                        Ok(Verdict::Append(EventEnvelope::new(event, user_id.clone())))
                    }
                    ReassignLineStep::Reject(e) => Ok(Verdict::Reject(e)),
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

    /// Account ids (expense 5000, cash 1000) for a store seeded by
    /// `create_test_accounts`.
    fn test_account_ids(store: &EventStore) -> (String, String) {
        let expense: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '5000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let cash: String = store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = '1000'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (expense, cash)
    }

    fn dup_cmd(expense: &str, cash: &str, reference: &str) -> PostEntryCommand {
        PostEntryCommand {
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "dup".to_string(),
            lines: vec![
                EntryLine::debit(expense, 10000, "USD"),
                EntryLine::credit(cash, 10000, "USD"),
            ],
            reference: Some(reference.to_string()),
            source: Some(JournalEntrySource::Manual),
        }
    }

    #[test]
    fn post_entry_rejects_duplicate_reference() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let (expense, cash) = test_account_ids(&store);

        let first = EntryCommands::new(&mut store, "user".to_string())
            .post_entry(dup_cmd(&expense, &cash, "REF-1"))
            .unwrap();
        let first_id = match &first.event {
            Event::JournalEntryPosted { entry_id, .. } => entry_id.clone(),
            _ => panic!("expected JournalEntryPosted"),
        };

        let before = store.count().unwrap();
        let err = EntryCommands::new(&mut store, "user".to_string())
            .post_entry(dup_cmd(&expense, &cash, "REF-1"))
            .unwrap_err();

        match err {
            EntryCommandError::DuplicateReference {
                reference,
                existing_entry_id,
            } => {
                assert_eq!(reference, "REF-1");
                assert_eq!(existing_entry_id, first_id, "must point at the first entry");
            }
            other => panic!("expected DuplicateReference, got {other:?}"),
        }
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected duplicate appends nothing"
        );
        let entries_with_ref: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM journal_entries WHERE reference = 'REF-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(entries_with_ref, 1, "only one entry may hold the reference");
    }

    #[test]
    fn concurrent_post_same_reference_cannot_double_post() {
        // The ingest ref-dedup race: two imports of the same source event (same
        // reference) race across two connections / one WAL file. The in-txn
        // idempotency check + head-CAS retry + the partial unique index let
        // exactly one entry land; the other is rejected DuplicateReference.
        let dir = std::env::temp_dir().join(format!("accountir-refdedup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("log.db");
        let (expense, cash) = {
            let mut store = EventStore::open(&db).unwrap();
            init_schema(store.connection()).unwrap();
            create_test_accounts(&mut store);
            test_account_ids(&store)
        };

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn = |db: std::path::PathBuf,
                     expense: String,
                     cash: String,
                     barrier: std::sync::Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                let mut store = EventStore::open(&db).unwrap();
                let mut cmds = EntryCommands::new(&mut store, "user".to_string());
                barrier.wait();
                cmds.post_entry(dup_cmd(&expense, &cash, "SRC-EVT-1"))
            })
        };

        let t1 = spawn(db.clone(), expense.clone(), cash.clone(), barrier.clone());
        let t2 = spawn(db.clone(), expense.clone(), cash.clone(), barrier.clone());
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(oks, 1, "exactly one import may post (r1={r1:?}, r2={r2:?})");
        assert!(
            [&r1, &r2]
                .iter()
                .any(|r| matches!(r, Err(EntryCommandError::DuplicateReference { .. }))),
            "the loser must be rejected as a duplicate (r1={r1:?}, r2={r2:?})"
        );

        let store = EventStore::open(&db).unwrap();
        let entries_with_ref: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM journal_entries WHERE reference = 'SRC-EVT-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(entries_with_ref, 1, "no double-post");
        std::fs::remove_dir_all(&dir).ok();
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
    fn post_entry_rejects_amount_overflow() {
        // Two i64::MIN lines wrap to a false balance of 0 under an unchecked sum
        // (release) or panic (debug); checked_add must reject them instead.
        let mut store = setup();
        create_test_accounts(&mut store);
        let (expense_id, cash_id) = test_account_ids(&store);
        let before = store.count().unwrap();
        let cmd = PostEntryCommand {
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "overflow".to_string(),
            lines: vec![
                EntryLine {
                    account_id: expense_id,
                    amount: i64::MIN,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                EntryLine {
                    account_id: cash_id,
                    amount: i64::MIN,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        let result = EntryCommands::new(&mut store, "user".to_string()).post_entry(cmd);
        assert!(
            matches!(result, Err(EntryCommandError::InvalidData(_))),
            "got {result:?}"
        );
        assert_eq!(
            store.count().unwrap(),
            before,
            "nothing appended on overflow"
        );
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

    /// Post a simple balanced entry (expense/cash) and return its entry_id.
    fn post_simple_entry(store: &mut EventStore) -> String {
        let (expense, cash) = test_account_ids(store);
        let stored = EntryCommands::new(store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
                memo: "e".to_string(),
                lines: vec![
                    EntryLine::debit(&expense, 10000, "USD"),
                    EntryLine::credit(&cash, 10000, "USD"),
                ],
                reference: None,
                source: None,
            })
            .unwrap();
        match stored.event {
            Event::JournalEntryPosted { entry_id, .. } => entry_id,
            _ => panic!("expected JournalEntryPosted"),
        }
    }

    #[test]
    fn unvoid_entry_happy_path() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let entry_id = post_simple_entry(&mut store);

        EntryCommands::new(&mut store, "user".to_string())
            .void_entry(VoidEntryCommand {
                entry_id: entry_id.clone(),
                reason: "oops".to_string(),
            })
            .unwrap();

        EntryCommands::new(&mut store, "user".to_string())
            .unvoid_entry(UnvoidEntryCommand {
                entry_id: entry_id.clone(),
                reason: "restore".to_string(),
            })
            .unwrap();

        let is_void: i32 = store
            .connection()
            .query_row(
                "SELECT is_void FROM journal_entries WHERE id = ?1",
                [&entry_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_void, 0, "entry should be live again after unvoid");
    }

    #[test]
    fn unvoid_of_non_voided_entry_rejected_appends_nothing() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let entry_id = post_simple_entry(&mut store);

        let before = store.count().unwrap();
        let err = EntryCommands::new(&mut store, "user".to_string())
            .unvoid_entry(UnvoidEntryCommand {
                entry_id,
                reason: "nope".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, EntryCommandError::NotVoided));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected unvoid appends nothing"
        );
    }

    #[test]
    fn unvoid_rejected_when_reference_reclaimed_appends_nothing() {
        // Post A with reference R, void A (frees R), post B with the same
        // reference R (now allowed). Unvoiding A would re-take R and violate the
        // partial unique index — it must be rejected ReferenceReclaimed with
        // nothing appended, not left as a log/projection split-brain.
        let mut store = setup();
        create_test_accounts(&mut store);
        let (expense, cash) = test_account_ids(&store);

        let a_id = {
            let stored = EntryCommands::new(&mut store, "user".to_string())
                .post_entry(dup_cmd(&expense, &cash, "R"))
                .unwrap();
            match stored.event {
                Event::JournalEntryPosted { entry_id, .. } => entry_id,
                _ => panic!("expected JournalEntryPosted"),
            }
        };

        EntryCommands::new(&mut store, "user".to_string())
            .void_entry(VoidEntryCommand {
                entry_id: a_id.clone(),
                reason: "free the ref".to_string(),
            })
            .unwrap();

        // B legitimately claims R now that A is voided.
        EntryCommands::new(&mut store, "user".to_string())
            .post_entry(dup_cmd(&expense, &cash, "R"))
            .unwrap();

        let before = store.count().unwrap();
        let err = EntryCommands::new(&mut store, "user".to_string())
            .unvoid_entry(UnvoidEntryCommand {
                entry_id: a_id.clone(),
                reason: "bring A back".to_string(),
            })
            .unwrap_err();
        match err {
            EntryCommandError::ReferenceReclaimed { reference, .. } => {
                assert_eq!(reference, "R");
            }
            other => panic!("expected ReferenceReclaimed, got {other:?}"),
        }
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected unvoid appends nothing"
        );
        // A is still voided.
        let is_void: i32 = store
            .connection()
            .query_row(
                "SELECT is_void FROM journal_entries WHERE id = ?1",
                [&a_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_void, 1, "A remains voided after the rejected unvoid");
    }

    /// The note has to be readable afterwards.
    ///
    /// This test used to stop at `.unwrap()`, and passed for as long as the
    /// projection for this event was an empty block with `// For now, we'll skip
    /// this` in it. A test that asserts a write returned Ok and never that the
    /// thing written can be read back is a test that cannot fail for the reason
    /// it exists.
    #[test]
    fn an_annotation_can_be_read_back() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let entry_id = post_simple_entry(&mut store);

        EntryCommands::new(&mut store, "user".to_string())
            .annotate_entry(AnnotateEntryCommand {
                entry_id: entry_id.clone(),
                annotation: "January rent — cheque to the landlord".to_string(),
            })
            .unwrap();

        let notes = annotations_for(store.connection(), &entry_id);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].annotation, "January rent — cheque to the landlord");
        assert_eq!(
            latest_annotation(store.connection(), &entry_id).as_deref(),
            Some("January rent — cheque to the landlord")
        );
    }

    /// Every note is kept, oldest first, and the latest is what a one-line view
    /// shows. A corrected note is part of the record rather than a replacement
    /// for one.
    #[test]
    fn a_corrected_note_keeps_the_one_it_corrected() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let entry_id = post_simple_entry(&mut store);

        for note in [
            "Rent?",
            "January rent",
            "January rent — landlord, cheque 1234",
        ] {
            EntryCommands::new(&mut store, "user".to_string())
                .annotate_entry(AnnotateEntryCommand {
                    entry_id: entry_id.clone(),
                    annotation: note.to_string(),
                })
                .unwrap();
        }

        let notes = annotations_for(store.connection(), &entry_id);
        assert_eq!(notes.len(), 3, "an earlier note was overwritten");
        assert_eq!(notes[0].annotation, "Rent?");
        assert_eq!(
            latest_annotation(store.connection(), &entry_id).as_deref(),
            Some("January rent — landlord, cheque 1234")
        );
    }

    /// The imported memo is what the bank said and is never touched by a note.
    /// It is how the entry matches the statement and how a re-import is spotted
    /// as a duplicate, so a note sits beside it rather than over it.
    #[test]
    fn annotating_leaves_the_banks_own_memo_alone() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let entry_id = post_simple_entry(&mut store);

        let memo_before: String = store
            .connection()
            .query_row(
                "SELECT memo FROM journal_entries WHERE id = ?1",
                [&entry_id],
                |r| r.get(0),
            )
            .unwrap();

        EntryCommands::new(&mut store, "user".to_string())
            .annotate_entry(AnnotateEntryCommand {
                entry_id: entry_id.clone(),
                annotation: "January rent".to_string(),
            })
            .unwrap();

        let memo_after: String = store
            .connection()
            .query_row(
                "SELECT memo FROM journal_entries WHERE id = ?1",
                [&entry_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(memo_before, memo_after);
    }

    /// The journal renders hundreds of rows, so the list view reads every
    /// entry's latest note in one query rather than one per row.
    #[test]
    fn the_latest_note_for_many_entries_comes_back_in_one_lookup() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let a = post_simple_entry(&mut store);
        let b = post_simple_entry(&mut store);
        let c = post_simple_entry(&mut store);

        for (id, note) in [(&a, "first"), (&a, "second"), (&b, "only")] {
            EntryCommands::new(&mut store, "user".to_string())
                .annotate_entry(AnnotateEntryCommand {
                    entry_id: id.clone(),
                    annotation: note.to_string(),
                })
                .unwrap();
        }

        let ids = vec![a.clone(), b.clone(), c.clone()];
        let map = latest_annotations(store.connection(), &ids);
        assert_eq!(map.get(&a).map(String::as_str), Some("second"));
        assert_eq!(map.get(&b).map(String::as_str), Some("only"));
        assert!(!map.contains_key(&c), "an entry with no note has no row");
    }

    #[test]
    fn annotate_missing_entry_rejected_appends_nothing() {
        let mut store = setup();
        create_test_accounts(&mut store);

        let before = store.count().unwrap();
        let err = EntryCommands::new(&mut store, "user".to_string())
            .annotate_entry(AnnotateEntryCommand {
                entry_id: "does-not-exist".to_string(),
                annotation: "x".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, EntryCommandError::NotFound(_)));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected annotation appends nothing"
        );
    }

    #[test]
    fn reassign_line_happy_path() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let entry_id = post_simple_entry(&mut store);
        let (expense, _cash) = test_account_ids(&store);

        // Create a third account to reassign a line to.
        let other = {
            let stored = AccountCommands::new(&mut store, "user".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Expense,
                    account_number: "5001".to_string(),
                    name: "Other Supplies".to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
            match stored.event {
                Event::AccountCreated { account_id, .. } => account_id,
                _ => panic!("expected AccountCreated"),
            }
        };

        // The expense line is the first line: "{entry_id}-line-1".
        let line_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM journal_lines WHERE entry_id = ?1 AND account_id = ?2",
                [&entry_id, &expense],
                |r| r.get(0),
            )
            .unwrap();

        EntryCommands::new(&mut store, "user".to_string())
            .reassign_line(ReassignLineCommand {
                entry_id: entry_id.clone(),
                line_id: line_id.clone(),
                new_account_id: other.clone(),
            })
            .unwrap();

        let acct: String = store
            .connection()
            .query_row(
                "SELECT account_id FROM journal_lines WHERE id = ?1",
                [&line_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(acct, other, "line should now point at the new account");
    }

    #[test]
    fn reassign_to_inactive_account_rejected_appends_nothing() {
        let mut store = setup();
        create_test_accounts(&mut store);
        let entry_id = post_simple_entry(&mut store);
        let (expense, _cash) = test_account_ids(&store);

        // Create a target account, then deactivate it.
        let target = {
            let stored = AccountCommands::new(&mut store, "user".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Expense,
                    account_number: "5001".to_string(),
                    name: "Dead Account".to_string(),
                    parent_id: None,
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
            match stored.event {
                Event::AccountCreated { account_id, .. } => account_id,
                _ => panic!("expected AccountCreated"),
            }
        };
        store
            .connection()
            .execute("UPDATE accounts SET is_active = 0 WHERE id = ?1", [&target])
            .unwrap();

        let line_id: String = store
            .connection()
            .query_row(
                "SELECT id FROM journal_lines WHERE entry_id = ?1 AND account_id = ?2",
                [&entry_id, &expense],
                |r| r.get(0),
            )
            .unwrap();

        let before = store.count().unwrap();
        let err = EntryCommands::new(&mut store, "user".to_string())
            .reassign_line(ReassignLineCommand {
                entry_id,
                line_id: line_id.clone(),
                new_account_id: target,
            })
            .unwrap_err();
        assert!(matches!(err, EntryCommandError::AccountInactive(_)));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected reassignment appends nothing"
        );

        // The line still points at the original expense account.
        let acct: String = store
            .connection()
            .query_row(
                "SELECT account_id FROM journal_lines WHERE id = ?1",
                [&line_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(acct, expense);
    }
}
