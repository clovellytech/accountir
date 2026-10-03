//! The portfolio: what every investment account is worth at market, and how that
//! has moved.
//!
//! # What this is not
//!
//! It is not the books. The books carry a taxable account at cost, lot by lot, and
//! never mark it to market (INVESTMENTS-SPEC §3); they are built from the broker's
//! *transactions*. This is built from its *holdings* alone — quantity, price, value,
//! the broker's own basis — for every investment account behind a connection,
//! configured or not, sheltered or taxable. Nothing here is posted, and nothing here
//! is in the log: see migration 052 for why it is machine-local.
//!
//! The one place the two meet is [`PortfolioAccount::book_basis_cents`]: for a
//! taxable account the books are configured for, the basis of the lots still held,
//! beside the broker's basis. A difference there is the visible end of a trade the
//! books are missing.
//!
//! # The shape of a refresh
//!
//! One holdings read per connection, recorded as one snapshot per account per day.
//! The history is those snapshots: there is no other source of one, because the
//! provider has no historical holdings, so a day nobody refreshed is a day the chart
//! carries the last known value across.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

use super::investment_import::{self as ii, to_cents, to_micro_shares, ConversionError};

#[derive(Debug, Error)]
pub enum PortfolioError {
    #[error(transparent)]
    Conversion(#[from] ConversionError),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
}

// ---------------------------------------------------------------------------
// The payload, as the proxy serialises a holdings read
// ---------------------------------------------------------------------------
//
// Its own types rather than `investment_import`'s. Those are part of what the books
// import, and the fields this view needs — prices, their dates, the security's type,
// the account's balance — have no business on an event. Everything but the ids is
// optional, because everything but the ids really is absent at some institution.

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PayloadAccount {
    pub account_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub official_name: Option<String>,
    /// `investment`, `depository`… Plaid's own field is `type`; the proxy renames it.
    #[serde(default, alias = "type")]
    pub account_type: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub mask: Option<String>,
    #[serde(default)]
    pub balances: Option<PayloadBalances>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PayloadBalances {
    #[serde(default)]
    pub current: Option<f64>,
    #[serde(default)]
    pub iso_currency_code: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PayloadSecurity {
    pub security_id: String,
    #[serde(default, alias = "ticker_symbol")]
    pub ticker: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, alias = "type")]
    pub security_type: Option<String>,
    #[serde(default)]
    pub close_price: Option<f64>,
    #[serde(default)]
    pub close_price_as_of: Option<String>,
    #[serde(default)]
    pub is_cash_equivalent: Option<bool>,
    #[serde(default)]
    pub iso_currency_code: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PayloadHolding {
    pub account_id: String,
    pub security_id: String,
    #[serde(default)]
    pub security: Option<PayloadSecurity>,
    #[serde(default)]
    pub quantity: f64,
    /// For the **whole** holding, not per share.
    #[serde(default)]
    pub cost_basis: Option<f64>,
    #[serde(default)]
    pub institution_price: Option<f64>,
    #[serde(default)]
    pub institution_price_as_of: Option<String>,
    #[serde(default)]
    pub institution_value: Option<f64>,
    #[serde(default)]
    pub iso_currency_code: Option<String>,
}

/// One holdings read, parsed.
#[derive(Debug, Clone, Default)]
pub struct HoldingsPayload {
    pub accounts: Vec<PayloadAccount>,
    pub holdings: Vec<PayloadHolding>,
    /// Rows that would not parse and were left out. Counted rather than failing the
    /// read: one malformed holding must not cost the value of everything else.
    pub unreadable: usize,
}

impl HoldingsPayload {
    /// Parse a body row by row, so one bad row is one row short and not a failure.
    pub fn parse(body: &serde_json::Value) -> Self {
        fn rows<T: serde::de::DeserializeOwned>(
            body: &serde_json::Value,
            key: &str,
            unreadable: &mut usize,
        ) -> Vec<T> {
            body.get(key)
                .and_then(|v| v.as_array())
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| match serde_json::from_value(row.clone()) {
                            Ok(parsed) => Some(parsed),
                            Err(_) => {
                                *unreadable += 1;
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        let mut unreadable = 0;
        let accounts = rows(body, "accounts", &mut unreadable);
        let holdings = rows(body, "holdings", &mut unreadable);
        Self {
            accounts,
            holdings,
            unreadable,
        }
    }
}

// ---------------------------------------------------------------------------
// Recording a refresh
// ---------------------------------------------------------------------------

/// What one refresh recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    pub accounts: usize,
    pub holdings: usize,
    /// Accounts the provider listed that are not investment accounts — a checking
    /// account behind the same login — and so were left out.
    pub not_investments: usize,
    pub unreadable: usize,
}

/// Which accounts in a read belong in the portfolio.
///
/// One that holds something, one the provider types as an investment account, or
/// one somebody has configured as one — the same test the rest of the investments
/// code applies, and in the same order of trust, because some institutions report a
/// brokerage as `depository`. A chequing account behind the same login, holding
/// nothing, is not part of a portfolio.
fn is_portfolio_account(
    conn: &Connection,
    item_id: &str,
    account: &PayloadAccount,
    holds_something: bool,
) -> bool {
    holds_something
        || account.account_type.as_deref().is_some_and(|t| {
            matches!(
                t.trim().to_ascii_lowercase().as_str(),
                "investment" | "brokerage"
            )
        })
        || ii::get_config(conn, item_id, &account.account_id).is_some()
}

/// A price, to millionths of a currency unit.
fn to_micros(price: f64) -> Result<i64, ConversionError> {
    ii::scale(price, 1_000_000.0, "millionths")
}

/// One line of a snapshot, before it is written.
#[derive(Debug, Clone, PartialEq)]
struct Line {
    plaid_security_id: String,
    quantity: i64,
    price_micros: Option<i64>,
    price_as_of: Option<String>,
    value_cents: Option<i64>,
    cost_basis_cents: Option<i64>,
    currency: Option<String>,
}

fn line(holding: &PayloadHolding) -> Result<Line, ConversionError> {
    let security = holding.security.as_ref();
    let (price, price_as_of) = match (holding.institution_price, security) {
        (Some(p), _) => (Some(p), holding.institution_price_as_of.clone()),
        (None, Some(s)) => (s.close_price, s.close_price_as_of.clone()),
        (None, None) => (None, None),
    };
    // The broker's value when it gave one. Otherwise quantity times price, which is
    // what the broker's value is — computed in floating point once, here, and then
    // rounded to cents like every other provider amount.
    let value = holding
        .institution_value
        .or_else(|| price.map(|p| p * holding.quantity));
    Ok(Line {
        plaid_security_id: holding.security_id.clone(),
        quantity: to_micro_shares(holding.quantity)?,
        price_micros: price.map(to_micros).transpose()?,
        price_as_of,
        value_cents: value.map(to_cents).transpose()?,
        cost_basis_cents: holding.cost_basis.map(to_cents).transpose()?,
        currency: holding
            .iso_currency_code
            .clone()
            .or_else(|| security.and_then(|s| s.iso_currency_code.clone())),
    })
}

/// Both, or neither: a sum missing one of its parts is not the sum.
fn add_both(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    Some(a? + b?)
}

/// Record one connection's holdings read as today's snapshot of each of its
/// investment accounts.
///
/// A second refresh on the same day replaces that day's snapshot rather than adding
/// one: the later read is the better one. An investment account the read lists with
/// nothing in it gets an empty snapshot, because an emptied account is a fact the
/// history has to show — skipping it would carry its last value forward for ever.
pub fn record_refresh(
    conn: &Connection,
    item_id: &str,
    as_of: NaiveDate,
    fetched_at: &str,
    payload: &HoldingsPayload,
) -> Result<RefreshReport, PortfolioError> {
    let mut report = RefreshReport {
        unreadable: payload.unreadable,
        ..Default::default()
    };

    let mut by_account: BTreeMap<&str, Vec<&PayloadHolding>> = BTreeMap::new();
    for holding in &payload.holdings {
        by_account
            .entry(holding.account_id.as_str())
            .or_default()
            .push(holding);
    }

    // Each account to snapshot, with how the read described it. A holding whose
    // account the read did not list still counts — it is in the portfolio whatever
    // the account list says — and is named from what the bank feed recorded.
    let mut accounts: BTreeMap<&str, PayloadAccount> = BTreeMap::new();
    for account in &payload.accounts {
        let holds = by_account.contains_key(account.account_id.as_str());
        if is_portfolio_account(conn, item_id, account, holds) {
            accounts.insert(account.account_id.as_str(), account.clone());
        } else {
            report.not_investments += 1;
        }
    }
    for account_id in by_account.keys() {
        accounts
            .entry(account_id)
            .or_insert_with(|| PayloadAccount {
                account_id: account_id.to_string(),
                name: recorded_name(conn, item_id, account_id)
                    .unwrap_or_else(|| "Investment account".to_string()),
                ..Default::default()
            });
    }

    let tx = conn.unchecked_transaction()?;
    for (account_id, account) in &accounts {
        // One line per security. Plaid sends one holding per security per account, but
        // nothing here depends on that: two are merged rather than one overwriting
        // the other under the primary key.
        let mut lines: BTreeMap<String, Line> = BTreeMap::new();
        for holding in by_account.get(account_id).into_iter().flatten() {
            let next = line(holding)?;
            match lines.get_mut(&next.plaid_security_id) {
                Some(existing) => {
                    existing.quantity += next.quantity;
                    existing.value_cents = add_both(existing.value_cents, next.value_cents);
                    existing.cost_basis_cents =
                        add_both(existing.cost_basis_cents, next.cost_basis_cents);
                }
                None => {
                    lines.insert(next.plaid_security_id.clone(), next);
                }
            }
            if let Some(security) = &holding.security {
                upsert_security(&tx, security, fetched_at)?;
            }
        }

        tx.execute(
            "DELETE FROM portfolio_holdings WHERE snapshot_id IN (
                 SELECT snapshot_id FROM portfolio_snapshots
                  WHERE item_id = ?1 AND plaid_account_id = ?2 AND as_of = ?3)",
            params![item_id, account_id, as_of.to_string()],
        )?;
        tx.execute(
            "DELETE FROM portfolio_snapshots
              WHERE item_id = ?1 AND plaid_account_id = ?2 AND as_of = ?3",
            params![item_id, account_id, as_of.to_string()],
        )?;

        let balance = account.balances.as_ref();
        let snapshot_id = Uuid::new_v4().to_string();
        let name = if account.name.trim().is_empty() {
            account
                .official_name
                .clone()
                .unwrap_or_else(|| "Investment account".to_string())
        } else {
            account.name.clone()
        };
        tx.execute(
            "INSERT INTO portfolio_snapshots
                 (snapshot_id, item_id, plaid_account_id, as_of, fetched_at, account_name,
                  account_subtype, mask, balance_cents, currency)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                snapshot_id,
                item_id,
                account_id,
                as_of.to_string(),
                fetched_at,
                name,
                account.subtype,
                account.mask,
                balance.and_then(|b| b.current).map(to_cents).transpose()?,
                balance.and_then(|b| b.iso_currency_code.clone()),
            ],
        )?;
        for l in lines.values() {
            tx.execute(
                "INSERT INTO portfolio_holdings
                     (snapshot_id, plaid_security_id, quantity, price_micros, price_as_of,
                      value_cents, cost_basis_cents, currency)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    snapshot_id,
                    l.plaid_security_id,
                    l.quantity,
                    l.price_micros,
                    l.price_as_of,
                    l.value_cents,
                    l.cost_basis_cents,
                    l.currency,
                ],
            )?;
            report.holdings += 1;
        }
        report.accounts += 1;
    }
    tx.commit()?;
    Ok(report)
}

/// What the bank feed recorded this account as called, for a holding whose account
/// the read itself did not describe.
fn recorded_name(conn: &Connection, item_id: &str, plaid_account_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT name FROM plaid_local_accounts WHERE item_id = ?1 AND plaid_account_id = ?2",
        [item_id, plaid_account_id],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Keep what the provider last said a security is. A field it has stopped sending
/// keeps its old value: a close price that is missing today is not a price of zero.
fn upsert_security(
    conn: &Connection,
    security: &PayloadSecurity,
    fetched_at: &str,
) -> Result<(), PortfolioError> {
    conn.execute(
        "INSERT INTO portfolio_securities
             (plaid_security_id, name, ticker, security_type, is_cash_equivalent,
              close_price_micros, close_price_as_of, currency, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(plaid_security_id) DO UPDATE SET
             name = COALESCE(excluded.name, name),
             ticker = COALESCE(excluded.ticker, ticker),
             security_type = COALESCE(excluded.security_type, security_type),
             is_cash_equivalent = excluded.is_cash_equivalent,
             close_price_micros = COALESCE(excluded.close_price_micros, close_price_micros),
             close_price_as_of = COALESCE(excluded.close_price_as_of, close_price_as_of),
             currency = COALESCE(excluded.currency, currency),
             updated_at = excluded.updated_at",
        params![
            security.security_id,
            security.name,
            security
                .ticker
                .as_deref()
                .map(|t| t.trim().to_uppercase())
                .filter(|t| !t.is_empty()),
            security.security_type,
            security.is_cash_equivalent.unwrap_or(false),
            security.close_price.map(to_micros).transpose()?,
            security.close_price_as_of,
            security.iso_currency_code,
            fetched_at,
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Reading it back
// ---------------------------------------------------------------------------

/// What kind of thing a holding is, for the allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AssetClass {
    Stocks,
    Etfs,
    MutualFunds,
    Bonds,
    Cash,
    Crypto,
    Other,
}

impl AssetClass {
    /// From Plaid's security type. Cash first: a money-market fund is typed
    /// `mutual fund` and flagged as a cash equivalent, and it is cash to anyone
    /// asking what their allocation is.
    pub fn of(security_type: Option<&str>, is_cash_equivalent: bool) -> Self {
        if is_cash_equivalent {
            return AssetClass::Cash;
        }
        match security_type
            .map(|t| t.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("equity") => AssetClass::Stocks,
            Some("etf") => AssetClass::Etfs,
            Some("mutual fund") => AssetClass::MutualFunds,
            Some("fixed income") => AssetClass::Bonds,
            Some("cash") => AssetClass::Cash,
            Some("cryptocurrency") => AssetClass::Crypto,
            _ => AssetClass::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AssetClass::Stocks => "Stocks",
            AssetClass::Etfs => "ETFs",
            AssetClass::MutualFunds => "Mutual funds",
            AssetClass::Bonds => "Bonds",
            AssetClass::Cash => "Cash",
            AssetClass::Crypto => "Crypto",
            AssetClass::Other => "Other",
        }
    }
}

/// One holding in one account, as of that account's latest refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioHolding {
    pub plaid_security_id: String,
    pub ticker: Option<String>,
    pub name: String,
    pub class: AssetClass,
    /// Micro-shares.
    pub quantity: i64,
    pub price_micros: Option<i64>,
    pub price_as_of: Option<NaiveDate>,
    pub value_cents: Option<i64>,
    /// The broker's basis for the whole holding.
    pub cost_basis_cents: Option<i64>,
}

impl PortfolioHolding {
    /// Ticker where there is one, which is what people recognise; the name where
    /// there is not, which is most of what is in a 401(k).
    pub fn label(&self) -> &str {
        self.ticker.as_deref().unwrap_or(&self.name)
    }

    /// The basis to count. Cash has none to speak of — a dollar cost a dollar — and
    /// brokers often leave it blank, which must not make the account's basis unknown.
    fn effective_basis_cents(&self) -> Option<i64> {
        match (self.cost_basis_cents, self.class) {
            (Some(basis), _) => Some(basis),
            (None, AssetClass::Cash) => self.value_cents,
            (None, _) => None,
        }
    }

    pub fn gain_cents(&self) -> Option<i64> {
        Some(self.value_cents? - self.effective_basis_cents()?)
    }
}

/// One investment account, as of its latest refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortfolioAccount {
    pub item_id: String,
    pub plaid_account_id: String,
    pub institution: String,
    pub name: String,
    pub subtype: Option<String>,
    pub mask: Option<String>,
    pub as_of: NaiveDate,
    pub fetched_at: String,
    /// What the institution says the account is worth.
    pub balance_cents: Option<i64>,
    pub holdings: Vec<PortfolioHolding>,
    /// The basis of the lots the **books** still hold, for a taxable account they
    /// are configured for; `None` for every other account.
    pub book_basis_cents: Option<i64>,
}

impl PortfolioAccount {
    /// `Fidelity · Brokerage ••1234`.
    pub fn label(&self) -> String {
        let mut s = format!("{} · {}", self.institution, self.name);
        if let Some(mask) = self.mask.as_deref().filter(|m| !m.is_empty()) {
            s.push_str(&format!(" ••{mask}"));
        }
        s
    }

    /// The sum of the holdings that have a value. See [`Self::unvalued`] for how
    /// many do not, which a display has to say rather than let this pass as a total.
    pub fn value_cents(&self) -> i64 {
        self.holdings.iter().filter_map(|h| h.value_cents).sum()
    }

    pub fn unvalued(&self) -> usize {
        self.holdings
            .iter()
            .filter(|h| h.value_cents.is_none())
            .count()
    }

    /// The broker's basis for the account, or `None` if any holding's is unknown —
    /// a basis missing one holding would make the gain on the rest look like a gain
    /// on the lot.
    pub fn broker_basis_cents(&self) -> Option<i64> {
        self.holdings
            .iter()
            .map(PortfolioHolding::effective_basis_cents)
            .try_fold(0i64, |acc, b| Some(acc + b?))
    }

    pub fn gain_cents(&self) -> Option<i64> {
        Some(self.value_cents() - self.broker_basis_cents()?)
    }

    /// The institution's balance and the holdings' sum, when they disagree by more
    /// than a dollar: a holding the read left out, or a balance that counts
    /// something the holdings do not (margin, pending settlement).
    pub fn balance_gap_cents(&self) -> Option<i64> {
        let gap = self.balance_cents? - self.value_cents();
        (gap.abs() > 100).then_some(gap)
    }

    /// The books' basis less the broker's, when both are known and they differ.
    pub fn basis_gap_cents(&self) -> Option<i64> {
        let gap = self.book_basis_cents? - self.broker_basis_cents()?;
        (gap != 0).then_some(gap)
    }
}

/// One security across every account that holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityPosition {
    pub ticker: Option<String>,
    pub name: String,
    pub class: AssetClass,
    pub quantity: i64,
    pub value_cents: i64,
    /// `None` if any account's basis for it is unknown.
    pub cost_basis_cents: Option<i64>,
    pub price_micros: Option<i64>,
    /// `(account label, micro-shares, value)`, largest first.
    pub accounts: Vec<(String, i64, Option<i64>)>,
}

impl SecurityPosition {
    pub fn label(&self) -> &str {
        self.ticker.as_deref().unwrap_or(&self.name)
    }

    pub fn gain_cents(&self) -> Option<i64> {
        Some(self.value_cents - self.cost_basis_cents?)
    }
}

/// Every investment account's latest refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Portfolio {
    pub accounts: Vec<PortfolioAccount>,
}

impl Portfolio {
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn value_cents(&self) -> i64 {
        self.accounts
            .iter()
            .map(PortfolioAccount::value_cents)
            .sum()
    }

