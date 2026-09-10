//! One-off repair: file the Amazon "reconciling difference" lines where they
//! belong, now that the item lines beside them have been categorised.
//!
//! The old importer listed a split-tender order's items once per payment and
//! plugged the excess. The plug cancels the duplicate copy, so an entry only
//! foots to what it actually paid if the plug sits in the *same account as its
//! own item lines*. Once the items were categorised out of Uncategorized and the
//! plugs were not, the expense accounts were left overstated by exactly the
//! duplication and Uncategorized held the offset.
//!
//! Dry run by default; pass --apply to write. Every move goes through
//! `reassign_line`, so each one is an event like any other and can be undone the
//! same way.

use accountir::commands::entry_commands::{EntryCommands, ReassignLineCommand};
use accountir::store::event_store::EventStore;

/// A line to move, named the way a human would check it: which entry, which
/// amount, and where it is going.
struct Move {
    reference: &'static str,
    /// Line amount in cents, which identifies the line within the entry.
    amount: i64,
    /// Only lines currently in this account are eligible — a guard, so a rerun
    /// after a partial apply cannot move something twice.
    from: &'static str,
    to: &'static str,
    why: &'static str,
}

/// The 16 split-tender plugs whose entries' items were all filed to Supplies.
const TO_SUPPLIES: &[(&str, i64)] = &[
    ("amazon-111-5401525-2338651-20240814-10000-", -4647),
    ("amazon-111-5401525-2338651-20240814-4647-1174", -10000),
    ("amazon-114-3641382-8276237-20241207-324-", -1207),
    ("amazon-114-3641382-8276237-20241208-1207-1174", -324),
    ("amazon-111-6493391-1164267-20250804-483-", -6435),
    ("amazon-111-6493391-1164267-20250804-2976-1007", -483),
    ("amazon-114-1426726-7752260-20251108-297-", -4101),
    ("amazon-114-1426726-7752260-20251109-4101-1007", -297),
    ("amazon-114-1885545-2426600-20260203-161-", -4378),
    ("amazon-114-1885545-2426600-20260205-4378-1174", -161),
    ("amazon-114-2102713-7939431-20260405-806-", -4075),
    ("amazon-111-4441467-9776221-20260409-1077-", -2100),
    ("amazon-111-4441467-9776221-20260410-2100-1007", -1077),
    ("amazon-111-7454510-9607444-20260424-404-", -2134),
    ("amazon-111-7454510-9607444-20260425-2134-1174", -404),
    ("amazon-114-0354356-0182612-20260620-10767-1174", -202),
];

