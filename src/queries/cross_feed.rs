//! Pairing a brokerage's cash movement with the bank transaction that is its other
//! half.
//!
//! # The seam this closes
//!
//! A transfer between a bank and a brokerage is one movement of money reported twice,
//! by two providers, into two different pipelines. The bank feed sees a withdrawal and
//! stages it; the investments pull sees a deposit and — with no clearing account
//! configured — holds it, because the brokerage does not say which bank account the
//! money came from and there is nowhere truthful to put the other leg.
//!
//! Before investment accounts were split out of the bank feed, the feed's own transfer
//! detection could pair the two: both legs were staged rows in one table. Splitting the
//! feeds (INVESTMENTS-SPEC.md §6) was right — a purchase in the transactions feed reads
//! as money spent — but it left this pairing with nothing to do it. The two halves now
//! live in different places and nothing looks across.
//!
//! # What a match is
//!
//! The ledger, not the staging table, is the bank side. By the time somebody is looking
//! at a held brokerage row the bank leg is usually already posted — to the landing
//! account, because nothing categorises it — and the ledger is where it can be found
//! whatever route it arrived by.
//!
//! The arithmetic is an equality rather than a tolerance. The provider's sign for an
//! investment cash transaction is "positive means cash left the brokerage", and the
//! bank's ledger line for the same movement carries the same signed figure: money
//! leaving the bank for the brokerage is a credit to the bank (negative) and the
//! brokerage reports cash arriving (negative). So the bank line's amount **equals** the
//! held row's, and a match that needed a sign rule would be a match that could pair a
//! withdrawal with a deposit.
//!
//! Dates are not equal, and cannot be: a transfer settles over days, and the two
//! providers date it differently. Hence a window, and the distance is reported so a
//! five-day-old match can be weighed against a same-day one.
//!
//! # What pairing does, and what it does not
//!
//! Nothing here writes. It finds candidates and says which line could be re-pointed;
//! deciding is a person's, because two $5,000 transfers in one week are
//! indistinguishable to any rule.

use chrono::NaiveDate;
use rusqlite::{Connection, OptionalExtension};

/// How far apart the two providers may date the same movement.
///
/// A transfer between institutions settles in one to three business days, and a
/// weekend stretches that to five. Wider starts pairing a transfer with the next one.
pub const DEFAULT_WINDOW_DAYS: i64 = 5;

/// A posted bank entry that could be the other half of a held brokerage movement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankLeg {
    pub entry_id: String,
    /// The bank's own line: which account, and what it says.
    pub bank_account_id: String,
    pub bank_account_name: String,
    pub date: NaiveDate,
    pub memo: String,
    /// Signed as the ledger holds it, and equal to the held row's own figure.
    pub amount_cents: i64,
    /// How many days apart the two providers dated it. Zero is same-day.
    pub days_apart: i64,
    /// The line sitting in the landing account, when the entry has one — the line
    /// that should point at the brokerage's cash account instead.
    ///
    /// `None` means the entry is already categorised. Pairing then changes nothing in
    /// the books and only records that the held row is accounted for, which is the
    /// honest outcome: somebody has already said where that money went, and a pairing
    /// that silently overwrote it would be a worse answer than none.
    pub landing_line_id: Option<String>,
}

impl BankLeg {
    /// Whether pairing would re-point a line, as against merely recording the link.
    pub fn would_recategorise(&self) -> bool {
        self.landing_line_id.is_some()
    }
}

