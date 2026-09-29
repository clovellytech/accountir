//! Schedule B for a person: interest and dividends, payer by payer.
//!
//! Note this is **not** [`super::schedule_b`], which holds Form 1065's "Other
//! Information" questions and is unrelated to this in every way but the name the
//! IRS gave them (INVESTMENTS-SPEC.md §8).
//!
//! # Where the figures come from, and why not from the 1099s
//!
//! [`super::schedule_d`] takes the 1099-B as authoritative, because basis and wash
//! sales are computations the broker made and the IRS already holds. Interest and
//! dividends are not like that. They are amounts of money that arrived, the books
//! recorded every one of them as it arrived, and the ledger is the better record —
//! it has the ones a payer never sent a form for, and it does not miss the ones
//! that arrived in December. So the summary is the ledger's, and the year's 1099s
//! are the cross-check.
//!
//! With exactly one exception, which is the next section.
//!
//! # The qualified split can only come from the form
//!
//! A qualified dividend is taxed at the capital-gains rate and an ordinary one is
//! not, and whether a payment qualifies turns on how long the *payer* held what it
//! paid out of and how long *you* held the share around the ex-dividend date. No
//! feed says which is which — Plaid certainly does not — so every dividend is
//! recorded as ordinary during the year and the split is taken from box 1b of the
//! 1099-DIV at year end (spec §7). That is not a shortcut; it is the only place the
//! fact exists.
//!
//! # Four accounts, not two, and why they are named rather than inferred
//!
//! Tax-exempt interest (1099-INT box 8) is not interest on Schedule B: it goes
//! straight to Form 1040 line 2a and nowhere else. A capital gain distribution
//! (1099-DIV box 2a) is not a dividend at all: it goes to Schedule D line 13. Both
//! arrive as cash from a fund or a bank and look exactly like the taxable kind in a
//! posting, so neither can be told apart by inspecting the books. They are given
//! **their own income accounts**, and this module reads those accounts. Inferring
//! either one is how the same money ends up on two schedules.
//!
//! # Who the payer is
//!
//! The entity that reports the payment to the IRS: the bank, or the broker as
//! nominee. Not the security that generated a dividend inside a brokerage account —
//! a consolidated 1099-DIV names the broker, and the broker's figure is what the
//! IRS matches the return against. So the payer is read from the account the money
//! arrived in, which is the one the entry names.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::commands::tax_statement_commands;
use crate::domain::documents::TaxStatement;
use crate::tax::information_returns::FormKind;

/// Which accounts hold which kind of investment income.
///
/// Named by the caller, never guessed: this module does not own the chart of
/// accounts, and deriving an account id from a word is how a figure lands on a
/// schedule nobody chose. Each list may name several accounts — one brokerage's
/// interest account and one bank's, say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IncomeAccounts {
    /// Schedule B, Part I.
    pub taxable_interest: Vec<String>,
    /// Form 1040 line 2a. Never Schedule B.
    pub tax_exempt_interest: Vec<String>,
    /// Schedule B, Part II.
    pub ordinary_dividends: Vec<String>,
    /// Schedule D line 13. Never Schedule B.
    pub capital_gain_distributions: Vec<String>,
}

/// One payer's line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayerAmount {
    /// The name the return lists: the bank, or the broker as nominee.
    pub payer: String,
    /// In cents.
    pub cents: i64,
    /// The part of it that is qualified, from the 1099-DIV. Always zero for
    /// interest, and zero for a dividend payer whose 1099-DIV has not arrived.
    pub qualified_cents: i64,
}

