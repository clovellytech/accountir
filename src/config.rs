use serde::{Deserialize, Serialize};

use crate::registry::Registry;
use crate::tui::theme::ThemePreset;

/// User-level application configuration. Persisted in the root registry
/// (`~/.local/share/accountir/registry.db`) — this struct is just a convenience
/// view over the `preferences` table.
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
    /// Load from the registry. Falls back to defaults if the registry can't be
    /// opened (e.g. no home directory in a sandboxed environment).
    pub fn load() -> Self {
        match Registry::open_default() {
            Ok(reg) => AppConfig {
                plaid: reg.get_plaid(),
                theme: reg.get_theme(),
            },
            Err(_) => AppConfig::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let reg = Registry::open_default()?;
        reg.set_plaid(&self.plaid)?;
        reg.set_theme(self.theme)?;
        Ok(())
    }
}
