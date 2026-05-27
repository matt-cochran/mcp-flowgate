//! `flowgate-agent` — governed agent runtime.
//!
//! Wraps the Aether agent framework with one architectural rule:
//! **mcp-flowgate is the sole MCP server.**
//!
//! Aether's built-in tool surface (filesystem, shell, etc.) is replaced
//! entirely. The model's only available tools are the 7 stable Flowgate
//! tools: `gateway.home`, `gateway.search`, `gateway.describe`,
//! `workflow.start`, `workflow.get`, `workflow.submit`, `workflow.explain`.
//!
//! Flowgate governs every action through typed workflows, guards, and
//! executors. The TUI/human sees the same HATEOAS link surface the model
//! does — governance is transparent.
//!
//! All Aether modes are supported: TUI (default), headless, ACP (editor),
//! and agent management.

mod theme;

// Library surface (interpreter, agent_config, tui_config, sub_agent,
// flowgate_mcp) lives in src/lib.rs so integration tests + the
// sub-agent spawner can reach them.
use mcp_flowgate_tui::agent_resolver::{
    verify_all_primary_bindings, AgentsFile, ConfigSource, Resolver,
};
use mcp_flowgate_tui::interpreter::{
    AgentRegistry, LegacyAgentRegistry, McpToolCaller, YamlAgentRegistry,
};
use mcp_flowgate_tui::{agent_config, flowgate_mcp, keyring, mcp_init, tui_config};

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use wisp::runtime_state::RuntimeState;

#[derive(Parser)]
#[command(
    name = "flowgate-agent",
    version,
    about = "Flowgate governed agent runtime",
    long_about = "AI coding agent with workflow governance.\n\
\n\
Aagent framework with mcp-flowgate as its\n\
sole MCP server. Every model action goes through governed\n\
workflows — no ungoverned tool access.\n\
\n\
Modes:\n\
  (default)   Interactive TUI\n\
  headless    Run a single prompt non-interactively\n\
  acp         Start ACP server for editor integration\n\
  agent       Manage agent configurations\n\
  walk        Drive a workflow via the deterministic interpreter\n\
  doctor      Pre-flight checks before walk\n\
  mcp init    Generate .mcp.json (and optional editor configs)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run a single prompt non-interactively
    Headless(aether_cli::headless::HeadlessArgs),
    /// Start the ACP server (for editor integration)
    Acp(aether_cli::acp::AcpArgs),
    /// Manage agent configurations
    #[command(subcommand)]
    Agent(aether_cli::agent::AgentCommand),
    /// Walk a Flowgate workflow to completion using the deterministic
    /// interpreter (SPEC §21). Spawns isolated sub-agents per delegate
    /// state; auto-advances states with no delegate.
    Walk(WalkArgs),
    /// Pre-flight checks for `flowgate walk` — binary discovery, config
    /// resolution, workflow declared, agent API keys, script file URIs.
    /// Exits 0 if all pass; 1 if any fail. Run before `walk` to catch
    /// env / config issues before the workflow starts.
    Doctor(DoctorCliArgs),
    /// MCP client config generators.
    #[command(subcommand)]
    Mcp(McpCommand),
}

/// `flowgate mcp <subcommand>` — operator-facing MCP wiring helpers.
/// Today: `init` generates `.mcp.json` (and optionally `.cursor/mcp.json`,
/// `claude_desktop_config.json`) so editors connecting via ACP — or any
/// MCP host like Cursor / Claude Desktop / Claude Code — see flowgate as
/// the sole MCP server.
#[derive(Subcommand)]
enum McpCommand {
    /// Generate MCP client config files for the project (`.mcp.json` plus
    /// optional editor-specific outputs via `--cursor` / `--claude-desktop`).
    Init(mcp_init::McpInitArgs),
}

#[derive(clap::Args, Debug)]
pub struct DoctorCliArgs {
    /// Path to the gateway YAML config (defaults to $FLOWGATE_CONFIG).
    #[arg(long)]
    pub config: Option<String>,
    /// Workflow id that walk will run — checked against declared workflows.
    #[arg(long)]
    pub workflow: Option<String>,
    /// Agent specs (same as walk's --agent). Each agent's provider's
    /// API key env var presence is verified.
    #[arg(long = "agent")]
    pub agents: Vec<String>,
}

/// CLI args for `flowgate walk` — drives a workflow end-to-end through
/// the deterministic interpreter.
#[derive(clap::Args, Debug)]
pub struct WalkArgs {
    /// Workflow id to start (e.g. `swe_agent`). Must match a workflow
    /// declared in the Flowgate config.
    #[arg(long)]
    pub workflow: String,

