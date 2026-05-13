//! Golden / scenario tests pressure-testing realistic workflow patterns.
//!
//! Each scenario:
//!   1. Builds a runtime from inline YAML.
//!   2. Drives a sequence of `start` / `submit` calls.
//!   3. Asserts the response shape, error codes, audit taxonomy, and final
//!      context — the **observable contract** of the gateway.
//!
//! Scenarios are grouped by the realistic pattern they probe:
//!   - **Baseline**: confirms the existing declarative surface handles the
//!     common case.
//!   - **Stress**: probes patterns that have to work declaratively or the
//!     "as declarative as possible" promise breaks.
//!
//! When a stress test was added because the system couldn't express the
//! pattern declaratively, the test serves as a regression guard for the
//! minimum surface added to fix the gap. See `docs/STRESS-TESTS.md` for
//! the narrative.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_flowgate_core::audit::{AuditSink, MemoryAuditSink};
use mcp_flowgate_core::config::resolve_str;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::guards::DefaultGuardEvaluator;
use mcp_flowgate_core::model::{
    Evidence, ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use mcp_flowgate_core::ports::{EvidenceStore, Executor, ExecutorRegistry};
use mcp_flowgate_core::store::{
    ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore,
};
use mcp_flowgate_core::WorkflowRuntime;
use serde_json::{json, Value};

// ---------- helpers --------------------------------------------------------

/// A canned executor that returns a fixed output (and optional evidence) on
/// every call. Useful for deterministic scenarios where the runtime is the
/// thing under test.
struct FixedExecutor {
    output: Value,
    evidence_kinds: Vec<String>,
}

impl FixedExecutor {
    fn new(output: Value) -> Self {
        Self {
            output,
            evidence_kinds: vec![],
        }
    }
}

#[async_trait]
impl Executor for FixedExecutor {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: self.output.clone(),
            evidence: self
                .evidence_kinds
                .iter()
                .map(|k| Evidence {
                    kind: k.clone(),
                    id: format!("ev_{}", k),
                    uri: None,
                    summary: None,
                })
                .collect(),
        })
    }
}

/// A registry that returns a single executor for any kind. Lets scenarios
/// inject canned outputs without touching the YAML executor.kind field.
struct AnyKind(Arc<dyn Executor>);
impl ExecutorRegistry for AnyKind {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

/// A registry that maps kind names to specific executors. Use when scenarios
/// need different per-step behavior.
struct ByKind(std::collections::HashMap<String, Arc<dyn Executor>>);
impl ByKind {
    fn new() -> Self {
        Self(std::collections::HashMap::new())
    }
    fn with(mut self, kind: &str, exec: Arc<dyn Executor>) -> Self {
        self.0.insert(kind.to_string(), exec);
        self
    }
}
impl ExecutorRegistry for ByKind {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        self.0.get(kind).cloned()
    }
}

struct Scenario {
    runtime: WorkflowRuntime,
    audit: Arc<MemoryAuditSink>,
    last: Option<Value>,
}

impl Scenario {
    fn build(yaml: &str, executors: Arc<dyn ExecutorRegistry>) -> Self {
        Self::build_with_evidence(yaml, executors, false)
    }

