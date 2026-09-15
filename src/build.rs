//! Reads the newline-delimited JSON that `yarn build --json` emits (webiny-js
//! #5629) and turns it into progress a bar can be drawn from, plus one readable
//! line per event for the run console.
//!
//! Every event is `{"type": "...", "ts": ..., ...}`. The build sends
//! `completed`/`total`/`progress` on `package:end` and `batch:skipped`, so we
//! never count packages ourselves — and `build:end` always arrives, even when
//! there was nothing to build.

use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    /// Packages that have to be built (cached ones aren't part of the bar).
    pub total: usize,
    pub completed: usize,
    /// Packages served from the build cache — worth showing, never "work".
    pub from_cache: usize,
    pub batch: usize,
    pub batches: usize,
    /// Packages currently building, in start order.
    pub building: Vec<String>,
    pub failed: Vec<String>,
    pub done: bool,
    pub success: bool,
}

impl Progress {
    /// 0.0-1.0. A build with nothing to do is complete, not stuck at zero.
    pub fn fraction(&self) -> f32 {
        if self.done {
            return 1.0;
        }
        if self.total == 0 {
            return 0.0;
        }
        (self.completed as f32 / self.total as f32).clamp(0.0, 1.0)
    }

    /// What the toast and the console header say, e.g. `14/58 · batch 3/7`.
    pub fn summary(&self) -> String {
        if self.done {
            let n = self.completed;
            return match self.failed.len() {
                0 => format!("{n} built"),
                f => format!("{n} built · {f} failed"),
            };
        }
        let mut s = format!("{}/{}", self.completed, self.total);
        if self.batches > 1 {
            s.push_str(&format!("  ·  batch {}/{}", self.batch, self.batches));
        }
        if let Some(first) = self.building.first() {
            s.push_str(&format!("  ·  {first}"));
            if self.building.len() > 1 {
                s.push_str(&format!(" +{}", self.building.len() - 1));
            }
        }
        s
    }

    /// Fold one line of the stream in. Returns the console line to append, if
    /// the event is worth showing. A line that isn't build JSON returns it
    /// unchanged, so a stray warning from the shell still reaches the console.
    pub fn apply(&mut self, line: &str) -> Option<String> {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            return (!trimmed.is_empty()).then(|| line.to_string());
        }
        let Ok(ev) = serde_json::from_str::<Value>(trimmed) else {
            return Some(line.to_string());
        };
        let Some(kind) = ev.get("type").and_then(Value::as_str) else {
            return Some(line.to_string());
        };
        let num = |k: &str| ev.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
        let text = |k: &str| ev.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let list = |k: &str| -> Vec<String> {
            ev.get(k)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };

        match kind {
            "build:info" => {
                let runner = text("runner");
                let tsc = text("tscVersion");
                Some(format!("{runner} · tsc {tsc}"))
            }
            "build:plan" => {
                self.total = num("packagesToBuild");
                self.from_cache = num("packagesFromCache");
                self.batches = num("batches");
                self.completed = 0;
                self.failed.clear();
                self.building.clear();
                self.done = false;
                let mut s = format!("{} package(s) to build", self.total);
                if self.from_cache > 0 {
                    s.push_str(&format!(", {} from cache", self.from_cache));
                }
                if self.batches > 1 {
                    s.push_str(&format!(", in {} batches", self.batches));
                }
                Some(s)
            }
            "batch:start" => {
                self.batch = num("batch");
                self.batches = num("totalBatches").max(self.batches);
                None // the bar says which batch; a line per batch is noise
            }
            "package:start" => {
                let name = text("name");
                if !name.is_empty() && !self.building.contains(&name) {
                    self.building.push(name);
                }
                None
            }
            "package:end" => {
                let name = text("name");
                self.building.retain(|p| *p != name);
                self.completed = num("completed").max(self.completed);
                if num("total") > 0 {
                    self.total = num("total");
                }
                let failed = !ev.get("success").and_then(Value::as_bool).unwrap_or(true);
                let secs = ev.get("duration").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0;
                if failed {
                    self.failed.push(name.clone());
                    let err = text("error");
                    return Some(format!("✕ {name} ({secs:.1}s) {err}"));
                }
                Some(format!("✓ {name} ({secs:.1}s)"))
            }
            "batch:skipped" => {
                self.completed = num("completed").max(self.completed);
                let pkgs = list("packages");
                Some(format!("skipped {} package(s) after a failure", pkgs.len()))
            }
            "batch:end" => None,
            "build:end" => {
                self.done = true;
                self.success = ev.get("success").and_then(Value::as_bool).unwrap_or(false);
                self.building.clear();
                let secs = ev.get("duration").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0;
                let built = num("built");
                let failed = num("failed");
                let skipped = num("skipped");
                let mut s = format!("build {} in {secs:.1}s — {built} built", if self.success { "ok" } else { "failed" });
                if failed > 0 {
                    s.push_str(&format!(", {failed} failed"));
                }
                if skipped > 0 {
                    s.push_str(&format!(", {skipped} skipped"));
                }
                for e in ev.get("errors").and_then(Value::as_array).into_iter().flatten() {
                    if let (Some(n), Some(m)) = (
                        e.get("name").and_then(Value::as_str),
                        e.get("message").and_then(Value::as_str),
                    ) {
                        s.push_str(&format!("\n  {n}: {m}"));
                    }
                }
                Some(s)
            }
            "log" => Some(text("message")).filter(|m| !m.is_empty()),
            _ => None,
        }
    }
}

