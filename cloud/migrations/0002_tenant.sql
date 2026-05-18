-- Per-tenant schema. Every table carries company_id and is protected by RLS.
-- The application sets app.company_id via SET LOCAL at the start of each
-- transaction; RLS policies key off it.

-- ---------- helper ---------------------------------------------------------

CREATE OR REPLACE FUNCTION current_company_id() RETURNS UUID
LANGUAGE plpgsql STABLE AS $$
BEGIN
    RETURN current_setting('app.company_id', true)::uuid;
EXCEPTION WHEN OTHERS THEN
    RETURN NULL;
END;
$$;

-- ---------- event store ----------------------------------------------------

CREATE TABLE events (
    id BIGSERIAL PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE RESTRICT,
    company_seq_id BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    hash BYTEA NOT NULL,
    user_id UUID REFERENCES auth_users(id),
    actor_label TEXT,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (company_id, company_seq_id),
    UNIQUE (company_id, hash)
);

CREATE INDEX idx_events_company_type ON events(company_id, event_type);
CREATE INDEX idx_events_company_timestamp ON events(company_id, timestamp);

CREATE TABLE merkle_nodes (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    level INTEGER NOT NULL,
    position BIGINT NOT NULL,
    hash BYTEA NOT NULL,
    left_child_pos BIGINT,
    right_child_pos BIGINT,
    PRIMARY KEY (company_id, level, position)
);

-- ---------- chart of accounts ---------------------------------------------

CREATE TYPE account_type AS ENUM ('asset', 'liability', 'equity', 'revenue', 'expense');

CREATE TABLE accounts (
    id UUID PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    account_type account_type NOT NULL,
    account_number TEXT NOT NULL,
    name TEXT NOT NULL,
    parent_id UUID REFERENCES accounts(id),
    currency TEXT,
    description TEXT,
    is_active BOOLEAN NOT NULL DEFAULT true,
    created_at_event BIGINT REFERENCES events(id),
    updated_at_event BIGINT REFERENCES events(id),
    UNIQUE (company_id, account_number)
);

CREATE INDEX idx_accounts_company_type ON accounts(company_id, account_type);

-- ---------- journal entries -----------------------------------------------

CREATE TYPE journal_entry_source AS ENUM ('manual', 'import', 'recurring', 'system', 'plaid');

CREATE TABLE journal_entries (
    id UUID PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    date DATE NOT NULL,
    memo TEXT,
    reference TEXT,
    source journal_entry_source,
    is_void BOOLEAN NOT NULL DEFAULT false,
    voided_by_entry_id UUID,
    posted_at_event BIGINT REFERENCES events(id)
);

CREATE INDEX idx_journal_entries_company_date ON journal_entries(company_id, date);

CREATE TABLE journal_lines (
    id UUID PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    entry_id UUID NOT NULL REFERENCES journal_entries(id) ON DELETE CASCADE,
    account_id UUID NOT NULL REFERENCES accounts(id),
    -- Smallest currency unit. Positive = debit, negative = credit. Sum per entry must be 0.
    amount BIGINT NOT NULL,
    currency TEXT NOT NULL,
    exchange_rate NUMERIC,
    memo TEXT,
    is_cleared BOOLEAN NOT NULL DEFAULT false,
    cleared_at_event BIGINT
);

CREATE INDEX idx_journal_lines_company_account ON journal_lines(company_id, account_id);
CREATE INDEX idx_journal_lines_entry ON journal_lines(entry_id);

-- ---------- multi-currency ------------------------------------------------

CREATE TABLE currencies (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    code TEXT NOT NULL,
    name TEXT NOT NULL,
    symbol TEXT,
    decimal_places SMALLINT NOT NULL DEFAULT 2,
    PRIMARY KEY (company_id, code)
);

CREATE TABLE exchange_rates (
    id BIGSERIAL PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    from_currency TEXT NOT NULL,
    to_currency TEXT NOT NULL,
    rate NUMERIC NOT NULL,
    effective_date DATE NOT NULL,
    recorded_at_event BIGINT REFERENCES events(id)
);

CREATE INDEX idx_exchange_rates_company_pair ON exchange_rates(company_id, from_currency, to_currency, effective_date);

-- ---------- reconciliation -------------------------------------------------

CREATE TYPE reconciliation_status AS ENUM ('in_progress', 'completed', 'abandoned');

