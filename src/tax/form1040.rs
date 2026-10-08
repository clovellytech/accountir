//! Form 1040, computed: a person's federal return for one year, from their own
//! books, the statements recorded in them, and the year's profile.
//!
//! # Where each figure comes from
//!
//! - **The year's profile** ([`crate::commands::personal_tax_commands`]): filing
//!   status, the household, estimated payments, carryovers, which accounts are which
//!   rental property. Nothing else here guesses at any of it.
//! - **The investment output** ([`super::personal_return`]): Schedule B from the
//!   income accounts the brokerages' configurations name, Schedule D from the
//!   1099-Bs, retirement distributions from the register.
//! - **Statements**: W-2s, the other 1099s, SSA-1099s, K-1s and Schedule Cs,
//!   box by box. A K-1 or a Schedule C from books kept in accountir arrives here
//!   by being pulled into these books' log (see
//!   [`crate::commands::tax_statement_commands`]), never by reading the other file.
//! - **The ledger**, for one thing only: Schedule E rents and expenses, from the
//!   accounts the profile assigns to each property.
//! - **The year's parameters** ([`super::federal_params`]).
//!
//! # What it does not do, and says so
//!
//! Itemized deductions, the alternative minimum tax, the QBI deduction above the
//! Form 8995 threshold (Form 8995-A), the Additional Medicare Tax, Form 8582
//! beyond the $25,000 allowance, and refundable credits. Each one that the year's
//! figures suggest might matter becomes a warning, not a zero passed off as an
//! answer. Schedule C income, Schedule SE and its deduction, and the QBI
//! deduction below the threshold are computed. The line
//! numbers follow the 2024 form's layout; the 2025 and 2026 forms renumber some of
//! them, which is why every line carries its label.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use thiserror::Error;

use crate::commands::{personal_tax_commands, tax_statement_commands};
use crate::domain::documents::TaxStatement;
use crate::events::types::{FilingStatus, PersonalTaxProfileData};
use crate::store::event_store::EventStore;
use crate::tax::federal_params::{self as fp, apply_bp, tax_on, YearParams};
use crate::tax::information_returns::FormKind;
use crate::tax::personal_return::{self, InvestmentReturn};
use crate::tax::personal_schedule_b::IncomeAccounts;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum Form1040Error {
    #[error(
        "{0} has no tax profile. Record the year's filing status (and household, state and \
         estimated payments) first — a return cannot be computed for nobody in particular."
    )]
    NoProfile(i32),
    #[error(
        "{0} is not a year this version has brackets for (it has {1}). Computing it with \
         another year's would produce a plausible, wrong tax."
    )]
    UnsupportedYear(i32, String),
}

/// One line of the return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnLine {
    /// The 2024 layout's line number: `1a`, `2b`, `16`.
    pub key: &'static str,
    pub label: &'static str,
    pub cents: i64,
    /// Where it came from, or what it leaves out.
    pub note: Option<String>,
}

/// One rental property on Schedule E, Part I.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RentalProperty {
    pub name: String,
    pub rents_cents: i64,
    /// Each expense account the profile names, with everything beneath it.
    pub expenses: Vec<(String, i64)>,
    pub expenses_cents: i64,
    pub net_cents: i64,
}

/// A K-1's contribution to Schedule E, Part II or III.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct K1Line {
    pub issuer: String,
    pub form: FormKind,
    pub cents: i64,
}

/// One business's Schedule C, as it reaches Schedule 1 line 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusinessLine {
    pub issuer: String,
    /// Line 31.
    pub net_profit_cents: i64,
}

/// Schedule SE, short form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleSe {
    /// Line 3: net profit from Schedule C and K-1 box 14a, combined.
    pub combined_cents: i64,
    /// Line 4: 92.35% of it — the net earnings the tax is charged on.
    pub net_earnings_cents: i64,
    /// The 12.4% part, on what the wage base leaves after W-2 Social Security wages.
    pub social_security_cents: i64,
    /// The 2.9% part, on all of it.
    pub medicare_cents: i64,
    /// Line 12: the tax, to Schedule 2 line 4.
    pub tax_cents: i64,
    /// Line 13: half of it, deducted on Schedule 1 line 15.
    pub deduction_cents: i64,
}

/// Form 8995, the simplified QBI deduction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Qbi {
    /// Qualified business income, after the deductible part of SE tax that is
    /// attributable to it.
    pub business_income_cents: i64,
    /// Section 199A dividends (1099-DIV box 5).
    pub reit_dividends_cents: i64,
    /// Taxable income before the deduction, which the limit is 20% of, less net
    /// capital gain.
    pub taxable_income_before_cents: i64,
    pub deduction_cents: i64,
    /// Whether taxable income is over the threshold, so Form 8995-A applies and
    /// nothing is deducted here.
    pub above_threshold: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleE {
    pub properties: Vec<RentalProperty>,
    /// Part I before any loss limit.
    pub rental_net_cents: i64,
    /// What Part I contributes after the $25,000 allowance for a loss.
    pub rental_allowed_cents: i64,
    /// A rental loss the allowance did not cover, carried to next year.
    pub rental_suspended_cents: i64,
    /// Parts II and III.
    pub k1_lines: Vec<K1Line>,
    pub k1_cents: i64,
    /// Line 26: what reaches Schedule 1, line 5.
    pub total_cents: i64,
}

/// A year's federal return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form1040 {
    pub tax_year: i32,
    pub filing_status: FilingStatus,
    pub params_source: &'static str,
    pub params_verified: bool,
    pub lines: Vec<ReturnLine>,
    pub schedule_e: ScheduleE,
    /// Schedule 1 line 3, business by business.
    pub schedule_c: Vec<BusinessLine>,
    pub business_income_cents: i64,
    pub schedule_se: ScheduleSe,
    pub qbi: Qbi,
    pub investment: InvestmentReturn,
    // The figures the state return starts from, named rather than read back out of
    // `lines` by key.
    pub wages_cents: i64,
    pub taxable_interest_cents: i64,
    pub us_obligation_interest_cents: i64,
    pub tax_exempt_interest_cents: i64,
    pub ordinary_dividends_cents: i64,
    pub qualified_dividends_cents: i64,
    pub retirement_taxable_cents: i64,
    pub social_security_taxable_cents: i64,
    pub capital_gain_cents: i64,
    pub agi_cents: i64,
    pub deduction_cents: i64,
    pub taxable_income_cents: i64,
    pub income_tax_cents: i64,
    pub niit_cents: i64,
    pub total_tax_cents: i64,
    pub payments_cents: i64,
    /// Positive is a refund, negative is owed.
    pub balance_cents: i64,
    /// Capital loss not used this year, as (short-term, long-term), for next year's
    /// profile.
    pub loss_carryforward_cents: (i64, i64),
    pub state_withheld_cents: i64,
    pub warnings: Vec<String>,
}

impl Form1040 {
    pub fn line(&self, key: &str) -> Option<&ReturnLine> {
        self.lines.iter().find(|l| l.key == key)
    }
}

