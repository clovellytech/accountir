//! One-off correction: rebuild 5 mis-signed credit-card payments as transfers.
//!
//! Each payment was originally imported as two separate single entries (card +
//! checking), both offset against Uncategorized, with the card leg wrong-signed
//! (negative, i.e. adding to the debt). This voids both single entries and posts
//! one balanced transfer (checking credit / card debit) in their place.
//!
//! Usage: cargo run --example fix_card_payments -- /path/to/bugbear.db [--apply]
//! Without --apply it runs as a dry run (prints the plan, makes no changes).

use accountir::commands::entry_commands::{
    EntryCommands, EntryLine, PostEntryCommand, VoidEntryCommand,
};
use accountir::events::types::JournalEntrySource;
use accountir::queries::account_queries::AccountQueries;
use accountir::store::event_store::EventStore;
use chrono::NaiveDate;

const CHECKING: &str = "7dfc9308-1770-4925-89cc-b23e1b7da1b0";
const CARD: &str = "1bed0ac9-a562-4717-8b77-8a90f2caad62";
const UNCATEGORIZED: &str = "13d07618"; // prefix only, resolved below for reporting

/// (card_entry_id, checking_entry_id, abs_amount_cents, date, label)
const PAYMENTS: &[(&str, &str, i64, &str, &str)] = &[
    (
        "937cebef-4650-43e5-be2e-c66aa27401e4",
        "3b30456c-b176-42d8-8d9a-58d930b02774",
        745134,
        "2025-12-05",
        "ONLINE SCHEDULED PAYMENT",
    ),
    (
        "0d80246f-44b2-4663-816d-9568e1e08789",
        "fdfb3d63-7158-47c2-9aff-c27e89e075bc",
        344950,
        "2026-01-12",
        "Online payment from CHK",
    ),
    (
        "dd12ae78-d816-4a67-8275-316a9f2a835c",
        "aaa020b9-66d3-4ac9-9228-6a820779c042",
        631029,
        "2026-03-06",
        "ONLINE SCHEDULED PAYMENT",
    ),
    (
        "b715a358-60e6-48ee-86dc-2f662eb72c8e",
        "ee11729a-b007-488b-b4d2-aacdd493195c",
        804153,
        "2026-04-07",
        "ONLINE PAYMENT FROM CHK",
    ),
    (
        "85bfa413-89ea-4395-89da-e3aee3342320",
        "271aed1b-b375-4a03-b1e9-9ad0bf4ea010",
        2183807,
        "2026-05-04",
        "ONLINE PAYMENT FROM CHK",
    ),
];

fn card_balance(store: &EventStore) -> i64 {
    let q = AccountQueries::new(store.connection());
    q.get_account_balance(CARD, None).map(|b| b.balance).unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args.get(1).expect("usage: fix_card_payments <db_path> [--apply]");
    let apply = args.iter().any(|a| a == "--apply");

    let mut store = EventStore::open(db_path)?;
    let _ = UNCATEGORIZED; // documented for the reader; balance reported via SQL outside

    println!("DB: {}", db_path);
    println!("card balance before: {:.2}", card_balance(&store) as f64 / 100.0);
    println!(
        "mode: {}\n",
        if apply { "APPLY (writing changes)" } else { "DRY RUN (no changes)" }
    );

    for (card_id, chk_id, abs, date, label) in PAYMENTS {
        println!(
            "- {} {}  ${:.2}\n    void card  {}\n    void chk   {}\n    post transfer: checking -{:.2} / card +{:.2}",
            date, label, *abs as f64 / 100.0, card_id, chk_id,
            *abs as f64 / 100.0, *abs as f64 / 100.0
        );

        if !apply {
            continue;
        }

        let mut cmds = EntryCommands::new(&mut store, "correction".to_string());

        cmds.void_entry(VoidEntryCommand {
            entry_id: card_id.to_string(),
            reason: "Re-booked as transfer: credit-card payment was mis-signed (single import)"
                .to_string(),
        })?;
        cmds.void_entry(VoidEntryCommand {
            entry_id: chk_id.to_string(),
            reason: "Re-booked as transfer: checking leg of credit-card payment".to_string(),
        })?;

        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")?;
        cmds.post_entry(PostEntryCommand {
            date,
            memo: format!("Transfer: {} (credit-card payment)", label),
            lines: vec![
                EntryLine::credit(CHECKING, *abs, "USD"),
                EntryLine::debit(CARD, *abs, "USD"),
            ],
            reference: Some(format!("transfer-correction:{}:{}", card_id, chk_id)),
            source: Some(JournalEntrySource::Plaid),
        })?;
    }

    println!("\ncard balance after:  {:.2}", card_balance(&store) as f64 / 100.0);
    if !apply {
        println!("(dry run — re-run with --apply to write)");
    }
    Ok(())
}
