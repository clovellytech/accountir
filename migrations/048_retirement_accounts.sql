-- The sheltered-account register: which ledger accounts are retirement accounts,
-- what kind each is, and what the last statement said they were worth.
--
-- INVESTMENTS-SPEC.md phase 2. Event-sourced like the brokerage register
-- (migration 047) and the asset register (030), and for the same reason: which
-- account is a 401(k) and which is a Roth decides what a distribution out of it
-- puts on a 1099-R, and that is a fact the whole book files on rather than a
-- setting on one laptop.
--
-- No foreign key to `accounts`, deliberately, for the reason migration 025 gives:
-- `Projector::rebuild` truncates projections and replays, and a foreign key would
-- fight the order rows come back in.
--
-- # Why there are no securities and no lots in here
--
-- Because nothing inside a sheltered account is taxable (spec §2b). A buy, a
-- sale, a dividend and a reinvestment inside a 401(k) have no tax consequence
-- whatever, so lot accounting there is machinery that answers no question: there
-- is no Form 8949 row to produce, no holding period that matters, and no basis
-- anybody will ever need. The account is therefore ONE ledger account carried at
-- **value**, moved by one entry per statement period, and the four hundred trades
-- a target-date fund makes in a year are never recorded at all.
--
-- That is the opposite of the taxable rule in migration 047, where cost is
-- carried and value is only ever a report. The difference is not inconsistency:
-- cost is carried in a taxable account because cost is what a gain is measured
-- against, and there is no gain to measure here.
--
-- # Units
--
-- Money is `_cents` INTEGER, like every other money column in this schema.

CREATE TABLE IF NOT EXISTS retirement_accounts (
    -- The ledger account carried at value, and the register's key: one ledger
    -- account is one retirement account. PRIMARY KEY rather than a surrogate id
    -- precisely so the register cannot hold two opinions about one account —
    -- two rows would mean two kinds, and a distribution would be taxable or not
    -- depending on which row was read.
    account_id TEXT PRIMARY KEY,
    -- Who holds it: "Fidelity ••5678". A label, for the register to be readable
    -- beside a chart of accounts that names the same thing.
    institution TEXT NOT NULL,
    -- 'traditional', 'roth' or 'other'. A CLOSED set, unlike `securities.kind`,
    -- and the difference is where the vocabulary comes from: a security's type is
    -- whatever a broker calls it, while how a distribution is taxed is decided by
    -- the statute, which knows pre-tax money, after-tax money, and the handful of
    -- purpose-built accounts (529, HSA) that are neither. 'other' is that last
    -- group, and it is the one case where the register declines to decide the
    -- taxable amount and makes the caller say it.
    kind TEXT NOT NULL,
    -- `Income:Investments:Retirement value change` — where growth and shrinkage
    -- go. Non-taxable by construction: see `tax::lines::load_effective_mapping`,
    -- which forces this account off every tax line no matter what the mapping
    -- table says.
    --
    -- Not unique, on purpose: spec §2b's chart has one value-change account for
    -- the whole book, shared by every sheltered account in it.
    value_change_account_id TEXT NOT NULL,
    -- What the most recent statement said the account was worth, and as of when.
    -- NULL until the first value is set — which is distinct from a value of zero,
    -- and the distinction is load-bearing: the out-of-order fence has nothing to
    -- compare against until a first value exists, and a newly registered account
    -- that happens to be empty must not be read as "the statement says $0".
    last_value_cents INTEGER,
    last_value_as_of TEXT,
    registered_at_event INTEGER REFERENCES events(id),
    updated_at_event INTEGER REFERENCES events(id)
);

-- "Is this account a value-change account?" — asked on every tax-line lookup and
-- on every attempt to map an account to a line, which is what makes the
-- exclusion by construction rather than by memory.
CREATE INDEX IF NOT EXISTS idx_retirement_accounts_value_change
    ON retirement_accounts(value_change_account_id);
