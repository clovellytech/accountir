-- Bank import mappings (links extension bank recipes to TUI accounts)
CREATE TABLE IF NOT EXISTS bank_accounts (
    bank_id TEXT PRIMARY KEY,
    bank_name TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES accounts(id),
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Pending bank imports (files waiting to be processed)
CREATE TABLE IF NOT EXISTS pending_imports (
    id INTEGER PRIMARY KEY,
    file_path TEXT NOT NULL,
    file_name TEXT NOT NULL,
    bank_id TEXT,
    bank_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    account_id TEXT REFERENCES accounts(id),
    transaction_count INTEGER,
    imported_count INTEGER DEFAULT 0,
    error_message TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    processed_at TEXT
);
