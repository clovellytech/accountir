use crate::events::types::{Event, JournalLineData};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ValidationError {
    #[error("Journal entry is not balanced: sum is {0}, expected 0")]
    JournalEntryNotBalanced(i64),
    #[error("Journal entry must have at least two lines")]
    InsufficientLines,
    #[error("Empty field: {0}")]
    EmptyField(String),
    #[error("Invalid value: {0}")]
    InvalidValue(String),
    #[error("Duplicate ID: {0}")]
    DuplicateId(String),
    #[error("Invalid currency code: {0}")]
    InvalidCurrencyCode(String),
    #[error("Invalid account number: {0}")]
    InvalidAccountNumber(String),
    #[error("Invalid fiscal year start month: {0}")]
    InvalidFiscalYearStart(u32),
    #[error("Invalid period: {0}")]
    InvalidPeriod(u8),
}

/// Validate an event before storing
pub fn validate_event(event: &Event) -> Result<(), ValidationError> {
    match event {
        Event::CompanyCreated {
            company_id,
            name,
            base_currency,
            fiscal_year_start,
        } => {
            validate_non_empty(company_id, "company_id")?;
            validate_non_empty(name, "company name")?;
            validate_currency_code(base_currency)?;
            validate_fiscal_year_start(*fiscal_year_start)?;
        }
        Event::CompanySettingsUpdated {
            field,
            old_value: _,
            new_value,
        } => {
            validate_non_empty(field, "field")?;
            validate_non_empty(new_value, "new_value")?;
        }
        Event::UserAdded {
            user_id,
            username,
            role: _,
        } => {
            validate_non_empty(user_id, "user_id")?;
            validate_non_empty(username, "username")?;
        }
        Event::UserModified {
            user_id,
            field,
            old_value: _,
            new_value,
        } => {
            validate_non_empty(user_id, "user_id")?;
            validate_non_empty(field, "field")?;
            validate_non_empty(new_value, "new_value")?;
        }
        Event::UserRemoved { user_id } => {
            validate_non_empty(user_id, "user_id")?;
        }
        Event::AccountCreated {
            account_id,
            account_type: _,
            account_number,
            name,
            parent_id: _,
            currency,
            description: _,
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_account_number(account_number)?;
            validate_non_empty(name, "name")?;
            if let Some(curr) = currency {
                validate_currency_code(curr)?;
            }
        }
        Event::AccountUpdated {
            account_id,
            field,
            old_value: _,
            new_value,
        } => {
            validate_non_empty(account_id, "account_id")?;
            validate_non_empty(field, "field")?;
            validate_non_empty(new_value, "new_value")?;
        }
        Event::AccountDeactivated {
            account_id,
            reason: _,
        } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::AccountReactivated { account_id } => {
            validate_non_empty(account_id, "account_id")?;
        }
        Event::JournalEntryPosted {
            entry_id,
            date: _,
            memo,
            lines,
            reference: _,
            source: _,
        } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(memo, "memo")?;
            validate_journal_lines(lines)?;
        }
        Event::JournalEntryVoided { entry_id, reason } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(reason, "reason")?;
        }
        Event::JournalEntryUnvoided { entry_id, reason } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(reason, "reason")?;
        }
        Event::JournalEntryAnnotated {
            entry_id,
            annotation,
        } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(annotation, "annotation")?;
        }
        Event::JournalLineReassigned {
            entry_id,
            line_id,
            old_account_id,
            new_account_id,
        } => {
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(line_id, "line_id")?;
            validate_non_empty(old_account_id, "old_account_id")?;
            validate_non_empty(new_account_id, "new_account_id")?;
        }
        Event::FiscalYearOpened {
            year: _,
            start_date: _,
            end_date: _,
        } => {
            // Dates are validated by chrono
        }
        Event::PeriodClosed {
            year: _,
            period,
            closed_by_user_id,
        } => {
            validate_period(*period)?;
            validate_non_empty(closed_by_user_id, "closed_by_user_id")?;
        }
        Event::PeriodReopened {
            year: _,
            period,
            reason,
            reopened_by_user_id,
        } => {
            validate_period(*period)?;
            validate_non_empty(reason, "reason")?;
            validate_non_empty(reopened_by_user_id, "reopened_by_user_id")?;
        }
        Event::YearEndClosed {
            year: _,
            retained_earnings_entry_id,
        } => {
            validate_non_empty(retained_earnings_entry_id, "retained_earnings_entry_id")?;
        }
        Event::CurrencyEnabled {
            code,
            name,
            symbol: _,
            decimal_places: _,
        } => {
            validate_currency_code(code)?;
            validate_non_empty(name, "name")?;
        }
        Event::ExchangeRateRecorded {
            from_currency,
            to_currency,
            rate,
            effective_date: _,
        } => {
            validate_currency_code(from_currency)?;
            validate_currency_code(to_currency)?;
            if *rate <= rust_decimal::Decimal::ZERO {
                return Err(ValidationError::InvalidValue(
                    "Exchange rate must be positive".to_string(),
                ));
            }
        }
        Event::ReconciliationStarted {
            reconciliation_id,
            account_id,
            statement_date: _,
            statement_ending_balance: _,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
            validate_non_empty(account_id, "account_id")?;
        }
        Event::TransactionCleared {
            reconciliation_id,
            entry_id,
            line_id,
            cleared_amount: _,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(line_id, "line_id")?;
        }
        Event::TransactionUncleared {
            reconciliation_id,
            entry_id,
            line_id,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
            validate_non_empty(entry_id, "entry_id")?;
            validate_non_empty(line_id, "line_id")?;
        }
        Event::ReconciliationCompleted {
            reconciliation_id,
            difference: _,
        } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
        }
        Event::ReconciliationAbandoned { reconciliation_id } => {
            validate_non_empty(reconciliation_id, "reconciliation_id")?;
        }
        Event::PlaidItemConnected {
            item_id,
            proxy_item_id,
            institution_name,
            plaid_accounts: _,
        } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(proxy_item_id, "proxy_item_id")?;
            validate_non_empty(institution_name, "institution_name")?;
        }
        Event::PlaidItemDisconnected { item_id, reason } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(reason, "reason")?;
        }
        Event::PlaidAccountMapped {
            item_id,
            plaid_account_id,
            local_account_id,
        } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(plaid_account_id, "plaid_account_id")?;
            validate_non_empty(local_account_id, "local_account_id")?;
        }
        Event::PlaidAccountUnmapped {
            item_id,
            plaid_account_id,
            local_account_id,
        } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(plaid_account_id, "plaid_account_id")?;
            validate_non_empty(local_account_id, "local_account_id")?;
        }
        Event::PlaidTransactionsSynced {
            item_id,
            sync_timestamp,
            ..
        } => {
            validate_non_empty(item_id, "item_id")?;
            validate_non_empty(sync_timestamp, "sync_timestamp")?;
        }
    }
    Ok(())
}

