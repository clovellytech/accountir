use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AccountError {
    #[error("Invalid account number: {0}")]
    InvalidAccountNumber(String),
    #[error("Account not found: {0}")]
    NotFound(String),
    #[error("Account is inactive: {0}")]
    Inactive(String),
    #[error("Cannot deactivate account with non-zero balance")]
    NonZeroBalance,
    #[error("Invalid parent account")]
    InvalidParent,
    #[error("Duplicate account number: {0}")]
    DuplicateAccountNumber(String),
}

/// Account type determines normal balance and financial statement placement
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountType {
    /// Assets - normal debit balance, Balance Sheet
    Asset,
    /// Liabilities - normal credit balance, Balance Sheet
    Liability,
    /// Equity - normal credit balance, Balance Sheet
    Equity,
    /// Revenue - normal credit balance, Income Statement
    Revenue,
    /// Expenses - normal debit balance, Income Statement
    Expense,
}

impl AccountType {
    /// Returns true if this account type has a normal debit balance
    pub fn is_normal_debit(&self) -> bool {
        matches!(self, AccountType::Asset | AccountType::Expense)
    }

    /// Returns true if this account type has a normal credit balance
    pub fn is_normal_credit(&self) -> bool {
        matches!(
            self,
            AccountType::Liability | AccountType::Equity | AccountType::Revenue
        )
    }

    /// Returns true if this account appears on the Balance Sheet
    pub fn is_balance_sheet(&self) -> bool {
        matches!(
            self,
            AccountType::Asset | AccountType::Liability | AccountType::Equity
        )
    }

    /// Returns true if this account appears on the Income Statement
    pub fn is_income_statement(&self) -> bool {
        matches!(self, AccountType::Revenue | AccountType::Expense)
    }

    /// Standard account number range for this type
    pub fn account_number_range(&self) -> (u32, u32) {
        match self {
            AccountType::Asset => (1000, 1999),
            AccountType::Liability => (2000, 2999),
            AccountType::Equity => (3000, 3999),
            AccountType::Revenue => (4000, 4999),
            AccountType::Expense => (5000, 9999),
        }
    }
}

impl std::fmt::Display for AccountType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccountType::Asset => write!(f, "Asset"),
            AccountType::Liability => write!(f, "Liability"),
            AccountType::Equity => write!(f, "Equity"),
            AccountType::Revenue => write!(f, "Revenue"),
            AccountType::Expense => write!(f, "Expense"),
        }
    }
}

/// An account in the chart of accounts
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub account_type: AccountType,
    pub account_number: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub currency: Option<String>,
    pub description: Option<String>,
    pub is_active: bool,
}

impl Account {
    pub fn new(
        id: String,
        account_type: AccountType,
        account_number: String,
        name: String,
    ) -> Result<Self, AccountError> {
        // Validate account number
        if account_number.is_empty() {
            return Err(AccountError::InvalidAccountNumber(
                "Account number cannot be empty".to_string(),
            ));
        }

        Ok(Self {
            id,
            account_type,
            account_number,
            name,
            parent_id: None,
            currency: None,
            description: None,
            is_active: true,
        })
    }

    pub fn with_parent(mut self, parent_id: String) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    pub fn with_currency(mut self, currency: String) -> Self {
        self.currency = Some(currency);
        self
    }

