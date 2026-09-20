//! Baseline retrieval without embeddings or LLMs (plan sections 30
//! and 31).
//!
//! - [`query`] defines [`RetrievalQuery`](query::RetrievalQuery),
//!   context budgets, safe FTS tokenization, deterministic bundle
//!   assembly, and rendering;
//! - [`membership`] proves which content belongs to the target
//!   snapshot: cache presence never proves membership (plan section 26);
//! - [`fts`] keeps the record and practice full-text indexes of the
//!   worktree cache in sync with the `.nisse` records and the bundled
//!   practice catalog;
//! - [`engine`] combines the signals — exact symbol, relation graph,
//!   FTS over skeletons/records/practices, and changed-set boosts —
//!   into bounded ranked [`ContextBundle`] values
//!   ([`Retriever`](engine::Retriever));
//! - [`rerank`] optionally cosine-reranks a bounded candidate set with
//!   a configured embedding provider, backed by the vector cache of
//!   the shared SQLite database;
//! - [`fixtures`] holds the frozen retrieval evaluation fixtures and
//!   their fixture repository (milestone 23);
//! - [`eval`] runs those fixtures and measures the frozen evaluation
//!   metrics (plan section 69).
//!
//! Embeddings are optional everywhere (permanent invariant 16); they
//! only rerank a bounded candidate set on top of this baseline.

pub mod engine;
pub mod error;
pub mod eval;
pub mod fixtures;
pub mod fts;
pub mod membership;
pub mod query;
pub mod rerank;

pub use engine::Retriever;
pub use error::RetrievalError;
pub use eval::{
    BASELINE_VARIANT, EMBEDDINGS_VARIANT, RETRIEVAL_TOP_K, RetrievalEvalError,
    RetrievalFixtureOutcome, RetrievalVariantReport, SnapshotContents, count_snapshot_leakage,
    render_retrieval_json, render_retrieval_report, run_retrieval_eval,
};
pub use membership::SnapshotMembership;
pub use query::{
    ContextBudget, RetrievalQuery, assemble_bundle, compare_entries, fts_match_expr, render_json,
    render_text, tokenize,
};
pub use rerank::{RerankError, RerankStats, rerank};
