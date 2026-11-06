use rota::config::{Config, Role};
use rota::{decision, launch, ps, readings, run as headless};
use std::io::Write;
use std::{env, fs, process::ExitCode};

const USAGE: &str = "Usage: rota launch [--role implement|review] [--level medium|high] [--only BACKEND] [--dry-run] <prompt-file>\n       rota run --brief FILE --name NAME --timeout DURATION [--role implement|review] [--only BACKEND] [--cwd DIR] [--effort LEVEL] [--add-dir DIR]...\n       rota ps [--json] [--all]\n       rota status [--json]\nConfig: $ROTA_CONFIG or $XDG_CONFIG_HOME/rota/config.toml (default ~/.config/rota/config.toml)";

fn load_config() -> Result<Config, String> {
    let path = env::var_os("ROTA_CONFIG")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
                })
                .map(|p| p.join("rota/config.toml"))
        })
        .ok_or("cannot locate config: set ROTA_CONFIG")?;
    Config::parse(&fs::read_to_string(path).map_err(|e| format!("cannot read config: {e}"))?)
}

fn run() -> Result<(), (u8, String)> {
    let args: Vec<_> = env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| (2, "arguments must be UTF-8".into()))
        })
        .collect::<Result<_, _>>()?;
    if args == ["--help"] || args == ["-h"] {
        println!("{USAGE}");
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "launch") {
        let options = launch::parse(&args[1..])?;
        let config = load_config().map_err(|e| (2, e))?;
        return launch::run(&config, &options);
    }
    if args.first().is_some_and(|s| s == "run") {
        let options = headless::parse(&args[1..])?;
        let config = load_config().map_err(|e| (2, e))?;
        return headless::run(&config, &options);
    }
    if args.first().is_some_and(|s| s == "ps") {
        let options = ps::parse(&args[1..])?;
        let config = load_config().map_err(|e| (2, e))?;
        return ps::run(&config, &options);
    }
    if args != ["status"] && args != ["status", "--json"] {
        return Err((2, USAGE.into()));
    }
    let config = load_config().map_err(|e| (2, e))?;
    let cwd = env::current_dir().map_err(|e| (1, e.to_string()))?;
    let indices: Vec<_> = (0..config.backends.len()).collect();
    let readings = readings::collect(&config, &indices, &cwd, false);
    if args.len() == 2 {
        let status: Vec<_> = config.backends.iter().zip(&readings).map(|(b,r)| {
            serde_json::json!({ "backend": b.name, "enabled": b.enabled, "reading": r,
                "implement": status_for(&config, b, r, Role::Implement), "review": status_for(&config, b, r, Role::Review) })
        }).collect();
        println!("{}", serde_json::to_string_pretty(&status).unwrap());
    } else {
        for (b, r) in config.backends.iter().zip(&readings) {
            println!(
                "{}: implement={} review={} running={} {}",
                b.name,
                status_for(&config, b, r, Role::Implement),
                status_for(&config, b, r, Role::Review),
                r.running,
                r.detail
            );
        }
    }
    Ok(())
}
fn status_for(
    config: &Config,
    b: &rota::config::Backend,
    r: &decision::Reading,
    role: Role,
) -> String {
    if role == Role::Implement && config.fallback.as_deref() == Some(&b.name) {
        return match decision::availability(b, r, role) {
            Ok(()) => "fallback available".into(),
            Err(problem) => format!("fallback unavailable: {problem}"),
        };
    }
    if !b.roles.contains(&role) {
        if b.run
            .as_ref()
            .is_some_and(|run| run.commands.contains_key(&role))
        {
            return match decision::availability(b, r, role) {
                Ok(()) => "available for rota run".into(),
                Err(problem) => format!("unavailable for rota run: {problem}"),
            };
        }
        return "not allowed for role".into();
    }
    decision::availability(b, r, role)
        .err()
        .unwrap_or_else(|| "available".into())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err((code, message)) => {
            // A run may end after its caller closed stderr; never panic here.
            let _ = writeln!(std::io::stderr(), "rota: {message}");
            ExitCode::from(code)
        }
    }
}
