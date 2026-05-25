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

use crate::discovery::{Lifecycle, Verb, BLESSED_SUBJECT_ROOTS};

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

    // 6b. SPEC §8.4 + §20.2 — reject runtime-only `flowgate.*` flags when
    //     they appear inside any `workflows:` block. The flags are read at
    //     gateway startup only; allowing them at workflow scope would let
    //     an LLM-authored workflow attempt to (silently) flip the bypass
    //     flag on for itself.
    validate_workflow_flag_scope(&config)?;

    // 7. Stamp each workflow definition with `_skillsLibrary: { subject: verb }`
    //    drawn from the top-level `skills:` map (subjects only — verb, no body;
    //    body is fetched on demand via `gateway.describe`). Lets the runtime
    //    decorate `guidance.refs` from the per-instance snapshot alone without
    //    needing a side channel to the top-level config.
    stamp_skills_library(&mut config);

    Ok((config, diagnostics))
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
        let root = subject.split('.').next().unwrap_or("");
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

/// SPEC §5.4.2 — subject pattern: dotted, lowercase-kebab segments, at least
/// two segments (`a.b`), no whitespace. Does NOT enforce blessed-root; that's
/// a separate check governed by `strict_namespacing`.
fn is_subject_pattern(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 {
        return false;
    }
    parts.iter().all(|p| is_kebab_token(p))
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
