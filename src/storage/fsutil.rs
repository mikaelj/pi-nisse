//! Atomic filesystem primitives for durable state.
//!
//! Durable files must never appear partially written and must never be
//! overwritten silently. Both helpers follow the same pattern: write the
//! full content to a dot-prefixed temporary file in the destination
//! directory, fsync it, and then publish it with a single atomic
//! filesystem operation. A crash at any point leaves either the old
//! state or an ignorable `.nisse-tmp-*` file — never a partial record.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::domain::ids::new_ulid;

/// Prefix of temporary files created while publishing durable writes.
pub const TEMP_PREFIX: &str = ".nisse-tmp-";

/// Outcome of an exclusive (create-new-only) write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusiveWrite {
    /// The file was created at the destination.
    Created,
    /// The destination already existed; nothing was changed.
    Exists,
}

/// Atomically create a file with the given content, refusing to
/// overwrite an existing file.
///
/// Publication uses `hard_link`: linking the fully written temporary
/// file to the destination fails with `AlreadyExists` when the
/// destination is present, which makes immutable-record creation
/// race-free on Unix and Windows. This is the write path for durable
/// records.
pub fn atomic_write_exclusive(path: &Path, bytes: &[u8]) -> io::Result<ExclusiveWrite> {
    // Directory creation is centralized in the consent-gated helpers
    // (records walker, cache ensure_parent, config-dir creator); the
    // parent must already exist.
    let parent = parent_dir(path)?;
    let temp = temp_path(parent);
    write_and_sync(&temp, bytes)?;
    let outcome = match fs::hard_link(&temp, path) {
        Ok(()) => ExclusiveWrite::Created,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => ExclusiveWrite::Exists,
        Err(err) => {
            let _ = fs::remove_file(&temp);
            return Err(err);
        }
    };
    let _ = fs::remove_file(&temp);
    Ok(outcome)
}

/// Atomically replace a file with the given content.
///
/// Publication uses `rename`, so readers observe either the old or the
/// new content, never a partial write. After the rename the containing
/// directory is synced best-effort (Unix), narrowing — not eliminating —
/// the crash window in which the rename could be rolled back; Windows
/// has no directory sync here. This is the write path for trusted local
/// configuration files.
pub fn atomic_write_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Directory creation is centralized in the consent-gated helpers;
    // the parent must already exist.
    let parent = parent_dir(path)?;
    let temp = temp_path(parent);
    write_and_sync(&temp, bytes)?;
    match fs::rename(&temp, path) {
        Ok(()) => {
            best_effort_parent_sync(parent);
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Atomically replace a file whose content is private (mode `0600` on
/// Unix from creation, not only after a post-rename chmod).
///
/// Used for files that carry identity or authorization data: the
/// temporary file is created with restrictive permissions, so no
/// crash window ever leaves the published file group/world-readable.
pub fn atomic_write_replace_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // Directory creation is centralized in the consent-gated helpers;
    // the parent must already exist.
    let parent = parent_dir(path)?;
    let temp = temp_path(parent);
    write_and_sync_private(&temp, bytes)?;
    match fs::rename(&temp, path) {
        Ok(()) => {
            best_effort_parent_sync(parent);
            Ok(())
        }
        Err(err) => {
            let _ = fs::remove_file(&temp);
            Err(err)
        }
    }
}

/// Best-effort directory sync after a publish: makes the rename (and
/// any just-created directory entries below `parent`) durable when the
/// platform allows opening the directory. Strictly best-effort — the
/// file is already published at this point, so neither an open nor a
/// sync failure may turn the publication into a reported failure
/// (notably, `File::open` on a directory fails on Windows, where the
/// sync is simply skipped).
pub fn best_effort_parent_sync(parent: &Path) {
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

/// The parent directory of `path`, or an error for a rootless path.
fn parent_dir(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| io::Error::other(format!("{} has no parent directory", path.display())))
}

/// Unique dot-prefixed temporary path inside `dir`.
fn temp_path(dir: &Path) -> PathBuf {
    dir.join(format!("{TEMP_PREFIX}{}", new_ulid()))
}

/// Write the full content to `path` and fsync it.
fn write_and_sync(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// [`write_and_sync`] creating the file with mode `0600` on Unix, so a
/// private file never exists with looser permissions (not even between
/// creation and a post-publish chmod).
#[cfg(unix)]
fn write_and_sync_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_and_sync_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_and_sync(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_write_creates_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let path = nested.join("record.json");

        assert_eq!(
            atomic_write_exclusive(&path, b"contents").unwrap(),
            ExclusiveWrite::Created
        );
        assert_eq!(fs::read(&path).unwrap(), b"contents");
    }

    #[test]
    fn exclusive_write_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        atomic_write_exclusive(&path, b"first").unwrap();

        assert_eq!(
            atomic_write_exclusive(&path, b"second").unwrap(),
            ExclusiveWrite::Exists
        );
        // The original content is untouched.
        assert_eq!(fs::read(&path).unwrap(), b"first");
    }

    #[test]
    fn exclusive_write_leaves_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        atomic_write_exclusive(&path, b"x").unwrap();
        atomic_write_exclusive(&path, b"y").unwrap();

        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["record.json".to_string()]);
    }

    #[test]
    fn private_replace_writes_content_with_restricted_mode_from_creation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.toml");
        atomic_write_replace_private(&path, b"first").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"first");

        atomic_write_replace_private(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");

        // fsutil has no post-publish chmod, so asserting the final mode
        // here proves the file was CREATED private (mode 0600 on Unix —
        // no window with looser permissions ever existed).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn replace_write_overwrites_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        atomic_write_replace(&path, b"old").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"old");

        atomic_write_replace(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");

        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["config.toml".to_string()]);
    }

    #[test]
    fn writes_require_an_existing_parent_directory() {
        // Directory creation is centralized in the consent-gated
        // helpers (records walker, cache ensure_parent, config-dir
        // creator); the atomic writers themselves demand an existing
        // parent so no ungated creation path can hide behind them.
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a").join("b").join("record.json");
        assert!(atomic_write_exclusive(&deep, b"x").is_err());

        let deep2 = dir.path().join("x").join("y").join("config.toml");
        assert!(atomic_write_replace(&deep2, b"x").is_err());
        assert!(atomic_write_replace_private(&deep2, b"x").is_err());

        let nested = dir.path().join("made").join("config.toml");
        std::fs::create_dir(nested.parent().unwrap()).unwrap();
        atomic_write_replace_private(&nested, b"x").unwrap();
        assert_eq!(fs::read(&nested).unwrap(), b"x");
    }
}
