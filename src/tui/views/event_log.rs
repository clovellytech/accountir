use crossterm::event::KeyCode;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::events::types::StoredEvent;

pub struct EventLogView {
    pub events: Vec<StoredEvent>,
    pub scroll_offset: usize,
    pub selected: Option<usize>,
}

impl EventLogView {
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            scroll_offset: 0,
            selected: Some(0),
        }
    }

    pub fn set_events(&mut self, events: Vec<StoredEvent>) {
        self.events = events;
        // Start at the most recent event (end of list)
        if !self.events.is_empty() {
            self.selected = Some(self.events.len() - 1);
        } else {
            self.selected = None;
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                true
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                true
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                true
            }
            KeyCode::Home => {
                self.selected = if self.events.is_empty() {
                    None
                } else {
                    Some(0)
                };
                true
            }
            KeyCode::End => {
                self.selected = if self.events.is_empty() {
                    None
                } else {
                    Some(self.events.len() - 1)
                };
                true
            }
            _ => false,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.events.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let new_idx = (current + delta).clamp(0, self.events.len() as isize - 1) as usize;
        self.selected = Some(new_idx);
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" Event Log ")
            .title_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.events.is_empty() {
            let empty = Paragraph::new("No events recorded yet")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(empty, inner);
            return;
        }

        // Calculate visible area
        let visible_height = inner.height as usize;

        // Calculate scroll offset to keep selection visible
        let scroll_offset = if let Some(selected) = self.selected {
            if selected < self.scroll_offset {
                selected
            } else if selected >= self.scroll_offset + visible_height {
                selected.saturating_sub(visible_height - 1)
            } else {
                self.scroll_offset
            }
        } else {
            self.scroll_offset
        };

        // Build lines for visible events
        let lines: Vec<Line> = self
            .events
            .iter()
            .enumerate()
            .skip(scroll_offset)
            .take(visible_height)
            .map(|(idx, event)| {
                let is_selected = self.selected == Some(idx);
                let style = if is_selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default()
                };

                // Format timestamp
                let timestamp = event.timestamp.format("%Y-%m-%d %H:%M:%S");

                // Format event type with color
                let event_type = event.event.event_type();
                let type_color = match event_type {
                    t if t.starts_with("journal") => Color::Green,
                    t if t.starts_with("account") => Color::Yellow,
                    t if t.starts_with("company") => Color::Magenta,
                    t if t.starts_with("user") => Color::Blue,
                    t if t.starts_with("reconciliation") || t.starts_with("transaction") => {
                        Color::Cyan
                    }
                    t if t.starts_with("fiscal")
                        || t.starts_with("period")
                        || t.starts_with("year") =>
                    {
                        Color::Red
                    }
                    _ => Color::White,
                };

                // Format entity ID if present
                let entity = event.event.entity_id().unwrap_or("-");
                let entity_display = if entity.len() > 12 {
                    format!("{}...", &entity[..12])
                } else {
                    entity.to_string()
                };

                // Format summary based on event type
                let summary = format_event_summary(&event.event);

                Line::from(vec![
                    Span::styled(format!("{:>5} ", event.id), style.fg(Color::DarkGray)),
                    Span::styled(format!("{} ", timestamp), style),
                    Span::styled(format!("{:<28} ", event_type), style.fg(type_color)),
                    Span::styled(format!("{:<15} ", entity_display), style.fg(Color::Cyan)),
                    Span::styled(summary, style),
                ])
            })
            .collect();

        let paragraph = Paragraph::new(lines);
        frame.render_widget(paragraph, inner);

        // Render scrollbar if needed
        if self.events.len() > visible_height {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓"));
            let mut scrollbar_state =
                ScrollbarState::new(self.events.len()).position(scroll_offset);
            frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
        }
    }

    pub fn title(&self) -> String {
        format!(" Event Log ({} events) ", self.events.len())
    }
}

