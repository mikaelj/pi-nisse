//! Contributor identity: resolution, first-use prompt, and persistence
//! (plan section 44).
//!
//! Contributor identity lives in the platform-native user config
//! ([`super::paths::user_config_path`]), never under the Git-tracked
//! `.nisse/**` tree and never in repository configuration.
//!
//! Resolution policy:
//!
//! - An identity already present in the merged configuration is used
//!   directly.
//! - **Noninteractive operation never waits for identity input.** When
//!   no identity exists, resolution returns
//!   [`IdentityOutcome::Unavailable`]; learning that needs an author
//!   must remain pending locally (the SQLite `pending_learning` cache)
//!   instead of inventing or blocking on an author.
//! - Interactive use may prompt (`What should Nisse call you?`) at most
//!   [`MAX_PROMPT_ATTEMPTS`] times for a valid slug, then saves the
//!   answer locally before using it. If the answer cannot be saved, no
//!   durable identity is assumed.
//!
//! Existing contributor IDs and the Git identity are hints only: they
//! pre-fill the prompt's default but never silently become the identity
//! (see [`suggest_identity`]).

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use thiserror::Error;

use super::{ContributorIdentity, is_valid_contributor_id};
use crate::storage::fsutil::atomic_write_replace;

/// The frozen first-use prompt text.
pub const IDENTITY_PROMPT: &str = "What should Nisse call you?";

/// Maximum number of rejected answers before the prompt gives up.
pub const MAX_PROMPT_ATTEMPTS: usize = 3;

/// Result of identity resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityOutcome {
    /// A durable contributor identity is available.
    Identity(ContributorIdentity),
    /// No identity is available. Callers must keep any learning that
    /// needs an author pending locally; they must never block waiting
    /// for input in noninteractive runs.
    Unavailable,
}

impl IdentityOutcome {
    /// The identity, when available.
    pub fn identity(&self) -> Option<&ContributorIdentity> {
        match self {
            IdentityOutcome::Identity(identity) => Some(identity),
            IdentityOutcome::Unavailable => None,
        }
    }
}

/// Typed identity failures.
#[derive(Debug, Error)]
pub enum IdentityError {
    /// The identity could not be saved to the user config.
    #[error("failed to save contributor identity to `{path}`: {message}")]
    Save { path: PathBuf, message: String },
}

/// Resolve the contributor identity, prompting interactively when
/// allowed.
///
/// `existing` is the identity from the merged configuration;
/// `user_config_path` is where a prompted identity is saved;
/// `interactive` decides whether prompting is allowed (callers derive
/// it from whether stdin is a terminal); `input`/`output` carry the
/// prompt conversation; `hint` pre-fills the prompt default.
pub fn resolve_or_prompt(
    existing: Option<&ContributorIdentity>,
    user_config_path: Option<&Path>,
    interactive: bool,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    hint: Option<&str>,
) -> Result<IdentityOutcome, IdentityError> {
    if let Some(identity) = existing {
        return Ok(IdentityOutcome::Identity(identity.clone()));
    }
    if !interactive {
        // Noninteractive: never wait for input; learning stays pending.
        return Ok(IdentityOutcome::Unavailable);
    }
    let Some(answer) = prompt_identity(input, output, hint) else {
        return Ok(IdentityOutcome::Unavailable);
    };
    let Some(path) = user_config_path else {
        tracing::warn!(
            contributor = %answer,
            "no platform user config directory available; contributor identity not saved"
        );
        return Ok(IdentityOutcome::Unavailable);
    };
    save_identity(path, &answer)?;
    Ok(IdentityOutcome::Identity(ContributorIdentity {
        id: answer,
    }))
}

