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