/// The income accounts Schedule B reads: every taxable brokerage's configured
/// interest, dividend, tax-exempt and capital-gain-distribution accounts, plus the
/// profile's extras. Named by configuration, never inferred from an account's name.
pub fn income_accounts(conn: &Connection, profile: &PersonalTaxProfileData) -> IncomeAccounts {
    let mut accounts = IncomeAccounts::default();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT interest_income_account_id, dividend_income_account_id,
                tax_exempt_interest_account_id, capital_gain_distribution_account_id
           FROM investment_account_config WHERE treatment = 'taxable'",
    ) {
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        });
        if let Ok(rows) = rows {
            for (interest, dividends, exempt, cap_gain) in rows.flatten() {
                accounts.taxable_interest.extend(interest);
                accounts.ordinary_dividends.extend(dividends);
                accounts.tax_exempt_interest.extend(exempt);
                accounts.capital_gain_distributions.extend(cap_gain);
            }
        }
    }
    accounts
        .taxable_interest
        .extend(profile.extra_interest_account_ids.iter().cloned());
    accounts
        .ordinary_dividends
        .extend(profile.extra_dividend_account_ids.iter().cloned());
    for list in [
        &mut accounts.taxable_interest,
        &mut accounts.ordinary_dividends,
        &mut accounts.tax_exempt_interest,
        &mut accounts.capital_gain_distributions,
    ] {
        let unique: BTreeSet<String> = list.drain(..).collect();
        list.extend(unique);
    }
    accounts
}

/// Sum one box across every statement of the given forms.
fn boxes(statements: &[TaxStatement], forms: &[FormKind], code: &str) -> i64 {
    statements
        .iter()
        .filter(|s| forms.contains(&s.form))
        .map(|s| s.amount(code))
        .sum()
}

/// One account and everything beneath it.
fn subtree(conn: &Connection, root: &str) -> BTreeSet<String> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, parent_id FROM accounts") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        }) {
            for (id, parent) in rows.flatten() {
                if let Some(parent) = parent {
                    children.entry(parent).or_default().push(id);
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    let mut stack = vec![root.to_string()];
    while let Some(id) = stack.pop() {
        if out.insert(id.clone()) {
            stack.extend(children.get(&id).cloned().unwrap_or_default());
        }
    }
    out
}

/// The year's net debits to an account and everything beneath it: an expense's
/// total, or an income account's total negated.
fn year_debits(conn: &Connection, root: &str, year: i32) -> i64 {
    subtree(conn, root)
        .iter()
        .map(|id| {
            conn.query_row(
                "SELECT COALESCE(SUM(jl.amount), 0)
                   FROM journal_lines jl
                   JOIN journal_entries je ON je.id = jl.entry_id
                  WHERE jl.account_id = ?1 AND je.is_void = 0
                    AND je.date >= ?2 AND je.date <= ?3",
                rusqlite::params![id, format!("{year}-01-01"), format!("{year}-12-31")],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
        })
        .sum()
}

fn account_name(conn: &Connection, id: &str) -> String {
    conn.query_row("SELECT name FROM accounts WHERE id = ?1", [id], |r| r.get(0))
        .unwrap_or_else(|_| id.to_string())
}

fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    let whole = (abs / 100).to_string();
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}${grouped}.{:02}", abs % 100)
}

/// What the year's K-1s carry, by where it goes.
#[derive(Debug, Default)]
struct K1Totals {
    interest: i64,
    tax_exempt_interest: i64,
    ordinary_dividends: i64,
    qualified_dividends: i64,
    schedule_e: Vec<K1Line>,
    passive_rental: i64,
    self_employment: i64,
    qbi: i64,
    foreign_tax: i64,
}

fn k1_totals(statements: &[TaxStatement]) -> K1Totals {
    let mut t = K1Totals::default();
    for s in statements {
        // (business/ordinary, rental, interest, ordinary div, qualified div, tax-exempt,
        //  self-employment, QBI, foreign tax) box codes per form.
        let codes: (&[&str], &[&str], &str, &str, &str, &str, &str, &str, &str) = match s.form {
            FormKind::K1Partnership => (
                &["1", "4a", "4b", "7"],
                &["2", "3"],
                "5",
                "6a",
                "6b",
                "18a",
                "14a",
                "20z_qbi",
                "21",
            ),
            FormKind::K1SCorporation => (
                &["1", "6"],
                &["2", "3"],
                "4",
                "5a",
                "5b",
                "16a",
                "",
                "17v_qbi",
                "",
            ),
            FormKind::K1EstateOrTrust => (&["6"], &["7", "8"], "1", "2a", "2b", "", "", "", ""),
            _ => continue,
        };
        let (business, rental, interest, div, qual, exempt, se, qbi, foreign) = codes;
        let business_cents: i64 = business.iter().map(|c| s.amount(c)).sum();
        let rental_cents: i64 = rental.iter().map(|c| s.amount(c)).sum();
        if business_cents + rental_cents != 0 {
            t.schedule_e.push(K1Line {
                issuer: s.issuer.clone(),
                form: s.form,
                cents: business_cents + rental_cents,
            });
        }
        t.passive_rental += rental_cents;
        t.interest += s.amount(interest);
        t.ordinary_dividends += s.amount(div);
        t.qualified_dividends += s.amount(qual);
        if !exempt.is_empty() {
            t.tax_exempt_interest += s.amount(exempt);
        }
        if !se.is_empty() {
            t.self_employment += s.amount(se);
        }
        if !qbi.is_empty() {
            t.qbi += s.amount(qbi);
        }
        if !foreign.is_empty() {
            t.foreign_tax += s.amount(foreign);
        }
    }
    t
}

/// Schedule E Part I, from the accounts the profile assigns to each property.
fn rental_properties(
    conn: &Connection,
    profile: &PersonalTaxProfileData,
    year: i32,
) -> Vec<RentalProperty> {
    profile
        .rental_properties
        .iter()
        .map(|p| {
            let rents_cents: i64 = p
                .income_account_ids
                .iter()
                .map(|id| -year_debits(conn, id, year))
                .sum();
            let expenses: Vec<(String, i64)> = p
                .expense_account_ids
                .iter()
                .map(|id| (account_name(conn, id), year_debits(conn, id, year)))
                .collect();
            let expenses_cents = expenses.iter().map(|(_, c)| c).sum();
            RentalProperty {
                name: p.name.clone(),
                rents_cents,
                expenses,
                expenses_cents,
                net_cents: rents_cents - expenses_cents,
            }
        })
        .collect()
}

/// The taxable part of Social Security benefits (the Form 1040 instructions'
/// worksheet): nothing below the first base amount, half of the excess up to the
/// second, 85% past it, and never more than 85% of the benefits.
pub fn taxable_social_security(
    status: FilingStatus,
    benefits_cents: i64,
    other_income_cents: i64,
) -> i64 {
    if benefits_cents <= 0 {
        return 0;
    }
    let provisional = other_income_cents + benefits_cents / 2;
    let base_one = fp::SS_BASE_ONE.of(status);
    let base_two = fp::SS_BASE_TWO.of(status);
    if provisional <= base_one {
        return 0;
    }
    let first = (provisional.min(base_two) - base_one).max(0) / 2;
    let above = (provisional - base_two).max(0);
    let worksheet = (apply_bp(above, 8_500) + first.min(benefits_cents / 2))
        .min(apply_bp(benefits_cents, 8_500));
    worksheet.max(0)
}