/// Bank entries that could be the other half of this movement, nearest in date first.
///
/// `amount_cents` is the held row's own figure, unchanged — see the module docs for why
/// no sign is flipped. An entry already named by a resolved brokerage row is left out:
/// it has been paired, and offering it again is how one bank transaction comes to
/// account for two brokerage movements.
pub fn bank_legs_for(
    conn: &Connection,
    amount_cents: i64,
    on: NaiveDate,
    window_days: i64,
) -> Vec<BankLeg> {
    if amount_cents == 0 || !has_table(conn, "plaid_local_accounts") {
        return Vec::new();
    }
    let from = on - chrono::Duration::days(window_days);
    let to = on + chrono::Duration::days(window_days);
    // The bank line and the entry it is in. Restricted to accounts the feed maps to a
    // chequing, savings or card account: those are the accounts money reaches a
    // brokerage from, and matching against every account in the chart would offer an
    // expense of the same amount as a candidate.
    let Ok(mut stmt) = conn.prepare(
        "SELECT jl.entry_id, jl.account_id, a.name, je.date, je.memo, jl.amount
           FROM journal_lines jl
           JOIN journal_entries je ON je.id = jl.entry_id
           JOIN accounts a ON a.id = jl.account_id
          WHERE je.is_void = 0
            AND jl.amount = ?1
            AND je.date >= ?2 AND je.date <= ?3
            AND EXISTS (
                SELECT 1 FROM plaid_local_accounts pa
                 WHERE pa.local_account_id = jl.account_id
                   AND lower(pa.account_type) IN ('depository', 'credit')
            )
            AND NOT EXISTS (
                SELECT 1 FROM investment_staged_activity s
                 WHERE s.resolution_entry_id = jl.entry_id
            )
          ORDER BY je.date",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(
        rusqlite::params![amount_cents, from.to_string(), to.to_string()],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        },
    );
    let Ok(rows) = rows else { return Vec::new() };
    let mut out: Vec<BankLeg> = rows
        .flatten()
        .filter_map(
            |(entry_id, bank_account_id, bank_account_name, date, memo, amount)| {
                let date = NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?;
                Some(BankLeg {
                    landing_line_id: landing_line(conn, &entry_id),
                    entry_id,
                    bank_account_id,
                    bank_account_name,
                    days_apart: (date - on).num_days().abs(),
                    date,
                    memo: memo.unwrap_or_default(),
                    amount_cents: amount,
                })
            },
        )
        .collect();
    // Nearest first, and the date as the tie-break so the order does not depend on how
    // SQLite happened to read the rows.
    out.sort_by_key(|leg| (leg.days_apart, leg.date, leg.entry_id.clone()));
    out
}

/// The line of this entry that sits in the landing account imports use, if any.
///
/// By the account's name under the chart, which is how `find_uncategorized` resolves
/// it, and only when the entry has exactly one such line: an entry with two is one
/// nobody should be re-pointing a single line of from here.
fn landing_line(conn: &Connection, entry_id: &str) -> Option<String> {
    let mut stmt = conn
        .prepare(
            "SELECT jl.id
               FROM journal_lines jl
               JOIN accounts a ON a.id = jl.account_id
              WHERE jl.entry_id = ?1 AND a.name = 'Uncategorized'",
        )
        .ok()?;
    let ids: Vec<String> = stmt
        .query_map([entry_id], |row| row.get::<_, String>(0))
        .ok()?
        .filter_map(|r| r.ok())
        .collect();
    match ids.len() {
        1 => ids.into_iter().next(),
        _ => None,
    }
}

