//! Our realized gains against the broker's 1099-B — the cross-check, not the
//! return.
//!
//! # What this is for
//!
//! INVESTMENTS-SPEC.md §8 puts the 1099-B in charge of the filed numbers, and that
//! is [`super::schedule_d`]'s job. This module is the reason the lot register was
//! worth building anyway: it is the only thing that can tell you the broker's form
//! and your books disagree. A difference is a **finding**. Nothing here adjusts a
//! figure, suppresses a difference, or decides which side is right — and that is a
//! deliberate refusal, not an omission. The broker's form is filed; the books are
//! how you know whether to question it.
//!
//! # Why a difference in proceeds means something different from a difference in
//! basis
//!
//! This is the whole diagnostic, and it is worth stating plainly.
//!
//! **Proceeds** are a fact: what the shares sold for. No lot method, no wash-sale
//! rule and no accounting choice changes them. So a difference in proceeds means a
//! *transaction* is different — one of us has a sale the other does not, or a
//! corporate action turned into a sale on one side, or the broker reports proceeds
//! gross where we carry them net of commission.
//!
//! **Basis** is a computation. Two honest parties can reach different numbers from
//! the same trades:
//!
//! - a **wash sale**, which the broker applies across the whole account and we do
//!   not model at all (spec §4). Its adjustment is captured from the form and
//!   carried, never computed.
//! - **average cost**, which is permitted for mutual fund shares and not for
//!   stocks. A fund difference is quite likely two legitimate methods rather than
//!   anybody's error, and this module says so rather than raising an alarm.
//! - a **corporate action** — a split, a merger, a spin-off, a return of capital —
//!   that the importer reported as a transfer or did not report at all (spec §7).
//!
//! So a finding names both differences separately and orders its likely causes by
//! which one moved.
//!
//! # What can and cannot be matched per security
//!
//! A 1099-B's *subtotals* name no security, so a category with only subtotals can
//! only be compared in total. A 1099-B's *transaction detail* names one in column
//! (a), as free text a broker wrote — "100 sh. ACME CORP COM". Where that text
//! contains a security's ticker or its name, this module compares that security on
//! its own; where it does not, it says which descriptions it could not place rather
//! than guessing.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::commands::investment_commands::{self, RealizedPortion, Security};
use crate::events::types::HoldingTerm;
use crate::tax::schedule_d::Brokerage1099B;

/// Proceeds, basis and gain — the three figures a comparison is made of.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub proceeds_cents: i64,
    pub basis_cents: i64,
    pub gain_cents: i64,
}

impl Totals {
    fn add(&mut self, other: Totals) {
        self.proceeds_cents += other.proceeds_cents;
        self.basis_cents += other.basis_cents;
        self.gain_cents += other.gain_cents;
    }
}

/// The same three figures, split short and long, which is the split both sides
/// have in common: our register knows a term per lot, and the 1099-B's categories
/// each belong to one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ByTerm {
    pub short: Totals,
    pub long: Totals,
}

impl ByTerm {
    pub fn of(&self, term: HoldingTerm) -> Totals {
        match term {
            HoldingTerm::Short => self.short,
            HoldingTerm::Long => self.long,
        }
    }

    fn add(&mut self, term: HoldingTerm, totals: Totals) {
        match term {
            HoldingTerm::Short => self.short.add(totals),
            HoldingTerm::Long => self.long.add(totals),
        }
    }
}

/// What a difference is likely to be, ordered by the evidence.
///
/// Never a verdict: several can apply to one finding, and the list is what a person
/// reads before deciding which to go and check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cause {
    /// The broker sold something the books do not have.
    MissingTrade,
    /// The books have a sale the broker's form does not.
    ExtraTrade,
    /// The broker reports proceeds before the commission, and we carry them after
    /// it. The difference is then exactly the commissions.
    CommissionTreatment,
    /// A wash sale, which the broker applies and we do not model (spec §4).
    WashSale,
    /// Average cost, permitted for fund shares and not for stocks — two legitimate
    /// methods, not an error.
    AverageCost,
    /// A split, a merger, a spin-off or a return of capital the books did not take
    /// up (spec §7).
    CorporateAction,
}

