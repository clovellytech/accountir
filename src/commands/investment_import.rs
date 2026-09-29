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
//!
//! # One planner, two sinks
//!
//! The same payload has to import into local books and into a group's, and on a
//! group's the local copy is a **replica**: the server owns the log and the only
//! writer to the replica's copy of it is the mirror path in [`crate::sync::replica`].
//! An append here would fork it.
//!
//! So the importer is split at the one place the two differ. Everything that is a
//! judgement — the classification, the subtype rules, the corporate-action holds, the
//! `f64` → integer conversion, the dedup fence, which securities subaccount a
//! position is carried in, which account a kind of income posts to — happens once, in
//! [`plan_payload`], and produces a [`PlannedWrite`] per transaction with nothing left
//! to decide. Where that write lands is the only fork:
//!
//! * [`import_transactions`] and [`import_holdings`] append it here;
//! * [`import_transactions_hosted`] and [`import_holdings_hosted`] submit it to the
//!   group server, which runs [`build_import_in_txn`] — *this* module's builder —
//!   inside its own append transaction.
//!
//! That last point is what makes the two paths equivalent rather than merely similar:
//! the fences are not re-implemented on the server, they are the same functions run
//! under a different write lock. `sync::commands::investments`'s
//! `a_hosted_import_and_a_local_import_of_one_payload_produce_the_same_log` is what
//! holds it to be true.
//!
//! One consequence worth naming: a payload's security masters are all resolved before
//! any of its postings, because resolution is the one thing the planner has to ask a
//! sink for and asking it up front is what keeps the planner free of writes. The log
//! of one import run therefore lists its `SecurityDefined` events first rather than
//! interleaved with the trades — on both paths, and with no effect on any balance,
//! any basis or any fence.
//!
//! The review list and the fetch window stay **local in both modes**: what this
//! machine is still looking at and how far it has fetched are facts about the machine
//! rather than about the books (migration 050).

use chrono::{Days, Months, NaiveDate};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;
use uuid::Uuid;

use crate::commands::investment_commands::{
    self, build_buy_in_txn, build_define_security_in_txn, build_fee_in_txn, build_income_in_txn,
    build_sell_in_txn, BuySecurityCommand, ChargeInvestmentFeeCommand, InvestmentError,
    InvestmentStep, LotSelection, NewSecurity, RecordInvestmentIncomeCommand, SellSecurityCommand,
};
use crate::commands::retirement_commands::{self, SetRetirementValueCommand, ValueSet};
use crate::events::types::{
    Event, EventEnvelope, HoldingsSnapshotData, ImportedActivityKind, InvestmentAccountConfigData,
    InvestmentActivityImportedData, InvestmentIncomeKind, InvestmentPostingAccounts,
    InvestmentTreatment, PlaidSecurityLinkData, SecurityKindGroup, SnapshotHoldingData,
    StoredEvent, TaxableBrokerageAccounts,
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
    #[error(
        "No activity with id {0} is waiting for review on this machine. The review list is \
         local, so a row resolved on another machine is not here to resolve again."
    )]
    NoSuchStagedActivity(String),
    #[error(
        "That activity is already {status}, so nothing was done to it. Resolving it twice would \
         post the same transaction twice."
    )]
    NotPending { status: String },
    #[error(
        "Only a dismissed row can be put back. This one is {status}: it reached the books, and \
         reopening it would invite the same transaction being posted a second time."
    )]
    NotDismissed { status: String },
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
    /// Income of a kind this account has no income account configured for. In
    /// practice a capital gain distribution, which is the one income account with
    /// no fallback: it is Schedule D and the dividend account is Schedule B.
    NoIncomeAccount,
    /// A trade with no security attached, which cannot become a lot.
    UnknownSecurity,
    /// A quantity or an amount that did not survive the conversion to integers.
    BadAmount,
    /// A command refused it under the write lock — a sale larger than the position,
    /// a posting into a closed year.
    Refused,
}

impl HoldReason {
    /// Every reason, so a caller can turn a stored string back into one without a
    /// second copy of the list. A row whose reason is not in here was written by a
    /// later build; it is still shown, carrying the string it came with.
    pub const ALL: [HoldReason; 9] = [
        HoldReason::Unconfigured,
        HoldReason::CorporateAction,
        HoldReason::UnhandledType,
        HoldReason::ShelteredCash,
        HoldReason::NoClearingAccount,
        HoldReason::NoIncomeAccount,
        HoldReason::UnknownSecurity,
        HoldReason::BadAmount,
        HoldReason::Refused,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            HoldReason::Unconfigured => "unconfigured",
            HoldReason::CorporateAction => "corporate_action",
            HoldReason::UnhandledType => "unhandled_type",
            HoldReason::ShelteredCash => "sheltered_cash",
            HoldReason::NoClearingAccount => "no_clearing_account",
            HoldReason::NoIncomeAccount => "no_income_account",
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
            HoldReason::NoIncomeAccount => {
                "This is a capital gain distribution and no account is configured for one. It is \
                 Schedule D income rather than Schedule B, so it cannot go to the dividend \
                 account — configure a capital gain distribution account for this brokerage and \
                 import again."
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

/// A fund passing through a gain it realized.
///
/// Held for review until phase 5 gave them an account of their own, and posted now
/// — to **that** account, never to dividends. A capital gain distribution is
/// Schedule D income; a dividend is Schedule B; and the difference is a rate, so
/// the account has to be configured before one can post (see
/// [`TaxableBrokerageAccounts::income_account_for`], which is the only slot with no
/// fallback).
///
/// The reinvestment subtypes are in here because that is how Plaid reports a fund
/// distribution taken in shares: the income arrives and buys shares, and it is
/// **two** events — Plaid sends the `cash` row and a separate `buy` row for the
/// purchase. Treating the cash row as the purchase would record the shares without
/// the income and leave a lot with no money behind it.
///
/// Short term and long term are not distinguished here on purpose. Both are
/// ordinary credits to one account during the year; which box of a 1099-DIV they
/// came out of is what the year-end capture reads (spec §8), and splitting them
/// into two accounts now would be a second opinion about a form we have not read
/// yet.
const CAPITAL_GAIN_SUBTYPES: [&str; 4] = [
    "long term capital gain",
    "long term capital gain reinvestment",
    "short term capital gain",
    "short term capital gain reinvestment",
];

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
            } else if CAPITAL_GAIN_SUBTYPES.contains(&sub.as_str()) {
                // Posted since phase 5, to the account configured for it. Held
                // before that, and still held when no account is configured — the
                // hold has moved from "there is no rule for this" to "there is
                // nowhere for this to go", which is a reason somebody can act on.
                Plan::Post(PostAs::Income(
                    InvestmentIncomeKind::CapitalGainDistribution,
                ))
            } else if CASH_MOVEMENT_SUBTYPES.contains(&sub.as_str()) {
                Plan::Post(PostAs::Cash)
            } else {
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
     transfer_clearing_account_id, retirement_account_id, mutual_funds_account_id,
     other_securities_account_id, tax_exempt_interest_account_id,
     capital_gain_distribution_account_id";

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
            //
            // The four columns migration 051 added are genuinely nullable — a
            // configuration written before they existed has none — so they are
            // read straight through and each one's fallback is the field's own
            // business. See `TaxableBrokerageAccounts`.
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
                    stocks_account_id: securities,
                    mutual_funds_account_id: row.get(13)?,
                    other_securities_account_id: row.get(14)?,
                    cash_account_id: cash,
                    dividend_income_account_id: dividends,
                    interest_income_account_id: interest,
                    tax_exempt_interest_account_id: row.get(15)?,
                    capital_gain_distribution_account_id: row.get(16)?,
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

pub(crate) fn build_configure_in_txn(
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
            // Every securities slot against the cash account, not only the stocks
            // one: a purchase posting to the account it is paid out of balances,
            // moves nothing, and leaves the balance sheet silently missing the
            // whole holding — and that is as available on a slot added later as on
            // the first.
            for group in SecurityKindGroup::ALL {
                if a.securities_account_of(group) == a.cash_account_id {
                    return Ok(ImportStep::Reject(ImportError::Invalid(format!(
                        "the {} securities account and the cash account cannot be the same \
                         account: every purchase would post to itself and change nothing",
                        group.label().to_lowercase()
                    ))));
                }
            }
            let mut checks: Vec<(&str, &'static str, &'static [&'static str])> = vec![
                (
                    a.stocks_account_id.as_str(),
                    "stocks securities account",
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
            // The slots migration 051 added, each checked only when it is named.
            // An unconfigured slot is not an error — it falls back to the stocks
            // account, or, for a capital gain distribution, holds the activity —
            // but a *named* one has to be able to play its part, for the reason
            // this whole block exists: a dividend account that is an asset turns
            // income into a balance-sheet line, the trial balance still balances,
            // and the return comes out short with nothing to point at.
            if let Some(id) = a.mutual_funds_account_id.as_deref() {
                checks.push((id, "mutual funds securities account", &["asset"]));
            }
            if let Some(id) = a.other_securities_account_id.as_deref() {
                checks.push((id, "other securities account", &["asset"]));
            }
            if let Some(id) = a.tax_exempt_interest_account_id.as_deref() {
                checks.push((id, "tax-exempt interest account", &["revenue"]));
            }
            if let Some(id) = a.capital_gain_distribution_account_id.as_deref() {
                checks.push((id, "capital gain distribution account", &["revenue"]));
            }
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

// ---------------------------------------------------------------------------
// One import's decided write: the plan, and the two places it can land
// ---------------------------------------------------------------------------

/// Cash into or out of a taxable brokerage, against the configured clearing
/// account.
///
/// The one imported write with no phase-1 command behind it — it is an ordinary
/// two-line transfer — and it gets a command type of its own anyway so that it can
/// travel the same road as the other four: one planner decides it, and either sink
/// writes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportedCashMovement {
    pub cash_account_id: String,
    /// Where the other leg goes while the bank feed has not reported it yet.
    pub clearing_account_id: String,
    /// **Signed**: positive is cash arriving at the brokerage, negative is cash
    /// leaving it. One signed field rather than two branches, because the sign is
    /// the whole content of this entry and two branches is two places to get it
    /// wrong.
    pub into_brokerage_cents: i64,
    pub on: NaiveDate,
    pub memo: String,
}

/// What the importer decided to write for one provider transaction, with nothing
/// left to decide.
///
/// This is the seam between the planner and the sink. Everything difficult — the
/// classification, the subtype rules, the corporate-action holds, the `f64` →
/// integer conversion, which securities subaccount a holding lives in, which
/// account a kind of income posts to — has already happened by the time one of
/// these exists. What is left is *where the write lands*: the append loop in this
/// module on a local book, or [`SyncClient::import_investment_activity`] on a
/// group's, which posts this very value to the group server.
///
/// One type on both paths rather than a wire twin of it, for the reason
/// [`LotSelection`] gives: a hosted import has to post exactly what a local one
/// would, and two types that have to agree about that are two types that can stop
/// agreeing.
///
/// [`SyncClient::import_investment_activity`]: crate::sync::SyncClient::import_investment_activity
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedWrite {
    Buy {
        /// Minted by the planner, not by whichever sink writes it, so the lot id
        /// in the register, in the entry's reference and in the import record is
        /// one id however the write travelled. It is also what makes a retry after
        /// a 409 idempotent against migration 014's reference fence.
        lot_id: String,
        cmd: BuySecurityCommand,
    },
    Sell {
        sale_id: String,
        cmd: SellSecurityCommand,
    },
    Income {
        cmd: RecordInvestmentIncomeCommand,
    },
    Fee {
        cmd: ChargeInvestmentFeeCommand,
    },
    Cash {
        cmd: ImportedCashMovement,
    },
}

impl PlannedWrite {
    /// What the import register will call this. Derived rather than carried beside
    /// the write, so the two cannot disagree about what was imported.
    pub fn outcome(&self) -> ImportedActivityKind {
        match self {
            PlannedWrite::Buy { .. } => ImportedActivityKind::Buy,
            PlannedWrite::Sell { .. } => ImportedActivityKind::Sell,
            PlannedWrite::Income { cmd } => match cmd.kind {
                InvestmentIncomeKind::Dividend => ImportedActivityKind::Dividend,
                // Tax-exempt interest is recorded as interest in the import
                // register: the importer never chooses it (Plaid has no subtype
                // for it — see the field on `TaxableBrokerageAccounts`), and a
                // caller that passes it anyway has posted interest to a different
                // account, which is what the entry says.
                InvestmentIncomeKind::Interest | InvestmentIncomeKind::TaxExemptInterest => {
                    ImportedActivityKind::Interest
                }
                InvestmentIncomeKind::CapitalGainDistribution => {
                    ImportedActivityKind::CapitalGainDistribution
                }
            },
            PlannedWrite::Fee { .. } => ImportedActivityKind::Fee,
            PlannedWrite::Cash { .. } => ImportedActivityKind::Cash,
        }
    }

    fn lot_id(&self) -> Option<&str> {
        match self {
            PlannedWrite::Buy { lot_id, .. } => Some(lot_id),
            _ => None,
        }
    }

    fn sale_id(&self) -> Option<&str> {
        match self {
            PlannedWrite::Sell { sale_id, .. } => Some(sale_id),
            _ => None,
        }
    }
}

/// Which provider transaction a [`PlannedWrite`] is the import of.
///
/// The dedup fence's key, and what the log records beside the entry. Everything
/// else the register holds — the outcome, the lot, the sale — is read off the write
/// and off the entry the command built, rather than repeated here where it could
/// drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportRecord {
    pub provider_transaction_id: String,
    pub item_id: String,
    pub plaid_account_id: String,
}

/// The refusal a second import of the same provider transaction gets.
///
/// One constructor because both sinks read it: the local path holds the row
/// carrying it, and the hosted path recognises it coming back as a `422` and
/// counts a duplicate instead of holding one. See
/// [`import_transactions_hosted`] for why that case is reachable at all.
pub(crate) fn already_imported_message(provider_transaction_id: &str) -> String {
    format!("{provider_transaction_id} has already been imported")
}

/// Build one import's batch: the trade or the entry, its register event, and the
/// import record that fences it against the next fetch — as one unit.
///
/// The **whole** of what an import writes to a book, and the reason the hosted path
/// is a different sink rather than a different importer: this function runs inside
/// the append transaction either way, locally under [`run`] and on a group's books
/// inside the server's own `append_checked_many`. So FIFO, the over-sale refusal,
/// the closed-year fence, the reference uniqueness *and* the dedup fence are
/// re-decided against locked state on both paths, and neither path can grow a
/// second opinion about any of them.
pub(crate) fn build_import_in_txn(
    tx: &rusqlite::Transaction<'_>,
    write: &PlannedWrite,
    record: &ImportRecord,
) -> Result<ImportStep, EventStoreError> {
    // Under the write lock, and repeated although the planner already looked: the
    // planner's read happened outside the lock, and two imports of the same payload
    // running at once would both pass it. Here the second one is refused.
    if already_imported_in_txn(tx, &record.provider_transaction_id)? {
        return Ok(ImportStep::Reject(ImportError::Refused(
            already_imported_message(&record.provider_transaction_id),
        )));
    }
    let step = match write {
        PlannedWrite::Buy { lot_id, cmd } => build_buy_in_txn(tx, lot_id, cmd)?,
        PlannedWrite::Sell { sale_id, cmd } => build_sell_in_txn(tx, sale_id, cmd)?,
        PlannedWrite::Income { cmd } => build_income_in_txn(tx, cmd)?,
        PlannedWrite::Fee { cmd } => build_fee_in_txn(tx, cmd)?,
        PlannedWrite::Cash { cmd } => build_cash_in_txn(tx, cmd)?,
    };
    Ok(from_investment_step(step, write, record))
}

/// The imported cash movement's entry.
///
/// The zero check is here as well as in the planner for the same reason the dedup
/// check is: [`investment_commands::entry_or_reject`] would pass two lines of zero
/// — they balance — and a hosted caller is not to be trusted to have looked.
fn build_cash_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &ImportedCashMovement,
) -> Result<InvestmentStep, EventStoreError> {
    if cmd.into_brokerage_cents == 0 {
        return Ok(InvestmentStep::Reject(InvestmentError::Invalid(
            NOTHING_MOVED.to_string(),
        )));
    }
    let currency = base_currency_in_txn(tx)?;
    let lines = vec![
        (
            cmd.cash_account_id.clone(),
            cmd.into_brokerage_cents,
            "Brokerage cash",
        ),
        (
            cmd.clearing_account_id.clone(),
            -cmd.into_brokerage_cents,
            "Cash in transit",
        ),
    ];
    Ok(
        match investment_commands::entry_or_reject(
            tx,
            cmd.on,
            cmd.memo.clone(),
            None,
            &lines,
            &currency,
        )? {
            Ok(entry) => InvestmentStep::Append(vec![entry]),
            Err(e) => InvestmentStep::Reject(e),
        },
    )
}

/// What a cash movement of nothing is refused with, on either path.
const NOTHING_MOVED: &str = "a cash movement of nothing moves no money";

/// Turn a phase-1 step into one of ours, appending the import record to the batch
/// and carrying a refusal through as an [`ImportError::Refused`] so the caller can
/// hold the row instead of failing the whole run.
fn from_investment_step(
    step: InvestmentStep,
    write: &PlannedWrite,
    record: &ImportRecord,
) -> ImportStep {
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
                    provider_transaction_id: record.provider_transaction_id.clone(),
                    item_id: record.item_id.clone(),
                    plaid_account_id: record.plaid_account_id.clone(),
                    outcome: write.outcome(),
                    entry_id,
                    lot_id: write.lot_id().map(str::to_string),
                    sale_id: write.sale_id().map(str::to_string),
                },
            )));
            ImportStep::Append(events)
        }
        InvestmentStep::Reject(e) => ImportStep::Reject(ImportError::Refused(e.to_string())),
    }
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

