//! Where the tokens went.
//!
//! Claude Code writes one JSONL file per session under
//! `~/.claude/projects/<slug>/<session-id>.jsonl`, and every assistant message
//! in it carries the usage the API reported: fresh input, cache written, cache
//! read, output. That is the only record of what a turn cost, and it is per
//! session, which means per workspace and per job: the reviewer sessions TIDE
//! opens have their own files, and `sess.rs` knows which ids those are.
//!
//! This module reads those files and adds the numbers up by hour. The files are
//! append-only and together run to gigabytes, so a scan remembers how far into
//! each one it got (`~/.config/tide/token-scan.json`) and reads only what has
//! been added since. A first scan of 1.7GB takes a few seconds; the ones after
//! it read a few megabytes.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// What one turn, one session or one hour cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Use {
    /// Input the model had not seen before.
    pub input: u64,
    /// Input written into the prompt cache, billed above the input rate.
    pub cache_write: u64,
    /// Input served from the cache, billed well under it.
    pub cache_read: u64,
    pub output: u64,
    /// Assistant messages, which is requests: the shape of a turn, not its size.
    pub calls: u64,
}

impl Use {
    pub fn add(&mut self, o: &Use) {
        self.input += o.input;
        self.cache_write += o.cache_write;
        self.cache_read += o.cache_read;
        self.output += o.output;
        self.calls += o.calls;
    }

    pub fn tokens(&self) -> u64 {
        self.input + self.cache_write + self.cache_read + self.output
    }

    /// One number to sort and draw by, at the published price ratios: cache
    /// reads a tenth of input, cache writes a quarter above it, output five
    /// times it. An estimate of what a stretch of work was worth, not a bill,
    /// and not what the plan's own meter counts.
    pub fn weighted(&self) -> u64 {
        self.input
            + self.cache_write * 5 / 4
            + self.cache_read / 10
            + self.output * 5
    }

    pub fn is_empty(&self) -> bool {
        self.calls == 0 && self.tokens() == 0
    }
}

/// One session's spend, bucketed by the hour it fell in.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Session {
    pub id: String,
    /// The working directory the session ran in, as the log reports it.
    pub root: String,
    pub first: i64,
    pub last: i64,
    /// Unix seconds at the top of the hour, and what that hour cost. Sorted.
    pub hours: Vec<(i64, Use)>,
    pub total: Use,
}

impl Session {
    fn bump(&mut self, when: i64, u: Use) {
        let hour = when - when.rem_euclid(3600);
        match self.hours.binary_search_by_key(&hour, |(h, _)| *h) {
            Ok(i) => self.hours[i].1.add(&u),
            Err(i) => self.hours.insert(i, (hour, u)),
        }
        self.total.add(&u);
        if self.first == 0 || when < self.first {
            self.first = when;
        }
        self.last = self.last.max(when);
    }

    /// The first hour inside `[from, to)` this session spent anything in. A
    /// session can run for days, so when it started is rarely when it cost
    /// anything: the window is what is being asked about.
    pub fn first_in(&self, from: i64, to: i64) -> i64 {
        self.hours
            .iter()
            .find(|(h, u)| *h >= from && *h < to && !u.is_empty())
            .map(|(h, _)| *h)
            .unwrap_or(self.first)
    }

    /// What this session spent in `[from, to)`.
    pub fn between(&self, from: i64, to: i64) -> Use {
        let mut out = Use::default();
        for (_, u) in self.hours.iter().filter(|(h, _)| *h >= from && *h < to) {
            out.add(u);
        }
        out
    }
}

/// Every session that has been read, newest activity first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub sessions: Vec<Session>,
    /// Files opened by the scan that produced this, and bytes read from them.
    pub files: usize,
    pub bytes: u64,
    /// When the scan ran.
    pub at: i64,
}

