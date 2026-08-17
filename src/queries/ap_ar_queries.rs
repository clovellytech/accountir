use chrono::NaiveDate;
use rusqlite::{params, Connection};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ApArQueryError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
pub struct BillRow {
    pub id: String,
    pub vendor: String,
    pub amount: i64,
    pub amount_paid: i64,
    pub status: String,
    pub due_date: String,
    pub memo: Option<String>,
    pub entry_id: String,
}

#[derive(Debug, Clone)]
pub struct InvoiceRow {
    pub id: String,
    pub customer: String,
    pub amount: i64,
    pub amount_paid: i64,
    pub status: String,
    pub due_date: String,
    pub memo: Option<String>,
    pub entry_id: String,
}

/// One line of a vendor's payable history, and where it currently sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayableLine {
    pub entry_id: String,
    pub line_id: String,
    pub account_id: String,
    pub account_name: String,
    /// Signed as the ledger holds it: negative on a bill (a credit, the debt
    /// taken on), positive on a payment (a debit, the debt cleared).
    pub amount: i64,
    pub is_payment: bool,
}

/// A vendor as the payables page lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorSummary {
    pub vendor: String,
    /// Billed less paid, in cents.
    pub outstanding: i64,
    pub bills: usize,
    /// The payable account(s) this vendor's bills currently credit. More than one
    /// means the history is split across accounts.
    pub accounts: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct AgingBucket {
    pub current: i64,
    pub days_1_30: i64,
    pub days_31_60: i64,
    pub days_61_90: i64,
    pub days_over_90: i64,
    pub total: i64,
}

pub struct ApArQueries<'a> {
    conn: &'a Connection,
}

