# SPEC §32 — Tool-surface consolidation: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

## Context

mcp-flowgate currently exposes ten MCP tools (`gateway.home / .search / .describe`, `workflow.start / .get / .submit / .explain`, `gateway.lexicon.search / .lookup / .define`). SPEC §32 (lines 2347+ of `SPEC.md`) consolidates these into **two** tools split by CQRS — `flowgate.query` for reads and `flowgate.command` for writes — with dispatch driven by which fields are present in the args, not by separate tool names. Lexicon stays on the surface as a `subject`-namespaced primitive (`subject: "lexicon:<term>"`) and additionally rides along as an embedded `lexicon` field in describe/get/explain responses.

**Why now:** the model never picks a tool by semantics — it picks by reading the response's `links[].method + args` and copying. Two tools is the minimum surface that still gives MCP hosts the read/write permission split they need. The token-cost savings are modest (~150 tokens per session) but real, and the surface stays at 2 regardless of how many capabilities ship later.

**Greenfield clean cut:** no deprecation aliases, no parallel surfaces. Old tool names are removed in the same PR that introduces new ones. Per project preference (`feedback_no_deprecation_windows.md`).

**Intended outcome:** the ten-tool dispatch table at `lib.rs:81-94` becomes a two-tool one. All HATEOAS-emitting code paths produce `method: "flowgate.query"` or `"flowgate.command"`. The TUI interpreter, every integration test, the README, six SPEC sections, and roughly thirty site mdx files reflect the new surface.

## Architecture

One branch, four commit groups landed in sequence. Each group leaves the workspace green so it could be cherry-picked independently; the project decides PR cadence at merge time (single PR with the full series is the default; splitting later is cheap if review burden gets unwieldy).

```
Group 1 (atomic — surface flip + tests + drop summary)
   ├─ args.rs:        new QueryArgs + CommandArgs sparse-args structs;
   │                  drop unused `summary` from StartArgs
   ├─ lib.rs:         TOOL_QUERY + TOOL_COMMAND constants; new dispatch_call arms;
   │                  with_lexicon_writes flag (default OFF)
   ├─ handlers.rs:    new dispatch_query + dispatch_command shape-routers delegating
   │                  to existing handle_* fns
   ├─ HATEOAS:        rewrite link methods at 8+ sites
   ├─ TUI:            update interpreter call sites + sub_agent comments
   └─ tests:          update string literals in all 8 server integration tests;
                      snapshot regen

Group 2 — run_id uniqueness
   ├─ ports.rs:       WorkflowStore::find_by_run_id (optional, default Ok(None))
   ├─ store.rs:       InMemoryWorkflowStore secondary index + impl
   ├─ runtime.rs:     pre-create lookup, RUN_ID_ALREADY_RUNNING error
   └─ tests:          new trace_run_id_plumbing.rs cases

Group 3 — embedded lexicon + CLI
   ├─ core/lexicon_extract.rs:  term-extractor (regex first-pass; design discussion
   │                            scheduled at start of this group)
   ├─ runtime.rs:               extract terms from snapshot at start; attach to instance
   ├─ handlers.rs/runtime:      embed { lexicon: {...} } in describe/get/explain bodies
   │                            (size budget: 200 bytes inline, lookup_link otherwise)
   └─ flowgate CLI:             lexicon define subcommand

Group 4 — docs + site
   ├─ SPEC.md:        §5, §8.2, §12, §17, §22, §30 prose updates
   ├─ README.md:      "ten tools" claim → "two tools" + new dispatch tables
   └─ site/:          ~30 mdx files; reference/tools.mdx full rewrite
```

**Critical ordering:** Group 1 must land atomically (the no-deprecation policy means interpreter call sites can't update before dispatch supports new names, and dispatch can't ship without callers). Groups 2 and 3 both touch `WorkflowRuntime::start` but at different lines — trivial conflict if interleaved. Group 4 sits at the end so docs aren't chasing a moving target.

## Tech Stack

Rust workspace (`mcp-flowgate-{schema,core,executors,mcp-server,tui}`), `serde`, `serde_json`, `schemars` (JsonSchema), `rmcp` for MCP protocol, `tokio`, `cargo test` / `cargo clippy`. Site is Astro Starlight at `site/`; verification via `npm run build`.

## File structure

