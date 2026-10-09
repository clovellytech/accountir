-- Attachments that know what they are, and the state K-1s read out of them.
--
-- `state_tax_statements`: one row per state K-1 a partner received — which
-- state, how much of the partnership's income is that state's, and what tax the
-- partnership already paid there on the partner's behalf. Its own table rather
-- than boxes on `tax_statements`, because one federal K-1 can come with ten of
-- these and each answers different questions. Fed by StateTaxStatementRecorded
-- and StateTaxStatementRemoved; amounts are a JSON object of
-- `tax::k1_extract::state_codes` to cents.
CREATE TABLE IF NOT EXISTS state_tax_statements (
    statement_id         TEXT PRIMARY KEY,
    tax_year             INTEGER NOT NULL,
    state                TEXT NOT NULL,
    issuer               TEXT NOT NULL,
    form                 TEXT NOT NULL,
    amounts              TEXT NOT NULL,
    apportionment_ppm    INTEGER,
    federal_statement_id TEXT,
    document_ids         TEXT NOT NULL DEFAULT '[]',
    note                 TEXT,
    recorded_at_event    INTEGER REFERENCES events(id)
);
CREATE INDEX IF NOT EXISTS idx_state_tax_statements_year
    ON state_tax_statements(tax_year, state);

-- What a document was recognised as (`documents::classify`), and what it
-- carries within that kind — for a K-1 package, its state K-1s, comma-separated.
--
-- Last in this file on purpose: on a database built by `init_schema` these
-- columns already exist, the first ALTER fails with "duplicate column", and the
-- runner treats the rest of the file as applied. Everything above it has run by
-- then.
ALTER TABLE documents ADD COLUMN kind TEXT;
ALTER TABLE documents ADD COLUMN kind_parts TEXT NOT NULL DEFAULT '';
