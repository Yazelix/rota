use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    config: PathBuf,
}

/// A config whose `state_file` is `DIR/state/last-pick`; no registry exists yet.
fn fixture() -> Fixture {
    let temp = tempfile::tempdir_in("target").unwrap();
    let dir = fs::canonicalize(temp.path()).unwrap();
    let config = dir.join("config.toml");
    let text = format!(
        r#"state_file = '{dir}/state/last-pick'
log_file = '{dir}/state/decisions.jsonl'
[[backends]]
name = 'fake'
roles = ['implement', 'review']
guard = ['true']
allow_unknown_quota = true
[backends.commands]
medium = ['never-start-this', '{{prompt}}']
high = ['never-start-this', '{{prompt}}']
"#,
        dir = dir.display()
    );
    fs::write(&config, text).unwrap();
    Fixture {
        _temp: temp,
        dir,
        config,
    }
}

/// Writes `json` to `DIR/state/sessions/NAME`, creating the directory.
fn put(f: &Fixture, name: &str, json: &str) {
    let dir = f.dir.join("state/sessions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(name), json).unwrap();
}

/// Runs the built `rota` with `ROTA_CONFIG` set, in `DIR`, with `args`.
fn rota(f: &Fixture, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rota"))
        .env("ROTA_CONFIG", &f.config)
        .current_dir(&f.dir)
        .args(args)
        .output()
        .unwrap()
}

/// A run record: the session fields with these values, `paths` holding `log` only when
/// `log` is `Some`, and `extra` (a JSON object text, e.g. `{"pane":"p-7"}`) merged in.
fn record(id: &str, pid: u32, started: u64, cwd: &str, log: Option<&str>, extra: &str) -> String {
    let name = id.rsplit_once('-').map_or(id, |(name, _)| name);
    let paths = match log {
        Some(log) => serde_json::json!({"log": log}),
        None => serde_json::json!({}),
    };
    let mut record = serde_json::json!({
        "schema": 1,
        "id": id,
        "name": name,
        "kind": "run",
        "backend": "fake",
        "role": "review",
        "cwd": cwd,
        "brief": "/fictional/brief",
        "pid": pid,
        "rota_pid": 1,
        "started": started,
        "paths": paths,
    });
    let extra: serde_json::Value = serde_json::from_str(extra).unwrap();
    record
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    record.to_string()
}

/// Seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The PID of a child that has exited and been reaped.
fn gone_pid() -> u32 {
    let mut child = Command::new("true").spawn().unwrap();
    let id = child.id();
    child.wait().unwrap();
    id
}

/// The `id` of each element of a `rota ps --json` stdout.
fn ids(stdout: &[u8]) -> Vec<String> {
    let records: serde_json::Value = serde_json::from_slice(stdout).unwrap();
    records
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["id"].as_str().unwrap().to_string())
        .collect()
}

fn registry(f: &Fixture) -> (u32, u32) {
    let me = std::process::id();
    let gone = gone_pid();
    let n = now();
    put(
        f,
        "live-1.json",
        &record(
            "live-1",
            me,
            n - 100,
            "/fictional/live",
            Some("/fictional/runs/live-1/log"),
            r#"{"pane":"p-7"}"#,
        ),
    );
    put(
        f,
        "dead-1.json",
        &record(
            "dead-1",
            me,
            n - 200,
            "/fictional/dead",
            Some("/fictional/runs/dead-1/log"),
            r#"{"unit":"rota-dead-1-00.scope"}"#,
        ),
    );
    put(
        f,
        "gone-1.json",
        &record(
            "gone-1",
            gone,
            n - 7300,
            "/fictional/gone",
            Some("/fictional/runs/gone-1/log"),
            "{}",
        ),
    );
    put(
        f,
        "done-1.json",
        &record(
            "done-1",
            gone,
            n - 90000,
            "/fictional/done",
            None,
            r#"{"finished":1,"outcome":"exited","exit_code":0}"#,
        ),
    );
    put(f, ".lock", "");
    put(f, ".tmp1.json", "{not json");
    put(f, "notes.txt", "not a record");
    (me, gone)
}