impl Cause {
    /// One sentence a person can act on.
    pub fn explain(self) -> &'static str {
        match self {
            Cause::MissingTrade => {
                "A sale the broker reported is not in the books. Proceeds are a fact no method \
                 changes, so a difference in them is a transaction and not a computation."
            }
            Cause::ExtraTrade => {
                "The books hold a sale the form does not report. It may belong to another account, \
                 to another year's settlement, or to a transfer entered as a sale."
            }
            Cause::CommissionTreatment => {
                "The difference in proceeds is exactly the commissions on these sales, so the \
                 broker is reporting proceeds gross where the books carry them net of the \
                 commission. The gain is the same either way."
            }
            Cause::WashSale => {
                "The form carries a wash-sale adjustment. The 30-day rule reaches across every \
                 account you hold, so the broker's figure is the one to file and the books are \
                 expected to differ by it — see INVESTMENTS-SPEC.md §4."
            }
            Cause::AverageCost => {
                "A mutual fund is involved, and average cost is permitted for fund shares where it \
                 is not for stocks. Two legitimate methods reach two different basis figures from \
                 the same trades; this is likely a method difference rather than an error."
            }
            Cause::CorporateAction => {
                "A split, a merger, a spin-off or a return of capital the broker applied and the \
                 books did not take up. Guessing one wrong restates every gain on the security, so \
                 it is reported and never inferred — see INVESTMENTS-SPEC.md §7."
            }
        }
    }
}

/// What the comparison is about: a whole term, or one security within it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every short-term or every long-term figure the form reports, against every
    /// one the books do.
    Term(HoldingTerm),
    /// One security within one term, matched through a 1099-B detail row's
    /// description.
    Security {
        ticker: String,
        name: String,
        term: HoldingTerm,
    },
}

impl Scope {
    /// Which term the comparison is within. Every scope has one: the form's
    /// categories each belong to a term, and so does every lot a sale consumed.
    pub fn term(&self) -> HoldingTerm {
        match self {
            Scope::Term(term) => *term,
            Scope::Security { term, .. } => *term,
        }
    }

    fn describe(&self) -> String {
        match self {
            Scope::Term(HoldingTerm::Short) => "Short-term total".to_string(),
            Scope::Term(HoldingTerm::Long) => "Long-term total".to_string(),
            Scope::Security { ticker, term, .. } => format!(
                "{ticker}, {}",
                match term {
                    HoldingTerm::Short => "short term",
                    HoldingTerm::Long => "long term",
                }
            ),
        }
    }
}

/// One difference between the form and the books.
///
/// Every difference is **form less books**: positive means the broker reports more
/// than the books hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub scope: Scope,
    pub statement: Totals,
    pub ledger: Totals,
    pub proceeds_difference_cents: i64,
    pub basis_difference_cents: i64,
    pub gain_difference_cents: i64,
    /// Most likely first.
    pub causes: Vec<Cause>,
    /// The finding as a person reads it, differences and all.
    pub description: String,
}

/// A year's comparison, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    pub tax_year: i32,
    /// From the 1099-Bs: categories A, B and C are short term; D, E and F long.
    pub statement: ByTerm,
    /// From the sale register.
    pub ledger: ByTerm,
    /// Empty when the two agree, which is the ordinary outcome.
    pub findings: Vec<Finding>,
    /// The books' realized gain per security and term, always present — the
    /// context somebody needs to find a difference the totals only hint at.
    pub ledger_by_security: Vec<RealizedPortion>,
    /// Detail-row descriptions no security on the master could be recognised in.
    /// Said rather than guessed at.
    pub unmatched_descriptions: Vec<String>,
    /// Whether the books held any sale at all in the year. A year with none and a
    /// form with figures is not a rounding difference.
    pub ledger_has_sales: bool,
}

