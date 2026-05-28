//! SPEC §22 — the `script` executor kind. Materializes a curated, hash-
//! pinned script body to a per-invocation temp file and execs it.
//!
//! Invocation shape in YAML:
//! ```yaml
//! transitions:
//!   build:
//!     executor:
//!       kind: script
//!       subject: build.cargo.release            # required
//!       args: ["--features=integration"]        # optional, templated
//!       workingDirectory: /path/to/repo         # optional
//!       env: { CI: "true" }                     # optional
//!       treatNonZeroAsFailure: true             # optional (default true)
//! ```
//!
//! Resolution chain:
//!
//! 1. Look up `subject` in the instance's `definition._scriptsLibrary` —
//!    stamped at config-load time by `stamp_scripts_library` (SPEC §22 / N).
//! 2. Write body to a temp file (`tempfile::NamedTempFile`), chmod 0700.
//! 3. Render `args` against `{$.arguments, $.context, $.input}` scopes
//!    (same templating as the cli executor).
//! 4. Exec: if body starts with `#!`, invoke the path directly (kernel
//!    honors shebang); otherwise default to `bash <path>`.
//! 5. Capture stdout/stderr/exit. Stdout auto-parses as JSON when valid.
//! 6. Emit `script_output` Evidence with body hash for audit replay.
//!
//! The temp file is dropped (and deleted) when execution returns. The
//! workflow's transition record carries the script's `subject` + `hash`
//! via the executor output JSON, so a future replay can pull the same
//! script body out of cold storage by hash.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use async_trait::async_trait;
use mcp_flowgate_core::error::ExecutorError;
use mcp_flowgate_core::mapping::read_in_scopes;
use mcp_flowgate_core::model::{Evidence, ExecuteRequest, ExecuteResult};
use mcp_flowgate_core::ports::Executor;
use serde_json::{json, Value};
use tempfile::NamedTempFile;
use tokio::process::Command;
use uuid::Uuid;

pub struct ScriptExecutor;

