# Workflow guidance, blackboard, transition records & versioning — design spec

**Date:** 2026-05-22
**Status:** Draft v2 — revised after an FMECA architecture review; supersedes the
pre-review draft.
**Scope:** `mcp-flowgate-core` and `mcp-flowgate-executors`. No new MCP tools —
the seven-tool surface is unchanged.

## 1. Summary

Four additions to workflow definitions. All are opt-in; existing configs stay
valid. Each survived an architecture-validity review (§4).

1. **Guidance** — reusable "how to think" text, delivered in **two tiers**: a
   small *inline* payload (decision-critical, every response, `{{ }}`-templated)
   and a *referenced* tier (larger reusable fragments, surfaced as keys, fetched
   on demand via `gateway.describe`). Inlining everything would re-create the
   context bloat this product exists to remove.
2. **Blackboard slot declaration** — name the workflow `context` slots so guards
   and templates are statically checkable. Per-slot typing is optional.
3. **Transition records** — every applied transition emits one typed, schema'd
   event through the **existing `AuditSink`** (not a new subsystem), written to
   date-rotated files. This is the basis for run reconstruction.
4. **Versioned definitions** — each workflow carries a version; an instance pins
   to — and carries — its creation-time definition; nothing is deleted.

## 2. Motivation

- **Guidance bloat.** Per-state `guidance` is re-authored and re-sent every
  turn. Reusable, on-demand-fetched guidance keeps the per-turn payload bounded.
- **Stringly-typed guards.** `expr` guards reference `$.context.X` against an
  untyped bag; a typo fails at runtime, not at `check` time.
- **Definition drift.** SIGHUP hot-reload can swap a workflow definition under a
  running instance — the state it sits in may vanish.
- **Traceability.** A snapshot-only store cannot answer "what did the model do
  on loop iteration 3, and why." A transition record stream can.

## 3. Non-goals

- **No LLM-driving, no autonomous learning.** The crate does not call, train, or
  tune a model, and the model never edits governance. Guidance improves only
  through human-authored new versions (§5.6).
- **No graph-free blackboard control.** The state→transition graph stays the
  control spine; the blackboard only feeds guards.
- **No event sourcing.** The `WorkflowStore` snapshot is authoritative; the
  transition record stream is a durable side-effect, never a recovery source.
- **No parallel abstractions.** Transition records ride the existing `AuditSink`
  — no second logging subsystem.
- **No archive lifecycle management.** The crate writes append-only files and
  never deletes them; retention, tiering, backup and legally mandated erasure of
  those files are the operator's filesystem responsibility.
- **No new MCP tools.** Seven in, seven out.

## 4. Considered and cut (FMECA review)

Recorded so they are not re-proposed:

| Element | Verdict | Reason |
|---|---|---|
| Standalone `TransitionLog` port + file/stdout/memory sinks | **Cut** | Duplicates `AuditSink` and its impls — a parallel abstraction. Transition records are a typed audit event instead (§7). |
| Declared migrations (`__migrate__`, totality `check`, `MIGRATION_FAILED`) | **Cut** | Version pinning + natural drain already makes hot-reload safe. Forced in-flight upgrade is a rare need; revisit with evidence. |
| Required `summary` on every agent transition | **Cut** | A breaking protocol change betting on an unproven behaviour. The slot stays, optional (§6.3). |
| Per-slot JSON-Schema typing as the default | **Demoted** | Slot *name* declaration is enough for the high-value `check`. Typing is opt-in (§6.2). |
| Skill packs / Agent-Skills interop / decomposition | **Deferred** | Speculative; depends on an ecosystem that does not exist. |
| Autonomous guidance learning | **Rejected** | Breaks the immutability invariant (§8), the trust boundary (model-authored governance content), and crate scope. |

## 5. Guidance

> **Note — "guidance" *is* "skills".** The two words name the same thing:
> reusable instruction text. They are not two features. The referenced tier is
> limited by the **same HATEOAS-inspired discovery the gateway already uses for
> MCP tool menus** — advertise a small key menu, fetch a body on demand — so
> guidance never bloats the model's context any more than the seven-tool
> surface does. It is the founding principle applied consistently: don't dump
> the library, advertise it and let the client pull what it needs.

### 5.1 One concept, two tiers

From the LLM's perspective there is one thing: rendered instruction text. It
cannot distinguish inline text, a templated value, or a reusable fragment.
Guidance is therefore *one concept* delivered in two tiers, split by
**criticality**:

| Tier | What | Delivery | Rationale |
|---|---|---|---|
| **Inline** | `goal` + a short situational line; `{{ }}` live values | in every response | small, bounded, decision-critical *now* |
| **Referenced** | reusable fragments ("skills") — larger "how we work" text | a surfaced key; body fetched via `gateway.describe` | the repeat-offender bloat; fetched once, then in the model's own memory |

