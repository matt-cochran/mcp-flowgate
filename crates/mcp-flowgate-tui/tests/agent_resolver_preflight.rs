//! FMECA U2 + T1 tests: eager primary-binding auth verification + the
//! CLI-flag/YAML mutual-exclusion check (the latter lives in main.rs
//! but is exercised via the public preflight surface).

use mcp_flowgate_tui::agent_resolver::preflight::{
    api_key_env_for, classify_outcome, probe_binding, PreflightOutcome,
};
use mcp_flowgate_tui::agent_resolver::{
    verify_primary_bindings, AgentsFile, Binding, ConfigSource, Delegate, FailureClass,
    PreflightError, Provider, ProviderFeatures, Resolver,
};
use std::path::PathBuf;
use std::sync::OnceLock;

use tokio::sync::Mutex;

// All tests in this file manipulate env vars; serialise to prevent
// interleaving. Uses tokio's async-aware Mutex so the guard can be
// held across `.await` without tripping the `await_holding_lock`
// clippy lint (`std::sync::Mutex` triggers it; this is the documented
// pattern for serialising env-touching async tests).
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn clear_env() {
    for var in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GOOGLE_API_KEY",
        "FLOWGATE_SKIP_PREFLIGHT",
    ] {
        std::env::remove_var(var);
    }
}

fn resolver_from(yaml: &str) -> Resolver {
    let file = AgentsFile::from_yaml(yaml).expect("yaml parses");
    Resolver::from_loaded(file, ConfigSource::Project(PathBuf::from("/tmp/agents.yaml")))
}

// ── api_key_env_for ─────────────────────────────────────────────────────────

#[test]
fn api_key_env_per_provider() {
    assert_eq!(api_key_env_for(&Provider::Anthropic), Some("ANTHROPIC_API_KEY"));
    assert_eq!(api_key_env_for(&Provider::Openai), Some("OPENAI_API_KEY"));
    assert_eq!(api_key_env_for(&Provider::Google), Some("GOOGLE_API_KEY"));
    assert_eq!(api_key_env_for(&Provider::Ollama), None);
    assert_eq!(api_key_env_for(&Provider::Lmstudio), None);
    assert_eq!(
        api_key_env_for(&Provider::Custom {
            endpoint: "https://x".into(),
        }),
        None
    );
}

// ── probe_binding ───────────────────────────────────────────────────────────

#[tokio::test]
async fn probe_with_credential_set_returns_ok() {
    let _g = env_lock().lock().await;
    clear_env();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test");
    let b = Binding {
        provider: Provider::Anthropic,
        model: "claude-sonnet-4-6".into(),
        features: ProviderFeatures::None,
    };
    assert!(matches!(probe_binding(&b).await, PreflightOutcome::Ok));
}

#[tokio::test]
async fn probe_without_credential_reports_missing() {
    let _g = env_lock().lock().await;
    clear_env();
    let b = Binding {
        provider: Provider::Openai,
        model: "gpt-5".into(),
        features: ProviderFeatures::None,
    };
    let outcome = probe_binding(&b).await;
    assert!(
        matches!(
            outcome,
            PreflightOutcome::MissingCredential { env_var: "OPENAI_API_KEY" }
        ),
        "got {outcome:?}"
    );
}

#[tokio::test]
async fn probe_ollama_no_credential_required_returns_ok() {
    let _g = env_lock().lock().await;
    clear_env();
    let b = Binding {
        provider: Provider::Ollama,
        model: "llama3".into(),
        features: ProviderFeatures::None,
    };
    assert!(matches!(probe_binding(&b).await, PreflightOutcome::Ok));
}

// ── verify_primary_bindings ─────────────────────────────────────────────────

#[tokio::test]
async fn primary_missing_credential_fails_startup() {
    let _g = env_lock().lock().await;
    clear_env();
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let d = Delegate::parse("coding").unwrap();
    let err = verify_primary_bindings(&r, &[d])
        .await
        .expect_err("missing ANTHROPIC_API_KEY = startup error");
    assert!(matches!(
        err.first(),
        Some(PreflightError::MissingCredential { .. })
    ));
}

#[tokio::test]
async fn primary_with_credential_passes_startup() {
    let _g = env_lock().lock().await;
    clear_env();
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test");
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let d = Delegate::parse("coding").unwrap();
    verify_primary_bindings(&r, &[d])
        .await
        .expect("ANTHROPIC_API_KEY set = passes");
}

#[tokio::test]
async fn skip_env_bypasses_preflight() {
    let _g = env_lock().lock().await;
    clear_env();
    // No credentials set, but SKIP env is on → preflight must pass.
    std::env::set_var("FLOWGATE_SKIP_PREFLIGHT", "1");
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let d = Delegate::parse("coding").unwrap();
    verify_primary_bindings(&r, &[d])
        .await
        .expect("SKIP=1 bypasses preflight even with missing creds");
}

// ── classify_outcome (dispatch logic) ──────────────────────────────────────

#[test]
fn primary_rate_limit_logs_warning_but_passes() {
    // FMECA U2: 429 (and 404 / network) on a primary preflight is a
    // transient class — the resolver's runtime CoR will route around it.
    // Startup must NOT fail. classify_outcome is the pure dispatch helper
    // both verify_* functions delegate to; testing it pins the contract
    // without needing real HTTP plumbing.
    let binding = Binding {
        provider: Provider::Anthropic,
        model: "claude-sonnet-4-6".into(),
        features: ProviderFeatures::None,
    };
    let outcome = PreflightOutcome::Warn {
        class: FailureClass::RateLimit429,
        detail: "429 from anthropic".into(),
    };
    assert!(
        classify_outcome("coding", &binding, outcome).is_none(),
        "429 on primary must warn-but-pass; only 401/403/missing-cred block startup"
    );
}

#[test]
fn primary_auth_401_blocks_startup() {
    // Inverse of the above — pins the boundary: 401 IS a hard failure.
    let binding = Binding {
        provider: Provider::Anthropic,
        model: "claude-sonnet-4-6".into(),
        features: ProviderFeatures::None,
    };
    let outcome = PreflightOutcome::Fail {
        class: FailureClass::Auth401,
        detail: "401 unauthorized".into(),
    };
    let err = classify_outcome("coding", &binding, outcome)
        .expect("401 on primary must produce a startup error");
    assert!(matches!(err, PreflightError::PrimaryAuthFailed { .. }));
}

#[tokio::test]
async fn preflight_dedupes_same_primary_across_delegates() {
    // Multiple delegates that resolve to the same primary binding
    // should only probe once. We can't observe the probe count
    // directly without an injectable HTTP client, but we CAN observe
    // that the error list isn't repeated.
    let _g = env_lock().lock().await;
    clear_env();
    let yaml = r#"
version: 1
default:
  - provider: { name: anthropic }
    model: claude-sonnet-4-6
"#;
    let r = resolver_from(yaml);
    let delegates = vec![
        Delegate::parse("coding").unwrap(),
        Delegate::parse("reasoning").unwrap(),
        Delegate::parse("prose").unwrap(),
    ];
    let err = verify_primary_bindings(&r, &delegates)
        .await
        .expect_err("missing cred");
    assert_eq!(
        err.len(),
        1,
        "the SAME primary binding's missing credential must surface ONCE, not per-delegate"
    );
}