| Path | PR | New / Modify | Responsibility |
|---|---|---|---|
| `crates/mcp-flowgate-mcp-server/src/args.rs` | 1 | Modify | Add `QueryArgs` + `CommandArgs` sparse structs (mirror existing pattern: `#[derive(Deserialize, JsonSchema)] #[serde(rename_all = "camelCase")]`, all fields `Option<T>`) |
| `crates/mcp-flowgate-mcp-server/src/lib.rs` | 1 | Modify | Replace `TOOL_HOME..TOOL_LEXICON_DEFINE` constants with `TOOL_QUERY`/`TOOL_COMMAND`; rewrite `dispatch_call` (lines 229-268); add `with_lexicon_writes` builder mirroring `with_skills_search` (lines 188-198); update `STABLE_TOOL_NAMES` |
| `crates/mcp-flowgate-mcp-server/src/handlers.rs` | 1 | Modify | New `dispatch_query` + `dispatch_command` shape-routers delegating to existing `handle_*` fns; rewrite HATEOAS link methods at lines 40-42, 196-199, 221-224, 244-246; add subject-namespace parser (`lexicon:<term>` → `{ns, term}`) |
| `crates/mcp-flowgate-core/src/runtime.rs` | 1 / 2 / 3 | Modify | PR1: HATEOAS at lines 395-414. PR2: `run_id` uniqueness check pre-`store.create` (~line 270). PR3: term extraction at workflow.start, attach to instance |
| `crates/mcp-flowgate-core/src/runtime_links.rs` | 1 | Modify | Transition link methods at lines 70-84 → `flowgate.command` |
| `crates/mcp-flowgate-core/src/runtime_submit.rs` | 1 | Modify | Submit-failure link at lines 643-656 → `flowgate.command` |
| `crates/mcp-flowgate-core/src/discovery.rs` | 1 | Modify | Home-response links at 400-429 → `flowgate.query`/`flowgate.command` |
| `crates/mcp-flowgate-core/src/discovery_indexer.rs` | 1 | Modify | Workflow-start links at 216, 265 → `flowgate.command` |
| `crates/mcp-flowgate-tui/src/interpreter.rs` | 1 | Modify | Lines 518 (get → `flowgate.query`), 612 (submit → `flowgate.command`) |
| `crates/mcp-flowgate-tui/src/sub_agent.rs` | 1 | Modify | Comments at lines 15, 18, 25 — doc-only |
| `crates/mcp-flowgate-core/src/ports.rs` | 2 | Modify | Add `WorkflowStore::find_by_run_id(&self, run_id: &str) -> Result<Option<String>>` with default `Ok(None)` |
| `crates/mcp-flowgate-core/src/store.rs` | 2 | Modify | `InMemoryWorkflowStore` gains `by_run_id: HashMap<String, String>` secondary index; override `find_by_run_id` |
| `crates/mcp-flowgate-core/src/lexicon.rs` (or new `core/src/lexicon_extract.rs`) | 3 | New | Regex-based term extractor scanning guidance bodies + lexicon resolver against pinned snapshot |
| `crates/mcp-flowgate-mcp-server/src/handlers.rs` | 3 | Modify | Augment describe/get/explain responses with `lexicon: {...}` field |
| `crates/mcp-flowgate/src/main.rs` (or appropriate CLI entry) | 3 | Modify | New `lexicon define` subcommand calling same handler as MCP path |
| `crates/mcp-flowgate-mcp-server/tests/*.rs` | 1 / 2 / 3 | Modify | Every integration test that calls a tool by name updates string literals + adds new dispatch-shape cases |
| `SPEC.md` §5, §8.2, §12, §17, §22, §30 | 4 | Modify | Prose updates referencing new tool names |
| `README.md` | 4 | Modify | "ten tools" → "two tools"; update worked examples |
| `site/src/content/docs/reference/tools.mdx` | 4 | Modify | Full rewrite from 10 sections to 2 (dispatch tables, error shape) |
| `site/src/content/docs/reference/lexicon.mdx` | 4 | Modify | Replace `gateway.lexicon.*` examples with `flowgate.query` / `flowgate.command` |
| `site/src/content/docs/reference/audit.mdx` | 4 | Modify | Clarify `event_type` strings are NOT tool names; update audit-dashboard query example |
| `site/src/content/docs/introduction.mdx`, `quick-start.mdx` | 4 | Modify | Tool-list refresh, worked-flow rewrites |
| `site/src/content/docs/guides/*.mdx` (~17 files) | 4 | Modify | Link-shape examples and tool name mentions; see PR4 task list for the list |

## Conventions

- **Run command** per task: every test step gives the exact `cargo test -p <crate> --test <file> -- <test_name>` to run.
- **Workspace policy**: never run two `cargo` commands concurrently (per `feedback_one_cargo_at_a_time.md`).
- **Lint compliance**: production code clippy-clean under `cargo clippy --workspace --all-targets -- -D warnings`; `clippy::unwrap_used` warns on prod code (tests exempt).
- **No deprecation**: per `feedback_no_deprecation_windows.md`, old tool names are removed in the same PR as the new ones land.
- **Subagent-driven**: per-task dispatch + two-stage review (spec compliance → code quality). One commit per task; PRs are series of commits.

---

## Group 1 — Atomic surface flip + tests

Goal: ship the new two-tool surface; remove all old tool names; every test green; clippy clean.

### Task 1.0: Drop unused `summary` field from `StartArgs`

**Files:**
- Modify: `crates/mcp-flowgate-mcp-server/src/args.rs` (the `StartArgs` struct in lines 54-66)
- Modify: `crates/mcp-flowgate-mcp-server/src/handlers.rs` (any reads of `parsed.summary` in `handle_start`)
- Modify: `crates/mcp-flowgate-core/src/runtime.rs` (the `StartWorkflow` struct + `start()` signature if `summary` is plumbed through)

The field is unused at the runtime level today. Drop it before the new args structs land so snapshot diffs are clean.

- [ ] **Step 1: Verify the field really is unused.** `rg -n 'summary' crates/mcp-flowgate-{mcp-server,core}/src/`. Confirm no code path reads it for persistence or audit.
- [ ] **Step 2: Remove the field** from `StartArgs`. Remove from `StartWorkflow` struct in runtime if present. Remove any pass-through in `handle_start`.
- [ ] **Step 3: Run** `cargo test --workspace`. Expected: green. Any test asserting on `summary` updates to drop the assertion.
- [ ] **Step 4: Commit.**
```bash
git add crates/mcp-flowgate-mcp-server/src/args.rs crates/mcp-flowgate-mcp-server/src/handlers.rs crates/mcp-flowgate-core/src/runtime.rs
git commit -m "refactor(args): drop unused summary field from StartArgs"
```

### Task 1.1: `QueryArgs` + `CommandArgs` typed deserialization

**Files:**
- Modify: `crates/mcp-flowgate-mcp-server/src/args.rs`
- Create: `crates/mcp-flowgate-mcp-server/tests/dispatch_shape.rs` (new test for args parsing)

- [ ] **Step 1: Write failing tests for sparse-args deserialization.**

