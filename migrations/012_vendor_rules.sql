-- Vendor → payable account rules. A counterparty name (bank merchant or event
-- supplier) is matched, case-insensitively, against `pattern` (a substring);
-- the longest matching pattern wins and routes the posting to `account_id`.
CREATE TABLE IF NOT EXISTS vendor_account_rules (
    id TEXT PRIMARY KEY,
    pattern TEXT NOT NULL,
    account_id TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_vendor_rules_pattern ON vendor_account_rules(pattern);
