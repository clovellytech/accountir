//! Keeping the asset register, and turning a year of it into a journal entry.
//!
//! # Why this posts to the ledger rather than overriding the return
//!
//! Every line on Form 1065 is filled from the ledger through
//! [`crate::tax::lines`]: an account is pointed at a line, and the line is the
//! sum of the accounts pointed at it. Depreciation could have been made an
//! exception — computed here and written straight onto line 16a — and it is
//! deliberately not.
//!
//! Instead [`post_year`] writes one ordinary journal entry, and the existing
//! mapping fills line 16a and Schedule L 9a/9b from the resulting balances with
//! no special case anywhere. The books and the return then cannot disagree,
//! because there is nothing for them to disagree about: one source, one path. An
//! override would have produced a return whose line 16a no trial balance
//! explains, and a set of books that quietly omitted the largest non-cash expense
//! the business has.
//!
//! # Re-running it
//!
//! Depreciation is computed late, corrected, and computed again — a class
//! reconsidered, a cost restated, an asset remembered in March. So the entry
//! carries [`reference_for`] as its idempotency key, and migration 014 makes at
//! most one *live* entry per reference at the database level. Re-posting a year
//! voids the previous entry and writes a new one under the same key, which the
//! partial index permits precisely because voiding frees the reference.
//!
//! The alternative — deleting and re-adding — would lose the audit trail of what
//! was filed before it was corrected. Voiding keeps it.
//!
//! # Where §179 goes, and where it must not
//!
//! Two debits come out of a year, not one. Ordinary depreciation (bonus plus
//! MACRS) goes to the asset's depreciation expense account, which is mapped to
//! page 1 line 16a. §179 goes to its own account, mapped to Schedule K line 12,
//! because the dollar limit and the taxable-income limit are applied on each
//! partner's return rather than here. Posting both to one account would deduct at
//! the partnership level what the statute deducts at the partner level, and
//! double-count it against the K-1 box 12 the partner also receives.
//!
//! `events::validation` refuses an asset that elects §179 without an account of
//! its own, so the broken state never reaches the register and this module never
//! has to guess.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::commands::entry_commands::{
    EntryCommands, EntryLine, PostEntryCommand, VoidEntryCommand,
};
use crate::domain::{
    BasisAdjustment, BonusElection, DepreciableAsset, DepreciationOverride, PropertyClass, System,
};
use crate::events::types::{DepreciableAssetData, Event, JournalEntrySource, StoredEvent};
use crate::store::event_store::EventStore;
use crate::tax::depreciation::{compute_year, YearSchedule};

#[derive(Debug, Error)]
pub enum DepreciationError {
    #[error("Store error: {0}")]
    Store(String),
    #[error("No asset with id {0}")]
    NoSuchAsset(String),
    #[error("Asset {0} was already disposed of")]
    AlreadyDisposed(String),
    #[error(
        "Asset {asset_id} was placed in service on {placed_in_service} and cannot be disposed of \
         on {disposed_on}, before that"
    )]
    DisposedBeforePlaced {
        asset_id: String,
        placed_in_service: NaiveDate,
        disposed_on: NaiveDate,
    },
    #[error("No account with id {0}")]
    NoSuchAccount(String),
    #[error(
        "Depreciation for {year} is already posted as entry {entry_id}. Post it again with \
         `replace` to void that entry and write a corrected one under the same reference."
    )]
    AlreadyPosted { year: i32, entry_id: String },
    #[error("Nothing to post: no asset in the register depreciates in {0}")]
    NothingToPost(i32),
    #[error("Invalid data: {0}")]
    Invalid(String),
    #[error("Could not post the entry: {0}")]
    Entry(String),
}

