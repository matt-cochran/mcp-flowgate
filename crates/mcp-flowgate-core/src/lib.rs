//! mcp-flowgate-core: workflow runtime, ports, audit, reliability.
//!
//! Every exposed proxy tool is internally represented as a workflow transition.
//! A simple proxy config and a fully-governed workflow share one execution
//! model — see `proxy_workflow::compile_proxy_workflow` for the bridge.

pub mod audit;
pub mod capability;
pub mod config;
pub mod discovery;
pub mod discovery_indexer;
pub mod error;
pub mod fs;
pub mod guards;
pub mod hot_reload;
pub mod mapping;
pub mod model;
pub mod ports;
pub mod proxy_workflow;
pub mod lexicon;
pub mod reliability;
pub mod repo;
pub mod runtime;
pub mod slot_constraint;
pub mod store;
pub mod store_file;
pub mod store_postgres;
pub mod store_sqlite;
pub mod validate;
pub mod templating;
pub mod runtime_chain;
pub mod runtime_links;
pub mod runtime_records;
pub mod runtime_response;
pub mod runtime_schema;
pub mod runtime_submit;

pub use audit::{
    AuditEvent, AuditSink, FileAuditSink, MemoryAuditSink, NullAuditSink, RotationInterval,
    StdoutAuditSink,
};
pub use capability::{Capability, CapabilityRegistry, CapabilitySource};
pub use discovery::{
    DiscoveryIndex, DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex, SearchHit,
    SearchRequest,
};
pub use error::{ErrorClass, ExecutorError, RuntimeError};
pub use fs::{Filesystem, InMemoryFilesystem, RealFilesystem};
pub use guards::DefaultGuardEvaluator;
pub use mapping::{merge_output, read_in_scopes};
pub use model::*;
pub use ports::*;
pub use proxy_workflow::{compile_proxy_workflow, DEFAULT_PROXY_STATE, DEFAULT_PROXY_WORKFLOW_ID};
pub use reliability::{Backoff, FallbackPolicy, ReliabilityPolicy, RetryPolicy};
pub use runtime::WorkflowRuntime;
pub use store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
pub use store_file::FileWorkflowStore;
pub use store_postgres::PostgresWorkflowStore;
pub use store_sqlite::SqliteWorkflowStore;
