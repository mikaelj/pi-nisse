//! Durable project record schemas (plan sections 39–45).
//!
//! Exactly three record kinds exist: `observation`, `fact`, and `lesson`.
//! Records are immutable after creation, carry provenance (`author`,
//! `created_at`), are identified by ULIDs, and are serialized as strict
//! JSON: unknown fields and unknown enum variants are rejected. The
//! filesystem store for these records lives in
//! [`crate::storage::records`]; this module defines the frozen data
//! shapes and their deterministic validation.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::SCHEMA_VERSION;
use super::evidence::EvidenceRef;
use super::ids::is_valid_ulid;
use super::review::ImpactClass;
use super::snapshot::SnapshotDescriptor;
use super::time::is_valid_utc_rfc3339;

/// Maximum length of a fact/lesson subject, in bytes.
pub const MAX_SUBJECT_BYTES: usize = 256;
/// Maximum length of a fact/lesson text body, in bytes.
pub const MAX_TEXT_BYTES: usize = 8192;
/// Maximum length of a single tag, in bytes.
pub const MAX_TAG_BYTES: usize = 64;
/// Maximum number of tags on one record.
pub const MAX_TAGS: usize = 32;
/// Maximum length of a language name, in bytes.
pub const MAX_LANGUAGE_BYTES: usize = 32;
/// Maximum number of languages on one observation.
pub const MAX_LANGUAGES: usize = 16;
/// Maximum length of a repository-relative path, in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// Maximum number of paths on one observation.
pub const MAX_OBSERVATION_PATHS: usize = 64;
/// Maximum length of a finding category, in bytes.
pub const MAX_CATEGORY_BYTES: usize = 64;
/// Maximum length of a reviewer-local finding ID, in bytes.
pub const MAX_FINDING_ID_BYTES: usize = 128;
/// Maximum length of a reviewer ID, in bytes.
pub const MAX_REVIEWER_ID_BYTES: usize = 64;
/// Maximum length of an evidence excerpt, in bytes.
pub const MAX_EXCERPT_BYTES: usize = 4096;
/// Maximum number of evidence references on one fact.
pub const MAX_EVIDENCE_REFS: usize = 32;
/// Maximum number of ID references (`supersedes`, `source_record_ids`).
pub const MAX_ID_REFS: usize = 64;
/// Maximum number of source observations referenced by one lesson.
pub const MAX_OBSERVATION_REFS: usize = 512;
/// Maximum number of path globs in one routing hint.
pub const MAX_PATH_GLOBS: usize = 32;
/// Maximum length of a single routing-hint path glob, in bytes.
pub const MAX_PATH_GLOB_BYTES: usize = 256;
/// Maximum routing boost a lesson may carry (plan section 51).
pub const MAX_ROUTING_BOOST: f64 = 0.35;

/// Check a contributor ID against the frozen slug pattern
/// `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` (plan section 44).
///
/// This is the canonical implementation; [`crate::config`] re-exports it
/// because contributor identity lives in user configuration.
pub fn is_valid_contributor_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    match bytes.first() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    if bytes.len() > 64 {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The kind of a durable record. Exactly three kinds exist (plan
/// section 40).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    Observation,
    Fact,
    Lesson,
}

impl RecordKind {
    /// Frozen serialized name of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            RecordKind::Observation => "observation",
            RecordKind::Fact => "fact",
            RecordKind::Lesson => "lesson",
        }
    }
}

impl fmt::Display for RecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A known outcome for a review finding (plan section 49).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedbackOutcome {
    /// Implementation changed specifically to address the finding.
    Fixed,
    /// Finding considered valid but intentionally not fixed now.
    Accepted,
    /// Finding investigated and judged incorrect or not applicable.
    Dismissed,
}

impl FeedbackOutcome {
    /// Frozen serialized name of this outcome.
    pub fn as_str(self) -> &'static str {
        match self {
            FeedbackOutcome::Fixed => "fixed",
            FeedbackOutcome::Accepted => "accepted",
            FeedbackOutcome::Dismissed => "dismissed",
        }
    }
}

impl fmt::Display for FeedbackOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What happened that an observation records. `review_outcome` is the
/// primary — and in Nisse 1.0 the only — observation type (plan section
/// 41).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationType {
    ReviewOutcome,
}

impl ObservationType {
    /// Frozen serialized name of this observation type.
    pub fn as_str(self) -> &'static str {
        match self {
            ObservationType::ReviewOutcome => "review_outcome",
        }
    }
}

impl fmt::Display for ObservationType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle state of a fact (plan section 42). Only `supported` facts
/// are treated as normal project truth; conflicts are surfaced
/// explicitly, never silently resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactState {
    Supported,
    UnresolvedConflict,
}

impl FactState {
    /// Frozen serialized name of this state.
    pub fn as_str(self) -> &'static str {
        match self {
            FactState::Supported => "supported",
            FactState::UnresolvedConflict => "unresolved_conflict",
        }
    }
}

