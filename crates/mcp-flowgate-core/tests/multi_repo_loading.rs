//! SPEC §9 — multi-repo loading. Acceptance milestone M1 + V19–V23
//! accepts/rejects pairs.
//!
//! Fixtures live under `tests/fixtures/repos/`:
//!   - `swe-core/`        : namespace `swe`, ships `cap.plan.vet` +
//!                          `flow.add-feature` (the latter references the
//!                          capability via an UNPREFIXED `cap.plan.vet`,
//!                          which `load_repo` rewrites to `swe/cap.plan.vet`).
//!   - `quality-core/`    : namespace `quality`, ships its own `cap.plan.vet`
//!                          — proves two namespaces can share an id without
//!                          collision (M1).
//!   - `dupe-namespace-{a,b}/` : both declare `namespace: dupe` — used to
//!                          assert V20 fires.
//!
//! Tests construct host gateway-config YAML on the fly (via tempfile) so
//! the test owns the `repos:` declarations and any host-level overrides.
//! Repo paths in the host config resolve relative to the host file's
//! directory — we point them at the on-disk fixtures via absolute paths
//! to keep the tests location-agnostic.

use std::path::PathBuf;

use mcp_flowgate_core::config::load_resolved_with_repos;
use serde_json::Value;
use tempfile::TempDir;

/// Absolute path to `tests/fixtures/repos`.
fn fixtures_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push("repos");
    p
}

/// Write `body` to `<tempdir>/flowgate.yaml` and return the path.
fn write_host(td: &TempDir, body: &str) -> PathBuf {
    let p = td.path().join("flowgate.yaml");
    std::fs::write(&p, body).unwrap();
    p
}

// ---------- M1 acceptance ----------

#[test]
fn two_repos_with_distinct_namespaces_load_both_capabilities() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
  - path: "{quality}"
"#,
        swe = fixtures_root().join("swe-core").display(),
        quality = fixtures_root().join("quality-core").display(),
    );
    let path = write_host(&td, &host);
    let (config, diagnostics) = load_resolved_with_repos(&path)
        .expect("two-repo load should succeed");
    assert!(diagnostics.is_empty(), "no soft diagnostics expected: {diagnostics:?}");

    let workflows = config
        .pointer("/workflows")
        .and_then(Value::as_object)
        .expect("workflows present");
    assert!(
        workflows.contains_key("swe/cap.plan.vet"),
        "expected swe-prefixed key; got {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
    assert!(
        workflows.contains_key("quality/cap.plan.vet"),
        "expected quality-prefixed key; got {:?}",
        workflows.keys().collect::<Vec<_>>()
    );
    assert!(
        workflows.contains_key("swe/flow.add-feature"),
        "orchestrator from swe-core should load"
    );

    // The unprefixed `definitionId: cap.plan.vet` reference inside
    // `swe/flow.add-feature` should be rewritten to `swe/cap.plan.vet`.
    let resolved_ref = config
        .pointer("/workflows/swe~1flow.add-feature/states/planning/transitions/plan_drafted/executor/definitionId")
        .and_then(Value::as_str)
        .expect("resolved ref present");
    assert_eq!(resolved_ref, "swe/cap.plan.vet");
}

// ---------- V19 — repo manifest schema ----------

#[test]
fn v19_accepts_well_formed_manifest() {
    // Implicitly covered by M1, but assert explicitly so the rule is
    // discoverable by name from the validation-parity script (PR3).
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let (_config, _diagnostics) =
        load_resolved_with_repos(&path).expect("well-formed manifest loads");
}

#[test]
fn v19_rejects_manifest_with_wrong_schema_constant() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("bad-schema-repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("flowgate.repo.yaml"),
        "schema: flowgate.repo/v999\nname: bad\nnamespace: bad\nversion: 0.1.0\n",
    )
    .unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        repo_dir.display()
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("wrong schema must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("flowgate.repo/v1"), "msg: {msg}");
}

// ---------- V20 — two repos sharing a namespace ----------

#[test]
fn v20_accepts_distinct_namespaces() {
    // Covered by M1, but kept as a named test to satisfy parity scanner.
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
  - path: "{quality}"
"#,
        swe = fixtures_root().join("swe-core").display(),
        quality = fixtures_root().join("quality-core").display(),
    );
    let path = write_host(&td, &host);
    load_resolved_with_repos(&path).expect("distinct namespaces accepted");
}

#[test]
fn v20_rejects_two_repos_with_same_namespace() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{a}"
  - path: "{b}"
"#,
        a = fixtures_root().join("dupe-namespace-a").display(),
        b = fixtures_root().join("dupe-namespace-b").display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("namespace collision must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("DUPLICATE_REPO_NAMESPACE"), "msg: {msg}");
    assert!(msg.contains("dupe"), "should name the namespace: {msg}");
}

// ---------- V21 — duplicate ids inside one repo ----------

