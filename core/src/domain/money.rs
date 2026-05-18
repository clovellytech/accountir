use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Add, Neg, Sub};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MoneyError {
    #[error("Currency mismatch: cannot operate on {0} and {1}")]
    CurrencyMismatch(String, String),
    #[error("Invalid amount: {0}")]
    InvalidAmount(String),
    #[error("Exchange rate required for currency conversion")]
    ExchangeRateRequired,
}

/// Currency code (ISO 4217)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Currency {
    pub code: String,
    pub name: String,
    pub symbol: String,
    pub decimal_places: u8,
}

impl Currency {
    pub fn new(code: &str, name: &str, symbol: &str, decimal_places: u8) -> Self {
        Self {
            code: code.to_uppercase(),
            name: name.to_string(),
            symbol: symbol.to_string(),
            decimal_places,
        }
    }

    pub fn usd() -> Self {
        Self::new("USD", "US Dollar", "$", 2)
    }

    pub fn eur() -> Self {
        Self::new("EUR", "Euro", "\u{20AC}", 2)
    }

    pub fn gbp() -> Self {
        Self::new("GBP", "British Pound", "\u{00A3}", 2)
    }

    /// Returns the multiplier to convert from decimal to smallest unit
    /// For USD (2 decimal places), this is 100
    pub fn smallest_unit_multiplier(&self) -> i64 {
        10_i64.pow(self.decimal_places as u32)
    }
}

impl Default for Currency {
    fn default() -> Self {
        Self::usd()
    }
}

/// Money represented as signed integer in smallest currency unit (cents for USD)
/// Positive = Debit, Negative = Credit
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    /// Amount in smallest currency unit (cents for USD)
    /// Positive = Debit, Negative = Credit
    pub amount: i64,
    pub currency_code: String,
}

impl Money {
    /// Create a new Money from the smallest currency unit (cents)
    pub fn from_cents(amount: i64, currency_code: &str) -> Self {
        Self {
            amount,
            currency_code: currency_code.to_uppercase(),
        }
    }

    /// Create a new Money from a decimal amount
    pub fn from_decimal(amount: Decimal, currency: &Currency) -> Self {
        let multiplier = Decimal::from(currency.smallest_unit_multiplier());
        let cents = (amount * multiplier)
            .round()
            .to_string()
            .parse::<i64>()
            .unwrap_or(0);
        Self {
            amount: cents,
            currency_code: currency.code.clone(),
        }
    }

    /// Create a debit (positive) amount
    pub fn debit(amount: i64, currency_code: &str) -> Self {
        Self::from_cents(amount.abs(), currency_code)
    }

    /// Create a credit (negative) amount
    pub fn credit(amount: i64, currency_code: &str) -> Self {
        Self::from_cents(-amount.abs(), currency_code)
    }

    /// Check if this is a debit (positive)
    pub fn is_debit(&self) -> bool {
        self.amount > 0
    }

    /// Check if this is a credit (negative)
    pub fn is_credit(&self) -> bool {
        self.amount < 0
    }

    /// Check if this is zero
    pub fn is_zero(&self) -> bool {
        self.amount == 0
    }

    /// Get the absolute value
    pub fn abs(&self) -> Self {
        Self {
            amount: self.amount.abs(),
            currency_code: self.currency_code.clone(),
        }
    }

    /// Convert to decimal amount
    pub fn to_decimal(&self, decimal_places: u8) -> Decimal {
        let multiplier = Decimal::from(10_i64.pow(decimal_places as u32));
        Decimal::from(self.amount) / multiplier
    }

    /// Add two Money values (must be same currency)
    pub fn add(&self, other: &Money) -> Result<Money, MoneyError> {
        if self.currency_code != other.currency_code {
            return Err(MoneyError::CurrencyMismatch(
                self.currency_code.clone(),
                other.currency_code.clone(),
            ));
        }
        Ok(Money {
            amount: self.amount + other.amount,
            currency_code: self.currency_code.clone(),
        })
    }

    /// Subtract two Money values (must be same currency)
    pub fn sub(&self, other: &Money) -> Result<Money, MoneyError> {
        if self.currency_code != other.currency_code {
            return Err(MoneyError::CurrencyMismatch(
                self.currency_code.clone(),
                other.currency_code.clone(),
            ));
        }
        Ok(Money {
            amount: self.amount - other.amount,
            currency_code: self.currency_code.clone(),
        })
    }

