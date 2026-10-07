//! Illinois Form IL-1040, computed from the federal return.
//!
//! Illinois starts where the federal return ends — federal adjusted gross income —
//! adds back what the federal return leaves out that Illinois taxes, subtracts what
//! Illinois exempts, takes an exemption allowance per person, and taxes the rest at
//! a flat rate. So this reads [`Form1040`] and the year's statements, and nothing
//! else; a figure that is right on the federal return is right here.
//!
//! # What it does not do, and says so
//!
//! Schedule M's other additions and subtractions, Schedule ICR's credits beyond the
//! property tax credit, the K-1-P pass-through items, Schedule CR (tax paid to other
//! states), and use tax. Warnings name each that the year's figures make relevant.

use crate::commands::{personal_tax_commands, tax_statement_commands};
use crate::events::types::FilingStatus;
use crate::store::event_store::EventStore;
use crate::tax::federal_params::apply_bp;
use crate::tax::form1040::{self, Form1040, Form1040Error, ReturnLine};
use crate::tax::information_returns::FormKind;

/// One year's Illinois figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IlParams {
    pub year: i32,
    pub verified: bool,
    pub source: &'static str,
    /// Per exemption: the taxpayer, a spouse on a joint return, each dependent.
    pub exemption_cents: i64,
    /// Per box checked: 65 or older, legally blind.
    pub additional_exemption_cents: i64,
    /// Federal AGI above which the exemption allowance is lost entirely.
    pub exemption_cutoff_cents: i64,
    pub exemption_cutoff_joint_cents: i64,
    pub rate_bp: i64,
    /// The property tax credit, as a share of property tax paid on a principal
    /// residence.
    pub property_tax_credit_bp: i64,
}

const IL2025: IlParams = IlParams {
    year: 2025,
    verified: false,
    source: "IL-1040 instructions (exemption $2,850, rate 4.95%)",
    exemption_cents: 285_000,
    additional_exemption_cents: 100_000,
    exemption_cutoff_cents: 25_000_000,
    exemption_cutoff_joint_cents: 50_000_000,
    rate_bp: 495,
    property_tax_credit_bp: 500,
};

/// 2026's exemption amount had not been published when this was written; it
/// carries 2025's, and the return says so.
const IL2026: IlParams = IlParams {
    year: 2026,
    verified: false,
    source: "2025's figures carried forward — the 2026 exemption amount is not yet entered",
    ..IL2025
};

pub fn params_for(year: i32) -> Option<&'static IlParams> {
    match year {
        2025 => Some(&IL2025),
        2026 => Some(&IL2026),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Il1040 {
    pub tax_year: i32,
    pub lines: Vec<ReturnLine>,
    pub base_income_cents: i64,
    pub exemption_cents: i64,
    pub net_income_cents: i64,
    pub tax_cents: i64,
    pub credits_cents: i64,
    pub total_tax_cents: i64,
    pub payments_cents: i64,
    /// Positive is a refund, negative is owed.
    pub balance_cents: i64,
    pub warnings: Vec<String>,
}

impl Il1040 {
    pub fn line(&self, key: &str) -> Option<&ReturnLine> {
        self.lines.iter().find(|l| l.key == key)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Il1040Error {
    Federal(Form1040Error),
    NotIllinois(i32, Option<String>),
    UnsupportedYear(i32),
}

impl std::fmt::Display for Il1040Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Il1040Error::Federal(e) => write!(f, "{e}"),
            Il1040Error::NotIllinois(year, state) => write!(
                f,
                "the {year} profile's state is {}, not IL",
                state.as_deref().unwrap_or("none")
            ),
            Il1040Error::UnsupportedYear(year) => {
                write!(f, "{year} is not a year this version has Illinois figures for")
            }
        }
    }
}

impl std::error::Error for Il1040Error {}

/// Build a year's IL-1040, computing the federal return it starts from.
pub fn build(store: &EventStore, year: i32) -> Result<Il1040, Il1040Error> {
    let federal = form1040::build(store, year).map_err(Il1040Error::Federal)?;
    build_from(store, &federal)
}

