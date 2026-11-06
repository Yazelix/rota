use crate::config::{Backend, Config, Role, Run, RunEnv, Stdin};
use crate::launch::{Failure, atomic_write, io_failure, lock, log, print_readings};
use crate::session::{self, SCHEMA, Session};
use crate::{decision, readings};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long rota waits after SIGTERM (`grace`), then keeps sending SIGKILL (`kill`), before
/// it reports the processes still in the run's scope as abandoned.
pub struct Phases {
    pub grace: Duration,
    pub kill: Duration,
}
const PHASES: Phases = Phases {
    grace: Duration::from_secs(5),
    kill: Duration::from_secs(10),
};
/// The longest `--timeout`: 30 days.
const MAX_TIMEOUT: u64 = 30 * 24 * 3600;
/// Exit status when rota cannot confirm that the run's processes ended.
const ABANDONED: u8 = 125;
/// The longest rota waits for the start-up check.
const PROBE: Duration = Duration::from_secs(10);
/// How rota reaches the systemd user manager. A run keeps rota's own values of these.
pub const MANAGER_ENV: [&str; 2] = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"];

#[derive(Debug, PartialEq, Eq)]
pub struct RunOptions {
    pub role: Role,
    pub only: Option<String>,
    pub cwd: Option<PathBuf>,
    pub brief: PathBuf,
    pub effort: Option<String>,
    pub timeout: u64,
    pub name: String,
    pub add_dirs: Vec<PathBuf>,
}

pub fn parse_timeout(text: &str) -> Option<u64> {
    let (digits, unit) = match text.strip_suffix(['s', 'm', 'h']) {
        Some(digits) => (digits, &text[digits.len()..]),
        None => (text, "s"),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        _ => 3600,
    };
    digits
        .parse::<u64>()
        .ok()?
        .checked_mul(scale)
        .filter(|s| (1..=MAX_TIMEOUT).contains(s))
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

pub fn parse(args: &[String]) -> Result<RunOptions, Failure> {
    let bad = |message: &str| (2, message.to_string());
    let mut role = Role::Implement;
    let (mut only, mut cwd, mut brief, mut effort, mut timeout, mut name) =
        (None, None, None, None, None, None);
    let mut add_dirs = Vec::new();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        let flags = [
            "--role",
            "--only",
            "--cwd",
            "--brief",
            "--effort",
            "--timeout",
            "--name",
            "--add-dir",
        ];
        if !flags.contains(&arg.as_str()) {
            return Err(bad("unknown run option"));
        }
        let value = args.get(i + 1).ok_or_else(|| bad("option needs a value"))?;
        match arg.as_str() {
            "--role" => {
                role = match value.as_str() {
                    "implement" => Role::Implement,
                    "review" => Role::Review,
                    _ => return Err(bad("bad --role")),
                }
            }
            "--only" => only = Some(value.clone()),
            "--cwd" => cwd = Some(PathBuf::from(value)),
            "--brief" => brief = Some(PathBuf::from(value)),
            "--effort" => effort = Some(value.clone()),
            "--timeout" => {
                timeout = Some(parse_timeout(value).ok_or_else(|| {
                    bad("bad --timeout (use e.g. 5400, 5400s, 90m or 2h; at most 30 days)")
                })?)
            }
            "--name" => {
                if !valid_name(value) {
                    return Err(bad(
                        "bad --name (use 1-64 letters, digits, '.', '_' or '-', not starting with '.' or '-')",
                    ));
                }
                name = Some(value.clone());
            }
            _ => add_dirs.push(PathBuf::from(value)),
        }
        i += 2;
    }
    let (Some(brief), Some(timeout), Some(name)) = (brief, timeout, name) else {
        return Err(bad(
            "run needs --brief FILE, --name NAME and --timeout DURATION",
        ));
    };
    Ok(RunOptions {
        role,
        only,
        cwd,
        brief,
        effort,
        timeout,
        name,
        add_dirs,
    })
}

/// Replaces each `{name}` in one left-to-right pass, so substituted text is never rescanned.
pub fn fill(arg: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::new();
    let mut rest = arg;
    'scan: while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        for (name, value) in values {
            if let Some(after) = rest.strip_prefix(name) {
                out.push_str(value);
                rest = after;
                continue 'scan;
            }
        }
        out.push('{');
        rest = &rest[1..];
    }
    out.push_str(rest);
    out
}

pub fn argv(run: &Run, role: Role, values: &[(&str, &str)], add_dirs: &[&str]) -> Vec<String> {
    let mut argv = Vec::new();
    for arg in &run.commands[&role] {
        if arg != "{add_dirs}" {
            argv.push(fill(arg, values));
            continue;
        }
        for dir in add_dirs {
            for part in run.add_dir.iter().flatten() {
                argv.push(if part == "{dir}" {
                    dir.to_string()
                } else {
                    part.clone()
                });
            }
        }
    }
    argv
}

