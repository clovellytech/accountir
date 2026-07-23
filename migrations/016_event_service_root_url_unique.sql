-- At most one ACTIVE event service per root_url (SPEC §6.2). The in-txn check in
-- register_service (event_service_commands.rs) rejects a duplicate active
-- registration; this is the DB-level backstop, mirroring that check exactly.
-- register_service stores the normalized url (trailing '/' trimmed), so the index
-- on the column matches the stored value. A *partial* index only constrains
-- active rows, so a disconnected/removed service can be re-registered.
--
-- NOTE: if an existing database already holds duplicate active root_urls
-- (possible, since this was never enforced), creating the index will fail; those
-- rows must be deduped/deactivated first.
CREATE UNIQUE INDEX IF NOT EXISTS idx_event_services_active_root_url
    ON event_services(root_url) WHERE status = 'active';
