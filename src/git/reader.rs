//! Snapshot-safe source readers.
//!
//! The reader exposes exactly one side of a resolved snapshot: the target
//! side. Reads resolve against the target definition — final working tree,
//! index, or commit tree — never against "whatever happens to be on disk"
//! or a shared cache. A path that is not part of the target simply reads as
//! absent ([`None`](SnapshotReader::read)); cache or index presence never
//! proves snapshot membership.
//!
//! Git is the semantic authority for content: index and tree reads go
//! through blob object IDs listed by Git, and working-tree reads reproduce
//! Git's view of files, including symlinks whose stored content is the link
//! target rather than the target file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::error::GitError;
use super::repo::{IndexEntry, ObjectKind, RepoContext, TreeEntry};
use super::snapshot::{ResolvedSnapshot, SnapshotTarget};

/// One readable file of a snapshot's target side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFile {
    /// Repository-relative path.
    pub path: String,
    /// Full file content.
    pub bytes: Vec<u8>,
    /// Blob object ID when the content comes from the Git object database
    /// (index or tree targets). `None` for working-tree content; the
    /// content index derives `sha256:` content keys for that side.
    pub blob_oid: Option<String>,
}

/// One listed file of a snapshot's target side, with its Git blob object
/// ID when the side defines one.
///
/// Index and commit targets know their blob object IDs from Git listings;
/// working-tree content has none (its content key is computed by hashing
/// the actual bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotListing {
    /// Repository-relative path.
    pub path: String,
    /// Blob object ID when this side is defined by Git objects.
    pub blob_oid: Option<String>,
}

/// Read-only accessor for the target side of a resolved snapshot.
#[derive(Debug)]
pub struct SnapshotReader<'a> {
    ctx: &'a RepoContext,
    target: &'a SnapshotTarget,
    /// Path-to-entry map for tree targets.
    tree: BTreeMap<String, TreeEntry>,
    /// Stage-0 index entries for index targets.
    index: BTreeMap<String, IndexEntry>,
}

/// Upper bound on blob bytes materialized to answer a bounded prefix
/// request on Git object sides. `git cat-file` cannot stream a partial
/// blob, so without this bound a prefix read of a huge extensionless
/// binary would load the entire object into memory; such blobs read as
/// absent for prefix purposes (they are never indexable anyway).
const OBJECT_PREFIX_READ_CAP: u64 = 1 << 20;

impl<'a> SnapshotReader<'a> {
    /// Build a reader for the target side of `resolved`.
    ///
    /// Tree and index targets are listed once through Git so every later
    /// read is an exact membership lookup.
    pub fn new(ctx: &'a RepoContext, resolved: &'a ResolvedSnapshot) -> Result<Self, GitError> {
        let (tree, index) = match &resolved.target {
            SnapshotTarget::Working => (BTreeMap::new(), BTreeMap::new()),
            SnapshotTarget::Index => {
                let mut index = BTreeMap::new();
                for entry in ctx.index_entries()? {
                    if entry.stage == 0 {
                        index.insert(entry.path.clone(), entry);
                    }
                }
                (BTreeMap::new(), index)
            }
            SnapshotTarget::Commit { tree, .. } => {
                let entries = ctx.list_tree(tree.as_str())?;
                let tree = entries
                    .into_iter()
                    .map(|entry| (entry.path.clone(), entry))
                    .collect();
                (tree, BTreeMap::new())
            }
        };
        Ok(SnapshotReader {
            ctx,
            target: &resolved.target,
            tree,
            index,
        })
    }

    /// List all paths of the target side, sorted and deduplicated.
    ///
    /// For the working-tree target this is the final state: tracked files
    /// that still exist plus untracked non-ignored files. Files deleted
    /// from the working tree are absent.
    pub fn list_files(&self) -> Result<Vec<String>, GitError> {
        match self.target {
            SnapshotTarget::Working => {
                let mut paths = std::collections::BTreeSet::new();
                for path in self.ctx.tracked_files()? {
                    if self.working_file_exists(&path)? {
                        paths.insert(path);
                    }
                }
                for path in self.ctx.untracked_non_ignored()? {
                    paths.insert(path);
                }
                Ok(paths.into_iter().collect())
            }
            SnapshotTarget::Index => Ok(self.index.keys().cloned().collect()),
            SnapshotTarget::Commit { .. } => Ok(self.tree.keys().cloned().collect()),
        }
    }

