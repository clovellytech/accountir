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

pub fn add_rule(
    conn: &Connection,
    pattern: &str,
    account_id: &str,
) -> Result<String, rusqlite::Error> {
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
    match_in(&list_rules(conn), name).map(|r| r.account_id.clone())
}

/// The rule that would route a bill from `name`, given a set of rules.
///
/// Split out from [`match_account`] so a screen showing "where this vendor's next
/// bill goes" can answer with the rule that will actually be used, rather than
/// with a second opinion written to look the same. The payables page reimplemented
/// this once; the two agreed until they wouldn't have.
///
/// Longest pattern wins. A rule for "Quality Bicycle" and one for "Quality Bicycle
/// Products" both match a QBP bill, and the more specific one is the one somebody
/// wrote on purpose.
pub fn match_in<'a>(rules: &'a [VendorRule], name: &str) -> Option<&'a VendorRule> {
    let name_lc = name.to_lowercase();
    rules
        .iter()
        .filter(|rule| {
            let p = rule.pattern.trim().to_lowercase();
            // An empty pattern is `contains`-true against everything. It should
            // not exist, and it must not match if it does.
            !p.is_empty() && name_lc.contains(&p)
        })
        .max_by_key(|rule| rule.pattern.trim().chars().count())
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
        assert_eq!(
            match_account(conn, "BTI Supplier").as_deref(),
            Some("acct-bti")
        );
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

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use crate::store::event_store::EventStore;
    use crate::store::migrations::{init_schema, run_migrations};

    /// A rule set in one session is there in the next.
    ///
    /// Written to a file and read back through a second connection, because the
    /// question is about durability and an in-memory store cannot answer it. The
    /// report that prompted this was "I linked some vendors, restarted, and the
    /// links were gone".
    #[test]
    fn a_rule_survives_closing_and_reopening_the_book() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("books.db").to_string_lossy().to_string();

        {
            let store = EventStore::open(&path).expect("open");
            init_schema(store.connection()).expect("schema");
            run_migrations(store.connection()).expect("migrations");
            set_rule_for(store.connection(), "Quality Bicycle Products", "ap-qbp")
                .expect("set the rule");
        }

        let store = EventStore::open(&path).expect("reopen");
        // Migrations run on every open, as they do when the app starts. A
        // migration that dropped or recreated this table would take the rules
        // with it, and the symptom would be exactly the one reported.
        run_migrations(store.connection()).expect("migrations");

        let rules = list_rules(store.connection());
        assert_eq!(rules.len(), 1, "the rule did not survive the restart");
        assert_eq!(rules[0].pattern, "Quality Bicycle Products");
        assert_eq!(rules[0].account_id, "ap-qbp");
    }

    /// And it survives a projection rebuild.
    ///
    /// The rebuild wipes and replays every projected table. This one is plain
    /// config — nothing in the log produces it — so it is deliberately not on
    /// that list, and if it ever were, every link a user had made would vanish
    /// the next time a replica caught up.
    #[test]
    fn a_rule_survives_a_projection_rebuild() {
        let mut store = EventStore::in_memory().expect("store");
        init_schema(store.connection()).expect("schema");
        set_rule_for(store.connection(), "Quality Bicycle Products", "ap-qbp").expect("rule");

        let events = store.get_all().expect("events");
        crate::store::projections::ProjectionStore::rebuild_projections(&mut store, &events)
            .expect("rebuild");

        assert_eq!(
            list_rules(store.connection()).len(),
            1,
            "a projection rebuild wiped the vendor links"
        );
    }

    /// Linking the same vendor twice leaves one rule, not two.
    #[test]
    fn re_linking_a_vendor_replaces_rather_than_appends() {
        let store = EventStore::in_memory().expect("store");
        init_schema(store.connection()).expect("schema");

        set_rule_for(store.connection(), "QBP", "ap-one").expect("first");
        set_rule_for(store.connection(), "qbp", "ap-two").expect("second, different case");

        let rules = list_rules(store.connection());
        assert_eq!(rules.len(), 1, "two rules for one vendor: {rules:?}");
        assert_eq!(rules[0].account_id, "ap-two", "the later link must win");
    }
}

#[cfg(test)]
mod matching_tests {
    use super::*;

    fn rule(pattern: &str, account: &str) -> VendorRule {
        VendorRule {
            id: pattern.to_string(),
            pattern: pattern.to_string(),
            account_id: account.to_string(),
        }
    }

    /// The more specific rule wins, wherever it is asked.
    ///
    /// Both the posting path and the payables page ask through this, so a page
    /// promising one account and the ledger using another is not a state the two
    /// can get into.
    #[test]
    fn the_longest_matching_pattern_wins() {
        let rules = vec![
            rule("Quality Bicycle", "ap-general"),
            rule("Quality Bicycle Products", "ap-qbp"),
        ];
        let hit = match_in(&rules, "QUALITY BICYCLE PRODUCTS INC").expect("a match");
        assert_eq!(hit.account_id, "ap-qbp");
    }

    /// An empty pattern matches everything under `contains`. It should not exist,
    /// and it must not route a bill if it does.
    #[test]
    fn an_empty_pattern_never_matches() {
        let rules = vec![rule("   ", "ap-wrong")];
        assert!(match_in(&rules, "Anybody At All").is_none());
    }

    #[test]
    fn a_vendor_with_no_rule_has_no_match() {
        let rules = vec![rule("Quality Bicycle", "ap-qbp")];
        assert!(match_in(&rules, "Shimano").is_none());
    }
}
