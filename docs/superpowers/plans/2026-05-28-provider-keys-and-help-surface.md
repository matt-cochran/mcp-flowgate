# `flowgate set-provider-keys` + CLI help surface — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a `flowgate set-provider-keys` subcommand (writes provider API keys to `~/.config/flowgate/providers.env`, loaded into env at startup) and turn on the rest of clap's discoverability surface (shell completions, man page, `long_about`, grouped headings) — so every CLI capability is findable from `--help`, `help <cmd>`, tab-complete, or `man flowgate`.

**Architecture:** One new module (`crates/mcp-flowgate-tui/src/provider_keys.rs`) holds the file format, atomic write, env-load, and CLI dispatch. `main.rs` gains three new `Command` variants (`SetProviderKeys`, `Completions`, `Man`), backfilled `long_about` on every existing variant, and `next_help_heading` groupings. No upstream aether changes; providers continue to read env vars exactly as today. Existing `keyring::ensure_keyring_available()` preflight is untouched; the new `provider_keys::load_into_env_if_present()` runs alongside it.

**Tech Stack:** Rust workspace (`mcp-flowgate-tui`), `clap` 4 (derive), `clap_complete`, `clap_mangen`, `rpassword`, `tempfile`, `dirs`. No new runtime deps beyond those four crates.

---

## Phase map

| Phase | Deliverable | Depends on |
|---|---|---|
| 1 | `provider_keys` module: path resolution, atomic read/write, load-into-env, mode-strictness | — |
| 2 | `ProviderId` enum + `SetProviderKeys` CLI subcommand (list / set / remove / interactive / path) | 1 |
| 3 | `clap_complete` `completions <shell>` subcommand | 2 |
| 4 | `clap_mangen` `man` subcommand | 2 |
| 5 | `long_about` backfill + `next_help_heading` groupings on every existing variant | 2, 3, 4 |
| 6 | README "Discovering commands" section | 5 |

**Phase gate (every phase):** `cargo test --workspace` green, `cargo clippy --workspace --all-targets -- -D warnings` clean, one logical commit per task. Each phase's commits sit on `feat/set-provider-keys` and get squashed only if the operator decides to at PR time — default is preserve.

**Concurrency note:** Phases 3 and 4 are independent of each other and can run in either order after Phase 2.

## File structure

| Path | New / Modify | Responsibility |
|---|---|---|
| `crates/mcp-flowgate-tui/src/provider_keys.rs` | **New** (~250 LOC) | `ProviderId`, `resolve_path`, `read`, `write_atomic`, `load_into_env_with`, `load_into_env_if_present`, `SetProviderKeysArgs`, `run` |
| `crates/mcp-flowgate-tui/src/main.rs` | Modify | +3 `Command` variants, `long_about` on all variants, `next_help_heading` groupings, call `load_into_env_if_present()` in `main()` |
| `crates/mcp-flowgate-tui/src/lib.rs` | Modify (1 line) | `pub mod provider_keys;` |
| `crates/mcp-flowgate-tui/Cargo.toml` | Modify | +`clap_complete`, +`clap_mangen`, +`rpassword`, +`tempfile` (promote from dev-dep) |
| `Cargo.toml` (workspace) | Modify | +workspace pins for `clap_complete`, `clap_mangen`, `rpassword`, `tempfile` |
| `crates/mcp-flowgate-tui/tests/provider_keys.rs` | **New** | Path resolution, round-trip, modes, env-load precedence, mode-strict reject, masking, malformed-line tolerance |
| `crates/mcp-flowgate-tui/tests/cli_help_surface.rs` | **New** | `--help` lists every subcommand under expected heading, `completions bash` emits non-empty script containing `_flowgate`, `man` emits roff starting with `.TH` |
| `README.md` | Modify | New section "Discovering commands" near the CLI usage block |

## Conventions used throughout

- **Run command:** every test step gives the exact `cargo test -p mcp-flowgate-tui --test <file> -- <test_name>` to run.
- **Expected output:** every run step states the literal expected status.
- **Commit messages:** prefix each with `feat(provider-keys):` / `feat(cli-help):` / `chore(deps):` to match the existing CHANGELOG style.
- **Workspace dep policy:** new deps go in workspace `[workspace.dependencies]` first, then `workspace = true` from the tui crate. Matches the existing pattern.
- **`std::env::set_var` is unsafe in edition 2021+ Rust:** wrap calls in `unsafe { ... }` as the existing `keyring.rs` does. The injectable `load_into_env_with` makes tests safe by accepting a `set_env: impl FnMut(&str, &str)` callback.

---

## Phase 1 — `provider_keys` module foundations

Goal: a tested, library-grade file backend before any CLI surface is added.

### Task 1.1: Workspace + crate deps for `tempfile` (promotion)

**Files:**
- Modify: `Cargo.toml` (workspace `[workspace.dependencies]`)
- Modify: `crates/mcp-flowgate-tui/Cargo.toml` (move `tempfile` from `[dev-dependencies]` to `[dependencies]` via workspace)

- [ ] **Step 1: Add workspace pin**

Edit `Cargo.toml` workspace `[workspace.dependencies]` block; add:

```toml
tempfile = "3"
```

- [ ] **Step 2: Use workspace dep from tui crate**

