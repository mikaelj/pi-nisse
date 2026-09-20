//! The optional local Needle adapter (plan section 34).
//!
//! [`NeedleAdapter`] speaks a small strict JSON protocol to a local
//! Needle-compatible service over HTTP(S) and provides exactly two
//! operations:
//!
//! - `route` — propose specialist reviewer scores for a
//!   [`RoutingInput`];
//! - `embed` — produce one vector per input text under one model id.
//!
//! Safety properties, all enforced here or by the deterministic
//! consumers of this adapter:
//!
//! - the configured endpoint is trust-checked before every request
//!   (plan section 10): hostnames are resolved, and every resolved
//!   address must belong to a default-permitted class unless trusted
//!   local configuration explicitly overrides the policy;
//! - every call is bounded by an explicit timeout;
//! - responses are parsed against strict schemas; any violation
//!   (unknown fields, missing fields, out-of-range scores, ragged
//!   dimensions, wrong vector counts) rejects the whole response as
//!   [`ProviderError::Malformed`], which callers degrade from;
//! - reviewer ids returned by `route` are *not* trusted here; the
//!   deterministic blend validates them against the static catalog
//!   ([`super::routing::apply_provider_scores`]).
//!
//! The adapter never mutates durable state and never decides
//! persistence. All I/O goes through the injectable
//! [`NeedleTransport`] so tests run without network access.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    EmbeddingProvider, ProviderEmbeddings, ProviderError, ProviderScore, RoutingClassifierProvider,
};
use crate::config::endpoint::{HostResolver, SystemResolver, check_endpoint_trust};
use crate::domain::routing::RoutingInput;

/// Provider name of the Needle adapter.
pub const PROVIDER_NAME: &str = "needle";

/// Default timeout of one `route` call.
pub const DEFAULT_ROUTE_TIMEOUT_MS: u64 = 5_000;

/// Default timeout of one `embed` call.
pub const DEFAULT_EMBED_TIMEOUT_MS: u64 = 30_000;

/// Maximum number of reviewer scores accepted from one `route`
/// response (the catalog holds at most a dozen static specialists).
pub const MAX_ROUTE_REVIEWERS: usize = 16;

/// Maximum byte length of one reviewer id.
pub const MAX_REVIEWER_ID_BYTES: usize = 64;

/// Maximum number of texts sent in one `embed` request.
pub const MAX_EMBED_TEXTS: usize = 64;

/// Maximum byte length of one text sent for embedding.
pub const MAX_EMBED_TEXT_BYTES: usize = 8_192;

/// Maximum accepted embedding dimension.
pub const MAX_EMBED_DIM: usize = 4_096;

/// Maximum byte length of the reported embedding model id.
pub const MAX_EMBED_MODEL_BYTES: usize = 128;

/// Bound of every classifier-input list sent to `route` (plan section
/// 33 classifier input; upstream producers already bound these lists,
/// the adapter bounds them again defensively).
const MAX_INPUT_LIST: usize = 64;

/// Low-level transport failure of one request.
#[derive(Debug, Error)]
pub enum TransportFailure {
    /// The request exceeded its timeout.
    #[error("request timed out")]
    Timeout,
    /// The request could not be completed (connection refused, DNS
    /// failure inside the HTTP stack, HTTP error status, …).
    #[error("{0}")]
    Other(String),
}

/// Transport abstraction: one JSON `POST` returning the raw response
/// body. Production uses [`HttpTransport`]; tests inject fakes.
pub trait NeedleTransport: Send + Sync {
    /// POST `body` to `url` within `timeout`.
    fn post(&self, url: &str, body: &[u8], timeout: Duration) -> Result<Vec<u8>, TransportFailure>;
}

/// Production HTTP transport (`reqwest` with `rustls`; plan section
/// 9). Environment proxies are disabled so a local service is never
/// silently rerouted through a proxy.
#[derive(Debug, Default)]
pub struct HttpTransport;

