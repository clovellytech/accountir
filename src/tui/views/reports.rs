use chrono::NaiveDate;
use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph, Row, Table, Tabs},
    Frame,
};

use std::collections::HashMap;

use crate::queries::reports::{
    BalanceSheet, BalanceSheetLine, IncomeStatement, IncomeStatementLine, TrialBalanceLine,
};
use crate::tui::theme::Theme;
use crate::tui::widgets;

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

/// A row in the rendered report tree
#[derive(Debug, Clone)]
enum ReportRow {
    /// A leaf account or collapsed parent — balance in the Balance column
    Account {
        name: String,
        balance: i64,
        depth: usize,
        is_last_child: bool,
        ancestor_is_last: Vec<bool>,
    },
    /// A parent header when children are visible — no amount shown
    ParentHeader {
        name: String,
        depth: usize,
        is_last_child: bool,
        ancestor_is_last: Vec<bool>,
    },
    /// "Total [Parent Name]" after children — amount in Subtotal column
    Subtotal {
        parent_name: String,
        amount: i64,
        depth: usize,
    },
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
    /// Collapse depth: accounts deeper than this are aggregated into parents.
    /// None means fully expanded (no collapsing).
    pub collapse_depth: Option<usize>,
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
            collapse_depth: None,
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

    fn increase_depth(&mut self) {
        match self.collapse_depth {
            None => {}
            Some(d) => {
                let max = self.max_account_depth();
                if d + 1 > max {
                    self.collapse_depth = None;
                } else {
                    self.collapse_depth = Some(d + 1);
                }
            }
        }
    }

    fn decrease_depth(&mut self) {
        match self.collapse_depth {
            None => {
                let max = self.max_account_depth();
                if max > 0 {
                    self.collapse_depth = Some(max);
                }
            }
            Some(d) if d > 0 => {
                self.collapse_depth = Some(d - 1);
            }
            _ => {}
        }
    }

    /// Find the maximum depth in the current report's account tree
    fn max_account_depth(&self) -> usize {
        match self.active_report {
            ReportType::BalanceSheet => {
                let Some(bs) = &self.balance_sheet else {
                    return 0;
                };
                let all_lines: Vec<_> = bs
                    .assets
                    .lines
                    .iter()
                    .chain(bs.liabilities.lines.iter())
                    .chain(bs.equity.lines.iter())
                    .map(|l| (l.account_id.as_str(), l.parent_id.as_deref()))
                    .collect();
                compute_max_depth(&all_lines)
            }
            ReportType::IncomeStatement => {
                let Some(is) = &self.income_statement else {
                    return 0;
                };
                let all_lines: Vec<_> = is
                    .revenue
                    .lines
                    .iter()
                    .chain(is.expenses.lines.iter())
                    .map(|l| (l.account_id.as_str(), l.parent_id.as_deref()))
                    .collect();
                compute_max_depth(&all_lines)
            }
            ReportType::TrialBalance => 0,
        }
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
            KeyCode::Char('[') => self.previous_day(),
            KeyCode::Char(']') => self.next_day(),
            KeyCode::Char('t') => self.reset_to_today(),
            KeyCode::Char('d') => self.start_date_edit(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.increase_depth(),
            KeyCode::Char('-') => self.decrease_depth(),
            _ => {}
        }
    }

    fn start_date_edit(&mut self) {
        self.editing_date = true;
        self.date_input = self.report_date.format("%Y-%m-%d").to_string();
    }

    fn apply_date_input(&mut self) {
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

    fn collapse_depth_label(&self) -> String {
        match self.collapse_depth {
            None => "all".to_string(),
            Some(d) => d.to_string(),
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Report tabs
                Constraint::Min(0),    // Report content
            ])
            .split(area);

        let titles = vec!["Trial Balance", "Balance Sheet", "Income Statement"];
        let tabs = Tabs::new(titles)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Reports (h/l to switch) "),
            )
            .select(self.active_report.index())
            .style(Style::default().fg(theme.fg))
            .highlight_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_widget(tabs, chunks[0]);

