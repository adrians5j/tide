//! Narration for a brief: the slides read out loud.
//!
//! macOS ships a speech synthesiser and an audio player on the path, so this
//! shells out to them rather than linking an audio stack — `say` writes a
//! clip, `afplay` plays it. One clip per sentence, which buys three things: a
//! pause lands on a sentence instead of restarting the slide, the line being
//! spoken can be lit on screen, and the next clip is already on disk by the
//! time the current one ends.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Words a minute. `say` defaults to around 175; a presentation is read a
/// little slower than a notification is.
pub const RATE: u32 = 170;

/// The voices worth reading a brief in. macOS installs a few dozen, most of
/// them novelties ("Bubbles", "Zarvox") or the compact versions of voices whose
/// good form is a separate download, so a blind "first en_US" would often pick
/// something unlistenable.
const GOOD: [&str; 16] = [
    "Ava", "Evan", "Zoe", "Allison", "Samantha", "Susan", "Tom", "Nicky", "Aaron", "Joelle",
    "Noelle", "Serena", "Daniel", "Karen", "Tessa", "Alex",
];

/// The best voice installed on this machine. The good ones are a download
/// rather than a default — System Settings, Spoken Content, Manage Voices —
/// so this prefers a Premium or Enhanced voice when one is there and settles
/// for whatever is.
pub fn best_voice() -> String {
    let out = Command::new("say").arg("-v").arg("?").output().ok();
    let listing = out
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let mut best: Option<(i32, String)> = None;
    for line in listing.lines() {
        // "Ava (Premium)        en_US    # Hello! My name is Ava."
        let Some((name, rest)) = line.split_once("  ") else { continue };
        let name = name.trim();
        if name.is_empty() || !rest.contains("en_") {
            continue;
        }
        let stem = name.split('(').next().unwrap_or(name).trim();
        let Some(rank) = GOOD.iter().position(|g| *g == stem) else { continue };
        let quality = if name.contains("(Premium)") {
            200
        } else if name.contains("(Enhanced)") {
            100
        } else {
            0
        };
        // US English first among equals: a brief is written in it
        let region = i32::from(rest.contains("en_US")) * 20;
        let score = quality + region - rank as i32;
        if best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, name.to_string()));
        }
    }
    best.map(|(_, n)| n).unwrap_or_default()
}

/// Narration as the clips it will be spoken as. Sentences, except that a
/// fragment too short to stand on its own joins the one after it: "Right." is
/// a clip whose startup costs more than its audio.
pub fn lines_of(text: &str) -> Vec<String> {
    let flat = spoken(text);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = flat.chars().peekable();
    while let Some(c) = chars.next() {
        cur.push(c);
        let ends = matches!(c, '.' | '!' | '?' | ':');
        // "3.5" and "e.g." are not the end of anything
        let next_is_space = chars.peek().is_none_or(|n| n.is_whitespace());
        if !ends || !next_is_space {
            continue;
        }
        let done = cur.trim().to_string();
        if done.chars().count() < 24 {
            cur.push(' '); // too short to be a clip of its own: keep going
            continue;
        }
        out.push(done);
        cur.clear();
    }
    let tail = cur.trim().to_string();
    if !tail.is_empty() {
        match out.last_mut() {
            // a trailing fragment belongs to the sentence before it
            Some(last) if tail.chars().count() < 24 => {
                last.push(' ');
                last.push_str(&tail);
            }
            _ => out.push(tail),
        }
    }
    out
}

/// Prose as a synthesiser should receive it: no markdown, no square brackets
/// (`say` reads `[[slnc 500]]` as a command, not as words), and paths cut back
/// to the file name, since nobody wants to hear a directory spelled out.
pub fn spoken(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        let w = word.trim_matches(|c: char| c == '`' || c == '*' || c == '_');
        // packages/api-tasks/src/recover.ts:17 → recover.ts
        let w = match w.rsplit('/').next() {
            Some(tail) if w.contains('/') && !w.contains(' ') => tail,
            _ => w,
        };
        let w = w.split(':').next().unwrap_or(w);
        out.push_str(w);
        word.clear();
    };
    for c in text.chars() {
        match c {
            '[' | ']' | '#' | '>' | '|' => {}
            c if c.is_whitespace() => {
                flush(&mut word, &mut out);
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
            }
            c => word.push(c),
        }
    }
    flush(&mut word, &mut out);
    out.trim().to_string()
}

