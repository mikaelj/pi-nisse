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

    /// `nisse doctor` found problems; the report is printed to stdout.
    #[error("doctor found problems; see the report on stdout")]
    DoctorFailed,

    /// A command requiring repository context was invoked outside a
    /// Git repository.
    #[error("not inside a Git repository; this command requires a repository")]
    NotInRepository,

    /// `nisse cache status` found problems; the report is printed to stdout.
    #[error("cache status found problems; see the report on stdout")]
    CacheUnhealthy,

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
            Command::Doctor => self.run_doctor(),
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
                crate::cli::RecordsCommand::List => self.run_records_list(),
                crate::cli::RecordsCommand::Pending => Err(AppError::NotImplemented(
                    "nisse records pending".to_string(),
                )),
                crate::cli::RecordsCommand::Compact => Err(AppError::NotImplemented(
                    "nisse records compact".to_string(),
                )),
            },
            Command::Cache { cache } => match cache {
                crate::cli::CacheCommand::Status => self.run_cache_status(),
                crate::cli::CacheCommand::Clear => self.run_cache_clear(),
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

    /// Discover the repository/worktree of the current directory together
    /// with its storage locations.
    fn discover_storage(
        &self,
    ) -> Result<(crate::git::RepoContext, crate::storage::StoragePaths), AppError> {
        let cwd = std::env::current_dir()
            .map_err(|err| anyhow::anyhow!("failed to determine the current directory: {err}"))?;
        let ctx = crate::git::RepoContext::discover(&crate::git::GitRunner::new(), &cwd).map_err(
            |err| match err {
                crate::git::GitError::NotARepository { .. } => AppError::NotInRepository,
                other => AppError::Other(anyhow::anyhow!("repository discovery failed: {other}")),
            },
        )?;
        let paths = crate::storage::StoragePaths::for_repo(&ctx);
        Ok((ctx, paths))
    }

    /// Run `nisse records list`.
    ///
    /// Lists the durable records of the current worktree. Loading is
    /// strict: an invalid `.nisse` tree fails loudly with the offending
    /// path instead of silently skipping records.
    fn run_records_list(&self) -> Result<(), AppError> {
        use crate::storage::records::{RecordStore, render_listing};

        let (ctx, _paths) = self.discover_storage()?;
        let store = RecordStore::new(&ctx.worktree_root);
        let records = store
            .load()
            .map_err(|err| AppError::Other(anyhow::anyhow!("records list failed: {err}")))?;
        print_to_stdout(&render_listing(&store.nisse_dir(), &records));
        Ok(())
    }

    /// Run `nisse cache status`.
    ///
    /// The report is written to stdout. Status never mutates a database;
    /// corrupt or too-new caches are reported as problems.
    fn run_cache_status(&self) -> Result<(), AppError> {
        use crate::storage;

        let (ctx, paths) = self.discover_storage()?;
        let report = storage::status::status(&ctx.worktree_root, &paths)
            .map_err(|err| AppError::Other(anyhow::anyhow!("cache status failed: {err}")))?;
        print_to_stdout(&report.render());
        if report.has_problems() {
            return Err(AppError::CacheUnhealthy);
        }
        Ok(())
    }

    /// Run `nisse cache clear`.
    ///
    /// Deletes both cache databases (including journal sidecars) and
    /// recreates them empty at the current schema version.
    fn run_cache_clear(&self) -> Result<(), AppError> {
        use crate::storage::schema::DbKind;

        let (_ctx, paths) = self.discover_storage()?;
        let mut out = String::from("nisse cache clear\n");
        for (path, kind) in [
            (&paths.shared, DbKind::Shared),
            (&paths.worktree, DbKind::Worktree),
        ] {
            crate::storage::db::clear(path, kind)
                .map_err(|err| AppError::Other(anyhow::anyhow!("cache clear failed: {err}")))?;
            out.push_str(&format!("cleared {}: {}\n", kind.label(), path.display()));
        }
        print_to_stdout(&out);
        Ok(())
    }

    /// Run `nisse doctor`.
    ///
    /// The report is written to stdout; failures additionally produce a
    /// typed error so the process exits non-zero. Doctor never performs
    /// LLM requests.
    fn run_doctor(&self) -> Result<(), AppError> {
        use crate::config::{self, endpoint::SystemResolver};

        let cwd = std::env::current_dir()
            .map_err(|err| anyhow::anyhow!("failed to determine the current directory: {err}"))?;
        let git = config::git_probe::probe_git(&cwd);
        let user_path = config::paths::user_config_path();
        let repo_path = git
            .git_common_dir
            .as_ref()
            .map(|common_dir| config::paths::repo_config_path(common_dir));
        let loaded = config::load_config(user_path.as_deref(), repo_path.as_deref());
        let resolver = SystemResolver;
        let report = config::doctor::run_doctor(git, loaded, &resolver);
        print_to_stdout(&report.render());

        if report.has_failures() {
            return Err(AppError::DoctorFailed);
        }
        Ok(())
    }
}

/// Write a complete report to stdout.
///
/// stdout carries machine-readable and report output; logs go to stderr.
fn print_to_stdout(text: &str) {
    use std::io::Write;
    let mut stdout = std::io::stdout().lock();
    let _ = write!(stdout, "{text}");
    let _ = stdout.flush();
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
    fn every_frozen_command_is_typed_not_implemented() {
        let app = App::new();

        let cases: Vec<Command> = vec![
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
                records: crate::cli::RecordsCommand::Pending,
            },
            Command::Records {
                records: crate::cli::RecordsCommand::Compact,
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
