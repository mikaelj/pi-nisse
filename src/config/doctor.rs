//! `nisse doctor`: report the local environment and trusted configuration.
//!
//! Doctor performs no LLM requests. It checks Git availability,
//! configuration validity, model profile completeness, endpoint trust
//! (which may resolve configured hostnames), and the optional classifier
//! status. Any failed check makes doctor exit non-zero.

use super::endpoint::{EndpointTrustError, HostResolver, TrustedEndpoint, check_endpoint_trust};
use super::git_probe::GitProbe;
use super::{ConfigError, LoadedConfig, NisseConfig, Provider, missing_required_profiles};

/// Outcome of one doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// The check passed.
    Pass,
    /// The check failed; doctor exits non-zero.
    Fail,
    /// The check passed with notable caveats.
    Warn,
    /// The check could not run because an earlier check failed.
    Skipped,
}

/// One rendered doctor check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCheck {
    /// Check name (for example `git`, `config`).
    pub name: &'static str,
    /// Outcome.
    pub status: CheckStatus,
    /// Detail lines.
    pub lines: Vec<String>,
}

/// Full doctor report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// Checks in report order.
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Whether any check failed.
    pub fn has_failures(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
    }

    /// Render the human-readable report.
    pub fn render(&self) -> String {
        let mut out = String::from("nisse doctor\n");
        for check in &self.checks {
            let status = match check.status {
                CheckStatus::Pass => "ok",
                CheckStatus::Fail => "FAIL",
                CheckStatus::Warn => "warn",
                CheckStatus::Skipped => "skipped",
            };
            out.push_str(&format!("{}: {status}\n", check.name));
            for line in &check.lines {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }
}

/// Trust evaluation for one configured endpoint.
#[derive(Debug)]
struct EndpointOutcome {
    label: String,
    url: String,
    result: Result<TrustedEndpoint, EndpointTrustError>,
}

const CLASSIFIER_LABEL: &str = "classifier";
const EMBEDDINGS_LABEL: &str = "embeddings";

/// Run all doctor checks and produce the report.
pub fn run_doctor(
    git: GitProbe,
    loaded: Result<LoadedConfig, ConfigError>,
    resolver: &dyn HostResolver,
) -> DoctorReport {
    let mut checks = Vec::new();
    checks.push(git_check(&git));

    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(err) => {
            checks.push(DoctorCheck {
                name: "config",
                status: CheckStatus::Fail,
                lines: vec![err.to_string()],
            });
            for name in ["model profiles", "endpoint trust", CLASSIFIER_LABEL] {
                checks.push(DoctorCheck {
                    name,
                    status: CheckStatus::Skipped,
                    lines: vec!["skipped: configuration could not be loaded".to_string()],
                });
            }
            return DoctorReport { checks };
        }
    };

    checks.push(config_check(&loaded));
    checks.push(model_profiles_check(&loaded.config));

    let outcomes = endpoint_outcomes(&loaded.config, resolver);
    checks.push(endpoint_trust_check(
        &outcomes,
        loaded.config.network.allow_non_private_endpoints,
    ));
    checks.push(classifier_check(&loaded.config, &outcomes));

    DoctorReport { checks }
}

fn git_check(git: &GitProbe) -> DoctorCheck {
    if !git.available {
        return DoctorCheck {
            name: "git",
            status: CheckStatus::Fail,
            lines: vec!["Git executable not found on PATH".to_string()],
        };
    }
    let mut lines = vec![
        git.version
            .clone()
            .unwrap_or_else(|| "unknown version".to_string()),
    ];
    match &git.git_common_dir {
        Some(common_dir) => lines.push(format!(
            "inside repository; common dir: {}",
            common_dir.display()
        )),
        None => lines.push(
            "not inside a Git repository (repository-local config layer unavailable)".to_string(),
        ),
    }
    DoctorCheck {
        name: "git",
        status: CheckStatus::Pass,
        lines,
    }
}

fn config_check(loaded: &LoadedConfig) -> DoctorCheck {
    let mut lines = Vec::new();
    match &loaded.user_config {
        Some(path) => lines.push(format!("user config loaded: {}", path.display())),
        None => lines.push("user config: not found".to_string()),
    }
    match &loaded.repo_config {
        Some(path) => lines.push(format!("repository config loaded: {}", path.display())),
        None => lines.push("repository config: not found".to_string()),
    }
    DoctorCheck {
        name: "config",
        status: CheckStatus::Pass,
        lines,
    }
}

fn model_profiles_check(config: &NisseConfig) -> DoctorCheck {
    let missing = missing_required_profiles(config);
    let mut lines: Vec<String> = missing
        .iter()
        .map(|name| format!("missing required model profile: `{name}`"))
        .collect();

    for (name, profile) in &config.model_profiles {
        let endpoint = profile.endpoint.as_deref().unwrap_or("none");
        let thinking = profile
            .thinking_level
            .map(|level| level.to_string())
            .unwrap_or_else(|| "default".to_string());
        lines.push(format!(
            "`{name}`: model={} endpoint={} thinking_level={}",
            profile.model, endpoint, thinking
        ));
    }

    DoctorCheck {
        name: "model profiles",
        status: if missing.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        lines,
    }
}

