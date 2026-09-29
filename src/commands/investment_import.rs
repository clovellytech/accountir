//! The investments importer: provider trades into ledger entries, a holdings
//! snapshot into a sheltered account's value, and a reconciliation report that
//! tells you when the two disagree.
//!
//! INVESTMENTS-SPEC.md phase 4, on top of phase 1's lot register
//! ([`super::investment_commands`]), phase 2's sheltered-account register
//! ([`super::retirement_commands`]) and phase 3's proxy endpoints.
//!
//! # What this module refuses to do
//!
//! Almost all of the design here is about what is *not* imported, so it is worth
//! saying first.
//!
//! * **Nothing posts without configuration.** Which ledger account a brokerage's
//!   dividends belong in is not derivable from anything the provider sends.
//!   Activity for an account nobody has configured is **held** — written to a local
//!   review table with its raw payload — because the alternatives are guessing
//!   (invisible until a return is prepared from it) and dropping (a transaction
//!   nobody will ever see again). That is the stance
//!   [`super::plaid_commands::stage_transactions_in_conn`] already takes for the
//!   bank feed.
//! * **Nothing inside a sheltered account is imported at all.** Not its buys, not
//!   its sales, not its dividends — spec §2b. Its holdings snapshot drives one
//!   value update per fetch, and that is the whole of it.
//! * **A corporate action is never guessed.** A split, a merger, a spin-off or a
//!   return of capital arrives as a `transfer` or as nothing at all, and spec §7 is
//!   explicit about why guessing is worse than holding: a split applied wrongly
//!   silently restates every gain on that security, for ever, and a wrong basis is
//!   worse than a missing one because nobody goes looking for it.
//! * **Cash into or out of a sheltered account is never posted.** The provider
//!   cannot say whether it is a contribution or a distribution, and the two have
//!   opposite effects on a return.
//! * **Plaid's cost basis never posts anything.** Our basis is the sum of the buys
//!   we imported. The broker's figure is the cross-check (spec §7), and being the
//!   check is a different job from being the source.
//!
//! # Floats stop at the boundary
//!
//! The provider sends IEEE floats for every quantity and every amount. They are
//! converted **once**, here, with explicit rounding, by [`to_cents`] and
//! [`to_micro_shares`], and no `f64` reaches a command. A float that got as far as
//! a lot's basis would make a holding that cannot reconcile against a statement,
//! because a float cannot represent 0.1 — which is the reason phase 1 stores
//! integers in the first place.
//!
//! # The fetch window
//!
//! `/investments/transactions/get` is date-ranged, not cursor-based, so there is no
//! server-side position to resume from and the window is chosen here: full history
//! on an account's first import, then a rolling 30 days on top of the last date
//! fetched (spec §6). Every run therefore re-reads a month it has already posted,
//! which is what makes deduplication load-bearing on every single run rather than
//! only after a mistake.
//!
//! # Deduplication is under the write lock, in one batch with the posting
//!
//! Keyed on `investment_transaction_id`, checked inside the append transaction, and
//! recorded by an event appended **in the same batch** as the trade and its journal
//! entry. Not belt-and-braces: a purchase whose import record failed to land would
//! be re-imported on the next fetch with a freshly minted lot id, which sails past
//! migration 014's reference fence because the reference contains that id — and the
//! same purchase is then deducted twice on a Form 8949.

use chrono::{Days, Months, NaiveDate};
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;
use thiserror::Error;
use uuid::Uuid;

use crate::commands::investment_commands::{
    self, build_buy_in_txn, build_define_security_in_txn, build_fee_in_txn, build_income_in_txn,
    build_sell_in_txn, BuySecurityCommand, ChargeInvestmentFeeCommand, InvestmentStep,
    LotSelection, NewSecurity, RecordInvestmentIncomeCommand, SellSecurityCommand,
};
use crate::commands::retirement_commands::{self, SetRetirementValueCommand, ValueSet};
use crate::events::types::{
    Event, EventEnvelope, HoldingsSnapshotData, ImportedActivityKind, InvestmentAccountConfigData,
    InvestmentActivityImportedData, InvestmentIncomeKind, InvestmentPostingAccounts,
    InvestmentTreatment, PlaidSecurityLinkData, SnapshotHoldingData, StoredEvent,
    TaxableBrokerageAccounts,
};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;

/// How far back the rolling re-fetch reaches on top of the last date fetched.
///
/// Spec §6's assumed 30 days. It exists because a provider can and does revise a
/// transaction after reporting it, and because a fetch that ended mid-settlement
/// leaves the last day or two incomplete. Thirty days past that is cheap: every
/// re-read row is discarded by the dedup fence in one indexed lookup.
pub const REFETCH_DAYS: u64 = 30;

/// How far back an account's **first** import reaches.
///
/// "Full history" in practice: no institution retains investment transactions for
/// ten years, so this asks for everything anyone has while still being a bounded,
/// valid date range the proxy will accept. An unbounded start date is not on offer
/// — the endpoint requires both ends — and picking a real epoch date would put a
/// fixed floor into the code that is wrong the moment the calendar passes it.
pub const FIRST_FETCH_YEARS: u32 = 10;

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// A command refused this transaction under the write lock. Not a failure of
    /// the import run: the row is held for review carrying this message, which is
    /// why the text has to read as a sentence somebody can act on.
    #[error("{0}")]
    Refused(String),
    /// This transaction cannot be posted, and the reason is one the review list has
    /// a category for. Carried as an error rather than returned as a value because
    /// it can be discovered at any depth of the posting path — after the security
    /// has been resolved, after the amounts have been converted — and every one of
    /// those places has to end the same way: held, with the reason attached.
    #[error("{}", detail.clone().unwrap_or_else(|| reason.guidance().to_string()))]
    Held {
        reason: HoldReason,
        detail: Option<String>,
    },
    #[error("No Plaid connection with id {0}")]
    NoSuchItem(String),
    #[error("No account with id {0}")]
    NoSuchAccount(String),
    #[error(
        "Account {account_id} is {found}, and a {role} has to be {wanted}. Check the accounts \
         are not in the wrong order."
    )]
    WrongAccountType {
        account_id: String,
        role: &'static str,
        wanted: &'static str,
        found: String,
    },
    #[error(
        "Account {0} is not on the retirement register. Register it first: whether a distribution \
         out of it is taxable depends on what kind of account it is, and only that register \
         records it."
    )]
    NotOnRetirementRegister(String),
    #[error("Invalid configuration: {0}")]
    Invalid(String),
    #[error("{0}")]
    Conversion(#[from] ConversionError),
}

impl From<EventStoreError> for ImportError {
    fn from(e: EventStoreError) -> Self {
        ImportError::Store(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// The boundary: provider floats become integers here and nowhere else
// ---------------------------------------------------------------------------

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConversionError {
    #[error(
        "the provider sent {unit} that is not a finite number, which is not a quantity of \
         anything"
    )]
    NotFinite { unit: &'static str },
    #[error(
        "the provider sent {value} {unit}, which is past the point where a floating-point number \
         can still name a whole one of them"
    )]
    TooLarge { value: String, unit: &'static str },
}

/// The largest scaled magnitude worth trusting: 2^53, above which an `f64` cannot
/// represent consecutive integers at all.
///
/// Rejecting beyond it rather than saturating, because a saturated basis is a
/// number that looks like money and is not, and the failure it causes (a holding
/// that never reconciles) is far harder to find than a refusal at the boundary.
const MAX_EXACT: f64 = 9_007_199_254_740_992.0;

/// Money, as the provider sends it, to cents.
///
/// Half away from zero — `f64::round` — which is the rounding a broker's own
/// statement uses and the only one that does not quietly bias a long series of
/// amounts in one direction. Done once, here: `0.1 + 0.2` is `0.30000000000000004`
/// in binary floating point, and the *only* safe moment to decide that it is 30
/// cents is at the boundary, before the value is ever added to anything.
pub fn to_cents(amount: f64) -> Result<i64, ConversionError> {
    scale(amount, 100.0, "cents")
}

/// A quantity of shares, as the provider sends it, to micro-shares (millionths).
///
/// Phase 1's unit. Six places is past every brokerage's own precision, so in
/// practice nothing is lost; a provider that sent more places than that is rounded
/// here, deliberately and in one place, rather than truncated in several.
pub fn to_micro_shares(quantity: f64) -> Result<i64, ConversionError> {
    scale(
        quantity,
        investment_commands::MICRO_SHARE as f64,
        "micro-shares",
    )
}

fn scale(value: f64, factor: f64, unit: &'static str) -> Result<i64, ConversionError> {
    if !value.is_finite() {
        return Err(ConversionError::NotFinite { unit });
    }
    let scaled = (value * factor).round();
    if scaled.abs() > MAX_EXACT {
        return Err(ConversionError::TooLarge {
            value: format!("{value}"),
            unit,
        });
    }
    Ok(scaled as i64)
}

// ---------------------------------------------------------------------------
// The provider's payloads, as the proxy serialises them
// ---------------------------------------------------------------------------

/// One security, embedded in a holding or a transaction.
///
/// Every field but the id is optional, because every field but the id really is
/// absent sometimes — a private fund with no ticker, a bond with no CUSIP in
/// Plaid's table, a cash-equivalent sweep with neither.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderSecurity {
    pub security_id: String,
    #[serde(default)]
    pub ticker: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub security_type: Option<String>,
    #[serde(default)]
    pub cusip: Option<String>,
    #[serde(default)]
    pub isin: Option<String>,
    #[serde(default)]
    pub iso_currency_code: Option<String>,
}

/// One investment account, as the holdings and transactions endpoints describe it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderAccount {
    pub account_id: String,
    #[serde(default)]
    pub name: String,
    /// `brokerage`, `ira`, `401k`, `hsa`… The proxy passes it through without
    /// forming an opinion, on purpose: which model applies is a bookkeeping rule
    /// that changes with jurisdiction and belongs here. See [`classify_subtype`].
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub mask: Option<String>,
}

/// One investment transaction.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderInvestmentTransaction {
    pub investment_transaction_id: String,
    pub account_id: String,
    #[serde(default)]
    pub security_id: Option<String>,
    #[serde(default)]
    pub security: Option<ProviderSecurity>,
    /// `YYYY-MM-DD`.
    pub date: String,
    #[serde(default)]
    pub name: String,
    /// Plaid's `type`: `buy`, `sell`, `cash`, `fee`, `transfer`, `cancel`.
    pub transaction_type: String,
    pub subtype: String,
    #[serde(default)]
    pub quantity: f64,
    #[serde(default)]
    pub price: f64,
    #[serde(default)]
    pub fees: Option<f64>,
    /// Positive when cash is debited (a purchase), negative when it is credited (a
    /// sale, a dividend). The **cash that actually moved**, which is why it is what
    /// the ledger is built from rather than `price * quantity`.
    #[serde(default)]
    pub amount: f64,
    #[serde(default)]
    pub iso_currency_code: Option<String>,
}

/// One holding.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderHolding {
    pub account_id: String,
    pub security_id: String,
    #[serde(default)]
    pub security: Option<ProviderSecurity>,
    pub quantity: f64,
    /// The broker's basis for the **whole** holding, not per share. `None` when it
    /// does not know — a lot transferred in from another custodian usually has none.
    #[serde(default)]
    pub cost_basis: Option<f64>,
    #[serde(default)]
    pub institution_value: Option<f64>,
    #[serde(default)]
    pub iso_currency_code: Option<String>,
}

// ---------------------------------------------------------------------------
// Which model an account is imported under
// ---------------------------------------------------------------------------

/// What a provider subtype says about an account, and whether it said it clearly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtypeVerdict {
    pub treatment: InvestmentTreatment,
    /// `false` when the subtype is not one spec §2 lists. The treatment is then an
    /// **assumption** and wants confirming.
    pub recognised: bool,
}

/// Spec §2's list, plus `roth ira`.
///
/// The addition is deliberate and is the only one: Plaid's subtype for a Roth IRA
/// is `roth`, but `roth ira` appears in the wild too, and the sheltered answer for
/// it is certainly right. Nothing else is guessed — see [`classify_subtype`] for
/// why guessing here fails in the expensive direction.
const SHELTERED_SUBTYPES: [&str; 10] = [
    "401k",
    "403b",
    "ira",
    "roth",
    "roth ira",
    "roth 401k",
    "sep ira",
    "simple ira",
    "529",
    "hsa",
];

const TAXABLE_SUBTYPES: [&str; 2] = ["brokerage", "cash management"];

/// Which model spec §2 says a provider subtype means.
///
/// **An unrecognised subtype is taxable and flagged**, which is not a coin toss.
/// Treating an unknown account as sheltered would exclude everything in it from
/// every tax report by construction (phase 2 forces the value-change account off
/// every line), and the mistake is invisible: a return comes out smaller and
/// nothing says why. Treating it as taxable produces the opposite failure — income
/// reported that did not have to be — which somebody notices, can correct, and is
/// not a filing offence. So the assumption is made in the direction that surfaces.
pub fn classify_subtype(subtype: Option<&str>) -> SubtypeVerdict {
    let normalised = subtype
        .map(|s| s.trim().to_lowercase().replace(['_', '-'], " "))
        .unwrap_or_default();
    if SHELTERED_SUBTYPES.contains(&normalised.as_str()) {
        SubtypeVerdict {
            treatment: InvestmentTreatment::Sheltered,
            recognised: true,
        }
    } else if TAXABLE_SUBTYPES.contains(&normalised.as_str()) {
        SubtypeVerdict {
            treatment: InvestmentTreatment::Taxable,
            recognised: true,
        }
    } else {
        SubtypeVerdict {
            treatment: InvestmentTreatment::Taxable,
            recognised: false,
        }
    }
}

// ---------------------------------------------------------------------------
// What a provider transaction becomes
// ---------------------------------------------------------------------------

/// What an importable transaction posts as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostAs {
    Buy,
    Sell,
    Income(InvestmentIncomeKind),
    Fee,
    /// Cash into or out of a taxable account, against the configured clearing
    /// account.
    Cash,
}

/// Why a transaction was held rather than posted.
///
/// Each of these is a case where the honest answer is "a person has to look", and
/// each is held with the raw provider payload beside it so that the person can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// No configuration for the account it arrived in.
    Unconfigured,
    /// A split, a merger, a spin-off, a return of capital. Spec §7: guessing one
    /// silently restates every gain on that security.
    CorporateAction,
    /// A type or subtype this importer has no rule for — including options, short
    /// sales and capital-gain distributions, all out of scope for v1 (spec §10).
    UnhandledType,
    /// Cash moving into or out of a sheltered account: a contribution or a
    /// distribution, and the provider cannot say which.
    ShelteredCash,
    /// Cash moving in or out of a taxable account with no clearing account
    /// configured, so there is nowhere truthful to put the other leg.
    NoClearingAccount,
    /// A trade with no security attached, which cannot become a lot.
    UnknownSecurity,
    /// A quantity or an amount that did not survive the conversion to integers.
    BadAmount,
    /// A command refused it under the write lock — a sale larger than the position,
    /// a posting into a closed year.
    Refused,
}

impl HoldReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            HoldReason::Unconfigured => "unconfigured",
            HoldReason::CorporateAction => "corporate_action",
            HoldReason::UnhandledType => "unhandled_type",
            HoldReason::ShelteredCash => "sheltered_cash",
            HoldReason::NoClearingAccount => "no_clearing_account",
            HoldReason::UnknownSecurity => "unknown_security",
            HoldReason::BadAmount => "bad_amount",
            HoldReason::Refused => "rejected",
        }
    }

    /// What to do about it, for the review list. A held row whose reason nobody can
    /// act on is a row that stays held for ever.
    pub fn guidance(&self) -> &'static str {
        match self {
            HoldReason::Unconfigured => {
                "Configure this brokerage account — which securities, cash, income, gain and fee \
                 accounts its activity posts to — and import again."
            }
            HoldReason::CorporateAction => {
                "This looks like a corporate action: a split, a merger, a spin-off or a return of \
                 capital. Enter it by hand against the lots it affects. Applying a split \
                 automatically would restate every gain on the security, so it is never guessed."
            }
            HoldReason::UnhandledType => {
                "No rule covers this type of activity. Options, short sales and capital-gain \
                 distributions are out of scope for now; enter it by hand if it belongs in the \
                 books."
            }
            HoldReason::ShelteredCash => {
                "Cash moved into or out of a sheltered account. Only you can say whether that is a \
                 contribution or a distribution, and the two are opposite on a return — record it \
                 with the retirement commands."
            }
            HoldReason::NoClearingAccount => {
                "Cash moved in or out and there is no clearing account configured for this \
                 brokerage, so there is nowhere to put the other leg. Configure one, or enter the \
                 transfer against the bank account it came from."
            }
            HoldReason::UnknownSecurity => {
                "The provider sent a trade with no security attached, so there is no holding to \
                 put it against. Enter it by hand."
            }
            HoldReason::BadAmount => {
                "The provider's quantity, amount or date could not be read as an exact number of \
                 shares, cents or a day. Enter it by hand from the statement."
            }
            HoldReason::Refused => {
                "The books refused this one — the message above says why. It usually means an \
                 earlier transaction is missing, or the period it belongs to is closed."
            }
        }
    }
}