/// What `yarn build --preview --json` reports: what a build would do right now,
/// without building anything. Cheap (~1.5s) and side-effect free, so it can be
/// polled to keep a "needs rebuild" indicator honest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Preview {
    pub total: usize,
    pub up_to_date: usize,
    pub to_build: usize,
    pub batches: usize,
    /// The packages that would build, when the build lists them.
    pub packages: Vec<String>,
}

/// Pull the `build:preview` event out of a `--preview --json` run. Scans the
/// lines rather than assuming stdout is exactly one, so a stray yarn warning
/// can't break it.
pub fn parse_preview(out: &str) -> Option<Preview> {
    for line in out.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        if ev.get("type").and_then(Value::as_str) != Some("build:preview") {
            continue;
        }
        let num = |k: &str| ev.get(k).and_then(Value::as_u64).unwrap_or(0) as usize;
        return Some(Preview {
            total: num("totalPackages"),
            up_to_date: num("upToDate"),
            to_build: num("packagesToBuild"),
            batches: num("batches"),
            packages: ev
                .get("packages")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        });
    }
    None
}

/// Does this command run the package build, so `--json` is worth adding?
/// Matches the aliases in use (`yb`, `yyb`, `yybnc`) and the underlying script.
pub fn is_build_command(cmd: &str) -> bool {
    let head = cmd.split_whitespace().next().unwrap_or("");
    matches!(head, "yb" | "yyb" | "yybnc" | "ybf")
        || cmd.contains("webiny-build")
        || (cmd.contains("yarn") && cmd.contains("build"))
}

