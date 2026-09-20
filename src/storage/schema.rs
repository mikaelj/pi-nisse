//! Cache database schemas and migrations.
//!
//! Two regenerable SQLite databases exist (plan section 29):
//!
//! - the **shared** cache under `$GIT_COMMON_DIR/nisse/cache.sqlite`, holding
//!   content-addressed artifacts reusable across worktrees and branches;
//! - the **worktree** cache under `$GIT_DIR/nisse/worktree.sqlite`, holding
//!   state specific to one worktree (manifest, per-path content keys,
//!   record index, refresh statistics).
//!
//! Every database records its schema version in the SQLite `user_version`
//! pragma. Schema changes are forward-only migrations: each migration
//! raises `user_version` by applying one batch of SQL. A database written
//! by a *newer* Nisse (higher `user_version`) is never opened and never
//! destroyed by this version; it fails with a typed error instead.

/// Kind of cache database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    /// `$GIT_COMMON_DIR/nisse/cache.sqlite` — shared across worktrees.
    Shared,
    /// `$GIT_DIR/nisse/worktree.sqlite` — private to one worktree.
    Worktree,
}

impl DbKind {
    /// Human-readable label used in reports.
    pub fn label(self) -> &'static str {
        match self {
            DbKind::Shared => "shared cache",
            DbKind::Worktree => "worktree cache",
        }
    }

    /// Logical file name of this database kind.
    pub fn file_name(self) -> &'static str {
        match self {
            DbKind::Shared => "cache.sqlite",
            DbKind::Worktree => "worktree.sqlite",
        }
    }

    /// Required tables of this database kind, in report order.
    pub fn tables(self) -> &'static [&'static str] {
        match self {
            DbKind::Shared => SHARED_TABLES,
            DbKind::Worktree => WORKTREE_TABLES,
        }
    }

    /// Migrations for this database kind, ordered by target version.
    pub fn migrations(self) -> &'static [Migration] {
        match self {
            DbKind::Shared => SHARED_MIGRATIONS,
            DbKind::Worktree => WORKTREE_MIGRATIONS,
        }
    }
}

/// One forward-only schema migration.
#[derive(Debug, Clone)]
pub struct Migration {
    /// Schema version in effect after this migration is applied.
    pub version: u32,
    /// Batch of SQL statements applied inside one transaction.
    pub sql: &'static str,
}

/// Shared-cache tables (plan section 29).
pub const SHARED_TABLES: &[&str] = &[
    "content_artifact",
    "parsed_artifact",
    "symbol",
    "reference",
    "text_chunk",
    "embedding",
    "review_event",
    "review_feedback",
    "pending_learning",
];

/// Worktree-cache tables (plan section 29).
pub const WORKTREE_TABLES: &[&str] = &["manifest", "path_content", "record_index", "refresh_state"];

/// Schema version of the current shared cache schema.
pub const SHARED_SCHEMA_VERSION: u32 = 2;

/// Schema version of the current worktree cache schema.
pub const WORKTREE_SCHEMA_VERSION: u32 = 3;

/// Highest schema version understood for a database kind.
pub fn supported_version(kind: DbKind) -> u32 {
    kind.migrations()
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0)
}

const SHARED_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r#"
CREATE TABLE content_artifact (
    content_key TEXT PRIMARY KEY,
    byte_len    INTEGER NOT NULL,
    created_at  TEXT    NOT NULL
);

CREATE TABLE parsed_artifact (
    id              INTEGER PRIMARY KEY,
    content_key     TEXT    NOT NULL,
    language        TEXT    NOT NULL,
    parser_version  TEXT    NOT NULL,
    artifact        TEXT    NOT NULL,
    created_at      TEXT    NOT NULL,
    UNIQUE (content_key, language, parser_version)
);

CREATE TABLE symbol (
    id                INTEGER PRIMARY KEY,
    parsed_artifact_id INTEGER NOT NULL REFERENCES parsed_artifact(id),
    name              TEXT    NOT NULL,
    kind              TEXT    NOT NULL,
    path              TEXT    NOT NULL,
    line_start        INTEGER,
    line_end          INTEGER,
    byte_start        INTEGER,
    byte_end          INTEGER
);
CREATE INDEX idx_symbol_name ON symbol(name);
CREATE INDEX idx_symbol_artifact ON symbol(parsed_artifact_id);

