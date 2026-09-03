-- Family ties between partners, for Schedule B-1's §267(c) constructive-ownership
-- test.
--
-- Schedule B-1 lists partners owning 50% or more, applying the constructive
-- ownership rules of IRC §267(c): a partner is treated as owning what their
-- family owns. Two spouses at 40% and 20% each own 60% and both belong on the
-- schedule, but the books could not see it — they held partners, not the ties
-- between them. This table is those ties.
--
-- Event-sourced like `partners` (migration 023), not local like `partner_tins`:
-- which two partners are married is a fact the whole partnership relies on to file
-- correctly, not a secret that belongs on one machine. So it is a projection of
-- `PartnerRelationshipSet` / `PartnerRelationshipCleared`, and every member sees
-- the same ties.
--
-- The pair is the key. For a symmetric kind (spouse, sibling) the two ids are
-- stored in canonical order before the event is built, so "Alice spouse of Bob"
-- and "Bob spouse of Alice" are the one row this primary key allows rather than
-- two half-duplicates. For `parent_of` the order is the meaning — the first id is
-- the parent — and the pair is still unique.
--
-- No foreign key to `partners`, deliberately, for the reason migration 025 gives:
-- `Projector::rebuild` truncates projections and replays, and a foreign key would
-- fight the order rows come back in. `updated_at_event` referencing `events` is
-- safe — the log is not a projection.
CREATE TABLE IF NOT EXISTS partner_relationships (
    partner_id TEXT NOT NULL,
    related_partner_id TEXT NOT NULL,
    -- 'spouse', 'sibling', or 'parent_of' — see domain::RelationshipKind.
    relationship TEXT NOT NULL,
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (partner_id, related_partner_id)
);
