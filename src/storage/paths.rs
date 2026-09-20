//! Storage locations derived from Git directory semantics.
//!
//! Generated state follows plan section 24:
//!
//! - shared, repository-wide state lives under `$GIT_COMMON_DIR/nisse/`;
//! - worktree-specific state lives under `$GIT_DIR/nisse/`.
//!
//! In linked worktrees these directories differ, which is exactly what
//! makes the shared cache reusable across worktrees while worktree caches
//! stay private. Paths are derived from discovered Git directories only —
//! `.git` is never assumed to be a directory.

use std::path::PathBuf;

use crate::git::RepoContext;

/// Directory name for generated Nisse state inside Git directories.
pub const NISSE_STATE_DIR: &str = "nisse";

/// Locations of the regenerable cache databases for one worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePaths {
    /// `$GIT_COMMON_DIR/nisse/cache.sqlite` — shared across worktrees.
    pub shared: PathBuf,
    /// `$GIT_DIR/nisse/worktree.sqlite` — private to this worktree.
    pub worktree: PathBuf,
}

impl StoragePaths {
    /// Derive cache locations from a discovered repository/worktree context.
    pub fn for_repo(ctx: &RepoContext) -> Self {
        StoragePaths {
            shared: ctx
                .git_common_dir
                .join(NISSE_STATE_DIR)
                .join(super::schema::DbKind::Shared.file_name()),
            worktree: ctx
                .git_dir
                .join(NISSE_STATE_DIR)
                .join(super::schema::DbKind::Worktree.file_name()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::GitRunner;
    use crate::git::testutil::{Fixture, commit_all, git_in, same_path, write_file};

    #[test]
    fn main_worktree_paths_live_under_git_nisse() {
        let fixture = Fixture::repo("storage-main");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let paths = StoragePaths::for_repo(&ctx);
        // Git-printed path forms differ from filesystem forms on Windows;
        // location equality goes through `same_path`.
        assert!(same_path(
            &paths.shared,
            &fixture.root.join(".git").join("nisse").join("cache.sqlite")
        ));
        assert!(same_path(
            &paths.worktree,
            &fixture
                .root
                .join(".git")
                .join("nisse")
                .join("worktree.sqlite")
        ));
    }

    #[test]
    fn linked_worktree_shares_common_cache_but_has_private_worktree_cache() {
        let fixture = Fixture::repo("storage-linked");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let wt_path = fixture
            .sibling("wt")
            .to_str()
            .expect("worktree path is valid UTF-8")
            .to_owned();
        git_in(
            &fixture.root,
            &["worktree", "add", "--quiet", &wt_path, "-b", "topic"],
        );

        let main_ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let wt_root = std::path::Path::new(&wt_path).canonicalize().unwrap();
        let wt_ctx = RepoContext::discover(&GitRunner::new(), &wt_root).unwrap();

        let main_paths = StoragePaths::for_repo(&main_ctx);
        let wt_paths = StoragePaths::for_repo(&wt_ctx);

        // Shared caches are the same physical file for every worktree.
        assert_eq!(main_paths.shared, wt_paths.shared);
        // Worktree caches are private per worktree.
        assert_ne!(main_paths.worktree, wt_paths.worktree);
        assert!(same_path(
            &wt_paths.worktree,
            &fixture
                .root
                .join(".git")
                .join("worktrees")
                .join("wt")
                .join("nisse")
                .join("worktree.sqlite")
        ));
    }

    #[test]
    fn shared_cache_is_physically_shared_and_worktree_caches_stay_private() {
        use crate::storage::db;
        use crate::storage::schema::DbKind;

        let fixture = Fixture::repo("storage-independence");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");
        let wt_path = fixture
            .sibling("wt")
            .to_str()
            .expect("worktree path is valid UTF-8")
            .to_owned();
        git_in(
            &fixture.root,
            &["worktree", "add", "--quiet", &wt_path, "-b", "topic"],
        );

        let main_ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let wt_root = std::path::Path::new(&wt_path).canonicalize().unwrap();
        let wt_ctx = RepoContext::discover(&GitRunner::new(), &wt_root).unwrap();
        let main_paths = StoragePaths::for_repo(&main_ctx);
        let wt_paths = StoragePaths::for_repo(&wt_ctx);

        // A row written to the shared cache through one worktree is visible
        // through the other: it is one physical database.
        {
            let opened = db::open_or_recover(&main_paths.shared, DbKind::Shared).unwrap();
            opened
                .conn
                .execute(
                    "INSERT INTO content_artifact (content_key, byte_len, created_at)\n                     VALUES ('git:abc', 3, '2026-01-01T00:00:00Z')",
                    [],
                )
                .unwrap();
        }
        let shared_via_wt = db::open_or_recover(&wt_paths.shared, DbKind::Shared).unwrap();
        let visible = shared_via_wt
            .conn
            .query_row("SELECT count(*) FROM content_artifact", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(visible, 1, "shared cache must be shared across worktrees");

        // Worktree caches are separate physical databases.
        let main_wt = db::open_or_recover(&main_paths.worktree, DbKind::Worktree).unwrap();
        main_wt
            .conn
            .execute(
                "INSERT INTO manifest (path, content_key, updated_at)\n                 VALUES ('a.txt', 'git:abc', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        drop(main_wt);
        let wt_wt = db::open_or_recover(&wt_paths.worktree, DbKind::Worktree).unwrap();
        let rows = wt_wt
            .conn
            .query_row("SELECT count(*) FROM manifest", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "worktree caches must be private per worktree");
    }
}
