use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};
use std::path::PathBuf;

use crate::tui::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStep {
    SelectFile,
    Parsing,
    SelectAccount,
    MapColumns,
    Confirm,
}

/// Which parse-options field is currently active on the Parsing step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseField {
    SkipLines,
    HasHeader,
    Delimiter,
}

/// Delimiter options the user can cycle through on the Parsing step.
const DELIMITER_CHOICES: &[(char, &str)] = &[
    (',', "Comma  ,"),
    ('\t', "Tab    ⇥"),
    (';', "Semicolon ;"),
    ('|', "Pipe   |"),
];

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

/// Account info for selection list
#[derive(Debug, Clone)]
pub struct AccountChoice {
    pub id: String,
    pub name: String,
    pub account_number: String,
    pub account_type: String,
}

pub struct CsvImportModal {
    pub visible: bool,
    pub step: ImportStep,

    // File selection
    pub file_path: String,
    pub file_suggestions: Vec<PathBuf>,
    pub suggestion_state: ListState,

    // Account selection
    pub available_accounts: Vec<AccountChoice>,
    pub account_filter: String,
    pub account_state: ListState,

    // CSV preview
    pub preview: Option<CsvPreview>,
    pub error_message: Option<String>,

    // Raw file content (cached so we can re-parse when parse options change)
    pub raw_content: Option<String>,
    // Number of leading lines to skip before the header (or first data) row
    pub skip_lines: usize,
    // Whether the first non-skipped row is a header row
    pub has_header: bool,
    // Field delimiter used to split each row into columns
    pub delimiter: char,
    // Currently active field on the Parsing step
    pub active_parse_field: ParseField,

    // Column mapping
    pub date_column: Option<usize>,
    pub description_column: Option<usize>,
    pub amount_column: Option<usize>,
    pub active_field: ColumnField,

    // Target account for import
    pub target_account_id: Option<String>,
    pub target_account_name: Option<String>,

    // Ready to import flag
    pub ready_to_import: bool,
}

