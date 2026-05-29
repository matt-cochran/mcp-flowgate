//! Config preprocessor.
//!
//! Two stages, both pure on `serde_json::Value`:
//!
//! 1. `merge_includes` — walk top-level `include: [paths…]`, load and
//!    deep-merge every referenced YAML file into the config. Maps merge
//!    (later wins on collisions), arrays concatenate. Cycles raise an error.
//! 2. `resolve` — flatten everything compositional into the inline shapes
//!    the runtime understands:
//!    - Capability `wraps:` chains become single normalized capabilities.
//!    - `executor: { capability: foo }` references become inline
//!      `executor: { kind: ..., ... }` configs; the capability's guards and
//!      reliability stack into the calling context.
//!    - `proxy.expose: [{ capability: foo, as: bar, ... }]` references
//!      become inline `{ name: bar, executor: ..., ... }` exposures.
//!
//! After preprocessing, the rest of the system (DefinitionStore,
//! discovery indexer, proxy compiler) sees only the original inline shapes —
//! they don't need to know about capabilities, wraps, or includes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::discovery::{
    Lifecycle, ScriptVerb, Verb, BLESSED_SCRIPT_ROOTS, BLESSED_SUBJECT_ROOTS,
};

/// Recursively load `path` as YAML and merge any `include:` files into it.
/// Includes resolve relative to the file that lists them.
pub fn load_yaml(path: impl AsRef<Path>) -> anyhow::Result<Value> {
    let mut visited = HashSet::new();
    load_yaml_inner(path.as_ref(), &mut visited)
}

fn load_yaml_inner(path: &Path, visited: &mut HashSet<PathBuf>) -> anyhow::Result<Value> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("resolving config path {}", path.display()))?;
    if !visited.insert(canonical.clone()) {
        bail!("config include cycle detected at {}", canonical.display());
    }

    let text = std::fs::read_to_string(&canonical)
        .with_context(|| format!("reading config {}", canonical.display()))?;
    let mut value: Value = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing YAML {}", canonical.display()))?;

    let parent = canonical.parent().unwrap_or_else(|| Path::new("."));

    // SPEC §22.2 — rewrite any `file://` URIs in `scripts:` entries to
    // absolute paths now, while we still know the config file's directory.
    // `resolve()` is path-agnostic by design; doing this here keeps the
    // pure-value abstraction downstream and gives sensible relative-path
    // semantics (file:// URIs resolve relative to the YAML they were
    // declared in, not the gateway's CWD).
    rewrite_script_uris_to_absolute(&mut value, parent);

    if let Some(includes) = value.get("include").and_then(Value::as_array).cloned() {
        // Each include is loaded in declaration order, then the current file's
        // body overrides on top. (Includes are "defaults" that the explicit
        // file can refine.) Final order of merging: includes[0], includes[1],
        // ..., main body (last wins).
        let mut merged = Value::Object(Map::new());
        for inc in &includes {
            let inc_path = inc
                .as_str()
                .ok_or_else(|| anyhow!("include entries must be strings"))?;
            let inc_full = parent.join(inc_path);
            let inc_value = load_yaml_inner(&inc_full, visited)?;
            merged = deep_merge(merged, inc_value);
        }
        // Drop the `include` key from the local body before merging — it's
        // already been processed.
        if let Some(obj) = value.as_object_mut() {
            let _: Option<Value> = obj.remove("include");
        }
        merged = deep_merge(merged, value);
        return Ok(merged);
    }

    Ok(value)
}

/// Deep-merge `b` into `a`. Maps merge recursively (b wins on key collisions).
/// Arrays concatenate (`a` first, then `b`). Scalars: `b` wins.
pub fn deep_merge(a: Value, b: Value) -> Value {
    match (a, b) {
        (Value::Object(mut am), Value::Object(bm)) => {
            for (k, v) in bm {
                let merged = match am.remove(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => v,
                };
                am.insert(k, merged);
            }
            Value::Object(am)
        }
        (Value::Array(mut aa), Value::Array(ab)) => {
            aa.extend(ab);
            Value::Array(aa)
        }
        (_, b) => b,
    }
}

/// SPEC §5.4.2 / audit-resolution C.2 — a single diagnostic produced by
/// `resolve_with_diagnostics`. Severity is `warn` for soft issues (e.g.
/// non-strict-mode unblessed subject roots) and `error` for hard issues
/// (which `resolve` itself returns via `Err`, so they don't appear here
/// except where surfacing a structured form is useful).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Diagnostic {
    pub severity: DiagnosticSeverity,
    pub code: String,
    pub message: String,
    /// JSON-Pointer style path to the offending location, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Free-form remediation hint (e.g. closest blessed root suggestion).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticSeverity {
    Warn,
    Error,
}

/// Resolve `capabilities:`, `wraps:`, and `capability:` references into the
/// inline shapes the runtime expects. Idempotent — calling it twice is safe.
///
/// Discards any soft diagnostics. Use `resolve_with_diagnostics` to capture
/// them (e.g. unblessed-subject-root warnings under
/// `strict_namespacing: false`).
pub fn resolve(value: Value) -> anyhow::Result<Value> {
    let (config, _diagnostics) = resolve_with_diagnostics(value)?;
    Ok(config)
}

/// SPEC §5.4.2 / audit-resolution C.2 — like `resolve` but also returns
/// any soft `Diagnostic`s collected during validation. Hard errors still
/// propagate via `Err`.
pub fn resolve_with_diagnostics(mut config: Value) -> anyhow::Result<(Value, Vec<Diagnostic>)> {
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    // 1. Flatten the capabilities block into a registry of normalized defs.
    let registry = flatten_capabilities(&config)?;

    // 2. Rewrite `proxy.expose` entries that are capability refs into inline
    //    exposures. Inline entries pass through unchanged.
    if let Some(exposures) = config
        .pointer_mut("/proxy/expose")
        .and_then(Value::as_array_mut)
    {
        let rewritten: Vec<Value> = std::mem::take(exposures)
            .into_iter()
            .map(|ex| rewrite_exposure(ex, &registry))
            .collect::<anyhow::Result<Vec<_>>>()?;
        *exposures = rewritten;
    }

    // 3. Rewrite executors throughout `proxy.expose`, `workflows.*`, and any
    //    nested onEnter / transitions / fallback executors.
    rewrite_executors_in_value(&mut config, &registry)?;

    // 4. Strip the now-fully-resolved `capabilities` block — it's an authoring
    //    affordance, not runtime state.
    //    SPEC §30.10.3 — capture capability subjects BEFORE stripping so
    //    `inject_pending_definitions` can see them even though the block will
    //    no longer be present in the config when that function runs.
    //    Only keys that follow the `verb.subject` pattern (contain a `.`) are
    //    lexicon subjects; simple names like `do_thing` are capability names,
    //    not subject references, and are skipped.
    let capability_subjects: Vec<String> = config
        .pointer("/capabilities")
        .and_then(Value::as_object)
        .map(|caps| {
            caps.keys()
                .filter(|k| k.contains('.'))
                .map(|k| crate::lexicon::subject_portion_pub(k))
                .collect()
        })
        .unwrap_or_default();
    if let Some(obj) = config.as_object_mut() {
        let _: Option<Value> = obj.remove("capabilities");
    }

    // 5. Apply the per-workflow-definition version default ("0") to any
    //    workflow definition that does not carry an explicit `version`.
    //    This ensures downstream code (runtime, stores) always sees a version
    //    on every workflow definition.
    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            if let Some(obj) = def.as_object_mut() {
                obj.entry("version")
                    .or_insert_with(|| Value::String("0".to_string()));
            }
        }
    }

    // 6. Poka-yoke on `skills:` (SPEC §5.4). `verb` and the `skills:` keys
    //    must match `^[a-z][a-z0-9-]*$` — lowercase kebab, no whitespace.
    //    Enforced at config load so malformed descriptors are unrepresentable
    //    rather than only linted.
    validate_skills(&config, &mut diagnostics)?;

    // 6a-bis. SPEC §22 — `scripts:` block validates next to `skills:`. Same
    //         strict-vs-lenient blessed-root semantics. Distinct verb enum
    //         (action verbs vs cognitive verbs) and stricter hash
    //         normalization (whitespace is load-bearing in shell).
    validate_scripts(&config, &mut diagnostics)?;

    // 6b. SPEC §8.4 + §20.2 — reject runtime-only `flowgate.*` flags when
    //     they appear inside any `workflows:` block. The flags are read at
    //     gateway startup only; allowing them at workflow scope would let
    //     an LLM-authored workflow attempt to (silently) flip the bypass
    //     flag on for itself.
    validate_workflow_flag_scope(&config)?;

    // 6c. SPEC §21 — `delegate` is a TUI pass-through string. It MUST be
    //     a non-empty string when present. Validating shape here means the
    //     runtime never has to defend against `delegate: ""` or `delegate: 42`
    //     reaching the response surface.
    validate_state_delegate(&config)?;

    // 6d. SPEC §17.x (v0.3) — `flowgate.authoring.*` preferences are
    //     advisory strings surfaced to LLM-driven authoring workflows via
    //     template substitution. Shape-validated here; nothing rejects a
    //     workflow for ignoring the preference.
    validate_authoring_preferences(&config)?;

    // 7. Stamp each workflow definition with `_skillsLibrary: { subject: verb }`
    //    drawn from the top-level `skills:` map (subjects only — verb, no body;
    //    body is fetched on demand via `gateway.describe`). Lets the runtime
    //    decorate `guidance.refs` from the per-instance snapshot alone without
    //    needing a side channel to the top-level config.
    stamp_skills_library(&mut config);

    // 7-bis. SPEC §22 — stamp `_scriptsLibrary` onto each workflow that
    //        references a curated script. Resolves file:// URIs and verifies
    //        hashes; `SCRIPT_HASH_MISMATCH` here means the external script
    //        body drifted since the workflow was authored.
    stamp_scripts_library(&mut config)?;

    // 7-ter. SPEC §17.x (v0.3) — stamp `_authoringPrefs` onto every workflow
    //        snapshot so authoring skills can reach the operator's
    //        preferences via template substitution `{{$.flowgate.authoring.*}}`.
    stamp_authoring_preferences(&mut config);

    // 7-quater. SPEC §29 — when a workflow declares `enable_human_ask: true`,
    //           inject a self-loop `ask_human` transition into every
    //           non-terminal state. Lets the agent ask mid-reasoning
    //           clarifying questions without per-state authoring burden.
    inject_human_ask_transitions(&mut config);

    // 7-quinquies. SPEC §30 — validate + stamp the lexicon library.
    //              Every workflow gets a `_lexiconLibrary` snapshot
    //              so in-flight reads are deterministic (same
    //              invariant as `_skillsLibrary` / `_scriptsLibrary`).
    crate::lexicon::validate_lexicon(&config)?;
    crate::lexicon::stamp_lexicon_library(&mut config);

    // 7-sexties-bis. SPEC §30.10.3 — inject PENDING_DEFINITION placeholders
    //               for any subject referenced in scripts/skills/executors that
    //               lacks an authored lexicon entry. Placeholders accumulate in
    //               the stamped _lexiconLibrary so doctor and (Task 3.3) the
    //               runtime can surface unresolved subjects without hard-failing
    //               the load.
    //               `capability_subjects` carries subjects harvested from the
    //               `capabilities:` block at step 4 (before it was stripped).
    //               Passing them here closes the pipeline-ordering gap so
    //               capability-block subjects are detected as pending (SPEC §30.10.3).
    // TODO(SPEC §30.10.3): inherit bounded_context from the referencing
    // config. Currently defaults to global; sufficient for v0.5 since
    // Tier-1 lexicons are typically single-context.
    crate::lexicon::inject_pending_definitions(&mut config, &capability_subjects);

    // 7-sexies. SPEC §6 — for every transition whose executor is
    //           `kind: workflow` with a `use:` block, synthesize the
    //           transition-level `output:` mapping from `use.outputs`
    //           and embed the target capability's `snippet.outputs`
    //           schema as `_snippetOutputs` on the executor config.
    //           After this pass, the runtime's existing merge_output
    //           projection drives cap-output writes; the executor needs
    //           no schema lookup at run time.
    expand_use_bindings(&mut config)?;

    Ok((config, diagnostics))
}

