use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::tui::theme::Theme;

/// A Plaid account available for linking
#[derive(Debug, Clone)]
pub struct PlaidAccountOption {
    pub item_id: String,
    pub plaid_account_id: String,
    pub institution_name: String,
    pub account_name: String,
    pub mask: Option<String>,
    pub account_type: String,
    /// If already mapped, the local account ID it's mapped to
    pub mapped_to_local_id: Option<String>,
    /// Display name of the local account it's mapped to (for showing "(mapped to X)")
    pub mapped_to_local_name: Option<String>,
}

/// Result from the Plaid link modal
#[derive(Debug, Clone)]
pub enum PlaidLinkResult {
    None,
    Cancel,
    Link {
        item_id: String,
        plaid_account_id: String,
        local_account_id: String,
    },
    Unlink {
        item_id: String,
        plaid_account_id: String,
        local_account_id: String,
    },
}

/// Info about the current mapping for the selected account
#[derive(Debug, Clone)]
pub struct CurrentMapping {
    pub item_id: String,
    pub plaid_account_id: String,
    pub local_account_id: String,
    pub institution_name: String,
    pub plaid_account_name: String,
    pub mask: Option<String>,
}

pub struct PlaidLinkModal {
    pub visible: bool,
    pub result: PlaidLinkResult,

    /// The local account we're managing
    local_account_id: String,
    local_account_name: String,

    /// Current Plaid mapping for this account (if any)
    current_mapping: Option<CurrentMapping>,

    /// Available Plaid accounts to link to
    available_accounts: Vec<PlaidAccountOption>,

    /// Selected index in the available accounts list
    selected: usize,
}

impl PlaidLinkModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            result: PlaidLinkResult::None,
            local_account_id: String::new(),
            local_account_name: String::new(),
            current_mapping: None,
            available_accounts: Vec::new(),
            selected: 0,
        }
    }

    pub fn show(
        &mut self,
        local_account_id: String,
        local_account_name: String,
        current_mapping: Option<CurrentMapping>,
        available_accounts: Vec<PlaidAccountOption>,
    ) {
        self.visible = true;
        self.result = PlaidLinkResult::None;
        self.local_account_id = local_account_id;
        self.local_account_name = local_account_name;
        self.current_mapping = current_mapping;
        self.available_accounts = available_accounts;
        self.selected = 0;
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.local_account_id.clear();
        self.local_account_name.clear();
        self.current_mapping = None;
        self.available_accounts.clear();
        self.selected = 0;
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.result = PlaidLinkResult::Cancel;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.available_accounts.is_empty() {
                    self.selected = (self.selected + 1) % self.available_accounts.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if !self.available_accounts.is_empty() {
                    self.selected = self
                        .selected
                        .checked_sub(1)
                        .unwrap_or(self.available_accounts.len() - 1);
                }
            }
            KeyCode::Enter => {
                // Link selected Plaid account
                if let Some(acct) = self.available_accounts.get(self.selected) {
                    // Only link if not already mapped to another account
                    if acct.mapped_to_local_id.is_none()
                        || acct.mapped_to_local_id.as_deref() == Some(&self.local_account_id)
                    {
                        self.result = PlaidLinkResult::Link {
                            item_id: acct.item_id.clone(),
                            plaid_account_id: acct.plaid_account_id.clone(),
                            local_account_id: self.local_account_id.clone(),
                        };
                    }
                }
            }
            KeyCode::Char('u') => {
                // Unlink current mapping
                if let Some(ref mapping) = self.current_mapping {
                    self.result = PlaidLinkResult::Unlink {
                        item_id: mapping.item_id.clone(),
                        plaid_account_id: mapping.plaid_account_id.clone(),
                        local_account_id: mapping.local_account_id.clone(),
                    };
                }
            }
            _ => {}
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let dialog_width = 60u16.min(area.width.saturating_sub(4));
        let dialog_height = 20u16.min(area.height.saturating_sub(4));
        let dialog_x = (area.width.saturating_sub(dialog_width)) / 2;
        let dialog_y = (area.height.saturating_sub(dialog_height)) / 2;

        let dialog_area = Rect {
            x: dialog_x,
            y: dialog_y,
            width: dialog_width,
            height: dialog_height,
        };

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(format!(" Plaid Link: {} ", self.local_account_name))
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let mut lines: Vec<Line> = Vec::new();

        // Current mapping status
        if let Some(ref mapping) = self.current_mapping {
            let mask_str = mapping
                .mask
                .as_deref()
                .map(|m| format!(" (***{})", m))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled("Linked: ", Style::default().fg(theme.success)),
                Span::raw(format!(
                    "{} - {}{}",
                    mapping.institution_name, mapping.plaid_account_name, mask_str
                )),
            ]));
            lines.push(Line::from(vec![
                Span::styled(
                    "  u: unlink",
                    Style::default()
                        .fg(theme.header)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  Esc: cancel"),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                "Not linked to any Plaid account",
                Style::default().fg(theme.fg_dim),
            )));
        }

        lines.push(Line::from(""));

        // Available accounts
        if self.available_accounts.is_empty() {
            lines.push(Line::from(Span::styled(
                "No Plaid items connected.",
                Style::default().fg(theme.fg_dim),
            )));
            lines.push(Line::from(vec![
                Span::styled("Press ", Style::default().fg(theme.fg_dim)),
                Span::styled("6", Style::default().fg(theme.header)),
                Span::styled(
                    " to open the Plaid view and connect a bank.",
                    Style::default().fg(theme.fg_dim),
                ),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                "Available Plaid accounts:",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));

            for (i, acct) in self.available_accounts.iter().enumerate() {
                let is_selected = i == self.selected;
                let mask_str = acct
                    .mask
                    .as_deref()
                    .map(|m| format!(" ***{}", m))
                    .unwrap_or_default();

                let is_mapped_here =
                    acct.mapped_to_local_id.as_deref() == Some(&self.local_account_id);
                let is_mapped_elsewhere = acct.mapped_to_local_id.is_some() && !is_mapped_here;

                let style = if is_selected {
                    theme.selected_style()
                } else if is_mapped_elsewhere {
                    Style::default().fg(theme.fg_disabled)
                } else {
                    Style::default()
                };

                let prefix = if is_selected { "> " } else { "  " };

                let mut spans = vec![
                    Span::styled(prefix, style),
                    Span::styled(
                        format!(
                            "{} - {}{}",
                            acct.institution_name, acct.account_name, mask_str
                        ),
                        style,
                    ),
                ];

                if is_mapped_here {
                    spans.push(Span::styled(" (current)", style.fg(theme.success)));
                } else if is_mapped_elsewhere {
                    let mapped_name = acct.mapped_to_local_name.as_deref().unwrap_or("?");
                    spans.push(Span::styled(
                        format!(" (mapped to {})", mapped_name),
                        style.fg(theme.fg_disabled),
                    ));
                }

                lines.push(Line::from(spans));
            }

            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("Enter", Style::default().fg(theme.header)),
                Span::raw(": link  "),
                Span::styled("j/k", Style::default().fg(theme.header)),
                Span::raw(": navigate  "),
                Span::styled("Esc", Style::default().fg(theme.header)),
                Span::raw(": cancel"),
            ]));
        }

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);
    }
}

impl Default for PlaidLinkModal {
    fn default() -> Self {
        Self::new()
    }
}