/// Which of our securities a provider's security is, and what our master calls its
/// type.
///
/// The kind travels with the id because [`TaxableBrokerageAccounts::securities_account_for_kind`]
/// needs it and **our** master is the only honest source for it — see
/// [`plan_post`]. On a replica the master may hold a security the local copy has
/// not pulled yet, so reading the kind back out of the local `securities` table
/// after a hosted define would find nothing and file a stock under "other
/// securities". The server answers with the kind it holds instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasterSecurity {
    pub security_id: String,
    pub kind: String,
}

/// The provider's security as our master would hold it.
///
/// Every normalisation the master needs happens here and nowhere else, so a hosted
/// import sends the server the same description a local import would have written
/// itself.
pub fn provider_security_as_new(security: &ProviderSecurity) -> NewSecurity {
    let ticker = ticker_for(security);
    NewSecurity {
        name: security
            .name
            .as_ref()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| ticker.clone()),
        // Plaid's own vocabulary, which is exactly what phase 1 left `kind` free
        // text for. "unknown" rather than a guess when it says nothing: a label
        // nothing branches on is better wrong-shaped than invented.
        kind: security
            .security_type
            .as_ref()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        cusip: security
            .cusip
            .as_ref()
            .map(|c| c.trim().to_uppercase())
            .filter(|c| !c.is_empty()),
        currency: security
            .iso_currency_code
            .clone()
            .unwrap_or_else(|| "USD".to_string()),
        ticker,
    }
}

/// What resolving a provider security came to, and the batch that records it.
pub(crate) struct SecurityResolution {
    pub master: MasterSecurity,
    /// Whether a master was minted for it. Counted in the report, and the one thing
    /// a caller cannot work out from the id alone.
    pub created: bool,
    pub step: ImportStep,
}

/// Find or create the security master for a provider security, and link it —
/// inside the append transaction.
///
/// Three ways of finding it before creating one, in order of trust:
///
/// 1. `plaid_securities`, which is the answer once it exists;
/// 2. the CUSIP, which survives a ticker change, so a security whose symbol changed
///    between two imports is recognised rather than duplicated;
/// 3. the ticker, which catches a security somebody already entered by hand.
///
/// Whichever way it is found, the link is appended so that the next import takes
/// the first route. A master created **and not linked** is indistinguishable from
/// one somebody typed, and the difference matters when a ticker is reassigned —
/// which is why the definition and the link are one batch.
///
/// All three lookups are under the write lock rather than before it, and on a
/// group's books that is what stops two members importing at once from minting two
/// masters for one CUSIP and splitting a holding across them. An already-mapped
/// security appends nothing at all, so re-resolving is free of events.
pub(crate) fn build_resolve_security_in_txn(
    tx: &rusqlite::Transaction<'_>,
    plaid_security_id: &str,
    new: &NewSecurity,
) -> Result<SecurityResolution, EventStoreError> {
    let found = |tx: &rusqlite::Transaction<'_>, security_id: String| {
        let kind = investment_commands::get_security(tx, &security_id)
            .map(|s| s.kind)
            .unwrap_or_default();
        MasterSecurity { security_id, kind }
    };

    if let Some(existing) = tx
        .query_row(
            "SELECT security_id FROM plaid_securities WHERE plaid_security_id = ?1",
            [plaid_security_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(SecurityResolution {
            master: found(tx, existing),
            created: false,
            step: ImportStep::Append(Vec::new()),
        });
    }

    let ticker = new.ticker.trim().to_uppercase();
    let matched: Option<String> = match &new.cusip {
        Some(cusip) => tx
            .query_row("SELECT id FROM securities WHERE cusip = ?1", [cusip], |r| {
                r.get::<_, String>(0)
            })
            .optional()?,
        None => None,
    }
    .or(tx
        .query_row("SELECT id FROM securities WHERE ticker = ?1", [&ticker], |r| {
            r.get::<_, String>(0)
        })
        .optional()?);

    let link = |security_id: &str| {
        Event::PlaidSecurityLinked(Box::new(PlaidSecurityLinkData {
            plaid_security_id: plaid_security_id.to_string(),
            security_id: security_id.to_string(),
        }))
    };

    if let Some(security_id) = matched {
        return Ok(SecurityResolution {
            master: found(tx, security_id.clone()),
            created: false,
            step: ImportStep::Append(vec![link(&security_id)]),
        });
    }

    let security_id = Uuid::new_v4().to_string();
    match build_define_security_in_txn(tx, &security_id, new)? {
        InvestmentStep::Append(mut events) => {
            events.push(link(&security_id));
            Ok(SecurityResolution {
                master: MasterSecurity {
                    security_id,
                    kind: new.kind.trim().to_string(),
                },
                created: true,
                step: ImportStep::Append(events),
            })
        }
        InvestmentStep::Reject(e) => Ok(SecurityResolution {
            master: MasterSecurity::default(),
            created: false,
            step: ImportStep::Reject(ImportError::Refused(e.to_string())),
        }),
    }
}

/// The local sink's half of security resolution: run the shared resolver, append
/// what it decided.
///
/// Creating a master is its own append rather than part of the trade's batch: the
/// master is harmless on its own (a security nobody holds is a row in a list), while
/// a trade that could not be posted must not take a security definition down with
/// it, because the next attempt would then have to create it again.
fn resolve_security_locally(
    store: &mut EventStore,
    user_id: &str,
    security: &ProviderSecurity,
    created: &mut u32,
) -> Result<MasterSecurity, ImportError> {
    let plaid_security_id = security.security_id.clone();
    let new = provider_security_as_new(security);
    // A slot rather than a return value, for the reason `sync::commands::investments`
    // uses one: what the resolver decided is decided *inside* the transaction, and
    // the head-mismatch retry runs the closure again.
    let outcome: std::cell::RefCell<Option<(MasterSecurity, bool)>> = std::cell::RefCell::new(None);
    run(store, user_id, |tx| {
        let resolved = build_resolve_security_in_txn(tx, &plaid_security_id, &new)?;
        *outcome.borrow_mut() = Some((resolved.master, resolved.created));
        Ok(resolved.step)
    })?;
    let (master, made) = outcome.into_inner().ok_or_else(|| {
        ImportError::Store("the security resolution landed without recording what it chose".into())
    })?;
    if made {
        *created += 1;
    }
    Ok(master)
}

/// The mapping row, when this copy already holds it.
///
/// The one lookup that stays outside the transaction, and only as a short cut: a
/// security already mapped needs no append and no round trip, and
/// [`build_resolve_security_in_txn`] re-checks under the lock anyway. On a replica a
/// miss here is not "no such mapping", only "not pulled yet", which is exactly what
/// makes asking the server the right next step rather than minting one locally.
fn mapped_security(conn: &Connection, plaid_security_id: &str) -> Option<MasterSecurity> {
    let security_id: String = conn
        .query_row(
            "SELECT security_id FROM plaid_securities WHERE plaid_security_id = ?1",
            [plaid_security_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()?;
    let kind = investment_commands::get_security(conn, &security_id)
        .map(|s| s.kind)
        .unwrap_or_default();
    Some(MasterSecurity { security_id, kind })
}

/// How the planner asks for our id for a provider security.
///
/// Two implementations, and neither does any work: [`Demand`] records the question
/// on a first pass so the sink can answer it — locally by appending, on a group's
/// books by asking the server — and [`Known`] answers it on the second. That is
/// what keeps the planner free of writes without the planner having to know which
/// securities will be needed: the code that asks is the code that decides, so the
/// set resolved is exactly the set a local import would have resolved, in the same
/// order.
trait SecurityIds {
    fn resolve(&mut self, security: &ProviderSecurity) -> MasterSecurity;
}

/// The first pass: collect, answer with nothing.
#[derive(Default)]
struct Demand {
    wanted: Vec<ProviderSecurity>,
    seen: std::collections::BTreeSet<String>,
}

impl SecurityIds for Demand {
    fn resolve(&mut self, security: &ProviderSecurity) -> MasterSecurity {
        if self.seen.insert(security.security_id.clone()) {
            self.wanted.push(security.clone());
        }
        MasterSecurity::default()
    }
}

/// The second pass: answer from what the sink resolved.
struct Known<'a>(&'a BTreeMap<String, MasterSecurity>);

impl SecurityIds for Known<'_> {
    fn resolve(&mut self, security: &ProviderSecurity) -> MasterSecurity {
        // A miss is unreachable — the demand pass asked the same questions in the
        // same order — and a security id of `""` is refused by every builder as
        // "no such security", so an unreachable miss holds the row rather than
        // posting a trade against nothing.
        self.0
            .get(&security.security_id)
            .cloned()
            .unwrap_or_default()
    }
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
    /// Fund distributions of a realized gain, posted since phase 5 gave them an
    /// account. Counted apart from dividends because they reach a different form.
    pub capital_gain_distributions: u32,
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
        self.bought
            + self.sold
            + self.dividends
            + self.interest
            + self.capital_gain_distributions
            + self.fees
            + self.cash_movements
    }
}

// ---------------------------------------------------------------------------
// The plan: one decision per provider transaction, and no writes
// ---------------------------------------------------------------------------

/// What the importer decided about one transaction.
#[derive(Debug, Clone)]
pub(crate) enum PlannedStep {
    /// Already imported, or already held.
    Duplicate,
    /// Inside a sheltered account, and ignored by design (spec §2b).
    IgnoreSheltered,
    /// For the review list.
    Hold {
        reason: HoldReason,
        detail: Option<String>,
    },
    Post {
        /// Boxed for the reason the big event payloads are: five command shapes
        /// inline would make every step of a payload as wide as the widest one, and
        /// most steps of a rolling re-fetch are a one-word `Duplicate`.
        write: Box<PlannedWrite>,
        record: ImportRecord,
    },
}

/// One transaction and what is to become of it.
pub(crate) struct Planned<'a> {
    txn: &'a ProviderInvestmentTransaction,
    step: PlannedStep,
}