Edit `crates/mcp-flowgate-tui/Cargo.toml`:
- Add to `[dependencies]`: `tempfile.workspace = true`
- Remove the `tempfile = "3"` line from `[dev-dependencies]` (it'll be inherited via the main deps).

- [ ] **Step 3: Verify nothing broke**

Run: `cargo check --workspace`
Expected: PASS, no warnings about `tempfile`.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/mcp-flowgate-tui/Cargo.toml
git commit -m "chore(deps): promote tempfile to runtime dep for provider_keys"
```

### Task 1.2: `resolve_path` honors env override + default

**Files:**
- Create: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Create: `crates/mcp-flowgate-tui/tests/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/src/lib.rs` (add `pub mod provider_keys;`)

- [ ] **Step 1: Write the failing test**

Create `crates/mcp-flowgate-tui/tests/provider_keys.rs`:

```rust
use mcp_flowgate_tui::provider_keys;
use std::path::PathBuf;

#[test]
fn resolve_path_honors_env_override() {
    // SAFETY: single-threaded test, no other code touches this env var.
    unsafe { std::env::set_var("FLOWGATE_PROVIDER_KEYS_FILE", "/tmp/custom-provider-keys.env"); }
    let p = provider_keys::resolve_path();
    assert_eq!(p, PathBuf::from("/tmp/custom-provider-keys.env"));
    unsafe { std::env::remove_var("FLOWGATE_PROVIDER_KEYS_FILE"); }
}

#[test]
fn resolve_path_defaults_under_config_dir() {
    unsafe { std::env::remove_var("FLOWGATE_PROVIDER_KEYS_FILE"); }
    let p = provider_keys::resolve_path();
    // The default is dirs::config_dir().join("flowgate/providers.env"); on every
    // supported platform `dirs::config_dir` returns Some. Assert the suffix
    // rather than the absolute path (which varies by user).
    assert!(p.ends_with("flowgate/providers.env"), "got {}", p.display());
}
```

- [ ] **Step 2: Add the module stub + lib export**

Edit `crates/mcp-flowgate-tui/src/lib.rs` — add `pub mod provider_keys;` near the other `pub mod` lines.

Create `crates/mcp-flowgate-tui/src/provider_keys.rs` with the bare minimum:

```rust
//! Provider API key file backend.
//!
//! Writes a flat dotenv file at `~/.config/flowgate/providers.env`
//! (override via `$FLOWGATE_PROVIDER_KEYS_FILE`) with mode 0600 inside
//! a 0700 parent dir. Loaded into env at flowgate-agent startup,
//! existing env vars taking precedence (CI overrides file).

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
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 2 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs \
        crates/mcp-flowgate-tui/src/lib.rs \
        crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): resolve_path with env override + default"
```

### Task 1.3: `read` parses dotenv-format file

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/tests/provider_keys.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/provider_keys.rs`:

```rust
use std::collections::BTreeMap;
use std::io::Write;

#[test]
fn read_missing_file_returns_empty_map() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.env");
    let m = provider_keys::read(&p).expect("missing-file is fine");
    assert!(m.is_empty());
}

#[test]
fn read_parses_two_var_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "ANTHROPIC_API_KEY=sk-ant-aaa").unwrap();
    writeln!(f, "OPENAI_API_KEY=sk-bbb").unwrap();
    let m = provider_keys::read(&p).unwrap();
    let expected: BTreeMap<String, String> = [
        ("ANTHROPIC_API_KEY".into(), "sk-ant-aaa".into()),
        ("OPENAI_API_KEY".into(), "sk-bbb".into()),
    ].into_iter().collect();
    assert_eq!(m, expected);
}

#[test]
fn read_skips_malformed_lines_and_blank_lines() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut f = std::fs::File::create(&p).unwrap();
    writeln!(f, "").unwrap();
    writeln!(f, "this-has-no-equals-sign").unwrap();
    writeln!(f, "ANTHROPIC_API_KEY=sk-ant-valid").unwrap();
    let m = provider_keys::read(&p).unwrap();
    assert_eq!(m.get("ANTHROPIC_API_KEY"), Some(&"sk-ant-valid".to_string()));
    assert_eq!(m.len(), 1);
}
```

- [ ] **Step 2: Implement `read`**

Add to `provider_keys.rs`:

```rust
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ProviderKeysError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("file {path} has permissions {mode:o}; expected 0600. \
             Fix with: chmod 600 {path}")]
    PermissionsTooOpen { path: String, mode: u32 },
}

/// Read the provider-keys file. Returns an empty map if the file does
/// not exist. Malformed lines (no `=`) are skipped with a warn log;
/// blank lines are ignored. Values are taken verbatim (no quote
/// stripping) — the writer doesn't quote, so the reader doesn't unquote.
pub fn read(path: &Path) -> Result<BTreeMap<String, String>, ProviderKeysError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(ProviderKeysError::Io(e)),
    };
    let mut out = BTreeMap::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) => {
                out.insert(k.trim().to_string(), v.to_string());
            }
            None => {
                tracing::warn!(line_no = i + 1, "skipping malformed line in provider-keys file");
            }
        }
    }
    Ok(out)
}
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 5 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): read parses dotenv format, tolerant of malformed lines"
```

### Task 1.4: `write_atomic` produces 0600 file in 0700 parent

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/tests/provider_keys.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/provider_keys.rs`:

```rust
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
#[cfg(unix)]
fn write_atomic_creates_0600_file_in_0700_parent() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("sub").join("providers.env");
    let mut vars = BTreeMap::new();
    vars.insert("ANTHROPIC_API_KEY".into(), "sk-ant-aaa".into());
    provider_keys::write_atomic(&p, &vars).unwrap();

    let f_mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(f_mode, 0o600, "file mode should be 0600, got {:o}", f_mode);
    let parent_mode = std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777;
    assert_eq!(parent_mode, 0o700, "parent dir mode should be 0700, got {:o}", parent_mode);
}

