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

/// A postal address as an event carries it.
///
/// Its own type rather than six loose fields on three events, and deliberately
/// not `domain::Address`: the log is permanent and the domain is not, so the
/// wire shape is fixed here and converted at the edge — the same reason
/// [`JournalLineData`] exists alongside the domain's journal line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressData {
    pub street: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    pub city: String,
    pub state: String,
    pub postal_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

/// A partner's share of profit, loss, and capital, in parts per million.
///
/// Integers, not percentages: three partners at a third each must sum to a
/// number somebody can check, and in floating point they do not. 100% is
/// 1_000_000.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ShareData {
    pub profit_ppm: i64,
    pub loss_ppm: i64,
    pub capital_ppm: i64,
}

/// The partnership header, as one event's payload.
///
/// Boxed into its variant rather than spelled out inline: these fields together
/// are larger than every other event in the log, and an enum is as big as its
/// largest variant. Inline, they nearly doubled [`Event`] — 160 bytes to 296 —
/// which is paid by every event the system moves, not just this one.
///
/// The wire format is unaffected. Serde's internally tagged representation
/// flattens a newtype variant's struct into the same object the equivalent
/// struct variant produces, so events already in a log deserialize unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusinessProfileData {
    pub legal_name: String,
    pub address: AddressData,
    /// `NN-NNNNNNN`.
    pub ein: String,
    /// Six digits — Form 1065 box C.
    pub naics_code: String,
    /// Form 1065 box E, "Date business started".
    pub formation_date: NaiveDate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_product: Option<String>,
}

/// The Illinois IL-1065 settings, as one event. Small, but boxed-free because it
/// is two flags rather than the dozen fields that make [`BusinessProfileData`]
/// worth boxing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Il1065SettingsData {
    pub apportions_outside_illinois: bool,
    pub elects_pte_tax: bool,
}

/// A partner joining. Boxed for the same reason as [`BusinessProfileData`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartnerAdmittedData {
    pub partner_id: String,
    pub name: String,
    /// "general" or "limited" — K-1 item G.
    pub partner_type: String,
    /// "domestic" or "foreign" — K-1 item H1.
    pub residency: String,
    /// K-1 item I1, free text because the form's own answer is free text.
    pub entity_type: String,
    pub address: AddressData,
    pub start_date: NaiveDate,
    pub shares: ShareData,
}

/// A partner's details changing. Carries the whole record, not a diff, so the
/// state after any event is readable without replaying the ones before it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartnerDetailsData {
    pub partner_id: String,
    pub name: String,
    pub partner_type: String,
    pub residency: String,
    pub entity_type: String,
    pub address: AddressData,
    pub shares: ShareData,
}

/// The individual who owns a sole proprietorship.
///
/// No identifying number, on purpose, and for the reason
/// [`PartnerAdmittedData`] carries none: this log is replicated in full to every
/// member's machine and cannot be redacted. A sole proprietor's number on
/// Schedule C is their own SSN and the whole return is about one person, so it
/// stays in `sole_proprietor_tin` on the machine that prepares the return.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoleProprietorData {
    pub name: String,
    /// "cash", "accrual" or "other" — Schedule C line F.
    pub accounting_method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting_method_other: Option<String>,
}

