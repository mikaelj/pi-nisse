//! The baseline retrieval engine (plan section 30).
//!
//! [`Retriever`] combines the signals available without embeddings and
//! without any LLM:
//!
//! - **exact symbol** — definitions looked up by name in the shared
//!   symbol table, mapped through worktree membership so results are
//!   always target-snapshot content;
//! - **relation graph** — references to a symbol, same membership rule;
//! - **full-text search** — SQLite FTS5 over source skeletons
//!   (shared cache, content-addressed), durable project records, and
//!   bundled practices (worktree cache);
//! - **changed-set signals** — exact boosts for changed symbols and
//!   changed paths plus module/path proximity.
//!
//! Ranking is deterministic: each signal contributes bounded entries
//! with a kind weight (plan section 31 priority) attenuated by rank
//! position; boosts never depend on model output. Cache rows whose
//! content is not part of the target snapshot are filtered out before
//! entries are built — cache presence never proves snapshot membership
//! (plan section 26).

use std::collections::BTreeSet;
use std::path::PathBuf;

use rusqlite::Connection;

use super::error::RetrievalError;
use super::fts::{sync_practices, sync_records};
use super::membership::SnapshotMembership;
use super::query::{
    ContextBudget, MAX_ENTRIES_PER_SIGNAL, RetrievalQuery, assemble_bundle, fts_match_expr,
    truncate_text,
};
use super::rerank::rerank;
use crate::domain::context::{ContextEntry, ContextKind};
use crate::git::ResolvedSnapshot;
use crate::index::artifact::{PARSER_VERSION, ParsedArtifact};
use crate::jury::catalog::Catalog;
use crate::providers::EmbeddingProvider;
use crate::storage::StoragePaths;
use crate::storage::db::open_or_recover;
use crate::storage::meta::{META_LAST_REFRESH_SNAPSHOT, meta_get};
use crate::storage::records::StoredRecord;
use crate::storage::schema::DbKind;

/// Kind weights follow the context priority of plan section 31.
const WEIGHT_SYMBOL_DEFINITION: f64 = 1.0;
const WEIGHT_PROJECT_FACT: f64 = 0.9;
const WEIGHT_CONFLICTING_FACT: f64 = 0.45;
/// Potentially stale facts (plan section 48) are downranked to the
/// same weight as conflicting facts: both are presented, never as
/// unquestioned current truth.
const WEIGHT_POTENTIALLY_STALE_FACT: f64 = 0.45;
const WEIGHT_SOURCE_SKELETON: f64 = 0.8;
const WEIGHT_SYMBOL_REFERENCE: f64 = 0.7;
const WEIGHT_PROJECT_LESSON: f64 = 0.6;
const WEIGHT_PRACTICE: f64 = 0.5;

/// Score attenuation per rank position within one signal's results.
const RANK_DECAY: f64 = 0.8;

/// Boost for entries about an exactly changed symbol.
const BOOST_CHANGED_SYMBOL: f64 = 0.25;
/// Boost for entries on an exactly changed path.
const BOOST_CHANGED_PATH: f64 = 0.15;
/// Boost for entries in the same directory as a changed path
/// (module/path proximity).
const BOOST_MODULE_PROXIMITY: f64 = 0.10;
/// Boost for records consolidated under `.nisse/common` (plan section
/// 45: common records may rank higher than contributor records).
const BOOST_COMMON_RECORD: f64 = 0.05;

/// Baseline retriever over one worktree's caches.
pub struct Retriever {
    shared: Connection,
    shared_path: PathBuf,
    worktree: Connection,
    worktree_path: PathBuf,
    membership: SnapshotMembership,
    /// Optional embedding provider reranking a bounded candidate set
    /// (plan section 30). `None` — the default — keeps retrieval fully
    /// deterministic without any provider.
    embedding_provider: Option<std::sync::Arc<dyn EmbeddingProvider>>,
}

impl Retriever {
    /// Open the retriever for a resolved snapshot.
    ///
    /// The worktree index must have been refreshed for exactly this
    /// snapshot: the membership tables and the recorded snapshot
    /// binding must match the descriptor, otherwise retrieval refuses
    /// to run ([`RetrievalError::SnapshotMismatch`]) rather than risk
    /// returning content of a different snapshot.
    pub fn open(
        resolved: &ResolvedSnapshot,
        storage: &StoragePaths,
    ) -> Result<Self, RetrievalError> {
        let shared = open_or_recover(&storage.shared, DbKind::Shared)?;
        let worktree = open_or_recover(&storage.worktree, DbKind::Worktree)?;

        let expected = serde_json::to_string(&resolved.descriptor)
            .expect("snapshot descriptors always serialize");
        match meta_get(
            &worktree.conn,
            &storage.worktree,
            META_LAST_REFRESH_SNAPSHOT,
        )? {
            Some(found) if found == expected => {}
            Some(found) => {
                return Err(RetrievalError::SnapshotMismatch { found, expected });
            }
            None => {
                return Err(RetrievalError::SnapshotMismatch {
                    found: "<no refresh recorded>".to_string(),
                    expected,
                });
            }
        }

        let membership = SnapshotMembership::load(&worktree.conn, &storage.worktree)?;
        Ok(Retriever {
            shared: shared.conn,
            shared_path: storage.shared.clone(),
            worktree: worktree.conn,
            worktree_path: storage.worktree.clone(),
            membership,
            embedding_provider: None,
        })
    }

    /// Attach an optional embedding provider (plan sections 30 and
    /// 34). Every lookup then cosine-reranks a bounded candidate set;
    /// any provider failure degrades to the unchanged baseline
    /// ranking — retrieval never depends on the provider.
    pub fn with_embedding_provider(
        mut self,
        provider: std::sync::Arc<dyn EmbeddingProvider>,
    ) -> Self {
        self.embedding_provider = Some(provider);
        self
    }