#[test]
fn write_atomic_round_trips_via_read() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut vars = BTreeMap::new();
    vars.insert("ANTHROPIC_API_KEY".into(), "sk-ant-aaa".into());
    vars.insert("OPENAI_API_KEY".into(), "sk-bbb".into());
    provider_keys::write_atomic(&p, &vars).unwrap();
    let back = provider_keys::read(&p).unwrap();
    assert_eq!(back, vars);
}
```

- [ ] **Step 2: Implement `write_atomic`**

Add to `provider_keys.rs`:

```rust
/// Write the provider-keys map atomically: tempfile in the same dir,
/// chmod 0600, then rename over the target. Parent dir created with
/// mode 0700 if missing. Atomic rename means a partial-write torn
/// state is impossible.
pub fn write_atomic(path: &Path, vars: &BTreeMap<String, String>) -> Result<(), ProviderKeysError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
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
        let mut perm = std::fs::metadata(temp.path())?.permissions();
        perm.set_mode(0o600);
        std::fs::set_permissions(temp.path(), perm)?;
    }

    temp.persist(path)
        .map_err(|e| ProviderKeysError::Io(e.error))?;
    Ok(())
}
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 7 passed (Unix) or 6 (non-Unix; the chmod test is `#[cfg(unix)]`).

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): write_atomic produces 0600 file in 0700 parent dir"
```

### Task 1.5: Strict mode-check on read

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/tests/provider_keys.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/provider_keys.rs`:

```rust
#[test]
#[cfg(unix)]
fn read_rejects_world_readable_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    std::fs::write(&p, "ANTHROPIC_API_KEY=x\n").unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o644);
    std::fs::set_permissions(&p, perm).unwrap();

    let err = provider_keys::read(&p).unwrap_err();
    assert!(
        matches!(err, provider_keys::ProviderKeysError::PermissionsTooOpen { .. }),
        "got {err:?}"
    );
}
```

- [ ] **Step 2: Add the mode-strict guard at the top of `read`**

In `provider_keys.rs`, replace the start of `read` to add the mode check on Unix. The check fires *before* parsing, so a too-open file is never read by accident:

```rust
pub fn read(path: &Path) -> Result<BTreeMap<String, String>, ProviderKeysError> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(ProviderKeysError::Io(e)),
    };

    #[cfg(unix)]
    {
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(ProviderKeysError::PermissionsTooOpen {
                path: path.display().to_string(),
                mode,
            });
        }
    }

    // (rest of existing parser unchanged)
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 8 passed (Unix).

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): read refuses files with mode > 0600 (Unix)"
```

### Task 1.6: `load_into_env_with` — pure, env-precedence respected

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/tests/provider_keys.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/provider_keys.rs`:

```rust
use std::cell::RefCell;

#[test]
fn load_into_env_with_skips_already_set_vars() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut vars = BTreeMap::new();
    vars.insert("ANTHROPIC_API_KEY".into(), "file-value".into());
    vars.insert("OPENAI_API_KEY".into(), "file-openai".into());
    provider_keys::write_atomic(&p, &vars).unwrap();

    let written: RefCell<BTreeMap<String, String>> = RefCell::new(BTreeMap::new());
    let read_env = |k: &str| {
        if k == "ANTHROPIC_API_KEY" { Some("env-wins".to_string()) } else { None }
    };
    let set_env = |k: &str, v: &str| {
        written.borrow_mut().insert(k.to_string(), v.to_string());
    };
    provider_keys::load_into_env_with(&p, read_env, set_env).unwrap();

    let w = written.borrow();
    assert_eq!(w.get("OPENAI_API_KEY"), Some(&"file-openai".to_string()), "file value loaded");
    assert!(!w.contains_key("ANTHROPIC_API_KEY"), "env-set var must not be overwritten");
}

#[test]
fn load_into_env_with_missing_file_is_silent_ok() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("nope.env");
    let written: RefCell<BTreeMap<String, String>> = RefCell::new(BTreeMap::new());
    let read_env = |_: &str| None;
    let set_env = |k: &str, v: &str| {
        written.borrow_mut().insert(k.to_string(), v.to_string());
    };
    provider_keys::load_into_env_with(&p, read_env, set_env).unwrap();
    assert!(written.borrow().is_empty());
}
```

- [ ] **Step 2: Implement `load_into_env_with` + thin wrapper**

Add to `provider_keys.rs`:

```rust
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

/// Production wrapper. Calls [`load_into_env_with`] against the real
/// process env, swallows missing-file (silent ok), logs other errors
/// as a single warning and continues. Called once from `main()`
/// before any CLI dispatch.
pub fn load_into_env_if_present() {
    let path = resolve_path();
    let result = load_into_env_with(
        &path,
        |k| std::env::var(k).ok(),
        // SAFETY: this runs from main() before any tokio runtime spawns
        // workers, so no other thread races on the env. Same justification
        // as `keyring::ensure_keyring_available`.
        |k, v| unsafe { std::env::set_var(k, v) },
    );
    if let Err(e) = result {
        tracing::warn!(error = %e, path = %path.display(), "failed to load provider-keys file");
    }
}
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 10 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): load_into_env honors env-over-file precedence"
```

---

## Phase 2 — `ProviderId` + `SetProviderKeys` CLI subcommand

### Task 2.1: `ProviderId` enum + provider→env-vars table

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/tests/provider_keys.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/provider_keys.rs`:

```rust
#[test]
fn provider_id_slug_round_trip_covers_all_variants() {
    use provider_keys::ProviderId;
    for p in ProviderId::ALL {
        let slug = p.slug();
        assert_eq!(ProviderId::from_slug(slug), Some(*p), "round trip failed for {slug}");
    }
}

#[test]
fn provider_id_env_vars_match_aether_llm() {
    use provider_keys::ProviderId;
    // Verified against
    // /home/mc/.opensrc/repos/github.com/contextbridge/aether/0.7.7/packages/llm/src/providers/
    // (anthropic/openrouter/openai/gemini all read a single env var; bedrock
    // uses the AWS SDK which honors AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY
    // / AWS_REGION).
    assert_eq!(ProviderId::Anthropic.env_vars(),  &["ANTHROPIC_API_KEY"]);
    assert_eq!(ProviderId::Openai.env_vars(),     &["OPENAI_API_KEY"]);
    assert_eq!(ProviderId::Openrouter.env_vars(), &["OPENROUTER_API_KEY"]);
    assert_eq!(ProviderId::Gemini.env_vars(),     &["GEMINI_API_KEY"]);
    assert_eq!(
        ProviderId::Bedrock.env_vars(),
        &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"]
    );
}

#[test]
fn provider_id_unknown_slug_returns_none() {
    assert_eq!(provider_keys::ProviderId::from_slug("claude"), None);
    assert_eq!(provider_keys::ProviderId::from_slug(""), None);
}
```

- [ ] **Step 2: Implement `ProviderId`**

Add to `provider_keys.rs`:

```rust
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
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 13 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): ProviderId enum + env-var table covers 5 providers"
```

### Task 2.2: `mask_value` for `--list` output

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/tests/provider_keys.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/provider_keys.rs`:

```rust
#[test]
fn mask_value_shows_short_prefix_and_last4() {
    assert_eq!(provider_keys::mask_value("sk-ant-1234567890abcd"), "sk-ant-***abcd");
}

#[test]
fn mask_value_handles_short_values() {
    // <=8 chars: don't risk leaking by showing prefix; mask entirely.
    assert_eq!(provider_keys::mask_value("short"), "***");
    assert_eq!(provider_keys::mask_value(""), "***");
}
```

- [ ] **Step 2: Implement `mask_value`**

Add to `provider_keys.rs`:

```rust
/// Mask a secret value for `--list` display. Long values become
/// `<7-char-prefix>***<last-4>`; values of 8 chars or less are masked
/// entirely (the prefix-plus-last4 form would leak too much of the
/// original).
pub fn mask_value(s: &str) -> String {
    if s.len() <= 8 {
        return "***".to_string();
    }
    let prefix: String = s.chars().take(7).collect();
    let last4: String = s.chars().rev().take(4).collect::<String>().chars().rev().collect();
    format!("{prefix}***{last4}")
}
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 15 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): mask_value helper for --list output"
```

### Task 2.3: Workspace + crate dep for `rpassword`

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/mcp-flowgate-tui/Cargo.toml`

- [ ] **Step 1: Add workspace pin**

Edit workspace `Cargo.toml` `[workspace.dependencies]`:

```toml
rpassword = "7"
```

- [ ] **Step 2: Use from tui crate**

Edit `crates/mcp-flowgate-tui/Cargo.toml` `[dependencies]`:

```toml
rpassword.workspace = true
```

- [ ] **Step 3: Verify build**

Run: `cargo check --workspace`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/mcp-flowgate-tui/Cargo.toml
git commit -m "chore(deps): add rpassword for no-echo prompts"
```

### Task 2.4: `SetProviderKeysArgs` + `run` (path, list, remove, set-from-stdin)

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/provider_keys.rs`
- Modify: `crates/mcp-flowgate-tui/tests/provider_keys.rs`

- [ ] **Step 1: Write the failing tests (file-mode operations only — interactive path tested manually)**

Append to `tests/provider_keys.rs`:

```rust
#[test]
fn set_var_upserts_one_var_preserving_others() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut existing = BTreeMap::new();
    existing.insert("OPENAI_API_KEY".into(), "sk-keep".into());
    provider_keys::write_atomic(&p, &existing).unwrap();

    provider_keys::set_var(&p, "ANTHROPIC_API_KEY", "sk-new").unwrap();

    let back = provider_keys::read(&p).unwrap();
    assert_eq!(back.get("OPENAI_API_KEY"), Some(&"sk-keep".to_string()));
    assert_eq!(back.get("ANTHROPIC_API_KEY"), Some(&"sk-new".to_string()));
    assert_eq!(back.len(), 2);
}

#[test]
fn remove_provider_deletes_only_that_providers_vars() {
    use provider_keys::ProviderId;
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("providers.env");
    let mut existing = BTreeMap::new();
    existing.insert("ANTHROPIC_API_KEY".into(), "sk-ant".into());
    existing.insert("OPENAI_API_KEY".into(), "sk-oai".into());
    existing.insert("AWS_ACCESS_KEY_ID".into(), "AKIA".into());
    existing.insert("AWS_SECRET_ACCESS_KEY".into(), "secret".into());
    existing.insert("AWS_REGION".into(), "us-east-1".into());
    provider_keys::write_atomic(&p, &existing).unwrap();

    provider_keys::remove_provider(&p, ProviderId::Bedrock).unwrap();

    let back = provider_keys::read(&p).unwrap();
    assert_eq!(back.get("ANTHROPIC_API_KEY"), Some(&"sk-ant".to_string()));
    assert_eq!(back.get("OPENAI_API_KEY"), Some(&"sk-oai".to_string()));
    assert!(!back.contains_key("AWS_ACCESS_KEY_ID"));
    assert!(!back.contains_key("AWS_SECRET_ACCESS_KEY"));
    assert!(!back.contains_key("AWS_REGION"));
}
```

- [ ] **Step 2: Implement `set_var` + `remove_provider` + args struct**

Add to `provider_keys.rs`:

