//! Frozen routing fixtures and their verifier (plan sections 33 and 69).
//!
//! These fixtures are the shared definition of "sensible bounded
//! juries" for rules-based routing. They are used by the routing tests
//! here and by `nisse eval routing` (milestone 23). Fixture semantics:
//!
//! - `expected_required_reviewers` must all hold jury seats;
//! - `expected_forbidden_reviewers` must hold no seats;
//! - every selected jury member must be required or appear in some
//!   `acceptable_any_groups` group (optional but sensible reviewers).
//!
//! The structural `quality` reviewer is always required; the dynamic
//! domain expert appears only in the unknown-domain fallback fixture.

use crate::domain::jury::JuryPlan;
use crate::domain::routing::RoutingInput;

/// One frozen routing evaluation fixture (plan section 69).
#[derive(Debug, Clone)]
pub struct RoutingFixture {
    /// Stable fixture name.
    pub name: &'static str,
    /// What the fixture exercises.
    pub description: &'static str,
    /// The routing input under test.
    pub input: RoutingInput,
    /// Reviewers that must be selected.
    pub expected_required_reviewers: Vec<&'static str>,
    /// Reviewers that must not be selected.
    pub expected_forbidden_reviewers: Vec<&'static str>,
    /// Groups of optional reviewers; any jury member that is not
    /// required must belong to at least one group.
    pub acceptable_any_groups: Vec<Vec<&'static str>>,
}

/// All frozen routing fixtures, in frozen order.
pub fn routing_fixtures() -> Vec<RoutingFixture> {
    vec![
        cortex_m_isr_queue(),
        sql_migration(),
        oauth_login(),
        ble_framing(),
        maintainability_refactor(),
        hot_path_performance(),
        unknown_niche_domain(),
    ]
}

/// Verify a jury plan against a fixture. Returns a human-readable
/// description of the first violation.
pub fn verify_fixture(plan: &JuryPlan, fixture: &RoutingFixture) -> Result<(), String> {
    let members = plan.reviewer_ids();
    for required in &fixture.expected_required_reviewers {
        if !plan.contains(required) {
            return Err(format!(
                "fixture `{}` requires reviewer `{required}`; jury: {members:?}",
                fixture.name
            ));
        }
    }
    for forbidden in &fixture.expected_forbidden_reviewers {
        if plan.contains(forbidden) {
            return Err(format!(
                "fixture `{}` forbids reviewer `{forbidden}`; jury: {members:?}",
                fixture.name
            ));
        }
    }
    let acceptable: Vec<&str> = fixture
        .acceptable_any_groups
        .iter()
        .flatten()
        .copied()
        .collect();
    for member in &members {
        let required = fixture.expected_required_reviewers.contains(member);
        let acceptable = acceptable.contains(member);
        if !required && !acceptable {
            return Err(format!(
                "fixture `{}`: jury member `{member}` is neither required nor acceptable; jury: {members:?}",
                fixture.name
            ));
        }
    }
    Ok(())
}

fn input(
    descriptors: &[&str],
    changed_paths: &[&str],
    languages: &[&str],
    changed_symbols: &[&str],
    deterministic_tags: &[&str],
) -> RoutingInput {
    RoutingInput {
        descriptors: descriptors.iter().map(|s| s.to_string()).collect(),
        changed_paths: changed_paths.iter().map(|s| s.to_string()).collect(),
        languages: languages.iter().map(|s| s.to_string()).collect(),
        changed_symbols: changed_symbols.iter().map(|s| s.to_string()).collect(),
        deterministic_tags: deterministic_tags.iter().map(|s| s.to_string()).collect(),
        lessons: Vec::new(),
    }
}

