use accountir::commands::account_commands::{AccountCommands, CreateAccountCommand};
use accountir::commands::bill_commands::{
    ApplyBillPaymentCommand, BillCommands as BillCommandHandler, ReceiveBillCommand,
    VoidBillCommand,
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

    /// Files attached to these books: statements received, notices, anything a
    /// return points at
    #[command(subcommand)]
    Document(DocumentCliCommands),

    /// Tax statements these books received (W-2s, 1099s, K-1s), box by box
    #[command(subcommand)]
    Statement(StatementCliCommands),

    /// Schedules K-1: packages from a partnership's books, and links that pull
    /// them into a partner's own books
    #[command(subcommand)]
    K1(K1CliCommands),
}

#[derive(Subcommand)]
enum DocumentCliCommands {
    /// Attach a file. Everyone with access to these books can open it
    Attach {
        file: PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// The tax year it belongs to
        #[arg(long)]
        year: Option<i32>,
        /// Which statement it is, by code — see `accountir statement forms`
        #[arg(long)]
        form: Option<String>,
    },
    /// List attached documents
    List {
        #[arg(long)]
        year: Option<i32>,
    },
    /// Write a document to a file, after checking its bytes against the log
    Export {
        /// The document id, or enough of its start to be unique
        id: String,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Take a document off the books. Its bytes are kept
    Remove { id: String },
    /// Check that every document's bytes are on this machine and intact
    Check,
}

#[derive(Subcommand)]
enum StatementCliCommands {
    /// List the statement forms, or one form's boxes and where each goes
    Forms { form: Option<String> },
    /// Record a statement typed in from the paper
    Record {
        #[arg(long)]
        year: i32,
        /// The form's code — see `accountir statement forms`
        #[arg(long)]
        form: String,
        /// Who sent it: the employer, the bank, the partnership
        #[arg(long)]
        issuer: String,
        /// A box and its amount in dollars, e.g. `--box 1=1234.56`. Repeat per box
        #[arg(long = "box", value_name = "CODE=AMOUNT")]
        boxes: Vec<String>,
        /// An attached document the statement was read from. Repeatable
        #[arg(long = "document")]
        documents: Vec<String>,
        #[arg(long)]
        note: Option<String>,
        /// Replace this statement instead of recording a new one
        #[arg(long)]
        id: Option<String>,
    },
    /// List recorded statements, box by box
    List {
        #[arg(long)]
        year: Option<i32>,
    },
    /// Remove a statement
    Remove { id: String },
    /// A tax year's inputs, gathered by where each amount goes on Form 1040
    Summary {
        #[arg(long)]
        year: i32,
    },
}

#[derive(Subcommand)]
enum K1CliCommands {
    /// In a partnership's books: each partner's K-1 as box amounts
    Package {
        #[arg(long)]
        year: i32,
        /// Only this partner, by id or name
        #[arg(long)]
        partner: Option<String>,
    },
    /// In a partner's own books: receive a partner's K-1 from a partnership's books
    Link {
        /// The partnership's database
        #[arg(long)]
        source: PathBuf,
        /// The partner, by id or name
        #[arg(long)]
        partner: String,
    },
    /// List the partnerships these books receive K-1s from
    Links {
        /// Also say whether that year's K-1 is pulled and still current
        #[arg(long)]
        year: Option<i32>,
    },
    /// Stop receiving K-1s through a link. K-1s already pulled stay
    Unlink { link: String },
    /// Pull a year's K-1s through every link, or one
    Pull {
        #[arg(long)]
        year: i32,
        /// One link, by id or partnership name
        #[arg(long)]
        link: Option<String>,
        /// The partnership's database, when this machine's registry does not
        /// know where it is. Pulls only the links to those books
        #[arg(long)]
        source: Option<PathBuf>,
    },
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

    /// Say that a ledger account holds a partner's capital.
    ///
    /// Item L on a Schedule K-1 — the partner's capital account analysis — is
    /// built from these links. Without them a K-1 shows the partner's share of
    /// this year's income and nothing else: no opening balance, no
    /// contributions, no draws.
    ///
    /// The role says which side of item L the account belongs on: "contribution"
    /// for money in (row 2), "draw" for money out (row 5). An account can be
    /// linked to one partner only; linking it again moves it.
    EquityLink {
        partner_id: String,
        /// The account's number, e.g. 4005, or its id
        account: String,
        /// "contribution" or "draw"
        #[arg(long, default_value = "contribution")]
        role: String,
    },

    /// Stop treating an account as a partner's capital
    EquityUnlink {
        partner_id: String,
        /// The account's number, e.g. 4005, or its id
        account: String,
    },

    /// Show which accounts hold each partner's capital, and which hold nobody's
    Equity,

    /// Record what a partner's percentages became, and from when.
    ///
    /// The partnership's split changes: somebody leaves, somebody is admitted,
    /// the agreement is renegotiated. Each change is recorded with the date it
    /// took effect, so a prior year's return keeps being built on the split that
    /// was in force during that year rather than on today's.
    SetShares {
        partner_id: String,
        /// The first day the new percentages apply, YYYY-MM-DD
        #[arg(long)]
        from: String,
        /// Share of profit, as a percentage
        #[arg(long)]
        profit: f64,
        /// Share of loss, as a percentage. Defaults to the profit share
        #[arg(long)]
        loss: Option<f64>,
        /// Share of capital, as a percentage. Defaults to the profit share
        #[arg(long)]
        capital: Option<f64>,
    },

    /// Show a partner's percentages over time
    ShareHistory {
        /// Only this partner. Omit for everybody
        partner_id: Option<String>,
    },

    /// Record that a partner has left
    RemovePartner {
        partner_id: String,
        /// The day they left, YYYY-MM-DD
        #[arg(long)]
        on: String,
    },

    /// Set a partner's TIN on this machine. Never written to the event log
    SetTin { partner_id: String, tin: String },

    /// Record a family tie between two partners, for Schedule B-1's §267(c)
    /// constructive-ownership test. Two spouses at 40% and 20% then each own 60%
    /// and both appear on Schedule B-1
    Relate {
        /// The first partner's id. For "parent_of", this is the parent
        partner_id: String,
        /// The second partner's id. For "parent_of", this is the child
        related_partner_id: String,
        /// "spouse", "sibling", or "parent_of"
        #[arg(long)]
        kind: String,
    },

    /// Remove a recorded family tie between two partners
    Unrelate {
        partner_id: String,
        related_partner_id: String,
    },

