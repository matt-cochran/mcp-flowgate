//! Per-repo manifest loading (SPEC §9, capability/orchestrator composition design §9).
//!
//! Each resource repo ships a `flowgate.repo.yaml` at its root declaring
//! a `namespace`, a `version`, and a `layout` of directories where
//! capabilities, orchestrators, skills, scripts, and connections live.
//!
//! Gateway configs reference repos via a top-level `repos:` array. At
//! config-load, every YAML under each repo's layout directories is loaded
//! and its top-level `workflows:` / `skills:` / `scripts:` / `connections:`
//! entries are merged into the gateway registry, with every key prefixed
//! `<namespace>/<id>`. See `config::load_repos` for the integration site.
//!
//! This module owns the manifest schema + loader. Namespace-prefixing
//! lives in `config.rs` where it can reuse `deep_merge`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;

/// The expected value of the manifest's `schema` field. Loaders refuse any
/// manifest whose `schema` is not exactly this string — forward-incompatible
/// schema bumps will introduce new constants (e.g. `flowgate.repo/v2`) so
/// older gateways can refuse rather than silently mis-parse.
pub const REPO_MANIFEST_SCHEMA_V1: &str = "flowgate.repo/v1";

/// Parsed `flowgate.repo.yaml` manifest. See `schemas/flowgate-repo.schema.json`
/// for the canonical schema.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoManifest {
    /// MUST equal [`REPO_MANIFEST_SCHEMA_V1`].
    pub schema: String,
    /// Human-readable repo identifier; lowercase-kebab.
    pub name: String,
    /// Single-segment prefix applied to every definitionId loaded from this
    /// repo. Two repos declaring the same `namespace` fail at config-load.
    pub namespace: String,
    /// Repo version, semver-shaped by convention. Surfaced via
    /// `gateway.describe`.
    pub version: String,
    /// Free-form description; surfaced via `gateway.describe`.
    #[serde(default)]
    pub description: Option<String>,
    /// Per-tier directory locations. Each field defaults to the directory
    /// name matching the field name.
    #[serde(default)]
    pub layout: RepoLayout,
}

/// Layout of resource directories within a repo. All fields are optional;
/// defaults match the directory names exactly (e.g. `capabilities/`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoLayout {
    #[serde(default = "default_capabilities_dir")]
    pub capabilities: String,
    #[serde(default = "default_orchestrators_dir")]
    pub orchestrators: String,
    #[serde(default = "default_skills_dir")]
    pub skills: String,
    #[serde(default = "default_scripts_dir")]
    pub scripts: String,
    #[serde(default = "default_connections_dir")]
    pub connections: String,
}

impl Default for RepoLayout {
    fn default() -> Self {
        Self {
            capabilities: default_capabilities_dir(),
            orchestrators: default_orchestrators_dir(),
            skills: default_skills_dir(),
            scripts: default_scripts_dir(),
            connections: default_connections_dir(),
        }
    }
}

fn default_capabilities_dir() -> String { "capabilities".to_string() }
fn default_orchestrators_dir() -> String { "orchestrators".to_string() }
fn default_skills_dir() -> String { "skills".to_string() }
fn default_scripts_dir() -> String { "scripts".to_string() }
fn default_connections_dir() -> String { "connections".to_string() }

/// Load and validate a `flowgate.repo.yaml` from the given repo root.
/// The path argument is the repo directory; the manifest is read from
/// `<root>/flowgate.repo.yaml`.
///
/// Errors at:
/// - missing manifest file
/// - YAML parse failure
/// - `schema` not equal to [`REPO_MANIFEST_SCHEMA_V1`]
/// - unknown fields (manifest uses `deny_unknown_fields`)
pub fn load_manifest(repo_root: &Path) -> anyhow::Result<RepoManifest> {
    let manifest_path: PathBuf = repo_root.join("flowgate.repo.yaml");
    let text = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("reading repo manifest {}", manifest_path.display()))?;
    let manifest: RepoManifest = serde_yaml::from_str(&text)
        .with_context(|| format!("parsing repo manifest {}", manifest_path.display()))?;
    if manifest.schema != REPO_MANIFEST_SCHEMA_V1 {
        bail!(
            "repo manifest {} declares schema `{}`; expected `{}`",
            manifest_path.display(),
            manifest.schema,
            REPO_MANIFEST_SCHEMA_V1
        );
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("flowgate.repo.yaml");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn load_manifest_accepts_minimal_well_formed_manifest() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: flowgate.repo/v1\nname: swe-core\nnamespace: swe\nversion: 0.6.0\n",
        );
        let m = load_manifest(td.path()).expect("manifest should load");
        assert_eq!(m.schema, REPO_MANIFEST_SCHEMA_V1);
        assert_eq!(m.name, "swe-core");
        assert_eq!(m.namespace, "swe");
        assert_eq!(m.version, "0.6.0");
        assert_eq!(m.layout.capabilities, "capabilities");
        assert_eq!(m.layout.orchestrators, "orchestrators");
    }

    #[test]
    fn load_manifest_rejects_wrong_schema_constant() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: flowgate.repo/v2\nname: swe-core\nnamespace: swe\nversion: 0.6.0\n",
        );
        let err = load_manifest(td.path()).expect_err("schema mismatch should error");
        let msg = format!("{:#}", err);
        assert!(msg.contains("flowgate.repo/v1"), "error should mention v1: {msg}");
        assert!(msg.contains("flowgate.repo/v2"), "error should mention actual: {msg}");
    }

    #[test]
    fn load_manifest_rejects_unknown_top_level_field() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: flowgate.repo/v1\nname: swe-core\nnamespace: swe\nversion: 0.6.0\nbogus: hi\n",
        );
        load_manifest(td.path()).expect_err("unknown field should error");
    }

    #[test]
    fn load_manifest_accepts_partial_layout_with_defaults_for_rest() {
        let td = TempDir::new().unwrap();
        write_manifest(
            td.path(),
            "schema: flowgate.repo/v1\nname: swe-core\nnamespace: swe\nversion: 0.6.0\nlayout:\n  capabilities: caps\n",
        );
        let m = load_manifest(td.path()).expect("partial layout should load");
        assert_eq!(m.layout.capabilities, "caps");
        assert_eq!(m.layout.orchestrators, "orchestrators");
        assert_eq!(m.layout.skills, "skills");
    }

    #[test]
    fn load_manifest_errors_when_file_missing() {
        let td = TempDir::new().unwrap();
        let err = load_manifest(td.path()).expect_err("missing file should error");
        let msg = format!("{:#}", err);
        assert!(msg.contains("flowgate.repo.yaml"), "error should mention file: {msg}");
    }
}
