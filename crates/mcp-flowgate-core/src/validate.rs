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

    let skill_subjects: HashSet<&str> = config
        .pointer("/skills")
        .and_then(Value::as_object)
        .map(|m| m.keys().map(String::as_str).collect())
        .unwrap_or_default();

    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return diagnostics;
    };

    for (id, def) in workflows {
        validate_one_workflow(id, def, &skill_subjects, &mut diagnostics);
    }

    diagnostics
}

fn validate_one_workflow(
    id: &str,
    def: &Value,
    skill_subjects: &HashSet<&str>,
    out: &mut Vec<Diagnostic>,
) {
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

    // Blackboard slot check: if blackboard is declared, warn on any output: key not in the set.
    if let Some(blackboard) = def.get("blackboard") {
        let declared: HashSet<&str> = match blackboard {
            Value::Array(arr) => arr
                .iter()
                .filter_map(|v| v.as_str())
                .collect(),
            Value::Object(obj) => obj.keys().map(String::as_str).collect(),
            _ => HashSet::new(),
        };

        for (state_name, state_def) in states {
            if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
                for (t_name, t_def) in ts {
                    if let Some(output) = t_def.get("output").and_then(Value::as_object) {
                        for key in output.keys() {
                            if !declared.contains(key.as_str()) {
                                out.push(Diagnostic::Warning(format!(
                                    "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                     writes output key '{key}' which is not declared in the blackboard"
                                )));
                            }
                        }
                    }
                }
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

    check_use_before_def(id, def, states, initial_state, out);
    check_skills_refs(id, def, states, skill_subjects, out);
}

/// Phase 6: SPEC §9, §11 — `$.context.X` referenced by an `expr` guard or
/// `{{ }}` template must have a reachable predecessor writer; `$.context.summary`
/// is never a valid guard input.
fn check_use_before_def(
    id: &str,
    def: &Value,
    states: &serde_json::Map<String, Value>,
    initial_state: &str,
    out: &mut Vec<Diagnostic>,
) {
    let writers = compute_writers_into(def, states, initial_state);

    for (state_name, state_def) in states {
        let available = writers.get(state_name.as_str()).cloned().unwrap_or_default();

        // Templates on the state (state.goal, state.guidance).
        for field in ["goal", "guidance"] {
            if let Some(text) = state_def.get(field).and_then(Value::as_str) {
                for slot in extract_template_context_slots(text) {
                    if slot == "summary" {
                        // summary is a model-authored content slot; reading it
                        // from a template is fine (it gets rendered). Only
                        // guards must not read it.
                        continue;
                    }
                    if !available.contains(slot.as_str()) {
                        out.push(Diagnostic::Warning(format!(
                            "workflow '{id}': state '{state_name}' template `{field}` reads `$.context.{slot}` \
                             which has no reachable writer (use-before-def)"
                        )));
                    }
                }
            }
        }

        // Guards on every outgoing transition (incl. branch `when` guards).
        if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
            for (t_name, t_def) in ts {
                let mut guards = collect_guards(t_def.get("guards"));
                if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                    for branch in branches {
                        if let Some(when) = branch.get("when") {
                            collect_guards_into(when, &mut guards);
                        }
                    }
                }
                for guard in guards {
                    let expr = match guard.get("expr").and_then(Value::as_str) {
                        Some(e) => e,
                        None => continue,
                    };
                    for slot in extract_expr_context_slots(expr) {
                        if slot == "summary" {
                            out.push(Diagnostic::Error(format!(
                                "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                 guard reads `$.context.summary` — model-authored summary is never \
                                 a valid guard input (SPEC §6.3)"
                            )));
                            continue;
                        }
                        if !available.contains(slot.as_str()) {
                            out.push(Diagnostic::Error(format!(
                                "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                 guard reads `$.context.{slot}` which has no reachable writer \
                                 (use-before-def, SPEC §11)"
                            )));
                        }
                    }
                }
            }
        }
    }
}