/// The command with `--json` appended, unless it's already there.
pub fn with_json(cmd: &str) -> String {
    if cmd.contains("--json") {
        return cmd.to_string();
    }
    format!("{cmd} --json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(p: &mut Progress, line: &str) -> Option<String> {
        p.apply(line)
    }

    #[test]
    fn the_plan_sets_the_bar_and_the_cache_count() {
        let mut p = Progress::default();
        let out = ev(
            &mut p,
            r#"{"type":"build:plan","ts":1,"packagesToBuild":4,"packagesFromCache":10,"batches":2}"#,
        );
        assert_eq!(p.total, 4);
        assert_eq!(p.from_cache, 10);
        assert_eq!(p.batches, 2);
        assert_eq!(out.unwrap(), "4 package(s) to build, 10 from cache, in 2 batches");
        assert_eq!(p.fraction(), 0.0);
    }

    /// The build reports `completed`/`total` itself; we take its word for it.
    #[test]
    fn package_end_advances_the_bar_and_logs_a_line() {
        let mut p = Progress::default();
        ev(&mut p, r#"{"type":"build:plan","packagesToBuild":2,"packagesFromCache":0,"batches":1}"#);
        ev(&mut p, r#"{"type":"package:start","name":"api-core","batch":1}"#);
        assert_eq!(p.building, vec!["api-core".to_string()]);
        let out = ev(
            &mut p,
            r#"{"type":"package:end","name":"api-core","batch":1,"duration":1500,"success":true,"completed":1,"total":2,"progress":0.5}"#,
        );
        assert_eq!(out.unwrap(), "✓ api-core (1.5s)");
        assert!(p.building.is_empty());
        assert_eq!(p.fraction(), 0.5);
        assert_eq!(p.summary(), "1/2");
    }

    #[test]
    fn a_failed_package_is_named_in_the_line_and_remembered() {
        let mut p = Progress::default();
        ev(&mut p, r#"{"type":"build:plan","packagesToBuild":1,"packagesFromCache":0,"batches":1}"#);
        let out = ev(
            &mut p,
            r#"{"type":"package:end","name":"app-aco","batch":1,"duration":800,"success":false,"error":"TS2345","completed":1,"total":1}"#,
        );
        assert_eq!(out.unwrap(), "✕ app-aco (0.8s) TS2345");
        assert_eq!(p.failed, vec!["app-aco".to_string()]);
    }

    /// A build with nothing to do still ends, and the bar must read full rather
    /// than sitting at zero forever.
    #[test]
    fn an_empty_build_finishes_at_full() {
        let mut p = Progress::default();
        ev(&mut p, r#"{"type":"build:plan","packagesToBuild":0,"packagesFromCache":58,"batches":0}"#);
        assert_eq!(p.fraction(), 0.0);
        let out = ev(
            &mut p,
            r#"{"type":"build:end","success":true,"duration":1200,"built":0,"failed":0,"skipped":0,"skippedPackages":[],"errors":[]}"#,
        );
        assert!(p.done && p.success);
        assert_eq!(p.fraction(), 1.0);
        assert_eq!(out.unwrap(), "build ok in 1.2s — 0 built");
    }

    #[test]
    fn skipped_batches_keep_the_bar_moving_to_the_end() {
        let mut p = Progress::default();
        ev(&mut p, r#"{"type":"build:plan","packagesToBuild":4,"packagesFromCache":0,"batches":2}"#);
        let out = ev(
            &mut p,
            r#"{"type":"batch:skipped","batch":2,"totalBatches":2,"packages":["a","b","c"],"completed":4,"total":4,"progress":1}"#,
        );
        assert_eq!(out.unwrap(), "skipped 3 package(s) after a failure");
        assert_eq!(p.fraction(), 1.0);
    }

    /// Anything that isn't build JSON still reaches the console — a shell
    /// warning before the build starts, say.
    #[test]
    fn plain_output_passes_through() {
        let mut p = Progress::default();
        assert_eq!(ev(&mut p, "warning: something"), Some("warning: something".to_string()));
        assert_eq!(ev(&mut p, "   "), None);
        // malformed JSON is shown rather than swallowed
        assert_eq!(ev(&mut p, "{not json"), Some("{not json".to_string()));
    }

    /// Shape taken from a real `yb --json` run where everything was cached: no
    /// `skipped` key at all, and `packagesToBuild: 0`.
    #[test]
    fn a_fully_cached_run_reads_as_done_not_stalled() {
        let mut p = Progress::default();
        ev(&mut p, r#"{"type":"build:info","ts":1,"projectFolder":"wby-next6","runner":"N/A","parallel":true,"cpuCount":10,"cpuName":"Apple M1 Pro","totalMemory":34359738368,"freeMemory":4110188544,"tscVersion":"Version 7.0.2"}"#);
        let plan = ev(&mut p, r#"{"type":"build:plan","ts":2,"totalPackages":1,"cacheEnabled":true,"packages":[],"cachedPackages":["@webiny/handler"],"restoredPackages":[],"batches":0,"packagesToBuild":0,"packagesFromCache":1}"#);
        assert_eq!(plan.unwrap(), "0 package(s) to build, 1 from cache");
        let end = ev(&mut p, r#"{"type":"build:end","ts":3,"success":true,"duration":0,"built":0,"failed":0,"errors":[]}"#);
        assert_eq!(end.unwrap(), "build ok in 0.0s — 0 built");
        assert_eq!(p.fraction(), 1.0);
        assert_eq!(p.summary(), "0 built");
    }

    /// A real `yarn build --preview --json` line, trimmed to the fields we read.
    #[test]
    fn the_preview_event_is_picked_out_of_the_output() {
        let out = "warning some yarn noise\n{\"type\":\"build:preview\",\"ts\":1,\"totalPackages\":148,\"cacheEnabled\":true,\"upToDate\":117,\"packagesToBuild\":31,\"packages\":[\"@webiny/api-aco\",\"webiny\"],\"batches\":7,\"duration\":1.507}\n";
        let p = parse_preview(out).expect("preview");
        assert_eq!((p.total, p.up_to_date, p.to_build, p.batches), (148, 117, 31, 7));
        assert_eq!(p.packages, vec!["@webiny/api-aco".to_string(), "webiny".to_string()]);
        // nothing to read in a text-mode run
        assert!(parse_preview("There is a total of 148 packages.").is_none());
    }

    #[test]
    fn build_commands_are_recognised_and_get_the_flag() {
        assert!(is_build_command("yyb"));
        assert!(is_build_command("yb -p api-core"));
        assert!(is_build_command("yarn build"));
        assert!(!is_build_command("git pull"));
        assert_eq!(with_json("yyb"), "yyb --json");
        assert_eq!(with_json("yb --json"), "yb --json");
    }
}
