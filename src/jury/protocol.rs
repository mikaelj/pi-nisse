//! Pi review host protocol schema (plan sections 20 and 21).
//!
//! Nisse Core (Rust) and the Pi review host (TypeScript, `runtime/pi-host`)
//! communicate using bidirectional JSONL over the child process's
//! stdin/stdout. stdout carries protocol messages only; logs go to stderr.
//!
//! Every message is a single JSON object per line with the envelope fields
//!
//! ```text
//! protocol_version  u32    (frozen at 1)
//! id                string (unique per sender, used for correlation)
//! type              string (frozen message vocabulary below)
//! ```
//!
//! plus type-specific payload fields flattened into the same object.
//!
//! The vocabulary below is the complete frozen set from plan section 21.
//! Message types belonging to later milestones are declared as constants so
//! both sides agree on the vocabulary. This milestone implements the core
//! review loop (`host_ready`, `start_review`, `reviewer_started`,
//! `reviewer_completed`, `reviewer_failed`, `tool_request`,
//! `tool_response`, `cancel`, `host_error`), the narrow
//! `start_finding_equivalence_check` / `finding_equivalence_completed`
//! exchange of finding consolidation (plan section 38), the constrained
//! `start_domain_description` / `domain_description_completed` exchange of
//! the dynamic domain expert (plan section 19), and the independent
//! `start_fact_verification` / `fact_verification_completed` exchange of
//! fact persistence (plan section 47). Receiving a
//! vocabulary-but-unimplemented type is a typed protocol error, not a guess.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::domain::context::ContextBundle;
use crate::domain::evidence::EvidenceRef;
use crate::domain::facts::FactVerificationVerdict;
use crate::domain::finding::ReviewerFinding;
use crate::domain::model::ModelProfile;
use crate::domain::report::ReviewerReport;
use crate::domain::review::ReviewKind;
use crate::domain::reviewer::{
    DynamicDomainDescription, ReviewerCapability, ReviewerExecutionSpec,
};

/// Protocol version of this build (plan section 21: version 1).
pub const PROTOCOL_VERSION: u32 = 1;

/// Frozen message-type names Nisse Core may send to the host.
pub mod rust_to_host {
    /// Host configuration (not yet active; reserved by the frozen plan).
    pub const HOST_CONFIG: &str = "host_config";
    /// Start a review: execute the given reviewer specs.
    pub const START_REVIEW: &str = "start_review";
    /// Start an independent fact verification job (later milestone).
    pub const START_FACT_VERIFICATION: &str = "start_fact_verification";
    /// Start an independent fact synthesis job (later milestone).
    pub const START_FACT_SYNTHESIS: &str = "start_fact_synthesis";
    /// Start a constrained dynamic-domain description job (later milestone).
    pub const START_DOMAIN_DESCRIPTION: &str = "start_domain_description";
    /// Start a narrow finding equivalence check (later milestone).
    pub const START_FINDING_EQUIVALENCE_CHECK: &str = "start_finding_equivalence_check";
    /// Answer a `tool_request` from the host.
    pub const TOOL_RESPONSE: &str = "tool_response";
    /// Cancel one review or all pending work.
    pub const CANCEL: &str = "cancel";

    /// The complete frozen Rust-to-host vocabulary, in plan order.
    pub const ALL: [&str; 8] = [
        HOST_CONFIG,
        START_REVIEW,
        START_FACT_VERIFICATION,
        START_FACT_SYNTHESIS,
        START_DOMAIN_DESCRIPTION,
        START_FINDING_EQUIVALENCE_CHECK,
        TOOL_RESPONSE,
        CANCEL,
    ];
}

/// Frozen message-type names the host may send to Nisse Core.
pub mod host_to_rust {
    /// The host finished startup and can accept jobs.
    pub const HOST_READY: &str = "host_ready";
    /// One reviewer session started.
    pub const REVIEWER_STARTED: &str = "reviewer_started";
    /// One reviewer session produced a valid structured report.
    pub const REVIEWER_COMPLETED: &str = "reviewer_completed";
    /// One reviewer session failed.
    pub const REVIEWER_FAILED: &str = "reviewer_failed";
    /// The host needs a read-only lookup served by Rust.
    pub const TOOL_REQUEST: &str = "tool_request";
    /// Independent fact verification finished (later milestone).
    pub const FACT_VERIFICATION_COMPLETED: &str = "fact_verification_completed";
    /// Independent fact synthesis finished (later milestone).
    pub const FACT_SYNTHESIS_COMPLETED: &str = "fact_synthesis_completed";
    /// Constrained dynamic-domain description finished (later milestone).
    pub const DOMAIN_DESCRIPTION_COMPLETED: &str = "domain_description_completed";
    /// Narrow finding equivalence check finished (later milestone).
    pub const FINDING_EQUIVALENCE_COMPLETED: &str = "finding_equivalence_completed";
    /// The host reports a protocol-level failure.
    pub const HOST_ERROR: &str = "host_error";

    /// The complete frozen host-to-Rust vocabulary, in plan order.
    pub const ALL: [&str; 10] = [
        HOST_READY,
        REVIEWER_STARTED,
        REVIEWER_COMPLETED,
        REVIEWER_FAILED,
        TOOL_REQUEST,
        FACT_VERIFICATION_COMPLETED,
        FACT_SYNTHESIS_COMPLETED,
        DOMAIN_DESCRIPTION_COMPLETED,
        FINDING_EQUIVALENCE_COMPLETED,
        HOST_ERROR,
    ];
}

/// Host runtime implementations. `Fake` is the deterministic reviewer
/// runtime used for protocol tests; real Pi SDK sessions are integrated in
/// a later milestone (plan section 20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRuntime {
    /// Deterministic fake reviewer runtime (tests, offline operation).
    Fake,
    /// Real Pi SDK reviewer sessions.
    Pi,
}

impl HostRuntime {
    /// The `--runtime` argument value selecting this runtime.
    pub fn as_arg(self) -> &'static str {
        match self {
            HostRuntime::Fake => "fake",
            HostRuntime::Pi => "pi",
        }
    }
}

/// A structured error reported by the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostErrorInfo {
    /// Stable machine-readable error code (e.g. `malformed_message`).
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

/// `host_ready` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostReady {
    /// Which runtime implementation is active.
    pub runtime: HostRuntime,
}

/// `host_config` payload: the resolved model profiles the host may use.
///
/// Rust resolves named profiles from trusted local configuration (plan
/// sections 10 and 11) and forwards the resolution to the host; reviewer
/// sessions never inherit a model. Profiles may be replaced by sending
/// `host_config` again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostModelProfile {
    /// Resolved model identifier (`provider/model-id`).
    pub model: String,
    /// Optional explicit endpoint base URL, already endpoint-trust-validated
    /// by the Rust core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