impl From<crate::store::event_store::EventStoreError> for DepreciationError {
    fn from(e: crate::store::event_store::EventStoreError) -> Self {
        DepreciationError::Store(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Reading the register
// ---------------------------------------------------------------------------

/// Every asset on the register, in the order a schedule reads best: oldest first,
/// because a depreciation schedule is read down the years.
///
/// A row this crate cannot parse — a class name from a later version, say — is
/// skipped rather than failing the read, the same choice
/// `list_relationships` makes. The cost is a missing asset on a schedule that
/// says how many it holds; the alternative is a register that will not open.
pub fn list_assets(conn: &Connection) -> Vec<DepreciableAsset> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, description, asset_account_id, expense_account_id,
                accumulated_account_id, section_179_account_id, acquired_on,
                placed_in_service, cost_cents, property_class, system,
                section_179_cents, bonus, disposed_on, notes
           FROM depreciable_assets
          ORDER BY placed_in_service, description",
    ) else {
        return Vec::new();
    };

    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, String>(9)?,
            r.get::<_, String>(10)?,
            r.get::<_, i64>(11)?,
            r.get::<_, String>(12)?,
            r.get::<_, Option<String>>(13)?,
            r.get::<_, Option<String>>(14)?,
        ))
    });

    let mut out = Vec::new();
    let Ok(rows) = rows else { return out };
    for row in rows.flatten() {
        let (
            asset_id,
            description,
            asset_account_id,
            expense_account_id,
            accumulated_account_id,
            section_179_account_id,
            acquired,
            placed,
            cost_cents,
            class,
            system,
            section_179_cents,
            bonus,
            disposed,
            notes,
        ) = row;

        let (Some(class), Some(system), Some(bonus)) = (
            PropertyClass::parse(&class),
            System::parse(&system),
            BonusElection::parse(&bonus),
        ) else {
            continue;
        };
        let (Some(acquired_on), Some(placed_in_service)) = (
            NaiveDate::parse_from_str(&acquired, "%Y-%m-%d").ok(),
            NaiveDate::parse_from_str(&placed, "%Y-%m-%d").ok(),
        ) else {
            continue;
        };

        out.push(DepreciableAsset {
            asset_id,
            description,
            asset_account_id,
            expense_account_id,
            accumulated_account_id,
            section_179_account_id,
            acquired_on,
            placed_in_service,
            cost_cents,
            class,
            system,
            section_179_cents,
            bonus,
            disposed_on: disposed.and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()),
            notes,
            overrides: Default::default(),
            basis_adjustments: Vec::new(),
        });
    }

    // The years fixed by hand, onto the assets they belong to. A row for an asset
    // no longer on the register cannot happen — removal deletes them — and would
    // be ignored if it did.
    if let Ok(mut stmt) =
        conn.prepare("SELECT asset_id, tax_year, amount_cents, note FROM depreciation_overrides")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        }) {
            for (asset_id, year, amount_cents, note) in rows.flatten() {
                if let Some(a) = out.iter_mut().find(|a| a.asset_id == asset_id) {
                    a.overrides
                        .insert(year, DepreciationOverride { amount_cents, note });
                }
            }
        }
    }

    // Basis adjustments, oldest first, onto their assets — removal deletes them
    // with the asset, as it does the overrides.
    if let Ok(mut stmt) = conn.prepare(
        "SELECT adjustment_id, asset_id, effective_year, amount_cents, note
           FROM depreciation_basis_adjustments ORDER BY effective_year, rowid",
    ) {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        }) {
            for (adjustment_id, asset_id, effective_year, amount_cents, note) in rows.flatten() {
                if let Some(a) = out.iter_mut().find(|a| a.asset_id == asset_id) {
                    a.basis_adjustments.push(BasisAdjustment {
                        adjustment_id,
                        effective_year,
                        amount_cents,
                        note,
                    });
                }
            }
        }
    }
    out
}

pub fn get_asset(conn: &Connection, asset_id: &str) -> Option<DepreciableAsset> {
    list_assets(conn)
        .into_iter()
        .find(|a| a.asset_id == asset_id)
}

// ---------------------------------------------------------------------------
// Changing the register
// ---------------------------------------------------------------------------

fn data_of(asset: &DepreciableAsset) -> DepreciableAssetData {
    DepreciableAssetData {
        asset_id: asset.asset_id.clone(),
        description: asset.description.clone(),
        asset_account_id: asset.asset_account_id.clone(),
        expense_account_id: asset.expense_account_id.clone(),
        accumulated_account_id: asset.accumulated_account_id.clone(),
        section_179_account_id: asset.section_179_account_id.clone(),
        acquired_on: asset.acquired_on,
        placed_in_service: asset.placed_in_service,
        cost_cents: asset.cost_cents,
        property_class: asset.class.as_str().to_string(),
        system: asset.system.as_str().to_string(),
        section_179_cents: asset.section_179_cents,
        bonus: asset.bonus.as_str().to_string(),
        notes: asset.notes.clone(),
    }
}

/// Whether every account the asset names actually exists.
///
/// Checked before the append rather than trusted, because the register's whole
/// job is to produce a journal entry later: an account id that names nothing
/// fails at posting time, months after the asset was entered and with no obvious
/// connection to it.
fn check_accounts(conn: &Connection, asset: &DepreciableAsset) -> Result<(), DepreciationError> {
    let mut ids = vec![
        &asset.asset_account_id,
        &asset.expense_account_id,
        &asset.accumulated_account_id,
    ];
    if let Some(a) = &asset.section_179_account_id {
        ids.push(a);
    }
    for id in ids {
        let exists: bool = conn
            .query_row("SELECT 1 FROM accounts WHERE id = ?1", [id], |_| Ok(true))
            .optional()
            .map_err(|e| DepreciationError::Store(e.to_string()))?
            .unwrap_or(false);
        if !exists {
            return Err(DepreciationError::NoSuchAccount(id.clone()));
        }
    }
    Ok(())
}

/// Put an asset on the register. Returns its new id.
///
/// The `asset_id` on the argument is ignored — one is minted here, so a caller
/// cannot collide with an existing asset by reusing an id they had lying around.
pub fn add_asset(
    store: &mut EventStore,
    user_id: &str,
    asset: &DepreciableAsset,
) -> Result<(String, StoredEvent), DepreciationError> {
    check_accounts(store.connection(), asset)?;

    let asset_id = Uuid::new_v4().to_string();
    let mut asset = asset.clone();
    asset.asset_id = asset_id.clone();

    let stored = append(
        store,
        user_id,
        Event::DepreciableAssetAdded(Box::new(data_of(&asset))),
    )?;
    Ok((asset_id, stored))
}