/// Decide what happens to every transaction in a payload, writing nothing.
///
/// The whole of the importer's judgement, and the reason a hosted import is a
/// different sink rather than a different importer: this runs identically on a local
/// book and on a replica of a group's, and what comes out of it is a list of
/// decisions somebody else writes.
fn plan_payload<'a>(
    conn: &Connection,
    item_id: &str,
    transactions: &'a [ProviderInvestmentTransaction],
    ids: &mut impl SecurityIds,
) -> Vec<Planned<'a>> {
    transactions
        .iter()
        .map(|txn| Planned {
            txn,
            step: plan_one(conn, item_id, txn, ids),
        })
        .collect()
}

fn plan_one(
    conn: &Connection,
    item_id: &str,
    txn: &ProviderInvestmentTransaction,
    ids: &mut impl SecurityIds,
) -> PlannedStep {
    if already_seen(conn, &txn.investment_transaction_id) {
        return PlannedStep::Duplicate;
    }
    let Some(config) = get_config(conn, item_id, &txn.account_id) else {
        return PlannedStep::Hold {
            reason: HoldReason::Unconfigured,
            detail: None,
        };
    };
    match plan(config.treatment(), &txn.transaction_type, &txn.subtype) {
        // Not recorded anywhere, and that is deliberate. Ignoring is idempotent by
        // construction — the same trade ignored twice is still ignored — so a
        // register row would buy nothing, and a sheltered account's four hundred
        // yearly trades in a replicated log is exactly the noise spec §2b exists to
        // avoid.
        Plan::IgnoreSheltered => PlannedStep::IgnoreSheltered,
        Plan::Hold(reason) => PlannedStep::Hold {
            reason,
            detail: None,
        },
        Plan::Post(post) => match plan_post(txn, &config, post, ids) {
            Ok(write) => PlannedStep::Post {
                write: Box::new(write),
                record: ImportRecord {
                    provider_transaction_id: txn.investment_transaction_id.clone(),
                    item_id: item_id.to_string(),
                    plaid_account_id: txn.account_id.clone(),
                },
            },
            Err(ImportError::Held { reason, detail }) => PlannedStep::Hold { reason, detail },
            // A refusal the planner can see for itself — a cash movement of
            // nothing. Held exactly as one the books hand back under the write
            // lock, because to the person reading the list they are the same
            // finding.
            Err(other) => PlannedStep::Hold {
                reason: HoldReason::Refused,
                detail: Some(other.to_string()),
            },
        },
    }
}

/// Turn one importable transaction into the write it becomes.
///
/// Every float in the payload becomes an integer here, before anything else happens
/// with it, and a conversion that fails holds the row rather than posting an
/// approximation. No database and no network: the securities are already resolved
/// and the accounts come off the configuration, which is what lets the same function
/// decide a local import and a hosted one.
fn plan_post(
    txn: &ProviderInvestmentTransaction,
    config: &AccountConfig,
    post: PostAs,
    ids: &mut impl SecurityIds,
) -> Result<PlannedWrite, ImportError> {
    let Some(taxable) = config.taxable() else {
        // Unreachable by construction: `plan` never returns `Post` for a sheltered
        // account. Stated rather than unwrapped, because the cost of being wrong is
        // a trade posted into a sheltered account's single value-carried balance.
        return Err(ImportError::Held {
            reason: HoldReason::UnhandledType,
            detail: Some("a sheltered account has no accounts to post a trade to".to_string()),
        });
    };

    let Some(date) = NaiveDate::parse_from_str(txn.date.trim(), "%Y-%m-%d").ok() else {
        return Err(ImportError::Held {
            reason: HoldReason::BadAmount,
            detail: Some(format!("{:?} is not a date this can read", txn.date)),
        });
    };

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

    let memo = memo_for(txn);

    match post {
        PostAs::Buy => {
            let master = traded_security(txn, ids)?;
            // Which securities subaccount the position is carried in is read off
            // **our** master's kind rather than the provider's payload: a security
            // matched to an existing master by CUSIP is carried where that master
            // says it is, so a purchase, a later sale and the holdings report all
            // agree about which account the position lives in. Taking it from the
            // payload would let a provider that changed its mind about a security's
            // type split one holding across two accounts, and the sale of it would
            // then find no lots.
            let securities_account_id = taxable
                .securities_account_for_kind(&master.kind)
                .to_string();
            // `amount` and not `price * quantity + fees`. The provider's amount is
            // the cash that actually left the account, commission included, which
            // is both what the cash account has to be credited and — because a
            // purchase commission capitalises into basis — exactly the lot's cost.
            // Rebuilding it from price and quantity would re-round the same money
            // and leave the lot disagreeing with the bank.
            Ok(PlannedWrite::Buy {
                lot_id: Uuid::new_v4().to_string(),
                cmd: BuySecurityCommand {
                    security_id: master.security_id,
                    securities_account_id,
                    cash_account_id: taxable.cash_account_id.clone(),
                    quantity,
                    total_cost_cents: amount_cents.abs(),
                    trade_date: date,
                    memo: Some(memo),
                },
            })
        }
        PostAs::Sell => {
            let master = traded_security(txn, ids)?;
            // The same slot the purchase used, by the same rule: lots are keyed by
            // `(security, securities account)`, so a sale looking in another
            // account finds no lots at all and is refused for want of a basis.
            let securities_account_id = taxable
                .securities_account_for_kind(&master.kind)
                .to_string();
            // The provider's amount on a sale is the **net** credited to cash. A
            // 1099-B reports proceeds gross with the fee shown separately, and
            // phase 1's command takes them that way and posts the difference — so
            // the gross is reconstructed as net + fee. Posting the net as the gross
            // would understate proceeds on every reconciliation against the form,
            // which is the one comparison spec §8 says the ledger exists to make.
            Ok(PlannedWrite::Sell {
                sale_id: Uuid::new_v4().to_string(),
                cmd: SellSecurityCommand {
                    security_id: master.security_id,
                    securities_account_id,
                    cash_account_id: taxable.cash_account_id.clone(),
                    realized_gain_account_id: taxable.realized_gain_account_id.clone(),
                    quantity,
                    proceeds_cents: amount_cents.abs() + fee_cents,
                    fee_cents,
                    trade_date: date,
                    // FIFO, which is both spec §4's default and what the IRS assumes
                    // when a seller specifies nothing. A specific-lot choice is a
                    // decision made at the point of sale by a person; an importer
                    // reading a month-old trade cannot make it, and guessing would
                    // put a basis on a filed return that nobody chose.
                    selection: LotSelection::Fifo,
                    memo: Some(memo),
                },
            })
        }
        PostAs::Income(kind) => {
            // Sweep interest belongs to the account and to no holding, which is why
            // phase 1 made the security optional on income.
            let security_id = txn
                .security
                .as_ref()
                .map(|security| ids.resolve(security).security_id);
            // One lookup on the configuration rather than a match here, so that the
            // rule about which account each kind of income posts to lives in one
            // place — beside the fields it reads. The `None` is the capital gain
            // distribution with no account configured, which is held rather than
            // posted to a guess.
            let Some(income_account_id) = taxable.income_account_for(kind).map(str::to_string)
            else {
                return Err(ImportError::Held {
                    reason: HoldReason::NoIncomeAccount,
                    detail: None,
                });
            };
            Ok(PlannedWrite::Income {
                cmd: RecordInvestmentIncomeCommand {
                    kind,
                    security_id,
                    cash_account_id: taxable.cash_account_id.clone(),
                    income_account_id,
                    // Income arrives as a credit to cash, so the provider's amount
                    // is negative. The magnitude is the income.
                    amount_cents: amount_cents.abs(),
                    received_on: date,
                    memo: Some(memo),
                },
            })
        }
        PostAs::Fee => {
            let security_id = txn
                .security
                .as_ref()
                .map(|security| ids.resolve(security).security_id);
            Ok(PlannedWrite::Fee {
                cmd: ChargeInvestmentFeeCommand {
                    cash_account_id: taxable.cash_account_id.clone(),
                    expense_account_id: taxable.fee_expense_account_id.clone(),
                    amount_cents: amount_cents.abs(),
                    charged_on: date,
                    security_id,
                    memo: Some(memo),
                },
            })
        }
        PostAs::Cash => {
            let Some(clearing_account_id) = taxable.transfer_clearing_account_id.clone() else {
                return Err(ImportError::Held {
                    reason: HoldReason::NoClearingAccount,
                    detail: None,
                });
            };
            // A transfer, so the sign is the provider's own: positive amount means
            // cash left the brokerage, negative means it arrived.
            let into_brokerage_cents = -amount_cents;
            if into_brokerage_cents == 0 {
                return Err(ImportError::Refused(NOTHING_MOVED.to_string()));
            }
            Ok(PlannedWrite::Cash {
                cmd: ImportedCashMovement {
                    cash_account_id: taxable.cash_account_id.clone(),
                    clearing_account_id,
                    into_brokerage_cents,
                    on: date,
                    memo,
                },
            })
        }
    }
}

/// The security a trade is a trade of, or a held row.
fn traded_security(
    txn: &ProviderInvestmentTransaction,
    ids: &mut impl SecurityIds,
) -> Result<MasterSecurity, ImportError> {
    let Some(security) = txn.security.as_ref() else {
        return Err(ImportError::Held {
            reason: HoldReason::UnknownSecurity,
            detail: None,
        });
    };
    Ok(ids.resolve(security))
}

/// The securities the plan will ask for, in the order it asks.
///
/// The plan is built and thrown away: what is wanted is not the plan but the
/// questions it asked, and asking them with the real planner is what guarantees the
/// set resolved is exactly the set a local import resolves. A payload of four
/// hundred sheltered trades asks for none of them, and a transaction whose amount
/// will not convert asks for none either — both are decided before a security is
/// ever needed.
fn demanded_securities(
    conn: &Connection,
    item_id: &str,
    transactions: &[ProviderInvestmentTransaction],
) -> Vec<ProviderSecurity> {
    let mut demand = Demand::default();
    let _ = plan_payload(conn, item_id, transactions, &mut demand);
    demand.wanted
}

/// The flags, per account, for one payload.
fn payload_flags(
    conn: &Connection,
    item_id: &str,
    accounts: &[ProviderAccount],
) -> Vec<SubtypeFlag> {
    accounts
        .iter()
        .filter_map(|account| {
            let config = get_config(conn, item_id, &account.account_id);
            flag_for(&account.account_id, account.subtype.as_deref(), &config)
        })
        .collect()
}

/// Apply everything about one planned step that is the same on both paths, and hand
/// back the ledger write when there is one.
///
/// A duplicate counted, a sheltered trade ignored, a row written to the local review
/// list: none of those touch the log, so none of them differ between a local book
/// and a group's. What is handed back is the one thing that does.
fn apply_step(
    conn: &Connection,
    item_id: &str,
    txn: &ProviderInvestmentTransaction,
    step: PlannedStep,
    report: &mut ImportReport,
) -> Result<Option<(Box<PlannedWrite>, ImportRecord)>, ImportError> {
    match step {
        PlannedStep::Duplicate => {
            report.duplicates += 1;
            Ok(None)
        }
        PlannedStep::IgnoreSheltered => {
            report.ignored_sheltered += 1;
            Ok(None)
        }
        PlannedStep::Hold { reason, detail } => {
            hold(conn, item_id, txn, reason, detail.as_deref())?;
            report.held += 1;
            Ok(None)
        }
        PlannedStep::Post { write, record } => Ok(Some((write, record))),
    }
}

/// Record what became of one write.
///
/// A refusal is a finding, not a failed run: the rest of the payload still has to
/// land, and this row has to be visible with the reason attached. Anything else is a
/// broken database, a broken log or a transport that is down, and carrying on through
/// one of those would write more of whatever is wrong.
fn record_outcome(
    conn: &Connection,
    item_id: &str,
    txn: &ProviderInvestmentTransaction,
    kind: ImportedActivityKind,
    outcome: Result<(), ImportError>,
    report: &mut ImportReport,
) -> Result<(), ImportError> {
    match outcome {
        Ok(()) => {
            count(report, kind);
            Ok(())
        }
        Err(ImportError::Refused(message)) => {
            hold(conn, item_id, txn, HoldReason::Refused, Some(&message))?;
            report.held += 1;
            Ok(())
        }
        Err(ImportError::Held { reason, detail }) => {
            hold(conn, item_id, txn, reason, detail.as_deref())?;
            report.held += 1;
            Ok(())
        }
        Err(other) => Err(other),
    }
}

fn count(report: &mut ImportReport, kind: ImportedActivityKind) {
    match kind {
        ImportedActivityKind::Buy => report.bought += 1,
        ImportedActivityKind::Sell => report.sold += 1,
        ImportedActivityKind::Dividend => report.dividends += 1,
        ImportedActivityKind::Interest => report.interest += 1,
        ImportedActivityKind::CapitalGainDistribution => report.capital_gain_distributions += 1,
        ImportedActivityKind::Fee => report.fees += 1,
        ImportedActivityKind::Cash => report.cash_movements += 1,
    }
}

/// Import investment transactions for one connection, into local books.
///
/// `accounts` is the payload's account list, used only for the subtype flags: what
/// an account is imported *as* comes from the configuration register, never from
/// the subtype, because the subtype is the provider's opinion and the configuration
/// is the book's decision.
///
/// See [`import_transactions_hosted`] for the same import into a group's books. The
/// two share the planner and differ only in where a write lands.
pub fn import_transactions(
    store: &mut EventStore,
    user_id: &str,
    item_id: &str,
    accounts: &[ProviderAccount],
    transactions: &[ProviderInvestmentTransaction],
) -> Result<ImportReport, ImportError> {
    let mut report = ImportReport {
        // The flags first, per account, so that a payload whose every transaction is
        // a duplicate still reports an account nobody has confirmed the kind of.
        flags: payload_flags(store.connection(), item_id, accounts),
        ..Default::default()
    };

    let mut known = BTreeMap::new();
    for security in demanded_securities(store.connection(), item_id, transactions) {
        let master = match mapped_security(store.connection(), &security.security_id) {
            Some(master) => master,
            None => resolve_security_locally(
                store,
                user_id,
                &security,
                &mut report.securities_created,
            )?,
        };
        known.insert(security.security_id.clone(), master);
    }

    for planned in plan_payload(
        store.connection(),
        item_id,
        transactions,
        &mut Known(&known),
    ) {
        let Planned { txn, step } = planned;
        let Some((write, record)) =
            apply_step(store.connection(), item_id, txn, step, &mut report)?
        else {
            continue;
        };
        let outcome = run(store, user_id, |tx| {
            build_import_in_txn(tx, &write, &record)
        })
        .map(|_| ());
        record_outcome(
            store.connection(),
            item_id,
            txn,
            write.outcome(),
            outcome,
            &mut report,
        )?;
    }

    Ok(report)
}

