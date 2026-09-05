//! Importing a Stripe balance summary.
//!
//! # What the report is
//!
//! Eight rows describing one period's movement of the Stripe balance:
//!
//! ```text
//! starting_balance   Starting balance (2026-07-01)     0.00
//! activity_gross     Account activity before fees    191.83
//! activity_fee       Less fees                        -7.67
//! activity           Activity                        184.16   ← subtotal
//! payouts_gross      Payouts to bank                -184.16
//! payouts_fee        Payout fees                       0.00
//! payouts            Total payouts                  -184.16   ← subtotal
//! ending_balance     Ending balance (2026-07-31)       0.00
//! ```
//!
//! Two of those are subtotals of the rows above them, and adding them in would
//! count the same money twice — so only the four movements are read, and the
//! report's own starting and ending balances are what check that reading.
//!
//! # Three things, which is what a balance summary can honestly say
//!
//! Money came in, Stripe took a fee, and the rest was paid to a bank. That is all
//! this report knows: it has no idea what was sold, to whom, or against which
//! invoice. So the entry it produces is deliberately coarse — revenue, fees,
//! payouts — and anything finer would have to be invented.

use rusqlite::Connection;

use crate::commands::entry_commands::{EntryLine, PostEntryCommand};
use crate::commands::ingest_commands::{IngestError, check_idempotent, load_ingest_mappings};
use crate::commands::import_commands::{parse_amount, parse_delimited_line};
use crate::commands::square_commands::extract_period;
use crate::events::types::JournalEntrySource;

/// One period's movement of the Stripe balance.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BalanceSummary {
    pub starting: i64,
    /// What customers paid, before Stripe's cut.
    pub activity_gross: i64,
    /// Negative on the report and kept negative here.
    pub activity_fee: i64,
    /// Negative: money leaving the balance for a bank account.
    pub payouts_gross: i64,
    pub payouts_fee: i64,
    pub ending: i64,
    /// Set when the rows do not account for the movement the report itself
    /// reports between its starting and ending balances.
    pub reconciliation: Option<String>,
}

impl BalanceSummary {
    /// What the balance actually moved by, from the rows.
    pub fn net_change(&self) -> i64 {
        self.activity_gross + self.activity_fee + self.payouts_gross + self.payouts_fee
    }

    /// Everything Stripe took, as a positive figure.
    pub fn fees(&self) -> i64 {
        -(self.activity_fee + self.payouts_fee)
    }

    /// What reached the bank, as a positive figure.
    pub fn paid_out(&self) -> i64 {
        -self.payouts_gross
    }
}

fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let a = cents.abs();
    format!("{sign}${}.{:02}", a / 100, a % 100)
}

/// Read a balance summary.
pub fn parse_balance_summary(content: &str) -> Result<BalanceSummary, IngestError> {
    let mut out = BalanceSummary::default();
    let mut seen = false;

    for line in content.lines() {
        let f = parse_delimited_line(line.trim_start_matches('\u{feff}'), ',');
        if f.len() < 3 {
            continue;
        }
        let category = f[0].trim().trim_matches('"').to_ascii_lowercase();
        let Some(amount) = parse_amount(f[2].trim().trim_matches('"')) else {
            continue;
        };
        // Only the movements. `activity` and `payouts` are subtotals of rows
        // already counted, and adding them would double the period.
        match category.as_str() {
            "starting_balance" => out.starting = amount,
            "activity_gross" => {
                out.activity_gross = amount;
                seen = true;
            }
            "activity_fee" => out.activity_fee = amount,
            "payouts_gross" => out.payouts_gross = amount,
            "payouts_fee" => out.payouts_fee = amount,
            "ending_balance" => out.ending = amount,
            _ => {}
        }
    }

    if !seen {
        return Err(IngestError::MissingMapping(
            "this does not look like a Stripe balance summary: no 'activity_gross' row".to_string(),
        ));
    }

    // The report's own arithmetic. If the movements do not carry the starting
    // balance to the ending one, something on the report is not being read and
    // the entry would be wrong by the difference.
    let expected = out.starting + out.net_change();
    if expected != out.ending {
        out.reconciliation = Some(format!(
            "The rows on this report move the balance from {} to {}, but it reports an ending \
             balance of {} — a difference of {}. Something on it is not being accounted for.",
            dollars(out.starting),
            dollars(expected),
            dollars(out.ending),
            dollars(expected - out.ending)
        ));
    }
    Ok(out)
}

