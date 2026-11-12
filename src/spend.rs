//! Where TIDE's own tokens went, by the thing in the window that spent them.
//!
//! `tokens.rs` answers "which workspace, which session, which hour". It cannot
//! answer "which part of TIDE", because a question typed in the agent pane, a
//! question asked under a code tour stop and a brief TIDE requested all land in
//! the same session log, in the same hour, under the same workspace. The log
//! has no idea which button was pressed.
//!
//! TIDE does. So every prompt it sends is written down as it goes out, with the
//! name of the thing that sent it and the byte the session log had reached at
//! that moment. When the turn ends, everything appended to the log after that
//! byte is the turn, and the usage lines in it are what the turn cost. One line
//! of JSON per turn goes to `~/.config/tide/spend.jsonl`:
//!
//! ```json
//! {"at":1794512345,"secs":42,"kind":"stop-ask","job":"","tide":false,
//!  "root":"/Users/you/dev/app","sid":"fc9eec45","in":2,"cw":27756,"cr":13398,
//!  "out":174,"calls":9}
//! ```
//!
//! The byte offset is what makes this exact and cheap: a turn's reading is the
//! few kilobytes it appended, not a scan of a log that runs to megabytes. A
//! measurement that misses costs one line of a report and nothing else.

use crate::tokens::{self, Use};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Typed in the agent pane.
pub const PANE: &str = "pane";
/// Asked under a stop on a code tour.
pub const STOP_ASK: &str = "stop-ask";
/// Asked in a review comment thread, in the editor.
pub const COMMENT: &str = "comment";
/// Asked under a slide of a brief.
pub const SLIDE_ASK: &str = "slide-ask";
/// Asked from the phone.
pub const PHONE: &str = "phone";
/// A to-do sent to the agent.
pub const TODO: &str = "todo";
/// Something TIDE sent on its own: a brief, a tour, a merge note, a review
/// round. `job` says which, when the send named one.
pub const JOB: &str = "job";

/// How big the log gets before the older half of it is dropped. A turn is
/// about 200 bytes, so this is tens of thousands of them.
const MAX_BYTES: u64 = 4 << 20;

/// A prompt on its way out: what the turn it starts will need to measure
/// itself when it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    /// The part of TIDE that sent it, one of the constants above.
    pub kind: String,
    /// The job it belongs to, when TIDE started it as one ("Brief of #6176").
    pub job: String,
    /// What on screen this turn belongs to, when something does: `brief:6236`,
    /// `slide:6236:4`, `stop:6200:7`. The thing itself then asks what it cost
    /// without having to guess which turn was its own. Empty for a turn that
    /// belongs to the pane and nothing else.
    pub tag: String,
    /// `true` when TIDE sent it rather than you typing it.
    pub by_tide: bool,
    pub root: PathBuf,
    pub sid: String,
    /// Bytes the session log had reached when the prompt went out. Everything
    /// after this is the turn.
    pub from: u64,
    /// Unix seconds.
    pub at: i64,
}

/// One measured turn, as a line of the log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub at: i64,
    /// Wall clock the turn took.
    pub secs: i64,
    pub kind: String,
    pub job: String,
    pub tag: String,
    pub by_tide: bool,
    pub root: String,
    pub sid: String,
    pub used: Use,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/spend.jsonl"))
}

/// Where the log of the session being prompted is, and how far it has got.
pub fn open(root: &Path, sid: &str, kind: &str, job: &str, tag: &str, by_tide: bool) -> Mark {
    let from = tokens::session_log(root, sid)
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);
    Mark {
        kind: kind.to_string(),
        job: job.to_string(),
        tag: tag.to_string(),
        by_tide,
        root: root.to_path_buf(),
        sid: sid.to_string(),
        from,
        at: now(),
    }
}

/// What the turn this mark opened cost: the usage lines the session log has
/// grown since the prompt went out.
pub fn measure(mark: &Mark) -> Use {
    let mut u = Use::default();
    let Some(p) = tokens::session_log(&mark.root, &mark.sid) else { return u };
    let Ok(mut f) = std::fs::File::open(&p) else { return u };
    if mark.from > 0 && f.seek(SeekFrom::Start(mark.from)).is_err() {
        return u;
    }
    for line in BufReader::with_capacity(1 << 16, f).lines().map_while(Result::ok) {
        if let Some((_, _, used)) = tokens::read_line(&line) {
            u.add(&used);
        }
    }
    u
}

