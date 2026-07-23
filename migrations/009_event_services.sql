-- Event services: external apps that publish accounting events via accountir-events
CREATE TABLE IF NOT EXISTS event_services (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    root_url TEXT NOT NULL,
    api_key TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    cursor TEXT,
    last_synced_at TEXT,
    events_processed INTEGER DEFAULT 0,
    entries_created INTEGER DEFAULT 0,
    connected_at_event INTEGER REFERENCES events(id)
);