/// SPEC §6 — Walk every workflow's transitions; for any `kind: workflow`
/// executor with a `use:` block:
///
/// 1. Resolve the target capability's `snippet.outputs` from
///    `config["workflows"][definitionId]["snippet"]["outputs"]` and embed
///    it on the executor as `_snippetOutputs` so the runtime executor
///    has the schema in hand without doing a DefinitionStore lookup.
///
/// 2. Synthesize the transition-level `output:` mapping from `use.outputs`.
///    Each `host_path → cap_output_name` entry becomes
///    `<host_path_tail>: "$.output.<cap_output_name>"` where
///    `host_path_tail` strips the `$.context.` prefix. The synthesized
///    mapping merges into any operator-declared `output:` block; operator
///    declarations win on tail-key collisions (so an author can override
///    a single field while letting the rest auto-project).
///
/// Errors when:
/// - `use:` is present but the target `definitionId` is not loaded.
/// - A `use.outputs` LHS does not match `^\$\.context\.[a-z][a-z0-9_-]*$`
///   (V12 — runtime can only write top-level context keys via merge_output).
///
/// Idempotent: re-running on already-expanded config detects the embedded
/// `_snippetOutputs` and skips.
fn expand_use_bindings(config: &mut Value) -> anyhow::Result<()> {
    // Borrow the workflows map immutably to harvest snippet schemas, then
    // walk it mutably to inject the synthesized outputs. We can't do both
    // at once, so snapshot the snippet schemas into a HashMap up front.
    let snippets: HashMap<String, Value> = match config
        .pointer("/workflows")
        .and_then(Value::as_object)
    {
        Some(workflows) => workflows
            .iter()
            .filter_map(|(id, def)| {
                def.pointer("/snippet/outputs")
                    .cloned()
                    .map(|outputs| (id.clone(), outputs))
            })
            .collect(),
        None => HashMap::new(),
    };

    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return Ok(());
    };

    for (wf_id, def) in workflows.iter_mut() {
        let Some(states) = def
            .pointer_mut("/states")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for (state_name, state_def) in states.iter_mut() {
            let Some(transitions) = state_def
                .pointer_mut("/transitions")
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            for (t_name, t_def) in transitions.iter_mut() {
                expand_one_transition(t_def, &snippets, wf_id, state_name, t_name)?;
            }
        }
    }
    Ok(())
}

/// Expand a single transition's `use:` block in place. See [`expand_use_bindings`]
/// for the full rule set. Trailing args (`wf_id`, `state_name`, `t_name`)
/// are diagnostic context for error messages — when V12 fires, the operator
/// gets the exact JSON-Pointer-equivalent path to the offender.
fn expand_one_transition(
    t_def: &mut Value,
    snippets: &HashMap<String, Value>,
    wf_id: &str,
    state_name: &str,
    t_name: &str,
) -> anyhow::Result<()> {
    let Some(t_obj) = t_def.as_object_mut() else {
        return Ok(());
    };
    let Some(executor) = t_obj.get_mut("executor") else {
        return Ok(());
    };
    let Some(exec_obj) = executor.as_object_mut() else {
        return Ok(());
    };
    let is_workflow = exec_obj.get("kind").and_then(Value::as_str) == Some("workflow");
    if !is_workflow {
        return Ok(());
    }
    let Some(use_val) = exec_obj.get("use").cloned() else {
        return Ok(());
    };

    // Without a definitionId we can't look up the snippet schema and we
    // can't validate references. Leave the transition untouched and let
    // `validate.rs::validate_use_bindings` surface the diagnostic — it
    // has the same context this function does and produces a proper
    // structured `Diagnostic::Error`.
    let Some(def_id) = exec_obj
        .get("definitionId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };

    // V22 (cross-PR with PR1): the ref must resolve to a loaded workflow.
    // We only emit `_snippetOutputs` when the target declares a snippet;
    // legacy non-cap callees stay untouched (some pre-v0.6 fixtures
    // use `kind: workflow` against plain workflows without `snippet:`).
    let snippet_outputs = snippets.get(&def_id);

    // Embed the snippet schema for the runtime executor.
    if let Some(s) = snippet_outputs {
        exec_obj.insert("_snippetOutputs".into(), s.clone());
    }

    // Synthesize the transition-level `output:` mapping from use.outputs.
    // Skips malformed entries silently — `validate.rs::validate_use_block_shape`
    // is the surface that reports them as `Diagnostic::Error`. Errors-as-data
    // beat errors-as-bail here so a single bad transition doesn't poison the
    // whole config load.
    let _ = (wf_id, state_name, t_name); // diagnostic context retained for future use
    let Some(use_outputs) = use_val.get("outputs").and_then(Value::as_object) else {
        return Ok(());
    };
    let mut synthesized = Map::new();
    for (host_path, cap_name_value) in use_outputs {
        let Some(cap_name) = cap_name_value.as_str() else { continue };
        let Some(tail) = host_path_tail(host_path) else { continue };
        synthesized.insert(tail, Value::String(format!("$.output.{cap_name}")));
    }

    // Merge with any operator-declared `output:` block. Operator wins on
    // collisions (lets authors override one slot while auto-projecting
    // the rest).
    let existing = t_obj
        .get("output")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (k, v) in existing {
        synthesized.insert(k, v);
    }
    t_obj.insert("output".into(), Value::Object(synthesized));
    Ok(())
}

/// Extract the top-level context-slot name from `$.context.<name>`. Returns
/// `None` for any other path shape (nested paths, non-context roots, etc.).
/// `<name>` must match `^[a-z][a-z0-9_-]*$`.
fn host_path_tail(host_path: &str) -> Option<String> {
    let tail = host_path.strip_prefix("$.context.")?;
    if tail.is_empty() || tail.contains('.') || tail.contains('/') {
        return None;
    }
    let mut chars = tail.chars();
    let first = chars.next()?;
    if !first.is_ascii_lowercase() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return None;
    }
    Some(tail.to_string())
}

/// SPEC §29 — for every workflow with `enable_human_ask: true`, inject
/// a self-loop `ask_human` transition into every non-terminal state.
/// The injected transition:
/// - target = same state (self-loop, doesn't advance)
/// - actor: human (only humans can submit; gates the answer)
/// - purpose: ask (TUI/dashboard filtering tag)
/// - lightweight: true (audit emits `workflow.interaction` not `.transition`)
/// - max_fires_per_visit: <workflow's `human_ask_cap` field, default 5>
/// - inputSchema requires the agent to fill question + context_summary +
///   attempted_alternatives so questions arrive WITH context
/// - outputSchema requires a string answer
///
/// Idempotent: if the state already declares an `ask_human` transition,
/// the injection is skipped (operator override takes precedence).
fn inject_human_ask_transitions(config: &mut Value) {
    use serde_json::json;
    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for (_id, def) in workflows {
        let enabled = def
            .get("enable_human_ask")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            continue;
        }
        let cap = def
            .get("human_ask_cap")
            .and_then(Value::as_u64)
            .unwrap_or(5);
        let Some(states) = def
            .pointer_mut("/states")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for (state_name, state_def) in states {
            // Skip terminal states — no point asking questions on a state
            // the workflow can never leave.
            if state_def
                .get("terminal")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let Some(state_obj) = state_def.as_object_mut() else {
                continue;
            };
            // Ensure transitions: {} exists.
            let transitions = state_obj
                .entry("transitions")
                .or_insert(Value::Object(Default::default()))
                .as_object_mut()
                .expect("transitions must be an object");
            // Operator override — don't clobber an existing ask_human.
            if transitions.contains_key("ask_human") {
                continue;
            }
            transitions.insert(
                "ask_human".to_string(),
                json!({
                    "target":              state_name,
                    "actor":               "human",
                    "purpose":             "ask",
                    "lightweight":         true,
                    "max_fires_per_visit": cap,
                    "inputSchema": {
                        "type": "object",
                        "required": ["question", "context_summary", "attempted_alternatives"],
                        "properties": {
                            "question": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 2000,
                                "description": "The question for the human. Be specific; the human can't see your reasoning chain."
                            },
                            "context_summary": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 1000,
                                "description": "Brief context — what you're trying to do, what state you're in."
                            },
                            "attempted_alternatives": {
                                "type": "string",
                                "minLength": 1,
                                "maxLength": 1000,
                                "description": "What you already tried (docs, scripts, other tools) before asking. SPEC §29.6 — agents must demonstrate effort before interrupting humans."
                            }
                        }
                    },
                    "outputSchema": {
                        "type": "object",
                        "required": ["answer"],
                        "properties": {
                            "answer": {
                                "type": "string",
                                "minLength": 1,
                                "description": "The human's answer."
                            }
                        }
                    }
                }),
            );
        }
    }
}