    pub fn unvalued(&self) -> usize {
        self.accounts.iter().map(PortfolioAccount::unvalued).sum()
    }

    pub fn broker_basis_cents(&self) -> Option<i64> {
        self.accounts
            .iter()
            .map(PortfolioAccount::broker_basis_cents)
            .try_fold(0i64, |acc, b| Some(acc + b?))
    }

    pub fn gain_cents(&self) -> Option<i64> {
        Some(self.value_cents() - self.broker_basis_cents()?)
    }

    /// The oldest and newest price dates behind the total, cash aside. Usually one
    /// day — the last close — and worth showing when it is not, because a total
    /// mixing Friday's prices with Monday's is a total of two different markets.
    pub fn prices_as_of(&self) -> Option<(NaiveDate, NaiveDate)> {
        let dates = self
            .accounts
            .iter()
            .flat_map(|a| &a.holdings)
            .filter(|h| h.class != AssetClass::Cash)
            .filter_map(|h| h.price_as_of);
        dates.fold(None, |range, d| match range {
            None => Some((d, d)),
            Some((lo, hi)) => Some((lo.min(d), hi.max(d))),
        })
    }

    /// The oldest refresh behind the total: if one account was last read a month ago,
    /// its month-old value is in today's figure, and the page has to be able to say so.
    pub fn oldest_refresh(&self) -> Option<NaiveDate> {
        self.accounts.iter().map(|a| a.as_of).min()
    }

