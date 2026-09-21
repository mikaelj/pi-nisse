//! Pi review host child-process handling (plan sections 20 and 21).
//!
//! [`HostProcess`] spawns the TypeScript host (`runtime/pi-host`), speaks
//! the JSONL protocol defined in [`super::protocol`] over the child's
//! stdin/stdout, and never trusts the child beyond the frozen schema:
//!
//! - every outbound message is a single strict JSON line;
//! - every inbound line is envelope-checked (version, id, type) before a
//!   typed payload is decoded;
//! - malformed or oversized lines, wrong protocol versions, unexpected
//!   message types, timeouts, and child crashes are typed errors;
//! - stderr is captured separately and never mixed into protocol parsing
//!   (stdout purity);
//! - dropping the client kills and reaps the child.

use std::io::{BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::protocol::{
    self, CancelMessage, CancelPayload, DomainDescriptionOutcome, FactSynthesisOutcome,
    FactSynthesisPayload, FactVerificationPayload, FindingEquivalenceCheckPayload,
    FindingEquivalenceOutcome, HostConfigMessage, HostConfigPayload, HostEvent, HostReady,
    HostRuntime, ProtocolError, StartDomainDescriptionMessage, StartFactSynthesisMessage,
    StartFactVerificationMessage, StartFindingEquivalenceCheckMessage, StartReviewMessage,
    StartReviewPayload, ToolRequestPayload, ToolResponseMessage, ToolResponsePayload,
};
use crate::domain::facts::FactVerificationResult;
use crate::domain::report::ReviewerReport;

/// Maximum accepted host output line length. The host speaks strict small
/// JSON messages; anything larger is treated as protocol corruption.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum stderr retained for diagnostics; older bytes are dropped.
const MAX_STDERR_BYTES: usize = 1024 * 1024;

/// Maximum time a terminal-error path polls a still-running host child
/// before killing it (and, after the kill, polls once more). Every other
/// host interaction is deadline-bounded; child reaping must be too, or a
/// child that closes its pipes without exiting would hang the caller
/// indefinitely.
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll interval of the bounded reaper (`try_wait` loop).
const CHILD_REAP_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Reap the host child within a bounded budget.
///
/// Polls `try_wait` for [`CHILD_REAP_TIMEOUT`]; on expiry kills the child
/// and polls for one more window. A well-behaved exiting child still
/// yields its real exit status because the poll observes it.
///
/// Returning `None` means the child survived even the kill (a pathological
/// case): the status is unknown, the child's pipe write ends stay open so
/// the reader/stderr threads may remain blocked until process exit, and
/// the owning [`HostProcess`] must be discarded. `None` never blocks the
/// caller for longer than about `2 * CHILD_REAP_TIMEOUT`.
fn reap_child(child: &mut Child) -> Option<ExitStatus> {
    let poll = |child: &mut Child| -> Option<ExitStatus> {
        let deadline = Instant::now() + CHILD_REAP_TIMEOUT;
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(CHILD_REAP_POLL_INTERVAL);
        }
    };
    if let Some(status) = poll(child) {
        return Some(status);
    }
    // Already-exited children make kill() error; that is fine.
    let _ = child.kill();
    poll(child)
}

/// How to launch the Pi review host child process.
#[derive(Debug, Clone)]
pub struct PiHostLaunch {
    /// Interpreter binary (normally `node`; Node >= 23.6 runs the
    /// TypeScript host directly via type stripping).
    pub program: String,
    /// Path of the host entry script (`runtime/pi-host/src/main.ts`).
    pub script: std::path::PathBuf,
    /// Which reviewer runtime the host should use (`--runtime`).
    pub runtime: HostRuntime,
    /// Extra environment variables for the child (e.g. fault injection).
    pub extra_env: Vec<(String, String)>,
    /// Working directory for the child, if any.
    pub cwd: Option<std::path::PathBuf>,
}

impl PiHostLaunch {
    /// Launch configuration for `script` with the given runtime and no
    /// extra environment.
    pub fn new(program: &str, script: std::path::PathBuf, runtime: HostRuntime) -> Self {
        PiHostLaunch {
            program: program.to_string(),
            script,
            runtime,
            extra_env: Vec::new(),
            cwd: None,
        }
    }

