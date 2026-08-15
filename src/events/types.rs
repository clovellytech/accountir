use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A journal entry line for the JournalEntryPosted event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalLineData {
    pub line_id: String,
    pub account_id: String,
    /// Amount in smallest currency unit. Positive = debit, negative = credit
    pub amount: i64,
    pub currency: String,
    pub exchange_rate: Option<Decimal>,
    pub memo: Option<String>,
}

/// Source of a journal entry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalEntrySource {
    Manual,
    Import,
    Recurring,
    System,
    Plaid,
    Pos,
    PurchaseOrder,
    InventoryAdjustment,
    EventService,
    BillPayable,
    InvoiceReceivable,
    BillPayment,
    InvoicePayment,
}

/// Info about a Plaid account, used in PlaidItemConnected events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaidAccountInfo {
    pub plaid_account_id: String,
    pub name: String,
    pub official_name: Option<String>,
    pub account_type: String,
    pub mask: Option<String>,
    /// The id that survives a re-link, where the institution provides one.
    ///
    /// `plaid_account_id` does not: Plaid mints account ids per Item, so linking
    /// the same bank again brings the same real account back under a different
    /// id. Anything keying on the old one sees a stranger — which is how one
    /// Chase login ended up recorded as three connections holding three sets of
    /// ids for the same checking account and card.
    ///
    /// `Option`, and omitted entirely when absent, so events written before this
    /// field existed deserialize and **hash** identically — the same rule
    /// `PlaidItemConnected::proxy_item_id` follows, and for the same reason: this
    /// is a hash-chained log and a changed byte is indistinguishable from
    /// tampering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_account_id: Option<String>,
}

/// User role in the system
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserRole {
    Admin,
    Accountant,
    Viewer,
}

/// Account type for the AccountCreated event
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventAccountType {
    Asset,
    Liability,
    Equity,
    Revenue,
    Expense,
}

impl From<crate::domain::AccountType> for EventAccountType {
    fn from(t: crate::domain::AccountType) -> Self {
        match t {
            crate::domain::AccountType::Asset => EventAccountType::Asset,
            crate::domain::AccountType::Liability => EventAccountType::Liability,
            crate::domain::AccountType::Equity => EventAccountType::Equity,
            crate::domain::AccountType::Revenue => EventAccountType::Revenue,
            crate::domain::AccountType::Expense => EventAccountType::Expense,
        }
    }
}

impl From<EventAccountType> for crate::domain::AccountType {
    fn from(t: EventAccountType) -> Self {
        match t {
            EventAccountType::Asset => crate::domain::AccountType::Asset,
            EventAccountType::Liability => crate::domain::AccountType::Liability,
            EventAccountType::Equity => crate::domain::AccountType::Equity,
            EventAccountType::Revenue => crate::domain::AccountType::Revenue,
            EventAccountType::Expense => crate::domain::AccountType::Expense,
        }
    }
}

