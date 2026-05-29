# Cognitive-Architecture Benchmark — Methodology + Scaffold

**Status: scaffold ready; runs require API budget ($50–$200).**

The thesis (see `RESEARCH.md`): a deterministic architecture +
cheap models can match or exceed a single frontier model run on
the same task, at a fraction of the cost. This benchmark spike
provides the empirical evidence operators need to validate the
claim for their specific workflows.

This document is the design + cost-estimate. The actual benchmark
runs require an operator with API budget and time; the codebase
ships the harness scaffolding (`crates/mcp-flowgate-core/benches/cognitive_architecture_spike.rs`),
this methodology, and a clear runbook.

---

## Hypothesis

For SWE tasks of moderate complexity (10-50 LOC change with
verification), an mcp-flowgate-driven multi-agent architecture
using Haiku 4.5 for sub-agents:

- Matches a single Opus 4.7 run on **correctness** (task completion + tests green)
- Beats the single-Opus run on **wall-clock** (parallelism)
- Beats the single-Opus run on **token cost** (cheaper models per step)
- Beats the single-Opus run on **observability** (every transition audited)

The null hypothesis: the deterministic architecture imposes enough
overhead (workflow round-trips, sub-agent spawn latency) that any
savings are erased.

---

## Methodology

### Task corpus

Three tiers of SWE tasks, ten per tier:

| Tier | Description | Source |
|------|-------------|--------|
| Easy | Single-file bug fix, no cross-cutting concerns | SWE-bench Lite (filter for &lt;3 changed files) |
| Medium | Multi-file refactor, requires test understanding | SWE-bench Verified |
| Hard | Architecture change, multiple subsystems | SWE-bench Verified hard tier |

For each task: clean checkout, deterministic test command, known-good fix.

### Agent configurations under test

| Configuration | Model | Architecture |
|---------------|-------|--------------|
| **opus_only** | Opus 4.7 (single agent) | Aether default (no Flowgate) |
| **opus_governed** | Opus 4.7 (single agent) | Aether + Flowgate workflow |
| **haiku_sub_agents** | Sonnet 4.6 (planner) + Haiku 4.5 (executor, critic) | Flowgate workflow with `delegate:` sub-agents per role |
| **mixed_ensemble** | Sonnet 4.6 (planner, critic) + Haiku 4.5 (executor x 3 parallel) | Flowgate `parallel:` fan-out + critic synthesis |

### Run protocol

- 10 runs per (configuration, task) pair = 1200 total runs.
- Each run: clean repo state, fresh workflow instance, hard timeout (30 min).
- Metrics captured per run:
  - `correctness`: 0/1 (tests green after the agent declares done)
  - `wall_clock_seconds`: from workflow.start to terminal state
  - `tokens_in`, `tokens_out`: summed across all model calls in the run
  - `dollars`: tokens × model pricing as of run date
  - `tool_calls`: count of MCP tool invocations
  - `transitions`: count of workflow state transitions (governance footprint)
  - `evidence_records`: count of evidence emitted to the blackboard

### Statistical analysis

- Mean + 95% CI per configuration per tier.
- Pairwise win/loss on each metric.
- Cost-per-success: dollars / correctness, per configuration per tier.
- Identify the configuration that minimizes cost-per-success at each tier.

---

## Cost estimate

| Config | Est. tokens/run | $/run (rough) | 30 runs |
|---|---|---|---|
| opus_only | 80k in + 20k out | $4 | $120 |
| opus_governed | 90k in + 20k out | $4.30 | $130 |
| haiku_sub_agents | 60k in + 20k out (mostly Haiku) | $0.30 | $9 |
| mixed_ensemble | 100k in + 30k out (parallel Haiku) | $0.60 | $18 |

**Total worst case: ~$280** across all 1200 runs. Add ~$50 for retries,
debugging runs, and harness iteration. Realistic budget: **$300–$500**.

Run-time estimate: ~10 minutes per run × 1200 runs = 200 hours.
Parallelizable down to ~12 hours wall-clock with 16 concurrent runners.

---

## Runbook

### Prerequisites

- `ANTHROPIC_API_KEY` (or per-provider equivalents) in env
- `mcp-flowgate` binary built and on PATH
- `flowgate-agent` binary built and on PATH (TUI walker)
- Clean copy of each SWE-bench task in `benches/corpus/<task_id>/`
- Disk: ~10 GB for corpus + intermediate state

### Steps

1. **Configure**: copy `crates/mcp-flowgate-core/benches/cognitive_architecture_spike.config.example.yaml`
   to `~/.config/flowgate-bench/config.yaml`, fill in API keys + concurrency limit.
2. **Sanity check**: run a single warmup (the harness has a `--smoke` mode).
3. **Bake**: `cargo bench -p mcp-flowgate-core --bench cognitive_architecture_spike -- --bake`
   runs all 1200 trials, writes structured JSONL to `bench-results/`.
4. **Analyze**: `python3 crates/mcp-flowgate-core/benches/analyze.py
   bench-results/*.jsonl > report.md` produces the comparison report.
5. **Publish**: drop `report.md` into `docs/COGNITIVE-ARCHITECTURE-RESULTS.md`
   and link from README's "Why this design" section.

### Tripwires

- Cost overrun: harness aborts if dollars-spent exceeds `bench.max_dollars` (set in config).
- Wall-clock overrun: harness aborts run after 30-min per-task timeout.
- Provider rate-limit: harness backs off + retries; failures past `bench.max_retries`
  are recorded as `error: rate_limited` and excluded from correctness statistics.

---

## What this benchmark does NOT measure

- **Agent helpfulness on novel tasks** — SWE-bench tasks have known-good
  fixes; the agent's ability to find a fix on a never-before-seen problem
  is not measured. (That's a different, harder benchmark.)
- **Tool-use sophistication** — the corpus is fix-existing-bug tasks;
  tasks requiring novel tool composition are out of scope.
- **Sustained operation** — single-task runs only; behaviour over a
  multi-day session with context drift is a separate study.

These are noted explicitly so the report's claims can be precise about
what the data does and doesn't support.

---

## Why this matters

The mcp-flowgate architecture claims (RESEARCH.md): governance and
multi-model orchestration aren't a tax on capability — they're a
multiplier on cost-effectiveness. That claim deserves data, not just
narrative. This benchmark, when run, replaces "we believe this works"
with "here are the numbers."

Until run, the project's positioning stays at "compelling design with
solid engineering" rather than "production-ready with measured wins."
Both states are valid; the benchmark moves the needle.
