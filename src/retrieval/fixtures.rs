//! Frozen retrieval evaluation fixtures (plan section 69, milestone 23).
//!
//! Each fixture names one query, the target snapshot it runs against
//! (a worktree of the frozen fixture repository), the sources that
//! must be retrieved (`required_sources`), and the sources that must
//! never appear (`forbidden_sources`). The fixture repository is
//! built deterministically in a temporary directory with the installed
//! Git: one `main` branch and one `feature` branch whose linked
//! worktree differs on the same path — exactly the shape that makes
//! snapshot leakage observable. Cache presence never proves snapshot
//! membership (plan section 26), so a fixture whose query names
//! content of another snapshot must never retrieve it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use tempfile::TempDir;
use thiserror::Error;

use super::query::MAX_QUERY_BYTES;
use crate::git::{GitRunner, RepoContext, SnapshotReader, SnapshotSpec, resolve_snapshot};

/// Label of the primary worktree (checked out on `main`).
pub const MAIN_WORKTREE: &str = "main";
/// Label of the linked worktree (checked out on `feature`).
pub const FEATURE_WORKTREE: &str = "feature";

/// Failures of the frozen retrieval fixture set or its repository.
#[derive(Debug, Error)]
pub enum RetrievalFixtureError {
    /// The fixture repository could not be built with the installed
    /// Git.
    #[error("building the retrieval fixture repository failed: {0}")]
    Repository(String),
    /// The frozen fixture set itself is invalid; a defect, not an
    /// evaluation result.
    #[error("invalid retrieval fixture set: {0}")]
    Invalid(String),
}

/// One frozen retrieval evaluation fixture (plan section 69).
#[derive(Debug, Clone)]
pub struct RetrievalFixture {
    /// Stable fixture name.
    pub name: &'static str,
    /// What the fixture exercises.
    pub description: &'static str,
    /// Target snapshot: the worktree of the fixture repository the
    /// query runs against.
    pub worktree: &'static str,
    /// The free-text query under evaluation.
    pub query: &'static str,
    /// Paths that must be retrieved within the top-K entries.
    pub required_sources: Vec<&'static str>,
    /// Paths that must never appear in the result bundle.
    pub forbidden_sources: Vec<&'static str>,
}

/// All frozen retrieval fixtures, in frozen order.
pub fn retrieval_fixtures() -> Vec<RetrievalFixture> {
    vec![
        queue_ring_buffer(),
        wire_framing(),
        oauth_session(),
        render_hot_path(),
        snapshot_isolation(),
    ]
}

/// Validate the structural shape of the frozen fixture set. Runs
/// before every evaluation; violations are defects, not results.
pub fn validate_fixture_set(fixtures: &[RetrievalFixture]) -> Result<(), String> {
    let mut names: Vec<&str> = Vec::new();
    for fixture in fixtures {
        if fixture.name.is_empty() {
            return Err("a fixture has an empty name".to_string());
        }
        if names.contains(&fixture.name) {
            return Err(format!("fixture name `{}` is used twice", fixture.name));
        }
        names.push(fixture.name);
        if fixture.description.is_empty() {
            return Err(format!(
                "fixture `{}` has an empty description",
                fixture.name
            ));
        }
        if fixture.worktree != MAIN_WORKTREE && fixture.worktree != FEATURE_WORKTREE {
            return Err(format!(
                "fixture `{}` names unknown worktree `{}`",
                fixture.name, fixture.worktree
            ));
        }
        if fixture.query.is_empty() {
            return Err(format!("fixture `{}` has an empty query", fixture.name));
        }
        if fixture.query.len() > MAX_QUERY_BYTES {
            return Err(format!(
                "fixture `{}` exceeds the {MAX_QUERY_BYTES}-byte query bound",
                fixture.name
            ));
        }
        if fixture.required_sources.is_empty() {
            return Err(format!(
                "fixture `{}` has no required sources; recall is undefined",
                fixture.name
            ));
        }
        let check_paths = |sources: &[&'static str], kind: &str| -> Result<(), String> {
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            for path in sources {
                if !valid_relative_path(path) {
                    return Err(format!(
                        "fixture `{}` has a malformed {kind} path `{path}`",
                        fixture.name
                    ));
                }
                if !seen.insert(path) {
                    return Err(format!(
                        "fixture `{}` names {kind} source `{path}` twice",
                        fixture.name
                    ));
                }
            }
            Ok(())
        };
        check_paths(&fixture.required_sources, "required")?;
        check_paths(&fixture.forbidden_sources, "forbidden")?;
        for required in &fixture.required_sources {
            if fixture.forbidden_sources.contains(required) {
                return Err(format!(
                    "fixture `{}` requires and forbids `{required}`",
                    fixture.name
                ));
            }
        }
    }
    Ok(())
}

