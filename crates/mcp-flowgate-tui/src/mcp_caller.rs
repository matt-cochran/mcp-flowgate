//! Production `McpToolCaller` impl — wraps an rmcp client connected to
//! a `mcp-flowgate` child process via stdio.
//!
//! The interpreter (`walk_workflow`) is the consumer; it only ever
//! issues `workflow.get` and `workflow.submit` against this caller.
//! `workflow.start` happens once at the binary entry point to acquire
//! a `workflowId`, and is exposed as a free function rather than
//! through the trait so the interpreter contract stays minimal.
//!
//! ## Lifecycle
//!
//! - Construct via [`FlowgateChildCaller::spawn`]. That spawns the
//!   `mcp-flowgate` binary (located via `flowgate_mcp::find_flowgate_binary`)
//!   over `TokioChildProcess` stdio, runs the MCP init handshake, and
//!   returns a caller backed by a long-lived `RunningService`.
//! - The caller owns the service. Drop on the caller cleanly shuts the
//!   child down.
//! - This caller is intentionally NOT cached — `flowgate walk` runs
//!   one workflow per invocation, so a one-shot child is the simplest
//!   correct model. (The executors-crate `McpExecutor` caches across
//!   tool calls because each call is one of many in a long-running
//!   gateway; here, there is exactly one consumer for one walk.)

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::Value;

use crate::flowgate_mcp;
use crate::interpreter::McpToolCaller;

/// Production caller. Holds the `RunningService` for the child
/// process's lifetime; drop = clean shutdown.
pub struct FlowgateChildCaller {
    service: RunningService<RoleClient, ()>,
}

impl FlowgateChildCaller {
    /// Spawn `mcp-flowgate` as a stdio child and run the MCP init
    /// handshake. `config_path` becomes `FLOWGATE_CONFIG` env var on
    /// the child; `extra_env` is merged on top so operators can set
    /// e.g. log levels.
    pub async fn spawn(
        config_path: Option<&str>,
        extra_env: HashMap<String, String>,
    ) -> Result<Self> {
        let binary = flowgate_mcp::find_flowgate_binary()
            .context("locating mcp-flowgate binary")?;
        let mut cmd = tokio::process::Command::new(&binary);
        if let Some(p) = config_path {
            cmd.env("FLOWGATE_CONFIG", p);
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let transport = TokioChildProcess::new(cmd)
            .with_context(|| format!("spawning mcp-flowgate binary '{binary}'"))?;
        let service = ServiceExt::serve((), transport)
            .await
            .context("rmcp client init against mcp-flowgate child process")?;
        Ok(Self { service })
    }
}

#[async_trait]
impl McpToolCaller for FlowgateChildCaller {
    async fn call(&self, tool: &str, args: Value) -> Result<Value> {
        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(obj) = args.as_object() {
            params = params.with_arguments(obj.clone());
        } else if !args.is_null() {
            return Err(anyhow!(
                "McpToolCaller args must be a JSON object or null; got: {}",
                args
            ));
        }
        let result = self
            .service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| anyhow!("mcp-flowgate tool '{tool}' call failed: {e}"))?;

        if result.is_error.unwrap_or(false) {
            let body = result
                .structured_content
                .or_else(|| {
                    (!result.content.is_empty())
                        .then(|| serde_json::json!({ "content": result.content }))
                })
                .unwrap_or(Value::Null);
            return Err(anyhow!(
                "mcp-flowgate tool '{tool}' returned is_error=true: {}",
                body
            ));
        }

        Ok(result
            .structured_content
            .or_else(|| {
                (!result.content.is_empty())
                    .then(|| serde_json::json!({ "content": result.content }))
            })
            .unwrap_or(Value::Null))
    }
}
