use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{fs, thread};

struct Fixture {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    config: PathBuf,
}

/// One backend `fake` with no launch roles; `run_table` is its `[backends.run]` text.
fn fixture(settings: &str, extra: &str, run_table: &str) -> Fixture {
    let temp = tempfile::tempdir_in("target").unwrap();
    let dir = fs::canonicalize(temp.path()).unwrap();
    fs::write(dir.join("brief.md"), "BRIEF LINE\n").unwrap();
    let config = dir.join("config.toml");
    fs::write(
        &config,
        format!(
            r#"state_file = '{dir}/state/last-pick'
log_file = '{dir}/state/decisions.jsonl'
{settings}
[[backends]]
name = 'fake'
roles = []
guard = ['true']
allow_unknown_quota = true
{extra}
[backends.commands]
medium = ['never-start-this', '{{prompt}}']
high = ['never-start-this', '{{prompt}}']
{run_table}
"#,
            dir = dir.display()
        ),
    )
    .unwrap();
    Fixture {
        _temp: temp,
        dir,
        config,
    }
}

fn run_table(stdin: &str, command: &str) -> String {
    format!(
        "[backends.run]\nstdin = '{stdin}'\nefforts = ['low', 'high']\ndefault_effort = 'high'\n\
         [backends.run.commands]\nreview = {command}\n"
    )
}