/// Build a year's IL-1040 from a federal return already computed.
pub fn build_from(store: &EventStore, federal: &Form1040) -> Result<Il1040, Il1040Error> {
    let year = federal.tax_year;
    let conn = store.connection();
    let params = params_for(year).ok_or(Il1040Error::UnsupportedYear(year))?;
    let profile = personal_tax_commands::get_profile(conn, year)
        .ok_or(Il1040Error::Federal(Form1040Error::NoProfile(year)))?;
    if profile.state.as_deref() != Some("IL") {
        return Err(Il1040Error::NotIllinois(year, profile.state.clone()));
    }
    let status = federal.filing_status;
    let statements = tax_statement_commands::list(conn, Some(year));
    let mut warnings = Vec::new();
    if !params.verified {
        warnings.push(format!(
            "The {year} Illinois figures ({}) have not been checked against the published \
             instructions.",
            params.source
        ));
    }

    let agi = federal.agi_cents;
    // Line 2: interest and dividends the federal return exempts and Illinois taxes.
    let tax_exempt = federal.tax_exempt_interest_cents;
    if tax_exempt > 0 {
        warnings.push(
            "All federally tax-exempt interest is added back on line 2. Interest on certain \
             Illinois bonds is exempt in Illinois too; subtract it on Schedule M if any of \
             yours is."
                .to_string(),
        );
    }
    let total_income = agi + tax_exempt;

    // Line 5: Illinois does not tax retirement income or Social Security that the
    // federal return included.
    let retirement = federal.retirement_taxable_cents + federal.social_security_taxable_cents;
    // Schedule M: interest on U.S. obligations, which no state may tax.
    let us_obligations = federal.us_obligation_interest_cents;
    let subtractions = retirement + us_obligations;
    let base_income = total_income - subtractions;

    // Line 10: the exemption allowance, lost entirely above the cutoff.
    let people = 1
        + i64::from(status == FilingStatus::MarriedFilingJointly)
        + profile.qualifying_children as i64
        + profile.other_dependents as i64;
    let extra_boxes = [
        profile.taxpayer_65_or_older,
        profile.taxpayer_blind,
        status.has_spouse() && profile.spouse_65_or_older,
        status.has_spouse() && profile.spouse_blind,
    ]
    .iter()
    .filter(|b| **b)
    .count() as i64;
    let cutoff = if status == FilingStatus::MarriedFilingJointly {
        params.exemption_cutoff_joint_cents
    } else {
        params.exemption_cutoff_cents
    };
    let exemption = if agi > cutoff {
        0
    } else {
        people * params.exemption_cents + extra_boxes * params.additional_exemption_cents
    };

    let net_income = (base_income - exemption).max(0);
    let tax = apply_bp(net_income, params.rate_bp);

    // Schedule ICR: 5% of property tax paid on the principal residence, not allowed
    // above the same cutoff.
    let property_tax: i64 = statements
        .iter()
        .filter(|s| s.form == FormKind::PropertyTaxBill)
        .map(|s| s.amount("paid"))
        .sum();
    let property_credit = if agi > cutoff || property_tax == 0 {
        0
    } else {
        warnings.push(
            "The property tax credit counts every property tax bill recorded for the year as \
             your principal residence's. A rental's property tax belongs on Schedule E, not \
             here."
                .to_string(),
        );
        apply_bp(property_tax, params.property_tax_credit_bp).min(tax)
    };
    if property_tax > 0 && agi > cutoff {
        warnings.push(
            "No property tax credit: it is not allowed above the federal AGI cutoff."
                .to_string(),
        );
    }
    let credits = property_credit;
    let total_tax = (tax - credits).max(0);

    let withheld = federal.state_withheld_cents;
    let estimated = profile.state_estimated_payments_cents;
    let payments = withheld + estimated;
    let balance = payments - total_tax;

    warnings.push(
        "Not computed: other Schedule M items, IL-1040 K-1-P pass-through items, credit for \
         tax paid to other states (Schedule CR), and use tax on out-of-state purchases."
            .to_string(),
    );

    let line = |key: &'static str, label: &'static str, cents: i64| ReturnLine {
        key,
        label,
        cents,
        note: None,
    };
    let mut lines = vec![
        line("1", "Federal adjusted gross income", agi),
        line("2", "Federally tax-exempt interest and dividend income", tax_exempt),
        line("4", "Total income", total_income),
        line(
            "5",
            "Social Security benefits and retirement income included in federal AGI",
            retirement,
        ),
        line("7", "Other subtractions (Schedule M): U.S. obligation interest", us_obligations),
        line("8", "Total subtractions", subtractions),
        line("9", "Illinois base income", base_income),
        ReturnLine {
            key: "10",
            label: "Exemption allowance",
            cents: exemption,
            note: Some(if agi > cutoff {
                "None: federal AGI is over the cutoff".to_string()
            } else {
                format!(
                    "{people} × ${}{}",
                    params.exemption_cents / 100,
                    if extra_boxes > 0 {
                        format!(" + {extra_boxes} × ${}", params.additional_exemption_cents / 100)
                    } else {
                        String::new()
                    }
                )
            }),
        },
        line("11", "Net income", net_income),
        line("12", "Income tax (4.95%)", tax),
        line("16", "Credits (Schedule ICR: property tax)", credits),
        line("24", "Total tax", total_tax),
        line("25", "Illinois income tax withheld", withheld),
        line("26", "Estimated payments", estimated),
        line("31", "Total payments", payments),
    ];
    if balance >= 0 {
        lines.push(line("32", "Overpayment", balance));
    } else {
        lines.push(line("38", "Amount you owe", -balance));
    }

    Ok(Il1040 {
        tax_year: year,
        lines,
        base_income_cents: base_income,
        exemption_cents: exemption,
        net_income_cents: net_income,
        tax_cents: tax,
        credits_cents: credits,
        total_tax_cents: total_tax,
        payments_cents: payments,
        balance_cents: balance,
        warnings,
    })
}
