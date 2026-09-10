//! One-off correction: restore checking legs that were reassigned off the
//! account.
//!
//! When filing bank payments out of Uncategorized, the *checking* leg was moved
//! onto the expense/vendor account instead of the counterpart. So a ComEd bill
//! posted as `Checking −104.52 / Uncategorized +104.52` became
//! `Electric −104.52 / Uncategorized +104.52` — the outflow vanished from
//! checking (inflating its balance) and the expense is booked backwards.
//!
//! The correct shape is `Checking −104.52 / Electric +104.52`. This finds every
//! entry that was *posted* with a checking leg but no longer touches checking,
//! and for each:
//!   1. reassigns the original checking line (now sitting on the category
//!      account C, carrying the original checking amount) back to Checking, and
//!   2. reassigns the counterpart line (on Uncategorized) to C.
//! Both the outflow and the categorisation end up correct.
//!
//! Usage: cargo run --example fix_moved_checking_legs -- /path/to/db [--apply]
//! Dry run without --apply.

use accountir::commands::entry_commands::{EntryCommands, ReassignLineCommand};
use accountir::queries::account_queries::AccountQueries;
use accountir::store::event_store::EventStore;

const CHECKING: &str = "57c78c46-7075-4032-acf3-13da277c2efe";

fn checking_balance(store: &EventStore) -> i64 {
    AccountQueries::new(store.connection())
        .get_account_balance(CHECKING, None)
        .map(|b| b.balance)
        .unwrap_or(0)
}

/// Entry ids that were posted with a checking leg but no longer touch checking.
fn affected(store: &EventStore) -> Vec<String> {
    let mut stmt = store
        .connection()
        .prepare(
            "SELECT json_extract(e.payload,'$.entry_id')
             FROM events e
             JOIN journal_entries je ON je.id = json_extract(e.payload,'$.entry_id')
             WHERE e.event_type='journal_entry_posted' AND je.is_void=0
               AND EXISTS (SELECT 1 FROM json_each(e.payload,'$.lines')
                           WHERE value->>'account_id'=?1)
               AND NOT EXISTS (SELECT 1 FROM journal_lines jl
                               WHERE jl.entry_id=je.id AND jl.account_id=?1)
             ORDER BY je.date",
        )
        .unwrap();
    let rows = stmt
        .query_map([CHECKING], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    rows
}

/// The line id that was the checking leg in the posting event.
fn original_checking_line(store: &EventStore, entry_id: &str) -> Option<String> {
    store
        .connection()
        .query_row(
            "SELECT je.value->>'line_id'
             FROM events e, json_each(e.payload,'$.lines') je
             WHERE e.event_type='journal_entry_posted'
               AND json_extract(e.payload,'$.entry_id')=?1
               AND je.value->>'account_id'=?2
             LIMIT 1",
            [entry_id, CHECKING],
            |r| r.get::<_, String>(0),
        )
        .ok()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args
        .get(1)
        .expect("usage: fix_moved_checking_legs <db_path> [--apply]");
    let apply = args.iter().any(|a| a == "--apply");

    let mut store = EventStore::open(db_path)?;
    let entries = affected(&store);

    println!("DB: {}", db_path);
    println!("affected entries: {}", entries.len());
    println!(
        "checking balance before: {:.2}",
        checking_balance(&store) as f64 / 100.0
    );
    println!("mode: {}\n", if apply { "APPLY" } else { "DRY RUN" });

    let (mut fixed, mut skipped) = (0usize, 0usize);
    for entry_id in &entries {
        let Some(chk_line) = original_checking_line(&store, entry_id) else {
            println!("  SKIP {entry_id}: no checking line in posting");
            skipped += 1;
            continue;
        };
        // Current lines: (line_id, account_id).
        let lines: Vec<(String, String)> = {
            let mut stmt = store
                .connection()
                .prepare("SELECT id, account_id FROM journal_lines WHERE entry_id=?1")?;
            let v = stmt
                .query_map([entry_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            v
        };
        if lines.len() != 2 {
            println!("  SKIP {entry_id}: {} lines (want 2)", lines.len());
            skipped += 1;
            continue;
        }
        // The category account is where the original checking line now sits.
        let category = lines
            .iter()
            .find(|(id, _)| id == &chk_line)
            .map(|(_, a)| a.clone());
        let other = lines.iter().find(|(id, _)| id != &chk_line).cloned();
        let (Some(category), Some((other_line, other_acct))) = (category, other) else {
            println!("  SKIP {entry_id}: could not resolve legs");
            skipped += 1;
            continue;
        };
        println!(
            "  {entry_id}: chk_line→Checking, counterpart→{}",
            &category[..8]
        );
        if apply {
            let mut cmds = EntryCommands::new(&mut store, "correction".to_string());
            if category != CHECKING {
                cmds.reassign_line(ReassignLineCommand {
                    entry_id: entry_id.clone(),
                    line_id: chk_line.clone(),
                    new_account_id: CHECKING.to_string(),
                })?;
            }
            if other_acct != category {
                cmds.reassign_line(ReassignLineCommand {
                    entry_id: entry_id.clone(),
                    line_id: other_line,
                    new_account_id: category,
                })?;
            }
        }
        fixed += 1;
    }

    println!(
        "\n{}: {} fixed, {} skipped",
        if apply { "applied" } else { "would fix" },
        fixed,
        skipped
    );
    println!(
        "checking balance after:  {:.2}",
        checking_balance(&store) as f64 / 100.0
    );
    Ok(())
}
