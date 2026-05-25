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
  review.style.house-voice:    # the map key IS the fragment's subject
    verb: review
    lifecycle: stable
    body: |
      # House voice
      Lead with the reader's problem. Short sentences. No hedging.
  deploy.safety.checklist:
    verb: review
    lifecycle: stable
    body: |
      # Deploy safety
      Confirm rollback path, error budget, and on-call coverage.
```

`body` is static — **referenced fragments are never templated**, so a body
fetched and cached on turn 3 can never be stale on turn 9. Live values belong in
the inline tier.

**Required fields** on every fragment: `verb`, `lifecycle`, `body`. All three
are required (no defaults — a missing field fails config-load with
`MISSING_VERB`, `MISSING_LIFECYCLE`, or `MISSING_BODY` respectively). An
optional `source` field records provenance for fragments pulled from external
libraries (see §19); fragments declared inline carry `source: "config"`
implicitly.

### 5.4 Surfaced refs — `verb` + `subject` (poka-yoke)

Every response surfaces the fragments in scope so the model knows what it *can*
fetch — the model cannot look up what it cannot see. Each surfaced ref is a
small object with bounded fields; no body content appears in the listing.

**Ref shape:**

```jsonc
{ "verb": "review",
  "subject": "review.style.house-voice",
  "title":   "House voice (optional human-readable label)",
  "hash":    "sha256:9c1d…" }
```

- **`subject`** — the fragment's `skills:` map key; also the `gateway.describe`
  lookup handle. Required.
- **`verb`** — one of eight closed cognitive operations (see below). Required.
- **`hash`** — `sha256:` prefix + hex digest of the **normalized** body (see
  §5.7). Required. Enables cache invalidation: when the body is edited, the
  hash flips; previously-cached refs are stale.
- **`title`** — optional, human-readable. Never carries body content.

No `excerpt`, `preview`, or `body` field exists on a ref — by design. The
listing carries discovery metadata only; bodies cross the wire exactly once,
on demand, via `gateway.describe`.

#### 5.4.1 Closed `verb` vocabulary (poka-yoke)

`verb` is a **closed enum** of eight cognitive operations. Unknown verbs fail
config-load with `INVALID_VERB { verb, allowed: [...] }`. There is no escape
hatch — no `Other(String)` variant, no opt-in extension. Adding a verb requires
a deliberate spec amendment, not authoring convention.

| Verb | Cognitive posture |
|---|---|
| `triage` | classify, prioritize, route |
| `diagnose` | find root cause |
| `plan` | design approach before acting |
| `implement` | produce / generate the artifact |
| `review` | evaluate against criteria |
| `refactor` | restructure preserving behavior |
| `explain` | build understanding (self-explain or teach others) |
| `compose` | assemble parts into a whole |

The verbs are **cognitive postures**, not methodologies. Methodologies (TDD,
spec-driven, design-by-contract) are workflow shapes that sequence the eight
verbs — see §17. Posture *modifiers* (speedrun, improvise, code-golf) belong
in the body of a fragment or the framing of a workflow state, not in the verb
metadata.

The model reads a ref as `"{verb} {subject}"` — `review review.style.house-voice`
— and fetches the body with `gateway.describe` only if relevant.

#### 5.4.2 Blessed `subject` namespace roots (poka-yoke)

`subject` is a dotted namespace. The first segment is a **blessed root**;
segments below the root are free-form.

Blessed roots:

| Root | Scope |
|---|---|
| `review.*` | evaluation guidance (code, plan, security, data, style…) |
| `authoring.*` | composing workflows or skills |
| `debug.*` | diagnosis / triage / reproduction |
| `deploy.*` | release-time guidance |
| `import.*` | external-source ingest |
| `lifecycle.*` | drafting / completing / archiving |
| `plan.*` | design — with two conventional second-level paths: `plan.specify.*` for durable artifacts (ADRs, RFCs, contracts, interfaces, acceptance tests), `plan.execute.*` for short-term sequencing (PR scope, sprint breakdown) |

A subject whose first segment is outside the blessed set produces a diagnostic.
Behavior depends on `flowgate.strict_namespacing` (default `true`):

- `strict_namespacing: true` — unblessed root fails config-load with
  `INVALID_SUBJECT_ROOT { subject, blessed_roots: [...] }`. **This is the
  default.**
- `strict_namespacing: false` — unblessed root surfaces a warning diagnostic
  in `startup_diagnostics()` and via the `gateway.diagnostics` tool, but load
  succeeds. The diagnostic message includes the Levenshtein-closest blessed
  root as a suggested alternative.

**Poka-yoke — malformed descriptors are unrepresentable, not merely linted.**
`subject` is constrained by schema pattern
`^[a-z][a-z0-9-]+(\.[a-z][a-z0-9-]+)+$` — lowercase, kebab, dotted, at least
two segments, **no whitespace**. The empty subject is rejected with
`EMPTY_SUBJECT`.

### 5.5 Scopes & response shape

Fragments are referenced at three scopes; the surfaced ref appears wherever the
scope is active:

```yaml
workflows:
  content_publish:
    skills: [review.style.house-voice]    # workflow scope — every response
    states:
      drafting:
        goal: Write the draft
        skills: [review.editorial.checklist] # state scope — in this state
        transitions:
          submit_draft:
            target: reviewing
            skills: [review.style.tone-for-review] # transition scope — on this link
