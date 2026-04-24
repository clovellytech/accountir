use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use chrono::Datelike;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
    Frame, Terminal,
};

use crate::queries::account_queries::AccountQueries;
use crate::queries::reports::Reports;
use crate::store::event_store::EventStore;
use crate::store::migrations::init_schema;

use crate::commands::account_commands::{
    AccountCommands, CreateAccountCommand, UpdateAccountCommand,
};
use crate::commands::entry_commands::{
    EntryCommands, EntryLine, PostEntryCommand, ReassignLineCommand, UnvoidEntryCommand,
    VoidEntryCommand,
};
use crate::domain::AccountType;
use crate::events::types::{Event as DomainEvent, JournalEntrySource};

use super::theme::Theme;
use super::views::{
    account_form::{AccountForm, AccountFormResult},
    accounts::AccountsView,
    bank_import::{BankImportModal, BankImportResult, ParsedTransaction, PendingImport},
    csv_import::{parse_amount, parse_date, parse_delimited_line, CsvImportModal, ImportConfig},
    dashboard::DashboardView,
    entry_detail::{EntryDetail, EntryDetailModal, EntryLineDetail},
    entry_form::{EntryForm, EntryFormResult},
    event_log::EventLogView,
    help::{HelpContext, HelpModal},
    journal::JournalView,
    plaid::{PlaidAccountDisplay, PlaidAction, PlaidItemDisplay, PlaidView},
    plaid_config::{PlaidConfigModal, PlaidConfigResult},
    plaid_link::{PlaidLinkModal, PlaidLinkResult},
    plaid_staged::{
        PlaidStagedView, StagedAction, StagedTransactionDisplay, TransferCandidateDisplay,
    },
    reports::ReportsView,
    settings::{SettingsModal, SettingsResult},
    startup::{StartupAction, StartupView},
    welcome::{should_show_welcome, WelcomeView},
};

/// Application phase
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppPhase {
    Welcome,
    Startup,
    Main,
}

/// Active view in the main application
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveView {
    Dashboard,
    Accounts,
    Journal,
    Reports,
    EventLog,
    Plaid,
}

impl ActiveView {
    fn index(&self) -> usize {
        match self {
            ActiveView::Dashboard => 0,
            ActiveView::Accounts => 1,
            ActiveView::Journal => 2,
            ActiveView::Reports => 3,
            ActiveView::EventLog => 4,
            ActiveView::Plaid => 5,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            0 => ActiveView::Dashboard,
            1 => ActiveView::Accounts,
            2 => ActiveView::Journal,
            3 => ActiveView::Reports,
            4 => ActiveView::EventLog,
            5 => ActiveView::Plaid,
            _ => ActiveView::Dashboard,
        }
    }

    fn count() -> usize {
        6
    }
}

/// Result from a background import task
enum BackgroundResult {
    Staged { message: String, all_done: bool },
    Csv(Result<usize, String>),
    Bank(Result<usize, String>),
}

/// A background task running in a separate thread
struct BackgroundTask {
    label: String,
    receiver: mpsc::Receiver<BackgroundResult>,
    tick: u8,
}

/// Main application state
pub struct App {
    pub phase: AppPhase,
    pub active_view: ActiveView,
    pub should_quit: bool,
    pub database_path: Option<PathBuf>,
    pub startup: StartupView,
    pub dashboard: DashboardView,
    pub accounts: AccountsView,
    pub journal: JournalView,
    pub reports: ReportsView,
    pub event_log: EventLogView,
    pub plaid_view: PlaidView,
    pub plaid_staged: PlaidStagedView,
    pub help: HelpModal,
    pub welcome: WelcomeView,
    pub account_form: AccountForm,
    pub entry_form: EntryForm,
    pub entry_detail: EntryDetailModal,
    pub csv_import: CsvImportModal,
    pub bank_import: BankImportModal,
    pub plaid_link: PlaidLinkModal,
    pub plaid_config: PlaidConfigModal,
    pub settings: SettingsModal,
    pub theme: Theme,
    pub pending_plaid_link: Option<String>, // local_account_id to show plaid link modal for
    pub pending_plaid_action: Option<PlaidAction>, // Action from PlaidView to process
    pub pending_staged_action: Option<StagedAction>, // Action from PlaidStagedView to process
    pub status_message: Option<String>,
    pub pending_import_count: usize,
    pub journal_needs_reload: bool,
    pub pending_entry_detail: Option<String>, // Entry ID to load
    pub pending_reassign: Option<String>,     // Entry ID for reassignment
    pub pending_bulk_reassign: bool,          // Bulk reassignment pending
    pub show_quit_confirm: bool,              // Show quit confirmation dialog
    pub sync_server_running: bool,            // Whether the background sync server is running
    pub pending_default_accounts: bool,       // Show default chart of accounts confirmation
    pub create_default_accounts: bool,        // User confirmed, create the accounts
    pub pending_bank_import_result: Option<BankImportResult>, // Result from bank import modal
    pub last_import_check: std::time::Instant, // Last time we checked for new imports
    background_task: Option<BackgroundTask>,
}

impl App {
    pub fn new() -> Self {
        // Start with Welcome phase if enabled, otherwise go to Startup
        let initial_phase = if should_show_welcome() {
            AppPhase::Welcome
        } else {
            AppPhase::Startup
        };

        let config = crate::config::AppConfig::load();
        let theme = Theme::from_preset(config.theme);

        Self {
            phase: initial_phase,
            active_view: ActiveView::Dashboard,
            should_quit: false,
            database_path: None,
            startup: StartupView::new(),
            dashboard: DashboardView::new(),
            accounts: AccountsView::new(),
            journal: JournalView::new(),
            reports: ReportsView::new(),
            event_log: EventLogView::new(),
            plaid_view: PlaidView::new(),
            plaid_staged: PlaidStagedView::new(),
            help: HelpModal::new(),
            welcome: WelcomeView::new(),
            account_form: AccountForm::new(),
            entry_form: EntryForm::new(),
            entry_detail: EntryDetailModal::new(),
            csv_import: CsvImportModal::new(),
            bank_import: BankImportModal::new(),
            plaid_link: PlaidLinkModal::new(),
            plaid_config: PlaidConfigModal::new(),
            settings: SettingsModal::new(),
            theme,
            pending_plaid_link: None,
            pending_plaid_action: None,
            pending_staged_action: None,
            status_message: None,
            pending_import_count: 0,
            journal_needs_reload: false,
            pending_entry_detail: None,
            pending_reassign: None,
            pending_bulk_reassign: false,
            show_quit_confirm: false,
            sync_server_running: false,
            pending_default_accounts: false,
            create_default_accounts: false,
            pending_bank_import_result: None,
            last_import_check: std::time::Instant::now(),
            background_task: None,
        }
    }

    /// Get the current help context based on app state
    fn help_context(&self) -> HelpContext {
        match self.phase {
            AppPhase::Welcome => HelpContext::Startup, // Use Startup context for Welcome
            AppPhase::Startup => HelpContext::Startup,
            AppPhase::Main => match self.active_view {
                ActiveView::Dashboard => HelpContext::Dashboard,
                ActiveView::Accounts => HelpContext::Accounts,
                ActiveView::Journal => HelpContext::Journal,
                ActiveView::Reports => HelpContext::Reports,
                ActiveView::EventLog => HelpContext::EventLog,
                ActiveView::Plaid => HelpContext::Plaid,
            },
        }
    }

    /// Whether a background task is currently running
    fn has_background_task(&self) -> bool {
        self.background_task.is_some()
    }

    /// Check if the background task has completed. Returns the result if done.
    fn poll_background_task(&mut self) -> Option<BackgroundResult> {
        let done = self
            .background_task
            .as_ref()
            .and_then(|t| t.receiver.try_recv().ok());
        if done.is_some() {
            self.background_task = None;
        } else if let Some(ref mut task) = self.background_task {
            task.tick = task.tick.wrapping_add(1);
        }
        done
    }

    /// Close the current database and return to startup menu
    pub fn close_database(&mut self) {
        self.phase = AppPhase::Startup;
        self.database_path = None;
        self.active_view = ActiveView::Dashboard;

        // Reset all views
        self.dashboard = DashboardView::new();
        self.accounts = AccountsView::new();
        self.journal = JournalView::new();
        self.reports = ReportsView::new();
        self.event_log = EventLogView::new();
        self.plaid_view = PlaidView::new();

        // Reset forms and modals
        self.account_form = AccountForm::new();
        self.entry_form = EntryForm::new();
        self.entry_detail = EntryDetailModal::new();
        self.csv_import = CsvImportModal::new();
        self.bank_import = BankImportModal::new();
        self.plaid_link = PlaidLinkModal::new();
        self.plaid_config = PlaidConfigModal::new();
        self.settings = SettingsModal::new();
        self.welcome = WelcomeView::new();
        self.pending_import_count = 0;

        // Reset pending actions
        self.pending_plaid_link = None;
        self.pending_plaid_action = None;
        self.pending_entry_detail = None;
        self.pending_reassign = None;
        self.pending_bulk_reassign = false;
        self.journal_needs_reload = false;

        // Clear status message
        self.status_message = None;

        // Reset sync server status (actual abort happens in the event loop)
        self.sync_server_running = false;

        // Reset startup view
        self.startup = StartupView::new();
    }

    pub fn load_data(&mut self, store: &EventStore) {
        let conn = store.connection();
        let queries = AccountQueries::new(conn);
        let reports = Reports::new(conn);

        // Load accounts
        if let Ok(accounts) = queries.get_all_accounts() {
            self.accounts.set_accounts(accounts);
        }

        // Load balances
        if let Ok(balances) = queries.get_all_balances(None) {
            self.dashboard.balances = balances.clone();
            self.accounts.balances = balances;
        }

        // Load trial balance
        if let Ok(trial_balance) = reports.trial_balance(None) {
            self.reports.trial_balance = trial_balance.lines;
        }

        // Load balance sheet and income statement using report_date
        self.load_reports(store);

        // Load journal entries (filtered or all)
        self.load_journal_entries(store);

        // Load events for event log
        if let Ok(events) = store.get_all() {
            self.event_log.set_events(events);
        }

        // Load Plaid mappings for accounts view
        self.load_plaid_mappings(conn);

        // Load Plaid items for Plaid view
        self.load_plaid_items(conn);

        // Load pending bank imports
        self.load_pending_imports(conn);
    }

    /// Check for new pending imports (called periodically)
    pub fn check_for_new_imports(&mut self, conn: &rusqlite::Connection) {
        // Only check every 2 seconds
        if self.last_import_check.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_import_check = std::time::Instant::now();

        // Quick count query
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pending_imports WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let count = count as usize;

        // Only reload if count changed
        if count != self.pending_import_count {
            self.load_pending_imports(conn);
        }
    }

