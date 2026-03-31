use accountir::commands::account_commands::{AccountCommands, CreateAccountCommand};
use accountir::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use accountir::domain::AccountType;
use accountir::events::types::{Event, JournalEntrySource};
use accountir::queries::account_queries::AccountQueries;
use accountir::queries::reports::Reports;
use accountir::store::event_store::EventStore;
use accountir::store::merkle::MerkleTree;
use accountir::store::migrations::init_schema;
use accountir::tui::views::welcome::reset_welcome;
use accountir::tui::{run_app, run_app_with_database};
use anyhow::Result;
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "accountir")]
#[command(about = "Event-sourced double-entry accounting system", long_about = None)]
struct Cli {
    /// Database file path
    #[arg(short, long, default_value = "accountir.db")]
    database: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new database
    Init,

    /// Launch the terminal user interface
    Tui,

    /// Account management
    #[command(subcommand)]
    Account(AccountCommands_),

    /// Journal entry management
    #[command(subcommand)]
    Entry(EntryCommands_),

    /// Generate reports
    #[command(subcommand)]
    Report(ReportCommands),

    /// Merkle tree operations
    #[command(subcommand)]
    Merkle(MerkleCommands),

    /// Show system status
    Status,

    /// Reset the welcome screen to show on next startup
    ResetWelcome,

    /// Start the HTTP sync server for browser extension communication
    Serve {
        /// Database file path (overrides top-level -d)
        #[arg(short, long)]
        database: Option<PathBuf>,
    },

    /// Plaid bank sync management
    #[command(subcommand)]
    Plaid(PlaidCommands_),

    /// Import a GnuCash file into a fresh database
    ImportGnucash {
        /// Path to the GnuCash file (gzip or plain XML)
        file: PathBuf,
        /// Output database path (default: {input_stem}.db)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum AccountCommands_ {
    /// Create a new account
    Create {
        #[arg(short = 't', long)]
        account_type: String,
        #[arg(short = 'n', long)]
        number: String,
        #[arg(long)]
        name: String,
        #[arg(short, long)]
        currency: Option<String>,
        #[arg(short, long)]
        description: Option<String>,
    },
    /// List all accounts
    List {
        #[arg(short = 't', long)]
        account_type: Option<String>,
    },
    /// Show account balance
    Balance {
        #[arg(short, long)]
        account_id: String,
        #[arg(short, long)]
        as_of: Option<String>,
    },
    /// Show account ledger
    Ledger {
        #[arg(short, long)]
        account_id: String,
        #[arg(long)]
        start: Option<String>,
        #[arg(long)]
        end: Option<String>,
    },
}

#[derive(Subcommand)]
enum EntryCommands_ {
    /// Post a new journal entry
    Post {
        #[arg(short, long)]
        date: String,
        #[arg(short, long)]
        memo: String,
        /// Lines in format: account_id:amount (positive=debit, negative=credit)
        #[arg(short, long, num_args = 2..)]
        lines: Vec<String>,
        #[arg(short, long)]
        reference: Option<String>,
    },
    /// List recent entries
    List {
        #[arg(short, long, default_value = "10")]
        limit: u32,
    },
    /// Void an entry
    Void {
        #[arg(short, long)]
        entry_id: String,
        #[arg(short, long)]
        reason: String,
    },
}

#[derive(Subcommand)]
enum ReportCommands {
    /// Generate trial balance
    TrialBalance {
        #[arg(short, long)]
        as_of: Option<String>,
    },
    /// Generate balance sheet
    BalanceSheet {
        #[arg(short, long)]
        as_of: String,
    },
    /// Generate income statement
    IncomeStatement {
        #[arg(long)]
        start: String,
        #[arg(long)]
        end: String,
    },
}

#[derive(Subcommand)]
enum MerkleCommands {
    /// Build/rebuild the Merkle tree
    Build,
    /// Show the root hash
    Root,
    /// Verify a specific event
    Verify {
        #[arg(short, long)]
        event_id: i64,
    },
}

#[derive(Subcommand)]
enum PlaidCommands_ {
    /// Configure Plaid proxy connection
    Config {
        /// Proxy server URL
        #[arg(long)]
        proxy_url: String,
        /// API key from proxy registration
        #[arg(long)]
        api_key: String,
    },
    /// Register with the Plaid proxy server
    Register {
        /// Email address
        #[arg(long)]
        email: String,
        /// Proxy server URL
        #[arg(long)]
        proxy_url: String,
    },
    /// List connected Plaid items
    Items,
    /// Sync transactions from a Plaid item
    Sync {
        /// Item ID to sync (syncs all if omitted)
        #[arg(long)]
        item_id: Option<String>,
    },
    /// Show Plaid configuration status
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init => {
            let mut store = EventStore::open(&cli.database)?;
            init_schema(store.connection())?;

            // Ensure company exists
            let has_company: bool = store
                .connection()
                .query_row(
                    "SELECT COUNT(*) > 0 FROM company WHERE id = 'default'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            if !has_company {
                let company_name = cli
                    .database
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("My Company")
                    .to_string();
                let envelope = accountir::events::types::EventEnvelope::new(
                    Event::CompanyCreated {
                        company_id: uuid::Uuid::new_v4().to_string(),
                        name: company_name,
                        base_currency: "USD".to_string(),
                        fiscal_year_start: 1,
                    },
                    "cli-user".to_string(),
                );
                let stored = store.append(envelope)?;
                let projector = accountir::store::projections::Projector::new(store.connection());
                projector.apply(&stored)?;
            }

            println!("Database initialized at {:?}", cli.database);
        }

