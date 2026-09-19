//! Snapshot membership of the worktree index.
//!
//! Retrieval must prove that every source entry belongs to the target
//! snapshot (plan section 26): the shared cache holds artifacts from
//! many branches and worktrees, and cache presence never implies
//! membership. The worktree cache's `path_content` table — written by
//! the last index refresh for exactly one target snapshot — is the
//! membership authority. [`SnapshotMembership`] loads it once and turns
//! content lookups into snapshot-local paths.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;

use super::error::RetrievalError;

/// Content-to-path membership of one target snapshot.
#[derive(Debug, Clone, Default)]
pub struct SnapshotMembership {
    /// `(content_key, language)` -> paths carrying that content as that
    /// language, sorted.
    by_content: BTreeMap<(String, String), Vec<String>>,
    /// `content_key` -> paths carrying that content under any indexed
    /// language, sorted.
    by_key: BTreeMap<String, Vec<String>>,
    /// Every indexed path of the snapshot.
    paths: Vec<String>,
}

impl SnapshotMembership {
    /// Load membership from the worktree cache.
    pub fn load(conn: &Connection, db_path: &Path) -> Result<Self, RetrievalError> {
        let mut stmt = conn
            .prepare("SELECT path, content_key, language FROM path_content")
            .map_err(|err| query_error(db_path, err))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|err| query_error(db_path, err))?;

        let mut membership = SnapshotMembership::default();
        for row in rows {
            let (path, content_key, language) = row.map_err(|err| query_error(db_path, err))?;
            let language = language.unwrap_or_default();
            membership
                .by_content
                .entry((content_key.clone(), language))
                .or_default()
                .push(path.clone());
            membership
                .by_key
                .entry(content_key)
                .or_default()
                .push(path.clone());
            membership.paths.push(path);
        }
        for paths in membership.by_content.values_mut() {
            paths.sort();
            paths.dedup();
        }
        for paths in membership.by_key.values_mut() {
            paths.sort();
            paths.dedup();
        }
        membership.paths.sort();
        membership.paths.dedup();
        Ok(membership)
    }

    /// Paths in this snapshot carrying `content_key` parsed as
    /// `language`.
    pub fn paths_for(&self, content_key: &str, language: &str) -> &[String] {
        self.by_content
            .get(&(content_key.to_owned(), language.to_owned()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Paths in this snapshot carrying `content_key` under any indexed
    /// language (identical bytes can appear under different extensions
    /// and therefore languages).
    pub fn paths_for_content(&self, content_key: &str) -> &[String] {
        self.by_key
            .get(content_key)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Whether `path` is part of the snapshot.
    pub fn contains_path(&self, path: &str) -> bool {
        self.paths.binary_search(&path.to_owned()).is_ok()
    }

    /// Number of distinct indexed paths.
    pub fn path_count(&self) -> usize {
        self.paths.len()
    }

    /// Whether the snapshot indexes nothing.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Map a rusqlite failure into a typed storage error.
fn query_error(db_path: &Path, err: rusqlite::Error) -> RetrievalError {
    RetrievalError::Storage(crate::storage::StorageError::Query {
        path: db_path.to_owned(),
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::open_or_recover;
    use crate::storage::schema::DbKind;

    #[test]
    fn membership_maps_content_and_language_to_paths() {
        let dir = tempfile::Builder::new()
            .prefix("nisse-membership-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("worktree.sqlite");
        let opened = open_or_recover(&path, DbKind::Worktree).unwrap();
        opened
            .conn
            .execute(
                "INSERT INTO path_content (path, content_key, language, byte_len, updated_at) VALUES \
                 ('src/a.rs', 'git:one', 'rust', 1, '2026-01-01T00:00:00Z'), \
                 ('copy/a.rs', 'git:one', 'rust', 1, '2026-01-01T00:00:00Z'), \
                 ('src/b.rs', 'git:two', 'rust', 2, '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();

        let membership = SnapshotMembership::load(&opened.conn, &path).unwrap();
        assert_eq!(membership.path_count(), 3);
        assert_eq!(
            membership.paths_for("git:one", "rust"),
            ["copy/a.rs".to_string(), "src/a.rs".to_string()]
        );
        assert!(membership.paths_for("git:one", "python").is_empty());
        assert!(membership.paths_for("git:missing", "rust").is_empty());
        let any = membership.paths_for_content("git:one");
        assert_eq!(any, ["copy/a.rs".to_string(), "src/a.rs".to_string()]);
        assert!(membership.paths_for_content("git:missing").is_empty());
        assert!(membership.contains_path("src/b.rs"));
        assert!(!membership.contains_path("src/other.rs"));
        assert!(!membership.is_empty());
    }

    #[test]
    fn empty_database_yields_empty_membership() {
        let dir = tempfile::Builder::new()
            .prefix("nisse-membership-empty-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("worktree.sqlite");
        let opened = open_or_recover(&path, DbKind::Worktree).unwrap();
        let membership = SnapshotMembership::load(&opened.conn, &path).unwrap();
        assert!(membership.is_empty());
        assert_eq!(membership.path_count(), 0);
    }
}