/// Compare the year's 1099-Bs with the year's sale register.
pub fn reconcile(conn: &Connection, year: i32, forms: &[Brokerage1099B]) -> Reconciliation {
    let securities = investment_commands::list_securities(conn);
    let portions = investment_commands::realized_in_year(conn, year);
    reconcile_from(year, forms, &portions, &securities)
}

/// The same, from figures already in hand.
pub fn reconcile_from(
    year: i32,
    forms: &[Brokerage1099B],
    portions: &[RealizedPortion],
    securities: &[Security],
) -> Reconciliation {
    let mut statement = ByTerm::default();
    let mut statement_adjustments: BTreeMap<HoldingTerm, i64> = BTreeMap::new();
    for form in forms {
        for (&category, totals) in &form.categories {
            statement.add(
                category.term(),
                Totals {
                    proceeds_cents: totals.proceeds_cents,
                    basis_cents: totals.basis_cents,
                    gain_cents: totals.computed_gain_cents(),
                },
            );
            *statement_adjustments.entry(category.term()).or_default() +=
                totals.adjustment_cents;
        }
    }

    let mut ledger = ByTerm::default();
    let mut ledger_fees: BTreeMap<HoldingTerm, i64> = BTreeMap::new();
    for p in portions {
        ledger.add(
            p.term,
            Totals {
                proceeds_cents: p.proceeds_cents,
                basis_cents: p.basis_cents,
                gain_cents: p.gain_cents,
            },
        );
        *ledger_fees.entry(p.term).or_default() += p.fee_cents;
    }

    let by_id: BTreeMap<&str, &Security> = securities
        .iter()
        .map(|s| (s.security_id.as_str(), s))
        .collect();
    let funds_by_term = fund_terms(portions, &by_id);

    let mut findings = Vec::new();
    for term in [HoldingTerm::Short, HoldingTerm::Long] {
        let s = statement.of(term);
        let l = ledger.of(term);
        if s == l {
            continue;
        }
        let evidence = Evidence {
            statement_adjustment_cents: statement_adjustments.get(&term).copied().unwrap_or(0),
            ledger_fee_cents: ledger_fees.get(&term).copied().unwrap_or(0),
            involves_a_fund: funds_by_term.contains(&term),
        };
        findings.push(finding(Scope::Term(term), s, l, &evidence));
    }

    // Per security, where a detail row names one we recognise.
    let (matched, unmatched_descriptions) = match_detail(forms, securities);
    let ledger_by_security_key: BTreeMap<(&str, HoldingTerm), &RealizedPortion> = portions
        .iter()
        .map(|p| ((p.security_id.as_str(), p.term), p))
        .collect();
    for ((security_id, term), (s, adjustment)) in matched {
        let security = by_id.get(security_id.as_str());
        let l = ledger_by_security_key
            .get(&(security_id.as_str(), term))
            .map(|p| Totals {
                proceeds_cents: p.proceeds_cents,
                basis_cents: p.basis_cents,
                gain_cents: p.gain_cents,
            })
            .unwrap_or_default();
        if s == l {
            continue;
        }
        let evidence = Evidence {
            statement_adjustment_cents: adjustment,
            ledger_fee_cents: ledger_by_security_key
                .get(&(security_id.as_str(), term))
                .map_or(0, |p| p.fee_cents),
            involves_a_fund: security.is_some_and(|s| is_a_fund(&s.kind)),
        };
        findings.push(finding(
            Scope::Security {
                ticker: security.map_or(security_id.clone(), |s| s.ticker.clone()),
                name: security.map_or_else(String::new, |s| s.name.clone()),
                term,
            },
            s,
            l,
            &evidence,
        ));
    }

    Reconciliation {
        tax_year: year,
        statement,
        ledger,
        findings,
        ledger_by_security: portions.to_vec(),
        unmatched_descriptions,
        ledger_has_sales: !portions.is_empty(),
    }
}

/// What the finding's causes are weighed against.
struct Evidence {
    statement_adjustment_cents: i64,
    ledger_fee_cents: i64,
    involves_a_fund: bool,
}

