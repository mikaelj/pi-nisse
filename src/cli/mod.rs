//! Frozen CLI contract.
//!
//! The complete command surface of Nisse 1.0 is defined here. Commands whose
//! implementation belongs to later milestones parse successfully but dispatch
//! to typed `NotImplemented` errors.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::domain::records::RecordKind;

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

    /// Review investigation results (PLAN_INVESTIGATION.md). Each input
    /// accepts literal text, an explicit `@file` reference, or a bare
    /// value naming an existing file.
    Investigation {
        /// What was supposed to be investigated (task).
        #[arg(long)]
        task: String,

        /// The investigation output to review (result).
        #[arg(long)]
        result: String,

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

/// Feedback outcome: exactly one of `--fixed`, `--accepted`,
/// `--dismissed` (enforced by the required, single-choice group).
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

impl FeedbackOutcome {
    /// The frozen domain outcome of the parsed flags (plan section 49).
    /// The CLI guarantees exactly one flag; `None` is unreachable for
    /// parsed commands and callers reject it defensively.
    pub fn outcome(&self) -> Option<crate::domain::records::FeedbackOutcome> {
        match (self.fixed, self.accepted, self.dismissed) {
            (true, false, false) => Some(crate::domain::records::FeedbackOutcome::Fixed),
            (false, true, false) => Some(crate::domain::records::FeedbackOutcome::Accepted),
            (false, false, true) => Some(crate::domain::records::FeedbackOutcome::Dismissed),
            _ => None,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum RecordsCommand {
    /// List durable project records, optionally filtered by --kind.
    List {
        /// Only list records of these kinds. Repeatable; lists every kind
        /// when omitted.
        #[arg(long = "kind", value_name = "KIND")]
        kind: Vec<RecordKindArg>,

        /// Print record header lines only (greppable, pipeable).
        #[arg(long = "oneline")]
        oneline: bool,

        /// Cap how many records each section shows; counts always report
        /// true totals, with truncation noted.
        #[arg(long = "limit", value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
        limit: Option<u64>,

        /// Emit machine-readable JSON (the format scripts must use;
        /// honors --kind, ignores --oneline/--limit).
        #[arg(long = "json")]
        json: bool,
    },

    /// List learning outcomes that did not become durable knowledge.
    ///
    /// Shows pending or rejected fact candidates, consent-deferred
    /// feedback, and pending lesson maintenance, each with the reason it
    /// did not persist. This state lives in the regenerable local cache:
    /// later runs resolve it (a verified candidate becomes a durable
    /// fact; granted consent flushes deferred feedback) or `nisse cache
    /// clear` discards it.
    Pending,

    /// Consolidate compatible durable records into canonical common ones.
    Compact,
}

/// `--kind` values accepted by `nisse records list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RecordKindArg {
    Observation,
    Fact,
    Lesson,
}

impl RecordKindArg {
    /// The domain kind this CLI value selects.
    pub fn record_kind(self) -> RecordKind {
        match self {
            RecordKindArg::Observation => RecordKind::Observation,
            RecordKindArg::Fact => RecordKind::Fact,
            RecordKindArg::Lesson => RecordKind::Lesson,
        }
    }
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
    fn parses_review_investigation() {
        let cli = Cli::try_parse_from([
            "nisse",
            "review",
            "investigation",
            "--task",
            "@task.md",
            "--result",
            "the findings",
        ])
        .unwrap();
        match cli.command {
            Command::Review {
                review: ReviewCommand::Investigation { task, result, json },
            } => {
                assert_eq!(task, "@task.md");
                assert_eq!(result, "the findings");
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn review_investigation_requires_both_inputs() {
        assert!(Cli::try_parse_from(["nisse", "review", "investigation"]).is_err());
        assert!(Cli::try_parse_from(["nisse", "review", "investigation", "--task", "t"]).is_err());
        assert!(
            Cli::try_parse_from(["nisse", "review", "investigation", "--result", "r"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "nisse",
                "review",
                "investigation",
                "--task",
                "t",
                "--result",
                "r",
                "--json"
            ])
            .is_ok()
        );
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
    fn feedback_flags_map_to_the_frozen_domain_outcomes() {
        let cases = [
            (vec!["--fixed"], "fixed"),
            (vec!["--accepted"], "accepted"),
            (vec!["--dismissed"], "dismissed"),
        ];
        for (flag, name) in cases {
            let cli = Cli::try_parse_from(["nisse", "feedback", "r1", "f1", flag[0]]).unwrap();
            match cli.command {
                Command::Feedback { outcome, .. } => {
                    assert_eq!(outcome.outcome().unwrap().as_str(), name);
                }
                other => panic!("unexpected command: {other:?}"),
            }
        }

        // Defensive: an all-false flag set has no outcome.
        assert!(
            crate::cli::FeedbackOutcome {
                fixed: false,
                accepted: false,
                dismissed: false
            }
            .outcome()
            .is_none()
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
                    records: RecordsCommand::List { .. },
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
    }

    #[test]
    fn parses_records_list_flags() {
        let unfiltered = Cli::try_parse_from(["nisse", "records", "list"]).unwrap();
        match unfiltered.command {
            Command::Records {
                records:
                    RecordsCommand::List {
                        kind,
                        oneline,
                        limit,
                        json,
                    },
            } => {
                assert!(kind.is_empty());
                assert!(!oneline);
                assert_eq!(limit, None);
                assert!(!json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let filtered = Cli::try_parse_from(["nisse", "records", "list", "--kind", "fact"]).unwrap();
        match filtered.command {
            Command::Records {
                records: RecordsCommand::List { kind, .. },
            } => {
                assert_eq!(kind, vec![RecordKindArg::Fact]);
                assert_eq!(kind[0].record_kind(), RecordKind::Fact);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let repeated = Cli::try_parse_from([
            "nisse", "records", "list", "--kind", "fact", "--kind", "lesson",
        ])
        .unwrap();
        match repeated.command {
            Command::Records {
                records: RecordsCommand::List { kind, .. },
            } => {
                assert_eq!(
                    kind.iter().map(|k| k.record_kind()).collect::<Vec<_>>(),
                    vec![RecordKind::Fact, RecordKind::Lesson]
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let everything = Cli::try_parse_from([
            "nisse",
            "records",
            "list",
            "--oneline",
            "--limit",
            "20",
            "--json",
        ])
        .unwrap();
        match everything.command {
            Command::Records {
                records:
                    RecordsCommand::List {
                        oneline,
                        limit,
                        json,
                        ..
                    },
            } => {
                assert!(oneline);
                assert_eq!(limit, Some(20));
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // The limit is a positive count; zero and negatives are parse
        // errors.
        assert!(Cli::try_parse_from(["nisse", "records", "list", "--limit", "0"]).is_err());
        assert!(Cli::try_parse_from(["nisse", "records", "list", "--limit", "-3"]).is_err());
        assert!(Cli::try_parse_from(["nisse", "records", "list", "--kind", "gospel"]).is_err());
    }

    #[test]
    fn parses_doctor_command() {
        let doctor = Cli::try_parse_from(["nisse", "doctor"]).unwrap();
        assert!(matches!(doctor.command, Command::Doctor));
    }

    #[test]
    fn command_surface_is_frozen_and_has_no_task_mode() {
        use clap::CommandFactory;

        // Nisse Core is operation-based (plan sections 1, 3, 26, 71): its
        // CLI surface is exactly the frozen §61 set and must never grow a
        // Plan/Build task-mode or workflow-state command. That policy
        // belongs to the Main Pi skill (milestone 25).
        let mut app = Cli::command();
        let mut names: Vec<String> = app
            .get_subcommands()
            .map(|command| command.get_name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "cache", "doctor", "eval", "feedback", "mcp", "query", "records", "review"
            ]
        );

        let review = app
            .find_subcommand_mut("review")
            .expect("review subcommand");
        let mut review_names: Vec<String> = review
            .get_subcommands()
            .map(|command| command.get_name().to_string())
            .collect();
        review_names.sort();
        // `review plan` reviews a plan document; it is not a Plan mode.
        // `review investigation` reviews investigation results
        // (PLAN_INVESTIGATION.md; §61 amendment).
        assert_eq!(review_names, vec!["code", "investigation", "plan"]);

        for forbidden in ["plan-mode", "build", "task", "mode"] {
            assert!(
                !names.contains(&forbidden.to_string()),
                "task-mode command `{forbidden}` must not exist"
            );
            assert!(!review_names.contains(&forbidden.to_string()));
        }
    }
}
