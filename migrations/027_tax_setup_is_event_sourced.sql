-- Make the Form 1065 setup — which account reports where, and what Schedule B
-- was answered — part of the event log rather than local config.
--
-- # Why this changed
--
-- Both tables were local: written by direct SQL, never replicated. That was a
-- defensible reading of "a return is prepared on one machine" — the same
-- argument that keeps `partner_tins` local (migration 023). It was the wrong
-- reading. A TIN is a secret and belongs on exactly one machine; which account
-- reports on line 21 is a *fact about the partnership*, and `business_profile`
-- and `partners` — the same Form 1065 data, equally "preparation" — have been
-- event-sourced since migration 023.
--
-- The symptom was the obvious one: open the books on a second machine and the
-- assignments and answers are simply absent, with nothing on screen saying why.
--
-- # What this migration does
--
-- 1. Adds `updated_at_event` to both tables, so a row records which event put
--    it there, exactly as `business_profile` and `partners` do.
--
-- 2. Copies every existing row into a staging table. `Projector::rebuild`
--    truncates projections, and from this migration on these two are
--    projections — so a row that predates the log would be destroyed by the
--    next rebuild. Staging holds it until the store adopts it, which happens on
--    the next writable open: one event per staged row, after which the staging
--    table is emptied and never used again.
--
-- Staging rather than adopting in place because a migration has no event store.
-- It can copy rows; it cannot hash and append an event.
--
-- # Why the staged rows are not deleted from the live tables
--
-- They stay readable. Between this migration and the adoption that follows it,
-- the setup keeps working exactly as before — the tables are still there, still
-- populated, still queried the same way. Adoption then overwrites each row with
-- an identical one carrying an event id. If adoption never happens (a read-only
-- replica, say) the rows survive until a rebuild, and the staging table is the
-- record of what was lost.
--
-- No foreign key to `accounts` or `partners`, still — see migration 025. The
-- new `updated_at_event` does reference `events`, which is safe: rebuild
-- truncates projections, and the event log is not one.

ALTER TABLE tax_line_mappings ADD COLUMN updated_at_event INTEGER REFERENCES events(id);
ALTER TABLE schedule_b_answers ADD COLUMN updated_at_event INTEGER REFERENCES events(id);

CREATE TABLE IF NOT EXISTS tax_line_mappings_pending_adoption (
    account_id TEXT PRIMARY KEY,
    line_key TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS schedule_b_answers_pending_adoption (
    tax_year INTEGER NOT NULL,
    answer_key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (tax_year, answer_key)
);

INSERT OR IGNORE INTO tax_line_mappings_pending_adoption (account_id, line_key)
SELECT account_id, line_key FROM tax_line_mappings;

INSERT OR IGNORE INTO schedule_b_answers_pending_adoption (tax_year, answer_key, value)
SELECT tax_year, answer_key, value FROM schedule_b_answers;
