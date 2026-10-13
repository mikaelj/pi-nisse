//! Final 1.0 regression suite (plan milestone 30).
//!
//! The frozen final regression scenarios A–AC are verified as follows.
//! Each mandatory scenario has a dedicated, named verification:
//!
//! | Scenario | Verification |
//! | -------- | ------------ |
//! | A. PLAN REVIEW | `plan_review::normal_routing_selects_specialists_and_quality_always_leads` and the rest of `plan_review` (substantial plan, dynamic jury, evidence) |
//! | B. MAIN PI PLAN LOOP | `integrations/pi/test/workflow-policy.test.ts` — "exits the plan loop early once blocker/high findings are resolved" |
//! | C. MAIN PI PLAN LOOP LIMIT | workflow-policy tests — "stops before implementation after three unresolved plan reviews" |
//! | D. WORKING CODE REVIEW | `code_review::working_mode_reviews_the_final_state_end_to_end` |
//! | E. CODE REVIEW LOOP | workflow-policy tests — "finishes after one clean plan review and one clean code review" and "submits feedback only for known outcomes" |
//! | F. CODE REVIEW LOOP LIMIT | workflow-policy tests — "stops incomplete after three unresolved code reviews without finishing" |
//! | G. STAGED REVIEW | `code_review::staged_mode_reviews_only_the_index` |
//! | H. HISTORICAL REVIEW | `code_review::commit_mode_reviews_history_with_zero_snapshot_leakage` and `pi_tools::historical_commit_snapshots_never_serve_current_files` |
//! | I. WORKTREES | `code_review::linked_worktrees_review_their_own_final_state_only`, `pi_tools::linked_worktrees_with_equal_paths_stay_isolated`, `cache_binary::linked_worktrees_share_the_cache_and_keep_private_worktree_databases` |
//! | J. RULES-ONLY | `rules_only_review_works_without_needle_or_embeddings` (below) |
//! | K. NEEDLE FALLBACK | `needle_binary::plan_review_falls_back_to_rules_when_needle_is_down` |
//! | L. FACT LEARNING | `facts_binary::supported_candidate_with_identity_becomes_a_durable_fact` |
//! | M. UNSUPPORTED FACT | `facts_binary::contradicted_candidate_is_never_persisted` |
//! | N. STALE FACT | `fact_staleness` (detection, downranking, all three revalidation results) |
//! | O. FEEDBACK | `feedback_binary::each_outcome_creates_durable_observations_from_the_stored_review` |
//! | P. LESSON | `lessons_binary::repeated_outcomes_form_a_deterministic_lesson` |
//! | Q. LESSON MERGE | `compact_binary::overlapping_lessons_merge_without_double_counting` |
//! | R. FACT CONSOLIDATION | `compact_binary::compatible_facts_merge_into_one_verified_common_fact` |
//! | S. UNSAFE CONSOLIDATION | `compact_binary::unsafe_merge_refusal_retains_the_unresolved_conflict` |
//! | T. CACHE LOSS | `cache_loss_preserves_durable_records_and_rebuilds` (below) |
//! | U. TEAM GIT MERGE | `different_contributor_records_survive_team_merge` (below) |
//! | V. PI INTEGRATION | `integrations/pi/test/nisse.test.ts` (tools invoke the `nisse` binary; thin adapter) |
//! | W. MCP | `mcp_binary` (initialize, tools, mocked reviews, query, feedback, stdout purity) |
//! | X. WINDOWS | `windows_compat` on the Windows CI matrix |
//! | Y. MALFORMED REVIEWER | `pi_reviewer::pi_runtime_fails_when_repair_stays_malformed` |
//! | Z. HALLUCINATING SYNTHESIZER | `pi_synthesis::scripted_pi_synthesis_invention_and_raise_are_rejected` |
//! | AA. HALLUCINATING FACT REVIEWER | `facts_binary::contradicted_candidate_is_never_persisted` |
//! | AB. TRIVIAL PI TASK | workflow-policy tests — "skips every Nisse tool for a trivial task" |
//! | AC. CORE MODE INVARIANT | `frozen_cli_surface_has_no_plan_or_build_mode` (below) plus the unit-level `cli::tests::command_surface_is_frozen_and_has_no_task_mode` |

