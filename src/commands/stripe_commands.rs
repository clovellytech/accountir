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
//!
//! # Payouts land in a clearing account, not the bank
//!
//! The payout leg is the one place this import can go quietly wrong. Stripe pays
//! out on its own schedule, several times a month, and each of those deposits
//! also arrives through the bank feed as its own transaction. A monthly total
//! debited straight to checking would sit alongside the individual deposits it
//! is made of, and the account would be overstated by the whole month.
//!
//! So `stripe_payouts_in_transit` is a clearing account. This import debits it;
//! the bank deposits credit it as they land. Its balance is what Stripe has sent
//! but the bank has not yet shown, and it returns to zero once everything has
//! arrived — which makes it a check rather than just a workaround.
//! [`load_ingest_mappings`] refuses to run if it is pointed at an account a bank
//! feed already fills.

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
    /// Positive: money sent *to* Stripe to fund the balance, the opposite of a
    /// payout. Wired from the bank, so it is the same journey in reverse and
    /// settles against the same clearing account.
    pub topups_gross: i64,
    pub topups_fee: i64,
    pub ending: i64,
    /// Set when the rows do not account for the movement the report itself
    /// reports between its starting and ending balances.
    pub reconciliation: Option<String>,
}

impl BalanceSummary {
    /// What the balance actually moved by, from the rows.
    pub fn net_change(&self) -> i64 {
        self.activity_gross
            + self.activity_fee
            + self.payouts_gross
            + self.payouts_fee
            + self.topups_gross
            + self.topups_fee
    }

    /// Everything Stripe took, as a positive figure.
    pub fn fees(&self) -> i64 {
        -(self.activity_fee + self.payouts_fee + self.topups_fee)
    }

    /// What reached the bank, as a positive figure.
    pub fn paid_out(&self) -> i64 {
        -self.payouts_gross
    }

