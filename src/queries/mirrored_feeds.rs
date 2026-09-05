//! Two Plaid accounts on one connection reporting the same purchases.
//!
//! # The shape of the problem
//!
//! A corporate card programme has a *central bill account* that every physical
//! card rolls up into. Plaid exposes both: the roll-up and the individual cards.
//! Link both and every purchase arrives twice — once under the card that made it
//! and once under the account that pays for it.
//!
//! Nothing downstream can catch this. Idempotency keys on the Plaid transaction
//! id, and the two reports carry *different* ids, because as far as the bank is
//! concerned they are two records of two different accounts. They are, and both
//! are true. What is false is booking both as spending.
//!
//! The damage is quiet and two-sided: total card debt is overstated, and
//! whatever clearing account the charges post to is pulled the other way, so a
//! reconciliation that ought to show a gap shows a smaller one. On the ledger
//! this was written for, three weeks of one employee's card produced $381.62 of
//! double-counted purchases while partly masking a $7,204.41 shortfall
//! elsewhere.
//!
//! # Detecting it
//!
//! Two accounts on the same item, compared on `(date, amount)` as a multiset. A
//! roll-up relationship is not symmetric — the central account carries every
//! card, so the *smaller* feed should be almost entirely contained in the larger
//! one. That containment is the signal, and it is a strong one: two genuinely
//! separate cards share the odd same-day same-amount charge, but not nine out of
//! nine.
//!
//! The thresholds below are deliberately conservative. A false positive here
//! tells someone their bank feed is broken when it is not, which costs more
//! trust than the occasional miss.
//!
//! # Where this runs
//!
//! Against *staged* transactions, before anything is posted. That is the whole
//! point: the moment to say "these two accounts are the same money" is while the
//! import is still a proposal.

use std::collections::HashMap;

use rusqlite::Connection;

/// How much of the smaller feed must appear in the larger one.
///
/// Nine of nine in the case this was built for. Four in five is low enough to
/// survive a card that also gets used somewhere the roll-up does not see, and
/// high enough that ordinary coincidence does not reach it.
pub const CONTAINMENT: f64 = 0.8;

/// Below this, coincidence is likely enough not to be worth an accusation.
pub const MIN_MATCHES: usize = 3;

/// One account's transactions appearing inside another's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirroredFeed {
    pub item_id: String,
    /// The account that appears to carry the other's transactions as well as
    /// its own — the roll-up.
    pub parent_account: String,
    pub parent_name: String,
    /// The account whose transactions are duplicated. This is the one to unlink:
    /// unlinking the roll-up would lose everything it alone reports.
    pub child_account: String,
    pub child_name: String,
    /// Transactions of the child also present in the parent.
    pub matched: usize,
    /// Transactions the child reports at all.
    pub child_total: usize,
    /// What the duplication is worth, as a positive figure.
    pub amount_cents: i64,
}

impl MirroredFeed {
    /// A sentence for somebody who has just linked two accounts and does not yet
    /// know that one contains the other.
    pub fn message(&self) -> String {
        format!(
            "\u{201c}{}\u{201d} and \u{201c}{}\u{201d} are reporting the same purchases: {} of the \
             {} transactions on \u{201c}{}\u{201d} also arrive on \u{201c}{}\u{201d}, worth {}. \
             That is what a central bill account looks like \u{2014} every card under it is \
             reported twice, once by the card and once by the account that pays it. The bank \
             gives each copy its own id, so nothing downstream can tell them apart. Importing \
             both would overstate this card's balance and understate whatever the charges clear \
             against. Unlink \u{201c}{}\u{201d} and keep the account that carries everything.",
            self.parent_name,
            self.child_name,
            self.matched,
            self.child_total,
            self.child_name,
            self.parent_name,
            dollars(self.amount_cents),
            self.child_name,
        )
    }
}

fn dollars(cents: i64) -> String {
    let a = cents.abs();
    format!("${}.{:02}", a / 100, a % 100)
}

#[derive(Clone)]
struct Feed {
    plaid_account_id: String,
    name: String,
    /// `(date, amount)` counted, so two identical charges on one day need two on
    /// the other side to match.
    keys: HashMap<(String, i64), usize>,
    total: usize,
}

