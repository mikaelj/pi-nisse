//! Model profile and thinking level domain types.
//!
//! Model profiles are named configuration concepts (`standard` and `strong`
//! are required by Nisse operations; `fast` and `strongest` are optional). A
//! profile maps the name to an explicit model identifier plus optional
//! endpoint and thinking settings. Nisse never hard-codes commercial model
//! names.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Reviewer thinking effort requested from the model runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
        })
    }
}

/// A named model configuration used by reviewer execution specs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    /// Model identifier understood by the provider runtime.
    pub model: String,
    /// Optional provider endpoint URL. When absent, the local runtime's own
    /// provider configuration (trusted local configuration) is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Optional explicit thinking level for sessions using this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_level_roundtrip() {
        for level in [
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ] {
            let json = serde_json::to_string(&level).unwrap();
            let back: ThinkingLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::High).unwrap(),
            "\"high\""
        );
    }

    #[test]
    fn thinking_level_rejects_unknown_variants() {
        assert!(serde_json::from_str::<ThinkingLevel>("\"extreme\"").is_err());
        assert!(serde_json::from_str::<ThinkingLevel>("\"MEDIUM\"").is_err());
    }

    #[test]
    fn model_profile_roundtrip_with_optional_fields() {
        let full = ModelProfile {
            model: "local-model".to_string(),
            endpoint: Some("http://127.0.0.1:8080/v1".to_string()),
            thinking_level: Some(ThinkingLevel::Medium),
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: ModelProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(full, back);
        assert!(json.contains("\"thinking_level\":\"medium\""));

        let minimal = ModelProfile {
            model: "m".to_string(),
            endpoint: None,
            thinking_level: None,
        };
        let json = serde_json::to_string(&minimal).unwrap();
        let back: ModelProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(minimal, back);
        assert!(!json.contains("endpoint"));
        assert!(!json.contains("thinking_level"));
    }

    #[test]
    fn model_profile_rejects_unknown_fields() {
        let json = r#"{"model":"m","temperature":0.2}"#;
        assert!(serde_json::from_str::<ModelProfile>(json).is_err());
        // `model` is required.
        assert!(serde_json::from_str::<ModelProfile>("{}").is_err());
    }
}
