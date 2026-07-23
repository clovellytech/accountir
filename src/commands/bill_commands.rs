use crate::commands::entry_commands::{
    check_entry_invariants_in_txn, check_entry_not_voided_in_txn, check_reference_free_in_txn,
};
use crate::domain::PaymentTerms;
use crate::events::types::{
    Event, EventEnvelope, JournalEntrySource, JournalLineData, StoredEvent,
};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::{ProjectionError, Projector};
use chrono::NaiveDate;
use rusqlite::OptionalExtension;
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum BillCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Projection error: {0}")]
    ProjectionError(#[from] ProjectionError),
    #[error("Entry command error: {0}")]
    EntryCommandError(#[from] crate::commands::entry_commands::EntryCommandError),
    #[error("Bill not found: {0}")]
    NotFound(String),
    #[error("Bill is voided")]
    Voided,
    #[error("Bill is already fully paid")]
    AlreadyPaid,
    #[error("Payment amount {0} exceeds remaining balance {1}")]
    OverPayment(i64, i64),
    #[error("Cannot void a bill with payments applied")]
    HasPayments,
    #[error("A bill entry with reference {reference} already exists")]
    DuplicateReference {
        reference: String,
        existing_entry_id: String,
    },
    #[error("Invalid data: {0}")]
    InvalidData(String),
}

pub struct ReceiveBillCommand {
    pub vendor: String,
    pub amount: i64,
    pub currency: String,
    pub issue_date: NaiveDate,
    pub terms: PaymentTerms,
    pub memo: Option<String>,
    pub expense_account_id: String,
    pub ap_account_id: String,
    /// Idempotency/reference key stored on the journal entry. When `None`, a
    /// fresh `BILL:<uuid>` is used. Ingest flows pass the source event's
    /// reference so re-imports are detected as duplicates.
    pub reference: Option<String>,
}

pub struct ApplyBillPaymentCommand {
    pub bill_id: String,
    pub payment_date: NaiveDate,
    pub amount_applied: i64,
    pub payment_account_id: String,
    pub ap_account_id: String,
    pub memo: Option<String>,
}

pub struct VoidBillCommand {
    pub bill_id: String,
    pub reason: String,
}

/// Pure (state-independent) validation for receiving a bill: the amount must be
/// positive. Run before opening the append transaction (mirrors
/// [`check_entry_pure`](crate::commands::entry_commands::check_entry_pure)).
pub(crate) fn check_receive_bill_pure(cmd: &ReceiveBillCommand) -> Result<(), BillCommandError> {
    if cmd.amount <= 0 {
        return Err(BillCommandError::InvalidData(
            "Amount must be positive".to_string(),
        ));
    }
    Ok(())
}

/// Outcome of the in-txn `receive_bill` validation + event build.
pub(crate) enum BillStep {
    /// All invariants hold under the write lock; append these events (the bill's
    /// `JournalEntryPosted` then `BillReceived`). The caller wraps each raw
    /// [`Event`] in an envelope, stamping identity as appropriate.
    Append(Vec<Event>),
    /// A domain invariant was violated (duplicate reference, inactive account,
    /// closed period).
    Reject(BillCommandError),
}

/// Run `receive_bill`'s state-dependent invariants inside the append transaction
/// — reference idempotency, then accounts-active / period-open fences — and, if
/// they hold, build the two events that must land together (`JournalEntryPosted`
/// DR expense / CR AP, then `BillReceived`). Shared by
/// [`BillCommands::receive_bill`] and the server-side sync command endpoint so
/// both enforce the SAME invariants under the write lock. The caller wraps each
/// returned [`Event`] in an envelope (local handler stamps its user, the sync
/// path stamps the authenticated actor). Pure checks are the caller's
/// responsibility ([`check_receive_bill_pure`]).
pub(crate) fn build_receive_bill_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ReceiveBillCommand,
) -> Result<BillStep, EventStoreError> {
    let bill_id = Uuid::new_v4().to_string();
    // The bill's journal entry carries this reference; ingest flows pass the
    // source event's reference so re-imports dedupe.
    let reference = cmd
        .reference
        .clone()
        .unwrap_or_else(|| format!("BILL:{}", bill_id));

    // Idempotency: a live entry with this reference already exists ⇒ duplicate.
    // Checked under the write lock (with the idx_journal_entries_reference_unique
    // backstop) so a concurrent import can't slip in after the pre-check.
    if let Some(existing_entry_id) = check_reference_free_in_txn(tx, &reference)? {
        return Ok(BillStep::Reject(BillCommandError::DuplicateReference {
            reference,
            existing_entry_id,
        }));
    }

    // Journal-entry fences for the bill entry we're about to emit.
    let account_ids = [cmd.expense_account_id.as_str(), cmd.ap_account_id.as_str()];
    if let Some(e) = check_entry_invariants_in_txn(tx, &account_ids, cmd.issue_date)? {
        return Ok(BillStep::Reject(BillCommandError::from(e)));
    }

    // Invariants hold against the write-locked state — build both events
    // atomically. Mint the entry id here so BillReceived can reference it.
    let due_date = cmd.terms.due_date(cmd.issue_date);
    let terms_json = serde_json::to_string(&cmd.terms)
        .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;
    let memo = cmd
        .memo
        .clone()
        .unwrap_or_else(|| format!("Bill from {}", cmd.vendor));
    let entry_id = Uuid::new_v4().to_string();
    let lines = vec![
        JournalLineData {
            line_id: format!("{}-line-1", entry_id),
            account_id: cmd.expense_account_id.clone(),
            amount: cmd.amount,
            currency: cmd.currency.clone(),
            exchange_rate: None,
            memo: Some(format!("Expense: {}", cmd.vendor)),
        },
        JournalLineData {
            line_id: format!("{}-line-2", entry_id),
            account_id: cmd.ap_account_id.clone(),
            amount: -cmd.amount,
            currency: cmd.currency.clone(),
            exchange_rate: None,
            memo: Some(format!("AP: {}", cmd.vendor)),
        },
    ];
    let entry_event = Event::JournalEntryPosted {
        entry_id: entry_id.clone(),
        date: cmd.issue_date,
        memo,
        lines,
        reference: Some(reference),
        source: Some(JournalEntrySource::BillPayable),
    };
    let bill_event = Event::BillReceived {
        bill_id,
        vendor: cmd.vendor.clone(),
        amount: cmd.amount,
        currency: cmd.currency.clone(),
        due_date,
        terms: terms_json,
        memo: cmd.memo.clone(),
        entry_id,
    };
    Ok(BillStep::Append(vec![entry_event, bill_event]))
}

