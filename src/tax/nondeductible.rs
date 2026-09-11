//! Each partner's share of Schedule K line 18c, itemised.
//!
//! # What this is for
//!
//! Line 18c is the disallowed part of a limited deduction — the half of a meal
//! §274(n) refuses, a fine, a political contribution. It is not deducted
//! anywhere, and it still reduces each partner's outside basis and their
//! tax-basis capital account, which is why the form reports it rather than
//! dropping it.
//!
//! That reaches a partner in two places, and both of them are one box with no
//! explanation in it: box 18 code C on their K-1, and item L row 4, "other
//! increase (decrease)" — a row the form explicitly asks you to attach an
//! explanation for. This module produces that explanation: the partner's own
//! share of each account that made up line 18c, on a page that travels with
//! their K-1.
//!
//! # Why the components are allocated and not the box
//!
//! The obvious arrangement is to split the box across the partners and then
//! prorate the components inside each partner's share. It is wrong in the way
//! that matters: the components are then derived from a rounded figure by a
//! second rounding, and a partner's own list stops adding up to their own box.
//! A statement whose total contradicts the form it supports is worse than no
//! statement, because it invites a reader to work out which of the two is the
//! error.
//!
//! So the components are what get allocated, each on the same percentages, and a
//! partner's box is the sum of their own rows by construction.
//!
//! # Why the allocation runs on cumulative totals
//!
//! Allocating each component independently gets half of what is needed. Each
//! component's shares sum to that component — largest remainder guarantees that
//! — but the partners' *rows* then need not sum to the partners' *boxes*:
//! rounding three components separately and adding is not the same arithmetic as
//! rounding their sum, and the two differ by a dollar often enough to matter on
//! a form where a dollar is visible.
//!
//! Allocating the running total instead, and taking each component's share as
//! the difference between two consecutive allocations, gets both:
//!
//! - down a column, the shares of component *k* are the increments of one
//!   allocation step, so they sum to **what that component added to the box** —
//!   the running total rounded with it, less the running total rounded without;
//! - along a row, a partner's components telescope to their share of the
//!   **whole**, which is exactly the figure [`crate::tax::capital`] puts in item
//!   L row 4 and exactly the figure `split_across_partners` puts in box 18
//!   code C.
//!
//! The three agree because all three are one line split on one weighting: item L
//! row 4, box 18 code C and this all ask [`crate::tax::varying`] to weight the
//! year's parts by line 18c's own history (§706(d)). Item L *row 3* is weighted
//! by the whole of Schedule K instead, because that is what row 3 is a share of —
//! so on a year whose interests moved, rows 3 and 4 are deliberately divided
//! differently. Weighting row 4 and this module by the whole of Schedule K, which
//! is what the first version did, put a statement of 140 and a row 4 of −140 on
//! the same page as a box of 180, on a $200 line.
//!
//! # A component's figure here is not quite its figure on the entity page
//!
//! "What that component added to the box" is not the same as "that component
//! rounded", and it cannot be. The box is `round(Σ components)`; the only
//! definition of a component's share under which the partners' rows and the
//! partners' boxes both foot is its contribution to that sum. Two components of
//! fifty cents make a box of one dollar, so between them the partner pages carry
//! one dollar — nought and one, in the order the components arrive — while the
//! entity's own line 18c statement, which rounds each row for display, prints a
//! dollar against each of them.
//!
//! That is where a reader will meet it: the entity page and the partner pages,
//! account by account, can differ by a dollar. Every total on every page is
//! right, and the alternative — rounding each component and allocating that —
//! breaks the invariant `crate::tax::allocate` exists for, which is that the
//! K-1s add back to Schedule K exactly.
//!
//! # A row can come out negative
//!
//! Largest-remainder rounding is not monotone. A partner who was owed the
//! leftover dollar at one prefix need not be owed it at the next, so an
//! increment can be −1: a page reading `Meals $1`, `Fines −$1`. Seen on three
//! partners at 12/34/54 with components of $4 and $1, and on 49/49/2 with $17
//! and $1 — small books, not contrived ones.
//!
//! It is not smoothed away, because both ways of smoothing it are worse.
//! Allocating each component independently gives non-negative rows and lets the
//! partners' boxes sum to `Σ round(component)` instead of `round(Σ component)`,
//! so the K-1s drift a dollar from Schedule K line 18c. Shuffling dollars between
//! rows after the fact means the printed rows are no longer the allocation
//! anybody can reproduce. The row is a real dollar in the right place and both
//! margins still foot, so it is left alone and
//! [`crate::tax::form1065`] names it in a warning against the K-1 it appears on —
//! a preparer who would rather not explain a negative to a reader can merge the
//! two rows by hand, which does not change a total.
//!
//! # Cost
//!
//! One [`allocate_over_year`] call per component, each of which re-reads the
//! share periods and, on a segmented year, one income statement per segment.
//! Components are accounts carrying a deduction limit — a handful on real books
//! — so this is a few queries, not a scan.
//!
//! [`allocate_over_year`]: crate::tax::varying::allocate_over_year