```rust
use std::process::ExitCode;

/// Upsert a single env var in the file. Reads, mutates one key, writes
/// atomically. Use this for the CLI `set` path; the interactive walker
/// composes multiple `set_var` calls.
pub fn set_var(path: &Path, key: &str, value: &str) -> Result<(), ProviderKeysError> {
    let mut vars = read(path).unwrap_or_default();
    vars.insert(key.to_string(), value.to_string());
    write_atomic(path, &vars)
}

/// Delete every env var that belongs to the given provider.
pub fn remove_provider(path: &Path, provider: ProviderId) -> Result<(), ProviderKeysError> {
    let mut vars = read(path).unwrap_or_default();
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
        let provider = ProviderId::from_slug(slug)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{slug}'. Valid: {}", valid_slugs()))?;
        remove_provider(&path, provider)?;
        eprintln!("removed {} keys from {}", provider.display(), path.display());
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(slug) = args.provider.as_deref() {
        let provider = ProviderId::from_slug(slug)
            .ok_or_else(|| anyhow::anyhow!("unknown provider '{slug}'. Valid: {}", valid_slugs()))?;
        return run_set_one(&path, provider, args.stdin);
    }

    run_interactive(&path)
}

fn valid_slugs() -> String {
    ProviderId::ALL.iter().map(|p| p.slug()).collect::<Vec<_>>().join(", ")
}

fn run_list(path: &Path) -> anyhow::Result<ExitCode> {
    let vars = read(path).unwrap_or_default();
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

fn run_set_one(path: &Path, provider: ProviderId, from_stdin: bool) -> anyhow::Result<ExitCode> {
    for env_var in provider.env_vars() {
        let value = if from_stdin {
            let mut s = String::new();
            std::io::stdin().read_line(&mut s)?;
            s.trim_end().to_string()
        } else {
            rpassword::prompt_password(format!("{} ({env_var}): ", provider.display()))?
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
            let value = rpassword::prompt_password(format!("  {env_var}: "))?;
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
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: 17 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/provider_keys.rs crates/mcp-flowgate-tui/tests/provider_keys.rs
git commit -m "feat(provider-keys): set_var/remove_provider + SetProviderKeysArgs + run dispatcher"
```

### Task 2.5: Wire into `main.rs` (new variant + startup hook)

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/main.rs`
- Create: `crates/mcp-flowgate-tui/tests/cli_help_surface.rs`

- [ ] **Step 1: Write the failing test (CLI integration smoke)**

Create `crates/mcp-flowgate-tui/tests/cli_help_surface.rs`:

```rust
use std::process::Command;

fn binary() -> String {
    env!("CARGO_BIN_EXE_flowgate").to_string()
}

#[test]
fn set_provider_keys_is_listed_in_help() {
    let out = Command::new(binary()).arg("--help").output().expect("run --help");
    assert!(out.status.success(), "--help failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("set-provider-keys"),
        "expected set-provider-keys in --help, got:\n{stdout}"
    );
}

#[test]
fn set_provider_keys_help_mentions_providers_file() {
    let out = Command::new(binary())
        .args(["set-provider-keys", "--help"])
        .output()
        .expect("run set-provider-keys --help");
    assert!(out.status.success(), "subcommand --help failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--provider") && stdout.contains("--list"));
}