/// Audit-resolution C.2 — return the blessed root closest to `candidate`
/// by simple shared-prefix length. Cheap heuristic: enough to catch
/// `revoew` → `review` typos without dragging in a Levenshtein dependency.
/// Returns `None` when candidate is empty or no prefix overlap exists.
fn closest_blessed_root(candidate: &str) -> Option<&'static str> {
    if candidate.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static str)> = None;
    for root in BLESSED_SUBJECT_ROOTS {
        let shared = candidate
            .chars()
            .zip(root.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if shared == 0 {
            continue;
        }
        if best.map(|(b, _)| shared > b).unwrap_or(true) {
            best = Some((shared, root));
        }
    }
    best.map(|(_, r)| r)
}

/// SPEC §17.x (v0.3) — Validate `flowgate.authoring.*` preferences. v1
/// surface is one optional field, `preferred_script_language`. Adding
/// more authoring preferences later means adding more checks here.
/// All authoring preferences are advisory — surfaced to LLMs via
/// template substitution, never enforced.
fn validate_authoring_preferences(config: &Value) -> anyhow::Result<()> {
    let Some(authoring) = config.pointer("/flowgate/authoring") else {
        return Ok(());
    };
    let Some(obj) = authoring.as_object() else {
        bail!(
            "INVALID_AUTHORING_PREFERENCE: `flowgate.authoring` must be an object \
             ({})",
            short_value_kind(authoring)
        );
    };
    if let Some(lang) = obj.get("preferred_script_language") {
        match lang {
            Value::String(s) if !s.is_empty() => {}
            Value::String(_) => bail!(
                "INVALID_AUTHORING_PREFERENCE: `flowgate.authoring.preferred_script_language` \
                 is empty. Either set a non-empty string (e.g. `bash`, `python3`, `powershell`) \
                 or omit the key entirely."
            ),
            other => bail!(
                "INVALID_AUTHORING_PREFERENCE: `flowgate.authoring.preferred_script_language` \
                 must be a string ({})",
                short_value_kind(other)
            ),
        }
    }
    Ok(())
}

/// SPEC §17.x (v0.3) — Stamp `flowgate.authoring` onto every workflow
/// snapshot as `_authoringPrefs` so template substitution can reach the
/// preferences at render time via `{{$.flowgate.authoring.*}}`. The
/// snapshot is self-contained (SPEC §8.2): an in-flight instance sees
/// the preferences that existed at `workflow.start`, not whatever the
/// live config currently says.
///
/// Cheap by design: the authoring block is typically a small map
/// (one or a few key/value pairs). The duplication cost across workflows
/// is negligible; the alternative — plumbing the live config Arc
/// through the template resolver — would add far more surface than this
/// saves.
fn stamp_authoring_preferences(config: &mut Value) {
    let prefs = match config.pointer("/flowgate/authoring") {
        Some(p) if !p.as_object().map(|m| m.is_empty()).unwrap_or(true) => p.clone(),
        _ => return,
    };
    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for def in workflows.values_mut() {
        let Some(obj) = def.as_object_mut() else { continue };
        obj.insert("_authoringPrefs".into(), prefs.clone());
    }
}

/// SPEC §21 — Validate that every `states.<name>.delegate` value (when
/// present) is a non-empty string. The runtime treats the field as a
/// pass-through pointer; shape-validation here means `runtime_response.rs`
/// never has to defend against `null`/`""`/numeric values reaching the
/// response surface. Returns `INVALID_DELEGATE` naming the workflow + state.
fn validate_state_delegate(config: &Value) -> anyhow::Result<()> {
    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf_def) in workflows {
        let Some(states) = wf_def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };
        for (state_name, state_def) in states {
            let Some(value) = state_def.get("delegate") else {
                continue;
            };
            match value {
                Value::String(s) if !s.is_empty() => {}
                Value::String(_) => bail!(
                    "INVALID_DELEGATE: workflow '{wf_id}' state '{state_name}' \
                     has empty `delegate`. Must be a non-empty agent-config name (SPEC §21)."
                ),
                _ => bail!(
                    "INVALID_DELEGATE: workflow '{wf_id}' state '{state_name}' \
                     has non-string `delegate` ({}). Must be a non-empty string naming \
                     an agent config (SPEC §21).",
                    short_value_kind(value)
                ),
            }
        }
    }
    Ok(())
}

/// Short human-readable name for a JSON value's kind. Used by error messages
/// that quote a config-shape mismatch.
fn short_value_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// SPEC §8.4 + §20.2 — `flowgate.*` flags (e.g. `flowgate.authoring.write_enabled`,
/// `flowgate.strict_namespacing`) are read only at gateway startup. They MUST
/// NOT appear nested inside any `workflows.<id>` definition — otherwise an
/// LLM-authored workflow could embed a key intending to flip a runtime
/// invariant.
///
/// This validator walks every workflow definition recursively and rejects
/// any object key literally named `flowgate` OR starting with `flowgate.`,
/// returning `CONFIG_FLAG_NOT_RUNTIME_MUTABLE` with the exact JSON Pointer
/// path to the offending key.
fn validate_workflow_flag_scope(config: &Value) -> anyhow::Result<()> {
    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return Ok(());
    };
    for (wf_id, wf_def) in workflows {
        let base = format!("/workflows/{wf_id}");
        check_no_flowgate_keys(wf_def, &base)?;
    }
    Ok(())
}

/// Recursively walk `value` looking for any object key literally `flowgate`
/// or starting with `flowgate.`. Returns a CONFIG_FLAG_NOT_RUNTIME_MUTABLE
/// error naming the JSON-Pointer path of the first offender.
fn check_no_flowgate_keys(value: &Value, path: &str) -> anyhow::Result<()> {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == "flowgate" || k.starts_with("flowgate.") {
                    bail!(
                        "CONFIG_FLAG_NOT_RUNTIME_MUTABLE: key '{k}' at '{path}' \
                         — `flowgate.*` flags are read at gateway startup only and \
                         MUST NOT appear inside `workflows:` (SPEC §8.4)."
                    );
                }
                let child_path = format!("{path}/{k}");
                check_no_flowgate_keys(v, &child_path)?;
            }
            Ok(())
        }
        Value::Array(arr) => {
            for (i, v) in arr.iter().enumerate() {
                let child_path = format!("{path}/{i}");
                check_no_flowgate_keys(v, &child_path)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_skills(config: &Value, diagnostics: &mut Vec<Diagnostic>) -> anyhow::Result<()> {
    let Some(skills) = config.pointer("/skills").and_then(Value::as_object) else {
        return Ok(());
    };
    let strict_ns = strict_namespacing(config);
    for (subject, entry) in skills {
        // SPEC §5.4.2 — subject must be dotted-namespaced. The dotted pattern
        // is enforced regardless of strict_namespacing; only the *blessed
        // root* check is governed by the flag.
        if subject.trim().is_empty() {
            bail!("EMPTY_SUBJECT: skills key is empty after trim");
        }
        if !is_subject_pattern(subject) {
            bail!(
                "skills key '{subject}' must match ^[a-z][a-z0-9-]+(\\.[a-z][a-z0-9-]+)+$ \
                 — lowercase, kebab, dotted, at least two segments, no whitespace (SPEC §5.4.2)"
            );
        }
        // First-segment blessed-root check. Under strict_namespacing
        // (default true), an unblessed root is a hard error; otherwise
        // (SPEC §5.4.2 / audit-resolution C.2) it's a soft warning
        // pushed into the diagnostics collector.
        let root = strip_namespace_prefix(subject).split('.').next().unwrap_or("");
        if !BLESSED_SUBJECT_ROOTS.contains(&root) {
            if strict_ns {
                bail!(
                    "INVALID_SUBJECT_ROOT: skills key '{subject}' has unblessed root '{root}'; \
                     blessed roots are {:?} (SPEC §5.4.2). Disable with `flowgate.strict_namespacing: false`.",
                    BLESSED_SUBJECT_ROOTS
                );
            } else {
                let suggestion = closest_blessed_root(root).map(|sugg| {
                    format!("did you mean '{sugg}'?")
                });
                diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warn,
                    code: "INVALID_SUBJECT_ROOT".to_string(),
                    message: format!(
                        "skills key '{subject}' has unblessed root '{root}'; \
                         blessed roots are {:?}",
                        BLESSED_SUBJECT_ROOTS
                    ),
                    location: Some(format!("/skills/{subject}")),
                    suggestion,
                });
            }
        }

        // SPEC §5.4.1 — `verb` is a closed enum.
        let verb_str = entry
            .get("verb")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("MISSING_VERB: skills entry '{subject}' is missing a `verb`"))?;
        if Verb::from_token(verb_str).is_none() {
            bail!(
                "INVALID_VERB: skills entry '{subject}' has verb '{verb_str}'; \
                 allowed verbs are {:?} (SPEC §5.4.1)",
                Verb::ALL_TOKENS
            );
        }

        // SPEC §5.3 — `lifecycle` is required, closed enum, no silent default.
        let lifecycle_str = entry
            .get("lifecycle")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "MISSING_LIFECYCLE: skills entry '{subject}' is missing a `lifecycle` field; \
                     allowed values are {:?} (SPEC §5.3)",
                    Lifecycle::ALL_TOKENS
                )
            })?;
        if Lifecycle::from_token(lifecycle_str).is_none() {
            bail!(
                "INVALID_LIFECYCLE: skills entry '{subject}' has lifecycle '{lifecycle_str}'; \
                 allowed values are {:?} (SPEC §5.3)",
                Lifecycle::ALL_TOKENS
            );
        }

        // Body required (no silent default).
        let body = entry.get("body").and_then(Value::as_str).ok_or_else(|| {
            anyhow!("MISSING_BODY: skills entry '{subject}' is missing a `body` string")
        })?;

        // SPEC §5.7 — if author provided a pre-computed hash, it must match
        // the normalized body hash. Authors aren't required to provide one;
        // we compute it at stamp time. But if present, mismatch is fail-fast.
        if let Some(stored_hash) = entry.get("hash").and_then(Value::as_str) {
            let computed = compute_skill_hash(body);
            if stored_hash != computed {
                bail!(
                    "HASH_MISMATCH: skills entry '{subject}' has stored hash '{stored_hash}' \
                     but normalize_for_hash(body) produced '{computed}' (SPEC §5.7)"
                );
            }
        }
    }
    Ok(())
}