/// All event types in the accounting system
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    // Company & System
    CompanyCreated {
        company_id: String,
        name: String,
        base_currency: String,
        fiscal_year_start: u32, // Month 1-12
    },
    CompanySettingsUpdated {
        field: String,
        old_value: String,
        new_value: String,
    },
    UserAdded {
        user_id: String,
        username: String,
        role: UserRole,
    },
    UserModified {
        user_id: String,
        field: String,
        old_value: String,
        new_value: String,
    },
    UserRemoved {
        user_id: String,
    },

    // Chart of Accounts
    AccountCreated {
        account_id: String,
        account_type: EventAccountType,
        account_number: String,
        name: String,
        parent_id: Option<String>,
        currency: Option<String>,
        description: Option<String>,
    },
    AccountUpdated {
        account_id: String,
        field: String,
        old_value: String,
        new_value: String,
    },
    AccountDeactivated {
        account_id: String,
        reason: Option<String>,
    },
    AccountReactivated {
        account_id: String,
    },

    // Journal Entries
    JournalEntryPosted {
        entry_id: String,
        date: NaiveDate,
        memo: String,
        lines: Vec<JournalLineData>,
        reference: Option<String>,
        source: Option<JournalEntrySource>,
    },
    JournalEntryVoided {
        entry_id: String,
        reason: String,
    },
    JournalEntryUnvoided {
        entry_id: String,
        reason: String,
    },
    JournalEntryAnnotated {
        entry_id: String,
        annotation: String,
    },
    JournalLineReassigned {
        entry_id: String,
        line_id: String,
        old_account_id: String,
        new_account_id: String,
    },

    // Fiscal Periods
    FiscalYearOpened {
        year: i32,
        start_date: NaiveDate,
        end_date: NaiveDate,
    },
    PeriodClosed {
        year: i32,
        period: u8,
        closed_by_user_id: String,
    },
    PeriodReopened {
        year: i32,
        period: u8,
        reason: String,
        reopened_by_user_id: String,
    },
    YearEndClosed {
        year: i32,
        retained_earnings_entry_id: String,
    },

    // Multi-Currency
    CurrencyEnabled {
        code: String,
        name: String,
        symbol: String,
        decimal_places: u8,
    },
    ExchangeRateRecorded {
        from_currency: String,
        to_currency: String,
        rate: Decimal,
        effective_date: NaiveDate,
    },

    // Plaid Integration
    PlaidItemConnected {
        item_id: String,
        /// The bank-sync proxy's id for this connection.
        ///
        /// Optional because on **group-hosted books it is deliberately omitted**.
        /// It is a handle, inert without the owner's proxy API key, and it is read
        /// only by the machine that talks to the proxy — on hosted books nothing
        /// does, because refreshing goes through the instance's `/bankfeed/` relay
        /// using a grant. Putting it in a log every member replicates would share
        /// something no member can use, which is a cost with no benefit.
        ///
        /// `Option` rather than an empty string so the absence is a fact rather
        /// than a sentinel someone has to remember to check for. Serialization is
        /// unchanged for events that have it — serde writes `Some(x)` exactly as
        /// it wrote `x` — so existing events deserialize and **hash** identically,
        /// which matters on a chained log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proxy_item_id: Option<String>,
        institution_name: String,
        plaid_accounts: Vec<PlaidAccountInfo>,
    },
    /// Accounts found behind a connection that were not known when it was made.
    ///
    /// A separate event rather than a second `PlaidItemConnected`, because that is
    /// what happened: the connection is the same connection, and re-announcing it
    /// would rewrite its institution name and its `proxy_item_id` from whatever
    /// the refreshing client happened to hold.
    ///
    /// Carries the **whole** list the bank now reports, not a delta. The projector
    /// upserts, so a list is idempotent where a delta is a thing that can be
    /// applied twice; and a log of full lists can answer "what did we think was
    /// behind this login in March", which a log of deltas can only reconstruct.
    ///
    /// Nothing is ever removed by this. An account the bank stops reporting keeps
    /// its row and its mapping — a closed card's history is still the business's,
    /// and dropping the mapping would strand the transactions already posted
    /// through it.
    PlaidAccountsRefreshed {
        item_id: String,
        plaid_accounts: Vec<PlaidAccountInfo>,
    },
    PlaidItemDisconnected {
        item_id: String,
        reason: String,
    },
    PlaidAccountMapped {
        item_id: String,
        plaid_account_id: String,
        local_account_id: String,
    },
    PlaidAccountUnmapped {
        item_id: String,
        plaid_account_id: String,
        local_account_id: String,
    },
    PlaidTransactionsSynced {
        item_id: String,
        transactions_added: u32,
        transactions_modified: u32,
        transactions_removed: u32,
        sync_timestamp: String,
    },

    // Event Services (external apps publishing via accountir-events)
    EventServiceRegistered {
        service_id: String,
        name: String,
        root_url: String,
        /// `None` on group-hosted books, where the key lives on the group's
        /// instance instead — see `accountir-server/src/servicekeys.rs`. This log
        /// is replicated in full to every member's laptop, so a key written here
        /// is a key on every one of them, unrecoverably.
        ///
        /// Optional rather than empty-string so the field is *absent* on the wire
        /// and nothing downstream has to decide what a blank key means. The
        /// serialization is unchanged for events that have one — serde writes
        /// `Some(x)` exactly as it wrote `x` — so existing events deserialize and
        /// **hash** identically, which matters on a chained log.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    EventServiceRemoved {
        service_id: String,
    },
    EventServiceSynced {
        service_id: String,
        events_processed: u32,
        entries_created: u32,
        errors: u32,
    },

    // Accounts Payable / Accounts Receivable
    BillReceived {
        bill_id: String,
        vendor: String,
        amount: i64,
        currency: String,
        due_date: NaiveDate,
        terms: String,
        memo: Option<String>,
        entry_id: String,
    },
    BillPaymentApplied {
        bill_id: String,
        payment_entry_id: String,
        amount_applied: i64,
    },
    BillVoided {
        bill_id: String,
        reason: String,
    },
    InvoiceIssued {
        invoice_id: String,
        customer: String,
        amount: i64,
        currency: String,
        due_date: NaiveDate,
        terms: String,
        memo: Option<String>,
        entry_id: String,
    },
    InvoicePaymentReceived {
        invoice_id: String,
        payment_entry_id: String,
        amount_applied: i64,
    },
    InvoiceVoided {
        invoice_id: String,
        reason: String,
    },

    // Bank Reconciliation
    ReconciliationStarted {
        reconciliation_id: String,
        account_id: String,
        statement_date: NaiveDate,
        statement_ending_balance: i64,
    },
    TransactionCleared {
        reconciliation_id: String,
        entry_id: String,
        line_id: String,
        cleared_amount: i64,
    },
    TransactionUncleared {
        reconciliation_id: String,
        entry_id: String,
        line_id: String,
    },
    ReconciliationCompleted {
        reconciliation_id: String,
        difference: i64,
    },
    ReconciliationAbandoned {
        reconciliation_id: String,
    },
}