    /// Synchronize the record full-text index with the durable records
    /// of the worktree. Safe to call before every query.
    ///
    /// `stale` names the potentially stale facts of the current target
    /// snapshot (plan section 48; see
    /// [`assess_records`](crate::learning::staleness::assess_records)).
    /// They are marked and downranked at retrieval time.
    pub fn sync_records(
        &self,
        records: &[StoredRecord],
        stale: &BTreeSet<String>,
    ) -> Result<(), RetrievalError> {
        sync_records(&self.worktree, &self.worktree_path, records, stale).map(|_| ())
    }

    /// Synchronize the practice full-text index with the bundled
    /// catalog. Safe to call before every query.
    pub fn sync_practices(&self, catalog: &Catalog) -> Result<(), RetrievalError> {
        sync_practices(&self.worktree, &self.worktree_path, catalog)
    }

    /// Membership view of the target snapshot (for diagnostics and
    /// tests).
    pub fn membership(&self) -> &SnapshotMembership {
        &self.membership
    }

    /// Definitions of `name` in the target snapshot.
    ///
    /// Each definition is reported once per snapshot path carrying the
    /// defining content, with its kind and line span.
    pub fn symbol_lookup(&self, name: &str) -> Result<Vec<ContextEntry>, RetrievalError> {
        self.symbol_entries(name)
    }

    /// Known bounded relationships of `name`: its definitions plus the
    /// places that reference it, all inside the target snapshot.
    pub fn relation_lookup(&self, name: &str) -> Result<Vec<ContextEntry>, RetrievalError> {
        let mut entries = self.symbol_entries(name)?;
        entries.extend(self.reference_entries(name)?);
        Ok(entries)
    }

    /// Run one full query and assemble the bounded context bundle.
    pub fn lookup(
        &self,
        query: &RetrievalQuery,
    ) -> Result<crate::domain::context::ContextBundle, RetrievalError> {
        query.validate()?;
        let tokens = query.tokens();
        let mut candidates: Vec<ContextEntry> = Vec::new();

        // Exact symbol and relation signals. Changed symbols come first
        // in `symbol_names`, so their entries surface earliest on ties.
        for name in query.symbol_names() {
            candidates.extend(self.symbol_entries(&name)?);
        }
        for name in query.symbol_names() {
            candidates.extend(self.reference_entries(&name)?);
        }

        // Full-text signals.
        if let Some(expr) = fts_match_expr(&tokens) {
            candidates.extend(self.skeleton_entries(&expr)?);
            candidates.extend(self.record_entries(&expr)?);
            candidates.extend(self.practice_entries(&expr)?);
        }

        apply_changed_set_boosts(&mut candidates, query);

        // Optional embeddings refine the ranking of a bounded candidate
        // set (plan section 30); failures degrade to the baseline.
        if let Some(provider) = &self.embedding_provider {
            match rerank(
                &self.shared,
                &self.shared_path,
                provider.as_ref(),
                &query.text,
                &mut candidates,
            ) {
                Ok(stats) => {
                    tracing::debug!(?stats, "optional embedding rerank applied");
                }
                Err(err) => {
                    tracing::warn!(%err, "optional embedding rerank skipped");
                }
            }
        }

        let budget = ContextBudget::default();
        Ok(assemble_bundle(candidates, &budget))
    }

    /// Definition entries for one exact symbol name.
    fn symbol_entries(&self, name: &str) -> Result<Vec<ContextEntry>, RetrievalError> {
        let mut stmt = self
            .shared
            .prepare(
                "SELECT s.kind, s.line_start, s.line_end, pa.content_key, pa.language, pa.artifact \
                 FROM symbol s JOIN parsed_artifact pa ON pa.id = s.parsed_artifact_id \
                 WHERE s.name = ?1 AND pa.parser_version = ?2 \
                 ORDER BY pa.content_key, s.line_start",
            )
            .map_err(|err| self.shared_error(err))?;
        let rows = stmt
            .query_map(rusqlite::params![name, PARSER_VERSION], |row| {
                Ok(SymbolRow {
                    kind: row.get(0)?,
                    line_start: row.get::<_, Option<i64>>(1)?,
                    line_end: row.get::<_, Option<i64>>(2)?,
                    content_key: row.get(3)?,
                    language: row.get(4)?,
                    artifact_json: row.get(5)?,
                })
            })
            .map_err(|err| self.shared_error(err))?;

        let mut entries = Vec::new();
        for row in rows {
            let row = row.map_err(|err| self.shared_error(err))?;
            let paths = self
                .membership
                .paths_for(&row.content_key, &row.language)
                .to_vec();
            if paths.is_empty() {
                // Content exists in the shared cache but is not part of
                // the target snapshot: never leak it.
                continue;
            }
            let skeleton_line = row.skeleton_line(name);
            for path in paths {
                if entries.len() >= MAX_ENTRIES_PER_SIGNAL {
                    break;
                }
                entries.push(ContextEntry {
                    kind: ContextKind::SymbolDefinition,
                    title: format!("{} {name}", row.kind),
                    path: Some(path),
                    symbol: Some(name.to_owned()),
                    line_start: row.line_start.map(|line| line.max(0) as u32),
                    line_end: row.line_end.map(|line| line.max(0) as u32),
                    record_id: None,
                    practice_id: None,
                    text: truncate_text(
                        &skeleton_line,
                        ContextBudget::default().max_entry_text_bytes,
                    )
                    .to_owned(),
                    score: WEIGHT_SYMBOL_DEFINITION,
                });
            }
            if entries.len() >= MAX_ENTRIES_PER_SIGNAL {
                break;
            }
        }
        Ok(entries)
    }