impl HostModelProfile {
    /// Build the wire profile from a configured [`ModelProfile`].
    ///
    /// The profile's optional `thinking_level` is deliberately dropped: the
    /// thinking level of a session is carried explicitly by each
    /// [`ReviewerExecutionSpec`] (plan section 18), never by the model table.
    pub fn from_config(profile: &ModelProfile) -> Self {
        HostModelProfile {
            model: profile.model.clone(),
            endpoint: profile.endpoint.clone(),
        }
    }
}

/// `host_config` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfigPayload {
    /// Resolved model profiles by profile name.
    pub models: BTreeMap<String, HostModelProfile>,
}

impl HostConfigPayload {
    /// Build the payload from configured model profiles.
    pub fn from_config_profiles(profiles: &BTreeMap<String, ModelProfile>) -> Self {
        HostConfigPayload {
            models: profiles
                .iter()
                .map(|(name, profile)| (name.clone(), HostModelProfile::from_config(profile)))
                .collect(),
        }
    }

    /// Whether `profile` was supplied to the host.
    pub fn contains(&self, profile: &str) -> bool {
        self.models.contains_key(profile)
    }
}

/// `start_review` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartReviewPayload {
    /// Identifier of the review (assigned by Rust).
    pub review_id: String,
    /// What kind of review this is.
    pub kind: ReviewKind,
    /// One execution spec per juror. Rust owns jury composition; the host
    /// executes exactly what it is given.
    pub reviewers: Vec<ReviewerExecutionSpec>,
}

/// `tool_request` payload. The envelope `id` of this message is the
/// `request_id` that `tool_response` must reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRequestPayload {
    /// The review the requesting reviewer belongs to.
    pub review_id: String,
    /// The reviewer performing the lookup.
    pub reviewer_id: String,
    /// The read-only tool being invoked (plan section 22).
    pub tool: ReviewerCapability,
    /// Tool arguments. Their exact schemas are owned by the tool-serving
    /// milestone; the boundary treats them as opaque JSON.
    pub arguments: serde_json::Value,
}

/// `tool_response` payload sent by Rust in answer to a `tool_request`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResponsePayload {
    /// The envelope `id` of the answered `tool_request`.
    pub request_id: String,
    /// Whether the lookup succeeded.
    pub ok: bool,
    /// The bounded context bundle on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle: Option<ContextBundle>,
    /// The failure reason on error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HostErrorInfo>,
}

impl ToolResponsePayload {
    /// Build a successful response carrying `bundle`.
    pub fn success(request_id: impl Into<String>, bundle: ContextBundle) -> Self {
        ToolResponsePayload {
            request_id: request_id.into(),
            ok: true,
            bundle: Some(bundle),
            error: None,
        }
    }

    /// Build a failed response carrying a code and message.
    pub fn failure(request_id: impl Into<String>, code: &str, message: &str) -> Self {
        ToolResponsePayload {
            request_id: request_id.into(),
            ok: false,
            bundle: None,
            error: Some(HostErrorInfo {
                code: code.to_string(),
                message: message.to_string(),
            }),
        }
    }
}

/// `cancel` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelPayload {
    /// The review to cancel, or `None` to cancel all pending work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
}

impl CancelPayload {
    /// Cancel every pending review.
    pub fn all() -> Self {
        CancelPayload { review_id: None }
    }

    /// Cancel one review.
    pub fn review(review_id: impl Into<String>) -> Self {
        CancelPayload {
            review_id: Some(review_id.into()),
        }
    }
}

/// A decoded host-to-Rust message, ready for dispatch.
#[derive(Debug, Clone, PartialEq)]
pub enum HostEvent {
    /// The host announced readiness.
    HostReady(HostReady),
    /// A reviewer session started.
    ReviewerStarted {
        /// The review the reviewer belongs to.
        review_id: String,
        /// The reviewer that started.
        reviewer_id: String,
    },
    /// A reviewer session produced a structured report.
    ReviewerCompleted {
        /// The review the reviewer belongs to.
        review_id: String,
        /// The reviewer that completed.
        reviewer_id: String,
        /// The strict structured report.
        report: ReviewerReport,
    },
    /// A reviewer session failed.
    ReviewerFailed {
        /// The review the reviewer belongs to.
        review_id: String,
        /// The reviewer that failed.
        reviewer_id: String,
        /// The failure reported by the host.
        error: HostErrorInfo,
    },
    /// The host requested a read-only lookup.
    ToolRequest {
        /// Envelope id of the request; `tool_response` must echo it as
        /// `request_id`.
        id: String,
        /// The request payload.
        payload: ToolRequestPayload,
    },
    /// A narrow finding-equivalence check finished.
    FindingEquivalenceCompleted {
        /// Correlation identifier of the answered check.
        check_id: String,
        /// The verdict; `uncertain` when the check fell back.
        verdict: EquivalenceVerdict,
        /// Why the verdict fell back to `uncertain`.
        error: Option<HostErrorInfo>,
    },
    /// A constrained dynamic-domain description finished (plan section 19).
    DomainDescriptionCompleted {
        /// Correlation identifier of the answered description job.
        description_id: String,
        /// The validated description; `None` when the session failed and
        /// the caller must fall back to the plain static reviewer template.
        description: Option<DynamicDomainDescription>,
        /// Why the description fell back, when its session failed.
        error: Option<HostErrorInfo>,
    },
    /// An independent fact-verification job finished (plan section 47).
    FactVerificationCompleted {
        /// Correlation identifier of the answered verification job.
        verification_id: String,
        /// The verdict. A failed session always reports `insufficient`
        /// plus an `error` (uncertainty never persists a fact).
        verdict: FactVerificationVerdict,
        /// Why the verifier could not decide, when its session failed;
        /// only permitted with the `insufficient` verdict.
        error: Option<HostErrorInfo>,
    },
    /// The host reported a protocol-level failure.
    HostError(HostErrorInfo),
}

/// Errors produced while decoding host output.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProtocolError {
    /// The line was not a valid protocol message object.
    #[error("malformed protocol line: {reason}")]
    MalformedLine {
        /// Why the line is malformed.
        reason: String,
    },
    /// The envelope's protocol version does not match this build.
    #[error("unsupported protocol version {got}; expected {expected}")]
    UnsupportedVersion {
        /// The version the host sent.
        got: u64,
        /// The version this build speaks.
        expected: u32,
    },
    /// The message type is not part of the frozen vocabulary.
    #[error("unknown host message type `{0}`")]
    UnknownMessageType(String),
    /// The message type is frozen vocabulary but not implemented by this
    /// milestone's parser.
    #[error("host message type `{0}` exists in the protocol vocabulary but is not implemented yet")]
    UnsupportedMessageType(String),
    /// The message type is known but the payload failed strict validation.
    #[error("invalid `{message_type}` payload: {reason}")]
    InvalidPayload {
        /// The message whose payload is invalid.
        message_type: String,
        /// Why the payload is invalid.
        reason: String,
    },
}

