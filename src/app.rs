//! Application root.
//!
//! The `App` owns command dispatch. Deterministic decisions live in the Rust
//! core; future milestones attach real behavior to the frozen command set.

use thiserror::Error;

use crate::cli::Command;
use crate::jury::review::MAX_PLAN_BYTES;

/// Typed application errors.
#[derive(Debug, Error)]
pub enum AppError {
    /// A frozen command whose implementation belongs to a later
    /// milestone. Since milestone 26 completed the frozen CLI surface
    /// (plan section 61) no command produces this error; it stays part
    /// of the typed error surface for boundary callers.
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

    /// `nisse eval <kind>` found failing fixtures in the mandatory
    /// baseline; the report is printed to stdout.
    #[error("eval {0} found failing baseline fixtures; see the report on stdout")]
    EvalFailed(&'static str),

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
                crate::cli::ReviewCommand::Plan { source, json } => {
                    self.run_review_plan(source, json)
                }
                crate::cli::ReviewCommand::Code { scope, json } => {
                    self.run_review_code(scope, json)
                }
                crate::cli::ReviewCommand::Investigation { task, result, json } => {
                    self.run_review_investigation(task, result, json)
                }
            },
            Command::Query { query, json } => self.run_query(query, json),
            Command::Feedback {
                review_id,
                finding_id,
                outcome,
            } => self.run_feedback(review_id, finding_id, outcome),
            Command::Records { records } => match records {
                crate::cli::RecordsCommand::List => self.run_records_list(),
                crate::cli::RecordsCommand::Pending => self.run_records_pending(),
                crate::cli::RecordsCommand::Compact => self.run_records_compact(),
            },
            Command::Cache { cache } => match cache {
                crate::cli::CacheCommand::Status => self.run_cache_status(),
                crate::cli::CacheCommand::Clear => self.run_cache_clear(),
            },
            Command::Eval { eval } => match eval {
                crate::cli::EvalCommand::Routing => self.run_eval_routing(),
                crate::cli::EvalCommand::Retrieval => self.run_eval_retrieval(),
            },
            // The MCP stdio server is a long-running operation: it owns
            // the process stdio until the client disconnects.
            Command::Mcp => crate::mcp::run_server().map_err(AppError::from),
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

    /// Run `nisse review plan` (plan sections 58 and 59).
    ///
    /// Thin CLI wrapper around [`App::plan_review_operation`]: read the
    /// plan input, run the shared operation, and print the result (text,
    /// or the strict `ReviewResult` JSON with `--json`).
    fn run_review_plan(&self, source: crate::cli::PlanSource, json: bool) -> Result<(), AppError> {
        use crate::jury::review::render_review_text;

        let (text, file) = read_plan_source(&source)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review plan failed: {err}")))?;

        // The plan file participates in the working snapshot when it
        // lives inside the worktree (untracked non-ignored content is
        // part of the target); compute its repository-relative path so
        // evidence can cite it.
        let plan_path = file
            .as_deref()
            .and_then(|file| self.plan_path_in_repo(file));
        let plan = crate::jury::review::PlanInput {
            text,
            path: plan_path,
        };

        let result = self.plan_review_operation(plan)?;

        let rendered = if json {
            serde_json::to_string(&result)
                .map_err(|err| AppError::Other(anyhow::anyhow!("review plan failed: {err}")))?
        } else {
            render_review_text(&result)
        };
        print_to_stdout(&rendered);
        Ok(())
    }

    /// Run `nisse review code` (plan sections 58 and 60).
    ///
    /// Thin CLI wrapper around [`App::code_review_operation`]: map the
    /// CLI scope flags to the snapshot specification, run the shared
    /// operation, and print the result (text, or the strict
    /// `ReviewResult` JSON with `--json`).
    fn run_review_code(&self, scope: crate::cli::CodeScope, json: bool) -> Result<(), AppError> {
        use crate::jury::review::render_review_text;

        let spec = code_scope_spec(&scope)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review code failed: {err}")))?;

        let result = self.code_review_operation(spec)?;

        let rendered = if json {
            serde_json::to_string(&result)
                .map_err(|err| AppError::Other(anyhow::anyhow!("review code failed: {err}")))?
        } else {
            render_review_text(&result)
        };
        print_to_stdout(&rendered);
        Ok(())
    }