CREATE TABLE reference (
    id                INTEGER PRIMARY KEY,
    parsed_artifact_id INTEGER NOT NULL REFERENCES parsed_artifact(id),
    name              TEXT    NOT NULL,
    path              TEXT    NOT NULL,
    line              INTEGER
);
CREATE INDEX idx_reference_name ON reference(name);

CREATE TABLE text_chunk (
    id          INTEGER PRIMARY KEY,
    content_key TEXT    NOT NULL,
    path        TEXT    NOT NULL,
    chunk_index INTEGER NOT NULL,
    text        TEXT    NOT NULL,
    UNIQUE (content_key, path, chunk_index)
);

CREATE TABLE embedding (
    id         INTEGER PRIMARY KEY,
    chunk_id   INTEGER NOT NULL REFERENCES text_chunk(id),
    model      TEXT    NOT NULL,
    dim        INTEGER NOT NULL,
    vector     BLOB    NOT NULL,
    created_at TEXT    NOT NULL,
    UNIQUE (chunk_id, model)
);

CREATE TABLE review_event (
    review_id  TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    snapshot   TEXT NOT NULL,
    payload    TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE review_feedback (
    review_id  TEXT NOT NULL,
    finding_id TEXT NOT NULL,
    outcome    TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (review_id, finding_id)
);

CREATE TABLE pending_learning (
    id         INTEGER PRIMARY KEY,
    kind       TEXT    NOT NULL,
    payload    TEXT    NOT NULL,
    created_at TEXT    NOT NULL
);
"#,
    },
    Migration {
        version: 2,
        sql: r#"
-- Full-text index over the text chunks used for baseline retrieval
-- (plan section 30). Kept in sync by index refresh; backfilled here
-- from any chunks written by schema version 1.
CREATE VIRTUAL TABLE text_chunk_fts USING fts5(
    text,
    content_key UNINDEXED,
    path       UNINDEXED
);
INSERT INTO text_chunk_fts (rowid, text, content_key, path)
    SELECT id, text, content_key, path FROM text_chunk;
"#,
    },
];

