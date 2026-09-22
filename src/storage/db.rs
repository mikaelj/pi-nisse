//! Opening, migrating, recovering, and clearing cache databases.
//!
//! All SQLite state is regenerable local state (plan section 29): losing it
//! never destroys durable shared knowledge. This module owns the exact
//! open/migrate/recover contract every other subsystem must use:
//!
//! - [`open_or_recover`] is the operational opener. It creates missing
//!   databases, migrates older schemas forward, and — because the content
//!   is disposable — recovers a corrupt database by deleting it (including
//!   journal sidecar files) and recreating an empty one. Recovery is
//!   reported, never hidden.
//! - A database whose schema version is *newer* than this Nisse supports
//!   is never opened and never deleted; it fails with a typed
//!   [`StorageError::SchemaTooNew`] error instead.
//! - [`probe`] inspects an existing database without mutating logical
//!   content and without migrating or recovering; `nisse cache status`
//!   uses it so corruption is surfaced, not silently rebuilt.
//! - [`clear`] deletes a database and recreates it empty; it is the
//!   explicit user-facing recovery (`nisse cache clear`).

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use thiserror::Error;

use super::schema::{DbKind, Migration, supported_version};

/// Busy timeout for concurrent short-lived Nisse processes.
const BUSY_TIMEOUT_MS: u64 = 5000;

/// A successfully opened cache database.
#[derive(Debug)]
pub struct OpenedCache {
    /// Live SQLite connection with pragmas applied and schema migrated.
    pub conn: Connection,
    /// Whether a corrupt database file was deleted and rebuilt.
    pub recovered: bool,
}

/// Typed storage failures.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The database file exists but is not a usable SQLite database.
    #[error("cache database `{path}` is corrupt: {message}")]
    Corrupt { path: PathBuf, message: String },

    /// Creating the database directory was refused by the
    /// directory-creation consent policy (user-requested plan change);
    /// noninteractive callers fail closed with this typed error.
    #[error("{}", crate::config::consent::consent_error_text(path))]
    DirectoryConsentRequired { path: PathBuf },

    /// The database could not be opened or created.
    #[error("failed to open cache database `{path}`: {message}")]
    Open { path: PathBuf, message: String },

    /// A query against the database failed.
    #[error("failed to query cache database `{path}`: {message}")]
    Query { path: PathBuf, message: String },

    /// The database was written by a newer Nisse and cannot be opened
    /// (and must not be destroyed) by this one.
    #[error(
        "cache database `{path}` uses schema version {found}, which is newer than supported version {supported}; upgrade Nisse"
    )]
    SchemaTooNew {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    /// A filesystem operation failed.
    #[error("filesystem error for cache database `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Open (creating if necessary) and migrate a cache database, recovering
/// from corruption by rebuilding it empty.
///
/// This is the only opener other subsystems may use for real work. The
/// returned connection has `WAL` journaling, a busy timeout, and foreign
/// key enforcement applied, and its schema is at the newest supported
/// version.
pub fn open_or_recover(path: &Path, kind: DbKind) -> Result<OpenedCache, StorageError> {
    let existed = path.is_file();
    match open_and_migrate(path, kind) {
        Ok(conn) => Ok(OpenedCache {
            conn,
            recovered: false,
        }),
        Err(StorageError::Corrupt { .. }) if existed => {
            tracing::warn!(
                path = %path.display(),
                kind = kind.label(),
                "corrupt cache database; rebuilding it empty (cache content is regenerable)"
            );
            remove_database_files(path)?;
            let conn = open_and_migrate(path, kind)?;
            Ok(OpenedCache {
                conn,
                recovered: true,
            })
        }
        Err(err) => Err(err),
    }
}

/// Open the database, apply pragmas, validate readability, and migrate the
/// schema forward.
fn open_and_migrate(path: &Path, kind: DbKind) -> Result<Connection, StorageError> {
    ensure_parent(path)?;
    let mut conn = Connection::open(path).map_err(|err| open_error(path, err))?;
    apply_pragmas(&conn, path)?;
    validate_readable(&conn, path)?;
    migrate(&mut conn, path, kind)?;
    Ok(conn)
}

/// Apply the operational connection pragmas.
///
/// `journal_mode` touches the file, so a garbage file surfaces its
/// not-a-database error here at the latest.
fn apply_pragmas(conn: &Connection, path: &Path) -> Result<(), StorageError> {
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(|err| open_error(path, err))?;
    conn.pragma_update(None, "foreign_keys", true)
        .map_err(|err| open_error(path, err))?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| open_error(path, err))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|err| open_error(path, err))?;
    Ok(())
}

