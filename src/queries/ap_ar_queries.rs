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
