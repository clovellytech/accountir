//! A year's investment tax output, assembled: Schedule D and Form 8949, Schedule
//! B, the 1099-R figures, and the reconciliation of the books against the broker's
//! form.
//!
//! INVESTMENTS-SPEC.md §8, phase 6. [`super::personal`] is the *inputs* side — what
//! paperwork has arrived and where each box goes. This is the output side, and the
//! two are separate on purpose: one answers "what is missing", the other "what does
//! the return say".
//!
//! # What is here and what is not
//!
//! Form 1040's own lines are not here. They need a dated line map, as Form 1065's
//! do ([`super::lines`]), and that is the next phase's work; what this module
//! produces is exactly what such a map reads. Nothing here fills a PDF either —
//! see [`super::schedule_d`] on why.
//!
//! # Sheltered accounts contribute nothing, and this module adds no fence
//!
//! Growth inside a 401(k) or an IRA is not income to anybody, and phase 2 already
//! keeps its value-change account off every tax line in three places: an explicit
//! `OFF_RETURN` mapping written when the account is registered,
//! [`crate::commands::tax_setup_commands::set_account_line`] refusing any other
//! line, and [`super::lines::load_effective_mapping`] forcing it after inheritance.
//! A fourth opinion here would be a fourth thing to keep in step, and the day they
//! disagreed the wrong one might win. So this module has no opinion at all —
//! [`tests::a_sheltered_accounts_growth_reaches_no_line_of_any_return`] asserts the
//! consequence instead.
//!
//! The one thing a sheltered account *does* put on a return is a distribution out
//! of it, and that is a 1099-R.

use crate::commands::{retirement_commands, tax_statement_commands};
use crate::domain::documents::TaxStatement;
use crate::store::event_store::EventStore;
use crate::tax::information_returns::FormKind;
use crate::tax::investment_reconciliation::{self, Reconciliation};
use crate::tax::personal_schedule_b::{self, IncomeAccounts, PersonalScheduleB};
use crate::tax::schedule_d::{self, Brokerage1099B, LedgerContributions, ScheduleD};

/// What came out of the sheltered accounts, and how much of it is income.
///
/// # Why the taxable amount is read from the log and not computed
///
/// Whether a distribution is taxable is not a fact the ledger holds. A traditional
/// account's distribution is ordinary income unless the owner has after-tax basis
/// in it; a qualified Roth distribution is not income at all; a 529's or an HSA's
/// turns on what the money was *spent on*. Phase 2 therefore records box 2a on the
/// distribution event itself — exactly as a sale records the lots it consumed — and
/// this reads it back. A rule applied at report time would restate a filed figure
/// the next time the rule changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetirementDistributions {
    /// Box 1, summed.
    pub gross_cents: i64,
    /// Box 2a, summed. Form 1040 line 4b or 5b.
    pub taxable_cents: i64,
    /// Box 4, summed. Form 1040 line 25b.
    pub withheld_cents: i64,
    /// One entry per distribution, so a figure can be traced to the day it came
    /// out.
    pub distributions: Vec<retirement_commands::Distribution>,
    pub findings: Vec<String>,
}

/// A year's investment tax output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvestmentReturn {
    pub tax_year: i32,
    /// The 1099-Bs the year's Schedule D was built from — the source of the filed
    /// capital-gains figures (spec §8).
    pub forms_1099b: Vec<Brokerage1099B>,
    pub schedule_d: ScheduleD,
    pub schedule_b: PersonalScheduleB,
    pub retirement: RetirementDistributions,
    /// The books against the forms. Findings, never adjustments.
    pub reconciliation: Reconciliation,
    /// Federal income tax withheld on the year's investment paperwork: the
    /// 1099-Bs' box 4 and the 1099-Rs' box 4. Form 1040 line 25b.
    pub withheld_cents: i64,
    /// Everything worth reading before filing, gathered from the pieces in the
    /// order they were built.
    pub warnings: Vec<String>,
}

