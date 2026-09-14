-- A change to a depreciable asset's basis after it was bought, with the reason.
--
-- # Why this exists
--
-- The register depreciates an asset's cost, and most assets keep that basis for
-- life. The exception is a basis that moves later: a grant or rebate that
-- reimburses what the asset cost, a casualty loss. The ledger can carry that as
-- a contra account beside the asset, but the register would go on depreciating
-- the full cost, and Form 4562 would have nothing to say about why the figure is
-- what it is.
--
-- `amount_cents` is signed; negative reduces the basis. From `effective_year` on,
-- depreciation runs on the adjusted basis less what has already been allowed,
-- over what is left of the recovery period. An adjustment in the year the asset
-- was placed in service is simply part of its opening basis.
--
-- # Why a note is required
--
-- An adjusted basis nobody can explain is a deduction nobody can defend, and the
-- note is what the Form 4562 statement prints beside it.
CREATE TABLE IF NOT EXISTS depreciation_basis_adjustments (
    adjustment_id    TEXT PRIMARY KEY,
    asset_id         TEXT NOT NULL,
    effective_year   INTEGER NOT NULL,
    amount_cents     INTEGER NOT NULL,
    note             TEXT NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id)
);
