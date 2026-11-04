use std::{fs, process::Command};

#[test]
fn status_reports_quota_and_never_refreshes() {
    let dir = tempfile::tempdir_in("target").unwrap();
    let config = dir.path().join("config.toml");
    let marker = dir.path().join("refreshed");
    let text = format!(
        r#"
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
