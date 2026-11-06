use crate::config::{Backend, Cap, Config};
use crate::decision::Reading;
use crate::quota;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub fn command(backend: &Backend, argv: &[String]) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    for key in &backend.unset_env {
        cmd.env_remove(key);
    }
    cmd.envs(&backend.env);
    cmd
}

pub fn probe(
    backend: &Backend,
    argv: &[String],
    seconds: u64,
    cwd: &Path,
) -> Result<String, String> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .ok_or("check timeout too large")?;
    let mut child = command(backend, argv)
        .process_group(0)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| "could not execute check")?;
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let stop = |child: &mut std::process::Child| {
        // The unreaped child's PID identifies only the process group we created.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    let mut bytes = None;
    loop {
        if bytes.is_none() {
            match receiver.try_recv() {
                Ok(Ok(output)) if output.len() <= 1_048_576 => bytes = Some(output),
                Ok(_) | Err(mpsc::TryRecvError::Disconnected) => {
                    stop(&mut child);
                    return Err("check output unreadable or exceeds 1 MiB".into());
                }
                Err(mpsc::TryRecvError::Empty) => (),
            }
        }
        // Keep the PID unreaped while a descendant may still hold stdout open.
        if let Some(output) = &mut bytes {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    return String::from_utf8(std::mem::take(output))
                        .map_err(|_| "check returned non-UTF-8 output".into());
                }
                Ok(Some(status)) => return Err(format!("check failed ({status})")),
                Ok(None) => (),
                Err(_) => {
                    stop(&mut child);
                    return Err("cannot wait for check".into());
                }
            }
        }
        if Instant::now() >= deadline {
            stop(&mut child);
            return Err("check timed out".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn canonical_equal(a: impl AsRef<Path>, b: impl AsRef<Path>) -> bool {
    let (a, b) = (a.as_ref(), b.as_ref());
    a == b
        || fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

fn executable_matches(
    cap: &Cap,
    argv: &[String],
    actual: Option<&Path>,
    resolved: Option<&Path>,
) -> bool {
    argv.first().is_some_and(|a| {
        Path::new(a)
            .file_name()
            .is_some_and(|n| n == cap.executable.as_str())
    }) || actual.is_some_and(|p| {
        p.file_name().is_some_and(|n| n == cap.executable.as_str())
            || resolved == Some(p)
            || cap
                .runtime_executables
                .iter()
                .any(|runtime| fs::canonicalize(runtime).ok().as_deref() == Some(p))
    })
}

fn matches_filters(cap: &Cap, argv: &[String], env: &BTreeMap<String, String>, cwd: &Path) -> bool {
    cap.script.as_ref().is_none_or(|script| {
        argv.iter()
            .skip(1)
            .any(|arg| canonical_equal(cwd.join(arg), script))
    }) && !argv.iter().any(|arg| cap.exclude_args.contains(arg))
        && cap.env.iter().all(|(key, value)| {
            env.get(key)
                .is_some_and(|v| v == value || canonical_equal(cwd.join(v), value))
        })
}

fn split_null(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

pub fn running(cap: &Cap) -> Result<usize, String> {
    running_in(Path::new("/proc"), unsafe { libc::geteuid() }, cap)
}

fn owned_by(dir: &Path, uid: u32) -> bool {
    fs::metadata(dir).is_ok_and(|meta| meta.uid() == uid)
}

/// The cap is per user: only `uid`'s processes count. A process that exits or
/// changes owner mid-scan is skipped. A live process of `uid` that matches by
/// command line but cannot be inspected fails the scan, as does an unlistable
/// `proc`.
fn running_in(proc: &Path, uid: u32, cap: &Cap) -> Result<usize, String> {
    let mut count = 0;
    let resolved = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .find_map(|dir| fs::canonicalize(dir.join(&cap.executable)).ok());
    for entry in fs::read_dir(proc).map_err(|_| "cannot scan /proc for concurrency cap")? {
        let entry = entry.map_err(|_| "cannot read process entry")?;
        if !entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        if !owned_by(&entry.path(), uid) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv = split_null(&cmdline);
        let actual = fs::read_link(entry.path().join("exe")).ok();
        if !executable_matches(cap, &argv, actual.as_deref(), resolved.as_deref()) {
            continue;
        }
        let gone = |e: &std::io::Error| {
            e.kind() == std::io::ErrorKind::NotFound || !owned_by(&entry.path(), uid)
        };
        let env = if cap.env.is_empty() {
            BTreeMap::new()
        } else {
            match fs::read(entry.path().join("environ")) {
                Ok(bytes) => split_null(&bytes)
                    .into_iter()
                    .filter_map(|s| s.split_once('=').map(|(k, v)| (k.into(), v.into())))
                    .collect(),
                Err(e) if gone(&e) => continue,
                Err(_) => return Err("cannot inspect matching process environment".into()),
            }
        };
        let cwd = if cap.script.is_some() || !cap.env.is_empty() {
            match fs::read_link(entry.path().join("cwd")) {
                Ok(cwd) => cwd,
                Err(e) if gone(&e) => continue,
                Err(_) => return Err("cannot inspect matching process working directory".into()),
            }
        } else {
            PathBuf::new()
        };
        if matches_filters(cap, &argv, &env, &cwd) {
            count += 1;
        }
    }
    Ok(count)
}

pub fn read(backend: &Backend, cwd: &Path, allow_refresh: bool) -> Reading {
    let mut reading = Reading::default();
    let result = (|| -> Result<(), String> {
        if !backend.enabled {
            return Err("disabled".into());
        }
        if backend
            .excluded_roots
            .iter()
            .any(|root| cwd.starts_with(fs::canonicalize(root).unwrap_or_else(|_| root.clone())))
        {
            return Err("folder belongs to an excluded account".into());
        }
        for path in &backend.required_files {
            if !path.is_file() {
                return Err("required credentials/file missing".into());
            }
        }
        probe(backend, &backend.guard, 15, cwd).map_err(|_| "command guard missing or failed")?;
        if let Some(check) = &backend.account_check {
            probe(backend, check, 20, cwd).map_err(|_| "account check failed")?;
        }
        if let Some(reader) = &backend.quota {
            let get = || {
                probe(backend, &reader.command, reader.timeout_seconds, cwd)
                    .and_then(|s| quota::parse(reader.parser, reader.agy_group.as_deref(), &s))
            };
            let mut quota = get();
            if quota.is_err() && allow_refresh {
                if let Some(refresh) = &reader.refresh {
                    recheck_cap(backend, &mut reading)?;
                    let _ = probe(backend, refresh, reader.refresh_timeout_seconds, cwd);
                    quota = get();
                }
            }
            let quota = quota.map_err(|e| format!("quota unreadable: {e}"))?;
            reading.detail = quota
                .remaining
                .iter()
                .map(|(k, v)| format!("{k}_left={v}%"))
                .collect::<Vec<_>>()
                .join(" ");
            reading.resets = quota.resets;
            reading.remaining = Some(quota.remaining);
        } else {
            reading.detail = "quota unknown".into();
        }
        Ok(())
    })();
    if let Err(problem) = result {
        reading.problem = Some(problem);
    }
    reading
}

pub fn recheck_cap(backend: &Backend, reading: &mut Reading) -> Result<(), String> {
    if let Some(cap) = &backend.cap {
        reading.running = running(cap)?;
        if reading.running >= cap.max {
            return Err(format!("busy ({} running)", reading.running));
        }
    }
    Ok(())
}

pub fn collect(
    config: &Config,
    indices: &[usize],
    cwd: &Path,
    allow_refresh: bool,
) -> Vec<Reading> {
    let mut readings: Vec<_> = config
        .backends
        .iter()
        .enumerate()
        .map(|(i, b)| {
            if indices.contains(&i) {
                read(b, cwd, allow_refresh)
            } else {
                Reading {
                    problem: Some("not checked".into()),
                    ..Reading::default()
                }
            }
        })
        .collect();
    for index in indices {
        if config.backends[*index].enabled {
            if let Err(problem) = recheck_cap(&config.backends[*index], &mut readings[*index]) {
                if readings[*index].problem.is_none() {
                    readings[*index].problem = Some(problem);
                }
            }
        }
    }
    readings
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_caps_include_all_roles_and_exclude_usage() {
        let mut cap = Cap {
            max: 1,
            executable: "fake-agent".into(),
            runtime_executables: vec![],
            script: None,
            env: BTreeMap::new(),
            exclude_args: vec!["/usage".into()],
        };
        let args = vec!["/bin/fake-agent".into(), "review anything".into()];
        assert!(matches_filters(
            &cap,
            &args,
            &BTreeMap::new(),
            Path::new(".")
        ));
        assert!(!matches_filters(
            &cap,
            &["fake-agent".into(), "/usage".into()],
            &BTreeMap::new(),
            Path::new(".")
        ));
        cap.env
            .insert("AGENT_HOME".into(), "/fictional/account".into());
        assert!(!matches_filters(
            &cap,
            &args,
            &BTreeMap::new(),
            Path::new(".")
        ));
        assert!(matches_filters(&cap, &args, &cap.env, Path::new(".")));
    }
    #[test]
    fn canonical_account_directory_aliases_match() {
        assert!(canonical_equal("src/../src", "src"));
        assert!(!canonical_equal("absent-one", "absent-two"));
    }
    #[test]
    fn disabled_backend_never_runs_checks() {
        let config = Config::parse(include_str!("../config.example.toml")).unwrap();
        assert_eq!(
            read(&config.backends[3], Path::new("."), false)
                .problem
                .as_deref(),
            Some("disabled")
        );
    }
    #[test]
    fn timeout_includes_descendant_output_drain() {
        let config = Config::parse(include_str!("../config.example.toml")).unwrap();
        let start = Instant::now();
        let result = probe(
            &config.backends[0],
            &["sh".into(), "-c".into(), "sleep 8 & printf '25 25'".into()],
            1,
            Path::new("."),
        );
        assert!(result.unwrap_err().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    fn scan_cap() -> Cap {
        Cap {
            max: 1,
            executable: "fake-agent".into(),
            runtime_executables: vec![],
            script: None,
            env: BTreeMap::from([("AGENT_HOME".into(), "/fictional/account".into())]),
            exclude_args: vec![],
        }
    }
    fn fake_process(proc: &Path, pid: &str, argv0: &str) -> PathBuf {
        let dir = proc.join(pid);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("cmdline"), format!("{argv0}\0work\0")).unwrap();
        fs::write(dir.join("environ"), "AGENT_HOME=/fictional/account\0").unwrap();
        std::os::unix::fs::symlink(proc, dir.join("cwd")).unwrap();
        dir
    }
    #[test]
    fn cap_scan_counts_current_users_matching_processes() {
        let root = tempfile::tempdir_in("target").unwrap();
        let proc = root.path();
        let uid = fs::metadata(proc).unwrap().uid();
        fake_process(proc, "100", "/bin/fake-agent");
        fake_process(proc, "101", "fake-agent");
        fake_process(proc, "102", "/bin/other-agent");
        fs::create_dir(proc.join("self")).unwrap();
        assert_eq!(running_in(proc, uid, &scan_cap()), Ok(2));
    }
    #[test]
    fn cap_scan_skips_other_users_processes() {
        let root = tempfile::tempdir_in("target").unwrap();
        let proc = root.path();
        let uid = fs::metadata(proc).unwrap().uid();
        fake_process(proc, "100", "/bin/fake-agent");
        assert_eq!(running_in(proc, uid.wrapping_add(1), &scan_cap()), Ok(0));
    }
    #[test]
    fn cap_scan_skips_vanished_processes() {
        let root = tempfile::tempdir_in("target").unwrap();
        let proc = root.path();
        let uid = fs::metadata(proc).unwrap().uid();
        fake_process(proc, "100", "/bin/fake-agent");
        std::os::unix::fs::symlink(proc.join("exited"), proc.join("200")).unwrap();
        let no_cwd = fake_process(proc, "201", "/bin/fake-agent");
        fs::remove_file(no_cwd.join("cwd")).unwrap();
        let no_environ = fake_process(proc, "202", "/bin/fake-agent");
        fs::remove_file(no_environ.join("environ")).unwrap();
        let unrelated = fake_process(proc, "203", "/bin/other-agent");
        fs::remove_file(unrelated.join("environ")).unwrap();
        fs::create_dir(unrelated.join("environ")).unwrap();
        assert_eq!(running_in(proc, uid, &scan_cap()), Ok(1));
    }
    #[test]
    fn cap_scan_fails_on_a_live_unreadable_matching_process() {
        let root = tempfile::tempdir_in("target").unwrap();
        let proc = root.path();
        let uid = fs::metadata(proc).unwrap().uid();
        let agent = fake_process(proc, "100", "/bin/fake-agent");
        fs::remove_file(agent.join("environ")).unwrap();
        fs::create_dir(agent.join("environ")).unwrap();
        assert_eq!(
            running_in(proc, uid, &scan_cap()),
            Err("cannot inspect matching process environment".into())
        );
        fs::remove_dir(agent.join("environ")).unwrap();
        fs::write(agent.join("environ"), "AGENT_HOME=/fictional/account\0").unwrap();
        fs::remove_file(agent.join("cwd")).unwrap();
        fs::write(agent.join("cwd"), "").unwrap();
        assert_eq!(
            running_in(proc, uid, &scan_cap()),
            Err("cannot inspect matching process working directory".into())
        );
    }
}