/// A year's Schedule B, and the two figures that deliberately are not on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonalScheduleB {
    pub tax_year: i32,
    /// Part I, line 1 — one entry per payer, in payer order.
    pub interest: Vec<PayerAmount>,
    /// Part I, line 4, and Form 1040 line 2b.
    pub interest_total_cents: i64,
    /// Part II, line 5 — one entry per payer.
    pub dividends: Vec<PayerAmount>,
    /// Part II, line 6, and Form 1040 line 3b.
    pub ordinary_dividends_total_cents: i64,
    /// Form 1040 line 3a. Not a Schedule B line: the schedule lists ordinary
    /// dividends and the qualified part is carried separately.
    pub qualified_dividends_total_cents: i64,
    /// Form 1040 line 2a. **Never** reaches [`interest_total_cents`].
    pub tax_exempt_interest_cents: i64,
    /// Schedule D line 13. **Never** reaches
    /// [`ordinary_dividends_total_cents`].
    pub capital_gain_distributions_cents: i64,
    /// Where the books and the year's 1099s disagree, and where a posting could
    /// not be attributed to a payer. Reported, never resolved.
    pub findings: Vec<String>,
}

/// Build a year's Schedule B from the books, with the qualified split taken from
/// the year's 1099-DIVs.
pub fn build(conn: &Connection, year: i32, accounts: &IncomeAccounts) -> PersonalScheduleB {
    let mut findings = Vec::new();
    let names = account_names(conn);

    let (interest_by_payer, unattributed_interest) =
        by_payer(conn, year, &accounts.taxable_interest, &names);
    let (dividends_by_payer, unattributed_dividends) =
        by_payer(conn, year, &accounts.ordinary_dividends, &names);
    let tax_exempt_interest_cents = total_for(conn, year, &accounts.tax_exempt_interest);
    let capital_gain_distributions_cents =
        total_for(conn, year, &accounts.capital_gain_distributions);

    for (kind, cents) in [
        ("interest", unattributed_interest),
        ("dividends", unattributed_dividends),
    ] {
        if cents != 0 {
            findings.push(format!(
                "{} of {kind} is posted in an entry with more than one account on the other side, \
                 so the payer the return has to list cannot be read off it. It is in the total and \
                 not against a payer; split the entry, or name the payer in the entry's memo.",
                dollars(cents),
            ));
        }
    }

    let statements = tax_statement_commands::list(conn, Some(year));

    // The qualified split, and only it, comes from the form.
    let mut qualified: BTreeMap<String, i64> = BTreeMap::new();
    let mut qualified_total = 0;
    for statement in statements.iter().filter(|s| s.form == FormKind::F1099Div) {
        let box1b = statement.amount("1b");
        if box1b == 0 {
            continue;
        }
        qualified_total += box1b;
        match match_payer(&statement.issuer, dividends_by_payer.keys()) {
            Some(payer) => *qualified.entry(payer).or_default() += box1b,
            None => findings.push(format!(
                "The {} from {} reports {} of qualified dividends, and no dividend payer in the \
                 books matches that name. The qualified total carries it, so Form 1040 line 3a is \
                 right, but Schedule B cannot say which payer it belongs to.",
                FormKind::F1099Div.label(),
                statement.issuer,
                dollars(box1b),
            )),
        }
    }

    let interest = lines(&interest_by_payer, &BTreeMap::new());
    let dividends = lines(&dividends_by_payer, &qualified);

    // The totals include what could not be attributed to a payer: the return's
    // total has to be the whole of the year's income, and a payment nobody could
    // name is still income. The finding above says it is not on a payer's line.
    let interest_total_cents =
        interest.iter().map(|p| p.cents).sum::<i64>() + unattributed_interest;
    let ordinary_dividends_total_cents =
        dividends.iter().map(|p| p.cents).sum::<i64>() + unattributed_dividends;

    for payer in &dividends {
        if payer.qualified_cents > payer.cents {
            findings.push(format!(
                "{} reports {} of qualified dividends against {} of ordinary dividends in the \
                 books. The qualified part cannot be larger than the whole; one of the two is \
                 wrong.",
                payer.payer,
                dollars(payer.qualified_cents),
                dollars(payer.cents),
            ));
        }
    }

    findings.extend(cross_check(
        &statements,
        FormKind::F1099Int,
        "1",
        interest_total_cents,
        "interest",
        "Schedule B Part I",
    ));
    findings.extend(cross_check(
        &statements,
        FormKind::F1099Div,
        "1a",
        ordinary_dividends_total_cents,
        "ordinary dividends",
        "Schedule B Part II",
    ));
    findings.extend(cross_check(
        &statements,
        FormKind::F1099Int,
        "8",
        tax_exempt_interest_cents,
        "tax-exempt interest",
        "Form 1040 line 2a",
    ));

    PersonalScheduleB {
        tax_year: year,
        interest,
        interest_total_cents,
        dividends,
        ordinary_dividends_total_cents,
        qualified_dividends_total_cents: qualified_total,
        tax_exempt_interest_cents,
        capital_gain_distributions_cents,
        findings,
    }
}

