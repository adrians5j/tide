//! What each workspace folder has been used for, over time.
//!
//! TIDE reuses a handful of checkouts: `wby-next1` through `wby-next12` are the
//! same repository, and which piece of work lives in which one changes every
//! few days. The title in the topbar says what a folder is for *now*. This file
//! remembers what it was for before, so finishing a task and resetting the
//! workspace doesn't throw away the only record of where that task happened.
//!
//! One record per stint: a folder picks up a title, carries it (and whatever
//! the agent writes under it as a subtitle) while the work runs, and the stint
//! closes when the workspace is reset. Nothing here needs a window, so the
//! whole model is tested without one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One stint of work in one folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stint {
    /// The project folder, absolute — the key everything else in TIDE is under.
    pub root: String,
    /// The workspace title, as it stood when the stint last changed.
    pub title: String,
    /// Where the work had got to, in the agent's words.
    pub subtitle: String,
    /// Who it was for, when it was client work.
    pub client: String,
    /// The branch it was on. The folder has moved on since; this hasn't, which
    /// is the point: it's how you find the work again.
    pub branch: String,
    pub started: u64,
    /// Unix seconds of the last change to any of the above. Not "last opened":
    /// the list is ordered by when the work moved, not by when you looked at it.
    pub touched: u64,
    /// When the workspace was reset, or 0 while the folder still holds this
    /// work.
    pub closed: u64,
}

impl Stint {
    /// The folder still holds this work — nobody has reset it.
    pub fn is_open(&self) -> bool {
        self.closed == 0
    }
}

/// Everything one write knows about a workspace. Every field is the caller's
/// current state rather than a change to apply, so an empty string means the
/// workspace has no subtitle, not that the caller didn't look.
#[derive(Default, Clone, Copy)]
pub struct Note<'a> {
    pub title: &'a str,
    pub subtitle: &'a str,
    pub client: &'a str,
    pub branch: &'a str,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A folder as a history key. `~/dev/xyz/` and `~/dev/xyz` are one folder, and
/// the title files TIDE has been keeping contain both spellings.
fn key(root: &str) -> String {
    match root.trim_end_matches('/') {
        "" => "/".to_string(),
        t => t.to_string(),
    }
}

fn path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/workspace-history.json"))
}

pub fn load() -> Vec<Stint> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|body| from_json(&body))
        .unwrap_or_default()
}

pub fn save(list: &[Stint]) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, to_json(list));
}

/// The history as the dialog shows it: the work that moved most recently first.
pub fn listed() -> Vec<Stint> {
    let mut list = load();
    list.sort_by(|a, b| b.touched.cmp(&a.touched).then_with(|| a.root.cmp(&b.root)));
    list
}

pub fn to_json(list: &[Stint]) -> String {
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|s| {
            serde_json::json!({
                "root": s.root,
                "title": s.title,
                "subtitle": s.subtitle,
                "client": s.client,
                "branch": s.branch,
                "started": s.started,
                "touched": s.touched,
                "closed": s.closed,
            })
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into())
}

pub fn from_json(body: &str) -> Vec<Stint> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(body) else { return Vec::new() };
    let text = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string()
    };
    let num = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    items
        .iter()
        .filter_map(|v| {
            let root = text(v, "root");
            let title = text(v, "title");
            // a stint with no folder or no name is nothing you could look for
            if root.trim().is_empty() || title.trim().is_empty() {
                return None;
            }
            Some(Stint {
                root: key(&root),
                title,
                subtitle: text(v, "subtitle"),
                client: text(v, "client"),
                branch: text(v, "branch"),
                started: num(v, "started"),
                touched: num(v, "touched"),
                closed: num(v, "closed"),
            })
        })
        .collect()
}

/// Record where a workspace stands. An empty title closes the folder's open
/// stint, because that is what resetting a workspace is: the work is over and
/// the folder is free for the next thing.
pub fn note(root: &Path, n: Note) {
    let mut list = load();
    if apply(&mut list, &root.to_string_lossy(), n, now()) {
        save(&list);
    }
}

/// The write, without the disk. `true` when something actually changed, so
/// repeating a note nobody has acted on doesn't rewrite the file.
pub fn apply(list: &mut Vec<Stint>, root: &str, n: Note, when: u64) -> bool {
    let root = key(root);
    let open = list.iter().position(|s| s.root == root && s.is_open());
    let (title, subtitle) = (n.title.trim(), n.subtitle.trim());
    let (client, branch) = (n.client.trim(), n.branch.trim());
    if title.is_empty() {
        let Some(i) = open else { return false };
        list[i].closed = when;
        list[i].touched = when;
        return true;
    }
    let Some(i) = open else {
        list.push(Stint {
            root,
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            client: client.to_string(),
            branch: branch.to_string(),
            started: when,
            touched: when,
            closed: 0,
        });
        return true;
    };
    let s = &mut list[i];
    if s.title == title && s.subtitle == subtitle && s.client == client && s.branch == branch {
        return false;
    }
    // a branch that has gone empty is git still reading, not a workspace that
    // left its branch: keep the one we have rather than losing it to a blank
    if !branch.is_empty() {
        s.branch = branch.to_string();
    }
    s.title = title.to_string();
    s.subtitle = subtitle.to_string();
    s.client = client.to_string();
    s.touched = when;
    true
}

