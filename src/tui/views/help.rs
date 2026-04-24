use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::tui::theme::Theme;

/// Help content for different views
pub struct HelpModal {
    pub visible: bool,
}

impl HelpModal {
    pub fn new() -> Self {
        Self { visible: false }
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    pub fn show(&mut self) {
        self.visible = true;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, context: HelpContext, theme: &Theme) {
        if !self.visible {
            return;
        }

        // Create a centered modal
        let modal_area = centered_rect(60, 70, area);

        // Clear the area behind the modal
        frame.render_widget(Clear, modal_area);

        let content = self.get_help_content(context, theme);

        let paragraph = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.border_style())
                    .title(" Help - Press ? or Esc to close ")
                    .title_style(theme.modal_title_style()),
            )
            .style(theme.text_style());

        frame.render_widget(paragraph, modal_area);
    }

    fn get_help_content(&self, context: HelpContext, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled("  Global Keys", theme.header_style())),
            Line::from(""),
            Self::key_line("  ?", "Toggle this help menu", theme),
            Self::key_line("  Esc", "Close file / go back", theme),
            Self::key_line("  q", "Quit application", theme),
            Self::key_line("  Ctrl+C", "Force quit", theme),
            Self::key_line("  Tab", "Next view", theme),
            Self::key_line("  Shift+Tab", "Previous view", theme),
            Self::key_line("  ,", "Open settings", theme),
            Self::key_line(
                "  1-6",
                "Jump to view (Dashboard/Accounts/Journal/Reports/Events/Plaid)",
                theme,
            ),
            Line::from(""),
        ];

        // Add context-specific help
        let context_lines = match context {
            HelpContext::Startup => self.startup_help(theme),
            HelpContext::Dashboard => self.dashboard_help(theme),
            HelpContext::Accounts => self.accounts_help(theme),
            HelpContext::Journal => self.journal_help(theme),
            HelpContext::Reports => self.reports_help(theme),
            HelpContext::EventLog => self.event_log_help(theme),
            HelpContext::Plaid => self.plaid_help(theme),
        };

        lines.extend(context_lines);
        lines
    }

    fn key_line(key: &'static str, description: &'static str, theme: &Theme) -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{:<16}", key), Style::default().fg(theme.success)),
            Span::raw(description),
        ])
    }

    fn section_header(title: &'static str, theme: &Theme) -> Line<'static> {
        Line::from(Span::styled(title, theme.header_style()))
    }

    fn section_label(label: &'static str, theme: &Theme) -> Line<'static> {
        Line::from(Span::styled(label, Style::default().fg(theme.info)))
    }

    fn startup_help(&self, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Self::section_header("  Startup Screen", theme),
            Line::from(""),
            Self::key_line("  ↑/↓ or j/k", "Navigate menu", theme),
            Self::key_line("  Enter", "Select option", theme),
            Self::key_line("  Esc", "Cancel input", theme),
            Line::from(""),
            Self::section_label("  Options:", theme),
            Line::from("  • Create New Database - Start a fresh accounting file"),
            Line::from("  • Open Database - Open an existing .db file"),
            Line::from("  • Recent Databases - Quick access to found .db files"),
        ]
    }

    fn dashboard_help(&self, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Self::section_header("  Dashboard View", theme),
            Line::from(""),
            Line::from("  The dashboard shows a financial overview:"),
            Line::from(""),
            Self::section_label("  Summary Section:", theme),
            Line::from("  • Total Assets, Liabilities, and Equity"),
            Line::from("  • Net Income for the period"),
            Line::from(""),
            Self::section_label("  Account Breakdown:", theme),
            Line::from("  • Assets and Liabilities (left side)"),
            Line::from("  • Revenue and Expenses (right side)"),
        ]
    }

    fn accounts_help(&self, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Self::section_header("  Accounts View", theme),
            Line::from(""),
            Self::key_line("  Enter", "View account ledger", theme),
            Self::key_line("  a", "Create new account", theme),
            Self::key_line("  e", "Edit selected account", theme),
            Self::key_line(
                "  p",
                "Link/unlink Plaid account (Asset/Liability only)",
                theme,
            ),
            Self::key_line("  ↑/↓ or j/k", "Navigate accounts", theme),
            Self::key_line("  Home", "Jump to first account", theme),
            Self::key_line("  End", "Jump to last account", theme),
            Line::from(""),
            Self::section_label("  Account Form (Parent field):", theme),
            Line::from("  • Type to search/filter accounts"),
            Line::from("  • ↑/↓ to navigate matches"),
            Line::from("  • Enter to confirm selection"),
            Line::from("  • Esc to clear filter"),
            Line::from(""),
            Self::section_label("  Columns:", theme),
            Line::from("  • Number - Account number (e.g., 1000)"),
            Line::from("  • Name - Account name (indented for children)"),
            Line::from("  • Type - Asset/Liability/Equity/Revenue/Expense"),
            Line::from("  • Balance - Current balance"),
            Line::from("  • Plaid - Linked bank institution and mask"),
            Line::from("  • Status - Active or Inactive"),
        ]
    }

    fn journal_help(&self, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Self::section_header("  Journal View", theme),
            Line::from(""),
            Self::key_line("  Enter", "View transaction details", theme),
            Self::key_line("  e", "Create new journal entry", theme),
            Self::key_line("  i", "Import transactions from CSV", theme),
            Self::key_line("  f", "Find subscriptions in Uncategorized", theme),
            Self::key_line("  a", "Reassign transaction to different account", theme),
            Self::key_line("  g", "Go to other account's ledger (ledger only)", theme),
            Self::key_line("  x", "Void selected entry", theme),
            Self::key_line("  v", "Enter multiselect mode (ledger only)", theme),
            Self::key_line(
                "  s",
                "Cycle sort field (Date/Memo/Reference/Amount)",
                theme,
            ),
            Self::key_line("  r", "Reverse sort direction", theme),
            Self::key_line("  h", "Toggle showing voided entries", theme),
            Self::key_line("  c", "Toggle ID column visibility", theme),
            Self::key_line("  ↑/↓ or j/k", "Navigate entries", theme),
            Self::key_line("  PgUp/PgDn", "Page through entries", theme),
            Self::key_line("  Home", "Jump to first entry", theme),
            Self::key_line("  End", "Jump to last entry", theme),
            Line::from(""),
            Self::section_label("  Multiselect Mode (ledger view only):", theme),
            Line::from("  Press 'v' to enter multiselect mode"),
            Self::key_line("  ↑/↓", "Navigate (selects when active)", theme),
            Self::key_line("  Space", "Toggle selection on/off", theme),
            Line::from("    Selection starts active; Space pauses/resumes"),
            Self::key_line("  a", "Assign all selected to an account", theme),
            Self::key_line("  x", "Void all selected entries", theme),
            Self::key_line("  Esc", "Exit multiselect mode", theme),
            Line::from(""),
            Self::section_label("  Journal Columns:", theme),
            Line::from("  • Date, Memo, Reference, Amount, Status"),
            Line::from(""),
            Self::section_label("  Ledger Columns (when viewing single account):", theme),
            Line::from("  • Date, Account, Memo, Reference, Debit, Credit, Balance"),
        ]
    }

    fn reports_help(&self, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Self::section_header("  Reports View", theme),
            Line::from(""),
            Self::key_line("  ←/→ or h/l", "Switch between reports", theme),
            Self::key_line("  ↑/↓ or j/k", "Scroll report content", theme),
            Self::key_line("  d", "Enter specific date (YYYY-MM-DD)", theme),
            Self::key_line("  [", "Previous day", theme),
            Self::key_line("  ]", "Next day", theme),
            Self::key_line("  t", "Reset to today", theme),
            Line::from(""),
            Self::section_label("  Available Reports:", theme),
            Line::from("  • Trial Balance - All accounts with debit/credit totals"),
            Line::from("  • Balance Sheet - Assets = Liabilities + Equity"),
            Line::from("  • Income Statement - Revenue - Expenses = Net Income"),
            Line::from(""),
            Self::section_label("  Note:", theme),
            Line::from("  Balance sheet and income statement use the"),
            Line::from("  selected report date. Use [/] to change date."),
        ]
    }

    fn plaid_help(&self, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Self::section_header("  Plaid View", theme),
            Line::from(""),
            Self::key_line("  C", "Configure Plaid proxy URL and API key", theme),
            Self::key_line("  c", "Connect a new bank via Plaid Link", theme),
            Self::key_line("  s", "Sync transactions for selected item", theme),
            Self::key_line("  S", "Sync all active items", theme),
            Self::key_line("  d", "Disconnect selected item", theme),
            Self::key_line("  ↑/↓ or j/k", "Navigate items", theme),
            Line::from(""),
            Self::section_label("  About Plaid:", theme),
            Line::from("  Connect bank accounts via Plaid to automatically"),
            Line::from("  import transactions. Use the Accounts view (key p)"),
            Line::from("  to link Plaid accounts to local accounts."),
            Line::from(""),
            Self::section_label("  Setup:", theme),
            Line::from("  1. Press 'C' to configure proxy URL and API key"),
            Line::from("  2. Press 'c' to connect a bank"),
            Line::from("  3. Map accounts in the Accounts view (p)"),
            Line::from("  4. Press 's' to sync transactions"),
        ]
    }

    fn event_log_help(&self, theme: &Theme) -> Vec<Line<'static>> {
        vec![
            Self::section_header("  Event Log View", theme),
            Line::from(""),
            Self::key_line("  ↑/↓ or j/k", "Navigate events", theme),
            Self::key_line("  PgUp/PgDn", "Page through events", theme),
            Self::key_line("  Home", "Jump to oldest event", theme),
            Self::key_line("  End", "Jump to newest event", theme),
            Line::from(""),
            Self::section_label("  About the Event Log:", theme),
            Line::from("  The event log shows all changes made to the"),
            Line::from("  accounting data in chronological order."),
            Line::from(""),
            Line::from("  Each event includes:"),
            Line::from("  • Timestamp - When the action occurred"),
            Line::from("  • Event Type - What kind of change"),
            Line::from("  • Entity ID - The affected record"),
            Line::from("  • Summary - Brief description"),
            Line::from(""),
            Line::from("  Events are immutable and form an audit trail."),
        ]
    }
}

impl Default for HelpModal {
    fn default() -> Self {
        Self::new()
    }
}

/// Context for which view the help is being shown
#[derive(Debug, Clone, Copy)]
pub enum HelpContext {
    Startup,
    Dashboard,
    Accounts,
    Journal,
    Reports,
    EventLog,
    Plaid,
}

/// Helper function to create a centered rectangle
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
