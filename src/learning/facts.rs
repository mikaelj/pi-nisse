//! Fact candidates and verification (plan sections 46 and 47).
//!
//! Reviewers may *propose* project facts
//! ([`CandidateProjectFact`](crate::domain::report::CandidateProjectFact)),
//! but reviewer output can never create durable knowledge directly.
//! Every candidate passes the frozen three-step pipeline before anything
//! is persisted:
//!
//! 1. **Deterministic validation** ([`validate_candidate`], plan
//!    section 47 step 1): confidence ≥ [`MIN_FACT_CONFIDENCE`], schema
//!    and text bounds, evidence validated against the exact target
//!    snapshot (reusing the finding-evidence validation of plan
//!    section 37), at least one *source* evidence entry grounded in
//!    project content, and obvious-secret filtering.
//! 2. **Independent verification** (step 2): one fresh Pi verifier
//!    session — always a separate session from the reviewer that
//!    proposed the candidate — judges the exact claim against the exact
//!    evidence excerpts and must answer `supported`, `contradicted`, or
//!    `insufficient` (plan section 21
//!    `start_fact_verification`/`fact_verification_completed`).
//! 3. **Persistence** (step 3): only `supported` plus every
//!    deterministic check plus a valid contributor identity creates a
//!    durable supported [`FactRecord`] under `.nisse/people/<id>/`.
//!    `contradicted` and near-duplicate candidates are rejected;
//!    everything else — including verifier failures and missing
//!    identity (plan section 44) — remains pending local state in the
//!    regenerable `pending_learning` cache, never durable project
//!    knowledge.
//!
//! All failure behavior is conservative: uncertainty produces pending
//! local state, never guessed truth.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::domain::SCHEMA_VERSION;
use crate::domain::evidence::{EvidenceKind, EvidenceRef};
use crate::domain::facts::{FactVerificationResult, FactVerificationVerdict};
use crate::domain::ids::new_ulid;
use crate::domain::model::ThinkingLevel;
use crate::domain::records::{
    FactRecord, FactState, MAX_SUBJECT_BYTES, MAX_TAG_BYTES, MAX_TAGS, MAX_TEXT_BYTES,
    ProjectRecord, is_valid_contributor_id,
};
use crate::domain::report::CandidateProjectFact;
use crate::domain::time::now_utc_rfc3339;
use crate::git::RepoContext;
use crate::jury::findings::{ValidationContext, validate_evidence};
use crate::jury::host::HostProcess;
use crate::jury::protocol::FactVerificationPayload;
use crate::storage::StoragePaths;
use crate::storage::db::open_or_recover;
use crate::storage::records::{RecordStore, StoredRecord};
use crate::storage::schema::DbKind;

/// Minimum reviewer confidence required before a candidate is even
/// verified (plan section 47 step 1: `confidence >= 0.90`).
pub const MIN_FACT_CONFIDENCE: f64 = 0.90;

/// Maximum evidence entries one candidate may carry.
pub const MAX_FACT_EVIDENCE: usize = 16;

/// Maximum candidates processed per review; candidates beyond the cap
/// remain pending (they were not processed, so nothing may be guessed
/// about them).
pub const MAX_CANDIDATES_PER_REVIEW: usize = 8;

/// Timeout of one verification session.
pub const FACT_VERIFICATION_TIMEOUT_MS: u64 = 120_000;

/// Slack added to the session timeout for driving one job to
/// completion (mirrors the other narrow-job exchanges).
const VERIFICATION_DRIVER_SLACK_MS: u64 = 10_000;

/// `pending_learning` row kind of fact candidates.
pub const PENDING_KIND: &str = "fact_candidate";

/// Bounds and model selection of the fact-candidate pipeline.
#[derive(Debug, Clone)]
pub struct FactProcessingOptions {
    /// Explicitly configured model profile for the verifier sessions.
    /// Verifier sessions never inherit a model.
    pub model_profile: String,
    /// Explicit thinking level for the verifier sessions.
    pub thinking_level: ThinkingLevel,
    /// Per-candidate verification session timeout (milliseconds).
    pub verification_timeout_ms: u64,
    /// Maximum candidates processed per review.
    pub max_candidates: usize,
}

impl Default for FactProcessingOptions {
    fn default() -> Self {
        FactProcessingOptions {
            model_profile: "standard".to_string(),
            thinking_level: ThinkingLevel::Medium,
            verification_timeout_ms: FACT_VERIFICATION_TIMEOUT_MS,
            max_candidates: MAX_CANDIDATES_PER_REVIEW,
        }
    }
}

/// Credential-assignment key names of the obvious-secret scan.
const SECRET_KEY_NAMES: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "secret_key",
    "secretkey",
    "token",
    "api_key",
    "apikey",
    "api_token",
    "access_token",
    "access_key",
    "accesskey",
    "private_key",
    "client_secret",
];

