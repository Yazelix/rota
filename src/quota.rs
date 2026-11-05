use crate::config::Parser;
use std::collections::BTreeMap;

pub type Windows = BTreeMap<String, f64>;
pub type Resets = BTreeMap<String, String>;

pub const DEFAULT_AGY_GROUP: &str = "Claude and GPT models";

#[derive(Debug, Default, PartialEq)]
pub struct Quota {
    pub remaining: Windows,
    pub resets: Resets,
}

fn percent(value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=100.0).contains(&value) {
        Ok(value)
    } else {
        Err("invalid percentage".into())
    }
}

/// Accepts `YYYY-MM-DDTHH:MM:SS[.fraction](Z|+HH:MM|-HH:MM)`; anything else is no reset time.
fn rfc3339(text: &str) -> bool {
    let b = text.as_bytes();
    let num = |at: usize, len: usize| -> Option<u32> {
        let digits = b.get(at..at + len)?;
        digits
            .iter()
            .all(u8::is_ascii_digit)
            .then(|| digits.iter().fold(0, |n, d| n * 10 + u32::from(d - b'0')))
    };
    let sep = |at: usize, c: u8| b.get(at) == Some(&c);
    let date_time = num(0, 4).is_some()
        && sep(4, b'-')
        && num(5, 2).is_some_and(|m| (1..=12).contains(&m))
        && sep(7, b'-')
        && num(8, 2).is_some_and(|d| (1..=31).contains(&d))
        && matches!(b.get(10), Some(b'T' | b't'))
        && num(11, 2).is_some_and(|h| h <= 23)
        && sep(13, b':')
        && num(14, 2).is_some_and(|m| m <= 59)
        && sep(16, b':')
        && num(17, 2).is_some_and(|s| s <= 60);
    if !date_time {
        return false;
    }
    let mut at = 19;
    if sep(at, b'.') {
        let len = b[at + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if len == 0 {
            return false;
        }
        at += 1 + len;
    }
    match b.get(at) {
        Some(b'Z' | b'z') => b.len() == at + 1,
        Some(b'+' | b'-') => {
            b.len() == at + 6
                && num(at + 1, 2).is_some_and(|h| h <= 23)
                && sep(at + 3, b':')
                && num(at + 4, 2).is_some_and(|m| m <= 59)
        }
        _ => false,
    }
}

pub fn parse(parser: Parser, agy_group: Option<&str>, text: &str) -> Result<Quota, String> {
    let mut windows = Windows::new();
    let mut resets = Resets::new();
    match parser {
        Parser::Agy => {
            let group = agy_group.unwrap_or(DEFAULT_AGY_GROUP);
            for line in text.lines() {
                let fields: Vec<_> = line.split('\t').map(str::trim).collect();
                if fields.len() < 3 || !fields[0].eq_ignore_ascii_case(group) {
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
                let value: f64 = if fields[2].eq_ignore_ascii_case("disabled") {
                    0.0
                } else {
                    fields[2]
                        .strip_suffix('%')
                        .and_then(|v| v.parse().ok())
                        .ok_or("invalid agy percentage")?
                };
                if windows.insert(key.into(), percent(value)?).is_some() {
                    return Err("duplicate agy window".into());
                }
                if let Some(reset) = fields.get(3).copied().filter(|r| rfc3339(r)) {
                    resets.insert(key.into(), reset.into());
                }
            }
            if !windows.contains_key("weekly") {
                return Err(format!("agy usage has no weekly row for group {group:?}"));
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
    Ok(Quota {
        remaining: windows,
        resets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const EXHAUSTED: &str = include_str!("../tests/fixtures/agy-exhausted.tsv");

    #[test]
    fn fixture_readers() {
        let agy = parse(Parser::Agy, None, include_str!("../tests/fixtures/agy.tsv")).unwrap();
        assert_eq!(agy.remaining["five_hour"], 67.5);
        assert_eq!(agy.remaining["weekly"], 48.0);
        assert!(agy.resets.is_empty());
        let claude = parse(
            Parser::ClaudeText,
            None,
            include_str!("../tests/fixtures/claude.txt"),
        )
        .unwrap();
        assert_eq!(claude.remaining["five_hour"], 76.5);
        assert!(!claude.remaining.contains_key("seven_day"));
        assert_eq!(
            claude,
            parse(
                Parser::ClaudeJson,
                None,
                include_str!("../tests/fixtures/claude.json")
            )
            .unwrap()
        );
        let workspace = include_str!("../tests/fixtures/workspace.json");
        assert_eq!(
            parse(Parser::EonCodex, None, workspace).unwrap().remaining["9000m/1"],
            42.0
        );
        assert_eq!(
            parse(Parser::EonClaude, None, workspace).unwrap().remaining["240m/0"],
            76.5
        );
        assert_eq!(
            parse(Parser::RemainingJson, None, "{\"week\": 43}")
                .unwrap()
                .remaining["week"],
            43.0
        );
    }
    #[test]
    fn malformed_missing_or_out_of_range_refuses() {
        for (parser, text) in [
            (Parser::Agy, ""),
            (Parser::Agy, "Claude and GPT models\tWeekly\tNaN%"),
            (
                Parser::Agy,
                "Claude and GPT models\tWeekly Limit Remaining\tn/a\t2030-01-08T14:00:00Z",
            ),
            (
                Parser::Agy,
                "Claude and GPT models\tFive Hour Limit Remaining\t50%",
            ),
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
            assert!(parse(parser, None, text).is_err(), "{text}");
        }
        assert!(
            parse(Parser::ClaudeText, None, "None None")
                .unwrap()
                .remaining
                .is_empty()
        );
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
                assert!(parse(parser, None, &text).is_err());
            }
        }
    }

    #[test]
    fn agy_disabled_row_reads_as_exhausted_and_keeps_reset() {
        let quota = parse(Parser::Agy, None, EXHAUSTED).unwrap();
        assert_eq!(
            quota.remaining,
            Windows::from([("five_hour".into(), 0.0), ("weekly".into(), 0.0)])
        );
        assert_eq!(
            quota.resets,
            Resets::from([("weekly".into(), "2030-01-08T14:00:00Z".into())])
        );
    }

    #[test]
    fn agy_group_selects_rows_and_weekly_only_groups_parse() {
        let gemini = parse(Parser::Agy, Some("Gemini Models"), EXHAUSTED).unwrap();
        assert_eq!(
            gemini.remaining,
            Windows::from([("five_hour".into(), 55.0), ("weekly".into(), 80.0)])
        );
        assert_eq!(gemini.resets["five_hour"], "2030-01-05T12:00:00Z");
        let model = parse(Parser::Agy, Some("example model"), EXHAUSTED).unwrap();
        assert_eq!(model.remaining, Windows::from([("weekly".into(), 90.0)]));
        assert_eq!(
            model.resets,
            Resets::from([("weekly".into(), "2030-01-07T09:30:00Z".into())])
        );
        let error = parse(Parser::Agy, Some("Absent Group"), EXHAUSTED).unwrap_err();
        assert!(error.contains("no weekly row"), "{error}");
    }

    #[test]
    fn agy_row_without_reset_field_has_no_reset() {
        let quota = parse(
            Parser::Agy,
            None,
            "Claude and GPT models\tWeekly Limit Remaining\t40%",
        )
        .unwrap();
        assert_eq!(quota.remaining, Windows::from([("weekly".into(), 40.0)]));
        assert!(quota.resets.is_empty());
    }

    #[test]
    fn reset_times_must_be_rfc3339() {
        for text in [
            "2030-01-08T14:00:00Z",
            "2030-01-08t14:00:00z",
            "2030-01-08T14:00:00.5+02:00",
        ] {
            assert!(rfc3339(text), "{text}");
        }
        for text in [
            "",
            "later",
            "2030-13-08T14:00:00Z",
            "2030-01-00T14:00:00Z",
            "2030-01-08 14:00:00Z",
            "2030-01-08T14:00:00",
            "2030-01-08T14:00:00.Z",
            "2030-01-08T14:00:00+2:00",
            "2030-01-08T14:00:00Zjunk",
        ] {
            assert!(!rfc3339(text), "{text}");
        }
    }
}
