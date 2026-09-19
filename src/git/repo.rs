//! Repository/worktree discovery and Git helpers.
//!
//! Git is the semantic authority for source truth (plan section 24). All
//! discovery goes through the installed `git` executable — nothing is guessed
//! from the filesystem, `.git` is never assumed to be a directory, and linked
//! worktrees are first-class.

use std::path::{Path, PathBuf};

use super::error::GitError;
use super::runner::{GitOutput, GitRunner};

/// Kind of object listed by `git ls-tree`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// A file blob.
    Blob,
    /// A tree (directory) object.
    Tree,
    /// A gitlink (submodule commit).
    Commit,
}

impl ObjectKind {
    /// Parse the `ls-tree` type column.
    fn parse(text: &str) -> Result<Self, GitError> {
        match text {
            "blob" => Ok(ObjectKind::Blob),
            "tree" => Ok(ObjectKind::Tree),
            "commit" => Ok(ObjectKind::Commit),
            other => Err(GitError::UnexpectedOutput {
                command: "ls-tree".to_string(),
                message: format!("unknown object kind `{other}`"),
            }),
        }
    }
}

/// One entry of the Git index (`git ls-files --stage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// File mode string, for example `100644`.
    pub mode: String,
    /// Blob object ID.
    pub oid: String,
    /// Stage: `0` is the normal entry, `1`–`3` are conflict stages.
    pub stage: u8,
    /// Path relative to the worktree root.
    pub path: String,
}

/// One entry of a recursive tree listing (`git ls-tree -r`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// File mode string, for example `100644` or `160000` for gitlinks.
    pub mode: String,
    /// Kind of the listed object.
    pub kind: ObjectKind,
    /// Object ID.
    pub oid: String,
    /// Full path relative to the tree root.
    pub path: String,
}

/// Discovered repository/worktree context.
///
/// Discovery starts from any directory inside a worktree (including linked
/// worktrees) and resolves the exact paths and HEAD state for that worktree.
#[derive(Debug, Clone)]
pub struct RepoContext {
    runner: GitRunner,
    /// Absolute path to the root of the current worktree.
    pub worktree_root: PathBuf,
    /// Absolute `$GIT_DIR` of this worktree (differs from
    /// `git_common_dir` in linked worktrees).
    pub git_dir: PathBuf,
    /// Absolute `$GIT_COMMON_DIR` shared by all worktrees of the repository.
    pub git_common_dir: PathBuf,
    /// Resolved HEAD commit object ID; `None` when HEAD is unborn.
    pub head: Option<String>,
    /// Current branch name when HEAD is a symbolic ref (born or unborn);
    /// `None` when detached.
    pub branch: Option<String>,
    /// Whether HEAD is detached at a commit.
    pub detached: bool,
    /// Whether HEAD points at a branch that has no commits yet.
    pub unborn: bool,
}

impl RepoContext {
    /// Discover the repository/worktree containing `cwd`.
    ///
    /// Fails with [`GitError::NotARepository`] when `cwd` is not inside a
    /// Git repository, and with [`GitError::MissingCwd`] when it does not
    /// exist.
    pub fn discover(runner: &GitRunner, cwd: &Path) -> Result<Self, GitError> {
        if !cwd.is_dir() {
            return Err(GitError::MissingCwd {
                path: cwd.to_owned(),
            });
        }

        let worktree_root = match runner.run(cwd, &["rev-parse", "--show-toplevel"]) {
            Ok(output) => PathBuf::from(output.stdout_trimmed()),
            Err(GitError::Execution { stderr, .. }) if is_not_a_repository(&stderr) => {
                return Err(GitError::NotARepository {
                    cwd: cwd.to_owned(),
                });
            }
            Err(err) => return Err(err),
        };

        let git_dir = PathBuf::from(
            runner
                .run(cwd, &["rev-parse", "--absolute-git-dir"])?
                .stdout_trimmed()
                .to_owned(),
        );
        let git_common_dir = PathBuf::from(
            runner
                .run(
                    cwd,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )?
                .stdout_trimmed()
                .to_owned(),
        );

        // `--quiet` makes an unresolvable HEAD a status-only outcome instead
        // of a diagnostic, which is how unborn HEAD is detected.
        let head = match runner.run_unchecked(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"]) {
            Ok(output) if output.success && !output.stdout_trimmed().is_empty() => {
                Some(output.stdout_trimmed().to_owned())
            }
            Ok(_) => None,
            Err(err) => return Err(err),
        };
        // `symbolic-ref -q` succeeds for branch refs (born or unborn) and
        // fails for a detached HEAD.
        let branch =
            match runner.run_unchecked(cwd, &["symbolic-ref", "--short", "--quiet", "HEAD"]) {
                Ok(output) if output.success && !output.stdout_trimmed().is_empty() => {
                    Some(output.stdout_trimmed().to_owned())
                }
                Ok(_) => None,
                Err(err) => return Err(err),
            };

        let detached = head.is_some() && branch.is_none();
        let unborn = head.is_none();

        Ok(RepoContext {
            runner: runner.clone(),
            worktree_root,
            git_dir,
            git_common_dir,
            head,
            branch,
            detached,
            unborn,
        })
    }