impl Event {
    /// Get the event type name for storage/display
    pub fn event_type(&self) -> &'static str {
        match self {
            Event::CompanyCreated { .. } => "company_created",
            Event::CompanySettingsUpdated { .. } => "company_settings_updated",
            Event::UserAdded { .. } => "user_added",
            Event::UserModified { .. } => "user_modified",
            Event::UserRemoved { .. } => "user_removed",
            Event::AccountCreated { .. } => "account_created",
            Event::AccountUpdated { .. } => "account_updated",
            Event::AccountDeactivated { .. } => "account_deactivated",
            Event::AccountReactivated { .. } => "account_reactivated",
            Event::JournalEntryPosted { .. } => "journal_entry_posted",
            Event::JournalEntryVoided { .. } => "journal_entry_voided",
            Event::JournalEntryUnvoided { .. } => "journal_entry_unvoided",
            Event::JournalEntryAnnotated { .. } => "journal_entry_annotated",
            Event::JournalLineReassigned { .. } => "journal_line_reassigned",
            Event::FiscalYearOpened { .. } => "fiscal_year_opened",
            Event::PeriodClosed { .. } => "period_closed",
            Event::PeriodReopened { .. } => "period_reopened",
            Event::YearEndClosed { .. } => "year_end_closed",
            Event::CurrencyEnabled { .. } => "currency_enabled",
            Event::ExchangeRateRecorded { .. } => "exchange_rate_recorded",
            Event::PlaidItemConnected { .. } => "plaid_item_connected",
            Event::PlaidAccountsRefreshed { .. } => "plaid_accounts_refreshed",
            Event::PlaidItemDisconnected { .. } => "plaid_item_disconnected",
            Event::PlaidAccountMapped { .. } => "plaid_account_mapped",
            Event::PlaidAccountUnmapped { .. } => "plaid_account_unmapped",
            Event::PlaidTransactionsSynced { .. } => "plaid_transactions_synced",
            Event::EventServiceRegistered { .. } => "event_service_registered",
            Event::EventServiceRemoved { .. } => "event_service_removed",
            Event::EventServiceSynced { .. } => "event_service_synced",
            Event::BillReceived { .. } => "bill_received",
            Event::BillPaymentApplied { .. } => "bill_payment_applied",
            Event::BillVoided { .. } => "bill_voided",
            Event::InvoiceIssued { .. } => "invoice_issued",
            Event::InvoicePaymentReceived { .. } => "invoice_payment_received",
            Event::InvoiceVoided { .. } => "invoice_voided",
            Event::ReconciliationStarted { .. } => "reconciliation_started",
            Event::TransactionCleared { .. } => "transaction_cleared",
            Event::TransactionUncleared { .. } => "transaction_uncleared",
            Event::ReconciliationCompleted { .. } => "reconciliation_completed",
            Event::ReconciliationAbandoned { .. } => "reconciliation_abandoned",
        }
    }

    /// Get the primary entity ID affected by this event (if any)
    pub fn entity_id(&self) -> Option<&str> {
        match self {
            Event::CompanyCreated { .. } => None,
            Event::CompanySettingsUpdated { .. } => None,
            Event::UserAdded { user_id, .. } => Some(user_id),
            Event::UserModified { user_id, .. } => Some(user_id),
            Event::UserRemoved { user_id } => Some(user_id),
            Event::AccountCreated { account_id, .. } => Some(account_id),
            Event::AccountUpdated { account_id, .. } => Some(account_id),
            Event::AccountDeactivated { account_id, .. } => Some(account_id),
            Event::AccountReactivated { account_id } => Some(account_id),
            Event::JournalEntryPosted { entry_id, .. } => Some(entry_id),
            Event::JournalEntryVoided { entry_id, .. } => Some(entry_id),
            Event::JournalEntryUnvoided { entry_id, .. } => Some(entry_id),
            Event::JournalEntryAnnotated { entry_id, .. } => Some(entry_id),
            Event::JournalLineReassigned { entry_id, .. } => Some(entry_id),
            Event::FiscalYearOpened { .. } => None,
            Event::PeriodClosed { .. } => None,
            Event::PeriodReopened { .. } => None,
            Event::YearEndClosed { .. } => None,
            Event::CurrencyEnabled { code, .. } => Some(code),
            Event::ExchangeRateRecorded { .. } => None,
            Event::PlaidItemConnected { item_id, .. } => Some(item_id),
            Event::PlaidAccountsRefreshed { item_id, .. } => Some(item_id),
            Event::PlaidItemDisconnected { item_id, .. } => Some(item_id),
            Event::PlaidAccountMapped { item_id, .. } => Some(item_id),
            Event::PlaidAccountUnmapped { item_id, .. } => Some(item_id),
            Event::PlaidTransactionsSynced { item_id, .. } => Some(item_id),
            Event::EventServiceRegistered { service_id, .. } => Some(service_id),
            Event::EventServiceRemoved { service_id } => Some(service_id),
            Event::EventServiceSynced { service_id, .. } => Some(service_id),
            Event::BillReceived { bill_id, .. } => Some(bill_id),
            Event::BillPaymentApplied { bill_id, .. } => Some(bill_id),
            Event::BillVoided { bill_id, .. } => Some(bill_id),
            Event::InvoiceIssued { invoice_id, .. } => Some(invoice_id),
            Event::InvoicePaymentReceived { invoice_id, .. } => Some(invoice_id),
            Event::InvoiceVoided { invoice_id, .. } => Some(invoice_id),
            Event::ReconciliationStarted {
                reconciliation_id, ..
            } => Some(reconciliation_id),
            Event::TransactionCleared {
                reconciliation_id, ..
            } => Some(reconciliation_id),
            Event::TransactionUncleared {
                reconciliation_id, ..
            } => Some(reconciliation_id),
            Event::ReconciliationCompleted {
                reconciliation_id, ..
            } => Some(reconciliation_id),
            Event::ReconciliationAbandoned { reconciliation_id } => Some(reconciliation_id),
        }
    }
}