/// What the importer will do with one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Post(PostAs),
    /// A trade inside a sheltered account: ignored **by design**, not held. Nothing
    /// inside one is taxable, so there is no question for a person to answer and a
    /// review list full of a target-date fund's four hundred yearly trades would
    /// bury the things that do need answering (spec §2b).
    IgnoreSheltered,
    Hold(HoldReason),
}

/// Subtypes that mean the shares themselves changed shape. Checked **before** the
/// transaction type, because a split can arrive as a `buy` and posting it as an
/// ordinary purchase would invent a basis nobody paid.
const CORPORATE_ACTION_SUBTYPES: [&str; 6] = [
    "merger",
    "spin off",
    "split",
    "stock distribution",
    "return of capital",
    "reverse split",
];

/// Options and short sales: out of scope for v1 (spec §10), and each would need a
/// model of its own rather than a rule bent to fit.
const OUT_OF_SCOPE_SUBTYPES: [&str; 5] = [
    "assignment",
    "exercise",
    "expire",
    "sell short",
    "buy to cover",
];

/// Plaid's dividend subtypes. Exact strings, never a substring test: `dividend
/// reinvestment` contains "dividend" and is a *purchase*, and a contains-match
/// would post the reinvested shares as income instead of buying them.
const DIVIDEND_SUBTYPES: [&str; 4] = [
    "dividend",
    "qualified dividend",
    "non qualified dividend",
    "dividend reinvestment",
];

const INTEREST_SUBTYPES: [&str; 3] = ["interest", "interest receivable", "interest reinvestment"];

/// Cash arriving or leaving. `contribution` and `distribution` are in here because
/// a taxable brokerage does use them for an ordinary deposit and withdrawal; in a
/// **sheltered** account the same words mean something a person has to confirm, and
/// [`plan`] sends them to review instead.
const CASH_MOVEMENT_SUBTYPES: [&str; 6] = [
    "deposit",
    "withdrawal",
    "contribution",
    "distribution",
    "transfer",
    "cash",
];

fn normalise(s: &str) -> String {
    s.trim().to_lowercase().replace(['_', '-'], " ")
}

/// Decide what a provider transaction becomes, from its type, its subtype and the
/// account's treatment. Pure: no database, no floats, no side effects, so it is the
/// one thing in this module that can be exhaustively tested against a list of
/// subtypes.
pub fn plan(treatment: InvestmentTreatment, transaction_type: &str, subtype: &str) -> Plan {
    let ty = normalise(transaction_type);
    let sub = normalise(subtype);

    // Sheltered first, and before the corporate-action check, because inside a
    // sheltered account a split is as uninteresting as a trade: nothing in there is
    // recorded, so there is nothing for it to restate.
    if treatment == InvestmentTreatment::Sheltered {
        return if is_cash_movement(&ty, &sub) {
            Plan::Hold(HoldReason::ShelteredCash)
        } else {
            Plan::IgnoreSheltered
        };
    }

    if CORPORATE_ACTION_SUBTYPES.contains(&sub.as_str()) {
        return Plan::Hold(HoldReason::CorporateAction);
    }
    if OUT_OF_SCOPE_SUBTYPES.contains(&sub.as_str()) {
        return Plan::Hold(HoldReason::UnhandledType);
    }

    match ty.as_str() {
        "buy" => Plan::Post(PostAs::Buy),
        "sell" => Plan::Post(PostAs::Sell),
        // Every subtype of a fee is a fee. There is nothing to distinguish an
        // advisory fee from an ADR fee in the books: both are ordinary expenses,
        // and neither appears on a 1099-B.
        "fee" => Plan::Post(PostAs::Fee),
        "cash" => {
            if DIVIDEND_SUBTYPES.contains(&sub.as_str()) {
                Plan::Post(PostAs::Income(InvestmentIncomeKind::Dividend))
            } else if INTEREST_SUBTYPES.contains(&sub.as_str()) {
                Plan::Post(PostAs::Income(InvestmentIncomeKind::Interest))
            } else if CASH_MOVEMENT_SUBTYPES.contains(&sub.as_str()) {
                Plan::Post(PostAs::Cash)
            } else {
                // Capital-gain distributions land here, and deliberately: they are
                // Schedule D income rather than Schedule B, and calling one a
                // dividend would put it on the wrong form.
                Plan::Hold(HoldReason::UnhandledType)
            }
        }
        // Securities moving in or out of the account. Almost always a corporate
        // action or a transfer from another custodian, and both need the basis of
        // the lots stating by hand.
        "transfer" => Plan::Hold(HoldReason::CorporateAction),
        _ => Plan::Hold(HoldReason::UnhandledType),
    }
}

fn is_cash_movement(ty: &str, sub: &str) -> bool {
    ty == "cash" && CASH_MOVEMENT_SUBTYPES.contains(&sub)
}

// ---------------------------------------------------------------------------
// Reading the configuration register
// ---------------------------------------------------------------------------

/// How one provider account is imported.
#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub item_id: String,
    pub plaid_account_id: String,
    pub accounts: InvestmentPostingAccounts,
    pub plaid_subtype: Option<String>,
    pub subtype_recognised: bool,
}

impl AccountConfig {
    pub fn treatment(&self) -> InvestmentTreatment {
        self.accounts.treatment()
    }

    /// The taxable accounts, or `None` for a sheltered account.
    pub fn taxable(&self) -> Option<&TaxableBrokerageAccounts> {
        match &self.accounts {
            InvestmentPostingAccounts::Taxable(a) => Some(a),
            InvestmentPostingAccounts::Sheltered { .. } => None,
        }
    }

    /// The sheltered ledger account, or `None` for a taxable one.
    pub fn sheltered(&self) -> Option<&str> {
        match &self.accounts {
            InvestmentPostingAccounts::Sheltered {
                retirement_account_id,
            } => Some(retirement_account_id),
            InvestmentPostingAccounts::Taxable(_) => None,
        }
    }
}

const CONFIG_COLUMNS: &str = "item_id, plaid_account_id, treatment, plaid_subtype,
     subtype_recognised, securities_account_id, cash_account_id, dividend_income_account_id,
     interest_income_account_id, realized_gain_account_id, fee_expense_account_id,
     transfer_clearing_account_id, retirement_account_id";