        match self.active_report {
            ReportType::TrialBalance => self.draw_trial_balance(frame, chunks[1], theme),
            ReportType::BalanceSheet => self.draw_balance_sheet(frame, chunks[1], theme),
            ReportType::IncomeStatement => self.draw_income_statement(frame, chunks[1], theme),
        }
    }

    fn draw_trial_balance(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let table_area = constrain_width(area, 80);
        let bold = Style::default().add_modifier(Modifier::BOLD);

        let mut rows: Vec<Row> = self
            .trial_balance
            .iter()
            .skip(self.scroll_offset)
            .map(|row| {
                Row::new(vec![
                    row.account_number.clone(),
                    row.account_name.clone(),
                    row.debit.map(widgets::format_currency).unwrap_or_default(),
                    row.credit.map(widgets::format_currency).unwrap_or_default(),
                ])
            })
            .collect();

        let total_debits: i64 = self.trial_balance.iter().filter_map(|r| r.debit).sum();
        let total_credits: i64 = self.trial_balance.iter().filter_map(|r| r.credit).sum();

        let header = Row::new(vec!["Number", "Account", "Debit", "Credit"])
            .style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(theme.header),
            )
            .bottom_margin(1);

        let balanced_label = if total_debits == total_credits {
            "BALANCED"
        } else {
            "OUT OF BALANCE"
        };

        // Totals with rules
        rows.push(Row::new(vec![
            String::new(),
            String::new(),
            SINGLE_LINE.to_string(),
            SINGLE_LINE.to_string(),
        ]));
        rows.push(
            Row::new(vec![
                String::new(),
                "Totals".to_string(),
                widgets::format_currency(total_debits),
                widgets::format_currency(total_credits),
            ])
            .style(bold),
        );
        rows.push(Row::new(vec![
            String::new(),
            String::new(),
            DOUBLE_LINE.to_string(),
            DOUBLE_LINE.to_string(),
        ]));

        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Fill(1),
                Constraint::Length(15),
                Constraint::Length(15),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Trial Balance - {} ", balanced_label)),
        );

        frame.render_widget(table, table_area);
    }

    fn draw_balance_sheet(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
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

        let bold = Style::default().add_modifier(Modifier::BOLD);

        // Assets
        let mut asset_rows: Vec<Row> = Vec::new();
        asset_rows.push(Row::new(vec!["ASSETS", "", ""]).style(bold));
        asset_rows.push(Row::new(vec!["", "", ""]));

        let asset_tree = build_report_rows(&bs_to_inputs(&bs.assets.lines), self.collapse_depth);
        render_report_rows(&asset_tree, widgets::format_currency, &mut asset_rows);

        push_single_rule(&mut asset_rows);
        asset_rows.push(
            Row::new(vec![
                "Total Assets".to_string(),
                String::new(),
                widgets::format_currency(bs.total_assets),
            ])
            .style(bold),
        );
        push_double_rule(&mut asset_rows);

        let depth_hint = format!("depth: {} (+/-)", self.collapse_depth_label());
        let date_title = if self.editing_date {
            format!(" Enter date: {}▏ (Enter/Esc) ", self.date_input)
        } else {
            format!(
                " Assets (as of {}) [/]: nav, d: date, t: today, {} ",
                bs.as_of_date, depth_hint
            )
        };
        let assets_table = Table::new(
            asset_rows,
            [
                Constraint::Fill(1),
                Constraint::Length(15),
                Constraint::Length(15),
            ],
        )
        .block(Block::default().borders(Borders::ALL).title(date_title));
        frame.render_widget(assets_table, chunks[0]);

        // Liabilities & Equity
        let mut le_rows: Vec<Row> = Vec::new();
        le_rows.push(Row::new(vec!["LIABILITIES", "", ""]).style(bold));
        le_rows.push(Row::new(vec!["", "", ""]));

        let liab_tree =
            build_report_rows(&bs_to_inputs(&bs.liabilities.lines), self.collapse_depth);
        render_report_rows(&liab_tree, |b| widgets::format_currency(b.abs()), &mut le_rows);

        push_single_rule(&mut le_rows);
        le_rows.push(
            Row::new(vec![
                "Total Liabilities".to_string(),
                String::new(),
                widgets::format_currency(bs.liabilities.total),
            ])
            .style(bold),
        );
        push_single_rule(&mut le_rows);
        le_rows.push(Row::new(vec!["", "", ""]));
        le_rows.push(Row::new(vec!["EQUITY", "", ""]).style(bold));
        le_rows.push(Row::new(vec!["", "", ""]));

        let equity_tree = build_report_rows(&bs_to_inputs(&bs.equity.lines), self.collapse_depth);
        render_report_rows(&equity_tree, |b| widgets::format_currency(b.abs()), &mut le_rows);

        push_single_rule(&mut le_rows);
        le_rows.push(
            Row::new(vec![
                "Total Equity".to_string(),
                String::new(),
                widgets::format_currency(bs.equity.total),
            ])
            .style(bold),
        );
        push_single_rule(&mut le_rows);
        le_rows.push(Row::new(vec!["", "", ""]));

        push_single_rule(&mut le_rows);
        le_rows.push(
            Row::new(vec![
                "Total L+E".to_string(),
                String::new(),
                widgets::format_currency(bs.total_liabilities_and_equity),
            ])
            .style(bold.fg(theme.header)),
        );
        push_double_rule(&mut le_rows);

        let le_table = Table::new(
            le_rows,
            [
                Constraint::Fill(1),
                Constraint::Length(15),
                Constraint::Length(15),
            ],
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Liabilities & Equity "),
        );
        frame.render_widget(le_table, chunks[1]);
    }

    fn draw_income_statement(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(is) = &self.income_statement else {
            let msg = Paragraph::new("No income statement data available").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Income Statement "),
            );
            frame.render_widget(msg, area);
            return;
        };

        // Constrain width so the table isn't too spread out
        let table_area = constrain_width(area, 75);

        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut rows: Vec<Row> = Vec::new();

        rows.push(Row::new(vec!["REVENUE", "", ""]).style(bold.fg(theme.success)));
        rows.push(Row::new(vec!["", "", ""]));
        let rev_tree = build_report_rows(&is_to_inputs(&is.revenue.lines), self.collapse_depth);
        render_report_rows(&rev_tree, widgets::format_currency, &mut rows);
        push_single_rule(&mut rows);
        rows.push(
            Row::new(vec![
                "Total Revenue".to_string(),
                String::new(),
                widgets::format_currency(is.revenue.total),
            ])
            .style(bold),
        );
        push_single_rule(&mut rows);
        rows.push(Row::new(vec!["", "", ""]));

        rows.push(Row::new(vec!["EXPENSES", "", ""]).style(bold.fg(theme.error)));
        rows.push(Row::new(vec!["", "", ""]));
        let exp_tree = build_report_rows(&is_to_inputs(&is.expenses.lines), self.collapse_depth);
        render_report_rows(&exp_tree, widgets::format_currency, &mut rows);
        push_single_rule(&mut rows);
        rows.push(
            Row::new(vec![
                "Total Expenses".to_string(),
                String::new(),
                widgets::format_currency(is.expenses.total),
            ])
            .style(bold),
        );
        push_single_rule(&mut rows);
        rows.push(Row::new(vec!["", "", ""]));

        let net_income_style = if is.net_income >= 0 {
            bold.fg(theme.success)
        } else {
            bold.fg(theme.error)
        };
        let net_income_str = if is.net_income >= 0 {
            widgets::format_currency(is.net_income)
        } else {
            format!("({}) LOSS", widgets::format_currency(-is.net_income))
        };
        push_single_rule(&mut rows);
        rows.push(
            Row::new(vec![
                "NET INCOME".to_string(),
                String::new(),
                net_income_str,
            ])
            .style(net_income_style),
        );
        push_double_rule(&mut rows);

        let depth_hint = format!("depth: {} (+/-)", self.collapse_depth_label());
        let date_title = if self.editing_date {
            format!(" Enter date: {}▏ (Enter/Esc) ", self.date_input)
        } else {
            format!(
                " Income Statement ({} to {}) [/]: nav, d: date, {} ",
                is.start_date, is.end_date, depth_hint
            )
        };
        let table = Table::new(
            rows,
            [
                Constraint::Fill(1),
                Constraint::Length(15),
                Constraint::Length(15),
            ],
        )
        .block(Block::default().borders(Borders::ALL).title(date_title));
        frame.render_widget(table, table_area);
    }
}

