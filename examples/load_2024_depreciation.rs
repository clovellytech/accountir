//! One-off: put Bunny Ears' 2024 assets on the depreciation register as the
//! filed 2024 return depreciated them, and post 2024's depreciation.
//!
//! From the preparer's Depreciation Detail Listing behind the filed return:
//!
//! | # | Asset                   | In service | Cost    | Class        | 2024  |
//! |---|-------------------------|------------|---------|--------------|-------|
//! | 1 | Leasehold (2023)        | 10-19-2023 | 124,816 | 39.5 SL      |   132 |
//! | 2 | Furniture (2023)        | 07-01-2023 |   1,714 | 7 yr, bonus  |    84 |
//! | 3 | Furniture - 2024        | 07-01-2024 |   8,395 | 7 yr, 60%    | 5,517 |
//! | 4 | Computer                | 07-01-2024 |   1,212 | 3 yr, 60%    |   889 |
//! | 5 | Refrigerator            | 07-01-2024 |   1,054 | 5 yr, 60%    |   716 |
//! | 6 | Leasehold (2024)        | 01-01-2024 |  63,561 | 39.5 SL      | 1,542 |
//!
//! Rows 1 and 2 are already on the register. The two leasehold rows are on a
//! 39.5-year life the tables do not give, and row 1 carries the preparer's
//! grant-reduced figure, so both are fixed by override with the reason. The
//! (114,442) grant basis adjustment has no register row: it depreciates nothing.
//!
//! Idempotent: accounts and assets already present are reused, and the posting
//! replaces an earlier 2024 depreciation entry.
//!
//! Usage: cargo run --example load_2024_depreciation -- /path/to/db

use accountir::commands::account_commands::{AccountCommands, CreateAccountCommand};
use accountir::commands::depreciation_commands as dc;
use accountir::domain::{AccountType, BonusElection, DepreciableAsset, PropertyClass, System};
use accountir::events::types::Event;
use accountir::store::event_store::EventStore;
use chrono::NaiveDate;
use rusqlite::OptionalExtension;

const EXPENSE_DEPRECIATION: &str = "08663e2e-da74-4153-bbaf-95e8480fe072";
const ACCUMULATED_DEPRECIATION: &str = "44b79cfb-b250-4ace-af5a-9e59a45f3824";
const FURNITURE: &str = "9ce9b200-c501-4e88-bfcd-90be9fb9fc23";
const FURNITURE_EXPENSE: &str = "e4841c15-fd3b-4566-ae10-8df2d49387bc";
const FURNITURE_ACCUMULATED: &str = "a101aa73-44ec-48e0-9f5e-3bab6d444d80";
const EQUIPMENT: &str = "a457462f-3b54-47cd-9956-0139ee275422";
const LEASEHOLD: &str = "22b499b2-b018-4e62-9198-653e650bc471";
const LEASEHOLD_EXPENSE: &str = "d5e3b8bf-022f-46ad-bc5e-8d316e8ff8ba";
const LEASEHOLD_ACCUMULATED: &str = "ebe42f32-2520-4d0e-948f-35bd78b15611";
const LEASEHOLD_2023_ASSET: &str = "fa8f9a74-e502-4b89-a772-d8733c308215";

const SOURCE: &str = "as filed on the 2024 return (preparer's depreciation detail listing)";