### 5.2 Inline tier — templated

`goal` and `guidance` on a state stay plain strings (unchanged shape) and become
**templates**: `{{ }}` placeholders interpolate against the live workflow before
the string is sent.

```yaml
states:
  ready_to_deploy:
    goal: Confirm deployment
    guidance: >
      Lint clean; {{ $.context.testCount }} tests green. Review before deploying.
```

Placeholders use the same `$.`-rooted paths as guards: `$.context.*`,
`$.workflow.input.*`, `$.workflow.*`. Interpolation is single-pass and
non-recursive (a value containing `{{ }}` is never re-expanded). An unresolved
placeholder renders as a marked stub — `(testCount: unset)` — never an error.
The `{{workflowId}}` substitution in `reliability.rs` is the existing primitive;
this generalises it.

### 5.3 Referenced tier — guidance fragments

A **guidance fragment** ("skill") is a named, reusable block of static markdown.
Fragments are declared once in a top-level `skills:` map:

```yaml
skills:
  house-voice:                 # the map key IS the fragment's subject
    verb: apply
    body: |
      # House voice
      Lead with the reader's problem. Short sentences. No hedging.
  deploy-safety:
    verb: check
    body: |
      # Deploy safety
      Confirm rollback path, error budget, and on-call coverage.
```

`body` is static — **referenced fragments are never templated**, so a body
fetched and cached on turn 3 can never be stale on turn 9. Live values belong in
the inline tier.

### 5.4 Surfaced refs — `verb` + `subject` (poka-yoke)

Every response surfaces the fragments in scope so the model knows what it *can*
fetch — the model cannot look up what it cannot see. Each surfaced ref is two
**space-free tokens**:

- **`subject`** — the fragment's `skills:` map key (`house-voice`); also the
  `gateway.describe` lookup handle.
- **`verb`** — the fragment's `verb` field (`apply`); encodes the fragment's
  *mode* — `apply` a style, `check` a verification, `avoid` a constraint.

**Poka-yoke — malformed descriptors are unrepresentable, not merely linted.**
`verb` and the `skills:` keys are constrained by schema pattern
`^[a-z][a-z0-9-]*$` — lowercase, kebab, **no whitespace**. A descriptor cannot
be a sentence, a paragraph, or contain a space; the data shape forbids it. This
is prevention at config-load (fail-fast), not detection after the fact.

The model reads the ref as `"{verb} {subject}"` — `apply house-voice` — and
fetches the body with `gateway.describe` only if relevant. Cost per turn: a few
tokens per ref; each body crosses the wire once for the workflow's life.

### 5.5 Scopes & response shape

Fragments are referenced at three scopes; the surfaced ref appears wherever the
scope is active:

```yaml
workflows:
  content_publish:
    skills: [house-voice]            # workflow scope — surfaced every response
    states:
      draft:
        goal: Write the draft
        skills: [editorial-checklist] # state scope — surfaced in this state
        transitions:
          submit_draft:
            target: review
            skills: [tone-for-review] # transition scope — surfaced on this link
```

The response `guidance` object carries the inline tier and the referenced-tier
menu together:

```jsonc
"guidance": {
  "goal": "Write the draft",
  "instructions": "…rendered inline guidance…",
  "refs": [ { "verb": "apply", "subject": "house-voice" },
            { "verb": "follow", "subject": "editorial-checklist" } ]
}
```

`check` lints: a `skills:` ref with no matching `skills:` entry → **error**; more
than ~4 refs surfaced at one scope → **warn** (the menu is itself payload).

### 5.6 Guidance evolution (emergent, not a feature)

Guidance improves through a human-and-version-driven loop, which is an *emergent
property* of the other sections, not a component:

- **observe** — transition records (§7) show which guidance preceded which
  outcomes;
- **refine** — a human edits guidance;
- **apply safely** — the edit is a new definition version (§8); in-flight
  instances ignore it, new ones adopt it; archive-never-delete allows comparing
  version N against N+1.

An "LLM proposes a guidance diff → human approves → new version" flow is
expressible *on* mcp-flowgate as an ordinary human-approval workflow (the
`content-publish` example with a guidance diff as the content); it ships as an
`examples/` config, not as crate code.

## 6. Blackboard slots

### 6.1 Slot declaration

The "blackboard" is the existing `WorkflowInstance.context` — `output:` mappings
write it, `expr` guards read `$.context.X`. The only addition is **declaring the
slot names**, so guards and templates can be statically checked:

```yaml
workflows:
  deploy_pipeline:
    blackboard: [lintPassed, testCount, coverage, artifactId]
```

`check` warns when an `output:` mapping writes a slot absent from `blackboard:`.

### 6.2 Optional typing

A slot may optionally carry a JSON-Schema fragment instead of a bare name:

