-- Files attached to the books, and what each one is about.
--
-- # Why the file itself is not here
--
-- `documents` holds what the log says about a file — its name, its size, and the
-- SHA-256 of its bytes — and never the bytes. Those live in a blob store keyed by
-- that digest (`src/documents`). The log is replicated in full to every member, hashed
-- and kept forever: a 2 MB scan in it is 2 MB in every replica and every export, and
-- a statement can carry a taxpayer identifier, which migration 023 already decided
-- must not enter it.
--
-- The digest is what makes the bytes safe to move later. Whoever holds a copy can
-- prove it is the one the log names, so bytes fetched from a server, a backup or
-- another laptop are checked rather than trusted — and a replica that holds the log
-- without the files knows exactly which files it is missing.
--
-- # Why the subject is two columns and not JSON
--
-- "What are this entry's attachments" is the question the program actually asks, and a
-- JSON blob cannot be indexed for it. The pair is nullable because a document need not
-- be about one thing: a year's brokerage statements belong to the books, not to a row.
--
-- A projection: truncated and replayed by `Projector::rebuild`.
--
-- This number was reserved for the personal-tax branch's own 049, which creates this
-- table along with `tax_statements` and `k1_links`. Those two belong to work that is
-- not merged, and schema nothing reads is schema nobody maintains, so only the
-- documents half is here. That branch's migration drops this table's creation and
-- keeps its own two under a later number.

CREATE TABLE IF NOT EXISTS documents (
    document_id       TEXT PRIMARY KEY,
    -- Lowercase hex SHA-256 of the file's bytes: the blob store's key.
    sha256            TEXT NOT NULL,
    size_bytes        INTEGER NOT NULL,
    media_type        TEXT NOT NULL,
    -- The name it was attached under. Display only; never a path.
    filename          TEXT NOT NULL,
    title             TEXT,
    tax_year          INTEGER,
    -- A form code, when the document is a statement — '1099-B', 'W-2'. Free text
    -- here; the personal-tax work holds it to a form that version knows.
    form              TEXT,
    -- What it is about: 'entry', 'account' or 'reconciliation', with the id of the
    -- thing. Both null together, for a document that is about the books at large.
    subject_kind      TEXT,
    subject_id        TEXT,
    attached_at       TEXT NOT NULL,
    attached_at_event INTEGER REFERENCES events(id),
    CHECK ((subject_kind IS NULL) = (subject_id IS NULL))
);
CREATE INDEX IF NOT EXISTS idx_documents_subject ON documents(subject_kind, subject_id);
CREATE INDEX IF NOT EXISTS idx_documents_tax_year ON documents(tax_year);