/// Write the turn down. A turn that spent nothing is still a line: a feature
/// that asks a lot and is answered from cache is worth seeing as that, rather
/// than as nothing at all.
pub fn record(mark: &Mark, used: &Use, secs: i64) {
    let row = Row {
        at: mark.at,
        secs,
        kind: mark.kind.clone(),
        job: mark.job.clone(),
        tag: mark.tag.clone(),
        by_tide: mark.by_tide,
        root: mark.root.to_string_lossy().to_string(),
        sid: mark.sid.clone(),
        used: *used,
    };
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    trim(&p);
    let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) else { return };
    let _ = writeln!(f, "{}", to_line(&row));
}

/// Drop the older half when the file gets long. Append-only files that nothing
/// ever deletes from are how a config directory ends up with a gigabyte in it.
fn trim(p: &Path) {
    let Ok(m) = std::fs::metadata(p) else { return };
    if m.len() <= MAX_BYTES {
        return;
    }
    let Ok(body) = std::fs::read_to_string(p) else { return };
    let lines: Vec<&str> = body.lines().collect();
    let keep = lines.split_at(lines.len() / 2).1.join("\n");
    let _ = std::fs::write(p, format!("{keep}\n"));
}

/// The rows written since byte `from`, and where the file now ends.
///
/// The log only ever grows, so a reader that remembers its offset reads the
/// turns it hasn't seen and nothing else. That is what lets the window ask
/// "what did this brief cost" without parsing megabytes on every frame.
pub fn tail(from: u64) -> (Vec<Row>, u64) {
    let Some(p) = path() else { return (Vec::new(), from) };
    let Ok(m) = std::fs::metadata(&p) else { return (Vec::new(), 0) };
    // trimmed, or a different file than the one that offset was for: start over
    let from = if m.len() < from { 0 } else { from };
    let Ok(mut f) = std::fs::File::open(&p) else { return (Vec::new(), from) };
    if from > 0 && f.seek(SeekFrom::Start(from)).is_err() {
        return (Vec::new(), from);
    }
    let mut body = String::new();
    if std::io::Read::read_to_string(&mut f, &mut body).is_err() {
        return (Vec::new(), from);
    }
    (body.lines().filter_map(from_line).collect(), m.len())
}

pub fn load() -> Vec<Row> {
    let Some(p) = path() else { return Vec::new() };
    let Ok(body) = std::fs::read_to_string(p) else { return Vec::new() };
    body.lines().filter_map(from_line).collect()
}

/// The rows of one stretch, newest end of the log first in the file order they
/// were written in.
pub fn since(rows: &[Row], from: i64, to: i64) -> Vec<Row> {
    rows.iter().filter(|r| r.at >= from && r.at < to).cloned().collect()
}

/// What each kind spent, biggest first: the turns it took, and their usage.
pub fn by_kind(rows: &[Row]) -> Vec<(String, u64, Use)> {
    fold(rows, |r| r.kind.clone())
}

/// The same by job, which names the TIDE sends the way the pane does.
pub fn by_job(rows: &[Row]) -> Vec<(String, u64, Use)> {
    fold(rows, |r| match r.job.is_empty() {
        true => r.kind.clone(),
        false => r.job.clone(),
    })
}

/// The same by workspace.
pub fn by_root(rows: &[Row]) -> Vec<(String, u64, Use)> {
    fold(rows, |r| r.root.clone())
}

fn fold(rows: &[Row], key: impl Fn(&Row) -> String) -> Vec<(String, u64, Use)> {
    let mut out: Vec<(String, u64, Use)> = Vec::new();
    for r in rows {
        let k = key(r);
        match out.iter_mut().find(|(got, _, _)| *got == k) {
            Some((_, turns, u)) => {
                *turns += 1;
                u.add(&r.used);
            }
            None => out.push((k, 1, r.used)),
        }
    }
    out.sort_by(|a, b| b.2.weighted().cmp(&a.2.weighted()).then_with(|| a.0.cmp(&b.0)));
    out
}

