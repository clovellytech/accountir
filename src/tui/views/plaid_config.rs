use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::config::AppConfig;
use crate::tui::theme::Theme;
use crate::tui::widgets::{self, TextField};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaidConfigResult {
    None,
    Cancel,
    Saved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Register,
    Login,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigField {
    ProxyUrl,
    Email,
    Password,
}

impl ConfigField {
    fn next(&self) -> Self {
        match self {
            ConfigField::ProxyUrl => ConfigField::Email,
            ConfigField::Email => ConfigField::Password,
            ConfigField::Password => ConfigField::ProxyUrl,
        }
    }

    fn prev(&self) -> Self {
        match self {
            ConfigField::ProxyUrl => ConfigField::Password,
            ConfigField::Email => ConfigField::ProxyUrl,
            ConfigField::Password => ConfigField::Email,
        }
    }
}

pub struct PlaidConfigModal {
    pub visible: bool,
    pub result: PlaidConfigResult,

    active_field: ConfigField,
    proxy_url: String,
    email: String,
    password: String,
    already_registered: bool,
    auth_mode: AuthMode,
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
            password: String::new(),
            already_registered: false,
            auth_mode: AuthMode::Register,
            error_message: None,
        }
    }

    pub fn show(&mut self) {
        let config = AppConfig::load();
        self.proxy_url = config.plaid.proxy_url.unwrap_or_default();
        self.already_registered = config.plaid.api_key.is_some();
        self.email.clear();
        self.password.clear();
        self.visible = true;
        self.result = PlaidConfigResult::None;
        self.active_field = ConfigField::ProxyUrl;
        self.auth_mode = AuthMode::Register;
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
            KeyCode::Tab | KeyCode::Down => {
                if !self.already_registered {
                    self.active_field = self.active_field.next();
                }
            }
            KeyCode::BackTab | KeyCode::Up => {
                if !self.already_registered {
                    self.active_field = self.active_field.prev();
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
                    ConfigField::Password => self.password.push(c),
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
                    ConfigField::Password => {
                        self.password.pop();
                    }
                }
            }
            _ => {}
        }
    }

    fn normalize_url(&self) -> String {
        let mut url = self.proxy_url.trim().to_string();
        if !url.is_empty() && !url.starts_with("http://") && !url.starts_with("https://") {
            url = format!("https://{}", url);
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

        let password = self.password.clone();
        if password.len() < 8 {
            self.error_message = Some("Password must be at least 8 characters".to_string());
            self.active_field = ConfigField::Password;
            return;
        }

        let auth_mode = self.auth_mode;
        let (endpoint, body) = match auth_mode {
            AuthMode::Register => (
                format!("{}/auth/register", proxy_url),
                serde_json::json!({ "email": email, "password": password }),
            ),
            AuthMode::Login => (
                format!("{}/auth/login", proxy_url),
                serde_json::json!({ "email": email, "password": password }),
            ),
        };

        let api_key: Result<String, (String, bool)> = std::thread::spawn(move || {
            let client = reqwest::blocking::Client::new();
            let resp = client
                .post(&endpoint)
                .json(&body)
                .send()
                .map_err(|e| (format!("Connection failed: {}", e), false))?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().unwrap_or_default();
                let msg = serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(|s| s.to_string()))
                    .unwrap_or(text);

                let is_already_registered = auth_mode == AuthMode::Register
                    && msg.to_lowercase().contains("already registered");

                return Err((format!("{} ({})", msg, status), is_already_registered));
            }

            let body: serde_json::Value = resp
                .json()
                .map_err(|e| (format!("Invalid response: {}", e), false))?;

            body["api_key"]
                .as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| ("No API key in response".to_string(), false))
        })
        .join()
        .unwrap_or_else(|_| Err(("Request thread panicked".to_string(), false)));

        let api_key = match api_key {
            Ok(k) => k,
            Err((msg, is_already_registered)) => {
                if is_already_registered {
                    self.auth_mode = AuthMode::Login;
                    self.error_message = Some(
                        "Email already registered — enter your password to log in".to_string(),
                    );
                } else {
                    self.error_message = Some(msg);
                }
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

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let modal_area = widgets::centered_rect(50, 50, area);
        frame.render_widget(Clear, modal_area);

        let title = match self.auth_mode {
            AuthMode::Register => " Plaid Configuration ",
            AuthMode::Login => " Plaid Login ",
        };

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

        let inner = widgets::inner_rect(modal_area, 2, 1);

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

            widgets::draw_text_field(
                frame,
                &TextField::new(chunks[0], "Proxy URL", &self.proxy_url, true),
                theme,
            );

            let status = Paragraph::new(Line::from(Span::styled(
                "  Registered (API key saved)",
                Style::default().fg(theme.success),
            )));
            frame.render_widget(status, chunks[1]);

            self.draw_help(frame, chunks[3], theme);
        } else {
            // Auth mode: proxy URL + email + password
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Proxy URL
                    Constraint::Length(3), // Email
                    Constraint::Length(3), // Password
                    Constraint::Length(1), // spacer
                    Constraint::Length(2), // Help
                    Constraint::Min(0),
                ])
                .split(inner);

            let mode_label = match self.auth_mode {
                AuthMode::Register => "Email",
                AuthMode::Login => "Email",
            };

            widgets::draw_text_field(
                frame,
                &TextField::new(
                    chunks[0],
                    "Proxy URL",
                    &self.proxy_url,
                    self.active_field == ConfigField::ProxyUrl,
                ),
                theme,
            );
            widgets::draw_text_field(
                frame,
                &TextField::new(
                    chunks[1],
                    mode_label,
                    &self.email,
                    self.active_field == ConfigField::Email,
                ),
                theme,
            );
            widgets::draw_text_field(
                frame,
                &TextField::new(
                    chunks[2],
                    "Password",
                    &self.password,
                    self.active_field == ConfigField::Password,
                )
                .secret(),
                theme,
            );

            self.draw_help(frame, chunks[4], theme);
        }
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
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
        frame.render_widget(Paragraph::new(message), area);
    }
}

impl Default for PlaidConfigModal {
    fn default() -> Self {
        Self::new()
    }
}
