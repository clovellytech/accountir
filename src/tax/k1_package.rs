//! A partner's Schedule K-1 as figures, for the books that receive it.
//!
//! # Why this goes through the whole return
//!
//! A K-1's figures depend on nearly everything Form 1065 computes: the line
//! mappings, the deduction limits, an interim closing when interests changed,
//! fixed allocations, draws in box 19, item L. Recomputing any of that here would
//! be a second implementation, and it would disagree with the first the moment
//! either changed. So a package is read off [`build_return_from_ledger`]'s own
//! [`K1Figures`] — the values its K-1 pages were filled from — and the PDF is
//! thrown away. That costs a second of rendering and buys a package that cannot
//! say anything the K-1 itself does not.
//!
//! # Provenance
//!
//! A package names the event its source books had reached (`through_event`,
//! `event_hash`). The receiving books keep that with the figures, so a personal
//! return prepared in April can be checked in September against a partnership
//! return that changed in between. The head is read before the return is built:
//! "at least through this event", which is the conservative direction, and
//! [`crate::commands::tax_statement_commands::k1_freshness`] compares figures
//! rather than heads anyway.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;

use super::form1065::{build_return_from_ledger, K1Figures, PartnerFiling, ReturnRequest};
use crate::commands::{
    partnership_commands as pc, share_period_commands, sole_proprietor_commands,
};
use crate::domain::BusinessType;

#[derive(Debug, Error)]
pub enum K1PackageError {
    #[error("these books file {0}, not Form 1065, so they issue no K-1s")]
    NotAPartnership(&'static str),
    #[error(
        "the partnership's details have not been set, so there is no return to take a K-1 from"
    )]
    NoProfile,
    #[error("these books have no ledger id, so a K-1 from them could not say where it came from")]
    NoLedgerId,
    #[error("these books have no events yet")]
    NoEvents,
    #[error("partner {partner_id} held no interest in {year}, so there is no K-1 for them")]
    NoSuchPartner { partner_id: String, year: i32 },
    #[error("building the return: {0}")]
    Form(String),
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// One partner's K-1, as the receiving books record it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct K1Package {
    pub ledger_id: String,
    /// The partnership's legal name, as its K-1 prints it.
    pub partnership_name: String,
    pub partnership_ein: String,
    pub tax_year: i32,
    pub partner_id: String,
    pub partner_name: String,
    /// Box code, from the Schedule K-1 (Form 1065) catalogue in
    /// [`super::information_returns`], to cents.
    pub amounts: BTreeMap<String, i64>,
    pub through_event: i64,
    pub event_hash: String,
    /// The partnership return's warnings. Carried with the package because a K-1
    /// from a return with unresolved warnings is one to question before a
    /// personal return is filed on it.
    pub warnings: Vec<String>,
}

/// The Form 1065 request these books would file for `year`, with everything the
/// ledger supplies left for [`build_return_from_ledger`] to read.
///
/// One place for it, so the CLI's `tax form1065` and a K-1 package are built
/// from the same request and so cannot describe different returns.
pub fn return_request(conn: &Connection, year: i32) -> Result<ReturnRequest, K1PackageError> {
    let kind = sole_proprietor_commands::business_type(conn);
    if kind != BusinessType::Partnership {
        return Err(K1PackageError::NotAPartnership(kind.form_name()));
    }
    let profile = pc::get_profile(conn).ok_or(K1PackageError::NoProfile)?;
    let partners: Vec<PartnerFiling> = share_period_commands::partners_for_year(conn, year)
        .into_iter()
        .map(|partner| PartnerFiling {
            tin: pc::get_tin(conn, &partner.partner_id),
            partner,
        })
        .collect();
    Ok(ReturnRequest {
        year,
        profile,
        partners,
        schedule_b: super::schedule_b::load(conn, year),
        // Family ties for Schedule B-1's §267(c) attribution.
        relationships: pc::list_relationships(conn),
        // Left empty for `build_return_from_ledger`, which reads each of these
        // from the books itself.
        assets: Vec::new(),
        schedule_l: None,
        capital: Default::default(),
        nondeductible: Vec::new(),
        segments: Vec::new(),
        detail: Default::default(),
        options: Default::default(),
        book_income_cents: 0,
        fixed_allocations: Vec::new(),
        liabilities: Default::default(),
    })
}

