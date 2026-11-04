use crate::config::{Backend, Cap, Config};
use crate::decision::Reading;
use crate::quota;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

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
    // GNU timeout bounds the command and its descendants, including blocked pipe writers.
    let mut cmd = Command::new("timeout");
    cmd.args(["--kill-after=2s", &format!("{seconds}s")])
        .args(argv);
    for key in &backend.unset_env {
        cmd.env_remove(key);
    }
    let output = cmd
        .envs(&backend.env)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| "could not execute check (requires GNU timeout)")?;
    if !output.status.success() {
        return Err(format!("check failed ({})", output.status));
    }
    String::from_utf8(output.stdout).map_err(|_| "check returned non-UTF-8 output".into())
}

fn canonical_equal(a: &str, b: &str) -> bool {
    a == b
        || fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

fn matches_process(cap: &Cap, argv: &[String], env: &BTreeMap<String, String>) -> bool {
    let executable = argv
        .first()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str());
    executable == Some(&cap.executable)
        && !argv.iter().any(|arg| cap.exclude_args.contains(arg))
        && cap
            .env
            .iter()
            .all(|(key, value)| env.get(key).is_some_and(|v| canonical_equal(v, value)))
}

fn split_null(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

pub fn running(cap: &Cap) -> Result<usize, String> {
    let mut count = 0;
    for entry in fs::read_dir("/proc").map_err(|_| "cannot scan /proc for concurrency cap")? {
        let entry = entry.map_err(|_| "cannot read process entry")?;
        if !entry
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let argv = split_null(&cmdline);
        if argv
            .first()
            .and_then(|s| Path::new(s).file_name())
            .and_then(|s| s.to_str())
            != Some(&cap.executable)
        {
            continue;
        }
        let env = if cap.env.is_empty() {
            BTreeMap::new()
        } else {
            match fs::read(entry.path().join("environ")) {
                Ok(bytes) => split_null(&bytes)
                    .into_iter()
                    .filter_map(|s| s.split_once('=').map(|(k, v)| (k.into(), v.into())))
                    .collect(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err("cannot inspect matching process environment".into()),
            }
        };
        if matches_process(cap, &argv, &env) {
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
        if let Some(cap) = &backend.cap {
            reading.running = running(cap)?;
        }
        if let Some(reader) = &backend.quota {
            let get = || {
                probe(backend, &reader.command, reader.timeout_seconds, cwd)
                    .and_then(|s| quota::parse(reader.parser, &s))
            };
            let mut quota = get();
            if quota.is_err()
                && allow_refresh
                && backend.cap.as_ref().is_none_or(|c| reading.running < c.max)
            {
                if let Some(refresh) = &reader.refresh {
                    let _ = probe(backend, refresh, reader.refresh_timeout_seconds, cwd);
                    quota = get();
                }
            }
            reading.remaining = Some(quota.map_err(|e| format!("quota unreadable: {e}"))?);
            reading.detail = reading
                .remaining
                .as_ref()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{k}_left={v}%"))
                .collect::<Vec<_>>()
                .join(" ");
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

pub fn collect(
    config: &Config,
    indices: &[usize],
    cwd: &Path,
    allow_refresh: bool,
) -> Vec<Reading> {
    config
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
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_caps_include_all_roles_and_exclude_usage() {
        let mut cap = Cap {
            max: 1,
            executable: "fake-agent".into(),
            env: BTreeMap::new(),
            exclude_args: vec!["/usage".into()],
        };
        let args = vec!["/bin/fake-agent".into(), "review anything".into()];
        assert!(matches_process(&cap, &args, &BTreeMap::new()));
        assert!(!matches_process(
            &cap,
            &["fake-agent".into(), "/usage".into()],
            &BTreeMap::new()
        ));
        cap.env
            .insert("AGENT_HOME".into(), "/fictional/account".into());
        assert!(!matches_process(&cap, &args, &BTreeMap::new()));
        assert!(matches_process(&cap, &args, &cap.env));
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
}