/// SPEC §22 — validate the top-level `scripts:` block. Mirrors
/// [`validate_skills`] in shape with three key differences:
///
/// 1. **Verb vocabulary** is the [`ScriptVerb`] closed enum (build/test/
///    deploy/format/lint/install/verify/run), not [`Verb`].
/// 2. **Blessed roots** come from [`BLESSED_SCRIPT_ROOTS`], not
///    [`BLESSED_SUBJECT_ROOTS`].
/// 3. **Body source is XOR**: either inline `body: string` OR external
///    `{ uri: string, hash: string }`. v1 supports `file://` URIs only.
fn validate_scripts(config: &Value, diagnostics: &mut Vec<Diagnostic>) -> anyhow::Result<()> {
    let Some(scripts) = config.pointer("/scripts").and_then(Value::as_object) else {
        return Ok(());
    };
    let strict_ns = strict_namespacing(config);
    for (subject, entry) in scripts {
        // Subject shape — same pattern as skills.
        if subject.trim().is_empty() {
            bail!("EMPTY_SCRIPT_SUBJECT: scripts key is empty after trim");
        }
        if !is_subject_pattern(subject) {
            bail!(
                "scripts key '{subject}' must match ^[a-z][a-z0-9-]+(\\.[a-z][a-z0-9-]+)+$ \
                 — lowercase, kebab, dotted, at least two segments, no whitespace (SPEC §22.4)"
            );
        }
        let root = strip_namespace_prefix(subject).split('.').next().unwrap_or("");
        if !BLESSED_SCRIPT_ROOTS.contains(&root) {
            if strict_ns {
                bail!(
                    "INVALID_SCRIPT_SUBJECT_ROOT: scripts key '{subject}' has unblessed root '{root}'; \
                     blessed roots are {:?} (SPEC §22.4). Disable with `flowgate.strict_namespacing: false`.",
                    BLESSED_SCRIPT_ROOTS
                );
            } else {
                let suggestion = closest_blessed_script_root(root).map(|sugg| {
                    format!("did you mean '{sugg}'?")
                });
                diagnostics.push(Diagnostic {
                    severity: DiagnosticSeverity::Warn,
                    code: "INVALID_SCRIPT_SUBJECT_ROOT".to_string(),
                    message: format!(
                        "scripts key '{subject}' has unblessed root '{root}'; \
                         blessed roots are {:?}",
                        BLESSED_SCRIPT_ROOTS
                    ),
                    location: Some(format!("/scripts/{subject}")),
                    suggestion,
                });
            }
        }

        // SPEC §22.3 — `verb` is a closed enum, distinct from cognitive Verb.
        let verb_str = entry
            .get("verb")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!("MISSING_SCRIPT_VERB: scripts entry '{subject}' is missing a `verb`")
            })?;
        if ScriptVerb::from_token(verb_str).is_none() {
            bail!(
                "INVALID_SCRIPT_VERB: scripts entry '{subject}' has verb '{verb_str}'; \
                 allowed verbs are {:?} (SPEC §22.3)",
                ScriptVerb::ALL_TOKENS
            );
        }

        // Lifecycle — same shape as skills; the Lifecycle enum is shared.
        let lifecycle_str = entry
            .get("lifecycle")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "MISSING_SCRIPT_LIFECYCLE: scripts entry '{subject}' is missing a `lifecycle` field; \
                     allowed values are {:?} (SPEC §22)",
                    Lifecycle::ALL_TOKENS
                )
            })?;
        if Lifecycle::from_token(lifecycle_str).is_none() {
            bail!(
                "INVALID_SCRIPT_LIFECYCLE: scripts entry '{subject}' has lifecycle '{lifecycle_str}'; \
                 allowed values are {:?} (SPEC §22)",
                Lifecycle::ALL_TOKENS
            );
        }

        // SPEC §22.2 — body source XOR: inline body OR external uri+hash.
        let body_inline = entry.get("body").and_then(Value::as_str);
        let uri = entry.get("uri").and_then(Value::as_str);
        match (body_inline, uri) {
            (Some(_), Some(_)) => bail!(
                "SCRIPT_BODY_SOURCE_AMBIGUOUS: scripts entry '{subject}' declares both \
                 `body` and `uri` — exactly one is required (SPEC §22.2)"
            ),
            (None, None) => bail!(
                "SCRIPT_BODY_SOURCE_AMBIGUOUS: scripts entry '{subject}' declares neither \
                 `body` nor `uri` — exactly one is required (SPEC §22.2)"
            ),
            (Some(body), None) => {
                // Inline body: hash is OPTIONAL (computed at stamp time).
                // If author provided one, it must match.
                if let Some(stored_hash) = entry.get("hash").and_then(Value::as_str) {
                    validate_hash_format(stored_hash, subject)?;
                    let computed = compute_script_hash(body);
                    if stored_hash != computed {
                        bail!(
                            "SCRIPT_HASH_MISMATCH: scripts entry '{subject}' has stored hash \
                             '{stored_hash}' but normalize_for_script_hash(body) produced \
                             '{computed}' (SPEC §22.2). Script hashing collapses trailing \
                             newlines only; internal whitespace is preserved."
                        );
                    }
                }
            }
            (None, Some(uri_str)) => {
                // External body: hash is REQUIRED (we verify at stamp time).
                let stored_hash = entry
                    .get("hash")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!(
                            "MISSING_SCRIPT_HASH: scripts entry '{subject}' uses an external \
                             `uri` but has no `hash`. Hash is required for uri-sourced bodies \
                             so the runtime can verify content-identity at load time (SPEC §22.2)."
                        )
                    })?;
                validate_hash_format(stored_hash, subject)?;
                if !(uri_str.starts_with("file://")
                    || uri_str.starts_with("https://")
                    || uri_str.starts_with("git+https://"))
                {
                    let scheme = uri_str.split("://").next().unwrap_or(uri_str);
                    bail!(
                        "UNSUPPORTED_SCRIPT_URI_SCHEME: scripts entry '{subject}' uri \
                         '{uri_str}' uses scheme '{scheme}://' — supported schemes are \
                         `file://` (relative to config), `https://` (load-time fetch), \
                         and `git+https://...@<ref>#<path>` (load-time `git archive` \
                         extraction). All non-file URIs require sha256 verification per \
                         SPEC §22.2."
                    );
                }
                if uri_str.starts_with("git+https://") {
                    // Cheap structural check at validate time — we want
                    // the load-time error if the shape is wrong, not a
                    // confusing git CLI failure later. Required form:
                    // git+https://<host>/<repo>(.git)?@<ref>#<path>
                    let body = uri_str.trim_start_matches("git+https://");
                    let (repo_at_ref, _path) = body.split_once('#').ok_or_else(|| {
                        anyhow!(
                            "INVALID_GIT_HTTPS_URI: scripts entry '{subject}' uri \
                             '{uri_str}' is missing the `#<path>` fragment. Required \
                             form: git+https://<host>/<repo>(.git)?@<ref>#<path>"
                        )
                    })?;
                    if !repo_at_ref.contains('@') {
                        bail!(
                            "INVALID_GIT_HTTPS_URI: scripts entry '{subject}' uri \
                             '{uri_str}' is missing the `@<ref>` revision. Required \
                             form: git+https://<host>/<repo>(.git)?@<ref>#<path>. \
                             Pinning to a ref is mandatory — branches drift, tags can \
                             be re-pointed, only a SHA or signed tag is reproducible."
                        );
                    }
                }
                // file:// resolution + hash verification happens at
                // stamp_scripts_library time (Tranche N) — needs the config
                // file path for relative-path resolution, which validate
                // doesn't have. https:// is also resolved there (no
                // base-dir rewrite needed; URLs are already absolute).
                // Shape is locked here; integrity is enforced there.
            }
        }
    }
    Ok(())
}

/// Validate that `s` matches `^sha256:[0-9a-f]{64}$` — the only hash format
/// the script library accepts. Future-proofed by making this a check, not a
/// hard parser: if we add `sha512:` later we update this in one place.
fn validate_hash_format(s: &str, subject: &str) -> anyhow::Result<()> {
    if !s.starts_with("sha256:") {
        bail!(
            "INVALID_SCRIPT_HASH_FORMAT: scripts entry '{subject}' hash '{s}' is missing \
             the `sha256:` prefix. Expected `sha256:<64-hex-chars>` (SPEC §22.2)."
        );
    }
    let hex = &s["sha256:".len()..];
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        bail!(
            "INVALID_SCRIPT_HASH_FORMAT: scripts entry '{subject}' hash '{s}' has malformed \
             digest. Expected `sha256:<64 lowercase hex chars>` (SPEC §22.2)."
        );
    }
    Ok(())
}

