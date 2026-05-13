# Changelog

All notable changes to **mcp-flowgate** are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
on the cargo crate version. The **config schema** is versioned
separately — see [`STABILITY.md`](STABILITY.md) for what is and isn't
covered by a stability commitment.

## [Unreleased]

### Added

- **Deterministic chaining.** Transitions tagged `actor: "deterministic"`
  auto-execute without LLM involvement. When a state has only
  deterministic transitions, the runtime chains through them
  automatically until it hits a decision point, terminal state, depth
  limit, or failure. Responses include a `chain` array tracing the
  auto-executed steps. On chain failure, the failed transition surfaces
  as a recovery link. `maxChainDepth` (default 50) caps chain length.
  Config schema adds `"deterministic"` to the actor enum and
  `maxChainDepth` to workflow definitions.
- **Phase guidance.** States can declare `goal` and `guidance` strings
  that surface in every workflow response as a `guidance` object with
  `goal` and `instructions` fields. Complements `prefill` (which
  pre-shapes arguments) by pre-shaping the LLM's reasoning about what
  to do at each step. Both fields are indexed by `gateway.search`.
- `workflow.explain` now includes `actor` and `deterministic` fields in
  its response, showing the actor type and whether the transition is
  deterministic.
- `examples/deploy-pipeline/` — worked example demonstrating
  deterministic chaining: lint → test → build auto-execute, LLM only
  sees the deploy decision.
- 16 new tests in `crates/mcp-flowgate-core/tests/deterministic_chain.rs`
  covering chain execution, mixed-state stop, depth limits, failure
  recovery, phase guidance, audit events, and edge cases.
- Response schema (`schemas/workflow-response.schema.json`) adds
  `chain`, `guidance`, and `chainStep` definitions.
- Guard kind `expr` replaces `jsonpath`. The evaluator is unchanged —
  it handles `<operand> <op> <operand>` binary predicates, not
  JSONPath. The new name is honest about what it does.
- `expr` guards now support bracket array index syntax in paths
  (e.g. `$.context.items[0].name`).
- `mcp-flowgate migrate` rewrites `kind: jsonpath` → `kind: expr` in
  YAML config files.
- **Discovery search improvements.** `gateway.search` now supports
  prefix matching, trigram-based fuzzy matching, and a new `aliases`
  field on capabilities/workflows for author-declared synonyms.
  A search for "deploy" now finds a capability named `release.promote`
  if it declares `aliases: ["deploy"]`.
- **Workflow graph static analysis.** `mcp-flowgate check` now validates
  workflow graph integrity: unreachable states, dangling transition
  targets, dead-end non-terminal states, branch target validation,
  initialState existence, and onTimeout target existence. Errors cause
  a non-zero exit; warnings are informational.
- **Config hot-reload via SIGHUP.** Send SIGHUP to reload config
  without restarting. Definitions, executors, connections, and the
  discovery index are rebuilt and swapped atomically. In-flight
  workflows continue uninterrupted. A `config.reloaded` audit event
  is emitted on success.

### Deprecated

- Guard kind `jsonpath` is deprecated in favor of `expr`. The old name
  is still accepted but emits a warning at runtime. Use
  `mcp-flowgate migrate --config <path>` to update configs. The alias
  will be removed in a future minor release.

## [0.1.1](https://github.com/matt-cochran/mcp-flowgate/releases/tag/v0.1.1)

### Added

- CI workflow (`.github/workflows/ci.yml`) covering build, clippy, fmt,
  workspace tests, and a mechanical dogfood transcript artifact.
- `CHANGELOG.md`, `SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`,
  `CONFIDENCE.md`, `ADOPTION.md`, `STABILITY.md` — trust-signal files.
- README transcript section ("What the model sees") demonstrating the
  HATEOAS walk through the `content-publish` example.
- Runtime actor enforcement: `workflow.submit` now rejects with
  `ACTOR_MISMATCH` when a transition is tagged `actor: "human"` and
  the submitting principal lacks the `human` role
  (`Principal::HUMAN_ROLE`). Previously the actor field was advisory —
  surfaced in link responses but not enforced at submit time. The
  executor never runs and the workflow state never advances on
  rejection; a `transition.rejected` audit event is emitted with the
  `ACTOR_MISMATCH` code.
- `Principal::is_human()` helper and `Principal::HUMAN_ROLE` constant
  (`"human"`). Embedders wiring identity per request should tag human
  principals with this role; see `docs/EMBEDDING.md`.
- `BACKLOG.md` — open invitations for graduating the Postgres store to
  Tier 2 and recruiting design-partner case studies.

### Changed

- Tagline: "framework for building governed MCP interfaces" →
  "composable MCP control layer that governs how LLMs use tools".
- README "What the model sees" walkthrough updated to describe the
  `ACTOR_MISMATCH` enforcement explicitly, plus the defense-in-depth
  layering with the `human` executor and `permission` guards.
- `s03_multi_approver_quorum` stress scenario now submits approvals
  with a human principal (`Principal::HUMAN_ROLE`), matching the
  stricter actor gate.

## [0.1.0](https://github.com/matt-cochran/mcp-flowgate/releases/tag/v0.1.0) — 2026-05-10

### Added

- Initial public release.
- Five crates: `mcp-flowgate-schema`, `mcp-flowgate-core`,
  `mcp-flowgate-executors`, `mcp-flowgate-mcp-server`, `mcp-flowgate`.
- Seven-tool MCP surface: `gateway.home`, `gateway.search`,
  `gateway.describe`, `workflow.start`, `workflow.get`,
  `workflow.submit`, `workflow.explain`.
- Executors: `cli`, `rest`, `mcp`, `human`, `workflow`, `noop`.
- Stores: `memory`, `sqlite`.
- Audit sinks: `stdout`, `file`, `memory`, `null`.
- YAML config schema v1.0 with JSON Schema at
  `schemas/gateway-config.schema.json`.
- Examples: `content-publish/`, `expense-approval/`, `tdd/`,
  plus `simple-proxy.yaml`, `governed-change.yaml`,
  `import-and-discovery.yaml`.
- Docs: `CONCEPTS`, `CONFIG`, `CONNECTIONS`, `DEVELOPMENT`,
  `EMBEDDING`, `GOVERNANCE`, `INVARIANTS`, `LLM-GUIDANCE`,
  `MCP-CONTROL-ARCHITECTURE`, `STRESS-TESTS`.
