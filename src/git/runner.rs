//! Git command runner.
//!
//! All Git access goes through [`GitRunner`], which enforces:
//!
//! - an explicit working directory;
//! - a per-command timeout with process kill on expiry;
//! - captured stdout and stderr;
//! - typed failures (see [`GitError`]);
//! - a deterministic environment: no ambient `GIT_*` variables are inherited
//!   and the locale is forced to `C` so Git diagnostics and output formats
//!   never depend on the user's locale.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::error::GitError;

/// Default per-command timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Interval between exit-status polls while waiting for Git.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A completed Git command.
#[derive(Debug, Clone)]
pub struct GitOutput {
    /// Arguments passed after `git`.
    pub args: Vec<String>,
    /// Whether Git exited with status 0.
    pub success: bool,
    /// Raw exit code; `None` when the process was terminated by a signal.
    pub code: Option<i32>,
    /// Captured stdout as validated UTF-8.
    pub stdout: String,
    /// Captured stderr as validated UTF-8.
    pub stderr: String,
}

impl GitOutput {
    /// Stdout with leading/trailing whitespace removed.
    pub fn stdout_trimmed(&self) -> &str {
        self.stdout.trim()
    }
}

/// Runs Git commands with a timeout, capturing stdout and stderr.
#[derive(Debug, Clone)]
pub struct GitRunner {
    program: PathBuf,
    timeout: Duration,
}

impl GitRunner {
    /// Create a runner invoking `git` with the default timeout.
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("git"),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Use a different Git executable (consumes and returns `self`).
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// Override the per-command timeout (consumes and returns `self`).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The configured per-command timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Run Git and fail unless it exits with status 0.
    pub fn run(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
        let output = self.run_unchecked(cwd, args)?;
        if !output.success {
            return Err(GitError::Execution {
                command: render_command(&self.program, args),
                exit: output.code,
                stderr: output.stderr,
            });
        }
        Ok(output)
    }

    /// Run Git and return the completed output regardless of exit status.
    ///
    /// Spawn, timeout, and UTF-8 failures are still typed errors; only the
    /// process exit status is left for the caller to interpret.
    pub fn run_unchecked(&self, cwd: &Path, args: &[&str]) -> Result<GitOutput, GitError> {
        if !cwd.is_dir() {
            return Err(GitError::MissingCwd {
                path: cwd.to_owned(),
            });
        }

        let mut command = self.build_command(cwd, args);
        let mut child = command.spawn().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => GitError::GitUnavailable,
            _ => GitError::Spawn { source: err },
        })?;

        // Drain both pipes on dedicated threads so a chatty child can never
        // block on a full pipe while we wait for it to exit.
        let stdout_pipe = child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>);
        let stderr_pipe = child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>);
        let stdout_handle = std::thread::spawn(read_pipe_to_end(stdout_pipe));
        let stderr_handle = std::thread::spawn(read_pipe_to_end(stderr_pipe));

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        let _ = stdout_handle.join();
                        let _ = stderr_handle.join();
                        return Err(GitError::Timeout {
                            command: render_command(&self.program, args),
                            timeout_ms: self.timeout.as_millis() as u64,
                        });
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_handle.join();
                    let _ = stderr_handle.join();
                    return Err(GitError::Wait { source: err });
                }
            }
        };

        let stdout_bytes = stdout_handle.join().unwrap_or_default();
        let stderr_bytes = stderr_handle.join().unwrap_or_default();

        let stdout = decode_output(&stdout_bytes)
            .ok_or_else(|| GitError::InvalidUtf8 {
                command: render_command(&self.program, args),
            })?
            .to_owned();
        let stderr = decode_output(&stderr_bytes)
            .ok_or_else(|| GitError::InvalidUtf8 {
                command: render_command(&self.program, args),
            })?
            .to_owned();

        Ok(GitOutput {
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            success: status.success(),
            code: status.code(),
            stdout,
            stderr,
        })
    }

    /// Build the child process for one Git invocation.
    fn build_command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Force a deterministic locale so diagnostics and output formats
            // are stable regardless of the user's environment.
            .env("LC_ALL", "C");
        // Ambient `GIT_*` variables (for example a stray `GIT_DIR`) must not
        // hijack repository discovery or object-database resolution. Every
        // such variable is removed from the child environment.
        for (key, _) in std::env::vars_os() {
            if key.as_encoded_bytes().starts_with(b"GIT_") {
                command.env_remove(&key);
            }
        }
        command
    }
}