/// Why `backend` cannot take this run, or `None` when it can.
pub fn unsupported(backend: &Backend, options: &RunOptions) -> Option<String> {
    let Some(run) = &backend.run else {
        return Some("no [backends.run] table".into());
    };
    let role = match options.role {
        Role::Implement => "implement",
        Role::Review => "review",
    };
    let Some(command) = run.commands.get(&options.role) else {
        return Some(format!("no headless command for role {role}"));
    };
    if let Some(effort) = &options.effort {
        if !run.efforts.contains(effort) {
            return Some(format!("effort {effort} is not in efforts"));
        }
    }
    if !options.add_dirs.is_empty() && !command.iter().any(|a| a == "{add_dirs}") {
        return Some(format!("the {role} command takes no --add-dir"));
    }
    None
}

pub fn scrubbed(settings: &RunEnv, key: &str) -> bool {
    settings
        .unset_env
        .iter()
        .any(|p| match p.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == p,
        })
}

/// Inherited environment minus `[run] unset_env`, then `[run.env]`, then the backend's own;
/// `MANAGER_ENV` always as rota has it, so launch and cleanup reach the same manager.
pub fn command(settings: &RunEnv, backend: &Backend, argv: &[String]) -> Command {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).env_clear();
    cmd.envs(std::env::vars_os().filter(|(k, _)| !scrubbed(settings, &k.to_string_lossy())));
    cmd.envs(&settings.env);
    for key in &backend.unset_env {
        cmd.env_remove(key);
    }
    cmd.envs(&backend.env);
    for key in MANAGER_ENV {
        match std::env::var_os(key) {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    cmd
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Whether `leader` has exited, leaving it unreaped so its status is kept. A leader whose
/// main thread exited while other threads run has not exited yet.
fn leader_exited(leader: i32) -> bool {
    // SAFETY: waitid writes only into `info`; WNOWAIT leaves the child waitable.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    let found = unsafe { libc::waitid(libc::P_PID, leader as libc::id_t, &mut info, flags) };
    found == 0 && unsafe { info.si_pid() } == leader
}

/// Runs `cmd` with stdin and stdout closed. If it has not finished within `limit`, `kill`
/// ends it and rota waits up to `REAP` for it to exit, then gives up on it either way. Its
/// stderr goes to a temporary file, so a process it leaves behind cannot hold rota up.
fn bounded(
    cmd: &mut Command,
    limit: Duration,
    kill: fn(&mut std::process::Child),
) -> Result<(ExitStatus, String), String> {
    let mut stderr = tempfile::tempfile().map_err(|e| e.to_string())?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr.try_clone().map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| e.to_string())?;
    let until = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::io::{Read, Seek};
                let mut text = String::new();
                let _ = stderr.rewind();
                let _ = stderr.read_to_string(&mut text);
                return Ok((status, text));
            }
            Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(10)),
            _ => {
                kill(&mut child);
                let ms = limit.as_millis();
                return Err(match readings::reap_within(&mut child, readings::REAP) {
                    true => format!("no answer within {ms} ms"),
                    false => format!("no answer within {ms} ms, and still running after SIGKILL"),
                });
            }
        }
    }
}

/// Sends `signal` to every process in the scope. Each PID read from `cgroup.procs` is pinned
/// with a pidfd and then checked to be alive and still in the scope, so a reused PID is never
/// signalled. This needs no systemd manager, and unlike `cgroup.kill` it also ends a process
/// whose main thread has exited while other threads run. SIGKILL also writes `cgroup.kill`,
/// which kills processes forked meanwhile.
pub fn signal_scope(scope: &Scope, signal: libc::c_int) {
    if let (libc::SIGKILL, Some(cgroup)) = (signal, &scope.cgroup) {
        let _ = fs::write(cgroup.join("cgroup.kill"), "1");
    }
    for pid in scope.pids() {
        // SAFETY: pidfd_open takes a PID and flags and returns a new descriptor or -1.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            continue;
        }
        // SAFETY: the descriptor is new and owned by nothing else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        let send = |signal: libc::c_int| {
            let none = std::ptr::null::<libc::siginfo_t>();
            // SAFETY: pidfd_send_signal reads only its arguments.
            unsafe {
                libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), signal, none, 0) == 0
            }
        };
        if scope.holds(pid) && send(0) {
            send(signal);
        }
    }
}

/// What `cgroup.events` says about the scope. A missing cgroup is empty: the kernel removes a
/// cgroup only once it is empty. Anything unreadable is unknown, never empty.
#[derive(Debug, PartialEq, Eq)]
enum Population {
    Empty,
    Populated,
    Unknown,
}

