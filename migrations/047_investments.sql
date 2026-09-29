-- The taxable-brokerage register: what securities these books know about, the
-- lots they were bought in, and which lots each sale consumed.
--
-- INVESTMENTS-SPEC.md phase 1. Event-sourced like the asset register (migration
-- 030), and for the same reason: what the business owns and what it realized on
-- selling it is a fact the whole business files on, not a secret belonging to one
-- machine. So these are projections of SecurityDefined / SecurityBought /
-- SecuritySold, and every member sees the same register.
--
-- No foreign key to `accounts`, deliberately, for the reason migration 025 gives:
-- `Projector::rebuild` truncates projections and replays, and a foreign key would
-- fight the order rows come back in. `*_at_event` referencing `events` is safe —
-- the log is not a projection.
--
-- # Why the ledger is not enough on its own
--
-- Securities are carried AT COST in one account per brokerage (spec §3 and the
-- open question in §10), so the ledger knows the total cost of everything held
-- and nothing else. A realized gain needs to know *which shares* were sold: their
-- cost and the date they were bought. A journal entry records neither. This is
-- where those facts live, exactly as `depreciable_assets` holds the inputs MACRS
-- needs and the ledger holds only the result.
--
-- # Units
--
-- Money is `_cents` INTEGER everywhere, like every other money column in this
-- schema. Quantity is INTEGER in **millionths of a share** ("micro-shares",
-- 1e-6). Fractional shares are ordinary now — dividend reinvestment and
-- dollar-based buying both produce them — and six places is past every
-- brokerage's own precision, so no quantity has to be rounded on the way in.
-- Floating point was not an option: a holding that has to reconcile to a
-- broker's statement cannot be stored in a type that cannot represent 0.1.

-- The security master.
--
-- A record of its own so that a ticker change does not fork history. Tickers are
-- reassigned and renamed; a lot that identified its security by ticker would
-- become a lot of a different company, and the gain on selling it would be
-- computed against the wrong basis.
CREATE TABLE IF NOT EXISTS securities (
    id TEXT PRIMARY KEY,
    -- What the broker calls it today. UNIQUE because phase 1 has no rename
    -- event: within one book at one time a ticker means one security, and two
    -- masters for AAPL would split one holding into two that neither reconcile
    -- against the 1099-B nor add up on the balance sheet.
    ticker TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    -- 'stock', 'etf', 'mutual fund', 'bond'… free text, not an enum, because a
    -- broker's own vocabulary is what will fill it (spec §6 imports
    -- `security.type` from Plaid) and a closed set in a permanent log means a
    -- type nobody anticipated cannot be recorded at all. Nothing in phase 1
    -- branches on it: it is a label for reports and for reconciliation.
    kind TEXT NOT NULL,
    -- The identifier that survives a ticker change. NULL for anything the broker
    -- does not give one for.
    cusip TEXT,
    -- Here from the start although multi-currency is out of scope (spec §10):
    -- adding it later would be a migration of every lot, and of every gain
    -- already computed from one.
    currency TEXT NOT NULL DEFAULT 'USD',
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id)
);

-- A lot per purchase, with what is left of it.
--
-- # Why a total cost and not a unit price
--
-- `total_cost_cents` is the whole cost of the purchase — including commission,
-- which capitalises into basis under the ordinary tax treatment of a purchase —
-- and there is deliberately no unit-price column. A price times a quantity has to
-- be rounded, and it would be rounded again on every sale out of the lot, so the
-- basis removed from the Securities account would drift from the basis the
-- account was debited. Storing the total makes the basis exact by construction.
-- Unit price is a division, and belongs to whatever displays it.
--
-- # Why `remaining_basis_cents` is stored rather than derived
--
-- Because a lot can be sold in pieces, and each piece's share of the cost has to
-- be a whole number of cents. Recomputing "cost times shares left over shares
-- bought" after each sale re-rounds the same money repeatedly and loses or
-- invents cents. Carrying what is left instead means the pieces sum to the total
-- exactly: the allocation takes `remaining_basis * sold / remaining_quantity`,
-- and the sale that closes the lot takes the whole remainder.
CREATE TABLE IF NOT EXISTS investment_lots (
    id TEXT PRIMARY KEY,
    security_id TEXT NOT NULL,
    -- Which Securities account holds it. Load-bearing: a sale may only consume
    -- lots sitting in the account it is selling out of, or one brokerage's basis
    -- would be relieved by another brokerage's sale.
    securities_account_id TEXT NOT NULL,
    -- Where the money came from — provenance, not an invariant.
    cash_account_id TEXT NOT NULL,
    -- Micro-shares. See the units note above.
    quantity INTEGER NOT NULL,
    total_cost_cents INTEGER NOT NULL,
    remaining_quantity INTEGER NOT NULL,
    remaining_basis_cents INTEGER NOT NULL,
    -- ISO-8601. The date the holding period runs from.
    trade_date TEXT NOT NULL,
    added_at_event INTEGER REFERENCES events(id),
    updated_at_event INTEGER REFERENCES events(id)
);

-- Oldest first within a security and account: the order FIFO consumes them in.
CREATE INDEX IF NOT EXISTS idx_investment_lots_holding
    ON investment_lots(security_id, securities_account_id, trade_date);

-- One row per sale: the totals a 1099-B reports, and the gain as it was computed
-- on the day.
CREATE TABLE IF NOT EXISTS investment_sales (
    id TEXT PRIMARY KEY,
    security_id TEXT NOT NULL,
    securities_account_id TEXT NOT NULL,
    cash_account_id TEXT NOT NULL,
    quantity INTEGER NOT NULL,
    -- Gross, as the broker reports it on the 1099-B…
    proceeds_cents INTEGER NOT NULL,
    -- …and the fee taken out of it. A sale fee reduces proceeds rather than
    -- posting as an expense, because that is how the 1099-B reports proceeds and
    -- reconciling against that form is the point (spec §7). A standalone account
    -- fee not tied to a trade is a different thing and posts to the fee expense
    -- account instead.
    fee_cents INTEGER NOT NULL,
    -- The basis of the lots consumed, which is also exactly what the sale
    -- credited to the Securities account. Stored although it is the sum of
    -- `investment_sale_lots.basis_cents`, because it is the figure the entry
    -- posted: a report that recomputed it could disagree with the books, and this
    -- table exists so it cannot.
    basis_cents INTEGER NOT NULL,
    -- (proceeds - fee) - basis. Negative is a loss.
    realized_gain_cents INTEGER NOT NULL,
    trade_date TEXT NOT NULL,
    recorded_at_event INTEGER REFERENCES events(id)
);

CREATE INDEX IF NOT EXISTS idx_investment_sales_security
    ON investment_sales(security_id, trade_date);

-- Which lots a sale consumed, and on what terms — the Form 8949 rows.
--
-- Recorded rather than recomputed at report time, because the lots consumed are
-- carried on the `SecuritySold` event itself: a filed gain must not be silently
-- restated by a later change of the default selection method (spec §4). This
-- table is that event's detail made queryable.
CREATE TABLE IF NOT EXISTS investment_sale_lots (
    sale_id TEXT NOT NULL,
    lot_id TEXT NOT NULL,
    quantity INTEGER NOT NULL,
    basis_cents INTEGER NOT NULL,
    -- 'short' or 'long', computed per lot, so one sale can produce both — which
    -- is why this is a table and not two columns on `investment_sales`.
    term TEXT NOT NULL,
    -- The primary key doubles as the fence that one sale cannot name one lot
    -- twice; the command refuses it first, with a message.
    PRIMARY KEY (sale_id, lot_id)
);
