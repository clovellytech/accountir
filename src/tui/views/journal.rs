use std::collections::HashSet;

use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
    },
    Frame,
};

use crate::domain::Account;
use crate::queries::search::EntrySearchResult;

/// Account choice for reassignment picker
#[derive(Debug, Clone)]
pub struct ReassignAccount {
    pub id: String,
    pub account_number: String,
    pub name: String,
}

/// Pending reassignment info
#[derive(Debug, Clone)]
pub struct PendingReassign {
    pub entry_id: String,
    pub line_id: String,
    pub current_account_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Date,
    Memo,
    Reference,
    Amount,
}

impl SortField {
    fn next(&self) -> Self {
        match self {
            SortField::Date => SortField::Memo,
            SortField::Memo => SortField::Reference,
            SortField::Reference => SortField::Amount,
            SortField::Amount => SortField::Date,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            SortField::Date => "Date",
            SortField::Memo => "Memo",
            SortField::Reference => "Reference",
            SortField::Amount => "Amount",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

impl SortDirection {
    fn toggle(&self) -> Self {
        match self {
            SortDirection::Ascending => SortDirection::Descending,
            SortDirection::Descending => SortDirection::Ascending,
        }
    }

    fn symbol(&self) -> &'static str {
        match self {
            SortDirection::Ascending => "↑",
            SortDirection::Descending => "↓",
        }
    }
}

pub struct JournalView {
    pub entries: Vec<EntrySearchResult>,
    pub state: TableState,
    pub filter_account: Option<Account>,
    pub sort_field: SortField,
    pub sort_direction: SortDirection,
    pub pending_void: Option<String>, // Entry ID pending void confirmation
    pub confirm_void: bool,           // Whether void is confirmed
    /// Running balances for ledger view (calculated after sorting)
    pub running_balances: Vec<i64>,
    /// Whether to show voided entries
    pub show_void: bool,
    /// Whether to show ID column
    pub show_id_column: bool,
    /// Multiselect mode (ledger view only)
    pub multiselect_mode: bool,
    /// Whether navigation selects entries (toggled with space in multiselect mode)
    pub multiselect_selecting: bool,
    /// Selected entry IDs for multiselect
    pub selected_entry_ids: HashSet<String>,
    /// Reassignment mode
    pub reassign_mode: bool,
    pub reassign_pending: Option<PendingReassign>,
    pub reassign_accounts: Vec<ReassignAccount>,
    pub reassign_filter: String,
    pub reassign_state: ListState,
    /// Confirmed reassignment (entry_id, line_id, new_account_id)
    pub reassign_confirmed: Option<(String, String, String)>,
    /// Confirmed bulk reassignment (list of entry_ids, new_account_id)
    pub bulk_reassign_confirmed: Option<(Vec<String>, String)>,
    /// Bulk void confirmation pending
    pub bulk_void_pending: bool,
    /// Confirmed bulk void (list of entry_ids)
    pub bulk_void_confirmed: Option<Vec<String>>,
    /// Last known visible height for scroll margin calculations
    visible_height: usize,
    /// Pending jump to other account's ledger (other_account_id, entry_id)
    pub pending_goto_account: Option<(String, String)>,
}

impl JournalView {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            state: TableState::default(),
            filter_account: None,
            sort_field: SortField::Date,
            sort_direction: SortDirection::Descending,
            pending_void: None,
            confirm_void: false,
            running_balances: Vec::new(),
            show_void: true,
            show_id_column: false,
            multiselect_mode: false,
            multiselect_selecting: false,
            selected_entry_ids: HashSet::new(),
            reassign_mode: false,
            reassign_pending: None,
            reassign_accounts: Vec::new(),
            reassign_filter: String::new(),
            reassign_state: ListState::default(),
            reassign_confirmed: None,
            bulk_reassign_confirmed: None,
            bulk_void_pending: false,
            bulk_void_confirmed: None,
            visible_height: 20, // Default, will be updated during draw
            pending_goto_account: None,
        }
    }