#[test]
fn set_provider_keys_path_prints_resolved_path() {
    // Override to a known scratch path so the test doesn't depend on $HOME.
    let dir = tempfile::tempdir().unwrap();
    let want = dir.path().join("custom.env");
    let out = Command::new(binary())
        .env("FLOWGATE_PROVIDER_KEYS_FILE", &want)
        .args(["set-provider-keys", "--path"])
        .output()
        .expect("run set-provider-keys --path");
    assert!(out.status.success(), "--path failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), want.to_string_lossy());
}
```

- [ ] **Step 2: Wire the variant + the startup hook**

Edit `crates/mcp-flowgate-tui/src/main.rs`:

Add to imports near the existing `use mcp_flowgate_tui::...` block:

```rust
use mcp_flowgate_tui::provider_keys;
```

In the `enum Command { ... }`, add a new variant **after** `MigrateAgentsFromCli`:

```rust
    /// Write provider API keys to ~/.config/flowgate/providers.env
    /// (override via $FLOWGATE_PROVIDER_KEYS_FILE). Loaded into env at
    /// flowgate-agent startup; existing env vars take precedence.
    /// Supported providers: anthropic, openai, openrouter, bedrock,
    /// gemini.
    SetProviderKeys(provider_keys::SetProviderKeysArgs),
```

In `main()`, **immediately after** `keyring::ensure_keyring_available();` and before `match cli.command`, add:

```rust
    provider_keys::load_into_env_if_present();
```

In the dispatch `match` block, add the new arm before the closing brace:

```rust
        Some(Command::SetProviderKeys(args)) => provider_keys::run(args),
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test cli_help_surface` and `cargo test -p mcp-flowgate-tui --test provider_keys`
Expected: both green; cli_help_surface 3 passed.

Then full workspace regression: `cargo test --workspace`
Expected: all green (no existing test broken).

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/main.rs crates/mcp-flowgate-tui/tests/cli_help_surface.rs
git commit -m "feat(provider-keys): wire SetProviderKeys subcommand + startup env-load"
```

---

## Phase 3 — `clap_complete` `completions <shell>` subcommand

### Task 3.1: Workspace + crate dep for `clap_complete`

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/mcp-flowgate-tui/Cargo.toml`

- [ ] **Step 1: Add workspace pin**

Edit workspace `Cargo.toml` `[workspace.dependencies]`:

```toml
clap_complete = "4"
```

- [ ] **Step 2: Use from tui crate**

Edit `crates/mcp-flowgate-tui/Cargo.toml` `[dependencies]`:

```toml
clap_complete.workspace = true
```

- [ ] **Step 3: Verify build**

Run: `cargo check --workspace`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/mcp-flowgate-tui/Cargo.toml
git commit -m "chore(deps): add clap_complete for shell completion generator"
```

### Task 3.2: `completions <shell>` subcommand

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/main.rs`
- Modify: `crates/mcp-flowgate-tui/tests/cli_help_surface.rs`

- [ ] **Step 1: Write the failing tests**

Append to `tests/cli_help_surface.rs`:

```rust
#[test]
fn completions_bash_emits_nonempty_script_with_command_name() {
    let out = Command::new(binary())
        .args(["completions", "bash"])
        .output()
        .expect("run completions bash");
    assert!(out.status.success(), "completions bash failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.is_empty(), "expected non-empty completion script");
    assert!(
        stdout.contains("flowgate"),
        "expected completion script to reference 'flowgate'; got first 200 chars:\n{}",
        &stdout.chars().take(200).collect::<String>()
    );
}

#[test]
fn completions_zsh_also_works() {
    let out = Command::new(binary())
        .args(["completions", "zsh"])
        .output()
        .expect("run completions zsh");
    assert!(out.status.success(), "completions zsh failed: {:?}", out);
    assert!(!out.stdout.is_empty());
}
```

- [ ] **Step 2: Add the variant + handler**

Edit `crates/mcp-flowgate-tui/src/main.rs`:

Add imports near the top:

```rust
use clap::CommandFactory;
use clap_complete::Shell;
```

Add a new `Command` variant after `SetProviderKeys`:

```rust
    /// Print a shell completion script to stdout. Source it from your
    /// shell rc to get tab-completion for every flowgate subcommand
    /// and flag. Example:
    ///   flowgate completions bash > ~/.local/share/bash-completion/completions/flowgate
    Completions(CompletionsArgs),
```

Add the args struct near the other `#[derive(clap::Args)]` blocks:

```rust
#[derive(clap::Args, Debug)]
pub struct CompletionsArgs {
    /// Shell to generate completions for (bash, zsh, fish, powershell, elvish).
    pub shell: Shell,
}
```

Add a free function below `main`:

```rust
fn run_completions(args: CompletionsArgs) -> ExitCode {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(args.shell, &mut cmd, name, &mut std::io::stdout());
    ExitCode::SUCCESS
}
```

Add the match arm in `main()`:

```rust
        Some(Command::Completions(args)) => Ok(run_completions(args)),
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test cli_help_surface`
Expected: 5 passed (3 from earlier + 2 new).

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/main.rs crates/mcp-flowgate-tui/tests/cli_help_surface.rs
git commit -m "feat(cli-help): completions <shell> subcommand via clap_complete"
```

---

## Phase 4 — `clap_mangen` `man` subcommand

### Task 4.1: Workspace + crate dep for `clap_mangen`

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/mcp-flowgate-tui/Cargo.toml`

- [ ] **Step 1: Add workspace pin**

Edit workspace `Cargo.toml` `[workspace.dependencies]`:

```toml
clap_mangen = "0.2"
```

- [ ] **Step 2: Use from tui crate**

Edit `crates/mcp-flowgate-tui/Cargo.toml` `[dependencies]`:

```toml
clap_mangen.workspace = true
```

- [ ] **Step 3: Verify build**

Run: `cargo check --workspace`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/mcp-flowgate-tui/Cargo.toml
git commit -m "chore(deps): add clap_mangen for man-page generator"
```

### Task 4.2: `man` subcommand

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/main.rs`
- Modify: `crates/mcp-flowgate-tui/tests/cli_help_surface.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/cli_help_surface.rs`:

```rust
#[test]
fn man_emits_roff_document() {
    let out = Command::new(binary())
        .arg("man")
        .output()
        .expect("run man");
    assert!(out.status.success(), "man failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with(".TH") || stdout.contains("\n.TH"),
        "expected roff `.TH` header; got first 200 chars:\n{}",
        &stdout.chars().take(200).collect::<String>()
    );
}
```

- [ ] **Step 2: Add the variant + handler**

Edit `crates/mcp-flowgate-tui/src/main.rs`:

Add a new `Command` variant after `Completions`:

```rust
    /// Render the man page to stdout (roff format). Install to a
    /// MANPATH directory to enable `man flowgate`. Example:
    ///   flowgate man | sudo tee /usr/local/share/man/man1/flowgate.1 > /dev/null
    Man,
```

Add a free function below `run_completions`:

```rust
fn run_man() -> anyhow::Result<ExitCode> {
    let cmd = Cli::command();
    let man = clap_mangen::Man::new(cmd);
    man.render(&mut std::io::stdout())
        .map_err(|e| anyhow::anyhow!("man render failed: {e}"))?;
    Ok(ExitCode::SUCCESS)
}
```

Add the match arm in `main()`:

```rust
        Some(Command::Man) => run_man(),
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test cli_help_surface`
Expected: 6 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/main.rs crates/mcp-flowgate-tui/tests/cli_help_surface.rs
git commit -m "feat(cli-help): man subcommand via clap_mangen"
```

---

## Phase 5 — `long_about` backfill + `next_help_heading` groupings

### Task 5.1: Group existing subcommands under headings

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/main.rs`
- Modify: `crates/mcp-flowgate-tui/tests/cli_help_surface.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/cli_help_surface.rs`:

```rust
#[test]
fn top_level_help_groups_subcommands_under_headings() {
    let out = Command::new(binary()).arg("--help").output().expect("--help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for heading in ["Agent runtime", "Agent configuration", "Diagnostics & generators"] {
        assert!(
            stdout.contains(heading),
            "expected heading '{heading}' in --help; got:\n{stdout}"
        );
    }
    // Every command must still be listed.
    for cmd in [
        "headless", "acp", "agent", "walk", "doctor", "mcp",
        "validate-agents-config", "migrate-agents-from-cli",
        "set-provider-keys", "completions", "man",
    ] {
        assert!(stdout.contains(cmd), "expected '{cmd}' in --help; got:\n{stdout}");
    }
}
```

- [ ] **Step 2: Add `next_help_heading` to each variant**

Edit `crates/mcp-flowgate-tui/src/main.rs`. On each variant of `enum Command`, prepend the matching `#[command(next_help_heading = "...")]`:

| Variant | Heading |
|---|---|
| `Headless` | `"Agent runtime"` |
| `Acp` | `"Agent runtime"` |
| `Walk` | `"Agent runtime"` |
| `Agent` | `"Agent configuration"` |
| `SetProviderKeys` | `"Agent configuration"` |
| `MigrateAgentsFromCli` | `"Agent configuration"` |
| `ValidateAgentsConfig` | `"Agent configuration"` |
| `Doctor` | `"Diagnostics & generators"` |
| `Mcp` | `"Diagnostics & generators"` |
| `Completions` | `"Diagnostics & generators"` |
| `Man` | `"Diagnostics & generators"` |

Pattern for each:

```rust
    #[command(next_help_heading = "Agent runtime")]
    Headless(aether_cli::headless::HeadlessArgs),
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test cli_help_surface`
Expected: 7 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/main.rs crates/mcp-flowgate-tui/tests/cli_help_surface.rs
git commit -m "feat(cli-help): group subcommands under three headings in --help"
```

### Task 5.2: Backfill `long_about` on every variant

**Files:**
- Modify: `crates/mcp-flowgate-tui/src/main.rs`
- Modify: `crates/mcp-flowgate-tui/tests/cli_help_surface.rs`

- [ ] **Step 1: Write the failing test**

Append to `tests/cli_help_surface.rs`:

```rust
#[test]
fn walk_long_help_mentions_deterministic_interpreter() {
    let out = Command::new(binary())
        .args(["help", "walk"])
        .output()
        .expect("help walk");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("deterministic interpreter"),
        "expected `help walk` to surface long_about; got:\n{stdout}"
    );
}

#[test]
fn doctor_long_help_mentions_preflight() {
    let out = Command::new(binary())
        .args(["help", "doctor"])
        .output()
        .expect("help doctor");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.to_lowercase().contains("pre-flight") || stdout.to_lowercase().contains("preflight"));
}

