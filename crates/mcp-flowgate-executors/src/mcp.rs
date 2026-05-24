use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::Executor;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Connections of `kind: mcp` parsed from gateway config, keyed by name.
#[derive(Default, Clone)]
pub struct McpConnections {
    inner: Arc<HashMap<String, McpConnection>>,
}

#[derive(Debug, Clone)]
pub struct McpConnection {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub url: Option<String>,
}

impl McpConnections {
    pub fn from_config(config: &Value) -> Self {
        let mut map = HashMap::new();
        if let Some(conns) = config.pointer("/connections").and_then(Value::as_object) {
            for (name, conn) in conns {
                if conn.get("kind").and_then(Value::as_str) != Some("mcp") {
                    continue;
                }
                let command = conn
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let args = conn
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let env = conn
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                let url = conn.get("url").and_then(Value::as_str).map(str::to_string);
                map.insert(
                    name.clone(),
                    McpConnection {
                        command,
                        args,
                        env,
                        url,
                    },
                );
            }
        }
        Self {
            inner: Arc::new(map),
        }
    }

    pub fn get(&self, name: &str) -> Option<&McpConnection> {
        self.inner.get(name)
    }
}

/// MCP executor: forwards `executor.kind=mcp` calls to a child MCP server
/// resolved by `executor.connection`. Clients are lazily started per
/// connection on first use and reused for the process lifetime.
pub struct McpExecutor {
    connections: McpConnections,
    cache: Mutex<HashMap<String, Arc<RunningService<RoleClient, ()>>>>,
}

impl McpExecutor {
    pub fn new(connections: McpConnections) -> Self {
        Self {
            connections,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Connect (or reuse a cached connection) to a configured MCP server and
    /// list its tools via the standard `tools/list` MCP method. Vendor-neutral
    /// — works for any process the connection knows how to spawn (native
    /// binary, `npx -y …`, `uvx …`, `docker run …`, `podman run …`, etc.).
    pub async fn list_remote_tools(
        &self,
        connection: &str,
    ) -> Result<Vec<rmcp::model::Tool>, ExecutorError> {
        let client = self.client_for(connection).await?;
        client
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| classify(e.to_string()))
    }

    async fn client_for(
        &self,
        name: &str,
    ) -> Result<Arc<RunningService<RoleClient, ()>>, ExecutorError> {
        {
            let g = self.cache.lock().await;
            if let Some(c) = g.get(name) {
                return Ok(c.clone());
            }
        }

        let conn = self.connections.get(name).ok_or_else(|| {
            ExecutorError::Permanent(format!("mcp connection '{name}' not found"))
        })?;

        // Two transports, picked by which connection field is set. URL wins
        // when both are present (since URL implies a hosted server, not a
        // process to launch).
        let arc: Arc<RunningService<RoleClient, ()>> = if let Some(url) = &conn.url {
            let transport = StreamableHttpClientTransport::<reqwest::Client>::from_uri(url.clone());
            let client: RunningService<RoleClient, ()> = ServiceExt::serve((), transport)
                .await
                .map_err(|e| ExecutorError::Connection(format!("mcp http init '{name}': {e}")))?;
            Arc::new(client)
        } else {
            let command = conn.command.as_deref().ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "mcp connection '{name}' has neither `command` nor `url`"
                ))
            })?;

            let mut cmd = tokio::process::Command::new(command);
            for a in &conn.args {
                cmd.arg(a);
            }
            for (k, v) in &conn.env {
                cmd.env(k, v);
            }

            let transport = TokioChildProcess::new(cmd)
                .map_err(|e| ExecutorError::Connection(format!("spawn '{command}': {e}")))?;

            let client: RunningService<RoleClient, ()> = ServiceExt::serve((), transport)
                .await
                .map_err(|e| ExecutorError::Connection(format!("mcp init '{name}': {e}")))?;
            Arc::new(client)
        };

        let mut g = self.cache.lock().await;
        let entry = g.entry(name.to_string()).or_insert_with(|| arc.clone());
        Ok(entry.clone())
    }
}

#[async_trait]
impl Executor for McpExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;
        let connection = cfg
            .get("connection")
            .and_then(Value::as_str)
            .ok_or_else(|| ExecutorError::Permanent("mcp executor needs `connection`".into()))?;
        let tool = cfg
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| ExecutorError::Permanent("mcp executor needs `tool`".into()))?;

        let mapped_args =
            render_args(cfg.get("map"), &request).unwrap_or(request.arguments.clone());
        let mut arguments = mapped_args.as_object().cloned();

        // If the runtime computed an idempotency key, surface it as a
        // `_idempotencyKey` field in the tool arguments. Downstream MCP
        // tools that honor the convention can dedupe; tools that don't
        // simply ignore the extra field.
        if let Some(key) = &request.idempotency_key {
            let mut a = arguments.unwrap_or_default();
            a.insert("_idempotencyKey".into(), Value::String(key.clone()));
            arguments = Some(a);
        }

        let client = self.client_for(connection).await?;

        let mut params = CallToolRequestParams::new(tool.to_string());
        if let Some(args) = arguments {
            params = params.with_arguments(args);
        }

        let result = client
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| classify(e.to_string()))?;

        let output = if let Some(structured) = result.structured_content {
            structured
        } else if !result.content.is_empty() {
            json!({ "content": result.content })
        } else {
            json!({})
        };

        if result.is_error.unwrap_or(false) {
            return Err(ExecutorError::Permanent(format!(
                "mcp tool '{}' returned error: {}",
                tool,
                serde_json::to_string(&output).unwrap_or_default()
            )));
        }

        Ok(ExecuteResult {
            output,
            evidence: vec![Evidence {
                kind: "mcp_tool_result".to_string(),
                id: Uuid::new_v4().to_string(),
                uri: None,
                summary: Some(format!("Called {connection}.{tool}")),
            }],
            child_workflow_id: None,
        })
    }
}

/// Apply an executor `map: { fooArg: $.context.bar }` block against the
/// available scopes. Falls back to passing `arguments` through unchanged.
fn render_args(map: Option<&Value>, request: &ExecuteRequest) -> Option<Value> {
    let map = map?.as_object()?;
    let mut out = serde_json::Map::new();
    for (target, source) in map {
        let s = source.as_str()?;
        if let Some(v) = mcp_flowgate_core::mapping::read_in_scopes(
            s,
            &request.arguments,
            &request.workflow.context,
            &request.workflow.input,
            None,
        ) {
            out.insert(target.clone(), v);
        }
    }
    Some(Value::Object(out))
}

fn classify(message: String) -> ExecutorError {
    let lc = message.to_lowercase();
    if lc.contains("timeout") || lc.contains("timed out") {
        ExecutorError::Timeout(0)
    } else if lc.contains("rate limit") {
        ExecutorError::RateLimited(message)
    } else if lc.contains("connection") || lc.contains("closed") || lc.contains("broken pipe") {
        ExecutorError::Connection(message)
    } else {
        ExecutorError::Transient(message)
    }
}
