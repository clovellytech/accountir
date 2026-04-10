use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};

/// Named theme presets
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    Dark,
    Light,
    HighContrast,
}

impl ThemePreset {
    pub const ALL: &'static [ThemePreset] = &[
        ThemePreset::Dark,
        ThemePreset::Light,
        ThemePreset::HighContrast,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            ThemePreset::Dark => "Dark",
            ThemePreset::Light => "Light",
            ThemePreset::HighContrast => "High Contrast",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ThemePreset::Dark => "Light text on dark backgrounds",
            ThemePreset::Light => "Dark text on light backgrounds",
            ThemePreset::HighContrast => "Maximum contrast for visibility",
        }
    }

    pub fn next(&self) -> Self {
        let all = Self::ALL;
        let idx = all.iter().position(|p| p == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    pub fn prev(&self) -> Self {
        let all = Self::ALL;
        let idx = all.iter().position(|p| p == self).unwrap_or(0);
        all[(idx + all.len() - 1) % all.len()]
    }
}

impl Default for ThemePreset {
    fn default() -> Self {
        ThemePreset::Dark
    }
}

/// Color palette used throughout the TUI
#[derive(Debug, Clone)]
pub struct Theme {
    // Text
    pub fg: Color,
    pub fg_dim: Color,
    pub fg_disabled: Color,

    // Backgrounds
    pub bg_selected: Color,
    pub fg_selected: Color,

    // Accents
    pub accent: Color,       // borders, active elements (cyan/blue)
    pub header: Color,       // column headers, section titles (yellow)
    pub highlight: Color,    // active tab, selected tab text
    pub success: Color,      // positive values, success indicators
    pub error: Color,        // errors, voided entries, negative
    pub info: Color,         // informational text

    // Input fields
    pub input_active_border: Color,
    pub input_active_fg: Color,
    pub input_inactive_border: Color,
    pub input_inactive_fg: Color,
}

impl Theme {
    pub fn from_preset(preset: ThemePreset) -> Self {
        match preset {
            ThemePreset::Dark => Self::dark(),
            ThemePreset::Light => Self::light(),
            ThemePreset::HighContrast => Self::high_contrast(),
        }
    }

    pub fn dark() -> Self {
        Self {
            fg: Color::White,
            fg_dim: Color::DarkGray,
            fg_disabled: Color::DarkGray,

            bg_selected: Color::DarkGray,
            fg_selected: Color::White,

            accent: Color::Cyan,
            header: Color::Yellow,
            highlight: Color::Yellow,
            success: Color::Green,
            error: Color::Red,
            info: Color::Cyan,

            input_active_border: Color::Yellow,
            input_active_fg: Color::Yellow,
            input_inactive_border: Color::DarkGray,
            input_inactive_fg: Color::White,
        }
    }

    pub fn light() -> Self {
        Self {
            fg: Color::Black,
            fg_dim: Color::DarkGray,
            fg_disabled: Color::Gray,

            bg_selected: Color::Rgb(200, 200, 220),
            fg_selected: Color::Black,

            accent: Color::Blue,
            header: Color::Rgb(100, 50, 0),
            highlight: Color::Blue,
            success: Color::Rgb(0, 128, 0),
            error: Color::Red,
            info: Color::Blue,

            input_active_border: Color::Blue,
            input_active_fg: Color::Blue,
            input_inactive_border: Color::Gray,
            input_inactive_fg: Color::Black,
        }
    }

    pub fn high_contrast() -> Self {
        Self {
            fg: Color::White,
            fg_dim: Color::Gray,
            fg_disabled: Color::Gray,

            bg_selected: Color::White,
            fg_selected: Color::Black,

            accent: Color::Cyan,
            header: Color::Yellow,
            highlight: Color::Yellow,
            success: Color::LightGreen,
            error: Color::LightRed,
            info: Color::LightCyan,

            input_active_border: Color::White,
            input_active_fg: Color::White,
            input_inactive_border: Color::Gray,
            input_inactive_fg: Color::White,
        }
    }

    // --- Convenience style builders ---

    /// Style for normal text
    pub fn text_style(&self) -> Style {
        Style::default().fg(self.fg)
    }

    /// Style for dim/secondary text
    pub fn dim_style(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    /// Style for column headers
    pub fn header_style(&self) -> Style {
        Style::default()
            .fg(self.header)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for selected/highlighted rows
    pub fn selected_style(&self) -> Style {
        Style::default()
            .bg(self.bg_selected)
            .fg(self.fg_selected)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for active borders (modals, focused blocks)
    pub fn border_style(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Style for inactive borders
    pub fn border_inactive_style(&self) -> Style {
        Style::default().fg(self.fg_dim)
    }

    /// Style for active tab highlight
    pub fn tab_highlight_style(&self) -> Style {
        Style::default()
            .fg(self.highlight)
            .add_modifier(Modifier::BOLD)
    }

    /// Style for success text (green)
    pub fn success_style(&self) -> Style {
        Style::default().fg(self.success)
    }

    /// Style for error text (red)
    pub fn error_style(&self) -> Style {
        Style::default().fg(self.error)
    }

    /// Style for informational text
    pub fn info_style(&self) -> Style {
        Style::default().fg(self.info)
    }

    /// Style for active input field border
    pub fn input_active_style(&self) -> Style {
        Style::default().fg(self.input_active_fg)
    }

    /// Style for active input field border
    pub fn input_active_border_style(&self) -> Style {
        Style::default().fg(self.input_active_border)
    }

    /// Style for inactive input field
    pub fn input_inactive_style(&self) -> Style {
        Style::default().fg(self.input_inactive_fg)
    }

    /// Style for inactive input field border
    pub fn input_inactive_border_style(&self) -> Style {
        Style::default().fg(self.input_inactive_border)
    }

    /// Modal title style
    pub fn modal_title_style(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }
}
