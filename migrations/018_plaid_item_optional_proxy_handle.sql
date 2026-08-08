-- `plaid_items.proxy_item_id` becomes nullable.
--
-- On group-hosted books a bank connection is recorded without the proxy's handle:
-- it is inert without the owner's proxy API key, it is read only by the machine
-- that talks to the proxy, and on hosted books that is the group's instance using
-- a grant — which holds the handle in its own store. Sharing it into a log every
-- member replicates would hand round something no member can use.
--
-- SQLite cannot drop a NOT NULL constraint in place, so the table is rebuilt.
--
-- `foreign_keys=OFF` around the rebuild is load-bearing, and getting it wrong
-- DESTROYS DATA. Three tables reference `plaid_items(id)` with ON DELETE CASCADE
-- — plaid_local_accounts, plaid_imported_transactions, plaid_staged_transactions.
-- With enforcement on, `DROP TABLE plaid_items` cascades and deletes every one of
-- their rows: the first draft of this migration silently emptied a real ledger's
-- account mappings, and the unit tests missed it because their fixtures had no
-- child rows. With enforcement off the drop touches only the parent, and because
-- ids are copied across unchanged the children still resolve afterwards.
--
-- This is SQLite's own documented procedure for altering a table (see "Making
-- Other Kinds Of Table Schema Changes"). It is safe in `execute_batch` because
-- the migration runner opens no transaction of its own — a `PRAGMA foreign_keys`
-- inside one would be silently ignored, which is the failure mode to watch for if
-- that ever changes.
PRAGMA foreign_keys=OFF;

CREATE TABLE IF NOT EXISTS plaid_items_new (
    id TEXT PRIMARY KEY,
    -- NULL means "recorded on hosted books"; see above.
    proxy_item_id TEXT,
    institution_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    last_synced_at TEXT,
    connected_at_event INTEGER REFERENCES events(id)
);

INSERT INTO plaid_items_new (id, proxy_item_id, institution_name, status, last_synced_at, connected_at_event)
    SELECT id, proxy_item_id, institution_name, status, last_synced_at, connected_at_event
    FROM plaid_items;

DROP TABLE plaid_items;
ALTER TABLE plaid_items_new RENAME TO plaid_items;

PRAGMA foreign_keys=ON;
