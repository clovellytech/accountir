//! One-off recovery for the bbb (group) book: post the 9 checking→credit-card
//! payments that were recognised as transfers but never posted.
//!
//! Background: on a hosted book, "Import All" only posted the unmatched staged
//! transactions (each against Uncategorized) and silently skipped the matched
//! transfer pairs — so neither leg of any credit-card payment ever posted, and
//! both Business Checking and Business Credit Card (2001) balances stayed
//! overstated. The staged pairs survive only in the desktop replica backup; the
//! nine below are transcribed from it (all confidence 1.0, exact-amount,
//! same-day). Each posts as ONE balanced transfer, identical to what
//! `import_transfer` would have created: credit Business Checking, debit
//! Business Credit Card, source Plaid, reference `transfer:<from>:<to>` so a
//! later real import of the same pair is a no-op.
//!
//! Usage: cargo run --example recover_group_transfers -- /path/to/bbb.db [--apply]
//! Without --apply it is a dry run (prints the plan and balances, writes nothing).
//! The desktop app hosts this DB — close it before running with --apply.

use accountir::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use accountir::commands::recurring_transfers as rt;
use accountir::events::types::JournalEntrySource;
use accountir::queries::account_queries::AccountQueries;
use accountir::store::event_store::EventStore;
use chrono::NaiveDate;

const CHECKING: &str = "d8944fa3-4ea4-4177-a185-76b60dea976f"; // 1001 Business Checking
const CARD: &str = "1d50cb7c-1ab1-4824-8462-c0601cf06285"; // 2001 Business Credit Card (CORP 3846)
const EMPLOYEE_CARD: &str = "b0c9fade-fdcc-454c-84b5-aaf8870437ee"; // 2002 Zak card (employee)

/// (from_ref, to_ref, abs_cents, date, label) — from = checking leg, to = card leg.
const PAYMENTS: &[(&str, &str, i64, &str, &str)] = &[
    (
        "Mmopgn740puoVEgpLeRgu4DNkXeKAQHQqO4Vz",
        "KZJYg0bjAYIo1PJYw83Ju7e3o1Z5KRiZ5AMER",
        745134,
        "2025-12-05",
        "Online Scheduled payment to CRD 3846",
    ),
    (
        "k8zOkAmEJOtOJZEwKV8EsyVd1B7qmvu4d0L16",
        "3MjXaB64oXtD9rvxLXwvFLJ5KVwk7eteJQ6ov",
        344950,
        "2026-01-12",
        "Online Banking payment to CRD 3846",
    ),
    (
        "x6obD7zMybI9jORwgr3RcVdkJbQ50jCLgyNgo",
        "4d4Xx3LaOXUg7xPwrpDPuqDeoVmpBdU0r8yRo",
        631029,
        "2026-03-06",
        "Online Scheduled payment to CRD 3846",
    ),
    (
        "7L5X6kP8QXUNO5bng8XbsrnV95RdZJFORE7p0",
        "r9Aba8zMDbizoZp03YLptkAKpd57zRCJ7exVD",
        804153,
        "2026-04-07",
        "Online Banking payment to CRD 3846",
    ),
    (
        "wwZbL8zM3bs7PO5RoVB5u5OAjJ9DpdcZqgmw9",
        "09yXj170zXi7b1gNRXDguJNxXAnzwjUMXo0Az",
        2183807,
        "2026-05-04",
        "Online Banking payment to CRD 3846",
    ),
    (
        "obqNjOzYwNtYeBpoXrMpTVj0kLegpqCaMyYp8",
        "AAwXk1jPLXH3y7b1aADbud1gVoMpabUkJQRMK",
        1588662,
        "2026-06-01",
        "Online Banking payment to CRD 3846",
    ),
    (
        "RVQXgnNZxXtk0dgXBNogSg9OQkwZ5Au85oJRY",
        "Z1BOgnNEDOty3g8ZjV18sDEPn04gMZfQEjNbN",
        2000000,
        "2026-06-18",
        "Online Banking payment to CRD 3846",
    ),
    (
        "obqNjOzYwNtYeBpoXrMpTVjzq4E9zPC0gKPowy",
        "pg3br8zM6bfKzapY87wpc5a4AJDq0xuLRyjZvj",
        3300000,
        "2026-07-14",
        "Bank of America Business Card Bill Payment",
    ),
    (
        "x6obD7zMybI9jORwgr3RcVd6jOZE6LCpqBvJdK",
        "Q9EpgXN8apiNDag8qEpgs3rRP40bomhoVLjzNe",
        1752000,
        "2026-07-15",
        "Bank of America Business Card Bill Payment",
    ),
];