impl Default for ReportsView {
    fn default() -> Self {
        Self::new()
    }
}


const SINGLE_LINE: &str = "───────────────";
const DOUBLE_LINE: &str = "═══════════════";

/// Push a single horizontal rule row (3-column: empty | line | line)
fn push_single_rule(rows: &mut Vec<Row>) {
    rows.push(Row::new(vec![
        String::new(),
        SINGLE_LINE.to_string(),
        SINGLE_LINE.to_string(),
    ]));
}

/// Push a double horizontal rule row (3-column: empty | line | line)
fn push_double_rule(rows: &mut Vec<Row>) {
    rows.push(Row::new(vec![
        String::new(),
        DOUBLE_LINE.to_string(),
        DOUBLE_LINE.to_string(),
    ]));
}

/// Constrain an area to a max width, left-aligned
fn constrain_width(area: Rect, max_width: u16) -> Rect {
    if area.width <= max_width {
        area
    } else {
        Rect {
            x: area.x,
            y: area.y,
            width: max_width,
            height: area.height,
        }
    }
}

// --- Tree building infrastructure ---

/// Common input for the tree builder, extracted from either BS or IS lines
struct TreeInput<'a> {
    account_id: &'a str,
    account_number: &'a str,
    account_name: &'a str,
    parent_id: Option<&'a str>,
    balance: i64,
}