/// Token prefixes that carry a long opaque body.
const SECRET_TOKEN_PREFIXES: &[&str] =
    &["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "sk-"];

/// Minimum byte length of an assigned secret value.
const MIN_SECRET_VALUE_BYTES: usize = 8;

/// Minimum byte length of the body after a secret token prefix.
const MIN_SECRET_TOKEN_BODY_BYTES: usize = 20;

/// Scan text for obvious secrets (plan section 47 step 1: "obvious
/// secret filtering"). Deterministic and conservative: it detects PEM
/// private keys, AWS-style access keys, well-known token prefixes with
/// a long body, and literal `key = "value"` / `key: 'value'`
/// assignments of credential-like names. Returns the kind of secret
/// found.
pub fn find_obvious_secret(text: &str) -> Option<&'static str> {
    let upper = text.to_ascii_uppercase();
    if upper.contains("-----BEGIN") && upper.contains("PRIVATE KEY-----") {
        return Some("PEM private key");
    }

    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(found) = bytes[index..]
        .windows(4)
        .position(|window| window == b"AKIA")
    {
        let start = index + found;
        if bytes.len() >= start + 20
            && bytes[start + 4..start + 20]
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            return Some("AWS access key");
        }
        index = start + 4;
    }

    for prefix in SECRET_TOKEN_PREFIXES {
        let prefix_bytes = prefix.as_bytes();
        let mut from = 0;
        while let Some(found) = bytes[from..]
            .windows(prefix_bytes.len())
            .position(|window| {
                window.len() == prefix_bytes.len() && window.eq_ignore_ascii_case(prefix_bytes)
            })
        {
            let start = from + found;
            let body_end = bytes[start + prefix_bytes.len()..]
                .iter()
                .position(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
                .map(|offset| start + prefix_bytes.len() + offset)
                .unwrap_or(bytes.len());
            if body_end - (start + prefix_bytes.len()) >= MIN_SECRET_TOKEN_BODY_BYTES {
                return Some("prefixed credential token");
            }
            from = start + prefix_bytes.len();
        }
    }

    // Literal credential assignments: `<key> = "value"` / `<key>: 'value'`.
    let lower = text.to_ascii_lowercase();
    let lower_bytes = lower.as_bytes();
    for key in SECRET_KEY_NAMES {
        let key_bytes = key.as_bytes();
        let mut from = 0;
        while let Some(found) = lower_bytes[from..]
            .windows(key_bytes.len())
            .position(|window| window == key_bytes)
        {
            let start = from + found;
            let after_key = start + key_bytes.len();
            // The key must not run into a longer word.
            let boundary_ok = lower_bytes
                .get(after_key)
                .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_');
            if boundary_ok {
                let mut cursor = after_key;
                while lower_bytes
                    .get(cursor)
                    .is_some_and(|byte| byte.is_ascii_whitespace())
                {
                    cursor += 1;
                }
                if matches!(lower_bytes.get(cursor), Some(b':') | Some(b'=')) {
                    cursor += 1;
                    while lower_bytes
                        .get(cursor)
                        .is_some_and(|byte| byte.is_ascii_whitespace())
                    {
                        cursor += 1;
                    }
                    if matches!(lower_bytes.get(cursor), Some(b'"') | Some(b'\'')) {
                        let quote = lower_bytes[cursor];
                        let value_start = cursor + 1;
                        if let Some(value_len) =
                            bytes[value_start..].iter().position(|byte| *byte == quote)
                            && value_len >= MIN_SECRET_VALUE_BYTES
                        {
                            return Some("credential assignment");
                        }
                    }
                }
            }
            from = after_key;
        }
    }
    None
}

/// Deterministic near-duplicate key of a fact candidate or fact
/// record: normalized (case- and whitespace-insensitive) subject and
/// claim. Two candidates or facts sharing a key are obvious duplicates
/// for fact persistence (plan section 47 step 1: "near-duplicate active
/// facts").
pub fn fact_duplicate_key(subject: &str, claim: &str) -> String {
    format!(
        "{}\n{}",
        normalize_key_text(subject),
        normalize_key_text(claim)
    )
}

/// Lowercase and collapse internal whitespace. The shared normalizer
/// of duplicate keys and consolidation grouping.
pub(crate) fn normalize_key_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// A candidate that passed every deterministic check (plan section 47
/// step 1). Evidence is the cleaned set: only entries that validated
/// against the exact target snapshot survive, and at least one of them
/// is grounded `source` evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedFactCandidate {
    /// What the fact is about.
    pub subject: String,
    /// The fact statement.
    pub claim: String,
    /// Topic tags.
    pub tags: Vec<String>,
    /// Validated evidence entries (at least one of kind `source`).
    pub evidence: Vec<EvidenceRef>,
    /// Reviewer confidence (≥ [`MIN_FACT_CONFIDENCE`]).
    pub confidence: f64,
}

