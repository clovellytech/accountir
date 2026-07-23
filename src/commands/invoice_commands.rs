use crate::commands::entry_commands::{
    check_entry_invariants_in_txn, check_entry_not_voided_in_txn,
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
pub enum InvoiceCommandError {
    #[error("Event store error: {0}")]
    EventStoreError(#[from] EventStoreError),
    #[error("Projection error: {0}")]
    ProjectionError(#[from] ProjectionError),
    #[error("Entry command error: {0}")]
    EntryCommandError(#[from] crate::commands::entry_commands::EntryCommandError),
    #[error("Invoice not found: {0}")]
    NotFound(String),
    #[error("Invoice is voided")]
    Voided,
    #[error("Invoice is already fully paid")]
    AlreadyPaid,
    #[error("Payment amount {0} exceeds remaining balance {1}")]
    OverPayment(i64, i64),
    #[error("Cannot void an invoice with payments received")]
    HasPayments,
    #[error("Invalid data: {0}")]
    InvalidData(String),
}

pub struct IssueInvoiceCommand {
    pub customer: String,
    pub amount: i64,
    pub currency: String,
    pub issue_date: NaiveDate,
    pub terms: PaymentTerms,
    pub memo: Option<String>,
    pub revenue_account_id: String,
    pub ar_account_id: String,
}

pub struct ReceiveInvoicePaymentCommand {
    pub invoice_id: String,
    pub payment_date: NaiveDate,
    pub amount_applied: i64,
    pub payment_account_id: String,
    pub ar_account_id: String,
    pub memo: Option<String>,
}

pub struct VoidInvoiceCommand {
    pub invoice_id: String,
    pub reason: String,
}

/// Pure (state-independent) validation for issuing an invoice: the amount must
/// be positive. Run before opening the append transaction (mirrors
/// [`check_entry_pure`](crate::commands::entry_commands::check_entry_pure)).
pub(crate) fn check_issue_invoice_pure(
    cmd: &IssueInvoiceCommand,
) -> Result<(), InvoiceCommandError> {
    if cmd.amount <= 0 {
        return Err(InvoiceCommandError::InvalidData(
            "Amount must be positive".to_string(),
        ));
    }
    Ok(())
}

/// Outcome of the in-txn `issue_invoice` validation + event build.
pub(crate) enum InvoiceStep {
    /// All invariants hold under the write lock; append these events (the
    /// invoice's `JournalEntryPosted` then `InvoiceIssued`). The caller wraps each
    /// raw [`Event`] in an envelope, stamping identity as appropriate.
    Append(Vec<Event>),
    /// A domain invariant was violated (inactive account, closed period).
    Reject(InvoiceCommandError),
}

/// Run `issue_invoice`'s state-dependent invariants inside the append transaction
/// — the accounts-active / period-open fences — and, if they hold, build the two
/// events that must land together (`JournalEntryPosted` DR AR / CR revenue, then
/// `InvoiceIssued`). Shared by [`InvoiceCommands::issue_invoice`] and the
/// server-side sync command endpoint so both enforce the SAME invariants under
/// the write lock. The caller wraps each returned [`Event`] in an envelope (local
/// handler stamps its user, the sync path stamps the authenticated actor). Pure
/// checks are the caller's responsibility ([`check_issue_invoice_pure`]).
pub(crate) fn build_issue_invoice_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &IssueInvoiceCommand,
) -> Result<InvoiceStep, EventStoreError> {
    // Journal-entry fences for the invoice entry we're about to emit.
    let account_ids = [cmd.ar_account_id.as_str(), cmd.revenue_account_id.as_str()];
    if let Some(e) = check_entry_invariants_in_txn(tx, &account_ids, cmd.issue_date)? {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::from(e)));
    }

    // Invariants hold against the write-locked state — build both events
    // atomically. Mint the entry id here so InvoiceIssued can reference it.
    let invoice_id = Uuid::new_v4().to_string();
    let due_date = cmd.terms.due_date(cmd.issue_date);
    let terms_json = serde_json::to_string(&cmd.terms)
        .map_err(|e| EventStoreError::SerializationError(e.to_string()))?;
    let memo = cmd
        .memo
        .clone()
        .unwrap_or_else(|| format!("Invoice to {}", cmd.customer));
    let entry_id = Uuid::new_v4().to_string();
    let lines = vec![
        JournalLineData {
            line_id: format!("{}-line-1", entry_id),
            account_id: cmd.ar_account_id.clone(),
            amount: cmd.amount,
            currency: cmd.currency.clone(),
            exchange_rate: None,
            memo: Some(format!("AR: {}", cmd.customer)),
        },
        JournalLineData {
            line_id: format!("{}-line-2", entry_id),
            account_id: cmd.revenue_account_id.clone(),
            amount: -cmd.amount,
            currency: cmd.currency.clone(),
            exchange_rate: None,
            memo: Some(format!("Revenue: {}", cmd.customer)),
        },
    ];
    let entry_event = Event::JournalEntryPosted {
        entry_id: entry_id.clone(),
        date: cmd.issue_date,
        memo,
        lines,
        reference: Some(format!("INV:{}", invoice_id)),
        source: Some(JournalEntrySource::InvoiceReceivable),
    };
    let invoice_event = Event::InvoiceIssued {
        invoice_id,
        customer: cmd.customer.clone(),
        amount: cmd.amount,
        currency: cmd.currency.clone(),
        due_date,
        terms: terms_json,
        memo: cmd.memo.clone(),
        entry_id,
    };
    Ok(InvoiceStep::Append(vec![entry_event, invoice_event]))
}

/// Run `receive_payment`'s state-dependent invariants inside the append
/// transaction — load the invoice under the write lock and check status + the
/// "cumulative payments ≤ amount" guard, then the payment entry's accounts-active
/// / period-open fences — and, if they hold, build the two events that must land
/// together (the payment `JournalEntryPosted` DR cash / CR AR, then
/// `InvoicePaymentReceived`). Shared by [`InvoiceCommands::receive_payment`] and
/// the server-side sync command endpoint so both enforce the SAME invariants
/// under the write lock. The caller wraps each returned [`Event`] in an envelope.
pub(crate) fn build_receive_payment_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ReceiveInvoicePaymentCommand,
) -> Result<InvoiceStep, EventStoreError> {
    // Invoice invariant: load it and check status + remaining balance under the
    // write lock.
    let invoice: Option<(i64, i64, String)> = tx
        .query_row(
            "SELECT amount, amount_paid, status FROM invoices WHERE id = ?1",
            [&cmd.invoice_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (amount, amount_paid, status) = match invoice {
        Some(i) => i,
        None => {
            return Ok(InvoiceStep::Reject(InvoiceCommandError::NotFound(
                cmd.invoice_id.clone(),
            )))
        }
    };
    if status == "void" {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::Voided));
    }
    if status == "paid" {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::AlreadyPaid));
    }
    let remaining = amount - amount_paid;
    if cmd.amount_applied > remaining {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::OverPayment(
            cmd.amount_applied,
            remaining,
        )));
    }

    // Journal-entry fences for the payment entry we're about to emit.
    let account_ids = [cmd.payment_account_id.as_str(), cmd.ar_account_id.as_str()];
    if let Some(e) = check_entry_invariants_in_txn(tx, &account_ids, cmd.payment_date)? {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::from(e)));
    }

    // Build both events atomically (id minted up front so the
    // InvoicePaymentReceived can reference the entry).
    let payment_entry_id = Uuid::new_v4().to_string();
    let memo = cmd.memo.clone().unwrap_or_else(|| {
        format!(
            "Payment on invoice {}",
            &cmd.invoice_id[..8.min(cmd.invoice_id.len())]
        )
    });
    // DR payment account / CR AR
    let lines = vec![
        JournalLineData {
            line_id: format!("{}-line-1", payment_entry_id),
            account_id: cmd.payment_account_id.clone(),
            amount: cmd.amount_applied,
            currency: "USD".to_string(),
            exchange_rate: None,
            memo: Some("Payment received".to_string()),
        },
        JournalLineData {
            line_id: format!("{}-line-2", payment_entry_id),
            account_id: cmd.ar_account_id.clone(),
            amount: -cmd.amount_applied,
            currency: "USD".to_string(),
            exchange_rate: None,
            memo: Some("AR payment".to_string()),
        },
    ];
    let entry_event = Event::JournalEntryPosted {
        entry_id: payment_entry_id.clone(),
        date: cmd.payment_date,
        memo,
        lines,
        reference: Some(format!("INVPAY:{}:{}", cmd.invoice_id, payment_entry_id)),
        source: Some(JournalEntrySource::InvoicePayment),
    };
    let payment_event = Event::InvoicePaymentReceived {
        invoice_id: cmd.invoice_id.clone(),
        payment_entry_id,
        amount_applied: cmd.amount_applied,
    };
    Ok(InvoiceStep::Append(vec![entry_event, payment_event]))
}

