//! One-off: clear the two negative asset balances on Bunny Ears' 2025 Schedule L.
//!
//! - `1010 Comed deposit` went to −$424.75 when the ComEd deposit came back on
//!   2024-11-29: the deposit it refunded was expensed when paid, so the refund is a
//!   reduction of the electric bill — as the filed 2024 return treated it. The
//!   refund's line is reassigned to `3016 Electric`.
//! - `1014 Amazon gift card balance` went negative as Amazon orders were paid
//!   with Jinny's personal gift card ($228.51: $174.28 in 2024 and $54.23 on
//!   2025-01-07), which was never loaded into the books. Those lines are
//!   reassigned to `4003 Jinny`, as her contribution. The later small gift card
//!   credits are Amazon refunds, not her card, and are left alone.
//!
//! 2024 is reopened for the 2024 lines and closed again the same way it was
//! closed (Equity:Years:2024, then Lois' fixed $1,843.56 and Jinny the rest).
//! Idempotent: a line already on its target is skipped, and 2024 is only reopened
//! when a 2024 line still needs moving.
//!
//! Usage: cargo run --example fix_negative_balances_2024 -- /path/to/db

use accountir::commands::closing_commands::{self as cc, CloseBooksCommand, ClosingTarget};
use accountir::commands::entry_commands::{EntryCommands, ReassignLineCommand};
use accountir::store::event_store::EventStore;

const YEARS_2024: &str = "06a4a555-f2f6-4a33-b85f-945ebb01c18b";

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

/// (entry, line, date) on `account` dated within [from, to].
fn lines_on(store: &EventStore, account: &str, from: &str, to: &str) -> Vec<(String, String, String)> {
    let mut stmt = store
        .connection()
        .prepare(
            "SELECT l.entry_id, l.id, e.date FROM journal_lines l
               JOIN journal_entries e ON e.id = l.entry_id
              WHERE l.account_id = ?1 AND e.is_void = 0 AND e.date BETWEEN ?2 AND ?3
              ORDER BY e.date",
        )
        .unwrap();
    let rows = stmt
        .query_map(rusqlite::params![account, from, to], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .flatten()
        .collect();
    rows
}

fn main() {
    let db = std::env::args().nth(1).expect("usage: fix_negative_balances_2024 <db>");
    let mut store = EventStore::open(&db).unwrap();
    accountir::store::migrations::run_migrations(store.connection()).unwrap();
    let (deposit, electric) = (id_of(&store, "1010"), id_of(&store, "3016"));
    let (gift_card, jinny) = (id_of(&store, "1014"), id_of(&store, "4003"));

    let mut moves: Vec<(String, String, String, String)> = Vec::new();
    for (entry, line, date) in lines_on(&store, &deposit, "2024-11-29", "2024-11-29") {
        moves.push((entry, line, date, electric.clone()));
    }
    for (entry, line, date) in lines_on(&store, &gift_card, "2024-01-01", "2025-01-07") {
        moves.push((entry, line, date, jinny.clone()));
    }
    if moves.is_empty() {
        println!("nothing to move");
        return;
    }

    let reopen = moves.iter().any(|(_, _, date, _)| date.as_str() <= "2024-12-31");
    if reopen {
        cc::reopen_books(
            &mut store,
            "cli-user",
            2024,
            "The ComEd deposit refund belongs to Electric, and Amazon orders paid with Jinny's \
             gift card are her contribution",
        )
        .unwrap();
        println!("reopened 2024");
    }

    for (entry, line, date, target) in &moves {
        EntryCommands::new(&mut store, "cli-user".to_string())
            .reassign_line(ReassignLineCommand {
                entry_id: entry.clone(),
                line_id: line.clone(),
                new_account_id: target.clone(),
            })
            .unwrap();
        let to = if *target == electric { "3016 Electric" } else { "4003 Jinny" };
        println!("  {date} line {line} → {to}");
    }

    if reopen {
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
}