```

The response `guidance` object carries the inline tier and the referenced-tier
menu together:

```jsonc
"guidance": {
  "goal": "Write the draft",
  "instructions": "…rendered inline guidance…",
  "refs": [
    { "verb": "review", "subject": "review.style.house-voice",
      "hash": "sha256:9c1d…" },
    { "verb": "review", "subject": "review.editorial.checklist",
      "hash": "sha256:a8f2…" }
  ]
}
```

`check` lints: a `skills:` ref with no matching `skills:` entry → **error**;
more than ~4 refs surfaced at one scope → **warn** (the menu is itself
payload); a `subject` outside blessed roots → **error** under default
`strict_namespacing` (warning otherwise).

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

### 5.7 Content-addressed bodies + cache invalidation

Every fragment's body is **normalized**, then SHA256-hashed; the hash is
attached to every emitted ref. The model sees a fresh hash whenever the body
changes — its cached body is invalidated by virtue of the ref being different.

**Normalization rule** (single canonical implementation; see TRIZ note in §5.8
for why it is centralized):

1. Trim leading and trailing whitespace from the body.
2. Replace each run of internal whitespace (spaces, tabs, line breaks) of
   length ≥1 with a single space.
3. Strip a trailing newline if present after step 2.

The hash is `sha256:` followed by the lowercase-hex digest of the normalized
body's UTF-8 bytes. A whitespace-only edit produces an identical hash; a
semantic edit produces a different hash. Whitespace within fenced code blocks
follows the same rule — guidance bodies are not source code; they are prose
the model reads, and whitespace stability matters more than verbatim
preservation of formatting.

The hash is **required** on every fragment ref (no `Option<String>`). At
config-load, the gateway recomputes hashes from bodies and compares against
any stored hash; a mismatch fails fast with `HASH_MISMATCH { subject, stored,
computed }`.

**Cross-implementation invariant:** every component that hashes a body MUST
import the same `normalize_for_hash()` function. Two independent
implementations of "normalize whitespace" produce the same hash with
probability 1 only by exhaustive accident; the spec mandates a single
source-of-truth function and a test that asserts read-side and write-side
agree on a fixture corpus.

### 5.8 Audit of body retrieval

`gateway.describe { subject }` is a body-retrieval call. Every call emits a
typed audit event so a workflow's authoring trail captures *which* guidance
the model fetched, *when*, and under *which* correlation:

```jsonc
{ "eventType":    "guidance.describe_requested",
  "subject":      "review.style.house-voice",
  "verb":         "review",
  "workflowId":   "wf_8f3a",        // null when called outside a workflow context
  "correlationId": "cor_a91…",       // null when called outside a workflow context
  "principal":    "agent:claude",
  "outcome":      "ok",              // "ok" | "failed"
  "errorCode":    null,              // "GUIDANCE_DESCRIBE_FAILED" on failure
  "timestamp":    "2026-05-24T14:03:11Z" }