    /// Add one environment variable.
    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.extra_env.push((key.to_string(), value.to_string()));
        self
    }

    /// Build the child command.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .arg(&self.script)
            .arg("--runtime")
            .arg(self.runtime.as_arg())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &self.extra_env {
            command.env(key, value);
        }
        command
    }
}

/// Failures of the host process boundary.
#[derive(Debug, Error)]
pub enum HostError {
    /// Spawning the child failed.
    #[error("failed to spawn pi review host `{program}`: {source}")]
    Spawn {
        /// The interpreter that was invoked.
        program: String,
        /// The underlying spawn failure.
        source: std::io::Error,
    },

    /// Writing to the child's stdin failed.
    #[error("failed to write to pi review host stdin: {0}")]
    WriteFailed(std::io::Error),

    /// Reading the child's stdout failed.
    #[error("failed to read from pi review host stdout: {0}")]
    ReadFailed(std::io::Error),

    /// The host emitted a line that is not a valid protocol message.
    #[error("pi review host emitted a malformed line: {reason}")]
    MalformedHostLine {
        /// Why the line was rejected.
        reason: String,
    },

    /// The host spoke a different protocol version.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),

    /// The host sent a message that does not fit the current exchange
    /// (wrong review, unknown reviewer, message before `host_ready`, …).
    #[error("pi review host sent an unexpected message: {reason}")]
    UnexpectedHostMessage {
        /// Why the message is unexpected.
        reason: String,
    },

    /// The host reported a protocol-level `host_error`.
    #[error("pi review host reported error `{code}`: {message}")]
    HostReportedError {
        /// Host error code.
        code: String,
        /// Host error message.
        message: String,
    },

    /// An operation did not complete within its deadline.
    #[error("timed out waiting for {what}")]
    Timeout {
        /// What was being waited for.
        what: String,
    },

    /// The child terminated while messages were still expected.
    #[error("pi review host terminated while waiting for {waiting_for}: {status}")]
    ChildTerminated {
        /// Human-readable description of the wait.
        waiting_for: String,
        /// The child's exit status.
        status: String,
    },
}

/// Raw events delivered by the reader thread.
enum ChannelEvent {
    Line(String),
    Malformed { reason: String },
    Eof,
    ReadFailed(String),
}

/// The outcome of one reviewer within a started review.
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewerOutcome {
    /// The reviewer produced a structured report.
    Completed(ReviewerReport),
    /// The reviewer session failed.
    Failed(protocol::HostErrorInfo),
}

impl ReviewerOutcome {
    /// The completed report, if this outcome is a completion.
    pub fn as_report(&self) -> Option<&ReviewerReport> {
        match self {
            ReviewerOutcome::Completed(report) => Some(report),
            ReviewerOutcome::Failed(_) => None,
        }
    }

    /// The failure info, if this outcome is a failure.
    pub fn as_failure(&self) -> Option<&protocol::HostErrorInfo> {
        match self {
            ReviewerOutcome::Completed(_) => None,
            ReviewerOutcome::Failed(info) => Some(info),
        }
    }
}

/// A running Pi review host child process.
pub struct HostProcess {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    events: Receiver<ChannelEvent>,
    reader: Option<JoinHandle<()>>,
    stderr_handle: Option<JoinHandle<()>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
    ready: bool,
}

impl HostProcess {
    /// Spawn the host described by `launch` and start reading its stdout.
    pub fn spawn(launch: &PiHostLaunch) -> Result<Self, HostError> {
        let mut child = launch
            .command()
            .spawn()
            .map_err(|source| HostError::Spawn {
                program: launch.program.clone(),
                source,
            })?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("nisse-pi-host-stdout".to_string())
            .spawn(move || read_stdout(stdout, tx))
            .expect("failed to spawn pi host reader thread");

        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_handle = stderr.map(|pipe| {
            let buffer = Arc::clone(&stderr_buffer);
            std::thread::Builder::new()
                .name("nisse-pi-host-stderr".to_string())
                .spawn(move || drain_stderr(pipe, &buffer))
                .expect("failed to spawn pi host stderr thread")
        });

        Ok(HostProcess {
            child,
            stdin,
            events: rx,
            reader: Some(reader),
            stderr_handle,
            stderr_buffer,
            ready: false,
        })
    }

