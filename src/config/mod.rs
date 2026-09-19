//! Trusted local configuration.
//!
//! Nisse configuration is TOML. Frozen precedence, highest first:
//!
//! 1. explicit CLI options (the frozen CLI defines no configuration
//!    options, so this layer is currently always empty);
//! 2. `$GIT_COMMON_DIR/nisse/config.toml` — repository-local trusted
//!    configuration, never committed;
//! 3. the platform-native user config (`<config dir>/nisse/config.toml`);
//! 4. compiled defaults.
//!
//! Layers merge field-level: a value present in a higher layer replaces the
//! lower layer's value, and absent fields are inherited. Model profiles
//! merge per profile name. The Git-tracked `.nisse/**` tree is project
//! data and is never a configuration source (see the trust rules in
//! PLAN.md section 10).
//!
//! Example:
//!
//! ```toml
//! [contributor]
//! id = "alice"
//!
//! [network]
//! allow_non_private_endpoints = false
//!
//! [model_profiles.standard]
//! model = "my-local-model"
//! endpoint = "http://127.0.0.1:8080/v1"
//! thinking_level = "medium"
//!
//! [model_profiles.strong]
//! model = "my-strong-model"
//! endpoint = "http://10.0.0.5:8080/v1"
//!
//! [classifier]
//! provider = "needle"
//! endpoint = "http://127.0.0.1:8787"
//!
//! [embeddings]
//! provider = "none"
//!
//! [learning]
//! enabled = true
//! ```

pub mod doctor;
pub mod endpoint;
pub mod git_probe;
pub mod paths;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::domain::model::ModelProfile;

/// Model profile names required before review operations can run.
pub const REQUIRED_PROFILE_NAMES: [&str; 2] = ["standard", "strong"];

/// Optional conceptual model profile names.
pub const OPTIONAL_PROFILE_NAMES: [&str; 2] = ["fast", "strongest"];

/// Fully merged Nisse configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NisseConfig {
    /// Network and endpoint trust policy.
    pub network: NetworkConfig,
    /// Named model profiles (`standard`, `strong`, optionally others).
    pub model_profiles: BTreeMap<String, ModelProfile>,
    /// Routing classifier configuration (Needle is optional).
    pub classifier: ClassifierConfig,
    /// Embedding provider configuration (optional).
    pub embeddings: EmbeddingsConfig,
    /// Learning behavior.
    pub learning: LearningConfig,
    /// Contributor identity (lives in user config).
    pub contributor: Option<ContributorIdentity>,
}

impl Default for NisseConfig {
    fn default() -> Self {
        Self {
            network: NetworkConfig {
                allow_non_private_endpoints: false,
            },
            model_profiles: BTreeMap::new(),
            classifier: ClassifierConfig {
                provider: Provider::None,
                endpoint: None,
            },
            embeddings: EmbeddingsConfig {
                provider: Provider::None,
                endpoint: None,
            },
            learning: LearningConfig { enabled: true },
            contributor: None,
        }
    }
}

/// Network and endpoint trust policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Explicit override permitting endpoints outside the default
    /// loopback/private address classes. Defaults to `false`.
    pub allow_non_private_endpoints: bool,
}

/// Optional helper provider selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    /// No helper provider; deterministic behavior only.
    #[default]
    None,
    /// The local Needle adapter.
    Needle,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Provider::None => "none",
            Provider::Needle => "needle",
        })
    }
}

/// Classifier (routing) provider configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifierConfig {
    /// Selected provider.
    pub provider: Provider,
    /// Provider endpoint, required when the provider is not `none`.
    pub endpoint: Option<String>,
}

/// Embedding provider configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingsConfig {
    /// Selected provider.
    pub provider: Provider,
    /// Provider endpoint, required when the provider is not `none`.
    pub endpoint: Option<String>,
}

/// Learning behavior configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningConfig {
    /// Whether lesson learning from observations is enabled.
    pub enabled: bool,
}

/// Local contributor identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributorIdentity {
    /// Contributor ID matching `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}`.
    pub id: String,
}

/// Check a contributor ID against the frozen slug pattern.
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

