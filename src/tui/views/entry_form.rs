use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table},
    Frame,
};

use crate::domain::Account;
use chrono::NaiveDate;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryFormResult {
    None,
    Cancel,
    Submit(NewEntryData),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntryData {
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub lines: Vec<EntryLineData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryLineData {
    pub account_id: String,
    pub account_display: String,
    pub amount: i64, // positive = debit, negative = credit
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    Date,
    Memo,
    Reference,
    Lines,
}

impl FormField {
    fn next(&self) -> Self {
        match self {
            FormField::Date => FormField::Memo,
            FormField::Memo => FormField::Reference,
            FormField::Reference => FormField::Lines,
            FormField::Lines => FormField::Date,
        }
    }

    fn prev(&self) -> Self {
        match self {
            FormField::Date => FormField::Lines,
            FormField::Memo => FormField::Date,
            FormField::Reference => FormField::Memo,
            FormField::Lines => FormField::Reference,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEditMode {
    Navigation,
    SelectAccount,
    EditAmount,
}

pub struct EntryForm {
    pub visible: bool,
    pub result: EntryFormResult,

    active_field: FormField,
    date_str: String,
    memo: String,
    reference: String,

    // Entry lines
    lines: Vec<EditableLine>,
    selected_line: usize,
    line_edit_mode: LineEditMode,

    // Account selection
    available_accounts: Vec<Account>,
    account_list_state: ListState,

    error_message: Option<String>,
}

#[derive(Debug, Clone)]
struct EditableLine {
    account_id: Option<String>,
    account_display: String,
    debit_str: String,
    credit_str: String,
    editing_debit: bool, // true = editing debit, false = editing credit
}

impl EditableLine {
    fn new() -> Self {
        Self {
            account_id: None,
            account_display: "(Select account)".to_string(),
            debit_str: String::new(),
            credit_str: String::new(),
            editing_debit: true,
        }
    }

    fn amount(&self) -> i64 {
        let debit = parse_amount(&self.debit_str);
        let credit = parse_amount(&self.credit_str);
        debit - credit // positive = debit, negative = credit
    }

    fn is_complete(&self) -> bool {
        self.account_id.is_some() && self.amount() != 0
    }
}

impl EntryForm {
    pub fn new() -> Self {
        let mut account_list_state = ListState::default();
        account_list_state.select(Some(0));

        Self {
            visible: false,
            result: EntryFormResult::None,
            active_field: FormField::Date,
            date_str: String::new(),
            memo: String::new(),
            reference: String::new(),
            lines: vec![EditableLine::new(), EditableLine::new()],
            selected_line: 0,
            line_edit_mode: LineEditMode::Navigation,
            available_accounts: Vec::new(),
            account_list_state,
            error_message: None,
        }
    }

    pub fn show(&mut self, existing_accounts: Vec<Account>) {
        self.show_with_account(existing_accounts, None);
    }

    pub fn show_with_account(
        &mut self,
        existing_accounts: Vec<Account>,
        preselected: Option<&Account>,
    ) {
        self.visible = true;
        self.result = EntryFormResult::None;
        self.active_field = FormField::Date;

        // Default to today's date
        let today = chrono::Local::now().date_naive();
        self.date_str = today.format("%Y-%m-%d").to_string();

        self.memo.clear();
        self.reference.clear();

        // Create lines with preselected account if provided
        let mut line1 = EditableLine::new();
        if let Some(account) = preselected {
            line1.account_id = Some(account.id.clone());
            line1.account_display = format!("{} - {}", account.account_number, account.name);
        }
        self.lines = vec![line1, EditableLine::new()];

        self.selected_line = 0;
        self.line_edit_mode = LineEditMode::Navigation;
        self.available_accounts = existing_accounts;
        self.error_message = None;
        self.account_list_state.select(Some(0));
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        self.handle_key_with_modifiers(key, KeyModifiers::empty());
    }

    pub fn handle_key_with_modifiers(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        // Ctrl+S or Ctrl+Enter always submits
        if modifiers.contains(KeyModifiers::CONTROL)
            && (key == KeyCode::Enter || key == KeyCode::Char('s'))
        {
            self.submit();
            return;
        }

        // Handle account selection dropdown
        if self.line_edit_mode == LineEditMode::SelectAccount {
            self.handle_account_selection_key(key);
            return;
        }

        // Handle amount editing
        if self.line_edit_mode == LineEditMode::EditAmount {
            self.handle_amount_edit_key(key);
            return;
        }

        match key {
            KeyCode::Esc => {
                self.result = EntryFormResult::Cancel;
            }
            KeyCode::Tab => {
                self.active_field = self.active_field.next();
            }
            KeyCode::BackTab => {
                self.active_field = self.active_field.prev();
            }
            KeyCode::Enter => {
                if self.active_field == FormField::Lines {
                    // Enter line edit mode - start with account selection
                    self.line_edit_mode = LineEditMode::SelectAccount;
                    self.account_list_state.select(Some(0));
                } else {
                    self.submit();
                }
            }
            KeyCode::Char('n') if self.active_field == FormField::Lines => {
                // Add new line
                self.lines.push(EditableLine::new());
                self.selected_line = self.lines.len() - 1;
            }
            KeyCode::Char('d') if self.active_field == FormField::Lines => {
                // Delete line (keep at least 2)
                if self.lines.len() > 2 {
                    self.lines.remove(self.selected_line);
                    if self.selected_line >= self.lines.len() {
                        self.selected_line = self.lines.len() - 1;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') if self.active_field == FormField::Lines => {
                if self.selected_line > 0 {
                    self.selected_line -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') if self.active_field == FormField::Lines => {
                if self.selected_line < self.lines.len() - 1 {
                    self.selected_line += 1;
                }
            }
            _ => {
                self.handle_field_input(key);
            }
        }
    }

    fn handle_account_selection_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.line_edit_mode = LineEditMode::Navigation;
            }
            KeyCode::Enter => {
                if let Some(i) = self.account_list_state.selected() {
                    if let Some(account) = self.available_accounts.get(i) {
                        self.lines[self.selected_line].account_id = Some(account.id.clone());
                        self.lines[self.selected_line].account_display =
                            format!("{} - {}", account.account_number, account.name);
                    }
                }
                // Move to amount editing
                self.line_edit_mode = LineEditMode::EditAmount;
                self.lines[self.selected_line].editing_debit = true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.account_list_state.selected().unwrap_or(0);
                let new_i = if i == 0 {
                    self.available_accounts.len().saturating_sub(1)
                } else {
                    i - 1
                };
                self.account_list_state.select(Some(new_i));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.account_list_state.selected().unwrap_or(0);
                let new_i = if i >= self.available_accounts.len().saturating_sub(1) {
                    0
                } else {
                    i + 1
                };
                self.account_list_state.select(Some(new_i));
            }
            _ => {}
        }
    }

    fn handle_amount_edit_key(&mut self, key: KeyCode) {
        let line = &mut self.lines[self.selected_line];

        match key {
            KeyCode::Esc => {
                self.line_edit_mode = LineEditMode::Navigation;
            }
            KeyCode::Enter => {
                self.line_edit_mode = LineEditMode::Navigation;
                self.error_message = None;
            }
            KeyCode::Tab => {
                // Toggle between debit and credit
                line.editing_debit = !line.editing_debit;
            }
            KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                let field = if line.editing_debit {
                    &mut line.debit_str
                } else {
                    &mut line.credit_str
                };
                field.push(c);

                // Clear the other field when entering a value
                if line.editing_debit {
                    line.credit_str.clear();
                } else {
                    line.debit_str.clear();
                }
            }
            KeyCode::Backspace => {
                let field = if line.editing_debit {
                    &mut line.debit_str
                } else {
                    &mut line.credit_str
                };
                field.pop();
            }
            _ => {}
        }
    }

    fn handle_field_input(&mut self, key: KeyCode) {
        let field = match self.active_field {
            FormField::Date => &mut self.date_str,
            FormField::Memo => &mut self.memo,
            FormField::Reference => &mut self.reference,
            FormField::Lines => return,
        };

        match key {
            KeyCode::Char(c) => {
                field.push(c);
                self.error_message = None;
            }
            KeyCode::Backspace => {
                field.pop();
                self.error_message = None;
            }
            _ => {}
        }
    }

    fn submit(&mut self) {
        // Validate date
        let date = match NaiveDate::parse_from_str(&self.date_str, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => {
                self.error_message = Some("Invalid date format (use YYYY-MM-DD)".to_string());
                self.active_field = FormField::Date;
                return;
            }
        };

        // Validate lines
        let complete_lines: Vec<_> = self.lines.iter().filter(|l| l.is_complete()).collect();

        if complete_lines.len() < 2 {
            self.error_message = Some("At least 2 complete lines required".to_string());
            self.active_field = FormField::Lines;
            return;
        }

        // Check balance
        let total: i64 = complete_lines.iter().map(|l| l.amount()).sum();
        if total != 0 {
            self.error_message = Some(format!(
                "Entry not balanced (off by {})",
                format_cents(total)
            ));
            self.active_field = FormField::Lines;
            return;
        }

        let reference = if self.reference.trim().is_empty() {
            None
        } else {
            Some(self.reference.trim().to_string())
        };

        let lines: Vec<EntryLineData> = complete_lines
            .iter()
            .filter_map(|l| {
                l.account_id.as_ref().map(|id| EntryLineData {
                    account_id: id.clone(),
                    account_display: l.account_display.clone(),
                    amount: l.amount(),
                })
            })
            .collect();

        self.result = EntryFormResult::Submit(NewEntryData {
            date,
            memo: self.memo.trim().to_string(),
            reference,
            lines,
        });
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        if !self.visible {
            return;
        }

        let modal_area = centered_rect(70, 80, area);
        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" New Journal Entry ")
            .title_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(block, modal_area);

        let inner = inner_rect(modal_area, 2, 1);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Date
                Constraint::Length(3), // Memo
                Constraint::Length(3), // Reference
                Constraint::Min(10),   // Lines table
                Constraint::Length(2), // Balance display
                Constraint::Length(2), // Error/Help
            ])
            .split(inner);

        // Date field
        self.draw_text_field(
            frame,
            chunks[0],
            "Date (YYYY-MM-DD)",
            &self.date_str,
            self.active_field == FormField::Date,
        );

        // Memo field
        self.draw_text_field(
            frame,
            chunks[1],
            "Memo (optional)",
            &self.memo,
            self.active_field == FormField::Memo,
        );

        // Reference field
        self.draw_text_field(
            frame,
            chunks[2],
            "Reference (optional)",
            &self.reference,
            self.active_field == FormField::Reference,
        );

        // Lines table
        self.draw_lines_table(frame, chunks[3]);

        // Balance display
        let total: i64 = self.lines.iter().map(|l| l.amount()).sum();
        let balance_style = if total == 0 {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };
        let balance_text = if total == 0 {
            "Balanced".to_string()
        } else if total > 0 {
            format!("Debits exceed by {}", format_cents(total))
        } else {
            format!("Credits exceed by {}", format_cents(-total))
        };
        let balance = Paragraph::new(Line::from(vec![
            Span::raw("  Balance: "),
            Span::styled(balance_text, balance_style),
        ]));
        frame.render_widget(balance, chunks[4]);

        // Error or help text
        let help_text = if let Some(ref err) = self.error_message {
            Line::from(Span::styled(err.clone(), Style::default().fg(Color::Red)))
        } else {
            Line::from(vec![
                Span::styled("Ctrl+S", Style::default().fg(Color::Yellow)),
                Span::raw(": save  "),
                Span::styled("Tab", Style::default().fg(Color::Yellow)),
                Span::raw(": next  "),
                Span::styled("n", Style::default().fg(Color::Yellow)),
                Span::raw(": new line  "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(": cancel"),
            ])
        };
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[5]);

        // Draw account dropdown if in selection mode
        if self.line_edit_mode == LineEditMode::SelectAccount {
            self.draw_account_dropdown(frame, chunks[3]);
        }
    }

    fn draw_text_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        label: &str,
        value: &str,
        is_active: bool,
    ) {
        let style = if is_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };

        let border_style = if is_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let display = if is_active {
            format!("{}|", value)
        } else {
            value.to_string()
        };

        let paragraph = Paragraph::new(display).style(style).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(" {} ", label)),
        );

        frame.render_widget(paragraph, area);
    }

    fn draw_lines_table(&self, frame: &mut Frame, area: Rect) {
        let is_active = self.active_field == FormField::Lines;

        let border_style = if is_active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let rows: Vec<Row> = self
            .lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let is_selected = i == self.selected_line && is_active;
                let is_editing = is_selected && self.line_edit_mode != LineEditMode::Navigation;

                let debit_display = if !line.debit_str.is_empty() {
                    format_cents(parse_amount(&line.debit_str))
                } else {
                    String::new()
                };

                let credit_display = if !line.credit_str.is_empty() {
                    format_cents(parse_amount(&line.credit_str))
                } else {
                    String::new()
                };

                let row_style = if is_selected {
                    Style::default().bg(Color::DarkGray)
                } else {
                    Style::default()
                };

                let account_cell =
                    if is_editing && self.line_edit_mode == LineEditMode::SelectAccount {
                        format!("{} <selecting>", line.account_display)
                    } else {
                        line.account_display.clone()
                    };

                let debit_cell = if is_editing
                    && self.line_edit_mode == LineEditMode::EditAmount
                    && line.editing_debit
                {
                    format!("{}|", line.debit_str)
                } else {
                    debit_display
                };

                let credit_cell = if is_editing
                    && self.line_edit_mode == LineEditMode::EditAmount
                    && !line.editing_debit
                {
                    format!("{}|", line.credit_str)
                } else {
                    credit_display
                };

                Row::new(vec![account_cell, debit_cell, credit_cell]).style(row_style)
            })
            .collect();

        let header = Row::new(vec!["Account", "Debit", "Credit"])
            .style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Yellow),
            )
            .bottom_margin(1);

        let table = Table::new(
            rows,
            [
                Constraint::Min(30),
                Constraint::Length(15),
                Constraint::Length(15),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(" Lines (j/k: nav, Enter: edit, n: new, d: delete) "),
        );

        frame.render_widget(table, area);
    }

    fn draw_account_dropdown(&self, frame: &mut Frame, anchor: Rect) {
        let items: Vec<ListItem> = self
            .available_accounts
            .iter()
            .map(|a| ListItem::new(format!("{} - {}", a.account_number, a.name)))
            .collect();

        if items.is_empty() {
            return;
        }

        let dropdown_height = (items.len() as u16 + 2).min(10);
        let dropdown_area = Rect {
            x: anchor.x + 1,
            y: anchor.y + 3 + (self.selected_line as u16).min(5),
            width: anchor.width.saturating_sub(2).min(50),
            height: dropdown_height,
        };

        frame.render_widget(Clear, dropdown_area);

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Select Account "),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, dropdown_area, &mut self.account_list_state.clone());
    }
}

impl Default for EntryForm {
    fn default() -> Self {
        Self::new()
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn inner_rect(area: Rect, margin_x: u16, margin_y: u16) -> Rect {
    Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: area.width.saturating_sub(margin_x * 2),
        height: area.height.saturating_sub(margin_y * 2),
    }
}

fn format_cents(cents: i64) -> String {
    let dollars = cents.abs() as f64 / 100.0;
    format!("{:.2}", dollars)
}

/// Parse a string amount (like "100.00" or "100") to cents
fn parse_amount(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }

    // Try parsing as a decimal (e.g., "100.50")
    if let Ok(f) = s.parse::<f64>() {
        return (f * 100.0).round() as i64;
    }

    // Try parsing as whole cents (e.g., "10050")
    s.parse().unwrap_or(0)
}
