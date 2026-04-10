use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::tui::theme::ThemePreset;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub plaid: PlaidConfig,
    #[serde(default)]
    pub theme: ThemePreset,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaidConfig {
    pub proxy_url: Option<String>,
    pub api_key: Option<String>,
}

impl PlaidConfig {
    pub fn is_configured(&self) -> bool {
        self.proxy_url.is_some() && self.api_key.is_some()
    }
}

impl AppConfig {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => AppConfig::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(&path, contents)?;
        Ok(())
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("accountir")
        .join("config.toml")
}