pub fn to_line(r: &Row) -> String {
    serde_json::json!({
        "at": r.at,
        "secs": r.secs,
        "kind": r.kind,
        "job": r.job,
        "tag": r.tag,
        "tide": r.by_tide,
        "root": r.root,
        "sid": r.sid,
        "in": r.used.input,
        "cw": r.used.cache_write,
        "cr": r.used.cache_read,
        "out": r.used.output,
        "calls": r.used.calls,
    })
    .to_string()
}

pub fn from_line(line: &str) -> Option<Row> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).unwrap_or("").to_string();
    Some(Row {
        at: v.get("at")?.as_i64()?,
        secs: v.get("secs").and_then(serde_json::Value::as_i64).unwrap_or(0),
        kind: s("kind"),
        job: s("job"),
        tag: s("tag"),
        by_tide: v.get("tide").and_then(serde_json::Value::as_bool).unwrap_or(false),
        root: s("root"),
        sid: s("sid"),
        used: Use {
            input: n("in"),
            cache_write: n("cw"),
            cache_read: n("cr"),
            output: n("out"),
            calls: n("calls"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, w: u64) -> Row {
        Row {
            at: 1_794_500_000,
            secs: 12,
            kind: kind.into(),
            job: String::new(),
            tag: String::new(),
            by_tide: false,
            root: "/w".into(),
            sid: "abc".into(),
            used: Use { input: w, cache_write: 0, cache_read: 0, output: 0, calls: 1 },
        }
    }

    #[test]
    fn a_row_survives_the_round_trip() {
        let mut r = row(STOP_ASK, 120);
        r.job = "Brief of #6176".into();
        r.by_tide = true;
        assert_eq!(from_line(&to_line(&r)), Some(r));
        assert_eq!(from_line("not json"), None);
        assert_eq!(from_line("{}"), None);
    }

    /// The point of the file: what each part of TIDE spent, biggest first.
    #[test]
    fn the_rollup_adds_up_by_kind() {
        let rows = vec![row(PANE, 100), row(STOP_ASK, 500), row(PANE, 50)];
        let by = by_kind(&rows);
        assert_eq!(by[0].0, STOP_ASK);
        assert_eq!(by[0].1, 1);
        assert_eq!(by[1].0, PANE);
        assert_eq!(by[1].1, 2, "two turns in the pane");
        assert_eq!(by[1].2.input, 150);
    }

    /// A job names itself; a turn with no job falls back to the kind, so the
    /// view has no blank row in it.
    #[test]
    fn the_job_view_falls_back_to_the_kind() {
        let mut j = row(JOB, 900);
        j.job = "Brief of #6176".into();
        let rows = vec![j, row(PANE, 100)];
        let by = by_job(&rows);
        assert_eq!(by[0].0, "Brief of #6176");
        assert_eq!(by[1].0, PANE);
    }

    #[test]
    fn a_window_takes_the_rows_inside_it() {
        let mut early = row(PANE, 10);
        early.at = 1_794_000_000;
        let rows = vec![early, row(PANE, 20)];
        let got = since(&rows, 1_794_400_000, 1_794_600_000);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].used.input, 20);
    }

    /// The turn is what the log grew by, not the whole log: a session with a
    /// day of requests in it must not be billed to one question.
    #[test]
    fn a_turn_is_measured_from_the_byte_the_prompt_went_out_at() {
        let dir = std::env::temp_dir().join(format!("tide-spend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sess.jsonl");
        let line = |out: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"2026-11-12T11:16:01.291Z","cwd":"/w","message":{{"usage":{{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":{out}}}}}}}"#
            )
        };
        let before = format!("{}\n", line(1000));
        std::fs::write(&path, &before).unwrap();
        let from = before.len() as u64;
        std::fs::write(&path, format!("{before}{}\n{}\n", line(7), line(5))).unwrap();

        let mut u = Use::default();
        let mut f = std::fs::File::open(&path).unwrap();
        f.seek(SeekFrom::Start(from)).unwrap();
        for l in BufReader::new(f).lines().map_while(Result::ok) {
            if let Some((_, _, used)) = tokens::read_line(&l) {
                u.add(&used);
            }
        }
        assert_eq!(u.output, 12, "only the two lines the turn added");
        assert_eq!(u.calls, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
