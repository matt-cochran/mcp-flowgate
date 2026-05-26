# Changelog

All notable changes to **mcp-flowgate** are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
on the cargo crate version. The **config schema** is versioned
separately — see [`STABILITY.md`](STABILITY.md) for what is and isn't
covered by a stability commitment.

## [Unreleased]

### Added — Lexicon / Ubiquitous Language primitive (SPEC §30)

- **`lexicon:` top-level config block** — typed vocabulary store
  embedded in `flowgate.yaml` (Tier 1, per-config). Each entry:
  `{definition, bounded_context?, examples?, refs?, governance?}`.
  Validated at config load (`INVALID_LEXICON_ENTRY`).
- **Snapshot stamping** — every workflow gets `_lexiconLibrary` on
  its definition snapshot, mirroring `_skillsLibrary` per SPEC §8.2.
  In-flight workflows immune to mid-flight lexicon edits.
- **Three new MCP tools** added to the always-advertised set
  (becoming 10 total): `gateway.lexicon.search`,
  `gateway.lexicon.lookup`, `gateway.lexicon.define`.
- **Governance default: `human-only`** — agents calling
  `gateway.lexicon.define` against `human-only` terms get
  `LEXICON_DEFINE_REQUIRES_HUMAN`. Workflows route through an
  `actor: human` transition to commit. `agent-may-propose` is the
  opt-in alternative for scratch / sandbox vocabularies.
- **Runtime overlay** — `gateway.lexicon.define` writes land in an
  in-memory overlay (gateway lifetime only). Operators persist by
  editing `flowgate.yaml` and reloading. `lexicon.defined` audit
  event records each successful define.
- **STABILITY Tier 1 entries** for the config shape, MCP tools, and
  governance default.
- 20 new tests in `crates/mcp-flowgate-core/tests/lexicon.rs`.
- `stable_tool_surface.rs` invariant test updated from 7 → 10 names
  (additive; the original 7 retain Tier 1 commitments).

## [0.4.0] - 2026-05-25

Substantial additive release. New executor surface, new URI schemes,
new state-machine primitives, real `flowgate walk` wiring. All
changes additive; no breaking changes to v0.3 surfaces.

### Added — Workflow primitives

- **`pipeline` executor kind** (SPEC §25) — sequential composition of
  N executor steps inside one transition; each step's `output` threads
  as the next step's `$.input`. `on_step_failure: bail | continue`.
- **`while:` loop on a state** (SPEC §26) — state-level guard
  re-evaluated after each transition; truthy → re-enter the state.
  `max_iterations:` REQUIRED. Iteration counter cleared on actual exit.
- **State-local blackboard slots** (SPEC §27) — slots may declare
  `scope: state`. Cleared on state exit; preserved across `while:`
  re-entry. Closes the long-standing SPEC §15 open question.

### Added — Parallel executor enhancements

- **Aggregator pattern** — `join: { aggregator: { kind: ..., ... } }`
  is the general form for verdict computation. Any executor kind can
  be an aggregator. `expression` is one built-in kind (inline eval);
  others dispatch through the registry.
- **`{percent: P}` join** — quorum as percentage with ceiling division.
- **`{expression: "<expr>"}` join** — operator-supplied predicate
  evaluated post-completion (sugar for aggregator).
- **`branches.where: <predicate>` filter on `for_each`** — pre-fan-out
  predicate; falsy elements dropped BEFORE branches spawn.

### Added — Script URI schemes (SPEC §22.2)

- **`https://` script URIs** — load-time blocking HTTP GET, 30 s
  timeout, sha256-verified.
- **`git+https://<host>/<repo>(.git)?@<ref>#<path>` script URIs** —
  load-time `git archive --remote` extraction. `<ref>` MUST be
  specified for reproducibility.

### Added — TUI walker end-to-end wiring

- **`flowgate walk` is wired end-to-end.** The CLI subcommand now
  spawns `mcp-flowgate` as an rmcp child, starts the workflow, drives
  it through `walk_workflow` against the real `AetherSubAgentSpawner`,
  and prints the final context. Previously printed a stderr message
  and exited 0.
- **`FlowgateChildCaller`** (production `McpToolCaller` impl) wraps
  an rmcp client over `TokioChildProcess`.

### Documentation reconciliation

- STABILITY Tier 1 entries for: noop first-class semantics, `(unset)`
  template graceful-degradation, parallel join enum, pipeline, while
  loop, state-local slots, `branches.where`.
- `docs/TUI-AGENT.md` no longer claims the spawner is a stub.
- `docs/LLM-LINK-FIDELITY.md` downgraded from "production blocker"
  framing to "open research question."