impl Report {
    /// Spend per workspace in `[from, to)`, biggest first.
    pub fn by_root(&self, from: i64, to: i64) -> Vec<(String, Use)> {
        let mut map: HashMap<&str, Use> = HashMap::new();
        for s in &self.sessions {
            let u = s.between(from, to);
            if !u.is_empty() {
                map.entry(s.root.as_str()).or_default().add(&u);
            }
        }
        sorted(map.into_iter().map(|(k, v)| (k.to_string(), v)))
    }

    /// Spend per hour in `[from, to)`, oldest first, with the empty hours in
    /// place: a chart of what was spent when needs the gaps as much as the bars.
    pub fn by_hour(&self, from: i64, to: i64) -> Vec<(i64, Use)> {
        let from = from - from.rem_euclid(3600);
        let mut out: Vec<(i64, Use)> =
            (from..to).step_by(3600).map(|h| (h, Use::default())).collect();
        for s in &self.sessions {
            for (h, u) in &s.hours {
                if let Ok(i) = out.binary_search_by_key(h, |(h, _)| *h) {
                    out[i].1.add(u);
                }
            }
        }
        out
    }

    /// The sessions that spent anything in `[from, to)`, biggest first.
    pub fn top(&self, from: i64, to: i64) -> Vec<(&Session, Use)> {
        let mut rows: Vec<(&Session, Use)> =
            self.sessions.iter().map(|s| (s, s.between(from, to))).filter(|(_, u)| !u.is_empty()).collect();
        rows.sort_by(|a, b| b.1.weighted().cmp(&a.1.weighted()));
        rows
    }

    pub fn total(&self, from: i64, to: i64) -> Use {
        let mut out = Use::default();
        for s in &self.sessions {
            out.add(&s.between(from, to));
        }
        out
    }
}

fn sorted(it: impl Iterator<Item = (String, Use)>) -> Vec<(String, Use)> {
    let mut v: Vec<(String, Use)> = it.collect();
    v.sort_by(|a, b| b.1.weighted().cmp(&a.1.weighted()));
    v
}

/// The usage out of one assistant line, with the time it happened.
///
/// Every other line in the file (user turns, tool results, queue operations)
/// has no usage on it and is skipped by the caller before this is reached.
pub fn read_line(line: &str) -> Option<(i64, String, Use)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let u = v.get("message")?.get("usage")?;
    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let when = epoch(v.get("timestamp")?.as_str()?)?;
    let root = v.get("cwd").and_then(|c| c.as_str()).unwrap_or_default().to_string();
    Some((when, root, Use {
        input: n("input_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        cache_read: n("cache_read_input_tokens"),
        output: n("output_tokens"),
        calls: 1,
    }))
}

/// `2026-11-05T11:16:01.291Z` as unix seconds. The logs are always UTC with
/// that exact shape, so this reads it rather than linking a date library.
pub fn epoch(stamp: &str) -> Option<i64> {
    let b = stamp.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |a: usize, z: usize| stamp.get(a..z)?.parse::<i64>().ok();
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Days since 1970-01-01 for a civil date, by Howard Hinnant's algorithm.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The machine's offset from UTC right now, in seconds, for drawing an hour in
/// the reader's own day. `date` knows the rules; an offset worked out from the
/// epoch would be wrong twice a year.
pub fn local_offset() -> i64 {
    static CACHE: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *CACHE.get_or_init(read_offset)
}

fn read_offset() -> i64 {
    let out = std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let s = out.trim();
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => return 0,
    };
    let h: i64 = rest.get(..2).and_then(|x| x.parse().ok()).unwrap_or(0);
    let m: i64 = rest.get(2..4).and_then(|x| x.parse().ok()).unwrap_or(0);
    sign * (h * 3600 + m * 60)
}

/// `14:00` for an hour bucket, in local time.
pub fn hour_label(hour: i64, offset: i64) -> String {
    let local = hour + offset;
    let h = local.rem_euclid(86_400) / 3600;
    format!("{h:02}:00")
}

