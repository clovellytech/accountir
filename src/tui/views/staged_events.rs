use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState},
    Frame,
};
use std::collections::HashMap;

use crate::commands::event_service_commands::{StagedEventDisplay, StagedEventReadiness};
use crate::domain::Account;
use crate::tui::theme::Theme;

pub enum ServiceStagedAction {
    None,
    ImportSingle(String),
    ImportAll,
    OpenMappingEditor(String),
    SaveMapping { key: String, account_id: String },
    Back,
}

pub struct MappingEditorState {
    pub visible: bool,
    pub missing_keys: Vec<String>,
    pub current_key_index: usize,
    pub available_accounts: Vec<Account>,
    pub filter: String,
    pub filtered_indices: Vec<usize>,
    pub selected_index: usize,
    pub pending_assignments: HashMap<String, String>,
}

impl MappingEditorState {
    pub fn new() -> Self {
        Self {
            visible: false,
            missing_keys: Vec::new(),
            current_key_index: 0,
            available_accounts: Vec::new(),
            filter: String::new(),
            filtered_indices: Vec::new(),
            selected_index: 0,
            pending_assignments: HashMap::new(),
        }
    }

    pub fn open(&mut self, missing_keys: Vec<String>, accounts: Vec<Account>) {
        self.missing_keys = missing_keys;
        self.current_key_index = 0;
        self.available_accounts = accounts;
        self.filter.clear();
        self.selected_index = 0;
        self.pending_assignments.clear();
        self.update_filtered();
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.missing_keys.clear();
        self.filter.clear();
        self.pending_assignments.clear();
    }

    pub fn current_key(&self) -> Option<&str> {
        self.missing_keys.get(self.current_key_index).map(|s| s.as_str())
    }

    pub fn advance_or_close(&mut self) {
        if self.current_key_index + 1 < self.missing_keys.len() {
            self.current_key_index += 1;
            self.filter.clear();
            self.selected_index = 0;
            self.update_filtered();
        } else {
            self.close();
        }
    }

