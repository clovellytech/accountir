use crossterm::event::KeyCode;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::config::AppConfig;
use crate::tui::theme::{Theme, ThemePreset};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsResult {
    None,
    Cancel,
    Saved(ThemePreset),
}

pub struct SettingsModal {
    pub visible: bool,
    pub result: SettingsResult,
    selected_preset: ThemePreset,
    original_preset: ThemePreset,
}

impl SettingsModal {
    pub fn new() -> Self {
        Self {
            visible: false,
            result: SettingsResult::None,
            selected_preset: ThemePreset::default(),
            original_preset: ThemePreset::default(),
        }
    }

    pub fn show(&mut self) {
        let config = AppConfig::load();
        self.selected_preset = config.theme;
        self.original_preset = config.theme;
        self.visible = true;
        self.result = SettingsResult::None;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    /// Get the currently previewed preset (for live preview while modal is open)
    pub fn preview_preset(&self) -> ThemePreset {
        self.selected_preset
    }

    pub fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Esc => {
                self.result = SettingsResult::Cancel;
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                self.selected_preset = self.selected_preset.next();
                // Live preview: emit a save result immediately
                self.result = SettingsResult::Saved(self.selected_preset);
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::BackTab => {
                self.selected_preset = self.selected_preset.prev();
                self.result = SettingsResult::Saved(self.selected_preset);
            }
            KeyCode::Enter => {
                self.save();
            }
            _ => {}
        }
    }

    fn save(&mut self) {
        let mut config = AppConfig::load();
        config.theme = self.selected_preset;
        if config.save().is_ok() {
            self.result = SettingsResult::Saved(self.selected_preset);
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if !self.visible {
            return;
        }

        let modal_area = centered_rect(50, 50, area);
        frame.render_widget(Clear, modal_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.border_style())
            .title(" Settings ")
            .title_style(theme.modal_title_style());

        frame.render_widget(block, modal_area);

        let inner = inner_rect(modal_area, 2, 1);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // Section header
                Constraint::Length(1), // Spacer
                Constraint::Min(6),   // Theme options
                Constraint::Length(1), // Spacer
                Constraint::Length(2), // Preview info
                Constraint::Length(1), // Spacer
                Constraint::Length(1), // Help
                Constraint::Min(0),
            ])
            .split(inner);

        // Section header
        let header = Paragraph::new(Line::from(Span::styled(
            "  Theme",
            theme.header_style(),
        )));
        frame.render_widget(header, chunks[0]);

        // Theme options
        let presets = ThemePreset::ALL;
        let mut lines = Vec::new();
        for preset in presets {
            let is_selected = *preset == self.selected_preset;
            let marker = if is_selected { " > " } else { "   " };
            let name_style = if is_selected {
                theme.selected_style()
            } else {
                theme.text_style()
            };
            let desc_style = if is_selected {
                Style::default()
                    .fg(theme.fg_selected)
                    .bg(theme.bg_selected)
            } else {
                theme.dim_style()
            };

            lines.push(Line::from(vec![
                Span::styled(marker, name_style),
                Span::styled(format!("{:<16}", preset.name()), name_style),
                Span::styled(preset.description(), desc_style),
            ]));
            lines.push(Line::from(""));
        }
        let options = Paragraph::new(lines);
        frame.render_widget(options, chunks[2]);

        // Preview hint
        let preview = Paragraph::new(Line::from(vec![
            Span::styled("  Preview: ", theme.dim_style()),
            Span::styled("Colors update as you browse", theme.info_style()),
        ]));
        frame.render_widget(preview, chunks[4]);

        // Help line
        let help = Paragraph::new(Line::from(vec![
            Span::styled("  ←/→", Style::default().fg(theme.header)),
            Span::raw(": browse  "),
            Span::styled("Enter", Style::default().fg(theme.header)),
            Span::raw(": save  "),
            Span::styled("Esc", Style::default().fg(theme.header)),
            Span::raw(": cancel"),
        ]));
        frame.render_widget(help, chunks[6]);
    }
}

impl Default for SettingsModal {
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
