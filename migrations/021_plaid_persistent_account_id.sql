-- The id that survives a re-link.
--
-- `plaid_account_id` does not. Plaid mints account ids per Item, and linking the
-- same bank again produces a new Item — so the same real account comes back
-- wearing a different id, and nothing that keys on the old one recognises it.
--
-- That is not hypothetical. A Chase login linked three times left three sets of
-- ids for one checking account and one card; re-reading the connection's accounts
-- then added the current ids alongside the stale ones, showing each account twice.
-- Worse, when a re-link is treated as a new account the mapping to a ledger
-- account goes with it: the connection appears healthy and silently imports
-- nothing.
--
-- `persistent_account_id` is Plaid's answer to exactly this and is stable across
-- Items for institutions that provide it. NULL is expected and normal: it is
-- absent for institutions that do not, and for every row written before this
-- column existed. Matching therefore falls back to the account's mask, name and
-- type, and only when that identifies exactly one candidate — see
-- `same_account_under_a_new_id` in `store/projections.rs`.
ALTER TABLE plaid_local_accounts ADD COLUMN persistent_account_id TEXT;

-- Deliberately not UNIQUE. Two rows on the same item legitimately share a NULL,
-- and a partial unique index over the non-NULL values would turn a bank that
-- reuses a persistent id across two accounts into a failed sync rather than a
-- visible duplicate. The matcher below refuses to guess instead.
CREATE INDEX IF NOT EXISTS idx_plaid_local_accounts_persistent
    ON plaid_local_accounts(item_id, persistent_account_id);