```yaml
    blackboard:
      testCount: { type: integer }
      artifactId: { type: string }
```

When a slot is typed, `output:` writes to it are validated and a mismatch raises
`BLACKBOARD_TYPE_ERROR` before the transition advances. Untyped (name-only)
slots are the default and are sufficient for use-before-def (§9).

### 6.3 The optional `summary` slot

`summary` is a reserved, **optional** string slot. `workflow.submit` accepts an
optional top-level `summary`; when present it is stored to `context.summary` and
surfaced in every response and `workflow.get`, letting a model resume a workflow
cold. It is **never** a guard input (model-authored content is untrusted; this
is why guards may not read `$.context.summary` — `check` errors on that). It is
not required and has no enforcing config knob.

## 7. Transition records

### 7.1 A typed audit event — not a subsystem

`mcp-flowgate` already has `AuditSink` (`Null`/`Stdout`/`Memory`/`File`). A
transition record is **one well-typed audit event** (`event_type:
"workflow.transition"`) carrying a payload that conforms to a canonical schema.
No `TransitionLog` port, no parallel sink tree.

### 7.2 Record shape

```jsonc
{
  "workflowId":        "wf_8f3a",
  "definitionId":      "content_publish",
  "definitionVersion": "2026-05-22",
  "seq":               5,                 // == resulting WorkflowInstance.version
  "timestamp":         "2026-05-22T14:03:11Z",
  "fromState":         "drafted",
  "toState":           "review",
  "transition":        "submit_draft",
  "actor":             "agent",           // agent | deterministic | human | system
  "principal":         "user:matt",
  "guards":            [ { "kind": "expr", "result": true } ],
  "arguments":         { "draft": "…" },
  "blackboardDelta":   { "documentId": "doc_2291" },
  "executor":          { "kind": "rest", "ok": true, "durationMs": 142 },
  "childWorkflowId":   null,              // set when executor kind == workflow
  "correlationId":     "…"
}
```

This payload is a **canonical schema** — `transition-record.schema.json`,
`typify`-generated (§10). Each applied transition — including each deterministic
chain hop — increments `version` by one and emits exactly one record; `seq` is
that `version`.

### 7.3 Snapshot authoritative; at-least-once; fail-fast

The `WorkflowStore` snapshot stays authoritative (`save_if_version` optimistic
locking). The record is a **durable side-effect of commit**, ordered
record-first:

1. durably write the transition record;
2. commit the snapshot.

If step 1 fails the transition **fails fast** (`RECORD_WRITE_FAILED`) and the
action does not happen — there is no path to a committed-but-unrecorded
transition. If step 1 succeeds but step 2 fails, the retry re-writes the record;
readers de-dupe by `(workflowId, seq)`. Recovery loads the snapshot, as today —
the record stream is never replayed for live state.

### 7.4 Date-rotated files

The `File` audit sink gains date rotation: `YYYY-MM-DD-{name}.log`, interval
configurable (`daily` default; `hourly`/`weekly`). Transition events route to
`…-transitions.log`, other audit events to `…-audit.log` — one rotating-file
writer, two `{name}`s. Files are append-only and never deleted by the crate.

### 7.5 Reconstruction

For any transition at any past time the system reconstructs **what the model
did, when, and why**, from retained files alone:

| Question | Source |
|---|---|
| what / when | the transition record |
| why it was legal | the retained definition version (§8) + recorded guard results |
| what the model reasoned over | blackboard at that `seq`, replayed from `blackboardDelta` |
| what the model was told | guidance for that state, re-derived from the retained definition version |

Because the gateway *is* the governance layer, "why" is causal: it knows the
legal moves it offered, which guards passed, and what guidance it served.

## 8. Versioned definitions

### 8.1 Version discriminator

Each workflow definition carries `version:` — an opaque unique string; an ISO
"as-of" date is the recommended convention (`version: 2026-05-22`). A workflow
without `version:` gets a default and behaves as today.

### 8.2 Instances carry their definition snapshot

A workflow definition version is a **complete, immutable, self-contained
snapshot** — states, transitions, guards, blackboard slots, guidance and the
*resolved fragment bodies* it references. At `workflow.start` the resolved
snapshot is stored **with the instance** in the `WorkflowStore`.

Consequence (FMECA mitigation): a running instance never depends on an external
definition file. Editing config, or deleting archived files, cannot strand an
in-flight workflow — it carries everything it needs. Editing a fragment or a
guard has no effect on running instances; it reaches only instances started
under a new version.

### 8.3 Hot-reload is additive; archive-never-delete