    /// Load Plaid account mappings for display in the accounts table
    fn load_plaid_mappings(&mut self, conn: &rusqlite::Connection) {
        use super::views::accounts::PlaidMappingSummary;

        let mappings: std::collections::HashMap<String, PlaidMappingSummary> = conn
            .prepare(
                "SELECT pla.local_account_id, pi.institution_name, pla.mask
                 FROM plaid_local_accounts pla
                 JOIN plaid_items pi ON pla.item_id = pi.id
                 WHERE pla.local_account_id IS NOT NULL AND pi.status = 'active'",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    let local_id: String = row.get(0)?;
                    let institution: String = row.get(1)?;
                    let mask: Option<String> = row.get(2)?;
                    Ok((
                        local_id,
                        PlaidMappingSummary {
                            institution_name: institution,
                            mask,
                        },
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        self.accounts.plaid_mappings = mappings;
    }

    /// Load Plaid items for the Plaid view
    fn load_plaid_items(&mut self, conn: &rusqlite::Connection) {
        let items: Vec<PlaidItemDisplay> = conn
            .prepare(
                "SELECT id, institution_name, status, last_synced_at FROM plaid_items ORDER BY rowid DESC",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
            })
            .unwrap_or_default()
            .into_iter()
            .map(|(id, institution_name, status, last_synced_at)| {
                let accounts: Vec<PlaidAccountDisplay> = conn
                    .prepare(
                        "SELECT pa.plaid_account_id, pa.name, pa.account_type, pa.mask, pa.local_account_id, a.name
                         FROM plaid_local_accounts pa
                         LEFT JOIN accounts a ON pa.local_account_id = a.id
                         WHERE pa.item_id = ?1",
                    )
                    .and_then(|mut stmt| {
                        stmt.query_map([&id], |row| {
                            Ok(PlaidAccountDisplay {
                                plaid_account_id: row.get(0)?,
                                name: row.get(1)?,
                                account_type: row.get(2)?,
                                mask: row.get(3)?,
                                local_account_name: row.get(5)?,
                            })
                        })
                        .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    })
                    .unwrap_or_default();

                PlaidItemDisplay {
                    id,
                    institution_name,
                    status,
                    last_synced_at,
                    accounts,
                }
            })
            .collect();

        self.plaid_view.set_items(items);

        // Load staged transaction counts
        if let Ok((staged, transfers)) = crate::commands::plaid_commands::staged_counts(conn) {
            self.plaid_view.staged_count = staged as usize;
            self.plaid_view.transfer_count = transfers as usize;
        }
    }

    /// Load staged transaction data for the review view
    fn load_plaid_staged(&mut self, conn: &rusqlite::Connection) {
        let candidates =
            crate::commands::plaid_commands::load_pending_transfers(conn).unwrap_or_default();
        let unmatched =
            crate::commands::plaid_commands::load_pending_staged(conn).unwrap_or_default();

        self.plaid_staged.transfer_candidates = candidates
            .iter()
            .map(|c| {
                let txn1_account = c
                    .txn1
                    .local_account_id
                    .as_ref()
                    .and_then(|aid| {
                        conn.query_row("SELECT name FROM accounts WHERE id = ?1", [aid], |row| {
                            row.get::<_, String>(0)
                        })
                        .ok()
                    })
                    .unwrap_or_else(|| "Unmapped".to_string());
                let txn2_account = c
                    .txn2
                    .local_account_id
                    .as_ref()
                    .and_then(|aid| {
                        conn.query_row("SELECT name FROM accounts WHERE id = ?1", [aid], |row| {
                            row.get::<_, String>(0)
                        })
                        .ok()
                    })
                    .unwrap_or_else(|| "Unmapped".to_string());

                TransferCandidateDisplay {
                    candidate_id: c.id.clone(),
                    txn1_name: c
                        .txn1
                        .merchant_name
                        .as_deref()
                        .unwrap_or(&c.txn1.name)
                        .to_string(),
                    txn1_account,
                    txn1_date: c.txn1.date.clone(),
                    txn1_amount_cents: c.txn1.amount_cents,
                    txn2_name: c
                        .txn2
                        .merchant_name
                        .as_deref()
                        .unwrap_or(&c.txn2.name)
                        .to_string(),
                    txn2_account,
                    txn2_date: c.txn2.date.clone(),
                    txn2_amount_cents: c.txn2.amount_cents,
                    confidence: c.confidence,
                }
            })
            .collect();

        self.plaid_staged.unmatched = unmatched
            .iter()
            .map(|t| {
                let account_name = t
                    .local_account_id
                    .as_ref()
                    .and_then(|aid| {
                        conn.query_row("SELECT name FROM accounts WHERE id = ?1", [aid], |row| {
                            row.get::<_, String>(0)
                        })
                        .ok()
                    })
                    .unwrap_or_else(|| "Unmapped".to_string());

                StagedTransactionDisplay {
                    id: t.id.clone(),
                    date: t.date.clone(),
                    name: t.merchant_name.as_deref().unwrap_or(&t.name).to_string(),
                    account_name,
                    amount_cents: t.amount_cents,
                }
            })
            .collect();
    }

    /// Load pending bank imports
    fn load_pending_imports(&mut self, conn: &rusqlite::Connection) {
        // Load pending imports
        let imports: Vec<PendingImport> = conn
            .prepare(
                "SELECT id, file_path, file_name, bank_id, bank_name, transaction_count, created_at
                 FROM pending_imports WHERE status = 'pending' ORDER BY created_at DESC",
            )
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    Ok(PendingImport {
                        id: row.get(0)?,
                        file_path: row.get(1)?,
                        file_name: row.get(2)?,
                        bank_id: row.get(3)?,
                        bank_name: row.get(4)?,
                        transaction_count: row.get(5)?,
                        created_at: row.get(6)?,
                    })
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        self.pending_import_count = imports.len();
        self.bank_import.set_pending_imports(imports);

        // Load bank-account mappings
        let mappings: std::collections::HashMap<String, String> = conn
            .prepare("SELECT bank_id, account_id FROM bank_accounts")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| {
                    let bank_id: String = row.get(0)?;
                    let account_id: String = row.get(1)?;
                    Ok((bank_id, account_id))
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();

        self.bank_import.set_mappings(mappings);

        // Set available accounts
        self.bank_import
            .set_accounts(self.accounts.accounts.clone());
    }

    /// Load/reload balance sheet and income statement using the current report_date
    pub fn load_reports(&mut self, store: &EventStore) {
        let conn = store.connection();
        let reports = Reports::new(conn);
        let report_date = self.reports.report_date;

        // Load balance sheet using report_date
        if let Ok(balance_sheet) = reports.balance_sheet(report_date) {
            self.reports.balance_sheet = Some(balance_sheet);
        }

        // Load income statement (year start to report_date)
        let year_start =
            chrono::NaiveDate::from_ymd_opt(report_date.year(), 1, 1).unwrap_or(report_date);
        if let Ok(income_statement) = reports.income_statement(year_start, report_date) {
            self.reports.income_statement = Some(income_statement);
        }

        self.reports.needs_reload = false;
    }

    /// Load entry details for displaying in modal
    pub fn load_entry_detail(&self, store: &EventStore, entry_id: &str) -> Option<EntryDetail> {
        let conn = store.connection();

        // Load entry header
        let entry_result: Result<(String, String, Option<String>, i32), _> = conn.query_row(
            "SELECT date, memo, reference, is_void FROM journal_entries WHERE id = ?1",
            [entry_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        );

        let (date_str, memo, reference, is_void) = entry_result.ok()?;
        let date = chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").ok()?;

        // Load entry lines with account info
        let mut stmt = conn
            .prepare(
                "SELECT jl.amount, jl.memo, a.account_number, a.name
             FROM journal_lines jl
             JOIN accounts a ON jl.account_id = a.id
             WHERE jl.entry_id = ?1
             ORDER BY jl.id",
            )
            .ok()?;

        let lines: Vec<EntryLineDetail> = stmt
            .query_map([entry_id], |row| {
                let amount: i64 = row.get(0)?;
                let line_memo: Option<String> = row.get(1)?;
                let account_number: String = row.get(2)?;
                let account_name: String = row.get(3)?;

                let (debit, credit) = if amount >= 0 {
                    (amount, 0)
                } else {
                    (0, -amount)
                };

                Ok(EntryLineDetail {
                    account_number,
                    account_name,
                    debit,
                    credit,
                    memo: line_memo,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        Some(EntryDetail {
            entry_id: entry_id.to_string(),
            date,
            memo,
            reference,
            is_void: is_void == 1,
            lines,
        })
    }

    /// Load journal entries based on current filter
    pub fn load_journal_entries(&mut self, store: &EventStore) {
        let conn = store.connection();
        let search = crate::queries::search::Search::new(conn);

        let entries = if let Some(ref account) = self.journal.filter_account {
            search
                .search_entries(None, None, None, Some(&account.id), true)
                .unwrap_or_default()
        } else {
            search.recent_entries(100).unwrap_or_default()
        };

        self.journal.entries = entries;
        self.journal.sort_entries();
    }

    pub fn next_tab(&mut self) {
        let next_index = (self.active_view.index() + 1) % ActiveView::count();
        self.active_view = ActiveView::from_index(next_index);
    }

    pub fn previous_tab(&mut self) {
        let prev_index = if self.active_view.index() == 0 {
            ActiveView::count() - 1
        } else {
            self.active_view.index() - 1
        };
        self.active_view = ActiveView::from_index(prev_index);
    }

    pub fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        // Handle quit confirmation dialog first
        if self.show_quit_confirm {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.should_quit = true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter | KeyCode::Esc => {
                    self.show_quit_confirm = false;
                }
                _ => {}
            }
            return;
        }

        // Handle default accounts confirmation dialog
        if self.pending_default_accounts {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.pending_default_accounts = false;
                    self.create_default_accounts = true;
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter | KeyCode::Esc => {
                    self.pending_default_accounts = false;
                    self.status_message = Some("Skipped default accounts".to_string());
                }
                _ => {}
            }
            return;
        }

        // Handle entry detail modal first - it captures all input when visible
        if self.entry_detail.visible {
            self.entry_detail.handle_key(key);
            return;
        }

        // Handle entry form - it captures all input when visible
        if self.entry_form.visible {
            self.entry_form.handle_key_with_modifiers(key, modifiers);
            return;
        }

        // Handle account form - it captures all input when visible
        if self.account_form.visible {
            self.account_form.handle_key(key);
            return;
        }

        // Handle CSV import modal - it captures all input when visible
        if self.csv_import.visible {
            self.csv_import.handle_key(key);
            return;
        }

        // Handle plaid config modal - it captures all input when visible
        if self.plaid_config.visible {
            self.plaid_config.handle_key(key);
            return;
        }

        // Handle settings modal - it captures all input when visible
        if self.settings.visible {
            let was_enter = key == KeyCode::Enter;
            self.settings.handle_key(key);
            match &self.settings.result {
                SettingsResult::Saved(preset) => {
                    self.theme = Theme::from_preset(*preset);
                    if was_enter {
                        self.settings.hide();
                        self.status_message = Some("Theme saved".to_string());
                    }
                }
                SettingsResult::Cancel => {
                    // Restore original theme from config
                    let config = crate::config::AppConfig::load();
                    self.theme = Theme::from_preset(config.theme);
                    self.settings.hide();
                }
                SettingsResult::None => {}
            }
            // Reset result after processing (except Cancel which already hid)
            if self.settings.result != SettingsResult::Cancel {
                self.settings.result = SettingsResult::None;
            }
            return;
        }

        // Handle plaid link modal - it captures all input when visible
        if self.plaid_link.visible {
            self.plaid_link.handle_key(key);
            return;
        }

        // Handle bank import modal - it captures all input when visible
        if self.bank_import.visible {
            let result = self.bank_import.handle_key(key);
            match result {
                BankImportResult::None => {}
                _ => {
                    self.pending_bank_import_result = Some(result);
                }
            }
            return;
        }

        // Handle help modal - it captures all input when visible
        if self.help.visible {
            match key {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.help.hide();
                }
                _ => {}
            }
            return;
        }

        // Toggle help with '?'
        if key == KeyCode::Char('?') {
            self.help.toggle();
            return;
        }

        match self.phase {
            AppPhase::Welcome => self.handle_welcome_key(key),
            AppPhase::Startup => self.handle_startup_key(key, modifiers),
            AppPhase::Main => self.handle_main_key(key, modifiers),
        }
    }

    /// Open the account creation form
    pub fn open_account_form(&mut self) {
        let existing_accounts = self.accounts.accounts.clone();

        // Check if an account is selected (use tree-aware selection)
        if let Some(selected_account) = self.accounts.get_selected_account() {
            // Calculate next available account number with same starting digit
            let next_number = self.calculate_next_account_number(&selected_account.account_number);

            self.account_form.show_with_defaults(
                existing_accounts,
                Some(selected_account.account_type),
                Some(&selected_account.id),
                Some(next_number),
            );
            return;
        }

        self.account_form.show(existing_accounts);
    }

    /// Open the account edit form for the selected account
    pub fn open_account_edit_form(&mut self) {
        // Use tree-aware selection
        if let Some(selected_account) = self.accounts.get_selected_account().cloned() {
            let existing_accounts = self.accounts.accounts.clone();
            self.account_form
                .show_edit(&selected_account, existing_accounts);
        }
    }

    /// Calculate the next available account number with the same starting digit as the parent
    fn calculate_next_account_number(&self, parent_number: &str) -> String {
        // Get the first digit of the parent account number
        let first_digit = parent_number.chars().next().unwrap_or('1');

        // Find all account numbers that start with this digit
        let mut used_numbers: Vec<u32> = self
            .accounts
            .accounts
            .iter()
            .filter_map(|a| {
                if a.account_number.starts_with(first_digit) {
                    a.account_number.parse::<u32>().ok()
                } else {
                    None
                }
            })
            .collect();

        used_numbers.sort();

        // Determine the range based on first digit (e.g., 1xxx for assets, 2xxx for liabilities)
        let range_start = first_digit.to_digit(10).unwrap_or(1) * 1000;
        let range_end = range_start + 999;

        // Find the next unused number in this range
        let mut next = range_start;
        for num in used_numbers {
            if num >= range_start && num <= range_end && num >= next {
                next = num + 1;
            }
        }

        // Ensure we don't exceed the range
        if next > range_end {
            next = range_end;
        }

        next.to_string()
    }

    /// Open the journal entry form
    pub fn open_entry_form(&mut self) {
        let existing_accounts = self.accounts.accounts.clone();
        let preselected = self.journal.filter_account.as_ref();
        self.entry_form
            .show_with_account(existing_accounts, preselected);
    }

    pub fn open_csv_import(&mut self) {
        use crate::tui::views::csv_import::AccountChoice;

        // Build list of accounts for selection
        let accounts: Vec<AccountChoice> = self
            .accounts
            .accounts
            .iter()
            .filter(|a| a.is_active)
            .map(|a| AccountChoice {
                id: a.id.clone(),
                name: a.name.clone(),
                account_number: a.account_number.clone(),
                account_type: format!("{}", a.account_type),
            })
            .collect();

        self.csv_import.show(accounts);
    }

    /// Request to start reassignment (actual start happens in main loop with store access)
    pub fn request_reassign(&mut self) -> Option<String> {
        // Need a selected entry - use visible_entries to respect show_void filter
        let i = self.journal.state.selected()?;
        let visible = self.journal.visible_entries();
        let entry = visible.get(i)?;
        // Don't allow reassigning void entries
        if entry.is_void {
            return None;
        }
        Some(entry.entry_id.clone())
    }

    /// Start reassignment mode with entry details (called from main loop)
    pub fn start_reassign_with_lines(
        &mut self,
        entry_id: String,
        lines: Vec<(String, String, String)>, // (line_id, account_id, account_name)
    ) {
        use crate::tui::views::journal::ReassignAccount;

        if lines.is_empty() {
            return;
        }

        // Find the line to reassign:
        // - In ledger view: the line that's NOT the current account (or first line if all same)
        // - In journal view: use the first line
        let (line_id, other_account_id, current_account_name) = if let Some(ref filter_account) =
            self.journal.filter_account
        {
            // Find the line that's not the filter account
            match lines
                .iter()
                .find(|(_, acct_id, _)| acct_id != &filter_account.id)
            {
                Some((lid, acct_id, name)) => (lid.clone(), Some(acct_id.clone()), name.clone()),
                None => {
                    // All lines are the same account - use the first line for reassignment
                    let (lid, acct_id, name) = &lines[0];
                    (lid.clone(), Some(acct_id.clone()), name.clone())
                }
            }
        } else {
            // Journal view: just use first line
            let (lid, acct_id, name) = &lines[0];
            (lid.clone(), Some(acct_id.clone()), name.clone())
        };

        // Build account list for picker
        let accounts: Vec<ReassignAccount> = self
            .accounts
            .accounts
            .iter()
            .filter(|a| a.is_active)
            .map(|a| ReassignAccount {
                id: a.id.clone(),
                account_number: a.account_number.clone(),
                name: a.name.clone(),
            })
            .collect();

        self.journal.start_reassign(
            accounts,
            entry_id,
            line_id,
            current_account_name,
            other_account_id,
        );
    }

    fn handle_welcome_key(&mut self, key: KeyCode) {
        self.welcome.handle_key(key);
        if self.welcome.should_continue() {
            // If a database is already loaded (via CLI), go to Main, otherwise Startup
            if self.database_path.is_some() {
                self.phase = AppPhase::Main;
            } else {
                self.phase = AppPhase::Startup;
            }
        }
    }

    fn handle_startup_key(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        // Global quit - Ctrl+C quits immediately, q/Esc shows confirmation
        if key == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if key == KeyCode::Char('q') || key == KeyCode::Esc {
            self.show_quit_confirm = true;
            return;
        }

        self.startup.handle_key(key);
    }

    fn handle_main_key(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        // If in reports view and editing date, handle that first (skip global keys)
        if self.active_view == ActiveView::Reports && self.reports.editing_date {
            self.reports.handle_key(key);
            return;
        }

        // If journal has an active modal (reassign, void confirm, bulk void confirm) or
        // any rows are currently selected for a bulk operation, let journal handle keys
        // first before global keys.
        if self.active_view == ActiveView::Journal {
            let has_modal = self.journal.is_reassigning()
                || self.journal.is_confirming_void()
                || self.journal.is_confirming_bulk_void()
                || self.journal.has_selections();

            if has_modal {
                let wants_reassign = self.journal.handle_key(key, modifiers);
                if wants_reassign && key == KeyCode::Char('a') {
                    if self.journal.has_selections() {
                        self.pending_bulk_reassign = true;
                    } else {
                        self.pending_reassign = self.request_reassign();
                    }
                }
                return;
            }

            // In ledger view, Esc goes back to journal (not quit)
            if key == KeyCode::Esc && self.journal.filter_account.is_some() {
                self.journal.clear_filter();
                self.journal_needs_reload = true;
                return;
            }
        }

        // In plaid staged view, Esc goes back to plaid (not close database)
        if key == KeyCode::Esc && self.active_view == ActiveView::Plaid && self.plaid_staged.visible
        {
            self.pending_staged_action = Some(StagedAction::Back);
            return;
        }

        // Global keys
        match key {
            KeyCode::Char('q') => {
                self.show_quit_confirm = true;
                return;
            }
            KeyCode::Esc => {
                // Esc closes database and returns to startup menu
                self.close_database();
                return;
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                // Ctrl+C quits immediately
                self.should_quit = true;
                return;
            }
            KeyCode::Tab => {
                if self.plaid_staged.visible && self.active_view == ActiveView::Plaid {
                    // Let the staged view handle Tab for section switching
                } else {
                    self.next_tab();
                    return;
                }
            }
            KeyCode::BackTab => {
                if self.plaid_staged.visible && self.active_view == ActiveView::Plaid {
                    // Let the staged view handle BackTab for section switching
                } else {
                    self.previous_tab();
                    return;
                }
            }
            KeyCode::Char('1') => {
                self.active_view = ActiveView::Dashboard;
                return;
            }
            KeyCode::Char('2') => {
                self.active_view = ActiveView::Accounts;
                return;
            }
            KeyCode::Char('3') => {
                self.active_view = ActiveView::Journal;
                return;
            }
            KeyCode::Char('4') => {
                self.active_view = ActiveView::Reports;
                return;
            }
            KeyCode::Char('5') => {
                self.active_view = ActiveView::EventLog;
                return;
            }
            KeyCode::Char('6') => {
                self.active_view = ActiveView::Plaid;
                return;
            }
            KeyCode::Char('b') => {
                // Open bank import modal if there are pending imports
                if self.pending_import_count > 0 {
                    self.bank_import.show();
                    return;
                }
            }
            KeyCode::Char(',') => {
                self.settings.show();
                return;
            }
            _ => {}
        }

        // View-specific keys
        match self.active_view {
            ActiveView::Dashboard => self.dashboard.handle_key(key),
            ActiveView::Accounts => {
                if key == KeyCode::Char('a') {
                    self.open_account_form();
                } else if key == KeyCode::Char('e') {
                    self.open_account_edit_form();
                } else if key == KeyCode::Char('p') {
                    // Open Plaid link modal for selected account
                    if let Some(acct) = self.accounts.get_selected_account() {
                        match acct.account_type {
                            AccountType::Asset | AccountType::Liability => {
                                self.pending_plaid_link = Some(acct.id.clone());
                            }
                            _ => {
                                self.status_message = Some(
                                    "Plaid link is only available for Asset and Liability accounts"
                                        .to_string(),
                                );
                            }
                        }
                    }
                } else {
                    self.accounts.handle_key(key);
                }
            }
            ActiveView::Journal => {
                // Modal states are handled above, so we're in normal mode here
                if key == KeyCode::Char('e') {
                    self.open_entry_form();
                } else if key == KeyCode::Char('i') {
                    self.open_csv_import();
                } else if key == KeyCode::Enter {
                    // Open entry detail for selected entry (use visible_entries for correct index)
                    if let Some(i) = self.journal.state.selected() {
                        let visible = self.journal.visible_entries();
                        if let Some(entry) = visible.get(i) {
                            self.pending_entry_detail = Some(entry.entry_id.clone());
                        }
                    }
                } else {
                    let wants_reassign = self.journal.handle_key(key, modifiers);
                    if wants_reassign && key == KeyCode::Char('a') {
                        if self.journal.has_selections() {
                            self.pending_bulk_reassign = true;
                        } else {
                            self.pending_reassign = self.request_reassign();
                        }
                    }
                }
            }
            ActiveView::Reports => self.reports.handle_key(key),
            ActiveView::EventLog => {
                self.event_log.handle_key(key);
            }
            ActiveView::Plaid => {
                if self.plaid_staged.visible {
                    let action = self.plaid_staged.handle_key(key);
                    match action {
                        StagedAction::None => {}
                        other => {
                            self.pending_staged_action = Some(other);
                        }
                    }
                } else {
                    let action = self.plaid_view.handle_key(key);
                    match action {
                        PlaidAction::None => {}
                        other => {
                            self.pending_plaid_action = Some(other);
                        }
                    }
                }
            }
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_welcome(frame: &mut Frame, app: &mut App) {
    let size = frame.area();
    app.welcome.draw(frame, size, &app.theme);
}

fn draw_startup(frame: &mut Frame, app: &mut App) {
    let size = frame.area();
    app.startup.draw(frame, size, &app.theme);

    // Draw help modal on top if visible
    app.help.draw(frame, size, app.help_context(), &app.theme);
}

fn draw_main(frame: &mut Frame, app: &mut App) {
    let size = frame.area();
    let theme = &app.theme;

    // Create main layout
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Tabs
            Constraint::Min(0),    // Content
            Constraint::Length(1), // Status bar
        ])
        .split(size);

    // Draw tabs with database name
    let db_name = app
        .database_path
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    let title = format!(" Accountir - {} ", db_name);

    let titles = vec![
        "1:Dashboard",
        "2:Accounts",
        "3:Journal",
        "4:Reports",
        "5:Events",
        "6:Plaid",
    ];
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(title))
        .select(app.active_view.index())
        .style(theme.text_style())
        .highlight_style(theme.tab_highlight_style());
    frame.render_widget(tabs, chunks[0]);

    // Draw active view
    match app.active_view {
        ActiveView::Dashboard => app.dashboard.draw(frame, chunks[1], theme),
        ActiveView::Accounts => app.accounts.draw(frame, chunks[1], theme),
        ActiveView::Journal => app.journal.draw(frame, chunks[1], theme),
        ActiveView::Reports => app.reports.draw(frame, chunks[1], theme),
        ActiveView::EventLog => app.event_log.draw(frame, chunks[1], theme),
        ActiveView::Plaid => {
            if app.plaid_staged.visible {
                app.plaid_staged.render(frame, chunks[1], theme);
            } else {
                app.plaid_view.render(frame, chunks[1], theme);
            }
        }
    }

    // Draw status bar
    let status_text = if let Some(ref msg) = app.status_message {
        msg.clone()
    } else {
        "Tab: switch views | 1-6: jump to view | ,: settings | ?: help | Esc: close file | q: quit"
            .to_string()
    };

    let sync_indicator = if app.sync_server_running {
        Span::styled(" Sync: :9876 ", theme.success_style())
    } else {
        Span::styled("", Style::default())
    };

    let import_indicator = if app.pending_import_count > 0 {
        Span::styled(
            format!(
                " [b] {} import{} ",
                app.pending_import_count,
                if app.pending_import_count == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("", Style::default())
    };

    let status = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(status_text, theme.dim_style()),
        Span::raw("  "),
        import_indicator,
        sync_indicator,
    ]));
    frame.render_widget(status, chunks[2]);

    // Draw help modal on top if visible
    app.help.draw(frame, size, app.help_context(), theme);

    // Draw account form on top if visible
    app.account_form.draw(frame, size, theme);

    // Draw entry form on top if visible
    app.entry_form.draw(frame, size, theme);

    // Draw entry detail modal on top if visible
    app.entry_detail.draw(frame, size, theme);

    // Draw CSV import modal on top if visible
    app.csv_import.draw(frame, size, theme);

    // Draw bank import modal on top if visible
    app.bank_import.draw(frame, size, theme);

    // Draw plaid link modal on top if visible
    app.plaid_link.draw(frame, size, theme);

    // Draw plaid config modal on top if visible
    app.plaid_config.draw(frame, size, theme);

    // Draw settings modal on top if visible
    app.settings.draw(frame, size, theme);
}

fn draw_ui(frame: &mut Frame, app: &mut App) {
    match app.phase {
        AppPhase::Welcome => draw_welcome(frame, app),
        AppPhase::Startup => draw_startup(frame, app),
        AppPhase::Main => draw_main(frame, app),
    }

    // Draw loading modal on top of everything when a background task is running
    if let Some(ref task) = app.background_task {
        draw_loading_modal(frame, &app.theme, &task.label, task.tick);
    }

    // Draw quit confirmation dialog on top of everything
    if app.show_quit_confirm {
        draw_quit_confirm(frame, &app.theme);
    }

    // Draw default accounts confirmation dialog
    if app.pending_default_accounts {
        draw_default_accounts_confirm(frame, &app.theme);
    }
}

fn draw_loading_modal(frame: &mut Frame, theme: &Theme, label: &str, tick: u8) {
    use ratatui::widgets::Clear;

    let area = frame.area();
    let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let spinner = spinner_chars[(tick as usize / 2) % spinner_chars.len()];

    let dialog_width = (label.len() as u16 + 8)
        .max(20)
        .min(area.width.saturating_sub(4));
    let dialog_height = 3;
    let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
    let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

    let dialog_area = ratatui::layout::Rect {
        x: dialog_x,
        y: dialog_y,
        width: dialog_width,
        height: dialog_height,
    };

    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.header));

    frame.render_widget(block, dialog_area);

    let inner = ratatui::layout::Rect {
        x: dialog_area.x + 2,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(4),
        height: 1,
    };

    let text = Line::from(vec![
        Span::styled(format!("{} ", spinner), theme.header_style()),
        Span::raw(label),
    ]);

    frame.render_widget(Paragraph::new(text), inner);
}

fn draw_quit_confirm(frame: &mut Frame, theme: &Theme) {
    use ratatui::widgets::Clear;

    let area = frame.area();

    let dialog_width = 30;
    let dialog_height = 5;
    let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
    let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

    let dialog_area = ratatui::layout::Rect {
        x: dialog_x,
        y: dialog_y,
        width: dialog_width,
        height: dialog_height,
    };

    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.header))
        .title(" Quit ")
        .title_style(theme.header_style());

    frame.render_widget(block, dialog_area);

    let inner = ratatui::layout::Rect {
        x: dialog_area.x + 2,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(4),
        height: dialog_area.height.saturating_sub(2),
    };

    let text = vec![
        Line::from("Are you sure you want to quit?"),
        Line::from(""),
        Line::from(vec![
            Span::raw("Press "),
            Span::styled("y", theme.success_style()),
            Span::raw(" to quit, "),
            Span::styled("N", theme.error_style().add_modifier(Modifier::BOLD)),
            Span::raw("/Enter to cancel"),
        ]),
    ];

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

