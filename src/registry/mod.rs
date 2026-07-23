//! Root-level registry of businesses and user preferences.
//!
//! Stored at `~/.local/share/accountir/registry.db`. Independent from per-business
//! event-store databases — the registry has its own `schema_migrations` table.

pub mod legacy_migration;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use uuid::Uuid;

use crate::config::PlaidConfig;
use crate::tui::theme::ThemePreset;

const REGISTRY_SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS businesses (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    display_name   TEXT,
    db_path        TEXT NOT NULL UNIQUE,
    original_path  TEXT,
    is_archived    INTEGER NOT NULL DEFAULT 0,
    last_opened_at TEXT,
    created_at     TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_businesses_archived ON businesses(is_archived);
CREATE INDEX IF NOT EXISTS idx_businesses_last_opened ON businesses(last_opened_at DESC);

CREATE TABLE IF NOT EXISTS preferences (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

#[derive(Clone, Debug)]
pub struct Business {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub db_path: PathBuf,
    pub original_path: Option<PathBuf>,
    pub is_archived: bool,
    pub last_opened_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Business {
    pub fn label(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.name)
    }
}

pub struct Registry {
    conn: Connection,
}

impl Registry {
    /// Open the registry at the standard location (`~/.local/share/accountir/registry.db`),
    /// creating directories and tables as needed.
    pub fn open_default() -> Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating registry data dir {}", dir.display()))?;
        Self::open_at(&dir.join("registry.db"))
    }

    /// Open the registry at an explicit path (used for tests).
    pub fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening registry at {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; \
             PRAGMA foreign_keys=ON; \
             PRAGMA busy_timeout=5000;",
        )?;
        let reg = Self { conn };
        reg.run_registry_migrations()?;
        Ok(reg)
    }

    fn run_registry_migrations(&self) -> Result<()> {
        self.conn.execute_batch(REGISTRY_SCHEMA_V1)?;
        let current: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if current < 1 {
            self.conn
                .execute("INSERT INTO schema_migrations (version) VALUES (1)", [])?;
        }
        Ok(())
    }

    // --- businesses ---

    pub fn list_active(&self) -> Result<Vec<Business>> {
        self.list_where("is_archived = 0", "last_opened_at DESC NULLS LAST, created_at DESC")
    }

    pub fn list_archived(&self) -> Result<Vec<Business>> {
        self.list_where("is_archived = 1", "created_at DESC")
    }

    fn list_where(&self, predicate: &str, order: &str) -> Result<Vec<Business>> {
        let sql = format!(
            "SELECT id, name, display_name, db_path, original_path, is_archived, \
                    last_opened_at, created_at \
             FROM businesses WHERE {} ORDER BY {}",
            predicate, order
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_business)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<Business>> {
        self.conn
            .query_row(
                "SELECT id, name, display_name, db_path, original_path, is_archived, \
                        last_opened_at, created_at \
                 FROM businesses WHERE id = ?1",
                params![id],
                row_to_business,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn find_by_path(&self, path: &Path) -> Result<Option<Business>> {
        let canonical = canonicalize_existing_or_parent(path)?;
        self.conn
            .query_row(
                "SELECT id, name, display_name, db_path, original_path, is_archived, \
                        last_opened_at, created_at \
                 FROM businesses WHERE db_path = ?1",
                params![canonical.to_string_lossy()],
                row_to_business,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn add_business(&self, name: &str, db_path: &Path) -> Result<Business> {
        let canonical = canonicalize_existing_or_parent(db_path)?;
        if let Some(existing) = self.find_by_path(&canonical)? {
            return Ok(existing);
        }
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO businesses (id, name, db_path) VALUES (?1, ?2, ?3)",
            params![id, name, canonical.to_string_lossy()],
        )?;
        self.get(&id)?
            .ok_or_else(|| anyhow!("inserted business {} not found", id))
    }

    pub fn rename(&self, id: &str, display_name: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE businesses SET display_name = ?1 WHERE id = ?2",
            params![display_name, id],
        )?;
        Ok(())
    }

    pub fn update_name_cache(&self, id: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE businesses SET name = ?1 WHERE id = ?2",
            params![name, id],
        )?;
        Ok(())
    }

    pub fn touch_last_opened(&self, id: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE businesses SET last_opened_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM businesses WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Move the business's .db file into the archive directory and flag the row.
    /// Returns the new (archived) path.
    pub fn archive(&self, id: &str) -> Result<PathBuf> {
        let biz = self
            .get(id)?
            .ok_or_else(|| anyhow!("business {} not found", id))?;
        if biz.is_archived {
            return Ok(biz.db_path);
        }
        let archive = archive_dir();
        std::fs::create_dir_all(&archive)?;
        let basename = biz
            .db_path
            .file_name()
            .ok_or_else(|| anyhow!("path has no filename: {}", biz.db_path.display()))?;
        let mut target = archive.join(basename);
        if target.exists() {
            let ts = chrono::Utc::now().timestamp();
            let stem = biz
                .db_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("business");
            let ext = biz
                .db_path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("db");
            target = archive.join(format!("{}_archived_{}.{}", stem, ts, ext));
        }
        move_file(&biz.db_path, &target)?;
        let canonical_target = std::fs::canonicalize(&target).unwrap_or(target.clone());
        self.conn.execute(
            "UPDATE businesses SET is_archived = 1, original_path = ?1, db_path = ?2 WHERE id = ?3",
            params![
                biz.db_path.to_string_lossy(),
                canonical_target.to_string_lossy(),
                id
            ],
        )?;
        Ok(canonical_target)
    }

    /// Move the archived file back to its original location and clear the archive flag.
    /// Returns the restored path (may differ from the original if a collision occurred).
    pub fn restore(&self, id: &str) -> Result<PathBuf> {
        let biz = self
            .get(id)?
            .ok_or_else(|| anyhow!("business {} not found", id))?;
        if !biz.is_archived {
            return Ok(biz.db_path);
        }
        let mut target = biz
            .original_path
            .clone()
            .ok_or_else(|| anyhow!("archived business {} has no original_path", id))?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if target.exists() {
            let ts = chrono::Utc::now().timestamp();
            let stem = target
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("business");
            let ext = target.extension().and_then(|s| s.to_str()).unwrap_or("db");
            let parent = target.parent().map(Path::to_path_buf).unwrap_or_default();
            target = parent.join(format!("{}_restored_{}.{}", stem, ts, ext));
        }
        move_file(&biz.db_path, &target)?;
        let canonical_target = std::fs::canonicalize(&target).unwrap_or(target.clone());
        self.conn.execute(
            "UPDATE businesses SET is_archived = 0, original_path = NULL, db_path = ?1 WHERE id = ?2",
            params![canonical_target.to_string_lossy(), id],
        )?;
        Ok(canonical_target)
    }

    // --- preferences ---

    pub fn get_pref(&self, k: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT v FROM preferences WHERE k = ?1",
                params![k],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn set_pref(&self, k: &str, v: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO preferences (k, v) VALUES (?1, ?2) \
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    pub fn delete_pref(&self, k: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM preferences WHERE k = ?1", params![k])?;
        Ok(())
    }

    pub fn get_bool(&self, k: &str, default: bool) -> bool {
        match self.get_pref(k).ok().flatten().as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => default,
        }
    }

    pub fn set_bool(&self, k: &str, v: bool) -> Result<()> {
        self.set_pref(k, if v { "true" } else { "false" })
    }

    pub fn get_theme(&self) -> ThemePreset {
        match self.get_pref("theme").ok().flatten().as_deref() {
            Some("light") => ThemePreset::Light,
            Some("highcontrast") => ThemePreset::HighContrast,
            Some("dark") => ThemePreset::Dark,
            _ => ThemePreset::default(),
        }
    }

    pub fn set_theme(&self, t: ThemePreset) -> Result<()> {
        let v = match t {
            ThemePreset::Dark => "dark",
            ThemePreset::Light => "light",
            ThemePreset::HighContrast => "highcontrast",
        };
        self.set_pref("theme", v)
    }

    pub fn get_plaid(&self) -> PlaidConfig {
        PlaidConfig {
            proxy_url: self.get_pref("plaid_proxy_url").ok().flatten(),
            api_key: self.get_pref("plaid_api_key").ok().flatten(),
        }
    }

    pub fn set_plaid(&self, p: &PlaidConfig) -> Result<()> {
        match &p.proxy_url {
            Some(v) => self.set_pref("plaid_proxy_url", v)?,
            None => self.delete_pref("plaid_proxy_url")?,
        }
        match &p.api_key {
            Some(v) => self.set_pref("plaid_api_key", v)?,
            None => self.delete_pref("plaid_api_key")?,
        }
        Ok(())
    }
}

/// `~/.local/share/accountir`
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("accountir")
}

/// `~/.local/share/accountir/archive`
pub fn archive_dir() -> PathBuf {
    data_dir().join("archive")
}

/// Open a SQLite file read-only and check for an `events` table — our shibboleth
/// for "this is an accountir database."
pub fn is_accountir_db(path: &Path) -> bool {
    let Ok(conn) = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='events'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

fn row_to_business(row: &rusqlite::Row<'_>) -> rusqlite::Result<Business> {
    let last_opened: Option<String> = row.get(6)?;
    let created: String = row.get(7)?;
    let db_path: String = row.get(3)?;
    let original_path: Option<String> = row.get(4)?;
    let is_archived: i64 = row.get(5)?;
    Ok(Business {
        id: row.get(0)?,
        name: row.get(1)?,
        display_name: row.get(2)?,
        db_path: PathBuf::from(db_path),
        original_path: original_path.map(PathBuf::from),
        is_archived: is_archived != 0,
        last_opened_at: last_opened.and_then(|s| DateTime::parse_from_rfc3339(&s).ok().map(|d| d.with_timezone(&Utc))),
        created_at: DateTime::parse_from_rfc3339(&created)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now()),
    })
}

/// Canonicalize `path` if it exists; otherwise canonicalize its parent and
/// append the filename. Useful for "about to be created" paths.
fn canonicalize_existing_or_parent(path: &Path) -> Result<PathBuf> {
    if let Ok(p) = std::fs::canonicalize(path) {
        return Ok(p);
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no filename: {}", path.display()))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .or_else(|_| {
            // Fall back to absolute if parent doesn't yet exist.
            if parent.is_absolute() {
                Ok(parent.to_path_buf())
            } else {
                std::env::current_dir().map(|cwd| cwd.join(parent))
            }
        })?;
    Ok(canonical_parent.join(name))
}

/// `std::fs::rename` with a copy+delete fallback if the source and dest are on
/// different filesystems (or rename otherwise fails).
fn move_file(src: &Path, dst: &Path) -> Result<()> {
    if let Err(e) = std::fs::rename(src, dst) {
        // Try copy + delete fallback
        std::fs::copy(src, dst).with_context(|| {
            format!(
                "fallback copy from {} to {} (rename failed: {})",
                src.display(),
                dst.display(),
                e
            )
        })?;
        std::fs::remove_file(src).with_context(|| {
            format!("removing source {} after copy", src.display())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_registry() -> (Registry, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let reg = Registry::open_at(&tmp.path().join("registry.db")).unwrap();
        (reg, tmp)
    }

    #[test]
    fn add_and_list_business() {
        let (reg, tmp) = temp_registry();
        let db = tmp.path().join("foo.db");
        std::fs::write(&db, b"").unwrap();
        let b = reg.add_business("Foo", &db).unwrap();
        assert_eq!(b.name, "Foo");
        assert!(!b.is_archived);
        let active = reg.list_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, b.id);
    }

    #[test]
    fn add_business_is_idempotent_on_path() {
        let (reg, tmp) = temp_registry();
        let db = tmp.path().join("foo.db");
        std::fs::write(&db, b"").unwrap();
        let a = reg.add_business("Foo", &db).unwrap();
        let b = reg.add_business("Foo Renamed", &db).unwrap();
        assert_eq!(a.id, b.id);
    }

    #[test]
    fn archive_then_restore() {
        let (reg, tmp) = temp_registry();
        let db = tmp.path().join("biz.db");
        std::fs::write(&db, b"x").unwrap();
        let b = reg.add_business("Biz", &db).unwrap();
        let original_path = b.db_path.clone();

        let archived = reg.archive(&b.id).unwrap();
        assert!(archived.exists());
        assert!(!original_path.exists());
        let row = reg.get(&b.id).unwrap().unwrap();
        assert!(row.is_archived);
        assert_eq!(row.original_path.as_ref().unwrap(), &original_path);

        let restored = reg.restore(&b.id).unwrap();
        assert_eq!(restored, original_path);
        assert!(restored.exists());
        let row = reg.get(&b.id).unwrap().unwrap();
        assert!(!row.is_archived);
        assert!(row.original_path.is_none());
    }

    #[test]
    fn preferences_roundtrip() {
        let (reg, _tmp) = temp_registry();
        assert_eq!(reg.get_bool("show_welcome", true), true);
        reg.set_bool("show_welcome", false).unwrap();
        assert_eq!(reg.get_bool("show_welcome", true), false);
        reg.set_theme(ThemePreset::Light).unwrap();
        assert_eq!(reg.get_theme(), ThemePreset::Light);
        reg.set_plaid(&PlaidConfig {
            proxy_url: Some("http://x".to_string()),
            api_key: Some("k".to_string()),
        })
        .unwrap();
        let p = reg.get_plaid();
        assert_eq!(p.proxy_url.as_deref(), Some("http://x"));
        assert_eq!(p.api_key.as_deref(), Some("k"));
    }
}
