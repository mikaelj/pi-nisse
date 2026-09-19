//! Git integration: the installed `git` executable is the semantic authority
//! for repository/worktree discovery and source truth (plan section 24).
//!
//! - All invocations go through [`GitRunner`] with a timeout, captured
//!   output, and typed failures.
//! - [`RepoContext`] discovers the current worktree, its Git directories,
//!   and HEAD state; linked worktrees are first-class and `.git` is never
//!   assumed to be a directory.
//! - Helpers resolve commit-ish values, merge bases, the empty tree, tracked
//!   and untracked files, the index, and commit trees.

mod error;
mod repo;
mod runner;
#[cfg(test)]
mod testutil;

pub use error::GitError;
pub use repo::{IndexEntry, ObjectKind, RepoContext, TreeEntry};
pub use runner::{DEFAULT_TIMEOUT, GitOutput, GitRunner};
