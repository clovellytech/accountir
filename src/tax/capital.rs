//! Schedule K-1 item L: each partner's capital account, from the books.
//!
//! Item L is a six-row arithmetic identity the IRS prints on every K-1:
//!
//! ```text
//!   beginning capital account
//! + capital contributed during the year
//! + current year net income (loss)
//! + other increase (decrease)
//! - withdrawals and distributions
//! = ending capital account
//! ```
//!
//! Four of those rows are ledger movements on the accounts a partner's capital
//! actually lives in, one is their slice of the year's result, and one — "other
//! increase (decrease)" — is a row the form itself asks you to attach an
//! explanation for. See [`CapitalAccount::other`] for why this program leaves it
//! at zero rather than plugging the identity with it.
//!
//! # Which accounts are whose
//!
//! Only the ones somebody linked, in `partner_equity_accounts`. The books hold
//! `4002 Zak` and `4005 Zak` — one partner's contributions and draws — and
//! nothing but the name says so. Matching on the name would reassign a partner's
//! capital the day somebody renames an account, and a K-1 that quietly moved
//! another partner's money into this one's is not a form anybody can see is
//! wrong.
//!
//! # Signs
//!
//! The ledger holds equity credit-normal, so a contribution is a credit and
//! arrives here negative, while a draw is a debit and arrives positive. Item L
//! prints capital as a positive figure and prints withdrawals into a box whose
//! parentheses are already on the paper. So contributions and the beginning
//! balance are negated on the way out and withdrawals are not — get this
//! backwards and item L still foots, because the identity is symmetric under a
//! sign flip, while every figure on it is the wrong way up.
//!
//! # What "beginning capital account" here actually includes
//!
//! It is the balance of the partner's own linked accounts on the day before the
//! tax year opens, and nothing else. That is every contribution they have ever
//! made and every draw they have ever taken — and **not** their share of prior
//! years' income, unless the books were closed each year by posting that share
//! into those same accounts.
//!
//! Deriving the missing part is not a matter of reading further back. A prior
//! year's allocation needs that year's Schedule K, and Schedule K is built from
//! the line mapping, the deduction limits and the depreciation register — none of
//! which carry a year. `tax_line_mappings` holds one row per account, the one in
//! force today, so replaying 2019 through them totals 2019's ledger onto 2026's
//! return. The partners' percentages *are* dated now
//! ([`crate::domain::SharePeriod`]), which is the half of the problem that got
//! solved; the figure they would be applied to is the half that did not.
//!
//! A number computed that way would look exactly like tax-basis capital, would
//! not be, and would be impossible to tell apart from one that is — which is the
//! failure worth avoiding here, not the missing figure itself.
//!
//! So the partial figure is reported and [`BEGINNING_CAPITAL_CAVEAT`] says out
//! loud what it leaves out. A preparer who closes income to the partners' capital
//! accounts each year gets a complete item L from this; one who does not gets a
//! figure they have to finish by hand, and is told so rather than left to notice.
//!
//! # §704(b) and losses
//!
//! A loss allocation is respected only so far as it does not drive a partner's
//! capital account below zero — past that it needs a deficit restoration
//! obligation in the partnership agreement, which is a document and not a ledger
//! entry. [`CapitalAccount::unsupported_loss`] finds the amount and
//! [`CapitalAccount::warnings`] names it. A warning and not a refusal: whether
//! there is a DRO is a fact this program cannot see, and a return that will not
//! build is worse than one that builds with a question attached to it.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use rusqlite::Connection;

use super::allocate::Basis;
use super::lines::cents_to_dollars;
use crate::domain::Partner;

/// The role strings the event carries, and `partner_equity_accounts.role` holds.
///
/// Duplicated here rather than imported from the validator: the validator refuses
/// anything else on the way in, and this has to read rows that are already on
/// disk — including any a hand-run `UPDATE` put there.
pub const CONTRIBUTION: &str = "contribution";
pub const DRAW: &str = "draw";

/// Said on every return that computes an item L. See the module docs.
pub const BEGINNING_CAPITAL_CAVEAT: &str =
    "Item L's beginning capital account is the balance of each partner's linked equity accounts \
     on the last day of the prior year — their contributions less their draws. It does not \
     include their share of prior years' income unless the books post that share into those \
     accounts at each year end. Check every beginning figure against last year's K-1 before \
     filing; where they differ, the difference is undistributed income that has to be added by \
     hand.";

/// Which side of a partner's capital an account records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Money in. Item L row 2.
    Contribution,
    /// Money out. Item L row 5.
    Draw,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            CONTRIBUTION => Some(Role::Contribution),
            DRAW => Some(Role::Draw),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Contribution => CONTRIBUTION,
            Role::Draw => DRAW,
        }
    }
}

/// One link: this account holds this partner's capital, in this role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquityAccount {
    pub partner_id: String,
    pub account_id: String,
    pub role: Role,
}

/// Every account linked to a partner, in a stable order.
///
/// A row whose role is neither word is dropped rather than guessed at — half of
/// item L is the difference between the two, so a link that does not say which
/// side it is on cannot be placed at all. [`compute`] counts what was dropped and
/// says so, because a silently ignored link is a partner whose draws simply do
/// not appear.
pub fn load_partner_equity_accounts(conn: &Connection) -> Vec<EquityAccount> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT partner_id, account_id, role FROM partner_equity_accounts
         ORDER BY partner_id, account_id",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|(partner_id, account_id, role)| {
            Some(EquityAccount {
                partner_id,
                account_id,
                role: Role::parse(&role)?,
            })
        })
        .collect()
}