/// Run `void_invoice`'s state-dependent invariants inside the append transaction
/// — load the invoice under the write lock, reject if voided or with payments
/// received, then the entry-not-already-voided guard — and, if they hold, build
/// the two events that must land together (`JournalEntryVoided`, then
/// `InvoiceVoided`). Shared by [`InvoiceCommands::void_invoice`] and the
/// server-side sync command endpoint so both enforce the SAME invariants under
/// the write lock. The caller wraps each returned [`Event`] in an envelope.
pub(crate) fn build_void_invoice_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &VoidInvoiceCommand,
) -> Result<InvoiceStep, EventStoreError> {
    let invoice: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT amount_paid, status, entry_id FROM invoices WHERE id = ?1",
            [&cmd.invoice_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (amount_paid, status, entry_id) = match invoice {
        Some(i) => i,
        None => {
            return Ok(InvoiceStep::Reject(InvoiceCommandError::NotFound(
                cmd.invoice_id.clone(),
            )))
        }
    };
    if status == "void" {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::Voided));
    }
    if amount_paid > 0 {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::HasPayments));
    }
    // The underlying journal entry must still be voidable.
    if let Some(e) = check_entry_not_voided_in_txn(tx, &entry_id)? {
        return Ok(InvoiceStep::Reject(InvoiceCommandError::from(e)));
    }

    let void_entry_event = Event::JournalEntryVoided {
        entry_id,
        reason: cmd.reason.clone(),
    };
    let void_invoice_event = Event::InvoiceVoided {
        invoice_id: cmd.invoice_id.clone(),
        reason: cmd.reason.clone(),
    };
    Ok(InvoiceStep::Append(vec![void_entry_event, void_invoice_event]))
}

