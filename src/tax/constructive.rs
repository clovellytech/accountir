//! Constructive ownership under IRC §267(c) — turning recorded family ties into
//! the ownership Schedule B-1 tests against.
//!
//! # The rule this implements
//!
//! Schedule B-1 asks who owns 50% or more of the partnership, and its
//! instructions say to answer it under the constructive-ownership rules of
//! §267(c). The one that reaches into a partnership's own books is §267(c)(2):
//!
//! > An individual shall be considered as owning the stock owned, directly or
//! > indirectly, by or for his family.
//!
//! and §267(c)(4) fixes what "family" means:
//!
//! > the family of an individual shall include only his brothers and sisters
//! > (whether by the whole or half blood), spouse, ancestors, and lineal
//! > descendants.
//!
//! So a partner is treated as owning their own interest **plus** the interest of
//! every family member — and two spouses at 40% and 20% each own 60%, which is why
//! both land on Schedule B-1 though neither owns 50% directly.
//!
//! # The trap: no re-attribution
//!
//! §267(c)(5) is the rule that keeps this from spiralling:
//!
//! > Stock constructively owned by an individual by reason of the application of
//! > paragraph (2) [family] … shall not be treated as owned by him for the purpose
//! > of again applying [paragraph (2)] in order to make another the constructive
//! > owner of such stock.
//!
//! In plain terms: attribution does not chain through family. Your spouse's
//! *direct* interest is yours; your spouse's *attributed* interest — what they in
//! turn get from their own sibling, say — is not. That sibling is your
//! sibling-in-law, and §267(c)(4) does not list in-laws.
//!
//! This module honours §267(c)(5) by construction: [`constructive_shares`] only
//! ever **sums the direct shares** of the people in [`family_of`], and
//! [`family_of`] is computed by the *meaning* of each tie from the target partner
//! — spouses and siblings from direct edges, ancestors and descendants by walking
//! parent-of chains — never as reachability over an undirected graph. An in-law is
//! two symmetric hops away and is never reached; a grandparent is a genuine
//! ancestor and is.
//!
//! # What it still does not do
//!
//! Attribution from and through **entities** (§267(c)(1) and (c)(3)) — a partner
//! that is itself a partnership or corporation, owned by someone who also holds a
//! direct interest. That needs ownership facts about those entities which the
//! books do not hold. Schedule B-1 still carries a caveat saying so.

use std::collections::{BTreeSet, VecDeque};

use crate::domain::{Partner, PartnerRelationship, RelationshipKind, Shares};

/// The family of a partner, per §267(c)(4): the partner ids whose *direct* shares
/// are attributed to them.
///
/// The target itself is **not** in the returned set — [`constructive_shares`] adds
/// the target's own shares separately, and keeping the two apart is what stops a
/// partner from being counted twice.
///
/// Each kind is read by its exact meaning, which is the whole point (see the
/// module docs on §267(c)(5)):
///
/// - **Spouse, sibling** — a direct edge, and one hop only. Your sibling is
///   family; your sibling's spouse is not, so the walk does not continue past
///   them.
/// - **Ancestors** — every partner reachable by following parent-of edges
///   *upward* from the target (parents, their parents, …). A grandparent is an
///   ancestor, and their direct interest is genuinely the target's under
///   §267(c)(2) — this is not re-attribution, because it is the grandparent's own
///   direct share, reached by a chain of true parent-child ties.
/// - **Descendants** — the mirror image, following parent-of edges *downward*.
///
/// The parent-of chains are walked with a visited set, so a cycle wrongly entered
/// into the data (A parent of B parent of A) terminates rather than looping.
pub fn family_of(partner_id: &str, relationships: &[PartnerRelationship]) -> BTreeSet<String> {
    let mut family = BTreeSet::new();

    // Spouses and siblings: one direct hop, either direction of a symmetric edge.
    for rel in relationships {
        if !rel.kind.is_symmetric() {
            continue;
        }
        if rel.partner_id == partner_id {
            family.insert(rel.related_partner_id.clone());
        } else if rel.related_partner_id == partner_id {
            family.insert(rel.partner_id.clone());
        }
    }

    // Ancestors: walk parent-of edges upward (from child to parent). Descendants:
    // downward (from parent to child). Same traversal, opposite ends of the edge.
    collect_lineal(partner_id, relationships, Lineal::Up, &mut family);
    collect_lineal(partner_id, relationships, Lineal::Down, &mut family);

    // A partner is never their own family — a self-edge or a cycle could otherwise
    // put them here, and `constructive_shares` would then add their share twice.
    family.remove(partner_id);
    family
}

