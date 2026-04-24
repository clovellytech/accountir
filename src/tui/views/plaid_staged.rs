use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::tui::theme::Theme;

pub struct PlaidStagedView {
    pub transfer_candidates: Vec<TransferCandidateDisplay>,
    pub unmatched: Vec<StagedTransactionDisplay>,
    pub section: StagedSection,
    pub transfer_index: usize,
    pub unmatched_index: usize,
    pub visible: bool,
    pub status_message: Option<String>,
    transfer_scroll: usize,
    unmatched_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedSection {
    Transfers,
    Unmatched,
}

pub struct TransferCandidateDisplay {
    pub candidate_id: String,
    pub txn1_name: String,
    pub txn1_account: String,
    pub txn1_date: String,
    pub txn1_amount_cents: i64,
    pub txn2_name: String,
    pub txn2_account: String,
    pub txn2_date: String,
    pub txn2_amount_cents: i64,
    pub confidence: f64,
}

pub struct StagedTransactionDisplay {
    pub id: String,
    pub date: String,
    pub name: String,
    pub account_name: String,
    pub amount_cents: i64,
    pub card_holder: Option<String>,
}

pub enum StagedAction {
    None,
    ConfirmTransfer(String),
    RejectTransfer(String),
    ConfirmAllTransfers,
    ImportUnmatched(String),
    ImportAll,
    ImportAllSkipBalanceCheck,
    Back,
}

impl Default for PlaidStagedView {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaidStagedView {
    pub fn new() -> Self {
        Self {
            transfer_candidates: Vec::new(),
            unmatched: Vec::new(),
            section: StagedSection::Transfers,
            transfer_index: 0,
            unmatched_index: 0,
            visible: false,
            status_message: None,
            transfer_scroll: 0,
            unmatched_scroll: 0,
        }
    }

    pub fn show(&mut self) {
        self.visible = true;
        self.section = StagedSection::Transfers;
        self.transfer_index = 0;
        self.unmatched_index = 0;
        self.transfer_scroll = 0;
        self.unmatched_scroll = 0;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn total_pending(&self) -> usize {
        self.transfer_candidates.len() + self.unmatched.len()
    }

    pub fn handle_key(&mut self, key: KeyCode) -> StagedAction {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => StagedAction::Back,
            KeyCode::Tab => {
                self.section = match self.section {
                    StagedSection::Transfers => StagedSection::Unmatched,
                    StagedSection::Unmatched => StagedSection::Transfers,
                };
                StagedAction::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                match self.section {
                    StagedSection::Transfers => {
                        if !self.transfer_candidates.is_empty() {
                            self.transfer_index =
                                (self.transfer_index + 1).min(self.transfer_candidates.len() - 1);
                        }
                    }
                    StagedSection::Unmatched => {
                        if !self.unmatched.is_empty() {
                            self.unmatched_index =
                                (self.unmatched_index + 1).min(self.unmatched.len() - 1);
                        }
                    }
                }
                StagedAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                match self.section {
                    StagedSection::Transfers => {
                        if self.transfer_index > 0 {
                            self.transfer_index -= 1;
                        }
                    }
                    StagedSection::Unmatched => {
                        if self.unmatched_index > 0 {
                            self.unmatched_index -= 1;
                        }
                    }
                }
                StagedAction::None
            }
            KeyCode::Enter | KeyCode::Char('y') => match self.section {
                StagedSection::Transfers => {
                    if let Some(c) = self.transfer_candidates.get(self.transfer_index) {
                        StagedAction::ConfirmTransfer(c.candidate_id.clone())
                    } else {
                        StagedAction::None
                    }
                }
                StagedSection::Unmatched => {
                    if let Some(t) = self.unmatched.get(self.unmatched_index) {
                        StagedAction::ImportUnmatched(t.id.clone())
                    } else {
                        StagedAction::None
                    }
                }
            },
            KeyCode::Char('n') => {
                if self.section == StagedSection::Transfers {
                    if let Some(c) = self.transfer_candidates.get(self.transfer_index) {
                        return StagedAction::RejectTransfer(c.candidate_id.clone());
                    }
                }
                StagedAction::None
            }
            KeyCode::Char('A') => StagedAction::ConfirmAllTransfers,
            KeyCode::Char('I') => StagedAction::ImportAll,
            _ => StagedAction::None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // summary
                Constraint::Min(5),    // transfers
                Constraint::Min(5),    // unmatched
                Constraint::Length(1), // help
            ])
            .split(area);