/// Find accounts whose staged transactions are contained in a sibling's.
pub fn mirrored_feeds(conn: &Connection) -> Result<Vec<MirroredFeed>, rusqlite::Error> {
    // Only accounts that are currently mapped.
    //
    // The warning is about what will be posted, and an unlinked account posts
    // nothing. Without this the banner outlives the fix: unlinking leaves the
    // rows it already staged behind, so the check would go on reporting a
    // duplication that has been dealt with, and a warning that cannot be
    // dismissed by doing the right thing stops being read.
    let mut stmt = conn.prepare(
        "SELECT s.item_id, s.plaid_account_id, a.name, s.date, s.amount_cents
         FROM plaid_staged_transactions s
         JOIN plaid_local_accounts a
           ON a.item_id = s.item_id AND a.plaid_account_id = s.plaid_account_id
         WHERE a.local_account_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;

    // item -> account -> feed
    let mut items: HashMap<String, HashMap<String, Feed>> = HashMap::new();
    for row in rows {
        let (item_id, account_id, name, date, amount) = row?;
        let feed = items
            .entry(item_id)
            .or_default()
            .entry(account_id.clone())
            .or_insert_with(|| Feed {
                plaid_account_id: account_id,
                name,
                keys: HashMap::new(),
                total: 0,
            });
        *feed.keys.entry((date, amount)).or_insert(0) += 1;
        feed.total += 1;
    }

    let mut out = Vec::new();
    for (item_id, accounts) in items {
        let feeds: Vec<&Feed> = accounts.values().collect();
        for child in &feeds {
            for parent in &feeds {
                if child.plaid_account_id == parent.plaid_account_id {
                    continue;
                }
                // The roll-up is the bigger feed. Comparing a feed against a
                // smaller sibling would report the pair twice, once each way,
                // and name the wrong account as the one to unlink.
                if parent.total <= child.total {
                    continue;
                }
                let mut matched = 0usize;
                let mut amount_cents = 0i64;
                for ((date, amount), n) in &child.keys {
                    let available = parent.keys.get(&(date.clone(), *amount)).copied().unwrap_or(0);
                    let pairs = (*n).min(available);
                    matched += pairs;
                    amount_cents += amount.abs() * pairs as i64;
                }
                if matched < MIN_MATCHES {
                    continue;
                }
                if (matched as f64) / (child.total as f64) < CONTAINMENT {
                    continue;
                }
                out.push(MirroredFeed {
                    item_id: item_id.clone(),
                    parent_account: parent.plaid_account_id.clone(),
                    parent_name: parent.name.clone(),
                    child_account: child.plaid_account_id.clone(),
                    child_name: child.name.clone(),
                    matched,
                    child_total: child.total,
                    amount_cents,
                });
            }
        }
    }
    // Stable, and worst first.
    out.sort_by(|a, b| {
        b.amount_cents
            .cmp(&a.amount_cents)
            .then(a.child_name.cmp(&b.child_name))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrations::init_schema;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO plaid_items (id, institution_name) VALUES ('i1', 'U.S. Bank')",
            [],
        )
        .unwrap();
        // Both mapped, which is the state that produces the problem: an
        // unmapped account posts nothing and is not worth warning about.
        for (acct, name, local, num) in [
            ("central", "Central Bill Account - 3552", "l-central", "5003"),
            ("card", "Business Credit Card - 1549", "l-card", "5013"),
        ] {
            conn.execute(
                "INSERT INTO accounts (id, account_type, account_number, name) \
                 VALUES (?1, 'liability', ?2, ?3)",
                rusqlite::params![local, num, name],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO plaid_local_accounts \
                     (item_id, plaid_account_id, name, account_type, local_account_id) \
                 VALUES ('i1', ?1, ?2, 'credit', ?3)",
                rusqlite::params![acct, name, local],
            )
            .unwrap();
        }
        conn
    }

    fn stage(conn: &Connection, account: &str, date: &str, cents: i64, id: &str) {
        conn.execute(
            "INSERT INTO plaid_staged_transactions \
                 (id, item_id, plaid_transaction_id, plaid_account_id, amount_cents, date, name) \
             VALUES (?1, 'i1', ?1, ?2, ?3, ?4, 'Amazon')",
            rusqlite::params![id, account, cents, date],
        )
        .unwrap();
    }

    /// The reported case: every charge on the employee's card arrives again on
    /// the account that pays for it, under a different Plaid id.
    #[test]
    fn a_card_reported_inside_its_central_bill_account_is_found() {
        let conn = conn();
        let charges = [
            ("2026-08-18", -3321),
            ("2026-08-19", -11130),
            ("2026-08-19", -1896),
            ("2026-08-25", -1649),
            ("2026-08-28", -2317),
        ];
        for (i, (d, c)) in charges.iter().enumerate() {
            stage(&conn, "card", d, *c, &format!("child{i}"));
            stage(&conn, "central", d, *c, &format!("parent{i}"));
        }
        // The roll-up also carries spending of its own, which is why it is the
        // one to keep.
        for i in 0..20 {
            stage(&conn, "central", "2026-07-01", -500 - i, &format!("own{i}"));
        }

        let found = mirrored_feeds(&conn).unwrap();
        assert_eq!(found.len(), 1, "one pair, named once: {found:?}");
        let f = &found[0];
        assert_eq!(f.child_name, "Business Credit Card - 1549", "unlink the card");
        assert_eq!(f.parent_name, "Central Bill Account - 3552", "keep the roll-up");
        assert_eq!(f.matched, 5);
        assert_eq!(f.child_total, 5);
        assert_eq!(f.amount_cents, 3321 + 11130 + 1896 + 1649 + 2317);

        let msg = f.message();
        assert!(msg.contains("$203.13"), "{msg}");
        assert!(msg.contains("5 of the 5"), "{msg}");
    }

    /// Acting on the warning has to clear it.
    ///
    /// Unlinking leaves the rows the account already staged in place, so a check
    /// that only looked at staged rows would go on reporting a duplication that
    /// has been fixed. A warning you cannot dismiss by doing the right thing
    /// stops being read.
    #[test]
    fn unlinking_the_duplicate_feed_silences_the_warning() {
        let conn = conn();
        for i in 0..5 {
            stage(&conn, "card", "2026-08-18", -100 - i, &format!("a{i}"));
            stage(&conn, "central", "2026-08-18", -100 - i, &format!("b{i}"));
        }
        for i in 0..20 {
            stage(&conn, "central", "2026-07-01", -900 - i, &format!("c{i}"));
        }
        assert_eq!(mirrored_feeds(&conn).unwrap().len(), 1, "both mapped");

        conn.execute(
            "UPDATE plaid_local_accounts SET local_account_id = NULL \
             WHERE item_id = 'i1' AND plaid_account_id = 'card'",
            [],
        )
        .unwrap();
        assert!(
            mirrored_feeds(&conn).unwrap().is_empty(),
            "the staged rows remain, but nothing will post them"
        );
    }

    /// Two real cards on one connection share the occasional same-day,
    /// same-amount charge. Calling that a duplicated feed would tell somebody
    /// their bank is broken when it is not, which costs more than the miss.
    #[test]
    fn two_genuine_cards_that_coincide_now_and_then_are_left_alone() {
        let conn = conn();
        for i in 0..10 {
            stage(&conn, "card", "2026-08-01", -100 - i, &format!("a{i}"));
        }
        for i in 0..30 {
            stage(&conn, "central", "2026-08-01", -900 - i, &format!("b{i}"));
        }
        // Three coincidences — enough to clear MIN_MATCHES on its own, and still
        // only three tenths of the smaller feed.
        for i in 0..3 {
            stage(&conn, "central", "2026-08-01", -100 - i, &format!("c{i}"));
        }
        assert!(mirrored_feeds(&conn).unwrap().is_empty());
    }

    /// A handful of transactions is not evidence, however well it matches. A
    /// newly linked card whose first two charges happen to coincide should not
    /// be accused.
    #[test]
    fn a_perfect_match_on_too_little_evidence_is_not_reported() {
        let conn = conn();
        for i in 0..2 {
            stage(&conn, "card", "2026-08-01", -100 - i, &format!("a{i}"));
            stage(&conn, "central", "2026-08-01", -100 - i, &format!("b{i}"));
        }
        for i in 0..10 {
            stage(&conn, "central", "2026-07-01", -900 - i, &format!("c{i}"));
        }
        assert!(mirrored_feeds(&conn).unwrap().is_empty(), "two is coincidence");
    }

    /// Counted as a multiset: two identical charges on one day need two on the
    /// other side to be matched, or a card that legitimately bought the same
    /// thing twice would look half-duplicated.
    #[test]
    fn identical_charges_need_a_partner_each() {
        let conn = conn();
        for i in 0..4 {
            stage(&conn, "card", "2026-08-01", -500, &format!("a{i}"));
        }
        stage(&conn, "central", "2026-08-01", -500, "b0");
        for i in 0..10 {
            stage(&conn, "central", "2026-07-01", -900 - i, &format!("c{i}"));
        }
        // One of the four matched is a quarter of the feed — well under the bar.
        assert!(mirrored_feeds(&conn).unwrap().is_empty());
    }
}
