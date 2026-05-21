# Blog articles — design spec

**Date:** 2026-05-21
**Status:** Approved (brainstorming complete)
**Branch:** `blog-articles`

## Goal

Write seven long-form blog articles for the mcp-flowgate site: one welcome
article and six deep-dives. Every article must be **specifically about
mcp-flowgate and rooted in the tool's actual, in-repo capabilities** — real
YAML, real wire traces, real audit event names, real numbers, real error
codes. No generic MCP think-pieces. No invented capabilities.

The articles serve a "long-form site → LinkedIn breakout" content strategy:
each post is the canonical reference (SEO + credibility), shareable enough
to spawn a compressed LinkedIn narrative.

## Context

- The site is an Astro project under `site/`. Blog posts are standalone
  `.astro` pages in `site/src/pages/blog/`.
- Two posts exist today: `why-seven-tools.astro` and
  `deterministic-chaining-explained.astro` (both dated 2026-05-21).
- Decision: **full set, replace old.** The two existing posts are deleted;
  their topics are absorbed (deeper) into the new set.

## Approach

**Hybrid (concept + worked example).** Each article opens with a concept
hook and narrative, then anchors to *one real worked example* — actual
config, actual wire output, actual numbers. Credible enough to be the
canonical reference, shareable enough for the LinkedIn breakout. This
matches the two existing posts' house style.

## Global mechanics

### File format & site integration

- Each article is a new file `site/src/pages/blog/<slug>.astro`.
- Structure copied verbatim from the existing posts:
  - frontmatter `---` importing `Landing` from `../../layouts/Landing.astro`;
    plus `const <name>Code = ...` strings for syntax-highlighted code blocks
    where needed (manual `<span>` coloring, as in
    `deterministic-chaining-explained.astro`).
  - `<Landing title="… — mcp-flowgate" description="…">`.
  - `<nav>` block — copied exactly from existing posts.
  - `<article class="pt-32 pb-20 px-6 bg-white min-h-screen">` with a
    `max-w-3xl` inner div, back-to-blog link, `<time datetime="2026-05-21">`,
    `<h1>`, and a `<div class="blog-body …">` body.
  - body uses the established classes: lead `<p class="text-xl …">`,
    `<h2 class="text-2xl font-bold text-slate-900 mt-12 mb-4">`,
    `<ul class="my-6 space-y-2 text-slate-600">`, inline `<code>`, and
    `code-window` blocks for YAML/JSON.
  - `<footer>` block — copied exactly from existing posts.
- `site/src/pages/blog/index.astro` is rewritten: one `blog-card` per
  article. Welcome listed first, then the six deep-dives.
- `why-seven-tools.astro` and `deterministic-chaining-explained.astro` are
  deleted. The site is days old with no SEO equity, so orphaned URLs are a
  non-issue; all seven articles get fresh, clean slugs.
- All articles dated 2026-05-21.

### Voice & style

- Follows the Matt C blog style guide (Henneke Duistermaat / Enchanting
  Marketing voice): write for one reader, empathy before advice, plain
  English, concrete specifics, short paragraphs, momentum, honest about
  trade-offs.
- Consistent with the two existing posts' tone and length feel.
- **Honesty is preserved verbatim from the repo.** Every article that
  touches a caveated area states the caveat:
  - "HATEOAS-*inspired*, not literally HATEOAS — JSON-RPC over MCP, not
    REST/hypermedia."
  - Pre-1.0; no published case studies; throughput under real load not yet
    measured.
  - The bundled `FlowgateServer` treats every caller as
    `Principal::anonymous()`; `permission`/`role` guards are not enforced
    for local single-user use.

### Evidence rule

- The evidence backbone is **in-repo reality**: real YAML snippets, the
  real `content-publish` wire trace from the README, real audit event
  names, real `PERFORMANCE.md` numbers, real error codes.