/// Deterministic validation of one fact candidate (plan section 47
/// step 1). Invalid evidence entries are dropped with their reasons
/// retained (the finding-validation precedent); the candidate survives
/// only with at least one valid entry, of which at least one must be
/// `source` evidence grounded in the target snapshot — a durable
/// project fact must be anchored in project content, not in transient
/// review input.
pub fn validate_candidate(
    context: &ValidationContext<'_>,
    candidate: &CandidateProjectFact,
) -> Result<ValidatedFactCandidate, String> {
    if !candidate.confidence.is_finite() || !(0.0..=1.0).contains(&candidate.confidence) {
        return Err(format!(
            "confidence {} is outside 0.0..=1.0",
            candidate.confidence
        ));
    }
    if candidate.confidence < MIN_FACT_CONFIDENCE {
        return Err(format!(
            "confidence {} is below the required minimum of {MIN_FACT_CONFIDENCE}",
            candidate.confidence
        ));
    }
    if candidate.subject.trim().is_empty() {
        return Err("`subject` must not be empty".to_string());
    }
    if candidate.subject.len() > MAX_SUBJECT_BYTES {
        return Err(format!(
            "`subject` exceeds {MAX_SUBJECT_BYTES} bytes (got {})",
            candidate.subject.len()
        ));
    }
    if candidate.claim.trim().is_empty() {
        return Err("`claim` must not be empty".to_string());
    }
    if candidate.claim.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "`claim` exceeds {MAX_TEXT_BYTES} bytes (got {})",
            candidate.claim.len()
        ));
    }
    if candidate.tags.len() > MAX_TAGS {
        return Err(format!(
            "`tags` exceeds {MAX_TAGS} entries (got {})",
            candidate.tags.len()
        ));
    }
    for tag in &candidate.tags {
        if tag.trim().is_empty() {
            return Err("`tags` entries must not be empty".to_string());
        }
        if tag.len() > MAX_TAG_BYTES {
            return Err(format!("`tags` entry exceeds {MAX_TAG_BYTES} bytes"));
        }
    }
    if candidate.evidence.is_empty() {
        return Err("a fact candidate requires at least one evidence entry".to_string());
    }
    if candidate.evidence.len() > MAX_FACT_EVIDENCE {
        return Err(format!(
            "`evidence` exceeds {MAX_FACT_EVIDENCE} entries (got {})",
            candidate.evidence.len()
        ));
    }

    let mut valid_evidence = Vec::new();
    let mut dropped = Vec::new();
    for evidence in &candidate.evidence {
        match validate_evidence(context, evidence) {
            Ok(()) => valid_evidence.push(evidence.clone()),
            Err(reason) => dropped.push(reason),
        }
    }
    if valid_evidence.is_empty() {
        return Err(format!(
            "no valid evidence remains (rejected: {})",
            dropped.join("; ")
        ));
    }
    if !valid_evidence
        .iter()
        .any(|evidence| evidence.kind == EvidenceKind::Source)
    {
        return Err(
            "a durable fact requires at least one source evidence entry grounded in the \
             target snapshot"
                .to_string(),
        );
    }

    let mut surface = format!(
        "{}\n{}\n{}",
        candidate.subject,
        candidate.claim,
        candidate.tags.join("\n")
    );
    for evidence in &valid_evidence {
        surface.push('\n');
        surface.push_str(&evidence.excerpt);
    }
    if let Some(kind) = find_obvious_secret(&surface) {
        return Err(format!(
            "candidate text contains an obvious secret ({kind})"
        ));
    }

    Ok(ValidatedFactCandidate {
        subject: candidate.subject.trim().to_string(),
        claim: candidate.claim.trim().to_string(),
        tags: candidate.tags.clone(),
        evidence: valid_evidence,
        confidence: candidate.confidence,
    })
}

/// Near-duplicate keys of the active supported facts of a record set
/// (plan section 47 step 1). Superseded facts and unresolved conflicts
/// do not block new knowledge.
pub fn active_fact_keys(records: &[StoredRecord]) -> BTreeSet<String> {
    let superseded = crate::storage::records::superseded_ids(records);
    records
        .iter()
        .filter(|stored| !superseded.contains(stored.id()))
        .filter_map(|stored| match &stored.record {
            ProjectRecord::Fact(fact) if fact.state == FactState::Supported => {
                Some(fact_duplicate_key(&fact.subject, &fact.text))
            }
            _ => None,
        })
        .collect()
}

/// Whether a non-persisted candidate was rejected outright or remains
/// pending (plan section 47 step 3: "others remain pending/rejected").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CandidateDisposition {
    /// Retained as pending local state: verification was insufficient,
    /// the verifier failed, no contributor identity was available, or
    /// the candidate cap was reached.
    Pending,
    /// Definitively rejected: deterministic validation failed, an
    /// active supported fact already covers the claim, or independent
    /// verification contradicted it.
    Rejected,
}

impl CandidateDisposition {
    /// Frozen serialized name.
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateDisposition::Pending => "pending",
            CandidateDisposition::Rejected => "rejected",
        }
    }
}

/// One fact candidate retained in local pending-learning state (never
/// durable project knowledge; regenerable SQLite, plan sections 44 and
/// 57).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingFactCandidate {
    /// Payload schema version; must be `1`.
    pub schema_version: u32,
    /// The review that produced the candidate.
    pub review_id: String,
    /// The reviewer that proposed the candidate.
    pub reviewer_id: String,
    /// Whether the candidate is pending or rejected.
    pub disposition: CandidateDisposition,
    /// Why the candidate was not persisted as a durable fact.
    pub reason: String,
    /// The original proposal.
    pub candidate: CandidateProjectFact,
    /// When the outcome was recorded (UTC RFC3339).
    pub created_at: String,
}

/// The outcome of processing one fact candidate.
#[derive(Debug, Clone, PartialEq)]
pub enum CandidateOutcome {
    /// A durable supported fact was created (its record ID).
    Persisted(String),
    /// Retained as pending local state (why).
    Pending(String),
    /// Definitively rejected (why).
    Rejected(String),
}

/// Summary of the fact-candidate steps of one review (plan section 58
/// steps 19–20).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FactProcessingSummary {
    /// One outcome per processed candidate, in processing order.
    pub outcomes: Vec<(String, CandidateOutcome)>,
    /// Human-readable diagnostics (one line per candidate outcome and
    /// every storage failure).
    pub diagnostics: Vec<String>,
}

