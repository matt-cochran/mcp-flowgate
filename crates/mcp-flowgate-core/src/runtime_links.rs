use serde_json::Value;
use serde_json::json;

use crate::model::WorkflowInstance;


/// `linkFilter: byGuards` may be declared on the workflow or per-state.
/// State setting wins when both exist.
pub(crate) fn link_filter_byguards(definition: &Value, state: &str) -> bool {
    let state_setting = definition
        .pointer(&format!("/states/{}/linkFilter", pointer_escape(state)))
        .and_then(Value::as_str);
    if let Some(s) = state_setting {
        return s == "byGuards";
    }
    definition
        .get("linkFilter")
        .and_then(Value::as_str)
        .map(|s| s == "byGuards")
        .unwrap_or(false)
}

pub(crate) fn links(definition: &Value, instance: &WorkflowInstance) -> Vec<Value> {
    if is_terminal(definition, &instance.state) {
        return vec![];
    }

    let path = format!("/states/{}/transitions", pointer_escape(&instance.state));
    let Some(transitions) = definition.pointer(&path).and_then(Value::as_object) else {
        return vec![];
    };

    let library = definition.get("_skillsLibrary").and_then(Value::as_object);

    transitions
        .iter()
        .filter(|(_, t)| t.get("actor").and_then(Value::as_str) != Some("deterministic"))
        .map(|(rel, transition)| {
            // Build the args block. Always carry workflowId / expectedVersion /
            // transition. If the transition declares `prefill`, resolve each
            // value against current scopes and embed under `args.arguments`
            // so an LLM caller can take them verbatim and only generate the
            // fields it actually needs to choose.
            let mut args = serde_json::Map::new();
            args.insert("workflowId".into(), json!(instance.id));
            args.insert("expectedVersion".into(), json!(instance.version));
            args.insert("transition".into(), json!(rel));
            if let Some(prefill) = transition.get("prefill").and_then(Value::as_object) {
                let empty = json!({});
                let mut resolved = serde_json::Map::with_capacity(prefill.len());
                for (k, spec) in prefill {
                    let v = crate::mapping::resolve_value(
                        spec,
                        &empty,             // no caller arguments at link-gen time
                        &instance.context,
                        &instance.input,
                        &empty,             // no executor output at link-gen time
                    );
                    resolved.insert(k.clone(), v);
                }
                if !resolved.is_empty() {
                    args.insert("arguments".into(), Value::Object(resolved));
                }
            }

            // SPEC v2 §5.5: transition-scope `skills:` refs ride on the link.
            // They are NOT folded into `guidance.refs` (which carries workflow
            // and state scope) so the model can tell which fragments are
            // tied to taking *this specific* transition.
            let mut link = json!({
                "rel": rel,
                "title": transition.get("title").and_then(Value::as_str).unwrap_or(rel),
                "description": transition.get("description"),
                "method": "workflow.submit",
                "actor": transition.get("actor").and_then(Value::as_str).unwrap_or("agent"),
                "args": args,
                "inputSchema": transition.get("inputSchema").cloned().unwrap_or_else(empty_object_schema),
            });
            let refs = resolve_skill_refs(transition.get("skills"), library);
            if !refs.is_empty() {
                link["guidance"] = json!({ "refs": refs });
            }
            link
        })
        .collect()
}

pub(crate) fn transition_definition<'a>(
    definition: &'a Value,
    state: &str,
    transition: &str,
) -> Option<&'a Value> {
    definition.pointer(&format!(
        "/states/{}/transitions/{}",
        pointer_escape(state),
        pointer_escape(transition)
    ))
}

pub fn is_terminal(definition: &Value, state: &str) -> bool {
    definition
        .pointer(&format!("/states/{}/terminal", pointer_escape(state)))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn pointer_escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

pub(crate) fn empty_object_schema() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

/// Build the `guidance.refs` array from a workflow snapshot. Pulls subjects
/// from workflow-scope `skills:` and the active state's `skills:` (de-duped,
/// declaration order). Transition-scope refs are surfaced on the link object
/// instead (SPEC §5.5) so callers can tell which fragments are tied to
/// taking *this specific* transition; they are NOT folded in here. Each
/// emitted ref pairs `subject` with the `verb` looked up in the
/// snapshot-stamped `_skillsLibrary`. Subjects with no library entry are
/// skipped — `check` reports those as errors.
pub(crate) fn collect_guidance_refs(definition: &Value, state_def: Option<&Value>) -> Vec<Value> {
    let library = definition.get("_skillsLibrary").and_then(Value::as_object);
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    push_resolved_refs(definition.get("skills"), library, &mut seen, &mut out);
    push_resolved_refs(
        state_def.and_then(|s| s.get("skills")),
        library,
        &mut seen,
        &mut out,
    );
    out
}

/// Resolve a single scope's `skills: [subject]` against the library and
/// emit `{verb, subject}` JSON values for the link layer. Used independently
/// of `collect_guidance_refs` so transition-scope refs (which need their own
/// `seen` set per link) don't accidentally consume workflow/state state.
pub(crate) fn resolve_skill_refs(
    scope: Option<&Value>,
    library: Option<&serde_json::Map<String, Value>>,
) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    push_resolved_refs(scope, library, &mut seen, &mut out);
    out
}

pub(crate) fn push_resolved_refs(
    scope: Option<&Value>,
    library: Option<&serde_json::Map<String, Value>>,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<Value>,
) {
    let Some(arr) = scope.and_then(Value::as_array) else {
        return;
    };
    for entry in arr {
        let Some(subject) = entry.as_str() else { continue };
        if !seen.insert(subject.to_string()) {
            continue;
        }
        // `_skillsLibrary` is `{ subject: { verb, body } }` post-§8.2; only
        // `verb` is needed to assemble the surfaced ref. Body is consulted
        // by `gateway.describe(id, workflowId)` against the snapshot.
        let verb = library
            .and_then(|lib| lib.get(subject))
            .and_then(|entry| entry.get("verb"))
            .and_then(Value::as_str);
        let Some(verb) = verb else { continue };
        out.push(json!({ "verb": verb, "subject": subject }));
    }
}
