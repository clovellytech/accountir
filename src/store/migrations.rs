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
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY,
            event_type TEXT NOT NULL,
            payload TEXT NOT NULL,
            hash BLOB NOT NULL,
            user_id TEXT NOT NULL,
            timestamp TEXT NOT NULL,
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
            proxy_item_id TEXT NOT NULL,
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
        CREATE INDEX IF NOT EXISTS idx_journal_lines_account ON journal_lines(account_id);
        CREATE INDEX IF NOT EXISTS idx_journal_lines_entry ON journal_lines(entry_id);
        CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
        CREATE INDEX IF NOT EXISTS idx_events_timestamp ON events(timestamp);
        CREATE INDEX IF NOT EXISTS idx_accounts_number ON accounts(account_number);
        CREATE INDEX IF NOT EXISTS idx_accounts_type ON accounts(account_type);
        "#,
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
