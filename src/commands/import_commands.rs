use chrono::NaiveDate;
use thiserror::Error;

use crate::commands::account_commands::find_or_create_uncategorized;
use crate::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use crate::domain::AccountType;
use crate::events::types::JournalEntrySource;
use crate::store::event_store::EventStore;

// ── CSV/bank file parsing utilities ──────────────────────────────────────────

/// Parse a delimited line, handling quoted fields.
pub fn parse_delimited_line(line: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        current.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                }
            }
            c if c == delimiter && !in_quotes => {
                fields.push(current.trim().to_string());
                current = String::new();
            }
            _ => {
                current.push(c);
            }
        }
    }

    fields.push(current.trim().to_string());
    fields
}

/// Parse a date string in various common formats.
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();

    for fmt in &[
        "%Y/%m/%d", "%Y-%m-%d", "%m/%d/%y", "%m-%d-%y", "%m/%d/%Y", "%m-%d-%Y",
    ] {
        if let Ok(date) = NaiveDate::parse_from_str(s, fmt) {
            return Some(date);
        }
    }

    None
}

/// Parse an amount string, handling currency symbols, commas, and parenthesized negatives.
pub fn parse_amount(s: &str) -> Option<i64> {
    let s = s.trim();

    let (is_negative, s) =
        if let Some(inner) = s.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            (true, inner)
        } else if let Some(rest) = s.strip_prefix('-') {
            (true, rest)
        } else {
            (false, s)
        };

    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();

    let value: f64 = cleaned.parse().ok()?;
    let cents = (value * 100.0).round() as i64;

    Some(if is_negative { -cents } else { cents })
}

// ── Import commands ──────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum ImportError {
    #[error("{0}")]
    General(String),
    #[error("Account error: {0}")]
    Account(#[from] crate::commands::account_commands::AccountCommandError),
}

/// Parameters for a CSV file import.
pub struct CsvImportParams {
    pub file_path: String,
    pub date_column: usize,
    pub description_column: usize,
    pub amount_column: usize,
    pub target_account_id: String,
    pub target_is_asset: bool,
    pub skip_lines: usize,
    pub has_header: bool,
    pub delimiter: char,
}

/// A parsed transaction ready for import (bank CSV or other source).
pub struct ImportTransaction {
    pub date: NaiveDate,
    pub description: String,
    pub amount: i64, // cents, positive = increase, negative = decrease
}

/// Import transactions from a CSV file.
/// Returns the number of successfully imported transactions.
pub fn import_csv(store: &mut EventStore, params: &CsvImportParams) -> Result<usize, ImportError> {
    let content = std::fs::read_to_string(&params.file_path)
        .map_err(|e| ImportError::General(format!("Failed to read file: {}", e)))?;

    let mut lines = content.lines();

    for _ in 0..params.skip_lines {
        lines.next();
    }
    if params.has_header {
        lines.next();
    }

    let uncategorized_id = find_or_create_uncategorized(store)?;

    let mut count = 0;
    let mut commands = EntryCommands::new(store, "csv-import".to_string());

    for line in lines {
        let fields = parse_delimited_line(line, params.delimiter);

        let date_str = fields
            .get(params.date_column)
            .map(|s| s.as_str())
            .unwrap_or("");
        let description = fields
            .get(params.description_column)
            .map(|s| s.as_str())
            .unwrap_or("");
        let amount_str = fields
            .get(params.amount_column)
            .map(|s| s.as_str())
            .unwrap_or("");

        let date = match parse_date(date_str) {
            Some(d) => d,
            None => continue,
        };

        let amount = match parse_amount(amount_str) {
            Some(a) if a != 0 => a,
            _ => continue,
        };

        let (target_amount, offset_amount) = if params.target_is_asset {
            (amount, -amount)
        } else {
            (-amount, amount)
        };

        let entry_lines = vec![
            EntryLine {
                account_id: params.target_account_id.clone(),
                amount: target_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
            EntryLine {
                account_id: uncategorized_id.clone(),
                amount: offset_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
        ];

        match commands.post_entry(PostEntryCommand {
            date,
            memo: description.to_string(),
            lines: entry_lines,
            reference: None,
            source: Some(JournalEntrySource::Import),
        }) {
            Ok(_) => count += 1,
            Err(e) => {
                eprintln!("Failed to import row: {}", e);
            }
        }
    }

    Ok(count)
}

/// Import bank transactions into the ledger.
/// Returns the number of successfully imported transactions.
pub fn import_bank_transactions(
    store: &mut EventStore,
    target_account_id: &str,
    target_account_type: AccountType,
    transactions: &[ImportTransaction],
) -> Result<usize, ImportError> {
    let uncategorized_id = find_or_create_uncategorized(store)?;

    let _is_asset = matches!(target_account_type, AccountType::Asset);

    let mut count = 0;
    let mut commands = EntryCommands::new(store, "bank-import".to_string());

    for txn in transactions {
        let target_amount = txn.amount;
        let offset_amount = -txn.amount;

        let entry_lines = vec![
            EntryLine {
                account_id: target_account_id.to_string(),
                amount: target_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
            EntryLine {
                account_id: uncategorized_id.clone(),
                amount: offset_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
        ];

        match commands.post_entry(PostEntryCommand {
            date: txn.date,
            memo: txn.description.clone(),
            lines: entry_lines,
            reference: None,
            source: Some(JournalEntrySource::Import),
        }) {
            Ok(_) => count += 1,
            Err(e) => {
                eprintln!("Failed to import transaction: {}", e);
            }
        }
    }

    Ok(count)
}

/// Mark a bank import as processed and optionally save the bank-account mapping.
pub fn finalize_bank_import(
    store: &EventStore,
    import_id: i64,
    account_id: &str,
    save_mapping: bool,
    imported_count: usize,
) {
    let conn = store.connection();

    if save_mapping {
        let bank_info: Option<(Option<String>, String)> = conn
            .query_row(
                "SELECT bank_id, bank_name FROM pending_imports WHERE id = ?1",
                [import_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();

        if let Some((Some(bank_id), bank_name)) = bank_info {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO bank_accounts (bank_id, bank_name, account_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![bank_id, bank_name, account_id],
            );
        }
    }

    let _ = conn.execute(
        "UPDATE pending_imports SET status = 'imported', imported_count = ?1, processed_at = datetime('now') WHERE id = ?2",
        rusqlite::params![imported_count as i64, import_id],
    );
}
