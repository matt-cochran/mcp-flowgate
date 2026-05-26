//! `flowgate doctor` — pre-flight checks for `flowgate walk`.
//!
//! Each check returns a structured `CheckResult` so callers (the CLI
//! subcommand, tests) can format output and assert on specific failures.
//!
//! Contract (SPEC §29 / Tranche 3): if `doctor` passes, `walk` will at
//! least START successfully. Doctor does NOT claim walk will SUCCEED
//! (that depends on the model). Each check ties to a specific failure
//! mode `walk` would surface less clearly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::flowgate_mcp::find_flowgate_binary;

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Fail(String), // identifier like MCP_FLOWGATE_NOT_FOUND for assertions
    Skip(String), // not applicable (e.g. workflow not specified)
}

impl CheckResult {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }
    fn fail(name: impl Into<String>, code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail(code.into()),
            detail: detail.into(),
        }
    }
    fn skip(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Skip(reason.into()),
            detail: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DoctorArgs {
    pub config: Option<String>,
    pub workflow: Option<String>,
    pub agents: Vec<String>,
}

/// Run all pre-flight checks in order. Returns the per-check results.
/// Caller decides exit code based on whether any `Fail` is present.
pub async fn run_doctor(args: &DoctorArgs) -> Vec<CheckResult> {
    let mut results = Vec::new();

    // 1. mcp-flowgate binary discoverable
    match find_flowgate_binary() {
        Ok(path) => results.push(CheckResult::pass("mcp-flowgate binary", path)),
        Err(e) => results.push(CheckResult::fail(
            "mcp-flowgate binary",
            "MCP_FLOWGATE_NOT_FOUND",
            format!(
                "{e} — install with `cargo install mcp-flowgate` or set MCP_FLOWGATE_PATH"
            ),
        )),
    }

    // 2. Config file
    let config_path = args
        .config
        .clone()
        .or_else(|| std::env::var("FLOWGATE_CONFIG").ok());
    let resolved_config: Option<Value> = match &config_path {
        None => {
            results.push(CheckResult::skip(
                "config file",
                "no --config or FLOWGATE_CONFIG; checks 3-5 will be skipped",
            ));
            None
        }
        Some(p) => {
            let path = Path::new(p);
            if !path.exists() {
                results.push(CheckResult::fail(
                    "config file",
                    "CONFIG_NOT_FOUND",
                    format!("{p} does not exist"),
                ));
                None
            } else {
                results.push(CheckResult::pass("config file", p));
                // 3. Config parses + resolves
                match resolve_config(path) {
                    Ok(cfg) => {
                        let n_workflows = cfg
                            .pointer("/workflows")
                            .and_then(Value::as_object)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        let n_skills = cfg
                            .pointer("/skills")
                            .and_then(Value::as_object)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        let n_scripts = cfg
                            .pointer("/scripts")
                            .and_then(Value::as_object)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        results.push(CheckResult::pass(
                            "config parses + resolves",
                            format!(
                                "{n_workflows} workflows, {n_skills} skills, {n_scripts} scripts"
                            ),
                        ));
                        Some(cfg)
                    }
                    Err(e) => {
                        results.push(CheckResult::fail(
                            "config parses + resolves",
                            "CONFIG_INVALID",
                            e.to_string(),
                        ));
                        None
                    }
                }
            }
        }
    };

    // 4. Workflow declared
    if let Some(cfg) = &resolved_config {
        if let Some(wf_name) = &args.workflow {
            if cfg
                .pointer(&format!("/workflows/{wf_name}"))
                .is_some()
            {
                results.push(CheckResult::pass("workflow declared", wf_name));
            } else {
                let available: Vec<&str> = cfg
                    .pointer("/workflows")
                    .and_then(Value::as_object)
                    .map(|m| m.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                results.push(CheckResult::fail(
                    "workflow declared",
                    "WORKFLOW_NOT_DECLARED",
                    format!(
                        "--workflow '{wf_name}' not found in config. Available: {available:?}"
                    ),
                ));
            }
        } else {
            results.push(CheckResult::skip(
                "workflow declared",
                "no --workflow argument",
            ));
        }
    }

    // 5. Per-agent API key present (parse `name=provider/model`)
    if args.agents.is_empty() {
        results.push(CheckResult::skip(
            "agent API keys",
            "no --agent arguments",
        ));
    } else {
        for spec in &args.agents {
            let parts: Vec<&str> = spec.splitn(2, '=').collect();
            let Some(name) = parts.first() else { continue };
            let Some(rest) = parts.get(1) else {
                results.push(CheckResult::fail(
                    format!("agent: {spec}"),
                    "AGENT_SPEC_INVALID",
                    format!("expected `name=provider/model`, got '{spec}'"),
                ));
                continue;
            };
            let prov_model: Vec<&str> = rest.splitn(2, '/').collect();
            let Some(provider) = prov_model.first().filter(|s| !s.is_empty()) else {
                results.push(CheckResult::fail(
                    format!("agent: {name}"),
                    "AGENT_SPEC_INVALID",
                    format!("missing provider in '{spec}'"),
                ));
                continue;
            };
            let env_var = provider_env_var(provider);
            if std::env::var(env_var).is_ok() {
                results.push(CheckResult::pass(
                    format!("agent: {name}"),
                    format!("{env_var} set"),
                ));
            } else {
                results.push(CheckResult::fail(
                    format!("agent: {name}"),
                    "MISSING_API_KEY",
                    format!("provider '{provider}' needs {env_var}"),
                ));
            }
        }
    }

    // 6. Script URIs (file:// only — https / git+https are load-time fetched)
    if let Some(cfg) = &resolved_config {
        if let Some(scripts) = cfg.pointer("/scripts").and_then(Value::as_object) {
            let mut missing: Vec<String> = Vec::new();
            for (subject, decl) in scripts {
                let Some(uri) = decl.get("uri").and_then(Value::as_str) else {
                    continue; // inline body, nothing to check
                };
                if let Some(path) = uri.strip_prefix("file://") {
                    if !Path::new(path).exists() {
                        missing.push(format!("{subject}: {uri}"));
                    }
                }
            }
            if missing.is_empty() {
                results.push(CheckResult::pass(
                    "script file:// URIs",
                    format!("{} script(s) verified", scripts.len()),
                ));
            } else {
                results.push(CheckResult::fail(
                    "script file:// URIs",
                    "SCRIPT_URI_MISSING",
                    format!("missing files: {}", missing.join(", ")),
                ));
            }
        }
    }

    results
}

fn resolve_config(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)?;
    let value: Value = serde_yaml::from_str(&raw)?;
    let resolved = mcp_flowgate_core::config::resolve(value)?;
    Ok(resolved)
}

fn provider_env_var(provider: &str) -> &'static str {
    // Static map — every known provider's env var name. Add to this
    // when adding provider support to AgentConfig.
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "ollama" => "OLLAMA_HOST", // local; not actually a key but the host needs to be set
        _ => "(unknown_provider_env_var)",
    }
}

