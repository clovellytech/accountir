use rusqlite::Connection;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum MigrationError {
    #[error("Database error: {0}")]
    DatabaseError(#[from] rusqlite::Error),
    #[error("Migration failed: {0}")]
    MigrationFailed(String),
}

/// Run all database migrations
pub fn run_migrations(conn: &Connection) -> Result<(), MigrationError> {
    // Create migrations table if it doesn't exist
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
        [],
    )?;

    // Get current version
    let current_version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // Run migrations
    let migrations: Vec<(i64, &str)> = vec![
        (1, include_str!("../../migrations/001_initial.sql")),
        (2, include_str!("../../migrations/002_add_company_id.sql")),
        (3, include_str!("../../migrations/003_bank_imports.sql")),
        (4, include_str!("../../migrations/004_plaid.sql")),
        (5, include_str!("../../migrations/005_plaid_staging.sql")),
        (
            6,
            include_str!("../../migrations/006_plaid_payment_meta.sql"),
        ),
        (
            7,
            include_str!("../../migrations/007_plaid_balance_snapshot.sql"),
        ),
        (8, include_str!("../../migrations/008_ingest_mappings.sql")),
        (9, include_str!("../../migrations/009_event_services.sql")),
        (10, include_str!("../../migrations/010_ap_ar.sql")),
        (
            11,
            include_str!("../../migrations/011_staged_service_events.sql"),
        ),
        (12, include_str!("../../migrations/012_vendor_rules.sql")),
        (
            13,
            include_str!("../../migrations/013_reconciliation_one_in_progress.sql"),
        ),
        (
            14,
            include_str!("../../migrations/014_journal_entry_reference_unique.sql"),
        ),
        (
            15,
            include_str!("../../migrations/015_event_actor_identity.sql"),
        ),
        (
            16,
            include_str!("../../migrations/016_event_service_root_url_unique.sql"),
        ),
        (17, include_str!("../../migrations/017_group_binding.sql")),
        (
            18,
            include_str!("../../migrations/018_plaid_item_optional_proxy_handle.sql"),
        ),
    ];

    for (version, sql) in migrations {
        if version > current_version {
            match conn.execute_batch(sql) {
                Ok(()) => {}
                Err(e) => {
                    // If a migration fails because the schema already matches
                    // (e.g. init_schema already created the column), treat it
                    // as already applied rather than failing.
                    let msg = e.to_string();
                    if msg.contains("duplicate column") || msg.contains("already exists") {
                        // Column/table already exists — schema is up to date
                    } else {
                        return Err(MigrationError::DatabaseError(e));
                    }
                }
            }
            conn.execute(
                "INSERT OR IGNORE INTO schema_migrations (version) VALUES (?1)",
                [version],
            )?;
        }
    }

    Ok(())
}