- `docs/BENCHMARK-COGNITIVE-ARCHITECTURE.md` (new) — methodology +
  cost estimate ($300-500) + runbook for the cheap-models-vs-frontier
  benchmark spike. Scaffold ready; runs need API budget.
- `docs/TUI-AGENT-DESIGN.md` archives the former WIP.md scratch pad.
  `SPEC_RESEARCH_GAPS.md` deleted (superseded by SPEC.md proper).

### Added — Parallel / fan-out execution (SPEC §24)

- **`parallel` executor kind** — fan-out N concurrent branches inside
  one transition. State machine stays singular (one state, one
  transition, one version bump, one transition record); the executor
  internally runs branches via `tokio::task::JoinSet`, bounded by an
  optional `max_concurrency` semaphore, with per-branch and total
  timeouts. Branches are any executor config (recursive): `script`,
  `mcp`, `cli`, `rest`, `workflow`, even nested `parallel`.
- **Branch sourcing** — static literal `branches: [...]` OR dynamic
  `branches: { for_each: "$.context.x", do: <executor template> }`.
  Template substitution markers `$.branch.value` and `$.branch.index`
  expand per branch.
- **Join conditions** — `all` (default), `any` (first success wins;
  siblings cancelled), `{at_least: K}` (quorum). Configurable
  `on_branch_failure: bail` (default — first failure aborts) /
  `continue` (all branches run regardless; verdict per join).
- **Aggregated output shape** — `{branches: [{ok, output, error?}],
  summary: {n, ok_count, failed_count, cancelled_count, durationMs,
  first_failure_index, max_in_flight_observed, join, verdict}}`.
  Both raw per-branch results AND structured rollup so workflows can
  guard on either.
- **Audit per-branch events** — `parallel.branch.started/.completed/
  .failed/.cancelled` plus aggregate `parallel.fanout.completed` (or
  `parallel.fanout.empty` for vacuous `for_each` cases). All carry
  parent transition's `correlation_id` + `branch_index` payload;
  three-tuple `(seq, branch_index, branch_seq)` is the canonical
  ordering.
- **Idempotency-key segmentation** (SPEC §24 F7) — branches that
  declare `idempotencyKey: true` get `:branch:<index>` appended so
  downstream dedupes per branch, not per fan-out. Stable across the
  SAME branch's retries.
- **Defensive snapshot-hash assert** — `PARALLEL_SNAPSHOT_MUTATED`
  raised if snapshot bytes diverge during fan-out. Structurally
  impossible in safe Rust; assert exists as future-regression safety
  net.
- **DOS poka-yoke** — `branches.len() >= 10` without explicit
  `max_concurrency` rejects at config-load with `INVALID_PARALLEL_CONFIG`
  naming the offending state.
- **Recursion-depth cap** — `max_recursion_depth` (default 3) on the
  parallel config; nested `parallel` beyond the cap raises
  `PARALLEL_DEPTH_EXCEEDED`.
- **`[*]` array-projection in output mapping** (`mapping.rs`) —
  `$.output.branches[*].field` plucks `field` from each element of
  `branches`, returning an array in original order. Multi-level
  wildcards supported (`$.context.groups[*].items[*].name`). Backward-
  compatible: paths without `[*]` resolve identically to before.
- **5 new error codes** (SPEC §24.9): `INVALID_PARALLEL_CONFIG`,
  `JOIN_THRESHOLD_NOT_MET`, `PARALLEL_DEPTH_EXCEEDED`,
  `PARALLEL_SNAPSHOT_MUTATED`, `PARALLEL_EXECUTOR_NOT_WIRED`.
- **`ExecuteRequest.correlation_id: Option<String>`** new field — the
  runtime threads the parent transition's correlation_id through so
  fan-out executors can emit per-branch events with the parent's
  linkage. Existing executors that don't care continue to ignore the
  field.

### Added — TUI sub-agent + walk hardening (GAP-C closure)

- **`AetherSubAgentSpawner` is no longer a stub.** Previously surfaced
  `SubAgentTimeout` unconditionally; now invokes
  `aether_cli::headless::run_headless` with a wall-clock timeout
  driven by `TuiConfig::max_sub_agent_seconds`. The trait scaffolding
  was correct; only the concrete call site changed.
- **`flowgate walk` fails loud when not wired.** The CLI subcommand
  now returns `ExitCode::FAILURE` with `WALK_NOT_WIRED` when the rmcp
  child-process `McpToolCaller` hasn't been wired (still pending),
  instead of silently returning `Ok` with an eprintln message.
  Discoverable absence > silent success.
- **Smoke test** `crates/mcp-flowgate-tui/tests/sub_agent_smoke.rs`
  covers construction + a `#[ignore]`-gated live-spawn test (requires
  `ANTHROPIC_API_KEY` and a live `mcp-flowgate` on PATH).

