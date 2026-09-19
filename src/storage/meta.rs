//! Key-value metadata of the worktree cache.
//!
//! The `meta` table (worktree schema version 2) holds small markers that
//! must survive between processes, such as the snapshot binding of the
//! last index refresh and the version of the indexed practice catalog.

use std::path::Path;

use rusqlite::Connection;

use super::db::StorageError;

/// Meta key holding the JSON [`SnapshotDescriptor`] of the snapshot the
/// worktree index (manifest, path_content) was last refreshed for.
pub const META_LAST_REFRESH_SNAPSHOT: &str = "last_refresh_snapshot";

/// Meta key holding the version marker of the indexed practice catalog.
pub const META_PRACTICE_CATALOG_VERSION: &str = "practice_catalog_version";

/// Read one meta value.
pub fn meta_get(
    conn: &Connection,
    db_path: &Path,
    key: &str,
) -> Result<Option<String>, StorageError> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .map(Some)
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(StorageError::Query {
            path: db_path.to_owned(),
            message: other.to_string(),
        }),
    })
}

/// Write one meta value.
pub fn meta_set(
    conn: &Connection,
    db_path: &Path,
    key: &str,
    value: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \n         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )
    .map(|_| ())
    .map_err(|err| StorageError::Query {
        path: db_path.to_owned(),
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::open_or_recover;
    use crate::storage::schema::DbKind;

    fn temp_worktree() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("nisse-meta-")
            .tempdir()
            .unwrap();
        let path = dir.path().join("worktree.sqlite");
        (dir, path)
    }

    #[test]
    fn meta_roundtrips_and_upserts() {
        let (_dir, path) = temp_worktree();
        let opened = open_or_recover(&path, DbKind::Worktree).unwrap();
        let conn = &opened.conn;

        assert_eq!(meta_get(conn, &path, "missing").unwrap(), None);

        meta_set(conn, &path, META_LAST_REFRESH_SNAPSHOT, "one").unwrap();
        assert_eq!(
            meta_get(conn, &path, META_LAST_REFRESH_SNAPSHOT).unwrap(),
            Some("one".to_string())
        );

        // Setting again overwrites.
        meta_set(conn, &path, META_LAST_REFRESH_SNAPSHOT, "two").unwrap();
        assert_eq!(
            meta_get(conn, &path, META_LAST_REFRESH_SNAPSHOT).unwrap(),
            Some("two".to_string())
        );
    }
}
