-- The transaction-by-transaction detail of a received statement: the Form 8949
-- rows a 1099-B's own subtotals are not enough for.
--
-- INVESTMENTS-SPEC.md §8, phase 6.
--
-- # Why a statement needs lines at all
--
-- `tax_statements` holds a statement as box code → cents, which is everything a
-- W-2, a 1099-INT or a K-1 says. A 1099-B is different: it reports subtotals per
-- Form 8949 category, and for three of the six categories — and for any
-- transaction the broker adjusted — the form requires each sale to be *listed*.
-- A list of sales is not a map of box codes to amounts, so it gets a table.
--
-- This is deliberately not a second statement store. A line belongs to a
-- statement, is entered against it, disappears with it, and is replayed from the
-- same log. The 1099-B's authority (spec §8) stays with the statement's category
-- subtotals; these rows are how the form prints them.
--
-- # Why the covered categories usually have no rows here
--
-- Categories A (short-term) and D (long-term) are basis-reported-to-the-IRS. With
-- no adjustment on them, their subtotals go straight onto Schedule D lines 1a and
-- 8a and **no Form 8949 is filed at all**. For an ordinary brokerage year that is
-- the whole return, and this table stays empty. It fills for the noncovered
-- categories (B, E), for anything not reported on a 1099-B (C, F), and for a
-- transaction carrying a wash sale, a basis correction, or an inherited or gifted
-- holding.
--
-- # Why the gain is not a column
--
-- Column (h) is proceeds − basis + adjustment, exactly, on every row. Storing it
-- would be storing a subtraction, and a stored subtraction is a second answer to
-- a question that already has one. The statement's *category* gain is stored,
-- because that one is a figure the broker printed and worth checking against.
--
-- A projection like the rest: truncated and replayed by `Projector::rebuild`.

CREATE TABLE IF NOT EXISTS tax_statement_lines (
    statement_id     TEXT NOT NULL,
    -- Stable within the statement, so a corrected entry replaces a row rather
    -- than adding one. The recording command replaces a statement's lines whole.
    line_id          TEXT NOT NULL,
    -- The order the rows were entered in, which is the order Form 8949 prints
    -- them. Kept explicitly: `rowid` is not stable across a rebuild.
    position         INTEGER NOT NULL,
    -- 'a'…'f' — the Form 8949 category, lowercase. Not a CHECK constraint with
    -- the six letters spelled out, because `tax::schedule_d::Category` is the one
    -- place that vocabulary lives and two copies of it would drift; the event's
    -- validation refuses anything else before it reaches here.
    category         TEXT NOT NULL,
    -- Column (a): "100 sh. XYZ Co."
    description      TEXT NOT NULL,
    -- Column (b), one of the two and never both: a date, or a word the form
    -- allows in its place — VARIOUS for a sale across lots bought on different
    -- days, INHERITED for a holding whose basis is its value at death. Which one
    -- applies is a fact about the shares that no ledger holds, so the statement
    -- or the person says (spec §8).
    acquired_on      TEXT,
    acquired_label   TEXT,
    -- Column (c).
    sold_on          TEXT NOT NULL,
    -- Column (d). Signed, because a short sale can settle for less than nothing.
    proceeds_cents   INTEGER NOT NULL,
    -- Column (e).
    basis_cents      INTEGER NOT NULL,
    -- Column (f): 'W' for a wash sale, 'B' for a basis correction, and the rest
    -- of the form's letters. Several may apply, so it is a short string and not
    -- one character.
    adjustment_code  TEXT,
    -- Column (g). Positive increases the gain, which is the direction a wash-sale
    -- disallowed loss goes. Not computed here: spec §4 puts wash sales out of
    -- scope, so this is the broker's own figure, carried.
    adjustment_cents INTEGER NOT NULL DEFAULT 0,
    recorded_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (statement_id, line_id)
);

-- The read a Form 8949 part does: one statement's rows, in order.
CREATE INDEX IF NOT EXISTS idx_tax_statement_lines_order
    ON tax_statement_lines(statement_id, position);