/// The same import, into a group's books.
///
/// Every ledger write goes through the group server; not one event is appended
/// locally, which is why this takes the store immutably. A replica's log has exactly
/// one writer — the mirror path in [`crate::sync::replica`] — and an import that
/// appended beside it would fork it.
///
/// # What is written where
///
/// The **review list** and the **fetch window** are written locally in both modes:
/// what this machine is still looking at and how far it has fetched are facts about
/// this machine, not about the books (migration 050). Everything else — the entry,
/// the lot, the security master, the provider-security mapping and the import
/// register — is appended by the server, in the same batches a local import would
/// have appended them in, and reaches this copy through the ordinary pull.
///
/// # The ordering, and what an interruption leaves behind
///
/// One submit per transaction, and the import record is part of that submit rather
/// than a step after it: [`build_import_in_txn`] runs inside the server's append
/// transaction and puts the `InvestmentActivityImported` in the same batch as the
/// entry. So there is no window in which a trade is posted and unfenced.
///
/// * **Interrupted before the server accepted** — nothing is recorded anywhere. The
///   next run re-plans the transaction and posts it once.
/// * **Interrupted after the server accepted** — the posting *and* its fence are in
///   the group's log, durably. This copy's register does not know yet; it learns at
///   the next pull, and nothing needs to be repaired. A re-import that beats the pull
///   is refused by the server's own fence under its write lock, and that refusal is
///   counted as the duplicate it is rather than held for review — see
///   [`already_imported_message`].
///
/// Nothing is echoed into the local projections to close that window early, and that
/// is the point: a row written into a replica's projections beside the mirror path is
/// a second writer, and the whole of [`crate::sync::replica`] exists to have only
/// one.
///
/// # Refusals
///
/// A `409` is ordinary — another member's write moved the head — and
/// [`SyncClient`](crate::sync::SyncClient) retries it. A `422` is the books refusing
/// this transaction, and the row is held carrying the server's own wording, exactly
/// as a local refusal is. Anything else (no token, a server too old, a transport that
/// is down) fails the run rather than holding four hundred rows as refused.
pub async fn import_transactions_hosted(
    store: &EventStore,
    client: &mut crate::sync::SyncClient,
    item_id: &str,
    accounts: &[ProviderAccount],
    transactions: &[ProviderInvestmentTransaction],
) -> Result<ImportReport, ImportError> {
    let conn = store.connection();
    let mut report = ImportReport {
        flags: payload_flags(conn, item_id, accounts),
        ..Default::default()
    };

    let mut known = BTreeMap::new();
    for security in demanded_securities(conn, item_id, transactions) {
        let master = match mapped_security(conn, &security.security_id) {
            Some(master) => master,
            None => {
                // The server mints it, and the id it chose is the id used from here
                // on. A locally invented id would be a second master for the same
                // holding the moment the group's log came back with the real one.
                let resolved = client
                    .resolve_plaid_security(
                        &security.security_id,
                        &provider_security_as_new(&security),
                    )
                    .await
                    .map_err(from_sync_error)?;
                if resolved.created {
                    report.securities_created += 1;
                }
                resolved.master
            }
        };
        known.insert(security.security_id.clone(), master);
    }

    for planned in plan_payload(conn, item_id, transactions, &mut Known(&known)) {
        let Planned { txn, step } = planned;
        let Some((write, record)) = apply_step(conn, item_id, txn, step, &mut report)? else {
            continue;
        };
        let outcome = client
            .import_investment_activity(&write, &record)
            .await
            .map(|_| ())
            .map_err(from_sync_error);
        // The one refusal that is not a finding: this copy's register had not caught
        // up, so the server was asked to import something it already holds. Counted
        // where the planner would have counted it.
        if let Err(ImportError::Refused(message)) = &outcome {
            if *message == already_imported_message(&record.provider_transaction_id) {
                report.duplicates += 1;
                continue;
            }
        }
        record_outcome(conn, item_id, txn, write.outcome(), outcome, &mut report)?;
    }

    Ok(report)
}

/// What the group server said, as this module's error.
///
/// A domain refusal becomes [`ImportError::Refused`], which holds the row with the
/// server's own wording. Everything else becomes [`ImportError::Store`], which fails
/// the run: a transport that is down has not refused anything, and recording four
/// hundred rows as "the books refused this" would be a lie somebody has to undo by
/// hand.
fn from_sync_error(e: crate::sync::client::SyncClientError) -> ImportError {
    match e {
        crate::sync::client::SyncClientError::Rejected(why) => ImportError::Refused(why),
        other => ImportError::Store(other.to_string()),
    }
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
    /// `pending`, `resolved` or `dismissed`.
    pub status: String,
    /// How it was resolved, as [`Resolution::as_str`] writes it. `None` while the
    /// row is pending.
    pub resolution: Option<String>,
    /// What the person said they did about it. Required on every transition, which
    /// is the point: a row that left this list without an entry to point at is
    /// explained by nothing else.
    pub resolution_note: Option<String>,
    /// The journal entry a resolution posted, when it posted one.
    pub resolution_entry_id: Option<String>,
    pub resolved_at: Option<String>,
}

impl StagedActivity {
    /// The hold reason as an enum, when it is one this build knows.
    ///
    /// `None` for a row written by a later build with a reason this one has never
    /// heard of — which is a row to display, not a row to hide, so the reason
    /// string is carried beside this.
    pub fn hold_reason(&self) -> Option<HoldReason> {
        HoldReason::ALL
            .into_iter()
            .find(|r| r.as_str() == self.reason)
    }

    pub fn is_pending(&self) -> bool {
        self.status == PENDING
    }

    /// Whether a contribution or a distribution is the question this row asks.
    ///
    /// Cash moving into or out of a sheltered account is the only held row where
    /// recording it *is* the resolution — every other kind is entered by hand,
    /// because what it should be is not one of two answers.
    pub fn is_sheltered_cash(&self) -> bool {
        self.hold_reason() == Some(HoldReason::ShelteredCash)
    }
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

/// The three statuses a held row can be in.
///
/// Strings rather than an enum in the database, as the bank feed's staging table
/// has it; the constants are here so that a typo is a compile error at the one
/// place each is written.
pub const PENDING: &str = "pending";
pub const RESOLVED: &str = "resolved";
pub const DISMISSED: &str = "dismissed";

const STAGED_COLUMNS: &str = "id, item_id, plaid_account_id, provider_transaction_id, reason,
     detail, provider_type, provider_subtype, date, name, amount_cents, raw_payload, status,
     resolution, resolution_note, resolution_entry_id, resolved_at";

fn read_staged(r: &rusqlite::Row<'_>) -> rusqlite::Result<StagedActivity> {
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
        resolution: r.get(13)?,
        resolution_note: r.get(14)?,
        resolution_entry_id: r.get(15)?,
        resolved_at: r.get(16)?,
    })
}

/// Everything still waiting for a person, oldest activity first.
pub fn pending_activity(conn: &Connection) -> Vec<StagedActivity> {
    activity_with_status(conn, PENDING)
}

/// Every held row in one status, oldest activity first.
///
/// What has been resolved and what has been dismissed are both worth being able to
/// read back: the first answers "where did that transaction go", and the second is
/// the only record that somebody decided a transaction did not belong in these
/// books at all.
pub fn activity_with_status(conn: &Connection, status: &str) -> Vec<StagedActivity> {
    let sql = format!(
        "SELECT {STAGED_COLUMNS} FROM investment_staged_activity
          WHERE status = ?1 ORDER BY date, rowid"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([status], read_staged);
    match rows {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// One held row, whatever status it is in.
pub fn get_activity(conn: &Connection, staged_id: &str) -> Option<StagedActivity> {
    let sql = format!("SELECT {STAGED_COLUMNS} FROM investment_staged_activity WHERE id = ?1");
    conn.query_row(&sql, [staged_id], read_staged)
        .optional()
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------
// Resolving a held row
// ---------------------------------------------------------------------------

/// What was done about a held row.
///
/// **A dismissal is not a kind of resolution**, and the distinction is the whole
/// reason this enum exists rather than a boolean "dealt with". A resolution says the
/// activity reached the books some other way; a dismissal says it never will, on
/// purpose. Collapsing the two would make "is anything missing from these books?"
/// unanswerable — which is the one question a review list is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Entered by hand. The note says what was done; a corporate action is the
    /// commonest case, and spec §7 is explicit that this program must not guess one.
    ByHand,
    /// Recorded as a contribution into a sheltered account.
    Contribution,
    /// Recorded as a distribution out of a sheltered account.
    Distribution,
    /// It does not belong in these books.
    Dismissed,
}

impl Resolution {
    pub fn as_str(&self) -> &'static str {
        match self {
            Resolution::ByHand => "by_hand",
            Resolution::Contribution => "contribution",
            Resolution::Distribution => "distribution",
            Resolution::Dismissed => "dismissed",
        }
    }

    /// The status a row lands in. Everything but a dismissal is `resolved`.
    pub fn status(&self) -> &'static str {
        match self {
            Resolution::Dismissed => DISMISSED,
            _ => RESOLVED,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Resolution::ByHand => "Entered by hand",
            Resolution::Contribution => "Recorded as a contribution",
            Resolution::Distribution => "Recorded as a distribution",
            Resolution::Dismissed => "Dismissed",
        }
    }
}

/// The idempotency key a resolution's entry carries.
///
/// Keyed on the provider's transaction id, which is what phase 4 left the
/// `reference` on a contribution and a distribution for. It is what makes the
/// two-step shape below safe: posting the entry and flipping the row's status are
/// separate writes — the status lives in a machine-local table and the entry in the
/// replicated log — so a crash between them leaves the row pending with the entry
/// already posted. Pressing the button again then finds the reference taken and is
/// refused, rather than posting the same contribution twice.
pub fn resolution_reference(provider_transaction_id: &str) -> String {
    format!("investment-activity-{provider_transaction_id}")
}

/// Mark a held row as entered by hand.
///
/// Posts **nothing**, and that is the point: what a corporate action should post is
/// a judgement about basis that this program will not make (spec §7), so the person
/// makes the entry themselves and this records that they did. `entry_id` is
/// optional because the entry may be one of several, or may be a correction
/// somewhere else entirely; the note is not optional, because without it the row
/// leaves the list explained by nothing.
pub fn resolve_by_hand(
    conn: &Connection,
    staged_id: &str,
    note: &str,
    entry_id: Option<&str>,
) -> Result<(), ImportError> {
    settle(conn, staged_id, Resolution::ByHand, note, entry_id)
}

/// Mark a held row as not belonging in these books.
///
/// A note is required here as well, and here it matters most: the provider will not
/// offer this transaction again — the dedup fence sees the row whatever status it is
/// in — so the note is the only surviving record of why a transaction that really
/// happened is not in the books.
pub fn dismiss(conn: &Connection, staged_id: &str, note: &str) -> Result<(), ImportError> {
    settle(conn, staged_id, Resolution::Dismissed, note, None)
}

/// Put a dismissed row back in the review list.
///
/// **Only a dismissed one.** A resolution posted an entry, or recorded that a
/// person posted one, and reopening it would invite the same transaction being
/// posted a second time. A dismissal posted nothing, so putting it back costs
/// nothing — and a row dismissed by mistake has nowhere else to come back from,
/// because the provider will not hand it over again.
pub fn reopen(conn: &Connection, staged_id: &str) -> Result<(), ImportError> {
    let row = get_activity(conn, staged_id)
        .ok_or_else(|| ImportError::NoSuchStagedActivity(staged_id.to_string()))?;
    if row.status != DISMISSED {
        return Err(ImportError::NotDismissed { status: row.status });
    }
    conn.execute(
        "UPDATE investment_staged_activity
            SET status = ?2, resolution = NULL, resolution_note = NULL,
                resolution_entry_id = NULL, resolved_at = NULL
          WHERE id = ?1",
        rusqlite::params![staged_id, PENDING],
    )?;
    Ok(())
}

/// Recording cash into a sheltered account, and resolving the row that asked about
/// it.
///
/// The retirement account is **not** a parameter: it comes from the configuration
/// of the provider account the activity arrived in. A caller that could name it
/// could name the wrong one, and a contribution into somebody else's IRA balances
/// perfectly.
#[derive(Debug, Clone)]
pub struct ResolveAsContributionCommand {
    pub staged_id: String,
    /// The bank account the money came from.
    pub funding_account_id: String,
    pub amount_cents: i64,
    pub on: NaiveDate,
    pub memo: Option<String>,
    /// What the person says they did. Recorded on the row, as on every transition.
    pub note: String,
}

/// Recording cash out of a sheltered account, and resolving the row.
#[derive(Debug, Clone)]
pub struct ResolveAsDistributionCommand {
    pub staged_id: String,
    /// Where the net lands.
    pub receiving_account_id: String,
    /// Box 1 of the 1099-R: everything that left the account.
    pub gross_cents: i64,
    /// Box 4: what the payer withheld.
    pub withheld_cents: i64,
    /// The prepaid-tax asset account the withholding becomes. Required whenever
    /// anything was withheld — phase 2 refuses to expense it, because it is money
    /// already paid toward a bill that is not settled yet.
    pub withheld_account_id: String,
    /// Box 2a, when the person knows better than the account's kind does.
    pub taxable_cents: Option<i64>,
    pub on: NaiveDate,
    pub memo: Option<String>,
    pub note: String,
}

/// What a resolution has to know before it posts anything.
///
/// The read-only half of [`resolve_as_contribution`] and [`resolve_as_distribution`],
/// split out so the **hosted** path can take it without taking the posting with it.
/// On a group's books the entry is appended by the server and the row's status is
/// flipped here, and the caller needs the account and the reference before it can
/// build the command it sends. Nothing in here is a guess: the account comes from
/// the provider account's configuration, exactly as the local path takes it, so a
/// hosted contribution cannot land in a different retirement account than a local
/// one would.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShelteredResolution {
    /// The ledger account carried at value, from the provider account's config.
    pub retirement_account_id: String,
    /// The idempotency key the posted entry must carry. See
    /// [`resolution_reference`] for why it is what makes the two-step shape safe.
    pub reference: String,
}

