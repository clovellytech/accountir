-- Sole proprietorships: which return a set of books files, and who files it.
--
-- Until now every ledger was a partnership filing Form 1065. A business with one
-- owner files Schedule C attached to that owner's Form 1040 instead, and the two
-- forms want the same money on entirely different lines — a partnership's income
-- is split across partners and separately stated items, a sole proprietor's is
-- one net figure. Nothing in the accounts can tell the two apart, so
-- `business_profile.business_type` is asked rather than inferred.
--
-- Guessing would be worse than asking. A sole proprietorship with no partners
-- entered looks exactly like a partnership whose partners have not been entered
-- yet, and the return that comes out either way looks finished.

-- Which return these books file: 'partnership' or 'sole_proprietorship'.
--
-- Defaulted to 'partnership' so every existing book keeps filing what it filed
-- yesterday. A book that has never been asked is not thereby a sole
-- proprietorship.
ALTER TABLE business_profile ADD COLUMN business_type TEXT NOT NULL DEFAULT 'partnership';

-- The individual who owns a sole proprietorship, as Schedule C's header asks for
-- them.
--
-- One row, keyed 'default' by a CHECK, for the reason `business_profile` is one
-- row: one ledger file is one business, and a sole proprietorship has exactly one
-- owner — that is what makes it one.
--
-- Deliberately small. Everything the form wants about the *business* — name,
-- address, EIN, business code — is already in `business_profile` and is read from
-- there. All that was missing is the person.
--
-- # Why the proprietor is not a row in `partners`
--
-- A partner owns an entity that files its own return. A sole proprietor's
-- business has no return of its own at all: there is no Schedule K-1, no capital
-- account on a Schedule L, and the identifying number on the form is the owner's
-- SSN rather than the business's EIN. Modelling them as a partner would emit a
-- K-1 for somebody who must never receive one.
--
-- Event-sourced like `business_profile`, and carrying no identifying number —
-- see `sole_proprietor_tin` below.
CREATE TABLE IF NOT EXISTS sole_proprietor (
    id TEXT PRIMARY KEY CHECK (id = 'default'),
    -- As it appears on the owner's Form 1040. The IRS pairs the two by name and
    -- SSN, so a mismatch is a correspondence letter.
    name TEXT NOT NULL,
    -- Schedule C line F: 'cash', 'accrual' or 'other'.
    accounting_method TEXT NOT NULL DEFAULT 'cash',
    -- What line F(3) prints when the method is 'other'. NULL otherwise.
    accounting_method_other TEXT,
    updated_at_event INTEGER REFERENCES events(id)
);

-- The proprietor's social security number, held locally and never in the event
-- log.
--
-- The same rule `partner_tins` follows (migration 023) and for the same reason:
-- the log is replicated in full to every member's machine and is append-only, so
-- a number written into it is on every other machine permanently, with no way to
-- redact it.
--
-- The argument is stronger here than for a partnership. A partnership's TIN is
-- usually an EIN — a number about a business. A sole proprietor's identifying
-- number on Schedule C is their own social security number, and the entire return
-- is about one person.
--
-- The cost is the same one `partner_tins` accepts: SSNs do not sync. A member who
-- has not entered one locally produces a Schedule C with the number box blank,
-- which is a form you can see is incomplete — rather than one carrying a number
-- that reached them by a route nobody intended.
CREATE TABLE IF NOT EXISTS sole_proprietor_tin (
    id TEXT PRIMARY KEY CHECK (id = 'default'),
    ssn TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Schedule C's yes/no and choice questions, per tax year.
--
-- Keyed by year as well as question because the form asks about "2025": whether
-- you materially participated, whether the business started this year, whether
-- 1099s were required and filed. Last year's answers are last year's — the same
-- reasoning `schedule_b_answers` records in migration 026, and the same shape, so
-- the two read alike.
--
-- A separate table rather than a reused one: `schedule_b_answers` is named for
-- the schedule it holds, and a row of Schedule C answers sitting in it would be
-- a lie that every future reader has to decode.
CREATE TABLE IF NOT EXISTS schedule_c_answers (
    tax_year INTEGER NOT NULL,
    -- See tax::schedule_c::QUESTIONS for what a key means. Keys, never line
    -- numbers: the IRS renumbers between revisions.
    answer_key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at_event INTEGER REFERENCES events(id),
    PRIMARY KEY (tax_year, answer_key)
);
