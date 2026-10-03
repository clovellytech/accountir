-- The portfolio: what each investment account is worth, at market, by day.
--
-- # Why this is not the holdings snapshot of migration 050
--
-- That snapshot is part of the books. It is an event, every member of a group sees
-- the same one, the reconciliation reads it, and a sheltered account's value is set
-- from it — so it is taken only for an account somebody has configured, and only
-- when the books import from the broker.
--
-- This is the other question: what is it all worth right now, and how has that
-- moved? It is answered from a holdings read alone, for every investment account
-- whether configured or not, and nothing here is ever posted. Market value never
-- reaches the books (INVESTMENTS-SPEC §3); it is a view of them.
--
-- # Why it is machine-local
--
-- None of these rows is derived from the log, and none is truncated by a
-- projection rebuild (see `Projections::rebuild`). Putting a daily price history
-- into a tamper-evident ledger would make the ledger the record of something that
-- is not bookkeeping, and on hosted books every refresh would be a write to the
-- group's log for a figure nobody else asked for.
--
-- # Units
--
-- Money in cents, quantities in micro-shares (as everywhere in the investment
-- tables), and prices in millionths of a currency unit, because a price is per share
-- and a fund priced at $10.123456 is not a price in cents.

-- One account's holdings on one day. A second refresh on the same day replaces the
-- day's row: the later read is the better one, and the history wants one point a
-- day, not one per press of the button.
CREATE TABLE IF NOT EXISTS portfolio_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    item_id TEXT NOT NULL,
    plaid_account_id TEXT NOT NULL,
    -- The local date the refresh ran. The prices inside may be older — they are
    -- usually the previous close — and carry their own dates.
    as_of TEXT NOT NULL,
    fetched_at TEXT NOT NULL,
    account_name TEXT NOT NULL,
    account_subtype TEXT,
    mask TEXT,
    -- What the institution says the account is worth, for checking the holdings
    -- against. NULL when it did not say.
    balance_cents INTEGER,
    currency TEXT,
    UNIQUE (item_id, plaid_account_id, as_of)
);

CREATE TABLE IF NOT EXISTS portfolio_holdings (
    snapshot_id TEXT NOT NULL REFERENCES portfolio_snapshots(snapshot_id)
        ON DELETE CASCADE,
    plaid_security_id TEXT NOT NULL,
    quantity INTEGER NOT NULL,
    price_micros INTEGER,
    price_as_of TEXT,
    -- The broker's value, or quantity times price when it gave none. NULL only when
    -- there was nothing to compute one from.
    value_cents INTEGER,
    -- The broker's basis for the whole holding. Never the books' — that is the
    -- lots — and shown beside it, not instead of it.
    cost_basis_cents INTEGER,
    currency TEXT,
    PRIMARY KEY (snapshot_id, plaid_security_id)
);

-- What each security is, as the provider last described it. Keyed on Plaid's
-- `security_id` and not on the security master's id, because most of what an
-- account holds — everything in a 401(k), and anything never traded through these
-- books — has no master and should not be given one just to be displayed.
CREATE TABLE IF NOT EXISTS portfolio_securities (
    plaid_security_id TEXT PRIMARY KEY,
    name TEXT,
    ticker TEXT,
    security_type TEXT,
    is_cash_equivalent INTEGER NOT NULL DEFAULT 0,
    close_price_micros INTEGER,
    close_price_as_of TEXT,
    currency TEXT,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_portfolio_snapshots_account
    ON portfolio_snapshots(item_id, plaid_account_id, as_of);