/// SPEC §22.2 — closest-blessed-script-root suggestion for the lenient
/// namespacing diagnostic. Mirror of [`closest_blessed_root`].
fn closest_blessed_script_root(candidate: &str) -> Option<&'static str> {
    if candidate.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &'static str)> = None;
    for root in BLESSED_SCRIPT_ROOTS {
        let shared = candidate
            .chars()
            .zip(root.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if shared == 0 {
            continue;
        }
        if best.map(|(b, _)| shared > b).unwrap_or(true) {
            best = Some((shared, root));
        }
    }
    best.map(|(_, r)| r)
}

/// SPEC §22.2 — script body normalization. **Stricter than
/// [`normalize_for_hash`]** because shell scripts treat whitespace as
/// load-bearing: `if [[ $x == "y" ]]` and `if [[ $x  ==  "y" ]]` are
/// different programs, and `\t` vs spaces matters for heredocs.
///
/// Rules:
/// 1. Preserve all internal whitespace exactly (no collapse).
/// 2. Collapse trailing newlines to exactly one terminal newline (so
///    `script\n` and `script\n\n\n` hash identically — editor-dependent
///    trailing-newline drift shouldn't break content-identity).
/// 3. No leading-whitespace trim (scripts may legitimately start with `#!`
///    on column 0 or with whitespace for indentation).
///
/// This stricter rule means inline `body: |` YAML scripts are hashed
/// verbatim modulo trailing newlines. Authors who edit a script body in
/// place (changing a tab to spaces, say) WILL get a SCRIPT_HASH_MISMATCH
/// when a uri-source script references that body — by design.
///
/// ```
/// use mcp_flowgate_core::config::normalize_for_script_hash;
///
/// // Internal whitespace preserved.
/// assert_eq!(normalize_for_script_hash("if [[  x ]]"), "if [[  x ]]\n");
/// // Trailing newlines collapsed to one.
/// assert_eq!(normalize_for_script_hash("echo hi\n\n\n"), "echo hi\n");
/// // Single trailing newline preserved.
/// assert_eq!(normalize_for_script_hash("echo hi\n"), "echo hi\n");
/// // No trailing newline -> one added.
/// assert_eq!(normalize_for_script_hash("echo hi"), "echo hi\n");
/// ```
pub fn normalize_for_script_hash(body: &str) -> String {
    // Strip all trailing newlines first.
    let mut s = body.to_string();
    while s.ends_with('\n') {
        s.pop();
    }
    // Re-append exactly one terminal newline.
    s.push('\n');
    s
}

/// SPEC §22.2 — content-identity hash for a script body. Pair with
/// [`normalize_for_script_hash`] always; never hash raw bytes.
///
/// ```
/// use mcp_flowgate_core::config::compute_script_hash;
///
/// // Trailing-newline drift produces identical hashes.
/// assert_eq!(
///     compute_script_hash("echo hi\n"),
///     compute_script_hash("echo hi\n\n\n"),
/// );
/// // But internal whitespace changes produce different hashes (unlike skill hash).
/// assert_ne!(
///     compute_script_hash("if [[ x ]]"),
///     compute_script_hash("if [[  x  ]]"),
/// );
/// // Hash carries algorithm prefix + lowercase-hex digest.
/// let h = compute_script_hash("echo hi");
/// assert!(h.starts_with("sha256:"));
/// assert_eq!(h.len(), "sha256:".len() + 64);
/// ```
pub fn compute_script_hash(body: &str) -> String {
    let normalized = normalize_for_script_hash(body);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("sha256:{:x}", digest)
}

/// SPEC §5.4.2 — subject pattern: dotted, lowercase-kebab segments, at least
/// two segments (`a.b`), no whitespace. Does NOT enforce blessed-root; that's
/// a separate check governed by `strict_namespacing`.
///
/// SPEC §9 — accepts an optional single-segment namespace prefix
/// (`<ns>/<a>.<b>`) for skills loaded via a `repos:` manifest.
/// Bare-subject form remains the canonical shape for skills declared
/// directly in the gateway config.
fn is_subject_pattern(s: &str) -> bool {
    let body = strip_namespace_prefix(s);
    let parts: Vec<&str> = body.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|p| is_kebab_token(p))
}

/// SPEC §9 — return the post-prefix portion of a subject string. If `s`
/// has a single leading `<ns>/` segment (kebab-token namespace), strip
/// it; otherwise return the input unchanged. Used by the blessed-root
/// check so namespace-prefixed subjects like `cognitive/plan.draft`
/// are evaluated against the root `plan`, not `cognitive`.
fn strip_namespace_prefix(s: &str) -> &str {
    match s.split_once('/') {
        Some((ns, rest)) if is_kebab_token(ns) => rest,
        _ => s,
    }
}

/// SPEC §5.4.2 — under `strict_namespacing: true` (default), an unblessed
/// subject root is a hard error. With `false`, it's a warning surfaced via
/// the `check` diagnostics layer. Top-level `flowgate.strict_namespacing`
/// only — schema must reject this flag at workflow scope
/// (`CONFIG_FLAG_NOT_RUNTIME_MUTABLE`); enforcement happens in `resolve`.
fn strict_namespacing(config: &Value) -> bool {
    config
        .pointer("/flowgate/strict_namespacing")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

/// SPEC §5.7 — body normalization rule applied before hashing.
///
/// 1. Trim leading and trailing whitespace.
/// 2. Replace each run of internal whitespace (spaces, tabs, newlines) of
///    length ≥1 with a single space.
/// 3. Strip a trailing newline if any remains after step 2.
///
/// This is the **single source-of-truth** function for hash normalization.
/// Every component that hashes a body MUST call this; read-side and
/// write-side parity is enforced by cross-impl test.
///
/// ```
/// use mcp_flowgate_core::config::normalize_for_hash;
///
/// assert_eq!(normalize_for_hash("  hello   world  "), "hello world");
/// assert_eq!(normalize_for_hash("a\n\nb"), "a b");
/// assert_eq!(normalize_for_hash("trailing\n\n"), "trailing");
/// // Idempotent: re-normalizing produces the same output.
/// let once = normalize_for_hash("  a b  c\n");
/// assert_eq!(normalize_for_hash(&once), once);
/// ```
pub fn normalize_for_hash(body: &str) -> String {
    let trimmed = body.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut in_ws_run = false;
    for c in trimmed.chars() {
        if c.is_whitespace() {
            if !in_ws_run {
                out.push(' ');
                in_ws_run = true;
            }
        } else {
            out.push(c);
            in_ws_run = false;
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// SPEC §5.7 — `sha256:` prefix + lowercase-hex digest of the normalized
/// body's UTF-8 bytes. Pair this with [`normalize_for_hash`] always — never
/// hash raw bytes.
///
/// ```
/// use mcp_flowgate_core::config::compute_skill_hash;
///
/// // Whitespace-only differences MUST produce the same hash (normalization
/// // is the whole point — read-side and write-side must agree).
/// assert_eq!(
///     compute_skill_hash("hello world"),
///     compute_skill_hash("  hello   world  "),
/// );
///
/// // Hash always carries the algorithm prefix and a lowercase-hex digest.
/// let h = compute_skill_hash("anything");
/// assert!(h.starts_with("sha256:"));
/// assert_eq!(h.len(), "sha256:".len() + 64);
/// ```
pub fn compute_skill_hash(body: &str) -> String {
    let normalized = normalize_for_hash(body);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("sha256:{:x}", digest)
}

fn stamp_skills_library(config: &mut Value) {
    let full_library: Map<String, Value> =
        match config.pointer("/skills").and_then(Value::as_object) {
            Some(skills) if !skills.is_empty() => {
                // SPEC §8.2: the snapshot is self-contained — it carries the
                // resolved fragment bodies the workflow references, not just
                // the verb. Editing the top-level `skills:` block cannot
                // mutate what an in-flight instance sees.
                let mut lib = Map::new();
                for (subject, entry) in skills {
                    // validate_skills already enforced these are present
                    // and well-typed; unwrap_or_default would mask drift, so
                    // we re-read defensively but with explicit shape.
                    let Some(verb) = entry.get("verb").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(lifecycle) = entry.get("lifecycle").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(body) = entry.get("body").and_then(Value::as_str) else {
                        continue;
                    };
                    let hash = compute_skill_hash(body);
                    let source = entry
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or("config");
                    lib.insert(
                        subject.clone(),
                        json!({
                            "verb":      verb,
                            "lifecycle": lifecycle,
                            "body":      body,
                            "hash":      hash,
                            "source":    source,
                        }),
                    );
                }
                lib
            }
            _ => return,
        };

    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            let Some(obj) = def.as_object_mut() else { continue };
            let referenced = collect_referenced_subjects(obj);
            if referenced.is_empty() {
                continue;
            }
            let mut scoped = Map::new();
            for subject in &referenced {
                if let Some(entry) = full_library.get(subject) {
                    scoped.insert(subject.clone(), entry.clone());
                }
            }
            // Skip stamping if none of the referenced subjects resolve — the
            // check pass reports those dangling refs as errors; no need to
            // bloat the snapshot with an empty library.
            if !scoped.is_empty() {
                obj.insert("_skillsLibrary".into(), Value::Object(scoped));
            }
        }
    }
}

/// SPEC §22 — stamp `_scriptsLibrary` onto each workflow that references a
/// curated script, mirroring [`stamp_skills_library`]. Resolution policy:
///
/// - Inline `body:` scripts → body stamped verbatim; hash computed.
/// - `uri:` scripts → file:// URIs resolved at load time (path is already
///   absolute by the time we get here, courtesy of [`rewrite_script_uris_to_absolute`]).
///   Body materialized into the snapshot; the declared `hash` is verified
///   against `compute_script_hash(resolved_body)`. Mismatch → `SCRIPT_HASH_MISMATCH`.
///
/// The instance-snapshot invariant (SPEC §8.2) holds for scripts the same way
/// it does for skills: editing the top-level `scripts:` block — or the
/// external file — after `workflow.start` cannot mutate what an in-flight
/// instance sees.
fn stamp_scripts_library(config: &mut Value) -> anyhow::Result<()> {
    let scripts = match config.pointer("/scripts").and_then(Value::as_object) {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return Ok(()),
    };
    let mut full_library: Map<String, Value> = Map::new();
    for (subject, entry) in &scripts {
        // validate_scripts has already enforced shape; re-read defensively.
        let Some(verb) = entry.get("verb").and_then(Value::as_str) else {
            continue;
        };
        let Some(lifecycle) = entry.get("lifecycle").and_then(Value::as_str) else {
            continue;
        };
        let source = entry
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("config")
            .to_string();

        // Materialize body (inline or uri). Hash verification on uri path.
        let (body, hash) = match (
            entry.get("body").and_then(Value::as_str),
            entry.get("uri").and_then(Value::as_str),
        ) {
            (Some(b), None) => (b.to_string(), compute_script_hash(b)),
            (None, Some(uri)) => {
                let declared_hash = entry
                    .get("hash")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!(
                            "scripts entry '{subject}' uri-source without hash reached \
                             stamp_scripts_library — validate_scripts should have caught this"
                        )
                    })?
                    .to_string();
                let body = read_script_uri(uri, subject)?;
                let computed = compute_script_hash(&body);
                if computed != declared_hash {
                    bail!(
                        "SCRIPT_HASH_MISMATCH: scripts entry '{subject}' uri '{uri}' \
                         resolved to a body whose content-hash is '{computed}' but the \
                         declared hash is '{declared_hash}'. Either the external source \
                         has drifted since the workflow was authored, or the declared \
                         hash is wrong (SPEC §22.2)."
                    );
                }
                (body, declared_hash)
            }
            // Both / neither — validate_scripts has already errored.
            _ => continue,
        };

        full_library.insert(
            subject.clone(),
            json!({
                "verb":      verb,
                "lifecycle": lifecycle,
                "body":      body,
                "hash":      hash,
                "source":    source,
            }),
        );
    }

    if let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    {
        for def in workflows.values_mut() {
            let Some(obj) = def.as_object_mut() else { continue };
            let referenced = collect_referenced_script_subjects(obj);
            if referenced.is_empty() {
                continue;
            }
            let mut scoped = Map::new();
            for subject in &referenced {
                if let Some(entry) = full_library.get(subject) {
                    scoped.insert(subject.clone(), entry.clone());
                }
            }
            if !scoped.is_empty() {
                obj.insert("_scriptsLibrary".into(), Value::Object(scoped));
            }
        }
    }
    Ok(())
}