/// Prompt until a valid contributor slug is entered, the attempt limit
/// is hit, or input ends. An empty answer accepts the hint's default.
fn prompt_identity(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    hint: Option<&str>,
) -> Option<String> {
    let hint = hint.filter(|hint| is_valid_contributor_id(hint));
    for _ in 0..MAX_PROMPT_ATTEMPTS {
        let _ = write!(output, "{IDENTITY_PROMPT}");
        if let Some(hint) = &hint {
            let _ = write!(output, " [{hint}]");
        }
        let _ = write!(output, ": ");
        let _ = output.flush();

        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => {
                // End of input: give up rather than loop.
                let _ = writeln!(output);
                return None;
            }
            Ok(_) => {}
        }
        let answer = line.trim();
        if answer.is_empty() {
            if let Some(hint) = &hint {
                return Some(hint.to_string());
            }
            let _ = writeln!(
                output,
                "contributor ids match `[a-zA-Z0-9][a-zA-Z0-9._-]{{0,63}}`; try again"
            );
            continue;
        }
        if is_valid_contributor_id(answer) {
            return Some(answer.to_string());
        }
        let _ = writeln!(
            output,
            "`{answer}` is not a valid contributor id \
             (`[a-zA-Z0-9][a-zA-Z0-9._-]{{0,63}}`, at most 64 characters); try again"
        );
    }
    None
}

/// Save the contributor ID durably in the user config.
///
/// When the file already exists it is rewritten through a TOML
/// value round-trip so other settings survive; comments and formatting
/// are not preserved. The write is atomic.
pub fn save_identity(path: &Path, id: &str) -> Result<(), IdentityError> {
    let fail = |message: String| IdentityError::Save {
        path: path.to_path_buf(),
        message,
    };

    if !is_valid_contributor_id(id) {
        return Err(fail(format!("`{id}` is not a valid contributor id")));
    }

    let mut table: toml::Table = if path.exists() {
        let text = std::fs::read_to_string(path)
            .map_err(|source| fail(format!("cannot read the existing config: {source}")))?;
        toml::from_str(&text)
            .map_err(|err| fail(format!("existing config is not valid TOML: {err}")))?
    } else {
        toml::Table::new()
    };

    let contributor = match table.get_mut("contributor") {
        Some(value) => value,
        None => {
            table.insert(
                "contributor".to_string(),
                toml::Value::Table(toml::Table::new()),
            );
            table
                .get_mut("contributor")
                .expect("contributor table was just inserted")
        }
    };
    let Some(contributor_table) = contributor.as_table_mut() else {
        return Err(fail(
            "`contributor` in the user config is not a table".to_string(),
        ));
    };
    contributor_table.insert("id".to_string(), toml::Value::from(id));

    let rendered = toml::to_string(&table)
        .map_err(|err| fail(format!("cannot render the updated config: {err}")))?;
    atomic_write_replace(path, rendered.as_bytes())
        .map_err(|source| fail(format!("atomic write failed: {source}")))?;
    Ok(())
}

