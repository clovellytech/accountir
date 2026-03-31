use crate::domain::money::Money;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum JournalEntryError {
    #[error("Entry is not balanced: sum is {0}, expected 0")]
    NotBalanced(i64),
    #[error("Entry must have at least two lines")]
    InsufficientLines,
    #[error("Entry date is in a closed period")]
    PeriodClosed,
    #[error("Entry has already been voided")]
    AlreadyVoided,
    #[error("Account not found: {0}")]
    AccountNotFound(String),
    #[error("Account is inactive: {0}")]
    AccountInactive(String),
    #[error("Currency mismatch in line")]
    CurrencyMismatch,
}

/// Source of the journal entry
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum EntrySource {
    #[default]
    Manual,
    Import,
    Recurring,
    System,
}

/// A single line in a journal entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalLine {
    pub line_id: String,
    pub account_id: String,
    /// Amount in smallest currency unit. Positive = debit, negative = credit
    pub amount: i64,
    pub currency: String,
    /// Exchange rate if not base currency
    pub exchange_rate: Option<rust_decimal::Decimal>,
    pub memo: Option<String>,
}

impl JournalLine {
    pub fn new(line_id: String, account_id: String, amount: i64, currency: String) -> Self {
        Self {
            line_id,
            account_id,
            amount,
            currency,
            exchange_rate: None,
            memo: None,
        }
    }

    pub fn with_exchange_rate(mut self, rate: rust_decimal::Decimal) -> Self {
        self.exchange_rate = Some(rate);
        self
    }

    pub fn with_memo(mut self, memo: String) -> Self {
        self.memo = Some(memo);
        self
    }

    /// Create a debit line
    pub fn debit(line_id: String, account_id: String, amount: i64, currency: String) -> Self {
        Self::new(line_id, account_id, amount.abs(), currency)
    }

    /// Create a credit line
    pub fn credit(line_id: String, account_id: String, amount: i64, currency: String) -> Self {
        Self::new(line_id, account_id, -amount.abs(), currency)
    }

    pub fn is_debit(&self) -> bool {
        self.amount > 0
    }

    pub fn is_credit(&self) -> bool {
        self.amount < 0
    }

    /// Convert amount to Money type
    pub fn to_money(&self) -> Money {
        Money::from_cents(self.amount, &self.currency)
    }
}

/// A complete journal entry with all lines
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub entry_id: String,
    pub date: NaiveDate,
    pub memo: String,
    pub lines: Vec<JournalLine>,
    pub reference: Option<String>,
    pub source: EntrySource,
    pub is_void: bool,
}

impl JournalEntry {
    /// Create a new journal entry (does not validate - use validate() after)
    pub fn new(entry_id: String, date: NaiveDate, memo: String, lines: Vec<JournalLine>) -> Self {
        Self {
            entry_id,
            date,
            memo,
            lines,
            reference: None,
            source: EntrySource::Manual,
            is_void: false,
        }
    }

    pub fn with_reference(mut self, reference: String) -> Self {
        self.reference = Some(reference);
        self
    }

    pub fn with_source(mut self, source: EntrySource) -> Self {
        self.source = source;
        self
    }

    /// Validate that the entry is balanced (sum of all lines = 0)
    pub fn validate(&self) -> Result<(), JournalEntryError> {
        if self.lines.len() < 2 {
            return Err(JournalEntryError::InsufficientLines);
        }

        let sum: i64 = self.lines.iter().map(|l| l.amount).sum();
        if sum != 0 {
            return Err(JournalEntryError::NotBalanced(sum));
        }

        Ok(())
    }

    /// Check if this entry is balanced
    pub fn is_balanced(&self) -> bool {
        self.lines.iter().map(|l| l.amount).sum::<i64>() == 0
    }

    /// Calculate total debits
    pub fn total_debits(&self) -> i64 {
        self.lines
            .iter()
            .filter(|l| l.is_debit())
            .map(|l| l.amount)
            .sum()
    }

    /// Calculate total credits (as positive number)
    pub fn total_credits(&self) -> i64 {
        self.lines
            .iter()
            .filter(|l| l.is_credit())
            .map(|l| l.amount.abs())
            .sum()
    }

    /// Mark this entry as voided
    pub fn void(&mut self) {
        self.is_void = true;
    }