pub struct InvoiceCommands<'a> {
    store: &'a mut EventStore,
    user_id: String,
}

impl<'a> InvoiceCommands<'a> {
    pub fn new(store: &'a mut EventStore, user_id: String) -> Self {
        Self { store, user_id }
    }

    /// Issue an invoice.
    ///
    /// Composite command: emits the invoice's journal entry (`JournalEntryPosted`,
    /// DR AR / CR revenue) **and** `InvoiceIssued`, which must land together. The
    /// journal-entry fences (both accounts active, period open — audit
    /// `InvoiceIssued`/`JournalEntryPosted`, HIGH) run inside one
    /// [`EventStore::append_checked_many`] transaction, so a concurrent writer
    /// can't deactivate an account or close the period between the check and the
    /// append, and the two events can never land apart (no orphan entry). Retries
    /// on a head move. Mirror of `receive_payment`.
    pub fn issue_invoice(
        &mut self,
        cmd: IssueInvoiceCommand,
    ) -> Result<StoredEvent, InvoiceCommandError> {
        // Pure validation (independent of ledger state) — do it once up front.
        check_issue_invoice_pure(&cmd)?;

        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked_many(
                head,
                |tx| match build_issue_invoice_in_txn(tx, &cmd)? {
                    // Local handler: stamp the acting user on each event.
                    InvoiceStep::Append(events) => Ok(Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    )),
                    InvoiceStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                // Return the InvoiceIssued event (the last of the batch).
                CheckedOutcome::Appended(events) => {
                    return Ok(events
                        .into_iter()
                        .last()
                        .expect("issue_invoice appends two events"))
                }
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Receive a payment against an invoice.
    ///
    /// Composite command (mirror of `BillCommands::apply_payment`): emits a
    /// payment journal entry (`JournalEntryPosted`) and an `InvoicePaymentReceived`
    /// atomically. The "cumulative payments ≤ amount" guard (audit
    /// `InvoicePaymentReceived`, HIGH), both events, and the payment entry's fences
    /// all run in one [`EventStore::append_checked_many`] transaction, so a
    /// concurrent payment can't overpay the invoice. Retries on a head move.
    pub fn receive_payment(
        &mut self,
        cmd: ReceiveInvoicePaymentCommand,
    ) -> Result<StoredEvent, InvoiceCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked_many(
                head,
                |tx| match build_receive_payment_in_txn(tx, &cmd)? {
                    // Local handler: stamp the acting user on each event.
                    InvoiceStep::Append(events) => Ok(Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    )),
                    InvoiceStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                // Return the InvoicePaymentReceived event (the last of the batch).
                CheckedOutcome::Appended(events) => {
                    return Ok(events
                        .into_iter()
                        .last()
                        .expect("receive_payment appends two events"))
                }
                CheckedOutcome::HeadMismatch { .. } => continue, // refetch & retry
                CheckedOutcome::Rejected(e) => return Err(e),
            }
        }
    }

