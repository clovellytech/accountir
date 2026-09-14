//! One-off: put Bunny Ears' Illinois Department of Revenue debits where they
//! belong.
//!
//! The bank feed's "Illinois Department of Revenue" debits were categorised to the
//! payroll tax liabilities, but Square pays the payroll taxes itself — every
//! quarter's Square and IRS pulls match the accrued payroll taxes to the cent. The
//! IDOR debits are the sales tax returns (5007 Sales tax, which had collected tax
//! from Square since mid-2024 with nothing paid out of it) and the 2024
//! replacement tax.
//!
//! - The 2024-11-12 payments ($59 + $72) sit in 2024, which is closed; their
//!   $131 was carried into 5015 Payroll clearing on 2025-01-01, so it is moved to
//!   5007 on the same day.
//! - The 2025-01-14 ($26, $34) and 2026-01-20 ($412) payments are reassigned to
//!   5007.
//! - The 2025-03-20 replacement tax payment ($639) is reassigned straight to 3061
//!   Illinois replacement tax, and the reclassification entry that had moved it
//!   there is voided.
//!
//! Idempotent: each step checks whether it is already done.
//!
//! Usage: cargo run --example sales_tax_payments_2025 -- /path/to/db

use accountir::commands::entry_commands::{
    EntryCommands, EntryLine, PostEntryCommand, ReassignLineCommand, VoidEntryCommand,
};
use accountir::events::types::JournalEntrySource;
use accountir::store::event_store::EventStore;
use chrono::NaiveDate;

const CARRY_REF: &str = "adj-2025-sales-tax-2024-payment";
const RECLASS_ENTRY: &str = "a1104026-ece0-4b55-8cb8-a90242800eb9";

fn id_of(store: &EventStore, number: &str) -> String {
    store
        .connection()
        .query_row(
            "SELECT id FROM accounts WHERE account_number = ?1",
            [number],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| panic!("no account {number}"))
}

fn line_account(store: &EventStore, line_id: &str) -> String {
    store
        .connection()
        .query_row(
            "SELECT account_id FROM journal_lines WHERE id = ?1",
            [line_id],
            |r| r.get(0),
        )
        .unwrap()
}

fn main() {
    let db = std::env::args().nth(1).expect("usage: sales_tax_payments_2025 <db>");
    let mut store = EventStore::open(&db).unwrap();
    let (sales_tax, clearing, replacement) =
        (id_of(&store, "5007"), id_of(&store, "5015"), id_of(&store, "3061"));

    let carried: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM journal_entries WHERE reference = ?1 AND is_void = 0",
            [CARRY_REF],
            |r| r.get(0),
        )
        .unwrap();
    if carried == 0 {
        EntryCommands::new(&mut store, "cli-user".to_string())
            .post_entry(PostEntryCommand {
                date: NaiveDate::from_ymd_opt(2025, 1, 1).unwrap(),
                memo: "Illinois sales tax paid 2024-11-12 ($59 + $72), recorded against the \
                       payroll tax liabilities in 2024: carried from 5015 Payroll clearing to \
                       5007 Sales tax"
                    .to_string(),
                lines: vec![
                    EntryLine::debit(&sales_tax, 13_100, "USD"),
                    EntryLine::credit(&clearing, 13_100, "USD"),
                ],
                reference: Some(CARRY_REF.to_string()),
                source: Some(JournalEntrySource::Manual),
            })
            .unwrap();
        println!("carried the 2024 sales tax payments ($131.00) to 5007");
    }

    for (entry, target, what) in [
        ("0d394bcd-6ded-4da4-aba8-7eeca5789e3c", &sales_tax, "2025-01-14 $26.00 → 5007"),
        ("42b97c63-8a61-4a98-8405-55221e6820f5", &sales_tax, "2025-01-14 $34.00 → 5007"),
        ("c996ddcf-a441-4a0f-9b56-3a0a72155525", &sales_tax, "2026-01-20 $412.00 → 5007"),
        ("2a7c05b0-985e-4022-b137-ee4b701ae7fd", &replacement, "2025-03-20 $639.00 → 3061"),
    ] {
        let line = format!("{entry}-line-2");
        if line_account(&store, &line) == *target {
            continue;
        }
        EntryCommands::new(&mut store, "cli-user".to_string())
            .reassign_line(ReassignLineCommand {
                entry_id: entry.to_string(),
                line_id: line,
                new_account_id: target.clone(),
            })
            .unwrap();
        println!("reassigned {what}");
    }

    let voided: bool = store
        .connection()
        .query_row(
            "SELECT is_void FROM journal_entries WHERE id = ?1",
            [RECLASS_ENTRY],
            |r| r.get(0),
        )
        .unwrap();
    if !voided {
        EntryCommands::new(&mut store, "cli-user".to_string())
            .void_entry(VoidEntryCommand {
                entry_id: RECLASS_ENTRY.to_string(),
                reason: "The 2025-03-20 payment line now posts to 3061 directly".to_string(),
            })
            .unwrap();
        println!("voided the replacement tax reclassification entry");
    }
}