fn finding(scope: Scope, statement: Totals, ledger: Totals, evidence: &Evidence) -> Finding {
    let proceeds = statement.proceeds_cents - ledger.proceeds_cents;
    let basis = statement.basis_cents - ledger.basis_cents;
    let gain = statement.gain_cents - ledger.gain_cents;

    let mut causes = Vec::new();
    if proceeds != 0 {
        // Gross-versus-net first, because it is the one cause that is not a
        // problem: it explains the difference exactly and changes no gain.
        if evidence.ledger_fee_cents != 0 && proceeds == evidence.ledger_fee_cents {
            causes.push(Cause::CommissionTreatment);
        } else if proceeds > 0 {
            causes.push(Cause::MissingTrade);
        } else {
            causes.push(Cause::ExtraTrade);
        }
    }
    if basis != 0 {
        if evidence.statement_adjustment_cents != 0 {
            causes.push(Cause::WashSale);
        }
        if evidence.involves_a_fund {
            causes.push(Cause::AverageCost);
        }
        causes.push(Cause::CorporateAction);
    } else if proceeds != 0 && !causes.contains(&Cause::CommissionTreatment) {
        causes.push(Cause::CorporateAction);
    }

    let description = format!(
        "{}: the 1099-B reports {} of proceeds, {} of basis and {} of gain; the books hold {}, {} \
         and {}. Proceeds differ by {}, basis by {} and gain by {}.",
        scope.describe(),
        dollars(statement.proceeds_cents),
        dollars(statement.basis_cents),
        dollars(statement.gain_cents),
        dollars(ledger.proceeds_cents),
        dollars(ledger.basis_cents),
        dollars(ledger.gain_cents),
        dollars(proceeds),
        dollars(basis),
        dollars(gain),
    );

    Finding {
        scope,
        statement,
        ledger,
        proceeds_difference_cents: proceeds,
        basis_difference_cents: basis,
        gain_difference_cents: gain,
        causes,
        description,
    }
}

/// Which terms have a fund in them, so a basis difference in that term can be
/// offered the average-cost explanation.
fn fund_terms(
    portions: &[RealizedPortion],
    by_id: &BTreeMap<&str, &Security>,
) -> BTreeSet<HoldingTerm> {
    portions
        .iter()
        .filter(|p| by_id.get(p.security_id.as_str()).is_some_and(|s| is_a_fund(&s.kind)))
        .map(|p| p.term)
        .collect()
}

/// Whether a security's kind, in whatever words the broker used, is a fund.
///
/// Free text by design (spec §5), so this matches on the word rather than on a
/// closed list: "mutual fund", "Mutual Fund", "fund" and "bond fund" are all funds,
/// and only funds may use average cost.
fn is_a_fund(kind: &str) -> bool {
    kind.to_ascii_lowercase().contains("fund")
}

type MatchedDetail = BTreeMap<(String, HoldingTerm), (Totals, i64)>;

/// Group a 1099-B's detail rows by the security their description names.
///
/// Returns the matched totals with the adjustment that went with them, and the
/// descriptions nothing matched.
fn match_detail(forms: &[Brokerage1099B], securities: &[Security]) -> (MatchedDetail, Vec<String>) {
    let mut matched: MatchedDetail = BTreeMap::new();
    let mut unmatched = Vec::new();
    for form in forms {
        for line in &form.lines {
            match recognise(&line.description, securities) {
                Some(security) => {
                    let entry = matched
                        .entry((security.security_id.clone(), line.category.term()))
                        .or_insert((Totals::default(), 0));
                    entry.0.add(Totals {
                        proceeds_cents: line.proceeds_cents,
                        basis_cents: line.basis_cents,
                        gain_cents: line.gain_cents(),
                    });
                    entry.1 += line.adjustment_cents;
                }
                None => unmatched.push(line.description.clone()),
            }
        }
    }
    unmatched.sort();
    unmatched.dedup();
    (matched, unmatched)
}

