# Changelog

All notable changes to **mcp-flowgate** are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
on the cargo crate version. The **config schema** is versioned
separately — see [`STABILITY.md`](STABILITY.md) for what is and isn't
covered by a stability commitment.

## [0.2.0] - 2026-05-25

A substantial additive release. Adds the skills / typed-blackboard /
versioned-definitions surfaces from SPEC §5 and §17–§20, ships the
`mcp-flowgate-tui` crate, and closes the v0.2 audit punch list. No
breaking changes to the v0.1 wire surface; every existing config
loads and every existing workflow runs unchanged.

### Added

- **Typed skills surface (SPEC §5).** Workflows can declare a
  `skills:` block of guidance fragments addressable by
  `verb`/`subject` (e.g. `verb: review, subject: review.style.house-voice`).
  Subjects are stamped into each running workflow snapshot
  (`_skillsLibrary`) so an in-flight instance sees the body that
  existed at `workflow.start`, not whatever the live config
  currently says. Bodies are fetched on demand via
  `gateway.describe(id, workflowId)` — progressive disclosure (§5.4).
- **`gateway.skills.search`** (SPEC §17.6) — authoring-time tool that
  returns guidance refs (never bodies) filterable by
  `verb` / `subject_root` / `source`. Advertised only when
  `FlowgateServer::with_skills_search(true)` is set; default off so
  runtime workflows use the push-not-pull guidance surface (§5.4).
- **`guidance_acknowledged` guard** (SPEC §5.9). Optional
  `GuidanceAcknowledgmentStore` records which subjects a workflow
  has `gateway.describe`d. The guard returns true iff the current
  body's hash matches what was acknowledged — hash-flip
  invalidation means a future edit to the body silently expires
  the acknowledgment.
- **Trace/run id plumbing** (SPEC §20.2). `workflow.start`,
  `workflow.get`, and `workflow.submit` accept `traceId` /
  `runId`. The instance persists trace id on first set; every
  audit record for that workflow propagates the values. Run id can
  override per-call.
- **§20.4 error codes** from the `evidence` guard. Filter rejections
  now surface as `EVIDENCE_DIGEST_REQUIRED` and
  `EVIDENCE_CONFIDENCE_BELOW_THRESHOLD` instead of the generic
  `GUARD_REJECTED`. `Evidence::validate_confidence()` raises
  `INVALID_CONFIDENCE` at submit time for out-of-range
  `confidence` values.
- **§20.1 evidence enrichment.** `Evidence` carries optional
  `summary`, `digest`, and `confidence` fields; gateway preserves
  and propagates them through transition records.
- **`audit.write_failed` self-events** (SPEC §5.8). Non-critical
  audit sites that previously swallowed sink errors now emit an
  observable self-event when the primary record fails. Critical
  audit sites (record-first emissions per §7.3) continue to fail
  fast — chain auto-execution sites at `runtime_chain.rs` are
  classified per criticality table.
- **`mcp-flowgate-tui` crate** — terminal UI that spawns
  `mcp-flowgate` as a child MCP server and drives a workflow
  interactively. Installs two binary aliases: `flowgate` (primary)
  and `flowgate-tui` (long-form). Log directory defaults to
  `dirs::cache_dir().join("flowgate/logs")` with
  `FLOWGATE_LOG_DIR` override; binary discovery honors
  `MCP_FLOWGATE_PATH` env var and falls back through sibling +
  PATH lookup with actionable error messages.
- **`CONFIG_FLAG_NOT_RUNTIME_MUTABLE` validator.** Config load
  rejects nested `flowgate.*` keys inside `workflows.*` /
  `states.*` / `transitions.*` scope — those flags are gateway
  defaults only and can't be overridden per workflow.
- **`strict_namespacing` soft warnings** via new
  `config::resolve_with_diagnostics()` /
  `load_resolved_with_diagnostics()`. Unblessed subject roots
  produce `Diagnostic { severity: warn, code: INVALID_SUBJECT_ROOT, … }`
  with closest-blessed-root suggestion; surfaced via
  `mcp-flowgate check`.
- **Authoring-time `RegistryExecutor`** (SPEC §17.2 + §8.4). Behind
  the `flowgate.authoring.write_enabled` flag, registers workflow
  definitions through the `InMemoryWritableDefinitionStore`.
- **CI doctest gate** (`cargo test --workspace --doc`) with seeded
  examples on `Evidence::validate_confidence`,
  `normalize_for_hash`, and `compute_skill_hash` — any future
  spec/code drift in API examples breaks the build.
- **`examples/swe-agent.yaml`** — reference workflow demonstrating
  the skills surface with three external connections, six states,
  and use-before-def-validated planning.

### Changed

- Discovery `DiscoveryItem` carries an optional `source` field
  (SPEC §5.3). Config-declared fragments default to `"config"`;
  ingested fragments carry their provenance (e.g.
  `git+https://github.com/org/repo@sha`). Used by the
  `gateway.skills.search` source filter.
- Blessed-subject-root set expanded from 7 to 13 to include verb
  mirrors (`triage`, `diagnose`, `implement`, `refactor`,
  `explain`, `compose`) so the eight cognitive-verb mirrors of a
  subject are all valid roots.
- `mcp-flowgate check` prints soft diagnostics under their own
  banner when the resolved config carries warnings.
- Workspace structurally rebalanced: `mcp-flowgate-mcp-server/src/lib.rs`
  split into `lib.rs` (250 LOC) + `handlers.rs` + `tools.rs` +
  `args.rs` (multi-file `impl FlowgateServer` pattern matching the
  existing `runtime_*.rs` split). Three god integration test
  files (`deterministic_chain.rs`, `invariants.rs`,
  `transition_records.rs`, ~2.6k LOC total) split into 8 sibling
  files using the `tests/common/` shared-harness pattern.
- Workspace-wide unused-imports and dead-code sweep.

### Fixed

- Guard expressions can now reference `$.workflow.{id,state,version}`
  per SPEC §5.2.
- Guards fail fast on unset slots instead of silently evaluating to
  false (SPEC §9).
- Transition records carry executor descriptor `{ kind, ok,
  durationMs }` per SPEC §7.2.

### Deprecated

- The `fg` shell alias was considered for `mcp-flowgate-tui` and
  rejected — it collides with the bash `fg` (foreground) builtin.
  Use `flowgate` (primary) or `flowgate-tui` (long-form) instead.

### Added (continued — deterministic execution + discovery + hot-reload)

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