/// The Qualified Dividends and Capital Gain Tax Worksheet: the ordinary part at the
/// ordinary rates, the preferential part at 0, 15 and 20% by where it sits above
/// the ordinary part. Never more than the tax on the whole at ordinary rates.
pub fn qdcg_tax(
    params: &YearParams,
    status: FilingStatus,
    taxable_cents: i64,
    qualified_dividends_cents: i64,
    net_capital_gain_cents: i64,
) -> i64 {
    let ti = taxable_cents.max(0);
    let preferential = (qualified_dividends_cents.max(0) + net_capital_gain_cents.max(0)).min(ti);
    let ordinary = ti - preferential;
    let zero_top = params.capital_gain_zero_max.of(status);
    let fifteen_top = params.capital_gain_fifteen_max.of(status);
    // The 0% band is what is left of it above the ordinary income.
    let at_zero = (ti.min(zero_top) - ordinary).max(0).min(preferential);
    let rest = preferential - at_zero;
    let at_fifteen = (ti.min(fifteen_top) - (ordinary + at_zero)).max(0).min(rest);
    let at_twenty = rest - at_fifteen;
    let worksheet = tax_on(params, status, ordinary)
        + apply_bp(at_fifteen, 1_500)
        + apply_bp(at_twenty, 2_000);
    worksheet.min(tax_on(params, status, ti))
}

/// Schedule SE: self-employment tax on Schedule C net profit and K-1 box 14a.
///
/// Net earnings are 92.35% of the combined net profit; under $400 of them there
/// is no tax at all. The 12.4% Social Security part stops at the wage base, which
/// W-2 Social Security wages use up first. The 2.9% Medicare part has no limit.
/// Half the tax is deductible (Schedule 1 line 15).
pub fn schedule_se(
    params: &YearParams,
    schedule_c_cents: i64,
    k1_cents: i64,
    social_security_wages_cents: i64,
) -> ScheduleSe {
    let combined = schedule_c_cents + k1_cents;
    let net_earnings = if combined > 0 {
        apply_bp(combined, fp::SE_NET_EARNINGS_BP)
    } else {
        combined
    };
    if net_earnings < fp::SE_MINIMUM_EARNINGS {
        return ScheduleSe {
            combined_cents: combined,
            net_earnings_cents: net_earnings.max(0),
            ..Default::default()
        };
    }
    let room = (params.social_security_wage_base - social_security_wages_cents.max(0)).max(0);
    let social_security = apply_bp(net_earnings.min(room), fp::SE_SOCIAL_SECURITY_BP);
    let medicare = apply_bp(net_earnings, fp::SE_MEDICARE_BP);
    let tax = social_security + medicare;
    ScheduleSe {
        combined_cents: combined,
        net_earnings_cents: net_earnings,
        social_security_cents: social_security,
        medicare_cents: medicare,
        tax_cents: tax,
        deduction_cents: tax / 2,
    }
}

/// Form 8995: 20% of qualified business income and of Section 199A dividends,
/// limited to 20% of taxable income (before the deduction) less net capital gain.
///
/// Only below the threshold. Above it the deduction turns on each business's W-2
/// wages and property (Form 8995-A) and on whether it is a specified service
/// business, which the books cannot say — so it is not computed, and the warning
/// says what the figures would have been.
pub fn qbi_deduction(
    params: &YearParams,
    status: FilingStatus,
    business_income_cents: i64,
    reit_dividends_cents: i64,
    taxable_income_before_cents: i64,
    net_capital_gain_cents: i64,
    warnings: &mut Vec<String>,
) -> Qbi {
    let mut qbi = Qbi {
        business_income_cents,
        reit_dividends_cents,
        taxable_income_before_cents,
        ..Default::default()
    };
    if business_income_cents <= 0 && reit_dividends_cents <= 0 {
        if business_income_cents < 0 {
            warnings.push(format!(
                "Qualified business income is a loss of {}. It carries to next year's QBI \
                 deduction (Form 8995 line 16), which is not tracked here.",
                dollars(-business_income_cents)
            ));
        }
        return qbi;
    }
    let threshold = params.qbi_threshold.of(status);
    if taxable_income_before_cents > threshold {
        qbi.above_threshold = true;
        warnings.push(format!(
            "Taxable income before the QBI deduction is {}, over the {} threshold, so the \
             deduction needs Form 8995-A — each business's W-2 wages and property, and whether \
             it is a specified service business. It is not computed, and line 13 is zero.",
            dollars(taxable_income_before_cents),
            dollars(threshold)
        ));
        return qbi;
    }
    // A QBI loss offsets REIT dividends' component only through the carryforward,
    // so each component is floored at zero.
    let components = apply_bp(business_income_cents.max(0), fp::QBI_RATE_BP)
        + apply_bp(reit_dividends_cents.max(0), fp::QBI_RATE_BP);
    let limit = apply_bp(
        (taxable_income_before_cents - net_capital_gain_cents).max(0),
        fp::QBI_RATE_BP,
    );
    qbi.deduction_cents = components.min(limit);
    qbi
}

/// A 1099-NEC is income of the business it was paid to, on that business's
/// Schedule C — which is how it reaches this return. Say when no Schedule C on
/// the return has room for it.
fn nonemployee_compensation_warning(statements: &[TaxStatement]) -> Option<String> {
    let nec = boxes(statements, &[FormKind::F1099Nec], "1");
    if nec <= 0 {
        return None;
    }
    let schedule_cs = statements.iter().filter(|s| s.form == FormKind::ScheduleC).count();
    if schedule_cs == 0 {
        return Some(format!(
            "1099-NECs report {} of nonemployee compensation, and there is no Schedule C on \
             this return. It belongs on the Schedule C of the business it was paid to: link that \
             business and pull its Schedule C. It is not included here.",
            dollars(nec)
        ));
    }
    let receipts = boxes(statements, &[FormKind::ScheduleC], "1");
    (receipts < nec).then(|| {
        format!(
            "1099-NECs report {} of nonemployee compensation, more than the {} of gross receipts \
             on the Schedule Cs here. Each 1099-NEC belongs in the gross receipts of the business \
             it was paid to — check that every business that received one is linked and pulled.",
            dollars(nec),
            dollars(receipts)
        )
    })
}

/// Linked K-1s and Schedule Cs with nothing pulled for the year: a return
/// computed without them is missing their income, and looks complete.
fn missing_pulls(conn: &Connection, year: i32, statements: &[TaxStatement]) -> Vec<String> {
    let recorded = |id: &str| statements.iter().any(|s| s.statement_id == id);
    let mut out = Vec::new();
    for link in tax_statement_commands::list_schedule_c_links(conn) {
        if !recorded(&tax_statement_commands::schedule_c_statement_id(&link, year)) {
            out.push(format!(
                "{} is linked, but its {year} Schedule C has not been pulled, so none of its \
                 income is on this return.",
                link.ledger_name
            ));
        }
    }
    for link in tax_statement_commands::list_k1_links(conn) {
        if !recorded(&tax_statement_commands::k1_statement_id(&link, year)) {
            out.push(format!(
                "{} is linked, but {}'s {year} K-1 has not been pulled, so none of it is on this \
                 return.",
                link.ledger_name, link.partner_name
            ));
        }
    }
    out
}