    /// The runner used for all Git invocations of this context.
    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }

    /// Resolve a commit-ish (revision, branch, tag, OID, suffixes) to a full
    /// commit object ID.
    ///
    /// Only commits resolve; annotated tags are peeled. Fails with
    /// [`GitError::RevisionNotFound`] otherwise.
    pub fn resolve_commitish(&self, spec: &str) -> Result<String, GitError> {
        let peeled = format!("{spec}^{{commit}}");
        // `--end-of-options` prevents a spec that looks like a Git option
        // from being interpreted as one.
        match self.run(&["rev-parse", "--verify", "--end-of-options", &peeled]) {
            Ok(output) => Ok(output.stdout_trimmed().to_owned()),
            Err(GitError::Execution { .. }) => Err(GitError::RevisionNotFound {
                spec: spec.to_string(),
            }),
            Err(err) => Err(err),
        }
    }

    /// Compute the best common ancestor of two commits.
    ///
    /// Fails with [`GitError::NoMergeBase`] when the commits share no
    /// common ancestor.
    pub fn merge_base(&self, a: &str, b: &str) -> Result<String, GitError> {
        match self.run(&["merge-base", "--end-of-options", a, b]) {
            Ok(output) => Ok(output.stdout_trimmed().to_owned()),
            // `git merge-base` exits with 1 exactly when no merge base
            // exists; every other failure is a genuine execution error.
            Err(GitError::Execution { exit: Some(1), .. }) => Err(GitError::NoMergeBase {
                a: a.to_string(),
                b: b.to_string(),
            }),
            Err(err) => Err(err),
        }
    }

    /// Object ID of the empty tree in this repository's object format.
    ///
    /// Produced by Git itself (`git mktree` with empty input) so SHA-1 and
    /// SHA-256 repositories both work.
    pub fn empty_tree_oid(&self) -> Result<String, GitError> {
        let output = self.run(&["mktree"])?;
        Ok(output.stdout_trimmed().to_owned())
    }

    /// Paths tracked in the index, relative to the worktree root and sorted
    /// by Git.
    ///
    /// This reflects the index, so staged-but-not-yet-committed files are
    /// included and files deleted from the working tree without `git rm`
    /// remain listed.
    pub fn tracked_files(&self) -> Result<Vec<String>, GitError> {
        let output = self.run(&["ls-files", "-z"])?;
        Ok(split_nul_entries(&output.stdout)
            .map(str::to_owned)
            .collect())
    }

    /// Untracked, non-ignored paths relative to the worktree root, sorted by
    /// Git.
    ///
    /// Standard ignore sources (`.gitignore`, `.git/info/exclude`,
    /// `core.excludesFile`) apply. Nothing under `.git` is ever listed.
    pub fn untracked_non_ignored(&self) -> Result<Vec<String>, GitError> {
        let output = self.run(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        Ok(split_nul_entries(&output.stdout)
            .map(str::to_owned)
            .collect())
    }

    /// Full index listing including stages, relative to the worktree root.
    pub fn index_entries(&self) -> Result<Vec<IndexEntry>, GitError> {
        let output = self.run(&["ls-files", "--stage", "-z"])?;
        let mut entries = Vec::new();
        for raw in split_nul_entries(&output.stdout) {
            let (meta, path) = raw
                .split_once('\t')
                .ok_or_else(|| GitError::UnexpectedOutput {
                    command: "ls-files --stage".to_string(),
                    message: format!("missing path separator in `{raw}`"),
                })?;
            let mut columns = meta.split(' ');
            let mode = columns.next().unwrap_or_default().to_owned();
            let oid = columns.next().unwrap_or_default().to_owned();
            let stage_text = columns.next().unwrap_or_default();
            let stage = stage_text
                .parse::<u8>()
                .map_err(|_| GitError::UnexpectedOutput {
                    command: "ls-files --stage".to_string(),
                    message: format!("invalid stage `{stage_text}` in `{raw}`"),
                })?;
            if mode.is_empty() || oid.is_empty() {
                return Err(GitError::UnexpectedOutput {
                    command: "ls-files --stage".to_string(),
                    message: format!("missing mode or object id in `{raw}`"),
                });
            }
            entries.push(IndexEntry {
                mode,
                oid,
                stage,
                path: path.to_owned(),
            });
        }
        Ok(entries)
    }

    /// Recursively list the files of a commit or tree.
    ///
    /// Paths are relative to the tree root and sorted by Git. Tree entries
    /// (directories) are not included; gitlinks appear with
    /// [`ObjectKind::Commit`].
    pub fn list_tree(&self, treeish: &str) -> Result<Vec<TreeEntry>, GitError> {
        let output = self.run(&["ls-tree", "-r", "-z", treeish])?;
        let mut entries = Vec::new();
        for raw in split_nul_entries(&output.stdout) {
            let (meta, path) = raw
                .split_once('\t')
                .ok_or_else(|| GitError::UnexpectedOutput {
                    command: "ls-tree".to_string(),
                    message: format!("missing path separator in `{raw}`"),
                })?;
            let mut columns = meta.split(' ');
            let mode = columns.next().unwrap_or_default().to_owned();
            let kind = ObjectKind::parse(columns.next().unwrap_or_default())?;
            let oid = columns.next().unwrap_or_default().to_owned();
            if mode.is_empty() || oid.is_empty() {
                return Err(GitError::UnexpectedOutput {
                    command: "ls-tree".to_string(),
                    message: format!("missing mode or object id in `{raw}`"),
                });
            }
            entries.push(TreeEntry {
                mode,
                kind,
                oid,
                path: path.to_owned(),
            });
        }
        Ok(entries)
    }

    /// Resolve the tree object ID of a commit.
    pub fn commit_tree(&self, commit: &str) -> Result<String, GitError> {
        let peeled = format!("{commit}^{{tree}}");
        let output = self.run(&["rev-parse", "--verify", "--end-of-options", &peeled])?;
        Ok(output.stdout_trimmed().to_owned())
    }

    /// First parent of a commit; `None` for a root commit.
    ///
    /// Uses `rev-list --parents` because it fails for unresolvable
    /// revisions instead of silently falling back to HEAD.
    pub fn first_parent(&self, commit: &str) -> Result<Option<String>, GitError> {
        let output = match self.run(&[
            "rev-list",
            "--parents",
            "-n",
            "1",
            "--end-of-options",
            commit,
        ]) {
            Ok(output) => output,
            Err(GitError::Execution { .. }) => {
                return Err(GitError::RevisionNotFound {
                    spec: commit.to_string(),
                });
            }
            Err(err) => return Err(err),
        };
        let mut tokens = output.stdout_trimmed().split_whitespace();
        let resolved = tokens.next().unwrap_or_default();
        Self::validate_oid(resolved)?;
        match tokens.next() {
            Some(parent) => {
                Self::validate_oid(parent)?;
                Ok(Some(parent.to_owned()))
            }
            None => Ok(None),
        }
    }

    /// Read the full content of a blob object.
    pub fn read_blob(&self, oid: &str) -> Result<Vec<u8>, GitError> {
        Self::validate_oid(oid)?;
        let output = self.run(&["cat-file", "blob", oid])?;
        Ok(output.stdout.into_bytes())
    }

    /// Paths that currently have conflict stages (stage 1–3) in the index.
    ///
    /// Each conflicted path is listed once, sorted.
    pub fn unmerged_paths(&self) -> Result<Vec<String>, GitError> {
        let mut paths: Vec<String> = self
            .index_entries()?
            .into_iter()
            .filter(|entry| entry.stage != 0)
            .map(|entry| entry.path)
            .collect();
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Check that `oid` is a full hex Git object ID (SHA-1 or SHA-256).
    ///
    /// Object IDs are validated before they are passed back to Git so a
    /// crafted value can never be interpreted as a Git option.
    pub fn validate_oid(oid: &str) -> Result<(), GitError> {
        let valid = (oid.len() == 40 || oid.len() == 64)
            && oid
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(())
        } else {
            Err(GitError::InvalidOid {
                oid: oid.to_owned(),
            })
        }
    }

    /// Run a Git command from the worktree root, requiring success.
    fn run(&self, args: &[&str]) -> Result<GitOutput, GitError> {
        self.runner.run(&self.worktree_root, args)
    }
}