/// Read what a sheltered-cash resolution needs, and refuse a row that is not one.
///
/// Runs the same three checks the local path runs — the row exists, it is still
/// pending, and it is held because cash moved in or out of a sheltered account —
/// and resolves the retirement account from the configuration rather than from a
/// caller's argument, for the reason [`ResolveAsContributionCommand`] gives.
pub fn prepare_sheltered_resolution(
    conn: &Connection,
    staged_id: &str,
) -> Result<ShelteredResolution, ImportError> {
    let row = pending_sheltered_row(conn, staged_id)?;
    let retirement_account_id = sheltered_account_for(conn, &row)?;
    Ok(ShelteredResolution {
        retirement_account_id,
        reference: resolution_reference(&row.provider_transaction_id),
    })
}

/// Move a row off `pending` for a resolution whose entry has **already** been
/// posted somewhere this function cannot see.
///
/// The second half of a hosted resolution: the group server appended the entry, and
/// the status transition is machine-local bookkeeping in a table no replica mirrors
/// (migration 050). Exposed rather than inlined at the call site so the guarded
/// `UPDATE` in [`settle`] — status-checked, so two presses cannot both take effect —
/// is the only thing that ever writes that column.
///
/// **Order matters, and this is the second step.** See [`resolution_reference`]:
/// posting first and settling second means a crash in between leaves the row
/// pending with the entry already in the books, and pressing the button again is
/// refused by the reference. Settling first would leave a resolved row with nothing
/// posted, which nothing can detect and the provider will never offer again.
pub fn settle_resolution(
    conn: &Connection,
    staged_id: &str,
    resolution: Resolution,
    note: &str,
    entry_id: Option<&str>,
) -> Result<(), ImportError> {
    settle(conn, staged_id, resolution, note, entry_id)
}

/// Record a held sheltered-cash row as a contribution. Returns the entry posted.
pub fn resolve_as_contribution(
    store: &mut EventStore,
    user_id: &str,
    cmd: &ResolveAsContributionCommand,
) -> Result<String, ImportError> {
    let prepared = prepare_sheltered_resolution(store.connection(), &cmd.staged_id)?;
    let entry_id = retirement_commands::record_contribution(
        store,
        user_id,
        &retirement_commands::RetirementContributionCommand {
            account_id: prepared.retirement_account_id,
            funding_account_id: cmd.funding_account_id.clone(),
            amount_cents: cmd.amount_cents,
            on: cmd.on,
            memo: cmd.memo.clone(),
            reference: Some(prepared.reference),
        },
    )
    .map_err(|e| ImportError::Refused(e.to_string()))?;
    settle(
        store.connection(),
        &cmd.staged_id,
        Resolution::Contribution,
        &cmd.note,
        Some(&entry_id),
    )?;
    Ok(entry_id)
}

/// Record a held sheltered-cash row as a distribution. Returns what it came to —
/// including box 2a, which is what a 1099-R will be built from.
pub fn resolve_as_distribution(
    store: &mut EventStore,
    user_id: &str,
    cmd: &ResolveAsDistributionCommand,
) -> Result<retirement_commands::Distributed, ImportError> {
    let prepared = prepare_sheltered_resolution(store.connection(), &cmd.staged_id)?;
    let distributed = retirement_commands::record_distribution(
        store,
        user_id,
        &retirement_commands::RetirementDistributionCommand {
            account_id: prepared.retirement_account_id,
            receiving_account_id: cmd.receiving_account_id.clone(),
            gross_cents: cmd.gross_cents,
            withheld_cents: cmd.withheld_cents,
            withheld_account_id: cmd.withheld_account_id.clone(),
            // Always none, and phase 2 refuses it when it is not: a sheltered
            // account is carried at value, so crediting income at distribution
            // would recognise the same dollar twice.
            taxable_income_account_id: None,
            taxable_cents: cmd.taxable_cents,
            on: cmd.on,
            memo: cmd.memo.clone(),
            reference: Some(prepared.reference),
        },
    )
    .map_err(|e| ImportError::Refused(e.to_string()))?;
    settle(
        store.connection(),
        &cmd.staged_id,
        Resolution::Distribution,
        &cmd.note,
        Some(&distributed.entry_id),
    )?;
    Ok(distributed)
}

/// The row, if it is there, is pending, and is the kind of row a contribution or a
/// distribution is an answer to.
fn pending_sheltered_row(
    conn: &Connection,
    staged_id: &str,
) -> Result<StagedActivity, ImportError> {
    let row = get_activity(conn, staged_id)
        .ok_or_else(|| ImportError::NoSuchStagedActivity(staged_id.to_string()))?;
    if !row.is_pending() {
        return Err(ImportError::NotPending { status: row.status });
    }
    if !row.is_sheltered_cash() {
        return Err(ImportError::Invalid(format!(
            "that row is held because {}, not because cash moved in or out of a sheltered \
             account. A contribution or a distribution is not the answer to it — enter it by \
             hand.",
            row.reason
        )));
    }
    Ok(row)
}

/// The sheltered ledger account the activity's provider account is configured with.
fn sheltered_account_for(conn: &Connection, row: &StagedActivity) -> Result<String, ImportError> {
    let config = get_config(conn, &row.item_id, &row.plaid_account_id).ok_or_else(|| {
        ImportError::Invalid(format!(
            "provider account {} is not configured, so there is no retirement account to record \
             this against. Configure it first.",
            row.plaid_account_id
        ))
    })?;
    config.sheltered().map(str::to_string).ok_or_else(|| {
        ImportError::Invalid(format!(
            "provider account {} is configured as a taxable brokerage. A contribution and a \
             distribution are movements in or out of a sheltered account; this one's cash \
             movements post against its clearing account.",
            row.plaid_account_id
        ))
    })
}

