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
}

/// Info about a Plaid account, used in PlaidItemConnected events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaidAccountInfo {
    pub plaid_account_id: String,
    pub name: String,
    pub official_name: Option<String>,
    pub account_type: String,
    pub mask: Option<String>,
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
        proxy_item_id: String,
        institution_name: String,
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
            Event::PlaidItemDisconnected { .. } => "plaid_item_disconnected",
            Event::PlaidAccountMapped { .. } => "plaid_account_mapped",
            Event::PlaidAccountUnmapped { .. } => "plaid_account_unmapped",
            Event::PlaidTransactionsSynced { .. } => "plaid_transactions_synced",
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
            Event::PlaidItemDisconnected { item_id, .. } => Some(item_id),
            Event::PlaidAccountMapped { item_id, .. } => Some(item_id),
            Event::PlaidAccountUnmapped { item_id, .. } => Some(item_id),
            Event::PlaidTransactionsSynced { item_id, .. } => Some(item_id),
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
    pub user_id: String,
    pub timestamp: DateTime<Utc>,
}

impl StoredEvent {
    /// Create a new stored event (hash will be computed by the event store)
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
        }
    }
}

/// Event envelope for creating new events
#[derive(Debug, Clone)]
pub struct EventEnvelope {
    pub event: Event,
    pub user_id: String,
    pub timestamp: DateTime<Utc>,
}

impl EventEnvelope {
    pub fn new(event: Event, user_id: String) -> Self {
        Self {
            event,
            user_id,
            timestamp: Utc::now(),
        }
    }

    pub fn with_timestamp(event: Event, user_id: String, timestamp: DateTime<Utc>) -> Self {
        Self {
            event,
            user_id,
            timestamp,
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