/// A depreciable asset, whole. Boxed for the same reason as
/// [`BusinessProfileData`] — a dozen fields would otherwise widen every
/// `Event` to the size of its largest variant.
///
/// Carries the entire record on both the add and the update, not a diff, so the
/// state after any one event is readable without replaying the ones before it —
/// the rule [`PartnerDetailsData`] follows for the same reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepreciableAssetData {
    pub asset_id: String,
    pub description: String,
    pub asset_account_id: String,
    pub expense_account_id: String,
    pub accumulated_account_id: String,
    /// Where a §179 election is expensed. Required once §179 is elected and
    /// distinct from `expense_account_id`, because §179 is separately stated on
    /// Schedule K line 12 rather than deducted on page 1 line 16a.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_179_account_id: Option<String>,
    /// When it was acquired — which decides the bonus rate, and is not the same
    /// question as when it was placed in service. 2025 splits on 20 January.
    pub acquired_on: NaiveDate,
    /// When it became available and ready for its intended use — which starts
    /// the recovery period and fixes the averaging convention.
    pub placed_in_service: NaiveDate,
    pub cost_cents: i64,
    /// `domain::PropertyClass::as_str`, never a bare number of years: 15-year
    /// land improvements and 15-year qualified improvement property share a life
    /// and not a method, and a log recording `"15"` could not tell them apart.
    pub property_class: String,
    /// "gds" or "ads".
    pub system: String,
    pub section_179_cents: i64,
    /// "take" or "decline" — §168(k).
    pub bonus: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
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
    /// A year-end closing entry: the one that sweeps revenue and expense to
    /// equity and zeroes the income statement.
    ///
    /// This is load-bearing, not a label. A closing entry is dated the last day
    /// of the year it closes, so it falls *inside* that year's income-statement
    /// window — and it debits every revenue account and credits every expense
    /// one. Counted, it reports the year it closed as having earned nothing, and
    /// takes Form 1065 page 1, Schedule C and every P&L down with it. This
    /// variant is how `Reports::income_statement` and
    /// `Reports::calculate_net_income` know to leave it out.
    Closing,
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

/// An assignment that predates dated assignments: it applies to every year
/// until a later one supersedes it.
///
/// Zero rather than a real year because it is not one — it means "as far back as
/// these books go". Rows carrying it are the ones written before assignments
/// were dated, and the desktop says so rather than printing "from year 0".
pub const ANY_YEAR: i32 = 0;