impl fmt::Display for FactState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Something that happened, recorded immutably as raw learning data
/// (plan section 41). Observations remain permanently available; full
/// LLM transcripts are never persisted as observations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationRecord {
    /// Record schema version; must be `1`.
    pub schema_version: u32,
    /// Record ID (canonical ULID string).
    pub id: String,
    /// Contributor ID of the author.
    pub author: String,
    /// Creation time (UTC RFC3339).
    pub created_at: String,
    /// What kind of event this observation records.
    pub observation_type: ObservationType,
    /// Review the outcome belongs to (ULID).
    pub review_id: String,
    /// Finding the outcome applies to (reviewer-local ID).
    pub finding_id: String,
    /// Reviewer that produced the finding.
    pub reviewer_id: String,
    /// Known outcome of the finding.
    pub outcome: FeedbackOutcome,
    /// Impact class carried from the finding.
    pub impact_class: ImpactClass,
    /// Short machine-readable category carried from the finding.
    pub category: String,
    /// Languages the finding touched.
    pub languages: Vec<String>,
    /// Topic tags.
    pub tags: Vec<String>,
    /// Repository-relative paths the finding touched.
    pub paths: Vec<String>,
    /// Snapshot descriptors of the review the outcome belongs to.
    pub snapshot: SnapshotDescriptor,
}

/// Supported project knowledge (plan section 42). Created only through
/// deterministic validation plus independent verification; updated
/// knowledge creates a new record that supersedes the old one, which is
/// preserved for provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactRecord {
    /// Record schema version; must be `1`.
    pub schema_version: u32,
    /// Record ID (canonical ULID string).
    pub id: String,
    /// Contributor ID of the author.
    pub author: String,
    /// Creation time (UTC RFC3339).
    pub created_at: String,
    /// What the fact is about (stable subject phrase).
    pub subject: String,
    /// The fact statement.
    pub text: String,
    /// Topic tags.
    pub tags: Vec<String>,
    /// Fact state; only `supported` is normal project truth.
    pub state: FactState,
    /// Evidence backing the fact. Required: a fact without evidence is
    /// never durable.
    pub evidence: Vec<EvidenceRef>,
    /// Review that proposed the fact, when applicable.
    pub source_review_id: Option<String>,
    /// Records this fact was derived from (e.g. merged contributor
    /// facts).
    pub source_record_ids: Vec<String>,
    /// Record IDs superseded by this record.
    pub supersedes: Vec<String>,
}

/// Deterministic routing hint attached to a lesson (plan section 43).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingHint {
    /// Reviewer the lesson boosts.
    pub reviewer_id: String,
    /// Path globs limiting the boost.
    pub path_globs: Vec<String>,
    /// Tags limiting the boost (any-of).
    pub required_tags_any: Vec<String>,
    /// Score boost in `[0.0, 0.35]`. Lessons may increase scores but
    /// never suppress a reviewer.
    pub boost: f64,
}

/// A useful project-specific pattern suggested by repeated observations
/// (plan section 43). Statistics and routing conditions are computed
/// deterministically by the Rust core; any stored aggregates are caches
/// recomputable from the referenced observations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LessonRecord {
    /// Record schema version; must be `1`.
    pub schema_version: u32,
    /// Record ID (canonical ULID string).
    pub id: String,
    /// Contributor ID of the author.
    pub author: String,
    /// Creation time (UTC RFC3339).
    pub created_at: String,
    /// What the lesson is about (stable subject phrase).
    pub subject: String,
    /// Human-readable lesson wording.
    pub text: String,
    /// Topic tags.
    pub tags: Vec<String>,
    /// Deterministic routing hint.
    pub routing_hint: RoutingHint,
    /// Observations this lesson was derived from (their IDs). At least
    /// one is required so statistics stay recomputable.
    pub source_observation_ids: Vec<String>,
    /// Record IDs superseded by this record.
    pub supersedes: Vec<String>,
}

/// One durable project record of any kind, discriminated by `kind`.
///
/// Serialization is strict: the `kind` tag selects the schema, and the
/// selected schema rejects unknown fields. Deserialization never guesses
/// a kind.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectRecord {
    Observation(ObservationRecord),
    Fact(FactRecord),
    Lesson(LessonRecord),
}

impl ProjectRecord {
    /// The kind of this record.
    pub fn kind(&self) -> RecordKind {
        match self {
            ProjectRecord::Observation(_) => RecordKind::Observation,
            ProjectRecord::Fact(_) => RecordKind::Fact,
            ProjectRecord::Lesson(_) => RecordKind::Lesson,
        }
    }

    /// The record ID.
    pub fn id(&self) -> &str {
        match self {
            ProjectRecord::Observation(record) => &record.id,
            ProjectRecord::Fact(record) => &record.id,
            ProjectRecord::Lesson(record) => &record.id,
        }
    }

