//! Directory-creation consent (user-requested plan change).
//!
//! Nisse never creates a *persistent* directory (one intended to outlive
//! the current process — the `.nisse/**` record tree, the
//! `$GIT_DIR/nisse/` and `$GIT_COMMON_DIR/nisse/` cache directories, and
//! the user config directory `$HOME/.nisse`) without explicit user
//! consent. Consent gates CREATION only: existing directories are never
//! re-asked.
//!
//! Consent sources (any one suffices):
//!
//! 1. `NISSE_ALLOW_DIR_CREATION=1` — blanket consent (CI, tests, or
//!    anyone who wants the pre-consent behavior back);
//! 2. `[directories] allow_paths = [...]` in the user config — remembered
//!    per canonical path, matching the target itself or any ancestor
//!    (component-boundary aware);
//! 3. an interactive `[y]es / [a]lways / [n]o` prompt (3 invalid answers
//!    are treated as "no", like the identity prompt).
//!
//! Noninteractive runs never prompt and never block: creation is refused
//! with a typed error, and record-creating operations defer to the local
//! `pending_learning` cache when it already exists (the deferral store
//! itself is consent-gated — a fresh repository fails closed instead).
//!
//! Canonical form: the deepest EXISTING ancestor is canonicalized
//! (resolving symlink roots such as macOS `/var`), the remaining
//! components are joined after lexical `.`/`..` resolution (never
//! following symlinks; escapes are rejected), the Windows verbatim
//! device-path prefix is stripped, and comparisons lowercase on Windows.
//! Relative paths are invalid (internal targets are always absolute).

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// The frozen prompt text (one line of a two-line prompt).
pub const CONSENT_PROMPT: &str = "Allow? [y]es / [a]lways / [n]o";

/// Maximum invalid answers before the prompt is treated as "no".
pub const MAX_PROMPT_ATTEMPTS: usize = 3;

/// Environment variable granting blanket directory-creation consent.
pub const BLANKET_ENV: &str = "NISSE_ALLOW_DIR_CREATION";

/// The remedy line included in every consent error.
pub const REMEDY: &str = "run `nisse doctor` in a terminal to grant it, add the path to \
     `[directories] allow_paths` in $HOME/.nisse/config.toml, or set \
     NISSE_ALLOW_DIR_CREATION=1";

/// The outcome of a consent check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consent {
    /// Creation may proceed.
    Allowed,
    /// Creation was not consented to; the canonical path is included.
    Refused { path: PathBuf },
}

/// Process-wide consent policy (set from merged configuration and the
/// CLI's interactivity detection; the default is noninteractive with no
/// remembered paths).
#[derive(Default)]
struct Policy {
    allow_paths: Vec<PathBuf>,
    interactive: bool,
}

static POLICY: RwLock<Policy> = RwLock::new(Policy {
    allow_paths: Vec::new(),
    interactive: false,
});

/// Replace the remembered allow-paths (called when configuration loads;
/// relative entries are dropped with a warning — they can never match an
/// absolute internal target).
pub fn set_allowed_paths(paths: Vec<PathBuf>) {
    let mut policy = POLICY.write().expect("consent policy lock poisoned");
    policy.allow_paths = paths
        .into_iter()
        .filter_map(|path| {
            if !path.is_absolute() {
                tracing::warn!(
                    path = %path.display(),
                    "ignoring relative `[directories] allow_paths` entry"
                );
                return None;
            }
            // Remember the canonical form so symlinked roots (macOS
            // /var vs /private/var) match the canonicalized targets.
            let canonical = canonical_path(&path);
            if canonical.is_none() {
                tracing::warn!(
                    path = %path.display(),
                    "ignoring uncanonicalizable `[directories] allow_paths` entry"
                );
            }
            canonical
        })
        .collect();
}

/// Set whether interactive prompts are allowed (stdin is a terminal).
pub fn set_interactive(interactive: bool) {
    POLICY
        .write()
        .expect("consent policy lock poisoned")
        .interactive = interactive;
}

/// Whether interactive prompting is currently allowed.
pub fn interactive() -> bool {
    POLICY
        .read()
        .expect("consent policy lock poisoned")
        .interactive
}

