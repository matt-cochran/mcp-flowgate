use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use serde_json::Value;

use crate::cap_verb::{CapVerb, CapVerbCategory, BLESSED_CAP_VERBS};
use crate::contract_hash::compute_contract_hash;
use crate::tier::Tier;

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

    // SPEC §6.2 — build the cross-workflow context once. The contract-hash
    // index lets V15/V16 compare an `expects_contract_hash:` value against
    // the loaded target capability without recomputing per call site.
    let cap_contract_hashes: HashMap<String, String> = workflows
        .iter()
        .filter(|(id, _)| matches!(Tier::from_id(id), Tier::Cap))
        .filter_map(|(id, def)| {
            def.get("snippet")
                .map(|s| (id.clone(), compute_contract_hash(s)))
        })
        .collect();
    let cap_lifecycles: HashMap<String, String> = workflows
        .iter()
        .filter(|(id, _)| matches!(Tier::from_id(id), Tier::Cap))
        .map(|(id, def)| {
            (
                id.clone(),
                def.get("lifecycle")
                    .and_then(Value::as_str)
                    .unwrap_or("experimental")
                    .to_string(),
            )
        })
        .collect();
    // Snippet outputs by cap id — drives slot-table type harvesting for
    // V13/V14.
    let cap_snippet_outputs: HashMap<String, Value> = workflows
        .iter()
        .filter(|(id, _)| matches!(Tier::from_id(id), Tier::Cap))
        .filter_map(|(id, def)| {
            def.pointer("/snippet/outputs")
                .cloned()
                .map(|outputs| (id.clone(), outputs))
        })
        .collect();
    let ctx = ValidationCtx {
        cap_contract_hashes: &cap_contract_hashes,
        cap_lifecycles: &cap_lifecycles,
        cap_snippet_outputs: &cap_snippet_outputs,
    };

    for (id, def) in workflows {
        validate_one_workflow(id, def, &skill_subjects, &ctx, &mut diagnostics);
    }

    diagnostics
}

/// Cross-workflow validation context. Lets per-rule helpers reach into
/// other workflows' contract hashes + lifecycle declarations without
/// re-walking the registry per call site.
struct ValidationCtx<'a> {
    cap_contract_hashes: &'a HashMap<String, String>,
    cap_lifecycles: &'a HashMap<String, String>,
    cap_snippet_outputs: &'a HashMap<String, Value>,
}