/// Names from [`REQUIRED_PROFILE_NAMES`] missing from `config`.
pub fn missing_required_profiles(config: &NisseConfig) -> Vec<&'static str> {
    REQUIRED_PROFILE_NAMES
        .iter()
        .filter(|name| !config.model_profiles.contains_key(**name))
        .copied()
        .collect()
}

/// Result of loading and merging configuration layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    /// Merged configuration.
    pub config: NisseConfig,
    /// Path of the user config file that was loaded, if any.
    pub user_config: Option<PathBuf>,
    /// Path of the repository config file that was loaded, if any.
    pub repo_config: Option<PathBuf>,
}

/// Typed configuration failures.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A config file could not be read.
    #[error("failed to read configuration `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// A config file could not be parsed, contains unknown fields, or uses
    /// unknown enum variants.
    #[error("malformed configuration `{path}`: {message}")]
    Malformed { path: PathBuf, message: String },
    /// A config file parses but violates configuration semantics.
    #[error("invalid configuration `{path}`: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// Load and merge configuration layers.
///
/// `user` and `repo` are optional layer paths; missing files are skipped
/// without error. The repository layer overrides the user layer. Each file
/// is validated on its own, and the merged result is validated again so
/// cross-layer inconsistencies are rejected.
pub fn load_config(user: Option<&Path>, repo: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
    let mut config = NisseConfig::default();
    let mut loaded_user = None;
    let mut loaded_repo = None;

    if let Some(path) = user
        && apply_layer(&mut config, path, Origin::User)?
    {
        loaded_user = Some(path.to_path_buf());
    }
    if let Some(path) = repo
        && apply_layer(&mut config, path, Origin::Repository)?
    {
        loaded_repo = Some(path.to_path_buf());
    }

    // Cross-layer validation. Errors are attributed to the highest loaded
    // layer, which is the most likely contributor of the offending value.
    let blame = loaded_repo.as_deref().or(loaded_user.as_deref());
    validate_merged(&config, blame)?;

    Ok(LoadedConfig {
        config,
        user_config: loaded_user,
        repo_config: loaded_repo,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    User,
    Repository,
}

/// Apply one config file if it exists. Returns whether the file was loaded.
fn apply_layer(config: &mut NisseConfig, path: &Path, origin: Origin) -> Result<bool, ConfigError> {
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let parsed: PartialConfig = toml::from_str(&text).map_err(|err| ConfigError::Malformed {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    validate_layer(&parsed, path, origin)?;
    parsed.apply_to(config);
    Ok(true)
}

fn invalid(path: &Path, message: String) -> ConfigError {
    ConfigError::Invalid {
        path: path.to_path_buf(),
        message,
    }
}

/// Validate one parsed file in isolation.
fn validate_layer(layer: &PartialConfig, path: &Path, origin: Origin) -> Result<(), ConfigError> {
    if origin == Origin::Repository && layer.contributor.as_ref().is_some_and(|c| c.id.is_some()) {
        return Err(invalid(
            path,
            "contributor identity lives in the user config, not repository configuration"
                .to_string(),
        ));
    }

    if let Some(contributor) = &layer.contributor
        && let Some(id) = &contributor.id
        && !is_valid_contributor_id(id)
    {
        return Err(invalid(
            path,
            format!("contributor id `{id}` does not match `[a-zA-Z0-9][a-zA-Z0-9._-]{{0,63}}`"),
        ));
    }

    if let Some(profiles) = &layer.model_profiles {
        for (name, profile) in profiles {
            if name.trim().is_empty() {
                return Err(invalid(
                    path,
                    "model profile names must not be empty".to_string(),
                ));
            }
            if profile.model.trim().is_empty() {
                return Err(invalid(
                    path,
                    format!("model profile `{name}` has an empty `model`"),
                ));
            }
            if let Some(endpoint) = &profile.endpoint {
                endpoint::parse_endpoint_url(endpoint)
                    .map_err(|err| invalid(path, err.to_string()))?;
            }
        }
    }

    Ok(())
}

/// Validate the merged configuration.
///
/// Compiled defaults are always valid, so validation is only attributed to
/// a path when at least one file was loaded.
fn validate_merged(config: &NisseConfig, blame: Option<&Path>) -> Result<(), ConfigError> {
    let Some(path) = blame else {
        return Ok(());
    };

    if let Some(contributor) = &config.contributor
        && !is_valid_contributor_id(&contributor.id)
    {
        return Err(invalid(
            path,
            format!(
                "contributor id `{}` does not match `[a-zA-Z0-9][a-zA-Z0-9._-]{{0,63}}`",
                contributor.id
            ),
        ));
    }

    for (name, profile) in &config.model_profiles {
        if name.trim().is_empty() {
            return Err(invalid(
                path,
                "model profile names must not be empty".to_string(),
            ));
        }
        if profile.model.trim().is_empty() {
            return Err(invalid(
                path,
                format!("model profile `{name}` has an empty `model`"),
            ));
        }
        if let Some(endpoint) = &profile.endpoint {
            endpoint::parse_endpoint_url(endpoint).map_err(|err| invalid(path, err.to_string()))?;
        }
    }

    let provider_sections = [
        (
            "classifier",
            config.classifier.provider,
            config.classifier.endpoint.as_deref(),
        ),
        (
            "embeddings",
            config.embeddings.provider,
            config.embeddings.endpoint.as_deref(),
        ),
    ];
    for (section, provider, endpoint) in provider_sections {
        let mismatch = match (provider, endpoint) {
            (Provider::None, Some(_)) => Some(format!(
                "`{section}` has an endpoint but its provider is `none`"
            )),
            (Provider::Needle, None) => {
                Some(format!("`{section}` provider `needle` requires `endpoint`"))
            }
            _ => None,
        };
        if let Some(message) = mismatch {
            return Err(invalid(path, message));
        }
    }

    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialConfig {
    network: Option<PartialNetwork>,
    model_profiles: Option<BTreeMap<String, ModelProfile>>,
    classifier: Option<PartialProviderConfig>,
    embeddings: Option<PartialProviderConfig>,
    learning: Option<PartialLearning>,
    contributor: Option<PartialContributor>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialNetwork {
    allow_non_private_endpoints: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialProviderConfig {
    provider: Option<Provider>,
    endpoint: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialLearning {
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PartialContributor {
    id: Option<String>,
}

impl PartialConfig {
    fn apply_to(self, config: &mut NisseConfig) {
        if let Some(network) = self.network
            && let Some(allow) = network.allow_non_private_endpoints
        {
            config.network.allow_non_private_endpoints = allow;
        }
        if let Some(profiles) = self.model_profiles {
            config.model_profiles.extend(profiles);
        }
        if let Some(classifier) = self.classifier {
            apply_provider_layer(
                &mut config.classifier.provider,
                &mut config.classifier.endpoint,
                classifier,
            );
        }
        if let Some(embeddings) = self.embeddings {
            apply_provider_layer(
                &mut config.embeddings.provider,
                &mut config.embeddings.endpoint,
                embeddings,
            );
        }
        if let Some(learning) = self.learning
            && let Some(enabled) = learning.enabled
        {
            config.learning.enabled = enabled;
        }
        if let Some(contributor) = self.contributor
            && let Some(id) = contributor.id
        {
            config.contributor = Some(ContributorIdentity { id });
        }
    }
}

fn apply_provider_layer(
    provider: &mut Provider,
    endpoint: &mut Option<String>,
    layer: PartialProviderConfig,
) {
    if let Some(value) = layer.provider {
        *provider = value;
    }
    if let Some(value) = layer.endpoint {
        *endpoint = Some(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::ThinkingLevel;
    use std::fs;

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn defaults_are_conservative() {
        let config = NisseConfig::default();
        assert!(!config.network.allow_non_private_endpoints);
        assert!(config.model_profiles.is_empty());
        assert_eq!(config.classifier.provider, Provider::None);
        assert!(config.classifier.endpoint.is_none());
        assert_eq!(config.embeddings.provider, Provider::None);
        assert!(config.embeddings.endpoint.is_none());
        assert!(config.learning.enabled);
        assert!(config.contributor.is_none());
        assert_eq!(
            missing_required_profiles(&config),
            vec!["standard", "strong"]
        );
    }

    #[test]
    fn load_without_files_returns_defaults() {
        let loaded = load_config(None, None).unwrap();
        assert_eq!(loaded.config, NisseConfig::default());
        assert!(loaded.user_config.is_none());
        assert!(loaded.repo_config.is_none());
    }

    #[test]
    fn missing_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("config.toml");
        let loaded = load_config(Some(&absent), Some(&absent)).unwrap();
        assert_eq!(loaded.config, NisseConfig::default());
        assert!(loaded.user_config.is_none());
        assert!(loaded.repo_config.is_none());
    }

    #[test]
    fn user_layer_overrides_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"
[contributor]
id = "alice"

[network]
allow_non_private_endpoints = false

[model_profiles.standard]
model = "user-standard"
endpoint = "http://127.0.0.1:1000/v1"
thinking_level = "low"

[learning]
enabled = false
"#,
        );

        let loaded = load_config(Some(&path), None).unwrap();
        let config = &loaded.config;
        assert!(!config.network.allow_non_private_endpoints);
        assert_eq!(
            config.contributor.as_ref().map(|c| c.id.as_str()),
            Some("alice")
        );
        assert!(!config.learning.enabled);
        let standard = &config.model_profiles["standard"];
        assert_eq!(standard.model, "user-standard");
        assert_eq!(standard.thinking_level, Some(ThinkingLevel::Low));
        assert_eq!(loaded.user_config.as_deref(), Some(path.as_path()));
        // Only one required profile present so far.
        assert_eq!(missing_required_profiles(config), vec!["strong"]);
    }

    #[test]
    fn repository_layer_overrides_user_layer() {
        let user_dir = tempfile::tempdir().unwrap();
        let user_path = write_config(
            user_dir.path(),
            r#"
[contributor]
id = "alice"

[network]
allow_non_private_endpoints = false

[model_profiles.standard]
model = "user-standard"
endpoint = "http://127.0.0.1:1000/v1"

[classifier]
provider = "none"
"#,
        );
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_path = write_config(
            repo_dir.path(),
            r#"
[network]
allow_non_private_endpoints = true

[model_profiles.standard]
model = "repo-standard"
endpoint = "http://10.0.0.5:2000/v1"

[model_profiles.strong]
model = "repo-strong"
"#,
        );

        let loaded = load_config(Some(&user_path), Some(&repo_path)).unwrap();
        let config = &loaded.config;
        // Repository layer wins for fields it defines.
        assert!(config.network.allow_non_private_endpoints);
        assert_eq!(config.model_profiles["standard"].model, "repo-standard");
        assert_eq!(
            config.model_profiles["standard"].endpoint.as_deref(),
            Some("http://10.0.0.5:2000/v1")
        );
        // Profiles merge per name; user-only values survive.
        assert_eq!(config.model_profiles["strong"].model, "repo-strong");
        assert_eq!(
            config.contributor.as_ref().map(|c| c.id.as_str()),
            Some("alice")
        );
        assert!(missing_required_profiles(config).is_empty());
        assert_eq!(loaded.user_config.as_deref(), Some(user_path.as_path()));
        assert_eq!(loaded.repo_config.as_deref(), Some(repo_path.as_path()));
    }

    #[test]
    fn repository_layer_alone_loads() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_path = write_config(
            repo_dir.path(),
            r#"
[model_profiles.standard]
model = "repo-standard"

[model_profiles.strong]
model = "repo-strong"
"#,
        );
        let loaded = load_config(None, Some(&repo_path)).unwrap();
        assert!(loaded.user_config.is_none());
        assert_eq!(loaded.repo_config.as_deref(), Some(repo_path.as_path()));
        assert!(missing_required_profiles(&loaded.config).is_empty());
    }

    #[test]
    fn malformed_toml_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "this is [ not toml");
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }), "{err}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "[frobnication]\nenabled = true\n");
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }), "{err}");

        let path = write_config(
            dir.path(),
            "[network]\nallow_non_private_endpoints = true\nextra = 3\n",
        );
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }), "{err}");
    }

    #[test]
    fn wrong_types_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "[network]\nallow_non_private_endpoints = \"yes\"\n",
        );
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }), "{err}");
    }

    #[test]
    fn unknown_provider_and_thinking_level_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "[classifier]\nprovider = \"gpt\"\nendpoint = \"http://127.0.0.1:1\"\n",
        );
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }), "{err}");

        let path = write_config(
            dir.path(),
            "[model_profiles.standard]\nmodel = \"m\"\nthinking_level = \"extreme\"\n",
        );
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Malformed { .. }), "{err}");
    }

    #[test]
    fn invalid_contributor_ids_are_rejected() {
        for id in [
            String::new(),
            ".bob".to_string(),
            "-bob".to_string(),
            "_bob".to_string(),
            "bo b".to_string(),
            "böb".to_string(),
            "x".repeat(65),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_config(dir.path(), &format!("[contributor]\nid = \"{id}\"\n"));
            let err = load_config(Some(&path), None).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { .. }),
                "expected `{id}` to be rejected, got {err}"
            );
        }
    }

    #[test]
    fn valid_contributor_ids_are_accepted() {
        for id in [
            "a",
            "Alice",
            "alice.dev",
            "a-b_c.1",
            "x".repeat(64).as_str(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_config(dir.path(), &format!("[contributor]\nid = \"{id}\"\n"));
            let loaded = load_config(Some(&path), None);
            assert!(
                loaded.is_ok(),
                "expected `{id}` to be accepted, got {:?}",
                loaded.err()
            );
        }
    }

    #[test]
    fn contributor_identity_is_rejected_in_repository_config() {
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_path = write_config(repo_dir.path(), "[contributor]\nid = \"repo\"\n");
        let err = load_config(None, Some(&repo_path)).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("user config"));
    }

    #[test]
    fn needle_provider_requires_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "[classifier]\nprovider = \"needle\"\n");
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
        assert!(err.to_string().contains("requires `endpoint`"));

        let path = write_config(dir.path(), "[embeddings]\nprovider = \"needle\"\n");
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
    }

    #[test]
    fn none_provider_rejects_stray_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "[classifier]\nprovider = \"none\"\nendpoint = \"http://127.0.0.1:1\"\n",
        );
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
    }

    #[test]
    fn cross_layer_provider_mismatch_is_rejected() {
        let user_dir = tempfile::tempdir().unwrap();
        let user_path = write_config(
            user_dir.path(),
            "[classifier]\nprovider = \"needle\"\nendpoint = \"http://127.0.0.1:9\"\n",
        );
        let repo_dir = tempfile::tempdir().unwrap();
        let repo_path = write_config(repo_dir.path(), "[classifier]\nprovider = \"none\"\n");

        let err = load_config(Some(&user_path), Some(&repo_path)).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
    }

    #[test]
    fn profile_with_invalid_endpoint_url_is_rejected() {
        for endpoint in ["ftp://127.0.0.1/x", "localhost:8080", "http://"] {
            let dir = tempfile::tempdir().unwrap();
            let path = write_config(
                dir.path(),
                &format!("[model_profiles.standard]\nmodel = \"m\"\nendpoint = \"{endpoint}\"\n"),
            );
            let err = load_config(Some(&path), None).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { .. }),
                "expected endpoint `{endpoint}` to be rejected, got {err}"
            );
        }
    }

    #[test]
    fn profile_with_empty_model_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "[model_profiles.standard]\nmodel = \"\"\n");
        let err = load_config(Some(&path), None).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "{err}");
    }

    #[test]
    fn hostname_endpoints_are_syntactically_valid_without_resolution() {
        // Load-time validation is syntactic only; trust resolution happens
        // in doctor and before actual provider use.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(
            dir.path(),
            "[model_profiles.standard]\nmodel = \"m\"\nendpoint = \"http://modelbox.internal:8000/v1\"\n",
        );
        let loaded = load_config(Some(&path), None);
        assert!(loaded.is_ok());
    }
}