/// Build a year's investment tax output from the books.
///
/// `accounts` names the four income accounts Schedule B reads — see
/// [`IncomeAccounts`], and why they are named rather than inferred.
pub fn build(store: &EventStore, year: i32, accounts: &IncomeAccounts) -> InvestmentReturn {
    let conn = store.connection();
    let forms_1099b = Brokerage1099B::for_year(conn, year);
    let statements = tax_statement_commands::list(conn, Some(year));
    let schedule_b = personal_schedule_b::build(conn, year, accounts);
    let schedule_d = schedule_d::from_parts(
        year,
        &forms_1099b,
        &statements,
        LedgerContributions {
            capital_gain_distributions_cents: schedule_b.capital_gain_distributions_cents,
        },
    );
    let reconciliation = investment_reconciliation::reconcile(conn, year, &forms_1099b);
    let retirement = retirement(store, year, &statements);

    let withheld_cents = schedule_d.withheld_cents + retirement.withheld_cents;
    let mut warnings = Vec::new();
    warnings.extend(schedule_d.warnings.iter().cloned());
    warnings.extend(schedule_b.findings.iter().cloned());
    warnings.extend(retirement.findings.iter().cloned());
    warnings.extend(reconciliation.findings.iter().map(|f| f.description.clone()));

    InvestmentReturn {
        tax_year: year,
        forms_1099b,
        schedule_d,
        schedule_b,
        retirement,
        reconciliation,
        withheld_cents,
        warnings,
    }
}

/// The year's distributions, and how they compare with the 1099-Rs received.
fn retirement(
    store: &EventStore,
    year: i32,
    statements: &[TaxStatement],
) -> RetirementDistributions {
    let distributions: Vec<_> = retirement_commands::list_distributions(store)
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.on.format("%Y").to_string() == year.to_string())
        .collect();
    let gross_cents = distributions.iter().map(|d| d.gross_cents).sum();
    let taxable_cents = distributions.iter().map(|d| d.taxable_cents).sum();
    let withheld_cents = distributions.iter().map(|d| d.withheld_cents).sum();

    let mut findings = Vec::new();
    let forms: Vec<&TaxStatement> = statements
        .iter()
        .filter(|s| s.form == FormKind::F1099R)
        .collect();
    if !forms.is_empty() {
        for (code, ours, what) in [
            ("1", gross_cents, "gross distributions"),
            ("2a", taxable_cents, "taxable amount"),
            ("4", withheld_cents, "federal income tax withheld"),
        ] {
            let reported: i64 = forms.iter().map(|s| s.amount(code)).sum();
            if reported != ours {
                findings.push(format!(
                    "The year's {}s report {} of {what} in box {code} and the books record {}. \
                     The books' figure is the one carried, because the taxable amount is recorded \
                     on each distribution as it happens; a difference means a distribution is \
                     missing, or the box was read wrong.",
                    FormKind::F1099R.label(),
                    dollars(reported),
                    dollars(ours),
                ));
            }
        }
    } else if gross_cents != 0 {
        findings.push(format!(
            "The books record {} of retirement distributions in {year} and no 1099-R has been \
             recorded. The payer sends one for every distribution; the return carries the books' \
             figure until the form arrives to check it against.",
            dollars(gross_cents),
        ));
    }

    RetirementDistributions {
        gross_cents,
        taxable_cents,
        withheld_cents,
        distributions,
        findings,
    }
}

fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let cents = cents.unsigned_abs();
    format!("{sign}${}.{:02}", cents / 100, cents % 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::investment_commands as ic;
    use crate::commands::retirement_commands as rc;
    use crate::domain::documents::{Acquired, StatementLine, StatementSource};
    use crate::domain::AccountType;
    use crate::events::types::{HoldingTerm, InvestmentIncomeKind, RetirementKind};
    use crate::store::migrations::SchemaStore;
    use crate::tax::investment_reconciliation::{Cause, Scope};
    use crate::tax::schedule_d::Category;
    use chrono::NaiveDate;

    const YEAR: i32 = 2025;

    fn day(month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(YEAR, month, day).unwrap()
    }

    fn books() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut store).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO company (id, company_id, name, base_currency, fiscal_year_start_month)
                 VALUES ('me', 'me', 'Me', 'USD', 1)",
                [],
            )
            .unwrap();
        store
    }

    /// An account, created and then looked up: the command mints the id.
    fn account(store: &mut EventStore, number: &str, name: &str, kind: AccountType) -> String {
        AccountCommands::new(store, "user".to_string())
            .create_account(CreateAccountCommand {
                account_type: kind,
                account_number: number.to_string(),
                name: name.to_string(),
                parent_id: None,
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

    /// The chart these tests post into, with the ids the commands minted.
    struct Chart {
        cash: String,
        securities: String,
        bank: String,
        dividends: String,
        interest: String,
        exempt: String,
        capgain: String,
        gain: String,
        accounts: IncomeAccounts,
    }

    /// A taxable brokerage with the accounts spec §2a's chart names, plus the two
    /// dedicated accounts tax-exempt interest and capital gain distributions need.
    fn brokerage(store: &mut EventStore) -> Chart {
        let cash = account(store, "1100", "Broad Street Brokerage", AccountType::Asset);
        let securities = account(store, "1110", "Securities", AccountType::Asset);
        let bank = account(store, "1000", "First Bank", AccountType::Asset);
        let dividends = account(store, "4300", "Dividends", AccountType::Revenue);
        let interest = account(store, "4200", "Interest", AccountType::Revenue);
        let exempt = account(store, "4210", "Tax-exempt interest", AccountType::Revenue);
        let capgain = account(
            store,
            "4310",
            "Capital gain distributions",
            AccountType::Revenue,
        );
        let gain = account(store, "4400", "Realized gain", AccountType::Revenue);
        Chart {
            cash,
            securities,
            bank,
            accounts: IncomeAccounts {
                taxable_interest: vec![interest.clone()],
                tax_exempt_interest: vec![exempt.clone()],
                ordinary_dividends: vec![dividends.clone()],
                capital_gain_distributions: vec![capgain.clone()],
            },
            dividends,
            interest,
            exempt,
            capgain,
            gain,
        }
    }

    fn security(store: &mut EventStore, ticker: &str, name: &str, kind: &str) -> String {
        ic::define_security(
            store,
            "user",
            &ic::NewSecurity {
                ticker: ticker.to_string(),
                name: name.to_string(),
                kind: kind.to_string(),
                cusip: None,
                currency: "USD".to_string(),
            },
        )
        .unwrap()
        .0
    }

    fn buy(
        store: &mut EventStore,
        chart: &Chart,
        security_id: &str,
        shares: i64,
        cost: i64,
        on: NaiveDate,
    ) {
        ic::buy_security(
            store,
            "user",
            &ic::BuySecurityCommand {
                security_id: security_id.to_string(),
                securities_account_id: chart.securities.clone(),
                cash_account_id: chart.cash.clone(),
                quantity: shares * ic::MICRO_SHARE,
                total_cost_cents: cost,
                trade_date: on,
                memo: None,
            },
        )
        .unwrap();
    }

    fn sell(
        store: &mut EventStore,
        chart: &Chart,
        security_id: &str,
        shares: i64,
        proceeds: i64,
        fee: i64,
        on: NaiveDate,
    ) -> ic::Sold {
        ic::sell_security(
            store,
            "user",
            &ic::SellSecurityCommand {
                security_id: security_id.to_string(),
                securities_account_id: chart.securities.clone(),
                cash_account_id: chart.cash.clone(),
                realized_gain_account_id: chart.gain.clone(),
                quantity: shares * ic::MICRO_SHARE,
                proceeds_cents: proceeds,
                fee_cents: fee,
                trade_date: on,
                selection: ic::LotSelection::Fifo,
                memo: None,
            },
        )
        .unwrap()
    }

    fn dividend(store: &mut EventStore, chart: &Chart, security_id: Option<&str>, cents: i64) {
        ic::record_income(
            store,
            "user",
            &ic::RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Dividend,
                security_id: security_id.map(str::to_string),
                cash_account_id: chart.cash.clone(),
                income_account_id: chart.dividends.clone(),
                amount_cents: cents,
                received_on: day(3, 15),
                memo: None,
            },
        )
        .unwrap();
    }

    fn statement(
        store: &mut EventStore,
        form: FormKind,
        issuer: &str,
        boxes: &[(&str, i64)],
    ) -> String {
        let id = tax_statement_commands::new_statement_id();
        tax_statement_commands::record(
            store,
            "user",
            &TaxStatement {
                statement_id: id.clone(),
                tax_year: YEAR,
                form,
                issuer: issuer.to_string(),
                amounts: boxes.iter().map(|(c, v)| (c.to_string(), *v)).collect(),
                document_ids: Vec::new(),
                source: StatementSource::Entered,
                note: None,
            },
        )
        .unwrap();
        id
    }

    // -----------------------------------------------------------------------
    // The common case, end to end
    // -----------------------------------------------------------------------

    /// One year, one broker, everything covered: Schedule D lines 1a and 8a, no
    /// Form 8949, the books agreeing with the form, and the qualified split taken
    /// from the 1099-DIV.
    #[test]
    fn a_covered_year_produces_schedule_d_totals_no_8949_and_a_matching_ledger() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let acme = security(&mut store, "ACME", "Acme Corp", "stock");
        let heri = security(&mut store, "HRTG", "Heritage Co", "stock");

        // Short term: bought and sold inside the year.
        buy(&mut store, &chart, &acme, 100, 1_000_000, day(2, 1));
        let short = sell(&mut store, &chart, &acme, 100, 1_250_000, 0, day(11, 1));
        assert_eq!(short.realized_gain_cents, 250_000);

        // Long term: bought two years before.
        ic::buy_security(
            &mut store,
            "user",
            &ic::BuySecurityCommand {
                security_id: heri.clone(),
                securities_account_id: chart.securities.clone(),
                cash_account_id: chart.cash.clone(),
                quantity: 200 * ic::MICRO_SHARE,
                total_cost_cents: 1_500_000,
                trade_date: NaiveDate::from_ymd_opt(2022, 5, 4).unwrap(),
                memo: None,
            },
        )
        .unwrap();
        let long = sell(&mut store, &chart, &heri, 200, 2_000_000, 0, day(9, 12));
        assert_eq!(long.realized_gain_cents, 500_000);

        dividend(&mut store, &chart, Some(&acme), 40_000);

        statement(
            &mut store,
            FormKind::F1099B,
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 1_250_000),
                ("a_basis", 1_000_000),
                ("a_gain", 250_000),
                ("d_proceeds", 2_000_000),
                ("d_basis", 1_500_000),
                ("d_gain", 500_000),
            ],
        );
        statement(
            &mut store,
            FormKind::F1099Div,
            "Broad Street Brokerage",
            &[("1a", 40_000), ("1b", 40_000)],
        );

        let r = build(&store, YEAR, &chart.accounts);

        // Schedule D: totals, no listing.
        assert!(r.schedule_d.parts.is_empty(), "no Form 8949 is filed");
        assert_eq!(r.schedule_d.lines.get("1a").unwrap().proceeds_cents, 1_250_000);
        assert_eq!(r.schedule_d.lines.get("1a").unwrap().basis_cents, 1_000_000);
        assert_eq!(r.schedule_d.lines.get("1a").unwrap().gain_cents, 250_000);
        assert_eq!(r.schedule_d.lines.get("8a").unwrap().gain_cents, 500_000);
        assert_eq!(r.schedule_d.short_term_cents, 250_000);
        assert_eq!(r.schedule_d.long_term_cents, 500_000);
        assert_eq!(r.schedule_d.net_cents, 750_000);

        // The ledger agrees with the form, which is the whole point of the check.
        assert!(
            r.reconciliation.findings.is_empty(),
            "{:?}",
            r.reconciliation.findings
        );
        assert_eq!(r.reconciliation.ledger.short.gain_cents, 250_000);
        assert_eq!(r.reconciliation.ledger.long.gain_cents, 500_000);
        assert_eq!(r.reconciliation.statement.short.gain_cents, 250_000);

        // Schedule B.
        assert_eq!(r.schedule_b.ordinary_dividends_total_cents, 40_000);
        assert_eq!(r.schedule_b.qualified_dividends_total_cents, 40_000);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    // -----------------------------------------------------------------------
    // Where the form and the books part company
    // -----------------------------------------------------------------------

    /// A sale the books never got. Proceeds differ, so a transaction is missing —
    /// and it is reported, not adjusted away.
    #[test]
    fn a_trade_missing_from_the_books_is_a_finding_and_changes_no_filed_figure() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let acme = security(&mut store, "ACME", "Acme Corp", "stock");
        buy(&mut store, &chart, &acme, 100, 1_000_000, day(2, 1));
        sell(&mut store, &chart, &acme, 100, 1_250_000, 0, day(11, 1));

        // The broker reports a second sale as well: 400,000 of proceeds against
        // 300,000 of basis.
        statement(
            &mut store,
            FormKind::F1099B,
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 1_650_000),
                ("a_basis", 1_300_000),
                ("a_gain", 350_000),
            ],
        );

        let r = build(&store, YEAR, &chart.accounts);
        // The filed figure is the form's.
        assert_eq!(r.schedule_d.lines.get("1a").unwrap().gain_cents, 350_000);
        assert_eq!(r.schedule_d.short_term_cents, 350_000);

        assert_eq!(r.reconciliation.findings.len(), 1);
        let f = &r.reconciliation.findings[0];
        assert_eq!(f.scope, Scope::Term(HoldingTerm::Short));
        assert_eq!(f.proceeds_difference_cents, 400_000);
        assert_eq!(f.basis_difference_cents, 300_000);
        assert_eq!(f.gain_difference_cents, 100_000);
        assert_eq!(f.causes[0], Cause::MissingTrade);
        assert!(r.warnings.iter().any(|w| w.contains("Proceeds differ by $4000.00")));
    }

    /// A fund, the same proceeds, a different basis: average cost against our
    /// FIFO. Two permitted methods, said so in as many words.
    #[test]
    fn a_funds_basis_difference_is_reported_as_a_method_difference() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let fund = security(&mut store, "VFIAX", "Vanguard 500 Index", "mutual fund");
        // Two lots at different prices, so FIFO and average cost part company.
        ic::buy_security(
            &mut store,
            "user",
            &ic::BuySecurityCommand {
                security_id: fund.clone(),
                securities_account_id: chart.securities.clone(),
                cash_account_id: chart.cash.clone(),
                quantity: 100 * ic::MICRO_SHARE,
                total_cost_cents: 1_000_000,
                trade_date: NaiveDate::from_ymd_opt(2021, 4, 1).unwrap(),
                memo: None,
            },
        )
        .unwrap();
        ic::buy_security(
            &mut store,
            "user",
            &ic::BuySecurityCommand {
                security_id: fund.clone(),
                securities_account_id: chart.securities.clone(),
                cash_account_id: chart.cash.clone(),
                quantity: 100 * ic::MICRO_SHARE,
                total_cost_cents: 1_400_000,
                trade_date: NaiveDate::from_ymd_opt(2022, 4, 1).unwrap(),
                memo: None,
            },
        )
        .unwrap();
        // FIFO sells the first lot: basis 1,000,000.
        let sold = sell(&mut store, &chart, &fund, 100, 1_600_000, 0, day(8, 8));
        assert_eq!(sold.basis_cents, 1_000_000);

        // The broker averaged: (1,000,000 + 1,400,000) / 2 = 1,200,000.
        statement(
            &mut store,
            FormKind::F1099B,
            "Old Mutual Trust",
            &[
                ("d_proceeds", 1_600_000),
                ("d_basis", 1_200_000),
                ("d_gain", 400_000),
            ],
        );

        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.schedule_d.lines.get("8a").unwrap().gain_cents, 400_000);

        assert_eq!(r.reconciliation.findings.len(), 1);
        let f = &r.reconciliation.findings[0];
        assert_eq!(f.proceeds_difference_cents, 0, "no trade is missing");
        assert_eq!(f.basis_difference_cents, 200_000);
        assert_eq!(f.gain_difference_cents, -200_000);
        assert_eq!(f.causes[0], Cause::AverageCost);
        assert!(!f.causes.contains(&Cause::MissingTrade));
        assert!(Cause::AverageCost.explain().contains("method difference rather than an error"));
    }

    // -----------------------------------------------------------------------
    // Where Form 8949 is required
    // -----------------------------------------------------------------------

    /// A wash sale on a covered category: the subtotal line cannot carry column
    /// (g), so the transactions are listed.
    #[test]
    fn a_wash_sale_adjustment_produces_form_8949_detail() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let id = statement(
            &mut store,
            FormKind::F1099B,
            "Broad Street Brokerage",
            &[
                ("a_proceeds", 800_000),
                ("a_basis", 900_000),
                ("a_adjustments", 30_000),
                ("a_gain", -70_000),
            ],
        );
        tax_statement_commands::record_lines(
            &mut store,
            "user",
            &id,
            &[
                StatementLine {
                    statement_id: id.clone(),
                    line_id: "1".to_string(),
                    category: Category::A,
                    description: "100 sh. ACME CORP".to_string(),
                    acquired: Acquired::On(day(1, 10)),
                    sold_on: day(2, 20),
                    proceeds_cents: 300_000,
                    basis_cents: 400_000,
                    adjustment_code: Some("W".to_string()),
                    adjustment_cents: 30_000,
                },
                StatementLine {
                    statement_id: id.clone(),
                    line_id: "2".to_string(),
                    category: Category::A,
                    description: "150 sh. ACME CORP".to_string(),
                    acquired: Acquired::On(day(3, 10)),
                    sold_on: day(6, 20),
                    proceeds_cents: 500_000,
                    basis_cents: 500_000,
                    adjustment_code: None,
                    adjustment_cents: 0,
                },
            ],
        )
        .unwrap();

        let r = build(&store, YEAR, &chart.accounts);
        assert!(!r.schedule_d.lines.contains_key("1a"));
        assert_eq!(r.schedule_d.lines.get("1b").unwrap().adjustment_cents, 30_000);
        assert_eq!(r.schedule_d.lines.get("1b").unwrap().gain_cents, -70_000);
        assert_eq!(r.schedule_d.parts.len(), 1);
        let part = &r.schedule_d.parts[0];
        assert_eq!(part.category, Category::A);
        assert_eq!(part.rows.len(), 2);
        assert_eq!(part.rows[0].adjustment_code.as_deref(), Some("W"));
        assert_eq!(part.rows[0].gain_cents, -70_000);
        assert_eq!(part.rows[1].gain_cents, 0);
        assert_eq!(part.totals.adjustment_cents, 30_000);
        assert_eq!(r.schedule_d.short_term_cents, -70_000);
    }

    /// Inherited shares: nobody reported the basis, so the sale is listed and
    /// column (b) says INHERITED rather than a date the ledger does not have.
    #[test]
    fn a_noncovered_category_requires_detail_and_takes_a_stated_acquisition() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let id = statement(
            &mut store,
            FormKind::F1099B,
            "Old Mutual Trust",
            &[
                ("e_proceeds", 1_200_000),
                ("e_basis", 900_000),
                ("e_gain", 300_000),
            ],
        );
        tax_statement_commands::record_lines(
            &mut store,
            "user",
            &id,
            &[StatementLine {
                statement_id: id.clone(),
                line_id: "1".to_string(),
                category: Category::E,
                description: "400 sh. HERITAGE CO".to_string(),
                acquired: Acquired::Stated("INHERITED".to_string()),
                sold_on: day(5, 1),
                proceeds_cents: 1_200_000,
                basis_cents: 900_000,
                adjustment_code: None,
                adjustment_cents: 0,
            }],
        )
        .unwrap();

        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.schedule_d.parts.len(), 1);
        assert_eq!(r.schedule_d.parts[0].category, Category::E);
        assert_eq!(r.schedule_d.parts[0].rows[0].acquired, "INHERITED");
        assert_eq!(r.schedule_d.lines.get("9").unwrap().gain_cents, 300_000);
        assert!(!r.schedule_d.lines.contains_key("8a"));
        assert_eq!(r.schedule_d.long_term_cents, 300_000);
    }

    // -----------------------------------------------------------------------
    // Schedule B's two exclusions, and the 1099-R
    // -----------------------------------------------------------------------

    /// Box 8 money is exempt. It reaches Form 1040 line 2a and no line of
    /// Schedule B.
    #[test]
    fn tax_exempt_interest_never_reaches_taxable_interest() {
        let mut store = books();
        let chart = brokerage(&mut store);
        ic::record_income(
            &mut store,
            "user",
            &ic::RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Interest,
                security_id: None,
                cash_account_id: chart.cash.clone(),
                income_account_id: chart.interest.clone(),
                amount_cents: 6_000,
                received_on: day(4, 1),
                memo: None,
            },
        )
        .unwrap();
        ic::record_income(
            &mut store,
            "user",
            &ic::RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Interest,
                security_id: None,
                cash_account_id: chart.cash.clone(),
                income_account_id: chart.exempt.clone(),
                amount_cents: 11_000,
                received_on: day(4, 1),
                memo: None,
            },
        )
        .unwrap();
        statement(
            &mut store,
            FormKind::F1099Int,
            "Broad Street Brokerage",
            &[("1", 6_000), ("8", 11_000)],
        );

        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.schedule_b.interest_total_cents, 6_000);
        assert_eq!(r.schedule_b.tax_exempt_interest_cents, 11_000);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    /// A capital gain distribution is Schedule D line 13 and nothing on
    /// Schedule B: it is not a dividend, and it is not a sale either.
    #[test]
    fn a_capital_gain_distribution_reaches_schedule_d_and_not_schedule_b() {
        let mut store = books();
        let chart = brokerage(&mut store);
        ic::record_income(
            &mut store,
            "user",
            &ic::RecordInvestmentIncomeCommand {
                kind: InvestmentIncomeKind::Dividend,
                security_id: None,
                cash_account_id: chart.cash.clone(),
                income_account_id: chart.capgain.clone(),
                amount_cents: 32_500,
                received_on: day(12, 20),
                memo: None,
            },
        )
        .unwrap();
        dividend(&mut store, &chart, None, 18_000);
        statement(
            &mut store,
            FormKind::F1099Div,
            "Broad Street Brokerage",
            &[("1a", 18_000), ("1b", 12_000), ("2a", 32_500)],
        );

        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.schedule_d.lines.get("13").unwrap().gain_cents, 32_500);
        assert_eq!(r.schedule_d.long_term_cents, 32_500);
        assert_eq!(r.schedule_d.net_cents, 32_500);
        assert_eq!(r.schedule_b.ordinary_dividends_total_cents, 18_000);
        assert_eq!(r.schedule_b.qualified_dividends_total_cents, 12_000);
        assert!(
            r.schedule_b.dividends.iter().all(|p| p.cents == 18_000),
            "no payer line carries the distribution"
        );
        assert!(r.schedule_d.parts.is_empty(), "a distribution is not a sale");
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    /// The 1099-R's box 2a, read off the distribution event that recorded it.
    #[test]
    fn a_1099r_taxable_amount_comes_from_the_distribution_event() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let (retirement_account, prepaid) = register_sheltered(&mut store);

        rc::record_contribution(
            &mut store,
            "user",
            &rc::RetirementContributionCommand {
                account_id: retirement_account.clone(),
                funding_account_id: chart.bank.clone(),
                amount_cents: 1_000_000,
                on: day(1, 15),
                memo: None,
                reference: None,
            },
        )
        .unwrap();
        rc::record_distribution(
            &mut store,
            "user",
            &rc::RetirementDistributionCommand {
                account_id: retirement_account.clone(),
                receiving_account_id: chart.bank.clone(),
                gross_cents: 500_000,
                withheld_cents: 50_000,
                withheld_account_id: prepaid.clone(),
                taxable_income_account_id: None,
                taxable_cents: Some(420_000),
                on: day(10, 2),
                memo: None,
                reference: None,
            },
        )
        .unwrap();

        statement(
            &mut store,
            FormKind::F1099R,
            "Retirement Custodian",
            &[("1", 500_000), ("2a", 420_000), ("4", 50_000)],
        );

        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.retirement.gross_cents, 500_000);
        assert_eq!(r.retirement.taxable_cents, 420_000);
        assert_eq!(r.retirement.withheld_cents, 50_000);
        assert_eq!(r.retirement.distributions.len(), 1);
        assert_eq!(r.withheld_cents, 50_000);
        assert!(r.retirement.findings.is_empty(), "{:?}", r.retirement.findings);
    }

    #[test]
    fn a_1099r_that_disagrees_with_the_books_is_reported() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let (retirement_account, prepaid) = register_sheltered(&mut store);
        rc::record_contribution(
            &mut store,
            "user",
            &rc::RetirementContributionCommand {
                account_id: retirement_account.clone(),
                funding_account_id: chart.bank.clone(),
                amount_cents: 1_000_000,
                on: day(1, 15),
                memo: None,
                reference: None,
            },
        )
        .unwrap();
        rc::record_distribution(
            &mut store,
            "user",
            &rc::RetirementDistributionCommand {
                account_id: retirement_account,
                receiving_account_id: chart.bank.clone(),
                gross_cents: 500_000,
                withheld_cents: 0,
                withheld_account_id: prepaid.clone(),
                taxable_income_account_id: None,
                taxable_cents: Some(500_000),
                on: day(10, 2),
                memo: None,
                reference: None,
            },
        )
        .unwrap();
        statement(
            &mut store,
            FormKind::F1099R,
            "Retirement Custodian",
            &[("1", 500_000), ("2a", 300_000)],
        );

        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.retirement.taxable_cents, 500_000, "the books are carried");
        assert_eq!(r.retirement.findings.len(), 1);
        assert!(r.retirement.findings[0].contains("$3000.00 of taxable amount"));
        assert!(r.retirement.findings[0].contains("the books record $5000.00"));
        crate::tax::warning_shape::assert_all(&r.retirement.findings);
    }

    /// A distribution the payer has not sent a form for yet. The books carry it —
    /// the taxable amount was recorded when it happened — and the finding says the
    /// form is still outstanding.
    #[test]
    fn a_distribution_with_no_1099r_recorded_yet_is_carried_and_reported() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let (retirement_account, prepaid) = register_sheltered(&mut store);
        rc::record_contribution(
            &mut store,
            "user",
            &rc::RetirementContributionCommand {
                account_id: retirement_account.clone(),
                funding_account_id: chart.bank.clone(),
                amount_cents: 1_000_000,
                on: day(1, 15),
                memo: None,
                reference: None,
            },
        )
        .unwrap();
        rc::record_distribution(
            &mut store,
            "user",
            &rc::RetirementDistributionCommand {
                account_id: retirement_account,
                receiving_account_id: chart.bank.clone(),
                gross_cents: 250_000,
                withheld_cents: 0,
                withheld_account_id: prepaid.clone(),
                taxable_income_account_id: None,
                taxable_cents: Some(250_000),
                on: day(11, 4),
                memo: None,
                reference: None,
            },
        )
        .unwrap();

        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.retirement.gross_cents, 250_000);
        assert_eq!(r.retirement.taxable_cents, 250_000);
        assert_eq!(r.retirement.findings.len(), 1);
        assert!(r.retirement.findings[0].contains("$2500.00 of retirement distributions"));
        assert!(r.retirement.findings[0].contains("no 1099-R has been recorded"));
        crate::tax::warning_shape::assert_all(&r.warnings);
    }

    // -----------------------------------------------------------------------
    // The non-taxable fence, asserted rather than re-implemented
    // -----------------------------------------------------------------------

    /// The most important assertion in phase 6. A sheltered account's growth is
    /// not income to anybody, and this checks it reaches **no line of any return**:
    /// not a Schedule D line, not a Schedule B line, not the 1099-R figures, and
    /// not a Form 1065 or Schedule C line either — the mapping every return reads
    /// says `off`.
    ///
    /// Deliberately an assertion and not a fourth fence. Phase 2 already holds it
    /// in three places; a fourth would be a fourth thing to keep in step.
    #[test]
    fn a_sheltered_accounts_growth_reaches_no_line_of_any_return() {
        let mut store = books();
        let chart = brokerage(&mut store);
        let (retirement_account, _prepaid) = register_sheltered(&mut store);

        rc::record_contribution(
            &mut store,
            "user",
            &rc::RetirementContributionCommand {
                account_id: retirement_account.clone(),
                funding_account_id: chart.bank.clone(),
                amount_cents: 1_000_000,
                on: day(1, 15),
                memo: None,
                reference: None,
            },
        )
        .unwrap();
        // The account grew by 250,000 over the year, all of it inside the shelter.
        rc::set_value(
            &mut store,
            "user",
            &rc::SetRetirementValueCommand {
                account_id: retirement_account.clone(),
                value_cents: 1_250_000,
                as_of: day(12, 31),
                memo: None,
            },
        )
        .unwrap();

        let value_change = rc::get_account(store.connection(), &retirement_account)
            .expect("the account is registered")
            .value_change_account_id;
        // It really did post: this test would pass vacuously otherwise.
        let posted: i64 = store
            .connection()
            .query_row(
                "SELECT COALESCE(SUM(-amount), 0) FROM journal_lines WHERE account_id = ?1",
                [&value_change],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(posted, 250_000, "the growth was recorded");

        // Phase 2's fence, asserted. `load_effective_mapping` is what every return
        // reads, and it answers `off` for this account.
        let mapping = crate::tax::lines::load_effective_mapping(store.connection(), YEAR);
        assert_eq!(
            mapping.get(&value_change).map(String::as_str),
            Some(crate::tax::lines::OFF_RETURN),
            "growth inside a shelter on a tax line is a filed error"
        );

        // And nothing of it reaches this phase's output.
        let r = build(&store, YEAR, &chart.accounts);
        assert_eq!(r.schedule_d.net_cents, 0);
        assert!(r.schedule_d.lines.is_empty());
        assert!(r.schedule_d.parts.is_empty());
        assert_eq!(r.schedule_b.interest_total_cents, 0);
        assert_eq!(r.schedule_b.ordinary_dividends_total_cents, 0);
        assert_eq!(r.schedule_b.qualified_dividends_total_cents, 0);
        assert_eq!(r.schedule_b.tax_exempt_interest_cents, 0);
        assert_eq!(r.schedule_b.capital_gain_distributions_cents, 0);
        assert_eq!(r.retirement.gross_cents, 0, "growth is not a distribution");
        assert_eq!(r.retirement.taxable_cents, 0);
        assert_eq!(r.withheld_cents, 0);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    }

    /// A sheltered account on the register, with the value-change account spec
    /// §2b's chart names and the prepaid-tax account a distribution needs.
    /// The sheltered account, its value-change account and the prepaid-tax
    /// account a distribution withholds into. Returns the account's id and the
    /// prepaid account's.
    fn register_sheltered(store: &mut EventStore) -> (String, String) {
        let ira = account(store, "1200", "Retirement Custodian", AccountType::Asset);
        let value_change = account(
            store,
            "4500",
            "Retirement value change",
            AccountType::Revenue,
        );
        let prepaid = account(store, "1300", "Prepaid federal tax", AccountType::Asset);
        rc::register_account(
            store,
            "user",
            &rc::RegisterRetirementAccountCommand {
                account_id: ira.clone(),
                institution: "Retirement Custodian".to_string(),
                kind: RetirementKind::Traditional,
                value_change_account_id: value_change,
            },
        )
        .unwrap();
        (ira, prepaid)
    }
}