fn bs_to_inputs(lines: &[BalanceSheetLine]) -> Vec<TreeInput<'_>> {
    lines
        .iter()
        .map(|l| TreeInput {
            account_id: &l.account_id,
            account_number: &l.account_number,
            account_name: &l.account_name,
            parent_id: l.parent_id.as_deref(),
            balance: l.balance,
        })
        .collect()
}

fn is_to_inputs(lines: &[IncomeStatementLine]) -> Vec<TreeInput<'_>> {
    lines
        .iter()
        .map(|l| TreeInput {
            account_id: &l.account_id,
            account_number: &l.account_number,
            account_name: &l.account_name,
            parent_id: l.parent_id.as_deref(),
            balance: l.balance,
        })
        .collect()
}

/// Compute the max depth of accounts in a set of (id, parent_id) pairs
fn compute_max_depth(lines: &[(&str, Option<&str>)]) -> usize {
    let ids: std::collections::HashSet<&str> = lines.iter().map(|(id, _)| *id).collect();
    let parent_map: HashMap<&str, Option<&str>> =
        lines.iter().map(|(id, pid)| (*id, *pid)).collect();

    let mut max_depth = 0;
    for (id, _) in lines {
        let mut depth = 0;
        let mut current = *id;
        while let Some(Some(pid)) = parent_map.get(current) {
            if ids.contains(pid) {
                depth += 1;
                current = pid;
            } else {
                break;
            }
        }
        if depth > max_depth {
            max_depth = depth;
        }
    }
    max_depth
}

