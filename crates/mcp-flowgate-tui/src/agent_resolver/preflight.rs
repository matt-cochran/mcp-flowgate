//! Eager auth verification for primary bindings — FMECA U2 mitigation.
//!
//! At workflow load, every distinct primary (index-0) `Binding`
//! referenced by any `delegate:` state is probed once with a tiny auth
//! check. 401/403 on a primary is a startup error, not a runtime
//! fall-through. 429/404/network is logged as a warning and allowed (the
//! resolver's per-list CoR will route around it at runtime).
//!
//! `FLOWGATE_SKIP_PREFLIGHT=1` is an escape hatch for CI / disconnected
//! dev. Skipped runs log a single line so the operator knows preflight
//! was bypassed.

use std::collections::BTreeSet;

use crate::agent_resolver::classify::FailureClass;
use crate::agent_resolver::config::{Binding, Provider};
use crate::agent_resolver::walk::{Delegate, Resolver};

/// Per-binding preflight outcome.
#[derive(Debug)]
pub enum PreflightError {
    /// Primary binding's auth was rejected (401 or 403). Hard failure —
    /// startup must not proceed; the operator's API key for this
    /// provider needs fixing before any workflow can run.
    PrimaryAuthFailed {
        delegate: String,
        binding: Binding,
        class: FailureClass,
        detail: String,
    },
    /// Primary binding's preflight couldn't complete because the env
    /// var carrying the API key isn't set. Hard failure — same shape as
    /// auth-failed; we don't probe at all if there's no credential.
    MissingCredential {
        delegate: String,
        binding: Binding,
        env_var: &'static str,
    },
}

impl std::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreflightError::PrimaryAuthFailed {
                delegate,
                binding,
                class,
                detail,
            } => write!(
                f,
                "preflight: primary binding for `{delegate}` failed auth: provider={} model={} \
                 class={class:?} detail={detail}",
                binding.provider.display_name(),
                binding.model
            ),
            PreflightError::MissingCredential {
                delegate,
                binding,
                env_var,
            } => write!(
                f,
                "preflight: primary binding for `{delegate}` requires ${env_var} (provider={} \
                 model={}). Set the env var or use `FLOWGATE_SKIP_PREFLIGHT=1` to bypass.",
                binding.provider.display_name(),
                binding.model
            ),
        }
    }
}

impl std::error::Error for PreflightError {}