```

`gateway.describe` is a non-critical-path audit (per §7.3 terminology): a
sink failure during the describe-audit emission **does not** abort the
describe call, but it MUST emit an `audit.write_failed` self-event so the
failure is observable. This differs from `workflow.transition` records, which
abort the transition on sink failure (§7.3).

### 5.9 Acknowledgment as a guard kind — semantic limit (TRIZ-bounded)

For workflows where reading the guidance before acting genuinely matters
(e.g. a review-style workflow that *requires* the reviewer to have consulted
the rubric), the runtime exposes a `guidance_acknowledged` guard kind (full
guard mechanics in §17). This guard fails until `gateway.describe { subject }`
has been called for the named subject within the **same workflow correlation**.

**Semantic limit (irreducible, documented as a constraint):** the gateway can
verify the model *fetched* the body. It cannot verify the model *read* or
*comprehended* it. The guard is a fetch-and-freshness proof, not a
comprehension proof.

**TRIZ resolution (Asymmetry — treat ack as time-bounded scope, not
permanent):** the ack is tied to `(correlation_id, subject, body-hash-at-ack-time)`.
If the body's hash changes after the ack but before the gated transition, the
ack is invalidated and the model must re-fetch. This converts the gate from
"trust that one describe call satisfies forever" into "trust that the
description seen was the current one." The semantic limit remains; the
TRIZ-resolved gate is meaningful within its scope.

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

### 8.4 Bypass path: authoring-workflow registry writes

The reference authoring workflow (§17) needs to *publish* new definitions
back to the gateway. Two safeguards make this safe:

1. **Feature flag, default off.** `flowgate.authoring.write_enabled` is the
   single switch. Default `false`. The flag is read at gateway startup and
   is **not runtime-mutable**: a workflow YAML that contains this key
   anywhere within `workflows:` is rejected at config-load with
   `CONFIG_FLAG_NOT_RUNTIME_MUTABLE`. An LLM-authored workflow cannot
   silently enable its own write path.

2. **Audit-before-commit ordering** (mirrors §7.3 record-first):
   - The `registry` executor (§17.2) emits `definition.published` to the
     audit sink BEFORE the new snapshot becomes loadable.
   - If audit emission fails, the commit is aborted; the new definition is
     NOT made loadable; the executor returns `RECORD_WRITE_FAILED`.
   - Successful commit fires `definition.loadable` post-commit (best-effort
     audit, mirrors §5.8 non-critical-path semantics).

Trait shape:

```rust
// crates/mcp-flowgate-core/src/ports.rs
#[async_trait]
pub trait DefinitionStoreWritable: DefinitionStore {
    async fn register(&self, id: &str, definition: Value) -> Result<(), DefinitionStoreError>;
}
```

The writable variant is constructed only when the flag is on; runtime call
sites hold `Option<Arc<dyn DefinitionStoreWritable>>` and pass `None` when
disabled. The registry executor checks for `None` and fails fast with
`WRITE_DISABLED`.

**Bypass-path-of-the-bypass-path:** in a deployment where the authoring
workflow itself becomes unrunnable (e.g. malformed by a published edit), the
operator may set `flowgate.authoring.write_enabled: true` AND author a
fix via the standard config-reload path (§8.3). The audit event
`definition.bypass_published` fires for any registry write made by a
principal carrying the `authoring` role, so direct-write usage is always
visible.

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
| `verb` is one of the eight closed cognitive verbs (§5.4.1) | error (load-time) |
| `subject` matches `^[a-z][a-z0-9-]+(\.[a-z][a-z0-9-]+)+$` | error (load-time) |
| `subject` first segment is a blessed root (§5.4.2) | error if `strict_namespacing: true` (default); warn otherwise |
| `lifecycle` is one of `experimental`/`stable`/`deprecated` | error (load-time) |
| fragment `hash` matches `normalize_for_hash(body)` recomputed at load | error (load-time) |
| guard / template `$.context.X` resolves to a declared slot | error |
| guard reads `$.context.summary` | error |
| use-before-def: guard/template slot has a reachable writer | error |
| `output:` writes an undeclared slot | warn |
| more than ~4 refs surfaced at one scope | warn |

## 12. Wire format

```jsonc
→ workflow.get { "workflowId": "wf_8f3a" }
← { "workflow": { "id": "wf_8f3a", "version": 4, "state": "drafting" },
    "guidance": {
      "goal": "Write the draft",
      "instructions": "Draft from the approved outline.",
      "refs": [ { "verb": "review", "subject": "review.style.house-voice",
                  "hash": "sha256:9c1d…" } ] },
    "links": [ { "rel": "submit_draft", "method": "workflow.submit", … } ] }