/// A protocol envelope with the common fields extracted and the remaining
/// payload fields kept as raw JSON.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// Envelope protocol version.
    pub protocol_version: u32,
    /// Envelope id.
    pub id: String,
    /// Envelope message type.
    pub message_type: String,
    /// Remaining payload fields.
    pub payload: serde_json::Value,
}

impl Envelope {
    /// Decode one JSONL line into an envelope, validating the common
    /// envelope fields against the frozen host-to-Rust vocabulary.
    pub fn parse_host_line(line: &str) -> Result<Envelope, ProtocolError> {
        let value: serde_json::Value =
            serde_json::from_str(line).map_err(|err| ProtocolError::MalformedLine {
                reason: format!("not valid JSON: {err}"),
            })?;
        Self::from_value(value)
    }

    /// Decode an already-parsed JSON value into an envelope.
    pub fn from_value(value: serde_json::Value) -> Result<Envelope, ProtocolError> {
        let object = value
            .as_object()
            .ok_or_else(|| ProtocolError::MalformedLine {
                reason: "message is not a JSON object".to_string(),
            })?;

        let got_version = object
            .get("protocol_version")
            .ok_or_else(|| ProtocolError::MalformedLine {
                reason: "missing `protocol_version`".to_string(),
            })?
            .as_u64()
            .ok_or_else(|| ProtocolError::MalformedLine {
                reason: "`protocol_version` is not a non-negative integer".to_string(),
            })?;
        if got_version != u64::from(PROTOCOL_VERSION) {
            return Err(ProtocolError::UnsupportedVersion {
                got: got_version,
                expected: PROTOCOL_VERSION,
            });
        }

        let id = object
            .get("id")
            .ok_or_else(|| ProtocolError::MalformedLine {
                reason: "missing `id`".to_string(),
            })?
            .as_str()
            .ok_or_else(|| ProtocolError::MalformedLine {
                reason: "`id` is not a string".to_string(),
            })?
            .to_string();
        if id.is_empty() {
            return Err(ProtocolError::MalformedLine {
                reason: "`id` is empty".to_string(),
            });
        }

        let message_type = object
            .get("type")
            .ok_or_else(|| ProtocolError::MalformedLine {
                reason: "missing `type`".to_string(),
            })?
            .as_str()
            .ok_or_else(|| ProtocolError::MalformedLine {
                reason: "`type` is not a string".to_string(),
            })?
            .to_string();

        if !host_to_rust::ALL.contains(&message_type.as_str()) {
            return Err(ProtocolError::UnknownMessageType(message_type.to_string()));
        }

        let payload = serde_json::Value::Object(
            object
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "protocol_version" | "id" | "type"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );

        Ok(Envelope {
            protocol_version: PROTOCOL_VERSION,
            id,
            message_type: message_type.to_string(),
            payload,
        })
    }
}

/// Decode an envelope into a typed [`HostEvent`].
pub fn parse_host_event(envelope: Envelope) -> Result<HostEvent, ProtocolError> {
    let ty = envelope.message_type.clone();
    let invalid = move |reason: String| ProtocolError::InvalidPayload {
        message_type: ty.clone(),
        reason,
    };

    match envelope.message_type.as_str() {
        host_to_rust::HOST_READY => {
            let ready: HostReady =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            Ok(HostEvent::HostReady(ready))
        }
        host_to_rust::REVIEWER_STARTED => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                review_id: String,
                reviewer_id: String,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_non_empty(&payload.review_id, "review_id", &invalid)?;
            require_non_empty(&payload.reviewer_id, "reviewer_id", &invalid)?;
            Ok(HostEvent::ReviewerStarted {
                review_id: payload.review_id,
                reviewer_id: payload.reviewer_id,
            })
        }
        host_to_rust::REVIEWER_COMPLETED => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                review_id: String,
                reviewer_id: String,
                report: ReviewerReport,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_non_empty(&payload.review_id, "review_id", &invalid)?;
            require_non_empty(&payload.reviewer_id, "reviewer_id", &invalid)?;
            if payload.report.reviewer_id != payload.reviewer_id {
                return Err(invalid(format!(
                    "report reviewer_id `{}` does not match message reviewer_id `{}`",
                    payload.report.reviewer_id, payload.reviewer_id
                )));
            }
            Ok(HostEvent::ReviewerCompleted {
                review_id: payload.review_id,
                reviewer_id: payload.reviewer_id,
                report: payload.report,
            })
        }
        host_to_rust::REVIEWER_FAILED => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                review_id: String,
                reviewer_id: String,
                error: HostErrorInfo,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_non_empty(&payload.review_id, "review_id", &invalid)?;
            require_non_empty(&payload.reviewer_id, "reviewer_id", &invalid)?;
            require_valid_error(&payload.error, &invalid)?;
            Ok(HostEvent::ReviewerFailed {
                review_id: payload.review_id,
                reviewer_id: payload.reviewer_id,
                error: payload.error,
            })
        }
        host_to_rust::TOOL_REQUEST => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                review_id: String,
                reviewer_id: String,
                tool: ReviewerCapability,
                arguments: serde_json::Value,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_non_empty(&payload.review_id, "review_id", &invalid)?;
            require_non_empty(&payload.reviewer_id, "reviewer_id", &invalid)?;
            if !payload.arguments.is_object() {
                return Err(invalid("`arguments` must be a JSON object".to_string()));
            }
            Ok(HostEvent::ToolRequest {
                id: envelope.id,
                payload: ToolRequestPayload {
                    review_id: payload.review_id,
                    reviewer_id: payload.reviewer_id,
                    tool: payload.tool,
                    arguments: payload.arguments,
                },
            })
        }
        host_to_rust::HOST_ERROR => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                error: HostErrorInfo,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_valid_error(&payload.error, &invalid)?;
            Ok(HostEvent::HostError(payload.error))
        }
        host_to_rust::FINDING_EQUIVALENCE_COMPLETED => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                check_id: String,
                verdict: EquivalenceVerdict,
                #[serde(default)]
                error: Option<HostErrorInfo>,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_non_empty(&payload.check_id, "check_id", &invalid)?;
            if let Some(error) = &payload.error {
                require_valid_error(error, &invalid)?;
                if payload.verdict != EquivalenceVerdict::Uncertain {
                    return Err(invalid(
                        "`error` is only permitted with the `uncertain` verdict".to_string(),
                    ));
                }
            }
            Ok(HostEvent::FindingEquivalenceCompleted {
                check_id: payload.check_id,
                verdict: payload.verdict,
                error: payload.error,
            })
        }
        host_to_rust::DOMAIN_DESCRIPTION_COMPLETED => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                description_id: String,
                #[serde(default)]
                description: Option<DynamicDomainDescription>,
                #[serde(default)]
                error: Option<HostErrorInfo>,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_non_empty(&payload.description_id, "description_id", &invalid)?;
            match (&payload.description, &payload.error) {
                (Some(_), Some(_)) => {
                    return Err(invalid(
                        "`error` is only permitted when no `description` was produced".to_string(),
                    ));
                }
                (None, Some(error)) => {
                    require_valid_error(error, &invalid)?;
                }
                (Some(description), None) => {
                    if description.domain_label.trim().is_empty() {
                        return Err(invalid("`domain_label` must be non-empty".to_string()));
                    }
                    for (list, field) in [
                        (&description.focus_terms, "focus_terms"),
                        (&description.risks_to_check, "risks_to_check"),
                        (
                            &description.relevant_practice_tags,
                            "relevant_practice_tags",
                        ),
                    ] {
                        if list.len() > MAX_DOMAIN_DESCRIPTION_TERMS {
                            return Err(invalid(format!(
                                "`{field}` exceeds {MAX_DOMAIN_DESCRIPTION_TERMS} entries"
                            )));
                        }
                    }
                }
                (None, None) => {
                    return Err(invalid(
                        "either `description` or `error` must be present".to_string(),
                    ));
                }
            }
            Ok(HostEvent::DomainDescriptionCompleted {
                description_id: payload.description_id,
                description: payload.description,
                error: payload.error,
            })
        }
        host_to_rust::FACT_VERIFICATION_COMPLETED => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Payload {
                verification_id: String,
                verdict: FactVerificationVerdict,
                #[serde(default)]
                error: Option<HostErrorInfo>,
            }
            let payload: Payload =
                serde_json::from_value(envelope.payload).map_err(|err| invalid(err.to_string()))?;
            require_non_empty(&payload.verification_id, "verification_id", &invalid)?;
            if let Some(error) = &payload.error {
                require_valid_error(error, &invalid)?;
                if payload.verdict != FactVerificationVerdict::Insufficient {
                    return Err(invalid(
                        "`error` is only permitted with the `insufficient` verdict".to_string(),
                    ));
                }
            }
            Ok(HostEvent::FactVerificationCompleted {
                verification_id: payload.verification_id,
                verdict: payload.verdict,
                error: payload.error,
            })
        }
        other => Err(ProtocolError::UnsupportedMessageType(other.to_string())),
    }
}

