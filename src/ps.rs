//! rota ps: the sessions in the registry, read-only.

use crate::config::{Config, Role};
use crate::launch::{Failure, io_failure};
use crate::run::now;
use crate::session::{self, Session};
use std::path::Path;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PsOptions {
    pub json: bool,
    pub all: bool,
}

pub fn parse(args: &[String]) -> Result<PsOptions, Failure> {
    let mut options = PsOptions::default();
    for arg in args {
        match arg.as_str() {
            "--json" => options.json = true,
            "--all" => options.all = true,
            _ => return Err((2, "unknown ps option".to_string())),
        }
    }
    Ok(options)
}

/// `finished` when the record has `finished`, else `running` or `dead` by `alive`.
pub fn state(session: &Session, alive: bool) -> &'static str {
    if session.finished.is_some() {
        return "finished";
    }
    if alive { "running" } else { "dead" }
}

/// A duration in its two largest units: `59s`, `3m20s`, `2h1m`, `1d1h`.
pub fn age(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m{}s", seconds / 60, seconds % 60);
    }
    if seconds < 86400 {
        return format!("{}h{}m", seconds / 3600, seconds % 3600 / 60);
    }
    format!("{}d{}h", seconds / 86400, seconds % 86400 / 3600)
}

/// The record as stored, with `state`, `alive` and `age_seconds` added.
pub fn view(session: &Session, alive: bool, now: u64) -> serde_json::Value {
    let mut value = serde_json::to_value(session).expect("a session serializes");
    let object = value.as_object_mut().unwrap();
    object.insert("state".into(), state(session, alive).into());
    object.insert("alive".into(), alive.into());
    object.insert(
        "age_seconds".into(),
        now.saturating_sub(session.started).into(),
    );
    value
}

/// One line of `rota ps` text output.
pub fn line(session: &Session, alive: bool, now: u64) -> String {
    let role = match session.role {
        Role::Implement => "implement",
        Role::Review => "review",
    };
    let log = session
        .paths
        .get("log")
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".into());
    let mut parts = vec![
        format!("{}:", session.id),
        state(session, alive).into(),
        format!("backend={}", session.backend),
        format!("role={role}"),
        format!("pid={}", session.pid),
        format!("age={}", age(now.saturating_sub(session.started))),
        format!("cwd={}", session.cwd.display()),
        format!("log={log}"),
    ];
    if let Some(pane) = session.extra.get("pane").and_then(|pane| pane.as_str()) {
        parts.push(format!("pane={pane}"));
    }
    if let Some(outcome) = &session.outcome {
        parts.push(format!("outcome={outcome}"));
    }
    if let Some(code) = session.exit_code {
        parts.push(format!("exit={code}"));
    }
    parts.join(" ")
}

pub fn run(config: &Config, options: &PsOptions) -> Result<(), Failure> {
    let (sessions, problems) = session::list(&config.state_file).map_err(io_failure)?;
    let now = now();
    let kept: Vec<_> = sessions
        .iter()
        .filter(|session| options.all || session.finished.is_none())
        .map(|session| {
            let alive = session::alive(Path::new("/proc"), session);
            (session, alive)
        })
        .collect();
    if options.json {
        let views: Vec<_> = kept
            .iter()
            .map(|(session, alive)| view(session, *alive, now))
            .collect();
        println!("{}", serde_json::to_string_pretty(&views).unwrap());
    } else if kept.is_empty() {
        println!("no sessions");
    } else {
        for (session, alive) in &kept {
            println!("{}", line(session, *alive, now));
        }
    }
    for problem in &problems {
        eprintln!("rota: {problem}");
    }
    if !problems.is_empty() {
        return Err((1, format!("unreadable session records: {}", problems.len())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = r#"{"schema":1,"id":"t-1","name":"t","kind":"run","backend":"b","role":"review",
        "cwd":"/fictional","brief":"/fictional/brief","pid":2,"rota_pid":1,"started":100,
        "paths":{"log":"/fictional/log"},"pane":"p-9"}"#;

    #[test]
    fn parse_accepts_json_and_all_in_any_order() {
        let to_args = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse(&to_args(&[])),
            Ok(PsOptions {
                json: false,
                all: false
            })
        );
        assert_eq!(
            parse(&to_args(&["--json"])),
            Ok(PsOptions {
                json: true,
                all: false
            })
        );
        assert_eq!(
            parse(&to_args(&["--all"])),
            Ok(PsOptions {
                json: false,
                all: true
            })
        );
        for args in [["--all", "--json"], ["--json", "--all"]] {
            assert_eq!(
                parse(&to_args(&args)),
                Ok(PsOptions {
                    json: true,
                    all: true
                })
            );
        }
        for args in [vec!["--jsn"], vec!["x"], vec!["--json", "--watch"]] {
            assert_eq!(
                parse(&to_args(&args)),
                Err((2, "unknown ps option".to_string()))
            );
        }
    }

    #[test]
    fn ages_use_two_units() {
        assert_eq!(age(0), "0s");
        assert_eq!(age(59), "59s");
        assert_eq!(age(60), "1m0s");
        assert_eq!(age(200), "3m20s");
        assert_eq!(age(3599), "59m59s");
        assert_eq!(age(3600), "1h0m");
        assert_eq!(age(7300), "2h1m");
        assert_eq!(age(86399), "23h59m");
        assert_eq!(age(86400), "1d0h");
        assert_eq!(age(90061), "1d1h");
    }

    #[test]
    fn view_adds_state_alive_and_age() {
        let mut s: Session = serde_json::from_str(TEXT).unwrap();
        let v = view(&s, true, 105);
        assert_eq!(v["state"], "running");
        assert_eq!(v["alive"], true);
        assert_eq!(v["age_seconds"], 5);
        assert_eq!(v["pane"], "p-9");
        assert_eq!(v["paths"]["log"], "/fictional/log");
        assert!(v.get("finished").is_none());
        assert_eq!(view(&s, false, 105)["state"], "dead");
        assert_eq!(view(&s, true, 50)["age_seconds"], 0);
        s.finished = Some(200);
        s.outcome = Some("exited".into());
        s.exit_code = Some(0);
        let v = view(&s, true, 300);
        assert_eq!(v["state"], "finished");
        assert_eq!(v["alive"], true);
        assert_eq!(v["finished"], 200);
    }

    #[test]
    fn line_has_every_part_in_order() {
        let mut s: Session = serde_json::from_str(TEXT).unwrap();
        assert_eq!(
            line(&s, true, 3700),
            "t-1: running backend=b role=review pid=2 age=1h0m cwd=/fictional log=/fictional/log pane=p-9"
        );
        s.paths.clear();
        s.extra.clear();
        s.finished = Some(200);
        s.outcome = Some("timeout".into());
        s.exit_code = Some(124);
        assert_eq!(
            line(&s, false, 160),
            "t-1: finished backend=b role=review pid=2 age=1m0s cwd=/fictional log=- outcome=timeout exit=124"
        );
    }
}
