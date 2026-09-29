-- The investments importer's registers: how each brokerage account is configured,
-- which of our securities a provider's security is, what has already been
-- imported, and what the last holdings snapshot said.
--
-- INVESTMENTS-SPEC.md phase 4. Four of these tables are projections of events
-- (`InvestmentAccountConfigured`, `PlaidSecurityLinked`,
-- `InvestmentActivityImported`, `HoldingsSnapshotRecorded`) and two are
-- machine-local. The split is not arbitrary, and it is the same one migrations
-- 004/005 make between `plaid_imported_transactions` and
-- `plaid_staged_transactions`:
--
-- * **Replicated** is anything a second machine must agree with. Which ledger
--   account a brokerage's dividends post to, which of our securities Plaid's
--   `sec_abc` is, and which trades have already been posted are all facts the
--   whole book depends on. A dedup register that lived on one laptop would let a
--   colleague's import post every trade a second time, each with its own lot and
--   its own basis — a Form 8949 that deducts one purchase twice.
-- * **Machine-local** is anything that is only about what *this* machine has done
--   or is still looking at. How far a fetch has got, and a row somebody has not
--   reviewed yet, are both of those. A replica that has never fetched should fall
--   back to a full-history pull and let the dedup register throw away what it
--   already has; that is exactly what a missing local row produces.
--
-- No foreign key from a projection to `accounts` or to `securities`, deliberately,
-- for the reason migration 025 gives: `Projector::rebuild` truncates projections
-- and replays, and a foreign key would fight the order rows come back in.
--
-- # Units
--
-- Money is `_cents` INTEGER and quantity is INTEGER micro-shares (millionths of a
-- share), as migration 047 has it. Plaid sends both as IEEE floats; they are
-- converted once, at the boundary, with explicit rounding, and no float is stored
-- anywhere in here. See `investment_import::to_cents` and
-- `investment_import::to_micro_shares`.

-- What a Plaid investment account is, and which ledger accounts its activity
-- posts to.
--
-- # Why configuration is required rather than inferred
--
-- Because every alternative is worse. Guessing the accounts from their names
-- posts a dividend to whatever account happened to sound right, and the mistake
-- is invisible until a return is being prepared from it. Dropping the activity of
-- an account nobody has configured loses transactions the provider will not hand
-- over again. So an unconfigured account's activity is **held** — see
-- `investment_staged_activity` — which is the stance
-- `plaid_commands::stage_transactions_in_conn` already takes, and for the same
-- reason.
--
-- # Why the taxable columns are nullable and so is the sheltered one
--
-- One row covers both kinds of account because one Plaid account is one row
-- whichever kind it is, and a second table would mean two rows could disagree
-- about which kind it is. `treatment` says which set of columns applies, and the
-- CHECK constraint at the bottom is what stops a row being half-configured: a
-- taxable row with no dividend account would import a dividend to nowhere.
CREATE TABLE IF NOT EXISTS investment_account_config (
    item_id TEXT NOT NULL,
    plaid_account_id TEXT NOT NULL,
    -- 'taxable' or 'sheltered'. A CLOSED set: spec §2 knows two models, full lot
    -- accounting and one account carried at value, and there is no third.
    treatment TEXT NOT NULL,
    -- What Plaid's `account.subtype` said when this was configured
    -- ('brokerage', 'ira', '401k'…), and whether that subtype was one we
    -- recognise. Recorded rather than re-derived so that a report can say "you
    -- confirmed this while Plaid was calling it `unknown thing`" years later;
    -- spec §2 makes an unrecognised subtype taxable *and flagged*, and a flag
    -- nobody can trace back to what was flagged is not a flag.
    plaid_subtype TEXT,
    subtype_recognised INTEGER NOT NULL DEFAULT 1,
    -- Taxable (spec §2a): securities at cost, the sweep cash, the two income
    -- accounts, the realized-gain account and the fee expense account.
    securities_account_id TEXT,
    cash_account_id TEXT,
    dividend_income_account_id TEXT,
    interest_income_account_id TEXT,
    realized_gain_account_id TEXT,
    fee_expense_account_id TEXT,
    -- Optional, and only for a taxable account: where the other side of a cash
    -- deposit or withdrawal goes. A brokerage tells us money arrived; it does not
    -- tell us which bank account it came from, and the bank feed will report that
    -- side separately. A clearing account is the honest place for one leg of a
    -- transfer whose other leg has not arrived yet. NULL means "don't post cash
    -- movements at all" — they are held for review instead, which is the right
    -- answer when there is nowhere truthful to put them.
    transfer_clearing_account_id TEXT,
    -- Sheltered (spec §2b): the one ledger account carried at value, which must
    -- be on the `retirement_accounts` register of migration 048. Nothing inside
    -- the account is imported at all.
    retirement_account_id TEXT,
    configured_at_event INTEGER REFERENCES events(id),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (item_id, plaid_account_id),
    CHECK (treatment IN ('taxable', 'sheltered')),
    CHECK (
        (treatment = 'taxable'
         AND securities_account_id IS NOT NULL
         AND cash_account_id IS NOT NULL
         AND dividend_income_account_id IS NOT NULL
         AND interest_income_account_id IS NOT NULL
         AND realized_gain_account_id IS NOT NULL
         AND fee_expense_account_id IS NOT NULL
         AND retirement_account_id IS NULL)
        OR
        (treatment = 'sheltered'
         AND retirement_account_id IS NOT NULL
         AND securities_account_id IS NULL
         AND cash_account_id IS NULL)
    )
);

