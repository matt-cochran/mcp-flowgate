# Stability tiers

| Artifact | Version | Stability commitment |
|----------|---------|----------------------|
| Crate (`mcp-flowgate` et al.) | 0.1.0 | Pre-1.0 — semver not yet in effect. Breaking changes may occur at any minor bump. |
| Config schema (`version` field) | "1.0.0" | Tier 1 stable — backward-compatible within the same minor schema version. |
| Seven-tool MCP surface | Stable | Fixed tool names, inputs, and output shapes. Additions may appear; removals follow a deprecation cycle. |

We distinguish three tiers of stability commitment. Every public
artifact — tools, config keys, schemas, doc links — falls into exactly
one tier. The tier decides how quickly you can depend on it and what
happens when it changes.

---

## Tier 1 — Committed

**Breaking changes require a deprecation cycle.** You can depend on
these without pinning a specific version.

| Artifact | Notes |
|----------|-------|
| Seven MCP tool names and their input/output shapes | `gateway.{home,search,describe}`, `workflow.{start,get,submit,explain}`. New tools may be added; removals or incompatible changes go through the deprecation cycle. |
| Config schema `version` field | Must parse as a semver-compatible string. Backward-compatible within the same minor version. The `mcp-flowgate check` command verifies this field exists. |
| Top-level config keys | `version`, `include`, `capabilities`, `connections`, `proxy`, `workflows`, `audit`, `discovery`, `store`. |
| Major guard kinds | `permission`, `role`, `expr`, `evidence`. The deprecated alias `jsonpath` is accepted but emits a warning. |
| Major executor kinds | `noop`, `cli`, `rest`, `mcp`, `human`. |
| Audit event taxonomy | Event type names and the shape of their payload. New event types may appear; existing ones don't change shape. |
| `WorkflowStore` trait | Implementations may be added; the trait's method signatures won't change incompatibly. |
| `Executor` and `ExecutorRegistry` traits | As above. |
| Intent-layer invariant (SPEC §23.6) | Verb taxonomy lives only on `skills` and `scripts`. The access layer (`connections`, `capabilities`, `executors`) is kind-typed. Workflows compose. Audit events describe. This is an architecture rule, not an implementation choice — breaking it requires a SPEC §23 amendment, not just an API deprecation. |
| Closed cognitive verb enum (SPEC §5.4.1) | 10 verbs as of v0.3: `triage`, `diagnose`, `plan`, `implement`, `review`, `refactor`, `explain`, `compose`, `research`, `summarize`. Adding a verb requires SPEC §23.7 amendment criteria. Removing one requires a deprecation cycle. |
| Closed script verb enum (SPEC §22.3) | 12 verbs as of v0.3: `build`, `test`, `deploy`, `format`, `lint`, `install`, `verify`, `run`, `inspect`, `search`, `fetch`, `audit`. Same amendment policy as cognitive verbs. |
| SPEC ↔ Rust enum drift detection | The `spec_enum_drift.rs` test asserts byte-equality between SPEC verb/root tables, JSON schema enums, and Rust closed enums. Drift fails build. Any change to the verb/root vocabularies must propagate to all three sources simultaneously. |

## Tier 2 — Deprecation cycle

**Subject to change with a deprecation notice in the changelog and  
one minor-version grace period.** This is the right tier for features
we believe in but want room to refine based on real use.