impl Default for GitRunner {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a piped child stream to end, returning raw bytes.
fn read_pipe_to_end(
    mut pipe: Option<Box<dyn Read + Send>>,
) -> impl FnOnce() -> Vec<u8> + Send + 'static {
    move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = pipe.as_mut() {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    }
}

/// Decode Git output as UTF-8, returning `None` on invalid sequences.
fn decode_output(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

/// Render a command line for diagnostics.
fn render_command(program: &Path, args: &[&str]) -> String {
    let mut rendered = program.display().to_string();
    for arg in args {
        rendered.push(' ');
        rendered.push_str(arg);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_reports_version_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GitRunner::new();
        let output = runner.run(dir.path(), &["--version"]).unwrap();
        assert!(output.success);
        assert_eq!(output.code, Some(0));
        assert!(output.stdout_trimmed().starts_with("git version"));
    }

    #[test]
    fn run_unchecked_exposes_nonzero_status_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GitRunner::new();
        let output = runner
            .run_unchecked(dir.path(), &["rev-parse", "--verify", "--quiet", "HEAD"])
            .unwrap();
        assert!(!output.success);
        assert!(output.code.is_some());
    }

    #[test]
    fn missing_cwd_is_typed() {
        let runner = GitRunner::new();
        let err = runner
            .run(Path::new("/nisse-definitely-missing-cwd"), &["--version"])
            .unwrap_err();
        assert!(matches!(err, GitError::MissingCwd { .. }));
    }

    #[test]
    fn unavailable_program_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GitRunner::new().with_program("/nisse-definitely-missing-git");
        let err = runner.run(dir.path(), &["--version"]).unwrap_err();
        assert!(matches!(err, GitError::GitUnavailable));
    }

    #[test]
    #[cfg(unix)]
    fn slow_program_times_out_and_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GitRunner::new()
            .with_program("/bin/sh")
            .with_timeout(Duration::from_millis(150));
        let err = runner.run(dir.path(), &["-c", "sleep 5"]).unwrap_err();
        assert!(matches!(err, GitError::Timeout { .. }));
    }

    #[test]
    #[cfg(unix)]
    fn non_zero_exit_is_execution_error_with_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GitRunner::new().with_program("/bin/sh");
        let err = runner
            .run(dir.path(), &["-c", "echo boom >&2; exit 7"])
            .unwrap_err();
        match err {
            GitError::Execution { exit, stderr, .. } => {
                assert_eq!(exit, Some(7));
                assert!(stderr.contains("boom"));
            }
            other => panic!("expected Execution, got {other:?}"),
        }
    }

    #[test]
    #[cfg(unix)]
    fn invalid_utf8_stdout_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GitRunner::new().with_program("/bin/sh");
        // `printf '\377\376'` asks the shell to emit raw bytes 0xff 0xfe,
        // which are not valid UTF-8.
        let err = runner
            .run(dir.path(), &["-c", "printf '\\377\\376'"])
            .unwrap_err();
        assert!(matches!(err, GitError::InvalidUtf8 { .. }));
    }

    // Note: stripping of ambient `GIT_*` environment variables is verified
    // end-to-end by `tests/integration/git_env.rs`, which re-executes the
    // test binary with a controlled environment.

    #[test]
    fn default_runner_uses_git_with_default_timeout() {
        let runner = GitRunner::default();
        assert_eq!(runner.program, PathBuf::from("git"));
        assert_eq!(runner.timeout(), DEFAULT_TIMEOUT);
    }

    // Milestone 28: revision strings containing shell metacharacters
    // reach Git as single literal arguments. No shell is ever involved,
    // so nothing can be executed through them.
    #[test]
    fn metacharacters_in_arguments_are_never_executed() {
        let dir = tempfile::tempdir().unwrap();
        let runner = GitRunner::new();
        for hostile in [
            "HEAD; touch pwned.txt",
            "HEAD && touch pwned.txt",
            "$(touch pwned.txt)",
            "`touch pwned.txt`",
            "HEAD\n touch pwned.txt",
        ] {
            let output = runner
                .run_unchecked(dir.path(), &["rev-parse", "--verify", hostile])
                .unwrap();
            assert!(
                !output.success,
                "git must reject the bogus revision {hostile:?}"
            );
        }
        assert!(
            !dir.path().join("pwned.txt").exists(),
            "no metacharacter may ever reach a shell"
        );
    }
}
