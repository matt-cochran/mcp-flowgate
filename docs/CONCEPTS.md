# Concepts

The mental model behind mcp-flowgate. Read this once and the rest of the
docs become reference material.

---

## Every tool is a transition

A "workflow" is a state machine: each state has transitions, each
transition has a target state, and each transition can have an executor
that does work. The simplest workflow has one state called `ready` with
one transition per tool — call any tool, end up back at `ready`.

```
proxy mode (one state, many tools):

   ready ──hello.echo──→ ready
   ready ──github.list_issues──→ ready
   ready ──dotnet.test──→ ready

governed workflow (many states, ordered transitions):

   planning ──submit_plan──→ risk_review ──remediate──→ risk_review
                                          \──request_approval──→ awaiting_approval
                                                                  ──approve──→ executing
                                                                  ──reject──→ planning
```

Same engine. Same tool surface to the model. The only difference is how
many states the config defines. You can start with a flat list of tools
and progressively wrap any of them in a workflow when you want
governance — without rewiring anything.

The proxy form lives in `proxy.expose`. The workflow form lives in
`workflows.*`. Internally the proxy form compiles to a workflow called
`proxy_default`.

---

## Two layers of links: discovery and action

If you've used a well-designed REST API, you've followed links from a
resource to its valid next operations. mcp-flowgate borrows the
**philosophy** of [HATEOAS](https://en.wikipedia.org/wiki/HATEOAS) — the
server returns links describing the legal next actions, so the client
doesn't need out-of-band knowledge of the state machine.

It's *HATEOAS-inspired*, not literally HATEOAS: the protocol is
JSON-RPC over MCP, not REST/hypermedia. What carries over is
server-driven navigation through links.

There are two layers of links.

**Gateway layer (discovery)** — *what can I do?*

| Tool                 | Returns                                              |
|----------------------|------------------------------------------------------|
| `gateway.home`       | search + list links                                  |
| `gateway.search`     | workflow & capability hits, each with a `start` link |
| `gateway.describe`   | details for one item, including its `inputSchema`    |

**Workflow layer (action)** — *what's the next legal step here?*

| Tool                | Returns                                          |
|---------------------|--------------------------------------------------|
| `workflow.start`    | workflow snapshot + transition links             |
| `workflow.submit`   | new snapshot + new transition links              |
| `workflow.get`      | current snapshot + current legal links           |
| `workflow.explain`  | debug: is this transition allowed right now?     |

A typical model loop:

```
1. gateway.search { query: "list github issues" }
   → hits[0] has a `start` link calling workflow.start
2. workflow.start { definitionId: "proxy_default", input: {} }
   → response includes links: [{ rel: "github.list_issues", method: "workflow.submit", args: {…} }]
3. workflow.submit { workflowId, expectedVersion, transition: "github.list_issues", arguments: { repo: "…" } }
   → response includes the result and any new legal links
```

The model never carries tool definitions in its context. It carries one
*current* response and follows its links.

---

## What if I call something invalid?

A wrong call still returns the current legal links — the model can
recover without restarting:

```json
{
  "result": { "status": "rejected" },
  "error": {
    "code": "GUARD_REJECTED",
    "message": "One or more guards rejected the transition.",
    "attemptedTransition": "approve"
  },
  "links": [
    { "rel": "request_changes", "method": "workflow.submit", "args": { "…": "…" } }
  ]
}
```

Error codes you'll see: `STALE_WORKFLOW_VERSION`, `INVALID_TRANSITION`,
`INPUT_SCHEMA_VIOLATION`, `GUARD_REJECTED`, `EXECUTOR_FAILED`,
`CHAIN_FAILED`. Every rejection emits a `transition.rejected` audit
event so you can see them even when the model recovers silently.

---

## Deterministic chaining

Not every transition needs an LLM decision. Tag a transition with
`actor: "deterministic"` and the runtime auto-executes it without
waiting for the model. When a state has *only* deterministic
transitions, the engine chains through them automatically — lint,
test, build, whatever — and stops at the first state that needs a
decision.

```
   lint ──run_lint──→ test ──run_tests──→ build ──build_artifact──→ ready_to_deploy
   ^^^ all deterministic, auto-executed ^^^                         ^^^ agent decides ^^^
```

The model calls `workflow.start` and gets back the response at
`ready_to_deploy` with a `chain` trace showing what happened. It
never sees the intermediate steps as links — they're hidden.

If a deterministic step fails mid-chain, the response includes
partial progress and a recovery link for the failed step, so the
model can retry.

See [CONFIG.md](CONFIG.md#deterministic-chaining) for the YAML shape.

---

## Phase guidance

Each state can carry `goal` and `guidance` strings that appear in
every workflow response. `goal` is the one-line objective;
`guidance` is detailed instructions for the model.

```yaml
states:
  ready_to_deploy:
    goal: Confirm deployment
    guidance: Review lint, test, and build results before proceeding.
```

This is the complement to `prefill` (which pre-shapes *arguments*):
phase guidance pre-shapes the model's *reasoning* about what to do
at each step. See [CONFIG.md](CONFIG.md#phase-guidance) for details.

---

## The full picture

```
   MCP host (Claude Desktop, IDE, agent runner)
                    │  stdio
                    ▼
   ┌─────────────────────────────────────────────┐
   │                FlowgateServer                │
   │  gateway.{home,search,describe}              │
   │  workflow.{start,get,submit,explain}         │
   └────────────┬────────────────┬────────────────┘
                │                │
                ▼                ▼
        DiscoveryIndex     WorkflowRuntime
        (lexical search)   ├─ DefinitionStore (workflows + proxy_default)
                           ├─ WorkflowStore   (memory | file | sqlite)
                           ├─ EvidenceStore   (memory; pluggable trait)
                           ├─ ExecutorRegistry
                           │   ├─ noop / cli / mcp (process or HTTP) / rest / human
                           │   └─ each call wrapped in:
                           │       ReliabilityPolicy (timeout / retry / fallback)
                           ├─ GuardEvaluator (permission / role / expr / evidence)
                           └─ AuditSink      (stdout / file / memory / null)

   Capabilities feed both DiscoveryIndex and proxy_default's transitions:
   - Defined  — `proxy.expose`
   - Imported — `proxy.import` (tools/list discovery; vendor-neutral)
```

Two link layers (discovery + action), three capability sources
(defined, imported, raw CLI / REST), every step audited, every executor
invocation reliability-wrapped, every successful executor's evidence
persisted for downstream guards.

---

## Where to next

- The list of governance knobs: [GOVERNANCE.md](GOVERNANCE.md)
- The configuration reference: [CONFIG.md](CONFIG.md)
- How to compose configs for larger systems: [MCP-CONTROL-ARCHITECTURE.md](MCP-CONTROL-ARCHITECTURE.md)
