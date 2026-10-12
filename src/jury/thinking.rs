//! Session thinking-level policy over host-reported model capabilities
//! (protocol version 2).
//!
//! The Pi review host resolves every configured model profile against
//! the local Pi model catalog and reports the thinking levels each
//! model supports (pi's `thinkingLevelMap`). This module turns that
//! report plus the trusted model-profile configuration into one
//! policy object that every session spec and semantic-job payload is
//! routed through, so an invalid (model, thinking_level) pair can
//! never be constructed:
//!
//! - the requested level for a session is the explicit per-request
//!   level when present (none of today's callers supply one), else the
//!   profile's configured `thinking_level` override, else the
//!   caller's catalog/option default;
//! - the requested level is then clamped to the model's supported
//!   levels with pi's own clamp semantics, recording a note whenever
//!   an adjustment happened;
//! - profiles whose model could not be resolved (or report an empty
//!   supported set) fail closed, but only for sessions that actually
//!   use them — a broken optional profile never aborts unrelated
//!   reviews; its problem is surfaced through [`ThinkingPolicy::warnings`].

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::model::{ModelProfile, ThinkingLevel};
use crate::jury::protocol::ModelCapabilitiesPayload;

/// The resolved thinking policy for the configured model profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingPolicy {
    entries: BTreeMap<String, PolicyEntry>,
    /// Every profile name the trusted configuration defines (resolved
    /// or not). Sessions on names outside this set were never sent to
    /// the host; they keep the legacy passthrough (the host's
    /// `model_profile_unresolved` session failure still guards them).
    configured: BTreeSet<String>,
    warnings: Vec<String>,
}

/// One profile's policy: the resolved model, its supported levels,
/// and the profile's optional configured level override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEntry {
    /// The resolved `provider/model-id` identifier.
    pub model: String,
    /// Levels the model supports (never empty for usable entries).
    pub supported: BTreeSet<ThinkingLevel>,
    /// The profile's configured `thinking_level` override, if any.
    pub override_level: Option<ThinkingLevel>,
}

impl ThinkingPolicy {
    /// Build the policy from the host's capability report and the
    /// trusted model-profile configuration. Never fails for problems
    /// of profiles — those surface per profile in [`resolve`] and in
    /// [`warnings`](ThinkingPolicy::warnings).
    pub fn build(
        capabilities: &ModelCapabilitiesPayload,
        profiles: &BTreeMap<String, ModelProfile>,
    ) -> Self {
        let mut entries = BTreeMap::new();
        let mut warnings = Vec::new();
        let configured: BTreeSet<String> = profiles.keys().cloned().collect();

        for (name, profile) in profiles {
            if let Some(reason) = capabilities.unresolved.get(name) {
                warnings.push(format!(
                    "model profile `{name}` (model `{model}`) could not be resolved by the pi host: {reason}",
                    model = profile.model
                ));
                continue;
            }
            let Some(entry) = capabilities.profiles.get(name) else {
                warnings.push(format!(
                    "model profile `{name}` (model `{model}`) is missing from the host's model capabilities report",
                    model = profile.model
                ));
                continue;
            };
            if entry.supported_thinking_levels.is_empty() {
                warnings.push(format!(
                    "model profile `{name}` (model `{model}`) reports no supported thinking levels",
                    model = profile.model
                ));
                continue;
            }
            entries.insert(
                name.clone(),
                PolicyEntry {
                    model: entry.model.clone(),
                    supported: entry.supported_thinking_levels.clone(),
                    override_level: profile.thinking_level,
                },
            );
        }

        ThinkingPolicy {
            entries,
            configured,
            warnings,
        }
    }

    /// Problems of configured profiles (unresolved models, missing
    /// reports, empty supported sets) that did not abort anything.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// A test double: every named profile resolves to a model that
    /// supports every thinking level. Used by integration tests that
    /// drive the host doubles directly, without protocol capability
    /// negotiation.
    pub fn permissive(profiles: &[&str]) -> Self {
        ThinkingPolicy {
            entries: profiles
                .iter()
                .map(|name| {
                    (
                        name.to_string(),
                        PolicyEntry {
                            model: format!("acme/{name}"),
                            supported: ThinkingLevel::PI_ORDER.into_iter().collect(),
                            override_level: None,
                        },
                    )
                })
                .collect(),
            configured: profiles.iter().map(|name| name.to_string()).collect(),
            warnings: Vec::new(),
        }
    }