fn endpoint_outcomes(config: &NisseConfig, resolver: &dyn HostResolver) -> Vec<EndpointOutcome> {
    let allow = config.network.allow_non_private_endpoints;

    let mut targets: Vec<(String, String)> = Vec::new();
    for (name, profile) in &config.model_profiles {
        if let Some(url) = &profile.endpoint {
            targets.push((format!("model profile `{name}`"), url.clone()));
        }
    }
    if config.classifier.provider == Provider::Needle
        && let Some(url) = &config.classifier.endpoint
    {
        targets.push((CLASSIFIER_LABEL.to_string(), url.clone()));
    }
    if config.embeddings.provider == Provider::Needle
        && let Some(url) = &config.embeddings.endpoint
    {
        targets.push((EMBEDDINGS_LABEL.to_string(), url.clone()));
    }

    targets
        .into_iter()
        .map(|(label, url)| {
            let result = check_endpoint_trust(&url, allow, resolver);
            EndpointOutcome { label, url, result }
        })
        .collect()
}

fn endpoint_trust_check(outcomes: &[EndpointOutcome], allow_non_private: bool) -> DoctorCheck {
    if outcomes.is_empty() {
        return DoctorCheck {
            name: "endpoint trust",
            status: CheckStatus::Pass,
            lines: vec!["no endpoints configured".to_string()],
        };
    }

    let mut lines = Vec::new();
    if allow_non_private {
        lines.push("non-private endpoints permitted by trusted local configuration".to_string());
    }
    let mut failed = false;
    for outcome in outcomes {
        match &outcome.result {
            Ok(trusted) => lines.push(format!(
                "{}: {} ({})",
                outcome.label, outcome.url, trusted.detail
            )),
            Err(err) => {
                failed = true;
                lines.push(format!("{}: {err}", outcome.label));
            }
        }
    }
    DoctorCheck {
        name: "endpoint trust",
        status: if failed {
            CheckStatus::Fail
        } else {
            CheckStatus::Pass
        },
        lines,
    }
}