On SIGHUP, the incoming config's definitions are *added*; new `workflow.start`
uses the newest version; in-flight instances are untouched and drain on their
pinned version. Superseded definition versions are retained on disk, never
deleted by the crate (their lifecycle is the operator's — §3). There are no
declared migrations (§4): pinning plus natural drain is the whole mechanism.

## 9. Control & guards

The control spine is unchanged: the declared state→transition graph with
`guards:` lists and `linkFilter`. The one addition is static checkability.

`check` gains **use-before-def**: an `expr` guard or `{{ }}` template that reads
`$.context.X` must have a reachable predecessor transition whose `output:`
writes `X`. A guard referencing an undeclared slot, or `$.context.summary`, is a
`check` error. The runtime remains the backstop — a guard hitting an unset slot
fails fast with rich context, never a silent `false`.

## 10. Schema surfaces

Boundary contracts get canonical JSON Schemas in `/schemas`, `typify`-generated;
internal types stay hand-written Rust.

| Schema | Boundary | Status |
|---|---|---|
| `gateway-config.schema.json` | author → gateway | exists; extended |
| `workflow-response.schema.json` | gateway → MCP client | exists; extended (`guidance.refs`) |
| `transition-record.schema.json` | gateway → disk / trace tooling | **new** |

The request schemas (tool argument shapes) remain Rust-first in
`mcp-flowgate-mcp-server`; that pre-existing asymmetry is real tech debt but is
**out of scope for this spec** — a separate ticket.

## 11. `check` additions

| Check | Level |
|---|---|
| `skills:` ref resolves to a declared fragment | error |
| `verb` / `skills:` key matches `^[a-z][a-z0-9-]*$` | error (load-time) |
| guard / template `$.context.X` resolves to a declared slot | error |
| guard reads `$.context.summary` | error |
| use-before-def: guard/template slot has a reachable writer | error |
| `output:` writes an undeclared slot | warn |
| more than ~4 refs surfaced at one scope | warn |

## 12. Wire format

```jsonc
→ workflow.get { "workflowId": "wf_8f3a" }
← { "workflow": { "id": "wf_8f3a", "version": 4, "state": "draft" },
    "guidance": {
      "goal": "Write the draft",
      "instructions": "Draft from the approved outline.",
      "refs": [ { "verb": "apply", "subject": "house-voice" } ] },
    "links": [ { "rel": "submit_draft", "method": "workflow.submit", … } ] }

→ gateway.describe { "id": "house-voice" }      // model chooses to fetch
← { "kind": "guidance", "subject": "house-voice", "verb": "apply",
    "body": "# House voice\n…" }
```

## 13. Config additions & error codes

| Key | Location | Notes |
|---|---|---|
| `skills:` | top level | fragment library — `{ <subject>: { verb, body } }` |
| `skills:` | workflow / state / transition | list of subject references |
| `blackboard:` | workflow | slot names, or `{ name: <schema> }` for typed slots |
| `version:` | workflow | version discriminator; ISO date recommended |
| `summary` | `workflow.submit` arg | optional model-written string |
| `rotation:` | `audit:` | `daily` (default) / `hourly` / `weekly` |

Error codes: `RECORD_WRITE_FAILED` (transition record not durably written —
transition aborts), `BLACKBOARD_TYPE_ERROR` (typed-slot write violates schema).
Existing codes unchanged. `SUMMARY_REQUIRED` and `MIGRATION_FAILED` are **not**
introduced (§4).

## 14. Compatibility

- `skills:`, `blackboard:`, `version:` are optional; configs without them behave
  identically.
- `goal` / `guidance` strings are now templates — strings with no `{{ }}` are
  unaffected.
- **Behaviour change:** `version` increments once per applied transition
  (including chain hops). Drivers must read `version` from the response — the
  prefilled links already do.

## 15. Open questions

- **Per-run outcome tag** — a success/failure signal per run would make the §5.6
  loop quantitative. Derivable from terminal states in the records today; a
  first-class tag is deferred until there is demand.
- **State-local blackboard slots** — deferred (lifecycle complexity).
- **Structured `summary`** — `summary` is a plain string; a schema'd summary is
  a later option.
- **Request-schema unification** (§10) — separate tech-debt ticket.

## 16. Implementation order

1. Blackboard slot declaration + `output:` name-check + `check` slot checks.
2. Transition records: the `transition-record.schema.json` schema, the typed
   `workflow.transition` audit event, record-first commit ordering with
   `RECORD_WRITE_FAILED` fail-fast.
3. Date-rotated `File` audit sink (`YYYY-MM-DD-{name}.log`), shared by both
   event streams.
4. Versioned definitions: `version:` discriminator, the per-instance definition
   snapshot, additive hot-reload.
5. Guidance: templated inline tier; the `skills:` fragment library; surfaced
   `verb`/`subject` refs; `gateway.describe` fetch; `check` lints.
6. use-before-def analysis in `check`.

Each step is independently shippable and rollback-able. A phased, test-first
implementation plan should be produced from this spec before code is written.