fn main() {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("usage: fix_amazon_plugs <db> [--apply]");
    let apply = args.any(|a| a == "--apply");

    let mut store = EventStore::open(&db).expect("open books");

    let account = |number: &str| -> String {
        store
            .connection()
            .query_row(
                "SELECT id FROM accounts WHERE account_number = ?1",
                [number],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| panic!("no account {number}"))
    };
    let supplies = account("3001");
    let equipment = account("1007");
    let clearing = account("6001");
    let uncategorized = account("9000");

    let mut moves: Vec<Move> = TO_SUPPLIES
        .iter()
        .map(|(reference, amount)| Move {
            reference,
            amount: *amount,
            from: "9000",
            to: "3001",
            why: "split-tender plug follows its entry's items",
        })
        .collect();

    // The one item still uncategorised, and its plug. Its twin in the sibling
    // entry was filed to Supplies, so this copy belongs there too.
    moves.push(Move {
        reference: "amazon-112-0535040-1055428-20240925-2911-1007",
        amount: 2938,
        from: "9000",
        to: "3001",
        why: "copy paper — its twin line was filed to Supplies",
    });
    moves.push(Move {
        reference: "amazon-112-0535040-1055428-20240925-2911-1007",
        amount: -27,
        from: "9000",
        to: "3001",
        why: "plug follows the item above",
    });

    // One KAPLA set, $372.65, paid part on a gift card and part on the Amex — so
    // the report listed it under both and it was booked twice. The two copies
    // were filed to different accounts, which left the cost split between them
    // by how it was *paid*. It is equipment, so all four lines go there and the
    // duplication cancels inside the one account.
    for (reference, amount, from) in [
        (
            "amazon-111-2532741-1940208-20250107-5423-",
            37265i64,
            "3001",
        ),
        ("amazon-111-2532741-1940208-20250107-5423-", -31842, "9000"),
        (
            "amazon-111-2532741-1940208-20250108-31842-1007",
            -5423,
            "3001",
        ),
    ] {
        moves.push(Move {
            reference,
            amount,
            from,
            to: "1007",
            why: "the KAPLA set is equipment; both copies and both plugs belong there",
        });
    }

    // Two butcher-paper rolls, genuinely charged twice. The old importer merged
    // the two charges into one entry, so only one of the two card charges ever
    // cleared: this plug is the second clearing credit the entry failed to make,
    // and the orphan card charge is its other half.
    moves.push(Move {
        reference: "amazon-114-7451507-5634636-20260410-4238-1007",
        amount: -4238,
        from: "9000",
        to: "6001",
        why: "the second clearing credit the merged entry never made",
    });

    let mut cmds: Vec<(ReassignLineCommand, String, &Move)> = Vec::new();
    for m in &moves {
        let from_id = account(m.from);
        let to_id = match m.to {
            "3001" => supplies.clone(),
            "1007" => equipment.clone(),
            "6001" => clearing.clone(),
            other => panic!("unexpected target {other}"),
        };
        let found: Result<(String, String, String), _> = store.connection().query_row(
            "SELECT jl.id, jl.entry_id, COALESCE(jl.memo, '') \
             FROM journal_lines jl JOIN journal_entries je ON je.id = jl.entry_id \
             WHERE je.reference = ?1 AND jl.amount = ?2 AND jl.account_id = ?3 AND je.is_void = 0",
            rusqlite::params![m.reference, m.amount, from_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        );
        match found {
            Ok((line_id, entry_id, memo)) => {
                println!(
                    "  {:>9}  {} → {}  {}  [{}]",
                    format!("{:.2}", m.amount as f64 / 100.0),
                    m.from,
                    m.to,
                    m.reference,
                    memo.chars().take(46).collect::<String>()
                );
                cmds.push((
                    ReassignLineCommand {
                        entry_id,
                        line_id,
                        new_account_id: to_id,
                    },
                    m.reference.to_string(),
                    m,
                ));
            }
            Err(_) => println!(
                "  SKIP (not found in {}, already moved?)  {:>9}  {}",
                m.from,
                format!("{:.2}", m.amount as f64 / 100.0),
                m.reference
            ),
        }
    }

    // The orphan card charge, found by shape rather than reference: a Plaid
    // Amazon debit sitting in Uncategorized instead of the clearing account.
    let orphan: Result<(String, String), _> = store.connection().query_row(
        "SELECT jl.id, jl.entry_id FROM journal_lines jl JOIN journal_entries je ON je.id = jl.entry_id \
         WHERE je.source = 'plaid' AND je.memo = 'Amazon' AND je.date = '2026-04-10' \
           AND jl.amount = 4238 AND jl.account_id = ?1 AND je.is_void = 0",
        [&uncategorized],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    if let Ok((line_id, entry_id)) = orphan {
        println!("      42.38  9000 → 6001  (plaid Amazon charge that never cleared)");
        cmds.push((
            ReassignLineCommand {
                entry_id,
                line_id,
                new_account_id: clearing.clone(),
            },
            "plaid orphan".to_string(),
            &moves[0],
        ));
    } else {
        println!("  SKIP  the orphan plaid 42.38 is not in 9000 (already moved?)");
    }

    println!("\n{} move(s)", cmds.len());
    if !apply {
        println!("dry run — pass --apply to write");
        return;
    }

    let mut done = 0;
    for (cmd, label, _) in cmds {
        let mut h = EntryCommands::new(&mut store, "plug-repair".to_string());
        match h.reassign_line(cmd) {
            Ok(_) => done += 1,
            Err(e) => println!("  FAILED {label}: {e}"),
        }
    }
    println!("{done} line(s) reassigned");
}