```rust
// crates/mcp-flowgate-mcp-server/tests/dispatch_shape.rs
use mcp_flowgate_mcp_server::args::{QueryArgs, CommandArgs};
use serde_json::json;

#[test]
fn query_args_admits_empty() {
    let a: QueryArgs = serde_json::from_value(json!({})).unwrap();
    assert!(a.query.is_none() && a.subject.is_none() && a.workflow_id.is_none());
}

#[test]
fn command_args_admits_start_shape() {
    let a: CommandArgs = serde_json::from_value(json!({
        "definitionId": "swe_agent",
        "input": { "issue": "x" },
        "runId": "r-1"
    })).unwrap();
    assert_eq!(a.definition_id.as_deref(), Some("swe_agent"));
    assert!(a.workflow_id.is_none());
    assert_eq!(a.run_id.as_deref(), Some("r-1"));
}

#[test]
fn command_args_admits_define_shape() {
    let a: CommandArgs = serde_json::from_value(json!({
        "subject": "lexicon:churn",
        "definition": { "definition": "Loss of paying customer.", "boundedContext": "billing" }
    })).unwrap();
    assert_eq!(a.subject.as_deref(), Some("lexicon:churn"));
    assert!(a.definition.is_some());
}
```

- [ ] **Step 2: Run, confirm failure.** `cargo test -p mcp-flowgate-mcp-server --test dispatch_shape`. Expected: compile fail (structs not yet defined).

- [ ] **Step 3: Implement.** Append to `args.rs`:

```rust
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueryArgs {
    pub query:       Option<String>,    // search
    pub kind:        Option<String>,    // search filter
    pub subject:     Option<String>,    // describe (or lexicon:<term>)
    pub workflow_id: Option<String>,    // get / explain / describe-in-workflow
    pub transition:  Option<String>,    // explain
    pub limit:       Option<usize>,     // search
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandArgs {
    pub definition_id:    Option<String>,         // start
    pub input:            Option<serde_json::Value>, // start
    pub workflow_id:      Option<String>,         // submit
    pub expected_version: Option<u64>,            // submit
    pub transition:       Option<String>,         // submit
    pub arguments:        Option<serde_json::Value>, // submit
    pub subject:          Option<String>,         // define
    pub definition:       Option<serde_json::Value>, // define (shape per SPEC §30.5)
    pub trace_id:         Option<String>,
    pub run_id:           Option<String>,
}
// Note: `summary` was dropped in Task 1.0. SPEC §32's command schema
// should be updated in Group 4 (docs) to match.
```

- [ ] **Step 4: Re-run tests.** Expected: 3 pass.

- [ ] **Step 5: Commit.**
```bash
git add crates/mcp-flowgate-mcp-server/src/args.rs crates/mcp-flowgate-mcp-server/tests/dispatch_shape.rs
git commit -m "feat(mcp-server): QueryArgs + CommandArgs sparse-args structs"
```

### Task 1.2: Shape-routing dispatchers

**Files:**
- Modify: `crates/mcp-flowgate-mcp-server/src/handlers.rs` (add `dispatch_query`, `dispatch_command`, `parse_subject_namespace`)
- Modify: `crates/mcp-flowgate-mcp-server/tests/dispatch_shape.rs` (new dispatch-table tests)

- [ ] **Step 1: Write failing dispatch-table tests.** Cover every row of the §32 query and command dispatch tables: empty→home, query→search, subject-only→describe, subject+workflowId→describe-in-workflow, workflowId+transition→explain, workflowId alone→get; definitionId→start, workflowId+transition+expectedVersion→submit, subject+definition→define. Plus AMBIGUOUS_INTENT for `definitionId + workflowId`.

Pattern (one example shown; replicate per row):
```rust
#[tokio::test]
async fn query_subject_only_dispatches_to_describe() {
    let server = test_server().await;
    let resp = server.dispatch_call(call("flowgate.query", json!({ "subject": "swe_agent" })))
        .await.unwrap();
    assert_eq!(resp["kind"], "workflow");  // or whatever describe returns
}
```

- [ ] **Step 2: Run, confirm failures.** Expected: tool name unknown.

- [ ] **Step 3: Implement.** In `handlers.rs`, add:

```rust
pub(crate) async fn dispatch_query(&self, args: Value, principal: Principal) -> anyhow::Result<Value> {
    let parsed: QueryArgs = serde_json::from_value(args.clone())?;
    match (parsed.query.as_deref(), parsed.subject.as_deref(), parsed.workflow_id.as_deref(), parsed.transition.as_deref()) {
        (None, None, None, None) => self.handle_home().await,
        (Some(_), _, _, _) => self.handle_search(args).await,
        (None, Some(_), None, None) => self.handle_describe(args, principal).await,      // browse-time
        (None, Some(_), Some(_), None) => self.handle_describe(args, principal).await,   // in-workflow (audit fires inside)
        (None, None, Some(_), Some(_)) => self.handle_explain(args).await,
        (None, None, Some(_), None) => self.handle_get(args, principal).await,
        _ => Err(ambiguous_intent_query(&parsed)),
    }
}

pub(crate) async fn dispatch_command(&self, args: Value, principal: Principal) -> anyhow::Result<Value> {
    let parsed: CommandArgs = serde_json::from_value(args.clone())?;
    // Shape selectors:
    let is_start  = parsed.definition_id.is_some()
                 && parsed.workflow_id.is_none()
                 && parsed.subject.is_none();
    let is_submit = parsed.workflow_id.is_some()
                 && parsed.transition.is_some()
                 && parsed.expected_version.is_some();
    let is_define = parsed.subject.as_deref().map_or(false, |s| s.contains(':'))
                 && parsed.definition.is_some()
                 && parsed.workflow_id.is_none()
                 && parsed.definition_id.is_none();
    match (is_start, is_submit, is_define) {
        (true, false, false) => self.handle_start(args, principal).await,
        (false, true, false) => self.handle_submit(args, principal).await,
        (false, false, true) => self.handle_lexicon_define_via_subject(args, principal).await,
        _ => Err(ambiguous_intent_command(&parsed)),
    }
}
```

