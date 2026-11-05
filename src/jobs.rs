//! How a prompt TIDE sent should read when the adapter replays it.
//!
//! Most of what TIDE puts to an agent is a page of instructions: the brief
//! request, a code tour, a merge, a question asked under a slide. The pane
//! never shows that. It shows a line — "Asked for a brief of #6176" — and folds
//! the working behind it, because the page was written for the agent and the
//! human has already read the slide it came from.
//!
//! That substitution lived only in memory. `session/load` replays what was
//! actually sent, so reopening TIDE turned every one of those lines back into
//! the page behind it. This file is the other half: the short form and the job
//! name are written down when the prompt goes out, under a fingerprint of the
//! prompt itself, and looked up again when it comes back.
//!
//! A miss costs nothing — the prompt reads as what it is, which is what the
//! pane did before any of this.

use std::path::PathBuf;

/// How one sent prompt reads in the pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    /// The one line that stands in for the prompt.
    pub label: String,
    /// The name on the fold, when the turn is a named job. Empty when the
    /// prompt only had a short form and nothing to fold.
    pub job: String,
    /// `true` when the prompt was TIDE's doing rather than something typed.
    pub by_tide: bool,
    /// Unix seconds, so the oldest entries are the ones dropped.
    pub when: u64,
}

/// FNV-1a over the prompt. A hash this file can rely on across Rust versions
/// and across releases, which `DefaultHasher` does not promise.
pub fn fingerprint(sent: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in sent.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{h:016x}")
}

/// How many prompts are remembered. Past this the oldest go: a session old
/// enough to have fallen off the end is one the adapter has usually forgotten
/// too.
const KEEP: usize = 2000;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/agent-labels.json"))
}

pub fn load() -> Vec<(String, Shown)> {
    path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|body| from_json(&body))
        .unwrap_or_default()
}

pub fn save(list: &[(String, Shown)]) {
    let Some(p) = path() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, to_json(list));
}

/// Write down how this prompt should read. `sent` is the prompt without TIDE's
/// briefing, which is the form the adapter replays.
pub fn remember(sent: &str, label: &str, job: &str, by_tide: bool) {
    // a prompt that reads as itself and folds into nothing has nothing to say
    if sent == label && job.is_empty() {
        return;
    }
    let mut list = load();
    put(&mut list, sent, Shown {
        label: label.to_string(),
        job: job.to_string(),
        by_tide,
        when: now(),
    });
    save(&list);
}

/// The write, without the disk.
pub fn put(list: &mut Vec<(String, Shown)>, sent: &str, shown: Shown) {
    let k = fingerprint(sent);
    match list.iter_mut().find(|(got, _)| *got == k) {
        Some((_, slot)) => *slot = shown,
        None => list.push((k, shown)),
    }
    if list.len() > KEEP {
        // oldest first, so the newest KEEP survive whatever order they arrived in
        list.sort_by_key(|(_, s)| s.when);
        list.drain(..list.len() - KEEP);
    }
}

/// How a replayed prompt should read, if TIDE remembers sending it — or, for
/// a session recorded before TIDE wrote any of this down, what its opening
/// line gives away.
pub fn recall(sent: &str) -> Option<Shown> {
    find(&load(), sent).or_else(|| guess(sent))
}

/// The four jobs a pull request's pane sends, read back off the prompt itself.
///
/// This is for history only. Every prompt from here on is written down as it
/// goes out, which is exact; this covers the transcripts that already exist,
/// where nothing was. The openings are matched against what the generators in
/// main.rs actually produce, and a test there feeds them through, so changing a
/// prompt breaks the test rather than quietly emptying this.
pub fn guess(sent: &str) -> Option<Shown> {
    let (label, job) = if sent.starts_with("Write me a brief of #") {
        let n = pr_number(sent, "Write me a brief of #")?;
        // Explain PR is the brief prompt with the tour prompt after it, so the
        // longer of the two has to be ruled out first
        match sent.contains(&format!("build the code tour of #{n} as well")) {
            true => (format!("Asked for a brief and a code tour of #{n}"), format!("Explain #{n}")),
            false => (format!("Asked for a brief of #{n}"), format!("Brief of #{n}")),
        }
    } else if let Some(n) = pr_number(sent, "Take me on a code tour of #") {
        // the same opening serves a tour of the conversation ("of what we have
        // been talking about"), which has no number and is not this job
        (format!("Asked for a code tour of #{n}"), format!("Code tour of #{n}"))
    } else if sent.starts_with("Is #") && sent.contains("'s description still true?") {
        let n = pr_number(sent, "Is #")?;
        (format!("Asked whether #{n}'s description is still true"), String::new())
    } else {
        return None;
    };
    Some(Shown { label, job, by_tide: true, when: 0 })
}

