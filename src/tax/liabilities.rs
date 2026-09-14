//! Schedule K-1 item K: each partner's share of the partnership's liabilities.
//!
//! # Where the figures come from
//!
//! The liabilities are Schedule L's — every account mapped to lines 15 to 20 —
//! read on the day before the year opens and on its last day, as Schedule L reads
//! them. What the ledger cannot say is who bears the economic risk of loss on each
//! one under §752, so that is recorded per account
//! ([`crate::domain::LiabilityClass`]) and, where nothing is recorded, taken from
//! the kind of entity Schedule B question 1 names:
//!
//! - an LLC's or LLP's debts are nonrecourse: no member is personally liable for
//!   them unless they guaranteed or lent, which is what a classification records;
//! - a general partnership's are recourse, shared on the loss percentages;
//! - line 18, "all nonrecourse loans", is nonrecourse whatever the entity;
//! - anything else is left out and named, because a guess would put real figures
//!   in real boxes.
//!
//! A loan from a partner (line 19a) still takes the default, so item K foots to
//! Schedule L, but is named: it is recourse to the partner who made it
//! (§1.752-2(c)), and only a classification can say who that was.
//!
//! # How each kind is shared
//!
//! Nonrecourse liabilities and qualified nonrecourse financing follow the profit
//! percentages on each date — the §1.752-3(a)(3) default for excess nonrecourse
//! liabilities. A recourse liability goes to the partner its classification
//! names, or on the loss percentages when it names none. Each kind is totalled,
//! rounded to dollars and then divided, so every column adds back exactly.

use std::collections::BTreeMap;

use chrono::{Duration, NaiveDate};
use rusqlite::Connection;

use super::allocate::{allocate_as_of, Basis};
use super::lines::{cents_to_dollars, format_dollars};
use super::schedule_l::Period;
use crate::domain::{LiabilityClass, LiabilityKind, Partner};
use crate::queries::reports::Reports;

/// The Schedule L lines that hold liabilities.
pub const LIABILITY_LINES: &[&str] = &["sl15", "sl16", "sl17", "sl18", "sl19a", "sl19b", "sl20"];

/// One partner's item K, in whole dollars.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartnerLiabilities {
    pub partner_id: String,
    pub nonrecourse: Period,
    pub qualified_nonrecourse: Period,
    pub recourse: Period,
    /// A liability in this partner's recourse share is one they guaranteed —
    /// item K3.
    pub guaranteed: bool,
}

/// A liability account's balances, in cents, as Schedule L prints them: positive
/// is owed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Liability {
    pub account_id: String,
    /// Number and name, for warnings.
    pub label: String,
    /// The Schedule L line the account is mapped to.
    pub line: &'static str,
    pub begin_cents: i64,
    pub end_cents: i64,
}

/// Item K for every partner on a return, and what it could not account for.
#[derive(Debug, Clone, Default)]
pub struct ItemK {
    /// One per partner passed in, in the same order.
    pub partners: Vec<PartnerLiabilities>,
    /// Whether any liability reached item K at either date. When one did, every
    /// K-1 carries all six boxes, zeros included.
    pub any: bool,
    pub warnings: Vec<String>,
}

impl ItemK {
    pub fn for_partner(&self, partner_id: &str) -> Option<&PartnerLiabilities> {
        self.partners.iter().find(|p| p.partner_id == partner_id)
    }

    /// Whether nothing was computed at all.
    pub fn is_empty(&self) -> bool {
        self.partners.is_empty() && self.warnings.is_empty()
    }
}

/// The kind an unclassified liability takes from the kind of entity — Schedule B
/// question 1's stored answer.
pub fn default_kind(entity: Option<&str>) -> Option<LiabilityKind> {
    match entity {
        Some("llc") | Some("llp") => Some(LiabilityKind::Nonrecourse),
        Some("general") => Some(LiabilityKind::Recourse),
        _ => None,
    }
}