const WORKTREE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r#"
CREATE TABLE manifest (
    path        TEXT PRIMARY KEY,
    content_key TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE path_content (
    path        TEXT    NOT NULL,
    content_key TEXT    NOT NULL,
    language    TEXT,
    byte_len    INTEGER NOT NULL,
    updated_at  TEXT    NOT NULL,
    PRIMARY KEY (path, content_key)
);

CREATE TABLE record_index (
    record_id  TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    path       TEXT NOT NULL,
    indexed_at TEXT NOT NULL
);

CREATE TABLE refresh_state (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    files_seen      INTEGER NOT NULL DEFAULT 0,
    files_reused    INTEGER NOT NULL DEFAULT 0,
    files_parsed    INTEGER NOT NULL DEFAULT 0,
    files_removed   INTEGER NOT NULL DEFAULT 0,
    duration_ms     REAL    NOT NULL DEFAULT 0.0,
    last_refresh_at TEXT
);
"#,
    },
    Migration {
        version: 2,
        sql: r#"
-- Full-text index over durable project records (facts and lessons)
-- used for baseline retrieval (plan section 30). Populated on demand
-- from the .nisse tree; observations are raw learning data and are
-- not retrieved as project knowledge.
CREATE VIRTUAL TABLE record_fts USING fts5(
    subject,
    text,
    tags,
    record_id UNINDEXED,
    kind      UNINDEXED,
    state     UNINDEXED
);

-- Full-text index over the bundled practice packs.
CREATE VIRTUAL TABLE practice_fts USING fts5(
    title,
    statement,
    tags,
    practice_id UNINDEXED,
    pack_id     UNINDEXED
);

-- Small key-value metadata (for example the snapshot binding of the
-- last refresh and the indexed practice-catalog version).
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#,
    },
    Migration {
        version: 3,
        sql: r#"
-- Fact-staleness marking (plan section 48): potentially stale facts
-- are marked and downranked at retrieval time. The flag is a
-- regenerable cache of a snapshot-dependent assessment, recomputed
-- by every record sync (`sync_records`).
ALTER TABLE record_index ADD COLUMN potentially_stale INTEGER NOT NULL DEFAULT 0;
"#,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_ordered_by_unique_version() {
        for kind in [DbKind::Shared, DbKind::Worktree] {
            let mut previous = 0;
            for migration in kind.migrations() {
                assert!(
                    migration.version > previous,
                    "{kind:?} migrations must be strictly increasing"
                );
                previous = migration.version;
            }
        }
    }

    #[test]
    fn supported_versions_are_current() {
        assert_eq!(supported_version(DbKind::Shared), SHARED_SCHEMA_VERSION);
        assert_eq!(supported_version(DbKind::Worktree), WORKTREE_SCHEMA_VERSION);
        assert_eq!(supported_version(DbKind::Shared), 2);
        assert_eq!(supported_version(DbKind::Worktree), 3);
    }

    #[test]
    fn worktree_version_two_databases_migrate_to_the_staleness_column() {
        use super::super::db::{open_or_recover, read_user_version};
        use rusqlite::Connection;

        // Build a worktree version-2 database with one indexed record,
        // as written by Nisse before fact-staleness marking. Applying
        // all migrations at once leaves the database at version 3, so
        // the version is reset to 2 after the rows are written.
        let dir = tempfile::Builder::new()
            .prefix("nisse-schema-migrate-worktree-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("worktree.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(WORKTREE_MIGRATIONS[0].sql).unwrap();
            conn.execute_batch(WORKTREE_MIGRATIONS[1].sql).unwrap();
            conn.execute(
                "INSERT INTO record_index (record_id, kind, path, indexed_at) \
                 VALUES ('01A', 'fact', 'people/a/01A.json', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2_i64).unwrap();
        }

        let opened = open_or_recover(&path, DbKind::Worktree).unwrap();
        assert!(!opened.recovered);
        assert_eq!(read_user_version(&opened.conn, &path).unwrap(), 3);
        let (stale, count): (i64, i64) = opened
            .conn
            .query_row(
                "SELECT potentially_stale, count(*) FROM record_index WHERE record_id = '01A'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1, "the pre-existing row survives");
        assert_eq!(stale, 0, "migrated rows default to not stale");
    }

    #[test]
    fn version_one_databases_migrate_forward_with_backfill() {
        use super::super::db::read_user_version;
        use rusqlite::Connection;

        // Build a shared version-1 database holding one text chunk, as
        // written by Nisse at schema version 1.
        let dir = tempfile::Builder::new()
            .prefix("nisse-schema-migrate-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("cache.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SHARED_MIGRATIONS[0].sql).unwrap();
            conn.pragma_update(None, "user_version", 1_i64).unwrap();
            conn.execute(
                "INSERT INTO text_chunk (content_key, path, chunk_index, text) \
                 VALUES ('git:abc', 'src/lib.rs', 0, 'fn alpha L1-L2')",
                [],
            )
            .unwrap();
        }

        let opened = super::super::db::open_or_recover(&path, DbKind::Shared).unwrap();
        assert!(!opened.recovered);
        assert_eq!(read_user_version(&opened.conn, &path).unwrap(), 2);
        let fts_rows: i64 = opened
            .conn
            .query_row(
                "SELECT count(*) FROM text_chunk_fts WHERE text_chunk_fts MATCH 'alpha'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_rows, 1, "v1 chunks must be backfilled into FTS");
    }

    #[test]
    fn labels_and_file_names_are_stable() {
        assert_eq!(DbKind::Shared.label(), "shared cache");
        assert_eq!(DbKind::Worktree.label(), "worktree cache");
        assert_eq!(DbKind::Shared.file_name(), "cache.sqlite");
        assert_eq!(DbKind::Worktree.file_name(), "worktree.sqlite");
    }
}