#[test]
fn set_provider_keys_long_help_mentions_file_location() {
    let out = Command::new(binary())
        .args(["help", "set-provider-keys"])
        .output()
        .expect("help set-provider-keys");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("providers.env"));
}
```

- [ ] **Step 2: Add `long_about` to every variant**

Edit `crates/mcp-flowgate-tui/src/main.rs`. For each variant, augment the existing `#[command(next_help_heading = "...")]` to also carry `long_about = "..."`. Example for `Walk`:

```rust
    #[command(
        next_help_heading = "Agent runtime",
        long_about = "Walk a Flowgate workflow end-to-end through the deterministic interpreter \
                      (SPEC §21). Spawns isolated sub-agents per `delegate:` state; \
                      auto-advances states with no delegate. Returns the final blackboard \
                      JSON on stdout.\n\n\
                      Example:\n  \
                      flowgate walk --workflow swe_agent \\\n    \
                      --input '{\"issue\":\"add timeout to RegistryExecutor\"}' \\\n    \
                      --agent planning=anthropic/claude-sonnet-4 \\\n    \
                      --agent editing=anthropic/claude-haiku-4-5-20251001"
    )]
    Walk(WalkArgs),
```

Apply the same shape to every variant. Suggested copy (compress / adapt where needed; the test asserts only the keyword each):

