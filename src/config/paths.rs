//! Configuration file locations.
//!
//! Nisse configuration is always trusted local configuration. It never
//! lives under the Git-tracked `.nisse/**` tree (project data) — only in
//! the platform-native user config directory or under `$GIT_COMMON_DIR`
//! (repository-local state that is never committed).

use std::path::{Path, PathBuf};

/// Platform-native user configuration file path
/// (`<config dir>/nisse/config.toml`).
///
/// Returns `None` when the platform provides no user config directory
/// (for example, no home directory can be determined).
pub fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("nisse").join("config.toml"))
}

/// Trusted repository-local configuration path under `$GIT_COMMON_DIR`
/// (`$GIT_COMMON_DIR/nisse/config.toml`).
///
/// This file lives outside the worktree and is shared by all worktrees of
/// a repository; it is local machine state and is never committed.
pub fn repo_config_path(git_common_dir: &Path) -> PathBuf {
    git_common_dir.join("nisse").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_config_path_is_nested_under_platform_config_dir() {
        if let Some(path) = user_config_path() {
            assert!(path.ends_with(Path::new("nisse").join("config.toml")));
        }
    }

    #[test]
    fn repo_config_path_lives_under_the_common_dir() {
        let path = repo_config_path(Path::new("/repo/.git"));
        assert_eq!(path, PathBuf::from("/repo/.git/nisse/config.toml"));
    }
}
