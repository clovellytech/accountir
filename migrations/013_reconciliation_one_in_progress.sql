-- At most one in-progress reconciliation per account (SPEC §6.2; the invariant
-- audit found ReconciliationStarted was unenforced). This is the DB-level
-- backstop for the in-txn check in start_reconciliation. A *partial* unique
-- index only constrains rows with status='in_progress', so any number of
-- completed/abandoned reconciliations per account is still fine.
--
-- NOTE: if an existing database already holds duplicate in-progress
-- reconciliations for one account (possible, since this was never enforced),
-- creating the index will fail; those rows must be abandoned/deduped first.
CREATE UNIQUE INDEX IF NOT EXISTS idx_reconciliations_one_in_progress
    ON reconciliations(account_id) WHERE status = 'in_progress';