    fn build_with_evidence(
        yaml: &str,
        executors: Arc<dyn ExecutorRegistry>,
        with_evidence: bool,
    ) -> Self {
        let config = resolve_str(yaml).expect("config parses + resolves");
        let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
        let store = Arc::new(InMemoryWorkflowStore::new());
        let audit = Arc::new(MemoryAuditSink::new());
        let evidence: Arc<dyn EvidenceStore> = Arc::new(InMemoryEvidenceStore::new());

        let guards: Arc<dyn mcp_flowgate_core::ports::GuardEvaluator> = if with_evidence {
            Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()))
        } else {
            Arc::new(DefaultGuardEvaluator::new())
        };

        let runtime = WorkflowRuntime::new(
            definitions,
            store,
            executors,
            guards,
            audit.clone() as Arc<dyn AuditSink>,
        );
        let runtime = if with_evidence {
            runtime.with_evidence(evidence)
        } else {
            runtime
        };

        Scenario {
            runtime,
            audit,
            last: None,
        }
    }

    async fn start(&mut self, def: &str, input: Value, principal: Principal) -> &Value {
        let resp = self
            .runtime
            .start(StartWorkflow {
                definition_id: def.to_string(),
                input,
                principal,
            })
            .await
            .expect("start succeeds");
        self.last = Some(resp);
        self.last.as_ref().unwrap()
    }

    async fn submit(&mut self, transition: &str, args: Value, principal: Principal) -> &Value {
        let workflow_id = self.last.as_ref().unwrap()["workflow"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let version = self.last.as_ref().unwrap()["workflow"]["version"]
            .as_u64()
            .unwrap();
        let resp = self
            .runtime
            .submit(SubmitTransition {
                workflow_id,
                expected_version: version,
                transition: transition.to_string(),
                arguments: args,
                principal,
            })
            .await
            .expect("submit returns Ok (rejection is in body)");
        self.last = Some(resp);
        self.last.as_ref().unwrap()
    }

    /// Submit with an explicit (e.g. stale) expectedVersion.
    async fn submit_with_version(
        &mut self,
        transition: &str,
        version: u64,
        args: Value,
        principal: Principal,
    ) -> &Value {
        let workflow_id = self.last.as_ref().unwrap()["workflow"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let resp = self
            .runtime
            .submit(SubmitTransition {
                workflow_id,
                expected_version: version,
                transition: transition.to_string(),
                arguments: args,
                principal,
            })
            .await
            .expect("submit returns Ok");
        self.last = Some(resp);
        self.last.as_ref().unwrap()
    }

    fn last(&self) -> &Value {
        self.last.as_ref().unwrap()
    }

    fn link_rels(&self) -> Vec<String> {
        self.last
            .as_ref()
            .unwrap()
            .get("links")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l["rel"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn audit_event_types(&self) -> Vec<String> {
        self.audit.event_types()
    }
}

fn anon() -> Principal {
    Principal::anonymous()
}

fn principal(perms: &[&str]) -> Principal {
    Principal {
        subject: "tester".into(),
        roles: vec![],
        permissions: perms.iter().map(|s| s.to_string()).collect(),
    }
}

fn human() -> Principal {
    Principal {
        subject: "human-tester".into(),
        roles: vec![Principal::HUMAN_ROLE.into()],
        permissions: vec![],
    }
}

// ===========================================================================
//  BASELINE SCENARIOS
//  Each captures a realistic pattern the existing surface should handle.
// ===========================================================================

/// **B-01.** Simplest possible proxy call: declare one capability, call it.
#[tokio::test]
async fn b01_proxy_default_call() {
    let yaml = r#"
        version: "1.0.0"
        proxy:
          expose:
            - name: hello.echo
              executor: { kind: noop }
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({ "ok": true })));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("proxy_default", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "started");
    assert_eq!(s.last()["workflow"]["state"], "ready");
    assert_eq!(s.link_rels(), vec!["hello.echo"]);

    s.submit("hello.echo", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "executed");
    assert_eq!(s.last()["workflow"]["state"], "ready");
}

/// **B-02.** Multi-state governed flow happy path: planning → review → done.
#[tokio::test]
async fn b02_governed_happy_path() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          change:
            initialState: planning
            states:
              planning:
                transitions:
                  submit:
                    target: reviewing
                    executor: { kind: noop }
              reviewing:
                transitions:
                  approve:
                    target: done
                    guards: [{ kind: permission, permission: change.approve }]
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("change", json!({}), anon()).await;
    s.submit("submit", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "reviewing");

    s.submit("approve", json!({}), principal(&["change.approve"]))
        .await;
    assert_eq!(s.last()["result"]["status"], "completed");
    assert_eq!(s.last()["workflow"]["state"], "done");
    assert!(s.last()["links"].as_array().unwrap().is_empty());
}