/// Initialize the database with the schema (for new databases or testing)
pub fn init_schema(conn: &Connection) -> Result<(), MigrationError> {
    conn.execute_batch(
        r#"
        -- Core event store (append-only)
        -- actor_id / received_at are the server-identity fields (migration 015).
        -- Nullable: NULL = legacy/solo single-writer. Not hash inputs.
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            event_type TEXT NOT NULL,
            payload TEXT NOT NULL,
            hash BLOB NOT NULL,
            user_id TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            actor_id TEXT,
            received_at TEXT,
            UNIQUE(hash)
        );

        -- Merkle tree nodes (rebuilt on sync)
        CREATE TABLE IF NOT EXISTS merkle_nodes (
            level INTEGER NOT NULL,
            position INTEGER NOT NULL,
            hash BLOB NOT NULL,
            left_child_pos INTEGER,
            right_child_pos INTEGER,
            PRIMARY KEY (level, position)
        );

        -- Materialized projections
        CREATE TABLE IF NOT EXISTS accounts (
            id TEXT PRIMARY KEY,
            account_type TEXT NOT NULL,
            account_number TEXT NOT NULL,
            name TEXT NOT NULL,
            parent_id TEXT,
            currency TEXT,
            description TEXT,
            is_active INTEGER DEFAULT 1,
            created_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS journal_entries (
            id TEXT PRIMARY KEY,
            date TEXT NOT NULL,
            memo TEXT,
            reference TEXT,
            source TEXT,
            is_void INTEGER DEFAULT 0,
            voided_by_entry_id TEXT,
            posted_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS journal_lines (
            id TEXT PRIMARY KEY,
            entry_id TEXT NOT NULL REFERENCES journal_entries(id),
            account_id TEXT NOT NULL REFERENCES accounts(id),
            amount INTEGER NOT NULL,
            currency TEXT NOT NULL,
            exchange_rate REAL,
            memo TEXT,
            is_cleared INTEGER DEFAULT 0,
            cleared_at_event INTEGER
        );

        CREATE TABLE IF NOT EXISTS currencies (
            code TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            symbol TEXT,
            decimal_places INTEGER DEFAULT 2
        );

        CREATE TABLE IF NOT EXISTS exchange_rates (
            id INTEGER PRIMARY KEY,
            from_currency TEXT NOT NULL,
            to_currency TEXT NOT NULL,
            rate REAL NOT NULL,
            effective_date TEXT NOT NULL,
            recorded_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS reconciliations (
            id TEXT PRIMARY KEY,
            account_id TEXT NOT NULL REFERENCES accounts(id),
            statement_date TEXT NOT NULL,
            statement_ending_balance INTEGER NOT NULL,
            status TEXT NOT NULL,
            started_at_event INTEGER REFERENCES events(id),
            completed_at_event INTEGER
        );

        CREATE TABLE IF NOT EXISTS cleared_transactions (
            reconciliation_id TEXT NOT NULL REFERENCES reconciliations(id),
            entry_id TEXT NOT NULL,
            line_id TEXT NOT NULL,
            cleared_amount INTEGER NOT NULL,
            cleared_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (reconciliation_id, entry_id, line_id)
        );

        CREATE TABLE IF NOT EXISTS users (
            id TEXT PRIMARY KEY,
            username TEXT UNIQUE NOT NULL,
            role TEXT NOT NULL,
            is_active INTEGER DEFAULT 1,
            created_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS company (
            id TEXT PRIMARY KEY,
            company_id TEXT NOT NULL,
            name TEXT NOT NULL,
            base_currency TEXT NOT NULL,
            fiscal_year_start_month INTEGER DEFAULT 1,
            created_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS fiscal_years (
            year INTEGER PRIMARY KEY,
            start_date TEXT NOT NULL,
            end_date TEXT NOT NULL,
            is_closed INTEGER DEFAULT 0,
            retained_earnings_entry_id TEXT
        );

        CREATE TABLE IF NOT EXISTS fiscal_periods (
            year INTEGER NOT NULL,
            period INTEGER NOT NULL,
            start_date TEXT NOT NULL,
            end_date TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'open',
            closed_by_user_id TEXT,
            closed_at TEXT,
            PRIMARY KEY (year, period)
        );

        -- Bank import mappings (links extension bank recipes to TUI accounts)
        CREATE TABLE IF NOT EXISTS bank_accounts (
            bank_id TEXT PRIMARY KEY,
            bank_name TEXT NOT NULL,
            account_id TEXT NOT NULL REFERENCES accounts(id),
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Pending bank imports (files waiting to be processed)
        CREATE TABLE IF NOT EXISTS pending_imports (
            id INTEGER PRIMARY KEY,
            file_path TEXT NOT NULL,
            file_name TEXT NOT NULL,
            bank_id TEXT,
            bank_name TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            account_id TEXT REFERENCES accounts(id),
            transaction_count INTEGER,
            imported_count INTEGER DEFAULT 0,
            error_message TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            processed_at TEXT
        );

        -- Plaid items connected through the proxy
        CREATE TABLE IF NOT EXISTS plaid_items (
            id TEXT PRIMARY KEY,
            proxy_item_id TEXT,
            institution_name TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active',
            last_synced_at TEXT,
            connected_at_event INTEGER REFERENCES events(id)
        );

        -- Plaid account to local account mappings
        CREATE TABLE IF NOT EXISTS plaid_local_accounts (
            item_id TEXT NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
            plaid_account_id TEXT NOT NULL,
            name TEXT NOT NULL,
            account_type TEXT NOT NULL,
            mask TEXT,
            local_account_id TEXT REFERENCES accounts(id),
            plaid_balance_cents INTEGER,
            balance_updated_at TEXT,
            PRIMARY KEY (item_id, plaid_account_id)
        );

        -- Track imported Plaid transactions for dedup
        CREATE TABLE IF NOT EXISTS plaid_imported_transactions (
            plaid_transaction_id TEXT PRIMARY KEY,
            item_id TEXT NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
            entry_id TEXT NOT NULL REFERENCES journal_entries(id)
        );

        CREATE INDEX IF NOT EXISTS idx_plaid_imported_item ON plaid_imported_transactions(item_id);

        -- Staged Plaid transactions awaiting review/import
        CREATE TABLE IF NOT EXISTS plaid_staged_transactions (
            id TEXT PRIMARY KEY,
            item_id TEXT NOT NULL REFERENCES plaid_items(id) ON DELETE CASCADE,
            plaid_transaction_id TEXT NOT NULL UNIQUE,
            plaid_account_id TEXT NOT NULL,
            local_account_id TEXT,
            amount_cents INTEGER NOT NULL,
            date TEXT NOT NULL,
            name TEXT NOT NULL,
            merchant_name TEXT,
            currency TEXT NOT NULL DEFAULT 'USD',
            staged_at TEXT NOT NULL DEFAULT (datetime('now')),
            status TEXT NOT NULL DEFAULT 'pending',
            payment_meta TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_staged_status ON plaid_staged_transactions(status);
        CREATE INDEX IF NOT EXISTS idx_staged_amount ON plaid_staged_transactions(amount_cents);
        CREATE INDEX IF NOT EXISTS idx_staged_date ON plaid_staged_transactions(date);

        -- Detected transfer candidate pairs
        CREATE TABLE IF NOT EXISTS plaid_transfer_candidates (
            id TEXT PRIMARY KEY,
            staged_txn_id_1 TEXT NOT NULL REFERENCES plaid_staged_transactions(id) ON DELETE CASCADE,
            staged_txn_id_2 TEXT NOT NULL REFERENCES plaid_staged_transactions(id) ON DELETE CASCADE,
            confidence REAL NOT NULL DEFAULT 0.0,
            status TEXT NOT NULL DEFAULT 'pending',
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_transfer_status ON plaid_transfer_candidates(status);

        -- Indexes for common queries
        CREATE INDEX IF NOT EXISTS idx_journal_entries_date ON journal_entries(date);
        -- At most one live journal entry per non-null reference (invariant audit:
        -- ingest ref-dedup). DB backstop for the in-txn check in post_entry;
        -- mirrors check_idempotent (non-null reference, not voided).
        CREATE UNIQUE INDEX IF NOT EXISTS idx_journal_entries_reference_unique
            ON journal_entries(reference) WHERE reference IS NOT NULL AND is_void = 0;
        CREATE INDEX IF NOT EXISTS idx_journal_lines_account ON journal_lines(account_id);
        CREATE INDEX IF NOT EXISTS idx_journal_lines_entry ON journal_lines(entry_id);
        CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
        CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_accounts_number ON accounts(account_number);
        CREATE INDEX IF NOT EXISTS idx_accounts_type ON accounts(account_type);

        -- At most one in-progress reconciliation per account (invariant audit:
        -- ReconciliationStarted). DB backstop for the in-txn check.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_reconciliations_one_in_progress
            ON reconciliations(account_id) WHERE status = 'in_progress';

        -- Ingest account mappings (for POS/inventory integration)
        CREATE TABLE IF NOT EXISTS ingest_account_mappings (
            key TEXT PRIMARY KEY,
            account_id TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Vendor → payable account rules (per-vendor AP matching)
        CREATE TABLE IF NOT EXISTS vendor_account_rules (
            id TEXT PRIMARY KEY,
            pattern TEXT NOT NULL,
            account_id TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Accounts Payable / Accounts Receivable
        CREATE TABLE IF NOT EXISTS bills (
            id TEXT PRIMARY KEY,
            vendor TEXT NOT NULL,
            amount INTEGER NOT NULL,
            currency TEXT NOT NULL DEFAULT 'USD',
            amount_paid INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'open',
            due_date TEXT NOT NULL,
            terms TEXT,
            memo TEXT,
            entry_id TEXT NOT NULL,
            posted_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS bill_payments (
            bill_id TEXT NOT NULL REFERENCES bills(id),
            payment_entry_id TEXT NOT NULL,
            amount_applied INTEGER NOT NULL,
            applied_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (bill_id, payment_entry_id)
        );

        CREATE TABLE IF NOT EXISTS invoices (
            id TEXT PRIMARY KEY,
            customer TEXT NOT NULL,
            amount INTEGER NOT NULL,
            currency TEXT NOT NULL DEFAULT 'USD',
            amount_paid INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'open',
            due_date TEXT NOT NULL,
            terms TEXT,
            memo TEXT,
            entry_id TEXT NOT NULL,
            posted_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS invoice_payments (
            invoice_id TEXT NOT NULL REFERENCES invoices(id),
            payment_entry_id TEXT NOT NULL,
            amount_applied INTEGER NOT NULL,
            applied_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (invoice_id, payment_entry_id)
        );

        CREATE INDEX IF NOT EXISTS idx_bills_status ON bills(status);
        CREATE INDEX IF NOT EXISTS idx_bills_due_date ON bills(due_date);
        CREATE INDEX IF NOT EXISTS idx_invoices_status ON invoices(status);
        CREATE INDEX IF NOT EXISTS idx_invoices_due_date ON invoices(due_date);

        -- Staged service events (fetched events awaiting user review)
        CREATE TABLE IF NOT EXISTS staged_service_events (
            id TEXT PRIMARY KEY,
            service_id TEXT NOT NULL,
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

        -- Event services (external apps publishing via accountir-events)
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

        -- At most one active event service per root_url. DB backstop for the
        -- in-txn check in register_service.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_event_services_active_root_url
            ON event_services(root_url) WHERE status = 'active';

        -- Which group server this ledger is a replica of (migration 017).
        -- Kept in step with the migration so a database built by `init_schema`
        -- alone is complete; see 017_group_binding.sql for why the binding lives
        -- in the ledger file rather than the machine's registry.
        CREATE TABLE IF NOT EXISTS group_binding (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            group_id TEXT NOT NULL,
            instance_url TEXT NOT NULL,
            control_plane_url TEXT NOT NULL,
            bound_at TEXT NOT NULL,
            last_server_head INTEGER NOT NULL DEFAULT 0,
            last_synced_at TEXT
        );
        "#,
    )?;

    Ok(())
}

/// Backend-level schema management (SPEC §6.1 storage abstraction).
///
/// Schema creation and migration are inherently backend-specific (DDL dialect,
/// autoincrement, index syntax). This trait lets callers initialize/migrate the
/// store without naming a raw `rusqlite::Connection`, so a Postgres backend can
/// provide its own DDL behind the same interface. The SQLite implementation
/// delegates to the free functions above.
pub trait SchemaStore {
    /// Create all tables/indexes if they don't exist (idempotent).
    fn init_schema(&mut self) -> Result<(), MigrationError>;

    /// Apply any pending versioned migrations.
    fn run_migrations(&mut self) -> Result<(), MigrationError>;
}

impl SchemaStore for crate::store::event_store::EventStore {
    fn init_schema(&mut self) -> Result<(), MigrationError> {
        init_schema(self.connection())
    }

    fn run_migrations(&mut self) -> Result<(), MigrationError> {
        run_migrations(self.connection())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    #[test]
    fn test_init_schema() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"events".to_string()));
        assert!(tables.contains(&"accounts".to_string()));
        assert!(tables.contains(&"journal_entries".to_string()));
        assert!(tables.contains(&"journal_lines".to_string()));
    }

    fn has_reference_unique_index(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index'
             AND name='idx_journal_entries_reference_unique'",
            [],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_some()
    }

    /// Insert two live entries sharing a reference; the second must violate the
    /// unique index. This is the DB-level backstop for ingest ref-dedup.
    fn assert_duplicate_reference_rejected(conn: &Connection) {
        conn.execute(
            "INSERT INTO journal_entries (id, date, reference, is_void) VALUES ('e1','2026-01-01','R',0)",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO journal_entries (id, date, reference, is_void) VALUES ('e2','2026-01-01','R',0)",
            [],
        );
        assert!(dup.is_err(), "duplicate live reference must be rejected");
        // Voiding the first frees the reference (mirrors check_idempotent).
        conn.execute("UPDATE journal_entries SET is_void = 1 WHERE id = 'e1'", [])
            .unwrap();
        conn.execute(
            "INSERT INTO journal_entries (id, date, reference, is_void) VALUES ('e3','2026-01-01','R',0)",
            [],
        )
        .expect("a voided entry frees its reference for re-use");
    }

    #[test]
    fn init_schema_has_reference_unique_index_and_enforces_it() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert!(has_reference_unique_index(&conn));
        assert_duplicate_reference_rejected(&conn);
    }

    fn events_has_column(conn: &Connection, col: &str) -> bool {
        conn.prepare("SELECT name FROM pragma_table_info('events')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|c| c == col)
    }

    #[test]
    fn init_schema_has_event_actor_identity_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert!(events_has_column(&conn, "actor_id"));
        assert!(events_has_column(&conn, "received_at"));
    }

    #[test]
    fn migration_015_adds_identity_columns_to_legacy_events_table() {
        // Simulate a pre-existing DB whose events table predates the identity
        // columns; applying migration 015 must add them and leave existing rows
        // backfilled as NULL.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL,
                hash BLOB NOT NULL,
                user_id TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                UNIQUE(hash)
            );
            INSERT INTO events (event_type, payload, hash, user_id, timestamp)
            VALUES ('x', '{}', X'00', 'u', '2026-01-01T00:00:00Z');",
        )
        .unwrap();
        assert!(!events_has_column(&conn, "actor_id"));

        // Apply the actual migration 015 SQL (the ALTER path exercised by
        // run_migrations for a DB whose events table predates the columns).
        conn.execute_batch(include_str!(
            "../../migrations/015_event_actor_identity.sql"
        ))
        .unwrap();

        assert!(events_has_column(&conn, "actor_id"));
        assert!(events_has_column(&conn, "received_at"));
        // Existing row backfilled as NULL.
        let (a, r): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT actor_id, received_at FROM events LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((a, r), (None, None));
    }

    #[test]
    fn init_schema_then_run_migrations_is_idempotent_with_015() {
        // Production flow: init_schema (creates events WITH the columns) then
        // run_migrations (whose 015 ALTER would hit "duplicate column"). The
        // migration runner tolerates that, so the full path must not error.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        run_migrations(&conn).unwrap();
        assert!(events_has_column(&conn, "actor_id"));
        assert!(events_has_column(&conn, "received_at"));
    }

    #[test]
    fn run_migrations_adds_reference_unique_index() {
        // The production path is init_schema THEN run_migrations; the migration
        // must also (idempotently) leave the index in place.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        run_migrations(&conn).unwrap();
        assert!(has_reference_unique_index(&conn));
    }

    fn has_event_service_url_index(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index'
             AND name='idx_event_services_active_root_url'",
            [],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_some()
    }

    #[test]
    fn init_schema_has_event_service_url_index_and_enforces_it() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert!(has_event_service_url_index(&conn));
        // Two active services can't share a root_url; a disconnected one frees it.
        conn.execute(
            "INSERT INTO event_services (id, name, root_url, api_key, status)
             VALUES ('s1','A','https://x','k','active')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO event_services (id, name, root_url, api_key, status)
             VALUES ('s2','B','https://x','k','active')",
            [],
        );
        assert!(dup.is_err(), "duplicate active root_url must be rejected");
        conn.execute(
            "UPDATE event_services SET status = 'disconnected' WHERE id = 's1'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO event_services (id, name, root_url, api_key, status)
             VALUES ('s3','C','https://x','k','active')",
            [],
        )
        .expect("a disconnected service frees its root_url");
    }

    #[test]
    fn run_migrations_adds_event_service_url_index() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        run_migrations(&conn).unwrap();
        assert!(has_event_service_url_index(&conn));
    }
}
#[cfg(test)]
mod plaid_item_rebuild {
    use super::*;
    use rusqlite::Connection;

    /// Migration 018 rebuilds `plaid_items`, and three tables reference it with
    /// ON DELETE CASCADE.
    ///
    /// The regression: the first draft dropped the parent with foreign keys
    /// enforced, so the DROP cascaded and deleted every account mapping, every
    /// dedup record and every staged transaction for that connection. It passed
    /// the whole suite, because the fixtures had no child rows — it was caught
    /// only by running it against a real ledger, where three mappings vanished.
    ///
    /// So this test's fixture is specifically a connection WITH children.
    #[test]
    fn rebuilding_plaid_items_does_not_cascade_away_its_children() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        init_schema(&conn).unwrap();
        run_migrations(&conn).unwrap();

        conn.execute_batch(
            "INSERT INTO plaid_items (id, proxy_item_id, institution_name)
                 VALUES ('item-1', 'proxy-1', 'Chase');
             INSERT INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type)
                 VALUES ('item-1', 'acc-1', 'Checking', 'depository');
             INSERT INTO plaid_local_accounts (item_id, plaid_account_id, name, account_type)
                 VALUES ('item-1', 'acc-2', 'Savings', 'depository');
             INSERT INTO plaid_staged_transactions
                 (id, item_id, plaid_transaction_id, plaid_account_id, amount_cents, date, name)
                 VALUES ('s-1', 'item-1', 'txn-1', 'acc-1', 100, '2026-08-01', 'Coffee');",
        )
        .unwrap();

        // Re-run the rebuild as an upgrade would: forget it was applied, then
        // migrate again. This is the exact operation that ate the data.
        conn.execute("DELETE FROM schema_migrations WHERE version = 18", [])
            .unwrap();
        run_migrations(&conn).unwrap();

        let mappings: i64 = conn
            .query_row("SELECT COUNT(*) FROM plaid_local_accounts", [], |r| {
                r.get(0)
            })
            .unwrap();
        let staged: i64 = conn
            .query_row("SELECT COUNT(*) FROM plaid_staged_transactions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            mappings, 2,
            "the rebuild cascaded away the account mappings — a user would find \
             every bank account silently unlinked from its ledger account"
        );
        assert_eq!(staged, 1, "the rebuild cascaded away staged transactions");

        // …and the point of the whole migration.
        let notnull: i64 = conn
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('plaid_items') WHERE name='proxy_item_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(notnull, 0, "proxy_item_id must be nullable after 018");

        // A hosted connection — the case that motivated it — must now insert.
        conn.execute(
            "INSERT INTO plaid_items (id, proxy_item_id, institution_name)
                 VALUES ('item-2', NULL, 'Hosted Bank')",
            [],
        )
        .expect("a connection recorded on hosted books has no proxy handle");
    }
}