/// A stored event with metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: i64,
    pub event: Event,
    pub hash: Vec<u8>,
    /// Free-text writer identity (legacy, always present). Retained for audit and
    /// back-compat; `actor_id` is the authenticated identity when one exists.
    pub user_id: String,
    /// Client-supplied wall-clock time the event occurred. Retained for audit;
    /// `received_at` (server-stamped) is canonical for ordering when set.
    pub timestamp: DateTime<Utc>,
    /// The authenticated user that produced this event. `None` for legacy/solo
    /// single-writer streams (existing rows backfill as NULL).
    pub actor_id: Option<String>,
    /// Server-stamped receive time; canonical for ordering. `None` until a server
    /// sets it (solo/local-first events never have one).
    pub received_at: Option<DateTime<Utc>>,
}

impl StoredEvent {
    /// Create a new stored event (hash will be computed by the event store).
    ///
    /// The server-identity fields (`actor_id`, `received_at`) default to `None`,
    /// preserving the legacy/solo single-writer shape. Use
    /// [`StoredEvent::with_identity`] to carry them through the store.
    pub fn new(
        id: i64,
        event: Event,
        hash: Vec<u8>,
        user_id: String,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            id,
            event,
            hash,
            user_id,
            timestamp,
            actor_id: None,
            received_at: None,
        }
    }

    /// Create a stored event carrying the server-identity fields — used by the
    /// store when hydrating rows and when appending events that have an actor.
    #[allow(clippy::too_many_arguments)]
    pub fn with_identity(
        id: i64,
        event: Event,
        hash: Vec<u8>,
        user_id: String,
        timestamp: DateTime<Utc>,
        actor_id: Option<String>,
        received_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            event,
            hash,
            user_id,
            timestamp,
            actor_id,
            received_at,
        }
    }
}