fn read_config(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<AccountConfig>> {
    let item_id: String = row.get(0)?;
    let plaid_account_id: String = row.get(1)?;
    let treatment: String = row.get(2)?;
    let plaid_subtype: Option<String> = row.get(3)?;
    let subtype_recognised: i64 = row.get(4)?;
    let accounts = match InvestmentTreatment::parse(&treatment) {
        Some(InvestmentTreatment::Taxable) => {
            // Every one of the six is NOT NULL for a taxable row by the CHECK
            // constraint, so a missing one means the row was written by something
            // that bypassed it. Reading it back as "no configuration" is the safe
            // answer: the account's activity is then held rather than posted
            // against a hole.
            let securities: Option<String> = row.get(5)?;
            let cash: Option<String> = row.get(6)?;
            let dividends: Option<String> = row.get(7)?;
            let interest: Option<String> = row.get(8)?;
            let gain: Option<String> = row.get(9)?;
            let fees: Option<String> = row.get(10)?;
            match (securities, cash, dividends, interest, gain, fees) {
                (
                    Some(securities),
                    Some(cash),
                    Some(dividends),
                    Some(interest),
                    Some(gain),
                    Some(fees),
                ) => InvestmentPostingAccounts::Taxable(Box::new(TaxableBrokerageAccounts {
                    securities_account_id: securities,
                    cash_account_id: cash,
                    dividend_income_account_id: dividends,
                    interest_income_account_id: interest,
                    realized_gain_account_id: gain,
                    fee_expense_account_id: fees,
                    transfer_clearing_account_id: row.get(11)?,
                })),
                _ => return Ok(None),
            }
        }
        Some(InvestmentTreatment::Sheltered) => match row.get::<_, Option<String>>(12)? {
            Some(retirement_account_id) => InvestmentPostingAccounts::Sheltered {
                retirement_account_id,
            },
            None => return Ok(None),
        },
        None => return Ok(None),
    };
    Ok(Some(AccountConfig {
        item_id,
        plaid_account_id,
        accounts,
        plaid_subtype,
        subtype_recognised: subtype_recognised != 0,
    }))
}

/// How one provider account is imported, or `None` if nobody has said.
pub fn get_config(
    conn: &Connection,
    item_id: &str,
    plaid_account_id: &str,
) -> Option<AccountConfig> {
    let sql =
        format!("SELECT {CONFIG_COLUMNS} FROM investment_account_config WHERE item_id = ?1 AND plaid_account_id = ?2");
    conn.query_row(&sql, [item_id, plaid_account_id], read_config)
        .optional()
        .ok()
        .flatten()
        .flatten()
}

/// Every configured account on one connection.
pub fn list_configs(conn: &Connection, item_id: &str) -> Vec<AccountConfig> {
    let sql = format!(
        "SELECT {CONFIG_COLUMNS} FROM investment_account_config
          WHERE item_id = ?1 ORDER BY plaid_account_id"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([item_id], read_config) else {
        return Vec::new();
    };
    rows.flatten().flatten().collect()
}

// ---------------------------------------------------------------------------
// Configuring an account
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ConfigureInvestmentAccountCommand {
    pub item_id: String,
    pub plaid_account_id: String,
    pub accounts: InvestmentPostingAccounts,
    /// What the provider calls the account. Passed in rather than read from
    /// anywhere, because the caller is the one holding the payload, and the
    /// recognised flag is derived from it here so that a caller cannot set the flag
    /// to whatever suits it.
    pub plaid_subtype: Option<String>,
}

/// Say how a provider investment account is imported.
///
/// Validated **inside** the append transaction, like every other command in phases
/// 1 and 2: that the connection exists, that every ledger account named exists and
/// is of a type that can play the part, and — for a sheltered account — that it is
/// already on phase 2's retirement register.
pub fn configure_account(
    store: &mut EventStore,
    user_id: &str,
    cmd: &ConfigureInvestmentAccountCommand,
) -> Result<StoredEvent, ImportError> {
    let verdict = classify_subtype(cmd.plaid_subtype.as_deref());
    let events = run(store, user_id, |tx| {
        build_configure_in_txn(tx, cmd, verdict.recognised)
    })?;
    events
        .into_iter()
        .find(|e| matches!(e.event, Event::InvestmentAccountConfigured(_)))
        .ok_or_else(|| ImportError::Store("the configuration did not land".to_string()))
}

fn build_configure_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ConfigureInvestmentAccountCommand,
    subtype_recognised: bool,
) -> Result<ImportStep, EventStoreError> {
    let item_exists: bool = tx
        .query_row(
            "SELECT 1 FROM plaid_items WHERE id = ?1",
            [&cmd.item_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !item_exists {
        return Ok(ImportStep::Reject(ImportError::NoSuchItem(
            cmd.item_id.clone(),
        )));
    }
    if cmd.plaid_account_id.trim().is_empty() {
        return Ok(ImportStep::Reject(ImportError::Invalid(
            "a configuration is about one provider account, and none was named".to_string(),
        )));
    }

    // The account types are checked, not merely the existence, for the reason phase
    // 2 checks them: a dividend account that is an *asset* silently turns income
    // into a balance-sheet line, the trial balance still balances, and the return
    // comes out short with nothing to point at.
    let checks: Vec<(&str, &'static str, &'static [&'static str])> = match &cmd.accounts {
        InvestmentPostingAccounts::Taxable(a) => {
            if a.securities_account_id == a.cash_account_id {
                return Ok(ImportStep::Reject(ImportError::Invalid(
                    "the securities account and the cash account cannot be the same account: \
                     every purchase would post to itself and change nothing"
                        .to_string(),
                )));
            }
            let mut checks: Vec<(&str, &'static str, &'static [&'static str])> = vec![
                (
                    a.securities_account_id.as_str(),
                    "securities account",
                    &["asset"],
                ),
                (a.cash_account_id.as_str(), "cash account", &["asset"]),
                (
                    a.dividend_income_account_id.as_str(),
                    "dividend income account",
                    &["revenue"],
                ),
                (
                    a.interest_income_account_id.as_str(),
                    "interest income account",
                    &["revenue"],
                ),
                (
                    a.realized_gain_account_id.as_str(),
                    "realized gain account",
                    &["revenue"],
                ),
                (
                    a.fee_expense_account_id.as_str(),
                    "fee expense account",
                    &["expense"],
                ),
            ];
            if let Some(clearing) = a.transfer_clearing_account_id.as_deref() {
                // Asset or liability, because money in transit is genuinely either:
                // a deposit on its way in is a receivable and a withdrawal on its
                // way out is a payable, and which one it is depends on the day.
                checks.push((clearing, "clearing account", &["asset", "liability"]));
            }
            checks
        }
        InvestmentPostingAccounts::Sheltered {
            retirement_account_id,
        } => {
            // On the register, not merely an asset account. Whether a distribution
            // out of it is taxable depends on the kind recorded there, and a
            // sheltered account imported without one would have its value set by
            // this importer while its distributions had nowhere to report.
            let registered: bool = tx
                .query_row(
                    "SELECT 1 FROM retirement_accounts WHERE account_id = ?1",
                    [retirement_account_id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if !registered {
                return Ok(ImportStep::Reject(ImportError::NotOnRetirementRegister(
                    retirement_account_id.clone(),
                )));
            }
            vec![(
                retirement_account_id.as_str(),
                "retirement account",
                &["asset"],
            )]
        }
    };

    for (account_id, role, wanted) in checks {
        match account_type_in_txn(tx, account_id)? {
            None => {
                return Ok(ImportStep::Reject(ImportError::NoSuchAccount(
                    account_id.to_string(),
                )))
            }
            Some(found) if !wanted.contains(&found.as_str()) => {
                return Ok(ImportStep::Reject(ImportError::WrongAccountType {
                    account_id: account_id.to_string(),
                    role,
                    wanted: wanted[0],
                    found,
                }))
            }
            Some(_) => {}
        }
    }

    Ok(ImportStep::Append(vec![
        Event::InvestmentAccountConfigured(Box::new(InvestmentAccountConfigData {
            item_id: cmd.item_id.clone(),
            plaid_account_id: cmd.plaid_account_id.clone(),
            accounts: cmd.accounts.clone(),
            plaid_subtype: cmd
                .plaid_subtype
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            subtype_recognised,
        })),
    ]))
}

fn account_type_in_txn(
    tx: &rusqlite::Transaction<'_>,
    account_id: &str,
) -> Result<Option<String>, EventStoreError> {
    Ok(tx
        .query_row(
            "SELECT account_type FROM accounts WHERE id = ?1",
            [account_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// The append-and-retry loop
// ---------------------------------------------------------------------------

/// The events to append as one unit, or a domain rejection. [`InvestmentStep`]'s
/// shape, for the reason it gives.
pub(crate) enum ImportStep {
    Append(Vec<Event>),
    Reject(ImportError),
}

fn run(
    store: &mut EventStore,
    user_id: &str,
    build: impl Fn(&rusqlite::Transaction<'_>) -> Result<ImportStep, EventStoreError>,
) -> Result<Vec<StoredEvent>, ImportError> {
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let outcome = store.append_checked_many(
            head,
            |tx| match build(tx)? {
                ImportStep::Append(events) => Ok(Verdict::Append(
                    events
                        .into_iter()
                        .map(|e| EventEnvelope::new(e, user_id.to_string()))
                        .collect(),
                )),
                ImportStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;
        match outcome {
            CheckedOutcome::Appended(events) => return Ok(events),
            // The log moved before the write lock; nothing was appended and the
            // checks did not run. Rebuild against fresh state — which matters more
            // here than in most places, because a concurrent sale changes which
            // lots FIFO would pick.
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

/// Turn a phase-1 step into one of ours, carrying the refusal through as a
/// [`ImportError::Refused`] so the caller can hold the row instead of failing the
/// whole run.
fn from_investment_step(step: InvestmentStep, import: ImportRecord) -> ImportStep {
    match step {
        InvestmentStep::Append(mut events) => {
            let Some(entry_id) = events.iter().find_map(|e| match e {
                Event::JournalEntryPosted { entry_id, .. } => Some(entry_id.clone()),
                _ => None,
            }) else {
                return ImportStep::Reject(ImportError::Store(
                    "the command posted no journal entry, so there is nothing to record as \
                     imported"
                        .to_string(),
                ));
            };
            events.push(Event::InvestmentActivityImported(Box::new(
                InvestmentActivityImportedData {
                    provider_transaction_id: import.provider_transaction_id,
                    item_id: import.item_id,
                    plaid_account_id: import.plaid_account_id,
                    outcome: import.outcome,
                    entry_id,
                    lot_id: import.lot_id,
                    sale_id: import.sale_id,
                },
            )));
            ImportStep::Append(events)
        }
        InvestmentStep::Reject(e) => ImportStep::Reject(ImportError::Refused(e.to_string())),
    }
}

/// What the import record will say, minus the entry id, which is read off the
/// entry the command built rather than minted here.
struct ImportRecord {
    provider_transaction_id: String,
    item_id: String,
    plaid_account_id: String,
    outcome: ImportedActivityKind,
    lot_id: Option<String>,
    sale_id: Option<String>,
}

/// Has this provider transaction already been dealt with?
///
/// Both tables, exactly as [`super::plaid_commands::stage_transactions_in_conn`]
/// checks both of its own: one holds what posted and the other what is waiting for
/// a person, and a transaction in either must not arrive a second time.
fn already_seen(conn: &Connection, provider_transaction_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM investment_imports WHERE provider_transaction_id = ?1
         UNION ALL
         SELECT 1 FROM investment_staged_activity WHERE provider_transaction_id = ?1
         LIMIT 1",
        [provider_transaction_id],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

fn already_imported_in_txn(
    tx: &rusqlite::Transaction<'_>,
    provider_transaction_id: &str,
) -> Result<bool, EventStoreError> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM investment_imports WHERE provider_transaction_id = ?1",
            [provider_transaction_id],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

// ---------------------------------------------------------------------------
// The security master, from the provider's securities
// ---------------------------------------------------------------------------

/// The ticker a security goes onto the master under.
///
/// Phase 1's master requires a non-empty, unique ticker, and plenty of real
/// securities have none: a private fund, a money-market sweep, a bond Plaid has no
/// symbol for. Rather than refuse them — which would mean refusing the trades in
/// them, which is how a position goes missing from a balance sheet — a stable
/// placeholder is synthesised, in order of how durable the identifier is:
///
/// 1. the provider's ticker, when there is one;
/// 2. `CUSIP:037833100`, which is the identifier that survives a ticker change and
///    is what a 1099-B is matched on;
/// 3. `ISIN:US0378331005`, the same idea for a non-US security;
/// 4. `PLAID:<security_id>`, which is stable for as long as the connection is.
///
/// It is a *label*, not a market symbol, and the prefix is there so that nobody
/// reading a report mistakes it for one. The holding does not depend on it staying
/// the same: `plaid_securities` is what ties the provider's security to ours, so a
/// real ticker appearing later does not fork the position.
pub fn ticker_for(security: &ProviderSecurity) -> String {
    let clean = |s: &Option<String>| {
        s.as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if let Some(ticker) = clean(&security.ticker) {
        return ticker.to_uppercase();
    }
    if let Some(cusip) = clean(&security.cusip) {
        return format!("CUSIP:{}", cusip.to_uppercase());
    }
    if let Some(isin) = clean(&security.isin) {
        return format!("ISIN:{}", isin.to_uppercase());
    }
    format!("PLAID:{}", security.security_id)
}

/// Find or create the security master for a provider security, and return our id.
///
/// Three ways of finding it before creating one, in order of trust:
///
/// 1. `plaid_securities`, which is the answer once it exists;
/// 2. the CUSIP, which survives a ticker change, so a security whose symbol changed
///    between two imports is recognised rather than duplicated;
/// 3. the ticker, which catches a security somebody already entered by hand.
///
/// Whichever way it is found, the link is appended so that the next import takes
/// the first route. Creating a master is its own append rather than part of the
/// trade's batch:
/// the master is harmless on its own (a security nobody holds is a row in a list),
/// while a trade that could not be posted must not take a security definition down
/// with it, because the next attempt would then have to create it again.
fn resolve_security(
    store: &mut EventStore,
    user_id: &str,
    security: &ProviderSecurity,
    created: &mut u32,
) -> Result<String, ImportError> {
    if let Some(existing) = store
        .connection()
        .query_row(
            "SELECT security_id FROM plaid_securities WHERE plaid_security_id = ?1",
            [&security.security_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(existing);
    }

    let ticker = ticker_for(security);
    let cusip = security
        .cusip
        .as_ref()
        .map(|c| c.trim().to_uppercase())
        .filter(|c| !c.is_empty());

    let matched: Option<String> = match &cusip {
        Some(cusip) => store
            .connection()
            .query_row("SELECT id FROM securities WHERE cusip = ?1", [cusip], |r| {
                r.get::<_, String>(0)
            })
            .optional()?,
        None => None,
    }
    .or(store
        .connection()
        .query_row(
            "SELECT id FROM securities WHERE ticker = ?1",
            [&ticker],
            |r| r.get::<_, String>(0),
        )
        .optional()?);

    if let Some(security_id) = matched {
        link_security(store, user_id, &security.security_id, &security_id)?;
        return Ok(security_id);
    }

    let security_id = Uuid::new_v4().to_string();
    let new = NewSecurity {
        ticker,
        name: security
            .name
            .as_ref()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| ticker_for(security)),
        // Plaid's own vocabulary, which is exactly what phase 1 left `kind` free
        // text for. "unknown" rather than a guess when it says nothing: a label
        // nothing branches on is better wrong-shaped than invented.
        kind: security
            .security_type
            .as_ref()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        cusip,
        currency: security
            .iso_currency_code
            .clone()
            .unwrap_or_else(|| "USD".to_string()),
    };
    let plaid_security_id = security.security_id.clone();
    let events = run(store, user_id, |tx| {
        match build_define_security_in_txn(tx, &security_id, &new)? {
            InvestmentStep::Append(mut events) => {
                // The link goes in the same batch as the definition: a master
                // created without one would be found again by ticker next time,
                // which works, but a master created and *not* linked is
                // indistinguishable from one somebody typed, and the difference
                // matters when a ticker is reassigned.
                events.push(Event::PlaidSecurityLinked(Box::new(
                    PlaidSecurityLinkData {
                        plaid_security_id: plaid_security_id.clone(),
                        security_id: security_id.clone(),
                    },
                )));
                Ok(ImportStep::Append(events))
            }
            InvestmentStep::Reject(e) => {
                Ok(ImportStep::Reject(ImportError::Refused(e.to_string())))
            }
        }
    })?;
    if events
        .iter()
        .any(|e| matches!(e.event, Event::SecurityDefined(_)))
    {
        *created += 1;
    }
    Ok(security_id)
}

fn link_security(
    store: &mut EventStore,
    user_id: &str,
    plaid_security_id: &str,
    security_id: &str,
) -> Result<(), ImportError> {
    let link = PlaidSecurityLinkData {
        plaid_security_id: plaid_security_id.to_string(),
        security_id: security_id.to_string(),
    };
    run(store, user_id, |_tx| {
        Ok(ImportStep::Append(vec![Event::PlaidSecurityLinked(
            Box::new(link.clone()),
        )]))
    })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Importing transactions
// ---------------------------------------------------------------------------

/// A flag the person is asked to confirm, rather than a decision taken for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtypeFlag {
    pub plaid_account_id: String,
    pub subtype: Option<String>,
    /// What the account is being imported as. For an unconfigured account this is
    /// the **assumption** spec §2 makes; for a configured one it is what the
    /// configuration says, and the flag means the provider's subtype does not
    /// support it.
    pub treatment: InvestmentTreatment,
    pub reason: FlagReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagReason {
    /// The provider's subtype is not one spec §2 lists, so taxable was assumed.
    UnrecognisedSubtype,
    /// The subtype is recognised and says the opposite of the configuration. Not
    /// refused — a person may know better than Plaid, a `brokerage` subtype on an
    /// account that is really an IRA does happen — but never silent.
    ConfiguredAgainstSubtype,
}

impl SubtypeFlag {
    /// What to put in front of a person.
    pub fn message(&self) -> String {
        let subtype = self.subtype.as_deref().unwrap_or("nothing at all");
        match self.reason {
            FlagReason::UnrecognisedSubtype => format!(
                "Account {} is reported as {subtype:?}, which is not a kind of account this knows. \
                 It is being treated as {} — confirm that, because treating a sheltered account as \
                 taxable reports income that is not taxable, and the opposite hides income that is.",
                self.plaid_account_id,
                self.treatment.as_str()
            ),
            FlagReason::ConfiguredAgainstSubtype => format!(
                "Account {} is configured as {} while the provider reports it as {subtype:?}, \
                 which is the other one. One of the two is wrong and only you can say which.",
                self.plaid_account_id,
                self.treatment.as_str()
            ),
        }
    }
}

/// What one import run did.
///
/// Counted by outcome rather than summed, for the reason
/// [`super::plaid_commands::StagedOutcome`] gives: "imported 12, skipped 300" sends
/// somebody hunting for three hundred missing transactions that were mostly
/// duplicates they already had.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub bought: u32,
    pub sold: u32,
    pub dividends: u32,
    pub interest: u32,
    pub fees: u32,
    pub cash_movements: u32,
    /// Already imported, or already held. The bulk of any rolling re-fetch.
    pub duplicates: u32,
    /// Written to the review list. Nothing was posted for these.
    pub held: u32,
    /// Trades inside a sheltered account, ignored by design (spec §2b).
    pub ignored_sheltered: u32,
    pub securities_created: u32,
    pub flags: Vec<SubtypeFlag>,
}

impl ImportReport {
    /// Everything that reached the books.
    pub fn posted(&self) -> u32 {
        self.bought + self.sold + self.dividends + self.interest + self.fees + self.cash_movements
    }
}

/// Import investment transactions for one connection.
///
/// `accounts` is the payload's account list, used only for the subtype flags: what
/// an account is imported *as* comes from the configuration register, never from
/// the subtype, because the subtype is the provider's opinion and the configuration
/// is the book's decision.
pub fn import_transactions(
    store: &mut EventStore,
    user_id: &str,
    item_id: &str,
    accounts: &[ProviderAccount],
    transactions: &[ProviderInvestmentTransaction],
) -> Result<ImportReport, ImportError> {
    let mut report = ImportReport::default();

    // The flags first, per account, so that a payload whose every transaction is a
    // duplicate still reports an account nobody has confirmed the kind of.
    for account in accounts {
        let config = get_config(store.connection(), item_id, &account.account_id);
        if let Some(flag) = flag_for(&account.account_id, account.subtype.as_deref(), &config) {
            report.flags.push(flag);
        }
    }

    for txn in transactions {
        if already_seen(store.connection(), &txn.investment_transaction_id) {
            report.duplicates += 1;
            continue;
        }

        let Some(config) = get_config(store.connection(), item_id, &txn.account_id) else {
            hold(
                store.connection(),
                item_id,
                txn,
                HoldReason::Unconfigured,
                None,
            )?;
            report.held += 1;
            continue;
        };

        match plan(config.treatment(), &txn.transaction_type, &txn.subtype) {
            Plan::IgnoreSheltered => {
                // Not recorded anywhere, and that is deliberate. Ignoring is
                // idempotent by construction — the same trade ignored twice is
                // still ignored — so a register row would buy nothing, and a
                // sheltered account's four hundred yearly trades in a replicated
                // log is exactly the noise spec §2b exists to avoid.
                report.ignored_sheltered += 1;
            }
            Plan::Hold(reason) => {
                hold(store.connection(), item_id, txn, reason, None)?;
                report.held += 1;
            }
            Plan::Post(post) => {
                match post_one(store, user_id, item_id, txn, &config, post, &mut report) {
                    Ok(()) => {}
                    // A refusal is a finding, not a failed run: the rest of the
                    // payload still has to land, and this row has to be visible with
                    // the reason attached.
                    Err(ImportError::Refused(message)) => {
                        hold(
                            store.connection(),
                            item_id,
                            txn,
                            HoldReason::Refused,
                            Some(&message),
                        )?;
                        report.held += 1;
                    }
                    Err(ImportError::Held { reason, detail }) => {
                        hold(store.connection(), item_id, txn, reason, detail.as_deref())?;
                        report.held += 1;
                    }
                    // Anything else is a broken database or a broken log, and
                    // carrying on through one would write more of whatever is wrong.
                    Err(other) => return Err(other),
                }
            }
        }
    }

    Ok(report)
}

/// The flag, if any, for one account in the payload.
fn flag_for(
    plaid_account_id: &str,
    subtype: Option<&str>,
    config: &Option<AccountConfig>,
) -> Option<SubtypeFlag> {
    let verdict = classify_subtype(subtype);
    match config {
        None => (!verdict.recognised).then(|| SubtypeFlag {
            plaid_account_id: plaid_account_id.to_string(),
            subtype: subtype.map(str::to_string),
            treatment: verdict.treatment,
            reason: FlagReason::UnrecognisedSubtype,
        }),
        Some(config) => {
            if !verdict.recognised {
                Some(SubtypeFlag {
                    plaid_account_id: plaid_account_id.to_string(),
                    subtype: subtype.map(str::to_string),
                    treatment: config.treatment(),
                    reason: FlagReason::UnrecognisedSubtype,
                })
            } else if verdict.treatment != config.treatment() {
                Some(SubtypeFlag {
                    plaid_account_id: plaid_account_id.to_string(),
                    subtype: subtype.map(str::to_string),
                    treatment: config.treatment(),
                    reason: FlagReason::ConfiguredAgainstSubtype,
                })
            } else {
                None
            }
        }
    }
}

/// Post one transaction, as one append batch: the journal entry, the phase-1
/// register event, and the import record that fences it against the next fetch.
fn post_one(
    store: &mut EventStore,
    user_id: &str,
    item_id: &str,
    txn: &ProviderInvestmentTransaction,
    config: &AccountConfig,
    post: PostAs,
    report: &mut ImportReport,
) -> Result<(), ImportError> {
    let Some(taxable) = config.taxable() else {
        // Unreachable by construction: `plan` never returns `Post` for a sheltered
        // account. Stated rather than unwrapped, because the cost of being wrong is
        // a trade posted into a sheltered account's single value-carried balance.
        return Err(ImportError::Held {
            reason: HoldReason::UnhandledType,
            detail: Some("a sheltered account has no accounts to post a trade to".to_string()),
        });
    };
    let taxable = taxable.clone();

    let Some(date) = NaiveDate::parse_from_str(txn.date.trim(), "%Y-%m-%d").ok() else {
        return Err(ImportError::Held {
            reason: HoldReason::BadAmount,
            detail: Some(format!("{:?} is not a date this can read", txn.date)),
        });
    };

    // Every float in the payload becomes an integer here, before anything else
    // happens with it, and a conversion that fails holds the row rather than
    // posting an approximation.
    let bad = |e: ConversionError| ImportError::Held {
        reason: HoldReason::BadAmount,
        detail: Some(e.to_string()),
    };
    let amount_cents = to_cents(txn.amount).map_err(bad)?;
    let fee_cents = match txn.fees {
        Some(fees) => to_cents(fees).map_err(bad)?,
        None => 0,
    };
    let quantity = to_micro_shares(txn.quantity.abs()).map_err(bad)?;

    let provider_transaction_id = txn.investment_transaction_id.clone();
    let memo = memo_for(txn);

    match post {
        PostAs::Buy => {
            let Some(security) = txn.security.as_ref() else {
                return Err(ImportError::Held {
                    reason: HoldReason::UnknownSecurity,
                    detail: None,
                });
            };
            let security_id =
                resolve_security(store, user_id, security, &mut report.securities_created)?;
            let lot_id = Uuid::new_v4().to_string();
            // `amount` and not `price * quantity + fees`. The provider's amount is
            // the cash that actually left the account, commission included, which
            // is both what the cash account has to be credited and — because a
            // purchase commission capitalises into basis — exactly the lot's cost.
            // Rebuilding it from price and quantity would re-round the same money
            // and leave the lot disagreeing with the bank.
            let cmd = BuySecurityCommand {
                security_id,
                securities_account_id: taxable.securities_account_id.clone(),
                cash_account_id: taxable.cash_account_id.clone(),
                quantity,
                total_cost_cents: amount_cents.abs(),
                trade_date: date,
                memo: Some(memo),
            };
            let record = ImportRecord {
                provider_transaction_id: provider_transaction_id.clone(),
                item_id: item_id.to_string(),
                plaid_account_id: txn.account_id.clone(),
                outcome: ImportedActivityKind::Buy,
                lot_id: Some(lot_id.clone()),
                sale_id: None,
            };
            append_import(store, user_id, &provider_transaction_id, move |tx| {
                Ok(from_investment_step(
                    build_buy_in_txn(tx, &lot_id, &cmd)?,
                    clone_record(&record),
                ))
            })?;
            report.bought += 1;
        }
        PostAs::Sell => {
            let Some(security) = txn.security.as_ref() else {
                return Err(ImportError::Held {
                    reason: HoldReason::UnknownSecurity,
                    detail: None,
                });
            };
            let security_id =
                resolve_security(store, user_id, security, &mut report.securities_created)?;
            let sale_id = Uuid::new_v4().to_string();
            // The provider's amount on a sale is the **net** credited to cash. A
            // 1099-B reports proceeds gross with the fee shown separately, and
            // phase 1's command takes them that way and posts the difference — so
            // the gross is reconstructed as net + fee. Posting the net as the gross
            // would understate proceeds on every reconciliation against the form,
            // which is the one comparison spec §8 says the ledger exists to make.
            let net_cents = amount_cents.abs();
            let cmd = SellSecurityCommand {
                security_id,
                securities_account_id: taxable.securities_account_id.clone(),
                cash_account_id: taxable.cash_account_id.clone(),
                realized_gain_account_id: taxable.realized_gain_account_id.clone(),
                quantity,
                proceeds_cents: net_cents + fee_cents,
                fee_cents,
                trade_date: date,
                // FIFO, which is both spec §4's default and what the IRS assumes
                // when a seller specifies nothing. A specific-lot choice is a
                // decision made at the point of sale by a person; an importer
                // reading a month-old trade cannot make it, and guessing would put
                // a basis on a filed return that nobody chose.
                selection: LotSelection::Fifo,
                memo: Some(memo),
            };
            let record = ImportRecord {
                provider_transaction_id: provider_transaction_id.clone(),
                item_id: item_id.to_string(),
                plaid_account_id: txn.account_id.clone(),
                outcome: ImportedActivityKind::Sell,
                lot_id: None,
                sale_id: Some(sale_id.clone()),
            };
            append_import(store, user_id, &provider_transaction_id, move |tx| {
                Ok(from_investment_step(
                    build_sell_in_txn(tx, &sale_id, &cmd)?,
                    clone_record(&record),
                ))
            })?;
            report.sold += 1;
        }
        PostAs::Income(kind) => {
            let security_id = match txn.security.as_ref() {
                Some(security) => Some(resolve_security(
                    store,
                    user_id,
                    security,
                    &mut report.securities_created,
                )?),
                // Sweep interest belongs to the account and to no holding, which is
                // why phase 1 made the security optional on income.
                None => None,
            };
            let income_account_id = match kind {
                InvestmentIncomeKind::Dividend => taxable.dividend_income_account_id.clone(),
                InvestmentIncomeKind::Interest => taxable.interest_income_account_id.clone(),
            };
            let cmd = RecordInvestmentIncomeCommand {
                kind,
                security_id,
                cash_account_id: taxable.cash_account_id.clone(),
                income_account_id,
                // Income arrives as a credit to cash, so the provider's amount is
                // negative. The magnitude is the income.
                amount_cents: amount_cents.abs(),
                received_on: date,
                memo: Some(memo),
            };
            let outcome = match kind {
                InvestmentIncomeKind::Dividend => ImportedActivityKind::Dividend,
                InvestmentIncomeKind::Interest => ImportedActivityKind::Interest,
            };
            let record = ImportRecord {
                provider_transaction_id: provider_transaction_id.clone(),
                item_id: item_id.to_string(),
                plaid_account_id: txn.account_id.clone(),
                outcome,
                lot_id: None,
                sale_id: None,
            };
            append_import(store, user_id, &provider_transaction_id, move |tx| {
                Ok(from_investment_step(
                    build_income_in_txn(tx, &cmd)?,
                    clone_record(&record),
                ))
            })?;
            match kind {
                InvestmentIncomeKind::Dividend => report.dividends += 1,
                InvestmentIncomeKind::Interest => report.interest += 1,
            }
        }
        PostAs::Fee => {
            let security_id = match txn.security.as_ref() {
                Some(security) => Some(resolve_security(
                    store,
                    user_id,
                    security,
                    &mut report.securities_created,
                )?),
                None => None,
            };
            let cmd = ChargeInvestmentFeeCommand {
                cash_account_id: taxable.cash_account_id.clone(),
                expense_account_id: taxable.fee_expense_account_id.clone(),
                amount_cents: amount_cents.abs(),
                charged_on: date,
                security_id,
                memo: Some(memo),
            };
            let record = ImportRecord {
                provider_transaction_id: provider_transaction_id.clone(),
                item_id: item_id.to_string(),
                plaid_account_id: txn.account_id.clone(),
                outcome: ImportedActivityKind::Fee,
                lot_id: None,
                sale_id: None,
            };
            append_import(store, user_id, &provider_transaction_id, move |tx| {
                Ok(from_investment_step(
                    build_fee_in_txn(tx, &cmd)?,
                    clone_record(&record),
                ))
            })?;
            report.fees += 1;
        }
        PostAs::Cash => {
            let Some(clearing) = taxable.transfer_clearing_account_id.clone() else {
                return Err(ImportError::Held {
                    reason: HoldReason::NoClearingAccount,
                    detail: None,
                });
            };
            // A transfer, so the sign is the provider's own: positive amount means
            // cash left the brokerage, negative means it arrived. One signed
            // construction rather than two branches, because the sign is the whole
            // content of this entry and two branches is two places to get it wrong.
            let into_brokerage = -amount_cents;
            let record = ImportRecord {
                provider_transaction_id: provider_transaction_id.clone(),
                item_id: item_id.to_string(),
                plaid_account_id: txn.account_id.clone(),
                outcome: ImportedActivityKind::Cash,
                lot_id: None,
                sale_id: None,
            };
            let cash_account_id = taxable.cash_account_id.clone();
            let memo_for_entry = memo.clone();
            append_import(store, user_id, &provider_transaction_id, move |tx| {
                if into_brokerage == 0 {
                    return Ok(ImportStep::Reject(ImportError::Refused(
                        "a cash movement of nothing moves no money".to_string(),
                    )));
                }
                let currency = base_currency_in_txn(tx)?;
                let lines = vec![
                    (cash_account_id.clone(), into_brokerage, "Brokerage cash"),
                    (clearing.clone(), -into_brokerage, "Cash in transit"),
                ];
                match investment_commands::entry_or_reject(
                    tx,
                    date,
                    memo_for_entry.clone(),
                    None,
                    &lines,
                    &currency,
                )? {
                    Ok(entry) => Ok(from_investment_step(
                        InvestmentStep::Append(vec![entry]),
                        clone_record(&record),
                    )),
                    Err(e) => Ok(ImportStep::Reject(ImportError::Refused(e.to_string()))),
                }
            })?;
            report.cash_movements += 1;
        }
    }
    Ok(())
}

fn clone_record(record: &ImportRecord) -> ImportRecord {
    ImportRecord {
        provider_transaction_id: record.provider_transaction_id.clone(),
        item_id: record.item_id.clone(),
        plaid_account_id: record.plaid_account_id.clone(),
        outcome: record.outcome,
        lot_id: record.lot_id.clone(),
        sale_id: record.sale_id.clone(),
    }
}

/// Append one import's batch, with the dedup check inside the same transaction.
///
/// The check is repeated here although the caller already made it, and the
/// repetition is the point: the caller's read happened outside the write lock, and
/// two imports of the same payload running at once would both pass it. Under the
/// lock, the second one is refused.
fn append_import(
    store: &mut EventStore,
    user_id: &str,
    provider_transaction_id: &str,
    build: impl Fn(&rusqlite::Transaction<'_>) -> Result<ImportStep, EventStoreError>,
) -> Result<(), ImportError> {
    let id = provider_transaction_id.to_string();
    run(store, user_id, move |tx| {
        if already_imported_in_txn(tx, &id)? {
            return Ok(ImportStep::Reject(ImportError::Refused(format!(
                "{id} has already been imported"
            ))));
        }
        build(tx)
    })?;
    Ok(())
}

fn base_currency_in_txn(tx: &rusqlite::Transaction<'_>) -> Result<String, EventStoreError> {
    Ok(tx
        .query_row("SELECT base_currency FROM company LIMIT 1", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .unwrap_or_else(|| "USD".to_string()))
}

/// The memo an imported entry carries: what the broker called it, with its own
/// vocabulary beside it, so a line in the books can be found on a statement.
fn memo_for(txn: &ProviderInvestmentTransaction) -> String {
    let name = txn.name.trim();
    if name.is_empty() {
        format!("{} {}", txn.transaction_type, txn.subtype)
    } else {
        format!("{name} ({} {})", txn.transaction_type, txn.subtype)
    }
}

// ---------------------------------------------------------------------------
// Holding for review
// ---------------------------------------------------------------------------

/// One row waiting for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedActivity {
    pub id: String,
    pub item_id: String,
    pub plaid_account_id: String,
    pub provider_transaction_id: String,
    pub reason: String,
    pub detail: String,
    pub provider_type: String,
    pub provider_subtype: String,
    pub date: String,
    pub name: String,
    pub amount_cents: Option<i64>,
    pub raw_payload: String,
    pub status: String,
}

/// Write a transaction to the review list, with its raw payload.
///
/// Local, like the bank feed's staging table and for the same reason: what one
/// member has looked at and not yet decided is not a fact about the books, and
/// appending an event for it would be a local write a replica would have to refuse.
///
/// `INSERT OR IGNORE` on the provider id, so the rolling re-fetch does not grow the
/// list by a copy of itself every run.
fn hold(
    conn: &Connection,
    item_id: &str,
    txn: &ProviderInvestmentTransaction,
    reason: HoldReason,
    detail: Option<&str>,
) -> Result<(), ImportError> {
    let amount_cents = to_cents(txn.amount).ok();
    let raw_payload = serde_json::to_string(txn).unwrap_or_else(|_| "{}".to_string());
    let detail = match detail {
        Some(message) => format!("{message} — {}", reason.guidance()),
        None => format!(
            "Plaid reported this as {} / {}. {}",
            txn.transaction_type,
            txn.subtype,
            reason.guidance()
        ),
    };
    conn.execute(
        "INSERT OR IGNORE INTO investment_staged_activity
            (id, item_id, plaid_account_id, provider_transaction_id, reason, detail,
             provider_type, provider_subtype, date, name, amount_cents, raw_payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            Uuid::new_v4().to_string(),
            item_id,
            txn.account_id,
            txn.investment_transaction_id,
            reason.as_str(),
            detail,
            txn.transaction_type,
            txn.subtype,
            txn.date,
            txn.name,
            amount_cents,
            raw_payload,
        ],
    )?;
    Ok(())
}

/// Everything still waiting for a person, oldest activity first.
pub fn pending_activity(conn: &Connection) -> Vec<StagedActivity> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, item_id, plaid_account_id, provider_transaction_id, reason, detail,
                provider_type, provider_subtype, date, name, amount_cents, raw_payload, status
           FROM investment_staged_activity
          WHERE status = 'pending'
          ORDER BY date, rowid",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(StagedActivity {
            id: r.get(0)?,
            item_id: r.get(1)?,
            plaid_account_id: r.get(2)?,
            provider_transaction_id: r.get(3)?,
            reason: r.get(4)?,
            detail: r.get(5)?,
            provider_type: r.get(6)?,
            provider_subtype: r.get(7)?,
            date: r.get(8)?,
            name: r.get(9)?,
            amount_cents: r.get(10)?,
            raw_payload: r.get(11)?,
            status: r.get(12)?,
        })
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Holdings: the snapshot, a sheltered account's value, and the reconciliation
// ---------------------------------------------------------------------------

/// What one holdings import did.
#[derive(Debug, Clone, Default)]
pub struct HoldingsReport {
    /// Snapshots recorded.
    pub recorded: u32,
    /// Accounts whose holdings had not changed since the snapshot already on file
    /// for that date, so nothing was appended.
    pub unchanged: u32,
    /// Accounts in the payload that nobody has configured. Skipped rather than
    /// held, and the difference from a transaction is that a holdings read is not
    /// destructive: the same snapshot can simply be taken again once the account is
    /// configured, whereas a transaction the provider has handed over once is not
    /// offered again.
    pub skipped_unconfigured: Vec<String>,
    /// Sheltered accounts whose value was set, and by how much.
    pub values_set: Vec<(String, ValueSet)>,
    /// Sheltered accounts where at least one holding came with no market value, so
    /// no total could be trusted and no value was set. A total missing one holding
    /// is not a total, and setting it would post a fictional loss.
    pub incomplete_values: Vec<String>,
    /// One per taxable account (spec §7).
    pub reconciliations: Vec<Reconciliation>,
    pub securities_created: u32,
}

/// Record what the broker said, and — for a sheltered account — set its value from
/// it.
///
/// Posts nothing for a taxable account (spec §3 and §5): the snapshot is the input
/// to the reconciliation and to the market-value report, and market value is never
/// posted.
pub fn import_holdings(
    store: &mut EventStore,
    user_id: &str,
    item_id: &str,
    as_of: NaiveDate,
    holdings: &[ProviderHolding],
    accounts: &[ProviderAccount],
) -> Result<HoldingsReport, ImportError> {
    let mut report = HoldingsReport::default();

    // Group by account, keeping the payload's order inside each group so a snapshot
    // reads the way the broker sent it.
    let mut by_account: BTreeMap<&str, Vec<&ProviderHolding>> = BTreeMap::new();
    // Every account in the payload's account list gets an entry, including one with
    // no holdings: an emptied account is a fact worth recording, and an account that
    // reported nothing would otherwise keep its last snapshot for ever.
    for account in accounts {
        by_account.entry(account.account_id.as_str()).or_default();
    }
    for holding in holdings {
        by_account
            .entry(holding.account_id.as_str())
            .or_default()
            .push(holding);
    }

    for (plaid_account_id, account_holdings) in by_account {
        let Some(config) = get_config(store.connection(), item_id, plaid_account_id) else {
            report
                .skipped_unconfigured
                .push(plaid_account_id.to_string());
            continue;
        };

        let mut lines = Vec::with_capacity(account_holdings.len());
        for holding in &account_holdings {
            // A sheltered account's holdings never reach the security master:
            // nothing inside one is recorded (spec §2b), so a master for every fund
            // a 401(k) has ever held would be a list nothing reads and nothing keeps
            // honest.
            let security_id = match (config.treatment(), holding.security.as_ref()) {
                (InvestmentTreatment::Taxable, Some(security)) => Some(resolve_security(
                    store,
                    user_id,
                    security,
                    &mut report.securities_created,
                )?),
                _ => None,
            };
            lines.push(SnapshotHoldingData {
                plaid_security_id: holding.security_id.clone(),
                security_id,
                ticker: holding
                    .security
                    .as_ref()
                    .and_then(|s| s.ticker.clone())
                    .map(|t| t.trim().to_uppercase())
                    .filter(|t| !t.is_empty()),
                quantity: to_micro_shares(holding.quantity)?,
                // `cost_basis` is the broker's basis for the **whole** holding, not
                // per share, which is what makes comparing it against the sum of our
                // lots' remaining basis the right comparison.
                cost_basis_cents: holding.cost_basis.map(to_cents).transpose()?,
                value_cents: holding.institution_value.map(to_cents).transpose()?,
                currency: holding.iso_currency_code.clone(),
            });
        }
        let holdings_data = lines;

        let snapshot = HoldingsSnapshotData {
            snapshot_id: Uuid::new_v4().to_string(),
            item_id: item_id.to_string(),
            plaid_account_id: plaid_account_id.to_string(),
            as_of,
            holdings: holdings_data,
        };

        if snapshot_unchanged(store.connection(), &snapshot) {
            report.unchanged += 1;
        } else {
            let to_append = snapshot.clone();
            run(store, user_id, move |_tx| {
                Ok(ImportStep::Append(vec![Event::HoldingsSnapshotRecorded(
                    Box::new(to_append.clone()),
                )]))
            })?;
            report.recorded += 1;
        }

        match config.sheltered() {
            Some(account_id) => match snapshot.total_value_cents() {
                // The register already holds this statement, with this value, for
                // this date. Phase 2's `set_value` would post no entry — it measures
                // the difference against the books and would find zero — but it does
                // append a `RetirementValueSet` recording that a statement was seen,
                // and re-reading the same holdings on the same day is not a second
                // statement. Skipping it is what makes a whole re-import append
                // literally nothing.
                Some(value_cents)
                    if statement_already_recorded(
                        store.connection(),
                        account_id,
                        as_of,
                        value_cents,
                    ) => {}
                Some(value_cents) => {
                    // A separate append from the snapshot, and safe as a separate
                    // one because `set_value` posts the **difference** between the
                    // statement and the books: if this fails, the next run finds the
                    // snapshot already recorded, appends nothing for it, and sets the
                    // value then. Nothing can be posted twice by retrying.
                    let value = retirement_commands::set_value(
                        store,
                        user_id,
                        &SetRetirementValueCommand {
                            account_id: account_id.to_string(),
                            as_of,
                            value_cents,
                            memo: None,
                        },
                    )
                    .map_err(|e| ImportError::Refused(e.to_string()))?;
                    report
                        .values_set
                        .push((plaid_account_id.to_string(), value));
                }
                None => report.incomplete_values.push(plaid_account_id.to_string()),
            },
            None => {
                if let Some(reconciliation) =
                    reconcile(store.connection(), item_id, plaid_account_id)
                {
                    report.reconciliations.push(reconciliation);
                }
            }
        }
    }

    Ok(report)
}

/// Does phase 2's register already hold this statement, at this value, for this
/// date?
fn statement_already_recorded(
    conn: &Connection,
    account_id: &str,
    as_of: NaiveDate,
    value_cents: i64,
) -> bool {
    retirement_commands::get_account(conn, account_id).is_some_and(|account| {
        account.last_value_as_of == Some(as_of) && account.last_value_cents == Some(value_cents)
    })
}

/// Does the log already hold this exact snapshot for this date?
///
/// Compared line by line rather than by a count or a total, because the case that
/// matters is two holdings swapping quantities: the totals agree and the account
/// holds something different. An unchanged snapshot appends nothing, which is what
/// makes re-importing the same payload a no-op rather than a second event saying the
/// same thing.
fn snapshot_unchanged(conn: &Connection, snapshot: &HoldingsSnapshotData) -> bool {
    let Ok(Some(existing_id)) = conn
        .query_row(
            "SELECT snapshot_id FROM investment_holdings_snapshots
              WHERE item_id = ?1 AND plaid_account_id = ?2 AND as_of = ?3",
            rusqlite::params![
                snapshot.item_id,
                snapshot.plaid_account_id,
                snapshot.as_of.to_string()
            ],
            |r| r.get::<_, String>(0),
        )
        .optional()
    else {
        return false;
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT plaid_security_id, quantity, cost_basis_cents, value_cents
           FROM investment_holdings_snapshot_lines
          WHERE snapshot_id = ?1
          ORDER BY plaid_security_id",
    ) else {
        return false;
    };
    let Ok(rows) = stmt.query_map([&existing_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    }) else {
        return false;
    };
    let stored: Vec<_> = rows.flatten().collect();
    let mut incoming: Vec<_> = snapshot
        .holdings
        .iter()
        .map(|h| {
            (
                h.plaid_security_id.clone(),
                h.quantity,
                h.cost_basis_cents,
                h.value_cents,
            )
        })
        .collect();
    incoming.sort_by(|a, b| a.0.cmp(&b.0));
    stored == incoming
}

/// One security's position, in the books and at the broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldingLine {
    pub plaid_security_id: Option<String>,
    pub security_id: Option<String>,
    pub ticker: Option<String>,
    /// Micro-shares the lot register says are held.
    pub book_quantity: i64,
    /// What those shares are carried at, from the lots.
    pub book_cost_cents: i64,
    /// Micro-shares the broker reports. Zero when the position is absent from the
    /// snapshot — which is a difference, not a missing value.
    pub broker_quantity: i64,
    /// The broker's basis, when it gave one. `None` is **not comparable** rather
    /// than zero: a lot transferred in from another custodian often has no basis at
    /// the new one, and calling that zero would report a difference equal to the
    /// whole holding.
    pub broker_cost_cents: Option<i64>,
}

impl HoldingLine {
    /// Books minus broker, in micro-shares.
    pub fn quantity_difference(&self) -> i64 {
        self.book_quantity - self.broker_quantity
    }

    /// Books minus broker, in cents, or `None` when the broker did not say.
    pub fn cost_difference(&self) -> Option<i64> {
        self.broker_cost_cents.map(|b| self.book_cost_cents - b)
    }

    /// Whether this line needs looking at. A cost the broker did not state is not a
    /// disagreement — there is nothing to disagree with.
    pub fn agrees(&self) -> bool {
        self.quantity_difference() == 0 && self.cost_difference().unwrap_or(0) == 0
    }
}

/// The comparison spec §7 calls the real safeguard: per security, what the books
/// hold against what the broker says.
///
/// It **reports**. It adjusts nothing, and that is the whole design: a mismatch
/// almost always means a corporate action, and applying one automatically would
/// silently restate every gain on that security.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    pub item_id: String,
    pub plaid_account_id: String,
    pub securities_account_id: String,
    pub as_of: NaiveDate,
    pub lines: Vec<HoldingLine>,
}

impl Reconciliation {
    pub fn agrees(&self) -> bool {
        self.lines.iter().all(HoldingLine::agrees)
    }

    /// Only the lines that need looking at.
    pub fn disagreements(&self) -> Vec<&HoldingLine> {
        self.lines.iter().filter(|l| !l.agrees()).collect()
    }
}

/// One stored snapshot line, as the reconciliation reads it back.
struct SnapshotLine {
    security_id: Option<String>,
    plaid_security_id: String,
    ticker: Option<String>,
    quantity: i64,
    cost_basis_cents: Option<i64>,
}

fn snapshot_lines(conn: &Connection, snapshot_id: &str) -> Vec<SnapshotLine> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT security_id, plaid_security_id, ticker, quantity, cost_basis_cents
           FROM investment_holdings_snapshot_lines
          WHERE snapshot_id = ?1
          ORDER BY ticker, plaid_security_id",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([snapshot_id], |r| {
        Ok(SnapshotLine {
            security_id: r.get(0)?,
            plaid_security_id: r.get(1)?,
            ticker: r.get(2)?,
            quantity: r.get(3)?,
            cost_basis_cents: r.get(4)?,
        })
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// Compare the books against the latest snapshot for one taxable account.
///
/// `None` when the account is not configured, is sheltered — there is nothing to
/// reconcile inside one, by design — or has no snapshot yet.
pub fn reconcile(
    conn: &Connection,
    item_id: &str,
    plaid_account_id: &str,
) -> Option<Reconciliation> {
    let config = get_config(conn, item_id, plaid_account_id)?;
    let taxable = config.taxable()?;
    let securities_account_id = taxable.securities_account_id.clone();

    let (snapshot_id, as_of) = conn
        .query_row(
            "SELECT snapshot_id, as_of FROM investment_holdings_snapshots
              WHERE item_id = ?1 AND plaid_account_id = ?2
              ORDER BY as_of DESC, rowid DESC LIMIT 1",
            [item_id, plaid_account_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()
        .ok()
        .flatten()?;
    let as_of = NaiveDate::parse_from_str(&as_of, "%Y-%m-%d").ok()?;

    // The snapshot's side, keyed by our security id where we have one. A holding
    // with no master of ours still gets a line — it is the loudest kind of
    // difference, a position the broker holds that the books have never heard of.
    let broker = snapshot_lines(conn, &snapshot_id);

    let mut lines = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for line in broker {
        let (book_quantity, book_cost_cents) = match &line.security_id {
            Some(id) => {
                seen.push(id.clone());
                investment_commands::holding_of(conn, id, &securities_account_id)
            }
            None => (0, 0),
        };
        lines.push(HoldingLine {
            plaid_security_id: Some(line.plaid_security_id),
            security_id: line.security_id,
            ticker: line.ticker,
            book_quantity,
            book_cost_cents,
            broker_quantity: line.quantity,
            broker_cost_cents: line.cost_basis_cents,
        });
    }

    // And the other direction: something the books hold that the snapshot does not
    // mention. Left out, this would be the silent half of the comparison — a sale
    // the broker recorded and we never imported would reconcile clean.
    for (security_id, ticker) in book_positions(conn, &securities_account_id) {
        if seen.contains(&security_id) {
            continue;
        }
        let (book_quantity, book_cost_cents) =
            investment_commands::holding_of(conn, &security_id, &securities_account_id);
        if book_quantity == 0 && book_cost_cents == 0 {
            continue;
        }
        lines.push(HoldingLine {
            plaid_security_id: None,
            security_id: Some(security_id),
            ticker: Some(ticker),
            book_quantity,
            book_cost_cents,
            broker_quantity: 0,
            broker_cost_cents: None,
        });
    }

    Some(Reconciliation {
        item_id: item_id.to_string(),
        plaid_account_id: plaid_account_id.to_string(),
        securities_account_id,
        as_of,
        lines,
    })
}

/// Which securities the lot register says are held in one account, with tickers.
fn book_positions(conn: &Connection, securities_account_id: &str) -> Vec<(String, String)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT l.security_id, COALESCE(s.ticker, l.security_id)
           FROM investment_lots l
           LEFT JOIN securities s ON s.id = l.security_id
          WHERE l.securities_account_id = ?1
          ORDER BY 2",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([securities_account_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    });
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// The fetch window
// ---------------------------------------------------------------------------

/// The date range to ask the provider for.
///
/// Full history the first time, then a rolling window on top of the last date
/// fetched (spec §6). The overlap is deliberate: a provider revises transactions
/// after reporting them, and a window that started exactly where the last one ended
/// would miss every revision and every trade that settled late.
pub fn fetch_window(
    last_fetched_through: Option<NaiveDate>,
    today: NaiveDate,
) -> (NaiveDate, NaiveDate) {
    let start = match last_fetched_through {
        // `checked_sub` rather than arithmetic that could wrap at the edges of the
        // calendar: a window that silently became the other direction would be
        // refused by the endpoint, which is at least visible, but a window of
        // nothing would import nothing and say it succeeded.
        Some(last) => last
            .checked_sub_days(Days::new(REFETCH_DAYS))
            .unwrap_or(last)
            // A last-fetched date in the future is a clock that has been put back.
            // Asking for a window that starts after it ends is refused by the
            // endpoint; falling back to the ordinary rolling window from today is
            // not, and the dedup fence makes the re-read free.
            .min(today),
        None => today
            .checked_sub_months(Months::new(12 * FIRST_FETCH_YEARS))
            .unwrap_or(today),
    };
    (start.min(today), today)
}

/// How far this machine has fetched one account, or `None` if it never has.
pub fn last_fetched_through(
    conn: &Connection,
    item_id: &str,
    plaid_account_id: &str,
) -> Option<NaiveDate> {
    conn.query_row(
        "SELECT last_fetched_through FROM investment_fetch_state
          WHERE item_id = ?1 AND plaid_account_id = ?2",
        [item_id, plaid_account_id],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
}

/// The window one account needs.
pub fn window_for(
    conn: &Connection,
    item_id: &str,
    plaid_account_id: &str,
    today: NaiveDate,
) -> (NaiveDate, NaiveDate) {
    fetch_window(last_fetched_through(conn, item_id, plaid_account_id), today)
}

/// The window that covers every one of an item's accounts in one call.
///
/// One request serves the whole connection, so the window has to be the widest any
/// account needs: an account added to a connection that has been synced for years
/// still needs its full history, and narrowing to the others' rolling window would
/// leave it permanently missing everything before today.
pub fn window_for_item(
    conn: &Connection,
    item_id: &str,
    plaid_account_ids: &[String],
    today: NaiveDate,
) -> (NaiveDate, NaiveDate) {
    plaid_account_ids
        .iter()
        .map(|id| window_for(conn, item_id, id, today).0)
        .min()
        .map(|start| (start, today))
        .unwrap_or_else(|| fetch_window(None, today))
}

/// Record that a window has been fetched.
pub fn record_fetch(
    conn: &Connection,
    item_id: &str,
    plaid_account_id: &str,
    through: NaiveDate,
) -> Result<(), ImportError> {
    conn.execute(
        "INSERT INTO investment_fetch_state
            (item_id, plaid_account_id, last_fetched_through, last_fetched_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(item_id, plaid_account_id) DO UPDATE SET
            -- Never backwards. Two machines, or one machine with a clock that has
            -- been put back, must not shorten a window that has already been
            -- covered; MAX keeps the furthest point anybody reached.
            last_fetched_through = MAX(last_fetched_through, excluded.last_fetched_through),
            last_fetched_at = excluded.last_fetched_at",
        rusqlite::params![item_id, plaid_account_id, through.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::investment_commands::{holding_of, list_securities, MICRO_SHARE};
    use crate::commands::retirement_commands::{
        register_account, RegisterRetirementAccountCommand,
    };
    use crate::domain::AccountType;
    use crate::events::types::RetirementKind;
    use crate::store::migrations::init_schema;

    // The chart of spec §2a, plus the sheltered account of §2b and the two accounts
    // outside the brokerage a transfer touches.
    const CHECKING: &str = "1000";
    const CLEARING: &str = "1090";
    const BROKER_CASH: &str = "1100";
    const SECURITIES: &str = "1110";
    const IRA: &str = "1500";
    const INVESTMENT_INCOME: &str = "4000";
    const DIVIDENDS: &str = "4100";
    const INTEREST: &str = "4110";
    const REALIZED_GAIN: &str = "4120";
    const VALUE_CHANGE: &str = "4130";
    const FEES: &str = "6000";

    const ITEM: &str = "item1";
    /// The taxable brokerage.
    const BRK: &str = "plaid-brokerage";
    /// The sheltered account.
    const IRA_PLAID: &str = "plaid-ira";
    /// Mapped at the bank, configured by nobody.
    const UNCONFIGURED: &str = "plaid-unconfigured";

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn store() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        crate::commands::partnership_commands::append_event_locally(
            &mut store,
            "u",
            Event::CompanyCreated {
                company_id: "c".into(),
                name: "Books".into(),
                base_currency: "USD".into(),
                fiscal_year_start: 1,
            },
        )
        .expect("company");
        for (id, name, kind, parent) in [
            (CHECKING, "Checking", AccountType::Asset, None),
            (CLEARING, "Cash in transit", AccountType::Asset, None),
            (BROKER_CASH, "Brokerage cash", AccountType::Asset, None),
            (SECURITIES, "Securities at cost", AccountType::Asset, None),
            (IRA, "Fidelity IRA ••5678", AccountType::Asset, None),
            (
                INVESTMENT_INCOME,
                "Investment income",
                AccountType::Revenue,
                None,
            ),
            (
                DIVIDENDS,
                "Dividends",
                AccountType::Revenue,
                Some(INVESTMENT_INCOME),
            ),
            (
                INTEREST,
                "Interest",
                AccountType::Revenue,
                Some(INVESTMENT_INCOME),
            ),
            (
                REALIZED_GAIN,
                "Realized gain",
                AccountType::Revenue,
                Some(INVESTMENT_INCOME),
            ),
            (
                VALUE_CHANGE,
                "Retirement value change",
                AccountType::Revenue,
                Some(INVESTMENT_INCOME),
            ),
            (FEES, "Investment fees", AccountType::Expense, None),
        ] {
            crate::commands::partnership_commands::append_event_locally(
                &mut store,
                "u",
                Event::AccountCreated {
                    account_id: id.into(),
                    account_type: kind.into(),
                    account_number: id.into(),
                    name: name.into(),
                    parent_id: parent.map(|p: &str| p.to_string()),
                    currency: Some("USD".into()),
                    description: None,
                },
            )
            .expect("account");
        }
        store
            .connection()
            .execute(
                "INSERT INTO plaid_items (id, proxy_item_id, institution_name)
                 VALUES ('item1','p1','Fidelity')",
                [],
            )
            .unwrap();
        store
    }

    fn taxable_accounts(clearing: Option<&str>) -> InvestmentPostingAccounts {
        InvestmentPostingAccounts::Taxable(Box::new(TaxableBrokerageAccounts {
            securities_account_id: SECURITIES.into(),
            cash_account_id: BROKER_CASH.into(),
            dividend_income_account_id: DIVIDENDS.into(),
            interest_income_account_id: INTEREST.into(),
            realized_gain_account_id: REALIZED_GAIN.into(),
            fee_expense_account_id: FEES.into(),
            transfer_clearing_account_id: clearing.map(str::to_string),
        }))
    }

    /// A store with the taxable brokerage configured, which is the starting point
    /// for most of these.
    fn configured(clearing: Option<&str>) -> EventStore {
        let mut store = store();
        configure_account(
            &mut store,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: ITEM.into(),
                plaid_account_id: BRK.into(),
                accounts: taxable_accounts(clearing),
                plaid_subtype: Some("brokerage".into()),
            },
        )
        .expect("configured");
        store
    }

    fn with_sheltered(store: &mut EventStore) {
        register_account(
            store,
            "u",
            &RegisterRetirementAccountCommand {
                account_id: IRA.into(),
                institution: "Fidelity ••5678".into(),
                kind: RetirementKind::Traditional,
                value_change_account_id: VALUE_CHANGE.into(),
            },
        )
        .expect("registered");
        configure_account(
            store,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: ITEM.into(),
                plaid_account_id: IRA_PLAID.into(),
                accounts: InvestmentPostingAccounts::Sheltered {
                    retirement_account_id: IRA.into(),
                },
                plaid_subtype: Some("ira".into()),
            },
        )
        .expect("configured");
    }

    fn apple() -> ProviderSecurity {
        ProviderSecurity {
            security_id: "sec-aapl".into(),
            ticker: Some("AAPL".into()),
            name: Some("Apple Inc".into()),
            security_type: Some("equity".into()),
            cusip: Some("037833100".into()),
            isin: None,
            iso_currency_code: Some("USD".into()),
        }
    }

    fn txn(
        id: &str,
        account: &str,
        ty: &str,
        subtype: &str,
        date: NaiveDate,
        amount: f64,
    ) -> ProviderInvestmentTransaction {
        ProviderInvestmentTransaction {
            investment_transaction_id: id.into(),
            account_id: account.into(),
            security_id: None,
            security: None,
            date: date.to_string(),
            name: format!("{ty} {subtype}"),
            transaction_type: ty.into(),
            subtype: subtype.into(),
            quantity: 0.0,
            price: 0.0,
            fees: None,
            amount,
            iso_currency_code: Some("USD".into()),
        }
    }

    /// 10 shares of AAPL at $150.00 with $4.95 of commission: $1,504.95 debited.
    fn buy_apple() -> ProviderInvestmentTransaction {
        ProviderInvestmentTransaction {
            security_id: Some("sec-aapl".into()),
            security: Some(apple()),
            quantity: 10.0,
            price: 150.0,
            fees: Some(4.95),
            ..txn("tx-buy", BRK, "buy", "buy", day(2026, 3, 2), 1504.95)
        }
    }

    /// 4 shares at $200.00 less $1.25 of fee: $798.75 credited.
    fn sell_apple() -> ProviderInvestmentTransaction {
        ProviderInvestmentTransaction {
            security_id: Some("sec-aapl".into()),
            security: Some(apple()),
            quantity: 4.0,
            price: 200.0,
            fees: Some(1.25),
            ..txn("tx-sell", BRK, "sell", "sell", day(2026, 6, 10), -798.75)
        }
    }

    fn brokerage_account(subtype: &str) -> ProviderAccount {
        ProviderAccount {
            account_id: BRK.into(),
            name: "Fidelity Brokerage".into(),
            subtype: Some(subtype.into()),
            mask: Some("1234".into()),
        }
    }

    fn import(
        store: &mut EventStore,
        accounts: &[ProviderAccount],
        transactions: &[ProviderInvestmentTransaction],
    ) -> ImportReport {
        import_transactions(store, "u", ITEM, accounts, transactions).expect("imported")
    }

    fn count(store: &EventStore, sql: &str) -> i64 {
        store
            .connection()
            .query_row(sql, [], |r| r.get(0))
            .unwrap_or(-1)
    }

    /// The entry an imported transaction posted, as `(account, signed cents)`,
    /// account order.
    fn entry_lines(store: &EventStore, provider_transaction_id: &str) -> Vec<(String, i64)> {
        let entry_id: String = store
            .connection()
            .query_row(
                "SELECT entry_id FROM investment_imports WHERE provider_transaction_id = ?1",
                [provider_transaction_id],
                |r| r.get(0),
            )
            .expect("an import record with an entry");
        let mut stmt = store
            .connection()
            .prepare(
                "SELECT account_id, amount FROM journal_lines
                  WHERE entry_id = ?1 ORDER BY account_id",
            )
            .unwrap();
        let rows = stmt
            .query_map([&entry_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .unwrap();
        rows.flatten().collect()
    }

    /// Every posted line in the book, account order. For the cases where there is
    /// exactly one entry and naming it would be more indirection than it is worth.
    fn all_lines(store: &EventStore) -> Vec<(String, i64)> {
        let mut stmt = store
            .connection()
            .prepare("SELECT account_id, amount FROM journal_lines ORDER BY account_id")
            .unwrap();
        let lines = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap()
            .flatten()
            .collect();
        lines
    }

    fn security_id_of(store: &EventStore, plaid_security_id: &str) -> String {
        store
            .connection()
            .query_row(
                "SELECT security_id FROM plaid_securities WHERE plaid_security_id = ?1",
                [plaid_security_id],
                |r| r.get(0),
            )
            .expect("a linked security")
    }

    // -----------------------------------------------------------------------
    // The boundary: floats become integers once, here
    // -----------------------------------------------------------------------

    /// The whole reason phase 1 stores integers: a float cannot represent 0.1, and a
    /// basis carried in one cannot reconcile against a statement. The conversion
    /// happens once, with explicit rounding, and these are the values that prove it
    /// happens at the right moment.
    #[test]
    fn provider_floats_convert_to_exact_integers_at_the_boundary() {
        // The canonical binary-floating-point surprise: 0.1 + 0.2 is
        // 0.30000000000000004, and thirty cents is the only defensible answer.
        assert_eq!(to_cents(0.1 + 0.2), Ok(30));
        assert_eq!(to_cents(1504.95), Ok(150_495));
        // A sale's amount arrives negative; the sign survives, because the sign is
        // what says which way the cash went.
        assert_eq!(to_cents(-798.75), Ok(-79_875));
        // Half away from zero, in both directions, so a long series of amounts does
        // not drift one way.
        assert_eq!(to_cents(0.005), Ok(1));
        assert_eq!(to_cents(-0.005), Ok(-1));

        // Six places is past every brokerage's precision; more than that is rounded
        // here, once, rather than truncated in several places later.
        assert_eq!(to_micro_shares(1.234_567_89), Ok(1_234_568));
        assert_eq!(to_micro_shares(10.0), Ok(10 * MICRO_SHARE));
        // 0.1 shares, which is an ordinary dividend reinvestment.
        assert_eq!(to_micro_shares(0.1), Ok(100_000));

        // And what is refused rather than approximated.
        assert_eq!(
            to_cents(f64::NAN),
            Err(ConversionError::NotFinite { unit: "cents" })
        );
        assert_eq!(
            to_cents(f64::INFINITY),
            Err(ConversionError::NotFinite { unit: "cents" })
        );
        assert!(matches!(
            to_cents(1e300),
            Err(ConversionError::TooLarge { .. })
        ));
        assert!(matches!(
            to_micro_shares(1e30),
            Err(ConversionError::TooLarge { .. })
        ));
    }

    /// A quantity the provider sent with more precision than six places reaches the
    /// lot register rounded, and the lot's cost is the cash that moved — not a
    /// re-multiplication of price by that rounded quantity, which would disagree
    /// with the bank.
    #[test]
    fn a_quantity_with_more_precision_than_six_places_rounds_once() {
        let mut store = configured(None);
        let precise = ProviderInvestmentTransaction {
            quantity: 3.756_493_55,
            price: 100.0,
            fees: None,
            ..ProviderInvestmentTransaction {
                security_id: Some("sec-aapl".into()),
                security: Some(apple()),
                ..txn("tx-odd", BRK, "buy", "buy", day(2026, 3, 2), 375.65)
            }
        };
        let report = import(&mut store, &[brokerage_account("brokerage")], &[precise]);
        assert_eq!(report.bought, 1);
        let security = security_id_of(&store, "sec-aapl");
        // 3.75649355 shares → 3_756_494 micro-shares (half away from zero, so the
        // eight-place figure rounds up at the sixth), and $375.65 → 37_565 cents.
        assert_eq!(
            holding_of(store.connection(), &security, SECURITIES),
            (3_756_494, 37_565)
        );
    }

    // -----------------------------------------------------------------------
    // Taxable: one command and one entry per kind of activity
    // -----------------------------------------------------------------------

    /// A purchase becomes a lot and an entry that nets to zero across assets: the
    /// money changed form, not amount. The commission is *in* the cost, because it
    /// capitalises into basis.
    #[test]
    fn a_buy_becomes_a_lot_and_a_balanced_entry() {
        let mut store = configured(None);
        let report = import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[buy_apple()],
        );
        assert_eq!(report.bought, 1);
        assert_eq!(report.posted(), 1);
        assert_eq!(report.held, 0);

        let security = security_id_of(&store, "sec-aapl");
        assert_eq!(
            holding_of(store.connection(), &security, SECURITIES),
            (10 * MICRO_SHARE, 150_495),
            "the lot carries the whole cost, commission included"
        );
        assert_eq!(
            entry_lines(&store, "tx-buy"),
            vec![
                (BROKER_CASH.to_string(), -150_495),
                (SECURITIES.to_string(), 150_495),
            ]
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM investment_imports WHERE outcome = 'buy'"
            ),
            1
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM investment_staged_activity"),
            0
        );
    }

    /// A sale relieves the basis of the lots FIFO picks, credits the cash the broker
    /// actually paid, and posts the gain. The proceeds recorded are **gross** — net
    /// plus the fee — because that is what a 1099-B reports and reconciling against
    /// that form is the point (spec §8).
    #[test]
    fn a_sell_relieves_fifo_basis_and_posts_the_gain() {
        let mut store = configured(None);
        import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[buy_apple()],
        );
        let report = import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[sell_apple()],
        );
        assert_eq!(report.sold, 1);

        // 4 of the 10 shares: 150_495 × 4 / 10 = 60_198 cents of basis.
        // Cash credited is the provider's own figure, 79_875. The gain is
        // (80_000 − 125) − 60_198 = 19_677.
        assert_eq!(
            entry_lines(&store, "tx-sell"),
            vec![
                (BROKER_CASH.to_string(), 79_875),
                (SECURITIES.to_string(), -60_198),
                (REALIZED_GAIN.to_string(), -19_677),
            ]
        );
        let (proceeds, fee, basis, gain): (i64, i64, i64, i64) = store
            .connection()
            .query_row(
                "SELECT proceeds_cents, fee_cents, basis_cents, realized_gain_cents
                   FROM investment_sales",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (proceeds, fee, basis, gain),
            (80_000, 125, 60_198, 19_677),
            "gross proceeds with the fee shown apart, as a 1099-B reports them"
        );
        let security = security_id_of(&store, "sec-aapl");
        assert_eq!(
            holding_of(store.connection(), &security, SECURITIES),
            (6 * MICRO_SHARE, 90_297),
            "what is left of the lot is the cost that was not relieved"
        );
    }

    /// A dividend credits the configured dividend account and nothing else. Every
    /// dividend subtype lands there as *ordinary* income: Plaid does not say which
    /// are qualified, and that split comes off the 1099-DIV at year end (spec §7).
    #[test]
    fn a_dividend_posts_to_the_configured_income_account() {
        let mut store = configured(None);
        let dividend = ProviderInvestmentTransaction {
            security_id: Some("sec-aapl".into()),
            security: Some(apple()),
            ..txn("tx-div", BRK, "cash", "dividend", day(2026, 4, 15), -12.34)
        };
        let report = import(&mut store, &[brokerage_account("brokerage")], &[dividend]);
        assert_eq!((report.dividends, report.interest), (1, 0));
        assert_eq!(
            entry_lines(&store, "tx-div"),
            vec![
                (BROKER_CASH.to_string(), 1_234),
                (DIVIDENDS.to_string(), -1_234),
            ]
        );
        // Sweep interest has no security, and that is why phase 1 made the security
        // optional on income.
        let interest = txn("tx-int", BRK, "cash", "interest", day(2026, 4, 30), -0.07);
        let report = import(&mut store, &[brokerage_account("brokerage")], &[interest]);
        assert_eq!((report.dividends, report.interest), (0, 1));
        assert_eq!(
            entry_lines(&store, "tx-int"),
            vec![(BROKER_CASH.to_string(), 7), (INTEREST.to_string(), -7)]
        );
    }

    /// A standalone fee is an ordinary expense. It is on no 1099-B, so folding it
    /// into proceeds would put it on a form that does not report it.
    #[test]
    fn a_fee_posts_as_an_expense() {
        let mut store = configured(None);
        let fee = txn("tx-fee", BRK, "fee", "account fee", day(2026, 5, 1), 9.99);
        let report = import(&mut store, &[brokerage_account("brokerage")], &[fee]);
        assert_eq!(report.fees, 1);
        assert_eq!(
            entry_lines(&store, "tx-fee"),
            vec![(BROKER_CASH.to_string(), -999), (FEES.to_string(), 999)]
        );
    }

    /// Cash in and out posts against the clearing account when one is configured,
    /// and is **held** when one is not — because there is then nowhere truthful to
    /// put the other leg.
    #[test]
    fn cash_movements_need_a_clearing_account_or_they_are_held() {
        let deposit = txn("tx-dep", BRK, "cash", "deposit", day(2026, 2, 1), -500.0);
        let withdrawal = txn("tx-wd", BRK, "cash", "withdrawal", day(2026, 2, 8), 250.0);

        let mut with_clearing = configured(Some(CLEARING));
        let report = import(
            &mut with_clearing,
            &[brokerage_account("brokerage")],
            &[deposit.clone(), withdrawal.clone()],
        );
        assert_eq!((report.cash_movements, report.held), (2, 0));
        assert_eq!(
            entry_lines(&with_clearing, "tx-dep"),
            vec![
                (CLEARING.to_string(), -50_000),
                (BROKER_CASH.to_string(), 50_000),
            ],
            "money in debits brokerage cash and credits the clearing account"
        );
        assert_eq!(
            entry_lines(&with_clearing, "tx-wd"),
            vec![
                (CLEARING.to_string(), 25_000),
                (BROKER_CASH.to_string(), -25_000),
            ]
        );

        let mut without = configured(None);
        let report = import(
            &mut without,
            &[brokerage_account("brokerage")],
            &[deposit, withdrawal],
        );
        assert_eq!((report.cash_movements, report.held), (0, 2));
        assert_eq!(count(&without, "SELECT COUNT(*) FROM journal_entries"), 0);
        let held = pending_activity(without.connection());
        assert!(held.iter().all(|h| h.reason == "no_clearing_account"));
    }

    // -----------------------------------------------------------------------
    // The dedup fence
    // -----------------------------------------------------------------------

    /// The rolling 30-day window means every run re-reads a month it has already
    /// posted, so this is not a guard against a mistake — it is the ordinary path.
    /// Nothing may change on a second import of the same payload.
    #[test]
    fn re_importing_the_same_payload_changes_nothing() {
        let mut store = configured(Some(CLEARING));
        let payload = vec![
            buy_apple(),
            sell_apple(),
            ProviderInvestmentTransaction {
                security_id: Some("sec-aapl".into()),
                security: Some(apple()),
                ..txn("tx-div", BRK, "cash", "dividend", day(2026, 4, 15), -12.34)
            },
            txn("tx-fee", BRK, "fee", "account fee", day(2026, 5, 1), 9.99),
            txn("tx-dep", BRK, "cash", "deposit", day(2026, 2, 1), -500.0),
            // One that is held, so the fence is tested on both tables.
            txn("tx-split", BRK, "transfer", "split", day(2026, 5, 20), 0.0),
        ];
        let first = import(&mut store, &[brokerage_account("brokerage")], &payload);
        assert_eq!((first.posted(), first.held, first.duplicates), (5, 1, 0));

        let entries = count(&store, "SELECT COUNT(*) FROM journal_entries");
        let lots = count(&store, "SELECT COUNT(*) FROM investment_lots");
        let sales = count(&store, "SELECT COUNT(*) FROM investment_sales");
        let events = count(&store, "SELECT COUNT(*) FROM events");
        let staged = count(&store, "SELECT COUNT(*) FROM investment_staged_activity");

        let second = import(&mut store, &[brokerage_account("brokerage")], &payload);
        assert_eq!(
            (second.posted(), second.held, second.duplicates),
            (0, 0, 6),
            "every row of a re-read window is a duplicate, and none of them is held again"
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM journal_entries"),
            entries
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM investment_lots"), lots);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM investment_sales"),
            sales
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM events"),
            events,
            "a re-import appends nothing at all, not even a record of having looked"
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM investment_staged_activity"),
            staged
        );
    }

    /// The fence is not the journal entry's reference, and this is why: a purchase's
    /// reference contains a freshly minted lot id, so a re-import would sail past
    /// migration 014's unique index. The register keyed on the provider's own id is
    /// what stops it.
    #[test]
    fn the_fence_is_the_provider_id_and_not_the_entry_reference() {
        let mut store = configured(None);
        import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[buy_apple()],
        );
        let reference: String = store
            .connection()
            .query_row(
                "SELECT reference FROM journal_entries WHERE reference IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            reference.starts_with("securities-buy-"),
            "the entry reference belongs to the lot link: {reference}"
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM investment_imports WHERE provider_transaction_id = 'tx-buy'"
            ),
            1
        );
    }

    // -----------------------------------------------------------------------
    // Configuration, and the absence of it
    // -----------------------------------------------------------------------

    /// An account nobody has configured has its activity **held** — not dropped and
    /// not guessed. The provider does not hand a transaction over twice, so dropping
    /// it loses it for good.
    #[test]
    fn an_unconfigured_account_holds_its_activity() {
        let mut store = configured(None);
        let stray = ProviderInvestmentTransaction {
            security_id: Some("sec-aapl".into()),
            security: Some(apple()),
            quantity: 1.0,
            price: 10.0,
            ..txn(
                "tx-stray",
                UNCONFIGURED,
                "buy",
                "buy",
                day(2026, 3, 3),
                10.0,
            )
        };
        let report = import(
            &mut store,
            &[ProviderAccount {
                account_id: UNCONFIGURED.into(),
                name: "Somebody else's brokerage".into(),
                subtype: Some("brokerage".into()),
                mask: None,
            }],
            &[stray],
        );
        assert_eq!((report.held, report.posted()), (1, 0));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM investment_lots"), 0);

        let held = pending_activity(store.connection());
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].reason, "unconfigured");
        assert_eq!(held[0].provider_transaction_id, "tx-stray");
        // The raw payload is kept, because a decision about a held row is made from
        // what the broker actually said and not from this importer's summary of it.
        assert!(held[0].raw_payload.contains("sec-aapl"));
    }

    /// Spec §2's subtype rules, and the direction the unknown case errs in.
    #[test]
    fn a_subtype_decides_the_model_and_an_unknown_one_is_taxable() {
        for subtype in ["brokerage", "cash management", "CASH_MANAGEMENT"] {
            assert_eq!(
                classify_subtype(Some(subtype)),
                SubtypeVerdict {
                    treatment: InvestmentTreatment::Taxable,
                    recognised: true
                },
                "{subtype}"
            );
        }
        for subtype in [
            "401k",
            "403b",
            "ira",
            "roth",
            "roth 401k",
            "sep ira",
            "simple ira",
            "529",
            "hsa",
            "ROTH_401K",
        ] {
            assert_eq!(
                classify_subtype(Some(subtype)),
                SubtypeVerdict {
                    treatment: InvestmentTreatment::Sheltered,
                    recognised: true
                },
                "{subtype}"
            );
        }
        // Taxable and flagged, because under-reporting tax is the worse failure:
        // calling an unknown account sheltered excludes everything in it from every
        // tax report, invisibly.
        for subtype in [None, Some("crypto"), Some("something new")] {
            assert_eq!(
                classify_subtype(subtype),
                SubtypeVerdict {
                    treatment: InvestmentTreatment::Taxable,
                    recognised: false
                },
                "{subtype:?}"
            );
        }
    }

    /// An unrecognised subtype is imported as taxable **and** reported, so somebody
    /// is asked. A configuration made against a recognised subtype is flagged too —
    /// a person may know better than the provider, but never silently.
    #[test]
    fn an_unrecognised_subtype_is_taxable_and_flagged_for_confirmation() {
        let mut store = configured(None);
        // Configured as taxable; the provider now calls it something nobody knows.
        let report = import(
            &mut store,
            &[brokerage_account("margin-plus-crypto")],
            &[buy_apple()],
        );
        assert_eq!(report.bought, 1, "it still imports, as taxable");
        assert_eq!(report.flags.len(), 1);
        assert_eq!(report.flags[0].reason, FlagReason::UnrecognisedSubtype);
        assert_eq!(report.flags[0].treatment, InvestmentTreatment::Taxable);
        assert!(
            report.flags[0].message().contains("confirm"),
            "a flag has to ask for something: {}",
            report.flags[0].message()
        );

        // And the other flag: configured one way, reported the other.
        let mut store = configured(None);
        with_sheltered(&mut store);
        let report = import(
            &mut store,
            &[ProviderAccount {
                account_id: IRA_PLAID.into(),
                name: "Fidelity IRA".into(),
                subtype: Some("brokerage".into()),
                mask: None,
            }],
            &[],
        );
        assert_eq!(report.flags.len(), 1);
        assert_eq!(report.flags[0].reason, FlagReason::ConfiguredAgainstSubtype);
        assert_eq!(report.flags[0].treatment, InvestmentTreatment::Sheltered);
    }

    /// The configuration refuses accounts that cannot play the part, for the reason
    /// phase 2 does: a dividend account that is an asset turns income into a
    /// balance-sheet line, the trial balance still balances, and the return comes out
    /// short with nothing to point at.
    #[test]
    fn a_configuration_refuses_accounts_of_the_wrong_type() {
        let mut store = store();
        let wrong = InvestmentPostingAccounts::Taxable(Box::new(TaxableBrokerageAccounts {
            securities_account_id: SECURITIES.into(),
            cash_account_id: BROKER_CASH.into(),
            // An expense account where income belongs.
            dividend_income_account_id: FEES.into(),
            interest_income_account_id: INTEREST.into(),
            realized_gain_account_id: REALIZED_GAIN.into(),
            fee_expense_account_id: FEES.into(),
            transfer_clearing_account_id: None,
        }));
        let err = configure_account(
            &mut store,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: ITEM.into(),
                plaid_account_id: BRK.into(),
                accounts: wrong,
                plaid_subtype: Some("brokerage".into()),
            },
        )
        .expect_err("an expense account cannot receive dividends");
        assert!(matches!(err, ImportError::WrongAccountType { .. }), "{err}");

        // A sheltered account has to be on phase 2's register first: only that
        // records whether a distribution out of it is taxable.
        let err = configure_account(
            &mut store,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: ITEM.into(),
                plaid_account_id: IRA_PLAID.into(),
                accounts: InvestmentPostingAccounts::Sheltered {
                    retirement_account_id: IRA.into(),
                },
                plaid_subtype: Some("ira".into()),
            },
        )
        .expect_err("an unregistered account cannot be imported as sheltered");
        assert!(
            matches!(err, ImportError::NotOnRetirementRegister(_)),
            "{err}"
        );
    }

    // -----------------------------------------------------------------------
    // What is never guessed
    // -----------------------------------------------------------------------

    /// Spec §7: guessing a split silently restates every gain on that security, for
    /// ever, and nobody goes looking for a wrong basis. So each of these is held with
    /// its payload instead.
    #[test]
    fn corporate_actions_and_unknown_types_are_held_never_guessed() {
        let taxable = InvestmentTreatment::Taxable;
        for subtype in ["split", "merger", "spin off", "stock distribution"] {
            // Even arriving as a `buy`, which is how a split sometimes does: posting
            // it as an ordinary purchase would invent a basis nobody paid.
            assert_eq!(
                plan(taxable, "buy", subtype),
                Plan::Hold(HoldReason::CorporateAction),
                "{subtype}"
            );
        }
        assert_eq!(
            plan(taxable, "transfer", "transfer"),
            Plan::Hold(HoldReason::CorporateAction)
        );
        for subtype in ["exercise", "assignment", "sell short", "buy to cover"] {
            assert_eq!(
                plan(taxable, "sell", subtype),
                Plan::Hold(HoldReason::UnhandledType),
                "{subtype}"
            );
        }
        // A capital-gain distribution is Schedule D income, not Schedule B, so it is
        // not quietly called a dividend.
        assert_eq!(
            plan(taxable, "cash", "long-term capital gain"),
            Plan::Hold(HoldReason::UnhandledType)
        );
        assert_eq!(
            plan(taxable, "cancel", "buy"),
            Plan::Hold(HoldReason::UnhandledType)
        );

        // And the ones that do have a rule.
        assert_eq!(plan(taxable, "buy", "buy"), Plan::Post(PostAs::Buy));
        assert_eq!(
            plan(taxable, "buy", "dividend reinvestment"),
            Plan::Post(PostAs::Buy),
            "reinvested shares are bought; the cash leg arrives as its own dividend"
        );
        assert_eq!(plan(taxable, "sell", "sell"), Plan::Post(PostAs::Sell));
        assert_eq!(
            plan(taxable, "fee", "management fee"),
            Plan::Post(PostAs::Fee)
        );
        for subtype in ["dividend", "qualified dividend", "non-qualified dividend"] {
            assert_eq!(
                plan(taxable, "cash", subtype),
                Plan::Post(PostAs::Income(InvestmentIncomeKind::Dividend)),
                "{subtype}"
            );
        }
        assert_eq!(
            plan(taxable, "cash", "interest"),
            Plan::Post(PostAs::Income(InvestmentIncomeKind::Interest))
        );
        assert_eq!(plan(taxable, "cash", "deposit"), Plan::Post(PostAs::Cash));
    }

    /// A trade a command refuses is held with the refusal attached, and the rest of
    /// the payload still lands. A run that stopped at the first refusal would leave
    /// the books half-imported with no record of why.
    #[test]
    fn a_refused_trade_is_held_with_the_reason_and_the_rest_still_imports() {
        let mut store = configured(None);
        // Selling shares that were never bought: phase 1 refuses it under the write
        // lock, and it is a data error every time — the shares are in another
        // account, or the purchase was never entered.
        let report = import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[sell_apple(), buy_apple()],
        );
        assert_eq!((report.sold, report.bought, report.held), (0, 1, 1));
        let held = pending_activity(store.connection());
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].reason, "rejected");
        assert!(
            held[0].detail.contains("nothing to sell") || held[0].detail.contains("lots"),
            "the refusal has to say what was wrong: {}",
            held[0].detail
        );
    }

    // -----------------------------------------------------------------------
    // The security master
    // -----------------------------------------------------------------------

    /// One security seen twice is one master. The mapping table is what makes that
    /// true across a ticker change, which is the whole reason it exists.
    #[test]
    fn a_security_seen_twice_does_not_create_two_masters() {
        let mut store = configured(None);
        let first = import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[buy_apple()],
        );
        assert_eq!(first.securities_created, 1);

        // The same provider security, a second purchase, and a ticker the broker has
        // since renamed. The link is by Plaid's security id, so the position does not
        // fork.
        let renamed = ProviderInvestmentTransaction {
            security: Some(ProviderSecurity {
                ticker: Some("AAPL.NEW".into()),
                ..apple()
            }),
            security_id: Some("sec-aapl".into()),
            quantity: 5.0,
            price: 160.0,
            fees: None,
            ..txn("tx-buy-2", BRK, "buy", "buy", day(2026, 7, 1), 800.0)
        };
        let second = import(&mut store, &[brokerage_account("brokerage")], &[renamed]);
        assert_eq!(second.securities_created, 0);
        assert_eq!(list_securities(store.connection()).len(), 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM plaid_securities"), 1);
        let security = security_id_of(&store, "sec-aapl");
        assert_eq!(
            holding_of(store.connection(), &security, SECURITIES),
            (15 * MICRO_SHARE, 230_495),
            "both purchases are lots of one holding"
        );
    }

    /// A provider security with no ticker still needs a stable identity, and gets a
    /// prefixed placeholder so nobody mistakes it for a market symbol. Most durable
    /// identifier wins.
    #[test]
    fn a_security_with_no_ticker_gets_a_stable_prefixed_identity() {
        assert_eq!(
            ticker_for(&ProviderSecurity {
                security_id: "sec-1".into(),
                ticker: Some(" aapl ".into()),
                ..Default::default()
            }),
            "AAPL"
        );
        assert_eq!(
            ticker_for(&ProviderSecurity {
                security_id: "sec-1".into(),
                ticker: Some("  ".into()),
                cusip: Some("037833100".into()),
                ..Default::default()
            }),
            "CUSIP:037833100"
        );
        assert_eq!(
            ticker_for(&ProviderSecurity {
                security_id: "sec-1".into(),
                isin: Some("US0378331005".into()),
                ..Default::default()
            }),
            "ISIN:US0378331005"
        );
        assert_eq!(
            ticker_for(&ProviderSecurity {
                security_id: "sec-1".into(),
                ..Default::default()
            }),
            "PLAID:sec-1"
        );

        // And it goes onto the master under that identity, so the trade in it is not
        // refused for want of a symbol.
        let mut store = configured(None);
        let untickered = ProviderInvestmentTransaction {
            security_id: Some("sec-fund".into()),
            security: Some(ProviderSecurity {
                security_id: "sec-fund".into(),
                name: Some("A private fund".into()),
                ..Default::default()
            }),
            quantity: 2.0,
            price: 50.0,
            ..txn("tx-fund", BRK, "buy", "buy", day(2026, 3, 9), 100.0)
        };
        let report = import(&mut store, &[brokerage_account("brokerage")], &[untickered]);
        assert_eq!(report.bought, 1);
        let master = list_securities(store.connection());
        assert_eq!(master.len(), 1);
        assert_eq!(master[0].ticker, "PLAID:SEC-FUND");
        assert_eq!(master[0].kind, "unknown", "no type guessed from nothing");
    }

    // -----------------------------------------------------------------------
    // Sheltered accounts
    // -----------------------------------------------------------------------

    /// Nothing inside a sheltered account is recorded, and its value comes from the
    /// holdings snapshot instead (spec §2b). Four hundred trades a year that mean
    /// nothing never reach the books.
    #[test]
    fn a_sheltered_account_ignores_trades_and_takes_its_value_from_holdings() {
        let mut store = configured(None);
        with_sheltered(&mut store);
        let ira_account = ProviderAccount {
            account_id: IRA_PLAID.into(),
            name: "Fidelity IRA".into(),
            subtype: Some("ira".into()),
            mask: Some("5678".into()),
        };

        let trades = vec![
            ProviderInvestmentTransaction {
                security_id: Some("sec-fund".into()),
                security: Some(ProviderSecurity {
                    security_id: "sec-fund".into(),
                    ticker: Some("VTSAX".into()),
                    ..Default::default()
                }),
                quantity: 12.0,
                price: 100.0,
                ..txn(
                    "tx-ira-buy",
                    IRA_PLAID,
                    "buy",
                    "buy",
                    day(2026, 3, 2),
                    1200.0,
                )
            },
            txn(
                "tx-ira-div",
                IRA_PLAID,
                "cash",
                "dividend",
                day(2026, 3, 31),
                -8.0,
            ),
        ];
        let report = import(&mut store, std::slice::from_ref(&ira_account), &trades);
        assert_eq!(
            (report.ignored_sheltered, report.posted(), report.held),
            (2, 0, 0),
            "ignored by design, and not put in front of anybody"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 0);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM investment_lots"), 0);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM investment_staged_activity"),
            0
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM securities"),
            0,
            "a sheltered account's funds never reach the security master"
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM investment_imports"),
            0,
            "ignoring is idempotent by construction, so it needs no register row"
        );

        // The holdings, which are the only thing that does reach the books.
        let holdings = vec![
            ProviderHolding {
                account_id: IRA_PLAID.into(),
                security_id: "sec-fund".into(),
                security: Some(ProviderSecurity {
                    security_id: "sec-fund".into(),
                    ticker: Some("VTSAX".into()),
                    ..Default::default()
                }),
                quantity: 12.0,
                cost_basis: Some(1200.0),
                institution_value: Some(60_000.0),
                iso_currency_code: Some("USD".into()),
            },
            ProviderHolding {
                account_id: IRA_PLAID.into(),
                security_id: "sec-bond".into(),
                security: None,
                quantity: 400.0,
                cost_basis: None,
                institution_value: Some(40_000.0),
                iso_currency_code: Some("USD".into()),
            },
        ];
        let holdings_report = import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 3, 31),
            &holdings,
            &[ira_account],
        )
        .expect("holdings imported");
        assert_eq!(holdings_report.recorded, 1);
        assert_eq!(holdings_report.values_set.len(), 1);
        let (account, value) = &holdings_report.values_set[0];
        assert_eq!(account, IRA_PLAID);
        // $100,000 against a book value of nothing.
        assert_eq!(
            (value.book_value_cents, value.change_cents),
            (0, 10_000_000)
        );
        assert_eq!(
            count(
                &store,
                "SELECT last_value_cents FROM retirement_accounts WHERE account_id = '1500'"
            ),
            10_000_000
        );
        // One entry, of the difference, against the non-taxable value-change account.
        let lines = all_lines(&store);
        assert_eq!(
            lines,
            vec![
                (IRA.to_string(), 10_000_000),
                (VALUE_CHANGE.to_string(), -10_000_000),
            ]
        );
        assert!(
            holdings_report.reconciliations.is_empty(),
            "there is nothing to reconcile inside a sheltered account"
        );

        // Re-importing the same snapshot changes nothing: the snapshot is unchanged,
        // and a value update measured against the books finds they already agree.
        let entries = count(&store, "SELECT COUNT(*) FROM journal_entries");
        let events = count(&store, "SELECT COUNT(*) FROM events");
        let again = import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 3, 31),
            &holdings,
            &[ProviderAccount {
                account_id: IRA_PLAID.into(),
                name: "Fidelity IRA".into(),
                subtype: Some("ira".into()),
                mask: None,
            }],
        )
        .expect("holdings imported again");
        assert_eq!((again.recorded, again.unchanged), (0, 1));
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM journal_entries"),
            entries
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM events"), events);
    }

    /// Cash into a sheltered account is a contribution or a distribution and the
    /// provider cannot say which. The two are opposite on a return, so it waits for a
    /// person rather than being posted either way.
    #[test]
    fn cash_into_a_sheltered_account_is_held_rather_than_posted() {
        let mut store = configured(None);
        with_sheltered(&mut store);
        let movements = vec![
            txn(
                "tx-ira-in",
                IRA_PLAID,
                "cash",
                "contribution",
                day(2026, 1, 15),
                -6_500.0,
            ),
            txn(
                "tx-ira-out",
                IRA_PLAID,
                "cash",
                "distribution",
                day(2026, 8, 1),
                2_000.0,
            ),
        ];
        let report = import(
            &mut store,
            &[ProviderAccount {
                account_id: IRA_PLAID.into(),
                name: "Fidelity IRA".into(),
                subtype: Some("ira".into()),
                mask: None,
            }],
            &movements,
        );
        assert_eq!(
            (report.held, report.posted(), report.ignored_sheltered),
            (2, 0, 0)
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 0);
        let held = pending_activity(store.connection());
        assert_eq!(held.len(), 2);
        assert!(held.iter().all(|h| h.reason == "sheltered_cash"));
        assert!(
            held[0].detail.contains("contribution") && held[0].detail.contains("distribution"),
            "the row has to say what the question is: {}",
            held[0].detail
        );
        // And the amounts are there for a person to act on, converted once.
        assert_eq!(held[0].amount_cents, Some(-650_000));
        assert_eq!(held[1].amount_cents, Some(200_000));
    }

    /// A sheltered account whose holdings do not all carry a value has **no** value
    /// set. A total missing one holding is not a total, and posting it would book a
    /// loss that did not happen.
    #[test]
    fn a_sheltered_value_is_not_set_from_an_incomplete_snapshot() {
        let mut store = configured(None);
        with_sheltered(&mut store);
        let account = ProviderAccount {
            account_id: IRA_PLAID.into(),
            name: "Fidelity IRA".into(),
            subtype: Some("ira".into()),
            mask: None,
        };
        let report = import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 3, 31),
            &[
                ProviderHolding {
                    account_id: IRA_PLAID.into(),
                    security_id: "sec-fund".into(),
                    quantity: 12.0,
                    institution_value: Some(60_000.0),
                    ..Default::default()
                },
                ProviderHolding {
                    account_id: IRA_PLAID.into(),
                    security_id: "sec-mystery".into(),
                    quantity: 1.0,
                    institution_value: None,
                    ..Default::default()
                },
            ],
            &[account],
        )
        .expect("holdings imported");
        assert_eq!(report.recorded, 1, "the snapshot is still recorded");
        assert!(report.values_set.is_empty());
        assert_eq!(report.incomplete_values, vec![IRA_PLAID.to_string()]);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 0);
    }

    // -----------------------------------------------------------------------
    // Reconciliation (spec §7)
    // -----------------------------------------------------------------------

    fn apple_holding(quantity: f64, cost_basis: Option<f64>) -> ProviderHolding {
        ProviderHolding {
            account_id: BRK.into(),
            security_id: "sec-aapl".into(),
            security: Some(apple()),
            quantity,
            cost_basis,
            institution_value: Some(quantity * 180.0),
            iso_currency_code: Some("USD".into()),
        }
    }

    fn reconciled(quantity: f64, cost_basis: Option<f64>) -> (EventStore, Reconciliation) {
        let mut store = configured(None);
        import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[buy_apple()],
        );
        let report = import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 3, 31),
            &[apple_holding(quantity, cost_basis)],
            &[brokerage_account("brokerage")],
        )
        .expect("holdings imported");
        assert_eq!(report.reconciliations.len(), 1);
        let reconciliation = report.reconciliations[0].clone();
        (store, reconciliation)
    }

    /// A taxable account's snapshot posts nothing and reconciles clean when the books
    /// and the broker agree.
    #[test]
    fn a_reconciliation_that_matches_reports_no_differences() {
        let (store, reconciliation) = reconciled(10.0, Some(1_504.95));
        assert!(reconciliation.agrees());
        assert!(reconciliation.disagreements().is_empty());
        assert_eq!(reconciliation.as_of, day(2026, 3, 31));
        assert_eq!(reconciliation.lines.len(), 1);
        let line = &reconciliation.lines[0];
        assert_eq!(line.ticker.as_deref(), Some("AAPL"));
        assert_eq!(
            (line.book_quantity, line.broker_quantity),
            (10 * MICRO_SHARE, 10 * MICRO_SHARE)
        );
        assert_eq!(
            (line.book_cost_cents, line.broker_cost_cents),
            (150_495, Some(150_495))
        );
        assert_eq!(line.quantity_difference(), 0);
        assert_eq!(line.cost_difference(), Some(0));

        // Market value is never posted: only the purchase's entry exists.
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 1);
    }

    /// Quantity drift on its own — the shape a split makes. Reported, never applied:
    /// applying it would restate every gain on the security.
    #[test]
    fn a_reconciliation_reports_quantity_drift_on_its_own() {
        let (_store, reconciliation) = reconciled(20.0, Some(1_504.95));
        assert!(!reconciliation.agrees());
        let differences = reconciliation.disagreements();
        assert_eq!(differences.len(), 1);
        // A 2-for-1 split: the broker has twice the shares at the same cost.
        assert_eq!(differences[0].quantity_difference(), -10 * MICRO_SHARE);
        assert_eq!(
            differences[0].cost_difference(),
            Some(0),
            "the cost agrees, which is exactly what makes it a split and not a missing trade"
        );
    }

    /// Cost drift on its own — a return of capital, or a basis the broker corrected.
    #[test]
    fn a_reconciliation_reports_cost_drift_on_its_own() {
        let (_store, reconciliation) = reconciled(10.0, Some(1_400.00));
        assert!(!reconciliation.agrees());
        let differences = reconciliation.disagreements();
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].quantity_difference(), 0);
        assert_eq!(differences[0].cost_difference(), Some(150_495 - 140_000));
    }

    /// A basis the broker will not state is **not comparable**, which is different
    /// from a basis of zero. Calling it zero would report a difference the size of
    /// the whole holding on every transferred-in lot.
    #[test]
    fn a_basis_the_broker_does_not_state_is_not_a_difference() {
        let (_store, reconciliation) = reconciled(10.0, None);
        assert!(reconciliation.agrees());
        assert_eq!(reconciliation.lines[0].cost_difference(), None);
    }

    /// The other direction: a position the books hold and the snapshot does not
    /// mention. Without this the comparison would be half blind — a sale the broker
    /// made and we never imported would reconcile clean.
    #[test]
    fn a_position_missing_from_the_snapshot_is_a_difference_too() {
        let mut store = configured(None);
        import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[buy_apple()],
        );
        let report = import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 3, 31),
            &[],
            &[brokerage_account("brokerage")],
        )
        .expect("an empty snapshot is still a snapshot");
        let reconciliation = &report.reconciliations[0];
        assert!(!reconciliation.agrees());
        assert_eq!(reconciliation.lines.len(), 1);
        let line = &reconciliation.lines[0];
        assert_eq!(line.plaid_security_id, None);
        assert_eq!(line.book_quantity, 10 * MICRO_SHARE);
        assert_eq!(line.broker_quantity, 0);
        assert_eq!(line.quantity_difference(), 10 * MICRO_SHARE);
    }

    /// An unconfigured account's **holdings** are skipped rather than held, and the
    /// difference from a transaction is the one that matters: a holdings read is not
    /// destructive, so the same snapshot can simply be taken again once somebody has
    /// configured the account.
    #[test]
    fn unconfigured_holdings_are_skipped_because_a_snapshot_can_be_taken_again() {
        let mut store = configured(None);
        let report = import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 3, 31),
            &[ProviderHolding {
                account_id: UNCONFIGURED.into(),
                security_id: "sec-aapl".into(),
                quantity: 1.0,
                ..Default::default()
            }],
            &[ProviderAccount {
                account_id: UNCONFIGURED.into(),
                ..Default::default()
            }],
        )
        .expect("holdings imported");
        assert_eq!(report.recorded, 0);
        assert_eq!(report.skipped_unconfigured, vec![UNCONFIGURED.to_string()]);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM investment_holdings_snapshots"),
            0
        );
    }

    // -----------------------------------------------------------------------
    // The fetch window
    // -----------------------------------------------------------------------

    /// Full history the first time, then a rolling window on top of the last date
    /// fetched. The overlap is the point: a provider revises transactions after
    /// reporting them, and a window starting exactly where the last one ended would
    /// miss every revision.
    #[test]
    fn the_fetch_window_is_full_history_first_and_thirty_days_after() {
        let today = day(2026, 9, 29);
        assert_eq!(
            fetch_window(None, today),
            (day(2016, 9, 29), today),
            "the first pull asks for everything anyone has"
        );
        assert_eq!(
            fetch_window(Some(day(2026, 9, 20)), today),
            (day(2026, 8, 21), today),
            "afterwards, thirty days back from where the last one finished"
        );
        // A month boundary, to make sure the 30 days are days and not a month.
        assert_eq!(
            fetch_window(Some(day(2026, 3, 1)), day(2026, 3, 2)).0,
            day(2026, 1, 30)
        );
        // A clock put back: a last-fetched date in the future would otherwise produce
        // a window that starts after it ends, which the endpoint refuses.
        let (start, end) = fetch_window(Some(day(2027, 1, 1)), today);
        assert!(start <= end, "{start} .. {end}");
        assert_eq!((start, end), (today, today));
    }

    /// And the window is read from, and recorded in, the machine-local fetch state.
    #[test]
    fn recording_a_fetch_moves_the_window_and_never_moves_it_back() {
        let store = configured(None);
        let conn = store.connection();
        let today = day(2026, 9, 29);
        assert_eq!(last_fetched_through(conn, ITEM, BRK), None);
        assert_eq!(window_for(conn, ITEM, BRK, today).0, day(2016, 9, 29));

        record_fetch(conn, ITEM, BRK, day(2026, 9, 20)).unwrap();
        assert_eq!(
            last_fetched_through(conn, ITEM, BRK),
            Some(day(2026, 9, 20))
        );
        assert_eq!(window_for(conn, ITEM, BRK, today).0, day(2026, 8, 21));

        // An older window recorded afterwards — a second machine catching up, or a
        // clock put back — must not shorten what has already been covered.
        record_fetch(conn, ITEM, BRK, day(2026, 5, 1)).unwrap();
        assert_eq!(
            last_fetched_through(conn, ITEM, BRK),
            Some(day(2026, 9, 20))
        );

        // One request serves a whole connection, so the window has to be the widest
        // any of its accounts needs: an account added years in must still get its
        // full history.
        assert_eq!(
            window_for_item(conn, ITEM, &[BRK.to_string(), IRA_PLAID.to_string()], today),
            (day(2016, 9, 29), today)
        );
        record_fetch(conn, ITEM, IRA_PLAID, day(2026, 9, 20)).unwrap();
        assert_eq!(
            window_for_item(conn, ITEM, &[BRK.to_string(), IRA_PLAID.to_string()], today),
            (day(2026, 8, 21), today)
        );
    }

    // -----------------------------------------------------------------------
    // Spec §6: investment accounts leave the transactions feed
    // -----------------------------------------------------------------------

    /// `/transactions/sync` reports only the cash leg, so a purchase arrives there as
    /// money spent and a sale as income. Once an account is configured as an
    /// investment account its rows stop being staged, because every fact in them
    /// arrives better — with the security attached — from the investments endpoints.
    #[test]
    fn a_configured_investment_account_stops_flowing_through_the_transactions_feed() {
        use crate::commands::plaid_commands::{stage_transactions_in_conn, SyncedTransaction};

        let store = configured(None);
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', ?1, 'Brokerage', 'investment', ?2),
                        ('item1', 'plaid-checking', 'Checking', 'depository', ?3)",
                rusqlite::params![BRK, BROKER_CASH, CHECKING],
            )
            .unwrap();

        let cash_leg = |id: &str, account: &str| SyncedTransaction {
            transaction_id: id.to_string(),
            account_id: account.to_string(),
            amount: 1504.95,
            date: "2026-03-02".to_string(),
            name: "BUY AAPL".to_string(),
            merchant_name: None,
            pending: false,
            iso_currency_code: Some("USD".to_string()),
            currency: None,
            payment_meta: None,
        };
        let outcome = stage_transactions_in_conn(
            store.connection(),
            ITEM,
            &[
                cash_leg("bank-1", BRK),
                cash_leg("bank-2", "plaid-checking"),
            ],
        )
        .expect("staged");
        assert_eq!(outcome.investment_accounts, 1);
        assert_eq!(outcome.staged, 1, "only the bank account's row is staged");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM plaid_staged_transactions WHERE plaid_account_id = 'plaid-brokerage'"
            ),
            0
        );
    }
}