/// Prove the file is a readable SQLite database.
fn validate_readable(conn: &Connection, path: &Path) -> Result<(), StorageError> {
    conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| {
        row.get::<_, i64>(0)
    })
    .map_err(|err| from_rusqlite(path, err))?;
    Ok(())
}

/// Migrate the schema forward to the newest supported version.
///
/// Each migration runs inside one transaction together with its
/// `user_version` bump, so a partially applied migration can never leave
/// a misleading version behind.
fn migrate(conn: &mut Connection, path: &Path, kind: DbKind) -> Result<(), StorageError> {
    let current = read_user_version(conn, path)?;
    let supported = supported_version(kind);
    if current > supported {
        return Err(StorageError::SchemaTooNew {
            path: path.to_owned(),
            found: current,
            supported,
        });
    }
    for migration in kind
        .migrations()
        .iter()
        .filter(|migration| migration.version > current)
    {
        apply_migration(conn, path, migration)?;
    }
    Ok(())
}

/// Apply one migration transactionally.
fn apply_migration(
    conn: &mut Connection,
    path: &Path,
    migration: &Migration,
) -> Result<(), StorageError> {
    let tx = conn.transaction().map_err(|err| from_rusqlite(path, err))?;
    tx.execute_batch(migration.sql)
        .map_err(|err| from_rusqlite(path, err))?;
    tx.pragma_update(None, "user_version", i64::from(migration.version))
        .map_err(|err| from_rusqlite(path, err))?;
    tx.commit().map_err(|err| from_rusqlite(path, err))?;
    Ok(())
}

/// Read the schema version recorded in `user_version`.
pub fn read_user_version(conn: &Connection, path: &Path) -> Result<u32, StorageError> {
    conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map(|version| version as u32)
        .map_err(|err| from_rusqlite(path, err))
}

/// Delete a cache database and recreate it empty at the newest supported
/// schema version.
///
/// This is the explicit recovery path: it works regardless of database
/// content (corrupt or too new) because the old file is removed first.
pub fn clear(path: &Path, kind: DbKind) -> Result<(), StorageError> {
    remove_database_files(path)?;
    let _ = open_and_migrate(path, kind)?;
    Ok(())
}

/// Remove a database file together with its journal sidecars.
///
/// Missing files are not an error; every other failure is.
fn remove_database_files(path: &Path) -> Result<(), StorageError> {
    for candidate in database_files(path) {
        match std::fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StorageError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Ok(())
}

/// A database file and its journal sidecar paths.
pub fn database_files(path: &Path) -> [PathBuf; 4] {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    let mut journal = path.as_os_str().to_owned();
    journal.push("-journal");
    [
        path.to_owned(),
        PathBuf::from(wal),
        PathBuf::from(shm),
        PathBuf::from(journal),
    ]
}

/// Create the parent directory of a database path if needed.
fn ensure_parent(path: &Path) -> Result<(), StorageError> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    if parent.exists() {
        return Ok(());
    }
    // CONSENT GATE (gate 2): cache directories are persistent state;
    // creation requires explicit consent. Interactive use prompts;
    // noninteractive use fails closed with the typed error.
    let consented = match crate::config::consent::require_creation_consent(parent) {
        crate::config::consent::Consent::Allowed => parent.to_path_buf(),
        crate::config::consent::Consent::Refused { path } => {
            return Err(StorageError::DirectoryConsentRequired { path });
        }
    };
    // Create only the MISSING components below the deepest existing
    // ancestor (existing ancestors — including platform symlink roots
    // such as macOS /var — are traversed, never modified). Each created
    // or encountered component below that point must be a real
    // directory: a planted symlink must not redirect cache creation.
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = parent.to_path_buf();
    while !probe.exists() {
        let Some(name) = probe.file_name().map(|name| name.to_os_string()) else {
            return Err(StorageError::Io {
                path: probe.clone(),
                source: std::io::Error::other("cache path has no creatable components"),
            });
        };
        missing.push(name);
        let Some(parent) = probe.parent().map(Path::to_path_buf) else {
            return Err(StorageError::Io {
                path: probe.clone(),
                source: std::io::Error::other("cache path has no parent"),
            });
        };
        probe = parent;
    }
    let mut current = probe;
    for name in missing.iter().rev() {
        current.push(name);
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let real_dir = std::fs::symlink_metadata(&current)
                    .map(|metadata| metadata.file_type().is_dir())
                    .unwrap_or(false);
                if !real_dir {
                    return Err(StorageError::Io {
                        path: current.clone(),
                        source: std::io::Error::other(
                            "cache path component is not a real directory",
                        ),
                    });
                }
            }
            Err(source) => {
                return Err(StorageError::Io {
                    path: current.clone(),
                    source,
                });
            }
        }
    }
    // TOCTOU guard: what was created must be where consent pointed.
    if let (Ok(created), Some(consented)) = (
        std::fs::canonicalize(parent),
        crate::config::consent::canonical_path(&consented),
    ) && std::fs::canonicalize(&consented)
        .map(|canonical| canonical != created)
        .unwrap_or(true)
    {
        return Err(StorageError::Io {
            path: parent.to_owned(),
            source: std::io::Error::other(
                "created cache directory does not match the consented canonical path",
            ),
        });
    }
    Ok(())
}