fn valid_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "..")
}

fn fixture(
    name: &'static str,
    description: &'static str,
    worktree: &'static str,
    query: &'static str,
    required: &[&'static str],
    forbidden: &[&'static str],
) -> RetrievalFixture {
    RetrievalFixture {
        name,
        description,
        worktree,
        query,
        required_sources: required.to_vec(),
        forbidden_sources: forbidden.to_vec(),
    }
}

fn queue_ring_buffer() -> RetrievalFixture {
    fixture(
        "queue-ring-buffer",
        "Symbol and text signals retrieve the shared ring-buffer queue",
        MAIN_WORKTREE,
        "queue enqueue drain ring buffer",
        &["src/queue.rs"],
        &["src/auth.rs", "src/protocol.rs"],
    )
}

fn wire_framing() -> RetrievalFixture {
    fixture(
        "wire-framing",
        "Frame encoding and checksum retrieval finds the wire protocol",
        MAIN_WORKTREE,
        "frame encode decode crc32",
        &["src/protocol.rs"],
        &["src/queue.rs"],
    )
}

fn oauth_session() -> RetrievalFixture {
    fixture(
        "oauth-session",
        "Authentication vocabulary retrieves the session module",
        MAIN_WORKTREE,
        "session token validate oauth",
        &["src/auth.rs"],
        &["src/render.rs", "src/queue.rs"],
    )
}

fn render_hot_path() -> RetrievalFixture {
    fixture(
        "render-hot-path",
        "Hot-path rendering vocabulary retrieves the tile renderer",
        MAIN_WORKTREE,
        "tile render allocation",
        &["src/render.rs"],
        &["src/auth.rs"],
    )
}

fn snapshot_isolation() -> RetrievalFixture {
    fixture(
        "snapshot-isolation",
        "Content of another snapshot never leaks: the query names main-only and feature-only symbols",
        FEATURE_WORKTREE,
        "feature_tool beta_fn legacy_hook",
        &["src/mod.rs", "src/extra/feature_tool.rs"],
        &["src/legacy/old_module.rs"],
    )
}

/// Files of the `main` branch, written before the first commit.
fn main_files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "src/queue.rs",
            "//! Bounded ring buffer queue shared between producers and consumers.\n\n\
             pub struct RingQueue {\n    \
                 slots: Vec<u8>,\n    \
                 head: usize,\n    \
                 tail: usize,\n\
             }\n\n\
             impl RingQueue {\n    \
                 pub fn new(capacity: usize) -> Self {\n        \
                     RingQueue {\n            \
                         slots: Vec::with_capacity(capacity),\n            \
                         head: 0,\n            \
                         tail: 0,\n        \
                     }\n    \
                 }\n\n    \
                 pub fn enqueue(&mut self, item: u8) {\n        \
                     self.slots.push(item);\n        \
                     self.tail = self.tail.wrapping_add(1);\n    \
                 }\n\n    \
                 pub fn drain(&mut self, limit: usize) -> Vec<u8> {\n        \
                     let taken: Vec<u8> = self.slots.drain(..limit.min(self.slots.len())).collect();\n        \
                     self.head = self.head.wrapping_add(taken.len());\n        \
                     taken\n    \
                 }\n\
             }\n\n\
             pub fn queue_capacity(queue: &RingQueue) -> usize {\n    \
                 queue.slots.capacity()\n\
             }\n",
        ),
        (
            "src/protocol.rs",
            "//! Wire framing of sensor packets: encode, decode, and checksum.\n\n\
             pub struct Frame {\n    \
                 pub len: u16,\n    \
                 pub payload: Vec<u8>,\n\
             }\n\n\
             pub fn encode_frame(frame: &Frame) -> Vec<u8> {\n    \
                 let mut bytes = Vec::new();\n    \
                 bytes.extend_from_slice(&frame.len.to_be_bytes());\n    \
                 bytes.extend_from_slice(&frame.payload);\n    \
                 bytes\n\
             }\n\n\
             pub fn decode_frame(bytes: &[u8]) -> Option<Frame> {\n    \
                 if bytes.len() < 2 {\n        \
                     return None;\n    \
                 }\n    \
                 let len = u16::from_be_bytes([bytes[0], bytes[1]]);\n    \
                 Some(Frame {\n        \
                     len,\n        \
                     payload: bytes[2..].to_vec(),\n    \
                 })\n\
             }\n\n\
             fn crc32(bytes: &[u8]) -> u32 {\n    \
                 bytes.iter().map(|byte| u32::from(*byte)).sum()\n\
             }\n",
        ),
        (
            "src/auth.rs",
            "//! Login sessions and OAuth token validation.\n\n\
             pub struct Session {\n    \
                 user: String,\n    \
                 token_hash: String,\n\
             }\n\n\
             pub fn validate_token(token: &str) -> bool {\n    \
                 !token.is_empty() && token.len() >= 32\n\
             }\n\n\
             pub fn store_session(session: &Session) {\n    \
                 let _ = &session.token_hash;\n\
             }\n\n\
             pub fn oauth_client_id() -> &'static str {\n    \
                 \"nisse-fixture-client\"\n\
             }\n",
        ),
        (
            "src/render.rs",
            "//! Tile rendering on the hot path.\n\n\
             pub struct Tile {\n    \
                 pub width: u32,\n    \
                 pub height: u32,\n    \
                 pub pixels: Vec<u8>,\n\
             }\n\n\
             pub fn allocation_budget(tile: &Tile) -> usize {\n    \
                 (tile.width as usize) * (tile.height as usize)\n\
             }\n\n\
             pub fn render_tile(tile: &mut Tile, shade: u8) {\n    \
                 for pixel in tile.pixels.iter_mut() {\n        \
                     *pixel = shade;\n    \
                 }\n\
             }\n",
        ),
        ("src/mod.rs", "pub fn alpha_fn() -> u8 {\n    1\n}\n"),
        (
            "src/legacy/old_module.rs",
            "//! Retired legacy helpers kept until the migration finishes.\n\n\
             pub fn legacy_hook() -> u8 {\n    \
                 7\n\
             }\n\n\
             pub fn retired_gate() -> bool {\n    \
                 false\n\
             }\n",
        ),
    ]
}

