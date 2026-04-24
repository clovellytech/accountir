use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::tui::theme::Theme;

pub struct PlaidView {
    pub items: Vec<PlaidItemDisplay>,
    pub selected: usize,
    pub status_message: Option<String>,
    pub staged_count: usize,
    pub transfer_count: usize,
}

pub struct PlaidItemDisplay {
    pub id: String,
    pub institution_name: String,
    pub status: String,
    pub last_synced_at: Option<String>,
    pub accounts: Vec<PlaidAccountDisplay>,
}

pub struct PlaidAccountDisplay {
    pub plaid_account_id: String,
    pub name: String,
    pub account_type: String,
    pub mask: Option<String>,
    pub local_account_name: Option<String>,
    pub plaid_balance_cents: Option<i64>,
    pub ledger_balance_cents: Option<i64>,
    pub balance_updated_at: Option<String>,
}

pub enum PlaidAction {
    None,
    Configure,
    Connect,
    Sync(String),
    SyncAll,
    Disconnect(String),
    ReviewStaged,
}

impl Default for PlaidView {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaidView {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            selected: 0,
            status_message: None,
            staged_count: 0,
            transfer_count: 0,
        }
    }

    pub fn set_items(&mut self, items: Vec<PlaidItemDisplay>) {
        self.items = items;
        if self.selected >= self.items.len() && !self.items.is_empty() {
            self.selected = self.items.len() - 1;
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> PlaidAction {
        match key {
            KeyCode::Char('r') => PlaidAction::ReviewStaged,
            KeyCode::Char('C') => PlaidAction::Configure,
            KeyCode::Char('c') => PlaidAction::Connect,
            KeyCode::Char('s') => {
                if let Some(item) = self.items.get(self.selected) {
                    PlaidAction::Sync(item.id.clone())
                } else {
                    PlaidAction::None
                }
            }
            KeyCode::Char('S') => PlaidAction::SyncAll,
            KeyCode::Char('d') => {
                if let Some(item) = self.items.get(self.selected) {
                    PlaidAction::Disconnect(item.id.clone())
                } else {
                    PlaidAction::None
                }
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.items.is_empty() {
                    self.selected = (self.selected + 1).min(self.items.len() - 1);
                }
                PlaidAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                PlaidAction::None
            }
            _ => PlaidAction::None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " Plaid Bank Connections (C: config, c: connect, s: sync, S: sync all, r: review staged [{}], d: disconnect) ",
                self.staged_count
            ))
            .title_style(Style::default().fg(theme.accent));

        if self.items.is_empty() {
            let config = crate::config::AppConfig::load();
            let config_status = if config.plaid.is_configured() {
                Line::from(Span::styled(
                    "Plaid is configured.",
                    Style::default().fg(theme.success),
                ))
            } else {
                Line::from(vec![
                    Span::styled(
                        "Plaid is not configured. ",
                        Style::default().fg(theme.error),
                    ),
                    Span::raw("Press "),
                    Span::styled("C", Style::default().fg(theme.header)),
                    Span::raw(" to set up proxy URL and API key."),
                ])
            };

            let msg = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No banks connected via Plaid.",
                    Style::default().fg(theme.fg_dim),
                )),
                Line::from(""),
                config_status,
                Line::from(""),
                Line::from(vec![
                    Span::raw("Press "),
                    Span::styled("c", Style::default().fg(theme.header)),
                    Span::raw(" to connect a bank account."),
                ]),
            ];
            let paragraph = Paragraph::new(msg).block(block);
            frame.render_widget(paragraph, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("Institution"),
            Cell::from("Account"),
            Cell::from("Mapped To"),
            Cell::from("Balance"),
            Cell::from("Last Synced"),
        ])
        .style(
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        );

        let mut rows: Vec<Row> = Vec::new();

        for (i, item) in self.items.iter().enumerate() {
            let is_selected = i == self.selected;
            let style = if is_selected {
                theme.selected_style()
            } else {
                Style::default()
            };
            let status_color = match item.status.as_str() {
                "active" => theme.success,
                "disconnected" | "revoked" => theme.error,
                _ => theme.fg_dim,
            };

            if item.accounts.is_empty() {
                rows.push(
                    Row::new(vec![
                        Cell::from(item.institution_name.clone()),
                        Cell::from(Span::styled(
                            "No accounts",
                            Style::default().fg(theme.fg_dim),
                        )),
                        Cell::from(""),
                        Cell::from(Span::styled(
                            item.status.clone(),
                            Style::default().fg(status_color),
                        )),
                        Cell::from(
                            item.last_synced_at
                                .as_deref()
                                .unwrap_or("Never")
                                .to_string(),
                        ),
                    ])
                    .style(style),
                );
            } else {
                for (j, acct) in item.accounts.iter().enumerate() {
                    let inst_col = if j == 0 {
                        Cell::from(vec![Line::from(vec![
                            Span::raw(item.institution_name.clone()),
                            Span::raw(" "),
                            Span::styled(item.status.clone(), Style::default().fg(status_color)),
                        ])])
                    } else {
                        Cell::from("")
                    };

                    let mask = acct.mask.as_deref().unwrap_or("");
                    let acct_label = format!("  {}({})", acct.name, mask);

                    let mapped = acct.local_account_name.as_deref().unwrap_or("[unmapped]");
                    let mapped_style = if acct.local_account_name.is_some() {
                        Style::default()
                    } else {
                        Style::default().fg(theme.fg_dim)
                    };

                    let balance_cell = match (acct.plaid_balance_cents, acct.ledger_balance_cents) {
                        (Some(plaid), Some(ledger)) => {
                            let diff = plaid - ledger;
                            if diff == 0 {
                                Cell::from(Span::styled(
                                    "Balanced",
                                    Style::default().fg(theme.success),
                                ))
                            } else {
                                let abs = diff.unsigned_abs() as i64;
                                let sign = if diff > 0 { "+" } else { "-" };
                                Cell::from(Span::styled(
                                    format!("Off {}${}.{:02}", sign, abs / 100, abs % 100),
                                    Style::default().fg(theme.error),
                                ))
                            }
                        }
                        _ => Cell::from(""),
                    };

                    let synced = acct
                        .balance_updated_at
                        .as_deref()
                        .or(item.last_synced_at.as_deref())
                        .unwrap_or("Never");
                    // Show just the date portion if it's an RFC3339 timestamp
                    let synced_short = if synced.len() > 10 {
                        &synced[..10]
                    } else {
                        synced
                    };

                    rows.push(
                        Row::new(vec![
                            inst_col,
                            Cell::from(acct_label),
                            Cell::from(Span::styled(mapped.to_string(), mapped_style)),
                            balance_cell,
                            Cell::from(synced_short.to_string()),
                        ])
                        .style(style),
                    );
                }
            }
        }

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(22),
                Constraint::Percentage(22),
                Constraint::Percentage(20),
                Constraint::Percentage(18),
                Constraint::Percentage(18),
            ],
        )
        .header(header)
        .block(block);

        frame.render_widget(table, area);

        // Status message
        if let Some(msg) = &self.status_message {
            let status_area = Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(1),
                width: area.width,
                height: 1,
            };
            let status = Paragraph::new(Span::styled(
                msg.as_str(),
                Style::default().fg(theme.header),
            ));
            frame.render_widget(status, status_area);
        }
    }
}