## [0.3.0] - 2026-05-25

A substantial additive release adding the scripts surface (SPEC §22),
the verb taxonomy expansion (skills 8 → 10, scripts 8 → 12), the
intent-layer architectural invariant (SPEC §23), authoring preferences
for LLM-driven script generation, and SPEC ↔ Rust drift detection.

Three changelog sections below cover the three feature areas, in
reverse order of landing (authoring → verb expansion → scripts) so the
most-recent context is on top.

### Added — Authoring preferences (SPEC §23.8)

- **`flowgate.authoring.preferred_script_language`** config — advisory
  signal for LLM-driven authoring workflows that generate new scripts.
  Operator declares their preferred runtime (`bash`, `python3`,
  `powershell`, `node`, anything that makes sense for their env).
  Value is free-form, not a closed enum.
- **Template substitution gains `$.flowgate.authoring.*` root.**
  Authoring skills can reference `{{$.flowgate.authoring.preferred_script_language}}`
  in their body; the resolver substitutes the operator's preference at
  render time. Missing preference → standard `(unset)` stub (same shape
  as other unresolved templates).
- **Snapshot stamping**: `flowgate.authoring` is copied onto each
  workflow's snapshot as `_authoringPrefs` at config-resolve time. SPEC
  §8.2 invariant holds — in-flight authoring workflows see the
  preferences that existed at `workflow.start`, not whatever the live
  config currently says.
- **Validation** at config load: `preferred_script_language` must be a
  non-empty string when present (`INVALID_AUTHORING_PREFERENCE` error
  code names the offending field). Absence is fine — preferences are
  optional.

### Added — Verb taxonomy expansion + intent-layer invariant (SPEC §23)

- **Two new cognitive verbs** (SPEC §5.4.1, closed enum 8 → 10):
  `research` (gather context from sources — web, local, docs) and
  `summarize` (condense). Both close the reconnaissance/condensation
  gap the original eight verbs forced into awkward fits with
  `diagnose`/`explain`.
- **Four new script verbs** (SPEC §22.3, closed enum 8 → 12):
  `inspect` (read-only local introspection), `search` (content
  discovery), `fetch` (retrieve known resource), `audit` (graded
  compliance/security/quality scan). All previously misused `run` or
  `lint`.
- **Six new blessed subject roots**: `research`, `summarize` (skills);
  `inspect`, `search`, `fetch`, `audit` (scripts). Verb-mirror pattern
  preserved.
- **SPEC §23 — Intent-layer invariant** locks the architectural rule:
  *verb taxonomy lives only on skills and scripts. The access layer
  (connections / capabilities / executors) is kind-typed. Workflows
  compose. Audit events describe. No surface gets two of these
  classifications at once.* Includes §23.7 amendment criterion for
  future closed-enum additions: documented gap, distinct semantic
  category, ≥2 example subjects.
- **SPEC ↔ Rust drift-detection test** (`spec_enum_drift.rs`) parses
  SPEC §5.4.1/§22.3/§22.4 verb and root tables, parses JSON schema
  enums, and asserts byte-equality with `Verb::ALL_TOKENS` /
  `ScriptVerb::ALL_TOKENS` / `BLESSED_*_ROOTS`. Drift between SPEC and
  Rust now fails build, naming the diverged token.
- **Tightened skill verb JSON schema** from free-form pattern to
  closed enum (the scripts schema was already closed). Schema-checking
  tools (jsonschema linters, IDEs) now catch unknown verbs at author
  time instead of waiting for config-load.

### Added — Audit descriptor enrichment for scripts (SPEC §22.6)

- **Transition record executor descriptor** now carries `subject` and
  `hash` fields when the executor is `kind: script`. Closes a gap from
  the v0.2 scripts surface plan: `scriptSubject`/`scriptHash` were
  landing only in the executor output JSON. Replay-by-hash tooling can
  now read the body identity directly from the descriptor.
- **Round-trip preserved for non-script executors**: cli/mcp/rest/noop
  descriptors stay at the legacy `{kind, ok, durationMs}` shape — the
  new fields are additive + serde `skip_serializing_if = "Option::is_none"`,
  so legacy audit consumers see no schema noise.

### Added — Scripts surface (SPEC §22)

- **Top-level `scripts:` block** — curated, hash-pinned script library
  alongside `skills:`. Each entry has `verb` (closed enum:
  build/test/deploy/format/lint/install/verify/run), `lifecycle`,
  optional `source`, and either inline `body:` OR external
  `uri + hash`. v1 supports `file://` URIs only; `https://` and
  `git+https://...@<ref>` deferred to v2.
- **`script` executor kind** — materializes the snapshot's stamped
  body to a `chmod 0700` temp file, execs via shebang (or bash
  fallback). Captures stdout/stderr/exit; emits `script_output`
  Evidence with the body hash. Output JSON carries `scriptSubject` +
  `scriptHash` for audit replay.
