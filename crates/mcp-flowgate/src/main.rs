use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use mcp_flowgate_core::audit::{
    AuditSink, FileAuditSink, MemoryAuditSink, NullAuditSink, RotationInterval, StdoutAuditSink,
};
use mcp_flowgate_core::capability::CapabilityRegistry;
use mcp_flowgate_core::discovery::{DiscoveryIndex, InMemoryDiscoveryIndex};
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::ports::{EvidenceStore, WorkflowStore};
use mcp_flowgate_core::store::{
    ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore,
};
use mcp_flowgate_core::store_file::FileWorkflowStore;
use mcp_flowgate_core::store_postgres::PostgresWorkflowStore;
use mcp_flowgate_core::store_sqlite::SqliteWorkflowStore;
use mcp_flowgate_core::WorkflowRuntime;
use mcp_flowgate_executors::{
    default_registry_with_mcp, import_capabilities, CliConnections, McpConnections, McpExecutor,
};
use mcp_flowgate_mcp_server::FlowgateServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "mcp-flowgate",
    version,
    about = "Configurable MCP gateway with HATEOAS workflow governance"
)]
struct Cli {
    /// Log format: "text" (default) or "json".
    #[arg(long, default_value = "text", global = true)]
    log_format: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the MCP server over stdio.
    Serve {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Validate a config and print the resolved workflow definition ids.
    Check {
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Run pending schema migrations (currently a no-op).
    Migrate {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
    },
    /// Inspect a running workflow.
    Inspect {
        #[command(subcommand)]
        command: InspectCommand,
    },
    /// Inspect and tail audit events.
    Audit {
        #[command(subcommand)]
        command: AuditCommand,
    },
    /// Manage approval queues.
    #[command(name = "approvals")]
    Approvals {
        #[command(subcommand)]
        command: ApprovalsCommand,
    },
}

#[derive(Subcommand, Debug)]
enum InspectCommand {
    /// Show detailed information about a workflow instance.
    Workflow {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
        /// The workflow instance ID.
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum ApprovalsCommand {
    /// List pending approvals.
    List {
        /// Path to the gateway YAML config (to find the audit file).
        #[arg(short, long)]
        config: PathBuf,
        /// Show all approvals, including resolved ones.
        #[arg(long)]
        all: bool,
    },
    /// Resolve a pending approval by its audit event id.
    Resolve {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
        /// The audit event id of the approval to resolve.
        id: String,
        /// Resolution outcome (approved | rejected).
        #[arg(short, long, default_value = "approved")]
        outcome: String,
    },
    /// Tail the audit log for new approval requests.
    Tail {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum AuditCommand {
    /// Tail the audit log for new events.
    Tail {
        /// Path to the gateway YAML config.
        #[arg(short, long)]
        config: PathBuf,
        /// Only show events matching this type (e.g. "human.approval.requested").
        #[arg(short, long)]
        filter: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli.log_format);

    match cli.command {
        Command::Serve { config } => serve(config).await,
        Command::Check { config } => check(config),
        Command::Migrate { config } => migrate(config),
        Command::Inspect { command } => match command {
            InspectCommand::Workflow { config, id } => inspect_workflow(&config, &id),
        },
        Command::Audit { command } => match command {
            AuditCommand::Tail { config, filter } => audit_tail(&config, &filter),
        },
        Command::Approvals { command } => match command {
            ApprovalsCommand::List { config, all } => approvals_list(&config, all),
            ApprovalsCommand::Resolve {
                config,
                id,
                outcome,
            } => approvals_resolve(&config, &id, &outcome),
            ApprovalsCommand::Tail { config } => approvals_tail(&config),
        },
    }
}

fn init_tracing(log_format: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    match log_format {
        "json" => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .json()
                .try_init();
        }
        _ => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .try_init();
        }
    }
}

fn load_config(path: &PathBuf) -> anyhow::Result<Value> {
    // Walks `include:` blocks, loads any declared `repos:` (namespace-prefixing
    // every definitionId), enforces the V20/V21/V22/V23 multi-repo invariants
    // (SPEC §9), then resolves `capabilities:` / `wraps` /
    // `executor: { capability: ... }` references into the inline shapes the
    // runtime expects. Soft diagnostics are discarded here; `check` uses the
    // diagnostics-returning variant.
    mcp_flowgate_core::config::load_resolved_with_repos(path)
        .map(|(config, _diagnostics)| config)
        .with_context(|| format!("loading config {}", path.display()))
}

async fn serve(config_path: PathBuf) -> anyhow::Result<()> {
    use mcp_flowgate_core::hot_reload::{
        SwappableDefinitionStore, SwappableDiscoveryIndex, SwappableExecutorRegistry,
    };

    let config = load_config(&config_path)?;
    let audit = build_audit_sink(&config)?;

    let (initial_defs, initial_executors, initial_discovery) =
        build_hot_components(&config, &audit).await;

    let swappable_defs = Arc::new(SwappableDefinitionStore::new(initial_defs));
    let swappable_executors = Arc::new(SwappableExecutorRegistry::new(initial_executors));
    let swappable_discovery = Arc::new(SwappableDiscoveryIndex::new(initial_discovery));

    let store = build_workflow_store(&config)?;
    let evidence: Arc<dyn EvidenceStore> = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let runtime = WorkflowRuntime::new(
        swappable_defs.clone() as Arc<dyn mcp_flowgate_core::ports::DefinitionStore>,
        store,
        swappable_executors.clone() as Arc<dyn mcp_flowgate_core::ports::ExecutorRegistry>,
        guards,
        audit.clone(),
    )
    .with_evidence(evidence);

    tracing::info!(
        path = %config_path.display(),
        "starting mcp-flowgate stdio server"
    );

    // SPEC §30 — pull the top-level `lexicon:` block out of the
    // resolved config and pass it as the lexicon base. Empty when no
    // block declared. Runtime writes via `gateway.lexicon.define`
    // land in the in-memory overlay; operators persist by editing
    // flowgate.yaml.
    let lexicon_base = config
        .get("lexicon")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let server = FlowgateServer::new(runtime.clone())
        .with_discovery(swappable_discovery.clone() as Arc<dyn DiscoveryIndex>)
        .with_lexicon(lexicon_base);
    let service = server
        .serve(stdio())
        .await
        .context("starting MCP service over stdio")?;

    // SIGHUP: hot-reload config without dropping connections or in-flight work.
    #[cfg(unix)]
    {
        let reload_defs = swappable_defs.clone();
        let reload_executors = swappable_executors.clone();
        let reload_discovery = swappable_discovery.clone();
        let reload_config_path = config_path.clone();
        let reload_audit = audit.clone();
        tokio::spawn(async move {
            let mut sighup =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to register SIGHUP handler");
                        return;
                    }
                };
            loop {
                sighup.recv().await;
                tracing::info!("received SIGHUP — reloading config");
                match load_config(&reload_config_path) {
                    Ok(new_config) => {
                        let (new_defs, new_executors, new_discovery) =
                            build_hot_components(&new_config, &reload_audit).await;
                        reload_defs.swap(new_defs);
                        reload_executors.swap(new_executors);
                        reload_discovery.swap(new_discovery);
                        let _ = reload_audit
                            .record(
                                mcp_flowgate_core::audit::AuditEvent::new("config.reloaded")
                                    .with_payload(json!({
                                        "config": reload_config_path.display().to_string(),
                                    })),
                            )
                            .await;
                        tracing::info!("config reloaded successfully");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "config reload failed — keeping current config");
                    }
                }
            }
        });
    }

    let drain_deadline_secs: u64 = std::env::var("FLOWGATE_DRAIN_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let cancel = service.cancellation_token();
    let drain_runtime = runtime.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        tracing::info!(
            deadline_secs = drain_deadline_secs,
            "received shutdown signal — draining"
        );
        drain_runtime.begin_drain();
        tokio::time::sleep(std::time::Duration::from_secs(drain_deadline_secs)).await;
        tracing::info!("drain deadline reached — closing service");
        cancel.cancel();
    });

