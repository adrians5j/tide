//! Small helpers shared by the agent panel and the review-comment store: the
//! Claude Code project-directory slug, and a compact token formatter.

use std::path::{Path, PathBuf};

/// One plan rate-limit window as Claude Code reports it to a statusline script.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    /// 0-100.
    pub pct: f32,
    /// Unix epoch seconds when the window rolls over.
    pub resets_at: i64,
}

/// The plan windows plus when we last saw them. Only Claude.ai subscriptions
/// report these, and only after the session's first API response — so every
/// field here is legitimately absent much of the time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// mtime of the dump, so a stale file can be greyed out instead of lying.
    pub seen: i64,
}

/// Where the statusline shim drops its payload.
pub fn limits_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".config/tide/usage.json"))
}

fn window(v: &serde_json::Value) -> Option<Window> {
    let pct = v.get("used_percentage")?.as_f64()? as f32;
    Some(Window { pct: pct.clamp(0.0, 100.0), resets_at: v.get("resets_at").and_then(|x| x.as_i64()).unwrap_or(0) })
}

/// Parse the statusline JSON Claude Code hands its status command.
pub fn parse_limits(text: &str, seen: i64) -> Option<Limits> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let rl = v.get("rate_limits")?;
    let out = Limits {
        five_hour: rl.get("five_hour").and_then(window),
        seven_day: rl.get("seven_day").and_then(window),
        seen,
    };
    (out.five_hour.is_some() || out.seven_day.is_some()).then_some(out)
}

/// Read the last statusline payload, if the shim is installed and has run.
pub fn load_limits() -> Option<Limits> {
    let path = limits_path()?;
    let seen = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    parse_limits(&std::fs::read_to_string(&path).ok()?, seen)
}

/// `Aug 27 at 12:30pm` in the machine's own timezone. `date` knows the rules;
/// re-deriving them from an offset would be wrong twice a year.
/// How long until `epoch`, as "4h 20m" or "35m". Empty when there's no
/// timestamp, and "any moment" once it's in the past but the reading hasn't
/// caught up — the wait is the number you actually want when you're rationing.
pub fn fmt_left(epoch: i64) -> String {
    if epoch <= 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = epoch - now;
    if secs <= 0 {
        return "any moment".into();
    }
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, 0) => "under a minute".into(),
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

pub fn fmt_reset(epoch: i64) -> String {
    if epoch <= 0 {
        return String::new();
    }
    std::process::Command::new("date")
        .args(["-r", &epoch.to_string(), "+%b %-d at %-I:%M%p"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_lowercase().replace("am", "am").replace("pm", "pm"))
        .map(|s| {
            // "aug 27 at 12:30pm" → "Aug 27 at 12:30pm"
            let mut c = s.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => s,
            }
        })
        .unwrap_or_default()
}

/// The statusline shim TIDE installs: it copies the payload Claude Code sends,
/// then runs whatever status command you had, so an existing status line keeps
/// working unchanged.
pub const STATUSLINE_SHIM: &str = r#"#!/bin/sh
# Installed by TIDE. Claude Code pipes its statusline JSON in; we keep a copy
# (that's where the plan usage bars come from) and hand it to the real status
# command, if one was passed as arguments.
dir="$HOME/.config/tide"
mkdir -p "$dir"
input=$(cat)
printf '%s' "$input" > "$dir/usage.json.tmp" && mv "$dir/usage.json.tmp" "$dir/usage.json"
[ "$#" -gt 0 ] && printf '%s' "$input" | "$@"
exit 0
"#;



/// Claude Code's per-project session directory name: the path with every
/// non-alphanumeric character replaced by '-'.
pub fn project_slug(root: &Path) -> String {
    root.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// `1.2M` / `847.3k` / `912` — compact enough for a topbar chip.
pub fn short(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wait, not the clock time: that's the number you ration against.
    #[test]
    fn time_left_reads_as_a_wait() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(fmt_left(now + 4 * 3600 + 20 * 60 + 5), "4h 20m");
        assert_eq!(fmt_left(now + 35 * 60), "35m");
        assert_eq!(fmt_left(now + 2 * 3600), "2h");
        assert_eq!(fmt_left(now + 20), "under a minute");
        assert_eq!(fmt_left(now - 60), "any moment");
        assert!(fmt_left(0).is_empty());
    }

    #[test]
    fn short_reads_at_a_glance() {
        assert_eq!(short(912), "912");
        assert_eq!(short(847_300), "847.3k");
        assert_eq!(short(2_450_000), "2.5M");
    }


    #[test]
    fn plan_windows_come_out_of_the_statusline_payload() {
        let json = r#"{"session_id":"x","rate_limits":{"five_hour":{"used_percentage":54,"resets_at":1738425600},"seven_day":{"used_percentage":69.5,"resets_at":1738857600}}}"#;
        let l = parse_limits(json, 7).expect("windows");
        assert_eq!(l.five_hour.unwrap().pct, 54.0);
        assert_eq!(l.seven_day.unwrap().resets_at, 1738857600);
        assert_eq!(l.seen, 7);
    }

    /// API-key sessions and pre-first-response sessions have no windows at all —
    /// that must read as "unknown", not as 0%.
    #[test]
    fn a_payload_without_limits_is_not_a_zero_reading() {
        assert!(parse_limits(r#"{"session_id":"x"}"#, 0).is_none());
        assert!(parse_limits(r#"{"rate_limits":{}}"#, 0).is_none());
    }

    /// One window can be present while the other is absent.
    #[test]
    fn one_window_is_enough() {
        let l = parse_limits(r#"{"rate_limits":{"five_hour":{"used_percentage":12,"resets_at":1}}}"#, 0)
            .expect("five hour");
        assert!(l.seven_day.is_none());
    }

    #[test]
    fn the_slug_matches_claude_codes_project_dir_naming() {
        assert_eq!(project_slug(Path::new("/Users/a/dev/tide")), "-Users-a-dev-tide");
    }
}