        Commands::Tui => {
            // Start background sync server before entering the TUI
            let server_db = accountir::server::start_server_task().await;

            // If a specific database was provided (not the default), open it directly
            if cli.database != std::path::Path::new("accountir.db") {
                run_app_with_database(&cli.database, server_db)?;
            } else {
                // Otherwise show the startup screen to select/create a database
                run_app(server_db)?;
            }
        }

        Commands::Account(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            handle_account_command(&mut store, cmd)?;
        }

        Commands::Entry(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            handle_entry_command(&mut store, cmd)?;
        }

        Commands::Report(cmd) => {
            let store = EventStore::open(&cli.database)?;
            handle_report_command(&store, cmd)?;
        }

        Commands::Merkle(cmd) => {
            let store = EventStore::open(&cli.database)?;
            handle_merkle_command(&store, cmd)?;
        }

        Commands::Status => {
            let store = EventStore::open(&cli.database)?;
            show_status(&store)?;
        }

        Commands::ResetWelcome => {
            reset_welcome();
            println!("Welcome screen reset. It will show on next startup.");
        }

        Commands::Serve { database } => {
            let db = database.unwrap_or(cli.database);
            let store = EventStore::open(&db)?;
            let db_path = std::fs::canonicalize(&db).unwrap_or_else(|_| db.clone());
            accountir::server::run_server(store, db_path).await?;
        }

        Commands::Plaid(cmd) => {
            handle_plaid_command(cmd).await?;
        }

        Commands::ImportGnucash { file, output } => {
            handle_import_gnucash(&file, output)?;
        }
    }

    Ok(())
}