/// Run the fact-candidate pipeline over the completed reports of one
/// review (plan sections 47 and 58 steps 19–20).
///
/// Every failure degrades to pending local state: a verifier or
/// protocol failure never fails the review itself, and nothing is ever
/// persisted on uncertainty alone.
#[allow(clippy::too_many_arguments)]
pub fn process_fact_candidates(
    host: &mut HostProcess,
    ctx: &RepoContext,
    paths: &StoragePaths,
    validation: &ValidationContext<'_>,
    review_id: &str,
    reports: &[(String, crate::domain::report::ReviewerReport)],
    existing_records: &[StoredRecord],
    contributor_id: Option<&str>,
    options: &FactProcessingOptions,
) -> FactProcessingSummary {
    let store = RecordStore::new(&ctx.worktree_root);
    let mut known = active_fact_keys(existing_records);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut summary = FactProcessingSummary::default();
    let mut processed = 0usize;

    let author = contributor_id.filter(|id| is_valid_contributor_id(id));

    for (reviewer_id, report) in reports {
        for candidate in &report.candidate_project_facts {
            if processed >= options.max_candidates {
                retain(
                    paths,
                    &mut summary,
                    review_id,
                    reviewer_id,
                    CandidateDisposition::Pending,
                    &format!(
                        "the review reached its fact-candidate cap of {}",
                        options.max_candidates
                    ),
                    candidate,
                );
                continue;
            }
            processed += 1;

            // Step 1: deterministic validation.
            let validated = match validate_candidate(validation, candidate) {
                Ok(validated) => validated,
                Err(reason) => {
                    reject(
                        paths,
                        &mut summary,
                        review_id,
                        reviewer_id,
                        candidate,
                        &reason,
                    );
                    continue;
                }
            };

            // Near-duplicate check: against active supported facts and
            // against candidates already processed in this review.
            let key = fact_duplicate_key(&validated.subject, &validated.claim);
            if !seen.insert(key.clone()) {
                reject(
                    paths,
                    &mut summary,
                    review_id,
                    reviewer_id,
                    candidate,
                    "duplicate of an earlier candidate of this review",
                );
                continue;
            }
            if known.contains(&key) {
                reject(
                    paths,
                    &mut summary,
                    review_id,
                    reviewer_id,
                    candidate,
                    "near-duplicate of an active supported fact",
                );
                continue;
            }

            // Step 2: independent verification (a separate session from
            // the proposing reviewer).
            let payload = FactVerificationPayload {
                verification_id: new_ulid(),
                timeout_ms: options.verification_timeout_ms,
                model_profile: options.model_profile.clone(),
                thinking_level: options.thinking_level,
                claim: validated.claim.clone(),
                evidence: validated.evidence.clone(),
            };
            let verification = match host.start_fact_verification(&payload).and_then(|()| {
                host.complete_fact_verification(
                    &payload.verification_id,
                    Duration::from_millis(
                        options.verification_timeout_ms + VERIFICATION_DRIVER_SLACK_MS,
                    ),
                )
            }) {
                Ok(result) => result,
                Err(error) => FactVerificationResult {
                    verification_id: payload.verification_id.clone(),
                    verdict: FactVerificationVerdict::Insufficient,
                    failure: Some(error.to_string()),
                },
            };

            // Step 3: persistence.
            match verification.verdict {
                FactVerificationVerdict::Supported => match author {
                    Some(author) => {
                        let fact = FactRecord {
                            schema_version: SCHEMA_VERSION,
                            id: new_ulid(),
                            author: author.to_string(),
                            created_at: now_utc_rfc3339(),
                            subject: validated.subject.clone(),
                            text: validated.claim.clone(),
                            tags: validated.tags.clone(),
                            state: FactState::Supported,
                            // Code-derived facts retain content
                            // provenance (plan section 48): Rust stamps
                            // the snapshot content key of every source
                            // entry so later staleness detection can
                            // prove whether the support is still
                            // current.
                            evidence: crate::learning::staleness::stamp_content_provenance(
                                validation,
                                &validated.evidence,
                            ),
                            source_review_id: Some(review_id.to_string()),
                            source_record_ids: Vec::new(),
                            supersedes: Vec::new(),
                        };
                        match store.create(&ProjectRecord::Fact(fact)) {
                            Ok(stored) => {
                                known.insert(key);
                                summary.diagnostics.push(format!(
                                    "fact candidate from `{reviewer_id}` persisted as durable fact {}",
                                    stored.id()
                                ));
                                summary.outcomes.push((
                                    reviewer_id.clone(),
                                    CandidateOutcome::Persisted(stored.id().to_string()),
                                ));
                            }
                            Err(error) => {
                                retain(
                                    paths,
                                    &mut summary,
                                    review_id,
                                    reviewer_id,
                                    CandidateDisposition::Pending,
                                    &format!("durable fact creation failed: {error}"),
                                    candidate,
                                );
                            }
                        }
                    }
                    None => {
                        retain(
                            paths,
                            &mut summary,
                            review_id,
                            reviewer_id,
                            CandidateDisposition::Pending,
                            "no contributor identity available; the fact remains pending \
                             (plan section 44)",
                            candidate,
                        );
                    }
                },
                FactVerificationVerdict::Contradicted => {
                    reject(
                        paths,
                        &mut summary,
                        review_id,
                        reviewer_id,
                        candidate,
                        "independent verification contradicted the claim",
                    );
                }
                FactVerificationVerdict::Insufficient => {
                    let reason = match &verification.failure {
                        Some(failure) => format!(
                            "verification could not decide ({failure}); the fact remains pending"
                        ),
                        None => "verification found the evidence insufficient; the fact remains \
                             pending"
                            .to_string(),
                    };
                    retain(
                        paths,
                        &mut summary,
                        review_id,
                        reviewer_id,
                        CandidateDisposition::Pending,
                        &reason,
                        candidate,
                    );
                }
            }
        }
    }
    summary
}

/// Record a rejected candidate in pending local state plus a
/// diagnostic and outcome entry.
fn reject(
    paths: &StoragePaths,
    summary: &mut FactProcessingSummary,
    review_id: &str,
    reviewer_id: &str,
    candidate: &CandidateProjectFact,
    reason: &str,
) {
    summary.diagnostics.push(format!(
        "fact candidate from `{reviewer_id}` rejected: {reason}"
    ));
    summary.outcomes.push((
        reviewer_id.to_string(),
        CandidateOutcome::Rejected(reason.to_string()),
    ));
    retain(
        paths,
        summary,
        review_id,
        reviewer_id,
        CandidateDisposition::Rejected,
        reason,
        candidate,
    );
}