fn validate_non_empty(value: &str, field_name: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::EmptyField(field_name.to_string()))
    } else {
        Ok(())
    }
}

fn validate_currency_code(code: &str) -> Result<(), ValidationError> {
    // ISO 4217 currency codes are 3 uppercase letters
    if code.len() != 3 || !code.chars().all(|c| c.is_ascii_uppercase()) {
        Err(ValidationError::InvalidCurrencyCode(code.to_string()))
    } else {
        Ok(())
    }
}

fn validate_account_number(number: &str) -> Result<(), ValidationError> {
    if number.trim().is_empty() {
        return Err(ValidationError::InvalidAccountNumber(
            "Account number cannot be empty".to_string(),
        ));
    }
    // Account numbers should be alphanumeric (allow dashes and dots for sub-accounts)
    if !number
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '.')
    {
        return Err(ValidationError::InvalidAccountNumber(format!(
            "Invalid characters in account number: {}",
            number
        )));
    }
    Ok(())
}

fn validate_fiscal_year_start(month: u32) -> Result<(), ValidationError> {
    if !(1..=12).contains(&month) {
        Err(ValidationError::InvalidFiscalYearStart(month))
    } else {
        Ok(())
    }
}

fn validate_period(period: u8) -> Result<(), ValidationError> {
    if !(1..=12).contains(&period) {
        Err(ValidationError::InvalidPeriod(period))
    } else {
        Ok(())
    }
}