/// Event envelope for creating new events
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub event: Event,
    pub user_id: String,
    pub timestamp: DateTime<Utc>,
    /// The authenticated user producing this event. `None` = legacy/solo
    /// single-writer (no server identity).
    pub actor_id: Option<String>,
    /// Server-stamped receive time. `None` until a server stamps it; solo
    /// local-first appends leave it `None`.
    pub received_at: Option<DateTime<Utc>>,
}

impl EventEnvelope {
    pub fn new(event: Event, user_id: String) -> Self {
        Self {
            event,
            user_id,
            timestamp: Utc::now(),
            actor_id: None,
            received_at: None,
        }
    }

    pub fn with_timestamp(event: Event, user_id: String, timestamp: DateTime<Utc>) -> Self {
        Self {
            event,
            user_id,
            timestamp,
            actor_id: None,
            received_at: None,
        }
    }

    /// Set the authenticated actor identity (builder-style). `None` keeps the
    /// legacy/solo shape.
    pub fn with_actor(mut self, actor_id: Option<String>) -> Self {
        self.actor_id = actor_id;
        self
    }

    /// Set the server-stamped receive time (builder-style). A server calls this
    /// when it accepts the event; solo appends never do.
    pub fn with_received_at(mut self, received_at: Option<DateTime<Utc>>) -> Self {
        self.received_at = received_at;
        self
    }
}

