# mcp-flowgate

[![CI](https://github.com/matt-cochran/mcp-flowgate/actions/workflows/ci.yml/badge.svg)](https://github.com/matt-cochran/mcp-flowgate/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Your LLM reads your entire tool list on every call. mcp-flowgate replaces it with ten.**

Wire in any number of MCP servers, CLI commands, and REST APIs. The
model never sees them in its tool list. It searches for what it needs,
follows links to act, and every action is schema-validated,
guard-checked, and audited. You configure it all in YAML.

---

## The problem you already have

Every MCP tool you register lands in the model's system prompt. Each
tool definition costs 50-150 tokens for its name, description, and
schema. Ten tools: fine. Fifty: you're spending 5,000+ tokens per
call just to describe what's available — before the model thinks a
single thought.

It gets worse. The model has to *reason* about every tool in the list
to pick the right one. More tools means more output tokens spent
choosing, more wrong choices, more retries, more cost. A model
staring at 50 tools and picking the wrong one wastes a full round
trip — the failed call, the recovery, the retry.

And none of this comes with audit, retries, approval gates, or any
governance. You get a flat list and a prayer.

---

## The fix: ten tools, any number of capabilities

mcp-flowgate exposes exactly ten MCP tools regardless of how many
capabilities you wire in:

| Layer | Tools | Purpose |
|-------|-------|---------|
| **Discovery** | `gateway.home`, `gateway.search`, `gateway.describe` | Find capabilities by keyword, get schemas on demand |
| **Action** | `workflow.start`, `workflow.get`, `workflow.submit`, `workflow.explain` | Execute capabilities through governed state machines |
| **Vocabulary** | `gateway.lexicon.search`, `gateway.lexicon.lookup`, `gateway.lexicon.define` | Persistent ubiquitous-language store; vocabulary accumulates across runs (SPEC §30) |

The model's tool list is always ten entries. Your 50 capabilities
surface through search results and response links — loaded one at a
time, only when relevant.

**The result:** fixed token cost for tool definitions regardless of
scale. The model searches instead of scanning, follows links instead
of guessing, and recovers from mistakes because every response
carries the legal next moves.

This pattern has a name in API design:
[HATEOAS](https://en.wikipedia.org/wiki/HATEOAS) — the server tells
the client what's legal next. The model doesn't need out-of-band
knowledge. It just follows the links.

---

## Install

**Pre-built binary** (recommended) — download for your platform from
[GitHub Releases](https://github.com/matt-cochran/mcp-flowgate/releases),
verify with the included `checksums.sha256`:

```bash
# Linux x86_64 example:
curl -LO https://github.com/matt-cochran/mcp-flowgate/releases/latest/download/mcp-flowgate-v0.2.0-x86_64-unknown-linux-gnu.tar.gz
tar xzf mcp-flowgate-*.tar.gz
./mcp-flowgate --help
```

**Cargo:**

```bash
cargo install mcp-flowgate
```

**Docker:**

```bash
docker run -v $(pwd)/gateway.yaml:/config/gateway.yaml ghcr.io/matt-cochran/mcp-flowgate
```

---

## Try it in 30 seconds

```bash
git clone https://github.com/matt-cochran/mcp-flowgate
cd mcp-flowgate
cargo build --release

cat > hello.yaml <<'EOF'
version: "1.0.0"
proxy:
  expose:
    - name: hello.echo
      description: Returns the message you sent.
      executor: { kind: noop }
EOF

./target/release/mcp-flowgate serve --config hello.yaml
```

Wire it into Claude Desktop
(`~/.config/Claude/claude_desktop_config.json`, macOS:
`~/Library/Application Support/Claude/claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "flowgate": {
      "command": "/absolute/path/to/mcp-flowgate",
      "args": ["serve", "--config", "/absolute/path/to/hello.yaml"]
    }
  }
}
```

Restart the host. Seven tools appear. The model can find and call
`hello.echo` through them. You just shipped a tool with discovery,
schema validation, and audit built in.

| Host | Config location | Example |
|------|----------------|---------|
| Claude Desktop | `~/.config/Claude/claude_desktop_config.json` (macOS: `~/Library/…/claude_desktop_config.json`) | See above |
| Zed | `~/.config/zed/settings.json` | [`examples/zed-gateway/`](examples/zed-gateway/) |

That was one tool. The interesting part is what happens when you add
fifty — and the model's tool list stays at ten.

---

## Discovering commands

`flowgate` ships with a full clap-driven CLI. Four ways to find what's available:

```bash
flowgate --help                    # grouped list of every subcommand
flowgate help <subcommand>         # long-form description + example
flowgate completions bash          # emit a tab-completion script for your shell
flowgate man | less                # roff-formatted man page (install with: sudo tee /usr/local/share/man/man1/flowgate.1)
```

Commands are grouped under three headings:

- **Agent runtime** — `headless`, `acp`, `walk` (interactive TUI is the default with no subcommand).
- **Agent configuration** — `agent`, `set-provider-keys`, `migrate-agents-from-cli`, `validate-agents-config`.
- **Diagnostics & generators** — `doctor`, `mcp init`, `completions`, `man`.

### Setting provider API keys

`flowgate set-provider-keys` writes a flat dotenv file at `~/.config/flowgate/providers.env` (mode 0600, parent dir 0700) which is loaded into env at startup. Existing environment variables take precedence, so CI / shell exports always win over the file.

```bash
flowgate set-provider-keys                          # interactive walk through all 5 providers
flowgate set-provider-keys --provider anthropic     # one provider, no-echo prompt
echo "$KEY" | flowgate set-provider-keys --provider openrouter --stdin
flowgate set-provider-keys --list                   # show configured providers (masked)
flowgate set-provider-keys --remove gemini          # clear one provider's vars
flowgate set-provider-keys --path                   # print the resolved file path
```

Override the file location with `$FLOWGATE_PROVIDER_KEYS_FILE`. Supported providers: `anthropic`, `openai`, `openrouter`, `bedrock`, `gemini`.

---

## v0.2 — two-tier composition

The biggest addition in v0.2 is **capabilities + orchestrators**:
typed sub-workflows you compose into lifecycle pipelines instead of
re-authoring every workflow from scratch.

- A **capability** (`cap.<verb>.<name>`) declares a typed
  `snippet: { inputs, outputs }` contract. It's a composition leaf
  — it runs cognitive work, deterministic scripts, HITL gates, or
  external coordination, but never invokes another workflow.
- An **orchestrator** (`flow.<name>`) declares an `inputs:` entry
  signature and composes capabilities via `kind: workflow`
  executors with `use: { inputs, outputs }` bindings. The
  capability runs in its own scoped blackboard; only declared
  outputs propagate back to host slots.
- A **repo manifest** (`flowgate.repo.yaml`) declares a namespace,
  version, and layout. Operators load any number of repos via the
  gateway's top-level `repos:` block; every loaded definitionId
  is namespace-prefixed `<ns>/<id>`.

Two sibling libraries demonstrate the shape:

- [**cognitive-architectures**](https://github.com/matt-cochran/cognitive-architectures) —
  4 lifecycle orchestrators (`flow.add-feature`,
  `flow.bugfix-from-error-log`, `flow.safe-refactor`,
  `flow.triage-issue`) composing 22 reusable capabilities.
- [**flowgate-meta**](https://github.com/matt-cochran/flowgate-meta) —
  5 orchestrators: 4 meta-authoring (`flow.author-capability`,
  `flow.author-flow`, `flow.optimize-capability`,
  `flow.optimize-flow`) plus `flow.configure-models` — the v0.3
  guided agents.yaml setup flow. Self-bootstrapping authoring loop.

Operators load both with a single top-level block:

```yaml
# gateway.yaml
version: "1.0.0"
repos:
  - path: /repos/cognitive-architectures
  - path: /repos/flowgate-meta
```

Validation runs at load (`mcp-flowgate check`). The validation
cloud V1–V23 catches verb misuse, slot reachability, type
consistency, missing snippet contracts, anonymous shadowing, and
more — bad configs fail at startup with named violations.

Spec: [docs/superpowers/specs/2026-05-26-capability-orchestrator-design.md](docs/superpowers/specs/2026-05-26-capability-orchestrator-design.md).

---

## Governance you declare, not code

Every capability passes through a state machine. The simplest has one
state and loops back to itself — a flat tool call. Add states and
rules when you need control:

```yaml
proxy:
  expose:
    - name: deploy.prod
      executor: { kind: human, queue: prod-deployments }
```

Now `deploy.prod` doesn't actually deploy. It records a
`human.approval.requested` audit event, returns a "pending" status,
and stops. The LLM cannot fire the action. A human watching the queue
does.

That's one line of YAML. The same declarative surface gives you:

| You know…                           | Declare…                                                         |
|-------------------------------------|------------------------------------------------------------------|
| What input is valid                 | `inputSchema` — bad input never reaches the executor             |
| Who should run this                 | Guards: `permission`, `role`, `expr`, `evidence`                 |
| What shouldn't run autonomously     | `actor: "human"` — enforced at submit time, not just a hint      |
| How calls fail                      | `reliability:` timeout, retry with backoff, fallback executors   |
| What should be logged               | Audit: every step emits structured JSON events automatically     |
| What steps come in what order       | Workflows: states, transitions, output mapping between steps     |

Every guardrail is YAML. No glue code, no per-tool wrapper, no
host-specific routing. Your tools stay in whatever language they
already live in.

---

## Deterministic chaining: skip what the LLM doesn't need to decide

Not every step requires judgment. Linting, running tests, building
artifacts — these are computable. Tag them `actor: "deterministic"`
and the runtime executes them automatically:

```yaml
states:
  lint:
    transitions:
      run_lint:
        target: test
        actor: deterministic              # auto-executes, no LLM involved
        executor: { kind: cli, command: lint-check }
  test:
    transitions:
      run_tests:
        target: build
        actor: deterministic
        executor: { kind: cli, command: test-runner }
  build:
    transitions:
      build_artifact:
        target: ready_to_deploy
        actor: deterministic
        executor: { kind: cli, command: build-artifact }
  ready_to_deploy:
    goal: Confirm deployment
    guidance: All checks passed. Review lint, test, and build results before deploying.
    transitions:
      deploy:
        target: deployed
        actor: agent                      # chain stops — LLM decides here
```

The model calls `workflow.start`. The runtime chains through lint →
test → build automatically and returns the response at
`ready_to_deploy`. Three executor calls, zero LLM round trips. The
response includes a `chain` trace of what happened and `guidance`
telling the model what to think about next.

If a chain step fails, the response includes the partial progress and
a recovery link so the model can retry just the failed step.

**The token math:** without chaining, the model would make three extra
round trips (read response → pick transition → submit → repeat),
each burning input tokens to re-read the workflow state and output
tokens to reason about the next step. With chaining, it's one call
and one response. For a 10-step deterministic pipeline, that's 10x
fewer round trips.

---

## Phase guidance: tell the model what to think about

Each state can carry `goal` and `guidance` strings:

```yaml
ready_to_deploy:
  goal: Confirm deployment
  guidance: >
    All automated checks passed. Review lint report, test count,
    coverage percentage, and artifact ID in the context before
    deciding to deploy or abort.
```

The response surfaces these as a `guidance` object. The model arrives
at a decision point with pre-shaped instructions — not just prefilled
arguments, but context for *how to reason* about the choice.

`goal` and `guidance` are indexed by `gateway.search`, so they
improve discoverability as well as runtime decisions.

---

## Proof: what the model sees on the wire

Claims deserve evidence. Here's the actual wire format from the
[`content-publish`](examples/content-publish/) workflow, captured
from a mechanical driver (the same pattern
[dogfooded in CI](examples/tdd/dogfood-drive.py)).

**Turn 1 — the model searches, not scans.**

```jsonc
→ gateway.search { "query": "publish content" }

← { "items": [
      { "id": "workflow:content_publish",
        "title": "Governed content publishing workflow",
        "tags":  ["content", "governed", "publishing"] } ] }
```

One hit. Not 50 tool definitions — one search result with a title.

**Turn 2 — start the workflow. Note the prefilled link.**

```jsonc
→ workflow.start {
    "definitionId": "content_publish",
    "input": { "topic": "Q2 launch", "audience": "enterprise" } }

← { "workflow": { "id": "wf_8f3a", "version": 0, "state": "idea" },
    "result":   { "status": "started" },
    "links": [
      { "rel":    "create_outline",
        "method": "workflow.submit",
        "args": {
          "workflowId":      "wf_8f3a",
          "expectedVersion": 0,
          "transition":      "create_outline",
          "arguments":       {} } } ] }
```

The response carries exactly *one legal next move*, prefilled. The
model doesn't guess the next tool or skip a step — it follows the
link.

**Turns 3–5 — the model walks the links forward.**

```
workflow.submit(create_outline)   → state="outlined",       link → write_draft
workflow.submit(write_draft)      → state="drafted",        link → run_brand_review
workflow.submit(run_brand_review) → state="brand_reviewed", link → request_approval
```

Each response advances state and offers only the legal next moves.

**Turn 6 — governance stops the model cold.**

```jsonc
→ workflow.submit { "transition": "request_approval", ... }

← { "workflow": { "state": "awaiting_approval" },
    "links": [
      { "rel": "approve",         "actor": "human", ... },
      { "rel": "request_changes", "actor": "human", ... } ] }
```

No `"actor": "agent"` link exists. If the model tries to submit
anyway, the runtime rejects with `ACTOR_MISMATCH` before the
executor runs. The only way forward is a human resolving the
approval queue.

The model reports: *"Submitted for approval; waiting on the
content-approvals queue."* The auditor has a complete trail. The
model has no path to skip the gate.

---

## Worked examples

| Example | What it demonstrates |
|---------|---------------------|
| [`content-publish/`](examples/content-publish/) | Governance: draft → brand review → human approval → publish. The LLM's only path is through the workflow. |
| [`expense-approval/`](examples/expense-approval/) | Multi-tenant: two-tier approval, quorum evidence, idempotent payment. |
| [`tdd/`](examples/tdd/) | Discipline: enforced red → green → refactor with cheat detection. [Dogfooded in CI](examples/tdd/dogfood-drive.py). |
| [`deploy-pipeline/`](examples/deploy-pipeline/) | Deterministic chaining: lint → test → build auto-execute; LLM only sees the deploy decision. |
| [`authoring-workflow.yaml`](examples/authoring-workflow.yaml) | Meta: the LLM authors new workflows and skill fragments, gated by structural analysis, dry-run, and the `guidance_acknowledged` rubric check. |

---

## Coding-agent recipe (commodity LLM ≥ frontier model)

See [`RESEARCH.md`](RESEARCH.md) for the full thesis: with a deterministic
gateway, a cheap or open-weight model can match or beat a frontier model
on real software-engineering work. The architecture is six Flowgate
states (planning → retrieving → editing → verifying → critiquing →
human_review), each backed by a guidance skill fragment, plus three
**external** tools that plug in via Flowgate's existing `connections:`
block. The Flowgate runtime stays unchanged.

### Three external tools (run as separate processes)

| Tool | Transport | Purpose |
|------|-----------|---------|
| **`scip-mcp`** *(community / write-your-own)* | MCP server | SCIP symbol queries, call/import graphs, CODEOWNERS lookup, test coverage mapping. Wraps the [SCIP indexers](https://github.com/sourcegraph/scip) for whichever languages your repos use. |
| **`constrained-edit`** *(write-your-own — small)* | CLI | Accept typed edit operations on stdin, validate paths against `constraints.forbiddenPaths`, apply atomically, emit unified diff on stdout. ~200 lines in your language of choice. |
| **`verifier-harness-mcp`** *(write-your-own)* | MCP server | Orchestrate containerised build → lint → fail-to-pass → pass-to-pass → coverage delta → mutation → security stages. Returns per-stage `{passed, artifacts}`. Wraps whatever build system your repo uses. |

Why external? Process isolation (a crashing linter doesn't crash the
workflow runtime), language freedom (each tool can be written in
whatever fits the domain), independent versioning (the verifier can
iterate without Flowgate releases), and a real security boundary
(a compromised editor tool cannot write transition records).

### Wiring it up

Once the three tools exist, declare them as connections and reference
them from the workflow:

```yaml
connections:
  codebase_graph:
    type: mcp
    transport: { kind: streamable_http, url: "http://localhost:7100/mcp" }
  verifier_harness:
    type: mcp
    transport: { kind: streamable_http, url: "http://localhost:7200/mcp" }
  constrained_edit:
    type: cli
    command: ["constrained-edit"]

workflows:
  swe_agent:
    initialState: planning
    states:
      retrieving:
        executor: { kind: mcp, connection: codebase_graph, tool: assemble_evidence_pack }
      editing:
        executor: { kind: cli, connection: constrained_edit }
      verifying:
        executor: { kind: mcp, connection: verifier_harness, tool: run_harness }
      # …critiquing, human_review states with the standard skills+guards…
```

The complete workflow YAML, the five skill fragments, and the
escalation guard expressions (cheap-first routing, retry budget,
risk-based human gating) live in [`RESEARCH.md`](RESEARCH.md) §"Reference
workflow (Flowgate YAML)". Copy-paste runnable.

### Setup checklist

1. **Pick or build the three external tools.** None ship with Flowgate.
   Build them in the language and runtime that fit your codebase. The
   SCIP server is the only one with substantive prior art; the other
   two are intentionally small.
2. **Run them locally** on the ports your config references. For a CI
   integration, run them as sidecar containers in the same pod.
3. **Author the SWE-agent YAML** under `examples/swe-agent.yaml` (or
   wherever your config tree puts it) following the
   [`RESEARCH.md`](RESEARCH.md) reference.
4. **Author the five guidance skills** (`plan.specify.change-request`,
   `diagnose.codebase.search`, `implement.edit.constrained`,
   `review.code.adversarial`, `review.code.final-approval`) — markdown
   blobs with `verb` and `lifecycle` per [SPEC §5](SPEC.md).
5. **Wire the LLM client** to call `workflow.start` on `swe_agent`
   with the incoming ticket, then drive the returned HATEOAS links
   until completion. The LLM sees only the ten Flowgate tools at any
   time; the workflow's state surfaces the next legal moves.

### What you get

- **Audit log of every coding session** — `tail -f audit.log | jq …`
  gives you the SWE-agent scorecard (resolved rate, cost per fix,
  retry count, etc.) per [SPEC §20.3](SPEC.md).
- **Cheap-first routing as guard expressions** — escalate to a more
  capable model only when `retryCount`, `risk`, or verifier failure
  evidence justifies it. The LLM client reads the audit log to decide
  which tier to use next.
- **Human-in-the-loop for high-risk changes** — any change touching
  auth, secrets, schemas, or cross-repo contracts routes to a
  `human_review` state via a simple guard on `$.context.risk`.

The point: you're not buying Flowgate to run a coding agent. You're
buying the gateway your existing coding agent infrastructure needs to
become governable, replayable, and cheap.

---

## Curated scripts: deterministic actions, hash-pinned

Skills tell the LLM what to think. Scripts tell the workflow what to
do — deterministically, with no model involvement. A `scripts:` block
declares a library of curated script bodies that workflows invoke
through the `script` executor:

```yaml
scripts:
  build.cargo.release:
    verb: build
    lifecycle: stable
    body: |
      #!/usr/bin/env bash
      set -euo pipefail
      cargo build --release --locked "$@"

workflows:
  ci:
    states:
      building:
        transitions:
          done:
            target: testing
            executor: { kind: script, subject: build.cargo.release }
```

Each script is **content-identified by hash** — workflows reference
them by subject, and the runtime materializes the body from the
workflow's pinned snapshot (SPEC §8.2 invariant). Editing the script
body after `workflow.start` is invisible to in-flight instances. An
audit replay can pull the exact body that ran by hash from cold
storage.

External bodies work too: a script can declare `uri: file://...` +
`hash: sha256:...` to source from disk, with load-time hash
verification. Hash drift fails the load with `SCRIPT_HASH_MISMATCH`.

For destructive scripts (deploys, migrations), pair the `script`
executor with a `script_acknowledged` guard — the workflow refuses to
run until an operator has called `gateway.describe` on the current
body (review-before-execute, hash-flip-invalidated).

The curated library lives in the sibling
[`cognitive-architectures`](https://github.com/matt-cochran/cognitive-architectures)
repo with its own `scripts/` directory — copy-paste-ready scripts for
the common verbs (build/test/deploy/format/lint/install/verify/run).

## The TUI agent — commodity models outperform frontier

The `flowgate-tui` binary (also installed as `flowgate`) walks a
workflow end-to-end through a **deterministic graph-walking interpreter**.
Where conventional agent loops let one orchestrator-LLM accumulate the
full conversation, the interpreter spawns **isolated sub-agent sessions**
per workflow state. Each sub-agent sees only its scoped guidance + the
blackboard at its phase — no accumulated context from previous phases.

```bash
flowgate walk \
  --workflow swe_agent \
  --input '{"issue": "Add timeout to RegistryExecutor"}' \
  --agent planning=anthropic/claude-sonnet-4 \
  --agent retrieval=openrouter/qwen-2.5-coder-7b \
  --agent editing=openrouter/qwen-2.5-coder-7b \
  --agent critique=anthropic/claude-opus-4 \
  --max-sub-agent-seconds 120 \
  --max-sub-agent-steps 20
```

What the interpreter does:

- **States with `delegate:`** spawn an isolated session running the
  named agent config. Returns when the sub-agent calls
  `workflow.submit` (advancing the workflow) or hits its timeout /
  step limit.
- **States with a single non-deterministic actionable link** auto-advance
  without any LLM involvement. No tokens spent on decisions that
  guards can make.
- **States with multiple links remaining** pick the first
  non-`escalate` link. The critic + retry cycle on the next workflow
  iteration corrects wrong picks automatically.
- **Sub-agent timeouts** retry up to 3 times, then submit the
  `escalate` transition if one is declared, else propagate.

`--max-sub-agent-seconds` and `--max-sub-agent-steps` are **required by
design** — an unbounded sub-agent is a foot-gun. The TUI rejects
startup if either is missing.

The result: a Qwen 7B editor directed by Sonnet-grade planning and
reviewed by an Opus-grade critic costs a fraction of running Opus for
the whole session — and produces better results because each model does
only the task it's best at, with only the context it needs.

See [`docs/TUI-AGENT.md`](docs/TUI-AGENT.md) for the full algorithm,
sub-agent lifecycle, and the cognitive-architecture thesis.

---

## Is this for you?

**Use it when** you have multiple tools and any of these matter:
fewer tokens in the model's context, audit, retries, approval gates,
schema validation, or multi-step workflows. The ten-tool surface
scales to hundreds of capabilities. `proxy.import` means you don't
rewrite tool definitions you already have.

**Skip it when** you have one MCP server with no governance needs —
just point the host at it directly.

**vs. a small stdio proxy.** A 30-line Python proxy gets you
multiplexing. Then you'll add audit, retries, idempotency keys,
fallback executors, optimistic-locking persistence, evidence gates,
recovery links, and tool-list import — each one a week of work.
If multiplexing is all you need, write the 30 lines. If anything
else on that list is on your roadmap, it's already here.

---

## Going to production

The 30-second setup uses defaults that trade durability for speed.
For production, see [docs/CONFIG.md](docs/CONFIG.md) — in particular:

- **Persistent store:** `store: { kind: sqlite, path: … }` — the
  default `memory` store loses state on restart.
- **Audit to disk:** `audit: { sink: file, path: … }` — one JSON
  line per event; set up rotation.
- **Config validation:** `mcp-flowgate check --config X.yaml` in CI —
  catches dangling transition targets, unreachable states, and dead-end
  non-terminal states.
- **Schema-aware editing:** point `yaml.schemas` at
  `schemas/gateway-config.schema.json` in VS Code / IntelliJ.
- **Config hot-reload:** Send SIGHUP to reload config without
  dropping in-flight workflows. See [docs/CONFIG.md](docs/CONFIG.md).

### What needs care

| Concern | Guidance |
|---------|----------|
| Multi-tenancy | Single-user and same-trust-boundary use is production-ready. Cross-trust-boundary deployments should add an identity proxy (Envoy, OAuth2-proxy) in front. |
| High availability | `store: { kind: postgres, url: … }` enables multi-process deployments behind a load balancer. |
| Load testing | Microbenchmarks in [`PERFORMANCE.md`](PERFORMANCE.md). Throughput under real load is not yet measured. |

---

## Go deeper

| You want to…                                       | Read…                                                                |
|----------------------------------------------------|----------------------------------------------------------------------|
| Get the mental model                               | [docs/CONCEPTS.md](docs/CONCEPTS.md)                                |
| Govern who/when/how tools run                      | [docs/GOVERNANCE.md](docs/GOVERNANCE.md)                            |
| Maximize LLM guidance via prefill + phase guidance | [docs/LLM-GUIDANCE.md](docs/LLM-GUIDANCE.md)                        |
| Connect to MCP servers, CLIs, REST APIs            | [docs/CONNECTIONS.md](docs/CONNECTIONS.md)                          |
| Look up a config knob                              | [docs/CONFIG.md](docs/CONFIG.md)                                    |
| Compose configs for big systems                    | [docs/MCP-CONTROL-ARCHITECTURE.md](docs/MCP-CONTROL-ARCHITECTURE.md) |
| Embed the library or bake config in                | [docs/EMBEDDING.md](docs/EMBEDDING.md)                              |
| See what the runtime guarantees                    | [docs/INVARIANTS.md](docs/INVARIANTS.md)                            |
| See what we pressure-tested and fixed              | [docs/STRESS-TESTS.md](docs/STRESS-TESTS.md)                        |
| Work on the codebase                               | [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)                          |

---

## Performance

Benchmarks measure the overhead of core operations. See
[`PERFORMANCE.md`](PERFORMANCE.md) for numbers and interpretation.

```bash
cargo bench --bench gateway_overhead
```

---

## Case studies

None published yet — this project is pre-1.0. If you've deployed
mcp-flowgate in production or pilot, open an issue — failure stories
are welcome alongside successes.

---

## License

Apache-2.0.