fn classifier_check(config: &NisseConfig, outcomes: &[EndpointOutcome]) -> DoctorCheck {
    if config.classifier.provider == Provider::None {
        return DoctorCheck {
            name: CLASSIFIER_LABEL,
            status: CheckStatus::Pass,
            lines: vec!["not configured (deterministic rules-only routing)".to_string()],
        };
    }

    let Some(endpoint) = &config.classifier.endpoint else {
        return DoctorCheck {
            name: CLASSIFIER_LABEL,
            status: CheckStatus::Fail,
            lines: vec!["provider needle configured without an endpoint".to_string()],
        };
    };
    let outcome = outcomes
        .iter()
        .find(|outcome| outcome.label == CLASSIFIER_LABEL);
    match outcome {
        Some(outcome) if outcome.result.is_ok() => DoctorCheck {
            name: CLASSIFIER_LABEL,
            status: CheckStatus::Pass,
            lines: vec![format!("provider needle; endpoint {endpoint} trusted")],
        },
        Some(outcome) => DoctorCheck {
            name: CLASSIFIER_LABEL,
            status: CheckStatus::Fail,
            lines: vec![format!(
                "provider needle; endpoint {endpoint} not trusted: {}",
                outcome
                    .result
                    .as_ref()
                    .err()
                    .map(|err| err.to_string())
                    .unwrap_or_default()
            )],
        },
        None => DoctorCheck {
            name: CLASSIFIER_LABEL,
            status: CheckStatus::Fail,
            lines: vec![format!(
                "provider needle; endpoint {endpoint} was not evaluated"
            )],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::endpoint::FakeResolver;
    use super::*;
    use crate::domain::model::{ModelProfile, ThinkingLevel};
    use std::net::{IpAddr, Ipv4Addr};

    fn healthy_config() -> NisseConfig {
        let mut config = NisseConfig::default();
        config.model_profiles.insert(
            "standard".to_string(),
            ModelProfile {
                model: "local-standard".to_string(),
                endpoint: Some("http://127.0.0.1:8100/v1".to_string()),
                thinking_level: Some(ThinkingLevel::Low),
            },
        );
        config.model_profiles.insert(
            "strong".to_string(),
            ModelProfile {
                model: "local-strong".to_string(),
                endpoint: None,
                thinking_level: None,
            },
        );
        config
    }

    fn healthy_loaded() -> LoadedConfig {
        LoadedConfig {
            config: healthy_config(),
            user_config: Some("/home/alice/.config/nisse/config.toml".into()),
            repo_config: None,
        }
    }

    fn healthy_git() -> GitProbe {
        GitProbe {
            available: true,
            version: Some("git version 2.39.0".to_string()),
            in_repository: false,
            git_common_dir: None,
        }
    }

    #[test]
    fn healthy_environment_passes() {
        let resolver = FakeResolver::default();
        let report = run_doctor(healthy_git(), Ok(healthy_loaded()), &resolver);
        assert!(!report.has_failures());
        let text = report.render();
        assert!(text.starts_with("nisse doctor\n"));
        assert!(text.contains("git: ok"));
        assert!(text.contains("not inside a Git repository"));
        assert!(text.contains("config: ok"));
        assert!(text.contains("user config loaded"));
        assert!(text.contains("model profiles: ok"));
        assert!(text.contains("`standard`: model=local-standard"));
        assert!(text.contains("`strong`: model=local-strong endpoint=none"));
        assert!(text.contains("endpoint trust: ok"));
        assert!(text.contains("http://127.0.0.1:8100/v1"));
        assert!(text.contains("classifier: ok"));
        assert!(text.contains("not configured (deterministic rules-only routing)"));
    }

    #[test]
    fn missing_required_profiles_fails() {
        let mut loaded = healthy_loaded();
        loaded.config.model_profiles.remove("strong");
        let resolver = FakeResolver::default();
        let report = run_doctor(healthy_git(), Ok(loaded), &resolver);
        assert!(report.has_failures());
        let text = report.render();
        assert!(text.contains("model profiles: FAIL"));
        assert!(text.contains("missing required model profile: `strong`"));
    }

    #[test]
    fn public_endpoint_fails_trust() {
        let mut loaded = healthy_loaded();
        loaded.config.model_profiles.insert(
            "standard".to_string(),
            ModelProfile {
                model: "cloud".to_string(),
                endpoint: Some("https://192.0.2.10/v1".to_string()),
                thinking_level: None,
            },
        );
        let resolver = FakeResolver::default();
        let report = run_doctor(healthy_git(), Ok(loaded), &resolver);
        assert!(report.has_failures());
        let text = report.render();
        assert!(text.contains("endpoint trust: FAIL"));
        assert!(text.contains("non-permitted address"));
    }

    #[test]
    fn hostname_endpoints_are_resolved() {
        let mut loaded = healthy_loaded();
        loaded.config.model_profiles.insert(
            "standard".to_string(),
            ModelProfile {
                model: "modelbox".to_string(),
                endpoint: Some("http://modelbox.internal:8000/v1".to_string()),
                thinking_level: None,
            },
        );
        let resolver = FakeResolver {
            ips: vec![IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))],
            fail: false,
        };
        let report = run_doctor(healthy_git(), Ok(loaded), &resolver);
        assert!(!report.has_failures());
        let text = report.render();
        assert!(text.contains("resolved to permitted addresses"));
    }

    #[test]
    fn failed_resolution_fails_trust() {
        let mut loaded = healthy_loaded();
        loaded.config.model_profiles.insert(
            "standard".to_string(),
            ModelProfile {
                model: "modelbox".to_string(),
                endpoint: Some("http://modelbox.internal:8000/v1".to_string()),
                thinking_level: None,
            },
        );
        let resolver = FakeResolver {
            ips: vec![],
            fail: true,
        };
        let report = run_doctor(healthy_git(), Ok(loaded), &resolver);
        assert!(report.has_failures());
        assert!(report.render().contains("endpoint trust: FAIL"));
    }

    #[test]
    fn config_error_fails_and_skips_dependent_checks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "not toml [").unwrap();
        let err = super::super::load_config(Some(&path), None).unwrap_err();

        let resolver = FakeResolver::default();
        let report = run_doctor(healthy_git(), Err(err), &resolver);
        assert!(report.has_failures());
        let text = report.render();
        assert!(text.contains("config: FAIL"));
        assert!(text.contains("model profiles: skipped"));
        assert!(text.contains("endpoint trust: skipped"));
        assert!(text.contains("classifier: skipped"));
    }

    #[test]
    fn missing_git_fails() {
        let git = GitProbe {
            available: false,
            version: None,
            in_repository: false,
            git_common_dir: None,
        };
        let resolver = FakeResolver::default();
        let report = run_doctor(git, Ok(healthy_loaded()), &resolver);
        assert!(report.has_failures());
        assert!(report.render().contains("git: FAIL"));
    }

    #[test]
    fn needle_classifier_status_is_reported() {
        let mut loaded = healthy_loaded();
        loaded.config.classifier.provider = Provider::Needle;
        loaded.config.classifier.endpoint = Some("http://127.0.0.1:8787".to_string());
        let resolver = FakeResolver::default();
        let report = run_doctor(healthy_git(), Ok(loaded), &resolver);
        assert!(!report.has_failures());
        let text = report.render();
        assert!(text.contains("classifier: ok"));
        assert!(text.contains("provider needle; endpoint http://127.0.0.1:8787 trusted"));
    }

    #[test]
    fn render_indents_detail_lines() {
        let report = DoctorReport {
            checks: vec![DoctorCheck {
                name: "git",
                status: CheckStatus::Pass,
                lines: vec!["git version 2.0".to_string()],
            }],
        };
        assert_eq!(
            report.render(),
            "nisse doctor\ngit: ok\n  git version 2.0\n"
        );
    }
}
