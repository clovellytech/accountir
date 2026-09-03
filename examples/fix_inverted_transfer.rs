//! One-off correction: un-invert the 2025-04-29 checking→credit-card payment.
//!
//! Entry 09711258 was imported correctly (checking credit −$10,000, offset in
//! Uncategorized) but a series of leg-level reassignments left the two signed
//! amounts glued to the wrong accounts: the −10,000 (credit) leg ended up on the
//! BOA card and the +10,000 (debit) leg on checking, so checking's running
//! balance climbs $10,000 instead of falling. This puts each amount back on the
//! account it belongs to via two reassignments (no void/repost — same entry).
//!
//! Correct end state:
//!   line-1  Business Checking  −10,000  (credit, money out)
//!   line-2  BOA Credit Card    +10,000  (debit, pays down card)
//!
//! Usage: cargo run --example fix_inverted_transfer -- /path/to/Tailorbird-Fabrics.db [--apply]
//! Without --apply it runs as a dry run (prints the plan, makes no changes).

use accountir::commands::entry_commands::{EntryCommands, ReassignLineCommand};
use accountir::queries::account_queries::AccountQueries;
use accountir::store::event_store::EventStore;

const CHECKING: &str = "57c78c46-7075-4032-acf3-13da277c2efe"; // Business Checking (asset)
const CARD: &str = "374f1594-5233-4934-99cb-233d06f41093"; // BOA Credit Card (liability)

/// Every inverted checking→card payment shares one shape: line-1 is the −amount
/// (credit) sitting on the card, line-2 is the +amount (debit) sitting on
/// checking — exactly backwards. Putting line-1 on checking and line-2 on the
/// card restores it. Confirmed against the original `journal_entry_posted`
/// events: for each of these the checking leg's sign flipped between posting and
/// now, which is the definition of the inversion.
const INVERTED: &[&str] = &[
    "09711258-a6ed-4858-8487-025b6ba4fe15", // 2025-04-29  −10,000
    "76121d6f-b0f0-4df8-8363-7f797ab51937", // 2025-05-20  −10,000
    "bdb80b2d-f9e9-46ff-abbb-2038758fcf8c", // 2025-06-10  −5,173
    "4bbcbe48-b20f-44e1-ab85-9b3c6aa40c17", // 2025-07-10  −11,010
];

fn checking_balance(store: &EventStore) -> i64 {
    AccountQueries::new(store.connection())
        .get_account_balance(CHECKING, None)
        .map(|b| b.balance)
        .unwrap_or(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args.get(1).expect("usage: fix_inverted_transfer <db_path> [--apply]");
    let apply = args.iter().any(|a| a == "--apply");

    let mut store = EventStore::open(db_path)?;

    println!("DB: {}", db_path);
    println!("entries: {}", INVERTED.len());
    println!("checking balance before: {:.2}", checking_balance(&store) as f64 / 100.0);
    println!(
        "mode: {}\n",
        if apply { "APPLY (writing changes)" } else { "DRY RUN (no changes)" }
    );

    // (line_id suffix, account it belongs on): line-1 is the credit, line-2 the
    // debit. Reassign only when the line is not already there, so a re-run (or an
    // entry already fixed by hand) is a no-op rather than an error.
    let targets = [("line-1", CHECKING), ("line-2", CARD)];

    for entry in INVERTED {
        for (suffix, want) in targets {
            let line_id = format!("{entry}-{suffix}");
            let current: String = store.connection().query_row(
                "SELECT account_id FROM journal_lines WHERE id = ?1",
                [&line_id],
                |r| r.get(0),
            )?;
            let action = if current == want { "ok" } else { "reassign" };
            println!("  {line_id}  {action} → {}", if want == CHECKING { "Checking" } else { "Card" });
            if apply && current != want {
                let mut cmds = EntryCommands::new(&mut store, "correction".to_string());
                cmds.reassign_line(ReassignLineCommand {
                    entry_id: entry.to_string(),
                    line_id,
                    new_account_id: want.to_string(),
                })?;
            }
        }
    }
    if apply {
        println!("\napplied.");
    }

    println!("checking balance after:  {:.2}", checking_balance(&store) as f64 / 100.0);
    if !apply {
        println!("(dry run — re-run with --apply to write)");
    }
    Ok(())
}
