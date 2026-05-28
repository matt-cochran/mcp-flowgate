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
use std::process::ExitCode;

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

/// Inject-friendly load. Read the file, then for each `(k, v)`:
/// - if `read_env(k)` returns Some, leave it (env wins over file).
/// - otherwise call `set_env(k, v)`.
///
/// Errors are returned, not silently swallowed — the production
/// wrapper [`load_into_env_if_present`] decides the swallow policy.
pub fn load_into_env_with(
    path: &Path,
    read_env: impl Fn(&str) -> Option<String>,
    mut set_env: impl FnMut(&str, &str),
) -> Result<(), ProviderKeysError> {
    let vars = read(path)?;
    for (k, v) in vars {
        if read_env(&k).is_some() {
            continue;
        }
        set_env(&k, &v);
    }
    Ok(())
}

/// Provider aliases for the CLI `--provider <name>` flag. Each maps to
/// one or more env vars that the underlying provider in `aether-llm`
/// reads at request time. Slugs are stable (used in the file, on the
/// CLI, and in log lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderId {
    Anthropic,
    Openai,
    Openrouter,
    Bedrock,
    Gemini,
}

impl ProviderId {
    pub const ALL: &'static [ProviderId] = &[
        ProviderId::Anthropic,
        ProviderId::Openai,
        ProviderId::Openrouter,
        ProviderId::Bedrock,
        ProviderId::Gemini,
    ];

    pub fn slug(&self) -> &'static str {
        match self {
            ProviderId::Anthropic  => "anthropic",
            ProviderId::Openai     => "openai",
            ProviderId::Openrouter => "openrouter",
            ProviderId::Bedrock    => "bedrock",
            ProviderId::Gemini     => "gemini",
        }
    }

    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().find(|p| p.slug() == s).copied()
    }

    pub fn env_vars(&self) -> &'static [&'static str] {
        match self {
            ProviderId::Anthropic  => &["ANTHROPIC_API_KEY"],
            ProviderId::Openai     => &["OPENAI_API_KEY"],
            ProviderId::Openrouter => &["OPENROUTER_API_KEY"],
            ProviderId::Bedrock    => &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"],
            ProviderId::Gemini     => &["GEMINI_API_KEY"],
        }
    }

    pub fn display(&self) -> &'static str {
        match self {
            ProviderId::Anthropic  => "Anthropic",
            ProviderId::Openai     => "OpenAI",
            ProviderId::Openrouter => "OpenRouter",
            ProviderId::Bedrock    => "AWS Bedrock",
            ProviderId::Gemini     => "Google Gemini",
        }
    }
}

/// Mask a secret value for `--list` display. Long values become
/// `<7-char-prefix>***<last-4>`; values of 8 chars or less are masked
/// entirely (the prefix-plus-last4 form would leak too much of the
/// original).
pub fn mask_value(s: &str) -> String {
    if s.len() <= 8 {
        return "***".to_string();
    }
    let prefix: String = s.chars().take(7).collect();
    let last4: String = s
        .chars()
        .rev()
        .take(4)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{prefix}***{last4}")
}

/// Upsert a single env var in the file. Reads, mutates one key, writes
/// atomically. Use this for the CLI `set` path; the interactive walker
/// composes multiple `set_var` calls.
pub fn set_var(path: &Path, key: &str, value: &str) -> Result<(), ProviderKeysError> {
    let mut vars = read(path)?;
    vars.insert(key.to_string(), value.to_string());
    write_atomic(path, &vars)
}

/// Delete every env var that belongs to the given provider.
pub fn remove_provider(path: &Path, provider: ProviderId) -> Result<(), ProviderKeysError> {
    let mut vars = read(path)?;
    for k in provider.env_vars() {
        vars.remove(*k);
    }
    write_atomic(path, &vars)
}

/// CLI args for `flowgate set-provider-keys`.
///
/// Without flags: interactive walk of every supported provider.
/// `--list`: show configured providers with masked values.
/// `--provider <name>`: set one provider; reads value from stdin
/// (with `--stdin`) or via no-echo prompt (without `--stdin`).
/// Multi-var providers (e.g. `bedrock` → `AWS_ACCESS_KEY_ID`,
/// `AWS_SECRET_ACCESS_KEY`, `AWS_REGION`) consume one stdin line per
/// env var in declaration order.
/// `--remove <name>`: clear one provider's vars.
/// `--path`: print the resolved file path and exit.
#[derive(clap::Args, Debug)]
pub struct SetProviderKeysArgs {
    #[arg(long, value_name = "PROVIDER")]
    pub provider: Option<String>,