/// Derive a suggested contributor slug from a display name such as a
/// Git `user.name`.
///
/// Invalid characters collapse into single `-` separators; the result
/// must still satisfy the frozen slug pattern or `None` is returned.
/// A suggestion is only ever a prompt default, never an implicit
/// identity.
pub fn slugify(name: &str) -> Option<String> {
    let mut out = String::new();
    let mut pending_dash = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(character);
        } else {
            pending_dash = true;
        }
    }
    let mut candidate: String = out
        .trim_start_matches(|character: char| !character.is_ascii_alphanumeric())
        .chars()
        .take(64)
        .collect();
    while candidate.ends_with(['.', '_', '-']) {
        candidate.pop();
    }
    if is_valid_contributor_id(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

/// Suggest a contributor ID from local hints (plan section 44).
///
/// A single existing contributor ID in the repository wins; otherwise a
/// Git identity name is slugified. When several contributors exist, none
/// of them wins and only the Git identity hint remains.
pub fn suggest_identity(
    existing_contributors: &[String],
    git_user_name: Option<&str>,
) -> Option<String> {
    let unique: HashSet<&str> = existing_contributors.iter().map(String::as_str).collect();
    if unique.len() == 1 {
        return unique.into_iter().next().map(str::to_string);
    }
    git_user_name.and_then(slugify)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(text: &str) -> std::io::Cursor<&str> {
        std::io::Cursor::new(text)
    }

    #[test]
    fn existing_identity_wins_without_prompting() {
        let mut input = reader("someone\n");
        let mut output = Vec::new();
        let existing = ContributorIdentity {
            id: "configured".to_string(),
        };
        let outcome =
            resolve_or_prompt(Some(&existing), None, true, &mut input, &mut output, None).unwrap();
        assert_eq!(outcome, IdentityOutcome::Identity(existing));
        // Nothing was consumed or printed.
        assert!(output.is_empty(), "output: {output:?}");
        assert!(input.position() == 0);
    }

    #[test]
    fn noninteractive_never_reads_input() {
        let mut input = reader("alice\n");
        let mut output = Vec::new();
        let outcome = resolve_or_prompt(None, None, false, &mut input, &mut output, None).unwrap();
        assert_eq!(outcome, IdentityOutcome::Unavailable);
        assert!(output.is_empty(), "output: {output:?}");
        assert_eq!(input.position(), 0, "no input byte may be consumed");
    }

    #[test]
    fn interactive_prompt_saves_and_returns_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut input = reader("alice\n");
        let mut output = Vec::new();

        let outcome =
            resolve_or_prompt(None, Some(&path), true, &mut input, &mut output, None).unwrap();

        assert_eq!(
            outcome,
            IdentityOutcome::Identity(ContributorIdentity {
                id: "alice".to_string()
            })
        );
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains(IDENTITY_PROMPT), "prompt: {text}");

        // The identity was saved and reloads through the config loader.
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("[contributor]"), "saved: {saved}");
        assert!(saved.contains("id = \"alice\""));
        let loaded = crate::config::load_config(Some(&path), None).unwrap();
        assert_eq!(
            loaded.config.contributor,
            Some(ContributorIdentity {
                id: "alice".to_string()
            })
        );
    }

    #[test]
    fn prompt_retries_invalid_answers_then_gives_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Invalid answer, then a valid one.
        let mut input = reader("../evil\nalice\n");
        let mut output = Vec::new();
        let outcome =
            resolve_or_prompt(None, Some(&path), true, &mut input, &mut output, None).unwrap();
        assert_eq!(
            outcome,
            IdentityOutcome::Identity(ContributorIdentity {
                id: "alice".to_string()
            })
        );
        assert!(
            String::from_utf8(output.clone())
                .unwrap()
                .contains("../evil")
        );

        // Three invalid answers exhaust the attempt budget.
        let dir2 = tempfile::tempdir().unwrap();
        let path2 = dir2.path().join("config.toml");
        let mut input = reader("no!\nnope!\nnada!\nalice\n");
        let mut output = Vec::new();
        let outcome =
            resolve_or_prompt(None, Some(&path2), true, &mut input, &mut output, None).unwrap();
        assert_eq!(outcome, IdentityOutcome::Unavailable);
        assert!(!path2.exists(), "nothing may be saved on failure");
    }

    #[test]
    fn prompt_end_of_input_returns_unavailable() {
        let mut input = reader("");
        let mut output = Vec::new();
        let outcome = resolve_or_prompt(None, None, true, &mut input, &mut output, None).unwrap();
        assert_eq!(outcome, IdentityOutcome::Unavailable);
    }

    #[test]
    fn prompt_hint_is_the_default_and_invalid_hints_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Empty answer accepts the hint.
        let mut input = reader("\n");
        let mut output = Vec::new();
        let outcome = resolve_or_prompt(
            None,
            Some(&path),
            true,
            &mut input,
            &mut output,
            Some("alice"),
        )
        .unwrap();
        assert_eq!(
            outcome,
            IdentityOutcome::Identity(ContributorIdentity {
                id: "alice".to_string()
            })
        );
        assert!(String::from_utf8(output).unwrap().contains("[alice]"));

        // An invalid hint is never offered or accepted.
        let mut input = reader("\nbob\n");
        let mut output = Vec::new();
        let outcome = resolve_or_prompt(
            None,
            Some(&path),
            true,
            &mut input,
            &mut output,
            Some("../evil"),
        )
        .unwrap();
        assert_eq!(
            outcome,
            IdentityOutcome::Identity(ContributorIdentity {
                id: "bob".to_string()
            })
        );
        assert!(!String::from_utf8(output).unwrap().contains("[../evil]"));
    }

    #[test]
    fn unsavable_identity_returns_unavailable() {
        // A prompt succeeded but there is nowhere durable to save it:
        // never use an identity that would not survive this run.
        let mut input = reader("alice\n");
        let mut output = Vec::new();
        let outcome = resolve_or_prompt(None, None, true, &mut input, &mut output, None).unwrap();
        assert_eq!(outcome, IdentityOutcome::Unavailable);
    }

    #[test]
    fn save_identity_merges_into_an_existing_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[network]\nallow_non_private_endpoints = false\n\n[model_profiles.standard]\nmodel = \"m\"\n",
        )
        .unwrap();

        save_identity(&path, "alice").unwrap();
        let loaded = crate::config::load_config(Some(&path), None).unwrap();
        assert_eq!(
            loaded.config.contributor,
            Some(ContributorIdentity {
                id: "alice".to_string()
            })
        );
        // Other settings survive the merge.
        assert!(!loaded.config.network.allow_non_private_endpoints);
        assert_eq!(loaded.config.model_profiles["standard"].model, "m");
    }

    #[test]
    fn save_identity_rejects_invalid_ids_and_broken_configs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        assert!(save_identity(&path, "../evil").is_err());
        assert!(save_identity(&path, "alice").is_ok());

        std::fs::write(&path, "not [ toml").unwrap();
        let err = save_identity(&path, "alice").unwrap_err();
        assert!(err.to_string().contains("not valid TOML"), "{err}");
    }

    #[test]
    fn slugify_derives_valid_ids_or_none() {
        assert_eq!(slugify("Mikael J"), Some("Mikael-J".to_string()));
        assert_eq!(slugify("user (admin)"), Some("user-admin".to_string()));
        assert_eq!(slugify("  spaced  out  "), Some("spaced-out".to_string()));
        assert_eq!(slugify(".leading"), Some("leading".to_string()));
        assert_eq!(slugify("trailing..."), Some("trailing".to_string()));
        assert_eq!(slugify(&"x".repeat(100)), Some("x".repeat(64)));
        assert_eq!(slugify("!!!---???"), None);
        assert_eq!(slugify(""), None);
        // Every produced slug satisfies the frozen pattern.
        for name in ["Mikael J", "ünicode", "bob.", "0xdeadbeat"] {
            if let Some(slug) = slugify(name) {
                assert!(is_valid_contributor_id(&slug), "{name} -> {slug}");
            }
        }
    }

    #[test]
    fn suggest_identity_prefers_unique_existing_contributors() {
        assert_eq!(
            suggest_identity(&[], Some("Mikael J")),
            Some("Mikael-J".to_string())
        );
        assert_eq!(suggest_identity(&[], None), None);
        assert_eq!(suggest_identity(&[], Some("???")), None);

        let alice = "alice".to_string();
        assert_eq!(
            suggest_identity(std::slice::from_ref(&alice), Some("Mikael J")),
            Some("alice".to_string())
        );
        // Several existing contributors: none wins, keep the git hint.
        assert_eq!(
            suggest_identity(&[alice, "bob".to_string()], Some("Mikael J")),
            Some("Mikael-J".to_string())
        );
    }
}
