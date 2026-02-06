use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::config::AppConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaidConfigResult {
    None,
    Cancel,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigField {
    ProxyUrl,
    Email,
}

impl ConfigField {
    fn next(&self) -> Self {
        match self {
            ConfigField::ProxyUrl => ConfigField::Email,
            ConfigField::Email => ConfigField::ProxyUrl,
        }
    }
}

pub struct PlaidConfigModal {
    pub visible: bool,
    pub result: PlaidConfigResult,

    active_field: ConfigField,
    proxy_url: String,
    email: String,
    already_registered: bool,
    error_message: Option<String>,
}

impl PlaidConfigModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            result: PlaidConfigResult::None,
            active_field: ConfigField::ProxyUrl,
            proxy_url: String::new(),
            email: String::new(),
            already_registered: false,
            error_message: None,
        }
    }

    pub fn show(&mut self) {
        let config = AppConfig::load();
        self.proxy_url = config.plaid.proxy_url.unwrap_or_default();
        self.already_registered = config.plaid.api_key.is_some();
        self.email.clear();
        self.visible = true;
        self.result = PlaidConfigResult::None;
        self.active_field = ConfigField::ProxyUrl;
        self.error_message = None;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.result = PlaidConfigResult::Cancel;
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::BackTab | KeyCode::Up => {
                if !self.already_registered {
                    self.active_field = self.active_field.next();
                }
            }
            KeyCode::Enter => {
                self.submit();
            }
            KeyCode::Char(c) => {
                self.error_message = None;
                match self.active_field {
                    ConfigField::ProxyUrl => self.proxy_url.push(c),
                    ConfigField::Email => self.email.push(c),
                }
            }
            KeyCode::Backspace => {
                self.error_message = None;
                match self.active_field {
                    ConfigField::ProxyUrl => {
                        self.proxy_url.pop();
                    }
                    ConfigField::Email => {
                        self.email.pop();
                    }
                }
            }
            _ => {}
        }
    }

    fn normalize_url(&self) -> String {
        let mut url = self.proxy_url.trim().to_string();
        if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
            url = format!("http://{}", url);
        }
        url.trim_end_matches('/').to_string()
    }

    fn submit(&mut self) {
        let proxy_url = self.normalize_url();

        if proxy_url.is_empty() {
            self.error_message = Some("Proxy URL is required".to_string());
            self.active_field = ConfigField::ProxyUrl;
            return;
        }

        if self.already_registered {
            // Just update the proxy URL
            let mut config = AppConfig::load();
            config.plaid.proxy_url = Some(proxy_url.clone());
            self.proxy_url = proxy_url;
            match config.save() {
                Ok(_) => self.result = PlaidConfigResult::Saved,
                Err(e) => self.error_message = Some(format!("Failed to save: {}", e)),
            }
            return;
        }

        let email = self.email.trim().to_string();
        if email.is_empty() || !email.contains('@') {
            self.error_message = Some("Valid email is required".to_string());
            self.active_field = ConfigField::Email;
            return;
        }

        // Register with the proxy to get an API key.
        // All reqwest work (including reading/dropping the response) must happen
        // on a separate thread to avoid "cannot drop a runtime" inside tokio.
        let register_url = format!("{}/auth/register", proxy_url);
        let api_key: Result<String, String> = std::thread::spawn(move || {
            let client = reqwest::blocking::Client::new();
            let resp = client
                .post(&register_url)
                .json(&serde_json::json!({ "email": email }))
                .send()
                .map_err(|e| format!("Connection failed: {}", e))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(|s| s.to_string()))
                    .unwrap_or(text);
                return Err(format!("Registration failed ({}): {}", status, msg));
            }

            let body: serde_json::Value = resp
                .json()
                .map_err(|e| format!("Invalid response: {}", e))?;

            body["api_key"]
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| "No API key in response".to_string())
        })
        .join()
        .unwrap_or_else(|_| Err("Registration thread panicked".to_string()));

        let api_key = match api_key {
            Ok(k) => k,
            Err(e) => {
                self.error_message = Some(e);
                return;
            }
        };

        let mut config = AppConfig::load();
        config.plaid.proxy_url = Some(proxy_url.clone());
        config.plaid.api_key = Some(api_key);
        self.proxy_url = proxy_url;
        match config.save() {
            Ok(_) => self.result = PlaidConfigResult::Saved,
            Err(e) => self.error_message = Some(format!("Failed to save: {}", e)),
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        if !self.visible {
            return;
        }

        let modal_area = centered_rect(50, 40, area);
        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Plaid Configuration ")
            .title_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(block, modal_area);

        let inner = inner_rect(modal_area, 2, 1);

        if self.already_registered {
            // Simple mode: just proxy URL
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Proxy URL
                    Constraint::Length(1), // Status
                    Constraint::Length(1), // spacer
                    Constraint::Length(2), // Help
                    Constraint::Min(0),
                ])
                .split(inner);

            self.draw_text_field(frame, chunks[0], "Proxy URL", &self.proxy_url, true);

            let status = Paragraph::new(Line::from(Span::styled(
                "  Registered (API key saved)",
                Style::default().fg(Color::Green),
            )));
            frame.render_widget(status, chunks[1]);

            self.draw_help(frame, chunks[3]);
        } else {
            // Registration mode: proxy URL + email
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Proxy URL
                    Constraint::Length(3), // Email
                    Constraint::Length(1), // spacer
                    Constraint::Length(2), // Help
                    Constraint::Min(0),
                ])
                .split(inner);

            self.draw_text_field(
                frame,
                chunks[0],
                "Proxy URL",
                &self.proxy_url,
                self.active_field == ConfigField::ProxyUrl,
            );
            self.draw_text_field(
                frame,
                chunks[1],
                "Email (for registration)",
                &self.email,
                self.active_field == ConfigField::Email,
            );

            self.draw_help(frame, chunks[3]);
        }
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let message = if let Some(ref err) = self.error_message {
            Line::from(Span::styled(err.clone(), Style::default().fg(Color::Red)))
        } else {
            Line::from(vec![
                Span::styled("Tab", Style::default().fg(Color::Yellow)),
                Span::raw(": next  "),
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(": save  "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(": cancel"),
            ])
        };
        frame.render_widget(Paragraph::new(message), area);
    }

    fn draw_text_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        label: &str,
        value: &str,
        is_active: bool,
    ) {
        let style = if is_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };

        let border_style = if is_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let display = if is_active {
            format!("{}█", value)
        } else {
            value.to_string()
        };

        let paragraph = Paragraph::new(display).style(style).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(" {} ", label)),
        );

        frame.render_widget(paragraph, area);
    }
}

impl Default for PlaidConfigModal {
    fn default() -> Self {
        Self::new()
    }
}

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

fn inner_rect(area: Rect, margin_x: u16, margin_y: u16) -> Rect {
    Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: area.width.saturating_sub(margin_x * 2),
        height: area.height.saturating_sub(margin_y * 2),
    }
}