    /// The contributing author.
    pub fn author(&self) -> &str {
        match self {
            ProjectRecord::Observation(record) => &record.author,
            ProjectRecord::Fact(record) => &record.author,
            ProjectRecord::Lesson(record) => &record.author,
        }
    }

    /// Creation time (UTC RFC3339).
    pub fn created_at(&self) -> &str {
        match self {
            ProjectRecord::Observation(record) => &record.created_at,
            ProjectRecord::Fact(record) => &record.created_at,
            ProjectRecord::Lesson(record) => &record.created_at,
        }
    }

    /// Record IDs this record supersedes. Observations never supersede.
    pub fn supersedes(&self) -> &[String] {
        match self {
            ProjectRecord::Observation(_) => &[],
            ProjectRecord::Fact(record) => &record.supersedes,
            ProjectRecord::Lesson(record) => &record.supersedes,
        }
    }

    /// Validate this record against the frozen schema (plan sections
    /// 41–43 plus the general record invariants of section 39).
    ///
    /// Structural validation only: it proves the record is well-formed,
    /// not that its claims are semantically true.
    pub fn validate(&self) -> Result<(), RecordValidationError> {
        match self {
            ProjectRecord::Observation(record) => {
                validate_envelope(
                    record.schema_version,
                    &record.id,
                    &record.author,
                    &record.created_at,
                )?;
                check_ulid_field("review_id", &record.review_id)?;
                check_text("finding_id", &record.finding_id, MAX_FINDING_ID_BYTES)?;
                check_text("reviewer_id", &record.reviewer_id, MAX_REVIEWER_ID_BYTES)?;
                check_text("category", &record.category, MAX_CATEGORY_BYTES)?;
                check_string_list(
                    "languages",
                    &record.languages,
                    MAX_LANGUAGES,
                    MAX_LANGUAGE_BYTES,
                )?;
                check_string_list("tags", &record.tags, MAX_TAGS, MAX_TAG_BYTES)?;
                check_path_list("paths", &record.paths, MAX_OBSERVATION_PATHS)?;
                check_optional_oid("snapshot.base_commit", &record.snapshot.base_commit)?;
                check_optional_oid("snapshot.base_tree", &record.snapshot.base_tree)?;
                check_optional_oid("snapshot.head_commit", &record.snapshot.head_commit)?;
                check_optional_oid("snapshot.head_tree", &record.snapshot.head_tree)?;
            }
            ProjectRecord::Fact(record) => {
                validate_envelope(
                    record.schema_version,
                    &record.id,
                    &record.author,
                    &record.created_at,
                )?;
                check_text("subject", &record.subject, MAX_SUBJECT_BYTES)?;
                check_text("text", &record.text, MAX_TEXT_BYTES)?;
                check_string_list("tags", &record.tags, MAX_TAGS, MAX_TAG_BYTES)?;
                if record.evidence.is_empty() {
                    return Err(RecordValidationError::EmptyEvidence);
                }
                if record.evidence.len() > MAX_EVIDENCE_REFS {
                    return Err(RecordValidationError::TooManyEntries {
                        field: "evidence",
                        actual: record.evidence.len(),
                        max: MAX_EVIDENCE_REFS,
                    });
                }
                for evidence in &record.evidence {
                    if let Some(path) = &evidence.path {
                        check_relative_path("evidence.path", path)?;
                    }
                    check_text("evidence.excerpt", &evidence.excerpt, MAX_EXCERPT_BYTES)?;
                }
                if let Some(review_id) = &record.source_review_id {
                    check_ulid_field("source_review_id", review_id)?;
                }
                check_ulid_list("source_record_ids", &record.source_record_ids, MAX_ID_REFS)?;
                check_supersedes(&record.id, &record.supersedes)?;
            }
            ProjectRecord::Lesson(record) => {
                validate_envelope(
                    record.schema_version,
                    &record.id,
                    &record.author,
                    &record.created_at,
                )?;
                check_text("subject", &record.subject, MAX_SUBJECT_BYTES)?;
                check_text("text", &record.text, MAX_TEXT_BYTES)?;
                check_string_list("tags", &record.tags, MAX_TAGS, MAX_TAG_BYTES)?;
                check_text(
                    "routing_hint.reviewer_id",
                    &record.routing_hint.reviewer_id,
                    MAX_REVIEWER_ID_BYTES,
                )?;
                check_path_list(
                    "routing_hint.path_globs",
                    &record.routing_hint.path_globs,
                    MAX_PATH_GLOBS,
                )?;
                check_string_list(
                    "routing_hint.required_tags_any",
                    &record.routing_hint.required_tags_any,
                    MAX_TAGS,
                    MAX_TAG_BYTES,
                )?;
                let boost = record.routing_hint.boost;
                if !(0.0..=MAX_ROUTING_BOOST).contains(&boost) {
                    return Err(RecordValidationError::BoostOutOfRange { boost });
                }
                if record.source_observation_ids.is_empty() {
                    return Err(RecordValidationError::EmptyObservationRefs);
                }
                check_ulid_list(
                    "source_observation_ids",
                    &record.source_observation_ids,
                    MAX_OBSERVATION_REFS,
                )?;
                check_supersedes(&record.id, &record.supersedes)?;
            }
        }
        Ok(())
    }
}

