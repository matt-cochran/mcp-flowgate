//! Provider API key file backend.
//!
//! Writes a flat dotenv file at `~/.config/flowgate/providers.env`
//! (override via `$FLOWGATE_PROVIDER_KEYS_FILE`) with mode 0600 inside
//! a 0700 parent dir. Loaded into env at flowgate-agent startup,
//! existing env vars taking precedence (CI overrides file).
//!
//! File-backed (not OS keyring) so agent sub-processes spawned by
//! `walk` / `headless` can read the keys without UI prompts and so
//! the path works identically across macOS, Linux, and WSL2.

use std::path::PathBuf;

/// Resolve the on-disk path for the provider-keys file. Precedence:
/// 1. `$FLOWGATE_PROVIDER_KEYS_FILE` if set + non-empty.
/// 2. `dirs::config_dir().join("flowgate/providers.env")`.
/// 3. `./flowgate-providers.env` as last-resort fallback (dirs::config_dir
///    returns None on some sandboxed CI environments).
pub fn resolve_path() -> PathBuf {
    if let Ok(p) = std::env::var("FLOWGATE_PROVIDER_KEYS_FILE") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    match dirs::config_dir() {
        Some(d) => d.join("flowgate").join("providers.env"),
        None => PathBuf::from("flowgate-providers.env"),
    }
}
