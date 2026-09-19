//! Deterministic routing (plan section 33).
//!
//! - [`rules`] implements the mandatory [`RulesClassifier`]: trigger
//!   terms, language hints, path hints, practice tags, and lesson
//!   boosts over the static reviewer catalog;
//! - [`pathglob`] provides the deterministic glob matching used by
//!   path hints and lesson routing hints;
//! - [`fixtures`] holds the frozen routing fixtures shared by the
//!   routing tests and `nisse eval routing` (milestone 23).
//!
//! Routing never consults an LLM. Lesson wording never influences
//! routing (plan section 51); lessons participate only through their
//! deterministic routing hints, which may raise scores but never
//! suppress a reviewer. Jury construction
//! ([`jury::plan::build_jury_plan`]) enforces the frozen bounds:
//! `quality` always participates, at most three scored specialists
//! join, the jury totals at most four reviewers, and the dynamic
//! domain expert is the fallback when no specialist reaches the
//! threshold.

pub mod fixtures;
pub mod pathglob;
pub mod rules;

pub use fixtures::{RoutingFixture, routing_fixtures, verify_fixture};
pub use pathglob::glob_match;
pub use rules::{
    DEFAULT_THRESHOLD, LANGUAGE_WEIGHT, PATH_WEIGHT, RulesClassifier, TAG_WEIGHT,
    TRIGGER_SATURATION_TERMS, TRIGGER_WEIGHT,
};
