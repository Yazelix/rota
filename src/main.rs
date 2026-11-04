use rota::config::{Config, Role};
use rota::{decision, readings};
use std::{env, fs, process::ExitCode};

const USAGE: &str = "Usage: rota launch [--role implement|review] [--level medium|high] [--only BACKEND] [--dry-run] <prompt-file>\n       rota status [--json]\nConfig: $ROTA_CONFIG or $XDG_CONFIG_HOME/rota/config.toml (default ~/.config/rota/config.toml)";

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
    let args: Vec<_> = env::args().skip(1).collect();
    if args == ["--help"] || args == ["-h"] {
        println!("{USAGE}");
        return Ok(());
    }
    if args != ["status"] && args != ["status", "--json"] {
        return Err((2, USAGE.into()));
    }
    let config = load_config().map_err(|e| (2, e))?;
    let cwd = env::current_dir().map_err(|e| (1, e.to_string()))?;
    let indices: Vec<_> = (0..config.backends.len()).collect();
    let readings = readings::collect(&config, &indices, &cwd, false);
    let status: Vec<_> = config.backends.iter().zip(&readings).map(|(b,r)| {
        serde_json::json!({ "backend": b.name, "enabled": b.enabled, "reading": r,
            "implement": status_for(b, r, Role::Implement), "review": status_for(b, r, Role::Review) })
    }).collect();
    if args.len() == 2 {
        println!("{}", serde_json::to_string_pretty(&status).unwrap());
    } else {
        for item in status {
            println!(
                "{}: implement={} review={} running={} {}{}",
                item["backend"].as_str().unwrap(),
                item["implement"],
                item["review"],
                item["reading"]["running"],
                item["reading"]["detail"].as_str().unwrap_or(""),
                item["reading"]["problem"]
                    .as_str()
                    .map(|s| format!(" ({s})"))
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}
fn status_for(b: &rota::config::Backend, r: &decision::Reading, role: Role) -> String {
    if !b.roles.contains(&role) {
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
            eprintln!("rota: {message}");
            ExitCode::from(code)
        }
    }
}
