# StructureOS — feedback log

Issues, friction, and improvement ideas encountered while using `structureos-mcp`
on this repo. Kept here so the tool can be improved.

> Scope: structureos invoked via MCP from a Claude Code session against
> `/home/mc/working/mcp-flowgate` on 2026-05-24. `evidence_tier: syntactic`.

## Findings to address upstream

### 1. False-positive "unused import" / "dead code" from macro-expanded usage

**Observed:** 137 SOS027 (unused imports) + 262 SOS026 (dead code). Sampled the
SOS027 stream — most entries are imports legitimately used via macro
expansion that the syntactic-tier analyser can't see.

**Concrete examples (verified in code, all compile + clippy-clean):**

| File | Import flagged as unused | Actual use |
|---|---|---|
| `crates/mcp-flowgate/src/main.rs` | `Parser`, `Subcommand` (from `clap`) | `#[derive(Parser)]`, `#[derive(Subcommand)]` |
| `crates/mcp-flowgate-core/src/audit.rs` | `Deserialize`, `Serialize`, `async_trait`, `Datelike` | `#[derive(Serialize, Deserialize)]`, `#[async_trait]`, chrono `.iso_week()` |
| `crates/mcp-flowgate-core/benches/*.rs` | `criterion_group`, `criterion_main` | invoked via `criterion_main!()` macro |
| `crates/mcp-flowgate/src/main.rs` | `Context` (from `anyhow`) | `.with_context(...)` extension method |

**Impact:** These two diagnostic IDs together account for **74%** of the
"info" volume in the scan (399 of 539 items) and turn the response into noise.
Worse, the recommendation ("Remove the import if unused") is actively dangerous
advice for derive macros — following it breaks the build.

**Suggested fixes (any one helps):**
- Lower the severity to `hint` (or hide entirely) until the analyser can
  rule in macro use.
- Move SOS026/SOS027 behind an opt-in flag (`--include-syntactic-only-rules`)
  until macro-aware analysis lands.