    service.waiting().await?;
    signal_task.abort();
    Ok(())
}

async fn build_hot_components(
    config: &Value,
    audit: &Arc<dyn mcp_flowgate_core::audit::AuditSink>,
) -> (
    Arc<dyn mcp_flowgate_core::ports::DefinitionStore>,
    Arc<dyn mcp_flowgate_core::ports::ExecutorRegistry>,
    Arc<dyn DiscoveryIndex>,
) {
    let mcp_conns = McpConnections::from_config(config);
    let mcp_executor = Arc::new(McpExecutor::new(mcp_conns));
    let imported = import_capabilities(config, &mcp_executor, audit).await;
    let effective_config = with_imports(config.clone(), &imported);
    let cli_conns = Arc::new(CliConnections::from_config(&effective_config));
    let executors =
        default_registry_with_mcp(&effective_config, mcp_executor, cli_conns, audit.clone());
    let definitions: Arc<dyn mcp_flowgate_core::ports::DefinitionStore> =
        Arc::new(ConfigDefinitionStore::from_config(&effective_config));
    let discovery: Arc<dyn DiscoveryIndex> =
        Arc::new(InMemoryDiscoveryIndex::from_config(&effective_config));
    (definitions, executors, discovery)
}

fn migrate(config_path: PathBuf) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(&config_path)?;
    let count = raw.matches("kind: jsonpath").count()
        + raw.matches("kind: 'jsonpath'").count()
        + raw.matches("kind: \"jsonpath\"").count();
    if count == 0 {
        println!(
            "migrate: no migrations to run (config: {})",
            config_path.display()
        );
        return Ok(());
    }
    let updated = raw
        .replace("kind: jsonpath", "kind: expr")
        .replace("kind: 'jsonpath'", "kind: 'expr'")
        .replace("kind: \"jsonpath\"", "kind: \"expr\"");
    std::fs::write(&config_path, updated)?;
    println!(
        "migrate: rewrote {} guard(s) from kind: jsonpath → kind: expr (config: {})",
        count,
        config_path.display()
    );
    Ok(())
}

