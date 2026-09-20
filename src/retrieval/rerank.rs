//! Optional embedding reranking (plan sections 30, 31, and 34).
//!
//! When an [`EmbeddingProvider`] is configured, retrieval cosine-reranks
//! a *bounded* candidate set: embeddings never replace the baseline
//! signals, they only refine the ranking of candidates the
//! deterministic signals already produced ("do not rely exclusively on
//! vector search", plan section 30).
//!
//! Vectors are cached in the regenerable shared cache using the frozen
//! tables (plan section 29): the embedded text is stored as a
//! `text_chunk` row under the synthetic content key `emb:<sha256>`,
//! and its vector as an `embedding` row keyed by `(chunk, model)`.
//! A model change therefore starts a fresh cache namespace, and a
//! dimension change under the same model id is detected (stale rows
//! are re-embedded and overwritten) so vectors of different geometry
//! are never mixed within one ranking.
//!
//! Every failure — provider or cache — is reported to the caller,
//! which degrades to the unchanged baseline ranking; reranking is
//! always optional.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::domain::context::ContextEntry;
use crate::providers::{EmbeddingProvider, ProviderEmbeddings, ProviderError};
use crate::retrieval::query::{compare_entries, truncate_text};

/// Blend weight of the cosine similarity added to the baseline score
/// (in `[-1.0, 1.0]`, so the adjustment stays within ±this weight).
pub const COSINE_WEIGHT: f64 = 0.3;

/// Maximum number of leading candidates reranked per query.
pub const MAX_RERANK_CANDIDATES: usize = 48;

/// Maximum byte length of one text considered for embedding
/// (entries are already bounded; this is a defensive bound).
pub const RERANK_TEXT_MAX_BYTES: usize = 8_192;

/// Sentinel path of embedding-cache chunk rows (never a real
/// repository path; the rows are not part of the searchable FTS index).
pub const EMBEDDING_CACHE_PATH: &str = "(embedding-cache)";

/// Prefix of embedding-cache content keys.
pub const EMBEDDING_KEY_PREFIX: &str = "emb:";

/// Statistics of one rerank run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RerankStats {
    /// Candidates whose scores were adjusted.
    pub reranked: usize,
    /// Texts sent to the provider (including the query text).
    pub embedded_texts: usize,
    /// Texts satisfied from the vector cache.
    pub cache_hits: usize,
}

/// Reranking failures. Always optional: callers degrade to the
/// baseline ranking instead of failing the operation.
#[derive(Debug, Error)]
pub enum RerankError {
    /// The provider failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The shared vector cache could not be read or written.
    #[error("embedding cache failure: {0}")]
    Cache(String),
}

