-- Drop the foreign keys from the two local config tables that point at
-- projections.
--
-- # The failure this fixes
--
-- `Projector::rebuild` replays the log from nothing, and to do that it truncates
-- every projection — `DELETE FROM accounts`, `DELETE FROM partners`. The store
-- opens with `PRAGMA foreign_keys = ON`, so a single row in `tax_line_mappings`
-- or `partner_tins` made that DELETE fail with a constraint error.
--
-- The consequence was that mapping one account to a Form 1065 line, or filing
-- one partner's TIN, left the ledger unable to rebuild its projections at all.
-- Rebuild is a *recovery* path. It has to stay runnable no matter how the books
-- happen to be configured, and it must certainly not be disabled by an ordinary
-- act like filling in a form.
--
-- # Why no foreign key is the right shape, not a workaround
--
-- These tables are local configuration keyed by a projection's id, and the two
-- that came before them — `ingest_account_mappings` (008) and
-- `vendor_account_rules` (012) — are declared exactly this way, `account_id TEXT
-- NOT NULL` with no reference. That is the established convention here and this
-- migration restores it.
--
-- The reasoning is the same one that keeps these rows out of the event log:
-- they outlive the projections they point at. A projection is derived and
-- disposable, rebuilt from the log on demand; this configuration is neither, and
-- is the thing that survives when the derived state is thrown away. A row left
-- pointing at an id that is momentarily absent is harmless and self-correcting —
-- the replay puts the account back under the same id, and `clear_account_line`
-- and `clear_tin` exist for a row that genuinely should go. An unrunnable
-- rebuild is not harmless.
--
-- SQLite cannot drop a constraint in place, so each table is rebuilt: create the
-- replacement, copy every row, drop the original, rename. Existing rows are
-- preserved.

-- Form 1065 line mappings (migration 024).
CREATE TABLE tax_line_mappings_new (
    account_id TEXT PRIMARY KEY,
    line_key TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO tax_line_mappings_new (account_id, line_key, updated_at)
    SELECT account_id, line_key, updated_at FROM tax_line_mappings;
DROP TABLE tax_line_mappings;
ALTER TABLE tax_line_mappings_new RENAME TO tax_line_mappings;

CREATE INDEX IF NOT EXISTS idx_tax_line_mappings_line ON tax_line_mappings(line_key);

-- Partner TINs (migration 023).
CREATE TABLE partner_tins_new (
    partner_id TEXT PRIMARY KEY,
    tin TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO partner_tins_new (partner_id, tin, updated_at)
    SELECT partner_id, tin, updated_at FROM partner_tins;
DROP TABLE partner_tins;
ALTER TABLE partner_tins_new RENAME TO partner_tins;