impl CsvImportModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            step: ImportStep::SelectFile,
            file_path: String::new(),
            file_suggestions: Vec::new(),
            suggestion_state: ListState::default(),
            available_accounts: Vec::new(),
            account_filter: String::new(),
            account_state: ListState::default(),
            preview: None,
            error_message: None,
            raw_content: None,
            skip_lines: 0,
            has_header: true,
            delimiter: ',',
            active_parse_field: ParseField::SkipLines,
            date_column: None,
            description_column: None,
            amount_column: None,
            active_field: ColumnField::Date,
            target_account_id: None,
            target_account_name: None,
            ready_to_import: false,
        }
    }

    pub fn show(&mut self, accounts: Vec<AccountChoice>) {
        self.visible = true;
        self.step = ImportStep::SelectFile;
        self.file_path.clear();
        self.file_suggestions.clear();
        self.suggestion_state = ListState::default();
        self.available_accounts = accounts;
        self.account_filter.clear();
        self.account_state = ListState::default();
        self.preview = None;
        self.error_message = None;
        self.raw_content = None;
        self.skip_lines = 0;
        self.has_header = true;
        self.delimiter = ',';
        self.active_parse_field = ParseField::SkipLines;
        self.date_column = None;
        self.description_column = None;
        self.amount_column = None;
        self.active_field = ColumnField::Date;
        self.target_account_id = None;
        self.target_account_name = None;
        self.ready_to_import = false;
        self.update_suggestions();
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.ready_to_import = false;
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        match self.step {
            ImportStep::SelectFile => self.handle_file_select_key(key),
            ImportStep::Parsing => self.handle_parsing_key(key),
            ImportStep::SelectAccount => self.handle_account_select_key(key),
            ImportStep::MapColumns => self.handle_map_columns_key(key),
            ImportStep::Confirm => self.handle_confirm_key(key),
        }
    }

    fn handle_file_select_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.hide();
            }
            KeyCode::Enter => {
                if self.suggestion_state.selected().is_some() {
                    // Use selected suggestion
                    if let Some(idx) = self.suggestion_state.selected() {
                        if let Some(path) = self.file_suggestions.get(idx) {
                            if path.is_dir() {
                                // Navigate into directory
                                self.file_path = path.to_string_lossy().to_string();
                                if !self.file_path.ends_with('/') {
                                    self.file_path.push('/');
                                }
                                self.update_suggestions();
                                self.suggestion_state.select(None);
                            } else {
                                // Select file
                                self.file_path = path.to_string_lossy().to_string();
                                self.load_csv_preview();
                            }
                        }
                    }
                } else if !self.file_path.is_empty() {
                    self.load_csv_preview();
                }
            }
            KeyCode::Tab => {
                // Autocomplete with first suggestion
                if let Some(first) = self.file_suggestions.first() {
                    self.file_path = first.to_string_lossy().to_string();
                    if first.is_dir() && !self.file_path.ends_with('/') {
                        self.file_path.push('/');
                    }
                    self.update_suggestions();
                }
            }
            KeyCode::Up => {
                if !self.file_suggestions.is_empty() {
                    let i = match self.suggestion_state.selected() {
                        Some(i) => {
                            if i == 0 {
                                self.file_suggestions.len() - 1
                            } else {
                                i - 1
                            }
                        }
                        None => self.file_suggestions.len() - 1,
                    };
                    self.suggestion_state.select(Some(i));
                }
            }
            KeyCode::Down => {
                if !self.file_suggestions.is_empty() {
                    let i = match self.suggestion_state.selected() {
                        Some(i) => {
                            if i >= self.file_suggestions.len() - 1 {
                                0
                            } else {
                                i + 1
                            }
                        }
                        None => 0,
                    };
                    self.suggestion_state.select(Some(i));
                }
            }
            KeyCode::Backspace => {
                self.file_path.pop();
                self.update_suggestions();
                self.suggestion_state = ListState::default();
            }
            KeyCode::Char(c) => {
                self.file_path.push(c);
                self.update_suggestions();
                self.suggestion_state = ListState::default();
            }
            _ => {}
        }
    }

    fn handle_account_select_key(&mut self, key: KeyCode) {
        let filtered = self.filtered_accounts();
        let num_accounts = filtered.len();

        match key {
            KeyCode::Esc => {
                self.step = ImportStep::Parsing;
                self.error_message = None;
            }
            KeyCode::Enter => {
                if let Some(idx) = self.account_state.selected() {
                    if let Some(account) = filtered.get(idx) {
                        let id = account.id.clone();
                        let name = format!("{} - {}", account.account_number, account.name);
                        self.target_account_id = Some(id);
                        self.target_account_name = Some(name);
                        self.step = ImportStep::MapColumns;
                    }
                }
            }
            KeyCode::Up => {
                if num_accounts > 0 {
                    let i = match self.account_state.selected() {
                        Some(i) if i > 0 => i - 1,
                        _ => num_accounts.saturating_sub(1),
                    };
                    self.account_state.select(Some(i));
                }
            }
            KeyCode::Down => {
                if num_accounts > 0 {
                    let i = match self.account_state.selected() {
                        Some(i) if i < num_accounts - 1 => i + 1,
                        _ => 0,
                    };
                    self.account_state.select(Some(i));
                }
            }
            KeyCode::Backspace => {
                self.account_filter.pop();
                self.account_state.select(Some(0));
            }
            KeyCode::Char(c) => {
                self.account_filter.push(c);
                self.account_state.select(Some(0));
            }
            _ => {}
        }
    }

    fn filtered_accounts(&self) -> Vec<&AccountChoice> {
        let filter_lower = self.account_filter.to_lowercase();
        self.available_accounts
            .iter()
            .filter(|a| {
                filter_lower.is_empty()
                    || a.name.to_lowercase().contains(&filter_lower)
                    || a.account_number.contains(&filter_lower)
            })
            .collect()
    }

    fn handle_map_columns_key(&mut self, key: KeyCode) {
        let num_columns = self.preview.as_ref().map(|p| p.headers.len()).unwrap_or(0);

        match key {
            KeyCode::Esc => {
                self.step = ImportStep::SelectAccount;
                self.error_message = None;
            }
            KeyCode::Enter => {
                if self.date_column.is_some()
                    && self.description_column.is_some()
                    && self.amount_column.is_some()
                {
                    self.step = ImportStep::Confirm;
                } else {
                    self.error_message = Some("Please map all required columns".to_string());
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                self.active_field = match self.active_field {
                    ColumnField::Date => ColumnField::Description,
                    ColumnField::Description => ColumnField::Amount,
                    ColumnField::Amount => ColumnField::Date,
                };
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.active_field = match self.active_field {
                    ColumnField::Date => ColumnField::Amount,
                    ColumnField::Description => ColumnField::Date,
                    ColumnField::Amount => ColumnField::Description,
                };
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
            }
            _ => {}
        }
    }

    fn handle_confirm_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.step = ImportStep::MapColumns;
            }
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.ready_to_import = true;
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.step = ImportStep::MapColumns;
            }
            _ => {}
        }
    }

    fn update_suggestions(&mut self) {
        self.file_suggestions.clear();

        let path = if self.file_path.is_empty() {
            PathBuf::from(".")
        } else {
            expand_tilde(&self.file_path)
        };

        let (dir, prefix) = if path.is_dir() {
            (path.clone(), String::new())
        } else {
            let parent = path.parent().unwrap_or(std::path::Path::new("."));
            let prefix = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            (parent.to_path_buf(), prefix)
        };

        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut suggestions: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    let prefix_lower = prefix.to_lowercase();

                    name.starts_with(&prefix_lower)
                })
                .collect();

            suggestions.sort();
            self.file_suggestions = suggestions;
        }
    }

    fn load_csv_preview(&mut self) {
        let path = expand_tilde(&self.file_path);

        if !path.exists() {
            self.error_message = Some("File not found".to_string());
            return;
        }

        if !path.is_file() {
            self.error_message = Some("Not a file".to_string());
            return;
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.raw_content = Some(content);
                self.skip_lines = 0;
                self.has_header = true;
                self.delimiter = self.guess_delimiter();
                self.active_parse_field = ParseField::SkipLines;
                self.recompute_preview();

                if self.error_message.is_some() {
                    return;
                }

                // Move on to the parsing-options step where the user can adjust
                // skip count, header detection, and delimiter.
                self.step = ImportStep::Parsing;
            }
            Err(e) => {
                self.error_message = Some(format!("Failed to read file: {}", e));
            }
        }
    }

    /// Best-effort guess at the delimiter by counting field counts on the first
    /// non-empty line for each candidate. Defaults to comma on a tie.
    fn guess_delimiter(&self) -> char {
        let Some(ref content) = self.raw_content else {
            return ',';
        };
        let Some(line) = content.lines().find(|l| !l.trim().is_empty()) else {
            return ',';
        };

        let mut best = (',', 0usize);
        for (delim, _) in DELIMITER_CHOICES {
            let count = parse_delimited_line(line, *delim).len();
            if count > best.1 {
                best = (*delim, count);
            }
        }
        best.0
    }

    /// Re-parse the cached file content using the current parse options and
    /// refresh the preview / auto-detected column mapping.
    fn recompute_preview(&mut self) {
        let Some(ref content) = self.raw_content else {
            return;
        };

        let mut lines = content.lines();
        for _ in 0..self.skip_lines {
            if lines.next().is_none() {
                self.preview = None;
                self.error_message =
                    Some("Skip count exceeds the number of lines in the file".to_string());
                return;
            }
        }

        let headers: Vec<String>;
        let rows: Vec<Vec<String>>;

        if self.has_header {
            headers = match lines.next() {
                Some(line) => parse_delimited_line(line, self.delimiter),
                None => {
                    self.preview = None;
                    self.error_message = Some("No header row after skipped lines".to_string());
                    return;
                }
            };

            if headers.is_empty() {
                self.preview = None;
                self.error_message = Some("No columns found".to_string());
                return;
            }

            rows = lines
                .take(5)
                .map(|l| parse_delimited_line(l, self.delimiter))
                .collect();
        } else {
            // No header row — generate synthetic column names from the widest
            // of the first few data rows so the user has something to map to.
            rows = lines
                .take(5)
                .map(|l| parse_delimited_line(l, self.delimiter))
                .collect();

            if rows.is_empty() {
                self.preview = None;
                self.error_message = Some("No data rows after skipped lines".to_string());
                return;
            }

            let column_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
            if column_count == 0 {
                self.preview = None;
                self.error_message = Some("No columns found".to_string());
                return;
            }

            headers = (1..=column_count).map(|i| format!("Column {}", i)).collect();
        }

        self.preview = Some(CsvPreview { headers, rows });
        self.error_message = None;

        // Re-run auto-detection — headers / column count may have changed.
        // Auto-detection only meaningfully works on real headers; for synthetic
        // ones it'll be a no-op, which is fine.
        self.date_column = None;
        self.description_column = None;
        self.amount_column = None;
        self.auto_detect_columns();
    }

    fn handle_parsing_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.step = ImportStep::SelectFile;
                self.error_message = None;
            }
            KeyCode::Enter => {
                if self.preview.is_some() {
                    self.step = ImportStep::SelectAccount;
                    self.account_state.select(Some(0));
                    self.error_message = None;
                }
            }
            KeyCode::Tab | KeyCode::Down => {
                self.active_parse_field = match self.active_parse_field {
                    ParseField::SkipLines => ParseField::HasHeader,
                    ParseField::HasHeader => ParseField::Delimiter,
                    ParseField::Delimiter => ParseField::SkipLines,
                };
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.active_parse_field = match self.active_parse_field {
                    ParseField::SkipLines => ParseField::Delimiter,
                    ParseField::HasHeader => ParseField::SkipLines,
                    ParseField::Delimiter => ParseField::HasHeader,
                };
            }
            KeyCode::Left => match self.active_parse_field {
                ParseField::SkipLines => {
                    self.skip_lines = self.skip_lines.saturating_sub(1);
                    self.recompute_preview();
                }
                ParseField::HasHeader => {
                    self.has_header = !self.has_header;
                    self.recompute_preview();
                }
                ParseField::Delimiter => {
                    self.cycle_delimiter(-1);
                    self.recompute_preview();
                }
            },
            KeyCode::Right => match self.active_parse_field {
                ParseField::SkipLines => {
                    self.skip_lines = self.skip_lines.saturating_add(1);
                    self.recompute_preview();
                }
                ParseField::HasHeader => {
                    self.has_header = !self.has_header;
                    self.recompute_preview();
                }
                ParseField::Delimiter => {
                    self.cycle_delimiter(1);
                    self.recompute_preview();
                }
            },
            KeyCode::Backspace if self.active_parse_field == ParseField::SkipLines => {
                self.skip_lines /= 10;
                self.recompute_preview();
            }
            KeyCode::Char(c)
                if c.is_ascii_digit() && self.active_parse_field == ParseField::SkipLines =>
            {
                let digit = c.to_digit(10).unwrap() as usize;
                // Cap to a sensible upper bound to avoid overflow on long input.
                if self.skip_lines < 1_000_000 {
                    self.skip_lines = self.skip_lines * 10 + digit;
                    self.recompute_preview();
                }
            }
            _ => {}
        }
    }

    /// Cycle the active delimiter forward (`+1`) or backward (`-1`) through
    /// `DELIMITER_CHOICES`.
    fn cycle_delimiter(&mut self, direction: i32) {
        let len = DELIMITER_CHOICES.len() as i32;
        let current = DELIMITER_CHOICES
            .iter()
            .position(|(c, _)| *c == self.delimiter)
            .unwrap_or(0) as i32;
        let next = ((current + direction).rem_euclid(len)) as usize;
        self.delimiter = DELIMITER_CHOICES[next].0;
    }

    fn auto_detect_columns(&mut self) {
        if let Some(ref preview) = self.preview {
            for (idx, header) in preview.headers.iter().enumerate() {
                let lower = header.to_lowercase();

                if self.date_column.is_none() && (lower.contains("date") || lower.contains("time"))
                {
                    self.date_column = Some(idx);
                }

                if self.description_column.is_none()
                    && (lower.contains("desc")
                        || lower.contains("memo")
                        || lower.contains("narr")
                        || lower.contains("detail")
                        || lower.contains("name"))
                {
                    self.description_column = Some(idx);
                }

                if self.amount_column.is_none()
                    && (lower.contains("amount")
                        || lower.contains("value")
                        || lower.contains("sum")
                        || lower.contains("total"))
                {
                    self.amount_column = Some(idx);
                }
            }
        }
    }

    /// Get import configuration if ready
    pub fn get_import_config(&self) -> Option<ImportConfig> {
        if !self.ready_to_import {
            return None;
        }

        Some(ImportConfig {
            file_path: self.file_path.clone(),
            date_column: self.date_column?,
            description_column: self.description_column?,
            amount_column: self.amount_column?,
            target_account_id: self.target_account_id.clone(),
            skip_lines: self.skip_lines,
            has_header: self.has_header,
            delimiter: self.delimiter,
        })
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let modal_area = centered_rect(80, 80, area);
        frame.render_widget(Clear, modal_area);

        match self.step {
            ImportStep::SelectFile => self.draw_file_select(frame, modal_area, theme),
            ImportStep::Parsing => self.draw_parsing(frame, modal_area, theme),
            ImportStep::SelectAccount => self.draw_account_select(frame, modal_area, theme),
            ImportStep::MapColumns => self.draw_map_columns(frame, modal_area, theme),
            ImportStep::Confirm => self.draw_confirm(frame, modal_area, theme),
        }
    }

    fn draw_file_select(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3), // Title
                Constraint::Length(3), // Input
                Constraint::Min(5),    // Suggestions
                Constraint::Length(2), // Help
            ])
            .split(area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Import CSV - Select File ");
        frame.render_widget(block, area);

        // Target account info
        let target_text = if let Some(ref name) = self.target_account_name {
            format!("Import transactions to: {}", name)
        } else {
            "Import transactions (select target account first)".to_string()
        };
        let target = Paragraph::new(target_text).style(Style::default().fg(theme.header));
        frame.render_widget(target, chunks[0]);

        // File path input
        let input_text = format!("{}▏", self.file_path);
        let input = Paragraph::new(input_text)
            .block(Block::default().borders(Borders::ALL).title(" File path "));
        frame.render_widget(input, chunks[1]);

        // Suggestions list
        let items: Vec<ListItem> = self
            .file_suggestions
            .iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| p.to_string_lossy().to_string());
                let display = if p.is_dir() {
                    format!("{}/", name)
                } else {
                    name
                };
                ListItem::new(display)
            })
            .collect();

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Suggestions "),
            )
            .highlight_style(theme.selected_style());

        frame.render_stateful_widget(list, chunks[2], &mut self.suggestion_state.clone());

        // Error or help
        let help_text = if let Some(ref err) = self.error_message {
            Line::from(Span::styled(err, Style::default().fg(theme.error)))
        } else {
            Line::from(vec![
                Span::styled("Tab", Style::default().fg(theme.header)),
                Span::raw(": autocomplete  "),
                Span::styled("↑↓", Style::default().fg(theme.header)),
                Span::raw(": select  "),
                Span::styled("Enter", Style::default().fg(theme.header)),
                Span::raw(": open  "),
                Span::styled("Esc", Style::default().fg(theme.header)),
                Span::raw(": cancel"),
            ])
        };
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[3]);
    }

    fn draw_parsing(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Import CSV - Parse Options ");
        frame.render_widget(block, area);

        let inner = inner_rect(area, 2, 2);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5), // Parse options block
                Constraint::Length(1), // Spacer / preview label
                Constraint::Min(5),    // Preview
                Constraint::Length(2), // Help / error
            ])
            .split(inner);

        // ----- Parse options field block -----
        let active_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let inactive_style = Style::default();
        let marker = |field: ParseField| {
            if self.active_parse_field == field {
                "► "
            } else {
                "  "
            }
        };
        let style_for = |field: ParseField| {
            if self.active_parse_field == field {
                active_style
            } else {
                inactive_style
            }
        };

        let delim_label = DELIMITER_CHOICES
            .iter()
            .find(|(c, _)| *c == self.delimiter)
            .map(|(_, l)| *l)
            .unwrap_or("?");

        let header_label = if self.has_header { "Yes" } else { "No" };

        let option_lines = vec![
            Line::from(Span::styled(
                format!(
                    "{}Leading lines to skip?  [ {} ]",
                    marker(ParseField::SkipLines),
                    self.skip_lines
                ),
                style_for(ParseField::SkipLines),
            )),
            Line::from(Span::styled(
                format!(
                    "{}Has header row?         [ {} ]",
                    marker(ParseField::HasHeader),
                    header_label
                ),
                style_for(ParseField::HasHeader),
            )),
            Line::from(Span::styled(
                format!(
                    "{}Delimiter:              [ {} ]",
                    marker(ParseField::Delimiter),
                    delim_label
                ),
                style_for(ParseField::Delimiter),
            )),
        ];
        let options_widget = Paragraph::new(option_lines)
            .block(Block::default().borders(Borders::ALL).title(" Options "));
        frame.render_widget(options_widget, chunks[0]);

        // ----- Preview label -----
        let preview_label = Paragraph::new(Span::styled(
            "Preview (header in accent color, then data rows):",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(preview_label, chunks[1]);

        // ----- Preview pane -----
        let mut preview_lines: Vec<Line> = Vec::new();
        if let Some(ref preview) = self.preview {
            let header_style = if self.has_header {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.header)
                    .add_modifier(Modifier::DIM | Modifier::ITALIC)
            };
            let header_line: Vec<Span> = preview
                .headers
                .iter()
                .enumerate()
                .flat_map(|(i, h)| {
                    let mut spans =
                        vec![Span::styled(format!("{}: {}", i + 1, h), header_style)];
                    if i + 1 < preview.headers.len() {
                        spans.push(Span::raw("  |  "));
                    }
                    spans
                })
                .collect();
            preview_lines.push(Line::from(header_line));
            preview_lines.push(Line::from(""));

            for row in &preview.rows {
                let row_spans: Vec<Span> = row
                    .iter()
                    .enumerate()
                    .flat_map(|(i, val)| {
                        let mut spans = vec![Span::raw(val.clone())];
                        if i + 1 < row.len() {
                            spans.push(Span::raw("  |  "));
                        }
                        spans
                    })
                    .collect();
                preview_lines.push(Line::from(row_spans));
            }
        } else {
            preview_lines.push(Line::from(Span::styled(
                "(no preview available)",
                Style::default().fg(theme.error),
            )));
        }

        let preview_widget =
            Paragraph::new(preview_lines).block(Block::default().borders(Borders::ALL));
        frame.render_widget(preview_widget, chunks[2]);

        // ----- Help / error -----
        let help_text = if let Some(ref err) = self.error_message {
            Line::from(Span::styled(err, Style::default().fg(theme.error)))
        } else {
            Line::from(vec![
                Span::styled("Tab/↑↓", Style::default().fg(theme.header)),
                Span::raw(": switch field  "),
                Span::styled("←→", Style::default().fg(theme.header)),
                Span::raw(": adjust  "),
                Span::styled("Enter", Style::default().fg(theme.header)),
                Span::raw(": continue  "),
                Span::styled("Esc", Style::default().fg(theme.header)),
                Span::raw(": back"),
            ])
        };
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[3]);
    }

    fn draw_account_select(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3), // Title/filter
                Constraint::Min(5),    // Account list
                Constraint::Length(2), // Help
            ])
            .split(area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Import CSV - Select Target Account ");
        frame.render_widget(block, area);

        // Filter input
        let filter_text = format!("Filter: {}▏", self.account_filter);
        let filter = Paragraph::new(filter_text).block(Block::default().borders(Borders::ALL));
        frame.render_widget(filter, chunks[0]);

        // Account list
        let filtered = self.filtered_accounts();
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|a| {
                let text = format!("{} - {} ({})", a.account_number, a.name, a.account_type);
                ListItem::new(text)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Accounts "))
            .highlight_style(theme.selected_style());

        frame.render_stateful_widget(list, chunks[1], &mut self.account_state.clone());

        // Help
        let help_text = Line::from(vec![
            Span::styled("↑↓", Style::default().fg(theme.header)),
            Span::raw(": select  "),
            Span::styled("Enter", Style::default().fg(theme.header)),
            Span::raw(": confirm  "),
            Span::styled("Type", Style::default().fg(theme.header)),
            Span::raw(": filter  "),
            Span::styled("Esc", Style::default().fg(theme.header)),
            Span::raw(": back"),
        ]);
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[2]);
    }

    fn draw_map_columns(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Import CSV - Map Columns ");
        frame.render_widget(block, area);

        let inner = inner_rect(area, 2, 2);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(8), // Preview
                Constraint::Length(6), // Column mapping
                Constraint::Min(3),    // Sample data
                Constraint::Length(2), // Help
            ])
            .split(inner);

        // Preview table headers
        if let Some(ref preview) = self.preview {
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
                header_lines.push(Line::from(format!("  {}: {}{}", idx + 1, header, marker)));
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
                    .map(|s| s.as_str())
                    .unwrap_or("(not set)");

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

            // Sample data from first row
            if let Some(first_row) = preview.rows.first() {
                let mut sample_lines = vec![Line::from(Span::styled(
                    "Sample row:",
                    Style::default().add_modifier(Modifier::BOLD),
                ))];

                if let Some(idx) = self.date_column {
                    let val = first_row.get(idx).map(|s| s.as_str()).unwrap_or("N/A");
                    sample_lines.push(Line::from(format!("  Date: {}", val)));
                }
                if let Some(idx) = self.description_column {
                    let val = first_row.get(idx).map(|s| s.as_str()).unwrap_or("N/A");
                    sample_lines.push(Line::from(format!("  Description: {}", val)));
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
        let help_text = if let Some(ref err) = self.error_message {
            Line::from(Span::styled(err, Style::default().fg(theme.error)))
        } else {
            Line::from(vec![
                Span::styled("↑↓/Tab", Style::default().fg(theme.header)),
                Span::raw(": select field  "),
                Span::styled("←→/1-9", Style::default().fg(theme.header)),
                Span::raw(": choose column  "),
                Span::styled("Enter", Style::default().fg(theme.header)),
                Span::raw(": continue  "),
                Span::styled("Esc", Style::default().fg(theme.header)),
                Span::raw(": back"),
            ])
        };
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[3]);
    }

    fn draw_confirm(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Import CSV - Confirm ");
        frame.render_widget(block, area);

        let inner = inner_rect(area, 2, 2);

        let row_count = self.preview.as_ref().map(|p| p.rows.len()).unwrap_or(0);
        let target = self
            .target_account_name
            .as_deref()
            .unwrap_or("(no account selected)");

        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Ready to import",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("File: {}", self.file_path)),
            Line::from(format!("Target account: {}", target)),
            Line::from(format!("Rows to import: {} (preview)", row_count)),
            Line::from(""),
            Line::from("Unmatched entries will be posted to 'Uncategorized' account."),
            Line::from(""),
            Line::from(vec![
                Span::styled("Press ", Style::default()),
                Span::styled(
                    "Y/Enter",
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" to import or "),
                Span::styled(
                    "N/Esc",
                    Style::default().fg(theme.error).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" to go back"),
            ]),
        ];

        let content = Paragraph::new(lines);
        frame.render_widget(content, inner);
    }
}

