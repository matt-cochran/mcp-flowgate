//! REST executor.
//!
//! Resolves a `connection: <name>` to a `restConnection` and dispatches an
//! HTTP request shaped by the executor config:
//!
//! ```yaml
//! executor:
//!   kind: rest
//!   connection: github_api
//!   method: POST
//!   path: "/repos/{owner}/{repo}/pulls"      # {var} interpolation
//!   query: { state: open }                    # may use $.arguments / $.context
//!   headers: { X-Foo: bar }                   # per-call overrides
//!   body:                                     # passed as JSON
//!     title: "$.arguments.title"
//!     head: "$.arguments.head"
//!     base: main
//! ```
//!
//! HTTP status maps to `ExecutorError` so reliability policies retry the
//! right things:
//!
//! - 408 / 504 / network timeout → `Timeout`
//! - 429                          → `RateLimited`
//! - 5xx                          → `Transient`
//! - other 4xx                    → `Permanent`
//! - connection refused / DNS     → `Connection`

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::mapping::read_in_scopes;
use mcp_flowgate_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::Executor;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use serde_json::{json, Value};
use uuid::Uuid;

/// `restConnection` entries from the gateway config, keyed by name.
#[derive(Default, Clone)]
pub struct RestConnections {
    inner: HashMap<String, RestConnection>,
}

#[derive(Debug, Clone)]
pub struct RestConnection {
    pub base_url: String,
    pub headers: HashMap<String, String>,
}

impl RestConnections {
    pub fn from_config(config: &Value) -> Self {
        let mut inner = HashMap::new();
        if let Some(map) = config.pointer("/connections").and_then(Value::as_object) {
            for (name, conn) in map {
                if conn.get("kind").and_then(Value::as_str) != Some("rest") {
                    continue;
                }
                let base_url = conn
                    .get("baseUrl")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim_end_matches('/')
                    .to_string();
                let headers = conn
                    .get("headers")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                inner.insert(name.clone(), RestConnection { base_url, headers });
            }
        }
        Self { inner }
    }

    pub fn get(&self, name: &str) -> Option<&RestConnection> {
        self.inner.get(name)
    }
}

pub struct RestExecutor {
    connections: Arc<RestConnections>,
    client: reqwest::Client,
}

impl RestExecutor {
    pub fn new(connections: Arc<RestConnections>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client");
        Self {
            connections,
            client,
        }
    }
}

#[async_trait]
impl Executor for RestExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;

        let connection_name = cfg
            .get("connection")
            .and_then(Value::as_str)
            .ok_or_else(|| ExecutorError::Permanent("rest executor needs `connection`".into()))?;

        let connection = self.connections.get(connection_name).ok_or_else(|| {
            ExecutorError::Permanent(format!("rest connection '{connection_name}' not found"))
        })?;

        let method_str = cfg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();
        let method = Method::from_bytes(method_str.as_bytes()).map_err(|_| {
            ExecutorError::Permanent(format!("rest executor: invalid method '{method_str}'"))
        })?;

        let path = cfg
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("/")
            .to_string();
        let rendered_path = render_path(&path, &request);

        let url = format!(
            "{}{}",
            connection.base_url,
            if rendered_path.starts_with('/') {
                rendered_path
            } else {
                format!("/{rendered_path}")
            }
        );

        let mut req = self.client.request(method, &url);

        // Connection-level headers, then per-call overrides.
        let mut headers = HeaderMap::new();
        for (k, v) in &connection.headers {
            apply_header(&mut headers, k, v)?;
        }
        if let Some(extra) = cfg.get("headers").and_then(Value::as_object) {
            for (k, v) in extra {
                if let Some(s) = v.as_str() {
                    apply_header(&mut headers, k, s)?;
                }
            }
        }
        // Idempotency key (computed by the runtime) becomes the standard
        // `Idempotency-Key` HTTP header so downstream services that respect
        // the convention can dedupe on retries.
        if let Some(key) = &request.idempotency_key {
            apply_header(&mut headers, "Idempotency-Key", key)?;
        }
        if !headers.is_empty() {
            req = req.headers(headers);
        }

        // Query string from `query: {key: value}`. Values may be JSON-path
        // expressions resolved against the usual scopes.
        if let Some(q) = cfg.get("query").and_then(Value::as_object) {
            let pairs: Vec<(String, String)> = q
                .iter()
                .filter_map(|(k, v)| render_value(v, &request).map(|rv| (k.clone(), rv)))
                .collect();
            if !pairs.is_empty() {
                req = req.query(&pairs);
            }
        }

        // Body — full template tree resolved against the scopes.
        if let Some(body_template) = cfg.get("body") {
            let body = render_template(body_template, &request);
            req = req.json(&body);
        }

        let response = req.send().await.map_err(classify_send)?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(classify_send)?;

        let body_value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };

        let summary = format!("{} {} → {}", method_str, url, status);

        if !status.is_success() {
            let class = classify_status(status.as_u16());
            let message = format!("{summary}: {body_value}");
            return Err(match class {
                StatusClass::Timeout => ExecutorError::Timeout(0),
                StatusClass::RateLimited => ExecutorError::RateLimited(message),
                StatusClass::Transient => ExecutorError::Transient(message),
                StatusClass::Permanent => ExecutorError::Permanent(message),
            });
        }

        Ok(ExecuteResult {
            output: json!({
                "status": status.as_u16(),
                "body": body_value,
            }),
            evidence: vec![Evidence {
                kind: "rest_call".into(),
                id: Uuid::new_v4().to_string(),
                uri: Some(url),
                summary: Some(summary),
                digest: None,
                confidence: None,
            }],
            child_workflow_id: None,
        })
    }
}