/// Correct an asset already on the register.
///
/// Carries the whole record, so this is also how a class is reconsidered — which
/// is the change most worth making after the fact, since whether a fit-out is
/// qualified improvement property or 39-year real property decides 24 years of
/// recovery period and whether bonus was available at all.
///
/// Does not touch the disposal date: [`dispose_asset`] owns that, because a
/// disposal is an event in the world and not a correction of one.
pub fn update_asset(
    store: &mut EventStore,
    user_id: &str,
    asset: &DepreciableAsset,
) -> Result<StoredEvent, DepreciationError> {
    if get_asset(store.connection(), &asset.asset_id).is_none() {
        return Err(DepreciationError::NoSuchAsset(asset.asset_id.clone()));
    }
    check_accounts(store.connection(), asset)?;
    append(
        store,
        user_id,
        Event::DepreciableAssetUpdated(Box::new(data_of(asset))),
    )
}

/// Record that an asset left the business.
pub fn dispose_asset(
    store: &mut EventStore,
    user_id: &str,
    asset_id: &str,
    disposed_on: NaiveDate,
) -> Result<StoredEvent, DepreciationError> {
    let Some(asset) = get_asset(store.connection(), asset_id) else {
        return Err(DepreciationError::NoSuchAsset(asset_id.to_string()));
    };
    if asset.disposed_on.is_some() {
        return Err(DepreciationError::AlreadyDisposed(asset_id.to_string()));
    }
    // Gone before it arrived is a typo, and one that would otherwise produce a
    // schedule with a negative holding period on it.
    if disposed_on < asset.placed_in_service {
        return Err(DepreciationError::DisposedBeforePlaced {
            asset_id: asset_id.to_string(),
            placed_in_service: asset.placed_in_service,
            disposed_on,
        });
    }
    append(
        store,
        user_id,
        Event::DepreciableAssetDisposed {
            asset_id: asset_id.to_string(),
            disposed_on,
        },
    )
}

/// Take an asset off the register entirely — entered in error, never owned.
///
/// Distinct from [`dispose_asset`]: a disposal happened and leaves a gain or loss
/// behind it, where a removal says the row should never have existed. Using one
/// for the other either invents a disposal or erases a real one.
pub fn remove_asset(
    store: &mut EventStore,
    user_id: &str,
    asset_id: &str,
) -> Result<StoredEvent, DepreciationError> {
    if get_asset(store.connection(), asset_id).is_none() {
        return Err(DepreciationError::NoSuchAsset(asset_id.to_string()));
    }
    append(
        store,
        user_id,
        Event::DepreciableAssetRemoved {
            asset_id: asset_id.to_string(),
        },
    )
}

/// Fix one year's depreciation on one asset by hand, with the reason.
///
/// For a year already filed on a figure the register cannot reproduce. The
/// amount replaces that year's bonus and MACRS everywhere the register is read —
/// the posting, Form 4562, and the accumulated depreciation later years build on
/// — so a year posted before the override reads as stale until it is posted
/// again, which is the prompt to do so.
pub fn set_override(
    store: &mut EventStore,
    user_id: &str,
    asset_id: &str,
    tax_year: i32,
    amount_cents: i64,
    note: &str,
) -> Result<StoredEvent, DepreciationError> {
    let Some(asset) = get_asset(store.connection(), asset_id) else {
        return Err(DepreciationError::NoSuchAsset(asset_id.to_string()));
    };
    let note = note.trim();
    if note.is_empty() {
        return Err(DepreciationError::Invalid(
            "an override needs a note saying why the computed figure is not the one to use"
                .to_string(),
        ));
    }
    if !(0..=asset.cost_cents).contains(&amount_cents) {
        return Err(DepreciationError::Invalid(format!(
            "a year's depreciation of ${:.2} is outside zero to the asset's cost of ${:.2}",
            amount_cents as f64 / 100.0,
            asset.cost_cents as f64 / 100.0
        )));
    }
    if asset.recovery_year(tax_year).is_none() {
        return Err(DepreciationError::Invalid(format!(
            "{} was not in service in {tax_year}, so there is no depreciation to fix",
            asset.description
        )));
    }
    append(
        store,
        user_id,
        Event::DepreciationOverrideSet {
            asset_id: asset_id.to_string(),
            tax_year,
            amount_cents,
            note: note.to_string(),
        },
    )
}

/// Put a year back to what the register computes.
pub fn clear_override(
    store: &mut EventStore,
    user_id: &str,
    asset_id: &str,
    tax_year: i32,
) -> Result<StoredEvent, DepreciationError> {
    if get_asset(store.connection(), asset_id).is_none() {
        return Err(DepreciationError::NoSuchAsset(asset_id.to_string()));
    }
    append(
        store,
        user_id,
        Event::DepreciationOverrideCleared {
            asset_id: asset_id.to_string(),
            tax_year,
        },
    )
}

