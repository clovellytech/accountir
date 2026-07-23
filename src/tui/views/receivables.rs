use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::queries::ap_ar_queries::{AgingBucket, InvoiceRow};
use crate::tui::theme::Theme;

pub enum ReceivablesAction {
    None,
    Refresh,
}

pub struct ReceivablesView {
    pub invoices: Vec<InvoiceRow>,
    pub aging: AgingBucket,
    pub selected: usize,
}

impl Default for ReceivablesView {
    fn default() -> Self {
        Self::new()
    }
}

impl ReceivablesView {
    pub fn new() -> Self {
        Self {
            invoices: Vec::new(),
            aging: AgingBucket::default(),
            selected: 0,
        }
    }

    pub fn set_data(&mut self, invoices: Vec<InvoiceRow>, aging: AgingBucket) {
        self.invoices = invoices;
        self.aging = aging;
        if self.selected >= self.invoices.len() && !self.invoices.is_empty() {
            self.selected = self.invoices.len() - 1;
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) -> ReceivablesAction {
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.invoices.is_empty() {
                    self.selected = (self.selected + 1).min(self.invoices.len() - 1);
                }
                ReceivablesAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                ReceivablesAction::None
            }
            KeyCode::Char('r') => ReceivablesAction::Refresh,
            _ => ReceivablesAction::None,
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(5)]).split(area);

        self.render_aging(frame, chunks[0], theme);
        self.render_invoices(frame, chunks[1], theme);
    }

    fn render_aging(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" AR Aging Summary ")
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

    fn render_invoices(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Open Invoices (j/k: navigate, r: refresh) ")
            .title_style(Style::default().fg(theme.accent));

        if self.invoices.is_empty() {
            let content = Paragraph::new(Line::from(Span::styled(
                "No open invoices.",
                Style::default().fg(theme.fg_dim),
            )))
            .block(block);
            frame.render_widget(content, area);
            return;
        }

        let header = Row::new(vec![
            Cell::from("Due Date"),
            Cell::from("Customer"),
            Cell::from("Amount"),
            Cell::from("Received"),
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
            .invoices
            .iter()
            .enumerate()
            .map(|(i, inv)| {
                let is_overdue = inv.due_date < today && inv.status != "paid" && inv.status != "void";
                let balance = inv.amount - inv.amount_paid;

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
                    Cell::from(inv.due_date.clone()),
                    Cell::from(inv.customer.clone()),
                    Cell::from(format_amount(inv.amount)),
                    Cell::from(format_amount(inv.amount_paid)),
                    Cell::from(format_amount(balance)),
                    Cell::from(inv.status.clone()),
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