→ gateway.describe { "id": "review.style.house-voice" }    // model chooses to fetch
← { "kind":     "guidance",
    "subject":  "review.style.house-voice",
    "verb":     "review",
    "lifecycle": "stable",
    "hash":     "sha256:9c1d…",
    "body":     "# House voice\n…" }
```

The `gateway.describe` call emits a `guidance.describe_requested` audit event
(see §5.8). The body is fetched once per workflow's life; subsequent
references to the same subject from the same correlation reuse the cached body
unless the ref's `hash` differs (cache invalidation, §5.7).

## 13. Config additions & error codes

| Key | Location | Notes |
|---|---|---|
| `skills:` | top level | fragment library — `{ <subject>: { verb, lifecycle, body, source? } }` |
| `skills:` | workflow / state / transition | list of subject references |
| `blackboard:` | workflow | slot names, or `{ name: <schema> }` for typed slots |
| `version:` | workflow | version discriminator; ISO date recommended |
| `summary` | `workflow.submit` arg | optional model-written string |
| `rotation:` | `audit:` | `daily` (default) / `hourly` / `weekly` |
| `strict_namespacing:` | `flowgate:` (top level) | `true` (default) / `false` — controls whether unblessed `subject` roots error or warn (§5.4.2) |

**Error codes.**

| Code | When |
|---|---|
| `RECORD_WRITE_FAILED` | transition record not durably written — transition aborts |
| `BLACKBOARD_TYPE_ERROR` | typed-slot write violates schema |
| `INVALID_VERB` | `verb` field not in the closed eight (§5.4.1); payload includes `allowed` list |
| `MISSING_VERB` | `verb` field absent from a fragment declaration |
| `INVALID_SUBJECT_ROOT` | first segment of `subject` not blessed; raised under `strict_namespacing: true` |
| `EMPTY_SUBJECT` | `subject` string is empty after trim |
| `MISSING_LIFECYCLE` | `lifecycle` field absent from a fragment declaration (no silent default) |
| `INVALID_LIFECYCLE` | `lifecycle` value not in `experimental`/`stable`/`deprecated` |
| `MISSING_BODY` | `body` field absent from a fragment declaration |
| `MISSING_SKILL_HASH` | a fragment ref reaches the runtime without a `hash` field |
| `HASH_MISMATCH` | stored `hash` does not match `normalize_for_hash(body)` at load |
| `GUIDANCE_DESCRIBE_FAILED` | `gateway.describe` could not resolve a body (snapshot lookup failure) |
| `GUIDANCE_NOT_ACKNOWLEDGED` | `guidance_acknowledged` guard fired; payload names the unacknowledged subject and the current vs acknowledged hash |
| `GUIDANCE_SUBJECT_UNKNOWN` | `guidance_acknowledged` guard names a subject absent from the instance's snapshot |
| `CONFIG_FLAG_NOT_RUNTIME_MUTABLE` | a flag scoped to `flowgate:` top level (e.g. `strict_namespacing`, `authoring.write_enabled`) appears within `workflows:` |

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

## 17. Authoring as a workflow

The LLM is a first-class workflow author. Authoring is **just another
Flowgate workflow** — same primitives, same guards, same audit log. No
special runtime path, no privileged escape hatch outside the bypass-flag
mechanism in §8.

### 17.1 Reference workflow shape

```
drafting → reviewing_structure → reviewed → validating → ready → published
                  ↑                              ↑          ↓
                  └──────── (issues found) ──────┘     (gates fail → drafting)