impl NeedleTransport for HttpTransport {
    fn post(&self, url: &str, body: &[u8], timeout: Duration) -> Result<Vec<u8>, TransportFailure> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
            .map_err(|err| TransportFailure::Other(format!("client build failed: {err}")))?;
        let response = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_vec())
            .send()
            .map_err(map_reqwest)?;
        let status = response.status();
        if !status.is_success() {
            return Err(TransportFailure::Other(format!("HTTP {status}")));
        }
        response
            .bytes()
            .map(|bytes| bytes.to_vec())
            .map_err(map_reqwest)
    }
}

/// Map a reqwest failure to a transport failure.
fn map_reqwest(err: reqwest::Error) -> TransportFailure {
    if err.is_timeout() {
        TransportFailure::Timeout
    } else {
        TransportFailure::Other(err.to_string())
    }
}

/// The `route` request payload: the bounded classifier input.
#[derive(Debug, Serialize)]
struct RouteRequest<'a> {
    descriptors: &'a [String],
    changed_paths: &'a [String],
    languages: &'a [String],
    changed_symbols: &'a [String],
    deterministic_tags: &'a [String],
    lessons: Vec<LessonHintWire>,
}

/// One lesson routing hint forwarded to the provider: only the
/// deterministic hint fields, never lesson text.
#[derive(Debug, Serialize)]
struct LessonHintWire {
    reviewer_id: String,
    boost: f64,
}

/// The strict `route` response.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteResponse {
    reviewers: Vec<RouteReviewer>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteReviewer {
    reviewer_id: String,
    score: f64,
}

/// The `embed` request payload.
#[derive(Debug, Serialize)]
struct EmbedRequest {
    texts: Vec<String>,
}

/// The strict `embed` response.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedResponse {
    model: String,
    embeddings: Vec<Vec<f32>>,
}

/// The local Needle adapter: optional classification and embeddings
/// only (plan section 34).
pub struct NeedleAdapter {
    endpoint: String,
    allow_non_private_endpoints: bool,
    route_timeout: Duration,
    embed_timeout: Duration,
    transport: Box<dyn NeedleTransport>,
    resolver: Arc<dyn HostResolver>,
}

impl NeedleAdapter {
    /// Create an adapter for `endpoint` under the network policy.
    /// Trust is enforced per request; construction performs no I/O.
    pub fn new(endpoint: &str, allow_non_private_endpoints: bool) -> Self {
        NeedleAdapter {
            endpoint: endpoint.to_string(),
            allow_non_private_endpoints,
            route_timeout: Duration::from_millis(DEFAULT_ROUTE_TIMEOUT_MS),
            embed_timeout: Duration::from_millis(DEFAULT_EMBED_TIMEOUT_MS),
            transport: Box::new(HttpTransport),
            resolver: Arc::new(SystemResolver),
        }
    }

    /// Override the operation timeouts (tests inject small bounds).
    pub fn with_timeouts(mut self, route_timeout: Duration, embed_timeout: Duration) -> Self {
        self.route_timeout = route_timeout;
        self.embed_timeout = embed_timeout;
        self
    }