CREATE TABLE reconciliations (
    id UUID PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    account_id UUID NOT NULL REFERENCES accounts(id),
    statement_date DATE NOT NULL,
    statement_ending_balance BIGINT NOT NULL,
    status reconciliation_status NOT NULL DEFAULT 'in_progress',
    started_at_event BIGINT REFERENCES events(id),
    completed_at_event BIGINT
);

CREATE INDEX idx_reconciliations_company_account ON reconciliations(company_id, account_id);

CREATE TABLE cleared_transactions (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    reconciliation_id UUID NOT NULL REFERENCES reconciliations(id) ON DELETE CASCADE,
    entry_id UUID NOT NULL,
    line_id UUID NOT NULL,
    cleared_amount BIGINT NOT NULL,
    cleared_at_event BIGINT REFERENCES events(id),
    PRIMARY KEY (reconciliation_id, entry_id, line_id)
);

-- ---------- fiscal periods -------------------------------------------------

CREATE TABLE fiscal_years (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    year INTEGER NOT NULL,
    start_date DATE NOT NULL,
    end_date DATE NOT NULL,
    is_closed BOOLEAN NOT NULL DEFAULT false,
    retained_earnings_entry_id UUID,
    PRIMARY KEY (company_id, year)
);

CREATE TYPE period_status AS ENUM ('open', 'closed');

CREATE TABLE fiscal_periods (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    year INTEGER NOT NULL,
    period SMALLINT NOT NULL,
    start_date DATE NOT NULL,
    end_date DATE NOT NULL,
    status period_status NOT NULL DEFAULT 'open',
    closed_by_user_id UUID REFERENCES auth_users(id),
    closed_at TIMESTAMPTZ,
    PRIMARY KEY (company_id, year, period)
);

-- ---------- Plaid (we host it) --------------------------------------------

CREATE TYPE plaid_item_status AS ENUM ('active', 'error', 'disconnected');

CREATE TABLE plaid_items (
    id UUID PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    plaid_item_id TEXT NOT NULL,
    access_token_ciphertext BYTEA NOT NULL,
    access_token_nonce BYTEA NOT NULL,
    institution_name TEXT NOT NULL,
    institution_id TEXT,
    status plaid_item_status NOT NULL DEFAULT 'active',
    last_synced_at TIMESTAMPTZ,
    sync_cursor TEXT,
    connected_at_event BIGINT REFERENCES events(id),
    UNIQUE (company_id, plaid_item_id)
);

CREATE TABLE plaid_local_accounts (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    item_id UUID NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
    plaid_account_id TEXT NOT NULL,
    name TEXT NOT NULL,
    account_type TEXT NOT NULL,
    mask TEXT,
    local_account_id UUID REFERENCES accounts(id),
    plaid_balance_cents BIGINT,
    balance_updated_at TIMESTAMPTZ,
    PRIMARY KEY (item_id, plaid_account_id)
);

CREATE TABLE plaid_imported_transactions (
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    plaid_transaction_id TEXT NOT NULL,
    item_id UUID NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
    entry_id UUID NOT NULL REFERENCES journal_entries(id),
    PRIMARY KEY (company_id, plaid_transaction_id)
);

CREATE INDEX idx_plaid_imported_item ON plaid_imported_transactions(item_id);

CREATE TYPE staged_status AS ENUM ('pending', 'imported', 'rejected', 'transfer');

CREATE TABLE plaid_staged_transactions (
    id UUID PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    item_id UUID NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
    plaid_transaction_id TEXT NOT NULL,
    plaid_account_id TEXT NOT NULL,
    local_account_id UUID,
    amount_cents BIGINT NOT NULL,
    date DATE NOT NULL,
    name TEXT NOT NULL,
    merchant_name TEXT,
    currency TEXT NOT NULL DEFAULT 'USD',
    staged_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    status staged_status NOT NULL DEFAULT 'pending',
    payment_meta JSONB,
    UNIQUE (company_id, plaid_transaction_id)
);

CREATE INDEX idx_staged_company_status ON plaid_staged_transactions(company_id, status);
CREATE INDEX idx_staged_company_date ON plaid_staged_transactions(company_id, date);

CREATE TYPE transfer_candidate_status AS ENUM ('pending', 'accepted', 'rejected');