    /// What was sent to Stripe from the bank, as a positive figure.
    ///
    /// A payout run backwards: the bank pays Stripe instead of Stripe paying the
    /// bank. The bank feed books its side to the same clearing account, so this
    /// credits what a payout debits and the two meet there.
    pub fn topped_up(&self) -> i64 {
        self.topups_gross
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
            "topups_gross" => out.topups_gross = amount,
            "topups_fee" => out.topups_fee = amount,
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
    if s.activity_gross == 0 && s.fees() == 0 && s.paid_out() == 0 && s.topped_up() == 0 {
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
    if s.paid_out() != 0 || s.topped_up() != 0 {
        required.push("stripe_payouts_in_transit");
    }
    let m = load_ingest_mappings(conn, &required)?;

    // The balance moves by what came in, less what Stripe took, less what was
    // paid away — which is the report's ending balance less its starting one.
    // Signed throughout: the balance can fall over a month, and a period with
    // more refunded than taken shows negative activity. Either flips the side of
    // its line rather than just its magnitude.
    let mut lines = vec![
        EntryLine::signed(&m["pos_stripe"], s.net_change(), "USD")
            .with_memo("Change in Stripe balance"),
    ];
    if s.fees() != 0 {
        lines.push(EntryLine::signed(&m["stripe_fees"], s.fees(), "USD").with_memo("Stripe fees"));
    }
    if s.paid_out() != 0 {
        lines.push(
            EntryLine::signed(&m["stripe_payouts_in_transit"], s.paid_out(), "USD")
                .with_memo("Paid out to bank — clears against the deposits"),
        );
    }
    if s.topped_up() != 0 {
        lines.push(
            EntryLine::signed(&m["stripe_payouts_in_transit"], -s.topped_up(), "USD")
                .with_memo("Topped up from the bank"),
        );
    }
    lines.push(
        EntryLine::signed(&m["stripe_revenue"], -s.activity_gross, "USD")
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

    /// Money can go the other way: a wire to Stripe funding the balance.
    ///
    /// Stripe calls it a top-up and reports it beside the payouts. The export
    /// this ledger has does not carry the category at all — thirty-seven files,
    /// not one topups row — but the dashboard does, so a later export will. Left
    /// unread it would not post quietly wrong: the balance check would fail and
    /// refuse the file. Read, it settles against the clearing account the bank
    /// side already books to, which is the same journey backwards.
    #[test]
    fn a_top_up_is_a_payout_in_reverse() {
        let csv = "\"category\",\"description\",\"net_amount\",\"currency\"\n\
\"starting_balance\",\"Starting balance\",\"0.00\",\"usd\"\n\
\"activity_gross\",\"Account activity before fees\",\"100.00\",\"usd\"\n\
\"activity_fee\",\"Less fees\",\"-5.00\",\"usd\"\n\
\"topups_gross\",\"Top-ups\",\"2000.00\",\"usd\"\n\
\"topups_fee\",\"Top-up fees\",\"0.00\",\"usd\"\n\
\"payouts_gross\",\"Payouts to bank\",\"-95.00\",\"usd\"\n\
\"payouts_fee\",\"Payout fees\",\"0.00\",\"usd\"\n\
\"ending_balance\",\"Ending balance\",\"2000.00\",\"usd\"\n";
        let s = parse_balance_summary(csv).unwrap();
        assert_eq!(s.topped_up(), 200000);
        assert_eq!(s.paid_out(), 9500);
        // It reconciles only because the top-up is counted.
        assert_eq!(s.starting + s.net_change(), s.ending);
        assert!(s.reconciliation.is_none(), "{:?}", s.reconciliation);

        let store = crate::store::event_store::EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        for (k, a) in [
            ("pos_stripe", "balance"),
            ("stripe_revenue", "revenue"),
            ("stripe_fees", "fees"),
            ("stripe_payouts_in_transit", "clearing"),
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
            csv,
            "Balance_Summary_USD_2024-06-01_to_2024-06-30_America-Chicago.csv",
        )
        .unwrap()
        .expect("an entry");
        assert_eq!(cmd.lines.iter().map(|l| l.amount).sum::<i64>(), 0, "balances");

        // The clearing account carries both directions on one line: $95.00 out
        // to the bank, $2,000.00 in from it.
        let clearing: i64 = cmd
            .lines
            .iter()
            .filter(|l| l.account_id == "clearing")
            .map(|l| l.amount)
            .sum();
        assert_eq!(clearing, 9500 - 200000, "a payout debits it, a top-up credits it");
        let of = |a: &str| cmd.lines.iter().find(|l| l.account_id == a).map(|l| l.amount);
        assert_eq!(of("balance"), Some(200000), "the balance rose by the top-up");
    }

    /// The payout leg must not go straight to checking.
    ///
    /// Stripe pays out several times a month and every one of those deposits
    /// also arrives through the bank feed. A monthly total debited to the same
    /// account would sit on top of the deposits it is made of, and nothing in
    /// the entry itself would look wrong — the damage shows up in a bank
    /// reconciliation weeks later. So it is refused at the point of import.
    #[test]
    fn a_payout_mapped_to_a_bank_feed_account_is_refused() {
        let store = crate::store::event_store::EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        let conn = store.connection();
        for (k, a) in [
            ("pos_stripe", "stripe-balance"),
            ("stripe_revenue", "revenue"),
            ("stripe_fees", "fees"),
            ("stripe_payouts_in_transit", "checking"),
        ] {
            conn.execute(
                "INSERT INTO ingest_account_mappings (key, account_id) VALUES (?1, ?2)",
                rusqlite::params![k, a],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO accounts (id, account_type, account_number, name) \
             VALUES ('checking', 'asset', '1001', 'Business Checking')",
            [],
        )
        .unwrap();
        let file = "Balance_Summary_USD_2026-07-01_to_2026-07-31_America-Chicago.csv";

        // With no bank connection there is nothing to collide with, so the same
        // mapping imports cleanly. This is the control: the guard has to fire on
        // the link, not on the key.
        assert!(plan_stripe(conn, SAMPLE, file).unwrap().is_some());

        conn.execute(
            "INSERT INTO plaid_items (id, institution_name) VALUES ('i1', 'Bank')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO plaid_local_accounts \
                 (item_id, plaid_account_id, name, account_type, local_account_id) \
             VALUES ('i1', 'p1', 'BUS COMPLETE CHK', 'depository', 'checking')",
            [],
        )
        .unwrap();

        let err = plan_stripe(conn, SAMPLE, file).expect_err("the double-count is refused");
        let msg = err.to_string();
        assert!(
            msg.contains("stripe_payouts_in_transit")
                && msg.contains("1001 Business Checking")
                && msg.contains("bank feed"),
            "the message has to name the mapping, the account, and the reason: {msg}"
        );
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
            ("stripe_payouts_in_transit", "bank"),
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
        assert_eq!(of("bank"), Some(18416), "the payout left Stripe for the bank");
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