    /// List all paths of the target side together with their Git blob
    /// object IDs, sorted and deduplicated.
    ///
    /// Membership semantics match [`SnapshotReader::list_files`]. Index and
    /// commit targets carry the blob object ID of every file (submodule
    /// gitlinks have no readable blob and are excluded); working-tree files
    /// carry `None` because their authoritative content is the file bytes.
    pub fn list_files_with_oids(&self) -> Result<Vec<SnapshotListing>, GitError> {
        match self.target {
            SnapshotTarget::Working => Ok(self
                .list_files()?
                .into_iter()
                .map(|path| SnapshotListing {
                    path,
                    blob_oid: None,
                })
                .collect()),
            SnapshotTarget::Index => Ok(self
                .index
                .values()
                .map(|entry| SnapshotListing {
                    path: entry.path.clone(),
                    blob_oid: Some(entry.oid.clone()),
                })
                .collect()),
            SnapshotTarget::Commit { .. } => Ok(self
                .tree
                .values()
                .filter(|entry| entry.kind == ObjectKind::Blob)
                .map(|entry| SnapshotListing {
                    path: entry.path.clone(),
                    blob_oid: Some(entry.oid.clone()),
                })
                .collect()),
        }
    }

    /// Read one file from the target side.
    ///
    /// Returns `None` when the path is not part of the target snapshot.
    /// Fails with a typed error when the path is malformed or escapes the
    /// snapshot.
    pub fn read(&self, path: &str) -> Result<Option<SnapshotFile>, GitError> {
        validate_relative_path(path)?;
        match self.target {
            SnapshotTarget::Working => self.read_working(path),
            SnapshotTarget::Index => self.read_index(path),
            SnapshotTarget::Commit { .. } => self.read_tree(path),
        }
    }

    /// Read at most `limit` bytes of one file from the target side.
    ///
    /// Membership semantics match [`SnapshotReader::read`]. The working
    /// tree serves a genuinely bounded read (enough for, say, shebang
    /// detection without reading whole files); Git object sides cannot
    /// serve partial blobs through the installed `git`, so their bytes
    /// are read and then truncated to `limit`.
    pub fn read_prefix(&self, path: &str, limit: usize) -> Result<Option<Vec<u8>>, GitError> {
        validate_relative_path(path)?;
        match self.target {
            SnapshotTarget::Working => self.read_working_prefix(path, limit),
            SnapshotTarget::Index => {
                let Some(entry) = self.index.get(path) else {
                    return Ok(None);
                };
                if self.ctx.blob_size(&entry.oid)? > OBJECT_PREFIX_READ_CAP {
                    return Ok(None);
                }
                Ok(self
                    .read_index(path)?
                    .map(|file| file.bytes[..file.bytes.len().min(limit)].to_vec()))
            }
            SnapshotTarget::Commit { .. } => {
                let Some(entry) = self.tree.get(path) else {
                    return Ok(None);
                };
                // Gitlinks (submodules) have no readable blob here.
                if entry.kind != ObjectKind::Blob {
                    return Ok(None);
                }
                if self.ctx.blob_size(&entry.oid)? > OBJECT_PREFIX_READ_CAP {
                    return Ok(None);
                }
                Ok(self
                    .read_tree(path)?
                    .map(|file| file.bytes[..file.bytes.len().min(limit)].to_vec()))
            }
        }
    }

