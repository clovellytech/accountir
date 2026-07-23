//! One-time migration from legacy on-disk config files (TOML/JSON) into the
//! registry's `preferences` table. Idempotent — gated by the
//! `schema_migrated_from_legacy` pref.

use std::path::PathBuf;

use anyhow::Result;

use super::Registry;

const SENTINEL: &str = "schema_migrated_from_legacy";

pub fn migrate_legacy(reg: &Registry) -> Result<()> {
    if reg.get_pref(SENTINEL)?.as_deref() == Some("true") {
        return Ok(());
    }

    let config_dir = dirs::config_dir()
        .map(|d| d.join("accountir"))
        .unwrap_or_else(|| PathBuf::from("."));

    let toml_path = config_dir.join("config.toml");
    let json_path = config_dir.join("config.json");

    // --- config.toml: { theme, plaid: { proxy_url, api_key } }
    if let Ok(contents) = std::fs::read_to_string(&toml_path) {
        if let Ok(parsed) = toml::from_str::<LegacyAppConfig>(&contents) {
            // Theme — write the lowercase serialized form directly.
            let theme = match parsed.theme.as_deref() {
                Some("light") => Some("light"),
                Some("highcontrast") => Some("highcontrast"),
                Some("dark") => Some("dark"),
                _ => None,
            };
            if let Some(t) = theme {
                reg.set_pref("theme", t)?;
            }
            if let Some(plaid) = parsed.plaid {
                if let Some(url) = plaid.proxy_url {
                    reg.set_pref("plaid_proxy_url", &url)?;
                }
                if let Some(key) = plaid.api_key {
                    reg.set_pref("plaid_api_key", &key)?;
                }
            }
        }
    }

    // --- config.json: { show_welcome: bool }
    if let Ok(contents) = std::fs::read_to_string(&json_path) {
        // Match the lightweight parsing used in the legacy welcome.rs.
        let show = !contents.contains("\"show_welcome\":false");
        reg.set_bool("show_welcome", show)?;
    }

    reg.set_pref(SENTINEL, "true")?;

    // Best-effort cleanup; don't fail the migration if removal fails.
    let _ = std::fs::remove_file(&toml_path);
    let _ = std::fs::remove_file(&json_path);

    Ok(())
}

#[derive(serde::Deserialize)]
struct LegacyAppConfig {
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    plaid: Option<LegacyPlaid>,
}

#[derive(serde::Deserialize)]
struct LegacyPlaid {
    proxy_url: Option<String>,
    api_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    #[test]
    fn migration_is_idempotent_and_reads_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open_at(&tmp.path().join("registry.db")).unwrap();
        // Migrate with no legacy files — should still set the sentinel.
        migrate_legacy(&reg).unwrap();
        assert_eq!(reg.get_pref(SENTINEL).unwrap().as_deref(), Some("true"));
        // Second call is a no-op.
        migrate_legacy(&reg).unwrap();
    }
}
