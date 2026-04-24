-- Store Plaid balance snapshots on each sync for reconciliation
ALTER TABLE plaid_local_accounts ADD COLUMN plaid_balance_cents INTEGER;
ALTER TABLE plaid_local_accounts ADD COLUMN balance_updated_at TEXT;
