//! `nisse cache status`: report the generated local caches.
//!
//! Status is diagnostic: it never creates, migrates, or recovers a
//! database (that belongs to [`super::db::open_or_recover`] and
//! `nisse cache clear`). Corrupt or too-new caches are surfaced as
//! problems so the user decides to rebuild.

use std::path::{Path, PathBuf};

use super::db::{ProbeOutcome, probe};
use super::paths::StoragePaths;
use super::schema::DbKind;

/// Status of one cache database.
#[derive(Debug)]
pub enum CacheDbStatus {
    /// No database file exists yet (normal for a fresh repository).
    Missing {
        /// Database file location.
        path: PathBuf,
    },
    /// The database file exists but is corrupt.
    Corrupt {
        /// Database file location.
        path: PathBuf,
        /// Corruption detail from SQLite.
        detail: String,
    },
    /// The database was written by a newer Nisse.
    TooNew {
        /// Database file location.
        path: PathBuf,
        /// Schema version found in the file.
        found: u32,
        /// Schema version this Nisse supports.
        supported: u32,
    },
    /// The database is usable.
    Ready {
        /// Database file location.
        path: PathBuf,
        /// Current file size in bytes.
        size_bytes: u64,
        /// Recorded schema version.
        version: u32,
        /// Whether the integrity check passed.
        integrity_ok: bool,
        /// Integrity check detail when it failed.
        integrity_detail: String,
        /// Per required table: row count, or `None` when the table does
        /// not exist yet.
        tables: Vec<(&'static str, Option<u64>)>,
    },
}

impl CacheDbStatus {
    /// The database file location.
    pub fn path(&self) -> &Path {
        match self {
            CacheDbStatus::Missing { path }
            | CacheDbStatus::Corrupt { path, .. }
            | CacheDbStatus::TooNew { path, .. }
            | CacheDbStatus::Ready { path, .. } => path,
        }
    }

    /// Whether this database needs attention.
    pub fn is_problem(&self) -> bool {
        match self {
            CacheDbStatus::Missing { .. } => false,
            CacheDbStatus::Corrupt { .. } | CacheDbStatus::TooNew { .. } => true,
            CacheDbStatus::Ready { integrity_ok, .. } => !*integrity_ok,
        }
    }
}

/// Full cache status for one worktree.
#[derive(Debug)]
pub struct CacheStatus {
    /// Absolute worktree root.
    pub worktree_root: PathBuf,
    /// Status of the shared cache.
    pub shared: CacheDbStatus,
    /// Status of this worktree's cache.
    pub worktree: CacheDbStatus,
}

impl CacheStatus {
    /// Whether any cache is corrupt, too new, or failed its integrity
    /// check.
    pub fn has_problems(&self) -> bool {
        self.shared.is_problem() || self.worktree.is_problem()
    }

