//! One-off: take Bunny Ears' 2024 sales tax collected in error out of the sales
//! tax liability, as the filed 2024 return did.
//!
//! Square's monthly summaries report $574.36 of tax collected from May to
//! December 2024. The tax actually owed Illinois was $191 — September $59,
//! October $72, November $34, December $26, which are exactly the four
//! Department of Revenue payments — and the other $383.36 (May to September) was
//! tax charged in error. GnuCash booked it as income, and the filed 2024 return
//! reported it in gross receipts; accountir's 2024 books had it as owed.
//!
//! This reopens 2024, posts a 2024-12-31 entry moving $383.36 from 5007 Sales tax
//! to 2008 Sales tax collected in error (under Gross Sales, so line 1a), and
//! closes 2024 again through Equity:Years:2024 to the partners. The re-close
//! follows 2024's fixed allocation, so Lois still takes $1,843.56 and the extra
//! income is Jinny's. Idempotent.
//!
//! Usage: cargo run --example sales_tax_errors_2024 -- /path/to/db

use accountir::commands::account_commands::{AccountCommands, CreateAccountCommand};
use accountir::commands::closing_commands::{self as cc, CloseBooksCommand, ClosingTarget};
use accountir::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use accountir::domain::AccountType;
use accountir::events::types::JournalEntrySource;
use accountir::store::event_store::EventStore;
use chrono::NaiveDate;

const REFERENCE: &str = "adj-2024-sales-tax-errors";
const YEARS_2024: &str = "06a4a555-f2f6-4a33-b85f-945ebb01c18b";

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

fn posted(store: &EventStore) -> bool {
    store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM journal_entries WHERE reference = ?1 AND is_void = 0",
            [REFERENCE],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

fn main() {
    let db = std::env::args().nth(1).expect("usage: sales_tax_errors_2024 <db>");
    let mut store = EventStore::open(&db).unwrap();
    accountir::store::migrations::run_migrations(store.connection()).unwrap();

    if posted(&store) {
        println!("already corrected");
        return;
    }

    match cc::reopen_books(
        &mut store,
        "cli-user",
        2024,
        "Sales tax collected in error in 2024 ($383.36) was income on the filed return, not a \
         liability",
    ) {
        Ok(()) => println!("reopened 2024"),
        Err(e) => println!("2024 not reopened: {e}"),
    }

    let error_account = match id_of(&store, "2008") {
        Some(id) => id,
        None => {
            let gross_sales = id_of(&store, "2001");
            AccountCommands::new(&mut store, "cli-user".to_string())
                .create_account(CreateAccountCommand {
                    account_type: AccountType::Revenue,
                    account_number: "2008".to_string(),
                    name: "Sales tax collected in error".to_string(),
                    parent_id: gross_sales,
                    currency: Some("USD".to_string()),
                    description: Some(
                        "Tax charged at the register on sales that owed none, kept rather than \
                         refunded: income, in gross receipts"
                            .to_string(),
                    ),
                })
                .unwrap();
            println!("created 2008 Sales tax collected in error");
            id_of(&store, "2008").unwrap()
        }
    };

    let sales_tax = id_of(&store, "5007").expect("5007 Sales tax");
    EntryCommands::new(&mut store, "cli-user".to_string())
        .post_entry(PostEntryCommand {
            date: NaiveDate::from_ymd_opt(2024, 12, 31).unwrap(),
            memo: "2024 sales tax collected in error (May–Sep, $383.36): income, as GnuCash and \
                   the filed 2024 return have it; the $191 owed Illinois stays in Sales tax"
                .to_string(),
            lines: vec![
                EntryLine::debit(&sales_tax, 38_336, "USD"),
                EntryLine::credit(&error_account, 38_336, "USD"),
            ],
            reference: Some(REFERENCE.to_string()),
            source: Some(JournalEntrySource::Manual),
        })
        .unwrap();
    println!("posted the 2024-12-31 correction");

    let target = ClosingTarget::PartnerCapital(YEARS_2024.to_string());
    let p = cc::preview(store.connection(), 2024, false, &target).unwrap();
    println!(
        "2024 net income {:.2}, trial balance ok: {}",
        p.net_income_cents as f64 / 100.0,
        p.trial_balance_ok
    );
    for share in &p.allocation {
        println!(
            "  {:20} {:>10.2} -> {}",
            share.partner_name,
            share.cents as f64 / 100.0,
            share.account_label
        );
    }
    cc::close_books(
        &mut store,
        "cli-user",
        CloseBooksCommand {
            year: 2024,
            target,
            include_draws: false,
        },
    )
    .unwrap();
    println!("closed 2024 again");
}