/// The books against the year's forms, for one box. A difference is a finding: a
/// payer who sent no form, a payment posted to the wrong account, or a form that
/// reports something the books never received.
fn cross_check(
    statements: &[TaxStatement],
    form: FormKind,
    code: &str,
    books_cents: i64,
    what: &str,
    destination: &str,
) -> Vec<String> {
    let reported: i64 = statements
        .iter()
        .filter(|s| s.form == form)
        .map(|s| s.amount(code))
        .sum();
    if reported == 0 || reported == books_cents {
        return Vec::new();
    }
    vec![format!(
        "The year's {}s report {} of {what} and the books hold {}. {destination} carries the \
         books' figure. The difference is a payment posted somewhere else, one the books never \
         received, or a payer whose form has not been recorded yet.",
        form.label(),
        dollars(reported),
        dollars(books_cents),
    )]
}

fn lines(
    by_payer: &BTreeMap<String, i64>,
    qualified: &BTreeMap<String, i64>,
) -> Vec<PayerAmount> {
    by_payer
        .iter()
        .map(|(payer, cents)| PayerAmount {
            payer: payer.clone(),
            cents: *cents,
            qualified_cents: qualified.get(payer).copied().unwrap_or(0),
        })
        .collect()
}

/// A statement's issuer against the payers the books know, case-insensitively and
/// either way round: "Broad Street Brokerage LLC" on the paper and "Broad Street
/// Brokerage" in the chart are the same payer.
fn match_payer<'a, I: Iterator<Item = &'a String>>(issuer: &str, payers: I) -> Option<String> {
    let issuer = issuer.to_ascii_lowercase();
    let mut best: Option<&'a String> = None;
    for payer in payers {
        let candidate = payer.to_ascii_lowercase();
        if candidate.is_empty() {
            continue;
        }
        if issuer.contains(&candidate) || candidate.contains(&issuer) {
            // The longest match wins, so "First Bank" does not beat "First Bank
            // of Illinois" for a statement naming the latter.
            if best.is_none_or(|b| b.len() < payer.len()) {
                best = Some(payer);
            }
        }
    }
    best.cloned()
}

/// A year's postings to a set of income accounts, grouped by the payer the entry
/// names — and what could not be attributed.
///
/// An income account is credited, which in this ledger's signing is a negative
/// amount, so the figures are negated on the way out.
///
/// The payer is the account on the **other side** of the same entry. An entry with
/// exactly one other account has exactly one payer, which is every entry a bank
/// feed or [`crate::commands::investment_commands`] produces. An entry with more
/// than one is not attributed rather than attributed arbitrarily: putting a payment
/// against the wrong payer on Schedule B is worse than leaving it in the total and
/// saying so.
fn by_payer(
    conn: &Connection,
    year: i32,
    accounts: &[String],
    names: &BTreeMap<String, String>,
) -> (BTreeMap<String, i64>, i64) {
    let mut by_payer: BTreeMap<String, i64> = BTreeMap::new();
    let mut unattributed = 0;
    for account_id in accounts {
        let Ok(mut stmt) = conn.prepare(
            "SELECT jl.entry_id, jl.amount
               FROM journal_lines jl
               JOIN journal_entries je ON je.id = jl.entry_id
              WHERE jl.account_id = ?1 AND je.is_void = 0
                AND je.date >= ?2 AND je.date <= ?3",
        ) else {
            continue;
        };
        let rows = stmt.query_map(
            rusqlite::params![account_id, format!("{year}-01-01"), format!("{year}-12-31")],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        );
        let Ok(rows) = rows else { continue };
        for (entry_id, amount) in rows.flatten() {
            let cents = -amount;
            match counterparties(conn, &entry_id, account_id) {
                Some(payer) => {
                    *by_payer
                        .entry(names.get(&payer).cloned().unwrap_or(payer))
                        .or_default() += cents
                }
                None => unattributed += cents,
            }
        }
    }
    by_payer.retain(|_, cents| *cents != 0);
    (by_payer, unattributed)
}