/// Deterministic validation failures for a single record.
#[derive(Debug, Error)]
pub enum RecordValidationError {
    /// The record was written for a different schema version.
    #[error("unsupported schema version {found} (expected {expected})")]
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    /// The record ID is not a canonical ULID.
    #[error("record id `{value}` is not a canonical ULID")]
    InvalidId { value: String },
    /// The author is not a valid contributor ID.
    #[error("author `{value}` is not a valid contributor id")]
    InvalidAuthor { value: String },
    /// The creation time is not a UTC RFC3339 timestamp.
    #[error("created_at `{value}` is not a UTC RFC3339 timestamp")]
    InvalidCreatedAt { value: String },
    /// A required text field is empty.
    #[error("field `{field}` must not be empty")]
    EmptyField { field: &'static str },
    /// A text field exceeds its byte limit.
    #[error("field `{field}` is {actual} bytes, exceeding the {max_bytes}-byte limit")]
    FieldTooLong {
        field: &'static str,
        actual: usize,
        max_bytes: usize,
    },
    /// A list field exceeds its entry-count limit.
    #[error("field `{field}` has {actual} entries, exceeding the limit of {max}")]
    TooManyEntries {
        field: &'static str,
        actual: usize,
        max: usize,
    },
    /// A referenced ID is not a canonical ULID.
    #[error("field `{field}` contains invalid ULID reference `{value}`")]
    InvalidUlidRef { field: &'static str, value: String },
    /// A list field contains duplicate entries.
    #[error("field `{field}` contains duplicate entries")]
    DuplicateEntries { field: &'static str },
    /// A record references itself in `supersedes`.
    #[error("record `{id}` supersedes itself")]
    SelfSupersede { id: String },
    /// A fact has no evidence references.
    #[error("fact records require at least one evidence reference")]
    EmptyEvidence,
    /// A lesson has no source observations.
    #[error("lesson records require at least one source observation")]
    EmptyObservationRefs,
    /// The routing boost is outside `[0.0, 0.35]`.
    #[error("routing boost {boost} is outside the allowed range 0.0..=0.35")]
    BoostOutOfRange { boost: f64 },
    /// A path field is not a safe repository-relative path.
    #[error("field `{field}` contains invalid repository-relative path `{value}`")]
    InvalidPath { field: &'static str, value: String },
    /// An object ID field is not a lowercase hex Git OID.
    #[error("field `{field}` contains invalid object id `{value}`")]
    InvalidOid { field: &'static str, value: String },
}

/// Validate the fields shared by every record kind.
fn validate_envelope(
    schema_version: u32,
    id: &str,
    author: &str,
    created_at: &str,
) -> Result<(), RecordValidationError> {
    if schema_version != SCHEMA_VERSION {
        return Err(RecordValidationError::UnsupportedSchemaVersion {
            found: schema_version,
            expected: SCHEMA_VERSION,
        });
    }
    if !is_valid_ulid(id) {
        return Err(RecordValidationError::InvalidId {
            value: id.to_string(),
        });
    }
    if !is_valid_contributor_id(author) {
        return Err(RecordValidationError::InvalidAuthor {
            value: author.to_string(),
        });
    }
    if !is_valid_utc_rfc3339(created_at) {
        return Err(RecordValidationError::InvalidCreatedAt {
            value: created_at.to_string(),
        });
    }
    Ok(())
}

/// Check a required, bounded text field.
fn check_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), RecordValidationError> {
    if value.is_empty() {
        return Err(RecordValidationError::EmptyField { field });
    }
    if value.len() > max_bytes {
        return Err(RecordValidationError::FieldTooLong {
            field,
            actual: value.len(),
            max_bytes,
        });
    }
    Ok(())
}

/// Check a list of bounded, non-empty strings.
fn check_string_list(
    field: &'static str,
    values: &[String],
    max_count: usize,
    max_item_bytes: usize,
) -> Result<(), RecordValidationError> {
    if values.len() > max_count {
        return Err(RecordValidationError::TooManyEntries {
            field,
            actual: values.len(),
            max: max_count,
        });
    }
    for value in values {
        check_text(field, value, max_item_bytes)?;
    }
    Ok(())
}

/// Check a list of safe repository-relative paths.
fn check_path_list(
    field: &'static str,
    values: &[String],
    max_count: usize,
) -> Result<(), RecordValidationError> {
    if values.len() > max_count {
        return Err(RecordValidationError::TooManyEntries {
            field,
            actual: values.len(),
            max: max_count,
        });
    }
    for value in values {
        check_relative_path(field, value)?;
    }
    Ok(())
}

/// Check one repository-relative path: non-empty, bounded, with no
/// root, prefix, parent, or current-directory components.
fn check_relative_path(field: &'static str, value: &str) -> Result<(), RecordValidationError> {
    if value.is_empty() {
        return Err(RecordValidationError::EmptyField { field });
    }
    if value.len() > MAX_PATH_BYTES {
        return Err(RecordValidationError::FieldTooLong {
            field,
            actual: value.len(),
            max_bytes: MAX_PATH_BYTES,
        });
    }
    let has_bad_component = Path::new(value)
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)));
    if has_bad_component {
        return Err(RecordValidationError::InvalidPath {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Check a required ULID field.
fn check_ulid_field(field: &'static str, value: &str) -> Result<(), RecordValidationError> {
    if !is_valid_ulid(value) {
        return Err(RecordValidationError::InvalidUlidRef {
            field,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Check a list of ULID references without duplicates.
fn check_ulid_list(
    field: &'static str,
    values: &[String],
    max_count: usize,
) -> Result<(), RecordValidationError> {
    if values.len() > max_count {
        return Err(RecordValidationError::TooManyEntries {
            field,
            actual: values.len(),
            max: max_count,
        });
    }
    for value in values {
        check_ulid_field(field, value)?;
    }
    if has_duplicates(values) {
        return Err(RecordValidationError::DuplicateEntries { field });
    }
    Ok(())
}

/// Check `supersedes`: bounded, unique, valid ULIDs, no self-reference.
fn check_supersedes(id: &str, supersedes: &[String]) -> Result<(), RecordValidationError> {
    check_ulid_list("supersedes", supersedes, MAX_ID_REFS)?;
    if supersedes.iter().any(|superseded| superseded == id) {
        return Err(RecordValidationError::SelfSupersede { id: id.to_string() });
    }
    Ok(())
}

/// Check an optional object ID field: lowercase hex, 40 or 64 digits.
fn check_optional_oid(
    field: &'static str,
    value: &Option<String>,
) -> Result<(), RecordValidationError> {
    let Some(oid) = value else {
        return Ok(());
    };
    let valid = (oid.len() == 40 || oid.len() == 64)
        && oid
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    if valid {
        Ok(())
    } else {
        Err(RecordValidationError::InvalidOid {
            field,
            value: oid.to_string(),
        })
    }
}

/// Whether `values` contains at least one duplicate.
fn has_duplicates(values: &[String]) -> bool {
    let mut seen = std::collections::HashSet::new();
    values.iter().any(|value| !seen.insert(value))
}

/// Human-readable JSON type name for error messages.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Parse one JSON value into a [`ProjectRecord`], dispatching strictly
/// on the `kind` tag and rejecting unknown fields within the selected
/// schema.
pub(crate) fn parse_project_record(value: serde_json::Value) -> Result<ProjectRecord, String> {
    let serde_json::Value::Object(mut map) = value else {
        return Err("record must be a JSON object".to_string());
    };
    let Some(kind) = map.remove("kind") else {
        return Err("record is missing the `kind` field".to_string());
    };
    let serde_json::Value::String(kind) = kind else {
        return Err("record `kind` must be a string".to_string());
    };
    match kind.as_str() {
        "observation" => serde_json::from_value(serde_json::Value::Object(map))
            .map(ProjectRecord::Observation)
            .map_err(|err| format!("invalid observation record: {err}")),
        "fact" => serde_json::from_value(serde_json::Value::Object(map))
            .map(ProjectRecord::Fact)
            .map_err(|err| format!("invalid fact record: {err}")),
        "lesson" => serde_json::from_value(serde_json::Value::Object(map))
            .map(ProjectRecord::Lesson)
            .map_err(|err| format!("invalid lesson record: {err}")),
        other => Err(format!(
            "unknown record kind `{other}` (expected observation, fact, or lesson)"
        )),
    }
}

impl Serialize for ProjectRecord {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::Error as _;

        let (kind, value) = match self {
            ProjectRecord::Observation(record) => ("observation", serde_json::to_value(record)),
            ProjectRecord::Fact(record) => ("fact", serde_json::to_value(record)),
            ProjectRecord::Lesson(record) => ("lesson", serde_json::to_value(record)),
        };
        let map = match value {
            Ok(serde_json::Value::Object(map)) => map,
            Ok(other) => {
                return Err(S::Error::custom(format!(
                    "record serialized to non-object JSON ({})",
                    json_type_name(&other)
                )));
            }
            Err(err) => return Err(S::Error::custom(err.to_string())),
        };
        let mut tagged = map;
        tagged.insert(
            "kind".to_string(),
            serde_json::Value::String(kind.to_string()),
        );
        tagged.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ProjectRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        parse_project_record(value).map_err(serde::de::Error::custom)
    }
}

/// Shared record builders for in-crate tests.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::domain::evidence::EvidenceKind;
    use crate::domain::ids::new_ulid;
    use crate::domain::snapshot::SnapshotMode;
    use crate::domain::time::now_utc_rfc3339;

    pub(crate) fn ulid() -> String {
        new_ulid()
    }

    fn timestamp() -> String {
        now_utc_rfc3339()
    }

    pub(crate) fn evidence() -> EvidenceRef {
        EvidenceRef {
            kind: EvidenceKind::Source,
            path: Some("src/lib.rs".to_string()),
            symbol: Some("enqueue".to_string()),
            line_start: Some(10),
            line_end: Some(12),
            content_key: Some("git:0123456789abcdef0123456789abcdef01234567".to_string()),
            plan_section: None,
            record_id: None,
            excerpt: "self.head = (self.head + 1) % len;".to_string(),
        }
    }

    pub(crate) fn observation(author: &str) -> ObservationRecord {
        ObservationRecord {
            schema_version: 1,
            id: ulid(),
            author: author.to_string(),
            created_at: timestamp(),
            observation_type: ObservationType::ReviewOutcome,
            review_id: ulid(),
            finding_id: "q-1".to_string(),
            reviewer_id: "quality".to_string(),
            outcome: FeedbackOutcome::Fixed,
            impact_class: ImpactClass::Correctness,
            category: "race".to_string(),
            languages: vec!["rust".to_string()],
            tags: vec!["concurrency".to_string()],
            paths: vec!["src/lib.rs".to_string()],
            snapshot: SnapshotDescriptor {
                mode: SnapshotMode::Working,
                base_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                base_tree: None,
                head_commit: None,
                head_tree: None,
            },
        }
    }

    pub(crate) fn fact(author: &str) -> FactRecord {
        FactRecord {
            schema_version: 1,
            id: ulid(),
            author: author.to_string(),
            created_at: timestamp(),
            subject: "queue discipline".to_string(),
            text: "The sensor queue is single-producer single-consumer.".to_string(),
            tags: vec!["queue".to_string()],
            state: FactState::Supported,
            evidence: vec![evidence()],
            source_review_id: None,
            source_record_ids: Vec::new(),
            supersedes: Vec::new(),
        }
    }

    pub(crate) fn lesson(author: &str) -> LessonRecord {
        LessonRecord {
            schema_version: 1,
            id: ulid(),
            author: author.to_string(),
            created_at: timestamp(),
            subject: "interrupt safety".to_string(),
            text: "Shared ISR state requires critical sections in this project.".to_string(),
            tags: vec!["embedded".to_string()],
            routing_hint: RoutingHint {
                reviewer_id: "concurrency".to_string(),
                path_globs: vec!["src/isr/**".to_string()],
                required_tags_any: vec!["isr".to_string()],
                boost: 0.2,
            },
            source_observation_ids: vec![ulid()],
            supersedes: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::{fact, lesson, observation, ulid};
    use super::*;

    #[test]
    fn contributor_id_pattern() {
        assert!(is_valid_contributor_id("a"));
        assert!(is_valid_contributor_id("Alice.dev-1_x"));
        assert!(is_valid_contributor_id(&"x".repeat(64)));
        assert!(!is_valid_contributor_id(""));
        assert!(!is_valid_contributor_id(".hidden"));
        assert!(!is_valid_contributor_id("-dash"));
        assert!(!is_valid_contributor_id("_under"));
        assert!(is_valid_contributor_id("9"));
        assert!(!is_valid_contributor_id("has/slash"));
        assert!(!is_valid_contributor_id("has\\backslash"));
        assert!(!is_valid_contributor_id("has space"));
        assert!(!is_valid_contributor_id(".."));
        assert!(!is_valid_contributor_id(&"x".repeat(65)));
        assert!(!is_valid_contributor_id("path/../traversal"));
    }

    #[test]
    fn enum_serialization_is_frozen() {
        assert_eq!(
            serde_json::to_string(&RecordKind::Observation).unwrap(),
            "\"observation\""
        );
        assert_eq!(
            serde_json::to_string(&FeedbackOutcome::Accepted).unwrap(),
            "\"accepted\""
        );
        assert_eq!(
            serde_json::to_string(&ObservationType::ReviewOutcome).unwrap(),
            "\"review_outcome\""
        );
        assert_eq!(
            serde_json::to_string(&FactState::UnresolvedConflict).unwrap(),
            "\"unresolved_conflict\""
        );
        for text in ["fixed", "accepted", "dismissed"] {
            assert!(serde_json::from_str::<FeedbackOutcome>(&format!("\"{text}\"")).is_ok());
        }
        assert!(serde_json::from_str::<FeedbackOutcome>("\"reverted\"").is_err());
        assert!(serde_json::from_str::<FactState>("\"conflict\"").is_err());
        assert!(serde_json::from_str::<ObservationType>("\"build_failure\"").is_err());
        assert!(serde_json::from_str::<RecordKind>("\"note\"").is_err());
        assert_eq!(RecordKind::Lesson.to_string(), "lesson");
        assert_eq!(FeedbackOutcome::Dismissed.to_string(), "dismissed");
        assert_eq!(FactState::Supported.to_string(), "supported");
        assert_eq!(ObservationType::ReviewOutcome.to_string(), "review_outcome");
    }

    #[test]
    fn project_record_roundtrips_each_kind() {
        for record in [
            ProjectRecord::Observation(observation("alice")),
            ProjectRecord::Fact(fact("alice")),
            ProjectRecord::Lesson(lesson("alice")),
        ] {
            let json = serde_json::to_string(&record).unwrap();
            assert!(json.contains(&format!("\"kind\":\"{}\"", record.kind())));
            let back: ProjectRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(record, back);
            // Accessors agree with the payload.
            assert_eq!(back.kind(), record.kind());
            assert!(is_valid_ulid(back.id()));
            assert_eq!(back.author(), "alice");
            assert!(is_valid_utc_rfc3339(back.created_at()));
            back.validate().unwrap();
        }
    }

    #[test]
    fn pretty_json_roundtrip() {
        let record = ProjectRecord::Fact(fact("bob"));
        let pretty = serde_json::to_string_pretty(&record).unwrap();
        let back: ProjectRecord = serde_json::from_str(&pretty).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn parse_rejects_unknown_kind_and_missing_kind() {
        let err = serde_json::from_str::<ProjectRecord>(r#"{"kind":"note"}"#).unwrap_err();
        assert!(
            err.to_string().contains("unknown record kind `note`"),
            "{err}"
        );

        let err =
            serde_json::from_str::<ProjectRecord>(r#"{"schema_version":1,"id":"x"}"#).unwrap_err();
        assert!(
            err.to_string().contains("missing the `kind` field"),
            "{err}"
        );

        let err =
            serde_json::from_str::<ProjectRecord>(r#"{"kind":3,"schema_version":1}"#).unwrap_err();
        assert!(err.to_string().contains("`kind` must be a string"), "{err}");
    }

    #[test]
    fn parse_rejects_non_object_records() {
        let err = serde_json::from_str::<ProjectRecord>("[]").unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"), "{err}");
        let err = serde_json::from_str::<ProjectRecord>("\"observation\"").unwrap_err();
        assert!(err.to_string().contains("must be a JSON object"), "{err}");
    }

    #[test]
    fn parse_rejects_unknown_fields_within_each_kind() {
        let observation_json =
            serde_json::to_value(ProjectRecord::Observation(observation("alice"))).unwrap();
        let fact_json = serde_json::to_value(ProjectRecord::Fact(fact("alice"))).unwrap();
        let lesson_json = serde_json::to_value(ProjectRecord::Lesson(lesson("alice"))).unwrap();

        for mut json in [observation_json, fact_json, lesson_json] {
            json.as_object_mut()
                .unwrap()
                .insert("transcript".to_string(), serde_json::json!("full LLM log"));
            let err = parse_project_record(json).expect_err("unknown field must be rejected");
            assert!(err.contains("unknown field `transcript`"), "{err}");
        }
    }

    #[test]
    fn parse_rejects_kind_schema_mismatches() {
        // An observation payload tagged as a fact must fail on the
        // observation-only fields.
        let mut json =
            serde_json::to_value(ProjectRecord::Observation(observation("alice"))).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("kind".to_string(), serde_json::json!("fact"));
        let err = parse_project_record(json).expect_err("kind mismatch must be rejected");
        assert!(err.contains("invalid fact record"), "{err}");
    }

    #[test]
    fn parse_requires_every_field() {
        let mut json = serde_json::to_value(ProjectRecord::Fact(fact("alice"))).unwrap();
        json.as_object_mut().unwrap().remove("state");
        let err = parse_project_record(json).expect_err("missing field must be rejected");
        assert!(err.contains("missing field `state`"), "{err}");
    }

    #[test]
    fn validation_rejects_envelope_defects() {
        let mut record = observation("alice");
        record.schema_version = 2;
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::UnsupportedSchemaVersion {
                found: 2,
                expected: 1
            })
        ));

        let mut record = observation("alice");
        record.id = "not-a-ulid".to_string();
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::InvalidId { .. })
        ));

        let record = observation("../evil");
        assert!(matches!(
            ProjectRecord::Observation(record).validate(),
            Err(RecordValidationError::InvalidAuthor { .. })
        ));

        let mut record = observation("alice");
        record.created_at = "2026-01-19T12:00:00+02:00".to_string();
        assert!(matches!(
            ProjectRecord::Observation(record).validate(),
            Err(RecordValidationError::InvalidCreatedAt { .. })
        ));
    }

    #[test]
    fn validation_rejects_observation_field_defects() {
        let mut record = observation("alice");
        record.review_id = "r1".to_string();
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::InvalidUlidRef {
                field: "review_id",
                ..
            })
        ));

        let mut record = observation("alice");
        record.category = String::new();
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::EmptyField { field: "category" })
        ));

        let mut record = observation("alice");
        record.paths = vec!["../outside.txt".to_string()];
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::InvalidPath { field: "paths", .. })
        ));

        let mut record = observation("alice");
        record.paths = vec!["/absolute/path".to_string()];
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::InvalidPath { .. })
        ));

        let mut record = observation("alice");
        record.snapshot.base_commit = Some("NOTANOID".to_string());
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::InvalidOid {
                field: "snapshot.base_commit",
                ..
            })
        ));

        // Uppercase hex is rejected to match Git's lowercase output.
        let mut record = observation("alice");
        record.snapshot.head_tree = Some("0123ABCDEF0123456789ABCDEF0123456789ABCD".to_string());
        assert!(matches!(
            ProjectRecord::Observation(record.clone()).validate(),
            Err(RecordValidationError::InvalidOid { .. })
        ));

        let mut record = observation("alice");
        record.languages = (0..MAX_LANGUAGES + 1)
            .map(|index| format!("lang{index}"))
            .collect();
        assert!(matches!(
            ProjectRecord::Observation(record).validate(),
            Err(RecordValidationError::TooManyEntries {
                field: "languages",
                ..
            })
        ));
    }

    #[test]
    fn validation_rejects_fact_defects() {
        let mut record = fact("alice");
        record.evidence = Vec::new();
        assert!(matches!(
            ProjectRecord::Fact(record.clone()).validate(),
            Err(RecordValidationError::EmptyEvidence)
        ));

        let mut record = fact("alice");
        record.text = "x".repeat(MAX_TEXT_BYTES + 1);
        assert!(matches!(
            ProjectRecord::Fact(record.clone()).validate(),
            Err(RecordValidationError::FieldTooLong { field: "text", .. })
        ));

        let mut record = fact("alice");
        record.supersedes = vec![record.id.clone()];
        assert!(matches!(
            ProjectRecord::Fact(record.clone()).validate(),
            Err(RecordValidationError::SelfSupersede { .. })
        ));

        let other = ulid();
        let mut record = fact("alice");
        record.supersedes = vec![other.clone(), other];
        assert!(matches!(
            ProjectRecord::Fact(record.clone()).validate(),
            Err(RecordValidationError::DuplicateEntries {
                field: "supersedes"
            })
        ));

        let mut record = fact("alice");
        record.source_record_ids = vec!["bogus".to_string()];
        assert!(matches!(
            ProjectRecord::Fact(record.clone()).validate(),
            Err(RecordValidationError::InvalidUlidRef {
                field: "source_record_ids",
                ..
            })
        ));

        let mut record = fact("alice");
        record.evidence[0].path = Some("..".to_string());
        assert!(matches!(
            ProjectRecord::Fact(record.clone()).validate(),
            Err(RecordValidationError::InvalidPath {
                field: "evidence.path",
                ..
            })
        ));

        let mut record = fact("alice");
        record.evidence[0].excerpt = String::new();
        assert!(matches!(
            ProjectRecord::Fact(record).validate(),
            Err(RecordValidationError::EmptyField {
                field: "evidence.excerpt"
            })
        ));
    }

    #[test]
    fn validation_rejects_lesson_defects() {
        let mut record = lesson("alice");
        record.source_observation_ids = Vec::new();
        assert!(matches!(
            ProjectRecord::Lesson(record.clone()).validate(),
            Err(RecordValidationError::EmptyObservationRefs)
        ));

        let mut record = lesson("alice");
        record.routing_hint.boost = 0.5;
        assert!(matches!(
            ProjectRecord::Lesson(record.clone()).validate(),
            Err(RecordValidationError::BoostOutOfRange { boost: 0.5 })
        ));

        let mut record = lesson("alice");
        record.routing_hint.boost = f64::NAN;
        assert!(matches!(
            ProjectRecord::Lesson(record.clone()).validate(),
            Err(RecordValidationError::BoostOutOfRange { .. })
        ));

        let mut record = lesson("alice");
        record.routing_hint.reviewer_id = String::new();
        assert!(matches!(
            ProjectRecord::Lesson(record.clone()).validate(),
            Err(RecordValidationError::EmptyField {
                field: "routing_hint.reviewer_id"
            })
        ));

        let mut record = lesson("alice");
        record.routing_hint.path_globs = vec!["../*.rs".to_string()];
        assert!(matches!(
            ProjectRecord::Lesson(record.clone()).validate(),
            Err(RecordValidationError::InvalidPath {
                field: "routing_hint.path_globs",
                ..
            })
        ));

        let mut record = lesson("alice");
        record.routing_hint.boost = MAX_ROUTING_BOOST;
        assert!(ProjectRecord::Lesson(record).validate().is_ok());
    }

    #[test]
    fn supersedes_accessor_defaults_for_observations() {
        assert!(
            ProjectRecord::Observation(observation("alice"))
                .supersedes()
                .is_empty()
        );
        let superseded = ulid();
        let mut record = fact("alice");
        record.supersedes = vec![superseded.clone()];
        assert_eq!(
            ProjectRecord::Fact(record).supersedes(),
            &[superseded] as &[String]
        );
    }
}