/// Render results as human-readable text. ANSI color when stdout is a TTY.
pub fn render_results(results: &[CheckResult]) -> String {
    let use_color = atty_stdout();
    let mut out = String::new();
    let mut failed = 0;
    for r in results {
        let (mark, color) = match &r.status {
            CheckStatus::Pass => ("✓", "\x1b[32m"),
            CheckStatus::Fail(_) => {
                failed += 1;
                ("✗", "\x1b[31m")
            }
            CheckStatus::Skip(_) => ("-", "\x1b[90m"),
        };
        let reset = if use_color { "\x1b[0m" } else { "" };
        let color = if use_color { color } else { "" };
        let prefix = format!("{color}{mark}{reset}");
        match &r.status {
            CheckStatus::Pass => {
                out.push_str(&format!(
                    "  {prefix} {:<35} {}\n",
                    r.name, r.detail
                ));
            }
            CheckStatus::Fail(code) => {
                out.push_str(&format!(
                    "  {prefix} {:<35} {code}: {}\n",
                    r.name, r.detail
                ));
            }
            CheckStatus::Skip(reason) => {
                out.push_str(&format!(
                    "  {prefix} {:<35} (skipped: {reason})\n",
                    r.name
                ));
            }
        }
    }
    out.push('\n');
    if failed == 0 {
        out.push_str("doctor: all checks passed.\n");
    } else {
        out.push_str(&format!(
            "doctor: {failed} check(s) failed. Resolve the above before running `flowgate walk`.\n"
        ));
    }
    out
}

fn atty_stdout() -> bool {
    // Cheap TTY detection without an extra crate dep. `isatty(1)`
    // returns non-zero on a TTY.
    use std::os::fd::AsRawFd;
    let fd = std::io::stdout().as_raw_fd();
    libc_isatty(fd)
}

// Tiny FFI shim to avoid pulling in the `libc` crate just for this.
extern "C" {
    fn isatty(fd: i32) -> i32;
}
fn libc_isatty(fd: i32) -> bool {
    // SAFETY: libc::isatty just reads fd flags, no side effects.
    unsafe { isatty(fd) != 0 }
}

/// Counts how many checks failed. Caller uses this to set exit code.
pub fn count_failures(results: &[CheckResult]) -> usize {
    results
        .iter()
        .filter(|r| matches!(r.status, CheckStatus::Fail(_)))
        .count()
}

// Suppress unused-import warnings when libc-shim path is taken.
#[allow(dead_code)]
fn _drop_unused() {
    let _: Option<HashMap<String, String>> = None;
    let _: Option<PathBuf> = None;
}
