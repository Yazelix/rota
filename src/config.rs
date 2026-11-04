use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Implement,
    Review,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Medium,
    High,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_file: PathBuf,
    pub log_file: PathBuf,
    pub fallback: Option<String>,
    pub backends: Vec<Backend>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub roles: Vec<Role>,
    pub commands: BTreeMap<Level, Vec<String>>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub unset_env: Vec<String>,
    pub guard: Vec<String>,
    #[serde(default)]
    pub required_files: Vec<PathBuf>,
    #[serde(default)]
    pub excluded_roots: Vec<PathBuf>,
    pub account_check: Option<Vec<String>>,
    pub quota: Option<Reader>,
    #[serde(default)]
    pub allow_unknown_quota: bool,
    #[serde(default)]
    pub limits: BTreeMap<Role, Limits>,
    pub cap: Option<Cap>,
    pub trusted_settings: Option<PathBuf>,
    #[serde(default)]
    pub prepare: Vec<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reader {
    pub command: Vec<String>,
    pub parser: Parser,
    #[serde(default = "timeout")]
    pub timeout_seconds: u64,
    pub refresh: Option<Vec<String>>,
    #[serde(default = "refresh_timeout")]
    pub refresh_timeout_seconds: u64,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parser {
    Agy,
    ClaudeText,
    ClaudeJson,
    EonCodex,
    EonClaude,
    RemainingJson,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default)]
    pub min_remaining: BTreeMap<String, f64>,
    #[serde(default)]
    pub max_used: BTreeMap<String, f64>,
    #[serde(default)]
    pub allow_missing: bool,
    #[serde(default)]
    pub strictly_above_min: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cap {
    pub max: usize,
    pub executable: String,
    #[serde(default)]
    pub runtime_executables: Vec<PathBuf>,
    pub script: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub exclude_args: Vec<String>,
}

fn yes() -> bool {
    true
}
fn refresh_timeout() -> u64 {
    90
}
fn timeout() -> u64 {
    30
}

fn command_valid(command: &[String]) -> bool {
    command.first().is_some_and(|s| !s.is_empty()) && command.iter().all(|s| !s.contains('\0'))
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.state_file.as_os_str().is_empty()
            || self.log_file.as_os_str().is_empty()
            || self.state_file == self.log_file
            || self.backends.is_empty()
        {
            return Err(
                "distinct state_file/log_file and at least one backend are required".into(),
            );
        }
        let mut names = BTreeSet::new();
        for b in &self.backends {
            let fail = |message| Err(format!("{}: {message}", b.name));
            if b.name.is_empty()
                || !b
                    .name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                || !names.insert(&b.name)
            {
                return fail("backend names must be unique identifiers");
            }
            if !command_valid(&b.guard)
                || [Level::Medium, Level::High].iter().any(|level| {
                    !b.commands.get(level).is_some_and(|c| {
                        command_valid(c) && c.iter().filter(|a| *a == "{prompt}").count() == 1
                    })
                })
            {
                return fail(
                    "guard and both level commands (with one {prompt} argument) are required",
                );
            }
            if b.roles.iter().collect::<BTreeSet<_>>().len() != b.roles.len() {
                return fail("duplicate role");
            }
            if b.account_check.as_ref().is_some_and(|c| !command_valid(c))
                || b.prepare.iter().any(|c| !command_valid(c))
                || b.quota.as_ref().is_some_and(|q| {
                    !command_valid(&q.command)
                        || q.timeout_seconds == 0
                        || q.refresh_timeout_seconds == 0
                        || q.refresh.as_ref().is_some_and(|c| !command_valid(c))
                })
            {
                return fail("invalid check, preparation or reader command/timeout");
            }
            if b.cap.as_ref().is_some_and(|c| {
                c.max == 0
                    || c.executable.is_empty()
                    || c.executable.contains('/')
                    || c.runtime_executables
                        .iter()
                        .chain(c.script.iter())
                        .any(|p| !p.is_absolute())
            }) {
                return fail(
                    "cap needs positive max, an executable basename and absolute runtime/script paths",
                );
            }
            if b.env
                .keys()
                .chain(&b.unset_env)
                .chain(b.cap.iter().flat_map(|c| c.env.keys()))
                .any(|k| k.is_empty() || k.contains(['=', '\0']))
                || b.env
                    .values()
                    .chain(b.cap.iter().flat_map(|c| c.env.values()))
                    .any(|v| v.contains('\0'))
                || b.unset_env.iter().any(|k| b.env.contains_key(k))
            {
                return fail("invalid or conflicting environment");
            }
            for limit in b.limits.values() {
                if limit
                    .min_remaining
                    .values()
                    .chain(limit.max_used.values())
                    .any(|v| !v.is_finite() || !(0.0..=100.0).contains(v))
                {
                    return fail("quota limits must be finite percentages");
                }
            }
        }
        if let Some(fallback) = &self.fallback {
            let Some(b) = self.backends.iter().find(|b| &b.name == fallback) else {
                return Err("fallback backend does not exist".into());
            };
            if !b.roles.is_empty() {
                return Err("fallback backend must have no normal routing roles".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const EXAMPLE: &str = include_str!("../config.example.toml");

    #[test]
    fn example_is_valid() {
        Config::parse(EXAMPLE).unwrap();
    }

    #[test]
    fn rejects_invalid_config() {
        for (from, to) in [
            ("name = \"alpha\"", "name = \"bad name\""),
            (
                "guard = [\"test\", \"-f\", \"/PATH/TO/guard\"]",
                "guard = []",
            ),
            ("max = 1", "max = 0"),
            ("max = 1", "max = 1\nruntime_executables = ['relative']"),
            ("five_hour = 30", "five_hour = 101"),
            ("fallback = \"main\"", "fallback = \"absent\""),
            ("roles = []", "roles = [\"implement\"]"),
            ("{prompt}", "missing-prompt"),
        ] {
            assert_ne!(EXAMPLE, EXAMPLE.replace(from, to));
            assert!(Config::parse(&EXAMPLE.replace(from, to)).is_err(), "{from}");
        }
        assert!(Config::parse(&format!("typo = true\n{EXAMPLE}")).is_err());
    }
}
