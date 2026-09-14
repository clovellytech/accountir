//! One-off: record the Chicago Recovery Grant as a reduction in the basis of
//! Bunny Ears' 2023 build-out, as the filed 2024 return treated it, and post
//! 2024's depreciation again.
//!
//! The preparer's Depreciation Detail Listing applies a (114,442) basis
//! adjustment to the fit-out placed in service on 10-19-2023, leaving 10,374 to
//! depreciate. The ledger already carries the grant in
//! `Assets:Leasehold Improvements:Grant reduction`; this puts the same reduction
//! on the register, so Schedule L line 9a reconciles, later years depreciate the
//! reduced basis, and Form 4562 carries a statement showing it.
//!
//! 2024 itself stays at the $132 filed, fixed by hand: the preparer's figure is
//! half a year on the reduced basis, which is not what the remaining-life rule
//! gives, and the books carry what the return claimed.
//!
//! Idempotent: an adjustment already recorded with the same amount and year is
//! left alone.
//!
//! Usage: cargo run --example grant_basis_adjustment_2024 -- /path/to/db

use accountir::commands::depreciation_commands as dc;
use accountir::store::event_store::EventStore;
use accountir::tax::depreciation::compute_year;

const FITOUT_2023: &str = "fa8f9a74-e502-4b89-a772-d8733c308215";
const GRANT_CENTS: i64 = -11_444_228;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args
        .get(1)
        .expect("usage: grant_basis_adjustment_2024 <db_path>");
    let mut store = EventStore::open(db_path)?;

    let asset = dc::get_asset(store.connection(), FITOUT_2023).ok_or("2023 fit-out not found")?;
    if asset
        .basis_adjustments
        .iter()
        .any(|a| a.effective_year == 2024 && a.amount_cents == GRANT_CENTS)
    {
        println!("already recorded on {}", asset.description);
    } else {
        let (id, _) = dc::add_basis_adjustment(
            &mut store,
            "cli-user",
            FITOUT_2023,
            2024,
            GRANT_CENTS,
            "Chicago Recovery Grant (received 2024-12-26) reimbursed the build-out: basis \
             reduced, not income, as filed on the 2024 return",
        )?;
        println!("recorded basis adjustment {id} on {}", asset.description);
    }

    dc::set_override(
        &mut store,
        "cli-user",
        FITOUT_2023,
        2024,
        13_200,
        "$132 as filed on the 2024 return: the preparer's half year on the grant-reduced \
         basis of $10,374, over 39.5 years",
    )?;

    let assets = dc::list_assets(store.connection());
    for year in [2024, 2025, 2026] {
        let schedule = compute_year(&assets, year);
        if let Some(row) = schedule
            .rows
            .iter()
            .find(|r| r.asset.asset_id == FITOUT_2023)
        {
            println!(
                "{year}: adjusted basis {:.2}  depreciation {:.2}  accumulated {:.2}  (line 16a {:.2}, 9a cost {:.2})",
                row.adjusted_cost_cents as f64 / 100.0,
                row.total_cents() as f64 / 100.0,
                row.accumulated_cents as f64 / 100.0,
                schedule.line_16a_cents() as f64 / 100.0,
                schedule.gross_cost_cents() as f64 / 100.0
            );
        }
    }

    let posted = dc::post_year(&mut store, "cli-user", 2024, true)?;
    println!(
        "posted 2024 depreciation {:.2} as entry {}",
        posted.depreciation_cents as f64 / 100.0,
        posted.entry_id
    );
    Ok(())
}
