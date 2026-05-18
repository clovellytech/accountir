use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ReconciliationError {
    #[error("Reconciliation not found: {0}")]
    NotFound(String),
    #[error("Reconciliation already completed")]
    AlreadyCompleted,
    #[error("Reconciliation was abandoned")]
    Abandoned,
    #[error("Transaction already cleared")]
    AlreadyCleared,
    #[error("Transaction not cleared")]
    NotCleared,
    #[error("Reconciliation has a difference of {0}")]
    HasDifference(i64),
    #[error("Invalid account for reconciliation")]
    InvalidAccount,
}

/// Status of a bank reconciliation
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationStatus {
    InProgress,
    Completed,
    Abandoned,
}

/// A cleared transaction in a reconciliation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearedTransaction {
    pub entry_id: String,
    pub line_id: String,
    pub cleared_amount: i64,
    pub cleared_at: chrono::DateTime<chrono::Utc>,
}

/// A bank reconciliation session
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reconciliation {
    pub id: String,
    pub account_id: String,
    pub statement_date: NaiveDate,
    /// Statement ending balance in cents (positive for asset accounts)
    pub statement_ending_balance: i64,
    pub status: ReconciliationStatus,
    pub cleared_transactions: Vec<ClearedTransaction>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl Reconciliation {
    pub fn new(
        id: String,
        account_id: String,
        statement_date: NaiveDate,
        statement_ending_balance: i64,
    ) -> Self {
        Self {
            id,
            account_id,
            statement_date,
            statement_ending_balance,
            status: ReconciliationStatus::InProgress,
            cleared_transactions: Vec::new(),
            started_at: chrono::Utc::now(),
            completed_at: None,
        }
    }

    pub fn is_in_progress(&self) -> bool {
        self.status == ReconciliationStatus::InProgress
    }

    pub fn is_completed(&self) -> bool {
        self.status == ReconciliationStatus::Completed
    }

    pub fn is_abandoned(&self) -> bool {
        self.status == ReconciliationStatus::Abandoned
    }

    /// Clear a transaction
    pub fn clear_transaction(
        &mut self,
        entry_id: String,
        line_id: String,
        amount: i64,
    ) -> Result<(), ReconciliationError> {
        if !self.is_in_progress() {
            if self.is_completed() {
                return Err(ReconciliationError::AlreadyCompleted);
            } else {
                return Err(ReconciliationError::Abandoned);
            }
        }

        // Check if already cleared
        if self.is_transaction_cleared(&entry_id, &line_id) {
            return Err(ReconciliationError::AlreadyCleared);
        }

        self.cleared_transactions.push(ClearedTransaction {
            entry_id,
            line_id,
            cleared_amount: amount,
            cleared_at: chrono::Utc::now(),
        });

        Ok(())
    }

    /// Unclear a transaction
    pub fn unclear_transaction(
        &mut self,
        entry_id: &str,
        line_id: &str,
    ) -> Result<(), ReconciliationError> {
        if !self.is_in_progress() {
            if self.is_completed() {
                return Err(ReconciliationError::AlreadyCompleted);
            } else {
                return Err(ReconciliationError::Abandoned);
            }
        }

        let initial_len = self.cleared_transactions.len();
        self.cleared_transactions
            .retain(|t| !(t.entry_id == entry_id && t.line_id == line_id));

        if self.cleared_transactions.len() == initial_len {
            return Err(ReconciliationError::NotCleared);
        }

        Ok(())
    }

    /// Check if a transaction is cleared
    pub fn is_transaction_cleared(&self, entry_id: &str, line_id: &str) -> bool {
        self.cleared_transactions
            .iter()
            .any(|t| t.entry_id == entry_id && t.line_id == line_id)
    }

    /// Calculate the total of cleared transactions
    pub fn cleared_balance(&self) -> i64 {
        self.cleared_transactions
            .iter()
            .map(|t| t.cleared_amount)
            .sum()
    }

    /// Calculate the difference between statement and cleared balance
    /// A difference of 0 means the reconciliation is balanced
    pub fn calculate_difference(&self, beginning_balance: i64) -> i64 {
        let book_balance = beginning_balance + self.cleared_balance();
        self.statement_ending_balance - book_balance
    }

    /// Complete the reconciliation
    pub fn complete(&mut self, _difference: i64) -> Result<(), ReconciliationError> {
        if !self.is_in_progress() {
            if self.is_completed() {
                return Err(ReconciliationError::AlreadyCompleted);
            } else {
                return Err(ReconciliationError::Abandoned);
            }
        }

        // Note: We allow completing with a difference (user accepts it)
        // The difference is recorded in the event

        self.status = ReconciliationStatus::Completed;
        self.completed_at = Some(chrono::Utc::now());
        Ok(())
    }

    /// Abandon the reconciliation
    pub fn abandon(&mut self) -> Result<(), ReconciliationError> {
        if !self.is_in_progress() {
            if self.is_completed() {
                return Err(ReconciliationError::AlreadyCompleted);
            } else {
                return Err(ReconciliationError::Abandoned);
            }
        }

        self.status = ReconciliationStatus::Abandoned;
        Ok(())
    }
}

/// Summary of a reconciliation for display
#[derive(Debug, Clone)]
pub struct ReconciliationSummary {
    pub statement_ending_balance: i64,
    pub beginning_balance: i64,
    pub cleared_deposits: i64,
    pub cleared_payments: i64,
    pub cleared_balance: i64,
    pub uncleared_deposits: i64,
    pub uncleared_payments: i64,
    pub difference: i64,
}

impl ReconciliationSummary {
    pub fn is_balanced(&self) -> bool {
        self.difference == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_reconciliation() {
        let recon = Reconciliation::new(
            "recon-001".to_string(),
            "checking-account".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            100000, // $1000.00
        );

        assert!(recon.is_in_progress());
        assert_eq!(recon.cleared_transactions.len(), 0);
        assert_eq!(recon.cleared_balance(), 0);
    }

    #[test]
    fn test_clear_transaction() {
        let mut recon = Reconciliation::new(
            "recon-001".to_string(),
            "checking-account".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            100000,
        );

        recon
            .clear_transaction("entry-001".to_string(), "line-001".to_string(), 5000)
            .unwrap();

        assert_eq!(recon.cleared_transactions.len(), 1);
        assert_eq!(recon.cleared_balance(), 5000);
        assert!(recon.is_transaction_cleared("entry-001", "line-001"));
    }

    #[test]
    fn test_unclear_transaction() {
        let mut recon = Reconciliation::new(
            "recon-001".to_string(),
            "checking-account".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            100000,
        );

        recon
            .clear_transaction("entry-001".to_string(), "line-001".to_string(), 5000)
            .unwrap();
        recon.unclear_transaction("entry-001", "line-001").unwrap();

        assert_eq!(recon.cleared_transactions.len(), 0);
        assert!(!recon.is_transaction_cleared("entry-001", "line-001"));
    }

    #[test]
    fn test_double_clear_error() {
        let mut recon = Reconciliation::new(
            "recon-001".to_string(),
            "checking-account".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            100000,
        );

        recon
            .clear_transaction("entry-001".to_string(), "line-001".to_string(), 5000)
            .unwrap();

        let result = recon.clear_transaction("entry-001".to_string(), "line-001".to_string(), 5000);
        assert!(matches!(result, Err(ReconciliationError::AlreadyCleared)));
    }

    #[test]
    fn test_calculate_difference() {
        let mut recon = Reconciliation::new(
            "recon-001".to_string(),
            "checking-account".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            100000, // Statement shows $1000.00
        );

        // Beginning balance was $500.00
        let beginning_balance = 50000;

        // Clear deposits of $600.00
        recon
            .clear_transaction("entry-001".to_string(), "line-001".to_string(), 60000)
            .unwrap();

        // Clear payments of -$100.00
        recon
            .clear_transaction("entry-002".to_string(), "line-001".to_string(), -10000)
            .unwrap();

        // Book balance = 50000 + 60000 - 10000 = 100000
        // Statement = 100000
        // Difference = 0
        let diff = recon.calculate_difference(beginning_balance);
        assert_eq!(diff, 0);
    }

    #[test]
    fn test_complete_reconciliation() {
        let mut recon = Reconciliation::new(
            "recon-001".to_string(),
            "checking-account".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            100000,
        );

        recon.complete(0).unwrap();
        assert!(recon.is_completed());
        assert!(recon.completed_at.is_some());

        // Cannot complete again
        let result = recon.complete(0);
        assert!(matches!(result, Err(ReconciliationError::AlreadyCompleted)));
    }

    #[test]
    fn test_abandon_reconciliation() {
        let mut recon = Reconciliation::new(
            "recon-001".to_string(),
            "checking-account".to_string(),
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
            100000,
        );

        recon.abandon().unwrap();
        assert!(recon.is_abandoned());

        // Cannot clear after abandonment
        let result = recon.clear_transaction("entry-001".to_string(), "line-001".to_string(), 5000);
        assert!(matches!(result, Err(ReconciliationError::Abandoned)));
    }
}