fn check(config_path: PathBuf) -> anyhow::Result<()> {
    // SPEC §5.4.2 / audit-resolution C.2 — `check` is the surface where
    // soft diagnostics (e.g. non-strict-mode unblessed subject roots)
    // become visible. Use the diagnostics-returning variant.
    let (config, soft_diagnostics) =
        mcp_flowgate_core::config::load_resolved_with_repos(&config_path)
            .with_context(|| format!("loading config {}", config_path.display()))?;

    let version = config
        .pointer("/version")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "config {}: 'version' field is required (e.g. version: \"1.0.0\")",
                config_path.display()
            )
        })?;
    println!("config version: {version}");

    let store = ConfigDefinitionStore::from_config(&config);
    let mut ids = store.ids();
    ids.sort();
    println!("config: {}", config_path.display());
    println!("workflows ({}):", ids.len());
    for id in &ids {
        println!("  - {id}");
    }

    let imports: Vec<&str> = config
        .pointer("/proxy/import")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.get("connection").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if !imports.is_empty() {
        println!("imports ({}):", imports.len());
        for c in imports {
            println!("  - from connection: {c}");
        }
    }

    let diagnostics = mcp_flowgate_core::validate::validate_workflows(&config);
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = diagnostics.iter().filter(|d| !d.is_error()).count();
    let soft_warnings = soft_diagnostics.len();

    if !diagnostics.is_empty() {
        println!();
        for d in &diagnostics {
            println!("  {d}");
        }
    }
    // SPEC §5.4.2 / audit-resolution C.2 — print soft diagnostics under
    // their own banner so operators see them even when the rest of
    // validation succeeds.
    if !soft_diagnostics.is_empty() {
        println!();
        println!("soft warnings (resolve-time):");
        for d in &soft_diagnostics {
            let loc = d
                .location
                .as_deref()
                .map(|l| format!(" at {l}"))
                .unwrap_or_default();
            let suggestion = d
                .suggestion
                .as_deref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            println!("  warn[{}]{loc}: {}{suggestion}", d.code, d.message);
        }
    }
    if !diagnostics.is_empty() || !soft_diagnostics.is_empty() {
        println!();
        println!(
            "validation: {} error(s), {} warning(s), {} soft warning(s)",
            errors, warnings, soft_warnings
        );
    } else if !ids.is_empty() {
        println!("validation: ok");
    }

    if errors > 0 {
        anyhow::bail!("config validation failed with {errors} error(s)");
    }

    Ok(())
}