    /// Reference entries for one exact symbol name.
    fn reference_entries(&self, name: &str) -> Result<Vec<ContextEntry>, RetrievalError> {
        let mut stmt = self
            .shared
            .prepare(
                "SELECT r.line, pa.content_key, pa.language \
                 FROM reference r JOIN parsed_artifact pa ON pa.id = r.parsed_artifact_id \
                 WHERE r.name = ?1 AND pa.parser_version = ?2 \
                 ORDER BY pa.content_key, r.line",
            )
            .map_err(|err| self.shared_error(err))?;
        let rows = stmt
            .query_map(rusqlite::params![name, PARSER_VERSION], |row| {
                Ok(ReferenceRow {
                    line: row.get(0)?,
                    content_key: row.get(1)?,
                    language: row.get(2)?,
                })
            })
            .map_err(|err| self.shared_error(err))?;

        let mut entries = Vec::new();
        for row in rows {
            let row = row.map_err(|err| self.shared_error(err))?;
            let paths = self
                .membership
                .paths_for(&row.content_key, &row.language)
                .to_vec();
            for path in paths {
                if entries.len() >= MAX_ENTRIES_PER_SIGNAL {
                    return Ok(entries);
                }
                entries.push(ContextEntry {
                    kind: ContextKind::SymbolReference,
                    title: format!("reference to {name}"),
                    path: Some(path),
                    symbol: Some(name.to_owned()),
                    line_start: row.line.map(|line| line.max(0) as u32),
                    line_end: row.line.map(|line| line.max(0) as u32),
                    record_id: None,
                    practice_id: None,
                    text: String::new(),
                    score: WEIGHT_SYMBOL_REFERENCE,
                });
            }
        }
        Ok(entries)
    }

    /// Skeleton entries matching one FTS expression.
    ///
    /// Identical bytes parsed under two languages produce two chunk
    /// rows with the same content key; only the first row per content
    /// key contributes entries so paths are never duplicated.
    fn skeleton_entries(&self, expr: &str) -> Result<Vec<ContextEntry>, RetrievalError> {
        let mut stmt = self
            .shared
            .prepare(
                "SELECT f.text, f.content_key \
                 FROM text_chunk_fts f WHERE text_chunk_fts MATCH ?1 \
                 ORDER BY bm25(text_chunk_fts), f.content_key, f.path \
                 LIMIT ?2",
            )
            .map_err(|err| self.shared_error(err))?;
        let rows = stmt
            .query_map(
                rusqlite::params![expr, MAX_ENTRIES_PER_SIGNAL as i64],
                |row| {
                    Ok(SkeletonRow {
                        text: row.get(0)?,
                        content_key: row.get(1)?,
                    })
                },
            )
            .map_err(|err| self.shared_error(err))?;

        let mut entries = Vec::new();
        let mut seen_content: BTreeSet<String> = BTreeSet::new();
        for row in rows {
            let row = row.map_err(|err| self.shared_error(err))?;
            if !seen_content.insert(row.content_key.clone()) {
                continue;
            }
            let paths = self.membership.paths_for_content(&row.content_key);
            for path in paths {
                if entries.len() >= MAX_ENTRIES_PER_SIGNAL {
                    return Ok(entries);
                }
                entries.push(ContextEntry {
                    kind: ContextKind::SourceSkeleton,
                    title: path.clone(),
                    path: Some(path.clone()),
                    symbol: None,
                    line_start: None,
                    line_end: None,
                    record_id: None,
                    practice_id: None,
                    text: truncate_text(&row.text, ContextBudget::default().max_entry_text_bytes)
                        .to_owned(),
                    score: WEIGHT_SOURCE_SKELETON,
                });
            }
        }
        Ok(entries)
    }