/// Whether a year is the undated sentinel, for `skip_serializing_if`.
fn is_any_year(year: &i32) -> bool {
    *year == ANY_YEAR
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

    // Partnership identity and partners — what a Form 1065 and its K-1s are about.
    /// The whole partnership header in one event.
    ///
    /// Set as a unit rather than field by field because the fields are checked
    /// against each other by the IRS, not individually: an EIN that belongs to a
    /// different legal name is a rejected return, and a log of independent field
    /// edits makes "what did we file under" a question you answer by replaying.
    BusinessProfileSet(Box<BusinessProfileData>),
    /// A partner joins.
    ///
    /// No taxpayer identification number here, on purpose. This log is
    /// replicated in full to every member's laptop, so a partner's SSN written
    /// here is that SSN on every other partner's machine, permanently, in an
    /// append-only file that cannot be redacted. The number is needed only where
    /// a return is actually prepared, so it is held in a local table on that
    /// machine instead — see `partner_tins` in migration 023. Same reasoning as
    /// the event-service API key, which is absent from
    /// [`Event::EventServiceRegistered`] for the same reason.
    PartnerAdmitted(Box<PartnerAdmittedData>),
    /// A partner's details or shares change.
    PartnerDetailsUpdated(Box<PartnerDetailsData>),
    // Preparing the return: which account reports where, and what Schedule B was
    // answered. Both are events rather than local rows because a return is
    // prepared from one member's machine but is *about* the partnership, and a
    // colleague opening the same books has to see the same return taking shape.
    // Contrast `partner_tins`, which stays local: a TIN is a secret, and these
    // are not.
    /// An account is pointed at a Form 1065 line.
    ///
    /// Carries the line *key* rather than the number: the IRS renumbers lines
    /// between revisions, and a log that recorded "line 13" would silently mean
    /// a different line after a renumbering. `tax::lines::MAPPABLE_LINES` owns
    /// what a key means.
    TaxLineMappingSet {
        account_id: String,
        line_key: String,
        /// The first tax year this assignment applies to.
        ///
        /// Resolution is "the greatest `effective_from` at or before the year
        /// being filed", so an assignment made in 2026 does not reach back into
        /// a 2023 return that was filed on the old one, and a year with no
        /// assignment of its own inherits the most recent earlier one rather
        /// than starting blank.
        ///
        /// # Why this is skipped when it is [`ANY_YEAR`]
        ///
        /// Events written before assignments were dated carry no such field.
        /// Replication re-serialises an event to re-derive its hash, so a field
        /// that appeared on the way back out would change the JSON and every
        /// historical mapping event would fail verification. Defaulting to
        /// `ANY_YEAR` and omitting it again reproduces the original bytes
        /// exactly — pinned by
        /// `a_mapping_event_written_before_years_existed_reserialises_byte_for_byte`.
        #[serde(default, skip_serializing_if = "is_any_year")]
        effective_from: i32,
    },
    /// What a partner's percentages became, and from when.
    ///
    /// Percentages change — a partner leaves, another is admitted, the agreement
    /// is renegotiated — and a return for a past year has to show what was true
    /// *then*. Recorded as a dated change rather than an edit for the same
    /// reason the ledger records entries rather than balances.
    PartnerSharesChanged {
        partner_id: String,
        effective_from: chrono::NaiveDate,
        profit_ppm: i64,
        loss_ppm: i64,
        capital_ppm: i64,
    },
    /// A ledger account holds this partner's capital, in the named role.
    ///
    /// `role` is "contribution" or "draw". A partner may own several accounts:
    /// keeping what was put in apart from what was taken out is ordinary
    /// bookkeeping, and Schedule K-1 item L wants both.
    PartnerEquityAccountLinked {
        partner_id: String,
        account_id: String,
        role: String,
    },
    /// An account is no longer a partner's capital.
    PartnerEquityAccountUnlinked {
        partner_id: String,
        account_id: String,
    },
    /// A partner's share of one year's result, fixed in dollars rather than by
    /// percentage — or, with no amount, whatever the fixed shares leave. See
    /// `domain::FixedAllocation`. The note is required: a split that departs
    /// from the percentages on file has to say where it came from.
    PartnerAllocationFixed {
        tax_year: i32,
        partner_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        amount_cents: Option<i64>,
        /// The amount is taken first out of the year's income and the rest is
        /// divided on the percentages. Absent on events written before it existed.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        preferred: bool,
        note: String,
    },
    /// The partner's share of the year goes back to their percentages.
    PartnerAllocationCleared {
        tax_year: i32,
        partner_id: String,
    },
    /// How a liability account bears on Schedule K-1 item K: nonrecourse,
    /// qualified nonrecourse financing, or recourse — to one partner, or on the
    /// loss percentages when none is named. See `domain::LiabilityClass`.
    LiabilityClassified {
        account_id: String,
        /// "nonrecourse", "qualified_nonrecourse" or "recourse".
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partner_id: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        guaranteed: bool,
        note: String,
    },
    /// The liability goes back to the kind of entity's default classification.
    LiabilityClassificationCleared {
        account_id: String,
    },
    /// An account created in error is removed from the chart entirely.
    ///
    /// Distinct from deactivating. A deactivated account is one that *was* used
    /// and no longer is — its history still has to be readable, so the row
    /// stays. This is for the account that was never used at all: a typo, a
    /// duplicate, a category somebody thought they needed. Refused unless
    /// nothing whatsoever points at it, so it can never orphan anything.
    AccountDeleted {
        account_id: String,
    },
    /// How much of an account's balance the law lets you deduct, as a
    /// percentage. Absent means all of it.
    ///
    /// Its own event rather than a field on `TaxLineMappingSet` because it is a
    /// separate fact with a separate life: which line an expense reports on is
    /// a property of the form, how much of it is deductible is a property of
    /// §274 and changes without the line changing.
    TaxDeductionLimitSet {
        account_id: String,
        deductible_pct: u8,
        /// The first tax year this limit applies to. See
        /// [`Event::TaxLineMappingSet`] for why it is skipped at [`ANY_YEAR`].
        #[serde(default, skip_serializing_if = "is_any_year")]
        effective_from: i32,
    },
    /// An account goes back to being fully deductible.
    TaxDeductionLimitCleared {
        account_id: String,
        /// The first tax year the account is fully deductible again. See
        /// [`Event::TaxLineMappingSet`].
        #[serde(default, skip_serializing_if = "is_any_year")]
        effective_from: i32,
    },
    /// A parent account's children print as one row on the statements attached
    /// to the return — or, with `grouped` false, go back to a row each.
    ///
    /// Its own event for the reason [`Event::TaxDeductionLimitSet`] is: which
    /// line an account reports on is a fact about the return, and how finely the
    /// statement behind that line itemises it is a choice about presentation
    /// that changes independently. An event rather than a local setting because
    /// the statement is part of a return about the partnership, and a colleague
    /// generating it has to get the same pages.
    ///
    /// A yes-or-no for a year rather than a set-and-clear pair. Grouping stopped
    /// from 2025 is a row of its own; deleting 2025's row instead would fall back
    /// to an earlier year's yes.
    TaxStatementGroupingSet {
        account_id: String,
        grouped: bool,
        /// The first tax year this applies to.
        effective_from: i32,
    },
    /// An account holds Illinois income or replacement tax — or no longer does —
    /// from a tax year on, so IL-1065 line 16 adds back what the federal return
    /// deducted from it. A dated yes-or-no, like
    /// [`Event::TaxStatementGroupingSet`].
    IllinoisTaxAddbackSet {
        account_id: String,
        added_back: bool,
        /// The first tax year this applies to.
        effective_from: i32,
    },
    /// An account is taken off the return.
    TaxLineMappingCleared {
        account_id: String,
        /// The first tax year the account is off the return. See
        /// [`Event::TaxLineMappingSet`].
        #[serde(default, skip_serializing_if = "is_any_year")]
        effective_from: i32,
    },
    /// One Schedule B answer is given, for one tax year.
    ///
    /// Keyed by year as well as question because the schedule asks about "the
    /// tax year" — see `migrations/026_schedule_b_answers.sql`.
    ScheduleBAnswerSet {
        tax_year: i32,
        answer_key: String,
        value: String,
    },
    /// One Schedule B answer goes back to unanswered.
    ///
    /// Its own event rather than a set-to-empty, because unanswered and "No" are
    /// different states on this form and a log that could not tell them apart
    /// would replay one as the other.
    ScheduleBAnswerCleared {
        tax_year: i32,
        answer_key: String,
    },
    /// A partner leaves. Their K-1 for the year containing `end_date` is final.
    PartnerWithdrawn {
        partner_id: String,
        end_date: NaiveDate,
    },
    /// A family tie between two partners is recorded, for Schedule B-1's §267(c)
    /// constructive-ownership test.
    ///
    /// Not a secret, so — unlike a TIN — it belongs in the log like every other
    /// fact about who the partners are: which two partners are married is
    /// something every member preparing the return has to agree on, exactly as
    /// they agree on the partners' shares.
    ///
    /// `relationship` is the [`crate::domain::RelationshipKind`] as a string
    /// (`"spouse"`, `"sibling"`, `"parent_of"`); for the symmetric kinds the two
    /// ids arrive in canonical order so the pair is one edge whichever way it was
    /// entered, and for `parent_of` `partner_id` is the parent.
    PartnerRelationshipSet {
        partner_id: String,
        related_partner_id: String,
        relationship: String,
    },
    /// A recorded family tie between two partners is removed.
    ///
    /// Its own event rather than a set-to-none: the pair either has a tie or does
    /// not, and a log that could not say a tie was *taken back* would keep
    /// attributing ownership from a marriage that ended.
    PartnerRelationshipCleared {
        partner_id: String,
        related_partner_id: String,
    },
    /// The Illinois IL-1065 settings for this book, set as a unit and replacing
    /// whatever was there — like [`BusinessProfileSet`](Self::BusinessProfileSet),
    /// because they are read together and the pair "apportions but no PTE" is one
    /// coherent position, not two independent toggles worth logging separately.
    Il1065SettingsSet(Box<Il1065SettingsData>),

    // The asset register (migration 030). Event-sourced like the partners, and
    // for the same reason: what the partnership owns and how it is being
    // depreciated is a fact the whole partnership files on, not a secret one
    // machine holds.
    /// An asset joins the register.
    DepreciableAssetAdded(Box<DepreciableAssetData>),
    /// An asset's details change — a cost corrected, a class reconsidered, a
    /// §179 election made or withdrawn.
    ///
    /// Carries the whole record rather than the changed field, so a reader of
    /// this one event knows the asset without replaying its history. The class
    /// in particular is worth changing after the fact: whether a fit-out is
    /// qualified improvement property or 39-year real property is a judgement
    /// people revise, and it is worth 24 years of recovery period.
    DepreciableAssetUpdated(Box<DepreciableAssetData>),
    /// An asset leaves the business.
    ///
    /// Its own event rather than an update with a date set, because a disposal is
    /// not a correction: it stops depreciation part-way through the year on the
    /// asset's own convention, and takes both the cost and the accumulated
    /// depreciation off Schedule L together.
    DepreciableAssetDisposed {
        asset_id: String,
        disposed_on: NaiveDate,
    },
    // --- sole proprietorships (migration 031) ---
    /// Which return these books file.
    ///
    /// Its own event rather than a field on
    /// [`BusinessProfileSet`](Self::BusinessProfileSet), because the two change
    /// on completely different occasions: the profile is edited when an address
    /// or an EIN changes, and the type is set once when the books are opened and
    /// essentially never again. Folding it in would mean re-stating the whole
    /// header to answer one question, and — worse — every profile edit would
    /// carry a business type the person editing was not thinking about.
    BusinessTypeSet {
        /// "partnership" or "sole_proprietorship" — see
        /// [`crate::domain::BusinessType`].
        business_type: String,
    },
    /// The owner of a sole proprietorship, set as a unit.
    SoleProprietorSet(Box<SoleProprietorData>),
    /// One Schedule C answer, for one tax year.
    ScheduleCAnswerSet {
        tax_year: i32,
        answer_key: String,
        value: String,
    },
    /// One Schedule C answer goes back to unanswered.
    ///
    /// Its own event rather than a set-to-empty, the same distinction
    /// [`ScheduleBAnswerCleared`](Self::ScheduleBAnswerCleared) draws: unanswered
    /// and "No" are different states on the form, and a log that could not tell
    /// them apart would replay one as the other.
    ScheduleCAnswerCleared {
        tax_year: i32,
        answer_key: String,
    },

    /// An asset is taken off the register entirely — entered in error, never
    /// owned. Distinct from a disposal, which is a real event in the world and
    /// leaves a gain or loss behind it.
    DepreciableAssetRemoved {
        asset_id: String,
    },
    /// One year's depreciation on one asset, fixed by hand — with the reason.
    ///
    /// For the year the register cannot reproduce: a return already filed on a
    /// figure the statute's tables do not give, which the books have to carry
    /// because the return did. Replaces that year's bonus and MACRS; §179 is an
    /// election of its own and is untouched. The note is required because an
    /// override with no reason cannot be told apart from a mistake.
    DepreciationOverrideSet {
        asset_id: String,
        tax_year: i32,
        amount_cents: i64,
        note: String,
    },
    /// The year goes back to what the register computes.
    DepreciationOverrideCleared {
        asset_id: String,
        tax_year: i32,
    },
    /// A change to an asset's basis after purchase — a grant that reimbursed it,
    /// say — in effect from a tax year, with the reason. Negative reduces the
    /// basis. See `domain::BasisAdjustment`.
    DepreciationBasisAdjusted {
        adjustment_id: String,
        asset_id: String,
        effective_year: i32,
        amount_cents: i64,
        note: String,
    },
    /// A basis adjustment entered in error comes back out.
    DepreciationBasisAdjustmentRemoved {
        adjustment_id: String,
        asset_id: String,
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

    // Fiscal Years
    FiscalYearOpened {
        year: i32,
        start_date: NaiveDate,
        end_date: NaiveDate,
    },
    /// The year's revenue and expense were swept to equity by
    /// `retained_earnings_entry_id`, and the year is now fenced against further
    /// posting. The two land together — see `closing_commands::close_books`.
    YearEndClosed {
        year: i32,
        retained_earnings_entry_id: String,
    },
    /// A closed year is opened again, so it can be corrected and re-closed.
    ///
    /// `reason` is required for the same purpose it serves everywhere else in
    /// this log: reopening a filed year is legitimate and is also what a mistake
    /// looks like, and the difference is only ever in someone's head until they
    /// write it down.
    YearEndReopened {
        year: i32,
        reason: String,
        reopened_by_user_id: String,
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
    /// How often a service's sales are totalled into the books, from when.
    ///
    /// An event rather than a setting, because the alternative silently
    /// double-counts revenue: a rollup's idempotency key carries its period, so
    /// two members syncing one service at different frequencies produce keys that
    /// do not collide and the same sales post twice. One value in the log means
    /// every member aggregates on the same boundary.
    ///
    /// `effective_from` because switching mid-period would re-total days already
    /// posted under a key that does not match them. What came before keeps the
    /// shape it was posted with.
    EventServiceReportingChanged {
        service_id: String,
        /// "per_event", "daily", "weekly", "monthly".
        frequency: String,
        effective_from: NaiveDate,
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
            Event::BusinessProfileSet(_) => "business_profile_set",
            Event::PartnerAdmitted(_) => "partner_admitted",
            Event::PartnerDetailsUpdated(_) => "partner_details_updated",
            Event::TaxLineMappingSet { .. } => "tax_line_mapping_set",
            Event::TaxLineMappingCleared { .. } => "tax_line_mapping_cleared",
            Event::PartnerSharesChanged { .. } => "partner_shares_changed",
            Event::PartnerEquityAccountLinked { .. } => "partner_equity_account_linked",
            Event::PartnerEquityAccountUnlinked { .. } => "partner_equity_account_unlinked",
            Event::PartnerAllocationFixed { .. } => "partner_allocation_fixed",
            Event::PartnerAllocationCleared { .. } => "partner_allocation_cleared",
            Event::LiabilityClassified { .. } => "liability_classified",
            Event::LiabilityClassificationCleared { .. } => "liability_classification_cleared",
            Event::AccountDeleted { .. } => "account_deleted",
            Event::TaxDeductionLimitSet { .. } => "tax_deduction_limit_set",
            Event::TaxDeductionLimitCleared { .. } => "tax_deduction_limit_cleared",
            Event::TaxStatementGroupingSet { .. } => "tax_statement_grouping_set",
            Event::IllinoisTaxAddbackSet { .. } => "illinois_tax_addback_set",
            Event::ScheduleBAnswerSet { .. } => "schedule_b_answer_set",
            Event::ScheduleBAnswerCleared { .. } => "schedule_b_answer_cleared",
            Event::PartnerWithdrawn { .. } => "partner_withdrawn",
            Event::PartnerRelationshipSet { .. } => "partner_relationship_set",
            Event::PartnerRelationshipCleared { .. } => "partner_relationship_cleared",
            Event::Il1065SettingsSet(_) => "il1065_settings_set",
            Event::DepreciableAssetAdded(_) => "depreciable_asset_added",
            Event::DepreciableAssetUpdated(_) => "depreciable_asset_updated",
            Event::DepreciableAssetDisposed { .. } => "depreciable_asset_disposed",
            Event::DepreciableAssetRemoved { .. } => "depreciable_asset_removed",
            Event::DepreciationOverrideSet { .. } => "depreciation_override_set",
            Event::DepreciationOverrideCleared { .. } => "depreciation_override_cleared",
            Event::DepreciationBasisAdjusted { .. } => "depreciation_basis_adjusted",
            Event::DepreciationBasisAdjustmentRemoved { .. } => {
                "depreciation_basis_adjustment_removed"
            }
            Event::BusinessTypeSet { .. } => "business_type_set",
            Event::SoleProprietorSet(_) => "sole_proprietor_set",
            Event::ScheduleCAnswerSet { .. } => "schedule_c_answer_set",
            Event::ScheduleCAnswerCleared { .. } => "schedule_c_answer_cleared",
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
            Event::YearEndClosed { .. } => "year_end_closed",
            Event::YearEndReopened { .. } => "year_end_reopened",
            Event::CurrencyEnabled { .. } => "currency_enabled",
            Event::ExchangeRateRecorded { .. } => "exchange_rate_recorded",
            Event::PlaidItemConnected { .. } => "plaid_item_connected",
            Event::PlaidAccountsRefreshed { .. } => "plaid_accounts_refreshed",
            Event::PlaidItemDisconnected { .. } => "plaid_item_disconnected",
            Event::PlaidAccountMapped { .. } => "plaid_account_mapped",
            Event::PlaidAccountUnmapped { .. } => "plaid_account_unmapped",
            Event::PlaidTransactionsSynced { .. } => "plaid_transactions_synced",
            Event::EventServiceRegistered { .. } => "event_service_registered",
            Event::EventServiceReportingChanged { .. } => "event_service_reporting_changed",
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
            Event::BusinessProfileSet(_) => None,
            Event::PartnerAdmitted(d) => Some(&d.partner_id),
            Event::PartnerDetailsUpdated(d) => Some(&d.partner_id),
            Event::TaxLineMappingSet { account_id, .. } => Some(account_id),
            Event::TaxLineMappingCleared { account_id, .. } => Some(account_id),
            Event::PartnerSharesChanged { partner_id, .. } => Some(partner_id),
            Event::PartnerEquityAccountLinked { partner_id, .. } => Some(partner_id),
            Event::PartnerEquityAccountUnlinked { partner_id, .. } => Some(partner_id),
            Event::PartnerAllocationFixed { partner_id, .. } => Some(partner_id),
            Event::PartnerAllocationCleared { partner_id, .. } => Some(partner_id),
            Event::LiabilityClassified { account_id, .. } => Some(account_id),
            Event::LiabilityClassificationCleared { account_id } => Some(account_id),
            Event::AccountDeleted { account_id } => Some(account_id),
            Event::TaxDeductionLimitSet { account_id, .. } => Some(account_id),
            Event::TaxDeductionLimitCleared { account_id, .. } => Some(account_id),
            Event::TaxStatementGroupingSet { account_id, .. } => Some(account_id),
            Event::IllinoisTaxAddbackSet { account_id, .. } => Some(account_id),
            // Keyed by (year, question), so no single id names the thing changed.
            Event::ScheduleBAnswerSet { .. } => None,
            Event::ScheduleBAnswerCleared { .. } => None,
            Event::PartnerWithdrawn { partner_id, .. } => Some(partner_id),
            Event::PartnerRelationshipSet { partner_id, .. } => Some(partner_id),
            Event::PartnerRelationshipCleared { partner_id, .. } => Some(partner_id),
            Event::Il1065SettingsSet(_) => None,
            Event::DepreciableAssetAdded(d) => Some(&d.asset_id),
            Event::DepreciableAssetUpdated(d) => Some(&d.asset_id),
            Event::DepreciableAssetDisposed { asset_id, .. } => Some(asset_id),
            Event::DepreciableAssetRemoved { asset_id } => Some(asset_id),
            Event::DepreciationOverrideSet { asset_id, .. } => Some(asset_id),
            Event::DepreciationOverrideCleared { asset_id, .. } => Some(asset_id),
            Event::DepreciationBasisAdjusted { asset_id, .. } => Some(asset_id),
            Event::DepreciationBasisAdjustmentRemoved { asset_id, .. } => Some(asset_id),
            // One business per book, so no id names the thing changed — the same
            // answer `BusinessProfileSet` gives.
            Event::BusinessTypeSet { .. } => None,
            Event::SoleProprietorSet(_) => None,
            // Keyed by (year, question), like the Schedule B answers.
            Event::ScheduleCAnswerSet { .. } => None,
            Event::ScheduleCAnswerCleared { .. } => None,
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
            Event::YearEndClosed { .. } => None,
            Event::YearEndReopened { .. } => None,
            Event::CurrencyEnabled { code, .. } => Some(code),
            Event::ExchangeRateRecorded { .. } => None,
            Event::PlaidItemConnected { item_id, .. } => Some(item_id),
            Event::PlaidAccountsRefreshed { item_id, .. } => Some(item_id),
            Event::PlaidItemDisconnected { item_id, .. } => Some(item_id),
            Event::PlaidAccountMapped { item_id, .. } => Some(item_id),
            Event::PlaidAccountUnmapped { item_id, .. } => Some(item_id),
            Event::PlaidTransactionsSynced { item_id, .. } => Some(item_id),
            Event::EventServiceRegistered { service_id, .. } => Some(service_id),
            Event::EventServiceReportingChanged { service_id, .. } => Some(service_id),
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

#[cfg(test)]
mod partnership_event_shape {
    use super::*;

    /// An enum is as big as its largest variant, and every event in the system
    /// pays for it. The partnership header inline made `Event` 296 bytes where
    /// it had been 160; boxed, it is back to the size the rest of the log sets.
    ///
    /// The bound is deliberately a bound and not the exact number — it is here
    /// to catch a variant that quietly doubles the enum, not to be updated
    /// every time a field moves.
    #[test]
    fn boxing_the_partnership_payloads_keeps_the_event_enum_small() {
        assert!(
            std::mem::size_of::<Event>() <= 176,
            "Event grew to {} bytes — box the payload of whichever variant did it",
            std::mem::size_of::<Event>()
        );
    }

    /// Boxing must not have changed what reaches the log.
    ///
    /// Serde's internally tagged representation flattens a newtype variant's
    /// struct into the same object a struct variant produces. If that ever
    /// stopped being true, every partnership event already written would stop
    /// deserializing — silently, since an event log is only read on replay.
    #[test]
    fn a_boxed_payload_serializes_flat_exactly_as_a_struct_variant_would() {
        let event = Event::PartnerWithdrawn {
            partner_id: "p1".into(),
            end_date: chrono::NaiveDate::from_ymd_opt(2025, 6, 30).unwrap(),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "partner_withdrawn");
        assert_eq!(json["partner_id"], "p1", "a struct variant is flat");

        let boxed = Event::BusinessProfileSet(Box::new(BusinessProfileData {
            legal_name: "Example LLC".into(),
            address: AddressData {
                street: "1 A St".into(),
                suite: None,
                city: "Town".into(),
                state: "TX".into(),
                postal_code: "78701".into(),
                country: None,
            },
            ein: "12-3456789".into(),
            naics_code: "541511".into(),
            formation_date: chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            principal_activity: None,
            principal_product: None,
        }));
        let json = serde_json::to_value(&boxed).unwrap();
        assert_eq!(json["type"], "business_profile_set");
        assert_eq!(
            json["legal_name"], "Example LLC",
            "the boxed payload must be flat too, not nested under a field"
        );
        assert_eq!(json["address"]["city"], "Town");
        assert!(
            json.get("principal_activity").is_none(),
            "absent options stay off the wire"
        );

        // And it reads back.
        let back: Event = serde_json::from_value(json).unwrap();
        match back {
            Event::BusinessProfileSet(d) => assert_eq!(d.ein, "12-3456789"),
            other => panic!("round-tripped into {other:?}"),
        }
    }

    /// An event written before assignments were dated re-serialises byte for byte.
    ///
    /// # Why this is load-bearing rather than tidy
    ///
    /// Replication does not trust the hash it is sent: it deserialises the
    /// event, re-serialises it, re-derives the hash and compares. So a field
    /// that appeared on the way back out — even one defaulted on the way in —
    /// would change the JSON of every mapping event ever written, and every one
    /// of them would fail verification on a replica.
    ///
    /// `skip_serializing_if` is what stops that, and it only holds while the new
    /// field is last in declaration order and the default is the value being
    /// skipped. Both are easy to break without noticing, which is what this
    /// pins.
    #[test]
    fn a_mapping_event_written_before_years_existed_reserialises_byte_for_byte() {
        for legacy in [
            r#"{"type":"tax_line_mapping_set","account_id":"6100","line_key":"l21"}"#,
            r#"{"type":"tax_line_mapping_cleared","account_id":"6100"}"#,
            r#"{"type":"tax_deduction_limit_set","account_id":"3055","deductible_pct":50}"#,
            r#"{"type":"tax_deduction_limit_cleared","account_id":"3055"}"#,
        ] {
            let event: Event = serde_json::from_str(legacy).expect("an old event still reads");
            let again = serde_json::to_string(&event).expect("and writes");
            assert_eq!(
                again, legacy,
                "re-serialising changed the bytes, so every historical event of this \
                 type would fail hash verification on a replica"
            );
        }
    }

    /// A dated event carries its year, so the two are told apart on the wire.
    #[test]
    fn a_dated_mapping_event_carries_its_year() {
        let event = Event::TaxLineMappingSet {
            account_id: "6100".into(),
            line_key: "l21".into(),
            effective_from: 2026,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""effective_from":2026"#), "{json}");

        // And it survives the round trip the replica makes.
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
    }
}
