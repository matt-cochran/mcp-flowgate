use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diagnostic {
    Error(String),
    Warning(String),
}

impl Diagnostic {
    pub fn is_error(&self) -> bool {
        matches!(self, Diagnostic::Error(_))
    }

    pub fn message(&self) -> &str {
        match self {
            Diagnostic::Error(m) | Diagnostic::Warning(m) => m,
        }
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Diagnostic::Error(m) => write!(f, "error: {m}"),
            Diagnostic::Warning(m) => write!(f, "warning: {m}"),
        }
    }
}

pub fn validate_workflows(config: &Value) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return diagnostics;
    };

    for (id, def) in workflows {
        validate_one_workflow(id, def, &mut diagnostics);
    }

    diagnostics
}

fn validate_one_workflow(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(initial_state) = def.get("initialState").and_then(Value::as_str) else {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': missing 'initialState'"
        )));
        return;
    };

    let Some(states) = def.get("states").and_then(Value::as_object) else {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': missing 'states' map"
        )));
        return;
    };

    let state_names: BTreeSet<&str> = states.keys().map(String::as_str).collect();

    if !state_names.contains(initial_state) {
        out.push(Diagnostic::Error(format!(
            "workflow '{id}': initialState '{initial_state}' is not in states"
        )));
    }

    if let Some(timeout_target) = def.pointer("/onTimeout/target").and_then(Value::as_str) {
        if !state_names.contains(timeout_target) {
            out.push(Diagnostic::Error(format!(
                "workflow '{id}': onTimeout.target '{timeout_target}' is not in states"
            )));
        }
    }

    let mut transition_targets: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();

    for (state_name, state_def) in states {
        let is_terminal = state_def
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let transitions = state_def.get("transitions").and_then(Value::as_object);

        if !is_terminal && transitions.is_none_or(|t| t.is_empty()) {
            let has_on_timeout = def.pointer("/onTimeout/target").is_some();
            if !has_on_timeout {
                out.push(Diagnostic::Warning(format!(
                    "workflow '{id}': state '{state_name}' is non-terminal with no outgoing transitions"
                )));
            }
        }

        if let Some(ts) = transitions {
            for (t_name, t_def) in ts {
                if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                    if !state_names.contains(target) {
                        out.push(Diagnostic::Error(format!(
                            "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                             targets '{target}' which is not in states"
                        )));
                    }
                    transition_targets
                        .entry(target)
                        .or_default()
                        .push((state_name, t_name));
                } else {
                    out.push(Diagnostic::Error(format!(
                        "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                         is missing 'target'"
                    )));
                }

                if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                    for (idx, branch) in branches.iter().enumerate() {
                        if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                            if !state_names.contains(bt) {
                                out.push(Diagnostic::Error(format!(
                                    "workflow '{id}': branch {idx} of transition '{t_name}' \
                                     in state '{state_name}' targets '{bt}' which is not in states"
                                )));
                            }
                        }
                    }
                }
            }
        }

        if let Some(on_enter) = state_def.get("onEnter") {
            if on_enter.get("executor").is_none() {
                out.push(Diagnostic::Warning(format!(
                    "workflow '{id}': state '{state_name}' has onEnter but no executor"
                )));
            }
        }
    }

    // Reachability: BFS from initialState
    if state_names.contains(initial_state) {
        let mut reachable = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(initial_state);
        reachable.insert(initial_state);

        while let Some(current) = queue.pop_front() {
            if let Some(state_def) = states.get(current) {
                if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
                    for (_t_name, t_def) in ts {
                        if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                            if state_names.contains(target) && reachable.insert(target) {
                                queue.push_back(target);
                            }
                        }
                        if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                            for branch in branches {
                                if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                                    if state_names.contains(bt) && reachable.insert(bt) {
                                        queue.push_back(bt);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(timeout_target) = def.pointer("/onTimeout/target").and_then(Value::as_str) {
            if state_names.contains(timeout_target) {
                reachable.insert(timeout_target);
            }
        }

        for state_name in &state_names {
            if !reachable.contains(state_name) {
                out.push(Diagnostic::Warning(format!(
                    "workflow '{id}': state '{state_name}' is unreachable from initialState '{initial_state}'"
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_workflow_produces_no_diagnostics() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "done" }
                            }
                        },
                        "done": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d.is_empty(), "expected no diagnostics, got: {d:?}");
    }

    #[test]
    fn missing_initial_state_in_states() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "nonexistent",
                    "states": {
                        "start": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d
            .iter()
            .any(|d| d.is_error() && d.message().contains("nonexistent")));
    }

    #[test]
    fn dangling_transition_target() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "nowhere" }
                            }
                        }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d
            .iter()
            .any(|d| d.is_error() && d.message().contains("nowhere")));
    }

    #[test]
    fn dangling_branch_target() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": {
                                    "target": "done",
                                    "branches": [
                                        { "when": { "kind": "expr", "expr": "1 == 1" }, "target": "ghost" }
                                    ]
                                }
                            }
                        },
                        "done": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d
            .iter()
            .any(|d| d.is_error() && d.message().contains("ghost")));
    }

    #[test]
    fn unreachable_state_warned() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "done" }
                            }
                        },
                        "done": { "terminal": true },
                        "orphan": {
                            "transitions": {
                                "x": { "target": "done" }
                            }
                        }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d
            .iter()
            .any(|d| !d.is_error() && d.message().contains("orphan")));
    }

    #[test]
    fn dead_end_non_terminal_warned() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "stuck" }
                            }
                        },
                        "stuck": {}
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d
            .iter()
            .any(|d| !d.is_error() && d.message().contains("stuck")));
    }

    #[test]
    fn dead_end_suppressed_when_timeout_exists() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "timeoutMs": 5000,
                    "onTimeout": { "target": "timed_out" },
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "target": "waiting" }
                            }
                        },
                        "waiting": {},
                        "timed_out": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        let dead_end_warnings: Vec<_> = d
            .iter()
            .filter(|d| !d.is_error() && d.message().contains("no outgoing transitions"))
            .collect();
        assert!(
            dead_end_warnings.is_empty(),
            "dead-end warning should be suppressed when onTimeout exists: {dead_end_warnings:?}"
        );
    }

    #[test]
    fn dangling_timeout_target() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "timeoutMs": 5000,
                    "onTimeout": { "target": "missing_timeout" },
                    "states": {
                        "start": { "terminal": true }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d
            .iter()
            .any(|d| d.is_error() && d.message().contains("missing_timeout")));
    }

    #[test]
    fn missing_transition_target_field() {
        let config = json!({
            "workflows": {
                "demo": {
                    "initialState": "start",
                    "states": {
                        "start": {
                            "transitions": {
                                "go": { "executor": { "kind": "noop" } }
                            }
                        }
                    }
                }
            }
        });
        let d = validate_workflows(&config);
        assert!(d
            .iter()
            .any(|d| d.is_error() && d.message().contains("missing 'target'")));
    }

    #[test]
    fn no_workflows_produces_no_diagnostics() {
        let config = json!({
            "version": "1.0.0",
            "proxy": { "expose": [] }
        });
        let d = validate_workflows(&config);
        assert!(d.is_empty());
    }
}