fn require_non_empty(
    value: &str,
    field: &str,
    invalid: &dyn Fn(String) -> ProtocolError,
) -> Result<(), ProtocolError> {
    if value.is_empty() {
        return Err(invalid(format!("`{field}` must be non-empty")));
    }
    Ok(())
}

fn require_valid_error(
    error: &HostErrorInfo,
    invalid: &dyn Fn(String) -> ProtocolError,
) -> Result<(), ProtocolError> {
    if error.code.is_empty() || error.message.is_empty() {
        return Err(invalid(
            "error `code` and `message` must be non-empty".to_string(),
        ));
    }
    Ok(())
}

/// Serialize one complete Rust-to-host message line: envelope fields merged
/// with the flattened payload fields.
fn encode_message(message_type: &str, id: &str, payload: &impl Serialize) -> String {
    let mut object = serde_json::to_value(payload).expect("message payloads serialize");
    let map = object
        .as_object_mut()
        .expect("message payloads serialize to objects");
    map.insert(
        "protocol_version".to_string(),
        serde_json::json!(PROTOCOL_VERSION),
    );
    map.insert("id".to_string(), serde_json::json!(id));
    map.insert("type".to_string(), serde_json::json!(message_type));
    serde_json::to_string(&object).expect("message lines serialize")
}

/// Verdict of a narrow finding-equivalence check (plan section 38.5).
///
/// Only `Same` findings may be grouped. `Uncertain` is also the forced
/// verdict of a check whose session failed: uncertainty produces less
/// automation, never guessed grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EquivalenceVerdict {
    /// The two findings describe the same underlying issue.
    Same,
    /// The two findings describe different issues.
    Different,
    /// The relationship could not be determined.
    Uncertain,
}

/// One finding of an equivalence check, with its reviewer provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EquivalenceFinding {
    /// Reviewer that produced the finding.
    pub reviewer_id: String,
    /// The finding itself.
    pub finding: ReviewerFinding,
}

/// `start_finding_equivalence_check` payload: exactly two validated
/// findings and the explicit bounds of the narrow session that compares
/// them. Rust owns the check identity, the model selection, and the
/// timeout; the host executes exactly what it is given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindingEquivalenceCheckPayload {
    /// Correlation identifier of this check (assigned by Rust).
    pub check_id: String,
    /// Session timeout in milliseconds.
    pub timeout_ms: u64,
    /// Explicitly configured model profile for the session.
    pub model_profile: String,
    /// Explicit thinking level for the session.
    pub thinking_level: crate::domain::model::ThinkingLevel,
    /// First finding of the comparison.
    pub finding_a: EquivalenceFinding,
    /// Second finding of the comparison.
    pub finding_b: EquivalenceFinding,
}

/// The resolved outcome of one equivalence check.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingEquivalenceOutcome {
    /// Correlation identifier of the check.
    pub check_id: String,
    /// The verdict.
    pub verdict: EquivalenceVerdict,
    /// Why the verdict fell back to `uncertain`, when the check's
    /// session failed.
    pub error: Option<HostErrorInfo>,
}

/// Maximum number of entries each list of a
/// [`DynamicDomainDescription`] may carry on the wire.
pub const MAX_DOMAIN_DESCRIPTION_TERMS: usize = 16;

/// `start_domain_description` payload: the deterministic request
/// descriptors a constrained non-juror session may use to describe the
/// domain, plus the explicit bounds of that session (plan section 19).
/// Rust owns the job identity, the model selection, and the timeout; the
/// description can never add tools, providers, or endpoint trust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainDescriptionPayload {
    /// Correlation identifier of this job (assigned by Rust).
    pub description_id: String,
    /// Session timeout in milliseconds.
    pub timeout_ms: u64,
    /// Explicitly configured model profile for the session.
    pub model_profile: String,
    /// Explicit thinking level for the session.
    pub thinking_level: crate::domain::model::ThinkingLevel,
    /// Free-text request descriptors of the review (plan headings, diff
    /// subjects, …).
    pub descriptors: Vec<String>,
    /// Repository-relative paths changed by the request, when known.
    pub changed_paths: Vec<String>,
    /// Lowercase index language ids detected in the change/snapshot.
    pub languages: Vec<String>,
}

