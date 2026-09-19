//! Review snapshot semantics.
//!
//! Every review resolves a target snapshot (plan section 25). A snapshot has
//! two sides:
//!
//! - a **base** side, which is always a Git tree (HEAD tree, first-parent
//!   tree, merge-base tree, or the empty tree);
//! - a **target** side, which is the final working tree, the index, or a
//!   commit tree.
//!
//! Git is the semantic authority: all revisions are resolved through Git
//! before any object ID is used, and the resulting
//! [`SnapshotDescriptor`](crate::domain::snapshot::SnapshotDescriptor)
//! records exactly which commits and trees defined the review scope.
//! Resolution is read-only: no Git objects are created and no index or
//! worktree state is modified.

use crate::domain::snapshot::{SnapshotDescriptor, SnapshotMode};

use super::error::GitError;
use super::repo::RepoContext;

/// A user-selected review scope.
///
/// This is the *input* specification; [`resolve_snapshot`] turns it into a
/// [`ResolvedSnapshot`] with every revision resolved through Git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotSpec {
    /// Compare HEAD against the final working-tree state (staged,
    /// unstaged, and untracked non-ignored content).
    Working,
    /// Compare HEAD against the index.
    Staged,
    /// Compare a commit against its first parent; a root commit compares
    /// against the Git empty tree.
    Commit {
        /// Commit-ish selecting the reviewed commit.
        revision: String,
    },
    /// Compare the BASE tree against the HEAD tree of `<base>..<head>`.
    Range {
        /// Raw `<base>..<head>` specification.
        spec: String,
    },
    /// Compare the merge-base tree of `<base-ref>` and HEAD against the
    /// HEAD tree.
    Branch {
        /// Base ref (branch, tag, or revision).
        base_ref: String,
    },
}

/// The base side of a resolved snapshot: always a tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotBase {
    /// The Git empty tree (used for unborn HEAD and root commits).
    EmptyTree,
    /// A commit and its tree.
    Commit {
        /// Resolved base commit object ID.
        oid: String,
        /// Resolved base tree object ID.
        tree: String,
    },
}

/// The target side of a resolved snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotTarget {
    /// The final working-tree state (staged, unstaged, and untracked
    /// non-ignored content combined). Carries no Git object IDs.
    Working,
    /// The index state. Carries no Git object IDs in the descriptor.
    Index,
    /// A commit and its tree.
    Commit {
        /// Resolved target commit object ID.
        oid: String,
        /// Resolved target tree object ID.
        tree: String,
    },
}

/// A fully resolved snapshot: descriptor plus both concrete sides.
///
/// All source retrieval for a review must go through this value so it can
/// prove membership in the target side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSnapshot {
    /// Descriptor recorded with the review.
    pub descriptor: SnapshotDescriptor,
    /// Base side (always a tree).
    pub base: SnapshotBase,
    /// Target side.
    pub target: SnapshotTarget,
}

/// Resolve a [`SnapshotSpec`] against a repository context.
///
/// Fails with typed errors when revisions do not resolve, when a range
/// specification is malformed, when histories share no merge base, or when
/// the index contains unresolved conflicts (working/staged modes are not
/// well-defined review targets in that state).
pub fn resolve_snapshot(
    ctx: &RepoContext,
    spec: &SnapshotSpec,
) -> Result<ResolvedSnapshot, GitError> {
    match spec {
        SnapshotSpec::Working => resolve_worktree_relative(ctx, SnapshotMode::Working, true),
        SnapshotSpec::Staged => resolve_worktree_relative(ctx, SnapshotMode::Staged, false),
        SnapshotSpec::Commit { revision } => resolve_commit_mode(ctx, revision),
        SnapshotSpec::Range { spec } => resolve_range_mode(ctx, spec),
        SnapshotSpec::Branch { base_ref } => resolve_branch_mode(ctx, base_ref),
    }
}

