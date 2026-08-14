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

/// Point one vendor at a payable account, replacing any rule already naming them.
///
/// Distinct from [`add_rule`], which appends: linking a vendor is something a
/// person will do twice — once wrongly — and appending would leave two rules for
/// the same name, with [`match_account`] picking between them by length rather
/// than by which was meant.
///
/// The pattern is the vendor's name exactly as their bills carry it, so the
/// substring match that routes ingest postings also matches the bills this rule
/// was created from.
pub fn set_rule_for(
    conn: &Connection,
    vendor: &str,
    account_id: &str,
) -> Result<String, rusqlite::Error> {
    let pattern = vendor.trim();
    conn.execute(
        "DELETE FROM vendor_account_rules WHERE LOWER(pattern) = LOWER(?1)",
        [pattern],
    )?;
    add_rule(conn, pattern, account_id)
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

#[cfg(test)]
mod set_rule_tests {
    use super::*;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::init_schema;

    fn conn() -> EventStore {
        let store = EventStore::in_memory().unwrap();
        init_schema(store.connection()).unwrap();
        store
    }

    /// Linking a vendor twice must leave one rule, not two.
    ///
    /// `add_rule` appends, and linking a vendor is something a person does twice —
    /// once wrongly. Two rules naming the same vendor leaves `match_account`
    /// choosing between them by pattern length, which for identical patterns is
    /// whichever the query happened to return first: the correction would appear
    /// to have worked and then not have.
    #[test]
    fn relinking_a_vendor_replaces_its_rule() {
        let store = conn();
        set_rule_for(store.connection(), "Quality Bicycle", "acct-wrong").unwrap();
        set_rule_for(store.connection(), "Quality Bicycle", "acct-right").unwrap();

        let rules = list_rules(store.connection());
        assert_eq!(rules.len(), 1, "the old rule survived: {rules:?}");
        assert_eq!(rules[0].account_id, "acct-right");
        assert_eq!(
            match_account(store.connection(), "Quality Bicycle").as_deref(),
            Some("acct-right")
        );
    }

    /// Case is how a vendor name arrives from a bill, an import and a bank feed —
    /// three sources that will not agree — so replacing has to be case-insensitive
    /// or the "replacement" silently becomes a second rule.
    #[test]
    fn replacing_ignores_the_case_the_vendor_was_typed_in() {
        let store = conn();
        set_rule_for(store.connection(), "quality bicycle", "acct-one").unwrap();
        set_rule_for(store.connection(), "QUALITY BICYCLE", "acct-two").unwrap();
        assert_eq!(list_rules(store.connection()).len(), 1);
    }

    /// Other vendors are left alone — an obvious property, and the one a `DELETE`
    /// with a loose predicate would break.
    #[test]
    fn other_vendors_are_untouched() {
        let store = conn();
        set_rule_for(store.connection(), "Shimano", "acct-shimano").unwrap();
        set_rule_for(store.connection(), "Quality Bicycle", "acct-qbp").unwrap();
        assert_eq!(list_rules(store.connection()).len(), 2);
        assert_eq!(
            match_account(store.connection(), "Shimano").as_deref(),
            Some("acct-shimano")
        );
    }
}
