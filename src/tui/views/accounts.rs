use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Rect},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Row, Table, TableState},
    Frame,
};

use std::collections::HashMap;

use crate::domain::{Account, AccountType};
use crate::queries::account_queries::AccountBalance;

/// Summary of a Plaid mapping for display in the accounts table
#[derive(Debug, Clone)]
pub struct PlaidMappingSummary {
    pub institution_name: String,
    pub mask: Option<String>,
}

/// An account with its tree depth for display purposes
#[derive(Debug, Clone)]
struct TreeNode {
    account: Account,
    depth: usize,
    is_last_child: bool,
    ancestor_is_last: Vec<bool>, // Track which ancestors are last children (for tree lines)
}

pub struct AccountsView {
    pub accounts: Vec<Account>,
    pub balances: Vec<AccountBalance>,
    pub plaid_mappings: HashMap<String, PlaidMappingSummary>, // local_account_id -> summary
    pub state: TableState,
    pub selected_account: Option<Account>, // Set when Enter is pressed

    // Tree structure for display
    tree_nodes: Vec<TreeNode>,
}

impl AccountsView {
    pub fn new() -> Self {
        Self {
            accounts: Vec::new(),
            balances: Vec::new(),
            plaid_mappings: HashMap::new(),
            state: TableState::default(),
            selected_account: None,
            tree_nodes: Vec::new(),
        }
    }

    /// Set accounts and rebuild the tree structure
    pub fn set_accounts(&mut self, accounts: Vec<Account>) {
        self.accounts = accounts;
        self.rebuild_tree();

        // Reset selection if needed
        if self.tree_nodes.is_empty() {
            self.state.select(None);
        } else if self.state.selected().is_none() {
            self.state.select(Some(0));
        } else if let Some(i) = self.state.selected() {
            if i >= self.tree_nodes.len() {
                self.state.select(Some(self.tree_nodes.len() - 1));
            }
        }
    }