    /// Each security once, combined across accounts, largest first.
    ///
    /// Combined on the ticker where there is one — the same fund held at two brokers
    /// has two provider ids and is one position — and on the provider's id where
    /// there is not, since two untickered holdings with the same name need not be
    /// the same thing.
    pub fn by_security(&self) -> Vec<SecurityPosition> {
        let mut out: BTreeMap<String, SecurityPosition> = BTreeMap::new();
        for account in &self.accounts {
            let label = account.label();
            for h in &account.holdings {
                let key = match &h.ticker {
                    Some(t) => format!("t:{t}"),
                    None => format!("p:{}", h.plaid_security_id),
                };
                let p = out.entry(key).or_insert_with(|| SecurityPosition {
                    ticker: h.ticker.clone(),
                    name: h.name.clone(),
                    class: h.class,
                    quantity: 0,
                    value_cents: 0,
                    cost_basis_cents: Some(0),
                    price_micros: h.price_micros,
                    accounts: Vec::new(),
                });
                p.quantity += h.quantity;
                p.value_cents += h.value_cents.unwrap_or(0);
                p.cost_basis_cents = add_both(p.cost_basis_cents, h.effective_basis_cents());
                p.price_micros = p.price_micros.or(h.price_micros);
                p.accounts.push((label.clone(), h.quantity, h.value_cents));
            }
        }
        let mut positions: Vec<SecurityPosition> = out.into_values().collect();
        for p in &mut positions {
            p.accounts
                .sort_by(|a, b| b.2.unwrap_or(0).cmp(&a.2.unwrap_or(0)));
        }
        positions.sort_by(|a, b| {
            b.value_cents
                .cmp(&a.value_cents)
                .then_with(|| a.label().cmp(b.label()))
        });
        positions
    }

