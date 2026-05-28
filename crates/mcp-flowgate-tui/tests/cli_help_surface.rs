use std::process::Command;

fn binary() -> String {
    env!("CARGO_BIN_EXE_flowgate").to_string()
}

#[test]
fn set_provider_keys_is_listed_in_help() {
    let out = Command::new(binary()).arg("--help").output().expect("run --help");
    assert!(out.status.success(), "--help failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("set-provider-keys"),
        "expected set-provider-keys in --help, got:\n{stdout}"
    );
}

#[test]
fn set_provider_keys_help_mentions_flags() {
    let out = Command::new(binary())
        .args(["set-provider-keys", "--help"])
        .output()
        .expect("run set-provider-keys --help");
    assert!(out.status.success(), "subcommand --help failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--provider") && stdout.contains("--list"));
}

#[test]
fn set_provider_keys_path_prints_resolved_path() {
    let dir = tempfile::tempdir().unwrap();
    let want = dir.path().join("custom.env");
    let out = Command::new(binary())
        .env("FLOWGATE_PROVIDER_KEYS_FILE", &want)
        .args(["set-provider-keys", "--path"])
        .output()
        .expect("run set-provider-keys --path");
    assert!(out.status.success(), "--path failed: {:?}", out);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), want.to_string_lossy());
}