fn approvals_list(config_path: &PathBuf, all: bool) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let rt = tokio::runtime::Runtime::new()?;
    let sink = build_audit_sink(&config)?;

    let events = rt.block_on(sink.list_events()).unwrap_or_default();
    if events.is_empty() {
        let sink_kind = config
            .pointer("/audit/sink")
            .and_then(Value::as_str)
            .unwrap_or("stdout");
        match sink_kind {
            "stdout" | "none" => {
                eprintln!("audit.sink is '{sink_kind}' — events are not stored.");
                eprintln!("Switch to audit.sink: file to enable approvals tracking.");
            }
            _ => {
                println!("No approval requests found.");
            }
        }
        return Ok(());
    }

    let mut pending = Vec::new();
    let mut resolved_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for event in &events {
        let _event_id = &event.id;

        if event.event_type == "human.approval.resolved" {
            if let Some(approval_id) = event.payload.get("approval_id").and_then(Value::as_str) {
                resolved_ids.insert(approval_id.to_string());
            }
        }

        if event.event_type == "human.approval.requested" {
            pending.push(event);
        }
    }

    for event in &pending {
        let id = &event.id;
        let status = if resolved_ids.contains(id) {
            "resolved"
        } else {
            "pending"
        };
        if !all && resolved_ids.contains(id) {
            continue;
        }
        println!("[{status}] {id}");
        println!(
            "  queue:      {}",
            event
                .payload
                .get("queue")
                .and_then(Value::as_str)
                .unwrap_or("?")
        );
        println!(
            "  transition: {}",
            event
                .payload
                .get("transition")
                .and_then(Value::as_str)
                .unwrap_or("?")
        );
        println!(
            "  workflow:   {}",
            event.workflow_id.as_deref().unwrap_or("?")
        );
        println!();
    }

    Ok(())
}

fn approvals_resolve(config_path: &PathBuf, id: &str, outcome: &str) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let rt = tokio::runtime::Runtime::new()?;
    let sink = build_audit_sink(&config)?;

    // Verify the approval exists
    let events = rt.block_on(sink.list_events()).unwrap_or_default();
    let found = events
        .iter()
        .any(|e| e.event_type == "human.approval.requested" && e.id == id);

    if !found {
        anyhow::bail!("approval event '{}' not found in audit log", id);
    }

    // Record a resolution event via the audit sink
    let resolution = mcp_flowgate_core::audit::AuditEvent::new("human.approval.resolved")
        .with_payload(serde_json::json!({
            "approval_id": id,
            "outcome": outcome,
        }));

    rt.block_on(sink.record(resolution))?;

    println!("resolved approval {id} with outcome '{outcome}'");
    Ok(())
}