/// **B-03.** Schema rejection includes the legal links so the caller can
/// recover without restarting.
#[tokio::test]
async fn b03_schema_rejection_returns_legal_links() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            states:
              open:
                transitions:
                  go:
                    target: done
                    inputSchema:
                      type: object
                      required: [name]
                      properties: { name: { type: string } }
                      additionalProperties: false
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({ "name": 42 }), anon()).await; // wrong type

    assert_eq!(s.last()["result"]["status"], "rejected");
    assert_eq!(s.last()["error"]["code"], "INPUT_SCHEMA_VIOLATION");
    assert!(s.link_rels().contains(&"go".to_string()));
}

/// **B-04.** Guard rejection: workflow stays put, response carries
/// recovery links, audit shows `transition.rejected`.
#[tokio::test]
async fn b04_guard_rejection_audited_and_recoverable() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            states:
              open:
                transitions:
                  approve:
                    target: done
                    guards: [{ kind: permission, permission: demo.approve }]
                    executor: { kind: noop }
                  reject:
                    target: open
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit("approve", json!({}), anon()).await; // missing permission

    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
    assert_eq!(s.last()["workflow"]["state"], "open");
    let rels = s.link_rels();
    assert!(rels.contains(&"approve".to_string()));
    assert!(rels.contains(&"reject".to_string()));

    let events = s.audit_event_types();
    assert!(events.iter().any(|e| e == "transition.rejected"));
    assert!(events.iter().any(|e| e == "guard.evaluated"));
}

/// **B-05.** Stale `expectedVersion` is rejected even when guards/schema
/// pass.
#[tokio::test]
async fn b05_stale_version_rejected() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            states:
              open:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit_with_version("go", 999, json!({}), anon()).await;

    assert_eq!(s.last()["error"]["code"], "STALE_WORKFLOW_VERSION");
}

/// **B-06.** Reliability: retries exhaust → `failed` status, not state advance.
#[tokio::test]
async fn b06_retry_exhaustion_marks_failed_not_advanced() {
    struct AlwaysFail;
    #[async_trait]
    impl Executor for AlwaysFail {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Err(ExecutorError::Transient("nope".into()))
        }
    }

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    reliability:
                      retry: { maxAttempts: 3, retryOn: [transient_error] }
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(Arc::new(AlwaysFail))));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    assert_eq!(s.last()["result"]["status"], "failed");
    assert_eq!(s.last()["workflow"]["state"], "s"); // didn't advance
    let evt = s.audit_event_types();
    assert!(evt.iter().any(|e| e == "executor.retrying"));
    assert!(evt.iter().any(|e| e == "executor.failed"));
}

/// **B-07.** Reliability: fallback wins after primary exhausts retries.
#[tokio::test]
async fn b07_fallback_succeeds_after_primary_exhausts() {
    struct AlwaysFail;
    #[async_trait]
    impl Executor for AlwaysFail {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Err(ExecutorError::Transient("primary down".into()))
        }
    }
    struct AlwaysOk;
    #[async_trait]
    impl Executor for AlwaysOk {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    let registry = ByKind::new()
        .with("primary", Arc::new(AlwaysFail))
        .with("backup", Arc::new(AlwaysOk));

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: primary }
                    reliability:
                      retry: { maxAttempts: 1 }
                      fallback:
                        executors:
                          - { kind: backup }
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(registry));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    assert_eq!(s.last()["result"]["status"], "completed");
    assert!(s
        .audit_event_types()
        .iter()
        .any(|e| e == "fallback.selected"));
}

/// **B-08.** Capability reference: same definition reused in proxy and
/// inside a workflow transition.
#[tokio::test]
async fn b08_named_capability_reused() {
    let yaml = r#"
        version: "1.0.0"
        capabilities:
          do_thing:
            executor: { kind: noop }
        proxy:
          expose: [{ capability: do_thing }]
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { capability: do_thing }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    // Use it from proxy_default…
    s.start("proxy_default", json!({}), anon()).await;
    assert_eq!(s.link_rels(), vec!["do_thing"]);
    s.submit("do_thing", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "executed");

    // …and from the named workflow.
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");
}