/// Files the `feature` branch adds or replaces.
fn feature_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("src/mod.rs", "pub fn beta_fn() -> u8 {\n    2\n}\n"),
        (
            "src/extra/feature_tool.rs",
            "//! Extra tooling introduced on the feature branch.\n\n\
             pub fn feature_tool(value: u8) -> u8 {\n    \
                 value.wrapping_add(1)\n\
             }\n",
        ),
    ]
}

/// Files the `feature` branch deletes.
const FEATURE_DELETED: &[&str] = &["src/legacy/old_module.rs"];

/// The frozen fixture repository: a temporary real repository with the
/// `main` and `feature` branches and a linked worktree on `feature`.
pub struct FixtureRepository {
    /// Keeps the temporary directory alive.
    _dir: TempDir,
    /// Main worktree root (checked out on `main`).
    main_root: PathBuf,
    /// Linked worktree root (checked out on `feature`).
    feature_root: PathBuf,
    /// Union of the paths present on any branch.
    all_paths: BTreeSet<String>,
}

impl FixtureRepository {
    /// Build the fixture repository with the installed Git.
    pub fn build() -> Result<Self, RetrievalFixtureError> {
        let dir = TempDir::new()
            .map_err(|err| RetrievalFixtureError::Repository(format!("temp dir: {err}")))?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)
            .map_err(|err| RetrievalFixtureError::Repository(format!("create repo dir: {err}")))?;

        let runner = GitRunner::new();
        let git = |args: &[&str]| -> Result<(), RetrievalFixtureError> {
            runner
                .run(&root, args)
                .map(|_| ())
                .map_err(|err| RetrievalFixtureError::Repository(err.to_string()))
        };