```

| State | Inbound action | Gating |
|---|---|---|
| `drafting` | LLM proposes a workflow YAML or skill fragment | input schema (well-formed YAML) |
| `reviewing_structure` | `structural_analysis` executor (see §18) runs against the draft | guard fails if any required structural issue surfaces |
| `validating` | `dry_run` executor (see §17.3) instantiates an isolated runtime and runs scripted inputs against the draft | guard fails on executor errors or unexpected traces |
| `ready` | author has acknowledged the change; awaits publish | `guidance_acknowledged` (§5.9), optional human-actor |
| `published` | `registry` executor (see §17.4) writes the new definition through the writable store (§8) | requires `flowgate.authoring.write_enabled: true` (§8) |

### 17.2 Required executor kinds

Four new executor kinds make authoring expressible as a workflow:

| Kind | Purpose | Mutates state? |
|---|---|---|
| `structural_analysis` | static checks on a candidate definition; returns `{ issues: [{ rule, severity, location, message }] }` | no |
| `dry_run` | instantiates an in-memory runtime and runs a scripted input set against the candidate; returns the audit trace | no (see §17.3) |
| `ingest` | reads an external guidance source (mattpocock-style markdown, etc.) and emits a Flowgate-shaped fragment; see §19 | no |
| `registry` | writes a new (or updated) definition through `DefinitionStoreWritable` (§8); fails fast with `WRITE_DISABLED` if the bypass flag is off | yes (gated) |

### 17.3 Isolation invariant for `dry_run`

The `dry_run` executor MUST construct an isolated `WorkflowRuntime` per
invocation, backed by `InMemoryWorkflowStore` and `MemoryAuditSink`. It MUST
NOT accept caller-supplied store or audit references. The signature is
intentionally narrow:

```rust
async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError>
// req.arguments.definition: Value     — the candidate workflow YAML
// req.arguments.script:     [Value]   — scripted inputs to drive
```

The isolation guarantee is enforced by type — there is no parameter through
which the caller can pass production state. The author cannot accidentally
"reuse the live runtime to save time" because the constructor signature
forbids it. See FMECA FM-6 in the implementation plan.

### 17.4 Required guards on the authoring workflow

At minimum the reference authoring workflow uses these guards:

- `structural_analysis_passes`: `expr` guard reading `$.context.structural_issues == []`.
- `dry_run_passes`: `expr` guard reading `$.context.dry_run_failed != true`.
- `guidance_acknowledged`: as defined in §5.9; required before `publish`.
- (Optional) actor-gated transitions: `publish` may be `actor: human` for orgs
  that require human-in-the-loop sign-off.

### 17.5 Meta-circularity & bootstrap

The authoring workflow is itself a Flowgate definition. Two consequences:

1. **The first authoring workflow ships with the gateway.** A reference
   `authoring-workflow.yaml` is provided in `examples/`; users can fork it.
2. **Bypass path for recovery.** If the deployed authoring workflow becomes
   uneditable (because it requires itself to publish a fix), §8 defines a
   privileged write path gated by `flowgate.authoring.write_enabled` AND a
   principal with `authoring` role. Audit-flagged loudly (`definition.bypass_published`)
   so misuse is impossible to hide.

## 18. Structural analysis

`structural_analysis` is an executor that validates a candidate definition
(workflow or skill fragment) against a closed set of structural rules.
Output shape:

```jsonc
{ "issues": [
    { "rule":     "CYCLE_DETECTED",
      "severity": "error",          // "error" | "warning"
      "location": "/workflows/demo/states/foo/transitions/bar/target",
      "message":  "transition path forms a cycle: foo → bar → foo" } ] }
