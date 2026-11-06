//! The session registry: one JSON record per agent session under
//! `<state dir>/sessions/<id>.json`, where the state dir holds `state_file`.
use crate::config::Role;
use crate::launch::{atomic_write, ensure_parent};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SCHEMA: u32 = 1;
/// How long `update` retries a busy registry lock before it fails.
pub const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Later commands add optional fields; fields this version does not know are kept.
#[derive(Debug, Deserialize, Serialize)]
pub struct Session {
    pub schema: u32,
    pub id: String,
    pub name: String,
    pub kind: String,
    pub backend: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub cwd: PathBuf,
    pub brief: PathBuf,
    pub pid: u32,
    pub rota_pid: u32,
    pub started: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub paths: BTreeMap<String, PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u8>,
    /// The run's transient systemd user scope, such as `rota-NAME-N-1a2b.scope`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

pub fn state_dir(state_file: &Path) -> PathBuf {
    state_file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

pub fn record_path(state_file: &Path, id: &str) -> PathBuf {
    state_dir(state_file)
        .join("sessions")
        .join(format!("{id}.json"))
}

/// Claims the first free `NAME-N` (N from 1) by creating its run directory.
pub fn claim(state_file: &Path, name: &str) -> std::io::Result<(String, PathBuf)> {
    let runs = state_dir(state_file).join("runs");
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&runs)?;
    for n in 1..10_000 {
        let id = format!("{name}-{n}");
        if record_path(state_file, &id).exists() {
            continue;
        }
        let dir = runs.join(&id);
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "no free session id for {name}"
    )))
}

