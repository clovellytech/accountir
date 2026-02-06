use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

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

    pub fn draw(&self, frame: &mut Frame, area: Rect, context: HelpContext) {
        if !self.visible {
            return;
        }

        // Create a centered modal
        let modal_area = centered_rect(60, 70, area);

        // Clear the area behind the modal
        frame.render_widget(Clear, modal_area);

        let content = self.get_help_content(context);

        let paragraph = Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Help - Press ? or Esc to close ")
                    .title_style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
            )
            .style(Style::default().fg(Color::White));

        frame.render_widget(paragraph, modal_area);
    }

    fn get_help_content(&self, context: HelpContext) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  Global Keys",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("  ?", "Toggle this help menu"),
            Self::key_line("  Esc", "Close file / go back"),
            Self::key_line("  q", "Quit application"),
            Self::key_line("  Ctrl+C", "Force quit"),
            Self::key_line("  Tab", "Next view"),
            Self::key_line("  Shift+Tab", "Previous view"),
            Self::key_line(
                "  1-6",
                "Jump to view (Dashboard/Accounts/Journal/Reports/Events/Plaid)",
            ),
            Line::from(""),
        ];

        // Add context-specific help
        let context_lines = match context {
            HelpContext::Startup => self.startup_help(),
            HelpContext::Dashboard => self.dashboard_help(),
            HelpContext::Accounts => self.accounts_help(),
            HelpContext::Journal => self.journal_help(),
            HelpContext::Reports => self.reports_help(),
            HelpContext::EventLog => self.event_log_help(),
            HelpContext::Plaid => self.plaid_help(),
        };

        lines.extend(context_lines);
        lines
    }

    fn key_line(key: &'static str, description: &'static str) -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{:<16}", key), Style::default().fg(Color::Green)),
            Span::raw(description),
        ])
    }

    fn startup_help(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "  Startup Screen",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("  ↑/↓ or j/k", "Navigate menu"),
            Self::key_line("  Enter", "Select option"),
            Self::key_line("  Esc", "Cancel input"),
            Line::from(""),
            Line::from(Span::styled("  Options:", Style::default().fg(Color::Cyan))),
            Line::from("  • Create New Database - Start a fresh accounting file"),
            Line::from("  • Open Database - Open an existing .db file"),
            Line::from("  • Recent Databases - Quick access to found .db files"),
        ]
    }

    fn dashboard_help(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "  Dashboard View",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  The dashboard shows a financial overview:"),
            Line::from(""),
            Line::from(Span::styled(
                "  Summary Section:",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  • Total Assets, Liabilities, and Equity"),
            Line::from("  • Net Income for the period"),
            Line::from(""),
            Line::from(Span::styled(
                "  Account Breakdown:",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  • Assets and Liabilities (left side)"),
            Line::from("  • Revenue and Expenses (right side)"),
        ]
    }

    fn accounts_help(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "  Accounts View",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("  Enter", "View account ledger"),
            Self::key_line("  a", "Create new account"),
            Self::key_line("  e", "Edit selected account"),
            Self::key_line("  p", "Link/unlink Plaid account (Asset/Liability only)"),
            Self::key_line("  ↑/↓ or j/k", "Navigate accounts"),
            Self::key_line("  Home", "Jump to first account"),
            Self::key_line("  End", "Jump to last account"),
            Line::from(""),
            Line::from(Span::styled(
                "  Account Form (Parent field):",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  • Type to search/filter accounts"),
            Line::from("  • ↑/↓ to navigate matches"),
            Line::from("  • Enter to confirm selection"),
            Line::from("  • Esc to clear filter"),
            Line::from(""),
            Line::from(Span::styled("  Columns:", Style::default().fg(Color::Cyan))),
            Line::from("  • Number - Account number (e.g., 1000)"),
            Line::from("  • Name - Account name (indented for children)"),
            Line::from("  • Type - Asset/Liability/Equity/Revenue/Expense"),
            Line::from("  • Balance - Current balance"),
            Line::from("  • Plaid - Linked bank institution and mask"),
            Line::from("  • Status - Active or Inactive"),
        ]
    }

    fn journal_help(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "  Journal View",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("  Enter", "View transaction details"),
            Self::key_line("  e", "Create new journal entry"),
            Self::key_line("  i", "Import transactions from CSV"),
            Self::key_line("  a", "Reassign transaction to different account"),
            Self::key_line("  g", "Go to other account's ledger (ledger only)"),
            Self::key_line("  x", "Void selected entry"),
            Self::key_line("  v", "Enter multiselect mode (ledger only)"),
            Self::key_line("  s", "Cycle sort field (Date/Memo/Reference/Amount)"),
            Self::key_line("  r", "Reverse sort direction"),
            Self::key_line("  h", "Toggle showing voided entries"),
            Self::key_line("  c", "Toggle ID column visibility"),
            Self::key_line("  ↑/↓ or j/k", "Navigate entries"),
            Self::key_line("  PgUp/PgDn", "Page through entries"),
            Self::key_line("  Home", "Jump to first entry"),
            Self::key_line("  End", "Jump to last entry"),
            Line::from(""),
            Line::from(Span::styled(
                "  Multiselect Mode (ledger view only):",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  Press 'v' to enter multiselect mode"),
            Self::key_line("  ↑/↓", "Navigate (selects when active)"),
            Self::key_line("  Space", "Toggle selection on/off"),
            Line::from("    Selection starts active; Space pauses/resumes"),
            Self::key_line("  a", "Assign all selected to an account"),
            Self::key_line("  x", "Void all selected entries"),
            Self::key_line("  Esc", "Exit multiselect mode"),
            Line::from(""),
            Line::from(Span::styled(
                "  Journal Columns:",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  • Date, Memo, Reference, Amount, Status"),
            Line::from(""),
            Line::from(Span::styled(
                "  Ledger Columns (when viewing single account):",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  • Date, Account, Memo, Reference, Debit, Credit, Balance"),
        ]
    }

    fn reports_help(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "  Reports View",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("  ←/→ or h/l", "Switch between reports"),
            Self::key_line("  ↑/↓ or j/k", "Scroll report content"),
            Self::key_line("  d", "Enter specific date (YYYY-MM-DD)"),
            Self::key_line("  [", "Previous day"),
            Self::key_line("  ]", "Next day"),
            Self::key_line("  t", "Reset to today"),
            Line::from(""),
            Line::from(Span::styled(
                "  Available Reports:",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  • Trial Balance - All accounts with debit/credit totals"),
            Line::from("  • Balance Sheet - Assets = Liabilities + Equity"),
            Line::from("  • Income Statement - Revenue - Expenses = Net Income"),
            Line::from(""),
            Line::from(Span::styled("  Note:", Style::default().fg(Color::Cyan))),
            Line::from("  Balance sheet and income statement use the"),
            Line::from("  selected report date. Use [/] to change date."),
        ]
    }

    fn plaid_help(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "  Plaid View",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("  C", "Configure Plaid proxy URL and API key"),
            Self::key_line("  c", "Connect a new bank via Plaid Link"),
            Self::key_line("  s", "Sync transactions for selected item"),
            Self::key_line("  S", "Sync all active items"),
            Self::key_line("  d", "Disconnect selected item"),
            Self::key_line("  ↑/↓ or j/k", "Navigate items"),
            Line::from(""),
            Line::from(Span::styled(
                "  About Plaid:",
                Style::default().fg(Color::Cyan),
            )),
            Line::from("  Connect bank accounts via Plaid to automatically"),
            Line::from("  import transactions. Use the Accounts view (key p)"),
            Line::from("  to link Plaid accounts to local accounts."),
            Line::from(""),
            Line::from(Span::styled("  Setup:", Style::default().fg(Color::Cyan))),
            Line::from("  1. Press 'C' to configure proxy URL and API key"),
            Line::from("  2. Press 'c' to connect a bank"),
            Line::from("  3. Map accounts in the Accounts view (p)"),
            Line::from("  4. Press 's' to sync transactions"),
        ]
    }

    fn event_log_help(&self) -> Vec<Line<'static>> {
        vec![
            Line::from(Span::styled(
                "  Event Log View",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("  ↑/↓ or j/k", "Navigate events"),
            Self::key_line("  PgUp/PgDn", "Page through events"),
            Self::key_line("  Home", "Jump to oldest event"),
            Self::key_line("  End", "Jump to newest event"),
            Line::from(""),
            Line::from(Span::styled(
                "  About the Event Log:",
                Style::default().fg(Color::Cyan),
            )),
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