/// Build per-state writers_into via a fixed-point over the reachable subgraph.
/// `writers_into[S]` = union over every reachable path from initial to S of
/// the slots written by initialContext + every transition output: on that path.
fn compute_writers_into(
    def: &Value,
    states: &serde_json::Map<String, Value>,
    initial_state: &str,
) -> HashMap<String, HashSet<String>> {
    let mut writers: HashMap<String, HashSet<String>> = HashMap::new();

    // Seed: initialContext keys + any onEnter output on the initial state are
    // available before the first guard fires.
    let mut seed: HashSet<String> = def
        .get("initialContext")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    if let Some(state) = states.get(initial_state) {
        if let Some(on_enter_out) = state.pointer("/onEnter/output").and_then(Value::as_object) {
            seed.extend(on_enter_out.keys().cloned());
        }
    }
    writers.insert(initial_state.to_string(), seed);

    // Propagate to a fixed point. Worst case O(|states| * |transitions|),
    // bounded by tens-to-hundreds of states in practice — no need for a
    // worklist-style optimisation.
    let mut changed = true;
    while changed {
        changed = false;
        for (state_name, state_def) in states {
            let Some(state_writers) = writers.get(state_name).cloned() else {
                continue;
            };
            let Some(ts) = state_def.get("transitions").and_then(Value::as_object) else {
                continue;
            };
            for (_t_name, t_def) in ts {
                let mut produced = state_writers.clone();
                if let Some(output) = t_def.get("output").and_then(Value::as_object) {
                    produced.extend(output.keys().cloned());
                }
                let mut targets: Vec<&str> = Vec::new();
                if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                    targets.push(target);
                }
                if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                    for branch in branches {
                        if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                            targets.push(bt);
                        }
                    }
                }
                for target in targets {
                    let entry = writers.entry(target.to_string()).or_default();
                    let mut to_merge = produced.clone();
                    // Add this state's own onEnter output (visible to any
                    // guard leaving the target state).
                    if let Some(target_state) = states.get(target) {
                        if let Some(on_enter_out) = target_state
                            .pointer("/onEnter/output")
                            .and_then(Value::as_object)
                        {
                            to_merge.extend(on_enter_out.keys().cloned());
                        }
                    }
                    for key in to_merge {
                        if entry.insert(key) {
                            changed = true;
                        }
                    }
                }
            }
        }
    }
    writers
}

fn collect_guards(guards: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(arr) = guards.and_then(Value::as_array) {
        for g in arr {
            collect_guards_into(g, &mut out);
        }
    }
    out
}

fn collect_guards_into(guard: &Value, out: &mut Vec<Value>) {
    match guard.get("kind").and_then(Value::as_str) {
        Some("all_of") | Some("any_of") => {
            if let Some(inner) = guard.get("guards").and_then(Value::as_array) {
                for g in inner {
                    collect_guards_into(g, out);
                }
            }
        }
        Some("not") => {
            if let Some(inner) = guard.get("guard") {
                collect_guards_into(inner, out);
            }
        }
        _ => out.push(guard.clone()),
    }
}

/// Extract slot names from `$.context.X` paths inside an expression. Conservative
/// regex-free scan — collects identifier-shaped suffixes after each `$.context.`.
fn extract_expr_context_slots(expr: &str) -> Vec<String> {
    extract_context_slots_from(expr)
}

/// Extract slot names from `{{ $.context.X }}` templates in a string.
fn extract_template_context_slots(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find closing `}}`.
            if let Some(end) = find_subslice(&bytes[i + 2..], b"}}") {
                let inner = &text[i + 2..i + 2 + end];
                out.extend(extract_context_slots_from(inner));
                i += 2 + end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    for i in 0..=hay.len() - needle.len() {
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

fn extract_context_slots_from(text: &str) -> Vec<String> {
    const PREFIX: &str = "$.context.";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(idx) = rest.find(PREFIX) {
        let after = &rest[idx + PREFIX.len()..];
        let slot: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !slot.is_empty() {
            out.push(slot);
        }
        rest = &after[after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len())..];
    }
    out
}

/// Phase 6: SPEC §5.5, §11 — skills references resolve to a declared fragment;
/// more than ~4 refs at one scope warns.
fn check_skills_refs(
    id: &str,
    def: &Value,
    states: &serde_json::Map<String, Value>,
    skill_subjects: &HashSet<&str>,
    out: &mut Vec<Diagnostic>,
) {
    const REF_WARN_THRESHOLD: usize = 4;

    let mut check_scope = |scope: &str, refs: &Value| {
        let Some(arr) = refs.as_array() else { return };
        if arr.len() > REF_WARN_THRESHOLD {
            out.push(Diagnostic::Warning(format!(
                "workflow '{id}': {scope} surfaces {n} skills refs — the menu is itself payload, \
                 consider trimming to ≤{REF_WARN_THRESHOLD}",
                n = arr.len()
            )));
        }
        for entry in arr {
            let Some(subject) = entry.as_str() else { continue };
            if !skill_subjects.contains(subject) {
                out.push(Diagnostic::Error(format!(
                    "workflow '{id}': {scope} references skills entry '{subject}' \
                     which is not declared in the top-level `skills:` library (SPEC §11)"
                )));
            }
        }
    };

    if let Some(refs) = def.get("skills") {
        check_scope("workflow scope", refs);
    }
    for (state_name, state_def) in states {
        if let Some(refs) = state_def.get("skills") {
            check_scope(&format!("state '{state_name}'"), refs);
        }
        if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
            for (t_name, t_def) in ts {
                if let Some(refs) = t_def.get("skills") {
                    check_scope(&format!("transition '{t_name}' in state '{state_name}'"), refs);
                }
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