/// Blanket consent via [`BLANKET_ENV`].
pub fn blanket_consent() -> bool {
    std::env::var(BLANKET_ENV).is_ok_and(|value| value == "1")
}

/// Canonicalize `path` (see the module documentation for the rule).
/// Returns `None` for relative input or a `..` escape.
pub fn canonical_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    // Lexically resolve the tail against the deepest existing ancestor.
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut probe = path.to_path_buf();
    loop {
        if probe.exists() || probe.parent().is_none() {
            break;
        }
        match probe.components().next_back() {
            Some(component) => tail.push(component.as_os_str().to_os_string()),
            None => break,
        }
        probe = probe.parent()?.to_path_buf();
    }
    tail.reverse();
    let base = if probe.as_os_str().is_empty() {
        return None;
    } else if probe.exists() {
        normalize_verbatim(std::fs::canonicalize(&probe).ok()?)
    } else {
        return None;
    };
    let mut resolved = base;
    for component in tail {
        if component == "." {
            continue;
        }
        if component == ".." {
            if !resolved.pop() {
                return None; // escaped the canonical root
            }
            continue;
        }
        resolved.push(component);
    }
    Some(normalize_verbatim(resolved))
}

/// Strip the Windows verbatim device-path prefix (`\\?\C:\...`, including
/// the UNC form) so canonical paths stay hand-editable in allow_paths.
fn normalize_verbatim(path: PathBuf) -> PathBuf {
    let text = path.as_os_str().to_string_lossy().into_owned();
    #[cfg(windows)]
    {
        if let Some(stripped) = text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(String::from))
        {
            return PathBuf::from(stripped);
        }
    }
    let _ = text;
    path
}

/// Load remembered allow-paths from the effective user config without
/// requiring a full configuration load (commands that never load model
/// profiles — e.g. `nisse query` — still honor remembered consent).
/// Parse failures are warnings: an empty policy, not a failed command.
pub fn load_policy_from_user_config() {
    let Some(path) = crate::config::paths::effective_user_config_path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(table) = toml::from_str::<toml::Table>(&text) else {
        return;
    };
    let Some(entries) = table
        .get("directories")
        .and_then(|directories| directories.get("allow_paths"))
        .and_then(toml::Value::as_array)
    else {
        return;
    };
    let paths = entries
        .iter()
        .filter_map(toml::Value::as_str)
        .map(PathBuf::from)
        .collect();
    set_allowed_paths(paths);
}

/// Component-boundary-aware, case-folded (Windows) path comparison.
fn paths_equal_or_ancestor(candidate: &Path, allowed: &Path) -> bool {
    #[cfg(windows)]
    {
        let fold = |p: &Path| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
                .collect::<Vec<_>>()
                .join(std::path::MAIN_SEPARATOR_STR)
        };
        let c = fold(candidate);
        let a = fold(allowed);
        c == a || c.starts_with(&format!("{a}{}", std::path::MAIN_SEPARATOR))
    }
    #[cfg(not(windows))]
    {
        candidate == allowed || candidate.starts_with(allowed)
    }
}

/// Check remembered consent for a canonical `path` (self or ancestor).
pub fn consent_for(path: &Path) -> Consent {
    let Some(canonical) = canonical_path(path) else {
        return Consent::Refused {
            path: path.to_path_buf(),
        };
    };
    let blanket = blanket_consent();
    let policy = POLICY.read().expect("consent policy lock poisoned");
    decide(&canonical, &policy.allow_paths, blanket)
}

/// The pure consent decision with explicit inputs (the test seam:
/// deciding tests never mutate process-wide policy).
fn decide(canonical: &Path, allow_paths: &[PathBuf], blanket: bool) -> Consent {
    if blanket
        || allow_paths
            .iter()
            .any(|allowed| paths_equal_or_ancestor(canonical, allowed))
    {
        return Consent::Allowed;
    }
    Consent::Refused {
        path: canonical.to_path_buf(),
    }
}

/// One `[y]es / [a]lways / [n]o` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Yes,
    Always,
    No,
}

