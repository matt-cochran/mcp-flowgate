// T26 — restriction-category lint on production code only. See
// mcp-flowgate-core/src/lib.rs for the rationale.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! Default executors for mcp-flowgate.

pub mod cli;
pub mod dry_run;
pub mod human;
pub mod import;
pub mod ingest;
pub mod mcp;
pub mod noop;
pub mod parallel;
pub mod pipeline;
pub mod registry;
pub mod registry_executor;
pub mod rest;
pub mod script;
pub mod structural_analysis;
pub mod workflow;

pub use cli::{CliConnection, CliConnections, CliExecutor};
pub use dry_run::DryRunExecutor;
pub use human::HumanExecutor;
pub use import::import_capabilities;
pub use ingest::IngestExecutor;
pub use mcp::{McpConnection, McpConnections, McpExecutor};
pub use noop::NoopExecutor;
pub use parallel::ParallelExecutor;
pub use pipeline::PipelineExecutor;

/// SPEC §24 GAP-E mitigation — the canonical list of executor kinds the
/// default registry builders wire in. Tooling (drift tests, schema
/// validators) reads this to assert parity against the JSON schema's
/// `executor.properties.kind.examples` array. Adding a new executor kind
/// means appending here AND to the schema in the SAME commit; the drift
/// test fails the build if they diverge.
///
/// `registry` is omitted intentionally — it's an authoring-time-only
/// executor (`RegistryExecutor`) whose `kind` value isn't a stable
/// runtime executor kind.
pub const REGISTERED_EXECUTOR_KINDS: &[&str] = &[
    "cli", "human", "mcp", "noop", "parallel", "pipeline", "rest", "script", "workflow",
];
pub use registry::HashMapExecutorRegistry;
pub use registry_executor::RegistryExecutor;
pub use rest::{RestConnection, RestConnections, RestExecutor};
pub use script::ScriptExecutor;
pub use structural_analysis::{StructuralAnalysisExecutor, REQUIRED_RULES};
pub use workflow::WorkflowExecutor;

use std::sync::Arc;

use mcp_flowgate_core::ports::ExecutorRegistry;
use mcp_flowgate_core::runtime::WorkflowRuntime;
use serde_json::Value;

/// Build a registry containing the default executor set wired up against the
/// given config. Convenient one-shot entry point for the binary.
pub fn default_registry(config: &Value) -> Arc<dyn ExecutorRegistry> {
    let cli_conns = Arc::new(CliConnections::from_config(config));
    let mcp_conns = McpConnections::from_config(config);
    default_registry_with_mcp(
        config,
        Arc::new(McpExecutor::new(mcp_conns)),
        cli_conns,
        Arc::new(mcp_flowgate_core::audit::NullAuditSink),
    )
}

/// Same as `default_registry` but lets the caller supply pre-built CLI and
/// MCP executors and an audit sink. Useful when you want to share the MCP
/// executor with the importer (so the connection cache is reused) and route
/// human-approval audit events to the gateway's main audit stream — see the
/// `mcp-flowgate` binary for the canonical wiring.
pub fn default_registry_with_mcp(
    config: &Value,
    mcp_executor: Arc<McpExecutor>,
    cli_connections: Arc<CliConnections>,
    audit: Arc<dyn mcp_flowgate_core::audit::AuditSink>,
) -> Arc<dyn ExecutorRegistry> {
    let rest_connections = Arc::new(RestConnections::from_config(config));
    // SPEC §24 — `ParallelExecutor` needs a back-reference to the registry
    // so its branches can invoke other executors. Construct first, register
    // with a clone, then wire the registry back into the parallel executor
    // after the registry Arc exists.
    let parallel = Arc::new(ParallelExecutor::new(audit.clone()));
    let pipeline = Arc::new(PipelineExecutor::new(audit.clone()));
    let registry = HashMapExecutorRegistry::new()
        .with("cli", Arc::new(CliExecutor::new(cli_connections)))
        .with(
            "mcp",
            mcp_executor as Arc<dyn mcp_flowgate_core::ports::Executor>,
        )
        .with("rest", Arc::new(RestExecutor::new(rest_connections)))
        .with("human", Arc::new(HumanExecutor::with_audit(audit)))
        .with("noop", Arc::new(NoopExecutor))
        .with("script", Arc::new(ScriptExecutor::new()))
        .with(
            "parallel",
            parallel.clone() as Arc<dyn mcp_flowgate_core::ports::Executor>,
        )
        .with(
            "pipeline",
            pipeline.clone() as Arc<dyn mcp_flowgate_core::ports::Executor>,
        );

    let registry: Arc<dyn ExecutorRegistry> = Arc::new(registry);
    parallel.set_registry(registry.clone());
    pipeline.set_registry(registry.clone());
    registry
}

/// Build a registry with the workflow executor. Requires a WorkflowRuntime
/// for spawning sub-workflows.
pub fn default_registry_with_workflow(
    config: &Value,
    mcp_executor: Arc<McpExecutor>,
    cli_connections: Arc<CliConnections>,
    audit: Arc<dyn mcp_flowgate_core::audit::AuditSink>,
    runtime: WorkflowRuntime,
) -> Arc<dyn ExecutorRegistry> {
    let rest_connections = Arc::new(RestConnections::from_config(config));
    let parallel = Arc::new(ParallelExecutor::new(audit.clone()));
    let pipeline = Arc::new(PipelineExecutor::new(audit.clone()));
    let registry = HashMapExecutorRegistry::new()
        .with("cli", Arc::new(CliExecutor::new(cli_connections)))
        .with(
            "mcp",
            mcp_executor as Arc<dyn mcp_flowgate_core::ports::Executor>,
        )
        .with("rest", Arc::new(RestExecutor::new(rest_connections)))
        .with("human", Arc::new(HumanExecutor::with_audit(audit.clone())))
        .with("noop", Arc::new(NoopExecutor))
        .with("script", Arc::new(ScriptExecutor::new()))
        .with("workflow", Arc::new(WorkflowExecutor::new(runtime, audit)))
        .with(
            "parallel",
            parallel.clone() as Arc<dyn mcp_flowgate_core::ports::Executor>,
        )
        .with(
            "pipeline",
            pipeline.clone() as Arc<dyn mcp_flowgate_core::ports::Executor>,
        );

    let registry: Arc<dyn ExecutorRegistry> = Arc::new(registry);
    parallel.set_registry(registry.clone());
    pipeline.set_registry(registry.clone());
    registry
}
