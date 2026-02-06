-- Initial schema for Accountir
-- This migration is handled by init_schema in migrations.rs
-- Keeping this file for documentation purposes

-- The schema includes:
-- - events: Core event store (append-only)
-- - merkle_nodes: Merkle tree for tamper-evident audit
-- - accounts: Chart of accounts
-- - journal_entries: Posted journal entries
-- - journal_lines: Individual lines in entries
-- - currencies: Enabled currencies
-- - exchange_rates: Historical exchange rates
-- - reconciliations: Bank reconciliation sessions
-- - cleared_transactions: Transactions cleared in reconciliations
-- - users: System users
-- - company: Company settings
-- - fiscal_years: Fiscal year definitions
-- - fiscal_periods: Monthly/quarterly periods

SELECT 1; -- Placeholder, actual schema is in migrations.rs
