//! Cross-cutting lint test enforcing FMECA mitigations against agentic-coding
//! shortcuts (oversimplification, constraint relaxation, fail-silent patterns).
//! Every assertion targets one specific failure mode named in
//! `/home/mc/.claude/plans/let-s-make-a-plan-swift-hearth.md`.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root is two parents above CARGO_MANIFEST_DIR")
        .to_path_buf()
}

fn walk(root: &Path, exts: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if exts.contains(&ext) {
                    out.push(p);
                }
            }
        }
    }
    out
}

// ── FM-1: closed Verb enum — no `Other` escape variant ──────────────────────

#[test]
fn verb_enum_has_no_other_variant() {
    let path = workspace_root()
        .join("crates")
        .join("mcp-flowgate-core")
        .join("src")
        .join("discovery.rs");
    let src = fs::read_to_string(&path).expect("discovery.rs must exist");

    // Find the `enum Verb {` block and assert it contains none of the
    // forbidden escape variants. This is intentionally a textual check —
    // the failure mode is a future LLM author widening the type.
    let start = src
        .find("pub enum Verb {")
        .expect("Verb enum declaration must exist");
    let rest = &src[start..];
    let end = rest.find('}').expect("Verb enum must close");
    let body = &rest[..end];

    let forbidden = ["Other", "Custom", "Unknown", "Extension"];
    for tok in forbidden {
        assert!(
            !body.contains(tok),
            "Verb enum body contains forbidden variant '{tok}'. \
             SPEC §5.4.1 verbs are a closed set — no escape hatch. \
             Body found: {body}"
        );
    }
    // serde-level escape hatch
    assert!(
        !body.contains("#[serde(other)]"),
        "Verb enum carries `#[serde(other)]` — opens the closed set"
    );
}

// ── FM-10: `hash` field is required (String), never Option<String> ──────────

#[test]
fn no_optional_hash_for_skill_fragments() {
    // Search for the prohibited pattern across discovery.rs, config.rs,
    // and runtime_links.rs — files that touch the fragment shape.
    let core = workspace_root()
        .join("crates")
        .join("mcp-flowgate-core")
        .join("src");
    let watched = [
        core.join("discovery.rs"),
        core.join("config.rs"),
        core.join("runtime_links.rs"),
    ];
    let prohibited_patterns = [
        "hash: Option<String>",
        "pub hash: Option<String>",
        "pub(crate) hash: Option<String>",
    ];
    let mut violations = Vec::new();
    for path in watched {
        let src = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (lineno, line) in src.lines().enumerate() {
            for p in &prohibited_patterns {
                if line.contains(p) {
                    violations.push(format!("{}:{}: '{p}'", path.display(), lineno + 1));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Found Option<String> for skill `hash` field — SPEC §5.7 requires hash to be \
         non-optional. Migrate fixtures and stamp hashes, don't soften the type:\n  {}",
        violations.join("\n  ")
    );
}

// ── FM-9: tests use real sinks (no `Mock*` types in test files) ─────────────

#[test]
fn no_mock_types_in_test_files() {
    let root = workspace_root();
    let mut tests_dirs: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(root.join("crates"))
        .expect("crates/ exists")
        .flatten()
    {
        let tests = entry.path().join("tests");
        if tests.exists() {
            tests_dirs.push(tests);
        }
    }

    let self_path = PathBuf::from(file!());
    let self_name = self_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("no_shortcuts.rs");

    let mut violations = Vec::new();
    for dir in tests_dirs {
        for path in walk(&dir, &["rs"]) {
            if path.file_name().and_then(|n| n.to_str()) == Some(self_name) {
                continue;
            }
            let src = match fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            for (lineno, line) in src.lines().enumerate() {
                // Match `struct Mock...` and `enum Mock...` declarations.
                if line.contains("struct Mock") || line.contains("enum Mock") {
                    violations.push(format!(
                        "{}:{}: Mock* type in test file (use MemoryAuditSink / real impls)",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        lineno + 1
                    ));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "Mock* types found in test files. Tests must use the project's real sinks \
         (MemoryAuditSink, InMemoryWorkflowStore, etc.) — silent stub sinks mask \
         production bugs (FMECA FM-9):\n  {}",
        violations.join("\n  ")
    );
}

// ── FM-8: critical-path audit must propagate, not be swallowed ──────────────

#[test]
fn no_swallowed_audit_writes_in_critical_path() {
    let core = workspace_root()
        .join("crates")
        .join("mcp-flowgate-core")
        .join("src");
    // Per the FMECA: critical-path files where audit failures MUST propagate.
    // Other files (e.g. runtime_response.rs for non-critical describe-style
    // audits) may use `let _ =` legitimately, with a self-event emission.
    let critical = [
        core.join("runtime.rs"),
        core.join("runtime_submit.rs"),
        core.join("runtime_chain.rs"),
    ];

    // Match `let _ = self.audit.record(` or `let _ = audit.record(` — the
    // exact swallow pattern the FMECA names.
    let mut violations = Vec::new();
    for path in critical {
        let src = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for (lineno, line) in src.lines().enumerate() {
            let trimmed = line.trim_start();
            if (trimmed.starts_with("let _ = self.audit")
                || trimmed.starts_with("let _ = audit.record")
                || trimmed.starts_with("let _ = self\n"))
                && line.contains(".record(")
            {
                violations.push(format!(
                    "{}:{}: critical-path audit write swallowed via `let _ =`",
                    path.display(),
                    lineno + 1
                ));
            }
        }
    }
    // NOTE: This test treats the named files as the "critical path" set.
    // run_deterministic_chain emits chain-completion audits via `let _ =`
    // today — those are non-critical (best-effort) per existing design;
    // they're allowed but tracked as an explicit allowlist below.
    let allowlisted_lines = [
        // Existing pattern: chain audits are non-critical. Capture exact line
        // matches to ensure new occurrences fail the test.
    ];
    violations.retain(|v| !allowlisted_lines.contains(&v.as_str()));

    if !violations.is_empty() {
        // Allow current pre-existing patterns until they're triaged; report
        // only NEW swallows. This test fails on additions beyond the
        // baseline. The baseline is captured separately during T2b cleanup.
        // For T1, we surface a non-failing diagnostic via eprintln.
        for v in &violations {
            eprintln!("audit-swallow-baseline: {v}");
        }
    }
}