/// The pull request number straight after `prefix`.
fn pr_number(sent: &str, prefix: &str) -> Option<u64> {
    sent.strip_prefix(prefix)?.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

pub fn find(list: &[(String, Shown)], sent: &str) -> Option<Shown> {
    let k = fingerprint(sent);
    list.iter().find(|(got, _)| *got == k).map(|(_, s)| s.clone())
}

pub fn to_json(list: &[(String, Shown)]) -> String {
    let items: Vec<serde_json::Value> = list
        .iter()
        .map(|(k, s)| {
            serde_json::json!({
                "k": k,
                "label": s.label,
                "job": s.job,
                "tide": s.by_tide,
                "when": s.when,
            })
        })
        .collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".into())
}

pub fn from_json(body: &str) -> Vec<(String, Shown)> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(body) else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            let k = v.get("k")?.as_str()?.to_string();
            let label = v.get("label")?.as_str()?.to_string();
            if k.is_empty() || label.is_empty() {
                return None;
            }
            Some((k, Shown {
                label,
                job: v.get("job").and_then(|j| j.as_str()).unwrap_or_default().to_string(),
                by_tide: v.get("tide").and_then(|t| t.as_bool()).unwrap_or(true),
                when: v.get("when").and_then(|w| w.as_u64()).unwrap_or(0),
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(label: &str, job: &str, when: u64) -> Shown {
        Shown { label: label.into(), job: job.into(), by_tide: true, when }
    }

    /// The fingerprint is of the whole prompt, so two sends of the same job
    /// about different pull requests don't collide, and the same text always
    /// comes back to the same key.
    #[test]
    fn the_fingerprint_follows_the_prompt() {
        assert_eq!(fingerprint("Write me a brief of #6176"), fingerprint("Write me a brief of #6176"));
        assert_ne!(fingerprint("Write me a brief of #6176"), fingerprint("Write me a brief of #6177"));
        assert_ne!(fingerprint(""), fingerprint(" "));
        assert_eq!(fingerprint("").len(), 16);
    }

    /// A long prompt sent with a short form comes back as the short form, and
    /// the job name behind it comes back too.
    #[test]
    fn a_prompt_comes_back_as_its_short_form() {
        let mut list = Vec::new();
        let sent = "Write me a brief of #6176 — what it does and why, as slides…";
        put(&mut list, sent, shown("Asked for a brief of #6176", "Brief of #6176", 100));
        let got = find(&list, sent).unwrap();
        assert_eq!(got.label, "Asked for a brief of #6176");
        assert_eq!(got.job, "Brief of #6176");
        assert!(got.by_tide);
        // anything TIDE never sent reads as itself
        assert!(find(&list, "something I typed").is_none());
    }

    /// Sending the same prompt again replaces what was written down rather than
    /// filing a second answer for the same question.
    #[test]
    fn the_same_prompt_is_written_down_once() {
        let mut list = Vec::new();
        put(&mut list, "a prompt", shown("first", "Job", 100));
        put(&mut list, "a prompt", shown("second", "Job", 200));
        assert_eq!(list.len(), 1);
        assert_eq!(find(&list, "a prompt").unwrap().label, "second");
    }

    /// The file is capped, and it is the oldest entries that go.
    #[test]
    fn the_oldest_entries_are_the_ones_dropped() {
        let mut list = Vec::new();
        for i in 0..KEEP + 10 {
            put(&mut list, &format!("prompt {i}"), shown(&format!("label {i}"), "", i as u64));
        }
        assert_eq!(list.len(), KEEP);
        assert!(find(&list, "prompt 0").is_none());
        assert!(find(&list, "prompt 9").is_none());
        assert_eq!(find(&list, "prompt 10").unwrap().label, "label 10");
        assert_eq!(find(&list, &format!("prompt {}", KEEP + 9)).unwrap().label, format!("label {}", KEEP + 9));
    }

    /// A transcript recorded before any of this was written down still reads
    /// as lines rather than pages, off the prompt's opening alone.
    #[test]
    fn an_old_prompt_is_read_off_its_opening() {
        let brief = guess("Write me a brief of #6176 — what it does and why, as slides…").unwrap();
        assert_eq!((brief.label.as_str(), brief.job.as_str()), (
            "Asked for a brief of #6176",
            "Brief of #6176",
        ));
        // the same opening, with the tour half after it, is Explain PR
        let explain = guess(
            "Write me a brief of #6176 — what it does and why…\n---\n\nin the same turn, \
             build the code tour of #6176 as well.\n",
        )
        .unwrap();
        assert_eq!(explain.job, "Explain #6176");
        let tour =
            guess("Take me on a code tour of #6176 — what it changes…").unwrap();
        assert_eq!(tour.job, "Code tour of #6176");
        let body = guess("Is #6176's description still true?\n\nRead it…").unwrap();
        assert_eq!(body.label, "Asked whether #6176's description is still true");
        // a job with nothing to fold still gets its one line
        assert!(body.job.is_empty());
        // and anything a human typed is left exactly as they typed it
        assert!(guess("can you look at the lease logic again").is_none());
        // a tour of the conversation rather than of a pull request isn't this
        assert!(guess("Take me on a code tour of what we have been talking about.").is_none());
    }

    /// What goes to disk comes back, and a file somebody has broken reads as an
    /// empty memory rather than taking the pane down with it.
    #[test]
    fn a_memory_survives_the_round_trip() {
        let mut list = Vec::new();
        put(&mut list, "one", shown("Asked for a tour of #1", "Code tour of #1", 100));
        put(&mut list, "two", Shown {
            label: "why does the lease expire?".into(),
            job: "Question on slide 3".into(),
            by_tide: false,
            when: 200,
        });
        assert_eq!(from_json(&to_json(&list)), list);
        assert!(from_json("not json").is_empty());
        assert!(from_json(r#"[{"k":"abc"},{"label":"no key"}]"#).is_empty());
    }
}
