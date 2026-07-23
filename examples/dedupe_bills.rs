//! Void duplicate goods-received bills created by repeated full re-syncs before
//! the idempotency fix. Bills are grouped by (vendor, amount, due_date, memo);
//! the earliest of each group is kept and the rest are voided (their journal
//! entries become [VOID] and drop out of balances/reports). Only bills with no
//! payments applied are touched.
//!
//! Usage: cargo run --example dedupe_bills -- /path/to/db [--apply]
//! Dry run by default (prints the plan, changes nothing). Back up the DB first.

use accountir::commands::bill_commands::{BillCommands, VoidBillCommand};
use accountir::store::event_store::EventStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args
        .get(1)
        .expect("usage: dedupe_bills <db_path> [--apply]");
    let apply = args.iter().any(|a| a == "--apply");

    let mut store = EventStore::open(db_path)?;

    // Duplicate bill ids: every bill except the earliest (by posted_at_event) in
    // each (vendor, amount, due_date, memo) group. Skip bills with payments.
    let dup: Vec<(String, String, i64)> = {
        let conn = store.connection();
        let mut stmt = conn.prepare(
            "SELECT id, vendor, amount FROM (
                SELECT id, vendor, amount, ROW_NUMBER() OVER (
                    PARTITION BY vendor, amount, due_date, COALESCE(memo, '')
                    ORDER BY posted_at_event ASC, id ASC
                ) AS rn
                FROM bills
                WHERE amount_paid = 0 AND status != 'void'
             ) WHERE rn > 1",
        )?;
        let rows: Vec<(String, String, i64)> = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    let total: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM bills WHERE status != 'void'", [], |r| r.get(0))?;
    let dup_value: i64 = dup.iter().map(|(_, _, a)| *a).sum();

    println!("DB: {}", db_path);
    println!("active bills: {}", total);
    println!(
        "duplicate bills to void: {}  (${:.2} of double-counted payables)",
        dup.len(),
        dup_value as f64 / 100.0
    );
    println!("mode: {}\n", if apply { "APPLY (writing)" } else { "DRY RUN" });

    if !apply {
        for (id, vendor, amount) in dup.iter().take(25) {
            println!("  would void {}  {}  ${:.2}", &id[..8.min(id.len())], vendor, *amount as f64 / 100.0);
        }
        if dup.len() > 25 {
            println!("  … and {} more", dup.len() - 25);
        }
        println!("\nRe-run with --apply to void them (back up the DB first).");
        return Ok(());
    }

    let (mut voided, mut failed) = (0u32, 0u32);
    for (id, _, _) in &dup {
        let mut cmds = BillCommands::new(&mut store, "dedupe".to_string());
        match cmds.void_bill(VoidBillCommand {
            bill_id: id.clone(),
            reason: "Duplicate goods-received bill from repeated re-sync".to_string(),
        }) {
            Ok(_) => voided += 1,
            Err(e) => {
                eprintln!("  failed to void {}: {}", &id[..8.min(id.len())], e);
                failed += 1;
            }
        }
    }

    let after: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM bills WHERE status != 'void'", [], |r| r.get(0))?;
    println!(
        "\nvoided {} duplicate bills ({} failed). active bills now: {}",
        voided, failed, after
    );
    Ok(())
}