    /// Replace the transport (tests inject fakes).
    pub fn with_transport(mut self, transport: Box<dyn NeedleTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Replace the hostname resolver (tests inject fakes; the resolver
    /// is only consulted for name endpoints before trust is granted).
    pub fn with_resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// The configured endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Propose specialist scores for `input`.
    pub fn route(&self, input: &RoutingInput) -> Result<Vec<ProviderScore>, ProviderError> {
        let request = RouteRequest {
            descriptors: bound_list(&input.descriptors),
            changed_paths: bound_list(&input.changed_paths),
            languages: bound_list(&input.languages),
            changed_symbols: bound_list(&input.changed_symbols),
            deterministic_tags: bound_list(&input.deterministic_tags),
            lessons: input
                .lessons
                .iter()
                .take(MAX_INPUT_LIST)
                .map(|hint| LessonHintWire {
                    reviewer_id: hint.reviewer_id.clone(),
                    boost: hint.boost,
                })
                .collect(),
        };
        let body = serialize(&request)?;
        let bytes = self.request("route", &body, self.route_timeout)?;
        let parsed: RouteResponse = parse(&bytes)?;

        if parsed.reviewers.len() > MAX_ROUTE_REVIEWERS {
            return Err(self.malformed(format!(
                "{} reviewer scores exceed the bound of {MAX_ROUTE_REVIEWERS}",
                parsed.reviewers.len()
            )));
        }
        let mut scores = Vec::with_capacity(parsed.reviewers.len());
        for reviewer in parsed.reviewers {
            if reviewer.reviewer_id.is_empty() || reviewer.reviewer_id.len() > MAX_REVIEWER_ID_BYTES
            {
                return Err(self.malformed(
                    "a reviewer id is empty or longer than {MAX_REVIEWER_ID_BYTES} bytes"
                        .to_string(),
                ));
            }
            if !reviewer.score.is_finite() || !(0.0..=1.0).contains(&reviewer.score) {
                return Err(self.malformed(format!(
                    "score for `{}` is outside [0.0, 1.0]",
                    reviewer.reviewer_id
                )));
            }
            scores.push(ProviderScore {
                reviewer_id: reviewer.reviewer_id,
                score: reviewer.score,
            });
        }
        Ok(scores)
    }

    /// Embed `texts`: exactly one uniform-dimension vector per text,
    /// identified by the model that produced them.
    pub fn embed(&self, texts: &[&str]) -> Result<ProviderEmbeddings, ProviderError> {
        if texts.is_empty() {
            return Err(self.malformed("no texts to embed".to_string()));
        }
        if texts.len() > MAX_EMBED_TEXTS {
            return Err(self.malformed(format!(
                "{} texts exceed the bound of {MAX_EMBED_TEXTS}",
                texts.len()
            )));
        }
        let request = EmbedRequest {
            texts: texts
                .iter()
                .map(|text| {
                    crate::retrieval::query::truncate_text(text, MAX_EMBED_TEXT_BYTES).to_owned()
                })
                .collect(),
        };
        let body = serialize(&request)?;
        let bytes = self.request("embed", &body, self.embed_timeout)?;
        let parsed: EmbedResponse = parse(&bytes)?;

        if parsed.model.is_empty() || parsed.model.len() > MAX_EMBED_MODEL_BYTES {
            return Err(self.malformed("the model id is empty or oversized".to_string()));
        }
        let dim = match parsed.embeddings.first() {
            None => return Err(self.malformed("no embeddings returned".to_string())),
            Some(first) => {
                if first.is_empty() || first.len() > MAX_EMBED_DIM {
                    return Err(self.malformed(format!(
                        "embedding dimension {} is outside 1..={MAX_EMBED_DIM}",
                        first.len()
                    )));
                }
                first.len()
            }
        };
        if parsed.embeddings.len() != texts.len() {
            return Err(self.malformed(format!(
                "{} embeddings for {} texts",
                parsed.embeddings.len(),
                texts.len()
            )));
        }
        for (index, vector) in parsed.embeddings.iter().enumerate() {
            if vector.len() != dim {
                return Err(self.malformed(format!(
                    "embedding {index} has dimension {} but {dim} was expected",
                    vector.len()
                )));
            }
            if vector.iter().any(|value| !value.is_finite()) {
                return Err(
                    self.malformed(format!("embedding {index} contains a non-finite value"))
                );
            }
        }
        Ok(ProviderEmbeddings {
            model: parsed.model,
            vectors: parsed.embeddings,
        })
    }

    /// Decide endpoint trust and POST one request.
    fn request(&self, path: &str, body: &str, timeout: Duration) -> Result<Vec<u8>, ProviderError> {
        check_endpoint_trust(
            &self.endpoint,
            self.allow_non_private_endpoints,
            self.resolver.as_ref(),
        )
        .map_err(|err| ProviderError::UntrustedEndpoint {
            endpoint: self.endpoint.clone(),
            reason: err.to_string(),
        })?;
        let url = format!("{}/{}", self.endpoint.trim_end_matches('/'), path);
        let timeout_ms = timeout.as_millis().min(u64::MAX as u128) as u64;
        self.transport
            .post(&url, body.as_bytes(), timeout)
            .map_err(|failure| match failure {
                TransportFailure::Timeout => ProviderError::Timeout {
                    provider: PROVIDER_NAME.to_string(),
                    after_ms: timeout_ms,
                },
                TransportFailure::Other(message) => ProviderError::Transport {
                    provider: PROVIDER_NAME.to_string(),
                    message,
                },
            })
    }

    /// A malformed-output error attributed to this provider.
    fn malformed(&self, message: String) -> ProviderError {
        ProviderError::Malformed {
            provider: PROVIDER_NAME.to_string(),
            message,
        }
    }
}

impl RoutingClassifierProvider for NeedleAdapter {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn route(&self, input: &RoutingInput) -> Result<Vec<ProviderScore>, ProviderError> {
        self.route(input)
    }
}

impl EmbeddingProvider for NeedleAdapter {
    fn name(&self) -> &'static str {
        PROVIDER_NAME
    }

    fn embed(&self, texts: &[&str]) -> Result<ProviderEmbeddings, ProviderError> {
        self.embed(texts)
    }
}

/// Take at most [`MAX_INPUT_LIST`] entries of a classifier-input list.
fn bound_list(list: &[String]) -> &[String] {
    let take = list.len().min(MAX_INPUT_LIST);
    &list[..take]
}

/// Serialize one request body (infallible for these types).
fn serialize(value: &impl Serialize) -> Result<String, ProviderError> {
    serde_json::to_string(value).map_err(|err| ProviderError::Malformed {
        provider: PROVIDER_NAME.to_string(),
        message: format!("request serialization failed: {err}"),
    })
}

/// Parse one strict response body.
fn parse<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProviderError> {
    serde_json::from_slice(bytes).map_err(|err| ProviderError::Malformed {
        provider: PROVIDER_NAME.to_string(),
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Mutex;

    use crate::domain::records::RoutingHint;

    /// Scripted transport: queued responses or failures, plus the
    /// recorded requests.
    #[derive(Default)]
    struct FakeTransport {
        queued: Mutex<VecDeque<Result<Vec<u8>, TransportFailure>>>,
        requests: Mutex<Vec<(String, Vec<u8>)>>,
    }

    impl FakeTransport {
        fn push_json(&self, body: &str) {
            self.queued
                .lock()
                .unwrap()
                .push_back(Ok(body.as_bytes().to_vec()));
        }

        fn push_failure(&self, failure: TransportFailure) {
            self.queued.lock().unwrap().push_back(Err(failure));
        }

        fn requests(&self) -> Vec<(String, Vec<u8>)> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl NeedleTransport for FakeTransport {
        fn post(
            &self,
            url: &str,
            body: &[u8],
            _timeout: Duration,
        ) -> Result<Vec<u8>, TransportFailure> {
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), body.to_vec()));
            self.queued
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(TransportFailure::Other("no scripted response".into())))
        }
    }

