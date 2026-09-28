//! The bundled reviewer catalog and practice packs.
//!
//! Reviewer methodology is explicit and frozen: every profile carries an
//! ordered checklist, base instructions, an explicit model profile, and a
//! set of read-only capabilities. A reviewer is never expected to invent its
//! own review methodology.
//!
//! Assets live under `assets/reviewers/` and `assets/practices/` and are
//! bundled into the binary at compile time, so normal operation never reads
//! reviewer definitions from the repository under review. The frozen sets of
//! built-in reviewer and practice pack ids are validated, not assumed.

use std::collections::BTreeSet;

use thiserror::Error;

use crate::domain::practice::PracticePack;
use crate::domain::reviewer::ReviewerProfile;

/// The reviewer that always participates in every jury. Added
/// deterministically by jury construction; never selected by routing
/// scores.
pub const QUALITY_ID: &str = "quality";

/// The stable identity of the dynamic domain expert. Used only as the
/// deterministic fallback when no static specialist qualifies; never
/// selected by routing scores.
pub const DYNAMIC_DOMAIN_EXPERT_ID: &str = "dynamic-domain-expert";

/// The investigation claim reviewer (PLAN_INVESTIGATION.md); structural
/// in every investigation jury, never routed by score.
pub const CLAIM_SKEPTIC_ID: &str = "claim-skeptic";

/// The investigation coverage reviewer (PLAN_INVESTIGATION.md);
/// structural in every investigation jury, never routed by score.
pub const COVERAGE_AUDITOR_ID: &str = "coverage-auditor";

/// The frozen set of built-in reviewer ids (plan section 16).
pub const BUILTIN_REVIEWER_IDS: &[&str] = &[
    "quality",
    "testing",
    "security",
    "concurrency",
    "performance",
    "embedded-resource",
    "realtime",
    "protocol",
    "networking",
    "persistence",
    "database",
    "api-design",
    "error-handling",
    "maintainability",
    "dynamic-domain-expert",
    "claim-skeptic",
    "coverage-auditor",
];

/// The frozen set of built-in practice pack ids (plan section 32).
pub const BUILTIN_PRACTICE_PACK_IDS: &[&str] = &[
    "quality",
    "testing",
    "security",
    "concurrency",
    "performance",
    "embedded",
    "realtime",
    "protocol",
    "networking",
    "persistence",
    "database",
    "api-design",
    "error-handling",
    "maintainability",
];

/// Named conceptual model profiles (plan section 11). Nisse never
/// hard-codes commercial model names into architecture contracts.
pub const CONCEPTUAL_MODEL_PROFILES: &[&str] = &["standard", "strong", "fast", "strongest"];

/// Languages parsed by the Nisse 1.0 source index (plan section 27).
pub const SUPPORTED_LANGUAGES: &[&str] = &[
    "c",
    "cpp",
    "csharp",
    "dart",
    "javascript",
    "rust",
    "typescript",
];

/// Reviewers that are added structurally by jury construction instead of
/// being scored by routing. They carry no trigger terms by design.
const STRUCTURAL_REVIEWER_IDS: &[&str] = &[
    QUALITY_ID,
    DYNAMIC_DOMAIN_EXPERT_ID,
    CLAIM_SKEPTIC_ID,
    COVERAGE_AUDITOR_ID,
];

