//! Which resumable sessions TIDE opened for itself.
//!
//! One round of `Send in reviewers` opens four ACP sessions against the same
//! project root, and the adapter lists them next to the one you have been
//! typing in. After a few rounds the resume list is mostly reviewers: twenty
//! seven rows, of which maybe eight are yours.
//!
//! So TIDE writes down the id of every session it opens, with the job that
//! opened it. The picker hides those rows behind a toggle, and auto-resume
//! walks past them to the newest session that is actually yours.
//!
//! Sessions opened before any of this was written down are read off their
//! titles by `guess`, which is a guess and is treated as one: a row it gets
//! wrong is a row behind a toggle, one click away.

use std::path::PathBuf;

/// A session TIDE opened, rather than one you did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawned {
    pub id: String,
    /// The job that opened it, as the picker should name it: "footprint",
    /// "code rabbit", and so on.
    pub why: String,
    /// Unix seconds, so the oldest rows are the ones dropped.
    pub when: u64,
}

/// How many are remembered. Four a round, so this is a few hundred rounds,
/// and anything older has long since fallen off the adapter's own list.
const KEEP: usize = 1000;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/spawned-sessions.json"))
}

pub fn load() -> Vec<Spawned> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|body| from_json(&body))
        .unwrap_or_default()
}

pub fn save(list: &[Spawned]) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, to_json(list));
}

/// Write down that TIDE opened this session, and what for.
pub fn remember(id: &str, why: &str) {
    if id.is_empty() {
        return;
    }
    let mut list = load();
    put(&mut list, id, why);
    save(&list);
}

/// The write, without the disk.
pub fn put(list: &mut Vec<Spawned>, id: &str, why: &str) {
    let row = Spawned { id: id.to_string(), why: why.to_string(), when: now() };
    match list.iter_mut().find(|s| s.id == id) {
        Some(slot) => *slot = row,
        None => list.push(row),
    }
    if list.len() > KEEP {
        list.sort_by(|a, b| a.when.cmp(&b.when).then_with(|| a.id.cmp(&b.id)));
        list.drain(..list.len() - KEEP);
    }
}

/// The job that opened this session, or `None` when it is one of yours.
pub fn why(list: &[Spawned], id: &str, title: &str) -> Option<String> {
    match list.iter().find(|s| s.id == id) {
        Some(s) if !s.why.is_empty() => Some(s.why.clone()),
        Some(_) => Some("TIDE".to_string()),
        None => guess(title).map(str::to_string),
    }
}

/// A reviewer session read off its title.
///
/// For history only: every session TIDE opens from here on is written down by
/// id, which is exact. The adapter titles a session from its first prompt, and
/// the four reviewer briefs all open by naming the branch they are reading and
/// the base they are reading it against, so their titles come back as "Branch
/// review against origin/next" or "CodeRabbit review triage against
/// release/6.5.2".
///
/// The rule is deliberately narrow. "review" alone is not enough: you ask for
/// reviews yourself. It has to carry one of the reviewers' own words with it.
pub fn guess(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    if !t.contains("review") {
        return None;
    }
    const MARKS: [&str; 7] = [
        "against ",
        // what the footprint reviewer was called until November 2026
        "slop cop",
        "coderabbit",
        "code rabbit",
        "branch review",
        "architecture",
        "code style",
    ];
    MARKS.iter().any(|m| t.contains(m)).then_some("a reviewer")
}

pub fn to_json(list: &[Spawned]) -> String {
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|s| serde_json::json!({ "id": s.id, "why": s.why, "when": s.when }))
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
}

pub fn from_json(body: &str) -> Vec<Spawned> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(body) else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            let id = v.get("id")?.as_str()?.to_string();
            if id.is_empty() {
                return None;
            }
            Some(Spawned {
                id,
                why: v.get("why").and_then(|w| w.as_str()).unwrap_or_default().to_string(),
                when: v.get("when").and_then(|w| w.as_u64()).unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session TIDE opened is known by its id, and the job that opened it
    /// comes back with it.
    #[test]
    fn a_session_tide_opened_is_known_by_its_id() {
        let mut list = Vec::new();
        put(&mut list, "abc123", "footprint");
        put(&mut list, "def456", "code rabbit");
        assert_eq!(why(&list, "abc123", "Branch review against origin/next").as_deref(), Some("footprint"));
        assert_eq!(why(&list, "def456", "whatever the adapter called it").as_deref(), Some("code rabbit"));
        // one of yours, with nothing in its title to suggest otherwise
        assert!(why(&list, "ghi789", "Gym location race condition fix").is_none());
    }

    /// The same session opened twice is one row, not two.
    #[test]
    fn a_session_is_written_down_once() {
        let mut list = Vec::new();
        put(&mut list, "abc123", "footprint");
        put(&mut list, "abc123", "architecture");
        assert_eq!(list.len(), 1);
        assert_eq!(why(&list, "abc123", "").as_deref(), Some("architecture"));
    }

    /// The file is capped, and it is the oldest rows that go.
    #[test]
    fn the_oldest_rows_are_the_ones_dropped() {
        let mut list = Vec::new();
        for i in 0..KEEP + 5 {
            list.push(Spawned { id: format!("s{i}"), why: "footprint".into(), when: i as u64 });
        }
        put(&mut list, "newest", "code style");
        assert_eq!(list.len(), KEEP);
        assert!(why(&list, "s0", "").is_none());
        assert!(why(&list, "s5", "").is_none());
        assert!(why(&list, "s6", "").is_some());
        assert_eq!(why(&list, "newest", "").as_deref(), Some("code style"));
    }

    /// Reviewer sessions from before any of this was written down are read off
    /// their titles, and the titles of your own work are left alone.
    #[test]
    fn an_old_reviewer_session_is_read_off_its_title() {
        for t in [
            "CodeRabbit review triage against release/6.5.2",
            "Branch review against origin/next",
            "Slop Cop review against release/6.5.2",
            "CodeRabbit CLI review against origin/next",
            "Release 6.5.1 branch review",
            "Code style review of the diff",
        ] {
            assert_eq!(guess(t), Some("a reviewer"), "{t}");
        }
        for t in [
            "Session workspace setup",
            "Webiny admin local setup",
            "New entry dialog folder from reference field",
            "Node.js CVE customer advisory",
            "CodeRabbit on child umbrella PRs",
            "Router navigation skip for active folder",
            // "review" on its own is something you ask for too
            "Review the lease logic with me",
        ] {
            assert_eq!(guess(t), None, "{t}");
        }
    }

    /// What goes to disk comes back, and a file somebody has broken reads as
    /// an empty memory rather than hiding every row in the picker.
    #[test]
    fn the_list_survives_the_round_trip() {
        let list = vec![
            Spawned { id: "abc".into(), why: "footprint".into(), when: 100 },
            Spawned { id: "def".into(), why: String::new(), when: 200 },
        ];
        assert_eq!(from_json(&to_json(&list)), list);
        assert!(from_json("not json").is_empty());
        assert!(from_json(r#"[{"why":"no id"},{"id":""}]"#).is_empty());
    }
}
