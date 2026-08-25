-- Which Form 1065 line each ledger account is reported on.
--
-- Keyed by account, not by line, and that direction is the whole point. The
-- ingest mappings (migration 008) go the other way — one account per key —
-- because there is exactly one Square balance account. A tax return is the
-- opposite shape: a chart of accounts has a dozen expense accounts that all land
-- on line 21, and one row per line could only ever name one of them.
--
-- Keying by account also makes the question that matters answerable in one
-- query: *which accounts have no line?* An account with a balance and no
-- mapping is income or expense that silently vanishes from the return, and the
-- only way to catch it is to be able to enumerate it.
CREATE TABLE IF NOT EXISTS tax_line_mappings (
    account_id TEXT PRIMARY KEY REFERENCES accounts(id),
    -- A key from `tax::lines::MAPPABLE_LINES`, e.g. 'l1a', 'l9', 'l21'.
    -- Not a foreign key: the canonical list is code, because it changes with
    -- the form and not with the books.
    line_key TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- The reporting direction: "every account on line 21".
CREATE INDEX IF NOT EXISTS idx_tax_line_mappings_line ON tax_line_mappings(line_key);
