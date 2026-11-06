use crate::config::{Backend, Config, Level, Role};
use crate::{decision, readings};
use fs2::FileExt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Failure = (u8, String);

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub role: Role,
    pub level: Level,
    pub only: Option<String>,
    pub dry_run: bool,
    pub prompt_file: PathBuf,
}

pub fn parse(args: &[String]) -> Result<Options, Failure> {
    let bad = |message: &str| (2, message.to_string());
    let mut options = Options {
        role: Role::Implement,
        level: Level::High,
        only: None,
        dry_run: false,
        prompt_file: PathBuf::new(),
    };
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        match arg.as_str() {
            "--role" | "--level" | "--only" => {
                i += 1;
                let value = args.get(i).ok_or_else(|| bad("option needs a value"))?;
                match arg.as_str() {
                    "--role" => {
                        options.role = match value.as_str() {
                            "implement" => Role::Implement,
                            "review" => Role::Review,
                            _ => return Err(bad("bad --role")),
                        }
                    }
                    "--level" => {
                        options.level = match value.as_str() {
                            "medium" => Level::Medium,
                            "high" => Level::High,
                            _ => return Err(bad("bad --level")),
                        }
                    }
                    _ => options.only = Some(value.clone()),
                }
            }
            "--dry-run" => options.dry_run = true,
            "--" => {
                i += 1;
                break;
            }
            s if s.starts_with('-') => return Err(bad("unknown launch option")),
            _ => break,
        }
        i += 1;
    }
    if args.len() != i + 1 {
        return Err(bad("launch needs exactly one prompt file"));
    }
    options.prompt_file = PathBuf::from(&args[i]);
    Ok(options)
}

pub fn ensure_parent(path: &Path) -> std::io::Result<&Path> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    Ok(parent)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = ensure_parent(path)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    match fs::metadata(path) {
        Ok(metadata) => temp.as_file().set_permissions(metadata.permissions())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e),
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()
}

pub fn trust_folder(path: &Path, cwd: &Path) -> Result<(), String> {
    let mut json: serde_json::Value = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| "invalid trusted settings JSON")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e.to_string()),
    };
    let object = json
        .as_object_mut()
        .ok_or("trusted settings must be a JSON object")?;
    let trusted = object
        .entry("trustedWorkspaces")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or("trustedWorkspaces must be an array")?;
    if trusted.iter().any(|v| v.as_str().is_none()) {
        return Err("trustedWorkspaces entries must be strings".into());
    }
    let folder = cwd.to_str().ok_or("working directory must be UTF-8")?;
    if !trusted.iter().any(|v| v.as_str() == Some(folder)) {
        trusted.push(folder.into());
        let mut bytes = serde_json::to_vec_pretty(&json).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        atomic_write(path, &bytes).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-/.=:".contains(c))
    {
        text.into()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

pub fn display_command(backend: &Backend, level: Level, cwd: &str, prompt_file: &str) -> String {
    let mut args = Vec::new();
    if !backend.env.is_empty() || !backend.unset_env.is_empty() {
        args.push("env".into());
        for key in &backend.unset_env {
            args.extend(["-u".into(), shell_quote(key)]);
        }
        for (key, value) in &backend.env {
            args.push(shell_quote(&format!("{key}={value}")));
        }
    }
    for arg in &backend.commands[&level] {
        args.push(match arg.as_str() {
            "{prompt}" => format!("\"$(cat {})\"", shell_quote(prompt_file)),
            "{cwd}" => shell_quote(cwd),
            _ => shell_quote(arg),
        });
    }
    args.join(" ")
}

pub fn log(
    config: &Config,
    role: Role,
    level: Option<Level>,
    cwd: &Path,
    readings: &[decision::Reading],
    pick: Option<&decision::Pick>,
) -> Result<(), Failure> {
    ensure_parent(&config.log_file).map_err(io_failure)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&config.log_file)
        .map_err(io_failure)?;
    let entry = serde_json::json!({
        "time": SystemTime::now().duration_since(UNIX_EPOCH).map_err(io_failure)?.as_secs(),
        "folder": cwd, "role": role, "level": level,
        "pick": pick.map(|p| &config.backends[p.index].name),
        "decision": match pick { Some(p) if p.fallback => "FALLBACK", Some(_) => "pick", None => "REFUSAL" },
        "readings": config.backends.iter().zip(readings).map(|(b,r)| serde_json::json!({"backend": b.name, "reading": r, "availability": decision::availability(b,r,role).err()})).collect::<Vec<_>>()
    });
    writeln!(file, "{entry}").map_err(io_failure)?;
    file.sync_data().map_err(io_failure)
}

pub fn io_failure(error: impl std::fmt::Display) -> Failure {
    (1, error.to_string())
}

/// Serializes rota's backend selection; the descriptor closes on exec.
pub fn lock(config: &Config, wait: bool) -> Result<File, Failure> {
    let mut lock_path = config.state_file.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    ensure_parent(&lock_path).map_err(io_failure)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)
        .map_err(io_failure)?;
    let locked = if wait {
        lock.lock_exclusive()
    } else {
        lock.try_lock_exclusive()
    };
    locked.map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            (3, "another rota command is selecting a backend".into())
        } else {
            io_failure(e)
        }
    })?;
    Ok(lock)
}

pub fn print_readings(
    config: &Config,
    indices: &[usize],
    readings: &[decision::Reading],
    role: Role,
) {
    for index in indices {
        let b = &config.backends[*index];
        let r = &readings[*index];
        eprintln!(
            "rota: {} [{}]: {}",
            b.name,
            decision::availability(b, r, role)
                .err()
                .unwrap_or_else(|| "available".into()),
            r.detail
        );
    }
}