/// Walk a workflow definition and collect every subject named by a
/// `script` executor — workflow-level `onEnter`, state-level `onEnter`,
/// and every transition's `executor`. Parallel to
/// [`collect_referenced_subjects`] but the harvest point is
/// `executor: { kind: script, subject: <name> }` rather than `skills: [...]`.
fn collect_referenced_script_subjects(workflow: &Map<String, Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_script_subject_from(workflow.get("onEnter"), &mut out);
    let Some(states) = workflow.get("states").and_then(Value::as_object) else {
        return out;
    };
    for state in states.values() {
        collect_script_subject_from(state.get("onEnter"), &mut out);
        let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for t in transitions.values() {
            collect_script_subject_from(t.get("executor"), &mut out);
        }
    }
    out
}

/// If `scope` is an executor block (or an action wrapping one) with
/// `kind: script`, push its `subject` into `out`. Tolerant of either
/// `{ kind, subject, ... }` directly or `{ executor: { kind, subject, ... } }`
/// nesting (the onEnter shape).
fn collect_script_subject_from(scope: Option<&Value>, out: &mut HashSet<String>) {
    let Some(v) = scope else { return };
    // Unwrap onEnter action wrapper.
    let executor = v
        .get("executor")
        .filter(|inner| inner.is_object())
        .unwrap_or(v);
    if executor.get("kind").and_then(Value::as_str) == Some("script") {
        if let Some(subj) = executor.get("subject").and_then(Value::as_str) {
            out.insert(subj.to_string());
        }
    }
}

/// SPEC §22.2 — read a script URI's contents. Dispatches by scheme:
/// - `file://` → local filesystem (absolute path post-rewrite).
/// - `https://` → blocking HTTP GET via reqwest::blocking. The
///   declared `hash:` is what makes this safe — we verify the
///   fetched bytes match the operator's declaration, so a hijacked
///   endpoint can't silently swap the script.
///
/// Other schemes are validator-rejected upstream; this function
/// errors on them as a defense-in-depth assertion.
fn read_script_uri(uri: &str, subject: &str) -> anyhow::Result<String> {
    if let Some(path) = uri.strip_prefix("file://") {
        return std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading scripts entry '{subject}' from {uri} (resolved path: {path})"
            )
        });
    }
    if uri.starts_with("https://") {
        return read_https_uri(uri, subject);
    }
    if uri.starts_with("git+https://") {
        return read_git_https_uri(uri, subject);
    }
    bail!(
        "UNSUPPORTED_SCRIPT_URI_SCHEME: scripts entry '{subject}' uri '{uri}' \
         reached read_script_uri with an unsupported scheme — validate_scripts \
         should have caught this. Supported: file://, https://, git+https://."
    )
}

/// Resolve a `git+https://<host>/<repo>(.git)?@<ref>#<path>` URI by
/// invoking `git archive --remote=<https-url> <ref> <path> | tar`.
/// This avoids a full clone — only the requested ref+path is fetched.
///
/// Many forges (GitHub, GitLab.com) do NOT support `git archive` over
/// https for security reasons (it's `git upload-archive` permission,
/// often disabled). When that's the case, this function emits a
/// `GIT_ARCHIVE_NOT_SUPPORTED` error suggesting the operator either
/// host the script via plain `https://` (raw.githubusercontent.com,
/// gist raw URL) or run a local mirror that allows `upload-archive`.
///
/// Hash-verified by the caller; we don't trust the network or git's
/// integrity guarantees — operator-declared sha256 is the gate.
fn read_git_https_uri(uri: &str, subject: &str) -> anyhow::Result<String> {
    let body = uri
        .strip_prefix("git+https://")
        .expect("validate_scripts ensures git+https:// prefix");
    let (repo_at_ref, path) = body
        .split_once('#')
        .ok_or_else(|| anyhow!("missing #<path> in {uri}"))?;
    let (repo, gitref) = repo_at_ref
        .rsplit_once('@')
        .ok_or_else(|| anyhow!("missing @<ref> in {uri}"))?;
    let repo_url = format!("https://{repo}");

    // `git archive --remote=<url> <ref> <path>` writes a tar to stdout.
    // We pipe to `tar -x -O -f -` to extract <path> only and dump
    // contents to stdout in one shot.
    //
    // Two child processes connected via a pipe. We capture tar's
    // stdout as the script body.
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut git = Command::new("git")
        .arg("archive")
        .arg("--format=tar")
        .arg(format!("--remote={repo_url}"))
        .arg(gitref)
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "spawning `git archive` for scripts entry '{subject}' uri '{uri}'. \
                 The `git` binary must be on PATH for git+https:// script URIs."
            )
        })?;

    let git_stdout = git.stdout.take().ok_or_else(|| {
        anyhow!("scripts entry '{subject}' git archive missing stdout pipe")
    })?;

    let mut tar = Command::new("tar")
        .arg("-x")
        .arg("-O")
        .arg("-f")
        .arg("-")
        .arg(path)
        .stdin(Stdio::from(git_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "spawning `tar` to extract scripts entry '{subject}' from git archive"
            )
        })?;

    let mut body = String::new();
    let mut tar_stdout = tar
        .stdout
        .take()
        .ok_or_else(|| anyhow!("tar missing stdout pipe"))?;
    tar_stdout.read_to_string(&mut body).with_context(|| {
        format!("reading scripts entry '{subject}' body from tar stdout")
    })?;

    let git_status = git.wait().with_context(|| {
        format!("waiting on `git archive` for scripts entry '{subject}'")
    })?;
    let tar_status = tar.wait().with_context(|| {
        format!("waiting on `tar` for scripts entry '{subject}'")
    })?;

    if !git_status.success() {
        bail!(
            "GIT_ARCHIVE_NOT_SUPPORTED: scripts entry '{subject}' uri '{uri}' — \
             `git archive --remote={repo_url}` exited with code {:?}. Many forges \
             (GitHub, GitLab.com) disable `upload-archive` over https for security. \
             Workarounds: host the script via plain https:// (e.g. \
             raw.githubusercontent.com/<owner>/<repo>/<ref>/<path>), or use a \
             self-hosted mirror that permits upload-archive.",
            git_status.code()
        );
    }
    if !tar_status.success() {
        bail!(
            "scripts entry '{subject}' uri '{uri}' — `tar -x -O` exited with code \
             {:?}. The git archive may not contain '{path}', or the path uses an \
             unsupported format.",
            tar_status.code()
        );
    }
    if body.is_empty() {
        bail!(
            "scripts entry '{subject}' uri '{uri}' resolved to an empty body. \
             Check that '{path}' exists in the repo at ref '{gitref}'."
        );
    }
    Ok(body)
}

/// Blocking HTTP GET for an `https://` script URI. 30-second hard
/// timeout (script bodies are small; long blocking calls at config
/// load are an operator-visible problem). Non-200 responses fail
/// with a clear error naming the URL + status code.
///
/// The fetched body is hash-verified by the caller; no need to
/// trust the network — operator-declared sha256 is the integrity gate.
fn read_https_uri(uri: &str, subject: &str) -> anyhow::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .user_agent(concat!("mcp-flowgate/", env!("CARGO_PKG_VERSION")))
        .build()
        .with_context(|| {
            format!("building blocking HTTP client for scripts entry '{subject}' {uri}")
        })?;
    let resp = client.get(uri).send().with_context(|| {
        format!("fetching scripts entry '{subject}' from {uri}")
    })?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "SCRIPT_URI_FETCH_FAILED: scripts entry '{subject}' uri '{uri}' returned \
             HTTP {} — expected 2xx. Caller may have moved/deleted the resource, or \
             the host requires authentication (not currently supported; the v1 https \
             fetcher is anonymous).",
            status.as_u16()
        );
    }
    resp.text().with_context(|| {
        format!("decoding body for scripts entry '{subject}' from {uri}")
    })
}