impl Default for EventLogView {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a human-readable summary of an event
fn format_event_summary(event: &crate::events::types::Event) -> String {
    use crate::events::types::Event;

    match event {
        Event::CompanyCreated {
            name,
            base_currency,
            ..
        } => {
            format!("Created company '{}' ({})", name, base_currency)
        }
        Event::CompanySettingsUpdated {
            field, new_value, ..
        } => {
            format!("Updated {} to '{}'", field, new_value)
        }
        Event::UserAdded { username, role, .. } => {
            format!("Added user '{}' as {:?}", username, role)
        }
        Event::UserModified { user_id, field, .. } => {
            format!("Modified {} for user {}", field, truncate(user_id, 8))
        }
        Event::UserRemoved { user_id } => {
            format!("Removed user {}", truncate(user_id, 8))
        }
        Event::AccountCreated {
            account_number,
            name,
            account_type,
            ..
        } => {
            format!(
                "{} {} - {} ({:?})",
                account_number,
                name,
                truncate(name, 20),
                account_type
            )
        }
        Event::AccountUpdated {
            account_id,
            field,
            new_value,
            ..
        } => {
            format!(
                "Updated {} to '{}' on {}",
                field,
                new_value,
                truncate(account_id, 8)
            )
        }
        Event::AccountDeactivated { account_id, .. } => {
            format!("Deactivated account {}", truncate(account_id, 8))
        }
        Event::AccountReactivated { account_id } => {
            format!("Reactivated account {}", truncate(account_id, 8))
        }
        Event::JournalEntryPosted { memo, lines, .. } => {
            let amount = lines
                .iter()
                .filter(|l| l.amount > 0)
                .map(|l| l.amount)
                .sum::<i64>();
            format!("{} (${:.2})", truncate(memo, 40), amount as f64 / 100.0)
        }
        Event::JournalEntryVoided { entry_id, reason } => {
            format!(
                "Voided {} - {}",
                truncate(entry_id, 8),
                truncate(reason, 30)
            )
        }
        Event::JournalEntryUnvoided { entry_id, reason } => {
            format!(
                "Unvoided {} - {}",
                truncate(entry_id, 8),
                truncate(reason, 30)
            )
        }
        Event::JournalEntryAnnotated {
            entry_id,
            annotation,
        } => {
            format!(
                "Annotated {} - {}",
                truncate(entry_id, 8),
                truncate(annotation, 30)
            )
        }
        Event::JournalLineReassigned {
            line_id,
            new_account_id,
            ..
        } => {
            format!(
                "Reassigned line {} to account {}",
                truncate(line_id, 8),
                truncate(new_account_id, 8)
            )
        }
        Event::FiscalYearOpened { year, .. } => {
            format!("Opened fiscal year {}", year)
        }
        Event::PeriodClosed { year, period, .. } => {
            format!("Closed period {} of {}", period, year)
        }
        Event::PeriodReopened {
            year,
            period,
            reason,
            ..
        } => {
            format!(
                "Reopened period {} of {} - {}",
                period,
                year,
                truncate(reason, 20)
            )
        }
        Event::YearEndClosed { year, .. } => {
            format!("Closed year-end {}", year)
        }
        Event::CurrencyEnabled { code, name, .. } => {
            format!("Enabled currency {} ({})", code, name)
        }
        Event::ExchangeRateRecorded {
            from_currency,
            to_currency,
            rate,
            ..
        } => {
            format!("{}/{} = {}", from_currency, to_currency, rate)
        }
        Event::ReconciliationStarted {
            account_id,
            statement_date,
            ..
        } => {
            format!(
                "Started reconciliation for {} on {}",
                truncate(account_id, 8),
                statement_date
            )
        }
        Event::TransactionCleared { entry_id, .. } => {
            format!("Cleared transaction {}", truncate(entry_id, 8))
        }
        Event::TransactionUncleared { entry_id, .. } => {
            format!("Uncleared transaction {}", truncate(entry_id, 8))
        }
        Event::ReconciliationCompleted {
            reconciliation_id,
            difference,
        } => {
            format!(
                "Completed reconciliation {} (diff: {})",
                truncate(reconciliation_id, 8),
                difference
            )
        }
        Event::ReconciliationAbandoned { reconciliation_id } => {
            format!(
                "Abandoned reconciliation {}",
                truncate(reconciliation_id, 8)
            )
        }
        Event::PlaidItemConnected {
            institution_name,
            plaid_accounts,
            ..
        } => {
            format!(
                "Connected {} ({} accounts)",
                institution_name,
                plaid_accounts.len()
            )
        }
        Event::PlaidItemDisconnected { item_id, reason } => {
            format!(
                "Disconnected {} - {}",
                truncate(item_id, 8),
                truncate(reason, 30)
            )
        }
        Event::PlaidAccountMapped {
            plaid_account_id,
            local_account_id,
            ..
        } => {
            format!(
                "Mapped Plaid {} to {}",
                truncate(plaid_account_id, 8),
                truncate(local_account_id, 8)
            )
        }
        Event::PlaidAccountUnmapped {
            plaid_account_id,
            local_account_id,
            ..
        } => {
            format!(
                "Unmapped Plaid {} from {}",
                truncate(plaid_account_id, 8),
                truncate(local_account_id, 8)
            )
        }
        Event::PlaidTransactionsSynced {
            transactions_added,
            item_id,
            ..
        } => {
            format!(
                "Synced {} transactions for {}",
                transactions_added,
                truncate(item_id, 8)
            )
        }
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
