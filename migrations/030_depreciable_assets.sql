-- The asset register: what the partnership owns that depreciates, and the facts
-- MACRS needs about each piece.
--
-- Every other figure on the return is a sum over the ledger. Depreciation is not.
-- The deduction for a kiln depends on its cost, the date it was placed in
-- service, its recovery class and which year of that class the return is — and a
-- journal entry records none of those. The ledger holds the *result* of the
-- calculation and never its inputs, so the calculation cannot be redone from the
-- books. This table is the inputs.
--
-- Event-sourced like `partners` (migration 023), not local like `partner_tins`:
-- what the partnership owns and how it is being depreciated is a fact the whole
-- partnership files on, not a secret belonging to one machine. So it is a
-- projection of DepreciableAssetAdded / Updated / Disposed / Removed, and every
-- member sees the same register.
--
-- No foreign key to `accounts`, deliberately, for the reason migration 025 gives:
-- `Projector::rebuild` truncates projections and replays, and a foreign key would
-- fight the order rows come back in. `updated_at_event` referencing `events` is
-- safe — the log is not a projection.
--
-- # Why two dates
--
-- `acquired_on` and `placed_in_service` are different facts and neither implies
-- the other. The recovery period and the averaging convention run from the date
-- the asset was placed in service — available and ready for its intended use — so
-- a kiln bought in November and first fired in January depreciates from January.
--
-- The bonus depreciation rate, by contrast, turns on acquisition. 2025 is a split
-- year: property acquired before 20 January 2025 earns 40% under the §168(k)(6)
-- phase-down, and property acquired on or after earns 100% under the restoration
-- enacted that July. One date column could not answer both questions, and on a
-- large asset the difference is most of its cost.
--
-- # Why the class is stored by name and not by a number of years
--
-- Because a recovery period does not determine a method. Land improvements and
-- qualified improvement property are both 15-year property: the first is 150%
-- declining balance, the second is straight line. A column holding `15` could not
-- tell a parking lot from a leased studio's fit-out, and would silently apply
-- declining balance to the fit-out and overstate its early years. `property_class`
-- holds `domain::PropertyClass::as_str` — 'fifteen_year_land_improvement' or
-- 'qualified_improvement' — which carries the method with the life.
CREATE TABLE IF NOT EXISTS depreciable_assets (
    id TEXT PRIMARY KEY,
    description TEXT NOT NULL,
    -- The fixed-asset account the cost sits in, so Schedule L line 9a can be
    -- reconciled against the register rather than assumed to agree with it.
    asset_account_id TEXT NOT NULL,
    -- Where the year's deduction is posted: debit the expense, credit the
    -- contra-asset. Held per asset rather than globally so a register can carry
    -- studio equipment and leasehold improvements against separate accounts.
    expense_account_id TEXT NOT NULL,
    accumulated_account_id TEXT NOT NULL,
    -- Where a §179 election is expensed, when one is made. Separate from
    -- `expense_account_id` and not optional once §179 is elected, because the two
    -- reach different lines of the return: ordinary depreciation is page 1 line
    -- 16a, and §179 is separately stated on Schedule K line 12 and K-1 box 12,
    -- since each partner applies their own dollar and taxable-income limits to
    -- it. Posting both to one account would deduct at the partnership level what
    -- the statute deducts at the partner level, and double-count it against the
    -- box 12 the partner also receives. NULL when no election is made.
    section_179_account_id TEXT,
    -- ISO-8601. See the note above on why both are here.
    acquired_on TEXT NOT NULL,
    placed_in_service TEXT NOT NULL,
    -- Cost or other basis, in cents, like every other money column.
    cost_cents INTEGER NOT NULL,
    -- domain::PropertyClass::as_str — never a bare number of years.
    property_class TEXT NOT NULL,
    -- 'gds' or 'ads' — domain::System.
    system TEXT NOT NULL DEFAULT 'gds',
    -- The §179 election against this asset, in cents. 0 for no election.
    section_179_cents INTEGER NOT NULL DEFAULT 0,
    -- 'take' or 'decline' — domain::BonusElection. Per asset here, though
    -- §168(k)(7) makes the election per class per year; tax::depreciation reports
    -- a class where the assets disagree rather than filing an election that was
    -- never validly made.
    bonus TEXT NOT NULL DEFAULT 'take',
    -- NULL while the asset is still held. Set on disposal, which stops
    -- depreciation part-way through the year on the asset's own convention and
    -- takes it off Schedule L.
    disposed_on TEXT,
    notes TEXT,
    added_at_event INTEGER REFERENCES events(id),
    updated_at_event INTEGER REFERENCES events(id)
);

CREATE INDEX IF NOT EXISTS idx_depreciable_assets_placed
    ON depreciable_assets(placed_in_service);