fn handle_account_command(store: &mut EventStore, cmd: AccountCommands_) -> Result<()> {
    match cmd {
        AccountCommands_::Create {
            account_type,
            number,
            name,
            currency,
            description,
        } => {
            let acc_type = parse_account_type(&account_type)?;
            let mut commands = AccountCommands::new(store, "cli-user".to_string());

            let event = commands.create_account(CreateAccountCommand {
                account_type: acc_type,
                account_number: number,
                name: name.clone(),
                parent_id: None,
                currency,
                description,
            })?;

            if let Event::AccountCreated { account_id, .. } = event.event {
                println!("Account created: {} ({})", name, account_id);
            }
        }

        AccountCommands_::List { account_type } => {
            let queries = AccountQueries::new(store.connection());
            let accounts = if let Some(type_str) = account_type {
                let acc_type = parse_account_type(&type_str)?;
                queries.get_accounts_by_type(acc_type)?
            } else {
                queries.get_all_accounts()?
            };

            println!(
                "{:<36} {:<10} {:<20} {:<10}",
                "ID", "Number", "Name", "Type"
            );
            println!("{}", "-".repeat(80));
            for acc in accounts {
                println!(
                    "{:<36} {:<10} {:<20} {:<10}",
                    acc.id, acc.account_number, acc.name, acc.account_type
                );
            }
        }

        AccountCommands_::Balance { account_id, as_of } => {
            let queries = AccountQueries::new(store.connection());
            let date = as_of
                .map(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d"))
                .transpose()?;
            let balance = queries.get_account_balance(&account_id, date)?;

            println!(
                "Account: {} ({})",
                balance.account_name, balance.account_number
            );
            println!("Type: {}", balance.account_type);
            println!(
                "Balance: {} {}",
                format_amount(balance.balance),
                balance.currency
            );
        }

        AccountCommands_::Ledger {
            account_id,
            start,
            end,
        } => {
            let queries = AccountQueries::new(store.connection());
            let start_date = start
                .map(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d"))
                .transpose()?;
            let end_date = end
                .map(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d"))
                .transpose()?;

            let ledger = queries.get_account_ledger(&account_id, start_date, end_date)?;

            println!(
                "{:<12} {:<30} {:>12} {:>12} {:>14}",
                "Date", "Memo", "Debit", "Credit", "Balance"
            );
            println!("{}", "-".repeat(84));

            for entry in ledger {
                let debit = entry.debit.map(format_amount).unwrap_or_default();
                let credit = entry.credit.map(format_amount).unwrap_or_default();
                let void_marker = if entry.is_void { " (VOID)" } else { "" };

                println!(
                    "{:<12} {:<30} {:>12} {:>12} {:>14}{}",
                    entry.date,
                    truncate(&entry.memo, 28),
                    debit,
                    credit,
                    format_amount(entry.running_balance),
                    void_marker
                );
            }
        }
    }
    Ok(())
}

fn handle_entry_command(store: &mut EventStore, cmd: EntryCommands_) -> Result<()> {
    match cmd {
        EntryCommands_::Post {
            date,
            memo,
            lines,
            reference,
        } => {
            let entry_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")?;
            let parsed_lines: Result<Vec<EntryLine>, _> = lines
                .iter()
                .map(|l| {
                    let parts: Vec<&str> = l.split(':').collect();
                    if parts.len() != 2 {
                        anyhow::bail!("Invalid line format: {}. Use account_id:amount", l);
                    }
                    let account_id = parts[0].to_string();
                    let amount: i64 = parts[1].parse()?;
                    Ok(EntryLine {
                        account_id,
                        amount,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    })
                })
                .collect();

            let mut commands = EntryCommands::new(store, "cli-user".to_string());
            let event = commands.post_entry(PostEntryCommand {
                date: entry_date,
                memo: memo.clone(),
                lines: parsed_lines?,
                reference,
                source: Some(JournalEntrySource::Manual),
            })?;

            if let Event::JournalEntryPosted { entry_id, .. } = event.event {
                println!("Entry posted: {} - {}", entry_id, memo);
            }
        }

        EntryCommands_::List { limit } => {
            let search = accountir::queries::search::Search::new(store.connection());
            let entries = search.recent_entries(limit)?;

            println!(
                "{:<36} {:<12} {:<30} {:>12}",
                "ID", "Date", "Memo", "Amount"
            );
            println!("{}", "-".repeat(94));

            for entry in entries {
                let void_marker = if entry.is_void { " (VOID)" } else { "" };
                println!(
                    "{:<36} {:<12} {:<30} {:>12}{}",
                    entry.entry_id,
                    entry.date,
                    truncate(&entry.memo, 28),
                    format_amount(entry.total_amount),
                    void_marker
                );
            }
        }

        EntryCommands_::Void { entry_id, reason } => {
            let mut commands = EntryCommands::new(store, "cli-user".to_string());
            let cmd = accountir::commands::entry_commands::VoidEntryCommand {
                entry_id: entry_id.clone(),
                reason,
            };
            commands.void_entry(cmd)?;
            println!("Entry {} voided", entry_id);
        }
    }
    Ok(())
}

fn handle_report_command(store: &EventStore, cmd: ReportCommands) -> Result<()> {
    let reports = Reports::new(store.connection());

    match cmd {
        ReportCommands::TrialBalance { as_of } => {
            let date = as_of
                .map(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d"))
                .transpose()?;
            let tb = reports.trial_balance(date)?;

            println!("TRIAL BALANCE");
            if let Some(d) = tb.as_of_date {
                println!("As of: {}", d);
            }
            println!();
            println!(
                "{:<10} {:<30} {:>14} {:>14}",
                "Number", "Account", "Debit", "Credit"
            );
            println!("{}", "-".repeat(70));

            for line in &tb.lines {
                let debit = line.debit.map(format_amount).unwrap_or_default();
                let credit = line.credit.map(format_amount).unwrap_or_default();
                println!(
                    "{:<10} {:<30} {:>14} {:>14}",
                    line.account_number,
                    truncate(&line.account_name, 28),
                    debit,
                    credit
                );
            }

            println!("{}", "-".repeat(70));
            println!(
                "{:<10} {:<30} {:>14} {:>14}",
                "",
                "TOTALS",
                format_amount(tb.total_debits),
                format_amount(tb.total_credits)
            );

            if tb.is_balanced {
                println!("\nTrial balance is BALANCED");
            } else {
                println!("\nWARNING: Trial balance is NOT BALANCED!");
            }
        }

        ReportCommands::BalanceSheet { as_of } => {
            let date = NaiveDate::parse_from_str(&as_of, "%Y-%m-%d")?;
            let bs = reports.balance_sheet(date)?;

            println!("BALANCE SHEET");
            println!("As of: {}", date);
            println!();

            println!("ASSETS");
            println!("{}", "-".repeat(50));
            for line in &bs.assets.lines {
                println!(
                    "  {:<30} {:>14}",
                    line.account_name,
                    format_amount(line.balance)
                );
            }
            println!(
                "  {:<30} {:>14}",
                "Total Assets",
                format_amount(bs.total_assets)
            );
            println!();

            println!("LIABILITIES");
            println!("{}", "-".repeat(50));
            for line in &bs.liabilities.lines {
                println!(
                    "  {:<30} {:>14}",
                    line.account_name,
                    format_amount(line.balance.abs())
                );
            }
            println!(
                "  {:<30} {:>14}",
                "Total Liabilities",
                format_amount(bs.liabilities.total)
            );
            println!();

            println!("EQUITY");
            println!("{}", "-".repeat(50));
            for line in &bs.equity.lines {
                println!(
                    "  {:<30} {:>14}",
                    line.account_name,
                    format_amount(line.balance.abs())
                );
            }
            println!(
                "  {:<30} {:>14}",
                "Total Equity",
                format_amount(bs.equity.total)
            );
            println!();

            println!("{}", "=".repeat(50));
            println!(
                "{:<32} {:>14}",
                "Total Liabilities & Equity",
                format_amount(bs.total_liabilities_and_equity)
            );

            if bs.is_balanced {
                println!("\nBalance sheet is BALANCED");
            } else {
                println!("\nWARNING: Balance sheet is NOT BALANCED!");
            }
        }

        ReportCommands::IncomeStatement { start, end } => {
            let start_date = NaiveDate::parse_from_str(&start, "%Y-%m-%d")?;
            let end_date = NaiveDate::parse_from_str(&end, "%Y-%m-%d")?;
            let is = reports.income_statement(start_date, end_date)?;

            println!("INCOME STATEMENT");
            println!("Period: {} to {}", start_date, end_date);
            println!();

            println!("REVENUE");
            println!("{}", "-".repeat(50));
            for line in &is.revenue.lines {
                println!(
                    "  {:<30} {:>14}",
                    line.account_name,
                    format_amount(line.balance)
                );
            }
            println!(
                "  {:<30} {:>14}",
                "Total Revenue",
                format_amount(is.revenue.total)
            );
            println!();

            println!("EXPENSES");
            println!("{}", "-".repeat(50));
            for line in &is.expenses.lines {
                println!(
                    "  {:<30} {:>14}",
                    line.account_name,
                    format_amount(line.balance)
                );
            }
            println!(
                "  {:<30} {:>14}",
                "Total Expenses",
                format_amount(is.expenses.total)
            );
            println!();

            println!("{}", "=".repeat(50));
            println!("{:<32} {:>14}", "NET INCOME", format_amount(is.net_income));
        }
    }
    Ok(())
}

fn handle_merkle_command(store: &EventStore, cmd: MerkleCommands) -> Result<()> {
    match cmd {
        MerkleCommands::Build => {
            let hashes = store.get_all_hashes()?;
            let conn = rusqlite::Connection::open_in_memory()?;
            init_schema(&conn)?;

            // Copy merkle_nodes table structure
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS merkle_nodes (
                    level INTEGER NOT NULL,
                    position INTEGER NOT NULL,
                    hash BLOB NOT NULL,
                    left_child_pos INTEGER,
                    right_child_pos INTEGER,
                    PRIMARY KEY (level, position)
                )",
            )?;

            let mut tree = MerkleTree::new(conn);
            let root = tree.build(&hashes)?;

            if let Some(hash) = root {
                println!("Merkle tree built with {} events", hashes.len());
                println!("Root hash: {}", hex::encode(&hash));
            } else {
                println!("No events to build tree from");
            }
        }

        MerkleCommands::Root => {
            let hashes = store.get_all_hashes()?;
            if hashes.is_empty() {
                println!("No events in the system");
                return Ok(());
            }

            let conn = rusqlite::Connection::open_in_memory()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS merkle_nodes (
                    level INTEGER NOT NULL,
                    position INTEGER NOT NULL,
                    hash BLOB NOT NULL,
                    left_child_pos INTEGER,
                    right_child_pos INTEGER,
                    PRIMARY KEY (level, position)
                )",
            )?;

            let mut tree = MerkleTree::new(conn);
            if let Some(hash) = tree.build(&hashes)? {
                println!("Root hash: {}", hex::encode(&hash));
                println!("Events: {}", hashes.len());
                println!("Tree height: {}", tree.height()?);
            }
        }

        MerkleCommands::Verify { event_id } => {
            let event_hash = store.get_hash(event_id)?;
            let hashes = store.get_all_hashes()?;

            let conn = rusqlite::Connection::open_in_memory()?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS merkle_nodes (
                    level INTEGER NOT NULL,
                    position INTEGER NOT NULL,
                    hash BLOB NOT NULL,
                    left_child_pos INTEGER,
                    right_child_pos INTEGER,
                    PRIMARY KEY (level, position)
                )",
            )?;

            let mut tree = MerkleTree::new(conn);
            tree.build(&hashes)?;

            let position = (event_id - 1) as usize; // Events are 1-indexed
            if tree.verify(position, &event_hash)? {
                println!("Event {} is VERIFIED in the Merkle tree", event_id);
                println!("Hash: {}", hex::encode(&event_hash));
            } else {
                println!("WARNING: Event {} FAILED verification!", event_id);
            }
        }
    }
    Ok(())
}

fn show_status(store: &EventStore) -> Result<()> {
    let event_count = store.count()?;
    let account_count: i32 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
        .unwrap_or(0);
    let entry_count: i32 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM journal_entries WHERE is_void = 0",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    println!("Accountir Status");
    println!("{}", "=".repeat(40));
    println!("Events:          {}", event_count);
    println!("Accounts:        {}", account_count);
    println!("Journal Entries: {}", entry_count);

    // Show Merkle root if events exist
    if event_count > 0 {
        let hashes = store.get_all_hashes()?;
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS merkle_nodes (
                level INTEGER NOT NULL,
                position INTEGER NOT NULL,
                hash BLOB NOT NULL,
                left_child_pos INTEGER,
                right_child_pos INTEGER,
                PRIMARY KEY (level, position)
            )",
        )?;

        let mut tree = MerkleTree::new(conn);
        if let Some(root) = tree.build(&hashes)? {
            println!("Merkle Root:     {}", &hex::encode(&root)[..16]);
        }
    }

    Ok(())
}

