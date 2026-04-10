use std::path::PathBuf;

use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::tui::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupAction {
    None,
    NewDatabase(PathBuf),
    OpenDatabase(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Menu,
    NewPath,
    OpenPath,
}

pub struct StartupView {
    pub action: StartupAction,
    menu_state: ListState,
    input_mode: InputMode,
    input_buffer: String,
    recent_databases: Vec<PathBuf>,
    error_message: Option<String>,
}

impl StartupView {
    pub fn new() -> Self {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));

        // Try to find recent databases in current directory
        let recent = Self::find_recent_databases();

        Self {
            action: StartupAction::None,
            menu_state,
            input_mode: InputMode::Menu,
            input_buffer: String::new(),
            recent_databases: recent,
            error_message: None,
        }
    }

    fn find_recent_databases() -> Vec<PathBuf> {
        let mut databases = Vec::new();

        // Check for common database names in current directory
        let common_names = ["accountir.db", "accounts.db", "ledger.db"];
        for name in common_names {
            let path = PathBuf::from(name);
            if path.exists() {
                databases.push(path);
            }
        }

        // Also look for any .db files
        if let Ok(entries) = std::fs::read_dir(".") {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.extension().map(|e| e == "db").unwrap_or(false)
                    && !databases.contains(&path)
                {
                    databases.push(path);
                }
            }
        }

        databases
    }

    pub fn has_action(&self) -> bool {
        !matches!(self.action, StartupAction::None)
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        match self.input_mode {
            InputMode::Menu => self.handle_menu_key(key),
            InputMode::NewPath | InputMode::OpenPath => self.handle_input_key(key),
        }
    }

    fn handle_menu_key(&mut self, key: KeyCode) {
        let menu_items = self.get_menu_items();
        let max_index = menu_items.len().saturating_sub(1);

        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.menu_state.selected().unwrap_or(0);
                let new_i = if i == 0 { max_index } else { i - 1 };
                self.menu_state.select(Some(new_i));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.menu_state.selected().unwrap_or(0);
                let new_i = if i >= max_index { 0 } else { i + 1 };
                self.menu_state.select(Some(new_i));
            }
            KeyCode::Enter => {
                self.select_menu_item();
            }
            _ => {}
        }
    }

    fn handle_input_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.input_mode = InputMode::Menu;
                self.input_buffer.clear();
                self.error_message = None;
            }
            KeyCode::Enter => {
                self.submit_input();
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                self.input_buffer.push(c);
            }
            _ => {}
        }
    }

    fn get_menu_items(&self) -> Vec<String> {
        let mut items = vec![
            "Create New Database".to_string(),
            "Open Database...".to_string(),
        ];

        if !self.recent_databases.is_empty() {
            items.push("─── Recent Databases ───".to_string());
            for db in &self.recent_databases {
                items.push(format!("  {}", db.display()));
            }
        }

        items
    }

    fn select_menu_item(&mut self) {
        let selected = self.menu_state.selected().unwrap_or(0);

        match selected {
            0 => {
                // Create New Database
                self.input_mode = InputMode::NewPath;
                self.input_buffer = "accountir.db".to_string();
                self.error_message = None;
            }
            1 => {
                // Open Database
                self.input_mode = InputMode::OpenPath;
                self.input_buffer.clear();
                self.error_message = None;
            }
            n if n >= 3 => {
                // Recent database selected
                let db_index = n - 3;
                if let Some(path) = self.recent_databases.get(db_index) {
                    self.action = StartupAction::OpenDatabase(path.clone());
                }
            }
            _ => {}
        }
    }

    fn submit_input(&mut self) {
        if self.input_buffer.is_empty() {
            self.error_message = Some("Please enter a file path".to_string());
            return;
        }

        let path = PathBuf::from(&self.input_buffer);

        match self.input_mode {
            InputMode::NewPath => {
                if path.exists() {
                    self.error_message =
                        Some("File already exists. Use 'Open' instead.".to_string());
                } else {
                    self.action = StartupAction::NewDatabase(path);
                }
            }
            InputMode::OpenPath => {
                if !path.exists() {
                    self.error_message = Some("File does not exist.".to_string());
                } else {
                    self.action = StartupAction::OpenDatabase(path);
                }
            }
            InputMode::Menu => {}
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8), // Title/logo
                Constraint::Min(10),   // Menu
                Constraint::Length(3), // Input or help
            ])
            .margin(2)
            .split(area);

        self.draw_title(frame, chunks[0], theme);
        self.draw_menu(frame, chunks[1], theme);
        self.draw_input_or_help(frame, chunks[2], theme);
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title = vec![
            Line::from(""),
            Line::from(Span::styled(
                "  ╔═══════════════════════════════════════╗",
                Style::default().fg(theme.accent),
            )),
            Line::from(Span::styled(
                "  ║          A C C O U N T I R            ║",
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "  ║   Event-Sourced Accounting System     ║",
                Style::default().fg(theme.accent),
            )),
            Line::from(Span::styled(
                "  ╚═══════════════════════════════════════╝",
                Style::default().fg(theme.accent),
            )),
        ];

        let paragraph = Paragraph::new(title);
        frame.render_widget(paragraph, area);
    }

    fn draw_menu(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let items: Vec<ListItem> = self
            .get_menu_items()
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let style = if item.starts_with("───") {
                    Style::default().fg(theme.fg_dim)
                } else if i == 0 || i == 1 {
                    Style::default().fg(theme.fg)
                } else {
                    Style::default().fg(theme.header)
                };
                ListItem::new(Line::from(Span::styled(item.clone(), style)))
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Select an Option "),
            )
            .highlight_style(theme.selected_style())
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, area, &mut self.menu_state.clone());
    }

    fn draw_input_or_help(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        match self.input_mode {
            InputMode::Menu => {
                let help = Paragraph::new(Line::from(vec![
                    Span::styled("↑/↓", Style::default().fg(theme.header)),
                    Span::raw(" navigate  "),
                    Span::styled("Enter", Style::default().fg(theme.header)),
                    Span::raw(" select  "),
                    Span::styled("q", Style::default().fg(theme.header)),
                    Span::raw(" quit"),
                ]))
                .block(Block::default().borders(Borders::ALL));
                frame.render_widget(help, area);
            }
            InputMode::NewPath | InputMode::OpenPath => {
                let title = if self.input_mode == InputMode::NewPath {
                    " New Database Path "
                } else {
                    " Open Database Path "
                };

                let input_style = if self.error_message.is_some() {
                    Style::default().fg(theme.error)
                } else {
                    Style::default().fg(theme.input_active_fg)
                };

                let display_text = if let Some(ref err) = self.error_message {
                    format!("{} ({})", self.input_buffer, err)
                } else {
                    format!("{}█", self.input_buffer)
                };

                let input = Paragraph::new(Line::from(Span::styled(display_text, input_style)))
                    .block(Block::default().borders(Borders::ALL).title(title));
                frame.render_widget(input, area);
            }
        }
    }
}

impl Default for StartupView {
    fn default() -> Self {
        Self::new()
    }
}
