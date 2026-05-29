//! SPEC §7 — per-orchestrator slot table.
//!
//! The host orchestrator's `$.context.*` slots are typed. Building this
//! table is what powers V13 (every `use:.inputs` RHS path must resolve
//! to a slot that's either declared in `inputs:` or written by some
//! state's `use:.outputs`) and V14 (two states writing the same host
//! path must declare structurally identical output types).
//!
//! Construction is **flat** — no topological walk, no inference. Spec
//! §7.4 explicitly says state-graph cycles do not participate in type
//! inference; a slot's type is decided at its declared write site.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::validate::Diagnostic;

/// Where a slot's type came from. Drives error messages so an operator
/// can navigate straight to the offending declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotSource {
    /// Declared on the orchestrator's top-level `inputs:` block.
    Input,
    /// Written by a state's transition via `use:.outputs`. The string
    /// is the state name. (Use it in error messages to point at the
    /// declaration site.)
    State(String),
}

/// A single entry in the slot table.
#[derive(Debug, Clone)]
pub struct SlotEntry {
    pub schema: Value,
    pub source: SlotSource,
}

/// The per-orchestrator slot table. Keyed by host path
/// (`$.context.<name>`); each entry carries the declared schema and the
/// declaration site.
///
/// **Storage:** [`BTreeMap`] so iteration order is deterministic for
/// stable error-message output (helps test snapshots stay reproducible).
#[derive(Debug, Clone, Default)]
pub struct SlotTable {
    entries: BTreeMap<String, SlotEntry>,
}

impl SlotTable {
    /// Returns `Some(entry)` iff `host_path` (e.g. `$.context.verdict`)
    /// is declared. Used by V13's Check A.
    pub fn get(&self, host_path: &str) -> Option<&SlotEntry> {
        self.entries.get(host_path)
    }

    /// Number of declared slots.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Useful for snapshot tests.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate slot entries in deterministic (lexicographic by host path)
    /// order. Surfaces stable error-message output.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &SlotEntry)> {
        self.entries.iter()
    }
}