/// One partner's item L, in whole dollars.
///
/// Dollars and not cents because every figure here is either already whole —
/// [`allocate`] apportions the K-1s in dollars so they add back to Schedule K —
/// or is a ledger total rounded once, here. [`ending`] then sums the rounded
/// rows rather than rounding a sum of cents, so the six rows on the paper add up
/// as read. A column that does not foot is the first thing an examiner checks.
///
/// [`ending`]: CapitalAccount::ending
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapitalAccount {
    pub partner_id: String,
    /// Carried so a warning can name the partner rather than their id.
    pub partner_name: String,
    /// Row 1. Their linked accounts' balance the day before the year opened.
    pub beginning: i64,
    /// Row 2. Movements on their `contribution` accounts during the year.
    pub contributed: i64,
    /// Row 3. Their allocated share of the Analysis of Net Income figure.
    pub net_income: i64,
    /// Row 4. Always zero.
    ///
    /// The form wants an explanation attached to whatever goes here, which is
    /// the IRS saying this row is for facts that are not ordinary contributions,
    /// draws or income — a transfer of interest, a revaluation, a correction of
    /// a prior year. None of those is distinguishable in a general ledger from
    /// any other entry on the same account. Left at zero and kept as a named
    /// field so a caller can see it was not merely forgotten; a residual plugged
    /// in here would make item L foot while asserting something nobody wrote.
    pub other: i64,
    /// Row 5. Movements on their `draw` accounts during the year, as a positive
    /// figure — the box's parentheses are printed on the form.
    pub withdrawals: i64,
    /// How many accounts fed the four ledger rows. Zero means item L is their
    /// income share and nothing else.
    pub linked_accounts: usize,
}

impl CapitalAccount {
    /// Row 6. The five rows above it, in the order the form adds them.
    pub fn ending(&self) -> i64 {
        self.beginning + self.contributed + self.net_income + self.other - self.withdrawals
    }

    /// How much of this year's allocated loss the capital account cannot support.
    ///
    /// `None` when the year allocated them income, or when the loss leaves them
    /// still positive. Otherwise the amount by which the loss ran past zero,
    /// which is the whole loss when they were already negative before it.
    pub fn unsupported_loss(&self) -> Option<i64> {
        if self.net_income >= 0 || self.ending() >= 0 {
            return None;
        }
        Some((-self.ending()).min(-self.net_income))
    }

    /// What somebody should know about this partner's item L before filing.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();

        if self.linked_accounts == 0 {
            out.push(format!(
                "{}: no equity account is linked to them, so item L shows their share of this \
                 year's income and nothing else — no opening balance, no contributions, no \
                 draws. Link their capital accounts (`partnership equity link`) and regenerate.",
                self.partner_name
            ));
        }

        // §704(b): the allocation is respected only to the extent the capital
        // account can carry it. Said even when the agreement does have a deficit
        // restoration obligation, because the check this replaces is somebody
        // remembering to look, and the figure is the one they would have to work
        // out by hand to know whether it matters.
        if let Some(amount) = self.unsupported_loss() {
            out.push(format!(
                "{}: this year's allocated loss of {} takes their capital account to {}, which is \
                 {} below zero. Under section 704(b) a loss allocation is respected only so far as \
                 the capital account can carry it — past that the partnership agreement needs a \
                 deficit restoration obligation, or the loss has to be reallocated to the partners \
                 whose capital can absorb it.",
                self.partner_name,
                super::lines::format_dollars(self.net_income),
                super::lines::format_dollars(self.ending()),
                super::lines::format_dollars(amount),
            ));
        }

        // A draw account in net credit for the year. The box prints its figure
        // inside parentheses already on the paper, so a negative written there
        // reads as a positive, which is the one error in item L that cannot be
        // seen on the finished form.
        if self.withdrawals < 0 {
            out.push(format!(
                "{}: their draw accounts were credited {} more than they were debited this year, \
                 so item L's withdrawals row is negative. That box is pre-printed with \
                 parentheses, so the figure will read as a distribution rather than as a return of \
                 one — check whether a contribution was posted to a draw account.",
                self.partner_name,
                super::lines::format_dollars(-self.withdrawals),
            ));
        }

        out
    }
}

/// An equity account carrying a balance that no partner claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkedEquity {
    pub account_id: String,
    pub account_number: String,
    pub name: String,
    /// Its balance the day before the year opened, oriented the way capital
    /// prints — positive is capital.
    pub dollars: i64,
}

/// Item L for every partner on a return, and what it could not account for.
#[derive(Debug, Clone, Default)]
pub struct Capital {
    /// One per partner passed to [`compute`], in the same order.
    pub accounts: Vec<CapitalAccount>,
    /// Equity accounts with an opening balance and no partner behind them.
    pub unlinked_equity: Vec<UnlinkedEquity>,
    /// Links whose role was neither word, and which were therefore dropped.
    pub unreadable_links: usize,
    /// The read failed and item L is blank. Carried rather than returned as an
    /// error, because a missing balance sheet is not a reason to refuse the whole
    /// return.
    pub failed: Option<String>,
}

impl Capital {
    pub fn for_partner(&self, partner_id: &str) -> Option<&CapitalAccount> {
        self.accounts.iter().find(|a| a.partner_id == partner_id)
    }