    /// Read a bounded prefix from the final working-tree state.
    fn read_working_prefix(&self, path: &str, limit: usize) -> Result<Option<Vec<u8>>, GitError> {
        use std::io::Read as _;

        let full = self.worktree_join(path)?;
        let metadata = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(GitError::Read {
                    path: PathBuf::from(path),
                    source: err,
                });
            }
        };
        // Git stores a symlink's content as the link target string; a
        // prefix read never follows the link.
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&full).map_err(|err| GitError::Read {
                path: PathBuf::from(path),
                source: err,
            })?;
            let bytes = target
                .as_os_str()
                .to_string_lossy()
                .into_owned()
                .into_bytes();
            return Ok(Some(bytes[..bytes.len().min(limit)].to_vec()));
        }
        let mut file = match std::fs::File::open(&full) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(GitError::Read {
                    path: PathBuf::from(path),
                    source: err,
                });
            }
        };
        let mut prefix = Vec::new();
        let mut chunk = [0u8; 8192];
        while prefix.len() < limit {
            let want = (limit - prefix.len()).min(chunk.len());
            match file.read(&mut chunk[..want]) {
                Ok(0) => break,
                Ok(n) => prefix.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    return Err(GitError::Read {
                        path: PathBuf::from(path),
                        source: err,
                    });
                }
            }
        }
        Ok(Some(prefix))
    }

    /// Read from the final working-tree state.
    fn read_working(&self, path: &str) -> Result<Option<SnapshotFile>, GitError> {
        let full = self.worktree_join(path)?;
        let metadata = match std::fs::symlink_metadata(&full) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(GitError::Read {
                    path: PathBuf::from(path),
                    source: err,
                });
            }
        };
        // Git stores a symlink's content as the link target string, so a
        // working-tree read of a symlink must not follow the link.
        let bytes = if metadata.file_type().is_symlink() {
            std::fs::read_link(&full)
                .map_err(|err| GitError::Read {
                    path: PathBuf::from(path),
                    source: err,
                })?
                .as_os_str()
                .to_string_lossy()
                .into_owned()
                .into_bytes()
        } else {
            std::fs::read(&full).map_err(|err| GitError::Read {
                path: PathBuf::from(path),
                source: err,
            })?
        };
        Ok(Some(SnapshotFile {
            path: path.to_owned(),
            bytes,
            blob_oid: None,
        }))
    }

    /// Read from the index.
    fn read_index(&self, path: &str) -> Result<Option<SnapshotFile>, GitError> {
        let Some(entry) = self.index.get(path) else {
            return Ok(None);
        };
        let bytes = self.ctx.read_blob(&entry.oid)?;
        Ok(Some(SnapshotFile {
            path: path.to_owned(),
            bytes,
            blob_oid: Some(entry.oid.clone()),
        }))
    }

    /// Read from a commit tree.
    fn read_tree(&self, path: &str) -> Result<Option<SnapshotFile>, GitError> {
        let Some(entry) = self.tree.get(path) else {
            return Ok(None);
        };
        // Gitlinks (submodules) have no readable blob in this repository.
        if entry.kind != ObjectKind::Blob {
            return Ok(None);
        }
        let bytes = self.ctx.read_blob(&entry.oid)?;
        Ok(Some(SnapshotFile {
            path: path.to_owned(),
            bytes,
            blob_oid: Some(entry.oid.clone()),
        }))
    }

    /// Whether a tracked path exists in the final working-tree state.
    fn working_file_exists(&self, path: &str) -> Result<bool, GitError> {
        let full = self.worktree_join(path)?;
        match std::fs::symlink_metadata(&full) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(GitError::Read {
                path: PathBuf::from(path),
                source: err,
            }),
        }
    }

    /// Join a validated repository-relative path onto the worktree root.
    fn worktree_join(&self, path: &str) -> Result<std::path::PathBuf, GitError> {
        validate_relative_path(path)?;
        Ok(self.ctx.worktree_root.join(path))
    }
}

