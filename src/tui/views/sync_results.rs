use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::commands::event_service_commands::{SyncEventResult, SyncEventStatus};
use crate::tui::theme::Theme;

pub struct SyncResultsModal {
    pub visible: bool,
    pub service_name: String,
    pub results: Vec<SyncEventResult>,
    pub summary: SyncSummary,
    scroll_offset: usize,
}

pub struct SyncSummary {
    pub events_processed: u32,
    pub entries_created: u32,
    pub errors: u32,
}

impl SyncResultsModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            service_name: String::new(),
            results: Vec::new(),
            summary: SyncSummary {
                events_processed: 0,
                entries_created: 0,
                errors: 0,
            },
            scroll_offset: 0,
        }
    }

    pub fn show(
        &mut self,
        service_name: String,
        results: Vec<SyncEventResult>,
        events_processed: u32,
        entries_created: u32,
        errors: u32,
    ) {
        self.service_name = service_name;
        self.results = results;
        self.summary = SyncSummary {
            events_processed,
            entries_created,
            errors,
        };
        self.scroll_offset = 0;
        self.visible = true;
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.results.clear();
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
                let max_scroll = self.results.len().saturating_sub(1);
                if self.scroll_offset < max_scroll {
                    self.scroll_offset += 1;
                }
                false
            }
            _ => false,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let modal_area = centered_rect(75, 70, area);
        frame.render_widget(Clear, modal_area);

        let has_errors = self.summary.errors > 0;
        let border_color = if has_errors { theme.error } else { theme.accent };

        let title = format!(" Sync Results: {} ", self.service_name);
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

        let inner = Rect {
            x: modal_area.x + 2,
            y: modal_area.y + 1,
            width: modal_area.width.saturating_sub(4),
            height: modal_area.height.saturating_sub(2),
        };

        let chunks = Layout::vertical([
            Constraint::Length(3), // Summary
            Constraint::Min(3),   // Event list
            Constraint::Length(1), // Help
        ])
        .split(inner);

        // Summary line
        let summary_line = Line::from(vec![
            Span::styled("Processed: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                self.summary.events_processed.to_string(),
                Style::default().fg(theme.fg),
            ),
            Span::raw("  "),
            Span::styled("Created: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                self.summary.entries_created.to_string(),
                Style::default().fg(theme.success),
            ),
            Span::raw("  "),
            Span::styled("Errors: ", Style::default().fg(theme.fg_dim)),
            Span::styled(
                self.summary.errors.to_string(),
                if self.summary.errors > 0 {
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.success)
                },
            ),
        ]);
        let summary = Paragraph::new(vec![Line::raw(""), summary_line]);
        frame.render_widget(summary, chunks[0]);

        // Event results list
        if self.results.is_empty() {
            let empty = Paragraph::new(Line::from(Span::styled(
                "No events processed.",
                Style::default().fg(theme.fg_dim),
            )));
            frame.render_widget(empty, chunks[1]);
        } else {
            let visible_height = chunks[1].height as usize;
            let max_scroll = self.results.len().saturating_sub(visible_height);
            let scroll = self.scroll_offset.min(max_scroll);

            let lines: Vec<Line> = self
                .results
                .iter()
                .skip(scroll)
                .take(visible_height)
                .map(|r| {
                    let id_short = if r.event_id.len() > 8 {
                        &r.event_id[..8]
                    } else {
                        &r.event_id
                    };

                    let (icon, status_text, status_style) = match &r.status {
                        SyncEventStatus::Created { entry_id } => {
                            let eid = if entry_id.len() > 8 {
                                &entry_id[..8]
                            } else {
                                entry_id
                            };
                            (
                                "+",
                                format!("Created entry {}", eid),
                                Style::default().fg(theme.success),
                            )
                        }
                        SyncEventStatus::Skipped { reason } => (
                            "-",
                            format!("Skipped: {}", reason),
                            Style::default().fg(Color::Yellow),
                        ),
                        SyncEventStatus::Error { message } => (
                            "!",
                            format!("Error: {}", message),
                            Style::default()
                                .fg(theme.error)
                                .add_modifier(Modifier::BOLD),
                        ),
                    };

                    Line::from(vec![
                        Span::styled(
                            format!(" {} ", icon),
                            status_style,
                        ),
                        Span::styled(
                            format!("{:<20} ", r.event_type),
                            Style::default().fg(theme.fg_dim),
                        ),
                        Span::styled(
                            format!("{:<10} ", id_short),
                            Style::default().fg(theme.fg_dim),
                        ),
                        Span::styled(status_text, status_style),
                    ])
                })
                .collect();

            let content = Paragraph::new(lines);
            frame.render_widget(content, chunks[1]);

            // Scrollbar
            if self.results.len() > visible_height {
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
                let mut scrollbar_state = ScrollbarState::new(self.results.len())
                    .position(scroll);
                let scrollbar_area = Rect {
                    x: chunks[1].x + chunks[1].width.saturating_sub(1),
                    y: chunks[1].y,
                    width: 1,
                    height: chunks[1].height,
                };
                frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
            }
        }

        // Help line
        let help = Paragraph::new(Line::from(Span::styled(
            " Esc/Enter: close  j/k: scroll ",
            Style::default().fg(theme.fg_dim),
        )));
        frame.render_widget(help, chunks[2]);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let popup_height = area.height * percent_y / 100;
    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_height)) / 2;
    Rect::new(x, y, popup_width, popup_height)
}
