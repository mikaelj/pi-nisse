//! Typed Git failures.
//!
//! Every Git invocation failure maps onto exactly one variant so callers can
//! make deterministic decisions without inspecting free-form output.

use std::path::PathBuf;

use thiserror::Error;

/// Errors produced while invoking or parsing output from Git.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum GitError {
    /// The configured Git executable could not be found.
    #[error("git executable not found")]
    GitUnavailable,

    /// A Git process could not be spawned.
    #[error("failed to spawn git: {source}")]
    Spawn {
        #[source]
        source: std::io::Error,
    },

    /// Waiting for a Git process failed.
    #[error("failed to wait for git: {source}")]
    Wait {
        #[source]
        source: std::io::Error,
    },

    /// The command exceeded its timeout and the process was killed.
    #[error("git timed out after {timeout_ms} ms: git {command}")]
    Timeout {
        /// Rendered command line for diagnostics.
        command: String,
        /// Configured timeout in milliseconds.
        timeout_ms: u64,
    },

    /// Git exited with a non-zero status.
    #[error("git exited with {exit}: git {command}: {stderr}", exit = display_exit(*.exit))]
    Execution {
        /// Rendered command line for diagnostics.
        command: String,
        /// Raw exit code; `None` when the process was terminated by a signal.
        exit: Option<i32>,
        /// Captured stderr.
        stderr: String,
    },

    /// Git produced output that is not valid UTF-8.
    #[error("git produced non-UTF-8 output: git {command}")]
    InvalidUtf8 {
        /// Rendered command line for diagnostics.
        command: String,
    },

    /// The requested working directory does not exist.
    #[error("working directory `{path}` does not exist")]
    MissingCwd {
        /// The offending directory.
        path: PathBuf,
    },

    /// The starting directory is not inside a Git repository.
    #[error("`{cwd}` is not inside a Git repository")]
    NotARepository {
        /// The directory Git rejected.
        cwd: PathBuf,
    },

    /// A commit-ish did not resolve to a commit object.
    #[error("revision `{spec}` does not resolve to a commit")]
    RevisionNotFound {
        /// The commit-ish that failed to resolve.
        spec: String,
    },

    /// Two commits share no common ancestor.
    #[error("no merge base exists between `{a}` and `{b}`")]
    NoMergeBase {
        /// First commit-ish.
        a: String,
        /// Second commit-ish.
        b: String,
    },

    /// Git exited successfully but its output could not be parsed.
    #[error("git produced unexpected output for `{command}`: {message}")]
    UnexpectedOutput {
        /// Git subcommand that produced the output.
        command: String,
        /// Description of the parse failure.
        message: String,
    },

    /// The index contains unresolved conflict entries, so neither the
    /// working tree nor the index is a well-defined review target.
    #[error("repository has unresolved conflicts in {} path(s)", paths.len())]
    UnmergedPaths {
        /// Paths with conflict stages in the index.
        paths: Vec<String>,
    },

    /// A `<base>..<head>` range specification was malformed.
    #[error("invalid range specification `{spec}`: {reason}")]
    InvalidRangeSpec {
        /// The offending range specification.
        spec: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A repository-relative path escaped the snapshot or was malformed.
    #[error("invalid snapshot path `{path}`")]
    InvalidPath {
        /// The offending path.
        path: String,
    },

    /// An object ID did not have the shape of a full Git object ID.
    #[error("invalid object id `{oid}`")]
    InvalidOid {
        /// The offending object ID.
        oid: String,
    },

    /// Reading snapshot content from the working tree failed.
    #[error("failed to read `{path}`: {source}")]
    Read {
        /// The repository-relative path that could not be read.
        path: std::path::PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

/// Render an optional exit code for error messages.
fn display_exit(exit: Option<i32>) -> String {
    match exit {
        Some(code) => code.to_string(),
        None => "a signal".to_string(),
    }
}
