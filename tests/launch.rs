use std::{fs, process::Command};

#[test]
fn dry_run_rotates_renders_and_changes_nothing() {
    let dir = tempfile::tempdir_in("target").unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let config_path = dir.join("config.toml");
    let state = dir.join("state");
    let log = dir.join("log");
    let trusted = dir.join("trust.json");
    let prompt = dir.join("task's prompt");
    let marker = dir.join("prepared");
    fs::write(&prompt, "SECRET PROMPT CONTENT").unwrap();
    fs::write(&state, "first\n").unwrap();
    let mut config = format!(
        "state_file = '{}'\nlog_file = '{}'\n",
        state.display(),
        log.display()
    );
    for name in ["first", "second"] {
        config.push_str(&format!(
            r#"
[[backends]]
name = '{name}'
roles = ['implement', 'review']
guard = ['true']
allow_unknown_quota = true
trusted_settings = '{}'
prepare = [['touch', '{}']]
[backends.commands]
medium = ['never-start-this', '--model', 'medium', '{{prompt}}']
high = ['never-start-this', '--model', 'high', '{{prompt}}']
"#,
            trusted.display(),
            marker.display()
        ));
    }
    fs::write(&config_path, &config).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rota"))
            .env("ROTA_CONFIG", &config_path)
            .current_dir(&dir)
            .args(args)
            .arg(&prompt)
            .output()
            .unwrap()
    };
    let output = run(&["launch", "--dry-run"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("pick: second"));
    assert!(stdout.contains("--model high"));
    assert!(stdout.contains("$(cat "));
    assert!(!stdout.contains("SECRET"));
    assert_eq!(fs::read_to_string(&state).unwrap(), "first\n");
    assert!(!log.exists());
    assert!(!trusted.exists());
    assert!(!marker.exists());
    assert!(!dir.join("state.lock").exists());
    let output = run(&[
        "launch",
        "--dry-run",
        "--level",
        "medium",
        "--only",
        "first",
    ]);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("pick: first\ncommand: never-start-this --model medium")
    );
    let output = run(&["launch", "--dry-run", "--only", "absent"]);
    assert_eq!(output.status.code(), Some(2));
    fs::write(
        &config_path,
        config.replace("guard = ['true']", "guard = ['false']"),
    )
    .unwrap();
    assert_eq!(
        run(&["launch", "--dry-run", "--role", "review"])
            .status
            .code(),
        Some(3)
    );
    fs::remove_file(&prompt).unwrap();
    assert_eq!(run(&["launch", "--dry-run"]).status.code(), Some(2));
}

#[test]
fn process_cap_follows_account_directory_and_executable_symlinks() {
    use rota::{config::Cap, readings::running};
    use std::{collections::BTreeMap, os::unix::fs::symlink};
    let dir = tempfile::tempdir_in("target").unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let account = dir.join("account");
    let alias = dir.join("alias");
    fs::create_dir(&account).unwrap();
    symlink(&account, &alias).unwrap();
    let cap = Cap {
        max: 1,
        executable: "sleep".into(),
        env: BTreeMap::from([("ROTA_TEST_ACCOUNT".into(), account.to_str().unwrap().into())]),
        exclude_args: vec![],
    };
    let executable_alias = dir.join("worker-alias");
    symlink("/bin/sleep", &executable_alias).unwrap();
    let mut child = Command::new(executable_alias)
        .arg("30")
        .env("ROTA_TEST_ACCOUNT", alias)
        .spawn()
        .unwrap();
    let count = running(&cap);
    let mut config = rota::config::Config::parse(include_str!("../config.example.toml")).unwrap();
    config.backends[0].cap = Some(cap);
    let mut reading = rota::decision::Reading::default();
    let rechecked = rota::readings::recheck_cap(&config.backends[0], &mut reading);
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(count.unwrap(), 1);
    assert!(rechecked.unwrap_err().contains("busy"));
    assert_eq!(reading.running, 1);
}

#[test]
fn refresh_rechecks_cap_after_a_slow_failed_probe() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let dir = tempfile::tempdir_in("target").unwrap();
    let dir = fs::canonicalize(dir.path()).unwrap();
    let started = dir.join("probe-started");
    let refreshed = dir.join("refreshed");
    let mut config = rota::config::Config::parse(include_str!("../config.example.toml")).unwrap();
    let mut backend = config.backends.remove(1);
    backend.guard = vec!["true".into()];
    backend.required_files.clear();
    backend
        .env
        .insert("ROTA_TEST_REFRESH".into(), dir.to_str().unwrap().into());
    backend.cap = Some(rota::config::Cap {
        max: 1,
        executable: "sleep".into(),
        env: [("ROTA_TEST_REFRESH".into(), dir.to_str().unwrap().into())].into(),
        exclude_args: vec![],
    });
    let reader = backend.quota.as_mut().unwrap();
    reader.command = vec![
        "sh".into(),
        "-c".into(),
        r#"touch "$1"; sleep 0.5; printf failed"#.into(),
        "probe".into(),
        started.to_str().unwrap().into(),
    ];
    reader.refresh = Some(vec!["touch".into(), refreshed.to_str().unwrap().into()]);
    let probe =
        thread::spawn(move || rota::readings::read(&backend, std::path::Path::new("."), true));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !started.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(started.exists());
    let mut child = Command::new("sleep")
        .arg("30")
        .env("ROTA_TEST_REFRESH", &dir)
        .spawn()
        .unwrap();
    let reading = probe.join();
    child.kill().unwrap();
    child.wait().unwrap();
    let reading = reading.unwrap();
    assert_eq!(reading.running, 1);
    assert!(reading.problem.unwrap().contains("busy"));
    assert!(!refreshed.exists());
}
