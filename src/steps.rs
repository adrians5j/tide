//! What has been done in a workspace, in the order it happened.
//!
//! With five workspaces running at once, the question on coming back to one is
//! not what the branch looks like. The card already answers that. It is what you
//! last did here, and nothing in TIDE wrote that down. The git log is the
//! commits, which is a fraction of it: a merge, three reviewers, a brief, a code
//! tour and the question you asked about the DI container leave no trace
//! anywhere.
//!
//! So every one of those is written down as it happens, one line of JSON per
//! step, per workspace, in `~/.config/tide/steps/<slug>.jsonl`:
//!
//! ```json
//! {"at":1795512345,"what":"Sent 3 comments to the agent"}
//! ```
//!
//! This is a reader's record, not a measurement. `spend.jsonl` next door is the
//! exact one, and it only knows about turns: it has no row for a merge, a push
//! or a pull request, because those cost no tokens.

use std::path::{Path, PathBuf};

/// One thing that was done, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// Unix seconds.
    pub at: i64,
    /// How it reads in the list. A sentence, in the past tense, naming the
    /// thing: "Merged next in", "Asked for a brief of #6292".
    pub what: String,
}

/// How many steps a workspace keeps. Past this the oldest go. Six are shown on
/// the card and the rest is for scrolling back through a day, which this covers
/// several times over.
const KEEP: usize = 200;

/// When the file is rewritten to `KEEP`. Appending is cheap and rewriting is
/// not, so it happens once every hundred steps rather than on every one.
const TRIM_AT: usize = KEEP * 2;

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/steps"))
}

fn path(root: &Path) -> Option<PathBuf> {
    Some(dir()?.join(format!("{}.jsonl", crate::usage::project_slug(root))))
}

/// Write a step down. Silent on failure: a list of what you did is worth
/// nothing if it can take the editor down with it.
pub fn note(root: &Path, what: &str) {
    let what = what.trim();
    if what.is_empty() {
        return;
    }
    let Some(p) = path(root) else { return };
    let mut list = read(&p);
    // the same thing twice inside a minute is one thing that reported twice:
    // a push that refreshed the branch, a command run from two places
    if list.last().is_some_and(|s| s.what == what && now() - s.at < 60) {
        return;
    }
    let step = Step { at: now(), what: what.to_string() };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    // the common path is one append; the rewrite is the hundredth
    list.push(step.clone());
    if list.len() > TRIM_AT {
        let body: String = tail(&list, KEEP).iter().map(line).collect();
        let _ = std::fs::write(&p, body);
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line(&step).as_bytes());
    }
}

/// Every step this workspace remembers, oldest first.
pub fn load(root: &Path) -> Vec<Step> {
    path(root).map(|p| read(&p)).unwrap_or_default()
}

fn read(p: &Path) -> Vec<Step> {
    std::fs::read_to_string(p).map(|body| from_jsonl(&body)).unwrap_or_default()
}

fn line(s: &Step) -> String {
    format!("{}\n", serde_json::json!({ "at": s.at, "what": s.what }))
}

/// Parse the file. A line that isn't a step is skipped rather than fatal — the
/// file is appended to by a running editor and a half-written last line is a
/// normal thing to find.
pub fn from_jsonl(body: &str) -> Vec<Step> {
    body.lines()
        .filter_map(|l| {
            let v = serde_json::from_str::<serde_json::Value>(l).ok()?;
            let what = v.get("what")?.as_str()?.trim().to_string();
            if what.is_empty() {
                return None;
            }
            Some(Step { at: v.get("at").and_then(|a| a.as_i64()).unwrap_or(0), what })
        })
        .collect()
}

/// The last `n`, newest last. What the card draws.
pub fn tail(list: &[Step], n: usize) -> &[Step] {
    &list[list.len().saturating_sub(n)..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(at: i64, what: &str) -> Step {
        Step { at, what: what.into() }
    }

    /// What goes to the file comes back, and a line something broke is skipped
    /// instead of emptying the list after it.
    #[test]
    fn a_step_survives_the_round_trip() {
        let list = vec![step(100, "Merged next in"), step(140, "Asked for a brief of #6292")];
        let body: String = list.iter().map(line).collect();
        assert_eq!(from_jsonl(&body), list);
        // a torn last line, a blank, and junk: the steps before them stand
        let torn = format!("{body}{{\"at\":200,\"wh");
        assert_eq!(from_jsonl(&torn), list);
        assert!(from_jsonl("\n\nnot json\n{}\n").is_empty());
        // a step with no text is not a step
        assert!(from_jsonl("{\"at\":1,\"what\":\"  \"}").is_empty());
    }

    /// The card shows the end of the list, and asking for more than there is
    /// gives what there is.
    #[test]
    fn the_card_reads_the_newest_end() {
        let list: Vec<Step> = (0..10).map(|i| step(i, &format!("step {i}"))).collect();
        assert_eq!(tail(&list, 3), &list[7..]);
        assert_eq!(tail(&list, 40), &list[..]);
        assert!(tail(&[], 6).is_empty());
    }
}