    /// Write one raw line to the host's stdin.
    ///
    /// Intended for protocol conformance testing and diagnostics; normal
    /// traffic uses the typed senders below.
    pub fn send_raw_line(&mut self, line: &str) -> Result<(), HostError> {
        self.write_line(line)
    }

    fn write_line(&mut self, line: &str) -> Result<(), HostError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(HostError::WriteFailed(std::io::Error::other(
                "stdin already closed",
            )));
        };
        if let Err(err) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
        {
            // A broken pipe almost always means the child died; report the
            // termination instead of a bare write error when possible. The
            // reap is bounded: a child that broke the pipe but still runs
            // is killed instead of blocking this write path forever.
            return Err(match reap_child(&mut self.child) {
                Some(status) => HostError::ChildTerminated {
                    waiting_for: "stdin write".to_string(),
                    status: exit_status_text(&status),
                },
                None => HostError::WriteFailed(err),
            });
        }
        Ok(())
    }

    /// Send `host_config`: the resolved model profiles the host may use for
    /// reviewer sessions.
    pub fn send_host_config(&mut self, payload: &HostConfigPayload) -> Result<(), HostError> {
        let id = crate::domain::ids::new_ulid();
        let line = HostConfigMessage::new(id, payload.clone()).to_line();
        self.write_line(&line)
    }

    /// Send `start_review`.
    pub fn start_review(&mut self, payload: &StartReviewPayload) -> Result<(), HostError> {
        let id = crate::domain::ids::new_ulid();
        let line = StartReviewMessage::new(id, payload.clone()).to_line();
        self.write_line(&line)
    }

    /// Send `start_finding_equivalence_check` for one narrow comparison.
    pub fn start_finding_equivalence_check(
        &mut self,
        payload: &FindingEquivalenceCheckPayload,
    ) -> Result<(), HostError> {
        let id = crate::domain::ids::new_ulid();
        let line = StartFindingEquivalenceCheckMessage::new(id, payload.clone()).to_line();
        self.write_line(&line)
    }

    /// Send `start_domain_description` for one constrained description
    /// job (plan section 19).
    pub fn start_domain_description(
        &mut self,
        payload: &super::protocol::DomainDescriptionPayload,
    ) -> Result<(), HostError> {
        let id = crate::domain::ids::new_ulid();
        let line = StartDomainDescriptionMessage::new(id, payload.clone()).to_line();
        self.write_line(&line)
    }

    /// Send `start_fact_verification` for one independent verification
    /// job (plan section 47 step 2).
    pub fn start_fact_verification(
        &mut self,
        payload: &FactVerificationPayload,
    ) -> Result<(), HostError> {
        let id = crate::domain::ids::new_ulid();
        let line = StartFactVerificationMessage::new(id, payload.clone()).to_line();
        self.write_line(&line)
    }

    /// Send `start_fact_synthesis` for one fresh synthesis job (plan
    /// section 54 step 5).
    pub fn start_fact_synthesis(
        &mut self,
        payload: &FactSynthesisPayload,
    ) -> Result<(), HostError> {
        let id = crate::domain::ids::new_ulid();
        let line = StartFactSynthesisMessage::new(id, payload.clone()).to_line();
        self.write_line(&line)
    }

    /// Send `tool_response`.
    pub fn send_tool_response(&mut self, payload: &ToolResponsePayload) -> Result<(), HostError> {
        let id = crate::domain::ids::new_ulid();
        let line = ToolResponseMessage::new(id, payload.clone()).to_line();
        self.write_line(&line)
    }

    /// Send `cancel` for one review or for all pending work.
    pub fn send_cancel(&mut self, review_id: Option<&str>) -> Result<(), HostError> {
        let payload = match review_id {
            Some(id) => CancelPayload::review(id),
            None => CancelPayload::all(),
        };
        let line = CancelMessage::new(crate::domain::ids::new_ulid(), payload).to_line();
        self.write_line(&line)
    }

    /// Block until the next decoded host event.
    pub fn recv_event(&mut self, timeout: Duration) -> Result<HostEvent, HostError> {
        match self.events.recv_timeout(timeout) {
            Ok(ChannelEvent::Line(line)) => {
                let envelope = protocol::Envelope::parse_host_line(&line)?;
                protocol::parse_host_event(envelope).map_err(HostError::Protocol)
            }
            Ok(ChannelEvent::Malformed { reason }) => Err(HostError::MalformedHostLine { reason }),
            Ok(ChannelEvent::ReadFailed(message)) => {
                let status = reap_child(&mut self.child);
                Err(HostError::ChildTerminated {
                    waiting_for: format!("host output (read error: {message})"),
                    status: status
                        .map(|s| exit_status_text(&s))
                        .unwrap_or_else(|| "unknown".to_string()),
                })
            }
            Ok(ChannelEvent::Eof) => {
                let status = reap_child(&mut self.child);
                Err(HostError::ChildTerminated {
                    waiting_for: "host output".to_string(),
                    status: status
                        .map(|s| exit_status_text(&s))
                        .unwrap_or_else(|| "unknown".to_string()),
                })
            }
            Err(RecvTimeoutError::Timeout) => Err(HostError::Timeout {
                what: "the next host event".to_string(),
            }),
            Err(RecvTimeoutError::Disconnected) => {
                let status = reap_child(&mut self.child);
                Err(HostError::ChildTerminated {
                    waiting_for: "host output".to_string(),
                    status: status
                        .map(|s| exit_status_text(&s))
                        .unwrap_or_else(|| "unknown".to_string()),
                })
            }
        }
    }

    /// Wait for `host_ready`. No other message may precede it.
    pub fn wait_ready(&mut self, timeout: Duration) -> Result<HostReady, HostError> {
        match self.recv_event(timeout)? {
            HostEvent::HostReady(ready) => {
                self.ready = true;
                Ok(ready)
            }
            other => Err(HostError::UnexpectedHostMessage {
                reason: format!(
                    "expected `host_ready` but received `{}` before startup completed",
                    event_type_name(&other)
                ),
            }),
        }
    }

    /// Drive one review to completion.
    ///
    /// Waits until every reviewer in `reviewers` has completed or failed,
    /// answering every `tool_request` of that review with `tool_handler`.
    /// This is generic protocol driving: review policy (validity minimums,
    /// retries, synthesis) belongs to later milestones.
    pub fn complete_review(
        &mut self,
        review_id: &str,
        reviewers: &[String],
        tool_handler: &mut dyn FnMut(&ToolRequestPayload) -> ToolResponsePayload,
        timeout: Duration,
    ) -> Result<Vec<(String, ReviewerOutcome)>, HostError> {
        if !self.ready {
            return Err(HostError::UnexpectedHostMessage {
                reason: "complete_review called before host_ready".to_string(),
            });
        }
        let deadline = Instant::now() + timeout;
        let mut pending: Vec<String> = reviewers.to_vec();
        let mut outcomes: Vec<(String, ReviewerOutcome)> = Vec::new();

        while !pending.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| HostError::Timeout {
                    what: format!("review `{review_id}` to complete"),
                })?;
            match self.recv_event(remaining)? {
                HostEvent::ReviewerStarted { review_id: r, .. } => {
                    if r != review_id {
                        return Err(HostError::UnexpectedHostMessage {
                            reason: format!(
                                "reviewer_started for review `{r}` while driving `{review_id}`"
                            ),
                        });
                    }
                }
                HostEvent::ReviewerCompleted {
                    review_id: r,
                    reviewer_id,
                    report,
                } => {
                    if r != review_id {
                        return Err(HostError::UnexpectedHostMessage {
                            reason: format!(
                                "reviewer_completed for review `{r}` while driving `{review_id}`"
                            ),
                        });
                    }
                    resolve(&mut pending, &mut outcomes, &reviewer_id, || {
                        ReviewerOutcome::Completed(report.clone())
                    })
                    .map_err(|reviewer_id| HostError::UnexpectedHostMessage {
                        reason: format!(
                            "reviewer_completed for unexpected or duplicate reviewer `{reviewer_id}`"
                        ),
                    })?;
                }
                HostEvent::ReviewerFailed {
                    review_id: r,
                    reviewer_id,
                    error,
                } => {
                    if r != review_id {
                        return Err(HostError::UnexpectedHostMessage {
                            reason: format!(
                                "reviewer_failed for review `{r}` while driving `{review_id}`"
                            ),
                        });
                    }
                    resolve(&mut pending, &mut outcomes, &reviewer_id, || {
                        ReviewerOutcome::Failed(error.clone())
                    })
                    .map_err(|reviewer_id| HostError::UnexpectedHostMessage {
                        reason: format!(
                            "reviewer_failed for unexpected or duplicate reviewer `{reviewer_id}`"
                        ),
                    })?;
                }
                HostEvent::ToolRequest { id, payload } => {
                    if payload.review_id != review_id {
                        return Err(HostError::UnexpectedHostMessage {
                            reason: format!(
                                "tool_request for review `{}` while driving `{review_id}`",
                                payload.review_id
                            ),
                        });
                    }
                    let response = tool_handler(&payload);
                    let payload = ToolResponsePayload {
                        request_id: id,
                        ..response
                    };
                    self.send_tool_response(&payload)?;
                }
                HostEvent::HostReady(_) => {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: "duplicate `host_ready` after startup".to_string(),
                    });
                }
                HostEvent::FindingEquivalenceCompleted { check_id, .. } => {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "finding_equivalence_completed for check `{check_id}` arrived while driving review `{review_id}`"
                        ),
                    });
                }
                HostEvent::DomainDescriptionCompleted { description_id, .. } => {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "domain_description_completed for job `{description_id}` arrived while driving review `{review_id}`"
                        ),
                    });
                }
                HostEvent::FactVerificationCompleted {
                    verification_id, ..
                } => {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "fact_verification_completed for job `{verification_id}` arrived while driving review `{review_id}`"
                        ),
                    });
                }
                HostEvent::FactSynthesisCompleted { synthesis_id, .. } => {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "fact_synthesis_completed for job `{synthesis_id}` arrived while driving review `{review_id}`"
                        ),
                    });
                }
                HostEvent::HostError(info) => {
                    return Err(HostError::HostReportedError {
                        code: info.code,
                        message: info.message,
                    });
                }
            }
        }
        Ok(outcomes)
    }

    /// Drive one batch of finding-equivalence checks to completion.
    ///
    /// Waits until every check in `expected` (the correlation ids of the
    /// checks already sent) has reported `finding_equivalence_completed`.
    /// Checks execute independently in the host; a check whose session
    /// failed completes with the `uncertain` verdict plus an error
    /// rather than failing the batch. Protocol-level failures, child
    /// death, reviewer traffic, and unknown check ids are typed errors.
    /// The returned outcomes keep the order of `expected`.
    pub fn complete_equivalence_checks(
        &mut self,
        expected: &[String],
        timeout: Duration,
    ) -> Result<Vec<FindingEquivalenceOutcome>, HostError> {
        if !self.ready {
            return Err(HostError::UnexpectedHostMessage {
                reason: "complete_equivalence_checks called before host_ready".to_string(),
            });
        }
        let deadline = Instant::now() + timeout;
        let mut pending: Vec<String> = expected.to_vec();
        let mut outcomes: Vec<FindingEquivalenceOutcome> = Vec::new();

        while !pending.is_empty() {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| HostError::Timeout {
                    what: "finding-equivalence checks to complete".to_string(),
                })?;
            match self.recv_event(remaining)? {
                HostEvent::FindingEquivalenceCompleted {
                    check_id,
                    verdict,
                    error,
                } => {
                    let Some(index) = pending.iter().position(|id| *id == check_id) else {
                        return Err(HostError::UnexpectedHostMessage {
                            reason: format!(
                                "finding_equivalence_completed for unknown or duplicate check `{check_id}`"
                            ),
                        });
                    };
                    pending.swap_remove(index);
                    outcomes.push(FindingEquivalenceOutcome {
                        check_id,
                        verdict,
                        error,
                    });
                }
                HostEvent::HostError(info) => {
                    return Err(HostError::HostReportedError {
                        code: info.code,
                        message: info.message,
                    });
                }
                other => {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "received `{}` while driving finding-equivalence checks",
                            event_type_name(&other)
                        ),
                    });
                }
            }
        }

        let by_check: std::collections::BTreeMap<String, FindingEquivalenceOutcome> = outcomes
            .into_iter()
            .map(|outcome| (outcome.check_id.clone(), outcome))
            .collect();
        Ok(expected
            .iter()
            .map(|check_id| by_check.get(check_id).cloned())
            .collect::<Option<Vec<_>>>()
            .expect("every expected check resolved"))
    }

    /// Drive one dynamic-domain description job to completion (plan
    /// section 19).
    ///
    /// Waits for the `domain_description_completed` answering
    /// `description_id`. A session that failed completes without a
    /// description plus an error rather than failing the exchange;
    /// protocol-level failures, child death, reviewer traffic, and a
    /// mismatched correlation id are typed errors.
    pub fn complete_domain_description(
        &mut self,
        description_id: &str,
        timeout: Duration,
    ) -> Result<DomainDescriptionOutcome, HostError> {
        if !self.ready {
            return Err(HostError::UnexpectedHostMessage {
                reason: "complete_domain_description called before host_ready".to_string(),
            });
        }
        let deadline = Instant::now() + timeout;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| HostError::Timeout {
                what: format!("domain description `{description_id}` to complete"),
            })?;
        match self.recv_event(remaining)? {
            HostEvent::DomainDescriptionCompleted {
                description_id: found,
                description,
                error,
            } => {
                if found != description_id {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "domain_description_completed for job `{found}` while driving `{description_id}`"
                        ),
                    });
                }
                Ok(DomainDescriptionOutcome {
                    description_id: found,
                    description,
                    error,
                })
            }
            HostEvent::HostError(info) => Err(HostError::HostReportedError {
                code: info.code,
                message: info.message,
            }),
            other => Err(HostError::UnexpectedHostMessage {
                reason: format!(
                    "received `{}` while driving domain description `{description_id}`",
                    event_type_name(&other)
                ),
            }),
        }
    }

    /// Drive one independent fact-verification job to completion (plan
    /// section 47 step 2).
    ///
    /// Waits for the `fact_verification_completed` answering
    /// `verification_id`. A verifier session that failed completes with
    /// the `insufficient` verdict plus an error rather than failing the
    /// exchange — uncertainty keeps the candidate pending in the core
    /// and never persists or rejects anything by itself. Protocol-level
    /// failures, child death, reviewer traffic, and a mismatched
    /// correlation id are typed errors.
    pub fn complete_fact_verification(
        &mut self,
        verification_id: &str,
        timeout: Duration,
    ) -> Result<FactVerificationResult, HostError> {
        if !self.ready {
            return Err(HostError::UnexpectedHostMessage {
                reason: "complete_fact_verification called before host_ready".to_string(),
            });
        }
        let deadline = Instant::now() + timeout;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| HostError::Timeout {
                what: format!("fact verification `{verification_id}` to complete"),
            })?;
        match self.recv_event(remaining)? {
            HostEvent::FactVerificationCompleted {
                verification_id: found,
                verdict,
                error,
            } => {
                if found != verification_id {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "fact_verification_completed for job `{found}` while driving `{verification_id}`"
                        ),
                    });
                }
                Ok(FactVerificationResult {
                    verification_id: found,
                    verdict,
                    failure: error.map(|error| format!("{}: {}", error.code, error.message)),
                })
            }
            HostEvent::HostError(info) => Err(HostError::HostReportedError {
                code: info.code,
                message: info.message,
            }),
            other => Err(HostError::UnexpectedHostMessage {
                reason: format!(
                    "received `{}` while driving fact verification `{verification_id}`",
                    event_type_name(&other)
                ),
            }),
        }
    }

    /// Drive one fresh fact-synthesis job to completion (plan section
    /// 54 step 5).
    ///
    /// Waits for the `fact_synthesis_completed` answering
    /// `synthesis_id`. A synthesis session that failed completes
    /// without a result plus an error rather than failing the exchange
    /// — the core treats it as uncertainty and consolidates nothing.
    /// Protocol-level failures, child death, reviewer traffic, and a
    /// mismatched correlation id are typed errors.
    pub fn complete_fact_synthesis(
        &mut self,
        synthesis_id: &str,
        timeout: Duration,
    ) -> Result<FactSynthesisOutcome, HostError> {
        if !self.ready {
            return Err(HostError::UnexpectedHostMessage {
                reason: "complete_fact_synthesis called before host_ready".to_string(),
            });
        }
        let deadline = Instant::now() + timeout;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| HostError::Timeout {
                what: format!("fact synthesis `{synthesis_id}` to complete"),
            })?;
        match self.recv_event(remaining)? {
            HostEvent::FactSynthesisCompleted {
                synthesis_id: found,
                result,
                error,
            } => {
                if found != synthesis_id {
                    return Err(HostError::UnexpectedHostMessage {
                        reason: format!(
                            "fact_synthesis_completed for job `{found}` while driving `{synthesis_id}`"
                        ),
                    });
                }
                Ok(FactSynthesisOutcome {
                    synthesis_id: found,
                    result,
                    error,
                })
            }
            HostEvent::HostError(info) => Err(HostError::HostReportedError {
                code: info.code,
                message: info.message,
            }),
            other => Err(HostError::UnexpectedHostMessage {
                reason: format!(
                    "received `{}` while driving fact synthesis `{synthesis_id}`",
                    event_type_name(&other)
                ),
            }),
        }
    }

    /// Gracefully shut down: close stdin, wait up to `timeout` for the
    /// child to exit, then kill it if necessary. Returns the exit status.
    pub fn shutdown(mut self, timeout: Duration) -> Result<ExitStatus, HostError> {
        self.stdin.take();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.join_threads();
                    return Ok(status);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(err) => {
                    self.join_threads();
                    return Err(HostError::ReadFailed(err));
                }
            }
        }
        self.kill_and_reap();
        Err(HostError::Timeout {
            what: "the pi review host to exit after stdin close".to_string(),
        })
    }

    /// The child's stderr output captured so far (lossy UTF-8, bounded).
    pub fn stderr_text(&self) -> String {
        let buffer = self.stderr_buffer.lock().expect("stderr buffer poisoned");
        String::from_utf8_lossy(&buffer).into_owned()
    }

    fn kill_and_reap(&mut self) {
        let _ = self.child.kill();
        // Bounded reap: a child that somehow survives the kill must not
        // block Drop (or the shutdown-timeout path) forever. When the reap
        // gives up, the reader/stderr threads stay blocked on the child's
        // still-open pipes and are left to drain at process exit.
        if reap_child(&mut self.child).is_some() {
            self.join_threads();
        }
    }

    fn join_threads(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(handle) = self.stderr_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for HostProcess {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

fn resolve(
    pending: &mut Vec<String>,
    outcomes: &mut Vec<(String, ReviewerOutcome)>,
    reviewer_id: &str,
    build: impl FnOnce() -> ReviewerOutcome,
) -> Result<(), String> {
    let Some(index) = pending.iter().position(|id| id == reviewer_id) else {
        return Err(reviewer_id.to_string());
    };
    pending.swap_remove(index);
    outcomes.push((reviewer_id.to_string(), build()));
    Ok(())
}

fn event_type_name(event: &HostEvent) -> &'static str {
    match event {
        HostEvent::HostReady(_) => "host_ready",
        HostEvent::ReviewerStarted { .. } => "reviewer_started",
        HostEvent::ReviewerCompleted { .. } => "reviewer_completed",
        HostEvent::ReviewerFailed { .. } => "reviewer_failed",
        HostEvent::ToolRequest { .. } => "tool_request",
        HostEvent::FindingEquivalenceCompleted { .. } => "finding_equivalence_completed",
        HostEvent::DomainDescriptionCompleted { .. } => "domain_description_completed",
        HostEvent::FactVerificationCompleted { .. } => "fact_verification_completed",
        HostEvent::FactSynthesisCompleted { .. } => "fact_synthesis_completed",
        HostEvent::HostError(_) => "host_error",
    }
}

fn exit_status_text(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => format!("terminated by signal: {status}"),
    }
}