fn has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(true),
    )
    .optional()
    .ok()
    .flatten()
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::account_commands::{
        create_default_accounts, AccountCommands, CreateAccountCommand,
    };
    use crate::domain::AccountType;
    use crate::events::types::{Event, EventEnvelope, JournalLineData};
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;
    use crate::store::projections::ProjectionStore;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    struct Book {
        store: EventStore,
        checking: String,
        landing: String,
        groceries: String,
    }

    fn book() -> Book {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let stored = store
            .append(EventEnvelope::new(
                Event::CompanyCreated {
                    company_id: "c".into(),
                    name: "Books".into(),
                    base_currency: "USD".into(),
                    fiscal_year_start: 1,
                },
                "u".to_string(),
            ))
            .unwrap();
        store.apply_projection(&stored).unwrap();
        create_default_accounts(&mut store).expect("chart");
        let id = |name: &str| -> String {
            store
                .connection()
                .query_row("SELECT id FROM accounts WHERE name = ?1", [name], |r| {
                    r.get(0)
                })
                .unwrap_or_else(|e| panic!("{name}: {e}"))
        };
        let checking = id("Business Checking");
        let landing = id("Uncategorized");
        let groceries = AccountCommands::new(&mut store, "u".into())
            .create_account(CreateAccountCommand {
                account_type: AccountType::Expense,
                account_number: "3100".into(),
                name: "Groceries".into(),
                parent_id: None,
                currency: Some("USD".into()),
                description: None,
            })
            .map(|e| match &e.event {
                Event::AccountCreated { account_id, .. } => account_id.clone(),
                _ => unreachable!(),
            })
            .expect("account");
        store
            .connection()
            .execute(
                "INSERT INTO plaid_items (id, proxy_item_id, institution_name, status)
                 VALUES ('item1', 'p1', 'Bank of America', 'active')",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', 'pa1', 'Checking', 'depository', ?1)",
                [&checking],
            )
            .unwrap();
        Book {
            store,
            checking,
            landing,
            groceries,
        }
    }

    impl Book {
        fn post(&mut self, id: &str, on: NaiveDate, lines: &[(&str, i64)]) {
            let stored = self
                .store
                .append(EventEnvelope::new(
                    Event::JournalEntryPosted {
                        entry_id: id.to_string(),
                        date: on,
                        memo: format!("memo {id}"),
                        lines: lines
                            .iter()
                            .enumerate()
                            .map(|(i, (account, amount))| JournalLineData {
                                line_id: format!("{id}-{i}"),
                                account_id: account.to_string(),
                                amount: *amount,
                                currency: "USD".into(),
                                exchange_rate: None,
                                memo: None,
                            })
                            .collect(),
                        reference: None,
                        source: None,
                    },
                    "u".to_string(),
                ))
                .unwrap();
            self.store.apply_projection(&stored).unwrap();
        }

        fn legs(&self, amount: i64, on: NaiveDate) -> Vec<BankLeg> {
            bank_legs_for(self.store.connection(), amount, on, DEFAULT_WINDOW_DAYS)
        }
    }

    /// $5,000 into the brokerage. The provider reports it as `-500000` — negative is
    /// cash arriving — and the bank's own line for the same movement is `-500000`
    /// too, because money left the bank. Equal, not opposite: see the module docs.
    #[test]
    fn a_deposit_matches_the_withdrawal_that_paid_for_it() {
        let mut book = book();
        let (checking, landing) = (book.checking.clone(), book.landing.clone());
        book.post(
            "bank",
            day(2026, 9, 3),
            &[(&landing, 500_000), (&checking, -500_000)],
        );
        let legs = book.legs(-500_000, day(2026, 9, 4));
        assert_eq!(legs.len(), 1, "{legs:?}");
        assert_eq!(legs[0].entry_id, "bank");
        assert_eq!(legs[0].bank_account_name, "Business Checking");
        assert_eq!(legs[0].days_apart, 1);
        assert_eq!(legs[0].landing_line_id.as_deref(), Some("bank-0"));
        assert!(legs[0].would_recategorise());
    }

    /// The other direction, which a sign rule would get wrong: cash leaving the
    /// brokerage is `+500000`, and the bank line that receives it is `+500000`.
    #[test]
    fn a_withdrawal_matches_the_deposit_it_landed_in() {
        let mut book = book();
        let (checking, landing) = (book.checking.clone(), book.landing.clone());
        book.post(
            "bank",
            day(2026, 9, 3),
            &[(&checking, 500_000), (&landing, -500_000)],
        );
        let legs = book.legs(500_000, day(2026, 9, 3));
        assert_eq!(legs.len(), 1, "{legs:?}");
        assert_eq!(legs[0].days_apart, 0);
    }

    /// A transfer the other way round is not this transfer. Matching on magnitude
    /// alone would offer a $5,000 withdrawal as the other half of a $5,000 deposit.
    #[test]
    fn the_opposite_direction_is_not_a_match() {
        let mut book = book();
        let (checking, landing) = (book.checking.clone(), book.landing.clone());
        book.post(
            "bank",
            day(2026, 9, 3),
            &[(&landing, 500_000), (&checking, -500_000)],
        );
        assert!(book.legs(500_000, day(2026, 9, 3)).is_empty());
    }

    /// Settlement takes days and the two providers date it differently, so the window
    /// is the whole point — and it has an edge that has to hold.
    #[test]
    fn the_window_has_an_edge() {
        let mut book = book();
        let (checking, landing) = (book.checking.clone(), book.landing.clone());
        book.post(
            "inside",
            day(2026, 9, 1),
            &[(&landing, 100), (&checking, -100)],
        );
        book.post(
            "outside",
            day(2026, 8, 20),
            &[(&landing, 100), (&checking, -100)],
        );
        let legs = book.legs(-100, day(2026, 9, 6));
        assert_eq!(
            legs.iter().map(|l| l.entry_id.as_str()).collect::<Vec<_>>(),
            vec!["inside"],
            "five days in, seventeen out"
        );
    }

    /// Nearest in date first, so a same-day match is not buried under a five-day-old
    /// one of the same amount.
    #[test]
    fn the_nearest_match_comes_first() {
        let mut book = book();
        let (checking, landing) = (book.checking.clone(), book.landing.clone());
        book.post(
            "far",
            day(2026, 9, 1),
            &[(&landing, 100), (&checking, -100)],
        );
        book.post(
            "near",
            day(2026, 9, 4),
            &[(&landing, 100), (&checking, -100)],
        );
        let legs = book.legs(-100, day(2026, 9, 5));
        assert_eq!(
            legs.iter().map(|l| l.entry_id.as_str()).collect::<Vec<_>>(),
            vec!["near", "far"]
        );
    }

    /// An entry somebody has already categorised is still a match — it is the money,
    /// and the held row should stop asking about it. But nothing is re-pointed: a
    /// pairing that overwrote a decision already taken would be worse than none.
    #[test]
    fn an_already_categorised_entry_matches_but_changes_nothing() {
        let mut book = book();
        let (checking, groceries) = (book.checking.clone(), book.groceries.clone());
        book.post(
            "bank",
            day(2026, 9, 3),
            &[(&groceries, 500_000), (&checking, -500_000)],
        );
        let legs = book.legs(-500_000, day(2026, 9, 3));
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].landing_line_id, None);
        assert!(!legs[0].would_recategorise());
    }

    /// Only accounts the feed maps to a bank or a card. An expense of the same amount
    /// on the same day is not where money reaches a brokerage from, and offering it
    /// would make the suggestion worthless.
    #[test]
    fn an_account_no_bank_is_mapped_to_is_not_offered() {
        let mut book = book();
        let (groceries, landing) = (book.groceries.clone(), book.landing.clone());
        book.post(
            "cash-purchase",
            day(2026, 9, 3),
            &[(&groceries, 500_000), (&landing, -500_000)],
        );
        assert!(book.legs(-500_000, day(2026, 9, 3)).is_empty());
    }

    /// And not the brokerage's own ledger account either, even though the feed maps
    /// it. The other half of a brokerage movement is at a *bank*; offering the
    /// brokerage's own cash line would pair the movement with itself.
    #[test]
    fn the_brokerage_side_is_not_offered_as_its_own_other_half() {
        let mut book = book();
        let (landing, brokerage) = {
            let brokerage = AccountCommands::new(&mut book.store, "u".into())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Asset,
                    account_number: "1200".into(),
                    name: "Brokerage cash".into(),
                    parent_id: None,
                    currency: Some("USD".into()),
                    description: None,
                })
                .map(|e| match &e.event {
                    Event::AccountCreated { account_id, .. } => account_id.clone(),
                    _ => unreachable!(),
                })
                .expect("account");
            (book.landing.clone(), brokerage)
        };
        book.store
            .connection()
            .execute(
                "INSERT INTO plaid_local_accounts
                    (item_id, plaid_account_id, name, account_type, local_account_id)
                 VALUES ('item1', 'pa2', 'Brokerage', 'investment', ?1)",
                [&brokerage],
            )
            .unwrap();
        book.post(
            "brokerage-side",
            day(2026, 9, 3),
            &[(&brokerage, -500_000), (&landing, 500_000)],
        );
        assert!(
            book.legs(-500_000, day(2026, 9, 3)).is_empty(),
            "an investment account is not where the money came from"
        );
    }

    /// A voided entry moved no money.
    #[test]
    fn a_voided_entry_is_not_a_match() {
        let mut book = book();
        let (checking, landing) = (book.checking.clone(), book.landing.clone());
        book.post(
            "bank",
            day(2026, 9, 3),
            &[(&landing, 100), (&checking, -100)],
        );
        let stored = book
            .store
            .append(EventEnvelope::new(
                Event::JournalEntryVoided {
                    entry_id: "bank".into(),
                    reason: "no".into(),
                },
                "u".to_string(),
            ))
            .unwrap();
        book.store.apply_projection(&stored).unwrap();
        assert!(book.legs(-100, day(2026, 9, 3)).is_empty());
    }

    /// One bank transaction cannot account for two brokerage movements. Once a held
    /// row has been paired to it, it stops being offered.
    #[test]
    fn an_entry_already_paired_is_not_offered_again() {
        let mut book = book();
        let (checking, landing) = (book.checking.clone(), book.landing.clone());
        book.post(
            "bank",
            day(2026, 9, 3),
            &[(&landing, 100), (&checking, -100)],
        );
        assert_eq!(book.legs(-100, day(2026, 9, 3)).len(), 1);
        book.store
            .connection()
            .execute(
                "INSERT INTO investment_staged_activity
                    (id, item_id, plaid_account_id, provider_transaction_id, reason, detail,
                     provider_type, provider_subtype, date, name, amount_cents, raw_payload,
                     status, resolution_entry_id)
                 VALUES ('s1','item1','pa9','tx1','no_clearing_account','d','cash','deposit',
                         '2026-09-03','Transfer',-100,'{}','resolved','bank')",
                [],
            )
            .unwrap();
        assert!(
            book.legs(-100, day(2026, 9, 3)).is_empty(),
            "already accounted for"
        );
    }

    /// Zero never matches. A provider row with no amount would otherwise pair with
    /// every zero-sum line in the window.
    #[test]
    fn nothing_matches_nothing() {
        let book = book();
        assert!(book.legs(0, day(2026, 9, 3)).is_empty());
    }
}