    pub fn with_description(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    /// Deactivate the account
    pub fn deactivate(&mut self) {
        self.is_active = false;
    }

    /// Reactivate the account
    pub fn reactivate(&mut self) {
        self.is_active = true;
    }

    /// Check if a given balance is in the normal direction for this account
    /// Returns true if positive balance for debit-normal accounts
    /// Returns true if negative balance for credit-normal accounts
    pub fn is_normal_balance(&self, amount: i64) -> bool {
        if self.account_type.is_normal_debit() {
            amount >= 0
        } else {
            amount <= 0
        }
    }
}

/// Standard chart of accounts template
pub struct ChartOfAccountsTemplate;

impl ChartOfAccountsTemplate {
    pub fn basic_accounts() -> Vec<(AccountType, &'static str, &'static str)> {
        vec![
            // Assets
            (AccountType::Asset, "1000", "Cash"),
            (AccountType::Asset, "1010", "Checking Account"),
            (AccountType::Asset, "1020", "Savings Account"),
            (AccountType::Asset, "1100", "Accounts Receivable"),
            (AccountType::Asset, "1200", "Inventory"),
            (AccountType::Asset, "1500", "Fixed Assets"),
            (AccountType::Asset, "1510", "Equipment"),
            (AccountType::Asset, "1520", "Accumulated Depreciation"),
            // Liabilities
            (AccountType::Liability, "2000", "Accounts Payable"),
            (AccountType::Liability, "2100", "Credit Card"),
            (AccountType::Liability, "2200", "Accrued Expenses"),
            (AccountType::Liability, "2500", "Notes Payable"),
            (AccountType::Liability, "2600", "Long-term Debt"),
            // Equity
            (AccountType::Equity, "3000", "Owner's Equity"),
            (AccountType::Equity, "3100", "Retained Earnings"),
            (AccountType::Equity, "3200", "Owner's Draws"),
            // Revenue
            (AccountType::Revenue, "4000", "Sales Revenue"),
            (AccountType::Revenue, "4100", "Service Revenue"),
            (AccountType::Revenue, "4200", "Interest Income"),
            (AccountType::Revenue, "4900", "Other Income"),
            // Expenses
            (AccountType::Expense, "5000", "Cost of Goods Sold"),
            (AccountType::Expense, "6000", "Salaries Expense"),
            (AccountType::Expense, "6100", "Rent Expense"),
            (AccountType::Expense, "6200", "Utilities Expense"),
            (AccountType::Expense, "6300", "Insurance Expense"),
            (AccountType::Expense, "6400", "Office Supplies"),
            (AccountType::Expense, "6500", "Depreciation Expense"),
            (AccountType::Expense, "6600", "Interest Expense"),
            (AccountType::Expense, "6900", "Miscellaneous Expense"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_account_type_normal_balance() {
        assert!(AccountType::Asset.is_normal_debit());
        assert!(AccountType::Expense.is_normal_debit());
        assert!(AccountType::Liability.is_normal_credit());
        assert!(AccountType::Equity.is_normal_credit());
        assert!(AccountType::Revenue.is_normal_credit());
    }

    #[test]
    fn test_account_type_financial_statement() {
        assert!(AccountType::Asset.is_balance_sheet());
        assert!(AccountType::Liability.is_balance_sheet());
        assert!(AccountType::Equity.is_balance_sheet());
        assert!(AccountType::Revenue.is_income_statement());
        assert!(AccountType::Expense.is_income_statement());
    }

    #[test]
    fn test_account_creation() {
        let account = Account::new(
            "acc-001".to_string(),
            AccountType::Asset,
            "1000".to_string(),
            "Cash".to_string(),
        )
        .unwrap();

        assert_eq!(account.account_number, "1000");
        assert_eq!(account.name, "Cash");
        assert!(account.is_active);
    }

    #[test]
    fn test_account_normal_balance() {
        let asset = Account::new(
            "acc-001".to_string(),
            AccountType::Asset,
            "1000".to_string(),
            "Cash".to_string(),
        )
        .unwrap();

        let liability = Account::new(
            "acc-002".to_string(),
            AccountType::Liability,
            "2000".to_string(),
            "Accounts Payable".to_string(),
        )
        .unwrap();

        // Asset has normal debit balance (positive)
        assert!(asset.is_normal_balance(1000));
        assert!(!asset.is_normal_balance(-1000));

        // Liability has normal credit balance (negative)
        assert!(liability.is_normal_balance(-1000));
        assert!(!liability.is_normal_balance(1000));
    }
}
