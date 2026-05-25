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

mod flowgate_mcp;
mod theme;

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
  agent       Manage agent configurations"
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
}

#[tokio::main]
async fn main() -> Result<ExitCode> {
    let cli = Cli::parse();

    match cli.command {
        None => run_tui().await,
        Some(Command::Headless(args)) => run_headless(args).await,
        Some(Command::Acp(args)) => run_acp(args).await,
        Some(Command::Agent(cmd)) => run_agent(cmd).await,
    }
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