    /// The complete plan-review operation shared by the CLI and the
    /// MCP adapter (plan section 67): execute the plan-review pipeline
    /// against the current working-tree snapshot and return the strict
    /// `ReviewResult`. The local review event is persisted in the
    /// shared cache. Optional providers (plan section 34) improve
    /// routing and retrieval when configured; every provider failure
    /// degrades to the deterministic behavior, so reviews never
    /// require Needle, embeddings, or network access in the
    /// deterministic runtimes.
    pub(crate) fn plan_review_operation(
        &self,
        plan: crate::jury::review::PlanInput,
    ) -> Result<crate::domain::review::ReviewResult, AppError> {
        use crate::git::{SnapshotSpec, resolve_snapshot};
        use crate::jury::review::{PlanReviewOptions, run_plan_review};

        let (ctx, paths) = self.discover_storage()?;
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).map_err(|err| {
            AppError::Other(anyhow::anyhow!(
                "review plan failed to resolve the working-tree snapshot: {err}"
            ))
        })?;

        let environment = self.review_environment(&ctx, "review plan")?;
        let mut options = PlanReviewOptions::new(environment.launch);
        options.model_profiles = environment.model_profiles;
        options.contributor_id = environment.contributor_id;
        options.routing_provider = environment.routing_provider;
        options.embedding_provider = environment.embedding_provider;
        options.max_concurrent_reviewers = environment.max_concurrent_reviewers;