- **Headless:** "Run a single prompt non-interactively. Flowgate is wired as the sole MCP server, so every model action goes through governed workflows."
- **Acp:** "Start the ACP (Agent Client Protocol) server for editor integration. The TUI spawns this mode as a subprocess; editors connect via ACP."
- **Agent:** "Manage agent configurations: create, list, remove. Settings live under `.aether/settings.json` per the upstream aether convention."
- **Walk:** as above (mentions deterministic interpreter).
- **Doctor:** "Pre-flight checks before `flowgate walk` — binary discovery, config resolution, workflow declared, agent API keys reachable, script file URIs hash-verified. Exits 0 if all pass; 1 if any fail."
- **Mcp:** "MCP client config generators. `mcp init` writes `.mcp.json` (and optional editor-specific outputs) so MCP hosts see flowgate as the sole MCP server."
- **ValidateAgentsConfig:** "Validate an `agents.yaml` file at an arbitrary path. Emits a JSON envelope `{ok, summary, error}` on stdout; exits 0 on pass, 1 on fail."
- **MigrateAgentsFromCli:** "Migrate v0.2 `--agent NAME=PROVIDER/MODEL` flags to a v0.3 `agents.yaml`. Operators with many workflows still on the legacy CLI path can run this once + commit the file."
- **SetProviderKeys:** "Write provider API keys to `~/.config/flowgate/providers.env` (override via `$FLOWGATE_PROVIDER_KEYS_FILE`). Loaded into env at flowgate-agent startup; existing env vars take precedence. Without flags, interactively walks all supported providers (anthropic, openai, openrouter, bedrock, gemini)."
- **Completions:** "Print a shell completion script to stdout. Source it from your shell rc to tab-complete subcommands and flags."
- **Man:** "Render the man page to stdout (roff format). Install to a MANPATH directory to enable `man flowgate`."

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p mcp-flowgate-tui --test cli_help_surface`
Expected: 10 passed.

Full regression: `cargo test --workspace` then `cargo clippy --workspace --all-targets -- -D warnings`.
Expected: both green.

- [ ] **Step 4: Commit**

```bash
git add crates/mcp-flowgate-tui/src/main.rs crates/mcp-flowgate-tui/tests/cli_help_surface.rs
git commit -m "feat(cli-help): long_about on every subcommand for help <cmd>"
```

---

## Phase 6 — README "Discovering commands" section

### Task 6.1: Document the help surface

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add the section**

Open `README.md`. Find the existing CLI usage section (around the `flowgate walk` example block, near "The TUI agent — commodity models outperform frontier"). Add a new sibling section directly after the install instructions:

````markdown
## Discovering commands

`flowgate` ships with a full clap-driven CLI. Four ways to find what's available:

```bash
flowgate --help                    # grouped list of every subcommand
flowgate help <subcommand>         # long-form description + example
flowgate completions bash          # emit a tab-completion script for your shell
flowgate man | less                # roff-formatted man page (install with: sudo tee /usr/local/share/man/man1/flowgate.1)
```

Commands are grouped under three headings:

- **Agent runtime** — `headless`, `acp`, `walk` (interactive TUI is the default with no subcommand).
- **Agent configuration** — `agent`, `set-provider-keys`, `migrate-agents-from-cli`, `validate-agents-config`.
- **Diagnostics & generators** — `doctor`, `mcp init`, `completions`, `man`.

### Setting provider API keys

`flowgate set-provider-keys` writes a flat dotenv file at `~/.config/flowgate/providers.env` (mode 0600, parent dir 0700) which is loaded into env at startup. Existing environment variables take precedence, so CI / shell exports always win over the file.

```bash
flowgate set-provider-keys                          # interactive walk through all 5 providers
flowgate set-provider-keys --provider anthropic     # one provider, no-echo prompt
echo "$KEY" | flowgate set-provider-keys --provider openrouter --stdin
flowgate set-provider-keys --list                   # show configured providers (masked)
flowgate set-provider-keys --remove gemini          # clear one provider's vars
flowgate set-provider-keys --path                   # print the resolved file path
```

Override the file location with `$FLOWGATE_PROVIDER_KEYS_FILE`. Supported providers: `anthropic`, `openai`, `openrouter`, `bedrock`, `gemini`.
````

- [ ] **Step 2: Verify the file still renders sensibly**

Run: `head -200 README.md | grep -n "^## "` to confirm section ordering.
Expected: "Discovering commands" appears in the right place (after install, before "Try it in 30 seconds" or wherever it best fits — adjust placement at author discretion).

- [ ] **Step 3: Final workspace regression**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: both green.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs(readme): Discovering commands section + set-provider-keys usage"
```

---

## Acceptance criteria (the whole branch)

When all phases are complete:

- [ ] `flowgate set-provider-keys` interactively walks all five providers, writes a 0600 dotenv file under a 0700 parent dir at `~/.config/flowgate/providers.env`, masks values in `--list`, and removes per-provider on `--remove`.
- [ ] Setting `$FLOWGATE_PROVIDER_KEYS_FILE` redirects the file.
- [ ] On every flowgate startup (TUI, headless, acp, walk, doctor), keys from the file are loaded into env — but **only for vars not already set** in the environment.
- [ ] `flowgate --help` shows all 11 subcommands grouped under "Agent runtime" / "Agent configuration" / "Diagnostics & generators".
- [ ] `flowgate help <cmd>` shows a long-form description with an example for every subcommand.
- [ ] `flowgate completions <shell>` emits a working completion script (verified for bash + zsh in tests).
- [ ] `flowgate man` emits a roff document.
- [ ] `cargo test --workspace` is green.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- [ ] Pre-existing tests are untouched; no test was deleted or `#[ignore]`d.
- [ ] CHANGELOG entry added under the next `[Unreleased]` block (this is conventional in the repo; check `CHANGELOG.md` for the existing pattern before adding).

## Out of scope (deliberately deferred)

- TUI slash command `/set-provider-keys` (upstream `aether-tui` change; not ours to add).
- Keyring backend via `aether-auth::OsKeyringStore` (file backend ships v1; keyring lands behind `--backend keyring` in a follow-up).
- Per-project key overrides (`./flowgate.providers.env`).
- Encryption-at-rest (`age` / `sops`).
- Topic-based help (`flowgate help <topic>` for narrative docs); cross-link to README from `long_about` instead.
- `flowgate set-provider-keys --import-env` to bootstrap from existing shell env.

---

## Self-review

Ran against the spec written in conversation; gaps found and fixed inline:

1. ✅ Spec coverage: file backend (Phase 1), CLI surface incl. interactive (Phase 2.4), startup hook (Phase 2.5), Anthropic + OpenRouter providers added (Phase 2.1's enum), mode 0600 + parent 0700 (Phase 1.4), env-overrides-file (Phase 1.6), idempotency via atomic write (Phase 1.4), composition with `keyring::ensure_keyring_available` (Phase 2.5 places the hook after it), discoverable help (Phases 3–5).
2. ✅ Placeholder scan: no TBD / "implement later" / "add validation" — every step shows complete code.
3. ✅ Type consistency: `ProviderId` slug strings (`anthropic` etc.) used identically across enum, tests, error messages, and README. `SetProviderKeysArgs` field names match between Phase 2.4 (definition), Phase 2.5 (wiring), and Phase 2.4 tests.
4. ✅ Verified-against-source: env var names (ANTHROPIC_API_KEY, GEMINI_API_KEY, AWS_*) cite `/home/mc/.opensrc/repos/github.com/contextbridge/aether/0.7.7/packages/llm/src/providers/`.
