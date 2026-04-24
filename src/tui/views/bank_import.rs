use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use super::csv_import::{parse_amount, parse_csv_line, parse_date};
use crate::domain::Account;
use crate::tui::theme::Theme;
use crate::tui::widgets;

/// A pending import from the bank sync
#[derive(Debug, Clone)]
pub struct PendingImport {
    pub id: i64,
    pub file_path: String,
    pub file_name: String,
    pub bank_id: Option<String>,
    pub bank_name: String,
    pub transaction_count: Option<i64>,
    pub created_at: String,
}

/// A parsed transaction from a bank CSV
#[derive(Debug, Clone)]
pub struct ParsedTransaction {
    pub date: chrono::NaiveDate,
    pub description: String,
    pub amount: i64, // In cents, positive = increase, negative = decrease
    pub selected: bool,
}

/// CSV preview for column mapping
#[derive(Debug, Clone)]
pub struct CsvPreview {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnField {
    Date,
    Description,
    Amount,
}

impl ColumnField {
    fn label(&self) -> &'static str {
        match self {
            ColumnField::Date => "Date",
            ColumnField::Description => "Description",
            ColumnField::Amount => "Amount",
        }
    }
}

/// Bank import modal state
pub struct BankImportModal {
    pub visible: bool,
    pub pending_imports: Vec<PendingImport>,
    pub selected_import_index: usize,

    // Import processing state
    pub processing_import: Option<PendingImport>,
    pub parsed_transactions: Vec<ParsedTransaction>,
    pub available_accounts: Vec<Account>,
    pub selected_account_index: usize,
    pub phase: ImportPhase,

    // Column mapping state
    pub csv_preview: Option<CsvPreview>,
    pub date_column: Option<usize>,
    pub description_column: Option<usize>,
    pub amount_column: Option<usize>,
    pub active_field: ColumnField,
    pub mapping_error: Option<String>,

    // Existing bank-account mappings
    pub bank_account_mappings: std::collections::HashMap<String, String>, // bank_id -> account_id
}

#[derive(Debug, Clone, PartialEq)]
pub enum ImportPhase {
    SelectImport,  // Choosing which pending import to process
    SelectAccount, // Choosing which account to import to
    MapColumns,    // Configure column mapping
    Preview,       // Previewing transactions before import
    Complete,      // Import finished
}

pub enum BankImportResult {
    None,
    Cancel,
    Import {
        import_id: i64,
        account_id: String,
        save_mapping: bool,
        transactions: Vec<ParsedTransaction>,
    },
    Skip(i64), // Skip/delete this import
}

impl Default for BankImportModal {
    fn default() -> Self {
        Self::new()
    }
}

