//! Library surface for the Flowgate TUI crate. Modules that have public
//! contracts (`interpreter`, `agent_config`, `tui_config`, `sub_agent`,
//! `flowgate_mcp`) live here so integration tests and the sub-agent
//! spawner can reach them. The bin's `main.rs` re-imports via
//! `use mcp_flowgate_tui::…`.
//!
//! Runtime-only modules with no test surface (e.g. `theme`) stay in
//! `main.rs`.

pub mod agent_config;
pub mod agent_resolver;
pub mod doctor;
pub mod flowgate_mcp;
pub mod interpreter;
pub mod keyring;
pub mod mcp_caller;
pub mod mcp_init;
pub mod migrate;
pub mod sub_agent;
pub mod tui_config;