/// Run `apply_payment`'s state-dependent invariants inside the append transaction
/// — load the bill under the write lock and check status + the "cumulative
/// payments ≤ amount" guard, then the payment entry's accounts-active /
/// period-open fences — and, if they hold, build the two events that must land
/// together (the payment `JournalEntryPosted` DR AP / CR cash, then
/// `BillPaymentApplied`). Shared by [`BillCommands::apply_payment`] and the
/// server-side sync command endpoint so both enforce the SAME invariants under
/// the write lock. The caller wraps each returned [`Event`] in an envelope.
pub(crate) fn build_apply_payment_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ApplyBillPaymentCommand,
) -> Result<BillStep, EventStoreError> {
    // Bill invariant: load the bill and check status + remaining balance under
    // the write lock.
    let bill: Option<(i64, i64, String)> = tx
        .query_row(
            "SELECT amount, amount_paid, status FROM bills WHERE id = ?1",
            [&cmd.bill_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (amount, amount_paid, status) = match bill {
        Some(b) => b,
        None => return Ok(BillStep::Reject(BillCommandError::NotFound(cmd.bill_id.clone()))),
    };
    if status == "void" {
        return Ok(BillStep::Reject(BillCommandError::Voided));
    }
    if status == "paid" {
        return Ok(BillStep::Reject(BillCommandError::AlreadyPaid));
    }
    let remaining = amount - amount_paid;
    if cmd.amount_applied > remaining {
        return Ok(BillStep::Reject(BillCommandError::OverPayment(
            cmd.amount_applied,
            remaining,
        )));
    }

    // Journal-entry fences for the payment entry we're about to emit.
    let account_ids = [cmd.ap_account_id.as_str(), cmd.payment_account_id.as_str()];
    if let Some(e) = check_entry_invariants_in_txn(tx, &account_ids, cmd.payment_date)? {
        return Ok(BillStep::Reject(BillCommandError::from(e)));
    }

    // Build both events atomically. We mint the entry id here so the
    // BillPaymentApplied can reference it.
    let payment_entry_id = Uuid::new_v4().to_string();
    let memo = cmd.memo.clone().unwrap_or_else(|| {
        format!(
            "Payment on bill {}",
            &cmd.bill_id[..8.min(cmd.bill_id.len())]
        )
    });
    let lines = vec![
        JournalLineData {
            line_id: format!("{}-line-1", payment_entry_id),
            account_id: cmd.ap_account_id.clone(),
            amount: cmd.amount_applied,
            currency: "USD".to_string(),
            exchange_rate: None,
            memo: Some("AP payment".to_string()),
        },
        JournalLineData {
            line_id: format!("{}-line-2", payment_entry_id),
            account_id: cmd.payment_account_id.clone(),
            amount: -cmd.amount_applied,
            currency: "USD".to_string(),
            exchange_rate: None,
            memo: Some("Cash/bank payment".to_string()),
        },
    ];
    let entry_event = Event::JournalEntryPosted {
        entry_id: payment_entry_id.clone(),
        date: cmd.payment_date,
        memo,
        lines,
        reference: Some(format!("BILLPAY:{}:{}", cmd.bill_id, payment_entry_id)),
        source: Some(JournalEntrySource::BillPayment),
    };
    let payment_event = Event::BillPaymentApplied {
        bill_id: cmd.bill_id.clone(),
        payment_entry_id,
        amount_applied: cmd.amount_applied,
    };
    Ok(BillStep::Append(vec![entry_event, payment_event]))
}

/// Run `void_bill`'s state-dependent invariants inside the append transaction —
/// load the bill under the write lock, reject if voided or with payments applied,
/// then the entry-not-already-voided guard — and, if they hold, build the two
/// events that must land together (`JournalEntryVoided`, then `BillVoided`).
/// Shared by [`BillCommands::void_bill`] and the server-side sync command
/// endpoint so both enforce the SAME invariants under the write lock. The caller
/// wraps each returned [`Event`] in an envelope.
pub(crate) fn build_void_bill_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &VoidBillCommand,
) -> Result<BillStep, EventStoreError> {
    let bill: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT amount_paid, status, entry_id FROM bills WHERE id = ?1",
            [&cmd.bill_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (amount_paid, status, entry_id) = match bill {
        Some(b) => b,
        None => return Ok(BillStep::Reject(BillCommandError::NotFound(cmd.bill_id.clone()))),
    };
    if status == "void" {
        return Ok(BillStep::Reject(BillCommandError::Voided));
    }
    if amount_paid > 0 {
        return Ok(BillStep::Reject(BillCommandError::HasPayments));
    }
    // The underlying journal entry must still be voidable.
    if let Some(e) = check_entry_not_voided_in_txn(tx, &entry_id)? {
        return Ok(BillStep::Reject(BillCommandError::from(e)));
    }

    let void_entry_event = Event::JournalEntryVoided {
        entry_id,
        reason: cmd.reason.clone(),
    };
    let void_bill_event = Event::BillVoided {
        bill_id: cmd.bill_id.clone(),
        reason: cmd.reason.clone(),
    };
    Ok(BillStep::Append(vec![void_entry_event, void_bill_event]))
}

pub struct BillCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> BillCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Receive a bill.
    ///
    /// Composite command: emits the bill's journal entry (`JournalEntryPosted`,
    /// DR expense / CR AP) **and** `BillReceived`, which must land together. The
    /// journal-entry fences (both accounts active, period open — audit
    /// `BillReceived`/`JournalEntryPosted`, HIGH) run inside one
    /// [`EventStore::append_checked_many`] transaction, so a concurrent writer
    /// can't deactivate an account or close the period between the check and the
    /// append, and the two events can never land apart (no orphan entry). Retries
    /// on a head move. Mirror of `apply_payment`.
    pub fn receive_bill(
        &mut self,
        cmd: ReceiveBillCommand,
    ) -> Result<StoredEvent, BillCommandError> {
        // Pure validation (independent of ledger state) — do it once up front.
        check_receive_bill_pure(&cmd)?;

        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked_many(
                head,
                |tx| match build_receive_bill_in_txn(tx, &cmd)? {
                    // Local handler: stamp the acting user on each event.
                    BillStep::Append(events) => Ok(Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    )),
                    BillStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                // Return the BillReceived event (the last of the batch).
                CheckedOutcome::Appended(events) => {
                    return Ok(events
                        .into_iter()
                        .last()
                        .expect("receive_bill appends two events"))
                }
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Apply a payment to a bill.
    ///
    /// Composite command: it emits a payment journal entry (`JournalEntryPosted`)
    /// **and** a `BillPaymentApplied`, which must land together. Both events, the
    /// "cumulative payments ≤ amount" guard (the double-payment invariant, audit
    /// #2 / HIGH) and the journal-entry fences all run inside one transaction via
    /// [`EventStore::append_checked_many`], so a concurrent payment can't slip in
    /// between the balance read and the append and overpay the bill. On a head
    /// move we retry against fresh state.
    pub fn apply_payment(
        &mut self,
        cmd: ApplyBillPaymentCommand,
    ) -> Result<StoredEvent, BillCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked_many(
                head,
                |tx| match build_apply_payment_in_txn(tx, &cmd)? {
                    // Local handler: stamp the acting user on each event.
                    BillStep::Append(events) => Ok(Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    )),
                    BillStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                // Return the BillPaymentApplied event (the last of the batch).
                CheckedOutcome::Appended(events) => {
                    return Ok(events
                        .into_iter()
                        .last()
                        .expect("apply_payment appends two events"))
                }
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Void a bill.
    ///
    /// Composite command: voids the bill's journal entry (`JournalEntryVoided`)
    /// and emits `BillVoided` atomically. The no-payments guard (audit
    /// `BillVoided`, HIGH) and the entry-not-already-voided guard both run inside
    /// one [`EventStore::append_checked_many`] transaction, so a payment can't
    /// land between the guard and the void. Retries on a head move.
    pub fn void_bill(&mut self, cmd: VoidBillCommand) -> Result<StoredEvent, BillCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked_many(
                head,
                |tx| match build_void_bill_in_txn(tx, &cmd)? {
                    // Local handler: stamp the acting user on each event.
                    BillStep::Append(events) => Ok(Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    )),
                    BillStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                // Return the BillVoided event (the last of the batch).
                CheckedOutcome::Appended(events) => {
                    return Ok(events
                        .into_iter()
                        .last()
                        .expect("void_bill appends two events"))
                }
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
    use crate::commands::ingest_commands::check_idempotent;
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

    /// A bill created with an event reference stores it on the journal entry, so
    /// re-importing the same event is detected as a duplicate (the goods-received
    /// idempotency fix). Previously the reference was clobbered with BILL:<uuid>.
    #[test]
    fn receive_bill_reference_enables_idempotency() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let inv = mk_account(&mut store, "1200", "Inventory", AccountType::Asset);
        let ap = mk_account(
            &mut store,
            "2000",
            "Accounts Payable",
            AccountType::Liability,
        );

        BillCommands::new(&mut store, "u".to_string())
            .receive_bill(ReceiveBillCommand {
                vendor: "QBP".to_string(),
                amount: 10_000,
                currency: "USD".to_string(),
                issue_date: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                expense_account_id: inv,
                ap_account_id: ap,
                reference: Some("Bugbear pos:evt-1".to_string()),
            })
            .unwrap();

        assert!(
            check_idempotent(store.connection(), "Bugbear pos:evt-1").is_some(),
            "the event reference must be stored so a re-sync dedupes"
        );
        assert!(check_idempotent(store.connection(), "Bugbear pos:evt-2").is_none());
    }

    #[test]
    fn receive_bill_emits_entry_and_bill_atomically() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let expense = mk_account(&mut store, "5000", "COGS", AccountType::Expense);
        let ap = mk_account(&mut store, "2000", "AP", AccountType::Liability);

        let before = store.count().unwrap();
        let stored = BillCommands::new(&mut store, "u".to_string())
            .receive_bill(ReceiveBillCommand {
                vendor: "V".to_string(),
                amount: 10_000,
                currency: "USD".to_string(),
                issue_date: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                expense_account_id: expense,
                ap_account_id: ap,
                reference: None,
            })
            .unwrap();

        // The returned event is BillReceived, and BOTH events landed together.
        let entry_id = match &stored.event {
            Event::BillReceived { entry_id, .. } => entry_id.clone(),
            other => panic!("expected BillReceived, got {other:?}"),
        };
        assert_eq!(
            store.count().unwrap(),
            before + 2,
            "journal entry + bill received"
        );
        // The bill's entry_id points at a real posted journal entry.
        let entry_exists: bool = store
            .connection()
            .query_row(
                "SELECT 1 FROM journal_entries WHERE id = ?1",
                [&entry_id],
                |_| Ok(true),
            )
            .optional()
            .unwrap()
            .unwrap_or(false);
        assert!(entry_exists, "BillReceived.entry_id must reference a posted entry");
    }

    #[test]
    fn receive_bill_rejects_inactive_account_atomically() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let expense = mk_account(&mut store, "5000", "COGS", AccountType::Expense);
        let ap = mk_account(&mut store, "2000", "AP", AccountType::Liability);

        // Deactivate the expense account (zero balance, so it's allowed) so the
        // in-txn fence must reject the bill.
        AccountCommands::new(&mut store, "u".to_string())
            .deactivate_account(crate::commands::account_commands::DeactivateAccountCommand {
                account_id: expense.clone(),
                reason: None,
            })
            .unwrap();

        let before = store.count().unwrap();
        let err = BillCommands::new(&mut store, "u".to_string())
            .receive_bill(ReceiveBillCommand {
                vendor: "V".to_string(),
                amount: 10_000,
                currency: "USD".to_string(),
                issue_date: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                expense_account_id: expense.clone(),
                ap_account_id: ap,
                reference: None,
            })
            .unwrap_err();

        assert!(
            matches!(
                err,
                BillCommandError::EntryCommandError(
                    crate::commands::entry_commands::EntryCommandError::AccountInactive(ref a)
                ) if *a == expense
            ),
            "expected AccountInactive, got {err:?}"
        );
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected bill appends neither the entry nor the bill"
        );
    }

    /// Set up accounts and a bill for `amount`; returns (bill_id, ap, cash).
    fn setup_bill(store: &mut EventStore, amount: i64) -> (String, String, String) {
        let expense = mk_account(store, "5000", "COGS", AccountType::Expense);
        let ap = mk_account(store, "2000", "AP", AccountType::Liability);
        let cash = mk_account(store, "1000", "Cash", AccountType::Asset);
        let stored = BillCommands::new(store, "u".to_string())
            .receive_bill(ReceiveBillCommand {
                vendor: "V".to_string(),
                amount,
                currency: "USD".to_string(),
                issue_date: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                expense_account_id: expense,
                ap_account_id: ap.clone(),
                reference: None,
            })
            .unwrap();
        let bill_id = match &stored.event {
            Event::BillReceived { bill_id, .. } => bill_id.clone(),
            _ => panic!("expected BillReceived"),
        };
        (bill_id, ap, cash)
    }

    #[test]
    fn apply_payment_emits_both_events_and_updates_amount_paid() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (bill_id, ap, cash) = setup_bill(&mut store, 100);

        let before = store.count().unwrap();
        let stored = BillCommands::new(&mut store, "u".to_string())
            .apply_payment(ApplyBillPaymentCommand {
                bill_id: bill_id.clone(),
                payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                amount_applied: 60,
                payment_account_id: cash,
                ap_account_id: ap,
                memo: None,
            })
            .unwrap();

        // The returned event is the BillPaymentApplied, and BOTH events landed.
        assert!(matches!(stored.event, Event::BillPaymentApplied { .. }));
        assert_eq!(
            store.count().unwrap(),
            before + 2,
            "journal entry + bill payment"
        );
        let amount_paid: i64 = store
            .connection()
            .query_row(
                "SELECT amount_paid FROM bills WHERE id = ?1",
                [&bill_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(amount_paid, 60);
    }

    #[test]
    fn apply_payment_rejects_overpayment_atomically() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (bill_id, ap, cash) = setup_bill(&mut store, 100);

        let before = store.count().unwrap();
        let err = BillCommands::new(&mut store, "u".to_string())
            .apply_payment(ApplyBillPaymentCommand {
                bill_id,
                payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                amount_applied: 150,
                payment_account_id: cash,
                ap_account_id: ap,
                memo: None,
            })
            .unwrap_err();
        assert!(matches!(err, BillCommandError::OverPayment(150, 100)));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected payment appends nothing"
        );
    }

    #[test]
    fn concurrent_payments_cannot_overpay_bill() {
        // The marquee double-payment invariant (audit #2), across TWO connections:
        // two 60 payments race against a 100 bill. Because the "cumulative ≤ amount"
        // guard, both events and their projections share one transaction (plus
        // head-CAS retry), exactly one lands and the other re-derives against the
        // committed payment and is rejected OverPayment. The bill is never overpaid.
        let dir = std::env::temp_dir().join(format!("accountir-billpay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("log.db");
        let (bill_id, ap, cash) = {
            let mut store = EventStore::open(&db).unwrap();
            init_schema(store.connection()).unwrap();
            setup_bill(&mut store, 100)
        };

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn_payment =
            |db: std::path::PathBuf,
             bill_id: String,
             ap: String,
             cash: String,
             barrier: std::sync::Arc<std::sync::Barrier>| {
                std::thread::spawn(move || {
                    let mut store = EventStore::open(&db).unwrap();
                    let mut cmds = BillCommands::new(&mut store, "u".to_string());
                    barrier.wait();
                    cmds.apply_payment(ApplyBillPaymentCommand {
                        bill_id,
                        payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                        amount_applied: 60,
                        payment_account_id: cash,
                        ap_account_id: ap,
                        memo: None,
                    })
                })
            };

        let t1 = spawn_payment(
            db.clone(),
            bill_id.clone(),
            ap.clone(),
            cash.clone(),
            barrier.clone(),
        );
        let t2 = spawn_payment(
            db.clone(),
            bill_id.clone(),
            ap.clone(),
            cash.clone(),
            barrier.clone(),
        );
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, 1,
            "exactly one 60 payment may land on a 100 bill (r1={r1:?}, r2={r2:?})"
        );
        for r in [&r1, &r2] {
            if let Err(e) = r {
                assert!(
                    matches!(e, BillCommandError::OverPayment(..)),
                    "the loser must be rejected OverPayment, got {e:?}"
                );
            }
        }

        let store = EventStore::open(&db).unwrap();
        let amount_paid: i64 = store
            .connection()
            .query_row(
                "SELECT amount_paid FROM bills WHERE id = ?1",
                [&bill_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(amount_paid, 60, "the bill must not be overpaid");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn void_bill_emits_both_events_and_sets_status_void() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (bill_id, _ap, _cash) = setup_bill(&mut store, 100);

        let before = store.count().unwrap();
        let stored = BillCommands::new(&mut store, "u".to_string())
            .void_bill(VoidBillCommand {
                bill_id: bill_id.clone(),
                reason: "oops".to_string(),
            })
            .unwrap();
        assert!(matches!(stored.event, Event::BillVoided { .. }));
        assert_eq!(
            store.count().unwrap(),
            before + 2,
            "JournalEntryVoided + BillVoided"
        );
        let status: String = store
            .connection()
            .query_row("SELECT status FROM bills WHERE id = ?1", [&bill_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "void");
    }

    #[test]
    fn void_bill_rejected_when_payments_applied() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (bill_id, ap, cash) = setup_bill(&mut store, 100);
        BillCommands::new(&mut store, "u".to_string())
            .apply_payment(ApplyBillPaymentCommand {
                bill_id: bill_id.clone(),
                payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                amount_applied: 40,
                payment_account_id: cash,
                ap_account_id: ap,
                memo: None,
            })
            .unwrap();

        let before = store.count().unwrap();
        let err = BillCommands::new(&mut store, "u".to_string())
            .void_bill(VoidBillCommand {
                bill_id,
                reason: "x".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, BillCommandError::HasPayments));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected void appends nothing"
        );
    }
}
