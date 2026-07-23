CREATE TABLE IF NOT EXISTS ingest_account_mappings (
    key TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
