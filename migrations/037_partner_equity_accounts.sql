-- Which ledger accounts hold a partner's capital, and in what role.
--
-- # Why this cannot be inferred
--
-- Schedule K-1 item L is a partner's capital account: what they started with,
-- what they put in, their share of income, what they took out. The books hold
-- all of that already — but in accounts whose only link to a partner is a name
-- somebody typed. On these books `4002 Zak` and `4005 Zak` are one partner's
-- contributions and draws; `4003`/`4006` are another's. Matching on the name
-- would attach a partner to an account the day somebody renames one.
--
-- # Why one partner may own several
--
-- Contributions and draws are conventionally kept apart, so the natural shape is
-- one partner to many accounts, each with a role. Collapsing them to a single
-- "capital account" column would force a chart of accounts to be reorganised to
-- suit the tax form, which is the wrong way round.
CREATE TABLE IF NOT EXISTS partner_equity_accounts (
    partner_id  TEXT NOT NULL REFERENCES partners(id),
    account_id  TEXT NOT NULL,
    -- 'contribution' | 'draw'. Text rather than an integer so the row reads.
    role        TEXT NOT NULL,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (partner_id, account_id)
);

-- An account belongs to at most one partner; the reverse lookup needs to be
-- cheap and unambiguous.
CREATE UNIQUE INDEX IF NOT EXISTS idx_partner_equity_accounts_account
    ON partner_equity_accounts(account_id);
