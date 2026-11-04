use crate::config::{Backend, Config, Limits, Role};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Default, Serialize)]
pub struct Reading {
    pub remaining: Option<BTreeMap<String, f64>>,
    pub running: usize,
    pub problem: Option<String>,
    pub detail: String,
}

pub fn availability(backend: &Backend, reading: &Reading, role: Role) -> Result<(), String> {
    if !backend.enabled {
        return Err("disabled".into());
    }
    if let Some(problem) = &reading.problem {
        return Err(problem.clone());
    }
    if backend
        .cap
        .as_ref()
        .is_some_and(|cap| reading.running >= cap.max)
    {
        return Err(format!("busy ({} running)", reading.running));
    }
    let Some(remaining) = &reading.remaining else {
        return if backend.allow_unknown_quota {
            Ok(())
        } else {
            Err("quota unknown (not explicitly allowed)".into())
        };
    };
    if remaining.is_empty()
        && !backend.allow_unknown_quota
        && !backend.limits.get(&role).is_some_and(|l| l.allow_missing)
    {
        return Err("quota empty (not explicitly allowed)".into());
    }
    if let Some(limits) = backend.limits.get(&role) {
        check_limits(remaining, limits)?;
    }
    Ok(())
}

fn percent_cmp(value: f64, threshold: f64) -> std::cmp::Ordering {
    // Complementary percentages can differ at an exact decimal cutoff by a rounding error.
    if (value - threshold).abs() <= 100.0 * f64::EPSILON {
        std::cmp::Ordering::Equal
    } else {
        value.total_cmp(&threshold)
    }
}