// ===========================================================================
//  STRESS SCENARIOS
//  Each was added because the existing declarative surface couldn't express
//  a realistic pattern. The scenario is the "test that proves the gap";
//  passing it requires the corresponding fix in the runtime.
// ===========================================================================

/// **S-01. Bounded loop with a counter.** "Remediate up to 3 times, then
/// you must escalate." This requires three things, each individually a
/// declarative gap:
///
/// - **A way to seed the counter.** The original failure was: the very
///   first remediate's guard `$.context.attempts < 3` evaluated against
///   missing `attempts`, returned false, and blocked the loop before it
///   started. Fixed by `initialContext: {...}` on the workflow
///   definition — a declarative way to seed instance state without an
///   `onEnter` that would also fire on every self-transition.
/// - **Arithmetic in output mappings.** Without `{ add: [a, b] }` the
///   only way to write `count + 1` was a custom executor — a procedural
///   workaround. Fixed by the operator object form in
///   `mapping::resolve_value`.
/// - **Scope-aware reads.** The mapping value `$.context.attempts` has
///   to resolve against the *workflow context* even though the mapping
///   normally reads `$.output.*`. Fixed by routing through
///   `read_in_scopes`.
#[tokio::test]
async fn s01_bounded_loop_counter() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            initialContext:
              attempts: 0
            states:
              open:
                transitions:
                  remediate:
                    target: open
                    guards:
                      - { kind: jsonpath, expr: "$.context.attempts < 3" }
                    executor: { kind: noop }
                    output:
                      attempts: { add: ["$.context.attempts", 1] }
                  escalate:
                    target: escalated
                    guards:
                      - { kind: jsonpath, expr: "$.context.attempts >= 3" }
                    executor: { kind: noop }
              escalated:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    assert_eq!(s.last()["context"]["attempts"], 0);

    for i in 1..=3 {
        s.submit("remediate", json!({}), anon()).await;
        assert_eq!(s.last()["context"]["attempts"], i);
        assert_eq!(s.last()["workflow"]["state"], "open");
    }

    // After 3 remediations, the remediate guard fails and only `escalate`
    // makes progress.
    s.submit("remediate", json!({}), anon()).await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");

    s.submit("escalate", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");
    assert_eq!(s.last()["workflow"]["state"], "escalated");
}

/// **S-02. Schema defaults are applied to arguments before validation.**
/// "If the caller omits `priority`, default to `normal`." Standard JSON
/// Schema feature; without it, every executor has to null-check the field.
///
/// Fix: walk the inputSchema's `properties` and fill in any `default` for
/// missing keys before validating + dispatching.
#[tokio::test]
async fn s02_schema_defaults_applied_to_arguments() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    inputSchema:
                      type: object
                      required: [priority, ticket]
                      properties:
                        priority: { type: string, default: "normal" }
                        ticket:   { type: string }
                      additionalProperties: false
                    executor: { kind: noop }
                    output:
                      priority: "$.arguments.priority"   # echoes resolved default
                      ticket:   "$.arguments.ticket"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    // Caller omits `priority`; the runtime must apply the schema default
    // and pass validation.
    s.submit("go", json!({ "ticket": "T-1" }), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");
    assert_eq!(s.last()["context"]["priority"], "normal");
    assert_eq!(s.last()["context"]["ticket"], "T-1");
}