-- Which of our securities a provider's security is.
--
-- The whole point of a mapping table rather than matching on ticker every time: a
-- ticker change must not fork a holding. Plaid's `security_id` is stable across a
-- rename, ours is stable for ever, and this is the join between them. Without it,
-- a company that changes symbol arrives as a new security, its lots go to a new
-- master, and the gain on selling the old shares is computed against nothing.
CREATE TABLE IF NOT EXISTS plaid_securities (
    plaid_security_id TEXT PRIMARY KEY,
    -- `securities.id`. No foreign key — see the note at the top.
    security_id TEXT NOT NULL,
    linked_at_event INTEGER REFERENCES events(id)
);

CREATE INDEX IF NOT EXISTS idx_plaid_securities_security
    ON plaid_securities(security_id);

-- One row per provider transaction we have acted on: the dedup fence.
--
-- Keyed on Plaid's `investment_transaction_id`, which is the only identifier that
-- survives a re-fetch. The rolling 30-day window (spec §6) means every import
-- sees a month of transactions it has already posted, so this table is load-
-- bearing on every run and not only after a mistake.
--
-- # Why this is not the journal entry's reference
--
-- Migration 014's unique index on `journal_entries.reference` is already the
-- idempotency fence for a bank import, and it would look like the obvious place.
-- It is taken: a purchase's reference is `securities-buy-<lot_id>`, which is the
-- link between the lot register and the entry that posted it (see
-- `investment_commands::buy_reference`), and a lot id is minted fresh on every
-- attempt. Overloading one reference with two jobs would mean a re-import with a
-- new lot id sails past the fence, which is the exact bug this table prevents.
CREATE TABLE IF NOT EXISTS investment_imports (
    provider_transaction_id TEXT PRIMARY KEY,
    item_id TEXT NOT NULL,
    plaid_account_id TEXT NOT NULL,
    -- What it became: 'buy', 'sell', 'dividend', 'interest', 'fee', 'cash'. Not a
    -- copy of Plaid's own type and subtype — those are on the event — but the
    -- decision this importer made, so that a later disagreement about a trade can
    -- be traced to the rule that was applied to it.
    outcome TEXT NOT NULL,
    -- The journal entry it posted. Always present: every outcome in this table
    -- posted something, and the things that posted nothing are either ignored by
    -- design (a sheltered account's trades) or held for review.
    entry_id TEXT NOT NULL,
    -- The lot a buy created, or the sale a sell recorded; NULL for income, a fee
    -- and a cash movement, which create neither.
    lot_id TEXT,
    sale_id TEXT,
    imported_at TEXT NOT NULL DEFAULT (datetime('now')),
    imported_at_event INTEGER REFERENCES events(id)
);

CREATE INDEX IF NOT EXISTS idx_investment_imports_account
    ON investment_imports(item_id, plaid_account_id);

-- What the broker said was in an account on a date. Reports and reconciliation
-- only; nothing here is ever posted for a taxable account (spec §3 and §5).
--
-- Kept as history rather than as one current row, because the unrealized gain and
-- the reconciliation are both comparisons *against a date*, and a snapshot
-- overwritten in place cannot answer what the account held at the end of a tax
-- year that has since closed.
CREATE TABLE IF NOT EXISTS investment_holdings_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    item_id TEXT NOT NULL,
    plaid_account_id TEXT NOT NULL,
    as_of TEXT NOT NULL,
    recorded_at_event INTEGER REFERENCES events(id),
    -- One snapshot per account per day. A second fetch on the same day replaces
    -- it, because the later read of the same day is the better one; a re-fetch
    -- that finds nothing changed appends no event at all, so this conflict only
    -- fires when the day's holdings really did move.
    UNIQUE (item_id, plaid_account_id, as_of)
);