impl BankImportModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            pending_imports: Vec::new(),
            selected_import_index: 0,
            processing_import: None,
            parsed_transactions: Vec::new(),
            available_accounts: Vec::new(),
            selected_account_index: 0,
            phase: ImportPhase::SelectImport,
            csv_preview: None,
            date_column: None,
            description_column: None,
            amount_column: None,
            active_field: ColumnField::Date,
            mapping_error: None,
            bank_account_mappings: std::collections::HashMap::new(),
        }
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.phase = ImportPhase::SelectImport;
        self.selected_import_index = 0;
        self.reset_mapping();
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.processing_import = None;
        self.parsed_transactions.clear();
        self.phase = ImportPhase::SelectImport;
        self.reset_mapping();
    }

    fn reset_mapping(&mut self) {
        self.csv_preview = None;
        self.date_column = None;
        self.description_column = None;
        self.amount_column = None;
        self.active_field = ColumnField::Date;
        self.mapping_error = None;
    }

    pub fn set_pending_imports(&mut self, imports: Vec<PendingImport>) {
        self.pending_imports = imports;
        if self.selected_import_index >= self.pending_imports.len() {
            self.selected_import_index = self.pending_imports.len().saturating_sub(1);
        }
    }

    pub fn set_accounts(&mut self, accounts: Vec<Account>) {
        self.available_accounts = accounts;
    }

    pub fn set_mappings(&mut self, mappings: std::collections::HashMap<String, String>) {
        self.bank_account_mappings = mappings;
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyCode) -> BankImportResult {
        match self.phase {
            ImportPhase::SelectImport => self.handle_select_import_key(key),
            ImportPhase::SelectAccount => self.handle_select_account_key(key),
            ImportPhase::MapColumns => self.handle_map_columns_key(key),
            ImportPhase::Preview => self.handle_preview_key(key),
            ImportPhase::Complete => {
                self.hide();
                BankImportResult::None
            }
        }
    }

    fn handle_select_import_key(&mut self, key: crossterm::event::KeyCode) -> BankImportResult {
        use crossterm::event::KeyCode;
        match key {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.hide();
                BankImportResult::Cancel
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_import_index > 0 {
                    self.selected_import_index -= 1;
                }
                BankImportResult::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_import_index + 1 < self.pending_imports.len() {
                    self.selected_import_index += 1;
                }
                BankImportResult::None
            }
            KeyCode::Enter => {
                if let Some(import) = self
                    .pending_imports
                    .get(self.selected_import_index)
                    .cloned()
                {
                    self.processing_import = Some(import.clone());

                    // Check if we have a mapping for this bank
                    if let Some(bank_id) = &import.bank_id {
                        if let Some(account_id) = self.bank_account_mappings.get(bank_id) {
                            // Find the account index
                            if let Some(idx) = self
                                .available_accounts
                                .iter()
                                .position(|a| &a.id == account_id)
                            {
                                self.selected_account_index = idx;
                            }
                        }
                    }

                    self.phase = ImportPhase::SelectAccount;
                }
                BankImportResult::None
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                // Skip/delete this import
                if let Some(import) = self.pending_imports.get(self.selected_import_index) {
                    let id = import.id;
                    return BankImportResult::Skip(id);
                }
                BankImportResult::None
            }
            _ => BankImportResult::None,
        }
    }

    fn handle_select_account_key(&mut self, key: crossterm::event::KeyCode) -> BankImportResult {
        use crossterm::event::KeyCode;
        match key {
            KeyCode::Esc => {
                self.phase = ImportPhase::SelectImport;
                self.processing_import = None;
                BankImportResult::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_account_index > 0 {
                    self.selected_account_index -= 1;
                }
                BankImportResult::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_account_index + 1 < self.available_accounts.len() {
                    self.selected_account_index += 1;
                }
                BankImportResult::None
            }
            KeyCode::Enter => {
                // Load CSV preview and move to column mapping
                let file_path = self.processing_import.as_ref().map(|i| i.file_path.clone());
                if let Some(path) = file_path {
                    self.load_csv_preview(&path);
                }
                self.phase = ImportPhase::MapColumns;
                BankImportResult::None
            }
            _ => BankImportResult::None,
        }
    }

    fn handle_map_columns_key(&mut self, key: crossterm::event::KeyCode) -> BankImportResult {
        use crossterm::event::KeyCode;
        let num_columns = self
            .csv_preview
            .as_ref()
            .map(|p| p.headers.len())
            .unwrap_or(0);

        match key {
            KeyCode::Esc => {
                self.phase = ImportPhase::SelectAccount;
                self.reset_mapping();
                BankImportResult::None
            }
            KeyCode::Enter => {
                if self.date_column.is_some()
                    && self.description_column.is_some()
                    && self.amount_column.is_some()
                {
                    // Parse transactions and move to preview
                    self.parse_transactions();
                    if !self.parsed_transactions.is_empty() {
                        self.phase = ImportPhase::Preview;
                    } else {
                        self.mapping_error = Some("No valid transactions found".to_string());
                    }
                } else {
                    self.mapping_error = Some("Please map all required columns".to_string());
                }
                BankImportResult::None
            }
            KeyCode::Tab | KeyCode::Down => {
                self.active_field = match self.active_field {
                    ColumnField::Date => ColumnField::Description,
                    ColumnField::Description => ColumnField::Amount,
                    ColumnField::Amount => ColumnField::Date,
                };
                BankImportResult::None
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.active_field = match self.active_field {
                    ColumnField::Date => ColumnField::Amount,
                    ColumnField::Description => ColumnField::Date,
                    ColumnField::Amount => ColumnField::Description,
                };
                BankImportResult::None
            }
            KeyCode::Left => {
                let current = match self.active_field {
                    ColumnField::Date => &mut self.date_column,
                    ColumnField::Description => &mut self.description_column,
                    ColumnField::Amount => &mut self.amount_column,
                };
                if let Some(idx) = current {
                    if *idx > 0 {
                        *idx -= 1;
                    }
                } else if num_columns > 0 {
                    *current = Some(num_columns - 1);
                }
                BankImportResult::None
            }
            KeyCode::Right => {
                let current = match self.active_field {
                    ColumnField::Date => &mut self.date_column,
                    ColumnField::Description => &mut self.description_column,
                    ColumnField::Amount => &mut self.amount_column,
                };
                if let Some(idx) = current {
                    if *idx < num_columns - 1 {
                        *idx += 1;
                    }
                } else if num_columns > 0 {
                    *current = Some(0);
                }
                BankImportResult::None
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                let num = c.to_digit(10).unwrap() as usize;
                if num > 0 && num <= num_columns {
                    let col_idx = num - 1;
                    match self.active_field {
                        ColumnField::Date => self.date_column = Some(col_idx),
                        ColumnField::Description => self.description_column = Some(col_idx),
                        ColumnField::Amount => self.amount_column = Some(col_idx),
                    }
                }
                BankImportResult::None
            }
            _ => BankImportResult::None,
        }
    }

    fn handle_preview_key(&mut self, key: crossterm::event::KeyCode) -> BankImportResult {
        use crossterm::event::KeyCode;
        match key {
            KeyCode::Esc => {
                self.phase = ImportPhase::MapColumns;
                self.parsed_transactions.clear();
                BankImportResult::None
            }
            KeyCode::Enter | KeyCode::Char('i') => {
                // Confirm import
                if let (Some(import), Some(account)) = (
                    self.processing_import.as_ref(),
                    self.available_accounts.get(self.selected_account_index),
                ) {
                    let result = BankImportResult::Import {
                        import_id: import.id,
                        account_id: account.id.clone(),
                        save_mapping: true, // Always save mapping on first import
                        transactions: self.parsed_transactions.clone(),
                    };
                    self.phase = ImportPhase::Complete;
                    return result;
                }
                BankImportResult::None
            }
            KeyCode::Char(' ') => {
                // Toggle transaction selection (for future use)
                BankImportResult::None
            }
            _ => BankImportResult::None,
        }
    }

    fn load_csv_preview(&mut self, file_path: &str) {
        self.reset_mapping();

        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(e) => {
                self.mapping_error = Some(format!("Failed to read file: {}", e));
                return;
            }
        };

        let mut lines = content.lines();

        // Parse header
        let headers: Vec<String> = match lines.next() {
            Some(line) => parse_csv_line(line),
            None => {
                self.mapping_error = Some("Empty file".to_string());
                return;
            }
        };

        if headers.is_empty() {
            self.mapping_error = Some("No columns found".to_string());
            return;
        }

        // Parse first few rows for preview
        let rows: Vec<Vec<String>> = lines.take(5).map(parse_csv_line).collect();

        self.csv_preview = Some(CsvPreview { headers, rows });

        // Try to auto-detect columns
        self.auto_detect_columns();
    }

    fn auto_detect_columns(&mut self) {
        if let Some(ref preview) = self.csv_preview {
            for (idx, header) in preview.headers.iter().enumerate() {
                let lower = header.to_lowercase();

                if self.date_column.is_none()
                    && (lower.contains("date") || lower.contains("time") || lower == "posting date")
                {
                    self.date_column = Some(idx);
                }

                if self.description_column.is_none()
                    && (lower.contains("desc")
                        || lower.contains("memo")
                        || lower.contains("narr")
                        || lower.contains("detail")
                        || lower.contains("payee"))
                {
                    self.description_column = Some(idx);
                }

                if self.amount_column.is_none()
                    && (lower == "amount"
                        || lower.contains("value")
                        || lower.contains("sum")
                        || lower.contains("total"))
                {
                    self.amount_column = Some(idx);
                }
            }

            // If no date found by header, try to find it by parsing first data row
            if self.date_column.is_none() {
                if let Some(first_row) = preview.rows.first() {
                    for (i, field) in first_row.iter().enumerate() {
                        if parse_date(field).is_some() {
                            self.date_column = Some(i);
                            break;
                        }
                    }
                }
            }
        }
    }

    fn parse_transactions(&mut self) {
        self.parsed_transactions.clear();

        let Some(ref import) = self.processing_import else {
            return;
        };
        let Some(date_col) = self.date_column else {
            return;
        };
        let Some(desc_col) = self.description_column else {
            return;
        };
        let Some(amount_col) = self.amount_column else {
            return;
        };

        let content = match std::fs::read_to_string(&import.file_path) {
            Ok(c) => c,
            Err(_) => return,
        };

        let mut lines = content.lines();
        lines.next(); // Skip header

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }

            let fields = parse_csv_line(line);

            let date = match fields.get(date_col).and_then(|s| parse_date(s)) {
                Some(d) => d,
                None => continue,
            };

            let description = fields.get(desc_col).cloned().unwrap_or_default();

            let amount = match fields.get(amount_col).and_then(|s| parse_amount(s)) {
                Some(a) if a != 0 => a,
                _ => continue,
            };

            self.parsed_transactions.push(ParsedTransaction {
                date,
                description,
                amount,
                selected: true,
            });
        }
    }

    pub fn get_selected_account(&self) -> Option<&Account> {
        self.available_accounts.get(self.selected_account_index)
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        // Center the modal - make it larger for column mapping
        let modal_width = 80.min(area.width.saturating_sub(4));
        let modal_height = 24.min(area.height.saturating_sub(4));
        let modal_x = (area.width.saturating_sub(modal_width)) / 2;
        let modal_y = (area.height.saturating_sub(modal_height)) / 2;

        let modal_area = Rect {
            x: modal_x,
            y: modal_y,
            width: modal_width,
            height: modal_height,
        };

        frame.render_widget(Clear, modal_area);

        match self.phase {
            ImportPhase::SelectImport => self.draw_select_import(frame, modal_area, theme),
            ImportPhase::SelectAccount => self.draw_select_account(frame, modal_area, theme),
            ImportPhase::MapColumns => self.draw_map_columns(frame, modal_area, theme),
            ImportPhase::Preview => self.draw_preview(frame, modal_area, theme),
            ImportPhase::Complete => self.draw_complete(frame, modal_area, theme),
        }
    }

    fn draw_select_import(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let title = format!(" Bank Imports ({}) ", self.pending_imports.len());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if self.pending_imports.is_empty() {
            let msg = Paragraph::new("No pending imports").style(Style::default().fg(theme.fg_dim));
            frame.render_widget(msg, inner);
            return;
        }

        let items: Vec<ListItem> = self
            .pending_imports
            .iter()
            .enumerate()
            .map(|(i, import)| {
                let count = import
                    .transaction_count
                    .map(|c| format!(" ({} txns)", c))
                    .unwrap_or_default();
                let content = Line::from(vec![
                    Span::styled(
                        &import.bank_name,
                        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(count),
                    Span::raw(" - "),
                    Span::styled(&import.file_name, Style::default().fg(theme.fg_dim)),
                ]);
                let style = if i == self.selected_import_index {
                    theme.selected_style()
                } else {
                    Style::default()
                };
                ListItem::new(content).style(style)
            })
            .collect();

        let list = List::new(items);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);

        frame.render_widget(list, chunks[0]);

        let help = Paragraph::new(Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme.success)),
            Span::raw(" select  "),
            Span::styled("d", Style::default().fg(theme.error)),
            Span::raw(" skip  "),
            Span::styled("Esc", Style::default().fg(theme.header)),
            Span::raw(" close"),
        ]));
        frame.render_widget(help, chunks[1]);
    }

    fn draw_select_account(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let import_name = self
            .processing_import
            .as_ref()
            .map(|i| i.bank_name.as_str())
            .unwrap_or("Import");
        let title = format!(" Select Account for {} ", import_name);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let items: Vec<ListItem> = self
            .available_accounts
            .iter()
            .enumerate()
            .map(|(i, account)| {
                let content = Line::from(vec![
                    Span::styled(
                        format!("{} ", account.account_number),
                        Style::default().fg(theme.header),
                    ),
                    Span::raw(&account.name),
                    Span::styled(
                        format!(" ({})", account.account_type),
                        Style::default().fg(theme.fg_dim),
                    ),
                ]);
                let style = if i == self.selected_account_index {
                    theme.selected_style()
                } else {
                    Style::default()
                };
                ListItem::new(content).style(style)
            })
            .collect();

        let list = List::new(items);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);

        frame.render_widget(list, chunks[0]);

        let help = Paragraph::new(Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme.success)),
            Span::raw(" select  "),
            Span::styled("Esc", Style::default().fg(theme.header)),
            Span::raw(" back"),
        ]));
        frame.render_widget(help, chunks[1]);
    }

    fn draw_map_columns(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Map Columns ")
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8), // CSV columns list
                Constraint::Length(6), // Column mapping
                Constraint::Min(3),    // Sample data
                Constraint::Length(2), // Help
            ])
            .split(inner);

        if let Some(ref preview) = self.csv_preview {
            // CSV columns list
            let mut header_lines = vec![
                Line::from(Span::styled(
                    "CSV Columns:",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];

            for (idx, header) in preview.headers.iter().enumerate() {
                let marker = if Some(idx) == self.date_column {
                    " [Date]"
                } else if Some(idx) == self.description_column {
                    " [Desc]"
                } else if Some(idx) == self.amount_column {
                    " [Amt]"
                } else {
                    ""
                };
                let display_header = if header.len() > 30 {
                    format!("{}...", &header[..27])
                } else {
                    header.clone()
                };
                header_lines.push(Line::from(format!(
                    "  {}: {}{}",
                    idx + 1,
                    display_header,
                    marker
                )));
            }

            let headers_widget = Paragraph::new(header_lines);
            frame.render_widget(headers_widget, chunks[0]);

            // Column mapping
            let fields = [
                (ColumnField::Date, self.date_column),
                (ColumnField::Description, self.description_column),
                (ColumnField::Amount, self.amount_column),
            ];

            let mut mapping_lines = vec![
                Line::from(Span::styled(
                    "Map columns (use number keys or ←→):",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ];

            for (field, selected) in fields {
                let is_active = field == self.active_field;
                let col_name = selected
                    .and_then(|i| preview.headers.get(i))
                    .map(|s| {
                        if s.len() > 25 {
                            format!("{}...", &s[..22])
                        } else {
                            s.clone()
                        }
                    })
                    .unwrap_or_else(|| "(not set)".to_string());

                let style = if is_active {
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };

                let marker = if is_active { "► " } else { "  " };
                mapping_lines.push(Line::from(Span::styled(
                    format!("{}{}: {}", marker, field.label(), col_name),
                    style,
                )));
            }

            let mapping_widget = Paragraph::new(mapping_lines);
            frame.render_widget(mapping_widget, chunks[1]);

            // Sample data
            if let Some(first_row) = preview.rows.first() {
                let mut sample_lines = vec![Line::from(Span::styled(
                    "Sample row:",
                    Style::default().add_modifier(Modifier::BOLD),
                ))];

                if let Some(idx) = self.date_column {
                    let val = first_row.get(idx).map(|s| s.as_str()).unwrap_or("N/A");
                    sample_lines.push(Line::from(format!("  Date: {}", widgets::truncate(val, 40))));
                }
                if let Some(idx) = self.description_column {
                    let val = first_row.get(idx).map(|s| s.as_str()).unwrap_or("N/A");
                    sample_lines.push(Line::from(format!(
                        "  Description: {}",
                        widgets::truncate(val, 40)
                    )));
                }
                if let Some(idx) = self.amount_column {
                    let val = first_row.get(idx).map(|s| s.as_str()).unwrap_or("N/A");
                    sample_lines.push(Line::from(format!("  Amount: {}", val)));
                }

                let sample_widget = Paragraph::new(sample_lines);
                frame.render_widget(sample_widget, chunks[2]);
            }
        }

        // Error or help
        let help_text = if let Some(ref err) = self.mapping_error {
            Line::from(Span::styled(err, Style::default().fg(theme.error)))
        } else {
            Line::from(vec![
                Span::styled("↑↓/Tab", Style::default().fg(theme.header)),
                Span::raw(": field  "),
                Span::styled("←→/1-9", Style::default().fg(theme.header)),
                Span::raw(": column  "),
                Span::styled("Enter", Style::default().fg(theme.header)),
                Span::raw(": continue  "),
                Span::styled("Esc", Style::default().fg(theme.header)),
                Span::raw(": back"),
            ])
        };
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[3]);
    }

    fn draw_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let account_name = self
            .get_selected_account()
            .map(|a| a.name.as_str())
            .unwrap_or("Unknown");
        let title = format!(" Preview Import to {} ", account_name);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let items: Vec<ListItem> = self
            .parsed_transactions
            .iter()
            .take(inner.height.saturating_sub(3) as usize)
            .map(|txn| {
                let amount_str = format!("{:.2}", txn.amount as f64 / 100.0);
                let amount_color = if txn.amount >= 0 {
                    theme.success
                } else {
                    theme.error
                };
                let content = Line::from(vec![
                    Span::styled(
                        format!("{} ", txn.date.format("%m/%d")),
                        Style::default().fg(theme.fg_dim),
                    ),
                    Span::raw(widgets::truncate(&txn.description, 40)),
                    Span::raw(" "),
                    Span::styled(amount_str, Style::default().fg(amount_color)),
                ]);
                ListItem::new(content)
            })
            .collect();

        let more_msg = if self.parsed_transactions.len() > items.len() {
            format!(
                "... and {} more",
                self.parsed_transactions.len() - items.len()
            )
        } else {
            String::new()
        };

        let list = List::new(items);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(2),
            ])
            .split(inner);

        frame.render_widget(list, chunks[0]);

        if !more_msg.is_empty() {
            let more = Paragraph::new(more_msg).style(Style::default().fg(theme.fg_dim));
            frame.render_widget(more, chunks[1]);
        }

        let total: i64 = self.parsed_transactions.iter().map(|t| t.amount).sum();
        let help = Paragraph::new(Line::from(vec![
            Span::styled("Enter/i", Style::default().fg(theme.success)),
            Span::raw(format!(
                " import {} txns (net: {:.2})  ",
                self.parsed_transactions.len(),
                total as f64 / 100.0
            )),
            Span::styled("Esc", Style::default().fg(theme.header)),
            Span::raw(" back"),
        ]));
        frame.render_widget(help, chunks[2]);
    }

    fn draw_complete(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.success))
            .title(" Import Complete ")
            .title_style(
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let msg =
            Paragraph::new("Transactions imported successfully!\n\nPress any key to continue.")
                .style(Style::default().fg(theme.fg));
        frame.render_widget(msg, inner);
    }
}

