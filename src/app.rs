//! Application root.
//!
//! The `App` owns command dispatch. Deterministic decisions live in the Rust
//! core; future milestones attach real behavior to the frozen command set.

use thiserror::Error;

use crate::cli::Command;

/// Typed application errors.
#[derive(Debug, Error)]
pub enum AppError {
    /// The command is part of the frozen CLI contract but its implementation
    /// belongs to a later milestone.
    #[error("`{0}` is not implemented yet")]
    NotImplemented(String),

    /// Unclassified application-boundary failure.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Application root.
#[derive(Debug, Default)]
pub struct App;

impl App {
    /// Create the application root.
    pub fn new() -> Self {
        App
    }

    /// Run a parsed CLI command.
    pub fn run(&self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Doctor => Err(AppError::NotImplemented("nisse doctor".to_string())),
            Command::Review { review } => match review {
                crate::cli::ReviewCommand::Plan { .. } => {
                    Err(AppError::NotImplemented("nisse review plan".to_string()))
                }
                crate::cli::ReviewCommand::Code { .. } => {
                    Err(AppError::NotImplemented("nisse review code".to_string()))
                }
            },
            Command::Query { .. } => Err(AppError::NotImplemented("nisse query".to_string())),
            Command::Feedback { .. } => Err(AppError::NotImplemented("nisse feedback".to_string())),
            Command::Records { records } => match records {
                crate::cli::RecordsCommand::List => {
                    Err(AppError::NotImplemented("nisse records list".to_string()))
                }
                crate::cli::RecordsCommand::Pending => Err(AppError::NotImplemented(
                    "nisse records pending".to_string(),
                )),
                crate::cli::RecordsCommand::Compact => Err(AppError::NotImplemented(
                    "nisse records compact".to_string(),
                )),
            },
            Command::Cache { cache } => match cache {
                crate::cli::CacheCommand::Status => {
                    Err(AppError::NotImplemented("nisse cache status".to_string()))
                }
                crate::cli::CacheCommand::Clear => {
                    Err(AppError::NotImplemented("nisse cache clear".to_string()))
                }
            },
            Command::Eval { eval } => match eval {
                crate::cli::EvalCommand::Routing => {
                    Err(AppError::NotImplemented("nisse eval routing".to_string()))
                }
                crate::cli::EvalCommand::Retrieval => {
                    Err(AppError::NotImplemented("nisse eval retrieval".to_string()))
                }
            },
            Command::Mcp => Err(AppError::NotImplemented("nisse mcp".to_string())),
        }
    }
}

/// Configure tracing output.
///
/// Logs always go to stderr; stdout is reserved for protocol and
/// machine-readable output.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    tracing::debug!(
        version = env!("CARGO_PKG_VERSION"),
        "nisse tracing initialized"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_is_not_implemented() {
        let app = App::new();
        let err = app.run(Command::Doctor).unwrap_err();
        assert!(matches!(err, AppError::NotImplemented(ref cmd) if cmd == "nisse doctor"));
    }

    #[test]
    fn every_frozen_command_is_typed_not_implemented() {
        let app = App::new();

        let cases: Vec<Command> = vec![
            Command::Doctor,
            Command::Review {
                review: crate::cli::ReviewCommand::Plan {
                    source: crate::cli::PlanSource {
                        file: Some("plan.md".into()),
                        stdin: false,
                    },
                    json: false,
                },
            },
            Command::Review {
                review: crate::cli::ReviewCommand::Code {
                    scope: crate::cli::CodeScope {
                        working: true,
                        staged: false,
                        commit: None,
                        range: None,
                        branch: None,
                    },
                    json: false,
                },
            },
            Command::Query {
                query: "queues".into(),
                json: false,
            },
            Command::Feedback {
                review_id: "01J8Z".into(),
                finding_id: "01J8Y".into(),
                outcome: crate::cli::FeedbackOutcome {
                    fixed: true,
                    accepted: false,
                    dismissed: false,
                },
            },
            Command::Records {
                records: crate::cli::RecordsCommand::List,
            },
            Command::Records {
                records: crate::cli::RecordsCommand::Pending,
            },
            Command::Records {
                records: crate::cli::RecordsCommand::Compact,
            },
            Command::Cache {
                cache: crate::cli::CacheCommand::Status,
            },
            Command::Cache {
                cache: crate::cli::CacheCommand::Clear,
            },
            Command::Eval {
                eval: crate::cli::EvalCommand::Routing,
            },
            Command::Eval {
                eval: crate::cli::EvalCommand::Retrieval,
            },
            Command::Mcp,
        ];

        for command in cases {
            let err = app.run(command).unwrap_err();
            assert!(
                matches!(err, AppError::NotImplemented(_)),
                "expected NotImplemented, got {err:?}"
            );
        }
    }
}