CREATE TABLE IF NOT EXISTS investment_holdings_snapshot_lines (
    snapshot_id TEXT NOT NULL REFERENCES investment_holdings_snapshots(snapshot_id)
        ON DELETE CASCADE,
    -- Always present: it is the provider's identity for the holding, and the only
    -- thing that is certain to be there.
    plaid_security_id TEXT NOT NULL,
    -- Ours, when we have one. NULL is ordinary rather than an error: a sheltered
    -- account's holdings are never put on the security master (nothing inside one
    -- is recorded, spec §2b), and a taxable holding of something never traded
    -- through this book has no master either.
    security_id TEXT,
    ticker TEXT,
    -- Micro-shares.
    quantity INTEGER NOT NULL,
    -- The broker's own basis for the whole holding. NULL when it does not know —
    -- a lot transferred in from another custodian often has none. **Never used to
    -- post anything**: our basis comes from the buys we imported, and this is the
    -- cross-check (spec §7), which is a different job from being the source.
    cost_basis_cents INTEGER,
    -- Market value, for the unrealized-gain report. Also never posted.
    value_cents INTEGER,
    currency TEXT,
    PRIMARY KEY (snapshot_id, plaid_security_id)
);

-- --- machine-local from here down ---

-- Investment activity this machine has seen and will not post without a person.
--
-- Local, like `plaid_staged_transactions`, and nothing is ever dropped into it
-- silently: the raw provider payload is kept beside the reason, so a review can
-- see exactly what arrived rather than this importer's summary of it.
--
-- # What lands here, and why each one is held rather than guessed
--
-- * **an unconfigured account** — there is nowhere to post it (see the config
--   table above);
-- * **a corporate action**: a split, a merger, a spin-off, a return of capital,
--   which Plaid reports as a `transfer` or not at all. Spec §7 is explicit:
--   guessing a split silently restates every gain on that security, for ever, and
--   a wrong basis is worse than a missing one because nobody goes looking for it;
-- * **cash into or out of a sheltered account** — the provider cannot say whether
--   that is a contribution or a distribution, and the two have opposite effects on
--   a tax return;
-- * **cash into or out of a taxable account with no clearing account configured**;
-- * **anything a command refused** — a sale larger than the position, a posting
--   into a closed year — carrying the refusal as the reason.
CREATE TABLE IF NOT EXISTS investment_staged_activity (
    id TEXT PRIMARY KEY,
    item_id TEXT NOT NULL,
    plaid_account_id TEXT NOT NULL,
    -- UNIQUE, so the rolling re-fetch does not grow this list by a copy of itself
    -- every run. Together with `investment_imports` this is the whole dedup fence:
    -- one table for what posted, one for what is waiting.
    provider_transaction_id TEXT NOT NULL UNIQUE,
    -- Why it is here: 'unconfigured', 'unhandled_type', 'corporate_action',
    -- 'sheltered_cash', 'no_clearing_account', 'rejected', 'unknown_security',
    -- 'bad_amount'.
    reason TEXT NOT NULL,
    -- A sentence a person can act on, including a command's own refusal text.
    detail TEXT NOT NULL,
    -- Plaid's own type and subtype, lifted out of the payload so a review list can
    -- be grouped and sorted without parsing JSON.
    provider_type TEXT NOT NULL,
    provider_subtype TEXT NOT NULL,
    date TEXT NOT NULL,
    name TEXT NOT NULL,
    amount_cents INTEGER,
    -- The whole provider payload, as JSON. The point of keeping it: a corporate
    -- action is decided by hand from what the broker actually said, and a
    -- summary of it drops precisely the field that turns out to matter.
    raw_payload TEXT NOT NULL,
    staged_at TEXT NOT NULL DEFAULT (datetime('now')),
    status TEXT NOT NULL DEFAULT 'pending'
);

CREATE INDEX IF NOT EXISTS idx_investment_staged_status
    ON investment_staged_activity(status, date);

-- How far this machine has fetched each account.
--
-- `/investments/transactions/get` is date-ranged rather than cursor-based (spec
-- §6), so there is no server-side position to resume from and the window has to be
-- chosen here: full history the first time, then a rolling 30 days on top of the
-- last date fetched, deduplicated against `investment_imports`.
--
-- Local, and the failure mode says why that is safe: a machine with no row here
-- pulls full history and the dedup register discards everything it already had.
-- The cost is one slow fetch. A *replicated* last-fetch date has the opposite and
-- much worse failure: machine A records that it fetched through Friday, machine B
-- never actually received those transactions, and the window they are in never
-- comes round again.
CREATE TABLE IF NOT EXISTS investment_fetch_state (
    item_id TEXT NOT NULL,
    plaid_account_id TEXT NOT NULL,
    -- The end of the last window fetched, inclusive. NULL cannot happen — a row
    -- only exists because a fetch finished — which is what makes "no row" mean
    -- "never fetched" unambiguously.
    last_fetched_through TEXT NOT NULL,
    last_fetched_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (item_id, plaid_account_id)
);
