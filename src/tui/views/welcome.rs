use crossterm::event::KeyCode;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use std::path::PathBuf;

use crate::tui::theme::Theme;

/// Config file location for storing preferences
fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("accountir").join("config.json"))
}

/// Check if welcome screen should be shown
pub fn should_show_welcome() -> bool {
    let Some(path) = config_path() else {
        return true; // Show by default if can't determine config path
    };

    if !path.exists() {
        return true; // Show by default if no config exists
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            // Parse JSON manually to avoid extra dependency
            !content.contains("\"show_welcome\":false")
        }
        Err(_) => true,
    }
}

/// Save the "don't show welcome" preference
pub fn set_show_welcome(show: bool) {
    let Some(path) = config_path() else {
        return;
    };

    // Ensure config directory exists
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let content = if show {
        "{\"show_welcome\":true}\n"
    } else {
        "{\"show_welcome\":false}\n"
    };

    let _ = std::fs::write(&path, content);
}

/// Reset the welcome screen to show on next startup
pub fn reset_welcome() {
    set_show_welcome(true);
}

/// Result of handling a key in the welcome view
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WelcomeAction {
    None,
    Continue,
    ContinueAndDisable,
}

/// Full-screen welcome view shown on application startup
pub struct WelcomeView {
    pub action: WelcomeAction,
}

impl WelcomeView {
    pub fn new() -> Self {
        Self {
            action: WelcomeAction::None,
        }
    }

    /// Handle key input
    pub fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('d') | KeyCode::Char('D') => {
                // Don't show again
                set_show_welcome(false);
                self.action = WelcomeAction::ContinueAndDisable;
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                // Reset (re-enable welcome on next startup)
                reset_welcome();
                // Stay on welcome screen, just reset the preference
            }
            _ => {
                // Any other key continues to startup
                self.action = WelcomeAction::Continue;
            }
        }
    }

    /// Check if user has chosen to continue
    pub fn should_continue(&self) -> bool {
        matches!(
            self.action,
            WelcomeAction::Continue | WelcomeAction::ContinueAndDisable
        )
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Create layout with padding
        let outer_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .padding(Padding::new(2, 2, 1, 1));

        let inner_area = outer_block.inner(area);
        frame.render_widget(outer_block, area);

        // Split into title area and content area
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5), // Title section
                Constraint::Min(0),    // Content
                Constraint::Length(4), // Footer with key hints
            ])
            .split(inner_area);

        // Draw title
        self.draw_title(frame, chunks[0], theme);

        // Draw main content
        self.draw_content(frame, chunks[1], theme);

        // Draw footer with key hints
        self.draw_footer(frame, chunks[2], theme);
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Welcome to Accountir",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Event-Sourced Double-Entry Accounting System",
                Style::default().fg(theme.header),
            )),
        ];

        let title = Paragraph::new(title_lines).alignment(Alignment::Center);
        frame.render_widget(title, area);
    }

    fn draw_content(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Create two columns
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Left column - About & Getting Started
        self.draw_left_column(frame, columns[0], theme);

        // Right column - Key Commands & Views
        self.draw_right_column(frame, columns[1], theme);
    }

    fn draw_left_column(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let content = vec![
            Line::from(Span::styled(
                "About",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  Accountir is a terminal-based accounting"),
            Line::from("  application using double-entry bookkeeping."),
            Line::from("  Every change is recorded as an immutable"),
            Line::from("  event, providing a complete audit trail."),
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                "Getting Started",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  1. Create or open a database"),
            Line::from("  2. Set up your chart of accounts"),
            Line::from("     (Assets, Liabilities, Equity, etc.)"),
            Line::from("  3. Record journal entries for transactions"),
            Line::from("  4. View financial reports"),
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                "Data Storage",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("  All data is stored in a local SQLite file."),
            Line::from("  You can create multiple databases for"),
            Line::from("  different entities or time periods."),
        ];

        let paragraph = Paragraph::new(content);
        frame.render_widget(paragraph, area);
    }

    fn draw_right_column(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let content = vec![
            Line::from(Span::styled(
                "Key Commands",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::key_line("?", "Open context-sensitive help", theme),
            Self::key_line("Tab / 1-5", "Navigate between views", theme),
            Self::key_line("Enter", "Select items or view details", theme),
            Self::key_line("a", "Add new account (Accounts view)", theme),
            Self::key_line("e", "Create journal entry (Journal view)", theme),
            Self::key_line("i", "Import transactions from CSV", theme),
            Self::key_line("Esc", "Close database / go back", theme),
            Self::key_line("q", "Quit application", theme),
            Line::from(""),
            Line::from(""),
            Line::from(Span::styled(
                "Views",
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Self::view_line("1", "Dashboard", "Financial summary", theme),
            Self::view_line("2", "Accounts", "Chart of accounts", theme),
            Self::view_line("3", "Journal", "Transactions & ledger", theme),
            Self::view_line("4", "Reports", "Financial statements", theme),
            Self::view_line("5", "Events", "Complete audit log", theme),
        ];

        let paragraph = Paragraph::new(content);
        frame.render_widget(paragraph, area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let footer_lines = vec![
            Line::from(Span::styled(
                "─".repeat(area.width.saturating_sub(4) as usize),
                Style::default().fg(theme.fg_dim),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("  Press "),
                Span::styled(
                    "any key",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" to continue    "),
                Span::styled("d", Style::default().fg(theme.header)),
                Span::raw(" = don't show again    "),
                Span::styled("r", Style::default().fg(Color::Magenta)),
                Span::raw(" = reset (show next time)"),
            ]),
        ];

        let footer = Paragraph::new(footer_lines);
        frame.render_widget(footer, area);
    }

    fn key_line(key: &'static str, description: &'static str, theme: &Theme) -> Line<'static> {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:<12}", key), Style::default().fg(theme.accent)),
            Span::raw(description),
        ])
    }

    fn view_line(
        key: &'static str,
        name: &'static str,
        description: &'static str,
        theme: &Theme,
    ) -> Line<'static> {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(key.to_string(), Style::default().fg(theme.accent)),
            Span::raw(" "),
            Span::styled(format!("{:<12}", name), Style::default().fg(theme.fg)),
            Span::styled(description, Style::default().fg(theme.fg_dim)),
        ])
    }
}

impl Default for WelcomeView {
    fn default() -> Self {
        Self::new()
    }
}
