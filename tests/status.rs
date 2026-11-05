use std::{fs, process::Command};

#[test]
fn status_reports_quota_and_never_refreshes() {
    let dir = tempfile::tempdir_in("target").unwrap();
    let config = dir.path().join("config.toml");
    let marker = dir.path().join("refreshed");
    let text = format!(
        r#"
fallback = 'fallback'
state_file = 'unused-state'
log_file = 'unused-log'
[[backends]]
name = 'test'
roles = ['implement', 'review']
guard = ['true']
[backends.commands]
medium = ['fake-agent', '{{prompt}}']
high = ['fake-agent', '{{prompt}}']
[backends.quota]
command = ['printf', 'not-json']
parser = 'remaining_json'
refresh = ['touch', '{}']

[[backends]]
name = 'fallback'
roles = []
guard = ['true']
allow_unknown_quota = true
[backends.commands]
medium = ['fake-agent', '{{prompt}}']
high = ['fake-agent', '{{prompt}}']
"#,
        marker.display()
    );
    fs::write(&config, &text).unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_rota"))
            .env("ROTA_CONFIG", &config)
            .args(["status", "--json"])
            .output()
            .unwrap()
    };
    let output = run();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        json[0]["implement"]
            .as_str()
            .unwrap()
            .contains("quota unreadable")
    );
    assert!(!marker.exists());
    assert_eq!(json[1]["implement"], "fallback available");
    assert_eq!(json[1]["review"], "not allowed for role");
    fs::write(&config, text.replace("not-json", "{\"week\":45}")).unwrap();
    let output = run();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json[0]["implement"], "available");
    assert_eq!(json[0]["reading"]["remaining"]["week"], 45.0);
    fs::write(
        &config,
        text.replace("guard = ['true']", "guard = ['false']"),
    )
    .unwrap();
    let output = run();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json[0]["implement"].as_str().unwrap().contains("guard"));
}

#[test]
fn exhausted_agy_names_window_and_reset() {
    let dir = tempfile::tempdir_in("target").unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let config = dir.join("config.toml");
    let prompt = dir.join("prompt");
    fs::write(&prompt, "task").unwrap();
    let fixture = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/agy-exhausted.tsv"
    );
    fs::write(
        &config,
        format!(
            r#"
state_file = 'unused-state'
log_file = 'unused-log'
[[backends]]
name = 'agy'
roles = ['implement', 'review']
guard = ['true']
[backends.commands]
medium = ['fake-agent', '{{prompt}}']
high = ['fake-agent', '{{prompt}}']
[backends.quota]
command = ['cat', '{fixture}']
parser = 'agy'
[backends.limits.implement.min_remaining]
five_hour = 10
weekly = 10
"#
        ),
    )
    .unwrap();
    let rota = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rota"))
            .env("ROTA_CONFIG", &config)
            .current_dir(&dir)
            .args(args)
            .output()
            .unwrap()
    };
    let expected = "quota limit: five_hour_left=0%, weekly_left=0% (resets 2030-01-08T14:00:00Z)";
    let output = rota(&["status", "--json"]);
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json[0]["implement"], expected);
    assert_eq!(json[0]["review"], "available");
    assert_eq!(
        json[0]["reading"]["resets"]["weekly"],
        "2030-01-08T14:00:00Z"
    );
    let text = String::from_utf8(rota(&["status"]).stdout).unwrap();
    assert!(
        text.contains(&format!("implement={expected} review=available")),
        "{text}"
    );
    assert!(!text.contains("quota unreadable"), "{text}");
    let output = rota(&[
        "launch",
        "--dry-run",
        "--only",
        "agy",
        prompt.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8(output.stderr).unwrap().contains(expected));
}