    /// List the recorded family ties between partners
    Relationships,

    /// Fix a partner's share of one year's ordinary income in dollars instead of
    /// by percentage — or make them the partner who takes the rest
    SetAllocation {
        partner_id: String,
        /// The tax year
        #[arg(long)]
        year: i32,
        /// Their share in dollars, e.g. 1843.56. Omit when using --remainder
        #[arg(long, allow_hyphen_values = true, conflicts_with = "remainder")]
        amount: Option<f64>,
        /// They take whatever the fixed amounts leave
        #[arg(long)]
        remainder: bool,
        /// The amount is taken first out of the year's income, and the rest is
        /// divided on the percentages
        #[arg(long, requires = "amount")]
        preferred: bool,
        /// Where the split comes from — required
        #[arg(long)]
        note: String,
    },

    /// Put a partner's share of a year back on their percentages
    ClearAllocation {
        partner_id: String,
        /// The tax year
        #[arg(long)]
        year: i32,
    },

    /// List the years whose income is divided in fixed amounts
    Allocations {
        /// Only this year
        #[arg(long)]
        year: Option<i32>,
    },

    /// Say how a liability account bears on K-1 item K
    ClassifyLiability {
        /// The liability account, by id or number
        account: String,
        /// nonrecourse, qualified-nonrecourse or recourse
        #[arg(long)]
        kind: String,
        /// For a recourse liability: the one partner who bears it. Omit to share
        /// it on the loss percentages
        #[arg(long)]
        partner: Option<String>,
        /// The partner guaranteed it (item K3)
        #[arg(long, requires = "partner")]
        guaranteed: bool,
        /// Where the classification comes from — required
        #[arg(long)]
        note: String,
    },

    /// Put a liability back on the entity's default classification
    ClearLiability {
        /// The liability account, by id or number
        account: String,
    },

    /// List the liabilities classified for item K
    Liabilities,
}

#[derive(Subcommand)]
enum TaxCliCommands {
    /// Show or set which return these books file: partnership (Form 1065),
    /// sole_proprietorship (Schedule C), or individual (Form 1040, personal books)
    BusinessType { kind: Option<String> },

    /// Build Form 1065 with a Schedule K-1 per partner, as one fillable PDF
    Form1065 {
        /// Tax year to file
        #[arg(long)]
        year: i32,
        /// Where to write the PDF
        #[arg(short, long)]
        output: PathBuf,
    },

    /// Build the Illinois IL-1065 Partnership Replacement Tax Return, as one
    /// fillable PDF (with Illinois Schedule B)
    #[command(name = "il-1065")]
    Il1065 {
        /// Tax year to file
        #[arg(long)]
        year: i32,
        /// Where to write the PDF
        #[arg(short, long)]
        output: PathBuf,
    },

    /// Show or change the Illinois IL-1065 settings for this book. With no flags,
    /// prints the current settings
    IlSettings {
        /// Whether any income is earned outside Illinois (apportion). Omit to leave
        /// unchanged
        #[arg(long)]
        apportion_outside: Option<bool>,
        /// Whether the partnership elected the 4.95% Pass-through Entity tax. Omit
        /// to leave unchanged
        #[arg(long)]
        elect_pte: Option<bool>,
    },

    /// Mark an account as Illinois income or replacement tax, so IL-1065 line 16
    /// adds back what the federal return deducts from it
    IlTaxAddback {
        /// The account, by id or number
        account: String,
        /// The first tax year this applies to
        #[arg(long)]
        from: i32,
        /// Stop adding it back from that year instead
        #[arg(long)]
        stop: bool,
    },
}

#[derive(Subcommand)]
enum AmazonCliCommands {
    /// Import an Amazon Business "Order History Report" CSV. Posts one entry per
    /// shipment, clearing the mapped `amazon_clearing` account. Idempotent.
    Orders {
        /// Path to the order history CSV, e.g. orders_from_20250529_to_20260629_*.csv
        file: PathBuf,
    },
    /// Pair the Amazon clearing account's card charges against the imported
    /// orders, and list what neither side can explain.
    Reconcile {
        /// Show every unmatched line rather than the first few of each kind.
        #[arg(long)]
        all: bool,
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
        /// Id of the account this one sits under
        #[arg(short, long)]
        parent: Option<String>,
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
            let mut store = EventStore::open(&cli.database)?;
            // The tax commands read and write the tax setup tables, which newer
            // migrations add — as the partnership commands do.
            accountir::store::migrations::run_migrations(store.connection())?;
            handle_tax_command(&mut store, cmd)?;
        }

        Commands::Document(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            accountir::store::migrations::run_migrations(store.connection())?;
            handle_document_command(&mut store, cmd)?;
        }

        Commands::Statement(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            accountir::store::migrations::run_migrations(store.connection())?;
            handle_statement_command(&mut store, cmd)?;
        }

        Commands::K1(cmd) => {
            let mut store = EventStore::open(&cli.database)?;
            accountir::store::migrations::run_migrations(store.connection())?;
            handle_k1_command(&mut store, cmd)?;
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
            if s.restated_payments > 0 {
                println!(
                    "  {} payment(s) Amazon had already reported under another reference — \
                     dropped, which is what makes those orders foot to their net total",
                    s.restated_payments
                );
            }
            if s.reconciled_charges > 0 {
                println!(
                    "  ⚠ {} charge(s) had line items that didn't foot to what was paid — \
                     review the 'reconciling difference' lines",
                    s.reconciled_charges
                );
            }
        }
        AmazonCliCommands::Reconcile { all } => {
            print_amazon_reconciliation(store, all);
        }
    }

    Ok(())
}

