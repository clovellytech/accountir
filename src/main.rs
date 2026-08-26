use accountir::commands::account_commands::{AccountCommands, CreateAccountCommand};
use accountir::commands::bill_commands::{
    ApplyBillPaymentCommand, BillCommands as BillCommandHandler, ReceiveBillCommand, VoidBillCommand,
};
use accountir::commands::entry_commands::{EntryCommands, EntryLine, PostEntryCommand};
use accountir::commands::invoice_commands::{
    InvoiceCommands as InvoiceCommandHandler, IssueInvoiceCommand, ReceiveInvoicePaymentCommand,
    VoidInvoiceCommand,
};
use accountir::domain::{AccountType, PaymentTerms};
use accountir::events::types::{Event, JournalEntrySource};
use accountir::queries::account_queries::AccountQueries;
use accountir::queries::ap_ar_queries::ApArQueries;
use accountir::queries::reports::Reports;
use accountir::store::event_store::EventStore;
use accountir::store::merkle::MerkleTree;
use accountir::store::migrations::init_schema;
use accountir::store::projections::ProjectionStore;
use accountir::tui::run_app;
use accountir::tui::views::welcome::reset_welcome;
use anyhow::Result;
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "accountir")]
#[command(about = "Event-sourced double-entry accounting system", long_about = None)]
struct Cli {
    /// Database file for non-TUI subcommands (ignored by `tui` — that goes through the picker)
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

    /// Accounts payable (bills)
    #[command(subcommand)]
    Bill(BillCliCommands),

    /// Accounts receivable (invoices)
    #[command(subcommand)]
    Invoice(InvoiceCliCommands),

    /// Import a GnuCash file into a fresh database
    ImportGnucash {
        /// Path to the GnuCash file (gzip or plain XML)
        file: PathBuf,
        /// Output database path (default: {input_stem}.db)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Import a Square export file by hand (sales CSV or payroll xlsx)
    #[command(subcommand)]
    Square(SquareCliCommands),

    /// Import an Amazon Business export by hand (order history CSV)
    #[command(subcommand)]
    Amazon(AmazonCliCommands),

    /// The partnership's own details, and its partners
    #[command(subcommand)]
    Partnership(PartnershipCliCommands),

    /// Generate tax forms from the books
    #[command(subcommand)]
    Tax(TaxCliCommands),
}

#[derive(Subcommand)]
enum PartnershipCliCommands {
    /// Set the partnership's details, as they appear at the head of Form 1065
    Profile {
        /// The name on the SS-4 — what the IRS matches the EIN against
        #[arg(long)]
        legal_name: String,
        #[arg(long)]
        street: String,
        #[arg(long)]
        suite: Option<String>,
        #[arg(long)]
        city: String,
        /// State, or province for a foreign address
        #[arg(long)]
        state: String,
        /// ZIP, or foreign postal code
        #[arg(long)]
        postal_code: String,
        /// Left blank for a US address, which is what the form expects
        #[arg(long)]
        country: Option<String>,
        /// Employer identification number, NN-NNNNNNN
        #[arg(long)]
        ein: String,
        /// Six-digit NAICS code (Form 1065 box C)
        #[arg(long)]
        naics: String,
        /// Date business started, YYYY-MM-DD (box E)
        #[arg(long)]
        started: String,
        /// Principal business activity (box A)
        #[arg(long)]
        activity: Option<String>,
        /// Principal product or service (box B)
        #[arg(long)]
        product: Option<String>,
    },

    /// Show the partnership's details and its partners
    Show,