/// Change an asset's basis after purchase, in effect from a tax year, with the
/// reason. Returns the adjustment's id.
///
/// Negative reduces the basis — a grant that reimbursed the cost, a rebate. From
/// `effective_year` on the register depreciates the adjusted basis over what is
/// left of the recovery period, and Form 4562 carries a statement saying so. A
/// year already posted reads as stale until it is posted again.
pub fn add_basis_adjustment(
    store: &mut EventStore,
    user_id: &str,
    asset_id: &str,
    effective_year: i32,
    amount_cents: i64,
    note: &str,
) -> Result<(String, StoredEvent), DepreciationError> {
    use chrono::Datelike;
    let Some(asset) = get_asset(store.connection(), asset_id) else {
        return Err(DepreciationError::NoSuchAsset(asset_id.to_string()));
    };
    let note = note.trim();
    if note.is_empty() {
        return Err(DepreciationError::Invalid(
            "a basis adjustment needs a note saying what changed the basis — a grant, a rebate, \
             a casualty"
                .to_string(),
        ));
    }
    if amount_cents == 0 {
        return Err(DepreciationError::Invalid(
            "an adjustment of $0.00 changes nothing".to_string(),
        ));
    }
    let placed = asset.placed_in_service.year();
    if effective_year < placed {
        return Err(DepreciationError::Invalid(format!(
            "{} was placed in service in {placed}, so its basis cannot change in {effective_year}",
            asset.description
        )));
    }
    if let Some(gone) = asset.disposed_on.filter(|d| d.year() < effective_year) {
        return Err(DepreciationError::Invalid(format!(
            "{} was disposed of on {gone}, before {effective_year}",
            asset.description
        )));
    }
    // Never below zero at any point, with this adjustment and those already made.
    let mut trial = asset.clone();
    trial.basis_adjustments.push(BasisAdjustment {
        adjustment_id: String::new(),
        effective_year,
        amount_cents,
        note: note.to_string(),
    });
    let lowest = trial
        .basis_adjustments
        .iter()
        .map(|a| trial.adjusted_cost_through(a.effective_year))
        .min()
        .unwrap_or(trial.cost_cents);
    if lowest < 0 {
        return Err(DepreciationError::Invalid(format!(
            "that would take the basis of {} below zero, to ${:.2}",
            asset.description,
            lowest as f64 / 100.0
        )));
    }

    let adjustment_id = Uuid::new_v4().to_string();
    let stored = append(
        store,
        user_id,
        Event::DepreciationBasisAdjusted {
            adjustment_id: adjustment_id.clone(),
            asset_id: asset_id.to_string(),
            effective_year,
            amount_cents,
            note: note.to_string(),
        },
    )?;
    Ok((adjustment_id, stored))
}

/// Take a basis adjustment entered in error back out.
pub fn remove_basis_adjustment(
    store: &mut EventStore,
    user_id: &str,
    adjustment_id: &str,
) -> Result<StoredEvent, DepreciationError> {
    let Some(asset) = list_assets(store.connection()).into_iter().find(|a| {
        a.basis_adjustments
            .iter()
            .any(|b| b.adjustment_id == adjustment_id)
    }) else {
        return Err(DepreciationError::Invalid(format!(
            "no basis adjustment with id {adjustment_id}"
        )));
    };
    append(
        store,
        user_id,
        Event::DepreciationBasisAdjustmentRemoved {
            adjustment_id: adjustment_id.to_string(),
            asset_id: asset.asset_id,
        },
    )
}

fn append(
    store: &mut EventStore,
    user_id: &str,
    event: Event,
) -> Result<StoredEvent, DepreciationError> {
    crate::commands::partnership_commands::append_event_locally(store, user_id, event)
        .map_err(|e| DepreciationError::Store(e.to_string()))
}

// ---------------------------------------------------------------------------
// Posting a year to the ledger
// ---------------------------------------------------------------------------

/// The idempotency key a year's depreciation entry carries.
///
/// One per tax year, stable across recomputation, so migration 014's partial
/// unique index is what actually stops a year being posted twice — not a check
/// this module remembers to make.
pub fn reference_for(year: i32) -> String {
    format!("depreciation-{year}")
}