/// Print the clearing-account reconciliation: what pairs, what does not, and
/// which card each order was settled on.
fn print_amazon_reconciliation(store: &EventStore, all: bool) {
    use accountir::commands::amazon_reconcile::{reconcile_amazon, ClearingLine};

    let r = reconcile_amazon(store.connection());
    if r.missing_mapping {
        println!(
            "No `amazon_clearing` account mapping is set, so there is nothing to \
             reconcile against. Set it on the Imports page first."
        );
        return;
    }

    let dollars = |c: i64| format!("${:.2}", c as f64 / 100.0);
    println!(
        "Amazon clearing: {} charge(s) paired ({}), balance {}",
        r.matched,
        dollars(r.matched_cents),
        dollars(r.clearing_balance_cents)
    );

    let show = |title: &str, note: &str, lines: &[ClearingLine], total: i64| {
        if lines.is_empty() {
            println!("\n{title}: none");
            return;
        }
        println!("\n{} — {} line(s), {}", title, lines.len(), dollars(total));
        println!("  {note}");
        let limit = if all { lines.len() } else { 15 };
        for l in lines.iter().take(limit) {
            let memo: String = l.memo.chars().take(58).collect();
            println!("    {}  {:>10}  {}", l.date, dollars(l.amount_cents), memo);
        }
        if lines.len() > limit {
            println!(
                "    … and {} more (--all to list them)",
                lines.len() - limit
            );
        }
    };

    show(
        "Card charges with no order",
        "Amazon charged the card for something the Business account never ordered: \
         a personal Amazon account paid with the business card, or a charge that is \
         not an order at all (Prime, AWS, a subscription).",
        &r.charges_without_orders,
        r.charges_without_orders_cents(),
    );
    show(
        "Orders with no card charge",
        "The Business account ordered it and something paid for it, but no card feed \
         in these books shows the money leaving: a personal card on the business \
         Amazon account, or a statement never imported.",
        &r.orders_without_charges,
        r.orders_without_charges_cents(),
    );

    if !r.cards.is_empty() {
        println!("\nPayment instruments across the imported orders:");
        for c in &r.cards {
            println!(
                "  {:<26} {:>4} charge(s)  {:>11}   {} → {}{}",
                c.card,
                c.charges,
                dollars(c.total_cents),
                c.first,
                c.last,
                if c.unmatched > 0 {
                    format!("   ⚠ {} unpaid by any card feed", c.unmatched)
                } else {
                    String::new()
                }
            );
        }
    }
}

