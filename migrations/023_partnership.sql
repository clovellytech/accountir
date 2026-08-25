-- The partnership header and its partners: what Form 1065 and its K-1s are about.
--
-- `business_profile` is a single row for the same reason `company` is: one
-- ledger file is one business's books, and a second row would leave every
-- reader picking between them.
CREATE TABLE IF NOT EXISTS business_profile (
    id TEXT PRIMARY KEY CHECK (id = 'default'),
    legal_name TEXT NOT NULL,
    street TEXT NOT NULL,
    suite TEXT,
    city TEXT NOT NULL,
    state TEXT NOT NULL,
    postal_code TEXT NOT NULL,
    country TEXT,
    ein TEXT NOT NULL,
    naics_code TEXT NOT NULL,
    formation_date TEXT NOT NULL,
    principal_activity TEXT,
    principal_product TEXT,
    updated_at_event INTEGER REFERENCES events(id)
);

CREATE TABLE IF NOT EXISTS partners (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    -- 'general' or 'limited' — K-1 item G.
    partner_type TEXT NOT NULL,
    -- 'domestic' or 'foreign' — K-1 item H1.
    residency TEXT NOT NULL,
    -- K-1 item I1; free text because the form's own answer is free text.
    entity_type TEXT NOT NULL,
    street TEXT NOT NULL,
    suite TEXT,
    city TEXT NOT NULL,
    state TEXT NOT NULL,
    postal_code TEXT NOT NULL,
    country TEXT,
    start_date TEXT NOT NULL,
    -- NULL while the partner is still in. Set on the day they leave, which is
    -- what makes that year's K-1 a final one.
    end_date TEXT,
    -- Parts per million of the whole; 100% is 1000000. Integers because three
    -- partners at a third each must sum to a number somebody can check.
    profit_ppm INTEGER NOT NULL,
    loss_ppm INTEGER NOT NULL,
    capital_ppm INTEGER NOT NULL,
    admitted_at_event INTEGER REFERENCES events(id),
    updated_at_event INTEGER REFERENCES events(id)
);

CREATE INDEX IF NOT EXISTS idx_partners_start ON partners(start_date);

-- Partner taxpayer identification numbers, held locally and never in the event
-- log.
--
-- The log is replicated in full to every member's laptop. An SSN written there
-- is that SSN on every other partner's machine, permanently, in an append-only
-- file that cannot be redacted — the same argument that keeps event-service API
-- keys out of the log (migration 020). A TIN is needed only on the machine where
-- a return is actually prepared, so it stays there.
--
-- The consequence is deliberate and worth stating: this table does not sync, and
-- a member who has not entered a TIN locally generates a K-1 with item E blank
-- rather than one with somebody else's number in it.
--
-- What this does NOT claim: that the number never leaves the machine. A ledger
-- backup is a whole-file copy, so these rows travel in it like any others. The
-- distinction being drawn is narrower and is the one that matters — a row in a
-- table can be corrected or deleted (see `clear_tin`), whereas an append-only
-- log replicated to every partner's laptop can be neither.
CREATE TABLE IF NOT EXISTS partner_tins (
    partner_id TEXT PRIMARY KEY REFERENCES partners(id),
    -- An SSN (NNN-NN-NNNN) or an EIN (NN-NNNNNNN); item E takes either.
    tin TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
