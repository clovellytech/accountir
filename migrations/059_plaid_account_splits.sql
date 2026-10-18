-- One bank feed carrying two ledger accounts' transactions.
--
-- Bank of America reports a business card's employee card inside the parent
-- account: one Plaid account, whose payments, rewards and fees belong to the
-- parent card and whose purchases belong to the cardholder's. A row here says:
-- staged rows on a feed mapped to `local_account_id` go to `split_to_account_id`
-- instead, unless their name contains one of `keep_patterns` (a JSON array of
-- case-insensitive substrings), in which case they stay.
--
-- Keyed by the ledger account rather than the Plaid connection, because a
-- re-link mints new Plaid ids and the rule is about the books, not the link.
-- Machine-local configuration, like the staged rows it is applied to: no event
-- carries it and a projection rebuild leaves it alone.
CREATE TABLE IF NOT EXISTS plaid_account_splits (
    local_account_id    TEXT PRIMARY KEY,
    split_to_account_id TEXT NOT NULL,
    keep_patterns       TEXT NOT NULL DEFAULT '[]'
);