fn draw_default_accounts_confirm(frame: &mut Frame, theme: &Theme) {
    use ratatui::widgets::Clear;

    let area = frame.area();

    let dialog_width = 50;
    let dialog_height = 14;
    let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
    let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

    let dialog_area = ratatui::layout::Rect {
        x: dialog_x,
        y: dialog_y,
        width: dialog_width,
        height: dialog_height,
    };

    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_style())
        .title(" Default Accounts ")
        .title_style(theme.modal_title_style());

    frame.render_widget(block, dialog_area);

    let inner = ratatui::layout::Rect {
        x: dialog_area.x + 2,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(4),
        height: dialog_area.height.saturating_sub(2),
    };

    let acct_num_style = Style::default().fg(theme.header);
    let text = vec![
        Line::from("No accounts found. Create defaults?"),
        Line::from(""),
        Line::from(vec![
            Span::styled("  1000", acct_num_style),
            Span::raw("  Assets"),
        ]),
        Line::from(vec![
            Span::styled("  1001", acct_num_style),
            Span::raw("    Business Checking"),
        ]),
        Line::from(vec![
            Span::styled("  2000", acct_num_style),
            Span::raw("  Income"),
        ]),
        Line::from(vec![
            Span::styled("  3000", acct_num_style),
            Span::raw("  Expenses"),
        ]),
        Line::from(vec![
            Span::styled("  4000", acct_num_style),
            Span::raw("  Equity"),
        ]),
        Line::from(vec![
            Span::styled("  4001", acct_num_style),
            Span::raw("    Opening Balances"),
        ]),
        Line::from(vec![
            Span::styled("  5000", acct_num_style),
            Span::raw("  Liabilities"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("Press "),
            Span::styled("y", theme.success_style()),
            Span::raw(" to create, "),
            Span::styled("N", theme.error_style().add_modifier(Modifier::BOLD)),
            Span::raw("/Esc to skip"),
        ]),
    ];

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

/// Result of running the TUI
pub enum TuiResult {
    Quit,
    OpenDatabase(PathBuf, bool), // path, is_new
}

/// Run the TUI application
pub fn run_app(server_db: Option<crate::server::ServerDb>) -> io::Result<TuiResult> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    app.sync_server_running = server_db.is_some();
    let mut store: Option<EventStore> = None;

    // Main loop
    let result = loop {
        terminal.draw(|f| draw_ui(f, &mut app))?;

        // Check for background task completion
        if let Some(result) = app.poll_background_task() {
            if let Some(ref mut s) = store {
                handle_background_result(&mut app, s, result);
            }
        }

        // While a background task is running, only handle quit — skip all other actions
        if app.has_background_task() {
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                        // Allow quit even during background work
                        app.show_quit_confirm = true;
                    }
                }
            }
            continue;
        }

        // Check if startup action was taken
        if app.phase == AppPhase::Startup && app.startup.has_action() {
            // Clone the action to avoid borrow issues
            let action = app.startup.action.clone();
            match action {
                StartupAction::NewDatabase(path) => {
                    // Create and initialize new database
                    match EventStore::open(&path) {
                        Ok(mut new_store) => {
                            if let Err(e) = init_schema(new_store.connection()) {
                                app.status_message = Some(format!("Failed to initialize: {}", e));
                            } else {
                                ensure_company(&mut new_store, &path);

                                app.database_path = Some(path.clone());
                                app.load_data(&new_store);

                                // Check if accounts are empty
                                if has_no_accounts(&new_store) {
                                    app.pending_default_accounts = true;
                                }

                                store = Some(new_store);
                                app.phase = AppPhase::Main;
                                app.status_message = Some("New database created".to_string());

                                // Notify sync server of the new database
                                if let Some(ref sdb) = server_db {
                                    sdb.set(&path);
                                }
                            }
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to create database: {}", e));
                            app.startup.action = StartupAction::None;
                        }
                    }
                }
                StartupAction::OpenDatabase(path) => {
                    // Open existing database
                    match EventStore::open(&path) {
                        Ok(mut existing_store) => {
                            let path_display = path.display().to_string();

                            // Run migrations on existing databases
                            if let Err(e) = crate::store::migrations::run_migrations(
                                existing_store.connection(),
                            ) {
                                app.status_message = Some(format!("Migration failed: {}", e));
                                app.startup.action = StartupAction::None;
                                continue;
                            }

                            // Ensure company exists for sync server
                            let company_msg = ensure_company(&mut existing_store, &path);

                            app.database_path = Some(path.clone());
                            app.load_data(&existing_store);

                            // Check if accounts are empty
                            if has_no_accounts(&existing_store) {
                                app.pending_default_accounts = true;
                            }

                            store = Some(existing_store);
                            app.phase = AppPhase::Main;
                            app.status_message = Some(match company_msg {
                                Some(msg) => format!("Opened {} ({})", path_display, msg),
                                None => format!("Opened {}", path_display),
                            });

                            // Notify sync server of the opened database
                            if let Some(ref sdb) = server_db {
                                sdb.set(&path);
                            }
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to open database: {}", e));
                            app.startup.action = StartupAction::None;
                        }
                    }
                }
                StartupAction::None => {}
            }
        }

        // Handle default accounts creation
        if app.create_default_accounts {
            app.create_default_accounts = false;
            if let Some(ref mut s) = store {
                match create_default_accounts(s) {
                    Ok(count) => {
                        app.status_message = Some(format!("{} default accounts created", count));
                        app.load_data(s);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to create accounts: {}", e));
                    }
                }
            }
        }

        // Handle account form result
        match &app.account_form.result {
            AccountFormResult::Cancel => {
                app.account_form.hide();
                app.account_form.result = AccountFormResult::None;
            }
            AccountFormResult::Create(data) => {
                if let Some(ref mut s) = store {
                    let mut commands = AccountCommands::new(s, "tui-user".to_string());
                    match commands.create_account(CreateAccountCommand {
                        account_type: data.account_type,
                        account_number: data.account_number.clone(),
                        name: data.name.clone(),
                        parent_id: data.parent_id.clone(),
                        currency: Some("USD".to_string()),
                        description: data.description.clone(),
                    }) {
                        Ok(_) => {
                            app.status_message = Some(format!("Account '{}' created", data.name));
                            app.load_data(s);
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to create account: {}", e));
                        }
                    }
                }
                app.account_form.hide();
                app.account_form.result = AccountFormResult::None;
            }
            AccountFormResult::Update(data) => {
                if let Some(ref mut s) = store {
                    let mut commands = AccountCommands::new(s, "tui-user".to_string());
                    match commands.update_account(UpdateAccountCommand {
                        account_id: data.account_id.clone(),
                        account_number: Some(data.account_number.clone()),
                        name: Some(data.name.clone()),
                        parent_id: Some(data.parent_id.clone()),
                        description: Some(data.description.clone().unwrap_or_default()),
                    }) {
                        Ok(_) => {
                            app.status_message = Some(format!("Account '{}' updated", data.name));
                            app.load_data(s);
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to update account: {}", e));
                        }
                    }
                }
                app.account_form.hide();
                app.account_form.result = AccountFormResult::None;
            }
            AccountFormResult::None => {}
        }

        // Handle entry form result
        match &app.entry_form.result {
            EntryFormResult::Cancel => {
                app.entry_form.hide();
                app.entry_form.result = EntryFormResult::None;
            }
            EntryFormResult::Submit(data) => {
                if let Some(ref mut s) = store {
                    let mut commands = EntryCommands::new(s, "tui-user".to_string());
                    let lines: Vec<EntryLine> = data
                        .lines
                        .iter()
                        .map(|l| EntryLine {
                            account_id: l.account_id.clone(),
                            amount: l.amount,
                            currency: "USD".to_string(),
                            exchange_rate: None,
                            memo: None,
                        })
                        .collect();

                    match commands.post_entry(PostEntryCommand {
                        date: data.date,
                        memo: data.memo.clone(),
                        lines,
                        reference: data.reference.clone(),
                        source: None,
                    }) {
                        Ok(_) => {
                            app.status_message = Some(format!("Entry '{}' posted", data.memo));
                            app.load_data(s);
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to post entry: {}", e));
                        }
                    }
                }
                app.entry_form.hide();
                app.entry_form.result = EntryFormResult::None;
            }
            EntryFormResult::None => {}
        }

        // Handle CSV import
        if let Some(config) = app.csv_import.get_import_config() {
            if let Some(db_path) = app.database_path.clone() {
                let accounts = app.accounts.accounts.clone();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let result = (|| {
                        let mut bg_store = EventStore::open(&db_path).map_err(|e| e.to_string())?;
                        init_schema(bg_store.connection()).map_err(|e| e.to_string())?;
                        perform_csv_import(&mut bg_store, &config, &accounts)
                    })();
                    let _ = tx.send(BackgroundResult::Csv(result));
                });
                app.background_task = Some(BackgroundTask {
                    label: "Importing CSV...".to_string(),
                    receiver: rx,
                    tick: 0,
                });
            }
            app.csv_import.hide();
        }

        // Handle bank import result
        if let Some(result) = app.pending_bank_import_result.take() {
            if let Some(ref mut s) = store {
                match result {
                    BankImportResult::Cancel => {
                        app.bank_import.hide();
                    }
                    BankImportResult::Skip(import_id) => {
                        // Delete the pending import
                        let _ = s.connection().execute(
                            "UPDATE pending_imports SET status = 'skipped' WHERE id = ?1",
                            [import_id],
                        );
                        app.status_message = Some("Import skipped".to_string());
                        app.load_data(s);
                    }
                    BankImportResult::Import {
                        import_id,
                        account_id,
                        save_mapping,
                        transactions,
                    } => {
                        if let Some(db_path) = app.database_path.clone() {
                            let accounts = app.accounts.accounts.clone();
                            let (tx, rx) = mpsc::channel();
                            let count = transactions.iter().filter(|t| t.selected).count();
                            std::thread::spawn(move || {
                                let result = (|| {
                                    let mut bg_store =
                                        EventStore::open(&db_path).map_err(|e| e.to_string())?;
                                    init_schema(bg_store.connection())
                                        .map_err(|e| e.to_string())?;
                                    perform_bank_import(
                                        &mut bg_store,
                                        import_id,
                                        &account_id,
                                        save_mapping,
                                        &transactions,
                                        &accounts,
                                    )
                                })();
                                let _ = tx.send(BackgroundResult::Bank(result));
                            });
                            app.background_task = Some(BackgroundTask {
                                label: format!("Importing {} transactions...", count),
                                receiver: rx,
                                tick: 0,
                            });
                        }
                    }
                    BankImportResult::None => {}
                }
            }
        }

        // Handle Plaid config result
        match app.plaid_config.result {
            PlaidConfigResult::Saved => {
                app.plaid_config.hide();
                app.plaid_config.result = PlaidConfigResult::None;
                app.status_message = Some("Plaid configuration saved".to_string());
            }
            PlaidConfigResult::Cancel => {
                app.plaid_config.hide();
                app.plaid_config.result = PlaidConfigResult::None;
            }
            PlaidConfigResult::None => {}
        }

        // Handle Plaid view actions
        if let Some(action) = app.pending_plaid_action.take() {
            if let Some(ref mut s) = store {
                handle_plaid_action(&mut app, s, action);
            }
        }

        // Handle staged transaction review actions
        if let Some(action) = app.pending_staged_action.take() {
            if let Some(ref mut s) = store {
                handle_staged_action(&mut app, s, action);
            }
        }

        // Handle pending Plaid link (load data and show modal)
        if let Some(local_account_id) = app.pending_plaid_link.take() {
            if let Some(ref s) = store {
                open_plaid_link_modal(&mut app, s.connection(), &local_account_id);
            }
        }

        // Handle Plaid link result
        match &app.plaid_link.result {
            PlaidLinkResult::Cancel => {
                app.plaid_link.hide();
                app.plaid_link.result = PlaidLinkResult::None;
            }
            PlaidLinkResult::Link {
                item_id,
                plaid_account_id,
                local_account_id,
            } => {
                let item_id = item_id.clone();
                let plaid_account_id = plaid_account_id.clone();
                let local_account_id = local_account_id.clone();
                app.plaid_link.hide();
                app.plaid_link.result = PlaidLinkResult::None;
                if let Some(ref mut s) = store {
                    let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
                        s,
                        "tui-user".to_string(),
                    );
                    match commands.map_account(&item_id, &plaid_account_id, &local_account_id) {
                        Ok(_) => {
                            app.status_message = Some("Plaid account linked".to_string());
                            app.load_data(s);
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to link: {}", e));
                        }
                    }
                }
            }
            PlaidLinkResult::Unlink {
                item_id,
                plaid_account_id,
                local_account_id,
            } => {
                let item_id = item_id.clone();
                let plaid_account_id = plaid_account_id.clone();
                let local_account_id = local_account_id.clone();
                app.plaid_link.hide();
                app.plaid_link.result = PlaidLinkResult::None;
                if let Some(ref mut s) = store {
                    let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
                        s,
                        "tui-user".to_string(),
                    );
                    match commands.unmap_account(&item_id, &plaid_account_id, &local_account_id) {
                        Ok(_) => {
                            app.status_message = Some("Plaid account unlinked".to_string());
                            app.load_data(s);
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to unlink: {}", e));
                        }
                    }
                }
            }
            PlaidLinkResult::None => {}
        }

        // Handle account selection (switch to ledger view)
        if let Some(account) = app.accounts.take_selected_account() {
            if let Some(ref s) = store {
                app.journal.set_filter(account);
                app.load_journal_entries(s);
                app.active_view = ActiveView::Journal;
            }
        }

        // Handle jump to other account's ledger (g key in ledger view)
        if let Some((account_id, entry_id)) = app.journal.take_pending_goto_account() {
            if let Some(ref s) = store {
                // Find the account by ID
                if let Some(account) = app
                    .accounts
                    .accounts
                    .iter()
                    .find(|a| a.id == account_id)
                    .cloned()
                {
                    app.journal.set_filter(account);
                    app.load_journal_entries(s);
                    // Find and select the entry with the matching ID
                    if let Some(pos) = app
                        .journal
                        .visible_entries()
                        .iter()
                        .position(|e| e.entry_id == entry_id)
                    {
                        app.journal.state.select(Some(pos));
                    }
                }
            }
        }

        // Handle journal filter cleared (reload entries)
        if app.journal_needs_reload {
            if let Some(ref s) = store {
                app.load_journal_entries(s);
            }
            app.journal_needs_reload = false;
        }

        // Handle report date change (reload reports)
        if app.reports.needs_reload {
            if let Some(ref s) = store {
                app.load_reports(s);
            }
        }

        // Handle pending entry detail (load and show)
        if let Some(entry_id) = app.pending_entry_detail.take() {
            if let Some(ref s) = store {
                if let Some(detail) = app.load_entry_detail(s, &entry_id) {
                    app.entry_detail.show(detail);
                }
            }
        }

        // Handle pending reassignment (load lines and show picker)
        if let Some(entry_id) = app.pending_reassign.take() {
            if let Some(ref s) = store {
                let lines: Vec<(String, String, String)> = {
                    let mut stmt = match s.connection().prepare(
                        "SELECT jl.id, jl.account_id, a.name
                         FROM journal_lines jl
                         JOIN accounts a ON jl.account_id = a.id
                         WHERE jl.entry_id = ?1",
                    ) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    stmt.query_map([&entry_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })
                    .ok()
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
                };

                app.start_reassign_with_lines(entry_id, lines);
            }
        }

        // Handle pending bulk reassignment
        if app.pending_bulk_reassign {
            app.pending_bulk_reassign = false;
            if !app.journal.selected_entry_ids.is_empty() {
                use crate::tui::views::journal::ReassignAccount;
                let accounts: Vec<ReassignAccount> = app
                    .accounts
                    .accounts
                    .iter()
                    .filter(|a| a.is_active)
                    .map(|a| ReassignAccount {
                        id: a.id.clone(),
                        account_number: a.account_number.clone(),
                        name: a.name.clone(),
                    })
                    .collect();
                app.journal.start_bulk_reassign(accounts);
            }
        }

        // Handle confirmed void/unvoid
        if let Some(entry_id) = app.journal.take_confirmed_void() {
            if let Some(ref mut s) = store {
                // Check if entry is currently voided
                let is_void: bool = s
                    .connection()
                    .query_row(
                        "SELECT is_void = 1 FROM journal_entries WHERE id = ?1",
                        [&entry_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);

                let mut commands = EntryCommands::new(s, "tui-user".to_string());
                if is_void {
                    // Unvoid the entry
                    match commands.unvoid_entry(UnvoidEntryCommand {
                        entry_id: entry_id.clone(),
                        reason: "Unvoided via TUI".to_string(),
                    }) {
                        Ok(_) => {
                            app.status_message = Some("Entry unvoided".to_string());
                            app.load_data(s);
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to unvoid entry: {}", e));
                        }
                    }
                } else {
                    // Void the entry
                    match commands.void_entry(VoidEntryCommand {
                        entry_id: entry_id.clone(),
                        reason: "Voided via TUI".to_string(),
                    }) {
                        Ok(_) => {
                            app.status_message = Some("Entry voided".to_string());
                            app.load_data(s);
                        }
                        Err(e) => {
                            app.status_message = Some(format!("Failed to void entry: {}", e));
                        }
                    }
                }
            }
        }

        // Handle confirmed bulk void
        if let Some(entry_ids) = app.journal.take_bulk_void_confirmed() {
            if let Some(ref mut s) = store {
                let mut success_count = 0;
                let mut error_count = 0;

                for entry_id in entry_ids {
                    let mut commands = EntryCommands::new(s, "tui-user".to_string());
                    match commands.void_entry(VoidEntryCommand {
                        entry_id,
                        reason: "Bulk voided via TUI".to_string(),
                    }) {
                        Ok(_) => success_count += 1,
                        Err(_) => error_count += 1,
                    }
                }

                if error_count == 0 {
                    app.status_message = Some(format!("{} entries voided", success_count));
                } else {
                    app.status_message =
                        Some(format!("{} voided, {} failed", success_count, error_count));
                }
                app.load_data(s);
            }
        }

        // Handle confirmed reassignment
        if let Some((entry_id, line_id, new_account_id)) = app.journal.take_reassign_confirmed() {
            if let Some(ref mut s) = store {
                let mut commands = EntryCommands::new(s, "tui-user".to_string());
                match commands.reassign_line(ReassignLineCommand {
                    entry_id,
                    line_id,
                    new_account_id,
                }) {
                    Ok(_) => {
                        app.status_message = Some("Transaction reassigned".to_string());
                        app.load_data(s);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to reassign: {}", e));
                    }
                }
            }
        }

        // Handle confirmed bulk reassignment
        if let Some((entry_ids, new_account_id)) = app.journal.take_bulk_reassign_confirmed() {
            if let Some(ref mut s) = store {
                let filter_account_id = app.journal.filter_account.as_ref().map(|a| a.id.clone());
                let mut success_count = 0;
                let mut error_count = 0;

                for entry_id in entry_ids {
                    // Find the line to reassign (the one that's NOT the filter account)
                    let line_id: Option<String> = {
                        let mut stmt = match s.connection().prepare(
                            "SELECT jl.id FROM journal_lines jl WHERE jl.entry_id = ?1 AND jl.account_id != ?2 LIMIT 1",
                        ) {
                            Ok(s) => s,
                            Err(_) => continue,
                        };

                        let filter_id = filter_account_id.clone().unwrap_or_default();
                        stmt.query_row([&entry_id, &filter_id], |row| row.get(0))
                            .ok()
                    };

                    if let Some(line_id) = line_id {
                        let mut commands = EntryCommands::new(s, "tui-user".to_string());
                        match commands.reassign_line(ReassignLineCommand {
                            entry_id,
                            line_id,
                            new_account_id: new_account_id.clone(),
                        }) {
                            Ok(_) => success_count += 1,
                            Err(_) => error_count += 1,
                        }
                    }
                }

                if error_count == 0 {
                    app.status_message = Some(format!("{} transactions reassigned", success_count));
                } else {
                    app.status_message = Some(format!(
                        "{} reassigned, {} failed",
                        success_count, error_count
                    ));
                }
                app.load_data(s);
            }
        }

        // Poll for events with timeout
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                // Only process Press events, not Release/Repeat (crossterm 0.28+
                // sends multiple event kinds on terminals with kitty keyboard
                // protocol support, which would otherwise cause duplicated input).
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code, key.modifiers);
                }
            }
        }

        // Periodically check for new pending imports
        if let Some(ref s) = store {
            app.check_for_new_imports(s.connection());
        }

        // Close store and clear server DB when returning to startup
        if app.phase == AppPhase::Startup && store.is_some() {
            if let Some(ref sdb) = server_db {
                sdb.clear();
            }
            store = None;
        }

        if app.should_quit {
            if let Some(ref sdb) = server_db {
                sdb.clear();
            }
            break TuiResult::Quit;
        }
    };

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    drop(store); // Ensure database is closed

    Ok(result)
}