/// **S-03. Multi-approver quorum.** "Two of any three reviewers must
/// approve." A common change-management pattern. Without counted evidence,
/// the only options are a custom guard (not declarative) or recording
/// distinct evidence kinds per approver and listing all combinations
/// (combinatorial).
///
/// Fix: the `evidence` guard's `requires` accepts `{ kind, count }` for
/// quorums alongside the bare-string form.
#[tokio::test]
async fn s03_multi_approver_quorum() {
    // Each approve transition records a fresh `approval` evidence record.
    struct Approver;
    #[async_trait]
    impl Executor for Approver {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult {
                output: json!({ "approved": true }),
                evidence: vec![Evidence {
                    kind: "approval".into(),
                    id: format!("ev_{}", uuid::Uuid::new_v4().simple()),
                    uri: None,
                    summary: None,
                }],
            })
        }
    }
    struct Noop;
    #[async_trait]
    impl Executor for Noop {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    let registry = ByKind::new()
        .with("noop", Arc::new(Noop))
        .with("approver", Arc::new(Approver));

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: pending
            states:
              pending:
                transitions:
                  approve:
                    target: pending          # self-loop until quorum reached
                    actor: human
                    executor: { kind: approver }
                  finalize:
                    target: done
                    guards:
                      - kind: evidence
                        requires:
                          - { kind: approval, count: 2 }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let mut s = Scenario::build_with_evidence(yaml, Arc::new(registry), true);

    s.start("demo", json!({}), anon()).await;

    // First approval: not yet enough; finalize must reject. `approve` is
    // tagged `actor: human`, so submits must come from a human principal.
    s.submit("approve", json!({}), human()).await;
    assert_eq!(s.last()["result"]["status"], "executed");
    s.submit("finalize", json!({}), anon()).await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");

    // Second approval: now quorum is reached.
    s.submit("approve", json!({}), human()).await;
    s.submit("finalize", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");
    assert_eq!(s.last()["workflow"]["state"], "done");
}

/// **S-04. Nested schema defaults.** Defaults aren't only top-level —
/// nested object properties should fill in too. Without recursion in the
/// default-application walk, complex shapes can't declare a default for a
/// nested field.
#[tokio::test]
async fn s04_nested_schema_defaults() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    inputSchema:
                      type: object
                      required: [request]
                      properties:
                        request:
                          type: object
                          properties:
                            priority: { type: string, default: "normal" }
                            channel:  { type: string, default: "email" }
                            ticket:   { type: string }
                    executor: { kind: noop }
                    output:
                      priority: "$.arguments.request.priority"
                      channel:  "$.arguments.request.channel"
                      ticket:   "$.arguments.request.ticket"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({ "request": { "ticket": "T-1" } }), anon())
        .await;
    assert_eq!(s.last()["result"]["status"], "completed");
    assert_eq!(s.last()["context"]["priority"], "normal");
    assert_eq!(s.last()["context"]["channel"], "email");
    assert_eq!(s.last()["context"]["ticket"], "T-1");
}

/// **S-05.** `set:` operator for declaring literal values in output
/// mappings (useful for status flags, bookmarks, etc.).
#[tokio::test]
async fn s05_output_mapping_set_literal() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  mark_reviewed:
                    target: done
                    executor: { kind: noop }
                    output:
                      status: { set: "reviewed" }
                      reviewer_count: { set: 1 }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("mark_reviewed", json!({}), anon()).await;
    assert_eq!(s.last()["context"]["status"], "reviewed");
    assert_eq!(s.last()["context"]["reviewer_count"], 1);
}

/// **S-07. Link prefill (LLM guidance).** A transition declares `prefill`:
/// at link-generation time those values resolve against current scopes
/// and land in `link.args.arguments`. The LLM caller takes that block as
/// the starting point and only generates the genuinely-LLM-required
/// fields (e.g. PR title and body), instead of having to assemble every
/// argument the call needs.
///
/// Fix: transition `prefill` block + reuse of `mapping::resolve_value` at
/// link generation.
#[tokio::test]
async fn s07_link_prefill_arguments() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: tested
            initialContext:
              branch: feat/x
            inputSchema:
              type: object
              properties:
                repo: { type: string }
            states:
              tested:
                transitions:
                  create_pr:
                    target: review
                    inputSchema:
                      type: object
                      required: [repo, base, head, title]
                      properties:
                        repo:  { type: string }
                        base:  { type: string }
                        head:  { type: string }
                        title: { type: string }
                    prefill:
                      repo: "$.workflow.input.repo"
                      base: "main"
                      head: "$.context.branch"
                      labels: ["auto-generated"]
                    executor: { kind: noop }
              review:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({ "repo": "owner/repo" }), anon())
        .await;
    let link = s.last()["links"]
        .as_array()
        .and_then(|a| a.iter().find(|l| l["rel"] == "create_pr"))
        .expect("create_pr link present");
    let prefilled = &link["args"]["arguments"];
    assert_eq!(prefilled["repo"], "owner/repo");
    assert_eq!(prefilled["base"], "main");
    assert_eq!(prefilled["head"], "feat/x");
    assert_eq!(prefilled["labels"], json!(["auto-generated"]));
    // The LLM only has to fill `title` (in inputSchema.required, not in prefilled).
    let inputs = link["inputSchema"]["required"].as_array().unwrap();
    assert!(inputs.iter().any(|v| v == "title"));
}