    /// JSON object passed as `input` to `workflow.start`.
    #[arg(long, default_value = "{}")]
    pub input: String,

    /// Agent config in `name=provider/model` form. Repeat for each
    /// sub-agent referenced by `delegate:` fields in the workflow.
    /// Example: `--agent planning=anthropic/claude-sonnet-4 --agent editing=anthropic/claude-haiku-4-5-20251001`
    ///
    /// **Deprecated in v0.3 in favor of `agents.yaml`** — prefer the
    /// file-based config for per-affinity overrides + feature toggles.
    /// Mutually exclusive with `--agents-config` and on-disk
    /// `.flowgate/agents.yaml` / `~/.config/flowgate/agents.yaml`.
    #[arg(long = "agent")]
    pub agents: Vec<String>,

    /// Path to an `agents.yaml` file (v0.3+). When unset, the resolver
    /// looks for `.flowgate/agents.yaml` (project) then
    /// `~/.config/flowgate/agents.yaml` (user). Setting this AND
    /// `--agent` flags is a startup error (FMECA T1 mitigation).
    #[arg(long, env = "FLOWGATE_AGENTS_CONFIG")]
    pub agents_config: Option<PathBuf>,

    /// Hard ceiling on wall-clock seconds per sub-agent. No default by
    /// design — operators must declare their tolerance for orphan
    /// sub-agents.
    #[arg(long)]
    pub max_sub_agent_seconds: Option<u64>,

    /// **Advisory** tool-call hint per sub-agent (no default; must be
    /// set explicitly so operators declare a number they consider
    /// reasonable). Currently logged + surfaced for observability;
    /// not enforced — aether's headless API has no per-tool-call
    /// hook, so the enforced cap is `--max-sub-agent-seconds`. The
    /// hint will be enforced once aether exposes a step callback;
    /// the CLI contract stays valid either way.
    #[arg(long)]
    pub max_sub_agent_steps: Option<usize>,

    /// Warning threshold for blackboard size (serialized JSON bytes).
    /// Defaults to 16 KiB. Exceeding this logs a warning but does not
    /// block the spawn.
    #[arg(long)]
    pub max_blackboard_bytes: Option<usize>,

    /// Path to the flowgate.yaml config used by the spawned
    /// `mcp-flowgate` child process. Becomes `FLOWGATE_CONFIG` on the
    /// child env. When unset, mcp-flowgate falls back to its own
    /// resolution (cwd `flowgate.yaml`).
    #[arg(long)]
    pub config: Option<String>,
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();

    // Pre-flight: ensure the platform keyring service is running. The
    // upstream ACP runtime eagerly initializes D-Bus Secret Service for
    // OAuth credential storage; on Linux/WSL2 the daemon may not be
    // running. See crates/mcp-flowgate-tui/src/keyring.rs. No-op on
    // macOS and Windows.
    keyring::ensure_keyring_available();

    match cli.command {
        None => run_tui().await,
        Some(Command::Headless(args)) => run_headless(args).await,
        Some(Command::Acp(args)) => run_acp(args).await,
        Some(Command::Agent(cmd)) => run_agent(cmd).await,
        Some(Command::Walk(args)) => run_walk(args).await,
        Some(Command::Doctor(args)) => run_doctor(args).await,
        Some(Command::Mcp(McpCommand::Init(args))) => {
            mcp_init::run_init(&args).map(|_| ExitCode::SUCCESS)
        }
    }
}