    /// Start reassignment mode for the selected entry
    pub fn start_reassign(
        &mut self,
        accounts: Vec<ReassignAccount>,
        entry_id: String,
        line_id: String,
        current_account_name: String,
        other_account_id: Option<String>,
    ) {
        self.reassign_mode = true;
        self.reassign_pending = Some(PendingReassign {
            entry_id,
            line_id,
            current_account_name,
        });
        self.reassign_accounts = accounts;
        self.reassign_filter.clear();

        // Select the other account if provided, otherwise start at 0
        let initial_selection = if let Some(ref other_id) = other_account_id {
            self.reassign_accounts
                .iter()
                .position(|a| a.id == *other_id)
                .unwrap_or(0)
        } else {
            0
        };
        self.reassign_state.select(Some(initial_selection));
        self.reassign_confirmed = None;
    }

    /// Cancel reassignment mode
    pub fn cancel_reassign(&mut self) {
        self.reassign_mode = false;
        self.reassign_pending = None;
        self.reassign_filter.clear();
    }

    /// Get filtered accounts for reassignment
    fn filtered_reassign_accounts(&self) -> Vec<&ReassignAccount> {
        let filter_lower = self.reassign_filter.to_lowercase();
        self.reassign_accounts
            .iter()
            .filter(|a| {
                filter_lower.is_empty()
                    || a.name.to_lowercase().contains(&filter_lower)
                    || a.account_number.contains(&filter_lower)
            })
            .collect()
    }

    /// Check if reassignment is confirmed and get the details
    pub fn take_reassign_confirmed(&mut self) -> Option<(String, String, String)> {
        self.reassign_confirmed.take()
    }

    /// Check if in reassignment mode
    pub fn is_reassigning(&self) -> bool {
        self.reassign_mode
    }

    /// Start bulk reassignment mode for selected entries
    pub fn start_bulk_reassign(&mut self, accounts: Vec<ReassignAccount>) {
        if self.selected_entry_ids.is_empty() {
            return;
        }
        self.reassign_mode = true;
        self.reassign_pending = None; // No single pending, we use selected_entry_ids
        self.reassign_accounts = accounts;
        self.reassign_filter.clear();
        self.reassign_state.select(Some(0));
        self.reassign_confirmed = None;
        self.bulk_reassign_confirmed = None;
    }

    /// Check if bulk reassignment is confirmed and get the details
    pub fn take_bulk_reassign_confirmed(&mut self) -> Option<(Vec<String>, String)> {
        self.bulk_reassign_confirmed.take()
    }

    /// Toggle multiselect mode (ledger view only)
    pub fn toggle_multiselect(&mut self) {
        if self.filter_account.is_none() {
            return; // Only in ledger view
        }
        self.multiselect_mode = !self.multiselect_mode;
        if !self.multiselect_mode {
            self.selected_entry_ids.clear();
            self.multiselect_selecting = false;
        } else {
            // Start with selection active
            self.multiselect_selecting = true;
            // Select current entry when entering multiselect
            if let Some(entry) = self.selected_entry() {
                if !entry.is_void {
                    self.selected_entry_ids.insert(entry.entry_id.clone());
                }
            }
        }
    }

    /// Exit multiselect mode
    pub fn exit_multiselect(&mut self) {
        self.multiselect_mode = false;
        self.multiselect_selecting = false;
        self.selected_entry_ids.clear();
    }

    /// Toggle selection of current entry
    pub fn toggle_current_selection(&mut self) {
        if let Some(entry) = self.selected_entry() {
            if entry.is_void {
                return; // Can't select voided entries
            }
            let entry_id = entry.entry_id.clone();
            if self.selected_entry_ids.contains(&entry_id) {
                self.selected_entry_ids.remove(&entry_id);
            } else {
                self.selected_entry_ids.insert(entry_id);
            }
        }
    }