    fn update_filtered(&mut self) {
        let filter_lower = self.filter.to_lowercase();
        self.filtered_indices = self
            .available_accounts
            .iter()
            .enumerate()
            .filter(|(_, a)| a.is_active)
            .filter(|(_, a)| {
                if filter_lower.is_empty() {
                    return true;
                }
                a.name.to_lowercase().contains(&filter_lower)
                    || a.account_number.to_lowercase().contains(&filter_lower)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected_index >= self.filtered_indices.len() {
            self.selected_index = 0;
        }
    }

    fn selected_account(&self) -> Option<&Account> {
        self.filtered_indices
            .get(self.selected_index)
            .and_then(|&i| self.available_accounts.get(i))
    }

    fn handle_key(&mut self, key: KeyCode) -> ServiceStagedAction {
        match key {
            KeyCode::Esc => {
                self.close();
                ServiceStagedAction::None
            }
            KeyCode::Enter => {
                if let (Some(key_name), Some(account)) =
                    (self.current_key().map(|s| s.to_string()), self.selected_account())
                {
                    let account_id = account.id.clone();
                    ServiceStagedAction::SaveMapping {
                        key: key_name,
                        account_id,
                    }
                } else {
                    ServiceStagedAction::None
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.filtered_indices.is_empty() {
                    self.selected_index =
                        (self.selected_index + 1).min(self.filtered_indices.len() - 1);
                }
                ServiceStagedAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_index > 0 {
                    self.selected_index -= 1;
                }
                ServiceStagedAction::None
            }
            KeyCode::Tab => {
                self.advance_or_close();
                ServiceStagedAction::None
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.update_filtered();
                ServiceStagedAction::None
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.update_filtered();
                ServiceStagedAction::None
            }
            _ => ServiceStagedAction::None,
        }
    }

    fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let Some(current_key) = self.current_key() else {
            return;
        };

        let popup_width = area.width.min(50).max(30);
        let popup_height = area.height.min(20).max(10);
        let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
        let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
        let modal_area = Rect::new(x, y, popup_width, popup_height);

        frame.render_widget(Clear, modal_area);

        let title = format!(
            " Assign Mapping: {} ({}/{}) ",
            current_key,
            self.current_key_index + 1,
            self.missing_keys.len()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(block, modal_area);

        let inner = Rect {
            x: modal_area.x + 1,
            y: modal_area.y + 1,
            width: modal_area.width.saturating_sub(2),
            height: modal_area.height.saturating_sub(2),
        };

        let chunks = Layout::vertical([
            Constraint::Length(1), // Filter input
            Constraint::Min(3),   // Account list
            Constraint::Length(1), // Help
        ])
        .split(inner);

        // Filter input
        let filter_display = if self.filter.is_empty() {
            "Type to filter accounts...".to_string()
        } else {
            self.filter.clone()
        };
        let filter_style = if self.filter.is_empty() {
            Style::default().fg(theme.fg_dim)
        } else {
            Style::default().fg(theme.fg)
        };
        let filter_line = Paragraph::new(Line::from(vec![
            Span::styled(" > ", Style::default().fg(theme.accent)),
            Span::styled(filter_display, filter_style),
        ]));
        frame.render_widget(filter_line, chunks[0]);

        // Account list
        let items: Vec<ListItem> = self
            .filtered_indices
            .iter()
            .enumerate()
            .map(|(i, &idx)| {
                let account = &self.available_accounts[idx];
                let style = if i == self.selected_index {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg)
                };
                let prefix = if i == self.selected_index { "> " } else { "  " };
                ListItem::new(Line::from(Span::styled(
                    format!("{}{} {}", prefix, account.account_number, account.name),
                    style,
                )))
            })
            .collect();

        let mut list_state = ListState::default();
        list_state.select(Some(self.selected_index));
        let list = List::new(items);
        frame.render_stateful_widget(list, chunks[1], &mut list_state);

        // Help
        let help = Paragraph::new(Line::from(Span::styled(
            " Enter: select  Tab: next key  Esc: cancel",
            Style::default().fg(theme.fg_dim),
        )));
        frame.render_widget(help, chunks[2]);
    }
}

pub struct StagedEventsView {
    pub events: Vec<StagedEventDisplay>,
    pub selected: usize,
    pub visible: bool,
    pub service_id: String,
    pub service_name: String,
    pub status_message: Option<String>,
    pub mapping_editor: MappingEditorState,
    scroll_offset: usize,
}

impl Default for StagedEventsView {
    fn default() -> Self {
        Self::new()
    }
}

impl StagedEventsView {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            selected: 0,
            visible: false,
            service_id: String::new(),
            service_name: String::new(),
            status_message: None,
            mapping_editor: MappingEditorState::new(),
            scroll_offset: 0,
        }
    }

    pub fn show(&mut self, service_id: String, service_name: String) {
        self.service_id = service_id;
        self.service_name = service_name;
        self.visible = true;
        self.selected = 0;
        self.scroll_offset = 0;
        self.status_message = None;
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.mapping_editor.close();
    }

