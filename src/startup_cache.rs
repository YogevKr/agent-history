//! Persistent per-file cache for the lightweight startup scan.
//!
//! Session files are effectively append-only: once a session ends its rollout
//! file never changes. Caching each file's parsed index entry keyed by a
//! size+mtime fingerprint lets startup skip re-reading unchanged files
//! entirely, turning a multi-gigabyte rescan into a stat sweep plus one small
//! SQLite read. The cache is best-effort: any failure falls back to parsing.

use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const STARTUP_CACHE_FILE: &str = "startup-index.sqlite";

/// Bump when the cached entry layout or parser semantics change.
const CACHE_SCHEMA_VERSION: u32 = 1;

pub fn default_startup_cache_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("AGENT_HISTORY_CACHE_DIR") {
        return Some(PathBuf::from(dir).join(STARTUP_CACHE_FILE));
    }
    Some(
        home::home_dir()?
            .join(".cache")
            .join("agent-history")
            .join(STARTUP_CACHE_FILE),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileFingerprint {
    pub size: i64,
    pub modified_secs: i64,
    pub modified_nanos: i64,
}

pub fn file_fingerprint(path: &Path) -> Option<FileFingerprint> {
    let metadata = fs::metadata(path).ok()?;
    let duration = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some(FileFingerprint {
        size: metadata.len().try_into().ok()?,
        modified_secs: duration.as_secs().try_into().ok()?,
        modified_nanos: duration.subsec_nanos().into(),
    })
}

pub struct StartupCache {
    conn: Connection,
}

impl StartupCache {
    pub fn open_default() -> Option<Self> {
        Self::open(&default_startup_cache_path()?)
    }

    pub fn open(db_path: &Path) -> Option<Self> {
        if let Some(parent) = db_path.parent() {
            ensure_private_cache_dir(parent).ok()?;
        }

        let conn = Connection::open(db_path).ok()?;
        let _ = set_private_file_permissions(db_path);
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .ok()?;
        conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            CREATE TABLE IF NOT EXISTS cache_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS file_index (
                source TEXT NOT NULL,
                path TEXT NOT NULL,
                size INTEGER NOT NULL,
                modified_secs INTEGER NOT NULL,
                modified_nanos INTEGER NOT NULL,
                data TEXT NOT NULL,
                PRIMARY KEY(source, path)
            );
            ",
        )
        .ok()?;

        let cache = Self { conn };
        cache.ensure_version().ok()?;
        Some(cache)
    }

    /// Cached entries are only trusted for the exact app version and cache
    /// schema that wrote them; parser changes between releases must not serve
    /// stale rows.
    fn expected_version() -> String {
        format!("{}-{}", CACHE_SCHEMA_VERSION, env!("CARGO_PKG_VERSION"))
    }

    fn ensure_version(&self) -> rusqlite::Result<()> {
        let expected = Self::expected_version();
        let current: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                err => Err(err),
            })?;

        if current.as_deref() != Some(expected.as_str()) {
            self.conn.execute("DELETE FROM file_index", [])?;
            self.conn.execute(
                "INSERT INTO cache_meta(key, value) VALUES('version', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![expected],
            )?;
        }
        Ok(())
    }

    pub fn load_source_entries(&self, source: &str) -> HashMap<String, (FileFingerprint, String)> {
        self.try_load_source_entries(source).unwrap_or_default()
    }

    fn try_load_source_entries(
        &self,
        source: &str,
    ) -> rusqlite::Result<HashMap<String, (FileFingerprint, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, size, modified_secs, modified_nanos, data
             FROM file_index WHERE source = ?1",
        )?;
        let rows = stmt.query_map(params![source], |row| {
            Ok((
                row.get::<_, String>(0)?,
                FileFingerprint {
                    size: row.get(1)?,
                    modified_secs: row.get(2)?,
                    modified_nanos: row.get(3)?,
                },
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut entries = HashMap::new();
        for row in rows {
            let (path, fingerprint, data) = row?;
            entries.insert(path, (fingerprint, data));
        }
        Ok(entries)
    }

    pub fn store_entries(&mut self, source: &str, entries: &[(String, FileFingerprint, String)]) {
        if entries.is_empty() {
            return;
        }
        let _ = self.try_store_entries(source, entries);
    }

    fn try_store_entries(
        &mut self,
        source: &str,
        entries: &[(String, FileFingerprint, String)],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO file_index(source, path, size, modified_secs, modified_nanos, data)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(source, path) DO UPDATE SET
                     size = excluded.size,
                     modified_secs = excluded.modified_secs,
                     modified_nanos = excluded.modified_nanos,
                     data = excluded.data",
            )?;
            for (path, fingerprint, data) in entries {
                stmt.execute(params![
                    source,
                    path,
                    fingerprint.size,
                    fingerprint.modified_secs,
                    fingerprint.modified_nanos,
                    data,
                ])?;
            }
        }
        tx.commit()
    }

    /// Drop rows for files that no longer exist on disk.
    ///
    /// Rows outside the current scan (`live_paths`) are only deleted after a
    /// stat confirms the file is gone: a scan of one session root (a different
    /// CODEX_HOME/CLAUDE_CONFIG_DIR, a test tempdir) must never wipe cached
    /// entries that belong to another root.
    pub fn prune_missing(&mut self, source: &str, live_paths: &HashSet<String>) {
        let _ = self.try_prune_missing(source, live_paths);
    }

    fn try_prune_missing(
        &mut self,
        source: &str,
        live_paths: &HashSet<String>,
    ) -> rusqlite::Result<()> {
        let cached_paths: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT path FROM file_index WHERE source = ?1")?;
            let rows = stmt.query_map(params![source], |row| row.get::<_, String>(0))?;
            rows.filter_map(|row| row.ok())
                .filter(|path| !live_paths.contains(path) && !Path::new(path).exists())
                .collect()
        };

        if cached_paths.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM file_index WHERE source = ?1 AND path = ?2")?;
            for path in &cached_paths {
                stmt.execute(params![source, path])?;
            }
        }
        tx.commit()
    }
}

