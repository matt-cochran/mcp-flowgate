//! FMECA U3/U4/T3 + PR1-vetted-plan tests for `agents.yaml` loader.
//!
//! Each test name maps directly to a row in the FMECA mapping table in
//! `/home/mc/.claude/plans/tender-honking-plum.md`.

use mcp_flowgate_tui::agent_resolver::{
    Affinity, AgentConfigError, AgentsFile, OverrideKey, Provider, ProviderFeatures, Tier,
};

// ── happy path: confirms the round-trip shape ───────────────────────────────

#[test]
fn minimal_valid_file_loads() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let f = AgentsFile::from_yaml(yaml).expect("loads");
    assert_eq!(f.version, 1);
    assert!(!f.strict_specificity);
    assert_eq!(f.default.len(), 1);
    assert_eq!(f.default[0].provider, Provider::Anthropic);
    assert_eq!(f.default[0].model, "claude-sonnet-4-6");
    assert!(matches!(
        f.default[0].features,
        ProviderFeatures::Anthropic(_)
    ));
    assert!(f.overrides.is_empty());
}

#[test]
fn overrides_keyed_by_affinity_tier_round_trip() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  coding-frontier:
    - provider: { name: openai }
      model: gpt-5
  coding:
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
  frontier:
    - provider: { name: anthropic }
      model: claude-opus-4-7
"#;
    let f = AgentsFile::from_yaml(yaml).expect("loads");
    let key_full = OverrideKey {
        affinity: Some(Affinity::Coding),
        tier: Some(Tier::Frontier),
    };
    let key_aff = OverrideKey {
        affinity: Some(Affinity::Coding),
        tier: None,
    };
    let key_tier = OverrideKey {
        affinity: None,
        tier: Some(Tier::Frontier),
    };
    assert!(f.overrides.contains_key(&key_full));
    assert!(f.overrides.contains_key(&key_aff));
    assert!(f.overrides.contains_key(&key_tier));
    assert_eq!(f.overrides[&key_full][0].model, "gpt-5");
}

// ── FMECA U4 ────────────────────────────────────────────────────────────────

#[test]
fn default_required_at_load() {
    let yaml = r#"
version: 1
overrides:
  coding:
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
"#;
    let err = AgentsFile::from_yaml(yaml).expect_err("no default field → error");
    assert!(
        matches!(err, AgentConfigError::MissingDefault),
        "expected MissingDefault, got {err:?}"
    );
}

#[test]
fn empty_default_rejected() {
    let yaml = r#"
version: 1
default: []
"#;
    let err = AgentsFile::from_yaml(yaml).expect_err("empty default → error");
    assert!(
        matches!(err, AgentConfigError::EmptyDefault),
        "expected EmptyDefault, got {err:?}"
    );
}

// ── FMECA U3 (UnknownOverrideKey) ───────────────────────────────────────────

#[test]
fn unknown_affinity_named_in_error() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
overrides:
  vision-frontier:
    - provider: { name: anthropic }
      model: claude-sonnet-4-6
"#;
    let err = AgentsFile::from_yaml(yaml).expect_err("unknown affinity → error");
    let msg = format!("{err}");
    assert!(
        msg.contains("vision-frontier"),
        "error must name the offending key (got: {msg})"
    );
}

// ── FMECA T3 (deny_unknown_fields on per-provider features) ─────────────────

#[test]
fn unknown_feature_key_named() {
    let yaml = r#"
version: 1
default:
  - provider: { name: openai }
    model: gpt-5
    features:
      reasoning_effrt: high
"#;
    let err = AgentsFile::from_yaml(yaml).expect_err("typo in feature key → error");
    let msg = format!("{err}");
    assert!(
        msg.contains("reasoning_effrt"),
        "error must name the typo'd key (got: {msg})"
    );
}

// ── provider custom requires endpoint ───────────────────────────────────────

#[test]
fn provider_custom_requires_endpoint() {
    let yaml = r#"
version: 1
default:
  - provider: { name: custom, endpoint: "" }
    model: my-model
"#;
    let err = AgentsFile::from_yaml(yaml).expect_err("custom w/o endpoint → error");
    assert!(
        matches!(err, AgentConfigError::ProviderEndpointRequired),
        "expected ProviderEndpointRequired, got {err:?}"
    );
}

#[test]
fn provider_custom_with_endpoint_loads() {
    let yaml = r#"
version: 1
default:
  - provider: { name: custom, endpoint: "https://my-llm.internal/v1" }
    model: my-model
"#;
    let f = AgentsFile::from_yaml(yaml).expect("loads");
    let p = &f.default[0].provider;
    assert!(matches!(p, Provider::Custom { endpoint } if endpoint == "https://my-llm.internal/v1"));
}

// ── version mismatch ────────────────────────────────────────────────────────

#[test]
fn version_mismatch_surfaces() {
    let yaml = r#"
version: 99
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let err = AgentsFile::from_yaml(yaml).expect_err("version mismatch → error");
    assert!(
        matches!(
            err,
            AgentConfigError::VersionMismatch {
                got: 99,
                expected: 1
            }
        ),
        "expected VersionMismatch{{got:99,expected:1}}, got {err:?}"
    );
}

// ── deny_unknown_fields at top level ────────────────────────────────────────

#[test]
fn deny_unknown_fields_top_level() {
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
foo: bar
"#;
    let err = AgentsFile::from_yaml(yaml).expect_err("unknown top-level field → error");
    let msg = format!("{err}");
    assert!(
        msg.contains("foo"),
        "error must name the offending key (got: {msg})"
    );
}

// ── strict_specificity is parseable as a bool (truthy on YAML true) ─────────

#[test]
fn strict_specificity_parses() {
    let yaml = r#"
version: 1
strict_specificity: true
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let f = AgentsFile::from_yaml(yaml).expect("loads");
    assert!(f.strict_specificity);
}
