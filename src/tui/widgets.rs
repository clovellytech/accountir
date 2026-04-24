use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::tui::theme::Theme;

pub struct TextField<'a> {
    pub area: Rect,
    pub label: &'a str,
    pub value: &'a str,
    pub is_secret: bool,
    pub is_active: bool,
    pub cursor: char,
}

impl<'a> TextField<'a> {
    pub fn new(area: Rect, label: &'a str, value: &'a str, is_active: bool) -> Self {
        Self {
            area,
            label,
            value,
            is_secret: false,
            is_active,
            cursor: '█',
        }
    }

    pub fn secret(mut self) -> Self {
        self.is_secret = true;
        self
    }

    pub fn cursor(mut self, cursor: char) -> Self {
        self.cursor = cursor;
        self
    }
}

pub fn draw_text_field(frame: &mut Frame, field: &TextField, theme: &Theme) {
    let style = if field.is_active {
        Style::default().fg(theme.input_active_fg)
    } else {
        Style::default().fg(theme.input_inactive_fg)
    };

    let border_style = if field.is_active {
        Style::default().fg(theme.input_active_border)
    } else {
        Style::default().fg(theme.input_inactive_border)
    };

    let displayed_value = if field.is_secret {
        "*".repeat(field.value.len())
    } else {
        field.value.to_string()
    };

    let display = if field.is_active {
        format!("{}{}", displayed_value, field.cursor)
    } else {
        displayed_value
    };

    let paragraph = Paragraph::new(display).style(style).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(format!(" {} ", field.label)),
    );

    frame.render_widget(paragraph, field.area);
}

pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
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

pub fn inner_rect(area: Rect, margin_x: u16, margin_y: u16) -> Rect {
    Rect {
        x: area.x + margin_x,
        y: area.y + margin_y,
        width: area.width.saturating_sub(margin_x * 2),
        height: area.height.saturating_sub(margin_y * 2),
    }
}

pub fn format_currency(cents: i64) -> String {
    let dollars = cents as f64 / 100.0;
    if cents < 0 {
        format!("(${:.2})", -dollars)
    } else {
        format!("${:.2}", dollars)
    }
}

pub fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}
