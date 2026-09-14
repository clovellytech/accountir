//! One-off: clear all of Bunny Ears' payroll cash through one account from 2025.
//!
//! The Square payroll entries credited withheld and employer taxes to `5008
//! Employer` and `5009 State`, and the checking withdrawals that pay them — Square
//! "PAYR TAX" pulls and the IRS debits — were categorised to one or the other by
//! hand. The split never matched (Illinois unemployment was accrued as federal and
//! paid as state), so both accounts drifted while together they nearly netted.
//!
//! This creates `5015 Payroll clearing` under `5006 Taxes`, moves the two
//! accounts' 2024 closing balances onto it on 2025-01-01 (2024 is closed and is
//! left as filed), reassigns every 2025-and-later line on them to it, points the
//! Square payroll import's `payroll_taxes_payable` mapping at it, and deactivates
//! the two old accounts. Idempotent: each step checks whether it is already done.
//!
//! Usage: cargo run --example payroll_clearing_2025 -- /path/to/db

use accountir::commands::account_commands::{
    AccountCommands, CreateAccountCommand, DeactivateAccountCommand,
};
use accountir::commands::entry_commands::{
    EntryCommands, EntryLine, PostEntryCommand, ReassignLineCommand,
};
use accountir::commands::ingest_commands::set_account_mapping;
use accountir::domain::AccountType;
use accountir::events::types::JournalEntrySource;
use accountir::store::event_store::EventStore;
use chrono::NaiveDate;

const OPENING_REF: &str = "adj-2025-payroll-clearing-opening";

fn id_of(store: &EventStore, number: &str) -> Option<String> {
    store
        .connection()
        .query_row(
            "SELECT id FROM accounts WHERE account_number = ?1",
            [number],
            |r| r.get(0),
        )
        .ok()
}

fn balance(store: &EventStore, account_id: &str, through: Option<&str>) -> i64 {
    store
        .connection()
        .query_row(
            "SELECT COALESCE(SUM(l.amount), 0) FROM journal_lines l
               JOIN journal_entries e ON e.id = l.entry_id
              WHERE l.account_id = ?1 AND e.is_void = 0 AND (?2 IS NULL OR e.date <= ?2)",
            rusqlite::params![account_id, through],
            |r| r.get(0),
        )
        .unwrap()
}

fn main() {
    let db = std::env::args().nth(1).expect("usage: payroll_clearing_2025 <db>");
    let mut store = EventStore::open(&db).unwrap();
    accountir::store::migrations::run_migrations(store.connection()).unwrap();

    let taxes = id_of(&store, "5006").expect("5006 Taxes");
    let federal = id_of(&store, "5008").expect("5008 Employer");
    let state = id_of(&store, "5009").expect("5009 State");

    let clearing = match id_of(&store, "5015") {
        Some(id) => id,
        None => {
            AccountCommands::new(&mut store, "cli-user".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Liability,
                    account_number: "5015".to_string(),
                    name: "Payroll clearing".to_string(),
                    parent_id: Some(taxes),
                    currency: Some("USD".to_string()),
                    description: Some(
                        "Payroll taxes accrued by the Square payroll entries, cleared by every \
                         payroll withdrawal from checking (Square PAYR TAX, IRS)"
                            .to_string(),
                    ),
                })
                .unwrap();
            println!("created 5015 Payroll clearing");
            id_of(&store, "5015").unwrap()
        }
    };

    let opened: bool = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM journal_entries WHERE reference = ?1 AND is_void = 0",
            [OPENING_REF],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0;
    if !opened {
        let mut lines = Vec::new();
        let mut net = 0i64;
        for (account, number) in [(&federal, "5008"), (&state, "5009")] {
            let b = balance(&store, account, Some("2024-12-31"));
            net += b;
            println!("{number} at 2024-12-31: {:.2}", b as f64 / 100.0);
            if b < 0 {
                lines.push(EntryLine::debit(account, -b, "USD"));
            } else if b > 0 {
                lines.push(EntryLine::credit(account, b, "USD"));
            }
        }
        if net > 0 {
            lines.push(EntryLine::debit(&clearing, net, "USD"));
        } else if net < 0 {
            lines.push(EntryLine::credit(&clearing, -net, "USD"));
        }
        if lines.len() >= 2 {
            EntryCommands::new(&mut store, "cli-user".to_string())
                .post_entry(PostEntryCommand {
                    date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
                    memo: "Payroll tax liabilities (5008 Employer, 5009 State) carried into 5015 \
                           Payroll clearing"
                        .to_string(),
                    lines,
                    reference: Some(OPENING_REF.to_string()),
                    source: Some(JournalEntrySource::Manual),
                })
                .unwrap();
            println!("posted the 2025-01-01 opening move ({:.2} net)", net as f64 / 100.0);
        }
    }

    let to_move: Vec<(String, String)> = {
        let mut stmt = store
            .connection()
            .prepare(
                "SELECT l.entry_id, l.id FROM journal_lines l
                   JOIN journal_entries e ON e.id = l.entry_id
                  WHERE l.account_id IN (?1, ?2) AND e.is_void = 0 AND e.date >= '2025-01-01'
                    AND COALESCE(e.reference, '') != ?3",
            )
            .unwrap();
        let rows = stmt
            .query_map(rusqlite::params![federal, state, OPENING_REF], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap()
            .flatten()
            .collect();
        rows
    };
    for (entry_id, line_id) in &to_move {
        EntryCommands::new(&mut store, "cli-user".to_string())
            .reassign_line(ReassignLineCommand {
                entry_id: entry_id.clone(),
                line_id: line_id.clone(),
                new_account_id: clearing.clone(),
            })
            .unwrap();
    }
    println!("reassigned {} line(s) to 5015", to_move.len());

    set_account_mapping(store.connection(), "payroll_taxes_payable", &clearing).unwrap();
    println!("Square payroll import now credits 5015 for taxes payable");

    for (account, number) in [(&federal, "5008"), (&state, "5009")] {
        let b = balance(&store, account, None);
        match AccountCommands::new(&mut store, "cli-user".to_string()).deactivate_account(
            DeactivateAccountCommand {
                account_id: account.clone(),
                reason: Some("Replaced by 5015 Payroll clearing from 2025".to_string()),
            },
        ) {
            Ok(_) => println!("deactivated {number} (balance {:.2})", b as f64 / 100.0),
            Err(e) => println!("{number} left active (balance {:.2}): {e}", b as f64 / 100.0),
        }
    }

    for through in ["2025-12-31", "9999-12-31"] {
        println!(
            "5015 balance through {through}: {:.2}",
            balance(&store, &clearing, Some(through)) as f64 / 100.0
        );
    }
}
