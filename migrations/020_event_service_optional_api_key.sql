-- `event_services.api_key` becomes nullable.
--
-- On group-hosted books a service is registered without its key. The key belongs
-- to the member who holds the account at the service, and `group.db` is the event
-- log — replicated in full to every member's laptop and into every backup they
-- take. A key written there is a key on N machines with no way back short of
-- rotating it at the service.
--
-- So it goes where the bank-grant token goes: the group's instance, in its own
-- database, one copy, one audit point (see `accountir-server/src/servicekeys.rs`).
-- What reaches the shared log is only what the group needs in order to *see* the
-- connection — its name and its root URL. NULL here means "registered on hosted
-- books; ask the instance".
--
-- Standalone books are unaffected: they keep putting the key in their own log,
-- which nobody else replicates, and those rows keep a non-NULL value.
--
-- SQLite cannot drop a NOT NULL constraint in place, so the table is rebuilt.
--
-- `foreign_keys=OFF` around the rebuild is load-bearing. `staged_service_events`
-- references `event_services(id)`, so with enforcement on the DROP either fails
-- outright or takes those rows with it — and staged rows are a member's unposted
-- review queue. With enforcement off the drop touches only this table, and
-- because ids are copied across unchanged the children still resolve afterwards.
--
-- This is SQLite's own documented procedure ("Making Other Kinds Of Table Schema
-- Changes"). It is safe in `execute_batch` because the migration runner opens no
-- transaction of its own — a `PRAGMA foreign_keys` inside one is silently
-- ignored, which is the failure mode to watch for if that ever changes.
PRAGMA foreign_keys=OFF;

CREATE TABLE IF NOT EXISTS event_services_new (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root_url TEXT NOT NULL,
    -- NULL means "registered on hosted books"; see above.
    api_key TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    cursor TEXT,
    last_synced_at TEXT,
    events_processed INTEGER DEFAULT 0,
    entries_created INTEGER DEFAULT 0,
    connected_at_event INTEGER REFERENCES events(id)
);

INSERT INTO event_services_new (id, name, root_url, api_key, status, cursor,
                                last_synced_at, events_processed, entries_created,
                                connected_at_event)
    SELECT id, name, root_url, api_key, status, cursor,
           last_synced_at, events_processed, entries_created, connected_at_event
    FROM event_services;

DROP TABLE event_services;
ALTER TABLE event_services_new RENAME TO event_services;

-- The rebuild dropped it with the old table. Same definition as migration 016 —
-- at most one ACTIVE service per root_url, the DB backstop for the in-txn check.
CREATE UNIQUE INDEX IF NOT EXISTS idx_event_services_active_root_url
    ON event_services(root_url) WHERE status = 'active';

PRAGMA foreign_keys=ON;
