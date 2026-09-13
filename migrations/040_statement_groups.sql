-- Which parent accounts print as one row on the statements attached to a return.
--
-- # Why this exists
--
-- Line 21's statement lists every account that reaches the line. On a chart
-- that files twelve subscriptions under "Software", that is twelve rows where
-- the filer may want one. The chart already says they belong together; this
-- says, per parent, that the statement should too. The line's figure does not
-- change — a grouped row relocates amounts, it never adds or loses one.
--
-- # Why a yes-or-no rather than a row that exists or does not
--
-- Dated like `tax_deduction_limits`: the value in force for a year is the row
-- with the greatest `effective_from` at or before it. If only "yes" were stored,
-- stopping grouping from 2025 would have to delete a row, and deleting 2025's
-- row falls back to 2023's yes. Storing the no makes "not from 2025" a fact of
-- its own, and a statement attached to a 2023 return stays as it was filed.
CREATE TABLE IF NOT EXISTS tax_statement_groups (
    account_id       TEXT NOT NULL,
    effective_from   INTEGER NOT NULL,
    -- 1: the account's children print as one row with it. 0: a row each.
    grouped          INTEGER NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (account_id, effective_from)
);
