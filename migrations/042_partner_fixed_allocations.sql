-- A partner's share of one year's result, fixed in dollars rather than by
-- percentage.
--
-- # Why this exists
--
-- Allocations run on the percentages on file, and most years that is the whole
-- story. The exception is an agreement that divides a year by amount — a
-- departing partner takes what they were paid out, another partner the rest —
-- which no set of percentages expresses. Schedule K line 1, item L row 3 and the
-- closing allocation read these rows; every other line still follows the
-- percentages.
--
-- `amount_cents` NULL is the partner who takes whatever the fixed amounts leave;
-- at most one per year, which the command enforces. The note is required: a
-- split that departs from the percentages is one somebody may have to show came
-- from the partnership agreement.
CREATE TABLE IF NOT EXISTS partner_fixed_allocations (
    tax_year         INTEGER NOT NULL,
    partner_id       TEXT NOT NULL,
    amount_cents     INTEGER,
    note             TEXT NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (tax_year, partner_id)
);
