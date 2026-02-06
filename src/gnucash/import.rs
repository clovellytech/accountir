use crate::events::types::{
    Event, EventAccountType, EventEnvelope, JournalEntrySource, JournalLineData,
};
use crate::gnucash::{fraction_to_cents, GncBook, GnuCashError};
use crate::store::event_store::EventStore;
use crate::store::projections::Projector;
use std::collections::{HashMap, HashSet, VecDeque};
use uuid::Uuid;

/// Summary of an import operation
#[derive(Debug)]
pub struct ImportSummary {
    pub currencies_imported: usize,
    pub accounts_imported: usize,
    pub accounts_skipped: usize,
    pub transactions_imported: usize,
    pub transactions_skipped: usize,
    pub total_splits: usize,
    pub total_events: usize,
    pub warnings: Vec<String>,
}

/// Map GnuCash account type to accountir EventAccountType
fn map_account_type(gnc_type: &str) -> Option<EventAccountType> {
    match gnc_type {
        "ASSET" | "BANK" | "RECEIVABLE" | "CREDIT" | "CASH" | "MUTUAL" | "STOCK" => {
            Some(EventAccountType::Asset)
        }
        "LIABILITY" | "PAYABLE" => Some(EventAccountType::Liability),
        "EQUITY" => Some(EventAccountType::Equity),
        "INCOME" => Some(EventAccountType::Revenue),
        "EXPENSE" => Some(EventAccountType::Expense),
        "ROOT" => None, // Skip ROOT accounts
        _ => None,
    }
}

/// Get currency name and symbol for common currencies
fn currency_info(code: &str) -> (&str, &str) {
    match code {
        "USD" => ("US Dollar", "$"),
        "EUR" => ("Euro", "\u{20AC}"),
        "GBP" => ("British Pound", "\u{00A3}"),
        "JPY" => ("Japanese Yen", "\u{00A5}"),
        "CAD" => ("Canadian Dollar", "CA$"),
        "AUD" => ("Australian Dollar", "A$"),
        "CHF" => ("Swiss Franc", "CHF"),
        "CNY" => ("Chinese Yuan", "\u{00A5}"),
        "SEK" => ("Swedish Krona", "kr"),
        "NZD" => ("New Zealand Dollar", "NZ$"),
        "MXN" => ("Mexican Peso", "MX$"),
        "BRL" => ("Brazilian Real", "R$"),
        "INR" => ("Indian Rupee", "\u{20B9}"),
        "KRW" => ("South Korean Won", "\u{20A9}"),
        _ => (code, code),
    }
}

/// Topological sort of accounts (parents before children) using Kahn's algorithm
fn topological_sort_accounts(accounts: &[crate::gnucash::GncAccount]) -> Vec<usize> {
    let n = accounts.len();
    let guid_to_idx: HashMap<&str, usize> = accounts
        .iter()
        .enumerate()
        .map(|(i, a)| (a.guid.as_str(), i))
        .collect();

    // Build adjacency: parent -> children
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut in_degree = vec![0usize; n];

    for (i, acc) in accounts.iter().enumerate() {
        if let Some(ref parent_guid) = acc.parent_guid {
            if let Some(&parent_idx) = guid_to_idx.get(parent_guid.as_str()) {
                children.entry(parent_idx).or_default().push(i);
                in_degree[i] += 1;
            }
        }
    }

    // Start with accounts that have no parent (or parent not in our set)
    let mut queue: VecDeque<usize> = VecDeque::new();
    for i in 0..n {
        if in_degree[i] == 0 {
            queue.push_back(i);
        }
    }

    let mut order = Vec::with_capacity(n);
    while let Some(idx) = queue.pop_front() {
        order.push(idx);
        if let Some(kids) = children.get(&idx) {
            for &child in kids {
                in_degree[child] -= 1;
                if in_degree[child] == 0 {
                    queue.push_back(child);
                }
            }
        }
    }

    // Any remaining accounts (cycles, shouldn't happen) — append them
    if order.len() < n {
        let in_order: HashSet<usize> = order.iter().copied().collect();
        for i in 0..n {
            if !in_order.contains(&i) {
                order.push(i);
            }
        }
    }

    order
}

