# Changelog

All notable changes to **mcp-flowgate** are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
on the cargo crate version. The **config schema** is versioned
separately — see [`STABILITY.md`](STABILITY.md) for what is and isn't
covered by a stability commitment.

## [Unreleased]

### Added — Per-provider feature toggles applied at spawn time

- **`sub_agent::features_to_reasoning_effort`** — translates `agents.yaml` per-provider feature structs (`AnthropicFeatures`, `OpenAIFeatures`, `GoogleFeatures`) into aether-llm's effective `ReasoningEffort`. Anthropic `extended_thinking: true` with no budget defaults to High; explicit `thinking_budget_tokens` snaps to the nearest effort level (Low ≤2048 / Medium ≤6144 / High ≤16384 / Xhigh) using the same thresholds aether-llm's anthropic provider uses internally. OpenAI's `reasoning_effort` string passes through `low|medium|high|xhigh`; unknown values drop with a warning. Google's `thinking_budget_tokens` reuses the same budget-to-effort mapping.
- **AetherSubAgentSpawner now bypasses `run_headless`** in favor of constructing an `AgentSpec` directly + calling `headless::run::run`. The wrapper's `HeadlessArgs` shape has no field for reasoning effort; the spec-direct path lets the translated effort actually take effect. The TUI's "deferred to v0.3.1" warning is gone — toggles apply.
- **`crates/mcp-flowgate-tui/Cargo.toml`** — new direct dep on `aether-llm` (the existing transitive dep was already in the lockfile; this just promotes it). Used to reference `ReasoningEffort`.
- **Tests** — `tests/feature_translation.rs` pins every cell of the mapping table.
- **Docs** — `agent-config.mdx` "Feature toggles" section drops the deferred-Aside and documents the new mapping.

### Added — Runtime CoR over the binding list

- **`interpreter::spawn_with_cor`** — walks a `ResolvedBindingList` in order, attempting each binding via the existing `SubAgentSpawner::spawn_and_wait`. Infrastructure-class failures (401/403/429/404/network) advance to the next binding; content-class failures surface immediately (FMECA R1). All-bindings-fail returns a structured `AgentResolutionExhausted` carrying the full attempt trail. `walk_workflow` now uses this path instead of single-binding spawn.
- **`interpreter::classify_spawn_error`** — maps `InterpreterError::Mcp { source }` strings to `FailureClass`. Aether currently emits LLM errors as in-stream `AgentMessage::Error` events rather than typed `CliError` returns, so this is a substring matcher (`"401"` → `Auth401`, etc.); when aether grows a typed error pass-through, the matcher gets a structured signal instead. `SubAgentTimeout` deliberately classifies as `ContentOther` so the existing retry-budget path keeps owning soft timeouts.
- **`AgentRegistry::resolve_bindings`** — new trait method returning a `ResolvedBindingList` (label + level + full bindings vector). Default impl wraps `resolve` in a 1-element list, so legacy `--agent`-flag callers keep working unchanged; `YamlAgentRegistry` overrides to return the full override list.
- **Tests** — `crates/mcp-flowgate-tui/tests/agent_resolver_runtime_cor.rs` pins the contract: primary succeeds → index 0, primary 401 → advance + secondary OK, primary content-other → surface, all-401 → exhaustion, timeout → propagate.
- **Defense in depth in `Resolver::try_next`** — any content-class entry in `prior_failures` surfaces immediately instead of advancing. Previously the docstring asked callers to short-circuit content failures; the resolver now enforces it itself.
- **Test infrastructure** — `preflight::classify_outcome` extracted as a pure dispatch helper so the warn-vs-fail-vs-error wiring is testable without HTTP plumbing.

## [0.3.0] — 2026-05-27

