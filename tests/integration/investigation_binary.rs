//! End-to-end tests for `nisse review investigation`
//! (PLAN_INVESTIGATION.md), driven through the real binary with the
//! deterministic fake reviewer runtime.

use std::path::PathBuf;

use super::{run_nisse_in_with_env, write_user_config};
use tempfile::TempDir;

/// The task cites the fixture module; the result makes one plausible
/// claim and one overstated claim.
const TASK: &str = "Investigate whether src/greeting.rs greets users correctly.\n\
Examine the module and its tests.\n";
const RESULT: &str = "Findings:\n1. The module greets users with a hello message (verified by \
reading src/greeting.rs).\n2. The module certainly never mishandles unicode, because unicode is \
impossible here.\n";

fn repo_with_greeting(label: &str) -> (TempDir, PathBuf, TempDir) {
    let dir = tempfile::Builder::new().prefix(label).tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("src").join("greeting.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n",
    )
    .unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "--quiet", "-b", "main"])
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=T", "add", "--all"])
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .status()
        .unwrap();
    assert!(status.success());
    let status = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=T",
            "commit",
            "--quiet",
            "-m",
            "one",
        ])
        .current_dir(&repo)
        .env("LC_ALL", "C")
        .status()
        .unwrap();
    assert!(status.success());
    let home = TempDir::new().unwrap();
    write_user_config(home.path(), "[contributor]\nid = \"alice\"\n");
    (dir, repo, home)
}

#[test]
fn investigation_review_end_to_end_renders_verdicts_and_gaps() {
    let (_dir, repo, home) = repo_with_greeting("investigation-e2e");

    let out = run_nisse_in_with_env(
        &repo,
        &[
            "review",
            "investigation",
            "--task",
            TASK,
            "--result",
            RESULT,
            "--json",
        ],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let result: serde_json::Value = serde_json::from_str(&stdout).expect("strict JSON");
    assert_eq!(result["kind"], "investigation");
    assert!(result["claims"].is_array(), "{result}");
    assert!(result["coverage_gaps"].is_array(), "{result}");

    // The fake claim-skeptic emits exactly the two scripted claims: the
    // contradicted one must carry the overstated note.
    let claims = result["claims"].as_array().unwrap();
    assert!(claims.len() >= 2, "claims: {claims:?}");
    let verdicts: Vec<&str> = claims
        .iter()
        .map(|claim| claim["verdict"].as_str().unwrap())
        .collect();
    assert!(verdicts.contains(&"supported"), "{verdicts:?}");
    assert!(verdicts.contains(&"contradicted"), "{verdicts:?}");

    // Text rendering groups by verdict and surfaces coverage gaps.
    let out = run_nisse_in_with_env(
        &repo,
        &[
            "review",
            "investigation",
            "--task",
            TASK,
            "--result",
            RESULT,
        ],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Supported"), "{stdout}");
    assert!(stdout.contains("Contradicted"), "{stdout}");
    assert!(stdout.contains("Coverage gaps"), "{stdout}");
    assert!(stdout.contains("incompleteness"), "{stdout}");
}

#[test]
fn investigation_input_errors_are_typed() {
    let (_dir, repo, home) = repo_with_greeting("investigation-inputs");

    // A missing explicit @file fails with the typed error before any
    // review runs.
    let out = run_nisse_in_with_env(
        &repo,
        &[
            "review",
            "investigation",
            "--task",
            "@missing-task.md",
            "--result",
            RESULT,
        ],
        home.path(),
        &[],
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success());
    assert!(
        stderr.contains("does not name a readable file"),
        "stderr: {stderr}"
    );

    // Blank literal inputs are rejected.
    let out = run_nisse_in_with_env(
        &repo,
        &[
            "review",
            "investigation",
            "--task",
            "   ",
            "--result",
            RESULT,
        ],
        home.path(),
        &[],
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(!out.status.success());
    assert!(stderr.contains("input is blank"), "stderr: {stderr}");
}

#[test]
fn investigation_accepts_auto_detected_file_inputs() {
    let (_dir, repo, home) = repo_with_greeting("investigation-files");
    std::fs::write(repo.join("task.md"), TASK).unwrap();
    std::fs::write(repo.join("result.md"), RESULT).unwrap();

    // Bare values naming existing files are read as files.
    let out = run_nisse_in_with_env(
        &repo,
        &[
            "review",
            "investigation",
            "--task",
            "task.md",
            "--result",
            "result.md",
            "--json",
        ],
        home.path(),
        &[],
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    let result: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result["kind"], "investigation");
}