    /// Void an invoice.
    ///
    /// Composite command: voids the invoice's journal entry (`JournalEntryVoided`)
    /// and emits `InvoiceVoided` atomically. The no-payments guard (audit
    /// `InvoiceVoided`, HIGH) and the entry-not-already-voided guard both run
    /// inside one [`EventStore::append_checked_many`] transaction, so a payment
    /// can't land between the guard and the void. Retries on a head move.
    pub fn void_invoice(
        &mut self,
        cmd: VoidInvoiceCommand,
    ) -> Result<StoredEvent, InvoiceCommandError> {
        let user_id = self.user_id.clone();
        loop {
            let head = self.store.latest_id()?.unwrap_or(0);
            let outcome = self.store.append_checked_many(
                head,
                |tx| match build_void_invoice_in_txn(tx, &cmd)? {
                    // Local handler: stamp the acting user on each event.
                    InvoiceStep::Append(events) => Ok(Verdict::Append(
                        events
                            .into_iter()
                            .map(|e| EventEnvelope::new(e, user_id.clone()))
                            .collect(),
                    )),
                    InvoiceStep::Reject(e) => Ok(Verdict::Reject(e)),
                },
                |tx, stored| {
                    Projector::new(tx)
                        .apply(stored)
                        .map_err(|e| EventStoreError::Projection(e.to_string()))
                },
            )?;

            match outcome {
                // Return the InvoiceVoided event (the last of the batch).
                CheckedOutcome::Appended(events) => {
                    return Ok(events
                        .into_iter()
                        .last()
                        .expect("void_invoice appends two events"))
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

    #[test]
    fn issue_invoice_emits_entry_and_invoice_atomically() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let revenue = mk_account(&mut store, "4000", "Revenue", AccountType::Revenue);
        let ar = mk_account(&mut store, "1100", "AR", AccountType::Asset);

        let before = store.count().unwrap();
        let stored = InvoiceCommands::new(&mut store, "u".to_string())
            .issue_invoice(IssueInvoiceCommand {
                customer: "C".to_string(),
                amount: 10_000,
                currency: "USD".to_string(),
                issue_date: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                revenue_account_id: revenue,
                ar_account_id: ar,
            })
            .unwrap();

        // The returned event is InvoiceIssued, and BOTH events landed together.
        let entry_id = match &stored.event {
            Event::InvoiceIssued { entry_id, .. } => entry_id.clone(),
            other => panic!("expected InvoiceIssued, got {other:?}"),
        };
        assert_eq!(
            store.count().unwrap(),
            before + 2,
            "journal entry + invoice issued"
        );
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
        assert!(entry_exists, "InvoiceIssued.entry_id must reference a posted entry");
    }

    #[test]
    fn issue_invoice_rejects_inactive_account_atomically() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let revenue = mk_account(&mut store, "4000", "Revenue", AccountType::Revenue);
        let ar = mk_account(&mut store, "1100", "AR", AccountType::Asset);

        // Deactivate the revenue account (zero balance, so it's allowed) so the
        // in-txn fence must reject the invoice.
        AccountCommands::new(&mut store, "u".to_string())
            .deactivate_account(crate::commands::account_commands::DeactivateAccountCommand {
                account_id: revenue.clone(),
                reason: None,
            })
            .unwrap();

        let before = store.count().unwrap();
        let err = InvoiceCommands::new(&mut store, "u".to_string())
            .issue_invoice(IssueInvoiceCommand {
                customer: "C".to_string(),
                amount: 10_000,
                currency: "USD".to_string(),
                issue_date: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                revenue_account_id: revenue.clone(),
                ar_account_id: ar,
            })
            .unwrap_err();

        assert!(
            matches!(
                err,
                InvoiceCommandError::EntryCommandError(
                    crate::commands::entry_commands::EntryCommandError::AccountInactive(ref a)
                ) if *a == revenue
            ),
            "expected AccountInactive, got {err:?}"
        );
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected invoice appends neither the entry nor the invoice"
        );
    }

    /// Set up accounts and an invoice for `amount`; returns (invoice_id, ar, cash).
    fn setup_invoice(store: &mut EventStore, amount: i64) -> (String, String, String) {
        let revenue = mk_account(store, "4000", "Revenue", AccountType::Revenue);
        let ar = mk_account(store, "1100", "Accounts Receivable", AccountType::Asset);
        let cash = mk_account(store, "1000", "Cash", AccountType::Asset);
        let stored = InvoiceCommands::new(store, "u".to_string())
            .issue_invoice(IssueInvoiceCommand {
                customer: "C".to_string(),
                amount,
                currency: "USD".to_string(),
                issue_date: NaiveDate::from_ymd_opt(2026, 7, 3).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                revenue_account_id: revenue,
                ar_account_id: ar.clone(),
            })
            .unwrap();
        let invoice_id = match &stored.event {
            Event::InvoiceIssued { invoice_id, .. } => invoice_id.clone(),
            _ => panic!("expected InvoiceIssued"),
        };
        (invoice_id, ar, cash)
    }

    #[test]
    fn receive_payment_emits_both_events_and_updates_amount_paid() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (invoice_id, ar, cash) = setup_invoice(&mut store, 100);

        let before = store.count().unwrap();
        let stored = InvoiceCommands::new(&mut store, "u".to_string())
            .receive_payment(ReceiveInvoicePaymentCommand {
                invoice_id: invoice_id.clone(),
                payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                amount_applied: 60,
                payment_account_id: cash,
                ar_account_id: ar,
                memo: None,
            })
            .unwrap();

        assert!(matches!(stored.event, Event::InvoicePaymentReceived { .. }));
        assert_eq!(
            store.count().unwrap(),
            before + 2,
            "journal entry + invoice payment"
        );
        let amount_paid: i64 = store
            .connection()
            .query_row(
                "SELECT amount_paid FROM invoices WHERE id = ?1",
                [&invoice_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(amount_paid, 60);
    }

    #[test]
    fn receive_payment_rejects_overpayment_atomically() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (invoice_id, ar, cash) = setup_invoice(&mut store, 100);

        let before = store.count().unwrap();
        let err = InvoiceCommands::new(&mut store, "u".to_string())
            .receive_payment(ReceiveInvoicePaymentCommand {
                invoice_id,
                payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                amount_applied: 150,
                payment_account_id: cash,
                ar_account_id: ar,
                memo: None,
            })
            .unwrap_err();
        assert!(matches!(err, InvoiceCommandError::OverPayment(150, 100)));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected payment appends nothing"
        );
    }

