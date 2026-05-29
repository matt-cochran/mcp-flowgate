//! SPEC §30 — Lexicon / Ubiquitous Language primitive (Tier 1).
//!
//! A persistent vocabulary store that workflows reach for via the
//! `gateway.lexicon.*` MCP tools. Each term carries a definition, an
//! optional bounded context (DDD), refs to related terms, and a
//! governance marker that defaults to `human-only`.
//!
//! ## Why a runtime primitive?
//!
//! A skill can extract terms via Socratic questioning. But to be
//! reusable across runs, the result needs a stable STORE that:
//!
//! 1. Snapshot-stamps onto in-flight workflows — same invariant as
//!    `_skillsLibrary` per SPEC §8.2. A workflow started before a term
//!    was redefined keeps its old understanding.
//! 2. Is searchable from any workflow via `gateway.lexicon.search`.
//! 3. Is human-governed by default — agents cannot silently drift
//!    vocabulary; they propose, humans accept.
//! 4. Is version-controllable — Tier 1 lives in `flowgate.yaml`,
//!    operators commit + review via PR.
//!
//! ## Tier 1 — Per-config
//!
//! The top-level `lexicon:` block in `flowgate.yaml` is the entire
//! store. At config-load, every workflow gets a stamped
//! `_lexiconLibrary` on its definition snapshot. The MCP tools read
//! from the snapshot (not the live config) so in-flight reads are
//! deterministic.
//!
//! Tier 2 (per-operator file store) and Tier 3 (multi-tenant DB)
//! follow the same shape; a `LexiconStore` trait is reserved but the
//! Tier 1 in-config form is the only one shipped today.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};

/// Default governance level when a lexicon entry omits the field.
/// SPEC §30.6 — `human-only` is the load-bearing default. Agents
/// proposing definitions get rejected so vocabulary doesn't drift
/// silently. Operators opting into `agent-may-propose` are making
/// an explicit choice to accept faster iteration over discipline.
pub const DEFAULT_GOVERNANCE: &str = "human-only";

/// Alias to keep the collision-detection map type readable.
type ContextEntries<'a> = Vec<(&'a str, &'a Map<String, Value>)>;

/// Validate the top-level `lexicon:` block at config load. Catches:
/// - non-object entries
/// - missing `definition_short` field
/// - invalid `governance` value
/// - non-string `refs` entries
/// - same-bounded-context alias collisions (SPEC §30.10.1)
///
/// Surfaces `INVALID_LEXICON_ENTRY` or `LEXICON_ALIAS_COLLISION` with the
/// offending term(s) named.
pub fn validate_lexicon(config: &Value) -> Result<()> {
    let Some(lexicon) = config.get("lexicon").and_then(Value::as_object) else {
        return Ok(()); // no lexicon block is fine
    };
    for (term, entry) in lexicon {
        let entry_obj = entry.as_object().ok_or_else(|| {
            anyhow!(
                "INVALID_LEXICON_ENTRY: lexicon entry '{term}' must be an object \
                 with at least `definition_short:` set"
            )
        })?;
        let definition = entry_obj
            .get("definition_short")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow!(
                    "INVALID_LEXICON_ENTRY: lexicon entry '{term}' is missing the \
                     required `definition:` field (string)"
                )
            })?;
        if definition.trim().is_empty() {
            bail!(
                "INVALID_LEXICON_ENTRY: lexicon entry '{term}' has empty \
                 `definition:` — definitions must be substantive"
            );
        }
        if let Some(gov) = entry_obj.get("governance").and_then(Value::as_str) {
            if gov != "human-only" && gov != "agent-may-propose" {
                bail!(
                    "INVALID_LEXICON_ENTRY: lexicon entry '{term}' has unknown \
                     `governance: {gov}` — supported: `human-only` (default) | \
                     `agent-may-propose`"
                );
            }
        }
        if let Some(refs) = entry_obj.get("refs").and_then(Value::as_array) {
            for (i, r) in refs.iter().enumerate() {
                if !r.is_string() {
                    bail!(
                        "INVALID_LEXICON_ENTRY: lexicon entry '{term}' refs[{i}] is not \
                         a string — refs must be term names"
                    );
                }
            }
        }
    }

    // ── SPEC §30.10.1 — same-bounded-context alias collision detection ────
    //
    // Group entries by bounded_context (empty string = no context).
    // Within each group build the combined-form index; if any alias or
    // canonical term appears more than once → LEXICON_ALIAS_COLLISION.
    let mut by_context: HashMap<&str, ContextEntries<'_>> = HashMap::new();
    for (term, entry) in lexicon {
        if let Some(obj) = entry.as_object() {
            let ctx = obj
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            by_context.entry(ctx).or_default().push((term.as_str(), obj));
        }
    }
    for (ctx, entries) in &by_context {
        build_combined_index_inner(entries, ctx)?;
    }
    Ok(())
}