    /// Whether anything was computed at all.
    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// What somebody should know about the return's item L as a whole.
    ///
    /// The per-partner warnings are on [`CapitalAccount::warnings`] and are
    /// emitted beside the K-1 they concern; these are the ones that belong to the
    /// return.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();

        if let Some(why) = &self.failed {
            out.push(format!(
                "Item L is blank on every K-1: the partners' capital accounts could not be read \
                 from the books ({why}). The rest of the return is unaffected."
            ));
            return out;
        }

        if self.accounts.is_empty() {
            return out;
        }

        out.push(BEGINNING_CAPITAL_CAVEAT.to_string());

        if self.unreadable_links > 0 {
            out.push(format!(
                "{} account link(s) say neither \"{CONTRIBUTION}\" nor \"{DRAW}\" and were \
                 ignored, so whatever they hold is missing from item L. Link those accounts \
                 again.",
                self.unreadable_links
            ));
        }

        if !self.unlinked_equity.is_empty() {
            let named: Vec<String> = self
                .unlinked_equity
                .iter()
                .map(|e| {
                    format!(
                        "{} {} ({})",
                        e.account_number,
                        e.name,
                        super::lines::format_dollars(e.dollars)
                    )
                })
                .collect();
            // Not necessarily an error — retained earnings and an opening-balance
            // plug both live here legitimately. It is said anyway because the case
            // it catches is the one that looks identical: a partner's own capital
            // account that nobody linked, whose balance is then missing from their
            // item L and present in nobody else's.
            out.push(format!(
                "Equity accounts carrying an opening balance that no partner claims: {}. Anything \
                 here that is a partner's capital is missing from their item L; anything that is \
                 retained earnings belongs to all of them and is missing from every one.",
                named.join(", ")
            ));
        }

        out
    }
}

/// Compute item L for `partners`, splitting `net_income` between them.
///
/// `net_income` is the Analysis of Net Income figure — Schedule K as a single
/// number — and is split on [`Basis::ProfitOrLoss`] so a year that made money
/// travels on the profit percentages and a year that lost it on the loss ones,
/// exactly as each partner's Part III does. Using the capital percentage here
/// instead would be the tempting mistake: item L is a capital account, but the
/// figure entering it is a distributive share.
///
/// Split on the percentages in force at the year's end, not today's, and on the
/// same date [`super::form1065`] splits Part III on. Item L row 3 and Part III
/// box 1 are the same partner's share of the same year read off one page, so the
/// two arriving on different percentages is a K-1 that contradicts itself.
///
/// The shares need not total the whole. [`allocate_as_of`] apportions what the
/// percentages claim and leaves any shortfall unallocated rather than inventing
/// dollars to make the K-1s foot, so a partnership whose splits sum to 90% closes
/// the year with a tenth of it in nobody's capital account — which is the state
/// of their records, and is warned about where the shares are checked.
pub fn compute(
    conn: &Connection,
    year: i32,
    partners: &[&Partner],
    net_income: i64,
) -> Result<Capital, rusqlite::Error> {
    let (year_start, year_end) = crate::commands::partnership_commands::calendar_year(year);
    // "Beginning of tax year" is the position before the year's first entry, the
    // same convention Schedule L's opening column uses. Asking as of January 1
    // would count January 1 in both the opening balance and the year's activity.
    let opening = year_start
        .pred_opt()
        .expect("January 1 has a day before it");

    let links = load_partner_equity_accounts(conn);
    let total_links: i64 = conn
        .query_row("SELECT COUNT(*) FROM partner_equity_accounts", [], |r| {
            r.get(0)
        })
        .unwrap_or(links.len() as i64);

    let mut by_partner: BTreeMap<&str, Vec<&EquityAccount>> = BTreeMap::new();
    for link in &links {
        by_partner
            .entry(link.partner_id.as_str())
            .or_default()
            .push(link);
    }

    // The same split Part III is built on — segmented at each change of interest
    // where the year contained one (§706(d)), and the year-end percentages where
    // it did not.
    //
    // Not simply `allocate_as_of(.., year_end)`: once `split_across_partners`
    // started dividing the year, item L row 3 and box 1 stopped agreeing. On a
    // year with a mid-year change that put 172,755 in row 3 beside a box 1 of
    // 155,560 — two figures for the same partner's share of the same year, on
    // the same page, differing by the part of the year they held a different
    // percentage.
    let shares =
        super::varying::allocate_over_year(conn, year, net_income, partners, Basis::ProfitOrLoss);

    let mut accounts = Vec::with_capacity(partners.len());
    for (i, p) in partners.iter().enumerate() {
        let mine = by_partner.get(p.partner_id.as_str());
        let mut beginning_cents = 0i64;
        let mut contributed_cents = 0i64;
        let mut drawn_cents = 0i64;

        for link in mine.into_iter().flatten() {
            // Every linked account, whatever its role: an opening balance is the
            // net of what went in and what came out, and splitting it by role
            // would report a partner's lifetime contributions as their opening
            // capital and lose every draw they ever took.
            // The opening balance is a *position*, so it counts everything —
            // including earlier years' closing entries, whose allocations are
            // genuinely part of what this partner had at the start.
            beginning_cents += movement(conn, &link.account_id, None, Some(opening), true)?;

            // The year's own contributions and draws are *activity*, and a
            // closing entry is neither. Once a partnership closes to partner
            // capital, this year's allocation lands in the contribution account —
            // and counting it here would report it as capital the partner paid
            // in, on row 2, while row 3 reports the same money as their share of
            // income. One year's earnings, twice, on one K-1.
            let in_year = movement(
                conn,
                &link.account_id,
                Some(year_start),
                Some(year_end),
                false,
            )?;
            match link.role {
                Role::Contribution => contributed_cents += in_year,
                Role::Draw => drawn_cents += in_year,
            }
        }

        accounts.push(CapitalAccount {
            partner_id: p.partner_id.clone(),
            partner_name: p.name.clone(),
            beginning: cents_to_dollars(-beginning_cents),
            contributed: cents_to_dollars(-contributed_cents),
            net_income: shares.get(i).map(|s| s.dollars).unwrap_or(0),
            other: 0,
            withdrawals: cents_to_dollars(drawn_cents),
            linked_accounts: mine.map(Vec::len).unwrap_or(0),
        });
    }

    Ok(Capital {
        accounts,
        unlinked_equity: unlinked_equity(conn, opening)?,
        unreadable_links: (total_links - links.len() as i64).max(0) as usize,
        failed: None,
    })
}

