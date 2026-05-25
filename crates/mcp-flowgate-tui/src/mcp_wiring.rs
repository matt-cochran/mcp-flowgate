//! FrontRails MCP server wiring.
//!
//! Injects the `frontrails-mcp` governance gateway as an additional MCP server
//! into the agent runtime. The gateway provides:
//! - `intentos` tool — spec-first governance (IntentOS)
//! - `structureos` tool — structural analysis (StructureOS)
//! - HATEOAS hints (`_required`, `_available`, `_action`) steering the agent
//!   through governed workflows

use aether_cli::mcp_config_args::McpConfigArgs;

/// Inject the FrontRails MCP gateway server config into MCP config args.
///
/// This adds `frontrails-mcp` as an additional MCP server alongside aether's
/// built-in servers. The gateway proxies to downstream intentos and structureos
/// MCP servers and applies governance (classify, gate, audit, consolidate HATEOAS).
pub fn inject_frontrails_mcp(mcp_config: &mut McpConfigArgs) {
    let config_json = frontrails_mcp_config_json();
    mcp_config.mcp_config_jsons.push(config_json);
}

/// Generate the MCP server config JSON for `frontrails-mcp`.
///
/// Uses the standard MCP config format that aether's `McpConfigArgs` expects.
/// The `frontrails-mcp` binary must be on PATH (installed via `fr install`).
fn frontrails_mcp_config_json() -> String {
    serde_json::json!({
        "mcpServers": {
            "frontrails": {
                "command": find_frontrails_mcp_binary(),
                "env": {
                    "INTENTOS_SPEC_ROOT": ".",
                    "INTENTOS_WORKSPACE_ROOT": ".",
                    "STRUCTUREOS_WORKSPACE_ROOT": "."
                }
            }
        }
    })
    .to_string()
}

/// Find the `frontrails-mcp` binary.
///
/// Checks these locations in order:
/// 1. Next to the current executable (same directory)
/// 2. Falls back to bare name (system PATH resolution)
fn find_frontrails_mcp_binary() -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            let sibling = dir.join("frontrails-mcp");
            if sibling.exists() {
                return sibling.to_string_lossy().to_string();
            }
        }
    "frontrails-mcp".to_string()
}