/// Drop one row. The history is a convenience, not a record of account: a stint
/// you will never look for again is noise in the list.
pub fn forget(target: &Stint) {
    let mut list = load();
    let before = list.len();
    list.retain(|s| !(s.root == target.root && s.started == target.started));
    if list.len() != before {
        save(&list);
    }
}

/// Does this stint answer the words typed into the dialog? Every word has to
/// land somewhere, so "nuxt kit" finds the nuxt starter kit and "next8 leak"
/// finds the identity leak that was in `wby-next8`.
pub fn matches(s: &Stint, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let hay =
        format!("{} {} {} {} {}", s.title, s.subtitle, s.client, s.branch, s.root).to_lowercase();
    q.split_whitespace().all(|w| hay.contains(w))
}

/// A first history from what TIDE already knows: the title each folder carries
/// now, plus the one it carried before that, plus whatever subtitles are still
/// current. Neither of those files records a time, so `when` supplies one.
pub fn seed(
    titles: &HashMap<String, String>,
    last: &HashMap<String, String>,
    subs: &HashMap<String, String>,
    when: impl Fn(&str) -> u64,
) -> Vec<Stint> {
    let mut out: Vec<Stint> = Vec::new();
    for (root, title) in titles {
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        let t = when(root);
        out.push(Stint {
            root: key(root),
            title: title.to_string(),
            subtitle: subs.get(root).map(|s| s.trim().to_string()).unwrap_or_default(),
            started: t,
            touched: t,
            ..Default::default()
        });
    }
    for (root, title) in last {
        let title = title.trim();
        // the last title and the current one are usually the same folder's same
        // work: only a folder that has moved on since gets a second row
        if title.is_empty() || titles.get(root).map(|t| t.trim()) == Some(title) {
            continue;
        }
        // a second behind the folder's own time, which is all the seed knows:
        // work a folder has finished did come before the work it holds now, and
        // the list reads wrong if the two land on the same second
        let t = when(root).saturating_sub(1);
        out.push(Stint {
            root: key(root),
            title: title.to_string(),
            started: t,
            touched: t,
            closed: t,
            ..Default::default()
        });
    }
    // HashMaps come out in no order at all, so settle one here
    out.sort_by(|a, b| a.touched.cmp(&b.touched).then_with(|| a.root.cmp(&b.root)));
    out
}

/// Build the history once, out of the files that came before it, so the dialog
/// has something in it the first time it opens rather than on the first rename
/// after that.
pub fn ensure_seeded(
    titles: &HashMap<String, String>,
    last: &HashMap<String, String>,
    subs: &HashMap<String, String>,
) {
    let Some(p) = path() else { return };
    if p.exists() {
        return;
    }
    save(&seed(titles, last, subs, folder_time));
}

