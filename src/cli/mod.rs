//! Frozen CLI contract.
//!
//! The complete command surface of Nisse 1.0 is defined here. Commands whose
//! implementation belongs to later milestones parse successfully but dispatch
//! to typed `NotImplemented` errors.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Nisse local software-review engine.
#[derive(Debug, Parser)]
#[command(
    name = "nisse",
    version,
    about = "Nisse local software-review engine",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check local environment and configuration.
    Doctor,

    /// Run reviews.
    Review {
        #[command(subcommand)]
        review: ReviewCommand,
    },

    /// Query indexed project knowledge.
    Query {
        /// Free-text project query.
        query: String,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Record a known outcome for a review finding.
    Feedback {
        /// Review identifier.
        review_id: String,

        /// Finding identifier.
        finding_id: String,

        #[command(flatten)]
        outcome: FeedbackOutcome,
    },

    /// Inspect durable project records.
    Records {
        #[command(subcommand)]
        records: RecordsCommand,
    },

    /// Inspect generated local caches.
    Cache {
        #[command(subcommand)]
        cache: CacheCommand,
    },

    /// Run evaluation fixtures.
    Eval {
        #[command(subcommand)]
        eval: EvalCommand,
    },

    /// Run the MCP stdio server.
    Mcp,
}

#[derive(Debug, Subcommand)]
pub enum ReviewCommand {
    /// Review an implementation plan.
    Plan {
        #[command(flatten)]
        source: PlanSource,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Review code changes.
    Code {
        #[command(flatten)]
        scope: CodeScope,

        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Plan input source: exactly one of `--file <path>` or `--stdin`.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct PlanSource {
    /// Read the plan from a file.
    #[arg(long)]
    pub file: Option<PathBuf>,

    /// Read the plan from stdin.
    #[arg(long)]
    pub stdin: bool,
}

/// Code review snapshot scope: exactly one mode.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct CodeScope {
    /// Compare HEAD against the final working-tree state.
    #[arg(long)]
    pub working: bool,

    /// Compare HEAD against the index.
    #[arg(long)]
    pub staged: bool,

    /// Compare the commit against its first parent.
    #[arg(long)]
    pub commit: Option<String>,

    /// Compare BASE tree against HEAD tree (`<base>..<head>`).
    #[arg(long)]
    pub range: Option<String>,

    /// Compare merge-base tree against HEAD tree.
    #[arg(long)]
    pub branch: Option<String>,
}

/// Feedback outcome: exactly one of `--fixed`, `--accepted`, `--dismissed`.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub struct FeedbackOutcome {
    /// Implementation changed specifically to address the finding.
    #[arg(long)]
    pub fixed: bool,

    /// Finding valid but intentionally not fixed now.
    #[arg(long)]
    pub accepted: bool,

    /// Finding investigated and judged incorrect or not applicable.
    #[arg(long)]
    pub dismissed: bool,
}

#[derive(Debug, Subcommand)]
pub enum RecordsCommand {
    /// List durable project records.
    List,

    /// List pending learning state.
    Pending,

    /// Consolidate durable records.
    Compact,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Show generated cache status.
    Status,

    /// Clear and recreate generated caches.
    Clear,
}

#[derive(Debug, Subcommand)]
pub enum EvalCommand {
    /// Run routing evaluation fixtures.
    Routing,