/// Rerank `entries` in place against `query_text`.
///
/// The candidate list is first ordered by the deterministic baseline
/// (`compare_entries`) and only the leading [`MAX_RERANK_CANDIDATES`]
/// entries participate; entries with empty text keep their baseline
/// score. The whole slice may exceed the bound — the remainder is
/// left untouched (and may still be admitted by the context budget).
pub fn rerank(
    conn: &Connection,
    db_path: &Path,
    provider: &dyn EmbeddingProvider,
    query_text: &str,
    entries: &mut [ContextEntry],
) -> Result<RerankStats, RerankError> {
    let query = query_text.trim();
    if query.is_empty() || entries.is_empty() {
        return Ok(RerankStats::default());
    }

    // Order by the deterministic baseline so the bounded prefix is the
    // strongest candidate set. `assemble_bundle` re-sorts afterwards.
    entries.sort_by(compare_entries);
    let take = entries.len().min(MAX_RERANK_CANDIDATES);
    let candidates = &mut entries[..take];

    // Learn the provider's current model (and query vector) with one
    // single-text call; it is the namespace of every cached vector.
    let query_text_bounded = truncate_text(query, RERANK_TEXT_MAX_BYTES);
    let first = provider.embed(&[query_text_bounded])?;
    let model = first.model.clone();
    let dim = validate_embeddings(provider.name(), &first, 1)?;
    let query_vector = first.vectors[0].clone();
    let mut embedded_texts = 1usize;
    let mut cache_hits = 0usize;

    // Collect the distinct texts of the bounded candidates.
    let mut texts: Vec<String> = Vec::new();
    for entry in candidates.iter() {
        if entry.text.is_empty() {
            continue;
        }
        let text = truncate_text(&entry.text, RERANK_TEXT_MAX_BYTES);
        if !texts.iter().any(|known| known == text) {
            texts.push(text.to_owned());
        }
    }

    // Cache lookup under the provider's model.
    let mut vectors: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    let mut missing: Vec<String> = Vec::new();
    for text in &texts {
        match cached_vector(conn, db_path, text, &model)? {
            Some(vector) if vector.len() == dim => {
                vectors.insert(text.clone(), vector);
                cache_hits += 1;
            }
            // A missing, corrupt, or stale-geometry row is re-embedded
            // and the row is overwritten (self-healing).
            _ => missing.push(text.clone()),
        }
    }

    if !missing.is_empty() {
        let refs: Vec<&str> = missing.iter().map(String::as_str).collect();
        let fresh = provider.embed(&refs)?;
        let fresh_dim = validate_embeddings(provider.name(), &fresh, missing.len())?;
        if fresh_dim != dim || fresh.model != model {
            return Err(RerankError::Provider(ProviderError::Malformed {
                provider: provider.name().to_string(),
                message: "the embedding model changed within one ranking".to_string(),
            }));
        }
        let count = refs.len();
        for (text, vector) in missing.into_iter().zip(fresh.vectors) {
            store_vector(conn, db_path, &text, &model, dim, &vector)?;
            vectors.insert(text, vector);
        }
        embedded_texts += count;
    }

    // Cosine blend over the bounded candidates.
    let mut reranked = 0usize;
    for entry in candidates.iter_mut() {
        if entry.text.is_empty() {
            continue;
        }
        let text = truncate_text(&entry.text, RERANK_TEXT_MAX_BYTES).to_owned();
        let Some(vector) = vectors.get(&text) else {
            continue;
        };
        let similarity = cosine(&query_vector, vector);
        entry.score += COSINE_WEIGHT * similarity;
        reranked += 1;
    }

    Ok(RerankStats {
        reranked,
        embedded_texts,
        cache_hits,
    })
}

/// Validate provider embeddings: one vector per expected text, uniform
/// positive dimension, finite values. Returns the dimension.
fn validate_embeddings(
    provider_name: &str,
    embeddings: &ProviderEmbeddings,
    expected: usize,
) -> Result<usize, RerankError> {
    let malformed = |message: String| {
        RerankError::Provider(ProviderError::Malformed {
            provider: provider_name.to_string(),
            message,
        })
    };
    if embeddings.vectors.len() != expected {
        return Err(malformed(format!(
            "{} vectors for {} texts",
            embeddings.vectors.len(),
            expected
        )));
    }
    let Some(first) = embeddings.vectors.first() else {
        return Err(malformed("no vectors returned".to_string()));
    };
    if first.is_empty() {
        return Err(malformed("embedding dimension is zero".to_string()));
    }
    let dim = first.len();
    for (index, vector) in embeddings.vectors.iter().enumerate() {
        if vector.len() != dim {
            return Err(malformed(format!(
                "vector {index} has dimension {} but {dim} was expected",
                vector.len()
            )));
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(malformed(format!(
                "vector {index} contains a non-finite value"
            )));
        }
    }
    Ok(dim)
}

