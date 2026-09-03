//! One-off correction: rewrite Plaid memos that were built from a garbled
//! `merchant_name` (e.g. "And Invo", "AND") to the full bank `name`.
//!
//! Uses the same `plaid_memo` selection the import now uses, so a good merchant
//! name ("Square Inc") maps to itself and only the broken fragments change.
//! Only entries whose current memo is exactly the raw `name` or `merchant_name`
//! are touched — transfer memos ("Transfer: …") and hand-edited ones are left
//! alone.
//!
//! NOTE: this updates the `journal_entries` projection directly, not the
//! `journal_entry_posted` events. It is the right fix for what the ledger
//! displays today; a full projection rebuild from events would reintroduce the
//! old memos (the durable fix is a memo-correction event, a larger change).
//!
//! Usage: cargo run --example fix_plaid_memos -- /path/to/db [--apply]

use accountir::commands::plaid_commands::plaid_memo;
use accountir::store::event_store::EventStore;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args.get(1).expect("usage: fix_plaid_memos <db_path> [--apply]");
    let apply = args.iter().any(|a| a == "--apply");

    let store = EventStore::open(db_path)?;
    let conn = store.connection();

    // (entry_id, current_memo, name, merchant_name) for every imported entry.
    let rows: Vec<(String, String, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT je.id, je.memo, s.name, s.merchant_name
             FROM plaid_imported_transactions pit
             JOIN plaid_staged_transactions s ON s.plaid_transaction_id = pit.plaid_transaction_id
             JOIN journal_entries je ON je.id = pit.entry_id
             WHERE je.is_void = 0",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?, r.get(3)?))
            })?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        out
    };

    println!("DB: {}\nimported entries: {}\nmode: {}\n", db_path, rows.len(), if apply { "APPLY" } else { "DRY RUN" });

    let mut changed = 0usize;
    for (entry_id, memo, name, merchant) in &rows {
        // Only rewrite a plain import memo (the raw name or the raw merchant),
        // never a transfer or hand-edited memo.
        let is_plain = memo == name || merchant.as_deref() == Some(memo.as_str());
        if !is_plain {
            continue;
        }
        let correct = plaid_memo(name, merchant.as_deref());
        if &correct != memo {
            changed += 1;
            if changed <= 12 {
                println!("  \"{}\"  →  \"{}\"", memo, truncate(&correct, 60));
            }
            if apply {
                conn.execute("UPDATE journal_entries SET memo=?1 WHERE id=?2", rusqlite::params![correct, entry_id])?;
            }
        }
    }
    if changed > 12 {
        println!("  … and {} more", changed - 12);
    }
    println!("\n{}: {} memos", if apply { "updated" } else { "would update" }, changed);
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}
