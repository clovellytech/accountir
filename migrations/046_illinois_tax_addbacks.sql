-- Accounts that hold Illinois income or replacement tax, from a tax year on.
--
-- # Why this exists
--
-- The replacement tax a partnership pays is deducted on its federal return like
-- any other tax, and Illinois then adds it back: IL-1065 line 16, "Illinois
-- taxes deducted in arriving at Line 14". The ledger knows the account the tax
-- was posted to and the federal line it reaches; only this says that the
-- account is Illinois tax, so line 16 can add back what was deducted from it.
--
-- Dated like `tax_statement_groups`, with a stored no rather than a deleted
-- row, so "not from 2027" is a fact of its own and an earlier return is left as
-- it was filed.
CREATE TABLE IF NOT EXISTS il_tax_addbacks (
    account_id       TEXT NOT NULL,
    effective_from   INTEGER NOT NULL,
    -- 1: added back on IL-1065 line 16 from this year. 0: not from this year.
    added_back       INTEGER NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (account_id, effective_from)
);