```

An empty `issues` array means the candidate passed.

### 18.1 Required rule set

Every implementation MUST execute these rules. A rule that fails to execute
returns an error (not an empty issue list), so coverage gaps are visible
rather than silent — see FMECA FM-5.

| Rule | Severity | Detects |
|---|---|---|
| `CYCLE_DETECTED` | error | non-loop-intent cycle in transition graph |
| `DEAD_STATE` | error | state with no inbound transition (and not initial) |
| `UNDEFINED_TARGET` | error | transition `target:` names a state not in `states:` |
| `UNDECLARED_SLOT_READ` | error | guard or template reads `$.context.X` where `X` is not in `blackboard:` |
| `UNBLESSED_SUBJECT_ROOT` | warning | skill fragment subject's first segment not in `BLESSED_SUBJECT_ROOTS` (§5.4.2) |
| `NO_TRANSITIONS` | error | workflow has zero transitions |
| `OVERSIZED_STATE` | warning | state with > N outbound transitions (N defaults to 8) |

### 18.2 Extensibility hook (T3)

The core rule set is fixed; additional rules may be registered via config
under `flowgate.structural_rules:`. Custom rules carry the same shape:
`{ rule, severity, location, message }`. Registration shape and lifetime
defined when extensibility ships in T3.

### 18.3 Self-check invariant

Implementations MUST ship a "rules-self-check" test: a fixture workflow
that triggers every required rule. If the analysis output omits any
required rule for that fixture, the test fails. This prevents the
oversimplification failure where an executor ships with two rules and
declares itself done.

## 19. Ingest transforms

`ingest` is an executor that adapts external guidance sources to Flowgate
fragment shape. The first-party adapter handles mattpocock-style
`.claude/skills/*.md` (frontmatter `name`, `description`; body is the
markdown body). Future adapters (Cursor rules, internal wikis) follow the
same pattern.

### 19.1 Input

```jsonc
{ "source_path": "path/to/external/skill.md",
  "subject":     "review.style.house-voice",   // optional; if absent, inferred from source path
  "verb_synonyms": { "fix": "implement", ... } // optional caller override }
```

### 19.2 Output

```jsonc
{ "fragment": {
    "subject":   "review.style.house-voice",
    "verb":      "review",            // either explicit in source, or mapped from synonym
    "lifecycle": "experimental",      // ingested fragments default to experimental
    "body":      "…markdown body…",
    "hash":      "sha256:…",
    "source":    "path/to/external/skill.md"
  },
  "diagnostics": [
    { "level": "info", "code": "VERB_MAPPED", "from": "fix", "to": "implement" }
  ] }
```

### 19.3 Verb synonym mapping

A small built-in synonym table maps common external verbs to the closed
eight (§5.4.1). Mappings emit a `VERB_MAPPED` info diagnostic so the
author can audit the rename:

| External verb | Mapped to |
|---|---|
| `fix` | `implement` |
| `verify`, `validate`, `test`, `audit` | `review` |
| `cleanup`, `tidy`, `improve` | `refactor` |
| `document`, `teach`, `walkthrough` | `explain` |
| `assemble`, `bundle`, `integrate` | `compose` |
| `investigate`, `inspect`, `analyze` | `diagnose` |
| `prioritize`, `classify`, `route` | `triage` |
| `design`, `spec`, `plan` | `plan` |

A source-side verb that's already in the closed eight passes through with
no `VERB_MAPPED` diagnostic. A source-side verb absent from both the closed
set and the synonym table fails with `INGEST_INVALID_VERB`.

### 19.4 Error codes

| Code | When |
|---|---|
| `INGEST_CANNOT_INFER_SUBJECT` | no `subject` argument and source path doesn't yield one |
| `INGEST_INVALID_VERB` | source verb is neither in closed eight nor in synonym table |
| `INGEST_SUBJECT_COLLISION` | proposed subject already exists in the live skill library |
| `INGEST_EMPTY_BODY` | source has no body content after frontmatter strip |

Ingest does NOT publish — it returns the fragment to the calling workflow,
which routes it through the rest of the authoring workflow (structural
analysis, dry-run, registry). This keeps the gates uniform regardless of
authoring source.

## 20. Audit & Evidence Enrichment for Downstream Analysis

Three additive fields enable hierarchical observability and richer
evidence-weighted decisions without breaking existing producers. Every
field is `Option<_>` with serde `skip_serializing_if = "Option::is_none"`,
so historical payloads round-trip unchanged.

### 20.1 Evidence enrichment

The existing `Evidence` struct (`crates/mcp-flowgate-core/src/model.rs`)
gains two optional fields:

| Field | Type | Meaning |
|---|---|---|
| `digest` | `Option<String>` | Content-identity of the evidence artifact. Convention: `sha256:` prefix + lowercase-hex digest of the artifact's bytes. Useful for verifier-produced artifacts (JUnit XML, SARIF, coverage JSON) where the consumer wants to deduplicate or attest. Producers SHOULD populate when the artifact is byte-stable. |
| `confidence` | `Option<f32>` | The producing model's stated confidence (0.0..=1.0) that this evidence supports the claim it's attached to. Out-of-range values fail validation with `INVALID_CONFIDENCE`. Producers SHOULD populate when the evidence is model-authored; deterministic executors typically omit. |

The `evidence` guard kind (§9) is extended with two optional clauses
that compose with the existing `requires: [{kind, count}]` shape:

```yaml
guards:
  - kind: evidence
    requires:
      - { kind: approval, count: 2, min_confidence: 0.7 }   # NEW: min_confidence
      - { kind: build-log, count: 1, require_digest: true } # NEW: require_digest
```

`min_confidence` rejects any evidence record whose `confidence` is below
the threshold (records with no `confidence` are also rejected when this
clause is set — explicit opt-in to model-authored evidence). `require_digest`
rejects evidence records missing a `digest`.

### 20.2 AuditEvent enrichment

The existing `AuditEvent` struct (`crates/mcp-flowgate-core/src/audit.rs`)
gains two optional hierarchical-identity fields:

| Field | Type | Meaning |
|---|---|---|
| `trace_id` | `Option<String>` | Caller-supplied trace id spanning multiple workflows in one logical operation (e.g. a CI build that launches N sub-workflows). The gateway is opaque to the value; it writes through unchanged. |
| `run_id` | `Option<String>` | Caller-supplied id for grouping related workflow instances (e.g. one model-evaluation run that exercises 100 workflows). Same opacity semantics as `trace_id`. |

Both are surfaced via builder methods on `AuditEvent` (`with_trace_id`,
`with_run_id`) mirroring the existing `with_workflow`/`with_correlation`
pattern. Sinks that serialize to JSON include the fields when present and
omit them otherwise.

**MCP server plumbing.** The MCP-server-level tools (`workflow.start`,
`workflow.submit`, `gateway.describe`, etc.) accept optional `traceId` /
`runId` arguments. When present, the server propagates them to every
`AuditEvent` produced by the resulting workflow operation. When absent,
the fields stay `None`. The plumbing is mechanical and does not change
existing semantics for callers that omit the fields.

### 20.3 Metric extraction contract

The audit log carries everything the standard SWE-agent scorecard
(`RESEARCH.md`) needs. No new metrics service ships with Flowgate.
Instead this section specifies the contract:

**Producers guarantee** that every transition record carries:
- `event_type` = `"workflow.transition"` (per §7.2),
- `workflow_id`, `correlation_id`, `actor`, `transition_name`,
- `executor_outcome.duration_ms` and `executor_outcome.ok` when an
  executor ran (per the §7.2 ordering),
- `timestamp` (ISO-8601, UTC).

**Consumers** derive metrics like the following from the log alone:

| Metric | Derivation |
|---|---|
| `resolved_rate` | count(workflows reaching a `terminal: true` state with no `error`) ÷ count(workflows started) |
| `time_to_reviewer_ready_patch` | `timestamp(first audit event in workflow with `state == "ready"`)` − `timestamp(workflow.started)` |
| `retry_count` | count(`transition.requested` with name `retry` per `workflow_id`) |
| `cost_per_accepted_fix` | Σ(`executor_outcome.duration_ms` × tier-cost) ÷ count(workflows completed). Tier-cost is a caller-side lookup; Flowgate does not assign monetary value to executor kinds. |
| `mutation_score` | Extract from `evidence[kind="mutation"]` records on verifying-state transitions. |
| `human_escalation_rate` | count(`transition.requested` whose target is a state with `actor: human`) ÷ count(all transitions). |
| `pass_to_pass_failure_rate` | Read `evidence[kind="pass-to-pass-failed"]` records; report fraction of verifier runs producing one. |

No gateway code change is needed for any of these. The contract is
sufficient because the audit log is already SPEC §7.4 date-rotated and
already passes through the existing `AuditSink` trait — any downstream
consumer (jq pipeline, Vector route, Prometheus exporter) can tail it.

### 20.4 Error codes added by §20

| Code | When |
|---|---|
| `INVALID_CONFIDENCE` | An `Evidence.confidence` value is outside `0.0..=1.0`. |
| `EVIDENCE_DIGEST_REQUIRED` | An `evidence` guard with `require_digest: true` saw a record missing a `digest`. |
| `EVIDENCE_CONFIDENCE_BELOW_THRESHOLD` | An `evidence` guard with `min_confidence: N` saw a record with no confidence or confidence < N. |

All three are surfaced as transition rejection codes (mirroring
`GUARD_REJECTED`) when the rejecting guard is the `evidence` kind with
the new clauses.