    #[arg(long, conflicts_with_all = ["provider", "remove", "path"])]
    pub list: bool,

    #[arg(long, value_name = "PROVIDER", conflicts_with_all = ["provider", "list", "path"])]
    pub remove: Option<String>,

    #[arg(long, conflicts_with_all = ["provider", "list", "remove"])]
    pub path: bool,

    #[arg(long, requires = "provider")]
    pub stdin: bool,
}

pub fn run(args: SetProviderKeysArgs) -> anyhow::Result<ExitCode> {
    let path = resolve_path();

    if args.path {
        println!("{}", path.display());
        return Ok(ExitCode::SUCCESS);
    }

    if args.list {
        return run_list(&path);
    }

    if let Some(slug) = args.remove.as_deref() {
        let provider = ProviderId::from_slug(slug).ok_or_else(|| {
            anyhow::anyhow!("unknown provider '{slug}'. Valid: {}", valid_slugs())
        })?;
        remove_provider(&path, provider)?;
        eprintln!("removed {} keys from {}", provider.display(), path.display());
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(slug) = args.provider.as_deref() {
        let provider = ProviderId::from_slug(slug).ok_or_else(|| {
            anyhow::anyhow!("unknown provider '{slug}'. Valid: {}", valid_slugs())
        })?;
        return run_set_one(&path, provider, args.stdin);
    }

    run_interactive(&path)
}

fn valid_slugs() -> String {
    ProviderId::ALL
        .iter()
        .map(|p| p.slug())
        .collect::<Vec<_>>()
        .join(", ")
}

fn run_list(path: &Path) -> anyhow::Result<ExitCode> {
    let vars = read(path)?;
    if vars.is_empty() {
        println!("(no provider keys configured at {})", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}:", path.display());
    for provider in ProviderId::ALL {
        let mut any = false;
        for k in provider.env_vars() {
            if let Some(v) = vars.get(*k) {
                if !any {
                    println!("  {}:", provider.display());
                    any = true;
                }
                println!("    {k}={}", mask_value(v));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_set_one(
    path: &Path,
    provider: ProviderId,
    from_stdin: bool,
) -> anyhow::Result<ExitCode> {
    for env_var in provider.env_vars() {
        let value = if from_stdin {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s.trim().to_string()
        } else {
            rpassword::prompt_password(format!("{} ({env_var}): ", provider.display()))?
                .trim()
                .to_string()
        };
        if value.is_empty() {
            eprintln!("(empty value for {env_var} — skipped)");
            continue;
        }
        set_var(path, env_var, &value)?;
    }
    eprintln!("saved {} keys to {}", provider.display(), path.display());
    Ok(ExitCode::SUCCESS)
}

fn run_interactive(path: &Path) -> anyhow::Result<ExitCode> {
    println!("flowgate provider keys → {}", path.display());
    println!("(press Enter to skip a provider; values are not echoed)");
    for provider in ProviderId::ALL {
        println!();
        println!("== {} ({}) ==", provider.display(), provider.slug());
        let mut any = false;
        for env_var in provider.env_vars() {
            let value = rpassword::prompt_password(format!("  {env_var}: "))?
                .trim()
                .to_string();
            if value.is_empty() {
                continue;
            }
            set_var(path, env_var, &value)?;
            any = true;
        }
        if any {
            eprintln!("  saved {} keys", provider.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Production wrapper. Calls [`load_into_env_with`] against the real
/// process env, swallows missing-file (silent ok), logs other errors
/// as a single warning and continues. Called once from `main()`
/// before any CLI dispatch.
pub fn load_into_env_if_present() {
    let path = resolve_path();
    let result = load_into_env_with(
        &path,
        |k| std::env::var(k).ok(),
        // SAFETY: called synchronously at the top of `main()` before
        // the first `.await`, so no `tokio::spawn`-ed task exists yet
        // that could race on the process env. The same invariant
        // applies to `keyring::ensure_keyring_available`.
        |k, v| unsafe { std::env::set_var(k, v) },
    );
    if let Err(e) = result {
        tracing::warn!(
            error = %e,
            path = %path.display(),
            "failed to load provider-keys file"
        );
    }
}