fn check_limits(remaining: &BTreeMap<String, f64>, limits: &Limits) -> Result<(), String> {
    for (used, rules) in [(false, &limits.min_remaining), (true, &limits.max_used)] {
        for (window, threshold) in rules {
            let values: Vec<_> = if window == "*" {
                remaining.values().copied().collect()
            } else {
                remaining.get(window).copied().into_iter().collect()
            };
            if values.is_empty() && !limits.allow_missing {
                return Err(format!("missing quota window {window}"));
            }
            if values.iter().any(|v| {
                if used {
                    percent_cmp(*v, 100.0 - threshold) != std::cmp::Ordering::Greater
                } else if limits.strictly_above_min {
                    percent_cmp(*v, *threshold) != std::cmp::Ordering::Greater
                } else {
                    percent_cmp(*v, *threshold) == std::cmp::Ordering::Less
                }
            }) {
                return Err(format!("quota limit for {window}"));
            }
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct Pick {
    pub index: usize,
    pub fallback: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub code: u8,
    pub message: String,
}

pub fn candidates(config: &Config, role: Role, only: Option<&str>) -> Result<Vec<usize>, Refusal> {
    if let Some(name) = only {
        let Some(index) = config
            .backends
            .iter()
            .position(|b| b.name == name && b.roles.contains(&role))
        else {
            return Err(Refusal {
                code: 2,
                message: format!("--only {name} is not allowed for role {role:?}"),
            });
        };
        return Ok(vec![index]);
    }
    Ok(config
        .backends
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.roles.contains(&role).then_some(i))
        .collect())
}

pub fn choose(
    config: &Config,
    readings: &[Reading],
    role: Role,
    only: Option<&str>,
    last: Option<&str>,
) -> Result<Pick, Refusal> {
    let allowed = candidates(config, role, only)?;
    let start = last
        .and_then(|name| config.backends.iter().position(|b| b.name == name))
        .map_or(0, |i| i + 1);
    for offset in 0..config.backends.len() {
        let index = (start + offset) % config.backends.len();
        if allowed.contains(&index)
            && readings
                .get(index)
                .is_some_and(|r| availability(&config.backends[index], r, role).is_ok())
        {
            return Ok(Pick {
                index,
                fallback: false,
            });
        }
    }
    if role == Role::Implement && only.is_none() {
        if let Some(index) = config
            .backends
            .iter()
            .position(|b| Some(&b.name) == config.fallback.as_ref())
        {
            if readings
                .get(index)
                .is_some_and(|r| availability(&config.backends[index], r, role).is_ok())
            {
                return Ok(Pick {
                    index,
                    fallback: true,
                });
            }
        }
    }
    Err(Refusal {
        code: 3,
        message: "no backend available, refusing".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        Config::parse(include_str!("../config.example.toml")).unwrap()
    }
    fn readings(config: &Config) -> Vec<Reading> {
        config
            .backends
            .iter()
            .map(|_| Reading {
                remaining: Some(
                    [
                        ("five_hour".into(), 90.0),
                        ("weekly".into(), 90.0),
                        ("seven_day".into(), 90.0),
                    ]
                    .into(),
                ),
                ..Reading::default()
            })
            .collect()
    }
    #[test]
    fn rotates_filters_caps_and_handles_unknown_last() {
        let config = config();
        let mut r = readings(&config);
        assert_eq!(
            choose(&config, &r, Role::Implement, None, None)
                .unwrap()
                .index,
            0
        );
        assert_eq!(
            choose(&config, &r, Role::Implement, None, Some("alpha"))
                .unwrap()
                .index,
            1
        );
        assert_eq!(
            choose(&config, &r, Role::Review, None, Some("secondary"))
                .unwrap()
                .index,
            0
        );
        assert_eq!(
            choose(&config, &r, Role::Implement, None, Some("code"))
                .unwrap()
                .index,
            0
        );
        assert_eq!(
            choose(&config, &r, Role::Implement, None, Some("deleted"))
                .unwrap()
                .index,
            0
        );
        r[0].running = 1;
        assert_eq!(
            choose(&config, &r, Role::Review, None, None).unwrap().index,
            1
        );
        assert_eq!(
            choose(&config, &r, Role::Review, Some("code"), None)
                .unwrap_err()
                .code,
            2
        );
    }
    #[test]
    fn refuses_or_explicitly_falls_back() {
        let config = config();
        let mut r = readings(&config);
        for reading in &mut r[..3] {
            reading.problem = Some("unavailable".into());
        }
        assert_eq!(
            choose(&config, &r, Role::Review, None, None)
                .unwrap_err()
                .code,
            3
        );
        assert_eq!(
            choose(&config, &r, Role::Implement, Some("code"), None)
                .unwrap_err()
                .code,
            3
        );
        assert_eq!(
            choose(&config, &r, Role::Implement, None, None).unwrap(),
            Pick {
                index: 4,
                fallback: true
            }
        );
        r[4].problem = Some("guard missing".into());
        assert_eq!(
            choose(&config, &r, Role::Implement, None, None)
                .unwrap_err()
                .code,
            3
        );
    }
    #[test]
    fn quota_boundaries_missing_unknown_and_role_limits() {
        let config = config();
        let mut r = readings(&config);
        r[0].remaining
            .as_mut()
            .unwrap()
            .insert("five_hour".into(), 30.0);
        assert!(availability(&config.backends[0], &r[0], Role::Implement).is_ok());
        r[0].remaining
            .as_mut()
            .unwrap()
            .insert("five_hour".into(), 29.9);
        assert!(availability(&config.backends[0], &r[0], Role::Implement).is_err());
        assert!(availability(&config.backends[0], &r[0], Role::Review).is_ok());
        r[1].remaining = Some([("five_hour".into(), 40.0)].into());
        assert!(availability(&config.backends[1], &r[1], Role::Implement).is_err());
        r[1].remaining = Some(BTreeMap::new());
        assert!(availability(&config.backends[1], &r[1], Role::Implement).is_ok());
        r[0].remaining = Some(BTreeMap::new());
        assert!(availability(&config.backends[0], &r[0], Role::Implement).is_err());
        r[2].remaining = Some([("window".into(), 0.0)].into());
        assert!(availability(&config.backends[2], &r[2], Role::Implement).is_err());
        r[0].remaining = None;
        assert!(availability(&config.backends[0], &r[0], Role::Implement).is_err());
        r[4].remaining = None;
        assert!(availability(&config.backends[4], &r[4], Role::Implement).is_ok());
    }
    #[test]
    fn empty_readings_need_explicit_permission_and_decimal_cutoffs_are_exclusive() {
        let mut config = config();
        let backend = &mut config.backends[1];
        backend.limits.clear();
        let mut reading = Reading {
            remaining: Some(BTreeMap::new()),
            ..Reading::default()
        };
        assert!(availability(backend, &reading, Role::Implement).is_err());
        backend.allow_unknown_quota = true;
        assert!(availability(backend, &reading, Role::Implement).is_ok());
        backend.allow_unknown_quota = false;
        for threshold in [30.1, 99.9] {
            backend.limits.insert(
                Role::Implement,
                Limits {
                    max_used: [("five_hour".into(), threshold)].into(),
                    ..Limits::default()
                },
            );
            for used in [threshold - 0.001, threshold, threshold + 0.001] {
                reading.remaining = Some(
                    crate::quota::parse(crate::config::Parser::ClaudeText, &format!("{used} None"))
                        .unwrap(),
                );
                assert_eq!(
                    availability(backend, &reading, Role::Implement).is_ok(),
                    used < threshold
                );
            }
        }
    }
}