/// Walk a workflow to completion via the deterministic interpreter
/// (SPEC §21). Validates args, builds the agent registry, spawns
/// mcp-flowgate as an rmcp child process, starts the workflow, then
/// drives it through `walk_workflow` against the real
/// `AetherSubAgentSpawner`.
async fn run_walk(args: WalkArgs) -> Result<ExitCode> {
    let tui_cfg = tui_config::TuiConfig::from_cli(
        args.max_sub_agent_seconds,
        args.max_sub_agent_steps,
        args.max_blackboard_bytes,
    )?;

    let registry: Box<dyn AgentRegistry> = build_agent_registry(&args).await?;

    let input_value: serde_json::Value = serde_json::from_str(&args.input)
        .map_err(|e| anyhow::anyhow!("--input is not valid JSON: {e}"))?;

    let spawner = mcp_flowgate_tui::sub_agent::AetherSubAgentSpawner::new(tui_cfg);

    let caller = mcp_flowgate_tui::mcp_caller::FlowgateChildCaller::spawn(
        args.config.as_deref(),
        std::collections::HashMap::new(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("spawning mcp-flowgate child for walk: {e}"))?;

    // Start the workflow to acquire a workflowId.
    let start_resp = caller
        .call(
            "workflow.start",
            serde_json::json!({ "definition": args.workflow, "input": input_value }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("workflow.start failed: {e}"))?;

    let workflow_id = start_resp
        .pointer("/workflow/id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "workflow.start response missing /workflow/id: {start_resp}"
            )
        })?
        .to_string();

    tracing::info!(workflow = %args.workflow, %workflow_id, "walking workflow");

    let final_ctx = mcp_flowgate_tui::interpreter::walk_workflow(
        &caller,
        &spawner,
        &workflow_id,
        registry.as_ref(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("walk failed: {e}"))?;

    println!(
        "{}",
        serde_json::to_string_pretty(&final_ctx)
            .unwrap_or_else(|_| final_ctx.to_string())
    );

    Ok(ExitCode::SUCCESS)
}

/// Resolve the agent config source and build a registry the interpreter
/// can use. Precedence (highest wins):
///
/// 1. `--agents-config <PATH>` (or `$FLOWGATE_AGENTS_CONFIG`).
/// 2. `.flowgate/agents.yaml` in the current working directory (project).
/// 3. `~/.config/flowgate/agents.yaml` (user — `dirs::config_dir()`).
/// 4. `--agent name=provider/model` CLI flags (deprecated v0.2 path).
///
/// Mutual exclusion (FMECA T1 mitigation): if any agents.yaml file is
/// resolvable AND `--agent` flags are present, return a startup error
/// rather than silently picking one source.
///
/// On YAML path success, runs `verify_all_primary_bindings` for the
/// eager auth preflight (FMECA U2). The preflight honors
/// `FLOWGATE_SKIP_PREFLIGHT=1`.
async fn build_agent_registry(args: &WalkArgs) -> Result<Box<dyn AgentRegistry>> {
    let yaml_path: Option<(PathBuf, ConfigSource)> = if let Some(p) = &args.agents_config {
        Some((p.clone(), ConfigSource::Project(p.clone())))
    } else {
        let project = std::path::Path::new(".flowgate").join("agents.yaml");
        if project.exists() {
            Some((project.clone(), ConfigSource::Project(project)))
        } else if let Some(user_dir) = dirs::config_dir() {
            let user = user_dir.join("flowgate").join("agents.yaml");
            if user.exists() {
                Some((user.clone(), ConfigSource::User(user)))
            } else {
                None
            }
        } else {
            None
        }
    };

    if yaml_path.is_some() && !args.agents.is_empty() {
        anyhow::bail!(
            "ambiguous agent source: both `--agent` CLI flag(s) AND an agents.yaml file are \
             present. Choose one — agents.yaml takes precedence going forward; the `--agent` \
             flag is deprecated. See /guides/agent-config.mdx for the migration path."
        );
    }

    if let Some((path, source)) = yaml_path {
        let file = AgentsFile::from_path(&path)
            .map_err(|e| anyhow::anyhow!("failed to load {}: {e}", path.display()))?;
        let resolver = Resolver::from_loaded(file, source);
        // FMECA U2: eager auth preflight on every distinct primary
        // binding declared in the file. Hard error on 401/403 or
        // missing-credential; warn-and-continue on transient infra.
        if let Err(errors) = verify_all_primary_bindings(&resolver).await {
            let summary: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
            anyhow::bail!(
                "preflight failed for {} primary binding(s):\n  - {}\n\
                 Set the missing credential(s) or pass \
                 `FLOWGATE_SKIP_PREFLIGHT=1` to bypass.",
                errors.len(),
                summary.join("\n  - ")
            );
        }
        return Ok(Box::new(YamlAgentRegistry::new(resolver)));
    }

    // Legacy CLI path (deprecated).
    if !args.agents.is_empty() {
        tracing::warn!(
            "--agent CLI flag is deprecated; prefer .flowgate/agents.yaml (see \
             /guides/agent-config.mdx)"
        );
    }
    let agents = agent_config::build_registry(&args.agents)
        .map_err(|e| anyhow::anyhow!("agent config parse error: {e}"))?;
    Ok(Box::new(LegacyAgentRegistry::new(agents)))
}

/// TUI mode (default) — interactive terminal with Flowgate branding.
///
/// Spawns `flowgate-agent acp` as a subprocess and connects via ACP.
/// The ACP subprocess inherits the sole-MCP wiring so the model
/// always routes through governed workflows.
async fn run_tui() -> Result<ExitCode> {
    let log_dir = resolve_log_dir();
    // Best-effort mkdir — wisp creates the file inside, but the dir must
    // exist. We don't fail if mkdir fails; logging falls back to stderr.
    let _ = std::fs::create_dir_all(&log_dir);
    wisp::setup_logging(Some(&log_dir.to_string_lossy()));

    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("flowgate"));
    let acp_command = format!("{} acp", exe.display());

    let mut state: RuntimeState = RuntimeState::new(&acp_command)
        .await
        .map_err(|e| anyhow::anyhow!("TUI initialization failed: {e}"))?;

    // Branding
    state.agent_name = "Flowgate".into();
    state.theme = theme::flowgate_theme();

    wisp::run_with_state(state)
        .await
        .map_err(|e| anyhow::anyhow!("TUI error: {e}"))?;

    Ok(ExitCode::SUCCESS)
}