/// **S-08. Idempotency key auto.** With `executor.idempotencyKey: true`,
/// the runtime computes a stable key per `submit` and feeds it to the
/// executor (REST → header, CLI → env, MCP → arg). Retries within the
/// same submit share the key so downstream services can dedupe.
#[tokio::test]
async fn s08_idempotency_key_auto() {
    use std::sync::Mutex as StdMutex;
    struct Recorder {
        keys: StdMutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl Executor for Recorder {
        async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            self.keys.lock().unwrap().push(req.idempotency_key.clone());
            Ok(ExecuteResult::default())
        }
    }
    let recorder = Arc::new(Recorder {
        keys: StdMutex::new(vec![]),
    });

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor:
                      kind: noop
                      idempotencyKey: true
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(recorder.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    let keys = recorder.keys.lock().unwrap();
    assert_eq!(
        keys.len(),
        1,
        "executor invoked exactly once on a happy path"
    );
    let key = keys[0].as_ref().expect("idempotency key present");
    assert!(
        key.starts_with("wf_") && key.contains(".go."),
        "auto key shape includes workflowId.transition.correlationId; got {key}"
    );
}

/// **S-09. Idempotency key custom template.** A workflow author can
/// provide a custom template using `{workflowId}`, `{transition}`,
/// `{correlationId}` placeholders. Useful when downstream APIs require a
/// specific key format.
#[tokio::test]
async fn s09_idempotency_key_custom_template() {
    use std::sync::Mutex as StdMutex;
    struct Recorder {
        keys: StdMutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl Executor for Recorder {
        async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            self.keys.lock().unwrap().push(req.idempotency_key.clone());
            Ok(ExecuteResult::default())
        }
    }
    let recorder = Arc::new(Recorder {
        keys: StdMutex::new(vec![]),
    });

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor:
                      kind: noop
                      idempotencyKey: "flowgate:{transition}:{workflowId}"
              done:
                terminal: true
    "#;
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(recorder.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;

    let keys = recorder.keys.lock().unwrap();
    let key = keys[0].as_ref().unwrap();
    assert!(key.starts_with("flowgate:go:wf_"), "got {key}");
}

/// **S-10. Workflow-level lazy timeout.** A workflow declares
/// `timeoutMs` + `onTimeout.target`. If the next `submit`/`get` arrives
/// after the deadline, the runtime auto-transitions to the timeout
/// state, emits `workflow.timed_out`, and short-circuits without
/// running the requested transition.
#[tokio::test]
async fn s10_workflow_level_lazy_timeout() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          short_lived:
            initialState: open
            timeoutMs: 1
            onTimeout:
              target: timed_out_state
            states:
              open:
                transitions:
                  approve:
                    target: done
                    executor: { kind: noop }
              timed_out_state:
                terminal: true
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("short_lived", json!({}), anon()).await;
    // Sleep past the 1ms deadline.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    s.submit("approve", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "timed_out_state");
    assert_eq!(s.last()["result"]["status"], "completed");
    assert!(
        s.audit_event_types()
            .iter()
            .any(|t| t == "workflow.timed_out"),
        "audit must include workflow.timed_out"
    );
}

/// **S-11. Link filtering by guards.** When a workflow declares
/// `linkFilter: byGuards`, the response's `links` array only shows
/// transitions whose guards would currently pass — the LLM never sees
/// transitions it can't take. Reduces wasted submit attempts.
#[tokio::test]
async fn s11_link_filter_byguards() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: triaged
            linkFilter: byGuards
            initialContext:
              risk: 30
            states:
              triaged:
                transitions:
                  auto_approve:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.context.risk <= 50" }
                    executor: { kind: noop }
                  manual_review:
                    target: review
                    guards:
                      - { kind: expr, expr: "$.context.risk > 50" }
                    executor: { kind: noop }
              review:
                terminal: true
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    // risk=30 ⇒ only auto_approve is reachable.
    let rels = s.link_rels();
    assert_eq!(rels, vec!["auto_approve"]);
}