    /// Add a partner
    AddPartner {
        #[arg(long)]
        name: String,
        /// "general" (or LLC member-manager) or "limited" — K-1 item G
        #[arg(long, default_value = "general")]
        r#type: String,
        /// "domestic" or "foreign" — K-1 item H1
        #[arg(long, default_value = "domestic")]
        residency: String,
        /// K-1 item I1, e.g. "Individual", "S Corporation", "Estate"
        #[arg(long, default_value = "Individual")]
        entity_type: String,
        #[arg(long)]
        street: String,
        #[arg(long)]
        suite: Option<String>,
        #[arg(long)]
        city: String,
        #[arg(long)]
        state: String,
        #[arg(long)]
        postal_code: String,
        #[arg(long)]
        country: Option<String>,
        /// YYYY-MM-DD. Defaults to the date the business started
        #[arg(long)]
        started: Option<String>,
        /// Share of profit, as a percentage
        #[arg(long)]
        profit: f64,
        /// Share of loss, as a percentage. Defaults to the profit share
        #[arg(long)]
        loss: Option<f64>,
        /// Share of capital, as a percentage. Defaults to the profit share
        #[arg(long)]
        capital: Option<f64>,
        /// SSN (NNN-NN-NNNN) or EIN (NN-NNNNNNN). Stored on this machine only,
        /// never in the replicated event log
        #[arg(long)]
        tin: Option<String>,
    },

    /// List the partners
    Partners {
        /// Only those who held an interest during this tax year
        #[arg(long)]
        year: Option<i32>,
    },

    /// Record that a partner has left
    RemovePartner {
        partner_id: String,
        /// The day they left, YYYY-MM-DD
        #[arg(long)]
        on: String,
    },