fn approvals_tail(config_path: &PathBuf) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let audit_dir = config
        .pointer("/audit/path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("audit.path is required for approvals tail"))?;

    let sink_kind = config
        .pointer("/audit/sink")
        .and_then(Value::as_str)
        .unwrap_or("stdout");
    if sink_kind != "file" {
        eprintln!("approvals tail requires audit.sink: file (current: {sink_kind})");
        return Ok(());
    }

    println!("tailing approvals from {}...", audit_dir);
    println!("(press Ctrl+C to stop)");

    // Track read position per log file so new rotated files are picked up.
    let mut file_offsets: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        tail_dir_once(audit_dir, &mut file_offsets, |event| {
            if event.get("event_type").and_then(Value::as_str)
                == Some("human.approval.requested")
            {
                let id = event.get("id").and_then(Value::as_str).unwrap_or("?");
                let queue = event
                    .get("payload")
                    .and_then(|p| p.get("queue"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let transition = event
                    .get("payload")
                    .and_then(|p| p.get("transition"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                println!("[{id}] queue={queue} transition={transition}");
            }
        });
    }
}

/// Pick a `WorkflowStore` implementation from `store: { kind, path }` config.
/// Defaults to in-memory.
fn build_workflow_store(config: &Value) -> anyhow::Result<Arc<dyn WorkflowStore>> {
    let kind = config
        .pointer("/store/kind")
        .and_then(Value::as_str)
        .unwrap_or("memory");
    let path = config.pointer("/store/path").and_then(Value::as_str);

    match kind {
        "memory" => Ok(Arc::new(InMemoryWorkflowStore::new())),
        "file" => {
            let path =
                path.ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=file"))?;
            Ok(Arc::new(FileWorkflowStore::new(path)?))
        }
        "sqlite" => {
            let path = path
                .ok_or_else(|| anyhow::anyhow!("store.path is required when store.kind=sqlite"))?;
            Ok(Arc::new(SqliteWorkflowStore::open(path)?))
        }
        "postgres" => {
            let url = path
                .ok_or_else(|| anyhow::anyhow!("store.url is required when store.kind=postgres"))?;
            // Support $ENV_VAR interpolation
            let url = resolve_env_vars(url);
            let store =
                tokio::runtime::Runtime::new()?.block_on(PostgresWorkflowStore::connect(&url))?;
            Ok(Arc::new(store))
        }
        other => anyhow::bail!("unknown store kind '{other}'"),
    }
}

/// Replace `${VAR_NAME}` patterns in a string with the corresponding
/// environment variable value. If a variable is not set, the placeholder
/// is left unchanged.
fn resolve_env_vars(s: &str) -> String {
    let mut result = s.to_string();
    while let Some(start) = result.find("${") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 2..start + end];
            if let Ok(val) = std::env::var(var_name) {
                result.replace_range(start..start + end + 1, &val);
            } else {
                // Keep the placeholder if env var not found
                break;
            }
        } else {
            break;
        }
    }
    result
}