- **`gateway.scripts.search`** (SPEC §22.7) — authoring-time tool
  returning script refs filterable by verb/subject_root/source.
  Progressive disclosure: bodies are fetched separately via
  `gateway.describe`. Advertised behind
  `FlowgateServer::with_scripts_search(true)`.
- **`script_acknowledged` guard** — review-before-execute gate for
  destructive scripts. Passes iff `gateway.describe` was called for
  the subject AND the recorded body hash matches the current snapshot.
  Hash flip invalidates the prior ack. Backed by
  `ScriptAcknowledgmentStore` trait + `InMemoryScriptAcknowledgmentStore`.
- **8 new error codes** (SPEC §22.9): `INVALID_SCRIPT_VERB`,
  `MISSING_SCRIPT_VERB`, `INVALID_SCRIPT_SUBJECT_ROOT`,
  `EMPTY_SCRIPT_SUBJECT`, `MISSING_SCRIPT_LIFECYCLE`,
  `INVALID_SCRIPT_LIFECYCLE`, `SCRIPT_BODY_SOURCE_AMBIGUOUS`,
  `MISSING_SCRIPT_HASH`, `UNSUPPORTED_SCRIPT_URI_SCHEME`,
  `INVALID_SCRIPT_HASH_FORMAT`, `SCRIPT_HASH_MISMATCH`,
  `SCRIPT_NOT_IN_SNAPSHOT`, `INVALID_SCRIPT_INVOCATION`,
  `SCRIPT_SUBJECT_UNKNOWN`.
- **`FLOWGATE_SCRIPT_SUBJECT` + `FLOWGATE_SCRIPT_HASH` env vars**
  exposed to script bodies so scripts can self-identify in logs/metrics
  without parsing argv.
- **Stricter normalization for script hashing** —
  `normalize_for_script_hash` preserves internal whitespace exactly
  (collapses only trailing newlines), distinct from
  `normalize_for_hash` (skills, whitespace-collapsing). Shell scripts
  treat whitespace as load-bearing; the script hash respects that.

### Added — TUI runtime + sub-agent orchestration (SPEC §21)

- **`delegate` field on workflow states** (SPEC §21). Optional non-empty
  string surfaced verbatim at the top level of every workflow response.
  The gateway treats it as pass-through — never reads it, never branches
  on it, never validates against any registry. The sole consumer is the
  TUI interpreter. Configs without `delegate` are unchanged.
- **`INVALID_DELEGATE` error code.** Raised at config load when a state
  declares `delegate` as an empty string or non-string. Names the
  offending workflow + state in the message.
- **TUI deterministic interpreter** (`crates/mcp-flowgate-tui/src/interpreter.rs`).
  `walk_workflow` drives a workflow to completion: auto-advances states
  whose only actionable link is non-deterministic, picks the first
  non-`escalate` link when several remain, and hands off to
  `SubAgentSpawner` for `delegate` states. Retry budget of 3 on
  sub-agent timeouts; submits `escalate` transition if declared, else
  propagates.
- **TUI sub-agent spawner abstraction** (`SubAgentSpawner` trait + stub
  `AetherSubAgentSpawner` impl). The trait is the integration seam for
  Aether headless sessions; the impl currently surfaces
  `SubAgentTimeout` so the integration with `aether_cli::headless::run_headless`
  is observably scoped to a follow-on commit while the interpreter
  itself ships fully tested via the scripted-double pattern.
- **`flowgate walk` CLI subcommand** with required-no-default poka-yoke
  on `--max-sub-agent-seconds` and `--max-sub-agent-steps`. Both
  rejected at startup if missing; rationale: an unbounded sub-agent is a
  foot-gun (orphan tasks, runaway cost, looping critic).
- **`--agent name=provider/model` CLI flag** (repeated) for wiring
  sub-agent configurations. Resolved against `delegate` field at spawn
  time; unknown name → `InterpreterError::UnknownAgent` naming the
  state + agent.
- **examples/swe-agent.yaml** — added `delegate:` fields on the four
  model-driven states (planning → planning-agent, retrieving →
  retrieval-agent, editing → editing-agent, critiquing → critique-agent).
  `verifying` (deterministic executor) and `human_review` (actor:human)
  intentionally do not delegate.

### Documentation

- New `docs/TUI-AGENT.md` covers the interpreter algorithm, sub-agent
  lifecycle, timeout poka-yoke rationale, and the cognitive-architecture
  rationale for "commodity models directed by precise architecture
  outperform frontier models without structure."
- README adds a `## The TUI agent — commodity models outperform frontier`
  section after the "What the model sees" walkthrough.

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