/// Bundled reviewer assets, embedded at compile time.
static REVIEWER_ASSETS: &[(&str, &str)] = &[
    (
        "quality",
        include_str!("../../assets/reviewers/quality.json"),
    ),
    (
        "claim-skeptic",
        include_str!("../../assets/reviewers/claim-skeptic.json"),
    ),
    (
        "coverage-auditor",
        include_str!("../../assets/reviewers/coverage-auditor.json"),
    ),
    (
        "testing",
        include_str!("../../assets/reviewers/testing.json"),
    ),
    (
        "security",
        include_str!("../../assets/reviewers/security.json"),
    ),
    (
        "concurrency",
        include_str!("../../assets/reviewers/concurrency.json"),
    ),
    (
        "performance",
        include_str!("../../assets/reviewers/performance.json"),
    ),
    (
        "embedded-resource",
        include_str!("../../assets/reviewers/embedded-resource.json"),
    ),
    (
        "realtime",
        include_str!("../../assets/reviewers/realtime.json"),
    ),
    (
        "protocol",
        include_str!("../../assets/reviewers/protocol.json"),
    ),
    (
        "networking",
        include_str!("../../assets/reviewers/networking.json"),
    ),
    (
        "persistence",
        include_str!("../../assets/reviewers/persistence.json"),
    ),
    (
        "database",
        include_str!("../../assets/reviewers/database.json"),
    ),
    (
        "api-design",
        include_str!("../../assets/reviewers/api-design.json"),
    ),
    (
        "error-handling",
        include_str!("../../assets/reviewers/error-handling.json"),
    ),
    (
        "maintainability",
        include_str!("../../assets/reviewers/maintainability.json"),
    ),
    (
        "dynamic-domain-expert",
        include_str!("../../assets/reviewers/dynamic-domain-expert.json"),
    ),
];

/// Bundled practice pack assets, embedded at compile time.
static PRACTICE_ASSETS: &[(&str, &str)] = &[
    (
        "quality",
        include_str!("../../assets/practices/quality.json"),
    ),
    (
        "testing",
        include_str!("../../assets/practices/testing.json"),
    ),
    (
        "security",
        include_str!("../../assets/practices/security.json"),
    ),
    (
        "concurrency",
        include_str!("../../assets/practices/concurrency.json"),
    ),
    (
        "performance",
        include_str!("../../assets/practices/performance.json"),
    ),
    (
        "embedded",
        include_str!("../../assets/practices/embedded.json"),
    ),
    (
        "realtime",
        include_str!("../../assets/practices/realtime.json"),
    ),
    (
        "protocol",
        include_str!("../../assets/practices/protocol.json"),
    ),
    (
        "networking",
        include_str!("../../assets/practices/networking.json"),
    ),
    (
        "persistence",
        include_str!("../../assets/practices/persistence.json"),
    ),
    (
        "database",
        include_str!("../../assets/practices/database.json"),
    ),
    (
        "api-design",
        include_str!("../../assets/practices/api-design.json"),
    ),
    (
        "error-handling",
        include_str!("../../assets/practices/error-handling.json"),
    ),
    (
        "maintainability",
        include_str!("../../assets/practices/maintainability.json"),
    ),
];

/// Catalog construction and validation failures.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// A bundled or supplied asset failed strict parsing.
    #[error("malformed {kind} asset `{name}`: {source}")]
    MalformedAsset {
        kind: &'static str,
        name: String,
        #[source]
        source: serde_json::Error,
    },
    /// Two reviewer profiles share an id.
    #[error("duplicate reviewer id `{0}`")]
    DuplicateReviewerId(String),
    /// A reviewer id outside the frozen built-in set.
    #[error("unknown reviewer id `{0}`; the built-in reviewer set is frozen")]
    UnknownReviewerId(String),
    /// A required reviewer id is absent.
    #[error("required reviewer `{0}` is missing from the catalog")]
    MissingReviewer(String),
    /// A reviewer profile violates catalog invariants.
    #[error("invalid reviewer profile `{id}`: {problem}")]
    InvalidProfile { id: String, problem: String },
    /// Two practice packs share an id.
    #[error("duplicate practice pack id `{0}`")]
    DuplicatePracticePackId(String),
    /// A practice pack id outside the frozen built-in set.
    #[error("unknown practice pack id `{0}`; the built-in practice pack set is frozen")]
    UnknownPracticePackId(String),
    /// A required practice pack is absent.
    #[error("required practice pack `{0}` is missing from the catalog")]
    MissingPracticePack(String),
    /// A practice pack violates catalog invariants.
    #[error("invalid practice pack `{id}`: {problem}")]
    InvalidPack { id: String, problem: String },
    /// Two practices share an id across all packs.
    #[error("duplicate practice id `{0}`")]
    DuplicatePracticeId(String),
    /// A practice violates catalog invariants.
    #[error("invalid practice `{id}`: {problem}")]
    InvalidPractice { id: String, problem: String },
    /// A reviewer references a practice tag no pack provides.
    #[error("reviewer `{reviewer}` references unknown practice tag `{tag}`")]
    UnknownPracticeTag { reviewer: String, tag: String },
}