fn handle_square_command(store: &mut EventStore, cmd: SquareCliCommands) -> Result<()> {
    use accountir::commands::square_commands;

    let report = |summary: square_commands::SquareImportSummary, label: &str| {
        println!(
            "Square {}: {} entr{} posted, {} skipped (already imported)",
            label,
            summary.entries_posted,
            if summary.entries_posted == 1 {
                "y"
            } else {
                "ies"
            },
            summary.skipped_duplicates
        );
    };

    match cmd {
        SquareCliCommands::Sales { file } => {
            let content = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("read {}: {}", file.display(), e))?;
            let name = file
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
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
            parent,
            currency,
            description,
        } => {
            let acc_type = parse_account_type(&account_type)?;
            let mut commands = AccountCommands::new(store, "cli-user".to_string());

            let event = commands.create_account(CreateAccountCommand {
                account_type: acc_type,
                account_number: number,
                name: name.clone(),
                parent_id: parent,
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
            println!(
                "  1-30 days overdue:     {}",
                format_amount(aging.days_1_30)
            );
            println!(
                "  31-60 days overdue:    {}",
                format_amount(aging.days_31_60)
            );
            println!(
                "  61-90 days overdue:    {}",
                format_amount(aging.days_61_90)
            );
            println!(
                "  Over 90 days:          {}",
                format_amount(aging.days_over_90)
            );
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
            println!("Invoice {} voided", &invoice_id[..8.min(invoice_id.len())]);
        }
        InvoiceCliCommands::Aging => {
            let queries = ApArQueries::new(store.connection());
            let today = chrono::Local::now().date_naive();
            let aging = queries.ar_aging(today)?;

            println!("AR Aging Report (as of {})", today);
            println!("{}", "-".repeat(60));
            println!("  Current (not yet due): {}", format_amount(aging.current));
            println!(
                "  1-30 days overdue:     {}",
                format_amount(aging.days_1_30)
            );
            println!(
                "  31-60 days overdue:    {}",
                format_amount(aging.days_31_60)
            );
            println!(
                "  61-90 days overdue:    {}",
                format_amount(aging.days_61_90)
            );
            println!(
                "  Over 90 days:          {}",
                format_amount(aging.days_over_90)
            );
            println!("{}", "-".repeat(60));
            println!("  Total:                 {}", format_amount(aging.total));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Partnership & tax forms
// ---------------------------------------------------------------------------

/// Find an account by its number or its id.
///
/// Numbers are what people have in front of them — 4005 is written on the chart
/// of accounts and the id is a UUID nobody has memorised — so the number is
/// accepted first and the id still works for scripts.
fn resolve_account(conn: &rusqlite::Connection, needle: &str) -> Result<String> {
    let accounts = AccountQueries::new(conn).get_all_accounts()?;
    if let Some(a) = accounts.iter().find(|a| a.account_number == needle) {
        return Ok(a.id.clone());
    }
    if let Some(a) = accounts.iter().find(|a| a.id == needle) {
        return Ok(a.id.clone());
    }
    // Named rather than a bare "not found": the usual cause is a number typed
    // from memory, and the near misses are the useful part of the answer.
    let close: Vec<String> = accounts
        .iter()
        .filter(|a| a.account_number.starts_with(needle.get(..1).unwrap_or("")))
        .take(8)
        .map(|a| format!("{} {}", a.account_number, a.name))
        .collect();
    anyhow::bail!(
        "No account {needle:?}.{}",
        if close.is_empty() {
            String::new()
        } else {
            format!(" Did you mean one of: {}?", close.join(", "))
        }
    )
}

fn parse_cli_date(s: &str, what: &str) -> Result<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|_| anyhow::anyhow!("{what} must be YYYY-MM-DD, got {s:?}"))
}

/// A liability account's id, from its id or its number.
fn account_id_from(store: &EventStore, account: &str) -> anyhow::Result<String> {
    store
        .connection()
        .query_row(
            "SELECT id FROM accounts WHERE id = ?1 OR account_number = ?1",
            [account],
            |r| r.get(0),
        )
        .map_err(|_| anyhow::anyhow!("no account {account}"))
}

fn handle_partnership_command(store: &mut EventStore, cmd: PartnershipCliCommands) -> Result<()> {
    use accountir::commands::partnership_commands as pc;
    use accountir::commands::share_period_commands as spc;
    use accountir::domain::format_ppm;
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
            report_share_totals(store.connection(), None);
        }

        PartnershipCliCommands::Partners { year } => {
            let partners = match year {
                Some(y) => accountir::commands::share_period_commands::partners_for_year(
                    store.connection(),
                    y,
                ),
                None => pc::list_partners(store.connection()),
            };
            if partners.is_empty() {
                println!("No partners.");
            }
            for p in &partners {
                print_partner(store.connection(), p);
            }
            report_share_totals(store.connection(), year);
        }

        PartnershipCliCommands::EquityLink {
            partner_id,
            account,
            role,
        } => {
            let account_id = resolve_account(store.connection(), &account)?;
            pc::link_equity_account(store, "cli-user", &partner_id, &account_id, &role)?;
            println!("{account} now holds {partner_id}'s capital, as a {role}.");
        }

        PartnershipCliCommands::EquityUnlink {
            partner_id,
            account,
        } => {
            let account_id = resolve_account(store.connection(), &account)?;
            pc::unlink_equity_account(store, "cli-user", &partner_id, &account_id)?;
            println!("{account} no longer holds {partner_id}'s capital.");
        }

        PartnershipCliCommands::Equity => {
            let conn = store.connection();
            let links = accountir::tax::capital::load_partner_equity_accounts(conn);
            let accounts = AccountQueries::new(conn)
                .get_all_accounts()
                .unwrap_or_default();
            let named = |id: &str| {
                accounts
                    .iter()
                    .find(|a| a.id == id)
                    .map(|a| format!("{} {}", a.account_number, a.name))
                    .unwrap_or_else(|| id.to_string())
            };
            for p in pc::list_partners(conn) {
                println!("{} ({})", p.name, p.partner_id);
                let mine: Vec<_> = links
                    .iter()
                    .filter(|l| l.partner_id == p.partner_id)
                    .collect();
                if mine.is_empty() {
                    println!("  no accounts linked — item L on their K-1 will show only their");
                    println!("  share of this year's income, with no opening balance or draws");
                }
                for l in mine {
                    println!("  {:<12} {}", l.role.as_str(), named(&l.account_id));
                }
            }
            // The accounts nobody claims are the point of the listing: an equity
            // account with a balance and no partner behind it is capital missing
            // from somebody's item L, and it is invisible until it is named.
            let orphans: Vec<&accountir::domain::Account> = accounts
                .iter()
                .filter(|a| a.account_type == accountir::domain::AccountType::Equity)
                .filter(|a| !links.iter().any(|l| l.account_id == a.id))
                .collect();
            if !orphans.is_empty() {
                println!("\nEquity accounts linked to nobody:");
                for a in orphans {
                    println!("  {} {}", a.account_number, a.name);
                }
                println!("Anything here that is a partner's capital is missing from their item L.");
            }
        }

        PartnershipCliCommands::SetShares {
            partner_id,
            from,
            profit,
            loss,
            capital,
        } => {
            let effective_from = parse_cli_date(&from, "--from")?;
            let shares = accountir::domain::Shares::from_percents(
                profit,
                loss.unwrap_or(profit),
                capital.unwrap_or(profit),
            );
            // Said before the write, because the point is to let somebody stop.
            if let Some(w) = spc::retrospective_warning(store.connection(), effective_from) {
                eprintln!("warning: {w}");
            }
            spc::set_partner_shares(store, "cli-user", &partner_id, effective_from, shares)?;
            println!(
                "From {effective_from}, {partner_id} takes {} of profit, {} of loss, {} of capital.",
                format_ppm(shares.profit_ppm),
                format_ppm(shares.loss_ppm),
                format_ppm(shares.capital_ppm)
            );
            // The split has to add up on the day it changed, not across a list
            // that mixes partners who were never in the partnership at once.
            let partners = spc::list_partners_with_history(store.connection());
            for problem in spc::problems_on(&partners, effective_from) {
                eprintln!("warning: {problem}");
            }
        }

        PartnershipCliCommands::ShareHistory { partner_id } => {
            let partners = spc::list_partners_with_history(store.connection());
            for p in partners
                .iter()
                .filter(|p| partner_id.as_ref().is_none_or(|id| &p.partner_id == id))
            {
                println!("{} ({})", p.name, p.partner_id);
                if p.history.is_empty() {
                    println!(
                        "  no recorded changes — treated as {} / {} / {} for every year",
                        format_ppm(p.shares.profit_ppm),
                        format_ppm(p.shares.loss_ppm),
                        format_ppm(p.shares.capital_ppm)
                    );
                }
                for period in &p.history {
                    println!(
                        "  from {}: {} / {} / {}",
                        period.effective_from,
                        format_ppm(period.shares.profit_ppm),
                        format_ppm(period.shares.loss_ppm),
                        format_ppm(period.shares.capital_ppm)
                    );
                }
            }
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

        PartnershipCliCommands::Relate {
            partner_id,
            related_partner_id,
            kind,
        } => {
            let kind = accountir::domain::RelationshipKind::parse(&kind)
                .ok_or_else(|| anyhow::anyhow!("--kind must be spouse, sibling, or parent_of"))?;
            pc::set_relationship(store, "cli-user", &partner_id, &related_partner_id, kind)?;
            println!(
                "Recorded: {partner_id} is {} {related_partner_id}",
                kind.label()
            );
        }

        PartnershipCliCommands::Unrelate {
            partner_id,
            related_partner_id,
        } => {
            pc::clear_relationship(store, "cli-user", &partner_id, &related_partner_id)?;
            println!("Removed the relationship between {partner_id} and {related_partner_id}.");
        }

        PartnershipCliCommands::SetAllocation {
            partner_id,
            year,
            amount,
            remainder,
            preferred,
            note,
        } => {
            let amount_cents = match (amount, remainder) {
                (Some(a), false) if a.is_finite() => Some((a * 100.0).round() as i64),
                (None, true) => None,
                _ => Err(anyhow::anyhow!("give either --amount or --remainder"))?,
            };
            pc::set_fixed_allocation(
                store,
                "cli-user",
                year,
                &partner_id,
                amount_cents,
                preferred,
                &note,
            )?;
            match amount_cents {
                Some(c) if preferred => println!(
                    "{partner_id} takes the first ${:.2} of {year}'s ordinary income, then a \
                     percentage share of the rest.",
                    c as f64 / 100.0
                ),
                Some(c) => println!(
                    "{partner_id} takes ${:.2} of {year}'s ordinary income.",
                    c as f64 / 100.0
                ),
                None => println!("{partner_id} takes what the fixed amounts leave of {year}."),
            }
        }

        PartnershipCliCommands::ClearAllocation { partner_id, year } => {
            pc::clear_fixed_allocation(store, "cli-user", year, &partner_id)?;
            println!("{partner_id}'s share of {year} is back on their percentages.");
        }

        PartnershipCliCommands::Allocations { year } => {
            let rows: Vec<_> = pc::list_fixed_allocations(store.connection())
                .into_iter()
                .filter(|f| year.is_none_or(|y| y == f.tax_year))
                .collect();
            if rows.is_empty() {
                println!("No year is divided in fixed amounts.");
            }
            for f in &rows {
                let name = pc::get_partner(store.connection(), &f.partner_id)
                    .map(|p| p.name)
                    .unwrap_or_else(|| f.partner_id.clone());
                match f.amount_cents {
                    Some(c) if f.preferred => println!(
                        "  {}  {name}: the first ${:.2}, then by percentage — {}",
                        f.tax_year,
                        c as f64 / 100.0,
                        f.note
                    ),
                    Some(c) => println!(
                        "  {}  {name}: ${:.2} — {}",
                        f.tax_year,
                        c as f64 / 100.0,
                        f.note
                    ),
                    None => println!("  {}  {name}: the remainder — {}", f.tax_year, f.note),
                }
            }
        }

        PartnershipCliCommands::ClassifyLiability {
            account,
            kind,
            partner,
            guaranteed,
            note,
        } => {
            let account_id = account_id_from(store, &account)?;
            let kind = accountir::domain::LiabilityKind::parse(&kind).ok_or_else(|| {
                anyhow::anyhow!("--kind is nonrecourse, qualified-nonrecourse or recourse")
            })?;
            pc::set_liability_class(
                store,
                "cli-user",
                &account_id,
                kind,
                partner.as_deref(),
                guaranteed,
                &note,
            )?;
            println!("{account} is {} for item K.", kind.label());
        }

        PartnershipCliCommands::ClearLiability { account } => {
            let account_id = account_id_from(store, &account)?;
            pc::clear_liability_class(store, "cli-user", &account_id)?;
            println!("{account} is back on the entity's default classification.");
        }

        PartnershipCliCommands::Liabilities => {
            let rows = pc::list_liability_classes(store.connection());
            if rows.is_empty() {
                println!("No liability is classified; every one takes the entity's default.");
            }
            for c in &rows {
                let label: String = store
                    .connection()
                    .query_row(
                        "SELECT account_number || ' ' || name FROM accounts WHERE id = ?1",
                        [&c.account_id],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| c.account_id.clone());
                let bearer = c
                    .partner_id
                    .as_deref()
                    .map(|id| {
                        let name = pc::get_partner(store.connection(), id)
                            .map(|p| p.name)
                            .unwrap_or_else(|| id.to_string());
                        format!(
                            " to {name}{}",
                            if c.guaranteed { ", guaranteed" } else { "" }
                        )
                    })
                    .unwrap_or_default();
                println!("  {label}: {}{bearer} — {}", c.kind.label(), c.note);
            }
        }

        PartnershipCliCommands::Relationships => {
            let rels = pc::list_relationships(store.connection());
            if rels.is_empty() {
                println!("No relationships recorded.");
            }
            for r in &rels {
                let name = |id: &str| {
                    pc::get_partner(store.connection(), id)
                        .map(|p| p.name)
                        .unwrap_or_else(|| id.to_string())
                };
                println!(
                    "  {} is {} {}",
                    name(&r.partner_id),
                    r.kind.label(),
                    name(&r.related_partner_id)
                );
            }
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
    ///
    /// # Why this asks about a day
    ///
    /// It used to sum the current percentages across every partner ever
    /// recorded, which meant that from the day somebody left, this fired
    /// permanently and could not be cleared — their percentages are still on
    /// file, so the sum counted a partner who was gone. It also described a
    /// different set of partners from the list printed above it whenever
    /// `--year` was given. The split has to add up on each day it was in force,
    /// and that is a question with an answer.
    fn report_share_totals(conn: &rusqlite::Connection, year: Option<i32>) {
        use accountir::commands::share_period_commands as spc;
        let partners = spc::list_partners_with_history(conn);
        if partners.is_empty() {
            return;
        }
        let days = match year {
            Some(y) => {
                let (start, end) = accountir::commands::partnership_commands::calendar_year(y);
                spc::days_to_check(&partners, start, end)
            }
            None => vec![chrono::Local::now().date_naive()],
        };
        let mut said: Vec<String> = Vec::new();
        for day in days {
            for problem in spc::problems_on(&partners, day) {
                if !said.contains(&problem) {
                    said.push(problem.clone());
                    println!("\nwarning: {problem}");
                }
            }
        }
    }

    Ok(())
}

/// Dollars as typed — `1234.56`, `1,234`, `$12.5`, `-40`, `(40.00)` — in cents.
fn parse_dollars_to_cents(s: &str) -> Result<i64> {
    let t = s.trim().replace([',', '$'], "");
    let (negative, t) = if let Some(rest) = t.strip_prefix('-') {
        (true, rest.to_string())
    } else if let Some(rest) = t.strip_prefix('(').and_then(|r| r.strip_suffix(')')) {
        (true, rest.to_string())
    } else {
        (false, t)
    };
    let (whole, fraction) = t.split_once('.').unwrap_or((t.as_str(), ""));
    let digits = |p: &str| p.chars().all(|c| c.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) || fraction.len() > 2 {
        anyhow::bail!("{s:?} is not a dollar amount");
    }
    let whole: i64 = if whole.is_empty() { 0 } else { whole.parse()? };
    let fraction: i64 = format!("{fraction:0<2}").parse()?;
    let cents = whole
        .checked_mul(100)
        .and_then(|w| w.checked_add(fraction))
        .ok_or_else(|| anyhow::anyhow!("{s:?} is too large"))?;
    Ok(if negative { -cents } else { cents })
}

fn show_cents(cents: i64) -> String {
    format!(
        "{}{}.{:02}",
        if cents < 0 { "-" } else { "" },
        cents.abs() / 100,
        cents.abs() % 100
    )
}

/// The one id among `ids` that starts with `needle`.
fn unique_prefix(ids: impl IntoIterator<Item = String>, needle: &str, what: &str) -> Result<String> {
    let matches: Vec<String> = ids.into_iter().filter(|id| id.starts_with(needle)).collect();
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!("no {what} {needle:?}"),
        _ => anyhow::bail!(
            "{needle:?} matches {} {what}s — give more of the id",
            matches.len()
        ),
    }
}

fn resolve_document(conn: &rusqlite::Connection, needle: &str) -> Result<String> {
    let ids = accountir::commands::document_commands::list(conn, None)
        .into_iter()
        .map(|d| d.document_id);
    unique_prefix(ids, needle, "document")
}

fn resolve_statement(conn: &rusqlite::Connection, needle: &str) -> Result<String> {
    let ids = accountir::commands::tax_statement_commands::list(conn, None)
        .into_iter()
        .map(|s| s.statement_id);
    unique_prefix(ids, needle, "statement")
}

fn parse_form(code: &str) -> Result<accountir::tax::information_returns::FormKind> {
    accountir::tax::information_returns::FormKind::parse(code).ok_or_else(|| {
        anyhow::anyhow!("{code:?} is not a statement form — see `accountir statement forms`")
    })
}

fn handle_document_command(store: &mut EventStore, cmd: DocumentCliCommands) -> Result<()> {
    use accountir::commands::document_commands as dc;
    use accountir::documents::BlobStore;

    match cmd {
        DocumentCliCommands::Attach {
            file,
            title,
            year,
            form,
        } => {
            let form = form.as_deref().map(parse_form).transpose()?;
            let doc = dc::attach_file(store, "cli-user", &file, title, year, form)?;
            println!(
                "Attached {} ({} bytes, {}) as {}",
                doc.filename, doc.size_bytes, doc.media_type, doc.document_id
            );
            println!("sha256 {}", doc.sha256);
            let kind =
                accountir::commands::sole_proprietor_commands::business_type(store.connection());
            if !kind.is_individual() {
                println!(
                    "\nnote: these are {} books, and everyone with access to them can open this \
                     document. A personal statement belongs in personal books.",
                    kind.label().to_lowercase()
                );
            }
        }
        DocumentCliCommands::List { year } => {
            let conn = store.connection();
            let docs = dc::list(conn, year);
            if docs.is_empty() {
                println!("No documents.");
            }
            let blobs = dc::local_store(conn).ok();
            for d in docs {
                let here = blobs.as_ref().is_some_and(|b| b.contains(&d.sha256));
                println!(
                    "{}  {}  {:>9} B  {}{}{}{}",
                    d.document_id,
                    d.attached_at.format("%Y-%m-%d"),
                    d.size_bytes,
                    d.filename,
                    d.tax_year.map(|y| format!("  {y}")).unwrap_or_default(),
                    d.form.map(|f| format!("  {}", f.label())).unwrap_or_default(),
                    if here { "" } else { "  (not on this machine)" }
                );
                if let Some(title) = &d.title {
                    println!("    {title}");
                }
            }
        }
        DocumentCliCommands::Export { id, output } => {
            let conn = store.connection();
            let id = resolve_document(conn, &id)?;
            let blobs = dc::local_store(conn)?;
            let bytes = dc::read(conn, &blobs, &id)?;
            std::fs::write(&output, &bytes)?;
            println!(
                "Wrote {} ({} bytes, checked against the log)",
                output.display(),
                bytes.len()
            );
        }
        DocumentCliCommands::Remove { id } => {
            let id = resolve_document(store.connection(), &id)?;
            dc::remove(store, "cli-user", &id)?;
            println!("Removed {id}. Its bytes are kept.");
        }
        DocumentCliCommands::Check => {
            let conn = store.connection();
            let blobs = dc::local_store(conn)?;
            let results = dc::check(conn, &blobs);
            let mut intact = 0;
            for (d, availability) in &results {
                match availability {
                    dc::Availability::Present => intact += 1,
                    dc::Availability::Missing => {
                        println!("missing  {}  {}", d.document_id, d.filename)
                    }
                    dc::Availability::Damaged(why) => {
                        println!("damaged  {}  {}: {why}", d.document_id, d.filename)
                    }
                }
            }
            println!(
                "{} document(s), {intact} on this machine and intact. Stored under {}",
                results.len(),
                blobs.root().display()
            );
        }
    }
    Ok(())
}

fn handle_statement_command(store: &mut EventStore, cmd: StatementCliCommands) -> Result<()> {
    use accountir::commands::tax_statement_commands as tsc;
    use accountir::domain::documents::{StatementSource, TaxStatement};
    use accountir::tax::information_returns::FormKind;

    match cmd {
        StatementCliCommands::Forms { form: None } => {
            for kind in FormKind::ALL {
                println!(
                    "{:18} {}{}",
                    kind.as_str(),
                    kind.label(),
                    if kind.is_open() { "  (any box code)" } else { "" }
                );
            }
        }
        StatementCliCommands::Forms { form: Some(code) } => {
            let kind = parse_form(&code)?;
            println!("{}", kind.label());
            if kind.is_open() {
                println!("  No box catalogue yet — any code of letters, digits and underscores.");
            }
            for b in kind.boxes() {
                println!("  {:34} {}", b.code, b.label);
                println!(
                    "  {:34} → {}{}",
                    "",
                    b.destination,
                    if b.summed { "" } else { "  (shown, not added)" }
                );
            }
        }
        StatementCliCommands::Record {
            year,
            form,
            issuer,
            boxes,
            documents,
            note,
            id,
        } => {
            let form = parse_form(&form)?;
            let mut amounts = std::collections::BTreeMap::new();
            for b in &boxes {
                let (code, amount) = b
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--box takes CODE=AMOUNT, got {b:?}"))?;
                let code = code.trim().to_string();
                if amounts
                    .insert(code.clone(), parse_dollars_to_cents(amount)?)
                    .is_some()
                {
                    anyhow::bail!("box {code} was given twice");
                }
            }
            let conn = store.connection();
            let document_ids = documents
                .iter()
                .map(|d| resolve_document(conn, d))
                .collect::<Result<Vec<_>>>()?;
            let statement_id = match id {
                Some(id) => resolve_statement(conn, &id)?,
                None => tsc::new_statement_id(),
            };
            let statement = TaxStatement {
                statement_id,
                tax_year: year,
                form,
                issuer,
                amounts,
                document_ids,
                source: StatementSource::Entered,
                note,
            };
            tsc::record(store, "cli-user", &statement)?;
            println!(
                "Recorded {year} {} from {} as {}",
                form.label(),
                statement.issuer.trim(),
                statement.statement_id
            );
        }
        StatementCliCommands::List { year } => {
            let statements = tsc::list(store.connection(), year);
            if statements.is_empty() {
                println!("No statements.");
            }
            for s in statements {
                let source = match &s.source {
                    StatementSource::Entered => "entered".to_string(),
                    StatementSource::Ledger(p) => format!(
                        "pulled from {}'s books, through event {}",
                        p.ledger_name, p.through_event
                    ),
                };
                println!(
                    "{}  {}  {}  {}  ({source})",
                    s.statement_id,
                    s.tax_year,
                    s.form.label(),
                    s.issuer
                );
                for (code, cents) in &s.amounts {
                    let label = s.form.box_def(code).map_or("", |d| d.label);
                    println!("  {code:>34}  {:>14}  {label}", show_cents(*cents));
                }
                if !s.document_ids.is_empty() {
                    println!("  documents: {}", s.document_ids.join(", "));
                }
                if let Some(note) = &s.note {
                    println!("  note: {note}");
                }
            }
        }
        StatementCliCommands::Remove { id } => {
            let id = resolve_statement(store.connection(), &id)?;
            tsc::remove(store, "cli-user", &id)?;
            println!("Removed {id}.");
        }
        StatementCliCommands::Summary { year } => {
            let inputs = accountir::tax::personal::inputs_for_year(store.connection(), year);
            println!("{year}: {} statement(s)\n", inputs.statements.len());
            for d in &inputs.destinations {
                println!("{:>14}  {}", show_cents(d.cents), d.destination);
                for c in &d.contributions {
                    println!(
                        "{:>14}      {} box {}, {}",
                        show_cents(c.cents),
                        c.form.label(),
                        c.box_code,
                        c.issuer
                    );
                }
            }
            if !inputs.informational.is_empty() {
                println!("\nShown, not added:");
                for c in &inputs.informational {
                    println!(
                        "{:>14}  {} box {} ({}), {}",
                        show_cents(c.cents),
                        c.form.label(),
                        c.box_code,
                        c.label,
                        c.issuer
                    );
                }
            }
            if !inputs.unrouted.is_empty() {
                println!("\nOn forms with no box catalogue yet, so not gathered:");
                for c in &inputs.unrouted {
                    println!(
                        "{:>14}  {} box {}, {}",
                        show_cents(c.cents),
                        c.form.label(),
                        c.box_code,
                        c.issuer
                    );
                }
            }
            for link in &inputs.missing_k1s {
                println!(
                    "\nmissing: no {year} K-1 from {} for {} — `accountir k1 pull --year {year}`",
                    link.ledger_name, link.partner_name
                );
            }
            for d in &inputs.unread_documents {
                println!(
                    "unread: {} ({}) — attached for {year}, but no statement was recorded from it",
                    d.filename, d.document_id
                );
            }
        }
    }
    Ok(())
}

/// Open another set of books by path, refusing to create one that is not there.
fn open_existing_books(path: &std::path::Path) -> Result<EventStore> {
    if !path.is_file() {
        anyhow::bail!("{} is not a database file", path.display());
    }
    let store = EventStore::open(path)?;
    accountir::store::migrations::run_migrations(store.connection())?;
    Ok(store)
}

/// Where a linked partnership's books are on this machine, by ledger id.
fn locate_linked_books(link: &accountir::domain::documents::K1Link) -> Result<PathBuf> {
    let registry = accountir::registry::Registry::open_default()?;
    registry
        .find_by_ledger_id(&link.ledger_id)?
        .map(|b| b.db_path)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{}'s books are not registered on this machine — pass --source <its database>",
                link.ledger_name
            )
        })
}