/// Build a year's federal return from the books.
pub fn build(store: &EventStore, year: i32) -> Result<Form1040, Form1040Error> {
    let conn = store.connection();
    let params = fp::for_year(year).ok_or_else(|| {
        Form1040Error::UnsupportedYear(
            year,
            fp::supported_years()
                .iter()
                .map(|y| y.to_string())
                .collect::<Vec<_>>()
                .join(" and "),
        )
    })?;
    let profile =
        personal_tax_commands::get_profile(conn, year).ok_or(Form1040Error::NoProfile(year))?;
    let status = profile.filing_status;
    let accounts = income_accounts(conn, &profile);
    let investment = personal_return::build(store, year, &accounts);
    let statements = tax_statement_commands::list(conn, Some(year));
    let k1 = k1_totals(&statements);
    let mut warnings: Vec<String> = Vec::new();
    if !params.verified {
        warnings.push(format!(
            "The {year} brackets, standard deduction and thresholds were entered by hand from \
             {} and have not been checked against the published tables. Check them before \
             relying on the tax.",
            params.source
        ));
    }
    warnings.extend(investment.warnings.iter().cloned());

    use FormKind::*;
    let wages = boxes(&statements, &[W2], "1");
    let us_obligation_interest = boxes(&statements, &[F1099Int], "3");
    let taxable_interest =
        investment.schedule_b.interest_total_cents + k1.interest;
    let tax_exempt_interest =
        investment.schedule_b.tax_exempt_interest_cents + k1.tax_exempt_interest;
    let ordinary_dividends =
        investment.schedule_b.ordinary_dividends_total_cents + k1.ordinary_dividends;
    let qualified_dividends =
        (investment.schedule_b.qualified_dividends_total_cents + k1.qualified_dividends)
            .min(ordinary_dividends.max(0));
    let retirement_gross = investment.retirement.gross_cents;
    let retirement_taxable = investment.retirement.taxable_cents;

    // Schedule D, with last year's carryovers.
    let sd = &investment.schedule_d;
    let short = sd.short_term_cents - profile.short_term_loss_carryover_cents;
    let long = sd.long_term_cents - profile.long_term_loss_carryover_cents;
    let net_gain = short + long;
    let loss_limit = fp::CAPITAL_LOSS_LIMIT.of(status);
    let capital_gain = if net_gain >= 0 {
        net_gain
    } else {
        net_gain.max(-loss_limit)
    };
    // What the limit did not let through, short-term first (the Capital Loss
    // Carryover Worksheet's order).
    let loss_carryforward_cents = if net_gain < -loss_limit {
        let unused = -net_gain - loss_limit;
        let short_unused = (-short).max(0).min(unused);
        (short_unused, unused - short_unused)
    } else {
        (0, 0)
    };
    // Qualified dividends and long-term gain get the preferential rates; the net
    // capital gain is the smaller of the long-term gain and the net gain.
    let net_capital_gain = if long > 0 && net_gain > 0 {
        long.min(net_gain)
    } else {
        0
    };

    // Schedule 1, Part I, before Schedule E's loss limit.
    let unemployment = boxes(&statements, &[F1099G], "1");
    let other_income = boxes(&statements, &[F1099Misc], "3");
    let properties = rental_properties(conn, &profile, year);
    let rental_net: i64 = properties.iter().map(|p| p.net_cents).sum();

    let social_security = boxes(&statements, &[Ssa1099], "5");
    let early_withdrawal = boxes(&statements, &[F1099Int], "2");

    // Schedule 1 line 3: each business's line 31.
    let schedule_c: Vec<BusinessLine> = statements
        .iter()
        .filter(|s| s.form == ScheduleC)
        .map(|s| BusinessLine {
            issuer: s.issuer.clone(),
            net_profit_cents: s.amount("31"),
        })
        .collect();
    let business_income: i64 = schedule_c.iter().map(|b| b.net_profit_cents).sum();
    let schedule_se = schedule_se(
        params,
        boxes(&statements, &[ScheduleC], "se"),
        k1.self_employment,
        boxes(&statements, &[W2], "3"),
    );
    if schedule_se.tax_cents > 0 && status.has_spouse() {
        warnings.push(
            "Schedule SE is figured as one person's. If a spouse owns any of these businesses \
             or partnership interests, each spouse files a Schedule SE of their own, with their \
             own wage base."
                .to_string(),
        );
    }

    // Everything in AGI except Social Security and the rental figure, which both
    // depend on it.
    let base_income = wages
        + taxable_interest
        + ordinary_dividends
        + retirement_taxable
        + capital_gain
        + unemployment
        + other_income
        + business_income
        + k1.schedule_e.iter().map(|l| l.cents).sum::<i64>()
        - schedule_se.deduction_cents;

    // The $25,000 allowance for actively managed rentals (Form 8582, Part II),
    // phased out between $100,000 and $150,000 of modified AGI. Nothing at all for
    // a married person filing separately — the usual case of spouses who lived
    // together.
    let (rental_allowed, rental_suspended) = if rental_net >= 0 {
        (rental_net, 0)
    } else {
        let magi = base_income + rental_net.max(0) - early_withdrawal;
        let allowance = if status == FilingStatus::MarriedFilingSeparately {
            0
        } else {
            (2_500_000 - (magi - 10_000_000).max(0) / 2).max(0)
        };
        let allowed = rental_net.max(-allowance);
        if allowed != rental_net {
            warnings.push(format!(
                "Schedule E shows a rental loss of {}; at this income only {} of it is \
                 allowed this year, and {} is suspended under the passive activity rules \
                 (Form 8582) — figured here from the $25,000 allowance alone, not the full \
                 form. Passive income from a K-1 could free more of it.",
                dollars(-rental_net),
                dollars(-allowed),
                dollars(allowed - rental_net),
            ));
        }
        (allowed, allowed - rental_net)
    };
    let k1_cents: i64 = k1.schedule_e.iter().map(|l| l.cents).sum();
    if k1.schedule_e.iter().any(|l| l.cents < 0) {
        warnings.push(
            "A K-1 reports a loss on Schedule E. It is deducted in full here; whether it is \
             passive, at risk and within your basis is not checked."
                .to_string(),
        );
    }
    let schedule_e = ScheduleE {
        properties,
        rental_net_cents: rental_net,
        rental_allowed_cents: rental_allowed,
        rental_suspended_cents: rental_suspended,
        k1_lines: k1.schedule_e.clone(),
        k1_cents,
        total_cents: rental_allowed + k1_cents,
    };
    let schedule_1_income =
        business_income + schedule_e.total_cents + unemployment + other_income;

    let before_ss = wages
        + taxable_interest
        + ordinary_dividends
        + retirement_taxable
        + capital_gain
        + schedule_1_income;
    let ss_taxable = taxable_social_security(
        status,
        social_security,
        before_ss - early_withdrawal - schedule_se.deduction_cents + tax_exempt_interest,
    );
    let total_income = before_ss + ss_taxable;
    let adjustments = early_withdrawal + schedule_se.deduction_cents;
    let agi = total_income - adjustments;

    // The standard deduction, with a box per person 65 or older and per person blind.
    let boxes_checked = [
        profile.taxpayer_65_or_older,
        profile.taxpayer_blind,
        status.has_spouse() && profile.spouse_65_or_older,
        status.has_spouse() && profile.spouse_blind,
    ]
    .iter()
    .filter(|b| **b)
    .count() as i64;
    let additional_each = match status {
        FilingStatus::MarriedFilingJointly
        | FilingStatus::MarriedFilingSeparately
        | FilingStatus::QualifyingSurvivingSpouse => params.additional_standard_married,
        _ => params.additional_standard_unmarried,
    };
    let standard = params.standard_deduction.of(status) + boxes_checked * additional_each;
    if status == FilingStatus::MarriedFilingSeparately {
        warnings.push(
            "Married filing separately: if your spouse itemizes, you must too, and the \
             standard deduction computed here is not allowed."
                .to_string(),
        );
    }

    // The senior deduction (Schedule 1-A): $6,000 per person 65 or older, reduced by
    // 6% of modified AGI above the threshold. Not for a married person filing
    // separately.
    let seniors = [
        profile.taxpayer_65_or_older,
        status.has_spouse() && profile.spouse_65_or_older,
    ]
    .iter()
    .filter(|b| **b)
    .count() as i64;
    let senior_deduction = if seniors == 0
        || params.senior_deduction == 0
        || status == FilingStatus::MarriedFilingSeparately
    {
        0
    } else {
        let excess = (agi - params.senior_deduction_phaseout.of(status)).max(0);
        seniors * (params.senior_deduction - apply_bp(excess, 600)).max(0)
    };

    // Itemizing is not computed; say when it might win.
    let mortgage_interest = boxes(&statements, &[F1098], "1");
    let property_tax =
        boxes(&statements, &[PropertyTaxBill], "paid") + boxes(&statements, &[F1098], "10");
    let state_withheld = boxes(&statements, &[W2], "17");
    let salt = property_tax + state_withheld + profile.state_estimated_payments_cents;
    if mortgage_interest + salt.min(4_000_000) > standard {
        warnings.push(format!(
            "Itemizing might beat the standard deduction of {}: the year's statements show {} \
             of mortgage interest and {} of state and local taxes (before the SALT cap). \
             Schedule A is not computed here.",
            dollars(standard),
            dollars(mortgage_interest),
            dollars(salt),
        ));
    }
    // Form 8995. The Schedule Cs' QBI is reduced by the deductible half of the SE
    // tax their own earnings produced; a K-1's box 20 code Z is already the
    // partnership's figure.
    let sc_se = boxes(&statements, &[ScheduleC], "se").max(0);
    let se_combined = schedule_se.combined_cents.max(0);
    let sc_share_of_deduction = if se_combined > 0 {
        ((schedule_se.deduction_cents as i128 * sc_se as i128) / se_combined as i128) as i64
    } else {
        0
    };
    let qbi = qbi_deduction(
        params,
        status,
        boxes(&statements, &[ScheduleC], "qbi") - sc_share_of_deduction + k1.qbi,
        boxes(&statements, &[F1099Div], "5"),
        (agi - standard - senior_deduction).max(0),
        net_capital_gain + qualified_dividends,
        &mut warnings,
    );
    if year >= 2026 && qbi.business_income_cents >= 100_000 && !qbi.above_threshold {
        warnings.push(
            "From 2026 the QBI deduction has a $400 minimum for at least $1,000 of qualified \
             business income from a business you materially participate in. It is not applied \
             here."
                .to_string(),
        );
    }

    let deduction = standard + senior_deduction + qbi.deduction_cents;
    let taxable_income = (agi - deduction).max(0);
    let income_tax = if qualified_dividends > 0 || net_capital_gain > 0 {
        qdcg_tax(
            params,
            status,
            taxable_income,
            qualified_dividends,
            net_capital_gain,
        )
    } else {
        tax_on(params, status, taxable_income)
    };

    // Child and other-dependent credits: $50 less per $1,000 (or part) of modified
    // AGI over the threshold, and only as far as there is tax to take them from.
    let credit_before = profile.qualifying_children as i64 * params.child_tax_credit
        + profile.other_dependents as i64 * params.other_dependent_credit;
    let over = (agi - params.child_credit_phaseout.of(status)).max(0);
    let reduction = ((over + 99_999) / 100_000) * 5_000;
    let child_credit_full = (credit_before - reduction).max(0);
    let child_credit = child_credit_full.min(income_tax);
    if child_credit < child_credit_full {
        warnings.push(format!(
            "{} of the child tax credit exceeds the tax and is not used here. Part of it may \
             be refundable (Schedule 8812); that is not computed.",
            dollars(child_credit_full - child_credit)
        ));
    }

    // Foreign tax paid on dividends, taken as a credit without Form 1116 up to $300
    // ($600 joint). Above that the form is required, and its limit is not computed.
    let foreign_tax = boxes(&statements, &[F1099Div], "7") + k1.foreign_tax;
    let foreign_limit = if status == FilingStatus::MarriedFilingJointly {
        60_000
    } else {
        30_000
    };
    if foreign_tax > foreign_limit {
        warnings.push(format!(
            "{} of foreign tax was paid, more than can be claimed without Form 1116. The \
             credit is taken in full here; Form 1116 may limit it.",
            dollars(foreign_tax)
        ));
    }
    let foreign_credit = foreign_tax.min((income_tax - child_credit).max(0));

    // Net investment income tax (Form 8960): 3.8% of the smaller of net investment
    // income and modified AGI over the threshold. Wages, retirement distributions and
    // Social Security are not investment income; a K-1's business income is counted
    // only if it is passive, which is not known here.
    let nii = (taxable_interest
        + ordinary_dividends
        + capital_gain
        + rental_allowed
        + k1.passive_rental)
        .max(0);
    let niit = apply_bp(
        nii.min((agi - fp::NIIT_THRESHOLD.of(status)).max(0)),
        fp::NIIT_RATE_BP,
    );
    if niit > 0 && k1.schedule_e.iter().any(|l| l.form != K1EstateOrTrust) {
        warnings.push(
            "The net investment income tax counts a K-1's rental income but not its business \
             income; if you do not materially participate in that business, add it."
                .to_string(),
        );
    }
    warnings.extend(nonemployee_compensation_warning(&statements));
    warnings.extend(missing_pulls(conn, year, &statements));
    let medicare_wages = boxes(&statements, &[W2], "5");
    if medicare_wages + schedule_se.net_earnings_cents
        > fp::ADDITIONAL_MEDICARE_THRESHOLD.of(status)
    {
        warnings.push(format!(
            "Medicare wages and self-employment earnings come to {}, over the Additional \
             Medicare Tax threshold of {}: the 0.9% tax (Form 8959) is not computed.",
            dollars(medicare_wages + schedule_se.net_earnings_cents),
            dollars(fp::ADDITIONAL_MEDICARE_THRESHOLD.of(status))
        ));
    }
    warnings.push(
        "The alternative minimum tax (Form 6251) is not computed. Large long-term gains, \
         incentive stock options or private-activity bond interest are the usual reasons it \
         applies."
            .to_string(),
    );
    if tax_exempt_interest > 0 {
        warnings.push(
            "Tax-exempt interest from private activity bonds is an AMT preference; the books \
             do not say which of it is."
                .to_string(),
        );
    }
    if boxes(&statements, &[F1099G], "2") != 0 {
        warnings.push(
            "A 1099-G reports a state refund. It is income only if last year's return \
             itemized and deducted the tax it refunds — not included here."
                .to_string(),
        );
    }

    let total_credits = child_credit + foreign_credit;
    let tax_after_credits = (income_tax - total_credits).max(0);
    let other_taxes = niit + schedule_se.tax_cents;
    let total_tax = tax_after_credits + other_taxes;

    let withheld = boxes(&statements, &[W2], "2")
        + boxes(
            &statements,
            &[F1099Int, F1099Div, F1099Misc, F1099Nec, F1099G, F1099K],
            "4",
        )
        + boxes(&statements, &[Ssa1099], "6")
        + investment.withheld_cents;
    let estimated = profile.federal_estimated_payments_cents;
    let payments = withheld + estimated;
    let balance = payments - total_tax;

    let line = |key: &'static str, label: &'static str, cents: i64| ReturnLine {
        key,
        label,
        cents,
        note: None,
    };
    let noted = |key: &'static str, label: &'static str, cents: i64, note: String| ReturnLine {
        key,
        label,
        cents,
        note: Some(note),
    };
    let mut lines = vec![
        line("1a", "Wages (W-2 box 1)", wages),
        line("2a", "Tax-exempt interest", tax_exempt_interest),
        noted(
            "2b",
            "Taxable interest",
            taxable_interest,
            format!(
                "Schedule B {} + K-1s {}",
                dollars(investment.schedule_b.interest_total_cents),
                dollars(k1.interest)
            ),
        ),
        line("3a", "Qualified dividends", qualified_dividends),
        noted(
            "3b",
            "Ordinary dividends",
            ordinary_dividends,
            format!(
                "Schedule B {} + K-1s {}",
                dollars(investment.schedule_b.ordinary_dividends_total_cents),
                dollars(k1.ordinary_dividends)
            ),
        ),
        line("4a", "IRA distributions and pensions (gross)", retirement_gross),
        line("4b", "IRA distributions and pensions (taxable)", retirement_taxable),
        line("6a", "Social Security benefits", social_security),
        line("6b", "Social Security benefits (taxable)", ss_taxable),
        noted(
            "7",
            "Capital gain or (loss)",
            capital_gain,
            if profile.short_term_loss_carryover_cents + profile.long_term_loss_carryover_cents
                != 0
            {
                format!(
                    "Schedule D {} less carryovers {}",
                    dollars(sd.net_cents),
                    dollars(
                        profile.short_term_loss_carryover_cents
                            + profile.long_term_loss_carryover_cents
                    )
                )
            } else {
                format!("Schedule D line 16, {}", dollars(sd.net_cents))
            },
        ),
        noted(
            "8",
            "Additional income (Schedule 1)",
            schedule_1_income,
            format!(
                "Schedule C {}, Schedule E {}, unemployment {}, other {}",
                dollars(business_income),
                dollars(schedule_e.total_cents),
                dollars(unemployment),
                dollars(other_income)
            ),
        ),
        line("9", "Total income", total_income),
        noted(
            "10",
            "Adjustments to income (Schedule 1)",
            adjustments,
            format!(
                "Deductible part of SE tax {}, early withdrawal penalty {}",
                dollars(schedule_se.deduction_cents),
                dollars(early_withdrawal)
            ),
        ),
        line("11", "Adjusted gross income", agi),
        noted(
            "12",
            "Standard deduction",
            standard,
            format!(
                "{} for {}{}",
                dollars(params.standard_deduction.of(status)),
                status.label(),
                if boxes_checked > 0 {
                    format!(" + {boxes_checked} × {}", dollars(additional_each))
                } else {
                    String::new()
                }
            ),
        ),
        noted(
            "13",
            "Qualified business income deduction",
            qbi.deduction_cents,
            if qbi.above_threshold {
                "Over the Form 8995 threshold — Form 8995-A is not computed".to_string()
            } else {
                format!(
                    "Form 8995: 20% of {} QBI and {} REIT dividends, limited to 20% of {}",
                    dollars(qbi.business_income_cents.max(0)),
                    dollars(qbi.reit_dividends_cents),
                    dollars(
                        (qbi.taxable_income_before_cents - net_capital_gain - qualified_dividends)
                            .max(0)
                    )
                )
            },
        ),
        line("13b", "Schedule 1-A deductions (senior)", senior_deduction),
        line("14", "Total deductions", deduction),
        line("15", "Taxable income", taxable_income),
        noted(
            "16",
            "Tax",
            income_tax,
            if qualified_dividends > 0 || net_capital_gain > 0 {
                "Qualified Dividends and Capital Gain Tax Worksheet".to_string()
            } else if taxable_income < 10_000_000 {
                "Tax Table".to_string()
            } else {
                "Tax Computation Worksheet".to_string()
            },
        ),
        line("19", "Child tax credit and credit for other dependents", child_credit),
        line("20", "Foreign tax credit (Schedule 3)", foreign_credit),
        line("22", "Tax after credits", tax_after_credits),
        noted(
            "23",
            "Other taxes (Schedule 2)",
            other_taxes,
            format!(
                "Self-employment tax {}; net investment income tax {} (3.8% of the smaller of {} \
                 and AGI over the threshold)",
                dollars(schedule_se.tax_cents),
                dollars(niit),
                dollars(nii)
            ),
        ),
        line("24", "Total tax", total_tax),
        line("25d", "Federal income tax withheld", withheld),
        line("26", "Estimated tax payments", estimated),
        line("33", "Total payments", payments),
    ];
    if balance >= 0 {
        lines.push(line("34", "Overpaid", balance));
    } else {
        lines.push(line("37", "Amount you owe", -balance));
    }

    Ok(Form1040 {
        tax_year: year,
        filing_status: status,
        params_source: params.source,
        params_verified: params.verified,
        lines,
        schedule_e,
        schedule_c,
        business_income_cents: business_income,
        schedule_se,
        qbi,
        investment,
        wages_cents: wages,
        taxable_interest_cents: taxable_interest,
        us_obligation_interest_cents: us_obligation_interest,
        tax_exempt_interest_cents: tax_exempt_interest,
        ordinary_dividends_cents: ordinary_dividends,
        qualified_dividends_cents: qualified_dividends,
        retirement_taxable_cents: retirement_taxable,
        social_security_taxable_cents: ss_taxable,
        capital_gain_cents: capital_gain,
        agi_cents: agi,
        deduction_cents: deduction,
        taxable_income_cents: taxable_income,
        income_tax_cents: income_tax,
        niit_cents: niit,
        total_tax_cents: total_tax,
        payments_cents: payments,
        balance_cents: balance,
        loss_carryforward_cents,
        state_withheld_cents: state_withheld,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tax::federal_params::for_year;
    use FilingStatus::*;

    fn d(dollars: i64) -> i64 {
        dollars * 100
    }

    /// W-2 Social Security wages use up the wage base first: with $170,000 of them
    /// in 2025 only $6,100 of self-employment earnings is left for the 12.4%, while
    /// Medicare's 2.9% takes all of it.
    #[test]
    fn wages_use_up_the_social_security_wage_base_before_self_employment() {
        let p = for_year(2025).unwrap();
        let se = schedule_se(p, d(50_000), 0, d(170_000));
        assert_eq!(se.net_earnings_cents, d(46_175));
        assert_eq!(se.social_security_cents, apply_bp(d(6_100), 1_240));
        assert_eq!(se.medicare_cents, apply_bp(d(46_175), 290));
        assert_eq!(se.deduction_cents, se.tax_cents / 2);
    }

    /// Under $400 of net earnings there is no self-employment tax at all, and a
    /// K-1's box 14a counts toward the same total as a Schedule C.
    #[test]
    fn under_four_hundred_dollars_of_net_earnings_owes_no_self_employment_tax() {
        let p = for_year(2025).unwrap();
        assert_eq!(schedule_se(p, d(400), 0, 0).tax_cents, 0, "$369.40 of net earnings");
        assert_eq!(schedule_se(p, -d(5_000), 0, 0).tax_cents, 0, "a loss");
        assert!(schedule_se(p, d(300), d(200), 0).tax_cents > 0, "together they pass $400");
    }

    /// Form 8995 is 20% of QBI, but never more than 20% of taxable income less net
    /// capital gain.
    #[test]
    fn the_qbi_deduction_is_limited_by_taxable_income_less_capital_gain() {
        let p = for_year(2025).unwrap();
        let mut w = Vec::new();
        let q = qbi_deduction(p, Single, d(50_000), 0, d(100_000), 0, &mut w);
        assert_eq!(q.deduction_cents, d(10_000), "20% of the QBI");
        let q = qbi_deduction(p, Single, d(50_000), 0, d(40_000), d(10_000), &mut w);
        assert_eq!(q.deduction_cents, d(6_000), "20% of $30,000 after the capital gain");
        assert!(w.is_empty(), "{w:?}");
    }

    /// Over the threshold the deduction needs Form 8995-A, which is not computed:
    /// nothing is deducted, and the warning says why.
    #[test]
    fn above_the_qbi_threshold_nothing_is_deducted_and_it_says_so() {
        let p = for_year(2025).unwrap();
        let mut w = Vec::new();
        let q = qbi_deduction(p, Single, d(300_000), 0, d(250_000), 0, &mut w);
        assert!(q.above_threshold);
        assert_eq!(q.deduction_cents, 0);
        assert!(w.iter().any(|w| w.contains("8995-A")), "{w:?}");
        crate::tax::warning_shape::assert_all(&w);
    }

    fn stmt(form: FormKind, amounts: &[(&str, i64)]) -> TaxStatement {
        TaxStatement {
            statement_id: format!("{}-x", form.as_str()),
            tax_year: 2025,
            form,
            issuer: "Issuer".to_string(),
            amounts: amounts.iter().map(|(k, v)| (k.to_string(), d(*v))).collect(),
            document_ids: Vec::new(),
            source: Default::default(),
            note: None,
        }
    }

    /// A 1099-NEC is income of the business it was paid to: with no Schedule C on
    /// the return it is missing, and with receipts below it something is.
    #[test]
    fn a_1099nec_with_no_schedule_c_to_hold_it_is_said() {
        let nec = stmt(FormKind::F1099Nec, &[("1", 12_000)]);
        let none = nonemployee_compensation_warning(std::slice::from_ref(&nec)).unwrap();
        assert!(none.contains("no Schedule C"), "{none}");

        let small = stmt(FormKind::ScheduleC, &[("1", 8_000), ("31", 5_000)]);
        let short = nonemployee_compensation_warning(&[nec.clone(), small]).unwrap();
        assert!(short.contains("more than"), "{short}");

        let enough = stmt(FormKind::ScheduleC, &[("1", 20_000), ("31", 9_000)]);
        assert_eq!(nonemployee_compensation_warning(&[nec, enough]), None);
        crate::tax::warning_shape::assert_all([none, short]);
    }

    /// Ordinary income at ordinary rates, the dividends at 15%: a joint return with
    /// $100,000 of ordinary taxable income and $20,000 of qualified dividends has
    /// used up the 0% band, so the dividends are all at 15%.
    #[test]
    fn qualified_dividends_above_the_zero_band_are_taxed_at_fifteen_percent() {
        let p = for_year(2025).unwrap();
        let tax = qdcg_tax(p, MarriedFilingJointly, d(120_000), d(20_000), 0);
        // Tax on $100,000 joint: 2,385 + 8,772 + 22% of 3,050 = 11,828; plus 3,000.
        assert_eq!(tax, d(11_828) + d(3_000));
    }

    /// Below the 0% band's top, preferential income is taxed at nothing.
    #[test]
    fn gains_inside_the_zero_band_are_untaxed() {
        let p = for_year(2025).unwrap();
        let tax = qdcg_tax(p, Single, d(40_000), 0, d(30_000));
        // Only the $10,000 of ordinary income is taxed: Tax Table row $10,000–10,050.
        assert_eq!(tax, tax_on(p, Single, d(10_000)));
    }

    /// The worksheet straddles the bands: part at 0%, the rest at 15%.
    #[test]
    fn gains_straddling_the_zero_band_split_across_it() {
        let p = for_year(2025).unwrap();
        // $40,000 ordinary + $20,000 long-term gain, single: the 0% band runs to
        // $48,350, so $8,350 of the gain is at 0% and $11,650 at 15%.
        let tax = qdcg_tax(p, Single, d(60_000), 0, d(20_000));
        assert_eq!(tax, tax_on(p, Single, d(40_000)) + apply_bp(d(11_650), 1_500));
    }

    /// High income: past the 15% band's top, the rest is at 20%.
    #[test]
    fn gains_above_the_fifteen_band_are_taxed_at_twenty_percent() {
        let p = for_year(2025).unwrap();
        let tax = qdcg_tax(p, MarriedFilingJointly, d(1_000_000), 0, d(900_000));
        // Ordinary $100,000; the 15% band ends at $600,050, so $500,050 at 15% and
        // $399,950 at 20%.
        let expected = tax_on(p, MarriedFilingJointly, d(100_000))
            + apply_bp(d(500_050), 1_500)
            + apply_bp(d(399_950), 2_000);
        assert_eq!(tax, expected);
    }

    mod assembled {
        use super::*;
        use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
        use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
        use crate::domain::documents::StatementSource;
        use crate::domain::AccountType;
        use crate::events::types::{JournalEntrySource, RentalPropertyData};
        use crate::store::migrations::SchemaStore;
        use chrono::NaiveDate;

        fn books() -> EventStore {
            let mut store = EventStore::in_memory().unwrap();
            SchemaStore::init_schema(&mut store).unwrap();
            store
                .connection()
                .execute(
                    "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                     VALUES ('personal', 'personal', 'personal', 'USD', 1)",
                    [],
                )
                .unwrap();
            store
        }

        fn account(
            store: &mut EventStore,
            ty: AccountType,
            number: &str,
            name: &str,
            parent: Option<&str>,
        ) -> String {
            AccountCommands::new(store, "u".to_string())
                .create_account(CreateAccountCommand {
                    account_type: ty,
                    account_number: number.to_string(),
                    name: name.to_string(),
                    parent_id: parent.map(str::to_string),
                    currency: Some("USD".to_string()),
                    description: None,
                })
                .unwrap();
            store
                .connection()
                .query_row(
                    "SELECT id FROM accounts WHERE account_number = ?1",
                    [number],
                    |r| r.get(0),
                )
                .unwrap()
        }

        fn post(store: &mut EventStore, debit: &str, credit: &str, dollars: i64) {
            EntryCommands::new(store, "u".to_string())
                .post_entry(PostEntryCommand {
                    date: NaiveDate::from_ymd_opt(2025, 6, 1).unwrap(),
                    memo: "test".to_string(),
                    lines: vec![
                        EntryLine::debit(debit, dollars * 100, "USD"),
                        EntryLine::credit(credit, dollars * 100, "USD"),
                    ],
                    reference: None,
                    source: Some(JournalEntrySource::Manual),
                })
                .unwrap();
        }

        fn statement(
            store: &mut EventStore,
            id: &str,
            form: FormKind,
            amounts: &[(&str, i64)],
        ) {
            tax_statement_commands::record(
                store,
                "u",
                &TaxStatement {
                    statement_id: id.to_string(),
                    tax_year: 2025,
                    form,
                    issuer: "Issuer".to_string(),
                    amounts: amounts
                        .iter()
                        .map(|(k, v)| (k.to_string(), v * 100))
                        .collect(),
                    document_ids: Vec::new(),
                    source: StatementSource::Entered,
                    note: None,
                },
            )
            .unwrap();
        }

        /// Wages and a rental property, filing jointly: the property's rents less its
        /// expenses (a parent account counting its child) reach Schedule E, the
        /// standard deduction comes off, the tax is the joint brackets', and the
        /// withholding and estimated payments make a refund.
        #[test]
        fn wages_and_a_rental_make_a_whole_return() {
            let mut store = books();
            let cash = account(&mut store, AccountType::Asset, "1000", "Cash", None);
            let rent = account(&mut store, AccountType::Revenue, "2002", "4541 N Lincoln", None);
            let repairs = account(&mut store, AccountType::Expense, "3001", "Repairs", None);
            let utilities = account(
                &mut store,
                AccountType::Expense,
                "3002",
                "Utilities",
                Some(&repairs),
            );
            post(&mut store, &cash, &rent, 30_000);
            post(&mut store, &repairs, &cash, 4_000);
            post(&mut store, &utilities, &cash, 1_000);
            statement(
                &mut store,
                "w2",
                FormKind::W2,
                &[("1", 150_000), ("2", 25_000), ("17", 7_000)],
            );

            let mut profile = crate::commands::personal_tax_commands::tests::profile(
                2025,
                MarriedFilingJointly,
            );
            profile.federal_estimated_payments_cents = d(5_000);
            profile.rental_properties.push(RentalPropertyData {
                name: "4541 N Lincoln".to_string(),
                income_account_ids: vec![rent],
                expense_account_ids: vec![repairs],
            });
            crate::commands::personal_tax_commands::set_profile(&mut store, "u", &profile)
                .unwrap();

            let r = build(&store, 2025).unwrap();
            assert_eq!(r.schedule_e.rental_net_cents, d(25_000), "{:?}", r.schedule_e);
            assert_eq!(r.agi_cents, d(175_000));
            assert_eq!(r.deduction_cents, d(31_500));
            assert_eq!(r.taxable_income_cents, d(143_500));
            // 2,385 + 8,772 + 22% of 46,550 = 21,398.
            assert_eq!(r.income_tax_cents, d(21_398));
            assert_eq!(r.niit_cents, 0, "under the joint threshold");
            assert_eq!(r.payments_cents, d(30_000));
            assert_eq!(r.balance_cents, d(8_602));
            assert_eq!(r.state_withheld_cents, d(7_000));
            assert_eq!(r.line("34").map(|l| l.cents), Some(d(8_602)));

            // Illinois from the same return: two exemptions, 4.95%, the W-2's state
            // withholding against it.
            let il = crate::tax::il1040::build_from(&store, &r).unwrap();
            assert_eq!(il.base_income_cents, d(175_000));
            assert_eq!(il.exemption_cents, d(5_700));
            assert_eq!(il.net_income_cents, d(169_300));
            assert_eq!(il.tax_cents, 838_035);
            assert_eq!(il.payments_cents, d(7_000));
            assert_eq!(il.balance_cents, -138_035);
            assert_eq!(il.line("38").map(|l| l.cents), Some(138_035));
        }

        /// Above the cutoff the exemption is lost, and retirement income is taken
        /// back out because Illinois does not tax it.
        #[test]
        fn illinois_drops_the_exemption_above_the_cutoff_and_exempts_retirement() {
            let mut store = books();
            statement(&mut store, "w2", FormKind::W2, &[("1", 600_000)]);
            statement(&mut store, "ssa", FormKind::Ssa1099, &[("5", 40_000)]);
            let profile = crate::commands::personal_tax_commands::tests::profile(
                2025,
                MarriedFilingJointly,
            );
            crate::commands::personal_tax_commands::set_profile(&mut store, "u", &profile)
                .unwrap();
            let r = build(&store, 2025).unwrap();
            // 85% of the benefits are federally taxable at this income.
            assert_eq!(r.social_security_taxable_cents, d(34_000));
            let il = crate::tax::il1040::build_from(&store, &r).unwrap();
            assert_eq!(il.exemption_cents, 0);
            assert_eq!(il.base_income_cents, d(600_000));
        }

        #[test]
        fn a_year_without_a_profile_is_refused() {
            let store = books();
            assert_eq!(build(&store, 2025), Err(Form1040Error::NoProfile(2025)));
            assert!(matches!(
                build(&store, 2023),
                Err(Form1040Error::UnsupportedYear(2023, _))
            ));
        }
    }

    #[test]
    fn social_security_is_untaxed_below_the_base_and_capped_at_eighty_five_percent() {
        // Provisional income of $20,000 + half of $20,000 = $30,000, under the joint
        // base of $32,000.
        assert_eq!(
            taxable_social_security(MarriedFilingJointly, d(20_000), d(20_000)),
            0
        );
        // Far above the second base: 85% of the benefits.
        assert_eq!(
            taxable_social_security(MarriedFilingJointly, d(30_000), d(500_000)),
            d(25_500)
        );
        // Between the bases: half the excess over the first.
        // Single, $10,000 benefits, $24,000 other → provisional $29,000; half of $4,000.
        assert_eq!(taxable_social_security(Single, d(10_000), d(24_000)), d(2_000));
    }
}