    /// Value by asset class, largest first, leaving out the classes with none.
    pub fn allocation(&self) -> Vec<(AssetClass, i64)> {
        let mut by: BTreeMap<AssetClass, i64> = BTreeMap::new();
        for h in self.accounts.iter().flat_map(|a| &a.holdings) {
            *by.entry(h.class).or_default() += h.value_cents.unwrap_or(0);
        }
        let mut out: Vec<(AssetClass, i64)> = by.into_iter().filter(|(_, v)| *v != 0).collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

/// Every investment account's latest refresh, for the connections the books still
/// hold. A connection that has been removed takes its accounts with it: its last
/// value is not part of what anybody owns through these books any more.
pub fn latest(conn: &Connection) -> Portfolio {
    let Ok(mut stmt) = conn.prepare(
        "SELECT s.snapshot_id, s.item_id, s.plaid_account_id, s.as_of, s.fetched_at,
                s.account_name, s.account_subtype, s.mask, s.balance_cents, i.institution_name
           FROM portfolio_snapshots s
           JOIN plaid_items i ON i.id = s.item_id
          WHERE i.status = 'active'
            AND s.as_of = (SELECT MAX(s2.as_of) FROM portfolio_snapshots s2
                            WHERE s2.item_id = s.item_id
                              AND s2.plaid_account_id = s.plaid_account_id)
          ORDER BY i.institution_name, s.account_name, s.plaid_account_id",
    ) else {
        return Portfolio::default();
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<i64>>(8)?,
            r.get::<_, String>(9)?,
        ))
    });
    let Ok(rows) = rows else {
        return Portfolio::default();
    };
    let mut accounts = Vec::new();
    for (
        snapshot_id,
        item_id,
        plaid_account_id,
        as_of,
        fetched_at,
        name,
        subtype,
        mask,
        balance,
        institution,
    ) in rows.flatten()
    {
        let Some(as_of) = parse_date(&as_of) else {
            continue;
        };
        let book_basis_cents =
            ii::holdings(conn, &item_id, &plaid_account_id).map(|h| h.cost_cents());
        accounts.push(PortfolioAccount {
            holdings: holdings_of(conn, &snapshot_id),
            item_id,
            plaid_account_id,
            institution,
            name,
            subtype,
            mask,
            as_of,
            fetched_at,
            balance_cents: balance,
            book_basis_cents,
        });
    }
    Portfolio { accounts }
}