fn fixture(
    name: &'static str,
    description: &'static str,
    input: RoutingInput,
    required: &[&'static str],
    forbidden: &[&'static str],
    acceptable: &[&[&'static str]],
) -> RoutingFixture {
    RoutingFixture {
        name,
        description,
        input,
        expected_required_reviewers: required.to_vec(),
        expected_forbidden_reviewers: forbidden.to_vec(),
        acceptable_any_groups: acceptable.iter().map(|group| group.to_vec()).collect(),
    }
}

fn cortex_m_isr_queue() -> RoutingFixture {
    fixture(
        "cortex-m-isr-queue",
        "Embedded firmware change sharing a queue between an ISR and the main loop",
        input(
            &[
                "Cortex-M sensor firmware: the sample ISR writes the shared queue; the main loop drains it",
            ],
            &["firmware/src/isr_handler.c", "firmware/src/queue.c"],
            &["c"],
            &["sensor_isr_handler", "queue_push", "queue_drain"],
            &["concurrency", "embedded", "queues"],
        ),
        &["quality", "concurrency", "embedded-resource"],
        &[
            "testing",
            "security",
            "performance",
            "protocol",
            "networking",
            "persistence",
            "database",
            "api-design",
            "error-handling",
            "maintainability",
        ],
        &[&["realtime"]],
    )
}

fn sql_migration() -> RoutingFixture {
    fixture(
        "sql-migration",
        "Schema migration adding a table with an index under db/migrations",
        input(
            &["Add users table migration with a unique index and transactional cutover"],
            &["db/migrations/0007_add_users.sql", "db/schema.sql"],
            &[],
            &["create_users_table"],
            &["database", "sql"],
        ),
        &["quality", "database"],
        &[
            "concurrency",
            "security",
            "testing",
            "embedded-resource",
            "realtime",
            "protocol",
            "networking",
            "api-design",
            "error-handling",
            "maintainability",
            "performance",
        ],
        &[&["persistence"]],
    )
}

fn oauth_login() -> RoutingFixture {
    fixture(
        "oauth-login",
        "Authentication change adding an OAuth login flow with token validation",
        input(
            &["Add OAuth login flow with auth token validation and session storage"],
            &["src/auth/oauth.rs", "src/auth/session.rs"],
            &["rust"],
            &["oauth_client", "validate_token", "store_session"],
            &["security", "auth"],
        ),
        &["quality", "security"],
        &[
            "concurrency",
            "embedded-resource",
            "realtime",
            "protocol",
            "database",
            "api-design",
            "error-handling",
            "maintainability",
            "performance",
            "testing",
        ],
        &[&["networking"], &["persistence"]],
    )
}

fn ble_framing() -> RoutingFixture {
    fixture(
        "ble-framing",
        "Wire-protocol change fixing packet framing and codec handling",
        input(
            &["Fix BLE packet framing bug in the GATT notification handler"],
            &["src/protocol/ble_frame.rs"],
            &["rust"],
            &["encode_frame", "decode_packet", "crc32"],
            &["protocol", "framing"],
        ),
        &["quality", "protocol"],
        &[
            "concurrency",
            "security",
            "embedded-resource",
            "database",
            "persistence",
            "api-design",
            "error-handling",
            "maintainability",
            "performance",
            "testing",
        ],
        &[&["networking"], &["realtime"]],
    )
}

fn maintainability_refactor() -> RoutingFixture {
    fixture(
        "maintainability-refactor",
        "Structural refactor removing duplication and dead code",
        input(
            &[
                "Refactor the oversized order module: extract services, remove dead code and duplication",
            ],
            &["src/legacy/orders.rs", "src/legacy/order_service.rs"],
            &["rust"],
            &["split_order_module", "OrderService"],
            &["maintainability"],
        ),
        &["quality", "maintainability"],
        &[
            "concurrency",
            "security",
            "embedded-resource",
            "realtime",
            "protocol",
            "networking",
            "persistence",
            "database",
            "error-handling",
            "performance",
        ],
        &[&["testing"], &["api-design"]],
    )
}

fn hot_path_performance() -> RoutingFixture {
    fixture(
        "hot-path-performance",
        "Performance change reducing allocation and copying in a hot path",
        input(
            &["Reduce allocation and needless copy in the hot render path"],
            &["src/render/tile.rs"],
            &["rust"],
            &["render_tile"],
            &["performance"],
        ),
        &["quality", "performance"],
        &[
            "concurrency",
            "security",
            "embedded-resource",
            "database",
            "persistence",
            "api-design",
            "error-handling",
            "maintainability",
            "testing",
            "networking",
        ],
        &[&["realtime"], &["protocol"]],
    )
}

fn unknown_niche_domain() -> RoutingFixture {
    fixture(
        "unknown-niche-domain",
        "Niche domain no static specialist covers; dynamic expert fallback must fire",
        input(
            &["Tune procedural texture synthesis noise parameters for the marble shader"],
            &["assets/shaders/marble.wgsl"],
            &[],
            &["fbm_noise3"],
            &["shader"],
        ),
        &["quality", "dynamic-domain-expert"],
        &[
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
        ],
        &[],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jury::catalog::Catalog;
    use crate::jury::plan::build_jury_plan;
    use crate::routing::rules::RulesClassifier;

    fn plan_for(input: &RoutingInput) -> JuryPlan {
        let catalog = Catalog::bundled().expect("bundled catalog must load");
        let decision = RulesClassifier::new(catalog.clone()).classify(input);
        build_jury_plan(&decision, &catalog).expect("valid decision must build a jury")
    }

    #[test]
    fn fixtures_are_unique_and_frozen() {
        let fixtures = routing_fixtures();
        let names: Vec<&str> = fixtures.iter().map(|f| f.name).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "fixture names must be unique");
        assert_eq!(
            names,
            vec![
                "cortex-m-isr-queue",
                "sql-migration",
                "oauth-login",
                "ble-framing",
                "maintainability-refactor",
                "hot-path-performance",
                "unknown-niche-domain",
            ]
        );
        for fixture in &fixtures {
            assert!(!fixture.description.is_empty());
            assert!(
                fixture.expected_required_reviewers.contains(&"quality"),
                "fixture `{}` must require the quality reviewer",
                fixture.name
            );
        }
    }

    #[test]
    fn every_fixture_passes_verification() {
        for fixture in routing_fixtures() {
            let plan = plan_for(&fixture.input);
            verify_fixture(&plan, &fixture).unwrap_or_else(|err| panic!("{err}"));
        }
    }

    #[test]
    fn every_fixture_produces_a_bounded_quality_led_jury() {
        for fixture in routing_fixtures() {
            let plan = plan_for(&fixture.input);
            assert!(
                (2..=4).contains(&plan.size()),
                "fixture `{}` jury size {} outside 2..=4",
                fixture.name,
                plan.size()
            );
            assert_eq!(
                plan.reviewer_ids().first(),
                Some(&"quality"),
                "fixture `{}` jury must be quality-led",
                fixture.name
            );
        }
    }

    #[test]
    fn cortex_m_fixture_selects_concurrency_and_embedded() {
        let fixture = cortex_m_isr_queue();
        let plan = plan_for(&fixture.input);
        assert_eq!(
            plan.reviewer_ids(),
            vec!["quality", "concurrency", "embedded-resource"]
        );
    }

    #[test]
    fn sql_migration_fixture_selects_database() {
        let fixture = sql_migration();
        let plan = plan_for(&fixture.input);
        assert_eq!(plan.reviewer_ids(), vec!["quality", "database"]);
    }

    #[test]
    fn oauth_fixture_selects_security() {
        let fixture = oauth_login();
        let plan = plan_for(&fixture.input);
        assert_eq!(plan.reviewer_ids(), vec!["quality", "security"]);
    }

    #[test]
    fn ble_fixture_selects_protocol() {
        let fixture = ble_framing();
        let plan = plan_for(&fixture.input);
        assert_eq!(plan.reviewer_ids(), vec!["quality", "protocol"]);
    }

    #[test]
    fn maintainability_fixture_selects_maintainability() {
        let fixture = maintainability_refactor();
        let plan = plan_for(&fixture.input);
        assert_eq!(plan.reviewer_ids(), vec!["quality", "maintainability"]);
    }

    #[test]
    fn performance_fixture_selects_performance() {
        let fixture = hot_path_performance();
        let plan = plan_for(&fixture.input);
        assert_eq!(plan.reviewer_ids(), vec!["quality", "performance"]);
    }

    #[test]
    fn unknown_domain_fixture_falls_back_to_the_dynamic_expert() {
        let fixture = unknown_niche_domain();
        let plan = plan_for(&fixture.input);
        assert!(plan.uses_dynamic_fallback());
        assert_eq!(
            plan.reviewer_ids(),
            vec!["quality", "dynamic-domain-expert"]
        );
    }

    #[test]
    fn verify_fixture_reports_each_violation_kind() {
        let fixture = cortex_m_isr_queue();
        let plan = plan_for(&fixture.input);

        // Missing required reviewer.
        let mut missing = fixture.clone();
        missing.expected_required_reviewers.push("database");
        assert!(
            verify_fixture(&plan, &missing)
                .unwrap_err()
                .contains("requires reviewer `database`")
        );

        // Forbidden reviewer present.
        let mut forbidding = fixture.clone();
        forbidding.expected_forbidden_reviewers.push("concurrency");
        assert!(
            verify_fixture(&plan, &forbidding)
                .unwrap_err()
                .contains("forbids reviewer `concurrency`")
        );

        // Jury member neither required nor acceptable.
        let mut strict = fixture.clone();
        strict.acceptable_any_groups.clear();
        strict.expected_forbidden_reviewers.clear();
        // Remove every selected reviewer from the required list except
        // quality: the specialists become unaccounted members.
        strict
            .expected_required_reviewers
            .retain(|id| *id == "quality");
        assert!(
            verify_fixture(&plan, &strict)
                .unwrap_err()
                .contains("neither required nor acceptable")
        );

        // The untouched fixture passes.
        assert_eq!(verify_fixture(&plan, &fixture), Ok(()));
    }
}