/// SPEC §7.2 — build the slot table for one orchestrator.
///
/// - Seeds entries from the orchestrator's top-level `inputs:` block,
///   keyed `$.context.<input_name>`.
/// - Walks every state. For each transition whose executor has a
///   `use:.outputs` block, contributes one entry per `host_path → cap_output`
///   pair. The schema is harvested from the target capability's
///   `snippet.outputs[cap_output]` via `cap_snippet_outputs`.
///
/// Returns the table on success, or a [`Vec<Diagnostic>`] of V14 (type
/// consistency) errors when two states write incompatible types to the
/// same host slot. V13 (reachability) is checked separately via
/// [`assert_reachable`] — that has different "caller knows the host_path"
/// semantics, so we keep the checks composable.
pub fn build_slot_table(
    orchestrator_def: &Value,
    cap_snippet_outputs: &HashMap<String, Value>,
) -> Result<SlotTable, Vec<Diagnostic>> {
    let mut table = SlotTable::default();
    let mut errors: Vec<Diagnostic> = Vec::new();

    // Seed from inputs:.
    if let Some(inputs) = orchestrator_def
        .pointer("/inputs")
        .and_then(Value::as_object)
    {
        for (name, schema) in inputs {
            let host_path = format!("$.context.{name}");
            table.entries.insert(
                host_path,
                SlotEntry {
                    schema: schema.clone(),
                    source: SlotSource::Input,
                },
            );
        }
    }

    // Walk states → transitions → executor.use.outputs.
    if let Some(states) = orchestrator_def
        .pointer("/states")
        .and_then(Value::as_object)
    {
        for (state_name, state_def) in states {
            let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object)
            else {
                continue;
            };
            for (_t_name, t_def) in transitions {
                let Some(exec) = t_def.pointer("/executor").and_then(Value::as_object) else {
                    continue;
                };
                if exec.get("kind").and_then(Value::as_str) != Some("workflow") {
                    continue;
                }
                let Some(use_outputs) = exec
                    .get("use")
                    .and_then(|u| u.get("outputs"))
                    .and_then(Value::as_object)
                else {
                    continue;
                };
                let target_id = exec
                    .get("definitionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let snippet = cap_snippet_outputs.get(target_id);
                for (host_path, cap_name_value) in use_outputs {
                    let Some(cap_name) = cap_name_value.as_str() else {
                        continue;
                    };
                    let schema = snippet
                        .and_then(|s| s.get(cap_name))
                        .cloned()
                        .unwrap_or(Value::Null);
                    insert_with_v14_check(
                        &mut table,
                        &mut errors,
                        host_path.clone(),
                        SlotEntry {
                            schema,
                            source: SlotSource::State(state_name.clone()),
                        },
                    );
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(table)
    } else {
        Err(errors)
    }
}

/// Insert into the slot table; on a host-path collision, structurally
/// compare schemas. Equal → keep the existing entry (first writer wins
/// on declaration order, deterministic per [`BTreeMap`] iteration).
/// Different → push a V14 diagnostic naming both states.
fn insert_with_v14_check(
    table: &mut SlotTable,
    errors: &mut Vec<Diagnostic>,
    host_path: String,
    new_entry: SlotEntry,
) {
    if let Some(existing) = table.entries.get(&host_path) {
        if schemas_equal(&existing.schema, &new_entry.schema) {
            return;
        }
        errors.push(Diagnostic::Error(format!(
            "SLOT_TYPE_CONFLICT: '{host_path}' is written by {} and {} with structurally \
             different schemas (SPEC §7.3, V14)",
            describe_source(&existing.source),
            describe_source(&new_entry.source)
        )));
        return;
    }
    table.entries.insert(host_path, new_entry);
}

/// Structural equality on canonical JSON. Reuses
/// [`crate::contract_hash::canonical_json_string`] to compare
/// sorted-key-canonicalized output — same algorithm operators see when
/// pinning a contract hash, so the equality intuition stays consistent.
fn schemas_equal(a: &Value, b: &Value) -> bool {
    use crate::contract_hash::canonical_json_string;
    canonical_json_string(a) == canonical_json_string(b)
}

fn describe_source(s: &SlotSource) -> String {
    match s {
        SlotSource::Input => "the orchestrator's `inputs:` block".to_string(),
        SlotSource::State(name) => format!("state '{name}'"),
    }
}

/// SPEC §7.3, V13 (Check A) — a single reachability check. Caller passes
/// the slot table and a `use:.inputs` host path; we emit a structured
/// diagnostic if it's not in the table.
pub fn assert_reachable(
    table: &SlotTable,
    host_path: &str,
    orchestrator_id: &str,
    state_name: &str,
    transition_name: &str,
) -> Option<Diagnostic> {
    if table.entries.contains_key(host_path) {
        None
    } else {
        Some(Diagnostic::Error(format!(
            "UNREACHABLE_SLOT: orchestrator '{orchestrator_id}' state '{state_name}' \
             transition '{transition_name}' references '{host_path}' via `use:.inputs`, \
             but no state writes that slot and it is not declared in `inputs:` \
             (SPEC §7.3, V13)"
        )))
    }
}

/// SPEC §7.3, V14 (Check B) entry point — assert that a slot's existing
/// declared schema matches `expected_schema`. Currently a thin wrapper
/// over [`schemas_equal`]; exposed as its own function so future
/// per-rule callers (e.g. PR3's runtime check, late-bound types) keep
/// the same comparison semantics.
pub fn assert_type_consistent(
    table: &SlotTable,
    host_path: &str,
    expected_schema: &Value,
) -> Option<Diagnostic> {
    let Some(entry) = table.entries.get(host_path) else {
        return None; // not declared — reachability is V13, not V14
    };
    if schemas_equal(&entry.schema, expected_schema) {
        None
    } else {
        Some(Diagnostic::Error(format!(
            "SLOT_TYPE_MISMATCH: '{host_path}' is declared with one schema by {} but \
             referenced with a different one (SPEC §7.3, V14)",
            describe_source(&entry.source)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cap_outputs() -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(
            "cap.plan.vet".to_string(),
            json!({ "verdict": { "type": "string", "enum": ["pass", "fail"] } }),
        );
        m
    }

    #[test]
    fn build_seeds_from_inputs_block() {
        let orch = json!({
            "inputs": {
                "feature_brief": { "type": "string" }
            },
            "states": {}
        });
        let t = build_slot_table(&orch, &HashMap::new()).expect("no errors");
        let entry = t.get("$.context.feature_brief").expect("present");
        assert!(matches!(entry.source, SlotSource::Input));
    }

    #[test]
    fn build_harvests_use_outputs_from_states() {
        let orch = json!({
            "states": {
                "vetting": {
                    "transitions": {
                        "vet": {
                            "executor": {
                                "kind": "workflow",
                                "definitionId": "cap.plan.vet",
                                "use": {
                                    "outputs": { "$.context.verdict": "verdict" }
                                }
                            }
                        }
                    }
                }
            }
        });
        let caps = cap_outputs();
        let t = build_slot_table(&orch, &caps).expect("no errors");
        let entry = t.get("$.context.verdict").expect("present");
        assert!(matches!(&entry.source, SlotSource::State(s) if s == "vetting"));
        assert_eq!(
            entry.schema.pointer("/type").and_then(Value::as_str),
            Some("string")
        );
    }

    #[test]
    fn build_flags_v14_when_two_states_write_incompatible_types() {
        let mut caps = HashMap::new();
        caps.insert("cap.a".to_string(), json!({ "v": { "type": "string" } }));
        caps.insert("cap.b".to_string(), json!({ "v": { "type": "integer" } }));
        let orch = json!({
            "states": {
                "s1": {
                    "transitions": { "t1": { "executor": {
                        "kind": "workflow",
                        "definitionId": "cap.a",
                        "use": { "outputs": { "$.context.x": "v" } }
                    } } }
                },
                "s2": {
                    "transitions": { "t1": { "executor": {
                        "kind": "workflow",
                        "definitionId": "cap.b",
                        "use": { "outputs": { "$.context.x": "v" } }
                    } } }
                }
            }
        });
        let err = build_slot_table(&orch, &caps).expect_err("V14 should fire");
        assert!(
            err.iter()
                .any(|d| d.message().contains("SLOT_TYPE_CONFLICT")),
            "{err:?}"
        );
    }

    #[test]
    fn assert_reachable_returns_none_for_declared_slot() {
        let orch = json!({
            "inputs": { "x": { "type": "string" } },
            "states": {}
        });
        let t = build_slot_table(&orch, &HashMap::new()).unwrap();
        assert!(assert_reachable(&t, "$.context.x", "flow", "s", "t").is_none());
    }

    #[test]
    fn assert_reachable_returns_diagnostic_for_undeclared_slot() {
        let t = SlotTable::default();
        let d = assert_reachable(&t, "$.context.missing", "flow", "s", "t").expect("must emit");
        assert!(d.message().contains("UNREACHABLE_SLOT"));
        assert!(d.message().contains("$.context.missing"));
    }
}