    /// A resolver answering with one fixed loopback address.
    struct LoopbackResolver;

    impl HostResolver for LoopbackResolver {
        fn resolve(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
            Ok(vec![IpAddr::V4(Ipv4Addr::LOCALHOST)])
        }
    }

    /// Wrapper implementing [`NeedleTransport`] over a shared fake so
    /// assertions can inspect the recorded requests afterwards.
    struct FakeHandle(std::sync::Arc<FakeTransport>);

    impl NeedleTransport for FakeHandle {
        fn post(
            &self,
            url: &str,
            body: &[u8],
            timeout: Duration,
        ) -> Result<Vec<u8>, TransportFailure> {
            self.0.post(url, body, timeout)
        }
    }

    /// Build an adapter over a shared scripted transport with small
    /// timeouts and the loopback-only test resolver.
    fn adapter_with(endpoint: &str, fake: std::sync::Arc<FakeTransport>) -> NeedleAdapter {
        NeedleAdapter::new(endpoint, false)
            .with_timeouts(Duration::from_millis(50), Duration::from_millis(50))
            .with_resolver(Arc::new(LoopbackResolver))
            .with_transport(Box::new(FakeHandle(fake)))
    }

    fn sample_input() -> RoutingInput {
        RoutingInput {
            descriptors: vec!["Fix the race on the shared queue".to_string()],
            changed_paths: vec!["src/queue.rs".to_string()],
            languages: vec!["rust".to_string()],
            changed_symbols: vec!["enqueue".to_string()],
            deterministic_tags: vec!["concurrency".to_string()],
            lessons: vec![RoutingHint {
                reviewer_id: "concurrency".to_string(),
                path_globs: vec!["src/queue/**".to_string()],
                required_tags_any: Vec::new(),
                boost: 0.25,
            }],
        }
    }