fn holdings_of(conn: &Connection, snapshot_id: &str) -> Vec<PortfolioHolding> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT h.plaid_security_id, h.quantity, h.price_micros, h.price_as_of,
                h.value_cents, h.cost_basis_cents,
                sec.name, sec.ticker, sec.security_type, COALESCE(sec.is_cash_equivalent, 0)
           FROM portfolio_holdings h
           LEFT JOIN portfolio_securities sec ON sec.plaid_security_id = h.plaid_security_id
          WHERE h.snapshot_id = ?1
          ORDER BY COALESCE(h.value_cents, 0) DESC, h.plaid_security_id",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([snapshot_id], |r| {
        let plaid_security_id: String = r.get(0)?;
        let ticker: Option<String> = r.get(7)?;
        let name: Option<String> = r.get(6)?;
        let security_type: Option<String> = r.get(8)?;
        let is_cash: bool = r.get(9)?;
        Ok(PortfolioHolding {
            name: name
                .or_else(|| ticker.clone())
                .unwrap_or_else(|| plaid_security_id.clone()),
            class: AssetClass::of(security_type.as_deref(), is_cash),
            ticker,
            quantity: r.get(1)?,
            price_micros: r.get(2)?,
            price_as_of: r
                .get::<_, Option<String>>(3)?
                .as_deref()
                .and_then(parse_date),
            value_cents: r.get(4)?,
            cost_basis_cents: r.get(5)?,
            plaid_security_id,
        })
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// The portfolio's total on each day anything was refreshed.
///
/// An account not refreshed on a given day contributes its last known value, so a
/// day on which one connection was read is not a day the others were worth nothing.
/// Before an account's first refresh it contributes nothing — there is no earlier
/// value to carry — which is why a line that starts low and steps up on the day a
/// new connection was first read is the truth about what was known, not a gain.
pub fn history(conn: &Connection) -> Vec<(NaiveDate, i64)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT s.item_id, s.plaid_account_id, s.as_of,
                COALESCE((SELECT SUM(h.value_cents) FROM portfolio_holdings h
                           WHERE h.snapshot_id = s.snapshot_id), 0)
           FROM portfolio_snapshots s
           JOIN plaid_items i ON i.id = s.item_id
          WHERE i.status = 'active'
          ORDER BY s.as_of",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };

    let mut current: BTreeMap<(String, String), i64> = BTreeMap::new();
    let mut points: Vec<(NaiveDate, i64)> = Vec::new();
    for (item_id, account_id, as_of, value) in rows.flatten() {
        let Some(date) = parse_date(&as_of) else {
            continue;
        };
        current.insert((item_id, account_id), value);
        let total = current.values().sum();
        match points.last_mut() {
            Some((d, t)) if *d == date => *t = total,
            _ => points.push((date, total)),
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::EventStore;

    fn store() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO plaid_items (id, proxy_item_id, institution_name, status)
                 VALUES ('item1', 'proxy1', 'Fidelity', 'active'),
                        ('item2', 'proxy2', 'Vanguard', 'active')",
                [],
            )
            .unwrap();
        store
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, d).unwrap()
    }

    fn payload(body: serde_json::Value) -> HoldingsPayload {
        let p = HoldingsPayload::parse(&body);
        assert_eq!(p.unreadable, 0, "{body}");
        p
    }

    fn holding(
        account: &str,
        sec: &str,
        ticker: &str,
        kind: &str,
        qty: f64,
        value: f64,
    ) -> serde_json::Value {
        serde_json::json!({
            "account_id": account,
            "security_id": sec,
            "security": {
                "security_id": sec, "ticker": ticker, "name": format!("{ticker} fund"),
                "security_type": kind, "close_price": value / qty,
                "close_price_as_of": "2026-09-30", "is_cash_equivalent": kind == "cash",
            },
            "quantity": qty,
            "cost_basis": value * 0.8,
            "institution_price": value / qty,
            "institution_price_as_of": "2026-09-30",
            "institution_value": value,
            "iso_currency_code": "USD",
        })
    }

    fn account(id: &str, ty: &str, balance: Option<f64>) -> serde_json::Value {
        serde_json::json!({
            "account_id": id, "name": format!("Account {id}"), "account_type": ty,
            "subtype": "brokerage", "mask": "1234",
            "balances": balance.map(|b| serde_json::json!({"current": b, "iso_currency_code": "USD"})),
        })
    }

    /// The portfolio is what is held, at market, for every investment account —
    /// configured or not, which is the difference from the books' own snapshot.
    #[test]
    fn a_refresh_records_every_investment_account_at_market() {
        let store = store();
        let conn = store.connection();
        let body = serde_json::json!({
            "accounts": [account("brk", "investment", Some(1500.0))],
            "holdings": [
                holding("brk", "s-vti", "VTI", "etf", 4.0, 1000.0),
                holding("brk", "s-cash", "CUR:USD", "cash", 500.0, 500.0),
            ],
        });
        let report = record_refresh(
            conn,
            "item1",
            day(1),
            "2026-10-01T09:00:00Z",
            &payload(body),
        )
        .unwrap();
        assert_eq!(report.accounts, 1);
        assert_eq!(report.holdings, 2);

        let p = latest(conn);
        assert_eq!(p.accounts.len(), 1, "{p:?}");
        let a = &p.accounts[0];
        assert_eq!(a.label(), "Fidelity · Account brk ••1234");
        assert_eq!(a.value_cents(), 150_000);
        assert_eq!(a.balance_gap_cents(), None, "balance and holdings agree");
        assert_eq!(
            a.book_basis_cents, None,
            "not configured, so the books say nothing"
        );
        assert_eq!(p.value_cents(), 150_000);
        assert_eq!(
            p.allocation(),
            vec![(AssetClass::Etfs, 100_000), (AssetClass::Cash, 50_000)]
        );
        let vti = &p.by_security()[0];
        assert_eq!(vti.label(), "VTI");
        assert_eq!(vti.price_micros, Some(250_000_000));
        assert_eq!(vti.gain_cents(), Some(20_000));
    }

    /// The same fund at two brokers is one position, with each account beneath it.
    #[test]
    fn a_security_held_in_two_accounts_is_one_position() {
        let store = store();
        let conn = store.connection();
        record_refresh(
            conn,
            "item1",
            day(1),
            "t",
            &payload(serde_json::json!({
                "accounts": [account("a", "investment", None)],
                "holdings": [holding("a", "fid-vti", "VTI", "etf", 2.0, 500.0)],
            })),
        )
        .unwrap();
        record_refresh(
            conn,
            "item2",
            day(1),
            "t",
            &payload(serde_json::json!({
                "accounts": [account("b", "investment", None)],
                "holdings": [holding("b", "van-vti", "VTI", "etf", 6.0, 1500.0)],
            })),
        )
        .unwrap();

        let positions = latest(conn).by_security();
        assert_eq!(positions.len(), 1, "{positions:?}");
        let vti = &positions[0];
        assert_eq!(vti.quantity, 8_000_000);
        assert_eq!(vti.value_cents, 200_000);
        assert_eq!(vti.accounts.len(), 2);
        assert!(
            vti.accounts[0].0.starts_with("Vanguard"),
            "largest first: {vti:?}"
        );
    }

    /// A chequing account behind the same login, holding nothing, is not part of a
    /// portfolio — but a brokerage the bank calls `depository` is, once it holds
    /// something.
    #[test]
    fn only_investment_accounts_are_recorded() {
        let store = store();
        let conn = store.connection();
        let report = record_refresh(
            conn,
            "item1",
            day(1),
            "t",
            &payload(serde_json::json!({
                "accounts": [
                    account("chk", "depository", Some(2000.0)),
                    account("cma", "depository", None),
                ],
                "holdings": [holding("cma", "s1", "SPAXX", "cash", 10.0, 10.0)],
            })),
        )
        .unwrap();
        assert_eq!(report.accounts, 1);
        assert_eq!(report.not_investments, 1);
        let p = latest(conn);
        assert_eq!(p.accounts[0].plaid_account_id, "cma");
    }

    /// One snapshot a day: the second refresh replaces the first.
    #[test]
    fn a_second_refresh_on_the_same_day_replaces_the_first() {
        let store = store();
        let conn = store.connection();
        for value in [1000.0, 1200.0] {
            record_refresh(
                conn,
                "item1",
                day(1),
                "t",
                &payload(serde_json::json!({
                    "accounts": [account("brk", "investment", None)],
                    "holdings": [holding("brk", "s1", "VTI", "etf", 4.0, value)],
                })),
            )
            .unwrap();
        }
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM portfolio_snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(latest(conn).value_cents(), 120_000);
        assert_eq!(history(conn), vec![(day(1), 120_000)]);
    }

    /// A day on which one connection was read is not a day the others were worth
    /// nothing; and an emptied account goes to zero rather than keeping its value.
    #[test]
    fn history_carries_each_account_forward_and_an_emptied_account_drops_out() {
        let store = store();
        let conn = store.connection();
        let one = |item: &str, acct: &str, d: u32, holdings: serde_json::Value| {
            record_refresh(
                conn,
                item,
                day(d),
                "t",
                &payload(serde_json::json!({
                    "accounts": [account(acct, "investment", None)],
                    "holdings": holdings,
                })),
            )
            .unwrap();
        };
        one(
            "item1",
            "a",
            1,
            serde_json::json!([holding("a", "s1", "VTI", "etf", 1.0, 100.0)]),
        );
        one(
            "item2",
            "b",
            2,
            serde_json::json!([holding("b", "s2", "BND", "etf", 1.0, 50.0)]),
        );
        one(
            "item1",
            "a",
            3,
            serde_json::json!([holding("a", "s1", "VTI", "etf", 1.0, 110.0)]),
        );
        one("item2", "b", 4, serde_json::json!([]));

        assert_eq!(
            history(conn),
            vec![
                (day(1), 10_000),
                (day(2), 15_000),
                (day(3), 16_000),
                (day(4), 11_000),
            ]
        );
    }

    /// No institution value: quantity times price, which is what it would have been.
    #[test]
    fn a_missing_value_is_computed_from_the_price() {
        let store = store();
        let conn = store.connection();
        let mut h = holding("brk", "s1", "VTI", "etf", 3.0, 300.0);
        h["institution_value"] = serde_json::Value::Null;
        h["institution_price"] = serde_json::json!(101.25);
        record_refresh(
            conn,
            "item1",
            day(1),
            "t",
            &payload(serde_json::json!({"accounts": [account("brk", "investment", None)], "holdings": [h]})),
        )
        .unwrap();
        assert_eq!(latest(conn).value_cents(), 30_375);
    }

    /// A disconnected connection is no longer something owned through these books.
    #[test]
    fn a_removed_connection_leaves_the_portfolio() {
        let store = store();
        let conn = store.connection();
        record_refresh(
            conn,
            "item1",
            day(1),
            "t",
            &payload(serde_json::json!({
                "accounts": [account("brk", "investment", None)],
                "holdings": [holding("brk", "s1", "VTI", "etf", 1.0, 100.0)],
            })),
        )
        .unwrap();
        conn.execute(
            "UPDATE plaid_items SET status = 'disconnected' WHERE id = 'item1'",
            [],
        )
        .unwrap();
        assert!(latest(conn).is_empty());
        assert!(history(conn).is_empty());
    }

    /// One malformed row is one row short, not a failed refresh.
    #[test]
    fn an_unreadable_row_is_counted_not_fatal() {
        let p = HoldingsPayload::parse(&serde_json::json!({
            "accounts": [account("brk", "investment", None)],
            "holdings": [
                holding("brk", "s1", "VTI", "etf", 1.0, 100.0),
                {"account_id": "brk"},
            ],
        }));
        assert_eq!(p.holdings.len(), 1);
        assert_eq!(p.unreadable, 1);
    }

    #[test]
    fn a_money_market_fund_is_cash() {
        assert_eq!(AssetClass::of(Some("mutual fund"), true), AssetClass::Cash);
        assert_eq!(
            AssetClass::of(Some("mutual fund"), false),
            AssetClass::MutualFunds
        );
        assert_eq!(AssetClass::of(Some("Equity"), false), AssetClass::Stocks);
        assert_eq!(AssetClass::of(None, false), AssetClass::Other);
    }
}