        git(&["init", "--quiet", "."])?;
        // Deterministic identity, no signing, no machine-global hooks.
        git(&["config", "user.email", "nisse@test.invalid"])?;
        git(&["config", "user.name", "Nisse Eval"])?;
        git(&["config", "commit.gpgsign", "false"])?;
        git(&["config", "tag.gpgsign", "false"])?;
        git(&["config", "core.hooksPath", ".git/hooks"])?;
        // Deterministic branch name on the unborn HEAD.
        git(&["symbolic-ref", "HEAD", "refs/heads/main"])?;

        let mut all_paths: BTreeSet<String> = BTreeSet::new();
        for (path, contents) in main_files() {
            write_file(&root, path, contents)?;
            all_paths.insert(path.to_string());
        }
        git(&["add", "-A"])?;
        git(&["commit", "--quiet", "-m", "main fixture content"])?;

        git(&["checkout", "--quiet", "-b", "feature"])?;
        for (path, contents) in feature_files() {
            write_file(&root, path, contents)?;
            all_paths.insert(path.to_string());
        }
        for path in FEATURE_DELETED {
            std::fs::remove_file(root.join(path)).map_err(|err| {
                RetrievalFixtureError::Repository(format!("delete `{path}`: {err}"))
            })?;
        }
        git(&["add", "-A"])?;
        git(&["commit", "--quiet", "-m", "feature fixture content"])?;
        git(&["checkout", "--quiet", "main"])?;

        let feature_root = dir.path().join("wt-feature");
        git(&[
            "worktree",
            "add",
            "--quiet",
            feature_root.to_str().ok_or_else(|| {
                RetrievalFixtureError::Repository("feature worktree path".to_string())
            })?,
            "feature",
        ])?;
        let main_root = root;
        let feature_root = feature_root.canonicalize().map_err(|err| {
            RetrievalFixtureError::Repository(format!("canonicalize feature worktree: {err}"))
        })?;