#[cfg(test)]
mod hash_is_computed_one_way_only {
    /// Nothing may compute an event hash except `compute_event_hash`.
    ///
    /// The regression: `server/mod.rs` hand-rolled an insert for the Plaid Link
    /// callback and hashed `event_type + payload + timestamp` — no separators, no
    /// `user_id`. Every bank connection ever linked wrote an event that could not
    /// be re-derived and so failed chain verification permanently. It was invisible
    /// because nothing verifies the chain on the write path, and it went unnoticed
    /// across 4,586 events in four ledgers, of which exactly the four written that
    /// way were wrong.
    ///
    /// A text lint over this crate's own sources, because the bug was not a wrong
    /// *value* anywhere — it was a second implementation existing at all.
    #[test]
    fn no_source_file_builds_its_own_event_hash() {
        use std::path::{Path, PathBuf};

        fn walk(dir: &Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("read src/") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push((
                        path.display().to_string(),
                        std::fs::read_to_string(&path).expect("read"),
                    ));
                }
            }
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources = Vec::new();
        walk(&root, &mut sources);
        assert!(sources.len() > 10, "the source walk found nothing");

        for (path, body) in &sources {
            // The two files that legitimately own this: `event_store.rs` IS the
            // append path, and `payload.rs` holds the one hash implementation.
            if path.ends_with("store/event_store.rs") || path.ends_with("events/payload.rs") {
                continue;
            }
            // Production code only. Test modules stand up fixture tables and
            // insert into them, which is not a second write path — and this
            // codebase puts `#[cfg(test)]` at the end of a file, so truncating
            // there is enough. Erring toward scanning MORE than needed would make
            // the lint noisy and get it deleted; erring toward less would let a
            // real second writer hide by sitting after a test module, which is
            // why the cut is at the first occurrence rather than any later one.
            let production = body.split("#[cfg(test)]").next().unwrap_or(body);
            assert!(
                !production.contains("INSERT INTO events"),
                "{path} writes the events table directly. Every append must go \
                 through EventStore, which hashes with compute_event_hash, \
                 validates, and projects — a second path silently produced events \
                 that fail chain verification forever."
            );
        }
    }
}

#[cfg(test)]
mod plaid_item_connected_compat {
    use super::*;

    /// Making `proxy_item_id` optional must not disturb a single existing event.
    ///
    /// The log is hash-chained and already has `PlaidItemConnected` events in it
    /// with this field present. If serialization changed shape — a wrapper, a
    /// `null`, a reordering — every one of those events would hash differently,
    /// the chain would fail to verify, and every replica would reject the ledger
    /// it had already accepted. So: `Some(x)` must serialize to exactly what `x`
    /// serialized to before, and the payload a pre-change event was written with
    /// must still deserialize.
    #[test]
    fn an_existing_event_deserializes_and_reserializes_byte_identically() {
        // Exactly the JSON the old `proxy_item_id: String` produced.
        let stored = r#"{"type":"plaid_item_connected","item_id":"i-1","proxy_item_id":"p-1","institution_name":"Chase","plaid_accounts":[]}"#;

        let event: Event = serde_json::from_str(stored).expect("old payload must still parse");
        match &event {
            Event::PlaidItemConnected { proxy_item_id, .. } => {
                assert_eq!(proxy_item_id.as_deref(), Some("p-1"));
            }
            other => panic!("wrong variant: {other:?}"),
        }

        let round_tripped = serde_json::to_string(&event).unwrap();
        assert_eq!(
            round_tripped, stored,
            "serialization changed for an event already on disk — every existing \
             PlaidItemConnected would hash differently and break the chain"
        );
    }

    /// The new shape: hosted books omit the field entirely rather than writing a
    /// `null`, so the payload carries no key at all.
    #[test]
    fn a_hosted_connection_omits_the_handle_rather_than_nulling_it() {
        let event = Event::PlaidItemConnected {
            item_id: "i-2".into(),
            proxy_item_id: None,
            institution_name: "Chase".into(),
            plaid_accounts: vec![],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("proxy_item_id"),
            "an absent handle must not appear on the wire at all: {json}"
        );
        // …and survives a round trip as absent.
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::PlaidItemConnected { proxy_item_id, .. } => assert_eq!(proxy_item_id, None),
            other => panic!("wrong variant: {other:?}"),
        }
    }
}