/// Import a parsed GnuCash book into the event store
pub fn import_gnucash(
    book: &GncBook,
    store: &mut EventStore,
    company_name: &str,
) -> Result<ImportSummary, GnuCashError> {
    let mut summary = ImportSummary {
        currencies_imported: 0,
        accounts_imported: 0,
        accounts_skipped: 0,
        transactions_imported: 0,
        transactions_skipped: 0,
        total_splits: 0,
        total_events: 0,
        warnings: Vec::new(),
    };

    let user_id = "gnucash-import".to_string();

    // Determine base currency from first CURRENCY commodity
    let base_currency = book
        .commodities
        .iter()
        .find(|c| c.space == "CURRENCY")
        .map(|c| c.id.clone())
        .unwrap_or_else(|| "USD".to_string());

    // Wrap in a SQLite transaction for performance
    store
        .connection()
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| GnuCashError::EventStore(e.to_string()))?;

    let result = import_inner(
        book,
        store,
        company_name,
        &base_currency,
        &user_id,
        &mut summary,
    );

    match result {
        Ok(()) => {
            store
                .connection()
                .execute_batch("COMMIT")
                .map_err(|e| GnuCashError::EventStore(e.to_string()))?;
            Ok(summary)
        }
        Err(e) => {
            let _ = store.connection().execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn import_inner(
    book: &GncBook,
    store: &mut EventStore,
    company_name: &str,
    base_currency: &str,
    user_id: &str,
    summary: &mut ImportSummary,
) -> Result<(), GnuCashError> {
    // 1. CompanyCreated
    let company_event = Event::CompanyCreated {
        company_id: Uuid::new_v4().to_string(),
        name: company_name.to_string(),
        base_currency: base_currency.to_string(),
        fiscal_year_start: 1,
    };
    append_and_project(store, company_event, user_id)?;
    summary.total_events += 1;

    // 2. CurrencyEnabled for each CURRENCY commodity
    let mut seen_currencies = HashSet::new();
    for commodity in &book.commodities {
        if commodity.space == "CURRENCY" && seen_currencies.insert(commodity.id.clone()) {
            let (name, symbol) = currency_info(&commodity.id);
            let currency_event = Event::CurrencyEnabled {
                code: commodity.id.clone(),
                name: name.to_string(),
                symbol: symbol.to_string(),
                decimal_places: 2,
            };
            append_and_project(store, currency_event, user_id)?;
            summary.currencies_imported += 1;
            summary.total_events += 1;
        }
    }

    // Also check account commodities for currencies not in the commodity list
    for acc in &book.accounts {
        if let Some(ref commodity) = acc.commodity {
            if commodity.space == "CURRENCY" && seen_currencies.insert(commodity.id.clone()) {
                let (name, symbol) = currency_info(&commodity.id);
                let currency_event = Event::CurrencyEnabled {
                    code: commodity.id.clone(),
                    name: name.to_string(),
                    symbol: symbol.to_string(),
                    decimal_places: 2,
                };
                append_and_project(store, currency_event, user_id)?;
                summary.currencies_imported += 1;
                summary.total_events += 1;
            }
        }
    }

    // 3. AccountCreated — topological sort, then create
    let sorted_indices = topological_sort_accounts(&book.accounts);

    // Account number counters per type
    let mut account_number_counters: HashMap<String, u32> = HashMap::new();
    account_number_counters.insert("Asset".to_string(), 1000);
    account_number_counters.insert("Liability".to_string(), 2000);
    account_number_counters.insert("Equity".to_string(), 3000);
    account_number_counters.insert("Revenue".to_string(), 4000);
    account_number_counters.insert("Expense".to_string(), 5000);

    // Maps: gnucash guid -> new account UUID
    let mut guid_to_account_id: HashMap<String, String> = HashMap::new();
    // Track ROOT account guids so we can set parent_id = None for their children
    let mut root_guids: HashSet<String> = HashSet::new();

    for &idx in &sorted_indices {
        let acc = &book.accounts[idx];

        if acc.account_type == "ROOT" {
            root_guids.insert(acc.guid.clone());
            summary.accounts_skipped += 1;
            continue;
        }

        let acc_type = match map_account_type(&acc.account_type) {
            Some(t) => t,
            None => {
                summary.warnings.push(format!(
                    "Skipping account '{}' with unknown type '{}'",
                    acc.name, acc.account_type
                ));
                summary.accounts_skipped += 1;
                continue;
            }
        };

        let type_key = format!("{:?}", acc_type);
        let counter = account_number_counters
            .entry(type_key.clone())
            .or_insert(1000);
        let account_number = format!("{}", *counter);
        *counter += 1;

        let account_id = Uuid::new_v4().to_string();
        guid_to_account_id.insert(acc.guid.clone(), account_id.clone());

        // Resolve parent: if parent was ROOT, set to None
        let parent_id = acc.parent_guid.as_ref().and_then(|pg| {
            if root_guids.contains(pg) {
                None
            } else {
                guid_to_account_id.get(pg).cloned()
            }
        });

        let currency = acc
            .commodity
            .as_ref()
            .filter(|c| c.space == "CURRENCY")
            .map(|c| c.id.clone());

        let description = if acc.description.is_empty() {
            None
        } else {
            Some(acc.description.clone())
        };

        let account_event = Event::AccountCreated {
            account_id,
            account_type: acc_type,
            account_number,
            name: acc.name.clone(),
            parent_id,
            currency,
            description,
        };
        append_and_project(store, account_event, user_id)?;
        summary.accounts_imported += 1;
        summary.total_events += 1;
    }

    // 4. JournalEntryPosted — sorted by date_posted then date_entered
    let mut txn_indices: Vec<usize> = (0..book.transactions.len()).collect();
    txn_indices.sort_by(|&a, &b| {
        let ta = &book.transactions[a];
        let tb = &book.transactions[b];
        ta.date_posted
            .cmp(&tb.date_posted)
            .then(ta.date_entered.cmp(&tb.date_entered))
    });

    for &idx in &txn_indices {
        let txn = &book.transactions[idx];
        summary.total_splits += txn.splits.len();

        // Convert splits to journal lines, filtering out ROOT references
        let mut lines: Vec<JournalLineData> = Vec::new();
        let mut skipped_root_split = false;

        for split in &txn.splits {
            let account_id = match guid_to_account_id.get(&split.account_guid) {
                Some(id) => id.clone(),
                None => {
                    // This split references an unmapped account (ROOT or unknown)
                    skipped_root_split = true;
                    continue;
                }
            };

            let amount = fraction_to_cents(split.value_num, split.value_denom);

            let memo = if split.memo.is_empty() {
                None
            } else {
                Some(split.memo.clone())
            };

            lines.push(JournalLineData {
                line_id: Uuid::new_v4().to_string(),
                account_id,
                amount,
                currency: txn.currency.id.clone(),
                exchange_rate: None,
                memo,
            });
        }

        // Verify: need >= 2 lines and balanced
        if lines.len() < 2 {
            if skipped_root_split {
                summary.warnings.push(format!(
                    "Skipping transaction '{}' ({}) — too few lines after filtering ROOT splits",
                    txn.description, txn.guid
                ));
            } else {
                summary.warnings.push(format!(
                    "Skipping transaction '{}' ({}) — fewer than 2 lines",
                    txn.description, txn.guid
                ));
            }
            summary.transactions_skipped += 1;
            continue;
        }

        let balance: i64 = lines.iter().map(|l| l.amount).sum();
        if balance != 0 {
            summary.warnings.push(format!(
                "Skipping unbalanced transaction '{}' ({}) — off by {} cents",
                txn.description, txn.guid, balance
            ));
            summary.transactions_skipped += 1;
            continue;
        }

        let memo = if txn.description.is_empty() {
            "(no description)".to_string()
        } else {
            txn.description.clone()
        };

        let reference = if txn.num.is_empty() {
            None
        } else {
            Some(txn.num.clone())
        };

        let entry_event = Event::JournalEntryPosted {
            entry_id: Uuid::new_v4().to_string(),
            date: txn.date_posted,
            memo,
            lines,
            reference,
            source: Some(JournalEntrySource::Import),
        };

        // Use with_timestamp to preserve date_entered as event timestamp
        let envelope =
            EventEnvelope::with_timestamp(entry_event, user_id.to_string(), txn.date_entered);
        let stored = store
            .append(envelope)
            .map_err(|e| GnuCashError::EventStore(e.to_string()))?;
        let projector = Projector::new(store.connection());
        projector
            .apply(&stored)
            .map_err(|e| GnuCashError::EventStore(e.to_string()))?;

        summary.transactions_imported += 1;
        summary.total_events += 1;
    }

    Ok(())
}

fn append_and_project(
    store: &mut EventStore,
    event: Event,
    user_id: &str,
) -> Result<(), GnuCashError> {
    let envelope = EventEnvelope::new(event, user_id.to_string());
    let stored = store
        .append(envelope)
        .map_err(|e| GnuCashError::EventStore(e.to_string()))?;
    let projector = Projector::new(store.connection());
    projector
        .apply(&stored)
        .map_err(|e| GnuCashError::EventStore(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_account_type() {
        assert_eq!(map_account_type("ASSET"), Some(EventAccountType::Asset));
        assert_eq!(map_account_type("BANK"), Some(EventAccountType::Asset));
        assert_eq!(
            map_account_type("RECEIVABLE"),
            Some(EventAccountType::Asset)
        );
        assert_eq!(map_account_type("CREDIT"), Some(EventAccountType::Asset));
        assert_eq!(
            map_account_type("LIABILITY"),
            Some(EventAccountType::Liability)
        );
        assert_eq!(
            map_account_type("PAYABLE"),
            Some(EventAccountType::Liability)
        );
        assert_eq!(map_account_type("EQUITY"), Some(EventAccountType::Equity));
        assert_eq!(map_account_type("INCOME"), Some(EventAccountType::Revenue));
        assert_eq!(map_account_type("EXPENSE"), Some(EventAccountType::Expense));
        assert_eq!(map_account_type("ROOT"), None);
    }

    #[test]
    fn test_topological_sort() {
        use crate::gnucash::GncAccount;

        let accounts = vec![
            GncAccount {
                guid: "child".to_string(),
                name: "Child".to_string(),
                account_type: "ASSET".to_string(),
                commodity: None,
                description: String::new(),
                parent_guid: Some("parent".to_string()),
                is_placeholder: false,
            },
            GncAccount {
                guid: "parent".to_string(),
                name: "Parent".to_string(),
                account_type: "ASSET".to_string(),
                commodity: None,
                description: String::new(),
                parent_guid: None,
                is_placeholder: false,
            },
            GncAccount {
                guid: "grandchild".to_string(),
                name: "Grandchild".to_string(),
                account_type: "ASSET".to_string(),
                commodity: None,
                description: String::new(),
                parent_guid: Some("child".to_string()),
                is_placeholder: false,
            },
        ];

        let order = topological_sort_accounts(&accounts);
        // parent (idx=1) should come before child (idx=0), which should come before grandchild (idx=2)
        let parent_pos = order.iter().position(|&i| i == 1).unwrap();
        let child_pos = order.iter().position(|&i| i == 0).unwrap();
        let grandchild_pos = order.iter().position(|&i| i == 2).unwrap();

        assert!(parent_pos < child_pos);
        assert!(child_pos < grandchild_pos);
    }

    #[test]
    fn test_currency_info() {
        let (name, symbol) = currency_info("USD");
        assert_eq!(name, "US Dollar");
        assert_eq!(symbol, "$");

        let (name, symbol) = currency_info("XYZ");
        assert_eq!(name, "XYZ");
        assert_eq!(symbol, "XYZ");
    }
}