/// The run's transient systemd user scope. Its cgroup holds every process the run starts,
/// whatever their session, process group, parent or threads.
pub struct Scope {
    pub unit: String,
    /// The cgroup path below `/sys/fs/cgroup`, once bound.
    path: Option<String>,
    cgroup: Option<PathBuf>,
    /// Where process cgroups are read from: `/proc`, or a stand-in in tests.
    proc_dir: PathBuf,
}

/// Whether the run's leader is in the unit's cgroup, as far as rota can tell.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Joined {
    Yes,
    /// The leader's cgroup was read and is not the unit's.
    No,
    /// The leader's cgroup could not be read.
    Unknown,
}

/// The cgroup-v2 path of `pid` (a removed cgroup shows as `PATH (deleted)`), or `None` when
/// it cannot be read.
pub(crate) fn cgroup_of(proc_dir: &Path, pid: i32) -> Option<String> {
    let cgroups = fs::read_to_string(proc_dir.join(format!("{pid}/cgroup"))).ok()?;
    let path = cgroups.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(path.strip_suffix(" (deleted)").unwrap_or(path).to_string())
}

impl Scope {
    pub fn new(unit: String) -> Scope {
        Scope {
            unit,
            path: None,
            cgroup: None,
            proc_dir: PathBuf::from("/proc"),
        }
    }

