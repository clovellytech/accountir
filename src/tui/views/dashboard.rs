use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table},
    Frame,
};

use crate::domain::AccountType;
use crate::queries::account_queries::AccountBalance;

pub struct DashboardView {
    pub balances: Vec<AccountBalance>,
}

impl DashboardView {
    pub fn new() -> Self {
        Self {
            balances: Vec::new(),
        }
    }

    pub fn handle_key(&mut self, _key: KeyCode) {
        // Dashboard has no interactive elements yet
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5), // Summary
                Constraint::Min(0),    // Account type breakdown
            ])
            .split(area);

        self.draw_summary(frame, chunks[0]);
        self.draw_breakdown(frame, chunks[1]);
    }

    fn draw_summary(&self, frame: &mut Frame, area: Rect) {
        let total_assets: i64 = self
            .balances
            .iter()
            .filter(|b| matches!(b.account_type, AccountType::Asset))
            .map(|b| b.balance)
            .sum();

        let total_liabilities: i64 = self
            .balances
            .iter()
            .filter(|b| matches!(b.account_type, AccountType::Liability))
            .map(|b| -b.balance) // Liabilities have credit balances (negative)
            .sum();

        let total_equity: i64 = self
            .balances
            .iter()
            .filter(|b| matches!(b.account_type, AccountType::Equity))
            .map(|b| -b.balance)
            .sum();

        let net_income: i64 = {
            let revenue: i64 = self
                .balances
                .iter()
                .filter(|b| matches!(b.account_type, AccountType::Revenue))
                .map(|b| -b.balance)
                .sum();
            let expenses: i64 = self
                .balances
                .iter()
                .filter(|b| matches!(b.account_type, AccountType::Expense))
                .map(|b| b.balance)
                .sum();
            revenue - expenses
        };

        let summary = Paragraph::new(vec![
            Line::from(vec![
                Span::raw("Total Assets: "),
                Span::styled(
                    format_currency(total_assets),
                    Style::default().fg(Color::Green),
                ),
                Span::raw("  |  Total Liabilities: "),
                Span::styled(
                    format_currency(total_liabilities),
                    Style::default().fg(Color::Red),
                ),
                Span::raw("  |  Equity: "),
                Span::styled(
                    format_currency(total_equity),
                    Style::default().fg(Color::Blue),
                ),
            ]),
            Line::from(vec![
                Span::raw("Net Income: "),
                Span::styled(
                    format_currency(net_income),
                    Style::default().fg(if net_income >= 0 {
                        Color::Green
                    } else {
                        Color::Red
                    }),
                ),
            ]),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Financial Summary "),
        );

        frame.render_widget(summary, area);
    }

    fn draw_breakdown(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Left side: Assets and Liabilities
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[0]);

        self.draw_account_type_table(frame, left_chunks[0], AccountType::Asset, " Assets ");
        self.draw_account_type_table(
            frame,
            left_chunks[1],
            AccountType::Liability,
            " Liabilities ",
        );

        // Right side: Revenue and Expenses
        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);

        self.draw_account_type_table(frame, right_chunks[0], AccountType::Revenue, " Revenue ");
        self.draw_account_type_table(frame, right_chunks[1], AccountType::Expense, " Expenses ");
    }

    fn draw_account_type_table(
        &self,
        frame: &mut Frame,
        area: Rect,
        account_type: AccountType,
        title: &str,
    ) {
        let accounts: Vec<&AccountBalance> = self
            .balances
            .iter()
            .filter(|b| b.account_type == account_type && b.balance != 0)
            .collect();

        let rows: Vec<Row> = accounts
            .iter()
            .map(|acc| {
                let display_balance = match account_type {
                    AccountType::Asset | AccountType::Expense => acc.balance,
                    AccountType::Liability | AccountType::Equity | AccountType::Revenue => {
                        -acc.balance
                    }
                };
                Row::new(vec![
                    acc.account_number.clone(),
                    acc.account_name.clone(),
                    format_currency(display_balance),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(8),
                Constraint::Min(20),
                Constraint::Length(15),
            ],
        )
        .header(
            Row::new(vec!["Number", "Name", "Balance"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(title));

        frame.render_widget(table, area);
    }
}

impl Default for DashboardView {
    fn default() -> Self {
        Self::new()
    }
}

fn format_currency(cents: i64) -> String {
    let dollars = cents as f64 / 100.0;
    if cents < 0 {
        format!("(${:.2})", -dollars)
    } else {
        format!("${:.2}", dollars)
    }
}