    /// Run retrieval evaluation fixtures.
    Retrieval,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_review_plan_file() {
        let cli = Cli::try_parse_from(["nisse", "review", "plan", "--file", "plan.md"]).unwrap();
        match cli.command {
            Command::Review {
                review: ReviewCommand::Plan { source, json },
            } => {
                assert_eq!(
                    source.file.as_deref(),
                    Some(std::path::Path::new("plan.md"))
                );
                assert!(!source.stdin);
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_review_plan_stdin_json() {
        let cli = Cli::try_parse_from(["nisse", "review", "plan", "--stdin", "--json"]).unwrap();
        match cli.command {
            Command::Review {
                review: ReviewCommand::Plan { source, json },
            } => {
                assert!(source.stdin);
                assert!(source.file.is_none());
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn review_plan_requires_exactly_one_source() {
        assert!(Cli::try_parse_from(["nisse", "review", "plan"]).is_err());
        assert!(
            Cli::try_parse_from(["nisse", "review", "plan", "--stdin", "--file", "p.md"]).is_err()
        );
    }

    #[test]
    fn parses_every_review_code_mode() {
        let working =
            Cli::try_parse_from(["nisse", "review", "code", "--working", "--json"]).unwrap();
        match working.command {
            Command::Review {
                review: ReviewCommand::Code { scope, json },
            } => {
                assert!(scope.working);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let staged = Cli::try_parse_from(["nisse", "review", "code", "--staged"]).unwrap();
        match staged.command {
            Command::Review {
                review: ReviewCommand::Code { scope, .. },
            } => assert!(scope.staged),
            other => panic!("unexpected command: {other:?}"),
        }

        let commit =
            Cli::try_parse_from(["nisse", "review", "code", "--commit", "abc123"]).unwrap();
        match commit.command {
            Command::Review {
                review: ReviewCommand::Code { scope, .. },
            } => {
                assert_eq!(scope.commit.as_deref(), Some("abc123"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let range = Cli::try_parse_from(["nisse", "review", "code", "--range", "v1..v2"]).unwrap();
        match range.command {
            Command::Review {
                review: ReviewCommand::Code { scope, .. },
            } => {
                assert_eq!(scope.range.as_deref(), Some("v1..v2"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let branch = Cli::try_parse_from(["nisse", "review", "code", "--branch", "main"]).unwrap();
        match branch.command {
            Command::Review {
                review: ReviewCommand::Code { scope, .. },
            } => {
                assert_eq!(scope.branch.as_deref(), Some("main"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn review_code_requires_exactly_one_mode() {
        assert!(Cli::try_parse_from(["nisse", "review", "code"]).is_err());
        assert!(Cli::try_parse_from(["nisse", "review", "code", "--working", "--staged"]).is_err());
        assert!(
            Cli::try_parse_from(["nisse", "review", "code", "--working", "--commit", "abc"])
                .is_err()
        );
    }

    #[test]
    fn parses_query() {
        let cli = Cli::try_parse_from(["nisse", "query", "queue boundaries"]).unwrap();
        match cli.command {
            Command::Query { query, json } => {
                assert_eq!(query, "queue boundaries");
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["nisse", "query", "x", "--json"]).unwrap();
        match cli.command {
            Command::Query { json, .. } => assert!(json),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_feedback_each_outcome() {
        for flag in ["--fixed", "--accepted", "--dismissed"] {
            let cli = Cli::try_parse_from(["nisse", "feedback", "r1", "f1", flag]).unwrap();
            match cli.command {
                Command::Feedback {
                    review_id,
                    finding_id,
                    outcome,
                } => {
                    assert_eq!(review_id, "r1");
                    assert_eq!(finding_id, "f1");
                    assert_eq!(
                        (outcome.fixed, outcome.accepted, outcome.dismissed),
                        (
                            flag == "--fixed",
                            flag == "--accepted",
                            flag == "--dismissed"
                        )
                    );
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }
    }

    #[test]
    fn feedback_requires_exactly_one_outcome() {
        assert!(Cli::try_parse_from(["nisse", "feedback", "r1", "f1"]).is_err());
        assert!(
            Cli::try_parse_from(["nisse", "feedback", "r1", "f1", "--fixed", "--dismissed"])
                .is_err()
        );
    }

    #[test]
    fn parses_subcommand_families() {
        for (args, name) in [
            (vec!["nisse", "records", "list"], "records list"),
            (vec!["nisse", "records", "pending"], "records pending"),
            (vec!["nisse", "records", "compact"], "records compact"),
            (vec!["nisse", "cache", "status"], "cache status"),
            (vec!["nisse", "cache", "clear"], "cache clear"),
            (vec!["nisse", "eval", "routing"], "eval routing"),
            (vec!["nisse", "eval", "retrieval"], "eval retrieval"),
        ] {
            let cli = Cli::try_parse_from(&args).unwrap_or_else(|e| {
                panic!("failed to parse {name}: {e}");
            });
            let actual = match cli.command {
                Command::Records {
                    records: RecordsCommand::List,
                } => "records list",
                Command::Records {
                    records: RecordsCommand::Pending,
                } => "records pending",
                Command::Records {
                    records: RecordsCommand::Compact,
                } => "records compact",
                Command::Cache {
                    cache: CacheCommand::Status,
                } => "cache status",
                Command::Cache {
                    cache: CacheCommand::Clear,
                } => "cache clear",
                Command::Eval {
                    eval: EvalCommand::Routing,
                } => "eval routing",
                Command::Eval {
                    eval: EvalCommand::Retrieval,
                } => "eval retrieval",
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(actual, name);
        }

        let mcp = Cli::try_parse_from(["nisse", "mcp"]).unwrap();
        assert!(matches!(mcp.command, Command::Mcp));

        let doctor = Cli::try_parse_from(["nisse", "doctor"]).unwrap();
        assert!(matches!(doctor.command, Command::Doctor));
    }
}
