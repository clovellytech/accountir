-- Which group server this ledger file is a replica of.
--
-- The binding lives in the *business's own database*, not in the machine's
-- registry, for two reasons:
--   1. it is a property of this ledger, so it must travel with a copied,
--      restored or synced DB file rather than being lost or, worse, inherited
--      from whatever the machine happened to be pointed at;
--   2. it makes "attach the replica of group A to group B" structurally
--      impossible — the file already says which group it mirrors, and
--      `binding::bind` refuses to overwrite a different one.
--
-- The single-row CHECK is the enforcement: one file, at most one group.
-- Deliberately no token, password or user id column — credentials never touch
-- disk (see accountir-desktop/src/session.rs for the full reasoning).
CREATE TABLE IF NOT EXISTS group_binding (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    group_id TEXT NOT NULL,
    instance_url TEXT NOT NULL,
    control_plane_url TEXT NOT NULL,
    bound_at TEXT NOT NULL,
    -- Last head the server reported, for the UI's "synced N ago". NOT the sync
    -- cursor: the cursor is MAX(events.id), so it cannot drift from the data.
    last_server_head INTEGER NOT NULL DEFAULT 0,
    last_synced_at TEXT
);