/// Resolve `working`/`staged` modes: HEAD (or the empty tree) is the base
/// and the worktree/index is the target.
fn resolve_worktree_relative(
    ctx: &RepoContext,
    mode: SnapshotMode,
    working: bool,
) -> Result<ResolvedSnapshot, GitError> {
    reject_unmerged(ctx)?;
    let (base, base_commit) = match &ctx.head {
        Some(head) => {
            let tree = ctx.commit_tree(head)?;
            (
                SnapshotBase::Commit {
                    oid: head.clone(),
                    tree: tree.clone(),
                },
                Some(head.clone()),
            )
        }
        None => (SnapshotBase::EmptyTree, None),
    };
    let base_tree = match &base {
        SnapshotBase::EmptyTree => ctx.empty_tree_oid()?,
        SnapshotBase::Commit { tree, .. } => tree.clone(),
    };
    let target = if working {
        SnapshotTarget::Working
    } else {
        SnapshotTarget::Index
    };
    Ok(ResolvedSnapshot {
        descriptor: SnapshotDescriptor {
            mode,
            base_commit,
            base_tree: Some(base_tree),
            head_commit: None,
            head_tree: None,
        },
        base,
        target,
    })
}

/// Resolve the `commit <oid>` mode: commit versus first parent, or versus
/// the empty tree for a root commit.
fn resolve_commit_mode(ctx: &RepoContext, revision: &str) -> Result<ResolvedSnapshot, GitError> {
    let commit = ctx.resolve_commitish(revision)?;
    let head_tree = ctx.commit_tree(&commit)?;
    let (base, base_commit) = match ctx.first_parent(&commit)? {
        Some(parent) => {
            let parent_tree = ctx.commit_tree(&parent)?;
            (
                SnapshotBase::Commit {
                    oid: parent.clone(),
                    tree: parent_tree,
                },
                Some(parent),
            )
        }
        None => (SnapshotBase::EmptyTree, None),
    };
    let base_tree = match &base {
        SnapshotBase::EmptyTree => ctx.empty_tree_oid()?,
        SnapshotBase::Commit { tree, .. } => tree.clone(),
    };
    Ok(ResolvedSnapshot {
        descriptor: SnapshotDescriptor {
            mode: SnapshotMode::Commit,
            base_commit,
            base_tree: Some(base_tree),
            head_commit: Some(commit.clone()),
            head_tree: Some(head_tree.clone()),
        },
        base,
        target: SnapshotTarget::Commit {
            oid: commit,
            tree: head_tree,
        },
    })
}

/// Resolve the `range <base>..<head>` mode: BASE tree versus HEAD tree.
fn resolve_range_mode(ctx: &RepoContext, spec: &str) -> Result<ResolvedSnapshot, GitError> {
    let (base_spec, head_spec) = split_range_spec(spec)?;
    let base_commit = ctx.resolve_commitish(&base_spec)?;
    let head_commit = ctx.resolve_commitish(&head_spec)?;
    finish_commit_pair(ctx, SnapshotMode::Range, base_commit, head_commit)
}

/// Resolve the `branch <base-ref>` mode: merge-base tree versus HEAD tree.
fn resolve_branch_mode(ctx: &RepoContext, base_ref: &str) -> Result<ResolvedSnapshot, GitError> {
    let head_commit = ctx.head.clone().ok_or_else(|| GitError::RevisionNotFound {
        spec: "HEAD".to_string(),
    })?;
    let head_commit = ctx.resolve_commitish(&head_commit)?;
    let base_commit = ctx.merge_base(base_ref, &head_commit)?;
    finish_commit_pair(ctx, SnapshotMode::Branch, base_commit, head_commit)
}

/// Complete resolution for modes whose sides are two resolved commits.
fn finish_commit_pair(
    ctx: &RepoContext,
    mode: SnapshotMode,
    base_commit: String,
    head_commit: String,
) -> Result<ResolvedSnapshot, GitError> {
    let base_tree = ctx.commit_tree(&base_commit)?;
    let head_tree = ctx.commit_tree(&head_commit)?;
    Ok(ResolvedSnapshot {
        descriptor: SnapshotDescriptor {
            mode,
            base_commit: Some(base_commit.clone()),
            base_tree: Some(base_tree.clone()),
            head_commit: Some(head_commit.clone()),
            head_tree: Some(head_tree.clone()),
        },
        base: SnapshotBase::Commit {
            oid: base_commit,
            tree: base_tree,
        },
        target: SnapshotTarget::Commit {
            oid: head_commit,
            tree: head_tree,
        },
    })
}