/// Internal helper: build the combined-form index for a slice of entries
/// that share a bounded context. Returns an error on the first collision.
fn build_combined_index_inner<'a>(
    entries: &[(&'a str, &'a Map<String, Value>)],
    bounded_context: &str,
) -> Result<HashMap<&'a str, &'a Map<String, Value>>> {
    let mut index: HashMap<&str, (&str, &Map<String, Value>)> = HashMap::new();

    let register = |key: &'a str,
                        owner_term: &'a str,
                        owner_obj: &'a Map<String, Value>,
                        index: &mut HashMap<&'a str, (&'a str, &'a Map<String, Value>)>|
     -> Result<()> {
        if let Some((existing_term, _)) = index.get(key) {
            bail!(
                "LEXICON_ALIAS_COLLISION: within bounded_context '{bounded_context}', \
                 key '{key}' is claimed by both '{existing_term}' and '{owner_term}'. \
                 Aliases must be unique within a bounded context. (SPEC §30.10.1)"
            );
        }
        index.insert(key, (owner_term, owner_obj));
        Ok(())
    };

    for &(term, obj) in entries {
        register(term, term, obj, &mut index)?;
        if let Some(aliases) = obj.get("aliases").and_then(Value::as_array) {
            for alias_val in aliases {
                if let Some(alias) = alias_val.as_str() {
                    register(alias, term, obj, &mut index)?;
                }
            }
        }
    }
    Ok(index.into_iter().map(|(k, (_, v))| (k, v)).collect())
}

/// SPEC §30.10.1 — build the snapshot-time combined-form index for a
/// single bounded context. Returns a `HashMap<&str, &Map<String, Value>>`
/// keyed by canonical term + every alias, all pointing at the same entry
/// object. O(1) lookup against any surface form.
///
/// Call once per bounded context at snapshot-stamp time (or validation).
/// Returns `Err` on collision (same check as `validate_lexicon`).
pub fn build_combined_index<'a>(
    lexicon_obj: &'a Map<String, Value>,
    bounded_context: &str,
) -> Result<HashMap<&'a str, &'a Map<String, Value>>> {
    let entries: Vec<(&str, &Map<String, Value>)> = lexicon_obj
        .iter()
        .filter_map(|(k, v)| {
            let obj = v.as_object()?;
            let entry_ctx = obj
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx == bounded_context {
                Some((k.as_str(), obj))
            } else {
                None
            }
        })
        .collect();
    build_combined_index_inner(&entries, bounded_context)
}

/// SPEC §30.4 — stamp the full lexicon onto every workflow that exists
/// in the config. Mirrors `stamp_skills_library` (SPEC §8.2): every
/// in-flight workflow sees the lexicon as it existed at
/// `workflow.start` time, immune to mid-flight edits of the top-level
/// `lexicon:` block.
pub fn stamp_lexicon_library(config: &mut Value) {
    let Some(lexicon) = config.get("lexicon").cloned() else {
        return;
    };
    let Some(workflows) = config
        .pointer_mut("/workflows")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for (_id, def) in workflows {
        if let Some(obj) = def.as_object_mut() {
            obj.insert("_lexiconLibrary".to_string(), lexicon.clone());
        }
    }
}

/// SPEC §30.5 — exact-term lookup against a workflow's stamped lexicon
/// library. Returns the entry value (`{definition, examples?, refs?,
/// bounded_context?, governance}`) or `None` when the term is absent.
pub fn lookup_term<'a>(
    workflow_definition: &'a Value,
    term: &str,
    bounded_context: Option<&str>,
) -> Option<&'a Value> {
    let lib = workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)?;
    let entry = lib.get(term)?;
    if let Some(filter_ctx) = bounded_context {
        let entry_ctx = entry
            .get("bounded_context")
            .and_then(Value::as_str)
            .unwrap_or("");
        if entry_ctx != filter_ctx {
            return None;
        }
    }
    Some(entry)
}

