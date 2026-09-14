-- How a liability account bears on Schedule K-1 item K.
--
-- # Why this exists
--
-- The ledger holds each liability's balance and nothing about who bears the
-- economic risk of loss on it under §752, which decides whether it is
-- nonrecourse, qualified nonrecourse financing or recourse — and, if recourse,
-- to whom. An account with no row here takes the kind of entity's default (see
-- `tax::liabilities`); a row records the exception: a loan a partner made or
-- guaranteed, financing that qualifies under §465(b)(6).
--
-- `partner_id` is only set for a recourse liability one partner bears; a
-- recourse liability without one is shared on the loss percentages.
-- `guaranteed` ticks that partner's item K3. The note is required, because a
-- classification is a statement about loan documents somebody may have to show.
CREATE TABLE IF NOT EXISTS liability_classifications (
    account_id       TEXT PRIMARY KEY,
    kind             TEXT NOT NULL,
    partner_id       TEXT,
    guaranteed       INTEGER NOT NULL DEFAULT 0,
    note             TEXT NOT NULL,
    updated_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id)
);
