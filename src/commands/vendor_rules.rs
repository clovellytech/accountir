//! Vendor → payable account rules. A counterparty name (bank merchant or event
//! supplier) is matched against a list of `pattern → account` rules so postings
//! route to a per-vendor payable account instead of one lumped AP. Matching is
//! case-insensitive substring; the longest matching pattern wins (most specific).

use rusqlite::Connection;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct VendorRule {
    pub id: String,
    pub pattern: String,
    pub account_id: String,
}

pub fn list_rules(conn: &Connection) -> Vec<VendorRule> {
    let mut out = Vec::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT id, pattern, account_id FROM vendor_account_rules ORDER BY pattern")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok(VendorRule {
                id: r.get(0)?,
                pattern: r.get(1)?,
                account_id: r.get(2)?,
            })
        }) {
            out.extend(rows.flatten());
        }
    }
    out
}

pub fn add_rule(conn: &Connection, pattern: &str, account_id: &str) -> Result<String, rusqlite::Error> {
    let id = Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO vendor_account_rules (id, pattern, account_id, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))",
        rusqlite::params![id, pattern.trim(), account_id],
    )?;
    Ok(id)
}

pub fn delete_rule(conn: &Connection, id: &str) -> Result<(), rusqlite::Error> {
    conn.execute("DELETE FROM vendor_account_rules WHERE id = ?1", [id])?;
    Ok(())
}

/// The payable account for a counterparty `name`: the longest rule pattern that
/// appears (case-insensitively) within `name`, or `None` if nothing matches.
pub fn match_account(conn: &Connection, name: &str) -> Option<String> {
    let name_lc = name.to_lowercase();
    let mut best: Option<(usize, String)> = None;
    for rule in list_rules(conn) {
        let p = rule.pattern.trim().to_lowercase();
        if !p.is_empty() && name_lc.contains(&p) {
            let len = p.chars().count();
            if best.as_ref().map(|(l, _)| len > *l).unwrap_or(true) {
                best = Some((len, rule.account_id));
            }
        }
    }
    best.map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    #[test]
    fn matches_longest_pattern_case_insensitively() {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        let conn = store.connection();
        add_rule(conn, "quality bicycle", "acct-qbp").unwrap();
        add_rule(conn, "quality bicycle products co", "acct-qbp-specific").unwrap();
        add_rule(conn, "bti", "acct-bti").unwrap();

        // Case-insensitive substring match.
        assert_eq!(
            match_account(conn, "QUALITY BICYCLE PRODUCTS").as_deref(),
            Some("acct-qbp")
        );
        // Longest matching pattern wins.
        assert_eq!(
            match_account(conn, "Quality Bicycle Products Co #4471").as_deref(),
            Some("acct-qbp-specific")
        );
        assert_eq!(match_account(conn, "BTI Supplier").as_deref(), Some("acct-bti"));
        assert_eq!(match_account(conn, "Some Other Vendor"), None);
    }
}