/// The one account on the other side of an entry, or `None` when there is not
/// exactly one.
fn counterparties(conn: &Connection, entry_id: &str, income_account: &str) -> Option<String> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT DISTINCT account_id FROM journal_lines
          WHERE entry_id = ?1 AND account_id <> ?2",
    ) else {
        return None;
    };
    let rows = stmt
        .query_map(rusqlite::params![entry_id, income_account], |r| {
            r.get::<_, String>(0)
        })
        .ok()?;
    let mut found: Vec<String> = rows.flatten().collect();
    match found.len() {
        1 => Some(found.remove(0)),
        _ => None,
    }
}

/// A year's credits to a set of accounts, as a positive amount.
fn total_for(conn: &Connection, year: i32, accounts: &[String]) -> i64 {
    accounts
        .iter()
        .filter_map(|account_id| {
            conn.query_row(
                "SELECT COALESCE(SUM(-jl.amount), 0)
                   FROM journal_lines jl
                   JOIN journal_entries je ON je.id = jl.entry_id
                  WHERE jl.account_id = ?1 AND je.is_void = 0
                    AND je.date >= ?2 AND je.date <= ?3",
                rusqlite::params![account_id, format!("{year}-01-01"), format!("{year}-12-31")],
                |r| r.get::<_, i64>(0),
            )
            .ok()
        })
        .sum()
}