fn resolve_link(
    conn: &rusqlite::Connection,
    needle: &str,
) -> Result<accountir::domain::documents::K1Link> {
    let lowered = needle.to_lowercase();
    let matches: Vec<_> = accountir::commands::tax_statement_commands::list_k1_links(conn)
        .into_iter()
        .filter(|l| l.link_id.starts_with(needle) || l.ledger_name.to_lowercase().contains(&lowered))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("exactly one")),
        0 => anyhow::bail!("no K-1 link {needle:?}"),
        n => anyhow::bail!("{needle:?} matches {n} K-1 links — use the link id"),
    }
}

fn handle_k1_command(store: &mut EventStore, cmd: K1CliCommands) -> Result<()> {
    use accountir::commands::tax_statement_commands as tsc;
    use accountir::tax::information_returns::FormKind;

    match cmd {
        K1CliCommands::Package { year, partner } => {
            let mut packages = accountir::tax::k1_package::for_year(store.connection(), year)?;
            if let Some(needle) = &partner {
                let lowered = needle.to_lowercase();
                packages.retain(|k| {
                    k.partner_id.starts_with(needle.as_str()) || k.partner_name.to_lowercase() == lowered
                });
                if packages.is_empty() {
                    anyhow::bail!("no partner {needle:?} has a {year} K-1");
                }
            }
            for k in &packages {
                println!(
                    "{} — {year} Schedule K-1 (Form 1065) for {} ({})",
                    k.partnership_name, k.partner_name, k.partner_id
                );
                for (code, cents) in &k.amounts {
                    let label = FormKind::K1Partnership.box_def(code).map_or("", |d| d.label);
                    println!("  {code:>34}  {:>14}  {label}", show_cents(*cents));
                }
                println!();
            }
            if let Some(k) = packages.first() {
                println!(
                    "As of event {} ({}…).{}",
                    k.through_event,
                    &k.event_hash[..12.min(k.event_hash.len())],
                    if k.warnings.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " The return has {} warning(s); `accountir tax form1065` lists them.",
                            k.warnings.len()
                        )
                    }
                );
            }
        }
        K1CliCommands::Link { source, partner } => {
            let source = open_existing_books(&source)?;
            // Before looking for the partner, so books that issue no K-1s say so
            // rather than reporting a partner they could never have had.
            let kind = accountir::commands::sole_proprietor_commands::business_type(source.connection());
            if kind != accountir::domain::BusinessType::Partnership {
                anyhow::bail!("those books file {}, not Form 1065, so they issue no K-1s", kind.form_name());
            }
            let partners = accountir::commands::partnership_commands::list_partners(source.connection());
            let lowered = partner.to_lowercase();
            let matches: Vec<_> = partners
                .iter()
                .filter(|p| p.partner_id.starts_with(&partner) || p.name.to_lowercase() == lowered)
                .collect();
            let chosen = match matches.as_slice() {
                [one] => one.partner_id.clone(),
                [] => anyhow::bail!("no partner {partner:?} in those books"),
                _ => anyhow::bail!("{partner:?} matches {} partners — use the id", matches.len()),
            };
            let link = tsc::link_k1_source(store, "cli-user", source.connection(), &chosen)?;
            println!(
                "These books now receive {}'s K-1 from {} (link {}).",
                link.partner_name, link.ledger_name, link.link_id
            );
        }
        K1CliCommands::Links { year } => {
            let links = tsc::list_k1_links(store.connection());
            if links.is_empty() {
                println!("These books receive no K-1s through a link.");
            }
            for link in links {
                println!("{}  {} — {}", link.link_id, link.ledger_name, link.partner_name);
                let Some(year) = year else { continue };
                let freshness = locate_linked_books(&link)
                    .and_then(|path| open_existing_books(&path))
                    .and_then(|source| {
                        Ok(tsc::k1_freshness(store.connection(), source.connection(), &link, year)?)
                    });
                match freshness {
                    Ok(tsc::K1Freshness::NotPulled) => println!("  {year}: not pulled"),
                    Ok(tsc::K1Freshness::EnteredByHand) => println!("  {year}: entered by hand"),
                    Ok(tsc::K1Freshness::Current) => println!("  {year}: pulled, and still current"),
                    Ok(tsc::K1Freshness::Changed(changes)) => {
                        println!("  {year}: CHANGED since it was pulled — pull again");
                        for c in changes {
                            println!(
                                "    box {:>34}: {} recorded, {} now",
                                c.code,
                                show_cents(c.recorded),
                                show_cents(c.now)
                            );
                        }
                    }
                    Err(e) => println!("  {year}: cannot check — {e}"),
                }
            }
        }
        K1CliCommands::Unlink { link } => {
            let link = resolve_link(store.connection(), &link)?;
            tsc::unlink_k1_source(store, "cli-user", &link.link_id)?;
            println!(
                "No longer receiving {}'s K-1 from {}. K-1s already pulled are kept.",
                link.partner_name, link.ledger_name
            );
        }
        K1CliCommands::Pull { year, link, source } => {
            let source = source.as_deref().map(open_existing_books).transpose()?;
            let mut links = match &link {
                Some(needle) => vec![resolve_link(store.connection(), needle)?],
                None => tsc::list_k1_links(store.connection()),
            };
            if let Some(source) = &source {
                let id = accountir::documents::ledger_id(source.connection()).unwrap_or_default();
                links.retain(|l| l.ledger_id == id);
            }
            if links.is_empty() {
                anyhow::bail!(
                    "No K-1 links to pull through. Link one first: \
                     `accountir k1 link --source <partnership.db> --partner <name>`"
                );
            }
            for link in links {
                let located;
                let from = match &source {
                    Some(source) => source,
                    None => {
                        located = open_existing_books(&locate_linked_books(&link)?)?;
                        &located
                    }
                };
                let pulled = tsc::pull_k1(store, "cli-user", from.connection(), &link, year)?;
                println!(
                    "{} {year} K-1 for {} from {}: box 1 {}, {} box(es) in all.",
                    if pulled.replaced { "Re-pulled" } else { "Pulled" },
                    link.partner_name,
                    link.ledger_name,
                    show_cents(pulled.statement.amount("1")),
                    pulled.statement.amounts.len()
                );
                if !pulled.warnings.is_empty() {
                    println!(
                        "  The partnership's return has {} warning(s) — review them before relying on this K-1.",
                        pulled.warnings.len()
                    );
                }
            }
        }
    }
    Ok(())
}