/// SPEC §22.2 — rewrite relative `file://` URIs in `scripts:` entries to
/// absolute paths, relative to `base_dir`. Called by `load_yaml_inner`
/// after parsing each YAML file so `resolve()` can stay path-agnostic.
///
/// Idempotent: an already-absolute `file:///etc/foo.sh` is left alone.
/// Non-`file://` URIs are left alone (the validator will reject them).
fn rewrite_script_uris_to_absolute(value: &mut Value, base_dir: &Path) {
    let Some(scripts) = value
        .pointer_mut("/scripts")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for entry in scripts.values_mut() {
        let Some(obj) = entry.as_object_mut() else { continue };
        let Some(uri_val) = obj.get_mut("uri") else { continue };
        let Some(uri_str) = uri_val.as_str() else { continue };
        let Some(rest) = uri_str.strip_prefix("file://") else { continue };
        if rest.starts_with('/') {
            // Already absolute.
            continue;
        }
        let abs = base_dir.join(rest);
        // Canonicalize when possible (cleans up `./` etc.); fall back to
        // join result for not-yet-existing paths so the load-time
        // file-read produces a clear NotFound rather than a canonicalize
        // error here.
        let final_path = abs.canonicalize().unwrap_or(abs);
        *uri_val = Value::String(format!("file://{}", final_path.display()));
    }
}

/// Walk a workflow definition and collect every subject named in any
/// `skills:` array — workflow, state, and transition scope (SPEC §5.5).
fn collect_referenced_subjects(workflow: &Map<String, Value>) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_skills_strings(workflow.get("skills"), &mut out);
    let Some(states) = workflow.get("states").and_then(Value::as_object) else {
        return out;
    };
    for state in states.values() {
        collect_skills_strings(state.get("skills"), &mut out);
        let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for t in transitions.values() {
            collect_skills_strings(t.get("skills"), &mut out);
        }
    }
    out
}

fn collect_skills_strings(scope: Option<&Value>, out: &mut HashSet<String>) {
    if let Some(arr) = scope.and_then(Value::as_array) {
        for entry in arr {
            if let Some(s) = entry.as_str() {
                out.insert(s.to_string());
            }
        }
    }
}

fn is_kebab_token(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Convenience: load + resolve in one call.
pub fn load_resolved(path: impl AsRef<Path>) -> anyhow::Result<Value> {
    resolve(load_yaml(path)?)
}

/// Convenience: load + resolve in one call, also returning any soft
/// diagnostics (SPEC §5.4.2 / audit-resolution C.2).
pub fn load_resolved_with_diagnostics(
    path: impl AsRef<Path>,
) -> anyhow::Result<(Value, Vec<Diagnostic>)> {
    resolve_with_diagnostics(load_yaml(path)?)
}

/// SPEC §9 — load + merge declared repos + resolve in one call.
///
/// Compared to [`load_resolved_with_diagnostics`], this variant additionally
/// honors top-level `repos: [{ path: <repo-root> }]` and `overrides: [<id>]`
/// blocks in the host config. Each repo is loaded via [`crate::repo::load_repo`],
/// its definitionIds prefixed `<namespace>/`, and merged into the gateway
/// registry BEFORE the host config's own entries — so the host can shadow a
/// repo-provided id only when it lists the id in `overrides:` (V23). Repos
/// declaring the same `namespace` fail at load (V20). After merging, every
/// `kind: workflow` `definitionId:` reference must resolve to a loaded entry
/// (V22).
///
/// Hosts with no `repos:` block behave exactly like
/// [`load_resolved_with_diagnostics`] — the wrapper is the new entrypoint
/// the binary should call regardless.
pub fn load_resolved_with_repos(
    path: impl AsRef<Path>,
) -> anyhow::Result<(Value, Vec<Diagnostic>)> {
    let path = path.as_ref();
    let host = load_yaml(path)?;
    let parent_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let merged = merge_declared_repos(host, &parent_dir)?;
    resolve_with_diagnostics(merged)
}

/// Extract `repos:` + `overrides:` from `host`, load each repo, validate
/// V20 / V21 / V22 / V23, deep-merge repo contents (then host on top so
/// declared overrides win), and return the cleaned value (with `repos:`
/// and `overrides:` stripped). Hosts without a `repos:` block round-trip
/// through unchanged.
fn merge_declared_repos(mut host: Value, host_dir: &Path) -> anyhow::Result<Value> {
    let (repos, overrides) = take_repos_and_overrides(&mut host)?;
    if repos.is_empty() {
        // No repos declared — strip an empty `overrides:` (it's meaningless
        // without any repo-provided ids to shadow) and return unchanged.
        return Ok(host);
    }

    let mut repo_aggregate = Value::Object(Map::new());
    let mut repo_provided_ids: HashSet<String> = HashSet::new();
    let mut seen_namespaces: HashMap<String, String> = HashMap::new();

    for repo_path in repos {
        // Relative paths resolve against the host config's directory — same
        // base-dir convention as `include:`.
        let repo_path = if repo_path.is_absolute() {
            repo_path
        } else {
            host_dir.join(repo_path)
        };
        let (manifest, repo_value) = crate::repo::load_repo(&repo_path).with_context(|| {
            format!("loading repo at {}", repo_path.display())
        })?;
        // V20 — namespace uniqueness across declared repos.
        if let Some(prev_name) =
            seen_namespaces.insert(manifest.namespace.clone(), manifest.name.clone())
        {
            bail!(
                "DUPLICATE_REPO_NAMESPACE: namespace '{}' is declared by repos '{}' and '{}'. \
                 Each repo MUST declare a unique namespace (SPEC §9.4).",
                manifest.namespace,
                prev_name,
                manifest.name
            );
        }
        for id in crate::repo::aggregate_ids(&repo_value) {
            repo_provided_ids.insert(id);
        }
        repo_aggregate = deep_merge(repo_aggregate, repo_value);
    }

    // V23 — any host-defined id that collides with a repo-provided id MUST
    // appear in the explicit `overrides:` block. This closes the supply-chain
    // backdoor: an operator cannot silently shadow a vendored definition.
    let host_ids = host_definition_ids(&host);
    let collisions: Vec<String> = host_ids
        .intersection(&repo_provided_ids)
        .cloned()
        .collect();
    for id in &collisions {
        if !overrides.contains(id) {
            bail!(
                "ANONYMOUS_OVERRIDE: '{id}' is provided by a declared repo and shadowed \
                 by the host config without an explicit `overrides:` entry. Add `{id}` to \
                 the top-level `overrides:` array to make the shadowing intentional \
                 (SPEC §9.4)."
            );
        }
    }
    // Any id listed in `overrides:` that doesn't actually collide is a
    // stale declaration — surface it as a hard error so authors aren't
    // misled into thinking they're shadowing something they aren't.
    for id in &overrides {
        if !repo_provided_ids.contains(id) {
            bail!(
                "STALE_OVERRIDE: `overrides:` lists '{id}', but no declared repo provides \
                 that id. Remove it or correct the namespace prefix (SPEC §9.4)."
            );
        }
    }

    // Repo contents first, host body last → host wins on the explicitly
    // declared overrides.
    let merged = deep_merge(repo_aggregate, host);

    // V22 — every `kind: workflow` definitionId reference in the merged
    // registry must resolve. References were namespace-prefixed inside
    // each repo's workflow bodies by `load_repo`; here we walk the final
    // registry and assert every `kind: workflow` ref binds.
    validate_workflow_refs_resolve(&merged)?;
    Ok(merged)
}

/// Remove the `repos:` and `overrides:` top-level keys from `host` and
/// return their parsed payloads. Errors on shape mismatches.
fn take_repos_and_overrides(
    host: &mut Value,
) -> anyhow::Result<(Vec<PathBuf>, HashSet<String>)> {
    let Some(obj) = host.as_object_mut() else {
        return Ok((Vec::new(), HashSet::new()));
    };
    let repos: Vec<PathBuf> = match obj.remove("repos") {
        None => Vec::new(),
        Some(Value::Array(arr)) => arr
            .into_iter()
            .enumerate()
            .map(|(i, entry)| parse_repo_entry(i, entry))
            .collect::<anyhow::Result<Vec<_>>>()?,
        Some(other) => bail!(
            "INVALID_REPOS_SHAPE: top-level `repos:` must be an array of `{{ path: <dir> }}` \
             objects ({})",
            short_value_kind(&other)
        ),
    };
    let overrides: HashSet<String> = match obj.remove("overrides") {
        None => HashSet::new(),
        Some(Value::Array(arr)) => arr
            .into_iter()
            .map(|entry| match entry {
                Value::String(s) if !s.is_empty() => Ok(s),
                Value::String(_) => bail!(
                    "INVALID_OVERRIDE_ENTRY: `overrides:` entries MUST be non-empty strings"
                ),
                other => bail!(
                    "INVALID_OVERRIDE_ENTRY: `overrides:` entries MUST be strings ({})",
                    short_value_kind(&other)
                ),
            })
            .collect::<anyhow::Result<HashSet<_>>>()?,
        Some(other) => bail!(
            "INVALID_OVERRIDES_SHAPE: top-level `overrides:` must be an array of \
             fully-qualified id strings ({})",
            short_value_kind(&other)
        ),
    };
    Ok((repos, overrides))
}

/// Parse one `repos:` array entry. Accepts `{ path: <string> }`; expands
/// `~/` to `$HOME` and `~` alone is treated literally (no expansion).
fn parse_repo_entry(index: usize, entry: Value) -> anyhow::Result<PathBuf> {
    let path_str = entry
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            anyhow!(
                "INVALID_REPO_ENTRY: `repos[{index}]` must be an object with a `path` field, \
                 e.g. `- path: ~/repos/swe-core`"
            )
        })?;
    Ok(expand_repo_path(path_str))
}

