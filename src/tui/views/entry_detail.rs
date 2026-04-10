use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Row, Table},
    Frame,
};

use chrono::NaiveDate;

use crate::tui::theme::Theme;

#[derive(Debug, Clone)]
pub struct EntryLineDetail {
    pub account_number: String,
    pub account_name: String,
    pub debit: i64,
    pub credit: i64,
    pub memo: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EntryDetail {
    pub entry_id: String,
    pub date: NaiveDate,
    pub memo: String,
    pub reference: Option<String>,
    pub is_void: bool,
    pub lines: Vec<EntryLineDetail>,
}

pub struct EntryDetailModal {
    pub visible: bool,
    pub entry: Option<EntryDetail>,
    scroll_offset: usize,
}

impl EntryDetailModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            entry: None,
            scroll_offset: 0,
        }
    }

    pub fn show(&mut self, entry: EntryDetail) {
        self.entry = Some(entry);
        self.visible = true;
        self.scroll_offset = 0;
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.entry = None;
    }

    pub fn handle_key(&mut self, key: KeyCode) -> bool {
        match key {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                self.hide();
                true
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.scroll_offset > 0 {
                    self.scroll_offset -= 1;
                }
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_offset += 1;
                false
            }
            _ => false,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let Some(ref entry) = self.entry else {
            return;
        };

        let modal_area = centered_rect(70, 70, area);
        frame.render_widget(Clear, modal_area);

        let title = if entry.is_void {
            " Transaction Details [VOID] "
        } else {
            " Transaction Details "
        };

        let border_color = if entry.is_void {
            theme.error
        } else {
            theme.accent
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(title)
            .title_style(
                Style::default()
                    .fg(border_color)
                    .add_modifier(Modifier::BOLD),
            );

        frame.render_widget(block, modal_area);

        let inner = inner_rect(modal_area, 2, 1);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6), // Header info
                Constraint::Min(5),    // Lines table
                Constraint::Length(2), // Totals
                Constraint::Length(1), // Help
            ])
            .split(inner);

        // Header info
        let header_lines = vec![
            Line::from(vec![
                Span::styled("Date: ", Style::default().fg(theme.fg_dim)),
                Span::raw(entry.date.format("%Y-%m-%d").to_string()),
            ]),
            Line::from(vec![
                Span::styled("Entry ID: ", Style::default().fg(theme.fg_dim)),
                Span::raw(&entry.entry_id),
            ]),
            Line::from(vec![
                Span::styled("Reference: ", Style::default().fg(theme.fg_dim)),
                Span::raw(entry.reference.as_deref().unwrap_or("-")),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Memo: ", Style::default().fg(theme.fg_dim)),
                Span::raw(&entry.memo),
            ]),
        ];
        let header = Paragraph::new(header_lines);
        frame.render_widget(header, chunks[0]);

        // Lines table
        let rows: Vec<Row> = entry
            .lines
            .iter()
            .map(|line| {
                let debit_str = if line.debit > 0 {
                    format_currency(line.debit)
                } else {
                    String::new()
                };
                let credit_str = if line.credit > 0 {
                    format_currency(line.credit)
                } else {
                    String::new()
                };

                Row::new(vec![
                    line.account_number.clone(),
                    line.account_name.clone(),
                    debit_str,
                    credit_str,
                ])
            })
            .collect();

        let header_row = Row::new(vec!["Account #", "Account Name", "Debit", "Credit"])
            .style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(theme.header),
            )
            .bottom_margin(1);

        let table = Table::new(
            rows,
            [
                Constraint::Length(12),
                Constraint::Min(20),
                Constraint::Length(14),
                Constraint::Length(14),
            ],
        )
        .header(header_row)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.fg_dim))
                .title(" Line Items "),
        );

        frame.render_widget(table, chunks[1]);

        // Totals
        let total_debits: i64 = entry.lines.iter().map(|l| l.debit).sum();
        let total_credits: i64 = entry.lines.iter().map(|l| l.credit).sum();

        let totals = Paragraph::new(Line::from(vec![
            Span::styled("Totals: ", Style::default().fg(theme.fg_dim)),
            Span::styled("Debits ", Style::default().fg(theme.success)),
            Span::raw(format_currency(total_debits)),
            Span::raw("  "),
            Span::styled("Credits ", Style::default().fg(theme.error)),
            Span::raw(format_currency(total_credits)),
        ]));
        frame.render_widget(totals, chunks[2]);

        // Help
        let help = Paragraph::new(Line::from(vec![
            Span::styled("Esc/Enter/q", Style::default().fg(theme.header)),
            Span::raw(": close"),
        ]));
        frame.render_widget(help, chunks[3]);
    }
}

impl Default for EntryDetailModal {
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

fn format_currency(cents: i64) -> String {
    let dollars = cents as f64 / 100.0;
    format!("${:.2}", dollars)
}
