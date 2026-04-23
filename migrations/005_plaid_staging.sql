-- Staged Plaid transactions awaiting review/import
CREATE TABLE IF NOT EXISTS plaid_staged_transactions (
    id TEXT PRIMARY KEY,
    item_id TEXT NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
    plaid_transaction_id TEXT NOT NULL UNIQUE,
    plaid_account_id TEXT NOT NULL,
    local_account_id TEXT,
    amount_cents INTEGER NOT NULL,
    date TEXT NOT NULL,
    name TEXT NOT NULL,
    merchant_name TEXT,
    currency TEXT NOT NULL DEFAULT 'USD',
    staged_at TEXT NOT NULL DEFAULT (datetime('now')),
    status TEXT NOT NULL DEFAULT 'pending'
);

CREATE INDEX IF NOT EXISTS idx_staged_status ON plaid_staged_transactions(status);
CREATE INDEX IF NOT EXISTS idx_staged_amount ON plaid_staged_transactions(amount_cents);
CREATE INDEX IF NOT EXISTS idx_staged_date ON plaid_staged_transactions(date);

-- Detected transfer candidate pairs
CREATE TABLE IF NOT EXISTS plaid_transfer_candidates (
    id TEXT PRIMARY KEY,
    staged_txn_id_1 TEXT NOT NULL REFERENCES plaid_staged_transactions(id) ON DELETE CASCADE,
    staged_txn_id_2 TEXT NOT NULL REFERENCES plaid_staged_transactions(id) ON DELETE CASCADE,
    confidence REAL NOT NULL DEFAULT 0.0,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_transfer_status ON plaid_transfer_candidates(status);