    /// Binds to the unit's cgroup once `leader` (alive or a zombie) is in it. That proves this
    /// run's systemd-run created the unit: if the name was taken, systemd-run fails before
    /// joining any scope, and rota never signals that unit.
    fn bind(&mut self, leader: i32) -> Joined {
        if self.cgroup.is_some() {
            return Joined::Yes;
        }
        let Some(path) = cgroup_of(&self.proc_dir, leader) else {
            return Joined::Unknown;
        };
        if !path.ends_with(&format!("/{}", self.unit)) {
            return Joined::No;
        }
        self.cgroup = Some(Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/')));
        self.path = Some(path);
        Joined::Yes
    }

    /// Whether `pid` is in the scope or one of its child cgroups.
    fn holds(&self, pid: i32) -> bool {
        let (Some(path), Some(of)) = (&self.path, cgroup_of(&self.proc_dir, pid)) else {
            return false;
        };
        of == *path || of.starts_with(&format!("{path}/"))
    }

    fn population(&self) -> Population {
        let Some(cgroup) = &self.cgroup else {
            return Population::Unknown;
        };
        match fs::read_to_string(cgroup.join("cgroup.events")) {
            Ok(events) if events.lines().any(|line| line == "populated 0") => Population::Empty,
            Ok(events) if events.lines().any(|line| line == "populated 1") => Population::Populated,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Population::Empty,
            _ => Population::Unknown,
        }
    }

    /// The processes in the scope and its child cgroups, for the record only.
    fn pids(&self) -> Vec<i32> {
        let mut pids = Vec::new();
        let mut dirs: Vec<PathBuf> = self.cgroup.iter().cloned().collect();
        while let Some(dir) = dirs.pop() {
            let procs = fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default();
            pids.extend(procs.lines().filter_map(|line| line.parse::<i32>().ok()));
            let entries = fs::read_dir(&dir).into_iter().flatten().flatten();
            dirs.extend(entries.map(|e| e.path()).filter(|p| p.is_dir()));
        }
        pids
    }
}

/// Stops the run within `phases`: SIGTERM and SIGCONT once to the whole scope, up to
/// `phases.grace` for the leader to exit and the scope to empty, then SIGKILL every 50 ms for
/// up to `phases.kill`. Signals go straight to the scope's processes, never through the
/// systemd manager, so a stalled manager cannot delay them. Until the
/// leader is bound to the scope (systemd-run still creating it), rota signals nothing but, in
/// the kill phase, the leader itself: rota's unreaped child, whose PID cannot be reused.
/// `Ok` when the run is confirmed over: the leader exited and the scope is empty, or the
/// exited leader's cgroup was read and is not the unit's (nothing ran in it). Otherwise `Err`
/// with the PIDs still seen, which may be none when the scope or the leader's cgroup cannot be
/// read.
fn stop(
    scope: &mut Scope,
    leader: i32,
    phases: &Phases,
    signal: fn(&Scope, libc::c_int),
) -> Result<(), Vec<i32>> {
    let term_until = Instant::now() + phases.grace;
    let kill_until = term_until + phases.kill;
    let mut termed = false;
    loop {
        let leader_alive = !leader_exited(leader);
        let joined = scope.bind(leader);
        let bound = joined == Joined::Yes;
        let over = match joined {
            Joined::Yes => scope.population() == Population::Empty,
            Joined::No => true,
            Joined::Unknown => false,
        };
        if !leader_alive && over {
            return Ok(());
        }
        let now = Instant::now();
        if now >= kill_until {
            let mut left = scope.pids();
            if leader_alive && !left.contains(&leader) {
                left.push(leader);
            }
            return Err(left);
        }
        if now >= term_until {
            if bound {
                signal(scope, libc::SIGKILL);
            } else if leader_alive {
                unsafe {
                    libc::kill(leader, libc::SIGKILL);
                }
            }
        } else if bound && !termed {
            termed = true;
            signal(scope, libc::SIGTERM);
            signal(scope, libc::SIGCONT);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

type Verdict = (&'static str, u8, String);

/// The outcome, exit status and message of a finished run. Abandonment always wins: rota
/// never reports success unless it confirmed that every process of the run ended.
fn verdict(
    stopped: Option<Verdict>,
    abandoned: Option<&[i32]>,
    status: Option<ExitStatus>,
) -> Verdict {
    if let Some(left) = abandoned {
        let pids: Vec<String> = left.iter().map(i32::to_string).collect();
        let message = match pids.is_empty() {
            true => "could not confirm that the run's processes ended".to_string(),
            false => format!("left processes running after SIGKILL: {}", pids.join(" ")),
        };
        return ("abandoned", ABANDONED, message);
    }
    if let Some(stopped) = stopped {
        return stopped;
    }
    match status.map(|s| (s.code(), s.signal())) {
        Some((Some(code), _)) => ("exited", code as u8, format!("exited with status {code}")),
        Some((None, Some(signal))) => (
            "signaled",
            128 + signal as u8,
            format!("killed by signal {signal}"),
        ),
        _ => ("exited", 1, "ended with an unknown status".into()),
    }
}

/// `argv` run by systemd-run in a new transient user scope, named `unit` if given. Arguments
/// are passed literally (systemd 258 expands `$VAR` in scope arguments by default).
fn scoped(unit: Option<&str>, argv: Vec<String>) -> Vec<String> {
    let mut wrapped: Vec<String> = [
        "systemd-run",
        "--user",
        "--scope",
        "--quiet",
        "--collect",
        "--expand-environment=no",
    ]
    .map(String::from)
    .into();
    wrapped.extend(unit.map(|unit| format!("--unit={unit}")));
    wrapped.push("--".into());
    wrapped.extend(argv);
    wrapped
}

/// 128 random bits from the kernel, as hex, so no other unit has this run's name.
fn random() -> Result<String, Failure> {
    let mut bytes = [0u8; 16];
    // SAFETY: getrandom writes at most `bytes.len()` bytes into `bytes`.
    let filled = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if filled != bytes.len() as isize {
        return Err(io_failure(std::io::Error::last_os_error()));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Checks from inside a throwaway scope that the manager answers and that the scope's cgroup
/// is cgroup v2 with readable `cgroup.events` and writable `cgroup.kill`.
const PROBE_SCRIPT: &str = r#"cg=$(sed -n 's/^0:://p' /proc/self/cgroup)
test -n "$cg" && test -r "/sys/fs/cgroup$cg/cgroup.events" && test -w "/sys/fs/cgroup$cg/cgroup.kill""#;

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn utf8(path: &Path) -> Result<&str, Failure> {
    path.to_str()
        .ok_or_else(|| (2, "paths must be UTF-8".into()))
}

pub fn run(config: &Config, options: &RunOptions) -> Result<(), Failure> {
    let input = |what: &str, e: std::io::Error| (2, format!("cannot use {what}: {e}"));
    let brief = fs::canonicalize(&options.brief).map_err(|e| input("brief file", e))?;
    if !brief.is_file() {
        return Err((2, "brief must be a file".into()));
    }
    let brief_file = File::open(&brief).map_err(|e| input("brief file", e))?;
    let cwd = match &options.cwd {
        Some(dir) => fs::canonicalize(dir).map_err(|e| input("--cwd", e))?,
        None => std::env::current_dir()
            .and_then(fs::canonicalize)
            .map_err(io_failure)?,
    };
    if !cwd.is_dir() {
        return Err((2, "--cwd must be a directory".into()));
    }
    let mut add_dirs = Vec::new();
    for dir in &options.add_dirs {
        let dir = fs::canonicalize(dir).map_err(|e| input("--add-dir", e))?;
        if !dir.is_dir() {
            return Err((2, "--add-dir must be a directory".into()));
        }
        add_dirs.push(dir);
    }
    let indices: Vec<usize> = match &options.only {
        Some(name) => {
            let index = config
                .backends
                .iter()
                .position(|b| &b.name == name)
                .ok_or_else(|| (2, format!("--only {name} is not a configured backend")))?;
            if let Some(reason) = unsupported(&config.backends[index], options) {
                return Err((2, format!("{name}: {reason}")));
            }
            vec![index]
        }
        None => (0..config.backends.len())
            .filter(|i| unsupported(&config.backends[*i], options).is_none())
            .collect(),
    };
    if indices.is_empty() {
        return Err((
            2,
            "no backend has a headless command for these options".into(),
        ));
    }
    // Wait for other rota selections instead of failing a batch of runs.
    let lock = lock(config, true)?;
    let last = match fs::read_to_string(&config.state_file) {
        Ok(last) => Some(last),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_failure(e)),
    };
    let last = last.as_deref().map(str::trim);
    let mut readings = readings::collect(config, &indices, &cwd, true);
    let pick = decision::rotate(config, &indices, &readings, options.role, last).map(|index| {
        decision::Pick {
            index,
            fallback: false,
        }
    });
    log(config, options.role, None, &cwd, &readings, pick.as_ref())?;
    print_readings(config, &indices, &readings, options.role);
    let Some(pick) = pick else {
        return Err((3, "no backend available, refusing".into()));
    };
    let backend = &config.backends[pick.index];
    let run = backend.run.as_ref().expect("candidates have a run table");
    println!("pick: {}", backend.name);
    for prepare in &backend.prepare {
        readings::probe(backend, prepare, 30, &cwd).map_err(io_failure)?;
    }
    if let Err(problem) = readings::recheck_cap(backend, &mut readings[pick.index]) {
        log(config, options.role, None, &cwd, &readings, None)?;
        return Err((3, problem));
    }
    // The run gets its own scope, so rota can stop and wait for every process it starts.
    let probe = ["sh", "-c", PROBE_SCRIPT].map(String::from).to_vec();
    let probe = bounded(
        &mut command(&config.run, backend, &scoped(None, probe)),
        PROBE,
        |child| {
            let _ = child.kill();
        },
    );
    if let Some(detail) = match &probe {
        Ok((status, _)) if status.success() => None,
        Ok((status, stderr)) => Some(format!("{status}, {}", stderr.trim())),
        Err(e) => Some(e.clone()),
    } {
        let needs = "systemd 254 or newer and cgroup v2 with cgroup.kill (Linux 5.14 or newer)";
        let message = format!("rota run needs a systemd user manager, {needs} ({detail})");
        return Err((1, message));
    }
    atomic_write(&config.state_file, format!("{}\n", backend.name).as_bytes())
        .map_err(io_failure)?;
    let (id, run_dir) = session::claim(&config.state_file, &options.name).map_err(io_failure)?;
    let run_dir = fs::canonicalize(&run_dir).map_err(io_failure)?;
    let log_path = run_dir.join("log");
    let report = run_dir.join("report.md");
    let effort = options.effort.clone().unwrap_or(run.default_effort.clone());
    let dirs: Vec<&str> = add_dirs.iter().map(|d| utf8(d)).collect::<Result<_, _>>()?;
    let values = [
        ("{brief}", utf8(&brief)?),
        ("{cwd}", utf8(&cwd)?),
        ("{report}", utf8(&report)?),
        ("{effort}", effort.as_str()),
    ];
    let unit = format!("rota-{id}-{}.scope", random()?);
    let argv = scoped(Some(&unit), argv(run, options.role, &values, &dirs));
    let mut scope = Scope::new(unit.clone());
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(io_failure)?;
    let stdin = match run.stdin {
        Stdin::Brief => Stdio::from(brief_file),
        Stdin::Null => Stdio::null(),
    };
    // Taken before spawn, so the first record write never waits while the run is going.
    let registry = session::lock(&config.state_file).map_err(io_failure)?;
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // The handler only sets an atomic flag, which is async-signal-safe.
        unsafe {
            libc::signal(signal, request_stop as *const () as libc::sighandler_t);
        }
    }
    let spawned = command(&config.run, backend, &argv)
        .current_dir(&cwd)
        .process_group(0)
        .stdin(stdin)
        .stdout(output.try_clone().map_err(io_failure)?)
        .stderr(output)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            let _ = fs::remove_dir_all(&run_dir);
            return Err((1, format!("could not start {}: {e}", backend.name)));
        }
    };
    let deadline = Instant::now() + Duration::from_secs(options.timeout);
    let leader = child.id() as i32;
    let record = Session {
        schema: SCHEMA,
        id: id.clone(),
        name: options.name.clone(),
        kind: "run".into(),
        backend: backend.name.clone(),
        role: options.role,
        effort: Some(effort),
        cwd: cwd.clone(),
        brief,
        pid: child.id(),
        rota_pid: std::process::id(),
        started: now(),
        timeout_seconds: Some(options.timeout),
        paths: [
            ("run_dir".to_string(), run_dir),
            ("log".to_string(), log_path.clone()),
            ("report".to_string(), report.clone()),
        ]
        .into(),
        finished: None,
        outcome: None,
        exit_code: None,
        unit: Some(unit.clone()),
        extra: Default::default(),
    };
    if let Err(e) = session::write(&registry, &config.state_file, &record) {
        let stopped = stop(&mut scope, leader, &PHASES, signal_scope);
        if leader_exited(leader) {
            let _ = child.wait();
        }
        let problem = format!("{id} cannot write session record: {e}");
        let Err(left) = stopped else {
            return Err((1, problem));
        };
        let (_, code, message) = verdict(None, Some(&left), None);
        return Err((code, format!("{problem}; {message} (unit {unit})")));
    }
    drop(registry);
    drop(lock);
    // From here on a closed stdout must not unwind past the live run.
    let _ = writeln!(
        std::io::stdout(),
        "session: {id}\npid: {leader}\nlog: {}\nreport: {}",
        log_path.display(),
        report.display()
    );
    let stopped = loop {
        if leader_exited(leader) {
            break None;
        }
        if STOP.load(Ordering::SeqCst) {
            break Some(("interrupted", 130, "interrupted; run stopped".to_string()));
        }
        if Instant::now() >= deadline {
            let message = format!("timed out after {}s", options.timeout);
            break Some(("timeout", 124, message));
        }
        thread::sleep(Duration::from_millis(100));
    };
    // Also after a normal exit: no worker outlives the run.
    let stopped_all = stop(&mut scope, leader, &PHASES, signal_scope);
    // A leader that never joined its scope means systemd-run failed: the command never ran.
    let stopped = stopped.or_else(|| {
        let failed = format!(
            "could not start the run's scope; see {}",
            log_path.display()
        );
        scope.cgroup.is_none().then_some(("failed", 1, failed))
    });
    // A leader that survived SIGKILL cannot be waited for without hanging.
    let status = match leader_exited(leader) {
        true => Some(child.wait().map_err(io_failure)?),
        false => None,
    };
    let (outcome, code, message) = verdict(
        stopped,
        stopped_all.as_ref().err().map(Vec::as_slice),
        status,
    );
    let mut fields =
        serde_json::json!({ "finished": now(), "outcome": outcome, "exit_code": code });
    if let Err(left) = &stopped_all {
        fields["abandoned_pids"] = left.clone().into();
    }
    let fields = fields.as_object().cloned().unwrap_or_default();
    if let Err(e) = session::update(&config.state_file, &record, fields) {
        return Err((
            1,
            format!("{id} {message}; cannot write session record: {e}"),
        ));
    }
    if code == 0 {
        let _ = writeln!(std::io::stderr(), "rota: {id} {message}");
        return Ok(());
    }
    Err((code, format!("{id} {message}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }
    #[test]
    fn run_arguments() {
        let line = "--brief b.md --name eon-x.review_2 --timeout 90m --role review \
                    --add-dir /a --add-dir /b --effort xhigh --cwd /w";
        let parsed = parse(&args(line)).unwrap();
        assert_eq!(parsed.role, Role::Review);
        assert_eq!(parsed.timeout, 5400);
        assert_eq!(parsed.name, "eon-x.review_2");
        assert_eq!(parsed.add_dirs, [PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(parsed.effort.as_deref(), Some("xhigh"));
        assert_eq!(parsed.cwd, Some(PathBuf::from("/w")));
        let parsed = parse(&args("--brief b --name n --timeout 5")).unwrap();
        assert_eq!(
            (parsed.role, parsed.cwd, parsed.effort),
            (Role::Implement, None, None)
        );
        for bad in [
            "--brief b --name n",
            "--brief b --name n --timeout 1d",
            "--brief b --name -n --timeout 5",
            "--brief b --name a/b --timeout 5",
            "--brief b --name n --timeout 5 --role plan",
            "--brief b --name n --timeout 5 --sandbox read-only",
            "--brief b --name n --timeout 5 extra",
            "--brief b --name n --timeout",
        ] {
            assert_eq!(parse(&args(bad)).unwrap_err().0, 2, "{bad}");
        }
    }
    #[test]
    fn timeouts() {
        assert_eq!(parse_timeout("5400"), Some(5400));
        assert_eq!(parse_timeout("45s"), Some(45));
        assert_eq!(parse_timeout("90m"), Some(5400));
        assert_eq!(parse_timeout("2h"), Some(7200));
        assert_eq!(parse_timeout("720h"), Some(MAX_TIMEOUT));
        for bad in [
            "",
            "m",
            "0",
            "0m",
            "-5",
            "1.5h",
            "5 m",
            "99999999999999999999h",
            "721h",
        ] {
            assert_eq!(parse_timeout(bad), None, "{bad}");
        }
    }
    #[test]
    fn placeholders_fill_in_one_pass_and_add_dirs_repeat() {
        let values = [("{cwd}", "/w/{effort}"), ("{effort}", "xhigh")];
        assert_eq!(fill("effort={effort}", &values), "effort=xhigh");
        assert_eq!(fill("{cwd}/{x}", &values), "/w/{effort}/{x}");
        let config = Config::parse(include_str!("../config.example.toml")).unwrap();
        let run = config.backends[2].run.as_ref().unwrap();
        let built = argv(run, Role::Implement, &values, &["/d1", "/d2"]);
        let at = built.iter().position(|a| a == "workspace-write").unwrap();
        assert_eq!(
            built[at + 1..at + 5],
            ["--add-dir", "/d1", "--add-dir", "/d2"]
        );
        assert!(built.contains(&"model_reasoning_effort=xhigh".to_string()));
        let none = argv(run, Role::Implement, &values, &[]);
        assert_eq!(none.len(), built.len() - 4);
        assert!(!none.iter().any(|a| a.contains("add_dirs")));
    }
    const FAST: Phases = Phases {
        grace: Duration::from_millis(300),
        kill: Duration::from_millis(500),
    };
    /// A scope named `unit` running `sleep 300` and a `setsid` one; returns its systemd-run.
    fn scope_with_workers(unit: &str) -> std::process::Child {
        let child = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--unit={unit}"))
            .args(["--", "sh", "-c", "setsid sleep 300 & exec sleep 300"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut scope = Scope::new(unit.into());
        let start = Instant::now();
        while !(scope.bind(child.id() as i32) == Joined::Yes && scope.pids().len() == 2) {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "scope never started"
            );
            thread::sleep(Duration::from_millis(20));
        }
        child
    }
    fn unit_name() -> String {
        format!("rota-unit-test-{}.scope", random().unwrap())
    }
    #[test]
    fn stop_reports_survivors_then_empties_the_scope() {
        let unit = unit_name();
        let mut leader = scope_with_workers(&unit);
        let pid = leader.id() as i32;
        let mut scope = Scope::new(unit.clone());
        // A signaller that delivers nothing stands in for processes that survive SIGKILL.
        let start = Instant::now();
        let mut left = stop(&mut scope, pid, &FAST, |_, _| {}).unwrap_err();
        assert!(start.elapsed() >= Duration::from_millis(800));
        left.sort();
        let mut all = scope.pids();
        all.sort();
        assert_eq!(left.len(), 2, "{left:?}");
        assert_eq!(left, all);
        assert!(left.contains(&pid));
        assert_eq!(stop(&mut scope, pid, &FAST, signal_scope), Ok(()));
        assert_eq!(scope.population(), Population::Empty);
        assert!(leader.wait().unwrap().signal().is_some());
    }
    #[test]
    fn a_listed_pid_outside_the_scope_is_spared() {
        // A PID read from cgroup.procs that now belongs to another process, as after PID reuse.
        let dir = tempfile::tempdir_in("target").unwrap();
        let mut other = Command::new("sleep").arg("30").spawn().unwrap();
        fs::write(dir.path().join("cgroup.procs"), format!("{}\n", other.id())).unwrap();
        let mut scope = Scope::new("rota-none.scope".into());
        scope.path = Some("/rota-none.scope".into());
        scope.cgroup = Some(dir.path().to_path_buf());
        assert_eq!(scope.pids(), vec![other.id() as i32]);
        signal_scope(&scope, libc::SIGKILL);
        thread::sleep(Duration::from_millis(200));
        assert!(
            other.try_wait().unwrap().is_none(),
            "signalled a process outside the scope"
        );
        other.kill().unwrap();
        other.wait().unwrap();
    }
    #[test]
    fn only_processes_in_the_scope_are_held() {
        let unit = unit_name();
        let mut leader = scope_with_workers(&unit);
        let mut scope = Scope::new(unit);
        assert!(!scope.holds(leader.id() as i32), "unbound");
        assert_eq!(scope.bind(leader.id() as i32), Joined::Yes);
        assert!(scope.pids().into_iter().all(|pid| scope.holds(pid)));
        assert!(!scope.holds(std::process::id() as i32));
        assert_eq!(
            stop(&mut scope, leader.id() as i32, &FAST, signal_scope),
            Ok(())
        );
        leader.wait().unwrap();
    }
    #[test]
    fn unit_names_are_random() {
        let (a, b) = (random().unwrap(), random().unwrap());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
    #[test]
    fn bounded_kills_a_stalled_helper() {
        let start = Instant::now();
        let kill: fn(&mut std::process::Child) = |child| {
            let _ = child.kill();
        };
        let result = bounded(
            Command::new("sleep").arg("30"),
            Duration::from_millis(200),
            kill,
        );
        assert_eq!(result.unwrap_err(), "no answer within 200 ms");
        assert!(start.elapsed() < Duration::from_secs(5));
        let shell = ["-c", "echo hi >&2; sleep 30 &"];
        let (status, stderr) = bounded(
            Command::new("sh").args(shell),
            Duration::from_secs(10),
            kill,
        )
        .unwrap();
        assert!(status.success());
        assert_eq!(stderr, "hi\n");
        // The backgrounded sleep keeps stderr open; rota must not wait for it.
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn a_helper_that_outlives_sigkill_is_given_up_on() {
        // A kill that does nothing stands in for a helper stuck in uninterruptible I/O.
        let start = Instant::now();
        let result = bounded(
            Command::new("sleep").arg("5"),
            Duration::from_millis(200),
            |_| {},
        );
        let error = result.unwrap_err();
        assert_eq!(
            error,
            "no answer within 200 ms, and still running after SIGKILL"
        );
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    }
    #[test]
    fn an_unreadable_leader_cgroup_is_abandoned_not_unjoined() {
        use std::os::unix::fs::PermissionsExt;
        let mut leader = Command::new("true").spawn().unwrap();
        while !leader_exited(leader.id() as i32) {
            thread::sleep(Duration::from_millis(10));
        }
        // A stand-in /proc whose cgroup file for the leader cannot be read.
        let proc_dir = tempfile::tempdir_in("target").unwrap();
        let file = proc_dir.path().join(format!("{}/cgroup", leader.id()));
        fs::create_dir(file.parent().unwrap()).unwrap();
        fs::write(&file, "0::/elsewhere\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        let mut scope = Scope::new("rota-none.scope".into());
        scope.proc_dir = proc_dir.path().to_path_buf();
        assert_eq!(scope.bind(leader.id() as i32), Joined::Unknown);
        let fail: fn(&Scope, libc::c_int) = |_, signal| panic!("signalled {signal}");
        let start = Instant::now();
        assert_eq!(
            stop(&mut scope, leader.id() as i32, &FAST, fail),
            Err(Vec::new())
        );
        assert!(start.elapsed() >= Duration::from_millis(800));
        // Readable, the same file shows the leader never joined.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(scope.bind(leader.id() as i32), Joined::No);
        assert_eq!(stop(&mut scope, leader.id() as i32, &FAST, fail), Ok(()));
        leader.wait().unwrap();
    }
    #[test]
    fn an_unreadable_scope_is_abandoned_not_empty() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir_in("target").unwrap();
        let events = dir.path().join("cgroup.events");
        fs::write(&events, "populated 0\n").unwrap();
        fs::set_permissions(&events, fs::Permissions::from_mode(0o000)).unwrap();
        let mut leader = Command::new("true").spawn().unwrap();
        while !leader_exited(leader.id() as i32) {
            thread::sleep(Duration::from_millis(10));
        }
        let mut scope = Scope::new("rota-none.scope".into());
        scope.cgroup = Some(dir.path().to_path_buf());
        assert_eq!(scope.population(), Population::Unknown);
        let left = stop(&mut scope, leader.id() as i32, &FAST, |_, _| {});
        assert_eq!(left, Err(Vec::new()));
        assert_eq!(verdict(None, Some(&[]), None).1, 125);
        leader.wait().unwrap();
    }
    #[test]
    fn a_taken_unit_name_is_never_signalled() {
        let unit = unit_name();
        let mut owner = scope_with_workers(&unit);
        let mut second = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--unit={unit}"))
            .args(["--", "sleep", "300"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut scope = Scope::new(unit.clone());
        let fail: fn(&Scope, libc::c_int) = |_, signal| panic!("signalled {signal}");
        assert_eq!(stop(&mut scope, second.id() as i32, &FAST, fail), Ok(()));
        assert_eq!(scope.bind(second.id() as i32), Joined::No);
        assert_eq!(second.wait().unwrap().code(), Some(1));
        let mut owned = Scope::new(unit);
        assert_eq!(owned.bind(owner.id() as i32), Joined::Yes);
        assert_eq!(owned.population(), Population::Populated);
        assert_eq!(
            stop(&mut owned, owner.id() as i32, &FAST, signal_scope),
            Ok(())
        );
        owner.wait().unwrap();
    }
    #[test]
    fn survivors_override_every_other_outcome() {
        let ok = Some(ExitStatus::from_raw(0));
        let timeout = Some(("timeout", 124, "timed out after 1s".to_string()));
        assert_eq!(verdict(None, None, ok).0, "exited");
        assert_eq!(verdict(None, None, ok).1, 0);
        assert_eq!(verdict(timeout.clone(), None, ok).1, 124);
        let abandoned = verdict(None, Some(&[41, 42]), ok);
        let message = "left processes running after SIGKILL: 41 42".to_string();
        assert_eq!(abandoned, ("abandoned", 125, message));
        assert_eq!(verdict(timeout, Some(&[41]), None).0, "abandoned");
        assert_eq!(verdict(None, None, Some(ExitStatus::from_raw(9))).1, 137);
    }
    #[test]
    fn scrub_patterns_match_names_and_prefixes() {
        let settings = RunEnv {
            unset_env: vec!["DISPLAY".into(), "TERMINAL_*".into()],
            ..RunEnv::default()
        };
        assert!(scrubbed(&settings, "DISPLAY"));
        assert!(scrubbed(&settings, "TERMINAL_SOCKET"));
        assert!(!scrubbed(&settings, "DISPLAY2"));
        assert!(!scrubbed(&settings, "XTERMINAL_SOCKET"));
    }
}