/// Every record write holds `sessions/.lock`, so a read-modify-write never loses another's fields.
fn open_lock(state_file: &Path) -> std::io::Result<File> {
    let path = state_dir(state_file).join("sessions").join(".lock");
    ensure_parent(&path)?;
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// Waits for the registry lock. Take it before starting anything that must not wait.
pub fn lock(state_file: &Path) -> std::io::Result<File> {
    let lock = open_lock(state_file)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

/// Tries the registry lock for up to `wait`, so a stuck holder cannot stall the caller forever.
fn lock_within(state_file: &Path, wait: Duration) -> std::io::Result<File> {
    let lock = open_lock(state_file)?;
    let until = Instant::now() + wait;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

fn store(path: &Path, json: &serde_json::Value) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(json)?;
    bytes.push(b'\n');
    atomic_write(path, &bytes)
}

/// Writes a new record; `_held` is the registry lock from `lock`.
pub fn write(_held: &File, state_file: &Path, session: &Session) -> std::io::Result<()> {
    store(
        &record_path(state_file, &session.id),
        &serde_json::to_value(session)?,
    )
}

/// Sets top-level `fields` on the record as it is on disk now, keeping every other field.
/// Only a missing record falls back to `session`; any other read or parse error fails.
pub fn update(
    state_file: &Path,
    session: &Session,
    fields: serde_json::Map<String, serde_json::Value>,
) -> std::io::Result<()> {
    let _lock = lock_within(state_file, LOCK_WAIT)?;
    let path = record_path(state_file, &session.id);
    let current = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::to_value(session)?,
        Err(e) => return Err(e),
    };
    let serde_json::Value::Object(mut json) = current else {
        return Err(std::io::Error::other("session record is not a JSON object"));
    };
    json.extend(fields);
    store(&path, &serde_json::Value::Object(json))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_keep_fields_added_by_later_versions() {
        let text = r#"{"schema":1,"id":"t-1","name":"t","kind":"run","backend":"b",
            "role":"review","cwd":"/fictional","brief":"/fictional/brief","pid":2,
            "rota_pid":1,"started":5,"paths":{"log":"/fictional/log"},
            "pane":"p-9","worktree":{"path":"/fictional/wt"}}"#;
        let session: Session = serde_json::from_str(text).unwrap();
        assert_eq!(session.extra["pane"], "p-9");
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["pane"], "p-9");
        assert_eq!(json["worktree"]["path"], "/fictional/wt");
        assert!(json.get("finished").is_none());
        assert!(json.get("effort").is_none());
    }
    #[test]
    fn update_keeps_fields_written_during_the_run() {
        let dir = tempfile::tempdir_in("target").unwrap();
        let state = dir.path().join("last-pick");
        let text = r#"{"schema":1,"id":"t-1","name":"t","kind":"run","backend":"b",
            "role":"review","cwd":"/fictional","brief":"/fictional/brief","pid":2,
            "rota_pid":1,"started":5,"paths":{"log":"/fictional/log"}}"#;
        let session: Session = serde_json::from_str(text).unwrap();
        write(&lock(&state).unwrap(), &state, &session).unwrap();
        let path = record_path(&state, "t-1");
        let mut on_disk: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        on_disk["pane"] = "p-9".into();
        on_disk["paths"]["target"] = "/fictional/target".into();
        fs::write(&path, on_disk.to_string()).unwrap();
        let fields = serde_json::json!({"finished": 9, "outcome": "exited", "exit_code": 0});
        update(&state, &session, fields.as_object().unwrap().clone()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["pane"], "p-9");
        assert_eq!(json["paths"]["target"], "/fictional/target");
        assert_eq!(json["paths"]["log"], "/fictional/log");
        assert_eq!(json["finished"], 9);
        assert_eq!(json["outcome"], "exited");
        assert_eq!(json["exit_code"], 0);
        assert!(dir.path().join("sessions/.lock").exists());
        fs::remove_file(&path).unwrap();
        let fields = serde_json::json!({"finished": 9});
        update(&state, &session, fields.as_object().unwrap().clone()).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!((&json["id"], &json["finished"]), (&"t-1".into(), &9.into()));
        fs::write(&path, "{not json").unwrap();
        assert!(update(&state, &session, fields.as_object().unwrap().clone()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{not json");
    }
    #[test]
    fn update_waits_for_the_registry_lock() {
        let dir = tempfile::tempdir_in("target").unwrap();
        let state = dir.path().join("last-pick");
        let text = r#"{"schema":1,"id":"t-1","name":"t","kind":"run","backend":"b",
            "role":"review","cwd":"/fictional","brief":"/fictional/brief","pid":2,
            "rota_pid":1,"started":5}"#;
        let session: Session = serde_json::from_str(text).unwrap();
        let held = lock(&state).unwrap();
        write(&held, &state, &session).unwrap();
        // Another command read-modify-writes the record while it holds the lock; the waiting
        // update must see its result, not the record as it was before.
        let path = record_path(&state, "t-1");
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let (state2, session2) = (
            state.clone(),
            serde_json::from_str::<Session>(text).unwrap(),
        );
        let finisher = std::thread::spawn(move || {
            let fields = serde_json::json!({"finished": 9});
            update(&state2, &session2, fields.as_object().unwrap().clone())
        });
        std::thread::sleep(Duration::from_millis(300));
        json["pane"] = "p-9".into();
        fs::write(&path, json.to_string()).unwrap();
        drop(held);
        finisher.join().unwrap().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            (&json["pane"], &json["finished"]),
            (&"p-9".into(), &9.into())
        );
    }
    #[test]
    fn claims_the_next_free_id() {
        let dir = tempfile::tempdir_in("target").unwrap();
        let state = dir.path().join("state/last-pick");
        let (first, run_dir) = claim(&state, "t").unwrap();
        assert_eq!(first, "t-1");
        assert!(run_dir.ends_with("state/runs/t-1"));
        assert_eq!(claim(&state, "t").unwrap().0, "t-2");
        std::fs::create_dir_all(dir.path().join("state/sessions")).unwrap();
        std::fs::write(record_path(&state, "t-3"), "{}").unwrap();
        assert_eq!(claim(&state, "t").unwrap().0, "t-4");
    }
}