    pub fn set_events(&mut self, events: Vec<StagedEventDisplay>) {
        self.events = events;
        if self.selected >= self.events.len() && !self.events.is_empty() {
            self.selected = self.events.len() - 1;
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> ServiceStagedAction {
        // If mapping editor is open, forward keys to it
        if self.mapping_editor.visible {
            return self.mapping_editor.handle_key(key);
        }

        match key {
            KeyCode::Esc | KeyCode::Char('q') => ServiceStagedAction::Back,
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.events.is_empty() {
                    self.selected = (self.selected + 1).min(self.events.len() - 1);
                }
                ServiceStagedAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                ServiceStagedAction::None
            }
            KeyCode::Enter => {
                if let Some(event) = self.events.get(self.selected) {
                    match &event.readiness {
                        StagedEventReadiness::Ready => {
                            ServiceStagedAction::ImportSingle(event.id.clone())
                        }
                        StagedEventReadiness::NeedsMapping(_) => {
                            ServiceStagedAction::OpenMappingEditor(event.id.clone())
                        }
                    }
                } else {
                    ServiceStagedAction::None
                }
            }
            KeyCode::Char('m') => {
                if let Some(event) = self.events.get(self.selected) {
                    ServiceStagedAction::OpenMappingEditor(event.id.clone())
                } else {
                    ServiceStagedAction::None
                }
            }
            KeyCode::Char('I') => ServiceStagedAction::ImportAll,
            _ => ServiceStagedAction::None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::vertical([
            Constraint::Length(3), // Summary
            Constraint::Min(5),   // Event table
            Constraint::Length(1), // Help / status
        ])
        .split(area);

        self.render_summary(frame, chunks[0], theme);
        self.render_events(frame, chunks[1], theme);
        self.render_help(frame, chunks[2], theme);

        // Mapping editor overlay
        self.mapping_editor.draw(frame, area, theme);
    }

    fn render_summary(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let total = self.events.len();
        let ready = self
            .events
            .iter()
            .filter(|e| matches!(e.readiness, StagedEventReadiness::Ready))
            .count();
        let needs_mapping = total - ready;

        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Staged Events: {} ", self.service_name))
            .title_style(Style::default().fg(theme.accent));

        let line = Line::from(vec![
            Span::styled(
                format!("{} staged", total),
                Style::default().fg(theme.fg),
            ),
            Span::raw("  |  "),
            Span::styled(
                format!("{} ready", ready),
                Style::default().fg(theme.success),
            ),
            Span::raw("  |  "),
            Span::styled(
                format!("{} need mappings", needs_mapping),
                if needs_mapping > 0 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(theme.fg_dim)
                },
            ),
        ]);

        let content = Paragraph::new(line).block(block);
        frame.render_widget(content, area);
    }

    fn render_events(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default().borders(Borders::ALL);

        if self.events.is_empty() {
            let content = Paragraph::new(Line::from(Span::styled(
                " No staged events.",
                Style::default().fg(theme.fg_dim),
            )))
            .block(block);
            frame.render_widget(content, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from(" "),
            Cell::from("Type"),
            Cell::from("Date"),
            Cell::from("Description"),
            Cell::from("Amount"),
            Cell::from("Info"),
        ])
        .style(
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        );

        let rows: Vec<Row> = self
            .events
            .iter()
            .enumerate()
            .map(|(i, event)| {
                let (icon, icon_style) = match &event.readiness {
                    StagedEventReadiness::Ready => {
                        if event.status == "error" {
                            ("!", Style::default().fg(theme.error))
                        } else {
                            ("*", Style::default().fg(theme.success))
                        }
                    }
                    StagedEventReadiness::NeedsMapping(_) => {
                        ("?", Style::default().fg(Color::Yellow))
                    }
                };

                let info = match &event.readiness {
                    StagedEventReadiness::Ready => {
                        if let Some(ref err) = event.error_message {
                            err.clone()
                        } else {
                            "Ready".to_string()
                        }
                    }
                    StagedEventReadiness::NeedsMapping(keys) => {
                        format!("Need: {}", keys.join(", "))
                    }
                };

                let amount_str = event
                    .amount_cents
                    .map(|a| format!("${:.2}", a as f64 / 100.0))
                    .unwrap_or_default();

                let date = event
                    .timestamp
                    .get(..10)
                    .unwrap_or(&event.timestamp)
                    .to_string();

                let row_style = if i == self.selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg)
                };

                Row::new(vec![
                    Cell::from(Span::styled(format!(" {} ", icon), icon_style)),
                    Cell::from(event.event_type.clone()),
                    Cell::from(date),
                    Cell::from(truncate(&event.description, 30)),
                    Cell::from(amount_str),
                    Cell::from(truncate(&info, 30)),
                ])
                .style(row_style)
            })
            .collect();

        let widths = [
            Constraint::Length(3),
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Min(15),
            Constraint::Length(12),
            Constraint::Min(15),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .row_highlight_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let mut table_state = TableState::default();
        table_state.select(Some(self.selected));
        frame.render_stateful_widget(table, area, &mut table_state);
    }

    fn render_help(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let text = if let Some(ref msg) = self.status_message {
            Span::styled(msg.as_str(), Style::default().fg(theme.accent))
        } else {
            Span::styled(
                " j/k: navigate  Enter: import/map  m: edit mappings  I: import all ready  Esc: back",
                Style::default().fg(theme.fg_dim),
            )
        };
        let help = Paragraph::new(Line::from(text));
        frame.render_widget(help, area);
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max.saturating_sub(3)])
    }
}