/// Store one non-persisted candidate in the local `pending_learning`
/// cache. Storage failures degrade to a diagnostic; they never fail
/// the review.
fn retain(
    paths: &StoragePaths,
    summary: &mut FactProcessingSummary,
    review_id: &str,
    reviewer_id: &str,
    disposition: CandidateDisposition,
    reason: &str,
    candidate: &CandidateProjectFact,
) {
    if disposition == CandidateDisposition::Pending {
        summary.diagnostics.push(format!(
            "fact candidate from `{reviewer_id}` remains pending: {reason}"
        ));
        summary.outcomes.push((
            reviewer_id.to_string(),
            CandidateOutcome::Pending(reason.to_string()),
        ));
    }
    let entry = PendingFactCandidate {
        schema_version: SCHEMA_VERSION,
        review_id: review_id.to_string(),
        reviewer_id: reviewer_id.to_string(),
        disposition,
        reason: reason.to_string(),
        candidate: candidate.clone(),
        created_at: now_utc_rfc3339(),
    };
    if let Err(error) = store_pending_candidate(paths, &entry) {
        summary.diagnostics.push(format!(
            "failed to retain the fact candidate of `{reviewer_id}` in pending state: {error}"
        ));
    }
}

/// Store one candidate outcome in the shared `pending_learning` cache.
pub fn store_pending_candidate(
    paths: &StoragePaths,
    entry: &PendingFactCandidate,
) -> Result<(), String> {
    let payload = serde_json::to_string(entry).map_err(|error| error.to_string())?;
    store_pending_row(paths, PENDING_KIND, &payload, &entry.created_at)
}

/// Store one raw pending-learning row in the shared cache. Shared by
/// every learning subsystem that defers knowledge to local state
/// (fact candidates, fact revalidations, lesson maintenance).
pub fn store_pending_row(
    paths: &StoragePaths,
    kind: &str,
    payload: &str,
    created_at: &str,
) -> Result<(), String> {
    let db = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|error| format!("{}: {error}", error))?;
    db.conn
        .execute(
            "INSERT INTO pending_learning (kind, payload, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![kind, payload, created_at],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// One row of local pending-learning state.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRow {
    /// Row identifier (insertion order).
    pub id: i64,
    /// Row kind (for example `fact_candidate`).
    pub kind: String,
    /// When the row was recorded (UTC RFC3339).
    pub created_at: String,
    /// The raw row payload.
    pub payload: serde_json::Value,
}

/// Load every pending-learning row of the shared cache, in insertion
/// order. A cache that does not exist yet is simply empty; the
/// listing never creates one.
pub fn load_pending(paths: &StoragePaths) -> Result<Vec<PendingRow>, String> {
    if !paths.shared.is_file() {
        return Ok(Vec::new());
    }
    let db = open_or_recover(&paths.shared, DbKind::Shared)
        .map_err(|error| format!("{}: {error}", error))?;
    let mut stmt = db
        .conn
        .prepare("SELECT id, kind, payload, created_at FROM pending_learning ORDER BY id")
        .map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(PendingRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                created_at: row.get(3)?,
                payload: serde_json::from_str(&row.get::<_, String>(2)?)
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .map_err(|error| error.to_string())?;
    let mut rows = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    // Consent-deferred observations live in the append-only sidecar
    // (outside the wipe-on-corruption cache); surface them here too.
    let sidecar = paths
        .shared
        .parent()
        .map(|dir| dir.join("deferred-observations.jsonl"))
        .unwrap_or_else(|| paths.shared.clone());
    if let Ok(text) = std::fs::read_to_string(&sidecar) {
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(line) {
                rows.push(PendingRow {
                    id: -(index as i64) - 1,
                    kind: crate::learning::feedback::DEFERRED_KIND.to_string(),
                    created_at: payload["observation"]["created_at"]
                        .as_str()
                        .unwrap_or("?")
                        .to_string(),
                    payload,
                });
            }
        }
    }
    Ok(rows)
}

