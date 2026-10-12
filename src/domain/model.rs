//! Model profile and thinking level domain types.
//!
//! Model profiles are named configuration concepts (`standard` and `strong`
//! are required by Nisse operations; `fast` and `strongest` are optional). A
//! profile maps the name to an explicit model identifier plus optional
//! endpoint and thinking settings. Nisse never hard-codes commercial model
//! names.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Reviewer thinking effort requested from the model runtime.
///
/// The vocabulary mirrors pi's `EXTENDED_THINKING_LEVELS` exactly
/// (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`): Nisse
/// session specs carry these strings straight through to the pi SDK,
/// and the pi host derives each model's supported subset from the
/// model catalog's `thinkingLevelMap` (protocol message
/// `model_capabilities`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        })
    }
}

impl ThinkingLevel {
    /// Every pi thinking level, in pi's `EXTENDED_THINKING_LEVELS`
    /// order. The order defines clamp direction (weaker → stronger).
    pub const PI_ORDER: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    /// The position of this level in [`ThinkingLevel::PI_ORDER`].
    fn rank(self) -> usize {
        Self::PI_ORDER
            .iter()
            .position(|level| *level == self)
            .expect("every variant appears in PI_ORDER")
    }

    /// Clamp a requested level to the nearest supported one, using
    /// pi's `clampThinkingLevel` semantics: an exact match is kept;
    /// otherwise the nearest supported level at or above the request
    /// (in `PI_ORDER` order) wins, then the nearest below, then the
    /// first supported. `None` only when `supported` is empty —
    /// callers fail closed on that case.
    pub fn clamp_to_supported(
        requested: ThinkingLevel,
        supported: &BTreeSet<ThinkingLevel>,
    ) -> Option<ThinkingLevel> {
        if supported.contains(&requested) {
            return Some(requested);
        }
        let rank = requested.rank();
        let at_or_above = Self::PI_ORDER[rank..]
            .iter()
            .find(|level| supported.contains(level));
        if let Some(level) = at_or_above {
            return Some(*level);
        }
        let below = Self::PI_ORDER[..rank]
            .iter()
            .rev()
            .find(|level| supported.contains(level));
        below.or_else(|| supported.iter().next()).copied()
    }
}

/// The shared golden fixture for the thinking-level vocabulary, the
/// per-model supported-level derivation, and clamp semantics. The file
/// is consumed by both the Rust suite (`include_str!`) and the pi-host
/// TypeScript suite, so both reimplementations of pi's semantics are
/// pinned to one table. Cases encode pi SDK 0.85.1 behavior and must
/// be re-derived from the SDK source at every pin bump.
pub const THINKING_GOLDEN_FIXTURE: &str =
    include_str!("../../runtime/pi-host/fixtures/thinking-levels.golden.json");

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
    /// Optional explicit thinking level for sessions using this
    /// profile. When set it overrides reviewer-catalog and job-option
    /// defaults for every session on the profile (per-request explicit
    /// levels still win); the effective level is then validated against
    /// the model's supported levels and clamped with a diagnostic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn thinking_level_roundtrip() {
        for level in ThinkingLevel::PI_ORDER {
            let json = serde_json::to_string(&level).unwrap();
            let back: ThinkingLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(level, back);
        }
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::High).unwrap(),
            "\"high\""
        );
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::Xhigh).unwrap(),
            "\"xhigh\""
        );
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::Max).unwrap(),
            "\"max\""
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

    /// Fixture shape mirrored in `runtime/pi-host/fixtures/thinking-levels.golden.json`.
    #[derive(Debug, Deserialize)]
    struct GoldenFixture {
        order: Vec<ThinkingLevel>,
        cases: Vec<GoldenCase>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenCase {
        name: String,
        #[allow(dead_code)]
        reasoning: bool,
        #[serde(default)]
        #[allow(dead_code)]
        thinking_level_map: BTreeMap<String, Option<String>>,
        expected_supported: Vec<ThinkingLevel>,
        clamps: Vec<(ThinkingLevel, ThinkingLevel)>,
    }

    #[test]
    fn clamp_semantics_match_the_shared_golden_fixture() {
        let fixture: GoldenFixture = serde_json::from_str(THINKING_GOLDEN_FIXTURE).unwrap();
        assert_eq!(
            fixture.order,
            ThinkingLevel::PI_ORDER.to_vec(),
            "fixture order must mirror pi's EXTENDED_THINKING_LEVELS"
        );
        assert!(!fixture.cases.is_empty());
        for case in &fixture.cases {
            let supported: BTreeSet<ThinkingLevel> =
                case.expected_supported.iter().copied().collect();
            assert_eq!(
                supported.len(),
                case.expected_supported.len(),
                "fixture case `{}` lists duplicate levels",
                case.name
            );
            for (requested, expected) in &case.clamps {
                assert_eq!(
                    ThinkingLevel::clamp_to_supported(*requested, &supported),
                    Some(*expected),
                    "fixture case `{}`: clamp({requested})",
                    case.name
                );
            }
            // Clamping never leaves the supported set.
            for level in ThinkingLevel::PI_ORDER {
                if let Some(clamped) = ThinkingLevel::clamp_to_supported(level, &supported) {
                    assert!(supported.contains(&clamped));
                }
            }
        }
        // An empty supported set is the only `None`.
        assert_eq!(
            ThinkingLevel::clamp_to_supported(ThinkingLevel::High, &BTreeSet::new()),
            None
        );
    }
}