/// Split a `<base>..<head>` specification.
///
/// Exactly one `..` with non-empty sides is accepted; `...` (symmetric
/// difference) has different Git semantics and is rejected, as are ranges
/// like `..`, `a..`, and `a..b..c`.
fn split_range_spec(spec: &str) -> Result<(String, String), GitError> {
    let reject = |reason: &str| {
        Err(GitError::InvalidRangeSpec {
            spec: spec.to_string(),
            reason: reason.to_string(),
        })
    };
    if spec.contains("...") {
        return reject("symmetric `...` ranges are not supported; use `<base>..<head>`");
    }
    let parts: Vec<&str> = spec.split("..").collect();
    if parts.len() != 2 {
        return reject("expected exactly one `..` separator");
    }
    let (base, head) = (parts[0].trim(), parts[1].trim());
    if base.is_empty() || head.is_empty() {
        return reject("both sides of `..` must name a revision");
    }
    Ok((base.to_string(), head.to_string()))
}

/// Fail with a typed error when the index contains conflict stages.
///
/// A conflicted repository has no well-defined final working-tree or index
/// state, so `working`/`staged` review scopes are refused instead of
/// reviewed against a guessed target.
fn reject_unmerged(ctx: &RepoContext) -> Result<(), GitError> {
    let unmerged = ctx.unmerged_paths()?;
    if unmerged.is_empty() {
        Ok(())
    } else {
        Err(GitError::UnmergedPaths { paths: unmerged })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testutil::{Fixture, commit_all, git_in, stdout_trimmed, write_file};

    const SHA1_EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    #[test]
    fn resolve_working_records_head_base_and_no_target_ids() {
        let fixture = Fixture::repo("snap-working");
        write_file(&fixture.root, "a.txt", "a\n");
        let head = commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        assert_eq!(resolved.descriptor.mode, SnapshotMode::Working);
        assert_eq!(
            resolved.descriptor.base_commit.as_deref(),
            Some(head.as_str())
        );
        assert!(resolved.descriptor.base_tree.is_some());
        assert_eq!(resolved.descriptor.head_commit, None);
        assert_eq!(resolved.descriptor.head_tree, None);
        assert_eq!(
            resolved.base,
            SnapshotBase::Commit {
                oid: head,
                tree: resolved.descriptor.base_tree.clone().unwrap(),
            }
        );
        assert_eq!(resolved.target, SnapshotTarget::Working);
    }

    #[test]
    fn resolve_staged_targets_index() {
        let fixture = Fixture::repo("snap-staged");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Staged).unwrap();
        assert_eq!(resolved.descriptor.mode, SnapshotMode::Staged);
        assert_eq!(resolved.target, SnapshotTarget::Index);
        assert_eq!(resolved.descriptor.head_commit, None);
    }

    #[test]
    fn resolve_working_and_staged_on_unborn_head_use_empty_tree() {
        let fixture = Fixture::repo("snap-unborn");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        for spec in [SnapshotSpec::Working, SnapshotSpec::Staged] {
            let resolved = resolve_snapshot(&ctx, &spec).unwrap();
            assert_eq!(resolved.base, SnapshotBase::EmptyTree);
            assert_eq!(resolved.descriptor.base_commit, None);
            assert_eq!(
                resolved.descriptor.base_tree.as_deref(),
                Some(SHA1_EMPTY_TREE)
            );
            assert_eq!(resolved.descriptor.head_commit, None);
            assert_eq!(resolved.descriptor.head_tree, None);
        }
    }

    #[test]
    fn resolve_commit_compares_against_first_parent() {
        let fixture = Fixture::repo("snap-commit");
        write_file(&fixture.root, "a.txt", "a\n");
        let parent = commit_all(&fixture.root, "one");
        write_file(&fixture.root, "a.txt", "b\n");
        let child = commit_all(&fixture.root, "two");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: child.clone(),
            },
        )
        .unwrap();
        assert_eq!(resolved.descriptor.mode, SnapshotMode::Commit);
        assert_eq!(
            resolved.descriptor.base_commit.as_deref(),
            Some(parent.as_str())
        );
        assert_eq!(
            resolved.descriptor.head_commit.as_deref(),
            Some(child.as_str())
        );
        assert_eq!(resolved.descriptor.head_tree, ctx.commit_tree(&child).ok());
        assert_eq!(resolved.descriptor.base_tree, ctx.commit_tree(&parent).ok());
        assert_eq!(
            resolved.target,
            SnapshotTarget::Commit {
                oid: child,
                tree: resolved.descriptor.head_tree.clone().unwrap(),
            }
        );
    }

    #[test]
    fn resolve_commit_root_compares_against_empty_tree() {
        let fixture = Fixture::repo("snap-commit-root");
        write_file(&fixture.root, "a.txt", "a\n");
        let root = commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: root.clone(),
            },
        )
        .unwrap();
        assert_eq!(resolved.base, SnapshotBase::EmptyTree);
        assert_eq!(resolved.descriptor.base_commit, None);
        assert_eq!(
            resolved.descriptor.base_tree.as_deref(),
            Some(SHA1_EMPTY_TREE)
        );
        assert_eq!(
            resolved.descriptor.head_commit.as_deref(),
            Some(root.as_str())
        );
    }

    #[test]
    fn resolve_commit_rejects_unknown_revision() {
        let fixture = Fixture::repo("snap-commit-bad");
        write_file(&fixture.root, "a.txt", "a\n");
        commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let err = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: "no-such-rev".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, GitError::RevisionNotFound { .. }));
    }

    #[test]
    fn resolve_range_compares_base_tree_against_head_tree() {
        let fixture = Fixture::repo("snap-range");
        write_file(&fixture.root, "a.txt", "a\n");
        let base = commit_all(&fixture.root, "one");
        write_file(&fixture.root, "a.txt", "b\n");
        write_file(&fixture.root, "c.txt", "c\n");
        let head = commit_all(&fixture.root, "two");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let spec = format!("{base}..{head}");
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Range { spec: spec.clone() }).unwrap();
        assert_eq!(resolved.descriptor.mode, SnapshotMode::Range);
        assert_eq!(
            resolved.descriptor.base_commit.as_deref(),
            Some(base.as_str())
        );
        assert_eq!(
            resolved.descriptor.head_commit.as_deref(),
            Some(head.as_str())
        );
        assert_eq!(resolved.descriptor.base_tree, ctx.commit_tree(&base).ok());
        assert_eq!(resolved.descriptor.head_tree, ctx.commit_tree(&head).ok());
    }

    #[test]
    fn resolve_range_accepts_short_and_symbolic_sides() {
        let fixture = Fixture::repo("snap-range-short");
        write_file(&fixture.root, "a.txt", "a\n");
        let base = commit_all(&fixture.root, "one");
        write_file(&fixture.root, "b.txt", "b\n");
        commit_all(&fixture.root, "two");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Range {
                spec: format!("{}..HEAD", &base[..7]),
            },
        )
        .unwrap();
        assert_eq!(
            resolved.descriptor.base_commit.as_deref(),
            Some(base.as_str())
        );
    }

    #[test]
    fn resolve_range_rejects_malformed_specifications() {
        let fixture = Fixture::repo("snap-range-bad");
        write_file(&fixture.root, "a.txt", "a\n");
        let base = commit_all(&fixture.root, "one");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        for spec in [
            "...".to_string(),
            format!("{base}...HEAD"),
            "..".to_string(),
            format!("{base}.."),
            "..HEAD".to_string(),
            format!("{base}..HEAD..x"),
            String::new(),
        ] {
            let err =
                resolve_snapshot(&ctx, &SnapshotSpec::Range { spec: spec.clone() }).unwrap_err();
            assert!(
                matches!(err, GitError::InvalidRangeSpec { .. }),
                "expected InvalidRangeSpec for `{spec}`, got {err:?}"
            );
        }

        // Syntactically valid but unresolvable side.
        let err = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Range {
                spec: "no-such-rev..HEAD".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, GitError::RevisionNotFound { .. }));
    }

    #[test]
    fn resolve_branch_compares_merge_base_tree_against_head_tree() {
        let fixture = Fixture::repo("snap-branch");
        write_file(&fixture.root, "base.txt", "base\n");
        let base = commit_all(&fixture.root, "base");
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "topic"]);
        write_file(&fixture.root, "topic.txt", "topic\n");
        let topic = commit_all(&fixture.root, "topic");
        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        write_file(&fixture.root, "main.txt", "main\n");
        commit_all(&fixture.root, "main2");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Branch {
                base_ref: "topic".to_string(),
            },
        )
        .unwrap();
        assert_eq!(resolved.descriptor.mode, SnapshotMode::Branch);
        // The base side is the merge base, not the branch tip, and the
        // head side is the current HEAD.
        assert_eq!(
            resolved.descriptor.base_commit.as_deref(),
            Some(base.as_str())
        );
        assert_eq!(resolved.descriptor.head_commit, ctx.head);
        // The merge base is an ancestor of the topic tip, not the tip.
        assert_ne!(
            resolved.descriptor.base_commit.as_deref(),
            Some(topic.as_str())
        );
        assert_ne!(
            resolved.descriptor.base_commit,
            resolved.descriptor.head_commit
        );
    }

    #[test]
    fn resolve_branch_without_merge_base_fails_typed() {
        let fixture = Fixture::repo("snap-branch-unrelated");
        write_file(&fixture.root, "a.txt", "a\n");
        let head = commit_all(&fixture.root, "one");
        let orphan = stdout_trimmed(git_in(
            &fixture.root,
            &["commit-tree", &format!("{head}^{{tree}}"), "-m", "orphan"],
        ));

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let err = resolve_snapshot(&ctx, &SnapshotSpec::Branch { base_ref: orphan }).unwrap_err();
        assert!(matches!(err, GitError::NoMergeBase { .. }));
    }

    #[test]
    fn resolve_branch_on_unborn_head_fails_typed() {
        let fixture = Fixture::repo("snap-branch-unborn");

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        let err = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Branch {
                base_ref: "main".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, GitError::RevisionNotFound { spec } if spec == "HEAD"));
    }

    #[test]
    fn resolve_working_on_detached_head_uses_the_commit() {
        let fixture = Fixture::repo("snap-detached");
        write_file(&fixture.root, "a.txt", "a\n");
        let head = commit_all(&fixture.root, "one");
        git_in(&fixture.root, &["checkout", "--quiet", "--detach", "HEAD"]);

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        assert!(ctx.detached);
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).unwrap();
        assert_eq!(
            resolved.descriptor.base_commit.as_deref(),
            Some(head.as_str())
        );
        assert_eq!(resolved.target, SnapshotTarget::Working);
    }

    #[test]
    fn resolve_worktree_modes_reject_unmerged_index() {
        let fixture = Fixture::repo("snap-conflict");
        write_file(&fixture.root, "a.txt", "base\n");
        let base = commit_all(&fixture.root, "base");
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "side"]);
        write_file(&fixture.root, "a.txt", "side\n");
        commit_all(&fixture.root, "side");
        git_in(
            &fixture.root,
            &["checkout", "--quiet", &fixture.branch_name],
        );
        write_file(&fixture.root, "a.txt", "main\n");
        commit_all(&fixture.root, "main");
        // Merge with deliberate conflict; leave it unresolved.
        let _ = git_in_allow_failure(&fixture.root, &["merge", "--no-ff", "side"]);
        assert!(
            !stdout_trimmed(git_in(&fixture.root, &["ls-files", "-u"])).is_empty(),
            "expected an unresolved conflict"
        );

        let ctx = RepoContext::discover(&crate::git::GitRunner::new(), &fixture.root).unwrap();
        for spec in [SnapshotSpec::Working, SnapshotSpec::Staged] {
            let err = resolve_snapshot(&ctx, &spec).unwrap_err();
            assert!(
                matches!(err, GitError::UnmergedPaths { ref paths } if paths.contains(&"a.txt".to_string())),
                "expected UnmergedPaths, got {err:?}"
            );
        }
        // Historical modes remain valid even with a conflicted index.
        let resolved = resolve_snapshot(
            &ctx,
            &SnapshotSpec::Commit {
                revision: base.clone(),
            },
        );
        assert!(resolved.is_ok());
    }

    /// Run git ignoring its exit status (for deliberately failing commands).
    fn git_in_allow_failure(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("LC_ALL", "C")
            .output()
            .expect("failed to spawn git")
    }
}