    /// Rebuild the tree structure from the flat accounts list
    fn rebuild_tree(&mut self) {
        self.tree_nodes.clear();

        // Build a map of parent_id -> children (using indices to avoid borrow issues)
        let mut children_map: std::collections::HashMap<Option<String>, Vec<usize>> =
            std::collections::HashMap::new();

        for (idx, account) in self.accounts.iter().enumerate() {
            children_map
                .entry(account.parent_id.clone())
                .or_default()
                .push(idx);
        }

        // Sort children by account number within each group
        for children in children_map.values_mut() {
            children.sort_by(|&a, &b| {
                self.accounts[a]
                    .account_number
                    .cmp(&self.accounts[b].account_number)
            });
        }

        // Build tree starting from root accounts (parent_id = None)
        // We collect nodes first to avoid borrowing issues
        let mut pending_nodes: Vec<(usize, usize, bool, Vec<bool>)> = Vec::new(); // (account_idx, depth, is_last, ancestor_is_last)

        fn collect_nodes(
            accounts: &[Account],
            children_map: &std::collections::HashMap<Option<String>, Vec<usize>>,
            parent_id: Option<String>,
            depth: usize,
            ancestor_is_last: &[bool],
            result: &mut Vec<(usize, usize, bool, Vec<bool>)>,
        ) {
            if let Some(children) = children_map.get(&parent_id) {
                let child_count = children.len();
                for (i, &account_idx) in children.iter().enumerate() {
                    let is_last = i == child_count - 1;
                    result.push((account_idx, depth, is_last, ancestor_is_last.to_vec()));

                    let mut new_ancestor_is_last = ancestor_is_last.to_vec();
                    new_ancestor_is_last.push(is_last);

                    collect_nodes(
                        accounts,
                        children_map,
                        Some(accounts[account_idx].id.clone()),
                        depth + 1,
                        &new_ancestor_is_last,
                        result,
                    );
                }
            }
        }

        collect_nodes(
            &self.accounts,
            &children_map,
            None,
            0,
            &[],
            &mut pending_nodes,
        );

        // Now build tree_nodes from the collected data
        for (account_idx, depth, is_last_child, ancestor_is_last) in pending_nodes {
            self.tree_nodes.push(TreeNode {
                account: self.accounts[account_idx].clone(),
                depth,
                is_last_child,
                ancestor_is_last,
            });
        }
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up | KeyCode::Char('k') => self.previous(),
            KeyCode::Down | KeyCode::Char('j') => self.next(),
            KeyCode::Home => self.first(),
            KeyCode::End => self.last(),
            KeyCode::Enter => self.select_current(),
            _ => {}
        }
    }

    fn select_current(&mut self) {
        if let Some(i) = self.state.selected() {
            if let Some(node) = self.tree_nodes.get(i) {
                self.selected_account = Some(node.account.clone());
            }
        }
    }

    /// Get the currently selected account (without clearing)
    pub fn get_selected_account(&self) -> Option<&Account> {
        self.state
            .selected()
            .and_then(|i| self.tree_nodes.get(i))
            .map(|node| &node.account)
    }

    /// Get and clear the selected account
    pub fn take_selected_account(&mut self) -> Option<Account> {
        self.selected_account.take()
    }

    fn next(&mut self) {
        if self.tree_nodes.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.tree_nodes.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.tree_nodes.is_empty() {
            return;
        }
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.tree_nodes.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn first(&mut self) {
        if !self.tree_nodes.is_empty() {
            self.state.select(Some(0));
        }
    }

    fn last(&mut self) {
        if !self.tree_nodes.is_empty() {
            self.state.select(Some(self.tree_nodes.len() - 1));
        }
    }

    fn get_balance(&self, account_id: &str) -> i64 {
        self.balances
            .iter()
            .find(|b| b.account_id == account_id)
            .map(|b| b.balance)
            .unwrap_or(0)
    }

    /// Generate tree prefix for a node (e.g., "  ├── " or "  └── ")
    fn tree_prefix(&self, node: &TreeNode) -> String {
        if node.depth == 0 {
            return String::new();
        }

        let mut prefix = String::new();

        // Add continuation lines for ancestors
        for &ancestor_is_last in &node.ancestor_is_last {
            if ancestor_is_last {
                prefix.push_str("   "); // Space where the line would be
            } else {
                prefix.push_str("│  "); // Vertical line continuing down
            }
        }

        // Add the branch for this node
        if node.is_last_child {
            prefix.push_str("└─ ");
        } else {
            prefix.push_str("├─ ");
        }

        prefix
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let rows: Vec<Row> = self
            .tree_nodes
            .iter()
            .map(|node| {
                let acc = &node.account;
                let balance = self.get_balance(&acc.id);
                let display_balance = match acc.account_type {
                    AccountType::Asset | AccountType::Expense => balance,
                    AccountType::Liability | AccountType::Equity | AccountType::Revenue => -balance,
                };

                let type_str = match acc.account_type {
                    AccountType::Asset => "Asset",
                    AccountType::Liability => "Liability",
                    AccountType::Equity => "Equity",
                    AccountType::Revenue => "Revenue",
                    AccountType::Expense => "Expense",
                };

                let status = if acc.is_active { "Active" } else { "Inactive" };

                let plaid_col = self
                    .plaid_mappings
                    .get(&acc.id)
                    .map(|m| {
                        let mask_str = m
                            .mask
                            .as_deref()
                            .map(|mk| format!(" ***{}", mk))
                            .unwrap_or_default();
                        format!("{}{}", m.institution_name, mask_str)
                    })
                    .unwrap_or_default();

                let style = if !acc.is_active {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };

                // Build the name with tree prefix
                let tree_prefix = self.tree_prefix(node);
                let name_with_tree = format!("{}{}", tree_prefix, acc.name);

                Row::new(vec![
                    acc.account_number.clone(),
                    name_with_tree,
                    type_str.to_string(),
                    format_currency(display_balance),
                    plaid_col,
                    status.to_string(),
                ])
                .style(style)
            })
            .collect();

        let header = Row::new(vec!["Number", "Name", "Type", "Balance", "Plaid", "Status"])
            .style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Yellow),
            )
            .bottom_margin(1);

        let table = Table::new(
            rows,
            [
                Constraint::Length(10),
                Constraint::Min(25),
                Constraint::Length(12),
                Constraint::Length(15),
                Constraint::Length(20),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Chart of Accounts (a: new, e: edit, p: plaid link, Enter: view ledger) "),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

        frame.render_stateful_widget(table, area, &mut self.state.clone());
    }
}

impl Default for AccountsView {
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