    /// Convert to another currency using an exchange rate
    pub fn convert(&self, to_currency: &str, exchange_rate: Decimal) -> Money {
        let converted = Decimal::from(self.amount) * exchange_rate;
        Money {
            amount: converted.round().to_string().parse::<i64>().unwrap_or(0),
            currency_code: to_currency.to_uppercase(),
        }
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Simple display: amount / 100 for 2 decimal places
        let decimal = self.to_decimal(2);
        write!(f, "{} {}", decimal, self.currency_code)
    }
}

impl Add for Money {
    type Output = Result<Money, MoneyError>;

    fn add(self, other: Money) -> Self::Output {
        Money::add(&self, &other)
    }
}

impl Sub for Money {
    type Output = Result<Money, MoneyError>;

    fn sub(self, other: Money) -> Self::Output {
        Money::sub(&self, &other)
    }
}

impl Neg for Money {
    type Output = Money;

    fn neg(self) -> Self::Output {
        Money {
            amount: -self.amount,
            currency_code: self.currency_code,
        }
    }
}

/// Exchange rate between two currencies
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeRate {
    pub from_currency: String,
    pub to_currency: String,
    pub rate: Decimal,
    pub effective_date: chrono::NaiveDate,
}

impl ExchangeRate {
    pub fn new(
        from_currency: &str,
        to_currency: &str,
        rate: Decimal,
        effective_date: chrono::NaiveDate,
    ) -> Self {
        Self {
            from_currency: from_currency.to_uppercase(),
            to_currency: to_currency.to_uppercase(),
            rate,
            effective_date,
        }
    }

    /// Get the inverse rate (to -> from)
    pub fn inverse(&self) -> Self {
        Self {
            from_currency: self.to_currency.clone(),
            to_currency: self.from_currency.clone(),
            rate: Decimal::from(1) / self.rate,
            effective_date: self.effective_date,
        }
    }

    /// Convert an amount using this rate
    pub fn convert(&self, amount: &Money) -> Result<Money, MoneyError> {
        if amount.currency_code != self.from_currency {
            return Err(MoneyError::CurrencyMismatch(
                amount.currency_code.clone(),
                self.from_currency.clone(),
            ));
        }
        Ok(amount.convert(&self.to_currency, self.rate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_money_from_cents() {
        let m = Money::from_cents(10000, "USD");
        assert_eq!(m.amount, 10000);
        assert_eq!(m.currency_code, "USD");
    }

    #[test]
    fn test_money_debit_credit() {
        let debit = Money::debit(100, "USD");
        let credit = Money::credit(100, "USD");

        assert!(debit.is_debit());
        assert!(!debit.is_credit());
        assert!(!credit.is_debit());
        assert!(credit.is_credit());
        assert_eq!(debit.amount, 100);
        assert_eq!(credit.amount, -100);
    }

    #[test]
    fn test_money_add_same_currency() {
        let a = Money::from_cents(100, "USD");
        let b = Money::from_cents(50, "USD");
        let result = Money::add(&a, &b).unwrap();
        assert_eq!(result.amount, 150);
    }

    #[test]
    fn test_money_add_different_currency() {
        let a = Money::from_cents(100, "USD");
        let b = Money::from_cents(50, "EUR");
        let result = Money::add(&a, &b);
        assert!(result.is_err());
    }

    #[test]
    fn test_balanced_entry() {
        // Supplies expense (debit) + Cash (credit) = 0
        let debit = Money::debit(10000, "USD");
        let credit = Money::credit(10000, "USD");
        let sum = Money::add(&debit, &credit).unwrap();
        assert!(sum.is_zero());
    }

    #[test]
    fn test_exchange_rate_conversion() {
        let rate = ExchangeRate::new(
            "USD",
            "EUR",
            Decimal::new(85, 2),
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
        );
        let usd = Money::from_cents(10000, "USD"); // $100.00
        let eur = rate.convert(&usd).unwrap();
        assert_eq!(eur.currency_code, "EUR");
        assert_eq!(eur.amount, 8500); // 85 EUR
    }
}