    /// Record entries (facts and lessons) matching one FTS expression.
    ///
    /// Conflicting facts are marked `[conflicting]` and downranked;
    /// potentially stale facts (plan section 48) are marked
    /// `[potentially stale]` and downranked the same way — current
    /// source evidence outranks stale memory (permanent invariant 22).
    fn record_entries(&self, expr: &str) -> Result<Vec<ContextEntry>, RetrievalError> {
        let mut stmt = self
            .worktree
            .prepare(
                "SELECT f.record_id, f.subject, f.text, f.kind, f.state, ri.path, \
                 ri.potentially_stale \
                 FROM record_fts f JOIN record_index ri ON ri.record_id = f.record_id \
                 WHERE record_fts MATCH ?1 \
                 ORDER BY bm25(record_fts), f.record_id \
                 LIMIT ?2",
            )
            .map_err(|err| self.worktree_error(err))?;
        let rows = stmt
            .query_map(
                rusqlite::params![expr, MAX_ENTRIES_PER_SIGNAL as i64],
                |row| {
                    Ok(RecordRow {
                        record_id: row.get(0)?,
                        subject: row.get(1)?,
                        text: row.get(2)?,
                        kind: row.get(3)?,
                        state: row.get(4)?,
                        path: row.get(5)?,
                        potentially_stale: row.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .map_err(|err| self.worktree_error(err))?;

        let mut entries = Vec::new();
        for row in rows {
            let row = row.map_err(|err| self.worktree_error(err))?;
            let is_common = row.path.starts_with("common/");
            let conflicting = row.state == "unresolved_conflict";
            let mut weight = match (row.kind.as_str(), conflicting) {
                ("fact", false) => WEIGHT_PROJECT_FACT,
                ("fact", true) => WEIGHT_CONFLICTING_FACT,
                _ => WEIGHT_PROJECT_LESSON,
            };
            if row.potentially_stale {
                weight = weight.min(WEIGHT_POTENTIALLY_STALE_FACT);
            }
            let rank = entries.len() as f64;
            let mut score = weight * RANK_DECAY.powi(rank as i32);
            if is_common {
                score += BOOST_COMMON_RECORD;
            }
            let kind = if row.kind == "fact" {
                ContextKind::ProjectFact
            } else {
                ContextKind::ProjectLesson
            };
            let mut title = row.subject;
            if conflicting {
                title.push_str(" [conflicting]");
            }
            if row.potentially_stale {
                title.push_str(" [potentially stale]");
            }
            entries.push(ContextEntry {
                kind,
                title,
                path: Some(format!(".nisse/{}", row.path)),
                symbol: None,
                line_start: None,
                line_end: None,
                record_id: Some(row.record_id),
                practice_id: None,
                text: truncate_text(&row.text, ContextBudget::default().max_entry_text_bytes)
                    .to_owned(),
                score,
            });
        }
        Ok(entries)
    }

    /// Practice entries matching one FTS expression.
    fn practice_entries(&self, expr: &str) -> Result<Vec<ContextEntry>, RetrievalError> {
        let mut stmt = self
            .worktree
            .prepare(
                "SELECT f.practice_id, f.pack_id, f.title, f.statement \
                 FROM practice_fts f WHERE practice_fts MATCH ?1 \
                 ORDER BY bm25(practice_fts), f.practice_id \
                 LIMIT ?2",
            )
            .map_err(|err| self.worktree_error(err))?;
        let rows = stmt
            .query_map(
                rusqlite::params![expr, MAX_ENTRIES_PER_SIGNAL as i64],
                |row| {
                    Ok(PracticeRow {
                        practice_id: row.get(0)?,
                        title: row.get(2)?,
                        statement: row.get(3)?,
                    })
                },
            )
            .map_err(|err| self.worktree_error(err))?;

        let mut entries = Vec::new();
        for (rank, row) in rows.enumerate() {
            let row = row.map_err(|err| self.worktree_error(err))?;
            entries.push(ContextEntry {
                kind: ContextKind::Practice,
                title: row.title,
                path: None,
                symbol: None,
                line_start: None,
                line_end: None,
                record_id: None,
                practice_id: Some(row.practice_id),
                text: truncate_text(
                    &row.statement,
                    ContextBudget::default().max_entry_text_bytes,
                )
                .to_owned(),
                score: WEIGHT_PRACTICE * RANK_DECAY.powi(rank as i32),
            });
        }
        Ok(entries)
    }

    /// Map a rusqlite failure of a shared-cache query.
    fn shared_error(&self, err: rusqlite::Error) -> RetrievalError {
        RetrievalError::Storage(crate::storage::StorageError::Query {
            path: self.shared_path.clone(),
            message: err.to_string(),
        })
    }

    /// Map a rusqlite failure of a worktree-cache query.
    fn worktree_error(&self, err: rusqlite::Error) -> RetrievalError {
        RetrievalError::Storage(crate::storage::StorageError::Query {
            path: self.worktree_path.clone(),
            message: err.to_string(),
        })
    }
}

/// One definition row of the shared symbol table.
struct SymbolRow {
    kind: String,
    line_start: Option<i64>,
    line_end: Option<i64>,
    content_key: String,
    language: String,
    artifact_json: String,
}

impl SymbolRow {
    /// The skeleton line naming `name`, when the artifact provides one;
    /// used as the bounded excerpt of the definition. Token equality
    /// prevents a definition named `alpha` from excerpting the line of
    /// an unrelated `alpha_counter`.
    fn skeleton_line(&self, name: &str) -> String {
        serde_json::from_str::<ParsedArtifact>(&self.artifact_json)
            .ok()
            .and_then(|artifact| {
                artifact
                    .skeleton
                    .lines()
                    .find(|line| line.split_whitespace().any(|token| token == name))
                    .map(str::to_owned)
            })
            .unwrap_or_default()
    }
}

/// One reference row of the shared reference table.
struct ReferenceRow {
    line: Option<i64>,
    content_key: String,
    language: String,
}

/// One skeleton match of the shared full-text index.
struct SkeletonRow {
    text: String,
    content_key: String,
}

/// One durable-record match of the worktree full-text index.
struct RecordRow {
    record_id: String,
    subject: String,
    text: String,
    kind: String,
    state: String,
    path: String,
    /// Cached staleness marking of the current target snapshot (plan
    /// section 48); recomputed by every record sync.
    potentially_stale: bool,
}

/// One practice match of the worktree full-text index.
struct PracticeRow {
    practice_id: String,
    title: String,
    statement: String,
}

/// Apply the deterministic changed-set boosts (plan section 30):
/// changed symbols, changed paths, and module/path proximity.
fn apply_changed_set_boosts(entries: &mut [ContextEntry], query: &RetrievalQuery) {
    let changed_directories: BTreeSet<&str> = query
        .changed_paths
        .iter()
        .map(|path| parent_directory(path))
        .collect();
    for entry in entries.iter_mut() {
        if let Some(symbol) = &entry.symbol
            && query
                .changed_symbols
                .iter()
                .any(|changed| changed == symbol)
        {
            entry.score += BOOST_CHANGED_SYMBOL;
        }
        if let Some(path) = &entry.path {
            if query.changed_paths.iter().any(|changed| changed == path) {
                entry.score += BOOST_CHANGED_PATH;
            } else if changed_directories.contains(parent_directory(path)) {
                entry.score += BOOST_MODULE_PROXIMITY;
            }
        }
    }
}

/// Directory prefix of a repository-relative path (`""` for top-level
/// paths).
fn parent_directory(path: &str) -> &str {
    match path.rfind('/') {
        Some(index) => &path[..index],
        None => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::context::{ContextBundle, ContextKind};
    use crate::domain::evidence::{EvidenceKind, EvidenceRef};
    use crate::domain::records::{FactRecord, FactState, LessonRecord, RoutingHint};
    use crate::git::GitRunner;
    use crate::git::testutil::{Fixture, commit_all, git_in, write_file};
    use crate::git::{SnapshotSpec, resolve_snapshot};
    use crate::index::refresh_index;
    use crate::storage::records::RecordStore;

    fn discover(fixture: &Fixture) -> crate::git::RepoContext {
        crate::git::RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap()
    }

    fn prepare(
        fixture: &Fixture,
        spec: &SnapshotSpec,
    ) -> (
        crate::git::RepoContext,
        ResolvedSnapshot,
        StoragePaths,
        Retriever,
    ) {
        let ctx = discover(fixture);
        let resolved = resolve_snapshot(&ctx, spec).unwrap();
        let storage = StoragePaths::for_repo(&ctx);
        refresh_index(&ctx, &resolved, &storage).unwrap();
        let retriever = Retriever::open(&resolved, &storage).unwrap();
        (ctx, resolved, storage, retriever)
    }

    fn make_fact(id: &str, subject: &str, text: &str, state: FactState) -> StoredRecord {
        StoredRecord {
            record: crate::domain::records::ProjectRecord::Fact(FactRecord {
                schema_version: 1,
                id: id.to_string(),
                author: "kalle".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                subject: subject.to_string(),
                text: text.to_string(),
                tags: vec!["queue".to_string()],
                state,
                evidence: vec![EvidenceRef {
                    kind: EvidenceKind::Source,
                    path: Some("src/queue.rs".to_string()),
                    symbol: None,
                    line_start: Some(1),
                    line_end: Some(2),
                    content_key: None,
                    plan_section: None,
                    record_id: None,
                    excerpt: "static QUEUE".to_string(),
                }],
                source_review_id: None,
                source_record_ids: Vec::new(),
                supersedes: Vec::new(),
            }),
            location: crate::storage::records::RecordLocation::People {
                contributor: "kalle".to_string(),
            },
            path: fixture_path("kalle", id),
        }
    }

    fn make_lesson(id: &str, subject: &str, text: &str) -> StoredRecord {
        StoredRecord {
            record: crate::domain::records::ProjectRecord::Lesson(LessonRecord {
                schema_version: 1,
                id: id.to_string(),
                author: "kalle".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                subject: subject.to_string(),
                text: text.to_string(),
                tags: vec!["queue".to_string()],
                routing_hint: RoutingHint {
                    reviewer_id: "concurrency".to_string(),
                    path_globs: vec!["src/*.rs".to_string()],
                    required_tags_any: Vec::new(),
                    boost: 0.2,
                },
                source_observation_ids: vec!["01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()],
                supersedes: Vec::new(),
            }),
            location: crate::storage::records::RecordLocation::People {
                contributor: "kalle".to_string(),
            },
            path: fixture_path("kalle", id),
        }
    }

    fn fixture_path(contributor: &str, id: &str) -> PathBuf {
        PathBuf::from(format!("/repo/.nisse/people/{contributor}/{id}.json"))
    }

    #[test]
    fn symbol_lookup_finds_exact_definitions_in_the_target_snapshot() {
        let fixture = Fixture::repo("retr-symbol");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn alpha() -> u8 {\n    1\n}\n\npub struct Config {\n    pub size: u8,\n}\n",
        );
        commit_all(&fixture.root, "one");
        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);

        let entries = retriever.symbol_lookup("alpha").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, ContextKind::SymbolDefinition);
        assert_eq!(entries[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(entries[0].symbol.as_deref(), Some("alpha"));
        assert_eq!(entries[0].line_start, Some(1));
        assert_eq!(entries[0].line_end, Some(3));
        assert!(entries[0].title.contains("alpha"));
        assert!(entries[0].text.contains("alpha"), "skeleton excerpt");

        // Exact matching only: no substring or case variants.
        assert!(retriever.symbol_lookup("alph").unwrap().is_empty());
        assert!(retriever.symbol_lookup("Alpha").unwrap().is_empty());
        assert!(retriever.symbol_lookup("missing").unwrap().is_empty());
    }

    #[test]
    fn symbol_lookup_reports_duplicate_content_at_every_path() {
        let fixture = Fixture::repo("retr-duplicate");
        let content = "pub fn shared() -> u8 {\n    1\n}\n";
        write_file(&fixture.root, "src/one.rs", content);
        write_file(&fixture.root, "vendored/one_copy.rs", content);
        commit_all(&fixture.root, "one");
        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);

        let entries = retriever.symbol_lookup("shared").unwrap();
        let paths: Vec<&str> = entries
            .iter()
            .map(|entry| entry.path.as_deref().unwrap())
            .collect();
        assert_eq!(paths, ["src/one.rs", "vendored/one_copy.rs"]);
    }

    #[test]
    fn relation_lookup_returns_definitions_and_references() {
        let fixture = Fixture::repo("retr-relation");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn shared_tool() -> u8 {\n    1\n}\n",
        );
        write_file(
            &fixture.root,
            "src/main.rs",
            "mod lib;\n\nfn main() {\n    let value = shared_tool();\n    let _ = value;\n}\n",
        );
        commit_all(&fixture.root, "one");
        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);

        let entries = retriever.relation_lookup("shared_tool").unwrap();
        let definitions: Vec<&ContextEntry> = entries
            .iter()
            .filter(|entry| entry.kind == ContextKind::SymbolDefinition)
            .collect();
        let references: Vec<&ContextEntry> = entries
            .iter()
            .filter(|entry| entry.kind == ContextKind::SymbolReference)
            .collect();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].path.as_deref(), Some("src/lib.rs"));
        assert!(!references.is_empty(), "usage must be found");
        assert!(
            references
                .iter()
                .all(|entry| entry.path.as_deref() == Some("src/main.rs"))
        );
        assert!(references.iter().all(|entry| entry.line_start.is_some()));
    }