/// Run the TUI with a pre-selected database (for CLI compatibility)
pub fn run_app_with_database(
    db_path: &std::path::Path,
    server_db: Option<crate::server::ServerDb>,
) -> io::Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Open database
    let mut store = EventStore::open(db_path).map_err(io::Error::other)?;

    // Run migrations on existing databases
    crate::store::migrations::run_migrations(store.connection())
        .map_err(|e| io::Error::other(format!("Migration failed: {}", e)))?;

    // Ensure company exists for sync server
    let company_msg = ensure_company(&mut store, db_path);

    // Create app and load data
    let mut app = App::new();
    // When opening with a specific database, start in Main phase (skip Welcome/Startup)
    // but check if welcome should show first
    if should_show_welcome() {
        app.phase = AppPhase::Welcome;
    } else {
        app.phase = AppPhase::Main;
    }
    app.database_path = Some(db_path.to_path_buf());
    app.load_data(&store);

    // Check if accounts are empty
    if has_no_accounts(&store) {
        app.pending_default_accounts = true;
    }

    if let Some(msg) = company_msg {
        app.status_message = Some(msg);
    }

    // Notify sync server of the opened database
    app.sync_server_running = server_db.is_some();
    if let Some(ref sdb) = server_db {
        sdb.set(db_path);
    }

    // Main loop
    loop {
        terminal.draw(|f| draw_ui(f, &mut app))?;

        // Check for background task completion
        if let Some(result) = app.poll_background_task() {
            handle_background_result(&mut app, &mut store, result);
        }

        // While a background task is running, only handle quit — skip all other actions
        if app.has_background_task() {
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                        app.show_quit_confirm = true;
                    }
                }
            }
            continue;
        }

        // Handle default accounts creation
        if app.create_default_accounts {
            app.create_default_accounts = false;
            match create_default_accounts(&mut store) {
                Ok(count) => {
                    app.status_message = Some(format!("{} default accounts created", count));
                    app.load_data(&store);
                }
                Err(e) => {
                    app.status_message = Some(format!("Failed to create accounts: {}", e));
                }
            }
        }

        // Handle account form result
        match &app.account_form.result {
            AccountFormResult::Cancel => {
                app.account_form.hide();
                app.account_form.result = AccountFormResult::None;
            }
            AccountFormResult::Create(data) => {
                let mut commands = AccountCommands::new(&mut store, "tui-user".to_string());
                match commands.create_account(CreateAccountCommand {
                    account_type: data.account_type,
                    account_number: data.account_number.clone(),
                    name: data.name.clone(),
                    parent_id: data.parent_id.clone(),
                    currency: Some("USD".to_string()),
                    description: data.description.clone(),
                }) {
                    Ok(_) => {
                        app.status_message = Some(format!("Account '{}' created", data.name));
                        app.load_data(&store);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to create account: {}", e));
                    }
                }
                app.account_form.hide();
                app.account_form.result = AccountFormResult::None;
            }
            AccountFormResult::Update(data) => {
                let mut commands = AccountCommands::new(&mut store, "tui-user".to_string());
                match commands.update_account(UpdateAccountCommand {
                    account_id: data.account_id.clone(),
                    account_number: Some(data.account_number.clone()),
                    name: Some(data.name.clone()),
                    parent_id: Some(data.parent_id.clone()),
                    description: Some(data.description.clone().unwrap_or_default()),
                }) {
                    Ok(_) => {
                        app.status_message = Some(format!("Account '{}' updated", data.name));
                        app.load_data(&store);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to update account: {}", e));
                    }
                }
                app.account_form.hide();
                app.account_form.result = AccountFormResult::None;
            }
            AccountFormResult::None => {}
        }

        // Handle entry form result
        match &app.entry_form.result {
            EntryFormResult::Cancel => {
                app.entry_form.hide();
                app.entry_form.result = EntryFormResult::None;
            }
            EntryFormResult::Submit(data) => {
                let mut commands = EntryCommands::new(&mut store, "tui-user".to_string());
                let lines: Vec<EntryLine> = data
                    .lines
                    .iter()
                    .map(|l| EntryLine {
                        account_id: l.account_id.clone(),
                        amount: l.amount,
                        currency: "USD".to_string(),
                        exchange_rate: None,
                        memo: None,
                    })
                    .collect();

                match commands.post_entry(PostEntryCommand {
                    date: data.date,
                    memo: data.memo.clone(),
                    lines,
                    reference: data.reference.clone(),
                    source: None,
                }) {
                    Ok(_) => {
                        app.status_message = Some(format!("Entry '{}' posted", data.memo));
                        app.load_data(&store);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to post entry: {}", e));
                    }
                }
                app.entry_form.hide();
                app.entry_form.result = EntryFormResult::None;
            }
            EntryFormResult::None => {}
        }

        // Handle CSV import
        if let Some(config) = app.csv_import.get_import_config() {
            let db_path = app.database_path.clone().unwrap();
            let accounts = app.accounts.accounts.clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let result = (|| {
                    let mut bg_store = EventStore::open(&db_path).map_err(|e| e.to_string())?;
                    init_schema(bg_store.connection()).map_err(|e| e.to_string())?;
                    perform_csv_import(&mut bg_store, &config, &accounts)
                })();
                let _ = tx.send(BackgroundResult::Csv(result));
            });
            app.background_task = Some(BackgroundTask {
                label: "Importing CSV...".to_string(),
                receiver: rx,
                tick: 0,
            });
            app.csv_import.hide();
        }

        // Handle bank import result
        if let Some(result) = app.pending_bank_import_result.take() {
            match result {
                BankImportResult::Cancel => {
                    app.bank_import.hide();
                }
                BankImportResult::Skip(import_id) => {
                    // Delete the pending import
                    let _ = store.connection().execute(
                        "UPDATE pending_imports SET status = 'skipped' WHERE id = ?1",
                        [import_id],
                    );
                    app.status_message = Some("Import skipped".to_string());
                    app.load_data(&store);
                }
                BankImportResult::Import {
                    import_id,
                    account_id,
                    save_mapping,
                    transactions,
                } => {
                    let db_path = app.database_path.clone().unwrap();
                    let accounts = app.accounts.accounts.clone();
                    let (tx, rx) = mpsc::channel();
                    let count = transactions.iter().filter(|t| t.selected).count();
                    std::thread::spawn(move || {
                        let result = (|| {
                            let mut bg_store =
                                EventStore::open(&db_path).map_err(|e| e.to_string())?;
                            init_schema(bg_store.connection()).map_err(|e| e.to_string())?;
                            perform_bank_import(
                                &mut bg_store,
                                import_id,
                                &account_id,
                                save_mapping,
                                &transactions,
                                &accounts,
                            )
                        })();
                        let _ = tx.send(BackgroundResult::Bank(result));
                    });
                    app.background_task = Some(BackgroundTask {
                        label: format!("Importing {} transactions...", count),
                        receiver: rx,
                        tick: 0,
                    });
                }
                BankImportResult::None => {}
            }
        }

        // Handle Plaid config result
        match app.plaid_config.result {
            PlaidConfigResult::Saved => {
                app.plaid_config.hide();
                app.plaid_config.result = PlaidConfigResult::None;
                app.status_message = Some("Plaid configuration saved".to_string());
            }
            PlaidConfigResult::Cancel => {
                app.plaid_config.hide();
                app.plaid_config.result = PlaidConfigResult::None;
            }
            PlaidConfigResult::None => {}
        }

        // Handle Plaid view actions
        if let Some(action) = app.pending_plaid_action.take() {
            handle_plaid_action(&mut app, &mut store, action);
        }

        // Handle staged transaction review actions
        if let Some(action) = app.pending_staged_action.take() {
            handle_staged_action(&mut app, &mut store, action);
        }

        // Handle pending Plaid link (load data and show modal)
        if let Some(local_account_id) = app.pending_plaid_link.take() {
            open_plaid_link_modal(&mut app, store.connection(), &local_account_id);
        }

        // Handle Plaid link result
        match &app.plaid_link.result {
            PlaidLinkResult::Cancel => {
                app.plaid_link.hide();
                app.plaid_link.result = PlaidLinkResult::None;
            }
            PlaidLinkResult::Link {
                item_id,
                plaid_account_id,
                local_account_id,
            } => {
                let item_id = item_id.clone();
                let plaid_account_id = plaid_account_id.clone();
                let local_account_id = local_account_id.clone();
                app.plaid_link.hide();
                app.plaid_link.result = PlaidLinkResult::None;
                let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
                    &mut store,
                    "tui-user".to_string(),
                );
                match commands.map_account(&item_id, &plaid_account_id, &local_account_id) {
                    Ok(_) => {
                        app.status_message = Some("Plaid account linked".to_string());
                        app.load_data(&store);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to link: {}", e));
                    }
                }
            }
            PlaidLinkResult::Unlink {
                item_id,
                plaid_account_id,
                local_account_id,
            } => {
                let item_id = item_id.clone();
                let plaid_account_id = plaid_account_id.clone();
                let local_account_id = local_account_id.clone();
                app.plaid_link.hide();
                app.plaid_link.result = PlaidLinkResult::None;
                let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
                    &mut store,
                    "tui-user".to_string(),
                );
                match commands.unmap_account(&item_id, &plaid_account_id, &local_account_id) {
                    Ok(_) => {
                        app.status_message = Some("Plaid account unlinked".to_string());
                        app.load_data(&store);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to unlink: {}", e));
                    }
                }
            }
            PlaidLinkResult::None => {}
        }

        // Handle account selection (switch to ledger view)
        if let Some(account) = app.accounts.take_selected_account() {
            app.journal.set_filter(account);
            app.load_journal_entries(&store);
            app.active_view = ActiveView::Journal;
        }

        // Handle jump to other account's ledger (g key in ledger view)
        if let Some((account_id, entry_id)) = app.journal.take_pending_goto_account() {
            // Find the account by ID
            if let Some(account) = app
                .accounts
                .accounts
                .iter()
                .find(|a| a.id == account_id)
                .cloned()
            {
                app.journal.set_filter(account);
                app.load_journal_entries(&store);
                // Find and select the entry with the matching ID
                if let Some(pos) = app
                    .journal
                    .visible_entries()
                    .iter()
                    .position(|e| e.entry_id == entry_id)
                {
                    app.journal.state.select(Some(pos));
                }
            }
        }

        // Handle journal filter cleared (reload entries)
        if app.journal_needs_reload {
            app.load_journal_entries(&store);
            app.journal_needs_reload = false;
        }

        // Handle report date change (reload reports)
        if app.reports.needs_reload {
            app.load_reports(&store);
        }

        // Handle pending entry detail (load and show)
        if let Some(entry_id) = app.pending_entry_detail.take() {
            if let Some(detail) = app.load_entry_detail(&store, &entry_id) {
                app.entry_detail.show(detail);
            }
        }

        // Handle pending reassignment (load lines and show picker)
        if let Some(entry_id) = app.pending_reassign.take() {
            let lines: Vec<(String, String, String)> = {
                let mut stmt = match store.connection().prepare(
                    "SELECT jl.id, jl.account_id, a.name
                     FROM journal_lines jl
                     JOIN accounts a ON jl.account_id = a.id
                     WHERE jl.entry_id = ?1",
                ) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                stmt.query_map([&entry_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
            };

            app.start_reassign_with_lines(entry_id, lines);
        }

        // Handle pending bulk reassignment
        if app.pending_bulk_reassign {
            app.pending_bulk_reassign = false;
            if !app.journal.selected_entry_ids.is_empty() {
                use crate::tui::views::journal::ReassignAccount;
                let accounts: Vec<ReassignAccount> = app
                    .accounts
                    .accounts
                    .iter()
                    .filter(|a| a.is_active)
                    .map(|a| ReassignAccount {
                        id: a.id.clone(),
                        account_number: a.account_number.clone(),
                        name: a.name.clone(),
                    })
                    .collect();
                app.journal.start_bulk_reassign(accounts);
            }
        }

        // Handle confirmed void/unvoid
        if let Some(entry_id) = app.journal.take_confirmed_void() {
            // Check if entry is currently voided
            let is_void: bool = store
                .connection()
                .query_row(
                    "SELECT is_void = 1 FROM journal_entries WHERE id = ?1",
                    [&entry_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            let mut commands = EntryCommands::new(&mut store, "tui-user".to_string());
            if is_void {
                // Unvoid the entry
                match commands.unvoid_entry(UnvoidEntryCommand {
                    entry_id: entry_id.clone(),
                    reason: "Unvoided via TUI".to_string(),
                }) {
                    Ok(_) => {
                        app.status_message = Some("Entry unvoided".to_string());
                        app.load_data(&store);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to unvoid entry: {}", e));
                    }
                }
            } else {
                // Void the entry
                match commands.void_entry(VoidEntryCommand {
                    entry_id: entry_id.clone(),
                    reason: "Voided via TUI".to_string(),
                }) {
                    Ok(_) => {
                        app.status_message = Some("Entry voided".to_string());
                        app.load_data(&store);
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to void entry: {}", e));
                    }
                }
            }
        }

        // Handle confirmed bulk void
        if let Some(entry_ids) = app.journal.take_bulk_void_confirmed() {
            let mut success_count = 0;
            let mut error_count = 0;

            for entry_id in entry_ids {
                let mut commands = EntryCommands::new(&mut store, "tui-user".to_string());
                match commands.void_entry(VoidEntryCommand {
                    entry_id,
                    reason: "Bulk voided via TUI".to_string(),
                }) {
                    Ok(_) => success_count += 1,
                    Err(_) => error_count += 1,
                }
            }

            if error_count == 0 {
                app.status_message = Some(format!("{} entries voided", success_count));
            } else {
                app.status_message =
                    Some(format!("{} voided, {} failed", success_count, error_count));
            }
            app.load_data(&store);
        }

        // Handle confirmed reassignment
        if let Some((entry_id, line_id, new_account_id)) = app.journal.take_reassign_confirmed() {
            let mut commands = EntryCommands::new(&mut store, "tui-user".to_string());
            match commands.reassign_line(ReassignLineCommand {
                entry_id,
                line_id,
                new_account_id,
            }) {
                Ok(_) => {
                    app.status_message = Some("Transaction reassigned".to_string());
                    app.load_data(&store);
                }
                Err(e) => {
                    app.status_message = Some(format!("Failed to reassign: {}", e));
                }
            }
        }

        // Handle confirmed bulk reassignment
        if let Some((entry_ids, new_account_id)) = app.journal.take_bulk_reassign_confirmed() {
            let filter_account_id = app.journal.filter_account.as_ref().map(|a| a.id.clone());
            let mut success_count = 0;
            let mut error_count = 0;

            for entry_id in entry_ids {
                // Find the line to reassign (the one that's NOT the filter account)
                let line_id: Option<String> = {
                    let mut stmt = match store.connection().prepare(
                        "SELECT jl.id FROM journal_lines jl WHERE jl.entry_id = ?1 AND jl.account_id != ?2 LIMIT 1",
                    ) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    let filter_id = filter_account_id.clone().unwrap_or_default();
                    stmt.query_row([&entry_id, &filter_id], |row| row.get(0))
                        .ok()
                };

                if let Some(line_id) = line_id {
                    let mut commands = EntryCommands::new(&mut store, "tui-user".to_string());
                    match commands.reassign_line(ReassignLineCommand {
                        entry_id,
                        line_id,
                        new_account_id: new_account_id.clone(),
                    }) {
                        Ok(_) => success_count += 1,
                        Err(_) => error_count += 1,
                    }
                }
            }

            if error_count == 0 {
                app.status_message = Some(format!("{} transactions reassigned", success_count));
            } else {
                app.status_message = Some(format!(
                    "{} reassigned, {} failed",
                    success_count, error_count
                ));
            }
            app.load_data(&store);
        }

        // Poll for events with timeout
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                // Only process Press events, not Release/Repeat (crossterm 0.28+
                // sends multiple event kinds on terminals with kitty keyboard
                // protocol support, which would otherwise cause duplicated input).
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code, key.modifiers);
                }
            }
        }

        // Periodically check for new pending imports
        app.check_for_new_imports(store.connection());

        // Check if returning to startup (database closed)
        if app.phase == AppPhase::Startup {
            // Clear sync server database
            if let Some(ref sdb) = server_db {
                sdb.clear();
            }

            // Drop store and transition to startup menu
            drop(store);

            // Restore terminal
            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                LeaveAlternateScreen,
                DisableMouseCapture
            )?;
            terminal.show_cursor()?;

            // Continue with normal app flow (startup menu)
            return match run_app(server_db)? {
                TuiResult::Quit => Ok(()),
                TuiResult::OpenDatabase(_, _) => Ok(()),
            };
        }

        if app.should_quit {
            if let Some(ref sdb) = server_db {
                sdb.clear();
            }
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

/// Check if the database has a company configured; if not, create one.
/// Returns a status message describing what happened.
fn handle_plaid_action(app: &mut App, store: &mut EventStore, action: PlaidAction) {
    match action {
        PlaidAction::Configure => {
            app.plaid_config.show();
        }
        PlaidAction::Connect => {
            let config = crate::config::AppConfig::load();
            if !config.plaid.is_configured() {
                app.status_message =
                    Some("Plaid not configured. Press C to set proxy URL and API key.".to_string());
                return;
            }
            if app.sync_server_running {
                app.status_message = Some(
                    "Opening Plaid Link in browser at http://localhost:9876/plaid/link".to_string(),
                );
                let _ = open::that("http://localhost:9876/plaid/link");
            } else {
                app.status_message =
                    Some("Sync server not running. Restart with: accountir tui".to_string());
            }
        }
        PlaidAction::Sync(item_id) => {
            app.status_message = Some(format!("Syncing {}...", item_id));

            // Route through the local server, which fetches from the proxy
            // and imports transactions into the DB directly.
            // All reqwest work must happen on a separate thread to avoid tokio runtime panic.
            let sync_body = serde_json::json!({ "item_id": item_id });
            let sync_result: Result<serde_json::Value, String> = std::thread::spawn(move || {
                let client = reqwest::blocking::Client::new();
                let resp = client
                    .post("http://localhost:9876/plaid/sync")
                    .json(&sync_body)
                    .send()
                    .map_err(|e| format!("Sync request failed: {}", e))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().unwrap_or_default();
                    return Err(format!("Sync failed ({}): {}", status, text));
                }

                resp.json::<serde_json::Value>()
                    .map_err(|e| format!("Invalid sync response: {}", e))
            })
            .join()
            .unwrap_or_else(|_| Err("Sync thread panicked".to_string()));

            match sync_result {
                Ok(body) => {
                    let staged = body["staged"].as_u64().unwrap_or(0);
                    let skipped = body["skipped"].as_u64().unwrap_or(0);
                    let transfers = body["transfer_candidates"].as_u64().unwrap_or(0);
                    let mut msg = format!("Synced: {} staged, {} skipped", staged, skipped);
                    if transfers > 0 {
                        msg.push_str(&format!(", {} transfer candidates detected", transfers));
                    }
                    app.status_message = Some(msg);
                    app.load_data(store);
                }
                Err(e) => {
                    app.status_message = Some(e);
                }
            }
        }
        PlaidAction::SyncAll => {
            // Collect item IDs first to avoid borrow issues
            let item_ids: Vec<String> = app
                .plaid_view
                .items
                .iter()
                .filter(|i| i.status == "active")
                .map(|i| i.id.clone())
                .collect();

            if item_ids.is_empty() {
                app.status_message = Some("No active Plaid items to sync".to_string());
                return;
            }

            let mut total_added = 0u32;
            let mut total_skipped = 0u32;
            let mut errors = 0u32;

            for item_id in item_ids {
                // Recursively handle each sync - but simplified inline
                handle_plaid_action(app, store, PlaidAction::Sync(item_id));
                // Parse the status message to accumulate counts
                if let Some(ref msg) = app.status_message {
                    if msg.starts_with("Synced:") {
                        // Parse "Synced: X staged, Y skipped"
                        let parts: Vec<&str> = msg.split_whitespace().collect();
                        if parts.len() >= 4 {
                            total_added += parts[1].parse::<u32>().unwrap_or(0);
                            total_skipped += parts[3].parse::<u32>().unwrap_or(0);
                        }
                    } else {
                        errors += 1;
                    }
                }
            }

            app.status_message = Some(format!(
                "Sync all: {} added, {} skipped, {} errors",
                total_added, total_skipped, errors
            ));
        }
        PlaidAction::Disconnect(item_id) => {
            let mut commands =
                crate::commands::plaid_commands::PlaidCommands::new(store, "tui-user".to_string());
            match commands.disconnect_item(&item_id, "Disconnected via TUI") {
                Ok(_) => {
                    app.status_message = Some("Plaid item disconnected".to_string());
                    app.load_data(store);
                }
                Err(e) => {
                    app.status_message = Some(format!("Failed to disconnect: {}", e));
                }
            }
        }
        PlaidAction::ReviewStaged => {
            app.load_plaid_staged(store.connection());
            app.plaid_staged.show();
        }
        PlaidAction::None => {}
    }
}