- External industry claims (e.g. "75% of API vendors will have MCP
  features", "tool-selection accuracy research") are included **only with a
  real, linkable source**. Otherwise they are cut or reframed as plain
  reasoning ("the math is simple") rather than borrowed authority. No
  unsourced statistics, no fake certainty.

### Length

- Welcome: ~1,300 words.
- Each deep-dive: ~1,800–2,400 words.

### References

- In-repo evidence is linked inline (to `/guides/*`, `/reference/*`,
  example directories on GitHub).
- A `References` section is added only where an article makes a
  research-backed external claim. Known real source: HATEOAS → Roy
  Fielding's dissertation (Ch. 5) / the HATEOAS Wikipedia page already
  linked in the repo.

### CTA

- Each article ends with a concrete "try it" close linking the most
  relevant `/guides/*` or `/quick-start` doc plus the GitHub repo.

## Capability reference (shared ground truth)

The seven stable MCP tools: `gateway.home`, `gateway.search`,
`gateway.describe` (discovery); `workflow.start`, `workflow.get`,
`workflow.submit`, `workflow.explain` (action).

Key facts articles draw on:

- Token math: ~50–150 tokens per tool definition; 10 tools ≈ 1,000 tokens,
  50 tools ≈ 5,000+; output tokens cost ~3–5× input tokens.
- Trichotomy: capability (what it can do) / exposure (`proxy.expose`, what's
  published) / workflow (`workflows.*`, when it may run). Proxy mode
  compiles to a workflow named `proxy_default`.
- Links: every response carries a `links` array of legal next moves; wrong
  calls still return legal links + a recovery path. Error codes:
  `STALE_WORKFLOW_VERSION`, `INVALID_TRANSITION`, `INPUT_SCHEMA_VIOLATION`,
  `GUARD_REJECTED`, `EXECUTOR_FAILED`, `CHAIN_FAILED`, `ACTOR_MISMATCH`.
- Guards: `permission`, `role`, `expr`, `evidence`. Actor enforcement:
  `actor: human` / `agent` / `deterministic`; mismatch → `ACTOR_MISMATCH`.
- Reliability: `timeoutMs`, `retry` (backoff), `fallback`, idempotency keys.
- Deterministic chaining: `actor: deterministic`; chains when a state has
  only deterministic transitions; stops at a decision point / terminal
  state / `maxChainDepth` (default 50) / executor failure; emits
  `chain.step`, `chain.completed`, `chain.failed`; hidden from `links`;
  failure surfaces a recovery link. Auto-branching via `branches:` +
  `treatNonZeroAsFailure: false`.
- Audit: ~18 event types to `stdout | file | memory | none`; every event
  carries `id`, `timestamp`, `workflowId`, `correlationId`, `actor`,
  `eventType`, `payload`.
- Performance: end-to-end gateway overhead per `workflow.submit` is under
  1 ms on the reference machine; ~700 ns in-memory floor; ~95 µs with the
  SQLite store; null audit sink ~180 ns.
- Examples: `content-publish` (governance), `expense-approval`
  (multi-tenant, quorum, idempotent payment), `tdd` (red→green→refactor),
  `deploy-pipeline` (deterministic chaining).

## The seven articles

### 1. Welcome — "The bet behind mcp-flowgate"

- **Slug:** `welcome` · **Length:** ~1,300 words
- **Core idea:** mcp-flowgate makes one bet — every tool reaches the model
  through a fixed seven-tool surface, and governance is *declared, not
  coded*.
- **Rooted in:** the 7 tools; capability/exposure/workflow model; the
  flat→governed progression.
- **Worked example:** the one-line `hello.echo` config (from the README
  quick-start) growing into a governed workflow without rewiring.
- **Honesty beat:** pre-1.0, no case studies yet, throughput not
  load-tested. Closes by previewing the six deep-dives.
- **CTA:** `/quick-start`, GitHub.

### 2. "The hidden cost of 50 MCP tools"

- **Slug:** `hidden-cost-of-mcp-tools` · **Length:** ~2,000 words
- **Core idea:** every registered tool is a recurring tax — input tokens
  for definitions *and* output tokens for the reasoning to choose among
  them; the second tax is bigger.
- **Rooted in:** the token math above; `gateway.search` returning one hit
  instead of 50 definitions; the fixed seven-tool surface.
- **Worked example:** before/after — 50 tool definitions in context vs. the
  real `content-publish` wire trace (`gateway.search "publish content"` →
  one result with a `start` link).
- **CTA:** `/quick-start`, `/reference/tools`.

### 3. "HATEOAS for AI: REST's oldest idea is the right pattern for agents"

- **Slug:** `hateoas-for-ai` · **Length:** ~2,100 words
- **Core idea:** the server telling the client what's legal next removes
  the model's need to carry the whole state machine in its context.
- **Rooted in:** the `links` array; two link layers (discovery + action);
  prefilled links; wrong calls that still return legal links + a recovery
  path (`GUARD_REJECTED` example).
- **Worked example:** the `content-publish` wire trace — `workflow.start` →
  prefilled link → walk links → a rejected call returns legal links →
  recover.
- **Honesty beat:** "HATEOAS-*inspired*, not literal" stated plainly.
- **References:** Roy Fielding's dissertation / HATEOAS page.
- **CTA:** `/guides/discovery`, `docs/CONCEPTS.md`.

### 4. "Stop writing approval gates in code"

- **Slug:** `governance-is-not-application-code` · **Length:** ~2,000 words
- **Core idea:** approval gates, retries, permission checks written as code
  inside each tool drift, can't be tested as a set, and are invisible.
  Declare them as data.
- **Rooted in:** guards (`permission`/`role`/`expr`/`evidence`);
  `actor: human` + `ACTOR_MISMATCH`; `inputSchema`; the `reliability:`
  block; audit events; `mcp-flowgate check`; the docs' own "why not CEL"
  reasoning (keep config as inspectable data, no logic smuggling).
- **Worked example:** `actor: human` — one line vs. the code you would
  otherwise write — and the wire trace where the model has *no* legal path
  past the gate.
- **CTA:** `/guides/governance`, `docs/GOVERNANCE.md`.

### 5. "Flat tool lists don't scale — and agents are about to find out"

- **Slug:** `flat-tool-lists-dont-scale` · **Length:** ~2,000 words
- **Core idea:** a flat list is fine at 10 tools and quietly breaks at 200 —
  the failure is not just tokens, it is that the model has no structure to
  navigate. Distinct from article 2: article 2 is cost *today*; article 5
  is the *structural/scaling* break.
- **Rooted in:** proxy mode vs. workflows; `gateway.search` lexical scoring
  (title/description/tags); `proxy.import` (no rewriting definitions); the
  hierarchical gateway layer cake.
- **Worked example:** the `MCP-CONTROL-ARCHITECTURE` stack — one tool →
  governed → stacked gateways, "six lines of meaningful code" per layer.
- **CTA:** `docs/MCP-CONTROL-ARCHITECTURE.md`, `/guides`.

### 6. "Where MCP security actually lives — and where it doesn't yet"

- **Slug:** `mcp-security-gateway-layer` · **Length:** ~2,100 words
- **Core idea:** the gateway is where controls that don't fit in a tool
  definition belong — and being honest about what is *enforced* vs. what
  needs wiring is the whole point.
- **Rooted in:** `inputSchema` rejecting bad input before the executor
  runs; guards; actor enforcement; the audit trail (`correlationId`, every
  rejection logged as `transition.rejected`); evidence gates.
- **Honesty beat (the spine of the piece):** the bundled server treats
  every caller as `Principal::anonymous()`; `permission`/`role` are not
  enforced for local single-user use; multi-tenant requires a custom
  `ServerHandler` sourcing a verified `Principal` (JWT/mTLS), and the
  warning against model-asserted identity; cross-trust-boundary deployments
  need an identity proxy. No overclaiming.
- **CTA:** `SECURITY.md`, `docs/MCP-CONTROL-ARCHITECTURE.md` (identity
  section), `docs/EMBEDDING.md`.

### 7. "Deterministic chaining: when you don't need the LLM to decide"

- **Slug:** `deterministic-chaining` · **Length:** ~2,000 words
- **Core idea:** not every pipeline step is a decision — tag the computable
  ones and the runtime runs them itself; the model only wakes for real
  choices.
- **Rooted in:** `actor: deterministic`; chaining + stop conditions
  (`maxChainDepth` 50); the chain trace; mid-chain failure + recovery link;
  `chain.*` audit events; auto-branching + `treatNonZeroAsFailure`.
- **Worked example:** the `deploy-pipeline` (lint → test → build
  auto-execute, stop at `ready_to_deploy`); round-trip math — a 10-step
  pipeline with 8 computable steps saves 8 LLM round trips. Deeper than the
  post it replaces: adds auto-branching and the audit-trail angle.
- **CTA:** `/guides/chaining`, the `deploy-pipeline` example on GitHub.

## Out of scope

- Site layout, styling, or component changes beyond adding blog pages and
  updating the blog index.
- LinkedIn post copy (the articles enable it; writing it is a later task).
- Any change to the docs under `site/src/content/docs/`.

## Success criteria

- Seven `.astro` articles plus an updated `blog/index.astro`, building
  cleanly under Astro.
- Every capability claim traceable to the repo. No invented features.
- Each article passes the Matt C style guide's pre-publish checklist.
- The two superseded posts are removed and not linked anywhere.