pub fn run(config: &Config, options: &Options) -> Result<(), Failure> {
    let prompt_file = fs::canonicalize(&options.prompt_file)
        .map_err(|e| (2, format!("cannot read prompt file: {e}")))?;
    let prompt = fs::read_to_string(&prompt_file)
        .map_err(|e| (2, format!("cannot read prompt file: {e}")))?;
    if prompt.contains('\0') {
        return Err((2, "prompt contains a NUL byte".into()));
    }
    let cwd = std::env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(io_failure)?;
    let cwd_text = cwd
        .to_str()
        .ok_or_else(|| (2, "working directory must be UTF-8".into()))?;
    let prompt_path = prompt_file
        .to_str()
        .ok_or_else(|| (2, "prompt path must be UTF-8".into()))?;
    let mut indices = decision::candidates(config, options.role, options.only.as_deref())
        .map_err(|e| (e.code, e.message))?;
    if options.role == Role::Implement && options.only.is_none() {
        if let Some(index) = config
            .backends
            .iter()
            .position(|b| Some(&b.name) == config.fallback.as_ref())
        {
            indices.push(index);
        }
    }
    // Serialize rota launches from scan through exec.
    let _lock = if options.dry_run {
        None
    } else {
        Some(lock(config, false)?)
    };
    let last = match fs::read_to_string(&config.state_file) {
        Ok(last) => Some(last),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_failure(e)),
    };
    let mut readings = readings::collect(config, &indices, &cwd, !options.dry_run);
    let result = decision::choose(
        config,
        &readings,
        options.role,
        options.only.as_deref(),
        last.as_deref().map(str::trim),
    );
    if !options.dry_run {
        let level = Some(options.level);
        log(
            config,
            options.role,
            level,
            &cwd,
            &readings,
            result.as_ref().ok(),
        )?;
    }
    print_readings(config, &indices, &readings, options.role);
    let pick = result.map_err(|e| (e.code, e.message))?;
    let backend = &config.backends[pick.index];
    if pick.fallback {
        eprintln!(
            "rota: FALLBACK {} (no normal backend available)",
            backend.name
        );
    }
    println!("pick: {}", backend.name);
    if options.dry_run {
        println!(
            "command: {}",
            display_command(backend, options.level, cwd_text, prompt_path)
        );
        return Ok(());
    }
    if let Some(path) = &backend.trusted_settings {
        trust_folder(path, &cwd).map_err(io_failure)?;
    }
    for prepare in &backend.prepare {
        readings::probe(backend, prepare, 30, &cwd).map_err(io_failure)?;
    }
    let argv: Vec<_> = backend.commands[&options.level]
        .iter()
        .map(|a| match a.as_str() {
            "{prompt}" => prompt.clone(),
            "{cwd}" => cwd_text.into(),
            _ => a.clone(),
        })
        .collect();
    if let Err(problem) = readings::recheck_cap(backend, &mut readings[pick.index]) {
        log(
            config,
            options.role,
            Some(options.level),
            &cwd,
            &readings,
            None,
        )?;
        return Err((3, problem));
    }
    atomic_write(&config.state_file, format!("{}\n", backend.name).as_bytes())
        .map_err(io_failure)?;
    let error = readings::command(backend, &argv).current_dir(&cwd).exec();
    Err(io_failure(format!(
        "could not exec {}: {error}",
        backend.name
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn launch_arguments() {
        let args = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let parsed = parse(&args(&[
            "--role",
            "review",
            "--level",
            "medium",
            "--only",
            "alpha",
            "--dry-run",
            "task",
        ]))
        .unwrap();
        assert_eq!(parsed.role, Role::Review);
        assert_eq!(parsed.level, Level::Medium);
        assert!(parsed.dry_run);
        for args in [
            vec![],
            vec!["--role"],
            vec!["--role", "bad", "task"],
            vec!["--level", "bad", "task"],
            vec!["--wat", "task"],
            vec!["one", "two"],
        ] {
            assert_eq!(
                parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
                    .unwrap_err()
                    .0,
                2
            );
        }
    }
    #[test]
    fn atomic_trust_keeps_keys_mode_and_rejects_corrupt_data() {
        let dir = tempfile::tempdir_in("target").unwrap();
        let path = dir.path().join("settings.json");
        fs::write(
            &path,
            r#"{"other":{"keep":true},"trustedWorkspaces":["/fictional/old"]}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        trust_folder(&path, Path::new("/fictional/new")).unwrap();
        trust_folder(&path, Path::new("/fictional/new")).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["other"]["keep"], true);
        assert_eq!(json["trustedWorkspaces"].as_array().unwrap().len(), 2);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::write(&path, "bad").unwrap();
        assert!(trust_folder(&path, Path::new("/fictional/new")).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "bad");
        let state = dir.path().join("nested/state");
        atomic_write(&state, b"alpha\n").unwrap();
        atomic_write(&state, b"secondary\n").unwrap();
        assert_eq!(fs::read(&state).unwrap(), b"secondary\n");
        assert_eq!(
            fs::metadata(&state).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn renders_shell_arguments_without_prompt_content() {
        let config = Config::parse(include_str!("../config.example.toml")).unwrap();
        let command = display_command(
            &config.backends[3],
            Level::High,
            "/fictional/folder",
            "/fictional/task's file",
        );
        assert!(command.contains("'PROVIDER/MODEL#HIGH'"));
        assert!(command.contains("\"$(cat '/fictional/task'\\''s file')\""));
        assert!(
            display_command(&config.backends[4], Level::High, ".", "task")
                .starts_with("env -u CLAUDE_CONFIG_DIR claude")
        );
    }
}
