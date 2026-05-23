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
use serde_json::{Map, Value};

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

/// Resolve `capabilities:`, `wraps:`, and `capability:` references into the
/// inline shapes the runtime expects. Idempotent — calling it twice is safe.
pub fn resolve(mut config: Value) -> anyhow::Result<Value> {
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
    validate_skills(&config)?;

    // 7. Stamp each workflow definition with `_skillsLibrary: { subject: verb }`
    //    drawn from the top-level `skills:` map (subjects only — verb, no body;
    //    body is fetched on demand via `gateway.describe`). Lets the runtime
    //    decorate `guidance.refs` from the per-instance snapshot alone without
    //    needing a side channel to the top-level config.
    stamp_skills_library(&mut config);

    Ok(config)
}

fn validate_skills(config: &Value) -> anyhow::Result<()> {
    let Some(skills) = config.pointer("/skills").and_then(Value::as_object) else {
        return Ok(());
    };
    for (subject, entry) in skills {
        if !is_kebab_token(subject) {
            bail!(
                "skills key '{subject}' must match ^[a-z][a-z0-9-]*$ — lowercase kebab, no whitespace (SPEC §5.4)"
            );
        }
        let verb = entry
            .get("verb")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("skills entry '{subject}' is missing a `verb`"))?;
        if !is_kebab_token(verb) {
            bail!(
                "skills entry '{subject}' has verb '{verb}' which must match ^[a-z][a-z0-9-]*$ — lowercase kebab, no whitespace (SPEC §5.4)"
            );
        }
        if entry.get("body").and_then(Value::as_str).is_none() {
            bail!("skills entry '{subject}' is missing a `body` string");
        }
    }
    Ok(())
}

fn stamp_skills_library(config: &mut Value) {
    let full_library: Map<String, Value> =
        match config.pointer("/skills").and_then(Value::as_object) {
            Some(skills) if !skills.is_empty() => {
                // Carry verb only — body lives in the top-level map and is fetched
                // via `gateway.describe`, not embedded per-workflow.
                let mut lib = Map::new();
                for (subject, entry) in skills {
                    if let Some(verb) = entry.get("verb").and_then(Value::as_str) {
                        lib.insert(subject.clone(), Value::String(verb.to_string()));
                    }
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
                if let Some(verb) = full_library.get(subject) {
                    scoped.insert(subject.clone(), verb.clone());
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