        // Summary line
        let summary = Line::from(vec![Span::styled(
            format!(
                " {} transfer candidates, {} unmatched transactions ",
                self.transfer_candidates.len(),
                self.unmatched.len()
            ),
            Style::default().fg(theme.header),
        )]);
        frame.render_widget(Paragraph::new(summary), chunks[0]);

        // Transfers section
        let transfers_active = self.section == StagedSection::Transfers;
        let transfers_border_color = if transfers_active {
            theme.accent
        } else {
            theme.fg_dim
        };
        let transfers_block = Block::default()
            .borders(Borders::ALL)
            .title(" Transfer Candidates (Enter: confirm, n: reject, A: confirm all) ")
            .title_style(Style::default().fg(transfers_border_color))
            .border_style(Style::default().fg(transfers_border_color));

        if self.transfer_candidates.is_empty() {
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No transfer candidates detected.",
                Style::default().fg(theme.fg_dim),
            )))
            .block(transfers_block);
            frame.render_widget(msg, chunks[1]);
        } else {
            let header = Row::new(vec![
                Cell::from("From Account"),
                Cell::from("To Account"),
                Cell::from("Amount"),
                Cell::from("Date (From)"),
                Cell::from("Date (To)"),
                Cell::from("Confidence"),
            ])
            .style(
                Style::default()
                    .fg(theme.header)
                    .add_modifier(Modifier::BOLD),
            );

            // Calculate visible height (area minus borders and header) and adjust scroll
            let transfer_visible = chunks[1].height.saturating_sub(3) as usize;
            if transfer_visible > 0 {
                if self.transfer_index < self.transfer_scroll {
                    self.transfer_scroll = self.transfer_index;
                } else if self.transfer_index >= self.transfer_scroll + transfer_visible {
                    self.transfer_scroll = self.transfer_index.saturating_sub(transfer_visible - 1);
                }
            }

            let rows: Vec<Row> = self
                .transfer_candidates
                .iter()
                .enumerate()
                .skip(self.transfer_scroll)
                .take(transfer_visible)
                .map(|(i, c)| {
                    let (from_name, from_date, to_name, to_date, amount) =
                        if c.txn1_amount_cents < 0 {
                            (
                                &c.txn1_account,
                                &c.txn1_date,
                                &c.txn2_account,
                                &c.txn2_date,
                                c.txn1_amount_cents.unsigned_abs(),
                            )
                        } else {
                            (
                                &c.txn2_account,
                                &c.txn2_date,
                                &c.txn1_account,
                                &c.txn1_date,
                                c.txn2_amount_cents.unsigned_abs(),
                            )
                        };

                    let style = if transfers_active && i == self.transfer_index {
                        theme.selected_style()
                    } else {
                        Style::default()
                    };

                    let confidence_color = if c.confidence >= 0.75 {
                        theme.success
                    } else if c.confidence >= 0.5 {
                        theme.header
                    } else {
                        theme.error
                    };

                    Row::new(vec![
                        Cell::from(from_name.clone()),
                        Cell::from(to_name.clone()),
                        Cell::from(format_amount(amount as i64)),
                        Cell::from(from_date.clone()),
                        Cell::from(to_date.clone()),
                        Cell::from(Span::styled(
                            format!("{:.0}%", c.confidence * 100.0),
                            Style::default().fg(confidence_color),
                        )),
                    ])
                    .style(style)
                })
                .collect();

            let table = Table::new(
                rows,
                [
                    Constraint::Percentage(22),
                    Constraint::Percentage(22),
                    Constraint::Percentage(14),
                    Constraint::Percentage(14),
                    Constraint::Percentage(14),
                    Constraint::Percentage(14),
                ],
            )
            .header(header)
            .block(transfers_block);

            frame.render_widget(table, chunks[1]);
        }

        // Unmatched section
        let unmatched_active = self.section == StagedSection::Unmatched;
        let unmatched_border_color = if unmatched_active {
            theme.accent
        } else {
            theme.fg_dim
        };
        let unmatched_block = Block::default()
            .borders(Borders::ALL)
            .title(" Unmatched Transactions (Enter: import, I: import all) ")
            .title_style(Style::default().fg(unmatched_border_color))
            .border_style(Style::default().fg(unmatched_border_color));

        if self.unmatched.is_empty() {
            let msg = Paragraph::new(Line::from(Span::styled(
                "  No unmatched staged transactions.",
                Style::default().fg(theme.fg_dim),
            )))
            .block(unmatched_block);
            frame.render_widget(msg, chunks[2]);
        } else {
            let has_card_holders = self.unmatched.iter().any(|t| t.card_holder.is_some());

            let header_cells = if has_card_holders {
                vec![
                    Cell::from("Date"),
                    Cell::from("Name"),
                    Cell::from("Account"),
                    Cell::from("Card"),
                    Cell::from("Amount"),
                ]
            } else {
                vec![
                    Cell::from("Date"),
                    Cell::from("Name"),
                    Cell::from("Account"),
                    Cell::from("Amount"),
                ]
            };
            let header = Row::new(header_cells).style(
                Style::default()
                    .fg(theme.header)
                    .add_modifier(Modifier::BOLD),
            );

            // Calculate visible height (area minus borders and header) and adjust scroll
            let unmatched_visible = chunks[2].height.saturating_sub(3) as usize;
            if unmatched_visible > 0 {
                if self.unmatched_index < self.unmatched_scroll {
                    self.unmatched_scroll = self.unmatched_index;
                } else if self.unmatched_index >= self.unmatched_scroll + unmatched_visible {
                    self.unmatched_scroll =
                        self.unmatched_index.saturating_sub(unmatched_visible - 1);
                }
            }

            let rows: Vec<Row> = self
                .unmatched
                .iter()
                .enumerate()
                .skip(self.unmatched_scroll)
                .take(unmatched_visible)
                .map(|(i, t)| {
                    let style = if unmatched_active && i == self.unmatched_index {
                        theme.selected_style()
                    } else {
                        Style::default()
                    };

                    let amount_color = if t.amount_cents < 0 {
                        theme.success
                    } else {
                        theme.error
                    };

                    let mut cells = vec![
                        Cell::from(t.date.clone()),
                        Cell::from(truncate(&t.name, 35)),
                        Cell::from(t.account_name.clone()),
                    ];
                    if has_card_holders {
                        cells.push(Cell::from(Span::styled(
                            t.card_holder.as_deref().unwrap_or(""),
                            Style::default().fg(theme.fg_dim),
                        )));
                    }
                    cells.push(Cell::from(Span::styled(
                        format_amount(t.amount_cents),
                        Style::default().fg(amount_color),
                    )));

                    Row::new(cells).style(style)
                })
                .collect();

            let widths = if has_card_holders {
                vec![
                    Constraint::Percentage(12),
                    Constraint::Percentage(30),
                    Constraint::Percentage(22),
                    Constraint::Percentage(18),
                    Constraint::Percentage(18),
                ]
            } else {
                vec![
                    Constraint::Percentage(15),
                    Constraint::Percentage(40),
                    Constraint::Percentage(25),
                    Constraint::Percentage(20),
                ]
            };

            let table = Table::new(rows, widths)
                .header(header)
                .block(unmatched_block);

            frame.render_widget(table, chunks[2]);
        }

        // Help line
        let help = Line::from(vec![
            Span::styled("Tab", Style::default().fg(theme.header)),
            Span::raw(": switch section  "),
            Span::styled("Enter/y", Style::default().fg(theme.header)),
            Span::raw(": confirm  "),
            Span::styled("n", Style::default().fg(theme.header)),
            Span::raw(": reject  "),
            Span::styled("A", Style::default().fg(theme.header)),
            Span::raw(": confirm all transfers  "),
            Span::styled("I", Style::default().fg(theme.header)),
            Span::raw(": import all  "),
            Span::styled("Esc", Style::default().fg(theme.header)),
            Span::raw(": back"),
        ]);
        frame.render_widget(Paragraph::new(help), chunks[3]);

        // Status message overlay
        if let Some(msg) = &self.status_message {
            let status_area = Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(2),
                width: area.width,
                height: 1,
            };
            let status = Paragraph::new(Span::styled(
                msg.as_str(),
                Style::default().fg(theme.accent),
            ));
            frame.render_widget(status, status_area);
        }
    }
}

fn format_amount(cents: i64) -> String {
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let remainder = abs % 100;
    if cents < 0 {
        format!("({}.{:02})", dollars, remainder)
    } else {
        format!("{}.{:02}", dollars, remainder)
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
