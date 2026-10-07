-- The tax statements recorded from documents attached to the books, and the
-- partnerships whose K-1s these books receive.
--
-- Written as 049 on the personal-tax branch, together with `documents`. Main took
-- 049 first for the documents half (`049_documents.sql`, which adds a subject),
-- so this file keeps only the other two tables under the next free number.
--
-- # Why the file itself is not here
--
-- `documents` holds what the log says about a file — its name, its size, and the
-- SHA-256 of its bytes — and never the bytes. Those live in a blob store keyed by
-- that digest (`src/documents`). The log is replicated in full and kept forever,
-- and a statement carries the recipient's SSN, which migration 023 already
-- decided must not enter it. The digest is what lets a copy of the bytes fetched
-- from anywhere be checked against the log rather than trusted.
--
-- # Why a statement is figures and not only a file
--
-- A return is computed from numbers, and a PDF is not a number. `tax_statements`
-- is what a W-2, a 1099 or a K-1 *says*, box by box, in cents, with the documents
-- it was read from beside it. A K-1 pulled from another set of books managed here
-- arrives the same way, carrying where it came from in `source`, so the personal
-- return never has to reach into another file to be computed — see
-- `commands::tax_statement_commands`.
--
-- All three are projections: truncated and replayed by `Projector::rebuild`.

CREATE TABLE IF NOT EXISTS tax_statements (
    statement_id      TEXT PRIMARY KEY,
    tax_year          INTEGER NOT NULL,
    form              TEXT NOT NULL,
    issuer            TEXT NOT NULL,
    -- JSON object, box code to cents.
    amounts           TEXT NOT NULL,
    -- JSON array of document ids.
    document_ids      TEXT NOT NULL DEFAULT '[]',
    -- JSON: {"kind":"entered"} or {"kind":"ledger", ...provenance}.
    source            TEXT NOT NULL,
    note              TEXT,
    recorded_at_event INTEGER REFERENCES events(id)
);
CREATE INDEX IF NOT EXISTS idx_tax_statements_year ON tax_statements(tax_year, form);

-- A partnership managed in accountir whose K-1 for one of its partners belongs in
-- these books. The id is `<ledger id>:<partner id>`, so linking twice is one link.
CREATE TABLE IF NOT EXISTS k1_links (
    link_id         TEXT PRIMARY KEY,
    ledger_id       TEXT NOT NULL,
    ledger_name     TEXT NOT NULL,
    partner_id      TEXT NOT NULL,
    partner_name    TEXT NOT NULL,
    linked_at_event INTEGER REFERENCES events(id)
);