    /// Render the human-readable report.
    pub fn render(&self) -> String {
        let mut out = String::from("nisse cache status\n");
        out.push_str(&format!("worktree: {}\n", self.worktree_root.display()));
        out.push_str(&render_db(DbKind::Shared, &self.shared));
        out.push_str(&render_db(DbKind::Worktree, &self.worktree));
        out
    }
}

/// Render one database section.
fn render_db(kind: DbKind, status: &CacheDbStatus) -> String {
    let mut out = format!("{}: {}\n", kind.label(), status.path().display());
    match status {
        CacheDbStatus::Missing { .. } => {
            out.push_str("  status: not created (empty caches are created on first use)\n");
        }
        CacheDbStatus::Corrupt { detail, .. } => {
            out.push_str("  status: corrupt\n");
            out.push_str(&format!("  detail: {detail}\n"));
            out.push_str(
                "  hint: run `nisse cache clear` to rebuild it (cache content is regenerable)\n",
            );
        }
        CacheDbStatus::TooNew {
            found, supported, ..
        } => {
            out.push_str(&format!(
                "  status: schema version {found} is newer than supported version {supported}; upgrade Nisse\n"
            ));
        }
        CacheDbStatus::Ready {
            size_bytes,
            version,
            integrity_ok,
            integrity_detail,
            tables,
            ..
        } => {
            out.push_str(&format!("  status: ok ({size_bytes} bytes)\n"));
            out.push_str(&format!("  schema version: {version}\n"));
            if *integrity_ok {
                out.push_str("  integrity: ok\n");
            } else {
                out.push_str(&format!("  integrity: FAILED ({integrity_detail})\n"));
                out.push_str("  hint: run `nisse cache clear` to rebuild it (cache content is regenerable)\n");
            }
            out.push_str("  tables:\n");
            for (name, rows) in tables {
                match rows {
                    Some(rows) => out.push_str(&format!("    {name}: {rows} rows\n")),
                    None => out.push_str(&format!("    {name}: absent (not migrated yet)\n")),
                }
            }
        }
    }
    out
}

/// Probe both cache databases of one worktree.
pub fn status(
    worktree_root: &Path,
    paths: &StoragePaths,
) -> Result<CacheStatus, super::db::StorageError> {
    let shared = probe_one(&paths.shared, DbKind::Shared)?;
    let worktree = probe_one(&paths.worktree, DbKind::Worktree)?;
    Ok(CacheStatus {
        worktree_root: worktree_root.to_owned(),
        shared,
        worktree,
    })
}

/// Probe one database and translate the outcome into a report status.
fn probe_one(path: &Path, kind: DbKind) -> Result<CacheDbStatus, super::db::StorageError> {
    let size_bytes = std::fs::metadata(path)
        .map(|meta| meta.len())
        .ok()
        .unwrap_or(0);
    match probe(path, kind)? {
        ProbeOutcome::Missing => Ok(CacheDbStatus::Missing {
            path: path.to_owned(),
        }),
        ProbeOutcome::Corrupt { detail } => Ok(CacheDbStatus::Corrupt {
            path: path.to_owned(),
            detail,
        }),
        ProbeOutcome::TooNew { found, supported } => Ok(CacheDbStatus::TooNew {
            path: path.to_owned(),
            found,
            supported,
        }),
        ProbeOutcome::Valid(data) => Ok(CacheDbStatus::Ready {
            path: path.to_owned(),
            size_bytes,
            version: data.version,
            integrity_ok: data.integrity_ok,
            integrity_detail: data.integrity_detail,
            tables: data.tables,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db;

    fn temp_paths(label: &str) -> (tempfile::TempDir, StoragePaths) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("nisse-status-{label}-"))
            .tempdir()
            .unwrap();
        let paths = StoragePaths {
            shared: dir.path().join("common").join("nisse").join("cache.sqlite"),
            worktree: dir.path().join("git").join("nisse").join("worktree.sqlite"),
        };
        (dir, paths)
    }

    fn report_status(paths: &StoragePaths) -> CacheStatus {
        status(Path::new("/repo"), paths).unwrap()
    }

    #[test]
    fn status_reports_missing_databases_without_creating_them() {
        let (_dir, paths) = temp_paths("missing");
        let report = report_status(&paths);
        assert!(!report.has_problems());
        assert!(matches!(report.shared, CacheDbStatus::Missing { .. }));
        assert!(matches!(report.worktree, CacheDbStatus::Missing { .. }));
        assert!(!paths.shared.exists(), "status must not create databases");
        assert!(!paths.worktree.exists(), "status must not create databases");

        let text = report.render();
        assert!(text.starts_with("nisse cache status\n"));
        assert!(text.contains("status: not created"));
        assert!(text.contains("shared cache: "));
        assert!(text.contains("worktree cache: "));
    }

    #[test]
    fn status_reports_ready_databases_with_table_counts() {
        let (_dir, paths) = temp_paths("ready");
        let opened = db::open_or_recover(&paths.shared, DbKind::Shared).unwrap();
        opened
            .conn
            .execute(
                "INSERT INTO content_artifact (content_key, byte_len, created_at)
                 VALUES ('git:abc', 3, '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        drop(opened);
        let _ = db::open_or_recover(&paths.worktree, DbKind::Worktree).unwrap();

        let report = report_status(&paths);
        assert!(!report.has_problems());
        let CacheDbStatus::Ready {
            tables, version, ..
        } = &report.shared
        else {
            panic!("expected ready shared cache");
        };
        assert_eq!(*version, 1);
        let content = tables
            .iter()
            .find(|(name, _)| *name == "content_artifact")
            .unwrap();
        assert_eq!(content.1, Some(1));

        let text = report.render();
        assert!(text.contains("status: ok"));
        assert!(text.contains("schema version: 1"));
        assert!(text.contains("integrity: ok"));
        assert!(text.contains("content_artifact: 1 rows"));
        assert!(text.contains("manifest: 0 rows"));
    }

    #[test]
    fn status_reports_corruption_as_a_problem() {
        let (_dir, paths) = temp_paths("corrupt");
        std::fs::create_dir_all(paths.shared.parent().unwrap()).unwrap();
        std::fs::write(&paths.shared, b"not a database at all").unwrap();

        let report = report_status(&paths);
        assert!(report.has_problems());
        let text = report.render();
        assert!(text.contains("status: corrupt"));
        assert!(text.contains("nisse cache clear"));
        // The corrupt file is still there; status never rebuilds.
        assert!(paths.shared.is_file());
    }

    #[test]
    fn status_reports_newer_schema_as_a_problem() {
        let (_dir, paths) = temp_paths("toonew");
        {
            std::fs::create_dir_all(paths.worktree.parent().unwrap()).unwrap();
            let conn = rusqlite::Connection::open(&paths.worktree).unwrap();
            conn.pragma_update(None, "user_version", 99_i64).unwrap();
        }
        let report = report_status(&paths);
        assert!(report.has_problems());
        let text = report.render();
        assert!(text.contains("newer than supported version"));
        assert!(text.contains("99"));
    }
}