fn bal(store: &EventStore, id: &str) -> i64 {
    AccountQueries::new(store.connection())
        .get_account_balance(id, None)
        .map(|b| b.balance)
        .unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args
        .get(1)
        .expect("usage: recover_group_transfers <db_path> [--apply]");
    let apply = args.iter().any(|a| a == "--apply");

    let mut store = EventStore::open(db_path)?;

    let total: i64 = PAYMENTS.iter().map(|p| p.2).sum();
    println!("DB: {}", db_path);
    let show = |store: &EventStore, when: &str| {
        println!(
            "{:7}  checking {:>12.2}   parent-2001 {:>12.2}   employee-2002 {:>12.2}",
            when,
            bal(store, CHECKING) as f64 / 100.0,
            bal(store, CARD) as f64 / 100.0,
            bal(store, EMPLOYEE_CARD) as f64 / 100.0,
        );
    };
    show(&store, "before:");
    println!(
        "mode:    {}\n",
        if apply { "APPLY (writing changes)" } else { "DRY RUN (no changes)" }
    );

    // --- Step 1: backfill the monthly employee→parent consolidation shifts, via
    // the same recurring-transfer runner the UI uses. Full-balance zeroes 2002
    // into 2001 each month; oldest-first moves only that month's incremental.
    let start = rt::earliest_activity_month(store.connection(), EMPLOYEE_CARD)
        .unwrap_or_else(|| "2025-01".to_string());
    let rule = rt::RecurringTransferRule {
        id: "bbb-employee-card".to_string(),
        source_account_id: EMPLOYEE_CARD.to_string(),
        dest_account_id: CARD.to_string(),
        day_of_month: 22,
        amount_mode: rt::AmountMode::FullBalance,
        fixed_amount_cents: None,
        memo: "Shift employee card (Zak) balance to parent (CRD 3846)".to_string(),
        start_month: start.clone(),
        active: true,
    };
    let today = chrono::Local::now().date_naive();
    let shifts = rt::due_transfers(store.connection(), &rule, today);
    let shift_total: i64 = shifts.iter().map(|s| s.magnitude_cents()).sum();
    println!(
        "Step 1 — consolidation shifts 2002→2001 (day 22, from {}): {} months, total {:.2}",
        start,
        shifts.len(),
        shift_total as f64 / 100.0
    );
    let (mut s_posted, mut s_skipped) = (0usize, 0usize);
    for p in &shifts {
        println!(
            "  - {} {:>12.2}  {}",
            p.date,
            p.magnitude_cents() as f64 / 100.0,
            p.period
        );
        if !apply {
            continue;
        }
        match rt::post_proposed(&mut store, "correction", p) {
            Ok(_) => s_posted += 1,
            Err(e) => {
                s_skipped += 1;
                println!("      skipped: {}", e);
            }
        }
    }
    if apply {
        show(&store, "mid:");
    }

    // --- Step 2: the 9 checking→parent card payments (recognised as transfers
    // but never posted).
    println!(
        "\nStep 2 — card payments checking→2001: {} transfers, total {:.2}",
        PAYMENTS.len(),
        total as f64 / 100.0
    );
    let (mut posted, mut skipped) = (0usize, 0usize);
    for (from_ref, to_ref, abs, date, label) in PAYMENTS {
        let reference = format!("transfer:{}:{}", from_ref, to_ref);
        println!(
            "- {} {:>12.2}  {}   ref {}",
            date,
            *abs as f64 / 100.0,
            label,
            reference
        );
        if !apply {
            continue;
        }
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
        let mut cmds = EntryCommands::new(&mut store, "correction".to_string());
        match cmds.post_entry(PostEntryCommand {
            date,
            memo: format!("Transfer: {}", label),
            lines: vec![
                EntryLine::credit(CHECKING, *abs, "USD"),
                EntryLine::debit(CARD, *abs, "USD"),
            ],
            reference: Some(reference.clone()),
            source: Some(JournalEntrySource::Plaid),
        }) {
            Ok(_) => posted += 1,
            // Most likely an already-present reference: safe to skip so the tool
            // is re-runnable.
            Err(e) => {
                skipped += 1;
                println!("    skipped: {}", e);
            }
        }
    }

    println!();
    show(&store, "after:");
    if apply {
        println!(
            "shifts posted {}/skipped {} · payments posted {}/skipped {}",
            s_posted, s_skipped, posted, skipped
        );
    } else {
        println!("(dry run — re-run with --apply to write; close the desktop app first)");
    }
    Ok(())
}