impl ScriptExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ScriptExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Executor for ScriptExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;

        // Subject lookup is required — there's no "inline body" path on
        // the executor itself (that's what the cli executor is for).
        let subject = cfg
            .get("subject")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent(
                    "INVALID_SCRIPT_INVOCATION: script executor requires `subject` \
                     (the curated script's dotted name from the top-level `scripts:` block)"
                        .into(),
                )
            })?;

        // SPEC §22 + N — body lives in the instance's stamped library, not
        // in the live config. This is the §8.2 invariant: an in-flight
        // instance sees the body that existed at workflow.start.
        let lib_pointer = format!(
            "/_scriptsLibrary/{}",
            subject.replace('~', "~0").replace('/', "~1")
        );
        let entry = request
            .workflow
            .definition
            .pointer(&lib_pointer)
            .ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "SCRIPT_NOT_IN_SNAPSHOT: script '{subject}' not found in this workflow's \
                     `_scriptsLibrary`. Likely cause: the script subject was not collected by \
                     `collect_referenced_script_subjects` at config-load time. Verify the \
                     `executor: {{ kind: script, subject: {subject} }}` reference is on a \
                     transition (not a free-form field) so stamp_scripts_library can find it."
                ))
            })?;
        let body = entry
            .get("body")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ExecutorError::Permanent(format!(
                    "SCRIPT_NOT_IN_SNAPSHOT: script '{subject}' entry has no `body`. \
                     The snapshot is malformed (likely a bug — file an issue)."
                ))
            })?;
        let hash = entry
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or("sha256:0000000000000000000000000000000000000000000000000000000000000000")
            .to_string();

        // Materialize body → temp file → chmod 0700 → exec.
        //
        // `into_temp_path()` is critical: it drops the open `File` handle
        // while keeping the path (and a Drop guard that deletes the file
        // when the TempPath goes out of scope). Without it, the kernel
        // refuses to exec a file with a writable open handle ("Text file
        // busy", ETXTBSY) on Linux.
        let temp = NamedTempFile::new().map_err(|e| {
            ExecutorError::Connection(format!("failed to create temp file for script: {e}"))
        })?;
        std::fs::write(temp.path(), body).map_err(|e| {
            ExecutorError::Connection(format!("failed to write script body: {e}"))
        })?;
        let temp_path = temp.into_temp_path();
        #[cfg(unix)]
        {
            std::fs::set_permissions(
                &temp_path,
                std::fs::Permissions::from_mode(0o700),
            )
            .map_err(|e| {
                ExecutorError::Connection(format!("failed to chmod 0700 script file: {e}"))
            })?;
        }

        // Render args from {$.arguments, $.context, $.input} scopes.
        let raw_args = cfg
            .get("args")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let rendered_args: Vec<String> =
            raw_args.iter().map(|a| render_arg(a, &request)).collect();

        // Honor shebang if body starts with `#!`. Otherwise default to bash.
        // Both paths preserve argv semantics; we don't pipe via stdin.
        let body_has_shebang = body.starts_with("#!");
        let (program, mut cmd) = if body_has_shebang {
            // Execute the file path directly — kernel honors the shebang.
            (
                temp_path.to_string_lossy().into_owned(),
                Command::new(&*temp_path),
            )
        } else {
            // No shebang: invoke through bash. Operators on shells without
            // bash can declare a shebang in their script bodies.
            let mut c = Command::new("bash");
            c.arg(&*temp_path);
            ("bash".to_string(), c)
        };
        for arg in &rendered_args {
            cmd.arg(arg);
        }

        // Optional working directory + env from executor config.
        if let Some(wd) = cfg.get("workingDirectory").and_then(Value::as_str) {
            cmd.current_dir(wd);
        }
        if let Some(extra) = cfg.get("env").and_then(Value::as_object) {
            for (k, v) in extra {
                if let Some(s) = v.as_str() {
                    cmd.env(k, s);
                }
            }
        }
        if let Some(key) = &request.idempotency_key {
            cmd.env("IDEMPOTENCY_KEY", key);
        }
        // Expose hash + subject to the script itself so it can self-identify
        // in logs / metrics without having to parse argv.
        cmd.env("FLOWGATE_SCRIPT_SUBJECT", subject);
        cmd.env("FLOWGATE_SCRIPT_HASH", &hash);

        // ETXTBSY retry loop. Linux returns ETXTBSY ("Text file busy",
        // errno 26 on Linux/macOS/BSD — POSIX defines the symbol but
        // not a portable numeric value, which is why we cfg-guard the
        // constant) when execve targets an inode that any process holds
        // open for writing. Even though our std::fs::write closed its
        // fd before we get here, a concurrent thread's Command::spawn()
        // can briefly inherit our fd during its own fork() (the window
        // between fork and execve, before the kernel runs the CLOEXEC
        // sweep). The race is well-known on multi-threaded test runs;
        // the standard mitigation is a small retry with backoff — once
        // the other child execves or exits, our fd reference vanishes
        // and the next attempt succeeds.
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
        const ETXTBSY_ERRNO: i32 = 26;
        let output = {
            let mut attempt = 0u32;
            loop {
                match cmd.output().await {
                    Ok(out) => break out,
                    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
                    Err(e) if e.raw_os_error() == Some(ETXTBSY_ERRNO) && attempt < 5 => {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(10 * (1 << attempt)))
                            .await;
                        continue;
                    }
                    Err(e) => {
                        return Err(ExecutorError::Connection(format!(
                            "script spawn failed ({program}): {e}"
                        )));
                    }
                }
            }
        };

        let stdout_str = String::from_utf8_lossy(&output.stdout).to_string();
        let parsed_json: Value =
            serde_json::from_str(stdout_str.trim()).unwrap_or(Value::Null);

        // Audit-grade output: includes scriptSubject + scriptHash so a
        // transition record uniquely identifies what code ran, and a
        // future replay can pull the body by hash from cold storage.
        let result = json!({
            "exitCode":      output.status.code(),
            "success":       output.status.success(),
            "stdout":        stdout_str,
            "stderr":        String::from_utf8_lossy(&output.stderr).to_string(),
            "json":          parsed_json,
            "scriptSubject": subject,
            "scriptHash":    hash,
        });

        let treat_nonzero_as_failure = cfg
            .get("treatNonZeroAsFailure")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if treat_nonzero_as_failure && !output.status.success() {
            return Err(ExecutorError::Permanent(format!(
                "script '{subject}' exited with code {:?}",
                output.status.code()
            )));
        }

        Ok(ExecuteResult {
            output: result,
            evidence: vec![Evidence {
                kind: "script_output".to_string(),
                id: Uuid::new_v4().to_string(),
                uri: None,
                summary: Some(format!("Executed script '{subject}'")),
                digest: Some(hash),
                confidence: None,
            }],
            child_workflow_id: None,
        })
    }
}

/// Same templating as cli executor's `render_arg`: literal strings pass
/// through; `$.context.x` / `$.arguments.x` / `$.input.x` paths resolve
/// against the request's blackboard / args / input scopes.
fn render_arg(value: &Value, request: &ExecuteRequest) -> String {
    let Some(raw) = value.as_str() else {
        return value.to_string();
    };
    if let Some(v) = read_in_scopes(
        raw,
        &request.arguments,
        &request.workflow.context,
        &request.workflow.input,
        None,
    ) {
        return match v {
            Value::String(s) => s,
            other => other.to_string(),
        };
    }
    raw.to_string()
}

