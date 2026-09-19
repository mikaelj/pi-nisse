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

/// Schema version of the initial shared cache schema.
pub const SHARED_SCHEMA_VERSION: u32 = 1;

/// Schema version of the initial worktree cache schema.
pub const WORKTREE_SCHEMA_VERSION: u32 = 1;

/// Highest schema version understood for a database kind.
pub fn supported_version(kind: DbKind) -> u32 {
    kind.migrations()
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0)
}

const SHARED_MIGRATIONS: &[Migration] = &[Migration {
    version: SHARED_SCHEMA_VERSION,
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
}];

const WORKTREE_MIGRATIONS: &[Migration] = &[Migration {
    version: WORKTREE_SCHEMA_VERSION,
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
}];

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
        assert_eq!(supported_version(DbKind::Shared), 1);
        assert_eq!(supported_version(DbKind::Worktree), 1);
    }

    #[test]
    fn labels_and_file_names_are_stable() {
        assert_eq!(DbKind::Shared.label(), "shared cache");
        assert_eq!(DbKind::Worktree.label(), "worktree cache");
        assert_eq!(DbKind::Shared.file_name(), "cache.sqlite");
        assert_eq!(DbKind::Worktree.file_name(), "worktree.sqlite");
    }
}