- Add a "trust the compiler" mode: if `cargo check` is green, suppress these
  classes entirely (Rust's own compiler already errors on truly-unused imports).
- Add an allow-list of well-known macro-trait imports (`serde::{Serialize,
  Deserialize}`, `clap::Parser`, `async_trait::async_trait`, `thiserror::Error`,
  `criterion::{criterion_group, criterion_main}`) as a baseline.

### 2. Test files flagged as god files

**Observed:** `tests/stress_scenarios.rs` (1525 LOC, 56 fns) and
`tests/deterministic_chain.rs` (896 LOC) flagged as SOS001 failures, blocking
the gate.

**Impact:** Most projects accept large test files (one scenario per fn). The
gate-CLOSED outcome forces a noise-vs-signal tradeoff: either split tests for
no functional benefit or accept a permanently red gate.

**Suggested fix:** Different (laxer) thresholds for files under `tests/`,
`benches/`, `fuzz/`, and `examples/` — or path-aware exclusion entirely. The
metrics that matter for production code (LCOM, broker_ratio, fan-out) don't
apply the same way to scenario harnesses.

### 3. Decomposition for god methods missing actionable next step

**Observed:** SOS011 ("Function X is N lines") and SOS014 ("mixed
orchestrator/operator profile") on the same function emit two `_required`
items with overlapping `decompose_method`/`analyze_method` hints. It wasn't
obvious whether to act on SOS011 first or SOS014 first, or whether
`decompose_method` does both at once.

**Suggested fix:** Dedupe per-target diagnostics into a single
"recommended next action" with a concise rationale, or surface a single
`refactor_god_method` action that subsumes the analysis.

### 4. Response truncation hides per-rule counts

**Observed:** `get_diagnostics severity=failure` reported `total: 539` with
`showing: 15` and `_truncated.reason: "Response was 37811 bytes (limit: 32000)"`.
The `total: 539` includes ALL diagnostics across severities — not just
failures — which was momentarily confusing given the `severity:failure` filter
in the same call. The 28-failure number from `scan_repo` is the actual count;
539 is workspace-wide.

**Suggested fix:** When the response is filtered, `total` should reflect the
post-filter count. Or rename to `total_unfiltered` to be explicit.

### 5. `move` auto-generates over-eager re-exports

**Observed:** After moving `render_template` + `resolve_template_path` from
`runtime.rs` to a new `templating.rs`, the tool wired a grouped re-export
back into `runtime.rs`:

```
pub(crate) use crate::templating::{render_template, resolve_template_path};
```

But `resolve_template_path` is a private helper that is only called by
`render_template` *inside the new module*. No code outside `templating.rs`
references it, so the re-export triggers an `unused_imports` warning that
blocks `clippy -D warnings`.

**Suggested fix:** When `move` analyses external call-sites, it knows which
moved entities are referenced outside the new file. The re-export should
list only those — internal helpers stay private to the new module.

### 6. `propose_decomposition` produced cyclic groups

**Observed:** `propose_decomposition` on `runtime.rs` returned three groups
with `safety_proof.satisfied: false` and `status: cycle_detected`. The
`_cross_dependencies` block listed ~25 symbols shared between group_20 and
group_11. The proposal as-is is not executable — applying the suggested
moves would break the build.

**Impact:** The HATEOAS `_required` block still pointed at those moves as
the "fix_now" next action, so a naive caller would have followed them
into a broken state.

**Suggested fix:**
- When `safety_proof.satisfied: false`, surface a corrective `_required`
  action (e.g. "merge group_X with group_Y first") instead of the move
  that would create the cycle.
- For Rust specifically: the decomposition planner could exploit the fact
  that `impl Foo` blocks are legal across files. Methods on the same type
  don't have to live in the same physical file — they only need consistent
  visibility. Right now it seems to assume "method = stays with type."

### 7. `move` writes mod declaration to wrong crate root

**Observed:** When moving free functions from `runtime.rs` to a new
`runtime_records.rs` sibling, the tool's wiring suggested:

```
"Add mod declaration for new file ... Place near other mod declarations."
target file: "crates/mcp-flowgate-core/src/mod.rs"
```

There is no `mod.rs` at that path — the crate root is `lib.rs`. The
auto-apply correctly reported:

```
[auto-apply failed: Cannot read crates/mcp-flowgate-core/src/mod.rs:
 No such file or directory (os error 2)]
```

…but did not retry against `lib.rs`. The other moves in this session
(to `templating.rs`, `runtime_links.rs`) correctly targeted `lib.rs`,
so the regression seems to depend on something about the third move
in the same session — possibly because earlier moves already added
`mod runtime_records;`-style declarations to lib.rs and the tool then
assumed a `mod.rs` style?

**Suggested fix:** When the planned file path doesn't exist, fall
through to crate-root detection: pick `lib.rs` (for libraries) or
`main.rs` (for binaries) in the same crate's `src/` directory. Both
are standard Rust crate roots.

### 8. `move` doesn't propagate dependent-symbol visibility upgrades

**Observed:** Moving `link_filter_byguards`, `links`, `transition_definition`,
`is_terminal` to `runtime_links.rs` succeeded — but the moved code
referenced `pointer_escape` (still private in `runtime.rs`), causing
a compile error. The tool flagged the moves as `status: "success"`
even though the resulting file didn't compile.

**Impact:** A naïve caller chain (`move` → `verify_fix`) would discover
the failure only after the build break. A more sophisticated planner
would detect call-site references to symbols in the source file and
either:

1. Add them to the move batch automatically (co-locate tightly-coupled
   helpers), or
2. Upgrade their visibility to `pub(crate)` in the source file so the
   moved code can still reach them, or
3. Refuse the move and surface a "would-break-build" diagnostic.

Today the user has to read the cargo errors and patch up the gaps by
hand or via additional `move` invocations.

### 9. `move` cannot extract a single method from a Rust `impl` block

**Observed:** Asked structureos to move ONE method (`emit_transition_record`)
from the giant `impl WorkflowRuntime` block in `runtime.rs` to a new
`runtime_records.rs` file. Dry-run preview returned:

```
"moves": [{ "name": "impl WorkflowRuntime", "status": "skipped", ... }]
```

The filter `{name: ["emit_transition_record"]}` matched a method, but the
tool's planner treated it as a request to move the **entire** parent
`impl WorkflowRuntime` block — which would have ripped out every method
(start, submit, get, run_deterministic_chain, …) along with it. The import
diff confirmed this: it would remove `chrono::Utc`, `uuid::Uuid`, and
`crate::mapping::merge_output` from `runtime.rs`, all of which are used
by methods that should have stayed.

**Impact:** This is the dominant blocker to using structureos for the
remaining decomposition work on this codebase. The biggest LOC/complexity
contributors are all individual methods inside one `impl` block, and the
tool can't split them.

**Suggested fix:**

Rust permits **multiple `impl Foo` blocks for the same type across
multiple files in the same crate**. The decomposition planner should
exploit this:

1. When asked to move a single method, generate a new `impl Foo { fn X }`
   block in the target file containing just that method.
2. Leave the original `impl Foo` block in the source file with the remaining
   methods.
3. Each method's signature, visibility, and `self`-binding remain unchanged.

The current behavior (move the whole impl) is correct for "move the type",
but `name: ["emit_transition_record"]` is clearly asking for the method.

Workaround in this session: skip structureos for impl-method extraction
and use manual edits (or just leave the impl-method decomposition for
later) — which negates a lot of the tool's value on Rust codebases.

### 10. `decompose_method` produced uncompilable output

**Observed:** Invoked `decompose_method` on `submit` (455 LOC, cyclomatic 37)
with `dry_run: false`. The tool reported `strategy: one_per_arm` and
`scan_invalidated: true` — i.e. it actually rewrote the file. Result:

```
error[E0425]: cannot find function `submit_none` in this scope
   --> crates/mcp-flowgate-core/src/runtime.rs:371:17
help: consider using the associated function on `Self`
        None => Self::submit_none(request),
                ++++++

error[E0282]: type annotations needed
   --> crates/mcp-flowgate-core/src/runtime.rs:486:17
                executor_config.clone(),
                ^^^^^^^^^^^^^^^ cannot infer type

error[E0728]: `await` is only allowed inside `async` functions ...

(9 errors total)
```

**Diagnosis (best-guess from the output):**

1. The tool extracted a match arm into a call to `submit_none(request)` but
   **never wrote the `submit_none` function**. The reference is dangling.
2. Even if the function had been written, the `Self::` prefix is missing —
   it should be a method on `WorkflowRuntime`, not a free function called
   from inside an `impl` block.
3. The `await` errors suggest the extracted code path runs inside an
   `async fn` but the extracted helper wasn't itself marked `async`.

**Impact:** This is the action specifically meant to break up giant
functions like `submit`. It blew up the build instead. I restored from
the `.structureos/bak/` backup — credit to the tool for taking that
backup before mutating.

**Suggested fix:**

- The "extract function" half of `decompose_method` is non-negotiable;
  without it the action is purely destructive. Generate the extracted
  function body, place it as a method on the same `impl` block (or a
  sibling impl), and use `Self::name` at the call site.
- Mirror `async`/`unsafe`/return-type modifiers from the parent function
  onto the extracted helper.
- Add a build-the-output-after-applying invariant: the post-rewrite file
  should at minimum tree-sit cleanly before the action returns
  `status: applied`. If the output won't even compile, prefer `dry_run`
  with a warning to the user.

### 11. Truncation hid the actionable details of `decompose_method`

**Observed:** Both the dry-run and the real call returned
`_truncated.reason: "Response was 78518 bytes (limit: 32000)"`. The actual
list of extracted helpers and their bodies was clipped — what made it
back was just one example rewrite snippet, not a complete plan. As a
caller this means I can't audit the plan before committing.

**Suggested fix:** When the output exceeds the per-call limit, paginate
or write the full plan to a file (similar to the existing `.structureos/`
artefacts) and return the path. Today the truncation silently hides the
exact code change the tool is about to make.

## Things that worked very well

- HATEOAS `_required` / `_available` / `_action` hand-off was excellent —
  navigating from `scan_repo` → `get_diagnostics` → `propose_decomposition`
  felt natural with no doc-reading.
- The god_file_score is well-calibrated against intuition (runtime.rs really
  IS the worst file; the score correctly orders the others below).
- The `evidence` arrays on each diagnostic (`loc=2252`, `functions=38`) make
  it trivial to write a useful narrative without re-fetching metrics.
- `_analysis_health` block at top is gold — `parse_success_ratio: 1.0` +
  `graph_edge_resolution_ratio: 0.94` told me at a glance that the analysis
  could be trusted on structure (and where it couldn't be — macro-expansion).
