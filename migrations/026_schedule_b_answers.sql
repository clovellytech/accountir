-- The answers to Schedule B, "Other Information", kept per tax year.
--
-- # Why per tax year and not one row like `business_profile`
--
-- Almost every question on the schedule is scoped to a year by its own wording
-- — "At any time during the tax year, did the partnership…". An answer stored
-- once and reused would be last year's answer presented as this year's, on a
-- signed return, with nothing on screen saying it was carried over. The tax
-- year is therefore half the key, and a year nobody has answered yet starts
-- empty rather than inheriting.
--
-- # Why this is local config and not an event
--
-- Same argument as `partner_tins` (migration 023) and `tax_line_mappings`
-- (024): a return is prepared on one machine — the one that has the TINs — and
-- these answers are part of preparing it. Putting them in the log would
-- replicate a half-finished return to every member's laptop, where the only
-- thing anyone could do with it is file a different copy.
--
-- # Why `value` is TEXT for every question
--
-- The schedule asks yes/no, pick-one, and "if yes, how many" in the same
-- numbered sequence, and the shapes move between form revisions — 10e is
-- "reserved for future use" this year and will be a real question in some
-- later one. A typed column per question would mean a migration every time the
-- IRS renumbers. The catalogue in `tax::schedule_b` owns what a key means and
-- validates on the way in; this table owns only that an answer was given.
--
-- No foreign key anywhere, deliberately — see migration 025. Nothing here
-- points at a projection, and it must stay that way: `Projector::rebuild`
-- truncates projections, and a config table that blocks rebuild is a recovery
-- path disabled by ordinary use.
CREATE TABLE IF NOT EXISTS schedule_b_answers (
    tax_year INTEGER NOT NULL,
    -- A key from `tax::schedule_b::QUESTIONS`, e.g. 'b1', 'b10a', 'b10a_date'.
    -- Not a foreign key: the canonical list is code, because it changes with
    -- the form and not with the books.
    answer_key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (tax_year, answer_key)
);

-- "Show me everything answered for 2025" is the only read this table has, and
-- it is the primary key's own prefix, so no further index is needed.
