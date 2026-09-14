-- One year's depreciation on one asset, fixed by hand, with the reason.
--
-- # Why this exists
--
-- The register computes depreciation from the statute's tables, and most years
-- that is the whole story. The exception is a return already filed on a figure
-- the tables do not give — a 39.5-year life, a wrong month — where the books have
-- to carry what the return claimed until it is amended. An override records that
-- year's figure and why, and everything downstream (the posting, Form 4562, the
-- accumulated depreciation later years build on) reads it.
--
-- # Why a note is required
--
-- An override with no reason cannot be told apart from a typo, and the figure it
-- sets is one somebody may have to defend. The column is NOT NULL and validation
-- refuses an empty one.
CREATE TABLE IF NOT EXISTS depreciation_overrides (
    asset_id         TEXT NOT NULL,
    tax_year         INTEGER NOT NULL,
    -- Bonus plus MACRS for the year, in cents. §179 is not included: it is an
    -- election of its own, recorded on the asset.
    amount_cents     INTEGER NOT NULL,
    note             TEXT NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (asset_id, tax_year)
);