    /// Set a partner's TIN on this machine. Never written to the event log
    SetTin {
        partner_id: String,
        tin: String,
    },
}

#[derive(Subcommand)]
enum TaxCliCommands {
    /// Build Form 1065 with a Schedule K-1 per partner, as one fillable PDF
    Form1065 {
        /// Tax year to file
        #[arg(long)]
        year: i32,
        /// Where to write the PDF
        #[arg(short, long)]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum AmazonCliCommands {
    /// Import an Amazon Business "Order History Report" CSV. Posts one entry per
    /// card charge, clearing the mapped `amazon_clearing` account. Idempotent.
    Orders {
        /// Path to the order history CSV, e.g. orders_from_20250529_to_20260629_*.csv
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum SquareCliCommands {
    /// Import a Square sales-summary CSV (the date range is read from the filename)
    Sales {
        /// Path to the sales-summary CSV, e.g. sales-summary-2026-06-26-2026-06-26.csv
        file: PathBuf,
    },
    /// Import a Square payroll "Company Totals" .xlsx (date range read from the filename)
    Payroll {
        /// Path to the Company Totals .xlsx, e.g. Company-Totals-2026-06-01-2026-06-30-.xlsx
        file: PathBuf,
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

#[derive(Subcommand)]
enum BillCliCommands {
    /// Record a new bill from a vendor
    Receive {
        #[arg(long)]
        vendor: String,
        /// Amount in dollars (e.g. 500.00)
        #[arg(long)]
        amount: f64,
        #[arg(long, default_value = "USD")]
        currency: String,
        /// Issue date (YYYY-MM-DD)
        #[arg(long)]
        date: String,
        /// Payment terms (net30, net60, net90, due-on-receipt, or number of days)
        #[arg(long, default_value = "net30")]
        terms: String,
        /// Expense account ID (debit side)
        #[arg(long)]
        expense_account: String,
        /// Accounts Payable account ID (credit side)
        #[arg(long)]
        ap_account: String,
        #[arg(long)]
        memo: Option<String>,
    },
    /// Apply a payment to a bill
    Pay {
        #[arg(long)]
        bill_id: String,
        /// Amount in dollars
        #[arg(long)]
        amount: f64,
        /// Payment date (YYYY-MM-DD)
        #[arg(long)]
        date: String,
        /// Bank/cash account to pay from
        #[arg(long)]
        payment_account: String,
        /// Accounts Payable account ID
        #[arg(long)]
        ap_account: String,
        #[arg(long)]
        memo: Option<String>,
    },
    /// List bills
    List {
        /// Filter by status (open, partial, paid, void)
        #[arg(long)]
        status: Option<String>,
    },
    /// Void a bill (only if no payments applied)
    Void {
        #[arg(long)]
        bill_id: String,
        #[arg(long)]
        reason: String,
    },
    /// Show AP aging report
    Aging,
}

#[derive(Subcommand)]
enum InvoiceCliCommands {
    /// Issue a new invoice to a customer
    Issue {
        #[arg(long)]
        customer: String,
        /// Amount in dollars (e.g. 1000.00)
        #[arg(long)]
        amount: f64,
        #[arg(long, default_value = "USD")]
        currency: String,
        /// Issue date (YYYY-MM-DD)
        #[arg(long)]
        date: String,
        /// Payment terms (net30, net60, net90, due-on-receipt, or number of days)
        #[arg(long, default_value = "net30")]
        terms: String,
        /// Revenue account ID (credit side)
        #[arg(long)]
        revenue_account: String,
        /// Accounts Receivable account ID (debit side)
        #[arg(long)]
        ar_account: String,
        #[arg(long)]
        memo: Option<String>,
    },
    /// Record a payment received on an invoice
    ReceivePayment {
        #[arg(long)]
        invoice_id: String,
        /// Amount in dollars
        #[arg(long)]
        amount: f64,
        /// Payment date (YYYY-MM-DD)
        #[arg(long)]
        date: String,
        /// Bank/cash account receiving the payment
        #[arg(long)]
        payment_account: String,
        /// Accounts Receivable account ID
        #[arg(long)]
        ar_account: String,
        #[arg(long)]
        memo: Option<String>,
    },
    /// List invoices
    List {
        /// Filter by status (open, partial, paid, void)
        #[arg(long)]
        status: Option<String>,
    },
    /// Void an invoice (only if no payments received)
    Void {
        #[arg(long)]
        invoice_id: String,
        #[arg(long)]
        reason: String,
    },
    /// Show AR aging report
    Aging,
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
                store.apply_projection(&stored)?;
            }

            println!("Database initialized at {:?}", cli.database);
        }

        Commands::Tui => {
            // Start background sync server before entering the TUI
            let server_db = accountir::server::start_server_task().await;
            // Always go through the business picker.
            run_app(server_db)?;
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

        Commands::Bill(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            handle_bill_command(&mut store, cmd)?;
        }

        Commands::Invoice(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            handle_invoice_command(&mut store, cmd)?;
        }

        Commands::ImportGnucash { file, output } => {
            handle_import_gnucash(&file, output)?;
        }

        Commands::Square(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            accountir::store::migrations::run_migrations(store.connection())?;
            handle_square_command(&mut store, cmd)?;
        }

        Commands::Amazon(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            accountir::store::migrations::run_migrations(store.connection())?;
            handle_amazon_command(&mut store, cmd)?;
        }

        Commands::Partnership(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            accountir::store::migrations::run_migrations(store.connection())?;
            handle_partnership_command(&mut store, cmd)?;
        }

        Commands::Tax(cmd) => {
            let store = EventStore::open(&cli.database)?;
            handle_tax_command(&store, cmd)?;
        }
    }

    Ok(())
}

fn handle_amazon_command(store: &mut EventStore, cmd: AmazonCliCommands) -> Result<()> {
    use accountir::commands::amazon_commands;

    match cmd {
        AmazonCliCommands::Orders { file } => {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("read {}: {}", file.display(), e))?;
            let s = amazon_commands::ingest_amazon_orders(store, "cli", &content)?;
            println!(
                "Amazon orders: {} entr{} posted, {} skipped (already imported)",
                s.entries_posted,
                if s.entries_posted == 1 { "y" } else { "ies" },
                s.skipped_duplicates
            );
            println!(
                "  {} charges seen · {} cancelled order(s) skipped · {} pending order(s) skipped",
                s.charges_seen, s.cancelled_orders, s.pending_orders
            );
            if s.reconciled_charges > 0 {
                println!(
                    "  ⚠ {} charge(s) had line items that didn't foot to the payment total — \
                     review the 'reconciling difference' lines",
                    s.reconciled_charges
                );
            }
        }
    }

    Ok(())
}

fn handle_square_command(store: &mut EventStore, cmd: SquareCliCommands) -> Result<()> {
    use accountir::commands::square_commands;

    let report = |summary: square_commands::SquareImportSummary, label: &str| {
        println!(
            "Square {}: {} entr{} posted, {} skipped (already imported)",
            label,
            summary.entries_posted,
            if summary.entries_posted == 1 { "y" } else { "ies" },
            summary.skipped_duplicates
        );
    };

    match cmd {
        SquareCliCommands::Sales { file } => {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("read {}: {}", file.display(), e))?;
            let name = file.file_name().and_then(|s| s.to_str()).unwrap_or_default();
            let summary = square_commands::ingest_square_sales(store, "cli", &content, name)?;
            report(summary, "sales");
        }
        SquareCliCommands::Payroll { file } => {
            let path = file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 file path"))?;
            let summary = square_commands::ingest_square_payroll(store, "cli", path)?;
            report(summary, "payroll");
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

fn handle_bill_command(store: &mut EventStore, cmd: BillCliCommands) -> Result<()> {
    match cmd {
        BillCliCommands::Receive {
            vendor,
            amount,
            currency,
            date,
            terms,
            expense_account,
            ap_account,
            memo,
        } => {
            let issue_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("Invalid date format, use YYYY-MM-DD"))?;
            let amount_cents = (amount * 100.0).round() as i64;
            let payment_terms = PaymentTerms::parse(&terms);

            let mut cmds = BillCommandHandler::new(store, "cli-user".to_string());
            let stored = cmds.receive_bill(ReceiveBillCommand {
                vendor: vendor.clone(),
                amount: amount_cents,
                currency,
                issue_date,
                terms: payment_terms.clone(),
                memo,
                debit_account_id: expense_account,
                ap_account_id: ap_account,
                reference: None,
            })?;

            if let Event::BillReceived {
                bill_id, due_date, ..
            } = &stored.event
            {
                println!(
                    "Bill received: {} from {} for ${:.2} (due {})",
                    &bill_id[..8],
                    vendor,
                    amount,
                    due_date
                );
            }
        }
        BillCliCommands::Pay {
            bill_id,
            amount,
            date,
            payment_account,
            ap_account,
            memo,
        } => {
            let payment_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("Invalid date format, use YYYY-MM-DD"))?;
            let amount_cents = (amount * 100.0).round() as i64;

            let mut cmds = BillCommandHandler::new(store, "cli-user".to_string());
            cmds.apply_payment(ApplyBillPaymentCommand {
                bill_id: bill_id.clone(),
                payment_date,
                amount_applied: amount_cents,
                payment_account_id: payment_account,
                ap_account_id: ap_account,
                memo,
            })?;

            println!(
                "Payment of ${:.2} applied to bill {}",
                amount,
                &bill_id[..8.min(bill_id.len())]
            );
        }
        BillCliCommands::List { status } => {
            let queries = ApArQueries::new(store.connection());
            let bills = queries.list_bills(status.as_deref())?;

            if bills.is_empty() {
                println!("No bills found.");
                return Ok(());
            }

            println!(
                "{:<10} {:<20} {:>12} {:>12} {:>12} {:<8} {}",
                "Due Date", "Vendor", "Amount", "Paid", "Balance", "Status", "ID"
            );
            println!("{}", "-".repeat(90));
            for bill in &bills {
                let balance = bill.amount - bill.amount_paid;
                println!(
                    "{:<10} {:<20} {:>12} {:>12} {:>12} {:<8} {}",
                    bill.due_date,
                    truncate(&bill.vendor, 20),
                    format_amount(bill.amount),
                    format_amount(bill.amount_paid),
                    format_amount(balance),
                    bill.status,
                    &bill.id[..8.min(bill.id.len())],
                );
            }
        }
        BillCliCommands::Void { bill_id, reason } => {
            let mut cmds = BillCommandHandler::new(store, "cli-user".to_string());
            cmds.void_bill(VoidBillCommand {
                bill_id: bill_id.clone(),
                reason,
            })?;
            println!("Bill {} voided", &bill_id[..8.min(bill_id.len())]);
        }
        BillCliCommands::Aging => {
            let queries = ApArQueries::new(store.connection());
            let today = chrono::Local::now().date_naive();
            let aging = queries.ap_aging(today)?;

            println!("AP Aging Report (as of {})", today);
            println!("{}", "-".repeat(60));
            println!("  Current (not yet due): {}", format_amount(aging.current));
            println!("  1-30 days overdue:     {}", format_amount(aging.days_1_30));
            println!("  31-60 days overdue:    {}", format_amount(aging.days_31_60));
            println!("  61-90 days overdue:    {}", format_amount(aging.days_61_90));
            println!("  Over 90 days:          {}", format_amount(aging.days_over_90));
            println!("{}", "-".repeat(60));
            println!("  Total:                 {}", format_amount(aging.total));
        }
    }
    Ok(())
}

fn handle_invoice_command(store: &mut EventStore, cmd: InvoiceCliCommands) -> Result<()> {
    match cmd {
        InvoiceCliCommands::Issue {
            customer,
            amount,
            currency,
            date,
            terms,
            revenue_account,
            ar_account,
            memo,
        } => {
            let issue_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("Invalid date format, use YYYY-MM-DD"))?;
            let amount_cents = (amount * 100.0).round() as i64;
            let payment_terms = PaymentTerms::parse(&terms);

            let mut cmds = InvoiceCommandHandler::new(store, "cli-user".to_string());
            let stored = cmds.issue_invoice(IssueInvoiceCommand {
                customer: customer.clone(),
                amount: amount_cents,
                currency,
                issue_date,
                terms: payment_terms,
                memo,
                revenue_account_id: revenue_account,
                ar_account_id: ar_account,
            })?;

            if let Event::InvoiceIssued {
                invoice_id,
                due_date,
                ..
            } = &stored.event
            {
                println!(
                    "Invoice issued: {} to {} for ${:.2} (due {})",
                    &invoice_id[..8],
                    customer,
                    amount,
                    due_date
                );
            }
        }
        InvoiceCliCommands::ReceivePayment {
            invoice_id,
            amount,
            date,
            payment_account,
            ar_account,
            memo,
        } => {
            let payment_date = NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .map_err(|_| anyhow::anyhow!("Invalid date format, use YYYY-MM-DD"))?;
            let amount_cents = (amount * 100.0).round() as i64;

            let mut cmds = InvoiceCommandHandler::new(store, "cli-user".to_string());
            cmds.receive_payment(ReceiveInvoicePaymentCommand {
                invoice_id: invoice_id.clone(),
                payment_date,
                amount_applied: amount_cents,
                payment_account_id: payment_account,
                ar_account_id: ar_account,
                memo,
            })?;

            println!(
                "Payment of ${:.2} received on invoice {}",
                amount,
                &invoice_id[..8.min(invoice_id.len())]
            );
        }
        InvoiceCliCommands::List { status } => {
            let queries = ApArQueries::new(store.connection());
            let invoices = queries.list_invoices(status.as_deref())?;

            if invoices.is_empty() {
                println!("No invoices found.");
                return Ok(());
            }

            println!(
                "{:<10} {:<20} {:>12} {:>12} {:>12} {:<8} {}",
                "Due Date", "Customer", "Amount", "Received", "Balance", "Status", "ID"
            );
            println!("{}", "-".repeat(90));
            for inv in &invoices {
                let balance = inv.amount - inv.amount_paid;
                println!(
                    "{:<10} {:<20} {:>12} {:>12} {:>12} {:<8} {}",
                    inv.due_date,
                    truncate(&inv.customer, 20),
                    format_amount(inv.amount),
                    format_amount(inv.amount_paid),
                    format_amount(balance),
                    inv.status,
                    &inv.id[..8.min(inv.id.len())],
                );
            }
        }
        InvoiceCliCommands::Void { invoice_id, reason } => {
            let mut cmds = InvoiceCommandHandler::new(store, "cli-user".to_string());
            cmds.void_invoice(VoidInvoiceCommand {
                invoice_id: invoice_id.clone(),
                reason,
            })?;
            println!(
                "Invoice {} voided",
                &invoice_id[..8.min(invoice_id.len())]
            );
        }
        InvoiceCliCommands::Aging => {
            let queries = ApArQueries::new(store.connection());
            let today = chrono::Local::now().date_naive();
            let aging = queries.ar_aging(today)?;

            println!("AR Aging Report (as of {})", today);
            println!("{}", "-".repeat(60));
            println!("  Current (not yet due): {}", format_amount(aging.current));
            println!("  1-30 days overdue:     {}", format_amount(aging.days_1_30));
            println!("  31-60 days overdue:    {}", format_amount(aging.days_31_60));
            println!("  61-90 days overdue:    {}", format_amount(aging.days_61_90));
            println!("  Over 90 days:          {}", format_amount(aging.days_over_90));
            println!("{}", "-".repeat(60));
            println!("  Total:                 {}", format_amount(aging.total));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Partnership & tax forms
// ---------------------------------------------------------------------------

fn parse_cli_date(s: &str, what: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("{what} must be YYYY-MM-DD, got {s:?}"))
}

fn handle_partnership_command(
    store: &mut EventStore,
    cmd: PartnershipCliCommands,
) -> Result<()> {
    use accountir::commands::partnership_commands as pc;
    use accountir::domain::{Address, BusinessProfile, PartnerType, Residency, Shares};

    match cmd {
        PartnershipCliCommands::Profile {
            legal_name,
            street,
            suite,
            city,
            state,
            postal_code,
            country,
            ein,
            naics,
            started,
            activity,
            product,
        } => {
            let profile = BusinessProfile {
                legal_name,
                address: Address {
                    street,
                    suite,
                    city,
                    state,
                    postal_code,
                    country,
                },
                ein,
                naics_code: naics,
                formation_date: parse_cli_date(&started, "--started")?,
                principal_activity: activity,
                principal_product: product,
            };
            pc::set_profile(store, "cli-user", &profile)?;
            println!("Partnership details saved for {}", profile.legal_name);
        }

        PartnershipCliCommands::Show => {
            match pc::get_profile(store.connection()) {
                Some(p) => {
                    println!("{}", p.legal_name);
                    println!("  EIN:        {}", p.ein);
                    println!("  NAICS:      {}", p.naics_code);
                    println!("  Started:    {}", p.formation_date);
                    for line in p.address.as_block("").lines() {
                        println!("  {line}");
                    }
                }
                None => println!("No partnership details yet — run `partnership profile`."),
            }
            let partners = pc::list_partners(store.connection());
            println!("\n{} partner(s):", partners.len());
            for p in &partners {
                print_partner(store.connection(), p);
            }
        }

        PartnershipCliCommands::AddPartner {
            name,
            r#type,
            residency,
            entity_type,
            street,
            suite,
            city,
            state,
            postal_code,
            country,
            started,
            profit,
            loss,
            capital,
            tin,
        } => {
            let partner_type = PartnerType::parse(&r#type)
                .ok_or_else(|| anyhow::anyhow!("--type must be general or limited"))?;
            let residency = Residency::parse(&residency)
                .ok_or_else(|| anyhow::anyhow!("--residency must be domestic or foreign"))?;
            let start_date = started
                .as_deref()
                .map(|d| parse_cli_date(d, "--started"))
                .transpose()?;

            let cmd = pc::AdmitPartner {
                name,
                partner_type,
                residency,
                entity_type,
                address: Address {
                    street,
                    suite,
                    city,
                    state,
                    postal_code,
                    country,
                },
                start_date,
                // Loss and capital default to the profit share: equal is the
                // common case, and making somebody type it three times is how
                // they end up differing by a typo nobody notices until a K-1.
                shares: Shares::from_percents(
                    profit,
                    loss.unwrap_or(profit),
                    capital.unwrap_or(profit),
                ),
                tin,
            };
            let (id, _) = pc::admit_partner(store, "cli-user", &cmd)?;
            println!("Added partner {} ({})", cmd.name, id);
            report_share_totals(store.connection());
        }

        PartnershipCliCommands::Partners { year } => {
            let partners = match year {
                Some(y) => pc::partners_for_year(store.connection(), y),
                None => pc::list_partners(store.connection()),
            };
            if partners.is_empty() {
                println!("No partners.");
            }
            for p in &partners {
                print_partner(store.connection(), p);
            }
            report_share_totals(store.connection());
        }

        PartnershipCliCommands::RemovePartner { partner_id, on } => {
            let end = parse_cli_date(&on, "--on")?;
            pc::withdraw_partner(store, "cli-user", &partner_id, end)?;
            println!("Partner {partner_id} left on {end}");
        }

        PartnershipCliCommands::SetTin { partner_id, tin } => {
            pc::set_tin(store.connection(), &partner_id, &tin)?;
            println!("TIN stored on this machine only — it is not in the event log.");
        }
    }

    fn print_partner(conn: &rusqlite::Connection, p: &accountir::domain::Partner) {
        use accountir::commands::partnership_commands as pc;
        use accountir::domain::format_ppm;
        let tin = match pc::get_tin(conn, &p.partner_id) {
            Some(_) => "TIN on file",
            None => "no TIN on this machine",
        };
        let until = match p.end_date {
            Some(e) => format!(" until {e}"),
            None => String::new(),
        };
        println!(
            "  {}  {}\n     {} / {}, {} — from {}{}, {}",
            &p.partner_id[..8.min(p.partner_id.len())],
            p.name,
            p.partner_type.label(),
            p.residency.label(),
            p.entity_type,
            p.start_date,
            until,
            tin
        );
        println!(
            "     profit {}%  loss {}%  capital {}%",
            format_ppm(p.shares.profit_ppm),
            format_ppm(p.shares.loss_ppm),
            format_ppm(p.shares.capital_ppm)
        );
    }

    /// Say so the moment the shares stop adding up, rather than at filing time.
    fn report_share_totals(conn: &rusqlite::Connection) {
        use accountir::commands::partnership_commands as pc;
        use accountir::domain::Shares;
        let shares: Vec<Shares> = pc::list_partners(conn).iter().map(|p| p.shares).collect();
        if shares.is_empty() {
            return;
        }
        let totals = Shares::sums_to_whole(&shares);
        if !totals.is_whole() {
            println!("\nwarning: {}", totals.discrepancies().join(", "));
        }
    }

    Ok(())
}

fn handle_tax_command(store: &EventStore, cmd: TaxCliCommands) -> Result<()> {
    use accountir::commands::partnership_commands as pc;
    use accountir::tax::{PartnerFiling, ReturnRequest, build_return_from_ledger};

    match cmd {
        TaxCliCommands::Form1065 { year, output } => {
            let conn = store.connection();
            let profile = pc::get_profile(conn).ok_or_else(|| {
                anyhow::anyhow!(
                    "No partnership details yet. Run `accountir partnership profile ...` first."
                )
            })?;

            let partners: Vec<PartnerFiling> = pc::partners_for_year(conn, year)
                .into_iter()
                .map(|partner| PartnerFiling {
                    tin: pc::get_tin(conn, &partner.partner_id),
                    partner,
                })
                .collect();

            // The ledger entry point, not `build_return`: the latter fills identity
            // only and leaves every money line blank.
            let partner_count = partners.len();
            let bundle = build_return_from_ledger(
                conn,
                &ReturnRequest {
                    year,
                    profile,
                    partners,
                    schedule_b: accountir::tax::schedule_b::load(conn, year),
                    // Both left to `build_return_from_ledger`, which has the
                    // connection and reads them from the books.
                    schedule_l: None,
                    detail: Default::default(),
                },
            )?;

            std::fs::write(&output, &bundle.pdf)?;
            println!(
                "Wrote {} ({} pages, {} Schedule K-1s)",
                output.display(),
                bundle.page_count,
                partner_count
            );
            for w in &bundle.warnings {
                println!("warning: {w}");
            }
            println!(
                "\nThe form is prefilled but still editable. Income and deduction lines \
                 come from the books via the Form 1065 line mappings; Schedule K, the \
                 capital accounts, and Schedule B are deliberately left blank."
            );
        }
    }
    Ok(())
}
