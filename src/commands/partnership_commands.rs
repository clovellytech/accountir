//! Recording who the partnership is and who its partners are.
//!
//! # Where a taxpayer identification number lives
//!
//! Everything here goes into the event log except one field. A partner's TIN is
//! written to `partner_tins`, an ordinary local table, and never to an event.
//!
//! The log is replicated in full to every member's laptop and is append-only, so
//! an SSN written into it is that SSN on every other partner's machine forever,
//! with no way to take it back. The number is needed only where a return is
//! actually prepared. `main` reached the same conclusion about event-service API
//! keys in migration 020; this is that decision applied to something rather more
//! sensitive than an API key.
//!
//! The cost is real and worth naming: TINs do not sync. A member who has not
//! entered one locally generates a K-1 with item E blank, which is a form you can
//! see is incomplete — rather than one carrying a number that reached them by a
//! route nobody intended.

use crate::domain::{
    Address, BusinessProfile, Partner, PartnerType, Residency, Shares, is_valid_tin,
};
use crate::events::types::{
    AddressData, BusinessProfileData, Event, EventEnvelope, PartnerAdmittedData, PartnerDetailsData,
    ShareData, StoredEvent,
};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum PartnershipError {
    #[error("Store error: {0}")]
    StoreError(String),
    #[error("No partner with id {0}")]
    NoSuchPartner(String),
    #[error("Partner {0} has already left")]
    AlreadyWithdrawn(String),
    #[error("Partner {partner_id} joined on {start_date} and cannot leave on {end_date}, before that")]
    LeftBeforeJoining {
        partner_id: String,
        start_date: NaiveDate,
        end_date: NaiveDate,
    },
    #[error("A partner with id {0} already exists")]
    PartnerExists(String),
    #[error("The partnership's details have not been set yet")]
    NoProfile,
    #[error(
        "Partner {partner_id} was admitted, but their TIN could not be stored ({reason}). \
         They exist — set the TIN again rather than admitting them a second time."
    )]
    TinNotStored { partner_id: String, reason: String },
    #[error("Invalid data: {0}")]
    InvalidData(String),
}