use std::path::{Path, PathBuf};

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

/// A plan whose queue/race language routes to [quality, concurrency].
const QUEUE_PLAN: &str = "\
# Fix the race on the shared queue

## Approach

Move queue mutation behind a mutex and document the locking order.

## Tests

Add a regression test for concurrent enqueue.
";

const QUEUE_RS: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n";

const QUEUE_RS_V2: &str = "pub struct Queue {\n    slots: [u8; 32],\n}\n\npub fn enqueue(queue: &mut Queue, item: u8) {\n    queue.slots[0] = item;\n}\n\npub fn drain(queue: &mut Queue) -> u8 {\n    queue.slots[0]\n}\n";

/// Create a repository with one committed rust source file and an
/// unstaged working-tree modification of it.
fn dirty_rust_repo(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new()
        .prefix(label)
        .tempdir()
        .expect("temp dir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("create src");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS).expect("write source");
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@t"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    git(&repo, &["config", "user.name", "T"]);
    git(&repo, &["add", "--all"]);
    commit(&repo, "one");
    std::fs::write(repo.join("src").join("queue.rs"), QUEUE_RS_V2).expect("modify source");
    let home = TempDir::new().expect("home");
    (dir, repo, home)
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("LC_ALL", "C")
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

fn commit(repo: &Path, message: &str) {
    git(
        repo,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

/// Configure the contributor identity `id` in `home`.
fn configure_identity(home: &Path, id: &str) {
    write_user_config(home, &format!("[contributor]\nid = \"{id}\"\n"));
}

/// The durable observation records of `repo` under one contributor.
fn observation_files(repo: &Path, contributor: &str) -> Vec<PathBuf> {
    let dir = repo.join(".nisse").join("people").join(contributor);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.to_string_lossy().ends_with(".json"))
        .collect();
    paths.sort();
    paths
}

/// Run one fake-runtime working code review and return its strict JSON
/// result (`review_id`, `findings[0].id`).
fn review_working(repo: &Path, home: &Path) -> serde_json::Value {
    let out = run_nisse_in_with_env(repo, &["review", "code", "--working", "--json"], home, &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    serde_json::from_str(&stdout).expect("strict review JSON")
}

/// Scenario J: reviews run end to end with rules-only routing and no
/// embeddings — no Needle anywhere (plan section 34: Needle is
/// optional; rules always exist).
#[test]
fn rules_only_review_works_without_needle_or_embeddings() {
    let (_dir, repo, home) = dirty_rust_repo("final-rules-only");
    // Explicitly no helper providers, plus valid model profiles so the
    // only optional dependency (Needle) is provably absent.
    write_user_config(
        home.path(),
        "\
[model_profiles.standard]
model = \"local-standard\"
endpoint = \"http://127.0.0.1:1000/v1\"

[model_profiles.strong]
model = \"local-strong\"
endpoint = \"http://127.0.0.1:1001/v1\"

[classifier]
provider = \"none\"

[embeddings]
provider = \"none\"
",
    );
    std::fs::write(repo.join("plan.md"), QUEUE_PLAN).expect("write plan");
    let out = run_nisse_in_with_env(
        &repo,
        &["review", "plan", "--file", "plan.md"],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("nisse plan review "), "{stdout}");

    // The rules classifier selected the expected deterministic jury.
    let db = repo.join(".git/nisse/cache.sqlite");
    let conn = rusqlite::Connection::open(&db).expect("open shared cache");
    let payload: String = conn
        .query_row(
            "SELECT payload FROM review_event ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("read review event");
    let event: serde_json::Value = serde_json::from_str(&payload).expect("event JSON");
    let jury: Vec<String> = event["jury"]["seats"]
        .as_array()
        .expect("jury seats")
        .iter()
        .map(|seat| seat["reviewer_id"].as_str().expect("seat id").to_string())
        .collect();
    assert_eq!(jury, vec!["quality", "concurrency"]);
    // The deterministic rules decision selected the specialists.
    assert_eq!(
        event["decision"]["selected_specialists"],
        serde_json::json!(["concurrency"])
    );
    assert_eq!(event["decision"]["dynamic_fallback"], false);
}

/// Scenario T: deleting all generated SQLite state keeps durable
/// records alive and the caches rebuild on the next operation
/// (permanent invariants 4 and 20).
#[test]
fn cache_loss_preserves_durable_records_and_rebuilds() {
    let (_dir, repo, home) = dirty_rust_repo("final-cache-loss");
    configure_identity(home.path(), "alice");

    // A review and a known outcome create both generated caches and
    // durable records.
    let result = review_working(&repo, home.path());
    let review_id = result["review_id"].as_str().expect("review id").to_string();
    let finding = result["findings"][0]["id"].as_str().expect("finding id");
    let out = run_nisse_in_with_env(
        &repo,
        &["feedback", &review_id, finding, "--fixed"],
        home.path(),
        &[],
    );
    assert!(out.status.success());
    let generated = repo.join(".git/nisse");
    assert!(generated.join("cache.sqlite").is_file());
    assert!(generated.join("worktree.sqlite").is_file());
    assert_eq!(observation_files(&repo, "alice").len(), 2);

    // Delete every generated database and journal sidecar.
    std::fs::remove_dir_all(&generated).expect("delete generated state");

    // Durable records survive and load strictly without any cache.
    let out = run_nisse_in_with_env(&repo, &["records", "list"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert_eq!(stderr, "");
    assert!(stdout.contains("observations (2)"), "{stdout}");
    assert!(stdout.contains("submitted by alice"), "{stdout}");
    assert_eq!(observation_files(&repo, "alice").len(), 2);

    // Status reports the loss without recreating anything.
    let out = run_nisse_in_with_env(&repo, &["cache", "status"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{stdout}");
    assert_eq!(stdout.matches("status: not created").count(), 2, "{stdout}");

    // The next review rebuilds the caches and succeeds.
    let result = review_working(&repo, home.path());
    assert!(
        result["review_id"]
            .as_str()
            .is_some_and(|id| id != review_id),
        "a fresh review event was persisted"
    );
    assert!(generated.join("cache.sqlite").is_file());
    assert!(generated.join("worktree.sqlite").is_file());
    let out = run_nisse_in_with_env(&repo, &["cache", "status"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{stdout}");
    assert_eq!(stdout.matches("status: ok").count(), 2, "{stdout}");
}

/// Scenario U: records created by different contributors on different
/// branches survive a normal team Git merge with authorship intact.
#[test]
fn different_contributor_records_survive_team_merge() {
    let (_dir, repo, alice_home) = dirty_rust_repo("final-team-merge");
    let bob_home = TempDir::new().expect("bob home");
    configure_identity(alice_home.path(), "alice");
    configure_identity(bob_home.path(), "bob");

    // Preserve the merge point, then diverge.
    git(&repo, &["branch", "b2"]);

    // Alice records on b1.
    git(&repo, &["checkout", "--quiet", "-b", "b1"]);
    let alice_review = review_working(&repo, alice_home.path());
    let out = run_nisse_in_with_env(
        &repo,
        &[
            "feedback",
            alice_review["review_id"].as_str().expect("review id"),
            alice_review["findings"][0]["id"]
                .as_str()
                .expect("finding id"),
            "--fixed",
        ],
        alice_home.path(),
        &[],
    );
    assert!(out.status.success());
    assert_eq!(observation_files(&repo, "alice").len(), 2);
    git(&repo, &["add", ".nisse"]);
    commit(&repo, "nisse: alice feedback");

    // Bob records the same known outcome on b2 independently.
    git(&repo, &["checkout", "--quiet", "b2"]);
    let bob_review = review_working(&repo, bob_home.path());
    let out = run_nisse_in_with_env(
        &repo,
        &[
            "feedback",
            bob_review["review_id"].as_str().expect("review id"),
            bob_review["findings"][0]["id"]
                .as_str()
                .expect("finding id"),
            "--fixed",
        ],
        bob_home.path(),
        &[],
    );
    assert!(out.status.success());
    assert_eq!(observation_files(&repo, "bob").len(), 2);
    git(&repo, &["add", ".nisse"]);
    commit(&repo, "nisse: bob feedback");

    // A normal team merge keeps both contributors' records.
    git(&repo, &["checkout", "--quiet", "main"]);
    git(&repo, &["merge", "--quiet", "--no-edit", "b1"]);
    git(&repo, &["merge", "--quiet", "--no-edit", "b2"]);
    assert_eq!(observation_files(&repo, "alice").len(), 2);
    assert_eq!(observation_files(&repo, "bob").len(), 2);

    // The merged tree loads strictly with provenance for both authors.
    let out = run_nisse_in_with_env(&repo, &["records", "list"], bob_home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("observations (4)"), "{stdout}");
    assert!(stdout.contains("submitted by alice"), "{stdout}");
    assert!(stdout.contains("submitted by bob"), "{stdout}");
    for path in observation_files(&repo, "alice")
        .into_iter()
        .chain(observation_files(&repo, "bob"))
    {
        let text = std::fs::read_to_string(&path).expect("read record");
        let record: serde_json::Value = serde_json::from_str(&text).expect("record JSON");
        let author = record["author"].as_str().expect("author");
        assert!(
            author == "alice" || author == "bob",
            "author preserved: {author}"
        );
        // Records sit inside their own contributor directory.
        assert!(
            path.to_string_lossy().contains(&format!("/{author}/"))
                || path.to_string_lossy().contains(&format!("\\{author}\\"))
        );
    }
}

/// Scenario AC: the shipped binary exposes exactly the frozen CLI
/// surface — four review/query/feedback operations plus inspection
/// commands — and no Plan/Build task mode or workflow state (permanent
/// invariant 26).
#[test]
fn frozen_cli_surface_has_no_plan_or_build_mode() {
    let cwd = TempDir::new().expect("temp dir");
    let home = TempDir::new().expect("home");

    let out = run_nisse_in_with_env(cwd.path(), &["--help"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(out.stderr).expect("stderr utf8");
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");

    let names = help_command_names(&stdout);
    assert_eq!(
        names,
        vec![
            "cache", "doctor", "eval", "feedback", "mcp", "query", "records", "review"
        ],
        "the top-level surface must stay frozen: {stdout}"
    );
    let lowered = stdout.to_lowercase();
    for forbidden in ["plan mode", "build mode", "task mode"] {
        assert!(
            !lowered.contains(forbidden),
            "`{forbidden}` must not appear in the CLI surface: {stdout}"
        );
    }

    // `review` reviews documents produced elsewhere; it is not a mode.
    let out = run_nisse_in_with_env(cwd.path(), &["review", "--help"], home.path(), &[]);
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    assert!(out.status.success(), "{stdout}");
    assert_eq!(
        help_command_names(&stdout),
        vec!["code", "investigation", "plan"],
        "{stdout}"
    );
}

/// Extract the subcommand names listed under `Commands:` in a clap
/// help text, sorted.
fn help_command_names(help: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            in_commands = false;
            continue;
        }
        if let Some(name) = trimmed.split_whitespace().next()
            && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && !name.starts_with('-')
        {
            names.push(name);
        }
    }
    names.sort_unstable();
    names
}
