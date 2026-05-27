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

use crate::agent_resolver::{AgentsFile, ConfigSource, Delegate, Resolver};
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

    // 7. agents.yaml (v0.3+) presence + parse. Both project and user
    //    files are reported; mutual-presence is flagged as a shadow.
    let resolver = check_agents_yaml(&mut results);

    // 8. Workflow delegates ↔ resolver coverage. For each `delegate:`
    //    string in the resolved workflow, run the resolver's walk and
    //    report the chosen level. Names every delegate whose only
    //    match is `default` (operator-visible "silent downgrade" list).
    if let (Some(r), Some(cfg), Some(wf_name)) = (&resolver, &resolved_config, &args.workflow) {
        check_workflow_delegate_coverage(&mut results, r, cfg, wf_name);
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

/// Load agents.yaml (project then user) and add CheckResults describing
/// presence, parse status, and shadowing. Returns the project resolver
/// when one is loadable, so the caller can run the delegate-coverage
/// check on the SAME resolver the operator's `flowgate walk` will use.
fn check_agents_yaml(results: &mut Vec<CheckResult>) -> Option<Resolver> {
    let project_path = std::path::Path::new(".flowgate").join("agents.yaml");
    let user_path = dirs::config_dir().map(|d| d.join("flowgate").join("agents.yaml"));

    let project_present = project_path.exists();
    let user_present = user_path.as_ref().is_some_and(|p| p.exists());

    if !project_present && !user_present {
        results.push(CheckResult::skip(
            "agents.yaml",
            "no project (.flowgate/agents.yaml) or user (~/.config/flowgate/agents.yaml) file",
        ));
        return None;
    }

    // Load whichever takes precedence (project first, then user).
    let (chosen_path, chosen_source, shadowed_path) = if project_present {
        let shadow = if user_present {
            user_path.clone()
        } else {
            None
        };
        (
            project_path.clone(),
            ConfigSource::Project(project_path.clone()),
            shadow,
        )
    } else {
        let p = user_path.clone().unwrap();
        (p.clone(), ConfigSource::User(p), None)
    };

    match AgentsFile::from_path(&chosen_path) {
        Ok(file) => {
            results.push(CheckResult::pass(
                "agents.yaml",
                format!(
                    "loaded {} ({} default binding(s), {} override(s)){}",
                    chosen_path.display(),
                    file.default.len(),
                    file.overrides.len(),
                    if file.strict_specificity {
                        ", strict_specificity=true"
                    } else {
                        ""
                    },
                ),
            ));
            if let Some(s) = shadowed_path {
                results.push(CheckResult::pass(
                    "agents.yaml shadow",
                    format!(
                        "project ({}) shadows user ({}) — user's bindings are NOT in effect",
                        chosen_path.display(),
                        s.display()
                    ),
                ));
            }
            Some(Resolver::from_loaded(file, chosen_source))
        }
        Err(e) => {
            results.push(CheckResult::fail(
                "agents.yaml",
                "AGENTS_YAML_PARSE_FAILED",
                format!("{}: {e}", chosen_path.display()),
            ));
            None
        }
    }
}

/// Walk the resolved config's workflow definition for `delegate:`
/// strings, run each through the resolver, and emit one CheckResult
/// per delegate that names the specificity level chosen.
fn check_workflow_delegate_coverage(
    results: &mut Vec<CheckResult>,
    resolver: &Resolver,
    cfg: &Value,
    wf_name: &str,
) {
    let states = cfg
        .pointer(&format!("/workflows/{wf_name}/states"))
        .and_then(Value::as_object);
    let Some(states) = states else {
        results.push(CheckResult::skip(
            "workflow delegates",
            format!("workflow '{wf_name}' has no `states:` map"),
        ));
        return;
    };

    let mut delegates: Vec<(String, String)> = Vec::new(); // (state, delegate string)
    for (state_name, state_val) in states {
        if let Some(d) = state_val.get("delegate").and_then(Value::as_str) {
            delegates.push((state_name.clone(), d.to_string()));
        }
    }

    if delegates.is_empty() {
        results.push(CheckResult::skip(
            "workflow delegates",
            format!("workflow '{wf_name}' has no `delegate:` states"),
        ));
        return;
    }

    let mut downgrades: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (state, delegate_str) in &delegates {
        match Delegate::parse(delegate_str) {
            Err(e) => {
                errors.push(format!("{state}: '{delegate_str}' ({e})"));
            }
            Ok(d) => match resolver.walk(&d) {
                Err(e) => {
                    errors.push(format!("{state}: '{delegate_str}' → exhausted ({e})"));
                }
                Ok((_bindings, level)) => {
                    if level == "default" {
                        downgrades.push(format!("{state}: '{delegate_str}' → default"));
                    }
                }
            },
        }
    }

    if !errors.is_empty() {
        results.push(CheckResult::fail(
            "workflow delegates",
            "WORKFLOW_DELEGATE_UNRESOLVED",
            format!("{} unresolvable: {}", errors.len(), errors.join("; ")),
        ));
    } else if !downgrades.is_empty() {
        // Soft signal: the walk succeeded but matched a less-specific
        // level than the delegate asked for. Op may have intended this;
        // we just surface it so they can verify (FMECA U1 detection).
        results.push(CheckResult::pass(
            "workflow delegates",
            format!(
                "{} delegate(s) resolved; {} fell through to default — verify intent: {}",
                delegates.len(),
                downgrades.len(),
                downgrades.join("; ")
            ),
        ));
    } else {
        results.push(CheckResult::pass(
            "workflow delegates",
            format!("{} delegate(s) resolved to explicit overrides", delegates.len()),
        ));
    }
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