fn handle_tax_command(store: &mut EventStore, cmd: TaxCliCommands) -> Result<()> {
    use accountir::commands::partnership_commands as pc;
    use accountir::tax::build_return_from_ledger;

    match cmd {
        TaxCliCommands::Form1065 { year, output } => {
            let conn = store.connection();
            // The same request a K-1 package is read from, so the return and the
            // packages cannot describe different things.
            let request = match accountir::tax::k1_package::return_request(conn, year) {
                Err(accountir::tax::k1_package::K1PackageError::NoProfile) => anyhow::bail!(
                    "No partnership details yet. Run `accountir partnership profile ...` first."
                ),
                other => other?,
            };
            let partner_count = request.partners.len();
            // The ledger entry point, not `build_return`: the latter fills identity
            // only and leaves every money line blank.
            let bundle = build_return_from_ledger(conn, &request)?;

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

        TaxCliCommands::BusinessType { kind } => {
            use accountir::commands::sole_proprietor_commands as spc;
            use accountir::domain::BusinessType;
            if let Some(kind) = kind {
                let parsed = BusinessType::parse(&kind).ok_or_else(|| {
                    let known: Vec<&str> = BusinessType::ALL.iter().map(|t| t.as_str()).collect();
                    anyhow::anyhow!("{kind:?} is not a business type; use one of {}", known.join(", "))
                })?;
                spc::set_business_type(store, "cli-user", parsed)?;
            }
            let current = spc::business_type(store.connection());
            println!("These books are {} and file {}.", current.label(), current.form_name());
        }

        TaxCliCommands::Il1065 { year, output } => {
            let settings = pc::get_il1065_settings(store.connection());
            let bundle =
                accountir::tax::il1065::build_from_ledger(store.connection(), year, &settings)?;
            std::fs::write(&output, &bundle.pdf)?;
            println!(
                "Wrote {} ({} pages) — Illinois IL-1065, {}{}.",
                output.display(),
                bundle.page_count,
                if settings.apportions_outside_illinois {
                    "apportioned"
                } else {
                    "Illinois-only"
                },
                if settings.elects_pte_tax {
                    ", PTE elected"
                } else {
                    ""
                }
            );
            for w in &bundle.warnings {
                println!("warning: {w}");
            }
        }

        TaxCliCommands::IlTaxAddback { account, from, stop } => {
            let account_id = account_id_from(store, &account)?;
            accountir::commands::tax_setup_commands::set_illinois_tax_addback(
                store,
                "cli-user",
                &account_id,
                !stop,
                from,
            )?;
            if stop {
                println!("{account} is no longer added back on IL-1065 line 16 from {from}.");
            } else {
                println!("IL-1065 line 16 adds back what {account} deducts, from {from}.");
            }
        }

        TaxCliCommands::IlSettings {
            apportion_outside,
            elect_pte,
        } => {
            let mut settings = pc::get_il1065_settings(store.connection());
            let changing = apportion_outside.is_some() || elect_pte.is_some();
            if let Some(a) = apportion_outside {
                settings.apportions_outside_illinois = a;
            }
            if let Some(p) = elect_pte {
                settings.elects_pte_tax = p;
            }
            if changing {
                pc::set_il1065_settings(store, "cli-user", &settings)?;
            }
            println!(
                "Illinois IL-1065 settings: {}, {}.",
                if settings.apportions_outside_illinois {
                    "apportions outside Illinois"
                } else {
                    "Illinois-only"
                },
                if settings.elects_pte_tax {
                    "PTE tax elected"
                } else {
                    "no PTE election"
                }
            );
        }
    }
    Ok(())
}
