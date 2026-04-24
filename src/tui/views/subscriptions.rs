use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table},
    Frame,
};

use crate::queries::subscriptions::DetectedSubscription;
use crate::tui::theme::Theme;
use crate::tui::widgets;

pub struct SubscriptionsModal {
    pub visible: bool,
    pub subscriptions: Vec<DetectedSubscription>,
    pub selected: usize,
    scroll: usize,
}

impl Default for SubscriptionsModal {
    fn default() -> Self {
        Self::new()
    }
}

impl SubscriptionsModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            subscriptions: Vec::new(),
            selected: 0,
            scroll: 0,
        }
    }

    pub fn show(&mut self, subscriptions: Vec<DetectedSubscription>) {
        self.subscriptions = subscriptions;
        self.visible = true;
        self.selected = 0;
        self.scroll = 0;
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.subscriptions.clear();
    }

    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        if !self.visible {
            return false;
        }
        match key {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.hide();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.subscriptions.is_empty() {
                    self.selected = (self.selected + 1).min(self.subscriptions.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => {
                if !self.subscriptions.is_empty() {
                    self.selected = self.subscriptions.len() - 1;
                }
            }
            _ => {}
        }
        true
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        // Center the modal, taking most of the screen
        let modal_width = area.width.saturating_sub(6).min(100);
        let modal_height = area.height.saturating_sub(4);
        let modal_x = (area.width.saturating_sub(modal_width)) / 2;
        let modal_y = (area.height.saturating_sub(modal_height)) / 2;

        let modal_area = Rect {
            x: modal_x,
            y: modal_y,
            width: modal_width,
            height: modal_height,
        };

        frame.render_widget(Clear, modal_area);

        if self.subscriptions.is_empty() {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Find Subscriptions ")
                .title_style(theme.modal_title_style())
                .border_style(theme.border_style());

            let msg = Paragraph::new(Line::from(Span::styled(
                "  No recurring transactions detected in Uncategorized.",
                Style::default().fg(theme.fg_dim),
            )))
            .block(block);
            frame.render_widget(msg, modal_area);
            return;
        }

        // Calculate monthly total
        let monthly_total: i64 = self
            .subscriptions
            .iter()
            .map(|s| {
                let monthly_multiplier = match s.frequency {
                    crate::queries::subscriptions::SubscriptionFrequency::Weekly => 4.33,
                    crate::queries::subscriptions::SubscriptionFrequency::Biweekly => 2.17,
                    crate::queries::subscriptions::SubscriptionFrequency::Monthly => 1.0,
                    crate::queries::subscriptions::SubscriptionFrequency::Quarterly => 1.0 / 3.0,
                    crate::queries::subscriptions::SubscriptionFrequency::Annual => 1.0 / 12.0,
                };
                (s.avg_amount.abs() as f64 * monthly_multiplier).round() as i64
            })
            .sum();

        let title = format!(
            " Find Subscriptions ({} found, ~{}/mo) ",
            self.subscriptions.len(),
            format_amount(monthly_total)
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_style(theme.modal_title_style())
            .border_style(theme.border_style());

        // Split into table area + help line
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let table_height = inner.height.saturating_sub(1) as usize;
        let help_area = Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        };
        let table_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        };

        // Adjust scroll
        if table_height > 1 {
            let visible = table_height.saturating_sub(1); // minus header
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + visible {
                self.scroll = self.selected.saturating_sub(visible - 1);
            }
        }

        let header = Row::new(vec![
            Cell::from("Name"),
            Cell::from("Frequency"),
            Cell::from("Amount"),
            Cell::from("Count"),
            Cell::from("Last Date"),
        ])
        .style(
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        );

        let visible_rows = table_height.saturating_sub(1);
        let rows: Vec<Row> = self
            .subscriptions
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible_rows)
            .map(|(i, sub)| {
                let style = if i == self.selected {
                    theme.selected_style()
                } else {
                    Style::default()
                };

                Row::new(vec![
                    Cell::from(widgets::truncate(&sub.memo, 40)),
                    Cell::from(sub.frequency.label()),
                    Cell::from(format_amount(sub.avg_amount.abs())),
                    Cell::from(format!("{}", sub.occurrence_count)),
                    Cell::from(sub.last_date.to_string()),
                ])
                .style(style)
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Percentage(38),
                Constraint::Percentage(14),
                Constraint::Percentage(16),
                Constraint::Percentage(10),
                Constraint::Percentage(22),
            ],
        )
        .header(header);

        frame.render_widget(table, table_area);

        // Help line
        let help = Line::from(vec![
            Span::styled("j/k", Style::default().fg(theme.header)),
            Span::raw(": navigate  "),
            Span::styled("Esc", Style::default().fg(theme.header)),
            Span::raw(": close"),
        ]);
        frame.render_widget(Paragraph::new(help), help_area);
    }
}

fn format_amount(cents: i64) -> String {
    let dollars = cents / 100;
    let remainder = (cents % 100).unsigned_abs();
    format!("${}.{:02}", dollars, remainder)
}