    /// Resolve a semantic-job session level through an optional
    /// policy. A `None` policy (host-less flows, which never start
    /// sessions) keeps the site default unchanged. Clamp notes are
    /// logged: internal fact jobs have no user-visible diagnostics
    /// channel.
    pub fn resolve_job_level(
        thinking: Option<&ThinkingPolicy>,
        profile: &str,
        default: ThinkingLevel,
    ) -> Result<ThinkingLevel, String> {
        match thinking {
            Some(policy) => policy.resolve(profile, None, default).map(|(level, note)| {
                if let Some(note) = note {
                    tracing::warn!("{note}");
                }
                level
            }),
            None => Ok(default),
        }
    }

    /// Resolve the effective thinking level for one session.
    ///
    /// `explicit` is a genuine per-request level (no current caller
    /// supplies one); `default` is the reviewer-catalog or job-option
    /// level the site would otherwise use. Precedence:
    /// `explicit` > profile override > `default`, then clamping to the
    /// model's supported levels. Fails closed (naming the profile)
    /// when the profile is unusable; on success the returned level is
    /// guaranteed to be supported by the profile's model.
    pub fn resolve(
        &self,
        profile: &str,
        explicit: Option<ThinkingLevel>,
        default: ThinkingLevel,
    ) -> Result<(ThinkingLevel, Option<String>), String> {
        if !self.configured.contains(profile) {
            // The profile is not part of the trusted configuration:
            // nothing was negotiated for it, so the site default stands
            // (the host fails the session closed when the profile is
            // unknown there).
            return Ok((default, None));
        }
        let entry = self.entries.get(profile).ok_or_else(|| {
            format!(
                "model profile `{profile}` is not usable: {}",
                self.warnings
                    .iter()
                    .find(|warning| warning.contains(&format!("`{profile}`")))
                    .cloned()
                    .unwrap_or_else(|| {
                        "the profile is not configured or not in the capabilities report"
                            .to_string()
                    })
            )
        })?;
        let requested = explicit.or(entry.override_level).unwrap_or(default);
        let Some(effective) = ThinkingLevel::clamp_to_supported(requested, &entry.supported) else {
            return Err(format!(
                "model profile `{profile}` (model `{model}`) reports no supported thinking levels",
                model = entry.model
            ));
        };
        if effective == requested {
            return Ok((effective, None));
        }
        let note = format!(
            "thinking level {requested} adjusted to {effective} for model `{model}` (profile `{profile}`)",
            model = entry.model
        );
        Ok((effective, Some(note)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jury::protocol::ModelCapabilityEntry;

    fn caps(
        profiles: &[(&str, &[ThinkingLevel])],
        unresolved: &[(&str, &str)],
    ) -> ModelCapabilitiesPayload {
        ModelCapabilitiesPayload {
            host_config_id: "cfg-1".to_string(),
            profiles: profiles
                .iter()
                .map(|(name, levels)| {
                    (
                        name.to_string(),
                        ModelCapabilityEntry {
                            model: format!("acme/{name}"),
                            supported_thinking_levels: levels.iter().copied().collect(),
                        },
                    )
                })
                .collect(),
            unresolved: unresolved
                .iter()
                .map(|(name, reason)| (name.to_string(), reason.to_string()))
                .collect(),
        }
    }

    fn profiles(cfg: &[(&str, Option<ThinkingLevel>)]) -> BTreeMap<String, ModelProfile> {
        cfg.iter()
            .map(|(name, level)| {
                (
                    name.to_string(),
                    ModelProfile {
                        model: format!("acme/{name}"),
                        endpoint: None,
                        thinking_level: *level,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn resolve_keeps_supported_defaults() {
        let policy = ThinkingPolicy::build(
            &caps(
                &[(
                    "standard",
                    &[ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max],
                )],
                &[],
            ),
            &profiles(&[("standard", None)]),
        );
        assert!(policy.warnings().is_empty());
        let (level, note) = policy
            .resolve("standard", None, ThinkingLevel::High)
            .unwrap();
        assert_eq!(level, ThinkingLevel::High);
        assert!(note.is_none());
    }

    #[test]
    fn profile_override_beats_site_default_then_clamps() {
        let policy = ThinkingPolicy::build(
            &caps(
                &[(
                    "standard",
                    &[ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max],
                )],
                &[],
            ),
            &profiles(&[("standard", Some(ThinkingLevel::Max))]),
        );
        // A consolidation-style site default of `medium` is overridden
        // to the configured `max`, which glm-5.3-like models support.
        let (level, note) = policy
            .resolve("standard", None, ThinkingLevel::Medium)
            .unwrap();
        assert_eq!(level, ThinkingLevel::Max);
        assert!(note.is_none(), "override to a supported level is silent");
        // Override itself unsupported (medium) clamps with a note.
        let policy = ThinkingPolicy::build(
            &caps(
                &[(
                    "standard",
                    &[ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max],
                )],
                &[],
            ),
            &profiles(&[("standard", Some(ThinkingLevel::Medium))]),
        );
        let (level, note) = policy
            .resolve("standard", None, ThinkingLevel::Low)
            .unwrap();
        assert_eq!(level, ThinkingLevel::High);
        assert_eq!(
            note.as_deref(),
            Some(
                "thinking level medium adjusted to high for model `acme/standard` (profile `standard`)"
            )
        );
    }

    #[test]
    fn explicit_request_wins_over_override() {
        let policy = ThinkingPolicy::build(
            &caps(
                &[(
                    "standard",
                    &[ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High],
                )],
                &[],
            ),
            &profiles(&[("standard", Some(ThinkingLevel::High))]),
        );
        let (level, _) = policy
            .resolve("standard", Some(ThinkingLevel::Low), ThinkingLevel::Medium)
            .unwrap();
        assert_eq!(level, ThinkingLevel::Low);
    }

    #[test]
    fn clamp_of_site_default_records_note() {
        let policy = ThinkingPolicy::build(
            &caps(
                &[(
                    "standard",
                    &[ThinkingLevel::Low, ThinkingLevel::High, ThinkingLevel::Max],
                )],
                &[],
            ),
            &profiles(&[("standard", None)]),
        );
        let (level, note) = policy
            .resolve("standard", None, ThinkingLevel::Medium)
            .unwrap();
        assert_eq!(level, ThinkingLevel::High);
        assert!(note.as_deref().unwrap().contains("medium adjusted to high"));
    }

    #[test]
    fn unusable_profile_fails_closed_only_for_its_sessions() {
        let policy = ThinkingPolicy::build(
            &caps(
                &[("standard", &[ThinkingLevel::Low, ThinkingLevel::High])],
                &[(
                    "fast",
                    "model `acme/fast` is not registered in the local model catalog",
                )],
            ),
            &profiles(&[("standard", None), ("fast", None)]),
        );
        // The broken optional profile did not abort the build and is
        // only a warning...
        assert_eq!(policy.warnings().len(), 1);
        assert!(policy.warnings()[0].contains("`fast`"));
        // ...but sessions on it fail closed with a named reason.
        let error = policy
            .resolve("fast", None, ThinkingLevel::High)
            .unwrap_err();
        assert!(error.contains("`fast`"));
        assert!(error.contains("not registered"));
        // Sessions on the healthy profile are unaffected.
        assert!(
            policy
                .resolve("standard", None, ThinkingLevel::High)
                .is_ok()
        );
    }

    #[test]
    fn empty_supported_set_fails_closed() {
        let policy = ThinkingPolicy::build(
            &caps(&[("standard", &[])], &[]),
            &profiles(&[("standard", None)]),
        );
        assert!(
            policy
                .resolve("standard", None, ThinkingLevel::High)
                .is_err()
        );
        assert_eq!(policy.warnings().len(), 1);
    }

    #[test]
    fn unconfigured_profiles_pass_through_but_configured_broken_ones_fail() {
        // `strong` is not configured at all: legacy passthrough.
        let policy = ThinkingPolicy::build(&caps(&[], &[]), &profiles(&[]));
        let (level, note) = policy.resolve("strong", None, ThinkingLevel::High).unwrap();
        assert_eq!(level, ThinkingLevel::High);
        assert!(note.is_none());
        // A configured profile with no capabilities report fails closed.
        let policy = ThinkingPolicy::build(
            &caps(&[("standard", &[ThinkingLevel::High])], &[]),
            &profiles(&[("standard", None), ("strong", None)]),
        );
        let error = policy
            .resolve("strong", None, ThinkingLevel::High)
            .unwrap_err();
        assert!(error.contains("`strong`"));
    }
}