fn ensure_private_cache_dir(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(path)?;
    set_private_dir_permissions(path)
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_dir_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fingerprint(size: i64) -> FileFingerprint {
        FileFingerprint {
            size,
            modified_secs: 100,
            modified_nanos: 5,
        }
    }

    #[test]
    fn store_and_load_round_trips_entries_per_source() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(STARTUP_CACHE_FILE);
        let mut cache = StartupCache::open(&db_path).unwrap();

        cache.store_entries(
            "codex",
            &[("a.jsonl".to_string(), fingerprint(1), "codex-a".to_string())],
        );
        cache.store_entries(
            "claude",
            &[(
                "a.jsonl".to_string(),
                fingerprint(2),
                "claude-a".to_string(),
            )],
        );

        let codex = cache.load_source_entries("codex");
        assert_eq!(codex.len(), 1);
        assert_eq!(codex["a.jsonl"], (fingerprint(1), "codex-a".to_string()));

        let claude = cache.load_source_entries("claude");
        assert_eq!(claude["a.jsonl"], (fingerprint(2), "claude-a".to_string()));
    }

    #[test]
    fn store_overwrites_changed_entries() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(STARTUP_CACHE_FILE);
        let mut cache = StartupCache::open(&db_path).unwrap();

        cache.store_entries(
            "codex",
            &[("a.jsonl".to_string(), fingerprint(1), "old".to_string())],
        );
        cache.store_entries(
            "codex",
            &[("a.jsonl".to_string(), fingerprint(9), "new".to_string())],
        );

        let entries = cache.load_source_entries("codex");
        assert_eq!(entries["a.jsonl"], (fingerprint(9), "new".to_string()));
    }

    #[test]
    fn prune_missing_removes_only_dead_paths_for_source() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(STARTUP_CACHE_FILE);
        let mut cache = StartupCache::open(&db_path).unwrap();

        // A file outside the scanned root but still present on disk: a scan
        // that does not include it must not prune it.
        let other_root_file = dir.path().join("other-root.jsonl");
        fs::write(&other_root_file, "{}").unwrap();
        let other_root_path = other_root_file.to_string_lossy().to_string();

        cache.store_entries(
            "codex",
            &[
                ("live.jsonl".to_string(), fingerprint(1), "live".to_string()),
                ("dead.jsonl".to_string(), fingerprint(2), "dead".to_string()),
                (other_root_path.clone(), fingerprint(3), "other".to_string()),
            ],
        );
        cache.store_entries(
            "claude",
            &[("dead.jsonl".to_string(), fingerprint(4), "keep".to_string())],
        );

        let live: HashSet<String> = ["live.jsonl".to_string()].into_iter().collect();
        cache.prune_missing("codex", &live);

        let codex = cache.load_source_entries("codex");
        assert_eq!(codex.len(), 2);
        assert!(codex.contains_key("live.jsonl"));
        assert!(codex.contains_key(&other_root_path));
        assert_eq!(cache.load_source_entries("claude").len(), 1);
    }

    #[test]
    fn version_mismatch_clears_stale_entries() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(STARTUP_CACHE_FILE);

        {
            let mut cache = StartupCache::open(&db_path).unwrap();
            cache.store_entries(
                "codex",
                &[("a.jsonl".to_string(), fingerprint(1), "data".to_string())],
            );
            cache
                .conn
                .execute(
                    "UPDATE cache_meta SET value = 'stale' WHERE key = 'version'",
                    [],
                )
                .unwrap();
        }

        let cache = StartupCache::open(&db_path).unwrap();
        assert!(cache.load_source_entries("codex").is_empty());
    }

    #[test]
    fn reopen_preserves_entries_for_same_version() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join(STARTUP_CACHE_FILE);

        {
            let mut cache = StartupCache::open(&db_path).unwrap();
            cache.store_entries(
                "codex",
                &[("a.jsonl".to_string(), fingerprint(1), "data".to_string())],
            );
        }

        let cache = StartupCache::open(&db_path).unwrap();
        assert_eq!(cache.load_source_entries("codex").len(), 1);
    }
}