fn ensure_account(
    store: &mut EventStore,
    account_type: AccountType,
    number: &str,
    name: &str,
    parent_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let existing: Option<String> = store
        .connection()
        .query_row(
            "SELECT id FROM accounts WHERE name = ?1 AND parent_id = ?2",
            [name, parent_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let event = AccountCommands::new(store, "cli-user".to_string()).create_account(
        CreateAccountCommand {
            account_type,
            account_number: number.to_string(),
            name: name.to_string(),
            parent_id: Some(parent_id.to_string()),
            currency: None,
            description: None,
        },
    )?;
    match event.event {
        Event::AccountCreated { account_id, .. } => {
            println!("created account {number} {name}");
            Ok(account_id)
        }
        other => Err(format!("expected an account, got {}", other.event_type()).into()),
    }
}

fn day(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

#[allow(clippy::too_many_arguments)]
fn asset(
    description: &str,
    asset_account: &str,
    expense: &str,
    accumulated: &str,
    placed: NaiveDate,
    cost_cents: i64,
    class: PropertyClass,
    notes: &str,
) -> DepreciableAsset {
    DepreciableAsset {
        asset_id: String::new(),
        description: description.to_string(),
        asset_account_id: asset_account.to_string(),
        expense_account_id: expense.to_string(),
        accumulated_account_id: accumulated.to_string(),
        section_179_account_id: None,
        acquired_on: placed,
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args.get(1).expect("usage: load_2024_depreciation <db_path>");
    let mut store = EventStore::open(db_path)?;

    let equipment_expense = ensure_account(
        &mut store,
        AccountType::Expense,
        "3060",
        "Equipment",
        EXPENSE_DEPRECIATION,
    )?;
    let equipment_accumulated = ensure_account(
        &mut store,
        AccountType::Asset,
        "1020",
        "Equipment",
        ACCUMULATED_DEPRECIATION,
    )?;

    let wanted = [
        asset(
            "Furniture - 2024",
            FURNITURE,
            FURNITURE_EXPENSE,
            FURNITURE_ACCUMULATED,
            day(2024, 7, 1),
            839_500,
            PropertyClass::SevenYear,
            "2024 furniture purchases, placed in service mid-2024 on the half-year convention; as filed",
        ),
        asset(
            "Computer",
            EQUIPMENT,
            &equipment_expense,
            &equipment_accumulated,
            day(2024, 7, 1),
            121_200,
            PropertyClass::ThreeYear,
            "Best Buy, 2024-12-20; 3-year property as filed on the 2024 return",
        ),
        asset(
            "Refrigerator",
            FURNITURE,
            FURNITURE_EXPENSE,
            FURNITURE_ACCUMULATED,
            day(2024, 7, 1),
            105_400,
            PropertyClass::FiveYear,
            "5-year property as filed on the 2024 return",
        ),
        asset(
            "Leasehold improvements - 2024",
            LEASEHOLD,
            LEASEHOLD_EXPENSE,
            LEASEHOLD_ACCUMULATED,
            day(2024, 1, 1),
            6_356_100,
            PropertyClass::Nonresidential,
            "2024 build-out additions; 39.5-year straight line as filed",
        ),
    ];

    let mut leasehold_2024 = None;
    for a in &wanted {
        let existing = dc::list_assets(store.connection())
            .into_iter()
            .find(|x| x.description == a.description);
        let id = match existing {
            Some(x) => {
                println!("already on the register: {}", a.description);
                x.asset_id
            }
            None => {
                let (id, _) = dc::add_asset(&mut store, "cli-user", a)?;
                println!("added: {} (${:.2})", a.description, a.cost_cents as f64 / 100.0);
                id
            }
        };
        if a.description == "Leasehold improvements - 2024" {
            leasehold_2024 = Some(id);
        }
    }

    dc::set_override(
        &mut store,
        "cli-user",
        LEASEHOLD_2023_ASSET,
        2024,
        13_200,
        &format!("$132 {SOURCE}: the preparer's figure on the grant-reduced basis"),
    )?;
    dc::set_override(
        &mut store,
        "cli-user",
        leasehold_2024.as_deref().expect("added above"),
        2024,
        154_200,
        &format!("$1,542 {SOURCE}: 39.5-year life, mid-month from January"),
    )?;

    let assets = dc::list_assets(store.connection());
    let schedule = accountir::tax::depreciation::compute_year(&assets, 2024);
    println!("\n2024 per the register:");
    for row in &schedule.rows {
        println!(
            "  {:32} bonus {:>10.2}  macrs {:>9.2}  total {:>10.2}",
            row.asset.description,
            row.bonus_cents as f64 / 100.0,
            row.macrs_cents as f64 / 100.0,
            row.total_cents() as f64 / 100.0
        );
    }
    println!("  line 16a: {:.2}", schedule.line_16a_cents() as f64 / 100.0);
    for w in &schedule.warnings {
        println!("  warning: {w}");
    }

    let posted = dc::post_year(&mut store, "cli-user", 2024, true)?;
    println!(
        "\nposted 2024 depreciation {:.2} as entry {}{}",
        posted.depreciation_cents as f64 / 100.0,
        posted.entry_id,
        posted
            .replaced
            .map(|r| format!(", replacing {r}"))
            .unwrap_or_default()
    );
    Ok(())
}