    #[test]
    fn full_text_lookup_covers_skeletons_and_survives_garbage_queries() {
        let fixture = Fixture::repo("retr-text");
        write_file(
            &fixture.root,
            "src/protocol.rs",
            "pub struct Frame {\n    pub len: u16,\n}\n\npub fn parse_frame() -> u8 {\n    1\n}\n",
        );
        commit_all(&fixture.root, "one");
        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);

        // Text search over skeletons: token "frame" matches the skeleton.
        let bundle = retriever
            .lookup(&RetrievalQuery::from_text("frame parsing"))
            .unwrap();
        assert!(
            bundle
                .entries
                .iter()
                .any(|entry| entry.kind == ContextKind::SourceSkeleton
                    && entry.path.as_deref() == Some("src/protocol.rs"))
        );

        // FTS syntax characters cannot corrupt the query.
        let bundle = retriever
            .lookup(&RetrievalQuery::from_text("frame) \" OR 1=1 --"))
            .unwrap();
        assert!(!bundle.is_empty());

        // Nothing usable: empty but successful.
        let bundle = retriever
            .lookup(&RetrievalQuery::from_text("!!! ???"))
            .unwrap();
        assert!(bundle.is_empty());
        assert!(!bundle.truncated);
    }

    #[test]
    fn lookup_retrieves_facts_and_lessons_with_state_semantics() {
        let fixture = Fixture::repo("retr-records");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn x() -> u8 {\n    1\n}\n",
        );
        commit_all(&fixture.root, "one");

        let store = RecordStore::new(&fixture.root);
        store
            .create(
                &make_fact(
                    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "queue locking",
                    "The ring buffer is only touched with the bus mutex held.",
                    FactState::Supported,
                )
                .record,
            )
            .unwrap();
        store
            .create(
                &make_fact(
                    "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                    "conflicting timers",
                    "Timer two is said to be unused here.",
                    FactState::UnresolvedConflict,
                )
                .record,
            )
            .unwrap();
        store
            .create(
                &make_lesson(
                    "01ARZ3NDEKTSV4RRFFQ69G5FAX",
                    "queue review lesson",
                    "Queue changes always need the concurrency reviewer here.",
                )
                .record,
            )
            .unwrap();
        let records = store.load().unwrap();

        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);
        retriever.sync_records(&records, &BTreeSet::new()).unwrap();
        retriever
            .sync_practices(&Catalog::bundled().unwrap())
            .unwrap();

        let bundle = retriever
            .lookup(&RetrievalQuery::from_text("ring buffer mutex"))
            .unwrap();
        let fact = bundle
            .entries
            .iter()
            .find(|entry| entry.record_id.as_deref() == Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"))
            .expect("supported fact must be found");
        assert_eq!(fact.kind, ContextKind::ProjectFact);
        assert!(
            fact.path
                .as_deref()
                .unwrap()
                .starts_with(".nisse/people/kalle/")
        );

        // A conflicting fact is retrievable but marked and downranked.
        let bundle = retriever
            .lookup(&RetrievalQuery::from_text("timer unused"))
            .unwrap();
        let conflicting = bundle
            .entries
            .iter()
            .find(|entry| entry.record_id.as_deref() == Some("01ARZ3NDEKTSV4RRFFQ69G5FAW"))
            .expect("conflicting fact must be surfaced as conflict");
        assert_eq!(conflicting.kind, ContextKind::ProjectFact);
        assert!(conflicting.title.contains("[conflicting]"));

        // Lessons are retrieved as lessons.
        let bundle = retriever
            .lookup(&RetrievalQuery::from_text("concurrency reviewer queue"))
            .unwrap();
        assert!(
            bundle
                .entries
                .iter()
                .any(|entry| entry.kind == ContextKind::ProjectLesson
                    && entry.record_id.as_deref() == Some("01ARZ3NDEKTSV4RRFFQ69G5FAX"))
        );
    }

    #[test]
    fn lookup_retrieves_bundled_practices() {
        let fixture = Fixture::repo("retr-practices");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn x() -> u8 {\n    1\n}\n",
        );
        commit_all(&fixture.root, "one");
        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);
        let catalog = Catalog::bundled().unwrap();
        retriever.sync_practices(&catalog).unwrap();

        // Derive the expectation from the catalog instead of hard-coding
        // bundled wording.
        let practice = catalog
            .practice_packs()
            .iter()
            .flat_map(|pack| pack.practices.iter())
            .find(|practice| {
                practice
                    .statement
                    .to_lowercase()
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|token| token == "lock")
            })
            .expect("bundled packs mention locking");
        let statement = practice.statement.to_lowercase();
        let token = statement
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .find(|token| *token == "lock")
            .unwrap();

        let bundle = retriever.lookup(&RetrievalQuery::from_text(token)).unwrap();
        let entry = bundle
            .entries
            .iter()
            .find(|entry| entry.practice_id.as_deref() == Some(practice.id.as_str()))
            .unwrap_or_else(|| panic!("practice `{}` must be found", practice.id));
        assert_eq!(entry.kind, ContextKind::Practice);
        assert_eq!(entry.text, practice.statement);
    }

    #[test]
    fn zero_branch_leakage_between_snapshots_of_the_same_path() {
        let fixture = Fixture::repo("retr-leakage");
        write_file(
            &fixture.root,
            "src/mod.rs",
            "pub fn alpha_fn() -> u8 {\n    1\n}\n",
        );
        commit_all(&fixture.root, "alpha");
        git_in(&fixture.root, &["checkout", "--quiet", "-b", "feature"]);
        write_file(
            &fixture.root,
            "src/mod.rs",
            "pub fn beta_fn() -> u8 {\n    2\n}\n",
        );
        commit_all(&fixture.root, "beta");
        // Main worktree returns to the alpha content.
        git_in(
            &fixture.root,
            &["checkout", "--quiet", fixture.branch_name.as_str()],
        );

        // Linked worktree on the feature branch.
        let wt_path = fixture
            .sibling("wt")
            .to_str()
            .expect("worktree path is valid UTF-8")
            .to_owned();
        git_in(
            &fixture.root,
            &["worktree", "add", "--quiet", &wt_path, "feature"],
        );
        let wt_root = std::path::Path::new(&wt_path).canonicalize().unwrap();
        let wt_ctx = crate::git::RepoContext::discover(&GitRunner::new(), &wt_root).unwrap();
        let wt_storage = StoragePaths::for_repo(&wt_ctx);

        // Refresh main (alpha) and the linked worktree (beta): the shared
        // cache now holds artifacts for both branches' content.
        let (_ctx, _resolved, _storage, main_retriever) = prepare(&fixture, &SnapshotSpec::Working);
        let wt_resolved = resolve_snapshot(&wt_ctx, &SnapshotSpec::Working).unwrap();
        refresh_index(&wt_ctx, &wt_resolved, &wt_storage).unwrap();
        let wt_retriever = Retriever::open(&wt_resolved, &wt_storage).unwrap();

        // The shared cache genuinely contains both branches' symbols…
        let both: i64 = main_retriever
            .shared
            .query_row(
                "SELECT count(*) FROM symbol WHERE name IN ('alpha_fn', 'beta_fn')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(both, 2, "shared cache must hold both branches' artifacts");

        // …but the feature worktree only ever returns beta content.
        assert!(wt_retriever.symbol_lookup("beta_fn").unwrap().len() == 1);
        assert!(
            wt_retriever.symbol_lookup("alpha_fn").unwrap().is_empty(),
            "content of another branch must never leak"
        );
        let bundle = wt_retriever
            .lookup(&RetrievalQuery::from_text("alpha_fn beta_fn"))
            .unwrap();
        assert!(
            bundle
                .entries
                .iter()
                .all(|entry| entry.path.as_deref() != Some("src/mod.rs")
                    || entry.symbol.as_deref() != Some("alpha_fn"))
        );

        // And the main worktree only returns alpha content.
        assert!(main_retriever.symbol_lookup("alpha_fn").unwrap().len() == 1);
        assert!(main_retriever.symbol_lookup("beta_fn").unwrap().is_empty());
    }

    #[test]
    fn open_rejects_membership_of_a_different_snapshot() {
        let fixture = Fixture::repo("retr-stale");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn one() -> u8 {\n    1\n}\n",
        );
        commit_all(&fixture.root, "one");

        let (ctx, _resolved, storage, _retriever) = prepare(&fixture, &SnapshotSpec::Working);

        // HEAD moves: a freshly discovered context resolves a working
        // snapshot whose descriptor no longer matches the recorded
        // binding of the worktree index.
        write_file(
            &fixture.root,
            "src/two.rs",
            "pub fn two() -> u8 {\n    2\n}\n",
        );
        commit_all(&fixture.root, "two");
        let fresh = discover(&fixture);
        let stale = resolve_snapshot(&fresh, &SnapshotSpec::Working).unwrap();
        let err = match Retriever::open(&stale, &storage) {
            Ok(_) => panic!("retrieval must refuse membership of another snapshot"),
            Err(err) => err,
        };
        assert!(
            matches!(err, RetrievalError::SnapshotMismatch { .. }),
            "unexpected error: {err:?}"
        );

        // Refreshing for the new snapshot restores retrieval. (The old
        // context is deliberately usable here: its HEAD still names a
        // real commit; only the binding check needed the fresh one.)
        let _ = ctx;
        refresh_index(&fresh, &stale, &storage).unwrap();
        let retriever = Retriever::open(&stale, &storage).unwrap();
        assert!(retriever.symbol_lookup("two").unwrap().len() == 1);
    }

    #[test]
    fn lookup_respects_bundle_budgets() {
        let fixture = Fixture::repo("retr-budget");
        // Four modules, each defining and using the same eight helpers:
        // symbol, reference, and skeleton signals together produce far
        // more candidates than the default entry budget of 32 (only the
        // first eight query tokens drive exact symbol lookups).
        for file in 1..=4 {
            let mut source = String::new();
            for i in 1..=8 {
                source.push_str(&format!("pub fn tok{i}() -> u8 {{\n    {i}\n}}\n\n"));
            }
            source.push_str("pub fn use_all() -> u8 {\n");
            for i in 1..=8 {
                source.push_str(&format!("    let v{i} = tok{i}();\n"));
            }
            source.push_str("    0\n}\n");
            write_file(&fixture.root, &format!("src/mod{file}.rs"), &source);
        }
        commit_all(&fixture.root, "one");

        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);
        retriever
            .sync_practices(&Catalog::bundled().unwrap())
            .unwrap();

        let query_text = (1..=8)
            .map(|i| format!("tok{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let bundle = retriever
            .lookup(&RetrievalQuery::from_text(query_text))
            .unwrap();
        let budget = ContextBudget::default();
        assert_eq!(bundle.entries.len(), budget.max_entries, "bundle is capped");
        assert!(bundle.truncated, "dropped candidates must be reported");
        assert!(bundle.text_bytes <= budget.max_total_text_bytes);
        assert!(
            bundle
                .entries
                .iter()
                .all(|entry| entry.text.len() <= budget.max_entry_text_bytes)
        );
        // Ordering is stable: best scores first, deterministic ties.
        let mut sorted = bundle.entries.clone();
        sorted.sort_by(super::super::query::compare_entries);
        assert_eq!(bundle.entries, sorted);
    }

    #[test]
    fn changed_set_signals_boost_affected_source() {
        let fixture = Fixture::repo("retr-boost");
        write_file(
            &fixture.root,
            "src/queue.rs",
            "pub fn alpha() -> u8 {\n    1\n}\n\npub fn beta() -> u8 {\n    2\n}\n",
        );
        write_file(
            &fixture.root,
            "other/alpha.rs",
            "pub fn alpha() -> u8 {\n    9\n}\n",
        );
        commit_all(&fixture.root, "one");
        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);

        // Plain query: two alpha definitions exist; deterministic order
        // decides (path tie-break).
        let plain = retriever
            .lookup(&RetrievalQuery::from_text("alpha"))
            .unwrap();
        assert_eq!(
            plain.entries[0].path.as_deref(),
            Some("other/alpha.rs"),
            "equal scores tie-break on path"
        );

        // Changed-set signals: the queue.rs definition must win.
        let boosted = retriever
            .lookup(&RetrievalQuery {
                text: "alpha".to_string(),
                changed_symbols: vec!["alpha".to_string()],
                changed_paths: vec!["src/queue.rs".to_string()],
            })
            .unwrap();
        assert_eq!(boosted.entries[0].path.as_deref(), Some("src/queue.rs"));
        assert!(boosted.entries[0].score > 1.0, "boost applied");
        // Module proximity also lifts the sibling in the same directory.
        let queue_entries: Vec<&ContextEntry> = boosted
            .entries
            .iter()
            .filter(|entry| entry.path.as_deref() == Some("src/queue.rs"))
            .collect();
        assert_eq!(
            queue_entries.len(),
            2,
            "alpha and beta in the changed module"
        );
    }

    #[test]
    fn potentially_stale_facts_are_marked_and_downranked() {
        let fixture = Fixture::repo("retr-stale-fact");
        write_file(
            &fixture.root,
            "src/lib.rs",
            "pub fn x() -> u8 {\n    1\n}\n",
        );
        commit_all(&fixture.root, "one");

        let store = RecordStore::new(&fixture.root);
        store
            .create(
                &make_fact(
                    "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "fresh queue fact",
                    "The fresh queue fact is about ring buffer discipline.",
                    FactState::Supported,
                )
                .record,
            )
            .unwrap();
        store
            .create(
                &make_fact(
                    "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                    "beta queue fact",
                    "The beta queue fact is about ring buffer discipline.",
                    FactState::Supported,
                )
                .record,
            )
            .unwrap();
        let records = store.load().unwrap();

        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);
        retriever.sync_records(&records, &BTreeSet::new()).unwrap();

        // Fresh sync: both facts rank as ordinary project facts.
        let fresh = retriever
            .lookup(&RetrievalQuery::from_text("queue fact ring buffer"))
            .unwrap();
        let fresh_entry = fresh
            .entries
            .iter()
            .find(|entry| entry.record_id.as_deref() == Some("01ARZ3NDEKTSV4RRFFQ69G5FAW"))
            .expect("the fact must be retrievable");
        assert!(!fresh_entry.title.contains("[potentially stale]"));

        // Re-sync with the second fact stale: it is marked and
        // downranked below the fresh fact.
        let stale = BTreeSet::from(["01ARZ3NDEKTSV4RRFFQ69G5FAW".to_string()]);
        retriever.sync_records(&records, &stale).unwrap();
        let marked = retriever
            .lookup(&RetrievalQuery::from_text("queue fact ring buffer"))
            .unwrap();
        let stale_entry = marked
            .entries
            .iter()
            .find(|entry| entry.record_id.as_deref() == Some("01ARZ3NDEKTSV4RRFFQ69G5FAW"))
            .expect("a stale fact stays retrievable, never hidden");
        assert!(
            stale_entry.title.contains("[potentially stale]"),
            "{}",
            stale_entry.title
        );
        let fresh_score = marked
            .entries
            .iter()
            .find(|entry| entry.record_id.as_deref() == Some("01ARZ3NDEKTSV4RRFFQ69G5FAV"))
            .expect("the fresh fact stays retrievable")
            .score;
        assert!(
            fresh_score > stale_entry.score,
            "fresh {fresh_score} must outrank stale {}",
            stale_entry.score
        );
    }

    #[test]
    fn empty_repository_yields_empty_source_results() {
        let fixture = Fixture::repo("retr-empty");
        // Unborn HEAD, no files: refresh and retrieval still work.
        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);
        assert!(retriever.membership().is_empty());
        let bundle: ContextBundle = retriever
            .lookup(&RetrievalQuery::from_text("anything"))
            .unwrap();
        // Practices still participate; no source entries exist.
        assert!(
            bundle
                .entries
                .iter()
                .all(|entry| entry.kind != ContextKind::SymbolDefinition
                    && entry.kind != ContextKind::SourceSkeleton)
        );
    }
    /// An optional embedding provider for rerank tests: deterministic
    /// keyword vectors, or a scripted failure.
    struct KeywordEmbedder {
        fail: bool,
    }

    impl crate::providers::EmbeddingProvider for KeywordEmbedder {
        fn name(&self) -> &'static str {
            "test-keywords"
        }

        fn embed(
            &self,
            texts: &[&str],
        ) -> Result<crate::providers::ProviderEmbeddings, crate::providers::ProviderError> {
            if self.fail {
                return Err(crate::providers::ProviderError::Transport {
                    provider: self.name().to_string(),
                    message: "scripted failure".to_string(),
                });
            }
            Ok(crate::providers::ProviderEmbeddings {
                model: "keywords".to_string(),
                vectors: texts
                    .iter()
                    .map(|text| {
                        if text.contains("alpha_fn") {
                            vec![1.0, 0.0]
                        } else {
                            vec![0.0, 1.0]
                        }
                    })
                    .collect(),
            })
        }
    }

    #[test]
    fn optional_reranker_reorders_and_degrades_gracefully() {
        let fixture = Fixture::repo("retr-rerank");
        // The path tie-break puts the beta definition first in the
        // baseline ranking; the embedding provider must lift the
        // alpha definition above it for the `alpha_fn` query.
        write_file(
            &fixture.root,
            "src/zz_alpha.rs",
            "pub fn alpha_fn() -> u8 {\n    1\n}\n",
        );
        write_file(
            &fixture.root,
            "src/aa_beta.rs",
            "pub fn beta_fn() -> u8 {\n    2\n}\n",
        );
        commit_all(&fixture.root, "one");

        let (_ctx, _resolved, _storage, retriever) = prepare(&fixture, &SnapshotSpec::Working);
        let query = RetrievalQuery::from_text("alpha_fn beta_fn");

        let baseline = retriever.lookup(&query).unwrap();
        assert_eq!(
            baseline.entries[0].path.as_deref(),
            Some("src/aa_beta.rs"),
            "baseline tie-break is the path order"
        );

        // A failing provider degrades to exactly the baseline bundle.
        let failing = Retriever::open(&_resolved, &_storage)
            .unwrap()
            .with_embedding_provider(std::sync::Arc::new(KeywordEmbedder { fail: true }));
        let degraded = failing.lookup(&query).unwrap();
        assert_eq!(degraded, baseline, "provider failure changes nothing");

        // A working provider cosine-reranks the bounded candidate set:
        // the alpha definition becomes the top entry.
        let reranking = Retriever::open(&_resolved, &_storage)
            .unwrap()
            .with_embedding_provider(std::sync::Arc::new(KeywordEmbedder { fail: false }));
        let reranked = reranking.lookup(&query).unwrap();
        assert_eq!(reranked.entries[0].path.as_deref(), Some("src/zz_alpha.rs"));
        assert!(reranked.entries[0].score > baseline.entries[0].score);
        // The same entries are present; only their ranking changed.
        let paths = |bundle: &ContextBundle| {
            let mut paths: Vec<String> = bundle
                .entries
                .iter()
                .map(|entry| entry.path.clone().unwrap_or_default())
                .collect();
            paths.sort();
            paths
        };
        assert_eq!(paths(&reranked), paths(&baseline));
    }
}
