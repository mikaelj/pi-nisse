```text
NISSE 1.0 — FROZEN IMPLEMENTATION PLAN
========================================

STATUS
------

This document is the implementation contract for Nisse 1.0.

It is authoritative for:

- implementation scope;
- architecture;
- milestone order;
- public interfaces;
- storage formats;
- workflow semantics;
- invariants;
- acceptance criteria.

Do not modify this document during implementation unless the user explicitly
requests a plan change.

The implementation agent is expected to be Pi.

IMPORTANT DESIGN ASSUMPTION:

Neither the implementation agent nor the LLMs used by Nisse should be assumed
to be exceptionally intelligent.

The system must therefore favor:

- explicit contracts;
- small sequential milestones;
- deterministic code;
- narrow LLM tasks;
- strict schemas;
- bounded workflows;
- independent verification;
- conservative failure behavior;
- tests instead of implicit assumptions;
- simple control flow instead of clever autonomous behavior.

Core rule:

    LLMs are untrusted semantic workers, not orchestrators.


======================================================================
0. MANDATORY IMPLEMENTATION LOOP
======================================================================

IMPLEMENT EXACTLY ONE MILESTONE AT A TIME.

For every milestone, Pi MUST perform this exact sequence:

    1. INSPECT CURRENT REPOSITORY STATE
    2. IDENTIFY THE NEXT INCOMPLETE MILESTONE
    3. IMPLEMENT THAT MILESTONE ONLY
    4. RUN TESTS
    5. REVIEW THE IMPLEMENTATION AGAINST THIS DOCUMENT
    6. FIX ALL DISCOVERED DEVIATIONS
    7. RUN TESTS AGAIN
    8. REVIEW AGAINST THIS DOCUMENT AGAIN
    9. COMMIT THE COMPLETED MILESTONE WITH GIT
   10. VERIFY THE WORKING TREE
   11. REPORT
   12. STOP

Do not start the following milestone in the same implementation run.

Even if the following milestone appears trivial, stop after committing the
current milestone.


----------------------------------------------------------------------
0.1 START OF AN IMPLEMENTATION RUN
----------------------------------------------------------------------

Inspect:

    git status --short
    git log --oneline --decorate -20

Inspect the implementation and tests relevant to the most recently completed
milestone.

Determine the highest milestone that is actually complete.

A milestone counts as complete only when:

- its implementation exists;
- its required tests exist;
- all acceptance criteria are satisfied;
- the implementation was reviewed against this document;
- a milestone completion commit exists.

Expected commit format:

    nisse: complete milestone NN - <short milestone name>

Examples:

    nisse: complete milestone 01 - foundation
    nisse: complete milestone 02 - configuration and trust

If the repository has not yet been initialized with Git when milestone 1
begins, initialize Git before implementing milestone 1.


----------------------------------------------------------------------
0.2 PRE-EXISTING WORKING-TREE CHANGES
----------------------------------------------------------------------

Never destroy, overwrite, stage, or commit unrelated user changes.

If unrelated uncommitted changes exist:

- leave them untouched;
- do not include them in the milestone commit;
- work around them where safe.

If implementation cannot proceed without ambiguously mixing with or
overwriting unrelated changes:

- stop;
- report the exact conflict;
- do not guess.


----------------------------------------------------------------------
0.3 IMPLEMENTATION SCOPE
----------------------------------------------------------------------

Implement exactly the next milestone.

Do not:

- skip milestones;
- combine milestones;
- continue automatically into another milestone;
- partially implement future milestones for convenience;
- redesign earlier milestones to make future work easier;
- introduce speculative abstractions whose first real use belongs to a later
  milestone.

Minimal compile-time scaffolding for later interfaces is permitted only where
the current milestone explicitly requires it.


----------------------------------------------------------------------
0.4 TESTING
----------------------------------------------------------------------

Run all tests required by the milestone.

Also run all existing regression tests.

For Rust milestones, where applicable:

    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets --all-features

Also run:

    git diff --check

For TypeScript milestones, run the repository-defined equivalents for:

- formatting;
- linting;
- type checking;
- unit tests.

The default test suite must not require:

- Internet access;
- a real LLM provider;
- a running inference server;
- Needle.

Use mocks/fakes for provider-dependent behavior.


----------------------------------------------------------------------
0.5 MANDATORY SELF-REVIEW
----------------------------------------------------------------------

After implementation and tests pass, perform a deliberate milestone review.

For the current milestone:

1. Re-check every requirement in its milestone specification.

2. Inspect every file changed by the milestone.

3. Verify:
   - required behavior;
   - public interfaces;
   - storage paths;
   - invariants;
   - error handling;
   - tests;
   - acceptance criteria.

4. Look explicitly for:
   - missing requirements;
   - accidental future-scope implementation;
   - architectural drift;
   - insufficient tests;
   - incorrect naming;
   - incorrect persistence semantics;
   - unsafe delegation to LLMs;
   - unjustified assumptions.

5. Fix all discovered problems.

6. Re-run tests.

7. Perform the review again.

A milestone must not be committed while a known deviation remains.


----------------------------------------------------------------------
0.6 GIT COMMIT
----------------------------------------------------------------------

Commit only when:

- implementation is complete;
- tests pass;
- self-review passes;
- acceptance criteria pass;
- no known milestone defect remains.

Before committing:

    git status --short
    git diff
    git diff --cached

Stage only milestone-related files.

Commit:

    git commit -m "nisse: complete milestone NN - <short milestone name>"

Do not amend previous milestone commits.

Do not squash milestone commits during implementation.

Each milestone commit is a permanent implementation checkpoint.


----------------------------------------------------------------------
0.7 AFTER COMMIT
----------------------------------------------------------------------

Run:

    git status --short

Verify that no milestone-related changes remain uncommitted.

Report:

- completed milestone number and name;
- commit hash;
- files created/changed;
- commands/tests executed;
- self-review result;
- acceptance criteria result;
- unrelated pre-existing changes left untouched.

Then STOP.


----------------------------------------------------------------------
0.8 DEFECTS IN EARLIER MILESTONES
----------------------------------------------------------------------

If milestone N exposes a genuine defect in an earlier milestone:

- fix the defect only if required to complete N;
- preserve the earlier architectural contract;
- add a regression test;
- report the earlier defect explicitly.

Do not use a defect as permission to redesign the architecture.


----------------------------------------------------------------------
0.9 CONTRACT CONFLICTS
----------------------------------------------------------------------

If implementation appears to require violating a frozen contract:

- do not silently reinterpret the contract;
- do not redesign the subsystem;
- implement only non-conflicting work that is safe;
- report the exact conflict;
- do not commit the milestone as complete.


======================================================================
1. PRODUCT DEFINITION
======================================================================

Nisse is a local software-review engine.

Nisse Core has four primary operations:

    review_plan
    review_code
    query_project
    record_feedback

Nisse Core does NOT have:

    Plan mode
    Build mode

Those are coding-agent workflow concepts, not Nisse Core concepts.

`review_plan` means:

    review a plan produced by another agent or human

`review_code` means:

    review code changes produced by another agent or human

Nisse itself does not implement the reviewed work.


======================================================================
2. PRIMARY USER EXPERIENCE
======================================================================

Pi is the primary coding agent.

Nisse is integrated into Pi's task workflow.

For substantial work, the intended flow is:

    understand task
        |
        v
    draft implementation plan
        |
        v
    bounded Nisse plan-review loop
        |
        v
    implement
        |
        v
    run project tests
        |
        v
    bounded Nisse code-review/fix loop
        |
        v
    report completion


For trivial work, Pi may use:

    understand
        |
        v
    implement
        |
        v
    test
        |
        v
    done

without invoking Nisse automatically.


======================================================================
3. NISSE CORE VS PI WORKFLOW
======================================================================

Nisse Core remains operation-based.

It does not own a task state machine.

The Pi integration owns workflow policy.

This distinction is frozen.

Nisse Core:

    review_plan(...)
    review_code(...)
    query_project(...)
    record_feedback(...)

Pi integration:

    decide whether work is substantial;
    construct/revise plans;
    call Nisse plan review;
    implement;
    run project tests;
    call Nisse code review;
    fix findings;
    submit known outcomes.


Do not add Plan/Build mode state to Nisse Core.


======================================================================
4. MAIN PI WORKFLOW FOR SUBSTANTIAL TASKS
======================================================================

The Pi integration must define the following bounded workflow.

PHASE A — UNDERSTAND
--------------------

Main Pi investigates the task and project sufficiently to understand the
requested change.

Nisse is not responsible for creating the original implementation plan.


PHASE B — DRAFT PLAN
--------------------

Main Pi creates a concrete implementation plan.

The plan must be specific enough to review.

It should identify, where applicable:

- files/modules;
- interfaces;
- data flow;
- compatibility constraints;
- tests;
- migration strategy;
- error handling;
- rollout implications.


PHASE C — PLAN REVIEW LOOP
--------------------------

Call:

    nisse_review_plan

If the review contains no:

    blocker
    high

findings:

    leave the plan-review loop.

For valid blocker/high findings:

1. Main Pi revises the plan.
2. Call plan review again.

Maximum plan-review attempts:

    3

If blocker/high findings remain after attempt 3:

- stop before implementation;
- explain the unresolved issues to the user;
- request user direction.

Do not continue autonomously into implementation with unresolved blocker/high
plan findings after the loop limit.

Medium/low/info findings do not automatically block implementation.

Main Pi may incorporate them when useful.


PHASE D — IMPLEMENT
-------------------

Main Pi implements the current reviewed plan.

Main Pi remains the coding agent.

Nisse does not modify source.


PHASE E — PROJECT TESTS
-----------------------

Main Pi runs the project's appropriate tests/checks before code review.

If basic implementation tests fail:

- fix them before requesting Nisse code review.


PHASE F — CODE REVIEW LOOP
--------------------------

Call:

    nisse_review_code

For valid blocker/high findings:

1. Main Pi investigates the finding.
2. If valid, fix it.
3. Run relevant project tests.
4. Record known outcome where appropriate.
5. Run Nisse code review again.

Maximum code-review attempts:

    3

If blocker/high findings remain after attempt 3:

- do not claim the task is complete;
- report the unresolved findings to the user;
- request user direction.

Medium/low/info findings do not automatically prevent completion.

Main Pi may address them based on relevance/scope.


PHASE G — COMPLETE
------------------

The task may be reported complete when:

- implementation tests pass;
- no unresolved blocker/high finding remains;
- the bounded review loops have completed successfully.


======================================================================
5. SUBSTANTIAL VS TRIVIAL WORK
======================================================================

Main Pi should normally use Nisse for work involving one or more of:

- architecture changes;
- multiple interacting files/modules;
- concurrency;
- realtime behavior;
- embedded constraints;
- persistence;
- database schema;
- networking/protocols;
- authentication/security;
- compatibility;
- public API changes;
- migrations;
- complex bug fixes;
- substantial refactors;
- performance-sensitive behavior;
- important project-specific constraints.


Main Pi may normally skip automatic Nisse review for:

- spelling;
- formatting;
- comments only;
- obvious mechanical rename;
- trivial generated changes;
- clearly non-risky one-line edits.


Explicit user instruction to use Nisse always overrides these heuristics.


======================================================================
6. HIGH-LEVEL ARCHITECTURE
======================================================================

Nisse consists of:

A. NISSE CORE
   Rust executable.

B. PI REVIEW HOST
   Small TypeScript child process using Pi SDK.

C. PRIMARY PI PACKAGE
   Thin Main-Pi extension plus skill.

D. OPTIONAL NEEDLE ADAPTER
   Local classifier/embedding integration.

E. SECONDARY MCP ADAPTER
   Portable integration for non-Pi clients.


Architecture:

                         MAIN PI
                            |
                     Pi extension tool
                            |
                            v
                  +-------------------+
                  |    NISSE CORE     |
                  |       Rust        |
                  +-------------------+
                    |     |      |
                    |     |      +--- durable project records
                    |     +---------- local RAG/index
                    +---------------- Git/snapshots
                            |
                         routing
                            |
                            v
                         JuryPlan
                            |
                            v
                  +-------------------+
                  |  PI REVIEW HOST   |
                  |    TypeScript     |
                  +-------------------+
                    |       |       |
                    v       v       v
                  Pi #1   Pi #2   Pi #3
                    |       |       |
                    +-------+-------+
                            |
                            v
                     constrained merge
                            |
                            v
                      ReviewResult
                            |
                            v
                         MAIN PI


The Rust core owns state and orchestration.

Pi reviewer sessions perform narrow semantic work.

Needle may assist classification and semantic similarity.

No LLM controls durable state.


======================================================================
7. CORE DESIGN PRINCIPLES
======================================================================

1. LLMs are untrusted semantic workers.

2. Deterministic decisions belong in Rust whenever practical.

3. LLM outputs use strict schemas.

4. Invalid/uncertain LLM output causes less automation, not guessed state.

5. LLMs never directly mutate durable project knowledge.

6. LLMs may propose knowledge.

7. Nisse validates knowledge before persistence.

8. Semantically important durable facts require independent verification.

9. LLMs do not choose Git snapshots.

10. LLMs do not determine snapshot membership.

11. LLMs do not manage Git.

12. LLMs do not choose their own capabilities.

13. LLMs do not invent permanent reviewer identities.

14. LLMs do not calculate lesson statistics.

15. LLMs do not deduplicate observation IDs.

16. Final synthesis cannot invent findings.

17. Reviewer tools are read-only and Nisse-controlled.

18. Current source evidence outranks remembered facts.

19. Uncertainty is represented explicitly.

20. Generated caches are disposable.

21. Durable shared knowledge preserves provenance.

22. Reviewer quality may improve with better models, but weak models must not
    corrupt system state.


======================================================================
8. IMPLEMENTATION LANGUAGES
======================================================================

NISSE CORE
----------

Rust.

Rust edition:

    2024

Use one Cargo package producing:

    nisse


PI REVIEW HOST
--------------

TypeScript.

Use the official Pi coding-agent SDK.

The host is a child process.

It exists only for the duration of required semantic work.

It is not a daemon.


PRIMARY PI INTEGRATION
----------------------

TypeScript Pi Package containing:

- extension;
- skill.


OPTIONAL NEEDLE INTEGRATION
---------------------------

Use a small isolated adapter compatible with Needle.

Needle availability is optional.


======================================================================
9. CORE TECHNOLOGY CHOICES
======================================================================

Rust async runtime:

    tokio

CLI:

    clap derive API

Serialization:

    serde
    serde_json
    toml

Typed errors:

    thiserror

Application-boundary errors:

    anyhow

Logging:

    tracing
    tracing-subscriber

HTTP:

    reqwest
    rustls

SQLite:

    rusqlite
    bundled SQLite

Git:

    installed git executable

Parsing:

    tree-sitter

MCP:

    official Rust MCP SDK

Primary reviewer runtime:

    Pi SDK


======================================================================
10. TRUST AND LOCALITY
======================================================================

Default permitted model endpoints:

- loopback;
- RFC1918 private IPv4;
- IPv6 loopback;
- IPv6 unique-local addresses.

Default:

    allow_non_private_endpoints = false

Other endpoints require explicit trusted local configuration.

Git-tracked `.nisse/**` files are project data.

They cannot configure:

- executable commands;
- arbitrary executable paths;
- arbitrary extensions;
- model endpoints;
- credentials;
- arbitrary filesystem roots.

Runtime/provider configuration is local trusted configuration.

Reviewer sessions do not inherit Main Pi's active model automatically.

Nisse selects an explicitly configured model profile.


======================================================================
11. CONFIGURATION PRECEDENCE
======================================================================

Configuration precedence:

1. explicit CLI options;
2. `$GIT_COMMON_DIR/nisse/config.toml`;
3. user config at `$HOME/.nisse/config.toml` (dotfile convention on
   every platform, including macOS; the legacy platform-native path is
   read with a per-run migration notice for one release);
4. compiled defaults.

Contributor identity lives in user config.

Secrets do not live under `.nisse/**`.

Model profiles are named configuration concepts.

Required conceptual profiles:

    standard
    strong

Optional:

    fast
    strongest

Do not hard-code commercial model names into architecture contracts.


======================================================================
12. SOURCE TREE CONTRACT
======================================================================

Major repository layout:

    PLAN.md
    Cargo.toml
    Cargo.lock
    rust-toolchain.toml
    README.md

    src/
        main.rs
        app.rs

        cli/
        config/
        domain/
        git/
        storage/
        index/
        retrieval/
        routing/
        jury/
        learning/
        providers/
        mcp/

    runtime/
        pi-host/
            package.json
            tsconfig.json
            src/
                main.ts
                protocol.ts
                session.ts
                tools.ts
                models.ts
                structured.ts

    integrations/
        pi/
            package.json
            extensions/
                nisse.ts
            skills/
                nisse/
                    SKILL.md

        mcp/
            README.md

        visual-studio/
            README.md

    assets/
        reviewers/
        practices/

    tools/
        needle/

    tests/
        fixtures/
        integration/

    docs/
        architecture-contract.md
        record-format.md
        reviewer-format.md
        pi-host-protocol.md
        storage.md
        learning.md
        security.md

Additional files inside these subsystem boundaries are allowed.


======================================================================
13. CORE DOMAIN TYPES
======================================================================

Define and retain:

    ReviewKind
    ReviewRequest
    ReviewResult

    SnapshotDescriptor

    EvidenceRef
    ContextEntry
    ContextBundle

    ReviewerProfile
    ReviewerExecutionSpec
    ReviewerFinding
    ReviewerReport

    FinalFinding
    SynthesisResult

    RoutingInput
    RoutingDecision
    JuryPlan

    ObservationRecord
    FactRecord
    LessonRecord
    ProjectRecord

    CandidateProjectFact
    FactVerificationResult

    FeedbackOutcome

    ModelProfile


======================================================================
14. REVIEW KINDS
======================================================================

ReviewKind:

    Plan
    Code


Severity:

    blocker
    high
    medium
    low
    info


ImpactClass:

    correctness
    security
    data_loss
    deadlock
    crash
    compatibility
    performance
    test_gap
    maintainability
    style
    other


Maximum severity policy:

    style           -> low
    maintainability -> medium
    test_gap        -> medium
    performance     -> high
    compatibility   -> high
    other           -> high


The following may reach blocker when evidence supports it:

    correctness
    security
    data_loss
    deadlock
    crash


Reviewer proposes severity.

Rust applies severity policy.

Synthesis may lower severity.

Synthesis may not raise severity beyond validated source findings.


======================================================================
15. EVIDENCE MODEL
======================================================================

EvidenceRef may refer to:

- source code;
- diff content;
- plan content;
- project fact.

Fields include:

    kind
    path
    optional symbol
    optional line_start
    optional line_end
    optional content_key
    optional plan_section
    optional record_id
    short excerpt


High/blocker findings require concrete evidence.

Nisse validates:

- referenced source exists;
- source belongs to target snapshot;
- ranges are valid;
- referenced record exists where applicable.

Structural validation does not itself prove semantic correctness.


======================================================================
16. STATIC REVIEWER CATALOG
======================================================================

Built-in reviewer IDs:

    quality
    testing
    security
    concurrency
    performance
    embedded-resource
    realtime
    protocol
    networking
    persistence
    database
    api-design
    error-handling
    maintainability
    dynamic-domain-expert


`quality` always participates.


ReviewerProfile contains:

    id
    display_name
    description

    trigger_terms[]
    language_hints[]
    path_hints[]
    practice_tags[]

    checklist[]
    base_instructions

    default_model_profile
    default_thinking_level

    allowed_capabilities[]


Reviewer checklists must be explicit.

Example concurrency checklist concepts:

- shared mutable state;
- task/thread/ISR context;
- queue boundaries;
- blocking calls;
- atomics;
- locks;
- ordering;
- lifetime;
- races;
- deadlocks.

A reviewer is not expected to invent its own review methodology.


======================================================================
17. JURY SIZE
======================================================================

Minimum:

    2

Maximum:

    4


Composition:

    quality
    +
    1 to 3 specialists


The synthesizer is not a juror.

`quality` is added deterministically.


======================================================================
18. REVIEWER EXECUTION SPEC
======================================================================

Rust constructs one ReviewerExecutionSpec per juror.

Fields:

    reviewer_id
    specialization

    instructions
    checklist[]

    model_profile
    thinking_level

    initial_context

    allowed_capabilities[]

    max_lookup_calls
    max_returned_context_bytes
    timeout


The reviewer cannot modify its execution spec.


======================================================================
19. DYNAMIC DOMAIN EXPERT
======================================================================

`dynamic-domain-expert` is a stable reviewer identity.

No LLM may invent an arbitrary agent architecture.

Nisse may obtain a constrained DynamicDomainDescription:

    domain_label
    focus_terms[]
    risks_to_check[]
    relevant_practice_tags[]


Nisse combines this description with a static reviewer template.

The dynamic description cannot:

- add tools;
- add executables;
- add providers;
- change endpoint policy;
- create a permanent reviewer identity.


======================================================================
20. PI REVIEW HOST
======================================================================

The Pi host creates isolated Pi SDK sessions.

For every reviewer:

- create new in-memory session;
- set explicit model;
- set explicit thinking level;
- set explicit system instructions;
- set explicit checklist;
- set explicit tools;
- set explicit initial context.

Reviewer sessions must not automatically discover or inherit:

- Main Pi conversation;
- project Pi extensions;
- arbitrary skills;
- prompt templates;
- AGENTS/context files.

Dispose reviewer session after completion.


======================================================================
21. PI HOST PROTOCOL
======================================================================

Rust and Pi host communicate using bidirectional JSONL over stdin/stdout.

stdout:

    protocol messages only

stderr:

    logs


Every message:

    protocol_version
    id
    type


Rust -> host:

    host_config
    start_review
    start_fact_verification
    start_fact_synthesis
    start_domain_description
    start_finding_equivalence_check
    tool_response
    cancel


Host -> Rust:

    host_ready
    reviewer_started
    reviewer_completed
    reviewer_failed
    tool_request
    fact_verification_completed
    fact_synthesis_completed
    domain_description_completed
    finding_equivalence_completed
    host_error


Protocol version:

    1


======================================================================
22. REVIEWER TOOLS
======================================================================

Initial read-only capabilities:

    project_lookup
    symbol_lookup
    source_lookup
    relation_lookup


project_lookup(query)

Returns bounded ranked ContextEntry values.


symbol_lookup(symbol)

Returns matching definitions and metadata from the target snapshot.


source_lookup(path, optional_range)

Returns source from the target snapshot only.


relation_lookup(symbol)

Returns known bounded relationships.


Reviewers do not receive:

- edit;
- write;
- shell;
- Git mutation;
- unrestricted filesystem access.


======================================================================
23. INITIAL CONTEXT
======================================================================

A reviewer must receive useful initial context.

Correctness must not depend on clever tool use.

Within budget, initial context includes:

1. plan or diff;
2. directly affected source;
3. changed symbols;
4. strongest known related symbols;
5. relevant project facts;
6. reviewer checklist;
7. relevant built-in practices;
8. relevant lessons.

Reviewer tools provide supplemental investigation.


======================================================================
24. GIT AND WORKTREE MODEL
======================================================================

Use installed Git as semantic authority.

Discover via Git:

    repository root
    worktree root
    GIT_DIR
    GIT_COMMON_DIR
    HEAD
    branch
    index
    merge-base
    tracked files
    ignored files
    untracked files


Generated shared state:

    $GIT_COMMON_DIR/nisse/


Generated worktree-specific state:

    $GIT_DIR/nisse/


Do not assume `.git` is a directory.

Linked worktrees are first-class.


======================================================================
25. SNAPSHOT SEMANTICS
======================================================================

PLAN REVIEW
-----------

Default project snapshot:

    current working tree


CODE REVIEW MODES
-----------------

working

Compare HEAD against final working-tree state.

Include:

- staged;
- unstaged;
- untracked non-ignored.


staged

Compare HEAD against index.


commit <OID>

Compare commit against first parent.

Root commit compares against Git empty tree.


range <BASE>..<HEAD>

Compare BASE tree against HEAD tree.


branch <BASE-REF>

Compute merge-base(BASE-REF, HEAD).

Compare merge-base tree against HEAD.


`.nisse/**` is excluded from application code review by default.


======================================================================
26. SNAPSHOT AUTHORITY
======================================================================

Every review has a SnapshotDescriptor.

All code retrieval must prove membership in that snapshot.

Shared cache may contain content from other:

- branches;
- commits;
- worktrees.

Cache presence does not imply snapshot membership.

This invariant must be tested explicitly.


======================================================================
27. CONTENT-ADDRESSED SOURCE INDEX
======================================================================

Baseline:

    tree-sitter


Nisse 1.0 languages:

    C
    C++
    C#
    Dart
    Rust
    JavaScript
    TypeScript


Extract where possible:

    functions
    methods
    classes/structs
    enums
    types
    imports/includes/usings
    definitions
    identifier references
    source spans
    compact source skeletons


Tracked Git content key:

    git:<blob-oid>


Dirty/untracked:

    sha256:<hash>


Parsed artifact identity includes:

    content key
    language
    parser/index version


Artifacts may be reused across branches/worktrees.


======================================================================
28. LAZY INDEX REFRESH
======================================================================

Normal operations refresh incrementally on demand.

Algorithm:

    enumerate target snapshot
        |
        v
    obtain content keys
        |
        v
    compare manifest
        |
        +-- known -> reuse
        |
        +-- new -> parse
        |
        +-- removed -> remove path mapping


Refresh statistics:

    files_seen
    files_reused
    files_parsed
    files_removed
    duration


No mandatory init or refresh command exists.


======================================================================
29. LOCAL SQLITE STORAGE
======================================================================

Shared:

    $GIT_COMMON_DIR/nisse/cache.sqlite


Worktree-specific:

    $GIT_DIR/nisse/worktree.sqlite


Shared tables include:

    content_artifact
    parsed_artifact
    symbol
    reference
    text_chunk
    embedding
    review_event
    review_feedback
    pending_learning


Worktree tables include:

    manifest
    path_content
    record_index
    refresh_state


SQLite is regenerable local state.

It is not authoritative shared memory.


======================================================================
30. RETRIEVAL
======================================================================

Baseline works without embeddings.

Signals:

    exact symbol
    FTS
    changed-path relevance
    changed-symbol relevance
    module/path proximity
    relation graph
    project facts
    project lessons
    reviewer practice relevance


Optional embeddings rerank a bounded candidate set.

Do not rely exclusively on vector search.


======================================================================
31. CONTEXT PRIORITY
======================================================================

Context priority:

1. review input;
2. changed/affected source;
3. verified relevant project facts;
4. directly related source;
5. relevant lessons;
6. built-in practices;
7. lower-confidence supplemental context.


Potentially stale facts are marked/downranked.

Conflicting facts are shown as conflict, not truth.


======================================================================
32. BUILT-IN PRACTICES
======================================================================

Ship built-in packs for:

    quality
    testing
    security
    concurrency
    performance
    embedded
    realtime
    protocol
    networking
    persistence
    database
    api-design
    error-handling
    maintainability


Practices contain reusable domain knowledge.

Reviewer profiles contain explicit checklists.

Project-specific knowledge belongs in project records.


======================================================================
33. ROUTING
======================================================================

Mandatory:

    RulesClassifier


Optional:

    NeedleClassifier


Classifier input:

    request descriptors
    changed paths
    languages
    changed symbols
    deterministic tags
    relevant lessons


Rules signals:

    trigger terms
    language hints
    path hints
    practice tags
    lesson boosts


Routing procedure:

1. add quality;
2. score static specialists;
3. select up to 3 specialists;
4. enforce total maximum 4;
5. if no static specialist reaches threshold, add dynamic-domain-expert.


Default threshold:

    0.35


Lessons may increase score.

Lessons may not suppress a reviewer.


======================================================================
34. NEEDLE
======================================================================

Needle is optional.

Suitable uses:

- reviewer classification;
- embeddings;
- candidate semantic clustering;
- cheap structured relation classification.


Needle does not:

- perform final substantive review;
- control Git;
- mutate durable state;
- author authoritative facts;
- calculate lesson statistics;
- decide persistence.


Needle failures degrade to deterministic behavior.


======================================================================
35. REVIEWER OUTPUT
======================================================================

ReviewerReport is strict structured output.

Contains:

    reviewer_id
    summary
    findings[]
    candidate_project_facts[]
    model_metadata
    duration


ReviewerFinding contains:

    id
    impact_class
    proposed_severity
    category
    title
    claim
    explanation
    recommendation
    evidence[]


Malformed structured output:

1. perform one constrained repair;
2. validate again;
3. if still invalid, reviewer fails.

Do not retry malformed structure indefinitely.


======================================================================
36. REVIEWER FAILURE POLICY
======================================================================

A failed reviewer may be retried once.

Retry uses the same ReviewerExecutionSpec.

Do not silently change:

- model;
- reviewer;
- checklist;
- capabilities.

Review may continue when at least:

    2 valid independent reviewer reports

exist.

Otherwise review fails explicitly.


======================================================================
37. FINDING VALIDATION
======================================================================

Rust validates:

- schema;
- reviewer identity;
- impact class;
- severity bounds;
- evidence existence;
- evidence snapshot membership;
- evidence requirement for blocker/high;
- text bounds.

Invalid findings are excluded and diagnostic information retained.


======================================================================
38. FINDING CONSOLIDATION
======================================================================

Final synthesis may not create new underlying findings.

Pipeline:

1. collect validated findings;

2. calculate deterministic fingerprints;

3. group obvious duplicates;

4. optionally use embeddings/Needle for semantic duplicate candidates;

5. where ambiguous, run narrow Pi equivalence check:

       SAME
       DIFFERENT
       UNCERTAIN

6. group only SAME findings;

7. synthesize wording/recommendation for existing groups.


Synthesis may:

- shorten;
- clarify;
- combine evidence;
- combine compatible recommendations;
- describe disagreement;
- lower severity.


Synthesis may not:

- invent a source finding;
- invent a new issue;
- increase severity beyond source maximum;
- silently delete a valid finding.


Every FinalFinding contains:

    source_finding_ids[]
    reviewer_ids[]


Cross-validation requires equivalent findings from at least two independent
reviewers.


======================================================================
39. DURABLE PROJECT STORAGE
======================================================================

Git-tracked layout:

    .nisse/
        people/
            <contributor-id>/
                <ULID>.json

        common/
            <ULID>.json


Durable records are immutable after creation.

New knowledge creates new records.

Old records remain for provenance.


Record ID:

    ULID


Timestamp:

    UTC RFC3339


Schema version:

    1


======================================================================
40. RECORD KINDS
======================================================================

Exactly three:

    observation
    fact
    lesson


Meaning:

    observation:
        something happened

    fact:
        supported project knowledge

    lesson:
        repeated observations suggest a useful project-specific pattern


======================================================================
41. OBSERVATION RECORDS
======================================================================

Primary 1.0 observation type:

    review-outcome


Example conceptual fields:

    schema_version
    id
    kind
    author
    created_at

    observation_type

    review_id
    finding_id
    reviewer_id

    outcome
    impact_class
    category

    languages[]
    tags[]
    paths[]

    snapshot


Observations remain permanently available as source learning data.

Do not persist full LLM transcripts as project observations.


======================================================================
42. FACT RECORDS
======================================================================

Fact fields include:

    schema_version
    id
    kind
    author
    created_at

    subject
    text
    tags[]

    state

    evidence[]

    source_review_id
    source_record_ids[]

    supersedes[]


Fact state:

    supported
    unresolved_conflict


Only supported facts are treated as normal project truth.

Conflicts are surfaced explicitly.


======================================================================
43. LESSON RECORDS
======================================================================

Lesson fields:

    schema_version
    id
    kind
    author
    created_at

    subject
    text
    tags[]

    routing_hint

    source_observation_ids[]

    supersedes[]


Routing hint:

    reviewer_id
    path_globs[]
    required_tags_any[]
    boost


Any stored aggregate counts are caches only.

They must be recomputable from source observations.


======================================================================
44. CONTRIBUTOR IDENTITY
======================================================================

Contributor IDs:

    [a-zA-Z0-9][a-zA-Z0-9._-]{0,63}


When interactive use first needs durable contributor identity and none exists,
ask:

    What should Nisse call you?


Save locally.

Existing contributor IDs and Git identity may be hints.

Noninteractive operation never waits for identity input.

Learning remains pending locally when identity is unavailable.


======================================================================
45. PEOPLE AND COMMON
======================================================================

`.nisse/people/<id>/` records are directly retrievable.

They do not require promotion.

`.nisse/common/` stores consolidated:

    facts
    lessons


Observations remain contributor records.

Common records may rank higher than contributor records.

Current source evidence always outranks remembered facts.


======================================================================
46. FACT CANDIDATES
======================================================================

Reviewers may propose:

    CandidateProjectFact


Fields:

    subject
    claim
    tags[]
    evidence[]
    confidence


This is only a proposal.

Reviewer output cannot directly create durable facts.


======================================================================
47. FACT PERSISTENCE
======================================================================

STEP 1 — DETERMINISTIC VALIDATION
---------------------------------

Require:

    confidence >= 0.90

Validate:

- schema;
- project-level durability;
- text bounds;
- evidence;
- snapshot membership;
- source existence;
- obvious secret filtering;
- near-duplicate active facts.


STEP 2 — INDEPENDENT VERIFICATION
---------------------------------

Create a fresh Pi verification session.

Input:

    exact claim
    exact evidence excerpts
    narrow instructions


Allowed result:

    SUPPORTED
    CONTRADICTED
    INSUFFICIENT


STEP 3 — PERSIST
----------------

Only:

    SUPPORTED

plus all deterministic checks may create a durable supported fact.


CONTRADICTED:

    reject.


INSUFFICIENT:

    retain pending locally.


Reviewer and verifier are separate sessions.


======================================================================
48. FACT STALENESS
======================================================================

Code-derived facts retain content provenance.

If supporting content is absent from the current target snapshot:

    fact becomes potentially stale.


Potentially stale facts are:

- downranked;
- not presented as unquestioned current truth;
- eligible for targeted revalidation.


Revalidation:

    SUPPORTED
    CONTRADICTED
    INSUFFICIENT


Updated knowledge creates a new record.

Old fact remains.


======================================================================
49. FEEDBACK
======================================================================

Outcomes:

    fixed
    accepted
    dismissed


Definitions:

fixed:
    implementation changed specifically to address the finding.

accepted:
    finding considered valid but intentionally not fixed now.

dismissed:
    finding investigated and judged incorrect/not applicable.


Unknown:

    remain unknown.


Do not infer outcome merely from later source changes.


======================================================================
50. OBSERVATION CREATION
======================================================================

Known outcome creates an immutable ObservationRecord.

Rust constructs authoritative observation metadata from:

- stored review;
- stored finding;
- explicit outcome;
- snapshot descriptors.

LLMs do not construct authoritative observation identity/statistics.


======================================================================
51. LESSON LEARNING
======================================================================

Lesson learning is primarily deterministic.

Group observations using stable descriptors such as:

    reviewer
    tags
    language
    path/module
    finding category


Initial routing lesson threshold:

    unique observations >= 5
    positive >= 4
    dismissed <= 1


Positive:

    fixed
    accepted


Rust calculates:

    support
    positive
    negative
    routing conditions
    routing boost


Maximum lesson routing boost:

    0.35


Pi may generate only concise human-readable wording.

Text does not control routing behavior.


======================================================================
52. KNOWLEDGE CONSOLIDATION
======================================================================

One conceptual operation:

    consolidate(records)


Same operation applies to:

- one contributor's records;
- multiple contributors' records.


Authorship affects provenance, not consolidation semantics.


======================================================================
53. OBSERVATIONS AND CONSOLIDATION
======================================================================

Observations are not semantically compacted away.

They remain the authoritative source for lesson statistics.

Historical packaging/archiving may be considered after 1.0 if necessary.

Do not implement observation archival in Nisse 1.0.


======================================================================
54. FACT CONSOLIDATION
======================================================================

Pipeline:

1. select active facts;

2. remove deterministic exact duplicates/candidates already superseded;

3. create semantic candidate groups using:
   - subject;
   - tags;
   - lexical similarity;
   - optional Needle embeddings;

4. classify relation:

       unrelated
       duplicate
       compatible
       conflicting
       uncertain

5. for compatible facts, run fresh Pi synthesis:

       produce the narrowest statement supported by supplied facts/evidence

   output:

       MERGED <candidate>
       CANNOT_MERGE

6. verify MERGED candidate with a separate fresh Pi verifier;

7. only SUPPORTED verified candidate becomes common fact;

8. preserve original facts;

9. record source_record_ids and provenance.


======================================================================
55. FACT CONFLICTS
======================================================================

For conflicting facts:

1. preserve the conflict;
2. retrieve current evidence where applicable;
3. verify each claim separately;
4. resolve only when evidence clearly supports one/current formulation;
5. otherwise retain unresolved conflict.

Never instruct an LLM to invent a compromise merely to reduce record count.


======================================================================
56. LESSON CONSOLIDATION
======================================================================

For compatible lessons:

1. union source_observation_ids;
2. deduplicate IDs;
3. load original observations;
4. recompute statistics deterministically;
5. recompute routing hint;
6. optionally generate concise wording;
7. create common lesson;
8. preserve original lessons and observations.


Never calculate:

    merged_support = support_A + support_B

because observation sets may overlap.


======================================================================
57. REVIEW HISTORY
======================================================================

Local SQLite review history may store:

    review request descriptors
    snapshot
    routing scores
    selected reviewers
    model profiles
    thinking levels
    timings
    structured reviewer reports
    final findings
    finding fingerprints
    feedback
    pending facts
    diagnostics


This supports:

- debugging;
- evaluation;
- future optimization.

It is not shared durable project knowledge.


======================================================================
58. MAIN REVIEW PIPELINE
======================================================================

Every review executes:

1. discover repository/worktree;

2. resolve target snapshot;

3. refresh source index;

4. derive deterministic request/diff descriptors;

5. retrieve preliminary project facts;

6. build RoutingInput;

7. route specialists;

8. enforce jury rules deterministically;

9. build ReviewerExecutionSpec values;

10. construct strong initial reviewer contexts;

11. start Pi host;

12. execute reviewers independently;

13. validate reports/findings;

14. build finding-group candidates;

15. run narrow equivalence checks where required;

16. synthesize existing finding groups only;

17. construct ReviewResult;

18. persist local review event;

19. validate/verify fact candidates;

20. persist supported facts or pending candidates;

21. perform deterministic learning maintenance;

22. return ReviewResult.


This order is stable.


======================================================================
59. PLAN REVIEW
======================================================================

CLI:

    nisse review plan --file <path>
    nisse review plan --stdin
    nisse review plan ... --json


Default snapshot:

    current working tree


Plan review may use evidence from:

- plan sections;
- relevant source;
- project facts;
- project lessons;
- practices.

The plan itself is always primary review input.


======================================================================
60. CODE REVIEW
======================================================================

CLI:

    nisse review code --working
    nisse review code --staged
    nisse review code --commit <oid>
    nisse review code --range <base>..<head>
    nisse review code --branch <base-ref>


Prioritize:

- actual diff;
- affected definitions;
- related symbols;
- relevant facts;
- relevant practices.


======================================================================
61. CLI CONTRACT
======================================================================

Required:

    nisse --version

    nisse doctor

    nisse review plan --file <path>
    nisse review plan --stdin
    nisse review plan ... --json

    nisse review code --working
    nisse review code --staged
    nisse review code --commit <oid>
    nisse review code --range <base>..<head>
    nisse review code --branch <base-ref>
    nisse review code ... --json

    nisse query "<query>"
    nisse query "<query>" --json

    nisse feedback <review-id> <finding-id> --fixed
    nisse feedback <review-id> <finding-id> --accepted
    nisse feedback <review-id> <finding-id> --dismissed

    nisse records list
    nisse records pending
    nisse records compact

    nisse cache status
    nisse cache clear

    nisse eval routing
    nisse eval retrieval

    nisse mcp


======================================================================
62. PRIMARY PI PACKAGE
======================================================================

Ship under:

    integrations/pi/


Register Main Pi tools:

    nisse_review_plan
    nisse_review_code
    nisse_query_project
    nisse_feedback


The extension is thin.

It invokes Nisse using structured machine-readable I/O.

No Nisse business logic is duplicated in the extension.


======================================================================
63. PI SKILL AND WORKFLOW POLICY
======================================================================

Ship:

    integrations/pi/skills/nisse/SKILL.md


The skill defines:

- substantial vs trivial task guidance;
- plan-review workflow;
- implementation workflow;
- code-review/fix workflow;
- review attempt limits;
- feedback semantics.


The permanent always-loaded description should remain short.

Detailed workflow instructions belong in the skill.


======================================================================
64. PI PLAN-REVIEW WORKFLOW
======================================================================

For a substantial task:

1. Main Pi creates a concrete plan.

2. Invoke:

       nisse_review_plan

3. Inspect blocker/high findings.

4. Revise plan for findings judged valid.

5. Repeat review.

Maximum:

    3 plan-review attempts


Exit successfully when:

    no unresolved blocker/high findings remain.


If attempt 3 still has blocker/high findings:

    stop;
    explain unresolved findings;
    request user direction;
    do not begin implementation automatically.


======================================================================
65. PI CODE-REVIEW WORKFLOW
======================================================================

After implementation and project tests:

1. invoke:

       nisse_review_code

2. investigate blocker/high findings;

3. fix valid findings;

4. run relevant project tests;

5. submit known feedback;

6. repeat code review.


Maximum:

    3 code-review attempts


Exit successfully when:

    no unresolved blocker/high findings remain.


If attempt 3 still contains blocker/high findings:

    stop;
    report unresolved issues;
    do not claim completion.


======================================================================
66. PI FEEDBACK WORKFLOW
======================================================================

If Main Pi knows a finding was fixed:

    submit fixed


If Main Pi accepts validity but deliberately leaves it unresolved:

    submit accepted


If Main Pi investigates and rejects the finding:

    submit dismissed


If outcome is unclear:

    submit nothing


Repeated submissions must be idempotent.


======================================================================
67. MCP
======================================================================

MCP is secondary generic integration.

Command:

    nisse mcp


Transport:

    stdio


Tools:

    review_plan
    review_code
    query_project
    record_feedback


MCP calls the same core APIs as CLI and Pi integration.


======================================================================
68. WINDOWS AND VISUAL STUDIO
======================================================================

Support Windows x86_64.

Audit:

- filesystem paths;
- spaces;
- Unicode;
- CRLF;
- Git;
- SQLite;
- subprocess JSONL;
- atomic writes;
- config paths;
- worktrees.


Provide:

    integrations/visual-studio/README.md


Visual Studio/GitHub Copilot integration uses:

    nisse mcp


No Visual Studio-specific behavior belongs in core.


======================================================================
69. EVALUATION
======================================================================

Routing fixture:

    input
    expected_required_reviewers[]
    expected_forbidden_reviewers[]
    acceptable_any_groups[]


Metrics:

    required reviewer recall
    forbidden reviewer rate
    average jury size
    dynamic-domain fallback rate
    latency


Retrieval fixture:

    query
    snapshot
    required_sources[]
    forbidden_sources[]


Metrics:

    Recall@K
    MRR where useful
    snapshot leakage count
    latency


Nisse 1.0 records evaluation data but does not train models.


======================================================================
70. FUTURE MODEL OPTIMIZATION
======================================================================

ReviewerExecutionSpec contains:

    model_profile
    thinking_level


Review history records:

    reviewer
    model profile
    thinking level
    latency
    findings
    feedback


This allows later evidence-based optimization.

Automatic model-policy learning is outside 1.0.

Do not implement it before 1.0.


======================================================================
71. PERMANENT INVARIANTS
======================================================================

1. Git snapshot is authoritative for source truth.

2. Source access always resolves against target snapshot.

3. Cache presence never proves snapshot membership.

4. SQLite loss does not destroy durable shared knowledge.

5. Durable records preserve provenance.

6. Original observations remain available.

7. Lesson statistics use unique original observation IDs.

8. An LLM cannot directly persist durable facts.

9. Fact persistence requires independent semantic verification.

10. Fact consolidation requires independent verification.

11. Synthesis cannot create new underlying findings.

12. Quality reviewer always participates.

13. Jury size remains 2–4.

14. Rules-based routing always exists.

15. Needle remains optional.

16. Embeddings remain optional.

17. Reviewer source access is read-only.

18. Reviewer sessions are isolated.

19. Main Pi context is not inherited by reviewers.

20. Reviewer model selection is explicit.

21. Uncertainty produces less automation rather than guessed truth.

22. Current source evidence outranks stale memory.

23. Nisse does not modify application source.

24. Normal operation needs no continuously running daemon.

25. CLI, Pi integration, and MCP use the same Nisse core.

26. Nisse Core has operations, not Plan/Build modes.

27. Plan/code review loops belong to the Pi integration workflow.

28. Both Pi review loops are bounded to three attempts.

29. Main Pi does not proceed through unresolved blocker/high findings after a
    loop limit without user direction.


======================================================================
MILESTONE 01 — FOUNDATION
======================================================================

GOAL
----

Create project foundation and frozen basic domain primitives.


IMPLEMENT
---------

1. Initialize Git if necessary.

2. Create Rust package.

3. Detect installed stable Rust version and pin the exact version in:

       rust-toolchain.toml

4. Create major source-tree directories.

5. Add only dependencies needed by this milestone.

6. Implement:

       ReviewKind
       Severity
       ImpactClass
       EvidenceRef
       ReviewerFinding
       ReviewerReport
       FinalFinding
       ReviewResult
       SnapshotDescriptor

7. Implement:

       schema_version = 1
       ULID helper
       UTC RFC3339 timestamp helper

8. Create App root.

9. Configure tracing.

10. Create full frozen CLI parser.

11. Future commands return typed NotImplemented errors.

12. Implement:

       nisse --version

13. Create:

       docs/architecture-contract.md

    as a concise architectural summary.


TESTS
-----

- serde roundtrip;
- enum rejection;
- CLI parsing;
- version output;
- ULID sanity;
- timestamp handling;
- stdout/stderr behavior.


ACCEPTANCE
----------

- build succeeds;
- frozen commands parse;
- version works;
- tests pass;
- self-review passes.


COMMIT
------

    nisse: complete milestone 01 - foundation


STOP.


======================================================================
MILESTONE 02 — CONFIGURATION AND TRUST
======================================================================

GOAL
----

Implement trusted local configuration before provider execution.


IMPLEMENT
---------

1. Platform-native user config path.

2. Config structures:

       network
       model profiles
       classifier
       embeddings
       learning
       contributor identity

3. Frozen precedence.

4. Endpoint validation.

5. Hostname/IP resolution before endpoint trust decision.

6. Validate private/loopback address classes.

7. Contributor slug validation.

8. Implement:

       nisse doctor

9. Doctor reports:

       Git availability
       configuration validity
       model profiles
       endpoint trust
       optional classifier status


No actual LLM request.


TESTS
-----

- precedence;
- malformed config;
- IPv4;
- IPv6;
- public rejection;
- explicit override;
- contributor IDs.


ACCEPTANCE
----------

Configuration/trust policy is deterministic and tested.


COMMIT
------

    nisse: complete milestone 02 - configuration and trust


STOP.


======================================================================
MILESTONE 03 — GIT AND WORKTREES
======================================================================

GOAL
----

Implement authoritative repository/worktree discovery.


IMPLEMENT
---------

Git command runner with:

    cwd
    timeout
    stdout
    stderr
    typed failures


RepoContext:

    worktree_root
    git_dir
    git_common_dir
    head
    branch
    detached
    unborn


Helpers:

    resolve commit-ish
    merge-base
    empty tree
    tracked files
    untracked non-ignored
    index listing
    commit-tree listing


TESTS
-----

Temporary real repositories:

- normal;
- detached HEAD;
- unborn;
- linked worktrees;
- shared common dir;
- separate worktree git dirs;
- merge-base.


ACCEPTANCE
----------

Repository/worktree semantics are correct.


COMMIT
------

    nisse: complete milestone 03 - git and worktrees


STOP.


======================================================================
MILESTONE 04 — SNAPSHOTS AND DIFFS
======================================================================

GOAL
----

Implement exact review snapshot semantics.


IMPLEMENT
---------

Snapshot modes:

    working
    staged
    commit
    range
    branch


Source readers:

    working final state
    Git index
    Git objects/trees


Diff handling:

    staged
    unstaged
    both same file
    untracked addition
    deletion
    rename
    binary
    root commit
    range
    branch merge-base


Exclude `.nisse/**` by default.


TESTS
-----

Cover every mode.

Include linked worktrees with different same-path content.


ACCEPTANCE
----------

Every review scope resolves exact expected target state.


COMMIT
------

    nisse: complete milestone 04 - snapshots and diffs


STOP.


======================================================================
MILESTONE 05 — LOCAL SQLITE STORAGE
======================================================================

GOAL
----

Implement regenerable local storage.


IMPLEMENT
---------

Shared:

    $GIT_COMMON_DIR/nisse/cache.sqlite

Worktree:

    $GIT_DIR/nisse/worktree.sqlite


Implement:

- schema version;
- migrations;
- required tables;
- cache status;
- cache clear;
- corruption recovery.


TESTS
-----

- initialization;
- reopen;
- migrations;
- clear/recreate;
- shared worktree cache;
- private worktree DB;
- corruption handling.


ACCEPTANCE
----------

Generated storage is rebuildable and worktree-safe.


COMMIT
------

    nisse: complete milestone 05 - local sqlite storage


STOP.


======================================================================
MILESTONE 06 — DURABLE RECORDS AND IDENTITY
======================================================================

GOAL
----

Implement immutable merge-friendly project records.


IMPLEMENT
---------

Record kinds:

    observation
    fact
    lesson


Paths:

    .nisse/people/<id>/<ULID>.json
    .nisse/common/<ULID>.json


Implement:

- record schemas;
- atomic writes;
- immutable create;
- loading;
- validation;
- supersedes;
- fact state;
- contributor identity;
- interactive identity prompt;
- noninteractive pending behavior.


Implement:

    nisse records list


TESTS
-----

- each record kind;
- invalid schema;
- path traversal;
- immutable behavior;
- two-branch merge;
- two-worktree record creation.


ACCEPTANCE
----------

Records work independently of SQLite and merge safely.


COMMIT
------

    nisse: complete milestone 06 - durable project records


STOP.


======================================================================
MILESTONE 07 — REVIEWER CATALOG AND PRACTICES
======================================================================

GOAL
----

Create explicit reviewer methodology suitable for weak LLMs.


IMPLEMENT
---------

Create every reviewer asset.

Every profile includes:

    ID
    description
    triggers
    hints
    checklist
    base instructions
    capabilities
    model profile
    thinking level


Create practice packs.

Bundle assets with Nisse.

Implement catalog validation.


TESTS
-----

- IDs unique;
- quality exists;
- dynamic expert exists;
- checklist nonempty;
- practice references valid;
- capability names valid.


ACCEPTANCE
----------

Reviewers do not need to invent their methodology.


COMMIT
------

    nisse: complete milestone 07 - reviewers and practices


STOP.


======================================================================
MILESTONE 08 — CONTENT-ADDRESSED SOURCE INDEX
======================================================================

GOAL
----

Implement deterministic incremental indexing.


IMPLEMENT
---------

LanguageIndexer abstraction.

Tree-sitter support:

    C
    C++
    C#
    Dart
    Rust
    JavaScript
    TypeScript


Implement ParsedArtifact.

Implement content keys.

Implement incremental refresh/manifests.

Cache parsed artifacts.


TESTS
-----

- initial large fixture;
- one modification -> one parse;
- revert -> cached reuse;
- worktree reuse;
- removed paths.


ACCEPTANCE
----------

Index is incremental and content-addressed.


COMMIT
------

    nisse: complete milestone 08 - source index


STOP.


======================================================================
MILESTONE 09 — BASELINE RETRIEVAL
======================================================================

GOAL
----

Create useful retrieval without embeddings.


IMPLEMENT
---------

SQLite FTS for:

    source skeletons
    selected chunks
    project records
    practices


Implement:

    symbol lookup
    relation graph
    RetrievalQuery
    ContextBundle
    context budgets


Implement:

    nisse query


TESTS
-----

- symbol retrieval;
- text retrieval;
- fact retrieval;
- related source;
- snapshot isolation;
- zero branch leakage.


ACCEPTANCE
----------

Useful RAG exists without LLMs/embeddings.


COMMIT
------

    nisse: complete milestone 09 - baseline retrieval


STOP.


======================================================================
MILESTONE 10 — DETERMINISTIC ROUTING
======================================================================

GOAL
----

Create useful jury selection without LLM routing.


IMPLEMENT
---------

RoutingInput.

RulesClassifier.

Signals:

    trigger
    language
    path
    tags
    lessons


Enforce:

    quality
    max 3 specialists
    max 4 total
    dynamic expert fallback


Fixtures:

    Cortex-M ISR/queue
    SQL migration
    OAuth
    BLE framing
    maintainability refactor
    performance
    unknown niche domain


TESTS
-----

Fixture expectations.


ACCEPTANCE
----------

Rules alone produce sensible bounded juries.


COMMIT
------

    nisse: complete milestone 10 - deterministic routing


STOP.


======================================================================
MILESTONE 11 — PI HOST PROTOCOL
======================================================================

GOAL
----

Implement Rust/TypeScript process boundary before real reviewer execution.


IMPLEMENT
---------

Create `runtime/pi-host`.

Define matching Rust and TypeScript protocol schemas.

Implement child-process handling.

Support:

    host_ready
    start_review
    tool_request
    tool_response
    cancel
    host_error


Use fake reviewer runtime for tests.

Do not integrate real Pi sessions yet.


TESTS
-----

- startup;
- protocol version;
- request correlation;
- malformed JSON;
- timeout;
- child crash;
- stderr isolation;
- stdout purity.


ACCEPTANCE
----------

Bidirectional structured protocol is reliable.


COMMIT
------

    nisse: complete milestone 11 - pi host protocol


STOP.


======================================================================
MILESTONE 12 — PI REVIEWER EXECUTION
======================================================================

GOAL
----

Execute constrained isolated reviewer sessions with Pi SDK.


IMPLEMENT
---------

Map ReviewerExecutionSpec to Pi AgentSession.

Use in-memory sessions.

Explicit:

    model
    thinking
    instructions
    checklist
    tools
    context


Prevent unintended resource inheritance.

Implement strict ReviewerReport parsing.

One repair attempt.

Timeout.

One reviewer retry.

Provide mocks for tests.


TESTS
-----

- spec mapping;
- isolation;
- schema;
- malformed result;
- repair;
- timeout;
- failure/retry.


ACCEPTANCE
----------

Pi host runs isolated structured reviewer jobs.


COMMIT
------

    nisse: complete milestone 12 - pi reviewer execution


STOP.


======================================================================
MILESTONE 13 — REVIEWER READ-ONLY TOOLS
======================================================================

GOAL
----

Provide snapshot-safe additional investigation.


IMPLEMENT
---------

Tools:

    project_lookup
    symbol_lookup
    source_lookup
    relation_lookup


Pi host forwards requests to Rust.

Rust resolves exact snapshot.

Enforce:

    capability allowlist
    lookup count
    returned context budget


TESTS
-----

- each tool;
- denied capability;
- exhausted budget;
- historical snapshot;
- worktree isolation.


ACCEPTANCE
----------

Reviewer investigation is read-only and snapshot-correct.


COMMIT
------

    nisse: complete milestone 13 - reviewer tools


STOP.


======================================================================
MILESTONE 14 — FINDING VALIDATION AND SYNTHESIS
======================================================================

GOAL
----

Ensure weak reviewers/synthesizers cannot corrupt final findings.


IMPLEMENT
---------

Implement:

    finding validation
    evidence validation
    severity clamp
    fingerprinting
    deterministic duplicate grouping
    narrow Pi equivalence check
    synthesis session
    synthesis output validation


Synthesis may only reference source finding IDs.


TESTS
-----

- duplicate grouping;
- SAME/DIFFERENT/UNCERTAIN;
- invalid evidence;
- severity clamp;
- invented synthesis finding rejected;
- cross-validation rules.


ACCEPTANCE
----------

Synthesis cannot invent review issues.


COMMIT
------

    nisse: complete milestone 14 - finding synthesis


STOP.


======================================================================
MILESTONE 15 — PLAN REVIEW
======================================================================

GOAL
----

Complete end-to-end plan review.


IMPLEMENT
---------

Commands:

    review plan --file
    review plan --stdin
    --json


Use:

    snapshot
    source refresh
    retrieval
    deterministic routing
    reviewer contexts
    Pi jury
    validation
    synthesis
    local review event


Implement constrained dynamic-domain description.


TESTS
-----

- file;
- stdin;
- JSON;
- normal routing;
- dynamic fallback;
- quality always;
- jury bounds;
- reviewer failures;
- context evidence.


ACCEPTANCE
----------

Plan review works without Needle.


COMMIT
------

    nisse: complete milestone 15 - plan review


STOP.


======================================================================
MILESTONE 16 — CODE REVIEW
======================================================================

GOAL
----

Complete code review for every snapshot mode.


IMPLEMENT
---------

Support:

    working
    staged
    commit
    range
    branch


Use exact snapshot/index/retrieval semantics.

Context starts from real diff plus related source.


TESTS
-----

End-to-end mocked tests for every mode.

Include:

    linked worktrees
    historical commits


ACCEPTANCE
----------

Code review works with zero snapshot leakage.


COMMIT
------

    nisse: complete milestone 16 - code review


STOP.


======================================================================
MILESTONE 17 — OPTIONAL NEEDLE
======================================================================

GOAL
----

Add optional routing/embedding improvements.


IMPLEMENT
---------

Stable classifier provider.

Needle adapter operations:

    route
    embed


Implement:

    timeout
    malformed handling
    reviewer-ID validation
    Rules fallback
    embedding cache
    cosine reranking


Needle cannot bypass deterministic jury constraints.


TESTS
-----

- fake adapter;
- crash;
- timeout;
- malformed output;
- unknown reviewer;
- fallback;
- embedding dimension/model changes.


ACCEPTANCE
----------

Needle improves optional behavior but is never mandatory.


COMMIT
------

    nisse: complete milestone 17 - needle integration


STOP.


======================================================================
MILESTONE 18 — FACT CANDIDATES AND VERIFICATION
======================================================================

GOAL
----

Learn facts without trusting reviewer claims.


IMPLEMENT
---------

Candidate validation.

Deterministic fact checks.

Independent Pi verification:

    SUPPORTED
    CONTRADICTED
    INSUFFICIENT


Only supported validated candidates persist.

Others remain pending/rejected.


Implement:

    nisse records pending


TESTS
-----

- supported;
- contradicted;
- insufficient;
- bad evidence;
- low confidence;
- secrets;
- duplicate;
- verifier failure;
- missing identity.


ACCEPTANCE
----------

Reviewer cannot directly persist facts.


COMMIT
------

    nisse: complete milestone 18 - fact verification


STOP.


======================================================================
MILESTONE 19 — FEEDBACK AND OBSERVATIONS
======================================================================

GOAL
----

Create permanent raw learning events.


IMPLEMENT
---------

CLI outcomes:

    fixed
    accepted
    dismissed


Generate ObservationRecord from stored review/finding.

Ensure idempotent semantic feedback.

Immutable records.


TESTS
-----

- each outcome;
- unknown review;
- unknown finding;
- duplicate submission;
- merge observations across branches.


ACCEPTANCE
----------

Known review outcomes become permanent structured observations.


COMMIT
------

    nisse: complete milestone 19 - observations and feedback


STOP.


======================================================================
MILESTONE 20 — LESSON LEARNING
======================================================================

GOAL
----

Derive routing experience deterministically.


IMPLEMENT
---------

Group observations.

Deduplicate IDs.

Compute:

    support
    positives
    dismissals
    path/tag conditions
    boost


Apply frozen threshold.

Optional Pi wording only.

RulesClassifier consumes active lessons.


TESTS
-----

- threshold;
- duplicated IDs;
- positive/negative balance;
- max boost;
- unrelated routing unaffected;
- Git sharing.


ACCEPTANCE
----------

Repeated outcomes can improve routing without LLM-controlled statistics.


COMMIT
------

    nisse: complete milestone 20 - lesson learning


STOP.


======================================================================
MILESTONE 21 — FACT STALENESS
======================================================================

GOAL
----

Prevent stale memory overriding current source.


IMPLEMENT
---------

Content-key staleness detection.

Downranking/exclusion.

Targeted revalidation:

    SUPPORTED
    CONTRADICTED
    INSUFFICIENT


Updated facts become new immutable records.


TESTS
-----

- unchanged evidence;
- modified content;
- deleted content;
- each revalidation result.


ACCEPTANCE
----------

Potentially stale facts are never silently current truth.


COMMIT
------

    nisse: complete milestone 21 - fact staleness


STOP.


======================================================================
MILESTONE 22 — KNOWLEDGE CONSOLIDATION
======================================================================

GOAL
----

Compact semantic knowledge without losing provenance.


IMPLEMENT
---------

Command:

    nisse records compact


FACTS:

    deterministic candidate grouping
    optional Needle similarity
    relation classification
    Pi candidate synthesis
    independent Pi verification
    common fact after support
    conflict handling
    originals retained


LESSONS:

    compatible grouping
    source observation union
    unique observation reload
    deterministic statistics
    deterministic boost
    optional wording
    common lesson
    originals retained


TESTS
-----

- compatible facts;
- unsafe merge;
- verifier rejection;
- unresolved conflict;
- overlapping lesson evidence;
- no double counting;
- provenance;
- Git merge.


ACCEPTANCE
----------

Semantic duplication can be reduced without loss of original evidence.


COMMIT
------

    nisse: complete milestone 22 - knowledge consolidation


STOP.


======================================================================
MILESTONE 23 — EVALUATION
======================================================================

GOAL
----

Make routing/retrieval measurable.


IMPLEMENT
---------

Commands:

    nisse eval routing
    nisse eval retrieval


Implement frozen fixtures/metrics.

Support rules and Needle comparison.

Machine-readable output.


TESTS
-----

- metric calculations;
- fixture validation;
- deterministic baseline;
- leakage counted as failure.


ACCEPTANCE
----------

Future routing/retrieval changes can be quantitatively evaluated.


COMMIT
------

    nisse: complete milestone 23 - evaluation


STOP.


======================================================================
MILESTONE 24 — PRIMARY PI PACKAGE
======================================================================

GOAL
----

Expose Nisse natively to Main Pi.


IMPLEMENT
---------

Pi Package.

Tools:

    nisse_review_plan
    nisse_review_code
    nisse_query_project
    nisse_feedback


Structured subprocess I/O.

Create Nisse skill.

Skill includes:

- substantial/trivial guidance;
- plan-review workflow;
- code-review workflow;
- three-attempt limits;
- feedback semantics.


TESTS
-----

- tool schemas;
- success;
- failure;
- JSON parsing;
- thin adapter invariant.


ACCEPTANCE
----------

Main Pi uses Nisse natively without MCP.


COMMIT
------

    nisse: complete milestone 24 - pi integration


STOP.


======================================================================
MILESTONE 25 — PI TASK WORKFLOW
======================================================================

GOAL
----

Implement/test the complete bounded Main Pi workflow policy.


IMPLEMENT
---------

Ensure the Pi skill/integration explicitly defines:

SUBSTANTIAL TASK:

    understand
    draft plan
    plan review
    revise if blocker/high
    max 3 reviews
    stop for user if unresolved

    implement
    test

    code review
    fix valid blocker/high
    retest
    max 3 reviews
    stop for user if unresolved

    finish


TRIVIAL TASK:

    implement
    test
    finish

unless user explicitly asks for Nisse.


Feedback:

    known fixed -> fixed
    known valid but deferred -> accepted
    known invalid -> dismissed
    unknown -> nothing


Do not implement a Plan/Build mode inside Nisse Core.

Add integration-level tests/fixtures proving the policy text and tool behavior
remain aligned.


ACCEPTANCE
----------

The Main Pi integration has an explicit bounded workflow and does not depend
on Pi inventing its own review loop.


COMMIT
------

    nisse: complete milestone 25 - pi task workflow


STOP.


======================================================================
MILESTONE 26 — MCP ADAPTER
======================================================================

GOAL
----

Provide generic integration.


IMPLEMENT
---------

Command:

    nisse mcp


Tools:

    review_plan
    review_code
    query_project
    record_feedback


Reuse existing core APIs.

MCP noninteractive.

stdout protocol only.


TESTS
-----

- initialize;
- tools;
- mocked reviews;
- query;
- feedback;
- stdout purity.


ACCEPTANCE
----------

MCP and Pi call the same Nisse core behavior.


COMMIT
------

    nisse: complete milestone 26 - mcp adapter


STOP.


======================================================================
MILESTONE 27 — WINDOWS AND VISUAL STUDIO
======================================================================

GOAL
----

Support Windows clients.


IMPLEMENT
---------

Audit/fix:

    paths
    spaces
    Unicode
    CRLF
    Git
    SQLite
    child JSONL
    atomic writes
    config paths
    worktrees


Create Visual Studio/GitHub Copilot MCP documentation.


TESTS
-----

Windows CI including:

    spaces
    Unicode
    worktrees
    Pi-host subprocess
    MCP subprocess


ACCEPTANCE
----------

Nisse core and MCP work on Windows x86_64.


COMMIT
------

    nisse: complete milestone 27 - windows support


STOP.


======================================================================
MILESTONE 28 — SECURITY HARDENING
======================================================================

GOAL
----

Harden trust boundaries.


AUDIT/TEST
----------

    path traversal
    malicious `.nisse` records
    oversized records
    invalid UTF-8
    symlinks
    malicious source prompt injection
    malicious plan prompt injection
    malicious fact text
    endpoint validation
    DNS resolution
    command injection
    protocol injection
    child crash
    stdout contamination
    malformed Git output
    concurrent Nisse processes
    SQLite locking
    interrupted record writes


Prompts must clearly separate untrusted:

    source
    plans
    project records

from reviewer/system instructions.


ACCEPTANCE
----------

Project-controlled data cannot escape frozen trust boundaries.


COMMIT
------

    nisse: complete milestone 28 - security hardening


STOP.


======================================================================
MILESTONE 29 — PERFORMANCE HARDENING
======================================================================

GOAL
----

Optimize after correctness.


MEASURE
-------

    warm no-change refresh
    one-file refresh
    retrieval
    routing
    Pi-host startup
    context size
    SQLite growth
    record loading


Target warm no-change refresh:

    < 100 ms

for ordinary local repositories when Git/filesystem are responsive.


VERIFY
------

- unchanged content not reparsed;
- reverted blobs reuse artifacts;
- worktrees share valid artifacts;
- context remains bounded;
- reviewer concurrency obeys config;
- ordinary review does not perform O(N²) work over all observations.


Do not change public contracts for performance.


ACCEPTANCE
----------

Routine performance is suitable for interactive coding use.


COMMIT
------

    nisse: complete milestone 29 - performance hardening


STOP.


======================================================================
MILESTONE 30 — RELEASE AND FINAL REGRESSION
======================================================================

GOAL
----

Complete Nisse 1.0 without adding new architecture.


DOCUMENTATION
-------------

README covers:

    purpose
    architecture
    installation
    Pi integration
    Main Pi workflow
    model configuration
    local/LAN trust model
    CLI
    project knowledge
    worktrees
    Needle
    MCP
    Windows/Visual Studio


RELEASE BUILDS
--------------

Where infrastructure supports:

    macOS arm64
    macOS x86_64
    Linux x86_64
    Linux arm64
    Windows x86_64


FINAL REGRESSION SCENARIOS
--------------------------

A. PLAN REVIEW

Substantial plan receives dynamic jury review.


B. MAIN PI PLAN LOOP

A mocked first plan review returns HIGH.

Pi workflow requires plan revision and second review.

Second review has no HIGH/BLOCKER.

Implementation may proceed.


C. MAIN PI PLAN LOOP LIMIT

Three reviews retain HIGH/BLOCKER.

Workflow stops for user direction.

Implementation does not begin automatically.


D. WORKING CODE REVIEW

Modified working tree receives review.


E. CODE REVIEW LOOP

First code review finds HIGH.

Main workflow fixes, tests, reviews again.


F. CODE REVIEW LOOP LIMIT

After three attempts blocker/high remains.

Workflow does not report task complete.


G. STAGED REVIEW

Index is reviewed independently of unstaged state.


H. HISTORICAL REVIEW

Historical commit review never reads current filesystem as target source.


I. WORKTREES

Different same-path contents do not leak.

Valid cache artifacts are reused.


J. RULES-ONLY

Needle unavailable.

Reviews still work.


K. NEEDLE FALLBACK

Needle works, then fails.

Rules fallback works.


L. FACT LEARNING

Candidate verified and persisted.


M. UNSUPPORTED FACT

Bad candidate blocked by verification.


N. STALE FACT

Changed source downranks/revalidates old fact.


O. FEEDBACK

Fixed finding creates observation.


P. LESSON

Sufficient observations create deterministic lesson.


Q. LESSON MERGE

Overlapping source observations are counted once.


R. FACT CONSOLIDATION

Compatible facts produce verified common fact.


S. UNSAFE CONSOLIDATION

Incompatible facts remain separate/conflicting.


T. CACHE LOSS

Delete generated SQLite state.

Durable records survive and cache rebuilds.


U. TEAM GIT MERGE

Different contributor records survive normal branch merge.


V. PI INTEGRATION

Main Pi tools invoke Nisse.


W. MCP

Generic client gets equivalent operations.


X. WINDOWS

Windows regression suite passes.


Y. MALFORMED REVIEWER

One repair fails.

Reviewer failure does not corrupt state.


Z. HALLUCINATING SYNTHESIZER

Unknown source finding is rejected.


AA. HALLUCINATING FACT REVIEWER

Unsupported fact does not persist.


AB. TRIVIAL PI TASK

Normal Pi workflow is allowed to skip Nisse.


AC. CORE MODE INVARIANT

Nisse Core contains no Plan/Build task mode or task workflow state machine.


FINAL QUALITY GATES
-------------------

Run all Rust gates.

Run all TypeScript gates.

Run integration tests.

Run evaluation baseline.

Run security tests.

Run:

    git diff --check


FINAL SELF-REVIEW
-----------------

Verify every permanent invariant.

Verify frozen CLI operations.

Verify milestone acceptance criteria.

Verify architecture boundaries.

Verify Needle is optional.

Verify no durable fact path trusts one reviewer claim directly.

Verify synthesis cannot create source-less findings.

Verify original observations remain available.

Verify worktree/snapshot isolation.

Verify Main Pi bounded plan/code workflows.

Verify Nisse Core contains no Plan/Build workflow state.


RELEASE CONDITION
-----------------

Nisse 1.0 is complete only when all mandatory regression scenarios and quality
gates pass.


COMMIT
------

    nisse: complete milestone 30 - nisse 1.0


STOP.


======================================================================
FINAL ARCHITECTURAL SUMMARY
======================================================================

RUST / NISSE CORE
-----------------

Owns:

    Git
    worktrees
    snapshots
    diffs
    indexing
    retrieval
    records
    observations
    lesson statistics
    routing constraints
    JuryPlan
    reviewer context
    evidence validation
    severity policy
    finding provenance
    orchestration
    persistence


Does not own:

    coding workflow modes
    implementation
    task management
    Plan/Build state


PI REVIEW HOST
--------------

Owns:

    isolated semantic reviewer sessions
    narrow equivalence checks
    dynamic-domain description
    fact verification
    fact synthesis
    constrained wording/synthesis


NEEDLE
------

Owns only optional:

    fast classification
    semantic grouping
    embeddings


GIT
---

Owns:

    source history
    source snapshot semantics
    transport for shared durable records


SQLITE
------

Stores regenerable:

    source index
    retrieval cache
    embedding cache
    local review history
    pending learning


DURABLE RECORDS
---------------

    observation = what happened

    fact        = what supported evidence says about the project

    lesson      = what repeated observations suggest is useful


MAIN PI
-------

Owns:

    normal developer conversation
    task understanding
    plan creation
    plan revision
    implementation
    project tests
    handling Nisse findings
    known finding outcomes


MAIN PI TASK WORKFLOW
---------------------

For substantial work:

    understand
        |
        v
    draft plan
        |
        v
    Nisse plan review
        |
        +-- blocker/high -> revise -> review again
        |                   max 3
        |
        +-- still serious after 3 -> USER
        |
        v
    implement
        |
        v
    project tests
        |
        v
    Nisse code review
        |
        +-- blocker/high -> fix -> test -> review again
        |                  max 3
        |
        +-- still serious after 3 -> USER
        |
        v
    done


For trivial work:

    understand
        |
        v
    implement
        |
        v
    test
        |
        v
    done


NISSE CORE OPERATIONS
---------------------

Nisse Core exposes:

    review_plan
    review_code
    query_project
    record_feedback


It does not expose or maintain:

    Plan mode
    Build mode


CENTRAL DESIGN RULE
-------------------

Every decision that can reasonably be made deterministically belongs in
Nisse Core.

LLMs receive narrow semantic tasks with:

    explicit inputs
    explicit schemas
    explicit capabilities
    bounded context
    independently checkable output


A stronger LLM should improve review quality.

A weaker LLM must not corrupt:

    Git state
    snapshot correctness
    routing constraints
    lesson statistics
    project memory
    durable knowledge
    workflow termination


======================================================================
DEFINITION OF "IMPLEMENT THE NEXT MILESTONE"
======================================================================

When instructed:

    Implement the next milestone.

Pi performs exactly:

    inspect Git state
        |
        v
    identify highest committed complete milestone
        |
        v
    select exactly the next milestone
        |
        v
    implement only that milestone
        |
        v
    run tests
        |
        v
    review against this document
        |
        v
    fix all deviations
        |
        v
    run tests again
        |
        v
    review again
        |
        v
    stage only milestone work
        |
        v
    commit milestone
        |
        v
    verify working tree
        |
        v
    report
        |
        v
    STOP


Never continue directly into the following milestone.


END OF NISSE 1.0 FROZEN IMPLEMENTATION PLAN
```