/// Decide what a balance summary posts, without writing anything.
///
/// `None` when the period is already in the books or there was no movement.
pub fn plan_stripe(
    conn: &Connection,
    content: &str,
    file_name: &str,
) -> Result<Option<PostEntryCommand>, IngestError> {
    let (start, end) = extract_period(file_name).ok_or_else(|| {
        IngestError::InvalidDate(format!(
            "no YYYY-MM-DD period found in the filename '{file_name}'"
        ))
    })?;

    let s = parse_balance_summary(content)?;
    if let Some(problem) = &s.reconciliation {
        return Err(IngestError::EntryError(problem.clone()));
    }
    if s.activity_gross == 0 && s.fees() == 0 && s.paid_out() == 0 {
        return Ok(None);
    }

    let reference = format!(
        "stripe-balance-{}_{}",
        start.format("%Y-%m-%d"),
        end.format("%Y-%m-%d")
    );
    if check_idempotent(conn, &reference).is_some() {
        return Ok(None);
    }

    let mut required = vec!["pos_stripe", "stripe_revenue"];
    if s.fees() != 0 {
        required.push("stripe_fees");
    }
    if s.paid_out() != 0 {
        required.push("stripe_payout_bank");
    }
    let m = load_ingest_mappings(conn, &required)?;

    // The balance moves by what came in, less what Stripe took, less what was
    // paid away — which is the report's ending balance less its starting one.
    let mut lines = vec![
        EntryLine::debit(&m["pos_stripe"], s.net_change(), "USD")
            .with_memo("Change in Stripe balance"),
    ];
    if s.fees() != 0 {
        lines.push(EntryLine::debit(&m["stripe_fees"], s.fees(), "USD").with_memo("Stripe fees"));
    }
    if s.paid_out() != 0 {
        lines.push(
            EntryLine::debit(&m["stripe_payout_bank"], s.paid_out(), "USD")
                .with_memo("Paid out to bank"),
        );
    }
    lines.push(
        EntryLine::credit(&m["stripe_revenue"], s.activity_gross, "USD")
            .with_memo("Stripe sales"),
    );
    lines.retain(|l| l.amount != 0);

    Ok(Some(PostEntryCommand {
        date: end,
        memo: format!(
            "Stripe {} – {}",
            start.format("%Y-%m-%d"),
            end.format("%Y-%m-%d")
        ),
        lines,
        reference: Some(reference),
        source: Some(JournalEntrySource::Pos),
    }))
}