/// Item K from the ledger: Schedule L's liabilities on the two dates, the
/// classifications on file, and the entity's default for the rest.
pub fn for_return(
    conn: &Connection,
    year: i32,
    partners: &[crate::tax::form1065::PartnerFiling],
    entity: Option<&str>,
) -> ItemK {
    let (year_start, year_end) = crate::commands::partnership_commands::calendar_year(year);
    let mapping = super::lines::load_effective_mapping(conn, year);
    let reports = Reports::new(conn);
    let (Ok(begin), Ok(end)) = (
        reports.balance_sheet(year_start - Duration::days(1)),
        reports.balance_sheet(year_end),
    ) else {
        return ItemK {
            warnings: vec![
                "Item K is blank: the balance sheet could not be read for the year.".to_string(),
            ],
            ..Default::default()
        };
    };

    let mut by_account: BTreeMap<String, Liability> = BTreeMap::new();
    for (sheet, is_end) in [(&begin, false), (&end, true)] {
        for line in &sheet.liabilities.lines {
            if line.balance == 0 {
                continue;
            }
            let Some(key) = mapping
                .get(&line.account_id)
                .and_then(|k| LIABILITY_LINES.iter().find(|l| **l == k.as_str()))
            else {
                continue;
            };
            let entry = by_account
                .entry(line.account_id.clone())
                .or_insert_with(|| Liability {
                    account_id: line.account_id.clone(),
                    label: format!("{} {}", line.account_number, line.account_name),
                    line: key,
                    begin_cents: 0,
                    end_cents: 0,
                });
            // Credit-normal, held negative: owed is positive.
            if is_end {
                entry.end_cents -= line.balance;
            } else {
                entry.begin_cents -= line.balance;
            }
        }
    }

    let people: Vec<&Partner> = partners.iter().map(|f| &f.partner).collect();
    let classes = crate::commands::partnership_commands::list_liability_classes(conn);
    let liabilities: Vec<Liability> = by_account.into_values().collect();
    split(
        &liabilities,
        &classes,
        default_kind(entity),
        &people,
        year_start,
        year_end,
    )
}