    /// Select current entry (used when navigating in multiselect mode)
    fn select_current(&mut self) {
        if let Some(entry) = self.selected_entry() {
            if !entry.is_void {
                self.selected_entry_ids.insert(entry.entry_id.clone());
            }
        }
    }

    /// Check if an entry is selected
    pub fn is_entry_selected(&self, entry_id: &str) -> bool {
        self.selected_entry_ids.contains(entry_id)
    }

    /// Get selected entry IDs
    pub fn get_selected_entry_ids(&self) -> Vec<String> {
        self.selected_entry_ids.iter().cloned().collect()
    }

    /// Get filtered entries (respecting show_void setting)
    pub fn visible_entries(&self) -> Vec<&EntrySearchResult> {
        self.entries
            .iter()
            .filter(|e| self.show_void || !e.is_void)
            .collect()
    }

    /// Get the entry at the current visual selection (respects show_void filter)
    fn selected_entry(&self) -> Option<&EntrySearchResult> {
        let i = self.state.selected()?;
        let visible = self.visible_entries();
        visible.get(i).copied()
    }

    /// Get the number of visible entries
    fn visible_count(&self) -> usize {
        if self.show_void {
            self.entries.len()
        } else {
            self.entries.iter().filter(|e| !e.is_void).count()
        }
    }

    /// Toggle showing voided entries
    pub fn toggle_show_void(&mut self) {
        // Get the currently selected entry before toggling
        let old_selection = self.state.selected();
        let selected_entry_id = self.selected_entry().map(|e| e.entry_id.clone());

        self.show_void = !self.show_void;

        let visible = self.visible_entries();
        if visible.is_empty() {
            self.state.select(None);
            return;
        }

        if let Some(entry_id) = selected_entry_id {
            // Try to find the same entry in the new visible list
            if let Some(pos) = visible.iter().position(|e| e.entry_id == entry_id) {
                self.state.select(Some(pos));
                return;
            }
        }

        // Entry not found (was voided and now hidden, or no selection)
        // Find the nearest visible entry based on old selection position
        if let Some(old_pos) = old_selection {
            // Select the entry at the same position, or the last one if out of bounds
            let new_pos = old_pos.min(visible.len().saturating_sub(1));
            self.state.select(Some(new_pos));
        } else {
            self.state.select(Some(0));
        }
    }

    /// Toggle ID column visibility
    pub fn toggle_id_column(&mut self) {
        self.show_id_column = !self.show_id_column;
    }

    /// Calculate running balances for ledger view (must be called after sorting by date ascending)
    pub fn calculate_running_balances(&mut self) {
        self.running_balances.clear();
        if self.filter_account.is_none() {
            return;
        }

        // For running balance, we need entries sorted by date ascending
        // But we display them in whatever sort order the user wants
        // So we calculate based on date order, then map to display order

        // Create a list of (index, date, amount, is_void) sorted by date
        let mut date_sorted: Vec<(usize, chrono::NaiveDate, i64, bool)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (i, e.date, e.account_amount.unwrap_or(0), e.is_void))
            .collect();
        date_sorted.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

        // Calculate running balances in date order
        // Skip voided entries
        let mut balances_by_index = vec![0i64; self.entries.len()];
        let mut running = 0i64;
        for (idx, _date, amount, is_void) in date_sorted {
            if !is_void {
                running += amount;
            }
            balances_by_index[idx] = running;
        }