/// The resolved outcome of one dynamic-domain description job.
#[derive(Debug, Clone, PartialEq)]
pub struct DomainDescriptionOutcome {
    /// Correlation identifier of the job.
    pub description_id: String,
    /// The description; `None` when the session failed.
    pub description: Option<DynamicDomainDescription>,
    /// Why the description fell back, when its session failed.
    pub error: Option<HostErrorInfo>,
}

/// `start_fact_verification` payload (plan section 47 step 2): the
/// exact claim and the exact evidence excerpts one fresh verifier
/// session must judge, plus the explicit bounds of that session.
/// Rust owns the job identity, the model selection, and the timeout;
/// the verifier is always a separate session from the reviewer that
/// proposed the candidate, and it receives no tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FactVerificationPayload {
    /// Correlation identifier of this job (assigned by Rust).
    pub verification_id: String,
    /// Session timeout in milliseconds.
    pub timeout_ms: u64,
    /// Explicitly configured model profile for the session.
    pub model_profile: String,
    /// Explicit thinking level for the session.
    pub thinking_level: crate::domain::model::ThinkingLevel,
    /// The exact claim under verification.
    pub claim: String,
    /// The exact evidence excerpts backing the claim.
    pub evidence: Vec<EvidenceRef>,
}

/// A Rust-to-host `start_fact_verification` message.
#[derive(Debug, Clone)]
pub struct StartFactVerificationMessage {
    /// Envelope id.
    pub id: String,
    /// Payload: the verification job to execute.
    pub payload: FactVerificationPayload,
}

impl StartFactVerificationMessage {
    /// Build the message for `payload` with an explicit envelope id.
    pub fn new(id: String, payload: FactVerificationPayload) -> Self {
        StartFactVerificationMessage { id, payload }
    }

    /// Serialize to one JSONL line.
    pub fn to_line(&self) -> String {
        encode_message(
            rust_to_host::START_FACT_VERIFICATION,
            &self.id,
            &self.payload,
        )
    }
}

/// A Rust-to-host `start_domain_description` message.
#[derive(Debug, Clone)]
pub struct StartDomainDescriptionMessage {
    /// Envelope id.
    pub id: String,
    /// Payload: the description job to execute.
    pub payload: DomainDescriptionPayload,
}

impl StartDomainDescriptionMessage {
    /// Build the message for `payload` with an explicit envelope id.
    pub fn new(id: String, payload: DomainDescriptionPayload) -> Self {
        StartDomainDescriptionMessage { id, payload }
    }

    /// Serialize to one JSONL line.
    pub fn to_line(&self) -> String {
        encode_message(
            rust_to_host::START_DOMAIN_DESCRIPTION,
            &self.id,
            &self.payload,
        )
    }
}

/// A Rust-to-host `host_config` message.
#[derive(Debug, Clone)]
pub struct HostConfigMessage {
    /// Envelope id.
    pub id: String,
    /// Payload: the model profiles the host may resolve.
    pub payload: HostConfigPayload,
}

impl HostConfigMessage {
    /// Build the message for `payload` with an explicit envelope id.
    pub fn new(id: String, payload: HostConfigPayload) -> Self {
        HostConfigMessage { id, payload }
    }

    /// Serialize to one JSONL line.
    pub fn to_line(&self) -> String {
        encode_message(rust_to_host::HOST_CONFIG, &self.id, &self.payload)
    }
}

/// A Rust-to-host `start_review` message.
#[derive(Debug, Clone)]
pub struct StartReviewMessage {
    /// Envelope id.
    pub id: String,
    /// Payload: the review to execute.
    pub payload: StartReviewPayload,
}

impl StartReviewMessage {
    /// Build the message for `payload` with an explicit envelope id.
    pub fn new(id: String, payload: StartReviewPayload) -> Self {
        StartReviewMessage { id, payload }
    }

    /// Serialize to one JSONL line.
    pub fn to_line(&self) -> String {
        encode_message(rust_to_host::START_REVIEW, &self.id, &self.payload)
    }
}

/// A Rust-to-host `tool_response` message.
#[derive(Debug, Clone)]
pub struct ToolResponseMessage {
    /// Envelope id.
    pub id: String,
    /// Payload: the answer to one `tool_request`.
    pub payload: ToolResponsePayload,
}

impl ToolResponseMessage {
    /// Build the message for `payload` with an explicit envelope id.
    pub fn new(id: String, payload: ToolResponsePayload) -> Self {
        ToolResponseMessage { id, payload }
    }

    /// Serialize to one JSONL line.
    pub fn to_line(&self) -> String {
        encode_message(rust_to_host::TOOL_RESPONSE, &self.id, &self.payload)
    }
}

/// A Rust-to-host `start_finding_equivalence_check` message.
#[derive(Debug, Clone)]
pub struct StartFindingEquivalenceCheckMessage {
    /// Envelope id.
    pub id: String,
    /// Payload: the check to execute.
    pub payload: FindingEquivalenceCheckPayload,
}

impl StartFindingEquivalenceCheckMessage {
    /// Build the message for `payload` with an explicit envelope id.
    pub fn new(id: String, payload: FindingEquivalenceCheckPayload) -> Self {
        StartFindingEquivalenceCheckMessage { id, payload }
    }

    /// Serialize to one JSONL line.
    pub fn to_line(&self) -> String {
        encode_message(
            rust_to_host::START_FINDING_EQUIVALENCE_CHECK,
            &self.id,
            &self.payload,
        )
    }
}

/// A Rust-to-host `cancel` message.
#[derive(Debug, Clone)]
pub struct CancelMessage {
    /// Envelope id.
    pub id: String,
    /// Payload: what to cancel.
    pub payload: CancelPayload,
}

impl CancelMessage {
    /// Build the message for `payload` with an explicit envelope id.
    pub fn new(id: String, payload: CancelPayload) -> Self {
        CancelMessage { id, payload }
    }

