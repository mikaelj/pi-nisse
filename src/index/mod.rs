//! Content-addressed source index (plan sections 27 and 28).
//!
//! The index maps file **content** (never paths) to parse artifacts:
//!
//! - [`language`] classifies paths into the seven indexed languages;
//! - [`content`] computes content keys (`git:<blob-oid>` for Git object
//!   content, `sha256:<hash>` for dirty or untracked working-tree
//!   content);
//! - [`artifact`] is the parsed-artifact model whose identity is
//!   `(content key, language, parser version)`;
//! - [`extract`] turns source text into artifacts deterministically with
//!   tree-sitter;
//! - [`refresh`] performs the lazy incremental refresh against the exact
//!   target snapshot, reusing artifacts across branches and worktrees.
//!
//! Invariants honored here: Git is the snapshot and membership authority
//! (all enumeration and reads go through [`crate::git::SnapshotReader`]);
//! artifacts may be reused across branches/worktrees because identity is
//! content-based; generated cache state is regenerable.

pub mod artifact;
pub mod content;
pub mod error;
pub mod extract;
pub mod language;
pub mod refresh;

pub use artifact::{
    ExtractedIndex, IdentifierRef, ImportDecl, PARSER_VERSION, ParsedArtifact, REFERENCE_ROW_LIMIT,
    SymbolDef, SymbolKind,
};
pub use content::ContentKey;
pub use error::IndexError;
pub use extract::{IndexerCache, LanguageIndexer, TreeSitterIndexer};
pub use language::Language;
pub use refresh::{RefreshStats, refresh_index};