/// The validated reviewer catalog: reviewer profiles plus practice packs.
///
/// Constructed from the bundled assets via [`Catalog::bundled`] or from
/// explicit parts via [`Catalog::new`]; both run full validation.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    profiles: Vec<ReviewerProfile>,
    practice_packs: Vec<PracticePack>,
}

impl Catalog {
    /// Load and validate the catalog bundled into this binary.
    pub fn bundled() -> Result<Self, CatalogError> {
        let profiles = parse_assets("reviewer", REVIEWER_ASSETS)?;
        let practice_packs = parse_assets("practice pack", PRACTICE_ASSETS)?;
        Catalog::new(profiles, practice_packs)
    }

    /// Build and validate a catalog from explicit parts.
    pub fn new(
        profiles: Vec<ReviewerProfile>,
        practice_packs: Vec<PracticePack>,
    ) -> Result<Self, CatalogError> {
        let catalog = Catalog {
            profiles,
            practice_packs,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    /// All reviewer profiles, in frozen catalog order.
    pub fn profiles(&self) -> &[ReviewerProfile] {
        &self.profiles
    }

    /// All practice packs, in frozen catalog order.
    pub fn practice_packs(&self) -> &[PracticePack] {
        &self.practice_packs
    }

    /// Look up a reviewer profile by id.
    pub fn profile(&self, id: &str) -> Option<&ReviewerProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    /// Look up a practice pack by id.
    pub fn practice_pack(&self, id: &str) -> Option<&PracticePack> {
        self.practice_packs.iter().find(|pack| pack.id == id)
    }

    /// The `quality` profile. Validation guarantees its presence.
    pub fn quality(&self) -> &ReviewerProfile {
        self.profile(QUALITY_ID)
            .expect("catalog validation guarantees the quality reviewer")
    }

    /// The `dynamic-domain-expert` profile. Validation guarantees its
    /// presence.
    pub fn dynamic_domain_expert(&self) -> &ReviewerProfile {
        self.profile(DYNAMIC_DOMAIN_EXPERT_ID)
            .expect("catalog validation guarantees the dynamic domain expert")
    }

    /// The set of all practice tags provided by any pack.
    pub fn practice_tags(&self) -> BTreeSet<&str> {
        self.practice_packs
            .iter()
            .flat_map(|pack| pack.tags.iter().map(String::as_str))
            .collect()
    }

    /// Validate every catalog invariant.
    ///
    /// Invariants checked:
    ///
    /// * reviewer ids and practice pack ids are unique;
    /// * the reviewer and pack sets are exactly the frozen built-in sets;
    /// * every profile has a nonempty explicit checklist and at least one
    ///   read-only capability;
    /// * every practice tag referenced by a profile is provided by a pack;
    /// * every capability, language hint, and model profile name is valid.
    fn validate(&self) -> Result<(), CatalogError> {
        validate_id_set(
            self.profiles.iter().map(|profile| profile.id.as_str()),
            BUILTIN_REVIEWER_IDS,
            CatalogError::DuplicateReviewerId,
            CatalogError::UnknownReviewerId,
            CatalogError::MissingReviewer,
        )?;
        validate_id_set(
            self.practice_packs.iter().map(|pack| pack.id.as_str()),
            BUILTIN_PRACTICE_PACK_IDS,
            CatalogError::DuplicatePracticePackId,
            CatalogError::UnknownPracticePackId,
            CatalogError::MissingPracticePack,
        )?;

        let practice_tags = self.practice_tags();
        for profile in &self.profiles {
            validate_profile(profile, &practice_tags)?;
        }
        for pack in &self.practice_packs {
            validate_pack(pack)?;
        }

        let mut practice_ids = BTreeSet::new();
        for pack in &self.practice_packs {
            for practice in &pack.practices {
                if !practice_ids.insert(practice.id.as_str()) {
                    return Err(CatalogError::DuplicatePracticeId(practice.id.clone()));
                }
            }
        }
        Ok(())
    }
}

/// Parse a table of embedded assets with strict schemas.
fn parse_assets<T: serde::de::DeserializeOwned>(
    kind: &'static str,
    assets: &[(&'static str, &'static str)],
) -> Result<Vec<T>, CatalogError> {
    assets
        .iter()
        .map(|(name, text)| {
            serde_json::from_str(text).map_err(|source| CatalogError::MalformedAsset {
                kind,
                name: (*name).to_string(),
                source,
            })
        })
        .collect()
}

/// Validate that a set of present ids is exactly the required frozen set.
fn validate_id_set<'a>(
    present: impl Iterator<Item = &'a str>,
    required: &[&str],
    duplicate: fn(String) -> CatalogError,
    unknown: fn(String) -> CatalogError,
    missing: fn(String) -> CatalogError,
) -> Result<(), CatalogError> {
    let mut seen = BTreeSet::new();
    for id in present {
        if !seen.insert(id) {
            return Err(duplicate(id.to_string()));
        }
        if !required.contains(&id) {
            return Err(unknown(id.to_string()));
        }
    }
    for id in required {
        if !seen.contains(id) {
            return Err(missing((*id).to_string()));
        }
    }
    Ok(())
}

/// Validate one reviewer profile.
fn validate_profile(
    profile: &ReviewerProfile,
    practice_tags: &BTreeSet<&str>,
) -> Result<(), CatalogError> {
    let id = profile.id.as_str();
    let invalid = |problem: String| CatalogError::InvalidProfile {
        id: id.to_string(),
        problem,
    };

    if !is_kebab_id(id) {
        return Err(invalid(
            "id must be nonempty lowercase kebab-case".to_string(),
        ));
    }
    for (field, value) in [
        ("display_name", &profile.display_name),
        ("description", &profile.description),
        ("base_instructions", &profile.base_instructions),
    ] {
        if value.trim().is_empty() {
            return Err(invalid(format!("`{field}` must not be empty")));
        }
    }
    if !CONCEPTUAL_MODEL_PROFILES.contains(&profile.default_model_profile.as_str()) {
        return Err(invalid(format!(
            "default_model_profile `{}` is not a conceptual model profile",
            profile.default_model_profile
        )));
    }

    // Structural reviewers (quality, dynamic domain expert) are added by
    // jury construction, never by scoring; every scored specialist must
    // carry trigger terms.
    if profile.trigger_terms.is_empty() && !STRUCTURAL_REVIEWER_IDS.contains(&id) {
        return Err(invalid(
            "`trigger_terms` must not be empty for scored specialists".to_string(),
        ));
    }
    check_tokens("trigger_terms", &profile.trigger_terms).map_err(&invalid)?;
    check_unique("trigger_terms", &profile.trigger_terms).map_err(&invalid)?;

    for language in &profile.language_hints {
        if !SUPPORTED_LANGUAGES.contains(&language.as_str()) {
            return Err(invalid(format!(
                "language_hint `{language}` is not a supported language"
            )));
        }
    }
    check_unique("language_hints", &profile.language_hints).map_err(&invalid)?;

    for hint in &profile.path_hints {
        if hint.trim().is_empty() || hint.chars().any(char::is_whitespace) {
            return Err(invalid(format!(
                "path_hint `{hint}` must be a nonempty pattern without whitespace"
            )));
        }
    }
    check_unique("path_hints", &profile.path_hints).map_err(&invalid)?;

    if profile.practice_tags.is_empty() && id != DYNAMIC_DOMAIN_EXPERT_ID {
        return Err(invalid(
            "`practice_tags` must not be empty (only the dynamic domain expert receives tags dynamically)"
                .to_string(),
        ));
    }
    check_tokens("practice_tags", &profile.practice_tags).map_err(&invalid)?;
    check_unique("practice_tags", &profile.practice_tags).map_err(&invalid)?;
    for tag in &profile.practice_tags {
        if !practice_tags.contains(tag.as_str()) {
            return Err(CatalogError::UnknownPracticeTag {
                reviewer: id.to_string(),
                tag: tag.clone(),
            });
        }
    }

    if profile.checklist.is_empty() {
        return Err(invalid("checklist must not be empty".to_string()));
    }
    for item in &profile.checklist {
        if item.trim().is_empty() {
            return Err(invalid("checklist items must not be empty".to_string()));
        }
    }
    check_unique("checklist", &profile.checklist).map_err(&invalid)?;

    if profile.allowed_capabilities.is_empty() {
        return Err(invalid(
            "must grant at least one read-only capability".to_string(),
        ));
    }
    let mut capabilities = BTreeSet::new();
    for capability in &profile.allowed_capabilities {
        if !capabilities.insert(capability.name()) {
            return Err(invalid(format!(
                "duplicate capability `{}`",
                capability.name()
            )));
        }
    }
    Ok(())
}

/// Validate one practice pack.
fn validate_pack(pack: &PracticePack) -> Result<(), CatalogError> {
    let id = pack.id.as_str();
    let invalid = |problem: String| CatalogError::InvalidPack {
        id: id.to_string(),
        problem,
    };

    if !is_kebab_id(id) {
        return Err(invalid(
            "id must be nonempty lowercase kebab-case".to_string(),
        ));
    }
    for (field, value) in [("title", &pack.title), ("description", &pack.description)] {
        if value.trim().is_empty() {
            return Err(invalid(format!("`{field}` must not be empty")));
        }
    }
    if pack.tags.is_empty() {
        return Err(invalid("`tags` must not be empty".to_string()));
    }
    check_tokens("tags", &pack.tags).map_err(&invalid)?;
    check_unique("tags", &pack.tags).map_err(&invalid)?;

    if pack.practices.is_empty() {
        return Err(invalid("`practices` must not be empty".to_string()));
    }
    for practice in &pack.practices {
        let practice_id = practice.id.as_str();
        let invalid_practice = |problem: String| CatalogError::InvalidPractice {
            id: practice_id.to_string(),
            problem,
        };
        if !is_practice_id(practice_id) {
            return Err(invalid_practice(
                "id must be lowercase `<pack>:<slug>` form".to_string(),
            ));
        }
        for (field, value) in [
            ("title", &practice.title),
            ("statement", &practice.statement),
        ] {
            if value.trim().is_empty() {
                return Err(invalid_practice(format!("`{field}` must not be empty")));
            }
        }
    }
    Ok(())
}

/// Nonempty lowercase kebab-case identifier.
fn is_kebab_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
}

