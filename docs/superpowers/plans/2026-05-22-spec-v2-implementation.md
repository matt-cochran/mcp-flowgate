# mcp-flowgate SPEC v2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the four opt-in additions in `SPEC.md` v2 — blackboard slot declaration, transition records, versioned definitions, two-tier guidance — without weakening any spec guarantee.

**Architecture:** Six phases, each independently shippable and revertable. Extends existing crate structures (`config.rs`, `validate.rs`, `audit.rs`, `runtime.rs`, `model.rs`); introduces no parallel abstractions. Every spec guarantee (fail-fast, poka-yoke, immutability) is encoded as a named, executable **guarantee test** committed before the code that satisfies it.

**Tech Stack:** Rust workspace (`mcp-flowgate-{schema,core,executors,mcp-server}`), `typify`, `serde`, `rmcp`, `tokio`, `cargo test` / `cargo clippy`.

---

## CPM — phases, dependencies, critical path

| Phase | Deliverable | Depends on | SPEC § |
|---|---|---|---|
| 1 | Blackboard slot declaration + `output:` name-check | — | §6, §11 |
| 2 | Transition records (typed `AuditSink` event) + record-first commit | — | §7 |
| 3 | Date-rotated `File` audit sink | 2 | §7.4 |
| 4 | Versioned definitions (pinning + per-instance snapshot) | — | §8 |
| 5 | Two-tier guidance (templating, `skills:`, refs, describe) | 1 | §5 |
| 6 | `check` use-before-def | 1, 5 | §9, §11 |

**Critical path:** 1 → 5 → 6. **Parallel track A:** 2 → 3. **Parallel track B:** 4.
Phases 2/3/4 may proceed concurrently with the critical path.

**Phase gate (every phase):** all tests green (`cargo test --workspace`), `cargo clippy --workspace -- -D warnings` clean, one commit per task, and the phase's **guarantee tests** (listed per phase) present and passing. A phase MUST NOT be marked complete with a guarantee test absent or `#[ignore]`d.

## Detailing strategy (just-in-time)

Phase 1 is fully expanded into bite-sized TDD steps below. Phases 2–6 are task lists with files, named guarantee tests, and acceptance criteria. **Expand the active phase to bite-sized steps at its start, after re-reading the touched files** — the codebase shifts under earlier phases, so pre-writing literal code for Phase 5 now would be stale by the time it runs (see plan FMECA, FM1).

## File structure

- **Phase 1:** `schemas/gateway-config.schema.json`, `crates/mcp-flowgate-core/src/validate.rs`, `tests/config_validation.rs`
- **Phase 2:** `schemas/transition-record.schema.json` (new), `crates/mcp-flowgate-core/src/audit.rs`, `src/runtime.rs`, `src/error.rs`, `tests/transition_records.rs` (new)
- **Phase 3:** `crates/mcp-flowgate-core/src/audit.rs`, `tests/audit_rotation.rs` (new)
- **Phase 4:** `crates/mcp-flowgate-core/src/model.rs`, `src/store*.rs`, `src/hot_reload.rs`, `src/runtime.rs`, `tests/definition_versioning.rs` (new)
- **Phase 5:** `schemas/gateway-config.schema.json`, `schemas/workflow-response.schema.json`, `crates/mcp-flowgate-core/src/{config,runtime}.rs`, `crates/mcp-flowgate-mcp-server/src/lib.rs`, `tests/guidance.rs` (new)
- **Phase 6:** `crates/mcp-flowgate-core/src/validate.rs`, `tests/config_validation.rs`

---

## Phase 1 — Blackboard slot declaration

**Guarantee tests:** `undeclared_output_slot_warns`, `declared_blackboard_accepted`.

### Task 1.1: Add `blackboard` to the config schema

**Files:** Modify `schemas/gateway-config.schema.json` (the `workflowDefinition` `$def`).

- [ ] **Step 1: Add the property.** In `$defs/workflowDefinition.properties`, add:

```json
"blackboard": {
  "type": "object",
  "description": "Declared context slots. Value is a bare name marker or a JSON-schema fragment.",
  "additionalProperties": true,
  "default": {}
}
```

- [ ] **Step 2: Verify the schema crate still builds.**
Run: `cargo build -p mcp-flowgate-schema`
Expected: PASS (typify regenerates types with the new optional field).

