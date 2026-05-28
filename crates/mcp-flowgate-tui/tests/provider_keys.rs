use mcp_flowgate_tui::provider_keys;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// All tests share this lock so env-var mutations don't interleave.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn resolve_path_honors_env_override() {
    let _guard = env_lock().lock().unwrap();
    // SAFETY: env_lock() serializes access across this test binary; no
    // other crate code touches FLOWGATE_PROVIDER_KEYS_FILE.
    unsafe { std::env::set_var("FLOWGATE_PROVIDER_KEYS_FILE", "/tmp/custom-provider-keys.env"); }
    let p = provider_keys::resolve_path();
    assert_eq!(p, PathBuf::from("/tmp/custom-provider-keys.env"));
    unsafe { std::env::remove_var("FLOWGATE_PROVIDER_KEYS_FILE"); }
}

#[test]
fn resolve_path_defaults_under_config_dir() {
    let _guard = env_lock().lock().unwrap();
    // SAFETY: env_lock() serializes access across this test binary; no
    // other crate code touches FLOWGATE_PROVIDER_KEYS_FILE.
    unsafe { std::env::remove_var("FLOWGATE_PROVIDER_KEYS_FILE"); }
    let p = provider_keys::resolve_path();
    // The default is dirs::config_dir().join("flowgate/providers.env"); on every
    // supported platform `dirs::config_dir` returns Some. Assert the suffix
    // rather than the absolute path (which varies by user).
    assert!(p.ends_with("flowgate/providers.env"), "got {}", p.display());
}

#[test]
fn resolve_path_whitespace_env_falls_through_to_default() {
    let _guard = env_lock().lock().unwrap();
    // SAFETY: env_lock() serializes access across this test binary.
    unsafe { std::env::set_var("FLOWGATE_PROVIDER_KEYS_FILE", "   "); }
    let p = provider_keys::resolve_path();
    assert!(
        p.ends_with("flowgate/providers.env") || p.ends_with("flowgate-providers.env"),
        "whitespace env should fall through; got {}", p.display()
    );
    unsafe { std::env::remove_var("FLOWGATE_PROVIDER_KEYS_FILE"); }
}