/// Which security a broker's free-text description is about, if it can be told.
///
/// The ticker as a whole word, or the name as a substring. Whole word because "F"
/// (Ford) would otherwise match every description containing the letter, and a
/// reconciliation that attributed sales to the wrong security would be worse than
/// one that said it could not tell.
fn recognise<'a>(description: &str, securities: &'a [Security]) -> Option<&'a Security> {
    let upper = description.to_ascii_uppercase();
    let words: BTreeSet<&str> = upper
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    securities
        .iter()
        .find(|s| words.contains(s.ticker.to_ascii_uppercase().as_str()))
        .or_else(|| {
            securities
                .iter()
                .find(|s| !s.name.is_empty() && upper.contains(&s.name.to_ascii_uppercase()))
        })
}

fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.unsigned_abs();
    format!("{sign}${}.{:02}", cents / 100, cents % 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::documents::{Acquired, StatementLine, StatementSource, TaxStatement};
    use crate::tax::information_returns::FormKind;
    use crate::tax::schedule_d::Category;
    use chrono::NaiveDate;

    const YEAR: i32 = 2025;

    fn security(id: &str, ticker: &str, name: &str, kind: &str) -> Security {
        Security {
            security_id: id.to_string(),
            ticker: ticker.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            cusip: None,
            currency: "USD".to_string(),
        }
    }

    fn portion(
        security_id: &str,
        term: HoldingTerm,
        proceeds: i64,
        basis: i64,
        fee: i64,
    ) -> RealizedPortion {
        RealizedPortion {
            security_id: security_id.to_string(),
            term,
            proceeds_cents: proceeds,
            basis_cents: basis,
            gain_cents: proceeds - basis,
            fee_cents: fee,
        }
    }

    fn form(broker: &str, boxes: &[(&str, i64)], lines: Vec<StatementLine>) -> Brokerage1099B {
        let statement = TaxStatement {
            statement_id: format!("s-{broker}"),
            tax_year: YEAR,
            form: FormKind::F1099B,
            issuer: broker.to_string(),
            amounts: boxes.iter().map(|(c, v)| (c.to_string(), *v)).collect(),
            document_ids: Vec::new(),
            source: StatementSource::Entered,
            note: None,
        };
        Brokerage1099B::from_statement(&statement, lines).unwrap()
    }

    fn detail(
        category: Category,
        description: &str,
        proceeds: i64,
        basis: i64,
        adjustment: i64,
    ) -> StatementLine {
        StatementLine {
            statement_id: "s".to_string(),
            line_id: description.to_string(),
            category,
            description: description.to_string(),
            acquired: Acquired::On(NaiveDate::from_ymd_opt(2023, 1, 5).unwrap()),
            sold_on: NaiveDate::from_ymd_opt(YEAR, 7, 9).unwrap(),
            proceeds_cents: proceeds,
            basis_cents: basis,
            adjustment_code: (adjustment != 0).then(|| "W".to_string()),
            adjustment_cents: adjustment,
        }
    }

    /// The ordinary outcome: the books agree with the form and there is nothing
    /// to say.
    #[test]
    fn a_ledger_that_matches_the_statement_produces_no_findings() {
        let f = form(
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 1_000_000),
                ("a_basis", 800_000),
                ("d_proceeds", 2_000_000),
                ("d_basis", 1_500_000),
            ],
            Vec::new(),
        );
        let securities = vec![security("s1", "ACME", "Acme Corp", "stock")];
        let portions = vec![
            portion("s1", HoldingTerm::Short, 1_000_000, 800_000, 0),
            portion("s1", HoldingTerm::Long, 2_000_000, 1_500_000, 0),
        ];
        let r = reconcile_from(YEAR, &[f], &portions, &securities);
        assert!(r.findings.is_empty(), "{:?}", r.findings);
        assert_eq!(r.statement.short.gain_cents, 200_000);
        assert_eq!(r.ledger.short.gain_cents, 200_000);
        assert_eq!(r.statement.long.gain_cents, 500_000);
        assert!(r.ledger_has_sales);
    }

    /// A sale the books never got: proceeds are a fact, so the difference is a
    /// transaction and the finding says so first.
    #[test]
    fn a_trade_missing_from_the_books_shows_as_a_proceeds_difference() {
        let f = form(
            "Broad Street Brokerage",
            &[("a_proceeds", 1_500_000), ("a_basis", 1_200_000)],
            Vec::new(),
        );
        let securities = vec![security("s1", "ACME", "Acme Corp", "stock")];
        let portions = vec![portion("s1", HoldingTerm::Short, 1_000_000, 800_000, 0)];
        let r = reconcile_from(YEAR, &[f], &portions, &securities);

        assert_eq!(r.findings.len(), 1);
        let finding = &r.findings[0];
        assert_eq!(finding.scope, Scope::Term(HoldingTerm::Short));
        assert_eq!(finding.proceeds_difference_cents, 500_000);
        assert_eq!(finding.basis_difference_cents, 400_000);
        assert_eq!(finding.gain_difference_cents, 100_000);
        assert_eq!(finding.causes[0], Cause::MissingTrade);
        assert!(finding.description.contains("Proceeds differ by $5000.00"));
        assert!(finding.description.contains("basis by $4000.00"));
        assert!(finding.description.contains("gain by $1000.00"));
        crate::tax::warning_shape::assert_all([&finding.description]);
        crate::tax::warning_shape::assert_all(finding.causes.iter().map(|c| c.explain()));
    }

    /// A fund's basis differs and its proceeds do not: two permitted methods, and
    /// the finding is required to say so rather than raise an alarm.
    #[test]
    fn a_funds_basis_difference_is_offered_as_a_method_difference() {
        let f = form(
            "Old Mutual Trust",
            &[
                ("d_proceeds", 3_000_000),
                ("d_basis", 2_400_000),
            ],
            Vec::new(),
        );
        let securities = vec![security("s1", "VFIAX", "Vanguard 500 Index Fund", "mutual fund")];
        // Same proceeds, and a basis that differs because the broker averaged.
        let portions = vec![portion("s1", HoldingTerm::Long, 3_000_000, 2_350_000, 0)];
        let r = reconcile_from(YEAR, &[f], &portions, &securities);

        assert_eq!(r.findings.len(), 1);
        let finding = &r.findings[0];
        assert_eq!(finding.proceeds_difference_cents, 0, "no trade is missing");
        assert_eq!(finding.basis_difference_cents, 50_000);
        assert_eq!(finding.gain_difference_cents, -50_000);
        assert_eq!(
            finding.causes,
            vec![Cause::AverageCost, Cause::CorporateAction],
            "a method difference is named before a corporate action, and no trade is blamed"
        );
        assert!(!finding.causes.contains(&Cause::MissingTrade));
        assert!(Cause::AverageCost.explain().contains("permitted for fund shares"));
        assert!(Cause::AverageCost.explain().contains("method difference"));
    }

    /// A wash sale on the form, which the books do not model: the adjustment is
    /// the explanation and the broker's figure is what gets filed.
    #[test]
    fn a_wash_sale_adjustment_on_the_form_explains_a_basis_difference() {
        let f = form(
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 1_000_000),
                ("a_basis", 1_100_000),
                ("a_adjustments", 60_000),
            ],
            Vec::new(),
        );
        let securities = vec![security("s1", "ACME", "Acme Corp", "stock")];
        let portions = vec![portion("s1", HoldingTerm::Short, 1_000_000, 1_040_000, 0)];
        let r = reconcile_from(YEAR, &[f], &portions, &securities);

        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].causes[0], Cause::WashSale);
        assert_eq!(r.findings[0].basis_difference_cents, 60_000);
        // The form's gain includes column (g); ours does not.
        assert_eq!(r.findings[0].statement.gain_cents, -40_000);
        assert_eq!(r.findings[0].ledger.gain_cents, -40_000);
        assert_eq!(r.findings[0].gain_difference_cents, 0);
    }

    /// A broker reporting proceeds gross where we carry them net of commission is
    /// not a missing trade, and saying it was would send somebody hunting for a
    /// sale that does not exist.
    #[test]
    fn proceeds_gross_of_commission_are_named_as_such_and_not_as_a_missing_trade() {
        let f = form(
            "Broad Street Brokerage",
            &[("a_proceeds", 1_000_995), ("a_basis", 800_000)],
            Vec::new(),
        );
        let securities = vec![security("s1", "ACME", "Acme Corp", "stock")];
        let portions = vec![portion("s1", HoldingTerm::Short, 1_000_000, 800_000, 995)];
        let r = reconcile_from(YEAR, &[f], &portions, &securities);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].causes, vec![Cause::CommissionTreatment]);
    }

    /// A detail row that names a security is compared on its own.
    #[test]
    fn a_detail_row_naming_a_security_is_reconciled_per_security() {
        let f = form(
            "Old Mutual Trust",
            &[("e_proceeds", 900_000), ("e_basis", 400_000)],
            vec![detail(Category::E, "300 sh. HERITAGE CO (HRTG)", 900_000, 400_000, 0)],
        );
        let securities = vec![
            security("s1", "HRTG", "Heritage Co", "stock"),
            security("s2", "ACME", "Acme Corp", "stock"),
        ];
        let portions = vec![portion("s1", HoldingTerm::Long, 900_000, 450_000, 0)];
        let r = reconcile_from(YEAR, &[f], &portions, &securities);

        assert!(r.unmatched_descriptions.is_empty());
        let per_security = r
            .findings
            .iter()
            .find(|f| matches!(&f.scope, Scope::Security { ticker, .. } if ticker == "HRTG"))
            .expect("the row names HRTG");
        assert_eq!(per_security.basis_difference_cents, -50_000);
        assert_eq!(per_security.proceeds_difference_cents, 0);
    }

    /// A description nothing on the master matches is said, not guessed at.
    #[test]
    fn a_description_no_security_matches_is_reported_rather_than_attributed() {
        let f = form(
            "Old Mutual Trust",
            &[("e_proceeds", 100_000), ("e_basis", 50_000)],
            vec![detail(Category::E, "42 units SOMETHING ELSE TRUST", 100_000, 50_000, 0)],
        );
        let securities = vec![security("s1", "ACME", "Acme Corp", "stock")];
        let r = reconcile_from(YEAR, &[f], &[], &securities);
        assert_eq!(r.unmatched_descriptions, vec!["42 units SOMETHING ELSE TRUST"]);
        assert!(r
            .findings
            .iter()
            .all(|f| matches!(f.scope, Scope::Term(_))));
    }

    /// A one-letter ticker must not match every description that contains the
    /// letter.
    #[test]
    fn a_ticker_matches_as_a_whole_word_and_not_as_a_letter() {
        let securities = vec![security("s1", "F", "Ford Motor Co", "stock")];
        assert!(recognise("100 sh. ACME CORP", &securities).is_none());
        assert_eq!(recognise("50 sh. F", &securities).unwrap().security_id, "s1");
        assert_eq!(
            recognise("50 sh. FORD MOTOR CO", &securities).unwrap().security_id,
            "s1"
        );
    }

    #[test]
    fn a_year_with_a_form_and_no_sales_at_all_says_so() {
        let f = form("Broad Street Brokerage", &[("a_proceeds", 500_000)], Vec::new());
        let r = reconcile_from(YEAR, &[f], &[], &[]);
        assert!(!r.ledger_has_sales);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].causes[0], Cause::MissingTrade);
    }

    #[test]
    fn every_cause_reads_as_a_sentence() {
        crate::tax::warning_shape::assert_all(
            [
                Cause::MissingTrade,
                Cause::ExtraTrade,
                Cause::CommissionTreatment,
                Cause::WashSale,
                Cause::AverageCost,
                Cause::CorporateAction,
            ]
            .iter()
            .map(|c| c.explain()),
        );
    }
}