/// The embedding-cache content key of a text: `emb:<sha256>`.
fn embedding_key(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    format!(
        "{EMBEDDING_KEY_PREFIX}{}",
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

/// Read a cached vector, if present and well-formed.
fn cached_vector(
    conn: &Connection,
    db_path: &Path,
    text: &str,
    model: &str,
) -> Result<Option<Vec<f32>>, RerankError> {
    let cache = |err: rusqlite::Error| RerankError::Cache(format!("{}: {err}", db_path.display()));
    let mut stmt = conn
        .prepare(
            "SELECT e.dim, e.vector FROM embedding e \
             JOIN text_chunk t ON t.id = e.chunk_id \
             WHERE t.content_key = ?1 AND t.path = ?2 AND t.chunk_index = 0 \
               AND e.model = ?3 LIMIT 1",
        )
        .map_err(cache)?;
    let row = stmt.query_row(
        rusqlite::params![embedding_key(text), EMBEDDING_CACHE_PATH, model],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
    );
    match row {
        Ok((dim, bytes)) => {
            let dim = dim.max(0) as usize;
            if dim == 0 || bytes.len() != dim * 4 {
                // Corrupt row: treat as absent; the caller re-embeds
                // and overwrites it.
                return Ok(None);
            }
            Ok(Some(decode_vector(&bytes)))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(other) => Err(cache(other)),
    }
}

/// Store a vector: ensure the cache chunk row exists, then upsert the
/// embedding under `(chunk, model)` so model-geometry changes repair
/// stale rows.
fn store_vector(
    conn: &Connection,
    db_path: &Path,
    text: &str,
    model: &str,
    dim: usize,
    vector: &[f32],
) -> Result<(), RerankError> {
    let cache = |err: rusqlite::Error| RerankError::Cache(format!("{}: {err}", db_path.display()));
    let key = embedding_key(text);
    let tx = conn
        .unchecked_transaction()
        .map_err(|err| RerankError::Cache(format!("{}: {err}", db_path.display())))?;
    tx.execute(
        "INSERT OR IGNORE INTO text_chunk (content_key, path, chunk_index, text) \
         VALUES (?1, ?2, 0, ?3)",
        rusqlite::params![key, EMBEDDING_CACHE_PATH, text],
    )
    .map_err(cache)?;
    let chunk_id: i64 = tx
        .query_row(
            "SELECT id FROM text_chunk \
             WHERE content_key = ?1 AND path = ?2 AND chunk_index = 0",
            rusqlite::params![key, EMBEDDING_CACHE_PATH],
            |row| row.get(0),
        )
        .map_err(cache)?;
    let bytes = encode_vector(vector);
    tx.execute(
        "INSERT INTO embedding (chunk_id, model, dim, vector, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5) \
         ON CONFLICT(chunk_id, model) DO UPDATE SET \
           dim = excluded.dim, vector = excluded.vector, created_at = excluded.created_at",
        rusqlite::params![
            chunk_id,
            model,
            dim as i64,
            bytes,
            crate::domain::time::now_utc_rfc3339()
        ],
    )
    .map_err(cache)?;
    tx.commit().map_err(cache)?;
    Ok(())
}

/// Encode a vector as little-endian `f32` bytes.
fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Decode little-endian `f32` bytes (length validated by the caller).
fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Cosine similarity; zero for zero-norm vectors (never NaN).
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len(), "dimensions are validated before use");
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::providers::ProviderEmbeddings;

    /// A deterministic fake embedder: vectors by keyword, a mutable
    /// model id, and a call counter.
    struct FakeEmbedder {
        model: Mutex<String>,
        calls: AtomicUsize,
        fail: Mutex<bool>,
    }

    impl FakeEmbedder {
        fn new(model: &str) -> Self {
            FakeEmbedder {
                model: Mutex::new(model.to_string()),
                calls: AtomicUsize::new(0),
                fail: Mutex::new(false),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn vector_for(text: &str) -> Vec<f32> {
            match text {
                t if t.contains("alpha") => vec![1.0, 0.0],
                t if t.contains("beta") => vec![0.0, 1.0],
                // The fallback hash-vector keeps distinct texts distinct
                // in the binary-identical dimension space.
                _ => vec![0.6, 0.8],
            }
        }
    }

    impl EmbeddingProvider for FakeEmbedder {
        fn name(&self) -> &'static str {
            "fake-embedder"
        }

        fn embed(&self, texts: &[&str]) -> Result<ProviderEmbeddings, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if *self.fail.lock().unwrap() {
                return Err(ProviderError::Transport {
                    provider: "fake-embedder".to_string(),
                    message: "scripted failure".to_string(),
                });
            }
            Ok(ProviderEmbeddings {
                model: self.model.lock().unwrap().clone(),
                vectors: texts.iter().map(|t| Self::vector_for(t)).collect(),
            })
        }
    }

    fn cache_db() -> (PathBuf, Connection) {
        // Leak the tempdir: the database file must outlive the helper.
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("cache.sqlite");
        let opened =
            crate::storage::db::open_or_recover(&path, crate::storage::schema::DbKind::Shared)
                .unwrap();
        (path, opened.conn)
    }

    fn entry(text: &str, score: f64) -> ContextEntry {
        ContextEntry {
            kind: crate::domain::context::ContextKind::SourceSkeleton,
            title: text.to_string(),
            path: Some(format!("src/{text}.rs")),
            symbol: None,
            line_start: None,
            line_end: None,
            record_id: None,
            practice_id: None,
            text: text.to_string(),
            score,
        }
    }

    #[test]
    fn cosine_reranking_reorders_by_similarity() {
        let (_path, conn) = cache_db();
        let provider = FakeEmbedder::new("mini");
        // Baseline puts beta first; the query "alpha" makes the alpha
        // entry more similar (cosine 1.0 vs 0.0).
        let mut entries = vec![entry("beta file", 0.6), entry("alpha file", 0.5)];
        let stats = rerank(
            &conn,
            Path::new("cache"),
            &provider,
            "alpha query",
            &mut entries,
        )
        .unwrap();
        assert_eq!(stats.reranked, 2);
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.embedded_texts, 3, "query plus two texts");
        let alpha = entries.iter().find(|e| e.text == "alpha file").unwrap();
        let beta = entries.iter().find(|e| e.text == "beta file").unwrap();
        let expected_alpha = 0.5 + COSINE_WEIGHT; // cosine 1.0
        let expected_beta = 0.6 + COSINE_WEIGHT * 0.0; // orthogonal
        assert!((alpha.score - expected_alpha).abs() < 1e-9);
        assert!((beta.score - expected_beta).abs() < 1e-9);
        // The blended alpha score now wins once the bundle is ranked.
        let mut ranked = entries.clone();
        ranked.sort_by(compare_entries);
        assert_eq!(ranked[0].text, "alpha file");
    }

    #[test]
    fn vector_cache_prevents_re_embedding() {
        let (path, conn) = cache_db();
        let provider = FakeEmbedder::new("mini");

        let mut first = vec![entry("alpha file", 0.5), entry("beta file", 0.6)];
        let stats = rerank(&conn, &path, &provider, "alpha query", &mut first).unwrap();
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(
            provider.calls(),
            2,
            "one call per round: query, then misses"
        );

        // Cache rows exist under the synthetic content key. The query
        // text is always embedded fresh (it also discovers the model),
        // so exactly the two entry texts are cached.
        let rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM text_chunk WHERE content_key LIKE 'emb:%' \
                 AND path = ?1",
                [EMBEDDING_CACHE_PATH],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2, "the two entry texts");
        let vectors: i64 = conn
            .query_row("SELECT count(*) FROM embedding", [], |row| row.get(0))
            .unwrap();
        assert_eq!(vectors, 2);

        // Second identical ranking: only the model-discovery call
        // remains; both entry texts come from the cache.
        let mut second = vec![entry("beta file", 0.6), entry("alpha file", 0.5)];
        let stats = rerank(&conn, &path, &provider, "alpha query", &mut second).unwrap();
        assert_eq!(stats.cache_hits, 2);
        assert_eq!(stats.embedded_texts, 1, "only the query text");
        assert_eq!(provider.calls(), 3, "exactly one more call (the query)");
        // Identical results: reranking is deterministic with the cache.
        let scores = |entries: &[ContextEntry]| -> std::collections::BTreeMap<String, f64> {
            entries
                .iter()
                .map(|entry| (entry.text.clone(), entry.score))
                .collect()
        };
        assert_eq!(scores(&second), scores(&first));
    }

    #[test]
    fn model_changes_start_a_fresh_cache_namespace() {
        let (path, conn) = cache_db();
        let provider = FakeEmbedder::new("m1");
        let mut entries = vec![entry("alpha file", 0.5)];
        rerank(&conn, &path, &provider, "alpha query", &mut entries).unwrap();

        // The provider switches models: everything re-embeds under m2.
        *provider.model.lock().unwrap() = "m2".to_string();
        let mut entries = vec![entry("alpha file", 0.5)];
        let stats = rerank(&conn, &path, &provider, "alpha query", &mut entries).unwrap();
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.embedded_texts, 2);

        // Both namespaces are cached side by side; no old vector is
        // used for the new model.
        let models: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT DISTINCT model FROM embedding ORDER BY model")
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(models, vec!["m1".to_string(), "m2".to_string()]);
    }

    #[test]
    fn dimension_change_under_the_same_model_self_heals() {
        let (path, conn) = cache_db();
        // dim-2 vectors first.
        let provider = FakeEmbedder::new("m");
        let mut entries = vec![entry("alpha file", 0.5)];
        rerank(&conn, &path, &provider, "alpha query", &mut entries).unwrap();

        // The same model now returns 3-dim vectors: the stale cache
        // rows mismatch the query geometry and are re-embedded and
        // overwritten instead of mixing geometries.
        struct WideEmbedder(FakeEmbedder);
        impl EmbeddingProvider for WideEmbedder {
            fn name(&self) -> &'static str {
                "fake-embedder"
            }
            fn embed(&self, texts: &[&str]) -> Result<ProviderEmbeddings, ProviderError> {
                let inner = EmbeddingProvider::embed(&self.0, texts)?;
                Ok(ProviderEmbeddings {
                    model: inner.model,
                    vectors: inner
                        .vectors
                        .into_iter()
                        .map(|v| {
                            let mut wide = v;
                            wide.push(0.0);
                            wide
                        })
                        .collect(),
                })
            }
        }
        let wide = WideEmbedder(FakeEmbedder::new("m"));
        let mut entries = vec![entry("alpha file", 0.5)];
        let stats = rerank(&conn, &path, &wide, "alpha query", &mut entries).unwrap();
        assert_eq!(stats.cache_hits, 0, "stale-geometry rows re-embed");
        let dim: i64 = conn
            .query_row("SELECT dim FROM embedding", [], |row| row.get(0))
            .unwrap();
        assert_eq!(dim, 3, "the stale row was overwritten");
    }

    #[test]
    fn zero_vectors_and_malformed_providers_degrade() {
        let (path, conn) = cache_db();
        let provider = FakeEmbedder::new("m");

        // A zero query vector: cosine is zero everywhere, never NaN.
        struct ZeroEmbedder(FakeEmbedder);
        impl EmbeddingProvider for ZeroEmbedder {
            fn name(&self) -> &'static str {
                "fake-embedder"
            }
            fn embed(&self, texts: &[&str]) -> Result<ProviderEmbeddings, ProviderError> {
                let inner = EmbeddingProvider::embed(&self.0, texts)?;
                Ok(ProviderEmbeddings {
                    model: inner.model,
                    vectors: vec![vec![0.0, 0.0]; texts.len()],
                })
            }
        }
        let mut entries = vec![entry("alpha file", 0.5), entry("beta file", 0.6)];
        let scores = |entries: &[ContextEntry]| -> std::collections::BTreeMap<String, f64> {
            entries
                .iter()
                .map(|entry| (entry.text.clone(), entry.score))
                .collect()
        };
        let baseline = scores(&entries);
        let stats = rerank(
            &conn,
            &path,
            &ZeroEmbedder(FakeEmbedder::new("m0")),
            "q alpha",
            &mut entries,
        )
        .unwrap();
        assert_eq!(stats.reranked, 2);
        assert_eq!(
            scores(&entries),
            baseline,
            "zero similarity changes nothing"
        );

        // A failing provider reports the error; the caller degrades.
        let failing = FakeEmbedder::new("mf");
        *failing.fail.lock().unwrap() = true;
        let mut entries = vec![entry("alpha file", 0.5)];
        assert!(matches!(
            rerank(&conn, &path, &failing, "alpha query", &mut entries),
            Err(RerankError::Provider(ProviderError::Transport { .. }))
        ));

        // A provider answering with the wrong vector count is rejected.
        struct ShortEmbedder(FakeEmbedder);
        impl EmbeddingProvider for ShortEmbedder {
            fn name(&self) -> &'static str {
                "fake-embedder"
            }
            fn embed(&self, texts: &[&str]) -> Result<ProviderEmbeddings, ProviderError> {
                let inner = EmbeddingProvider::embed(&self.0, texts)?;
                Ok(ProviderEmbeddings {
                    model: inner.model,
                    vectors: vec![inner.vectors[0].clone()],
                })
            }
        }
        let mut entries = vec![entry("alpha file", 0.5), entry("beta file", 0.6)];
        assert!(matches!(
            rerank(
                &conn,
                &path,
                &ShortEmbedder(FakeEmbedder::new("ms")),
                "alpha",
                &mut entries
            ),
            Err(RerankError::Provider(ProviderError::Malformed { .. }))
        ));

        // Empty query text and empty candidate texts are no-ops.
        let mut entries = vec![entry("", 0.5)];
        let stats = rerank(&conn, &path, &provider, "", &mut entries).unwrap();
        assert_eq!(stats, RerankStats::default());
        assert_eq!(provider.calls(), 0, "no provider call without a query");

        let mut entries = vec![entry("", 0.5)];
        let stats = rerank(&conn, &path, &provider, "alpha query", &mut entries).unwrap();
        assert_eq!(stats.reranked, 0);
        assert_eq!(provider.calls(), 1, "only the query call");
    }

    #[test]
    fn only_the_bounded_leading_candidates_are_reranked() {
        let (_path, conn) = cache_db();
        let provider = FakeEmbedder::new("m");
        // More entries than the bound: the tail keeps its baseline
        // score, and the provider never sees more texts than the bound.
        let mut entries: Vec<ContextEntry> = (0..MAX_RERANK_CANDIDATES + 20)
            .map(|index| entry(&format!("tail {index}"), 0.1))
            .collect();
        entries[0].text = "alpha file".to_string();
        entries[1].text = "beta file".to_string();
        let stats = rerank(
            &conn,
            Path::new("c"),
            &provider,
            "alpha query",
            &mut entries,
        )
        .unwrap();
        assert_eq!(stats.reranked, MAX_RERANK_CANDIDATES);
        let tail = &entries[MAX_RERANK_CANDIDATES];
        assert_eq!(tail.score, 0.1, "the tail is untouched");
    }

    #[test]
    fn vector_encoding_roundtrips_and_keys_are_stable() {
        assert_eq!(embedding_key("alpha"), embedding_key("alpha"));
        assert_ne!(embedding_key("alpha"), embedding_key("beta"));
        assert!(embedding_key("alpha").starts_with("emb:"));

        let vector = vec![1.0f32, -0.5, 0.25];
        assert_eq!(decode_vector(&encode_vector(&vector)), vector);
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), 1.0);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-12);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }
}
