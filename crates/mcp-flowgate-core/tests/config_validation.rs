//! Config validation tests — version field requirements, etc.

use mcp_flowgate_core::config;

#[test]
fn valid_config_with_version_field() {
    let yaml = r#"
version: "1.0.0"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
"#;
    let result = config::resolve_str(yaml);
    assert!(result.is_ok(), "config with version field should parse OK");
}

#[test]
fn config_without_version_field_still_parses() {
    // The resolver doesn't require version; the `check` subcommand does.
    // This test verifies that resolve_str doesn't reject it — the
    // requirement is at the binary layer.
    let yaml = r#"
proxy:
  expose:
    - name: echo
      executor: { kind: noop }
"#;
    let result = config::resolve_str(yaml);
    assert!(
        result.is_ok(),
        "config without version should still resolve"
    );
}