impl<'a> ApArQueries<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Open + partial bills, sorted by due_date ASC
    pub fn open_bills(&self) -> Result<Vec<BillRow>, ApArQueryError> {
        self.list_bills_where("status IN ('open', 'partial')")
    }

    /// All bills matching a status filter (None = all)
    pub fn list_bills(&self, status: Option<&str>) -> Result<Vec<BillRow>, ApArQueryError> {
        match status {
            Some(s) => self.list_bills_where(&format!("status = '{}'", s)),
            None => self.list_bills_where("1=1"),
        }
    }

    fn list_bills_where(&self, where_clause: &str) -> Result<Vec<BillRow>, ApArQueryError> {
        let sql = format!(
            "SELECT id, vendor, amount, amount_paid, status, due_date, memo, entry_id
             FROM bills WHERE {} ORDER BY due_date ASC",
            where_clause
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(BillRow {
                    id: row.get(0)?,
                    vendor: row.get(1)?,
                    amount: row.get(2)?,
                    amount_paid: row.get(3)?,
                    status: row.get(4)?,
                    due_date: row.get(5)?,
                    memo: row.get(6)?,
                    entry_id: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One vendor's payable lines: the leg of each of their bills — and of every
    /// payment against those bills — that sits in a payable account.
    ///
    /// # Why this has to be derived rather than looked up
    ///
    /// `BillReceived` does not carry the payable account, and neither does the
    /// `bills` table. The account exists only as a *line* of the bill's journal
    /// entry, so the only way to answer "which account is this vendor's AP in?" is
    /// to read the entry back.
    ///
    /// # Which line is the payable one
    ///
    /// A bill posts a debit to an expense account and a credit to a payable; a
    /// payment posts a debit to that payable and a credit to whatever funded it.
    /// So the payable leg is identified by *side*, not by account type alone —
    /// paying a bill from a credit card credits a liability too, and picking "the
    /// liability line" would move the card instead of the payable.
    ///
    /// A payment's payable line is further anchored to the account its own bill
    /// used, so a bill and its payments always move together. Half-moving them
    /// would leave a vendor account holding a liability that its payment never
    /// clears.
    ///
    /// Voided entries are excluded: their lines net to nothing and rewriting them
    /// changes no balance, but it does rewrite history somebody already corrected.
    pub fn vendor_payable_lines(&self, vendor: &str) -> Result<Vec<PayableLine>, ApArQueryError> {
        let mut out = Vec::new();

        // The bills, and the payable account each one credited.
        let mut bills = self.conn.prepare(
            "SELECT b.id, b.entry_id, jl.id, jl.account_id, a.name, jl.amount
               FROM bills b
               JOIN journal_entries je ON je.id = b.entry_id AND je.is_void = 0
               JOIN journal_lines jl   ON jl.entry_id = b.entry_id
               JOIN accounts a         ON a.id = jl.account_id
              WHERE b.vendor = ?1
                AND a.account_type = 'liability'
                -- The credit side: what the vendor is owed.
                AND jl.amount < 0
              ORDER BY b.due_date",
        )?;
        let rows = bills
            .query_map([vendor], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    PayableLine {
                        entry_id: r.get(1)?,
                        line_id: r.get(2)?,
                        account_id: r.get(3)?,
                        account_name: r.get(4)?,
                        amount: r.get(5)?,
                        is_payment: false,
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Each bill's payments, restricted to the account that bill used.
        let mut payments = self.conn.prepare(
            "SELECT jl.entry_id, jl.id, jl.account_id, a.name, jl.amount
               FROM bill_payments bp
               JOIN journal_entries je ON je.id = bp.payment_entry_id AND je.is_void = 0
               JOIN journal_lines jl   ON jl.entry_id = bp.payment_entry_id
               JOIN accounts a         ON a.id = jl.account_id
              WHERE bp.bill_id = ?1
                AND jl.account_id = ?2
                -- The debit side: what the payment clears.
                AND jl.amount > 0",
        )?;
        for (bill_id, line) in rows {
            let account = line.account_id.clone();
            out.push(line);
            let paid = payments
                .query_map(rusqlite::params![bill_id, account], |r| {
                    Ok(PayableLine {
                        entry_id: r.get(0)?,
                        line_id: r.get(1)?,
                        account_id: r.get(2)?,
                        account_name: r.get(3)?,
                        amount: r.get(4)?,
                        is_payment: true,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            out.extend(paid);
        }
        Ok(out)
    }

    /// Every vendor with at least one bill, with what they are owed and which
    /// payable account(s) their bills currently sit in.
    ///
    /// More than one account is a state worth showing rather than hiding: it means
    /// this vendor's history is split, which is exactly what someone linking them
    /// to a single account is trying to fix.
    pub fn vendors(&self) -> Result<Vec<VendorSummary>, ApArQueryError> {
        let mut stmt = self.conn.prepare(
            "SELECT vendor, SUM(amount), SUM(amount_paid), COUNT(*)
               FROM bills
              WHERE status != 'void'
              GROUP BY vendor
              ORDER BY vendor",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut out = Vec::new();
        for (vendor, billed, paid, bills) in rows {
            let lines = self.vendor_payable_lines(&vendor)?;
            let mut accounts: Vec<(String, String)> = lines
                .iter()
                .filter(|l| !l.is_payment)
                .map(|l| (l.account_id.clone(), l.account_name.clone()))
                .collect();
            accounts.sort();
            accounts.dedup();
            out.push(VendorSummary {
                vendor,
                outstanding: billed - paid,
                bills: bills as usize,
                accounts,
            });
        }
        Ok(out)
    }

    /// Open + partial invoices, sorted by due_date ASC
    pub fn open_invoices(&self) -> Result<Vec<InvoiceRow>, ApArQueryError> {
        self.list_invoices_where("status IN ('open', 'partial')")
    }

    /// All invoices matching a status filter (None = all)
    pub fn list_invoices(&self, status: Option<&str>) -> Result<Vec<InvoiceRow>, ApArQueryError> {
        match status {
            Some(s) => self.list_invoices_where(&format!("status = '{}'", s)),
            None => self.list_invoices_where("1=1"),
        }
    }

    fn list_invoices_where(&self, where_clause: &str) -> Result<Vec<InvoiceRow>, ApArQueryError> {
        let sql = format!(
            "SELECT id, customer, amount, amount_paid, status, due_date, memo, entry_id
             FROM invoices WHERE {} ORDER BY due_date ASC",
            where_clause
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(InvoiceRow {
                    id: row.get(0)?,
                    customer: row.get(1)?,
                    amount: row.get(2)?,
                    amount_paid: row.get(3)?,
                    status: row.get(4)?,
                    due_date: row.get(5)?,
                    memo: row.get(6)?,
                    entry_id: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// AP aging report as of a given date
    pub fn ap_aging(&self, as_of: NaiveDate) -> Result<AgingBucket, ApArQueryError> {
        let as_of_str = as_of.to_string();
        let row = self.conn.query_row(
            "SELECT
                COALESCE(SUM(CASE WHEN julianday(?1) < julianday(due_date) THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) >= julianday(due_date) AND julianday(?1) - julianday(due_date) <= 30 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) - julianday(due_date) > 30 AND julianday(?1) - julianday(due_date) <= 60 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) - julianday(due_date) > 60 AND julianday(?1) - julianday(due_date) <= 90 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) - julianday(due_date) > 90 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(amount - amount_paid), 0)
             FROM bills WHERE status IN ('open', 'partial')",
            params![as_of_str],
            |row| {
                Ok(AgingBucket {
                    current: row.get(0)?,
                    days_1_30: row.get(1)?,
                    days_31_60: row.get(2)?,
                    days_61_90: row.get(3)?,
                    days_over_90: row.get(4)?,
                    total: row.get(5)?,
                })
            },
        )?;
        Ok(row)
    }

    /// AR aging report as of a given date
    pub fn ar_aging(&self, as_of: NaiveDate) -> Result<AgingBucket, ApArQueryError> {
        let as_of_str = as_of.to_string();
        let row = self.conn.query_row(
            "SELECT
                COALESCE(SUM(CASE WHEN julianday(?1) < julianday(due_date) THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) >= julianday(due_date) AND julianday(?1) - julianday(due_date) <= 30 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) - julianday(due_date) > 30 AND julianday(?1) - julianday(due_date) <= 60 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) - julianday(due_date) > 60 AND julianday(?1) - julianday(due_date) <= 90 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN julianday(?1) - julianday(due_date) > 90 THEN amount - amount_paid ELSE 0 END), 0),
                COALESCE(SUM(amount - amount_paid), 0)
             FROM invoices WHERE status IN ('open', 'partial')",
            params![as_of_str],
            |row| {
                Ok(AgingBucket {
                    current: row.get(0)?,
                    days_1_30: row.get(1)?,
                    days_31_60: row.get(2)?,
                    days_61_90: row.get(3)?,
                    days_over_90: row.get(4)?,
                    total: row.get(5)?,
                })
            },
        )?;
        Ok(row)
    }
}

#[cfg(test)]
mod vendor_payable_tests {
    use super::*;
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::commands::bill_commands::{
        ApplyBillPaymentCommand, BillCommands, ReceiveBillCommand,
    };
    use crate::domain::{AccountType, PaymentTerms};
    use crate::events::types::Event;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    fn mk(store: &mut EventStore, num: &str, ty: AccountType, name: &str) -> String {
        let stored = AccountCommands::new(store, "seed".to_string())
            .create_account(CreateAccountCommand {
                account_type: ty,
                account_number: num.to_string(),
                name: name.to_string(),
                parent_id: None,
                currency: Some("USD".to_string()),
                description: None,
            })
            .unwrap();
        match &stored.event {
            Event::AccountCreated { account_id, .. } => account_id.clone(),
            _ => unreachable!(),
        }
    }

    /// A bill from one vendor, paid **from a credit card** — so the payment entry
    /// carries two liability lines. That is the case a naive "find the liability
    /// line" rule gets wrong, moving the card instead of the payable.
    #[test]
    fn a_payment_from_a_credit_card_does_not_confuse_the_payable_leg() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let expense = mk(&mut store, "5000", AccountType::Expense, "Supplies");
        let ap = mk(
            &mut store,
            "2000",
            AccountType::Liability,
            "Accounts payable",
        );
        let card = mk(&mut store, "2100", AccountType::Liability, "Business card");

        let stored = BillCommands::new(&mut store, "t".to_string())
            .receive_bill(ReceiveBillCommand {
                vendor: "Quality Bicycle".to_string(),
                amount: 5000,
                currency: "USD".to_string(),
                issue_date: chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                debit_account_id: expense,
                ap_account_id: ap.clone(),
                reference: None,
            })
            .unwrap();
        let bill_id = match &stored.event {
            Event::BillReceived { bill_id, .. } => bill_id.clone(),
            other => panic!("expected a bill, got {other:?}"),
        };

        BillCommands::new(&mut store, "t".to_string())
            .apply_payment(ApplyBillPaymentCommand {
                bill_id,
                payment_date: chrono::NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(),
                amount_applied: 2000,
                payment_account_id: card.clone(),
                ap_account_id: ap.clone(),
                memo: None,
            })
            .unwrap();

        let lines = ApArQueries::new(store.connection())
            .vendor_payable_lines("Quality Bicycle")
            .unwrap();

        assert_eq!(
            lines.len(),
            2,
            "expected the bill and its payment: {lines:?}"
        );
        assert!(
            lines.iter().all(|l| l.account_id == ap),
            "a line outside the payable account was picked up — the credit card \
             leg was probably mistaken for the payable: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| !l.is_payment && l.amount < 0),
            "the bill's credit is missing: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.is_payment && l.amount > 0),
            "the payment's debit is missing — moving a bill without its payment \
             leaves a vendor account that never clears: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.account_id == card),
            "the credit card was included: {lines:?}"
        );
    }

    /// The outcome the whole feature exists for: after moving a vendor's payable
    /// lines, that vendor's balance is in the account they were linked to, and the
    /// account it came from no longer carries it.
    ///
    /// Asserted on account *balances* rather than on line rows, because balances
    /// are what the user is looking at when they say the bills "aren't included in
    /// those accounts payable accounts".
    #[test]
    fn moving_a_vendor_puts_their_balance_in_the_linked_account() {
        use crate::commands::entry_commands::{EntryCommands, ReassignLineCommand};
        use crate::queries::account_queries::AccountQueries;

        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let expense = mk(&mut store, "5000", AccountType::Expense, "Supplies");
        let cash = mk(&mut store, "1000", AccountType::Asset, "Cash");
        let ap = mk(
            &mut store,
            "2000",
            AccountType::Liability,
            "Accounts payable",
        );
        let qbp = mk(&mut store, "2010", AccountType::Liability, "QBP payable");

        let stored = BillCommands::new(&mut store, "t".to_string())
            .receive_bill(ReceiveBillCommand {
                vendor: "Quality Bicycle".to_string(),
                amount: 5000,
                currency: "USD".to_string(),
                issue_date: chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
                terms: PaymentTerms::Net { days: 30 },
                memo: None,
                debit_account_id: expense,
                ap_account_id: ap.clone(),
                reference: None,
            })
            .unwrap();
        let bill_id = match &stored.event {
            Event::BillReceived { bill_id, .. } => bill_id.clone(),
            other => panic!("expected a bill, got {other:?}"),
        };
        BillCommands::new(&mut store, "t".to_string())
            .apply_payment(ApplyBillPaymentCommand {
                bill_id,
                payment_date: chrono::NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(),
                amount_applied: 2000,
                payment_account_id: cash,
                ap_account_id: ap.clone(),
                memo: None,
            })
            .unwrap();

        let balance = |store: &EventStore, acct: &str| {
            AccountQueries::new(store.connection())
                .get_account_balance(acct, None)
                .map(|b| b.balance)
                .unwrap_or(0)
        };
        let owed_before = balance(&store, &ap);
        assert_ne!(owed_before, 0, "the fixture owes nothing to begin with");
        assert_eq!(balance(&store, &qbp), 0);

        // What the Payables page does on "Link and move".
        let lines = ApArQueries::new(store.connection())
            .vendor_payable_lines("Quality Bicycle")
            .unwrap();
        for line in lines {
            EntryCommands::new(&mut store, "t".to_string())
                .reassign_line(ReassignLineCommand {
                    entry_id: line.entry_id,
                    line_id: line.line_id,
                    new_account_id: qbp.clone(),
                })
                .unwrap();
        }

        assert_eq!(
            balance(&store, &qbp),
            owed_before,
            "the vendor's balance did not land in the account they were linked to"
        );
        assert_eq!(
            balance(&store, &ap),
            0,
            "the generic payable still carries this vendor — the bill moved but \
             its payment did not, or the other way round"
        );
        // …and the page now reports them where they actually are.
        let vendors = ApArQueries::new(store.connection()).vendors().unwrap();
        let qb = vendors
            .iter()
            .find(|v| v.vendor == "Quality Bicycle")
            .unwrap();
        assert_eq!(qb.accounts, vec![(qbp, "QBP payable".to_string())]);
    }

    /// The listing a payables page shows, including a vendor whose history is
    /// split across two payable accounts — the state someone linking a vendor is
    /// trying to fix, so it has to be visible rather than collapsed.
    #[test]
    fn vendors_report_what_they_owe_and_where_it_sits() {
        let mut store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let expense = mk(&mut store, "5000", AccountType::Expense, "Supplies");
        let ap = mk(
            &mut store,
            "2000",
            AccountType::Liability,
            "Accounts payable",
        );
        let qbp = mk(&mut store, "2010", AccountType::Liability, "QBP payable");

        let mut bill = |vendor: &str, amount: i64, ap_id: &str| {
            BillCommands::new(&mut store, "t".to_string())
                .receive_bill(ReceiveBillCommand {
                    vendor: vendor.to_string(),
                    amount,
                    currency: "USD".to_string(),
                    issue_date: chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
                    terms: PaymentTerms::Net { days: 30 },
                    memo: None,
                    debit_account_id: expense.clone(),
                    ap_account_id: ap_id.to_string(),
                    reference: Some(format!("{vendor}-{amount}")),
                })
                .unwrap();
        };
        bill("Quality Bicycle", 5000, &ap);
        bill("Quality Bicycle", 3000, &qbp);
        bill("Shimano", 1000, &ap);

        let vendors = ApArQueries::new(store.connection()).vendors().unwrap();
        let qb = vendors
            .iter()
            .find(|v| v.vendor == "Quality Bicycle")
            .unwrap();
        assert_eq!(qb.outstanding, 8000);
        assert_eq!(qb.bills, 2);
        assert_eq!(
            qb.accounts.len(),
            2,
            "a vendor split across two payable accounts must show as split: {:?}",
            qb.accounts
        );

        let shimano = vendors.iter().find(|v| v.vendor == "Shimano").unwrap();
        assert_eq!(shimano.accounts.len(), 1);
        assert_eq!(shimano.outstanding, 1000);
    }
}