/// Where a brief's clips live. Keyed by the commit it was written against and
/// by the voice, so a rewritten brief or a changed voice gets its own set and
/// a second viewing of the same one is instant.
pub fn clip_dir(pr: u64, oid: &str, voice: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let tag: String = oid.chars().filter(|c| c.is_ascii_alphanumeric()).take(10).collect();
    let v: String = voice.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect();
    let v = if v.is_empty() { "default".to_string() } else { v };
    let tag = if tag.is_empty() { "head".to_string() } else { tag };
    Some(PathBuf::from(home).join(format!(".config/tide/narration/{pr}-{tag}-{v}")))
}

/// The clip for one line of one slide.
pub fn clip_path(dir: &Path, slide: usize, line: usize) -> PathBuf {
    dir.join(format!("{slide:02}-{line:02}.aiff"))
}

/// Write one clip. Through a temporary name, so a clip that exists is a clip
/// that finished: the player treats the file being there as "ready to play".
fn synth(voice: &str, text: &str, out: &Path) -> bool {
    if out.exists() {
        return true;
    }
    let part = out.with_extension("part");
    let mut cmd = Command::new("say");
    if !voice.is_empty() {
        cmd.arg("-v").arg(voice);
    }
    let ok = cmd
        .arg("-r")
        .arg(RATE.to_string())
        .arg("-o")
        .arg(&part)
        .arg(text)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(&part);
        return false;
    }
    std::fs::rename(&part, out).is_ok()
}

/// How many clips are synthesised at once. `say` spends most of a short clip
/// starting up, so a few in parallel is most of the win; more than that only
/// competes with the clip being played.
const WORKERS: usize = 3;

/// Synthesise the whole deck in the background, in the order it will be heard,
/// and stop when `stop` is raised. Each job is a clip's path and what to say.
pub fn spawn_synth(voice: String, jobs: Vec<(PathBuf, String)>, stop: Arc<AtomicBool>) {
    if jobs.is_empty() {
        return;
    }
    if let Some(dir) = jobs[0].0.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let jobs = Arc::new(jobs);
    let next = Arc::new(AtomicUsize::new(0));
    for _ in 0..WORKERS.min(jobs.len()) {
        let (jobs, next, stop, voice) = (jobs.clone(), next.clone(), stop.clone(), voice.clone());
        std::thread::spawn(move || {
            loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= jobs.len() || stop.load(Ordering::Relaxed) {
                    return;
                }
                let (path, text) = &jobs[i];
                synth(&voice, text, path);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Narration is cut into clips at sentence ends, and a fragment too short
    /// to be worth a clip rides along with its neighbour.
    #[test]
    fn narration_is_cut_into_sentences() {
        let got = lines_of(
            "The worker used to stall for ever. Now a stale lease is treated as a failure. Good.",
        );
        assert_eq!(
            got,
            vec![
                "The worker used to stall for ever.",
                "Now a stale lease is treated as a failure. Good.",
            ]
        );
        // one sentence is one clip, and an unterminated tail still counts
        assert_eq!(lines_of("Just the one thing worth saying here"), vec![
            "Just the one thing worth saying here"
        ]);
        assert!(lines_of("   ").is_empty());
    }

    /// A decimal point is not the end of a sentence, because the character
    /// after it isn't a space.
    #[test]
    fn a_number_does_not_end_a_sentence() {
        let got = lines_of("The lease is 60.5 seconds long, which is the number that matters.");
        assert_eq!(got.len(), 1, "{got:?}");
    }

    /// What goes to the synthesiser is prose, not markup: no backticks, no
    /// brackets for it to read as a command, and a path said as its file name.
    #[test]
    fn markup_and_paths_are_said_as_words() {
        assert_eq!(spoken("the `recover()` loop"), "the recover() loop");
        assert_eq!(
            spoken("it lives in packages/api-tasks/src/recover.ts:17 now"),
            "it lives in recover.ts now"
        );
        assert_eq!(spoken("**bold** and _thin_"), "bold and thin");
        assert_eq!(spoken("a [[slnc 500]] command"), "a slnc 500 command");
    }

    /// Clips are kept per pull request, per commit and per voice, so a brief
    /// written again doesn't play the old narration.
    #[test]
    fn clips_are_filed_by_commit_and_voice() {
        let Some(a) = clip_dir(6164, "80c32ef7b1", "Ava (Premium)") else { return };
        let Some(b) = clip_dir(6164, "ff0011aa22", "Ava (Premium)") else { return };
        let Some(c) = clip_dir(6164, "80c32ef7b1", "Evan (Enhanced)") else { return };
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.to_string_lossy().contains("6164-80c32ef7b1-AvaPremium"));
        assert_eq!(clip_path(&a, 3, 1).file_name().unwrap(), "03-01.aiff");
    }
}