fn handle_import_gnucash(file: &std::path::Path, output: Option<PathBuf>) -> Result<()> {
    use accountir::gnucash;
    use accountir::store::migrations::init_schema;

    // Determine output path
    let db_path = output.unwrap_or_else(|| {
        let stem = file.file_stem().unwrap_or_default().to_string_lossy();
        PathBuf::from(format!("{}.db", stem))
    });

    // Refuse to overwrite existing database
    if db_path.exists() {
        anyhow::bail!(
            "Database '{}' already exists. Remove it first or specify a different output with -o.",
            db_path.display()
        );
    }

    // Derive company name from filename
    let company_name = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("GnuCash Import")
        .to_string();

    println!("Parsing GnuCash file: {}", file.display());
    let book = gnucash::parse_gnucash_file(file)?;
    println!(
        "  Found {} commodities, {} accounts, {} transactions",
        book.commodities.len(),
        book.accounts.len(),
        book.transactions.len()
    );

    println!("Creating database: {}", db_path.display());
    let mut store = EventStore::open(&db_path)?;
    init_schema(store.connection())?;

    println!("Importing...");
    let summary = gnucash::import::import_gnucash(&book, &mut store, &company_name)?;

    println!();
    println!("Import Summary");
    println!("{}", "=".repeat(40));
    println!("Currencies:          {}", summary.currencies_imported);
    println!(
        "Accounts:            {} imported, {} skipped",
        summary.accounts_imported, summary.accounts_skipped
    );
    println!(
        "Transactions:        {} imported, {} skipped",
        summary.transactions_imported, summary.transactions_skipped
    );
    println!("Total splits:        {}", summary.total_splits);
    println!("Total events:        {}", summary.total_events);

    if !summary.warnings.is_empty() {
        println!();
        println!("Warnings ({}):", summary.warnings.len());
        for w in &summary.warnings {
            println!("  - {}", w);
        }
    }

    println!();
    println!("Database written to: {}", db_path.display());

    Ok(())
}