/// Every partner's K-1 package for `year`.
pub fn for_year(conn: &Connection, year: i32) -> Result<Vec<K1Package>, K1PackageError> {
    let ledger_id = crate::documents::ledger_id(conn).ok_or(K1PackageError::NoLedgerId)?;
    let (through_event, event_hash) = head(conn)?;
    let req = return_request(conn, year)?;
    let bundle =
        build_return_from_ledger(conn, &req).map_err(|e| K1PackageError::Form(e.to_string()))?;
    Ok(bundle
        .k1s
        .iter()
        .map(|k1| K1Package {
            ledger_id: ledger_id.clone(),
            partnership_name: req.profile.legal_name.clone(),
            partnership_ein: req.profile.ein.clone(),
            tax_year: year,
            partner_id: k1.partner_id.clone(),
            partner_name: k1.partner_name.clone(),
            amounts: amounts(k1),
            through_event,
            event_hash: event_hash.clone(),
            warnings: bundle.warnings.clone(),
        })
        .collect())
}

/// One partner's K-1 package for `year`.
pub fn for_partner(
    conn: &Connection,
    year: i32,
    partner_id: &str,
) -> Result<K1Package, K1PackageError> {
    for_year(conn, year)?
        .into_iter()
        .find(|p| p.partner_id == partner_id)
        .ok_or_else(|| K1PackageError::NoSuchPartner {
            partner_id: partner_id.to_string(),
            year,
        })
}

/// The last event in the books, and its hash in hex.
fn head(conn: &Connection) -> Result<(i64, String), K1PackageError> {
    conn.query_row(
        "SELECT id, hash FROM events ORDER BY id DESC LIMIT 1",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)),
    )
    .optional()?
    .map(|(id, hash)| (id, hex::encode(hash)))
    .ok_or(K1PackageError::NoEvents)
}

/// Schedule K lines that never reach a K-1 box of their own: 3a and 3b are the
/// two halves of 3c, which is box 3.
#[cfg(test)]
const SCHEDULE_K_ONLY: &[&str] = &["k3a", "k3b"];

/// The K-1 box a Schedule K line's share is reported in.
pub fn box_for_line(key: &str) -> Option<&'static str> {
    Some(match key {
        "k1" => "1",
        "k2" => "2",
        "k3c" => "3",
        "k4a" => "4a",
        "k4b" => "4b",
        "k4c" => "4c",
        "k5" => "5",
        "k6a" => "6a",
        "k6b" => "6b",
        "k6c" => "6c",
        "k7" => "7",
        "k8" => "8",
        "k9a" => "9a",
        "k9b" => "9b",
        "k9c" => "9c",
        "k10" => "10",
        "k11" => "11",
        "k12" => "12",
        "k13a" => "13_cash_contributions",
        "k13b" => "13_noncash_contributions",
        "k13c" => "13_investment_interest",
        "k13d" => "13_section_59e2",
        "k13e" => "13_other",
        "k14a" => "14a",
        "k14b" => "14b",
        "k14c" => "14c",
        "k18a" => "18a",
        "k18b" => "18b",
        "k18c" => "18c",
        "k19a" => "19a",
        "k19b" => "19b",
        "k20a" => "20a",
        "k20b" => "20b",
        "k21" => "21",
        _ => return None,
    })
}