/// **S-12. Per-state link filter overrides workflow-level.** The flag
/// can be opt-in for one tricky state without committing the whole
/// workflow. The state-level setting wins.
#[tokio::test]
async fn s12_link_filter_per_state_override() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            linkFilter: all
            initialContext:
              risk: 90
            states:
              open:
                linkFilter: byGuards
                transitions:
                  go_safe:
                    target: open
                    guards:
                      - { kind: expr, expr: "$.context.risk < 50" }
                    executor: { kind: noop }
                  go_risky:
                    target: open
                    guards:
                      - { kind: expr, expr: "$.context.risk >= 50" }
                    executor: { kind: noop }
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    // risk=90 ⇒ only go_risky is reachable, even though the workflow's
    // top-level linkFilter is "all" (the state's "byGuards" wins).
    assert_eq!(s.link_rels(), vec!["go_risky"]);
}

/// **S-15. Composite guards (`all_of`, `any_of`, `not`).** "Tests
/// passed AND coverage didn't drop" is one logical condition; without
/// composition you'd need an intermediate state. The composite guard
/// kinds let the workflow author state the rule directly.
#[tokio::test]
async fn s15_composite_guards() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              tests_passed: true
              coverage_dropped: false
            states:
              s:
                transitions:
                  ship_when_all_clear:
                    target: shipped
                    guards:
                      - kind: all_of
                        guards:
                          - { kind: expr, expr: "$.context.tests_passed == true" }
                          - kind: not
                            guard: { kind: expr, expr: "$.context.coverage_dropped == true" }
                    executor: { kind: noop }
                  ship_when_any_emergency:
                    target: shipped
                    guards:
                      - kind: any_of
                        guards:
                          - { kind: expr, expr: "$.context.always_false == true" }
                          - { kind: expr, expr: "$.context.tests_passed == true" }
                    executor: { kind: noop }
              shipped:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));

    // all_of: tests passed AND coverage didn't drop → ship_when_all_clear works
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship_when_all_clear", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");

    // any_of: at least one passes → ship_when_any_emergency works
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship_when_any_emergency", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");
}

#[tokio::test]
async fn s15b_composite_guard_blocks_when_any_clause_fails() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              tests_passed: true
              coverage_dropped: true            # this should sink all_of
            states:
              s:
                transitions:
                  ship:
                    target: shipped
                    guards:
                      - kind: all_of
                        guards:
                          - { kind: expr, expr: "$.context.tests_passed == true" }
                          - kind: not
                            guard: { kind: expr, expr: "$.context.coverage_dropped == true" }
                    executor: { kind: noop }
              shipped:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship", json!({}), anon()).await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
}

/// **S-13. String + bool comparison in jsonpath.** "Continue only if the
/// last test run reported success." Without bool literals + string
/// equality, the only options were custom guards or proxying every
/// boolean as 1/0.
///
/// Fix: `eval_tiny_numeric_expr` now supports string literals
/// (`"foo"` / `'foo'`), bool literals (`true` / `false`), `null`, and
/// path-to-path / path-to-literal `==` / `!=`.
#[tokio::test]
async fn s13_jsonpath_string_and_bool_compare() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              ok: true
              status: "ready"
            states:
              s:
                transitions:
                  go_when_ok:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.context.ok == true" }
                      - { kind: expr, expr: "$.context.status == 'ready'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("go_when_ok", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");
}

#[tokio::test]
async fn s13b_jsonpath_path_to_path_compare() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              before: 5
              after: 7
            states:
              s:
                transitions:
                  improved:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.context.after > $.context.before" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("improved", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "completed");
}

