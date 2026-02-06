use chrono::NaiveDate;
use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, Tabs},
    Frame,
};

use crate::queries::reports::{BalanceSheet, IncomeStatement, TrialBalanceLine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportType {
    TrialBalance,
    BalanceSheet,
    IncomeStatement,
}

impl ReportType {
    fn index(&self) -> usize {
        match self {
            ReportType::TrialBalance => 0,
            ReportType::BalanceSheet => 1,
            ReportType::IncomeStatement => 2,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            0 => ReportType::TrialBalance,
            1 => ReportType::BalanceSheet,
            2 => ReportType::IncomeStatement,
            _ => ReportType::TrialBalance,
        }
    }
}

pub struct ReportsView {
    pub active_report: ReportType,
    pub trial_balance: Vec<TrialBalanceLine>,
    pub balance_sheet: Option<BalanceSheet>,
    pub income_statement: Option<IncomeStatement>,
    pub scroll_offset: usize,
    /// The as-of date for balance sheet and income statement
    pub report_date: NaiveDate,
    /// Flag to signal that reports need to be reloaded due to date change
    pub needs_reload: bool,
    /// Whether we're in date editing mode
    pub editing_date: bool,
    /// Buffer for date input
    pub date_input: String,
}

impl ReportsView {
    pub fn new() -> Self {
        Self {
            active_report: ReportType::TrialBalance,
            trial_balance: Vec::new(),
            balance_sheet: None,
            income_statement: None,
            scroll_offset: 0,
            report_date: chrono::Local::now().date_naive(),
            needs_reload: false,
            editing_date: false,
            date_input: String::new(),
        }
    }

    /// Move report date back by one day
    pub fn previous_day(&mut self) {
        if let Some(prev) = self.report_date.pred_opt() {
            self.report_date = prev;
            self.needs_reload = true;
        }
    }

    /// Move report date forward by one day
    pub fn next_day(&mut self) {
        if let Some(next) = self.report_date.succ_opt() {
            self.report_date = next;
            self.needs_reload = true;
        }
    }

    /// Reset report date to today
    pub fn reset_to_today(&mut self) {
        self.report_date = chrono::Local::now().date_naive();
        self.needs_reload = true;
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        // Handle date editing mode
        if self.editing_date {
            match key {
                KeyCode::Esc => {
                    self.editing_date = false;
                    self.date_input.clear();
                }
                KeyCode::Enter => {
                    self.apply_date_input();
                }
                KeyCode::Backspace => {
                    self.date_input.pop();
                }
                KeyCode::Char(c) if c.is_ascii_digit() || c == '-' || c == '/' => {
                    self.date_input.push(c);
                }
                _ => {}
            }
            return;
        }

        match key {
            KeyCode::Left | KeyCode::Char('h') => self.previous_report(),
            KeyCode::Right | KeyCode::Char('l') => self.next_report(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_up(),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_down(),
            // Date navigation for balance sheet/income statement
            KeyCode::Char('[') => self.previous_day(),
            KeyCode::Char(']') => self.next_day(),
            KeyCode::Char('t') => self.reset_to_today(),
            KeyCode::Char('d') => self.start_date_edit(),
            _ => {}
        }
    }

    /// Start editing the date
    fn start_date_edit(&mut self) {
        self.editing_date = true;
        self.date_input = self.report_date.format("%Y-%m-%d").to_string();
    }

    /// Apply the date input
    fn apply_date_input(&mut self) {
        // Try parsing different formats
        let input = self.date_input.replace('/', "-");

        if let Ok(date) = NaiveDate::parse_from_str(&input, "%Y-%m-%d") {
            self.report_date = date;
            self.needs_reload = true;
        } else if let Ok(date) = NaiveDate::parse_from_str(&input, "%m-%d-%Y") {
            self.report_date = date;
            self.needs_reload = true;
        } else if let Ok(date) = NaiveDate::parse_from_str(&input, "%d-%m-%Y") {
            self.report_date = date;
            self.needs_reload = true;
        }

        self.editing_date = false;
        self.date_input.clear();
    }

    fn next_report(&mut self) {
        let next_index = (self.active_report.index() + 1) % 3;
        self.active_report = ReportType::from_index(next_index);
        self.scroll_offset = 0;
    }

    fn previous_report(&mut self) {
        let prev_index = if self.active_report.index() == 0 {
            2
        } else {
            self.active_report.index() - 1
        };
        self.active_report = ReportType::from_index(prev_index);
        self.scroll_offset = 0;
    }

    fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Report tabs
                Constraint::Min(0),    // Report content
            ])
            .split(area);

        // Draw report type tabs
        let titles = vec!["Trial Balance", "Balance Sheet", "Income Statement"];
        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Reports (h/l to switch) "),
            )
            .select(self.active_report.index())
            .style(Style::default().fg(Color::White))
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(tabs, chunks[0]);

        // Draw active report
        match self.active_report {
            ReportType::TrialBalance => self.draw_trial_balance(frame, chunks[1]),
            ReportType::BalanceSheet => self.draw_balance_sheet(frame, chunks[1]),
            ReportType::IncomeStatement => self.draw_income_statement(frame, chunks[1]),
        }
    }

    fn draw_trial_balance(&self, frame: &mut Frame, area: Rect) {
        let rows: Vec<Row> = self
            .trial_balance
            .iter()
            .skip(self.scroll_offset)
            .map(|row| {
                Row::new(vec![
                    row.account_number.clone(),
                    row.account_name.clone(),
                    row.debit.map(format_currency).unwrap_or_default(),
                    row.credit.map(format_currency).unwrap_or_default(),
                ])
            })
            .collect();

        let total_debits: i64 = self.trial_balance.iter().filter_map(|r| r.debit).sum();
        let total_credits: i64 = self.trial_balance.iter().filter_map(|r| r.credit).sum();

        let header = Row::new(vec!["Number", "Account", "Debit", "Credit"])
            .style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Yellow),
            )
            .bottom_margin(1);

        let footer_text = format!(
            "Totals: Debits {} | Credits {} | {}",
            format_currency(total_debits),
            format_currency(total_credits),
            if total_debits == total_credits {
                "BALANCED"
            } else {
                "OUT OF BALANCE"
            }
        );

        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Min(30),
                Constraint::Length(15),
                Constraint::Length(15),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Trial Balance - {} ", footer_text)),
        );

        frame.render_widget(table, area);
    }

    fn draw_balance_sheet(&self, frame: &mut Frame, area: Rect) {
        let Some(bs) = &self.balance_sheet else {
            let msg = Paragraph::new("No balance sheet data available").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Balance Sheet "),
            );
            frame.render_widget(msg, area);
            return;
        };

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        // Assets
        let mut asset_lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "ASSETS",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for acc in &bs.assets.lines {
            asset_lines.push(Line::from(format!(
                "  {} {}",
                acc.account_name,
                format_currency(acc.balance)
            )));
        }
        asset_lines.push(Line::from(""));
        asset_lines.push(Line::from(Span::styled(
            format!("Total Assets: {}", format_currency(bs.total_assets)),
            Style::default().add_modifier(Modifier::BOLD),
        )));

        let date_title = if self.editing_date {
            format!(
                " Assets - Enter date: {}▏ (Enter: apply, Esc: cancel) ",
                self.date_input
            )
        } else {
            format!(
                " Assets (as of {}) d: pick date, [/]: prev/next, t: today ",
                bs.as_of_date
            )
        };
        let assets_widget = Paragraph::new(asset_lines)
            .block(Block::default().borders(Borders::ALL).title(date_title));
        frame.render_widget(assets_widget, chunks[0]);

        // Liabilities & Equity
        let mut le_lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "LIABILITIES",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for acc in &bs.liabilities.lines {
            le_lines.push(Line::from(format!(
                "  {} {}",
                acc.account_name,
                format_currency(acc.balance.abs())
            )));
        }
        le_lines.push(Line::from(Span::styled(
            format!(
                "Total Liabilities: {}",
                format_currency(bs.liabilities.total)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        le_lines.push(Line::from(""));
        le_lines.push(Line::from(Span::styled(
            "EQUITY",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        le_lines.push(Line::from(""));
        for acc in &bs.equity.lines {
            le_lines.push(Line::from(format!(
                "  {} {}",
                acc.account_name,
                format_currency(acc.balance.abs())
            )));
        }
        le_lines.push(Line::from(Span::styled(
            format!("Total Equity: {}", format_currency(bs.equity.total)),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        le_lines.push(Line::from(""));
        le_lines.push(Line::from(Span::styled(
            format!(
                "Total L+E: {}",
                format_currency(bs.total_liabilities_and_equity)
            ),
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Yellow),
        )));

        let le_widget = Paragraph::new(le_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Liabilities & Equity "),
        );
        frame.render_widget(le_widget, chunks[1]);
    }

    fn draw_income_statement(&self, frame: &mut Frame, area: Rect) {
        let Some(is) = &self.income_statement else {
            let msg = Paragraph::new("No income statement data available").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Income Statement "),
            );
            frame.render_widget(msg, area);
            return;
        };

        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                "REVENUE",
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Green),
            )),
            Line::from(""),
        ];

        for acc in &is.revenue.lines {
            lines.push(Line::from(format!(
                "  {} {}",
                acc.account_name,
                format_currency(acc.balance)
            )));
        }
        lines.push(Line::from(Span::styled(
            format!("Total Revenue: {}", format_currency(is.revenue.total)),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));

        lines.push(Line::from(Span::styled(
            "EXPENSES",
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
        )));
        lines.push(Line::from(""));

        for acc in &is.expenses.lines {
            lines.push(Line::from(format!(
                "  {} {}",
                acc.account_name,
                format_currency(acc.balance)
            )));
        }
        lines.push(Line::from(Span::styled(
            format!("Total Expenses: {}", format_currency(is.expenses.total)),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from("═".repeat(40)));
        lines.push(Line::from(""));

        let net_income_style = if is.net_income >= 0 {
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Green)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red)
        };

        lines.push(Line::from(Span::styled(
            format!(
                "NET INCOME: {}",
                if is.net_income >= 0 {
                    format_currency(is.net_income)
                } else {
                    format!("({}) LOSS", format_currency(-is.net_income))
                }
            ),
            net_income_style,
        )));

        let date_title = if self.editing_date {
            format!(
                " Income Statement - Enter date: {}▏ (Enter: apply, Esc: cancel) ",
                self.date_input
            )
        } else {
            format!(
                " Income Statement ({} to {}) d: pick date, [/]: prev/next, t: today ",
                is.start_date, is.end_date
            )
        };
        let widget =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(date_title));
        frame.render_widget(widget, area);
    }
}

impl Default for ReportsView {
    fn default() -> Self {
        Self::new()
    }
}

fn format_currency(cents: i64) -> String {
    let dollars = cents as f64 / 100.0;
    format!("${:.2}", dollars)
}
