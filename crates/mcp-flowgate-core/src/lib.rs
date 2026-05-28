//! mcp-flowgate-core: workflow runtime, ports, audit, reliability.
//!
//! Every exposed proxy tool is internally represented as a workflow transition.
//! A simple proxy config and a fully-governed workflow share one execution
//! model — see `proxy_workflow::compile_proxy_workflow` for the bridge.
//!
//! # Lock poisoning policy
//!
//! Every `RwLock` / `Mutex` in this crate is acquired via
//! `.expect("LOCK_POISONED: ...")` rather than `.unwrap()`. The
//! invariant: NO holder of any lock in this crate performs fallible
//! I/O or holds an `await` point while the guard is live. Under
//! that invariant, the locks cannot be poisoned (poisoning requires
//! a panic in a holder, which the invariant forbids).
//!
//! If you add a `?`, `.await`, or `panic!()` inside a lock guard,
//! the invariant is broken and the `expect` becomes a real panic
//! risk. Either refactor to release the guard first or upgrade to
//! `parking_lot` (no poisoning).

pub mod audit;
pub mod cap_verb;
pub mod capability;
pub mod contract_hash;
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
pub mod slot_table;
pub mod store;
pub mod store_file;
pub mod store_postgres;
pub mod store_sqlite;
pub mod validate;
pub mod templating;
pub mod tier;
pub mod use_binding;
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
pub use repo::{load_manifest, load_repo, RepoLayout, RepoManifest, REPO_MANIFEST_SCHEMA_V1};
pub use model::*;
pub use ports::*;
pub use proxy_workflow::{compile_proxy_workflow, DEFAULT_PROXY_STATE, DEFAULT_PROXY_WORKFLOW_ID};
pub use reliability::{Backoff, FallbackPolicy, ReliabilityPolicy, RetryPolicy};
pub use runtime::WorkflowRuntime;
pub use store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
pub use store_file::FileWorkflowStore;
pub use store_postgres::PostgresWorkflowStore;
pub use store_sqlite::SqliteWorkflowStore;