/// Build a flat list of ReportRows from tree inputs.
///
/// - Parent accounts with visible children emit: ParentHeader, children..., Subtotal
/// - Leaf accounts or collapsed parents emit: Account (with balance/aggregated total)
fn build_report_rows(inputs: &[TreeInput<'_>], collapse_depth: Option<usize>) -> Vec<ReportRow> {
    if inputs.is_empty() {
        return Vec::new();
    }

    let id_set: std::collections::HashSet<&str> = inputs.iter().map(|l| l.account_id).collect();

    // Build parent->children index map
    let mut children_map: HashMap<Option<&str>, Vec<usize>> = HashMap::new();
    for (idx, input) in inputs.iter().enumerate() {
        let effective_parent = match input.parent_id {
            Some(pid) if id_set.contains(pid) => Some(pid),
            _ => None,
        };
        children_map.entry(effective_parent).or_default().push(idx);
    }

    // Sort children by account number
    for children in children_map.values_mut() {
        children.sort_by(|&a, &b| inputs[a].account_number.cmp(inputs[b].account_number));
    }

    // Compute subtotals for every node (own balance + all descendants)
    let mut subtotals: HashMap<&str, i64> = HashMap::new();
    fn compute_subtotal<'a>(
        inputs: &[TreeInput<'a>],
        children_map: &HashMap<Option<&'a str>, Vec<usize>>,
        account_id: &'a str,
        subtotals: &mut HashMap<&'a str, i64>,
    ) -> i64 {
        if let Some(&cached) = subtotals.get(account_id) {
            return cached;
        }
        let own_balance = inputs
            .iter()
            .find(|i| i.account_id == account_id)
            .map(|i| i.balance)
            .unwrap_or(0);
        let children_total: i64 = children_map
            .get(&Some(account_id))
            .map(|children| {
                children
                    .iter()
                    .map(|&idx| {
                        compute_subtotal(inputs, children_map, inputs[idx].account_id, subtotals)
                    })
                    .sum()
            })
            .unwrap_or(0);
        let total = own_balance + children_total;
        subtotals.insert(account_id, total);
        total
    }

    for input in inputs {
        compute_subtotal(inputs, &children_map, input.account_id, &mut subtotals);
    }

    // Walk the tree and emit ReportRows
    let mut result = Vec::new();

    #[allow(clippy::too_many_arguments)]
    fn emit_rows<'a>(
        inputs: &[TreeInput<'a>],
        children_map: &HashMap<Option<&'a str>, Vec<usize>>,
        subtotals: &HashMap<&str, i64>,
        parent_id: Option<&'a str>,
        depth: usize,
        collapse_depth: Option<usize>,
        ancestor_is_last: &[bool],
        result: &mut Vec<ReportRow>,
    ) {
        let Some(children) = children_map.get(&parent_id) else {
            return;
        };
        let count = children.len();
        for (i, &idx) in children.iter().enumerate() {
            let input = &inputs[idx];
            let is_last = i == count - 1;
            let has_visible_children = {
                let has_kids = children_map
                    .get(&Some(input.account_id))
                    .map(|c| !c.is_empty())
                    .unwrap_or(false);
                let within_depth = match collapse_depth {
                    None => true,
                    Some(max_d) => depth < max_d,
                };
                has_kids && within_depth
            };

            if has_visible_children {
                // Parent with visible children: emit header, recurse, emit subtotal
                result.push(ReportRow::ParentHeader {
                    name: input.account_name.to_string(),
                    depth,
                    is_last_child: is_last,
                    ancestor_is_last: ancestor_is_last.to_vec(),
                });

                let mut new_ancestors = ancestor_is_last.to_vec();
                new_ancestors.push(is_last);
                emit_rows(
                    inputs,
                    children_map,
                    subtotals,
                    Some(input.account_id),
                    depth + 1,
                    collapse_depth,
                    &new_ancestors,
                    result,
                );

                result.push(ReportRow::Subtotal {
                    parent_name: input.account_name.to_string(),
                    amount: subtotals[input.account_id],
                    depth,
                });
            } else {
                // Leaf or collapsed parent: show aggregated balance
                result.push(ReportRow::Account {
                    name: input.account_name.to_string(),
                    balance: subtotals[input.account_id],
                    depth,
                    is_last_child: is_last,
                    ancestor_is_last: ancestor_is_last.to_vec(),
                });
            }
        }
    }

    emit_rows(
        inputs,
        &children_map,
        &subtotals,
        None,
        0,
        collapse_depth,
        &[],
        &mut result,
    );

    result
}

/// Convert ReportRows into table Rows (3-column: Account | Balance | Subtotal).
/// `fmt_balance` controls how the raw i64 balance is formatted (e.g. abs() for liabilities).
fn render_report_rows(
    report_rows: &[ReportRow],
    fmt_balance: impl Fn(i64) -> String,
    out: &mut Vec<Row>,
) {
    let bold = Style::default().add_modifier(Modifier::BOLD);

    for row in report_rows {
        match row {
            ReportRow::Account {
                name,
                balance,
                depth,
                is_last_child,
                ancestor_is_last,
            } => {
                let prefix = make_tree_prefix(*depth, *is_last_child, ancestor_is_last);
                let name_col = format!("  {}{}", prefix, name);
                out.push(Row::new(vec![
                    name_col,
                    fmt_balance(*balance),
                    String::new(),
                ]));
            }
            ReportRow::ParentHeader {
                name,
                depth,
                is_last_child,
                ancestor_is_last,
            } => {
                let prefix = make_tree_prefix(*depth, *is_last_child, ancestor_is_last);
                let name_col = format!("  {}{}", prefix, name);
                out.push(Row::new(vec![name_col, String::new(), String::new()]).style(bold));
            }
            ReportRow::Subtotal {
                parent_name,
                amount,
                depth,
            } => {
                // Single rule above subtotal
                out.push(Row::new(vec![
                    String::new(),
                    String::new(),
                    SINGLE_LINE.to_string(),
                ]));
                // Indent the subtotal line to align with the parent's children
                let indent = "   ".repeat(*depth);
                let name_col = format!("  {}  Total {}", indent, parent_name);
                out.push(Row::new(vec![name_col, String::new(), fmt_balance(*amount)]).style(bold));
            }
        }
    }
}

/// Generate tree prefix string for a node at given depth
fn make_tree_prefix(depth: usize, is_last_child: bool, ancestor_is_last: &[bool]) -> String {
    if depth == 0 {
        return String::new();
    }

    let mut prefix = String::new();
    for &ail in ancestor_is_last {
        if ail {
            prefix.push_str("   ");
        } else {
            prefix.push_str("│  ");
        }
    }

    if is_last_child {
        prefix.push_str("└─ ");
    } else {
        prefix.push_str("├─ ");
    }

    prefix
}
