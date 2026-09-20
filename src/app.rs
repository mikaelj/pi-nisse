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

    /// Run `nisse review plan` (plan sections 58 and 59).
    ///
    /// Executes the complete plan-review pipeline against the current
    /// working-tree snapshot and prints the result (text, or the strict
    /// `ReviewResult` JSON with `--json`). The local review event is
    /// persisted in the shared cache. Optional providers (plan section
    /// 34) improve routing and retrieval when configured; every
    /// provider failure degrades to the deterministic behavior, so
    /// reviews never require Needle, embeddings, or network access in
    /// the deterministic runtimes.
    fn run_review_plan(&self, source: crate::cli::PlanSource, json: bool) -> Result<(), AppError> {
        use crate::config;
        use crate::git::{SnapshotSpec, resolve_snapshot};
        use crate::jury::review::{
            PlanInput, PlanReviewOptions, host_launch_from_env, render_review_text, run_plan_review,
        };

        let (text, file) = read_plan_source(&source)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review plan failed: {err}")))?;

        let (ctx, paths) = self.discover_storage()?;
        let resolved = resolve_snapshot(&ctx, &SnapshotSpec::Working).map_err(|err| {
            AppError::Other(anyhow::anyhow!(
                "review plan failed to resolve the working-tree snapshot: {err}"
            ))
        })?;

        // Trusted local configuration supplies the model profiles the
        // host may use; reviewer sessions never inherit a model. The
        // optional classifier/embedding providers come from the same
        // trusted configuration.
        let user_path = config::paths::user_config_path();
        let repo_path = config::paths::repo_config_path(&ctx.git_common_dir);
        let loaded =
            config::load_config(user_path.as_deref(), Some(&repo_path)).map_err(|err| {
                AppError::Other(anyhow::anyhow!(
                    "review plan failed to load configuration: {err}"
                ))
            })?;

        let launch = host_launch_from_env()
            .map_err(|err| AppError::Other(anyhow::anyhow!("review plan failed: {err}")))?;
        let mut options = PlanReviewOptions::new(launch);
        options.model_profiles = loaded.config.model_profiles.clone();
        options.contributor_id = loaded
            .config
            .contributor
            .as_ref()
            .map(|contributor| contributor.id.clone());
        let providers = optional_providers(&loaded.config);
        options.routing_provider = providers.routing;
        options.embedding_provider = providers.embeddings;

        // The plan file participates in the working snapshot when it
        // lives inside the worktree (untracked non-ignored content is
        // part of the target); compute its repository-relative path so
        // evidence can cite it.
        let plan_path = file.as_deref().and_then(|file| repo_relative(&ctx, file));
        let plan = PlanInput {
            text,
            path: plan_path,
        };

        let outcome = run_plan_review(&ctx, &resolved, &paths, &plan, &options)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review plan failed: {err}")))?;

        let rendered = if json {
            serde_json::to_string(&outcome.result)
                .map_err(|err| AppError::Other(anyhow::anyhow!("review plan failed: {err}")))?
        } else {
            render_review_text(&outcome.result)
        };
        print_to_stdout(&rendered);
        Ok(())
    }

    /// Run `nisse review code` (plan sections 58 and 60).
    ///
    /// Executes the complete code-review pipeline for the selected
    /// snapshot mode (`--working`, `--staged`, `--commit`, `--range`,
    /// `--branch`) and prints the result (text, or the strict
    /// `ReviewResult` JSON with `--json`). The local review event is
    /// persisted in the shared cache. Optional providers (plan section
    /// 34) improve routing and retrieval when configured; every
    /// provider failure degrades to the deterministic behavior, so
    /// reviews never require Needle, embeddings, or network access in
    /// the deterministic runtimes.
    fn run_review_code(&self, scope: crate::cli::CodeScope, json: bool) -> Result<(), AppError> {
        use crate::config;
        use crate::git::resolve_snapshot;
        use crate::jury::review::{
            CodeReviewOptions, host_launch_from_env, render_review_text, run_code_review,
        };

        let spec = code_scope_spec(&scope)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review code failed: {err}")))?;

        let (ctx, paths) = self.discover_storage()?;
        let resolved = resolve_snapshot(&ctx, &spec).map_err(|err| {
            AppError::Other(anyhow::anyhow!(
                "review code failed to resolve the snapshot: {err}"
            ))
        })?;

        // Trusted local configuration supplies the model profiles the
        // host may use; reviewer sessions never inherit a model. The
        // optional classifier/embedding providers come from the same
        // trusted configuration.
        let user_path = config::paths::user_config_path();
        let repo_path = config::paths::repo_config_path(&ctx.git_common_dir);
        let loaded =
            config::load_config(user_path.as_deref(), Some(&repo_path)).map_err(|err| {
                AppError::Other(anyhow::anyhow!(
                    "review code failed to load configuration: {err}"
                ))
            })?;

        let launch = host_launch_from_env()
            .map_err(|err| AppError::Other(anyhow::anyhow!("review code failed: {err}")))?;
        let mut options = CodeReviewOptions::new(launch);
        options.model_profiles = loaded.config.model_profiles.clone();
        options.contributor_id = loaded
            .config
            .contributor
            .as_ref()
            .map(|contributor| contributor.id.clone());
        let providers = optional_providers(&loaded.config);
        options.routing_provider = providers.routing;
        options.embedding_provider = providers.embeddings;

        let outcome = run_code_review(&ctx, &resolved, &paths, &options)
            .map_err(|err| AppError::Other(anyhow::anyhow!("review code failed: {err}")))?;

        let rendered = if json {
            serde_json::to_string(&outcome.result)
                .map_err(|err| AppError::Other(anyhow::anyhow!("review code failed: {err}")))?
        } else {
            render_review_text(&outcome.result)
        };
        print_to_stdout(&rendered);
        Ok(())
    }

    /// Run `nisse query`.
    ///
    /// Queries the durable project knowledge and the source index of
    /// the current working-tree snapshot. The index is refreshed
    /// lazily on demand (plan section 28); retrieval is deterministic
    /// and never requires an LLM. A configured embedding provider
    /// (plan section 34) reranks the bounded candidate set; provider
    /// failures degrade to the baseline ranking.
    fn run_query(&self, query: String, json: bool) -> Result<(), AppError> {
        use crate::git::{SnapshotSpec, resolve_snapshot};
        use crate::index::refresh_index;
        use crate::jury::catalog::Catalog;
        use crate::retrieval::{RetrievalQuery, Retriever, render_json, render_text};
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
        let user_path = crate::config::paths::user_config_path();
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

        let rendered = if json {
            render_json(&query, &resolved.descriptor, &bundle)
                .map_err(|err| AppError::Other(anyhow::anyhow!("query failed: {err}")))?
        } else {
            render_text(&query, &resolved.descriptor, &bundle)
        };
        print_to_stdout(&rendered);
        Ok(())
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
        let rows = crate::learning::facts::load_pending(&paths)
            .map_err(|err| AppError::Other(anyhow::anyhow!("records pending failed: {err}")))?;
        print_to_stdout(&crate::learning::facts::render_pending(&rows));
        Ok(())
    }

    /// Run `nisse feedback` (plan sections 49 and 50).
    ///
    /// Records a known outcome for one finding of a stored review as
    /// immutable durable observations. Rust constructs every
    /// authoritative observation field from the stored review, the
    /// stored finding, the explicit outcome, and the review's snapshot
    /// descriptors; LLMs construct nothing. Submission is idempotent,
    /// outcomes are immutable, and without a contributor identity the
    /// outcome remains pending locally (plan section 44) while the
    /// command fails explicitly.
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

        let (ctx, paths) = self.discover_storage()?;

        // Contributor identity lives in trusted local configuration;
        // feedback is noninteractive and never prompts.
        let user_path = crate::config::paths::user_config_path();
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

        let recording = feedback::record_feedback(
            &ctx,
            &paths,
            &review_id,
            &finding_id,
            outcome,
            contributor.as_deref(),
        )
        .map_err(|err| AppError::Other(anyhow::anyhow!("feedback failed: {err}")))?;

        print_to_stdout(&feedback::render_feedback(
            &review_id,
            &finding_id,
            outcome,
            &recording,
        ));
        Ok(())
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
        let user_path = crate::config::paths::user_config_path();
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
                let outcome = crate::config::identity::resolve_or_prompt(
                    None,
                    user_path.as_deref(),
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
        let user_path = config::paths::user_config_path();
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
    use super::*;

    #[test]
    fn every_frozen_command_is_typed_not_implemented() {
        let app = App::new();

        // Evaluation commands are implemented (milestone 23); the MCP
        // server remains a future milestone.
        let cases: Vec<Command> = vec![Command::Mcp];

        for command in cases {
            let err = app.run(command).unwrap_err();
            assert!(
                matches!(err, AppError::NotImplemented(_)),
                "expected NotImplemented, got {err:?}"
            );
        }
    }
}