/// The one write that moves a row off `pending`.
///
/// Guarded on the status **in the UPDATE**, so two presses of the same button
/// cannot both take effect: the second changes no rows and is refused with what the
/// row has become. That is not the only fence — a resolution that posts an entry
/// carries an idempotency reference as well (see [`resolution_reference`]) — and it
/// is the cheap one that catches the ordinary double click.
fn settle(
    conn: &Connection,
    staged_id: &str,
    resolution: Resolution,
    note: &str,
    entry_id: Option<&str>,
) -> Result<(), ImportError> {
    let note = note.trim();
    if note.is_empty() {
        return Err(ImportError::Invalid(
            "say what was done about it. A row that leaves the review list without a note is \
             explained by nothing, and the provider will not offer the transaction again."
                .to_string(),
        ));
    }
    let changed = conn.execute(
        "UPDATE investment_staged_activity
            SET status = ?2, resolution = ?3, resolution_note = ?4, resolution_entry_id = ?5,
                resolved_at = datetime('now')
          WHERE id = ?1 AND status = ?6",
        rusqlite::params![
            staged_id,
            resolution.status(),
            resolution.as_str(),
            note,
            entry_id,
            PENDING,
        ],
    )?;
    if changed == 0 {
        return match get_activity(conn, staged_id) {
            Some(row) => Err(ImportError::NotPending { status: row.status }),
            None => Err(ImportError::NoSuchStagedActivity(staged_id.to_string())),
        };
    }
    Ok(())
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

/// What one account's holdings import will write.
pub(crate) struct PlannedHoldings {
    plaid_account_id: String,
    snapshot: HoldingsSnapshotData,
    /// The ledger account carried at value, when this is a sheltered account, and
    /// `None` for a taxable one — which posts nothing from a snapshot at all
    /// (spec §3 and §5).
    sheltered_account_id: Option<String>,
}

/// Group a holdings payload by account and decide what each account's snapshot is.
///
/// Writes nothing, so it runs the same on a local book and on a replica of a group's.
fn plan_holdings(
    conn: &Connection,
    item_id: &str,
    as_of: NaiveDate,
    holdings: &[ProviderHolding],
    accounts: &[ProviderAccount],
    ids: &mut impl SecurityIds,
) -> Result<(Vec<String>, Vec<PlannedHoldings>), ImportError> {
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

    let mut skipped = Vec::new();
    let mut planned = Vec::new();
    for (plaid_account_id, account_holdings) in by_account {
        let Some(config) = get_config(conn, item_id, plaid_account_id) else {
            // Skipped rather than held, and the difference from a transaction is that
            // a holdings read is not destructive: the same snapshot can simply be
            // taken again once the account is configured, whereas a transaction the
            // provider has handed over once is not offered again.
            skipped.push(plaid_account_id.to_string());
            continue;
        };

        let mut lines = Vec::with_capacity(account_holdings.len());
        for holding in &account_holdings {
            // A sheltered account's holdings never reach the security master:
            // nothing inside one is recorded (spec §2b), so a master for every fund
            // a 401(k) has ever held would be a list nothing reads and nothing keeps
            // honest.
            let security_id = match (config.treatment(), holding.security.as_ref()) {
                (InvestmentTreatment::Taxable, Some(security)) => {
                    Some(ids.resolve(security).security_id)
                }
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

        planned.push(PlannedHoldings {
            plaid_account_id: plaid_account_id.to_string(),
            snapshot: HoldingsSnapshotData {
                snapshot_id: Uuid::new_v4().to_string(),
                item_id: item_id.to_string(),
                plaid_account_id: plaid_account_id.to_string(),
                as_of,
                holdings: lines,
            },
            sheltered_account_id: config.sheltered().map(str::to_string),
        });
    }
    Ok((skipped, planned))
}

/// The securities a holdings payload will ask for, in the order it asks.
fn demanded_holdings_securities(
    conn: &Connection,
    item_id: &str,
    as_of: NaiveDate,
    holdings: &[ProviderHolding],
    accounts: &[ProviderAccount],
) -> Result<Vec<ProviderSecurity>, ImportError> {
    let mut demand = Demand::default();
    plan_holdings(conn, item_id, as_of, holdings, accounts, &mut demand)?;
    Ok(demand.wanted)
}

/// Record what the broker said, if it is not already on file.
///
/// The comparison is inside the transaction rather than before it, so re-importing
/// the same payload appends literally nothing on either path — and so two members
/// reading the same holdings at the same moment cannot both record them. An unchanged
/// snapshot appends no events at all, which is how a caller tells the two apart.
pub(crate) fn build_snapshot_in_txn(
    tx: &rusqlite::Transaction<'_>,
    snapshot: &HoldingsSnapshotData,
) -> Result<ImportStep, EventStoreError> {
    if snapshot_unchanged(tx, snapshot) {
        return Ok(ImportStep::Append(Vec::new()));
    }
    Ok(ImportStep::Append(vec![Event::HoldingsSnapshotRecorded(
        Box::new(snapshot.clone()),
    )]))
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

    let mut known = BTreeMap::new();
    for security in
        demanded_holdings_securities(store.connection(), item_id, as_of, holdings, accounts)?
    {
        let master = match mapped_security(store.connection(), &security.security_id) {
            Some(master) => master,
            None => resolve_security_locally(
                store,
                user_id,
                &security,
                &mut report.securities_created,
            )?,
        };
        known.insert(security.security_id.clone(), master);
    }

    let (skipped, planned) = plan_holdings(
        store.connection(),
        item_id,
        as_of,
        holdings,
        accounts,
        &mut Known(&known),
    )?;
    report.skipped_unconfigured = skipped;

    for plan in planned {
        let snapshot = plan.snapshot.clone();
        let recorded = !run(store, user_id, move |tx| {
            build_snapshot_in_txn(tx, &snapshot)
        })?
        .is_empty();
        if recorded {
            report.recorded += 1;
        } else {
            report.unchanged += 1;
        }

        match &plan.sheltered_account_id {
            Some(account_id) => match plan.snapshot.total_value_cents() {
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
                        .push((plan.plaid_account_id.clone(), value));
                }
                // A total missing one holding is not a total, and setting a sheltered
                // account's value from one would post a fictional loss.
                None => report.incomplete_values.push(plan.plaid_account_id.clone()),
            },
            None => {
                if let Some(reconciliation) =
                    reconcile(store.connection(), item_id, &plan.plaid_account_id)
                {
                    report.reconciliations.push(reconciliation);
                }
            }
        }
    }

    Ok(report)
}

/// The same holdings import, into a group's books.
///
/// The snapshot goes through the server for the reason the trades do: the
/// reconciliation (spec §7) and the market-value report both read it, and on a
/// group's books every member has to be looking at the same statement. A snapshot
/// appended locally would fork the replica's log, and a snapshot only this machine
/// held would make one member's reconciliation disagree with another's about what the
/// broker said.
///
/// # What this does not do
///
/// It computes **no reconciliations**. Both halves of that comparison — the lots the
/// books hold and the snapshot the broker sent — have just been written to the
/// group's log and are not in this copy until the next pull, so a reconciliation run
/// here would report every position the import just posted as missing from the books.
/// The caller runs [`reconcile`] once the replica has caught up, which is the same
/// place every other hosted figure comes from.
pub async fn import_holdings_hosted(
    store: &EventStore,
    client: &mut crate::sync::SyncClient,
    item_id: &str,
    as_of: NaiveDate,
    holdings: &[ProviderHolding],
    accounts: &[ProviderAccount],
) -> Result<HoldingsReport, ImportError> {
    let conn = store.connection();
    let mut report = HoldingsReport::default();

    let mut known = BTreeMap::new();
    for security in demanded_holdings_securities(conn, item_id, as_of, holdings, accounts)? {
        let master = match mapped_security(conn, &security.security_id) {
            Some(master) => master,
            None => {
                let resolved = client
                    .resolve_plaid_security(
                        &security.security_id,
                        &provider_security_as_new(&security),
                    )
                    .await
                    .map_err(from_sync_error)?;
                if resolved.created {
                    report.securities_created += 1;
                }
                resolved.master
            }
        };
        known.insert(security.security_id.clone(), master);
    }

    let (skipped, planned) =
        plan_holdings(conn, item_id, as_of, holdings, accounts, &mut Known(&known))?;
    report.skipped_unconfigured = skipped;

    for plan in planned {
        let recorded = client
            .record_holdings_snapshot(&plan.snapshot)
            .await
            .map_err(from_sync_error)?
            .recorded;
        if recorded {
            report.recorded += 1;
        } else {
            report.unchanged += 1;
        }

        let Some(account_id) = plan.sheltered_account_id.as_deref() else {
            continue;
        };
        match plan.snapshot.total_value_cents() {
            Some(value_cents) if statement_already_recorded(conn, account_id, as_of, value_cents) => {
            }
            Some(value_cents) => {
                let set = client
                    .set_retirement_value(&SetRetirementValueCommand {
                        account_id: account_id.to_string(),
                        as_of,
                        value_cents,
                        memo: None,
                    })
                    .await
                    .map_err(from_sync_error)?;
                report.values_set.push((
                    plan.plaid_account_id.clone(),
                    ValueSet {
                        book_value_cents: set.book_value_cents,
                        change_cents: set.change_cents,
                        entry_id: set.entry_id,
                    },
                ));
            }
            None => report.incomplete_values.push(plan.plaid_account_id.clone()),
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
    /// What the security master calls this security's type, when we have a master
    /// for it. The input to [`HoldingLine::group`], and worth carrying rather than
    /// re-deriving: a reconciliation printed a year from now should say what the
    /// kind was when it was run.
    pub kind: Option<String>,
    /// The securities subaccount the books carry it in. `None` when there is no
    /// master for it, which is also when there is no position of ours to speak of.
    pub securities_account_id: Option<String>,
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

    /// Which securities slot this line's kind falls in, when the kind is known.
    pub fn group(&self) -> Option<SecurityKindGroup> {
        self.kind.as_deref().map(SecurityKindGroup::of)
    }

    /// Whether a cost difference on this line has an innocent explanation.
    ///
    /// **Only for a mutual fund, and only when the quantities agree.** A broker may
    /// compute a fund's basis by average cost, which the regulations permit for
    /// funds (§1.1012-1(e)) and do not permit for stocks; ours is always the sum of
    /// the lots. So the same shares can carry two different bases and neither side
    /// is wrong.
    ///
    /// The quantity condition is what stops this becoming an excuse: a difference in
    /// the number of shares is not a method difference in anybody's method, and a
    /// missing trade on a fund would otherwise be waved through as one. This says
    /// "this one may be innocent", never "this one is fine" — nothing here suppresses
    /// a line or adjusts anything (spec §7).
    pub fn method_difference_possible(&self) -> bool {
        self.group() == Some(SecurityKindGroup::MutualFunds)
            && self.quantity_difference() == 0
            && self.cost_difference().is_some_and(|d| d != 0)
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
    /// Every distinct securities subaccount the books carry this brokerage's
    /// holdings in — one for a configuration written before the kinds were split,
    /// up to three after. Plural because the comparison has to look in all of them:
    /// a position read out of the wrong slot reconciles as missing.
    pub securities_account_ids: Vec<String>,
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

    /// Disagreements that a fund's average-cost basis could account for on its own.
    ///
    /// Reported beside the rest rather than instead of them: they are still
    /// differences, and one of them can still be a missing trade. See
    /// [`HoldingLine::method_difference_possible`].
    pub fn possible_method_differences(&self) -> Vec<&HoldingLine> {
        self.lines
            .iter()
            .filter(|l| l.method_difference_possible())
            .collect()
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
    let securities_account_ids: Vec<String> = taxable
        .securities_accounts()
        .into_iter()
        .map(|(id, _)| id)
        .collect();

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
        // Which slot to look in comes from the security's kind, which is the same
        // rule the importer posted it under. Looking in the stocks account for a
        // bond would report the whole position missing and a second line holding
        // it, which is two findings where there is no fact to find.
        let (kind, securities_account_id, book_quantity, book_cost_cents) = match &line.security_id
        {
            Some(id) => {
                seen.push(id.clone());
                let kind = investment_commands::get_security(conn, id).map(|s| s.kind);
                let account = taxable
                    .securities_account_for_kind(kind.as_deref().unwrap_or_default())
                    .to_string();
                let (quantity, cost) = investment_commands::holding_of(conn, id, &account);
                (kind, Some(account), quantity, cost)
            }
            None => (None, None, 0, 0),
        };
        lines.push(HoldingLine {
            plaid_security_id: Some(line.plaid_security_id),
            security_id: line.security_id,
            ticker: line.ticker,
            kind,
            securities_account_id,
            book_quantity,
            book_cost_cents,
            broker_quantity: line.quantity,
            broker_cost_cents: line.cost_basis_cents,
        });
    }

    // And the other direction: something the books hold that the snapshot does not
    // mention. Left out, this would be the silent half of the comparison — a sale
    // the broker recorded and we never imported would reconcile clean.
    //
    // Every slot, and deduplicated by security: two slots may be one account under
    // a configuration written before the split, and one position listed twice reads
    // as two.
    for securities_account_id in &securities_account_ids {
        for (security_id, ticker) in book_positions(conn, securities_account_id) {
            if seen.contains(&security_id) {
                continue;
            }
            let (book_quantity, book_cost_cents) =
                investment_commands::holding_of(conn, &security_id, securities_account_id);
            if book_quantity == 0 && book_cost_cents == 0 {
                continue;
            }
            seen.push(security_id.clone());
            let kind = investment_commands::get_security(conn, &security_id).map(|s| s.kind);
            lines.push(HoldingLine {
                plaid_security_id: None,
                security_id: Some(security_id),
                ticker: Some(ticker),
                kind,
                securities_account_id: Some(securities_account_id.clone()),
                book_quantity,
                book_cost_cents,
                broker_quantity: 0,
                broker_cost_cents: None,
            });
        }
    }

    Some(Reconciliation {
        item_id: item_id.to_string(),
        plaid_account_id: plaid_account_id.to_string(),
        securities_account_ids,
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
// Holdings, with cost beside value
// ---------------------------------------------------------------------------

/// One security held in one brokerage account: what our lots say it cost, and what
/// the broker last said it was worth.
///
/// The two numbers come from different places on purpose, and neither is derived
/// from the other. Cost is the sum of the remaining basis of the lots we posted —
/// it is in the ledger, on the balance sheet, and it is what a gain is computed
/// against. Value is from the latest holdings snapshot, is **never posted** (spec
/// §3), and is missing whenever the broker did not state it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub security_id: String,
    /// The master's ticker — which may be a synthesised label such as
    /// `CUSIP:037833100` for a security with no symbol. See [`ticker_for`].
    pub ticker: String,
    pub name: String,
    /// The master's own word for what it is, as the broker gave it.
    pub kind: String,
    /// Which slot that kind falls in.
    pub group: SecurityKindGroup,
    /// The subaccount the position is actually carried in — which is the slot's
    /// account under the configuration that posted it, and can therefore differ
    /// from `group`'s account if the configuration has since been changed.
    pub securities_account_id: String,
    /// Micro-shares.
    pub quantity: i64,
    pub cost_cents: i64,
    /// Market value from the latest snapshot. `None` when the broker gave none, or
    /// when the snapshot does not mention this holding at all.
    pub value_cents: Option<i64>,
}

impl Position {
    /// Value less cost: **the unrealized gain**, which is what the gap between the
    /// two numbers is.
    ///
    /// `None` rather than zero when there is no value to compare, because "the
    /// broker did not say" and "it has not moved" are different facts and only one
    /// of them is worth reporting as a gain of nothing.
    pub fn unrealized_gain_cents(&self) -> Option<i64> {
        self.value_cents.map(|v| v - self.cost_cents)
    }
}

/// Every position in one brokerage account, and the date the values are as of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holdings {
    pub item_id: String,
    pub plaid_account_id: String,
    /// The snapshot the values came from. `None` when there is no snapshot yet, in
    /// which case every position's value is `None` too — the cost side still reads
    /// perfectly well on its own, which is why this is not an error.
    pub as_of: Option<NaiveDate>,
    pub positions: Vec<Position>,
}

/// The positions carried in one securities subaccount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldingsGroup<'a> {
    pub securities_account_id: String,
    /// Which slots this account serves. More than one when a configuration points
    /// two of them at the same account, which is what every configuration written
    /// before the kinds were split does.
    pub groups: Vec<SecurityKindGroup>,
    pub positions: Vec<&'a Position>,
}

impl HoldingsGroup<'_> {
    /// "Stocks", or "Stocks · Mutual funds · Other" for an account serving several.
    pub fn label(&self) -> String {
        self.groups
            .iter()
            .map(|g| g.label())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn cost_cents(&self) -> i64 {
        self.positions.iter().map(|p| p.cost_cents).sum()
    }

    /// The account's market value, or `None` if any position in it has none.
    ///
    /// All or nothing, for the reason phase 4 refuses to set a sheltered account's
    /// value from an incomplete snapshot: a total missing one holding is not a
    /// total, and an unrealized gain computed from it is a number that looks like
    /// money and is not.
    pub fn value_cents(&self) -> Option<i64> {
        self.positions
            .iter()
            .map(|p| p.value_cents)
            .try_fold(0i64, |acc, v| Some(acc + v?))
    }

    pub fn unrealized_gain_cents(&self) -> Option<i64> {
        self.value_cents().map(|v| v - self.cost_cents())
    }
}

impl Holdings {
    pub fn cost_cents(&self) -> i64 {
        self.positions.iter().map(|p| p.cost_cents).sum()
    }

    /// As [`HoldingsGroup::value_cents`], and `None` for the same reason.
    pub fn value_cents(&self) -> Option<i64> {
        self.positions
            .iter()
            .map(|p| p.value_cents)
            .try_fold(0i64, |acc, v| Some(acc + v?))
    }

    pub fn unrealized_gain_cents(&self) -> Option<i64> {
        self.value_cents().map(|v| v - self.cost_cents())
    }

    /// The positions grouped by the subaccount they are carried in, slots in order.
    ///
    /// Grouped by the **account** rather than by the kind, because the account is
    /// what a trial balance shows and what a reader is reconciling against. An
    /// account with nothing in it is left out: an empty group is furniture.
    pub fn by_account(&self, accounts: &TaxableBrokerageAccounts) -> Vec<HoldingsGroup<'_>> {
        let mut out = Vec::new();
        for (securities_account_id, groups) in accounts.securities_accounts() {
            let positions: Vec<&Position> = self
                .positions
                .iter()
                .filter(|p| p.securities_account_id == securities_account_id)
                .collect();
            if positions.is_empty() {
                continue;
            }
            out.push(HoldingsGroup {
                securities_account_id,
                groups,
                positions,
            });
        }
        // A position carried in an account no slot points at any more — the
        // configuration was changed after it was bought. Listed under its own
        // account rather than dropped: the shares are really there, and a holdings
        // report that hides them is worse than one that shows an account the
        // configuration has moved on from.
        let mut orphans: Vec<&Position> = self
            .positions
            .iter()
            .filter(|p| {
                !out.iter()
                    .any(|g: &HoldingsGroup<'_>| g.securities_account_id == p.securities_account_id)
            })
            .collect();
        orphans.sort_by(|a, b| a.securities_account_id.cmp(&b.securities_account_id));
        for position in orphans {
            match out
                .iter_mut()
                .find(|g| g.securities_account_id == position.securities_account_id)
            {
                Some(group) => group.positions.push(position),
                None => out.push(HoldingsGroup {
                    securities_account_id: position.securities_account_id.clone(),
                    groups: Vec::new(),
                    positions: vec![position],
                }),
            }
        }
        out
    }
}

/// What one taxable brokerage account holds, with cost beside value.
///
/// `None` when the account is not configured or is **sheltered** — there are no
/// holdings to report inside one, by design: nothing in it is recorded, and its
/// value is one figure on phase 2's register rather than a list of positions (spec
/// §2b).
///
/// Every securities subaccount the configuration names is read, and a position is
/// listed under the account it is genuinely carried in.
pub fn holdings(conn: &Connection, item_id: &str, plaid_account_id: &str) -> Option<Holdings> {
    let config = get_config(conn, item_id, plaid_account_id)?;
    let taxable = config.taxable()?;

    let snapshot = latest_snapshot(conn, item_id, plaid_account_id);
    let values: BTreeMap<String, i64> = snapshot
        .as_ref()
        .map(|(snapshot_id, _)| {
            snapshot_lines_with_value(conn, snapshot_id)
                .into_iter()
                .filter_map(|(security_id, value)| Some((security_id?, value?)))
                .collect()
        })
        .unwrap_or_default();

    let mut positions: Vec<Position> = Vec::new();
    for (securities_account_id, _) in taxable.securities_accounts() {
        for (security_id, ticker) in book_positions(conn, &securities_account_id) {
            let (quantity, cost_cents) =
                investment_commands::holding_of(conn, &security_id, &securities_account_id);
            // A lot that has been sold down to nothing is history, not a holding.
            // It stays in the register — a Form 8949 row still has to name the date
            // it was acquired — and it does not belong in a list of what is held.
            if quantity == 0 && cost_cents == 0 {
                continue;
            }
            let security = investment_commands::get_security(conn, &security_id);
            let kind = security
                .as_ref()
                .map(|s| s.kind.clone())
                .unwrap_or_default();
            positions.push(Position {
                value_cents: values.get(&security_id).copied(),
                group: SecurityKindGroup::of(&kind),
                name: security
                    .as_ref()
                    .map(|s| s.name.clone())
                    .unwrap_or_else(|| ticker.clone()),
                kind,
                ticker,
                security_id,
                securities_account_id: securities_account_id.clone(),
                quantity,
                cost_cents,
            });
        }
    }
    positions.sort_by(|a, b| {
        a.securities_account_id
            .cmp(&b.securities_account_id)
            .then_with(|| a.ticker.cmp(&b.ticker))
    });

    Some(Holdings {
        item_id: item_id.to_string(),
        plaid_account_id: plaid_account_id.to_string(),
        as_of: snapshot.map(|(_, as_of)| as_of),
        positions,
    })
}

/// The most recent snapshot for one account: its id and its date.
fn latest_snapshot(
    conn: &Connection,
    item_id: &str,
    plaid_account_id: &str,
) -> Option<(String, NaiveDate)> {
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
    Some((
        snapshot_id,
        NaiveDate::parse_from_str(&as_of, "%Y-%m-%d").ok()?,
    ))
}

/// `(our security id, market value)` per snapshot line. Both may be absent — a
/// holding with no master of ours, and a broker that stated no value — and a line
/// missing either is no use to a value report.
fn snapshot_lines_with_value(
    conn: &Connection,
    snapshot_id: &str,
) -> Vec<(Option<String>, Option<i64>)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT security_id, value_cents FROM investment_holdings_snapshot_lines
          WHERE snapshot_id = ?1",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([snapshot_id], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?))
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
    /// The stocks slot, under the name it has had since phase 4.
    const SECURITIES: &str = "1110";
    const MUTUAL_FUNDS: &str = "1111";
    const OTHER_SECURITIES: &str = "1112";
    /// Where withholding on a distribution becomes a prepaid tax.
    const PREPAID_TAX: &str = "1200";
    const IRA: &str = "1500";
    const INVESTMENT_INCOME: &str = "4000";
    const DIVIDENDS: &str = "4100";
    const INTEREST: &str = "4110";
    const TAX_EXEMPT_INTEREST: &str = "4111";
    const CAPITAL_GAIN_DISTRIBUTIONS: &str = "4112";
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
            (
                MUTUAL_FUNDS,
                "Mutual funds at cost",
                AccountType::Asset,
                None,
            ),
            (
                OTHER_SECURITIES,
                "Other securities at cost",
                AccountType::Asset,
                None,
            ),
            (PREPAID_TAX, "Prepaid tax", AccountType::Asset, None),
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
                TAX_EXEMPT_INTEREST,
                "Tax-exempt interest",
                AccountType::Revenue,
                Some(INVESTMENT_INCOME),
            ),
            (
                CAPITAL_GAIN_DISTRIBUTIONS,
                "Capital gain distributions",
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
            stocks_account_id: SECURITIES.into(),
            mutual_funds_account_id: None,
            other_securities_account_id: None,
            cash_account_id: BROKER_CASH.into(),
            dividend_income_account_id: DIVIDENDS.into(),
            interest_income_account_id: INTEREST.into(),
            tax_exempt_interest_account_id: None,
            capital_gain_distribution_account_id: None,
            realized_gain_account_id: REALIZED_GAIN.into(),
            fee_expense_account_id: FEES.into(),
            transfer_clearing_account_id: clearing.map(str::to_string),
        }))
    }

    /// The configuration phase 5 asks for: securities split three ways, four income
    /// accounts.
    fn split_accounts() -> InvestmentPostingAccounts {
        let InvestmentPostingAccounts::Taxable(mut a) = taxable_accounts(Some(CLEARING)) else {
            unreachable!("taxable_accounts builds a taxable configuration");
        };
        a.mutual_funds_account_id = Some(MUTUAL_FUNDS.into());
        a.other_securities_account_id = Some(OTHER_SECURITIES.into());
        a.tax_exempt_interest_account_id = Some(TAX_EXEMPT_INTEREST.into());
        a.capital_gain_distribution_account_id = Some(CAPITAL_GAIN_DISTRIBUTIONS.into());
        InvestmentPostingAccounts::Taxable(a)
    }

    /// A store configured with [`split_accounts`].
    fn configured_split() -> EventStore {
        let mut store = store();
        configure_account(
            &mut store,
            "u",
            &ConfigureInvestmentAccountCommand {
                item_id: ITEM.into(),
                plaid_account_id: BRK.into(),
                accounts: split_accounts(),
                plaid_subtype: Some("brokerage".into()),
            },
        )
        .expect("configured");
        store
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
            stocks_account_id: SECURITIES.into(),
            mutual_funds_account_id: None,
            other_securities_account_id: None,
            cash_account_id: BROKER_CASH.into(),
            // An expense account where income belongs.
            dividend_income_account_id: FEES.into(),
            interest_income_account_id: INTEREST.into(),
            tax_exempt_interest_account_id: None,
            capital_gain_distribution_account_id: None,
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
        // A capital-gain distribution is Schedule D income, not Schedule B. Phase 5
        // gave it an account of its own, so it posts — to that account, and never to
        // dividends. Without one configured it is held again, which
        // `a_capital_gain_distribution_needs_its_own_account` covers.
        assert_eq!(
            plan(taxable, "cash", "long-term capital gain"),
            Plan::Post(PostAs::Income(
                InvestmentIncomeKind::CapitalGainDistribution
            ))
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

    // -----------------------------------------------------------------------
    // Phase 5: securities by kind, four income accounts, holdings and review
    // -----------------------------------------------------------------------

    fn security_of(id: &str, ticker: &str, kind: &str) -> ProviderSecurity {
        ProviderSecurity {
            security_id: id.into(),
            ticker: Some(ticker.into()),
            name: Some(format!("{ticker} holding")),
            security_type: Some(kind.into()),
            cusip: None,
            isin: None,
            iso_currency_code: Some("USD".into()),
        }
    }

    /// A purchase of `quantity` units of one security for `amount` dollars.
    fn buy_of(
        id: &str,
        security: ProviderSecurity,
        quantity: f64,
        amount: f64,
    ) -> ProviderInvestmentTransaction {
        ProviderInvestmentTransaction {
            security_id: Some(security.security_id.clone()),
            security: Some(security),
            quantity,
            price: amount / quantity,
            fees: None,
            ..txn(id, BRK, "buy", "buy", day(2026, 3, 2), amount)
        }
    }

    /// A stock, a fund and a bond bought in one account go to three different
    /// securities subaccounts — and a later sale relieves the basis out of the same
    /// one, which is not a nicety: lots are keyed by `(security, securities
    /// account)`, so a sale looking in the wrong account finds no lots and cannot
    /// compute a gain at all.
    #[test]
    fn each_kind_of_security_is_carried_in_the_subaccount_its_kind_says() {
        let mut store = configured_split();
        let payload = vec![
            buy_of(
                "tx-stock",
                security_of("sec-aapl", "AAPL", "equity"),
                10.0,
                1_000.0,
            ),
            buy_of(
                "tx-fund",
                security_of("sec-vtsax", "VTSAX", "mutual fund"),
                10.0,
                1_000.0,
            ),
            buy_of(
                "tx-bond",
                security_of("sec-tbill", "T-BILL", "fixed income"),
                1.0,
                950.0,
            ),
        ];
        let report = import(&mut store, &[brokerage_account("brokerage")], &payload);
        assert_eq!((report.bought, report.held), (3, 0));

        for (provider_security, account) in [
            ("sec-aapl", SECURITIES),
            ("sec-vtsax", MUTUAL_FUNDS),
            ("sec-tbill", OTHER_SECURITIES),
        ] {
            let security = security_id_of(&store, provider_security);
            let (quantity, _) = holding_of(store.connection(), &security, account);
            assert!(
                quantity > 0,
                "{provider_security} is not carried in {account}"
            );
        }
        assert_eq!(
            entry_lines(&store, "tx-fund"),
            vec![
                (BROKER_CASH.to_string(), -100_000),
                (MUTUAL_FUNDS.to_string(), 100_000),
            ]
        );

        // Half the fund, at $120 a share: $600 in, $500 of basis out of the fund
        // account, $100 of gain.
        let sale = ProviderInvestmentTransaction {
            security_id: Some("sec-vtsax".into()),
            security: Some(security_of("sec-vtsax", "VTSAX", "mutual fund")),
            quantity: 5.0,
            price: 120.0,
            fees: None,
            ..txn("tx-fund-sell", BRK, "sell", "sell", day(2026, 6, 1), -600.0)
        };
        let report = import(&mut store, &[brokerage_account("brokerage")], &[sale]);
        assert_eq!((report.sold, report.held), (1, 0));
        assert_eq!(
            entry_lines(&store, "tx-fund-sell"),
            vec![
                (BROKER_CASH.to_string(), 60_000),
                (MUTUAL_FUNDS.to_string(), -50_000),
                (REALIZED_GAIN.to_string(), -10_000),
            ]
        );
    }

    /// A capital gain distribution posts to the account configured for it — never to
    /// dividends, which is a different form at a different rate — and is held when
    /// there is no such account, which is the only income slot with no fallback.
    #[test]
    fn a_capital_gain_distribution_posts_to_its_own_account_or_is_held() {
        let distribution = || ProviderInvestmentTransaction {
            security_id: Some("sec-vtsax".into()),
            security: Some(security_of("sec-vtsax", "VTSAX", "mutual fund")),
            ..txn(
                "tx-cg",
                BRK,
                "cash",
                "long term capital gain",
                day(2026, 12, 20),
                -250.0,
            )
        };

        let mut split = configured_split();
        let report = import(
            &mut split,
            &[brokerage_account("brokerage")],
            &[distribution()],
        );
        assert_eq!(
            (
                report.capital_gain_distributions,
                report.dividends,
                report.held
            ),
            (1, 0, 0)
        );
        assert_eq!(
            entry_lines(&split, "tx-cg"),
            vec![
                (BROKER_CASH.to_string(), 25_000),
                (CAPITAL_GAIN_DISTRIBUTIONS.to_string(), -25_000),
            ]
        );
        assert_eq!(
            count(
                &split,
                "SELECT COUNT(*) FROM investment_imports
                  WHERE outcome = 'capital_gain_distribution'"
            ),
            1
        );

        // Without the account: held, and the reason says what to configure rather
        // than "no rule covers this".
        let mut plain = configured(None);
        let report = import(
            &mut plain,
            &[brokerage_account("brokerage")],
            &[distribution()],
        );
        assert_eq!((report.held, report.posted()), (1, 0));
        let held = pending_activity(plain.connection());
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].reason, "no_income_account");
        assert!(
            held[0].detail.contains("Schedule D"),
            "the row has to say why the dividend account will not do: {}",
            held[0].detail
        );
    }

    /// Holdings put our cost beside the broker's value and call the gap what it is.
    /// Neither number is derived from the other: cost is the ledger's, value is the
    /// snapshot's, and value is never posted (spec §3).
    #[test]
    fn holdings_show_cost_beside_value_and_name_the_gap_the_unrealized_gain() {
        let mut store = configured_split();
        import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[
                buy_of(
                    "tx-stock",
                    security_of("sec-aapl", "AAPL", "equity"),
                    10.0,
                    1_000.0,
                ),
                buy_of(
                    "tx-fund",
                    security_of("sec-vtsax", "VTSAX", "mutual fund"),
                    10.0,
                    1_000.0,
                ),
            ],
        );
        let snapshot = vec![
            ProviderHolding {
                account_id: BRK.into(),
                security_id: "sec-aapl".into(),
                security: Some(security_of("sec-aapl", "AAPL", "equity")),
                quantity: 10.0,
                cost_basis: Some(1_000.0),
                institution_value: Some(1_500.0),
                iso_currency_code: Some("USD".into()),
            },
            ProviderHolding {
                account_id: BRK.into(),
                security_id: "sec-vtsax".into(),
                security: Some(security_of("sec-vtsax", "VTSAX", "mutual fund")),
                quantity: 10.0,
                cost_basis: Some(1_000.0),
                // The broker did not state a value for this one.
                institution_value: None,
                iso_currency_code: Some("USD".into()),
            },
        ];
        import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 6, 30),
            &snapshot,
            &[brokerage_account("brokerage")],
        )
        .expect("snapshot recorded");

        let held = holdings(store.connection(), ITEM, BRK).expect("a taxable account holds");
        assert_eq!(held.as_of, Some(day(2026, 6, 30)));
        assert_eq!(held.positions.len(), 2);
        let apple = held
            .positions
            .iter()
            .find(|p| p.ticker == "AAPL")
            .expect("the stock");
        assert_eq!(
            (
                apple.cost_cents,
                apple.value_cents,
                apple.unrealized_gain_cents()
            ),
            (100_000, Some(150_000), Some(50_000))
        );
        let fund = held
            .positions
            .iter()
            .find(|p| p.ticker == "VTSAX")
            .expect("the fund");
        assert_eq!(
            (fund.value_cents, fund.unrealized_gain_cents()),
            (None, None),
            "a value the broker did not state is not a gain of nothing"
        );
        // One holding with no value makes the total no total, which is the same
        // stance phase 4 takes before setting a sheltered account's value.
        assert_eq!(held.cost_cents(), 200_000);
        assert_eq!(held.value_cents(), None);

        // Grouped by the subaccount each is carried in.
        let InvestmentPostingAccounts::Taxable(accounts) = split_accounts() else {
            unreachable!()
        };
        let groups = held.by_account(&accounts);
        assert_eq!(
            groups.len(),
            2,
            "the bond slot holds nothing and is not shown"
        );
        assert_eq!(groups[0].securities_account_id, SECURITIES);
        assert_eq!(groups[0].label(), "Stocks");
        assert_eq!(groups[0].unrealized_gain_cents(), Some(50_000));
        assert_eq!(groups[1].securities_account_id, MUTUAL_FUNDS);
        assert_eq!(groups[1].label(), "Mutual funds");
        assert_eq!(groups[1].value_cents(), None);

        // A sheltered account has no holdings to report: nothing inside one is
        // recorded, and its value is one figure on the retirement register.
        with_sheltered(&mut store);
        assert!(holdings(store.connection(), ITEM, IRA_PLAID).is_none());
    }

    /// The reconciliation looks for a position in the slot its kind says, and a fund
    /// whose cost differs while its quantity agrees is flagged as *possibly* a method
    /// difference — the broker may use average cost, which is permitted for funds and
    /// not for stocks. It is still reported: nothing here suppresses a line.
    #[test]
    fn a_funds_cost_difference_may_be_a_method_difference_and_a_stocks_may_not() {
        let mut store = configured_split();
        import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[
                buy_of(
                    "tx-stock",
                    security_of("sec-aapl", "AAPL", "equity"),
                    10.0,
                    1_000.0,
                ),
                buy_of(
                    "tx-fund",
                    security_of("sec-vtsax", "VTSAX", "mutual fund"),
                    10.0,
                    1_000.0,
                ),
            ],
        );
        let snapshot = vec![
            ProviderHolding {
                account_id: BRK.into(),
                security_id: "sec-aapl".into(),
                security: Some(security_of("sec-aapl", "AAPL", "equity")),
                quantity: 10.0,
                // $10 apart on a stock: a finding, not a method.
                cost_basis: Some(1_010.0),
                institution_value: Some(1_500.0),
                iso_currency_code: Some("USD".into()),
            },
            ProviderHolding {
                account_id: BRK.into(),
                security_id: "sec-vtsax".into(),
                security: Some(security_of("sec-vtsax", "VTSAX", "mutual fund")),
                quantity: 10.0,
                cost_basis: Some(1_010.0),
                institution_value: Some(1_100.0),
                iso_currency_code: Some("USD".into()),
            },
        ];
        import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 6, 30),
            &snapshot,
            &[brokerage_account("brokerage")],
        )
        .expect("snapshot recorded");

        let recon = reconcile(store.connection(), ITEM, BRK).expect("a taxable account reconciles");
        assert_eq!(
            recon.securities_account_ids,
            vec![SECURITIES, MUTUAL_FUNDS, OTHER_SECURITIES],
            "every slot is looked in, or a position reconciles as missing"
        );
        assert!(!recon.agrees());
        assert_eq!(recon.disagreements().len(), 2);
        // Each line was read out of the account its kind says, so neither position
        // reads as missing.
        for line in &recon.lines {
            assert_eq!(line.quantity_difference(), 0, "{:?}", line.ticker);
            assert_eq!(line.cost_difference(), Some(-1_000));
        }
        let flagged = recon.possible_method_differences();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].ticker.as_deref(), Some("VTSAX"));
        assert_eq!(
            flagged[0].securities_account_id.as_deref(),
            Some(MUTUAL_FUNDS)
        );

        // A quantity difference is not a method difference in anybody's method, so
        // the fund stops being excused the moment the share counts disagree.
        let moved = vec![ProviderHolding {
            account_id: BRK.into(),
            security_id: "sec-vtsax".into(),
            security: Some(security_of("sec-vtsax", "VTSAX", "mutual fund")),
            quantity: 9.0,
            cost_basis: Some(1_010.0),
            institution_value: Some(1_100.0),
            iso_currency_code: Some("USD".into()),
        }];
        import_holdings(
            &mut store,
            "u",
            ITEM,
            day(2026, 7, 31),
            &moved,
            &[brokerage_account("brokerage")],
        )
        .expect("a later snapshot");
        let recon = reconcile(store.connection(), ITEM, BRK).expect("reconciles");
        assert!(recon.possible_method_differences().is_empty());
    }

    // -----------------------------------------------------------------------
    // The review list: getting a held row off `pending`
    // -----------------------------------------------------------------------

    /// One held corporate action, the shape every review test starts from.
    fn with_a_held_split() -> EventStore {
        let mut store = configured_split();
        let split = ProviderInvestmentTransaction {
            security_id: Some("sec-aapl".into()),
            security: Some(apple()),
            quantity: 40.0,
            ..txn("tx-split", BRK, "buy", "split", day(2026, 8, 31), 0.0)
        };
        import(&mut store, &[brokerage_account("brokerage")], &[split]);
        assert_eq!(pending_activity(store.connection()).len(), 1);
        store
    }

    fn only_pending(store: &EventStore) -> StagedActivity {
        let mut rows = pending_activity(store.connection());
        assert_eq!(rows.len(), 1, "expected exactly one pending row");
        rows.remove(0)
    }

    /// Entering a corporate action by hand posts nothing — what it should post is a
    /// judgement about basis this program will not make (spec §7) — and records that
    /// a person dealt with it, with a note and, when there is one, the entry.
    #[test]
    fn a_resolution_and_a_dismissal_are_not_the_same_answer() {
        let store = with_a_held_split();
        let row = only_pending(&store);
        resolve_by_hand(
            store.connection(),
            &row.id,
            "Entered the 4-for-1 split by hand across the two open lots",
            Some("entry-123"),
        )
        .expect("resolved");
        assert!(pending_activity(store.connection()).is_empty());
        let resolved = get_activity(store.connection(), &row.id).expect("still there");
        assert_eq!(resolved.status, RESOLVED);
        assert_eq!(resolved.resolution.as_deref(), Some("by_hand"));
        assert_eq!(resolved.resolution_entry_id.as_deref(), Some("entry-123"));
        assert!(resolved.resolved_at.is_some());
        assert_eq!(activity_with_status(store.connection(), RESOLVED).len(), 1);
        assert!(activity_with_status(store.connection(), DISMISSED).is_empty());

        // The other answer, on another row, is a different status — because "it
        // reached the books another way" and "it never will" are opposite answers to
        // "is anything missing from these books?".
        let store = with_a_held_split();
        let row = only_pending(&store);
        dismiss(
            store.connection(),
            &row.id,
            "Duplicate of the split the broker also reported as a transfer",
        )
        .expect("dismissed");
        let dismissed = get_activity(store.connection(), &row.id).expect("still there");
        assert_eq!(dismissed.status, DISMISSED);
        assert_eq!(dismissed.resolution.as_deref(), Some("dismissed"));
        assert_ne!(dismissed.status, RESOLVED);
    }

    /// A note is required on every transition. Without one the row leaves the list
    /// explained by nothing, and the provider will not offer the transaction again.
    #[test]
    fn a_transition_without_a_note_is_refused() {
        let store = with_a_held_split();
        let row = only_pending(&store);
        let err = resolve_by_hand(store.connection(), &row.id, "   ", None).unwrap_err();
        assert!(err.to_string().contains("say what was done"), "{err}");
        assert!(dismiss(store.connection(), &row.id, "").is_err());
        assert_eq!(pending_activity(store.connection()).len(), 1);
    }

    /// The transition is guarded on the status inside the UPDATE, so a second press
    /// of the same button changes nothing and says so. The failure it prevents on the
    /// paths that post: the same contribution recorded twice.
    #[test]
    fn a_row_can_only_leave_pending_once() {
        let store = with_a_held_split();
        let row = only_pending(&store);
        resolve_by_hand(store.connection(), &row.id, "Entered by hand", None).expect("resolved");
        let err =
            resolve_by_hand(store.connection(), &row.id, "Entered by hand", None).unwrap_err();
        assert!(err.to_string().contains("already resolved"), "{err}");
        let err = dismiss(store.connection(), &row.id, "Changed my mind").unwrap_err();
        assert!(err.to_string().contains("already resolved"), "{err}");
        assert!(
            resolve_by_hand(store.connection(), "no-such-row", "Entered by hand", None).is_err()
        );
    }

    /// A dismissal can be taken back and a resolution cannot. A dismissed row posted
    /// nothing, and the provider will never offer the transaction again, so there is
    /// nowhere else for a mistaken dismissal to come back from; a resolution posted
    /// an entry, and reopening it would invite a second one.
    #[test]
    fn only_a_dismissed_row_can_be_put_back() {
        let store = with_a_held_split();
        let row = only_pending(&store);
        dismiss(store.connection(), &row.id, "Dismissed by mistake").expect("dismissed");
        reopen(store.connection(), &row.id).expect("reopened");
        let back = get_activity(store.connection(), &row.id).expect("still there");
        assert_eq!(back.status, PENDING);
        assert_eq!(back.resolution, None);
        assert_eq!(back.resolution_note, None);

        resolve_by_hand(store.connection(), &row.id, "Entered by hand", None).expect("resolved");
        let err = reopen(store.connection(), &row.id).unwrap_err();
        assert!(err.to_string().contains("Only a dismissed row"), "{err}");
    }

    /// Cash into a sheltered account: the one held row where recording it *is* the
    /// resolution. The retirement account comes from the configuration rather than
    /// from the caller — a contribution into the wrong IRA balances perfectly — and
    /// the entry carries an idempotency reference, so a second attempt after a crash
    /// between the posting and the status change is refused instead of posting twice.
    #[test]
    fn sheltered_cash_recorded_as_a_contribution_closes_the_row() {
        let mut store = configured_split();
        with_sheltered(&mut store);
        import(
            &mut store,
            &[ProviderAccount {
                account_id: IRA_PLAID.into(),
                name: "Fidelity IRA".into(),
                subtype: Some("ira".into()),
                mask: None,
            }],
            &[txn(
                "tx-ira-in",
                IRA_PLAID,
                "cash",
                "contribution",
                day(2026, 1, 15),
                -6_500.0,
            )],
        );
        let row = only_pending(&store);
        assert!(row.is_sheltered_cash());

        let cmd = ResolveAsContributionCommand {
            staged_id: row.id.clone(),
            funding_account_id: CHECKING.into(),
            amount_cents: 650_000,
            on: day(2026, 1, 15),
            memo: None,
            note: "2026 IRA contribution, from checking".into(),
        };
        let entry_id = resolve_as_contribution(&mut store, "u", &cmd).expect("recorded");
        assert!(pending_activity(store.connection()).is_empty());
        let resolved = get_activity(store.connection(), &row.id).expect("still there");
        assert_eq!(resolved.status, RESOLVED);
        assert_eq!(resolved.resolution.as_deref(), Some("contribution"));
        assert_eq!(resolved.resolution_entry_id.as_deref(), Some(&*entry_id));

        // A transfer between two assets, nothing more: the money changed which
        // account holds it, not how much there is.
        assert_eq!(
            all_lines(&store),
            vec![(CHECKING.to_string(), -650_000), (IRA.to_string(), 650_000)]
        );
        // And the reference is the fence: pretend the status change never landed.
        reopen_for_test(&store, &row.id);
        let err = resolve_as_contribution(&mut store, "u", &cmd).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("reference")
                || err.to_string().contains("already"),
            "a second attempt has to be refused by the reference, not posted: {err}"
        );
        assert_eq!(count(&store, "SELECT COUNT(*) FROM journal_entries"), 1);
    }

    /// Put a row back on `pending` without going through [`reopen`], to stand in for
    /// a crash between the entry landing and the status changing. The point of the
    /// test it serves is that the *reference*, not the status, is what stops a second
    /// posting.
    fn reopen_for_test(store: &EventStore, staged_id: &str) {
        store
            .connection()
            .execute(
                "UPDATE investment_staged_activity SET status = 'pending' WHERE id = ?1",
                [staged_id],
            )
            .unwrap();
    }

    /// A taxable brokerage's cash movements are not contributions. Offering the
    /// wrong answer to the wrong row would post a transfer into an account that is
    /// not on the retirement register at all.
    #[test]
    fn a_taxable_row_cannot_be_recorded_as_a_contribution() {
        let mut store = configured(None);
        import(
            &mut store,
            &[brokerage_account("brokerage")],
            &[txn(
                "tx-dep",
                BRK,
                "cash",
                "deposit",
                day(2026, 2, 1),
                -500.0,
            )],
        );
        let row = only_pending(&store);
        assert_eq!(row.reason, "no_clearing_account");
        let err = resolve_as_contribution(
            &mut store,
            "u",
            &ResolveAsContributionCommand {
                staged_id: row.id.clone(),
                funding_account_id: CHECKING.into(),
                amount_cents: 50_000,
                on: day(2026, 2, 1),
                memo: None,
                note: "Not a contribution".into(),
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not because cash moved"),
            "the refusal has to say what the row is actually asking: {err}"
        );
        assert_eq!(pending_activity(store.connection()).len(), 1);
    }

    /// A distribution out of a sheltered account: the gross leaves it, the net lands
    /// in the receiving account, and the withholding becomes a prepaid tax — money
    /// already paid toward a bill not yet settled, which is an asset and not an
    /// expense. No income is posted, whatever the account's kind.
    #[test]
    fn sheltered_cash_recorded_as_a_distribution_withholds_to_a_prepaid_tax() {
        let mut store = configured_split();
        with_sheltered(&mut store);
        import(
            &mut store,
            &[ProviderAccount {
                account_id: IRA_PLAID.into(),
                name: "Fidelity IRA".into(),
                subtype: Some("ira".into()),
                mask: None,
            }],
            &[txn(
                "tx-ira-out",
                IRA_PLAID,
                "cash",
                "distribution",
                day(2026, 8, 1),
                2_000.0,
            )],
        );
        let row = only_pending(&store);
        let distributed = resolve_as_distribution(
            &mut store,
            "u",
            &ResolveAsDistributionCommand {
                staged_id: row.id.clone(),
                receiving_account_id: CHECKING.into(),
                gross_cents: 200_000,
                withheld_cents: 20_000,
                withheld_account_id: PREPAID_TAX.into(),
                taxable_cents: None,
                on: day(2026, 8, 1),
                memo: None,
                note: "Took $2,000 out, 10% withheld".into(),
            },
        )
        .expect("recorded");
        assert_eq!(distributed.net_cents, 180_000);
        assert_eq!(
            distributed.taxable_cents, 200_000,
            "the whole gross out of a traditional account, from the register's kind"
        );
        assert_eq!(
            all_lines(&store),
            vec![
                (CHECKING.to_string(), 180_000),
                (PREPAID_TAX.to_string(), 20_000),
                (IRA.to_string(), -200_000),
            ]
        );
        let resolved = get_activity(store.connection(), &row.id).expect("still there");
        assert_eq!(resolved.resolution.as_deref(), Some("distribution"));
        assert_eq!(resolved.status, RESOLVED);
    }
}
