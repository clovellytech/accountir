use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::queries::ap_ar_queries::{AgingBucket, BillRow};
use crate::tui::theme::Theme;

pub enum PayablesAction {
    None,
    Refresh,
}

pub struct PayablesView {
    pub bills: Vec<BillRow>,
    pub aging: AgingBucket,
    pub selected: usize,
}

impl Default for PayablesView {
    fn default() -> Self {
        Self::new()
    }
}

impl PayablesView {
    pub fn new() -> Self {
        Self {
            bills: Vec::new(),
            aging: AgingBucket::default(),
            selected: 0,
        }
    }

    pub fn set_data(&mut self, bills: Vec<BillRow>, aging: AgingBucket) {
        self.bills = bills;
        self.aging = aging;
        if self.selected >= self.bills.len() && !self.bills.is_empty() {
            self.selected = self.bills.len() - 1;
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> PayablesAction {
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.bills.is_empty() {
                    self.selected = (self.selected + 1).min(self.bills.len() - 1);
                }
                PayablesAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                PayablesAction::None
            }
            KeyCode::Char('r') => PayablesAction::Refresh,
            _ => PayablesAction::None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(5)]).split(area);

        // Aging summary bar
        self.render_aging(frame, chunks[0], theme);

        // Bills table
        self.render_bills(frame, chunks[1], theme);
    }

    fn render_aging(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" AP Aging Summary ")
            .title_style(Style::default().fg(theme.accent));

        let a = &self.aging;
        let line = Line::from(vec![
            Span::styled("Current: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                format_amount(a.current),
                Style::default().fg(theme.success),
            ),
            Span::raw("  "),
            Span::styled("1-30: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                format_amount(a.days_1_30),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("  "),
            Span::styled("31-60: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                format_amount(a.days_31_60),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("  "),
            Span::styled("61-90: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                format_amount(a.days_61_90),
                Style::default().fg(theme.error),
            ),
            Span::raw("  "),
            Span::styled("90+: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                format_amount(a.days_over_90),
                Style::default().fg(theme.error),
            ),
            Span::raw("  "),
            Span::styled("Total: ", Style::default().fg(theme.fg_dim).add_modifier(Modifier::BOLD)),
            Span::styled(
                format_amount(a.total),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
        ]);

        let content = Paragraph::new(line).block(block);
        frame.render_widget(content, area);
    }

    fn render_bills(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Open Bills (j/k: navigate, r: refresh) ")
            .title_style(Style::default().fg(theme.accent));

        if self.bills.is_empty() {
            let content = Paragraph::new(Line::from(Span::styled(
                "No open bills.",
                Style::default().fg(theme.fg_dim),
            )))
            .block(block);
            frame.render_widget(content, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("Due Date"),
            Cell::from("Vendor"),
            Cell::from("Amount"),
            Cell::from("Paid"),
            Cell::from("Balance"),
            Cell::from("Status"),
        ])
        .style(
            Style::default()
                .fg(theme.header)
                .add_modifier(Modifier::BOLD),
        );

        let today = chrono::Local::now().date_naive().to_string();

        let rows: Vec<Row> = self
            .bills
            .iter()
            .enumerate()
            .map(|(i, bill)| {
                let is_overdue = bill.due_date < today && bill.status != "paid" && bill.status != "void";
                let balance = bill.amount - bill.amount_paid;

                let style = if i == self.selected {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else if is_overdue {
                    Style::default().fg(theme.error)
                } else {
                    Style::default().fg(theme.fg)
                };

                Row::new(vec![
                    Cell::from(bill.due_date.clone()),
                    Cell::from(bill.vendor.clone()),
                    Cell::from(format_amount(bill.amount)),
                    Cell::from(format_amount(bill.amount_paid)),
                    Cell::from(format_amount(balance)),
                    Cell::from(bill.status.clone()),
                ])
                .style(style)
            })
            .collect();

        let widths = [
            Constraint::Length(12),
            Constraint::Min(20),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(10),
        ];

        let table = Table::new(rows, widths).header(header).block(block);

        frame.render_widget(table, area);
    }
}

fn format_amount(cents: i64) -> String {
    if cents < 0 {
        format!("(${:.2})", (-cents) as f64 / 100.0)
    } else {
        format!("${:.2}", cents as f64 / 100.0)
    }
}
