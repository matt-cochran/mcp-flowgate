//! Library surface for the Flowgate TUI crate. Modules that have public
//! contracts (`interpreter`, `agent_config`, `tui_config`) live here so
//! integration tests in `tests/` can `use mcp_flowgate_tui::…`.
//!
//! Runtime-only modules (`flowgate_mcp`, `theme`) stay in `main.rs` —
//! they have no test surface and the bin doesn't need to share them.

pub mod agent_config;
pub mod interpreter;
pub mod sub_agent;
pub mod tui_config;
