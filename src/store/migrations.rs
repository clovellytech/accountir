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

    // WHICH versions have been applied — not how high they go.
    //
    // This used to gate on `MAX(version)`, and that is a silent data loss as soon as
    // two branches number migrations independently. Concretely: `main` carried 047,
    // 048 and 050 while `feature-personal-tax` carried 049. A database migrated on
    // `main` is stamped 50, so when 049 arrived in the merge the old gate said
    // "50 > 49, nothing to do" — and the documents and tax-statement tables were
    // never created, with no error, on a database that looked fully migrated.
    //
    // A set closes that: a version is applied because it is recorded as applied. A
    // database with no rows at all (one built by `init_schema`, or migrated by a
    // runner that predates this table) is treated exactly as before — every version
    // is attempted, and the "already exists" tolerance below absorbs the ones whose
    // schema is already there.
    let applied: std::collections::HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        rows.filter_map(Result::ok).collect()
    };

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
        (
            19,
            include_str!("../../migrations/019_recurring_transfers.sql"),
        ),
        (
            20,
            include_str!("../../migrations/020_event_service_optional_api_key.sql"),
        ),
        (
            21,
            include_str!("../../migrations/021_plaid_persistent_account_id.sql"),
        ),
        (
            22,
            include_str!("../../migrations/022_event_service_reporting.sql"),
        ),
        (23, include_str!("../../migrations/023_partnership.sql")),
        (
            24,
            include_str!("../../migrations/024_tax_line_mappings.sql"),
        ),
        (
            25,
            include_str!("../../migrations/025_config_tables_have_no_projection_fk.sql"),
        ),
        (
            26,
            include_str!("../../migrations/026_schedule_b_answers.sql"),
        ),
        (
            27,
            include_str!("../../migrations/027_tax_setup_is_event_sourced.sql"),
        ),
        (
            28,
            include_str!("../../migrations/028_partner_relationships.sql"),
        ),
        (29, include_str!("../../migrations/029_il1065_settings.sql")),
        (
            30,
            include_str!("../../migrations/030_depreciable_assets.sql"),
        ),
        (31, include_str!("../../migrations/031_schedule_c.sql")),
        (
            32,
            include_str!("../../migrations/032_journal_entry_annotations.sql"),
        ),
        (
            34,
            include_str!("../../migrations/034_stripe_payouts_clear.sql"),
        ),
        (
            35,
            include_str!("../../migrations/035_deduction_limits.sql"),
        ),
        (
            36,
            include_str!("../../migrations/036_partner_share_periods.sql"),
        ),
        (
            37,
            include_str!("../../migrations/037_partner_equity_accounts.sql"),
        ),
        (
            38,
            include_str!("../../migrations/038_dated_tax_line_assignments.sql"),
        ),
        (
            39,
            include_str!("../../migrations/039_drop_fiscal_periods.sql"),
        ),
        (
            40,
            include_str!("../../migrations/040_statement_groups.sql"),
        ),
        (
            41,
            include_str!("../../migrations/041_depreciation_overrides.sql"),
        ),
        (
            42,
            include_str!("../../migrations/042_partner_fixed_allocations.sql"),
        ),
        (
            43,
            include_str!("../../migrations/043_depreciation_basis_adjustments.sql"),
        ),
        (
            44,
            include_str!("../../migrations/044_preferred_allocations.sql"),
        ),
        (
            45,
            include_str!("../../migrations/045_liability_classifications.sql"),
        ),
        (
            46,
            include_str!("../../migrations/046_illinois_tax_addbacks.sql"),
        ),
        (47, include_str!("../../migrations/047_investments.sql")),
        (
            48,
            include_str!("../../migrations/048_retirement_accounts.sql"),
        ),
        // 49 arrived after 50 and 51, which the runner now handles: it applies every
        // version it has no record of rather than everything above `MAX(version)`, so a
        // database already stamped at 51 picks this one up on its next open. The
        // personal-tax work reserved this number for its documents and statements; only
        // the documents half is here, because schema nothing reads is schema nobody
        // maintains. See the file.
        (49, include_str!("../../migrations/049_documents.sql")),
        (
            50,
            include_str!("../../migrations/050_investment_imports.sql"),
        ),
        (
            51,
            include_str!("../../migrations/051_investment_subaccounts_and_review.sql"),
        ),
        (52, include_str!("../../migrations/052_portfolio.sql")),
    ];

    for (version, sql) in migrations {
        if !applied.contains(&version) {
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
            -- Stable across re-links where the institution provides it; see
            -- migration 021. `plaid_account_id` is not.
            persistent_account_id TEXT,
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
            -- Nullable since migration 020: a service registered on group-hosted
            -- books keeps its key on the group's instance, never in this log.
            api_key TEXT,
            status TEXT NOT NULL DEFAULT 'active',
            cursor TEXT,
            last_synced_at TEXT,
            events_processed INTEGER DEFAULT 0,
            entries_created INTEGER DEFAULT 0,
            connected_at_event INTEGER REFERENCES events(id),
            -- See migration 022: a ledger fact, not a preference, because two
            -- members aggregating differently would post the same sales twice.
            reporting_frequency TEXT NOT NULL DEFAULT 'per_event',
            reporting_from TEXT
        );

        -- At most one active event service per root_url. DB backstop for the
        -- in-txn check in register_service.
        CREATE UNIQUE INDEX IF NOT EXISTS idx_event_services_active_root_url
            ON event_services(root_url) WHERE status = 'active';

        -- The partnership header and its partners (migration 023). Kept in step
        -- with the migration so a database built by `init_schema` alone is
        -- complete; see 023_partnership.sql for why TINs are not in the log.
        CREATE TABLE IF NOT EXISTS business_profile (
            id TEXT PRIMARY KEY CHECK (id = 'default'),
            legal_name TEXT NOT NULL,
            street TEXT NOT NULL,
            suite TEXT,
            city TEXT NOT NULL,
            state TEXT NOT NULL,
            postal_code TEXT NOT NULL,
            country TEXT,
            ein TEXT NOT NULL,
            naics_code TEXT NOT NULL,
            formation_date TEXT NOT NULL,
            principal_activity TEXT,
            principal_product TEXT,
            -- 'partnership' or 'sole_proprietorship' (migration 031): which
            -- return these books file. Nothing in the accounts can tell.
            business_type TEXT NOT NULL DEFAULT 'partnership',
            updated_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS partners (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            partner_type TEXT NOT NULL,
            residency TEXT NOT NULL,
            entity_type TEXT NOT NULL,
            street TEXT NOT NULL,
            suite TEXT,
            city TEXT NOT NULL,
            state TEXT NOT NULL,
            postal_code TEXT NOT NULL,
            country TEXT,
            start_date TEXT NOT NULL,
            end_date TEXT,
            profit_ppm INTEGER NOT NULL,
            loss_ppm INTEGER NOT NULL,
            capital_ppm INTEGER NOT NULL,
            admitted_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id)
        );

        CREATE INDEX IF NOT EXISTS idx_partners_start ON partners(start_date);

        -- Family ties between partners, for Schedule B-1's §267(c) constructive
        -- ownership test (migration 028). Event-sourced like `partners`, not local
        -- like `partner_tins` — which two partners are married is a fact the whole
        -- partnership files against, not a secret. Pair is the key; for symmetric
        -- kinds the ids are stored canonically so a tie is one row. No foreign key
        -- to `partners`, for the reason migration 025 gives.
        CREATE TABLE IF NOT EXISTS partner_relationships (
            partner_id TEXT NOT NULL,
            related_partner_id TEXT NOT NULL,
            relationship TEXT NOT NULL,
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (partner_id, related_partner_id)
        );

        -- Illinois IL-1065 settings (migration 029). One row keyed 'default', like
        -- business_profile — the standing apportionment and PTE-election choices
        -- that decide which path tax::il1065 fills. Event-sourced, not local.
        CREATE TABLE IF NOT EXISTS il1065_settings (
            id TEXT PRIMARY KEY CHECK (id = 'default'),
            apportions_outside_illinois INTEGER NOT NULL DEFAULT 0,
            elects_pte_tax INTEGER NOT NULL DEFAULT 0,
            updated_at_event INTEGER REFERENCES events(id)
        );

        -- Notes added to an entry after it was posted (migration 032). The
        -- imported memo is what the bank said and stays untouched; this is what
        -- a person says it was for. Every note is kept, keyed by the event that
        -- made it, so a replay is idempotent.
        CREATE TABLE IF NOT EXISTS journal_entry_annotations (
            event_id INTEGER PRIMARY KEY REFERENCES events(id),
            entry_id TEXT NOT NULL,
            annotation TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_journal_entry_annotations_entry
            ON journal_entry_annotations(entry_id, event_id);

        -- Sole proprietorships (migration 031): which return these books file,
        -- and who files it. `business_type` lives on the profile row because it
        -- is one fact about the one business these books describe.
        CREATE TABLE IF NOT EXISTS sole_proprietor (
            id TEXT PRIMARY KEY CHECK (id = 'default'),
            name TEXT NOT NULL,
            accounting_method TEXT NOT NULL DEFAULT 'cash',
            accounting_method_other TEXT,
            updated_at_event INTEGER REFERENCES events(id)
        );

        -- The proprietor's SSN, local like `partner_tins` and never in the log.
        CREATE TABLE IF NOT EXISTS sole_proprietor_tin (
            id TEXT PRIMARY KEY CHECK (id = 'default'),
            ssn TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS schedule_c_answers (
            tax_year INTEGER NOT NULL,
            answer_key TEXT NOT NULL,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (tax_year, answer_key)
        );

        -- The asset register (migration 030): what depreciates, and the facts
        -- MACRS needs. Event-sourced. `property_class` holds a class name and
        -- never a number of years — land improvements and qualified improvement
        -- property are both 15-year property with different methods, so a life
        -- alone cannot say how to depreciate anything.
        CREATE TABLE IF NOT EXISTS depreciable_assets (
            id TEXT PRIMARY KEY,
            description TEXT NOT NULL,
            asset_account_id TEXT NOT NULL,
            expense_account_id TEXT NOT NULL,
            accumulated_account_id TEXT NOT NULL,
            -- Where §179 is expensed: a different line of the return from ordinary
            -- depreciation (Schedule K line 12, not page 1 line 16a), so a
            -- different account. NULL when no election is made.
            section_179_account_id TEXT,
            acquired_on TEXT NOT NULL,
            placed_in_service TEXT NOT NULL,
            cost_cents INTEGER NOT NULL,
            property_class TEXT NOT NULL,
            system TEXT NOT NULL DEFAULT 'gds',
            section_179_cents INTEGER NOT NULL DEFAULT 0,
            bonus TEXT NOT NULL DEFAULT 'take',
            disposed_on TEXT,
            notes TEXT,
            added_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id)
        );

        CREATE INDEX IF NOT EXISTS idx_depreciable_assets_placed
            ON depreciable_assets(placed_in_service);

        -- A year's depreciation on one asset fixed by hand, with the reason
        -- (migration 041).
        CREATE TABLE IF NOT EXISTS depreciation_overrides (
            asset_id TEXT NOT NULL,
            tax_year INTEGER NOT NULL,
            amount_cents INTEGER NOT NULL,
            note TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (asset_id, tax_year)
        );

        -- Changes to an asset's basis after purchase (migration 043).
        CREATE TABLE IF NOT EXISTS depreciation_basis_adjustments (
            adjustment_id TEXT PRIMARY KEY,
            asset_id TEXT NOT NULL,
            effective_year INTEGER NOT NULL,
            amount_cents INTEGER NOT NULL,
            note TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id)
        );

        -- Which Form 1065 line each account is reported on (migration 024).
        -- Keyed by account because many accounts share one line, and because
        -- "which accounts have no line" is the question that catches money
        -- going missing from a return.
        -- No foreign key to `accounts`, deliberately — see migration 025.
        -- Event-sourced since migration 027 — `updated_at_event` names the event
        -- that put the row here, exactly as `business_profile` does.
        -- Dated since migration 038: the assignment in force for a year is the
        -- row with the greatest `effective_from` at or before it, and `0` means
        -- "as far back as these books go" — written before assignments were
        -- dated.
        CREATE TABLE IF NOT EXISTS tax_line_mappings (
            account_id TEXT NOT NULL,
            effective_from INTEGER NOT NULL DEFAULT 0,
            line_key TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (account_id, effective_from)
        );

        CREATE INDEX IF NOT EXISTS idx_tax_line_mappings_line ON tax_line_mappings(line_key);

        -- How much of an account's balance the law lets you deduct (migration
        -- 035). Sparse: an account with no row is fully deductible, which is
        -- almost all of them.
        -- A partner's percentages, and from when (migration 036).
        CREATE TABLE IF NOT EXISTS partner_share_periods (
            partner_id      TEXT NOT NULL,
            effective_from  TEXT NOT NULL,
            profit_ppm      INTEGER NOT NULL,
            loss_ppm        INTEGER NOT NULL,
            capital_ppm     INTEGER NOT NULL,
            updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (partner_id, effective_from)
        );

        -- Which ledger accounts hold a partner's capital (migration 037).
        CREATE TABLE IF NOT EXISTS partner_equity_accounts (
            partner_id  TEXT NOT NULL,
            account_id  TEXT NOT NULL,
            role        TEXT NOT NULL,
            updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (partner_id, account_id)
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_partner_equity_accounts_account
            ON partner_equity_accounts(account_id);

        -- A partner's share of a year fixed in dollars (migration 042).
        CREATE TABLE IF NOT EXISTS partner_fixed_allocations (
            tax_year INTEGER NOT NULL,
            partner_id TEXT NOT NULL,
            amount_cents INTEGER,
            note TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            preferred INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (tax_year, partner_id)
        );

        -- How a liability account bears on K-1 item K (migration 045).
        CREATE TABLE IF NOT EXISTS liability_classifications (
            account_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            partner_id TEXT,
            guaranteed INTEGER NOT NULL DEFAULT 0,
            note TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id)
        );

        -- Dated since migration 038, like `tax_line_mappings`.
        CREATE TABLE IF NOT EXISTS tax_deduction_limits (
            account_id TEXT NOT NULL,
            effective_from INTEGER NOT NULL DEFAULT 0,
            deductible_pct INTEGER NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (account_id, effective_from)
        );

        -- Which parent accounts print as one row on the attached statements
        -- (migration 040). Dated like `tax_deduction_limits`, and a stored no
        -- is a row rather than an absence — see the migration.
        CREATE TABLE IF NOT EXISTS tax_statement_groups (
            account_id TEXT NOT NULL,
            effective_from INTEGER NOT NULL,
            grouped INTEGER NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (account_id, effective_from)
        );

        -- Accounts holding Illinois income or replacement tax, added back on
        -- IL-1065 line 16 (migration 046). Dated, with a stored no.
        CREATE TABLE IF NOT EXISTS il_tax_addbacks (
            account_id TEXT NOT NULL,
            effective_from INTEGER NOT NULL,
            added_back INTEGER NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (account_id, effective_from)
        );

        -- The taxable-brokerage register (migration 047). Kept in step with the
        -- migration so a database built by `init_schema` alone is complete; see
        -- 047_investments.sql for why cost and not value, why a total and not a
        -- unit price, and why quantity is in millionths of a share.
        CREATE TABLE IF NOT EXISTS securities (
            id TEXT PRIMARY KEY,
            ticker TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            cusip TEXT,
            currency TEXT NOT NULL DEFAULT 'USD',
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id)
        );

        CREATE TABLE IF NOT EXISTS investment_lots (
            id TEXT PRIMARY KEY,
            security_id TEXT NOT NULL,
            securities_account_id TEXT NOT NULL,
            cash_account_id TEXT NOT NULL,
            quantity INTEGER NOT NULL,
            total_cost_cents INTEGER NOT NULL,
            remaining_quantity INTEGER NOT NULL,
            remaining_basis_cents INTEGER NOT NULL,
            trade_date TEXT NOT NULL,
            added_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id)
        );
        CREATE INDEX IF NOT EXISTS idx_investment_lots_holding
            ON investment_lots(security_id, securities_account_id, trade_date);

        CREATE TABLE IF NOT EXISTS investment_sales (
            id TEXT PRIMARY KEY,
            security_id TEXT NOT NULL,
            securities_account_id TEXT NOT NULL,
            cash_account_id TEXT NOT NULL,
            quantity INTEGER NOT NULL,
            proceeds_cents INTEGER NOT NULL,
            fee_cents INTEGER NOT NULL,
            basis_cents INTEGER NOT NULL,
            realized_gain_cents INTEGER NOT NULL,
            trade_date TEXT NOT NULL,
            recorded_at_event INTEGER REFERENCES events(id)
        );
        CREATE INDEX IF NOT EXISTS idx_investment_sales_security
            ON investment_sales(security_id, trade_date);

        CREATE TABLE IF NOT EXISTS investment_sale_lots (
            sale_id TEXT NOT NULL,
            lot_id TEXT NOT NULL,
            quantity INTEGER NOT NULL,
            basis_cents INTEGER NOT NULL,
            term TEXT NOT NULL,
            PRIMARY KEY (sale_id, lot_id)
        );

        -- The sheltered-account register (migration 048). Kept in step with the
        -- migration for the same reason the brokerage tables above are; see
        -- 048_retirement_accounts.sql for why there are no lots in here, why
        -- `kind` is closed where `securities.kind` is not, and why the last value
        -- is nullable rather than zero.
        CREATE TABLE IF NOT EXISTS retirement_accounts (
            account_id TEXT PRIMARY KEY,
            institution TEXT NOT NULL,
            kind TEXT NOT NULL,
            value_change_account_id TEXT NOT NULL,
            last_value_cents INTEGER,
            last_value_as_of TEXT,
            registered_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id)
        );
        CREATE INDEX IF NOT EXISTS idx_retirement_accounts_value_change
            ON retirement_accounts(value_change_account_id);

        -- The investments importer's registers (migration 050). Kept in step with
        -- the migration for the reason the two blocks above are; see
        -- 050_investment_imports.sql for which of these are projections of events
        -- and which are machine-local, and why the split falls where it does.
        CREATE TABLE IF NOT EXISTS investment_account_config (
            item_id TEXT NOT NULL,
            plaid_account_id TEXT NOT NULL,
            treatment TEXT NOT NULL,
            plaid_subtype TEXT,
            subtype_recognised INTEGER NOT NULL DEFAULT 1,
            -- The stocks slot, under the name every configuration before
            -- migration 051 wrote it under; see that migration for why it is not
            -- renamed.
            securities_account_id TEXT,
            mutual_funds_account_id TEXT,
            other_securities_account_id TEXT,
            cash_account_id TEXT,
            dividend_income_account_id TEXT,
            interest_income_account_id TEXT,
            tax_exempt_interest_account_id TEXT,
            capital_gain_distribution_account_id TEXT,
            realized_gain_account_id TEXT,
            fee_expense_account_id TEXT,
            transfer_clearing_account_id TEXT,
            retirement_account_id TEXT,
            configured_at_event INTEGER REFERENCES events(id),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (item_id, plaid_account_id),
            CHECK (treatment IN ('taxable', 'sheltered')),
            CHECK (
                (treatment = 'taxable'
                 AND securities_account_id IS NOT NULL
                 AND cash_account_id IS NOT NULL
                 AND dividend_income_account_id IS NOT NULL
                 AND interest_income_account_id IS NOT NULL
                 AND realized_gain_account_id IS NOT NULL
                 AND fee_expense_account_id IS NOT NULL
                 AND retirement_account_id IS NULL)
                OR
                (treatment = 'sheltered'
                 AND retirement_account_id IS NOT NULL
                 AND securities_account_id IS NULL
                 AND cash_account_id IS NULL)
            )
        );

        CREATE TABLE IF NOT EXISTS plaid_securities (
            plaid_security_id TEXT PRIMARY KEY,
            security_id TEXT NOT NULL,
            linked_at_event INTEGER REFERENCES events(id)
        );
        CREATE INDEX IF NOT EXISTS idx_plaid_securities_security
            ON plaid_securities(security_id);

        CREATE TABLE IF NOT EXISTS investment_imports (
            provider_transaction_id TEXT PRIMARY KEY,
            item_id TEXT NOT NULL,
            plaid_account_id TEXT NOT NULL,
            outcome TEXT NOT NULL,
            entry_id TEXT NOT NULL,
            lot_id TEXT,
            sale_id TEXT,
            imported_at TEXT NOT NULL DEFAULT (datetime('now')),
            imported_at_event INTEGER REFERENCES events(id)
        );
        CREATE INDEX IF NOT EXISTS idx_investment_imports_account
            ON investment_imports(item_id, plaid_account_id);

        CREATE TABLE IF NOT EXISTS investment_holdings_snapshots (
            snapshot_id TEXT PRIMARY KEY,
            item_id TEXT NOT NULL,
            plaid_account_id TEXT NOT NULL,
            as_of TEXT NOT NULL,
            recorded_at_event INTEGER REFERENCES events(id),
            UNIQUE (item_id, plaid_account_id, as_of)
        );

        CREATE TABLE IF NOT EXISTS investment_holdings_snapshot_lines (
            snapshot_id TEXT NOT NULL REFERENCES investment_holdings_snapshots(snapshot_id)
                ON DELETE CASCADE,
            plaid_security_id TEXT NOT NULL,
            security_id TEXT,
            ticker TEXT,
            quantity INTEGER NOT NULL,
            cost_basis_cents INTEGER,
            value_cents INTEGER,
            currency TEXT,
            PRIMARY KEY (snapshot_id, plaid_security_id)
        );

        -- Machine-local: what this machine is still reviewing, and how far it has
        -- fetched. Neither is derived from the log, and neither is truncated by a
        -- rebuild.
        CREATE TABLE IF NOT EXISTS investment_staged_activity (
            id TEXT PRIMARY KEY,
            item_id TEXT NOT NULL,
            plaid_account_id TEXT NOT NULL,
            provider_transaction_id TEXT NOT NULL UNIQUE,
            reason TEXT NOT NULL,
            detail TEXT NOT NULL,
            provider_type TEXT NOT NULL,
            provider_subtype TEXT NOT NULL,
            date TEXT NOT NULL,
            name TEXT NOT NULL,
            amount_cents INTEGER,
            raw_payload TEXT NOT NULL,
            staged_at TEXT NOT NULL DEFAULT (datetime('now')),
            -- 'pending', 'resolved' or 'dismissed' (migration 051). A dismissal
            -- is its own status rather than a kind of resolution, because
            -- "nothing is missing from these books" is the question this list
            -- answers and the two answers are opposite.
            status TEXT NOT NULL DEFAULT 'pending',
            resolution TEXT,
            resolution_note TEXT,
            resolution_entry_id TEXT,
            resolved_at TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_investment_staged_status
            ON investment_staged_activity(status, date);

        -- Files attached to the books (migration 049). Kept in step with that file
        -- for the reason the blocks above are; see it for why the bytes are not here
        -- and why the subject is two columns.
        CREATE TABLE IF NOT EXISTS documents (
            document_id       TEXT PRIMARY KEY,
            sha256            TEXT NOT NULL,
            size_bytes        INTEGER NOT NULL,
            media_type        TEXT NOT NULL,
            filename          TEXT NOT NULL,
            title             TEXT,
            tax_year          INTEGER,
            form              TEXT,
            subject_kind      TEXT,
            subject_id        TEXT,
            attached_at       TEXT NOT NULL,
            attached_at_event INTEGER REFERENCES events(id),
            CHECK ((subject_kind IS NULL) = (subject_id IS NULL))
        );
        CREATE INDEX IF NOT EXISTS idx_documents_subject
            ON documents(subject_kind, subject_id);
        CREATE INDEX IF NOT EXISTS idx_documents_tax_year ON documents(tax_year);

        CREATE TABLE IF NOT EXISTS investment_fetch_state (
            item_id TEXT NOT NULL,
            plaid_account_id TEXT NOT NULL,
            last_fetched_through TEXT NOT NULL,
            last_fetched_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (item_id, plaid_account_id)
        );

        -- The portfolio (migration 052): market value by day, machine-local and
        -- never posted. Kept in step with the migration; see it for why none of
        -- this is in the log.
        CREATE TABLE IF NOT EXISTS portfolio_snapshots (
            snapshot_id TEXT PRIMARY KEY,
            item_id TEXT NOT NULL,
            plaid_account_id TEXT NOT NULL,
            as_of TEXT NOT NULL,
            fetched_at TEXT NOT NULL,
            account_name TEXT NOT NULL,
            account_subtype TEXT,
            mask TEXT,
            balance_cents INTEGER,
            currency TEXT,
            UNIQUE (item_id, plaid_account_id, as_of)
        );
        CREATE TABLE IF NOT EXISTS portfolio_holdings (
            snapshot_id TEXT NOT NULL REFERENCES portfolio_snapshots(snapshot_id)
                ON DELETE CASCADE,
            plaid_security_id TEXT NOT NULL,
            quantity INTEGER NOT NULL,
            price_micros INTEGER,
            price_as_of TEXT,
            value_cents INTEGER,
            cost_basis_cents INTEGER,
            currency TEXT,
            PRIMARY KEY (snapshot_id, plaid_security_id)
        );
        CREATE TABLE IF NOT EXISTS portfolio_securities (
            plaid_security_id TEXT PRIMARY KEY,
            name TEXT,
            ticker TEXT,
            security_type TEXT,
            is_cash_equivalent INTEGER NOT NULL DEFAULT 0,
            close_price_micros INTEGER,
            close_price_as_of TEXT,
            currency TEXT,
            updated_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_portfolio_snapshots_account
            ON portfolio_snapshots(item_id, plaid_account_id, as_of);

        -- Local only, never replicated — see migration 023.
        -- No foreign key to `partners`, deliberately — see migration 025. This
        -- config outlives the projection it points at, and `rebuild` truncates
        -- that projection.
        CREATE TABLE IF NOT EXISTS partner_tins (
            partner_id TEXT PRIMARY KEY,
            tin TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        -- Schedule B, "Other Information" (migration 026). Keyed by tax year
        -- as well as question, because the schedule asks about "the tax year"
        -- and a carried-over answer is last year's fact on this year's signed
        -- return. Local config, like the two tables above, and with no foreign
        -- key for the same reason.
        CREATE TABLE IF NOT EXISTS schedule_b_answers (
            tax_year INTEGER NOT NULL,
            answer_key TEXT NOT NULL,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at_event INTEGER REFERENCES events(id),
            PRIMARY KEY (tax_year, answer_key)
        );

        -- Rows that predate migration 027, held until the store adopts them into
        -- the log. Empty on a database built by `init_schema`, which has no
        -- pre-event rows to rescue.
        CREATE TABLE IF NOT EXISTS tax_line_mappings_pending_adoption (
            account_id TEXT PRIMARY KEY,
            line_key TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS schedule_b_answers_pending_adoption (
            tax_year INTEGER NOT NULL,
            answer_key TEXT NOT NULL,
            value TEXT NOT NULL,
            PRIMARY KEY (tax_year, answer_key)
        );

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

    const INVESTMENT_TABLES: [&str; 4] = [
        "securities",
        "investment_lots",
        "investment_sales",
        "investment_sale_lots",
    ];

    fn has_table(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1",
            [name],
            |_| Ok(()),
        )
        .optional()
        .unwrap()
        .is_some()
    }

    /// A database built from scratch has the brokerage register (migration 047),
    /// because `init_schema` carries the same DDL the migration does — the two
    /// drifting apart is how a fresh ledger ends up missing a table the code
    /// writes to.
    #[test]
    fn init_schema_has_the_investment_register() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        for table in INVESTMENT_TABLES {
            assert!(has_table(&conn, table), "{table} is missing");
        }
        // One ticker is one security, enforced by the schema and not only by the
        // command that checks it.
        conn.execute(
            "INSERT INTO securities (id, ticker, name, kind) VALUES ('s1','ACME','Acme','stock')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO securities (id, ticker, name, kind) VALUES ('s2','ACME','Acme 2','etf')",
            [],
        );
        assert!(
            dup.is_err(),
            "a second master for one ticker must be refused"
        );
    }

    /// And a database that predates it gets it too. Simulated the way the others
    /// here are: the pre-047 shape, stamped at version 46, then migrated.
    #[test]
    fn migration_047_adds_the_investment_register_to_an_existing_database() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute_batch(
            "DROP TABLE investment_sale_lots;
             DROP TABLE investment_sales;
             DROP TABLE investment_lots;
             DROP TABLE securities;
             CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             INSERT INTO schema_migrations (version) VALUES (46);",
        )
        .unwrap();
        for table in INVESTMENT_TABLES {
            assert!(!has_table(&conn, table), "the fixture still has {table}");
        }

        run_migrations(&conn).unwrap();
        for table in INVESTMENT_TABLES {
            assert!(has_table(&conn, table), "{table} was not created");
        }
        // The lot register carries what a part-sold lot has left, which is the
        // column the exact-basis allocation depends on.
        conn.execute(
            "INSERT INTO investment_lots
               (id, security_id, securities_account_id, cash_account_id, quantity,
                total_cost_cents, remaining_quantity, remaining_basis_cents, trade_date)
             VALUES ('l1','s1','1102','1101',3000000,1000,2000000,667,'2025-02-02')",
            [],
        )
        .expect("the register accepts a part-sold lot");

        // And running it again changes nothing — the production path is
        // init_schema then run_migrations, repeatedly.
        run_migrations(&conn).unwrap();
        let lots: i64 = conn
            .query_row("SELECT COUNT(*) FROM investment_lots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lots, 1, "a re-run must not recreate the table empty");
    }

    /// A database built from scratch has the sheltered-account register
    /// (migration 048), for the reason the 047 test above gives: `init_schema` and
    /// the migration drifting apart is how a fresh ledger ends up missing a table
    /// the code writes to.
    #[test]
    fn init_schema_has_the_retirement_register() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert!(has_table(&conn, "retirement_accounts"));

        // One ledger account is one retirement account, enforced by the schema
        // and not only by the command that checks it: two rows would mean two
        // kinds, and a distribution would be taxable or not depending on which
        // was read.
        conn.execute(
            "INSERT INTO retirement_accounts
               (account_id, institution, kind, value_change_account_id)
             VALUES ('1500','Fidelity ••5678','traditional','4130')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO retirement_accounts
               (account_id, institution, kind, value_change_account_id)
             VALUES ('1500','Fidelity ••5678','roth','4130')",
            [],
        );
        assert!(dup.is_err(), "a second row for one account must be refused");

        // Sharing one value-change account is normal, not an error: spec §2b's
        // chart has one for the whole book.
        conn.execute(
            "INSERT INTO retirement_accounts
               (account_id, institution, kind, value_change_account_id)
             VALUES ('1510','Vanguard ••9012','roth','4130')",
            [],
        )
        .expect("two sheltered accounts may share one value-change account");

        // And a freshly registered account has no last value, which is distinct
        // from a last value of zero.
        let unset: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM retirement_accounts
                  WHERE last_value_cents IS NULL AND last_value_as_of IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unset, 2);
    }

    /// And a database that predates it gets it too, by the route the others here
    /// use: the pre-048 shape, stamped at version 47, then migrated.
    #[test]
    fn migration_048_adds_the_retirement_register_to_an_existing_database() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute_batch(
            "DROP TABLE retirement_accounts;
             CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             INSERT INTO schema_migrations (version) VALUES (47);",
        )
        .unwrap();
        assert!(
            !has_table(&conn, "retirement_accounts"),
            "the fixture still has the table"
        );

        run_migrations(&conn).unwrap();
        assert!(has_table(&conn, "retirement_accounts"));
        conn.execute(
            "INSERT INTO retirement_accounts
               (account_id, institution, kind, value_change_account_id,
                last_value_cents, last_value_as_of)
             VALUES ('1500','Fidelity ••5678','traditional','4130',10000000,'2026-01-31')",
            [],
        )
        .expect("the register accepts an account with a statement value");

        // And running it again changes nothing — the production path is
        // init_schema then run_migrations, repeatedly.
        run_migrations(&conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM retirement_accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "a re-run must not recreate the table empty");
    }

    /// The importer's registers (migration 050). Four projections and two
    /// machine-local tables; see `050_investment_imports.sql` for which is which and
    /// why.
    const IMPORT_TABLES: [&str; 6] = [
        "investment_account_config",
        "plaid_securities",
        "investment_imports",
        "investment_holdings_snapshots",
        "investment_holdings_snapshot_lines",
        "investment_staged_activity",
    ];

    #[test]
    fn init_schema_has_the_investment_import_registers() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        for table in IMPORT_TABLES {
            assert!(has_table(&conn, table), "{table} is missing");
        }
        assert!(has_table(&conn, "investment_fetch_state"));

        // A taxable configuration is all six accounts or it is not a configuration:
        // one with no dividend account would import a dividend into nowhere, and the
        // CHECK constraint is what stops a row like that existing at all.
        let half = conn.execute(
            "INSERT INTO investment_account_config
               (item_id, plaid_account_id, treatment, securities_account_id, cash_account_id)
             VALUES ('i1','pa1','taxable','1110','1100')",
            [],
        );
        assert!(
            half.is_err(),
            "a half-configured taxable account must be refused"
        );

        conn.execute(
            "INSERT INTO investment_account_config
               (item_id, plaid_account_id, treatment, securities_account_id, cash_account_id,
                dividend_income_account_id, interest_income_account_id,
                realized_gain_account_id, fee_expense_account_id)
             VALUES ('i1','pa1','taxable','1110','1100','4100','4110','4120','6000')",
            [],
        )
        .expect("a fully configured taxable account is accepted");

        // And the two kinds cannot be mixed: a row with both sets would mean two
        // answers to whether anything in the account is taxable.
        let both = conn.execute(
            "INSERT INTO investment_account_config
               (item_id, plaid_account_id, treatment, securities_account_id, cash_account_id,
                dividend_income_account_id, interest_income_account_id,
                realized_gain_account_id, fee_expense_account_id, retirement_account_id)
             VALUES ('i1','pa2','taxable','1110','1100','4100','4110','4120','6000','1500')",
            [],
        );
        assert!(
            both.is_err(),
            "a taxable row naming a retirement account must be refused"
        );

        conn.execute(
            "INSERT INTO investment_account_config
               (item_id, plaid_account_id, treatment, retirement_account_id)
             VALUES ('i1','pa3','sheltered','1500')",
            [],
        )
        .expect("a sheltered account is one ledger account and nothing else");

        // One provider transaction is dealt with once, whichever table it landed in.
        conn.execute(
            "INSERT INTO investment_imports
               (provider_transaction_id, item_id, plaid_account_id, outcome, entry_id)
             VALUES ('tx1','i1','pa1','buy','e1')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO investment_imports
               (provider_transaction_id, item_id, plaid_account_id, outcome, entry_id)
             VALUES ('tx1','i1','pa1','buy','e2')",
            [],
        );
        assert!(dup.is_err(), "one provider transaction is imported once");

        conn.execute(
            "INSERT INTO investment_staged_activity
               (id, item_id, plaid_account_id, provider_transaction_id, reason, detail,
                provider_type, provider_subtype, date, name, raw_payload)
             VALUES ('s1','i1','pa1','tx2','corporate_action','split','transfer','split',
                     '2026-05-20','SPLIT','{}')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO investment_staged_activity
               (id, item_id, plaid_account_id, provider_transaction_id, reason, detail,
                provider_type, provider_subtype, date, name, raw_payload)
             VALUES ('s2','i1','pa1','tx2','corporate_action','split','transfer','split',
                     '2026-05-20','SPLIT','{}')",
            [],
        );
        assert!(
            dup.is_err(),
            "the rolling re-fetch must not grow the review list by a copy of itself"
        );

        // One snapshot per account per day: the later read of the same day is the
        // better one, and two would double every position in the reconciliation.
        conn.execute(
            "INSERT INTO investment_holdings_snapshots
               (snapshot_id, item_id, plaid_account_id, as_of)
             VALUES ('sn1','i1','pa1','2026-03-31')",
            [],
        )
        .unwrap();
        let dup = conn.execute(
            "INSERT INTO investment_holdings_snapshots
               (snapshot_id, item_id, plaid_account_id, as_of)
             VALUES ('sn2','i1','pa1','2026-03-31')",
            [],
        );
        assert!(
            dup.is_err(),
            "two snapshots for one account on one day must be refused"
        );
    }

    /// A version below the high-water mark still gets applied.
    ///
    /// The failure this pins is the one that actually happened: `main` numbered
    /// 047, 048 and 050 while another branch numbered 049, so a database migrated on
    /// `main` was stamped 50, and the old `MAX(version)` gate then skipped 049
    /// forever — silently, on a database that reported itself fully migrated. Any
    /// two branches numbering migrations independently reproduce it.
    #[test]
    fn a_migration_below_the_high_water_mark_is_still_applied() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // A database that has been through the runner, then had one migration's
        // tables removed and its stamp erased — exactly the shape a merge produces.
        conn.execute_batch(
            "DROP TABLE investment_lots;
             DROP TABLE investment_sale_lots;
             DROP TABLE investment_sales;
             DROP TABLE securities;
             CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             DELETE FROM schema_migrations;",
        )
        .unwrap();
        // Everything except 47 is recorded, including numbers ABOVE it.
        for v in 1..=50 {
            if v != 47 {
                conn.execute(
                    "INSERT OR IGNORE INTO schema_migrations (version) VALUES (?1)",
                    [v],
                )
                .unwrap();
            }
        }

        run_migrations(&conn).unwrap();

        for table in [
            "securities",
            "investment_lots",
            "investment_sales",
            "investment_sale_lots",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "{table} was skipped because 50 > 47");
        }
        let stamped: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 47",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stamped, 1, "and it is recorded as applied afterwards");
    }

    /// And a database that predates it gets it too, by the route the others here use:
    /// the pre-050 shape, stamped at version 49, then migrated.
    #[test]
    fn migration_050_adds_the_investment_import_registers_to_an_existing_database() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute_batch(
            "DROP TABLE investment_holdings_snapshot_lines;
             DROP TABLE investment_holdings_snapshots;
             DROP TABLE investment_imports;
             DROP TABLE plaid_securities;
             DROP TABLE investment_account_config;
             DROP TABLE investment_staged_activity;
             DROP TABLE investment_fetch_state;
             CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             INSERT INTO schema_migrations (version) VALUES (49);",
        )
        .unwrap();
        for table in IMPORT_TABLES {
            assert!(!has_table(&conn, table), "the fixture still has {table}");
        }

        run_migrations(&conn).unwrap();
        for table in IMPORT_TABLES {
            assert!(has_table(&conn, table), "{table} was not created");
        }
        assert!(has_table(&conn, "investment_fetch_state"));

        conn.execute(
            "INSERT INTO investment_fetch_state
               (item_id, plaid_account_id, last_fetched_through)
             VALUES ('i1','pa1','2026-09-20')",
            [],
        )
        .expect("the fetch state records how far this machine has got");
        conn.execute(
            "INSERT INTO plaid_securities (plaid_security_id, security_id)
             VALUES ('sec-aapl','s1')",
            [],
        )
        .expect("the security mapping is what stops a ticker change forking a holding");

        // And running it again changes nothing — the production path is init_schema
        // then run_migrations, repeatedly.
        run_migrations(&conn).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM investment_fetch_state", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1, "a re-run must not recreate the table empty");
    }
}
#[cfg(test)]
mod event_service_rebuild {
    use super::*;
    use rusqlite::Connection;

