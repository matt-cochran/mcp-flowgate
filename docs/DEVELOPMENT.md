# Development

Working on the codebase, running tests, and what the trait seams give
you for future work.

---

## Workspace layout

```
crates/
  mcp-flowgate-schema/        typify-generated types from schemas/*.json
  mcp-flowgate-core/          runtime, ports, audit, reliability, discovery,
                              capability, evidence, in-memory + file + sqlite stores,
                              config preprocessor (capabilities / wraps / include)
  mcp-flowgate-executors/     cli, mcp (process + HTTP), rest, human, noop,
                              registry, import (tools/list)
  mcp-flowgate-mcp-server/    FlowgateServer (rmcp ServerHandler) — the 7 tools
  mcp-flowgate/               binary: serve | check

schemas/
  gateway-config.schema.json
  workflow-response.schema.json

examples/
  simple-proxy.yaml           proxy mode, one cli + one mcp tool
  governed-change.yaml        full workflow with guards + human approval
  import-and-discovery.yaml   import block across native/npx/uvx/container/HTTP

docs/                         topical deep-dives
```

---

## Common commands

```bash
# Build the whole workspace.
cargo build --workspace

# Run every test (~57 across 9 binaries / suites).
cargo test --workspace

# Lint with all warnings denied.
cargo clippy --workspace --all-targets -- -D warnings

# Validate a config without serving.
cargo run -p mcp-flowgate -- check --config examples/simple-proxy.yaml

# Serve over stdio (logs go to stderr, MCP wire protocol on stdout).
cargo run -p mcp-flowgate -- serve --config examples/simple-proxy.yaml

# Tracing filter — defaults to info; everything goes to stderr.
RUST_LOG=mcp_flowgate=debug cargo run -p mcp-flowgate -- serve --config examples/simple-proxy.yaml
```

---

## Test layout

| File                                                           | What it covers                                                       |
|----------------------------------------------------------------|-----------------------------------------------------------------------|
| `crates/mcp-flowgate-core/tests/invariants.rs`                 | The 10 invariants + audit emission                                    |
| `crates/mcp-flowgate-core/tests/composability.rs`              | `capabilities:`, `wraps:`, `include:`, capability refs                |
| `crates/mcp-flowgate-core/tests/discovery.rs`                  | `gateway.search` / `describe` / `home`                                |
| `crates/mcp-flowgate-core/tests/capability.rs`                 | Capability registry + proxy compilation from registry                 |
| `crates/mcp-flowgate-core/tests/evidence_guard.rs`             | End-to-end evidence guard                                             |
| `crates/mcp-flowgate-core/tests/persistent_stores.rs`          | File + SQLite WorkflowStore round-trips                               |
| `crates/mcp-flowgate-core/tests/postgres_store.rs`             | Postgres WorkflowStore round-trips (requires `POSTGRES_TEST_URL`)     |
| `crates/mcp-flowgate-executors/tests/rest_executor.rs`         | REST executor (wiremock-driven)                                       |
| `crates/mcp-flowgate-executors/tests/human_audit.rs`           | Human executor's `human.approval.requested` event                     |
| `crates/mcp-flowgate-mcp-server/tests/stable_tool_surface.rs`  | Invariant 9 — tool list is exactly the documented seven               |

When adding a feature, mirror this taxonomy: one test file per topic,
fail-loud assertions, real backends where cheap (wiremock for HTTP,
tempfile for filesystem, in-memory SQLite for the DB).

---

### Postgres tests

The Postgres `WorkflowStore` integration tests require a running Postgres
instance. Set the `POSTGRES_TEST_URL` environment variable to point to it:

```bash
export POSTGRES_TEST_URL="postgres://postgres:postgres@localhost:5432/flowgate_test"
cargo test --test postgres_store
```

If the env var is not set, the tests print a skip message and pass
gracefully, so local development without Docker still works.

To spin up a temporary Postgres for testing:

```bash
docker run -d --name flowgate-pg \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_DB=flowgate_test \
  -p 5432:5432 \
  postgres:16

export POSTGRES_TEST_URL="postgres://postgres:postgres@localhost:5432/flowgate_test"
cargo test --test postgres_store

docker stop flowgate-pg && docker rm flowgate-pg
```

---

## Schema regeneration

`mcp-flowgate-schema/build.rs` reads `schemas/*.json` and emits Rust
types via [typify](https://github.com/oxidecomputer/typify). Edits to
the schemas trigger a rebuild of the schema crate automatically.

If you change a schema, you'll usually also need to:

1. Update `mcp-flowgate-core/src/config.rs` if the field affects
   resolution (capabilities, wraps, include, etc.).
2. Update `crates/mcp-flowgate-core/src/runtime.rs` or downstream
   consumers if it changes runtime behavior.
3. Add an example in `examples/`.
4. Update the relevant `/docs/*.md`.

---

## Status

What's implemented:

- Two link layers (HATEOAS-inspired): `gateway.{home,search,describe}`
  for discovery and `workflow.{start,get,submit,explain}` for action.
  Stable seven-tool surface.
- Configurable proxy and multi-state governed workflows (one engine).
- Connection runtimes: `mcp` over child process or Streamable HTTP URL,
  `cli` for any process, `rest` for any HTTP endpoint.
- Vendor-neutral `proxy.import`: connect to any MCP server and import
  its `tools/list` as proxy capabilities.
- Reliability per executor invocation: timeout, retry (none / fixed /
  exponential), fallback executors with `first_success`.
- Audit taxonomy with stdout / file / memory / null sinks.
- Evidence store backing the `evidence` guard.
- Persistent `WorkflowStore`: in-memory, file-backed, SQLite.
- Lexical `DiscoveryIndex` over workflows / capabilities / connections.
- Composability: named `capabilities:`, capability references in
  exposures and workflow executors, `wraps:` for stacking policy,
  `include:` for multi-file config composition.

Trait seams left for future work — implement the trait and drop in:

- Distributed `WorkflowStore` (Postgres, Redis, …)
- Vector / hybrid `DiscoveryIndex` (Tantivy, embeddings)
- Persistent `EvidenceStore` (file / DB-backed)
- Postgres / Kafka / OTel `AuditSink`
- Domain-specific `Executor` and `GuardEvaluator` impls

---

## Where to next

- The runtime contract: [INVARIANTS.md](INVARIANTS.md)
- Embedding the crates as a library: [EMBEDDING.md](EMBEDDING.md)
- Composing for larger systems: [MCP-CONTROL-ARCHITECTURE.md](MCP-CONTROL-ARCHITECTURE.md)