CREATE TABLE plaid_transfer_candidates (
    id UUID PRIMARY KEY,
    company_id UUID NOT NULL REFERENCES companies(id) ON DELETE CASCADE,
    staged_txn_id_1 UUID NOT NULL REFERENCES plaid_staged_transactions(id) ON DELETE CASCADE,
    staged_txn_id_2 UUID NOT NULL REFERENCES plaid_staged_transactions(id) ON DELETE CASCADE,
    confidence NUMERIC NOT NULL DEFAULT 0,
    status transfer_candidate_status NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_transfer_company_status ON plaid_transfer_candidates(company_id, status);

-- ---------- background jobs (single-DB, no external queue) ----------------

CREATE TYPE job_status AS ENUM ('pending', 'running', 'completed', 'failed');

CREATE TABLE jobs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    company_id UUID REFERENCES companies(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status job_status NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    run_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_at TIMESTAMPTZ,
    locked_by TEXT,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ
);

CREATE INDEX idx_jobs_runnable ON jobs(status, run_at) WHERE status IN ('pending', 'failed');
CREATE INDEX idx_jobs_company ON jobs(company_id);

-- ---------- RLS ------------------------------------------------------------

ALTER TABLE events                       ENABLE ROW LEVEL SECURITY;
ALTER TABLE merkle_nodes                 ENABLE ROW LEVEL SECURITY;
ALTER TABLE accounts                     ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_entries              ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_lines                ENABLE ROW LEVEL SECURITY;
ALTER TABLE currencies                   ENABLE ROW LEVEL SECURITY;
ALTER TABLE exchange_rates               ENABLE ROW LEVEL SECURITY;
ALTER TABLE reconciliations              ENABLE ROW LEVEL SECURITY;
ALTER TABLE cleared_transactions         ENABLE ROW LEVEL SECURITY;
ALTER TABLE fiscal_years                 ENABLE ROW LEVEL SECURITY;
ALTER TABLE fiscal_periods               ENABLE ROW LEVEL SECURITY;
ALTER TABLE plaid_items                  ENABLE ROW LEVEL SECURITY;
ALTER TABLE plaid_local_accounts         ENABLE ROW LEVEL SECURITY;
ALTER TABLE plaid_imported_transactions  ENABLE ROW LEVEL SECURITY;
ALTER TABLE plaid_staged_transactions    ENABLE ROW LEVEL SECURITY;
ALTER TABLE plaid_transfer_candidates    ENABLE ROW LEVEL SECURITY;

-- Force RLS even for table owners. App must connect as a non-superuser role.
ALTER TABLE events                       FORCE ROW LEVEL SECURITY;
ALTER TABLE merkle_nodes                 FORCE ROW LEVEL SECURITY;
ALTER TABLE accounts                     FORCE ROW LEVEL SECURITY;
ALTER TABLE journal_entries              FORCE ROW LEVEL SECURITY;
ALTER TABLE journal_lines                FORCE ROW LEVEL SECURITY;
ALTER TABLE currencies                   FORCE ROW LEVEL SECURITY;
ALTER TABLE exchange_rates               FORCE ROW LEVEL SECURITY;
ALTER TABLE reconciliations              FORCE ROW LEVEL SECURITY;
ALTER TABLE cleared_transactions         FORCE ROW LEVEL SECURITY;
ALTER TABLE fiscal_years                 FORCE ROW LEVEL SECURITY;
ALTER TABLE fiscal_periods               FORCE ROW LEVEL SECURITY;
ALTER TABLE plaid_items                  FORCE ROW LEVEL SECURITY;
ALTER TABLE plaid_local_accounts         FORCE ROW LEVEL SECURITY;
ALTER TABLE plaid_imported_transactions  FORCE ROW LEVEL SECURITY;
ALTER TABLE plaid_staged_transactions    FORCE ROW LEVEL SECURITY;
ALTER TABLE plaid_transfer_candidates    FORCE ROW LEVEL SECURITY;

DO $$
DECLARE
    t TEXT;
BEGIN
    FOR t IN
        SELECT unnest(ARRAY[
            'events', 'merkle_nodes', 'accounts', 'journal_entries', 'journal_lines',
            'currencies', 'exchange_rates', 'reconciliations', 'cleared_transactions',
            'fiscal_years', 'fiscal_periods',
            'plaid_items', 'plaid_local_accounts', 'plaid_imported_transactions',
            'plaid_staged_transactions', 'plaid_transfer_candidates'
        ])
    LOOP
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I USING (company_id = current_company_id()) WITH CHECK (company_id = current_company_id())',
            t
        );
    END LOOP;
END $$;
