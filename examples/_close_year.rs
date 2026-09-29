use accountir::commands::closing_commands::{self as cc, CloseBooksCommand, ClosingTarget};
use accountir::store::event_store::EventStore;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (db, year, acct) = (&a[1], a[2].parse::<i32>().unwrap(), &a[3]);
    let mut store = EventStore::open(db).unwrap();
    let target = ClosingTarget::PartnerCapital(acct.clone());
    let p = cc::preview(store.connection(), year, false, &target).unwrap();
    println!(
        "{year} net income {:.2}, trial balance ok {}",
        p.net_income_cents as f64 / 100.0,
        p.trial_balance_ok
    );
    for s in &p.allocation {
        println!(
            "  {:20} {:>12.2} -> {}",
            s.partner_name,
            s.cents as f64 / 100.0,
            s.account_label
        );
    }
    for w in &p.warnings {
        println!("  warning: {w}");
    }
    cc::close_books(
        &mut store,
        "cli-user",
        CloseBooksCommand {
            year,
            target,
            include_draws: false,
        },
    )
    .unwrap();
    println!("closed");
}