/// Which way to walk parent-of edges.
enum Lineal {
    /// Toward parents — the target's ancestors.
    Up,
    /// Toward children — the target's descendants.
    Down,
}

/// Breadth-first over parent-of edges in one direction, collecting everyone
/// reached into `family`.
fn collect_lineal(
    start: &str,
    relationships: &[PartnerRelationship],
    direction: Lineal,
    family: &mut BTreeSet<String>,
) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    seen.insert(start.to_string());
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(start.to_string());

    while let Some(current) = queue.pop_front() {
        for rel in relationships {
            if rel.kind != RelationshipKind::ParentOf {
                continue;
            }
            // Up: current is the child (related_partner_id), step to the parent.
            // Down: current is the parent (partner_id), step to the child.
            let next = match direction {
                Lineal::Up if rel.related_partner_id == current => &rel.partner_id,
                Lineal::Down if rel.partner_id == current => &rel.related_partner_id,
                _ => continue,
            };
            if seen.insert(next.clone()) {
                family.insert(next.clone());
                queue.push_back(next.clone());
            }
        }
    }
}

/// A partner's ownership as Schedule B-1 tests it: their own shares plus, in each
/// column independently, the summed **direct** shares of their §267(c)(4) family.
///
/// Summed per column — profit, loss, and capital each add up on their own, because
/// a partner can be family-attributed to a majority of capital while holding a
/// minority of profit, and Schedule B-1's "50% or more in profit, loss, **or**
/// capital" tests each column separately.
///
/// The result cannot exceed the whole: `family_of` is a set of *other* partners,
/// and every partner's direct shares together total 100% at most, so a subset of
/// them plus the target is still bounded by the whole.
///
/// Only direct shares are summed — never a family member's own attributed total —
/// which is how §267(c)(5)'s no-re-attribution rule is kept. See the module docs.
pub fn constructive_shares(
    target: &Partner,
    all_partners: &[Partner],
    relationships: &[PartnerRelationship],
) -> Shares {
    let family = family_of(&target.partner_id, relationships);

    let mut shares = target.shares;
    for p in all_partners {
        if family.contains(&p.partner_id) {
            shares.profit_ppm += p.shares.profit_ppm;
            shares.loss_ppm += p.shares.loss_ppm;
            shares.capital_ppm += p.shares.capital_ppm;
        }
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, PartnerType, Residency};
    use chrono::NaiveDate;

    fn partner(id: &str, profit: f64, loss: f64, capital: f64) -> Partner {
        Partner {
            history: Vec::new(),
            partner_id: id.to_string(),
            name: id.to_string(),
            partner_type: PartnerType::General,
            residency: Residency::Domestic,
            entity_type: "Individual".to_string(),
            address: Address::default(),
            start_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            end_date: None,
            shares: Shares::from_percents(profit, loss, capital),
        }
    }

    fn rel(a: &str, b: &str, kind: RelationshipKind) -> PartnerRelationship {
        PartnerRelationship::new(a, b, kind)
    }

    /// The case that prompted all this: two spouses each own the couple's total.
    #[test]
    fn spouses_each_own_the_couples_combined_interest() {
        let me = partner("me", 40.0, 40.0, 40.0);
        let wife = partner("wife", 20.0, 20.0, 20.0);
        let all = [me.clone(), wife.clone()];
        let rels = [rel("me", "wife", RelationshipKind::Spouse)];

        // 40 + 20 = 60, and symmetrically for the spouse.
        assert_eq!(constructive_shares(&me, &all, &rels).capital_ppm, 600_000);
        assert_eq!(constructive_shares(&wife, &all, &rels).capital_ppm, 600_000);
    }

    /// Symmetric ties read the same however they were entered.
    #[test]
    fn a_spouse_edge_works_from_either_side() {
        let me = partner("me", 40.0, 40.0, 40.0);
        let wife = partner("wife", 20.0, 20.0, 20.0);
        let all = [me.clone(), wife.clone()];
        // Entered "wife spouse of me" — the reverse of the ids' canonical order.
        let rels = [rel("wife", "me", RelationshipKind::Spouse)];
        assert_eq!(constructive_shares(&me, &all, &rels).profit_ppm, 600_000);
    }

    /// With no tie recorded, nobody is attributed anything — the same two partners
    /// each own only their direct share, and neither reaches 50%.
    #[test]
    fn without_a_relationship_there_is_no_attribution() {
        let me = partner("me", 40.0, 40.0, 40.0);
        let wife = partner("wife", 20.0, 20.0, 20.0);
        let all = [me.clone(), wife.clone()];
        assert_eq!(constructive_shares(&me, &all, &[]).capital_ppm, 400_000);
        assert_eq!(constructive_shares(&wife, &all, &[]).capital_ppm, 200_000);
    }

    /// A grandparent is an ancestor: parent-of chains are walked to the end, so a
    /// direct interest two generations up is attributed down.
    #[test]
    fn ancestors_and_descendants_carry_through_a_chain() {
        let grandparent = partner("gp", 30.0, 30.0, 30.0);
        let parent = partner("p", 10.0, 10.0, 10.0);
        let child = partner("c", 15.0, 15.0, 15.0);
        let all = [grandparent.clone(), parent.clone(), child.clone()];
        let rels = [
            rel("gp", "p", RelationshipKind::ParentOf),
            rel("p", "c", RelationshipKind::ParentOf),
        ];

        // The child owns its own 15 + parent 10 + grandparent 30 = 55.
        assert_eq!(
            constructive_shares(&child, &all, &rels).capital_ppm,
            550_000
        );
        // And the grandparent, looking down, owns 30 + 10 + 15 = 55.
        assert_eq!(
            constructive_shares(&grandparent, &all, &rels).capital_ppm,
            550_000
        );
    }

    /// §267(c)(5): no re-attribution through family. A spouse's sibling is a
    /// sibling-in-law, not family, so their interest must not leak across the
    /// marriage.
    #[test]
    fn an_in_law_is_not_attributed() {
        let me = partner("me", 40.0, 40.0, 40.0);
        let wife = partner("wife", 20.0, 20.0, 20.0);
        let wifes_brother = partner("bro", 25.0, 25.0, 25.0);
        let all = [me.clone(), wife.clone(), wifes_brother.clone()];
        let rels = [
            rel("me", "wife", RelationshipKind::Spouse),
            rel("wife", "bro", RelationshipKind::Sibling),
        ];

        // I get my wife's 20 but NOT her brother's 25: 40 + 20 = 60, not 85.
        assert_eq!(constructive_shares(&me, &all, &rels).capital_ppm, 600_000);
        // My wife, though, is family to both of us: 20 + 40 + 25 = 85.
        assert_eq!(constructive_shares(&wife, &all, &rels).capital_ppm, 850_000);
    }

    /// Columns are summed independently — a family majority of capital need not be
    /// a majority of profit.
    #[test]
    fn columns_attribute_independently() {
        let me = partner("me", 10.0, 10.0, 40.0);
        let wife = partner("wife", 5.0, 5.0, 30.0);
        let all = [me.clone(), wife.clone()];
        let rels = [rel("me", "wife", RelationshipKind::Spouse)];
        let s = constructive_shares(&me, &all, &rels);
        assert_eq!(s.profit_ppm, 150_000);
        assert_eq!(s.capital_ppm, 700_000);
    }

    /// A cycle wrongly entered into the data must not loop forever.
    #[test]
    fn a_parent_of_cycle_terminates() {
        let a = partner("a", 20.0, 20.0, 20.0);
        let b = partner("b", 20.0, 20.0, 20.0);
        let all = [a.clone(), b.clone()];
        let rels = [
            rel("a", "b", RelationshipKind::ParentOf),
            rel("b", "a", RelationshipKind::ParentOf),
        ];
        // Terminates, and each is in the other's family exactly once: 20 + 20 = 40.
        assert_eq!(constructive_shares(&a, &all, &rels).capital_ppm, 400_000);
    }

    /// `family_of` never contains the target, even if the data names them their
    /// own relative — otherwise their share would be added twice.
    #[test]
    fn a_partner_is_not_their_own_family() {
        let fam = family_of("me", &[rel("me", "me", RelationshipKind::Sibling)]);
        assert!(!fam.contains("me"));
    }
}