/// Validate a repository-relative snapshot path.
///
/// Rejects absolute paths, empty paths, redundant separators or `.`/`..`
/// components, and any reference to Git internals (`.git`). Git-tracked
/// paths never contain any of these, so rejection cannot hide valid
/// snapshot content.
fn validate_relative_path(path: &str) -> Result<(), GitError> {
    let invalid = || GitError::InvalidPath {
        path: path.to_owned(),
    };
    if path.is_empty()
        || Path::new(path).is_absolute()
        || path.contains('\0')
        || path.contains("//")
    {
        return Err(invalid());
    }
    for component in Path::new(path).components() {
        match component {
            std::path::Component::Normal(part) => {
                if part == ".git" {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitRunner;
    use crate::git::snapshot::{SnapshotSpec, resolve_snapshot};
    use crate::git::testutil::{Fixture, commit_all, git_in, stdout_trimmed, write_file};

    fn ctx_of(fixture: &Fixture) -> RepoContext {
        RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap()
    }

    #[test]
    fn read_prefix_skips_oversized_object_blobs() {
        let fixture = Fixture::repo("reader-prefix-cap");
        write_file(&fixture.root, "small", "#!/bin/sh\n:\n");
        let big: Vec<u8> = vec![b'x'; OBJECT_PREFIX_READ_CAP as usize + 1];
        std::fs::write(fixture.root.join("huge"), big).unwrap();
        commit_all(&fixture.root, "one");

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: "HEAD".to_string(),
            },
        )
        .unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        // A blob above the cap reads as absent for prefix purposes —
        // never an error, never a full materialization.
        assert_eq!(reader.read_prefix("huge", 64).unwrap(), None);
        // Small blobs keep working.
        assert!(
            reader
                .read_prefix("small", 64)
                .unwrap()
                .unwrap()
                .starts_with(b"#!")
        );
    }

    #[test]
    fn read_prefix_of_commit_binary_blob_is_bounded_and_verbatim() {
        let fixture = Fixture::repo("reader-binary-prefix");
        // An extensionless binary blob (commit-graph header shape) in the
        // target tree: full reads and bounded prefix reads must both
        // return the bytes verbatim, never a UTF-8 error.
        let bytes: &[u8] = b"CGPH\x01\x01\x04\x00OIDF\x00\x00\x00\xff\xfe";
        std::fs::write(fixture.root.join("commit-graph"), bytes).unwrap();
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: "HEAD".to_string(),
            },
        )
        .unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        assert_eq!(
            reader.read_prefix("commit-graph", 8).unwrap().unwrap(),
            &bytes[..8]
        );
        assert_eq!(reader.read("commit-graph").unwrap().unwrap().bytes, bytes);
    }

    #[test]
    fn read_working_final_state_reflects_staged_unstaged_and_untracked() {
        let fixture = Fixture::repo("reader-working");
        write_file(&fixture.root, "kept.txt", "original\n");
        write_file(&fixture.root, "gone.txt", "will vanish\n");
        commit_all(&fixture.root, "one");

        write_file(&fixture.root, "kept.txt", "modified\n");
        write_file(&fixture.root, "gone.txt", "staged content\n");
        git_in(&fixture.root, &["add", "gone.txt"]);
        std::fs::remove_file(fixture.root.join("gone.txt")).unwrap();
        write_file(&fixture.root, "fresh.txt", "untracked\n");

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();

        assert_eq!(
            reader.read("kept.txt").unwrap().unwrap().bytes,
            b"modified\n"
        );
        // Staged but deleted from the worktree: absent from the final state.
        assert_eq!(reader.read("gone.txt").unwrap(), None);
        assert_eq!(
            reader.read("fresh.txt").unwrap().unwrap().bytes,
            b"untracked\n"
        );
        // Working-tree content has no Git blob OID.
        assert_eq!(reader.read("kept.txt").unwrap().unwrap().blob_oid, None);

        let files = reader.list_files().unwrap();
        assert_eq!(files, vec!["fresh.txt".to_string(), "kept.txt".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn read_working_returns_symlink_target_not_target_content() {
        let fixture = Fixture::repo("reader-symlink");
        write_file(&fixture.root, "real.txt", "secret contents\n");
        let link = fixture.root.join("link.txt");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();

        let file = reader.read("link.txt").unwrap().unwrap();
        assert_eq!(file.bytes, b"real.txt");
    }

    #[test]
    fn read_index_returns_staged_content() {
        let fixture = Fixture::repo("reader-index");
        write_file(&fixture.root, "a.txt", "committed\n");
        commit_all(&fixture.root, "one");
        // Staged content differs from the working-tree content.
        write_file(&fixture.root, "a.txt", "staged\n");
        git_in(&fixture.root, &["add", "a.txt"]);
        write_file(&fixture.root, "a.txt", "working tree\n");

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Staged).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();

        let file = reader.read("a.txt").unwrap().unwrap();
        assert_eq!(file.bytes, b"staged\n");
        let oid = file.blob_oid.expect("index content has a blob OID");
        // The OID matches Git's view of the staged blob.
        let listed = stdout_trimmed(git_in(&fixture.root, &["ls-files", "--stage", "a.txt"]));
        assert!(listed.contains(&oid));
        // A path that is only in the working tree is not in the index.
        write_file(&fixture.root, "untracked.txt", "x\n");
        assert_eq!(reader.read("untracked.txt").unwrap(), None);
    }

    #[test]
    fn read_tree_returns_historical_content_not_worktree_content() {
        let fixture = Fixture::repo("reader-tree");
        write_file(&fixture.root, "a.txt", "historical\n");
        let first = commit_all(&fixture.root, "one");
        write_file(&fixture.root, "a.txt", "current\n");
        commit_all(&fixture.root, "two");

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: first.clone(),
            },
        )
        .unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();

        // Reading the historical snapshot must not leak current content.
        let file = reader.read("a.txt").unwrap().unwrap();
        assert_eq!(file.bytes, b"historical\n");
        assert!(file.blob_oid.is_some());
        assert_eq!(reader.read("nope.txt").unwrap(), None);

        let files = reader.list_files().unwrap();
        assert_eq!(files, vec!["a.txt".to_string()]);
    }

    #[test]
    fn reader_list_files_covers_commit_tree_completely() {
        let fixture = Fixture::repo("reader-tree-list");
        write_file(&fixture.root, "deep/nested/file.txt", "d\n");
        write_file(&fixture.root, "top.txt", "t\n");
        let commit = commit_all(&fixture.root, "one");

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Commit { revision: commit }).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        assert_eq!(
            reader.list_files().unwrap(),
            vec!["deep/nested/file.txt".to_string(), "top.txt".to_string()]
        );
        // Directories themselves are not files.
        assert_eq!(reader.read("deep").unwrap(), None);
        assert_eq!(reader.read("deep/nested").unwrap(), None);
    }

    #[test]
    fn list_files_with_oids_reports_git_oids_for_object_sides_only() {
        let fixture = Fixture::repo("reader-list-oids");
        write_file(&fixture.root, "a.txt", "committed\n");
        write_file(&fixture.root, "deep/b.txt", "b\n");
        let first = commit_all(&fixture.root, "one");
        // Stage different content for the index target.
        write_file(&fixture.root, "a.txt", "staged\n");
        git_in(&fixture.root, &["add", "a.txt"]);
        // Working-only content stays outside both object sides.
        write_file(&fixture.root, "fresh.txt", "untracked\n");

        let ctx = ctx_of(&fixture);

        // Working target: paths present, no object IDs.
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        let listing = reader.list_files_with_oids().unwrap();
        let paths: Vec<&str> = listing.iter().map(|l| l.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "deep/b.txt", "fresh.txt"]);
        assert!(listing.iter().all(|l| l.blob_oid.is_none()));

        // Index target: staged object IDs.
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Staged).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        let listing = reader.list_files_with_oids().unwrap();
        let a = listing.iter().find(|l| l.path == "a.txt").unwrap();
        assert!(a.blob_oid.is_some());
        let listed = stdout_trimmed(git_in(&fixture.root, &["ls-files", "--stage", "a.txt"]));
        assert!(listed.contains(a.blob_oid.as_deref().unwrap()));
        assert!(!listing.iter().any(|l| l.path == "fresh.txt"));

        // Commit target: historical object IDs, excluding gitlinks.
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: first.clone(),
            },
        )
        .unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        let listing = reader.list_files_with_oids().unwrap();
        assert_eq!(listing.len(), 2);
        for entry in &listing {
            assert!(entry.blob_oid.is_some());
        }
    }

    #[test]
    fn read_prefix_is_bounded_and_target_exact() {
        let fixture = Fixture::repo("reader-prefix");
        let large: String = "#!/usr/bin/env bash\n".to_string() + &"x".repeat(4096);
        write_file(&fixture.root, "big", &large);
        write_file(&fixture.root, "small", "tiny\n");
        commit_all(&fixture.root, "one");

        let ctx = ctx_of(&fixture);

        // Working tree: bounded read of a large file.
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        assert_eq!(reader.read_prefix("big", 64).unwrap().unwrap().len(), 64);
        assert_eq!(
            reader
                .read_prefix("big", usize::MAX)
                .unwrap()
                .unwrap()
                .len(),
            large.len()
        );
        // Short files are returned whole.
        assert_eq!(
            reader.read_prefix("small", 512).unwrap().unwrap(),
            b"tiny\n"
        );
        // Absent paths stay absent.
        assert_eq!(reader.read_prefix("missing", 8).unwrap(), None);

        // Index and commit sides return the same bytes, truncated.
        for spec in [
            SnapshotSpec::Staged,
            SnapshotSpec::Commit {
                revision: ctx.head.clone().unwrap(),
            },
        ] {
            let resolved = resolve_snapshot(&ctx, &spec).unwrap();
            let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
            let prefix = reader.read_prefix("big", 20).unwrap().unwrap();
            assert_eq!(prefix, b"#!/usr/bin/env bash\n".as_slice());
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_prefix_of_symlink_returns_link_target() {
        let fixture = Fixture::repo("reader-prefix-symlink");
        write_file(&fixture.root, "real.sh", "x\n");
        let link = fixture.root.join("linked");
        std::os::unix::fs::symlink("real.sh", &link).unwrap();

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        assert_eq!(
            reader.read_prefix("linked", 512).unwrap().unwrap(),
            b"real.sh"
        );
    }

    #[test]
    fn reader_rejects_escaping_and_internal_paths() {
        let fixture = Fixture::repo("reader-paths");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let ctx = ctx_of(&fixture);
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();

        for bad in [
            "",
            "/etc/passwd",
            "../outside.txt",
            "a/../../outside.txt",
            ".git/config",
            "sub/.git/config",
            "./a.txt",
            "a//b.txt",
        ] {
            let err = reader.read(bad).unwrap_err();
            assert!(
                matches!(err, GitError::InvalidPath { .. }),
                "expected InvalidPath for `{bad}`, got {err:?}"
            );
        }

        // The same path set is rejected regardless of target side.
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: ctx.head.clone().unwrap(),
            },
        )
        .unwrap();
        let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
        assert!(matches!(
            reader.read("../outside.txt"),
            Err(GitError::InvalidPath { .. })
        ));
    }

    #[test]
    fn reader_valid_paths_that_do_not_exist_are_absent_not_errors() {
        let fixture = Fixture::repo("reader-absent");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let ctx = ctx_of(&fixture);
        for spec in [
            SnapshotSpec::Working,
            SnapshotSpec::Staged,
            SnapshotSpec::Commit {
                revision: ctx.head.clone().unwrap(),
            },
        ] {
            let resolved = resolve_snapshot(&ctx, &spec).unwrap();
            let reader = SnapshotReader::new(&ctx, &resolved).unwrap();
            assert_eq!(
                reader.read("missing.txt").unwrap(),
                None,
                "expected absence for {spec:?}"
            );
        }
    }
}