| Artifact | Notes |
|----------|-------|
| `actor: "deterministic"` and chaining semantics | Auto-execution of deterministic transitions, `maxChainDepth`, `chain` response array, `CHAIN_FAILED` error code. |
| Phase guidance (`goal`, `guidance` on states) | `guidance` response object with `goal` and `instructions` fields. |
| Non-major guard kinds | Any guard kind not listed in Tier 1 above. |
| Non-major executor kinds | Any executor kind not listed in Tier 1 above. |
| Discovery index scoring | Scoring weights, prefix/fuzzy matching thresholds, and the `aliases` field may be tuned. |
| Config hot-reload (SIGHUP) | Swappable definitions, executors, and discovery index. The set of swappable components may expand. |
| Workflow graph validation | The set of `check` diagnostics (unreachable states, dangling targets, dead-ends) may grow. |
| Link-filter semantics | The `byGuards` filter's exact behaviour may be refined. |
| Stress-test scenarios | Test scenarios in `crates/mcp-flowgate-core/tests/stress_scenarios.rs` — they document concurrency properties we commit to, but the exact scenario list may grow or shrink. |
| Examples | `examples/*` — illustrative and dogfooded, but the exact directory layout and filenames may change. |
| `delegate` on workflow states (SPEC §21) | Pass-through string surfaced on every workflow response. The shape (non-empty string) and the response surfacing are stable; the set of consumers (TUI today; future harnesses) may grow. |
| `scripts:` top-level block (SPEC §22) | Script library shape (verb / lifecycle / body \| uri+hash / source) is stable. The closed verb enum (12 values as of v0.3) is committed; adding verbs requires a spec amendment per SPEC §23.7. The blessed-script-roots set (15 values) may grow with the same strict-vs-lenient flag treatment as skill roots. |
| `script` executor kind (SPEC §22.6) | Subject lookup, temp-file materialization (chmod 0700), shebang-or-bash invocation, `script_output` Evidence emission with body hash. Output schema (`exitCode`/`success`/`stdout`/`stderr`/`json`/`scriptSubject`/`scriptHash`) is committed. Transition record's executor descriptor carries `subject`+`hash` for script executors (additive, optional, serde-`skip_serializing_if`-bypassed for non-script kinds). |
| `gateway.scripts.search` (SPEC §22.7) | Authoring-time discovery tool. Refs-only response (verb / subject / source); progressive disclosure invariant committed. Filter set may grow. |
| `script_acknowledged` guard (SPEC §22.8) | Hash-flip-invalidated review-before-execute gate. Distinct keyspace from `guidance_acknowledged`. |
| `file://` URI scheme for script bodies | v1 only scheme. `https://` and `git+https://...@<ref>` are planned for v2 and will be ADDITIVE (file:// stays valid). |
| TUI interpreter behavior | Sub-agent spawn / timeout / retry / escalation policies. The algorithm shape is stable; the retry budget (3) and the multi-link tiebreaker (first non-`escalate`) may be refined based on usage. |
| Agent config CLI format | `--agent name=provider/model` syntax. v2 will add TOML config files; the CLI form stays valid for one minor-version cycle minimum after that lands. |

## Tier 3 — Internal

**No stability promises. May change or disappear without notice.**  
Use at your own risk.

| Artifact | Notes |
|----------|-------|
| Internal crate APIs | Anything not re-exported from `mcp-flowgate-core/src/lib.rs` or the top-level crate. |
| Unstable config keys | Any key not listed in Tier 1 above. |
| Benchmark internals | `crates/mcp-flowgate-core/benches/*` — we report numbers but the exact benchmark harness is an internal tool. |
| CI configuration | `.github/workflows/*` — workflow files are operational, not a public API. |
| Dev-only doc pages | `docs/STRESS-TESTS.md`, `docs/INVARIANTS.md` — accurate but may be restructured. |

---

## Verification coverage

What we've tested per tier and how we decide each tier's verification
standard.

### Tier 1 — Stable surface

| Area | Verified | How |
|------|----------|-----|
| Seven MCP tools | Yes | Invariant 9 test in `crates/mcp-flowgate-mcp-server/tests/stable_tool_surface.rs` |
| 10 core invariants | Yes | `crates/mcp-flowgate-core/tests/invariants.rs` |
| Capability wrapping and `wraps:` chain | Yes | `crates/mcp-flowgate-core/tests/capability.rs` |
| `include:` multi-file composition | Yes | `crates/mcp-flowgate-core/tests/composability.rs` |
| Discovery indexing and search | Yes | `crates/mcp-flowgate-core/tests/discovery.rs` |
| Evidence guard | Yes | `crates/mcp-flowgate-core/tests/evidence_guard.rs` |
| File- and SQLite-backed WorkflowStore | Yes | `crates/mcp-flowgate-core/tests/persistent_stores.rs` |
| REST executor | Yes | `crates/mcp-flowgate-executors/tests/rest_executor.rs` |
| Human executor audit event | Yes | `crates/mcp-flowgate-executors/tests/human_audit.rs` |
| TDD example dogfood transcript | Yes | CI runs `examples/tdd/dogfood-drive.py` |
| Deterministic chaining | Yes | `crates/mcp-flowgate-core/tests/deterministic_chain.rs` (16 scenarios) |
| Phase guidance in responses | Yes | Covered by deterministic chain tests |

### Tier 2 — Deprecation-cycle surface

| Area | Verified | How |
|------|----------|-----|
| Stress tests under concurrency | Yes | `crates/mcp-flowgate-core/tests/stress_scenarios.rs` |
| Discovery prefix/fuzzy/alias matching | Yes | `crates/mcp-flowgate-core/tests/discovery.rs` (3 new tests) |
| Workflow graph validation | Yes | `crates/mcp-flowgate-core/src/validate.rs` (10 unit tests) |
| Hot-reload swap mechanism | Yes | `crates/mcp-flowgate-core/src/hot_reload.rs` (unit test) |
| Postgres store backend | Partial | Implementation exists; opt-in integration test (`POSTGRES_TEST_URL`); CI service-container coverage pending |

### What we don't test (and why)

- **LLM behaviour.** Whether a model follows HATEOAS links is a
  model-level property, not a gateway property. The gateway returns
  correct links; the dogfood transcript mechanically verifies the bytes.
- **Throughput under load.** Stress tests cover correctness under
  concurrency, not throughput. See `PERFORMANCE.md` for latency numbers.

---

## Deprecation process (Tier 1 and Tier 2)

1. The breaking change is announced in `CHANGELOG.md` under
   `## [Unreleased]` with `### Deprecated`.
2. For Tier 1, the old behaviour is maintained for at least one minor
   release after the announcement.
3. For Tier 2, the old behaviour is maintained for at least one patch
   release after the announcement.
4. After the grace period, the old behaviour is removed and the
   changelog entry moves to `### Removed` in the release where it
   actually disappears.