/// Item L for the partners a return actually files a K-1 for.
///
/// The filter is the one [`crate::tax::form1065`] applies before it builds any
/// K-1, and it is repeated here rather than left to the caller because the shares
/// have to be apportioned over the *same* set: allocating over every partner who
/// ever held an interest hands dollars to partners whose K-1 is not in the
/// bundle, and item L then disagrees with Part III on the same page.
///
/// Failures come back as a [`Capital`] carrying `failed` rather than as an error.
/// A ledger this cannot read is a blank item L and a warning, not a return that
/// refuses to build.
pub fn for_return(
    conn: &Connection,
    year: i32,
    partners: &[crate::tax::form1065::PartnerFiling],
    net_income: i64,
) -> Capital {
    let (year_start, year_end) = crate::commands::partnership_commands::calendar_year(year);
    let filed: Vec<&Partner> = partners
        .iter()
        .map(|f| &f.partner)
        .filter(|p| p.was_partner_during(year_start, year_end))
        .collect();

    match compute(conn, year, &filed, net_income) {
        Ok(capital) => capital,
        Err(e) => Capital {
            failed: Some(e.to_string()),
            ..Capital::default()
        },
    }
}

/// One account's net movement over a date range, in cents, debit-positive.
///
/// Voided entries are excluded, matching every other reader of this ledger — a
/// reversed draw that still counted would show a partner having taken money the
/// books say they gave back.
/// What moved on an account between two dates.
///
/// `include_closing` decides whether year-end closing entries count. Both
/// answers are needed and they are not interchangeable — see the two call sites
/// in [`compute`], and `AccountQueries::period_movement` for the same
/// distinction drawn for the income statement.
fn movement(
    conn: &Connection,
    account_id: &str,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    include_closing: bool,
) -> Result<i64, rusqlite::Error> {
    let mut sql = String::from(
        "SELECT COALESCE(SUM(jl.amount), 0)
         FROM journal_lines jl
         JOIN journal_entries je ON jl.entry_id = je.id
         WHERE jl.account_id = ?1 AND je.is_void = 0",
    );
    if !include_closing {
        sql.push_str(" AND (je.source IS NULL OR je.source != 'closing')");
    }
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(account_id.to_string())];
    if let Some(from) = from {
        params.push(Box::new(from.to_string()));
        sql.push_str(&format!(" AND je.date >= ?{}", params.len()));
    }
    if let Some(to) = to {
        params.push(Box::new(to.to_string()));
        sql.push_str(&format!(" AND je.date <= ?{}", params.len()));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    conn.query_row(&sql, refs.as_slice(), |r| r.get(0))
}

