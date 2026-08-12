-- Recurring transfer rules: on a chosen day each month, move an account's
-- running balance (or a fixed amount) to another account. Motivating case: a
-- business credit card whose employee sub-cards must be paid via the parent
-- account — each month the employee card's balance is shifted to the parent.
--
-- The transfers this generates are ordinary journal entries, posted through the
-- normal command path (source 'recurring', with a deterministic reference
-- `recurring:<rule_id>:<YYYY-MM>` so re-running never double-posts a period).
-- Only the rule *config* lives here; like vendor_account_rules this table is
-- plain config, not event-sourced.
CREATE TABLE IF NOT EXISTS recurring_transfer_rules (
    id TEXT PRIMARY KEY,
    source_account_id TEXT NOT NULL,
    dest_account_id TEXT NOT NULL,
    -- Day of month to run on; clamped to the last day of shorter months.
    day_of_month INTEGER NOT NULL,
    -- 'full_balance' zeroes the source into the destination; 'fixed' moves a set amount.
    amount_mode TEXT NOT NULL DEFAULT 'full_balance',
    fixed_amount_cents INTEGER,
    memo TEXT NOT NULL DEFAULT '',
    -- First period to consider, inclusive, as 'YYYY-MM'.
    start_month TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