#[test]
fn v21_accepts_single_id_per_repo() {
    let td = TempDir::new().unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    load_resolved_with_repos(&path).expect("unique ids per repo");
}

#[test]
fn v21_rejects_duplicate_definition_within_one_repo() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("dup-defs-repo");
    std::fs::create_dir_all(repo_dir.join("capabilities")).unwrap();
    std::fs::write(
        repo_dir.join("flowgate.repo.yaml"),
        "schema: flowgate.repo/v1\nname: dup\nnamespace: dup\nversion: 0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("capabilities/a.yaml"),
        "workflows:\n  cap.collide:\n    title: A\n",
    )
    .unwrap();
    std::fs::write(
        repo_dir.join("capabilities/b.yaml"),
        "workflows:\n  cap.collide:\n    title: B\n",
    )
    .unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        repo_dir.display()
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("duplicate id must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("DUPLICATE_REPO_DEF"), "msg: {msg}");
    assert!(msg.contains("dup/cap.collide"), "msg should name id: {msg}");
}

// ---------- V22 — unprefixed cross-repo (unresolved) ref ----------

#[test]
fn v22_accepts_unprefixed_ref_that_resolves_in_current_namespace() {
    // swe/flow.add-feature references `cap.plan.vet` (unprefixed). Repo
    // loading rewrites it to `swe/cap.plan.vet`, which IS loaded. So it
    // resolves. This is the only positive test we need — the rewriting
    // is the mechanism that makes the positive case work.
    let td = TempDir::new().unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    load_resolved_with_repos(&path).expect("intra-namespace ref resolves");
}

#[test]
fn v22_rejects_workflow_ref_that_does_not_resolve() {
    let td = TempDir::new().unwrap();
    let repo_dir = td.path().join("unresolved-ref-repo");
    std::fs::create_dir_all(repo_dir.join("orchestrators")).unwrap();
    std::fs::write(
        repo_dir.join("flowgate.repo.yaml"),
        "schema: flowgate.repo/v1\nname: ur\nnamespace: ur\nversion: 0.1.0\n",
    )
    .unwrap();
    // References cap.missing — never defined anywhere.
    std::fs::write(
        repo_dir.join("orchestrators/flow.broken.yaml"),
        r#"
workflows:
  flow.broken:
    initial: s
    states:
      s:
        transitions:
          t:
            target: done
            executor:
              kind: workflow
              definitionId: cap.missing
      done:
        terminal: true
"#,
    )
    .unwrap();
    let host = format!(
        "version: \"1.0.0\"\nrepos:\n  - path: \"{}\"\n",
        repo_dir.display()
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("unresolved ref must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("UNRESOLVED_WORKFLOW_REF"), "msg: {msg}");
    // After namespace-prefixing the unprefixed ref `cap.missing` becomes
    // `ur/cap.missing` — that's the name V22 reports.
    assert!(msg.contains("ur/cap.missing"), "msg should name the unresolved id: {msg}");
}

// ---------- V23 — anonymous shadowing via host include ----------

#[test]
fn v23_accepts_explicit_override_of_repo_provided_id() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
overrides:
  - swe/cap.plan.vet
workflows:
  swe/cap.plan.vet:
    title: Operator-customized vet
    initial: ready
    states:
      ready:
        terminal: true
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let (config, _diagnostics) = load_resolved_with_repos(&path)
        .expect("explicit override should be accepted");
    // Host wins on the explicitly declared override.
    let title = config
        .pointer("/workflows/swe~1cap.plan.vet/title")
        .and_then(Value::as_str)
        .unwrap();
    assert_eq!(title, "Operator-customized vet");
}

#[test]
fn v23_rejects_anonymous_shadowing_without_overrides_declaration() {
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
workflows:
  swe/cap.plan.vet:
    title: Silent shadow attempt
    initial: ready
    states:
      ready:
        terminal: true
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("anonymous shadow must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("ANONYMOUS_OVERRIDE"), "msg: {msg}");
    assert!(msg.contains("swe/cap.plan.vet"), "msg should name the id: {msg}");
}

#[test]
fn v23_rejects_stale_override_with_no_collision() {
    // An `overrides:` entry that doesn't actually shadow a repo-provided
    // id is almost certainly an author mistake (renamed id, deleted repo).
    let td = TempDir::new().unwrap();
    let host = format!(
        r#"
version: "1.0.0"
repos:
  - path: "{swe}"
overrides:
  - swe/cap.does-not-exist
"#,
        swe = fixtures_root().join("swe-core").display(),
    );
    let path = write_host(&td, &host);
    let err = load_resolved_with_repos(&path).expect_err("stale override must error");
    let msg = format!("{:#}", err);
    assert!(msg.contains("STALE_OVERRIDE"), "msg: {msg}");
    assert!(msg.contains("swe/cap.does-not-exist"), "msg: {msg}");
}