/// A K-1's figures as box amounts, in cents.
///
/// A box that came to nothing is left out, as the K-1 leaves it blank. Items L
/// and K are the exception: they are positions, and a capital account or a share
/// of recourse debt that ended the year at zero is a figure a basis computation
/// needs, so every row is carried whenever the K-1 reports the item.
pub fn amounts(k1: &K1Figures) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    for (key, dollars) in &k1.by_line {
        if *dollars == 0 {
            continue;
        }
        if let Some(code) = box_for_line(key) {
            out.insert(code.to_string(), dollars * 100);
        }
    }
    if let Some(q) = k1.qbi {
        for (code, dollars) in [
            ("20z_qbi", q.qbi()),
            ("20z_w2_wages", q.w2_wages),
            ("20z_ubia", q.ubia),
        ] {
            if dollars != 0 {
                out.insert(code.to_string(), dollars * 100);
            }
        }
    }
    if let Some(c) = &k1.capital {
        for (code, dollars) in [
            ("L_beginning", c.beginning),
            ("L_contributed", c.contributed),
            ("L_income", c.net_income),
            ("L_other", c.other),
            ("L_withdrawals", c.withdrawals),
            ("L_ending", c.ending()),
        ] {
            out.insert(code.to_string(), dollars * 100);
        }
    }
    if let Some(l) = &k1.liabilities {
        for (code, dollars) in [
            ("K_nonrecourse_beginning", l.nonrecourse.begin),
            ("K_nonrecourse_ending", l.nonrecourse.end),
            (
                "K_qualified_nonrecourse_beginning",
                l.qualified_nonrecourse.begin,
            ),
            (
                "K_qualified_nonrecourse_ending",
                l.qualified_nonrecourse.end,
            ),
            ("K_recourse_beginning", l.recourse.begin),
            ("K_recourse_ending", l.recourse.end),
        ] {
            out.insert(code.to_string(), dollars * 100);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::information_returns::FormKind;

    /// A Schedule K line added to the catalogue without a K-1 box here would be
    /// computed, allocated, printed on the K-1 — and silently missing from every
    /// package. This is the tripwire.
    #[test]
    fn every_schedule_k_line_reaches_a_k1_box_or_is_named_as_not_reaching_one() {
        for def in crate::tax::lines::MAPPABLE_LINES
            .iter()
            .filter(|d| d.schedule == crate::tax::lines::Schedule::K)
        {
            assert!(
                box_for_line(def.key).is_some() || SCHEDULE_K_ONLY.contains(&def.key),
                "Schedule K line {} ({}) reaches no K-1 box",
                def.number,
                def.key
            );
        }
    }

    /// And every box a package can produce is one the receiving books accept.
    #[test]
    fn every_code_a_package_produces_is_in_the_k1_catalogue() {
        let mut figures = K1Figures::default();
        let keys = crate::tax::lines::MAPPABLE_LINES
            .iter()
            .map(|d| d.key)
            .chain(["k1", "k3c", "k4c"]);
        for key in keys {
            figures.by_line.insert(key, 1);
        }
        figures.qbi = Some(crate::tax::qbi::Share {
            ordinary: 10,
            section_179: 1,
            w2_wages: 2,
            ubia: 3,
        });
        figures.capital = Some(Default::default());
        let one = crate::tax::schedule_l::Period { begin: 1, end: 1 };
        figures.liabilities = Some(crate::tax::liabilities::PartnerLiabilities {
            partner_id: "p".to_string(),
            nonrecourse: one,
            qualified_nonrecourse: one,
            recourse: one,
            guaranteed: false,
        });
        for code in amounts(&figures).keys() {
            assert!(
                FormKind::K1Partnership.accepts_box(code),
                "a package can produce box {code:?}, which the K-1 catalogue refuses"
            );
        }
    }

    #[test]
    fn amounts_are_cents_and_blank_boxes_are_left_out() {
        let mut figures = K1Figures::default();
        figures.by_line.insert("k1", 7_329);
        figures.by_line.insert("k5", 0);
        figures.by_line.insert("k3a", 500);
        let out = amounts(&figures);
        assert_eq!(out.get("1"), Some(&732_900));
        assert!(!out.contains_key("5"), "a zero box is blank");
        assert_eq!(out.len(), 1, "3a is Schedule K only");
    }
}