- [ ] **Step 3: Commit.**
```bash
git add schemas/gateway-config.schema.json
git commit -m "feat(schema): declare optional workflow blackboard slots"
```

### Task 1.2: Warn when `output:` writes an undeclared slot

**Files:** Modify `crates/mcp-flowgate-core/src/validate.rs`; Test `crates/mcp-flowgate-core/tests/config_validation.rs`.

- [ ] **Step 1: Re-read `validate.rs`** and note the existing diagnostic type and how warnings are emitted (do not assume signatures — match the file).

- [ ] **Step 2: Write the failing test** in `tests/config_validation.rs`:

```rust
#[test]
fn undeclared_output_slot_warns() {
    // A workflow with blackboard: {lintPassed} but a transition output: writing `typo`.
    let cfg = config_with_output_slot(&["lintPassed"], "typo");
    let report = validate(&cfg);
    assert!(
        report.warnings.iter().any(|w| w.contains("typo") && w.contains("blackboard")),
        "expected a warning naming the undeclared slot `typo`; got {:?}", report.warnings
    );
}

#[test]
fn declared_blackboard_accepted() {
    let cfg = config_with_output_slot(&["lintPassed"], "lintPassed");
    assert!(validate(&cfg).warnings.is_empty());
}
```

- [ ] **Step 3: Run, verify failure.**
Run: `cargo test -p mcp-flowgate-core --test config_validation undeclared_output_slot_warns`
Expected: FAIL — no such warning emitted.

- [ ] **Step 4: Implement** the check in `validate.rs`: for each workflow, collect declared `blackboard` keys; for each transition `output:` map, push a warning for any target key not declared. Reuse the existing warning channel — do not invent a new diagnostic type.

- [ ] **Step 5: Run, verify pass.**
Run: `cargo test -p mcp-flowgate-core --test config_validation`
Expected: PASS (both tests).

- [ ] **Step 6: Commit.**
```bash
git add crates/mcp-flowgate-core/src/validate.rs crates/mcp-flowgate-core/tests/config_validation.rs
git commit -m "feat(validate): warn on output: writing an undeclared blackboard slot"
```

**Phase 1 acceptance:** `blackboard:` parses; undeclared `output:` targets warn (SPEC §6.1); no behaviour change for configs without `blackboard:`.

---

## Phase 2 — Transition records

**Guarantee tests:** `record_emitted_per_applied_transition`, `record_write_failure_aborts_transition`, `version_unchanged_when_record_write_fails`.

**Tasks:**
1. Add `schemas/transition-record.schema.json`; wire into the schema crate's `build.rs`; verify `cargo build -p mcp-flowgate-schema`.
2. In `error.rs`, add `RECORD_WRITE_FAILED` to the error enum/codes, with a message naming the workflow id and `seq`.
3. In `audit.rs`, define the `workflow.transition` event as a typed `AuditEvent` payload conforming to the schema. Reuse `AuditEvent` — no new sink trait.
4. In `runtime.rs`, on transition commit enforce **record-first ordering**: durably emit the transition record, *then* commit the snapshot. If the record emit fails, return `RECORD_WRITE_FAILED` and do not commit.

**Named assertions (must be exact, not paraphrased):**
- `record_write_failure_aborts_transition`: inject an `AuditSink` whose write returns `Err`; call `workflow.submit`; assert the result is `Err` with code `RECORD_WRITE_FAILED`.
- `version_unchanged_when_record_write_fails`: after the failed submit above, `workflow.get` reports the **same `version`** as before — proof the snapshot did not commit.
- `record_emitted_per_applied_transition`: a 3-hop deterministic chain emits exactly 3 records with `seq` 1,2,3.

**Acceptance:** SPEC §7 — snapshot authoritative, record-first, fail-fast; no committed-but-unrecorded transition is reachable.

---

## Phase 3 — Date-rotated File audit sink

**Guarantee tests:** `file_sink_rotates_on_interval`, `transition_and_audit_streams_split_by_name`.

**Tasks:**
1. Extend `FileAuditSink` with a rotation interval (`daily`/`hourly`/`weekly`) and `YYYY-MM-DD-{name}.log` naming. Direct writes — **not** the lossy non-blocking `tracing` path.
2. Route `workflow.transition` events to `…-transitions.log`, other events to `…-audit.log`, via one rotating writer.
3. Add `rotation:` to the `audit:` config block + schema.