/// Outcome of a non-mutating inspection probe.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// No database file exists (normal for a fresh repository).
    Missing,
    /// The file exists but is not a usable database.
    Corrupt { detail: String },
    /// The file was written by a newer Nisse.
    TooNew { found: u32, supported: u32 },
    /// The file is a readable database.
    Valid(ProbeData),
}

/// Read-only facts about an existing database.
#[derive(Debug)]
pub struct ProbeData {
    /// Recorded schema version.
    pub version: u32,
    /// Supported schema version for the database kind.
    pub supported: u32,
    /// Whether `PRAGMA quick_check` reported no problems.
    pub integrity_ok: bool,
    /// First problem reported by `quick_check`, if any.
    pub integrity_detail: String,
    /// Per required table: row count, or `None` when the table does not
    /// exist yet (an unmigrated or emptied database).
    pub tables: Vec<(&'static str, Option<u64>)>,
}

/// Inspect an existing database without mutating or migrating it.
///
/// Used by `nisse cache status`, which must surface corruption instead of
/// silently rebuilding it. Opening a WAL database read-write is expected
/// and may create empty journal sidecars; no logical content changes.
pub fn probe(path: &Path, kind: DbKind) -> Result<ProbeOutcome, StorageError> {
    if !path.is_file() {
        return Ok(ProbeOutcome::Missing);
    }
    let conn = Connection::open(path).map_err(|err| open_error(path, err))?;
    conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))
        .map_err(|err| open_error(path, err))?;
    if let Err(err) = validate_readable(&conn, path) {
        return match err {
            StorageError::Corrupt { message, .. } => Ok(ProbeOutcome::Corrupt { detail: message }),
            other => Err(other),
        };
    }
    let version = read_user_version(&conn, path)?;
    let supported = supported_version(kind);
    if version > supported {
        return Ok(ProbeOutcome::TooNew {
            found: version,
            supported,
        });
    }

    let integrity_detail = conn
        .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|err| err.to_string());
    let integrity_ok = integrity_detail == "ok";

    let mut tables = Vec::new();
    for name in kind.tables() {
        let rows = if table_exists(&conn, path, name)? {
            Some(count_rows(&conn, path, name)?)
        } else {
            None
        };
        tables.push((*name, rows));
    }

    Ok(ProbeOutcome::Valid(ProbeData {
        version,
        supported,
        integrity_ok,
        integrity_detail,
        tables,
    }))
}

/// Whether a table exists in the main schema.
fn table_exists(conn: &Connection, path: &Path, name: &str) -> Result<bool, StorageError> {
    conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .map_err(|err| from_rusqlite(path, err))
}

