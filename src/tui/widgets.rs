use ratatui::{
    layout::Rect,
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
