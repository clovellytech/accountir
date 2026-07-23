-- Staged service events: fetched events awaiting user review before import
CREATE TABLE IF NOT EXISTS staged_service_events (
    id TEXT PRIMARY KEY,
    service_id TEXT NOT NULL REFERENCES event_services(id),
    remote_event_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    data TEXT NOT NULL,
    timestamp TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    error_message TEXT,
    staged_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(service_id, remote_event_id)
);

CREATE INDEX IF NOT EXISTS idx_staged_svc_events_status ON staged_service_events(status);