/// `Mon 14:00`, for a bucket far enough back that the clock alone is ambiguous.
pub fn day_label(hour: i64, offset: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let local = hour + offset;
    let day = local.div_euclid(86_400).rem_euclid(7) as usize;
    format!("{} {}", DAYS[day], hour_label(hour, offset))
}

fn logs_dir() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".claude/projects"))
}

fn cache_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var("HOME").ok()?).join(".config/tide/token-scan.json"))
}

/// What the last scan read of one file, so the next one starts where it stopped.
#[derive(Clone, Debug, Default, PartialEq)]
struct Cached {
    size: u64,
    session: Session,
}

fn to_json(map: &HashMap<String, Cached>) -> String {
    let files: Vec<serde_json::Value> = map
        .iter()
        .map(|(path, c)| {
            let hours: Vec<serde_json::Value> = c
                .session
                .hours
                .iter()
                .map(|(h, u)| {
                    serde_json::json!([h, u.input, u.cache_write, u.cache_read, u.output, u.calls])
                })
                .collect();
            serde_json::json!({
                "path": path,
                "size": c.size,
                "id": c.session.id,
                "root": c.session.root,
                "hours": hours,
            })
        })
        .collect();
    serde_json::to_string(&serde_json::json!({ "files": files })).unwrap_or_else(|_| "{}".into())
}

fn from_json(body: &str) -> HashMap<String, Cached> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else { return HashMap::new() };
    let mut out = HashMap::new();
    for f in v.get("files").and_then(|f| f.as_array()).into_iter().flatten() {
        let Some(path) = f.get("path").and_then(|p| p.as_str()) else { continue };
        let mut session = Session {
            id: f.get("id").and_then(|i| i.as_str()).unwrap_or_default().to_string(),
            root: f.get("root").and_then(|r| r.as_str()).unwrap_or_default().to_string(),
            ..Session::default()
        };
        for h in f.get("hours").and_then(|h| h.as_array()).into_iter().flatten() {
            let a = h.as_array().filter(|a| a.len() >= 6);
            let Some(a) = a else { continue };
            let n = |i: usize| a[i].as_u64().unwrap_or(0);
            let hour = a[0].as_i64().unwrap_or(0);
            let u = Use {
                input: n(1),
                cache_write: n(2),
                cache_read: n(3),
                output: n(4),
                calls: n(5),
            };
            session.hours.push((hour, u));
            session.total.add(&u);
            if session.first == 0 {
                session.first = hour;
            }
            session.last = session.last.max(hour);
        }
        session.hours.sort_by_key(|(h, _)| *h);
        out.insert(path.to_string(), Cached {
            size: f.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
            session,
        });
    }
    out
}

/// Read what has been added to the session logs since the last scan and add it
/// to what was already counted.
///
/// `keep` is how far back to hold on to, in seconds: anything whose file has
/// not been touched inside that is dropped from the cache and never reopened.
pub fn scan(keep: i64) -> Report {
    let at = now();
    let mut cache = cache_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|b| from_json(&b))
        .unwrap_or_default();
    let mut files = 0usize;
    let mut bytes = 0u64;

    let dirs = logs_dir()
        .and_then(|d| std::fs::read_dir(d).ok())
        .map(|it| it.filter_map(Result::ok).map(|e| e.path()).collect::<Vec<_>>())
        .unwrap_or_default();
    let mut seen: Vec<String> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.filter_map(Result::ok) {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            let key = path.to_string_lossy().to_string();
            let touched = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if touched < at - keep {
                continue; // too old to care about, and never reopened
            }
            seen.push(key.clone());
            let size = meta.len();
            let slot = cache.entry(key).or_insert_with(|| Cached {
                size: 0,
                session: Session {
                    id: path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    ..Session::default()
                },
            });
            if slot.size == size {
                continue; // nothing new in it
            }
            // a file that shrank was rotated or rewritten: start it again
            let from = if size < slot.size {
                slot.session = Session { id: slot.session.id.clone(), ..Session::default() };
                0
            } else {
                slot.size
            };
            files += 1;
            bytes += size - from;
            read_into(&path, from, &mut slot.session);
            slot.size = size;
        }
    }

    cache.retain(|k, _| seen.contains(k));
    if let Some(p) = cache_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, to_json(&cache));
    }

    let mut sessions: Vec<Session> = cache.into_values().map(|c| c.session).collect();
    sessions.retain(|s| !s.total.is_empty());
    sessions.sort_by(|a, b| b.last.cmp(&a.last));
    Report { sessions, files, bytes, at }
}

