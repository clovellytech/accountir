//! A business's Schedule C, as its owner's personal books record it.
//!
//! The counterpart of [`super::k1_package`] for a sole proprietorship. The owner's
//! Form 1040 needs four things from each business they own: line 31, which goes
//! to Schedule 1 line 3; the self-employment figure Schedule SE starts from; the
//! Section 199A figures Form 8995 reads; and gross receipts, against which a
//! 1099-NEC paid to the business can be checked.
//!
//! # The same computation the business files
//!
//! The figures come from [`sole_proprietor_commands::figures`] with the year's
//! recorded inputs — the one computation the business's own Schedule C page and
//! PDF read — so the line 31 that reaches the 1040 is the line 31 the business
//! files, §179 limit and register check included. The figures' warnings travel
//! with the package: a Schedule C with an unposted depreciation year is one to
//! fix before a personal return is filed on it.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use rusqlite::Connection;
use thiserror::Error;

use crate::commands::{depreciation_commands, partnership_commands as pc, sole_proprietor_commands};
use crate::domain::{BusinessType, System};

#[derive(Debug, Error)]
pub enum ScheduleCPackageError {
    #[error("these books file {0}, not Schedule C")]
    NotASoleProprietorship(&'static str),
    #[error("these books have no ledger id, so a Schedule C from them could not say where it came from")]
    NoLedgerId,
    #[error("these books have no events yet")]
    NoEvents,
    #[error("computing the Schedule C: {0}")]
    Figures(String),
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// One year's Schedule C, as the owner's books record it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleCPackage {
    pub ledger_id: String,
    /// The business, as line C names it — or the ledger's name when it trades
    /// under the owner's own.
    pub business_name: String,
    /// The proprietor, as those books name them. Empty when they name nobody.
    pub proprietor_name: String,
    pub tax_year: i32,
    /// Box code, from the Schedule C catalogue in
    /// [`super::information_returns`], to cents.
    pub amounts: BTreeMap<String, i64>,
    pub through_event: i64,
    pub event_hash: String,
    pub warnings: Vec<String>,
}

/// The year's Schedule C from a sole proprietorship's books.
pub fn for_year(conn: &Connection, year: i32) -> Result<ScheduleCPackage, ScheduleCPackageError> {
    let kind = sole_proprietor_commands::business_type(conn);
    if kind != BusinessType::SoleProprietorship {
        return Err(ScheduleCPackageError::NotASoleProprietorship(kind.form_name()));
    }
    let ledger_id = crate::documents::ledger_id(conn).ok_or(ScheduleCPackageError::NoLedgerId)?;
    let (through_event, event_hash) = super::k1_package::head(conn).map_err(|e| match e {
        super::k1_package::K1PackageError::Database(d) => ScheduleCPackageError::Database(d),
        _ => ScheduleCPackageError::NoEvents,
    })?;

    let inputs = sole_proprietor_commands::get_inputs(conn, year);
    let figures = sole_proprietor_commands::figures(conn, year, &inputs)
        .map_err(|e| ScheduleCPackageError::Figures(e.to_string()))?;
    let lines = &figures.computed.lines;
    let line_31 = lines.line_31(lines.get("sc48"), inputs.home_office_dollars.unwrap_or(0));
    let mut warnings = figures.computed.warnings.clone();
    warnings.extend(figures.warnings.iter().cloned());
    if inputs.home_office_dollars.is_none() {
        warnings.push(format!(
            "Line 30, business use of the home, is not worked out for {year} on the business's \
             Schedule C page, so line 31 is figured without it."
        ));
    }

    let mut amounts = BTreeMap::new();
    let mut put = |code: &str, cents: i64| {
        if cents != 0 {
            amounts.insert(code.to_string(), cents);
        }
    };
    put("1", lines.get("sc1") * 100);
    put("31", line_31 * 100);
    // A sole proprietor's net profit is their net earnings from self-employment
    // (Schedule SE line 2), and their qualified business income before the 1040
    // takes the deductible half of the SE tax out of it.
    put("se", line_31 * 100);
    put("qbi", line_31 * 100);
    put("qbi_w2_wages", lines.get("sc26") * 100);
    put("qbi_ubia", ubia_cents(conn, year));

    let profile = pc::get_profile(conn);
    let proprietor = sole_proprietor_commands::get_proprietor(conn);
    let business_name = profile
        .as_ref()
        .and_then(|p| super::schedule_c::separate_business_name(p, proprietor.as_ref()))
        .map(str::to_string)
        .or_else(|| profile.as_ref().map(|p| p.legal_name.clone()))
        .or_else(|| crate::documents::ledger_name(conn))
        .unwrap_or_else(|| ledger_id.clone());

    Ok(ScheduleCPackage {
        ledger_id,
        business_name,
        proprietor_name: proprietor.map(|p| p.name).unwrap_or_default(),
        tax_year: year,
        amounts,
        through_event,
        event_hash,
        warnings,
    })
}

/// The unadjusted basis immediately after acquisition of the business's
/// qualified property at the end of `year` — Form 8995-A's UBIA.
///
/// Property on the register, still held at year end, and still inside its
/// depreciable period, which for §199A is the later of ten years from being
/// placed in service and the end of its recovery period. Cost, not cost less
/// depreciation: that is what "unadjusted" means. Land is never on the register,
/// so it never reaches this.
pub fn ubia_cents(conn: &Connection, year: i32) -> i64 {
    let Some(year_end) = NaiveDate::from_ymd_opt(year, 12, 31) else {
        return 0;
    };
    depreciation_commands::list_assets(conn)
        .iter()
        .filter(|a| a.placed_in_service <= year_end)
        .filter(|a| a.disposed_on.is_none_or(|d| d > year_end))
        .filter(|a| {
            let recovery = a.class.recovery_years(System::Gds).ceil() as i32;
            year - a.placed_in_service.year() < recovery.max(10)
        })
        .map(|a| a.cost_cents)
        .sum()
}