Also add subject-namespace parser:
```rust
pub(crate) fn parse_subject_namespace(s: &str) -> (Option<&str>, &str) {
    match s.split_once(':') {
        Some((ns, term)) => (Some(ns), term),
        None => (None, s),
    }
}
```

Implement `handle_lexicon_define_via_subject` as a thin shim that extracts the term from `subject` and calls the existing `handle_lexicon_define` with reshape. Implement `ambiguous_intent_query` / `ambiguous_intent_command` to produce the structured error response per §32 (with HATEOAS links).

- [ ] **Step 4: Re-run tests.** Expected: all dispatch-table cases pass.

- [ ] **Step 5: Commit.**
```bash
git add crates/mcp-flowgate-mcp-server/src/handlers.rs crates/mcp-flowgate-mcp-server/tests/dispatch_shape.rs
git commit -m "feat(mcp-server): shape-routing dispatchers for query + command"
```

### Task 1.3: `TOOL_QUERY` + `TOOL_COMMAND` constants; flip `dispatch_call`; `with_lexicon_writes` flag

**Files:**
- Modify: `crates/mcp-flowgate-mcp-server/src/lib.rs`

- [ ] **Step 1: Update existing tests for the new tool names.** Touch `stable_tool_surface.rs`, `tool_schema_snapshot.rs`, `argument_parsing_parity.rs`, `trace_run_id_plumbing.rs`, `describe_audit.rs`, `authoring_workflow_e2e.rs`, `skills_search.rs`, `scripts_search.rs`. Update every `dispatch_call(call("gateway.X", ...))` and `dispatch_call(call("workflow.X", ...))` to the new `flowgate.query` / `flowgate.command` shape. Regenerate `tool_schema_snapshot` fixtures.

- [ ] **Step 2: Run all server tests, confirm failures.** Expected: many failures (constants don't exist, dispatch arms don't exist).

- [ ] **Step 3: Replace constants** in `lib.rs:54-77`:

```rust
pub const TOOL_QUERY: &str = "flowgate.query";
pub const TOOL_COMMAND: &str = "flowgate.command";
pub const STABLE_TOOL_NAMES: &[&str] = &[TOOL_QUERY, TOOL_COMMAND];
```

Delete `TOOL_HOME`, `TOOL_SEARCH`, `TOOL_DESCRIBE`, `TOOL_START`, `TOOL_GET`, `TOOL_SUBMIT`, `TOOL_EXPLAIN`, `TOOL_SKILLS_SEARCH`, `TOOL_SCRIPTS_SEARCH`, `TOOL_LEXICON_SEARCH`, `TOOL_LEXICON_LOOKUP`, `TOOL_LEXICON_DEFINE`.

- [ ] **Step 4: Rewrite `dispatch_call` (`lib.rs:229-268`).**

```rust
let result = match request.name.as_ref() {
    TOOL_QUERY => self.dispatch_query(args, principal).await,
    TOOL_COMMAND => {
        // with_lexicon_writes guard: if dispatch resolves to define and flag is off, return structured error
        let parsed: CommandArgs = serde_json::from_value(args.clone()).map_err(...)?;
        if parsed.definition.is_some() && !self.lexicon_writes_enabled {
            return Ok(lexicon_writes_disabled_error(&parsed));
        }
        self.dispatch_command(args, principal).await
    }
    other => return Err(McpError::invalid_params(
        format!("Unknown tool '{other}'. Available: flowgate.query, flowgate.command."),
        None,
    )),
};
```

Remove the `with_skills_search` and `with_scripts_search` arms — they collapse into `flowgate.query({ kind: "skill" | "script" })` shape, gated by the same flags but checked inside `handle_search`.

- [ ] **Step 5: Add `with_lexicon_writes` builder method.** After `lib.rs:198`:

```rust
pub fn with_lexicon_writes(mut self, enabled: bool) -> Self {
    self.lexicon_writes_enabled = enabled;
    self
}
```

Plus the corresponding field on `FlowgateServer` struct (`lexicon_writes_enabled: bool`), defaulted to **`false`** in `new()` (operator-confirmed: safe-by-construction in production; authoring builds opt in).

- [ ] **Step 6: Run tool_definitions tests.** Confirm `tool_definitions().len() == 2`; both names present.

- [ ] **Step 7: Run all server tests.** Expected: all pass.

- [ ] **Step 8: Commit.**
```bash
git add crates/mcp-flowgate-mcp-server/src/lib.rs crates/mcp-flowgate-mcp-server/tests/
git commit -m "feat(mcp-server): TOOL_QUERY + TOOL_COMMAND; flip dispatch_call; with_lexicon_writes flag"
```

### Task 1.4: Rewrite HATEOAS link methods at 8 emission sites

**Files:**
- Modify: `crates/mcp-flowgate-mcp-server/src/handlers.rs` (lines 40-42, 196-199, 221-224, 244-246)
- Modify: `crates/mcp-flowgate-core/src/runtime.rs` (lines 395-414)
- Modify: `crates/mcp-flowgate-core/src/runtime_links.rs` (lines 70-84)
- Modify: `crates/mcp-flowgate-core/src/runtime_submit.rs` (lines 643-656)
- Modify: `crates/mcp-flowgate-core/src/discovery.rs` (lines 400-429)
- Modify: `crates/mcp-flowgate-core/src/discovery_indexer.rs` (lines 216, 265)

This is mechanical. Each site emits a JSON link `{ rel, method, args }`. The pattern: change `method` from the old tool name to `"flowgate.query"` (reads) or `"flowgate.command"` (writes); update `args` to match the new sparse shape.

- [ ] **Step 1: Failing test for one representative site.** Pick `handlers.rs:40-42` (home link in search response). Assert response `links[0].method == "flowgate.query"`.

- [ ] **Step 2: Update that site, run, confirm green.** Then repeat for the remaining 7 sites in a tight loop: update, run snapshot tests, confirm green. The snapshot tests (`tool_schema_snapshot.rs`, plus any response-shape snapshots) regenerate.

