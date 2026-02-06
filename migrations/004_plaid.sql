-- Plaid items connected through the proxy
CREATE TABLE IF NOT EXISTS plaid_items (
    id TEXT PRIMARY KEY,
    proxy_item_id TEXT NOT NULL,
    institution_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    last_synced_at TEXT,
    connected_at_event INTEGER REFERENCES events(id)
);

-- Plaid account to local account mappings
CREATE TABLE IF NOT EXISTS plaid_local_accounts (
    item_id TEXT NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
    plaid_account_id TEXT NOT NULL,
    name TEXT NOT NULL,
    account_type TEXT NOT NULL,
    mask TEXT,
    local_account_id TEXT REFERENCES accounts(id),
    PRIMARY KEY (item_id, plaid_account_id)
);

-- Track imported Plaid transactions for dedup
CREATE TABLE IF NOT EXISTS plaid_imported_transactions (
    plaid_transaction_id TEXT PRIMARY KEY,
    item_id TEXT NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
    entry_id TEXT NOT NULL REFERENCES journal_entries(id)
);

CREATE INDEX IF NOT EXISTS idx_plaid_imported_item ON plaid_imported_transactions(item_id);