#[cfg(test)]
mod event_service_registered_compat {
    use super::*;

    /// Making `api_key` optional must not disturb a single existing event.
    ///
    /// Same hazard as `plaid_item_connected_compat`: the log is hash-chained and
    /// standalone ledgers already hold `EventServiceRegistered` events with the
    /// key present. If `Some(x)` serialized to anything other than what `x`
    /// serialized to, every one of those events would hash differently and the
    /// chain would stop verifying.
    #[test]
    fn an_existing_event_deserializes_and_reserializes_byte_identically() {
        // Exactly the JSON the old `api_key: String` produced.
        let stored = r#"{"type":"event_service_registered","service_id":"s-1","name":"Bugbear Bikes","root_url":"https://bugbearbikes.com","api_key":"k-1"}"#;

        let event: Event = serde_json::from_str(stored).expect("old payload must still parse");
        match &event {
            Event::EventServiceRegistered { api_key, .. } => {
                assert_eq!(api_key.as_deref(), Some("k-1"));
            }
            other => panic!("wrong variant: {other:?}"),
        }

        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            stored,
            "serialization changed for an event already on disk — every existing \
             EventServiceRegistered would hash differently and break the chain"
        );
    }

    /// The point of the change: a service registered on hosted books carries no
    /// key at all, so nothing that replicates the group's log replicates a
    /// credential. Absent rather than `null` — a `null` is still a field saying
    /// "there was a key here", and something downstream eventually reads it.
    #[test]
    fn a_hosted_registration_carries_no_key_at_all() {
        let event = Event::EventServiceRegistered {
            service_id: "s-2".into(),
            name: "Bugbear Bikes".into(),
            root_url: "https://bugbearbikes.com".into(),
            api_key: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            !json.contains("api_key"),
            "an absent key must not appear on the wire at all: {json}"
        );
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::EventServiceRegistered { api_key, .. } => assert_eq!(api_key, None),
            other => panic!("wrong variant: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_serialization() {
        let event = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: Some("Main cash account".to_string()),
        };

        let json = serde_json::to_string(&event).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.event_type(), "account_created");
        assert_eq!(parsed.entity_id(), Some("acc-001"));
    }

    #[test]
    fn test_journal_entry_event() {
        let event = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Paid for supplies".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "supplies-expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "cash".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: Some("CHK-001".to_string()),
            source: Some(JournalEntrySource::Manual),
        };

        let json = serde_json::to_string_pretty(&event).unwrap();
        assert!(json.contains("journal_entry_posted"));

        let parsed: Event = serde_json::from_str(&json).unwrap();
        if let Event::JournalEntryPosted { lines, .. } = parsed {
            assert_eq!(lines.len(), 2);
            let sum: i64 = lines.iter().map(|l| l.amount).sum();
            assert_eq!(sum, 0); // Balanced
        } else {
            panic!("Wrong event type");
        }
    }

    #[test]
    fn test_all_event_types() {
        // Ensure all event types serialize correctly
        let events = vec![
            Event::CompanyCreated {
                company_id: "test-company-id".to_string(),
                name: "Test Co".to_string(),
                base_currency: "USD".to_string(),
                fiscal_year_start: 1,
            },
            Event::UserAdded {
                user_id: "user-001".to_string(),
                username: "admin".to_string(),
                role: UserRole::Admin,
            },
            Event::CurrencyEnabled {
                code: "EUR".to_string(),
                name: "Euro".to_string(),
                symbol: "\u{20AC}".to_string(),
                decimal_places: 2,
            },
            Event::ReconciliationStarted {
                reconciliation_id: "recon-001".to_string(),
                account_id: "checking".to_string(),
                statement_date: NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
                statement_ending_balance: 100000,
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let _parsed: Event = serde_json::from_str(&json).unwrap();
        }
    }
}