/// A folder's own modified time, as a stand-in for when the work in it stopped.
/// Only the seeded rows need it: everything written afterwards is timed as it
/// happens.
fn folder_time(root: &str) -> u64 {
    std::fs::metadata(root)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note<'a>(title: &'a str, subtitle: &'a str, branch: &'a str) -> Note<'a> {
        Note { title, subtitle, branch, client: "" }
    }

    /// The first title a folder gets opens a stint; later renames land on that
    /// same stint rather than starting a new one, because an agent refining the
    /// name it wrote is still the same piece of work.
    #[test]
    fn a_rename_stays_on_the_same_stint() {
        let mut list = Vec::new();
        assert!(apply(&mut list, "/dev/w1", note("AI chat", "", "main"), 100));
        assert!(apply(&mut list, "/dev/w1", note("✨ AI CHAT", "wiring the stream", "feat/ai"), 200));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "✨ AI CHAT");
        assert_eq!(list[0].subtitle, "wiring the stream");
        assert_eq!(list[0].branch, "feat/ai");
        assert_eq!((list[0].started, list[0].touched), (100, 200));
        // nothing moved, nothing written
        assert!(!apply(&mut list, "/dev/w1", note("✨ AI CHAT", "wiring the stream", "feat/ai"), 300));
        assert_eq!(list[0].touched, 200);
    }

    /// Resetting the workspace closes the stint, and the next piece of work in
    /// that folder is a new row — which is the whole reason this file exists.
    #[test]
    fn a_reset_closes_the_stint_and_the_next_one_is_new() {
        let mut list = Vec::new();
        apply(&mut list, "/dev/w8", note("identity leak", "one execution id", "fix/leak"), 100);
        assert!(apply(&mut list, "/dev/w8", note("", "", ""), 500));
        assert_eq!(list[0].closed, 500);
        assert!(!list[0].is_open());
        // a second reset has nothing left to close
        assert!(!apply(&mut list, "/dev/w8", note("", "", ""), 600));

        apply(&mut list, "/dev/w8", note("vue sdk parity", "", "feat/vue"), 700);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].started, 700);
        assert!(list[1].is_open());
        // the finished one still says where it was and what branch it was on
        assert_eq!((list[0].title.as_str(), list[0].branch.as_str()), ("identity leak", "fix/leak"));
    }

    /// Two folders working at once keep their own rows.
    #[test]
    fn folders_do_not_share_a_stint() {
        let mut list = Vec::new();
        apply(&mut list, "/dev/w1", note("one", "", ""), 100);
        apply(&mut list, "/dev/w2", note("two", "", ""), 100);
        apply(&mut list, "/dev/w1", note("one, renamed", "", ""), 200);
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].title, "two");
    }

    /// git reporting no branch for a moment doesn't wipe the branch already
    /// recorded, since a stint with no branch is a stint you can't go back to.
    #[test]
    fn a_blank_branch_does_not_erase_the_one_we_have() {
        let mut list = Vec::new();
        apply(&mut list, "/dev/w1", note("thing", "", "feat/thing"), 100);
        apply(&mut list, "/dev/w1", note("thing", "halfway", ""), 200);
        assert_eq!(list[0].branch, "feat/thing");
    }

    /// A trailing slash is the same folder: the title files TIDE kept before
    /// this one have `/dev/xyz/` in them, and a second row for it would be a
    /// lie about how many pieces of work that folder has held.
    #[test]
    fn a_trailing_slash_is_the_same_folder() {
        let mut list = Vec::new();
        apply(&mut list, "/dev/xyz/", note("mtrx", "", ""), 100);
        apply(&mut list, "/dev/xyz", note("mtrx, renamed", "", ""), 200);
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].root.as_str(), list[0].title.as_str()), ("/dev/xyz", "mtrx, renamed"));
    }

    /// Typing filters on every word, across the title, the subtitle and the
    /// folder, so you can find a stint by any part of what you remember.
    #[test]
    fn the_filter_takes_every_word() {
        let s = Stint {
            root: "/Users/me/dev/wby-next8".into(),
            title: "🐛 identity leak in resolvers".into(),
            subtitle: "one execution id".into(),
            branch: "fix/identity".into(),
            ..Default::default()
        };
        assert!(matches(&s, ""));
        assert!(matches(&s, "leak"));
        assert!(matches(&s, "next8"));
        assert!(matches(&s, "IDENTITY resolvers"));
        assert!(matches(&s, "execution"));
        assert!(!matches(&s, "leak nuxt"));
    }

    /// The first history is built out of the two title files: what each folder
    /// holds now is open, what it held before is closed, and a folder whose
    /// last title is its current one gets one row, not two.
    #[test]
    fn the_seed_reads_the_old_title_files() {
        let titles = HashMap::from([
            ("/dev/w1".to_string(), "✨ AI CHAT".to_string()),
            ("/dev/w8".to_string(), "🐛 identity leak".to_string()),
        ]);
        let last = HashMap::from([
            ("/dev/w1".to_string(), "✨ AI CHAT".to_string()),
            ("/dev/w8".to_string(), "🧹 an older job".to_string()),
            ("/dev/wcp".to_string(), "🧹 free port 3737".to_string()),
        ]);
        let subs = HashMap::from([("/dev/w8".to_string(), "one execution id".to_string())]);
        let got = seed(&titles, &last, &subs, |root| match root {
            "/dev/w1" => 300,
            "/dev/w8" => 200,
            _ => 100,
        });
        let rows: Vec<(&str, &str, bool)> =
            got.iter().map(|s| (s.root.as_str(), s.title.as_str(), s.is_open())).collect();
        assert_eq!(rows, vec![
            ("/dev/wcp", "🧹 free port 3737", false),
            ("/dev/w8", "🧹 an older job", false),
            ("/dev/w8", "🐛 identity leak", true),
            ("/dev/w1", "✨ AI CHAT", true),
        ]);
        // a folder's finished work sorts before the work it holds now
        assert!(got[1].touched < got[2].touched);
        // the subtitle rides along with the folder's current work
        assert_eq!(got[2].subtitle, "one execution id");
        // w1 holds the same work it last held: one row, not two
        assert_eq!(got.iter().filter(|s| s.root == "/dev/w1").count(), 1);
    }

    /// What goes to disk comes back, including the stints that are over.
    #[test]
    fn a_history_survives_the_round_trip() {
        let mut list = Vec::new();
        apply(&mut list, "/dev/w1", note("✨ AI CHAT", "the stream", "feat/ai"), 100);
        apply(&mut list, "/dev/w2", note("gone", "", ""), 200);
        apply(&mut list, "/dev/w2", note("", "", ""), 300);
        assert_eq!(from_json(&to_json(&list)), list);
        // rows with nothing to look for are dropped on the way back in
        assert!(from_json(r#"[{"root":"/dev/w1","title":""},{"title":"no folder"}]"#).is_empty());
        assert!(from_json("not json at all").is_empty());
    }
}
