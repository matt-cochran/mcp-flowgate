//! Layered agent configuration with Chain-of-Responsibility resolution.
//!
//! Replaces v0.2's single `--agent name=provider/model` registry with:
//!
//! - **`agents.yaml`** at `.flowgate/agents.yaml` (project) or
//!   `~/.config/flowgate/agents.yaml` (user). Project wins whole-file.
//! - **Closed enums.** `Affinity` (5: coding, reasoning, prose, web-search,
//!   recon) × `Tier` (3: frontier, standard, commoditized). Workflows
//!   reference a `Delegate` made from one or both.
//! - **Sparse overrides** keyed by `<affinity>-<tier>`, `<affinity>`, or
//!   `<tier>`. One mandatory `default:` list catches anything unmatched.
//! - **Per-list Chain of Responsibility.** Each override list is tried in
//!   order; only *infrastructure* failures (401/403/429/404/network/timeout)
//!   trigger fall-through. Content failures surface to the caller.
//!
//! Safety properties (every one is FMECA-vetted — see
//! `/home/mc/.claude/plans/tender-honking-plum.md`):
//!
//! 1. Unknown response status defaults to `ContentOther` (surface, never
//!    fall through). `classify::FailureClass::from_response` test-pins this.
//! 2. Missing `default:` field fails at load (no `#[serde(default)]`).
//! 3. Primary (index-0) bindings auth-probed once at workflow load via
//!    `preflight::verify_primary_bindings`. 401/403 → startup error.
//! 4. CLI `--agent` flag and an on-disk `agents.yaml` are mutually
//!    exclusive — both set → `AmbiguousAgentSource` startup error.
//! 5. `strict_specificity: true` opt-in turns specificity-walk fall-through
//!    into a load-time error (poka-yoke for operators who want exact-match
//!    semantics only).

pub mod classify;
pub mod config;
pub mod preflight;
pub mod walk;

pub use classify::FailureClass;
pub use config::{
    Affinity, AgentConfigError, AgentsFile, AnthropicFeatures, Binding, GoogleFeatures,
    OpenAIFeatures, OverrideKey, Provider, ProviderFeatures, Tier,
};
pub use preflight::{verify_all_primary_bindings, verify_primary_bindings, PreflightError};
pub use walk::{
    AgentResolutionExhausted, AttemptRecord, ConfigSource, Delegate, DelegateParseError, Resolver,
};

/// FMECA T1: refuse to start when both `--agent` CLI flags AND an
/// on-disk `agents.yaml` are present. Picking one silently would mask
/// operator intent — surfacing the ambiguity is the only safe choice.
///
/// Pure function so `main.rs` can call it and tests can exercise the
/// poka-yoke without shelling out to the binary.
#[derive(Debug, thiserror::Error)]
#[error(
    "ambiguous agent source: both `--agent` CLI flag(s) AND an agents.yaml file are present. \
     Choose one — agents.yaml takes precedence going forward; the `--agent` flag is deprecated. \
     See /guides/agent-config.mdx for the migration path."
)]
pub struct AmbiguousAgentSourceError;

pub fn validate_agent_source_exclusivity(
    has_yaml: bool,
    has_cli_agent_flag: bool,
) -> Result<(), AmbiguousAgentSourceError> {
    if has_yaml && has_cli_agent_flag {
        Err(AmbiguousAgentSourceError)
    } else {
        Ok(())
    }
}

/// Validate an `agents.yaml` file at an arbitrary path by loading it
/// through `AgentsFile::from_path` — exactly the same path the
/// resolver uses at workflow start. Returns the JSON envelope the
/// `flowgate validate-agents-config` CLI emits.
///
/// On success: `{"ok": true, "summary": "..."}`. On failure:
/// `{"ok": false, "error_kind": "<variant>", "detail": "<rendered>"}`.
/// `error_kind` is one of: `MISSING_DEFAULT`, `EMPTY_DEFAULT`,
/// `MISSING_PROVIDER_MODEL`, `UNKNOWN_OVERRIDE_KEY`,
/// `UNKNOWN_FEATURE_KEY`, `PROVIDER_ENDPOINT_REQUIRED`,
/// `VERSION_MISMATCH`, `YAML_SYNTAX`, `IO`. The kind is the
/// stable contract; the detail is for humans.
pub fn validate_agents_config_envelope(path: &std::path::Path) -> serde_json::Value {
    match AgentsFile::from_path(path) {
        Ok(file) => serde_json::json!({
            "ok": true,
            "summary": format!(
                "{} default binding(s), {} override list(s), strict_specificity={}",
                file.default.len(),
                file.overrides.len(),
                file.strict_specificity,
            ),
        }),
        Err(e) => {
            let kind = match &e {
                AgentConfigError::MissingDefault => "MISSING_DEFAULT",
                AgentConfigError::EmptyDefault => "EMPTY_DEFAULT",
                AgentConfigError::MissingProviderModel => "MISSING_PROVIDER_MODEL",
                AgentConfigError::UnknownOverrideKey(_) => "UNKNOWN_OVERRIDE_KEY",
                AgentConfigError::UnknownFeatureKey { .. } => "UNKNOWN_FEATURE_KEY",
                AgentConfigError::ProviderEndpointRequired => "PROVIDER_ENDPOINT_REQUIRED",
                AgentConfigError::VersionMismatch { .. } => "VERSION_MISMATCH",
                AgentConfigError::YamlSyntax(_) => "YAML_SYNTAX",
                AgentConfigError::Io(_) => "IO",
            };
            serde_json::json!({
                "ok": false,
                "error_kind": kind,
                "detail": e.to_string(),
            })
        }
    }
}
