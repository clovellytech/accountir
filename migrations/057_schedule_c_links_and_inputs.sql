-- A person's books receiving the Schedule C of a business kept in accountir, and
-- the per-year figures a Schedule C needs that no ledger holds.
--
-- # Why the inputs are in the log now
--
-- Line 30 and Form 4562's lines 10 and 11 were typed on the Schedule C page and
-- held there. Once the owner's personal books pull the Schedule C to put it on
-- their Form 1040, a pull that could not see them would carry a different line 31
-- from the one the business files. So they are a fact about the year, recorded
-- like the Schedule C answers are. NULL is "not worked out", not zero.
CREATE TABLE IF NOT EXISTS schedule_c_inputs (
    tax_year                     INTEGER PRIMARY KEY,
    home_office_cents            INTEGER,
    other_business_income_cents  INTEGER,
    section_179_carryover_cents  INTEGER,
    updated_at_event             INTEGER REFERENCES events(id)
);

-- A sole proprietorship managed in accountir whose Schedule C belongs on these
-- books' Form 1040. The id is the business's ledger id: one business, one
-- Schedule C, so linking twice is one link. Shaped like `k1_links` (053).
CREATE TABLE IF NOT EXISTS schedule_c_links (
    link_id          TEXT PRIMARY KEY,
    ledger_id        TEXT NOT NULL,
    ledger_name      TEXT NOT NULL,
    proprietor_name  TEXT NOT NULL DEFAULT '',
    linked_at_event  INTEGER REFERENCES events(id)
);