impl From<EventStoreError> for PartnershipError {
    fn from(e: EventStoreError) -> Self {
        PartnershipError::StoreError(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Conversions between the log's shapes and the domain's
// ---------------------------------------------------------------------------

impl From<&Address> for AddressData {
    fn from(a: &Address) -> Self {
        AddressData {
            street: a.street.clone(),
            suite: a.suite.clone(),
            city: a.city.clone(),
            state: a.state.clone(),
            postal_code: a.postal_code.clone(),
            country: a.country.clone(),
        }
    }
}

impl From<&AddressData> for Address {
    fn from(a: &AddressData) -> Self {
        Address {
            street: a.street.clone(),
            suite: a.suite.clone(),
            city: a.city.clone(),
            state: a.state.clone(),
            postal_code: a.postal_code.clone(),
            country: a.country.clone(),
        }
    }
}

impl From<Shares> for ShareData {
    fn from(s: Shares) -> Self {
        ShareData {
            profit_ppm: s.profit_ppm,
            loss_ppm: s.loss_ppm,
            capital_ppm: s.capital_ppm,
        }
    }
}

impl From<ShareData> for Shares {
    fn from(s: ShareData) -> Self {
        Shares {
            profit_ppm: s.profit_ppm,
            loss_ppm: s.loss_ppm,
            capital_ppm: s.capital_ppm,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared invariants — one set of rules, both writer paths
// ---------------------------------------------------------------------------
//
// Standalone books append through `append_checked_locally`; group-hosted books
// append through `sync/commands/partnership.rs`. Both call the `build_*_in_txn`
// functions below, so there is exactly one statement of each rule and no way for
// the two paths to drift into disagreeing about what is allowed. This is the
// convention `bill_commands.rs` established with `build_receive_bill_in_txn` /
// `check_receive_bill_pure`.

/// The result of checking a partnership command against write-locked state.
pub(crate) enum PartnerStep {
    /// The invariants hold; append this event. The caller wraps it in an
    /// envelope — the local path stamps its user, the sync path the
    /// authenticated actor.
    Append(Event),
    /// A domain invariant was violated. `422` on the sync path.
    Reject(PartnershipError),
}

/// State-independent validation of the partnership header.
///
/// Run before the transaction opens. `validate_event` inside the append checks
/// the same shapes, but reaching it means an `EventStoreError` and a `500` where
/// the truth is that the caller sent a malformed EIN — so it is checked here,
/// where the answer can be a `422`.
pub(crate) fn check_set_profile_pure(profile: &BusinessProfile) -> Result<(), PartnershipError> {
    if profile.legal_name.trim().is_empty() {
        return Err(PartnershipError::InvalidData(
            "the partnership's legal name is required".to_string(),
        ));
    }
    if !crate::domain::is_valid_ein(profile.ein.trim()) {
        return Err(PartnershipError::InvalidData(format!(
            "{:?} is not an EIN (NN-NNNNNNN)",
            profile.ein
        )));
    }
    if !crate::domain::is_valid_naics(profile.naics_code.trim()) {
        return Err(PartnershipError::InvalidData(format!(
            "{:?} is not a six-digit NAICS code",
            profile.naics_code
        )));
    }
    if profile.address.street.trim().is_empty() || profile.address.city.trim().is_empty() {
        return Err(PartnershipError::InvalidData(
            "a street and a city are required".to_string(),
        ));
    }
    Ok(())
}

/// Build the header event. No state-dependent check: the row is keyed
/// `'default'` by a CHECK constraint, so there is one header and setting it
/// again replaces it.
pub(crate) fn build_set_profile_event(profile: &BusinessProfile) -> Event {
    Event::BusinessProfileSet(Box::new(BusinessProfileData {
        legal_name: profile.legal_name.trim().to_string(),
        address: (&profile.address).into(),
        ein: profile.ein.trim().to_string(),
        naics_code: profile.naics_code.trim().to_string(),
        formation_date: profile.formation_date,
        principal_activity: profile.principal_activity.clone(),
        principal_product: profile.principal_product.clone(),
    }))
}

/// State-independent validation for admitting a partner.
///
/// The TIN is checked here and then goes no further than this machine — it is
/// never a field on any event, so the sync path never sees one to validate.
pub(crate) fn check_admit_partner_pure(cmd: &AdmitPartner) -> Result<(), PartnershipError> {
    check_partner_fields(&cmd.name, &cmd.entity_type, cmd.shares)?;
    if let Some(tin) = cmd.tin.as_deref().filter(|t| !t.trim().is_empty()) {
        if !is_valid_tin(tin.trim()) {
            // The value is deliberately absent from the message. A mistyped TIN
            // is usually a nearly-correct one, and this text reaches terminal
            // scrollback, log files and the desktop's error bar — every place
            // the number was kept out of the event log to avoid.
            return Err(PartnershipError::InvalidData(
                "that is not an SSN (NNN-NN-NNNN) or an EIN (NN-NNNNNNN)".to_string(),
            ));
        }
    }
    Ok(())
}

/// State-independent validation for editing a partner.
pub(crate) fn check_update_partner_pure(cmd: &UpdatePartner) -> Result<(), PartnershipError> {
    check_partner_fields(&cmd.name, &cmd.entity_type, cmd.shares)
}

fn check_partner_fields(
    name: &str,
    entity_type: &str,
    shares: Shares,
) -> Result<(), PartnershipError> {
    if name.trim().is_empty() {
        return Err(PartnershipError::InvalidData(
            "a partner's name is required".to_string(),
        ));
    }
    if entity_type.trim().is_empty() {
        return Err(PartnershipError::InvalidData(
            "a partner's entity type is required (K-1 item I1)".to_string(),
        ));
    }
    if !shares.is_in_range() {
        return Err(PartnershipError::InvalidData(
            "each share must be between 0% and 100%".to_string(),
        ));
    }
    Ok(())
}

/// Admit a partner, against write-locked state.
///
/// Two things have to happen in here rather than before:
///
/// - **The id must not already be taken.** The projector writes
///   `INSERT OR REPLACE INTO partners (id, …)`, so admitting onto an id that
///   exists silently *replaces* that partner — their shares, their dates, their
///   name — and the only visible consequence is a K-1 that allocates somebody
///   else's income. Refusing here makes that impossible whoever minted the id.
/// - **The formation date must be read under the lock.** An omitted start date
///   means "since the business started", which is a read of the header; doing it
///   outside the transaction is a read-then-append that a concurrent
///   `BusinessProfileSet` can invalidate, dating the partner to a formation date
///   the books no longer claim.
pub(crate) fn build_admit_partner_in_txn(
    tx: &rusqlite::Transaction<'_>,
    partner_id: &str,
    cmd: &AdmitPartner,
) -> Result<PartnerStep, EventStoreError> {
    let taken: bool = tx
        .query_row("SELECT 1 FROM partners WHERE id = ?1", [partner_id], |_| {
            Ok(true)
        })
        .optional()?
        .unwrap_or(false);
    if taken {
        return Ok(PartnerStep::Reject(PartnershipError::PartnerExists(
            partner_id.to_string(),
        )));
    }

    let start_date = match cmd.start_date {
        Some(d) => d,
        None => match get_profile(tx) {
            Some(profile) => profile.formation_date,
            None => return Ok(PartnerStep::Reject(PartnershipError::NoProfile)),
        },
    };

    Ok(PartnerStep::Append(Event::PartnerAdmitted(Box::new(
        PartnerAdmittedData {
            partner_id: partner_id.to_string(),
            name: cmd.name.trim().to_string(),
            partner_type: cmd.partner_type.as_str().to_string(),
            residency: cmd.residency.as_str().to_string(),
            entity_type: cmd.entity_type.trim().to_string(),
            address: (&cmd.address).into(),
            start_date,
            shares: cmd.shares.into(),
        },
    ))))
}

/// Edit a partner, against write-locked state.
///
/// The partner must exist. The projector's `UPDATE … WHERE id = ?1` matches no
/// rows for an id nobody has, so without this check the append succeeds, the log
/// gains an event, and nothing whatsoever changes — a write that reports success
/// and did nothing.
///
/// A partner who has left may still be edited: correcting a misspelled name on
/// somebody's final K-1 is a legitimate thing to need, and their dates are not
/// editable here anyway.
pub(crate) fn build_update_partner_in_txn(
    tx: &rusqlite::Transaction<'_>,
    cmd: &UpdatePartner,
) -> Result<PartnerStep, EventStoreError> {
    // A departed partner's record is history: their K-1 for the year they left
    // was filed from these figures, and editing them now silently rewrites what
    // that return said. Their shares are also exactly what the
    // no-effective-date gap makes dangerous — see `partners_changed_after`.
    let end_date: Option<Option<String>> = tx
        .query_row(
            "SELECT end_date FROM partners WHERE id = ?1",
            [&cmd.partner_id],
            |r| r.get(0),
        )
        .optional()?;
    match end_date {
        None => {
            return Ok(PartnerStep::Reject(PartnershipError::NoSuchPartner(
                cmd.partner_id.clone(),
            )));
        }
        Some(Some(_)) => {
            return Ok(PartnerStep::Reject(PartnershipError::AlreadyWithdrawn(
                cmd.partner_id.clone(),
            )));
        }
        Some(None) => {}
    }
    Ok(PartnerStep::Append(Event::PartnerDetailsUpdated(Box::new(
        PartnerDetailsData {
            partner_id: cmd.partner_id.clone(),
            name: cmd.name.trim().to_string(),
            partner_type: cmd.partner_type.as_str().to_string(),
            residency: cmd.residency.as_str().to_string(),
            entity_type: cmd.entity_type.trim().to_string(),
            address: (&cmd.address).into(),
            shares: cmd.shares.into(),
        },
    ))))
}

/// Record a partner leaving, against write-locked state.
///
/// The exists-and-has-not-already-left check has to be in here: two members
/// withdrawing the same partner would otherwise both read "still in", both
/// append, and the second end date would silently move which tax year that
/// partner's *final* K-1 falls in — a difference nobody sees until the K-1 is
/// wrong.
pub(crate) fn build_withdraw_partner_in_txn(
    tx: &rusqlite::Transaction<'_>,
    partner_id: &str,
    end_date: NaiveDate,
) -> Result<PartnerStep, EventStoreError> {
    let existing: Option<(Option<String>, String)> = tx
        .query_row(
            "SELECT end_date, start_date FROM partners WHERE id = ?1",
            [partner_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        None => Ok(PartnerStep::Reject(PartnershipError::NoSuchPartner(
            partner_id.to_string(),
        ))),
        Some((Some(_), _)) => Ok(PartnerStep::Reject(PartnershipError::AlreadyWithdrawn(
            partner_id.to_string(),
        ))),
        Some((None, start)) => {
            // A partner cannot leave before they joined, and the failure is not
            // an error anybody would see: `shares_over` reads such a partner as
            // having joined mid-year *and* left within it, so both columns of
            // item J come out at 0% with Final ticked. Every box on that K-1 is
            // individually plausible. Read here rather than trusted from the
            // caller because the start date is the log's, not theirs.
            let start_date = parse_stored_date(&start);
            match start_date {
                Some(start_date) if end_date < start_date => Ok(PartnerStep::Reject(
                    PartnershipError::LeftBeforeJoining {
                        partner_id: partner_id.to_string(),
                        start_date,
                        end_date,
                    },
                )),
                _ => Ok(PartnerStep::Append(Event::PartnerWithdrawn {
                    partner_id: partner_id.to_string(),
                    end_date,
                })),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The partnership header
// ---------------------------------------------------------------------------

/// Record the partnership's details, replacing whatever was there.
///
/// Replacing rather than merging: the header is filed as a unit and the IRS
/// checks its parts against each other, so a half-updated header is not a state
/// worth being able to reach.
pub fn set_profile(
    store: &mut EventStore,
    user_id: &str,
    profile: &BusinessProfile,
) -> Result<StoredEvent, PartnershipError> {
    check_set_profile_pure(profile)?;
    append_checked_locally(store, user_id, |_tx| {
        Ok(PartnerStep::Append(build_set_profile_event(profile)))
    })
}

pub fn get_profile(conn: &Connection) -> Option<BusinessProfile> {
    conn.query_row(
        "SELECT legal_name, street, suite, city, state, postal_code, country,
                ein, naics_code, formation_date, principal_activity, principal_product
         FROM business_profile WHERE id = 'default'",
        [],
        |r| {
            Ok(BusinessProfile {
                legal_name: r.get(0)?,
                address: Address {
                    street: r.get(1)?,
                    suite: r.get(2)?,
                    city: r.get(3)?,
                    state: r.get(4)?,
                    postal_code: r.get(5)?,
                    country: r.get(6)?,
                },
                ein: r.get(7)?,
                naics_code: r.get(8)?,
                // Unreadable reads as no header at all, which sends the caller
                // to "set the partnership's details" — a visible, fixable state.
                // 1970 would instead be printed on box E of a filed return.
                formation_date: parse_stored_date(&r.get::<_, String>(9)?)
                    .ok_or(rusqlite::Error::InvalidQuery)?,
                principal_activity: r.get(10)?,
                principal_product: r.get(11)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// Partners
// ---------------------------------------------------------------------------

/// Everything needed to admit a partner, minus the id, which is minted here.
#[derive(Debug, Clone)]
pub struct AdmitPartner {
    pub name: String,
    pub partner_type: PartnerType,
    pub residency: Residency,
    pub entity_type: String,
    pub address: Address,
    /// `None` means "since the business started" — the common case, and the one
    /// worth not making somebody retype. Resolved against the stored profile, so
    /// it needs the profile to have been set.
    pub start_date: Option<NaiveDate>,
    pub shares: Shares,
    /// Written to the local table, never to the log. See the module docs.
    pub tin: Option<String>,
}

/// Admit a partner, returning their new id.
///
/// The id is minted here for standalone books. On group-hosted books the
/// **server** mints it instead — see `sync/commands/partnership.rs` — because the
/// projector writes `INSERT OR REPLACE INTO partners (id, …)` and a
/// client-chosen id is therefore a way to overwrite somebody else's partner
/// record, which rewrites their shares and lands on their K-1.
pub fn admit_partner(
    store: &mut EventStore,
    user_id: &str,
    cmd: &AdmitPartner,
) -> Result<(String, StoredEvent), PartnershipError> {
    check_admit_partner_pure(cmd)?;

    let partner_id = Uuid::new_v4().to_string();
    let stored = append_checked_locally(store, user_id, |tx| {
        build_admit_partner_in_txn(tx, &partner_id, cmd)
    })?;

    // After the append, and deliberately outside it: the TIN goes to a local
    // table that is not part of the log, so it has nothing to be atomic with.
    //
    // The partner is already admitted by this point, so a failure here must not
    // read as "nothing happened" — a caller who retried on that reading would
    // admit them a second time. The error says so, and the id is recoverable
    // from the message; `set_tin` fixes it without a second admission.
    if let Some(tin) = cmd.tin.as_deref().filter(|t| !t.trim().is_empty()) {
        if let Err(e) = set_tin(store.connection(), &partner_id, tin.trim()) {
            return Err(PartnershipError::TinNotStored {
                partner_id,
                reason: e.to_string(),
            });
        }
    }
    Ok((partner_id, stored))
}

/// A partner's details as they should now stand.
///
/// A struct rather than a row of arguments, mirroring [`AdmitPartner`]: nine
/// positional parameters of which five are strings is a call somebody
/// eventually gets out of order, and `name` and `entity_type` transposed is a
/// K-1 that looks plausible and is wrong.
#[derive(Debug, Clone)]
pub struct UpdatePartner {
    pub partner_id: String,
    pub name: String,
    pub partner_type: PartnerType,
    pub residency: Residency,
    pub entity_type: String,
    pub address: Address,
    pub shares: Shares,
}

/// Change a partner's details or shares.
///
/// Their start and end dates are not editable here — joining and leaving are
/// their own events, and letting an edit move them would quietly change which
/// years the partner gets a K-1 for.
pub fn update_partner(
    store: &mut EventStore,
    user_id: &str,
    cmd: &UpdatePartner,
) -> Result<StoredEvent, PartnershipError> {
    check_update_partner_pure(cmd)?;
    append_checked_locally(store, user_id, |tx| build_update_partner_in_txn(tx, cmd))
}

/// Record that a partner has left.
///
/// Refused if they already have, because a second end date would silently move
/// which year their final K-1 falls in.
pub fn withdraw_partner(
    store: &mut EventStore,
    user_id: &str,
    partner_id: &str,
    end_date: NaiveDate,
) -> Result<StoredEvent, PartnershipError> {
    append_checked_locally(store, user_id, |tx| {
        build_withdraw_partner_in_txn(tx, partner_id, end_date)
    })
}

pub fn list_partners(conn: &Connection) -> Vec<Partner> {
    list_partners_with_problems(conn).0
}

/// The partners, and every row that could not be read as one.
///
/// Two return values because the alternative is the failure this exists to
/// prevent: a partner silently absent from a list is a partner silently absent
/// from a *return*, filed as though they were never there. Anything preparing a
/// filing should take the second value and put it in front of somebody; screens
/// that are only browsing can keep using [`list_partners`].
pub fn list_partners_with_problems(conn: &Connection) -> (Vec<Partner>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();

    let mut stmt = match conn.prepare(
        "SELECT id, name, partner_type, residency, entity_type,
                street, suite, city, state, postal_code, country,
                start_date, end_date, profit_ppm, loss_ppm, capital_ppm
         FROM partners ORDER BY start_date, name",
    ) {
        Ok(stmt) => stmt,
        Err(e) => {
            problems.push(format!("Could not read the partner list: {e}"));
            return (out, problems);
        }
    };

    let rows = match stmt.query_map([], row_to_partner) {
        Ok(rows) => rows,
        Err(e) => {
            problems.push(format!("Could not read the partner list: {e}"));
            return (out, problems);
        }
    };
    for row in rows {
        match row {
            Ok(Ok(partner)) => out.push(partner),
            // A row that is there but unreadable — a date this crate did not
            // write, most likely. Named, because "one fewer K-1 than you
            // expected" is not something anybody counts.
            Ok(Err(problem)) => problems.push(problem),
            Err(e) => problems.push(format!("Could not read a partner row: {e}")),
        }
    }
    (out, problems)
}

pub fn get_partner(conn: &Connection, partner_id: &str) -> Option<Partner> {
    conn.query_row(
        "SELECT id, name, partner_type, residency, entity_type,
                street, suite, city, state, postal_code, country,
                start_date, end_date, profit_ppm, loss_ppm, capital_ppm
         FROM partners WHERE id = ?1",
        [partner_id],
        row_to_partner,
    )
    .optional()
    .ok()
    .flatten()
    // A row that cannot be read is reported as no such partner. Callers that
    // need to say *why* — anything assembling a filing — use
    // [`list_partners_with_problems`], which keeps the note.
    .and_then(|row| row.ok())
}

/// The partners who held an interest at any point in a tax year — one K-1 each.
pub fn partners_for_year(conn: &Connection, year: i32) -> Vec<Partner> {
    partners_for_year_with_problems(conn, year).0
}

/// The partners for a year, and every row that could not be read as one.
///
/// The variant a filing should call. See [`list_partners_with_problems`].
pub fn partners_for_year_with_problems(
    conn: &Connection,
    year: i32,
) -> (Vec<Partner>, Vec<String>) {
    let (start, end) = calendar_year(year);
    let (partners, problems) = list_partners_with_problems(conn);
    (
        partners
            .into_iter()
            .filter(|p| p.was_partner_during(start, end))
            .collect(),
        problems,
    )
}

/// Partners whose record was last changed after `cutoff`, named.
///
/// # The gap this exists to expose
///
/// A partner's shares are stored as one current figure with no effective date,
/// so [`Event::PartnerDetailsUpdated`] simply overwrites them. Regenerating an
/// *earlier* year's return therefore prints *today's* split: partners who were
/// 50/50 through 2024 and moved to 70/30 in March 2025 get 2024 K-1s showing
/// 70/30 in both columns of item J. The totals still come to 100%, so nothing
/// downstream objects.
///
/// The real fix is a dated share change — item J has a beginning and an ending
/// column precisely because splits move mid-year, and the model cannot yet say
/// so. Until then this reports the condition, because a return that is quietly
/// wrong about who earned what is worse than one that says it might be.
///
/// Keyed on the edit event itself, not on `partners.updated_at_event`. That
/// column moves when a partner is *admitted* too, so it fires for every partner
/// entered after the year they are being reported for — which is most of them,
/// every filing season. A warning that is always on is one nobody reads.
pub fn partners_changed_after(conn: &Connection, cutoff: NaiveDate) -> Vec<String> {
    let mut out = Vec::new();
    // `PartnerDetailsUpdated` is precisely the event that overwrites shares with
    // no effective date. The payload is flat (see `Event`'s serde tagging), so
    // the partner id is a top-level key.
    let sql = "SELECT DISTINCT p.name
               FROM events e
               JOIN partners p ON p.id = json_extract(e.payload, '$.partner_id')
               WHERE e.event_type = 'partner_details_updated'
                 AND date(e.timestamp) > ?1
               ORDER BY p.name";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return out;
    };
    if let Ok(rows) = stmt.query_map([cutoff.to_string()], |r| r.get::<_, String>(0)) {
        out.extend(rows.flatten());
    }
    out
}

/// The calendar year as a pair of dates.
///
/// Calendar years only, for now: a fiscal-year partnership files with the year's
/// beginning and ending dates written into the form header, and guessing them
/// from `company.fiscal_year_start_month` would put dates on a return that
/// nobody chose. A caller wanting a fiscal year should pass the dates.
pub fn calendar_year(year: i32) -> (NaiveDate, NaiveDate) {
    (
        NaiveDate::from_ymd_opt(year, 1, 1).expect("January 1 exists in every year"),
        NaiveDate::from_ymd_opt(year, 12, 31).expect("December 31 exists in every year"),
    )
}

// ---------------------------------------------------------------------------
// TINs — local only
// ---------------------------------------------------------------------------

/// Store a partner's TIN on this machine. Never appended to the event log.
pub fn set_tin(conn: &Connection, partner_id: &str, tin: &str) -> Result<(), PartnershipError> {
    let tin = tin.trim();
    if !is_valid_tin(tin) {
        // Shape, never the value — see `check_admit_partner_pure`.
        return Err(PartnershipError::InvalidData(
            "that is not an SSN (NNN-NN-NNNN) or an EIN (NN-NNNNNNN)".to_string(),
        ));
    }
    conn.execute(
        "INSERT INTO partner_tins (partner_id, tin, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(partner_id) DO UPDATE SET tin = ?2, updated_at = datetime('now')",
        params![partner_id, tin],
    )
    .map_err(|e| PartnershipError::StoreError(e.to_string()))?;
    Ok(())
}

/// A partner's TIN, if this machine holds one.
pub fn get_tin(conn: &Connection, partner_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT tin FROM partner_tins WHERE partner_id = ?1",
        [partner_id],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Forget a partner's TIN on this machine.
pub fn clear_tin(conn: &Connection, partner_id: &str) -> Result<(), PartnershipError> {
    conn.execute("DELETE FROM partner_tins WHERE partner_id = ?1", [partner_id])
        .map_err(|e| PartnershipError::StoreError(e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// A row, or a note saying why it could not be read.
///
/// The outer `Result` is the database's; the inner one is ours, for a row that
/// arrived intact but says something we cannot honour — a date not in the format
/// this crate writes. Kept apart because "the query failed" and "this partner's
/// start date is gibberish" want different words in front of a person.
fn row_to_partner(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Partner, String>> {
    let id: String = r.get(0)?;
    let name: String = r.get(1)?;
    let end: Option<String> = r.get(12)?;

    let raw_start: String = r.get(11)?;
    let Some(start_date) = parse_stored_date(&raw_start) else {
        return Ok(Err(format!(
            "Partner '{name}' ({id}) has an unreadable start date {raw_start:?} and is left off.              Fix the record and try again."
        )));
    };
    let end_date = match end.as_deref() {
        None => None,
        Some(raw) => match parse_stored_date(raw) {
            Some(d) => Some(d),
            None => {
                return Ok(Err(format!(
                    "Partner '{name}' ({id}) has an unreadable end date {raw:?} and is left off.                      Fix the record and try again."
                )));
            }
        },
    };

    Ok(Ok(Partner {
        partner_id: id,
        name,
        partner_type: PartnerType::parse(&r.get::<_, String>(2)?).unwrap_or(PartnerType::General),
        residency: Residency::parse(&r.get::<_, String>(3)?).unwrap_or(Residency::Domestic),
        entity_type: r.get(4)?,
        address: Address {
            street: r.get(5)?,
            suite: r.get(6)?,
            city: r.get(7)?,
            state: r.get(8)?,
            postal_code: r.get(9)?,
            country: r.get(10)?,
        },
        start_date,
        end_date,
        shares: Shares {
            profit_ppm: r.get(13)?,
            loss_ppm: r.get(14)?,
            capital_ppm: r.get(15)?,
        },
    }))
}

/// Dates are written by us, in ISO form, so a bad one is a bug rather than
/// input. The epoch keeps a corrupt row readable instead of panicking a TUI
/// three screens away from the cause.
/// A date as this crate wrote it, or nothing.
///
/// It used to fall back to the epoch, on the reasoning that a corrupt row should
/// not panic a screen three steps from the cause. That was the wrong trade for
/// this data. A `start_date` of 1970-01-01 is not visibly broken — it is simply
/// early, so [`Partner::shares_over`] reads the partner as having been there all
/// year and issues them a K-1 with full shares and an entirely ordinary
/// appearance. The corruption becomes a number on a tax form instead of a
/// message on a screen.
///
/// So the failure is returned and the partner is dropped from the list with a
/// note — see [`list_partners_with_problems`]. Absent is a state somebody
/// notices; plausible is not.
fn parse_stored_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

/// Run an append on **this machine's** books, retrying if the log head moves.
///
/// The mirror of the sync handlers: both call the same `build_*_in_txn`
/// predicate, so a rule enforced on standalone books is the same rule enforced
/// on the group's server. Checked rather than blind-appended even though local
/// books are single-writer, because the predicate has to run against
/// write-locked state either way and having one code path is worth more than
/// the microsecond.
fn append_checked_locally(
    store: &mut EventStore,
    user_id: &str,
    build: impl Fn(&rusqlite::Transaction<'_>) -> Result<PartnerStep, EventStoreError>,
) -> Result<StoredEvent, PartnershipError> {
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let outcome = store.append_checked(
            head,
            |tx| match build(tx)? {
                PartnerStep::Append(event) => Ok(Verdict::Append(EventEnvelope::new(
                    event,
                    user_id.to_string(),
                ))),
                PartnerStep::Reject(e) => Ok(Verdict::Reject(e)),
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;

        match outcome {
            CheckedOutcome::Appended(stored) => return Ok(stored),
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::FULL_SHARE;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        crate::store::migrations::SchemaStore::init_schema(&mut s).unwrap();
        s
    }

    fn profile() -> BusinessProfile {
        BusinessProfile {
            legal_name: "Clovelly Technology Partners LLC".into(),
            address: Address {
                street: "1 Example Street".into(),
                suite: None,
                city: "Cape Town".into(),
                state: "WC".into(),
                postal_code: "8001".into(),
                country: None,
            },
            ein: "88-1234567".into(),
            naics_code: "541511".into(),
            formation_date: day(2021, 7, 1),
            principal_activity: Some("Software".into()),
            principal_product: Some("Accounting software".into()),
        }
    }

    fn a_partner(name: &str) -> AdmitPartner {
        AdmitPartner {
            name: name.into(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: "Individual".into(),
            address: Address {
                street: "2 Other Road".into(),
                suite: None,
                city: "Cape Town".into(),
                state: "WC".into(),
                postal_code: "8001".into(),
                country: None,
            },
            start_date: None,
            shares: Shares::from_percents(50.0, 50.0, 50.0),
            tin: Some("123-45-6789".into()),
        }
    }

    #[test]
    fn a_profile_reads_back_as_it_was_written() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let got = get_profile(s.connection()).unwrap();
        assert_eq!(got, profile());
    }

    #[test]
    fn setting_the_profile_again_replaces_it_rather_than_adding_a_second() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let mut p2 = profile();
        p2.legal_name = "Renamed LLC".into();
        set_profile(&mut s, "u", &p2).unwrap();

        assert_eq!(get_profile(s.connection()).unwrap().legal_name, "Renamed LLC");
        let n: i64 = s
            .connection()
            .query_row("SELECT COUNT(*) FROM business_profile", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "one business, one header");
    }

    /// The default nobody should have to retype.
    #[test]
    fn a_partner_with_no_start_date_started_when_the_business_did() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let (id, _) = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap();
        let p = get_partner(s.connection(), &id).unwrap();
        assert_eq!(p.start_date, day(2021, 7, 1));
        assert_eq!(p.end_date, None, "still a partner");
    }

    #[test]
    fn a_partner_cannot_default_their_start_date_before_the_business_exists() {
        let mut s = store();
        let err = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap_err();
        assert!(matches!(err, PartnershipError::NoProfile), "got {err:?}");
    }

    /// The whole point of the local table: the number must not reach the log.
    #[test]
    fn a_tin_is_stored_locally_and_never_appears_in_the_event_log() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let (id, _) = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap();

        assert_eq!(get_tin(s.connection(), &id).as_deref(), Some("123-45-6789"));

        let log: String = s
            .connection()
            .query_row(
                "SELECT COALESCE(GROUP_CONCAT(payload, ' '), '') FROM events",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !log.contains("123-45-6789"),
            "an SSN reached the replicated log"
        );
    }

    #[test]
    fn a_malformed_tin_is_refused_before_the_partner_is_admitted() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let mut cmd = a_partner("Alice");
        cmd.tin = Some("123456789".into());

        let err = admit_partner(&mut s, "u", &cmd).unwrap_err();
        assert!(matches!(err, PartnershipError::InvalidData(_)), "got {err:?}");
        assert!(
            list_partners(s.connection()).is_empty(),
            "nothing was admitted"
        );
    }

    /// A failed TIN write must not read as a failed admission.
    ///
    /// The partner is already in the log by the time the local note is written,
    /// and the note is deliberately not atomic with it — rolling a partnership
    /// fact back because a local convenience failed would be the wrong trade. So
    /// the contract the callers rely on is this one: `TinNotStored` means *the
    /// partner exists*. A caller that treated it as "nothing happened" would
    /// retry and admit them twice.
    #[test]
    fn a_failed_tin_write_still_leaves_the_partner_admitted() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();

        // Fault injection: make the local note impossible to write, without
        // touching the event log the partner is recorded in.
        s.connection()
            .execute_batch("DROP TABLE partner_tins;")
            .unwrap();

        let err = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap_err();
        let partner_id = match &err {
            PartnershipError::TinNotStored { partner_id, .. } => partner_id.clone(),
            other => panic!("expected TinNotStored, got {other:?}"),
        };

        let partners = list_partners(s.connection());
        assert_eq!(partners.len(), 1, "the admission must have stood");
        assert_eq!(partners[0].partner_id, partner_id, "and the error names it");
        assert_eq!(partners[0].name, "Alice");
    }

    #[test]
    fn updating_a_partner_changes_their_shares_but_not_their_dates() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let (id, _) = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap();
        let before = get_partner(s.connection(), &id).unwrap();

        update_partner(
            &mut s,
            "u",
            &UpdatePartner {
                partner_id: id.clone(),
                name: "Alice Renamed".into(),
                partner_type: PartnerType::Limited,
                residency: Residency::Foreign,
                entity_type: "Corporation".into(),
                address: before.address.clone(),
                shares: Shares::from_percents(60.0, 60.0, 60.0),
            },
        )
        .unwrap();

        let after = get_partner(s.connection(), &id).unwrap();
        assert_eq!(after.name, "Alice Renamed");
        assert_eq!(after.partner_type, PartnerType::Limited);
        assert_eq!(after.residency, Residency::Foreign);
        assert_eq!(after.shares.profit_ppm, 600_000);
        assert_eq!(after.start_date, before.start_date, "dates are not editable");
        assert_eq!(after.end_date, before.end_date);
    }

    #[test]
    fn updating_a_partner_who_does_not_exist_is_refused() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let err = update_partner(
            &mut s,
            "u",
            &UpdatePartner {
                partner_id: "nobody".into(),
                name: "X".into(),
                partner_type: PartnerType::General,
                residency: Residency::Domestic,
                entity_type: "Individual".into(),
                address: Address::default(),
                shares: Shares::from_percents(1.0, 1.0, 1.0),
            },
        )
        .unwrap_err();
        assert!(matches!(err, PartnershipError::NoSuchPartner(_)), "got {err:?}");
    }

    /// The second lock on the door the server's id-minting is the first lock on.
    ///
    /// The projector writes `INSERT OR REPLACE INTO partners (id, …)`, so
    /// admitting onto an id that exists does not fail — it *replaces* that
    /// partner's name, dates and shares, and the only visible consequence is a
    /// K-1 allocating them somebody else's income. Refused under the write lock,
    /// so it holds whoever minted the id.
    #[test]
    fn admitting_onto_an_id_that_already_exists_is_refused_under_the_write_lock() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let (taken, _) = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap();

        let tx = s.connection_mut().transaction().unwrap();

        let clobber = build_admit_partner_in_txn(&tx, &taken, &a_partner("Mallory")).unwrap();
        assert!(
            matches!(
                clobber,
                PartnerStep::Reject(PartnershipError::PartnerExists(_))
            ),
            "an existing partner could be overwritten by admitting onto their id"
        );

        let fresh = build_admit_partner_in_txn(&tx, "a-fresh-id", &a_partner("Carol")).unwrap();
        assert!(
            matches!(fresh, PartnerStep::Append(_)),
            "an unused id must still be admissible"
        );
    }

    /// A departed partner's record is history and must not be edited.
    ///
    /// Their K-1 for the year they left was filed from these figures. Because
    /// shares carry no effective date, changing them now silently rewrites what
    /// that return said — and the partner is gone, so nobody is watching.
    #[test]
    fn a_partner_who_has_left_can_no_longer_be_edited() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let (id, _) = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap();
        let before = get_partner(s.connection(), &id).unwrap();
        withdraw_partner(&mut s, "u", &id, day(2025, 6, 30)).unwrap();

        let err = update_partner(
            &mut s,
            "u",
            &UpdatePartner {
                partner_id: id.clone(),
                name: "Rewritten".into(),
                partner_type: PartnerType::Limited,
                residency: Residency::Foreign,
                entity_type: "Corporation".into(),
                address: before.address.clone(),
                shares: Shares::from_percents(90.0, 90.0, 90.0),
            },
        )
        .unwrap_err();

        assert!(
            matches!(err, PartnershipError::AlreadyWithdrawn(_)),
            "got {err:?}"
        );
        let after = get_partner(s.connection(), &id).unwrap();
        assert_eq!(after.name, before.name, "a departed partner was rewritten");
        assert_eq!(after.shares, before.shares);
    }

    /// A partner cannot leave before they joined.
    ///
    /// The refusal matters because the result is not an error anybody would see.
    /// `shares_over` reads such a partner as having joined mid-year *and* left
    /// within it, so item J comes out at 0% in both columns with Final ticked —
    /// a K-1 whose every box is individually plausible.
    #[test]
    fn a_partner_cannot_leave_before_the_day_they_joined() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let mut cmd = a_partner("Alice");
        cmd.start_date = Some(day(2021, 7, 1));
        let (id, _) = admit_partner(&mut s, "u", &cmd).unwrap();

        let err = withdraw_partner(&mut s, "u", &id, day(2020, 1, 1)).unwrap_err();
        assert!(
            matches!(err, PartnershipError::LeftBeforeJoining { .. }),
            "got {err:?}"
        );
        assert_eq!(
            get_partner(s.connection(), &id).unwrap().end_date,
            None,
            "the partner was withdrawn anyway"
        );

        // The same day is fine — somebody who joined and left in one day held an
        // interest during the year and is owed a K-1.
        withdraw_partner(&mut s, "u", &id, day(2021, 7, 1)).unwrap();
    }

    /// The number must not travel in the refusal.
    ///
    /// A mistyped TIN is usually a nearly-correct one, and an error string
    /// reaches terminal scrollback, log files and the desktop's error bar — all
    /// the places keeping it out of the event log was meant to avoid.
    #[test]
    fn a_refused_tin_is_described_by_shape_and_never_quoted_back() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let mut cmd = a_partner("Alice");
        cmd.tin = Some("123-45-678".into()); // one digit short

        let err = admit_partner(&mut s, "u", &cmd).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("123-45-678"),
            "the refusal quoted the TIN back: {msg}"
        );
        assert!(msg.contains("NNN-NN-NNNN"), "no shape given: {msg}");

        let err = set_tin(s.connection(), "whoever", "123-45-678").unwrap_err();
        assert!(
            !err.to_string().contains("123-45-678"),
            "set_tin quoted the TIN back: {err}"
        );
    }

    /// A partner whose stored date is gibberish is dropped and named, not dated
    /// to the epoch.
    ///
    /// The epoch is not a visibly wrong start date — it is merely early, so the
    /// partner reads as having been there all year and collects a K-1 with full
    /// shares and an entirely ordinary appearance. Absent-and-reported is a state
    /// somebody acts on; plausible-and-wrong is one they file.
    #[test]
    fn a_partner_with_an_unreadable_date_is_left_out_and_named() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let (good, _) = admit_partner(&mut s, "u", &a_partner("Readable")).unwrap();
        let mut other = a_partner("Corrupt");
        other.tin = None;
        let (bad, _) = admit_partner(&mut s, "u", &other).unwrap();

        s.connection()
            .execute(
                "UPDATE partners SET start_date = 'not-a-date' WHERE id = ?1",
                [&bad],
            )
            .unwrap();

        let (partners, problems) = list_partners_with_problems(s.connection());
        let ids: Vec<&str> = partners.iter().map(|p| p.partner_id.as_str()).collect();
        assert_eq!(ids, [good.as_str()], "the corrupt row was read anyway");
        assert!(
            problems.iter().any(|p| p.contains("Corrupt")),
            "the dropped partner went unnamed: {problems:?}"
        );
        assert!(
            !partners.iter().any(|p| p.start_date == day(1970, 1, 1)),
            "a date was invented"
        );
    }

    #[test]
    fn a_partner_can_only_leave_once() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let (id, _) = admit_partner(&mut s, "u", &a_partner("Alice")).unwrap();

        withdraw_partner(&mut s, "u", &id, day(2025, 6, 30)).unwrap();
        assert_eq!(
            get_partner(s.connection(), &id).unwrap().end_date,
            Some(day(2025, 6, 30))
        );

        let err = withdraw_partner(&mut s, "u", &id, day(2025, 9, 1)).unwrap_err();
        assert!(matches!(err, PartnershipError::AlreadyWithdrawn(_)), "got {err:?}");
        assert_eq!(
            get_partner(s.connection(), &id).unwrap().end_date,
            Some(day(2025, 6, 30)),
            "the first end date stands"
        );
    }

    /// A joiner and a leaver both get a K-1; somebody outside the year does not.
    #[test]
    fn a_year_lists_everyone_who_held_an_interest_during_it() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();

        let mut stayed = a_partner("Stayed");
        stayed.start_date = Some(day(2021, 7, 1));
        let (stayed_id, _) = admit_partner(&mut s, "u", &stayed).unwrap();

        let mut joined = a_partner("Joined");
        joined.start_date = Some(day(2025, 3, 1));
        joined.tin = None;
        let (joined_id, _) = admit_partner(&mut s, "u", &joined).unwrap();

        let mut left = a_partner("Left");
        left.start_date = Some(day(2021, 7, 1));
        left.tin = None;
        let (left_id, _) = admit_partner(&mut s, "u", &left).unwrap();
        withdraw_partner(&mut s, "u", &left_id, day(2024, 12, 31)).unwrap();

        let ids: Vec<String> = partners_for_year(s.connection(), 2025)
            .into_iter()
            .map(|p| p.partner_id)
            .collect();
        assert!(ids.contains(&stayed_id));
        assert!(ids.contains(&joined_id), "joined in March, still gets a K-1");
        assert!(!ids.contains(&left_id), "left before 2025 began");
    }

    #[test]
    fn two_half_partners_shares_add_up_to_the_whole() {
        let mut s = store();
        set_profile(&mut s, "u", &profile()).unwrap();
        let mut b = a_partner("Bob");
        b.tin = None;
        admit_partner(&mut s, "u", &a_partner("Alice")).unwrap();
        admit_partner(&mut s, "u", &b).unwrap();

        let shares: Vec<Shares> = list_partners(s.connection()).iter().map(|p| p.shares).collect();
        let totals = Shares::sums_to_whole(&shares);
        assert_eq!(totals.profit_ppm, FULL_SHARE);
        assert!(totals.is_whole());
    }
}