use rusqlite::Connection;

use super::allocate::Basis;
use super::lines::{cents_to_dollars, LineDetail};
use crate::domain::Partner;

/// One partner's share of line 18c, account by account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartnerStatement {
    pub partner_id: String,
    /// Carried so the statement page can name them without a second lookup.
    pub partner_name: String,
    /// Their share of each component that reached line 18c.
    ///
    /// The amounts are **whole dollars held in the cents field**, times one
    /// hundred. [`LineDetail`] is the shape [`crate::tax::statement`] draws, and
    /// it carries cents because entity-level statements round a sum of cents
    /// once. These figures are already whole: they came out of an allocator that
    /// works in dollars so the K-1s add back to Schedule K, and re-rounding them
    /// is the one thing that could make this list stop footing to the box it
    /// explains. Storing them ×100 keeps the same type and the same drawing code
    /// while rounding nothing twice.
    ///
    /// A component this partner got nothing of is omitted rather than printed as
    /// a zero: a row of zeros on a tax statement reads as a figure somebody
    /// failed to fill in.
    pub rows: Vec<LineDetail>,
}

impl PartnerStatement {
    /// What this partner's rows come to, in whole dollars — their box 18c.
    pub fn total(&self) -> i64 {
        cents_to_dollars(self.rows.iter().map(|r| r.cents).sum())
    }
}

/// The partners a return actually files a K-1 for, and their shares.
///
/// The filter is [`crate::tax::capital::for_return`]'s, and for the same reason:
/// the statements have to be apportioned over the same set item L row 4 is, or a
/// partner's statement and their own capital account disagree about a figure
/// that is meant to explain it.
pub fn for_return(
    conn: &Connection,
    year: i32,
    partners: &[crate::tax::form1065::PartnerFiling],
    components: &[LineDetail],
) -> Vec<PartnerStatement> {
    let (year_start, year_end) = crate::commands::partnership_commands::calendar_year(year);
    let filed: Vec<&Partner> = partners
        .iter()
        .map(|f| &f.partner)
        .filter(|p| p.was_partner_during(year_start, year_end))
        .collect();
    split(conn, year, &filed, components)
}