/// Expand a `~/`-prefixed path against `$HOME`. Returns the input unchanged
/// when no `~/` prefix is present or `$HOME` is unset (load-time error will
/// surface in `load_repo` instead, with the unresolved literal in the
/// message).
fn expand_repo_path(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Collect every host-declared definitionId across the four prefixable
/// blocks. Mirror of [`crate::repo::aggregate_ids`] for the host side.
fn host_definition_ids(host: &Value) -> HashSet<String> {
    let mut out = HashSet::new();
    let Some(obj) = host.as_object() else { return out };
    for block in ["workflows", "skills", "scripts", "connections"] {
        if let Some(entries) = obj.get(block).and_then(Value::as_object) {
            for k in entries.keys() {
                out.insert(k.clone());
            }
        }
    }
    out
}

/// SPEC §9.3 — after repo loading, every `kind: workflow` executor's
/// `definitionId:` reference must resolve to a loaded workflow. Unresolved
/// refs are V22 (likely an unprefixed cross-repo ref or a typo).
fn validate_workflow_refs_resolve(config: &Value) -> anyhow::Result<()> {
    let known: HashSet<String> = config
        .pointer("/workflows")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    if known.is_empty() {
        return Ok(());
    }
    let mut unresolved: Vec<(String, String)> = Vec::new();
    if let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) {
        for (wf_id, wf_def) in workflows {
            collect_unresolved_workflow_refs(wf_def, &known, wf_id, &mut unresolved);
        }
    }
    if let Some((wf_id, target)) = unresolved.first() {
        bail!(
            "UNRESOLVED_WORKFLOW_REF: workflow '{wf_id}' references definitionId '{target}' \
             via a `kind: workflow` executor, but no workflow with that id is loaded. \
             Unprefixed names resolve in the workflow's OWN namespace; to call into \
             another repo, fully qualify the id as `<namespace>/{target}` (SPEC §9.3)."
        );
    }
    Ok(())
}

fn collect_unresolved_workflow_refs(
    value: &Value,
    known: &HashSet<String>,
    wf_id: &str,
    out: &mut Vec<(String, String)>,
) {
    match value {
        Value::Object(map) => {
            let is_workflow_exec = map.get("kind").and_then(Value::as_str) == Some("workflow");
            if is_workflow_exec {
                if let Some(def_id) = map.get("definitionId").and_then(Value::as_str) {
                    if !known.contains(def_id) {
                        out.push((wf_id.to_string(), def_id.to_string()));
                    }
                }
            }
            for child in map.values() {
                collect_unresolved_workflow_refs(child, known, wf_id, out);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_unresolved_workflow_refs(v, known, wf_id, out);
            }
        }
        _ => {}
    }
}

/// Parse + resolve a YAML string in-process. Use this when the config is
/// embedded with `include_str!` so rules ship with the binary and end users
/// can't edit them. Multi-file `include:` directives won't work in this path
/// since there's no filesystem to walk — pre-merge with `deep_merge` if you
/// need that.
pub fn resolve_str(yaml: &str) -> anyhow::Result<Value> {
    let value: Value = serde_yaml::from_str(yaml).context("parsing embedded YAML")?;
    resolve(value)
}

// ---------- capability flattening -----------------------------------------

/// A normalized capability: executor + the union of all guards/reliability
/// down the wraps chain, plus carried metadata.
#[derive(Debug, Clone)]
struct NormalizedCapability {
    executor: Value,
    input_schema: Option<Value>,
    title: Option<String>,
    description: Option<String>,
    tags: Vec<Value>,
    examples: Vec<Value>,
    guards: Vec<Value>,
    reliability: Option<Value>,
}

fn flatten_capabilities(config: &Value) -> anyhow::Result<HashMap<String, NormalizedCapability>> {
    let Some(map) = config.pointer("/capabilities").and_then(Value::as_object) else {
        return Ok(HashMap::new());
    };

    let mut resolving = HashSet::new();
    let mut resolved: HashMap<String, NormalizedCapability> = HashMap::new();

    // Preserve declaration order so error messages reference the first one
    // a user hits.
    let names: Vec<String> = map.keys().cloned().collect();
    for name in names {
        flatten_one(&name, map, &mut resolving, &mut resolved)?;
    }
    Ok(resolved)
}

fn flatten_one(
    name: &str,
    raw: &Map<String, Value>,
    resolving: &mut HashSet<String>,
    resolved: &mut HashMap<String, NormalizedCapability>,
) -> anyhow::Result<()> {
    if resolved.contains_key(name) {
        return Ok(());
    }
    if !resolving.insert(name.to_string()) {
        bail!("capability `wraps` cycle detected at '{}'", name);
    }

    let def = raw
        .get(name)
        .ok_or_else(|| anyhow!("capability '{}' is referenced but not defined", name))?;

    let mut current = NormalizedCapability {
        executor: Value::Null,
        input_schema: def.get("inputSchema").cloned(),
        title: def.get("title").and_then(Value::as_str).map(str::to_string),
        description: def
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string),
        tags: def
            .get("tags")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        examples: def
            .get("examples")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        guards: def
            .get("guards")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        reliability: def.get("reliability").cloned(),
    };

    // If this capability wraps another, flatten the parent first and then
    // layer this one's guards / reliability on top. The parent provides the
    // executor unless this def overrides it.
    if let Some(parent_name) = def.get("wraps").and_then(Value::as_str) {
        flatten_one(parent_name, raw, resolving, resolved)?;
        let parent = resolved.get(parent_name).expect("just resolved").clone();
        current.executor = def
            .get("executor")
            .cloned()
            .unwrap_or(parent.executor.clone());
        current.input_schema = current.input_schema.or(parent.input_schema);
        current.title = current.title.or(parent.title);
        current.description = current.description.or(parent.description);
        // Tags/examples union (preserving order, parent first).
        let mut tags = parent.tags;
        tags.extend(current.tags);
        current.tags = tags;
        let mut examples = parent.examples;
        examples.extend(current.examples);
        current.examples = examples;
        // Guards stack: parent first, then wrapper's. Both must pass.
        let mut guards = parent.guards;
        guards.extend(current.guards);
        current.guards = guards;
        // Reliability: more specific (this def) wins; else inherit.
        current.reliability = current.reliability.or(parent.reliability);
    } else {
        current.executor = def
            .get("executor")
            .cloned()
            .ok_or_else(|| anyhow!("capability '{}' needs `executor` or `wraps`", name))?;
    }

    resolving.remove(name);
    resolved.insert(name.to_string(), current);
    Ok(())
}

// ---------- exposure rewriting --------------------------------------------

fn rewrite_exposure(
    exposure: Value,
    registry: &HashMap<String, NormalizedCapability>,
) -> anyhow::Result<Value> {
    let Some(obj) = exposure.as_object() else {
        return Ok(exposure);
    };

    if let Some(cap_name) = obj.get("capability").and_then(Value::as_str) {
        let cap = registry
            .get(cap_name)
            .ok_or_else(|| anyhow!("exposure references unknown capability '{}'", cap_name))?;

        let alias = obj
            .get("as")
            .and_then(Value::as_str)
            .unwrap_or(cap_name)
            .to_string();

        let mut out = Map::new();
        out.insert("name".into(), Value::String(alias));
        if let Some(t) = &cap.title {
            out.insert("title".into(), Value::String(t.clone()));
        }
        let description = obj
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| cap.description.clone());
        if let Some(d) = description {
            out.insert("description".into(), Value::String(d));
        }
        if let Some(s) = &cap.input_schema {
            out.insert("inputSchema".into(), s.clone());
        }

        // Tags = capability tags ++ exposure's own.
        let mut tags = cap.tags.clone();
        if let Some(local) = obj.get("tags").and_then(Value::as_array) {
            tags.extend(local.iter().cloned());
        }
        if !tags.is_empty() {
            out.insert("tags".into(), Value::Array(tags));
        }

        // Guards = capability guards ++ exposure's own.
        let mut guards = cap.guards.clone();
        if let Some(local) = obj.get("guards").and_then(Value::as_array) {
            guards.extend(local.iter().cloned());
        }
        if !guards.is_empty() {
            out.insert("guards".into(), Value::Array(guards));
        }

        // Reliability: exposure overrides if specified.
        let reliability = obj.get("reliability").cloned().or(cap.reliability.clone());
        if let Some(r) = reliability {
            out.insert("reliability".into(), r);
        }

        out.insert("executor".into(), cap.executor.clone());

        return Ok(Value::Object(out));
    }

    Ok(exposure)
}

// ---------- executor reference rewriting ----------------------------------

fn rewrite_executors_in_value(
    value: &mut Value,
    registry: &HashMap<String, NormalizedCapability>,
) -> anyhow::Result<()> {
    match value {
        Value::Object(map) => {
            // If `executor` is itself a capability ref, rewrite this object
            // to merge the capability's guards/reliability into the parent.
            if let Some(executor) = map.get("executor").cloned() {
                if let Some(cap_name) = executor
                    .as_object()
                    .and_then(|o| o.get("capability"))
                    .and_then(Value::as_str)
                {
                    let cap = registry.get(cap_name).ok_or_else(|| {
                        anyhow!("executor references unknown capability '{}'", cap_name)
                    })?;
                    map.insert("executor".into(), cap.executor.clone());

                    // Stack guards: capability first, then existing.
                    let existing_guards = map
                        .get("guards")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut all_guards = cap.guards.clone();
                    all_guards.extend(existing_guards);
                    if !all_guards.is_empty() {
                        map.insert("guards".into(), Value::Array(all_guards));
                    }

                    // Reliability: parent (transition/etc.) wins if set, else
                    // capability's.
                    if !map.contains_key("reliability") {
                        if let Some(r) = &cap.reliability {
                            map.insert("reliability".into(), r.clone());
                        }
                    }

                    if !map.contains_key("inputSchema") {
                        if let Some(s) = &cap.input_schema {
                            map.insert("inputSchema".into(), s.clone());
                        }
                    }
                }
            }

            // Recurse into all children — covers transitions, onEnter,
            // fallback executors, etc.
            for child in map.values_mut() {
                rewrite_executors_in_value(child, registry)?;
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                rewrite_executors_in_value(v, registry)?;
            }
        }
        _ => {}
    }
    Ok(())
}