/// Read stdout lines forever, forwarding them to the client thread.
///
/// The reader never panics on bad output; malformed input is delivered as
/// an event so the client can fail with a typed error.
fn read_stdout(stdout: Option<std::process::ChildStdout>, sender: Sender<ChannelEvent>) {
    let Some(stdout) = stdout else {
        let _ = sender.send(ChannelEvent::Eof);
        return;
    };
    let mut reader = BufReader::new(stdout);
    let mut byte_count = 0usize;
    loop {
        let mut bytes: Vec<u8> = Vec::new();
        // Read one byte at a time to enforce the line bound exactly.
        let mut one = [0u8; 1];
        loop {
            match reader.read(&mut one) {
                Ok(0) => break,
                Ok(_) => {
                    byte_count = byte_count.saturating_add(1);
                    if one[0] == b'\n' {
                        break;
                    }
                    if bytes.len() >= MAX_LINE_BYTES {
                        let _ = sender.send(ChannelEvent::Malformed {
                            reason: format!(
                                "line exceeds the {MAX_LINE_BYTES} byte protocol bound"
                            ),
                        });
                        return;
                    }
                    bytes.push(one[0]);
                }
                Err(err) => {
                    let _ = sender.send(ChannelEvent::ReadFailed(err.to_string()));
                    return;
                }
            }
        }
        if bytes.is_empty() && byte_count == 0 {
            let _ = sender.send(ChannelEvent::Eof);
            return;
        }
        let line = match String::from_utf8(bytes) {
            Ok(line) => line,
            Err(_) => {
                let _ = sender.send(ChannelEvent::Malformed {
                    reason: "line is not valid UTF-8".to_string(),
                });
                return;
            }
        };
        let line = line.trim_end_matches('\r').to_string();
        if sender.send(ChannelEvent::Line(line)).is_err() {
            return;
        }
        byte_count = 0;
    }
}

