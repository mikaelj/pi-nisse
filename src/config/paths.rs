//! Configuration file locations.
//!
//! Nisse configuration is always trusted local configuration. It never
//! lives under the Git-tracked `.nisse/**` tree (project data) — only in
//! the user config directory (`$HOME/.nisse`, dotfile convention on
//! every platform, including macOS) or under `$GIT_COMMON_DIR`
//! (repository-local state that is never committed).
//!
//! The user config used to live at the platform-native config path; for
//! one release that legacy location is still read (with a per-run
//! migration notice) when the dotfile config does not exist yet.

use std::path::{Path, PathBuf};

/// The user home directory backing the user config file.
///
/// Resolution (user-requested plan change, PLAN.md section 11):
///
/// - non-Windows: `$HOME` when it is non-empty and absolute, else
///   `dirs::home_dir()` under the same usability filters;
/// - Windows: `dirs::home_dir()` (USERPROFILE) — `$HOME` is nonstandard
///   there (Git Bash/MSYS set it) and could otherwise split the config
///   across two files depending on the shell.
///
/// Empty or relative candidates count as unset. Returns `None` only when
/// no usable home directory is resolvable at all.
fn usable_home() -> Option<PathBuf> {
    fn usable(home: Option<PathBuf>) -> Option<PathBuf> {
        home.filter(|home| !home.as_os_str().is_empty())
            .filter(|home| home.is_absolute())
    }
    if cfg!(windows) {
        usable(dirs::home_dir())
    } else {
        usable(std::env::var_os("HOME").map(PathBuf::from)).or_else(|| usable(dirs::home_dir()))
    }
}

/// User configuration file path: `$HOME/.nisse/config.toml`.
pub fn user_config_path() -> Option<PathBuf> {
    usable_home().map(|home| home.join(".nisse").join("config.toml"))
}

/// The pre-dotfile user configuration path
/// (`<platform config dir>/nisse/config.toml`), read as a fallback for
/// one release after the config moved to `$HOME/.nisse`.
pub fn legacy_user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("nisse").join("config.toml"))
}

/// The effective user configuration path for LOADING.
///
/// Honors the one-release legacy fallback: when the dotfile config is
/// absent but the legacy platform-native config exists, the legacy file
/// is the effective config and a per-run migration notice is printed to
/// stderr (once per process; no marker file — persistent notice state
/// would itself require gated directory creation). When both files
/// exist the dotfile wins. Writes always target [`user_config_path`].
pub fn effective_user_config_path() -> Option<PathBuf> {
    static NOTICE: std::sync::Once = std::sync::Once::new();
    let dotfile = user_config_path();
    if dotfile.as_ref().is_some_and(|path| path.exists()) {
        return dotfile;
    }
    let Some(legacy) = legacy_user_config_path() else {
        return dotfile;
    };
    if !legacy.exists() {
        return dotfile;
    }
    NOTICE.call_once(|| {
        let dotfile = dotfile
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "$HOME/.nisse/config.toml".to_string());
        eprintln!(
            "nisse: user config found at {}; Nisse now reads {dotfile} — move the file to migrate",
            legacy.display()
        );
    });
    Some(legacy)
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

    /// Env-mutating tests serialize on one lock: `HOME` is process-wide.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn user_config_path_is_the_home_dotfile() {
        let guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", "/home/nisse-user") };
        let path = user_config_path();
        if let Some(previous) = previous {
            unsafe { std::env::set_var("HOME", previous) };
        } else if let Some(home) = dirs::home_dir() {
            unsafe { std::env::set_var("HOME", home) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        drop(guard);

        assert_eq!(
            path,
            Some(PathBuf::from("/home/nisse-user/.nisse/config.toml"))
        );
    }

    #[test]
    fn empty_or_relative_home_falls_back_or_vanishes() {
        for home in ["", "relative/path"] {
            let guard = ENV_LOCK.lock().unwrap();
            let previous = std::env::var_os("HOME");
            unsafe { std::env::set_var("HOME", home) };
            let path = user_config_path();
            if let Some(previous) = previous {
                unsafe { std::env::set_var("HOME", previous) };
            } else if let Some(home) = dirs::home_dir() {
                unsafe { std::env::set_var("HOME", home) };
            } else {
                unsafe { std::env::remove_var("HOME") };
            }
            drop(guard);

            // Never a relative `.nisse/config.toml` resolved against the
            // cwd: either an absolute fallback home applies or none does.
            let absolute = match &path {
                Some(path) => path.is_absolute(),
                None => true,
            };
            assert!(absolute, "HOME={home:?} produced {path:?}");
        }
    }

    #[test]
    fn legacy_user_config_path_keeps_the_platform_shape() {
        if let Some(path) = legacy_user_config_path() {
            assert!(path.ends_with(Path::new("nisse").join("config.toml")));
        }
    }

    #[test]
    fn repo_config_path_lives_under_the_common_dir() {
        let path = repo_config_path(Path::new("/repo/.git"));
        assert_eq!(path, PathBuf::from("/repo/.git/nisse/config.toml"));
    }
}