/// Equity accounts with an opening balance that no partner is linked to.
fn unlinked_equity(
    conn: &Connection,
    opening: NaiveDate,
) -> Result<Vec<UnlinkedEquity>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.account_number, a.name,
                COALESCE((SELECT SUM(jl.amount) FROM journal_lines jl
                          JOIN journal_entries je ON jl.entry_id = je.id
                          WHERE jl.account_id = a.id AND je.is_void = 0 AND je.date <= ?1), 0)
         FROM accounts a
         WHERE a.account_type = 'equity'
           AND a.id NOT IN (SELECT account_id FROM partner_equity_accounts)
         ORDER BY a.account_number",
    )?;
    let rows = stmt.query_map([opening.to_string()], |r| {
        Ok(UnlinkedEquity {
            account_id: r.get(0)?,
            account_number: r.get(1)?,
            name: r.get(2)?,
            // Credit-normal, like everything else in this module.
            dollars: cents_to_dollars(-r.get::<_, i64>(3)?),
        })
    })?;
    Ok(rows.flatten().filter(|e| e.dollars != 0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::partnership_commands as pc;
    use crate::domain::{Address, Shares};
    use crate::events::types::{Event, EventAccountType, EventEnvelope, JournalLineData};
    use crate::store::event_store::EventStore;
    use crate::store::projections::ProjectionStore;

    const YEAR: i32 = 2025;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn address() -> Address {
        Address {
            street: "2 Other Road".into(),
            suite: None,
            city: "Cape Town".into(),
            state: "WC".into(),
            postal_code: "8001".into(),
            country: None,
        }
    }

    /// Two partners at 50/50, the four equity accounts their capital lives in,
    /// and one more nobody claims — the shape of the real books this was written
    /// against, where `4007` belongs to a partner who left before there was a
    /// partner record to leave.
    fn books() -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();

        let accounts = [
            ("cash", EventAccountType::Asset, "1000", "Cash"),
            (
                "zak-in",
                EventAccountType::Equity,
                "4002",
                "Zak contributions",
            ),
            (
                "jinny-in",
                EventAccountType::Equity,
                "4003",
                "Jinny contributions",
            ),
            ("zak-out", EventAccountType::Equity, "4005", "Zak draws"),
            ("jinny-out", EventAccountType::Equity, "4006", "Jinny draws"),
            ("lois", EventAccountType::Equity, "4007", "Lois capital"),
        ];
        for (id, ty, number, name) in accounts {
            let e = Event::AccountCreated {
                account_id: id.into(),
                account_type: ty,
                account_number: number.into(),
                name: name.into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            };
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        }

        for (id, name) in [("zak", "Zak"), ("jinny", "Jinny")] {
            let e = Event::PartnerAdmitted(Box::new(crate::events::types::PartnerAdmittedData {
                partner_id: id.into(),
                name: name.into(),
                partner_type: "general".into(),
                residency: "domestic".into(),
                entity_type: "Individual".into(),
                address: (&address()).into(),
                start_date: day(2020, 1, 1),
                shares: Shares::from_percents(50.0, 50.0, 50.0).into(),
            }));
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        }

        store
    }

    /// Post one entry. Cents, debits positive and credits negative, exactly as
    /// the ledger holds them.
    fn post(store: &mut EventStore, id: &str, on: NaiveDate, pairs: &[(&str, i64)]) {
        let lines: Vec<JournalLineData> = pairs
            .iter()
            .enumerate()
            .map(|(i, (acct, amount))| JournalLineData {
                line_id: format!("{id}-{i}"),
                account_id: (*acct).into(),
                amount: *amount,
                currency: "USD".into(),
                exchange_rate: None,
                memo: None,
            })
            .collect();
        let e = Event::JournalEntryPosted {
            entry_id: id.into(),
            date: on,
            memo: "seed".into(),
            lines,
            reference: None,
            source: None,
        };
        let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
        store.apply_projection(&stored).unwrap();
    }

    fn link_all(store: &mut EventStore) {
        for (partner, account, role) in [
            ("zak", "zak-in", CONTRIBUTION),
            ("zak", "zak-out", DRAW),
            ("jinny", "jinny-in", CONTRIBUTION),
            ("jinny", "jinny-out", DRAW),
        ] {
            pc::link_equity_account(store, "u", partner, account, role).unwrap();
        }
    }

    fn partners(conn: &rusqlite::Connection) -> Vec<Partner> {
        let mut ps = pc::list_partners(conn);
        // A stable order so the shares land where the assertions expect them.
        ps.sort_by(|a, b| a.partner_id.cmp(&b.partner_id));
        ps
    }

    fn run(store: &EventStore, net_income: i64) -> Capital {
        let ps = partners(store.connection());
        let refs: Vec<&Partner> = ps.iter().collect();
        compute(store.connection(), YEAR, &refs, net_income).unwrap()
    }

    /// Item L row 3 follows §706(d) too, or it contradicts box 1 on the same page.
    ///
    /// Row 3 used to be split on the year-end percentages while Part III was
    /// split over the year's segments. On a year whose split moved in July that
    /// put two different figures for the same partner's share of the same year
    /// on the same K-1 — and the one in item L was the one nobody checks.
    #[test]
    fn the_income_row_is_split_over_the_year_the_way_part_three_is() {
        let mut store = books();
        link_all(&mut store);
        // A revenue account, mapped, so the segments have figures to weight by:
        // the split over the year is read from the books, not assumed.
        let e = Event::AccountCreated {
            account_id: "revenue".into(),
            account_type: EventAccountType::Revenue,
            account_number: "5000".into(),
            name: "Sales".into(),
            parent_id: None,
            currency: Some("USD".into()),
            description: None,
        };
        let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
        store.apply_projection(&stored).unwrap();
        crate::commands::tax_setup_commands::set_account_line(
            &mut store, "u", "revenue", "l1a", 2025,
        )
        .unwrap();

        // Earned evenly: half the year's income before July, half after.
        post(
            &mut store,
            "h1",
            day(YEAR, 3, 1),
            &[("cash", 100_000_00), ("revenue", -100_000_00)],
        );
        post(
            &mut store,
            "h2",
            day(YEAR, 9, 1),
            &[("cash", 100_000_00), ("revenue", -100_000_00)],
        );
        // Zak goes from half to nine tenths on 1 July.
        crate::commands::share_period_commands::set_partner_shares(
            &mut store,
            "u",
            "zak",
            day(YEAR, 7, 1),
            Shares::from_percents(90.0, 90.0, 90.0),
        )
        .unwrap();
        crate::commands::share_period_commands::set_partner_shares(
            &mut store,
            "u",
            "jinny",
            day(YEAR, 7, 1),
            Shares::from_percents(10.0, 10.0, 10.0),
        )
        .unwrap();

        let capital = run(&store, 200_000);
        let zak = capital.for_partner("zak").expect("Zak has an item L");

        // Half the year at 50% and half at 90% is 70% — not 90%, which is what
        // the year-end split alone would have given him.
        assert_eq!(
            zak.net_income, 140_000,
            "row 3 has to follow the split that actually held: {:?}",
            capital.accounts
        );
        let jinny = capital.for_partner("jinny").expect("Jinny has an item L");
        assert_eq!(jinny.net_income, 60_000);
        assert_eq!(
            zak.net_income + jinny.net_income,
            200_000,
            "and the two still foot to the Analysis of Net Income"
        );
    }

    /// The whole of item L in one test: contributions and draws come off the
    /// ledger the right way up, the prior year lands in the opening balance, and
    /// the six rows add up as the form adds them.
    #[test]
    fn a_capital_account_is_the_identity_the_form_prints() {
        let mut store = books();
        link_all(&mut store);

        // Before the year: Zak put in $10,000 and drew $2,000.
        post(
            &mut store,
            "p1",
            day(2024, 3, 1),
            &[("cash", 1_000_000), ("zak-in", -1_000_000)],
        );
        post(
            &mut store,
            "p2",
            day(2024, 9, 1),
            &[("zak-out", 200_000), ("cash", -200_000)],
        );
        // During the year: another $4,000 in, $1,500 out.
        post(
            &mut store,
            "c1",
            day(YEAR, 4, 1),
            &[("cash", 400_000), ("zak-in", -400_000)],
        );
        post(
            &mut store,
            "d1",
            day(YEAR, 8, 1),
            &[("zak-out", 150_000), ("cash", -150_000)],
        );

        let capital = run(&store, 6_000);
        let zak = capital
            .for_partner("zak")
            .expect("Zak has a capital account");

        assert_eq!(zak.beginning, 8_000, "10,000 in less 2,000 out");
        assert_eq!(zak.contributed, 4_000);
        assert_eq!(
            zak.withdrawals, 1_500,
            "printed as a magnitude, not a negative"
        );
        assert_eq!(zak.net_income, 3_000, "half of 6,000");
        assert_eq!(zak.other, 0);
        assert_eq!(zak.ending(), 8_000 + 4_000 + 3_000 - 1_500);
    }

    /// The sign error this module's whole orientation exists to prevent: equity
    /// is credit-normal, so a contribution arrives negative and has to come out
    /// positive. Reversed, item L still foots and every figure on it is upside
    /// down.
    #[test]
    fn a_contribution_is_a_credit_and_prints_positive() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "c1",
            day(YEAR, 4, 1),
            &[("cash", 500_000), ("zak-in", -500_000)],
        );

        let capital = run(&store, 0);
        let zak = capital.for_partner("zak").unwrap();
        assert_eq!(zak.contributed, 5_000);
        assert!(zak.ending() > 0, "a partner who paid money in has capital");
    }

    /// "Beginning of tax year" is the position before the year's first entry.
    /// An entry on January 1 counted in both would report the same dollar twice.
    #[test]
    fn the_opening_balance_stops_the_day_before_the_year_starts() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "eve",
            day(YEAR - 1, 12, 31),
            &[("cash", 100_000), ("zak-in", -100_000)],
        );
        post(
            &mut store,
            "day1",
            day(YEAR, 1, 1),
            &[("cash", 700_000), ("zak-in", -700_000)],
        );

        let zak = run(&store, 0);
        let zak = zak.for_partner("zak").unwrap();
        assert_eq!(zak.beginning, 1_000, "New Year's Eve is opening capital");
        assert_eq!(zak.contributed, 7_000, "New Year's Day is this year's");
    }

    /// A partner's opening balance is the net of both their accounts. Taking only
    /// the contribution account would report their lifetime contributions as
    /// opening capital and lose every draw they ever took.
    #[test]
    fn the_opening_balance_nets_both_of_a_partners_accounts() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "p1",
            day(2022, 1, 1),
            &[("cash", 900_000), ("zak-in", -900_000)],
        );
        post(
            &mut store,
            "p2",
            day(2023, 1, 1),
            &[("zak-out", 400_000), ("cash", -400_000)],
        );

        let capital = run(&store, 0);
        assert_eq!(capital.for_partner("zak").unwrap().beginning, 5_000);
    }

    /// §704(b): the loss allocation this return makes is bigger than the capital
    /// account behind it, and the return has to say so by name and by amount.
    #[test]
    fn a_loss_that_runs_a_capital_account_negative_is_named() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "p1",
            day(2024, 1, 1),
            &[("cash", 100_000), ("zak-in", -100_000)],
        );
        post(
            &mut store,
            "p2",
            day(2024, 1, 1),
            &[("cash", 100_000), ("jinny-in", -100_000)],
        );

        // A $10,000 loss, half each, against $1,000 of capital each.
        let capital = run(&store, -10_000);
        let zak = capital.for_partner("zak").unwrap();
        assert_eq!(zak.ending(), 1_000 - 5_000);
        assert_eq!(zak.unsupported_loss(), Some(4_000));

        let warnings = zak.warnings();
        let w = warnings
            .iter()
            .find(|w| w.contains("704(b)"))
            .expect("the unsupported loss must be reported");
        assert!(w.contains("Zak"), "the partner has to be named: {w}");
        assert!(w.contains("4,000"), "the amount has to be named: {w}");
        crate::tax::warning_shape::assert_all(&warnings);
    }

    /// The other half of the check: a loss the capital account can carry is not
    /// worth a warning, or every return with a bad year is noise.
    #[test]
    fn a_loss_a_partner_can_absorb_is_not_reported() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "p1",
            day(2024, 1, 1),
            &[("cash", 5_000_000), ("zak-in", -5_000_000)],
        );
        post(
            &mut store,
            "p2",
            day(2024, 1, 1),
            &[("cash", 5_000_000), ("jinny-in", -5_000_000)],
        );

        let capital = run(&store, -10_000);
        let zak = capital.for_partner("zak").unwrap();
        assert_eq!(zak.unsupported_loss(), None);
        assert!(zak.warnings().is_empty(), "{:?}", zak.warnings());
    }

    /// A partner already under water takes no more capital with them: the whole
    /// of this year's loss is unsupported, not only the part below zero.
    #[test]
    fn a_partner_already_negative_has_the_whole_loss_unsupported() {
        let mut store = books();
        link_all(&mut store);
        // Drew more than they ever put in.
        post(
            &mut store,
            "p1",
            day(2024, 1, 1),
            &[("zak-out", 300_000), ("cash", -300_000)],
        );

        let capital = run(&store, -10_000);
        let zak = capital.for_partner("zak").unwrap();
        assert_eq!(zak.beginning, -3_000);
        assert_eq!(zak.unsupported_loss(), Some(5_000), "all of their share");
    }

    /// Income never triggers the §704(b) check, however negative the account is.
    /// The rule is about loss allocations; a negative capital account with income
    /// on it is a partner digging themselves out.
    #[test]
    fn a_negative_account_with_income_allocated_is_not_a_704b_problem() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "p1",
            day(2024, 1, 1),
            &[("zak-out", 900_000), ("cash", -900_000)],
        );

        let capital = run(&store, 10_000);
        let zak = capital.for_partner("zak").unwrap();
        assert!(zak.ending() < 0);
        assert_eq!(zak.unsupported_loss(), None);
    }

    /// A partner nobody linked an account to gets an item L that is only their
    /// income share — true, and useless without being told why.
    #[test]
    fn a_partner_with_no_linked_accounts_is_said_out_loud() {
        let store = books();
        let capital = run(&store, 10_000);
        let zak = capital.for_partner("zak").unwrap();

        assert_eq!(zak.linked_accounts, 0);
        assert_eq!(zak.beginning, 0);
        assert_eq!(zak.ending(), 5_000);
        let warnings = zak.warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("no equity account is linked")),
            "{warnings:?}"
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }

    /// The former partner's account on the real books: equity with a balance and
    /// no partner behind it. Silence here is a capital account that is on nobody's
    /// K-1 and on no warning either.
    #[test]
    fn equity_no_partner_claims_is_named_with_its_balance() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "p1",
            day(2019, 1, 1),
            &[("cash", 1_234_500), ("lois", -1_234_500)],
        );

        let capital = run(&store, 0);
        let orphan = capital
            .unlinked_equity
            .iter()
            .find(|e| e.account_number == "4007")
            .expect("4007 has a balance and no partner");
        assert_eq!(orphan.dollars, 12_345, "credit-normal, printed positive");

        let warnings = capital.warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("4007") && w.contains("12,345")),
            "{warnings:?}"
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }

    /// The caveat is unconditional, because what the beginning figure leaves out
    /// is invisible on the finished form: a partial capital account looks exactly
    /// like a complete one.
    #[test]
    fn every_computed_return_carries_the_beginning_capital_caveat() {
        let mut store = books();
        link_all(&mut store);
        let capital = run(&store, 1_000);
        assert!(capital
            .warnings()
            .iter()
            .any(|w| w == BEGINNING_CAPITAL_CAVEAT));
    }

    /// A void is a reversal the books have already made. Counting it would show a
    /// partner having taken money the ledger says they gave back.
    #[test]
    fn a_voided_entry_does_not_move_a_capital_account() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "d1",
            day(YEAR, 5, 1),
            &[("zak-out", 500_000), ("cash", -500_000)],
        );
        let e = Event::JournalEntryVoided {
            entry_id: "d1".into(),
            reason: "posted twice".into(),
        };
        let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
        store.apply_projection(&stored).unwrap();

        assert_eq!(run(&store, 0).for_partner("zak").unwrap().withdrawals, 0);
    }

    /// A role neither word cannot be placed on either side of item L, so it is
    /// dropped — and counted, because a link that silently does nothing is a
    /// partner whose draws simply do not appear.
    #[test]
    fn a_link_with_an_unreadable_role_is_dropped_and_counted() {
        let mut store = books();
        link_all(&mut store);
        // Validation refuses this on the way in, so only a hand-run UPDATE gets
        // here — which is exactly the case worth surviving.
        store
            .connection()
            .execute(
                "UPDATE partner_equity_accounts SET role = 'withdrawal' WHERE account_id = 'zak-out'",
                [],
            )
            .unwrap();
        post(
            &mut store,
            "d1",
            day(YEAR, 5, 1),
            &[("zak-out", 500_000), ("cash", -500_000)],
        );

        let capital = run(&store, 0);
        assert_eq!(capital.unreadable_links, 1);
        assert_eq!(capital.for_partner("zak").unwrap().withdrawals, 0);
        let warnings = capital.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("ignored")),
            "{warnings:?}"
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }

    /// Item L row 3 is a distributive share, so a loss travels on the loss
    /// percentages. A partnership that puts the losses on one partner and the
    /// profits on the other is where this is visible at all.
    #[test]
    fn the_income_row_travels_on_the_loss_share_when_the_year_lost_money() {
        let store = books();
        let mut ps = partners(store.connection());
        // Jinny carries the losses; Zak takes the profits.
        ps[0].shares = Shares::from_percents(90.0, 10.0, 50.0);
        ps[1].shares = Shares::from_percents(10.0, 90.0, 50.0);
        let refs: Vec<&Partner> = ps.iter().collect();

        let profit = compute(store.connection(), YEAR, &refs, 1_000).unwrap();
        assert_eq!(profit.accounts[0].net_income, 900, "Zak's profit share");

        let loss = compute(store.connection(), YEAR, &refs, -1_000).unwrap();
        assert_eq!(loss.accounts[0].net_income, -100, "Zak's loss share");
        assert_eq!(loss.accounts[1].net_income, -900);
    }

    /// A draw account in net credit puts a negative in a box whose parentheses
    /// are printed on the paper, where it reads as its own opposite.
    #[test]
    fn a_draw_account_in_net_credit_is_reported() {
        let mut store = books();
        link_all(&mut store);
        post(
            &mut store,
            "x1",
            day(YEAR, 5, 1),
            &[("cash", 250_000), ("zak-out", -250_000)],
        );

        let capital = run(&store, 0);
        let zak = capital.for_partner("zak").unwrap();
        assert_eq!(zak.withdrawals, -2_500);
        let warnings = zak.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("parentheses")),
            "{warnings:?}"
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }

    /// The rows are rounded before they are added, so the six figures on the
    /// paper are the six figures that foot. Rounding a sum of cents instead
    /// produces a column that is a dollar out and cannot be checked by eye.
    #[test]
    fn the_column_foots_as_printed_rather_than_as_computed() {
        let mut store = books();
        link_all(&mut store);
        // Two halves of a cent each way: 100.50 in, 0.50 out.
        post(
            &mut store,
            "c1",
            day(YEAR, 1, 2),
            &[("cash", 10_050), ("zak-in", -10_050)],
        );
        post(
            &mut store,
            "d1",
            day(YEAR, 1, 3),
            &[("zak-out", 50), ("cash", -50)],
        );

        let capital = run(&store, 0);
        let zak = capital.for_partner("zak").unwrap();
        assert_eq!(zak.contributed, 101, "100.50 rounds up");
        assert_eq!(zak.withdrawals, 1, "0.50 rounds up");
        assert_eq!(
            zak.ending(),
            zak.beginning + zak.contributed + zak.net_income + zak.other - zak.withdrawals
        );
    }

    /// The reader is the only route from the table to the return, so a row it
    /// cannot read is a link that does not exist.
    #[test]
    fn the_reader_returns_the_links_that_were_made() {
        let mut store = books();
        link_all(&mut store);
        let links = load_partner_equity_accounts(store.connection());
        assert_eq!(links.len(), 4);
        assert!(links
            .iter()
            .any(|l| l.partner_id == "zak" && l.account_id == "zak-out" && l.role == Role::Draw));
    }

    /// A K-1 is filed for the partners on the return, so item L has to be split
    /// over the same set. Allocating over a partner whose K-1 is not in the
    /// bundle hands them dollars nobody sees and leaves item L disagreeing with
    /// Part III on the page beside it.
    #[test]
    fn a_partner_who_left_before_the_year_is_not_allocated_anything() {
        let mut store = books();
        link_all(&mut store);
        let e = Event::PartnerWithdrawn {
            partner_id: "jinny".into(),
            end_date: day(2023, 6, 30),
        };
        let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
        store.apply_projection(&stored).unwrap();

        // Zak takes the whole interest when Jinny leaves, so the year's income
        // has one place to go and the assertion is arithmetic rather than a
        // rounding artefact.
        let filings: Vec<crate::tax::form1065::PartnerFiling> = partners(store.connection())
            .into_iter()
            .map(|mut partner| {
                if partner.partner_id == "zak" {
                    partner.shares = Shares::from_percents(100.0, 100.0, 100.0);
                }
                crate::tax::form1065::PartnerFiling { partner, tin: None }
            })
            .collect();
        let capital = for_return(store.connection(), YEAR, &filings, 10_000);

        assert_eq!(capital.accounts.len(), 1, "only the partner still in");
        assert_eq!(
            capital.for_partner("zak").unwrap().net_income,
            10_000,
            "the whole year, not a share of it split with somebody who left"
        );
        assert!(capital.for_partner("jinny").is_none());
    }

    /// A ledger this cannot read blanks item L and says so. Refusing to build the
    /// return would leave somebody with no form at all over a table they may not
    /// even use.
    #[test]
    fn a_read_that_fails_blanks_item_l_rather_than_failing_the_return() {
        let store = books();
        store
            .connection()
            .execute("DROP TABLE partner_equity_accounts", [])
            .unwrap();

        let filings: Vec<crate::tax::form1065::PartnerFiling> = partners(store.connection())
            .into_iter()
            .map(|partner| crate::tax::form1065::PartnerFiling { partner, tin: None })
            .collect();
        let capital = for_return(store.connection(), YEAR, &filings, 10_000);

        assert!(capital.is_empty());
        let warnings = capital.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("Item L is blank")),
            "{warnings:?}"
        );
        crate::tax::warning_shape::assert_all(&warnings);
    }
}