    /// Migration 020 rebuilds `event_services`, and `staged_service_events`
    /// references it.
    ///
    /// Same shape as the 018 regression, which ate real data: drop a parent table
    /// with foreign keys enforced and the children go with it — here, a member's
    /// queue of fetched-but-unposted events. The fixture therefore has children,
    /// because a fixture without them cannot catch this.
    #[test]
    fn rebuilding_event_services_keeps_its_staged_events_and_its_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        init_schema(&conn).unwrap();
        run_migrations(&conn).unwrap();

        conn.execute_batch(
            "INSERT INTO event_services (id, name, root_url, api_key, cursor, events_processed)
                 VALUES ('svc-1', 'Bugbear Bikes', 'https://bugbearbikes.com', 'k-1', 'c-9', 7);
             INSERT INTO staged_service_events
                 (id, service_id, remote_event_id, event_type, data, timestamp)
                 VALUES ('st-1', 'svc-1', 'r-1', 'sale', '{}', '2026-08-01T00:00:00Z');",
        )
        .unwrap();

        // Re-run the rebuild the way an upgrade does.
        conn.execute("DELETE FROM schema_migrations WHERE version = 20", [])
            .unwrap();
        run_migrations(&conn).unwrap();

        let staged: i64 = conn
            .query_row("SELECT COUNT(*) FROM staged_service_events", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            staged, 1,
            "the rebuild took the staged events with it — a member would lose \
             every fetched event they had not yet posted"
        );

