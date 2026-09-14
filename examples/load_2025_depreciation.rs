//! One-off: put Bunny Ears' 2025 fixed-asset purchases on the depreciation
//! register and post 2025's depreciation.
//!
//! Grouped from the 2025 ledger: the April build-out check, the November and
//! December OCP payments (in service at the last payment), the IKEA furniture net
//! of its return, the KAPLA sets bought before 20 January 2025 (40% bonus), and the
//! crossing sign (100% bonus). The 2025 leasehold improvements are interior
//! work on the leased premises, so qualified improvement property (15-year,
//! 100% bonus), unlike the 39-year treatment the 2023 and 2024 fit-outs were
//! filed under.
//!
//! Idempotent: an asset already on the register by description is reclassified
//! if its class differs and otherwise left alone, and the posting replaces an
//! earlier 2025 entry.
//!
//! Usage: cargo run --example load_2025_depreciation -- /path/to/db

use accountir::commands::depreciation_commands as dc;
use accountir::domain::{BonusElection, DepreciableAsset, PropertyClass, System};
use accountir::store::event_store::EventStore;
use accountir::tax::depreciation::compute_year;
use chrono::NaiveDate;
use rusqlite::OptionalExtension;

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn account(store: &EventStore, name: &str, parent: &str) -> String {
    store
        .connection()
        .query_row(
            "SELECT a.id FROM accounts a JOIN accounts p ON p.id = a.parent_id
              WHERE a.name = ?1 AND p.name = ?2",
            [name, parent],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
        .unwrap_or_else(|| panic!("no account {parent}:{name}"))
}

#[allow(clippy::too_many_arguments)]
fn asset(
    description: &str,
    accounts: (&str, &str, &str),
    acquired: NaiveDate,
    placed: NaiveDate,
    cost_cents: i64,
    class: PropertyClass,
    notes: &str,
) -> DepreciableAsset {
    DepreciableAsset {
        asset_id: String::new(),
        description: description.to_string(),
        asset_account_id: accounts.0.to_string(),
        expense_account_id: accounts.1.to_string(),
        accumulated_account_id: accounts.2.to_string(),
        section_179_account_id: None,
        acquired_on: acquired,
        placed_in_service: placed,
        cost_cents,
        class,
        system: System::Gds,
        section_179_cents: 0,
        bonus: BonusElection::Take,
        disposed_on: None,
        notes: Some(notes.to_string()),
        overrides: Default::default(),
        basis_adjustments: Vec::new(),
    }
}

fn main() {
    let db = std::env::args().nth(1).expect("usage: load_2025_depreciation <db>");
    let mut store = EventStore::open(&db).unwrap();

    let leasehold = (
        "22b499b2-b018-4e62-9198-653e650bc471".to_string(),
        "d5e3b8bf-022f-46ad-bc5e-8d316e8ff8ba".to_string(),
        "ebe42f32-2520-4d0e-948f-35bd78b15611".to_string(),
    );
    let furniture = (
        "9ce9b200-c501-4e88-bfcd-90be9fb9fc23".to_string(),
        "e4841c15-fd3b-4566-ae10-8df2d49387bc".to_string(),
        "a101aa73-44ec-48e0-9f5e-3bab6d444d80".to_string(),
    );
    let equipment = (
        "a457462f-3b54-47cd-9956-0139ee275422".to_string(),
        account(&store, "Equipment", "Depreciation"),
        account(&store, "Equipment", "Accumulated Depreciation"),
    );
    let l = (leasehold.0.as_str(), leasehold.1.as_str(), leasehold.2.as_str());
    let f = (furniture.0.as_str(), furniture.1.as_str(), furniture.2.as_str());
    let e = (equipment.0.as_str(), equipment.1.as_str(), equipment.2.as_str());

    let wanted = [
        asset("Leasehold improvements - 2025 April", l, day(2025, 4, 10), day(2025, 4, 10), 1_662_057, PropertyClass::QualifiedImprovement,
              "Check 3146 (2025-04-10) $16,601.85 and Crafty Beaver hardware (2025-07-12) $18.72"),
        asset("Leasehold improvements - 2025 OCP", l, day(2025, 11, 20), day(2025, 12, 16), 1_165_120, PropertyClass::QualifiedImprovement,
              "OCP Construction: check 3153 (2025-11-20) and Zelle (2025-12-16), $5,825.60 each; in service at the final payment"),
        asset("Furniture - 2025", f, day(2025, 5, 7), day(2025, 7, 28), 143_007, PropertyClass::SevenYear,
              "IKEA 2025-05-07, 05-26 and 07-28, net of the 05-21 purchase returned 05-22"),
        asset("KAPLA construction sets", e, day(2025, 1, 7), day(2025, 1, 7), 37_265, PropertyClass::SevenYear,
              "Amazon 2025-01-07/08, net of the order reconciling differences; acquired before 2025-01-20, so 40% bonus"),
        asset("Pedestrian crossing sign", e, day(2025, 6, 30), day(2025, 6, 30), 21_389, PropertyClass::SevenYear,
              "Amazon 2025-06-30"),
    ];
    for a in &wanted {
        let existing = dc::list_assets(store.connection())
            .into_iter()
            .find(|x| x.description == a.description);
        if let Some(mut existing) = existing {
            if existing.class != a.class {
                println!("reclassified: {} ({:?} -> {:?})", a.description, existing.class, a.class);
                existing.class = a.class;
                dc::update_asset(&mut store, "cli-user", &existing).unwrap();
            } else {
                println!("already on the register: {}", a.description);
            }
            continue;
        }
        dc::add_asset(&mut store, "cli-user", a).unwrap();
        println!("added: {} (${:.2})", a.description, a.cost_cents as f64 / 100.0);
    }

    let assets = dc::list_assets(store.connection());
    let s = compute_year(&assets, 2025);
    println!("\n2025 per the register:");
    for r in &s.rows {
        println!(
            "  {:38} yr{} {:?} bonus {:>8.2} ({:.0}%)  macrs {:>9.2}  total {:>9.2}  accumulated {:>10.2}",
            r.asset.description, r.recovery_year, r.convention, r.bonus_cents as f64 / 100.0, r.bonus_rate * 100.0,
            r.macrs_cents as f64 / 100.0, r.total_cents() as f64 / 100.0, r.accumulated_cents as f64 / 100.0
        );
    }
    println!("  line 16a {:.2}   9a {:.2}   9b {:.2}   mid-quarter years {:?}", s.line_16a_cents() as f64 / 100.0,
             s.gross_cost_cents() as f64 / 100.0, s.accumulated_cents() as f64 / 100.0, s.mid_quarter_years);
    for w in &s.warnings {
        println!("  warning: {}", w.chars().take(200).collect::<String>());
    }
    let posted = dc::post_year(&mut store, "cli-user", 2025, true).unwrap();
    println!("\nposted 2025 depreciation {:.2} as entry {}", posted.depreciation_cents as f64 / 100.0, posted.entry_id);
}
