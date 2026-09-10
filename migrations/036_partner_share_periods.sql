-- What each partner's profit, loss and capital percentages were, and from when.
--
-- # Why a series and not three columns on `partners`
--
-- A partnership's split changes: somebody leaves, somebody is admitted, the
-- agreement is renegotiated. Held as one value per partner, the *current* split
-- is the only one the books can describe — so a prior year's Schedule K-1 shows
-- this year's percentages in item J, and the return is wrong in a way that foots
-- perfectly. `form1065.rs` has carried a warning about exactly that, and this is
-- the fix it was waiting for.
--
-- # Why `effective_from` alone
--
-- The split in force on a date is the row with the greatest `effective_from` on
-- or before it. A from-and-until pair can be written with gaps and overlaps —
-- two rows both claiming the 3rd of June, or neither — and nothing downstream
-- could resolve that. A start date alone cannot be inconsistent with itself.
CREATE TABLE IF NOT EXISTS partner_share_periods (
    partner_id      TEXT NOT NULL REFERENCES partners(id),
    -- The first day this split applies.
    effective_from  TEXT NOT NULL,
    -- Parts per million, so 33.3333% is 333333 and no percentage is ever a float.
    profit_ppm      INTEGER NOT NULL,
    loss_ppm        INTEGER NOT NULL,
    capital_ppm     INTEGER NOT NULL,
    updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (partner_id, effective_from)
);

-- "Who held what on this date" is the question every reader asks.
CREATE INDEX IF NOT EXISTS idx_partner_share_periods_date
    ON partner_share_periods(effective_from);
