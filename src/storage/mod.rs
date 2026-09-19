//! Storage of Nisse state.
//!
//! Two fundamentally different kinds of state live here:
//!
//! 1. **Durable project records** ([`records`], plan sections 39–48):
//!    immutable, Git-tracked JSON files under the worktree's `.nisse/`
//!    tree. They are authoritative shared knowledge, preserve
//!    provenance, and survive any cache loss. Observations, facts, and
//!    lessons never depend on SQLite.
//! 2. **Regenerable local caches** ([`db`], [`schema`], [`status`]; plan
//!    section 29): SQLite under `$GIT_COMMON_DIR/nisse/` and
//!    `$GIT_DIR/nisse/`. Everything in them can be rebuilt from Git and
//!    the durable records; their loss never destroys durable knowledge
//!    (permanent invariant 4).
//!
//! [`fsutil`] provides the atomic filesystem primitives used for
//! durable writes.
//!
//! Schema evolution of the SQLite caches is forward-only via [`schema`]
//! migrations recorded in each database's `user_version`. A database
//! written by a newer Nisse is never opened or destroyed by an older
//! one. See [storage.md](../../docs/storage.md) and
//! [record-format.md](../../docs/record-format.md) for details.

pub mod db;
pub mod fsutil;
pub mod meta;
pub mod paths;
pub mod records;
pub mod schema;
pub mod status;

pub use db::{OpenedCache, ProbeData, ProbeOutcome, StorageError, clear, open_or_recover, probe};
pub use paths::StoragePaths;
pub use records::{
    RecordLocation, RecordStore, RecordStoreError, StoredRecord, active_records, render_listing,
    superseded_ids,
};
pub use schema::{DbKind, Migration, supported_version};
pub use status::{CacheDbStatus, CacheStatus};