/// Split NUL-separated `-z` output into non-empty entries.
fn split_nul_entries(output: &str) -> impl Iterator<Item = &str> {
    output.split('\0').filter(|entry| !entry.is_empty())
}

/// Recognize Git's "not a repository" diagnostic.
///
/// The runner forces `LC_ALL=C`, so the message is stable.
fn is_not_a_repository(stderr: &str) -> bool {
    stderr.contains("not a git repository")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::{Fixture, commit_all, git_in, stdout_trimmed, write_file};

    #[test]
    fn discover_normal_repository_from_root_and_subdirectory() {
        let fixture = Fixture::repo("normal");
        write_file(&fixture.root, "src/main.rs", "fn main() {}\n");
        let oid = commit_all(&fixture.root, "one");

        let runner = GitRunner::new();
        let ctx = RepoContext::discover(&runner, &fixture.root).unwrap();
        assert_eq!(ctx.worktree_root, fixture.root);
        assert_eq!(ctx.git_dir, fixture.root.join(".git"));
        assert_eq!(ctx.git_common_dir, fixture.root.join(".git"));
        assert_eq!(ctx.head.as_deref(), Some(oid.as_str()));
        assert_eq!(ctx.branch.as_deref(), Some(fixture.branch_name.as_str()));
        assert!(!ctx.detached);
        assert!(!ctx.unborn);

        // Discovery from a subdirectory still resolves the worktree root.
        let sub = fixture.root.join("src");
        let ctx_sub = RepoContext::discover(&runner, &sub).unwrap();
        assert_eq!(ctx_sub.worktree_root, fixture.root);
        assert_eq!(ctx_sub.head, ctx.head);
    }

    #[test]
    fn discover_detached_head() {
        let fixture = Fixture::repo("detached");
        write_file(&fixture.root, "a.txt", "a\n");
        let oid = commit_all(&fixture.root, "one");
        git_in(&fixture.root, &["checkout", "--quiet", "--detach", "HEAD"]);

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert_eq!(ctx.head.as_deref(), Some(oid.as_str()));
        assert_eq!(ctx.branch, None);
        assert!(ctx.detached);
        assert!(!ctx.unborn);
    }

    #[test]
    fn discover_unborn_head() {
        let fixture = Fixture::repo("unborn");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert_eq!(ctx.head, None);
        // The branch name is read straight from the HEAD file so the test
        // does not depend on the installed Git's default branch name.
        assert_eq!(ctx.branch.as_deref(), Some(fixture.branch_name.as_str()));
        assert!(!ctx.detached);
        assert!(ctx.unborn);
    }

    #[test]
    fn discover_rejects_directory_outside_any_repository() {
        let dir = tempfile::tempdir().unwrap();
        // Canonicalize: temp dirs can live behind symlinks while parents may
        // still be inside a repository through the resolved path.
        let outside = dir.path().canonicalize().unwrap();
        let err = RepoContext::discover(&GitRunner::new(), &outside).unwrap_err();
        assert!(matches!(err, GitError::NotARepository { .. }));
    }

    #[test]
    fn discover_missing_cwd_is_typed() {
        let err =
            RepoContext::discover(&GitRunner::new(), Path::new("/nisse-missing-cwd")).unwrap_err();
        assert!(matches!(err, GitError::MissingCwd { .. }));
    }

    #[test]
    fn discover_linked_worktree_has_separate_git_dir_and_shared_common_dir() {
        let fixture = Fixture::repo("linked");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let wt_path = fixture.sibling("wt");
        let wt_path = wt_path
            .to_str()
            .expect("worktree path is valid UTF-8")
            .to_owned();
        git_in(
            &fixture.root,
            &["worktree", "add", "--quiet", &wt_path, "-b", "topic"],
        );

        let runner = GitRunner::new();

        let main_ctx = RepoContext::discover(&runner, &fixture.root).unwrap();
        assert_eq!(main_ctx.git_dir, fixture.root.join(".git"));
        assert_eq!(main_ctx.git_common_dir, fixture.root.join(".git"));

        let wt_root = Path::new(&wt_path).canonicalize().unwrap();
        let wt_ctx = RepoContext::discover(&runner, &wt_root).unwrap();
        assert_eq!(wt_ctx.worktree_root, wt_root);
        assert_eq!(
            wt_ctx.git_dir,
            fixture.root.join(".git").join("worktrees").join("wt")
        );
        // The worktree git dir is separate and exists on disk.
        assert!(wt_ctx.git_dir.is_dir());
        assert_ne!(wt_ctx.git_dir, wt_ctx.git_common_dir);
        // Both worktrees share the same common dir.
        assert_eq!(wt_ctx.git_common_dir, main_ctx.git_common_dir);
        assert_eq!(wt_ctx.git_common_dir, fixture.root.join(".git"));
        assert_eq!(wt_ctx.branch.as_deref(), Some("topic"));
    }

    #[test]
    fn resolve_commitish_accepts_revisions_branches_and_peels_tags() {
        let fixture = Fixture::repo("resolve");
        write_file(&fixture.root, "a.txt", "a\n");
        let oid = commit_all(&fixture.root, "one");
        git_in(&fixture.root, &["tag", "--annotate", "-m", "v1", "v1"]);

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert_eq!(ctx.resolve_commitish("HEAD").unwrap(), oid);
        assert_eq!(ctx.resolve_commitish(&oid).unwrap(), oid);
        assert_eq!(ctx.resolve_commitish(&oid[..7]).unwrap(), oid);
        assert_eq!(ctx.resolve_commitish(&fixture.branch_name).unwrap(), oid);
        // Annotated tags peel down to the commit.
        assert_eq!(ctx.resolve_commitish("v1").unwrap(), oid);
    }

    #[test]
    fn resolve_commitish_rejects_missing_and_non_commit_revisions() {
        let fixture = Fixture::repo("resolve-bad");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");
        let tree_oid = stdout_trimmed(git_in(&fixture.root, &["rev-parse", "HEAD^{tree}"]));

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert!(matches!(
            ctx.resolve_commitish("does-not-exist"),
            Err(GitError::RevisionNotFound { spec }) if spec == "does-not-exist"
        ));
        // Trees do not peel to commits.
        assert!(matches!(
            ctx.resolve_commitish(&tree_oid),
            Err(GitError::RevisionNotFound { .. })
        ));
    }

    #[test]
    fn resolve_commitish_rejects_option_like_revisions() {
        let fixture = Fixture::repo("resolve-option");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert!(matches!(
            ctx.resolve_commitish("--help"),
            Err(GitError::RevisionNotFound { spec }) if spec == "--help"
        ));
        assert!(matches!(
            ctx.resolve_commitish("--upload-pack=evil"),
            Err(GitError::RevisionNotFound { .. })
        ));
    }

    #[test]
    fn merge_base_finds_common_ancestor_and_ancestor_shortcut() {
        let fixture = Fixture::repo("mergebase");
        write_file(&fixture.root, "base.txt", "base\n");
        let base = commit_all(&fixture.root, "base");

        git_in(&fixture.root, &["checkout", "--quiet", "-b", "topic"]);
        write_file(&fixture.root, "topic.txt", "topic\n");
        let topic = commit_all(&fixture.root, "topic");

        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        write_file(&fixture.root, "main2.txt", "main\n");
        let main = commit_all(&fixture.root, "main2");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert_eq!(ctx.merge_base(&main, &topic).unwrap(), base);
        // An ancestor pair resolves to the ancestor itself.
        assert_eq!(ctx.merge_base(&base, &main).unwrap(), base);
    }

    #[test]
    fn merge_base_reports_unrelated_histories() {
        let fixture = Fixture::repo("unrelated");
        write_file(&fixture.root, "a.txt", "a\n");
        let head = commit_all(&fixture.root, "one");
        // A parentless commit: a second root with no shared history.
        let orphan = stdout_trimmed(git_in(
            &fixture.root,
            &["commit-tree", &format!("{head}^{{tree}}"), "-m", "orphan"],
        ));

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert!(matches!(
            ctx.merge_base(&head, &orphan),
            Err(GitError::NoMergeBase { .. })
        ));
    }

    #[test]
    fn empty_tree_oid_matches_git_and_lists_empty() {
        let fixture = Fixture::repo("emptytree");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let empty = ctx.empty_tree_oid().unwrap();
        // The empty tree of the default SHA-1 object format. The helper
        // obtains it from Git, so SHA-256 repositories keep working.
        assert_eq!(empty, "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert!(ctx.list_tree(&empty).unwrap().is_empty());
    }

    #[test]
    fn tracked_files_reflect_the_index() {
        let fixture = Fixture::repo("tracked");
        write_file(&fixture.root, "README.md", "readme\n");
        write_file(&fixture.root, "src/main.rs", "fn main() {}\n");
        commit_all(&fixture.root, "one");

        // Staged new files are tracked; untracked files are not.
        write_file(&fixture.root, "new.txt", "new\n");
        git_in(&fixture.root, &["add", "new.txt"]);
        write_file(&fixture.root, "other.txt", "other\n");
        // A file removed from the working tree without `git rm` remains
        // listed because the index still has it.
        std::fs::remove_file(fixture.root.join("README.md")).unwrap();

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert_eq!(
            ctx.tracked_files().unwrap(),
            vec![
                "README.md".to_string(),
                "new.txt".to_string(),
                "src/main.rs".to_string()
            ]
        );
    }

    #[test]
    fn untracked_non_ignored_respects_ignore_rules() {
        let fixture = Fixture::repo("untracked");
        // Commit only the ignore rules; everything else stays untracked.
        write_file(&fixture.root, ".gitignore", "ignored.txt\nbuild/\n");
        commit_all(&fixture.root, "one");
        write_file(&fixture.root, "ignored.txt", "x\n");
        write_file(&fixture.root, "build/x.o", "x\n");
        write_file(&fixture.root, "note.txt", "x\n");
        write_file(&fixture.root, "deep/dir/file.rs", "x\n");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let files = ctx.untracked_non_ignored().unwrap();
        assert_eq!(
            files,
            vec!["deep/dir/file.rs".to_string(), "note.txt".to_string()]
        );
        assert!(!files.iter().any(|path| path.starts_with(".git")));
    }

    #[test]
    fn index_entries_expose_mode_oid_and_stage() {
        let fixture = Fixture::repo("index");
        write_file(&fixture.root, "hello.txt", "hello\n");
        git_in(&fixture.root, &["add", "hello.txt"]);

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let entries = ctx.index_entries().unwrap();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.mode, "100644");
        assert_eq!(entry.stage, 0);
        assert_eq!(entry.path, "hello.txt");
        // Well-known OID of a blob containing exactly "hello\n".
        assert_eq!(entry.oid, "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn list_tree_lists_files_recursively() {
        let fixture = Fixture::repo("tree");
        write_file(&fixture.root, "src/lib.rs", "pub fn f() {}\n");
        write_file(&fixture.root, "src/deep/mod.rs", "mod deep;\n");
        write_file(&fixture.root, "top.txt", "top\n");
        commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let entries = ctx.list_tree("HEAD").unwrap();
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["src/deep/mod.rs", "src/lib.rs", "top.txt"]);
        assert!(entries.iter().all(|e| e.kind == ObjectKind::Blob));

        // Tree OIDs agree with the index for identical content.
        let index = ctx.index_entries().unwrap();
        for entry in &entries {
            let index_entry = index
                .iter()
                .find(|candidate| candidate.path == entry.path)
                .unwrap_or_else(|| panic!("{} missing from index", entry.path));
            assert_eq!(entry.oid, index_entry.oid);
        }
    }

    #[test]
    fn commit_tree_resolves_the_tree_of_a_commit() {
        let fixture = Fixture::repo("commit-tree");
        write_file(&fixture.root, "a.txt", "a\n");
        let commit = commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let tree = ctx.commit_tree(&commit).unwrap();
        assert_eq!(
            tree,
            stdout_trimmed(git_in(&fixture.root, &["rev-parse", "HEAD^{tree}"]))
        );
    }

    #[test]
    fn first_parent_returns_first_parent_and_none_for_root() {
        let fixture = Fixture::repo("first-parent");
        write_file(&fixture.root, "a.txt", "a\n");
        let root = commit_all(&fixture.root, "root");
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "side"]);
        write_file(&fixture.root, "side.txt", "side\n");
        let side = commit_all(&fixture.root, "side");
        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        write_file(&fixture.root, "main.txt", "main\n");
        let main_tip = commit_all(&fixture.root, "main");
        git_in(
            &fixture.root,
            &["merge", "--quiet", "--no-ff", "-m", "merge", "side"],
        );
        let merge = stdout_trimmed(git_in(&fixture.root, &["rev-parse", "HEAD"]));

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        // A merge commit's first parent is the pre-merge HEAD.
        assert_eq!(
            ctx.first_parent(&merge).unwrap().as_deref(),
            Some(main_tip.as_str())
        );
        // Ordinary commits have exactly one parent.
        assert_eq!(
            ctx.first_parent(&side).unwrap().as_deref(),
            Some(root.as_str())
        );
        // Root commits have none.
        assert_eq!(ctx.first_parent(&root).unwrap(), None);
        // Unknown revisions fail typed.
        assert!(matches!(
            ctx.first_parent("no-such-rev"),
            Err(GitError::RevisionNotFound { .. })
        ));
    }

    #[test]
    fn read_blob_returns_exact_content_and_validates_oids() {
        let fixture = Fixture::repo("read-blob");
        write_file(&fixture.root, "a.txt", "exact content\n");
        commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        let oid = stdout_trimmed(git_in(&fixture.root, &["rev-parse", "HEAD:a.txt"]));
        assert_eq!(ctx.read_blob(&oid).unwrap(), b"exact content\n");
        // Non-hex or truncated OIDs are rejected before reaching Git.
        assert!(matches!(
            ctx.read_blob("--exec=evil"),
            Err(GitError::InvalidOid { .. })
        ));
        assert!(matches!(
            ctx.read_blob("abc123"),
            Err(GitError::InvalidOid { .. })
        ));
    }

    #[test]
    fn unmerged_paths_lists_conflicted_entries() {
        let fixture = Fixture::repo("unmerged-paths");
        write_file(&fixture.root, "a.txt", "base\n");
        commit_all(&fixture.root, "base");
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "side"]);
        write_file(&fixture.root, "a.txt", "side\n");
        commit_all(&fixture.root, "side");
        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        write_file(&fixture.root, "a.txt", "main\n");
        commit_all(&fixture.root, "main");
        let _ = std::process::Command::new("git")
            .args(["merge", "--no-ff", "side"])
            .current_dir(&fixture.root)
            .env("LC_ALL", "C")
            .output()
            .expect("failed to spawn git");

        let ctx = RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap();
        assert_eq!(ctx.unmerged_paths().unwrap(), vec!["a.txt".to_string()]);
    }
}