/// The live journal entry holding this year's depreciation, if one is posted.
pub fn posted_entry_for(conn: &Connection, year: i32) -> Option<String> {
    conn.query_row(
        "SELECT id FROM journal_entries WHERE reference = ?1 AND is_void = 0",
        [reference_for(year)],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// What a post did.
#[derive(Debug, Clone)]
pub struct Posted {
    pub entry_id: String,
    /// The entry voided to make room, when this replaced an earlier posting.
    pub replaced: Option<String>,
    /// Bonus plus MACRS — the debit that reaches page 1 line 16a.
    pub depreciation_cents: i64,
    /// §179 — the debit that reaches Schedule K line 12, and must not reach 16a.
    pub section_179_cents: i64,
}

/// Compute a tax year and post it to the ledger as one journal entry.
///
/// Dated the last day of the year, which is what makes the closed-period fence in
/// `check_entry_invariants_in_txn` meaningful here: depreciation is a year-end
/// entry, and a closed year is exactly the case where it should be refused rather
/// than quietly landed.
///
/// `replace` voids an existing posting for the year first. Without it a year
/// already posted is refused by [`DepreciationError::AlreadyPosted`], which names
/// the entry so the caller can look at what is about to be discarded.
pub fn post_year(
    store: &mut EventStore,
    user_id: &str,
    year: i32,
    replace: bool,
) -> Result<Posted, DepreciationError> {
    let existing = posted_entry_for(store.connection(), year);
    if let Some(entry_id) = &existing {
        if !replace {
            return Err(DepreciationError::AlreadyPosted {
                year,
                entry_id: entry_id.clone(),
            });
        }
    }

    let assets = list_assets(store.connection());
    let schedule = compute_year(&assets, year);
    let lines = entry_lines(&schedule, &base_currency(store.connection()));

    if lines.is_empty() {
        return Err(DepreciationError::NothingToPost(year));
    }

    // Void first, and only once there is something to replace it with: a failure
    // between the two would otherwise leave the year with no entry at all, which
    // is worse than the stale one it was replacing.
    let replaced = match existing {
        Some(entry_id) => {
            EntryCommands::new(store, user_id.to_string())
                .void_entry(VoidEntryCommand {
                    entry_id: entry_id.clone(),
                    reason: format!("Superseded by a recomputed {year} depreciation entry"),
                })
                .map_err(|e| DepreciationError::Entry(e.to_string()))?;
            Some(entry_id)
        }
        None => None,
    };

    let date = NaiveDate::from_ymd_opt(year, 12, 31).expect("31 December exists in every year");
    let stored = EntryCommands::new(store, user_id.to_string())
        .post_entry(PostEntryCommand {
            date,
            memo: memo_for(&schedule),
            lines,
            reference: Some(reference_for(year)),
            source: Some(JournalEntrySource::System),
        })
        .map_err(|e| DepreciationError::Entry(e.to_string()))?;

    // Read off the event rather than minted here, so the id this reports is the
    // one the ledger actually holds.
    let entry_id = match &stored.event {
        Event::JournalEntryPosted { entry_id, .. } => entry_id.clone(),
        other => {
            return Err(DepreciationError::Entry(format!(
                "posting produced a {} rather than a journal entry",
                other.event_type()
            )));
        }
    };

    Ok(Posted {
        entry_id,
        replaced,
        depreciation_cents: schedule.line_16a_cents(),
        section_179_cents: schedule.section_179_cents(),
    })
}

/// The entry's lines: a debit per expense account and a credit per accumulated
/// account, netted across the assets that share one.
///
/// Grouped rather than one line per asset because a register of forty assets
/// against three accounts should post three debits, not forty. The asset detail
/// lives on the depreciation schedule, which is where somebody looks for it; a
/// journal entry with forty lines is not a better record, only a longer one.
///
/// §179 is kept in its own group throughout — see the module docs.
fn entry_lines(schedule: &YearSchedule<'_>, currency: &str) -> Vec<EntryLine> {
    let mut debits: BTreeMap<&str, i64> = BTreeMap::new();
    let mut credits: BTreeMap<&str, i64> = BTreeMap::new();

    for row in &schedule.rows {
        let ordinary = row.bonus_cents + row.macrs_cents;
        if ordinary != 0 {
            *debits.entry(&row.asset.expense_account_id).or_default() += ordinary;
            *credits
                .entry(&row.asset.accumulated_account_id)
                .or_default() += ordinary;
        }
        if row.section_179_cents != 0 {
            // Validation guarantees this account exists once §179 is elected, so
            // an asset without one here has no election to post.
            if let Some(account) = row.asset.section_179_account_id.as_deref() {
                *debits.entry(account).or_default() += row.section_179_cents;
                *credits
                    .entry(&row.asset.accumulated_account_id)
                    .or_default() += row.section_179_cents;
            }
        }
    }

    let mut lines = Vec::new();
    for (account, amount) in debits {
        if amount != 0 {
            lines.push(EntryLine::debit(account, amount, currency));
        }
    }
    for (account, amount) in credits {
        if amount != 0 {
            lines.push(EntryLine::credit(account, amount, currency));
        }
    }
    lines
}

fn memo_for(schedule: &YearSchedule<'_>) -> String {
    let held = schedule.rows.len();
    let mut memo = format!(
        "Depreciation for {} — {held} asset(s), computed from the register",
        schedule.tax_year
    );
    if schedule.section_179_cents() != 0 {
        memo.push_str(" (including §179, posted separately for Schedule K line 12)");
    }
    memo
}

fn base_currency(conn: &Connection) -> String {
    conn.query_row("SELECT base_currency FROM company LIMIT 1", [], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
    .unwrap_or_else(|| "USD".to_string())
}

/// Whether the ledger and the register agree about a year, for the desktop to
/// show beside the Post button.
///
/// Posting is the only thing that makes them agree, so a register edited after a
/// posting leaves a return built on the earlier figures. Reported rather than
/// re-posted automatically: the entry may sit in a closed period, and rewriting
/// somebody's books without being asked is not a thing to do quietly.
pub fn posting_is_stale(conn: &Connection, year: i32) -> Option<String> {
    let entry_id = posted_entry_for(conn, year)?;

    let posted: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM journal_lines
              WHERE entry_id = ?1 AND amount > 0",
            [&entry_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
        .unwrap_or(0);

    let assets = list_assets(conn);
    let schedule = compute_year(&assets, year);
    let computed = schedule.total_cents();

    (posted != computed).then(|| {
        format!(
            "The {year} depreciation entry posts ${:.2}, but the register now computes ${:.2}. \
             The register has changed since it was posted — post {year} again to bring the \
             books back in line, or the return is built on the earlier figures.",
            posted as f64 / 100.0,
            computed as f64 / 100.0,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountType, PropertyClass};
    use crate::store::event_store::EventStore;
    use rusqlite::params;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn store() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        crate::store::migrations::SchemaStore::init_schema(&mut store).unwrap();
        crate::commands::partnership_commands::append_event_locally(
            &mut store,
            "u",
            Event::CompanyCreated {
                company_id: "c".into(),
                name: "Bunny Ears".into(),
                base_currency: "USD".into(),
                fiscal_year_start: 1,
            },
        )
        .expect("company");
        for (id, number, name, kind) in [
            ("1500", "1500", "Studio equipment", AccountType::Asset),
            (
                "1590",
                "1590",
                "Accumulated depreciation",
                AccountType::Asset,
            ),
            ("6500", "6500", "Depreciation expense", AccountType::Expense),
            ("6501", "6501", "Section 179 expense", AccountType::Expense),
        ] {
            crate::commands::partnership_commands::append_event_locally(
                &mut store,
                "u",
                Event::AccountCreated {
                    account_id: id.into(),
                    account_type: kind.into(),
                    account_number: number.into(),
                    name: name.into(),
                    parent_id: None,
                    currency: Some("USD".into()),
                    description: None,
                },
            )
            .expect("account");
        }
        store
    }

    fn kiln() -> DepreciableAsset {
        DepreciableAsset {
            asset_id: String::new(),
            description: "Kiln".into(),
            asset_account_id: "1500".into(),
            expense_account_id: "6500".into(),
            accumulated_account_id: "1590".into(),
            section_179_account_id: None,
            acquired_on: day(2025, 3, 1),
            placed_in_service: day(2025, 3, 1),
            cost_cents: 1_000_000,
            class: PropertyClass::SevenYear,
            system: System::Gds,
            section_179_cents: 0,
            bonus: BonusElection::Decline,
            disposed_on: None,
            notes: None,
            overrides: Default::default(),
            basis_adjustments: Vec::new(),
        }
    }

    /// An override is what gets posted, the reason travels with it, and clearing
    /// it leaves the posting visibly out of date.
    #[test]
    fn an_override_is_what_gets_posted_and_clearing_it_restores_the_computed_year() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        let computed = compute_year(&list_assets(s.connection()), 2025).line_16a_cents();

        assert!(
            set_override(&mut s, "u", &id, 2025, 50_000, "  ").is_err(),
            "a note is required"
        );
        assert!(
            set_override(&mut s, "u", &id, 2024, 50_000, "before it existed").is_err(),
            "not in service in 2024"
        );
        set_override(
            &mut s,
            "u",
            &id,
            2025,
            50_000,
            "As filed on the 2025 return",
        )
        .expect("set");
        let asset = get_asset(s.connection(), &id).unwrap();
        assert_eq!(
            asset.overrides.get(&2025).map(|o| o.note.as_str()),
            Some("As filed on the 2025 return")
        );

        let posted = post_year(&mut s, "u", 2025, false).expect("posted");
        assert_eq!(posted.depreciation_cents, 50_000);
        assert!(posting_is_stale(s.connection(), 2025).is_none());

        clear_override(&mut s, "u", &id, 2025).expect("cleared");
        assert!(
            posting_is_stale(s.connection(), 2025).is_some(),
            "the posting now disagrees with the register"
        );
        assert_eq!(
            compute_year(&list_assets(s.connection()), 2025).line_16a_cents(),
            computed
        );
    }

    #[test]
    fn removing_an_asset_takes_its_overrides_with_it() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        set_override(&mut s, "u", &id, 2025, 1, "test").unwrap();
        remove_asset(&mut s, "u", &id).unwrap();
        let n: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM depreciation_overrides", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn an_asset_round_trips_through_the_log() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");

        let back = get_asset(s.connection(), &id).expect("on the register");
        assert_eq!(back.description, "Kiln");
        assert_eq!(back.class, PropertyClass::SevenYear);
        assert_eq!(back.cost_cents, 1_000_000);
        assert_eq!(back.placed_in_service, day(2025, 3, 1));
        assert!(back.disposed_on.is_none());
    }

    /// The change most worth making after the fact, and the one the model exists
    /// for: a fit-out reclassed from 39-year real property to QIP.
    #[test]
    fn reclassifying_an_asset_changes_its_whole_schedule() {
        let mut s = store();
        let mut fitout = kiln();
        fitout.description = "Studio fit-out".into();
        fitout.class = PropertyClass::Nonresidential;
        let (id, _) = add_asset(&mut s, "u", &fitout).expect("added");

        let before = compute_year(&list_assets(s.connection()), 2025).line_16a_cents();

        let mut asset = get_asset(s.connection(), &id).unwrap();
        asset.class = PropertyClass::QualifiedImprovement;
        update_asset(&mut s, "u", &asset).expect("reclassified");

        let after = compute_year(&list_assets(s.connection()), 2025).line_16a_cents();
        assert!(after > before, "15 years beats 39: {before} then {after}");
        assert_eq!(
            get_asset(s.connection(), &id).unwrap().class,
            PropertyClass::QualifiedImprovement
        );
    }

    #[test]
    fn an_account_that_names_nothing_is_refused_before_it_reaches_the_log() {
        let mut s = store();
        let mut a = kiln();
        a.expense_account_id = "9999".into();
        assert!(matches!(
            add_asset(&mut s, "u", &a),
            Err(DepreciationError::NoSuchAccount(_))
        ));
        assert!(list_assets(s.connection()).is_empty());
    }

    /// §179 sharing the depreciation expense account would land it on page 1 line
    /// 16a, which is the one place it must never be.
    #[test]
    fn section_179_without_its_own_account_is_refused() {
        let mut s = store();
        let mut a = kiln();
        a.section_179_cents = 500_000;
        assert!(add_asset(&mut s, "u", &a).is_err(), "no §179 account");

        a.section_179_account_id = Some("6500".into());
        assert!(
            add_asset(&mut s, "u", &a).is_err(),
            "same as the expense account"
        );

        a.section_179_account_id = Some("6501".into());
        assert!(add_asset(&mut s, "u", &a).is_ok());
    }

    // --- posting ---

    #[test]
    fn posting_a_year_writes_one_balanced_entry() {
        let mut s = store();
        add_asset(&mut s, "u", &kiln()).expect("added");

        let posted = post_year(&mut s, "u", 2025, false).expect("posted");
        assert!(posted.replaced.is_none());
        // 7-year, half-year, first year: 14.29% of 1,000,000 cents.
        assert_eq!(posted.depreciation_cents, 142_857);
        assert_eq!(posted.section_179_cents, 0);

        let (debits, credits): (i64, i64) = s
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(CASE WHEN amount > 0 THEN amount END), 0),
                        COALESCE(SUM(CASE WHEN amount < 0 THEN -amount END), 0)
                   FROM journal_lines WHERE entry_id = ?1",
                [&posted.entry_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("lines");
        assert_eq!(debits, 142_857);
        assert_eq!(debits, credits, "the entry balances");
    }

    #[test]
    fn the_entry_is_dated_the_last_day_of_the_tax_year() {
        let mut s = store();
        add_asset(&mut s, "u", &kiln()).expect("added");
        let posted = post_year(&mut s, "u", 2025, false).expect("posted");

        let date: String = s
            .connection()
            .query_row(
                "SELECT date FROM journal_entries WHERE id = ?1",
                [&posted.entry_id],
                |r| r.get(0),
            )
            .expect("entry");
        assert_eq!(date, "2025-12-31");
    }

    /// The idempotency key is what stops a year being posted twice, and it is
    /// enforced by the database rather than remembered here.
    #[test]
    fn a_year_already_posted_is_refused_rather_than_posted_again() {
        let mut s = store();
        add_asset(&mut s, "u", &kiln()).expect("added");
        post_year(&mut s, "u", 2025, false).expect("posted");

        match post_year(&mut s, "u", 2025, false) {
            Err(DepreciationError::AlreadyPosted { year, .. }) => assert_eq!(year, 2025),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// Re-posting voids the old entry and writes a new one under the same
    /// reference — which the partial unique index permits precisely because
    /// voiding frees it.
    #[test]
    fn reposting_replaces_the_entry_and_keeps_the_reference() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        let first = post_year(&mut s, "u", 2025, false).expect("posted");

        // The cost was wrong; correct it and post again.
        let mut asset = get_asset(s.connection(), &id).unwrap();
        asset.cost_cents = 2_000_000;
        update_asset(&mut s, "u", &asset).expect("corrected");

        let second = post_year(&mut s, "u", 2025, true).expect("reposted");
        assert_eq!(second.replaced.as_deref(), Some(first.entry_id.as_str()));
        assert_ne!(second.entry_id, first.entry_id);
        assert_eq!(second.depreciation_cents, 285_714);

        // Exactly one live entry holds the reference, and it is the new one.
        assert_eq!(
            posted_entry_for(s.connection(), 2025).as_deref(),
            Some(second.entry_id.as_str())
        );
        let live: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM journal_entries WHERE reference = ?1 AND is_void = 0",
                [reference_for(2025)],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(live, 1, "the voided one no longer holds the key");
    }

    /// §179 and ordinary depreciation reach different accounts, because they
    /// reach different lines of the return.
    #[test]
    fn section_179_posts_to_its_own_account_and_not_the_depreciation_one() {
        let mut s = store();
        let mut a = kiln();
        a.section_179_cents = 400_000;
        a.section_179_account_id = Some("6501".into());
        add_asset(&mut s, "u", &a).expect("added");

        let posted = post_year(&mut s, "u", 2025, false).expect("posted");
        assert_eq!(posted.section_179_cents, 400_000);

        let amount_on = |account: &str| -> i64 {
            s.connection()
                .query_row(
                    "SELECT COALESCE(SUM(amount), 0) FROM journal_lines
                      WHERE entry_id = ?1 AND account_id = ?2",
                    params![&posted.entry_id, account],
                    |r| r.get(0),
                )
                .expect("sum")
        };

        assert_eq!(amount_on("6501"), 400_000, "§179 on its own account");
        assert_eq!(
            amount_on("6500"),
            posted.depreciation_cents,
            "and nothing of it on the depreciation account"
        );
        assert_eq!(amount_on("1590"), -(400_000 + posted.depreciation_cents));
    }

    #[test]
    fn an_empty_register_posts_nothing_rather_than_an_empty_entry() {
        let mut s = store();
        assert!(matches!(
            post_year(&mut s, "u", 2025, false),
            Err(DepreciationError::NothingToPost(2025))
        ));
    }

    /// Assets sharing an account post one line between them, not one each.
    #[test]
    fn assets_sharing_an_account_are_netted_into_one_line() {
        let mut s = store();
        let mut wheel = kiln();
        wheel.description = "Pottery wheel".into();
        add_asset(&mut s, "u", &kiln()).expect("added");
        add_asset(&mut s, "u", &wheel).expect("added");

        let posted = post_year(&mut s, "u", 2025, false).expect("posted");
        let lines: i64 = s
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM journal_lines WHERE entry_id = ?1",
                [&posted.entry_id],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(lines, 2, "one debit and one credit for two assets");
        assert_eq!(posted.depreciation_cents, 285_714);
    }

    // --- staleness ---

    #[test]
    fn a_register_edited_after_posting_is_reported_as_stale() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        post_year(&mut s, "u", 2025, false).expect("posted");
        assert!(posting_is_stale(s.connection(), 2025).is_none());

        let mut asset = get_asset(s.connection(), &id).unwrap();
        asset.cost_cents = 2_000_000;
        update_asset(&mut s, "u", &asset).expect("corrected");

        let stale = posting_is_stale(s.connection(), 2025).expect("stale now");
        assert!(
            stale.contains("post 2025 again") || stale.contains("post the"),
            "{stale}"
        );

        post_year(&mut s, "u", 2025, true).expect("reposted");
        assert!(posting_is_stale(s.connection(), 2025).is_none());
    }

    #[test]
    fn a_year_never_posted_is_not_stale() {
        let mut s = store();
        add_asset(&mut s, "u", &kiln()).expect("added");
        assert!(posting_is_stale(s.connection(), 2025).is_none());
    }

    // --- disposal ---

    #[test]
    fn disposing_of_an_asset_stops_it_depreciating_the_year_after() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        dispose_asset(&mut s, "u", &id, day(2026, 5, 1)).expect("disposed");

        let assets = list_assets(s.connection());
        assert_eq!(assets[0].disposed_on, Some(day(2026, 5, 1)));
        assert!(compute_year(&assets, 2027).rows.is_empty());
    }

    #[test]
    fn a_disposal_before_the_asset_arrived_is_refused() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        assert!(matches!(
            dispose_asset(&mut s, "u", &id, day(2024, 1, 1)),
            Err(DepreciationError::DisposedBeforePlaced { .. })
        ));
    }

    #[test]
    fn an_asset_cannot_be_disposed_of_twice() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        dispose_asset(&mut s, "u", &id, day(2026, 5, 1)).expect("disposed");
        assert!(matches!(
            dispose_asset(&mut s, "u", &id, day(2026, 7, 1)),
            Err(DepreciationError::AlreadyDisposed(_))
        ));
    }

    #[test]
    fn removing_an_asset_takes_it_off_the_register() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        remove_asset(&mut s, "u", &id).expect("removed");
        assert!(list_assets(s.connection()).is_empty());
        assert!(matches!(
            remove_asset(&mut s, "u", &id),
            Err(DepreciationError::NoSuchAsset(_))
        ));
    }

    /// A basis reduced after purchase reprices the years after it, needs a
    /// reason, cannot go below zero or before the asset existed, and comes back
    /// out cleanly.
    #[test]
    fn a_basis_adjustment_reprices_later_years_and_can_be_removed() {
        let mut s = store();
        let (id, _) = add_asset(&mut s, "u", &kiln()).expect("added");
        let before = compute_year(&list_assets(s.connection()), 2027).line_16a_cents();

        assert!(
            add_basis_adjustment(&mut s, "u", &id, 2026, -500_000, " ").is_err(),
            "a note is required"
        );
        assert!(
            add_basis_adjustment(&mut s, "u", &id, 2024, -500_000, "grant").is_err(),
            "not in service in 2024"
        );
        assert!(
            add_basis_adjustment(&mut s, "u", &id, 2026, -2_000_000, "grant").is_err(),
            "below zero"
        );
        let (adjustment, _) =
            add_basis_adjustment(&mut s, "u", &id, 2026, -500_000, "City grant reimbursed half")
                .expect("adjusted");
        let assets = list_assets(s.connection());
        assert_eq!(assets[0].basis_adjustments.len(), 1);
        assert_eq!(assets[0].adjusted_cost_through(2026), 500_000);
        let after = compute_year(&assets, 2027).line_16a_cents();
        assert!(after < before, "{after} is not below {before}");

        remove_basis_adjustment(&mut s, "u", &adjustment).expect("removed");
        assert!(list_assets(s.connection())[0].basis_adjustments.is_empty());
        assert_eq!(
            compute_year(&list_assets(s.connection()), 2027).line_16a_cents(),
            before
        );
    }
}