**Named assertions:**
- `file_sink_rotates_on_interval`: with a clock stub advanced past the interval, a second event lands in a new dated file.
- `transition_and_audit_streams_split_by_name`: a transition event and an approval event land in differently-named files for the same date.

**Acceptance:** SPEC §7.4 — date-rotated, category-split, append-only files.

---

## Phase 4 — Versioned definitions

**Guarantee tests:** `instance_carries_definition_snapshot`, `config_edit_does_not_disturb_inflight_instance`, `archived_version_not_deleted`.

**Tasks:**
1. `version:` on the workflow definition (schema + parse); default-assigned when absent.
2. At `workflow.start`, resolve the definition and store the **complete snapshot with the instance** in the `WorkflowStore` (`model.rs` + store impls). Running instances resolve their definition from the snapshot, never from current config.
3. `hot_reload.rs`: SIGHUP adds new versions; never overwrites or removes a version a live instance references.

**Named assertions:**
- `instance_carries_definition_snapshot`: start an instance; mutate the source config; the instance's resolved definition is unchanged.
- `config_edit_does_not_disturb_inflight_instance`: an in-flight instance whose state was renamed in a reload still advances on its pinned definition.

**Acceptance:** SPEC §8 — pinning + per-instance snapshot; hot-reload additive; **no migrations** (SPEC §4).

---

## Phase 5 — Two-tier guidance

**Guarantee tests:** `guidance_string_interpolates_context`, `unresolved_placeholder_renders_stub_not_error`, `template_value_not_re_expanded`, `verb_with_space_rejected_at_load`, `response_surfaces_guidance_refs`.

**Tasks:**
1. Templating: `goal`/`guidance` strings interpolated against `$.context`/`$.workflow.*` at response render — single-pass, non-recursive, unresolved → marked stub.
2. `skills:` top-level fragment library (`{ <subject>: { verb, body } }`) in config + schema, with `verb` and `skills:` keys constrained by `pattern: "^[a-z][a-z0-9-]*$"` — **rejected at config load**, not lint-time.
3. `skills:` reference lists at workflow/state/transition scope.
4. Response `guidance` object gains `refs: [{verb, subject}]`; `gateway.describe` resolves a `subject` to its `body`.

**Named assertions:**
- `verb_with_space_rejected_at_load`: a config with `verb: "apply now"` fails **config load** (poka-yoke is prevention, not a check warning).
- `template_value_not_re_expanded`: a context value containing literal `{{x}}` appears verbatim in output.
- `response_surfaces_guidance_refs`: a state referencing a `skills:` entry yields a `guidance.refs` entry `{verb, subject}` and `gateway.describe(subject)` returns the body.

**Acceptance:** SPEC §5 — two-tier delivery; templating inline-only; verb/subject space-free by construction.

---

## Phase 6 — `check` use-before-def

**Guarantee tests:** `guard_reading_unwritten_slot_errors`, `guard_reading_summary_errors`, `template_unknown_slot_warns`.

**Tasks:**
1. Build the per-workflow reachability graph; for each `expr` guard / `{{ }}` template reading `$.context.X`, require a reachable predecessor transition whose `output:` writes `X`.
2. `$.context.summary` in any guard → **error** (model-authored content is never a guard input).
3. Dangling `skills:` ref → error; >4 refs surfaced at one scope → warn.

**Named assertions:**
- `guard_reading_unwritten_slot_errors`: a guard on slot `X` with no reachable writer → a `check` **error** (not warning).
- `guard_reading_summary_errors`: `expr: "$.context.summary == ..."` → `check` error.

**Acceptance:** SPEC §9, §11 — use-before-def is an error; runtime fail-fast remains the backstop.

---

## Self-review (completed)

- **Spec coverage:** §5 → Phase 5; §6 → Phase 1; §7 → Phases 2–3; §8 → Phase 4; §9/§11 → Phases 1+6. §10 (schemas) is folded into the phase that owns each schema. §4 cuts are enforced as negative acceptance criteria ("no migrations").
- **No placeholders:** Phase 1 carries literal test + code; Phases 2–6 carry exact files, named assertions, and acceptance tied to spec sections — full bite-sized expansion is JIT per the detailing strategy.
- **Type consistency:** `RECORD_WRITE_FAILED` used identically in Phase 2 tasks; `verb`/`subject` naming consistent §5 ↔ Phase 5.
