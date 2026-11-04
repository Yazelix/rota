use crate::config::Parser;
use std::collections::BTreeMap;

pub type Windows = BTreeMap<String, f64>;

fn percent(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(value)
    } else {
        Err("invalid percentage".into())
    }
}

pub fn parse(parser: Parser, text: &str) -> Result<Windows, String> {
    let mut windows = Windows::new();
    match parser {
        Parser::Agy => {
            for line in text.lines() {
                let fields: Vec<_> = line.split('\t').map(str::trim).collect();
                if fields.len() < 3 || !fields[0].eq_ignore_ascii_case("Claude and GPT models") {
                    continue;
                }
                let name = fields[1].to_lowercase();
                let key = if name.contains("five hour") {
                    "five_hour"
                } else if name.contains("weekly") {
                    "weekly"
                } else {
                    continue;
                };
                let value = fields[2]
                    .strip_suffix('%')
                    .and_then(|v| v.parse().ok())
                    .ok_or("invalid agy percentage")?;
                if windows.insert(key.into(), percent(value)?).is_some() {
                    return Err("duplicate agy window".into());
                }
            }
            if !windows.contains_key("five_hour") || !windows.contains_key("weekly") {
                return Err("agy usage missing five-hour or weekly window".into());
            }
        }
        Parser::ClaudeText => {
            let fields: Vec<_> = text.split_whitespace().collect();
            if fields.len() < 2 {
                return Err("Claude usage needs two fields".into());
            }
            for (key, value) in ["five_hour", "seven_day"].into_iter().zip(fields) {
                if value != "None" {
                    let used = percent(value.parse().map_err(|_| "invalid Claude percentage")?)?;
                    windows.insert(key.into(), 100.0 - used);
                }
            }
        }
        Parser::ClaudeJson => {
            let json: serde_json::Value =
                serde_json::from_str(text).map_err(|_| "invalid quota JSON")?;
            if !json.is_object()
                || !["five_hour", "seven_day"]
                    .iter()
                    .any(|k| json.get(k).is_some())
            {
                return Err("not a Claude usage response".into());
            }
            for key in ["five_hour", "seven_day"] {
                if let Some(window) = json.get(key).filter(|v| !v.is_null()) {
                    let value = window.get("utilization").ok_or("missing utilization")?;
                    if !value.is_null() {
                        windows.insert(
                            key.into(),
                            100.0 - percent(value.as_f64().ok_or("invalid utilization")?)?,
                        );
                    }
                }
            }
        }
        Parser::EonCodex | Parser::EonClaude => {
            let json: serde_json::Value =
                serde_json::from_str(text).map_err(|_| "invalid workspace JSON")?;
            let key = if matches!(parser, Parser::EonCodex) {
                "codex_quota"
            } else {
                "claude_quota"
            };
            let snapshot = json.get(key).ok_or("quota snapshot unavailable")?;
            if snapshot.get("state").and_then(|s| s.as_str()) != Some("fresh") {
                return Err("quota snapshot is not fresh".into());
            }
            let items = snapshot
                .get("windows")
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
                .ok_or("quota windows unavailable")?;
            for (i, window) in items.iter().enumerate() {
                let value = percent(
                    window
                        .get("remaining_percent")
                        .and_then(|v| v.as_f64())
                        .ok_or("invalid remaining_percent")?,
                )?;
                let duration = window
                    .get("duration_minutes")
                    .and_then(|v| v.as_u64())
                    .ok_or("invalid duration_minutes")?;
                // Include the index: scoped windows may share the same duration.
                windows.insert(format!("{duration}m/{i}"), value);
            }
        }
        Parser::RemainingJson => {
            let values: BTreeMap<String, f64> = serde_json::from_str(text)
                .map_err(|_| "expected JSON object of remaining percentages")?;
            if values.is_empty() {
                return Err("no quota windows".into());
            }
            for (key, value) in values {
                windows.insert(key, percent(value)?);
            }
        }
    }
    Ok(windows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_readers() {
        let agy = parse(Parser::Agy, include_str!("../tests/fixtures/agy.tsv")).unwrap();
        assert_eq!(agy["five_hour"], 67.5);
        assert_eq!(agy["weekly"], 48.0);
        let claude = parse(
            Parser::ClaudeText,
            include_str!("../tests/fixtures/claude.txt"),
        )
        .unwrap();
        assert_eq!(claude["five_hour"], 76.5);
        assert!(!claude.contains_key("seven_day"));
        assert_eq!(
            claude,
            parse(
                Parser::ClaudeJson,
                include_str!("../tests/fixtures/claude.json")
            )
            .unwrap()
        );
        let workspace = include_str!("../tests/fixtures/workspace.json");
        assert_eq!(parse(Parser::EonCodex, workspace).unwrap()["9000m/1"], 42.0);
        assert_eq!(parse(Parser::EonClaude, workspace).unwrap()["240m/0"], 76.5);
        assert_eq!(
            parse(Parser::RemainingJson, "{\"week\": 43}").unwrap()["week"],
            43.0
        );
    }
    #[test]
    fn malformed_missing_or_out_of_range_refuses() {
        for (parser, text) in [
            (Parser::Agy, ""),
            (Parser::Agy, "Claude and GPT models\tWeekly\tNaN%"),
            (Parser::ClaudeText, "probe-failed"),
            (Parser::ClaudeText, "101 3"),
            (Parser::ClaudeJson, "{}"),
            (
                Parser::ClaudeJson,
                "{\"five_hour\":{\"utilization\":\"oops\"}}",
            ),
            (Parser::EonCodex, "{\"codex_quota\":{\"windows\":[]}}"),
            (
                Parser::EonCodex,
                "{\"codex_quota\":{\"windows\":[{\"duration_minutes\":240,\"remaining_percent\":null}]}}",
            ),
            (Parser::RemainingJson, "{\"week\":-1}"),
        ] {
            assert!(parse(parser, text).is_err(), "{text}");
        }
        assert!(parse(Parser::ClaudeText, "None None").unwrap().is_empty());
    }
    #[test]
    fn eon_rejects_unusable_snapshots_even_with_remaining_quota() {
        for parser in [Parser::EonCodex, Parser::EonClaude] {
            for state in [Some("stale"), Some("error"), Some("unknown"), None] {
                let mut snapshot = serde_json::json!({"windows":[{"duration_minutes":240,"remaining_percent":80}]});
                if let Some(state) = state {
                    snapshot["state"] = state.into();
                }
                let text = serde_json::json!({"codex_quota":snapshot, "claude_quota":snapshot})
                    .to_string();
                assert!(parse(parser, &text).is_err());
            }
        }
    }
}