/// Outcome of a single preflight probe. Public so doctor can re-use the
/// same machinery to surface preflight state without halting startup.
#[derive(Debug)]
pub enum PreflightOutcome {
    /// 200 OK — credentials valid.
    Ok,
    /// 429 / 404 / network — transient or recoverable; warn and continue.
    Warn { class: FailureClass, detail: String },
    /// 401 / 403 — hard failure; surface as `PrimaryAuthFailed`.
    Fail { class: FailureClass, detail: String },
    /// Required env var missing.
    MissingCredential { env_var: &'static str },
}

/// Classify a probe outcome into a startup error (if any) for the given
/// (label, binding). Pure function so the warn-vs-fail dispatch logic is
/// testable without HTTP plumbing.
///
/// FMECA U2: only `Fail` and `MissingCredential` block startup. `Warn`
/// (429/404/transient network) is logged at the call site and lets
/// startup proceed — the resolver's runtime CoR will route around it.
pub fn classify_outcome(
    label: &str,
    binding: &Binding,
    outcome: PreflightOutcome,
) -> Option<PreflightError> {
    match outcome {
        PreflightOutcome::Ok => None,
        PreflightOutcome::Warn { class, detail } => {
            tracing::warn!(
                target: "flowgate.agent_resolver",
                label = %label,
                provider = binding.provider.display_name(),
                model = %binding.model,
                ?class,
                %detail,
                "primary preflight: transient — runtime CoR will handle"
            );
            None
        }
        PreflightOutcome::Fail { class, detail } => Some(PreflightError::PrimaryAuthFailed {
            delegate: label.to_string(),
            binding: binding.clone(),
            class,
            detail,
        }),
        PreflightOutcome::MissingCredential { env_var } => Some(PreflightError::MissingCredential {
            delegate: label.to_string(),
            binding: binding.clone(),
            env_var,
        }),
    }
}

/// Verify primary bindings for the given delegates. Returns Ok(()) if
/// every primary either probed Ok or warned; returns a list of all
/// failures otherwise.
///
/// Honors `FLOWGATE_SKIP_PREFLIGHT=1` — when set, returns Ok(()) without
/// probing.
pub async fn verify_primary_bindings(
    resolver: &Resolver,
    delegates: &[Delegate],
) -> Result<(), Vec<PreflightError>> {
    if std::env::var("FLOWGATE_SKIP_PREFLIGHT").as_deref() == Ok("1") {
        tracing::info!(
            target: "flowgate.agent_resolver",
            "preflight skipped (FLOWGATE_SKIP_PREFLIGHT=1)"
        );
        return Ok(());
    }
    let mut errors = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for d in delegates {
        let (bindings, _) = match resolver.walk(d) {
            Ok(x) => x,
            Err(_) => continue, // resolution failures are doctor's problem, not preflight's
        };
        let Some(primary) = bindings.first() else {
            continue;
        };
        let key = (primary.provider.display_name().to_string(), primary.model.clone());
        if !seen.insert(key) {
            continue;
        }
        let outcome = probe_binding(primary).await;
        if let Some(err) = classify_outcome(&d.to_string(), primary, outcome) {
            errors.push(err);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Probe a single binding. The implementation reads the provider's
/// standard API key env var and (in v0.3) checks only its *presence* —
/// actual HTTP probing is deferred to a future point where we want the
/// extra dep on a HTTP client. Missing env var → MissingCredential;
/// present env var → Ok.
///
/// FMECA U2 says "auth-probe at startup." For v0.3 we ship the
/// env-presence check (zero new deps, deterministic, no network). A
/// follow-up can upgrade to a real HTTP probe behind the same function
/// — every caller and test pins behavior to `PreflightOutcome`, so the
/// upgrade is a one-function change.
pub async fn probe_binding(binding: &Binding) -> PreflightOutcome {
    let env_var = api_key_env_for(&binding.provider);
    let Some(var) = env_var else {
        // Providers with no env var (Ollama, LmStudio, custom with
        // unauthenticated endpoint) — nothing to verify, return Ok.
        return PreflightOutcome::Ok;
    };
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => PreflightOutcome::Ok,
        _ => PreflightOutcome::MissingCredential { env_var: var },
    }
}

/// Workflow-agnostic preflight. Probes the primary binding of every
/// override list + the default's primary, dedup'd by (provider, model).
///
/// PR1 scoping: rather than parse the workflow YAML to extract its
/// `delegate:` set, this probes every declared primary in `agents.yaml`.
/// Slightly broader than strictly necessary, but catches "you wrote a
/// binding for `coding-frontier` but forgot the API key" at startup
/// regardless of which workflow the operator is about to run.
///
/// A workflow-aware variant (`verify_primary_bindings(&[Delegate])`)
/// is available for callers that already know the delegate set.
pub async fn verify_all_primary_bindings(
    resolver: &Resolver,
) -> Result<(), Vec<PreflightError>> {
    if std::env::var("FLOWGATE_SKIP_PREFLIGHT").as_deref() == Ok("1") {
        tracing::info!(
            target: "flowgate.agent_resolver",
            "preflight skipped (FLOWGATE_SKIP_PREFLIGHT=1)"
        );
        return Ok(());
    }
    let mut errors = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let file = resolver.file();

    let mut all: Vec<(String, &Binding)> = Vec::new();
    if let Some(b) = file.default.first() {
        all.push(("default".to_string(), b));
    }
    for (key, list) in &file.overrides {
        if let Some(b) = list.first() {
            all.push((key.to_string(), b));
        }
    }

    for (label, b) in all {
        let key = (b.provider.display_name().to_string(), b.model.clone());
        if !seen.insert(key) {
            continue;
        }
        let outcome = probe_binding(b).await;
        if let Some(err) = classify_outcome(&label, b, outcome) {
            errors.push(err);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Map a provider to the env var carrying its API key (matches the
/// existing `doctor.rs:174-215` convention).
pub fn api_key_env_for(p: &Provider) -> Option<&'static str> {
    match p {
        Provider::Anthropic => Some("ANTHROPIC_API_KEY"),
        Provider::Openai => Some("OPENAI_API_KEY"),
        Provider::Google => Some("GOOGLE_API_KEY"),
        Provider::Ollama | Provider::Lmstudio | Provider::Custom { .. } => None,
    }
}