/// Render the human-readable `nisse records pending` report.
pub fn render_pending(rows: &[PendingRow]) -> String {
    let mut out = String::from("nisse records pending\n");
    if rows.is_empty() {
        out.push_str("no pending learning state\n");
        return out;
    }
    for row in rows {
        let detail = match row.kind.as_str() {
            crate::learning::feedback::PENDING_KIND => format!(
                "pending review feedback ({}): review {} finding {} — {}",
                row.payload["outcome"].as_str().unwrap_or("?"),
                row.payload["review_id"].as_str().unwrap_or("?"),
                row.payload["finding_id"].as_str().unwrap_or("?"),
                row.payload["reason"].as_str().unwrap_or("?"),
            ),
            PENDING_KIND => format!(
                "{} fact candidate ({}): {} — {}",
                row.payload["disposition"].as_str().unwrap_or("?"),
                row.payload["reviewer_id"].as_str().unwrap_or("?"),
                row.payload["candidate"]["subject"].as_str().unwrap_or("?"),
                row.payload["reason"].as_str().unwrap_or("?"),
            ),
            crate::learning::feedback::DEFERRED_KIND => format!(
                "deferred observation ({}): review {} finding {} — {}",
                row.payload["observation"]["outcome"]
                    .as_str()
                    .unwrap_or("?"),
                row.payload["observation"]["review_id"]
                    .as_str()
                    .unwrap_or("?"),
                row.payload["observation"]["finding_id"]
                    .as_str()
                    .unwrap_or("?"),
                row.payload["reason"].as_str().unwrap_or("?"),
            ),
            crate::learning::lessons::PENDING_KIND => {
                let groups: Vec<String> = row.payload["groups"]
                    .as_array()
                    .map(|groups| {
                        groups
                            .iter()
                            .map(|group| {
                                format!(
                                    "{}/{}",
                                    group["reviewer_id"].as_str().unwrap_or("?"),
                                    group["category"].as_str().unwrap_or("?")
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                format!(
                    "pending lesson maintenance ({} group(s): {}): {}",
                    groups.len(),
                    groups.join(", "),
                    row.payload["reason"].as_str().unwrap_or("?"),
                )
            }
            crate::learning::staleness::PENDING_KIND => format!(
                "pending fact revalidation ({}): fact {} — {}",
                row.payload["verdict"].as_str().unwrap_or("?"),
                row.payload["fact_id"].as_str().unwrap_or("?"),
                row.payload["reason"].as_str().unwrap_or("?"),
            ),
            other => other.to_string(),
        };
        out.push_str(&format!("#{} {} {}\n", row.id, row.created_at, detail));
    }
    out.push_str(&format!("{} pending item(s)\n", rows.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::EvidenceKind;
    use crate::domain::records::testutil::fact as build_fact;
    use crate::git::ObjectFormat;
    use crate::git::resolve_snapshot;
    use crate::git::testutil::{Fixture, commit_all, write_file};
    use crate::git::{GitRunner, RepoContext, ResolvedSnapshot, SnapshotReader, SnapshotSpec};
    use crate::storage::records::RecordLocation;
    use std::collections::BTreeSet;

    fn candidate(confidence: f64, evidence: Vec<EvidenceRef>) -> CandidateProjectFact {
        CandidateProjectFact {
            subject: "queue capacity".to_string(),
            claim: "The sensor queue is bounded to 32 slots.".to_string(),
            tags: vec!["queue".to_string()],
            evidence,
            confidence,
        }
    }

    fn source_evidence(path: &str, excerpt: &str) -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some(path.to_string()),
            symbol: None,
            line_start: None,
            line_end: None,
            content_key: None,
            plan_section: None,
            record_id: None,
            excerpt: excerpt.to_string(),
        }
    }

    /// A fixture repository whose working snapshot contains `src/a.rs`
    /// (3 lines); values are leaked so the borrowed context outlives
    /// the setup call.
    struct Repo {
        _fixture: Fixture,
        ctx: &'static RepoContext,
        resolved: &'static ResolvedSnapshot,
    }

    fn setup(name: &str) -> Repo {
        let fixture = Fixture::repo(name);
        write_file(&fixture.root, "src/a.rs", "fn a() {\n    marker\n}\n");
        commit_all(&fixture.root, "one");
        let ctx: &'static RepoContext = Box::leak(Box::new(
            RepoContext::discover(&GitRunner::new(), &fixture.root).unwrap(),
        ));
        let resolved: &'static ResolvedSnapshot = Box::leak(Box::new(
            resolve_snapshot(ctx, &SnapshotSpec::Working).unwrap(),
        ));
        Repo {
            _fixture: fixture,
            ctx,
            resolved,
        }
    }

    impl Repo {
        fn context(&self) -> ValidationContext<'static> {
            let reader = Box::leak(Box::new(
                SnapshotReader::new(self.ctx, self.resolved).unwrap(),
            ));
            ValidationContext {
                reader,
                object_format: ObjectFormat::Sha1,
                changed_paths: None,
                known_record_ids: Box::leak(Box::new(BTreeSet::new())),
            }
        }

        fn paths(&self) -> StoragePaths {
            StoragePaths::for_repo(self.ctx)
        }
    }

    #[test]
    fn obvious_secret_scan_is_deterministic() {
        assert_eq!(
            find_obvious_secret(
                "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----"
            ),
            Some("PEM private key")
        );
        assert_eq!(
            find_obvious_secret("aws key AKIAIOSFODNN7EXAMPLE here"),
            Some("AWS access key")
        );
        // Too short: not a key.
        assert_eq!(find_obvious_secret("AKIA1234"), None);
        assert_eq!(
            find_obvious_secret("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ1234"),
            Some("prefixed credential token")
        );
        assert_eq!(
            find_obvious_secret("github_pat_11ABCDEFG0123456789012345678901234567890"),
            Some("prefixed credential token")
        );
        assert_eq!(
            find_obvious_secret("sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ12"),
            Some("prefixed credential token")
        );
        // Too short: not a token.
        assert_eq!(find_obvious_secret("ghp_short"), None);
        assert_eq!(
            find_obvious_secret(r#"password = "hunter2hunter2""#),
            Some("credential assignment")
        );
        assert_eq!(
            find_obvious_secret("API_TOKEN: 'abcdefghijklmnopqrstuvwxyz'"),
            Some("credential assignment")
        );
        // Word-boundary: `pwd` inside `pwdx` is not a key.
        assert_eq!(
            find_obvious_secret(r#"pwdx = "abcdefghijklmnopqrstuvwxyz""#),
            None
        );
        // Too short / empty values are not secrets.
        assert_eq!(find_obvious_secret(r#"password = "hunter2""#), None);
        assert_eq!(find_obvious_secret(r#"password = """#), None);
        // Case-insensitive keys, benign source text.
        assert_eq!(
            find_obvious_secret("const CLIENT_SECRET = \"0123456789abcdef\";"),
            Some("credential assignment")
        );
        assert_eq!(find_obvious_secret("The queue capacity is 32."), None);
        assert_eq!(
            find_obvious_secret("Use the token bucket algorithm for rate limiting."),
            None
        );
    }

    #[test]
    fn duplicate_keys_normalize_case_and_whitespace() {
        assert_eq!(
            fact_duplicate_key("Queue  capacity", "The  queue is bounded."),
            fact_duplicate_key("queue capacity", "the queue is bounded.")
        );
        assert_ne!(
            fact_duplicate_key("queue capacity", "the queue is bounded."),
            fact_duplicate_key("queue capacity", "the queue is unbounded.")
        );
    }

    #[test]
    fn validation_enforces_confidence_bounds() {
        let repo = setup("fact-confidence");
        let context = repo.context();

        for bad in [0.0, 0.5, 0.89, 0.899, -0.1, 1.2, f64::NAN] {
            let err = validate_candidate(
                &context,
                &candidate(bad, vec![source_evidence("src/a.rs", "fn a()")]),
            )
            .unwrap_err();
            assert!(err.contains("confidence"), "{bad}: {err}");
        }

        // Exactly the minimum passes; so does 1.0.
        for good in [MIN_FACT_CONFIDENCE, 1.0] {
            assert!(
                validate_candidate(
                    &context,
                    &candidate(good, vec![source_evidence("src/a.rs", "fn a()")])
                )
                .is_ok(),
                "{good} must validate"
            );
        }
    }

    #[test]
    fn validation_enforces_text_bounds() {
        let repo = setup("fact-bounds");
        let context = repo.context();
        let evidence = vec![source_evidence("src/a.rs", "fn a()")];

        let mut oversized_subject = candidate(0.95, evidence.clone());
        oversized_subject.subject = "s".repeat(MAX_SUBJECT_BYTES + 1);
        assert!(
            validate_candidate(&context, &oversized_subject)
                .unwrap_err()
                .contains("`subject` exceeds")
        );

        let mut oversized_claim = candidate(0.95, evidence.clone());
        oversized_claim.claim = "c".repeat(MAX_TEXT_BYTES + 1);
        assert!(
            validate_candidate(&context, &oversized_claim)
                .unwrap_err()
                .contains("`claim` exceeds")
        );

        let mut empty_subject = candidate(0.95, evidence.clone());
        empty_subject.subject = "   ".to_string();
        assert!(
            validate_candidate(&context, &empty_subject)
                .unwrap_err()
                .contains("`subject`")
        );

        let mut too_many_tags = candidate(0.95, evidence.clone());
        too_many_tags.tags = (0..MAX_TAGS + 1)
            .map(|index| format!("tag{index}"))
            .collect();
        assert!(
            validate_candidate(&context, &too_many_tags)
                .unwrap_err()
                .contains("`tags` exceeds")
        );

        let mut too_many_entries = candidate(0.95, Vec::new());
        too_many_entries.evidence = (0..MAX_FACT_EVIDENCE + 1)
            .map(|_| source_evidence("src/a.rs", "fn a()"))
            .collect();
        assert!(
            validate_candidate(&context, &too_many_entries)
                .unwrap_err()
                .contains("`evidence` exceeds")
        );
    }

    #[test]
    fn validation_requires_valid_snapshot_grounded_evidence() {
        let repo = setup("fact-evidence");
        let context = repo.context();

        // No evidence at all.
        assert!(
            validate_candidate(&context, &candidate(0.95, Vec::new()))
                .unwrap_err()
                .contains("requires at least one evidence entry")
        );

        // Evidence pointing outside the snapshot is dropped; nothing
        // valid remains.
        let missing = candidate(0.95, vec![source_evidence("src/missing.rs", "nothing")]);
        let err = validate_candidate(&context, &missing).unwrap_err();
        assert!(err.contains("no valid evidence remains"), "{err}");

        // Invalid entries are dropped but the candidate survives on its
        // valid entry (the finding-validation precedent).
        let mixed = candidate(
            0.95,
            vec![
                source_evidence("src/missing.rs", "nothing"),
                source_evidence("src/a.rs", "fn a()"),
            ],
        );
        let validated = validate_candidate(&context, &mixed).unwrap();
        assert_eq!(validated.evidence.len(), 1);
        assert_eq!(validated.evidence[0].path.as_deref(), Some("src/a.rs"));

        // Line ranges beyond the file (3 lines) drop the entry.
        let mut beyond = source_evidence("src/a.rs", "fn a()");
        beyond.line_start = Some(2);
        beyond.line_end = Some(9);
        assert!(
            validate_candidate(&context, &candidate(0.95, vec![beyond]))
                .unwrap_err()
                .contains("no valid evidence remains")
        );
    }

    #[test]
    fn validation_requires_source_evidence_for_durability() {
        let repo = setup("fact-grounding");
        let context = repo.context();

        // Plan evidence alone is not durable project grounding.
        let plan_only = candidate(
            0.95,
            vec![EvidenceRef {
                kind: EvidenceKind::Plan,
                path: None,
                symbol: None,
                line_start: None,
                line_end: None,
                content_key: None,
                plan_section: Some("Approach".to_string()),
                record_id: None,
                excerpt: "Move the queue behind a mutex.".to_string(),
            }],
        );
        let err = validate_candidate(&context, &plan_only).unwrap_err();
        assert!(err.contains("source evidence entry"), "{err}");
    }

    #[test]
    fn validation_rejects_secrets_everywhere_in_the_candidate() {
        let repo = setup("fact-secrets");
        let context = repo.context();

        let mut with_key = candidate(0.95, vec![source_evidence("src/a.rs", "fn a()")]);
        with_key.claim = "The deploy uses key AKIAIOSFODNN7EXAMPLE.".to_string();
        assert!(
            validate_candidate(&context, &with_key)
                .unwrap_err()
                .contains("obvious secret")
        );

        let in_excerpt = candidate(
            0.95,
            vec![source_evidence(
                "src/a.rs",
                "password = \"supersecretvalue1\"",
            )],
        );
        assert!(
            validate_candidate(&context, &in_excerpt)
                .unwrap_err()
                .contains("obvious secret")
        );

        let mut in_subject = candidate(0.95, vec![source_evidence("src/a.rs", "fn a()")]);
        in_subject.subject = "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ12".to_string();
        assert!(
            validate_candidate(&context, &in_subject)
                .unwrap_err()
                .contains("obvious secret")
        );
    }

    #[test]
    fn active_fact_keys_cover_only_active_supported_facts() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::new(dir.path());
        let records = store.load().unwrap();
        assert!(active_fact_keys(&records).is_empty());

        let original = store
            .create(&ProjectRecord::Fact(build_fact("alice")))
            .unwrap();
        let mut superseding = build_fact("alice");
        superseding.subject = "queue discipline".to_string();
        // Same normalized claim text as the original.
        superseding.text = "The sensor queue is single-producer single-consumer.".to_string();
        superseding.supersedes = vec![original.id().to_string()];
        store.create(&ProjectRecord::Fact(superseding)).unwrap();

        let records = store.load().unwrap();
        let keys = active_fact_keys(&records);
        assert_eq!(keys.len(), 1, "the superseded fact must not block");
        assert!(keys.contains(&fact_duplicate_key(
            "queue discipline",
            "The sensor queue is single-producer single-consumer."
        )));

        // A conflicting fact state does not act as duplicate truth.
        let mut conflicting = build_fact("alice");
        conflicting.state = FactState::UnresolvedConflict;
        store.create(&ProjectRecord::Fact(conflicting)).unwrap();
        let records = store.load().unwrap();
        assert_eq!(active_fact_keys(&records).len(), 1);
    }

    #[test]
    fn pending_candidates_roundtrip_and_render() {
        let repo = setup("fact-pending-store");
        let paths = repo.paths();
        assert!(load_pending(&paths).unwrap().is_empty());

        let entry = PendingFactCandidate {
            schema_version: SCHEMA_VERSION,
            review_id: new_ulid(),
            reviewer_id: "quality".to_string(),
            disposition: CandidateDisposition::Pending,
            reason: "verification could not decide (session_timeout: timed out)".to_string(),
            candidate: candidate(0.95, vec![source_evidence("src/a.rs", "fn a()")]),
            created_at: "2026-02-01T12:00:00.000Z".to_string(),
        };
        store_pending_candidate(&paths, &entry).unwrap();

        let rows = load_pending(&paths).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, PENDING_KIND);
        assert_eq!(rows[0].created_at, entry.created_at);
        let parsed: PendingFactCandidate = serde_json::from_value(rows[0].payload.clone()).unwrap();
        assert_eq!(parsed, entry);

        let text = render_pending(&rows);
        assert!(text.starts_with("nisse records pending\n"));
        assert!(
            text.contains("pending fact candidate (quality): queue capacity"),
            "{text}"
        );
        assert!(text.contains("verification could not decide"), "{text}");
        assert!(text.ends_with("1 pending item(s)\n"));

        // Empty state renders without panic.
        let empty = render_pending(&[]);
        assert!(empty.contains("no pending learning state"));

        // Deferred lesson maintenance renders its groups.
        let lesson_row = PendingRow {
            id: 9,
            kind: crate::learning::lessons::PENDING_KIND.to_string(),
            created_at: "2026-03-01T12:00:00.000Z".to_string(),
            payload: serde_json::json!({
                "groups": [
                    {"reviewer_id": "concurrency", "category": "race", "observation_ids": []},
                    {"reviewer_id": "database", "category": "migration", "observation_ids": []},
                ],
                "reason": "no contributor identity available (plan section 44)",
            }),
        };
        let text = render_pending(&[lesson_row]);
        assert!(
            text.contains(
                "pending lesson maintenance (2 group(s): concurrency/race, database/migration)"
            ),
            "{text}"
        );
        assert!(text.contains("no contributor identity"), "{text}");

        // Deferred fact revalidations render their verdict and reason.
        let revalidation_row = PendingRow {
            id: 10,
            kind: crate::learning::staleness::PENDING_KIND.to_string(),
            created_at: "2026-04-01T12:00:00.000Z".to_string(),
            payload: serde_json::json!({
                "schema_version": 1,
                "fact_id": "01FACTIDFACTIDFACTIDFACTID",
                "verdict": "insufficient",
                "reason": "revalidation found the current evidence insufficient",
            }),
        };
        let text = render_pending(&[revalidation_row]);
        assert!(
            text.contains(
                "pending fact revalidation (insufficient): fact 01FACTIDFACTIDFACTIDFACTID"
            ),
            "{text}"
        );
        assert!(
            text.contains("revalidation found the current evidence insufficient"),
            "{text}"
        );

        // Unknown future kinds render generically.
        let foreign = PendingRow {
            id: 7,
            kind: "something_else".to_string(),
            created_at: "2026-02-01T12:00:00.000Z".to_string(),
            payload: serde_json::Value::Null,
        };
        assert!(render_pending(&[foreign]).contains("something_else"));
    }

    /// `RecordStore` creation paths used by the pipeline are the frozen
    /// durable-record paths (a shallow wiring check; the full pipeline
    /// is covered by the binary integration tests).
    #[test]
    fn durable_fact_records_use_the_people_location() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::new(dir.path());
        let created = store
            .create(&ProjectRecord::Fact(build_fact("alice")))
            .unwrap();
        assert_eq!(
            created.location,
            RecordLocation::People {
                contributor: "alice".to_string()
            }
        );
        assert!(created.path.ends_with(format!("{}.json", created.id())));
    }
}