fn validate_journal_lines(lines: &[JournalLineData]) -> Result<(), ValidationError> {
    if lines.len() < 2 {
        return Err(ValidationError::InsufficientLines);
    }

    // Check that the entry is balanced
    let sum: i64 = lines.iter().map(|l| l.amount).sum();
    if sum != 0 {
        return Err(ValidationError::JournalEntryNotBalanced(sum));
    }

    // Validate each line
    for line in lines {
        validate_non_empty(&line.line_id, "line_id")?;
        validate_non_empty(&line.account_id, "account_id")?;
        validate_currency_code(&line.currency)?;
    }

    // Check for duplicate line IDs
    let mut seen_ids = std::collections::HashSet::new();
    for line in lines {
        if !seen_ids.insert(&line.line_id) {
            return Err(ValidationError::DuplicateId(line.line_id.clone()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::types::{EventAccountType, JournalEntrySource};
    use chrono::NaiveDate;

    #[test]
    fn test_validate_company_created() {
        let event = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "Test Company".to_string(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 1,
        };
        assert!(validate_event(&event).is_ok());

        let invalid = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "".to_string(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 1,
        };
        assert!(validate_event(&invalid).is_err());

        let invalid_currency = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "Test".to_string(),
            base_currency: "usd".to_string(), // lowercase
            fiscal_year_start: 1,
        };
        assert!(validate_event(&invalid_currency).is_err());

        let invalid_month = Event::CompanyCreated {
            company_id: "test-id".to_string(),
            name: "Test".to_string(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 13, // invalid
        };
        assert!(validate_event(&invalid_month).is_err());
    }

    #[test]
    fn test_validate_journal_entry() {
        let valid = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
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
            reference: None,
            source: Some(JournalEntrySource::Manual),
        };
        assert!(validate_event(&valid).is_ok());

        // Unbalanced entry
        let unbalanced = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-002".to_string(),
                    account_id: "cash".to_string(),
                    amount: -5000, // Not balanced!
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        assert!(matches!(
            validate_event(&unbalanced),
            Err(ValidationError::JournalEntryNotBalanced(_))
        ));

        // Single line entry
        let single_line = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
            lines: vec![JournalLineData {
                line_id: "line-001".to_string(),
                account_id: "expense".to_string(),
                amount: 0,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            }],
            reference: None,
            source: None,
        };
        assert!(matches!(
            validate_event(&single_line),
            Err(ValidationError::InsufficientLines)
        ));

        // Duplicate line IDs
        let duplicate = Event::JournalEntryPosted {
            entry_id: "entry-001".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            memo: "Test entry".to_string(),
            lines: vec![
                JournalLineData {
                    line_id: "line-001".to_string(),
                    account_id: "expense".to_string(),
                    amount: 10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
                JournalLineData {
                    line_id: "line-001".to_string(), // Duplicate!
                    account_id: "cash".to_string(),
                    amount: -10000,
                    currency: "USD".to_string(),
                    exchange_rate: None,
                    memo: None,
                },
            ],
            reference: None,
            source: None,
        };
        assert!(matches!(
            validate_event(&duplicate),
            Err(ValidationError::DuplicateId(_))
        ));
    }

    #[test]
    fn test_validate_account_created() {
        let valid = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "1000".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };
        assert!(validate_event(&valid).is_ok());

        let invalid_number = Event::AccountCreated {
            account_id: "acc-001".to_string(),
            account_type: EventAccountType::Asset,
            account_number: "".to_string(),
            name: "Cash".to_string(),
            parent_id: None,
            currency: None,
            description: None,
        };
        assert!(validate_event(&invalid_number).is_err());
    }
}