async fn handle_plaid_command(cmd: PlaidCommands_) -> Result<()> {
    use accountir::config::{AppConfig, PlaidConfig};

    match cmd {
        PlaidCommands_::Config { proxy_url, api_key } => {
            let mut config = AppConfig::load();
            config.plaid = PlaidConfig {
                proxy_url: Some(proxy_url.clone()),
                api_key: Some(api_key),
            };
            config.save()?;
            println!("Plaid proxy configured: {}", proxy_url);
        }

        PlaidCommands_::Register { email, proxy_url } => {
            let client = reqwest::Client::new();
            let resp = client
                .post(format!("{}/auth/register", proxy_url))
                .json(&serde_json::json!({ "email": email }))
                .send()
                .await?;

            if !resp.status().is_success() {
                let err: serde_json::Value = resp.json().await.unwrap_or_default();
                anyhow::bail!(
                    "Registration failed: {}",
                    err["error"].as_str().unwrap_or("Unknown error")
                );
            }

            let body: serde_json::Value = resp.json().await?;
            let api_key = body["api_key"].as_str().unwrap_or("");
            let user_id = body["user_id"].as_str().unwrap_or("");

            println!("Registration successful!");
            println!("User ID: {}", user_id);
            println!("API Key: {}", api_key);
            println!();
            println!("Save this API key - it cannot be retrieved again.");
            println!(
                "To configure: accountir plaid config --proxy-url {} --api-key {}",
                proxy_url, api_key
            );
        }

        PlaidCommands_::Items => {
            let config = AppConfig::load();
            if !config.plaid.is_configured() {
                anyhow::bail!("Plaid not configured. Run: accountir plaid config --proxy-url <url> --api-key <key>");
            }

            let client = reqwest::Client::new();
            let resp = client
                .get(format!("{}/plaid/items", config.plaid.proxy_url.unwrap()))
                .bearer_auth(config.plaid.api_key.unwrap())
                .send()
                .await?;

            if !resp.status().is_success() {
                anyhow::bail!("Failed to fetch items: {}", resp.status());
            }

            let body: serde_json::Value = resp.json().await?;
            let items = body["items"].as_array();

            match items {
                Some(items) if !items.is_empty() => {
                    println!("{:<36} {:<25} {:<10}", "ID", "Institution", "Status");
                    println!("{}", "-".repeat(75));
                    for item in items {
                        println!(
                            "{:<36} {:<25} {:<10}",
                            item["id"].as_str().unwrap_or(""),
                            item["institution_name"].as_str().unwrap_or(""),
                            item["status"].as_str().unwrap_or(""),
                        );
                    }
                }
                _ => println!("No connected bank accounts."),
            }
        }

        PlaidCommands_::Sync { item_id: _ } => {
            println!("Sync via CLI requires the local server to be running.");
            println!("Start the TUI (accountir tui) and use the Plaid view to sync.");
        }

        PlaidCommands_::Status => {
            let config = AppConfig::load();
            println!("Plaid Configuration Status");
            println!("{}", "=".repeat(40));
            if config.plaid.is_configured() {
                println!(
                    "Proxy URL: {}",
                    config.plaid.proxy_url.as_deref().unwrap_or("")
                );
                println!(
                    "API Key:   {}...",
                    &config.plaid.api_key.as_deref().unwrap_or("")
                        [..12.min(config.plaid.api_key.as_deref().unwrap_or("").len())]
                );
                println!("Status:    Configured");
            } else {
                println!("Status:    Not configured");
                println!();
                println!("To set up: accountir plaid config --proxy-url <url> --api-key <key>");
            }
        }
    }

    Ok(())
}

fn parse_account_type(s: &str) -> Result<AccountType> {
    match s.to_lowercase().as_str() {
        "asset" => Ok(AccountType::Asset),
        "liability" => Ok(AccountType::Liability),
        "equity" => Ok(AccountType::Equity),
        "revenue" => Ok(AccountType::Revenue),
        "expense" => Ok(AccountType::Expense),
        _ => anyhow::bail!(
            "Invalid account type: {}. Use: asset, liability, equity, revenue, expense",
            s
        ),
    }
}

fn format_amount(cents: i64) -> String {
    let abs = cents.abs();
    let dollars = abs / 100;
    let remainder = abs % 100;
    if cents < 0 {
        format!("({}.{:02})", dollars, remainder)
    } else {
        format!("{}.{:02}", dollars, remainder)
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}