Adds the FMECA-vetted agent-resolver design: `agents.yaml` with closed-enum affinities/tiers, sparse overrides keyed by `<affinity>-<tier>`, eager auth preflight, and a guided-setup orchestrator (`meta/flow.configure-models`) in the sibling [flowgate-meta](https://github.com/matt-cochran/flowgate-meta) repo. Replaces the v0.2 pattern of repeating `--agent name=provider/model` CLI flags per workflow invocation.

### Added — Agent resolver (`agents.yaml`)

- **`crates/mcp-flowgate-tui/src/agent_resolver/`** — new module with sub-modules `config`, `classify`, `walk`, `preflight`. Loads `.flowgate/agents.yaml` (project) or `~/.config/flowgate/agents.yaml` (user); project shadows user whole-file.
- **Closed enums** — `Affinity` (`coding | reasoning | prose | web-search | recon`), `Tier` (`frontier | standard | commoditized`), `Provider` (`anthropic | openai | google | ollama | lmstudio | custom`). Enum additions are minor-version compatible per the documented policy.
- **Specificity walk** — `<affinity>-<tier>` → `<affinity>` → `<tier>` → `default`. Affinity wins tiebreaker. Opt-in `strict_specificity: true` upgrades the fall-through to a load-time error.
- **`FailureClass`** — closed enum `Auth401 | Auth403 | RateLimit429 | NotFound404 | NetworkTimeout | ContentSchema | ContentSafety | ContentOther`. Unknown response status defaults to `ContentOther` (surface, never fall through).
- **Eager auth preflight** at workflow load — every primary (index 0) binding referenced by any declared `delegate:` is auth-probed once. 401/403 is a startup error, never a runtime fall-through. `FLOWGATE_SKIP_PREFLIGHT=1` escape for CI / disconnected dev.
- **Mutual exclusion** between `--agent` CLI flags and `agents.yaml` (FMECA T1). Both set → `AmbiguousAgentSource` startup error.
- **Per-provider feature structs** with `#[serde(deny_unknown_fields)]` — `extended_thinking`, `reasoning_effort`, etc. Typos fail at load with the offending key named.
- **Structured `AgentResolutionExhausted`** carrying `delegate`, `walked_levels`, `attempts: Vec<AttemptRecord { binding, class, detail }>`.

### Added — Doctor checks

- **`agents.yaml`** — loads project + user files; reports binding/override counts and `strict_specificity` status.
- **`agents.yaml shadow`** — names the shadowed file when both project and user files exist.
- **`workflow delegates`** — runs each `delegate:` state through `resolver.walk()` and reports the specificity level chosen (names every delegate whose only match is a less-specific fallback).

### Added — `flowgate validate-agents-config <path>` subcommand

- Loads an `agents.yaml` at an arbitrary path via the SAME `AgentsFile::from_path` the resolver uses at workflow start; emits a JSON envelope `{ok, summary, error_kind, detail}` on stdout. Stable `error_kind` codes (`MISSING_DEFAULT`, `EMPTY_DEFAULT`, `UNKNOWN_OVERRIDE_KEY`, `UNKNOWN_FEATURE_KEY`, etc.) scripts can switch on.
- Powers the round-trip validation step in `meta/cap.implement.write-agents-config` (FMECA U3).

### Added — `meta/flow.configure-models` orchestrator (in [flowgate-meta](https://github.com/matt-cochran/flowgate-meta))

- Five caps: `cap.research.model-inventory`, `cap.plan.suggest-bindings`, `cap.gate.human-approve-plan`, `cap.implement.write-agents-config`, `cap.verify.auth-only-smoke-test`.
- One orchestrator wiring them: inventory → plan → approve (`mode: auto` or `review_plan`) → atomic write + round-trip → 1-token smoke per binding.
- Smoke-test output names its limitation explicitly: **auth verified, capability not tested**. v0.4 roadmap replaces it with a capability harness.
- E2E walked-to-terminal test in `crates/mcp-flowgate-executors/tests/meta_orchestrators_e2e.rs::meta_flow_configure_models_walks_to_terminal_in_auto_mode`.

### Documentation

- **`site/src/content/docs/guides/agent-config.mdx`** — migration story, closed-enum reference, strict-mode discipline, `flow.configure-models` walkthrough.

### Production hardening (post-audit 2026-05-27)

The 2026-05-27 four-agent production-readiness audit flagged eleven items; ten landed in this release and one is documented as honestly deferred.

- **HTTP `connect_timeout(10s)`** added to both `reqwest::Client::builder()` sites in the workspace (`crates/mcp-flowgate-executors/src/rest.rs` and `crates/mcp-flowgate-core/src/config.rs`). The pre-existing total timeouts (120s + 30s) stand; the new connect_timeout guards against hung DNS / TCP handshakes that the total timeout couldn't catch.
- **Lock-poisoning signal preserved.** 33 `RwLock`/`Mutex` `.unwrap()` sites in `crates/mcp-flowgate-core/src/` converted to `.expect("LOCK_POISONED: <holder>")` so a poisoned-panic message names the originating subsystem (`workflow store`, `audit event buffer`, `sqlite connection`, etc.). The no-I/O-under-lock invariant is documented at the top of `crates/mcp-flowgate-core/src/lib.rs`. The workspace `clippy::unwrap_used` lint was deferred — too many pre-existing `Option`/`Result` unwraps to enable cleanly in this commit; targeted for v0.4.
- **Workflow failure-path tests.** New `crates/mcp-flowgate-executors/tests/workflow_failure_paths.rs`: 2 active tests confirm permanent executor failures don't silently report `status="completed"` and guard rejection blocks advance via `submit()`. 2 `#[ignore]`'d honest stubs name v0.4 gaps — the runtime timeout is lazy-poll not watchdog, and there's no cancellation API yet. Each stub body shows the test shape for when the API lands.
- **ScriptExecutor integration tests for the three meta scripts.** The orchestrator e2e bypasses scripts via the `CapShortCircuit` fixture; new `meta_scripts_integration.rs` exercises `fetch.provider-model-inventory`, `install.agents-config`, and `verify.auth-only-smoke-test` against `std::net::TcpListener`-backed mock providers. The atomic-rollback contract on the write script is now test-pinned. Companion `*_BASE_URL` env-var overrides shipped in flowgate-meta — also useful as a corporate-proxy escape hatch.
- **Doctor reference page.** New `/reference/doctor/` site page documenting all 9 checks the binary runs, their failure codes, and the operator action for each. Until now only 3 of 9 were documented.
- **Nightly CI workflow.** New `.github/workflows/nightly.yml` runs `cargo test --workspace -- --include-ignored` + `examples/smoke-ete/walk-live.sh` against real provider credentials at 04:00 UTC daily. Auto-files a labeled GitHub issue on failure so live-path regressions surface within 24h. Required secrets (`ANTHROPIC_API_KEY_CI`, `OPENAI_API_KEY_CI`, `GOOGLE_API_KEY_CI`) documented in `CONTRIBUTING.md`. Fork-gated so PRs from forks don't accidentally trigger live API calls.

### Honest deferrals (v0.3.1 / v0.4 roadmap)

- Per-list runtime CoR over actual provider failures (v0.3.1) — the classifier and structured exhaustion error are already in place.
- Per-provider feature-toggle translation to upstream SDK extras (v0.3.1).
- `flowgate doctor --refresh-agents` periodic re-probe (v0.4).
- Capability-quality harness replacing the auth-only smoke test (v0.4).



## [0.2.0] — 2026-05-26

First public release since 0.1.1. Ships the **two-tier composition
model** (capabilities + orchestrators) as the v0.6 spec lands, plus
multi-repo loading, a 24-verb capability cloud, a typed slot table,
contract-hash pinning, and an end-to-end acceptance suite against the
sibling [cognitive-architectures](https://github.com/matt-cochran/cognitive-architectures)
library and the new [flowgate-meta](https://github.com/matt-cochran/flowgate-meta)
self-authoring repo.

This release also bundles every internal version marker between 0.1.1
and 0.2.0 (the `[0.4.0]`, `[0.3.0]`, and `[0.2.0] - 2026-05-25`
sections preserved below are historical development markers — none of
them were ever publicly tagged). Cumulative public diff from 0.1.1:

- The typed skills surface (SPEC §5)
- The scripts surface (SPEC §22) and the verb-taxonomy expansion
- The lexicon / ubiquitous-language primitive (SPEC §30)
- Deterministic chaining, hot-reload via SIGHUP, dynamic fan-out
- The mcp-flowgate-tui crate
- Trace/run id plumbing, evidence enrichment
- — plus the v0.6 composition headline below

### Added — Multi-repo loading (SPEC §9)

- **Repo manifest** (`flowgate.repo.yaml`) declares a `namespace`,
  `version`, and `layout` of directories where capabilities,
  orchestrators, skills, scripts, and connections live. Each repo's
  loaded definitionIds are namespace-prefixed `<namespace>/<id>`
  before being merged into the gateway registry.
- **Top-level `repos:` block** on gateway configs accepts an array of
  `{ path: <dir> }` entries. Relative paths resolve against the host
  config's directory; `~/` expands to `$HOME`.
- **Top-level `overrides:` block** lists fully-qualified ids the host
  config explicitly shadows after a repo provides them. Anonymous
  shadowing — defining `<ns>/<id>` locally without listing it in
  `overrides:` — is a config-load error (V23). Stale overrides that
  don't collide are also rejected.
- **Cross-namespace references**: `kind: workflow` `definitionId:`
  references inside a repo-loaded workflow are namespace-prefixed at
  load time. Unprefixed names bind to the workflow's own namespace;
  unresolved refs fail at load (V22).
- **Load-time rules V19–V23** enforced by
  `mcp-flowgate-core::repo` and `config::load_resolved_with_repos`.
  Binary's `serve` and `check` subcommands now call the multi-repo
  loader transparently.

### Added — Two-tier composition (SPEC §3, §5–§6)

- **Capability workflows** (`cap.<verb>.<name>`) declare a typed
  `snippet: { inputs, outputs }` contract. Capabilities are
  composition leaves and may NOT invoke other workflows (V10).
- **Orchestrator workflows** (`flow.<name>`) declare an `inputs:`
  block defining their entry signature. Orchestrators invoke
  capabilities via `kind: workflow` executors with `use: { inputs,
  outputs }` bindings. Orchestrators may not invoke other
  orchestrators (V11).
- **`use:` bindings** thread typed inputs from host context to the
  capability's snippet, and project declared outputs back into host
  slots at the LHS paths. Capabilities run in their own private
  blackboard (the scoping firewall); only declared outputs propagate.
- **Snippet output validation (V17)** — every projected cap output is
  schema-checked against `snippet.outputs` at runtime. A failure
  emits `cap.output.schema_violation` audit, returns the new
  `ExecutorError::SchemaViolation` variant, and leaves the host
  blackboard untouched (no partial projection).
- **Capability termination semantics (V18)** — abnormal cap
  termination emits `cap.terminated` with `error_kind` +
  `parent_correlation_id`, no partial output projection.
- **The 24-verb cloud** (`cap_verb` module) — 10 cognitive + 12
  deterministic + 2 coordination tokens (`gate`, `coordinate`).
  V6 primary-executor verb-shape check enforces per-category
  executor kinds (Cognitive→mcp/noop, Deterministic→script/mcp,
  Gate→human/ask actor, Coordinate→mcp).

### Added — Slot table + contract hash (SPEC §6.2, §7)

- **Per-orchestrator slot table** (`slot_table` module) seeded from
  the orchestrator's `inputs:` block + every state's `use:.outputs`
  declarations. Powers V13 reachability (every `use:.inputs` host
  path must resolve to a declared slot) and V14 type consistency
  (two states writing the same host slot must declare structurally
  identical schemas).
- **Contract hash** (`contract_hash` module) — sorted-key canonical
  JSON + SHA-256 over a capability's `snippet:` block, formatted as
  `sha256:<hex>`. Stability is part of the public contract; pinned
  by `tests/contract_hash_canonical.rs` so refactors that change the
  encoding surface as test failures.
- **`expects_contract_hash:` pin** on `use:` blocks. V15 fires when
  the pin doesn't match the loaded capability's hash; V16 fires when
  a `stable`-lifecycle capability is invoked without any pin.

### Added — Validation cloud V1–V23

- Rule-keyed dispatcher in `validate.rs` with one private fn per
  rule. Centralised via `validate_workflows` and called from the
  `check` subcommand.
- **Validation-rule parity scanner** (`scripts/check-validation-parity.sh`)
  enforces that every rule V1–V23 has at least one accepts test AND
  one rejects test. Wired into CI before `cargo test`.

### Added — Library content (sibling repos)

- **cognitive-architectures v0.6** — 22 capabilities + 4 lifecycle
  orchestrators (`flow.add-feature`, `flow.bugfix-from-error-log`,
  `flow.safe-refactor`, `flow.triage-issue`) covering the main
  inbound surfaces of an engineering team. Loaded by operators via
  `repos: [{ path: /repos/cognitive-architectures }]`.
- **flowgate-meta v0.1** — new sibling repo shipping four
  meta-authoring orchestrators (`flow.author-capability`,
  `flow.author-flow`, `flow.optimize-capability`,
  `flow.optimize-flow`) that compose 10 meta caps including
  introspect-the-gateway primitives (`cap.research.tool-inventory`)
  + typed wrappers over `gateway.lexicon.{lookup,define}`. Adapts
  to whatever tools the operator actually has reachable rather
  than assuming a fixed stack.
- **Vendored fixtures** under `crates/mcp-flowgate-core/tests/fixtures/`
  for both libraries; e2e tests walk every shipping orchestrator to
  its terminal state.

### Changed

- Binary entrypoints (`serve`, `check`) now call
  `load_resolved_with_repos` instead of `load_resolved`. Hosts with
  no `repos:` block round-trip unchanged.
- `ExecutorError::SchemaViolation(String)` variant added; classifies
  as `ErrorClass::Permanent` (never retryable). All `class()`
  dispatch sites picked up automatically.
- Config-resolve gains `expand_use_bindings` pass: walks every
  transition with a `kind: workflow` + `use:` executor; synthesises
  the transition-level `output:` mapping from `use.outputs` so the
  existing `merge_output` projection layer drives writes; embeds
  the target capability's `snippet.outputs` schema as `_snippetOutputs`
  on the executor config (no DefinitionStore lookup needed at run
  time).
- Workspace cleared of all `clippy --workspace --all-targets -- -D
  warnings` errors. CI's clippy gate now passes.

### Fixed

- WorkflowExecutor previously polled `runtime.get` indefinitely when
  a sub-workflow's start auto-chain failed (start returned
  `status: failed` but subsequent get returned
  `status: waiting_for_action`). Now detects the failed start
  response and short-circuits with `ExecutorError::Permanent` +
  `cap.terminated` audit event.

### Test surface

- **30+ new integration tests** across `multi_repo_loading`,
  `snippet_contract`, `use_binding`, `validation_rules`,
  `slot_table_rules`, `contract_hash_canonical`,
  `cap_output_violation`, `cap_terminated`,
  `scoped_capability_io_roundtrip`, `flow_orchestrators_e2e`,
  `meta_orchestrators_e2e`. Cumulative workspace test count: 826.
- New unit-test modules for `cap_verb`, `tier`, `slot_table`,
  `contract_hash`, `use_binding`, `repo`.

## [Historical / development markers (pre-0.2.0 — never released)]

The version bumps below were internal development markers in the
0.1.1 → 0.2.0 window. They never received public tags. The cumulative
diff is rolled up into the 0.2.0 release above.

## [0.4.0-dev] - 2026-05-25

(Originally marked `[0.4.0]`. Renamed to clarify it never shipped.)

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

## [0.4.0-dev / continued] - 2026-05-25

(Continuation of the `[0.4.0-dev]` development marker above; the
lexicon block landed first, then the additive surfaces below.)

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

## [0.3.0-dev] - 2026-05-25

(Originally marked `[0.3.0]`. Renamed to clarify it never shipped.)


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

## [0.2.0-dev] - 2026-05-25

(Originally marked `[0.2.0]`. Renamed to clarify it never shipped;
the public 0.2.0 is the 2026-05-26 release above.)

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
