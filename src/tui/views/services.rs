use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::commands::event_service_commands::ServiceDisplay;
use crate::tui::theme::Theme;

pub enum ServiceAction {
    None,
    Register,
    Sync(String),
    Review(String),
    Remove(String),
}

pub struct ServicesView {
    pub services: Vec<ServiceDisplay>,
    pub selected: usize,
    pub status_message: Option<String>,
}

impl Default for ServicesView {
    fn default() -> Self {
        Self::new()
    }
}

impl ServicesView {
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
            selected: 0,
            status_message: None,
        }
    }

    pub fn set_services(&mut self, services: Vec<ServiceDisplay>) {
        self.services = services;
        if self.selected >= self.services.len() && !self.services.is_empty() {
            self.selected = self.services.len() - 1;
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> ServiceAction {
        match key {
            KeyCode::Char('a') => ServiceAction::Register,
            KeyCode::Char('s') => {
                if let Some(svc) = self.services.get(self.selected) {
                    ServiceAction::Sync(svc.id.clone())
                } else {
                    ServiceAction::None
                }
            }
            KeyCode::Char('r') => {
                if let Some(svc) = self.services.get(self.selected) {
                    ServiceAction::Review(svc.id.clone())
                } else {
                    ServiceAction::None
                }
            }
            KeyCode::Char('d') => {
                if let Some(svc) = self.services.get(self.selected) {
                    ServiceAction::Remove(svc.id.clone())
                } else {
                    ServiceAction::None
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.services.is_empty() {
                    self.selected = (self.selected + 1).min(self.services.len() - 1);
                }
                ServiceAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                ServiceAction::None
            }
            _ => ServiceAction::None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Event Services (a: add, s: sync, r: review staged, d: remove) ")
            .title_style(Style::default().fg(theme.accent));

        if self.services.is_empty() {
            let msg = if let Some(ref status) = self.status_message {
                status.clone()
            } else {
                "No event services registered. Press 'a' to add one.".to_string()
            };
            let content = Paragraph::new(Line::from(Span::styled(
                msg,
                Style::default().fg(theme.fg_dim),
            )))
            .block(block);
            frame.render_widget(content, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("Name"),
            Cell::from("URL"),
            Cell::from("Status"),
            Cell::from("Last Synced"),
            Cell::from("Events"),
            Cell::from("Entries"),
        ])
        .style(
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        );

        let rows: Vec<Row> = self
            .services
            .iter()
            .enumerate()
            .map(|(i, svc)| {
                let style = if i == self.selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg)
                };

                let last_synced = svc
                    .last_synced_at
                    .as_deref()
                    .unwrap_or("never");

                Row::new(vec![
                    Cell::from(svc.name.clone()),
                    Cell::from(svc.root_url.clone()),
                    Cell::from(svc.status.clone()),
                    Cell::from(last_synced.to_string()),
                    Cell::from(svc.events_processed.to_string()),
                    Cell::from(svc.entries_created.to_string()),
                ])
                .style(style)
            })
            .collect();

        let widths = [
            Constraint::Length(20),
            Constraint::Min(30),
            Constraint::Length(10),
            Constraint::Length(20),
            Constraint::Length(8),
            Constraint::Length(8),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .block(block);

        frame.render_widget(table, area);

        // Draw status message below if present
        if let Some(ref msg) = self.status_message {
            let status_area = Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(1),
                width: area.width,
                height: 1,
            };
            let status = Paragraph::new(Line::from(Span::styled(
                msg.clone(),
                Style::default().fg(theme.accent),
            )));
            frame.render_widget(status, status_area);
        }
    }
}