/// SPEC §30.5 — keyword search across the stamped lexicon library.
/// Substring match against term name + definition; optional
/// bounded_context filter; results limited to `limit` (default 10).
/// Returns a list of `{term, ...entry-fields}` objects in match order.
pub fn search_terms(
    workflow_definition: &Value,
    query: &str,
    bounded_context: Option<&str>,
    limit: Option<usize>,
) -> Vec<Value> {
    let limit = limit.unwrap_or(10);
    let Some(lib) = workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
    else {
        return vec![];
    };
    let q_lower = query.to_lowercase();
    let mut hits: Vec<Value> = Vec::new();
    for (term, entry) in lib {
        if let Some(filter_ctx) = bounded_context {
            let entry_ctx = entry
                .get("bounded_context")
                .and_then(Value::as_str)
                .unwrap_or("");
            if entry_ctx != filter_ctx {
                continue;
            }
        }
        let term_match = term.to_lowercase().contains(&q_lower);
        let def_match = entry
            .get("definition_short")
            .and_then(Value::as_str)
            .map(|d| d.to_lowercase().contains(&q_lower))
            .unwrap_or(false);
        if term_match || def_match {
            let mut hit = entry.clone();
            if let Some(obj) = hit.as_object_mut() {
                obj.insert("term".to_string(), json!(term));
            }
            hits.push(hit);
            if hits.len() >= limit {
                break;
            }
        }
    }
    hits
}

/// SPEC §30.6 — governance check. Returns the governance level for a
/// term (defaults to `human-only` when absent). Used by the MCP
/// `gateway.lexicon.define` handler to gate agent writes.
pub fn governance_for(workflow_definition: &Value, term: &str) -> String {
    workflow_definition
        .get("_lexiconLibrary")
        .and_then(Value::as_object)
        .and_then(|lib| lib.get(term))
        .and_then(|entry| entry.get("governance"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_GOVERNANCE)
        .to_string()
}

/// SPEC §30.6 — whether a proposed write to `term` is allowed for the
/// given actor role. Agents calling `gateway.lexicon.define` against
/// a `human-only` term must be rejected with
/// `LEXICON_DEFINE_REQUIRES_HUMAN`. Humans always pass; agents only
/// pass against `agent-may-propose` terms.
///
/// `actor_is_human` reflects whether the calling principal has the
/// `human` role (same gate the existing `actor: human` transition
/// machinery uses).
pub fn define_allowed(
    workflow_definition: &Value,
    term: &str,
    actor_is_human: bool,
) -> Result<(), String> {
    if actor_is_human {
        return Ok(());
    }
    let governance = governance_for(workflow_definition, term);
    if governance == "agent-may-propose" {
        Ok(())
    } else {
        Err(format!(
            "LEXICON_DEFINE_REQUIRES_HUMAN: term '{term}' has governance \
             '{governance}'; an agent attempted to define it. Route through an \
             actor: human transition to commit. (SPEC §30.6)"
        ))
    }
}

/// SPEC §30.5 / §30.10.1 — build a proposed entry value from
/// `gateway.lexicon.define` arguments. Uses `definition_short` as the
/// primary one-sentence definition field. Used by the MCP handler before
/// persisting; centralized so the shape is consistent and validation runs
/// in one place.
pub fn build_entry(
    definition_short: &str,
    bounded_context: Option<&str>,
    refs: Option<&Vec<String>>,
    governance: Option<&str>,
) -> Result<Value> {
    if definition_short.trim().is_empty() {
        bail!("INVALID_LEXICON_ENTRY: definition must be non-empty");
    }
    let mut entry = Map::new();
    entry.insert("definition_short".into(), json!(definition_short));
    if let Some(ctx) = bounded_context {
        entry.insert("bounded_context".into(), json!(ctx));
    }
    if let Some(rs) = refs {
        entry.insert("refs".into(), json!(rs));
    }
    let gov = governance.unwrap_or(DEFAULT_GOVERNANCE);
    if gov != "human-only" && gov != "agent-may-propose" {
        bail!(
            "INVALID_LEXICON_ENTRY: governance must be `human-only` or \
             `agent-may-propose`; got '{gov}'"
        );
    }
    entry.insert("governance".into(), json!(gov));
    Ok(Value::Object(entry))
}
