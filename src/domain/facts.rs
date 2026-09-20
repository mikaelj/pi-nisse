//! Fact-verification domain types (plan sections 13, 46, and 47).
//!
//! Reviewers may *propose* project facts
//! ([`CandidateProjectFact`](super::report::CandidateProjectFact)), but
//! reviewer output can never create durable knowledge directly. An
//! independent verification session judges the exact claim against the
//! exact evidence excerpts and must answer with one of exactly three
//! verdicts; the deterministic consequences belong to the Rust core
//! (see [`crate::learning::facts`]):
//!
//! - `supported` — the candidate may become a durable supported fact
//!   (only after every deterministic check passed and a contributor
//!   identity is available);
//! - `contradicted` — the candidate is rejected;
//! - `insufficient` — the candidate remains pending local state.
//!
//! There is deliberately no "uncertain" verdict: a verifier that cannot
//! decide *is* insufficient, and insufficient never persists anything.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Verdict of one independent fact-verification session (plan section
/// 47 step 2). The allowed results are exactly these three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactVerificationVerdict {
    /// The evidence supports the exact claim.
    Supported,
    /// The evidence contradicts the claim.
    Contradicted,
    /// The evidence is insufficient to decide.
    Insufficient,
}

impl FactVerificationVerdict {
    /// Frozen serialized name of this verdict.
    pub fn as_str(self) -> &'static str {
        match self {
            FactVerificationVerdict::Supported => "supported",
            FactVerificationVerdict::Contradicted => "contradicted",
            FactVerificationVerdict::Insufficient => "insufficient",
        }
    }
}

impl fmt::Display for FactVerificationVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The resolved outcome of one independent fact-verification job (plan
/// section 13: `FactVerificationResult`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactVerificationResult {
    /// Correlation identifier of the verification job.
    pub verification_id: String,
    /// The verdict.
    pub verdict: FactVerificationVerdict,
    /// Why the verifier could not decide, when its session failed. A
    /// failed session always yields
    /// [`FactVerificationVerdict::Insufficient`]; the core treats it
    /// exactly like a semantic `insufficient` verdict (the candidate
    /// stays pending), so a verifier outage can never persist or
    /// reject anything on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_serialization_is_frozen() {
        for (verdict, name) in [
            (FactVerificationVerdict::Supported, "supported"),
            (FactVerificationVerdict::Contradicted, "contradicted"),
            (FactVerificationVerdict::Insufficient, "insufficient"),
        ] {
            assert_eq!(
                serde_json::to_string(&verdict).unwrap(),
                format!("\"{name}\"")
            );
            assert_eq!(
                serde_json::from_str::<FactVerificationVerdict>(&format!("\"{name}\"")).unwrap(),
                verdict
            );
            assert_eq!(verdict.as_str(), name);
            assert_eq!(verdict.to_string(), name);
        }
        assert!(serde_json::from_str::<FactVerificationVerdict>("\"uncertain\"").is_err());
        assert!(serde_json::from_str::<FactVerificationVerdict>("\"maybe\"").is_err());
        assert!(serde_json::from_str::<FactVerificationVerdict>("\"SUPPORTED\"").is_err());
    }

    #[test]
    fn result_roundtrips_with_and_without_failure() {
        let with_failure = FactVerificationResult {
            verification_id: "01JOB".to_string(),
            verdict: FactVerificationVerdict::Insufficient,
            failure: Some("session_timeout: timed out".to_string()),
        };
        let json = serde_json::to_string(&with_failure).unwrap();
        assert!(json.contains("\"verdict\":\"insufficient\""));
        assert!(json.contains("\"failure\":\"session_timeout: timed out\""));
        assert_eq!(
            serde_json::from_str::<FactVerificationResult>(&json).unwrap(),
            with_failure
        );

        let clean = FactVerificationResult {
            verification_id: "01JOB2".to_string(),
            verdict: FactVerificationVerdict::Supported,
            failure: None,
        };
        let json = serde_json::to_string(&clean).unwrap();
        assert!(!json.contains("failure"), "{json}");
        assert_eq!(
            serde_json::from_str::<FactVerificationResult>(&json).unwrap(),
            clean
        );
    }
}
