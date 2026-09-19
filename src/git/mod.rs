//! Git integration: the installed `git` executable is the semantic authority
//! for repository/worktree discovery, snapshots, and source truth (plan
//! sections 24–26).
//!
//! - All invocations go through [`GitRunner`] with a timeout, captured
//!   output, and typed failures.
//! - [`RepoContext`] discovers the current worktree, its Git directories,
//!   and HEAD state; linked worktrees are first-class and `.git` is never
//!   assumed to be a directory.
//! - Helpers resolve commit-ish values, merge bases, the empty tree, tracked
//!   and untracked files, the index, and commit trees.
//! - [`resolve_snapshot`] turns a review scope into a [`ResolvedSnapshot`]
//!   with an exact descriptor; [`snapshot_diff`] computes the change set;
//!   [`SnapshotReader`] reads target-side content with snapshot-membership
//!   guarantees.

mod diff;
mod error;
mod reader;
mod repo;
mod runner;
mod snapshot;
#[cfg(test)]
pub(crate) mod testutil;

pub use diff::{DiffEntry, DiffStatus, snapshot_diff};
pub use error::GitError;
pub use reader::{SnapshotFile, SnapshotListing, SnapshotReader};
pub use repo::{IndexEntry, ObjectFormat, ObjectKind, RepoContext, TreeEntry};
pub use runner::{DEFAULT_TIMEOUT, GitOutput, GitRunner};
pub use snapshot::{
    ResolvedSnapshot, SnapshotBase, SnapshotSpec, SnapshotTarget, resolve_snapshot,
};