#[test]
fn ps_lists_unfinished_sessions_with_liveness() {
    let f = fixture();
    let (me, gone) = registry(&f);
    let output = rota(&f, &["ps", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(ids(&output.stdout), ["gone-1", "dead-1", "live-1"]);
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v[2]["state"], "running");
    assert_eq!(v[2]["alive"], true);
    assert_eq!(v[2]["pid"], me);
    assert_eq!(v[2]["backend"], "fake");
    assert_eq!(v[2]["role"], "review");
    assert_eq!(v[2]["cwd"], "/fictional/live");
    assert_eq!(v[2]["paths"]["log"], "/fictional/runs/live-1/log");
    assert_eq!(v[2]["pane"], "p-7");
    let age = v[2]["age_seconds"].as_u64().unwrap();
    assert!((100..160).contains(&age));
    assert!(v[2].get("finished").is_none());
    assert_eq!(v[1]["state"], "dead");
    assert_eq!(v[1]["alive"], false);
    assert_eq!(v[1]["unit"], "rota-dead-1-00.scope");
    assert_eq!(v[0]["state"], "dead");
    assert_eq!(v[0]["alive"], false);
    assert_eq!(v[0]["pid"], gone);
}

#[test]
fn ps_all_adds_finished_sessions() {
    let f = fixture();
    registry(&f);
    let output = rota(&f, &["ps", "--all", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        ids(&output.stdout),
        ["done-1", "gone-1", "dead-1", "live-1"]
    );
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v[0]["state"], "finished");
    assert_eq!(v[0]["alive"], false);
    assert_eq!(v[0]["outcome"], "exited");
    assert_eq!(v[0]["exit_code"], 0);
    assert_eq!(v[0]["finished"], 1);
    let output = rota(&f, &["ps", "--json", "--all"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        ids(&output.stdout),
        ["done-1", "gone-1", "dead-1", "live-1"]
    );
}

#[test]
fn ps_prints_one_line_per_session() {
    let f = fixture();
    let (me, gone) = registry(&f);
    let output = rota(&f, &["ps", "--all"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 4);
    let prefix = [
        format!("done-1: finished backend=fake role=review pid={gone} age=1d1h "),
        format!("gone-1: dead backend=fake role=review pid={gone} age=2h1m "),
        format!("dead-1: dead backend=fake role=review pid={me} age=3m"),
        format!("live-1: running backend=fake role=review pid={me} age=1m"),
    ];
    let suffix = [
        " cwd=/fictional/done log=- outcome=exited exit=0",
        " cwd=/fictional/gone log=/fictional/runs/gone-1/log",
        " cwd=/fictional/dead log=/fictional/runs/dead-1/log",
        " cwd=/fictional/live log=/fictional/runs/live-1/log pane=p-7",
    ];
    for i in 0..4 {
        assert!(lines[i].starts_with(&prefix[i]), "{}", lines[i]);
        assert!(lines[i].ends_with(suffix[i]), "{}", lines[i]);
    }
    let output = rota(&f, &["ps"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 3);
    for (line, prefix) in lines.iter().zip(["gone-1: ", "dead-1: ", "live-1: "]) {
        assert!(line.starts_with(prefix));
    }
}

#[test]
fn ps_on_an_empty_registry_writes_nothing() {
    let f = fixture();
    let output = rota(&f, &["ps"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"no sessions\n");
    let output = rota(&f, &["ps", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v, serde_json::json!([]));
    assert!(!f.dir.join("state").exists());
    let output = rota(&f, &["ps", "--watch"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stderr, b"rota: unknown ps option\n");
}

#[test]
fn ps_reports_unreadable_records_and_prints_the_rest() {
    let f = fixture();
    let me = std::process::id();
    let n = now();
    put(
        &f,
        "live-1.json",
        &record(
            "live-1",
            me,
            n - 100,
            "/fictional/live",
            Some("/fictional/runs/live-1/log"),
            r#"{"pane":"p-7"}"#,
        ),
    );
    put(&f, "broken.json", "{not json");
    put(&f, "half.json", "{\"schema\":1}");
    let output = rota(&f, &["ps", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(ids(&output.stdout), ["live-1"]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let lines: Vec<_> = stderr.lines().collect();
    assert_eq!(lines.len(), 3);
    let sessions = f.dir.join("state/sessions");
    assert!(lines[0].starts_with(&format!(
        "rota: cannot read {}/broken.json: ",
        sessions.display()
    )));
    assert!(lines[1].starts_with(&format!(
        "rota: cannot read {}/half.json: ",
        sessions.display()
    )));
    assert_eq!(lines[2], "rota: unreadable session records: 2");
    assert_eq!(
        fs::read_to_string(sessions.join("broken.json")).unwrap(),
        "{not json"
    );
    assert_eq!(
        fs::read_to_string(sessions.join("half.json")).unwrap(),
        "{\"schema\":1}"
    );
}