/// Drain the child's stderr into a bounded shared buffer.
fn drain_stderr<R: Read>(pipe: R, buffer: &Mutex<Vec<u8>>) {
    let mut reader = BufReader::new(pipe);
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => {
                let mut buffer = buffer.lock().expect("stderr buffer poisoned");
                if buffer.len() + n > MAX_STDERR_BYTES {
                    let overflow = buffer.len() + n - MAX_STDERR_BYTES;
                    let keep = buffer.len().saturating_sub(overflow);
                    buffer.drain(..keep);
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_command_selects_runtime_and_script() {
        let launch = PiHostLaunch::new(
            "node",
            std::path::PathBuf::from("/tmp/pi-host/src/main.ts"),
            HostRuntime::Fake,
        )
        .env("NISSE_PI_HOST_FAULT", "stderr-noise");

        let debug = format!("{:?}", launch.command());
        assert!(debug.contains("main.ts"));
        assert!(debug.contains("--runtime"));
        assert!(debug.contains("fake"));
        assert!(debug.contains("NISSE_PI_HOST_FAULT"));
    }

    #[test]
    fn reap_child_kills_a_lingering_child_within_the_budget() {
        // A child that simply never exits (like a host that ignores its
        // deadline) must be killed and reaped within roughly two reap
        // budgets, never waited on indefinitely. Cross-platform (the
        // linger fixtures in tests/integration cover the full EOF/EPIPE
        // paths on POSIX).
        let mut child = Command::new("node")
            .args(["-e", "setTimeout(() => {}, 15000)"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning the lingering node child failed");
        let started = Instant::now();
        let status = reap_child(&mut child);
        let elapsed = started.elapsed();
        let status = status.expect("the lingering child must be killed and reaped");
        assert!(
            elapsed < Duration::from_secs(8),
            "bounded reap took {elapsed:?} (status {status})"
        );
    }
}