        // Every column has to survive, not just the row: a dropped `cursor` would
        // silently re-fetch a service's whole history on the next sync.
        let (key, cursor, processed): (Option<String>, Option<String>, i64) = conn
            .query_row(
                "SELECT api_key, cursor, events_processed FROM event_services WHERE id = 'svc-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            key.as_deref(),
            Some("k-1"),
            "a standalone book keeps its key"
        );
        assert_eq!(cursor.as_deref(), Some("c-9"));
        assert_eq!(processed, 7);

        let notnull: i64 = conn
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('event_services') WHERE name='api_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(notnull, 0, "api_key must be nullable after 020");

        // The case the migration exists for.
        conn.execute(
            "INSERT INTO event_services (id, name, root_url, api_key)
                 VALUES ('svc-2', 'Hosted Service', 'https://hosted.test', NULL)",
            [],
        )
        .expect("a service registered on hosted books has no key in the log");

        // The uniqueness backstop is part of the table's contract and the rebuild
        // dropped the index along with the old table. Losing it would let a
        // service be registered twice and double-post every event it publishes.
        conn.execute(
            "INSERT INTO event_services (id, name, root_url, api_key)
                 VALUES ('svc-3', 'Duplicate', 'https://hosted.test', NULL)",
            [],
        )
        .expect_err("the active-root_url unique index must survive the rebuild");
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