impl Default for CsvImportModal {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for import operation
#[derive(Debug, Clone)]
pub struct ImportConfig {
    pub file_path: String,
    pub date_column: usize,
    pub description_column: usize,
    pub amount_column: usize,
    pub target_account_id: Option<String>,
    pub skip_lines: usize,
    pub has_header: bool,
    pub delimiter: char,
}

/// Expand a leading `~` or `~/` in a path to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// Parse a CSV line using a comma delimiter, handling quoted fields.
/// Kept for callers that don't need to choose a delimiter.
pub fn parse_csv_line(line: &str) -> Vec<String> {
    parse_delimited_line(line, ',')
}

/// Parse a delimited line, handling double-quoted fields with escaped quotes.
pub fn parse_delimited_line(line: &str, delimiter: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => {
                if in_quotes {
                    // Check for escaped quote
                    if chars.peek() == Some(&'"') {
                        current.push('"');
                        chars.next();
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                }
            }
            c if c == delimiter && !in_quotes => {
                fields.push(current.trim().to_string());
                current = String::new();
            }
            _ => {
                current.push(c);
            }
        }
    }

    fields.push(current.trim().to_string());
    fields
}

/// Parse a date string in various formats
pub fn parse_date(s: &str) -> Option<chrono::NaiveDate> {
    let s = s.trim();

    // Try yyyy/mm/dd or yyyy-mm-dd
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y/%m/%d") {
        return Some(date);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(date);
    }

    // Try mm/dd/yy or mm-dd-yy
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%m/%d/%y") {
        return Some(date);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%m-%d-%y") {
        return Some(date);
    }

    // Try mm/dd/yyyy or mm-dd-yyyy
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%m/%d/%Y") {
        return Some(date);
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%m-%d-%Y") {
        return Some(date);
    }

    None
}

/// Parse an amount string, handling currency symbols and negative formats
pub fn parse_amount(s: &str) -> Option<i64> {
    let s = s.trim();

    // Handle parentheses for negative: (123.45) -> -123.45
    let (is_negative, s) =
        if let Some(inner) = s.strip_prefix('(').and_then(|s| s.strip_suffix(')')) {
            (true, inner)
        } else if let Some(rest) = s.strip_prefix('-') {
            (true, rest)
        } else {
            (false, s)
        };

    // Remove currency symbols and commas
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();

    // Parse as float and convert to cents
    let value: f64 = cleaned.parse().ok()?;
    let cents = (value * 100.0).round() as i64;

    Some(if is_negative { -cents } else { cents })
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