/// Divide the liabilities across the partners. Split from [`for_return`] so the
/// arithmetic is testable without a ledger.
pub fn split(
    liabilities: &[Liability],
    classes: &[LiabilityClass],
    default: Option<LiabilityKind>,
    partners: &[&Partner],
    year_start: NaiveDate,
    year_end: NaiveDate,
) -> ItemK {
    let mut out: Vec<PartnerLiabilities> = partners
        .iter()
        .map(|p| PartnerLiabilities {
            partner_id: p.partner_id.clone(),
            ..Default::default()
        })
        .collect();
    let mut warnings = Vec::new();

    // Totals in cents per kind and, for recourse, per partner who bears it.
    let mut buckets: Vec<(LiabilityKind, Option<String>, i64, i64)> = Vec::new();
    let mut defaulted = false;
    for l in liabilities {
        let class = classes.iter().find(|c| c.account_id == l.account_id);
        let (kind, partner) = match class {
            Some(c) => {
                if let (true, Some(pid)) = (c.guaranteed, &c.partner_id) {
                    if let Some(o) = out.iter_mut().find(|o| &o.partner_id == pid) {
                        o.guaranteed = true;
                    }
                }
                (c.kind, c.partner_id.clone())
            }
            None if l.line == "sl18" => (LiabilityKind::Nonrecourse, None),
            None => match default {
                Some(k) => {
                    defaulted = true;
                    if l.line == "sl19a" {
                        warnings.push(format!(
                            "Item K: {} is a loan from partners, which is recourse to the partner \
                             who made it (§1.752-2(c)), but nothing says who that was, so it was \
                             counted as {} with the rest. Classify it as recourse to the lending \
                             partner.",
                            l.label,
                            k.label()
                        ));
                    }
                    (k, None)
                }
                None => {
                    warnings.push(format!(
                        "Item K: {} ({} at the start of the year, {} at the end) has no \
                         classification, and the kind of entity (Schedule B question 1) gives no \
                         default, so it is left out of item K. Classify it.",
                        l.label,
                        format_dollars(cents_to_dollars(l.begin_cents)),
                        format_dollars(cents_to_dollars(l.end_cents))
                    ));
                    continue;
                }
            },
        };
        if let Some(pid) = &partner {
            if !partners.iter().any(|p| &p.partner_id == pid) {
                warnings.push(format!(
                    "Item K: {} is recourse to a partner who is not on this return, so it is left \
                     out of item K.",
                    l.label
                ));
                continue;
            }
        }
        match buckets
            .iter_mut()
            .find(|(k, p, _, _)| *k == kind && *p == partner)
        {
            Some(b) => {
                b.2 += l.begin_cents;
                b.3 += l.end_cents;
            }
            None => buckets.push((kind, partner, l.begin_cents, l.end_cents)),
        }
    }

    let mut any = false;
    let (mut total_begin, mut total_end) = (0i64, 0i64);
    for (kind, partner, begin_cents, end_cents) in &buckets {
        for (cents, day, is_end) in [
            (*begin_cents, year_start, false),
            (*end_cents, year_end, true),
        ] {
            let dollars = cents_to_dollars(cents);
            if dollars == 0 {
                continue;
            }
            any = true;
            if is_end {
                total_end += dollars;
            } else {
                total_begin += dollars;
            }
            let shares: Vec<(usize, i64)> = match (partner, kind) {
                (Some(pid), _) => partners
                    .iter()
                    .position(|p| &p.partner_id == pid)
                    .map(|i| vec![(i, dollars)])
                    .unwrap_or_default(),
                (None, LiabilityKind::Recourse) => {
                    allocate_as_of(dollars, partners, Basis::Loss, Some(day))
                        .into_iter()
                        .map(|s| (s.partner, s.dollars))
                        .collect()
                }
                (None, _) => allocate_as_of(dollars, partners, Basis::ProfitOrLoss, Some(day))
                    .into_iter()
                    .map(|s| (s.partner, s.dollars))
                    .collect(),
            };
            for (i, d) in shares {
                let slot = match kind {
                    LiabilityKind::Nonrecourse => &mut out[i].nonrecourse,
                    LiabilityKind::QualifiedNonrecourse => &mut out[i].qualified_nonrecourse,
                    LiabilityKind::Recourse => &mut out[i].recourse,
                };
                if is_end {
                    slot.end += d;
                } else {
                    slot.begin += d;
                }
            }
        }
    }

    if any {
        warnings.push(format!(
            "Item K shares {} of liabilities at the start of the year and {} at the end: \
             nonrecourse liabilities and qualified nonrecourse financing on each date's profit \
             percentages, recourse liabilities to the partner their classification names or else \
             on the loss percentages.{} Check the classifications against the loan documents and \
             any guarantee a partner gave.",
            format_dollars(total_begin),
            format_dollars(total_end),
            match (defaulted, default) {
                (true, Some(k)) => format!(
                    " Liabilities with no classification were treated as {}, the default for \
                     this kind of entity.",
                    k.label()
                ),
                _ => String::new(),
            }
        ));
    }

    ItemK {
        partners: out,
        any,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Residency, Shares};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn partner(id: &str, profit: f64, loss: f64) -> Partner {
        Partner {
            history: Vec::new(),
            partner_id: id.into(),
            name: id.into(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: "Individual".into(),
            address: Address {
                street: "1 Main St".into(),
                suite: None,
                city: "Chicago".into(),
                state: "IL".into(),
                postal_code: "60601".into(),
                country: None,
            },
            start_date: day(2023, 1, 1),
            end_date: None,
            shares: Shares::from_percents(profit, loss, profit),
        }
    }

    fn liability(id: &str, line: &'static str, begin: i64, end: i64) -> Liability {
        Liability {
            account_id: id.into(),
            label: id.into(),
            line,
            begin_cents: begin * 100,
            end_cents: end * 100,
        }
    }

    /// An LLC: its debts are nonrecourse on the profit percentages, and a loan
    /// classified as one partner's is recourse to them alone.
    #[test]
    fn an_llcs_liabilities_are_nonrecourse_except_what_a_partner_bears() {
        let (a, b) = (
            partner("active", 51.0, 0.0),
            partner("investor", 49.0, 100.0),
        );
        let people = [&a, &b];
        let liabilities = [
            liability("ap", "sl15", 0, 295),
            liability("other", "sl20", 4_174, 4_509),
            liability("loans", "sl19a", 6_076, 0),
        ];
        let classes = [LiabilityClass {
            account_id: "loans".into(),
            kind: LiabilityKind::Recourse,
            partner_id: Some("investor".into()),
            guaranteed: false,
            note: "Paid personally".into(),
        }];
        let k = split(
            &liabilities,
            &classes,
            default_kind(Some("llc")),
            &people,
            day(2025, 1, 1),
            day(2025, 12, 31),
        );
        let active = k.for_partner("active").unwrap();
        let investor = k.for_partner("investor").unwrap();
        assert_eq!(
            active.nonrecourse,
            Period {
                begin: 2_129,
                end: 2_450
            }
        );
        assert_eq!(
            investor.nonrecourse,
            Period {
                begin: 2_045,
                end: 2_354
            }
        );
        assert_eq!(active.recourse, Period::default());
        assert_eq!(
            investor.recourse,
            Period {
                begin: 6_076,
                end: 0
            }
        );
        assert!(k.any);
        assert!(
            !k.warnings.iter().any(|w| w.contains("loan from partners")),
            "a classified partner loan is not questioned: {:?}",
            k.warnings
        );
    }

    /// With nothing classified: a partner loan is named, a general partnership's
    /// debts are recourse on the loss percentages, an entity with no default leaves
    /// the liability out, and a guarantee ticks K3.
    #[test]
    fn defaults_follow_the_entity_and_say_what_they_could_not_know() {
        let (a, b) = (partner("a", 50.0, 25.0), partner("b", 50.0, 75.0));
        let people = [&a, &b];
        let (start, end) = (day(2025, 1, 1), day(2025, 12, 31));

        let loan = [liability("loans", "sl19a", 1_000, 1_000)];
        let k = split(&loan, &[], default_kind(Some("llc")), &people, start, end);
        assert!(k.warnings.iter().any(|w| w.contains("loan from partners")));
        assert_eq!(k.for_partner("a").unwrap().nonrecourse.end, 500);

        let note = [liability("note", "sl16", 0, 4_000)];
        let k = split(
            &note,
            &[],
            default_kind(Some("general")),
            &people,
            start,
            end,
        );
        assert_eq!(k.for_partner("a").unwrap().recourse.end, 1_000);
        assert_eq!(k.for_partner("b").unwrap().recourse.end, 3_000);

        let k = split(&note, &[], default_kind(Some("other")), &people, start, end);
        assert!(!k.any);
        assert!(k.warnings.iter().any(|w| w.contains("left out of item K")));

        let k = split(
            &note,
            &[LiabilityClass {
                account_id: "note".into(),
                kind: LiabilityKind::Recourse,
                partner_id: Some("b".into()),
                guaranteed: true,
                note: "Personal guarantee".into(),
            }],
            None,
            &people,
            start,
            end,
        );
        assert_eq!(k.for_partner("b").unwrap().recourse.end, 4_000);
        assert!(k.for_partner("b").unwrap().guaranteed);
        assert!(!k.for_partner("a").unwrap().guaranteed);
    }

    /// Line 18 is nonrecourse by its own name, whatever the entity.
    #[test]
    fn all_nonrecourse_loans_are_nonrecourse_without_a_default() {
        let (a, b) = (partner("a", 60.0, 60.0), partner("b", 40.0, 40.0));
        let k = split(
            &[liability("mortgage", "sl18", 10_000, 9_000)],
            &[],
            None,
            &[&a, &b],
            day(2025, 1, 1),
            day(2025, 12, 31),
        );
        assert_eq!(
            k.for_partner("a").unwrap().nonrecourse,
            Period {
                begin: 6_000,
                end: 5_400
            }
        );
    }
}