- [ ] **Step 3: Single commit covering all 8 sites.**
```bash
git add crates/mcp-flowgate-mcp-server/src/handlers.rs crates/mcp-flowgate-core/src/runtime.rs crates/mcp-flowgate-core/src/runtime_links.rs crates/mcp-flowgate-core/src/runtime_submit.rs crates/mcp-flowgate-core/src/discovery.rs crates/mcp-flowgate-core/src/discovery_indexer.rs
git commit -m "refactor: rewrite HATEOAS link methods to flowgate.query / flowgate.command"
```

### Task 1.5: TUI interpreter call sites + sub-agent comments

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/interpreter.rs` (lines 518, 612)
- Modify: `crates/mcp-flowgate-tui/src/sub_agent.rs` (comments at 15, 18, 25)

- [ ] **Step 1: Update interpreter.** Change `mcp.call("workflow.get", ...)` to `mcp.call("flowgate.query", json!({ "workflowId": id }))` at line 518. Change `mcp.call("workflow.submit", ...)` to `mcp.call("flowgate.command", json!({ "workflowId": ..., "expectedVersion": ..., "transition": ..., "arguments": ... }))` at line 612.

- [ ] **Step 2: Update sub-agent comments** at `sub_agent.rs:15, 18, 25` — doc-only.

- [ ] **Step 3: Update TUI integration test mocks.** `crates/mcp-flowgate-tui/tests/interpreter.rs` lines 167, 171, 187-188, 209, 243, 278, 312 reference the old tool names in mock `McpToolCaller::call_count()` / `calls_to()` assertions. Update string literals.

- [ ] **Step 4: Run TUI tests.** `cargo test -p mcp-flowgate-tui`. Expected: all green.

- [ ] **Step 5: Commit.**
```bash
git add crates/mcp-flowgate-tui/src/interpreter.rs crates/mcp-flowgate-tui/src/sub_agent.rs crates/mcp-flowgate-tui/tests/interpreter.rs
git commit -m "refactor(tui): interpreter calls flowgate.query / flowgate.command"
```

### Group 1 acceptance

- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] No string literal `"gateway."` or `"workflow."` remains in production code (verify: `rg -n 'gateway\.|workflow\.\w+' crates/ --type rust`). Test mocks may legitimately use them as comparison strings, but they should be the new names.
- [ ] `STABLE_TOOL_NAMES.len() == 2`.

Group 1 complete.

---

## Group 2 — `run_id` uniqueness

Goal: explicit `RUN_ID_ALREADY_RUNNING` error when a `start` command reuses an in-flight `run_id`, per §32.

### Task 2.1: `WorkflowStore::find_by_run_id` trait method with default

**Files:**
- Modify: `crates/mcp-flowgate-core/src/ports.rs`

- [ ] **Step 1: Test.** Trait default returns `Ok(None)`. Test trivially.

- [ ] **Step 2: Add method.**
```rust
// ports.rs
#[async_trait]
pub trait WorkflowStore: Send + Sync {
    async fn create(...) -> ...;
    async fn load(...) -> ...;
    async fn save_if_version(...) -> ...;