        Ok(FixtureRepository {
            _dir: dir,
            main_root,
            feature_root,
            all_paths,
        })
    }

    /// The worktree root a fixture's snapshot resolves against.
    pub fn root_for(&self, worktree: &str) -> Result<&PathBuf, RetrievalFixtureError> {
        match worktree {
            MAIN_WORKTREE => Ok(&self.main_root),
            FEATURE_WORKTREE => Ok(&self.feature_root),
            other => Err(RetrievalFixtureError::Invalid(format!(
                "unknown worktree `{other}`"
            ))),
        }
    }

    /// Union of the paths present on any branch of the fixture
    /// repository.
    pub fn all_paths(&self) -> &BTreeSet<String> {
        &self.all_paths
    }

    /// Validate the frozen fixture set against the built repository:
    /// every required source must exist in its target snapshot and
    /// every forbidden source must exist somewhere in the repository
    /// (so the expectation is meaningful).
    pub fn validate_against_fixtures(
        &self,
        fixtures: &[RetrievalFixture],
    ) -> Result<(), RetrievalFixtureError> {
        let runner = GitRunner::new();
        for fixture in fixtures {
            let root = self.root_for(fixture.worktree)?;
            let ctx = RepoContext::discover(&runner, root)
                .map_err(|err| RetrievalFixtureError::Repository(err.to_string()))?;
            let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working)
                .map_err(|err| RetrievalFixtureError::Repository(err.to_string()))?;
            let reader = SnapshotReader::new(&ctx, &resolved)
                .map_err(|err| RetrievalFixtureError::Repository(err.to_string()))?;
            let files: BTreeSet<String> = reader
                .list_files()
                .map_err(|err| RetrievalFixtureError::Repository(err.to_string()))?
                .into_iter()
                .collect();
            for required in &fixture.required_sources {
                if !files.contains(*required) {
                    return Err(RetrievalFixtureError::Invalid(format!(
                        "fixture `{}` requires `{required}`, absent from its target snapshot",
                        fixture.name
                    )));
                }
            }
            for forbidden in &fixture.forbidden_sources {
                if !self.all_paths.contains(*forbidden) {
                    return Err(RetrievalFixtureError::Invalid(format!(
                        "fixture `{}` forbids `{forbidden}`, absent from the fixture repository",
                        fixture.name
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Write one fixture file, creating parent directories.
fn write_file(
    root: &std::path::Path,
    path: &str,
    contents: &str,
) -> Result<(), RetrievalFixtureError> {
    let target = root.join(path);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            RetrievalFixtureError::Repository(format!("create `{path}` parent: {err}"))
        })?;
    }
    std::fs::write(&target, contents)
        .map_err(|err| RetrievalFixtureError::Repository(format!("write `{path}`: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_are_frozen_and_unique() {
        let fixtures = retrieval_fixtures();
        let names: Vec<&str> = fixtures.iter().map(|f| f.name).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        assert_eq!(
            names,
            vec![
                "queue-ring-buffer",
                "wire-framing",
                "oauth-session",
                "render-hot-path",
                "snapshot-isolation",
            ]
        );
        validate_fixture_set(&fixtures).unwrap();
        assert!(
            fixtures.iter().any(|f| f.worktree == FEATURE_WORKTREE),
            "one fixture must target the feature snapshot"
        );
    }

    #[test]
    fn validation_rejects_malformed_fixture_sets() {
        let good = retrieval_fixtures();
        validate_fixture_set(&good).unwrap();

        // Duplicate names.
        let mut duplicate = good.clone();
        duplicate.push(good[0].clone());
        assert!(
            validate_fixture_set(&duplicate)
                .unwrap_err()
                .contains("used twice")
        );

        // Empty required set.
        let mut no_required = good;
        no_required[0].required_sources.clear();
        assert!(
            validate_fixture_set(&no_required)
                .unwrap_err()
                .contains("no required sources")
        );

        // Required and forbidden overlap.
        let mut overlap = retrieval_fixtures();
        let required = overlap[0].required_sources[0];
        overlap[0].forbidden_sources.push(required);
        assert!(
            validate_fixture_set(&overlap)
                .unwrap_err()
                .contains("requires and forbids")
        );

        // Empty query.
        let mut empty_query = retrieval_fixtures();
        empty_query[0].query = "";
        assert!(
            validate_fixture_set(&empty_query)
                .unwrap_err()
                .contains("empty query")
        );

        // Unknown worktree.
        let mut bad_worktree = retrieval_fixtures();
        bad_worktree[0].worktree = "other";
        assert!(
            validate_fixture_set(&bad_worktree)
                .unwrap_err()
                .contains("unknown worktree")
        );

        // Malformed paths.
        for bad in ["", "/abs.rs", "a/../b.rs", "a\\b.rs", "a//b.rs"] {
            let mut bad_path = retrieval_fixtures();
            bad_path[0].required_sources = vec![bad];
            assert!(
                validate_fixture_set(&bad_path).is_err(),
                "path `{bad}` must be rejected"
            );
        }
    }

    #[test]
    fn fixture_repository_builds_branches_and_worktrees() {
        let repo = FixtureRepository::build().unwrap();

        // The frozen fixture set is valid against the real repository.
        let fixtures = retrieval_fixtures();
        validate_fixture_set(&fixtures).unwrap();
        repo.validate_against_fixtures(&fixtures).unwrap();

        // The main worktree holds main content: the legacy module
        // exists and src/mod.rs defines alpha_fn.
        let main = std::fs::read_to_string(repo.main_root.join("src/mod.rs")).unwrap();
        assert!(main.contains("alpha_fn"));
        assert!(repo.main_root.join("src/legacy/old_module.rs").is_file());

        // The linked worktree holds feature content: beta_fn, the new
        // module, and no legacy module.
        let feature = std::fs::read_to_string(repo.feature_root.join("src/mod.rs")).unwrap();
        assert!(feature.contains("beta_fn"));
        assert!(!feature.contains("alpha_fn"));
        assert!(
            repo.feature_root
                .join("src/extra/feature_tool.rs")
                .is_file()
        );
        assert!(!repo.feature_root.join("src/legacy/old_module.rs").exists());

        // The path union covers forbidden expectations from both
        // branches.
        for path in ["src/legacy/old_module.rs", "src/extra/feature_tool.rs"] {
            assert!(repo.all_paths().contains(path), "{path} missing");
        }

        // Both worktrees are discoverable repositories.
        let runner = GitRunner::new();
        for root in [&repo.main_root, &repo.feature_root] {
            let ctx = RepoContext::discover(&runner, root).unwrap();
            assert!(ctx.head.is_some());
        }
    }
}
