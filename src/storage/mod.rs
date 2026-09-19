//! Regenerable local SQLite storage (plan section 29).
//!
//! Two databases hold all generated local state:
//!
//! - **shared**: `$GIT_COMMON_DIR/nisse/cache.sqlite` — content-addressed
//!   artifacts, symbols, references, chunks, embeddings, review history,
//!   and pending learning, reusable across worktrees and branches;
//! - **worktree**: `$GIT_DIR/nisse/worktree.sqlite` — the per-worktree
//!   manifest, path/content keys, record index, and refresh statistics.
//!
//! SQLite state is a disposable cache. Durable project knowledge lives in
//! Git-tracked `.nisse/**` records and survives cache loss (permanent
//! invariant 4). Databases are created on demand — no init command exists —
//! and are rebuilt automatically when corrupt (see [`db::open_or_recover`]).
//!
//! Schema evolution is forward-only via [`schema`] migrations recorded in
//! each database's `user_version`. A database written by a newer Nisse is
//! never opened or destroyed by an older one.

pub mod db;
pub mod paths;
pub mod schema;
pub mod status;

pub use db::{OpenedCache, ProbeData, ProbeOutcome, StorageError, clear, open_or_recover, probe};
pub use paths::StoragePaths;
pub use schema::{DbKind, Migration, supported_version};
pub use status::{CacheDbStatus, CacheStatus};