enum StatusClass {
    Timeout,
    RateLimited,
    Transient,
    Permanent,
}

fn classify_status(code: u16) -> StatusClass {
    match code {
        408 | 504 => StatusClass::Timeout,
        429 => StatusClass::RateLimited,
        500..=599 => StatusClass::Transient,
        _ => StatusClass::Permanent,
    }
}

fn classify_send(err: reqwest::Error) -> ExecutorError {
    if err.is_timeout() {
        return ExecutorError::Timeout(0);
    }
    if err.is_connect() || err.is_request() {
        return ExecutorError::Connection(err.to_string());
    }
    ExecutorError::Transient(err.to_string())
}

fn apply_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), ExecutorError> {
    let n = HeaderName::from_bytes(name.as_bytes())
        .map_err(|e| ExecutorError::Permanent(format!("invalid header name '{name}': {e}")))?;
    let v = HeaderValue::from_str(value)
        .map_err(|e| ExecutorError::Permanent(format!("invalid header value: {e}")))?;
    headers.insert(n, v);
    Ok(())
}

/// Replace `{var}` with the matching value from arguments first, then context,
/// then workflow input. Unresolved vars are left as-is so users can spot them.
fn render_path(template: &str, request: &ExecuteRequest) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            let mut name = String::new();
            while let Some(&n) = chars.peek() {
                if n == '}' {
                    chars.next();
                    break;
                }
                name.push(n);
                chars.next();
            }
            let resolved = lookup(&name, request).unwrap_or_else(|| format!("{{{name}}}"));
            out.push_str(&resolved);
        } else {
            out.push(c);
        }
    }
    out
}

fn lookup(name: &str, request: &ExecuteRequest) -> Option<String> {
    request
        .arguments
        .get(name)
        .or_else(|| request.workflow.context.get(name))
        .or_else(|| request.workflow.input.get(name))
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
}

fn render_value(v: &Value, request: &ExecuteRequest) -> Option<String> {
    match v {
        Value::String(s) => {
            if let Some(expr) = read_in_scopes(
                s,
                &request.arguments,
                &request.workflow.context,
                &request.workflow.input,
                None,
            ) {
                return match expr {
                    Value::String(s) => Some(s),
                    other => Some(other.to_string()),
                };
            }
            Some(s.clone())
        }
        other => Some(other.to_string()),
    }
}

/// Recursively resolve a JSON template: any string value matching a known
/// scope expression is replaced with the resolved value (preserving its
/// type), other strings are passed through.
fn render_template(template: &Value, request: &ExecuteRequest) -> Value {
    match template {
        Value::String(s) => {
            if let Some(resolved) = read_in_scopes(
                s,
                &request.arguments,
                &request.workflow.context,
                &request.workflow.input,
                None,
            ) {
                resolved
            } else {
                Value::String(s.clone())
            }
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| render_template(v, request)).collect())
        }
        Value::Object(obj) => {
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(k.clone(), render_template(v, request));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}
