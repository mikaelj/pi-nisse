//! Optional classifier/embedding providers (plan section 34).
//!
//! Nisse works fully without any helper provider: rules-based routing
//! always exists (permanent invariant 14) and baseline retrieval never
//! requires embeddings (permanent invariant 16). When configured, a
//! provider may only *improve* optional behavior:
//!
//! - **routing** — the provider proposes specialist scores that are
//!   deterministically blended into the mandatory rules decision;
//!   jury construction still enforces `quality` always participates,
//!   at most three specialists, 2..=4 total, and the dynamic fallback
//!   (permanent invariant 13). The provider can never add reviewers,
//!   tools, or capabilities ([`routing::apply_provider_scores`]).
//! - **embeddings** — the provider supplies vectors used to cosine
//!   rerank a *bounded* candidate set of retrieval results
//!   ([`crate::retrieval::rerank`]); vector search is never the only
//!   signal.
//!
//! Every provider failure — timeout, crash, malformed output,
//! untrusted endpoint — degrades to the deterministic behavior instead
//! of failing the operation (plan section 34). Providers never mutate
//! durable state, never control Git, and never decide persistence.

pub mod needle;
pub mod routing;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::routing::RoutingInput;

/// Failures of an optional provider. Never fatal to the operation:
/// callers degrade to deterministic behavior (plan section 34).
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The configured endpoint is not trusted under the network policy
    /// (plan section 10). No request was attempted.
    #[error("endpoint `{endpoint}` is not trusted: {reason}")]
    UntrustedEndpoint {
        /// The configured endpoint URL.
        endpoint: String,
        /// Why the endpoint was rejected.
        reason: String,
    },
    /// The provider did not answer within the timeout.
    #[error("provider `{provider}` timed out after {after_ms} ms")]
    Timeout {
        /// Provider name.
        provider: String,
        /// Elapsed bound in milliseconds.
        after_ms: u64,
    },
    /// The provider could not be reached or answered with a transport
    /// failure (connection refused, HTTP error status, …).
    #[error("provider `{provider}` transport failed: {message}")]
    Transport {
        /// Provider name.
        provider: String,
        /// Failure detail.
        message: String,
    },
    /// The provider answered with output that violates its schema or
    /// bounds. The whole response is discarded (design principle:
    /// invalid output causes less automation, not guessed state).
    #[error("provider `{provider}` returned malformed output: {message}")]
    Malformed {
        /// Provider name.
        provider: String,
        /// What was invalid.
        message: String,
    },
}

/// One provider-assigned reviewer score.
///
/// The score is a proposal only: it is validated against the static
/// catalog and blended into the rules decision deterministically
/// ([`routing::apply_provider_scores`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderScore {
    /// Reviewer id; must name a static specialist of the catalog.
    pub reviewer_id: String,
    /// Proposed score in `[0.0, 1.0]`.
    pub score: f64,
}

/// The result of one embedding call: the model that produced the
/// vectors (their cache identity) and exactly one vector per input
/// text, all of one uniform dimension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEmbeddings {
    /// Embedding model identifier (non-empty; keys the vector cache).
    pub model: String,
    /// One vector per input text, all of the same dimension.
    pub vectors: Vec<Vec<f32>>,
}

/// Stable routing-classifier provider abstraction (plan section 33:
/// the optional `NeedleClassifier`).
pub trait RoutingClassifierProvider: Send + Sync {
    /// Stable provider name used in diagnostics.
    fn name(&self) -> &'static str;
    /// Propose scores for the static specialists of the catalog.
    fn route(&self, input: &RoutingInput) -> Result<Vec<ProviderScore>, ProviderError>;
}

/// Stable embedding provider abstraction.
pub trait EmbeddingProvider: Send + Sync {
    /// Stable provider name used in diagnostics.
    fn name(&self) -> &'static str;
    /// Embed `texts` (bounded by the caller). Implementations return
    /// exactly one vector per text under one model identifier.
    fn embed(&self, texts: &[&str]) -> Result<ProviderEmbeddings, ProviderError>;
}

/// Build the optional Needle adapter for one configured provider
/// section, or `None` when the provider is `none`/unconfigured.
///
/// Endpoint trust is enforced per request inside the adapter (plan
/// section 10); construction never performs I/O.
pub fn needle_adapter_from_config(
    provider: crate::config::Provider,
    endpoint: Option<&str>,
    allow_non_private_endpoints: bool,
) -> Option<needle::NeedleAdapter> {
    match (provider, endpoint) {
        (crate::config::Provider::Needle, Some(endpoint)) => Some(needle::NeedleAdapter::new(
            endpoint,
            allow_non_private_endpoints,
        )),
        // Configuration validation requires an endpoint for `needle`;
        // a missing endpoint degrades to no provider rather than a guess.
        (crate::config::Provider::Needle, None) => None,
        (crate::config::Provider::None, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_score_roundtrip_and_strictness() {
        let score = ProviderScore {
            reviewer_id: "security".to_string(),
            score: 0.75,
        };
        let json = serde_json::to_string(&score).unwrap();
        assert_eq!(json, r#"{"reviewer_id":"security","score":0.75}"#);
        let back: ProviderScore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, score);

        assert!(
            serde_json::from_str::<ProviderScore>(
                r#"{"reviewer_id":"security","score":0.5,"extra":true}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<ProviderScore>(r#"{"reviewer_id":"security"}"#).is_err());
    }

    #[test]
    fn provider_embeddings_roundtrip_and_strictness() {
        let embeddings = ProviderEmbeddings {
            model: "mini".to_string(),
            vectors: vec![vec![0.5, -0.5], vec![1.0, 0.0]],
        };
        let json = serde_json::to_string(&embeddings).unwrap();
        let back: ProviderEmbeddings = serde_json::from_str(&json).unwrap();
        assert_eq!(back, embeddings);

        assert!(
            serde_json::from_str::<ProviderEmbeddings>(
                r#"{"model":"mini","vectors":[[1.0]],"dim":1}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<ProviderEmbeddings>(r#"{"model":"mini"}"#).is_err());
    }

    #[test]
    fn adapter_construction_follows_configuration() {
        use crate::config::Provider;

        assert!(needle_adapter_from_config(Provider::None, None, false).is_none());
        assert!(
            needle_adapter_from_config(Provider::None, Some("http://127.0.0.1:1"), false).is_none()
        );
        // A needle provider without an endpoint (impossible through
        // validated configuration) degrades to no provider.
        assert!(needle_adapter_from_config(Provider::Needle, None, false).is_none());

        let adapter =
            needle_adapter_from_config(Provider::Needle, Some("http://127.0.0.1:8787"), false)
                .expect("needle adapter");
        assert_eq!(adapter.endpoint(), "http://127.0.0.1:8787");
    }
}