fn handle_background_result(app: &mut App, store: &mut EventStore, result: BackgroundResult) {
    match result {
        BackgroundResult::Staged { message, all_done } => {
            if all_done {
                app.plaid_staged.hide();
                app.status_message = Some(message);
            } else {
                app.plaid_staged.status_message = Some(message);
            }
            app.load_plaid_staged(store.connection());
            app.load_data(store);
        }
        BackgroundResult::Csv(Ok(count)) => {
            app.status_message = Some(format!("Imported {} transactions", count));
            app.load_data(store);
        }
        BackgroundResult::Csv(Err(e)) => {
            app.status_message = Some(format!("Import failed: {}", e));
        }
        BackgroundResult::Bank(Ok(count)) => {
            app.status_message = Some(format!("Imported {} transactions", count));
            app.bank_import.hide();
            app.load_data(store);
        }
        BackgroundResult::Bank(Err(e)) => {
            app.status_message = Some(format!("Import failed: {}", e));
        }
    }
}

fn handle_staged_action(app: &mut App, store: &mut EventStore, action: StagedAction) {
    match action {
        StagedAction::Back => {
            app.plaid_staged.hide();
        }
        StagedAction::ConfirmTransfer(candidate_id) => {
            let mut commands =
                crate::commands::plaid_commands::PlaidCommands::new(store, "tui-user".to_string());
            match commands.import_transfer(&candidate_id) {
                Ok(_) => {
                    app.plaid_staged.status_message =
                        Some("Transfer imported successfully".to_string());
                    app.load_plaid_staged(store.connection());
                    app.load_data(store);
                }
                Err(e) => {
                    app.plaid_staged.status_message =
                        Some(format!("Failed to import transfer: {}", e));
                }
            }
        }
        StagedAction::RejectTransfer(candidate_id) => {
            match crate::commands::plaid_commands::reject_transfer(
                store.connection(),
                &candidate_id,
            ) {
                Ok(_) => {
                    app.plaid_staged.status_message =
                        Some("Transfer candidate rejected".to_string());
                    app.load_plaid_staged(store.connection());
                }
                Err(e) => {
                    app.plaid_staged.status_message =
                        Some(format!("Failed to reject transfer: {}", e));
                }
            }
        }
        StagedAction::ConfirmAllTransfers => {
            let candidate_ids: Vec<String> = app
                .plaid_staged
                .transfer_candidates
                .iter()
                .map(|c| c.candidate_id.clone())
                .collect();
            if let Some(db_path) = app.database_path.clone() {
                let (tx, rx) = mpsc::channel();
                let count = candidate_ids.len();
                std::thread::spawn(move || {
                    let result = (|| -> Result<(u32, u32), String> {
                        let mut bg_store = EventStore::open(&db_path).map_err(|e| e.to_string())?;
                        init_schema(bg_store.connection()).map_err(|e| e.to_string())?;
                        let mut imported = 0u32;
                        let mut errors = 0u32;
                        for cid in candidate_ids {
                            let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
                                &mut bg_store,
                                "tui-user".to_string(),
                            );
                            match commands.import_transfer(&cid) {
                                Ok(_) => imported += 1,
                                Err(_) => errors += 1,
                            }
                        }
                        Ok((imported, errors))
                    })();
                    let message = match result {
                        Ok((imported, errors)) => {
                            format!("Confirmed {} transfers, {} errors", imported, errors)
                        }
                        Err(e) => format!("Import failed: {}", e),
                    };
                    let _ = tx.send(BackgroundResult::Staged {
                        message,
                        all_done: false,
                    });
                });
                app.background_task = Some(BackgroundTask {
                    label: format!("Importing {} transfers...", count),
                    receiver: rx,
                    tick: 0,
                });
            }
        }
        StagedAction::ImportUnmatched(staged_id) => {
            let mut commands =
                crate::commands::plaid_commands::PlaidCommands::new(store, "tui-user".to_string());
            match commands.import_single_staged(&staged_id) {
                Ok(_) => {
                    app.plaid_staged.status_message = Some("Transaction imported".to_string());
                    app.load_plaid_staged(store.connection());
                    app.load_data(store);
                }
                Err(e) => {
                    app.plaid_staged.status_message = Some(format!("Failed to import: {}", e));
                }
            }
        }
        StagedAction::ImportAll => {
            if let Some(db_path) = app.database_path.clone() {
                let total = app.plaid_staged.total_pending();
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let result = (|| -> Result<(u32, u32), String> {
                        let mut bg_store = EventStore::open(&db_path).map_err(|e| e.to_string())?;
                        init_schema(bg_store.connection()).map_err(|e| e.to_string())?;
                        let mut commands = crate::commands::plaid_commands::PlaidCommands::new(
                            &mut bg_store,
                            "tui-user".to_string(),
                        );
                        commands.import_all_staged().map_err(|e| e.to_string())
                    })();
                    let (message, all_done) = match result {
                        Ok((transfers, unmatched)) => (
                            format!("Imported {} transfers, {} unmatched", transfers, unmatched),
                            transfers + unmatched > 0,
                        ),
                        Err(e) => (format!("Import failed: {}", e), false),
                    };
                    let _ = tx.send(BackgroundResult::Staged { message, all_done });
                });
                app.background_task = Some(BackgroundTask {
                    label: format!("Importing {} transactions...", total),
                    receiver: rx,
                    tick: 0,
                });
            }
        }
        StagedAction::None => {}
    }
}

