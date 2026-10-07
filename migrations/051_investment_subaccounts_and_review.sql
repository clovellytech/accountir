-- The desktop's two demands on phase 4's registers: securities split by kind with
-- four income accounts instead of two, and a held row that can be resolved.
--
-- INVESTMENTS-SPEC.md phase 5. Nothing here is a new table — both changes are
-- columns on migration 050's tables, and the split falls the same way it did
-- there: the configuration is replicated because the whole book depends on which
-- account a dividend posts to, and what somebody has reviewed is machine-local.
--
-- # Why the securities account became three
--
-- A broker may compute a **mutual fund's** cost basis by average cost, which the
-- regulations permit for funds (§1.1012-1(e)) and do not permit for stocks. Our
-- basis is always the sum of the lots we imported, so a difference against the
-- broker's figure means one thing on a stock — a finding, spec §7's real
-- safeguard — and quite possibly another on a fund, where the two methods simply
-- disagree. A trial balance that keeps them in separate accounts can say which
-- kind of difference it is looking at; one account holding both cannot.
--
-- Three slots and not one per security: an account per holding makes the chart of
-- accounts unreadable inside a year, which is the same reason lots live in a
-- register rather than in the chart (spec §4).
--
-- `securities_account_id` is NOT renamed, although it is now the stocks slot. It is
-- named in the CHECK constraint, and SQLite cannot alter a constraint without
-- rebuilding the table — which would mean rebuilding it on every replica too. More
-- to the point, the column's *meaning* has not changed for any row that already
-- exists: everything was carried in it, and everything still is until somebody
-- configures the other two. See `TaxableBrokerageAccounts::securities_account_of`
-- for the fallback, and why the fallback is the stocks slot rather than an error.
ALTER TABLE investment_account_config ADD COLUMN mutual_funds_account_id TEXT;
ALTER TABLE investment_account_config ADD COLUMN other_securities_account_id TEXT;

-- The two income accounts phase 4 did not have.
--
-- Neither is a refinement of dividends or interest; each reaches a different line
-- of a return:
--
-- * **tax-exempt interest** is reported and not taxed (Form 1040 line 2a). Folded
--   into ordinary interest it overstates taxable income; left out of the books it
--   loses a figure the return still has to state. The importer never chooses it —
--   Plaid has no subtype that distinguishes municipal interest — so it is for a
--   payment entered by hand and for the year-end split off the 1099-INT, which is
--   the order spec §7 already sets for qualified dividends;
-- * **a capital gain distribution** is Schedule D, not Schedule B. Phase 4 held
--   every one of them for review because calling one a dividend puts it on the
--   wrong form at the wrong rate. With this column configured they post; without
--   it they are still held, which is why it is nullable and has no fallback.
ALTER TABLE investment_account_config ADD COLUMN tax_exempt_interest_account_id TEXT;
ALTER TABLE investment_account_config ADD COLUMN capital_gain_distribution_account_id TEXT;

-- What was done about a held row.
--
-- Phase 4 wrote rows with `status = 'pending'` and had no way to move them off it,
-- so the review list only ever grew. The three statuses are now 'pending',
-- 'resolved' and 'dismissed', and the last two are deliberately **different
-- statuses rather than one "done"**: a resolution says the activity reached the
-- books some other way, a dismissal says it never will. Collapsing them would make
-- "is anything missing from these books?" unanswerable, which is the one question
-- a review list exists to answer.
--
-- No CHECK on the status: SQLite cannot add one to an existing table, and the
-- transition functions in `investment_import` are the only writers — each refuses
-- a row that is not pending, so a second click cannot post a second time.
--
-- Machine-local, like the row it describes. What one member has looked at and
-- decided is not a fact about the books; the *entry* a resolution posted is, and
-- that is in the ledger, in the log, where it belongs. `resolution_entry_id` is a
-- pointer to it and not a second copy of it.
ALTER TABLE investment_staged_activity ADD COLUMN resolution TEXT;
ALTER TABLE investment_staged_activity ADD COLUMN resolution_note TEXT;
ALTER TABLE investment_staged_activity ADD COLUMN resolution_entry_id TEXT;
ALTER TABLE investment_staged_activity ADD COLUMN resolved_at TEXT;
