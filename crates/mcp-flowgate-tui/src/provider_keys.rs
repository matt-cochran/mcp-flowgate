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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// Errors from the provider-keys file backend.
#[derive(Debug, thiserror::Error)]
pub enum ProviderKeysError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "file {path} has permissions {mode:o}; expected 0600. \
         Fix with: chmod 600 {path}"
    )]
    PermissionsTooOpen { path: String, mode: u32 },
}

/// Read the provider-keys file. Returns an empty map if the file does
/// not exist. Malformed lines (no `=`) are skipped with a warn log;
/// blank lines are ignored. Values are taken verbatim (no quote
/// stripping) — the writer doesn't quote, so the reader doesn't unquote.
/// Surrounding whitespace on both keys and values is trimmed so
/// hand-edited files with `KEY = value` syntax round-trip correctly.
pub fn read(path: &Path) -> Result<BTreeMap<String, String>, ProviderKeysError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(ProviderKeysError::Io(e)),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ProviderKeysError::PermissionsTooOpen {
                path: path.display().to_string(),
                mode,
            });
        }
    }

    let mut out = BTreeMap::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) => {
                out.insert(k.trim().to_string(), v.trim().to_string());
            }
            None => {
                tracing::warn!(
                    file = %path.display(),
                    line_no = i + 1,
                    "skipping malformed line in provider-keys file"
                );
            }
        }
    }
    Ok(out)
}

/// Write the provider-keys map atomically: tempfile in the same dir,
/// chmod 0600, then rename over the target. Parent dir created with
/// mode 0700 if missing. Atomic rename means a partial-write torn
/// state is impossible.
pub fn write_atomic(
    path: &Path,
    vars: &BTreeMap<String, String>,
) -> Result<(), ProviderKeysError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(parent)?.permissions();
            perm.set_mode(0o700);
            std::fs::set_permissions(parent, perm)?;
        }
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = tempfile::Builder::new()
        .prefix(".providers.env.")
        .suffix(".tmp")
        .tempfile_in(parent)?;

    {
        use std::io::Write;
        let mut f = temp.as_file();
        for (k, v) in vars {
            writeln!(f, "{k}={v}")?;
        }
        f.flush()?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(temp.path())?.permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(temp.path(), perm)?;
    }

    temp.persist(path).map_err(|e| ProviderKeysError::Io(e.error))?;
    Ok(())
}