fn open_plaid_link_modal(app: &mut App, conn: &rusqlite::Connection, local_account_id: &str) {
    use super::views::plaid_link::{CurrentMapping, PlaidAccountOption};

    // Find the local account name
    let local_account_name = app
        .accounts
        .accounts
        .iter()
        .find(|a| a.id == local_account_id)
        .map(|a| a.name.clone())
        .unwrap_or_default();

    // Check for current mapping
    let current_mapping: Option<CurrentMapping> = conn
        .prepare(
            "SELECT pla.item_id, pla.plaid_account_id, pla.local_account_id, pi.institution_name, pla.name, pla.mask
             FROM plaid_local_accounts pla
             JOIN plaid_items pi ON pla.item_id = pi.id
             WHERE pla.local_account_id = ?1 AND pi.status = 'active'",
        )
        .and_then(|mut stmt| {
            stmt.query_row([local_account_id], |row| {
                Ok(CurrentMapping {
                    item_id: row.get(0)?,
                    plaid_account_id: row.get(1)?,
                    local_account_id: row.get(2)?,
                    institution_name: row.get(3)?,
                    plaid_account_name: row.get(4)?,
                    mask: row.get(5)?,
                })
            })
        })
        .ok();

    // Load all available Plaid accounts from active items
    let available: Vec<PlaidAccountOption> = conn
        .prepare(
            "SELECT pla.item_id, pla.plaid_account_id, pi.institution_name, pla.name, pla.mask, pla.account_type, pla.local_account_id
             FROM plaid_local_accounts pla
             JOIN plaid_items pi ON pla.item_id = pi.id
             WHERE pi.status = 'active'
             ORDER BY pi.institution_name, pla.name",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                let mapped_local_id: Option<String> = row.get(6)?;
                Ok(PlaidAccountOption {
                    item_id: row.get(0)?,
                    plaid_account_id: row.get(1)?,
                    institution_name: row.get(2)?,
                    account_name: row.get(3)?,
                    mask: row.get(4)?,
                    account_type: row.get(5)?,
                    mapped_to_local_id: mapped_local_id.clone(),
                    mapped_to_local_name: mapped_local_id.as_ref().and_then(|lid| {
                        app.accounts.accounts.iter().find(|a| &a.id == lid).map(|a| a.name.clone())
                    }),
                })
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();

    app.plaid_link.show(
        local_account_id.to_string(),
        local_account_name,
        current_mapping,
        available,
    );
}

fn ensure_company(store: &mut EventStore, db_path: &std::path::Path) -> Option<String> {
    let has_company: bool = store
        .connection()
        .query_row(
            "SELECT COUNT(*) > 0 FROM company WHERE id = 'default'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if has_company {
        return None;
    }

    let company_name = db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("My Company")
        .to_string();
    let company_id = uuid::Uuid::new_v4().to_string();
    let envelope = crate::events::types::EventEnvelope::new(
        DomainEvent::CompanyCreated {
            company_id,
            name: company_name.clone(),
            base_currency: "USD".to_string(),
            fiscal_year_start: 1,
        },
        "system".to_string(),
    );
    match store.append(envelope) {
        Ok(stored) => {
            let projector = crate::store::projections::Projector::new(store.connection());
            if let Err(e) = projector.apply(&stored) {
                return Some(format!("Failed to project company: {}", e));
            }
            Some(format!("Company '{}' created for sync", company_name))
        }
        Err(e) => Some(format!("Failed to create company: {}", e)),
    }
}

/// Check if the database has any accounts
fn has_no_accounts(store: &EventStore) -> bool {
    let count: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE is_active = 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    count == 0
}

/// Create the default chart of accounts
fn create_default_accounts(store: &mut EventStore) -> Result<usize, String> {
    use crate::commands::account_commands::{AccountCommands, CreateAccountCommand};
    use crate::domain::AccountType;

    // Define the default accounts: (number, name, type, parent_number)
    let defaults: Vec<(&str, &str, AccountType, Option<&str>)> = vec![
        ("1000", "Assets", AccountType::Asset, None),
        (
            "1001",
            "Business Checking",
            AccountType::Asset,
            Some("1000"),
        ),
        ("2000", "Income", AccountType::Revenue, None),
        ("3000", "Expenses", AccountType::Expense, None),
        ("4000", "Equity", AccountType::Equity, None),
        (
            "4001",
            "Opening Balances",
            AccountType::Equity,
            Some("4000"),
        ),
        ("5000", "Liabilities", AccountType::Liability, None),
    ];

    // First pass: create accounts without parent references, collect IDs
    let mut account_ids: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut created = 0;

    for (number, name, account_type, _parent_number) in &defaults {
        let mut commands = AccountCommands::new(store, "system".to_string());
        let cmd = CreateAccountCommand {
            account_type: *account_type,
            account_number: number.to_string(),
            name: name.to_string(),
            parent_id: None,
            currency: Some("USD".to_string()),
            description: None,
        };
        match commands.create_account(cmd) {
            Ok(stored) => {
                if let crate::events::types::Event::AccountCreated { account_id, .. } =
                    &stored.event
                {
                    account_ids.insert(number.to_string(), account_id.clone());
                }
                created += 1;
            }
            Err(e) => return Err(format!("Failed to create account {}: {}", number, e)),
        }
    }

    // Second pass: set parent_id for child accounts
    for (number, _name, _account_type, parent_number) in &defaults {
        if let Some(parent_num) = parent_number {
            let account_id = account_ids.get(*number).cloned();
            let parent_id = account_ids.get(*parent_num).cloned();
            if let (Some(aid), Some(pid)) = (account_id, parent_id) {
                let mut commands = AccountCommands::new(store, "system".to_string());
                let cmd = crate::commands::account_commands::UpdateAccountCommand {
                    account_id: aid,
                    account_number: None,
                    name: None,
                    parent_id: Some(Some(pid)),
                    description: None,
                };
                if let Err(e) = commands.update_account(cmd) {
                    return Err(format!("Failed to set parent for {}: {}", number, e));
                }
            }
        }
    }

    Ok(created)
}

/// Perform CSV import
fn perform_csv_import(
    store: &mut EventStore,
    config: &ImportConfig,
    existing_accounts: &[crate::domain::Account],
) -> Result<usize, String> {
    // Read the CSV file
    let content = std::fs::read_to_string(&config.file_path)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    let mut lines = content.lines();

    // Skip any leading lines the user marked as non-data (e.g. bank preamble).
    for _ in 0..config.skip_lines {
        lines.next();
    }

    // Skip header row if the user said there is one.
    if config.has_header {
        lines.next();
    }

    // Target account is required
    let target_account_id = config
        .target_account_id
        .clone()
        .ok_or("Target account is required for CSV import")?;

    // Find or create Uncategorized account for offset entries
    let uncategorized_id = find_or_create_uncategorized_account(store, existing_accounts)?;

    // Get target account type to determine debit/credit direction
    let target_is_asset = existing_accounts
        .iter()
        .find(|a| a.id == target_account_id)
        .map(|a| matches!(a.account_type, AccountType::Asset))
        .unwrap_or(true);

    let mut count = 0;
    let mut commands = EntryCommands::new(store, "csv-import".to_string());

    for line in lines {
        let fields = parse_delimited_line(line, config.delimiter);

        // Extract fields based on column mapping
        let date_str = fields
            .get(config.date_column)
            .map(|s| s.as_str())
            .unwrap_or("");
        let description = fields
            .get(config.description_column)
            .map(|s| s.as_str())
            .unwrap_or("");
        let amount_str = fields
            .get(config.amount_column)
            .map(|s| s.as_str())
            .unwrap_or("");

        // Parse date
        let date = match parse_date(date_str) {
            Some(d) => d,
            None => continue, // Skip rows with invalid dates
        };

        // Parse amount
        let amount = match parse_amount(amount_str) {
            Some(a) if a != 0 => a,
            _ => continue, // Skip rows with invalid or zero amounts
        };

        // For asset accounts (like checking):
        // - Positive amount = money coming in = debit to asset, credit to uncategorized
        // - Negative amount = money going out = credit to asset, debit to uncategorized
        //
        // For liability/expense accounts, the logic reverses

        let (target_amount, offset_amount) = if target_is_asset {
            // Asset account: positive CSV amount = debit (increase)
            (amount, -amount)
        } else {
            // Liability/equity/revenue: positive CSV amount = credit (increase)
            (-amount, amount)
        };

        // Create journal entry
        let entry_lines = vec![
            EntryLine {
                account_id: target_account_id.clone(),
                amount: target_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
            EntryLine {
                account_id: uncategorized_id.clone(),
                amount: offset_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
        ];

        match commands.post_entry(PostEntryCommand {
            date,
            memo: description.to_string(),
            lines: entry_lines,
            reference: None,
            source: Some(JournalEntrySource::Import),
        }) {
            Ok(_) => count += 1,
            Err(e) => {
                // Log error but continue with other entries
                eprintln!("Failed to import row: {}", e);
            }
        }
    }

    Ok(count)
}

/// Find or create the Uncategorized account
fn find_or_create_uncategorized_account(
    store: &mut EventStore,
    existing_accounts: &[crate::domain::Account],
) -> Result<String, String> {
    // Look for existing Uncategorized account
    for account in existing_accounts {
        if account.name.to_lowercase() == "uncategorized" {
            return Ok(account.id.clone());
        }
    }

    // Create new Uncategorized account
    let mut commands = AccountCommands::new(store, "csv-import".to_string());

    // Find next available account number in 9000 range (other expenses)
    let mut next_number = 9000;
    for account in existing_accounts {
        if let Ok(num) = account.account_number.parse::<u32>() {
            if (9000..10000).contains(&num) && num >= next_number {
                next_number = num + 1;
            }
        }
    }

    match commands.create_account(CreateAccountCommand {
        account_type: AccountType::Expense,
        account_number: next_number.to_string(),
        name: "Uncategorized".to_string(),
        parent_id: None,
        currency: Some("USD".to_string()),
        description: Some("Uncategorized transactions from CSV import".to_string()),
    }) {
        Ok(stored_event) => {
            // Extract account_id from the stored event
            if let DomainEvent::AccountCreated { account_id, .. } = stored_event.event {
                Ok(account_id)
            } else {
                Err("Unexpected event type returned".to_string())
            }
        }
        Err(e) => Err(format!("Failed to create Uncategorized account: {}", e)),
    }
}

/// Perform the bank import - create journal entries for all transactions
fn perform_bank_import(
    store: &mut EventStore,
    import_id: i64,
    account_id: &str,
    save_mapping: bool,
    transactions: &[ParsedTransaction],
    existing_accounts: &[crate::domain::Account],
) -> Result<usize, String> {
    // Get the pending import info to access bank_id
    let (bank_id, bank_name): (Option<String>, String) = store
        .connection()
        .query_row(
            "SELECT bank_id, bank_name FROM pending_imports WHERE id = ?1",
            [import_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("Failed to get import info: {}", e))?;

    // Find or create Uncategorized account for offset entries
    let uncategorized_id = find_or_create_uncategorized_account(store, existing_accounts)?;

    // Get target account type to determine debit/credit direction
    let target_account = existing_accounts.iter().find(|a| a.id == account_id);
    let target_is_asset = target_account
        .map(|a| matches!(a.account_type, AccountType::Asset))
        .unwrap_or(true);
    let target_is_liability = target_account
        .map(|a| matches!(a.account_type, AccountType::Liability))
        .unwrap_or(false);

    let mut count = 0;
    let mut commands = EntryCommands::new(store, "bank-import".to_string());

    for txn in transactions.iter().filter(|t| t.selected) {
        // Determine the correct debit/credit direction based on account type and sign
        //
        // For ASSET accounts (e.g., checking):
        //   - Positive amount (deposit) = debit to asset (increase), credit to uncategorized
        //   - Negative amount (withdrawal) = credit to asset (decrease), debit to uncategorized
        //
        // For LIABILITY accounts (e.g., credit card):
        //   - Positive amount (payment) = debit to liability (decrease), credit to uncategorized
        //   - Negative amount (charge) = credit to liability (increase), debit to uncategorized
        //
        // The user specified:
        //   - Asset accounts: positives increase balance, negatives decrease balance
        //   - Liability accounts: positives are payments (decrease balance), negatives are charges (increase balance)

        let (target_amount, offset_amount) = if target_is_asset {
            // Asset account: positive = debit (increase), negative = credit (decrease)
            (txn.amount, -txn.amount)
        } else if target_is_liability {
            // Liability account: positive = payment (debit to decrease), negative = charge (credit to increase)
            (txn.amount, -txn.amount)
        } else {
            // Other account types: same as asset for now
            (txn.amount, -txn.amount)
        };

        let entry_lines = vec![
            EntryLine {
                account_id: account_id.to_string(),
                amount: target_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
            EntryLine {
                account_id: uncategorized_id.clone(),
                amount: offset_amount,
                currency: "USD".to_string(),
                exchange_rate: None,
                memo: None,
            },
        ];

        match commands.post_entry(PostEntryCommand {
            date: txn.date,
            memo: txn.description.clone(),
            lines: entry_lines,
            reference: None,
            source: Some(JournalEntrySource::Import),
        }) {
            Ok(_) => count += 1,
            Err(e) => {
                eprintln!("Failed to import transaction: {}", e);
            }
        }
    }

    // Save the bank-account mapping if requested and we have a bank_id
    if save_mapping {
        if let Some(ref bid) = bank_id {
            let _ = store.connection().execute(
                "INSERT OR REPLACE INTO bank_accounts (bank_id, bank_name, account_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![bid, bank_name, account_id],
            );
        }
    }

    // Mark the import as processed
    let _ = store.connection().execute(
        "UPDATE pending_imports SET status = 'imported', imported_count = ?1, processed_at = datetime('now') WHERE id = ?2",
        rusqlite::params![count as i64, import_id],
    );

    Ok(count)
}
