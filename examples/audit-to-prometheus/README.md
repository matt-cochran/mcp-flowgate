# Audit-to-Prometheus

This example shows how to convert mcp-flowgate's structured audit
events into Prometheus metrics for monitoring and alerting.

## How it works

1. mcp-flowgate writes one JSON line per audit event to a file
   (`audit.sink: file`).
2. The Python converter script (`convert.py`) reads the JSONL file and
   emits Prometheus exposition-format metrics.
3. Prometheus scrapes the converter's output (via a push or scrape
   endpoint), or you pipe it through `prometheus-pushgateway`.
4. Visualize with the included Grafana dashboard.

## Quick start

```bash
# 1. Start mcp-flowgate with file audit
cargo run -p mcp-flowgate -- serve --config examples/simple-proxy.yaml &
FLOWGATE_PID=$!

# 2. Generate some audit events (make calls to the gateway)
# ...

# 3. Convert audit events to Prometheus metrics
cat /var/log/flowgate-audit.jsonl | python3 examples/audit-to-prometheus/convert.py

# 4. Pipe to pushgateway for Prometheus scraping
cat /var/log/flowgate-audit.jsonl | python3 examples/audit-to-prometheus/convert.py | \
  curl --data-binary @- http://pushgateway:9091/metrics/job/mcp-flowgate

# 5. Import the Grafana dashboard
# In Grafana: Create → Import → paste grafana-dashboard.json

# 6. Stop
kill $FLOWGATE_PID
```

## Metrics emitted

| Metric | Type | Labels |
|--------|------|--------|
| `mcp_flowgate_audit_events_total` | Counter | `event_type` |
| `mcp_flowgate_audit_workflows_active` | Gauge | (none) |
| `mcp_flowgate_audit_workflow_state_total` | Counter | `state` |
| `mcp_flowgate_audit_executor_total` | Counter | `event`, `kind` |
| `mcp_flowgate_audit_guard_total` | Counter | `guard_kind`, `result` |
| `mcp_flowgate_audit_last_scrape_timestamp` | Gauge | (none) |

## Alternative: mtail

If you prefer [mtail](https://github.com/google/mtail) over Python,
you can write an mtail program that parses the same JSONL format.
The Python converter is provided as a reference — it requires no
external dependencies beyond the Python standard library.