    #[test]
    fn concurrent_payments_cannot_overpay_invoice() {
        // Mirror of the bill double-payment race: two 60 payments race against a
        // 100 invoice across two connections; the in-txn balance guard + head-CAS
        // retry let exactly one land, the other is rejected OverPayment.
        let dir = std::env::temp_dir().join(format!("accountir-invpay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("log.db");
        let (invoice_id, ar, cash) = {
            let mut store = EventStore::open(&db).unwrap();
            init_schema(store.connection()).unwrap();
            setup_invoice(&mut store, 100)
        };

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn_payment =
            |db: std::path::PathBuf,
             invoice_id: String,
             ar: String,
             cash: String,
             barrier: std::sync::Arc<std::sync::Barrier>| {
                std::thread::spawn(move || {
                    let mut store = EventStore::open(&db).unwrap();
                    let mut cmds = InvoiceCommands::new(&mut store, "u".to_string());
                    barrier.wait();
                    cmds.receive_payment(ReceiveInvoicePaymentCommand {
                        invoice_id,
                        payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                        amount_applied: 60,
                        payment_account_id: cash,
                        ar_account_id: ar,
                        memo: None,
                    })
                })
            };

        let t1 = spawn_payment(
            db.clone(),
            invoice_id.clone(),
            ar.clone(),
            cash.clone(),
            barrier.clone(),
        );
        let t2 = spawn_payment(
            db.clone(),
            invoice_id.clone(),
            ar.clone(),
            cash.clone(),
            barrier.clone(),
        );
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            oks, 1,
            "exactly one 60 payment may land on a 100 invoice (r1={r1:?}, r2={r2:?})"
        );
        for r in [&r1, &r2] {
            if let Err(e) = r {
                assert!(
                    matches!(e, InvoiceCommandError::OverPayment(..)),
                    "the loser must be rejected OverPayment, got {e:?}"
                );
            }
        }

        let store = EventStore::open(&db).unwrap();
        let amount_paid: i64 = store
            .connection()
            .query_row(
                "SELECT amount_paid FROM invoices WHERE id = ?1",
                [&invoice_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(amount_paid, 60, "the invoice must not be overpaid");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn void_invoice_emits_both_events_and_sets_status_void() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (invoice_id, _ar, _cash) = setup_invoice(&mut store, 100);

        let before = store.count().unwrap();
        let stored = InvoiceCommands::new(&mut store, "u".to_string())
            .void_invoice(VoidInvoiceCommand {
                invoice_id: invoice_id.clone(),
                reason: "oops".to_string(),
            })
            .unwrap();
        assert!(matches!(stored.event, Event::InvoiceVoided { .. }));
        assert_eq!(
            store.count().unwrap(),
            before + 2,
            "JournalEntryVoided + InvoiceVoided"
        );
        let status: String = store
            .connection()
            .query_row(
                "SELECT status FROM invoices WHERE id = ?1",
                [&invoice_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "void");
    }

    #[test]
    fn void_invoice_rejected_when_payments_received() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let (invoice_id, ar, cash) = setup_invoice(&mut store, 100);
        InvoiceCommands::new(&mut store, "u".to_string())
            .receive_payment(ReceiveInvoicePaymentCommand {
                invoice_id: invoice_id.clone(),
                payment_date: NaiveDate::from_ymd_opt(2026, 7, 10).unwrap(),
                amount_applied: 40,
                payment_account_id: cash,
                ar_account_id: ar,
                memo: None,
            })
            .unwrap();

        let before = store.count().unwrap();
        let err = InvoiceCommands::new(&mut store, "u".to_string())
            .void_invoice(VoidInvoiceCommand {
                invoice_id,
                reason: "x".to_string(),
            })
            .unwrap_err();
        assert!(matches!(err, InvoiceCommandError::HasPayments));
        assert_eq!(
            store.count().unwrap(),
            before,
            "a rejected void appends nothing"
        );
    }
}