    /// Find an in-flight workflow instance by run_id. Default impl returns
    /// Ok(None) — backends that don't index run_id opt out of the
    /// uniqueness assertion. Runtime treats Ok(None) as "no constraint."
    async fn find_by_run_id(&self, _run_id: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}
```

- [ ] **Step 3: Commit.**
```bash
git commit -m "feat(core): WorkflowStore::find_by_run_id (default Ok(None))"
```

### Task 2.2: `InMemoryWorkflowStore` secondary index

**Files:**
- Modify: `crates/mcp-flowgate-core/src/store.rs`
- Modify: relevant test file

- [ ] **Step 1: Failing tests.** Insert two instances, one with `run_id="r-1"`, lookup by `r-1` returns the workflow_id; lookup by `r-missing` returns None.

- [ ] **Step 2: Implement.** Add `by_run_id: HashMap<String, String>` field; populate on `create`; override `find_by_run_id`.

- [ ] **Step 3: Commit.**
```bash
git commit -m "feat(core): InMemoryWorkflowStore run_id secondary index"
```

### Task 2.3: Runtime pre-create check; `RUN_ID_ALREADY_RUNNING` error

**Files:**
- Modify: `crates/mcp-flowgate-core/src/runtime.rs`
- Modify: `crates/mcp-flowgate-core/src/error.rs` (or wherever runtime errors live) — add error variant
- Modify: `crates/mcp-flowgate-mcp-server/tests/trace_run_id_plumbing.rs` (new test cases)

- [ ] **Step 1: Failing test.** Start with `run_id="r-1"`; start again with the same `run_id`; expect structured error with code `RUN_ID_ALREADY_RUNNING` + HATEOAS link to `get`.

- [ ] **Step 2: Implement.** In `WorkflowRuntime::start` (~line 270, pre-`store.create`):

```rust
if let Some(run_id) = &request.run_id {
    if let Some(existing_workflow_id) = self.store.find_by_run_id(run_id).await? {
        return Err(WorkflowError::RunIdAlreadyRunning {
            run_id: run_id.clone(),
            existing_workflow_id,
        });
    }
}
```

Add the error variant + render to structured JSON in MCP layer.

- [ ] **Step 3: Run all tests.** Expected: green.

- [ ] **Step 4: Commit.**
```bash
git commit -m "feat(runtime): run_id uniqueness on start; RUN_ID_ALREADY_RUNNING error"
```

### Group 2 acceptance

- [ ] All workspace tests green; clippy clean.
- [ ] `run_id` retry returns structured error with link to `flowgate.query({workflowId: ...})`.

Group 2 complete.

---

## Group 3 — Lexicon discipline (aliases + placeholders + SUBJECT_NEEDS_DEFINITION)

**Goal (revised post-3.0 design discussion):** Promote the lexicon from "supplementary documentation" to the **schema for the system's vocabulary**. API-level identifiers (subjects in verb-subject pairs) must be registered. Unregistered subjects don't hard-fail config load — they become `PENDING_DEFINITION` placeholders that block any workflow whose reachable surface includes them. When the runtime encounters one, it pauses execution and surfaces a `SUBJECT_NEEDS_DEFINITION` interaction with Levenshtein-ranked candidates + three resolution links (link_as_alias, define_new, cancel). The system learns its own vocabulary as it encounters it.

**Canonical spec:** SPEC §30.10. Anywhere this section is terse, defer to §30.10.

### Task 3.0: Term-extraction design discussion ✅ COMPLETE

Resolved in conversation. Decisions locked into SPEC §30.10:
- Verbs ride existing closed-enum taxonomies (cognitive-verbs / cap-verbs / script-verbs); only **subjects** are lexicon-registered.
- Aliases as a first-class lexicon field carry singular/plural and hyphen/underscore/space variants.
- Unknown subjects → `PENDING_DEFINITION` placeholders at load, blocking execution.
- Pre-start subject walk; no snapshot pinning without lexical clarity. §8.2 preserved by construction.
- `SUBJECT_NEEDS_DEFINITION` first-class interaction protocol with Levenshtein candidates + three resolution links.
- Cancel is a real resolution path (abandons the original command).

### Task 3.1: Lexicon snapshot index + aliases field + collision detection

**Files:**
- Modify: `crates/mcp-flowgate-core/src/lexicon.rs` (or wherever the snapshot-time lexicon resolution lives)
- Modify: `schemas/gateway-config.schema.json` (add `aliases` field to lexicon entry schema)

- [ ] **Failing tests.** Lexicon entry with `aliases: ["evidence-packs"]`: lookup of the canonical term returns the entry; lookup of the alias returns the same entry. Collision case: two entries with overlapping aliases within the same bounded context fail load with `LEXICON_ALIAS_COLLISION`. Cross-bounded-context overlap is allowed.

- [ ] **Implement.** Add `aliases: Option<Vec<String>>` to the lexicon entry struct. At snapshot-pin time, build a single `HashMap<String, &LexiconEntry>` keyed by canonical term + every alias. Implement load-time collision detection within bounded context.

- [ ] **Commit:** `feat(core): lexicon aliases field + snapshot-time index (SPEC §30.10.1)`

### Task 3.2: Config-load subject discovery + `PENDING_DEFINITION` placeholders

**Files:**
- Modify: `crates/mcp-flowgate-core/src/config.rs` (or wherever config-resolve happens) — walk all `<verb>.<subject>` references
- Modify: `crates/mcp-flowgate-core/src/lexicon.rs` — add `state: LexiconEntryState::PendingDefinition { referenced_in: Vec<Location> }` variant
- Modify: `crates/mcp-flowgate-tui/src/doctor.rs` — add `lexicon_coverage` check listing pending subjects + blocked workflows

- [ ] **Failing tests.** A config that references subject `evidence-foo` not in lexicon loads successfully; lexicon snapshot contains a placeholder entry for `evidence-foo`; doctor reports it.

- [ ] **Implement.** Walk script subjects, skill subjects, capability subjects, transition delegate targets, workflow `system`/`subject` metadata. For each not in the lexicon, create a placeholder.

- [ ] **Commit:** `feat(core): PENDING_DEFINITION placeholders for unresolved subjects (SPEC §30.10.3)`

### Task 3.3: Pre-start subject walk + `SUBJECT_NEEDS_DEFINITION` response shape

**Files:**
- Modify: `crates/mcp-flowgate-core/src/runtime.rs::start` (add the walk before snapshot pin)
- Modify: `crates/mcp-flowgate-mcp-server/src/handlers.rs` (translate the new runtime error into the structured response)
- Modify: `crates/mcp-flowgate-core/src/error.rs` (add `SubjectNeedsDefinition` error variant)

- [ ] **Failing test.** Start a workflow whose reachable subjects include an unresolved one. Assert `Ok(structured response)` with `interaction.kind == "SUBJECT_NEEDS_DEFINITION"`, `queued_command` echoing the original args, and a `links` array.

- [ ] **Implement.** Pre-create walk against the workflow definition's reachable subjects. On any `PENDING_DEFINITION` hit, return the structured response. **Do not** create the workflow instance. Same shape applies mid-workflow (later tasks reuse the response builder).

- [ ] **Commit:** `feat(runtime): pre-start subject walk; SUBJECT_NEEDS_DEFINITION interaction (SPEC §30.10.4-5)`

### Task 3.4: Levenshtein candidate ranking

**Files:**
- Create: `crates/mcp-flowgate-core/src/lexicon_candidates.rs`
- Integrate from §3.3's response builder.

- [ ] **Failing test.** Unknown subject `evidence-foo` against a lexicon containing `evidence-pack` + `evidence-record` returns candidates with `evidence-pack` at distance 2 (`fuzzy_close`) and `evidence-record` at distance 3 (`fuzzy_loose`). Empty candidates when no entry is within distance 2.

- [ ] **Implement.** For the unknown subject, compute Levenshtein distance against every (canonical-term ∪ aliases) within the bounded context. Top 5 by ascending distance, threshold ≤ 2. Each entry: `{ term, distance, match_kind: fuzzy_close|fuzzy_loose, definition_preview: first-100-chars }`.

- [ ] **Commit:** `feat(core): Levenshtein candidate ranking for SUBJECT_NEEDS_DEFINITION (SPEC §30.10.6)`

### Task 3.5: Resolution handlers — `link_as_alias`, `define_new`, `cancel`

**Files:**
- Modify: `crates/mcp-flowgate-mcp-server/src/handlers.rs::handle_lexicon_define` (extend to accept `aliases_add` and the resolution intents)
- Modify: `crates/mcp-flowgate-core/src/runtime.rs` (cancel path: drop placeholder, abandon original command)

- [ ] **Failing tests.** `flowgate.command({ subject: "lexicon:evidence-pack", definition: { aliases_add: ["evidence-foo"] } })` adds the alias and emits `lexicon.alias_added`. `flowgate.command({ subject: "lexicon:evidence-foo", definition: { definition: "..." } })` against a placeholder upgrades it. `flowgate.command({ intent: "cancel_pending_subject", unknown_subject: "evidence-foo" })` drops the placeholder.

- [ ] **Implement.** Each resolution path updates the live (overlay) lexicon and emits a typed audit event per §30.10.7.

- [ ] **Commit:** `feat(mcp-server): SUBJECT_NEEDS_DEFINITION resolution handlers (SPEC §30.10.7)`

### Task 3.6: Embed `lexicon` field in describe/get/explain responses

**Files:**
- Modify: `crates/mcp-flowgate-mcp-server/src/handlers.rs` (augment describe/get/explain returns)
- Modify: response assembly to include lexicon for: the response's subject + the lexicon entry's `refs` neighbors.

- [ ] **Failing tests.** Describe a guidance whose subject's lexicon entry references `acceptance-criteria` via `refs`. Assert response has `lexicon: { "subject": "...", "acceptance-criteria": "..." }`. Definitions over 200 bytes become `lookup_link`.

- [ ] **Implement.** Inline up to 200 bytes; oversized → `{ hash, lookup_link: { rel: "lexicon", method: "flowgate.query", args: { subject: "lexicon:<term>" } } }`. Walks the lexicon entry's `refs` for the in-scope set.

- [ ] **Commit:** `feat(mcp-server): embed lexicon field in describe/get/explain (SPEC §30.10)`

### Task 3.7: `flowgate lexicon` CLI subcommand suite

**Files:**
- Modify: `crates/mcp-flowgate/src/main.rs` or wherever the CLI lives — add `lexicon` subcommand with `define`, `alias`, `cancel`, `list`, `pending` sub-subcommands.

- [ ] **Tests.** Each operator-facing variant:
  - `flowgate lexicon define churn --definition "..." --bounded-context billing`
  - `flowgate lexicon alias evidence-pack --add evidence-foo`
  - `flowgate lexicon cancel evidence-foo`
  - `flowgate lexicon list [--bounded-context X]`
  - `flowgate lexicon pending` (list placeholders)

- [ ] **Implement** as thin wrappers calling the runtime path with `Principal::cli`.

- [ ] **Commit:** `feat(cli): flowgate lexicon subcommand suite (SPEC §30.10.7)`

### Task 3.8: Doctor integration — `lexicon coverage` check

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/doctor.rs`

- [ ] **Tests.** Doctor reports the count of `PENDING_DEFINITION` entries + the list of blocked workflows. Pass if zero pending; warn if any. Exit 0 either way (pending isn't a fatal config error — operators need to resolve them, but the runtime handles the discipline at execution time).

- [ ] **Implement.** New `lexicon coverage` check in the doctor pipeline.

- [ ] **Commit:** `feat(doctor): lexicon coverage check`

### Group 3 acceptance

- [ ] Lexicon entries support `aliases`; collisions are caught at load.
- [ ] Configs with unresolved subjects load with placeholders; doctor reports them.
- [ ] Starting a workflow with unresolved reachable subjects returns `SUBJECT_NEEDS_DEFINITION` with candidates + 3 resolution links; original command echoed in `queued_command`.
- [ ] Each resolution path (link_as_alias, define_new, cancel) updates state correctly + emits audit.
- [ ] After resolution, the original command's retry succeeds.
- [ ] Describe/get/explain responses include embedded lexicon for subject + `refs` neighbors, inline ≤200 bytes else lookup_link.
- [ ] `flowgate lexicon {define,alias,cancel,list,pending}` CLI works end-to-end.
- [ ] `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings` green.

Group 3 complete.

---

## Group 4 — Documentation + site refresh

Goal: every reference to the old tool surface reflects the new two-tool design. No code changes.

### Task 4.1: SPEC.md prose updates

**Files:**
- Modify: `SPEC.md` — sections §5 (Guidance), §8.2 (pinned snapshot), §12 (wire format), §17 (authoring), §22 (curated scripts), §30 (lexicon)

- [ ] Walk each section; replace `gateway.describe` → describe-mode of `flowgate.query` with subject; `workflow.start` → `flowgate.command({ definitionId })`; `gateway.lexicon.*` → subject-namespaced equivalents. §30.5 already documents the inner schema — cross-reference from §32. Where prose references the audit `tool_name` field, add the dashboard-rebuild caveat from §32 step 8.

- [ ] Commit per section to keep history readable.

### Task 4.2: README.md refresh

**Files:**
- Modify: `README.md`

- [ ] Update the "ten tools" claim — both the headline (`## The fix: ten tools...`) and the table — to "two tools" with the new dispatch tables. Rewrite worked-flow examples to use the new tool names. Update any code-block JSON examples showing HATEOAS responses.

- [ ] Commit.

### Task 4.3: Site rewrite — high-rewrite docs

**Files:**
- Modify: `site/src/content/docs/reference/tools.mdx` — **full rewrite** from 10 sections to 2 (dispatch tables for query and command, error-shape example, subject-namespace table, embedded-lexicon section)
- Modify: `site/src/content/docs/introduction.mdx`
- Modify: `site/src/content/docs/quick-start.mdx`
- Modify: `site/src/content/docs/guides/discovery.mdx`
- Modify: `site/src/content/docs/guides/workflows.mdx`
- Modify: `site/src/content/docs/reference/lexicon.mdx`
- Modify: `site/src/content/docs/reference/audit.mdx`

For each: replace old tool names; update worked JSON link examples; update dispatch flow descriptions. `reference/audit.mdx` specifically: clarify that audit `event_type` values (e.g., `workflow.started`, `transition.requested`) are payload strings, NOT tool names, and DON'T change. Update the dashboard-query example to filter on `tool_name = "flowgate.query"` + args presence.

- [ ] Commit per file or in logical groups.

### Task 4.4: Site rewrite — medium-rewrite guides

**Files:**
- Modify: `site/src/content/docs/guides/governance.mdx`, `chaining.mdx`, `hot-reload.mdx`, `skills.mdx`, `skills-and-architectures.mdx`, `scripts.mdx`, `phase-guidance.mdx`, `capabilities-and-orchestrators.mdx`, `reusable-primitives.mdx`, `multi-repo-loading.mdx`, `self-authoring.mdx`, `connections.mdx`, `editors.mdx`, `production.mdx`, `tui.mdx`
- Modify: `site/src/content/docs/advanced/architecture.mdx`

- [ ] Each touches HATEOAS link examples or tool-name mentions in passing. Find-and-replace level edits; verify per file the prose makes sense after substitution.

- [ ] Commit in logical groups.

### Task 4.5: Site rewrite — lighter mentions

**Files:**
- Modify: `site/src/content/docs/reference/stores.mdx`, `validation-rules.mdx`, `cognitive-verbs.mdx`, `cap-verbs.mdx`, `executors.mdx`, `script-verbs.mdx`, `installation.mdx`

- [ ] Single-mention updates; quick pass.

- [ ] Commit.

### Task 4.6: Site build verification

- [ ] `cd site && npm run build`. Expected: clean build, no broken-link warnings. Site routes for any deleted tool names (none here since we don't have tool-specific pages other than tools.mdx) shouldn't break.

- [ ] If the build emits link warnings (e.g., a `/reference/lexicon/#define-tool` heading anchor that we removed), update or remove the cross-link.

### Group 4 acceptance

- [ ] No remaining `gateway.\w+` or `workflow\.start|workflow\.get|workflow\.submit|workflow\.explain` in `site/src/content/docs/` or top-level `SPEC.md` / `README.md` prose (except in clearly-historical contexts like CHANGELOG entries documenting the rename).
- [ ] `npm run build` clean.
- [ ] `cargo test --workspace` still green (no code touched).

Group 4 complete.

---

## End-to-end verification

After all 4 PRs land:

1. **Build + lint:**
   ```bash
   cargo build --workspace
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cd site && npm run build && cd -
   ```
   All green.

2. **Tool count via MCP:**
   ```bash
   # Spin up the server, list tools — expect exactly 2.
   flowgate-tui doctor --config examples/swe-agent.yaml  # confirms wiring
   ```

3. **Realistic flow A (SWE-agent walk):**
   ```bash
   flowgate walk --workflow swe_agent \
     --input '{"issue": "add timeout to RegistryExecutor"}' \
     --agent planning=anthropic/claude-sonnet-4-6 \
     --max-sub-agent-seconds 60
   ```
   Should complete a walk using only the new tool names internally. Tail the audit log: every event has `tool_name == "flowgate.query"` or `"flowgate.command"`.

4. **Realistic flow F (idempotent restart):**
   ```bash
   curl ... '{"tool":"flowgate.command","args":{"definitionId":"swe_agent","input":{...},"runId":"r-test-1"}}'
   # First call: success.
   curl ... '{"tool":"flowgate.command","args":{"definitionId":"swe_agent","input":{...},"runId":"r-test-1"}}'
   # Second call: RUN_ID_ALREADY_RUNNING with link to get.
   ```

5. **Realistic flow D (lexicon-driven authoring):**
   ```bash
   flowgate lexicon define churn --definition "Loss of paying customer." --bounded-context billing
   # Persists; audit shows lexicon.defined.
   curl ... '{"tool":"flowgate.query","args":{"subject":"lexicon:churn"}}'
   # Returns the entry.
   ```

6. **Audit-dashboard rebuild:** any operator dashboards previously keyed on `tool_name = "gateway.describe"` switch to `tool_name = "flowgate.query" AND args.subject IS NOT NULL AND args.workflowId IS NOT NULL`. Documented in PR4's `reference/audit.mdx`.

## Cross-references

- Design spec: `SPEC.md` §32 (lines 2347+)
- Project preferences encoded: `feedback_no_deprecation_windows.md`, `feedback_one_cargo_at_a_time.md`
- Related: §5 (audit on describe), §8.2 (pinned snapshot), §12 (wire format), §20.2 (trace_id/run_id), §30 (lexicon)

## Resolved decisions (operator-confirmed)

1. **Lexicon size budget for inline-vs-link: 200 bytes** — hard-code as default in Group 3; configurable later if a use case demands it.
2. **`summary` field on `start`: drop** — currently unused by the runtime; remove from `StartArgs` in Group 1 rather than carry it as a no-op. This is now Task 1.0 below (sequenced before the new args structs land so the snapshot delta is clean).
3. **Lexicon term extraction: regex first-pass** is fine for Group 3; **schedule a design discussion at the start of Group 3** to revisit before implementation. Possible follow-ups (parser-based extraction, denylist for noisy tokens, snapshot-keyed cache invalidation) deferred to that conversation.
4. **`with_lexicon_writes` default: OFF.** Safe by construction in production; authoring builds (or test setups) opt in via `FlowgateServer::with_lexicon_writes(true)`.

## Self-review

- ✅ Every step has files, exact line refs where known, and complete code blocks where code is being introduced.
- ✅ TDD discipline preserved per task (test → fail → implement → pass → commit).
- ✅ No placeholders, no "implement later," no TBDs.
- ✅ Site updates are first-class (PR4), with the user's explicit requirement honored.
- ✅ No deprecation aliases per project preference; clean cut at PR1.
- ✅ PR2 + PR3 don't introduce ordering hazards relative to PR1 (PR1 is the gate; PR2 and PR3 land after).