        let outcome = run_plan_review(&ctx, &resolved, &paths, &plan, &options)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review plan failed: {err}")))?;
        Ok(outcome.result)
    }

    /// The complete code-review operation shared by the CLI and the
    /// MCP adapter (plan section 67): execute the code-review pipeline
    /// for the given snapshot mode and return the strict
    /// `ReviewResult`. The local review event is persisted in the
    /// shared cache. Provider semantics are those of
    /// [`App::plan_review_operation`].
    pub(crate) fn code_review_operation(
        &self,
        spec: crate::git::SnapshotSpec,
    ) -> Result<crate::domain::review::ReviewResult, AppError> {
        use crate::git::resolve_snapshot;
        use crate::jury::review::{CodeReviewOptions, run_code_review};

        let (ctx, paths) = self.discover_storage()?;
        let resolved = resolve_snapshot(&ctx, &spec).map_err(|err| {
            AppError::Other(anyhow::anyhow!(
                "review code failed to resolve the snapshot: {err}"
            ))
        })?;

        let environment = self.review_environment(&ctx, "review code")?;
        let mut options = CodeReviewOptions::new(environment.launch);
        options.model_profiles = environment.model_profiles;
        options.contributor_id = environment.contributor_id;
        options.routing_provider = environment.routing_provider;
        options.embedding_provider = environment.embedding_provider;
        options.max_concurrent_reviewers = environment.max_concurrent_reviewers;

        let outcome = run_code_review(&ctx, &resolved, &paths, &options)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review code failed: {err}")))?;
        Ok(outcome.result)
    }

    /// The trusted review environment shared by both review
    /// operations: host launch selection, model profiles, contributor
    /// identity, and the optional classifier/embedding providers
    /// (plan sections 10 and 34). Reviewer sessions never inherit a
    /// model; the optional providers come from trusted local
    /// configuration.
    fn review_environment(
        &self,
        ctx: &crate::git::RepoContext,
        operation: &str,
    ) -> Result<ReviewEnvironment, AppError> {
        use crate::config;
        use crate::jury::review::host_launch_from_env;

        let user_path = config::paths::user_config_paths().load;
        let repo_path = config::paths::repo_config_path(&ctx.git_common_dir);
        let loaded =
            config::load_config(user_path.as_deref(), Some(&repo_path)).map_err(|err| {
                AppError::Other(anyhow::anyhow!(
                    "{operation} failed to load configuration: {err}"
                ))
            })?;
        let providers = optional_providers(&loaded.config);
        Ok(ReviewEnvironment {
            launch: host_launch_from_env()
                .map_err(|err| AppError::Other(anyhow::anyhow!("{operation} failed: {err}")))?,
            model_profiles: loaded.config.model_profiles.clone(),
            contributor_id: loaded
                .config
                .contributor
                .as_ref()
                .map(|contributor| contributor.id.clone()),
            routing_provider: providers.routing,
            embedding_provider: providers.embeddings,
            max_concurrent_reviewers: loaded.config.performance.max_concurrent_reviewers as u32,
        })
    }

    /// Repository-relative form of a plan file inside the worktree,
    /// or `None` (a failed discovery is surfaced by the operation that
    /// follows).
    fn plan_path_in_repo(&self, file: &std::path::Path) -> Option<String> {
        let (ctx, _) = self.discover_storage().ok()?;
        repo_relative(&ctx, file)
    }

    /// Run `nisse query`.
    ///
    /// Thin CLI wrapper around [`App::query_operation`]: run the
    /// shared retrieval pipeline and print the result (text, or the
    /// strict report JSON with `--json`).
    fn run_query(&self, query: String, json: bool) -> Result<(), AppError> {
        use crate::retrieval::{render_json, render_text};

        let outcome = self.query_operation(query)?;
        let rendered = if json {
            render_json(&outcome.query, &outcome.snapshot, &outcome.bundle)
                .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?
        } else {
            render_text(&outcome.query, &outcome.snapshot, &outcome.bundle)
        };
        print_to_stdout(&rendered);
        Ok(())
    }

    /// The complete project-query operation shared by the CLI and the
    /// MCP adapter (plan section 67): lazily refresh the source index
    /// of the current working-tree snapshot, load the durable records
    /// and built-in practices, mark potentially stale facts (plan
    /// section 48), and run the deterministic retrieval. An optional
    /// embedding provider (plan section 34) reranks the bounded
    /// candidate set; provider failures degrade to the baseline
    /// ranking. No LLM is involved.
    pub(crate) fn query_operation(&self, query: String) -> Result<QueryOutcome, AppError> {
        use crate::git::{SnapshotSpec, resolve_snapshot};
        use crate::index::refresh_index;
        use crate::jury::catalog::Catalog;
        use crate::retrieval::{RetrievalQuery, Retriever};
        use crate::storage::records::RecordStore;

        let (ctx, paths) = self.discover_storage()?;
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).map_err(|err| {
            AppError::Other(anyhow::anyhow!(
                "failed to resolve the working-tree snapshot: {err}"
            ))
        })?;
        refresh_index(&ctx, &resolved, &paths)
            .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;

        let records = RecordStore::new(&ctx.worktree_root)
            .load()
            .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;
        let catalog = Catalog::bundled()
            .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;

        let retriever = Retriever::open(&resolved, &paths)
            .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;
        // Optional embedding provider from trusted local
        // configuration; absent or failing providers change nothing.
        let user_path = crate::config::paths::user_config_paths().load;
        let repo_path = crate::config::paths::repo_config_path(&ctx.git_common_dir);
        let loaded =
            crate::config::load_config(user_path.as_deref(), Some(&repo_path)).map_err(|err| {
                AppError::Other(anyhow::anyhow!("query failed to load configuration: {err}"))
            })?;
        let retriever = match optional_providers(&loaded.config).embeddings {
            Some(provider) => retriever.with_embedding_provider(provider),
            None => retriever,
        };
        // Potentially stale facts (plan section 48) are marked and
        // downranked: current source evidence outranks stale memory.
        let stale = {
            use crate::learning::staleness::assess_records;
            let reader = crate::git::SnapshotReader::new(&ctx, &resolved)
                .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;
            assess_records(
                &records,
                &reader,
                ctx.object_format()
                    .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?,
            )
            .ids()
        };
        retriever
            .sync_records(&records, &stale)
            .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;
        retriever
            .sync_practices(&catalog)
            .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;

        let retrieval_query = RetrievalQuery::from_text(query.clone());
        let bundle = retriever
            .lookup(&retrieval_query)
            .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?;

        Ok(QueryOutcome {
            query,
            snapshot: resolved.descriptor,
            bundle,
        })
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

    /// Run `nisse records pending`.
    ///
    /// Lists the local pending-learning state of the shared cache:
    /// fact candidates that were not persisted as durable facts —
    /// pending ones (insufficient verification, verifier failure,
    /// missing contributor identity) and rejected ones (deterministic
    /// rejection or contradiction) — plus any other pending-learning
    /// kinds later milestones add. Pending state is regenerable local
    /// cache, never durable project knowledge.
    fn run_records_pending(&self) -> Result<(), AppError> {
        let (_ctx, paths) = self.discover_storage()?;
        let rows = match crate::learning::facts::load_pending(&paths) {
            Ok(rows) => rows,
            // Read-only listing: when the pending store's directory does
            // not exist yet (and creation is not consented), the answer
            // is simply "nothing pending" — never a consent demand for
            // a directory the command does not need in order to report
            // an empty list. load_pending flattens errors to strings, so
            // the shared marker predicate is used.
            Err(err) if crate::config::consent::is_consent_error(&err) => Vec::new(),
            Err(err) => {
                return Err(AppError::Other(anyhow::anyhow!(
                    "records pending failed: {err}"
                )));
            }
        };
        print_to_stdout(&crate::learning::facts::render_pending(&rows));
        Ok(())
    }

    /// Run `nisse review investigation` (PLAN_INVESTIGATION.md): thin CLI
    /// wrapper around [`App::investigation_operation`].
    fn run_review_investigation(
        &self,
        task: String,
        result: String,
        json: bool,
    ) -> Result<(), AppError> {
        let rendered = self.investigation_operation(task, result)?;
        if json {
            print_to_stdout(
                &serde_json::to_string(&rendered)
                    .map_err(|err| AppError::Other(anyhow::anyhow!("{err}")))?,
            );
        } else {
            print_to_stdout(&crate::jury::investigation::render_investigation_text(
                &rendered,
            ));
        }
        Ok(())
    }

    /// The complete investigation review operation (PLAN_INVESTIGATION.md).
    pub(crate) fn investigation_operation(
        &self,
        task: String,
        result: String,
    ) -> Result<crate::domain::investigation::InvestigationResult, AppError> {
        use crate::domain::investigation::resolve_input_value;
        use crate::jury::investigation::{self as pipeline, InvestigationInputs};

        let cwd = std::env::current_dir()
            .map_err(|err| anyhow::anyhow!("failed to determine the current directory: {err}"))?;
        let (ctx, paths) = self.discover_storage()?;
        let worktree_root = ctx.worktree_root.clone();
        let environment = self.review_environment(&ctx, "review investigation")?;

        let task_input = resolve_input_value(&task, "task", Some(&worktree_root), &cwd)
            .map_err(|err| AppError::Other(anyhow::anyhow!("{err}")))?;
        let result_input = resolve_input_value(&result, "result", Some(&worktree_root), &cwd)
            .map_err(|err| AppError::Other(anyhow::anyhow!("{err}")))?;
        let resolved = crate::git::resolve_snapshot(&ctx, &crate::git::SnapshotSpec::Working)
            .map_err(|err| {
                AppError::Other(anyhow::anyhow!(
                    "investigation review failed to resolve the working-tree snapshot: {err}"
                ))
            })?;
        let options = pipeline::InvestigationReviewOptions {
            launch: environment.launch,
            model_profiles: environment.model_profiles,
            routing_provider: environment.routing_provider,
            embedding_provider: environment.embedding_provider,
            contributor_id: environment.contributor_id,
            max_concurrent_reviewers: environment.max_concurrent_reviewers,
        };
        pipeline::run_investigation_review(
            &ctx,
            &resolved,
            &paths,
            &InvestigationInputs {
                task: task_input,
                result: result_input,
            },
            &options,
        )
        .map_err(|err| AppError::Other(anyhow::anyhow!("investigation review failed: {err}")))
    }

    /// Run `nisse feedback` (plan sections 49 and 50).
    ///
    /// Thin CLI wrapper around [`App::feedback_operation`]: record a
    /// known outcome for one finding of a stored review and print the
    /// confirmation. Rust constructs every authoritative observation
    /// field from the stored review, the stored finding, the explicit
    /// outcome, and the review's snapshot descriptors; LLMs construct
    /// nothing.
    fn run_feedback(
        &self,
        review_id: String,
        finding_id: String,
        outcome: crate::cli::FeedbackOutcome,
    ) -> Result<(), AppError> {
        use crate::learning::feedback;

        let Some(outcome) = outcome.outcome() else {
            return Err(AppError::Other(anyhow::anyhow!(
                "exactly one feedback outcome flag is required"
            )));
        };

        let recording = self.feedback_operation(&review_id, &finding_id, outcome)?;
        print_to_stdout(&feedback::render_feedback(
            &review_id,
            &finding_id,
            outcome,
            &recording,
        ));
        Ok(())
    }

    /// The feedback operation shared by the CLI and the MCP adapter
    /// (plan section 67): record a known outcome for one finding of a
    /// stored review as immutable durable observations. Submission is
    /// idempotent, outcomes are immutable, and without a contributor
    /// identity the outcome remains pending locally (plan section 44)
    /// while the operation fails explicitly.
    pub(crate) fn feedback_operation(
        &self,
        review_id: &str,
        finding_id: &str,
        outcome: crate::domain::records::FeedbackOutcome,
    ) -> Result<crate::learning::feedback::FeedbackRecording, AppError> {
        use crate::learning::feedback;

        let (ctx, paths) = self.discover_storage()?;

        // Contributor identity lives in trusted local configuration;
        // feedback is noninteractive and never prompts.
        let user_path = crate::config::paths::user_config_paths().load;
        let repo_path = crate::config::paths::repo_config_path(&ctx.git_common_dir);
        let loaded =
            crate::config::load_config(user_path.as_deref(), Some(&repo_path)).map_err(|err| {
                AppError::Other(anyhow::anyhow!(
                    "feedback failed to load configuration: {err}"
                ))
            })?;
        let contributor = loaded
            .config
            .contributor
            .as_ref()
            .map(|contributor| contributor.id.clone());

        feedback::record_feedback(
            &ctx,
            &paths,
            review_id,
            finding_id,
            outcome,
            contributor.as_deref(),
        )
        .map_err(|err| AppError::Other(anyhow::anyhow!("feedback failed: {err}")))
    }

    /// Run `nisse records compact` (plan sections 52–56).
    ///
    /// Consolidates the durable records of the current worktree into
    /// `.nisse/common/`: deterministic exact-duplicate facts and
    /// compatible lessons merge deterministically (the duplicates
    /// still independently verified; permanent invariant 10),
    /// compatible facts merge through fresh Pi synthesis plus a
    /// separate fresh verifier, and conflicts are preserved — resolved
    /// only when the current evidence clearly supports exactly one
    /// claim. Originals always remain stored for provenance; only the
    /// active set compacts. Creating durable records requires a
    /// contributor identity: interactive use may prompt for one
    /// (plan section 44), noninteractive use fails explicitly —
    /// nothing is written on uncertainty.
    fn run_records_compact(&self) -> Result<(), AppError> {
        use crate::git::{SnapshotReader, SnapshotSpec, resolve_snapshot};
        use crate::learning::consolidation::{self, ConsolidationOptions, run_consolidation};
        use crate::storage::records::RecordStore;

        let (ctx, _paths) = self.discover_storage()?;

        // Trusted local configuration supplies the contributor
        // identity and the model profiles the semantic sessions may
        // use (reviewer sessions never inherit a model).
        let user_path = crate::config::paths::user_config_paths().load;
        let repo_path = crate::config::paths::repo_config_path(&ctx.git_common_dir);
        let loaded =
            crate::config::load_config(user_path.as_deref(), Some(&repo_path)).map_err(|err| {
                AppError::Other(anyhow::anyhow!(
                    "records compact failed to load configuration: {err}"
                ))
            })?;

        let store = RecordStore::new(&ctx.worktree_root);
        let records = store
            .load()
            .map_err(|err| AppError::Other(anyhow::anyhow!("records compact failed: {err}")))?;

        // Contributor identity: interactive use prompts on first use
        // (plan section 44); noninteractive use fails explicitly —
        // consolidation output is re-derivable at any time, so nothing
        // is lost by refusing to guess an author.
        let author = match loaded.config.contributor.as_ref() {
            Some(contributor) => contributor.id.clone(),
            None => {
                let existing_contributors: Vec<String> = records
                    .iter()
                    .map(|stored| stored.record.author().to_string())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
                let hint = crate::config::identity::suggest_identity(&existing_contributors, None);
                // Loading honors the legacy fallback, but an identity
                // save always targets the dotfile path (which seeds
                // itself from the legacy file on first write).
                let save_path = crate::config::paths::user_config_paths().save;
                let outcome = crate::config::identity::resolve_or_prompt(
                    None,
                    save_path.as_deref(),
                    interactive,
                    &mut std::io::stdin().lock(),
                    &mut std::io::stdout().lock(),
                    hint.as_deref(),
                )
                .map_err(|err| {
                    AppError::Other(anyhow::anyhow!(
                        "records compact failed to resolve the contributor identity: {err}"
                    ))
                })?;
                match outcome.identity() {
                    Some(identity) => identity.id.clone(),
                    None => {
                        return Err(AppError::Other(anyhow::anyhow!(
                            "records compact creates durable records and needs a contributor \
                             identity; nothing was written. Configure `[contributor] id` in \
                             the user config (plan section 44) and run it again"
                        )));
                    }
                }
            }
        };

        // Plan first: consolidation work that needs no semantic
        // sessions (in particular lesson merging) never spawns the
        // host and runs fully offline. The snapshot reader is needed
        // only by fact work (conflict handling rebuilds current
        // evidence), but building it unconditionally keeps the flow
        // simple; it is cheap and side-effect free.
        let plan = consolidation::plan_consolidation(&records);
        let options = ConsolidationOptions::default();
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).map_err(|err| {
            AppError::Other(anyhow::anyhow!(
                "records compact failed to resolve the working-tree snapshot: {err}"
            ))
        })?;
        let reader = SnapshotReader::new(&ctx, &resolved).map_err(|err| {
            AppError::Other(anyhow::anyhow!(
                "records compact failed to read the working-tree snapshot: {err}"
            ))
        })?;
        let format = ctx
            .object_format()
            .map_err(|err| AppError::Other(anyhow::anyhow!("records compact failed: {err}")))?;

        let summary = if plan.needs_host() {
            let launch = crate::jury::review::host_launch_from_env()
                .map_err(|err| AppError::Other(anyhow::anyhow!("records compact failed: {err}")))?;
            let mut host = crate::jury::host::HostProcess::spawn(&launch)
                .map_err(|err| AppError::Other(anyhow::anyhow!("records compact failed: {err}")))?;
            host.wait_ready(crate::jury::review::HOST_READY_TIMEOUT)
                .map_err(|err| AppError::Other(anyhow::anyhow!("records compact failed: {err}")))?;
            let host_config = crate::jury::protocol::HostConfigPayload::from_config_profiles(
                &loaded.config.model_profiles,
            );
            host.send_host_config(&host_config)
                .map_err(|err| AppError::Other(anyhow::anyhow!("records compact failed: {err}")))?;
            let summary = run_consolidation(
                Some(&mut host),
                &ctx,
                &reader,
                format,
                &plan,
                &author,
                &options,
            );
            let _ = host.shutdown(std::time::Duration::from_secs(5));
            summary
        } else {
            run_consolidation(None, &ctx, &reader, format, &plan, &author, &options)
        };

        print_to_stdout(&consolidation::render_compact_report(&summary));
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

    /// Run `nisse eval routing` (plan section 69, milestone 23).
    ///
    /// Runs the frozen routing fixtures through the real
    /// classification and jury construction pipeline and prints the
    /// frozen metrics with a machine-readable JSON report. The
    /// mandatory rules baseline always runs; a configured routing
    /// provider (plan section 34) adds a comparison variant. The
    /// command's exit status follows the mandatory baseline only —
    /// Needle is never mandatory (permanent invariant 15).
    fn run_eval_routing(&self) -> Result<(), AppError> {
        let providers = self.eval_providers()?;
        let variants = crate::routing::eval::run_routing_eval(
            providers.routing.as_ref().map(|provider| provider.as_ref()),
        )
        .map_err(|err| AppError::Other(anyhow::anyhow!("eval routing failed: {err}")))?;
        print_to_stdout(&crate::routing::eval::render_routing_report(&variants));

        match variants.first() {
            Some(baseline) if baseline.passed() => Ok(()),
            Some(_) => Err(AppError::EvalFailed("routing")),
            None => Err(AppError::Other(anyhow::anyhow!(
                "eval routing produced no variants"
            ))),
        }
    }

    /// Run `nisse eval retrieval` (plan section 69, milestone 23).
    ///
    /// Builds the frozen fixture repository (real temporary Git
    /// repositories; no network access), runs the frozen retrieval
    /// fixtures through the real refresh/membership/ranking pipeline,
    /// and prints the frozen metrics with a machine-readable JSON
    /// report. Snapshot leakage always fails a fixture. The mandatory
    /// baseline always runs; a configured embedding provider adds a
    /// comparison variant whose failures are reported but never fail
    /// the command (permanent invariant 16).
    fn run_eval_retrieval(&self) -> Result<(), AppError> {
        let providers = self.eval_providers()?;
        let variants = crate::retrieval::eval::run_retrieval_eval(providers.embeddings)
            .map_err(|err| AppError::Other(anyhow::anyhow!("eval retrieval failed: {err}")))?;
        print_to_stdout(&crate::retrieval::eval::render_retrieval_report(&variants));

        match variants.first() {
            Some(baseline) if baseline.passed() => Ok(()),
            Some(_) => Err(AppError::EvalFailed("retrieval")),
            None => Err(AppError::Other(anyhow::anyhow!(
                "eval retrieval produced no variants"
            ))),
        }
    }

    /// Load the optional eval comparison providers from trusted local
    /// configuration. Evaluation works from any directory; repository
    /// configuration participates when the current directory is inside
    /// a repository.
    fn eval_providers(&self) -> Result<OptionalProviders, AppError> {
        use crate::config;

        let cwd = std::env::current_dir()
            .map_err(|err| anyhow::anyhow!("failed to determine the current directory: {err}"))?;
        let repo_path = match crate::git::RepoContext::discover(&crate::git::GitRunner::new(), &cwd)
        {
            Ok(ctx) => Some(config::paths::repo_config_path(&ctx.git_common_dir)),
            Err(crate::git::GitError::NotARepository { .. }) => None,
            Err(other) => {
                return Err(AppError::Other(anyhow::anyhow!(
                    "repository discovery failed: {other}"
                )));
            }
        };
        let user_path = config::paths::user_config_paths().load;
        let loaded =
            config::load_config(user_path.as_deref(), repo_path.as_deref()).map_err(|err| {
                AppError::Other(anyhow::anyhow!(
                    "evaluation failed to load configuration: {err}"
                ))
            })?;
        Ok(optional_providers(&loaded.config))
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
        let user_path = config::paths::user_config_paths().load;
        let repo_path = git
            .git_common_dir
            .as_ref()
            .map(|common_dir| config::paths::repo_config_path(common_dir));
        let loaded = config::load_config(user_path.as_deref(), repo_path.as_deref());
        let resolver = SystemResolver;
        // Repository-derived directory locations for the `directories`
        // check (consent prompts fire only in interactive runs).
        let dirs = if git.in_repository {
            match crate::git::RepoContext::discover(&crate::git::GitRunner::new(), &cwd) {
                Ok(ctx) => config::doctor::DoctorDirs {
                    worktree_root: Some(ctx.worktree_root.clone()),
                    git_dir: Some(ctx.git_dir.clone()),
                    git_common_dir: Some(ctx.git_common_dir.clone()),
                    discovery_error: None,
                },
                Err(err) => config::doctor::DoctorDirs {
                    worktree_root: None,
                    git_dir: None,
                    git_common_dir: None,
                    discovery_error: Some(err.to_string()),
                },
            }
        } else {
            config::doctor::DoctorDirs::default()
        };
        let report = config::doctor::run_doctor(git, loaded, &resolver, &dirs);
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

/// The optional provider adapters of a configuration (plan section
/// 34): a routing classifier for the configured `[classifier]`
/// section and an embedding provider for `[embeddings]`.
///
/// Construction performs no I/O; endpoint trust is enforced per
/// request inside the adapters, and every failure degrades to the
/// deterministic behavior.
fn optional_providers(config: &crate::config::NisseConfig) -> OptionalProviders {
    OptionalProviders {
        routing: crate::providers::needle_adapter_from_config(
            config.classifier.provider,
            config.classifier.endpoint.as_deref(),
            config.network.allow_non_private_endpoints,
        )
        .map(|adapter| {
            std::sync::Arc::new(adapter)
                as std::sync::Arc<dyn crate::providers::RoutingClassifierProvider>
        }),
        embeddings: crate::providers::needle_adapter_from_config(
            config.embeddings.provider,
            config.embeddings.endpoint.as_deref(),
            config.network.allow_non_private_endpoints,
        )
        .map(|adapter| {
            std::sync::Arc::new(adapter) as std::sync::Arc<dyn crate::providers::EmbeddingProvider>
        }),
    }
}

/// The optional providers of one configuration.
struct OptionalProviders {
    /// Optional routing classifier (`[classifier]`).
    routing: Option<std::sync::Arc<dyn crate::providers::RoutingClassifierProvider>>,
    /// Optional embedding provider (`[embeddings]`).
    embeddings: Option<std::sync::Arc<dyn crate::providers::EmbeddingProvider>>,
}

/// The trusted review environment shared by the CLI and the MCP
/// adapter (see [`App::review_environment`]). (Not `Debug`: the
/// optional provider handles are opaque trait objects.)
struct ReviewEnvironment {
    /// How to launch the Pi review host child process.
    launch: crate::jury::host::PiHostLaunch,
    /// Model profiles the host may use, from trusted local
    /// configuration.
    model_profiles: std::collections::BTreeMap<String, crate::domain::model::ModelProfile>,
    /// Contributor identity for durable knowledge created by the
    /// review (plan section 44).
    contributor_id: Option<String>,
    /// Optional routing classifier blended into the rules decision.
    routing_provider: Option<std::sync::Arc<dyn crate::providers::RoutingClassifierProvider>>,
    /// Optional embedding provider reranking retrieval results.
    embedding_provider: Option<std::sync::Arc<dyn crate::providers::EmbeddingProvider>>,
    /// Bound on concurrently running reviewer sessions (plan milestone
    /// 29: reviewer concurrency obeys config).
    max_concurrent_reviewers: u32,
}

/// The result of the shared query operation: everything the text and
/// JSON renderers need.
pub(crate) struct QueryOutcome {
    /// The original query text.
    pub(crate) query: String,
    /// The working-tree snapshot the answer was retrieved from.
    pub(crate) snapshot: crate::domain::snapshot::SnapshotDescriptor,
    /// The bounded retrieval bundle.
    pub(crate) bundle: crate::domain::context::ContextBundle,
}

/// Read the plan input from `--file <path>` or `--stdin` (exactly one
/// is set; the CLI enforces it). The input is bounded to
/// [`MAX_PLAN_BYTES`](crate::jury::review::MAX_PLAN_BYTES).
fn read_plan_source(
    source: &crate::cli::PlanSource,
) -> Result<(String, Option<std::path::PathBuf>), String> {
    let bytes: Vec<u8> = if let Some(file) = &source.file {
        std::fs::read(file)
            .map_err(|err| format!("failed to read plan `{}`: {err}", file.display()))?
    } else {
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut buffer)
            .map_err(|err| format!("failed to read the plan from stdin: {err}"))?;
        buffer
    };
    if bytes.len() > MAX_PLAN_BYTES {
        return Err(format!("the plan exceeds {MAX_PLAN_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "the plan is not valid UTF-8".to_string())?;
    Ok((text, source.file.clone()))
}

/// Map the parsed code-review scope to its snapshot specification.
/// The CLI enforces that exactly one mode is set.
fn code_scope_spec(scope: &crate::cli::CodeScope) -> Result<crate::git::SnapshotSpec, String> {
    use crate::git::SnapshotSpec;
    if scope.working {
        Ok(SnapshotSpec::Working)
    } else if scope.staged {
        Ok(SnapshotSpec::Staged)
    } else if let Some(commit) = &scope.commit {
        Ok(SnapshotSpec::Commit {
            revision: commit.clone(),
        })
    } else if let Some(range) = &scope.range {
        Ok(SnapshotSpec::Range {
            spec: range.clone(),
        })
    } else if let Some(branch) = &scope.branch {
        Ok(SnapshotSpec::Branch {
            base_ref: branch.clone(),
        })
    } else {
        Err("exactly one code review mode is required".to_string())
    }
}

/// Repository-relative form of `path` when it lies inside the
/// worktree, using forward slashes; `None` otherwise.
fn repo_relative(ctx: &crate::git::RepoContext, path: &std::path::Path) -> Option<String> {
    let absolute = std::fs::canonicalize(path).ok()?;
    let root = std::fs::canonicalize(&ctx.worktree_root).ok()?;
    let relative = absolute.strip_prefix(&root).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
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
    // The complete frozen command surface (plan section 61) dispatches
    // to real implementations since milestone 26: `nisse mcp` was the
    // last remaining `NotImplemented` arm. `AppError::NotImplemented`
    // stays part of the typed error surface, but no frozen command
    // produces it; dispatch behavior is proven by the binary-level
    // integration tests (`tests/integration/`), including the MCP
    // server lifecycle in `mcp_binary.rs`.
}