/// Split every component of line 18c across `partners`.
///
/// Returns one entry per partner, in the same order, including partners whose
/// share came to nothing — the caller decides whether an empty statement is
/// worth a page, and [`crate::tax::statement::build`] already answers no.
///
/// # This depends on the order of `components`
///
/// The running total is walked in the order given, so which partner a rounding
/// dollar lands on — and which row can come out negative — is a function of that
/// order. It is not arbitrary: [`crate::tax::lines::sum_by_line`] sorts each
/// line's detail by descending magnitude, tie-broken by account number, so the
/// same books produce the same statements every time, which is what makes a
/// regenerated return comparable to the one already filed.
///
/// Re-sorting the detail elsewhere would silently redistribute every partner's
/// rows. Every total would still be right and every page would still foot, so
/// nothing would fail — which is exactly why it is written down here.
pub fn split(
    conn: &Connection,
    year: i32,
    partners: &[&Partner],
    components: &[LineDetail],
) -> Vec<PartnerStatement> {
    let mut out: Vec<PartnerStatement> = partners
        .iter()
        .map(|p| PartnerStatement {
            partner_id: p.partner_id.clone(),
            partner_name: p.name.clone(),
            rows: Vec::new(),
        })
        .collect();
    if partners.is_empty() || components.is_empty() {
        return out;
    }

    // §706(d), read once. Every prefix below is split on these percentages, and
    // they are the percentages [`crate::tax::capital`] puts in item L row 4 and
    // `split_across_partners` puts in box 18 code C — same basis, same line, so
    // the three cannot disagree about who held what when.
    //
    // Weighted by line 18c and not by the whole of Schedule K. That distinction
    // is not cosmetic: on a partnership that earned evenly and bought its meals
    // in one half of the year, weighting by `k_analysis` put a statement of 140
    // beside a box of 180 on a $200 line.
    //
    // Line 18c is positive — an expense, printed positive — so each prefix
    // travels on the profit percentages. A component big enough to run the
    // running total negative (an expense account in net credit) puts that prefix
    // on the loss percentages instead, which is the rule [`super::allocate`]
    // applies to every other figure and is deliberate rather than an artefact.
    let year_split = super::varying::year_split(
        conn,
        year,
        partners,
        Basis::ProfitOrLoss,
        super::lines::NONDEDUCTIBLE_LINE,
    );

    // The allocation so far, per partner. Each component's share is the step
    // this list takes when that component is added to the running total — see
    // the module docs for why the increments and not the components themselves
    // are what gets rounded.
    let mut allocated_so_far = vec![0i64; partners.len()];
    let mut running_cents = 0i64;

    for component in components {
        running_cents += component.cents;
        // Rounded from the running cents, not summed from rounded components:
        // the same order the box's own figure is computed in, so the last step
        // of this loop lands exactly on it.
        let so_far_dollars = cents_to_dollars(running_cents);
        let shares = year_split.allocate(so_far_dollars);

        let mut cumulative = vec![0i64; partners.len()];
        for share in shares {
            cumulative[share.partner] = share.dollars;
        }

        for (i, statement) in out.iter_mut().enumerate() {
            let mine = cumulative[i] - allocated_so_far[i];
            if mine == 0 {
                continue;
            }
            statement.rows.push(LineDetail {
                account_id: component.account_id.clone(),
                account_number: component.account_number.clone(),
                // Already suffixed "— 50% disallowed" by `lines::sum_by_line`,
                // which is the part of the account this row is about.
                account_name: component.account_name.clone(),
                cents: mine * 100,
            });
        }
        allocated_so_far = cumulative;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Address, Shares};
    use crate::events::types::{Event, EventEnvelope, PartnerAdmittedData};
    use crate::store::event_store::EventStore;
    use crate::store::projections::ProjectionStore;
    use chrono::NaiveDate;

    const YEAR: i32 = 2023;

    fn address() -> Address {
        Address {
            street: "2 Other Road".into(),
            suite: None,
            city: "Cape Town".into(),
            state: "WC".into(),
            postal_code: "8001".into(),
            country: None,
        }
    }

    /// Two partners on the real books' percentages: 51/49 on profit.
    fn books(splits: &[(&str, f64)]) -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        for (name, pct) in splits {
            let e = Event::PartnerAdmitted(Box::new(PartnerAdmittedData {
                partner_id: name.to_lowercase(),
                name: (*name).into(),
                partner_type: "general".into(),
                residency: "domestic".into(),
                entity_type: "Individual".into(),
                address: (&address()).into(),
                start_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                shares: Shares::from_percents(*pct, *pct, *pct).into(),
            }));
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        }
        store
    }

    /// The real books' shape: a partner whose profit and loss shares are
    /// nothing like each other. Jinny takes 51% of the profit and none of the
    /// loss; Zachary takes 49% of the profit and all of the loss.
    fn books_with(splits: &[(&str, f64, f64)]) -> EventStore {
        let mut store = EventStore::in_memory().unwrap();
        crate::store::migrations::init_schema(store.connection()).unwrap();
        for (name, profit, loss) in splits {
            let e = Event::PartnerAdmitted(Box::new(PartnerAdmittedData {
                partner_id: name.to_lowercase(),
                name: (*name).into(),
                partner_type: "general".into(),
                residency: "domestic".into(),
                entity_type: "Individual".into(),
                address: (&address()).into(),
                start_date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                shares: Shares::from_percents(*profit, *loss, *profit).into(),
            }));
            let stored = store.append(EventEnvelope::new(e, "u".into())).unwrap();
            store.apply_projection(&stored).unwrap();
        }
        store
    }

    fn partners(store: &EventStore) -> Vec<Partner> {
        let mut ps = crate::commands::partnership_commands::list_partners(store.connection());
        ps.sort_by(|a, b| a.partner_id.cmp(&b.partner_id));
        ps
    }

    fn component(number: &str, name: &str, cents: i64) -> LineDetail {
        LineDetail {
            account_id: number.to_string(),
            account_number: number.to_string(),
            account_name: name.to_string(),
            cents,
        }
    }

    fn run(store: &EventStore, components: &[LineDetail]) -> Vec<PartnerStatement> {
        let ps = partners(store);
        let refs: Vec<&Partner> = ps.iter().collect();
        split(store.connection(), YEAR, &refs, components)
    }

    /// The real scenario this was written against: one meals account, $279.12,
    /// half of it disallowed, two partners at 51 and 49.
    #[test]
    fn the_one_component_case_splits_on_the_profit_percentages() {
        let store = books(&[("Jinny", 51.0), ("Zachary", 49.0)]);
        let rows = run(
            &store,
            &[component("3055", "Partner meals — 50% disallowed", 139_56)],
        );

        let total: i64 = rows.iter().map(|r| r.total()).sum();
        assert_eq!(
            total, 140,
            "the box is 140 and the statements have to be it"
        );
        assert_eq!(rows.iter().map(|r| r.rows.len()).sum::<usize>(), 2);
    }

    /// The property the whole module is arranged around: down a column the
    /// partners' shares are the component, along a row a partner's components
    /// are their box. Both, on figures chosen so the rounding actually bites.
    #[test]
    fn the_rows_foot_in_both_directions() {
        let store = books(&[("Ada", 33.0), ("Bea", 33.0), ("Cyd", 34.0)]);
        let components = [
            component("6100", "Meals — 50% disallowed", 100_01),
            component("6200", "Fines", 33_33),
            component("6300", "Political contributions", 66_66),
        ];
        let statements = run(&store, &components);

        // Along the rows: each partner's components are their own box figure.
        let entity_total = cents_to_dollars(components.iter().map(|c| c.cents).sum());
        let boxes: i64 = statements.iter().map(|s| s.total()).sum();
        assert_eq!(boxes, entity_total, "{statements:?}");

        // Down the columns: every partner's share of one component adds back to
        // what that component contributed to the box.
        let mut running = 0i64;
        let mut previous = 0i64;
        for (i, c) in components.iter().enumerate() {
            running += c.cents;
            let contributed = cents_to_dollars(running) - previous;
            previous = cents_to_dollars(running);
            let column: i64 = statements
                .iter()
                .map(|s| {
                    s.rows
                        .iter()
                        .filter(|r| r.account_number == c.account_number)
                        .map(|r| r.cents / 100)
                        .sum::<i64>()
                })
                .sum();
            assert_eq!(
                column, contributed,
                "component {i} ({}) does not add back",
                c.account_number
            );
        }
    }

    /// Largest-remainder is not monotone, so an increment can be negative: the
    /// dollar left over moves from one partner to another between two prefixes
    /// and the partner who loses it comes out a dollar short on the second
    /// component. Documented, warned about, and deliberately not smoothed away —
    /// see the module docs for what the alternatives cost.
    ///
    /// 49/49/2 with components of $17 and $1 is the smallest case anybody found.
    /// At $17 the third partner is owed the leftover dollar (.34 beats .33); at
    /// $18 the first two are (.82 each), so the third gives it back.
    #[test]
    fn a_row_can_come_out_negative_and_both_margins_still_foot() {
        let store = books(&[("Ada", 49.0), ("Bea", 49.0), ("Cyd", 2.0)]);
        let components = [
            component("6100", "Meals — 50% disallowed", 17_00),
            component("6200", "Fines", 1_00),
        ];
        let statements = run(&store, &components);

        let cyd = statements.iter().find(|s| s.partner_id == "cyd").unwrap();
        let fine = cyd
            .rows
            .iter()
            .find(|r| r.account_number == "6200")
            .expect("Cyd has a row for the fine");
        assert_eq!(fine.cents, -100, "{:?}", cyd.rows);

        // And it is still right: her own rows total her box, and every partner's
        // share of each component still totals that component.
        assert_eq!(cyd.total(), 0, "one dollar taken back off one dollar");
        assert_eq!(
            statements.iter().map(|s| s.total()).sum::<i64>(),
            18,
            "the partners are the whole of the line: {statements:?}"
        );
        for (number, added) in [("6100", 17i64), ("6200", 1)] {
            let column: i64 = statements
                .iter()
                .flat_map(|s| s.rows.iter())
                .filter(|r| r.account_number == number)
                .map(|r| r.cents / 100)
                .sum();
            assert_eq!(column, added, "component {number} does not add back");
        }
    }

    /// A partner allocated nothing gets no rows, so no page is drawn for them.
    /// A statement of zeros attached to a K-1 is a figure somebody has to go and
    /// check before they can conclude it means nothing.
    #[test]
    fn a_partner_with_no_share_gets_no_rows() {
        let store = books(&[("All", 100.0), ("None", 0.0)]);
        let statements = run(&store, &[component("6100", "Meals", 500_00)]);
        let none = statements
            .iter()
            .find(|s| s.partner_id == "none")
            .expect("both partners come back");
        assert!(none.rows.is_empty());
        assert_eq!(none.total(), 0);
    }

    /// Nothing on line 18c is not an empty statement — it is no statement.
    #[test]
    fn no_components_produce_no_rows() {
        let store = books(&[("Jinny", 51.0), ("Zachary", 49.0)]);
        let statements = run(&store, &[]);
        assert_eq!(statements.len(), 2);
        assert!(statements.iter().all(|s| s.rows.is_empty()));
    }

    // -----------------------------------------------------------------------
    // Attack tests. Everything below here was written against the finished
    // module rather than beside it.
    // -----------------------------------------------------------------------

    /// **The mistake this file exists to catch.** Line 18c is an expense and is
    /// carried positive, so it travels on the *profit* percentages. The real
    /// books have a partner on 51% of the profit and 0% of the loss and another
    /// on 49% and 100%; splitting a positive 18c on the loss shares would put
    /// the whole of it on one partner and nothing on the other, and every total
    /// on the return would still foot.
    #[test]
    fn a_positive_18c_splits_on_profit_and_not_on_loss() {
        let store = books_with(&[("Jinny", 51.0, 0.0), ("Zachary", 49.0, 100.0)]);
        let ps = partners(&store);
        let refs: Vec<&Partner> = ps.iter().collect();
        let statements = split(
            store.connection(),
            YEAR,
            &refs,
            &[component("3055", "Partner meals — 50% disallowed", 139_56)],
        );

        let jinny = statements.iter().find(|s| s.partner_id == "jinny").unwrap();
        let zachary = statements
            .iter()
            .find(|s| s.partner_id == "zachary")
            .unwrap();
        assert_eq!(
            jinny.total(),
            71,
            "51% of 140, not 0% of it: {statements:?}"
        );
        assert_eq!(
            zachary.total(),
            69,
            "49% of 140, not the whole of it: {statements:?}"
        );
        assert_eq!(jinny.total() + zachary.total(), 140);
    }

    /// The same partners, and the same figure carried the wrong way round. A
    /// *negative* 18c is a limited expense account in net credit, and the
    /// allocator's own rule then puts it on the loss shares — which for these
    /// partners is the whole of it on one of them. Asserted so that if the sign
    /// convention is ever reversed, the 51/49 test above is not the only thing
    /// standing in the way: this one starts failing too, in the opposite
    /// direction.
    #[test]
    fn a_negative_18c_travels_on_the_loss_shares_instead() {
        let store = books_with(&[("Jinny", 51.0, 0.0), ("Zachary", 49.0, 100.0)]);
        let ps = partners(&store);
        let refs: Vec<&Partner> = ps.iter().collect();
        let statements = split(
            store.connection(),
            YEAR,
            &refs,
            &[component("3055", "Refunded meals", -100_00)],
        );

        let jinny = statements.iter().find(|s| s.partner_id == "jinny").unwrap();
        let zachary = statements
            .iter()
            .find(|s| s.partner_id == "zachary")
            .unwrap();
        assert_eq!(jinny.total(), 0, "no loss share: {statements:?}");
        assert_eq!(zachary.total(), -100, "all of the loss: {statements:?}");
    }

    /// Both directions of footing, over a grid rather than one example: several
    /// component lists chosen so the rounding bites, against several splits
    /// including ones where the percentages do not divide.
    ///
    /// The column property is stated the way the module actually guarantees it —
    /// a component's shares sum to the *increment* the rounded running total
    /// takes, not to that component rounded on its own. The two are not always
    /// the same figure; `a_components_own_rounding_is_not_what_the_column_shows`
    /// below pins down where they part company.
    #[test]
    fn both_directions_foot_over_a_grid_of_awkward_figures() {
        let component_sets: Vec<Vec<LineDetail>> = vec![
            vec![component("a", "one cent", 1)],
            vec![component("a", "three cents", 3)],
            vec![
                component("a", "a", 1),
                component("b", "b", 1),
                component("c", "c", 1),
            ],
            vec![
                component("a", "a", 33_33),
                component("b", "b", 33_33),
                component("c", "c", 33_34),
            ],
            vec![
                component("a", "a", 139_56),
                component("b", "b", 0),
                component("c", "c", 60_50),
            ],
            vec![
                component("a", "a", 50),
                component("b", "b", 50),
                component("c", "c", 50),
                component("d", "d", 50),
            ],
            vec![
                component("a", "a", 100_01),
                component("b", "b", -40_00),
                component("c", "c", 66_66),
            ],
            vec![component("a", "a", 4_00), component("b", "b", 1_00)],
            vec![component("a", "a", 0)],
            vec![component("a", "a", 7_777_777_77), component("b", "b", 1)],
        ];
        let splits: Vec<Vec<(&str, f64)>> = vec![
            vec![("Ada", 100.0)],
            vec![("Ada", 51.0), ("Bea", 49.0)],
            vec![("Ada", 50.0), ("Bea", 50.0)],
            vec![("Ada", 33.0), ("Bea", 33.0), ("Cyd", 34.0)],
            vec![("Ada", 12.0), ("Bea", 34.0), ("Cyd", 54.0)],
            vec![("Ada", 1.0), ("Bea", 1.0), ("Cyd", 98.0)],
            vec![("Ada", 25.0), ("Bea", 25.0), ("Cyd", 25.0), ("Dee", 25.0)],
        ];

        for split_def in &splits {
            let store = books(split_def);
            for components in &component_sets {
                let statements = run(&store, components);
                let case = format!(
                    "{split_def:?} × {:?}",
                    components.iter().map(|c| c.cents).collect::<Vec<_>>()
                );

                // Along a row: the partners' boxes add back to Schedule K's own
                // figure, which is the whole rounded once.
                let entity = cents_to_dollars(components.iter().map(|c| c.cents).sum());
                let boxes: i64 = statements.iter().map(|s| s.total()).sum();
                assert_eq!(boxes, entity, "boxes do not foot: {case}");

                // Down a column: component k's shares are the step the running
                // allocation took when k was added.
                let mut running = 0i64;
                let mut previous = 0i64;
                for c in components {
                    running += c.cents;
                    let step = cents_to_dollars(running) - previous;
                    previous = cents_to_dollars(running);
                    let column: i64 = statements
                        .iter()
                        .flat_map(|s| s.rows.iter())
                        .filter(|r| r.account_number == c.account_number)
                        .map(|r| r.cents / 100)
                        .sum();
                    assert_eq!(
                        column, step,
                        "column {} does not foot: {case}",
                        c.account_number
                    );
                }

                // And nothing is ever a fraction of a dollar, whatever the
                // figures: the drawing code rounds what it is given and a second
                // rounding would unfoot both directions at once.
                for s in &statements {
                    for r in &s.rows {
                        assert_eq!(r.cents % 100, 0, "{r:?}: {case}");
                    }
                }
            }
        }
    }

    /// **The linchpin.** The statement is meant to be the explanation for item L
    /// row 4, so it has to total to row 4 — for every partner, on every split.
    /// The two are computed by different code on different inputs (one splits a
    /// single rounded figure, the other telescopes a list of components) and
    /// nothing but this arithmetic makes them agree.
    #[test]
    fn every_partners_statement_totals_to_their_own_item_l_row_four() {
        use crate::tax::capital;

        let component_sets: Vec<Vec<LineDetail>> = vec![
            vec![component("a", "a", 139_56)],
            vec![
                component("a", "a", 100_01),
                component("b", "b", 33_33),
                component("c", "c", 66_66),
            ],
            vec![
                component("a", "a", 50),
                component("b", "b", 50),
                component("c", "c", 1),
            ],
        ];
        for split_def in [
            vec![("Jinny", 51.0), ("Zachary", 49.0)],
            vec![("Ada", 33.0), ("Bea", 33.0), ("Cyd", 34.0)],
            vec![("Ada", 12.0), ("Bea", 34.0), ("Cyd", 54.0)],
        ] {
            let store = books(&split_def);
            let ps = partners(&store);
            let refs: Vec<&Partner> = ps.iter().collect();
            for components in &component_sets {
                let statements = split(store.connection(), YEAR, &refs, components);
                // What `build_return_from_ledger` hands `capital`: the same sum
                // of cents, rounded once.
                let line_18c = cents_to_dollars(components.iter().map(|c| c.cents).sum());
                let capital =
                    capital::compute(store.connection(), YEAR, &refs, 0, line_18c).unwrap();

                for s in &statements {
                    let row_four = capital
                        .for_partner(&s.partner_id)
                        .unwrap_or_else(|| panic!("{} has no capital account", s.partner_id))
                        .other;
                    assert_eq!(
                        s.total(),
                        -row_four,
                        "{}'s statement and their item L row 4 disagree on {split_def:?}",
                        s.partner_id
                    );
                }
            }
        }
    }

    /// A single component is the degenerate case of the telescoping sum, and the
    /// one the real books are: each partner's only row *is* their box.
    #[test]
    fn one_component_makes_every_partner_a_single_row_equal_to_their_box() {
        let store = books(&[("Ada", 33.0), ("Bea", 33.0), ("Cyd", 34.0)]);
        let statements = run(&store, &[component("6100", "Meals", 1_000_00)]);
        for s in &statements {
            assert_eq!(s.rows.len(), 1, "{s:?}");
            assert_eq!(s.rows[0].cents / 100, s.total());
        }
        assert_eq!(statements.iter().map(|s| s.total()).sum::<i64>(), 1_000);
    }

    /// A component of nothing is not a row on anybody's statement. It reaches
    /// the allocator — the running total is unchanged by it — and an account
    /// that netted to zero has nothing to explain.
    #[test]
    fn a_zero_component_gets_no_row_from_anybody() {
        let store = books(&[("Ada", 51.0), ("Bea", 49.0)]);
        let statements = run(
            &store,
            &[
                component("6100", "Meals", 139_56),
                component("6200", "Fines", 0),
                component("6300", "Politics", 60_44),
            ],
        );
        for s in &statements {
            assert!(
                !s.rows.iter().any(|r| r.account_number == "6200"),
                "a zero component was printed: {s:?}"
            );
        }
        assert_eq!(statements.iter().map(|s| s.total()).sum::<i64>(), 200);
    }

    /// **A known artefact, pinned down rather than claimed away.**
    ///
    /// The column property is about the *increment* of the rounded running
    /// total, and that is not the same figure as the component rounded on its
    /// own. Two fifty-cent components round to a dollar each in isolation and to
    /// one dollar together, so the partners' second column is empty while the
    /// partnership's own statement page prints a dollar against that account.
    ///
    /// Both pages are internally consistent and the return foots; they disagree
    /// with each other, account by account, by up to a dollar. Recorded here so
    /// the disagreement is a known quantity rather than a surprise in an audit.
    #[test]
    fn a_components_own_rounding_is_not_what_the_column_shows() {
        let store = books(&[("Ada", 100.0)]);
        let statements = run(
            &store,
            &[
                component("a", "first half-dollar", 50),
                component("b", "second half-dollar", 50),
            ],
        );
        let ada = &statements[0];
        assert_eq!(ada.total(), 1, "the box is one dollar");
        assert_eq!(
            ada.rows.len(),
            1,
            "and only one of the two accounts gets a row: {ada:?}"
        );
        assert_eq!(ada.rows[0].account_number, "a");
        // What the entity's own statement page would print against each of them.
        assert_eq!(cents_to_dollars(50), 1, "each is a dollar on its own");
    }

    /// **A second known artefact.** Largest-remainder allocation is not monotone
    /// in the total — the Alabama paradox — so a partner's cumulative share can
    /// *fall* when a positive component is added to the running total. Their
    /// statement then carries a negative row against an expense account.
    ///
    /// The arithmetic is right: 12% of five dollars is sixty cents, so this
    /// partner's box is correctly zero. The presentation is not: a reader sees
    /// a dollar of meals and minus a dollar of fines where the truth is that
    /// neither rounded to anything.
    #[test]
    fn a_positive_component_can_produce_a_negative_row() {
        let store = books(&[("Ada", 12.0), ("Bea", 34.0), ("Cyd", 54.0)]);
        let statements = run(
            &store,
            &[
                component("6100", "Meals", 4_00),
                component("6200", "Fines", 1_00),
            ],
        );
        let ada = statements.iter().find(|s| s.partner_id == "ada").unwrap();
        assert_eq!(
            ada.total(),
            0,
            "12% of $5 rounds to nothing: {statements:?}"
        );
        let fines: i64 = ada
            .rows
            .iter()
            .filter(|r| r.account_number == "6200")
            .map(|r| r.cents)
            .sum();
        assert_eq!(
            fines, -100,
            "a dollar of fines shows as minus a dollar on this partner's page: {ada:?}"
        );
        // The totals are still right in both directions, which is why this is a
        // presentation defect and not an arithmetic one.
        assert_eq!(statements.iter().map(|s| s.total()).sum::<i64>(), 5);
    }

    /// Percentages that do not total the whole are not scaled up here, and the
    /// statements must not invent the shortfall either — they have to match
    /// whatever the box does, which is to come up short and be warned about
    /// elsewhere.
    #[test]
    fn shares_that_do_not_total_the_whole_come_up_short_rather_than_being_scaled() {
        let store = books(&[("Ada", 40.0), ("Bea", 40.0)]);
        let statements = run(&store, &[component("6100", "Meals", 1_000_00)]);
        assert_eq!(
            statements.iter().map(|s| s.total()).sum::<i64>(),
            800,
            "80% of the whole, not the whole: {statements:?}"
        );
    }

    /// The figures are whole dollars in a field that holds cents, and the drawing
    /// code rounds what it is given. A row that was not a whole number of dollars
    /// would be rounded a second time and the column would stop footing.
    #[test]
    fn every_row_is_a_whole_number_of_dollars() {
        let store = books(&[("Jinny", 51.0), ("Zachary", 49.0)]);
        let statements = run(
            &store,
            &[component("3055", "Partner meals — 50% disallowed", 139_56)],
        );
        for s in &statements {
            for r in &s.rows {
                assert_eq!(r.cents % 100, 0, "{r:?} is not whole dollars");
            }
        }
    }
}
