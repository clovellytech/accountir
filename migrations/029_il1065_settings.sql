-- The Illinois IL-1065 settings for this book: the standing choices that shape
-- the state return.
--
-- Two flags — whether income is earned outside Illinois (apportionment) and
-- whether the pass-through entity tax was elected — drive which path
-- tax::il1065 takes when it fills the form. They are the partnership's position,
-- not one year's figures, and they differ between businesses, so they are
-- recorded once per book.
--
-- Event-sourced like business_profile (migration 023) rather than local like
-- partner_tins: which return a partnership files is a fact every member preparing
-- it must agree on, not a secret. So this is a projection of Il1065SettingsSet,
-- one row keyed 'default' — one partnership per book, exactly as business_profile
-- is one row.
--
-- No foreign key anywhere; updated_at_event references events, which is safe
-- because rebuild truncates projections and the log is not one (migration 025).
CREATE TABLE IF NOT EXISTS il1065_settings (
    id TEXT PRIMARY KEY CHECK (id = 'default'),
    -- 1 if any income is earned outside Illinois (apportion), 0 if Illinois-only.
    apportions_outside_illinois INTEGER NOT NULL DEFAULT 0,
    -- 1 if the partnership elected the 4.95% Pass-through Entity tax.
    elects_pte_tax INTEGER NOT NULL DEFAULT 0,
    updated_at_event INTEGER REFERENCES events(id)
);
