# Zed Editor Gateway

A complete mcp-flowgate configuration that gates all AI tool access
in [Zed](https://zed.dev) behind a seven-tool governed surface.

## What this does

When you connect Zed to this gateway (and **only** this gateway), the AI
assistant sees exactly seven tools. Every downstream capability — reading
files, running tests, creating PRs — is a link in a response payload,
not a separate tool.

| Action | How |
|--------|-----|
| Read files | `gateway.search` → `workflow.start(proxy_default)` → follow `fs.read` link |
| Run tests | Same, follow `test.run` link |
| Start a TDD cycle | `workflow.start(tdd)` → follow red → green → refactor links |
| Start a governed PR | `workflow.start(governed_change)` → plan → test → human approval |

The model **cannot**: write files without tests passing, create a PR
without tests, run arbitrary shell commands, skip TDD phases, or merge
without human approval.

## Setup

### Prerequisites

- [Zed editor](https://zed.dev)
- Rust toolchain (for building `mcp-flowgate`)
- Node.js (for `npx`-based MCP servers)

### 1. Build and validate

```bash
cargo build --release -p mcp-flowgate
mcp-flowgate check --config examples/zed-gateway/gateway.yaml
```

### 2. Configure Zed

Edit `~/.config/zed/settings.json`:

```json
{
  "context_servers": [
    {
      "id": "flowgate",
      "executable": "/absolute/path/to/mcp-flowgate",
      "args": ["serve", "--config", "/absolute/path/to/examples/zed-gateway/gateway.yaml"]
    }
  ]
}
```

### 3. Verify

Open Zed and ask: *"What tools do you have available?"* — should list
exactly seven tools starting with `gateway.` and `workflow.`.

Or run the automated check:

```bash
bash examples/zed-gateway/verify.sh
```

## Hardening

**This only works if flowgate is your ONLY MCP source in Zed.** If you
also configure a raw `github-mcp-server` or `filesystem` server, the
model gets those tools directly and routes around governance.

Verify: `~/.config/zed/settings.json` should have exactly one
`context_servers` entry with `"id": "flowgate"`.

## Audit trail

```bash
# View all events
cat ~/.local/share/flowgate/audit.jsonl | jq .

# Tail in real time
mcp-flowgate audit tail --config examples/zed-gateway/gateway.yaml

# Filter for approval requests
cat ~/.local/share/flowgate/audit.jsonl | jq 'select(.event_type == "human.approval.requested")'
```

## See also

- [Config reference](../../docs/CONFIG.md)
- [Governance knobs](../../docs/GOVERNANCE.md)
- [TDD example](../tdd/)
- [Content publishing example](../content-publish/)
