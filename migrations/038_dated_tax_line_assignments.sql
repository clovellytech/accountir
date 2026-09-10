-- Which line an account reports on, and how much of it is deductible, as facts
-- with a date rather than facts about now.
--
-- # What was wrong with one row per account
--
-- `tax_line_mappings` held `account_id` as its whole key, so an account was on
-- one line for all time. Remapping it in 2026 silently changed the 2023 return
-- too — a return that had already been filed on the old assignment. Rebuilding
-- any earlier year reconstructed it through today's chart rather than the one it
-- was filed on, which is also why item L's beginning capital carries a caveat
-- instead of being computed from prior years.
--
-- # Why a start year and not a start-and-end pair
--
-- The assignment in force for a year is the row with the greatest
-- `effective_from` at or before it. A from-and-until pair can be written with a
-- gap or an overlap — two rows both claiming 2024, or neither — and nothing
-- downstream could resolve that. A start alone cannot be inconsistent with
-- itself, and a year with no row of its own inherits the most recent earlier
-- one, so a new tax year does not start with an unmapped chart.
--
-- The same shape as `partner_share_periods`, and for the same reasons.

-- `effective_from = 0` means "as far back as these books go": the assignment was
-- made before assignments were dated. Every existing row gets it, so nothing
-- about what any year already produces changes when this migration runs.
--
-- # Why this rebuilds rather than adding a column
--
-- SQLite cannot widen a primary key in place, and an `ALTER TABLE ... ADD
-- COLUMN` here would be worse than useless: on a database built by
-- `init_schema` the column already exists, the statement fails with "duplicate
-- column", and the migration runner treats that as "already applied" and skips
-- **everything after it** — leaving the primary key un-widened and every
-- `ON CONFLICT (account_id, effective_from)` in the projector failing at
-- runtime. So the year is supplied as a literal instead, which works whether or
-- not the old table had the column.
DROP TABLE IF EXISTS tax_line_mappings_dated;
CREATE TABLE tax_line_mappings_dated (
    account_id       TEXT NOT NULL,
    effective_from   INTEGER NOT NULL,
    -- A key from `tax::lines::MAPPABLE_LINES`, e.g. 'l1a', 'l21'. Not a foreign
    -- key: the canonical list is code, because it changes with the form and not
    -- with the books. `tax::lines::OFF_RETURN` means the account is deliberately
    -- on no line from that year.
    line_key         TEXT NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (account_id, effective_from)
);
INSERT INTO tax_line_mappings_dated (account_id, effective_from, line_key, updated_at)
    SELECT account_id, 0, line_key, updated_at FROM tax_line_mappings;
DROP TABLE tax_line_mappings;
ALTER TABLE tax_line_mappings_dated RENAME TO tax_line_mappings;

DROP TABLE IF EXISTS tax_deduction_limits_dated;
CREATE TABLE tax_deduction_limits_dated (
    account_id       TEXT NOT NULL,
    effective_from   INTEGER NOT NULL,
    -- 0-100. The rest of the balance is not deducted anywhere: on Form 1065 it
    -- is reported on Schedule K line 18c so it reduces each partner's basis.
    deductible_pct   INTEGER NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (account_id, effective_from)
);
INSERT INTO tax_deduction_limits_dated
        (account_id, effective_from, deductible_pct, updated_at)
    SELECT account_id, 0, deductible_pct, updated_at FROM tax_deduction_limits;
DROP TABLE tax_deduction_limits;
ALTER TABLE tax_deduction_limits_dated RENAME TO tax_deduction_limits;

-- "Every account on line 21, for this year" is the reporting direction.
CREATE INDEX IF NOT EXISTS idx_tax_line_mappings_line ON tax_line_mappings(line_key);
CREATE INDEX IF NOT EXISTS idx_tax_line_mappings_year ON tax_line_mappings(effective_from);
CREATE INDEX IF NOT EXISTS idx_tax_deduction_limits_year ON tax_deduction_limits(effective_from);