fn validate_one_workflow(
    id: &str,
    def: &Value,
    skill_subjects: &HashSet<&str>,
    ctx: &ValidationCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    // SPEC §28 — slot constraint shape validation. Catches typos like
    // unknown constraint kinds, malformed globs, empty allowlists at
    // load time so they don't surface as runtime
    // SLOT_CONSTRAINT_VIOLATED errors on the first transition.
    if let Err(e) = crate::slot_constraint::validate_constraints_in_definition(def) {
        out.push(Diagnostic::Error(format!("workflow '{id}': {e}")));
    }

    // SPEC §5.1, V3/V4/V5 — capability workflows MUST declare a typed
    // `snippet:` contract; orchestrators MUST NOT (V8).
    // The tier is determined by the unprefixed id stem (`cap.` vs `flow.`).
    let tier = Tier::from_id(id);
    match tier {
        Tier::Cap => {
            v1_verb_in_cloud(id, def, out);
            v2_id_matches_verb_name(id, def, out);
            validate_snippet(id, def, out); // V3/V4/V5
            v6_primary_executor_verb_shape(id, def, out);
            v10_capability_does_not_invoke_workflow(id, def, out);
        }
        Tier::Flow => {
            v7_id_matches_flow_pattern(id, def, out);
            v8_orchestrator_has_no_snippet(id, def, out);
            v9_orchestrator_has_no_verb(id, def, out);
            v11_orchestrator_does_not_invoke_orchestrator(id, def, out);
            // V13 reachability + V14 type consistency run against the
            // per-orchestrator slot table built in slot_table.rs.
            v13_v14_slot_table(id, def, ctx, out);
        }
        Tier::Other => {}
    }
    // SPEC §6.1, V12 — every `kind: workflow` executor inside this
    // workflow's transitions must conform to the use-binding contract.
    validate_use_bindings(id, def, out);
    // SPEC §6.2, V15/V16 — contract-hash pinning checks against the
    // pre-built `cap_contract_hashes` index (so we don't recompute per
    // call site).
    validate_contract_hash_pins(id, def, ctx, out);

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
            Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
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
///
/// When `blackboard:` is declared, an additional check fires: a guard or
/// template that reads `$.context.X` for X **not in the declared slots** is
/// an error on the read side — independent of whether a writer happens to
/// exist (a writer to the same undeclared slot triggers the separate output:
/// warn from §6.1). Without a declared blackboard this check is skipped so
/// blackboard remains opt-in (SPEC §14 compatibility).
fn check_use_before_def(
    id: &str,
    def: &Value,
    states: &serde_json::Map<String, Value>,
    initial_state: &str,
    out: &mut Vec<Diagnostic>,
) {
    let writers = compute_writers_into(def, states, initial_state);
    let declared = declared_blackboard_slots(def);

    for (state_name, state_def) in states {
        let available = writers
            .get(state_name.as_str())
            .cloned()
            .unwrap_or_default();

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
                    if let Some(declared) = &declared {
                        if !declared.contains(slot.as_str()) {
                            out.push(Diagnostic::Error(format!(
                                "workflow '{id}': state '{state_name}' template `{field}` reads \
                                 `$.context.{slot}` which is not a declared blackboard slot \
                                 (SPEC §11)"
                            )));
                            // Slot isn't declared — the use-before-def check
                            // is moot. Skip to the next slot.
                            continue;
                        }
                    }
                    if !available.contains(slot.as_str()) {
                        out.push(Diagnostic::Error(format!(
                            "workflow '{id}': state '{state_name}' template `{field}` reads `$.context.{slot}` \
                             which has no reachable writer (use-before-def, SPEC §11). \
                             Runtime will render a stub but this is a likely authoring bug."
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
                        if let Some(declared) = &declared {
                            if !declared.contains(slot.as_str()) {
                                out.push(Diagnostic::Error(format!(
                                    "workflow '{id}': transition '{t_name}' in state '{state_name}' \
                                     guard reads `$.context.{slot}` which is not a declared \
                                     blackboard slot (SPEC §11)"
                                )));
                                continue;
                            }
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

/// Extract declared blackboard slot names from a workflow def. Returns
/// `Some(set)` only when `blackboard:` is present — `None` means "no
/// declaration; skip the read-side declared-slot check entirely" so configs
/// without a blackboard remain compatible (SPEC §14).
fn declared_blackboard_slots(def: &Value) -> Option<HashSet<String>> {
    let bb = def.get("blackboard")?;
    let set: HashSet<String> = match bb {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Value::Object(obj) => obj.keys().cloned().collect(),
        _ => return None,
    };
    Some(set)
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
    let timeout_target = def
        .pointer("/onTimeout/target")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut changed = true;
    while changed {
        changed = false;
        for (state_name, state_def) in states {
            let Some(state_writers) = writers.get(state_name).cloned() else {
                continue;
            };

            // SPEC §9: onTimeout is reachable from EVERY state. Whatever the
            // current state has accumulated, the timeout target can see — so
            // we propagate state_writers (plus the target's onEnter output)
            // into the timeout target as if from every reachable state.
            if let Some(target) = &timeout_target {
                let entry = writers.entry(target.clone()).or_default();
                let mut to_merge = state_writers.clone();
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

    // SPEC §9 — a workflow loaded via a `repos:` manifest has an id of
    // the form `<ns>/<stem>`. A bare-subject skill reference like
    // `plan.draft` MAY resolve either to the bare key `plan.draft` OR
    // to the namespace-prefixed `<ns>/plan.draft` — same fall-through
    // pattern PR1 uses for `kind: workflow` references. Pre-compute the
    // workflow's own namespace here so the per-entry check stays O(1).
    let own_namespace: Option<&str> = id.split_once('/').map(|(ns, _)| ns);

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
            let Some(subject) = entry.as_str() else {
                continue;
            };
            // Direct match first (bare subject, OR already-prefixed).
            if skill_subjects.contains(subject) {
                continue;
            }
            // Fall through: try the workflow's own-namespace prefix.
            if let Some(ns) = own_namespace {
                let prefixed = format!("{}/{}", ns, subject);
                if skill_subjects.contains(prefixed.as_str()) {
                    continue;
                }
            }
            out.push(Diagnostic::Error(format!(
                "workflow '{id}': {scope} references skills entry '{subject}' \
                 which is not declared in the top-level `skills:` library (SPEC §11)"
            )));
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
                    check_scope(
                        &format!("transition '{t_name}' in state '{state_name}'"),
                        refs,
                    );
                }
            }
        }
    }
}

/// SPEC §5.1, V3/V4/V5 — capability workflows MUST declare a `snippet:`
/// block with `inputs:` AND `outputs:` keys. Each schema entry must be a
/// JSON object (the runtime later validates against the embedded schema
/// via `jsonschema::validator_for`; here we only insist on shape so V17's
/// runtime check has well-formed material to work with).
fn validate_snippet(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(snippet) = def.get("snippet") else {
        out.push(Diagnostic::Error(format!(
            "MISSING_SNIPPET: capability '{id}' is missing required `snippet:` block \
             (SPEC §5.1, V3)"
        )));
        return;
    };
    let Some(snip_obj) = snippet.as_object() else {
        out.push(Diagnostic::Error(format!(
            "INVALID_SNIPPET: capability '{id}' `snippet:` must be an object (SPEC §5.1, V4)"
        )));
        return;
    };
    for key in ["inputs", "outputs"] {
        let Some(block) = snip_obj.get(key) else {
            out.push(Diagnostic::Error(format!(
                "INVALID_SNIPPET: capability '{id}' `snippet:` is missing required \
                 `{key}:` key (may be `{{}}` — but must be present) (SPEC §5.1, V4)"
            )));
            continue;
        };
        let Some(entries) = block.as_object() else {
            out.push(Diagnostic::Error(format!(
                "INVALID_SNIPPET: capability '{id}' `snippet.{key}` must be a mapping \
                 (SPEC §5.1, V4)"
            )));
            continue;
        };
        for (name, schema) in entries {
            if !schema.is_object() {
                out.push(Diagnostic::Error(format!(
                    "INVALID_SNIPPET: capability '{id}' `snippet.{key}.{name}` must be a \
                     JSON-schema-shaped object (SPEC §5.1, V5)"
                )));
            }
        }
    }
}

/// SPEC §6.1, V12 — walk every transition; for any `kind: workflow`
/// executor targeting a `cap.*` definition, require a `use:` block;
/// validate its shape. Also enforces the `host_path → cap_output_name`
/// shape baked into `expand_use_bindings`.
fn validate_use_bindings(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
        return;
    };
    for (state_name, state_def) in states {
        let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t_def) in transitions {
            let Some(exec) = t_def.pointer("/executor").and_then(Value::as_object) else {
                continue;
            };
            if exec.get("kind").and_then(Value::as_str) != Some("workflow") {
                continue;
            }
            let target_def_id = exec.get("definitionId").and_then(Value::as_str);
            let targets_capability = target_def_id
                .map(|d| matches!(Tier::from_id(d), Tier::Cap))
                .unwrap_or(false);
            let has_use = exec.contains_key("use");
            if targets_capability && !has_use {
                out.push(Diagnostic::Error(format!(
                    "MISSING_USE: workflow '{id}' state '{state_name}' transition \
                     '{t_name}' invokes capability '{}' via `kind: workflow` without a \
                     `use:` block. Capability invocations require a typed use-binding \
                     (SPEC §6.1, V12).",
                    target_def_id.unwrap_or("?")
                )));
                continue;
            }
            if let Some(use_val) = exec.get("use") {
                validate_use_block_shape(id, state_name, t_name, use_val, out);
            }
        }
    }
}

fn validate_use_block_shape(
    id: &str,
    state_name: &str,
    t_name: &str,
    use_val: &Value,
    out: &mut Vec<Diagnostic>,
) {
    let Some(obj) = use_val.as_object() else {
        out.push(Diagnostic::Error(format!(
            "INVALID_USE: workflow '{id}' state '{state_name}' transition '{t_name}' \
             `use:` must be a mapping (SPEC §6.1, V12)"
        )));
        return;
    };
    for key in ["inputs", "outputs"] {
        let Some(block) = obj.get(key) else {
            // inputs OR outputs may be omitted only when literally empty — but
            // since the runtime treats absence as `{}`, we accept either.
            continue;
        };
        if !block.is_object() {
            out.push(Diagnostic::Error(format!(
                "INVALID_USE: workflow '{id}' state '{state_name}' transition '{t_name}' \
                 `use.{key}` must be a mapping (SPEC §6.1, V12)"
            )));
        }
    }
    // V12 (shape half): every use.outputs value must be a string naming a
    // capability output, every key must match `$.context.<simple-name>`.
    if let Some(outputs) = obj.get("outputs").and_then(Value::as_object) {
        for (host_path, cap_name) in outputs {
            if cap_name.as_str().is_none() {
                out.push(Diagnostic::Error(format!(
                    "INVALID_USE_OUTPUT_VALUE: workflow '{id}' state '{state_name}' \
                     transition '{t_name}' use.outputs[{host_path}] must be a string \
                     naming a capability output (SPEC §6.1, V12)"
                )));
            }
            if !host_path_tail_ok(host_path) {
                out.push(Diagnostic::Error(format!(
                    "INVALID_USE_OUTPUT_PATH: workflow '{id}' state '{state_name}' \
                     transition '{t_name}' use.outputs key '{host_path}' must match \
                     `^\\$\\.context\\.[a-z][a-z0-9_-]*$` — v0.6 projects only to \
                     single-segment context slots (SPEC §6.1, V12)"
                )));
            }
        }
    }
}

/// Mirror of `crate::config::host_path_tail` — accept iff the path matches
/// `^\$\.context\.[a-z][a-z0-9_-]*$`. Kept private to validate.rs so
/// callers can't accidentally couple to one of the two implementations.
fn host_path_tail_ok(host_path: &str) -> bool {
    let Some(tail) = host_path.strip_prefix("$.context.") else {
        return false;
    };
    if tail.is_empty() || tail.contains('.') || tail.contains('/') {
        return false;
    }
    let mut chars = tail.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

// ============================================================================
// Capability tier rules (Tier::Cap)
// ============================================================================

/// V1 — `verb:` on a capability MUST be one of the 24 closed-cloud tokens.
fn v1_verb_in_cloud(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(verb_str) = def.get("verb").and_then(Value::as_str) else {
        out.push(Diagnostic::Error(format!(
            "MISSING_VERB: capability '{id}' is missing required `verb:` field; \
             allowed verbs are {BLESSED_CAP_VERBS:?} (SPEC §4, V1)"
        )));
        return;
    };
    if CapVerb::from_token(verb_str).is_none() {
        out.push(Diagnostic::Error(format!(
            "INVALID_VERB: capability '{id}' has verb '{verb_str}'; allowed verbs are \
             {BLESSED_CAP_VERBS:?} (SPEC §4, V1)"
        )));
    }
}

/// V2 — capability id stem must be `cap.<verb>.<name>`. Namespace prefix
/// (`swe/`) is stripped before the check.
fn v2_id_matches_verb_name(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let stem = id.rsplit('/').next().unwrap_or(id);
    let parts: Vec<&str> = stem.split('.').collect();
    // `cap.<verb>.<name...>` → at least 3 segments. Allow longer names
    // (`cap.plan.specify.change-request`) by treating everything from
    // index 2 onward as the name body.
    if parts.len() < 3 || parts[0] != "cap" {
        out.push(Diagnostic::Error(format!(
            "INVALID_ID_SHAPE: capability '{id}' must match `cap.<verb>.<name>` \
             (SPEC §4, V2)"
        )));
        return;
    }
    let id_verb = parts[1];
    // The verb-in-cloud check is V1; here we only assert that the id's
    // verb segment matches the declared `verb:` field. If `verb:` is
    // absent or unrecognized, V1 already fired — silently skip to avoid
    // double-reporting.
    if let Some(declared_verb) = def.get("verb").and_then(Value::as_str) {
        if declared_verb != id_verb {
            out.push(Diagnostic::Error(format!(
                "ID_VERB_MISMATCH: capability '{id}' declares verb '{declared_verb}' but \
                 its id stem uses verb segment '{id_verb}' — must agree (SPEC §4, V2)"
            )));
        }
    }
}

/// V6 — primary-executor verb-shape check. Inspects the executor on the
/// transition leaving the capability's initial state (TRIZ Local Quality:
/// narrow check that catches gross misuse without walking every transition).
fn v6_primary_executor_verb_shape(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(verb_str) = def.get("verb").and_then(Value::as_str) else {
        return; // V1 already flagged
    };
    let Some(verb) = CapVerb::from_token(verb_str) else {
        return; // V1 already flagged
    };
    let Some(initial_state) = def.get("initialState").and_then(Value::as_str) else {
        return; // generic missing-initialState error fired elsewhere
    };
    let Some(transitions) = def
        .pointer(&format!(
            "/states/{}/transitions",
            pointer_escape(initial_state)
        ))
        .and_then(Value::as_object)
    else {
        return;
    };
    if transitions.is_empty() {
        // A capability with no outgoing transitions is a different issue;
        // V6 has nothing to constrain.
        return;
    }

    // Find at least one primary transition whose executor kind matches.
    let mut primary_kinds: Vec<&str> = Vec::new();
    let mut has_human_actor = false;
    for (_t_name, t_def) in transitions {
        if let Some(kind) = t_def.pointer("/executor/kind").and_then(Value::as_str) {
            primary_kinds.push(kind);
        }
        if t_def.get("actor").and_then(Value::as_str) == Some("human")
            || t_def.get("purpose").and_then(Value::as_str) == Some("ask")
        {
            has_human_actor = true;
        }
    }

    let category = verb.category();
    let ok = match category {
        CapVerbCategory::Cognitive => primary_kinds.iter().any(|k| matches!(*k, "mcp" | "noop")),
        CapVerbCategory::Deterministic => {
            primary_kinds.iter().any(|k| matches!(*k, "script" | "mcp"))
        }
        CapVerbCategory::Coordination => match verb {
            CapVerb::Gate => has_human_actor,
            // Spec §4.1 ideal for `coordinate` is `kind: mcp` AND
            // connection `external: true`. PR3 enforces only the
            // `kind: mcp` half (no `external:` field exists yet);
            // documented gap, CHANGELOG entry for v0.7 follow-up.
            CapVerb::Coordinate => primary_kinds.contains(&"mcp"),
            _ => true,
        },
    };
    if !ok {
        let allowed = match category {
            CapVerbCategory::Cognitive => "kind: mcp OR kind: noop (skill-surfacing)",
            CapVerbCategory::Deterministic => "kind: script OR kind: mcp",
            CapVerbCategory::Coordination => match verb {
                CapVerb::Gate => {
                    "at least one initial-state transition with actor: human OR purpose: ask"
                }
                CapVerb::Coordinate => "kind: mcp",
                _ => "?",
            },
        };
        out.push(Diagnostic::Error(format!(
            "INVALID_PRIMARY_EXECUTOR: capability '{id}' (verb '{verb_str}', category \
             {category:?}) has initial-state transitions whose primary executor kinds \
             are {primary_kinds:?}; expected {allowed} (SPEC §4.1, V6)"
        )));
    }
}

/// V10 — capabilities MUST NOT invoke other workflows via `kind: workflow`
/// (the no-nesting rule). Capabilities are leaves of the composition tree.
fn v10_capability_does_not_invoke_workflow(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    if let Some(target) = find_first_workflow_invocation(def) {
        out.push(Diagnostic::Error(format!(
            "CAPABILITY_NESTING: capability '{id}' invokes workflow '{target}' via \
             `kind: workflow`. Capabilities are composition leaves; only orchestrators \
             may invoke other workflows (SPEC §3, V10)"
        )));
    }
}

// ============================================================================
// Orchestrator tier rules (Tier::Flow)
// ============================================================================

/// V7 — orchestrator id stem must match `flow.<name>`.
fn v7_id_matches_flow_pattern(id: &str, _def: &Value, out: &mut Vec<Diagnostic>) {
    let stem = id.rsplit('/').next().unwrap_or(id);
    let parts: Vec<&str> = stem.split('.').collect();
    if parts.len() < 2 || parts[0] != "flow" || parts[1].is_empty() {
        out.push(Diagnostic::Error(format!(
            "INVALID_ID_SHAPE: orchestrator '{id}' must match `flow.<name>` (SPEC §3, V7)"
        )));
    }
}

/// V8 — orchestrators MUST NOT declare a `snippet:` block.
fn v8_orchestrator_has_no_snippet(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    if def.get("snippet").is_some() {
        out.push(Diagnostic::Error(format!(
            "ORCHESTRATOR_HAS_SNIPPET: orchestrator '{id}' declares a `snippet:` block; \
             snippets are capability-only — orchestrators are not externally invokable \
             as snippets (SPEC §5.1, V8)"
        )));
    }
}

/// V9 — orchestrators MUST NOT declare a `verb:` field.
fn v9_orchestrator_has_no_verb(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    if def.get("verb").is_some() {
        out.push(Diagnostic::Error(format!(
            "ORCHESTRATOR_HAS_VERB: orchestrator '{id}' declares `verb:`; verbs are \
             capability-only (SPEC §4, V9)"
        )));
    }
}

/// V11 — orchestrators MUST NOT invoke other orchestrators (the
/// no-nesting rule, sibling of V10).
fn v11_orchestrator_does_not_invoke_orchestrator(id: &str, def: &Value, out: &mut Vec<Diagnostic>) {
    let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
        return;
    };
    for (_state_name, state_def) in states {
        let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object) else {
            continue;
        };
        for (_t_name, t_def) in transitions {
            let Some(target) = t_def
                .pointer("/executor/definitionId")
                .and_then(Value::as_str)
            else {
                continue;
            };
            if matches!(Tier::from_id(target), Tier::Flow) {
                out.push(Diagnostic::Error(format!(
                    "ORCHESTRATOR_NESTING: orchestrator '{id}' invokes orchestrator \
                     '{target}' via `kind: workflow`. Orchestrators may only invoke \
                     capabilities (SPEC §3, V11)"
                )));
                return; // one violation per workflow keeps error noise low
            }
        }
    }
}

/// Walk every transition's executor and return the first `kind: workflow`
/// `definitionId:` found. Helper for V10's no-nesting check.
fn find_first_workflow_invocation(def: &Value) -> Option<String> {
    let states = def.pointer("/states").and_then(Value::as_object)?;
    for state_def in states.values() {
        if let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object) {
            for t_def in transitions.values() {
                if t_def.pointer("/executor/kind").and_then(Value::as_str) == Some("workflow") {
                    if let Some(target) = t_def
                        .pointer("/executor/definitionId")
                        .and_then(Value::as_str)
                    {
                        return Some(target.to_string());
                    }
                }
            }
        }
    }
    None
}

// ============================================================================
// Cross-tier contract-hash rules
// ============================================================================

/// V15 / V16 — `expects_contract_hash:` validation, walked across every
/// `kind: workflow` invocation. V15 fires when an explicit pin doesn't
/// match the target capability's computed contract hash; V16 fires when
/// a stable-lifecycle target is invoked without any pin.
fn validate_contract_hash_pins(
    id: &str,
    def: &Value,
    ctx: &ValidationCtx<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
        return;
    };
    for (state_name, state_def) in states {
        let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t_def) in transitions {
            let Some(exec) = t_def.pointer("/executor").and_then(Value::as_object) else {
                continue;
            };
            if exec.get("kind").and_then(Value::as_str) != Some("workflow") {
                continue;
            }
            let Some(target_id) = exec.get("definitionId").and_then(Value::as_str) else {
                continue;
            };
            let Some(actual_hash) = ctx.cap_contract_hashes.get(target_id) else {
                continue; // target isn't a snippet-bearing cap; nothing to pin
            };
            let declared_pin = exec.get("expects_contract_hash").and_then(Value::as_str);
            let lifecycle = ctx
                .cap_lifecycles
                .get(target_id)
                .map(String::as_str)
                .unwrap_or("experimental");
            match (declared_pin, lifecycle) {
                (Some(pin), _) if pin != actual_hash => {
                    out.push(Diagnostic::Error(format!(
                        "CONTRACT_HASH_MISMATCH: workflow '{id}' state '{state_name}' \
                         transition '{t_name}' pins capability '{target_id}' to \
                         `{pin}` but the loaded contract hash is `{actual_hash}` \
                         (SPEC §6.2, V15)"
                    )));
                }
                (None, "stable") => {
                    out.push(Diagnostic::Error(format!(
                        "MISSING_CONTRACT_HASH: workflow '{id}' state '{state_name}' \
                         transition '{t_name}' invokes stable-lifecycle capability \
                         '{target_id}' without `expects_contract_hash:`. Add: \
                         expects_contract_hash: \"{actual_hash}\" (SPEC §6.2, V16)"
                    )));
                }
                _ => {}
            }
        }
    }
}

// ============================================================================
// Slot-table rules (V13, V14) — orchestrator-only
// ============================================================================

/// V13/V14 — build the orchestrator's slot table, then check every
/// `use:.inputs` reference for reachability against it. V14 (type
/// consistency between two states writing the same path) is enforced
/// inside [`crate::slot_table::build_slot_table`] and surfaces here as
/// part of the returned diagnostic list.
fn v13_v14_slot_table(id: &str, def: &Value, ctx: &ValidationCtx<'_>, out: &mut Vec<Diagnostic>) {
    let table = match crate::slot_table::build_slot_table(def, ctx.cap_snippet_outputs) {
        Ok(t) => t,
        Err(diagnostics) => {
            out.extend(diagnostics);
            return;
        }
    };

    let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
        return;
    };
    for (state_name, state_def) in states {
        let Some(transitions) = state_def.pointer("/transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t_def) in transitions {
            let Some(use_inputs) = t_def
                .pointer("/executor/use/inputs")
                .and_then(Value::as_object)
            else {
                continue;
            };
            for (_input_name, expr_value) in use_inputs {
                let Some(expr) = expr_value.as_str() else {
                    continue;
                };
                if !expr.starts_with("$.context.") {
                    // Non-context references (literals, $.workflow.input.*,
                    // $.arguments.*) bypass the slot table — they don't
                    // need to resolve through state writes.
                    continue;
                }
                if let Some(d) =
                    crate::slot_table::assert_reachable(&table, expr, id, state_name, t_name)
                {
                    out.push(d);
                }
            }
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Escape a JSON-Pointer path segment per RFC 6901: `~` → `~0`, `/` → `~1`.
/// Lets us index state names that contain `/` (rare but legal — namespace
/// fixtures sometimes have them).
fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
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
