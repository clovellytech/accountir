-- At most one live journal entry per non-null reference (SPEC §6.2; the
-- invariant audit flagged the ingest ref-dedup TOCTOU under concurrency). The
-- `reference` column is the idempotency key ingest/Plaid/Square/Amazon stamp on
-- an entry so re-imports are detected as duplicates. Until now dedup was a
-- read-then-append check with no DB backstop, so two concurrent imports of the
-- same source event could both pass the check and double-post.
--
-- This is the DB-level backstop for the in-txn check in `post_entry`. A
-- *partial* unique index mirrors `check_idempotent` exactly: it only constrains
-- rows with a non-null reference that are not voided, so entries with no
-- reference are unconstrained and voiding an entry frees its reference for
-- re-use (as `check_idempotent`'s `is_void = 0` already allows).
--
-- NOTE: if an existing database already holds duplicate references among live
-- entries (possible, since this was never enforced), creating the index will
-- fail; those rows must be voided/deduped first.
CREATE UNIQUE INDEX IF NOT EXISTS idx_journal_entries_reference_unique
    ON journal_entries(reference) WHERE reference IS NOT NULL AND is_void = 0;