    #[test]
    fn route_sends_bounded_input_and_parses_scores() {
        let fake = std::sync::Arc::new(FakeTransport::default());
        fake.push_json(r#"{"reviewers":[{"reviewer_id":"concurrency","score":0.9},{"reviewer_id":"testing","score":0.4}]}"#);
        let adapter = adapter_with("http://127.0.0.1:8787", fake.clone());

        let scores = adapter.route(&sample_input()).unwrap();
        assert_eq!(
            scores,
            vec![
                ProviderScore {
                    reviewer_id: "concurrency".to_string(),
                    score: 0.9
                },
                ProviderScore {
                    reviewer_id: "testing".to_string(),
                    score: 0.4
                },
            ]
        );

        let (url, body) = &fake.requests()[0];
        assert_eq!(url, "http://127.0.0.1:8787/route");
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(body["descriptors"][0], "Fix the race on the shared queue");
        assert_eq!(body["languages"], serde_json::json!(["rust"]));
        assert_eq!(body["lessons"][0]["reviewer_id"], "concurrency");
        assert_eq!(body["lessons"][0]["boost"], 0.25);
        // Lesson text never crosses the boundary, only deterministic
        // hints (there is no `text` field at all).
        assert_eq!(body.get("text"), None);

        // Lists are bounded defensively.
        let mut wide = sample_input();
        wide.descriptors = (0..200).map(|i| format!("descriptor {i}")).collect();
        fake.push_json(r#"{"reviewers":[]}"#);
        adapter.route(&wide).unwrap();
        let (_, body) = &fake.requests()[1];
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(
            body["descriptors"].as_array().unwrap().len(),
            MAX_INPUT_LIST
        );
    }

    #[test]
    fn embed_parses_uniform_vectors_and_bounds_texts() {
        let fake = std::sync::Arc::new(FakeTransport::default());
        fake.push_json(r#"{"model":"mini","embeddings":[[1.0,0.0],[0.0,1.0]]}"#);
        let adapter = adapter_with("http://127.0.0.1:8787", fake.clone());

        let embeddings = adapter.embed(&["alpha", "beta"]).unwrap();
        assert_eq!(embeddings.model, "mini");
        assert_eq!(embeddings.vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);

        // Long texts are truncated to the request bound.
        let long = "x".repeat(MAX_EMBED_TEXT_BYTES + 100);
        fake.push_json(r#"{"model":"mini","embeddings":[[1.0],[2.0]]}"#);
        adapter.embed(&["a", &long]).unwrap();
        let (_, body) = &fake.requests()[1];
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        let texts = body["texts"].as_array().unwrap();
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[1].as_str().unwrap().len(), MAX_EMBED_TEXT_BYTES);

        // Too many texts is a client-side bound violation.
        let many: Vec<&str> = (0..MAX_EMBED_TEXTS + 1).map(|_| "x").collect();
        assert!(matches!(
            adapter.embed(&many),
            Err(ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn timeout_is_mapped_to_a_typed_provider_error() {
        let fake = std::sync::Arc::new(FakeTransport::default());
        fake.push_failure(TransportFailure::Timeout);
        let adapter = NeedleAdapter::new("http://127.0.0.1:8787", false)
            .with_timeouts(Duration::from_millis(1_234), Duration::from_millis(1_234))
            .with_resolver(Arc::new(LoopbackResolver))
            .with_transport(Box::new(FakeHandle(fake)));

        let err = adapter.route(&sample_input()).unwrap_err();
        match err {
            ProviderError::Timeout { provider, after_ms } => {
                assert_eq!(provider, "needle");
                assert_eq!(after_ms, 1_234);
            }
            other => panic!("expected timeout, got {other:?}"),
        }
    }

    #[test]
    fn crash_is_mapped_to_a_transport_error() {
        let fake = std::sync::Arc::new(FakeTransport::default());
        fake.push_failure(TransportFailure::Other("connection refused".to_string()));
        let adapter = adapter_with("http://127.0.0.1:8787", fake);

        let err = adapter.route(&sample_input()).unwrap_err();
        assert!(
            matches!(err, ProviderError::Transport { ref message, .. } if message.contains("connection refused")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn malformed_route_output_rejects_the_whole_response() {
        let cases: &[&str] = &[
            "not json",
            "{}",
            r#"{"reviewers":[]}"#, // valid-but-empty is OK; covered below
            r#"{"reviewers":[{"reviewer_id":"x","score":0.5,"extra":1}]}"#,
            r#"{"reviewers":[{"reviewer_id":"x"}]}"#,
            r#"{"reviewers":[{"reviewer_id":"","score":0.5}]}"#,
            r#"{"reviewers":[{"reviewer_id":"x","score":1.5}]}"#,
            r#"{"reviewers":[{"reviewer_id":"x","score":-0.1}]}"#,
            r#"{"scores":[{"reviewer_id":"x","score":0.5}]}"#,
        ];
        for body in cases {
            let fake = std::sync::Arc::new(FakeTransport::default());
            fake.push_json(body);
            let adapter = adapter_with("http://127.0.0.1:8787", fake.clone());
            let result = adapter.route(&sample_input());
            // The empty reviewer list is a valid no-op response; every
            // other case must be rejected as malformed.
            if *body == r#"{"reviewers":[]}"# {
                assert!(result.unwrap().is_empty(), "{body}");
            } else {
                assert!(
                    matches!(result, Err(ProviderError::Malformed { .. })),
                    "expected malformed for {body}, got {:?}",
                    result
                );
            }
        }

        // Too many scores.
        let many = format!(
            r#"{{"reviewers":[{}]}}"#,
            (0..MAX_ROUTE_REVIEWERS + 1)
                .map(|i| format!(r#"{{"reviewer_id":"r{i}","score":0.1}}"#))
                .collect::<Vec<_>>()
                .join(",")
        );
        let fake = std::sync::Arc::new(FakeTransport::default());
        fake.push_json(&many);
        let adapter = adapter_with("http://127.0.0.1:8787", fake);
        assert!(matches!(
            adapter.route(&sample_input()),
            Err(ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn malformed_embed_output_rejects_the_whole_response() {
        let cases: &[&str] = &[
            "not json",
            r#"{"model":"mini"}"#,
            r#"{"model":"mini","embeddings":[]}"#,
            r#"{"model":"","embeddings":[[1.0]]}"#,
            r#"{"model":"mini","embeddings":[[1.0],[1.0,2.0]]}"#, // ragged
            r#"{"model":"mini","embeddings":[[1.0],[2.0,3.0]]}"#, // count/dim mix
            r#"{"model":"mini","embeddings":[[1.0],[1.0,2.0,3.0]]}"#,
            r#"{"model":"mini","embeddings":[[],[1.0]]}"#, // zero dim
            r#"{"model":"mini","vectors":[[1.0],[1.0]]}"#,
        ];
        for body in cases {
            let fake = std::sync::Arc::new(FakeTransport::default());
            fake.push_json(body);
            let adapter = adapter_with("http://127.0.0.1:8787", fake.clone());
            let result = adapter.embed(&["a", "b"]);
            // Vector-count mismatches for these two texts are checked
            // after parsing; every case must fail as malformed.
            assert!(
                matches!(result, Err(ProviderError::Malformed { .. })),
                "expected malformed for {body}, got {:?}",
                result.map(|e| e.vectors)
            );
        }
    }

    #[test]
    fn untrusted_endpoints_never_reach_the_transport() {
        for endpoint in [
            "http://8.8.8.8:8080",
            "http://192.0.2.10:1",
            "http://[2001:db8::1]:1",
        ] {
            let fake = std::sync::Arc::new(FakeTransport::default());
            let adapter = NeedleAdapter::new(endpoint, false)
                .with_timeouts(Duration::from_millis(50), Duration::from_millis(50))
                .with_resolver(Arc::new(LoopbackResolver))
                .with_transport(Box::new(FakeHandle(fake.clone())));
            let err = adapter.route(&sample_input()).unwrap_err();
            assert!(
                matches!(err, ProviderError::UntrustedEndpoint { .. }),
                "expected `{endpoint}` to be untrusted, got {err:?}"
            );
            // No request may leave for a rejected endpoint.
            assert!(
                fake.requests().is_empty(),
                "no request may leave for `{endpoint}`"
            );
        }
    }
    #[test]
    fn hostnames_are_resolved_before_trust_and_override_permits() {
        // A name resolving to a public address is refused before the
        // transport is touched.
        struct PublicResolver;
        impl HostResolver for PublicResolver {
            fn resolve(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
                Ok(vec![IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))])
            }
        }
        let fake = std::sync::Arc::new(FakeTransport::default());
        let adapter = NeedleAdapter::new("http://needle.internal:8787", false)
            .with_resolver(Arc::new(PublicResolver))
            .with_transport(Box::new(FakeHandle(fake.clone())));
        assert!(matches!(
            adapter.embed(&["x"]),
            Err(ProviderError::UntrustedEndpoint { .. })
        ));
        assert!(fake.requests().is_empty());

        // The explicit trusted-local-configuration override permits the
        // same endpoint and reaches the transport.
        let fake = std::sync::Arc::new(FakeTransport::default());
        fake.push_json(r#"{"model":"mini","embeddings":[[0.5]]}"#);
        let adapter = NeedleAdapter::new("http://needle.internal:8787", true)
            .with_resolver(Arc::new(PublicResolver))
            .with_transport(Box::new(FakeHandle(fake.clone())));
        assert!(adapter.embed(&["x"]).is_ok());
        assert_eq!(fake.requests()[0].0, "http://needle.internal:8787/embed");
    }

    #[test]
    fn endpoint_paths_and_trailing_slashes_join_cleanly() {
        let fake = std::sync::Arc::new(FakeTransport::default());
        fake.push_json(r#"{"reviewers":[]}"#);
        let adapter = adapter_with("http://10.0.0.5:8787/base/", fake.clone());
        adapter.route(&sample_input()).unwrap();
        assert_eq!(fake.requests()[0].0, "http://10.0.0.5:8787/base/route");
    }
}