/// Post one balance summary. `false` when there was nothing to post — the
/// period is already in the books, or nothing moved.
pub fn ingest_stripe(
    store: &mut crate::store::event_store::EventStore,
    user_id: &str,
    content: &str,
    file_name: &str,
) -> Result<bool, IngestError> {
    let Some(cmd) = plan_stripe(store.connection(), content, file_name)? else {
        return Ok(false);
    };
    let mut commands =
        crate::commands::entry_commands::EntryCommands::new(store, user_id.to_string());
    let (_, was_duplicate) =
        crate::commands::ingest_commands::post_ingest_entry(&mut commands, cmd)?;
    Ok(!was_duplicate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real report from this ledger.
    const SAMPLE: &str = "\"category\",\"description\",\"net_amount\",\"currency\"\n\
\"starting_balance\",\"Starting balance (2026-07-01)\",\"0.00\",\"usd\"\n\
\"activity_gross\",\"Account activity before fees\",\"191.83\",\"usd\"\n\
\"activity_fee\",\"Less fees\",\"-7.67\",\"usd\"\n\
\"activity\",\"Activity\",\"184.16\",\"usd\"\n\
\"payouts_gross\",\"Payouts to bank\",\"-184.16\",\"usd\"\n\
\"payouts_fee\",\"Payout fees\",\"0.00\",\"usd\"\n\
\"payouts\",\"Total payouts\",\"-184.16\",\"usd\"\n\
\"ending_balance\",\"Ending balance (2026-07-31)\",\"0.00\",\"usd\"\n";

    #[test]
    fn the_three_movements_are_read_and_the_subtotals_are_not() {
        let s = parse_balance_summary(SAMPLE).unwrap();
        assert_eq!(s.activity_gross, 19183);
        assert_eq!(s.fees(), 767);
        assert_eq!(s.paid_out(), 18416);
        assert!(s.reconciliation.is_none(), "{:?}", s.reconciliation);

        // "activity" and "payouts" are subtotals of rows already counted. Adding
        // them would double the period — the balance moved by nothing here, and
        // it has to come out as nothing.
        assert_eq!(s.net_change(), 0);
        assert_eq!(s.starting + s.net_change(), s.ending);
    }

    /// The entry balances, and each of the three things the report knows lands
    /// on its own line.
    #[test]
    fn the_entry_says_revenue_fees_and_payout() {
        let store = crate::store::event_store::EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        for (k, a) in [
            ("pos_stripe", "stripe-balance"),
            ("stripe_revenue", "revenue"),
            ("stripe_fees", "fees"),
            ("stripe_payout_bank", "bank"),
        ] {
            store
                .connection()
                .execute(
                    "INSERT INTO ingest_account_mappings (key, account_id) VALUES (?1, ?2)",
                    rusqlite::params![k, a],
                )
                .unwrap();
        }

        let cmd = plan_stripe(
            store.connection(),
            SAMPLE,
            "Balance_Summary_USD_2026-07-01_to_2026-07-31_America-Chicago.csv",
        )
        .unwrap()
        .expect("an entry");

        assert_eq!(cmd.lines.iter().map(|l| l.amount).sum::<i64>(), 0, "balances");
        let of = |a: &str| cmd.lines.iter().find(|l| l.account_id == a).map(|l| l.amount);
        assert_eq!(of("fees"), Some(767), "fees are a cost");
        assert_eq!(of("bank"), Some(18416), "the payout reached the bank");
        assert_eq!(of("revenue"), Some(-19183), "gross activity is the revenue");
        // The balance itself did not move this period, so it carries no line.
        assert_eq!(of("stripe-balance"), None);
        assert_eq!(
            cmd.date,
            chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap()
        );
    }

    /// A period that keeps money on the balance shows it there.
    #[test]
    fn money_left_on_the_balance_stays_on_the_balance() {
        let csv = "category,description,net_amount,currency\n\
starting_balance,Starting,0.00,usd\n\
activity_gross,Activity,100.00,usd\n\
activity_fee,Fees,-3.00,usd\n\
payouts_gross,Payouts,-50.00,usd\n\
payouts_fee,Payout fees,0.00,usd\n\
ending_balance,Ending,47.00,usd\n";
        let s = parse_balance_summary(csv).unwrap();
        assert!(s.reconciliation.is_none(), "{:?}", s.reconciliation);
        assert_eq!(s.net_change(), 4700);
    }

    /// A report whose rows do not carry its own starting balance to its own
    /// ending balance is reported rather than posted.
    #[test]
    fn a_report_that_does_not_reconcile_is_refused() {
        let csv = "category,description,net_amount,currency\n\
starting_balance,Starting,0.00,usd\n\
activity_gross,Activity,100.00,usd\n\
ending_balance,Ending,999.00,usd\n";
        let s = parse_balance_summary(csv).unwrap();
        assert!(s.reconciliation.is_some());

        let store = crate::store::event_store::EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        let err = plan_stripe(
            store.connection(),
            csv,
            "Balance_Summary_USD_2026-07-01_to_2026-07-31.csv",
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not being accounted for"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_a_balance_summary_says_so() {
        let e = parse_balance_summary("a,b,c\n1,2,3").unwrap_err();
        assert!(format!("{e}").contains("Stripe balance summary"), "{e}");
    }
}
