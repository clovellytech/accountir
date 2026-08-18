-- How often a service's sales become journal entries, and from when.
--
-- In the log rather than in a local preference, because the alternative
-- double-counts revenue. A rollup's idempotency key carries its period —
-- `bugbear:rollup:daily:2026-08-17` against `bugbear:rollup:monthly:2026-08` — so
-- two members of a group syncing the same service at different frequencies
-- produce keys that do not collide, nothing catches the overlap, and the month
-- posts twice. One value, replicated, and every member aggregates the same way.
--
-- `per_event` is the default so that every service registered before this
-- existed, and every one registered after, behaves exactly as it did: one entry
-- per sale until somebody chooses otherwise.
ALTER TABLE event_services ADD COLUMN reporting_frequency TEXT NOT NULL DEFAULT 'per_event';

-- The date the current frequency applies from. NULL means "always", which is
-- what `per_event` has always meant.
--
-- Dated because switching mid-period would otherwise re-total days already
-- posted under a key that does not match them. Everything before the cut-over
-- keeps the shape it was posted with, and the two styles simply meet at a date.
ALTER TABLE event_services ADD COLUMN reporting_from TEXT;