/// Add every usage line from `offset` to the end of `path` into `session`.
fn read_into(path: &Path, offset: u64, session: &mut Session) {
    let Ok(mut f) = std::fs::File::open(path) else { return };
    if offset > 0 && f.seek(SeekFrom::Start(offset)).is_err() {
        return;
    }
    let mut reader = BufReader::with_capacity(1 << 20, f);
    let mut line = String::new();
    loop {
        line.clear();
        // a session log can hold a line megabytes long (a pasted file, a tool
        // result); lossy is for the rare one that isn't valid UTF-8, which is
        // not a usage line anyway
        let mut raw = Vec::new();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // the cheap test first: only assistant messages carry usage, and
        // parsing every line of a gigabyte would cost seconds
        if !has_usage(&raw) {
            continue;
        }
        line.push_str(&String::from_utf8_lossy(&raw));
        if let Some((when, root, u)) = read_line(&line) {
            if session.root.is_empty() {
                session.root = root;
            }
            session.bump(when, u);
        }
    }
}

/// Does this line carry a usage block? A substring search over the raw bytes,
/// which is what keeps a full scan to seconds rather than minutes.
fn has_usage(line: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"\"cache_read_input_tokens\"";
    memchr::memmem::find(line, NEEDLE).is_some()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"type":"assistant","timestamp":"2026-11-05T11:16:01.291Z","sessionId":"abc","cwd":"/Users/a/dev/wby-next9","isSidechain":false,"message":{"model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":27756,"cache_read_input_tokens":13398,"output_tokens":174}}}"#;

    #[test]
    fn a_usage_line_gives_up_its_numbers() {
        let (when, root, u) = read_line(LINE).expect("usage");
        assert_eq!(root, "/Users/a/dev/wby-next9");
        assert_eq!(when, 1793877361);
        assert_eq!(u.input, 2);
        assert_eq!(u.cache_write, 27756);
        assert_eq!(u.cache_read, 13398);
        assert_eq!(u.output, 174);
        assert_eq!(u.calls, 1);
        // the rest of a session log is not usage and must not be counted
        assert!(read_line(r#"{"type":"user","message":{"content":"hi"}}"#).is_none());
        assert!(read_line(r#"{"type":"queue-operation","operation":"enqueue"}"#).is_none());
        assert!(read_line("not json").is_none());
    }

    #[test]
    fn the_cheap_filter_matches_what_the_parser_wants() {
        assert!(has_usage(LINE.as_bytes()));
        assert!(!has_usage(br#"{"type":"user","message":{"content":"hi"}}"#));
        assert!(!has_usage(b""));
    }

    /// The logs are UTC with one shape. A wrong epoch puts a spend in the wrong
    /// hour, which is the whole point of the panel.
    #[test]
    fn the_timestamp_reads_as_utc_seconds() {
        assert_eq!(epoch("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(epoch("2026-11-05T11:16:01.291Z"), Some(1793877361));
        assert_eq!(epoch("2024-02-29T23:59:59Z"), Some(1709251199));
        assert!(epoch("not a date").is_none());
        assert!(epoch("2026-13-05T11:16:01Z").is_none());
    }

    #[test]
    fn an_hour_reads_in_the_local_day() {
        // 11:16 UTC in a +01:00 zone is the 12:00 bucket
        let hour = 1793877361 - 1793877361 % 3600;
        assert_eq!(hour_label(hour, 3600), "12:00");
        assert_eq!(hour_label(hour, -5 * 3600), "06:00");
        // 2026-11-05 was a Thursday
        assert_eq!(day_label(hour, 3600), "Thu 12:00");
    }

    fn sess(id: &str, root: &str, at: i64, out: u64) -> Session {
        let mut s = Session { id: id.into(), root: root.into(), ..Session::default() };
        s.bump(at, Use { input: 10, cache_write: 20, cache_read: 1000, output: out, calls: 1 });
        s
    }

    /// An hour's spend lands in one bucket however many turns it took, and a
    /// window takes only the hours inside it.
    #[test]
    fn spend_adds_up_by_hour_and_by_window() {
        let mut s = sess("a", "/w", 3600 * 10 + 60, 1);
        s.bump(3600 * 10 + 120, Use { output: 1, calls: 1, ..Use::default() });
        s.bump(3600 * 12, Use { output: 2, calls: 1, ..Use::default() });
        assert_eq!(s.hours.len(), 2);
        assert_eq!(s.hours[0].1.calls, 2);
        assert_eq!(s.total.calls, 3);
        assert_eq!(s.between(3600 * 10, 3600 * 11).output, 2);
        assert_eq!(s.between(3600 * 11, 3600 * 13).output, 2);
        assert_eq!(s.between(0, 3600).output, 0);
    }

    #[test]
    fn a_report_groups_by_workspace_and_leaves_the_quiet_hours_in() {
        let r = Report {
            sessions: vec![
                sess("a", "/w1", 3600 * 10, 100),
                sess("b", "/w1", 3600 * 10, 1),
                sess("c", "/w2", 3600 * 12, 1),
            ],
            ..Report::default()
        };
        let roots = r.by_root(0, 3600 * 24);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].0, "/w1"); // the bigger one leads
        assert_eq!(roots[0].1.calls, 2);
        let hours = r.by_hour(3600 * 9, 3600 * 13);
        assert_eq!(hours.len(), 4);
        assert!(hours[0].1.is_empty()); // 09:00 had nothing, and says so
        assert_eq!(hours[1].1.calls, 2); // 10:00
        assert_eq!(hours[3].1.calls, 1); // 12:00
        let top = r.top(0, 3600 * 24);
        assert_eq!(top[0].0.id, "a"); // 100 output tokens, weighted above the rest
        // the hour it spent in the window, not the day the session opened
        assert_eq!(top[0].0.first_in(3600 * 9, 3600 * 24), 3600 * 10);
        assert_eq!(r.total(0, 3600 * 24).calls, 3);
        // and a window with nothing in it is empty rather than everything
        assert!(r.by_root(0, 3600).is_empty());
    }

    /// The weighting is what the panel sorts by, so it has to be the published
    /// ratios and not a token count: 1M cache reads is not 1M of work.
    #[test]
    fn the_weighting_follows_the_price_ratios() {
        let u = Use { input: 100, cache_write: 100, cache_read: 1000, output: 100, calls: 1 };
        assert_eq!(u.tokens(), 1300);
        assert_eq!(u.weighted(), 100 + 125 + 100 + 500);
        assert!(Use::default().is_empty());
    }

    #[test]
    fn the_cache_survives_the_round_trip() {
        let mut map = HashMap::new();
        map.insert("/p/a.jsonl".to_string(), Cached {
            size: 1234,
            session: sess("a", "/w1", 3600 * 10, 7),
        });
        let back = from_json(&to_json(&map));
        assert_eq!(back.len(), 1);
        let got = &back["/p/a.jsonl"];
        assert_eq!(got.size, 1234);
        assert_eq!(got.session.id, "a");
        assert_eq!(got.session.root, "/w1");
        assert_eq!(got.session.total.output, 7);
        assert_eq!(got.session.hours.len(), 1);
        // a file somebody has broken reads as nothing counted yet, not a panic
        assert!(from_json("not json").is_empty());
        assert!(from_json(r#"{"files":[{"size":1}]}"#).is_empty());
    }
}