fn rota(f: &Fixture, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .current_dir(&f.dir)
        .args(["run", "--role", "review", "--brief", "brief.md"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A run must never read rota's own stdin. rota may already have exited (input errors).
    let _ = child.stdin.take().unwrap().write_all(b"LEAKED\n");
    child.wait_with_output().unwrap()
}

fn record(f: &Fixture, id: &str) -> serde_json::Value {
    let path = f.dir.join(format!("state/sessions/{id}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// Gone from /proc, reaped and not a zombie, polling for up to `wait`.
fn gone_within(pid: &str, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        let dead = !Path::new(&format!("/proc/{pid}")).exists();
        if dead || Instant::now() >= deadline {
            return dead;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn gone(pid: &str) -> bool {
    gone_within(pid, Duration::from_secs(10))
}

/// Starts `rota run` with `args` and stdout and stderr discarded.
fn spawn_rota(f: &Fixture, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .current_dir(&f.dir)
        .args(["run", "--role", "review", "--brief", "brief.md"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// The contents of `path` once it is non-empty, waiting up to 60 seconds.
fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let text = fs::read_to_string(path).unwrap_or_default();
        if !text.is_empty() || Instant::now() >= deadline {
            return text;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stdin_is_null_or_the_brief_never_rotas_own() {
    let probe = r#"['sh', '-c', 'if read line; then echo "stdin: $line"; else echo "stdin: eof"; fi; cat "$0"', '{brief}']"#;
    let f = fixture("", "", &run_table("null", probe));
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let log = fs::read_to_string(f.dir.join("state/runs/t-1/log")).unwrap();
    assert_eq!(log, "stdin: eof\nBRIEF LINE\n");
    let f = fixture(
        "",
        "",
        &run_table("brief", "['sh', '-c', 'read line; echo \"stdin: $line\"']"),
    );
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let log = fs::read_to_string(f.dir.join("state/runs/t-1/log")).unwrap();
    assert_eq!(log, "stdin: BRIEF LINE\n");
}

#[test]
fn records_the_session_and_mirrors_the_exit_status() {
    let command =
        r#"['sh', '-c', 'echo "$1" > "$0"; exit 0', '{report}', 'effort={effort} cwd={cwd}']"#;
    let f = fixture("", "", &run_table("brief", command));
    let output = rota(&f, &["--name", "t", "--timeout", "30", "--effort", "low"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    let run_dir = f.dir.join("state/runs/t-1");
    let report = run_dir.join("report.md");
    assert!(
        stdout.starts_with("pick: fake\nsession: t-1\npid: "),
        "{stdout}"
    );
    assert!(stdout.ends_with(&format!(
        "log: {}\nreport: {}\n",
        run_dir.join("log").display(),
        report.display()
    )));
    assert!(text(&output.stderr).contains("rota: t-1 exited with status 0"));
    let unit = record(&f, "t-1")["unit"].as_str().unwrap().to_string();
    assert!(
        unit.starts_with("rota-t-1-") && unit.ends_with(".scope"),
        "{unit}"
    );
    let cwd = f.dir.to_str().unwrap();
    assert_eq!(
        fs::read_to_string(&report).unwrap(),
        format!("effort=low cwd={cwd}\n")
    );
    let json = record(&f, "t-1");
    assert_eq!(json["schema"], 1);
    assert_eq!(json["id"], "t-1");
    assert_eq!(json["name"], "t");
    assert_eq!(json["kind"], "run");
    assert_eq!(json["backend"], "fake");
    assert_eq!(json["role"], "review");
    assert_eq!(json["effort"], "low");
    assert_eq!(json["cwd"], cwd);
    assert_eq!(json["brief"], f.dir.join("brief.md").to_str().unwrap());
    assert!(json["pid"].as_u64().unwrap() > 0);
    assert!(json["rota_pid"].as_u64().unwrap() > 0);
    assert_ne!(json["pid"], json["rota_pid"]);
    assert_eq!(json["timeout_seconds"], 30);
    assert_eq!(json["paths"]["run_dir"], run_dir.to_str().unwrap());
    assert_eq!(json["paths"]["log"], run_dir.join("log").to_str().unwrap());
    assert_eq!(json["paths"]["report"], report.to_str().unwrap());
    assert!(json["finished"].as_u64().unwrap() >= json["started"].as_u64().unwrap());
    assert_eq!(json["outcome"], "exited");
    assert_eq!(json["exit_code"], 0);
    let decisions = fs::read_to_string(f.dir.join("state/decisions.jsonl")).unwrap();
    assert!(decisions.contains(r#""decision":"pick""#));
    assert!(decisions.contains(r#""level":null"#));
    assert_eq!(
        fs::read_to_string(f.dir.join("state/last-pick")).unwrap(),
        "fake\n"
    );
    let text_config = fs::read_to_string(&f.config).unwrap();
    fs::write(&f.config, text_config.replace("exit 0", "exit 7")).unwrap();
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert_eq!(output.status.code(), Some(7));
    assert!(text(&output.stderr).contains("rota: t-2 exited with status 7"));
    let json = record(&f, "t-2");
    assert_eq!(json["effort"], "high");
    assert_eq!(
        (&json["outcome"], &json["exit_code"]),
        (&"exited".into(), &7.into())
    );
}

/// Starts a background `sleep 300` (PID in the report file) and a double-forked `setsid` one
/// that left the run's process group, session and parent (PID in `report.md.setsid`), waits
/// until both PIDs are written, then runs `tail`.
fn workers(tail: &str) -> String {
    format!(
        r#"["sh", "-c", "sleep 300 & echo $! > \"$0\"; (setsid sh -c 'echo $$ > \"$1\"; exec sleep 300' x \"$0.setsid\" &); while [ ! -s \"$0.setsid\" ]; do sleep 0.05; done; {tail}", "{{report}}"]"#
    )
}

fn assert_workers_gone(f: &Fixture) {
    for name in ["report.md", "report.md.setsid"] {
        let pid = wait_for(&f.dir.join("state/runs/t-1").join(name));
        assert!(!pid.is_empty(), "{name} has no PID");
        assert!(gone(pid.trim()), "{name} worker {pid} survived");
    }
}

/// Waits up to 60 s for `child` to exit.
fn exit_within_a_minute(child: &mut std::process::Child) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code();
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    panic!("rota still running after 60 s");
}

/// Whether systemd user unit `unit` is confirmed gone or its cgroup confirmed empty; `false`
/// when it is populated. Panics when neither can be confirmed.
fn scope_empty(unit: &str) -> bool {
    let output = Command::new("systemctl")
        .args([
            "--user",
            "show",
            "-p",
            "LoadState",
            "-p",
            "ActiveState",
            "-p",
            "ControlGroup",
        ])
        .arg(unit)
        .output()
        .unwrap();
    emptiness(unit, &output, Path::new("/sys/fs/cgroup"))
}

/// `scope_empty` from `systemctl show` output, with cgroups under `root`.
fn emptiness(unit: &str, output: &Output, root: &Path) -> bool {
    assert!(output.status.success(), "systemctl show {unit}: {output:?}");
    let shown = text(&output.stdout);
    let field = |name: &str| {
        let prefix = format!("{name}=");
        let value = shown.lines().find_map(|line| line.strip_prefix(&prefix));
        value.unwrap_or_else(|| panic!("no {name} for {unit}: {shown}"))
    };
    if field("LoadState") == "not-found" {
        return true;
    }
    let cgroup = field("ControlGroup");
    if cgroup.is_empty() {
        let state = field("ActiveState");
        assert!(
            ["inactive", "failed"].contains(&state),
            "{unit} {state} without a cgroup"
        );
        return true;
    }
    let events = root
        .join(cgroup.trim_start_matches('/'))
        .join("cgroup.events");
    match fs::read_to_string(events) {
        Ok(events) if events.lines().any(|line| line == "populated 0") => true,
        Ok(events) if events.lines().any(|line| line == "populated 1") => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        other => panic!("cannot tell whether {unit} is empty: {other:?}"),
    }
}

#[test]
fn the_emptiness_check_fails_on_unknown_results() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    let root = tempfile::tempdir_in("target").unwrap();
    let shown = |status: i32, text: &str| Output {
        status: ExitStatus::from_raw(status << 8),
        stdout: text.as_bytes().to_vec(),
        stderr: Vec::new(),
    };
    let check = |output: Output| {
        let root = root.path().to_path_buf();
        std::panic::catch_unwind(move || emptiness("u", &output, &root)).ok()
    };
    let loaded = "LoadState=loaded\nActiveState=active\nControlGroup=/u\n";
    // Confirmed results.
    assert_eq!(check(shown(0, "LoadState=not-found\n")), Some(true));
    assert_eq!(check(shown(0, loaded)), Some(true), "cgroup removed");
    fs::create_dir(root.path().join("u")).unwrap();
    let events = root.path().join("u/cgroup.events");
    fs::write(&events, "populated 1\n").unwrap();
    assert_eq!(check(shown(0, loaded)), Some(false));
    fs::write(&events, "populated 0\n").unwrap();
    assert_eq!(check(shown(0, loaded)), Some(true));
    // Unknown results: a failed systemctl, an unreadable cgroup.events.
    assert_eq!(check(shown(1, "LoadState=not-found\n")), None);
    fs::set_permissions(&events, fs::Permissions::from_mode(0o000)).unwrap();
    assert_eq!(check(shown(0, loaded)), None);
}

#[test]
fn timeout_stops_the_whole_tree() {
    let f = fixture("", "", &run_table("brief", &workers("wait")));
    let start = Instant::now();
    let output = rota(&f, &["--name", "t", "--timeout", "1s"]);
    assert!(start.elapsed() < Duration::from_secs(60));
    assert_eq!(output.status.code(), Some(124));
    assert!(text(&output.stderr).contains("rota: t-1 timed out after 1s"));
    let json = record(&f, "t-1");
    assert_eq!(json["outcome"], "timeout");
    assert_eq!(json["exit_code"], 124);
    assert_workers_gone(&f);
}

#[test]
fn leader_exit_stops_leftover_workers() {
    let f = fixture("", "", &run_table("brief", &workers("sleep 0.5; exit 0")));
    let start = Instant::now();
    let output = rota(&f, &["--name", "t", "--timeout", "60"]);
    assert!(start.elapsed() < Duration::from_secs(60));
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(record(&f, "t-1")["outcome"], "exited");
    assert_workers_gone(&f);
}

#[test]
fn grace_applies_to_every_worker() {
    // On SIGTERM the worker ignores further TERMs, says so, and waits for the test's go-ahead
    // before it saves. rota must still be waiting for it a second later.
    let command = r#"["sh", "-c", "(trap 'trap \"\" TERM; touch \"$0.term\"; while [ ! -e \"$0.go\" ]; do sleep 0.05; done; echo saved > \"$0\"; exit 0' TERM; while :; do sleep 0.1; done) & sleep 30", "{report}"]"#;
    let f = fixture("", "", &run_table("brief", command));
    let mut child = spawn_rota(&f, &["--name", "t", "--timeout", "1s"]);
    let report = f.dir.join("state/runs/t-1/report.md");
    let deadline = Instant::now() + Duration::from_secs(60);
    while !report.with_extension("md.term").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(report.with_extension("md.term").exists(), "no SIGTERM");
    thread::sleep(Duration::from_secs(1));
    assert_eq!(child.try_wait().unwrap(), None, "rota left the grace early");
    fs::write(report.with_extension("md.go"), "").unwrap();
    assert_eq!(exit_within_a_minute(&mut child), Some(124));
    assert_eq!(fs::read_to_string(report).unwrap(), "saved\n");
}

#[test]
fn cleanup_finishes_before_rota_reports() {
    use fs2::FileExt;
    // A double-forked worker that ignores SIGTERM outlives the leader; only SIGKILL ends it.
    let command = r#"["sh", "-c", "(setsid sh -c 'trap \"\" TERM; echo $$ > \"$1\"; exec sleep 300' x \"$0.setsid\" &); while [ ! -s \"$0.setsid\" ]; do sleep 0.05; done", "{report}"]"#;
    let f = fixture("", "", &run_table("brief", command));
    let start = Instant::now();
    let mut child = spawn_rota(&f, &["--name", "t", "--timeout", "60"]);
    wait_for(&f.dir.join("state/sessions/t-1.json"));
    // Holding the registry lock keeps rota from writing its final record, so rota is still
    // running while the test checks that cleanup already finished.
    let held = fs::File::open(f.dir.join("state/sessions/.lock")).unwrap();
    held.lock_exclusive().unwrap();
    let unit = record(&f, "t-1")["unit"].as_str().unwrap().to_string();
    let pid = wait_for(&f.dir.join("state/runs/t-1/report.md.setsid"));
    assert!(
        gone_within(pid.trim(), Duration::from_secs(30)),
        "worker {pid} survived"
    );
    assert!(scope_empty(&unit), "{unit} still has processes");
    assert_eq!(
        child.try_wait().unwrap(),
        None,
        "rota exited before the check"
    );
    assert_eq!(record(&f, "t-1")["finished"], serde_json::Value::Null);
    held.unlock().unwrap();
    assert_eq!(exit_within_a_minute(&mut child), Some(0));
    assert!(
        start.elapsed() >= Duration::from_secs(5),
        "no grace before SIGKILL"
    );
    assert_eq!(record(&f, "t-1")["outcome"], "exited");
}

#[test]
fn a_leader_whose_main_thread_exited_is_still_stopped() {
    // The main thread exits with pthread_exit while another thread keeps the process alive.
    let command = r#"['python3', '-c', 'import ctypes, os, sys, threading, time; open(sys.argv[1], "w").write(str(os.getpid())); threading.Thread(target=time.sleep, args=(300,)).start(); ctypes.CDLL(None).pthread_exit(None)', '{report}']"#;
    let f = fixture("", "", &run_table("brief", command));
    let mut child = spawn_rota(&f, &["--name", "t", "--timeout", "2s"]);
    let pid = wait_for(&f.dir.join("state/runs/t-1/report.md"));
    assert!(!pid.is_empty());
    assert_eq!(exit_within_a_minute(&mut child), Some(124));
    assert_eq!(record(&f, "t-1")["outcome"], "timeout");
    assert!(gone(pid.trim()), "leader {pid} survived");
}

#[test]
fn a_failed_first_record_write_stops_the_run() {
    use std::os::unix::fs::PermissionsExt;
    // A run that is not stopped leaves a marker after a second.
    let command = r#"['sh', '-c', 'sleep 1; touch "$0.late"; sleep 300', '{report}']"#;
    let f = fixture("", "", &run_table("brief", command));
    let sessions = f.dir.join("state/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join(".lock"), "").unwrap();
    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o500)).unwrap();
    let output = rota(&f, &["--name", "wfail", "--timeout", "60"]);
    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("rota: wfail-1 cannot write session record"));
    thread::sleep(Duration::from_secs(3));
    assert!(
        !f.dir.join("state/runs/wfail-1/report.md.late").exists(),
        "the run went on"
    );
}

#[test]
fn arguments_reach_the_command_literally() {
    let command = r#"['sh', '-c', 'printf %s "$1" > "$0"', '{report}', '$HOME ${HOME} $$']"#;
    let f = fixture("", "", &run_table("brief", command));
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let report = fs::read_to_string(f.dir.join("state/runs/t-1/report.md")).unwrap();
    assert_eq!(report, "$HOME ${HOME} $$");
}

#[test]
fn the_run_reaches_rotas_manager() {
    // A backend that points the manager variables elsewhere cannot cut the run off from the
    // manager that rota uses for cleanup.
    let extra = "[backends.env]\nXDG_RUNTIME_DIR = '/nonexistent'\n\
                 DBUS_SESSION_BUS_ADDRESS = 'unix:path=/nonexistent/bus'\n";
    let command = r#"['sh', '-c', 'printf %s "$XDG_RUNTIME_DIR" > "$0"', '{report}']"#;
    let f = fixture("", extra, &run_table("brief", command));
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let report = fs::read_to_string(f.dir.join("state/runs/t-1/report.md")).unwrap();
    assert_eq!(report, std::env::var("XDG_RUNTIME_DIR").unwrap());
}

/// `PATH` with a directory first that holds `name`, a stand-in that never answers.
fn stalled(f: &Fixture, name: &str) -> String {
    let bin = f.dir.join("stalled");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join(name);
    fs::write(&script, "#!/bin/sh\nexec sleep 300\n").unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    fs::set_permissions(&script, mode).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap())
}

#[test]
fn a_stalled_manager_is_refused_within_the_start_up_bound() {
    let f = fixture("", "", &run_table("brief", "['touch', 'ran']"));
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .env("PATH", stalled(&f, "systemd-run"))
        .current_dir(&f.dir)
        .args(["run", "--role", "review", "--brief", "brief.md"])
        .args(["--name", "t", "--timeout", "30"])
        .output()
        .unwrap();
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(60),
        "{elapsed:?} {}",
        text(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("no answer within 10000 ms"));
    assert!(!f.dir.join("state/runs").exists());
    assert!(!f.dir.join("ran").exists());
}

#[test]
fn cleanup_never_waits_for_systemctl() {
    // A TERM-proof run whose cleanup must reach SIGKILL on time with `systemctl` hung.
    let command = r#"['sh', '-c', 'trap "" TERM; echo $$ > "$0"; sleep 120', '{report}']"#;
    let f = fixture("", "", &run_table("brief", command));
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .env("PATH", stalled(&f, "systemctl"))
        .current_dir(&f.dir)
        .args(["run", "--role", "review", "--brief", "brief.md"])
        .args(["--name", "t", "--timeout", "1s"])
        .output()
        .unwrap();
    let (elapsed, code, stderr) = (start.elapsed(), output.status.code(), text(&output.stderr));
    assert!(
        elapsed < Duration::from_secs(60),
        "{elapsed:?} {code:?} {stderr}"
    );
    assert_eq!(code, Some(124), "{stderr}");
    let pid = fs::read_to_string(f.dir.join("state/runs/t-1/report.md")).unwrap();
    assert!(gone(pid.trim()), "TERM-proof shell {pid} survived");
}

#[test]
fn refuses_without_a_systemd_user_manager() {
    let f = fixture("", "", &run_table("brief", "['touch', 'ran']"));
    let output = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .current_dir(&f.dir)
        .args(["run", "--role", "review", "--brief", "brief.md"])
        .args(["--name", "t", "--timeout", "30"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("rota run needs a systemd user manager"));
    assert!(!f.dir.join("state/runs").exists());
    assert!(!f.dir.join("ran").exists());
}

#[test]
fn registry_lock_delays_the_start_not_the_timeout() {
    use fs2::FileExt;
    let command = r#"['sh', '-c', 'sleep 1; echo done > "$0"', '{report}']"#;
    let f = fixture("", "", &run_table("brief", command));
    fs::create_dir_all(f.dir.join("state/sessions")).unwrap();
    let held = fs::File::create(f.dir.join("state/sessions/.lock")).unwrap();
    held.lock_exclusive().unwrap();
    let start = Instant::now();
    let mut child = spawn_rota(&f, &["--name", "t", "--timeout", "2s"]);
    thread::sleep(Duration::from_secs(3));
    // Nothing starts while another command holds the registry lock.
    assert!(!f.dir.join("state/runs/t-1/report.md").exists());
    assert!(!f.dir.join("state/sessions/t-1.json").exists());
    held.unlock().unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(start.elapsed() >= Duration::from_secs(4));
    assert_eq!(record(&f, "t-1")["outcome"], "exited");
    let report = fs::read_to_string(f.dir.join("state/runs/t-1/report.md")).unwrap();
    assert_eq!(report, "done\n");
}

#[test]
fn sigkill_follows_an_ignored_sigterm() {
    let command = r#"['sh', '-c', 'trap "" TERM; echo $$ > "$0"; sleep 120', '{report}']"#;
    let f = fixture("", "", &run_table("brief", command));
    let start = Instant::now();
    let output = rota(&f, &["--name", "t", "--timeout", "1s"]);
    let elapsed = start.elapsed();
    assert!(elapsed >= Duration::from_secs(6), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(60), "{elapsed:?}");
    assert_eq!(output.status.code(), Some(124));
    assert_eq!(record(&f, "t-1")["outcome"], "timeout");
    let pid = fs::read_to_string(f.dir.join("state/runs/t-1/report.md")).unwrap();
    assert!(gone(pid.trim()), "TERM-proof shell {pid} survived");
}

#[test]
fn signal_death_exits_128_plus_n() {
    let f = fixture("", "", &run_table("brief", "['sh', '-c', 'kill -9 $$']"));
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert_eq!(output.status.code(), Some(137));
    assert!(text(&output.stderr).contains("rota: t-1 killed by signal 9"));
    let json = record(&f, "t-1");
    assert_eq!(
        (&json["outcome"], &json["exit_code"]),
        (&"signaled".into(), &137.into())
    );
}

#[test]
fn closed_stdout_does_not_stop_supervision() {
    let extra = "prepare = [['sleep', '1']]\n";
    let command = r#"['sh', '-c', 'sleep 1; echo done > "$0"', '{report}']"#;
    let f = fixture("", extra, &run_table("brief", command));
    let mut child = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .current_dir(&f.dir)
        .args(["run", "--role", "review", "--brief", "brief.md"])
        .args(["--name", "t", "--timeout", "30"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert_eq!(first, "pick: fake\n");
    // The reader is dropped: rota's next writes, after the 1 s prepare, hit a closed pipe.
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert_eq!(record(&f, "t-1")["outcome"], "exited");
    let report = f.dir.join("state/runs/t-1/report.md");
    assert_eq!(fs::read_to_string(report).unwrap(), "done\n");
}

#[test]
fn status_shows_headless_only_roles() {
    let f = fixture("", "", &run_table("brief", "['true']"));
    let output = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .args(["status", "--json"])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json[0]["review"], "available for rota run");
    assert_eq!(json[0]["implement"], "not allowed for role");
}

#[test]
fn termination_signal_stops_the_run() {
    let f = fixture("", "", &run_table("brief", "['sleep', '30']"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .current_dir(&f.dir)
        .args([
            "run", "--role", "review", "--brief", "brief.md", "--name", "t",
        ])
        .args(["--timeout", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for(&f.dir.join("state/sessions/t-1.json"));
    let pid = record(&f, "t-1")["pid"].to_string();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    assert_eq!(child.wait().unwrap().code(), Some(130));
    assert_eq!(record(&f, "t-1")["outcome"], "interrupted");
    assert!(gone(&pid));
}

#[test]
fn scrub_list_applies_to_the_run_but_not_to_checks() {
    let settings = "[run]\nunset_env = ['ROTA_T_DROP', 'ROTA_T_PREFIX_*']\n\
                    [run.env]\nROTA_T_SET = 'private'\nROTA_T_PREFIX_OVERRIDE = 'set'\n";
    let extra = "[backends.env]\nROTA_T_BACKEND = 'backend'\n";
    let f = fixture(settings, extra, &run_table("brief", "['env']"));
    let config = fs::read_to_string(&f.config).unwrap();
    let guard = r#"guard = ['sh', '-c', 'test "$ROTA_T_DROP" = 1']"#;
    fs::write(&f.config, config.replace("guard = ['true']", guard)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .env("ROTA_T_DROP", "1")
        .env("ROTA_T_PREFIX_A", "1")
        .env("ROTA_T_KEEP", "1")
        .current_dir(&f.dir)
        .args(["run", "--role", "review", "--brief", "brief.md"])
        .args(["--name", "t", "--timeout", "30"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    let log = fs::read_to_string(f.dir.join("state/runs/t-1/log")).unwrap();
    let lines: Vec<&str> = log.lines().collect();
    for present in [
        "ROTA_T_KEEP=1",
        "ROTA_T_SET=private",
        "ROTA_T_PREFIX_OVERRIDE=set",
        "ROTA_T_BACKEND=backend",
    ] {
        assert!(lines.contains(&present), "{present} missing:\n{log}");
    }
    for absent in ["ROTA_T_DROP=", "ROTA_T_PREFIX_A="] {
        assert!(
            !lines.iter().any(|l| l.starts_with(absent)),
            "{absent} leaked"
        );
    }
}

fn assert_refused(f: &Fixture, message: &str) {
    let output = rota(f, &["--name", "t", "--timeout", "30"]);
    assert_eq!(output.status.code(), Some(3));
    let stderr = text(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
    assert!(
        stderr.contains("no backend available, refusing"),
        "{stderr}"
    );
    assert!(!f.dir.join("ran").exists());
    assert!(!f.dir.join("state/sessions").exists());
    assert!(!f.dir.join("state/runs").exists());
    let decisions = fs::read_to_string(f.dir.join("state/decisions.jsonl")).unwrap();
    assert!(decisions.contains(r#""decision":"REFUSAL""#));
}

#[test]
fn guard_and_quota_refusals_start_nothing() {
    let marker = "['touch', 'ran']";
    let f = fixture("", "", &run_table("brief", marker));
    let config = fs::read_to_string(&f.config).unwrap();
    fs::write(
        &f.config,
        config.replace("guard = ['true']", "guard = ['false']"),
    )
    .unwrap();
    assert_refused(&f, "command guard missing or failed");
    let quota = "[backends.quota]\ncommand = ['printf', '{\"week\":5}']\nparser = 'remaining_json'\n\
                 [backends.limits.review.min_remaining]\nweek = 10\n";
    let f = fixture("", quota, &run_table("brief", marker));
    assert_refused(&f, "quota limit: week_left=5%");
}

#[test]
fn unsupported_backends_are_input_errors() {
    let f = fixture("", "", "");
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("no backend has a headless command for these options"));
    let output = rota(&f, &["--name", "t", "--timeout", "30", "--only", "fake"]);
    assert!(text(&output.stderr).contains("fake: no [backends.run] table"));
    let output = rota(&f, &["--name", "t", "--timeout", "30", "--only", "absent"]);
    assert!(text(&output.stderr).contains("--only absent is not a configured backend"));
    let f = fixture("", "", &run_table("brief", "['true']"));
    let output = rota(
        &f,
        &[
            "--name",
            "t",
            "--timeout",
            "30",
            "--only",
            "fake",
            "--effort",
            "max",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("fake: effort max is not in efforts"));
    let output = rota(
        &f,
        &[
            "--name",
            "t",
            "--timeout",
            "30",
            "--only",
            "fake",
            "--add-dir",
            ".",
        ],
    );
    assert!(text(&output.stderr).contains("fake: the review command takes no --add-dir"));
    assert!(!Path::new(&f.dir.join("state")).exists());
    let f = fixture("", "", &run_table("brief", "['touch', 'ran']"));
    let output = rota(&f, &["--name", "t", "--timeout", "18446744073709551615"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(text(&output.stderr).contains("at most 30 days"));
    assert!(!f.dir.join("state/runs").exists());
    assert!(!f.dir.join("ran").exists());
    fs::write(f.dir.join("brief.md"), "").unwrap();
    fs::set_permissions(
        f.dir.join("brief.md"),
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .unwrap();
    let output = rota(&f, &["--name", "t", "--timeout", "30"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output.stderr));
    assert!(text(&output.stderr).contains("cannot use brief file"));
    assert!(!f.dir.join("state/runs").exists());
}
