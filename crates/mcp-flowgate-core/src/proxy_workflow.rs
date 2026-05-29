use serde_json::{json, Value};

use crate::capability::CapabilityRegistry;

pub const DEFAULT_PROXY_WORKFLOW_ID: &str = "proxy_default";
pub const DEFAULT_PROXY_STATE: &str = "ready";

/// Compile a `proxy.expose: [...]` block into a single null-op workflow.
/// Each exposure becomes one transition `ready -> ready`.
///
/// Returns `None` when the config has no `proxy.expose` array.
pub fn compile_proxy_workflow(config: &Value) -> Option<Value> {
    let exposures = config.pointer("/proxy/expose")?.as_array()?;
    compile_proxy_workflow_from_exposures(exposures)
}

/// Compile from a `CapabilityRegistry` instead of raw config. Lets imported
/// tools (from `proxy.import`) participate in `proxy_default` alongside
/// declared exposures.
pub fn compile_proxy_workflow_from_registry(registry: &CapabilityRegistry) -> Option<Value> {
    if registry.is_empty() {
        return None;
    }
    let exposures = registry.as_proxy_exposures();
    compile_proxy_workflow_from_exposures(&exposures)
}

fn compile_proxy_workflow_from_exposures(exposures: &[Value]) -> Option<Value> {
    let mut transitions = serde_json::Map::new();
    for exposure in exposures {
        let Some(name) = exposure.get("name").and_then(Value::as_str) else {
            continue;
        };

        let mut t = serde_json::Map::new();
        t.insert(
            "title".into(),
            exposure
                .get("title")
                .cloned()
                .unwrap_or_else(|| json!(name)),
        );
        if let Some(d) = exposure.get("description") {
            t.insert("description".into(), d.clone());
        }
        t.insert("target".into(), json!(DEFAULT_PROXY_STATE));
        t.insert("actor".into(), json!("agent"));
        t.insert(
            "inputSchema".into(),
            exposure
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(empty_object_schema),
        );
        t.insert(
            "guards".into(),
            exposure.get("guards").cloned().unwrap_or_else(|| json!([])),
        );
        t.insert(
            "executor".into(),
            exposure
                .get("executor")
                .cloned()
                .unwrap_or_else(|| json!({ "kind": "noop" })),
        );
        if let Some(rel) = exposure.get("reliability") {
            t.insert("reliability".into(), rel.clone());
        }
        t.insert("output".into(), json!({ "lastResult": "$.output" }));

        transitions.insert(name.to_string(), Value::Object(t));
    }

    Some(json!({
        "version": "0",
        "description": "Generated null-op workflow for configurable proxy exposures.",
        "initialState": DEFAULT_PROXY_STATE,
        "states": {
            DEFAULT_PROXY_STATE: {
                "description": "Proxy-ready state. All transitions return to this state.",
                "transitions": transitions
            }
        }
    }))
}

fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}