/// Practice identifier: kebab-case pack namespace, colon, kebab-case slug.
fn is_practice_id(value: &str) -> bool {
    match value.split_once(':') {
        Some((pack, slug)) => is_kebab_id(pack) && is_kebab_id(slug),
        None => false,
    }
}

/// All entries are nonempty single tokens (no whitespace), already
/// lowercase.
fn check_tokens(field: &str, values: &[String]) -> Result<(), String> {
    for value in values {
        if value.trim().is_empty() {
            return Err(format!("`{field}` entries must not be empty"));
        }
        if value.chars().any(char::is_whitespace) {
            return Err(format!(
                "`{field}` entries must be single tokens: `{value}`"
            ));
        }
        if value.chars().any(|c| c.is_uppercase()) {
            return Err(format!("`{field}` entries must be lowercase: `{value}`"));
        }
    }
    Ok(())
}

/// All entries are unique.
fn check_unique(field: &str, values: &[String]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value.as_str()) {
            return Err(format!("`{field}` contains a duplicate entry: `{value}`"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::ThinkingLevel;
    use crate::domain::practice::Practice;
    use crate::domain::reviewer::ReviewerCapability;

    fn bundled_parts() -> (Vec<ReviewerProfile>, Vec<PracticePack>) {
        let catalog = Catalog::bundled().expect("bundled catalog must load");
        (catalog.profiles.clone(), catalog.practice_packs.clone())
    }

    #[test]
    fn bundled_catalog_loads_and_validates() {
        let catalog = Catalog::bundled().expect("bundled catalog must be valid");
        assert_eq!(catalog.profiles().len(), BUILTIN_REVIEWER_IDS.len());
        assert_eq!(
            catalog.practice_packs().len(),
            BUILTIN_PRACTICE_PACK_IDS.len()
        );
        for id in BUILTIN_PRACTICE_PACK_IDS {
            assert!(catalog.practice_pack(id).is_some(), "pack `{id}` missing");
        }
    }

    #[test]
    fn reviewer_ids_are_unique_and_exactly_the_frozen_set() {
        let catalog = Catalog::bundled().unwrap();
        let ids: Vec<&str> = catalog.profiles().iter().map(|p| p.id.as_str()).collect();
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "reviewer ids must be unique");
        for required in BUILTIN_REVIEWER_IDS {
            assert!(unique.contains(required), "missing reviewer `{required}`");
        }
        assert_eq!(unique.len(), BUILTIN_REVIEWER_IDS.len());
    }

    #[test]
    fn practice_pack_ids_are_unique_and_exactly_the_frozen_set() {
        let catalog = Catalog::bundled().unwrap();
        let ids: Vec<&str> = catalog
            .practice_packs()
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "practice pack ids must be unique");
        for required in BUILTIN_PRACTICE_PACK_IDS {
            assert!(
                unique.contains(required),
                "missing practice pack `{required}`"
            );
        }
        assert_eq!(unique.len(), BUILTIN_PRACTICE_PACK_IDS.len());
    }

    #[test]
    fn quality_exists_and_is_structural() {
        let catalog = Catalog::bundled().unwrap();
        let quality = catalog.quality();
        assert_eq!(quality.id, QUALITY_ID);
        // Quality is added deterministically, never scored.
        assert!(
            quality.trigger_terms.is_empty(),
            "quality must not be trigger-selected"
        );
        assert!(!quality.checklist.is_empty());
    }

    #[test]
    fn dynamic_domain_expert_exists_and_is_never_scored() {
        let catalog = Catalog::bundled().unwrap();
        let expert = catalog.dynamic_domain_expert();
        assert_eq!(expert.id, DYNAMIC_DOMAIN_EXPERT_ID);
        assert!(expert.trigger_terms.is_empty());
        assert!(expert.practice_tags.is_empty());
        assert!(!expert.checklist.is_empty());
    }

    #[test]
    fn every_checklist_is_nonempty_and_explicit() {
        let catalog = Catalog::bundled().unwrap();
        for profile in catalog.profiles() {
            assert!(
                !profile.checklist.is_empty(),
                "reviewer `{}` has an empty checklist",
                profile.id
            );
            for item in &profile.checklist {
                assert!(!item.trim().is_empty());
            }
        }
    }

    #[test]
    fn every_practice_reference_is_valid() {
        let catalog = Catalog::bundled().unwrap();
        let tags = catalog.practice_tags();
        for profile in catalog.profiles() {
            for tag in &profile.practice_tags {
                assert!(
                    tags.contains(tag.as_str()),
                    "reviewer `{}` references unknown practice tag `{tag}`",
                    profile.id
                );
            }
        }
        // Every pack's own id is its primary tag.
        for pack in catalog.practice_packs() {
            assert!(
                pack.tags.iter().any(|tag| tag == &pack.id),
                "pack `{}` must carry its own id as a tag",
                pack.id
            );
        }
    }

    #[test]
    fn every_capability_name_is_valid_and_read_only() {
        let catalog = Catalog::bundled().unwrap();
        let valid: BTreeSet<&str> = ReviewerCapability::ALL
            .iter()
            .map(|capability| capability.name())
            .collect();
        for profile in catalog.profiles() {
            assert!(
                !profile.allowed_capabilities.is_empty(),
                "reviewer `{}` grants no capabilities",
                profile.id
            );
            for capability in &profile.allowed_capabilities {
                assert!(
                    valid.contains(capability.name()),
                    "reviewer `{}` grants unknown capability `{}`",
                    profile.id,
                    capability.name()
                );
            }
        }
    }

    #[test]
    fn every_profile_declares_a_conceptual_model_profile() {
        let catalog = Catalog::bundled().unwrap();
        for profile in catalog.profiles() {
            assert!(
                CONCEPTUAL_MODEL_PROFILES.contains(&profile.default_model_profile.as_str()),
                "reviewer `{}` uses unknown model profile `{}`",
                profile.id,
                profile.default_model_profile
            );
        }
    }

    #[test]
    fn practice_ids_are_globally_unique() {
        let catalog = Catalog::bundled().unwrap();
        let mut seen = BTreeSet::new();
        for pack in catalog.practice_packs() {
            for practice in &pack.practices {
                assert!(seen.insert(practice.id.as_str()), "duplicate practice id");
                assert!(practice.id.starts_with(&format!("{}:", pack.id)));
            }
        }
    }

    #[test]
    fn malformed_assets_are_rejected() {
        let bad_json = [("broken", "{ not valid json")];
        let err = parse_assets::<ReviewerProfile>("reviewer", &bad_json).unwrap_err();
        assert!(matches!(err, CatalogError::MalformedAsset { name, .. } if name == "broken"));

        // Unknown fields and invalid enum names are rejected by strict
        // parsing.
        let bad_field = [(
            "unknown-field",
            r#"{"id":"x","display_name":"X","description":"d","trigger_terms":[],
                "language_hints":[],"path_hints":[],"practice_tags":[],"checklist":["c"],
                "base_instructions":"b","default_model_profile":"standard",
                "default_thinking_level":"medium","allowed_capabilities":["shell"]}"#,
        )];
        let err = parse_assets::<ReviewerProfile>("reviewer", &bad_field).unwrap_err();
        assert!(matches!(err, CatalogError::MalformedAsset { .. }));

        let bad_capability = [(
            "bad-capability",
            r#"{"id":"x","display_name":"X","description":"d","trigger_terms":[],
                "language_hints":[],"path_hints":[],"practice_tags":[],"checklist":["c"],
                "base_instructions":"b","default_model_profile":"standard",
                "default_thinking_level":"medium","allowed_capabilities":["edit"]}"#,
        )];
        assert!(
            matches!(
                parse_assets::<ReviewerProfile>("reviewer", &bad_capability).unwrap_err(),
                CatalogError::MalformedAsset { .. }
            ),
            "write/edit capabilities must not parse"
        );
    }

    #[test]
    fn duplicate_reviewer_id_is_rejected() {
        let (mut profiles, packs) = bundled_parts();
        profiles.push(profile_with_id("testing"));
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(err, CatalogError::DuplicateReviewerId(id) if id == "testing"));
    }

    #[test]
    fn unknown_and_missing_reviewers_are_rejected() {
        let (mut profiles, packs) = bundled_parts();
        let extra = profile_with_id("linting");
        profiles.push(extra.clone());
        let err = Catalog::new(profiles.clone(), packs.clone()).unwrap_err();
        assert!(matches!(err, CatalogError::UnknownReviewerId(id) if id == "linting"));

        profiles.pop();
        profiles.retain(|profile| profile.id != "api-design");
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(err, CatalogError::MissingReviewer(id) if id == "api-design"));
    }

    #[test]
    fn duplicate_and_missing_practice_packs_are_rejected() {
        let (profiles, mut packs) = bundled_parts();
        packs.push(packs[0].clone());
        let err = Catalog::new(profiles.clone(), packs.clone()).unwrap_err();
        assert!(matches!(err, CatalogError::DuplicatePracticePackId(id) if id == packs[0].id));

        packs.pop();
        packs.retain(|pack| pack.id != "database");
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(err, CatalogError::MissingPracticePack(id) if id == "database"));
    }

    #[test]
    fn unknown_practice_tag_is_rejected() {
        let (mut profiles, packs) = bundled_parts();
        let index = profiles.iter().position(|p| p.id == "quality").unwrap();
        profiles[index]
            .practice_tags
            .push("no-such-tag".to_string());
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::UnknownPracticeTag { reviewer, tag }
            if reviewer == "quality" && tag == "no-such-tag"
        ));
    }

    #[test]
    fn empty_checklist_is_rejected() {
        let (mut profiles, packs) = bundled_parts();
        let index = profiles.iter().position(|p| p.id == "testing").unwrap();
        profiles[index].checklist.clear();
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::InvalidProfile { id, problem }
            if id == "testing" && problem.contains("checklist")
        ));
    }

    #[test]
    fn specialist_without_trigger_terms_is_rejected_but_structural_reviewers_are_not() {
        let (mut profiles, packs) = bundled_parts();
        let index = profiles.iter().position(|p| p.id == "testing").unwrap();
        profiles[index].trigger_terms.clear();
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::InvalidProfile { id, problem }
            if id == "testing" && problem.contains("trigger_terms")
        ));

        // quality and dynamic-domain-expert are structural: empty trigger
        // terms are correct for them.
        let (mut profiles, packs) = bundled_parts();
        let quality = profiles.iter().position(|p| p.id == QUALITY_ID).unwrap();
        profiles[quality].trigger_terms.clear();
        let expert = profiles
            .iter()
            .position(|p| p.id == DYNAMIC_DOMAIN_EXPERT_ID)
            .unwrap();
        profiles[expert].trigger_terms.clear();
        Catalog::new(profiles, packs).expect("structural reviewers need no trigger terms");
    }

    #[test]
    fn invalid_language_hint_is_rejected() {
        let (mut profiles, packs) = bundled_parts();
        let index = profiles.iter().position(|p| p.id == "testing").unwrap();
        profiles[index].language_hints.push("kotlin".to_string());
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::InvalidProfile { id, problem }
            if id == "testing" && problem.contains("kotlin")
        ));
    }

    #[test]
    fn invalid_model_profile_name_is_rejected() {
        let (mut profiles, packs) = bundled_parts();
        let index = profiles.iter().position(|p| p.id == "testing").unwrap();
        profiles[index].default_model_profile = "gpt-9-turbo".to_string();
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::InvalidProfile { id, problem }
            if id == "testing" && problem.contains("default_model_profile")
        ));
    }

    #[test]
    fn empty_capabilities_is_rejected() {
        let (mut profiles, packs) = bundled_parts();
        let index = profiles.iter().position(|p| p.id == "testing").unwrap();
        profiles[index].allowed_capabilities.clear();
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::InvalidProfile { id, problem }
            if id == "testing" && problem.contains("capability")
        ));
    }

    #[test]
    fn duplicate_practice_id_across_packs_is_rejected() {
        let (profiles, mut packs) = bundled_parts();
        let duplicate_id = packs[1].practices[0].id.clone();
        packs[0].practices.push(Practice {
            id: duplicate_id,
            title: "Duplicate".to_string(),
            statement: "Statement.".to_string(),
        });
        let err = Catalog::new(profiles, packs).unwrap_err();
        assert!(matches!(err, CatalogError::DuplicatePracticeId(_)));
    }

    #[test]
    fn empty_pack_tags_and_practices_are_rejected() {
        let catalog = Catalog::bundled().unwrap();

        let mut no_tags = catalog.practice_packs()[0].clone();
        no_tags.tags.clear();
        let err = validate_pack(&no_tags).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::InvalidPack { problem, .. } if problem.contains("tags")
        ));

        let mut no_practices = catalog.practice_packs()[0].clone();
        no_practices.practices.clear();
        let err = validate_pack(&no_practices).unwrap_err();
        assert!(matches!(
            err,
            CatalogError::InvalidPack { problem, .. } if problem.contains("practices")
        ));
    }

    #[test]
    fn bundled_profiles_roundtrip_through_serde() {
        let catalog = Catalog::bundled().unwrap();
        for profile in catalog.profiles() {
            let json = serde_json::to_string(profile).unwrap();
            let back: ReviewerProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(profile, &back);
        }
        for pack in catalog.practice_packs() {
            let json = serde_json::to_string(pack).unwrap();
            let back: PracticePack = serde_json::from_str(&json).unwrap();
            assert_eq!(pack, &back);
        }
    }

    /// Build a minimal syntactically valid profile with a chosen id.
    fn profile_with_id(id: &str) -> ReviewerProfile {
        ReviewerProfile {
            id: id.to_string(),
            display_name: "X".to_string(),
            description: "Test profile.".to_string(),
            trigger_terms: vec!["x".to_string()],
            language_hints: Vec::new(),
            path_hints: Vec::new(),
            practice_tags: vec!["quality".to_string()],
            checklist: vec!["Check something.".to_string()],
            base_instructions: "Work the checklist.".to_string(),
            default_model_profile: "standard".to_string(),
            default_thinking_level: ThinkingLevel::Medium,
            allowed_capabilities: ReviewerCapability::ALL.to_vec(),
        }
    }
}