/// **S-14. Transition auto-branches.** "Run tests; if pass go green,
/// if fail go red." Single submit, two outcomes — the branching is
/// declared, not procedurally chosen by the caller.
///
/// Fix: `branches: [{ when, target }]` on transitions, evaluated after
/// the executor's output mapping is applied. First match wins; falls
/// back to the declared `target`.
#[tokio::test]
async fn s14_transition_auto_branches() {
    struct ReturnsBool(bool);
    #[async_trait]
    impl Executor for ReturnsBool {
        async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult {
                output: json!({ "success": self.0 }),
                evidence: vec![],
            })
        }
    }

    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: red
            states:
              red:
                transitions:
                  run_tests:
                    target: red                # default fallback
                    executor: { kind: noop }
                    output:
                      passed: "$.output.success"
                    branches:
                      - when:   { kind: expr, expr: "$.context.passed == true" }
                        target: green
                      - when:   { kind: expr, expr: "$.context.passed == false" }
                        target: red
              green:
                terminal: true
    "#;

    // Tests pass → go green.
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(Arc::new(ReturnsBool(true)))));
    s.start("demo", json!({}), anon()).await;
    s.submit("run_tests", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "green");
    assert!(s
        .audit_event_types()
        .iter()
        .any(|t| t == "transition.branched"));

    // Tests fail → stay in red.
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(Arc::new(ReturnsBool(false)))));
    s.start("demo", json!({}), anon()).await;
    s.submit("run_tests", json!({}), anon()).await;
    assert_eq!(s.last()["workflow"]["state"], "red");
}

/// **S-06.** Output mapping reads from the broader scopes (workflow input,
/// arguments, context) — not just the executor's output. Without this you
/// can't pass a transition argument straight into context for later steps.
#[tokio::test]
async fn s06_output_mapping_reads_arguments_and_input() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            inputSchema:
              type: object
              properties: { project: { type: string } }
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    output:
                      caller_note: "$.arguments.note"
                      project:     "$.workflow.input.project"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({ "project": "cool-app" }), anon())
        .await;
    s.submit("go", json!({ "note": "shipping after lunch" }), anon())
        .await;
    assert_eq!(s.last()["context"]["caller_note"], "shipping after lunch");
    assert_eq!(s.last()["context"]["project"], "cool-app");
}

// ---------------------------------------------------------------------------
// S-17 — concat operator and string-comparison guards (starts_with / contains).
// The gateway used to require an executor round-trip to concatenate strings,
// and `expr` guards only did equality on strings. These tests pin the
// declarative replacements.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn s17_concat_in_output_mapping() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    output:
                      message:
                        concat:
                          - "branch="
                          - "$.arguments.branch"
                          - " (pr "
                          - "$.arguments.pr"
                          - ")"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({ "branch": "feat/login", "pr": 42 }), anon())
        .await;
    assert_eq!(s.last()["context"]["message"], "branch=feat/login (pr 42)");
}

#[tokio::test]
async fn s17b_concat_with_null_element() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    output:
                      message:
                        concat:
                          - "before="
                          - "$.arguments.missing"
                          - "=after"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;
    assert_eq!(s.last()["context"]["message"], "before=null=after");
}

#[tokio::test]
async fn s17c_starts_with_guard_passes() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  ship:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.branch starts_with 'feat/'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship", json!({ "branch": "feat/login" }), anon())
        .await;
    assert_eq!(s.last()["result"]["status"], "completed");
}

#[tokio::test]
async fn s17d_starts_with_guard_rejects() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  ship:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.branch starts_with 'feat/'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship", json!({ "branch": "fix/leak" }), anon())
        .await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
}

#[tokio::test]
async fn s17e_contains_guard_passes() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  retry:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.error contains 'timeout'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit(
        "retry",
        json!({ "error": "upstream connection timeout after 30s" }),
        anon(),
    )
    .await;
    assert_eq!(s.last()["result"]["status"], "completed");
}

#[tokio::test]
async fn s17f_contains_guard_rejects() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  retry:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.error contains 'timeout'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("retry", json!({ "error": "not found" }), anon())
        .await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
}
