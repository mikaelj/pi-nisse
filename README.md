# Nisse

Nisse is a local software-review engine.

Nisse Core exposes four operations:

- `review_plan` — review a plan produced by another agent or human
- `review_code` — review code changes produced by another agent or human
- `query_project` — query indexed project knowledge
- `record_feedback` — record known review outcomes as durable observations

Nisse does not implement the reviewed work itself. It has no Plan mode or
Build mode; workflow policy belongs to coding-agent integrations such as the
primary Pi package.

Status: implementing the frozen plan in [PLAN.md](PLAN.md).

Architecture summary: [docs/architecture-contract.md](docs/architecture-contract.md).
