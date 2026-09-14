//! One-off: close Bunny Ears' 2024 books the way 2023 was closed — the year's
//! result into `Equity:Years:2024`, then allocated to the partners' capital
//! accounts (by the 2024 fixed allocation: Lois $1,843.56, Jinny the rest).
//! Distributions stay on their own accounts, as 2023's close left them.
//!
//! Prints the preview, then closes. Pass `--preview` to stop after the preview.
//!
//! Usage: cargo run --example close_2024 -- /path/to/db <Equity:Years:2024 account id> [--preview]

use accountir::commands::closing_commands::{self as cc, CloseBooksCommand, ClosingTarget};
use accountir::store::event_store::EventStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args.get(1).expect("usage: close_2024 <db> <year account id> [--preview]");
    let year_account = args.get(2).expect("usage: close_2024 <db> <year account id> [--preview]");
    let preview_only = args.iter().any(|a| a == "--preview");
    let mut store = EventStore::open(db_path)?;

    let target = ClosingTarget::PartnerCapital(year_account.clone());
    let p = cc::preview(store.connection(), 2024, false, &target)?;
    println!(
        "2024: {} to {}  net income {:.2}  trial balance ok: {}",
        p.year_start,
        p.year_end,
        p.net_income_cents as f64 / 100.0,
        p.trial_balance_ok
    );
    println!(
        "  swept: {} revenue, {} expense account(s)",
        p.revenue.len(),
        p.expenses.len()
    );
    for share in &p.allocation {
        println!(
            "  {:20} {:>12.2}  -> {}",
            share.partner_name,
            share.cents as f64 / 100.0,
            share.account_label
        );
    }
    if preview_only {
        return Ok(());
    }

    let closed = cc::close_books(
        &mut store,
        "cli-user",
        CloseBooksCommand {
            year: 2024,
            target,
            include_draws: false,
        },
    )?;
    println!("closed: {closed:?}");
    Ok(())
}