fn account_names(conn: &Connection) -> BTreeMap<String, String> {
    let Ok(mut stmt) = conn.prepare("SELECT id, name FROM accounts") else {
        return BTreeMap::new();
    };
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
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
    use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
    use crate::domain::documents::StatementSource;
    use crate::domain::AccountType;
    use crate::events::types::JournalEntrySource;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::SchemaStore;
    use chrono::NaiveDate;

    const YEAR: i32 = 2025;

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

    /// An account, created and then looked up: ids are minted by the command, so
    /// a test that wants to post to one has to ask which it got.
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

    /// Post an entry from signed amounts: positive debits, negative credits.
    fn post(store: &mut EventStore, memo: &str, lines: &[(&str, i64)]) {
        EntryCommands::new(store, "user".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(YEAR, 6, 30).unwrap(),
                memo: memo.to_string(),
                reference: None,
                source: Some(JournalEntrySource::Manual),
                lines: lines
                    .iter()
                    .map(|(id, amount)| {
                        if *amount >= 0 {
                            EntryLine::debit(id, *amount, "USD")
                        } else {
                            EntryLine::credit(id, -*amount, "USD")
                        }
                    })
                    .collect(),
            })
            .unwrap();
    }

    fn statement(store: &mut EventStore, form: FormKind, issuer: &str, boxes: &[(&str, i64)]) {
        tax_statement_commands::record(
            store,
            "user",
            &TaxStatement {
                statement_id: tax_statement_commands::new_statement_id(),
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
    }

    /// The chart these tests post into, with the ids the commands minted.
    struct Chart {
        bank: String,
        broker: String,
        accounts: IncomeAccounts,
    }

    /// A bank and a broker paying interest, dividends, exempt interest and a
    /// capital gain distribution.
    fn a_year_of_income(store: &mut EventStore) -> Chart {
        let bank = account(store, "1000", "First Bank", AccountType::Asset);
        let broker = account(store, "1100", "Broad Street Brokerage", AccountType::Asset);
        let interest = account(store, "4200", "Interest", AccountType::Revenue);
        let exempt = account(store, "4210", "Tax-exempt interest", AccountType::Revenue);
        let dividends = account(store, "4300", "Dividends", AccountType::Revenue);
        let capgain = account(
            store,
            "4310",
            "Capital gain distributions",
            AccountType::Revenue,
        );

        post(store, "Bank interest", &[(&bank, 12_000), (&interest, -12_000)]);
        post(store, "Sweep interest", &[(&broker, 3_000), (&interest, -3_000)]);
        post(store, "Dividends", &[(&broker, 40_000), (&dividends, -40_000)]);
        post(
            store,
            "Municipal bond interest",
            &[(&broker, 9_000), (&exempt, -9_000)],
        );
        post(
            store,
            "Fund capital gain distribution",
            &[(&broker, 25_000), (&capgain, -25_000)],
        );

        Chart {
            bank,
            broker,
            accounts: IncomeAccounts {
                taxable_interest: vec![interest],
                tax_exempt_interest: vec![exempt],
                ordinary_dividends: vec![dividends],
                capital_gain_distributions: vec![capgain],
            },
        }
    }

    #[test]
    fn interest_and_dividends_are_summarised_per_payer() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        let b = build(store.connection(), YEAR, &chart.accounts);

        assert_eq!(b.interest.len(), 2);
        assert_eq!(b.interest[0].payer, "Broad Street Brokerage");
        assert_eq!(b.interest[0].cents, 3_000);
        assert_eq!(b.interest[1].payer, "First Bank");
        assert_eq!(b.interest[1].cents, 12_000);
        assert_eq!(b.interest_total_cents, 15_000);

        assert_eq!(b.dividends.len(), 1);
        assert_eq!(b.dividends[0].payer, "Broad Street Brokerage");
        assert_eq!(b.dividends[0].cents, 40_000);
        assert_eq!(b.ordinary_dividends_total_cents, 40_000);
    }

    /// The whole reason the split is a year-end step: nothing during the year says
    /// which dividends qualify.
    #[test]
    fn the_qualified_split_is_taken_from_the_1099_div_at_year_end() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        // Before the form: every dividend is ordinary and none is qualified.
        let before = build(store.connection(), YEAR, &chart.accounts);
        assert_eq!(before.ordinary_dividends_total_cents, 40_000);
        assert_eq!(before.qualified_dividends_total_cents, 0);

        statement(
            &mut store,
            FormKind::F1099Div,
            "Broad Street Brokerage LLC",
            &[("1a", 40_000), ("1b", 31_500)],
        );
        let after = build(store.connection(), YEAR, &chart.accounts);
        assert_eq!(after.ordinary_dividends_total_cents, 40_000, "line 6 is unchanged");
        assert_eq!(after.qualified_dividends_total_cents, 31_500);
        assert_eq!(after.dividends[0].qualified_cents, 31_500);
        assert!(after.findings.is_empty(), "{:?}", after.findings);
    }

    /// Box 8 money must never appear as interest on Schedule B. It is exempt, and
    /// putting it on line 1 is tax paid on money the statute does not reach.
    #[test]
    fn tax_exempt_interest_never_reaches_taxable_interest() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        statement(
            &mut store,
            FormKind::F1099Int,
            "First Bank",
            &[("1", 12_000), ("8", 9_000)],
        );
        statement(
            &mut store,
            FormKind::F1099Int,
            "Broad Street Brokerage",
            &[("1", 3_000)],
        );
        let b = build(store.connection(), YEAR, &chart.accounts);

        assert_eq!(b.tax_exempt_interest_cents, 9_000);
        assert_eq!(b.interest_total_cents, 15_000, "the exempt 9,000 is not in it");
        assert!(
            b.interest.iter().all(|p| p.cents != 9_000),
            "no payer line carries the exempt interest"
        );
        assert!(b.findings.is_empty(), "{:?}", b.findings);
    }

    /// A capital gain distribution is not a dividend: Schedule D line 13, and
    /// nothing on Schedule B.
    #[test]
    fn a_capital_gain_distribution_stays_off_schedule_b() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        let b = build(store.connection(), YEAR, &chart.accounts);

        assert_eq!(b.capital_gain_distributions_cents, 25_000);
        assert_eq!(b.ordinary_dividends_total_cents, 40_000);
        assert!(b.dividends.iter().all(|p| p.cents != 65_000));
        let on_schedule_b: i64 = b.interest_total_cents + b.ordinary_dividends_total_cents;
        assert_eq!(on_schedule_b, 55_000, "no part of the 25,000 is on the schedule");
    }

    #[test]
    fn a_form_that_disagrees_with_the_books_is_reported_and_the_books_are_carried() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        statement(
            &mut store,
            FormKind::F1099Int,
            "First Bank",
            &[("1", 14_000)],
        );
        let b = build(store.connection(), YEAR, &chart.accounts);
        assert_eq!(b.interest_total_cents, 15_000, "the books are carried");
        assert_eq!(b.findings.len(), 1);
        assert!(b.findings[0].contains("$140.00 of interest"));
        assert!(b.findings[0].contains("the books hold $150.00"));
        crate::tax::warning_shape::assert_all(&b.findings);
    }

    #[test]
    fn a_1099_div_naming_no_payer_the_books_know_still_reaches_line_3a() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        statement(
            &mut store,
            FormKind::F1099Div,
            "Some Other Custodian",
            &[("1b", 5_000)],
        );
        let b = build(store.connection(), YEAR, &chart.accounts);
        assert_eq!(b.qualified_dividends_total_cents, 5_000);
        assert_eq!(b.dividends[0].qualified_cents, 0);
        assert!(b.findings.iter().any(|f| f.contains("no dividend payer in the books matches")));
        crate::tax::warning_shape::assert_all(&b.findings);
    }

    /// A payment whose entry names two other accounts cannot be attributed, and
    /// guessing would put it against the wrong payer.
    #[test]
    fn income_with_no_single_counterparty_stays_in_the_total_and_is_reported() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        post(
            &mut store,
            "Interest split across two accounts",
            &[
                (&chart.bank, 1_000),
                (&chart.broker, 1_000),
                (&chart.accounts.taxable_interest[0].clone(), -2_000),
            ],
        );
        let b = build(store.connection(), YEAR, &chart.accounts);
        assert_eq!(b.interest_total_cents, 17_000);
        assert_eq!(b.interest.iter().map(|p| p.cents).sum::<i64>(), 15_000);
        assert!(b.findings.iter().any(|f| f.contains("more than one account on the other side")));
        crate::tax::warning_shape::assert_all(&b.findings);
    }

    /// A qualified part larger than the whole is one of the two figures being
    /// wrong, and it is said rather than clamped.
    #[test]
    fn a_qualified_part_larger_than_the_ordinary_dividends_is_reported() {
        let mut store = books();
        let chart = a_year_of_income(&mut store);
        statement(
            &mut store,
            FormKind::F1099Div,
            "Broad Street Brokerage",
            &[("1a", 55_000), ("1b", 55_000)],
        );
        let b = build(store.connection(), YEAR, &chart.accounts);
        assert_eq!(b.dividends[0].cents, 40_000);
        assert_eq!(b.dividends[0].qualified_cents, 55_000);
        assert!(b
            .findings
            .iter()
            .any(|f| f.contains("cannot be larger than the whole")));
        crate::tax::warning_shape::assert_all(&b.findings);
    }

    #[test]
    fn a_longer_payer_name_wins_a_match_over_a_shorter_one() {
        let payers = ["First Bank".to_string(), "First Bank of Illinois".to_string()];
        assert_eq!(
            match_payer("FIRST BANK OF ILLINOIS, N.A.", payers.iter()).as_deref(),
            Some("First Bank of Illinois")
        );
    }
}