    /// Serialize to one JSONL line.
    pub fn to_line(&self) -> String {
        encode_message(rust_to_host::CANCEL, &self.id, &self.payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_frozen_at_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn vocabularies_match_the_frozen_plan_section_21() {
        assert_eq!(
            rust_to_host::ALL,
            [
                "host_config",
                "start_review",
                "start_fact_verification",
                "start_fact_synthesis",
                "start_domain_description",
                "start_finding_equivalence_check",
                "tool_response",
                "cancel",
            ]
        );
        assert_eq!(
            host_to_rust::ALL,
            [
                "host_ready",
                "reviewer_started",
                "reviewer_completed",
                "reviewer_failed",
                "tool_request",
                "fact_verification_completed",
                "fact_synthesis_completed",
                "domain_description_completed",
                "finding_equivalence_completed",
                "host_error",
            ]
        );
    }

    #[test]
    fn envelope_parses_host_ready() {
        let line = r#"{"protocol_version":1,"id":"h-1","type":"host_ready","runtime":"fake"}"#;
        let envelope = Envelope::parse_host_line(line).unwrap();
        assert_eq!(envelope.protocol_version, 1);
        assert_eq!(envelope.id, "h-1");
        assert_eq!(envelope.message_type, "host_ready");
        let event = parse_host_event(envelope).unwrap();
        assert_eq!(
            event,
            HostEvent::HostReady(HostReady {
                runtime: HostRuntime::Fake
            })
        );
    }

    #[test]
    fn envelope_rejects_malformed_lines() {
        for line in ["", "not json", "[]", "42", "\"text\"", "{\"id\":\"x\"}"] {
            let err = Envelope::parse_host_line(line).unwrap_err();
            assert!(
                matches!(err, ProtocolError::MalformedLine { .. }),
                "expected MalformedLine for {line:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn envelope_rejects_wrong_versions_and_bad_common_fields() {
        let err = Envelope::parse_host_line(
            r#"{"protocol_version":2,"id":"h-1","type":"host_ready","runtime":"fake"}"#,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ProtocolError::UnsupportedVersion {
                got: 2,
                expected: 1
            }
        );

        let err = Envelope::parse_host_line(
            r#"{"protocol_version":1,"type":"host_ready","runtime":"fake"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedLine { .. }));

        let err = Envelope::parse_host_line(
            r#"{"protocol_version":1,"id":"","type":"host_ready","runtime":"fake"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedLine { .. }));

        let err = Envelope::parse_host_line(
            r#"{"protocol_version":"1","id":"h-1","type":"host_ready","runtime":"fake"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, ProtocolError::MalformedLine { .. }));
    }

    #[test]
    fn unknown_and_unsupported_types_are_typed_errors() {
        let err = Envelope::parse_host_line(
            r#"{"protocol_version":1,"id":"h-1","type":"banana","runtime":"fake"}"#,
        )
        .unwrap_err();
        assert_eq!(err, ProtocolError::UnknownMessageType("banana".to_string()));

        let envelope = Envelope::parse_host_line(
            r#"{"protocol_version":1,"id":"h-1","type":"fact_synthesis_completed"}"#,
        )
        .unwrap();
        let err = parse_host_event(envelope).unwrap_err();
        assert_eq!(
            err,
            ProtocolError::UnsupportedMessageType("fact_synthesis_completed".to_string())
        );
    }

    #[test]
    fn reviewer_completed_requires_matching_report_identity() {
        let report = serde_json::json!({
            "reviewer_id": "quality",
            "summary": "ok",
            "findings": [],
            "candidate_project_facts": [],
            "model_metadata": {},
            "duration_ms": 10
        });
        let line = format!(
            r#"{{"protocol_version":1,"id":"h-1","type":"reviewer_completed","review_id":"r","reviewer_id":"quality","report":{report}}}"#
        );
        let event = parse_host_event(Envelope::parse_host_line(&line).unwrap()).unwrap();
        match event {
            HostEvent::ReviewerCompleted {
                review_id,
                reviewer_id,
                report,
            } => {
                assert_eq!(review_id, "r");
                assert_eq!(reviewer_id, "quality");
                assert_eq!(report.reviewer_id, "quality");
            }
            other => panic!("expected ReviewerCompleted, got {other:?}"),
        }

        // Report identity must match the message identity.
        let mismatched = line.replace(
            "\"reviewer_id\":\"quality\",\"report",
            "\"reviewer_id\":\"security\",\"report",
        );
        let envelope = Envelope::parse_host_line(&mismatched).unwrap();
        let err = parse_host_event(envelope).unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidPayload { .. }));
    }

    #[test]
    fn tool_request_parses_and_rejects_unknown_tools() {
        let line = r#"{"protocol_version":1,"id":"t-1","type":"tool_request","review_id":"r","reviewer_id":"quality","tool":"source_lookup","arguments":{"path":"src/main.rs"}}"#;
        let event = parse_host_event(Envelope::parse_host_line(line).unwrap()).unwrap();
        match event {
            HostEvent::ToolRequest { id, payload } => {
                assert_eq!(id, "t-1");
                assert_eq!(payload.review_id, "r");
                assert_eq!(payload.reviewer_id, "quality");
                assert_eq!(payload.tool, ReviewerCapability::SourceLookup);
                assert!(payload.arguments.is_object());
            }
            other => panic!("expected ToolRequest, got {other:?}"),
        }

        let bad = r#"{"protocol_version":1,"id":"t-1","type":"tool_request","review_id":"r","reviewer_id":"quality","tool":"shell","arguments":{}}"#;
        let err = parse_host_event(Envelope::parse_host_line(bad).unwrap()).unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidPayload { .. }));
    }

    #[test]
    fn empty_identifiers_and_errors_are_rejected() {
        let bad = r#"{"protocol_version":1,"id":"h-1","type":"reviewer_started","review_id":"","reviewer_id":"quality"}"#;
        let err = parse_host_event(Envelope::parse_host_line(bad).unwrap()).unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidPayload { .. }));

        let bad = r#"{"protocol_version":1,"id":"h-1","type":"host_error","error":{"code":"","message":""}}"#;
        let err = parse_host_event(Envelope::parse_host_line(bad).unwrap()).unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidPayload { .. }));
    }

    #[test]
    fn host_config_serializes_profiles_and_maps_from_config() {
        let mut profiles = std::collections::BTreeMap::new();
        profiles.insert(
            "standard".to_string(),
            ModelProfile {
                model: "acme/local-small".to_string(),
                endpoint: Some("http://127.0.0.1:8080/v1".to_string()),
                thinking_level: Some(crate::domain::model::ThinkingLevel::High),
            },
        );
        profiles.insert(
            "strong".to_string(),
            ModelProfile {
                model: "acme/local-large".to_string(),
                endpoint: None,
                thinking_level: None,
            },
        );

        let payload = HostConfigPayload::from_config_profiles(&profiles);
        assert!(payload.contains("standard"));
        assert!(!payload.contains("fast"));
        // The profile's thinking level never leaks into the model table:
        // thinking is explicit per execution spec.
        let json = serde_json::to_string(&payload).unwrap();
        assert!(!json.contains("thinking_level"));
        assert!(json.contains("acme/local-small"));
        assert!(json.contains("http://127.0.0.1:8080/v1"));
        assert!(!json.contains("\"endpoint\":null"));

        let line = HostConfigMessage::new("m-0".to_string(), payload.clone()).to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["type"], "host_config");
        assert_eq!(value["id"], "m-0");
        assert_eq!(value["models"]["strong"]["model"], "acme/local-large");
        assert!(value["models"]["strong"].get("endpoint").is_none());

        // Strict roundtrip: wrong shape is rejected.
        let back: HostConfigPayload = serde_json::from_value(value).unwrap();
        assert_eq!(back, payload);
        assert!(serde_json::from_str::<HostConfigPayload>("{\"profiles\":{}}").is_err());
    }

    #[test]
    fn finding_equivalence_messages_roundtrip_strictly() {
        let finding = serde_json::json!({
            "id": "q-1",
            "impact_class": "correctness",
            "proposed_severity": "info",
            "category": "logic",
            "title": "Off by one",
            "claim": "Drops the last element.",
            "explanation": "Wraparound uses >.",
            "recommendation": "Use >=.",
            "evidence": []
        });
        let payload = FindingEquivalenceCheckPayload {
            check_id: "chk-1".to_string(),
            timeout_ms: 30_000,
            model_profile: "standard".to_string(),
            thinking_level: crate::domain::model::ThinkingLevel::Low,
            finding_a: EquivalenceFinding {
                reviewer_id: "quality".to_string(),
                finding: serde_json::from_value(finding.clone()).unwrap(),
            },
            finding_b: EquivalenceFinding {
                reviewer_id: "concurrency".to_string(),
                finding: serde_json::from_value(finding).unwrap(),
            },
        };
        let line = StartFindingEquivalenceCheckMessage::new("m-4".to_string(), payload).to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["type"], "start_finding_equivalence_check");
        assert_eq!(value["id"], "m-4");
        assert_eq!(value["check_id"], "chk-1");
        assert_eq!(value["timeout_ms"], 30_000);
        assert_eq!(value["model_profile"], "standard");
        assert_eq!(value["thinking_level"], "low");
        assert_eq!(value["finding_a"]["reviewer_id"], "quality");
        assert!(!line.contains('\n'), "messages must be single lines");
        let back: FindingEquivalenceCheckPayload = serde_json::from_value(value).unwrap();
        assert_eq!(back.check_id, "chk-1");

        // Completion parses for every verdict without an error.
        for (verdict, name) in [
            (EquivalenceVerdict::Same, "same"),
            (EquivalenceVerdict::Different, "different"),
            (EquivalenceVerdict::Uncertain, "uncertain"),
        ] {
            let line = format!(
                r#"{{"protocol_version":1,"id":"h-9","type":"finding_equivalence_completed","check_id":"chk-1","verdict":"{name}"}}"#
            );
            let event = parse_host_event(Envelope::parse_host_line(&line).unwrap()).unwrap();
            assert_eq!(
                event,
                HostEvent::FindingEquivalenceCompleted {
                    check_id: "chk-1".to_string(),
                    verdict,
                    error: None,
                }
            );
        }

        // An error is only permitted with the uncertain verdict.
        let fallback = r#"{"protocol_version":1,"id":"h-9","type":"finding_equivalence_completed","check_id":"chk-1","verdict":"uncertain","error":{"code":"session_timeout","message":"timed out"}}"#;
        let event = parse_host_event(Envelope::parse_host_line(fallback).unwrap()).unwrap();
        assert!(matches!(
            event,
            HostEvent::FindingEquivalenceCompleted { error: Some(_), .. }
        ));
        let inconsistent = fallback.replace("\"uncertain\"", "\"same\"");
        let err = parse_host_event(Envelope::parse_host_line(&inconsistent).unwrap()).unwrap_err();
        assert!(matches!(err, ProtocolError::InvalidPayload { .. }));

        // Unknown verdicts and empty check ids are rejected.
        for bad in [
            r#"{"protocol_version":1,"id":"h-9","type":"finding_equivalence_completed","check_id":"chk-1","verdict":"maybe"}"#,
            r#"{"protocol_version":1,"id":"h-9","type":"finding_equivalence_completed","check_id":"","verdict":"same"}"#,
        ] {
            let err = parse_host_event(Envelope::parse_host_line(bad).unwrap()).unwrap_err();
            assert!(
                matches!(err, ProtocolError::InvalidPayload { .. }),
                "{err:?}"
            );
        }
    }

    #[test]
    fn domain_description_messages_roundtrip_strictly() {
        let payload = DomainDescriptionPayload {
            description_id: "desc-1".to_string(),
            timeout_ms: 60_000,
            model_profile: "standard".to_string(),
            thinking_level: crate::domain::model::ThinkingLevel::Low,
            descriptors: vec!["Audio pipeline rewrite".to_string()],
            changed_paths: Vec::new(),
            languages: vec!["rust".to_string()],
        };
        let line = StartDomainDescriptionMessage::new("m-5".to_string(), payload.clone()).to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["type"], "start_domain_description");
        assert_eq!(value["id"], "m-5");
        assert_eq!(value["description_id"], "desc-1");
        assert_eq!(value["timeout_ms"], 60_000);
        let back: DomainDescriptionPayload = serde_json::from_value(value).unwrap();
        assert_eq!(back, payload);
        assert!(!line.contains('\n'), "messages must be single lines");

        // A successful completion carries the description and no error.
        let description = serde_json::json!({
            "domain_label": "audio dsp",
            "focus_terms": ["sampling rate"],
            "risks_to_check": ["denormals"],
            "relevant_practice_tags": ["realtime"],
        });
        let line = format!(
            r#"{{"protocol_version":1,"id":"h-10","type":"domain_description_completed","description_id":"desc-1","description":{description}}}"#
        );
        let event = parse_host_event(Envelope::parse_host_line(&line).unwrap()).unwrap();
        match event {
            HostEvent::DomainDescriptionCompleted {
                description_id,
                description,
                error,
            } => {
                assert_eq!(description_id, "desc-1");
                assert!(error.is_none());
                assert_eq!(
                    description.expect("description present").domain_label,
                    "audio dsp"
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // A failed completion carries an error and no description.
        let line = r#"{"protocol_version":1,"id":"h-10","type":"domain_description_completed","description_id":"desc-1","error":{"code":"session_timeout","message":"timed out"}}"#;
        let event = parse_host_event(Envelope::parse_host_line(line).unwrap()).unwrap();
        match event {
            HostEvent::DomainDescriptionCompleted {
                description, error, ..
            } => {
                assert!(description.is_none());
                assert!(error.is_some());
            }
            other => panic!("unexpected event: {other:?}"),
        }

        // Malformed completions are rejected: neither field, both fields,
        // empty label, oversized lists, unknown fields.
        for bad in [
            r#"{"protocol_version":1,"id":"h","type":"domain_description_completed","description_id":"d"}"#.to_string(),
            r#"{"protocol_version":1,"id":"h","type":"domain_description_completed","description_id":"d","description":{"domain_label":"x","focus_terms":[],"risks_to_check":[],"relevant_practice_tags":[]},"error":{"code":"c","message":"m"}}"#.to_string(),
            r#"{"protocol_version":1,"id":"h","type":"domain_description_completed","description_id":"d","description":{"domain_label":"  ","focus_terms":[],"risks_to_check":[],"relevant_practice_tags":[]}}"#.to_string(),
            {
                let mut oversized = serde_json::json!({
                    "protocol_version": 1, "id": "h", "type": "domain_description_completed",
                    "description_id": "d",
                    "description": {
                        "domain_label": "x",
                        "focus_terms": [],
                        "risks_to_check": [],
                        "relevant_practice_tags": [],
                    },
                });
                let tags: Vec<String> = (0..=MAX_DOMAIN_DESCRIPTION_TERMS)
                    .map(|index| format!("tag{index}"))
                    .collect();
                oversized["description"]["relevant_practice_tags"] =
                    serde_json::json!(tags);
                oversized.to_string()
            },
            r#"{"protocol_version":1,"id":"h","type":"domain_description_completed","description_id":"d","description":{"domain_label":"x","focus_terms":[],"risks_to_check":[],"relevant_practice_tags":[],"tools":["shell"]}}"#.to_string(),
        ] {
            let err = parse_host_event(Envelope::parse_host_line(&bad).unwrap()).unwrap_err();
            assert!(matches!(err, ProtocolError::InvalidPayload { .. }), "{err:?}");
        }
    }

    #[test]
    fn fact_verification_messages_roundtrip_strictly() {
        let evidence = serde_json::json!({
            "kind": "source",
            "path": "src/queue.rs",
            "line_start": 1,
            "line_end": 3,
            "excerpt": "pub struct Queue {"
        });
        let payload = FactVerificationPayload {
            verification_id: "ver-1".to_string(),
            timeout_ms: 120_000,
            model_profile: "standard".to_string(),
            thinking_level: crate::domain::model::ThinkingLevel::Medium,
            claim: "The sensor queue is bounded to 32 slots.".to_string(),
            evidence: vec![serde_json::from_value(evidence).unwrap()],
        };
        let line = StartFactVerificationMessage::new("m-6".to_string(), payload.clone()).to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["type"], "start_fact_verification");
        assert_eq!(value["id"], "m-6");
        assert_eq!(value["verification_id"], "ver-1");
        assert_eq!(value["timeout_ms"], 120_000);
        assert_eq!(value["model_profile"], "standard");
        assert_eq!(value["thinking_level"], "medium");
        assert_eq!(value["claim"], "The sensor queue is bounded to 32 slots.");
        assert_eq!(value["evidence"][0]["path"], "src/queue.rs");
        assert!(!line.contains('\n'), "messages must be single lines");
        let back: FactVerificationPayload = serde_json::from_value(value).unwrap();
        assert_eq!(back, payload);

        // Completions parse for every verdict without an error.
        for (verdict, name) in [
            (FactVerificationVerdict::Supported, "supported"),
            (FactVerificationVerdict::Contradicted, "contradicted"),
            (FactVerificationVerdict::Insufficient, "insufficient"),
        ] {
            let line = format!(
                r#"{{"protocol_version":1,"id":"h-11","type":"fact_verification_completed","verification_id":"ver-1","verdict":"{name}"}}"#
            );
            let event = parse_host_event(Envelope::parse_host_line(&line).unwrap()).unwrap();
            assert_eq!(
                event,
                HostEvent::FactVerificationCompleted {
                    verification_id: "ver-1".to_string(),
                    verdict,
                    error: None,
                }
            );
        }

        // An error is only permitted with the insufficient verdict (a
        // failed verifier session must never look like support or
        // contradiction).
        let failed = r#"{"protocol_version":1,"id":"h-11","type":"fact_verification_completed","verification_id":"ver-1","verdict":"insufficient","error":{"code":"session_timeout","message":"timed out"}}"#;
        let event = parse_host_event(Envelope::parse_host_line(failed).unwrap()).unwrap();
        assert!(matches!(
            event,
            HostEvent::FactVerificationCompleted { error: Some(_), .. }
        ));
        for corrupted in [
            failed.replace("\"insufficient\"", "\"supported\""),
            failed.replace("\"insufficient\"", "\"contradicted\""),
        ] {
            let err = parse_host_event(Envelope::parse_host_line(&corrupted).unwrap()).unwrap_err();
            assert!(
                matches!(err, ProtocolError::InvalidPayload { .. }),
                "{err:?}"
            );
        }

        // Unknown verdicts and empty ids are rejected.
        for bad in [
            r#"{"protocol_version":1,"id":"h","type":"fact_verification_completed","verification_id":"v","verdict":"uncertain"}"#,
            r#"{"protocol_version":1,"id":"h","type":"fact_verification_completed","verification_id":"","verdict":"supported"}"#,
            r#"{"protocol_version":1,"id":"h","type":"fact_verification_completed","verification_id":"v","verdict":"supported","extra":1}"#,
        ] {
            let err = parse_host_event(Envelope::parse_host_line(bad).unwrap()).unwrap_err();
            assert!(
                matches!(err, ProtocolError::InvalidPayload { .. }),
                "{err:?}"
            );
        }
    }

    #[test]
    fn outbound_messages_carry_envelope_and_payload() {
        let payload = StartReviewPayload {
            review_id: "01J8Z".to_string(),
            kind: ReviewKind::Plan,
            reviewers: Vec::new(),
        };
        let line = StartReviewMessage::new("m-1".to_string(), payload).to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["protocol_version"], 1);
        assert_eq!(value["type"], "start_review");
        assert_eq!(value["id"], "m-1");
        assert_eq!(value["review_id"], "01J8Z");
        assert_eq!(value["kind"], "plan");
        assert!(value["reviewers"].is_array());
        assert!(!line.contains('\n'), "messages must be single lines");

        let line = CancelMessage::new("m-2".to_string(), CancelPayload::all()).to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "cancel");
        assert!(
            value.get("review_id").is_none(),
            "cancel-all must omit review_id"
        );

        let line = CancelMessage::new(
            "m-3".to_string(),
            CancelPayload::review("01J8Z".to_string()),
        )
        .to_line();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["review_id"], "01J8Z");
    }

    #[test]
    fn tool_response_payload_is_wellformed_by_construction() {
        let bundle = ContextBundle::empty();
        let ok = ToolResponsePayload::success("t-1", bundle);
        let json = serde_json::to_string(&ok).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("\"bundle\""));
        assert!(!json.contains("\"error\""));
        let back: ToolResponsePayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ok);

        let err = ToolResponsePayload::failure("t-2", "denied", "not granted");
        let json = serde_json::to_string(&err).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("\"code\":\"denied\""));
        assert!(!json.contains("\"bundle\""));
    }
}
