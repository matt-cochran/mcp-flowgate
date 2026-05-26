//! SPEC §30 — lexicon primitive tests.
//!
//! Covers Tier 1: per-config lexicon block, snapshot-stamping onto
//! every workflow, search/lookup/define semantics + governance gating.

use mcp_flowgate_core::config::resolve;
use mcp_flowgate_core::lexicon::{
    build_entry, define_allowed, governance_for, lookup_term, search_terms,
    stamp_lexicon_library, validate_lexicon,
};
use serde_json::json;

fn config_with_lexicon(lexicon: serde_json::Value) -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "lexicon": lexicon,
        "workflows": {
            "demo": {
                "initialState": "idle",
                "states": { "idle": { "terminal": true } }
            }
        }
    })
}

// ── validation ────────────────────────────────────────────────────────────

#[test]
fn validate_accepts_well_formed_lexicon() {
    let cfg = config_with_lexicon(json!({
        "connector": {
            "definition": "A unit of integration between the gateway and an external system.",
            "bounded_context": "gateway",
            "refs": ["capability"],
            "governance": "human-only"
        }
    }));
    assert!(validate_lexicon(&cfg).is_ok());
}

#[test]
fn validate_rejects_missing_definition() {
    let cfg = config_with_lexicon(json!({
        "broken": { "bounded_context": "gateway" }
    }));
    let err = validate_lexicon(&cfg).expect_err("must reject");
    assert!(format!("{err:?}").contains("INVALID_LEXICON_ENTRY"));
    assert!(format!("{err:?}").contains("missing the required `definition:`"));
}

#[test]
fn validate_rejects_empty_definition() {
    let cfg = config_with_lexicon(json!({ "broken": { "definition": "   " } }));
    let err = validate_lexicon(&cfg).expect_err("must reject");
    assert!(format!("{err:?}").contains("empty `definition:`"));
}

#[test]
fn validate_rejects_unknown_governance() {
    let cfg = config_with_lexicon(json!({
        "x": { "definition": "y", "governance": "free-for-all" }
    }));
    let err = validate_lexicon(&cfg).expect_err("must reject");
    assert!(format!("{err:?}").contains("unknown `governance: free-for-all`"));
}

// ── snapshot stamping ─────────────────────────────────────────────────────

#[test]
fn stamping_writes_lexicon_library_onto_every_workflow() {
    let mut cfg = config_with_lexicon(json!({
        "connector": { "definition": "Integration unit." }
    }));
    stamp_lexicon_library(&mut cfg);
    let lib = cfg.pointer("/workflows/demo/_lexiconLibrary").unwrap();
    assert!(lib.get("connector").is_some());
    assert_eq!(
        lib.pointer("/connector/definition").and_then(|v| v.as_str()),
        Some("Integration unit.")
    );
}

#[test]
fn full_resolve_pipeline_stamps_lexicon() {
    // Through the public resolve() — confirms validate + stamp are wired.
    let cfg = config_with_lexicon(json!({
        "ubiquitous_language": {
            "definition": "Shared vocabulary between domain experts and developers.",
            "bounded_context": "ddd"
        }
    }));
    let resolved = resolve(cfg).expect("resolve must succeed");
    let lib = resolved
        .pointer("/workflows/demo/_lexiconLibrary")
        .expect("workflow must have _lexiconLibrary stamped");
    assert!(lib.get("ubiquitous_language").is_some());
}

// ── lookup ────────────────────────────────────────────────────────────────

#[test]
fn lookup_returns_stamped_entry() {
    let mut cfg = config_with_lexicon(json!({
        "x": { "definition": "X is X." }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let entry = lookup_term(def, "x", None).expect("term must exist");
    assert_eq!(
        entry.get("definition").and_then(|v| v.as_str()),
        Some("X is X.")
    );
}

#[test]
fn lookup_returns_none_for_unknown_term() {
    let mut cfg = config_with_lexicon(json!({}));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(lookup_term(def, "unknown", None).is_none());
}

#[test]
fn lookup_with_bounded_context_filter() {
    let mut cfg = config_with_lexicon(json!({
        "x": { "definition": "X in A", "bounded_context": "A" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(lookup_term(def, "x", Some("A")).is_some());
    assert!(lookup_term(def, "x", Some("B")).is_none());
}

// ── search ────────────────────────────────────────────────────────────────

#[test]
fn search_matches_term_name_substring() {
    let mut cfg = config_with_lexicon(json!({
        "connector":  { "definition": "Integration." },
        "capability": { "definition": "Surface." },
        "executor":   { "definition": "Runs work." }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "connect", None, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get("term").and_then(|v| v.as_str()), Some("connector"));
}

#[test]
fn search_matches_definition_substring() {
    let mut cfg = config_with_lexicon(json!({
        "alpha": { "definition": "The first letter." },
        "beta":  { "definition": "The second letter." }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "second", None, None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get("term").and_then(|v| v.as_str()), Some("beta"));
}

#[test]
fn search_respects_bounded_context_filter() {
    let mut cfg = config_with_lexicon(json!({
        "x_in_a": { "definition": "X", "bounded_context": "A" },
        "x_in_b": { "definition": "X", "bounded_context": "B" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "X", Some("A"), None);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get("term").and_then(|v| v.as_str()), Some("x_in_a"));
}

#[test]
fn search_respects_limit() {
    let mut cfg = config_with_lexicon(json!({
        "a": { "definition": "match" },
        "b": { "definition": "match" },
        "c": { "definition": "match" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let hits = search_terms(def, "match", None, Some(2));
    assert_eq!(hits.len(), 2);
}

// ── governance ────────────────────────────────────────────────────────────

#[test]
fn governance_defaults_to_human_only_when_unset() {
    let mut cfg = config_with_lexicon(json!({
        "term_no_gov": { "definition": "no governance field" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert_eq!(governance_for(def, "term_no_gov"), "human-only");
}

#[test]
fn agent_rejected_against_human_only_term() {
    let mut cfg = config_with_lexicon(json!({
        "locked": { "definition": "x", "governance": "human-only" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    let err = define_allowed(def, "locked", false).expect_err("agent must be rejected");
    assert!(err.contains("LEXICON_DEFINE_REQUIRES_HUMAN"));
    assert!(err.contains("locked"));
}

#[test]
fn human_always_allowed() {
    let mut cfg = config_with_lexicon(json!({
        "locked": { "definition": "x", "governance": "human-only" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(define_allowed(def, "locked", true).is_ok());
}

#[test]
fn agent_allowed_against_agent_may_propose_term() {
    let mut cfg = config_with_lexicon(json!({
        "open": { "definition": "x", "governance": "agent-may-propose" }
    }));
    stamp_lexicon_library(&mut cfg);
    let def = cfg.pointer("/workflows/demo").unwrap();
    assert!(define_allowed(def, "open", false).is_ok());
}

// ── build_entry ───────────────────────────────────────────────────────────

#[test]
fn build_entry_sets_defaults() {
    let entry = build_entry("a real def", None, None, None).expect("ok");
    assert_eq!(entry.pointer("/definition").and_then(|v| v.as_str()), Some("a real def"));
    assert_eq!(entry.pointer("/governance").and_then(|v| v.as_str()), Some("human-only"));
}

#[test]
fn build_entry_rejects_empty_definition() {
    let err = build_entry("  ", None, None, None).expect_err("must reject");
    assert!(format!("{err:?}").contains("definition must be non-empty"));
}

#[test]
fn build_entry_rejects_unknown_governance() {
    let err = build_entry("a", None, None, Some("wat")).expect_err("must reject");
    assert!(format!("{err:?}").contains("governance must be"));
}