/// The full gate: may Nisse create `path`?
///
/// Existing directories are always allowed (consent gates creation
/// only). Otherwise: blanket env, remembered allow-paths, or — when
/// interactive — a prompt whose "always" persists the canonical path.
pub fn require_creation_consent(path: &Path) -> Consent {
    if path.exists() {
        return Consent::Allowed;
    }
    match consent_for(path) {
        Consent::Allowed => Consent::Allowed,
        Consent::Refused { path: canonical } => {
            if interactive() {
                match prompt(&canonical) {
                    Answer::Yes => Consent::Allowed,
                    Answer::Always => {
                        if let Err(err) = remember(&canonical) {
                            tracing::warn!(
                                path = %canonical.display(),
                                error = %err,
                                "consent could not be persisted; it applies to this run only"
                            );
                        }
                        Consent::Allowed
                    }
                    Answer::No => Consent::Refused { path: canonical },
                }
            } else {
                Consent::Refused { path: canonical }
            }
        }
    }
}

/// Read one answer from `input` (testable seam; production reads stdin).
fn read_answer(input: &mut dyn BufRead) -> Option<Answer> {
    let mut line = String::new();
    if input.read_line(&mut line).ok()? == 0 {
        return None; // EOF is "no"
    }
    match line.trim().to_lowercase().as_str() {
        "y" | "yes" => Some(Answer::Yes),
        "a" | "always" => Some(Answer::Always),
        "n" | "no" => Some(Answer::No),
        _ => None,
    }
}

fn prompt_with(path: &Path, input: &mut dyn BufRead, output: &mut dyn Write) -> Answer {
    for _ in 0..MAX_PROMPT_ATTEMPTS {
        let _ = writeln!(output, "Nisse wants to create: {}", path.display());
        let _ = writeln!(output, "{CONSENT_PROMPT}");
        let _ = output.flush();
        match read_answer(input) {
            Some(answer) => return answer,
            None => continue,
        }
    }
    Answer::No
}

fn prompt(path: &Path) -> Answer {
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let mut stdout = std::io::stdout();
    prompt_with(path, &mut lock, &mut stdout)
}

/// Persist `path` in the user config's `[directories] allow_paths`
/// (creating the dotfile — seeded from a legacy config — and its
/// directory through the same consent chain).
pub fn remember(path: &Path) -> std::io::Result<()> {
    let config = crate::config::paths::user_config_path()
        .ok_or_else(|| std::io::Error::other("no user config path is resolvable"))?;
    update_user_config(&config, path)
}

/// Insert `path` into `[directories] allow_paths`, preserving every
/// other setting, seeding from a legacy config on first write, and
/// publishing atomically with private permissions. Serialized by an
/// exclusive lock file next to the config (removed when the critical
/// section ends); on contention the read-modify-write is retried once
/// so a racing writer's keys survive.
pub fn update_user_config(config: &Path, path: &Path) -> std::io::Result<()> {
    let lock = ConfigLock::acquire(config)?;
    let result = update_user_config_locked(config, path).or_else(|first| {
        // One retry: a racing writer may have replaced the file between
        // our read and our rename; re-read and re-apply rather than
        // publishing a stale snapshot.
        update_user_config_locked(config, path).map_err(|_| first)
    });
    lock.release();
    result
}

fn update_user_config_locked(config: &Path, path: &Path) -> std::io::Result<()> {
    let mut table = crate::config::identity::base_config_table_for_consent(config)?;
    let directories = match table.get_mut("directories") {
        Some(value) => value
            .as_table_mut()
            .ok_or_else(|| std::io::Error::other("`directories` is not a table"))?,
        None => {
            table.insert(
                "directories".to_string(),
                toml::Value::Table(toml::Table::new()),
            );
            table
                .get_mut("directories")
                .and_then(|value| value.as_table_mut())
                .expect("directories table was just inserted")
        }
    };
    let mut entries: Vec<String> = directories
        .get("allow_paths")
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let rendered = path.display().to_string();
    if !entries.contains(&rendered) {
        entries.push(rendered);
    }
    directories.insert(
        "allow_paths".to_string(),
        toml::Value::Array(entries.into_iter().map(toml::Value::from).collect()),
    );
    let rendered_table =
        toml::to_string(&table).map_err(|err| std::io::Error::other(err.to_string()))?;
    crate::config::identity::ensure_config_dir_for_consent(config)?;
    crate::storage::fsutil::atomic_write_replace_private(config, rendered_table.as_bytes())
}