/// SPEC §B.4 — resolve the TUI's log directory. Order:
/// 1. `$FLOWGATE_LOG_DIR` (operator override).
/// 2. `dirs::cache_dir().join("flowgate/logs")` — platform standard cache:
///    `~/.cache/flowgate/logs` (Linux), `~/Library/Caches/flowgate/logs`
///    (macOS), `%LOCALAPPDATA%\flowgate\logs` (Windows).
/// 3. `./flowgate-logs` as last-resort fallback (if `dirs::cache_dir`
///    returns `None`, e.g. in some sandboxed CI environments).
///
/// Exposed as a free function so tests can exercise it directly.
pub fn resolve_log_dir() -> PathBuf {
    if let Ok(override_path) = std::env::var("FLOWGATE_LOG_DIR") {
        if !override_path.trim().is_empty() {
            return PathBuf::from(override_path);
        }
    }
    match dirs::cache_dir() {
        Some(cache) => cache.join("flowgate").join("logs"),
        None => PathBuf::from("flowgate-logs"),
    }
}

/// Headless mode — run a single prompt, output result.
///
/// Injects mcp-flowgate as the **sole MCP server**, replacing
/// aether's built-in tool surface entirely.
async fn run_headless(mut args: aether_cli::headless::HeadlessArgs) -> Result<ExitCode> {
    // SPEC §B.3 — fail fast at startup if `MCP_FLOWGATE_PATH` is set to a
    // non-existent file. A bare PATH fallback is still permitted (silent +
    // logged) so end-users don't need the env var in the common install case.
    flowgate_mcp::set_as_sole_mcp(&mut args.mcp_config)?;

    aether_cli::headless::run_headless(args)
        .await
        .map(|_| ExitCode::SUCCESS)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// ACP mode — Agent Client Protocol server for editor integration.
///
/// The TUI spawns this mode as a subprocess. Editors connect via ACP.
/// ACP resolves its MCP config from the agent's settings or `.mcp.json`,
/// not from CLI args, so the sole-MCP wiring happens through the agent
/// configuration rather than programmatic injection.
async fn run_acp(args: aether_cli::acp::AcpArgs) -> Result<ExitCode> {
    aether_cli::acp::run_acp(args)
        .await
        .map(|_| ExitCode::SUCCESS)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Agent management — create, list, remove agent configurations.
async fn run_agent(cmd: aether_cli::agent::AgentCommand) -> Result<ExitCode> {
    match cmd {
        aether_cli::agent::AgentCommand::New(args) => {
            aether_cli::agent::run_new(args)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        aether_cli::agent::AgentCommand::List(args) => {
            aether_cli::agent::run_list(args).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        aether_cli::agent::AgentCommand::Remove(args) => {
            aether_cli::agent::run_remove(args).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `flowgate doctor` — pre-flight checks. Exits 0 if all pass; 1 if any fail.
async fn run_doctor(args: DoctorCliArgs) -> Result<ExitCode> {
    let doctor_args = mcp_flowgate_tui::doctor::DoctorArgs {
        config: args.config,
        workflow: args.workflow,
        agents: args.agents,
    };
    let results = mcp_flowgate_tui::doctor::run_doctor(&doctor_args).await;
    print!("{}", mcp_flowgate_tui::doctor::render_results(&results));
    if mcp_flowgate_tui::doctor::count_failures(&results) > 0 {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}