fn build_audit_sink(config: &Value) -> anyhow::Result<Arc<dyn AuditSink>> {
    let sink_kind = config
        .pointer("/audit/sink")
        .and_then(Value::as_str)
        .unwrap_or("stdout");

    let sink: Arc<dyn AuditSink> = match sink_kind {
        "stdout" => Arc::new(StdoutAuditSink),
        "memory" => Arc::new(MemoryAuditSink::new()),
        "none" => Arc::new(NullAuditSink),
        "file" => {
            let path = config
                .pointer("/audit/path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("audit.path is required when audit.sink=file"))?;
            let rotation = parse_rotation_interval(config);
            Arc::new(FileAuditSink::new(path, rotation))
        }
        other => anyhow::bail!("unknown audit sink '{other}'"),
    };
    Ok(sink)
}

/// Parse `audit.rotation` from config; defaults to `Daily` when absent or
/// unrecognized.
fn parse_rotation_interval(config: &Value) -> RotationInterval {
    match config
        .pointer("/audit/rotation")
        .and_then(Value::as_str)
        .unwrap_or("daily")
    {
        "hourly" => RotationInterval::Hourly,
        "weekly" => RotationInterval::Weekly,
        _ => RotationInterval::Daily,
    }
}

/// Append imported capabilities to the config's `proxy.expose` array. Doesn't
/// touch declared exposures — guards, reliability, etc. on those are
/// preserved.
fn with_imports(mut config: Value, imported: &CapabilityRegistry) -> Value {
    if imported.is_empty() {
        return config;
    }
    let root = match config.as_object_mut() {
        Some(m) => m,
        None => return config,
    };
    let proxy = root.entry("proxy".to_string()).or_insert_with(|| json!({}));
    let proxy_obj = match proxy.as_object_mut() {
        Some(m) => m,
        None => return Value::Object(root.clone()),
    };
    let expose = proxy_obj
        .entry("expose".to_string())
        .or_insert_with(|| json!([]));
    let arr = match expose.as_array_mut() {
        Some(a) => a,
        None => return Value::Object(root.clone()),
    };
    arr.extend(imported.as_proxy_exposures());
    Value::Object(root.clone())
}

/// Poll a directory of rotated log files for new lines. Tracks per-file byte
/// offsets in `file_offsets` so each call only reads appended bytes. Newly
/// appearing files (rotation events) are picked up automatically.
///
/// `handler` is called once per parsed JSON line; errors on individual lines
/// are silently skipped to keep the tail running.
fn tail_dir_once(
    dir: &str,
    file_offsets: &mut std::collections::HashMap<PathBuf, u64>,
    mut handler: impl FnMut(&Value),
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    paths.sort();

    for path in paths {
        let file_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let offset = file_offsets.entry(path.clone()).or_insert(0);
        if file_len <= *offset {
            continue;
        }
        if let Ok(file) = std::fs::File::open(&path) {
            use std::io::{BufRead, BufReader, Seek, SeekFrom};
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(*offset)).ok();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if let Ok(event) = serde_json::from_str::<Value>(trimmed) {
                        handler(&event);
                    }
                }
                line.clear();
            }
            *offset = reader.stream_position().unwrap_or(file_len);
        }
    }
}

fn inspect_workflow(config_path: &PathBuf, workflow_id: &str) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let store = build_workflow_store(&config)?;

    let rt = tokio::runtime::Runtime::new()?;
    let instance = rt.block_on(store.load(workflow_id))?;

    println!("Workflow: {}", instance.id);
    println!("  Definition:  {}", instance.definition_id);
    println!("  State:       {}", instance.state);
    println!("  Version:     {}", instance.version);
    println!("  Started at:  {}", instance.started_at.to_rfc3339());
    println!(
        "  Input:       {}",
        serde_json::to_string_pretty(&instance.input)?
    );
    println!(
        "  Context:     {}",
        serde_json::to_string_pretty(&instance.context)?
    );

    Ok(())
}

fn audit_tail(config_path: &PathBuf, filter: &Option<String>) -> anyhow::Result<()> {
    let config = load_config(config_path)?;
    let audit_dir = config
        .pointer("/audit/path")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("audit.path is required for audit tail"))?;

    println!("tailing audit events from {}...", audit_dir);
    if let Some(f) = filter {
        println!("filter: event_type == \"{f}\"");
    }
    println!("(press Ctrl+C to stop)");

    let mut file_offsets: std::collections::HashMap<PathBuf, u64> = std::collections::HashMap::new();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let filter_ref = filter.as_deref();
        tail_dir_once(audit_dir, &mut file_offsets, |event| {
            let event_type = event
                .get("event_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            if let Some(f) = filter_ref {
                if event_type != f {
                    return;
                }
            }
            if let Ok(pretty) = serde_json::to_string_pretty(&event) {
                println!("{pretty}");
                println!("---");
            }
        });
    }
}
