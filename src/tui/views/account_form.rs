use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::domain::{Account, AccountType};
use crate::tui::theme::Theme;
use crate::tui::widgets::{self, TextField};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountFormResult {
    None,
    Cancel,
    Create(NewAccountData),
    Update(UpdateAccountData),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAccountData {
    pub account_type: AccountType,
    pub account_number: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAccountData {
    pub account_id: String,
    pub account_number: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormField {
    AccountType,
    AccountNumber,
    Name,
    Parent,
    Description,
}

impl FormField {
    fn next(&self) -> Self {
        match self {
            FormField::AccountType => FormField::AccountNumber,
            FormField::AccountNumber => FormField::Name,
            FormField::Name => FormField::Parent,
            FormField::Parent => FormField::Description,
            FormField::Description => FormField::AccountType,
        }
    }

    fn prev(&self) -> Self {
        match self {
            FormField::AccountType => FormField::Description,
            FormField::AccountNumber => FormField::AccountType,
            FormField::Name => FormField::AccountNumber,
            FormField::Parent => FormField::Name,
            FormField::Description => FormField::Parent,
        }
    }
}

pub struct AccountForm {
    pub visible: bool,
    pub result: AccountFormResult,

    editing_account_id: Option<String>, // Some = edit mode, None = create mode
    active_field: FormField,
    account_type_index: usize,
    account_number: String,
    name: String,
    parent_index: usize, // 0 = None, 1+ = account index
    description: String,

    available_parents: Vec<Account>,
    error_message: Option<String>,

    // For dropdown selection
    type_list_state: ListState,
    parent_list_state: ListState,
    show_type_dropdown: bool,
    show_parent_dropdown: bool,

    // Autocomplete for parent selection
    parent_filter: String,
}

const ACCOUNT_TYPES: [AccountType; 5] = [
    AccountType::Asset,
    AccountType::Liability,
    AccountType::Equity,
    AccountType::Revenue,
    AccountType::Expense,
];

impl AccountForm {
    pub fn new() -> Self {
        let mut type_list_state = ListState::default();
        type_list_state.select(Some(0));
        let mut parent_list_state = ListState::default();
        parent_list_state.select(Some(0));

        Self {
            visible: false,
            result: AccountFormResult::None,
            editing_account_id: None,
            active_field: FormField::AccountType,
            account_type_index: 0,
            account_number: String::new(),
            name: String::new(),
            parent_index: 0,
            description: String::new(),
            available_parents: Vec::new(),
            error_message: None,
            type_list_state,
            parent_list_state,
            show_type_dropdown: false,
            show_parent_dropdown: false,
            parent_filter: String::new(),
        }
    }

    pub fn show(&mut self, existing_accounts: Vec<Account>) {
        self.show_with_defaults(existing_accounts, None, None, None);
    }

    /// Show the form with optional prepopulated defaults (for creating new accounts)
    pub fn show_with_defaults(
        &mut self,
        existing_accounts: Vec<Account>,
        account_type: Option<AccountType>,
        parent_id: Option<&str>,
        account_number: Option<String>,
    ) {
        self.visible = true;
        self.result = AccountFormResult::None;
        self.editing_account_id = None; // Create mode
        self.active_field = FormField::AccountType;
        self.error_message = None;
        self.show_type_dropdown = false;
        self.show_parent_dropdown = false;
        self.parent_filter.clear();

        // Set account type
        self.account_type_index = account_type
            .and_then(|t| ACCOUNT_TYPES.iter().position(|at| *at == t))
            .unwrap_or(0);
        self.type_list_state.select(Some(self.account_type_index));

        // Set account number
        self.account_number = account_number.unwrap_or_default();

        // Clear other fields
        self.name.clear();
        self.description.clear();

        // Set available parents and find parent index
        self.available_parents = existing_accounts;
        self.parent_index = parent_id
            .and_then(|pid| {
                self.available_parents
                    .iter()
                    .position(|a| a.id == pid)
                    .map(|i| i + 1) // +1 because 0 = None
            })
            .unwrap_or(0);
        self.parent_list_state.select(Some(self.parent_index));
    }

    /// Show the form for editing an existing account
    pub fn show_edit(&mut self, account: &Account, existing_accounts: Vec<Account>) {
        self.visible = true;
        self.result = AccountFormResult::None;
        self.editing_account_id = Some(account.id.clone()); // Edit mode
        self.active_field = FormField::AccountNumber; // Start at account number (can't change type)
        self.error_message = None;
        self.show_type_dropdown = false;
        self.show_parent_dropdown = false;
        self.parent_filter.clear();

        // Set account type (read-only in edit mode)
        self.account_type_index = ACCOUNT_TYPES
            .iter()
            .position(|at| *at == account.account_type)
            .unwrap_or(0);
        self.type_list_state.select(Some(self.account_type_index));

        // Set current values
        self.account_number = account.account_number.clone();
        self.name = account.name.clone();
        self.description = account.description.clone().unwrap_or_default();

        // Filter out the account being edited from available parents (can't be its own parent)
        self.available_parents = existing_accounts
            .into_iter()
            .filter(|a| a.id != account.id)
            .collect();

        // Find current parent index
        self.parent_index = account
            .parent_id
            .as_ref()
            .and_then(|pid| {
                self.available_parents
                    .iter()
                    .position(|a| &a.id == pid)
                    .map(|i| i + 1) // +1 because 0 = None
            })
            .unwrap_or(0);
        self.parent_list_state.select(Some(self.parent_index));
    }

    /// Returns true if the form is in edit mode
    pub fn is_editing(&self) -> bool {
        self.editing_account_id.is_some()
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        // Handle dropdowns first
        if self.show_type_dropdown {
            self.handle_type_dropdown_key(key);
            return;
        }
        if self.show_parent_dropdown {
            self.handle_parent_dropdown_key(key);
            return;
        }

        match key {
            KeyCode::Esc => {
                // Clear filter if typing in parent field, otherwise cancel
                if self.active_field == FormField::Parent && !self.parent_filter.is_empty() {
                    self.parent_filter.clear();
                    self.show_parent_dropdown = false;
                } else {
                    self.result = AccountFormResult::Cancel;
                }
            }
            KeyCode::Tab => {
                // Clear parent filter when leaving field
                if self.active_field == FormField::Parent {
                    self.parent_filter.clear();
                    self.show_parent_dropdown = false;
                }
                self.active_field = self.active_field.next();
            }
            KeyCode::BackTab => {
                // Clear parent filter when leaving field
                if self.active_field == FormField::Parent {
                    self.parent_filter.clear();
                    self.show_parent_dropdown = false;
                }
                self.active_field = self.active_field.prev();
            }
            KeyCode::Enter => {
                match self.active_field {
                    FormField::AccountType => {
                        // Can't change account type in edit mode
                        if !self.is_editing() {
                            self.show_type_dropdown = true;
                        }
                    }
                    FormField::Parent => {
                        // If dropdown is open with filter, confirm selection
                        if self.show_parent_dropdown {
                            // Confirm current selection and close
                            self.show_parent_dropdown = false;
                            self.parent_filter.clear();
                        } else {
                            // Open dropdown for browsing
                            self.show_parent_dropdown = true;
                        }
                    }
                    _ => {
                        self.submit();
                    }
                }
            }
            KeyCode::Up | KeyCode::Down => {
                // For Parent field, handle navigation; for others, move between fields
                if self.active_field == FormField::Parent {
                    self.handle_field_input(key);
                } else if key == KeyCode::Down {
                    self.active_field = self.active_field.next();
                } else {
                    self.active_field = self.active_field.prev();
                }
            }
            _ => {
                self.handle_field_input(key);
            }
        }
    }

    fn handle_type_dropdown_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.show_type_dropdown = false;
            }
            KeyCode::Enter => {
                if let Some(i) = self.type_list_state.selected() {
                    self.account_type_index = i;
                }
                self.show_type_dropdown = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let i = self.type_list_state.selected().unwrap_or(0);
                let new_i = if i == 0 {
                    ACCOUNT_TYPES.len() - 1
                } else {
                    i - 1
                };
                self.type_list_state.select(Some(new_i));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let i = self.type_list_state.selected().unwrap_or(0);
                let new_i = if i >= ACCOUNT_TYPES.len() - 1 {
                    0
                } else {
                    i + 1
                };
                self.type_list_state.select(Some(new_i));
            }
            _ => {}
        }
    }

    fn handle_parent_dropdown_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.show_parent_dropdown = false;
                self.parent_filter.clear();
            }
            KeyCode::Enter => {
                // Confirm the currently selected match
                if let Some(i) = self.parent_list_state.selected() {
                    let filtered = self.filtered_parent_indices();
                    if let Some(&actual_index) = filtered.get(i) {
                        self.parent_index = actual_index;
                    }
                }
                self.show_parent_dropdown = false;
                self.parent_filter.clear();
            }
            KeyCode::Up => {
                self.navigate_filtered_parents(-1);
            }
            KeyCode::Down => {
                self.navigate_filtered_parents(1);
            }
            KeyCode::Backspace => {
                self.parent_filter.pop();
                self.update_parent_selection_from_filter();
            }
            KeyCode::Char(c) => {
                self.parent_filter.push(c);
                self.update_parent_selection_from_filter();
            }
            _ => {}
        }
    }

    /// Get indices of parent accounts that match the current filter
    /// Returns indices into the display list (0 = None, 1+ = account index)
    fn filtered_parent_indices(&self) -> Vec<usize> {
        if self.parent_filter.is_empty() {
            // All indices including (None)
            (0..=self.available_parents.len()).collect()
        } else {
            let filter_lower = self.parent_filter.to_lowercase();
            let mut matches: Vec<usize> = vec![];

            // Check if "(None)" matches
            if "none".contains(&filter_lower) {
                matches.push(0);
            }

            // Check each account
            for (i, account) in self.available_parents.iter().enumerate() {
                let account_num = &account.account_number.to_lowercase();
                let account_name = &account.name.to_lowercase();

                if account_num.contains(&filter_lower) || account_name.contains(&filter_lower) {
                    matches.push(i + 1); // +1 because 0 = None
                }
            }

            matches
        }
    }

    /// Update parent selection to the first match based on filter
    fn update_parent_selection_from_filter(&mut self) {
        let filtered = self.filtered_parent_indices();
        if !filtered.is_empty() {
            self.parent_list_state.select(Some(0)); // Select first in filtered list
            self.parent_index = filtered[0];
        }
    }

    /// Navigate through filtered parents
    fn navigate_filtered_parents(&mut self, delta: isize) {
        let filtered = self.filtered_parent_indices();
        if filtered.is_empty() {
            return;
        }

        let current_list_idx = self.parent_list_state.selected().unwrap_or(0);
        let new_list_idx = if delta > 0 {
            if current_list_idx >= filtered.len() - 1 {
                0
            } else {
                current_list_idx + 1
            }
        } else if current_list_idx == 0 {
            filtered.len() - 1
        } else {
            current_list_idx - 1
        };

        self.parent_list_state.select(Some(new_list_idx));
        if let Some(&actual_index) = filtered.get(new_list_idx) {
            self.parent_index = actual_index;
        }
    }

    fn handle_field_input(&mut self, key: KeyCode) {
        // Handle Parent field with autocomplete
        if self.active_field == FormField::Parent {
            match key {
                KeyCode::Char(c) => {
                    // Start typing - open dropdown and filter
                    self.show_parent_dropdown = true;
                    self.parent_filter.push(c);
                    self.update_parent_selection_from_filter();
                    self.error_message = None;
                }
                KeyCode::Backspace => {
                    if !self.parent_filter.is_empty() {
                        self.parent_filter.pop();
                        self.update_parent_selection_from_filter();
                    }
                    self.error_message = None;
                }
                KeyCode::Up => {
                    // Navigate to previous match
                    self.show_parent_dropdown = true;
                    self.navigate_filtered_parents(-1);
                }
                KeyCode::Down => {
                    // Navigate to next match (but not when using Tab to move fields)
                    self.show_parent_dropdown = true;
                    self.navigate_filtered_parents(1);
                }
                _ => {}
            }
            return;
        }

        let field = match self.active_field {
            FormField::AccountNumber => &mut self.account_number,
            FormField::Name => &mut self.name,
            FormField::Description => &mut self.description,
            _ => return,
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
        // Validate
        if self.account_number.trim().is_empty() {
            self.error_message = Some("Account number is required".to_string());
            self.active_field = FormField::AccountNumber;
            return;
        }

        if self.name.trim().is_empty() {
            self.error_message = Some("Account name is required".to_string());
            self.active_field = FormField::Name;
            return;
        }

        let parent_id = if self.parent_index == 0 {
            None
        } else {
            self.available_parents
                .get(self.parent_index - 1)
                .map(|a| a.id.clone())
        };

        let description = if self.description.trim().is_empty() {
            None
        } else {
            Some(self.description.trim().to_string())
        };

        if let Some(account_id) = &self.editing_account_id {
            // Edit mode - return Update result
            self.result = AccountFormResult::Update(UpdateAccountData {
                account_id: account_id.clone(),
                account_number: self.account_number.trim().to_string(),
                name: self.name.trim().to_string(),
                parent_id,
                description,
            });
        } else {
            // Create mode - return Create result
            self.result = AccountFormResult::Create(NewAccountData {
                account_type: ACCOUNT_TYPES[self.account_type_index],
                account_number: self.account_number.trim().to_string(),
                name: self.name.trim().to_string(),
                parent_id,
                description,
            });
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let modal_area = widgets::centered_rect(50, 60, area);
        frame.render_widget(Clear, modal_area);

        let title = if self.is_editing() {
            " Edit Account "
        } else {
            " New Account "
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(title)
            .title_style(
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(block, modal_area);

        let inner = widgets::inner_rect(modal_area, 2, 1);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Account Type
                Constraint::Length(3), // Account Number
                Constraint::Length(3), // Name
                Constraint::Length(3), // Parent
                Constraint::Length(3), // Description
                Constraint::Length(2), // Error/Help
                Constraint::Min(0),    // Spacer
            ])
            .split(inner);

        // Account Type field (read-only in edit mode)
        if self.is_editing() {
            self.draw_readonly_field(
                frame,
                chunks[0],
                "Account Type (read-only)",
                &format!("{:?}", ACCOUNT_TYPES[self.account_type_index]),
                theme,
            );
        } else {
            self.draw_dropdown_field(
                frame,
                chunks[0],
                "Account Type",
                &format!("{:?}", ACCOUNT_TYPES[self.account_type_index]),
                self.active_field == FormField::AccountType,
                theme,
            );
        }

        // Account Number field
        widgets::draw_text_field(
            frame,
            &TextField::new(
                chunks[1],
                "Account Number",
                &self.account_number,
                self.active_field == FormField::AccountNumber,
            ),
            theme,
        );

        // Name field
        widgets::draw_text_field(
            frame,
            &TextField::new(
                chunks[2],
                "Name",
                &self.name,
                self.active_field == FormField::Name,
            ),
            theme,
        );

        // Parent field - show filter text when typing, otherwise show selected value
        let is_parent_active = self.active_field == FormField::Parent;
        if is_parent_active && !self.parent_filter.is_empty() {
            // Show filter input with cursor
            self.draw_autocomplete_field(
                frame,
                chunks[3],
                "Parent Account (type to search)",
                &self.parent_filter,
                true,
                theme,
            );
        } else {
            // Show selected value
            let parent_display = if self.parent_index == 0 {
                "(None)".to_string()
            } else {
                self.available_parents
                    .get(self.parent_index - 1)
                    .map(|a| format!("{} - {}", a.account_number, a.name))
                    .unwrap_or_else(|| "(None)".to_string())
            };
            self.draw_dropdown_field(
                frame,
                chunks[3],
                "Parent Account",
                &parent_display,
                is_parent_active,
                theme,
            );
        }

        // Description field
        widgets::draw_text_field(
            frame,
            &TextField::new(
                chunks[4],
                "Description",
                &self.description,
                self.active_field == FormField::Description,
            ),
            theme,
        );

        // Error or help text
        let help_text = if let Some(ref err) = self.error_message {
            Line::from(Span::styled(err.clone(), Style::default().fg(theme.error)))
        } else {
            Line::from(vec![
                Span::styled("Tab", Style::default().fg(theme.header)),
                Span::raw(": next field  "),
                Span::styled("Enter", Style::default().fg(theme.header)),
                Span::raw(": select/submit  "),
                Span::styled("Esc", Style::default().fg(theme.header)),
                Span::raw(": cancel"),
            ])
        };
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[5]);

        // Draw dropdowns on top
        if self.show_type_dropdown {
            self.draw_type_dropdown(frame, chunks[0], theme);
        }
        if self.show_parent_dropdown {
            self.draw_parent_dropdown(frame, chunks[3], theme);
        }
    }

    fn draw_readonly_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        label: &str,
        value: &str,
        theme: &Theme,
    ) {
        let paragraph = Paragraph::new(value.to_string())
            .style(Style::default().fg(theme.fg_disabled))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.fg_disabled))
                    .title(format!(" {} ", label)),
            );

        frame.render_widget(paragraph, area);
    }

    fn draw_dropdown_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        label: &str,
        value: &str,
        is_active: bool,
        theme: &Theme,
    ) {
        let style = if is_active {
            Style::default().fg(theme.input_active_fg)
        } else {
            Style::default().fg(theme.input_inactive_fg)
        };

        let border_style = if is_active {
            Style::default().fg(theme.input_active_border)
        } else {
            Style::default().fg(theme.input_inactive_border)
        };

        let display = format!("{} ▼", value);

        let paragraph = Paragraph::new(display).style(style).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(" {} ", label)),
        );

        frame.render_widget(paragraph, area);
    }

    fn draw_autocomplete_field(
        &self,
        frame: &mut Frame,
        area: Rect,
        label: &str,
        filter: &str,
        _is_active: bool,
        theme: &Theme,
    ) {
        let style = Style::default().fg(theme.input_active_fg);
        let border_style = Style::default().fg(theme.input_active_border);

        // Show filter text with cursor
        let display = format!("{}█", filter);

        let paragraph = Paragraph::new(display).style(style).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(format!(" {} ", label)),
        );

        frame.render_widget(paragraph, area);
    }

    fn draw_type_dropdown(&self, frame: &mut Frame, anchor: Rect, theme: &Theme) {
        let items: Vec<ListItem> = ACCOUNT_TYPES
            .iter()
            .map(|t| ListItem::new(format!("{:?}", t)))
            .collect();

        let dropdown_area = Rect {
            x: anchor.x,
            y: anchor.y + anchor.height,
            width: anchor.width,
            height: (ACCOUNT_TYPES.len() as u16 + 2).min(10),
        };

        frame.render_widget(Clear, dropdown_area);

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.accent)),
            )
            .highlight_style(theme.selected_style())
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, dropdown_area, &mut self.type_list_state.clone());
    }

    fn draw_parent_dropdown(&self, frame: &mut Frame, anchor: Rect, theme: &Theme) {
        // Get filtered indices
        let filtered_indices = self.filtered_parent_indices();

        // Build items from filtered indices
        let items: Vec<ListItem> = filtered_indices
            .iter()
            .map(|&idx| {
                if idx == 0 {
                    ListItem::new("(None)")
                } else {
                    let account = &self.available_parents[idx - 1];
                    ListItem::new(format!("{} - {}", account.account_number, account.name))
                }
            })
            .collect();

        if items.is_empty() {
            // Show "no matches" message
            let dropdown_area = Rect {
                x: anchor.x,
                y: anchor.y + anchor.height,
                width: anchor.width,
                height: 3,
            };
            frame.render_widget(Clear, dropdown_area);
            let no_match = Paragraph::new("  No matches found")
                .style(Style::default().fg(theme.fg_dim))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(theme.accent)),
                );
            frame.render_widget(no_match, dropdown_area);
            return;
        }

        let dropdown_height = (items.len() as u16 + 2).min(10);
        let dropdown_area = Rect {
            x: anchor.x,
            y: anchor.y + anchor.height,
            width: anchor.width,
            height: dropdown_height,
        };

        frame.render_widget(Clear, dropdown_area);

        let title = if self.parent_filter.is_empty() {
            " Select Parent ".to_string()
        } else {
            format!(" {} matches ", filtered_indices.len())
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme.accent))
                    .title(title),
            )
            .highlight_style(theme.selected_style())
            .highlight_symbol("> ");

        frame.render_stateful_widget(list, dropdown_area, &mut self.parent_list_state.clone());
    }
}

impl Default for AccountForm {
    fn default() -> Self {
        Self::new()
    }
}