    /// Get all unique account IDs in this entry
    pub fn account_ids(&self) -> Vec<&str> {
        self.lines
            .iter()
            .map(|l| l.account_id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }
}

/// Builder for creating journal entries
pub struct JournalEntryBuilder {
    entry_id: String,
    date: NaiveDate,
    memo: String,
    lines: Vec<JournalLine>,
    reference: Option<String>,
    source: EntrySource,
    next_line_id: usize,
}

impl JournalEntryBuilder {
    pub fn new(entry_id: String, date: NaiveDate, memo: String) -> Self {
        Self {
            entry_id,
            date,
            memo,
            lines: Vec::new(),
            reference: None,
            source: EntrySource::Manual,
            next_line_id: 1,
        }
    }

    pub fn reference(mut self, reference: String) -> Self {
        self.reference = Some(reference);
        self
    }

    pub fn source(mut self, source: EntrySource) -> Self {
        self.source = source;
        self
    }

    fn generate_line_id(&mut self) -> String {
        let id = format!("{}-line-{}", self.entry_id, self.next_line_id);
        self.next_line_id += 1;
        id
    }

    pub fn debit(mut self, account_id: &str, amount: i64, currency: &str) -> Self {
        let line_id = self.generate_line_id();
        self.lines.push(JournalLine::debit(
            line_id,
            account_id.to_string(),
            amount,
            currency.to_string(),
        ));
        self
    }

    pub fn credit(mut self, account_id: &str, amount: i64, currency: &str) -> Self {
        let line_id = self.generate_line_id();
        self.lines.push(JournalLine::credit(
            line_id,
            account_id.to_string(),
            amount,
            currency.to_string(),
        ));
        self
    }

    pub fn line(mut self, line: JournalLine) -> Self {
        self.lines.push(line);
        self
    }

    /// Build and validate the entry
    pub fn build(self) -> Result<JournalEntry, JournalEntryError> {
        let entry = JournalEntry {
            entry_id: self.entry_id,
            date: self.date,
            memo: self.memo,
            lines: self.lines,
            reference: self.reference,
            source: self.source,
            is_void: false,
        };
        entry.validate()?;
        Ok(entry)
    }

    /// Build without validation (use with caution)
    pub fn build_unchecked(self) -> JournalEntry {
        JournalEntry {
            entry_id: self.entry_id,
            date: self.date,
            memo: self.memo,
            lines: self.lines,
            reference: self.reference,
            source: self.source,
            is_void: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_balanced_entry() {
        let entry = JournalEntryBuilder::new(
            "entry-001".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            "Paid for supplies".to_string(),
        )
        .debit("supplies-expense", 10000, "USD") // $100.00 debit
        .credit("cash", 10000, "USD") // $100.00 credit
        .build()
        .unwrap();

        assert!(entry.is_balanced());
        assert_eq!(entry.total_debits(), 10000);
        assert_eq!(entry.total_credits(), 10000);
    }

    #[test]
    fn test_unbalanced_entry() {
        let result = JournalEntryBuilder::new(
            "entry-001".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            "Bad entry".to_string(),
        )
        .debit("supplies-expense", 10000, "USD")
        .credit("cash", 5000, "USD") // Not balanced!
        .build();

        assert!(result.is_err());
        if let Err(JournalEntryError::NotBalanced(sum)) = result {
            assert_eq!(sum, 5000);
        }
    }

    #[test]
    fn test_insufficient_lines() {
        let result = JournalEntryBuilder::new(
            "entry-001".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            "Single line".to_string(),
        )
        .debit("cash", 10000, "USD")
        .build();

        assert!(matches!(result, Err(JournalEntryError::InsufficientLines)));
    }

    #[test]
    fn test_multi_line_entry() {
        // Split payment: $100 supplies, $50 utilities, paid from cash
        let entry = JournalEntryBuilder::new(
            "entry-001".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
            "Office expenses".to_string(),
        )
        .debit("supplies-expense", 10000, "USD")
        .debit("utilities-expense", 5000, "USD")
        .credit("cash", 15000, "USD")
        .build()
        .unwrap();

        assert!(entry.is_balanced());
        assert_eq!(entry.lines.len(), 3);
        assert_eq!(entry.total_debits(), 15000);
        assert_eq!(entry.total_credits(), 15000);
    }
}