/// Count rows of an existing table.
fn count_rows(conn: &Connection, path: &Path, name: &str) -> Result<u64, StorageError> {
    let sql = format!("SELECT count(*) FROM \"{name}\"");
    conn.query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map(|count| count as u64)
        .map_err(|err| from_rusqlite(path, err))
}

/// Classify a rusqlite error for a database at `path`.
fn from_rusqlite(path: &Path, err: rusqlite::Error) -> StorageError {
    if is_corruption(&err) {
        StorageError::Corrupt {
            path: path.to_owned(),
            message: err.to_string(),
        }
    } else {
        StorageError::Query {
            path: path.to_owned(),
            message: err.to_string(),
        }
    }
}

/// Map an open/pragma failure.
fn open_error(path: &Path, err: rusqlite::Error) -> StorageError {
    if is_corruption(&err) {
        StorageError::Corrupt {
            path: path.to_owned(),
            message: err.to_string(),
        }
    } else {
        StorageError::Open {
            path: path.to_owned(),
            message: err.to_string(),
        }
    }
}

/// Whether an error represents a corrupt or not-a-database file, which
/// makes recovery-by-rebuild appropriate for regenerable caches.
fn is_corruption(err: &rusqlite::Error) -> bool {
    use rusqlite::ffi::{Error as SqliteError, ErrorCode};
    matches!(
        err,
        rusqlite::Error::SqliteFailure(
            SqliteError {
                code: ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase,
                ..
            },
            _,
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema::SHARED_TABLES;

    fn temp_path(label: &str, file: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("nisse-storage-{label}-"))
            .tempdir()
            .unwrap();
        let path = dir.path().join(file);
        (dir, path)
    }

    /// FTS5 virtual tables of the current schemas; each creates shadow
    /// tables named `<fts>_<suffix>` that are implementation detail.
    const FTS_TABLES: &[&str] = &["text_chunk_fts", "record_fts", "practice_fts"];

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        names
            .into_iter()
            .filter(|name| {
                !name.starts_with("sqlite_")
                    && !FTS_TABLES
                        .iter()
                        .any(|fts| name.starts_with(&format!("{fts}_")))
            })
            .collect()
    }

    /// Expected physical tables of a database kind: the required plan
    /// tables plus the full-text/meta tables of the current schema.
    fn expected_tables(kind: DbKind) -> Vec<&'static str> {
        let mut expected: Vec<&str> = kind.tables().to_vec();
        match kind {
            DbKind::Shared => expected.push("text_chunk_fts"),
            DbKind::Worktree => {
                expected.push("record_fts");
                expected.push("practice_fts");
                expected.push("meta");
            }
        }
        expected.sort_unstable();
        expected
    }

    #[test]
    fn fresh_open_creates_all_tables_at_current_version() {
        for kind in [DbKind::Shared, DbKind::Worktree] {
            let (_dir, path) = temp_path("fresh", kind.file_name());
            let opened = open_or_recover(&path, kind).unwrap();
            assert!(!opened.recovered);
            assert_eq!(
                read_user_version(&opened.conn, &path).unwrap(),
                supported_version(kind)
            );
            let mut names = table_names(&opened.conn);
            names.sort_unstable();
            assert_eq!(names, expected_tables(kind), "tables for {kind:?}");
        }
    }

    #[test]
    fn reopen_preserves_rows_and_schema_version() {
        let (_dir, path) = temp_path("reopen", "cache.sqlite");
        {
            let opened = open_or_recover(&path, DbKind::Shared).unwrap();
            opened
                .conn
                .execute(
                    "INSERT INTO review_feedback (review_id, finding_id, outcome, created_at)
                     VALUES ('r1', 'f1', 'fixed', '2026-01-01T00:00:00Z')",
                    [],
                )
                .unwrap();
        }
        let reopened = open_or_recover(&path, DbKind::Shared).unwrap();
        assert!(!reopened.recovered);
        assert_eq!(
            read_user_version(&reopened.conn, &path).unwrap(),
            supported_version(DbKind::Shared)
        );
        let count = reopened
            .conn
            .query_row("SELECT count(*) FROM review_feedback", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn version_zero_database_is_migrated_without_recovery() {
        let (_dir, path) = temp_path("migrate", "cache.sqlite");
        {
            // A valid but empty SQLite database records user_version 0.
            let conn = Connection::open(&path).unwrap();
            assert_eq!(
                conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        let opened = open_or_recover(&path, DbKind::Worktree).unwrap();
        assert!(!opened.recovered);
        assert_eq!(
            read_user_version(&opened.conn, &path).unwrap(),
            supported_version(DbKind::Worktree)
        );
        // Required tables now exist.
        assert!(table_exists(&opened.conn, &path, "manifest").unwrap());
    }

    #[test]
    fn newer_schema_version_is_typed_error_and_file_is_kept() {
        let (_dir, path) = temp_path("toonew", "cache.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 3_i64).unwrap();
        }
        let err = open_or_recover(&path, DbKind::Shared).unwrap_err();
        assert!(
            matches!(
                err,
                StorageError::SchemaTooNew {
                    found: 3,
                    supported: 2,
                    ..
                }
            ),
            "unexpected error: {err:?}"
        );
        // The too-new file must not be destroyed by the failed open.
        assert!(path.is_file());

        // Probing reports the same condition without deleting.
        assert!(matches!(
            probe(&path, DbKind::Shared).unwrap(),
            ProbeOutcome::TooNew {
                found: 3,
                supported: 2
            }
        ));
        assert!(path.is_file());

        // Explicit clear is allowed to rebuild it.
        clear(&path, DbKind::Shared).unwrap();
        let opened = open_or_recover(&path, DbKind::Shared).unwrap();
        assert_eq!(read_user_version(&opened.conn, &path).unwrap(), 2);
    }

    #[test]
    fn garbage_file_is_recovered_as_empty_database() {
        let (_dir, path) = temp_path("garbage", "cache.sqlite");
        std::fs::write(&path, b"this is definitely not a sqlite database\n").unwrap();

        let opened = open_or_recover(&path, DbKind::Shared).unwrap();
        assert!(opened.recovered, "garbage file must be rebuilt");
        assert_eq!(
            read_user_version(&opened.conn, &path).unwrap(),
            supported_version(DbKind::Shared)
        );
        let count = opened
            .conn
            .query_row("SELECT count(*) FROM content_artifact", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 0);

        // The rebuilt database opens normally on the next attempt.
        let reopened = open_or_recover(&path, DbKind::Shared).unwrap();
        assert!(!reopened.recovered);
    }

    #[test]
    fn remove_database_files_removes_database_and_all_sidecars() {
        let (_dir, path) = temp_path("sidecars", "cache.sqlite");
        let files = database_files(&path);
        for file in &files {
            std::fs::write(file, b"stale content").unwrap();
            assert!(file.is_file(), "{} must exist", file.display());
        }
        remove_database_files(&path).unwrap();
        for file in &files {
            assert!(!file.exists(), "{} must be removed", file.display());
        }
    }

    #[test]
    fn recovery_removes_stale_journal_sidecars_together_with_the_database() {
        let (_dir, path) = temp_path("garbage-sidecar", "cache.sqlite");
        std::fs::write(&path, b"this is definitely not a sqlite database\n").unwrap();
        let sidecar = database_files(&path)[1].clone();
        std::fs::write(&sidecar, b"stale wal content").unwrap();

        let opened = open_or_recover(&path, DbKind::Shared).unwrap();
        assert!(opened.recovered);
        drop(opened);
        // After the rebuilt connection closes, no stale sidecar content may
        // survive anywhere.
        let sidecar_content = std::fs::read(&sidecar).unwrap_or_default();
        assert_ne!(
            sidecar_content, b"stale wal content",
            "stale sidecar must not survive recovery"
        );
    }

    #[test]
    fn clear_removes_content_and_recreates_schema() {
        let (_dir, path) = temp_path("clear", "cache.sqlite");
        {
            let opened = open_or_recover(&path, DbKind::Shared).unwrap();
            opened
                .conn
                .execute(
                    "INSERT INTO content_artifact (content_key, byte_len, created_at)
                     VALUES ('git:abc', 3, '2026-01-01T00:00:00Z')",
                    [],
                )
                .unwrap();
        }
        clear(&path, DbKind::Shared).unwrap();
        let opened = open_or_recover(&path, DbKind::Shared).unwrap();
        assert!(!opened.recovered);
        assert_eq!(
            read_user_version(&opened.conn, &path).unwrap(),
            supported_version(DbKind::Shared)
        );
        let count = opened
            .conn
            .query_row("SELECT count(*) FROM content_artifact", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn clear_works_when_nothing_exists_yet() {
        let (_dir, path) = temp_path("clear-missing", "worktree.sqlite");
        assert!(!path.exists());
        clear(&path, DbKind::Worktree).unwrap();
        assert!(path.is_file());
        let opened = open_or_recover(&path, DbKind::Worktree).unwrap();
        assert_eq!(
            read_user_version(&opened.conn, &path).unwrap(),
            supported_version(DbKind::Worktree)
        );
    }

    #[test]
    fn probe_reports_missing_valid_and_corrupt_states() {
        let (_dir, path) = temp_path("probe", "cache.sqlite");

        // Missing before any use.
        assert!(matches!(
            probe(&path, DbKind::Shared).unwrap(),
            ProbeOutcome::Missing
        ));

        // Valid after creation.
        let _ = open_or_recover(&path, DbKind::Shared).unwrap();
        let ProbeOutcome::Valid(data) = probe(&path, DbKind::Shared).unwrap() else {
            panic!("expected valid probe");
        };
        assert_eq!(data.version, supported_version(DbKind::Shared));
        assert_eq!(data.supported, supported_version(DbKind::Shared));
        assert!(data.integrity_ok, "detail: {}", data.integrity_detail);
        assert_eq!(data.tables.len(), SHARED_TABLES.len());
        assert!(data.tables.iter().all(|(_, rows)| *rows == Some(0)));

        // Corrupt after the file is replaced with garbage.
        std::fs::write(&path, b"garbage bytes").unwrap();
        assert!(matches!(
            probe(&path, DbKind::Shared).unwrap(),
            ProbeOutcome::Corrupt { .. }
        ));
    }

    #[test]
    fn open_or_recover_on_directory_path_fails_without_deleting() {
        let (_dir, base) = temp_path("dirpath", "base");
        let dir_path = base.join("as-directory.sqlite");
        std::fs::create_dir_all(&dir_path).unwrap();
        let err = open_or_recover(&dir_path, DbKind::Shared).unwrap_err();
        assert!(
            !matches!(err, StorageError::Corrupt { .. }),
            "a directory is not corruption: {err:?}"
        );
        assert!(dir_path.is_dir(), "the directory must not be deleted");
    }

    #[test]
    fn concurrent_writers_are_serialized_by_the_busy_timeout() {
        // Two independent connections (models of two concurrent Nisse
        // processes) writing the same database: the busy timeout plus
        // WAL journaling must serialize them without corruption.
        let (dir, path) = temp_path("concurrent", "cache.sqlite");
        let first = open_or_recover(&path, DbKind::Shared).unwrap();

        // Hold an exclusive write transaction on the first connection.
        first
            .conn
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO review_feedback \n            (review_id, finding_id, outcome, created_at)\n            VALUES ('held', 'f1', 'fixed', '2026-01-01T00:00:00Z');")
            .unwrap();

        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let second = open_or_recover(&writer_path, DbKind::Shared).unwrap();
            second
                .conn
                .execute(
                    "INSERT INTO review_feedback (review_id, finding_id, outcome, created_at)\n                     VALUES ('waited', 'f2', 'accepted', '2026-01-01T00:00:01Z')",
                    [],
                )
                .expect("the busy timeout must wait for the held lock, not fail");
            second.conn
        });

        // Give the second writer time to block on the held lock, then
        // release it. If the writer somehow raced ahead, the test still
        // passes: both rows must be durable either way.
        std::thread::sleep(std::time::Duration::from_millis(300));
        first.conn.execute_batch("COMMIT;").unwrap();
        let second = writer.join().expect("writer thread panicked");
        drop(second);

        let check = open_or_recover(&path, DbKind::Shared).unwrap();
        let reviews: Vec<String> = check
            .conn
            .prepare("SELECT review_id FROM review_feedback ORDER BY review_id")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(reviews, vec!["held".to_string(), "waited".to_string()]);
        drop(check);
        drop(dir);
    }
}