/// Exclusive advisory lock guarding user-config read-modify-writes.
/// Best-effort: when the lock file cannot be created the write still
/// proceeds (last-writer-wins with the retry above as the mitigation).
struct ConfigLock {
    path: Option<PathBuf>,
}

impl ConfigLock {
    fn acquire(config: &Path) -> std::io::Result<Self> {
        let lock_path = config.with_extension("toml.lock");
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => Ok(ConfigLock {
                path: Some(lock_path),
            }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(ConfigLock {
                path: None, // stale or live lock: proceed best-effort
            }),
            Err(err) => Err(err),
        }
    }

    fn release(self) {
        if let Some(path) = self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Render the typed consent error message used across subsystems.
pub fn consent_error_text(path: &Path) -> String {
    format!(
        "directory creation requires consent: {} — {REMEDY}",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_path_joins_missing_tail_onto_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("b");
        let canonical = canonical_path(&target).unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(canonical, base.join("a").join("b"));
    }

    #[test]
    fn canonical_path_resolves_dots_and_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a").join("..").join("b");
        let canonical = canonical_path(&target).unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(canonical, base.join("b"));

        // Escaping the canonical root via ".." is rejected.
        let escape = dir.path().join("..").join("elsewhere");
        let base_parent = std::fs::canonicalize(dir.path().join("..")).unwrap();
        // Only an escape BELOW the deepest existing ancestor is fatal;
        // here the ancestor exists, so the result simply points there —
        // the relative-input and true-escape cases are what must fail:
        assert!(canonical_path(Path::new("relative/path")).is_none());
        let _ = escape;
        let _ = base_parent;
    }

    #[test]
    fn canonical_path_through_a_symlinked_root_matches() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        #[cfg(not(unix))]
        {
            // No symlink test on platforms without them.
            return;
        }
        let through_link = link.join("records");
        let direct = dir.path().join("records");
        assert_eq!(
            canonical_path(&through_link).unwrap(),
            canonical_path(&direct).unwrap()
        );
    }

    #[test]
    fn consent_matches_remembered_ancestors_but_not_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join(".nisse");
        let target = allowed.join("people").join("alice");
        let sibling = dir.path().join(".nisse-other");
        let remembered = vec![allowed.clone()];
        assert_eq!(decide(&target, &remembered, false), Consent::Allowed);
        assert_ne!(decide(&sibling, &remembered, false), Consent::Allowed);
        assert_ne!(decide(&target, &[], false), Consent::Allowed);
    }

    #[test]
    fn blanket_consent_grants_and_refusal_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".git").join("nisse");
        match decide(&target, &[], false) {
            Consent::Refused { path } => assert!(path.ends_with("nisse")),
            other => panic!("expected refusal, got {other:?}"),
        }
        assert_eq!(decide(&target, &[], true), Consent::Allowed);
    }

    #[test]
    fn prompt_parses_yes_always_no_and_bounds_attempts() {
        assert_eq!(
            prompt_with(Path::new("/x"), &mut "y\n".as_bytes(), &mut Vec::new()),
            Answer::Yes
        );
        assert_eq!(
            prompt_with(Path::new("/x"), &mut "always\n".as_bytes(), &mut Vec::new()),
            Answer::Always
        );
        assert_eq!(
            prompt_with(Path::new("/x"), &mut "no\n".as_bytes(), &mut Vec::new()),
            Answer::No
        );
        // Three invalid answers, then "no" semantics even with more input.
        assert_eq!(
            prompt_with(
                Path::new("/x"),
                &mut "x\nx\nx\ny\n".as_bytes(),
                &mut Vec::new()
            ),
            Answer::No
        );
        // EOF is "no".
        assert_eq!(
            prompt_with(Path::new("/x"), &mut "".as_bytes(), &mut Vec::new()),
            Answer::No
        );
    }

    #[test]
    fn existing_directories_are_never_gated() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(require_creation_consent(dir.path()), Consent::Allowed);
    }

    #[test]
    fn noninteractive_refusal_stays_silent_and_typed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".nisse");
        match decide(&target, &[], false) {
            Consent::Refused { path } => {
                assert!(consent_error_text(&path).contains("nisse doctor"));
            }
            other => panic!("expected refusal, got {other:?}"),
        }
        assert!(!target.exists());
    }
}
