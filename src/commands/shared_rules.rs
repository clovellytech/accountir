//! Vendor payable rules and recurring transfer rules, as events.
//!
//! Both used to be plain tables written on whichever machine made them, so on
//! group-hosted books a second member had none of them: a supplier's bill went
//! to the generic payable account when they synced the shop feed, and the
//! monthly card transfer was a rule only one laptop knew about. See migrations
//! 061 and 062.
//!
//! Each check here runs inside the append transaction and is shared by the
//! local commands below and the group server's routes in
//! `sync::commands::shared_rules`, so both refuse the same things.

use crate::commands::recurring_transfers::{AmountMode, RecurringTransferRule};
use crate::commands::vendor_rules::VendorRule;
use crate::events::types::{Event, EventEnvelope, StoredEvent};
use crate::store::event_store::{CheckedOutcome, EventStore, EventStoreError, Verdict};
use crate::store::projections::Projector;
use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

#[derive(Error, Debug)]
pub enum RuleError {
    #[error("Enter a name pattern to match")]
    EmptyPattern,
    #[error("No account with id {0}")]
    NoSuchAccount(String),
    #[error("Account {0} is inactive")]
    InactiveAccount(String),
    #[error("No rule with id {0}")]
    NoSuchRule(String),
    #[error("A rule with id {0} already exists")]
    RuleExists(String),
    #[error("Invalid rule: {0}")]
    Invalid(String),
    #[error("Store error: {0}")]
    Store(#[from] EventStoreError),
}

/// A check's answer: the event to append, or why not. The outer `Result` is a
/// store failure, which aborts rather than refuses.
pub type Checked = Result<Result<Event, RuleError>, EventStoreError>;

fn active_account(conn: &Connection, account_id: &str) -> Result<Option<RuleError>, EventStoreError> {
    let active: Option<bool> = conn
        .query_row(
            "SELECT is_active = 1 FROM accounts WHERE id = ?1",
            [account_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match active {
        None => Some(RuleError::NoSuchAccount(account_id.to_string())),
        Some(false) => Some(RuleError::InactiveAccount(account_id.to_string())),
        Some(true) => None,
    })
}

fn rule_exists(conn: &Connection, table: &str, rule_id: &str) -> Result<bool, EventStoreError> {
    Ok(conn
        .query_row(
            &format!("SELECT 1 FROM {table} WHERE id = ?1"),
            [rule_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

// ---------------------------------------------------------------------------
// Vendor rules
// ---------------------------------------------------------------------------

/// Point a vendor pattern at a payable account.
///
/// Keeps the id of a rule already naming the same pattern, so setting a vendor
/// twice edits their rule rather than leaving two. `rule_id` is used only for a
/// pattern with no rule yet — publishing keeps a rule's id that way — and a
/// fresh one is minted when it is `None`.
pub fn check_set_vendor_rule(
    conn: &Connection,
    rule_id: Option<&str>,
    pattern: &str,
    account_id: &str,
) -> Checked {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Ok(Err(RuleError::EmptyPattern));
    }
    if let Some(e) = active_account(conn, account_id)? {
        return Ok(Err(e));
    }
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM vendor_account_rules WHERE LOWER(pattern) = LOWER(?1) LIMIT 1",
            [pattern],
            |row| row.get(0),
        )
        .optional()?;
    let rule_id = existing
        .or_else(|| rule_id.map(str::to_string))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    Ok(Ok(Event::VendorRuleSet {
        rule_id,
        pattern: pattern.to_string(),
        account_id: account_id.to_string(),
    }))
}

pub fn check_delete_vendor_rule(conn: &Connection, rule_id: &str) -> Checked {
    if !rule_exists(conn, "vendor_account_rules", rule_id)? {
        return Ok(Err(RuleError::NoSuchRule(rule_id.to_string())));
    }
    Ok(Ok(Event::VendorRuleDeleted {
        rule_id: rule_id.to_string(),
    }))
}

/// Set a vendor rule on standalone books.
pub fn set_vendor_rule(
    store: &mut EventStore,
    user_id: &str,
    pattern: &str,
    account_id: &str,
) -> Result<StoredEvent, RuleError> {
    append_locally(store, user_id, |tx| {
        check_set_vendor_rule(tx, None, pattern, account_id)
    })
}

pub fn delete_vendor_rule(
    store: &mut EventStore,
    user_id: &str,
    rule_id: &str,
) -> Result<StoredEvent, RuleError> {
    append_locally(store, user_id, |tx| check_delete_vendor_rule(tx, rule_id))
}

/// Vendor rules this machine made before rules were events. Nobody else has
/// them; on hosted books the Services page offers to publish them.
pub fn unpublished_vendor_rules(conn: &Connection) -> Vec<VendorRule> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT r.id, r.pattern, r.account_id FROM vendor_account_rules r
           JOIN accounts a ON a.id = r.account_id AND a.is_active = 1
          WHERE r.updated_at_event IS NULL
          ORDER BY r.pattern",
    ) else {
        return Vec::new();
    };
    stmt.query_map([], |r| {
        Ok(VendorRule {
            id: r.get(0)?,
            pattern: r.get(1)?,
            account_id: r.get(2)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Recurring transfer rules
// ---------------------------------------------------------------------------

/// What a new recurring transfer rule says. `rule_id` is `None` for a new rule
/// and the existing id when publishing one made before rules were events.
#[derive(Debug, Clone)]
pub struct NewRecurringRule {
    pub rule_id: Option<String>,
    pub source_account_id: String,
    pub dest_account_id: String,
    pub day_of_month: u32,
    pub amount_mode: AmountMode,
    pub fixed_amount_cents: Option<i64>,
    pub memo: String,
    pub start_month: String,
}

impl NewRecurringRule {
    /// The rule as it stands, for publishing under its own id.
    pub fn from_existing(rule: &RecurringTransferRule) -> Self {
        NewRecurringRule {
            rule_id: Some(rule.id.clone()),
            source_account_id: rule.source_account_id.clone(),
            dest_account_id: rule.dest_account_id.clone(),
            day_of_month: rule.day_of_month,
            amount_mode: rule.amount_mode,
            fixed_amount_cents: rule.fixed_amount_cents,
            memo: rule.memo.clone(),
            start_month: rule.start_month.clone(),
        }
    }
}

/// Create a recurring transfer rule.
///
/// A supplied id that is already taken is refused rather than overwritten: the
/// projection replaces by id, so accepting it would let one member's rule
/// silently become another's.
pub fn check_create_recurring_rule(conn: &Connection, rule: &NewRecurringRule) -> Checked {
    for account in [&rule.source_account_id, &rule.dest_account_id] {
        if let Some(e) = active_account(conn, account)? {
            return Ok(Err(e));
        }
    }
    let rule_id = match &rule.rule_id {
        Some(id) if rule_exists(conn, "recurring_transfer_rules", id)? => {
            return Ok(Err(RuleError::RuleExists(id.clone())))
        }
        Some(id) => id.clone(),
        None => Uuid::new_v4().to_string(),
    };
    let event = Event::RecurringTransferRuleCreated {
        rule_id,
        source_account_id: rule.source_account_id.clone(),
        dest_account_id: rule.dest_account_id.clone(),
        day_of_month: rule.day_of_month,
        amount_mode: rule.amount_mode.as_str().to_string(),
        fixed_amount_cents: match rule.amount_mode {
            AmountMode::Fixed => rule.fixed_amount_cents,
            AmountMode::FullBalance => None,
        },
        memo: rule.memo.trim().to_string(),
        start_month: rule.start_month.trim().to_string(),
    };
    // The shape rules live in validation; asking here turns a bad day or month
    // into a refusal the caller can show instead of a store error.
    if let Err(e) = crate::events::validation::validate_event(&event) {
        return Ok(Err(RuleError::Invalid(e.to_string())));
    }
    Ok(Ok(event))
}

pub fn check_set_recurring_rule_active(conn: &Connection, rule_id: &str, active: bool) -> Checked {
    if !rule_exists(conn, "recurring_transfer_rules", rule_id)? {
        return Ok(Err(RuleError::NoSuchRule(rule_id.to_string())));
    }
    Ok(Ok(Event::RecurringTransferRuleActiveSet {
        rule_id: rule_id.to_string(),
        active,
    }))
}

pub fn check_delete_recurring_rule(conn: &Connection, rule_id: &str) -> Checked {
    if !rule_exists(conn, "recurring_transfer_rules", rule_id)? {
        return Ok(Err(RuleError::NoSuchRule(rule_id.to_string())));
    }
    Ok(Ok(Event::RecurringTransferRuleDeleted {
        rule_id: rule_id.to_string(),
    }))
}

pub fn create_recurring_rule(
    store: &mut EventStore,
    user_id: &str,
    rule: &NewRecurringRule,
) -> Result<StoredEvent, RuleError> {
    append_locally(store, user_id, |tx| check_create_recurring_rule(tx, rule))
}

pub fn set_recurring_rule_active(
    store: &mut EventStore,
    user_id: &str,
    rule_id: &str,
    active: bool,
) -> Result<StoredEvent, RuleError> {
    append_locally(store, user_id, |tx| {
        check_set_recurring_rule_active(tx, rule_id, active)
    })
}

pub fn delete_recurring_rule(
    store: &mut EventStore,
    user_id: &str,
    rule_id: &str,
) -> Result<StoredEvent, RuleError> {
    append_locally(store, user_id, |tx| check_delete_recurring_rule(tx, rule_id))
}

/// Recurring rules this machine made before rules were events.
pub fn unpublished_recurring_rules(conn: &Connection) -> Vec<RecurringTransferRule> {
    let ids: Vec<String> = conn
        .prepare("SELECT id FROM recurring_transfer_rules WHERE created_at_event IS NULL")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| r.get(0))
                .map(|rows| rows.filter_map(Result::ok).collect())
        })
        .unwrap_or_default();
    crate::commands::recurring_transfers::list_rules(conn)
        .into_iter()
        .filter(|r| ids.contains(&r.id))
        .collect()
}

fn append_locally(
    store: &mut EventStore,
    user_id: &str,
    check: impl Fn(&rusqlite::Transaction<'_>) -> Checked,
) -> Result<StoredEvent, RuleError> {
    loop {
        let head = store.latest_id()?.unwrap_or(0);
        let outcome = store.append_checked(
            head,
            |tx| {
                Ok(match check(tx)? {
                    Ok(event) => Verdict::Append(EventEnvelope::new(event, user_id.to_string())),
                    Err(e) => Verdict::Reject(e),
                })
            },
            |tx, stored| {
                Projector::new(tx)
                    .apply(stored)
                    .map_err(|e| EventStoreError::Projection(e.to_string()))
            },
        )?;
        match outcome {
            CheckedOutcome::Appended(stored) => return Ok(stored),
            CheckedOutcome::HeadMismatch { .. } => continue,
            CheckedOutcome::Rejected(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{recurring_transfers, vendor_rules};
    use crate::store::event_store::MirrorEvent;
    use crate::store::migrations::SchemaStore;

    fn store() -> EventStore {
        let mut s = EventStore::in_memory().unwrap();
        SchemaStore::init_schema(&mut s).unwrap();
        s.connection()
            .execute_batch(
                "INSERT INTO accounts (id, account_type, account_number, name, is_active)
                 VALUES ('qbp', 'liability', '2010', 'AP - QBP', 1),
                        ('ap', 'liability', '2000', 'Accounts payable', 1),
                        ('zak', 'liability', '2110', 'Zak card', 1),
                        ('bcc', 'liability', '2100', 'Business credit card', 1);",
            )
            .unwrap();
        s
    }

    /// What a second member's machine has: the same accounts, and the log.
    fn replica_of(origin: &EventStore) -> EventStore {
        let mirrored: Vec<MirrorEvent> = origin
            .get_all()
            .unwrap()
            .into_iter()
            .map(|e| MirrorEvent {
                seq: e.id,
                event: e.event,
                user_id: e.user_id,
                timestamp: e.timestamp,
                actor_id: None,
                received_at: None,
                hash: e.hash,
            })
            .collect();
        let mut replica = store();
        replica
            .append_mirrored(&mirrored, crate::sync::project)
            .unwrap();
        replica
    }

    fn card_rule(rule_id: Option<&str>) -> NewRecurringRule {
        NewRecurringRule {
            rule_id: rule_id.map(str::to_string),
            source_account_id: "zak".into(),
            dest_account_id: "bcc".into(),
            day_of_month: 22,
            amount_mode: AmountMode::FullBalance,
            fixed_amount_cents: None,
            memo: "Employee card to parent".into(),
            start_month: "2026-01".into(),
        }
    }

    /// The reported problem: a vendor rule set by one member never reached
    /// another, so the same supplier's bills were routed differently depending
    /// on who synced.
    #[test]
    fn a_vendor_rule_reaches_every_member() {
        let mut origin = store();
        set_vendor_rule(&mut origin, "user", "Quality Bicycle", "qbp").unwrap();

        let replica = replica_of(&origin);
        assert_eq!(
            vendor_rules::match_account(replica.connection(), "QUALITY BICYCLE PRODUCTS"),
            Some("qbp".to_string())
        );
        assert!(unpublished_vendor_rules(replica.connection()).is_empty());
    }

    #[test]
    fn setting_a_vendor_again_edits_their_rule_and_delete_removes_it() {
        let mut s = store();
        set_vendor_rule(&mut s, "user", "Quality Bicycle", "ap").unwrap();
        set_vendor_rule(&mut s, "user", "quality bicycle", "qbp").unwrap();
        let rules = vendor_rules::list_rules(s.connection());
        assert_eq!(rules.len(), 1, "one rule per vendor: {rules:?}");
        assert_eq!(rules[0].account_id, "qbp");

        delete_vendor_rule(&mut s, "user", &rules[0].id).unwrap();
        assert!(vendor_rules::list_rules(s.connection()).is_empty());
        assert!(matches!(
            set_vendor_rule(&mut s, "user", "  ", "qbp"),
            Err(RuleError::EmptyPattern)
        ));
    }

    #[test]
    fn a_recurring_rule_and_its_pause_reach_every_member() {
        let mut origin = store();
        create_recurring_rule(&mut origin, "user", &card_rule(None)).unwrap();
        let id = recurring_transfers::list_rules(origin.connection())[0].id.clone();
        set_recurring_rule_active(&mut origin, "user", &id, false).unwrap();

        let replica = replica_of(&origin);
        let rules = recurring_transfers::list_rules(replica.connection());
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, id, "the same id, so posted months stay posted");
        assert!(!rules[0].active);
        assert!(unpublished_recurring_rules(replica.connection()).is_empty());

        delete_recurring_rule(&mut origin, "user", &id).unwrap();
        assert!(recurring_transfers::list_rules(origin.connection()).is_empty());
    }

    /// Publishing a rule made before rules were events keeps its id: its posted
    /// transfers are referenced `recurring:<id>:<month>`, and a new id would
    /// offer every one of those months again.
    #[test]
    fn publishing_a_local_recurring_rule_keeps_its_id() {
        let mut s = store();
        let old_id = recurring_transfers::create_rule(
            s.connection(),
            "zak",
            "bcc",
            22,
            AmountMode::FullBalance,
            None,
            "",
            "2026-01",
        )
        .unwrap();
        let local = unpublished_recurring_rules(s.connection());
        assert_eq!(local.len(), 1);

        create_recurring_rule(&mut s, "user", &NewRecurringRule::from_existing(&local[0]))
            .expect_err("the id is this machine's own row, so the create is refused");

        // On the group's books — where the local row does not exist — it lands
        // under the same id.
        let mut group = store();
        create_recurring_rule(&mut group, "user", &NewRecurringRule::from_existing(&local[0]))
            .unwrap();
        assert_eq!(recurring_transfers::list_rules(group.connection())[0].id, old_id);
    }

    #[test]
    fn a_bad_recurring_rule_is_refused_before_it_reaches_the_log() {
        let mut s = store();
        let mut same = card_rule(None);
        same.dest_account_id = "zak".into();
        assert!(matches!(
            create_recurring_rule(&mut s, "user", &same),
            Err(RuleError::Invalid(_))
        ));
        let mut fixed_without_amount = card_rule(None);
        fixed_without_amount.amount_mode = AmountMode::Fixed;
        assert!(create_recurring_rule(&mut s, "user", &fixed_without_amount).is_err());
        assert!(s.get_all().unwrap().is_empty());
    }
}