        // Store in display order
        self.running_balances = balances_by_index;
    }

    pub fn set_filter(&mut self, account: Account) {
        self.filter_account = Some(account);
        self.state.select(Some(0));
    }

    pub fn clear_filter(&mut self) {
        self.filter_account = None;
        self.exit_multiselect(); // Exit multiselect when leaving ledger view
    }

    pub fn is_filtered(&self) -> bool {
        self.filter_account.is_some()
    }

    /// Sort entries by current sort field and direction
    pub fn sort_entries(&mut self) {
        match self.sort_field {
            SortField::Date => {
                self.entries.sort_by(|a, b| {
                    if self.sort_direction == SortDirection::Ascending {
                        a.date.cmp(&b.date)
                    } else {
                        b.date.cmp(&a.date)
                    }
                });
            }
            SortField::Memo => {
                self.entries.sort_by(|a, b| {
                    if self.sort_direction == SortDirection::Ascending {
                        a.memo.to_lowercase().cmp(&b.memo.to_lowercase())
                    } else {
                        b.memo.to_lowercase().cmp(&a.memo.to_lowercase())
                    }
                });
            }
            SortField::Reference => {
                self.entries.sort_by(|a, b| {
                    let a_ref = a.reference.as_deref().unwrap_or("");
                    let b_ref = b.reference.as_deref().unwrap_or("");
                    if self.sort_direction == SortDirection::Ascending {
                        a_ref.cmp(b_ref)
                    } else {
                        b_ref.cmp(a_ref)
                    }
                });
            }
            SortField::Amount => {
                self.entries.sort_by(|a, b| {
                    if self.sort_direction == SortDirection::Ascending {
                        a.total_amount.cmp(&b.total_amount)
                    } else {
                        b.total_amount.cmp(&a.total_amount)
                    }
                });
            }
        }
        // Recalculate running balances after sort
        self.calculate_running_balances();
    }

    /// Cycle to next sort field
    pub fn next_sort_field(&mut self) {
        self.sort_field = self.sort_field.next();
        self.sort_entries();
    }

    /// Toggle sort direction
    pub fn toggle_sort_direction(&mut self) {
        self.sort_direction = self.sort_direction.toggle();
        self.sort_entries();
    }

    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        // Handle reassignment mode
        if self.reassign_mode {
            return self.handle_reassign_key(key);
        }

        // Handle void confirmation
        if self.pending_void.is_some() {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.confirm_void = true;
                    // pending_void will be consumed by the main loop
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                    self.pending_void = None;
                }
                _ => {}
            }
            return false;
        }

        // Handle bulk void confirmation
        if self.bulk_void_pending {
            match key {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let entry_ids: Vec<String> = self.selected_entry_ids.iter().cloned().collect();
                    self.bulk_void_confirmed = Some(entry_ids);
                    self.bulk_void_pending = false;
                    self.exit_multiselect();
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                    self.bulk_void_pending = false;
                }
                _ => {}
            }
            return false;
        }

        let is_ledger = self.filter_account.is_some();

        // Handle multiselect mode (ledger view only)
        if self.multiselect_mode {
            match key {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.previous();
                    if self.multiselect_selecting {
                        self.select_current();
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.next();
                    if self.multiselect_selecting {
                        self.select_current();
                    }
                }
                KeyCode::Char(' ') => {
                    // Toggle selection mode and select/deselect current entry
                    self.multiselect_selecting = !self.multiselect_selecting;
                    if self.multiselect_selecting {
                        self.select_current();
                    } else {
                        // Deselect current entry when turning off selection
                        let entry_id = self.selected_entry().map(|e| e.entry_id.clone());
                        if let Some(id) = entry_id {
                            self.selected_entry_ids.remove(&id);
                        }
                    }
                }
                KeyCode::Char('a') => return true, // Signal to app.rs to start bulk reassignment
                KeyCode::Char('x') => {
                    if !self.selected_entry_ids.is_empty() {
                        self.bulk_void_pending = true;
                    }
                }
                KeyCode::Esc => {
                    self.exit_multiselect();
                }
                KeyCode::Home => self.first(),
                KeyCode::End => self.last(),
                _ => {}
            }
            return false;
        }

        match key {
            KeyCode::Up | KeyCode::Char('k') => self.previous(),
            KeyCode::Down | KeyCode::Char('j') => self.next(),
            KeyCode::Home => self.first(),
            KeyCode::End => self.last(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::Char('s') => self.next_sort_field(),
            KeyCode::Char('r') => self.toggle_sort_direction(),
            KeyCode::Char('v') => {
                if is_ledger {
                    self.toggle_multiselect();
                }
            }
            KeyCode::Char('x') => {
                self.start_void();
            }
            KeyCode::Char('h') => self.toggle_show_void(),
            KeyCode::Char('c') => self.toggle_id_column(),
            KeyCode::Char('a') => return true, // Signal to app.rs to start reassignment
            KeyCode::Char('g') => {
                // Jump to other account's ledger (only in ledger view)
                if is_ledger {
                    self.request_goto_other_account();
                }
            }
            KeyCode::Esc | KeyCode::Backspace => {
                if self.filter_account.is_some() {
                    self.clear_filter();
                    return true; // Signal that filter was cleared
                }
            }
            _ => {}
        }
        false
    }

    fn start_void(&mut self) {
        if let Some(entry) = self.selected_entry() {
            // Toggle void/unvoid based on current state
            self.pending_void = Some(entry.entry_id.clone());
            self.confirm_void = false;
        }
    }

    /// Request to jump to the other account's ledger for the selected entry
    fn request_goto_other_account(&mut self) {
        if let Some(entry) = self.selected_entry() {
            if let Some(ref other_account_id) = entry.other_account_id {
                self.pending_goto_account =
                    Some((other_account_id.clone(), entry.entry_id.clone()));
            }
        }
    }

    /// Take pending goto account request (account_id, entry_id)
    pub fn take_pending_goto_account(&mut self) -> Option<(String, String)> {
        self.pending_goto_account.take()
    }

    /// Check if the pending void is actually an unvoid operation
    pub fn is_pending_unvoid(&self) -> bool {
        if let Some(ref entry_id) = self.pending_void {
            self.entries
                .iter()
                .any(|e| &e.entry_id == entry_id && e.is_void)
        } else {
            false
        }
    }

    /// Take confirmed void entry ID if ready
    pub fn take_confirmed_void(&mut self) -> Option<String> {
        if self.confirm_void {
            self.confirm_void = false;
            self.pending_void.take()
        } else {
            None
        }
    }

    /// Check if waiting for void confirmation
    pub fn is_confirming_void(&self) -> bool {
        self.pending_void.is_some() && !self.confirm_void
    }

    /// Check if waiting for bulk void confirmation
    pub fn is_confirming_bulk_void(&self) -> bool {
        self.bulk_void_pending
    }

    /// Take confirmed bulk void entry IDs if ready
    pub fn take_bulk_void_confirmed(&mut self) -> Option<Vec<String>> {
        self.bulk_void_confirmed.take()
    }

    fn handle_reassign_key(&mut self, key: KeyCode) -> bool {
        let filtered = self.filtered_reassign_accounts();
        let num_accounts = filtered.len();
        let is_bulk = self.reassign_pending.is_none();

        match key {
            KeyCode::Esc => {
                self.cancel_reassign();
            }
            KeyCode::Enter => {
                if let Some(idx) = self.reassign_state.selected() {
                    if let Some(account) = filtered.get(idx) {
                        if is_bulk {
                            // Bulk reassignment
                            let entry_ids: Vec<String> =
                                self.selected_entry_ids.iter().cloned().collect();
                            self.bulk_reassign_confirmed = Some((entry_ids, account.id.clone()));
                            self.exit_multiselect();
                        } else if let Some(ref pending) = self.reassign_pending {
                            // Single reassignment
                            self.reassign_confirmed = Some((
                                pending.entry_id.clone(),
                                pending.line_id.clone(),
                                account.id.clone(),
                            ));
                        }
                        self.cancel_reassign();
                    }
                }
            }
            KeyCode::Up => {
                if num_accounts > 0 {
                    let i = match self.reassign_state.selected() {
                        Some(i) if i > 0 => i - 1,
                        _ => num_accounts.saturating_sub(1),
                    };
                    self.reassign_state.select(Some(i));
                }
            }
            KeyCode::Down => {
                if num_accounts > 0 {
                    let i = match self.reassign_state.selected() {
                        Some(i) if i < num_accounts - 1 => i + 1,
                        _ => 0,
                    };
                    self.reassign_state.select(Some(i));
                }
            }
            KeyCode::Backspace => {
                self.reassign_filter.pop();
                self.reassign_state.select(Some(0));
            }
            KeyCode::Char(c) => {
                self.reassign_filter.push(c);
                self.reassign_state.select(Some(0));
            }
            _ => {}
        }
        false
    }

    fn next(&mut self) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i >= count - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        self.adjust_scroll_down(i);
    }

    fn previous(&mut self) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    count - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
        self.adjust_scroll_up(i);
    }

    fn first(&mut self) {
        if self.visible_count() > 0 {
            self.state.select(Some(0));
            *self.state.offset_mut() = 0;
        }
    }

    fn last(&mut self) {
        let count = self.visible_count();
        if count > 0 {
            let last = count - 1;
            self.state.select(Some(last));
            // Set offset so last item is at bottom
            let offset = last.saturating_sub(self.visible_height.saturating_sub(1));
            *self.state.offset_mut() = offset;
        }
    }

    /// Adjust scroll offset when moving down (80% margin)
    fn adjust_scroll_down(&mut self, selected: usize) {
        if self.visible_height == 0 {
            return;
        }
        let count = self.visible_count();
        let offset = self.state.offset();

        // If we wrapped to the beginning, reset offset
        if selected == 0 {
            *self.state.offset_mut() = 0;
            return;
        }

        // Calculate position relative to visible area
        let pos_in_view = selected.saturating_sub(offset);
        let threshold = (self.visible_height * 80) / 100;

        // If cursor is past 80% of visible area, scroll down
        if pos_in_view >= threshold {
            let new_offset = selected.saturating_sub(threshold);
            let max_offset = count.saturating_sub(self.visible_height);
            *self.state.offset_mut() = new_offset.min(max_offset);
        }
    }

    /// Adjust scroll offset when moving up (20% margin)
    fn adjust_scroll_up(&mut self, selected: usize) {
        if self.visible_height == 0 {
            return;
        }
        let count = self.visible_count();
        let offset = self.state.offset();

        // If we wrapped to the end, set offset to show last items
        if selected == count - 1 {
            let new_offset = count.saturating_sub(self.visible_height);
            *self.state.offset_mut() = new_offset;
            return;
        }

        // Calculate position relative to visible area
        let pos_in_view = selected.saturating_sub(offset);
        let threshold = (self.visible_height * 20) / 100;

        // If cursor is before 20% of visible area, scroll up
        if pos_in_view <= threshold && offset > 0 {
            let scroll_amount = threshold.saturating_sub(pos_in_view) + 1;
            *self.state.offset_mut() = offset.saturating_sub(scroll_amount);
        }
    }

    fn page_up(&mut self) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let page_size = self.visible_height.max(10);
        let i = match self.state.selected() {
            Some(i) => i.saturating_sub(page_size),
            None => 0,
        };
        self.state.select(Some(i));
        // Move offset by same amount
        let offset = self.state.offset();
        *self.state.offset_mut() = offset.saturating_sub(page_size);
    }

    fn page_down(&mut self) {
        let count = self.visible_count();
        if count == 0 {
            return;
        }
        let page_size = self.visible_height.max(10);
        let i = match self.state.selected() {
            Some(i) => (i + page_size).min(count - 1),
            None => 0,
        };
        self.state.select(Some(i));
        // Move offset by same amount, but don't exceed max
        let offset = self.state.offset();
        let max_offset = count.saturating_sub(self.visible_height);
        *self.state.offset_mut() = (offset + page_size).min(max_offset);
    }

    fn get_title(&self) -> String {
        // Show void/unvoid confirmation prompt
        if self.is_confirming_void() {
            if self.is_pending_unvoid() {
                return " UNVOID ENTRY? (y: confirm, n/Enter: cancel) ".to_string();
            } else {
                return " VOID ENTRY? (y: confirm, n/Enter: cancel) ".to_string();
            }
        }

        // Show bulk void confirmation prompt
        if self.is_confirming_bulk_void() {
            let count = self.selected_entry_ids.len();
            return format!(
                " You're going to void {} transactions. Proceed? (y: confirm, n/Enter: cancel) ",
                count
            );
        }

        let sort_info = format!(
            "[{} {}]",
            self.sort_field.label(),
            self.sort_direction.symbol()
        );
        let void_info = if self.show_void { "" } else { " [hiding void]" };

        if let Some(ref account) = self.filter_account {
            if self.multiselect_mode {
                let count = self.selected_entry_ids.len();
                let select_status = if self.multiselect_selecting {
                    "selecting"
                } else {
                    "paused"
                };
                format!(
                    " MULTISELECT ({} selected, {}) - ↑↓: move, Space: toggle select, a: assign, x: void, Esc: cancel ",
                    count, select_status
                )
            } else {
                format!(
                    " Ledger: {} - {} {}{} ",
                    account.account_number, account.name, sort_info, void_info
                )
            }
        } else {
            format!(" Journal Entries {}{} ", sort_info, void_info)
        }
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let is_ledger = self.filter_account.is_some();

        // Calculate and store visible height for scroll margin calculations
        // Area height minus borders (2) minus header row (1) minus header margin (1)
        self.visible_height = area.height.saturating_sub(4) as usize;

        // Filter entries based on show_void setting
        let visible: Vec<(usize, &EntrySearchResult)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| self.show_void || !e.is_void)
            .collect();

        let rows: Vec<Row> = visible
            .iter()
            .map(|(original_idx, entry)| {
                let is_selected = self.selected_entry_ids.contains(&entry.entry_id);
                let status = if entry.is_void {
                    "VOID"
                } else if is_selected {
                    "✓"
                } else {
                    ""
                };

                let style = if entry.is_void {
                    Style::default().fg(Color::DarkGray)
                } else if is_selected {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                };

                if is_ledger {
                    // Ledger view columns
                    let amount = entry.account_amount.unwrap_or(0);
                    let (debit, credit) = if amount > 0 {
                        (format_currency(amount), String::new())
                    } else if amount < 0 {
                        (String::new(), format_currency(-amount))
                    } else {
                        (String::new(), String::new())
                    };
                    let balance = self
                        .running_balances
                        .get(*original_idx)
                        .copied()
                        .unwrap_or(0);
                    let other_account = entry.other_account.clone().unwrap_or_default();

                    let mut cells = Vec::new();
                    if self.show_id_column {
                        cells.push(entry.entry_id[..8].to_string());
                    }
                    cells.extend([
                        entry.date.format("%Y-%m-%d").to_string(),
                        other_account,
                        entry.memo.clone(),
                        entry.reference.clone().unwrap_or_default(),
                        debit,
                        credit,
                        format_currency(balance),
                        status.to_string(),
                    ]);
                    Row::new(cells).style(style)
                } else {
                    // Journal view columns
                    let mut cells = Vec::new();
                    if self.show_id_column {
                        cells.push(entry.entry_id[..8].to_string());
                    }
                    cells.extend([
                        entry.date.format("%Y-%m-%d").to_string(),
                        entry.memo.clone(),
                        entry.reference.clone().unwrap_or_default(),
                        format_currency(entry.total_amount),
                        status.to_string(),
                    ]);
                    Row::new(cells).style(style)
                }
            })
            .collect();

        let (header, widths): (Row, Vec<Constraint>) = if is_ledger {
            let mut headers: Vec<&str> = Vec::new();
            let mut constraints = Vec::new();

            if self.show_id_column {
                headers.push("ID");
                constraints.push(Constraint::Length(10));
            }
            headers.extend([
                "Date",
                "Account",
                "Memo",
                "Reference",
                "Debit",
                "Credit",
                "Balance",
                "Status",
            ]);
            constraints.extend([
                Constraint::Length(12),
                Constraint::Length(20), // Account column
                Constraint::Min(15),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(6),
            ]);

            (
                Row::new(headers)
                    .style(
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Yellow),
                    )
                    .bottom_margin(1),
                constraints,
            )
        } else {
            let mut headers: Vec<&str> = Vec::new();
            let mut constraints = Vec::new();

            if self.show_id_column {
                headers.push("ID");
                constraints.push(Constraint::Length(10));
            }
            headers.extend(["Date", "Memo", "Reference", "Amount", "Status"]);
            constraints.extend([
                Constraint::Length(12),
                Constraint::Min(25),
                Constraint::Length(15),
                Constraint::Length(15),
                Constraint::Length(8),
            ]);

            (
                Row::new(headers)
                    .style(
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Yellow),
                    )
                    .bottom_margin(1),
                constraints,
            )
        };

        let table = Table::new(rows, widths)
            .header(header)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(self.get_title()),
            )
            .row_highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_stateful_widget(table, area, &mut self.state.clone());

        // Draw reassignment modal if active
        if self.reassign_mode {
            self.draw_reassign_modal(frame, area);
        }
    }

    fn draw_reassign_modal(&self, frame: &mut Frame, area: Rect) {
        let modal_area = centered_rect(60, 60, area);
        frame.render_widget(Clear, modal_area);

        let is_bulk = self.reassign_pending.is_none();

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3), // Title/current account
                Constraint::Length(3), // Filter input
                Constraint::Min(5),    // Account list
                Constraint::Length(2), // Help
            ])
            .split(modal_area);

        let title = if is_bulk {
            format!(" Assign {} Transactions ", self.selected_entry_ids.len())
        } else {
            " Reassign Transaction ".to_string()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title);
        frame.render_widget(block, modal_area);

        // Current account info
        let current_text = if is_bulk {
            format!(
                "Assigning {} transactions to new account",
                self.selected_entry_ids.len()
            )
        } else if let Some(ref pending) = self.reassign_pending {
            format!("Current: {}", pending.current_account_name)
        } else {
            "Select new account".to_string()
        };
        let current = Paragraph::new(current_text).style(Style::default().fg(Color::Yellow));
        frame.render_widget(current, chunks[0]);

        // Filter input
        let filter_text = format!("Filter: {}▏", self.reassign_filter);
        let filter = Paragraph::new(filter_text).block(Block::default().borders(Borders::ALL));
        frame.render_widget(filter, chunks[1]);

        // Account list
        let filtered = self.filtered_reassign_accounts();
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|a| {
                let text = format!("{} - {}", a.account_number, a.name);
                ListItem::new(text)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Accounts "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_stateful_widget(list, chunks[2], &mut self.reassign_state.clone());

        // Help
        let help_text = Line::from(vec![
            Span::styled("↑↓", Style::default().fg(Color::Yellow)),
            Span::raw(": select  "),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::raw(": confirm  "),
            Span::styled("Type", Style::default().fg(Color::Yellow)),
            Span::raw(": filter  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(": cancel"),
        ]);
        let help = Paragraph::new(help_text);
        frame.render_widget(help, chunks[3]);
    }
}

impl Default for JournalView {
    fn default() -> Self {
        Self::new()
    }
}

fn format_currency(cents: i64) -> String {
    let dollars = cents as f64 / 100.0;
    format!("${:.2}", dollars)
}

/// Helper function to create a centered rectangle
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
