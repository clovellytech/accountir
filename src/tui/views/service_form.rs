use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::tui::theme::Theme;
use crate::tui::widgets::{self, TextField};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceFormResult {
    None,
    Cancel,
    Saved {
        name: String,
        root_url: String,
        api_key: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Name,
    RootUrl,
    ApiKey,
}

impl FormField {
    fn next(&self) -> Self {
        match self {
            FormField::Name => FormField::RootUrl,
            FormField::RootUrl => FormField::ApiKey,
            FormField::ApiKey => FormField::Name,
        }
    }

    fn prev(&self) -> Self {
        match self {
            FormField::Name => FormField::ApiKey,
            FormField::RootUrl => FormField::Name,
            FormField::ApiKey => FormField::RootUrl,
        }
    }
}

pub struct ServiceFormModal {
    pub visible: bool,
    pub result: ServiceFormResult,
    active_field: FormField,
    name: String,
    root_url: String,
    api_key: String,
    error_message: Option<String>,
}

impl Default for ServiceFormModal {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceFormModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            result: ServiceFormResult::None,
            active_field: FormField::Name,
            name: String::new(),
            root_url: String::new(),
            api_key: String::new(),
            error_message: None,
        }
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.result = ServiceFormResult::None;
        self.active_field = FormField::Name;
        self.name.clear();
        self.root_url.clear();
        self.api_key.clear();
        self.error_message = None;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.result = ServiceFormResult::Cancel;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.active_field = self.active_field.next();
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.active_field = self.active_field.prev();
            }
            KeyCode::Enter => {
                self.submit();
            }
            KeyCode::Char(c) => {
                self.error_message = None;
                match self.active_field {
                    FormField::Name => self.name.push(c),
                    FormField::RootUrl => self.root_url.push(c),
                    FormField::ApiKey => self.api_key.push(c),
                }
            }
            KeyCode::Backspace => {
                self.error_message = None;
                match self.active_field {
                    FormField::Name => {
                        self.name.pop();
                    }
                    FormField::RootUrl => {
                        self.root_url.pop();
                    }
                    FormField::ApiKey => {
                        self.api_key.pop();
                    }
                }
            }
            _ => {}
        }
    }

    fn submit(&mut self) {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            self.error_message = Some("Name is required".to_string());
            self.active_field = FormField::Name;
            return;
        }

        let mut root_url = self.root_url.trim().to_string();
        if root_url.is_empty() {
            self.error_message = Some("Root URL is required".to_string());
            self.active_field = FormField::RootUrl;
            return;
        }
        if !root_url.starts_with("http://") && !root_url.starts_with("https://") {
            root_url = format!("https://{}", root_url);
        }
        root_url = root_url.trim_end_matches('/').to_string();

        let api_key = self.api_key.trim().to_string();
        if api_key.is_empty() {
            self.error_message = Some("API key is required".to_string());
            self.active_field = FormField::ApiKey;
            return;
        }

        self.result = ServiceFormResult::Saved {
            name,
            root_url,
            api_key,
        };
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let modal_area = widgets::centered_rect(50, 40, area);
        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Register Event Service ")
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(block, modal_area);

        let inner = widgets::inner_rect(modal_area, 2, 1);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Name
                Constraint::Length(3), // Root URL
                Constraint::Length(3), // API Key
                Constraint::Length(1), // spacer
                Constraint::Length(2), // Help
                Constraint::Min(0),
            ])
            .split(inner);

        widgets::draw_text_field(
            frame,
            &TextField::new(
                chunks[0],
                "Name",
                &self.name,
                self.active_field == FormField::Name,
            ),
            theme,
        );
        widgets::draw_text_field(
            frame,
            &TextField::new(
                chunks[1],
                "Root URL",
                &self.root_url,
                self.active_field == FormField::RootUrl,
            ),
            theme,
        );
        widgets::draw_text_field(
            frame,
            &TextField::new(
                chunks[2],
                "API Key",
                &self.api_key,
                self.active_field == FormField::ApiKey,
            )
            .secret(),
            theme,
        );

        let message = if let Some(ref err) = self.error_message {
            Line::from(Span::styled(err.clone(), Style::default().fg(theme.error)))
        } else {
            Line::from(vec![
                Span::styled("Tab", Style::default().fg(theme.header)),
                Span::raw(": next  "),
                Span::styled("Enter", Style::default().fg(theme.header)),
                Span::raw(": save  "),
                Span::styled("Esc", Style::default().fg(theme.header)),
                Span::raw(": cancel"),
            ])
        };
        frame.render_widget(Paragraph::new(message), chunks[4]);
    }
}
