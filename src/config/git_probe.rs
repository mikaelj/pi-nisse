//! Minimal Git probing for configuration discovery and `nisse doctor`.
//!
//! Full repository/worktree semantics are implemented in the Git milestone;
//! this module only answers whether Git is available at all and where the
//! common Git directory of the current directory is. The common directory
//! hosts the repository-local trusted configuration layer.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Result of probing the local Git environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitProbe {
    /// Whether the `git` executable could be executed at all.
    pub available: bool,
    /// Raw `git --version` output when available.
    pub version: Option<String>,
    /// Whether the probed directory is inside a Git repository.
    pub in_repository: bool,
    /// Absolute `$GIT_COMMON_DIR` when inside a repository.
    pub git_common_dir: Option<PathBuf>,
}

/// Probe Git availability and the repository containing `cwd`.
pub fn probe_git(cwd: &Path) -> GitProbe {
    probe_git_inner(cwd, None)
}

fn probe_git_inner(cwd: &Path, path_env: Option<&str>) -> GitProbe {
    let version = match run_git(cwd, &["--version"], path_env) {
        Some(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        _ => {
            return GitProbe {
                available: false,
                version: None,
                in_repository: false,
                git_common_dir: None,
            };
        }
    };

    let git_common_dir = match run_git(cwd, &["rev-parse", "--git-common-dir"], path_env) {
        Some(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() {
                None
            } else {
                let path = PathBuf::from(text);
                Some(if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                })
            }
        }
        _ => None,
    };

    GitProbe {
        available: true,
        version,
        in_repository: git_common_dir.is_some(),
        git_common_dir,
    }
}

fn run_git(cwd: &Path, args: &[&str], path_env: Option<&str>) -> Option<Output> {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    if let Some(path) = path_env {
        command.env("PATH", path);
    }
    command.output().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        let output = Command::new("git")
            .args(["init", "-q", "."])
            .current_dir(dir)
            .output()
            .expect("failed to run git init");
        assert!(
            output.status.success(),
            "git init failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn probe_finds_repository_and_common_dir() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());

        let probe = probe_git(dir.path());
        assert!(probe.available);
        assert!(
            probe
                .version
                .as_deref()
                .unwrap_or_default()
                .starts_with("git version")
        );
        assert!(probe.in_repository);
        let common_dir = probe.git_common_dir.expect("common dir expected");
        assert!(common_dir.is_absolute());
        assert!(common_dir.ends_with(".git"));
    }

    #[test]
    fn probe_reports_not_in_repository() {
        let dir = tempfile::tempdir().unwrap();
        let probe = probe_git(dir.path());
        assert!(probe.available);
        assert!(!probe.in_repository);
        assert!(probe.git_common_dir.is_none());
    }

    #[test]
    #[cfg(unix)]
    fn probe_reports_unavailable_when_git_is_not_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let probe = probe_git_inner(dir.path(), Some(""));
        assert!(!probe.available);
        assert!(!probe.in_repository);
        assert!(probe.git_common_dir.is_none());
    }
}
