use gpui::{
    AnyElement, AnyView, AnyWindowHandle, App, Bounds, ClipboardItem, Context, CursorStyle, Div, Entity, EntityId,
    ElementInputHandler, EntityInputHandler, EventEmitter, ExternalPaths, FocusHandle, KeyBinding, WeakEntity, relative,
    KeyDownEvent, Keystroke, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ScrollStrategy, SharedString,
    StyledText, TextRun, Window, WindowBounds, WindowOptions, actions, div, font, prelude::*, px,
    rgb, rgba, size, uniform_list, ScrollHandle, UniformListScrollHandle,
};
use gpui_platform::application;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod theme;
use theme::*;
mod field;
use field::{Edit, Field};
mod syntax;
mod editor;
mod term;
mod acp;
mod browser;
mod diff;
mod build;
mod lsp;
mod md;
mod rpc;
mod usage;
use rpc::AgentState;
use diff::{DiffKind, DiffRow};
use syntax::{Highlighter, Run};
use lsp::Lsp;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use term::Terminal;
use editor::{
    Backspace, CompDismiss, CompTrigger, Copy, Cut, Delete, DeleteLine, DeleteWordLeft,
    DeleteWordRight, DupLine, Editor, EnableEdit, End, GotoDef, Home, Indent, MoveDown, MoveLeft, MoveLineDown,
    MoveLineUp, MoveRight, MoveUp, Newline, OpenLocation, Paste, Redo, Save, SearchOpen, SelectAll,
    SelectDown, SelectEnd, SelectHome, SelectLeft, SelectRight, SelectUp, SelectWordLeft,
    SelectWordRight, ToggleComment, ToggleReadOnly, Undo, WordLeft, WordRight,
};
use editor::CommentRequest;

// Codicon (VS Code icon font) glyphs — rendered with font_family("codicon").
const IC_COMMIT: &str = "\u{eafc}";
const IC_PUSH: &str = "\u{eb41}";
const IC_PR: &str = "\u{ea64}";
const IC_HOME: &str = "\u{eb06}";
const IC_TOOLS: &str = "\u{eb6d}";
const IC_ADD: &str = "\u{ea60}";
const IC_BRANCH: &str = "\u{ec6f}";
const IC_SEARCH: &str = "\u{ea6d}";
const IC_FILES: &str = "\u{eaf0}";
const IC_SCM: &str = "\u{ea68}";
const IC_TERMINAL: &str = "\u{ea85}";
const IC_RUN: &str = "\u{eb2c}"; // play
const IC_FOLDER: &str = "\u{ea83}";
const IC_COPY: &str = "\u{ebcc}";
const IC_CHECK: &str = "\u{eab2}";
const IC_TRASH: &str = "\u{ea81}";
const IC_BROWSER: &str = "\u{eb01}"; // codicon "globe"
const IC_COMMENT: &str = "\u{ea6b}"; // codicon "comment"
const IC_AGENT: &str = "\u{ec4f}"; // codicon "chat-sparkle" — the agent pane
const IC_GRAPH: &str = "\u{ebe2}"; // codicon "graph-line" — plan usage
const IC_GEAR: &str = "\u{eb51}"; // codicon "gear" — the agent's own settings
// The codicon font is renamed to "Segoe Fluent Icons" because GPUI refuses to
// load any font lacking an 'm' glyph — except that one specially-cased name.
const ICON_FONT: &str = "Segoe Fluent Icons";

// Native ACP agent panel: the right-side dock + ⌘⇧A. Talks to
// @agentclientprotocol/claude-agent-acp over stdio; see src/acp.rs.
const AGENT_PANEL: bool = true;

// In-pane browser (embedded WKWebView) — parked (too hiccuppy). Flip to `true`
// to bring back the Browser icon + right-dock. See src/browser.rs.
const BROWSER_PANEL: bool = false;

actions!(
    workspace,
    [
        CloseTab, CloseOthers, ToggleTerminal, OpenFinder, GotoLine, NewTerminal, CloseTerminalTab,
        CloseOtherTerminals, SplitTermRight, SplitTermDown, ToggleTermZoom, FocusPaneLeft, FocusPaneRight,
        FocusPaneUp, FocusPaneDown, GotoCommit, ShowDiff, FindInFiles,
        GitPopup, CommandPalette, CopyReference, CopyReferenceLine, OpenOnGithub, NextProject, PrevProject, OpenProject,
        ShowProjects, PushDialog, RunCommand, NewProject, FetchRemotes, PullRemote, WipPush, RunBuild,
        NewScratch, ToggleAgent, GitLog, SetWsTitle, RefreshPr, AddComment, ShowComments, SendComments,
        QuitApp, ToggleMdPreview, QuoteAgentSel
    ]
);

/// One entry in the agent conversation transcript.
enum AgentMsg {
    /// Something the user typed, and whatever was attached to it. The images
    /// outlive the composer: `[Image #1]` in the prose says where a picture
    /// went, and the chips under the bubble say what it was.
    User { text: String, imgs: Vec<SentImage> },
    /// Streamed assistant text (mutated in place as chunks arrive).
    Agent(String),
    /// Streamed assistant reasoning ("thinking"), shown dimmed.
    Thought(String),
    /// A tool call the agent ran: title (for Bash, the command itself), latest
    /// status, what the adapter said about it (a description before it runs, the
    /// output after), and when it started, so a long one can show its age.
    Tool {
        id: String,
        title: String,
        status: String,
        detail: String,
        started: std::time::Instant,
    },
}

/// The live `@` mention in front of the caret: where its `@` sits and what has
/// been typed after it. A mention starts a word (never an e-mail's `a@b`) and
/// ends at the first space, so `@wby` is live and `@wby next` is plain text
/// again.
fn mention_span(text: &str, caret: usize) -> Option<(usize, String)> {
    let head = text.get(..caret)?;
    let i = head.rfind('@')?;
    let starts_word = head[..i].chars().next_back().map_or(true, |c| c.is_whitespace() || c == '(');
    if !starts_word || head[i + 1..].contains(char::is_whitespace) {
        return None;
    }
    Some((i, head[i + 1..].to_string()))
}

/// The adapter titles a session from its first prompt — which, for the first
/// prompt of a session, carries TIDE's hidden briefing. That makes a title out
/// of the briefing's opening line, so it's dropped rather than shown. Anything
/// else is kept to its first line, clipped to a header's worth of words.
fn session_title(raw: &str) -> Option<String> {
    let line = raw.lines().find(|l| !l.trim().is_empty())?.trim();
    // the briefing's two possible openings (the house-rules line comes first
    // when the repo has an AGENTS.md / CLAUDE.md)
    if line.starts_with("You are running inside TIDE")
        || line.starts_with("Read AGENTS.md")
        || line.starts_with("Read CLAUDE.md")
    {
        return None;
    }
    let mut out: String = line.chars().take(70).collect();
    if line.chars().count() > 70 {
        out.push('…');
    }
    (!out.is_empty()).then_some(out)
}

/// One `@` completion candidate in the agent composer.
#[derive(Clone)]
struct Mention {
    /// What lands in the text, `@` included.
    token: String,
    /// Dimmed, right of the token: a workspace's title, a file's folder.
    detail: String,
    /// What the agent gets in place of the token. `None` for a candidate that
    /// already says everything (a file path).
    expand: Option<String>,
}

#[derive(Clone)]
struct FindResult {
    path: PathBuf,
    line: usize,
    text: String,
}

/// Cached, syntax-highlighted contents of the file shown in the find preview.
struct FindPreview {
    path: PathBuf,
    lines: Vec<String>,
    styles: Vec<Vec<syntax::Run>>,
}

/// Shared syntax highlighter for the find preview (loaded once).
fn highlighter() -> &'static syntax::Highlighter {
    use std::sync::OnceLock;
    static H: OnceLock<syntax::Highlighter> = OnceLock::new();
    H.get_or_init(syntax::Highlighter::new)
}

const FIND_CAP: usize = 500;
/// Height above the find dialog's results: title 24 + search 42 + scope 22 +
/// the syntax legend 20.
const FIND_HEAD_H: f32 = 108.0;

/// Which edges a find-dialog resize drag is moving (for window-style resizing).
#[derive(Clone, Copy, Default)]
struct ResizeEdges {
    l: bool,
    r: bool,
    t: bool,
    b: bool,
}

/// Full-text search across files under `scope`. Uses ripgrep when available
/// (fast, parallel, respects .gitignore); falls back to a pure-Rust walk.
/// Split a search query into (include, excludes). A `-word` token (dash + at
/// least one non-space char) is an exclusion; everything else is the literal
/// search term. E.g. `context. -context.container` → ("context.", ["context.container"]).
/// Split the query box into the literal search term, `-exclude` terms, and file
/// globs. A token containing `*` or `?` is a filename pattern (`*.ts`, `src/**`)
/// rather than search text; `-*.ts` negates a glob, `-foo` excludes a substring.
fn parse_search_query(q: &str) -> SearchQuery {
    let is_glob = |s: &str| s.contains('*') || s.contains('?');
    // A bare directory name (`__tests__`, `dist`) only excludes anything if it
    // covers the whole subtree, so grow it into `**/name/**` — typing the full
    // form by hand is the difference between "no matches removed" and working.
    let as_path_glob = |s: &str| {
        if s.contains('/') || s.contains('.') {
            s.to_string()
        } else {
            format!("**/{s}/**")
        }
    };
    // `api-core*` names a *directory* far more often than a file (files carry
    // an extension), and a `--glob` only matches the entry itself — so a
    // pattern whose last segment has no dot also gets its subtree form. Both go
    // to ripgrep, which ORs its positive globs, so a file by that name still
    // matches.
    let subtree_form = |g: &str| -> Option<String> {
        let g = g.trim_end_matches('/');
        let last = g.rsplit('/').next().unwrap_or(g);
        if last.contains('.') || g.ends_with("/**") {
            return None;
        }
        // a pattern with no slash names an entry anywhere, so its subtree form
        // needs the `**/` lead that a rooted pattern already has
        Some(if g.contains('/') { format!("{g}/**") } else { format!("**/{g}/**") })
    };
    let mut include = Vec::new();
    let mut excludes = Vec::new();
    let mut globs = Vec::new();
    for tok in q.split_whitespace() {
        // `-x` and `!x` both mean "exclude x" — `!` is what most search UIs use
        if tok.len() > 1 && (tok.starts_with('-') || tok.starts_with('!')) {
            let inner = &tok[1..];
            if is_glob(inner) {
                if let Some(sub) = subtree_form(inner) {
                    globs.push(format!("!{sub}"));
                }
                globs.push(format!("!{inner}"));
            } else if inner.contains('/') || inner.contains('.') {
                // looks like text or an explicit path (`-context.container`):
                // drop matching *lines*, and the path glob if it happens to hit
                globs.push(format!("!{inner}"));
                excludes.push(inner.to_string());
            } else {
                // a bare word is a directory name (`!__tests__`, `!dist`): skip
                // that subtree, but keep lines elsewhere that merely mention it
                globs.push(format!("!{}", as_path_glob(inner)));
            }
        } else if is_glob(tok) {
            globs.push(tok.to_string());
            if let Some(sub) = subtree_form(tok) {
                globs.push(sub);
            }
        } else {
            include.push(tok);
        }
    }
    SearchQuery { include: include.join(" "), excludes, globs }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn numbers_sort_numerically_not_lexically() {
        let mut v: Vec<PathBuf> = ["wby-next10", "wby-next2", "wby-next1", "wby-next9"]
            .iter()
            .map(PathBuf::from)
            .collect();
        v.sort_by_key(|p| natural_key(p));
        let names: Vec<String> = v.iter().map(|p| p.to_string_lossy().to_string()).collect();
        assert_eq!(names, ["wby-next1", "wby-next2", "wby-next9", "wby-next10"]);
    }

    #[test]
    fn a_glob_arg_expands_to_matching_dirs_in_natural_order() {
        let tmp = std::env::temp_dir().join(format!("tide-cli-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        for n in [1, 2, 10] {
            std::fs::create_dir_all(tmp.join(format!("wby-next{n}"))).unwrap();
        }
        std::fs::create_dir_all(tmp.join("other")).unwrap();
        std::fs::write(tmp.join("wby-next-file"), b"x").unwrap(); // a file must not match

        let hits = resolve_arg(&format!("{}/wby-next*", tmp.to_string_lossy()));
        let names: Vec<String> = hits
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, ["wby-next1", "wby-next2", "wby-next10"]);

        // a plain path still resolves to itself, and a non-directory is dropped
        assert_eq!(resolve_arg(&tmp.join("other").to_string_lossy()).len(), 1);
        assert!(resolve_arg(&tmp.join("wby-next-file").to_string_lossy()).is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod search_query_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bare_exclusions_grow_into_subtree_globs() {
        for q in ["RequestInitializer *.ts !__tests__", "RequestInitializer *.ts -__tests__"] {
            let p = parse_search_query(q);
            assert_eq!(p.include, "RequestInitializer");
            assert!(p.globs.contains(&"*.ts".to_string()), "{q}: {:?}", p.globs);
            assert!(p.globs.contains(&"!**/__tests__/**".to_string()), "{q}: {:?}", p.globs);
        }
    }

    #[test]
    fn a_test_dir_file_is_excluded_but_a_source_file_is_not() {
        let globs = parse_search_query("RequestInitializer *.ts !__tests__").globs;
        let in_tests = PathBuf::from("packages/event-handler-aws/__tests__/requestInitializer.test.ts");
        let in_src = PathBuf::from("packages/event-handler-core/src/features/events/createHandler.ts");
        assert!(!file_matches_globs(&in_tests, &globs));
        assert!(file_matches_globs(&in_src, &globs));
    }

    /// `api-core*` is a folder name, and a glob only ever matches the entry it
    /// names — so the subtree form goes along with it, or the search finds
    /// nothing at all (a folder has no lines to match).
    #[test]
    fn a_folder_pattern_covers_what_is_inside_it() {
        let globs = parse_search_query("expiresAt **/api-core*").globs;
        assert!(globs.contains(&"**/api-core*".to_string()), "{globs:?}");
        assert!(globs.contains(&"**/api-core*/**".to_string()), "{globs:?}");
        let inside = PathBuf::from("packages/api-core-sql/src/expiry.ts");
        let elsewhere = PathBuf::from("packages/api-file-manager/src/expiry.ts");
        assert!(file_matches_globs(&inside, &globs));
        assert!(!file_matches_globs(&elsewhere, &globs));
        // a bare folder pattern works the same way
        let globs = parse_search_query("expiresAt api-core*").globs;
        assert!(file_matches_globs(&inside, &globs), "{globs:?}");
        // an extension is a file pattern and stays one
        let globs = parse_search_query("expiresAt *.ts").globs;
        assert_eq!(globs, vec!["*.ts".to_string()]);
    }

    /// The negative side needs the same treatment: `-api-core*` has to prune the
    /// subtree, not just a file that happens to be named that.
    #[test]
    fn an_excluded_folder_pattern_prunes_the_subtree() {
        let globs = parse_search_query("expiresAt -api-core*").globs;
        let inside = PathBuf::from("packages/api-core-sql/src/expiry.ts");
        let elsewhere = PathBuf::from("packages/api-file-manager/src/expiry.ts");
        assert!(!file_matches_globs(&inside, &globs), "{globs:?}");
        assert!(file_matches_globs(&elsewhere, &globs), "{globs:?}");
    }

    /// Exclusions stack, and a hyphenated package name is still one bare token
    /// (the leading `-`/`!` is the only thing treated as a sign).
    #[test]
    fn several_bare_exclusions_stack() {
        let globs = parse_search_query("RequestInitializer *.ts !__tests__ !api-event-handler-core").globs;
        assert!(globs.contains(&"!**/__tests__/**".to_string()), "{globs:?}");
        assert!(globs.contains(&"!**/api-event-handler-core/**".to_string()), "{globs:?}");
        let excluded = PathBuf::from("packages/api-event-handler-core/src/features/events/createHandler.ts");
        let kept = PathBuf::from("packages/api-file-manager-s3/src/FileManagerS3Feature.ts");
        assert!(!file_matches_globs(&excluded, &globs));
        assert!(file_matches_globs(&kept, &globs));
    }

    /// A bare `!folder` prunes that subtree only. A line elsewhere that happens
    /// to mention the folder name is still a legitimate match — excluding it
    /// silently dropped results that looked like they should be there.
    #[test]
    fn a_bare_folder_exclusion_does_not_filter_line_text() {
        let p = parse_search_query("RequestInitializer !event-handler-core");
        assert!(p.globs.contains(&"!**/event-handler-core/**".to_string()), "{:?}", p.globs);
        assert!(p.excludes.is_empty(), "{:?}", p.excludes);
        // a dotted token is text, not a directory: that one still filters lines
        let q = parse_search_query("context. -context.container");
        assert_eq!(q.excludes, vec!["context.container".to_string()]);
    }

    #[test]
    fn explicit_path_globs_are_left_alone() {
        let p = parse_search_query("foo -**/dist/** -*.test.ts");
        assert!(p.globs.contains(&"!**/dist/**".to_string()), "{:?}", p.globs);
        assert!(p.globs.contains(&"!*.test.ts".to_string()), "{:?}", p.globs);
    }
}

/// Parsed pieces of the find-in-files query box.
struct SearchQuery {
    include: String,
    excludes: Vec<String>,
    globs: Vec<String>,
}

/// Match a `*`/`?` glob against `text` (used by the pure-Rust search fallback;
/// ripgrep handles globbing natively). Patterns without `/` match the file name;
/// otherwise the whole relative path.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    // classic two-pointer wildcard match with backtracking on the last '*'
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Keep a result line unless every occurrence of `include` in it is the start
/// of some `exclude` term (so `context.` matches but `context.container` doesn't).
/// Honors the case-sensitivity toggle so excludes match the search.
fn line_passes_excludes(text: &str, include: &str, excludes: &[String], case_sensitive: bool) -> bool {
    if excludes.is_empty() {
        return true;
    }
    let fold = |s: &str| if case_sensitive { s.to_string() } else { s.to_lowercase() };
    let t = fold(text);
    let inc = fold(include);
    let exs: Vec<String> = excludes.iter().map(|e| fold(e)).collect();
    let mut found = false;
    let mut start = 0;
    while let Some(rel) = t[start..].find(&inc) {
        let i = start + rel;
        found = true;
        if !exs.iter().any(|e| t[i..].starts_with(e.as_str())) {
            return true; // a non-excluded occurrence → keep the line
        }
        start = i + inc.len().max(1);
    }
    // no visible occurrence (e.g. truncated) → can't judge, keep; else all excluded → drop
    !found
}

fn search_files(query: &str, scopes: &[PathBuf], case_sensitive: bool) -> Vec<FindResult> {
    let SearchQuery { include, excludes, globs } = parse_search_query(query);
    if include.len() < 2 || scopes.is_empty() {
        return Vec::new();
    }
    let mut results = search_ripgrep(&include, scopes, case_sensitive, &globs)
        .unwrap_or_else(|| search_rust(&include, scopes, case_sensitive, &globs));
    if !excludes.is_empty() {
        results.retain(|r| line_passes_excludes(&r.text, &include, &excludes, case_sensitive));
    }
    results
}

/// Locate the ripgrep binary once (PATH may be minimal under a GUI launch).
fn rg_bin() -> Option<&'static str> {
    use std::sync::OnceLock;
    static BIN: OnceLock<Option<&'static str>> = OnceLock::new();
    *BIN.get_or_init(|| {
        for c in ["rg", "/opt/homebrew/bin/rg", "/usr/local/bin/rg"] {
            if Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
                return Some(c);
            }
        }
        None
    })
}

/// Run `rg` and parse `path:line:text`. Returns None if rg isn't available.
fn search_ripgrep(query: &str, scopes: &[PathBuf], case_sensitive: bool, globs: &[String]) -> Option<Vec<FindResult>> {
    let bin = rg_bin()?;
    let mut cmd = Command::new(bin);
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        // truly case-insensitive unless the "match case" toggle is on (smart-case
        // would flip to case-sensitive whenever the query has any uppercase)
        .arg(if case_sensitive { "--case-sensitive" } else { "--ignore-case" })
        .arg("--max-count=50") // per-file cap
        .arg("--max-filesize=1M") // skip huge committed bundles (minified .cjs, etc.)
        .arg("--fixed-strings"); // literal, not regex
    for g in globs {
        cmd.arg("--glob").arg(g);
    }
    cmd.arg("--").arg(query);
    for s in scopes {
        cmd.arg(s);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        // rg exits 1 when no matches — that's a valid empty result, but a
        // missing binary errors differently; treat spawn failure as None above.
        return Some(Vec::new());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut results = Vec::new();
    for line in text.lines() {
        if results.len() >= FIND_CAP {
            break;
        }
        // path:line:content  (path may contain ':' on Windows but not here)
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(lno), Some(content)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(line_no) = lno.parse::<usize>() else { continue };
        results.push(FindResult {
            path: PathBuf::from(path),
            line: line_no,
            text: content.trim_start().chars().take(200).collect(),
        });
    }
    Some(results)
}

/// Pure-Rust fallback: substring over a manual walk (case-sensitive on demand).
fn search_rust(query: &str, scopes: &[PathBuf], case_sensitive: bool, globs: &[String]) -> Vec<FindResult> {
    let mut out = Vec::new();
    let ql = if case_sensitive { query.to_string() } else { query.to_lowercase() };
    // gather candidate files across every scope (a scope may be a file or a dir)
    let mut files = Vec::new();
    for scope in scopes {
        if scope.is_file() {
            files.push(scope.clone());
        } else {
            files.extend(collect_paths(scope).0);
        }
    }
    // apply filename globs: a file must pass every positive glob and no `!` negation
    if !globs.is_empty() {
        files.retain(|p| file_matches_globs(p, globs));
    }
    for path in files {
        if out.len() >= FIND_CAP {
            break;
        }
        // skip huge bundles (minified .cjs, etc.) — matches rg's --max-filesize
        if fs::metadata(&path).map(|m| m.len() > 1024 * 1024).unwrap_or(false) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else { continue };
        for (i, line) in content.lines().enumerate() {
            let hay = if case_sensitive { line.to_string() } else { line.to_lowercase() };
            if hay.contains(&ql) {
                out.push(FindResult {
                    path: path.clone(),
                    line: i + 1,
                    text: line.trim_start().chars().take(200).collect(),
                });
                if out.len() >= FIND_CAP {
                    break;
                }
            }
        }
    }
    out
}

/// Does `path` satisfy the glob set? Positive globs are OR'd (file passes if it
/// matches any), `!`-prefixed globs are exclusions (file fails if it matches any).
/// A pattern with `/` matches the path tail; otherwise just the file name.
fn file_matches_globs(path: &Path, globs: &[String]) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let full = path.to_string_lossy().to_string();
    let target = |g: &str| if g.contains('/') { full.as_str() } else { name.as_str() };
    let mut positives = false;
    let mut matched_positive = false;
    for g in globs {
        if let Some(neg) = g.strip_prefix('!') {
            if glob_match(neg, target(neg)) {
                return false;
            }
        } else {
            positives = true;
            if glob_match(g, target(g)) {
                matched_positive = true;
            }
        }
    }
    !positives || matched_positive
}

/// The set of workspaces that were open last time, in order — so plain `tide`
/// picks up where you left off instead of opening whatever directory you happen
/// to be standing in. Written whenever the list changes.
fn session_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/session.txt"))
}
fn load_session() -> Vec<PathBuf> {
    session_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| {
            s.lines()
                .map(|l| PathBuf::from(l.trim()))
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default()
}
fn save_session(roots: &[PathBuf]) {
    let Some(path) = session_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body: String = roots.iter().map(|p| format!("{}\n", p.to_string_lossy())).collect();
    let _ = std::fs::write(path, body);
}

/// File holding the recent-projects list (most-recent first, one path per line).
fn recents_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/recent-projects.txt"))
}

/// `/Users/me/dev/x` → `~/dev/x`; other paths are left as-is. Used to show the
/// real project folder next to a (possibly custom) workspace title.
fn tilde_path(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
}

/// Persistent custom workspace titles, keyed by absolute project path.
fn ws_titles_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/workspace-titles.json"))
}
fn load_ws_titles() -> HashMap<String, String> {
    ws_titles_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn load_ws_title(root: &Path) -> Option<String> {
    load_ws_titles().remove(&root.to_string_lossy().to_string())
}
/// The title each folder *last* carried, kept when a workspace is reset so the
/// Open Project list can still say what that folder was used for.
fn ws_last_titles_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/workspace-titles-last.json"))
}

fn load_ws_last_titles() -> HashMap<String, String> {
    ws_last_titles_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_ws_last_title(root: &Path, title: &str) {
    let Some(path) = ws_last_titles_path() else { return };
    let mut m = load_ws_last_titles();
    m.insert(root.to_string_lossy().to_string(), title.to_string());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(&m) {
        let _ = std::fs::write(path, s);
    }
}

/// Set (or clear, if empty) the custom title for `root` and persist.
fn save_ws_title(root: &Path, title: Option<&str>) {
    let Some(path) = ws_titles_path() else { return };
    let mut m = load_ws_titles();
    let key = root.to_string_lossy().to_string();
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => {
            save_ws_last_title(root, t);
            m.insert(key, t.to_string());
        }
        None => {
            // reset: the name is gone from the topbar, but remember it for ⌘O
            if let Some(prev) = m.get(&key).filter(|p| !p.trim().is_empty()) {
                let prev = prev.clone();
                save_ws_last_title(root, &prev);
            }
            m.remove(&key);
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(&m) {
        let _ = std::fs::write(path, s);
    }
}

/// A workspace whose agent was deliberately started over (a reset, or ⌃N).
/// The adapter only remembers a session once it carries messages, so the fresh
/// empty one isn't in `session/list` — without this flag the next launch would
/// auto-resume the very conversation the reset was meant to end. Cleared the
/// moment the new session gets a prompt, since from then on it's the newest one
/// anyway.
fn ws_fresh_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/agent-fresh.json"))
}
fn load_ws_fresh_set() -> HashSet<String> {
    ws_fresh_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}
fn load_ws_fresh(root: &Path) -> bool {
    load_ws_fresh_set().contains(&root.to_string_lossy().to_string())
}
fn save_ws_fresh(root: &Path, fresh: bool) {
    let Some(path) = ws_fresh_path() else { return };
    let mut set = load_ws_fresh_set();
    let key = root.to_string_lossy().to_string();
    if fresh {
        set.insert(key);
    } else if !set.remove(&key) {
        return; // nothing to write: it wasn't marked
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut list: Vec<String> = set.into_iter().collect();
    list.sort();
    if let Ok(body) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(path, body);
    }
}

/// Idle is a property of the *workspace*, not of a running pane: "nobody is on
/// this one". So unlike working/blocked/done it is persisted, and every TIDE
/// instance that has the project open sees it.
fn ws_idle_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/workspace-idle.json"))
}
fn load_ws_idle_set() -> HashSet<String> {
    ws_idle_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}
fn load_ws_idle(root: &Path) -> bool {
    load_ws_idle_set().contains(&root.to_string_lossy().to_string())
}
fn save_ws_idle(root: &Path, idle: bool) {
    let Some(path) = ws_idle_path() else { return };
    let mut set = load_ws_idle_set();
    let key = root.to_string_lossy().to_string();
    if idle {
        set.insert(key);
    } else {
        set.remove(&key);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut list: Vec<String> = set.into_iter().collect();
    list.sort();
    if let Ok(s) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(path, s);
    }
}

/// Client a workspace's work belongs to, keyed by project path. Shown next to
/// the title so client work is identifiable at a glance.
fn ws_clients_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/workspace-clients.json"))
}
fn load_ws_clients() -> HashMap<String, String> {
    ws_clients_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn load_ws_client(root: &Path) -> Option<String> {
    load_ws_clients().remove(&root.to_string_lossy().to_string())
}
fn save_ws_client(root: &Path, client: Option<&str>) {
    let Some(path) = ws_clients_path() else { return };
    let mut m = load_ws_clients();
    let key = root.to_string_lossy().to_string();
    match client.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => {
            m.insert(key, c.to_string());
        }
        None => {
            m.remove(&key);
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(&m) {
        let _ = std::fs::write(path, s);
    }
}

/// Paused workspaces: ⌥Tab skips them, so parked work stays out of the rotation
/// until you pick it up again. Persisted per project path, like the idle marker.
fn ws_paused_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/workspace-paused.json"))
}
fn load_ws_paused_set() -> HashSet<String> {
    ws_paused_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}
fn load_ws_paused(root: &Path) -> bool {
    load_ws_paused_set().contains(&root.to_string_lossy().to_string())
}
fn save_ws_paused(root: &Path, paused: bool) {
    let Some(path) = ws_paused_path() else { return };
    let mut set = load_ws_paused_set();
    let key = root.to_string_lossy().to_string();
    if paused {
        set.insert(key);
    } else {
        set.remove(&key);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut list: Vec<String> = set.into_iter().collect();
    list.sort();
    if let Ok(s) = serde_json::to_string_pretty(&list) {
        let _ = std::fs::write(path, s);
    }
}

/// Recently-opened project paths, most recent first; only existing dirs.
fn load_recent_projects() -> Vec<PathBuf> {
    let Some(file) = recents_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&file) else { return Vec::new() };
    text.lines().map(|l| PathBuf::from(l.trim())).filter(|p| p.is_dir()).collect()
}

/// File storing the last milestone used for Create PR (pre-filled next time).
fn milestone_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/last-milestone.txt"))
}

/// The milestone chosen last time Create PR ran, if any.
fn load_last_milestone() -> Option<String> {
    let text = std::fs::read_to_string(milestone_path()?).ok()?;
    let t = text.trim().to_string();
    (!t.is_empty()).then_some(t)
}

/// Remember `m` as the milestone to pre-fill next time Create PR opens.
fn save_last_milestone(m: &str) {
    let m = m.trim();
    if m.is_empty() {
        return;
    }
    let Some(file) = milestone_path() else { return };
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&file, m);
}

/// Record `root` as the most-recently-opened project (dedup, cap 20).
fn push_recent_project(root: &Path) {
    let Some(file) = recents_path() else { return };
    let mut list = load_recent_projects();
    list.retain(|p| p != root);
    list.insert(0, root.to_path_buf());
    list.truncate(20);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = list.iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<_>>().join("\n");
    let _ = std::fs::write(&file, body);
}

/// Recursively collect all files under `root`, skipping ignored and hidden
/// directories (.git, .webiny, node_modules, target, …). Caps total count so a
/// pathological tree can't hang the finder.
/// Recursively copy `src` to `dst` (a file or a whole directory tree).
/// Process-global tree clipboard shared across all workspaces (each project is
/// a separate `Storm`, so a per-view clipboard can't paste across windows/tabs).
/// Holds `(source path, is_cut)`.
fn tree_clip() -> &'static Mutex<Option<(PathBuf, bool)>> {
    use std::sync::OnceLock;
    static C: OnceLock<Mutex<Option<(PathBuf, bool)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// Accent color for a branch tag pill: `next` → green, `release/*` → orange,
/// everything else → dull gray.
fn branch_style(name: &str) -> u32 {
    if name == "next" {
        GIT_NEW // green
    } else if name.starts_with("release/") {
        0xd9a23b // orange
    } else {
        MUTED // dull gray
    }
}

/// Render a branch name as a colored tag pill (see `branch_style`).
fn branch_name_el(name: &str) -> gpui::Div {
    let fg = branch_style(name);
    div()
        .px_1()
        .rounded_md()
        .bg(rgba((fg << 8) | 0x33)) // ~20% tint of the accent
        .text_color(rgb(fg))
        .child(name.to_string())
}

fn copy_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)?.flatten() {
            copy_path(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst).map(|_| ())
    }
}

/// If `dest` already exists, find a free name by inserting " copy" (and a
/// counter) before the extension — e.g. `foo.ts` → `foo copy.ts` → `foo copy 2.ts`.
fn unique_dest(dest: PathBuf) -> PathBuf {
    if !dest.exists() {
        return dest;
    }
    let parent = dest.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let ext = dest.extension().map(|e| e.to_string_lossy().to_string());
    let make = |suffix: &str| {
        let name = match &ext {
            Some(e) => format!("{}{}.{}", stem, suffix, e),
            None => format!("{}{}", stem, suffix),
        };
        parent.join(name)
    };
    let first = make(" copy");
    if !first.exists() {
        return first;
    }
    for n in 2.. {
        let candidate = make(&format!(" copy {}", n));
        if !candidate.exists() {
            return candidate;
        }
    }
    dest
}

/// Walk the tree once, returning (files, directories) for the fuzzy finder.
fn collect_paths(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    const MAX: usize = 50_000;
    fn walk(dir: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
        if files.len() >= MAX {
            return;
        }
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if IGNORED.contains(&name.as_str()) {
                continue;
            }
            let p = e.path();
            if p.is_dir() {
                // skip hidden dirs (.webiny, .next, .cache, .git, …) — they're
                // almost always build/cache noise you don't fuzzy-open
                if name.starts_with('.') {
                    continue;
                }
                dirs.push(p.clone());
                walk(&p, files, dirs);
            } else {
                files.push(p);
                if files.len() >= MAX {
                    return;
                }
            }
        }
    }
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    walk(root, &mut files, &mut dirs);
    (files, dirs)
}


/// A short colored badge for a file's extension (WebStorm-style icon stand-in).
fn ext_badge(path: &Path) -> (String, u32) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "ts" | "tsx" => ("ts".into(), 0x3178c6),
        "js" | "jsx" | "mjs" | "cjs" => ("js".into(), 0xf1e05a),
        "rs" => ("rs".into(), 0xdea584),
        "json" => ("{}".into(), 0xcbcb41),
        "md" => ("md".into(), 0x9aa5ce),
        "toml" | "yaml" | "yml" => ("cfg".into(), 0x6d8086),
        "css" | "scss" => ("css".into(), 0x563d7c),
        "html" => ("<>".into(), 0xe34c26),
        "sh" | "zsh" | "bash" => ("sh".into(), 0x89e051),
        "lock" => ("lk".into(), 0x565f89),
        _ => ("·".into(), 0x565f89),
    }
}

/// Subsequence fuzzy match; higher score = better. None if no match.
fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let q = query.to_lowercase();
    let s = candidate.to_lowercase();
    let mut qchars = q.chars().peekable();
    let mut score = 0i32;
    let mut last: i32 = -1;
    let mut idx: i32 = 0;
    for ch in s.chars() {
        if let Some(&qc) = qchars.peek() {
            if ch == qc {
                if last >= 0 {
                    score -= idx - last - 1; // penalize gaps
                }
                last = idx;
                qchars.next();
            }
        }
        idx += 1;
    }
    if qchars.peek().is_none() {
        Some(score - last / 4) // mild preference for earlier matches
    } else {
        None
    }
}

/// Score a file-finder candidate against `query`, ranking by relevance:
/// a contiguous substring in the file *name* beats a fuzzy name match, which
/// beats a fuzzy match anywhere in the path. Higher = better. None = no match.
fn finder_score(query: &str, rel: &str) -> Option<i32> {
    let q = query.to_lowercase();
    let name = rel.rsplit('/').next().unwrap_or(rel).to_lowercase();
    // best: the query appears verbatim in the filename (prefix is even better)
    if let Some(pos) = name.find(&q) {
        let prefix = if pos == 0 { 5_000 } else { 0 };
        return Some(100_000 + prefix - (pos as i32) * 50 - name.len() as i32);
    }
    // good: fuzzy subsequence within the filename
    if let Some(sc) = fuzzy_score(&q, &name) {
        return Some(50_000 + sc - name.len() as i32);
    }
    // weak: fuzzy subsequence anywhere in the path
    fuzzy_score(&q, rel).map(|sc| sc - rel.len() as i32)
}

// ── state ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum GitState {
    New,
    Modified,
    Deleted,
}

/// Strip ANSI escape sequences (color codes etc.) from a line of output.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // consume params until the final letter
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                chars.next(); // skip a single-char escape
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

/// A small square checkbox glyph (shared by the commit + PR panes).
fn check_box(checked: bool) -> Div {
    div()
        .size(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border_1()
        .border_color(rgb(if checked { ACCENT } else { MUTED }))
        .when(checked, |d| d.bg(rgb(ACCENT)))
        .text_size(px(10.))
        .text_color(rgb(SEL_TEXT))
        .cursor_pointer()
        .child(if checked { "✓" } else { "" })
}

/// Compute side-by-side diff rows + per-line syntax runs for each side.
/// `old` Some → committed diff of `old` rev vs `new_rev` (defaulting to HEAD);
/// `old` None → working-tree diff (HEAD vs the file on disk). Callers resolve
/// the actual revs (merge-base, commit^, …) so this stays a plain git show.
fn compute_diff(
    root: &Path,
    path: &Path,
    old: &Option<String>,
    new_rev: &Option<String>,
    hl: &Highlighter,
) -> (Vec<DiffRow>, Vec<Vec<Run>>, Vec<Vec<Run>>) {
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string();
    let (old, new) = match old {
        Some(o) => (
            git_show(root, o, &rel),
            git_show(root, new_rev.as_deref().unwrap_or("HEAD"), &rel),
        ),
        None => (git_show_head(root, &rel), std::fs::read_to_string(path).unwrap_or_default()),
    };
    // expand tabs up front so the rendered grid, syntax runs, and selection all
    // share the same character positions
    let old = old.replace('\t', "    ");
    let new = new.replace('\t', "    ");
    let rows = diff::compute(&old, &new);
    let left_styles = hl.highlight(&old, path);
    let right_styles = hl.highlight(&new, path);
    (rows, left_styles, right_styles)
}

/// The scrollable side-by-side diff body. Columns are sized to the longest line
/// (tabs expanded) so it scrolls horizontally instead of clipping.
#[derive(Clone, Copy, PartialEq)]
enum DiffSide {
    Left,
    Right,
}

/// A text selection within one side of a diff (char-level, row/col).
#[derive(Clone)]
struct DiffSel {
    side: DiffSide,
    anchor: (usize, usize),
    head: (usize, usize),
}

impl DiffSel {
    /// Normalized (start, end) row/col, start <= end in reading order.
    fn range(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// Display text for one side of a row (tabs are already expanded in the rows).
fn diff_side_text(row: &DiffRow, side: DiffSide) -> String {
    let s = match side {
        DiffSide::Left => &row.left,
        DiffSide::Right => &row.right,
    };
    s.clone().unwrap_or_default()
}

/// Build styled runs for a diff line: syntax colors from `styles`, with any
/// number of background `spans` (char range + bg color, e.g. selection +
/// search matches). Later spans win where they overlap.
fn diff_line_runs(text: &str, styles: Option<&Vec<Run>>, spans: &[(usize, usize, u32)]) -> Vec<TextRun> {
    // base color runs (byte_len, rgb); fall back to one plain run
    let color_runs: Vec<(usize, u32)> = match styles {
        Some(s) if !s.is_empty() && s.iter().map(|(l, _)| *l).sum::<usize>() == text.len() => s.clone(),
        _ => vec![(text.len(), TEXT)],
    };
    let byte = |c: usize| text.char_indices().nth(c).map(|(b, _)| b).unwrap_or(text.len());
    let bspans: Vec<(usize, usize, u32)> =
        spans.iter().map(|(s, e, c)| (byte(*s), byte(*e), *c)).collect();
    // segment boundaries: color-run edges + every span edge
    let mut bounds = vec![0usize, text.len()];
    let mut p = 0usize;
    for (len, _) in &color_runs {
        p += len;
        bounds.push(p);
    }
    for (s, e, _) in &bspans {
        bounds.push((*s).min(text.len()));
        bounds.push((*e).min(text.len()));
    }
    bounds.sort_unstable();
    bounds.dedup();
    let color_at = |pos: usize| {
        let mut acc = 0usize;
        for (len, color) in &color_runs {
            acc += len;
            if pos < acc {
                return *color;
            }
        }
        TEXT
    };
    let mut runs = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        // last span covering this segment wins (selection is pushed last)
        let bg = bspans.iter().rev().find(|(s, e, _)| a >= *s && b <= *e).map(|(_, _, c)| *c);
        runs.push(TextRun {
            len: b - a,
            font: font("Menlo"),
            color: rgb(color_at(a)).into(),
            background_color: bg.map(|c| rgb(c).into()),
            underline: None,
            strikethrough: None,
        });
    }
    runs
}

/// Selected (col_start, col_end) on `side` row `r` (line `len` chars), if any.
fn diff_row_sel(sel: Option<&DiffSel>, side: DiffSide, r: usize, len: usize) -> Option<(usize, usize)> {
    let s = sel?;
    if s.side != side {
        return None;
    }
    let (a, b) = s.range();
    if r < a.0 || r > b.0 {
        return None;
    }
    let cs = if r == a.0 { a.1 } else { 0 }.min(len);
    let ce = if r == b.0 { b.1 } else { len }.min(len);
    if cs >= ce {
        None
    } else {
        Some((cs, ce))
    }
}

/// Pixel widths of the two diff columns: each is its longest line, but at least
/// half of `avail_w` (so they read 50/50 when content fits).
/// Display width (px) of one diff side: longest line, plus gutter + padding.
fn diff_side_w(rows: &[DiffRow], char_w: f32, side: DiffSide) -> f32 {
    let max = rows.iter().map(|r| diff_side_text(r, side).chars().count()).max().unwrap_or(0);
    44.0 + 16.0 + max as f32 * char_w
}

/// Text with the selected span highlighted (for one diff line).
/// Horizontal scrollbar overlay pinned to the bottom of a scroll pane.
fn h_scrollbar(handle: &ScrollHandle) -> impl IntoElement {
    let vp = f32::from(handle.bounds().size.width).max(1.0);
    let max = f32::from(handle.max_offset().x).max(0.0);
    let bar = div().absolute().left(px(0.)).bottom(px(0.)).w_full().h(px(10.)).px_1().flex().items_center();
    if max <= 0.5 {
        return bar;
    }
    let content = vp + max;
    let off = (-f32::from(handle.offset().x)).clamp(0.0, max);
    let thumb_w = (vp / content * vp).max(24.0);
    let left = (off / max) * (vp - thumb_w).max(0.0);
    bar.child(
        div()
            .relative()
            .w_full()
            .h(px(6.))
            .child(div().absolute().left(px(left)).w(px(thumb_w)).h(px(6.)).rounded_sm().bg(rgb(MUTED))),
    )
}

/// Vertical scrollbar overlay pinned to the right of a scroll pane.
fn v_scrollbar(handle: &ScrollHandle) -> impl IntoElement {
    let vp = f32::from(handle.bounds().size.height).max(1.0);
    let max = f32::from(handle.max_offset().y).max(0.0);
    let bar = div().absolute().top(px(0.)).right(px(0.)).h_full().w(px(10.)).py_1().flex().justify_center();
    if max <= 0.5 {
        return bar;
    }
    let content = vp + max;
    let off = (-f32::from(handle.offset().y)).clamp(0.0, max);
    let thumb_h = (vp / content * vp).max(24.0);
    let top = (off / max) * (vp - thumb_h).max(0.0);
    bar.child(
        div()
            .relative()
            .h_full()
            .w(px(6.))
            .child(div().absolute().top(px(top)).w(px(6.)).h(px(thumb_h)).rounded_sm().bg(rgb(MUTED))),
    )
}

/// A search match: which side, row, and the char range [start, end).
type DiffMatch = (DiffSide, usize, usize, usize);

/// One independently-scrolling side of the diff: its own 2D scroll + scrollbars.
#[allow(clippy::too_many_arguments)]
fn diff_pane(
    rows: &[DiffRow],
    side: DiffSide,
    handle: &ScrollHandle,
    char_w: f32,
    sel: Option<&DiffSel>,
    styles: &[Vec<Run>],
    matches: &[DiffMatch],
    cur_match: usize,
    caret: Option<(DiffSide, usize, usize)>,
    caret_on: bool,
    // review comments on this file, as (file line, note) — drawn as a row under
    // the line they belong to, on the right side only (that's the new code)
    comments: &[(usize, String)],
    // the row currently being commented on, plus its rendered input
    mut editing: Option<(usize, f32, gpui::AnyElement)>,
    // visible width of this pane — comment widgets use it so they don't run off
    // into the horizontally-scrollable area
    vis_w: f32,
) -> impl IntoElement {
    let side_w = diff_side_w(rows, char_w, side);
    let id = match side {
        DiffSide::Left => "diff-left",
        DiffSide::Right => "diff-right",
    };
    let mut area = div()
        .id(id)
        .size_full()
        .overflow_x_scroll()
        .overflow_y_scroll()
        .track_scroll(handle)
        .flex()
        .flex_col()
        .items_start()
        .font_family("Menlo")
        .bg(rgb(BG));
    for (i, row) in rows.iter().enumerate() {
        let bg = match (side, row.kind) {
            (DiffSide::Left, DiffKind::Del) | (DiffSide::Left, DiffKind::Replace) => DIFF_REMOVE_BG,
            (DiffSide::Right, DiffKind::Ins) | (DiffSide::Right, DiffKind::Replace) => DIFF_ADD_BG,
            _ => 0,
        };
        let no = match side {
            DiffSide::Left => row.left_no,
            DiffSide::Right => row.right_no,
        };
        let text = diff_side_text(row, side);
        let nchars = text.chars().count();
        // background spans: search matches first, selection last (so it wins)
        let mut spans: Vec<(usize, usize, u32)> = Vec::new();
        for (mi, (ms, mr, cs, ce)) in matches.iter().enumerate() {
            if *ms == side && *mr == i {
                let color = if mi == cur_match { SEARCH_CURRENT_BG } else { SEARCH_MATCH_BG };
                spans.push((*cs, *ce, color));
            }
        }
        if let Some((cs, ce)) = diff_row_sel(sel, side, i, nchars) {
            spans.push((cs, ce, SELECTION));
        }
        let line_styles = no.and_then(|n| styles.get(n - 1));
        let runs = diff_line_runs(&text, line_styles, &spans);
        let mut line = div().relative().flex().flex_row().items_center().h(px(18.)).w(px(side_w)).flex_shrink_0();
        if bg != 0 {
            line = line.bg(rgb(bg));
        }
        // blinking text caret on this side/row
        if caret_on {
            if let Some((cside, cr, cc)) = caret {
                if cside == side && cr == i {
                    line = line.child(
                        div()
                            .absolute()
                            .top(px(0.))
                            .left(px(44.0 + 8.0 + cc as f32 * char_w))
                            .w(px(1.5))
                            .h(px(18.))
                            .bg(rgb(CURSOR)),
                    );
                }
            }
        }
        // a commented line gets the same ✎ marker as the editor's gutter
        let has_note = row.right_no.is_some_and(|n| comments.iter().any(|(l, _)| *l == n));
        area = area.child(
            line.child(
                div()
                    .w(px(44.))
                    .pr_2()
                    .flex()
                    .justify_end()
                    .gap(px(2.))
                    .text_color(rgb(if has_note { COMMENT_MARK } else { LINE_NUMBER }))
                    .when(has_note, |d| {
                        d.child(div().font_family(ICON_FONT).text_size(px(9.)).child(IC_COMMENT))
                    })
                    .child(no.map(|n| n.to_string()).unwrap_or_default()),
            )
            .child(div().flex_grow(1.0).px_2().child(StyledText::new(text).with_runs(runs))),
        );
        // Notes and the comment input are extra rows. Both panes scroll
        // independently, so each extra row needs a matching spacer on the other
        // side or the two files stop lining up. Heights are fixed for that reason.
        const NOTE_H: f32 = RC_NOTE_H;
        if let Some(n) = row.right_no {
            for (_, note) in comments.iter().filter(|(l, _)| *l == n) {
                area = area.child(if side == DiffSide::Right {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .h(px(NOTE_H))
                        .w(px(vis_w))
                        .flex_shrink_0()
                        .px_2()
                        .bg(rgba(COMMENT_TINT))
                        .text_size(px(11.))
                        .text_color(rgb(COMMENT_MARK))
                        .child(
                            div()
                                .w(px(44.))
                                .flex_shrink_0()
                                .font_family(ICON_FONT)
                                .text_size(px(10.))
                                .child(IC_COMMENT),
                        )
                        .child(div().flex_grow(1.0).truncate().child(note.clone()))
                } else {
                    div().h(px(NOTE_H)).w(px(vis_w)).flex_shrink_0() // alignment spacer
                });
            }
        }
        if let Some((erow, box_h)) = editing.as_ref().map(|(r, h, _)| (*r, *h)) {
            if erow == i {
                area = area.child(if side == DiffSide::Right {
                    let el = editing.take().map(|(_, _, el)| el);
                    div().h(px(box_h)).w(px(vis_w)).flex_shrink_0().children(el)
                } else {
                    div().h(px(box_h)).w(px(vis_w)).flex_shrink_0()
                });
            }
        }
    }
    // fill the (already split-sized) wrapper — the 50/50 vs custom split is set
    // by the flex_grow on the wrappers in DiffWindow::render, not here
    div()
        .relative()
        .w_full()
        .h_full()
        .overflow_hidden()
        .child(area)
        .child(v_scrollbar(handle))
        .child(h_scrollbar(handle))
}

/// Progress-bar color interpolated red (0%) → yellow → green (100%).
fn progress_color(pct: usize) -> u32 {
    let h = (pct.min(100) as f32 / 100.0) * 120.0; // hue 0=red .. 120=green
    hsl_to_rgb(h, 0.55, 0.5)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> u32 {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = if hp < 1.0 {
        (c, x, 0.0)
    } else if hp < 2.0 {
        (x, c, 0.0)
    } else if hp < 3.0 {
        (0.0, c, x)
    } else if hp < 4.0 {
        (0.0, x, c)
    } else if hp < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = l - c / 2.0;
    let to8 = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u32;
    (to8(r) << 16) | (to8(g) << 8) | to8(b)
}

/// File-status color used in the PR review tree: added=blue, modified=green,
/// deleted=gray (per the requested mapping).
fn pr_status_color(state: GitState) -> u32 {
    match state {
        GitState::New => 0x73c991,      // green — created
        GitState::Modified => 0x569cd6, // blue — modified
        GitState::Deleted => 0x858585,  // gray — deleted
    }
}

/// A node in the commit changes tree, built from the changed-file paths.
#[derive(Default)]
struct ChangeDir {
    dirs: std::collections::BTreeMap<String, ChangeDir>,
    files: Vec<(String, PathBuf, GitState)>, // (name, absolute path, state)
}

impl ChangeDir {
    fn insert(&mut self, comps: &[&str], full: PathBuf, state: GitState) {
        match comps {
            [name] => self.files.push((name.to_string(), full, state)),
            [head, rest @ ..] => {
                self.dirs.entry(head.to_string()).or_default().insert(rest, full, state)
            }
            [] => {}
        }
    }

    fn collect_files(&self, out: &mut Vec<PathBuf>) {
        for (_, p, _) in &self.files {
            out.push(p.clone());
        }
        for d in self.dirs.values() {
            d.collect_files(out);
        }
    }
}

/// One flattened row of the commit tree.
enum CommitRow {
    Dir { depth: usize, key: PathBuf, label: String, files: Vec<PathBuf> },
    File { depth: usize, path: PathBuf, name: String, state: GitState },
}

/// Flatten the tree into display rows, compressing single-child directory
/// chains (WebStorm-style: `a/b/c/src`) and skipping collapsed subtrees.
fn flatten_changes(
    node: &ChangeDir,
    base: &Path,
    depth: usize,
    collapsed: &HashSet<PathBuf>,
    out: &mut Vec<CommitRow>,
) {
    for (name, child) in &node.dirs {
        let mut label = name.clone();
        let mut path = base.join(name);
        let mut cur = child;
        // absorb a chain of single directories into one node
        while cur.files.is_empty() && cur.dirs.len() == 1 {
            let (cn, cc) = cur.dirs.iter().next().unwrap();
            label = format!("{}/{}", label, cn);
            path = path.join(cn);
            cur = cc;
        }
        let mut files = Vec::new();
        cur.collect_files(&mut files);
        let is_collapsed = collapsed.contains(&path);
        out.push(CommitRow::Dir { depth, key: path.clone(), label, files });
        if !is_collapsed {
            flatten_changes(cur, &path, depth + 1, collapsed, out);
        }
    }
    for (name, p, state) in &node.files {
        out.push(CommitRow::File { depth, path: p.clone(), name: name.clone(), state: *state });
    }
}

/// Which right-click menu is open.
#[derive(Clone, Copy, PartialEq)]
enum CtxKind {
    Tree,
    /// A file row in Changes / PR Review — file-level actions only.
    ReviewFile,
    /// Selected text in the agent transcript.
    AgentSel,
}

#[derive(Clone, Copy, PartialEq)]
enum LeftView {
    Files,
    Changes,
    PullRequest,
    /// Pending review comments, with a button that sends them to the agent.
    Review,
}

/// PR review file filter by viewed-state.
#[derive(Clone, Copy, PartialEq)]
enum PrViewFilter {
    All,
    Unviewed,
    Viewed,
}

/// Commit pane file filter by checked-for-commit state.
#[derive(Clone, Copy, PartialEq)]
enum CommitFilter {
    All,
    Unchecked,
    Checked,
}

/// Render an agent reply as light markdown: inline `code` chips, **bold**,
/// *italic*, bullets, and fenced blocks. Words are separate children with
/// `flex_wrap`, which is what lets a formatted line still wrap inside the box.
fn md_text(text: &str, size: f32, color: u32) -> Div {
    let rows = md_prepare(text);
    let mut col = div().flex().flex_col().min_w(px(0.)).text_color(rgb(color));
    for row in rows.iter() {
        col = col.child(div().text_size(px(size)).child(md_row_el(row, false)));
    }
    col
}

/// A markdown line ready to draw: the flattened text plus the styling runs over
/// it. One `StyledText` per line instead of one div per word — a long agent
/// transcript was building tens of thousands of elements every frame.
#[derive(Clone)]
struct MdRow {
    text: SharedString,
    highlights: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)>,
    /// Ranges that render monospaced (inline `code`, whole code blocks).
    mono: Vec<std::ops::Range<usize>>,
    size: f32,
    heading: bool,
    /// A fenced code block: drawn in its own bordered, scrollable slab.
    fence: bool,
    indent: f32,
    /// A table's cells (each already flattened the same way).
    cells: Option<Vec<MdRow>>,
    table_header: bool,
    /// A blank line — pure paragraph spacing.
    gap: bool,
    /// Position among this document's rows, so a fence's scroll id is unique
    /// even when two blocks happen to be the same length.
    ix: usize,
}

fn md_row_blank() -> MdRow {
    MdRow {
        text: SharedString::default(),
        highlights: Vec::new(),
        mono: Vec::new(),
        size: 0.0,
        heading: false,
        fence: false,
        indent: 0.0,
        cells: None,
        table_header: false,
        gap: true,
        ix: 0,
    }
}

/// Flatten spans into one string plus its styling runs.
fn md_flatten(spans: &[md::Span], size: f32, prefix: &str) -> MdRow {
    let mut text = String::from(prefix);
    let mut highlights = Vec::new();
    let mut mono = Vec::new();
    for span in spans {
        let start = text.len();
        match span {
            md::Span::Text(t) => text.push_str(t),
            md::Span::Code(c) => {
                text.push_str(c);
                highlights.push((
                    start..text.len(),
                    gpui::HighlightStyle {
                        background_color: Some(rgb(CURRENT_LINE).into()),
                        ..Default::default()
                    },
                ));
                mono.push(start..text.len());
            }
            md::Span::Bold(t) => {
                text.push_str(t);
                highlights.push((
                    start..text.len(),
                    gpui::HighlightStyle {
                        color: Some(rgb(SEL_TEXT).into()),
                        font_weight: Some(gpui::FontWeight::BOLD),
                        ..Default::default()
                    },
                ));
            }
            md::Span::Italic(t) => {
                text.push_str(t);
                highlights.push((
                    start..text.len(),
                    gpui::HighlightStyle {
                        font_style: Some(gpui::FontStyle::Italic),
                        ..Default::default()
                    },
                ));
            }
        }
    }
    MdRow {
        text: text.into(),
        highlights,
        mono,
        size,
        heading: false,
        fence: false,
        indent: 0.0,
        cells: None,
        table_header: false,
        gap: false,
        ix: 0,
    }
}

/// Parse + highlight a markdown document into drawable rows. Cached by content,
/// so re-rendering a long transcript costs no parsing and no syntax work.
fn md_prepare(text: &str) -> Arc<Vec<MdRow>> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    static CACHE: std::sync::OnceLock<Mutex<HashMap<(u64, usize), Arc<Vec<MdRow>>>>> =
        std::sync::OnceLock::new();
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    let key = (h.finish(), text.len());
    if let Some(hit) = CACHE.get_or_init(Default::default).lock().ok().and_then(|c| c.get(&key).cloned()) {
        return hit;
    }

    let mut rows: Vec<MdRow> = Vec::new();
    let mut fence: Vec<String> = Vec::new();
    let mut fence_lang = String::new();
    let mut table: Vec<MdRow> = Vec::new();

    // a fenced block becomes one row: the whole body, highlighted by syntect
    let flush_fence = |fence: &mut Vec<String>, lang: &str, rows: &mut Vec<MdRow>| {
        if fence.is_empty() {
            return;
        }
        let body = std::mem::take(fence).join("\n");
        let runs = highlighter().highlight_lang(&body, lang);
        let mut highlights = Vec::new();
        let mut at = 0usize;
        for (li, line) in body.split('\n').enumerate() {
            let mut col = at;
            for (len, color) in runs.get(li).into_iter().flatten() {
                let end = (col + len).min(at + line.len());
                if col < end && body.is_char_boundary(col) && body.is_char_boundary(end) {
                    highlights.push((
                        col..end,
                        gpui::HighlightStyle { color: Some(rgb(*color).into()), ..Default::default() },
                    ));
                }
                col = end;
            }
            at += line.len() + 1; // past the newline
        }
        let len = body.len();
        rows.push(MdRow {
            text: body.into(),
            highlights,
            mono: vec![0..len],
            size: 13.0,
            heading: false,
            fence: true,
            indent: 0.0,
            cells: None,
            table_header: false,
            gap: false,
            ix: rows.len(),
        });
    };

    for line in md::parse(text) {
        if let Some(cells) = line.cells {
            let mut row = md_flatten(&[], 13.5, "");
            row.cells = Some(cells.iter().map(|c| md_flatten(c, 13.5, "")).collect());
            row.table_header = line.header;
            table.push(row);
            continue;
        }
        if !table.is_empty() {
            rows.append(&mut table);
        }
        if line.code {
            if !line.lang.is_empty() {
                fence_lang = line.lang.clone();
            }
            fence.push(match line.spans.first() {
                Some(md::Span::Text(t)) => t.clone(),
                _ => String::new(),
            });
            continue;
        }
        flush_fence(&mut fence, &fence_lang, &mut rows);
        let size = match line.heading {
            1 => 23.0,
            2 => 19.0,
            3 => 16.5,
            4..=6 => 15.0,
            _ => 14.5,
        };
        if line.spans.is_empty() && line.bullet.is_none() {
            rows.push(md_row_blank());
            continue;
        }
        let prefix = line.bullet.map(|b| format!("{b}  ")).unwrap_or_default();
        let mut row = md_flatten(&line.spans, size, &prefix);
        row.heading = line.heading > 0;
        row.indent = line.indent as f32 * 16.0;
        rows.push(row);
    }
    if !table.is_empty() {
        rows.append(&mut table);
    }
    flush_fence(&mut fence, &fence_lang, &mut rows);

    let mut rows = rows;
    for (i, r) in rows.iter_mut().enumerate() {
        r.ix = i;
    }
    let out = Arc::new(rows);
    if let Ok(mut c) = CACHE.get_or_init(Default::default).lock() {
        // bounded: a long session would otherwise keep every version of every
        // streaming message alive
        if c.len() > 512 {
            c.clear();
        }
        c.insert(key, out.clone());
    }
    out
}

/// Draw one prepared row.
fn md_row_el(row: &MdRow, selected: bool) -> AnyElement {
    if row.gap {
        return div().h(px(8.)).into_any_element();
    }
    if let Some(cells) = &row.cells {
        let n = cells.len().max(1);
        let mut el = div()
            .flex()
            .flex_row()
            .items_start()
            .when(row.table_header, |d| d.bg(rgb(CURRENT_LINE)))
            .border_b_1()
            .border_color(rgb(BORDER));
        for (i, cell) in cells.iter().enumerate() {
            el = el.child(
                div()
                    .flex_basis(relative(1.0 / n as f32))
                    .min_w(px(0.))
                    .px_2()
                    .py_1()
                    .when(i + 1 < n, |d| d.border_r_1().border_color(rgb(BORDER)))
                    .child(md_styled_sel(cell, row.table_header, selected)),
            );
        }
        return el.into_any_element();
    }
    if row.fence {
        return div()
            .id(SharedString::from(format!("md-fence-{}-{}", row.ix, row.text.len())))
            .my_1()
            .px_2()
            .py_1()
            .bg(rgb(CURRENT_LINE))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .overflow_x_scroll()
            .child(div().whitespace_nowrap().child(md_styled_sel(row, false, selected)))
            .into_any_element();
    }
    div()
        .when(row.indent > 0.0, |d| d.pl(px(row.indent)))
        .when(row.heading, |d| d.mt_2())
        .child(md_styled_sel(row, row.heading, selected))
        .into_any_element()
}

/// One row's text as a single styled element: `mono` ranges get Menlo, the
/// highlight runs carry colour, weight, italics and code backgrounds.
/// `selected` drops the inline-code chip backgrounds: they're opaque and would
/// paint over the row's selection highlight, leaving `code` words looking
/// unselected inside a selected line.
/// gpui's `compute_runs` walks highlight ranges in order and assumes they don't
/// overlap: out-of-order or overlapping ranges shift every run after them, which
/// is how a link's underline ended up on the prose next to it. Flatten to
/// sorted, non-overlapping runs, merging the styles that cover each stretch.
fn merged_highlights(
    spans: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)>,
) -> Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> {
    let mut bounds: Vec<usize> = Vec::with_capacity(spans.len() * 2);
    for (r, _) in &spans {
        bounds.push(r.start);
        bounds.push(r.end);
    }
    bounds.sort_unstable();
    bounds.dedup();
    let mut out: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        // later spans win per field, which is why links (pushed last) keep their
        // colour over an inline-code style underneath
        let mut style: Option<gpui::HighlightStyle> = None;
        for (r, st) in &spans {
            if r.start <= a && r.end >= b {
                style = Some(match style {
                    Some(acc) => acc.highlight(*st),
                    None => *st,
                });
            }
        }
        if let Some(style) = style {
            // extend the previous run when the style is identical
            match out.last_mut() {
                Some((pr, pst)) if pr.end == a && *pst == style => pr.end = b,
                _ => out.push((a..b, style)),
            }
        }
    }
    out
}

fn md_styled_sel(row: &MdRow, bold: bool, selected: bool) -> gpui::StyledText {
    md_styled_links(row, bold, selected, &[])
}

/// As `md_styled_sel`, plus `links` drawn underlined in the accent colour — the
/// file references the transcript makes clickable.
fn md_styled_links(
    row: &MdRow,
    bold: bool,
    selected: bool,
    links: &[std::ops::Range<usize>],
) -> gpui::StyledText {
    let mut hl: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = row
        .highlights
        .iter()
        .cloned()
        .map(|(r, mut style)| {
            if selected {
                style.background_color = None;
            }
            (r, style)
        })
        .collect();
    if bold {
        hl.push((
            0..row.text.len(),
            gpui::HighlightStyle {
                color: Some(rgb(SEL_TEXT).into()),
                font_weight: Some(gpui::FontWeight::BOLD),
                ..Default::default()
            },
        ));
    }
    for r in links {
        if row.text.is_char_boundary(r.start) && row.text.is_char_boundary(r.end.min(row.text.len()))
        {
            hl.push((
                r.start..r.end.min(row.text.len()),
                gpui::HighlightStyle {
                    color: Some(rgb(ACCENT).into()),
                    underline: Some(gpui::UnderlineStyle {
                        thickness: px(1.),
                        color: Some(rgb(ACCENT).into()),
                        wavy: false,
                    }),
                    ..Default::default()
                },
            ));
        }
    }
    let mut mono: Vec<(std::ops::Range<usize>, SharedString)> =
        row.mono.iter().cloned().map(|r| (r, SharedString::from("Menlo"))).collect();
    mono.sort_by_key(|(r, _)| r.start);
    gpui::StyledText::new(row.text.clone())
        .with_highlights(merged_highlights(hl))
        .with_font_family_overrides(mono)
}

/// Clamp a byte span into `text`, snapping both ends down to char boundaries.
fn clamp_span_str(text: &str, a: usize, b: usize) -> (usize, usize) {
    let floor = |mut i: usize| {
        i = i.min(text.len());
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    };
    let (a, b) = (floor(a), floor(b));
    (a.min(b), b.max(a))
}

/// A whole markdown file, rendered at document scale: sized headings, bullets,
/// fenced blocks as one monospaced slab, and blank lines kept as spacing. Shares
/// the parser with `md_text`; only the sizing differs.
fn md_doc(text: &str) -> Div {
    // the caller decides the padding: a file preview wants a margin, an agent
    // bubble sits flush in the transcript
    let rows = md_prepare(text);
    let mut col = div().flex().flex_col().gap(px(4.)).text_color(rgb(TEXT));
    // consecutive table rows share one bordered grid
    let mut i = 0;
    while i < rows.len() {
        if rows[i].cells.is_some() {
            let mut grid = div()
                .my_1()
                .flex()
                .flex_col()
                .border_1()
                .border_color(rgb(BORDER))
                .rounded_md()
                .overflow_hidden();
            while i < rows.len() && rows[i].cells.is_some() {
                grid = grid.child(md_row_el(&rows[i], false));
                i += 1;
            }
            col = col.child(grid);
            continue;
        }
        let row = &rows[i];
        col = col.child(div().text_size(px(row.size.max(1.0))).child(md_row_el(row, false)));
        i += 1;
    }
    col
}

/// The `packages/<name>` dirs this branch touches — the same set `wwp` watches:
/// changed against the base branch, plus the working tree, plus untracked files.
/// Computed with git rather than read off the PR pane, which is only populated
/// once that pane has loaded.
fn changed_packages(root: &Path, base: &str) -> Vec<String> {
    let run = |args: &[&str]| -> Vec<String> {
        Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.to_string()).collect())
            .unwrap_or_default()
    };
    let base_ref = resolve_base_ref(root, base);
    let mut names: Vec<String> = run(&["diff", "--name-only", &format!("{base_ref}...HEAD")])
        .into_iter()
        .chain(run(&["diff", "--name-only", "HEAD"]))
        .chain(run(&["ls-files", "--others", "--exclude-standard"]))
        .filter_map(|p| {
            let rest = p.strip_prefix("packages/")?;
            let name = rest.split('/').next()?;
            (!name.is_empty() && rest.contains('/')).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// An image pasted into the agent composer: what the adapter needs (mime type
/// and base64) plus the decoded bytes, so the chip can show it and hovering can
/// preview it.
#[derive(Clone)]
struct PastedImage {
    mime: String,
    b64: String,
    bytes: usize,
    image: Arc<gpui::Image>,
    /// The token written into the message text (`Image #1`), so the chip and the
    /// prose refer to the same picture — Claude Code does the same thing.
    label: String,
}

/// A picture that went out with a message. The base64 the wire needed is not
/// kept — only what it takes to show the chip again.
#[derive(Clone)]
struct SentImage {
    label: String,
    mime: String,
    bytes: usize,
    image: Arc<gpui::Image>,
}

/// Tooltip that shows a pasted image at a readable size.
struct ImagePreview {
    image: Arc<gpui::Image>,
}

impl Render for ImagePreview {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_1()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .child(gpui::img(self.image.clone()).max_w(px(840.)).max_h(px(640.)))
    }
}

/// The `[Image #3]` token ending at `caret` (a trailing space counts as part of
/// it), as `(start, end, label)`. Used to make backspace remove the whole token
/// — and its attachment — rather than one character of it.
fn image_token_before(text: &str, caret: usize) -> Option<(usize, usize, String)> {
    let end = caret.min(text.len());
    let head = text.get(..end)?;
    let head = head.strip_suffix(' ').unwrap_or(head);
    let head = head.strip_suffix(']')?;
    let open = head.rfind('[')?;
    let label = head.get(open + 1..)?;
    is_atomic_label(label).then(|| (open, end, label.to_string()))
}

/// Labels the composer treats as one unit: pasted images and collapsed pastes.
fn is_atomic_label(label: &str) -> bool {
    label.starts_with("Image #") || label.starts_with("Pasted text #")
}

/// The `[Image #3]` token starting at `caret`, as `(start, end)`.
fn image_token_after(text: &str, caret: usize) -> Option<(usize, usize)> {
    let start = caret.min(text.len());
    let tail = text.get(start..)?;
    if !tail.starts_with("[Image #") && !tail.starts_with("[Pasted text #") {
        return None;
    }
    let close = tail.find(']')?;
    let mut end = start + close + 1;
    if text.get(end..).is_some_and(|r| r.starts_with(' ')) {
        end += 1; // the space the paste added travels with it
    }
    Some((start, end))
}

/// The `[Image #3]` token that `caret` sits strictly inside, as `(start, end)`.
/// Word motions (⌥←/⌥→) land mid-token otherwise, which puts the caret in the
/// middle of something that behaves like one character.
fn image_token_around(text: &str, caret: usize) -> Option<(usize, usize)> {
    let mut at = 0usize;
    loop {
        let rest = text.get(at..)?;
        let rel = match (rest.find("[Image #"), rest.find("[Pasted text #")) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        let start = at + rel;
        let Some(close) = text.get(start..)?.find(']') else { break };
        let mut end = start + close + 1;
        if text.get(end..).is_some_and(|r| r.starts_with(' ')) {
            end += 1;
        }
        if caret > start && caret < end {
            return Some((start, end));
        }
        at = end;
    }
    None
}

/// Comments quoted out of the agent's answer aren't attached to a file, so they
/// carry this sentinel path (line 0) instead. Rendered as "agent answer".
const AGENT_SRC: &str = "<agent answer>";

fn is_agent_src(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == AGENT_SRC)
}

/// Which transcript message a comment on the answer belongs to — the index the
/// sentinel path carries as its last segment.
fn agent_msg_index(path: &Path) -> Option<usize> {
    is_agent_src(path).then(|| path.file_name()?.to_str()?.parse().ok()).flatten()
}

/// Markdown by extension — the only files that get a rendered view.
fn is_markdown(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|e| e == "md" || e == "markdown" || e == "mdx")
}

/// Green under half, amber past it, red when the window is nearly spent.
fn usage_color(pct: f32) -> u32 {
    match pct {
        p if p >= 90.0 => GIT_DELETED,
        p if p >= 60.0 => GIT_MODIFIED,
        _ => GIT_NEW,
    }
}

/// A short random lowercase-alphanumeric id (e.g. for `wip: <id>` commits),
/// seeded from the system clock and mixed so successive calls differ.
fn random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    const CH: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut s = String::new();
    for _ in 0..5 {
        // splitmix64-style mixing for a decent spread from a clock seed
        n = n.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = n;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^= z >> 31;
        s.push(CH[(z % CH.len() as u64) as usize] as char);
    }
    s
}

/// Condense a process's argv into something that fits a pane badge: paths become
/// basenames, a leading interpreter is dropped (`node …/yarn watch` → `yarn
/// watch`), flags are cut, and the rest is truncated.
fn short_cmd(args: &str) -> String {
    let is_interpreter = |t: &str| {
        matches!(t, "sh" | "bash" | "zsh" | "fish" | "env" | "sudo" | "npm" | "npx" | "bunx")
            || t.starts_with("node")
            || t.starts_with("python")
            || t.starts_with("ruby")
            || t.starts_with("perl")
    };
    let mut parts: Vec<String> = args
        .split_whitespace()
        .filter(|t| !t.starts_with('-')) // flags say little in a badge
        .map(|t| t.rsplit('/').next().unwrap_or(t).to_string())
        .filter(|t| !t.is_empty())
        .collect();
    while parts.len() > 1 && is_interpreter(&parts[0]) {
        parts.remove(0);
    }
    let joined = parts.join(" ");
    if joined.chars().count() > 38 {
        format!("{}…", joined.chars().take(37).collect::<String>())
    } else {
        joined
    }
}

/// What each terminal pane is running, keyed by its shell's pid: the pane's
/// foreground child, condensed. One `ps` for the whole tree, so this costs the
/// same whether there are two panes or twenty.
fn running_cmds(shell_pids: &[u32]) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    if shell_pids.is_empty() {
        return out;
    }
    let Ok(o) = Command::new("ps").args(["-axo", "pid=,ppid=,args="]).output() else { return out };
    let text = String::from_utf8_lossy(&o.stdout);
    // ppid -> (pid, args) children, in ps order (oldest first)
    let mut kids: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else { continue };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else { continue };
        let args = it.collect::<Vec<_>>().join(" ");
        kids.entry(ppid).or_default().push((pid, args));
    }
    for &sh in shell_pids {
        // the shell's own child is the command it's running; nothing → idle shell
        if let Some((_, args)) = kids.get(&sh).and_then(|v| v.first()) {
            let label = short_cmd(args);
            if !label.is_empty() {
                out.insert(sh, label);
            }
        }
    }
    out
}

/// Resident memory of our own process, in MB (via `ps`).
fn process_mem_mb() -> u64 {
    let pid = std::process::id().to_string();
    Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

/// Contents of a file at HEAD (empty for untracked/new files).
fn git_show_head(root: &Path, rel: &str) -> String {
    git_show(root, "HEAD", rel)
}

/// `git show <rev>:<rel>` — file contents at a revision (empty if absent).
fn git_show(root: &Path, rev: &str, rel: &str) -> String {
    Command::new("git")
        .arg("show")
        .arg(format!("{}:{}", rev, rel))
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
}

/// The PR base branch: origin's default branch, falling back to main/master.
fn git_default_branch(root: &Path) -> String {
    if let Some(b) = Command::new("git")
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_start_matches("origin/").to_string())
        .filter(|s| !s.is_empty())
    {
        return b;
    }
    // origin/HEAD isn't always set (it's only written on clone); probe the usual
    // names, remote-tracking ref first so a branch we've never checked out counts
    for cand in ["main", "master", "next", "develop", "trunk"] {
        for git_ref in [format!("refs/remotes/origin/{cand}"), format!("refs/heads/{cand}")] {
            let ok = Command::new("git")
                .args(["rev-parse", "--verify", "--quiet", &git_ref])
                .current_dir(root)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return cand.to_string();
            }
        }
    }
    "main".to_string()
}

/// What GitHub says about the PR on this branch: base branch, the base tip its
/// diff is computed against, and how many files it lists. One `gh` call instead
/// of three, and `base_oid` + `changed_files` are what tell us a stale local
/// `origin/<base>` is inflating our own file list.
struct PrMeta {
    base: String,
    base_oid: String,
    changed_files: usize,
    node_id: String,
}

fn gh_pr_meta(root: &Path) -> Option<PrMeta> {
    let out = Command::new("gh")
        .args(["pr", "view", "--json", "baseRefName,baseRefOid,changedFiles,id"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let base = v.get("baseRefName")?.as_str()?.to_string();
    if base.is_empty() {
        return None;
    }
    Some(PrMeta {
        base,
        base_oid: v.get("baseRefOid").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        changed_files: v.get("changedFiles").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        node_id: v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
    })
}

/// True when `rev` is a commit this clone already has.
fn git_has_commit(root: &Path, rev: &str) -> bool {
    !rev.is_empty()
        && Command::new("git")
            .args(["cat-file", "-e", &format!("{rev}^{{commit}}")])
            .current_dir(root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}

/// Fetch just the PR's base branch, so `origin/<base>` — and with it the
/// merge-base our three-dot diff uses — matches what GitHub is comparing against.
fn git_fetch_base(root: &Path, base: &str) -> bool {
    Command::new("git")
        .args(["fetch", "--no-tags", "--quiet", "origin", base])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Why the local file list disagrees with GitHub's, if it does. Two causes,
/// both from a clone that hasn't fetched: `origin/<base>` sits behind the tip
/// GitHub diffs against (so our merge-base is too far back and we list every
/// file that landed on the base since), or the base tip isn't here at all.
fn pr_stale_note(root: &Path, meta: &PrMeta, ours: usize) -> Option<String> {
    if meta.base_oid.is_empty() {
        return None;
    }
    let base_ref = resolve_base_ref(root, &meta.base);
    let local = Command::new("git")
        .args(["rev-parse", &base_ref])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if local == meta.base_oid {
        return None;
    }
    // the base moved on GitHub; only a *missing* commit or a differing count
    // actually distorts what we show
    if !git_has_commit(root, &meta.base_oid) {
        return Some(format!(
            "{base_ref} is behind GitHub — showing {ours} files, the PR has {}",
            meta.changed_files
        ));
    }
    (meta.changed_files > 0 && ours != meta.changed_files).then(|| {
        format!(
            "{base_ref} is behind GitHub — showing {ours} files, the PR has {}",
            meta.changed_files
        )
    })
}

/// The PR's base branch for the current checkout. Prefers GitHub's own answer
/// (`gh pr view`), so it matches the actual PR; falls back to the repo default.
fn git_pr_base(root: &Path) -> String {
    if let Some(b) = Command::new("gh")
        .args(["pr", "view", "--json", "baseRefName", "-q", ".baseRefName"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        return b;
    }
    git_default_branch(root)
}

/// The PR's GraphQL node id for the current branch (used to mark files viewed).
fn gh_pr_node_id(root: &Path) -> Option<String> {
    Command::new("gh")
        .args(["pr", "view", "--json", "id", "-q", ".id"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn gh_json_field(root: &Path, args: &[&str]) -> Option<String> {
    Command::new("gh")
        .args(args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Files already marked "viewed" on the PR (per the GitHub web UI), as absolute
/// paths. Paginated GraphQL, so it covers PRs with thousands of files.
fn gh_pr_viewed_files(root: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let Some(nwo) = gh_json_field(root, &["repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"]) else {
        return out;
    };
    let Some((owner, repo)) = nwo.split_once('/') else { return out };
    let Some(num) = gh_json_field(root, &["pr", "view", "--json", "number", "-q", ".number"]) else {
        return out;
    };
    let query = "query($owner:String!,$repo:String!,$num:Int!,$endCursor:String){repository(owner:$owner,name:$repo){pullRequest(number:$num){files(first:100,after:$endCursor){nodes{path viewerViewedState} pageInfo{hasNextPage endCursor}}}}}";
    let result = Command::new("gh")
        .args([
            "api",
            "graphql",
            "--paginate",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("owner={owner}"),
            "-f",
            &format!("repo={repo}"),
            "-F",
            &format!("num={num}"),
            "-q",
            ".data.repository.pullRequest.files.nodes[] | select(.viewerViewedState==\"VIEWED\") | .path",
        ])
        .current_dir(root)
        .output();
    if let Ok(o) = result {
        if o.status.success() {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                let p = line.trim();
                if !p.is_empty() {
                    out.insert(root.join(p));
                }
            }
        }
    }
    out
}

/// Mark/unmark a file as viewed on the PR via GitHub's GraphQL API.
fn gh_set_file_viewed(root: &Path, pr_id: &str, path: &str, viewed: bool) -> bool {
    let field = if viewed { "markFileAsViewed" } else { "unmarkFileAsViewed" };
    let query = format!(
        "mutation($id:ID!,$path:String!){{ {field}(input:{{pullRequestId:$id, path:$path}}){{ clientMutationId }} }}"
    );
    Command::new("gh")
        .args([
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("id={pr_id}"),
            "-f",
            &format!("path={path}"),
        ])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve a base branch name to the ref to diff against: prefer the remote
/// tracking ref `origin/<base>` (what GitHub compares against) when present.
fn resolve_base_ref(root: &Path, base: &str) -> String {
    let origin = format!("origin/{}", base);
    let ok = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("{}^{{commit}}", origin)])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        origin
    } else {
        base.to_string()
    }
}

/// The merge-base of `base_ref` and HEAD (where the branch diverged), or None.
/// This is the "old" side GitHub's PR diff compares against (three-dot).
fn git_merge_base(root: &Path, base_ref: &str) -> Option<String> {
    Command::new("git")
        .args(["merge-base", base_ref, "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The rev a PR/push diff compares against: the merge-base of `origin/<base>`
/// (or `<base>`) and HEAD — the three-dot point GitHub uses.
fn diff_base_rev(root: &Path, base: &str) -> String {
    let base_ref = resolve_base_ref(root, base.strip_prefix("origin/").unwrap_or(base));
    git_merge_base(root, &base_ref).unwrap_or(base_ref)
}

/// Files changed by a single commit, as (absolute path, status).
fn git_commit_files(root: &Path, sha: &str) -> Vec<(PathBuf, GitState)> {
    let out = Command::new("git")
        .args(["diff-tree", "--no-commit-id", "--name-status", "-M", "-r", sha])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut files = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else { continue };
        let path = parts.last().unwrap_or("").trim();
        if path.is_empty() {
            continue;
        }
        let state = match status.chars().next() {
            Some('A') => GitState::New,
            Some('D') => GitState::Deleted,
            _ => GitState::Modified,
        };
        files.push((root.join(path), state));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// PR changes: files changed on this branch since it diverged from `base`,
/// as (absolute path, status). Uses the three-dot (merge-base) diff, comparing
/// against `origin/<base>` to match GitHub's "Files changed".
fn git_pr_files(root: &Path, base: &str) -> Vec<(PathBuf, GitState)> {
    let base_ref = resolve_base_ref(root, base);
    let out = Command::new("git")
        .args(["diff", "--name-status", &format!("{}...HEAD", base_ref)])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut files = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else { continue };
        // renames/copies have two paths ("R100\told\tnew"); take the last
        let path = parts.last().unwrap_or("").trim();
        if path.is_empty() {
            continue;
        }
        let state = match status.chars().next() {
            Some('A') => GitState::New,
            Some('D') => GitState::Deleted,
            _ => GitState::Modified, // M, R, C, T, …
        };
        files.push((root.join(path), state));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// The upstream tracking ref of the current branch (e.g. `origin/feature`),
/// or None when the branch has no upstream configured.
fn git_upstream(root: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Commits in `range` (e.g. `origin/feat..HEAD`) as (short hash, subject),
/// newest first.
fn git_log_range(root: &Path, range: &str) -> Vec<(String, String)> {
    let out = Command::new("git")
        .args(["log", "--format=%h%x00%s", range])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\0').map(|(h, s)| (h.to_string(), s.to_string())))
        .collect()
}

/// One commit row in the Git log panel.
#[derive(Clone)]
struct LogCommit {
    hash: String,   // full sha
    short: String,  // abbreviated sha
    author: String, // author name
    date: String,   // formatted date/time
    subject: String,
}

/// Recent commits reachable from `rev` (a branch name, "HEAD", or "--all"),
/// newest first, capped at `limit`. Fields are unit-separated (\x1f) so the
/// subject can safely contain any character.
fn git_log_commits(root: &Path, rev: &str, limit: usize) -> Vec<LogCommit> {
    let out = Command::new("git")
        .args([
            "log",
            &format!("-n{}", limit),
            "--date=format:%Y-%m-%d %H:%M",
            "--pretty=format:%H\x1f%h\x1f%an\x1f%ad\x1f%s",
            rev,
        ])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(5, '\x1f');
            Some(LogCommit {
                hash: f.next()?.to_string(),
                short: f.next()?.to_string(),
                author: f.next()?.to_string(),
                date: f.next()?.to_string(),
                subject: f.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Full commit message body for `hash` (`git show -s --format=%B`).
fn git_commit_message(root: &Path, hash: &str) -> String {
    Command::new("git")
        .args(["show", "-s", "--format=%B", hash])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim_end().to_string())
        .unwrap_or_default()
}

/// Files changed in `range` (two-dot, e.g. `origin/feat..HEAD`), as
/// (absolute path, status).
fn git_range_files(root: &Path, range: &str) -> Vec<(PathBuf, GitState)> {
    let out = Command::new("git")
        .args(["diff", "--name-status", "-M", range])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut files = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else { continue };
        let path = parts.last().unwrap_or("").trim();
        if path.is_empty() {
            continue;
        }
        let state = match status.chars().next() {
            Some('A') => GitState::New,
            Some('D') => GitState::Deleted,
            _ => GitState::Modified,
        };
        files.push((root.join(path), state));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

/// Local + remote branches, most-recently-committed first, deduped by name
/// (remote-only branches are checked out via git's tracking DWIM).
fn git_branches(root: &Path) -> Vec<String> {
    let Some(text) = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "--sort=-committerdate",
            "refs/heads",
            "refs/remotes",
        ])
        .current_dir(root)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
    else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        // locals shown bare ("next"), remotes shown full ("origin/next") so both
        // appear and can be merged/checked-out independently
        let name = if let Some(b) = line.strip_prefix("refs/heads/") {
            b.to_string()
        } else if let Some(r) = line.strip_prefix("refs/remotes/") {
            if r.ends_with("/HEAD") {
                continue; // skip origin/HEAD pointer
            }
            r.to_string()
        } else {
            continue;
        };
        if !name.is_empty() && seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

/// Names of local branches only (refs/heads), for distinguishing local vs
/// remote-tracking entries in the branch popup.
fn git_local_branches(root: &Path) -> HashSet<String> {
    Command::new("git")
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
enum GitAction {
    Update,
    Commit,
    Push,
    Pr,
    CreatePr,
    NewBranch,
}

#[derive(Clone)]
enum GitItem {
    Action(GitAction, &'static str, &'static str), // action, label, icon
    Branch(String),
    /// Typing a bare number offers that PR: checking it out asks `gh` for the
    /// head branch, so you never have to look the name up first.
    Pr(u64),
}

/// Per-branch actions, shown in a submenu when you press enter on a branch in
/// the git popup.
#[derive(Clone, Copy, PartialEq)]
enum BranchAction {
    Checkout,
    Merge,
    Delete,
}


/// Commands available in the command palette (cmd+shift+p).
#[derive(Clone, Copy, PartialEq)]
enum Cmd {
    WipCommit,
    WipPush,
    OpenPr,
    CreatePr,
    Build,
    InstallBuild,
    BuildPreview,
    GitAdd,
    Pull,
    Fetch,
    CheckoutNext,
    NewBranch,
    Commit,
    ToggleTerminal,
    FindInFiles,
    GoToFile,
    GoToLine,
    GitPopup,
    ShowDiff,
    MyPrs,
    ReleasePrs,
    CopyBranch,
    CopyRenameSession,
    ProcessManager,
    ToggleReadOnly,
    ResolveConflicts,
    OpenAdmin,
    SetWorkspaceTitle,
    ResetWorkspace,
    ResetWorkspaceClose,
    ResetWorkspacePause,
    TogglePause,
    AddComment,
    ShowComments,
    SendComments,
    OpenRepository,
    InstallUsageBars,
}

/// (command, label, icon glyph, shortcut hint)
const PALETTE: &[(Cmd, &str, &str, &str)] = &[
    (Cmd::WipCommit, "WIP Commit", IC_COMMIT, ""),
    (Cmd::WipPush, "WIP Commit & Push", IC_PUSH, ""),
    (Cmd::OpenPr, "Open Pull Request", IC_PR, ""),
    (Cmd::CreatePr, "Create Pull Request", IC_PR, ""),
    (Cmd::MyPrs, "My PRs", IC_PR, ""),
    (Cmd::ReleasePrs, "Release PRs", IC_PR, ""),
    (Cmd::CheckoutNext, "Checkout / Pull Next", IC_HOME, ""),
    (Cmd::Build, "Build", IC_TOOLS, "⌘⇧B"),
    (Cmd::InstallBuild, "Install and Build", IC_TOOLS, ""),
    (Cmd::BuildPreview, "What Needs Rebuilding", IC_TOOLS, ""),
    (Cmd::GitAdd, "Git Add", IC_ADD, ""),
    (Cmd::Pull, "Pull", IC_HOME, ""),
    (Cmd::Fetch, "Fetch", IC_HOME, ""),
    (Cmd::NewBranch, "New Branch", IC_BRANCH, ""),
    (Cmd::Commit, "Commit…", IC_COMMIT, ""),
    (Cmd::GitPopup, "Branches & Git Actions", IC_BRANCH, "⌥B"),
    (Cmd::ShowDiff, "Show Diff", IC_PR, "⌘D"),
    (Cmd::ToggleTerminal, "Toggle Terminal", IC_TERMINAL, "⌥F12"),
    (Cmd::FindInFiles, "Find in Files", IC_SEARCH, "⌘⇧F"),
    (Cmd::GoToFile, "Go to File", IC_FILES, "⌘⇧O"),
    (Cmd::GoToLine, "Go to Line", IC_HOME, "⌘L"),
    (Cmd::CopyBranch, "Copy Branch Name", IC_BRANCH, ""),
    (Cmd::CopyRenameSession, "Copy Claude /rename Command", IC_COPY, ""),
    (Cmd::ProcessManager, "Process Manager", IC_TOOLS, ""),
    (Cmd::ToggleReadOnly, "Toggle Read-Only / Edit Mode", IC_HOME, ""),
    (Cmd::ResolveConflicts, "Resolve Merge Conflicts", IC_BRANCH, ""),
    (Cmd::OpenAdmin, "Open Admin", IC_BROWSER, ""),
    (Cmd::SetWorkspaceTitle, "Set Workspace Title…", IC_HOME, ""),
    (Cmd::ResetWorkspace, "Reset Workspace", IC_HOME, ""),
    (Cmd::ResetWorkspaceClose, "Reset Workspace & Close", IC_HOME, ""),
    (Cmd::ResetWorkspacePause, "Reset Workspace & Pause", IC_HOME, ""),
    (Cmd::TogglePause, "Pause / Resume Workspace  (⌥Tab skips paused)", IC_HOME, ""),
    (Cmd::AddComment, "Comment on This Line…", IC_COMMIT, "⌥C"),
    (Cmd::ShowComments, "Review Comments…", IC_COMMIT, "⌥⇧C"),
    (Cmd::SendComments, "Send Comments to Agent", IC_PUSH, "⌥↩ / ⌘F12"),
    (Cmd::OpenRepository, "Open Repository or Project…  (clones a GitHub URL into ~/dev)", IC_FILES, "⌘O"),
    (Cmd::InstallUsageBars, "Install Plan Usage Bars (statusline shim)", IC_GRAPH, ""),
];

/// A running process row for the Process Manager dialog.
#[derive(Clone)]
struct Proc {
    pid: u32,
    ppid: u32,      // parent pid (to find tide's descendant tree)
    name: String,   // basename of the executable
    comm: String,   // full command path (used for filtering)
    rss_kb: u64,    // resident memory in KB
    user: String,
}

/// List running processes via `ps`, sorted by memory (largest first).
fn list_processes() -> Vec<Proc> {
    let Some(out) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,user=,comm="])
        .output()
        .ok()
        .filter(|o| o.status.success())
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut procs = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(rss), Some(user)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let comm = parts.collect::<Vec<_>>().join(" ");
        let (Ok(pid), Ok(ppid), Ok(rss_kb)) =
            (pid.parse::<u32>(), ppid.parse::<u32>(), rss.parse::<u64>())
        else {
            continue;
        };
        if comm.is_empty() {
            continue;
        }
        let name = comm.rsplit('/').next().unwrap_or(&comm).to_string();
        procs.push(Proc { pid, ppid, name, comm, rss_kb, user: user.to_string() });
    }
    procs.sort_by(|a, b| b.rss_kb.cmp(&a.rss_kb));
    procs
}

/// Human-readable memory size from KB (MB, or GB above 1 GB).
fn fmt_mem(kb: u64) -> String {
    let mb = kb as f64 / 1024.0;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else {
        format!("{:.1} MB", mb)
    }
}

/// The [start, end) char range of the word at `col` in `line` (alphanumeric +
/// underscore run); an empty range if `col` isn't on a word char.
fn word_range(line: &str, col: usize) -> (usize, usize) {
    let ch: Vec<char> = line.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if col >= ch.len() || !is_word(ch[col]) {
        return (col, col);
    }
    let mut s = col;
    while s > 0 && is_word(ch[s - 1]) {
        s -= 1;
    }
    let mut e = col;
    while e < ch.len() && is_word(ch[e]) {
        e += 1;
    }
    (s, e)
}

/// Split a finder query into (path, line, col), peeling a trailing `:line` or
/// `:line:col` so a pasted "src/x.ts:45" still matches the file and jumps there.
fn split_finder_query(q: &str) -> (&str, Option<usize>, Option<usize>) {
    let mut parts = q.splitn(3, ':');
    let path = parts.next().unwrap_or("");
    let line = parts.next().and_then(|s| s.trim().parse::<usize>().ok());
    let col = parts.next().and_then(|s| s.trim().parse::<usize>().ok());
    (path, line, col)
}

/// Current git branch name (empty if not a repo).
/// Files with unresolved merge conflicts (git status filter U).
fn conflicted_files(root: &Path) -> Vec<PathBuf> {
    Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| root.join(l.trim()))
                .collect()
        })
        .unwrap_or_default()
}

/// Contents of a conflicted file's merge stage (1=base, 2=ours, 3=theirs).
fn git_stage(root: &Path, rel: &str, stage: u8) -> String {
    Command::new("git")
        .args(["show", &format!(":{stage}:{rel}")])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// The branch/ref being merged in, for display. Parses MERGE_MSG's quoted ref
/// ("Merge remote-tracking branch 'origin/next' …"); falls back to MERGE_HEAD.
fn merge_source(root: &Path) -> String {
    let git_dir = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(root)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|d| {
            let p = PathBuf::from(&d);
            if p.is_absolute() { p } else { root.join(p) }
        });
    if let Some(dir) = git_dir {
        if let Ok(msg) = std::fs::read_to_string(dir.join("MERGE_MSG")) {
            if let Some(first) = msg.lines().next() {
                if let (Some(a), Some(b)) = (first.find('\''), first.rfind('\'')) {
                    if b > a + 1 {
                        return first[a + 1..b].to_string();
                    }
                }
            }
        }
    }
    "the other branch".to_string()
}

fn git_branch(root: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Best-effort parent branch `branch` was created from, read from the reflog's
/// creation entry ("branch: Created from <ref>"). None if the reflog was pruned,
/// the branch wasn't created locally, or it forked from a bare commit.
fn git_branch_parent(root: &Path, branch: &str) -> Option<String> {
    if branch.is_empty() {
        return None;
    }
    // reflog subjects, oldest last; the creation entry is the last one
    let out = Command::new("git")
        .args(["reflog", "show", "--format=%gs", branch])
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let parent = text
        .lines()
        .rev()
        .find_map(|l| l.split_once("Created from ").map(|(_, p)| p.trim().to_string()))
        .filter(|p| !p.is_empty() && p != branch && p != "HEAD")?;
    // tidy display: drop a leading remote prefix (origin/next -> next)
    Some(parent.strip_prefix("origin/").unwrap_or(&parent).to_string())
}

/// The open PR for `branch`, as (number, url), via `gh`. None if there's no PR
/// (or `gh` is unavailable). Runs on a background thread, so the network call
/// never blocks the UI.
/// CI/checks rollup status for a PR, shown as a colored dot by the PR link.
#[derive(Clone, Copy, PartialEq)]
enum PrStatus {
    Passing, // all checks green
    Pending, // something still running / queued
    Failing, // at least one check failed
    None,    // no checks reported
}

impl PrStatus {
    fn color(self) -> u32 {
        match self {
            PrStatus::Passing => 0x6aaf6a, // green
            PrStatus::Pending => 0xd9a23b, // orange
            PrStatus::Failing => 0xe04141, // red
            PrStatus::None => MUTED,
        }
    }
}

/// A PR's lifecycle state, shown as a small pill next to its number.
#[derive(Clone, Copy, PartialEq)]
enum PrState {
    Draft,
    Open,
    Merged,
    Closed,
}

impl PrState {
    fn label(self) -> &'static str {
        match self {
            PrState::Draft => "draft",
            PrState::Open => "open",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
        }
    }

    fn color(self) -> u32 {
        match self {
            PrState::Draft => MUTED,
            PrState::Open => 0x6aaf6a,   // green
            PrState::Merged => 0xa371f7, // purple (GitHub's merged color)
            PrState::Closed => 0xe04141, // red
        }
    }

    /// Merged/closed win over `isDraft` (a closed draft reads as closed).
    fn from_json(v: &serde_json::Value) -> PrState {
        match v.get("state").and_then(|s| s.as_str()).unwrap_or("OPEN").to_uppercase().as_str() {
            "MERGED" => PrState::Merged,
            "CLOSED" => PrState::Closed,
            _ if v.get("isDraft").and_then(|d| d.as_bool()).unwrap_or(false) => PrState::Draft,
            _ => PrState::Open,
        }
    }
}

/// The `draft` / `merged` / `closed` pill next to a PR number.
fn pr_state_pill(state: PrState, size: f32) -> Div {
    div()
        .px_1()
        .rounded_sm()
        .border_1()
        .border_color(rgb(state.color()))
        .text_size(px(size))
        .text_color(rgb(state.color()))
        .child(state.label())
}

/// Aggregate a PR's `statusCheckRollup` array into a single status.
fn rollup_status(v: &serde_json::Value) -> PrStatus {
    let Some(arr) = v.get("statusCheckRollup").and_then(|x| x.as_array()).filter(|a| !a.is_empty())
    else {
        return PrStatus::None;
    };
    let mut pending = false;
    for c in arr {
        // CheckRun reports `status` (+ `conclusion`); StatusContext reports `state`
        if let Some(status) = c.get("status").and_then(|s| s.as_str()) {
            if status != "COMPLETED" {
                pending = true;
                continue;
            }
        }
        let outcome = c
            .get("conclusion")
            .and_then(|s| s.as_str())
            .or_else(|| c.get("state").and_then(|s| s.as_str()))
            .unwrap_or("");
        match outcome.to_uppercase().as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "" | "PENDING" | "EXPECTED" | "IN_PROGRESS" | "QUEUED" | "WAITING" => pending = true,
            _ => return PrStatus::Failing, // FAILURE, ERROR, CANCELLED, TIMED_OUT, ACTION_REQUIRED…
        }
    }
    if pending {
        PrStatus::Pending
    } else {
        PrStatus::Passing
    }
}

fn fetch_pr_link(root: &Path, branch: &str) -> Option<(u64, String, PrStatus, String, String, PrState)> {
    if branch.is_empty() {
        return None;
    }
    let out = Command::new("gh")
        .args([
            "pr",
            "view",
            branch,
            "--json",
            "number,url,statusCheckRollup,baseRefName,headRefName,state,isDraft",
        ])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let number = v.get("number")?.as_u64()?;
    let url = v.get("url")?.as_str()?.to_string();
    let base = v.get("baseRefName").and_then(|b| b.as_str()).unwrap_or("").to_string();
    let head = v.get("headRefName").and_then(|b| b.as_str()).unwrap_or("").to_string();
    Some((number, url, rollup_status(&v), base, head, PrState::from_json(&v)))
}

/// Open milestone titles for the repo, newest first (via `gh api`). Empty when
/// gh isn't available, there are none, or the repo isn't on GitHub.
fn gh_milestones(root: &Path) -> Vec<String> {
    let out = Command::new("gh")
        .args([
            "api",
            "repos/{owner}/{repo}/milestones",
            "--method",
            "GET",
            "-f",
            "state=open",
            "-f",
            "sort=due_on",
            "--jq",
            ".[].title",
        ])
        .current_dir(root)
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// The `https://github.com/org/repo` base URL for the repo's `origin` remote,
/// normalized from either SSH or HTTPS remote forms. None if not a GitHub repo.
fn github_base_url(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let url = url.strip_suffix(".git").unwrap_or(&url);
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        Some(format!("https://github.com/{}", rest))
    } else if let Some(rest) = url.strip_prefix("ssh://git@github.com/") {
        Some(format!("https://github.com/{}", rest))
    } else if url.starts_with("https://github.com/") || url.starts_with("http://github.com/") {
        Some(url.to_string())
    } else {
        None
    }
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    depth: usize,
    ignored: bool, // git-ignored (or under an ignored dir) → shown dimmed
}

/// One terminal tab's pane layout: leaves are shells, splits are equal-share
/// groups (⌘D side by side, ⌘⇧D stacked). A split always *nests* around the
/// pane being split, so the pane you split is the one cut in half — repeated
/// splits never shrink the untouched panes.
enum TermPane {
    Leaf(Entity<Terminal>),
    /// `shares` are flex weights, one per kid — dragging a divider moves weight
    /// between the two panes it separates.
    Split { horizontal: bool, kids: Vec<TermPane>, shares: Vec<f32> },
}

impl TermPane {
    fn leaves(&self, out: &mut Vec<Entity<Terminal>>) {
        match self {
            TermPane::Leaf(t) => out.push(t.clone()),
            TermPane::Split { kids, .. } => kids.iter().for_each(|k| k.leaves(out)),
        }
    }

    fn find(&self, id: EntityId) -> Option<&Entity<Terminal>> {
        match self {
            TermPane::Leaf(t) => (t.entity_id() == id).then_some(t),
            TermPane::Split { kids, .. } => kids.iter().find_map(|k| k.find(id)),
        }
    }

    /// Top-left-most pane — the fallback focus target.
    fn first(&self) -> &Entity<Terminal> {
        match self {
            TermPane::Leaf(t) => t,
            TermPane::Split { kids, .. } => kids[0].first(),
        }
    }

    /// Replace the leaf holding `id` with a 50/50 split of it and `new`.
    fn split(&mut self, id: EntityId, horizontal: bool, new: &Entity<Terminal>) -> bool {
        match self {
            TermPane::Leaf(t) => {
                if t.entity_id() != id {
                    return false;
                }
                let old = TermPane::Leaf(t.clone());
                *self = TermPane::Split {
                    horizontal,
                    kids: vec![old, TermPane::Leaf(new.clone())],
                    shares: vec![0.5, 0.5],
                };
                true
            }
            TermPane::Split { kids, .. } => kids.iter_mut().any(|k| k.split(id, horizontal, new)),
        }
    }

    /// Index path from this node down to the leaf holding `id` (root-first).
    fn path_to(&self, id: EntityId) -> Option<Vec<usize>> {
        match self {
            TermPane::Leaf(t) => (t.entity_id() == id).then(Vec::new),
            TermPane::Split { kids, .. } => kids.iter().enumerate().find_map(|(i, k)| {
                let mut p = k.path_to(id)?;
                p.insert(0, i);
                Some(p)
            }),
        }
    }

    fn at(&self, path: &[usize]) -> Option<&TermPane> {
        match path.split_first() {
            None => Some(self),
            Some((i, rest)) => match self {
                TermPane::Leaf(_) => None,
                TermPane::Split { kids, .. } => kids.get(*i)?.at(rest),
            },
        }
    }

    fn at_mut(&mut self, path: &[usize]) -> Option<&mut TermPane> {
        match path.split_first() {
            None => Some(self),
            Some((i, rest)) => match self {
                TermPane::Leaf(_) => None,
                TermPane::Split { kids, .. } => kids.get_mut(*i)?.at_mut(rest),
            },
        }
    }

    /// Leaf on one edge of this subtree: `last` picks the bottom/right-most,
    /// else the top/left-most. Used to land on the pane nearest the one we left.
    fn edge_leaf(&self, last: bool) -> &Entity<Terminal> {
        match self {
            TermPane::Leaf(t) => t,
            TermPane::Split { kids, .. } => {
                let k = if last { kids.last() } else { kids.first() };
                k.expect("split always has kids").edge_leaf(last)
            }
        }
    }

    /// Drop the pane holding `id`. Returns true when *this* node is that pane and
    /// the caller should drop it; a split left with one kid collapses into it.
    fn remove(&mut self, id: EntityId) -> bool {
        match self {
            TermPane::Leaf(t) => t.entity_id() == id,
            TermPane::Split { kids, shares, .. } => {
                let mut i = 0;
                while i < kids.len() {
                    if kids[i].remove(id) {
                        kids.remove(i);
                        shares.remove(i);
                    } else {
                        i += 1;
                    }
                }
                if kids.len() == 1 {
                    *self = kids.pop().unwrap();
                }
                false
            }
        }
    }
}

/// Conventional-commit-style types a workspace title can carry: (key, emoji,
/// label). The emoji becomes the title's prefix, so the topbar shows what kind
/// of work the workspace is for at a glance.
const TITLE_TYPES: &[(&str, &str, &str)] = &[
    ("feat", "✨", "New feature"),
    ("fix", "🐛", "Bug fix"),
    ("docs", "📝", "Documentation"),
    ("style", "🎨", "Code style/formatting"),
    ("refactor", "♻️", "Code refactoring"),
    ("perf", "⚡", "Performance improvements"),
    ("test", "🧪", "Testing"),
    ("build", "📦", "Build system"),
    ("ci", "⚙️", "CI/CD"),
    ("chore", "🧹", "Other changes"),
    ("bip", "📣", "Build in public"),
    ("client", "💼", "Client work"),
    ("release", "🚀", "Release tasks"),
];

/// Split a stored title back into (type index, description) by its emoji prefix,
/// so reopening the dialog shows the type that was picked last time.
fn split_title(title: &str) -> (Option<usize>, String) {
    for (i, (_, emoji, _)) in TITLE_TYPES.iter().enumerate() {
        if let Some(rest) = title.strip_prefix(emoji) {
            return (Some(i), rest.trim().to_string());
        }
    }
    (None, title.to_string())
}

/// A title an agent proposed, given the same type prefix a human picks in the
/// title dialog: `fix: sql date bug` → `🐛 sql date bug`. Conventional-commit
/// scopes (`fix(api):`) are accepted and dropped. A title that already carries
/// an emoji, or whose prefix isn't one of ours, is left exactly as sent.
fn typed_title(proposed: &str) -> String {
    let t = proposed.trim();
    if split_title(t).0.is_some() {
        return t.to_string();
    }
    let Some((head, rest)) = t.split_once(':') else { return t.to_string() };
    let key = head.trim().to_ascii_lowercase();
    let key = key.split('(').next().unwrap_or(&key).trim();
    let Some((_, emoji, _)) = TITLE_TYPES.iter().find(|(k, _, _)| *k == key) else {
        return t.to_string();
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return t.to_string();
    }
    format!("{emoji} {rest}")
}

/// The workspace number in the topbar and the quick-jump keys in the project
/// switcher: a bare number, fixed-width so a column of them stays aligned.
fn key_cap(label: impl Into<SharedString>, dim: bool) -> Div {
    div()
        .flex()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .min_w(px(14.))
        .h(px(16.))
        .text_size(px(11.))
        .text_color(rgb(if dim { MUTED } else { TEXT }))
        .child(label.into())
}

/// Solid "FREE" badge — the loud form of the idle state. "Idle" would suggest
/// someone taking a break; a reset workspace has no task and no one on it, so
/// the word that matters is that it's available to claim.
fn idle_badge(size: f32) -> Div {
    div()
        .flex_shrink_0()
        .px_2()
        .py(px(1.))
        .rounded_md()
        .bg(rgb(agent_color(AgentState::Idle)))
        .text_size(px(size))
        .text_color(rgb(BG))
        .child("FREE")
}

/// Colour an agent state shows up as (pane border, chip, tab dot).
fn agent_color(s: AgentState) -> u32 {
    match s {
        // cyan: not green (that's "checks passing" / "admin up" / "done"), not
        // amber or blue, and not the purple that marks review comments
        AgentState::Idle => 0x56c8d8,
        AgentState::Working => ACCENT,
        AgentState::Blocked => 0xe0af68, // amber: it wants something from you
        AgentState::Done => GIT_NEW,
        AgentState::Failed => GIT_DELETED,
    }
}

fn agent_label(s: AgentState) -> &'static str {
    match s {
        AgentState::Idle => "free",
        AgentState::Working => "working",
        AgentState::Blocked => "needs you",
        AgentState::Done => "done",
        AgentState::Failed => "failed",
    }
}

/// The review-comment marker: a message glyph from the icon font, optionally
/// followed by a count. Digits aren't in the icon font, so they're a sibling.
fn comment_marker(n: usize, size: f32) -> Div {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(1.))
        .flex_shrink_0()
        .text_color(rgb(COMMENT_MARK))
        .child(div().font_family(ICON_FONT).text_size(px(size)).child(IC_COMMENT));
    if n > 0 {
        row = row.child(div().text_size(px(size - 1.0)).child(n.to_string()));
    }
    row
}

/// One button in a comment box's footer. `primary` fills it (the commit action);
/// the rest are outlined.
fn rc_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
    danger: bool,
) -> gpui::Stateful<gpui::Div> {
    let fg = if primary { BG } else if danger { MUTED } else { TEXT };
    div()
        .id(id)
        .px_2()
        .py(px(2.))
        .rounded_md()
        .cursor_pointer()
        .text_size(px(11.))
        .border_1()
        .border_color(rgb(if primary { COMMENT_MARK } else { BORDER }))
        .when(primary, |d| d.bg(rgb(COMMENT_MARK)))
        .text_color(rgb(fg))
        .when(!primary && danger, |d| d.hover(|s| s.text_color(rgb(GIT_DELETED)).border_color(rgb(GIT_DELETED))))
        .when(!primary && !danger, |d| d.hover(|s| s.bg(rgb(HOVER))))
        .when(primary, |d| d.hover(|s| s.opacity(0.85)))
        .child(label)
}

/// Height of an inline note row in the diff window (the editor shows notes in
/// the right margin instead, so it reserves no row for them).
const RC_NOTE_H: f32 = 20.0;
/// Chrome around the comment input: header, padding, borders, button row.
const RC_BOX_CHROME: f32 = 74.0;
const RC_LINE_H: f32 = 16.0;
/// Inset from the line's left edge, so the box doesn't start hard against the
/// gutter.
const RC_BOX_PAD: f32 = 6.0;
/// Breathing room above and below the box: the code it pushes apart shouldn't
/// look glued to its border.
const RC_BOX_VPAD: f32 = 12.0;
/// The input always looks like a text area, never a one-line field.
const RC_MIN_LINES: usize = 3;
const RC_MAX_LINES: usize = 20;

/// Visible row count of a comment body, wrapping included, clamped to the box's
/// range. The box is ~90 chars wide at 12px, the same estimate the thread area
/// uses — a hard newline count alone under-measures a wrapped line and the last
/// row gets clipped.
fn rc_lines(text: &str) -> usize {
    let rows: usize = text.split('\n').map(|l| 1 + l.chars().count() / 90).sum();
    rows.clamp(RC_MIN_LINES, RC_MAX_LINES)
}

/// Rows the thread area takes: one per reply line, capped so a long answer
/// scrolls inside the box instead of pushing the file apart.
fn rc_thread_rows(turns: &[ReviewReply]) -> usize {
    let lines: usize = turns
        .iter()
        .map(|r| {
            // rough wrap: the box is ~80 chars wide at 12px, plus a spacer row
            r.text.lines().map(|l| 1 + l.chars().count() / 80).sum::<usize>().max(1) + 1
        })
        .sum();
    lines.min(10)
}

/// Height of the comment box itself (excludes the padding around it).
fn rc_box_h(text: &str, replies: &[ReviewReply]) -> f32 {
    RC_BOX_CHROME + rc_lines(text) as f32 * RC_LINE_H + rc_thread_rows(replies) as f32 * RC_LINE_H
}

/// Pixels to shift the body up so the caret's line stays visible once the note is
/// taller than the box's cap — without this, typing past 20 lines vanishes.
fn rc_scroll_off(f: &Field) -> f32 {
    let (_, end) = clamp_span_str(&f.text, 0, f.caret);
    let caret_line = f.text[..end].split('\n').count();
    let visible = rc_lines(&f.text);
    (caret_line.saturating_sub(visible)) as f32 * RC_LINE_H
}

/// Total space the editor has to reserve for the box: the box plus its padding.
fn rc_gap_h(text: &str, replies: &[ReviewReply]) -> f32 {
    rc_box_h(text, replies) + RC_BOX_VPAD * 2.0
}

/// A repo reference typed into the Open Project dialog: a GitHub URL, an SSH
/// remote, or `owner/repo`. Returns `(clone target, folder name)`.
fn parse_repo_ref(input: &str) -> Option<(String, String)> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() || raw.contains(char::is_whitespace) {
        return None;
    }
    // git@host:owner/repo(.git)
    if let Some((_, path)) = raw.split_once(':').filter(|_| raw.starts_with("git@")) {
        let name = path.rsplit('/').next()?.trim_end_matches(".git");
        return (!name.is_empty()).then(|| (raw.to_string(), name.to_string()));
    }
    if let Some(rest) = raw.strip_prefix("https://").or_else(|| raw.strip_prefix("http://")) {
        let mut parts = rest.split('/');
        let host = parts.next()?;
        let owner = parts.next()?;
        // a URL deeper than the repo (/tree/main/src/…) still names the repo
        let repo = parts.next()?.trim_end_matches(".git");
        if host.is_empty() || owner.is_empty() || repo.is_empty() {
            return None;
        }
        return Some((format!("https://{host}/{owner}/{repo}.git"), repo.to_string()));
    }
    // owner/repo — only when it isn't a path on this machine
    let (owner, repo) = raw.split_once('/')?;
    let looks_like_path = raw.starts_with('~')
        || raw.starts_with('.')
        || repo.contains('/')
        || PathBuf::from(raw).exists();
    let ok = !looks_like_path
        && !owner.is_empty()
        && !repo.is_empty()
        && raw.chars().all(|c| c.is_alphanumeric() || "._-/".contains(c));
    ok.then(|| (format!("https://github.com/{owner}/{repo}.git"), repo.trim_end_matches(".git").to_string()))
}

/// Where a cloned repo lands: alongside the projects already there.
fn clone_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("dev")
}

/// A file reference inside agent prose: `Icon.tsx:14`, `Icon.tsx:14,25`, or
/// `packages/api-aco/src/x.ts:31`. Returns the byte range in `text`, the path as
/// written, and the first line number.
#[derive(Clone, Debug, PartialEq)]
struct FileRef {
    range: std::ops::Range<usize>,
    path: String,
    line: usize,
}

/// What a link in the transcript points at.
#[derive(Clone)]
enum LinkTarget {
    /// A file in the project, at a line (0 = no line given).
    File(String, usize),
    Url(String),
}

/// Scan `text` for `http(s)://…` runs. A URL ends at whitespace, and trailing
/// sentence punctuation is left out of it — `see https://x.com/a.` links `a`,
/// not `a.` — but a closing bracket is kept when the URL opened one.
fn url_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        let rest = &text[i..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            i += 1;
            while i < b.len() && !text.is_char_boundary(i) {
                i += 1;
            }
            continue;
        }
        let mut end = i + rest.find(char::is_whitespace).unwrap_or(rest.len());
        // `(https://x)` and `<https://x>` are wrappers, not part of the address
        while end > i {
            let last = text[i..end].chars().next_back().unwrap_or(' ');
            let unbalanced = last == ')' && !text[i..end].contains('(');
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '"' | '\'' | '>' | ']' | '}')
                || unbalanced
            {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        // "https://" on its own isn't a link
        if text[i..end].len() > "https://".len() {
            out.push(i..end);
        }
        i = end.max(i + 1);
    }
    out
}

/// Scan `text` for file references. Deliberately narrow: something that looks
/// like a filename with an extension, optionally followed by `:line`. Prose like
/// "3 files" or "10:30" never matches, since a path needs a dot-extension.
fn file_refs(text: &str) -> Vec<FileRef> {
    const EXTS: &[&str] = &[
        "ts", "tsx", "js", "jsx", "mjs", "cjs", "rs", "toml", "json", "md", "css", "scss", "html",
        "yml", "yaml", "sh", "py", "go", "java", "sql", "txt", "lock", "env",
    ];
    let b = text.as_bytes();
    let is_path_char = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'/' | b'@');
    let mut out: Vec<FileRef> = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if !is_path_char(b[i]) || (i > 0 && is_path_char(b[i - 1])) {
            i += 1;
            continue;
        }
        // the run of path-ish characters starting here
        let start = i;
        let mut end = i;
        while end < b.len() && is_path_char(b[end]) {
            end += 1;
        }
        i = end;
        let word = &text[start..end];
        let word = word.trim_end_matches('.'); // sentence punctuation
        let end = start + word.len();
        let Some(dot) = word.rfind('.') else { continue };
        let ext = &word[dot + 1..];
        if !EXTS.contains(&ext) || word.starts_with('.') && !word[1..].contains('.') {
            continue;
        }
        // optional ":line" (and a second number after a comma, which we ignore)
        let mut ref_end = end;
        let mut line = 0usize;
        if text[end..].starts_with(':') {
            let digits: String = text[end + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if !digits.is_empty() {
                line = digits.parse().unwrap_or(0);
                ref_end = end + 1 + digits.len();
                // `Icon.tsx:14,25` — a second line number belongs to the same ref
                if text[ref_end..].starts_with(',') {
                    let more: String =
                        text[ref_end + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
                    if !more.is_empty() {
                        ref_end += 1 + more.len();
                    }
                }
            }
        }
        out.push(FileRef { range: start..ref_end, path: word.to_string(), line });
    }
    out
}

/// Where a project's review comments live between runs. Keyed by project path,
/// like the workspace titles.
fn review_path(root: &Path) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        PathBuf::from(home)
            .join(".config/tide/comments")
            .join(format!("{}.json", usage::project_slug(root))),
    )
}

/// Serialise comments with **repo-relative** paths, so a moved or re-cloned
/// checkout still matches them up.
fn review_to_json(root: &Path, comments: &[ReviewComment]) -> String {
    let items: Vec<serde_json::Value> = comments
        .iter()
        .map(|c| {
            let rel = c.path.strip_prefix(root).unwrap_or(&c.path);
            serde_json::json!({
                "id": c.id,
                "path": rel.to_string_lossy(),
                "line": c.line,
                "end": c.end,
                "snippet": c.snippet,
                "text": c.text,
                "draft": c.draft,
                "sent": c.sent,
                "resolved": c.resolved,
                "replies": c
                    .replies
                    .iter()
                    .map(|r| serde_json::json!({ "who": r.who, "text": r.text }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into())
}

fn review_from_json(root: &Path, body: &str) -> Vec<ReviewComment> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(body) else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            let rel = v.get("path")?.as_str()?;
            let line = v.get("line")?.as_u64()? as usize;
            let text = v.get("text")?.as_str()?.to_string();
            if text.trim().is_empty() {
                return None;
            }
            Some(ReviewComment {
                id: v
                    .get("id")
                    .and_then(|i| i.as_str())
                    .map(|i| i.to_string())
                    .unwrap_or_else(random_id),
                path: root.join(rel),
                line,
                end: v.get("end").and_then(|e| e.as_u64()).unwrap_or(line as u64) as usize,
                snippet: v.get("snippet").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                text,
                draft: v.get("draft").and_then(|d| d.as_bool()).unwrap_or(false),
                sent: v.get("sent").and_then(|d| d.as_bool()).unwrap_or(false),
                resolved: v.get("resolved").and_then(|d| d.as_bool()).unwrap_or(false),
                replies: v
                    .get("replies")
                    .and_then(|r| r.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|r| {
                                Some(ReviewReply {
                                    who: r.get("who")?.as_str()?.to_string(),
                                    text: r.get("text")?.as_str()?.to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect()
}

fn load_review(root: &Path) -> Vec<ReviewComment> {
    review_path(root)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|body| review_from_json(root, &body))
        .unwrap_or_default()
}

/// Format `(reference, snippet, note)` triples as the message typed into the
/// agent's pane: numbered, with the code quoted so the agent sees what was meant
/// without having to guess which lines moved.
fn format_review(items: &[ReviewItem], note: &str) -> String {
    let mut out = String::new();
    // a general note reads first: it's the frame the line comments sit in
    if !note.trim().is_empty() {
        out.push_str("Review from TIDE.\n\n");
        for line in note.trim().lines() {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        if items.is_empty() {
            return out;
        }
        out.push_str("Comments on specific lines — please address each:\n\n");
    } else {
        out.push_str("Review comments from TIDE — please address each:\n\n");
    }
    for (i, it) in items.iter().enumerate() {
        out.push_str(&format!("{}. [{}] {}\n", i + 1, it.id, it.reference));
        for line in it.snippet.lines().take(12) {
            out.push_str(&format!("   | {}\n", line.trim_end()));
        }
        out.push_str(&format!("   -> {}\n", it.note));
        // an ongoing thread: the agent sees what was already said, so a follow-up
        // reads as a conversation instead of a fresh comment
        for (who, text) in &it.thread {
            let label = if who == "agent" { "you (agent)" } else { "me" };
            for (li, line) in text.lines().enumerate() {
                if li == 0 {
                    out.push_str(&format!("   {label}: {line}\n"));
                } else {
                    out.push_str(&format!("      {line}\n"));
                }
            }
        }
        out.push('\n');
    }
    // each comment is a thread in TIDE; answering by id puts your reply in the
    // thread instead of only in this transcript
    out.push_str(
        "When you've handled one, post a one-or-two-sentence answer back to its thread:\n         curl -s -X POST \"$TIDE_RPC/comment\" -H \"X-Tide-Token: $TIDE_TOKEN\" \\\n           -d '{\"id\":\"<id in brackets above>\",\"text\":\"<what you did>\"}'\n\nThe reply renders as markdown in TIDE, so use it where it helps: `backticks` for\nidentifiers, paths, and values, **bold** for the one thing that matters most, and\n- bullets when there's more than one point. Prose is fine for a one-liner — don't\ndress up a short answer.\n",
    );
    out
}

/// One comment as handed to the agent: its id, where it points, the code, the
/// note, and whatever has already been said in its thread.
struct ReviewItem {
    id: String,
    reference: String,
    snippet: String,
    note: String,
    thread: Vec<(String, String)>,
}

/// A note pinned to a line span of a file, to be handed to the coding agent
/// later. Held per project and never written to disk — these are meant to be
/// written, sent, and dropped inside one review pass.
/// One turn in a comment thread.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ReviewReply {
    /// "you" for a note you added, "agent" for one posted to `/comment`.
    who: String,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ReviewComment {
    /// Short id handed to the agent so its answer can find its way back here.
    id: String,
    path: PathBuf,
    /// 1-based, inclusive
    line: usize,
    end: usize,
    /// the code as it read when the note was written (quoted when sending)
    snippet: String,
    text: String,
    /// Written but never committed — kept when the file closes with the box open,
    /// so an accidental ⌘W doesn't throw the text away. Drafts are listed in the
    /// review pane but held back when the batch is sent.
    draft: bool,
    /// True once the comment has been handed to the agent; it stays in the list
    /// so replies have somewhere to land.
    sent: bool,
    /// The thread: the agent's answers, and any follow-ups you add.
    replies: Vec<ReviewReply>,
    /// Done with: hidden from the open list and from everything that counts
    /// comments, but kept, so a thread resolved too early can be reopened.
    resolved: bool,
}

/// An in-flight drag of the divider between `kids[ix]` and `kids[ix + 1]` of the
/// split at `path`. Sizes come from the panes' painted bounds, so the divider
/// tracks the cursor regardless of how deeply the split is nested.
#[derive(Clone)]
struct PaneDrag {
    path: Vec<usize>,
    ix: usize,
    horizontal: bool,
    origin: f32,     // cursor position on the split axis when the drag began
    extent: f32,     // px spanned by the two panes being resized
    total: f32,      // their combined flex weight (conserved across the drag)
    start_frac: f32, // kids[ix]'s share of `total` when the drag began
}

/// A terminal tab: its pane tree plus which pane had focus (so switching tabs
/// away and back lands on the same shell).
struct TermTab {
    panes: TermPane,
    active: EntityId,
    /// Name shown on the tab instead of "Terminal N" — set when a tab is opened
    /// for a specific job (the `admin` watch, say).
    title: Option<String>,
    /// ⌘⇧↩: show only the active pane, full dock. The tree is untouched, so
    /// unzooming restores the layout (and its dragged shares) as it was.
    zoomed: bool,
}

impl TermTab {
    fn new(t: Entity<Terminal>) -> Self {
        Self { active: t.entity_id(), panes: TermPane::Leaf(t), zoomed: false, title: None }
    }

    fn terminals(&self) -> Vec<Entity<Terminal>> {
        let mut v = Vec::new();
        self.panes.leaves(&mut v);
        v
    }

    fn active_terminal(&self) -> &Entity<Terminal> {
        self.panes.find(self.active).unwrap_or_else(|| self.panes.first())
    }
}

/// Run `git status --porcelain` in `root` and map each path to its state.
fn compute_git(root: &Path) -> HashMap<PathBuf, GitState> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .arg("-u")
        .current_dir(root)
        .output()
    else {
        return map;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if line.len() < 4 {
            continue;
        }
        let xy = &line[0..2];
        let raw = line[3..].trim();
        // rename: "old -> new"
        let file = raw.rsplit(" -> ").next().unwrap_or(raw);
        let full = root.join(file);
        // a deleted file shows 'D' in either the index (X) or worktree (Y)
        // column; classify before modified so it isn't swallowed
        let state = if xy.contains('D') {
            GitState::Deleted
        } else if xy.contains('?') || xy.contains('A') {
            GitState::New
        } else {
            GitState::Modified
        };
        map.insert(full, state);
    }
    map
}

struct Tab {
    path: PathBuf,
    editor: Entity<Editor>,
    scratch: bool, // unsaved in-memory scratch buffer (gone when closed)
    /// Markdown files can show rendered instead of raw (⌘⇧V / the crumb button).
    preview: bool,
}

struct Storm {
    root: PathBuf,
    expanded: HashSet<PathBuf>,
    entries: Vec<Entry>,
    tabs: Vec<Tab>,
    active: usize,
    tree_width: f32,
    resizing: bool,
    tree_scroll: UniformListScrollHandle,
    tree_focus: FocusHandle,
    // pending delete confirmation: the tree path awaiting yes/no
    confirm_delete: Option<PathBuf>,
    confirm_focus: FocusHandle,
    // pending revert confirmation: the path awaiting yes/no (git checkout HEAD)
    confirm_revert: Option<PathBuf>,
    term_tabs: Vec<TermTab>,
    active_term: usize,          // active tab (each tab holds its own pane tree)
    pane_drag: Option<PaneDrag>, // in-flight pane-divider drag
    // loopback channel agents in the panes report their state to, and the latest
    // state per pane (keyed by the pane terminal's EntityId as u64)
    agent_rpc: Option<rpc::Server>,
    agent_states: HashMap<u64, (AgentState, String)>,
    pane_cmds: HashMap<u64, String>, // pane entity id -> the command it's running
    // ── review comments: notes pinned to lines, sent to the agent in one go ──
    review: Vec<ReviewComment>,
    rc_open: bool,          // the "write a comment" prompt
    rc_focus: FocusHandle,
    rc_query: Field,
    rc_target: Option<(PathBuf, usize, usize)>, // path + 1-based line span
    /// True when the box is writing a *follow-up* in a thread rather than editing
    /// the original note — the default once an answer has arrived.
    rc_reply_mode: bool,
    /// The review pane is showing the resolved threads instead of the open ones.
    review_resolved_tab: bool,
    /// User-dragged height of the thread area in the comment box (`None` = size
    /// to content), and the in-flight drag: (mouse origin y, height at grab).
    /// Review-pane rows (by comment id) whose thread is shown in full. Collapsed
    /// is the default: a long back-and-forth would otherwise fill the pane.
    review_expanded: HashSet<String>,
    rc_thread_h: Option<f32>,
    /// Drag on the box's bottom edge or corner: `(origin x, origin y, width at
    /// grab, thread height at grab, corner)`. The corner drags both axes.
    rc_resize: Option<(f32, f32, f32, f32, bool)>,
    /// User-set box width (corner drag); `None` sizes to the pane.
    rc_width: Option<f32>,
    ws_idle: bool,   // persisted "nobody is on this workspace" marker
    ws_paused: bool, // persisted "skip me in ⌥Tab" marker
    show_terminal: bool,
    term_width: f32,
    resizing_term: bool,
    // read-only Run console (command output)
    run_open: bool,
    run_cmd: String,
    run_lines: Vec<String>,
    run_editor: Entity<Editor>, // read-only view of the log (selectable/copyable)
    run_running: bool,
    run_active: bool, // a command is running or just finished (toast visible)
    run_failed: bool,
    run_spin: usize,
    /// Live progress for `yarn build --json` runs; `None` for anything else.
    run_progress: Option<build::Progress>,
    /// What a build would do, as of the last check (`yarn build --preview
    /// --json`). `None` until asked; refreshed after a build finishes.
    build_preview: Option<build::Preview>,
    build_checking: bool,
    run_gen: u64,
    run_buf: Arc<Mutex<Vec<String>>>, // appended by the reader thread
    run_dirty: Arc<AtomicBool>,
    run_done: Arc<AtomicBool>,
    run_ok: Arc<AtomicBool>, // exit status (set by the reader thread)
    // transient bottom-right notification (e.g. "Reference copied")
    flash: Option<String>,
    flash_gen: u64,
    win_width: f32,
    win_height: f32,
    // editor right-click context menu, anchored at the (x, y) click position
    editor_ctx: Option<(f32, f32)>,
    editor_ctx_focus: FocusHandle,
    // dir-tree right-click menu: anchor position + the entry it targets
    tree_ctx: Option<(f32, f32)>,
    tree_ctx_path: Option<PathBuf>,
    /// Which action list the open context menu shows — the dir tree's, or the
    /// short one a file row in Changes / PR Review gets.
    tree_ctx_kind: CtxKind,
    tree_ctx_focus: FocusHandle,
    git_status: HashMap<PathBuf, GitState>,
    // pull-request review
    pr_files: Vec<(PathBuf, GitState)>,
    pr_viewed: HashSet<PathBuf>,
    /// Viewed-state changes made here that GitHub hasn't confirmed yet, as
    /// `path -> viewed`. A refresh reads the PR's state from `gh`, which lags a
    /// mutation by a moment; without these the checkbox would flip back.
    pr_viewed_pending: Arc<Mutex<HashMap<PathBuf, bool>>>,
    pr_collapsed: HashSet<PathBuf>,
    pr_base: String,
    /// Set when our file list can't be trusted: `origin/<base>` is behind the
    /// tip GitHub diffs against, so the three-dot merge-base is too far back.
    pr_stale: Option<String>,
    pr_selected: Option<PathBuf>,
    pr_focus: FocusHandle,
    pr_gen: u64,
    pr_loading: bool,
    pr_node_id: Option<String>, // PR GraphQL node id, for marking files viewed
    /// `/viewed` batches that arrived before the PR's file list was loaded; they
    /// are applied as soon as it is.
    pr_viewed_queued: Vec<rpc::Viewed>,
    pr_filter: Field,
    pr_filter_focus: FocusHandle,
    pr_view_filter: PrViewFilter,
    // flattened rows for the virtualized PR list + scroll handle
    pr_rows: Vec<CommitRow>,
    pr_shown_files: Vec<PathBuf>,
    pr_scroll: UniformListScrollHandle,
    // fuzzy file finder
    finder_open: bool,
    finder_focus: FocusHandle,
    finder_query: Field,
    finder_results: Vec<PathBuf>,
    finder_selected: usize,
    finder_gen: u64,
    // true when the query ends with '/', so results are folders, not files
    finder_dirs_mode: bool,
    all_files: Vec<PathBuf>,
    all_dirs: Vec<PathBuf>,
    // memoized `path as written -> real file` for agent-transcript references,
    // so styling a link doesn't rescan the file list on every frame
    ref_hits: HashMap<String, Option<PathBuf>>,
    // the file index is scanned lazily (the finder, or the first transcript
    // reference that needs resolving); true once that scan has been kicked off
    files_scanned: bool,
    // go-to-line dialog
    goto_open: bool,
    goto_focus: FocusHandle,
    goto_query: Field,
    // root focus so global shortcuts dispatch even with no file/tab open
    focus: FocusHandle,
    inited: bool,
    scratch_count: usize, // running number for scratch buffer names
    // chrome
    branch: String,
    branch_parent: Option<String>, // branch this one was created from (reflog, best-effort)
    // open PR for the current branch, if any: (number, url). Refetched when the
    // branch changes; `pr_link_branch` is the branch it was last queried for.
    // (number, url, checks status, base, head, lifecycle state)
    pr_link: Option<(u64, String, PrStatus, String, String, PrState)>,
    pr_link_branch: String,
    mem_mb: u64,
    read_only: bool, // when on, editors block manual edits (nav/select/copy still work)
    left_view: LeftView,
    show_left: bool,
    // branch-name prompt (for the `br` alias)
    br_open: bool,
    br_focus: FocusHandle,
    br_query: Field,
    // new-file prompt (from the tree context menu)
    nf_open: bool,
    nf_focus: FocusHandle,
    nf_query: Field,
    nf_dir: PathBuf, // directory the new file is created in
    br_make_pr: bool, // checkbox: also create a PR after the branch
    // create-PR prompt: optional milestone for `gh pr create`
    prc_open: bool,
    prc_focus: FocusHandle,
    prc_milestone: Field,
    prc_milestones: Vec<String>, // open milestone titles, fetched from gh on open
    prc_sel: usize,              // highlighted autocomplete suggestion
    // run-arbitrary-command prompt (cmd+shift+t)
    runc_open: bool,
    runc_focus: FocusHandle,
    runc_query: Field,
    // new-project dialog (cmd+shift+n): type or choose a folder path to open
    newproj_open: bool,
    newproj_focus: FocusHandle,
    newproj_path: Field,
    newproj_recents: Vec<PathBuf>, // recent projects, loaded when the dialog opens
    newproj_titles: HashMap<String, String>, // their custom titles, same load
    newproj_last_titles: HashMap<String, String>, // titles those folders used to have
    newproj_sel: usize,            // highlighted row in the filtered recents list
    // commit message
    commit_msg: Field,
    commit_focus: FocusHandle,
    // random id for the "wip: <id>" quick-commit placeholder
    wip_id: String,
    // commit-pane file filter
    commit_filter: Field,
    commit_filter_focus: FocusHandle,
    commit_view_filter: CommitFilter,
    // commit-pane file/folder list selection (for copy-reference)
    commit_selected: Option<PathBuf>,
    changes_focus: FocusHandle,
    // push dialog (cmd+shift+k)
    push_open: bool,
    push_focus: FocusHandle,
    push_branch: String,
    push_target: String,
    push_base_ref: String,
    // a push was rejected (remote ahead) → offer merge/rebase before retrying
    push_ahead: bool,
    push_commits: Vec<(String, String)>,
    push_files: Vec<(PathBuf, GitState)>,
    push_collapsed: HashSet<PathBuf>,
    push_selected: Option<PathBuf>,
    // when set, the file tree + diffs are scoped to this commit's changes;
    // None shows the whole to-be-pushed range
    push_commit_sel: Option<String>,
    push_tags: bool,
    // files checked for commit
    commit_checked: HashSet<PathBuf>,
    // collapsed nodes in the commit tree (relative dir paths; empty = root group)
    commit_collapsed: HashSet<PathBuf>,
    // tree selection (for scoping find-in-files)
    tree_selected: Option<PathBuf>,
    // shift-click multi-selection; when non-empty it's the full selected set
    tree_multi: HashSet<PathBuf>,
    // git-ignored paths (dirs collapsed), hidden from the tree; refreshed by the poll
    ignored: HashSet<PathBuf>,
    // find in files
    find_open: bool,
    find_focus: FocusHandle,
    find_query: Field,
    find_results: Vec<FindResult>,
    find_selected: usize,
    find_scope: Vec<PathBuf>,
    find_gen: u64,
    find_preview: Option<FindPreview>,
    find_scroll: ScrollHandle, // keeps the selected result row in view
    // text selection in the preview pane: (anchor_row, anchor_col, head_row, head_col)
    // in file-line/char coords; plus drag state, scroll handle and measured char width
    find_psel: Option<(usize, usize, usize, usize)>,
    find_pdragging: bool,
    find_pscroll: ScrollHandle,
    find_char_w: f32,

    // find-in-files panel position (top-left, set centered on open) + size
    // (resizable via the bottom-right grip, which anchors the top-left corner)
    find_left: f32,
    find_top: f32,
    find_w: f32,
    find_h: f32,
    find_resize: Option<ResizeEdges>, // which edges an active resize drag moves
    find_moving: bool,                // dragging the title bar to reposition
    find_move_dx: f32,                // cursor offset from top-left at move start
    find_move_dy: f32,
    // fraction of the results+preview area the results pane takes (drag divider)
    find_split: f32,
    find_split_dragging: bool,
    find_case_sensitive: bool,
    // blinking caret for chrome inputs (finder/goto/branch/find/commit)
    caret_on: bool,
    // language server (shared across editors)
    lsp: Option<Arc<Lsp>>,
    // git branches/actions popup
    gitp_open: bool,
    gitp_focus: FocusHandle,
    gitp_query: Field,
    gitp_branches: Vec<String>,
    gitp_sel: usize,
    // when Some, the per-branch action submenu is open for this branch
    gitp_action_branch: Option<String>,
    gitp_local: HashSet<String>, // local branch names (refs/heads), for gating Delete
    gitp_action_sel: usize,
    // command palette
    palette_open: bool,
    palette_focus: FocusHandle,
    palette_query: Field,
    palette_sel: usize,
    palette_results: Vec<(Cmd, String, &'static str, &'static str)>,
    palette_gen: u64,
    // merge-conflicts dialog
    mc_open: bool,
    mc_focus: FocusHandle,
    mc_files: Vec<PathBuf>,
    mc_into: String, // current branch (label)
    mc_from: String, // branch being merged in
    // process manager dialog
    proc_open: bool,
    proc_focus: FocusHandle,
    proc_filter: Field,
    proc_list: Vec<Proc>,
    proc_selected: HashSet<u32>,
    proc_anchor: Option<usize>, // shift-range anchor (index in the filtered list)
    proc_only_tide: bool,       // show only processes in tide's descendant tree
    proc_workspace_only: bool,  // narrow further to this workspace's terminals
    proc_ws_pids: Vec<u32>,     // this workspace's terminal shell pids (load-time snapshot)
    // workspace (multi-project) info, pushed down by the Workspace each render
    ws_names: Vec<String>,
    ws_branches: Vec<String>,
    /// Each workspace's rolled-up agent state, pushed down by the parent so the
    /// topbar can show the same tally the ⌘E switcher does.
    ws_states: Vec<Option<AgentState>>,
    ws_roots: Vec<PathBuf>, // roots of every open workspace (for "already open")
    ws_active: usize,
    ws_open: bool, // project-switcher dropdown expanded
    // ── native agent panel (ACP) ───────────────────────────────────────────
    agent_open: bool,                // right-side dock visible
    agent_share: f32,                // fraction of the terminal dock's height
    resizing_agent: bool,            // dragging the divider under the agent
    agent_zoomed: bool,              // ⌘⇧↩ on the agent: take the whole dock
    /// ⌘⇧↩ on a terminal pane: the mirror image — the panes take the whole dock
    /// and the agent is hidden until it's toggled back.
    term_zoomed: bool,
    acp: Option<Arc<acp::Acp>>,      // lazily spawned on first open
    acp_polling: bool,               // the drain poll loop is running
    agent_msgs: Vec<AgentMsg>,       // conversation transcript
    agent_cur: Option<usize>,        // index of the assistant text bubble being streamed
    agent_cur_thought: Option<usize>, // index of the reasoning bubble being streamed
    agent_busy: bool,                // a turn is in flight
    agent_input: Field,              // prompt input box
    agent_focus: FocusHandle,        // focus for the dock (routes typing)
    agent_ctx: Option<(u64, u64)>,   // ACP usage_update: (context used, window)
    agent_sessions: Vec<(String, String)>, // resumable sessions: (id, title)
    /// A `session/list` answer we asked for in order to resume the newest
    /// session, rather than to populate the picker.
    agent_autoresume: bool,
    /// Whether this workspace already auto-resumed once — reopening the panel
    /// shouldn't throw away the conversation you're in the middle of.
    agent_resumed: bool,
    /// The agent was started over here (reset, or ⌃N) and hasn't been prompted
    /// since. Persisted, so a restart doesn't resume what was thrown away.
    agent_fresh: bool,
    agent_picker: bool,              // the resume list is open
    /// The briefing panel: what TIDE tells the agent before your first prompt.
    agent_brief_open: bool,
    agent_title: Option<String>,     // the adapter's title for this session
    /// Rows selected in one agent message: `(message, anchor row, current row)`.
    /// Row granularity on purpose — pixel→byte mapping through gpui's text
    /// layout was unreliable (it can't address headings, fences or table cells at
    /// all), and a paragraph is the unit you actually want to ask about.
    agent_sel: Option<(usize, usize, usize)>,
    agent_sel_drag: bool,
    /// Row a press landed on, before any movement: `(message, row)`. Holding it
    /// here keeps a plain click free for the file links inside the row.
    agent_sel_armed: Option<(usize, usize)>,
    /// True once a drag has actually extended the selection — a bare click
    /// shouldn't pop the comment box open.
    agent_sel_moved: bool,
    /// Where the selection was last touched, in window coordinates: the comment
    /// box opens there instead of centred over the editor.
    agent_sel_pos: Option<(f32, f32)>,
    /// Images pasted into the composer, waiting to go with the next prompt.
    agent_imgs: Vec<PastedImage>,
    /// Numbers the `[Image #N]` tokens; never reused within a session so a
    /// dropped attachment can't renumber the ones already typed.
    agent_img_seq: usize,
    /// Long pastes, collapsed to a `[Pasted text #N +M lines]` token in the box
    /// and expanded again when the message is sent.
    agent_pastes: HashMap<usize, String>,
    agent_paste_seq: usize,
    /// A general note for the whole review — the PR-review body, not tied to a
    /// line — sent with the next batch.
    review_note: Field,
    review_note_focus: FocusHandle,
    /// Snippet for a comment whose source isn't a file (the agent's answer).
    rc_snippet: Option<String>,
    /// Spinner frame + when the running turn started, for the live "thinking" row.
    agent_spin: usize,
    agent_turn_start: Option<std::time::Instant>,
    /// Whether this session has been told how to reach TIDE (a hidden preamble
    /// on the first prompt, so the transcript stays yours).
    agent_briefed: bool,
    /// Ghost completion for the composer: the rest of the most recent message
    /// that starts with what's typed. ⇥ accepts it.
    agent_ghost: Option<String>,
    /// The "my messages" jump list in the agent header is open, and its filter.
    /// `@` completion in the composer: what's typed after the `@`, the matches,
    /// and where the `@` sits in the text.
    at_open: bool,
    at_start: usize,
    at_sel: usize,
    at_items: Vec<Mention>,
    /// `@token` → what the agent is sent in its place, for every mention picked
    /// since the composer was last cleared.
    agent_mentions: HashMap<String, String>,
    agent_jump_open: bool,
    agent_jump_query: Field,
    agent_jump_focus: FocusHandle,
    /// Prompts typed while a turn was still running, sent in order as each turn
    /// ends — the same queueing Claude Code does in the terminal.
    agent_queue: Vec<(String, Option<String>)>,
    /// Virtualized transcript state, and the item count it was last told about.
    agent_list: gpui::ListState,
    agent_list_len: usize,
    /// What the ACP panel is doing, in the same vocabulary the terminal panes
    /// report through the RPC channel — hooks can't see this agent, so the panel
    /// reports itself.
    acp_state: Option<AgentState>,
    // ── Git log panel (bottom dock) ────────────────────────────────────────
    git_open: bool,                          // dock visible
    git_height: f32,                         // dock height
    resizing_git: bool,                      // dragging the top divider
    git_focus: FocusHandle,                  // commit-list nav focus
    git_filter_focus: FocusHandle,           // filter input focus
    git_query: Field,                        // text/hash/author filter
    git_rev: String,                         // branch/rev the log is showing ("HEAD" default)
    git_commits: Vec<LogCommit>,             // loaded log (newest first)
    git_sel: Option<usize>,                  // selected commit (index into git_commits)
    git_gen: u64,                            // async-load generation guard
    git_branches_list: Vec<String>,          // branches for the left tree
    git_collapsed: HashSet<String>,          // collapsed branch-tree folders
    git_details_msg: String,                 // full message of the selected commit
    git_details_files: Vec<(PathBuf, GitState)>, // its changed files
    git_scroll: ScrollHandle,                // commit list scroll
    // ── in-pane browser (shares the right dock with the terminal) ──────────
    browser_open: bool,
    browser: Option<browser::Browser>, // embedded WebView, created on first open
    browser_url: Field,                // address bar
    browser_focus: FocusHandle,
    // wby-next admin health: None = unknown, Some(true/false) = up/down
    admin_up: Option<bool>,
    /// Pane running the `admin` watch, when it was started from the topbar. Its
    /// own terminal tab, so the build log is there when it's wanted and out of
    /// the way when it isn't.
    admin_pane: Option<u64>,
    /// Terminal pane running `wwp` (watch the branch's changed packages).
    watch_pane: Option<u64>,
    /// Package names the watch button offers, refreshed by the git poll.
    watch_pkgs: Vec<String>,
    admin_spin: usize, // spinner frame while an admin build is coming up
    /// Set when ▶ started the admin: the first time it answers, open it in the
    /// default browser. Cleared once used, so a later restart-and-recover of the
    /// same admin doesn't steal focus.
    admin_autoopen: bool,
    /// Plan usage windows, as last reported to the statusline shim, and whether
    /// the topbar chip's detail panel is open.
    limits: Option<usage::Limits>,
    limits_open: bool,
    // custom workspace title (persisted); overrides the folder name in the UI
    ws_title: Option<String>,
    title_open: bool,
    title_focus: FocusHandle,
    title_query: Field,
    title_type: Option<usize>, // index into TITLE_TYPES; None = plain title
    title_client: Field,       // "who is this for" — optional, persisted
    title_field: usize,        // 0 = title, 1 = client (⇥ moves between them)
    ws_client: Option<String>, // the saved client for this workspace
}

/// Navigation requests a project view sends up to the workspace.
enum ProjectNav {
    Switch(usize),
    /// Open the ⌘E switcher (the dialog lives on the parent).
    Switcher,
    Open,
    OpenPath(PathBuf), // open a project at a specific folder path (from the new-project dialog)
    Remove(usize),
}

impl EventEmitter<ProjectNav> for Storm {}

// directories never walked by the finder / search-fallback index (heavy/noise)
const IGNORED: &[&str] = &["node_modules", ".git", ".DS_Store", "target", "dist", "build"];
// the tree only hides git internals + macOS noise; everything else shows, with
// git-ignored entries dimmed (not hidden)
const TREE_HIDDEN: &[&str] = &[".git", ".DS_Store"];

/// Absolute paths git ignores under `root` (whole dirs collapsed), so the tree
/// can hide them. Untracked + ignored, via `git ls-files`; empty if not a repo.
fn git_ignored_paths(root: &Path) -> HashSet<PathBuf> {
    let out = Command::new("git")
        .args(["ls-files", "--others", "--ignored", "--exclude-standard", "--directory"])
        .current_dir(root)
        .output();
    let mut set = HashSet::new();
    if let Ok(out) = out {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let rel = line.trim().trim_end_matches('/');
                if !rel.is_empty() {
                    set.insert(root.join(rel));
                }
            }
        }
    }
    set
}

impl Storm {
    fn new(root: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        let browser_default = default_browser_url(&root);
        let ws_title = load_ws_title(&root);
        let ws_idle = load_ws_idle(&root);
        let ws_paused = load_ws_paused(&root);
        let ws_client = load_ws_client(&root);
        let agent_fresh = load_ws_fresh(&root);
        let review = load_review(&root);
        let mut s = Self {
            root,
            expanded,
            entries: Vec::new(),
            tabs: Vec::new(),
            active: 0,
            tree_width: 420.,
            resizing: false,
            tree_scroll: UniformListScrollHandle::new(),
            tree_focus: cx.focus_handle(),
            confirm_delete: None,
            confirm_focus: cx.focus_handle(),
            confirm_revert: None,
            pr_files: Vec::new(),
            pr_viewed: HashSet::new(),
            pr_viewed_pending: Arc::new(Mutex::new(HashMap::new())),
            pr_viewed_queued: Vec::new(),
            pr_collapsed: HashSet::new(),
            pr_base: String::new(),
            pr_stale: None,
            pr_selected: None,
            pr_focus: cx.focus_handle(),
            pr_gen: 0,
            pr_loading: false,
            pr_node_id: None,
            pr_filter: Field::default(),
            pr_filter_focus: cx.focus_handle(),
            pr_view_filter: PrViewFilter::Unviewed, // start on what's left to review
            pr_rows: Vec::new(),
            pr_shown_files: Vec::new(),
            pr_scroll: UniformListScrollHandle::new(),
            term_tabs: Vec::new(),
            pane_drag: None,
            agent_rpc: rpc::Server::start(random_id() + &random_id()),
            agent_states: HashMap::new(),
            pane_cmds: HashMap::new(),
            review,
            rc_open: false,
            rc_focus: cx.focus_handle(),
            rc_query: Field::default(),
            rc_target: None,
            rc_reply_mode: false,
            review_resolved_tab: false,
            review_expanded: HashSet::new(),
            rc_thread_h: None,
            rc_resize: None,
            rc_width: None,
            ws_idle,
            ws_paused,
            active_term: 0,
            show_terminal: true,
            term_width: 900.,
            run_open: false,
            run_cmd: String::new(),
            run_lines: Vec::new(),
            run_editor: cx.new(|cx| Editor::new(None, cx)),
            run_running: false,
            run_active: false,
            run_failed: false,
            run_spin: 0,
            run_progress: None,
            build_preview: None,
            build_checking: false,
            run_gen: 0,
            run_buf: Arc::new(Mutex::new(Vec::new())),
            run_dirty: Arc::new(AtomicBool::new(false)),
            run_done: Arc::new(AtomicBool::new(true)),
            run_ok: Arc::new(AtomicBool::new(true)),
            flash: None,
            flash_gen: 0,
            resizing_term: false,
            win_width: 1280.,
            win_height: 800.,
            editor_ctx: None,
            editor_ctx_focus: cx.focus_handle(),
            tree_ctx: None,
            tree_ctx_path: None,
            tree_ctx_kind: CtxKind::Tree,
            tree_ctx_focus: cx.focus_handle(),
            git_status: HashMap::new(),
            finder_open: false,
            finder_focus: cx.focus_handle(),
            finder_query: Field::default(),
            finder_results: Vec::new(),
            finder_selected: 0,
            finder_gen: 0,
            finder_dirs_mode: false,
            all_files: Vec::new(),
            ref_hits: HashMap::new(),
            files_scanned: false,
            all_dirs: Vec::new(),
            goto_open: false,
            goto_focus: cx.focus_handle(),
            goto_query: Field::default(),
            focus: cx.focus_handle(),
            inited: false,
            scratch_count: 0,
            branch: String::new(),
            branch_parent: None,
            pr_link: None,
            pr_link_branch: String::new(),
            mem_mb: 0,
            read_only: true, // read-only by default
            left_view: LeftView::Files,
            show_left: true,
            br_open: false,
            br_focus: cx.focus_handle(),
            br_query: Field::default(),
            nf_open: false,
            nf_focus: cx.focus_handle(),
            nf_query: Field::default(),
            nf_dir: PathBuf::new(),
            br_make_pr: false,
            prc_open: false,
            prc_focus: cx.focus_handle(),
            prc_milestone: Field::default(),
            prc_milestones: Vec::new(),
            prc_sel: 0,
            runc_open: false,
            runc_focus: cx.focus_handle(),
            runc_query: Field::default(),
            newproj_open: false,
            newproj_focus: cx.focus_handle(),
            newproj_path: Field::default(),
            newproj_recents: Vec::new(),
            newproj_titles: HashMap::new(),
            newproj_last_titles: HashMap::new(),
            newproj_sel: 0,
            commit_msg: Field::default(),
            commit_focus: cx.focus_handle(),
            wip_id: random_id(),
            commit_filter: Field::default(),
            commit_filter_focus: cx.focus_handle(),
            commit_view_filter: CommitFilter::All,
            commit_selected: None,
            changes_focus: cx.focus_handle(),
            push_open: false,
            push_focus: cx.focus_handle(),
            push_branch: String::new(),
            push_target: String::new(),
            push_base_ref: String::new(),
            push_ahead: false,
            push_commits: Vec::new(),
            push_files: Vec::new(),
            push_collapsed: HashSet::new(),
            push_selected: None,
            push_commit_sel: None,
            push_tags: false,
            commit_checked: HashSet::new(),
            commit_collapsed: HashSet::new(),
            tree_selected: None,
            tree_multi: HashSet::new(),
            ignored: HashSet::new(),
            find_open: false,
            find_focus: cx.focus_handle(),
            find_query: Field::default(),
            find_results: Vec::new(),
            find_selected: 0,
            find_scope: Vec::new(),
            find_gen: 0,
            find_preview: None,
            find_scroll: ScrollHandle::new(),
            find_psel: None,
            find_pdragging: false,
            find_pscroll: ScrollHandle::new(),
            find_char_w: 8.0,
            find_left: 0.,
            find_top: 40.,
            find_w: 760.,
            find_h: 460.,
            find_resize: None,
            find_moving: false,
            find_move_dx: 0.,
            find_move_dy: 0.,
            find_split: 0.42,
            find_split_dragging: false,
            find_case_sensitive: false,
            caret_on: true,
            lsp: None,
            gitp_open: false,
            gitp_focus: cx.focus_handle(),
            gitp_query: Field::default(),
            gitp_branches: Vec::new(),
            gitp_sel: 0,
            gitp_action_branch: None,
            gitp_local: HashSet::new(),
            gitp_action_sel: 0,
            palette_open: false,
            palette_focus: cx.focus_handle(),
            palette_query: Field::default(),
            palette_sel: 0,
            palette_results: Vec::new(),
            palette_gen: 0,
            mc_open: false,
            mc_focus: cx.focus_handle(),
            mc_files: Vec::new(),
            mc_into: String::new(),
            mc_from: String::new(),
            proc_open: false,
            proc_focus: cx.focus_handle(),
            proc_filter: Field::default(),
            proc_list: Vec::new(),
            proc_selected: HashSet::new(),
            proc_anchor: None,
            proc_only_tide: true,
            proc_workspace_only: true,
            proc_ws_pids: Vec::new(),
            ws_names: Vec::new(),
            ws_roots: Vec::new(),
            ws_branches: Vec::new(),
            ws_states: Vec::new(),
            ws_active: 0,
            ws_open: false,
            // the agent pane is the default right-hand view; it starts zoomed,
            // so the terminal panes stay out of the way until asked for
            agent_open: true,
            // when the terminal is revealed it gets a strip at the bottom, not
            // half the dock
            agent_share: 0.78,
            resizing_agent: false,
            agent_zoomed: true,
            term_zoomed: false,
            acp: None,
            acp_polling: false,
            agent_msgs: Vec::new(),
            agent_cur: None,
            agent_cur_thought: None,
            agent_busy: false,
            agent_input: Field::multiline(),
            agent_focus: cx.focus_handle(),
            agent_ctx: None,
            agent_sessions: Vec::new(),
            agent_autoresume: false,
            agent_resumed: false,
            agent_fresh,
            agent_picker: false,
            agent_brief_open: false,
            agent_title: None,
            agent_sel: None,
            agent_sel_drag: false,
            agent_sel_armed: None,
            agent_sel_moved: false,
            agent_sel_pos: None,
            agent_imgs: Vec::new(),
            agent_img_seq: 0,
            agent_pastes: HashMap::new(),
            agent_paste_seq: 0,
            review_note: Field::multiline(),
            review_note_focus: cx.focus_handle(),
            rc_snippet: None,
            agent_spin: 0,
            agent_turn_start: None,
            agent_briefed: false,
            agent_ghost: None,
            at_open: false,
            at_start: 0,
            at_sel: 0,
            at_items: Vec::new(),
            agent_mentions: HashMap::new(),
            agent_jump_open: false,
            agent_jump_query: Field::default(),
            agent_jump_focus: cx.focus_handle(),
            agent_queue: Vec::new(),
            agent_list: gpui::ListState::new(0, gpui::ListAlignment::Bottom, px(600.)),
            agent_list_len: 0,
            acp_state: None,
            git_open: false,
            git_height: 320.,
            resizing_git: false,
            git_focus: cx.focus_handle(),
            git_filter_focus: cx.focus_handle(),
            git_query: Field::default(),
            git_rev: "HEAD".into(),
            git_commits: Vec::new(),
            git_sel: None,
            git_gen: 0,
            git_branches_list: Vec::new(),
            git_collapsed: HashSet::new(),
            git_details_msg: String::new(),
            git_details_files: Vec::new(),
            git_scroll: ScrollHandle::new(),
            browser_open: false,
            browser: None,
            browser_url: {
                let mut f = Field::default();
                f.set(browser_default);
                f
            },
            browser_focus: cx.focus_handle(),
            admin_up: None,
            admin_pane: None,
            watch_pane: None,
            watch_pkgs: Vec::new(),
            admin_spin: 0,
            admin_autoopen: false,
            limits: None,
            limits_open: false,
            ws_title,
            title_open: false,
            title_focus: cx.focus_handle(),
            title_query: Field::default(),
            title_type: None,
            title_client: Field::default(),
            title_field: 0,
            ws_client,
        };
        s.ignored = git_ignored_paths(&s.root); // hide git-ignored paths from the tree
        s.expanded.insert(s.root.clone()); // the root node starts expanded
        s.rebuild();
        // the pinned "You" header is computed from the list's scroll position, so
        // a scroll has to repaint the pane
        {
            let this = cx.entity().downgrade();
            s.agent_list.set_scroll_handler(move |_ev, _w, cx| {
                if let Some(this) = this.upgrade() {
                    this.update(cx, |_, cx| cx.notify());
                }
            });
        }
        s.start_git_poll(cx);
        s.start_caret_blink(cx);
        s.start_admin_health(cx);
        s.start_usage_poll(cx);
        s.start_agent_poll(cx);
        s.start_pane_cmd_poll(cx);
        s.lsp = Lsp::new(&s.root);
        // One plain shell. Claude Code itself lives in the agent pane now, so
        // there's no `cc --continue` pane to reserve half the dock for.
        let root = s.root.clone();
        let rpc = s.rpc_env();
        let shell = cx.new(|cx| Terminal::new(root, rpc, cx));
        s.sync_comment_gutters(cx); // markers for comments restored from disk
        s.term_tabs.push(TermTab::new(shell));
        s
    }

    /// True when some chrome text input is on screen (so we only repaint for
    /// the blink when a caret is actually visible).
    fn input_visible(&self) -> bool {
        self.finder_open
            || self.goto_open
            || self.br_open
            || self.nf_open
            || self.prc_open
            || self.runc_open
            || self.newproj_open
            || self.find_open
            || self.gitp_open
            || self.palette_open
            || self.proc_open
            || (self.show_left && self.left_view == LeftView::Changes)
    }

    fn caret(&self) -> &'static str {
        if self.caret_on { "▏" } else { " " }
    }

    /// Caret glyph for an inline input that's only visible/blinking when its
    /// field actually holds focus (so resting inputs don't show a fake cursor).
    fn caret_if(&self, focused: bool) -> &'static str {
        if focused {
            self.caret()
        } else {
            ""
        }
    }

    /// `Watch N packages`: runs `wwp` in its own tab. Absent when the branch
    /// touches no package. Green while the watch is running — click it for the
    /// log, ⌥click to stop it.
    fn render_watch_btn(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let pkgs = self.watch_pkgs.clone();
        if pkgs.is_empty() {
            return None;
        }
        let running = self.watch_terminal(cx).is_some();
        let n = pkgs.len();
        let tip_text = if running {
            format!("Watching:\n{}\n\nclick for its log, ⌥click to stop", pkgs.join("\n"))
        } else {
            format!("Run `wwp` to watch:\n{}", pkgs.join("\n"))
        };
        Some(
            div()
                .id("watch-pkgs")
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(18.))
                .px_1()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(rgb(if running { GIT_NEW } else { MUTED }))
                .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                .child(div().font_family(ICON_FONT).text_size(px(12.)).child(IC_TOOLS))
                .child(format!("watch {n} package{}", if n == 1 { "" } else { "s" }))
                .tooltip(move |_w, cx| {
                    let text = tip_text.clone();
                    cx.new(|_| TooltipView { text: text.into() }).into()
                })
                .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                    match (running, ev.modifiers().alt) {
                        (true, true) => this.stop_watch(cx),
                        (true, false) => this.show_watch_log(window, cx),
                        _ => this.start_watch(window, cx),
                    }
                })),
        )
    }

    /// The admin ▶ control: grey means stopped, a spinner means the build we
    /// started hasn't answered yet, green means it's serving. Click a running one
    /// for its log, ⌥click to stop it. Shared by the topbar and the status card,
    /// which is why the id is a parameter.
    fn admin_play_btn(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let ours = self.admin_terminal(cx).is_some(); // started from here, so stoppable
        let up = self.admin_up == Some(true);
        let building = ours && !up;
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        // down → grey ▶; our build coming up → spinner; serving → red ■
        let (glyph, color) = if building {
            (SPIN[self.admin_spin % SPIN.len()], GIT_MODIFIED)
        } else if up {
            ("■", GIT_DELETED)
        } else {
            ("▶", MUTED)
        };
        let running = ours || up;
        div()
            .id(id)
            .px_1()
            .flex_shrink_0()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(rgb(color))
            .hover(|s| s.bg(rgb(HOVER)))
            .child(glyph)
            .tooltip(move |_w, cx| {
                let text = if building {
                    "admin is building — opens in your browser when it answers; click to stop, ⌥click for its log"
                } else if up && ours {
                    "admin is up — click to stop it, ⌥click for its log"
                } else if up {
                    "admin is up, but it wasn't started from TIDE"
                } else {
                    "Run `admin` in its own terminal tab"
                };
                cx.new(|_| TooltipView { text: text.into() }).into()
            })
            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                match (running, ev.modifiers().alt) {
                    // no pane of ours to stop or show: it was started elsewhere
                    (true, _) if !ours => this.show_flash("Admin wasn't started from TIDE", cx),
                    (true, true) => this.show_admin_log(window, cx),
                    (true, false) => this.stop_admin(cx),
                    _ => this.start_admin(window, cx),
                }
            }))
    }

    /// Poll the wby-next admin URL periodically (every ~15s) and cache up/down.
    fn start_admin_health(&self, cx: &mut Context<Self>) {
        let Some(url) = admin_url(&self.root) else { return };
        let host = url.trim_start_matches("https://").trim_start_matches("http://").to_string();
        let open_url = url.clone();
        cx.spawn(async move |this, cx| loop {
            let u = url.clone();
            let up = cx.background_executor().spawn(async move { admin_reachable(&u) }).await;
            // while a build we started is still coming up, check often so the
            // spinner stops promptly; otherwise idle along at 7s
            let Ok(waiting) = this.update(cx, |this, cx| {
                let became_up = up && this.admin_up != Some(true);
                this.admin_up = Some(up);
                if became_up && this.admin_pane.is_some() {
                    this.show_flash(&format!("Admin is up — {host}"), cx);
                    // the build we kicked off just started serving: show it
                    if std::mem::take(&mut this.admin_autoopen) {
                        open_browser(&open_url);
                    }
                }
                cx.notify();
                this.admin_pane.is_some() && !up
            }) else {
                break;
            };
            let wait = if waiting { 2 } else { 7 };
            cx.background_executor().timer(Duration::from_secs(wait)).await;
        })
        .detach();
    }

    /// Install the statusline shim that feeds the usage chip: write the script,
    /// then wrap whatever `statusLine` command `~/.claude/settings.json` already
    /// has so an existing status line keeps rendering unchanged.
    fn install_usage_bars(&mut self, cx: &mut Context<Self>) {
        let Ok(home) = std::env::var("HOME") else {
            self.show_flash("No HOME — cannot install", cx);
            return;
        };
        let home = PathBuf::from(home);
        let shim = home.join(".claude/tide-statusline.sh");
        if let Err(e) = std::fs::write(&shim, usage::STATUSLINE_SHIM) {
            self.show_flash(&format!("Could not write the shim: {e}"), cx);
            return;
        }
        let _ = Command::new("chmod").arg("+x").arg(&shim).output();
        let settings = home.join(".claude/settings.json");
        let text = std::fs::read_to_string(&settings).unwrap_or_else(|_| "{}".to_string());
        let mut v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({}));
        let shim_q = format!("\"{}\"", shim.display());
        let prev = v
            .get("statusLine")
            .and_then(|s| s.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        if prev.contains("tide-statusline.sh") {
            self.show_flash("Usage bars already installed", cx);
            return;
        }
        // the old command becomes the shim's argument, stdin and all
        let cmd = if prev.is_empty() { shim_q.clone() } else { format!("{shim_q} {prev}") };
        v["statusLine"] = serde_json::json!({ "type": "command", "command": cmd });
        let Ok(out) = serde_json::to_string_pretty(&v) else { return };
        if let Err(e) = std::fs::write(&settings, out + "\n") {
            self.show_flash(&format!("Could not update settings.json: {e}"), cx);
            return;
        }
        self.show_flash("Usage bars installed — restart Claude Code to start reporting", cx);
        self.start_usage_poll(cx);
    }

    /// Ask "what would a build do?" once — `yarn build --preview --json`, which
    /// builds nothing and takes ~2s. On demand only: polling meant every open
    /// workspace (paused ones included) spawning a yarn every few seconds.
    fn refresh_build_preview(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        if !root.join("scripts/buildPackages").is_dir() {
            self.show_flash("No package build in this project", cx);
            return;
        }
        self.build_checking = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let preview = cx
                .background_executor()
                .spawn(async move {
                    let out = Command::new("zsh")
                        .arg("-lc")
                        .arg("yarn build --preview --json 2>/dev/null")
                        .current_dir(&root)
                        .output()
                        .ok()?;
                    // parse stdout, not the exit code: a preview always exits 0
                    build::parse_preview(&String::from_utf8_lossy(&out.stdout))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.build_checking = false;
                match preview {
                    Some(p) => {
                        let msg = if p.to_build == 0 {
                            format!("All {} packages are up to date", p.total)
                        } else {
                            format!("{} of {} packages need a rebuild", p.to_build, p.total)
                        };
                        this.build_preview = Some(p);
                        this.show_flash(&msg, cx);
                    }
                    None => this.show_flash("Could not read the build preview", cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-read the statusline dump so the usage chip follows the session. Cheap:
    /// one small file, and only when it changed.
    fn start_usage_poll(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            let l = cx.background_executor().spawn(async move { usage::load_limits() }).await;
            if this
                .update(cx, |this, cx| {
                    if this.limits != l {
                        this.limits = l;
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(15)).await;
        })
        .detach();
    }

    /// Spin the admin button while a build we started is coming up. Runs only for
    /// the wait itself, so nothing ticks when there's nothing to show.
    fn start_admin_spinner(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(120)).await;
            let Ok(spinning) = this.update(cx, |this, cx| {
                let spinning = this.admin_pane.is_some() && this.admin_up != Some(true);
                if spinning {
                    this.admin_spin = this.admin_spin.wrapping_add(1);
                    cx.notify();
                }
                spinning
            }) else {
                break;
            };
            if !spinning {
                break;
            }
        })
        .detach();
    }

    /// The terminal pane running `admin`, if it's still alive.
    fn admin_terminal(&self, _cx: &App) -> Option<Entity<Terminal>> {
        let id = self.admin_pane?;
        let t = self.all_terminals().into_iter().find(|t| t.entity_id().as_u64() == id)?;
        // The shell outlives the command, so a pane sitting at its prompt means
        // admin has exited. `pane_cmds` only lists panes with a child process, so
        // an entry at all means something is still running there.
        self.pane_cmds.contains_key(&id).then_some(t)
    }

    /// Start `admin` in its own terminal tab and leave focus where it was.
    /// The pane running `wwp`, if it's still alive.
    fn watch_terminal(&self, _cx: &App) -> Option<Entity<Terminal>> {
        let id = self.watch_pane?;
        let t = self.all_terminals().into_iter().find(|t| t.entity_id().as_u64() == id)?;
        self.pane_cmds.contains_key(&id).then_some(t)
    }

    /// Run `wwp` in its own terminal tab. It picks the base branch and the
    /// changed packages itself; we only mirror the list for the label.
    fn start_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.watch_terminal(cx) {
            self.focus_pane(&t, window, cx); // already watching: show me its log
            return;
        }
        let (was_tab, was_terminal) = (self.active_term, self.show_terminal);
        self.new_terminal(window, cx);
        let Some(t) = self.focused_pane(window, cx) else { return };
        t.read(cx).send_text("wwp\n");
        self.watch_pane = Some(t.entity_id().as_u64());
        if let Some(tab) = self.term_tabs.get_mut(self.active_term) {
            tab.title = Some("Watch".into());
        }
        // background job: hand focus back where it was
        if was_tab < self.term_tabs.len() {
            self.active_term = was_tab;
        }
        self.show_terminal = was_terminal;
        self.focus_active(window, cx);
        self.show_flash("Watching packages — logs in its own terminal tab", cx);
        cx.notify();
    }

    fn show_watch_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.watch_terminal(cx) {
            self.focus_pane(&t, window, cx);
        }
    }

    fn stop_watch(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.watch_terminal(cx) else { return };
        t.read(cx).send_text("\x03");
        self.show_flash("Stopped the package watch", cx);
        cx.notify();
    }

    fn start_admin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.admin_terminal(cx) {
            // already running: show me its log instead of starting a second one
            self.focus_pane(&t, window, cx);
            return;
        }
        let (was_tab, was_terminal) = (self.active_term, self.show_terminal);
        self.new_terminal(window, cx);
        let Some(t) = self.focused_pane(window, cx) else { return };
        t.read(cx).send_text("admin\n");
        self.admin_pane = Some(t.entity_id().as_u64());
        self.admin_up = None; // unknown until it answers
        self.admin_spin = 0;
        self.admin_autoopen = true; // show it as soon as it's actually serving
        self.start_admin_spinner(cx);
        if let Some(tab) = self.term_tabs.get_mut(self.active_term) {
            tab.title = Some("Admin".into());
        }
        // hand focus back: this is meant to run in the background
        if was_tab < self.term_tabs.len() {
            self.active_term = was_tab;
        }
        self.show_terminal = was_terminal;
        self.focus_active(window, cx);
        self.show_flash("Started admin — logs in its own terminal tab", cx);
        cx.notify();
    }

    /// Jump to the admin tab (its build log).
    fn show_admin_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.admin_terminal(cx) else {
            self.show_flash("admin isn't running", cx);
            return;
        };
        let id = t.entity_id();
        if let Some(ix) = self
            .term_tabs
            .iter()
            .position(|tab| tab.terminals().iter().any(|p| p.entity_id() == id))
        {
            self.active_term = ix;
        }
        self.show_terminal = true;
        self.focus_pane(&t, window, cx);
        cx.notify();
    }

    /// Ctrl-C the admin watch (the tab stays, so the log can still be read).
    fn stop_admin(&mut self, cx: &mut Context<Self>) {
        let Some(t) = self.admin_terminal(cx) else { return };
        t.read(cx).send_text("\x03");
        self.admin_autoopen = false; // a stop cancels the pending browser open
        self.show_flash("Stopped admin", cx);
        cx.notify();
    }

    /// Re-check admin health now (used by the summary Refresh button).
    fn check_admin_now(&self, cx: &mut Context<Self>) {
        let Some(url) = admin_url(&self.root) else { return };
        cx.spawn(async move |this, cx| {
            let up = cx.background_executor().spawn(async move { admin_reachable(&url) }).await;
            this.update(cx, |this, cx| { this.admin_up = Some(up); cx.notify(); }).ok();
        })
        .detach();
    }

    fn start_caret_blink(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(530)).await;
            let ok = this
                .update(cx, |this, cx| {
                    this.caret_on = !this.caret_on;
                    if this.input_visible() {
                        cx.notify();
                    }
                })
                .is_ok();
            if !ok {
                break;
            }
        })
        .detach();
    }

    fn active_terminal(&self) -> Option<&Entity<Terminal>> {
        self.term_tabs.get(self.active_term).map(|t| t.active_terminal())
    }

    /// Every shell in the dock — all panes of all tabs.
    fn all_terminals(&self) -> Vec<Entity<Terminal>> {
        self.term_tabs.iter().flat_map(|t| t.terminals()).collect()
    }

    /// The pane the user is actually in: the focused leaf of the active tab,
    /// falling back to that tab's last-active pane.
    fn focused_pane(&self, window: &Window, cx: &App) -> Option<Entity<Terminal>> {
        let tab = self.term_tabs.get(self.active_term)?;
        Some(
            tab.terminals()
                .into_iter()
                .find(|t| t.read(cx).focus_handle.is_focused(window))
                .unwrap_or_else(|| tab.active_terminal().clone()),
        )
    }

    fn focus_pane(&mut self, t: &Entity<Terminal>, window: &mut Window, cx: &mut Context<Self>) {
        self.agent_zoomed = false; // a pane can't take focus while the agent covers it
        if let Some(tab) = self.term_tabs.get_mut(self.active_term) {
            tab.active = t.entity_id();
        }
        let fh = t.read(cx).focus_handle.clone();
        window.focus(&fh, cx);
    }

    /// `(url, token)` handed to every pane shell so agents can report back.
    fn rpc_env(&self) -> Option<(String, String)> {
        self.agent_rpc.as_ref().map(|s| (s.url(), s.token.clone()))
    }

    /// Drain agent reports into `agent_states` a few times a second.
    fn start_agent_poll(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(250)).await;
            let alive = this
                .update(cx, |this, cx| {
                    let (reports, replies, refresh, titles, viewed) = match this.agent_rpc.as_ref()
                    {
                        Some(s) => (
                            s.drain(),
                            s.drain_replies(),
                            s.take_refresh(),
                            s.drain_titles(),
                            s.drain_viewed(),
                        ),
                        None => (Vec::new(), Vec::new(), false, Vec::new(), Vec::new()),
                    };
                    for batch in viewed {
                        this.apply_agent_viewed(&batch, false, cx);
                    }
                    // a proposed name is only taken when this workspace has none:
                    // an agent never renames work you named yourself
                    if let Some(t) = titles.into_iter().next().map(|t| typed_title(&t)) {
                        if this.ws_title.as_ref().map(|t| t.trim().is_empty()).unwrap_or(true) {
                            this.ws_title = Some(t.clone());
                            save_ws_title(&this.root, Some(&t));
                            this.ws_idle = false;
                            save_ws_idle(&this.root, false);
                            this.show_flash(&format!("Workspace named “{t}”"), cx);
                            cx.notify();
                        }
                    }
                    if refresh {
                        // an agent opened a PR or switched branch: re-read now
                        // instead of waiting for ⌘R
                        this.pr_link_branch.clear(); // forces the git poll to re-query
                        this.refresh_pr_link(cx);
                        this.load_pr(cx);
                        this.check_admin_now(cx);
                        cx.notify();
                    }
                    if !replies.is_empty() {
                        let mut landed = 0usize;
                        for r in replies {
                            if let Some(c) = this.review.iter_mut().find(|c| c.id == r.id) {
                                c.replies.push(ReviewReply { who: "agent".into(), text: r.text });
                                landed += 1;
                            }
                        }
                        if landed > 0 {
                            this.save_review();
                            this.show_flash(
                                &format!("{landed} answer{} in review threads", if landed == 1 { "" } else { "s" }),
                                cx,
                            );
                            cx.notify();
                        }
                    }
                    if reports.is_empty() {
                        return;
                    }
                    for r in reports {
                        // an idle report is about the workspace, not one pane
                        if r.state.is_idle() {
                            this.agent_states.clear();
                            this.ws_idle = true;
                            save_ws_idle(&this.root, true);
                            continue;
                        }
                        // anything else means someone is on this workspace again
                        if this.ws_idle {
                            this.ws_idle = false;
                            save_ws_idle(&this.root, false);
                        }
                        // no pane in the report (someone curl'd by hand) → pin it
                        // on the active pane so it still shows up somewhere
                        let pane = r.pane.or_else(|| {
                            this.term_tabs.get(this.active_term).map(|t| t.active.as_u64())
                        });
                        if let Some(pane) = pane {
                            this.agent_states.insert(pane, (r.state, r.summary));
                        }
                    }
                    cx.notify();
                })
                .is_ok();
            if !alive {
                break;
            }
        })
        .detach();
    }

    /// Track what each pane is running, so a badge can say it without you having
    /// to read the scrollback. One `ps` per tick for every pane.
    fn start_pane_cmd_poll(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let Ok(panes) = this.update(cx, |this, cx| {
                this.all_terminals()
                    .into_iter()
                    .filter_map(|t| t.read(cx).child_pid().map(|pid| (t.entity_id().as_u64(), pid)))
                    .collect::<Vec<_>>()
            }) else {
                break;
            };
            let pids: Vec<u32> = panes.iter().map(|(_, p)| *p).collect();
            let cmds = cx.background_executor().spawn(async move { running_cmds(&pids) }).await;
            let ok = this
                .update(cx, |this, cx| {
                    let next: HashMap<u64, String> = panes
                        .iter()
                        .filter_map(|(id, pid)| cmds.get(pid).map(|c| (*id, c.clone())))
                        .collect();
                    if next != this.pane_cmds {
                        this.pane_cmds = next;
                        cx.notify();
                    }
                })
                .is_ok();
            if !ok {
                break;
            }
        })
        .detach();
    }

    /// Reported state of one pane.
    fn pane_state(&self, t: &Entity<Terminal>) -> Option<(AgentState, &str)> {
        self.agent_states.get(&t.entity_id().as_u64()).map(|(s, m)| (*s, m.as_str()))
    }

    /// The most attention-hungry state across a tab's panes (for its tab dot).
    /// Idle doesn't earn a dot — a row of grey dots on every tab says nothing.
    fn tab_state(&self, tab: &TermTab) -> Option<AgentState> {
        tab.terminals()
            .iter()
            .filter_map(|t| self.pane_state(t).map(|(s, _)| s))
            .filter(|s| !s.is_idle())
            .max_by_key(|s| s.priority())
    }

    /// Whole-project roll-up: the loudest agent state across every pane of every
    /// terminal tab. Drives the status screen row and the project switcher dot.
    /// Only live panes count, so a closed pane's leftover state never shows.
    fn agent_rollup(&self) -> Option<(AgentState, String)> {
        self.term_tabs
            .iter()
            .flat_map(|t| t.terminals())
            .filter_map(|t| self.pane_state(&t).map(|(s, m)| (s, m.to_string())))
            // the ACP panel counts as one more agent in this workspace
            .chain(self.acp_state.map(|s| (s, String::new())))
            .max_by_key(|(s, _)| s.priority())
            // nothing live to report → fall back to the persisted idle marker,
            // which is how a workspace marked idle elsewhere shows up here
            .or_else(|| self.ws_idle.then(|| (AgentState::Idle, String::new())))
    }

    /// Ensure at least one terminal exists; returns the active one's handle.
    fn ensure_terminal(&mut self, cx: &mut Context<Self>) {
        if self.term_tabs.is_empty() {
            let (root, rpc) = (self.root.clone(), self.rpc_env());
            let t = cx.new(|cx| Terminal::new(root, rpc, cx));
            self.term_tabs.push(TermTab::new(t));
            self.active_term = 0;
        }
    }

    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (root, rpc) = (self.root.clone(), self.rpc_env());
        let t = cx.new(|cx| Terminal::new(root, rpc, cx));
        self.term_tabs.push(TermTab::new(t.clone()));
        self.active_term = self.term_tabs.len() - 1;
        self.show_terminal = true;
        self.focus_pane(&t, window, cx);
        cx.notify();
    }

    /// ⌘D / ⌘⇧D: halve the focused pane — a fresh shell lands beside it
    /// (`horizontal`) or below it — and takes focus.
    fn split_terminal(&mut self, horizontal: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.hide_browser(); // terminal + browser share the right dock
        self.show_terminal = true;
        self.ensure_terminal(cx);
        let Some(target) = self.focused_pane(window, cx).map(|t| t.entity_id()) else { return };
        let (root, rpc) = (self.root.clone(), self.rpc_env());
        let new = cx.new(|cx| Terminal::new(root, rpc, cx));
        let tab = &mut self.term_tabs[self.active_term];
        tab.zoomed = false; // a new split has to be visible to make sense
        if !tab.panes.split(target, horizontal, &new) {
            // stale focus target (shouldn't happen) — don't leave an orphan shell
            new.update(cx, |t, _| t.kill());
            return;
        }
        self.focus_pane(&new, window, cx);
        cx.notify();
    }

    /// ⌘⇧↩: blow the focused pane up to the whole dock (and back).
    fn act_toggle_zoom(&mut self, _: &ToggleTermZoom, window: &mut Window, cx: &mut Context<Self>) {
        // the agent is zoomable in its own right — it's half of the dock
        if AGENT_PANEL && self.agent_open && self.agent_focus.is_focused(window) {
            self.agent_zoomed = !self.agent_zoomed;
            cx.notify();
            return;
        }
        let Some(cur) = self.focused_pane(window, cx) else { return };
        let ix = self.active_term;
        let Some(tab) = self.term_tabs.get_mut(ix) else { return };
        tab.active = cur.entity_id();
        let zoom = !tab.zoomed;
        tab.zoomed = zoom;
        // "full screen" means the whole dock: hide the sibling panes *and* the
        // agent above them, or a zoomed pane just fills the strip it had
        self.term_zoomed = zoom;
        self.focus_pane(&cur, window, cx);
        cx.notify();
    }

    /// ⌘←/→/↑/↓: move focus to the neighbouring pane. Walks up the pane tree to
    /// the nearest split on the requested axis and takes the sibling next to us,
    /// then descends to that subtree's nearest edge pane. While zoomed there is
    /// no geometry to walk, so the arrows just cycle panes in tree order
    /// (left/up = previous, right/down = next, wrapping around).
    fn move_pane_focus(&mut self, horizontal: bool, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        // The agent UI sits above the panes in the same dock, so vertical moves
        // cross into it: ⌘↓ from the agent lands on the top-left pane, and ⌘↑
        // from a pane with nothing above it goes back to the agent.
        let agent_shown = AGENT_PANEL && self.agent_open;
        if agent_shown && !horizontal && self.agent_focus.is_focused(window) {
            if forward {
                if let Some(tab) = self.term_tabs.get(self.active_term) {
                    let target = tab.panes.edge_leaf(false).clone();
                    self.agent_zoomed = false;
                    self.focus_pane(&target, window, cx);
                    cx.notify();
                }
            }
            return; // ⌘↑ from the agent has nowhere to go
        }
        let Some(cur) = self.focused_pane(window, cx).map(|t| t.entity_id()) else { return };
        let Some(tab) = self.term_tabs.get(self.active_term) else { return };
        if tab.zoomed {
            let panes = tab.terminals();
            let n = panes.len();
            if n < 2 {
                return;
            }
            let at = panes.iter().position(|t| t.entity_id() == cur).unwrap_or(0);
            let next = if forward { (at + 1) % n } else { (at + n - 1) % n };
            let target = panes[next].clone();
            self.focus_pane(&target, window, cx);
            cx.notify();
            return;
        }
        let Some(mut path) = tab.panes.path_to(cur) else { return };
        let at_top = !horizontal && !forward;
        while let Some(ix) = path.pop() {
            let Some(TermPane::Split { horizontal: axis, kids, .. }) = tab.panes.at(&path) else { break };
            if *axis != horizontal {
                continue; // wrong axis — keep climbing
            }
            let next = if forward { ix + 1 } else { ix.wrapping_sub(1) };
            if let Some(sib) = kids.get(next).filter(|_| forward || ix > 0) {
                let target = sib.edge_leaf(!forward).clone();
                self.focus_pane(&target, window, cx);
                cx.notify();
                return;
            }
        }
        // nothing above this pane inside the tab → the agent is what's up there
        if at_top && agent_shown {
            self.focus_agent(window, cx);
        }
    }

    /// Move focus into the agent UI (and make sure it's running).
    fn focus_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_terminal = true;
        self.agent_open = true;
        self.term_zoomed = false;
        self.ensure_acp(cx);
        self.autoresume_agent(cx);
        self.agent_list.scroll_to_end();
        window.focus(&self.agent_focus, cx);
        cx.notify();
    }

    fn act_pane_left(&mut self, _: &FocusPaneLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(true, false, window, cx);
    }

    fn act_pane_right(&mut self, _: &FocusPaneRight, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(true, true, window, cx);
    }

    fn act_pane_up(&mut self, _: &FocusPaneUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(false, false, window, cx);
    }

    fn act_pane_down(&mut self, _: &FocusPaneDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(false, true, window, cx);
    }

    fn act_split_term_right(&mut self, _: &SplitTermRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split_terminal(true, window, cx);
    }

    fn act_split_term_down(&mut self, _: &SplitTermDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split_terminal(false, window, cx);
    }

    /// ⌘T from anywhere. The first press brings the terminal up (the agent pane
    /// holds the dock on its own until then); after that it adds tabs the way it
    /// always has.
    fn act_new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal_visible() {
            self.new_terminal(window, cx);
        } else {
            self.reveal_terminal(window, cx);
        }
    }

    /// True when the terminal strip is actually on screen: it's toggled on, it
    /// has a tab, and the agent hasn't folded it away.
    fn terminal_visible(&self) -> bool {
        self.show_terminal
            && !self.term_tabs.is_empty()
            && !(AGENT_PANEL && self.agent_open && self.agent_zoomed)
    }

    /// Put the terminal strip back under the agent and focus it, starting one
    /// if this workspace has none yet.
    fn reveal_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.agent_zoomed = false;
        self.show_terminal = true;
        self.hide_browser(); // terminal + browser share the right dock
        self.ensure_terminal(cx);
        if let Some(t) = self.active_terminal() {
            let fh = t.read(cx).focus_handle.clone();
            window.focus(&fh, cx);
        }
        cx.notify();
    }

    /// ⌘W in the terminal: close the focused pane; the last pane closes the tab.
    fn act_close_terminal(&mut self, _: &CloseTerminalTab, window: &mut Window, cx: &mut Context<Self>) {
        if self.term_tabs.is_empty() {
            return;
        }
        if self.term_tabs[self.active_term].terminals().len() <= 1 {
            self.close_terminal(self.active_term, window, cx);
            return;
        }
        let Some(target) = self.focused_pane(window, cx) else { return };
        target.update(cx, |t, _| t.kill());
        let id = target.entity_id();
        let tab = &mut self.term_tabs[self.active_term];
        tab.panes.remove(id);
        tab.zoomed = false;
        let next = tab.panes.first().clone();
        self.focus_pane(&next, window, cx);
        cx.notify();
    }

    fn act_close_other_terminals(&mut self, _: &CloseOtherTerminals, window: &mut Window, cx: &mut Context<Self>) {
        if self.term_tabs.is_empty() {
            return;
        }
        let keep = self.term_tabs.remove(self.active_term);
        for t in self.all_terminals() {
            t.update(cx, |t, _| t.kill());
        }
        self.term_tabs.clear();
        self.term_tabs.push(keep);
        self.active_term = 0;
        let t = self.term_tabs[0].active_terminal().clone();
        self.focus_pane(&t, window, cx);
        cx.notify();
    }

    fn switch_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.term_tabs.len() {
            self.active_term = ix;
            let t = self.term_tabs[ix].active_terminal().clone();
            self.focus_pane(&t, window, cx);
            cx.notify();
        }
    }

    /// Close a whole tab (every pane in it).
    fn close_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.term_tabs.len() {
            return;
        }
        for t in self.term_tabs[ix].terminals() {
            t.update(cx, |t, _| t.kill());
        }
        self.term_tabs.remove(ix);
        if self.active_term >= self.term_tabs.len() && !self.term_tabs.is_empty() {
            self.active_term = self.term_tabs.len() - 1;
        } else if ix < self.active_term {
            self.active_term = self.active_term.saturating_sub(1);
        }
        if self.term_tabs.is_empty() {
            self.show_terminal = false;
            self.focus_active(window, cx);
        } else {
            let t = self.term_tabs[self.active_term].active_terminal().clone();
            self.focus_pane(&t, window, cx);
        }
        cx.notify();
    }

    /// Refresh git status, branch, and memory every 5s on a background thread.
    fn start_git_poll(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            let Some((root, prev_pr_branch)) =
                this.update(cx, |this, _| (this.root.clone(), this.pr_link_branch.clone())).ok()
            else {
                break;
            };
            let root2 = root.clone();
            let base = this.update(cx, |this, _| this.pr_base.clone()).unwrap_or_default();
            let (status, branch, mem, ignored, (idle, paused), pkgs) = cx
                .background_executor()
                .spawn(async move {
                    let base = if base.is_empty() { git_default_branch(&root2) } else { base };
                    (
                        compute_git(&root2),
                        git_branch(&root2),
                        process_mem_mb(),
                        git_ignored_paths(&root2),
                        // another TIDE instance (or a script) may have marked this
                        // workspace idle or paused since the last tick
                        (load_ws_idle(&root2), load_ws_paused(&root2)),
                        changed_packages(&root2, &base),
                    )
                })
                .await;
            // refetch the PR link only when the branch actually changed — the
            // gh call hits the network, so we don't want it on every 2s tick
            let (pr_update, parent_update) = if branch != prev_pr_branch {
                let (root3, br) = (root.clone(), branch.clone());
                let (pr, parent) = cx
                    .background_executor()
                    .spawn(async move { (fetch_pr_link(&root3, &br), git_branch_parent(&root3, &br)) })
                    .await;
                (Some(pr), Some(parent))
            } else {
                (None, None)
            };
            if this
                .update(cx, |this, cx| {
                    this.git_status = status;
                    this.branch = branch.clone();
                    this.mem_mb = mem;
                    this.watch_pkgs = pkgs;
                    this.ws_idle = idle;
                    this.ws_paused = paused;
                    if let Some(parent) = parent_update {
                        this.branch_parent = parent;
                    }
                    // refresh the git-ignored set; rebuild the tree if it changed
                    if ignored != this.ignored {
                        this.ignored = ignored;
                        this.rebuild();
                    }
                    if let Some(link) = pr_update {
                        this.pr_link = link;
                        this.pr_link_branch = branch;
                    }
                    // pick up external edits (e.g. from Claude Code) on any open
                    // tab whose buffer has no unsaved changes
                    let editors: Vec<_> = this.tabs.iter().map(|t| t.editor.clone()).collect();
                    for ed in &editors {
                        ed.update(cx, |e, cx| e.reload_if_changed(cx));
                    }
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
            cx.background_executor().timer(Duration::from_secs(5)).await;
        })
        .detach();
    }

    fn rebuild(&mut self) {
        let mut out = Vec::new();
        let root = self.root.clone();
        // the project root itself is the first node (like WebStorm); selecting it
        // scopes find-in-files to the whole project. Its children sit at depth 1.
        let root_name =
            root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "project".into());
        out.push(Entry { name: root_name, path: root.clone(), is_dir: true, depth: 0, ignored: false });
        if self.expanded.contains(&root) {
            self.walk(&root, 1, false, &mut out);
        }
        self.entries = out;
    }

    /// Read + sort a directory's children (dirs first, then case-insensitive by
    /// name), hiding only git internals + macOS noise. Shared by both tree walks.
    fn read_children(dir: &Path) -> Vec<(String, PathBuf, bool)> {
        let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
        let mut items: Vec<(String, PathBuf, bool)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if TREE_HIDDEN.contains(&name.as_str()) {
                    return None;
                }
                let path = e.path();
                let is_dir = path.is_dir();
                Some((name, path, is_dir))
            })
            .collect();
        items.sort_by(|a, b| match (a.2, b.2) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        });
        items
    }

    fn walk(&self, dir: &Path, depth: usize, ignored_ancestor: bool, out: &mut Vec<Entry>) {
        for (name, path, is_dir) in Self::read_children(dir) {
            let ignored = ignored_ancestor || self.ignored.contains(&path);
            out.push(Entry { name, path: path.clone(), is_dir, depth, ignored });
            if is_dir && self.expanded.contains(&path) {
                self.walk(&path, depth + 1, ignored, out);
            }
        }
    }

    fn active_path(&self) -> Option<&PathBuf> {
        self.tabs.get(self.active).map(|t| &t.path)
    }

    /// Headline name for this workspace: its custom title, or "Untitled" — the
    /// folder name is always shown next to it, so the fallback says "nobody has
    /// named this one" instead of repeating the folder.
    fn project_name(&self) -> String {
        self.ws_title
            .as_ref()
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .unwrap_or_else(|| "Untitled".into())
    }

    fn folder_name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "tide".into())
    }

    /// Root-level key handling: escape closes any open popup, and the right-click
    /// context menus respond to Esc / number keys here. Fires for the whole focus
    /// path, so it works even when the menu overlay didn't grab keyboard focus
    /// (e.g. the tree menu, opened from a virtualized uniform_list row).
    fn on_root_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        // context menus: Esc closes, 1-9 invokes an action
        if self.editor_ctx.is_some() {
            if key == "escape" {
                self.editor_ctx = None;
                cx.notify();
            } else if let Ok(n) = key.parse::<usize>() {
                if n >= 1 {
                    self.editor_ctx_run(n - 1, window, cx);
                }
            }
            return;
        }
        if self.tree_ctx.is_some() {
            if key == "escape" {
                self.tree_ctx = None;
                self.tree_ctx_path = None;
                self.tree_ctx_kind = CtxKind::Tree;
                cx.notify();
            } else if let Ok(n) = key.parse::<usize>() {
                if n >= 1 {
                    self.tree_ctx_run(n - 1, window, cx);
                }
            }
            return;
        }
        if key != "escape" {
            return;
        }
        // escape inside the per-branch submenu backs out to the branch list,
        // keeping the git popup open.
        if self.gitp_open && self.gitp_action_branch.is_some() {
            self.gitp_action_branch = None;
            cx.notify();
            return;
        }
        let any_open = self.palette_open
            || self.finder_open
            || self.goto_open
            || self.br_open
            || self.nf_open
            || self.prc_open
            || self.runc_open
            || self.newproj_open
            || self.find_open
            || self.gitp_open
            || self.mc_open
            || self.ws_open
            || self.limits_open;
        if any_open {
            self.palette_open = false;
            self.finder_open = false;
            self.goto_open = false;
            self.br_open = false;
            self.nf_open = false;
            self.mc_open = false;
            self.prc_open = false;
            self.runc_open = false;
            self.newproj_open = false;
            self.find_open = false;
            self.gitp_open = false;
            self.ws_open = false;
            self.limits_open = false;
            self.gitp_action_branch = None;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(tab) = self.tabs.get(self.active) {
            let fh = tab.editor.read(cx).focus_handle.clone();
            window.focus(&fh, cx);
        } else {
            // no open file → keep focus on the root so global shortcuts work
            window.focus(&self.focus, cx);
        }
    }

    /// Click in the tree. Plain = single select; shift = contiguous range from
    /// the anchor to here; cmd = cherry-pick toggle. Multi-selection scopes
    /// find-in-files to several paths.
    fn select_entry(&mut self, ix: usize, shift: bool, cmd: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else { return };
        let path = entry.path.clone();
        if shift {
            // range from the anchor (last plain/cmd selection) to the clicked row
            let anchor_ix = self
                .tree_selected
                .as_ref()
                .and_then(|a| self.entries.iter().position(|e| &e.path == a))
                .unwrap_or(ix);
            let (lo, hi) = (anchor_ix.min(ix), anchor_ix.max(ix));
            self.tree_multi = self.entries[lo..=hi].iter().map(|e| e.path.clone()).collect();
            // keep the anchor so re-shift-clicking re-ranges from the same start
        } else if cmd {
            // cherry-pick: seed with the anchor, then toggle this entry
            if self.tree_multi.is_empty() {
                if let Some(a) = self.tree_selected.clone() {
                    self.tree_multi.insert(a);
                }
            }
            if !self.tree_multi.remove(&path) {
                self.tree_multi.insert(path.clone());
            }
            self.tree_selected = Some(path);
        } else {
            self.tree_multi.clear();
            self.tree_selected = Some(path);
        }
        cx.notify();
    }

    /// True if a tree path is part of the current selection (multi when active).
    fn is_tree_selected(&self, path: &Path) -> bool {
        if self.tree_multi.is_empty() {
            self.tree_selected.as_deref() == Some(path)
        } else {
            self.tree_multi.contains(path)
        }
    }

    /// Tree key handling: cmd+c copies the selected entry, cmd+v pastes it into
    /// the selected folder (or the selected file's parent).
    fn tree_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // F4 (no modifier): open the selected file in a tab
        if ks.key == "f4" && !ks.modifiers.platform {
            if let Some(p) = self.tree_selected.clone() {
                if p.is_file() {
                    self.open_file(p, window, cx);
                }
            }
            return;
        }
        // cmd combos stay off the filter buffer (clipboard / paste / copy-ref)
        if ks.modifiers.platform {
            // cmd+shift+c: copy the selected entry's relative path
            if ks.modifiers.shift && ks.key == "c" {
                if let Some(p) = self.tree_selected.clone() {
                    self.copy_reference(&p, cx);
                }
                return;
            }
            match ks.key.as_str() {
                "c" => {
                    if let Some(p) = self.tree_selected.clone() {
                        *tree_clip().lock().unwrap() = Some((p, false));
                        self.show_flash("Copied", cx);
                    }
                }
                "x" => {
                    if let Some(p) = self.tree_selected.clone() {
                        *tree_clip().lock().unwrap() = Some((p, true));
                        self.show_flash("Cut", cx);
                    }
                }
                "v" => self.paste_into_selected(cx),
                _ => {}
            }
            return;
        }
        // enter opens the selected file, else the first matching file
        if ks.key == "enter" {
            let target = self
                .tree_selected
                .clone()
                .filter(|p| p.is_file())
                .or_else(|| self.entries.iter().find(|e| !e.is_dir).map(|e| e.path.clone()));
            if let Some(p) = target {
                self.tree_selected = Some(p.clone());
                self.open_file(p, window, cx);
            }
            return;
        }
        // backspace deletes the selected entry
        if ks.key == "backspace" {
            if let Some(p) = self.tree_selected.clone() {
                self.confirm_delete = Some(p);
                window.focus(&self.confirm_focus, cx);
                cx.notify();
            }
            return;
        }
        // any other key is ignored — type-to-filter was removed because it
        // scanned the whole repo (incl. node_modules) on the main thread and hung
        cx.notify();
    }

    /// Paste the copied file/folder into the selected folder (recursively for
    /// directories), avoiding clobbering by adding a " copy" suffix on collision.
    fn paste_into_selected(&mut self, cx: &mut Context<Self>) {
        let Some((src, is_cut)) = tree_clip().lock().unwrap().clone() else { return };
        if !src.exists() {
            return;
        }
        // target dir: the selected folder, or the selected file's parent, or root
        let target_dir = match &self.tree_selected {
            Some(p) if p.is_dir() => p.clone(),
            Some(p) => p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        };
        let Some(name) = src.file_name() else { return };
        let dest = unique_dest(target_dir.join(name));
        if src == dest || copy_path(&src, &dest).is_err() {
            return;
        }
        // a cut moves: remove the source after a successful copy, clear the clipboard
        if is_cut {
            let _ = if src.is_dir() { std::fs::remove_dir_all(&src) } else { std::fs::remove_file(&src) };
            *tree_clip().lock().unwrap() = None;
        }
        self.expanded.insert(target_dir);
        self.rebuild();
        self.tree_selected = Some(dest.clone());
        self.reveal_in_tree(&dest);
        self.show_flash(if is_cut { "Moved" } else { "Pasted" }, cx);
        cx.notify();
    }

    /// Keys for the delete-confirmation dialog: enter/y deletes, escape/n cancels.
    fn confirm_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "enter" | "y" => self.do_delete(window, cx),
            "escape" | "n" => {
                self.confirm_delete = None;
                window.focus(&self.tree_focus, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Permanently delete the confirmed path (recursively for folders), dropping
    /// any open tabs under it without re-saving them (which would resurrect the
    /// file), then rebuild the tree.
    fn do_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.confirm_delete.take() else { return };
        let res = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if res.is_ok() {
            // drop tabs for the deleted file or files under a deleted dir (no save)
            self.tabs.retain(|t| !(t.path == path || t.path.starts_with(&path)));
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len().saturating_sub(1);
            }
            self.expanded.remove(&path);
            if self.tree_selected.as_ref() == Some(&path) {
                self.tree_selected = None;
            }
            if self.commit_selected.as_ref() == Some(&path) {
                self.commit_selected = None;
            }
            if tree_clip().lock().unwrap().as_ref().map(|(p, _)| p) == Some(&path) {
                *tree_clip().lock().unwrap() = None;
            }
            self.rebuild();
        }
        window.focus(&self.tree_focus, cx);
        cx.notify();
    }

    fn revert_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "enter" | "y" => self.do_revert(window, cx),
            "escape" | "n" => {
                self.confirm_revert = None;
                window.focus(&self.tree_focus, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Revert the confirmed file to its HEAD version (`git checkout HEAD -- path`),
    /// discarding staged + unstaged changes, then reload any open tab for it.
    fn do_revert(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.confirm_revert.take() else { return };
        let rel = self.rel(&path);
        let ok = Command::new("git")
            .args(["checkout", "HEAD", "--", &rel])
            .current_dir(&self.root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            // reload the on-disk (now reverted) content into any open tab
            for t in &self.tabs {
                if t.path == path {
                    t.editor.update(cx, |e, cx| e.force_reload(cx));
                }
            }
            self.rebuild();
            self.show_flash("Reverted to HEAD", cx);
        } else {
            self.show_flash("Nothing to revert", cx);
        }
        window.focus(&self.tree_focus, cx);
        cx.notify();
    }

    /// Double click in the tree: expand/collapse a folder or open a file.
    fn on_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix) else { return };
        let path = entry.path.clone();
        self.tree_selected = Some(path.clone());
        if entry.is_dir {
            if self.expanded.contains(&path) {
                self.expanded.remove(&path);
            } else {
                self.expanded.insert(path);
            }
            self.rebuild();
        } else {
            self.open_file(path, window, cx);
        }
        cx.notify();
    }

    /// Run a command (zsh alias or git command) in the terminal pane so its
    /// output is visible. Ensures the terminal exists and is shown.
    /// Run `cmd` (through the login shell, so zsh aliases work) and stream its
    /// output into the read-only Run console — no terminal needed.
    fn run_command(&mut self, cmd: String, cx: &mut Context<Self>) {
        // the package build can report structured progress (webiny-js #5629):
        // ask for it, and drive a real bar off the event stream
        let json_build = build::is_build_command(&cmd);
        let cmd = if json_build { build::with_json(&cmd) } else { cmd };
        self.run_progress = json_build.then(build::Progress::default);
        self.run_cmd = cmd.clone();
        self.run_running = true;
        self.run_active = true; // show the bottom-right toast (not the console)
        self.run_failed = false;
        self.run_spin = 0;
        self.run_lines.clear();
        self.run_lines.push(format!("$ {}", cmd));
        let head = self.run_lines.join("\n");
        self.run_editor.update(cx, |e, cx| e.set_log(head, cx));

        // fresh shared state for this run
        let buf = Arc::new(Mutex::new(Vec::new()));
        let dirty = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let ok = Arc::new(AtomicBool::new(true));
        self.run_buf = buf.clone();
        self.run_dirty = dirty.clone();
        self.run_done = done.clone();
        self.run_ok = ok.clone();
        self.run_gen += 1;
        let gen = self.run_gen;
        let root = self.root.clone();

        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let child = Command::new("zsh")
                .arg("-ic")
                .arg(format!("{} 2>&1", cmd))
                .current_dir(&root)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn();
            match child {
                Ok(mut c) => {
                    if let Some(out) = c.stdout.take() {
                        for line in BufReader::new(out).lines().map_while(Result::ok) {
                            if let Ok(mut b) = buf.lock() {
                                b.push(strip_ansi(&line));
                            }
                            dirty.store(true, Ordering::Relaxed);
                        }
                    }
                    let status = c.wait();
                    ok.store(status.map(|s| s.success()).unwrap_or(false), Ordering::Relaxed);
                }
                Err(e) => {
                    if let Ok(mut b) = buf.lock() {
                        b.push(format!("failed to start: {e}"));
                    }
                    ok.store(false, Ordering::Relaxed);
                    dirty.store(true, Ordering::Relaxed);
                }
            }
            done.store(true, Ordering::Relaxed);
            dirty.store(true, Ordering::Relaxed);
        });

        // poll the buffer ~10x/s, animate the spinner, and append output
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let keep_going = this
                .update(cx, |this, cx| {
                    if this.run_gen != gen {
                        return false; // a newer run replaced us
                    }
                    this.run_spin = this.run_spin.wrapping_add(1);
                    if this.run_dirty.swap(false, Ordering::Relaxed) {
                        let new = this
                            .run_buf
                            .lock()
                            .map(|mut b| std::mem::take(&mut *b))
                            .unwrap_or_default();
                        match this.run_progress.as_mut() {
                            // JSON build: keep the readable line, drop the event
                            Some(p) => {
                                let shown: Vec<String> =
                                    new.iter().filter_map(|l| p.apply(l)).collect();
                                this.run_lines.extend(shown);
                            }
                            None => this.run_lines.extend(new),
                        }
                        let n = this.run_lines.len();
                        if n > 5000 {
                            this.run_lines.drain(0..n - 5000);
                        }
                        let text = this.run_lines.join("\n");
                        this.run_editor.update(cx, |e, cx| e.set_log(text, cx));
                    }
                    if this.run_done.load(Ordering::Relaxed) {
                        this.run_running = false;
                        // a build changed what a preview would say: re-check, so
                        // the chip is right without being asked
                        if build::is_build_command(&this.run_cmd) {
                            this.build_preview = None;
                            this.refresh_build_preview(cx);
                        }
                        let success = this.run_ok.load(Ordering::Relaxed);
                        this.run_failed = !success;
                        // after a merge/pull/rebase, auto-open the conflicts dialog
                        // if it left unresolved conflicts
                        let cl = this.run_cmd.to_lowercase();
                        if cl.contains("merge") || cl.contains("pull") || cl.contains("rebase") || cl.contains("cherry-pick") {
                            let files = conflicted_files(&this.root);
                            if !files.is_empty() {
                                this.mc_files = files;
                                this.mc_into = this.branch_label();
                                this.mc_from = merge_source(&this.root);
                                this.mc_open = true;
                                this.run_active = false;
                            }
                        }
                        // "Already up to date" → say it in a toast, no console
                        if success
                            && this.run_lines.iter().any(|l| l.to_lowercase().contains("already up to date"))
                        {
                            this.run_active = false;
                            this.show_flash("Already up to date", cx);
                        }
                        if success {
                            // brief success toast, then auto-hide
                            cx.spawn(async move |this, cx| {
                                cx.background_executor().timer(Duration::from_secs(3)).await;
                                this.update(cx, |this, cx| {
                                    if this.run_gen == gen && !this.run_running {
                                        this.run_active = false;
                                        cx.notify();
                                    }
                                })
                                .ok();
                            })
                            .detach();
                        } else {
                            // a push rejected because the remote is ahead → offer
                            // merge/rebase instead of just dumping the error
                            let rejected = this.run_lines.iter().any(|l| {
                                let s = l.to_lowercase();
                                s.contains("rejected") || s.contains("fetch first")
                            });
                            if rejected {
                                this.push_ahead = true;
                                this.run_active = false;
                            } else {
                                // surface the console on error; replace the toast
                                this.run_open = true;
                                this.run_active = false;
                            }
                        }
                        cx.notify();
                        return false;
                    }
                    cx.notify(); // keep the spinner animating
                    true
                })
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        })
        .detach();
    }

    /// Save the tab at `ix` to disk (no-op if clean).
    fn save_tab(&self, ix: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(ix) {
            if tab.scratch {
                return; // scratch buffers are never written to disk
            }
            tab.editor.update(cx, |e, cx| e.save(cx));
        }
    }

    /// cmd+shift+n: open a new empty scratch buffer (editable, not on disk; gone
    /// when the tab is closed).
    fn act_new_scratch(&mut self, _: &NewScratch, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, cx);
        self.scratch_count += 1;
        let n = self.scratch_count;
        let editor = cx.new(|cx| Editor::new(None, cx));
        editor.update(cx, |e, _| e.set_read_only(false)); // scratch is always editable
        // synthetic path just for the tab label; the editor itself has no path
        // (so save() is a no-op — nothing hits disk)
        let path = PathBuf::from(format!("scratch {n}"));
        self.tabs.push(Tab { path, editor: editor.clone(), scratch: true, preview: false });
        self.active = self.tabs.len() - 1;
        window.focus(&editor.read(cx).focus_handle.clone(), cx);
        cx.notify();
    }

    fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if is_agent_src(&path) {
            return; // the agent-answer sentinel isn't a file; nothing to open
        }
        // leaving the current tab → auto-save it
        self.save_tab(self.active, cx);
        // already open? just switch (and pick up any external edits)
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            let ed = self.tabs[i].editor.clone();
            ed.update(cx, |e, cx| e.reload_if_changed(cx));
        } else {
            let lsp = self.lsp.clone();
            let ro = self.read_only;
            let editor = cx.new(|cx| Editor::new(lsp, cx));
            editor.update(cx, |e, cx| {
                e.set_read_only(ro);
                e.load(path.clone(), cx);
            });
            // go-to-definition: open the target file at the position
            cx.subscribe_in(&editor, window, |this, _ed, ev: &OpenLocation, window, cx| {
                this.open_file(ev.path.clone(), window, cx);
                if let Some(tab) = this.tabs.get(this.active) {
                    let (line, col) = (ev.line, ev.col);
                    tab.editor.update(cx, |e, cx| e.goto(line, col, cx));
                }
            })
            .detach();
            // the gutter "+" (click, or drag over a range) asks for a comment box
            cx.subscribe_in(&editor, window, |this, _ed, ev: &CommentRequest, window, cx| {
                let (line, end) = (ev.line, ev.end);
                if let Some(tab) = this.tabs.get(this.active) {
                    let path = tab.path.clone();
                    this.begin_comment(path, line, end, window, cx);
                }
            })
            .detach();
            // clicking the read-only hint's "enable EDIT" link flips edit mode
            cx.subscribe(&editor, |this, _ed, _ev: &ToggleReadOnly, cx| {
                if this.read_only {
                    this.toggle_read_only(cx);
                }
            })
            .detach();
            // a markdown file opens rendered; ⌘⇧V flips it back to the source
            let preview = is_markdown(&path);
            self.tabs.push(Tab { path: path.clone(), editor, scratch: false, preview });
            self.active = self.tabs.len() - 1;
        }
        self.tree_selected = Some(path.clone());
        self.reveal_in_tree(&path);
        // a freshly created editor knows nothing about pending comments on its file
        self.sync_comment_gutters(cx);
        self.focus_active(window, cx);
    }

    /// Expand all ancestor folders of `path` and scroll the tree to it so the
    /// open file is always visible/highlighted in the explorer.
    /// Act on a chosen finder result: open a file, or reveal a folder in the
    /// project tree (expanded + selected).
    fn open_finder_result(&mut self, p: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.finder_open = false;
        if self.finder_dirs_mode {
            self.left_view = LeftView::Files;
            self.show_left = true;
            self.expanded.insert(p.clone());
            self.tree_selected = Some(p.clone());
            self.reveal_in_tree(&p); // expands ancestors + scrolls into view
            self.focus_active(window, cx);
            cx.notify();
        } else {
            self.open_file(p, window, cx);
        }
    }

    fn reveal_in_tree(&mut self, path: &Path) {
        let mut dir = path.parent();
        while let Some(d) = dir {
            self.expanded.insert(d.to_path_buf());
            if d == self.root {
                break;
            }
            dir = d.parent();
        }
        self.rebuild();
        if let Some(ix) = self.entries.iter().position(|e| e.path == path) {
            self.tree_scroll.scroll_to_item(ix, ScrollStrategy::Center);
        }
    }

    /// Show the active file in the explorer: switch to the Files view, select +
    /// reveal it, and move focus to the tree. Used by the editor context menu.
    fn reveal_active_in_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_path().cloned() else { return };
        self.left_view = LeftView::Files;
        self.show_left = true;
        self.tree_selected = Some(path.clone());
        self.reveal_in_tree(&path);
        window.focus(&self.tree_focus, cx);
        cx.notify();
    }

    /// Drop every collapsed ancestor of `path` from a dir-collapse set (relative
    /// keys, plus the root `""`), so a revealed row is actually on screen.
    /// Chain-compressed nodes are keyed by their deepest path, which is still an
    /// ancestor prefix, so this clears those too.
    fn expand_ancestors(collapsed: &mut HashSet<PathBuf>, path: &Path, root: &Path) {
        collapsed.remove(Path::new(""));
        let rel = path.strip_prefix(root).unwrap_or(path);
        let mut cur = PathBuf::new();
        for comp in rel.parent().unwrap_or(Path::new("")).components() {
            cur = cur.join(comp);
            collapsed.remove(&cur);
        }
    }

    fn reveal_active_in_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_path().cloned() else { return };
        if !self.git_status.contains_key(&path) {
            self.show_flash("Not in Changes — file is unmodified", cx);
            return;
        }
        self.left_view = LeftView::Changes;
        self.show_left = true;
        Self::expand_ancestors(&mut self.commit_collapsed, &path, &self.root);
        self.commit_selected = Some(path);
        window.focus(&self.changes_focus, cx);
        cx.notify();
    }

    fn reveal_active_in_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_path().cloned() else { return };
        if !self.pr_files.iter().any(|(p, _)| p == &path) {
            self.show_flash("Not in this PR", cx);
            return;
        }
        self.left_view = LeftView::PullRequest;
        self.show_left = true;
        // a name or viewed-state filter would hide the row we just revealed
        if !self.rel(&path).to_lowercase().contains(&self.pr_filter.text.to_lowercase()) {
            self.pr_filter.set(String::new());
        }
        let viewed = self.pr_viewed.contains(&path);
        if (viewed && self.pr_view_filter == PrViewFilter::Unviewed)
            || (!viewed && self.pr_view_filter == PrViewFilter::Viewed)
        {
            self.pr_view_filter = PrViewFilter::All;
        }
        Self::expand_ancestors(&mut self.pr_collapsed, &path, &self.root);
        self.pr_selected = Some(path);
        window.focus(&self.pr_focus, cx);
        cx.notify();
    }

    fn switch_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.tabs.len() {
            if ix != self.active {
                self.save_tab(self.active, cx); // auto-save the tab we leave
            }
            self.active = ix;
            // show any external edits made while this tab was in the background
            let ed = self.tabs[ix].editor.clone();
            ed.update(cx, |e, cx| e.reload_if_changed(cx));
            // keep the explorer highlight in sync with the file you switched to
            if let Some(p) = self.tabs.get(ix).map(|t| t.path.clone()) {
                self.tree_selected = Some(p.clone());
                self.reveal_in_tree(&p);
            }
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.save_tab(ix, cx); // auto-save before closing
        self.stash_comment_draft(ix, cx); // don't lose an open comment box
        self.tabs.remove(ix);
        if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.active = self.tabs.len() - 1;
        } else if ix < self.active {
            self.active = self.active.saturating_sub(1);
        }
        // keep the explorer highlight on whatever file is now active
        if let Some(p) = self.active_path().cloned() {
            self.tree_selected = Some(p);
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    fn act_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.close_tab(self.active, window, cx);
        }
    }

    fn act_close_others(&mut self, _: &CloseOthers, _window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let keep = self.tabs.remove(self.active);
        self.tabs.clear();
        self.tabs.push(keep);
        self.active = 0;
        cx.notify();
    }

    fn act_toggle_term(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_terminal(window, cx);
    }

    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, cx); // leaving the editor → auto-save
        if AGENT_PANEL && self.agent_open {
            // ⌥F12 while a pane is zoomed means "give me the agent back"
            if self.term_zoomed {
                self.term_zoomed = false;
                self.focus_agent(window, cx);
                cx.notify();
                return;
            }
            // the dock belongs to the agent; this folds the terminal strip under
            // it away and back, and never hides the agent along with it
            if self.terminal_visible() {
                self.agent_zoomed = true; // fold the strip away; the agent takes the dock
                self.focus_agent(window, cx);
                cx.notify();
            } else {
                self.reveal_terminal(window, cx);
            }
            return;
        }
        self.show_terminal = !self.show_terminal;
        if self.show_terminal {
            self.hide_browser(); // terminal + browser share the right dock
            self.ensure_terminal(cx);
            if let Some(t) = self.active_terminal() {
                let fh = t.read(cx).focus_handle.clone();
                window.focus(&fh, cx);
            }
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// Close the browser dock and hide the WebView (terminal ↔ browser swap).
    fn hide_browser(&mut self) {
        self.browser_open = false;
        if let Some(b) = &self.browser {
            b.set_visible(false);
        }
    }

    /// Hide this project's native WebView without changing `browser_open`, so a
    /// background project's browser doesn't float over the active one. It
    /// re-shows when this project becomes active again (render_browser_dock).
    fn park_browser(&self) {
        if let Some(b) = &self.browser {
            b.set_visible(false);
        }
    }

    fn toggle_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !BROWSER_PANEL {
            return; // feature parked; flip BROWSER_PANEL to re-enable
        }
        self.browser_open = !self.browser_open;
        if self.browser_open {
            self.show_terminal = false; // shares the right dock with the terminal
            window.focus(&self.browser_focus, cx);
            // the WebView is created + shown on the next render (needs &Window)
        } else if let Some(b) = &self.browser {
            b.set_visible(false);
        }
        cx.notify();
    }

    /// Navigate the browser to the address-bar text (adds a scheme if missing).
    fn browser_navigate(&mut self, cx: &mut Context<Self>) {
        let url = normalize_url(self.browser_url.text.trim());
        self.browser_url.set(url.clone());
        if let Some(b) = &self.browser {
            b.navigate(&url);
        }
        cx.notify();
    }

    fn browser_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "enter" => self.browser_navigate(cx),
            _ => {
                Self::field_input(&mut self.browser_url, &ev.keystroke, cx, |_| true);
                cx.notify();
            }
        }
    }

    fn act_open_finder(&mut self, _: &OpenFinder, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, cx);
        self.finder_open = true;
        self.finder_query.clear();
        self.finder_selected = 0;
        self.update_finder(); // show whatever we already have (instant)
        window.focus(&self.finder_focus, cx);
        cx.notify();

        // (re)scan the file list off the main thread so opening never blocks
        self.files_scanned = true; // this scan is the index; no need for another
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let (files, dirs) = cx
                .background_executor()
                .spawn(async move { collect_paths(&root) })
                .await;
            this.update(cx, |this, cx| {
                this.all_files = files;
                this.all_dirs = dirs;
                this.ref_hits.clear();
                this.update_finder();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn update_finder(&mut self) {
        let raw = self.finder_query.text.clone();
        // peel a trailing ":line[:col]" so a pasted "path:45" still matches the file
        let (path_q, _, _) = split_finder_query(&raw);
        // a trailing '/' switches the finder to listing folders
        let dirs_mode = path_q.ends_with('/');
        let query = if dirs_mode { path_q.trim_end_matches('/').to_string() } else { path_q.to_string() };
        let root = self.root.clone();
        let source = if dirs_mode { &self.all_dirs } else { &self.all_files };
        let mut scored: Vec<(i32, PathBuf)> = source
            .iter()
            .filter_map(|p| {
                let rel = p.strip_prefix(&root).unwrap_or(p).to_string_lossy().to_string();
                finder_score(&query, &rel).map(|sc| (sc, p.clone()))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                a.1.to_string_lossy().len().cmp(&b.1.to_string_lossy().len())
            })
        });
        self.finder_results = scored.into_iter().take(50).map(|(_, p)| p).collect();
        self.finder_dirs_mode = dirs_mode;
    }

    /// Debounced finder rescan — a burst of keystrokes scans `all_files` once.
    fn schedule_finder(&mut self, cx: &mut Context<Self>) {
        self.finder_gen += 1;
        let gen = self.finder_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(80)).await;
            this.update(cx, |this, cx| {
                if this.finder_gen == gen {
                    this.update_finder();
                    this.finder_selected = 0;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Clipboard text (for paste into chrome inputs).
    /// Drive a [`Field`] from a keystroke, bridging the system clipboard for
    /// cmd+v (read) and cmd+c/cmd+x (write). One place for every chrome input.
    /// True when this keystroke will also arrive as text through the input
    /// handler (`insertText:`). Boxes that register an `input_sink` must not
    /// insert it on the key path too, or every character lands twice — the same
    /// double-typing the terminal hit.
    fn text_comes_via_input_handler(ks: &Keystroke) -> bool {
        let m = &ks.modifiers;
        !m.platform
            && !m.control
            && !m.function
            && ks.key_char.as_deref().is_some_and(|c| {
                !c.is_empty() && !c.chars().any(|ch| ch.is_control())
            })
    }

    fn field_input(
        field: &mut Field,
        ks: &Keystroke,
        cx: &mut Context<Self>,
        accept: impl Fn(&str) -> bool,
    ) -> Edit {
        let clip = cx.read_from_clipboard().and_then(|i| i.text());
        let edit = field.key(ks, clip, accept);
        if let Some(text) = field.take_copy() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        edit
    }

    fn finder_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.finder_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                if let Some(p) = self.finder_results.get(self.finder_selected).cloned() {
                    // a ":line[:col]" suffix in the query jumps there after opening
                    let (_, line, col) = split_finder_query(&self.finder_query.text);
                    self.open_finder_result(p, window, cx);
                    if let Some(line) = line {
                        if let Some(tab) = self.tabs.get(self.active) {
                            tab.editor.update(cx, |e, cx| e.goto(line, col.unwrap_or(1), cx));
                        }
                    }
                }
            }
            "up" => self.finder_selected = self.finder_selected.saturating_sub(1),
            "down" => {
                self.finder_selected =
                    (self.finder_selected + 1).min(self.finder_results.len().saturating_sub(1))
            }
            _ => {
                if Self::field_input(&mut self.finder_query, ks, cx, |_| true) == Edit::Changed {
                    self.schedule_finder(cx); // debounced rescan
                }
            }
        }
        cx.notify();
    }

    fn act_goto(&mut self, _: &GotoLine, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        self.save_tab(self.active, cx);
        self.goto_open = true;
        self.goto_query.clear();
        window.focus(&self.goto_focus, cx);
        cx.notify();
    }

    fn goto_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.goto_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                let mut parts = self.goto_query.text.split(':');
                let line: usize = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(1);
                let col: usize = parts.next().and_then(|s| s.trim().parse().ok()).unwrap_or(1);
                if let Some(tab) = self.tabs.get(self.active) {
                    tab.editor.update(cx, |e, cx| e.goto(line, col, cx));
                }
                self.goto_open = false;
                self.focus_active(window, cx);
            }
            _ => {
                // only digits and ':' may be inserted
                Self::field_input(&mut self.goto_query, ks, cx, |s| {
                    s.chars().all(|c| c.is_ascii_digit() || c == ':')
                });
            }
        }
        cx.notify();
    }

    fn open_title_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.title_open = true;
        let (ty, desc) = split_title(self.ws_title.as_deref().unwrap_or(""));
        self.title_type = ty;
        self.title_query.set(desc);
        self.title_client.set(self.ws_client.clone().unwrap_or_default());
        self.title_field = 0;
        window.focus(&self.title_focus, cx);
        cx.notify();
    }

    /// `<emoji> <description>`, or just the description with no type picked.
    fn composed_title(&self) -> String {
        let desc = self.title_query.text.trim();
        match self.title_type.and_then(|i| TITLE_TYPES.get(i)) {
            Some((_, emoji, _)) if !desc.is_empty() => format!("{emoji} {desc}"),
            Some((_, emoji, _)) => (*emoji).to_string(),
            None => desc.to_string(),
        }
    }

    fn title_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.title_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                let t = self.composed_title();
                self.ws_title = (!t.is_empty()).then(|| t.clone());
                save_ws_title(&self.root, if t.is_empty() { None } else { Some(&t) });
                let c = self.title_client.text.trim().to_string();
                self.ws_client = (!c.is_empty()).then(|| c.clone());
                save_ws_client(&self.root, if c.is_empty() { None } else { Some(&c) });
                // naming a workspace means you're picking it up: it's neither
                // parked nor free any more
                if !t.is_empty() {
                    if self.ws_paused {
                        self.ws_paused = false;
                        save_ws_paused(&self.root, false);
                    }
                    if self.ws_idle {
                        self.ws_idle = false;
                        save_ws_idle(&self.root, false);
                    }
                }
                self.title_open = false;
                self.focus_active(window, cx);
            }
            // ⇥ moves between the title and the client (types are chips, or a
            // "fix:" prefix in the title)
            "tab" => self.title_field = 1 - self.title_field,
            _ => {
                if self.title_field == 1 {
                    Self::field_input(&mut self.title_client, &ev.keystroke, cx, |_| true);
                } else {
                    Self::field_input(&mut self.title_query, &ev.keystroke, cx, |_| true);
                    self.consume_type_prefix();
                }
            }
        }
        cx.notify();
    }

    /// Typing `fix:` (or `ci: `) picks that type and drops the prefix from the
    /// description — the fast path when you already know the type you want.
    fn consume_type_prefix(&mut self) {
        let text = self.title_query.text.clone();
        let Some((head, rest)) = text.split_once(':') else { return };
        let key = head.trim().to_lowercase();
        if let Some(i) = TITLE_TYPES.iter().position(|(k, _, _)| *k == key) {
            self.title_type = Some(i);
            self.title_query.set(rest.trim_start().to_string());
        }
    }

    fn render_title_prompt(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 460.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        // type chips: emoji + key, the picked one outlined
        let mut types = div().flex().flex_row().flex_wrap().gap_1().mx_3().mb_2();
        for (i, (key, emoji, label)) in TITLE_TYPES.iter().enumerate() {
            let on = self.title_type == Some(i);
            let tip_text: SharedString = format!("{key} — {label}").into();
            types = types.child(
                div()
                    .id(("title-type", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .py(px(2.))
                    .rounded_md()
                    .cursor_pointer()
                    .bg(rgb(if on { SELECTED_BG } else { BG }))
                    .border_1()
                    .border_color(rgb(if on { ACCENT } else { BORDER }))
                    .text_size(px(10.))
                    .text_color(rgb(if on { SEL_TEXT } else { MUTED }))
                    .child(div().text_size(px(11.)).child(*emoji))
                    .child(*key)
                    .tooltip(move |_w, cx| {
                        cx.new(|_| TooltipView { text: tip_text.clone() }).into()
                    })
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.title_type = if this.title_type == Some(i) { None } else { Some(i) };
                        window.focus(&this.title_focus, cx); // keep typing in the field
                        cx.notify();
                    })),
            );
        }
        let preview = self.composed_title();
        div()
            .absolute()
            .top(px(120.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.title_focus)
            .on_key_down(cx.listener(Self::title_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("Workspace title  ·  type \"fix:\" to pick a type  ·  ⇥ title/client  ·  ↩ saves"),
            )
            .child(types)
            .child(
                div()
                    .mx_3()
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .border_color(rgb(if self.title_field == 0 { ACCENT } else { BORDER }))
                    .text_color(rgb(TEXT))
                    .child(self.title_query.render(self.caret_if(self.title_field == 0), SELECTION)),
            )
            // who the work is for — optional, and only shown on the workspace when set
            .child(
                div()
                    .mx_3()
                    .mt_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().w(px(44.)).text_size(px(10.)).text_color(rgb(MUTED)).child("Client"))
                    .child(
                        div()
                            .id("title-client")
                            .flex_grow(1.0)
                            .px_2()
                            .py_1()
                            .bg(rgb(BG))
                            .border_1()
                            .border_color(rgb(if self.title_field == 1 { ACCENT } else { BORDER }))
                            .rounded_md()
                            .text_size(px(12.))
                            .cursor_pointer()
                            .text_color(rgb(TEXT))
                            .child(if self.title_client.is_empty() && self.title_field != 1 {
                                div().text_color(rgb(MUTED)).child("optional — e.g. Acme").into_any_element()
                            } else {
                                self.title_client.render(self.caret_if(self.title_field == 1), SELECTION)
                            })
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.title_field = 1;
                                cx.notify();
                            })),
                    ),
            )
            // what the topbar will show (empty = falls back to the folder name)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(10.))
                    .text_color(rgb(if preview.is_empty() { LINE_NUMBER } else { MUTED }))
                    .child(if preview.is_empty() {
                        "empty = Untitled".to_string()
                    } else {
                        format!("→ {preview}")
                    }),
            )
    }

    // ── toolbar alias actions (run zsh aliases in the terminal) ─────────────

    fn open_branch_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.br_open = true;
        self.br_query.clear();
        self.br_make_pr = false;
        window.focus(&self.br_focus, cx);
        cx.notify();
    }

    /// New-file prompt. Target dir = the right-clicked folder (or the file's
    /// parent), else the project root.
    fn open_new_file_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.nf_dir = match self.tree_ctx_path.clone() {
            Some(p) if p.is_dir() => p,
            Some(p) => p.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        };
        self.nf_open = true;
        self.nf_query.clear();
        window.focus(&self.nf_focus, cx);
        cx.notify();
    }

    fn nf_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.nf_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                let name = self.nf_query.text.trim().to_string();
                self.nf_open = false;
                if name.is_empty() {
                    return;
                }
                // name may include subdirs (e.g. "sub/file.ts") — create parents
                let path = self.nf_dir.join(&name);
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if !path.exists() {
                    let _ = std::fs::write(&path, "");
                }
                self.rebuild();
                self.open_file(path, window, cx);
            }
            _ => {
                Self::field_input(&mut self.nf_query, ks, cx, |_| true);
            }
        }
        cx.notify();
    }

    fn render_new_file_prompt(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 420.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let dir = self.rel(&self.nf_dir);
        let dir = if dir.is_empty() { ".".to_string() } else { dir };
        div()
            .absolute()
            .top(px(120.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.nf_focus)
            .on_key_down(cx.listener(Self::nf_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(format!("New file in {}", dir)),
            )
            .child(
                div()
                    .mx_3()
                    .mb_3()
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .text_color(rgb(TEXT))
                    .child(self.nf_query.render(self.caret(), SELECTION)),
            )
    }

    fn br_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.br_open = false;
                self.focus_active(window, cx);
            }
            // Tab toggles the "also create a PR" checkbox
            "tab" => {
                self.br_make_pr = !self.br_make_pr;
            }
            "enter" => {
                let name = self.br_query.text.trim().to_string();
                let make_pr = self.br_make_pr;
                self.br_open = false;
                if !name.is_empty() {
                    // PR needs the branch pushed first (upstream + a ref to open against)
                    let cmd = if make_pr {
                        format!("br {} && git push -u origin HEAD && pr", name)
                    } else {
                        format!("br {}", name)
                    };
                    self.run_command(cmd, cx);
                }
            }
            _ => {
                Self::field_input(&mut self.br_query, ks, cx, |_| true);
            }
        }
        cx.notify();
    }

    /// Open the create-PR prompt: an optional milestone, then `gh pr create`.
    /// Milestones are fetched from GitHub in the background to drive autocomplete.
    fn open_pr_create_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prc_open = true;
        // pre-fill (and auto-select via the suggestion list) the last milestone used
        match load_last_milestone() {
            Some(m) => self.prc_milestone.set(m),
            None => self.prc_milestone.clear(),
        }
        self.prc_milestones.clear();
        self.prc_sel = 0;
        window.focus(&self.prc_focus, cx);
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let list = cx.background_executor().spawn(async move { gh_milestones(&root) }).await;
            this.update(cx, |this, cx| {
                if this.prc_open {
                    this.prc_milestones = list;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Milestone titles matching what's typed (case-insensitive substring); the
    /// full list when the field is empty.
    fn prc_suggestions(&self) -> Vec<String> {
        let q = self.prc_milestone.text.trim().to_lowercase();
        self.prc_milestones
            .iter()
            .filter(|m| q.is_empty() || m.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }

    fn prc_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let sugg = self.prc_suggestions();
        match ks.key.as_str() {
            "escape" => {
                self.prc_open = false;
                self.focus_active(window, cx);
            }
            "down" => {
                if !sugg.is_empty() {
                    self.prc_sel = (self.prc_sel + 1).min(sugg.len() - 1);
                }
            }
            "up" => {
                self.prc_sel = self.prc_sel.saturating_sub(1);
            }
            // Tab fills the field with the highlighted suggestion (keeps editing)
            "tab" => {
                if let Some(m) = sugg.get(self.prc_sel) {
                    self.prc_milestone.set(m.clone());
                }
            }
            "enter" => {
                // a highlighted suggestion wins over the raw typed text
                let milestone = sugg
                    .get(self.prc_sel)
                    .cloned()
                    .unwrap_or_else(|| self.prc_milestone.text.trim().to_string());
                let milestone = milestone.trim().to_string();
                self.prc_open = false;
                // runs the `pr` zsh function; milestone is optional (--ms <ver>)
                let cmd = if milestone.is_empty() {
                    "pr".to_string()
                } else {
                    save_last_milestone(&milestone); // remember for next time
                    format!("pr --ms {}", milestone)
                };
                self.run_command(cmd, cx);
            }
            _ => {
                if Self::field_input(&mut self.prc_milestone, ks, cx, |_| true) == Edit::Changed {
                    self.prc_sel = 0; // re-filtered list → reset highlight
                }
            }
        }
        cx.notify();
    }

    /// cmd+shift+t: prompt for an arbitrary shell command to run.
    fn act_set_ws_title(&mut self, _: &SetWsTitle, window: &mut Window, cx: &mut Context<Self>) {
        self.open_title_prompt(window, cx);
    }

    fn act_add_comment(&mut self, _: &AddComment, window: &mut Window, cx: &mut Context<Self>) {
        self.open_comment_prompt(window, cx);
    }

    fn act_show_comments(&mut self, _: &ShowComments, window: &mut Window, cx: &mut Context<Self>) {
        self.open_comment_list(window, cx);
    }

    fn act_send_comments(&mut self, _: &SendComments, window: &mut Window, cx: &mut Context<Self>) {
        self.send_review(window, cx);
    }

    /// ⌘R: re-query the branch's PR (and the admin health dot) right now — the
    /// same thing the status screen's ↻ Refresh button does. The background poll
    /// only refetches the PR when the branch changes, so a PR opened or a check
    /// finished since then wouldn't show up on its own.
    fn act_refresh_pr(&mut self, _: &RefreshPr, _window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_pr_link(cx);
        self.check_admin_now(cx);
        // with the review pane open, also re-diff the changed files
        if self.show_left && self.left_view == LeftView::PullRequest {
            self.load_pr(cx);
        }
        self.show_flash("Refreshing PR status…", cx);
        cx.notify();
    }

    fn act_run_command(&mut self, _: &RunCommand, window: &mut Window, cx: &mut Context<Self>) {
        self.runc_open = true;
        self.runc_query.clear();
        window.focus(&self.runc_focus, cx);
        cx.notify();
    }

    fn runc_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.runc_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                let cmd = self.runc_query.text.trim().to_string();
                self.runc_open = false;
                self.focus_active(window, cx);
                if !cmd.is_empty() {
                    self.run_command(cmd, cx);
                }
            }
            _ => {
                Self::field_input(&mut self.runc_query, ks, cx, |_| true);
            }
        }
        cx.notify();
    }

    /// opt+f: fetch all remotes (quick shortcut for the palette's Fetch command).
    fn act_fetch(&mut self, _: &FetchRemotes, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_command("git fetch --all --prune".into(), cx);
    }

    /// cmd+shift+m: WIP commit + push (`wipp`).
    fn act_wip_push(&mut self, _: &WipPush, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_command("wipp".into(), cx);
    }

    /// cmd+shift+b: build (`yb`). "Install and Build" (`yyb`) is a palette
    /// command — installing is the slow, occasional case.
    fn act_run_build(&mut self, _: &RunBuild, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_command("yb".into(), cx);
    }

    /// opt+l: pull the current branch, merging when local+remote have diverged
    /// (--no-rebase) so it never errors asking how to reconcile.
    fn act_pull(&mut self, _: &PullRemote, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_command("git pull --no-rebase".into(), cx);
    }

    /// Flip read-only mode and apply it to every open editor.
    fn toggle_read_only(&mut self, cx: &mut Context<Self>) {
        self.read_only = !self.read_only;
        let ro = self.read_only;
        for tab in &self.tabs {
            tab.editor.update(cx, |e, _| e.set_read_only(ro));
        }
        let msg = if ro { "Read-only mode" } else { "Edit mode" };
        self.show_flash(msg, cx);
        cx.notify();
    }

    /// cmd+shift+n: open the "New Project" dialog (type a folder path or pick one).
    fn act_new_project(&mut self, _: &NewProject, window: &mut Window, cx: &mut Context<Self>) {
        self.newproj_open = true;
        self.newproj_path.clear();
        self.newproj_recents = load_recent_projects();
        // titles come off disk once, on open — not per frame
        self.newproj_titles = load_ws_titles();
        self.newproj_last_titles = load_ws_last_titles();
        self.newproj_sel = 0;
        window.focus(&self.newproj_focus, cx);
        cx.notify();
    }

    /// Recent projects filtered by the typed text (matches name or full path),
    /// most-recent first, capped at 20.
    fn newproj_filtered(&self) -> Vec<PathBuf> {
        let q = self.newproj_path.text.trim().to_lowercase();
        self.newproj_recents
            .iter()
            .filter(|p| q.is_empty() || p.to_string_lossy().to_lowercase().contains(&q))
            .take(20)
            .cloned()
            .collect()
    }

    fn newproj_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.newproj_open = false;
                self.focus_active(window, cx);
            }
            "down" => {
                let n = self.newproj_filtered().len();
                self.newproj_sel = (self.newproj_sel + 1).min(n.saturating_sub(1));
            }
            "up" => self.newproj_sel = self.newproj_sel.saturating_sub(1),
            "enter" => {
                // open the highlighted recent, or fall back to the typed path
                let filtered = self.newproj_filtered();
                if let Some(p) = filtered.get(self.newproj_sel).cloned() {
                    self.newproj_open = false;
                    cx.emit(ProjectNav::OpenPath(p));
                } else {
                    self.newproj_submit(cx);
                }
            }
            _ => {
                Self::field_input(&mut self.newproj_path, ks, cx, |_| true);
                self.newproj_sel = 0; // filter changed → reset highlight to the top
            }
        }
        cx.notify();
    }

    /// Open the project at the dialog's typed path (if it's an existing folder).
    fn newproj_submit(&mut self, cx: &mut Context<Self>) {
        let raw = self.newproj_path.text.trim();
        if raw.is_empty() {
            return;
        }
        // expand a leading ~ to the home directory for convenience
        let expanded = if let Some(rest) = raw.strip_prefix("~/") {
            std::env::var("HOME").map(|h| format!("{h}/{rest}")).unwrap_or_else(|_| raw.to_string())
        } else {
            raw.to_string()
        };
        let path = PathBuf::from(&expanded);
        if path.is_dir() {
            self.newproj_open = false;
            cx.emit(ProjectNav::OpenPath(path));
            return;
        }
        // a GitHub URL (or owner/repo) is cloned into ~/dev and opened
        if let Some((url, name)) = parse_repo_ref(raw) {
            self.newproj_open = false;
            self.clone_and_open(url, name, cx);
            return;
        }
        // not a folder or a repo → leave the dialog open so the path can be fixed
    }

    /// Clone `url` into `~/dev/<name>` and open it. An existing folder of that
    /// name is opened as-is rather than cloned over.
    fn clone_and_open(&mut self, url: String, name: String, cx: &mut Context<Self>) {
        let dest = clone_root().join(&name);
        if dest.is_dir() {
            self.show_flash(&format!("{name} is already in ~/dev — opening it"), cx);
            cx.emit(ProjectNav::OpenPath(dest));
            return;
        }
        self.show_flash(&format!("Cloning {name} into ~/dev…"), cx);
        cx.spawn(async move |this, cx| {
            let (u, d) = (url.clone(), dest.clone());
            let ok = cx
                .background_executor()
                .spawn(async move {
                    let _ = std::fs::create_dir_all(clone_root());
                    Command::new("git")
                        .args(["clone", &u, &d.to_string_lossy()])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if ok {
                    this.show_flash(&format!("Cloned {name}"), cx);
                    cx.emit(ProjectNav::OpenPath(dest));
                } else {
                    this.show_flash(&format!("Could not clone {url}"), cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Choose…" button: open the native macOS folder picker and fill the field.
    fn newproj_choose(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let path = cx
                .background_executor()
                .spawn(async {
                    let out = Command::new("osascript")
                        .arg("-e")
                        .arg("POSIX path of (choose folder with prompt \"Choose Project Folder\")")
                        .output()
                        .ok()?;
                    if !out.status.success() {
                        return None;
                    }
                    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    (!p.is_empty()).then_some(p)
                })
                .await;
            if let Some(p) = path {
                this.update(cx, |this, cx| {
                    // strip the trailing slash AppleScript adds to folder paths
                    this.newproj_path.set(p.trim_end_matches('/').to_string());
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn commit_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => self.focus_active(window, cx),
            "enter" if ks.modifiers.platform => self.do_commit(false, window, cx),
            _ => {
                Self::field_input(&mut self.commit_msg, ks, cx, |_| true);
            }
        }
        cx.notify();
    }

    /// cmd+k: jump to the commit pane and pre-select the active file.
    fn act_goto_commit(&mut self, _: &GotoCommit, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, cx);
        self.left_view = LeftView::Changes;
        self.show_left = true;
        self.commit_checked.clear();
        if let Some(p) = self.active_path().cloned() {
            if self.git_status.contains_key(&p) {
                self.commit_checked.insert(p);
            }
        }
        window.focus(&self.commit_focus, cx);
        cx.notify();
    }

    fn commit_filter_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" {
            self.commit_filter.clear();
            cx.notify();
            return;
        }
        Self::field_input(&mut self.commit_filter, &ev.keystroke, cx, |_| true);
        cx.notify();
    }

    fn pr_filter_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" {
            self.pr_filter.clear();
            cx.notify();
            return;
        }
        Self::field_input(&mut self.pr_filter, &ev.keystroke, cx, |_| true);
        cx.notify();
    }

    fn toggle_checked(&mut self, path: PathBuf) {
        if !self.commit_checked.remove(&path) {
            self.commit_checked.insert(path);
        }
    }

    /// Toggle a group of files together: if all are checked, uncheck all;
    /// otherwise check all. Used by the "Changes" and folder checkboxes.
    fn toggle_checked_all(&mut self, paths: &[PathBuf]) {
        let all = !paths.is_empty() && paths.iter().all(|p| self.commit_checked.contains(p));
        if all {
            for p in paths {
                self.commit_checked.remove(p);
            }
        } else {
            for p in paths {
                self.commit_checked.insert(p.clone());
            }
        }
    }

    fn toggle_collapsed(&mut self, key: PathBuf) {
        if !self.commit_collapsed.remove(&key) {
            self.commit_collapsed.insert(key);
        }
    }

    // ── diff ────────────────────────────────────────────────────────────────

    fn act_show_diff(&mut self, _: &ShowDiff, _window: &mut Window, cx: &mut Context<Self>) {
        // in the Push dialog, cmd+d shows the selected file's diff for the
        // about-to-be-pushed range (the global keybinding fires before the
        // dialog's own key handler, so route it here)
        if self.push_open {
            if let Some(p) = self.push_selected.clone() {
                self.push_open_diff(p, cx);
            }
            return;
        }
        // in the PR pane, cmd+d shows the selected file's PR diff (base..HEAD)
        if self.left_view == LeftView::PullRequest {
            if let Some(p) = self.pr_selected.clone() {
                self.open_pr_diff(p, _window, cx);
            }
            return;
        }
        // otherwise: working-tree diff of the changed files, in a separate window.
        // Prefer the file selected in the Changes (git) panel; fall back to the
        // active editor tab.
        let focus = self
            .commit_selected
            .clone()
            .filter(|p| p.is_file())
            .or_else(|| self.active_path().cloned());
        self.open_working_diff(focus, cx);
    }

    /// Open a working-tree diff window over all changed files, focused on
    /// `focus` (a specific changed file) when given.
    fn open_working_diff(&mut self, focus: Option<PathBuf>, cx: &mut Context<Self>) {
        let mut files: Vec<PathBuf> = self.git_status.keys().cloned().collect();
        if let Some(a) = &focus {
            if !files.contains(a) {
                files.push(a.clone());
            }
        }
        files.sort();
        if files.is_empty() {
            return;
        }
        let idx = focus.and_then(|p| files.iter().position(|f| f == &p)).unwrap_or(0);
        // carry the Changes-pane filter text when that's the pane in use
        let filter_text = if self.left_view == LeftView::Changes {
            self.commit_filter.text.clone()
        } else {
            String::new()
        };
        self.open_diff_window(files, idx, None, None, false, filter_text, PrViewFilter::All, cx);
    }

    /// Open a diff in its own window. `old` Some → committed diff of `old` vs
    /// `new_rev` (default HEAD); `old` None → working-tree diff (HEAD vs disk).
    fn open_diff_window(
        &mut self,
        files: Vec<PathBuf>,
        idx: usize,
        old: Option<String>,
        new_rev: Option<String>,
        pr_mode: bool,
        // carried over from the source pane so the diff opens already filtered:
        // `filter_text` seeds the sidebar path filter, `pr_view` the viewed chips
        filter_text: String,
        pr_view: PrViewFilter,
        cx: &mut Context<Self>,
    ) {
        // guard: diffing a huge file (e.g. a JSON db export) synchronously
        // computes + syntax-highlights the whole thing and freezes the UI
        const MAX_DIFF_BYTES: u64 = 2_000_000;
        if let Some(p) = files.get(idx) {
            if std::fs::metadata(p).map(|m| m.len() > MAX_DIFF_BYTES).unwrap_or(false) {
                self.show_flash("File too large to diff (>2 MB)", cx);
                return;
            }
        }
        let pr_viewed = if pr_mode { self.pr_viewed.clone() } else { HashSet::new() };
        // added/modified/deleted colours for the sidebar: the PR's own file states
        // in review mode, the working tree's otherwise
        let states: HashMap<PathBuf, GitState> = if pr_mode {
            self.pr_files.iter().cloned().collect()
        } else {
            self.git_status.clone()
        };
        let root = self.root.clone();
        let storm = cx.entity().downgrade();
        let main = cx.active_window();
        // 1.5× the old 1660×820 default, clamped to the display so it can't open
        // bigger than the screen on a laptop
        let (mut w, mut h) = (2490.0_f32, 1230.0_f32);
        if let Some(d) = cx.primary_display() {
            let b = d.bounds();
            w = w.min(f32::from(b.size.width) * 0.95);
            h = h.min(f32::from(b.size.height) * 0.95);
        }
        let bounds = Bounds::centered(None, size(px(w), px(h)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Diff".into()),
                    ..Default::default()
                }),
                focus: true,
                ..Default::default()
            },
            move |_, cx| {
                cx.new(|cx| {
                    DiffWindow::new(
                        root, files, idx, old, new_rev, pr_mode, pr_viewed, states, filter_text,
                        pr_view, storm, main, cx,
                    )
                })
            },
        )
        .ok();
    }

    // ── pull-request review ──────────────────────────────────────────────────

    /// Re-fetch the current branch's PR link off the main thread and update the
    /// topbar chip. Used by cmd+R so a freshly-created PR shows up without a
    /// branch switch (the 2s poll only refetches when the branch changes).
    fn refresh_pr_link(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let branch = self.branch.clone();
        cx.spawn(async move |this, cx| {
            let fetch_branch = branch.clone();
            let link = cx
                .background_executor()
                .spawn(async move { fetch_pr_link(&root, &fetch_branch) })
                .await;
            this.update(cx, |this, cx| {
                this.pr_link = link;
                this.pr_link_branch = branch;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Load the PR's changed files (base..HEAD) off the main thread.
    fn load_pr(&mut self, cx: &mut Context<Self>) {
        self.pr_gen += 1;
        self.pr_loading = true;
        let gen = self.pr_gen;
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let (base, files, node_id, viewed, stale) = cx
                .background_executor()
                .spawn(async move {
                    let meta = gh_pr_meta(&root);
                    let base = meta.as_ref().map(|m| m.base.clone()).unwrap_or_else(|| git_pr_base(&root));
                    let files = git_pr_files(&root, &base);
                    let node_id = match meta.as_ref().map(|m| m.node_id.clone()).filter(|s| !s.is_empty()) {
                        Some(id) => Some(id),
                        None => gh_pr_node_id(&root),
                    };
                    // restore the "viewed" state from the actual PR on GitHub
                    let viewed = gh_pr_viewed_files(&root);
                    let stale = meta.as_ref().and_then(|m| pr_stale_note(&root, m, files.len()));
                    (base, files, node_id, viewed, stale)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.pr_gen == gen {
                    this.pr_base = base;
                    this.pr_files = files;
                    this.pr_stale = stale;
                    this.pr_node_id = node_id;
                    let mut viewed = viewed;
                    // keep marks gh hasn't caught up with yet
                    if let Ok(pending) = this.pr_viewed_pending.lock() {
                        for (path, want) in pending.iter() {
                            if *want {
                                viewed.insert(path.clone());
                            } else {
                                viewed.remove(path);
                            }
                        }
                    }
                    this.pr_viewed = viewed;
                    this.pr_loading = false;
                    // an agent's batch that beat the file list here
                    for batch in std::mem::take(&mut this.pr_viewed_queued) {
                        this.apply_agent_viewed(&batch, true, cx);
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// A `/viewed` batch from the agent: the same thing as ticking the boxes by
    /// hand, GitHub mirroring included. Paths are matched against the files the
    /// PR actually touches, however the agent wrote them (repo-relative,
    /// absolute, `./`-prefixed), and anything that isn't in the PR is reported
    /// back rather than silently dropped.
    fn apply_agent_viewed(&mut self, batch: &rpc::Viewed, retry: bool, cx: &mut Context<Self>) {
        if self.pr_files.is_empty() {
            // the pane may never have been opened this session: load the PR and
            // let the batch land when its file list arrives. A batch that has
            // already waited for one load is dropped — there's no PR to mark.
            if retry {
                self.show_flash("Agent marked files viewed, but no PR is loaded", cx);
                return;
            }
            self.pr_viewed_queued.push(rpc::Viewed {
                paths: batch.paths.clone(),
                viewed: batch.viewed,
            });
            if !self.pr_loading {
                self.load_pr(cx);
            }
            return;
        }
        let mut hit: Vec<PathBuf> = Vec::new();
        let mut missed = 0usize;
        for want in &batch.paths {
            let want = want.trim().trim_start_matches("./");
            let abs = self
                .pr_files
                .iter()
                .map(|(p, _)| p)
                .find(|p| {
                    let rel = self.rel(p);
                    rel == want || p.to_string_lossy() == want || rel.ends_with(&format!("/{want}"))
                })
                .cloned();
            match abs {
                Some(p) if !hit.contains(&p) => hit.push(p),
                Some(_) => {}
                None => missed += 1,
            }
        }
        for p in &hit {
            if batch.viewed {
                self.pr_viewed.insert(p.clone());
            } else {
                self.pr_viewed.remove(p);
            }
        }
        self.push_pr_viewed(&hit);
        let verb = if batch.viewed { "viewed" } else { "unviewed" };
        let msg = match (hit.len(), missed) {
            (0, m) => format!("Agent: none of those {m} files are in this PR"),
            (n, 0) => format!("Agent marked {n} file(s) {verb}"),
            (n, m) => format!("Agent marked {n} file(s) {verb} · {m} not in this PR"),
        };
        self.show_flash(&msg, cx);
        cx.notify();
    }

    fn toggle_pr_viewed(&mut self, path: PathBuf) {
        if !self.pr_viewed.remove(&path) {
            self.pr_viewed.insert(path.clone());
        }
        self.push_pr_viewed(&[path]);
    }

    /// Directory keys of a file set, as the flattened trees use them (the parent
    /// chain of every file, plus the root).
    fn dir_keys(files: impl Iterator<Item = PathBuf>, root: &Path) -> HashSet<PathBuf> {
        let mut out = HashSet::new();
        out.insert(PathBuf::from(""));
        for f in files {
            let rel = f.strip_prefix(root).unwrap_or(&f).to_path_buf();
            let mut cur = PathBuf::new();
            for comp in rel.parent().unwrap_or(Path::new("")).components() {
                cur = cur.join(comp);
                out.insert(cur.clone());
            }
        }
        out
    }

    /// Collapse or expand every directory in the PR tree.
    fn pr_collapse_all(&mut self, collapse: bool, cx: &mut Context<Self>) {
        self.pr_collapsed = if collapse {
            Self::dir_keys(self.pr_files.iter().map(|(p, _)| p.clone()), &self.root)
        } else {
            HashSet::new()
        };
        cx.notify();
    }

    /// Collapse or expand every directory in the Changes tree.
    fn changes_collapse_all(&mut self, collapse: bool, cx: &mut Context<Self>) {
        self.commit_collapsed = if collapse {
            Self::dir_keys(self.git_status.keys().cloned(), &self.root)
        } else {
            HashSet::new()
        };
        cx.notify();
    }

    fn toggle_pr_collapsed(&mut self, key: PathBuf) {
        if !self.pr_collapsed.remove(&key) {
            self.pr_collapsed.insert(key);
        }
    }

    /// Mark a group of files viewed/unviewed together (folder + root checkboxes).
    fn toggle_pr_viewed_all(&mut self, paths: &[PathBuf]) {
        let all = !paths.is_empty() && paths.iter().all(|p| self.pr_viewed.contains(p));
        if all {
            for p in paths {
                self.pr_viewed.remove(p);
            }
        } else {
            for p in paths {
                self.pr_viewed.insert(p.clone());
            }
        }
        self.push_pr_viewed(paths);
    }

    /// Mirror the local viewed-state of `paths` onto the GitHub PR (fire-and-forget).
    fn push_pr_viewed(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let Some(pr_id) = self.pr_node_id.clone() else { return };
        let root = self.root.clone();
        // (absolute path, repo-relative path, viewed-now) for each file
        let items: Vec<(PathBuf, String, bool)> = paths
            .iter()
            .map(|p| (p.clone(), self.rel(p), self.pr_viewed.contains(p)))
            .collect();
        if let Ok(mut pending) = self.pr_viewed_pending.lock() {
            for (abs, _, viewed) in &items {
                pending.insert(abs.clone(), *viewed);
            }
        }
        let pending = self.pr_viewed_pending.clone();
        std::thread::spawn(move || {
            for (abs, path, viewed) in items {
                if gh_set_file_viewed(&root, &pr_id, &path, viewed) {
                    // confirmed: gh is now the source of truth for this file
                    if let Ok(mut p) = pending.lock() {
                        if p.get(&abs) == Some(&viewed) {
                            p.remove(&abs);
                        }
                    }
                }
            }
        });
    }

    /// Open the base..HEAD diff for a PR file in a separate window.
    /// Fetch the PR's base branch and reload the file list. The only fix for a
    /// stale `origin/<base>`; no working tree is touched.
    fn fetch_pr_base(&mut self, cx: &mut Context<Self>) {
        let (root, base) = (self.root.clone(), self.pr_base.clone());
        if base.is_empty() {
            return;
        }
        self.pr_loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let b = base.clone();
            let ok = cx.background_executor().spawn(async move { git_fetch_base(&root, &b) }).await;
            let _ = this.update(cx, |this, cx| {
                this.show_flash(
                    &if ok { format!("Fetched origin/{base}") } else { format!("Could not fetch origin/{base}") },
                    cx,
                );
                this.load_pr(cx);
            });
        })
        .detach();
    }

    fn open_pr_diff(&mut self, path: PathBuf, _window: &mut Window, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = self.pr_files.iter().map(|(p, _)| p.clone()).collect();
        let Some(idx) = files.iter().position(|p| p == &path) else { return };
        let old = Some(diff_base_rev(&self.root, &self.pr_base));
        // carry the PR pane's path filter + viewed-state chip into the diff window
        self.open_diff_window(files, idx, old, None, true, self.pr_filter.text.clone(), self.pr_view_filter, cx);
    }

    /// PR pane keys: F4 opens the selected file in a tab, Enter shows its diff,
    /// cmd+r refetches the PR data (handy after pushing).
    fn pr_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // the filter bar is nested inside this focusable, so its keystrokes
        // bubble up here too — bail out or every typed char is handled twice
        // (once by pr_filter_key, once here) and gets inserted twice
        if self.pr_filter_focus.is_focused(window) {
            return;
        }
        let ks = &ev.keystroke;
        if ks.modifiers.platform && ks.key == "r" {
            self.load_pr(cx);
            self.refresh_pr_link(cx); // also re-sync the topbar PR link
            return;
        }
        // alt+c: comment on the selected file as a whole (same as right-click)
        if ks.modifiers.alt && !ks.modifiers.platform && ks.key == "c" {
            if let Some(p) = self.pr_selected.clone().filter(|p| p.is_file()) {
                self.begin_comment(p, 0, 0, window, cx);
            }
            return;
        }
        // cmd+shift+c: copy the selected file/folder's repo-relative reference
        if ks.modifiers.platform && ks.modifiers.shift && ks.key == "c" {
            if let Some(p) = self.pr_selected.clone() {
                self.copy_reference(&p, cx);
            }
            return;
        }
        match ks.key.as_str() {
            "f4" => {
                if let Some(p) = self.pr_selected.clone() {
                    if p.is_file() {
                        self.open_file(p, window, cx);
                    }
                }
            }
            "enter" => {
                if let Some(p) = self.pr_selected.clone() {
                    self.open_pr_diff(p, window, cx);
                }
            }
            "escape" => {
                if !self.pr_filter.is_empty() {
                    self.pr_filter.clear();
                    cx.notify();
                }
            }
            _ => {
                // typing while the list is focused fills the filter
                Self::field_input(&mut self.pr_filter, ks, cx, |_| true);
                cx.notify();
            }
        }
    }

    // ── find in files ────────────────────────────────────────────────────────

    fn act_find_in_files(&mut self, _: &FindInFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, cx);
        // scope to the shift-selected paths, else the single selected folder,
        // else the whole project
        self.find_scope = if !self.tree_multi.is_empty() {
            let mut v: Vec<PathBuf> = self.tree_multi.iter().cloned().collect();
            v.sort();
            v
        } else {
            match &self.tree_selected {
                Some(p) if p.is_dir() => vec![p.clone()],
                _ => vec![self.root.clone()],
            }
        };
        self.find_open = true;
        self.find_query.clear();
        self.find_results.clear();
        self.find_selected = 0;
        self.find_psel = None;
        // measure the monospace advance for preview text selection (Menlo 13px)
        let run = TextRun {
            len: 1,
            font: font("Menlo"),
            color: rgb(TEXT).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        self.find_char_w =
            f32::from(window.text_system().shape_line("0".into(), px(13.), &[run], None).width);
        // open centered; resizing then anchors this top-left corner
        let w = self.find_w.clamp(420.0, (self.win_width - 80.0).max(420.0));
        let h = self.find_h.clamp(300.0, (self.win_height - 120.0).max(300.0));
        self.find_left = ((self.win_width - w) / 2.0).max(0.);
        self.find_top = ((self.win_height - h) / 2.0).max(40.);
        window.focus(&self.find_focus, cx);
        cx.notify();
    }

    // ── push dialog ───────────────────────────────────────────────────────

    /// cmd+shift+k: open the "Push Commits" dialog — the commits and files that
    /// would be pushed to the branch's upstream.
    fn act_push_dialog(&mut self, _: &PushDialog, window: &mut Window, cx: &mut Context<Self>) {
        let branch = self.branch.clone();
        // diff/range against the upstream when set, else origin/<branch>
        let target = git_upstream(&self.root).unwrap_or_else(|| format!("origin/{branch}"));
        let base_ref = resolve_base_ref(&self.root, target.strip_prefix("origin/").unwrap_or(&target));
        let range = format!("{base_ref}..HEAD");
        self.push_branch = branch;
        self.push_target = target;
        self.push_commits = git_log_range(&self.root, &range);
        self.push_files = git_range_files(&self.root, &range);
        self.push_base_ref = base_ref;
        self.push_collapsed.clear();
        self.push_selected = None;
        self.push_commit_sel = None;
        self.push_open = true;
        window.focus(&self.push_focus, cx);
        cx.notify();
    }

    /// Click a commit in the push dialog: scope the file tree to that commit's
    /// changes (click again, or the branch header, to show the whole range).
    fn push_select_commit(&mut self, sha: Option<String>, cx: &mut Context<Self>) {
        // toggle off if the same commit is clicked again
        self.push_commit_sel = if self.push_commit_sel == sha { None } else { sha };
        self.push_files = match &self.push_commit_sel {
            Some(s) => git_commit_files(&self.root, s),
            None => git_range_files(&self.root, &format!("{}..HEAD", self.push_base_ref)),
        };
        self.push_selected = None;
        cx.notify();
    }

    /// Open the diff for a file selected in the push dialog. When a single
    /// commit is selected, show just that commit's change (sha^..sha);
    /// otherwise the whole to-be-pushed range (base..HEAD).
    fn push_open_diff(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let mut files: Vec<PathBuf> = self.push_files.iter().map(|(p, _)| p.clone()).collect();
        if files.is_empty() {
            return;
        }
        if !files.contains(&path) {
            files.push(path.clone());
        }
        let idx = files.iter().position(|f| f == &path).unwrap_or(0);
        let (old, new_rev) = match &self.push_commit_sel {
            Some(sha) => (Some(format!("{sha}^")), Some(sha.clone())),
            None => (Some(diff_base_rev(&self.root, &self.push_base_ref)), None),
        };
        self.open_diff_window(files, idx, old, new_rev, false, String::new(), PrViewFilter::All, cx);
    }

    fn do_push(&mut self, cx: &mut Context<Self>) {
        self.push_open = false;
        // -u origin HEAD so a brand-new branch (no upstream yet) pushes + tracks
        let cmd = if self.push_tags {
            "git push --tags && git push -u origin HEAD"
        } else {
            "git push -u origin HEAD"
        };
        self.run_command(cmd.into(), cx);
        cx.notify();
    }

    fn push_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.push_open = false;
                cx.notify();
            }
            "enter" if !ks.modifiers.platform => {
                if let Some(p) = self.push_selected.clone() {
                    self.push_open_diff(p, cx);
                } else {
                    self.do_push(cx);
                }
            }
            // cmd+d is handled by the global ShowDiff action (act_show_diff),
            // which routes to the selected file while this dialog is open
            _ => {}
        }
    }

    // ── git popup ─────────────────────────────────────────────────────────

    fn act_git_popup(&mut self, _: &GitPopup, window: &mut Window, cx: &mut Context<Self>) {
        self.gitp_branches = git_branches(&self.root);
        self.gitp_local = git_local_branches(&self.root);
        self.gitp_open = true;
        self.gitp_query.clear();
        self.gitp_sel = 0;
        self.gitp_action_branch = None;
        self.gitp_action_sel = 0;
        window.focus(&self.gitp_focus, cx);
        cx.notify();
    }

    /// Build the filtered action+branch list shown in the git popup.
    fn gitp_items(&self) -> Vec<GitItem> {
        let q = self.gitp_query.text.to_lowercase();
        let mut items = Vec::new();
        let actions = [
            (GitAction::Update, "Update Project", IC_HOME),
            (GitAction::Commit, "Commit", IC_COMMIT),
            (GitAction::Push, "Push", IC_PUSH),
            (GitAction::Pr, "Open Pull Request", IC_PR),
            (GitAction::CreatePr, "Create Pull Request", IC_PR),
            (GitAction::NewBranch, "New Branch", IC_BRANCH),
        ];
        for (a, label, icon) in actions {
            if q.is_empty() || fuzzy_score(&q, &label.to_lowercase()).is_some() {
                items.push(GitItem::Action(a, label, icon));
            }
        }
        // a bare number is a PR: offer it first, since that's clearly the intent
        if let Ok(n) = q.trim().parse::<u64>() {
            if n > 0 {
                items.push(GitItem::Pr(n));
            }
        }
        // branches: fuzzy-filtered (so "orinext" finds "origin/next"), best
        // matches first; committerdate order when there's no query
        if q.is_empty() {
            for b in &self.gitp_branches {
                items.push(GitItem::Branch(b.clone()));
            }
        } else {
            let mut scored: Vec<(i32, &String)> = self
                .gitp_branches
                .iter()
                .filter_map(|b| fuzzy_score(&q, &b.to_lowercase()).map(|s| (s, b)))
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0)); // stable → committerdate order on ties
            for (_, b) in scored {
                items.push(GitItem::Branch(b.clone()));
            }
        }
        items
    }

    /// `gh pr checkout <n>` — it resolves the head branch, fetches it, and
    /// switches, including for forks. Runs in the console like any other job.
    fn checkout_pr(&mut self, n: u64, cx: &mut Context<Self>) {
        self.run_command(format!("gh pr checkout {n}"), cx);
    }

    fn gitp_execute(&mut self, item: GitItem, window: &mut Window, cx: &mut Context<Self>) {
        // a branch doesn't act directly — it opens the per-branch action submenu
        if let GitItem::Branch(name) = item {
            self.gitp_action_branch = Some(name);
            self.gitp_action_sel = 0;
            cx.notify();
            return;
        }
        self.gitp_open = false;
        // don't leave focus on the dialog we just closed (see palette_execute)
        self.focus_active(window, cx);
        match item {
            GitItem::Action(GitAction::Update, ..) => self.run_command("git pull".into(), cx),
            GitItem::Action(GitAction::Commit, ..) => {
                self.left_view = LeftView::Changes;
                self.show_left = true;
                window.focus(&self.commit_focus, cx);
                cx.notify();
            }
            GitItem::Action(GitAction::Push, ..) => self.run_command("git push -u origin HEAD".into(), cx),
            GitItem::Action(GitAction::Pr, ..) => self.run_command("pro".into(), cx),
            GitItem::Action(GitAction::CreatePr, ..) => self.open_pr_create_prompt(window, cx),
            GitItem::Action(GitAction::NewBranch, ..) => self.open_branch_prompt(window, cx),
            GitItem::Pr(n) => self.checkout_pr(n, cx),
            GitItem::Branch(_) => {}
        }
    }

    /// Submenu entries for the branch the popup is acting on (display order).
    /// Delete is listed for any local branch — including the current one, where
    /// picking it says why it can't run rather than hiding the option and
    /// leaving you wondering where it went.
    fn gitp_branch_actions(&self) -> Vec<(BranchAction, &'static str, &'static str)> {
        let mut v = vec![
            (BranchAction::Checkout, "Checkout", IC_HOME),
            (BranchAction::Merge, "Merge", IC_BRANCH),
        ];
        let b = self.gitp_action_branch.as_deref().unwrap_or("");
        if self.gitp_local.contains(b) {
            v.push((BranchAction::Delete, "Delete", IC_TRASH));
        }
        v
    }

    /// Run a submenu action against the branch the submenu was opened for.
    fn gitp_run_branch_action(
        &mut self,
        action: BranchAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(branch) = self.gitp_action_branch.take() else { return };
        self.gitp_open = false;
        self.focus_active(window, cx);
        match action {
            BranchAction::Checkout => {
                // if the entry is a remote-tracking ref (origin/x), check out the
                // local name so git creates/uses a tracking branch instead of
                // landing in detached HEAD; local branches check out as-is
                let cmd = format!(
                    "b='{branch}'; if git show-ref --verify --quiet \"refs/remotes/$b\"; \
                     then git checkout \"${{b#*/}}\"; else git checkout \"$b\"; fi"
                );
                self.run_command(cmd, cx);
            }
            BranchAction::Merge => self.run_command(format!("git merge {}", branch), cx),
            // safe delete (-d refuses unmerged branches; git prints why in the console)
            BranchAction::Delete if branch == self.branch => {
                self.show_flash("That's the branch you're on — check out another one first", cx)
            }
            BranchAction::Delete => self.run_command(format!("git branch -d {}", branch), cx),
        }
    }

    fn gitp_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // while the per-branch submenu is open it owns the navigation keys;
        // escape (to back out) is handled by on_root_key so it doesn't also
        // close the whole popup.
        if self.gitp_action_branch.is_some() {
            match ks.key.as_str() {
                "enter" => {
                    if let Some(&(action, ..)) = self.gitp_branch_actions().get(self.gitp_action_sel) {
                        self.gitp_run_branch_action(action, window, cx);
                    }
                }
                "down" => {
                    self.gitp_action_sel =
                        (self.gitp_action_sel + 1).min(self.gitp_branch_actions().len().saturating_sub(1));
                }
                "up" => self.gitp_action_sel = self.gitp_action_sel.saturating_sub(1),
                _ => {}
            }
            cx.notify();
            return;
        }
        match ks.key.as_str() {
            "escape" => {
                self.gitp_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                let items = self.gitp_items();
                if let Some(item) = items.get(self.gitp_sel).cloned() {
                    self.gitp_execute(item, window, cx);
                }
            }
            "down" => {
                let n = self.gitp_items().len();
                self.gitp_sel = (self.gitp_sel + 1).min(n.saturating_sub(1));
            }
            "up" => self.gitp_sel = self.gitp_sel.saturating_sub(1),
            _ => {
                if Self::field_input(&mut self.gitp_query, ks, cx, |_| true) == Edit::Changed {
                    self.gitp_sel = 0;
                }
            }
        }
        cx.notify();
    }

    // ── command palette ───────────────────────────────────────────────────

    /// Copy "relative/path.ts:line" of the active file's cursor to the clipboard.
    /// cmd+shift+c: copy the active file's path (no line number).
    fn act_copy_reference(&mut self, _: &CopyReference, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(self.active) {
            let reference = self.rel(&tab.path);
            cx.write_to_clipboard(ClipboardItem::new_string(reference));
            self.show_flash("Path copied", cx);
        }
    }

    /// cmd+shift+opt+c: copy the active file's path with the cursor line (path:N).
    fn act_copy_reference_line(&mut self, _: &CopyReferenceLine, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(self.active) {
            let line = tab.editor.read(cx).cursor_line();
            let reference = format!("{}:{}", self.rel(&tab.path), line);
            cx.write_to_clipboard(ClipboardItem::new_string(reference));
            self.show_flash("Path + line copied", cx);
        }
    }

    /// Copy `path`'s repo-relative reference to the clipboard and flash a toast.
    fn copy_reference(&mut self, path: &PathBuf, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.rel(path)));
        self.show_flash("Reference copied", cx);
    }

    /// Show a transient bottom-right toast that auto-dismisses after 2s.
    fn show_flash(&mut self, msg: &str, cx: &mut Context<Self>) {
        self.flash = Some(msg.to_string());
        self.flash_gen += 1;
        let gen = self.flash_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(2000)).await;
            this.update(cx, |this, cx| {
                if this.flash_gen == gen {
                    this.flash = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Commit-pane list keys: cmd+shift+c copies the selected file/folder ref.
    /// Open a file picked in the changes pane and collapse the pane (so the
    /// editor takes the space), like dismissing a dialog.
    /// Open a file from the Changes pane. `close_pane` is for Enter (jump away to
    /// work on it); F4 keeps the pane open so you can keep walking the list, the
    /// same way the PR pane behaves.
    fn open_commit_file(&mut self, path: PathBuf, close_pane: bool, window: &mut Window, cx: &mut Context<Self>) {
        if close_pane {
            self.show_left = false;
        }
        self.open_file(path, window, cx);
    }

    /// Re-scan git working-tree status off the main thread and refresh the
    /// Changes pane (and the file-tree's git colors). Bound to cmd+r there.
    fn refresh_changes(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let status = cx.background_executor().spawn(async move { compute_git(&root) }).await;
            this.update(cx, |this, cx| {
                this.git_status = status;
                cx.notify();
            })
            .ok();
        })
        .detach();
        self.show_flash("Refreshed changes", cx);
    }

    fn changes_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.modifiers.platform && ks.modifiers.shift && ks.key == "c" {
            if let Some(p) = self.commit_selected.clone() {
                self.copy_reference(&p, cx);
            }
            return;
        }
        // cmd+r: re-scan git status (like the PR pane's refresh)
        if ks.modifiers.platform && ks.key == "r" {
            self.refresh_changes(cx);
            return;
        }
        if ks.modifiers.platform {
            return; // leave other cmd combos alone
        }
        if ks.key == "escape" {
            if !self.commit_filter.is_empty() {
                self.commit_filter.clear();
                cx.notify();
            }
            return;
        }
        // F4 opens the file and stays; Enter opens it and gets out of the way
        if (ks.key == "f4" || ks.key == "enter") && !ks.modifiers.shift {
            let close_pane = ks.key == "enter";
            if let Some(p) = self.commit_selected.clone() {
                if p.is_file() {
                    self.open_commit_file(p, close_pane, window, cx);
                }
            }
            return;
        }
        // delete the selected file/folder (backspace only when the filter is
        // empty, so it can still edit the filter text); forward-delete always
        if (ks.key == "backspace" && self.commit_filter.is_empty()) || ks.key == "delete" {
            if let Some(p) = self.commit_selected.clone() {
                self.confirm_delete = Some(p);
                window.focus(&self.confirm_focus, cx);
                cx.notify();
            }
            return;
        }
        // typing while the list is focused fills the filter (nvim-explorer style)
        Self::field_input(&mut self.commit_filter, ks, cx, |_| true);
        cx.notify();
    }

    /// cmd+shift+h: open the active file (at the cursor line) on GitHub, or the
    /// current branch if no file is open.
    fn act_open_on_github(&mut self, _: &OpenOnGithub, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(base) = github_base_url(&self.root) else { return };
        let branch = if self.branch.is_empty() { "main".to_string() } else { self.branch.clone() };
        let url = if let Some(tab) = self.tabs.get(self.active) {
            let line = tab.editor.read(cx).cursor_line();
            let rel = self.rel(&tab.path);
            format!("{}/blob/{}/{}#L{}", base, branch, rel, line)
        } else {
            format!("{}/tree/{}", base, branch)
        };
        let _ = Command::new("open").arg(url).spawn();
    }

    fn act_command_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, cx);
        self.palette_open = true;
        self.palette_query.clear();
        self.palette_sel = 0;
        self.palette_results = self.palette_items(); // full list, instant on open
        window.focus(&self.palette_focus, cx);
        cx.notify();
    }

    /// Debounced palette re-filter so typing doesn't recompute every keystroke.
    fn schedule_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_gen += 1;
        let gen = self.palette_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(80)).await;
            this.update(cx, |this, cx| {
                if this.palette_gen == gen {
                    this.palette_results = this.palette_items();
                    this.palette_sel = 0;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Fuzzy-filtered, ranked command list.
    fn palette_items(&self) -> Vec<(Cmd, String, &'static str, &'static str)> {
        let admin = admin_url(&self.root);
        let mut scored: Vec<(i32, (Cmd, String, &'static str, &'static str))> = Vec::new();
        for &(c, label, icon, hint) in PALETTE {
            // Open Admin: wby-next* only, and show the target URL in the label
            let label = if c == Cmd::OpenAdmin {
                match &admin {
                    Some(u) => format!("Open Admin ({u})"),
                    None => continue,
                }
            } else {
                label.to_string()
            };
            if let Some(s) = fuzzy_score(&self.palette_query.text, &label) {
                scored.push((s, (c, label, icon, hint)));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, it)| it).collect()
    }

    fn palette_execute(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        // The palette's focus handle belongs to an element that is now gone. A
        // command that doesn't claim focus itself would leave focus dangling,
        // and a dangling focus has no dispatch path — which is how ⌘K stopped
        // working after commands like "Pause Workspace". Park focus somewhere
        // alive first; commands that open their own dialog take it from here.
        self.focus_active(window, cx);
        match cmd {
            Cmd::WipCommit => self.run_command("wip".into(), cx),
            Cmd::WipPush => self.run_command("wipp".into(), cx),
            Cmd::OpenPr => self.run_command("pro".into(), cx),
            Cmd::CreatePr => self.open_pr_create_prompt(window, cx),
            // `yb` builds; `yyb` installs first (`y && yb`)
            Cmd::Build => self.run_command("yb".into(), cx),
            Cmd::InstallBuild => self.run_command("yyb".into(), cx),
            Cmd::BuildPreview => self.refresh_build_preview(cx),
            Cmd::GitAdd => self.run_command("gaa".into(), cx),
            Cmd::Pull => self.run_command("git pull --no-rebase".into(), cx),
            Cmd::Fetch => self.run_command("git fetch --all --prune".into(), cx),
            Cmd::CheckoutNext => self.run_command("next".into(), cx),
            Cmd::NewBranch => self.open_branch_prompt(window, cx),
            Cmd::Commit => self.act_goto_commit(&GotoCommit, window, cx),
            Cmd::ToggleTerminal => self.toggle_terminal(window, cx),
            Cmd::FindInFiles => self.act_find_in_files(&FindInFiles, window, cx),
            Cmd::GoToFile => self.act_open_finder(&OpenFinder, window, cx),
            Cmd::GoToLine => self.act_goto(&GotoLine, window, cx),
            Cmd::GitPopup => self.act_git_popup(&GitPopup, window, cx),
            Cmd::ShowDiff => self.act_show_diff(&ShowDiff, window, cx),
            Cmd::MyPrs => {
                let url = "https://github.com/webiny/webiny-js/pulls?q=sort%3Aupdated-desc+is%3Apr+is%3Aopen+author%3Aadrians5j";
                let _ = Command::new("open").arg(url).spawn();
            }
            Cmd::ReleasePrs => {
                let url = "https://github.com/webiny/webiny-js/pulls?q=sort%3Aupdated-desc+is%3Apr+state%3Aopen+head%3Arelease";
                let _ = Command::new("open").arg(url).spawn();
            }
            Cmd::CopyBranch => {
                if !self.branch.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(self.branch.clone()));
                    self.show_flash("Branch name copied", cx);
                }
            }
            // copy a ready-to-paste Claude Code slash command that renames the
            // session to "⭐ <branch>" — paste it straight into Claude
            Cmd::CopyRenameSession => {
                let branch = if self.branch.is_empty() { "session" } else { &self.branch };
                cx.write_to_clipboard(ClipboardItem::new_string(format!("/rename ⭐ {branch}")));
                self.show_flash("/rename command copied", cx);
            }
            Cmd::ProcessManager => self.open_process_manager(window, cx),
            Cmd::ToggleReadOnly => self.toggle_read_only(cx),
            Cmd::ResolveConflicts => self.open_conflicts(window, cx),
            Cmd::OpenAdmin => {
                if let Some(u) = admin_url(&self.root) {
                    open_browser(&u);
                }
            }
            Cmd::SetWorkspaceTitle => self.open_title_prompt(window, cx),
            Cmd::AddComment => self.open_comment_prompt(window, cx),
            Cmd::ShowComments => self.open_comment_list(window, cx),
            Cmd::SendComments => self.send_review(window, cx),
            Cmd::OpenRepository => self.act_new_project(&NewProject, window, cx),
            Cmd::InstallUsageBars => self.install_usage_bars(cx),
            Cmd::TogglePause => self.toggle_pause(cx),
            Cmd::ResetWorkspace => self.reset_workspace(cx),
            // reset, then hand the workspace back — the git work keeps running in
            // its own thread, so closing here doesn't cancel the checkout/pull
            Cmd::ResetWorkspaceClose => {
                self.reset_workspace(cx);
                cx.emit(ProjectNav::Remove(self.ws_active));
            }
            // reset, then park it: the folder stays open but ⌥Tab skips it until
            // you pick the work up again
            Cmd::ResetWorkspacePause => {
                self.reset_workspace(cx);
                if !self.ws_paused {
                    self.toggle_pause(cx);
                }
            }
        }
    }

    /// Fresh start on a workspace: back to the default branch with everything
    /// pulled (`next`), custom title dropped, and PR state cleared — the branch
    /// we're moving to has no PR yet, so keeping the old chip is just wrong.
    /// Park this workspace: ⌥Tab stops landing on it until it's resumed.
    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        self.ws_paused = !self.ws_paused;
        save_ws_paused(&self.root, self.ws_paused);
        let msg = if self.ws_paused { "Workspace paused — ⌥Tab skips it" } else { "Workspace resumed" };
        self.show_flash(msg, cx);
        cx.notify();
    }

    fn reset_workspace(&mut self, cx: &mut Context<Self>) {
        self.ws_title = None;
        save_ws_title(&self.root, None);
        self.ws_client = None;
        save_ws_client(&self.root, None);
        self.title_type = None;
        // reports describe work on the branch we're leaving — drop them and mark
        // the workspace idle (persisted, so other TIDE instances see it too)
        self.agent_states.clear();
        // the conversation was about the work being reset: start the agent fresh
        // rather than carrying it into the next piece of work
        if self.acp.is_some() {
            self.new_agent_session(cx);
        }
        self.agent_resumed = true; // don't auto-resume the old session either
        // …and don't resume it after a restart, whether or not the adapter was
        // up when the reset happened
        self.mark_agent_fresh(true);
        self.review.clear();
        self.save_review();
        self.ws_idle = true;
        save_ws_idle(&self.root, true);
        self.pr_link = None;
        self.pr_link_branch.clear(); // forces the git poll to re-query
        self.branch_parent = None;
        self.pr_files.clear();
        self.pr_rows.clear();
        self.pr_viewed.clear();
        self.pr_selected = None;
        // whatever this repo calls its default branch — `next` in webiny-js,
        // `main`/`master` elsewhere — rather than one hardcoded name
        let default_branch = git_default_branch(&self.root);
        // `git reset --hard` first: a reset is meant to hand back a clean slate,
        // and without it a modified file either rides along into the default
        // branch or makes the checkout fail outright. Untracked files are left
        // alone — that's `git clean`, and it eats things git never knew about.
        self.run_command(
            format!("git reset --hard && git checkout {default_branch} && git pull --no-rebase"),
            cx,
        );
        cx.notify();
    }


    // ── review comments ─────────────────────────────────────────────────────

    /// Open the comment prompt for the caret's line (or the selected span) in the
    /// active editor. Editing an existing comment on the same span reuses it.
    fn open_comment_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(self.active) else {
            self.show_flash("Open a file to comment on it", cx);
            return;
        };
        let path = tab.path.clone();
        let (a, b) = tab.editor.read(cx).selected_lines();
        self.begin_comment(path, a, b, window, cx);
    }

    /// Open the box on `path:a-b`. A thread that already has turns opens ready
    /// for the next reply; a fresh line opens ready for the note. Every entry
    /// point (⌥C, the margin chip, the gutter "+") goes through here so they
    /// can't drift apart.
    /// `a == 0` (and `b == 0`) is a whole-file comment: no gutter span, no
    /// snippet, and the box opens centred instead of pinned to a line.
    fn begin_comment(
        &mut self,
        path: PathBuf,
        a: usize,
        b: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = self.review.iter().find(|c| c.path == path && c.line == a && c.end == b);
        let has_thread = found.is_some_and(|c| !c.replies.is_empty());
        let text = if has_thread { String::new() } else { found.map(|c| c.text.clone()).unwrap_or_default() };
        self.rc_reply_mode = has_thread;
        // an existing comment keeps the snippet it was saved with
        if found.is_some() {
            self.rc_snippet = None;
        }
        self.rc_target = Some((path, a, b));
        self.rc_query.multiline = true; // ↩ adds a line, ⌘↩ saves
        self.rc_query.set(text);
        self.rc_open = true;
        self.sync_comment_gutters(cx); // reserve the space the box will sit in
        // a fresh note wants the caret; an existing thread opens for reading
        // first (GitHub-style) — click the reply area to start typing
        if !has_thread {
            window.focus(&self.rc_focus, cx);
        }
        cx.notify();
    }

    /// Shorten a thread turn for the collapsed review row. Whole sentences would
    /// be nicer, but a hard cap is what keeps every row the same order of size.
    fn clip_turn(text: &str) -> String {
        const MAX: usize = 180;
        if text.chars().count() <= MAX {
            return text.to_string();
        }
        format!("{}…", text.chars().take(MAX).collect::<String>().trim_end())
    }

    /// Extra height the user dragged onto the thread area, over what its content
    /// would take. The editor reserves the box's gap, so this has to be added
    /// there too or a resized box overlaps the code below it.
    fn rc_thread_extra(&self, replies: &[ReviewReply]) -> f32 {
        if replies.is_empty() {
            return 0.0; // no thread area on screen to stretch
        }
        self.rc_thread_h
            .map(|h| (h - rc_thread_rows(replies) as f32 * RC_LINE_H).max(0.))
            .unwrap_or(0.)
    }

    fn rc_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => self.cancel_comment(window, cx),
            // ⌘↩ commits; a bare ↩ is a newline (handled by the field below)
            "enter" if ks.modifiers.platform => self.commit_comment(window, cx),
            _ => {
                if Self::text_comes_via_input_handler(ks) {
                    return; // the input handler inserts it
                }
                let edit = Self::field_input(&mut self.rc_query, ks, cx, |_| true);
                if edit == Edit::Changed {
                    self.sync_comment_gutters(cx); // the box grew or shrank
                }
            }
        }
        cx.notify();
    }

    /// Store (or update, or delete when emptied) the comment being written.
    fn save_comment(&mut self, cx: &mut Context<Self>) {
        let Some((path, a, b)) = self.rc_target.clone() else { return };
        let text = self.rc_query.text.trim().to_string();
        let snippet = if let Some(q) = self.rc_snippet.clone() {
            q // quoted from the agent's answer, not read off a file
        } else if a == 0 {
            String::new() // whole-file note: there's no line to quote
        } else {
            self.tabs
                .iter()
                .find(|t| t.path == path)
                .map(|t| t.editor.read(cx).lines_text(a, b))
                .unwrap_or_default()
        };
        self.set_comment(path, a, b, snippet, text, false, cx);
    }

    /// If the comment box is open on the tab about to close, keep its text as a
    /// draft instead of dropping it on the floor.
    fn stash_comment_draft(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get(ix) else { return };
        let Some((path, a, b)) = self.rc_target.clone() else { return };
        if !self.rc_open || path != tab.path {
            return;
        }
        let text = self.rc_query.text.trim().to_string();
        self.rc_open = false;
        if text.is_empty() {
            return;
        }
        let snippet = tab.editor.read(cx).lines_text(a, b);
        self.set_comment(path, a, b, snippet, text, true, cx);
        self.show_flash("Comment kept as draft", cx);
    }

    /// Save the box and close it (⌘↩ / the primary button). In reply mode the
    /// text becomes your turn in the thread and the comment goes back into the
    /// send queue, so ⌥↩ carries the follow-up to the agent.
    fn commit_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rc_open = false;
        if self.rc_reply_mode {
            let text = self.rc_query.text.trim().to_string();
            if let (Some((path, a, b)), false) = (self.rc_target.clone(), text.is_empty()) {
                if let Some(c) = self
                    .review
                    .iter_mut()
                    .find(|c| c.path == path && c.line == a && c.end == b)
                {
                    c.replies.push(ReviewReply { who: "you".into(), text });
                    c.sent = false; // your turn goes out with the next send
                    c.draft = false;
                    c.resolved = false; // replying reopens what you'd filed away
                }
                self.save_review();
                self.show_flash("Reply queued — ⌥↩ sends it", cx);
            }
            self.rc_reply_mode = false;
            self.sync_comment_gutters(cx);
        } else {
            self.save_comment(cx); // also re-syncs gaps + gutter markers
        }
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Done discussing: file the thread away. It leaves the open list and every
    /// count, but stays in the Resolved tab, where it can be reopened.
    fn resolve_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, a, b)) = self.rc_target.clone() else { return };
        self.rc_open = false;
        self.rc_reply_mode = false;
        if let Some(c) =
            self.review.iter_mut().find(|c| c.path == path && c.line == a && c.end == b)
        {
            c.resolved = true;
        }
        self.sync_comment_gutters(cx);
        self.save_review();
        self.show_flash("Thread resolved", cx);
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Close the box, leaving the note as it was (esc / the Cancel button).
    fn cancel_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rc_snippet = None;
        self.rc_width = None;
        self.rc_open = false;
        self.sync_comment_gutters(cx); // give the reserved space back
        self.focus_active(window, cx);
        cx.notify();
    }

    /// Drop the note the open box is editing (⌫ in the pane does the same thing
    /// from the list side).
    fn delete_open_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((path, a, b)) = self.rc_target.clone() else { return };
        self.rc_open = false;
        self.set_comment(path, a, b, String::new(), String::new(), false, cx);
        self.focus_active(window, cx);
    }

    /// Pin `text` to `path:a-b` (empty text removes it). Shared by the editor's
    /// inline box and the diff window, which owns no comment state of its own.
    fn set_comment(
        &mut self,
        path: PathBuf,
        a: usize,
        b: usize,
        snippet: String,
        text: String,
        draft: bool,
        cx: &mut Context<Self>,
    ) {
        // editing a comment keeps its id and thread — only the text changes
        let existing = self
            .review
            .iter()
            .find(|c| c.path == path && c.line == a && c.end == b)
            .map(|c| (c.id.clone(), c.sent, c.replies.clone(), c.text.clone()));
        self.review.retain(|c| !(c.path == path && c.line == a && c.end == b));
        if text.is_empty() {
            self.sync_comment_gutters(cx);
            self.save_review();
            self.show_flash("Comment removed", cx);
            cx.notify();
            return;
        }
        let (id, was_sent, replies, old_text) =
            existing.unwrap_or_else(|| (random_id(), false, Vec::new(), String::new()));
        // an edit to a comment the agent already has is a correction, not a
        // no-op: it goes back into the queue so the next send carries the new
        // wording, instead of the thread quietly disagreeing with what was sent
        let resend = was_sent && old_text != text;
        // writing on a thread you'd filed away means you're not done with it
        let sent = was_sent && !resend;
        self.review.push(ReviewComment {
            id,
            path,
            line: a,
            end: b,
            snippet,
            text,
            draft,
            sent,
            replies,
            resolved: false,
        });
        self.review.sort_by(|x, y| x.path.cmp(&y.path).then(x.line.cmp(&y.line)));
        self.sync_comment_gutters(cx);
        self.save_review();
        if resend {
            self.show_flash("Comment updated — ⌥↩ sends it again", cx);
            cx.notify();
            return;
        }
        let n = self.review.len();
        self.show_flash(&format!("{n} comment{} pending", if n == 1 { "" } else { "s" }), cx);
        cx.notify();
    }

    /// Write the comment list out so an accidental quit doesn't lose it. Called
    /// from every path that mutates `review`.
    fn save_review(&self) {
        let Some(path) = review_path(&self.root) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if self.review.is_empty() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let _ = std::fs::write(path, review_to_json(&self.root, &self.review));
    }

    /// Review comments pinned to `path` (for the per-file markers in the Changes
    /// and PR panes).
    fn comment_count(&self, path: &Path) -> usize {
        self.review.iter().filter(|c| c.path == path && !c.resolved).count()
    }

    /// Tell each open editor which of its lines carry comments (gutter marker)
    /// and how much blank space to insert for the notes and the open input box,
    /// so the code moves down instead of being covered.
    fn sync_comment_gutters(&mut self, cx: &mut Context<Self>) {
        let editing = self.rc_open.then(|| self.rc_target.clone()).flatten();
        for tab in &self.tabs {
            // spans, not individual lines: the editor draws one icon plus a bar
            // down the side of each range
            let mut spans: Vec<(usize, usize)> = self
                .review
                .iter()
                // 0 = whole file; a resolved thread leaves the gutter too
                .filter(|c| c.path == tab.path && c.line > 0 && !c.resolved)
                .map(|c| (c.line, c.end))
                .collect();
            spans.sort();
            // only the open box needs space carved out; saved notes sit in the
            // right margin beside their line, so they push nothing down
            let mut gaps: Vec<(usize, f32)> = Vec::new();
            if let Some((p, a, b)) = &editing {
                if p == &tab.path && *a > 0 {
                    // the box's own line: stack it under any note already there
                    let replies = self
                        .review
                        .iter()
                        .find(|c| &c.path == p && c.line == *a && c.end == *b)
                        .map(|c| c.replies.clone())
                        .unwrap_or_default();
                    let h = rc_gap_h(&self.rc_query.text, &replies) + self.rc_thread_extra(&replies);
                    match gaps.iter_mut().find(|(l, _)| l == b) {
                        Some(g) => g.1 += h,
                        None => gaps.push((*b, h)),
                    }
                }
            }
            gaps.sort_by_key(|(l, _)| *l);
            // the span being written keeps its bar until it's saved or dropped
            let pending = editing
                .as_ref()
                .filter(|(p, _, _)| p == &tab.path)
                .map(|(_, a, b)| (*a, *b));
            tab.editor.update(cx, |e, cx| {
                if e.comment_spans != spans || e.inline_gaps != gaps || e.pending_span != pending {
                    e.comment_spans = spans;
                    e.inline_gaps = gaps;
                    e.pending_span = pending;
                    cx.notify();
                }
            });
        }
    }

    /// The pane to hand the comments to: one running an agent (`claude`, `cc`) in
    /// the active terminal tab, else that tab's focused pane.
    fn agent_pane(&self, _cx: &App) -> Option<Entity<Terminal>> {
        let tab = self.term_tabs.get(self.active_term)?;
        let panes = tab.terminals();
        panes
            .iter()
            .find(|t| {
                self.pane_cmds
                    .get(&t.entity_id().as_u64())
                    .is_some_and(|c| c.starts_with("claude") || c.starts_with("cc"))
            })
            .cloned()
            .or_else(|| Some(tab.active_terminal().clone()))
            .filter(|_| !panes.is_empty())
    }

    /// The message handed to the agent: a numbered list of `path:line` + the code
    /// as quoted context + the note.
    fn review_message(&self) -> String {
        let items: Vec<ReviewItem> = self
            .review
            .iter()
            .filter(|c| !c.draft && !c.sent)
            .map(|c| {
                let reference = if is_agent_src(&c.path) {
                    "your own earlier answer (quoted below)".to_string()
                } else if c.line == 0 {
                    format!("{} (whole file)", self.rel(&c.path))
                } else if c.end > c.line {
                    format!("{}:{}-{}", self.rel(&c.path), c.line, c.end)
                } else {
                    format!("{}:{}", self.rel(&c.path), c.line)
                };
                ReviewItem {
                    id: c.id.clone(),
                    reference,
                    snippet: c.snippet.clone(),
                    note: c.text.clone(),
                    thread: c.replies.iter().map(|r| (r.who.clone(), r.text.clone())).collect(),
                }
            })
            .collect();
        format_review(&items, &self.review_note.text)
    }

    /// Type the comments into the agent's pane and submit them.
    fn send_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ready = self.review.iter().filter(|c| !c.draft && !c.sent && !c.resolved).count();
        let note = self.review_note.text.trim().to_string();
        if ready == 0 && note.is_empty() {
            let msg = if self.review.is_empty() {
                "No comments to send"
            } else {
                "Only drafts — open one and press Comment to make it ready"
            };
            self.show_flash(msg, cx);
            return;
        }
        let msg = self.review_message();
        // the agent pane is the agent now; a terminal pane is the fallback for
        // when the ACP adapter can't run (no npx on PATH)
        self.show_terminal = true;
        if AGENT_PANEL {
            self.agent_open = true;
            self.ensure_acp(cx);
        }
        let sent_to_panel = AGENT_PANEL && self.acp.is_some();
        let pane = if sent_to_panel { None } else { self.agent_pane(cx) };
        if sent_to_panel {
            // the transcript gets a receipt; the agent gets the full batch
            let mut label = if ready == 0 {
                "Posted a review note".to_string()
            } else {
                format!("Posted {ready} review comment{}", if ready == 1 { "" } else { "s" })
            };
            if !note.is_empty() && ready > 0 {
                label.push_str(" + a note");
            }
            for c in self.review.iter().filter(|c| !c.draft && !c.sent && !c.resolved) {
                let where_ = if is_agent_src(&c.path) {
                    "your earlier answer".to_string()
                } else if c.line == 0 {
                    format!("{} (whole file)", self.rel(&c.path))
                } else if c.end > c.line {
                    format!("{}:{}-{}", self.rel(&c.path), c.line, c.end)
                } else {
                    format!("{}:{}", self.rel(&c.path), c.line)
                };
                label.push_str(&format!("\n• {where_}"));
            }
            self.send_agent_prompt_as(msg, Some(label), cx);
        } else if let Some(pane) = pane.clone() {
            pane.read(cx).send_paste(&msg);
            pane.read(cx).send_text("\r");
        } else {
            self.show_flash("Nowhere to send: no agent pane and no terminal", cx);
            return;
        }
        let n = ready;
        let drafts = self.review.iter().filter(|c| c.draft && !c.resolved).count();
        // the comments stay: each one is a thread now, waiting for its answer
        for c in self.review.iter_mut().filter(|c| !c.draft && !c.resolved) {
            c.sent = true;
        }
        self.review_note.clear();
        self.sync_comment_gutters(cx);
        self.save_review();
        match pane {
            Some(pane) => self.focus_pane(&pane, window, cx),
            None => self.focus_agent(window, cx),
        }
        let note = if drafts > 0 { format!(" · {drafts} draft(s) kept") } else { String::new() };
        self.show_flash(&format!("Sent {n} comment{}{note}", if n == 1 { "" } else { "s" }), cx);
        cx.notify();
    }

    /// Flip the active markdown tab between rendered and raw.
    fn toggle_md_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        if !is_markdown(&tab.path) {
            self.show_flash("Not a markdown file", cx);
            return;
        }
        tab.preview = !tab.preview;
        if !tab.preview {
            self.focus_active(window, cx); // back to the editor, caret and all
        }
        cx.notify();
    }

    fn act_md_preview(&mut self, _: &ToggleMdPreview, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_md_preview(window, cx);
    }

    /// Show the review-comments pane in the left dock (⌥⇧C / the activity icon).
    fn open_comment_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.left_view = LeftView::Review;
        self.show_left = true;
        self.focus_active(window, cx);
        cx.notify();
    }

    // ── merge conflicts ─────────────────────────────────────────────────────────

    fn open_conflicts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let files = conflicted_files(&self.root);
        if files.is_empty() {
            self.show_flash("No merge conflicts", cx);
            return;
        }
        self.mc_files = files;
        self.mc_into = self.branch_label();
        self.mc_from = merge_source(&self.root);
        self.mc_open = true;
        window.focus(&self.mc_focus, cx);
        cx.notify();
    }

    /// Re-scan conflicts; close the dialog once everything is resolved.
    fn mc_reload(&mut self, cx: &mut Context<Self>) {
        self.mc_files = conflicted_files(&self.root);
        if self.mc_files.is_empty() {
            self.mc_open = false;
            self.show_flash("All conflicts resolved — commit to finish the merge", cx);
        }
        cx.notify();
    }

    /// Resolve one file by taking our side (`--ours`) or theirs (`--theirs`),
    /// then stage it, off the main thread; reloads the list when done.
    fn mc_accept(&mut self, path: PathBuf, theirs: bool, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let side = if theirs { "--theirs" } else { "--ours" };
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().to_string();
        cx.spawn(async move |this, cx| {
            let (r1, r2) = (root.clone(), root.clone());
            let (rel1, rel2) = (rel.clone(), rel.clone());
            cx.background_executor()
                .spawn(async move {
                    let _ = Command::new("git").args(["checkout", side, &rel1]).current_dir(&r1).output();
                    let _ = Command::new("git").args(["add", "--", &rel2]).current_dir(&r2).output();
                })
                .await;
            this.update(cx, |this, cx| this.mc_reload(cx)).ok();
        })
        .detach();
    }

    fn mc_edit(&mut self, path: PathBuf, _window: &mut Window, cx: &mut Context<Self>) {
        self.mc_open = false;
        // 3-way merge editor in its own window (Yours | Result | Theirs)
        let root = self.root.clone();
        let rel = self.rel(&path);
        let ours = git_stage(&root, &rel, 2);
        let theirs = git_stage(&root, &rel, 3);
        let result = std::fs::read_to_string(&path).unwrap_or_default();
        let storm = cx.entity().downgrade();
        let main = cx.active_window();
        let bounds = Bounds::centered(None, size(px(1660.), px(820.)), cx);
        let title = format!("Merge — {}", rel);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions { title: Some(title.into()), ..Default::default() }),
                focus: true,
                ..Default::default()
            },
            move |_, cx| cx.new(|cx| MergeWindow::new(root, path, ours, theirs, result, storm, main, cx)),
        )
        .ok();
    }

    fn mc_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" {
            self.mc_open = false;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    // ── process manager ───────────────────────────────────────────────────────

    fn open_process_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.proc_open = true;
        self.proc_filter.clear();
        self.proc_selected.clear();
        self.proc_anchor = None;
        self.reload_processes(cx);
        window.focus(&self.proc_focus, cx);
        cx.notify();
    }

    /// `roots` plus all their descendant pids (from the ppid tree in proc_list).
    fn descendants_of(&self, roots: &[u32]) -> HashSet<u32> {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for p in &self.proc_list {
            children.entry(p.ppid).or_default().push(p.pid);
        }
        let mut set: HashSet<u32> = roots.iter().copied().collect();
        let mut stack: Vec<u32> = roots.to_vec();
        while let Some(pid) = stack.pop() {
            if let Some(kids) = children.get(&pid) {
                for &k in kids {
                    if set.insert(k) {
                        stack.push(k);
                    }
                }
            }
        }
        set
    }

    /// Reload the process list + this workspace's terminal shell pids.
    fn reload_processes(&mut self, cx: &mut Context<Self>) {
        self.proc_list = list_processes();
        self.proc_ws_pids = self.all_terminals().iter().filter_map(|t| t.read(cx).child_pid()).collect();
    }

    /// Filtered + sorted process rows. Scope: this workspace's terminals →
    /// all of tide → all system, per the two toggles.
    fn proc_rows(&self) -> Vec<Proc> {
        let q = self.proc_filter.text.to_lowercase();
        let own = std::process::id();
        let scope: Option<HashSet<u32>> = if self.proc_workspace_only {
            Some(self.descendants_of(&self.proc_ws_pids))
        } else if self.proc_only_tide {
            Some(self.descendants_of(&[own]))
        } else {
            None
        };
        self.proc_list
            .iter()
            .filter(|p| p.pid != own) // never list tide itself
            .filter(|p| scope.as_ref().map_or(true, |set| set.contains(&p.pid)))
            .filter(|p| q.is_empty() || p.comm.to_lowercase().contains(&q) || p.user.to_lowercase().contains(&q))
            .cloned()
            .collect()
    }

    /// Click a process row: plain = single select, cmd = toggle, shift = range.
    fn proc_select(&mut self, ix: usize, shift: bool, cmd: bool, cx: &mut Context<Self>) {
        let rows = self.proc_rows();
        let Some(row) = rows.get(ix) else { return };
        let pid = row.pid;
        if shift {
            let anchor = self.proc_anchor.unwrap_or(ix);
            let (lo, hi) = (anchor.min(ix), anchor.max(ix));
            self.proc_selected = rows[lo..=hi].iter().map(|p| p.pid).collect();
        } else if cmd {
            if !self.proc_selected.remove(&pid) {
                self.proc_selected.insert(pid);
            }
            self.proc_anchor = Some(ix);
        } else {
            self.proc_selected.clear();
            self.proc_selected.insert(pid);
            self.proc_anchor = Some(ix);
        }
        cx.notify();
    }

    /// Kill the given pids (SIGTERM), then reload the list.
    fn proc_kill(&mut self, pids: Vec<u32>, cx: &mut Context<Self>) {
        for pid in pids {
            let _ = Command::new("kill").arg(pid.to_string()).output();
        }
        self.proc_selected.clear();
        self.proc_anchor = None;
        self.reload_processes(cx);
        cx.notify();
    }

    fn proc_kill_selected(&mut self, cx: &mut Context<Self>) {
        let pids: Vec<u32> = self.proc_selected.iter().copied().collect();
        self.proc_kill(pids, cx);
    }

    /// Kill every process currently shown (after the filter + TIDE filter).
    fn proc_kill_all(&mut self, cx: &mut Context<Self>) {
        let pids: Vec<u32> = self.proc_rows().iter().map(|p| p.pid).collect();
        self.proc_kill(pids, cx);
    }

    fn proc_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.proc_open = false;
                self.focus_active(window, cx);
            }
            // cmd+backspace kills the selected processes (plain backspace edits filter)
            "backspace" if ks.modifiers.platform => self.proc_kill_selected(cx),
            // cmd+r reloads the list
            "r" if ks.modifiers.platform => self.reload_processes(cx),
            _ => {
                if Self::field_input(&mut self.proc_filter, ks, cx, |_| true) == Edit::Changed {
                    self.proc_anchor = None;
                }
            }
        }
        cx.notify();
    }

    fn palette_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // ⌘K again closes it, the way a toggle should
        if ks.key == "k" && ks.modifiers.platform {
            self.palette_open = false;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        match ks.key.as_str() {
            "escape" => {
                self.palette_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                if let Some(&(cmd, ..)) = self.palette_results.get(self.palette_sel) {
                    self.palette_execute(cmd, window, cx);
                }
            }
            "down" => {
                let n = self.palette_results.len();
                self.palette_sel = (self.palette_sel + 1).min(n.saturating_sub(1));
            }
            "up" => self.palette_sel = self.palette_sel.saturating_sub(1),
            // 1-9 instantly run the Nth listed command (no typing needed)
            d if d.len() == 1
                && d.as_bytes()[0].is_ascii_digit()
                && !ks.modifiers.platform
                && !ks.modifiers.control
                && !ks.modifiers.alt =>
            {
                if let Some(n) = d.parse::<usize>().ok().filter(|n| (1..=9).contains(n)) {
                    if let Some(&(cmd, ..)) = self.palette_results.get(n - 1) {
                        self.palette_execute(cmd, window, cx);
                    }
                }
            }
            _ => {
                if Self::field_input(&mut self.palette_query, ks, cx, |_| true) == Edit::Changed {
                    self.schedule_palette(cx); // debounced re-filter
                }
            }
        }
        cx.notify();
    }

    fn run_find_search(&mut self, cx: &mut Context<Self>) {
        self.find_gen += 1;
        let gen = self.find_gen;
        let query = self.find_query.text.clone();
        let scopes = self.find_scope.clone();
        let case_sensitive = self.find_case_sensitive;
        cx.spawn(async move |this, cx| {
            let results = cx
                .background_executor()
                .spawn(async move { search_files(&query, &scopes, case_sensitive) })
                .await;
            this.update(cx, |this, cx| {
                if this.find_gen == gen {
                    this.find_results = results;
                    this.find_selected = 0;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn find_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.find_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                if let Some(r) = self.find_results.get(self.find_selected).cloned() {
                    self.find_open = false;
                    self.open_file(r.path, window, cx);
                    if let Some(tab) = self.tabs.get(self.active) {
                        tab.editor.update(cx, |e, cx| e.goto(r.line, 1, cx));
                    }
                }
            }
            // F4 opens the result in the editor and closes the find panel
            "f4" => {
                if let Some(r) = self.find_results.get(self.find_selected).cloned() {
                    self.find_open = false;
                    self.open_file(r.path, window, cx);
                    if let Some(tab) = self.tabs.get(self.active) {
                        tab.editor.update(cx, |e, cx| e.goto(r.line, 1, cx));
                    }
                }
            }
            "down" => {
                self.find_selected =
                    (self.find_selected + 1).min(self.find_results.len().saturating_sub(1));
                self.find_psel = None; // different file → drop the preview selection
                self.find_scroll_into_view();
            }
            "up" => {
                self.find_selected = self.find_selected.saturating_sub(1);
                self.find_psel = None;
                self.find_scroll_into_view();
            }
            // cmd+a: select the search text by default; once you're selecting in
            // the preview pane, select all of its code instead
            "a" if ks.modifiers.platform => {
                if self.find_psel.is_some() {
                    if let Some(pv) = &self.find_preview {
                        if !pv.lines.is_empty() {
                            let last = pv.lines.len() - 1;
                            let last_len = pv.lines[last].chars().count();
                            self.find_psel = Some((0, 0, last, last_len));
                        }
                    }
                } else {
                    Self::field_input(&mut self.find_query, ks, cx, |_| true);
                }
            }
            "c" if ks.modifiers.platform => {
                if let Some(text) = self.find_selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            _ => {
                if Self::field_input(&mut self.find_query, ks, cx, |_| true) == Edit::Changed {
                    self.find_psel = None;
                    self.run_find_search(cx);
                }
            }
        }
        cx.notify();
    }

    /// Keep the selected result row visible as you arrow through the list.
    fn find_scroll_into_view(&self) {
        let row_h = 20.0_f32; // matches the result row height
        let vh = f32::from(self.find_scroll.bounds().size.height).max(1.0);
        let y = self.find_selected as f32 * row_h;
        let top = -f32::from(self.find_scroll.offset().y);
        if y < top {
            self.find_scroll.set_offset(gpui::point(px(0.), px(-y)));
        } else if y + row_h > top + vh {
            self.find_scroll.set_offset(gpui::point(px(0.), px(-(y + row_h - vh))));
        }
    }

    /// File line index of the first row rendered in the preview (matches the
    /// `start` used when rendering: 6 lines of context above the match).
    fn find_preview_start(&self) -> Option<usize> {
        self.find_results.get(self.find_selected).map(|r| r.line.saturating_sub(6))
    }

    /// Map a mouse position over the preview to a (file-line, char-col) cell.
    fn find_cell_at(&self, pos: gpui::Point<gpui::Pixels>) -> Option<(usize, usize)> {
        let pv = self.find_preview.as_ref()?;
        let b = self.find_pscroll.bounds();
        let off = self.find_pscroll.offset();
        let gutter = 48.0 + 8.0; // line-number cell + code left padding
        let lx = f32::from(pos.x) - f32::from(b.left()) - f32::from(off.x);
        let ly = f32::from(pos.y) - f32::from(b.top()) - f32::from(off.y);
        if lx < 0.0 || ly < 0.0 {
            return None;
        }
        let line = self.find_preview_start()? + (ly / 18.0).floor() as usize;
        if line >= pv.lines.len() {
            return None;
        }
        let len = pv.lines[line].chars().count();
        let col = (((lx - gutter) / self.find_char_w).floor()).max(0.0) as usize;
        Some((line, col.min(len)))
    }

    /// The preview's current selection as text (newline-joined), if any.
    fn find_selected_text(&self) -> Option<String> {
        let (ar, ac, hr, hc) = self.find_psel?;
        let pv = self.find_preview.as_ref()?;
        let ((sr, sc), (er, ec)) =
            if (ar, ac) <= (hr, hc) { ((ar, ac), (hr, hc)) } else { ((hr, hc), (ar, ac)) };
        let mut out = Vec::new();
        for r in sr..=er {
            let ch: Vec<char> = pv.lines.get(r).map(|s| s.as_str()).unwrap_or("").chars().collect();
            let n = ch.len();
            let cs = if r == sr { sc } else { 0 }.min(n);
            let ce = if r == er { ec } else { n }.min(n);
            out.push(ch[cs..ce].iter().collect::<String>());
        }
        Some(out.join("\n"))
    }

    fn do_commit(&mut self, push: bool, window: &mut Window, cx: &mut Context<Self>) {
        // empty message → quick "wip: <id>" commit (matches the `wip` zsh alias)
        let mut msg = self.commit_msg.text.trim().to_string();
        if msg.is_empty() {
            msg = format!("wip: {}", self.wip_id);
        }
        let safe = msg.replace('"', "\\\"");
        // stage only the checked files; if none checked, stage everything
        let add = if self.commit_checked.is_empty() {
            "git add -A".to_string()
        } else {
            let paths: Vec<String> = self
                .commit_checked
                .iter()
                .map(|p| format!("\"{}\"", self.rel(p)))
                .collect();
            format!("git add {}", paths.join(" "))
        };
        let cmd = if push {
            format!("{} && git commit -m \"{}\" && git push -u origin HEAD", add, safe)
        } else {
            format!("{} && git commit -m \"{}\"", add, safe)
        };
        self.commit_msg.clear();
        self.commit_checked.clear();
        self.wip_id = random_id(); // fresh id for the next quick commit
        let _ = window;
        self.run_command(cmd, cx);
    }

    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.resizing {
            self.tree_width = f32::from(ev.position.x).clamp(150., 750.);
            cx.notify();
        } else if self.resizing_term {
            // allow the terminal/browser dock to grow up to ~90% of the window
            // (so the browser can go nearly full-screen)
            let max_w = (self.win_width * 0.92).max(200.);
            self.term_width = (self.win_width - f32::from(ev.position.x)).clamp(200., max_w);
            cx.notify();
        } else if self.resizing_agent {
            // the agent sits on top of the terminal panes; this divider splits the
            // dock vertically, so the share follows the cursor's y within it
            let top = 44.0; // topbar
            let h = (self.win_height - top).max(1.0);
            self.agent_share = ((f32::from(ev.position.y) - top) / h).clamp(0.15, 0.85);
            cx.notify();
        } else if self.resizing_git {
            // bottom dock: divider on its top edge; drag up grows it
            self.git_height = (self.win_height - f32::from(ev.position.y)).clamp(160., self.win_height - 120.);
            cx.notify();
        } else if let Some(e) = self.find_resize {
            // window-style resize: drag any edge/corner; the opposite edge stays
            let (mx, my) = (f32::from(ev.position.x), f32::from(ev.position.y));
            let (min_w, min_h) = (420.0_f32, 300.0_f32);
            let right = self.find_left + self.find_w;
            let bottom = self.find_top + self.find_h;
            if e.r {
                self.find_w = (mx - self.find_left).clamp(min_w, (self.win_width - self.find_left).max(min_w));
            }
            if e.b {
                self.find_h = (my - self.find_top).clamp(min_h, (self.win_height - self.find_top).max(min_h));
            }
            if e.l {
                let nl = mx.clamp(0.0, right - min_w);
                self.find_left = nl;
                self.find_w = right - nl;
            }
            if e.t {
                let nt = my.clamp(40.0, bottom - min_h);
                self.find_top = nt;
                self.find_h = bottom - nt;
            }
            cx.notify();
        } else if self.find_moving {
            // drag the title bar to reposition (keep the grab offset constant)
            let (mx, my) = (f32::from(ev.position.x), f32::from(ev.position.y));
            self.find_left = (mx - self.find_move_dx).clamp(0.0, (self.win_width - self.find_w).max(0.0));
            self.find_top = (my - self.find_move_dy).clamp(40.0, (self.win_height - self.find_h).max(40.0));
            cx.notify();
        } else if self.find_split_dragging {
            // drag the divider between results (top) and preview (bottom):
            // down → bigger results, up → bigger preview
            let h = self.find_h.clamp(300.0, (self.win_height - 120.0).max(300.0));
            let top = self.find_top.clamp(40.0, (self.win_height - h).max(40.0));
            let content_h = (h - FIND_HEAD_H).max(160.0); // area below header+scope
            let results_h =
                (f32::from(ev.position.y) - top - FIND_HEAD_H).clamp(80.0, content_h - 80.0);
            self.find_split = results_h / content_h;
            cx.notify();
        } else if let Some((ox, oy, w0, h0, corner)) = self.rc_resize {
            // bottom edge → taller thread area; corner → wider too
            let h = (h0 + (f32::from(ev.position.y) - oy)).clamp(40.0, 900.0);
            self.rc_thread_h = Some(h);
            if corner {
                let w = (w0 + (f32::from(ev.position.x) - ox))
                    .clamp(280.0, (self.win_width - 40.0).max(280.0));
                self.rc_width = Some(w);
            }
            self.sync_comment_gutters(cx); // the editor's reserved gap grows with it
            cx.notify();
        } else if let Some(d) = self.pane_drag.clone() {
            // move weight between the two panes the divider separates, keeping
            // their combined share (so the rest of the tab doesn't shift)
            let pos = if d.horizontal { f32::from(ev.position.x) } else { f32::from(ev.position.y) };
            let frac = (d.start_frac + (pos - d.origin) / d.extent).clamp(0.08, 0.92);
            let tab_ix = self.active_term;
            if let Some(tab) = self.term_tabs.get_mut(tab_ix) {
                if let Some(TermPane::Split { shares, .. }) = tab.panes.at_mut(&d.path) {
                    if d.ix + 1 < shares.len() {
                        shares[d.ix] = d.total * frac;
                        shares[d.ix + 1] = d.total * (1. - frac);
                    }
                }
            }
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.resizing
            || self.resizing_term
            || self.resizing_agent
            || self.resizing_git
            || self.find_resize.is_some()
            || self.find_moving
            || self.find_split_dragging
            || self.pane_drag.is_some()
            || self.rc_resize.is_some()
            || self.agent_sel_drag
        {
            self.resizing = false;
            self.resizing_term = false;
            self.resizing_agent = false;
            self.resizing_git = false;
            self.find_resize = None;
            self.find_moving = false;
            self.find_split_dragging = false;
            self.pane_drag = None;
            self.rc_resize = None;
            self.agent_sel_drag = false;
            self.agent_sel_moved = false;
            self.agent_sel_armed = None;
            for t in self.all_terminals() {
                t.update(cx, |t, _| t.defer_resize = false);
            }
            cx.notify();
        }
    }

    fn rel(&self, path: &PathBuf) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }
}

/// A zero-size overlay whose only job is to register `ElementInputHandler` for
/// `focus` during paint — `window.handle_input` is paint-phase only, and a plain
/// div gives us no paint hook.
fn input_sink(focus: FocusHandle, storm: Entity<Storm>) -> impl IntoElement {
    gpui::canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            window.handle_input(&focus, ElementInputHandler::new(bounds, storm.clone()), cx);
        },
    )
    .absolute()
    .size_full()
}

/// Dictation, IME, and the emoji picker deliver text through macOS's
/// `insertText:`, never as key events — so the chrome inputs need a real input
/// handler or a dictated sentence arrives as a single stray character.
impl Storm {
    fn focused_field(&mut self, window: &Window) -> Option<&mut Field> {
        if self.agent_focus.is_focused(window) {
            return Some(&mut self.agent_input);
        }
        if self.rc_focus.is_focused(window) {
            return Some(&mut self.rc_query);
        }
        if self.review_note_focus.is_focused(window) {
            return Some(&mut self.review_note);
        }
        if self.agent_jump_focus.is_focused(window) {
            return Some(&mut self.agent_jump_query);
        }
        None
    }
}

impl EntityInputHandler for Storm {
    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _actual: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        // an empty selection is enough to mark this a live text destination
        Some(gpui::UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _range: Option<std::ops::Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // control characters (⌃C sends U+0003) aren't text; inserting one would
        // also delete the current selection on the way in
        let new_text: String =
            new_text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
        if new_text.is_empty() {
            return;
        }
        let agent = self.agent_focus.is_focused(window);
        if let Some(f) = self.focused_field(window) {
            f.insert_text(&new_text);
            self.recompute_agent_ghost();
            if agent {
                self.recompute_mentions(cx);
            }
            self.sync_comment_gutters(cx); // the comment box may have grown
            cx.notify();
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected: Option<std::ops::Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // mid-composition IME text: insert it as-is, same as the terminal does
        self.replace_text_in_range(range, new_text, window, cx);
    }

    fn bounds_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        bounds: Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<gpui::Pixels>> {
        Some(bounds) // IME candidates sit over the input itself
    }

    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

// ── render ───────────────────────────────────────────────────────────────

impl Render for Storm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let win = window.viewport_size();
        self.win_width = f32::from(win.width);
        self.win_height = f32::from(win.height);
        let show_term = self.show_terminal && !self.term_tabs.is_empty();

        // Focus the root once so global shortcuts (cmd+shift+o, cmd+l, opt+f12)
        // dispatch even before any file/tab is open.
        if !self.inited {
            self.inited = true;
            window.focus(&self.focus, cx);
        }

        // WebStorm-style title: "<project> - <open file>"
        let project = self.project_name();
        let title = match self.active_path() {
            Some(p) => format!(
                "{} - {}",
                project,
                p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            ),
            None => project.clone(),
        };
        window.set_window_title(&title);

        // Build pieces as locals first (each borrows self/cx sequentially).
        let topbar = self.render_topbar(project.clone(), cx);
        let act_left = self.render_activity_left(cx);
        let act_right = self.render_activity_right(cx);
        let run_dock = if self.run_open { Some(self.render_run(cx).into_any_element()) } else { None };
        let git_dock = if self.git_open {
            let panel = self.render_git_log(window, cx);
            let divider = div()
                .h(px(4.))
                .w_full()
                .flex_shrink_0()
                .bg(rgb(if self.resizing_git { ACCENT } else { BORDER }))
                .cursor(CursorStyle::ResizeUpDown)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _ev, _window, cx| {
                        this.resizing_git = true;
                        cx.notify();
                    }),
                );
            Some(div().flex().flex_col().flex_shrink_0().child(divider).child(panel).into_any_element())
        } else {
            None
        };
        let left_panel = if self.show_left {
            Some(match self.left_view {
                LeftView::Files => self.render_tree(window, cx).into_any_element(),
                LeftView::Changes => self.render_changes(window, cx).into_any_element(),
                LeftView::PullRequest => self.render_pr(window, cx).into_any_element(),
                LeftView::Review => self.render_review_pane(window, cx).into_any_element(),
            })
        } else {
            None
        };
        let left_divider = if self.show_left { Some(self.render_divider(cx)) } else { None };
        // center: editor pane (which now also hosts the diff as a tab)
        let center = self.render_editor(cx).into_any_element();

        // middle row: [activity-left] [panel] [divider] [center] [term] [activity-right]
        let mut middle = div().flex().flex_row().flex_grow(1.0).min_h(px(0.)).child(act_left);
        if let Some(panel) = left_panel {
            // flex_shrink_0 wrapper → the tree keeps its dragged width regardless
            // of how wide the editor tab bar gets
            middle = middle.child(div().flex_shrink_0().h_full().child(panel));
            if let Some(d) = left_divider {
                middle = middle.child(d);
            }
        }
        middle = middle.child(self.render_editor_wrap(center, cx));

        // The right dock: the agent pane on top, the terminal under it. The
        // agent is always in it — closing the terminal hands the whole dock to
        // the agent rather than taking the dock away with it.
        let show_agent = AGENT_PANEL && self.agent_open && !self.term_zoomed;
        let show_panes = show_term && !(show_agent && self.agent_zoomed);
        if show_agent || show_term {
            // built first: each of these borrows self on its own
            let term_parts =
                show_panes.then(|| (self.render_term_panes(window, cx), self.render_term_tabs(cx)));
            let agent = show_agent.then(|| self.render_agent(window, cx));
            middle = middle
                .child(
                    div()
                        .w(px(4.))
                        .h_full()
                        .flex_shrink_0()
                        .bg(rgb(if self.resizing_term { ACCENT } else { BORDER }))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev, _window, cx| {
                                this.resizing_term = true;
                                for t in this.all_terminals() {
                                    t.update(cx, |t, _| t.defer_resize = true);
                                }
                                cx.notify();
                            }),
                        ),
                )
                .child({
                    let mut dock = div()
                        .w(px(self.term_width))
                        .h_full()
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .bg(rgb(PANEL_BG));
                    let split = agent.is_some() && term_parts.is_some();
                    if let Some(agent) = agent {
                        // with no terminal under it, the agent is the dock
                        let share = if split { self.agent_share } else { 1.0 };
                        dock = dock.child(
                            div()
                                .flex_grow(share)
                                .flex_basis(relative(0.))
                                .min_h(px(0.))
                                .overflow_hidden()
                                .child(agent),
                        );
                    }
                    // the height divider only exists while there are two things
                    // to divide
                    if split {
                        dock = dock.child(
                            div()
                                .h(px(4.))
                                .w_full()
                                .flex_shrink_0()
                                .bg(rgb(if self.resizing_agent { ACCENT } else { BORDER }))
                                .cursor(CursorStyle::ResizeUpDown)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _ev, _window, cx| {
                                        this.resizing_agent = true;
                                        for t in this.all_terminals() {
                                            t.update(cx, |t, _| t.defer_resize = true);
                                        }
                                        cx.notify();
                                    }),
                                ),
                        );
                    }
                    if let Some((panes, tab_bar)) = term_parts {
                        let rest = if split { 1.0 - self.agent_share } else { 1.0 };
                        dock = dock.child(
                            div()
                                .flex_grow(rest)
                                .flex_basis(relative(0.))
                                .min_h(px(0.))
                                .flex()
                                .flex_col()
                                .child(tab_bar)
                                .child(div().flex_grow(1.0).flex().min_h(px(0.)).child(panes)),
                        );
                    }
                    dock
                });
        }
        // browser shares the right dock with the terminal (never both — the
        // toggles are mutually exclusive); its WebView floats over the dock
        if self.browser_open {
            let dock = self.render_browser_dock(window, cx);
            middle = middle
                .child(
                    div()
                        .w(px(4.))
                        .h_full()
                        .flex_shrink_0()
                        .bg(rgb(if self.resizing_term { ACCENT } else { BORDER }))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _ev, _window, cx| {
                                this.resizing_term = true;
                                cx.notify();
                            }),
                        ),
                )
                .child(dock);
        } else if let Some(b) = &self.browser {
            b.set_visible(false); // dock closed → hide the floating WebView
        }
        middle = middle.child(act_right);

        let mut root = div()
            .flex()
            .flex_col()
            .w(win.width)
            .h(win.height)
            .bg(rgb(BG))
            .font_family("Inter") // UI font; editor/terminal/diff set their own mono
            .text_size(px(14.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_root_key))
            .on_action(cx.listener(Self::act_close_tab))
            .on_action(cx.listener(Self::act_close_others))
            .on_action(cx.listener(Self::act_toggle_term))
            .on_action(cx.listener(Self::act_open_finder))
            .on_action(cx.listener(Self::act_goto))
            .on_action(cx.listener(Self::act_new_terminal))
            .on_action(cx.listener(Self::act_close_terminal))
            .on_action(cx.listener(Self::act_close_other_terminals))
            .on_action(cx.listener(Self::act_split_term_right))
            .on_action(cx.listener(Self::act_split_term_down))
            .on_action(cx.listener(Self::act_toggle_zoom))
            .on_action(cx.listener(Self::act_pane_left))
            .on_action(cx.listener(Self::act_pane_right))
            .on_action(cx.listener(Self::act_pane_up))
            .on_action(cx.listener(Self::act_pane_down))
            .on_action(cx.listener(Self::act_goto_commit))
            .on_action(cx.listener(Self::act_show_diff))
            .on_action(cx.listener(Self::act_find_in_files))
            .on_action(cx.listener(Self::act_git_popup))
            .on_action(cx.listener(Self::act_command_palette))
            .on_action(cx.listener(Self::act_copy_reference))
            .on_action(cx.listener(Self::act_copy_reference_line))
            .on_action(cx.listener(Self::act_open_on_github))
            .on_action(cx.listener(Self::act_push_dialog))
            .on_action(cx.listener(Self::act_run_command))
            .on_action(cx.listener(Self::act_set_ws_title))
            .on_action(cx.listener(Self::act_refresh_pr))
            .on_action(cx.listener(Self::act_add_comment))
            .on_action(cx.listener(Self::act_show_comments))
            .on_action(cx.listener(Self::act_send_comments))
            .on_action(cx.listener(Self::act_md_preview))
            .on_action(cx.listener(Self::act_quote_agent_sel))
            .on_action(cx.listener(Self::act_new_project))
            .on_action(cx.listener(Self::act_new_scratch))
            .on_action(cx.listener(Self::act_fetch))
            .on_action(cx.listener(Self::act_wip_push))
            .on_action(cx.listener(Self::act_run_build))
            .on_action(cx.listener(Self::act_pull))
            .on_action(cx.listener(Self::act_toggle_agent))
            .on_action(cx.listener(Self::act_git_log))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(topbar)
            .child(middle)
            .when_some(run_dock, |d, dock| d.child(dock))
            .when_some(git_dock, |d, dock| d.child(dock));

        if self.finder_open {
            root = root.child(self.render_finder(cx));
        }
        if self.goto_open {
            root = root.child(self.render_goto(cx));
        }
        if self.nf_open {
            root = root.child(self.render_new_file_prompt(cx));
        }
        if self.mc_open {
            root = root.child(self.render_conflicts(cx));
        }
        if self.br_open {
            root = root.child(self.render_branch_prompt(cx));
        }
        if !self.review.is_empty() {
            root = root.child(self.render_inline_notes(cx));
        }
        let rc_inline = self
            .rc_target
            .as_ref()
            .is_some_and(|(p, _, _)| is_agent_src(p));
        if self.rc_open && !rc_inline {
            let rc_focused = self.rc_focus.is_focused(window);
            root = root.child(self.render_comment_prompt(rc_focused, false, cx));
        }
        if self.title_open {
            root = root.child(self.render_title_prompt(cx));
        }
        if self.prc_open {
            root = root.child(self.render_pr_create_prompt(cx));
        }
        if self.runc_open {
            root = root.child(self.render_run_prompt(cx));
        }
        if self.newproj_open {
            root = root.child(self.render_new_project(cx));
        }
        if self.find_open {
            root = root.child(self.render_find(cx));
        }
        if self.gitp_open {
            root = root.child(self.render_git_popup(cx));
            if self.gitp_action_branch.is_some() {
                root = root.child(self.render_branch_actions(cx));
            }
        }
        if self.confirm_delete.is_some() {
            root = root.child(self.render_confirm_delete(cx));
        }
        if self.confirm_revert.is_some() {
            root = root.child(self.render_confirm_revert(cx));
        }
        if let Some(pos) = self.editor_ctx {
            root = root.child(self.render_editor_ctx_menu(pos, cx));
        }
        if let Some(pos) = self.tree_ctx {
            root = root.child(self.render_tree_ctx_menu(pos, cx));
        }
        if self.palette_open {
            root = root.child(self.render_palette(cx));
        }
        if self.push_open {
            root = root.child(self.render_push_dialog(cx));
        }
        if self.proc_open {
            root = root.child(self.render_process_manager(cx));
        }
        if self.push_ahead {
            root = root.child(self.render_push_ahead(cx));
        }
        if self.ws_open {
            root = root.child(self.render_project_dropdown(cx));
        }
        if self.limits_open {
            root = root.children(self.render_usage_overlay(cx));
        }
        // one bottom-right column, so a flash and the run toast stack instead of
        // landing on top of each other
        let toast = self.run_active && !self.run_open;
        if toast || self.flash.is_some() {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(56.))
                    .right(px(28.))
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_2()
                    .when(toast, |d| d.child(self.render_run_toast(cx)))
                    .when_some(self.flash.clone(), |d, msg| {
                        d.child(
                            div()
                                .min_w(px(340.))
                                .max_w(px(560.))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_3()
                                .pl_3()
                                .pr_4()
                                .py_3()
                                .bg(rgb(POPUP_BG))
                                .border_1()
                                .border_color(rgb(GIT_NEW))
                                .border_l_4()
                                .rounded_md()
                                .shadow_lg()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(20.))
                                        .text_color(rgb(GIT_NEW))
                                        .child("✓"),
                                )
                                .child(
                                    div()
                                        .flex_grow(1.0)
                                        .min_w(px(0.))
                                        .text_size(px(14.))
                                        .text_color(rgb(TEXT))
                                        .truncate()
                                        .child(msg),
                                ),
                        )
                    }),
            );
        }

        root
    }
}

impl Storm {
    fn render_editor_wrap(&self, editor: impl IntoElement, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("editor-surface")
            .flex_grow(1.0)
            .min_w(px(0.))
            .h_full()
            // drop OS files (from Finder) here → open them, even outside the project
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                for p in paths.paths() {
                    if p.is_file() {
                        this.open_file(p.clone(), window, cx);
                    }
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                    if this.tabs.is_empty() {
                        return; // nothing to act on without an open file
                    }
                    this.editor_ctx = Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                    window.focus(&this.editor_ctx_focus, cx); // route Esc / number keys here
                    cx.notify();
                }),
            )
            .child(editor)
    }

    /// Branch name for the topbar. In detached HEAD, fall back to the PR's head
    /// branch (the real branch name) instead of the useless literal "HEAD".
    fn branch_label(&self) -> String {
        if self.branch == "HEAD" {
            if let Some((_, _, _, _, head, _)) = &self.pr_link {
                if !head.is_empty() {
                    return head.clone();
                }
            }
        }
        self.branch.clone()
    }

    /// A branch tag pill that copies the branch name to the clipboard on click.
    fn branch_tag(name: &str, id: &'static str, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let copied = name.to_string();
        branch_name_el(name)
            .id(id)
            .cursor_pointer()
            .hover(|s| s.opacity(0.8))
            .tooltip(tip("Click to copy branch name"))
            .on_click(cx.listener(move |this, _e, _w, cx| {
                if !copied.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                    this.show_flash("Branch name copied", cx);
                }
            }))
    }

    fn render_topbar(&self, project: String, cx: &mut Context<Self>) -> impl IntoElement {
        // the real folder, shown grayed next to a custom title (redundant otherwise)
        let folder = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| *n != project);
        let mut bar = div()
            .h(px(44.)) // match the activity-bar width
            .flex_shrink_0() // never let a tall panel squeeze the top bar
            .w_full()
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            // left: project switcher + branch (equal-grow side so the strip centers)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    // title chip + its edit pencil, kept tight together (the
                    // container's gap_3 spaces them from the branch group)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            // workspace number (1-based position in the project
                            // switcher) — same key-cap as the switcher's ⌘E rows
                            .child(
                                key_cap(format!("{}", self.ws_active + 1), true)
                                    .id("ws-number")
                                    .tooltip(|_w, cx| {
                                        cx.new(|_| TooltipView {
                                            text: "Workspace number  ·  ⌘E to switch".into(),
                                        })
                                        .into()
                                    }),
                            )
                            .child(
                                div()
                                    .id("project-switcher")
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .px_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .text_color(rgb(TEXT))
                                    .text_size(px(12.))
                                    .child(project)
                                    .when_some(folder, |d, f| {
                                        d.child(
                                            div()
                                                .flex_shrink_0()
                                                .text_size(px(10.))
                                                .text_color(rgb(LINE_NUMBER))
                                                .child(f),
                                        )
                                    })
                                    // who this work is for, when it's client work
                                    .when_some(self.ws_client.clone(), |d, c| {
                                        d.child(
                                            div()
                                                .flex_shrink_0()
                                                .px_1()
                                                .rounded_md()
                                                .bg(rgb(HOVER))
                                                .text_size(px(9.))
                                                .text_color(rgb(FOLDER_ICON))
                                                .child(c),
                                        )
                                    })
                                    .child(div().text_color(rgb(MUTED)).text_size(px(9.)).child("▾"))
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.ws_open = !this.ws_open;
                                        cx.notify();
                                    })),
                            )
                            // edit workspace title
                            .child(
                                div()
                                    .id("edit-ws-title")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .h(px(18.))
                                    .px_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_size(px(11.))
                                    .text_color(rgb(MUTED))
                                    .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                    .child("✎")
                                    .tooltip(|_w, cx| {
                                        cx.new(|_| TooltipView { text: "Set workspace title".into() }).into()
                                    })
                                    .on_click(cx.listener(|this, _e, window, cx| this.open_title_prompt(window, cx))),
                            )
                            // pause: keep this workspace out of the ⌥Tab rotation
                            .child({
                                let paused = self.ws_paused;
                                div()
                                    .id("ws-pause")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .h(px(18.))
                                    .px_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_size(px(10.))
                                    .text_color(rgb(if paused { GIT_MODIFIED } else { MUTED }))
                                    .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                    .child(if paused { "❙❙ PAUSED" } else { "❙❙" })
                                    .tooltip(move |_w, cx| {
                                        let text = if paused {
                                            "Paused — ⌥Tab skips this workspace. Click to resume."
                                        } else {
                                            "Pause: keep this workspace out of the ⌥Tab rotation"
                                        };
                                        cx.new(|_| TooltipView { text: text.into() }).into()
                                    })
                                    .on_click(cx.listener(|this, _e, _w, cx| this.toggle_pause(cx)))
                            })
                            // agent state for this workspace: the loud badge when
                            // nothing's running, a dot + word otherwise
                            .when_some(self.agent_rollup(), |d, (s, _)| {
                                if s.is_idle() {
                                    d.child(idle_badge(9.))
                                } else {
                                    d.child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_1()
                                            .ml_1()
                                            .child(
                                                div()
                                                    .w(px(6.))
                                                    .h(px(6.))
                                                    .rounded_full()
                                                    .flex_shrink_0()
                                                    .bg(rgb(agent_color(s))),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(rgb(agent_color(s)))
                                                    .child(agent_label(s)),
                                            ),
                                    )
                                }
                            }),
                    )
                    .when(!self.branch.is_empty(), |d| {
                        d.child(div().w(px(1.)).h(px(16.)).mx_1().bg(rgb(BORDER))) // separator
                            .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .text_color(rgb(MUTED))
                                .text_size(px(12.))
                                // target/base first (GitHub order: base ← head): the
                                // PR's base branch if there's a PR, else the reflog guess
                                .when_some(
                                    self.pr_link
                                        .as_ref()
                                        .map(|(_, _, _, b, _, _)| b.clone())
                                        .filter(|b| !b.is_empty())
                                        .or_else(|| self.branch_parent.clone()),
                                    |d, target| {
                                        d.child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .items_center()
                                                .gap_1()
                                                .text_size(px(11.))
                                                .child(Self::branch_tag(&target, "copy-base-branch", cx))
                                                .child(div().text_color(rgb(LINE_NUMBER)).child("←")),
                                        )
                                    },
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_1()
                                        .child(div().text_color(rgb(MUTED)).child("⎇"))
                                        .child(Self::branch_tag(&self.branch_label(), "copy-branch", cx)),
                                ),
                        )
                    })
                    // PR link for the current branch, if one exists → opens it
                    .when_some(self.pr_link.clone(), |d, (num, url, status, _base, _head, state)| {
                        d.child(
                            div()
                                .id("pr-link")
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .px_1()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(HOVER)))
                                .text_color(rgb(ACCENT))
                                .text_size(px(12.))
                                // status dot: green = checks pass, orange = pending, red = failing
                                .child(
                                    div()
                                        .size(px(8.))
                                        .rounded_full()
                                        .bg(rgb(status.color())),
                                )
                                .child(div().font_family(ICON_FONT).text_size(px(11.)).child(IC_PR))
                                .child(format!("#{}", num))
                                // draft / merged / closed pill (open needs no label)
                                .when(state != PrState::Open, |d| d.child(pr_state_pill(state, 10.)))
                                .on_click(cx.listener(move |_this, _e, _w, _cx| {
                                    let _ = Command::new("open").arg(&url).spawn();
                                })),
                        )
                    })
                    // no PR for this branch yet → say so, so the refresh icon reads clearly
                    .when(self.pr_link.is_none() && !self.branch.is_empty(), |d| {
                        d.child(div().text_size(px(12.)).text_color(rgb(MUTED)).child("No PR"))
                    })
                    // refresh the PR link + checks status (otherwise only updates
                    // on branch change / cmd+R)
                    .when(!self.branch.is_empty(), |d| {
                        let tip: &'static str =
                            if self.pr_link.is_some() { "Refresh PR status" } else { "Check for a PR" };
                        d.child(
                            div()
                                .id("pr-refresh")
                                .flex()
                                .items_center()
                                .justify_center()
                                .h(px(18.))
                                .px_1()
                                .rounded_md()
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(rgb(MUTED))
                                .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                .child("↻")
                                .tooltip(move |_w, cx| {
                                    cx.new(|_| TooltipView { text: tip.into() }).into()
                                })
                                .on_click(cx.listener(|this, _e, _w, cx| this.refresh_pr_link(cx))),
                        )
                    })
                    // Open Admin (wby-next* only): the host opens admin{N}.localhost
                    // in the default browser; the ▶ beside it carries up/down
                    .when_some(admin_url(&self.root), |d, url| {
                        // the play button carries the up/down colour now; this is
                        // just the hover text on the host itself
                        let up = self.admin_up == Some(true);
                        let tip = if up {
                            "Admin is up  ·  click to open  ·  ⌘R to re-check"
                        } else {
                            "Admin is down  ·  click to start it  ·  ⌘R to re-check"
                        };
                        d.child(div().w(px(1.)).h(px(16.)).mx_1().bg(rgb(BORDER))) // separator
                            // host + ▶ read as one control, so they sit tight
                            // together instead of a container gap apart
                            .child(div().flex().flex_row().items_center().gap(px(2.)).child(
                            div()
                                .id("open-admin")
                                .flex()
                                .items_center()
                                .justify_center()
                                .h(px(18.))
                                .px_1()
                                .gap_1()
                                .rounded_md()
                                .cursor_pointer()
                                .text_size(px(11.))
                                // green once it's actually serving
                                .text_color(rgb(if up { GIT_NEW } else { MUTED }))
                                .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                .child(div().font_family(ICON_FONT).text_size(px(12.)).child(IC_BROWSER))
                                .child(url.trim_start_matches("https://").trim_start_matches("http://").to_string())
                                .tooltip(move |_w, cx| cx.new(|_| TooltipView { text: tip.into() }).into())
                                // a dead admin has nothing to show: clicking the
                                // host starts it instead of opening a failed page
                                .on_click(cx.listener(move |this, _e, window, cx| {
                                    if up {
                                        open_browser(&url);
                                    } else if this.admin_terminal(cx).is_some() {
                                        this.show_admin_log(window, cx); // already building
                                    } else {
                                        this.start_admin(window, cx);
                                    }
                                })),
                        )
                        .child(self.admin_play_btn("admin-run", cx)))
                    })
                    // watch the branch's changed packages (`wwp`), beside admin
                    .children(self.render_watch_btn(cx)),
            );
        // right: read-only toggle + memory (moved here from the old bottom bar).
        // The flex_1 spacer keeps the left group left-aligned and pushes these right.
        bar = bar.child(div().flex_1()).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .children(self.render_build_chip(cx))
                // build state and workspace states are different things: keep a
                // rule between them
                .when(self.render_build_chip(cx).is_some() && self.ws_states.len() > 1, |d| {
                    d.child(div().w(px(1.)).h(px(14.)).bg(rgb(BORDER)))
                })
                .children(self.render_ws_tally(cx))
                .children(self.render_usage_chip(cx))
                .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(format!("{} MB", self.mem_mb)))
                // last item in the bar, hard against the right edge
                .child({
                    let ro = self.read_only;
                    div()
                        .id("ro-toggle")
                        .flex_shrink_0()
                        .px_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .text_size(px(11.))
                        // blue pill when read-only (locked), amber when editing
                        .bg(rgb(if ro { ACCENT } else { GIT_MODIFIED }))
                        .text_color(rgb(if ro { SEL_TEXT } else { BG }))
                        .child(if ro { "🔒 READ-ONLY" } else { "✎ EDIT" })
                        .tooltip(|_w, cx| {
                            cx.new(|_| TooltipView { text: "Toggle read-only / edit mode".into() }).into()
                        })
                        .on_click(cx.listener(|this, _e, _w, cx| this.toggle_read_only(cx)))
                }),
        );
        bar
    }

    /// The project-switcher dropdown — rendered as a top-level overlay so it
    /// paints above the panels below the top bar (not clipped by it).
    fn render_project_dropdown(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = div()
            .absolute()
            .top(px(44.))
            .left(px(8.))
            .w(px(520.))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .py_1();
        for (i, name) in self.ws_names.iter().enumerate() {
            let sel = i == self.ws_active;
            let idx = i;
            panel = panel.child(
                div()
                    .id(("ws-proj", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .px_3()
                    .cursor_pointer()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .child(
                        div()
                            .w(px(14.))
                            .font_family(ICON_FONT)
                            .text_size(px(13.))
                            .text_color(rgb(if sel { SEL_TEXT } else { FOLDER_ICON }))
                            .child(IC_FOLDER),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                            .child(name.clone()),
                    )
                    .when(!self.ws_branches.get(i).map(|b| b.is_empty()).unwrap_or(true), |d| {
                        let branch = self.ws_branches[i].clone();
                        d.child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                                .child(format!("⎇ {}", branch)),
                        )
                    })
                    // close button (hidden when only one project is left)
                    .when(self.ws_names.len() > 1, |d| {
                        d.child(
                            div()
                                .id(("ws-proj-close", i))
                                .px_1()
                                .text_size(px(12.))
                                .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                                .hover(|s| s.text_color(rgb(0xf7768e)))
                                .cursor_pointer()
                                .child("✕")
                                .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |_this, _e, _w, cx| {
                                    cx.emit(ProjectNav::Remove(idx));
                                    cx.notify();
                                })),
                        )
                    })
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.ws_open = false;
                        cx.emit(ProjectNav::Switch(idx));
                        cx.notify();
                    })),
            );
        }
        panel
            .child(div().h(px(1.)).bg(rgb(BORDER)).my_1())
            .child(
                div()
                    .id("ws-open-project")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .px_3()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .text_color(rgb(MUTED))
                    .child(div().w(px(14.)).flex().justify_center().child("+"))
                    .child(div().child("Open Project…"))
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.ws_open = false;
                        cx.emit(ProjectNav::Open);
                        cx.notify();
                    })),
            )
    }

    fn render_activity_left(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files_active = self.show_left && self.left_view == LeftView::Files;
        let changes_active = self.show_left && self.left_view == LeftView::Changes;
        let pr_active = self.show_left && self.left_view == LeftView::PullRequest;
        let review_active = self.show_left && self.left_view == LeftView::Review;
        div()
            .w(px(44.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .py_2()
            .gap_3()
            .bg(rgb(PANEL_BG))
            .border_r_1()
            .border_color(rgb(BORDER))
            .child(activity_icon("act-files", IC_FILES, "Files", files_active, 0, cx.listener(|this, _ev, _w, cx| {
                if this.show_left && this.left_view == LeftView::Files {
                    this.show_left = false;
                } else {
                    this.left_view = LeftView::Files;
                    this.show_left = true;
                }
                cx.notify();
            })))
            .child(activity_icon("act-changes", IC_SCM, "Commit", changes_active, self.git_status.len(), cx.listener(|this, _ev, _w, cx| {
                if this.show_left && this.left_view == LeftView::Changes {
                    this.show_left = false;
                } else {
                    this.left_view = LeftView::Changes;
                    this.show_left = true;
                }
                cx.notify();
            })))
            .child(activity_icon("act-pr", IC_PR, "Pull Request Review", pr_active, 0, cx.listener(|this, _ev, _w, cx| {
                if this.show_left && this.left_view == LeftView::PullRequest {
                    this.show_left = false;
                } else {
                    this.left_view = LeftView::PullRequest;
                    this.show_left = true;
                    this.load_pr(cx);
                }
                cx.notify();
            })))
            .child(activity_icon("act-review", IC_COMMENT, "Review Comments  (⌥⇧C)", review_active, self.review.len(), cx.listener(|this, _ev, _w, cx| {
                if this.show_left && this.left_view == LeftView::Review {
                    this.show_left = false;
                } else {
                    this.left_view = LeftView::Review;
                    this.show_left = true;
                }
                cx.notify();
            })))
            // push Run + Git to the bottom-left corner
            .child(div().flex_grow(1.0))
            .child(activity_icon("act-run", IC_RUN, "Run console", self.run_open, 0, cx.listener(|this, _ev, _w, cx| {
                this.toggle_run(cx);
            })))
            .child(activity_icon("act-git", IC_BRANCH, "Git  (⌘9)", self.git_open, 0, cx.listener(|this, _ev, window, cx| {
                this.toggle_git_log(window, cx);
            })))
    }

    fn render_activity_right(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(44.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .py_2()
            .gap_3()
            .bg(rgb(PANEL_BG))
            .border_l_1()
            .border_color(rgb(BORDER))
            .child(activity_icon("act-term", IC_TERMINAL, "Terminal  (⌥F12)", self.show_terminal && !(AGENT_PANEL && self.agent_open && self.agent_zoomed), 0, cx.listener(|this, _ev, window, cx| {
                this.toggle_terminal(window, cx);
            })))
            .when(BROWSER_PANEL, |d| d.child(activity_icon("act-browser", IC_BROWSER, "Browser", self.browser_open, 0, cx.listener(|this, _ev, window, cx| {
                this.toggle_browser(window, cx);
            }))))
    }

    /// The active tab's pane tree.
    fn render_term_panes(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.term_tabs.get(self.active_term) else {
            return div().into_any_element();
        };
        // zoomed: the active pane alone, full dock — the tree stays as it is
        if tab.zoomed {
            let only = TermPane::Leaf(tab.active_terminal().clone());
            return self.render_term_pane(&only, Vec::new(), 1.0, false, window, cx);
        }
        let split = tab.terminals().len() > 1;
        self.render_term_pane(&tab.panes, Vec::new(), 1.0, split, window, cx)
    }

    /// One node of the pane tree. Panes are flex children with basis 0 and grow =
    /// their share, so a split starts 50/50 and free space always divides by the
    /// current shares (which the dividers drag).
    fn render_term_pane(
        &self,
        pane: &TermPane,
        path: Vec<usize>,
        weight: f32,
        split: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // this node's share of the parent axis, allowed to shrink below content
        let share = move |d: Div| d.flex_grow(weight).flex_basis(relative(0.)).min_w(px(0.)).min_h(px(0.));
        match pane {
            TermPane::Leaf(t) => {
                let id = t.entity_id();
                // outline the focused pane, but only once there's more than one
                let focused = split && t.read(cx).focus_handle.contains_focused(window, cx);
                // whatever the agent in this pane last reported about itself
                // (idle is deliberately silent here — it means "nothing going on")
                let state = self
                    .pane_state(t)
                    .filter(|(s, _)| !s.is_idle())
                    .map(|(s, m)| (s, m.to_string()));
                let border = match (focused, state.as_ref()) {
                    (true, _) => ACCENT,
                    (false, Some((s, _))) => agent_color(*s),
                    (false, None) => BORDER,
                };
                share(div())
                    .relative()
                    .overflow_hidden()
                    .when(split || state.is_some(), |d| d.border_1().border_color(rgb(border)))
                    .child(t.clone())
                    // after the terminal: children paint in order, so the chip has
                    // to come last to sit on top of the grid instead of under it
                    .when_some(state, |d, (s, msg)| {
                        d.child(self.render_agent_chip(id.as_u64(), s, &msg, cx))
                    })
                    // what this pane is running, bottom-right (the agent chip sits
                    // top-right, so they never collide) — tells you which pane is
                    // which at a glance
                    .when_some(self.pane_cmds.get(&id.as_u64()).cloned(), |d, cmd| {
                        d.child(
                            div()
                                .absolute()
                                .bottom(px(2.))
                                .right(px(4.))
                                .px_1()
                                .rounded_md()
                                .bg(rgb(POPUP_BG))
                                .text_size(px(9.))
                                .text_color(rgb(MUTED))
                                .child(cmd),
                        )
                    })
                    // clicking a pane makes it this tab's focus target
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            let ix = this.active_term;
                            if let Some(tab) = this.term_tabs.get_mut(ix) {
                                tab.active = id;
                            }
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            }
            TermPane::Split { horizontal, kids, shares } => {
                let horizontal = *horizontal;
                let mut group = share(div()).flex();
                group = if horizontal { group.flex_row() } else { group.flex_col() };
                for (i, kid) in kids.iter().enumerate() {
                    if i > 0 {
                        group = group.child(self.render_pane_divider(
                            kids, shares, &path, i, horizontal, cx,
                        ));
                    }
                    let mut kid_path = path.clone();
                    kid_path.push(i);
                    let w = shares.get(i).copied().unwrap_or(1.0);
                    group = group.child(self.render_term_pane(kid, kid_path, w, split, window, cx));
                }
                group.into_any_element()
            }
        }
    }

    /// Status chip floating in a pane's top-right corner: what the agent running
    /// there last reported. Click it to acknowledge (clears the state).
    fn render_agent_chip(&self, pane: u64, state: AgentState, msg: &str, cx: &mut Context<Self>) -> AnyElement {
        let color = agent_color(state);
        div()
            .id(SharedString::from(format!("agent-chip-{pane}")))
            .absolute()
            .top(px(3.))
            .right(px(3.))
            .max_w(px(260.))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py(px(2.))
            .rounded_md()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(color))
            .cursor_pointer()
            .tooltip(|_w, cx| cx.new(|_| TooltipView { text: "Click to clear".into() }).into())
            .child(div().w(px(6.)).h(px(6.)).rounded_full().flex_shrink_0().bg(rgb(color)))
            .child(
                div()
                    .text_size(px(9.))
                    .text_color(rgb(color))
                    .flex_shrink_0()
                    .child(agent_label(state)),
            )
            .when(!msg.is_empty(), |d| {
                d.child(div().text_size(px(9.)).text_color(rgb(MUTED)).truncate().child(msg.to_string()))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _ev, _w, cx| {
                    this.agent_states.remove(&pane);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// Draggable divider between `kids[i - 1]` and `kids[i]`. The drag span comes
    /// from the two subtrees' painted bounds (their outermost leaves), so it is
    /// exact for nested splits too; a first frame with no bounds yet just isn't
    /// draggable.
    fn render_pane_divider(
        &self,
        kids: &[TermPane],
        shares: &[f32],
        path: &[usize],
        i: usize,
        horizontal: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (near, far) = (kids[i - 1].edge_leaf(false), kids[i].edge_leaf(true));
        let extent = match (near.read(cx).bounds(), far.read(cx).bounds()) {
            (Some(a), Some(b)) if horizontal => f32::from(b.right() - a.left()),
            (Some(a), Some(b)) => f32::from(b.bottom() - a.top()),
            _ => 0.,
        };
        let total = shares[i - 1] + shares[i];
        let start_frac = if total > 0. { shares[i - 1] / total } else { 0.5 };
        let drag = PaneDrag {
            path: path.to_vec(),
            ix: i - 1,
            horizontal,
            origin: 0., // filled in on mouse-down from the cursor position
            extent,
            total,
            start_frac,
        };
        let active = self.pane_drag.as_ref().is_some_and(|d| d.path == drag.path && d.ix == drag.ix);
        let id = SharedString::from(format!("term-div-{path:?}-{i}"));
        let d = div()
            .id(id)
            .flex_shrink_0()
            .bg(rgb(if active { ACCENT } else { BORDER }))
            .hover(|s| s.bg(rgb(ACCENT)))
            .cursor(if horizontal { CursorStyle::ResizeLeftRight } else { CursorStyle::ResizeUpDown })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, _window, cx| {
                    if drag.extent <= 0. {
                        return;
                    }
                    let origin = if drag.horizontal { ev.position.x } else { ev.position.y };
                    this.pane_drag = Some(PaneDrag { origin: f32::from(origin), ..drag.clone() });
                    // live PTY resizes while dragging make full-screen apps flood
                    // output — resize once on release instead
                    for t in this.all_terminals() {
                        t.update(cx, |t, _| t.defer_resize = true);
                    }
                    cx.notify();
                }),
            );
        if horizontal { d.w(px(4.)).h_full().into_any_element() } else { d.h(px(4.)).w_full().into_any_element() }
    }

    fn render_term_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = div()
            .h(px(30.))
            .flex()
            .flex_row()
            .items_center()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .overflow_hidden();

        for ix in 0..self.term_tabs.len() {
            let active = ix == self.active_term;
            bar = bar.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .px_3()
                    .gap_2()
                    .border_r_1()
                    .border_color(rgb(BORDER))
                    .when(active, |d| d.bg(rgb(BG)))
                    .when(!active, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    // agent state roll-up for this tab's panes (worst wins), so a
                    // background tab can still say "needs you"
                    .when_some(self.tab_state(&self.term_tabs[ix]), |d, s| {
                        d.child(
                            div()
                                .w(px(6.))
                                .h(px(6.))
                                .rounded_full()
                                .flex_shrink_0()
                                .bg(rgb(agent_color(s))),
                        )
                    })
                    .child(
                        div()
                            .id(("term-tab", ix))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(if active { TEXT } else { MUTED }))
                            .child({
                                let tab = &self.term_tabs[ix];
                                let zoom = if tab.zoomed { " ⤢" } else { "" };
                                let name = tab
                                    .title
                                    .clone()
                                    .unwrap_or_else(|| format!("Terminal {}", ix + 1));
                                match tab.terminals().len() {
                                    1 => format!("{name}{zoom}"),
                                    n => format!("{name} ({n}){zoom}"), // n panes
                                }
                            })
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.switch_terminal(ix, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .id(("term-close", ix))
                            .px_1()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.text_color(rgb(GIT_DELETED)))
                            .child("✕")
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.close_terminal(ix, window, cx);
                            })),
                    ),
            );
        }

        // "+" new terminal
        let bar = bar.child(
            div()
                .id("term-add")
                .px_3()
                .h_full()
                .flex()
                .items_center()
                .cursor_pointer()
                .text_size(px(14.))
                .text_color(rgb(MUTED))
                .hover(|s| s.text_color(rgb(TEXT)))
                .child("+")
                .on_click(cx.listener(|this, _e, window, cx| {
                    this.new_terminal(window, cx);
                })),
        );

        // send pending review comments straight from here — the agent pane is
        // right below this bar, so this is where you are when you want to send
        let ready = self.review.iter().filter(|c| !c.draft && !c.resolved).count();
        bar.when(ready > 0, |b| {
            b.child(div().flex_grow(1.0)).child(
                div()
                    .id("term-send-comments")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .mr_2()
                    .px_1p5()
                    .py(px(1.))
                    .rounded_md()
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(COMMENT_MARK))
                    .text_size(px(10.))
                    .text_color(rgb(COMMENT_MARK))
                    .hover(|s| s.bg(rgba(COMMENT_TINT)))
                    .child(div().font_family(ICON_FONT).text_size(px(11.)).child(IC_COMMENT))
                    .child(format!("Send {ready} comment{}", if ready == 1 { "" } else { "s" }))
                    .tooltip(tip("Send review comments to the agent  (⌥↩)"))
                    .on_click(cx.listener(|this, _e, window, cx| this.send_review(window, cx))),
            )
        })
    }

    /// Bottom-right status toast for a running/finished command. Click to open
    /// the full Run console.
    /// Progress bar for a `--json` build: filled track plus `14/58 · batch 3/7`.
    fn render_build_bar(&self, color: u32) -> Option<impl IntoElement> {
        let p = self.run_progress.clone()?;
        // nothing to show until the plan arrives (or an empty build ends)
        if p.total == 0 && !p.done {
            return None;
        }
        let color = if p.failed.is_empty() { color } else { GIT_DELETED };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div().h(px(4.)).w_full().rounded_md().bg(rgb(BORDER)).child(
                        div().h_full().w(relative(p.fraction())).rounded_md().bg(rgb(color)),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .text_size(px(10.))
                        .text_color(rgb(MUTED))
                        .child(div().flex_grow(1.0).min_w(px(0.)).truncate().child(p.summary()))
                        .when(p.from_cache > 0, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(rgb(LINE_NUMBER))
                                    .child(format!("{} cached", p.from_cache)),
                            )
                        }),
                ),
        )
    }

    fn render_run_toast(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let (glyph, color, msg) = if self.run_running {
            (SPIN[self.run_spin % SPIN.len()].to_string(), ACCENT, format!("Running  {}", self.run_cmd))
        } else if self.run_failed {
            ("✕".to_string(), GIT_DELETED, format!("{} failed", self.run_cmd))
        } else {
            ("✓".to_string(), GIT_NEW, format!("{} done", self.run_cmd))
        };
        let label = if self.run_running { "RUNNING" } else if self.run_failed { "FAILED" } else { "DONE" };
        div()
            .id("run-toast")
            .min_w(px(340.))
            .max_w(px(560.))
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .pl_3()
            .pr_4()
            .py_3()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(color))
            .border_l_4()
            .rounded_md()
            .shadow_lg()
            .overflow_hidden()
            .cursor_pointer()
            // big state glyph
            .child(div().flex_shrink_0().text_size(px(20.)).text_color(rgb(color)).child(glyph))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_grow(1.0)
                    .min_w(px(0.))
                    .gap_1()
                    .child(div().text_size(px(11.)).text_color(rgb(color)).child(label))
                    .child(div().text_size(px(14.)).text_color(rgb(TEXT)).truncate().child(msg))
                    .children(self.render_build_bar(color)),
            )
            .on_click(cx.listener(|this, _e, _w, cx| {
                this.run_open = true;
                this.run_active = false;
                cx.notify();
            }))
    }

    /// "How stale is my build?" — the count `yarn build --preview` reports, or
    /// a check when nothing needs building. Click it to run the build.
    fn render_build_chip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.root.join("scripts/buildPackages").is_dir() {
            return None; // no package build here at all
        }
        let checking = self.build_checking;
        // the chip is always on show, so its absence never has to be interpreted
        // always a ratio, so the number means the same thing in both states
        let (label, color) = match (&self.build_preview, checking) {
            (_, true) => ("checking…".to_string(), MUTED),
            (None, _) => ("build ?".to_string(), MUTED),
            (Some(p), _) if p.to_build == 0 => {
                (format!("{}/{} packages built", p.total, p.total), GIT_NEW)
            }
            (Some(p), _) => (
                format!("{}/{} packages built", p.up_to_date, p.total),
                GIT_MODIFIED,
            ),
        };
        let mut tip_text = match &self.build_preview {
            None => "Haven't checked what needs rebuilding".to_string(),
            Some(p) if p.to_build == 0 => format!("All {} packages are up to date", p.total),
            Some(p) => format!(
                "{} of {} packages need a rebuild  ·  {} batch{}\n",
                p.to_build,
                p.total,
                p.batches,
                if p.batches == 1 { "" } else { "es" }
            ),
        };
        // a handful of names is useful; the whole list isn't
        if let Some(p) = &self.build_preview {
            for name in p.packages.iter().take(12) {
                tip_text.push_str(&format!("\n{name}"));
            }
            if p.packages.len() > 12 {
                tip_text.push_str(&format!("\n… and {} more", p.packages.len() - 12));
            }
        }
        tip_text.push_str("\n\nclick to re-check  ·  ⌥click to build (⌘⇧B)");
        Some(
            div()
                .id("build-chip")
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_1()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(rgb(color))
                .hover(|s| s.bg(rgb(HOVER)))
                // a dot in the state's colour, like the workspace tally uses
                .child(div().w(px(6.)).h(px(6.)).rounded_full().flex_shrink_0().bg(rgb(color)))
                .child(if checking { "checking…".to_string() } else { label })
                .tooltip(move |_w, cx| {
                    let text = tip_text.clone();
                    cx.new(|_| TooltipView { text: text.into() }).into()
                })
                .on_click(cx.listener(|this, ev: &gpui::ClickEvent, _w, cx| {
                    if ev.modifiers().alt {
                        this.run_command("yb".into(), cx);
                    } else {
                        this.refresh_build_preview(cx);
                    }
                })),
        )
    }

    /// How the other workspaces are doing, as the ⌘E switcher counts them: a
    /// coloured dot and a number per state, most urgent first. `None` with a
    /// single workspace open — there's nothing to compare.
    fn render_ws_tally(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if self.ws_states.len() < 2 {
            return None;
        }
        let mut tally: Vec<(AgentState, usize)> = Vec::new();
        for st in self.ws_states.iter().flatten() {
            match tally.iter_mut().find(|(s, _)| s == st) {
                Some((_, n)) => *n += 1,
                None => tally.push((*st, 1)),
            }
        }
        tally.sort_by_key(|(s, _)| std::cmp::Reverse(s.priority()));
        // workspaces nobody has reported on: without them the numbers don't add
        // up to the workspaces you have open, which is the first thing you check
        let quiet = self.ws_states.iter().filter(|s| s.is_none()).count();
        let mut tip_text = tally
            .iter()
            .map(|(s, n)| format!("{n} {}", agent_label(*s)))
            .collect::<Vec<_>>()
            .join("  ·  ");
        if quiet > 0 {
            if !tip_text.is_empty() {
                tip_text.push_str("  ·  ");
            }
            tip_text.push_str(&format!("{quiet} with nothing reported"));
        }
        let chip = |color: u32, n: usize, hollow: bool| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .rounded_full()
                        .flex_shrink_0()
                        // hollow = nothing reported, so it reads as "unknown"
                        // rather than as a state of its own
                        .when(hollow, |d| d.border_1().border_color(rgb(color)))
                        .when(!hollow, |d| d.bg(rgb(color))),
                )
                .child(div().text_color(rgb(color)).child(format!("{n}")))
        };
        let mut row = div()
            .id("ws-tally")
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_1()
            .rounded_md()
            .cursor_pointer()
            .text_size(px(11.))
            .hover(|s| s.bg(rgb(HOVER)))
            .tooltip(move |_w, cx| {
                let text = format!("{tip_text}  ·  ⌘E to switch");
                cx.new(|_| TooltipView { text: text.into() }).into()
            })
            .on_click(cx.listener(|_this, _e, _w, cx| cx.emit(ProjectNav::Switcher)));
        for (st, n) in tally {
            row = row.child(chip(agent_color(st), n, false));
        }
        if quiet > 0 {
            row = row.child(chip(MUTED, quiet, true));
        }
        Some(row)
    }

    /// Plan usage: a small percentage chip in the topbar. Clicking it opens
    /// `render_usage_overlay`, which is drawn at the root. `None` until the
    /// statusline shim has run (see `docs/usage-bars.md`) — a missing reading is
    /// not a 0% reading.
    fn render_usage_chip(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let l = self.limits.clone()?;
        // both windows on the chip — session first, then weekly — so the common
        // case needs no click; each keeps its own colour
        let pct = |w: &Option<usage::Window>| match w {
            Some(w) => (format!("{}%", w.pct.round() as i64), usage_color(w.pct)),
            None => ("—".to_string(), MUTED),
        };
        let (five_txt, five_color) = pct(&l.five_hour);
        let (week_txt, week_color) = pct(&l.seven_day);
        // the tooltip is where the numbers get their names
        let line = |label: &str, w: &Option<usage::Window>| match w {
            Some(w) if w.resets_at > 0 => format!(
                "{label}: {}%  ·  resets {}",
                w.pct.round() as i64,
                usage::fmt_reset(w.resets_at)
            ),
            Some(w) => format!("{label}: {}%", w.pct.round() as i64),
            None => format!("{label}: no reading yet"),
        };
        let tip_text = format!(
            "{}\n{}",
            line("Session (5 hours)", &l.five_hour),
            line("Weekly", &l.seven_day)
        );
        let open = self.limits_open;
        Some(
            div()
                .id("usage-chip")
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_1p5()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(11.))
                .when(open, |d| d.bg(rgb(HOVER)))
                .hover(|s| s.bg(rgb(HOVER)))
                .child(div().font_family(ICON_FONT).text_size(px(11.)).text_color(rgb(MUTED)).child(IC_GRAPH))
                .child(div().text_color(rgb(five_color)).child(five_txt))
                .child(div().text_color(rgb(LINE_NUMBER)).child("/"))
                .child(div().text_color(rgb(week_color)).child(week_txt))
                .tooltip(move |_w, cx| {
                    let text = tip_text.clone();
                    cx.new(|_| TooltipView { text: text.into() }).into()
                })
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.limits_open = !this.limits_open;
                    cx.notify();
                })),
        )
    }

    /// The chip's detail panel, rendered at the root so nothing paints over it —
    /// inside the topbar it was drawn before the editor and disappeared behind it.
    /// A transparent backdrop takes the dismissing click.
    fn render_usage_overlay(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let l = self.limits.clone()?;
        Some(
            div()
                .absolute()
                .inset_0()
                .id("usage-backdrop")
                .child(self.render_usage_panel(&l))
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.limits_open = false;
                    cx.notify();
                })),
        )
    }

    fn render_usage_panel(&self, l: &usage::Limits) -> impl IntoElement {
        let row = |label: &'static str, w: &Option<usage::Window>, color: u32| {
            let w = w.clone();
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .flex_grow(1.0)
                                .text_size(px(10.))
                                .text_color(rgb(MUTED))
                                .child(label),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(rgb(match &w {
                                    Some(w) => usage_color(w.pct),
                                    None => MUTED,
                                }))
                                .child(match &w {
                                    Some(w) => format!("{}%", w.pct.round() as i64),
                                    None => "—".to_string(),
                                }),
                        ),
                )
                // the bar: a filled track, so the number has a shape to go with it
                .child(
                    div()
                        .h(px(5.))
                        .w_full()
                        .rounded_md()
                        .bg(rgb(BORDER))
                        .child(
                            div()
                                .h_full()
                                .w(relative(w.as_ref().map(|w| w.pct / 100.0).unwrap_or(0.0)))
                                .rounded_md()
                                .bg(rgb(color)),
                        ),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(LINE_NUMBER))
                        .child(match &w {
                            Some(w) if w.resets_at > 0 => format!("resets {}", usage::fmt_reset(w.resets_at)),
                            Some(_) => String::new(),
                            None => "no reading yet".to_string(),
                        }),
                )
        };
        div()
            .absolute()
            .top(px(46.))
            .right(px(12.))
            .w(px(260.))
            .p_3()
            .flex()
            .flex_col()
            .gap_3()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .child(row("SESSION", &l.five_hour, ACCENT))
            .child(row("WEEKLY", &l.seven_day, GIT_MODIFIED))
    }

    /// The read-only Run console: a bottom dock streaming command output.
    fn render_run(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.run_running;
        // last run exited non-zero → tint the console red so failures are obvious
        let failed = self.run_failed && !running;
        let btn = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .px_2()
                .cursor_pointer()
                .text_size(px(13.))
                .text_color(rgb(MUTED))
                .hover(|s| s.text_color(rgb(TEXT)))
                .child(glyph)
        };
        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(if failed { GIT_DELETED } else { BORDER }))
            .child(
                div()
                    .font_family(ICON_FONT)
                    .text_size(px(12.))
                    .text_color(rgb(if running { ACCENT } else if failed { GIT_DELETED } else { MUTED }))
                    .child(IC_RUN),
            )
            .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child(self.run_cmd.clone()))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(if running { ACCENT } else if failed { GIT_DELETED } else { MUTED }))
                    .child(if running { "running…" } else if failed { "failed" } else { "done" }),
            )
            // the build's own progress, inline in the header
            .children(self.render_build_bar(if running { ACCENT } else { GIT_NEW }).map(|bar| {
                div().flex_grow(1.0).min_w(px(120.)).max_w(px(420.)).child(bar)
            }))
            .child(div().flex_1())
            .child(btn("run-rerun", "↻").tooltip(tip("Re-run")).on_click(cx.listener(|this, _e, _w, cx| {
                let c = this.run_cmd.clone();
                if !c.is_empty() {
                    this.run_command(c, cx);
                }
            })))
            .child(btn("run-clear", "⌫").tooltip(tip("Clear")).on_click(cx.listener(|this, _e, _w, cx| {
                this.run_lines.clear();
                cx.notify();
            })))
            .child(btn("run-close", "✕").tooltip(tip("Close")).on_click(cx.listener(|this, _e, _w, cx| {
                this.run_open = false;
                cx.notify();
            })));

        // read-only Editor as the log view → text is selectable + copyable
        div()
            .h(px(260.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .border_t_1()
            .border_color(rgb(if failed { GIT_DELETED } else { BORDER }))
            .child(header)
            .child(div().flex_grow(1.0).min_h(px(0.)).child(self.run_editor.clone()))
    }

    fn render_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = px(self.tree_width);
        let filter_focused =
            self.commit_filter_focus.is_focused(window) || self.changes_focus.is_focused(window);
        let msg_focused = self.commit_focus.is_focused(window);
        let active_path = self.active_path().cloned();
        let mut changed: Vec<(PathBuf, GitState)> =
            self.git_status.iter().map(|(p, s)| (p.clone(), *s)).collect();
        changed.sort_by(|a, b| a.0.cmp(&b.0));
        // progress reflects the whole change set, before any filtering
        let total = changed.len();
        let checked_count = changed.iter().filter(|(p, _)| self.commit_checked.contains(p)).count();
        let pct = if total > 0 { checked_count * 100 / total } else { 0 };
        // case-insensitive substring filter
        let filter = self.commit_filter.text.to_lowercase();
        if !filter.is_empty() {
            changed.retain(|(p, _)| self.rel(p).to_lowercase().contains(&filter));
        }
        // checked-state filter
        let view_filter = self.commit_view_filter;
        changed.retain(|(p, _)| match view_filter {
            CommitFilter::All => true,
            CommitFilter::Checked => self.commit_checked.contains(p),
            CommitFilter::Unchecked => !self.commit_checked.contains(p),
        });

        // build a directory tree from the changed paths, then flatten to rows
        let mut root_node = ChangeDir::default();
        for (p, s) in &changed {
            let rel = self.rel(p);
            let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
            root_node.insert(&comps, p.clone(), *s);
        }
        let all_changed: Vec<PathBuf> = changed.iter().map(|(p, _)| p.clone()).collect();
        let root_collapsed = self.commit_collapsed.contains(Path::new(""));
        let mut rows = Vec::new();
        if !root_collapsed {
            flatten_changes(&root_node, Path::new(""), 1, &self.commit_collapsed, &mut rows);
        }

        // a small square checkbox glyph
        let checkbox = |checked: bool| {
            div()
                .size(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(rgb(if checked { ACCENT } else { MUTED }))
                .when(checked, |d| d.bg(rgb(ACCENT)))
                .text_size(px(10.))
                .text_color(rgb(SEL_TEXT))
                .cursor_pointer()
                .child(if checked { "✓" } else { "" })
        };

        let mut list = div()
            .id("changes-list")
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_h(px(0.)) // let the flex child shrink so the list actually scrolls
            .overflow_y_scroll()
            .track_focus(&self.changes_focus)
            .on_key_down(cx.listener(Self::changes_key));

        // root "Changes" group row
        {
            let group_checked =
                !all_changed.is_empty() && all_changed.iter().all(|p| self.commit_checked.contains(p));
            let group_files = all_changed.clone();
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .h(px(22.))
                    .w_full()
                    .pl(px(8.))
                    .pr_2()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(
                        div()
                            .id("changes-chevron")
                            .w(px(16.))
                            .flex()
                            .justify_center()
                            .text_size(px(13.))
                            .text_color(rgb(DIR))
                            .cursor_pointer()
                            .child(if root_collapsed { "▸" } else { "▾" })
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.toggle_collapsed(PathBuf::new());
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("changes-check")
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.toggle_checked_all(&group_files);
                                cx.notify();
                            }))
                            .child(checkbox(group_checked)),
                    )
                    .child(div().text_color(rgb(TEXT)).child("Changes"))
                    .child(
                        div()
                            .text_color(rgb(MUTED))
                            .text_size(px(11.))
                            .child(format!("{}", all_changed.len())),
                    ),
            );
        }

        for (i, row) in rows.into_iter().enumerate() {
            match row {
                CommitRow::Dir { depth, key, label, files } => {
                    let collapsed = self.commit_collapsed.contains(&key);
                    let dir_checked =
                        !files.is_empty() && files.iter().all(|p| self.commit_checked.contains(p));
                    let n = files.len();
                    let key_toggle = key.clone();
                    let files_toggle = files.clone();
                    let dir_abs = self.root.join(&key);
                    let is_dir_sel = self.commit_selected.as_ref() == Some(&dir_abs);
                    let dir_select = dir_abs.clone();
                    let dir_ctx = dir_abs.clone();
                    list = list.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .h(px(22.))
                            .w_full()
                            .pl(px(8. + depth as f32 * 14.))
                            .pr_2()
                            .when(is_dir_sel, |d| d.bg(rgb(SELECTED_BG)))
                            .when(!is_dir_sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                            // right-click → context menu (Refresh / Delete)
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                    this.tree_ctx = Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                                    this.tree_ctx_path = Some(dir_ctx.clone());
                                    this.commit_selected = Some(dir_ctx.clone());
                                    window.focus(&this.tree_ctx_focus, cx);
                                    cx.notify();
                                }),
                            )
                            .child(
                                div()
                                    .id(("cdir-chev", i))
                                    .w(px(16.))
                                    .flex()
                                    .justify_center()
                                    .text_size(px(13.))
                                    .text_color(rgb(DIR))
                                    .cursor_pointer()
                                    .child(if collapsed { "▸" } else { "▾" })
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.toggle_collapsed(key_toggle.clone());
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .id(("cdir-check", i))
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.toggle_checked_all(&files_toggle);
                                        cx.notify();
                                    }))
                                    .child(checkbox(dir_checked)),
                            )
                            .child(
                                div()
                                    .id(("cdir-label", i))
                                    .flex_grow(1.0)
                                    .cursor_pointer()
                                    .text_color(rgb(if is_dir_sel { SEL_TEXT } else { DIR }))
                                    .child(label)
                                    .on_click(cx.listener(move |this, _e, window, cx| {
                                        this.commit_selected = Some(dir_select.clone());
                                        window.focus(&this.changes_focus, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                div()
                                    .text_color(rgb(MUTED))
                                    .text_size(px(11.))
                                    .child(format!("{}", n)),
                            ),
                    );
                }
                CommitRow::File { depth, path, name, state } => {
                    // highlight the open tab or the currently-selected row
                    let is_open = active_path.as_ref() == Some(&path)
                        || self.commit_selected.as_ref() == Some(&path);
                    let checked = self.commit_checked.contains(&path);
                    let color = match state {
                        GitState::New => GIT_NEW,
                        GitState::Modified => GIT_MODIFIED,
                        GitState::Deleted => GIT_DELETED,
                    };
                    let (badge, badge_color) = ext_badge(&path);
                    let path_open = path.clone();
                    let path_check = path.clone();
                    let path_ctx = path.clone();
                    list = list.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .h(px(22.))
                            .w_full()
                            .pl(px(8. + depth as f32 * 14.))
                            .pr_2()
                            .when(is_open, |d| d.bg(rgb(SELECTED_BG)))
                            .when(!is_open, |d| d.hover(|s| s.bg(rgb(HOVER))))
                            // right-click → context menu (Refresh / Delete)
                            .on_mouse_down(
                                MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                    this.tree_ctx = Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                                    this.tree_ctx_path = Some(path_ctx.clone());
                                    this.commit_selected = Some(path_ctx.clone());
                                    window.focus(&this.tree_ctx_focus, cx);
                                    cx.notify();
                                }),
                            )
                            .child(div().w(px(16.))) // chevron spacer
                            .child(
                                div()
                                    .id(("cfile-check", i))
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.toggle_checked(path_check.clone());
                                        cx.notify();
                                    }))
                                    .child(checkbox(checked)),
                            )
                            .child(
                                div()
                                    .w(px(16.))
                                    .flex()
                                    .justify_center()
                                    .text_size(px(9.))
                                    .text_color(rgb(if is_open { SEL_TEXT } else { badge_color }))
                                    .child(badge),
                            )
                            .child(
                                div()
                                    .id(("cfile", i))
                                    .flex_grow(1.0)
                                    .cursor_pointer()
                                    .text_color(rgb(if is_open { SEL_TEXT } else { color }))
                                    .child(name)
                                    // single-click selects; double-click opens + closes the pane
                                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                                        this.commit_selected = Some(path_open.clone());
                                        if ev.click_count() >= 2 {
                                            this.open_commit_file(path_open.clone(), true, window, cx);
                                        } else {
                                            window.focus(&this.changes_focus, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            // pending review comments on this file
                            .when(self.comment_count(&path) > 0, |d| {
                                d.child(comment_marker(self.comment_count(&path), 11.))
                            }),
                    );
                }
            }
        }

        let count = changed.len();

        div()
            .flex()
            .flex_col()
            .w(width)
            .h_full()
            .bg(rgb(BG))
            // header
            .child(
                div()
                    .h(px(32.))
                    .px_3()
                    .flex()
                    .items_center()
                    .text_color(rgb(MUTED))
                    .text_size(px(12.))
                    .child(format!("COMMIT  ·  {} changed", count))
                    .child(div().flex_grow(1.0))
                    .child(self.copy_paths_btn("copy-changed", all_changed.clone(), cx))
                    .child(self.collapse_left_btn(cx)),
            )
            // filter bar
            .child(
                div()
                    .id("commit-filter")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .px_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .track_focus(&self.commit_filter_focus)
                    .on_key_down(cx.listener(Self::commit_filter_key))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, window, cx| {
                            window.focus(&this.commit_filter_focus, cx);
                            cx.notify();
                        }),
                    )
                    .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(MUTED)).child(IC_SEARCH))
                    .child(if self.commit_filter.is_empty() {
                        div().flex_grow(1.0).text_size(px(12.)).text_color(rgb(MUTED)).child(format!("Filter files…{}", self.caret_if(filter_focused)))
                    } else {
                        div().flex_grow(1.0).text_size(px(12.)).text_color(rgb(TEXT)).child(self.commit_filter.render(self.caret_if(filter_focused), SELECTION))
                    })
                    .child(self.tree_fold_btns(
                        "changes-collapse",
                        "changes-expand",
                        |this, c, cx| this.changes_collapse_all(c, cx),
                        cx,
                    )),
            )
            // checked-state segmented control
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .h(px(28.))
                    .px_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(self.commit_filter_chip("c-vf-all", "All", CommitFilter::All, cx))
                    .child(self.commit_filter_chip("c-vf-unchecked", "Unchecked", CommitFilter::Unchecked, cx))
                    .child(self.commit_filter_chip("c-vf-checked", "Checked", CommitFilter::Checked, cx)),
            )
            // changed files (top, grows)
            .child(if changed.is_empty() {
                div()
                    .flex_grow(1.0)
                    .px_3()
                    .text_color(rgb(MUTED))
                    .text_size(px(12.))
                    .child("No changes")
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            // progress bar (checked / total)
            .child({
                let bar_w = f32::from(width) - 24.0;
                let filled = (bar_w * (pct as f32 / 100.0)).max(0.0);
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(format!("{}/{} checked", checked_count, total))
                            .child(format!("{}%", pct)),
                    )
                    .child(
                        div()
                            .w(px(bar_w))
                            .h(px(6.))
                            .rounded_sm()
                            .bg(rgb(HOVER))
                            .child(div().w(px(filled)).h_full().rounded_sm().bg(rgb(progress_color(pct)))),
                    )
            })
            // commit box (bottom)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .p_2()
                    .gap_2()
                    .child(
                        div()
                            .id("commit-msg")
                            .min_h(px(72.))
                            .p_2()
                            .bg(rgb(BG))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .rounded_md()
                            .text_color(rgb(TEXT))
                            .text_size(px(12.))
                            .track_focus(&self.commit_focus)
                            .on_key_down(cx.listener(Self::commit_key))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| {
                                    window.focus(&this.commit_focus, cx);
                                    cx.notify();
                                }),
                            )
                            .child(if self.commit_msg.is_empty() {
                                // placeholder doubles as the quick-commit message
                                div().text_color(rgb(MUTED)).child(format!("{}wip: {}", self.caret_if(msg_focused), self.wip_id))
                            } else {
                                div().text_color(rgb(TEXT)).child(self.commit_msg.render(self.caret_if(msg_focused), SELECTION))
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div()
                                    .id("commit-btn")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .bg(rgb(ACCENT))
                                    .text_color(rgb(SEL_TEXT))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .child("Commit")
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.do_commit(false, window, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("commit-push-btn")
                                    .px_3()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .text_color(rgb(TEXT))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(HOVER)))
                                    .child("Commit & Push")
                                    .on_click(cx.listener(|this, _e, window, cx| {
                                        this.do_commit(true, window, cx);
                                    })),
                            ),
                    ),
            )
    }

    /// One chip of the PR viewed-state segmented control.
    fn pr_filter_chip(
        &self,
        id: &'static str,
        label: &'static str,
        variant: PrViewFilter,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.pr_view_filter == variant;
        div()
            .id(id)
            .px_2()
            .py(px(2.))
            .rounded_md()
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(rgb(if active { SEL_TEXT } else { MUTED }))
            .when(active, |d| d.bg(rgb(ACCENT)))
            .when(!active, |d| d.hover(|s| s.bg(rgb(HOVER))))
            .child(label)
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.pr_view_filter = variant;
                cx.notify();
            }))
    }

    fn commit_filter_chip(
        &self,
        id: &'static str,
        label: &'static str,
        variant: CommitFilter,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.commit_view_filter == variant;
        div()
            .id(id)
            .px_2()
            .py(px(2.))
            .rounded_md()
            .cursor_pointer()
            .text_size(px(11.))
            .text_color(rgb(if active { SEL_TEXT } else { MUTED }))
            .when(active, |d| d.bg(rgb(ACCENT)))
            .when(!active, |d| d.hover(|s| s.bg(rgb(HOVER))))
            .child(label)
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.commit_view_filter = variant;
                cx.notify();
            }))
    }

    /// Render one row of the virtualized PR list (index 0 = the root group).
    fn pr_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        if ix == 0 {
            let root_collapsed = self.pr_collapsed.contains(Path::new(""));
            let group_files = self.pr_shown_files.clone();
            let group_viewed =
                !group_files.is_empty() && group_files.iter().all(|p| self.pr_viewed.contains(p));
            let n = group_files.len();
            return div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(22.))
                .w_full()
                .pl(px(8.))
                .pr_2()
                .hover(|s| s.bg(rgb(HOVER)))
                .child(
                    div()
                        .id("pr-chevron")
                        .w(px(16.))
                        .flex()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(rgb(DIR))
                        .cursor_pointer()
                        .child(if root_collapsed { "▸" } else { "▾" })
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.toggle_pr_collapsed(PathBuf::new());
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id("pr-check-all")
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.toggle_pr_viewed_all(&group_files);
                            cx.notify();
                        }))
                        .child(check_box(group_viewed)),
                )
                .child(div().text_color(rgb(TEXT)).child("Changes"))
                .child(div().text_color(rgb(MUTED)).text_size(px(11.)).child(format!("{}", n)))
                .into_any_element();
        }

        match &self.pr_rows[ix - 1] {
            CommitRow::Dir { depth, key, label, files } => {
                let (depth, key, label, files) = (*depth, key.clone(), label.clone(), files.clone());
                let collapsed = self.pr_collapsed.contains(&key);
                let dir_viewed = !files.is_empty() && files.iter().all(|p| self.pr_viewed.contains(p));
                let n = files.len();
                let key_toggle = key.clone();
                let files_toggle = files.clone();
                let dir_abs = self.root.join(&key);
                let is_dir_sel = self.pr_selected.as_ref() == Some(&dir_abs);
                let dir_select = dir_abs.clone();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .h(px(22.))
                    .w_full()
                    .pl(px(8. + depth as f32 * 14.))
                    .pr_2()
                    .when(is_dir_sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!is_dir_sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .child(
                        div()
                            .id(("pr-dir-chev", ix))
                            .w(px(16.))
                            .flex()
                            .justify_center()
                            .text_size(px(13.))
                            .text_color(rgb(DIR))
                            .cursor_pointer()
                            .child(if collapsed { "▸" } else { "▾" })
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.toggle_pr_collapsed(key_toggle.clone());
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id(("pr-dir-check", ix))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.toggle_pr_viewed_all(&files_toggle);
                                cx.notify();
                            }))
                            .child(check_box(dir_viewed)),
                    )
                    .child(
                        div()
                            .font_family(ICON_FONT)
                            .text_size(px(12.))
                            .text_color(rgb(FOLDER_ICON))
                            .child(IC_FOLDER),
                    )
                    .child(
                        div()
                            .id(("pr-dir-label", ix))
                            .flex_grow(1.0)
                            .cursor_pointer()
                            .text_color(rgb(if is_dir_sel { SEL_TEXT } else { DIR }))
                            .child(label)
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.pr_selected = Some(dir_select.clone());
                                window.focus(&this.pr_focus, cx);
                                cx.notify();
                            })),
                    )
                    .child(div().text_color(rgb(MUTED)).text_size(px(11.)).child(format!("{}", n)))
                    .into_any_element()
            }
            CommitRow::File { depth, path, name, state } => {
                let (depth, path, name, state) = (*depth, path.clone(), name.clone(), *state);
                let is_sel = self.pr_selected.as_ref() == Some(&path);
                let is_viewed = self.pr_viewed.contains(&path);
                let color = pr_status_color(state);
                let (badge, badge_color) = ext_badge(&path);
                let path_check = path.clone();
                let path_click = path.clone();
                let path_ctx = path.clone();
                div()
                    .id(("pr-file-row", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .h(px(22.))
                    .w_full()
                    .pl(px(8. + depth as f32 * 14.))
                    .pr_2()
                    .cursor_pointer()
                    .when(is_sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!is_sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    // click anywhere on the row to select; double-click opens the diff
                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                        this.pr_selected = Some(path_click.clone());
                        window.focus(&this.pr_focus, cx);
                        if ev.click_count() >= 2 {
                            this.open_pr_diff(path_click.clone(), window, cx);
                        }
                        cx.notify();
                    }))
                    // right-click: comment on the file as a whole
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                            this.pr_selected = Some(path_ctx.clone());
                            let pos = (f32::from(ev.position.x), f32::from(ev.position.y));
                            this.open_review_ctx(path_ctx.clone(), pos, window, cx);
                        }),
                    )
                    .child(div().w(px(16.)))
                    .child(
                        div()
                            .id(("pr-file-check", ix))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.toggle_pr_viewed(path_check.clone());
                                cx.notify();
                            }))
                            .child(check_box(is_viewed)),
                    )
                    .child(
                        div()
                            .w(px(16.))
                            .flex()
                            .justify_center()
                            .text_size(px(9.))
                            .text_color(rgb(if is_sel { SEL_TEXT } else { badge_color }))
                            .child(badge),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            // status color always (viewed is shown by the checkbox, not by graying)
                            .text_color(rgb(if is_sel { SEL_TEXT } else { color }))
                            .child(name),
                    )
                    // pending review comments on this file
                    .when(self.comment_count(&path) > 0, |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.))
                                .text_color(rgb(COMMENT_MARK))
                                .child(format!("✎{}", self.comment_count(&path))),
                        )
                    })
                    .into_any_element()
            }
        }
    }

    fn render_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = px(self.tree_width);
        let filter_focused =
            self.pr_filter_focus.is_focused(window) || self.pr_focus.is_focused(window);

        // case-insensitive substring filter + viewed-state filter
        let filter = self.pr_filter.text.to_lowercase();
        let view_filter = self.pr_view_filter;
        let filtered: Vec<(PathBuf, GitState)> = self
            .pr_files
            .iter()
            .filter(|(p, _)| filter.is_empty() || self.rel(p).to_lowercase().contains(&filter))
            .filter(|(p, _)| match view_filter {
                PrViewFilter::All => true,
                PrViewFilter::Viewed => self.pr_viewed.contains(p),
                PrViewFilter::Unviewed => !self.pr_viewed.contains(p),
            })
            .cloned()
            .collect();

        // build the directory tree from the (filtered) PR files
        let mut root_node = ChangeDir::default();
        for (p, s) in &filtered {
            let rel = self.rel(p);
            let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
            root_node.insert(&comps, p.clone(), *s);
        }
        // the list/group reflect the filtered set
        let all_files: Vec<PathBuf> = filtered.iter().map(|(p, _)| p.clone()).collect();
        let shown = all_files.len();
        // progress reflects the whole PR, not the filtered view
        let total = self.pr_files.len();
        let viewed = self.pr_files.iter().filter(|(p, _)| self.pr_viewed.contains(p)).count();
        let pct = if total > 0 { viewed * 100 / total } else { 0 };
        let root_collapsed = self.pr_collapsed.contains(Path::new(""));
        let mut rows = Vec::new();
        if !root_collapsed {
            flatten_changes(&root_node, Path::new(""), 1, &self.pr_collapsed, &mut rows);
        }

        // store the flattened rows and render the list virtualized (only the
        // visible rows are laid out — keeps hover snappy on huge PRs)
        self.pr_shown_files = all_files;
        self.pr_rows = rows;
        let list = uniform_list(
            "pr-list",
            self.pr_rows.len() + 1, // +1 for the root "Changes" group row
            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                range.map(|ix| this.pr_row(ix, cx)).collect()
            }),
        )
        .track_scroll(&self.pr_scroll)
        .flex_grow(1.0);

        // progress bar (viewed / total)
        let bar_w = f32::from(width) - 24.0;
        let filled = (bar_w * (pct as f32 / 100.0)).max(0.0);
        let progress = div()
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_1()
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(format!("{}/{} viewed", viewed, total))
                    .child(format!("{}%", pct)),
            )
            .child(
                div()
                    .w(px(bar_w))
                    .h(px(6.))
                    .rounded_sm()
                    .bg(rgb(HOVER))
                    .child(div().w(px(filled)).h_full().rounded_sm().bg(rgb(progress_color(pct)))),
            );

        div()
            .flex()
            .flex_col()
            .w(width)
            .h_full()
            .bg(rgb(BG))
            .track_focus(&self.pr_focus)
            .key_context("Pr")
            .on_key_down(cx.listener(Self::pr_key))
            // header
            .child(
                div()
                    .h(px(32.))
                    .px_3()
                    .flex()
                    .items_center()
                    .text_color(rgb(if self.pr_loading { ACCENT } else { MUTED }))
                    .text_size(px(12.))
                    .child(if self.pr_loading {
                        "PULL REQUEST  ·  refreshing…".to_string()
                    } else if self.pr_base.is_empty() {
                        "PULL REQUEST".to_string()
                    } else {
                        format!("PULL REQUEST  ·  vs {}", self.pr_base)
                    })
                    .child(div().flex_grow(1.0))
                    .child(self.tree_fold_btns("pr-collapse", "pr-expand", |this, c, cx| this.pr_collapse_all(c, cx), cx))
                    // re-diff against the base: ⌘R refreshes the PR chip and the
                    // admin dot, but the file list is loaded separately
                    .child(
                        div()
                            .id("pr-refresh")
                            .flex()
                            .items_center()
                            .justify_center()
                            .h(px(20.))
                            .px_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(if self.pr_loading { ACCENT } else { MUTED }))
                            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                            .child("↻") // same glyph as the status screen's Refresh
                            .tooltip(|_w, cx| {
                                cx.new(|_| TooltipView { text: "Reload changed files  (⌘R)".into() }).into()
                            })
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.refresh_pr_link(cx);
                                this.load_pr(cx);
                            })),
                    )
                    .child(self.collapse_left_btn(cx)),
            )
            // stale-base warning: the list is wrong until origin/<base> is fetched
            .when_some(self.pr_stale.clone(), |d, note| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .bg(rgba(0xe0af6820))
                        .text_size(px(11.))
                        .text_color(rgb(GIT_MODIFIED))
                        .child(div().flex_shrink_0().child("⚠"))
                        .child(div().flex_grow(1.0).min_w(px(0.)).child(note))
                        .child(
                            div()
                                .id("pr-fetch-base")
                                .flex_shrink_0()
                                .px_1()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(GIT_MODIFIED))
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(HOVER)))
                                .child("Fetch")
                                .on_click(cx.listener(|this, _e, _w, cx| this.fetch_pr_base(cx))),
                        ),
                )
            })
            // filter bar (copy button here copies the currently-filtered files)
            .child(
                div()
                    .id("pr-filter")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(28.))
                    .px_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .track_focus(&self.pr_filter_focus)
                    .on_key_down(cx.listener(Self::pr_filter_key))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, window, cx| {
                            window.focus(&this.pr_filter_focus, cx);
                            cx.notify();
                        }),
                    )
                    .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(MUTED)).child(IC_SEARCH))
                    .child(if self.pr_filter.is_empty() {
                        div().flex_grow(1.0).text_size(px(12.)).text_color(rgb(MUTED)).child(format!("Filter files…{}", self.caret_if(filter_focused)))
                    } else {
                        div().flex_grow(1.0).text_size(px(12.)).text_color(rgb(TEXT)).child(self.pr_filter.render(self.caret_if(filter_focused), SELECTION))
                    })
                    .child(self.copy_paths_btn("copy-pr", self.pr_shown_files.clone(), cx)),
            )
            // viewed-state segmented control
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .h(px(28.))
                    .px_3()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(self.pr_filter_chip("pr-vf-all", "All", PrViewFilter::All, cx))
                    .child(self.pr_filter_chip("pr-vf-unviewed", "Unviewed", PrViewFilter::Unviewed, cx))
                    .child(self.pr_filter_chip("pr-vf-viewed", "Viewed", PrViewFilter::Viewed, cx)),
            )
            .child(if shown == 0 {
                div()
                    .flex_grow(1.0)
                    .px_3()
                    .text_color(rgb(MUTED))
                    .text_size(px(12.))
                    .child(if total == 0 { "No changes vs base" } else { "No matching files" })
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .child(progress)
    }


    /// The "write a comment" prompt. Parked directly under the line it belongs to
    /// when that line is on screen (so you keep your place in the code), centred
    /// only as a fallback — a different file's tab, or a scrolled-out line.
    /// `inline`: drop the absolute positioning and fill the width of whatever
    /// contains it, so the box takes part in layout and pushes content down —
    /// what the editor's gap does for a file, done structurally for the
    /// transcript.
    fn render_comment_prompt(
        &mut self,
        rc_focused: bool,
        inline: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let head = match &self.rc_target {
            // there's no file to name here; say what's being commented on and
            // which rows of the answer it covers
            Some((p, a, b)) if is_agent_src(p) => {
                let where_ = if b > a { format!("lines {a}-{b}") } else { format!("line {a}") };
                let what = if self.rc_reply_mode { "Thread on the answer" } else { "Comment on the answer" };
                format!("{what}  ·  {where_}")
            }
            Some((p, 0, _)) => format!("{}  ·  whole file", self.rel(p)),
            Some((p, a, b)) if b > a => format!("{}:{}-{}", self.rel(p), a, b),
            Some((p, a, _)) => format!("{}:{}", self.rel(p), a),
            None => String::new(),
        };
        // the line's own rect, if the comment belongs to the file on screen
        let anchor = self.rc_target.as_ref().filter(|(_, a, _)| *a > 0).and_then(|(path, _, b)| {
            let tab = self.tabs.get(self.active)?;
            if &tab.path != path {
                return None;
            }
            tab.editor.read(cx).line_screen_rect(*b)
        });
        let notes_above = 0.0;
        // a comment on the agent's answer belongs beside that answer, so it opens
        // at the point the selection was made
        let agent_anchor = self
            .rc_target
            .as_ref()
            .filter(|(p, _, _)| is_agent_src(p))
            .and_then(|_| self.agent_sel_pos);
        let (left, top, w) = match (anchor, agent_anchor) {
            (Some((x, y, avail, h)), _) => (
                x + RC_BOX_PAD,
                y + h + notes_above + RC_BOX_VPAD,
                (avail - RC_BOX_PAD * 2.0).clamp(220.0, 760.0),
            ),
            (None, Some((mx, my))) => {
                let w = (self.term_width - 24.0).clamp(320.0, 640.0);
                let left = (mx - 24.0).clamp(0.0, (self.win_width - w - 8.0).max(0.0));
                let top = (my + 18.0).min((self.win_height - 220.0).max(60.0));
                (left, top, w)
            }
            (None, None) => (((self.win_width - 520.0) / 2.0).max(0.), 120.0, 520.0),
        };
        // the conversation so far: the note, then every turn in the thread. In
        // reply mode the note belongs up here as the first message, not in the
        // input — the input is for what you're about to say.
        let comment = self.rc_target.as_ref().and_then(|(p, a, b)| {
            self.review.iter().find(|c| &c.path == p && c.line == *a && c.end == *b)
        });
        let has_thread = comment.is_some_and(|c| !c.replies.is_empty());
        let mut replies: Vec<ReviewReply> = Vec::new();
        if has_thread {
            if let Some(c) = comment {
                if self.rc_reply_mode {
                    replies.push(ReviewReply { who: "you".into(), text: c.text.clone() });
                }
                replies.extend(c.replies.iter().cloned());
            }
        }
        let thread_h = self.rc_thread_h.unwrap_or(rc_thread_rows(&replies) as f32 * RC_LINE_H);
        let box_h = rc_box_h(&self.rc_query.text, &replies) + self.rc_thread_extra(&replies);
        let body_h = rc_lines(&self.rc_query.text) as f32 * RC_LINE_H;
        // is this box editing a note that already exists?
        let existing_note = self
            .rc_target
            .as_ref()
            .is_some_and(|(p, a, b)| {
                self.review.iter().any(|c| &c.path == p && c.line == *a && c.end == *b)
            });
        // a corner drag pins the width; otherwise it follows the pane
        let box_w = self.rc_width.unwrap_or(w).clamp(280.0, (self.win_width - 40.0).max(280.0));
        div()
            .when(!inline, |d| d.absolute().top(px(top)).left(px(left)).w(px(box_w)))
            .when(inline && self.rc_width.is_some(), |d| d.w(px(box_w)))
            // `relative` only when it isn't already absolutely positioned —
            // setting it on an absolute box overrides that, dropping the box into
            // the layout flow and squeezing the whole window
            .when(inline && self.rc_width.is_none(), |d| d.w_full())
            .when(inline, |d| d.relative().my_1())
            .child(input_sink(self.rc_focus.clone(), cx.entity()))
            // the estimate is a floor: let the box grow if the text wraps further
            .min_h(px(box_h))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(COMMENT_MARK)) // amber, matching the gutter marker
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .overflow_hidden() // a long note must not spill over the code
            .track_focus(&self.rc_focus)
            .on_key_down(cx.listener(Self::rc_key))
            // the box sits over the editor (or inside the transcript): swallow the
            // wheel so scrolling in it never moves what's behind it. `occlude` is
            // what stops a gpui `list`: it registers its scroll handler *after*
            // painting its children, so a mere stop_propagation comes too late —
            // the hitbox has to take the list out of the hit test instead.
            .occlude()
            .on_scroll_wheel(|_e, _w, cx| cx.stop_propagation())
            // `occlude` also hides the root's drag handlers from anything over
            // the box, which left a resize drag stuck on mouse-up. Re-dispatch
            // both here so a drag that lives inside the box still tracks and ends.
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(10.)).text_color(rgb(COMMENT_MARK)).child(head))
                    .child(div().flex_grow(1.0))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(LINE_NUMBER))
                            .child("⌘↩ save · ↩ newline · esc cancel"),
                    ),
            )
            // the answers, read here rather than in the terminal transcript
            .when(!replies.is_empty(), |d| {
                d.child(
                    div()
                        .id("rc-thread")
                        .mx_2()
                        .mt_1()
                        .px_2()
                        .h(px(thread_h))
                        .overflow_y_scroll()
                        // the wheel belongs to this list, not the code behind it
                        .on_scroll_wheel(|_e, _w, cx| cx.stop_propagation())
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .children(replies.iter().map(|r| {
                            let agent = r.who == "agent";
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(1.))
                                .pl_2()
                                .border_l_2()
                                .border_color(rgb(if agent { GIT_NEW } else { COMMENT_MARK }))
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(rgb(if agent { GIT_NEW } else { COMMENT_MARK }))
                                        .child(if agent { "Agent" } else { "You" }),
                                )
                                // the agent writes markdown; render it as such
                                .child(md_text(&r.text, 12.0, TEXT).min_w(px(0.)))
                        })),
                )
            })
            .child(
                div()
                    .mx_2()
                    .mb_2()
                    .mt_1()
                    .px_2()
                    .py_1()
                    // grow with the text (wrapping included) instead of clipping;
                    // the cap is what makes a long note scroll inside the box
                    .min_h(px(body_h + 8.0))
                    .max_h(px(RC_MAX_LINES as f32 * RC_LINE_H + 8.0))
                    .line_height(px(RC_LINE_H))
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .overflow_hidden()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    // the box can open unfocused (an existing thread): clicking
                    // the reply area is what starts typing
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, window, cx| {
                            window.focus(&this.rc_focus, cx);
                            cx.notify();
                        }),
                    )
                    .child({
                        // no caret until the field holds focus, or an unfocused
                        // box looks like it's taking keystrokes
                        let caret = self.caret_if(rc_focused);
                        if self.rc_reply_mode && self.rc_query.is_empty() {
                            div()
                                .text_color(rgb(MUTED))
                                .child(format!("Reply to the agent…{caret}"))
                        } else {
                            div()
                                .mt(px(-rc_scroll_off(&self.rc_query)))
                                .child(self.rc_query.render(caret, SELECTION))
                        }
                    }),
            )
            // footer: the actions, spelled out rather than keyboard-only
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .mx_2()
                    .mb_2()
                    .when(existing_note, |d| {
                        d.child(rc_button("rc-delete", "Delete", false, true).on_click(cx.listener(
                            |this, _e, window, cx| this.delete_open_comment(window, cx),
                        )))
                    })
                    .child(div().flex_grow(1.0))
                    .child(rc_button("rc-cancel", "Cancel", false, false).on_click(cx.listener(
                        |this, _e, window, cx| this.cancel_comment(window, cx),
                    )))
                    .child(
                        rc_button(
                            "rc-save",
                            match (self.rc_reply_mode, existing_note) {
                                (true, _) => "Reply",
                                (false, true) => "Update",
                                (false, false) => "Comment",
                            },
                            true,
                            false,
                        )
                        .on_click(cx.listener(|this, _e, window, cx| this.commit_comment(window, cx))),
                    )
                    // a thread that's been talked through is resolved, not deleted —
                    // last in the row, since it's what ends the conversation
                    .when(!replies.is_empty(), |d| {
                        d.child(
                            // rc_button already carries the hover style — setting
                            // another one panics in gpui
                            rc_button("rc-resolve", "Resolve", false, false)
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .border_color(rgb(GIT_NEW))
                                .child(
                                    div()
                                        .font_family(ICON_FONT)
                                        .text_size(px(10.))
                                        .text_color(rgb(GIT_NEW))
                                        .child(IC_CHECK),
                                )
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    this.resolve_comment(window, cx)
                                })),
                        )
                    }),
            )
            // bottom edge grows the box vertically; the corner grows both axes
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(6.))
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .id("rc-resize")
                            .flex_grow(1.0)
                            .h_full()
                            .cursor(CursorStyle::ResizeUpDown)
                            .bg(rgb(if self.rc_resize.is_some() { ACCENT } else { PANEL_BG }))
                            .hover(|s| s.bg(rgb(ACCENT)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                                    this.rc_resize = Some((
                                        f32::from(ev.position.x),
                                        f32::from(ev.position.y),
                                        box_w,
                                        thread_h,
                                        false,
                                    ));
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("rc-resize-corner")
                            .w(px(14.))
                            .h_full()
                            .flex_shrink_0()
                            .cursor(CursorStyle::ResizeUpLeftDownRight)
                            .bg(rgb(if self.rc_resize.is_some_and(|r| r.4) { ACCENT } else { BORDER }))
                            .hover(|s| s.bg(rgb(ACCENT)))
                            .tooltip(tip("Drag to resize"))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                                    this.rc_resize = Some((
                                        f32::from(ev.position.x),
                                        f32::from(ev.position.y),
                                        box_w,
                                        thread_h,
                                        true,
                                    ));
                                    cx.notify();
                                }),
                            ),
                    ),
            )
    }

    /// Saved notes for the file on screen, parked in the right margin beside the
    /// **first** line of their span — that's the line you were looking at when you
    /// wrote it. A multi-line note shows as many lines as its span covers, so it
    /// never spills past the code it belongs to.
    fn render_inline_notes(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut layer = div().absolute().top(px(0.)).left(px(0.));
        let Some(tab) = self.tabs.get(self.active) else { return layer };
        let path = tab.path.clone();
        // several notes can share a first line; that one shows with a count
        let mut by_line: Vec<(usize, usize, String, usize, usize)> = Vec::new();
        for c in self.review.iter().filter(|c| c.path == path && !c.resolved) {
            match by_line.iter_mut().find(|(l, _, _, _, _)| *l == c.line) {
                Some((_, _, _, extra, answers)) => {
                    *extra += 1;
                    *answers += c.replies.len();
                }
                None => by_line.push((c.line, c.end, c.text.clone(), 0, c.replies.len())),
            }
        }
        if by_line.is_empty() {
            return layer;
        }
        let editor = tab.editor.clone();
        // start the column just past the longest line on screen
        let anchor = editor.read(cx).notes_anchor_x();
        for (i, (line, end, text, extra, answers)) in by_line.into_iter().enumerate() {
            let Some((x, y, avail, h)) = editor.read(cx).line_screen_rect(line) else { continue };
            let start = anchor.unwrap_or(x + avail * 0.55).max(x);
            let w = (x + avail - start).max(140.0);
            // as many rows as the span is tall, capped by the note's own length
            let span_rows = end.saturating_sub(line) + 1;
            let all: Vec<&str> = text.lines().collect();
            let shown = all.len().min(span_rows.max(1));
            let clipped = all.len() > shown;
            let mut body = div().flex().flex_col().flex_grow(1.0).min_w(px(0.));
            for (li, l) in all.iter().take(shown).enumerate() {
                let last_shown = li + 1 == shown;
                let text = if clipped && last_shown { format!("{l} …") } else { (*l).to_string() };
                body = body.child(div().h(px(h)).truncate().child(text));
            }
            layer = layer.child(
                div()
                    .id(("inline-note", i))
                    .absolute()
                    .top(px(y))
                    .left(px(start))
                    .w(px(w))
                    .h(px(h * shown as f32))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_1()
                    .px_1()
                    .rounded_md()
                    .overflow_hidden()
                    .bg(rgba(COMMENT_TINT))
                    .border_l_2()
                    .border_color(rgb(COMMENT_MARK))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(rgb(COMMENT_MARK))
                    .hover(|s| s.bg(rgba(COMMENT_TINT_HOVER)))
                    .child(
                        div()
                            .h(px(h))
                            .flex()
                            .items_center()
                            .flex_shrink_0()
                            .font_family(ICON_FONT)
                            .text_size(px(10.))
                            .child(IC_COMMENT),
                    )
                    .child(body)
                    // an answer is waiting — click the chip to read it
                    .when(answers > 0, |d| {
                        d.child(
                            div()
                                .h(px(h))
                                .flex()
                                .items_center()
                                .flex_shrink_0()
                                .text_color(rgb(GIT_NEW))
                                .child(format!("✓{answers}")),
                        )
                    })
                    .when(extra > 0, |d| {
                        d.child(div().h(px(h)).flex().items_center().flex_shrink_0().child(format!("+{extra}")))
                    })
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        // open this thread: reply if it has answers, else edit
                        if let Some(tab) = this.tabs.get(this.active) {
                            let path = tab.path.clone();
                            this.begin_comment(path, line, end, window, cx);
                        }
                    })),
            );
        }
        layer
    }

    /// The review-comments pane: every pending note, and one button that hands
    /// the whole batch to the agent.
    fn render_review_pane(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = px(self.tree_width);
        let open = |c: &&ReviewComment| !c.resolved;
        let n = self.review.iter().filter(open).filter(|c| !c.draft && !c.sent).count();
        let drafts = self.review.iter().filter(open).filter(|c| c.draft).count();
        let answered = self.review.iter().filter(open).filter(|c| !c.replies.is_empty()).count();
        let waiting =
            self.review.iter().filter(open).filter(|c| c.sent && c.replies.is_empty()).count();
        let resolved_n = self.review.iter().filter(|c| c.resolved).count();
        // reopening the last one takes the tab away with it: don't strand the
        // pane on a tab that no longer exists
        if resolved_n == 0 {
            self.review_resolved_tab = false;
        }
        let resolved_tab = self.review_resolved_tab;
        let mut list = div().flex().flex_col().min_h(px(0.)).id("review-list").overflow_y_scroll();
        // `i` stays the index into `self.review`: the row's handlers look the
        // comment back up by it
        for (i, c) in self.review.iter().enumerate().filter(|(_, c)| c.resolved == resolved_tab) {
            let head = if is_agent_src(&c.path) {
                if c.end > c.line {
                    format!("the agent's answer  ·  lines {}-{}", c.line, c.end)
                } else {
                    format!("the agent's answer  ·  line {}", c.line)
                }
            } else if c.line == 0 {
                format!("{}  ·  whole file", self.rel(&c.path))
            } else if c.end > c.line {
                format!("{}:{}-{}", self.rel(&c.path), c.line, c.end)
            } else {
                format!("{}:{}", self.rel(&c.path), c.line)
            };
            let is_draft = c.draft;
            let sent = c.sent;
            let replies = c.replies.clone();
            let (path, line, end) = (c.path.clone(), c.line, c.end);
            // long threads collapse to the last two turns; the row stays scannable
            let expanded = self.review_expanded.contains(&c.id);
            let hidden = if expanded { 0 } else { replies.len().saturating_sub(2) };
            let cid = c.id.clone();
            list = list.child(
                div()
                    .id(("review-row", i))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_grow(1.0)
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(rgb(COMMENT_MARK))
                                    .child(head),
                            )
                            // where this thread stands
                            .when_some(
                                match (is_draft, sent, replies.is_empty()) {
                                    (true, _, _) => Some(("draft", MUTED)),
                                    (_, true, false) => Some(("answered", GIT_NEW)),
                                    (_, true, true) => Some(("sent", GIT_MODIFIED)),
                                    // unsent with a thread = your reply is queued
                                    (_, false, false) => Some(("your turn", COMMENT_MARK)),
                                    _ => None,
                                },
                                |d, (label, color)| {
                                    d.child(
                                        div()
                                            .flex_shrink_0()
                                            .px_1()
                                            .rounded_md()
                                            .border_1()
                                            .border_color(rgb(color))
                                            .text_size(px(9.))
                                            .text_color(rgb(color))
                                            .child(label),
                                    )
                                },
                            )
                            // a discussed thread is resolved; ✕ still deletes
                            .when(!replies.is_empty() || resolved_tab, |d| {
                                let (p2, l2, e2) = (path.clone(), line, end);
                                d.child(
                                    div()
                                        .id(("review-resolve", i))
                                        .px_1()
                                        .flex_shrink_0()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_size(px(9.))
                                        .text_color(rgb(GIT_NEW))
                                        .hover(|s| s.bg(rgb(HOVER)))
                                        .child(if resolved_tab { "reopen" } else { "resolve" })
                                        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                        .on_click(cx.listener(move |this, _e, _w, cx| {
                                            if let Some(c) = this.review.iter_mut().find(|c| {
                                                c.path == p2 && c.line == l2 && c.end == e2
                                            }) {
                                                c.resolved = !c.resolved;
                                            }
                                            this.sync_comment_gutters(cx);
                                            this.save_review();
                                            let msg = if resolved_tab {
                                                "Thread reopened"
                                            } else {
                                                "Thread resolved — in the Resolved tab"
                                            };
                                            this.show_flash(msg, cx);
                                            cx.notify();
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .id(("review-del", i))
                                    .px_1()
                                    .flex_shrink_0()
                                    .text_size(px(11.))
                                    .text_color(rgb(MUTED))
                                    .hover(|s| s.text_color(rgb(GIT_DELETED)))
                                    .child("✕")
                                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.review.retain(|c| {
                                            !(c.path == path && c.line == line && c.end == end)
                                        });
                                        this.sync_comment_gutters(cx);
                                        this.save_review();
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child(if expanded {
                        c.text.clone()
                    } else {
                        Self::clip_turn(&c.text)
                    }))
                    // how many turns are folded away, and the switch to unfold them
                    .when(hidden > 0 || (expanded && replies.len() > 2), |d| {
                        let cid = cid.clone();
                        d.child(
                            div()
                                .id(("review-thread-toggle", i))
                                .px_1()
                                .rounded_md()
                                .cursor_pointer()
                                .text_size(px(9.))
                                .text_color(rgb(MUTED))
                                .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                .child(if hidden > 0 {
                                    format!("▸ {hidden} earlier turn{}", if hidden == 1 { "" } else { "s" })
                                } else {
                                    "▾ collapse thread".to_string()
                                })
                                .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    if !this.review_expanded.remove(&cid) {
                                        this.review_expanded.insert(cid.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    // the thread: answers the agent posted back to this comment
                    .children(replies.iter().skip(hidden).map(|r| {
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .pl_2()
                            .border_l_2()
                            .border_color(rgb(if r.who == "agent" { GIT_NEW } else { COMMENT_MARK }))
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(rgb(if r.who == "agent" { GIT_NEW } else { COMMENT_MARK }))
                                    .child(if r.who == "agent" { "agent" } else { "you" }),
                            )
                            .child(
                                md_text(
                                    &if expanded { r.text.clone() } else { Self::clip_turn(&r.text) },
                                    11.0,
                                    MUTED,
                                )
                                .flex_grow(1.0),
                            )
                    }))
                    // click the row to open the file at that line *and* its thread,
                    // so the conversation is right there instead of one click away
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        let Some((p, l, e)) =
                            this.review.get(i).map(|c| (c.path.clone(), c.line, c.end))
                        else {
                            return;
                        };
                        // a comment on the answer has no file: show the pane and
                        // scroll to the message it was made on
                        if let Some(msg) = agent_msg_index(&p) {
                            this.show_terminal = true;
                            this.agent_open = true;
                            this.agent_list.scroll_to_reveal_item(msg);
                            this.begin_comment(p, l, e, window, cx);
                            return;
                        }
                        this.open_file(p.clone(), window, cx);
                        if let Some(tab) = this.tabs.get(this.active) {
                            tab.editor.update(cx, |ed, cx| ed.goto(l, 1, cx));
                        }
                        this.begin_comment(p, l, e, window, cx);
                    })),
            );
        }
        div()
            .flex()
            .flex_col()
            .w(width)
            .h_full()
            .bg(rgb(BG))
            .child(
                div()
                    .h(px(32.))
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child({
                        let mut parts = Vec::new();
                        if n > 0 {
                            parts.push(format!("{n} ready"));
                        }
                        if waiting > 0 {
                            parts.push(format!("{waiting} sent"));
                        }
                        if answered > 0 {
                            parts.push(format!("{answered} answered"));
                        }
                        if drafts > 0 {
                            parts.push(format!("{drafts} draft"));
                        }
                        if parts.is_empty() {
                            "REVIEW COMMENTS".to_string()
                        } else {
                            format!("REVIEW COMMENTS  ·  {}", parts.join(" · "))
                        }
                    })
                    .child(div().flex_grow(1.0))
                    .child(self.collapse_left_btn(cx)),
            )
            // Open / Resolved: a resolved thread leaves the list but not the
            // pane, so one filed away too early can be picked back up
            .when(resolved_n > 0, |d| {
                let tab = |label: String, on: bool, to: bool| {
                    div()
                        .id(if to { "review-tab-done" } else { "review-tab-open" })
                        .flex_grow(1.0)
                        .py(px(3.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .text_size(px(11.))
                        .border_b_2()
                        .border_color(rgb(if on { COMMENT_MARK } else { BORDER }))
                        .text_color(rgb(if on { TEXT } else { MUTED }))
                        .hover(|s| s.text_color(rgb(TEXT)))
                        .child(label)
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.review_resolved_tab = to;
                            cx.notify();
                        }))
                };
                let open_n = self.review.len() - resolved_n;
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .mx_3()
                        .mb_2()
                        .child(tab(format!("Open  {open_n}"), !resolved_tab, false))
                        .child(tab(format!("Resolved  {resolved_n}"), resolved_tab, true)),
                )
            })
            // send-to-agent button, disabled-looking when there's nothing to send
            .child(
                div()
                    .id("review-send")
                    .mx_3()
                    .mb_2()
                    .py_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(if n > 0 { COMMENT_MARK } else { BORDER }))
                    .text_size(px(11.))
                    .text_color(rgb(if n > 0 { COMMENT_MARK } else { MUTED }))
                    .when(n > 0, |d| d.cursor_pointer().hover(|s| s.bg(rgba(COMMENT_TINT))))
                    .child(if n == 0 && !self.review_note.is_empty() {
                        "Send note to agent  ·  ⌥↩".to_string()
                    } else {
                        format!("Send {n} to agent  ·  ⌥↩")
                    })
                    .on_click(cx.listener(|this, _e, window, cx| this.send_review(window, cx))),
            )
            .child(if self.review.iter().all(|c| c.resolved != resolved_tab) {
                div()
                    .flex_grow(1.0)
                    .px_3()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(if resolved_tab {
                        "No resolved threads."
                    } else {
                        "No comments yet. ⌥C on a line in the editor or the diff."
                    })
                    .into_any_element()
            } else {
                list.flex_grow(1.0).into_any_element()
            })
            // a general note for the review as a whole, like a PR review body:
            // it leads the batch and isn't tied to any line
            .child({
                let focused = self.review_note_focus.is_focused(window);
                let rows = self.review_note.text.split('\n').count().clamp(3, 10) as f32;
                div()
                    .flex_shrink_0()
                    .relative()
                    .m_2()
                    .child(input_sink(self.review_note_focus.clone(), cx.entity()))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .min_h(px(rows * 18.0))
                            .max_h(px(10.0 * 18.0))
                            .overflow_hidden()
                            .line_height(px(18.))
                            .bg(rgb(BG))
                            .border_1()
                            .border_color(rgb(if focused { ACCENT } else { BORDER }))
                            .rounded_md()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .track_focus(&self.review_note_focus)
                            .on_key_down(cx.listener(Self::review_note_key))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _e, window, cx| {
                                    window.focus(&this.review_note_focus, cx);
                                    cx.notify();
                                }),
                            )
                            .child(if self.review_note.is_empty() {
                                div()
                                    .text_color(rgb(MUTED))
                                    .child(format!("General comment…{}", self.caret_if(focused)))
                                    .into_any_element()
                            } else {
                                self.review_note.render(self.caret_if(focused), SELECTION)
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_grow(1.0)
                                    .text_size(px(10.))
                                    .text_color(rgb(LINE_NUMBER))
                                    .child("⌘↩ send  ·  ↩ newline"),
                            )
                            .child(
                                rc_button("review-note-send", "Send to agent", true, false).on_click(
                                    cx.listener(|this, _e, window, cx| this.send_review(window, cx)),
                                ),
                            ),
                    )
            })
    }

    /// Keys in the general-comment box: ⌘↩ sends the review, esc gives focus back.
    fn review_note_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "enter" if ks.modifiers.platform => self.send_review(window, cx),
            "escape" => self.focus_active(window, cx),
            _ => {
                if !Self::text_comes_via_input_handler(ks) {
                    Self::field_input(&mut self.review_note, ks, cx, |_| true);
                }
            }
        }
        cx.notify();
    }

    /// A transparent resize handle for the find dialog; caller positions/sizes it.
    fn find_resize_handle(
        &self,
        id: &'static str,
        edges: ResizeEdges,
        cursor: CursorStyle,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div().id(id).absolute().cursor(cursor).on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _e, _w, cx| {
                this.find_resize = Some(edges);
                cx.notify();
            }),
        )
    }

    fn render_find(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // positioned at its stored top-left (centered on open); resizing the
        // bottom-right grip keeps this corner fixed, like a macOS window
        let w = self.find_w.clamp(420.0, (self.win_width - 80.0).max(420.0));
        let h = self.find_h.clamp(300.0, (self.win_height - 120.0).max(300.0));
        let left = self.find_left.clamp(0.0, (self.win_width - w).max(0.0));
        let top = self.find_top.clamp(40.0, (self.win_height - h).max(40.0));
        let scope_label = match self.find_scope.as_slice() {
            [one] if *one == self.root => "Project".to_string(),
            [one] => self.rel(one),
            many => format!("{} locations", many.len()),
        };
        let count = self.find_results.len();
        let SearchQuery { excludes, globs, .. } = parse_search_query(&self.find_query.text);

        // ── search input row ──
        let header = div()
            .h(px(42.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().font_family(ICON_FONT).text_color(rgb(MUTED)).child(IC_SEARCH))
            .child(
                div()
                    .flex_grow(1.0)
                    .text_color(rgb(TEXT))
                    .child(if self.find_query.is_empty() {
                        div().text_color(rgb(MUTED)).child(format!("Search in files…  (add *.ts to filter by file){}", self.caret()))
                    } else {
                        div().child(self.find_query.render(self.caret(), SELECTION))
                    }),
            )
            // case-sensitivity toggle (highlighted when on)
            .child({
                let on = self.find_case_sensitive;
                div()
                    .id("find-case")
                    .px_1()
                    .rounded_md()
                    .text_size(px(12.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(rgb(ACCENT)).text_color(rgb(SEL_TEXT)))
                    .when(!on, |d| d.text_color(rgb(MUTED)).hover(|s| s.bg(rgb(HOVER))))
                    .child("Aa")
                    .tooltip(|_w, cx| {
                        cx.new(|_| TooltipView { text: "Match case".into() }).into()
                    })
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.find_case_sensitive = !this.find_case_sensitive;
                        this.run_find_search(cx);
                        cx.notify();
                    }))
            })
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(format!("{} matches", count)),
            );

        // ── scope + refinements row: shows where the search runs and any excludes ──
        let scope_row = div()
            .h(px(22.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .text_size(px(11.))
            .child(div().font_family(ICON_FONT).text_color(rgb(FOLDER_ICON)).child(IC_FOLDER))
            .child(div().text_color(rgb(MUTED)).truncate().child(format!("in {}", scope_label)))
            // positives and negatives read very differently — showing them in one
            // joined list hid whether an exclusion was actually picked up
            .when(globs.iter().any(|g| !g.starts_with('!')), |d| {
                // a folder pattern is passed twice (`api-core*` and its subtree
                // form); the row shows what was typed, once
                let pos: Vec<&str> = globs
                    .iter()
                    .filter(|g| !g.starts_with('!'))
                    .map(|g| g.as_str())
                    .filter(|g| {
                        !g.strip_suffix("/**").is_some_and(|base| globs.iter().any(|o| o == base))
                    })
                    .collect();
                d.child(
                    div()
                        .text_color(rgb(ACCENT))
                        .truncate()
                        .child(format!("· files {}", pos.join(", "))),
                )
            })
            // path exclusions (whole subtrees skipped)
            .when(globs.iter().any(|g| g.starts_with('!')), |d| {
                let mut neg: Vec<String> = Vec::new();
                for g in globs.iter().filter_map(|g| g.strip_prefix('!')) {
                    // show `**/dist/**` as `dist` — the form you typed — and only
                    // once, since a folder pattern is passed in both forms
                    let shown = g.trim_start_matches("**/").trim_end_matches("/**").to_string();
                    if !neg.contains(&shown) {
                        neg.push(shown);
                    }
                }
                d.child(
                    div()
                        .text_color(rgb(GIT_DELETED))
                        .truncate()
                        .child(format!("· not {}", neg.join(", "))),
                )
            })
            // text exclusions (matching lines dropped) — only tokens that aren't
            // already covered by the path list, so nothing is listed twice
            .when(
                excludes.iter().any(|e| !globs.contains(&format!("!**/{e}/**"))),
                |d| {
                    let text: Vec<&str> = excludes
                        .iter()
                        .filter(|e| !globs.contains(&format!("!**/{e}/**")))
                        .map(|e| e.as_str())
                        .collect();
                    d.child(
                        div()
                            .text_color(rgb(GIT_DELETED))
                            .truncate()
                            .child(format!("· lines without {}", text.join(", "))),
                    )
                },
            );

        // ── syntax legend: the four things the box understands, on screen so
        //    the syntax doesn't have to be remembered ──
        let legend = |example: &str, what: &str, color: u32| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .child(div().font_family("Menlo").text_color(rgb(color)).child(example.to_string()))
                .child(div().text_color(rgb(LINE_NUMBER)).child(what.to_string()))
        };
        let hints = div()
            .h(px(20.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .overflow_hidden()
            .border_b_1()
            .border_color(rgb(BORDER))
            .text_size(px(10.))
            .child(legend("*.ts", "file type", ACCENT))
            .child(legend("api-core*", "folder", ACCENT))
            .child(legend("src/**/*.tsx", "path", ACCENT))
            .child(legend("-dist", "skip folder", GIT_DELETED))
            .child(legend("-TODO", "drop lines", GIT_DELETED));

        // ── results list (height = draggable split fraction of the area below
        //    the header; the rest goes to the preview) ──
        let content_h = (h - FIND_HEAD_H).max(160.0);
        let results_h = (self.find_split * content_h).clamp(80.0, content_h - 80.0);
        let mut results = div()
            .id("find-results")
            .flex()
            .flex_col()
            .h(px(results_h))
            .flex_shrink_0()
            .overflow_y_scroll()
            .track_scroll(&self.find_scroll);

        for (i, r) in self.find_results.iter().enumerate() {
            let selected = i == self.find_selected;
            let rel = self.rel(&r.path);
            let text = r.text.clone();
            let line = r.line;
            results = results.child(
                div()
                    .id(("find", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(20.))
                    .px_3()
                    .gap_2()
                    .when(selected, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex_grow(1.0)
                            .text_color(rgb(if selected { SEL_TEXT } else { TEXT }))
                            .child(text),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(format!("{}:{}", rel, line)),
                    )
                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                        this.find_selected = i;
                        // double-click opens the result and closes the dialog (like F4)
                        if ev.click_count() >= 2 {
                            if let Some(r) = this.find_results.get(i).cloned() {
                                this.find_open = false;
                                this.open_file(r.path, window, cx);
                                if let Some(tab) = this.tabs.get(this.active) {
                                    tab.editor.update(cx, |e, cx| e.goto(r.line, 1, cx));
                                }
                            }
                        }
                        cx.notify();
                    })),
            );
        }

        // ── preview of the selected result (syntax-highlighted, cached) ──
        let sel = self.find_results.get(self.find_selected).cloned();
        if let Some(r) = &sel {
            let stale = self.find_preview.as_ref().map(|p| p.path != r.path).unwrap_or(true);
            if stale {
                let content = std::fs::read_to_string(&r.path).unwrap_or_default();
                // Guard against minified bundles / huge files: highlighting the
                // whole file and shaping 100k-char lines runs in render and would
                // freeze the UI. For such files skip highlighting and hard-truncate
                // each line before it's ever shaped.
                const MAX_LINE: usize = 2000;
                let big = content.len() > 512 * 1024
                    || content.split('\n').take(4000).any(|l| l.len() > MAX_LINE);
                let styles = if big { Vec::new() } else { highlighter().highlight(&content, &r.path) };
                let lines = content
                    .split('\n')
                    .map(|s| {
                        if big && s.len() > MAX_LINE {
                            let mut t: String = s.chars().take(MAX_LINE).collect();
                            t.push('…');
                            t
                        } else {
                            s.to_string()
                        }
                    })
                    .collect();
                self.find_preview = Some(FindPreview { path: r.path.clone(), lines, styles });
            }
        }
        // breadcrumb of the selected file (name + its directory), shown above
        // the preview like WebStorm's Find in Files
        let sel_loc = sel.as_ref().map(|r| {
            let name = r.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let dir = r.path.parent().map(|d| self.rel(&d.to_path_buf())).unwrap_or_default();
            (name, dir)
        });

        // normalized selection (start, end) in file-line/char coords
        let psel = self.find_psel.map(|(ar, ac, hr, hc)| {
            if (ar, ac) <= (hr, hc) { ((ar, ac), (hr, hc)) } else { ((hr, hc), (ar, ac)) }
        });
        // blinking caret sits at the selection head (where the cursor is)
        let caret_head = self.find_psel.map(|(_, _, hr, hc)| (hr, hc));
        let caret_on = self.caret_on;
        let char_w = self.find_char_w;
        let mut preview = div()
            .id("find-preview")
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .overflow_y_scroll()
            .track_scroll(&self.find_pscroll)
            .font_family("Menlo")
            .text_size(px(13.))
            .bg(rgb(BG))
            // text selection: click-drag, double-click word, visible as you drag
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                    if let Some((line, col)) = this.find_cell_at(ev.position) {
                        if ev.click_count >= 2 {
                            let l = this
                                .find_preview
                                .as_ref()
                                .and_then(|pv| pv.lines.get(line))
                                .cloned()
                                .unwrap_or_default();
                            let (s, e) = word_range(&l, col);
                            this.find_psel = Some((line, s, line, e));
                            this.find_pdragging = false;
                        } else {
                            this.find_psel = Some((line, col, line, col));
                            this.find_pdragging = true;
                        }
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                if this.find_pdragging {
                    if let Some((line, col)) = this.find_cell_at(ev.position) {
                        if let Some((ar, ac, _, _)) = this.find_psel {
                            this.find_psel = Some((ar, ac, line, col));
                            cx.notify();
                        }
                    }
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _e, _w, _cx| this.find_pdragging = false));

        if let (Some(r), Some(pv)) = (&sel, &self.find_preview) {
            let start = r.line.saturating_sub(6);
            let end = (r.line + 30).min(pv.lines.len());
            for i in start..end {
                let no = i + 1;
                let is_match = no == r.line;
                let line = pv.lines.get(i).map(|s| s.as_str()).unwrap_or("");
                // selection highlight span for this line, if any
                let span: Vec<(usize, usize, u32)> = psel
                    .and_then(|((sr, sc), (er, ec))| {
                        if i < sr || i > er {
                            return None;
                        }
                        let len = line.chars().count();
                        let cs = if i == sr { sc } else { 0 }.min(len);
                        let ce = if i == er { ec } else { len }.min(len);
                        (cs < ce).then_some((cs, ce, SELECTION))
                    })
                    .into_iter()
                    .collect();
                let runs = diff_line_runs(line, pv.styles.get(i), &span);
                let caret_col = caret_head.filter(|(r, _)| *r == i).map(|(_, c)| c);
                preview = preview.child(
                    div()
                        .relative()
                        .flex()
                        .flex_row()
                        .h(px(18.))
                        .when(is_match && span.is_empty(), |d| d.bg(rgb(SEARCH_CURRENT_BG)))
                        // blinking text caret at the cursor position
                        .when_some(caret_col.filter(|_| caret_on), |d, c| {
                            d.child(
                                div()
                                    .absolute()
                                    .top(px(0.))
                                    .left(px(48.0 + 8.0 + c as f32 * char_w))
                                    .w(px(1.5))
                                    .h(px(18.))
                                    .bg(rgb(CURSOR)),
                            )
                        })
                        .child(
                            div()
                                .w(px(48.))
                                .pr_2()
                                .flex()
                                .justify_end()
                                .text_color(rgb(LINE_NUMBER))
                                .child(no.to_string()),
                        )
                        .child(
                            div()
                                .flex_grow(1.0)
                                .px_2()
                                .child(StyledText::new(line.to_string()).with_runs(runs)),
                        ),
                );
            }
        }

        div()
            .absolute()
            .top(px(top))
            .left(px(left))
            .w(px(w))
            .h(px(h))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.find_focus)
            .on_key_down(cx.listener(Self::find_key))
            // title bar: drag to move the dialog (like a window)
            .child(
                div()
                    .id("find-title")
                    .h(px(24.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL_BG))
                    .cursor(CursorStyle::OpenHand)
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("Find in Files")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                            this.find_moving = true;
                            this.find_move_dx = f32::from(ev.position.x) - this.find_left;
                            this.find_move_dy = f32::from(ev.position.y) - this.find_top;
                            cx.notify();
                        }),
                    ),
            )
            .child(header)
            .child(scope_row)
            .child(hints)
            .child(results)
            // draggable divider: drag down → bigger results, up → bigger preview
            .child(
                div()
                    .id("find-split")
                    .h(px(4.))
                    .flex_shrink_0()
                    .bg(rgb(if self.find_split_dragging { ACCENT } else { BORDER }))
                    .cursor(CursorStyle::ResizeUpDown)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, _w, cx| {
                            this.find_split_dragging = true;
                            cx.notify();
                        }),
                    ),
            )
            // breadcrumb: the selected result's file name + its directory
            .when_some(sel_loc, |d, (name, dir)| {
                d.child(
                    div()
                        .h(px(22.))
                        .px_3()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .bg(rgb(PANEL_BG))
                        .text_size(px(11.))
                        .child(div().text_color(rgb(TEXT)).child(name))
                        .child(div().text_color(rgb(MUTED)).truncate().child(dir)),
                )
            })
            .child(preview)
            // bottom-right grip: drag to resize the panel
            .child(
                div()
                    .id("find-resize")
                    .absolute()
                    .right(px(0.))
                    .bottom(px(0.))
                    .size(px(16.))
                    .cursor(CursorStyle::ResizeUpLeftDownRight)
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .flex()
                    .items_end()
                    .justify_end()
                    .child("◢")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, _w, cx| {
                            this.find_resize = Some(ResizeEdges { r: true, b: true, ..Default::default() });
                            cx.notify();
                        }),
                    ),
            )
            // window-style resize handles on every edge + the other corners
            .child(
                self.find_resize_handle("find-rs-l", ResizeEdges { l: true, ..Default::default() }, CursorStyle::ResizeLeftRight, cx)
                    .top(px(8.)).bottom(px(8.)).left(px(0.)).w(px(5.)),
            )
            .child(
                self.find_resize_handle("find-rs-r", ResizeEdges { r: true, ..Default::default() }, CursorStyle::ResizeLeftRight, cx)
                    .top(px(8.)).bottom(px(8.)).right(px(0.)).w(px(5.)),
            )
            .child(
                self.find_resize_handle("find-rs-t", ResizeEdges { t: true, ..Default::default() }, CursorStyle::ResizeUpDown, cx)
                    .left(px(8.)).right(px(8.)).top(px(0.)).h(px(5.)),
            )
            .child(
                self.find_resize_handle("find-rs-b", ResizeEdges { b: true, ..Default::default() }, CursorStyle::ResizeUpDown, cx)
                    .left(px(8.)).right(px(8.)).bottom(px(0.)).h(px(5.)),
            )
            .child(
                self.find_resize_handle("find-rs-tl", ResizeEdges { t: true, l: true, ..Default::default() }, CursorStyle::ResizeUpLeftDownRight, cx)
                    .top(px(0.)).left(px(0.)).size(px(10.)),
            )
            .child(
                self.find_resize_handle("find-rs-tr", ResizeEdges { t: true, r: true, ..Default::default() }, CursorStyle::ResizeUpRightDownLeft, cx)
                    .top(px(0.)).right(px(0.)).size(px(10.)),
            )
            .child(
                self.find_resize_handle("find-rs-bl", ResizeEdges { b: true, l: true, ..Default::default() }, CursorStyle::ResizeUpRightDownLeft, cx)
                    .bottom(px(0.)).left(px(0.)).size(px(10.)),
            )
    }

    fn render_palette(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 560.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let items = self.palette_results.clone();

        let mut list = div()
            .id("palette-list")
            .flex()
            .flex_col()
            .max_h(px(420.))
            .overflow_y_scroll();

        for (i, (cmd, label, icon, hint)) in items.iter().enumerate() {
            let sel = i == self.palette_sel;
            let cmd = *cmd;
            list = list.child(
                div()
                    .id(("palette", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .h(px(34.))
                    .px_3()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .cursor_pointer()
                    // 1-9 shortcut number for the first nine commands
                    .child(
                        div()
                            .w(px(12.))
                            .text_size(px(11.))
                            .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                            .child(if i < 9 { format!("{}", i + 1) } else { String::new() }),
                    )
                    .child(
                        div()
                            .w(px(20.))
                            .font_family(ICON_FONT)
                            .text_size(px(15.))
                            .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                            .child(*icon),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                            .child(label.clone()),
                    )
                    .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(*hint))
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.palette_execute(cmd, window, cx);
                    })),
            );
        }

        div()
            .absolute()
            .top(px(80.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.palette_focus)
            .on_key_down(cx.listener(Self::palette_key))
            .child(
                div()
                    .h(px(44.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(div().font_family(ICON_FONT).text_size(px(16.)).text_color(rgb(MUTED)).child(IC_SEARCH))
                    .child(if self.palette_query.is_empty() {
                        div().text_color(rgb(MUTED)).child(format!("Type a command…{}", self.caret()))
                    } else {
                        div().text_color(rgb(TEXT)).child(self.palette_query.render(self.caret(), SELECTION))
                    }),
            )
            .child(list)
    }

    fn render_git_popup(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 440.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let items = self.gitp_items();

        let mut list = div()
            .id("gitp-list")
            .flex()
            .flex_col()
            .max_h(px(420.))
            .overflow_y_scroll();

        for (i, item) in items.iter().enumerate() {
            let sel = i == self.gitp_sel;
            let it = item.clone();
            let (icon, label, sub) = match item {
                GitItem::Action(_, label, icon) => (*icon, label.to_string(), String::new()),
                GitItem::Branch(name) => (IC_BRANCH, name.clone(), String::new()),
                GitItem::Pr(n) => (IC_PR, format!("Checkout PR #{n}"), String::new()),
            };
            let _ = sub;
            list = list.child(
                div()
                    .id(("gitp", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(26.))
                    .px_3()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(18.))
                            .font_family(ICON_FONT)
                            .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                            .child(icon),
                    )
                    .child(
                        div()
                            .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                            .child(label),
                    )
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.gitp_execute(it.clone(), window, cx);
                    })),
            );
        }

        div()
            .absolute()
            .top(px(64.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.gitp_focus)
            .on_key_down(cx.listener(Self::gitp_key))
            .child(
                div()
                    .h(px(40.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(div().font_family(ICON_FONT).text_color(rgb(MUTED)).child(IC_SEARCH))
                    .child(if self.gitp_query.is_empty() {
                        div().text_color(rgb(MUTED)).child(format!("Branches & actions{}", self.caret()))
                    } else {
                        div().text_color(rgb(TEXT)).child(self.gitp_query.render(self.caret(), SELECTION))
                    }),
            )
            .child(list)
    }

    /// The per-branch action submenu, floated beside the git popup next to the
    /// selected branch row (falling back to the left side if it would overflow).
    fn render_branch_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let branch = self.gitp_action_branch.clone().unwrap_or_default();
        let main_w = 440.0_f32;
        let main_left = ((self.win_width - main_w) / 2.0).max(0.);
        let subw = 320.0_f32;
        let mut sub_left = main_left + main_w + 6.0;
        if sub_left + subw > self.win_width {
            sub_left = (main_left - subw - 6.0).max(0.);
        }
        // align with the selected row: popup top (64) + header (40) + rows above
        let row_top = 64.0 + 40.0 + (self.gitp_sel as f32) * 26.0;
        let top = row_top.min((self.win_height - 80.0).max(64.0));

        let mut panel = div()
            .absolute()
            .top(px(top))
            .left(px(sub_left))
            .w(px(subw))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(30.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(div().font_family(ICON_FONT).text_color(rgb(MUTED)).child(IC_BRANCH))
                    .child(div().text_size(px(12.)).text_color(rgb(MUTED)).child(branch.clone())),
            );

        for (i, (action, label, icon)) in self.gitp_branch_actions().into_iter().enumerate() {
            let sel = i == self.gitp_action_sel;
            // dynamic, WebStorm-style label (e.g. "Merge 'next' into 'mine'")
            let display = match action {
                BranchAction::Merge => format!("Merge '{}' into '{}'", branch, self.branch),
                BranchAction::Delete => format!("Delete '{}'", branch),
                _ => label.to_string(),
            };
            panel = panel.child(
                div()
                    .id(("gitp-action", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(26.))
                    .px_3()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(18.))
                            .font_family(ICON_FONT)
                            .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                            .child(icon),
                    )
                    .child(div().min_w(px(0.)).truncate().text_color(rgb(if sel { SEL_TEXT } else { TEXT })).child(display))
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.gitp_run_branch_action(action, window, cx);
                    })),
            );
        }
        panel
    }

    /// "Push rejected — remote is ahead" prompt: merge or rebase, then push.
    fn render_push_ahead(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 440.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let btn = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .px_4()
                .py_1()
                .rounded_md()
                .text_size(px(12.))
                .cursor_pointer()
                .border_1()
                .border_color(rgb(if primary { ACCENT } else { BORDER }))
                .when(primary, |d| d.bg(rgb(ACCENT)).text_color(rgb(SEL_TEXT)))
                .when(!primary, |d| d.text_color(rgb(TEXT)).hover(|s| s.bg(rgb(HOVER))))
                .child(label)
        };
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .child(
                div().absolute().inset_0().bg(rgba(0x00000088)).id("pa-backdrop").on_click(
                    cx.listener(|this, _e, _w, cx| {
                        this.push_ahead = false;
                        cx.notify();
                    }),
                ),
            )
            .child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(160.))
                    .w(px(w))
                    .bg(rgb(PANEL_BG))
                    .border_1()
                    .border_color(rgb(ACCENT))
                    .rounded_lg()
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                    .child(
                        div()
                            .px_4()
                            .pt_4()
                            .pb_1()
                            .text_size(px(13.))
                            .text_color(rgb(TEXT))
                            .child("Push rejected — the remote has commits you don't have."),
                    )
                    .child(
                        div()
                            .px_4()
                            .pb_3()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child("Integrate the remote changes, then push again."),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .px_4()
                            .pb_4()
                            .child(btn("pa-cancel", "Cancel", false).on_click(cx.listener(
                                |this, _e, _w, cx| {
                                    this.push_ahead = false;
                                    cx.notify();
                                },
                            )))
                            .child(btn("pa-rebase", "Rebase & Push", false).on_click(cx.listener(
                                |this, _e, _w, cx| {
                                    this.push_ahead = false;
                                    this.run_command("git pull --rebase && git push -u origin HEAD".into(), cx);
                                },
                            )))
                            .child(btn("pa-merge", "Merge & Push", true).on_click(cx.listener(
                                |this, _e, _w, cx| {
                                    this.push_ahead = false;
                                    this.run_command("git pull --no-rebase && git push -u origin HEAD".into(), cx);
                                },
                            ))),
                    ),
            )
    }

    /// Process Manager dialog: filterable list of running processes with
    /// multi-select (cmd/shift-click) and kill.
    fn render_conflicts(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 620.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let mut list = div().flex().flex_col().px_2().pb_2().gap_1();
        for (i, path) in self.mc_files.clone().into_iter().enumerate() {
            let rel = self.rel(&path);
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| rel.clone());
            let dir = rel.strip_suffix(&name).unwrap_or("").trim_end_matches('/').to_string();
            let btn = |id: (&'static str, usize), label: &str, accent: bool| {
                div()
                    .id(id)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(if accent { ACCENT } else { BORDER }))
                    .text_color(rgb(if accent { SEL_TEXT } else { TEXT }))
                    .when(accent, |d| d.bg(rgb(ICON_SELECTED_BG)))
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(label.to_string())
            };
            let (po, pt, pe) = (path.clone(), path.clone(), path.clone());
            list = list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(BG))
                    .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(GIT_DELETED)).child(IC_BRANCH))
                    .child(
                        div()
                            .flex_grow(1.0)
                            .flex()
                            .flex_col()
                            .child(div().text_size(px(13.)).text_color(rgb(TEXT)).truncate().child(name))
                            .when(!dir.is_empty(), |d| {
                                d.child(div().text_size(px(10.)).text_color(rgb(MUTED)).truncate().child(dir))
                            }),
                    )
                    .child(btn(("mc-ours", i), "Accept Yours", false).on_click(
                        cx.listener(move |this, _e, _w, cx| this.mc_accept(po.clone(), false, cx)),
                    ))
                    .child(btn(("mc-theirs", i), "Accept Theirs", false).on_click(
                        cx.listener(move |this, _e, _w, cx| this.mc_accept(pt.clone(), true, cx)),
                    ))
                    .child(btn(("mc-edit", i), "Merge…", true).on_click(
                        cx.listener(move |this, _e, window, cx| this.mc_edit(pe.clone(), window, cx)),
                    )),
            );
        }
        div()
            .absolute()
            .top(px(100.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .py_1()
            .track_focus(&self.mc_focus)
            .on_key_down(cx.listener(Self::mc_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .child(format!("Conflicts — merging {} into {}", self.mc_from, self.mc_into)),
            )
            .child(
                div()
                    .px_3()
                    .pb_1()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child("Accept Yours/Theirs to take one side, or Merge… to edit the file. Esc to close."),
            )
            .child(list)
    }

    fn render_process_manager(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 760.0_f32;
        let h = 560.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let top = ((self.win_height - h) / 2.0).max(40.);
        let rows = self.proc_rows();
        let total = rows.len();
        let sel_count = self.proc_selected.len();

        // search row
        let header = div()
            .h(px(40.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().font_family(ICON_FONT).text_color(rgb(MUTED)).child(IC_SEARCH))
            .child(
                div().flex_grow(1.0).text_size(px(13.)).text_color(rgb(TEXT)).child(
                    if self.proc_filter.is_empty() {
                        div().text_color(rgb(MUTED)).child(format!("Filter processes…{}", self.caret()))
                    } else {
                        div().child(self.proc_filter.render(self.caret(), SELECTION))
                    },
                ),
            )
            // scope toggles: this workspace ⊂ all TIDE ⊂ all system
            .child({
                let on = self.proc_workspace_only;
                div()
                    .id("proc-ws")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(rgb(ACCENT)).text_color(rgb(SEL_TEXT)))
                    .when(!on, |d| d.text_color(rgb(MUTED)).hover(|s| s.bg(rgb(HOVER))))
                    .child("This workspace")
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.proc_workspace_only = !this.proc_workspace_only;
                        this.proc_anchor = None;
                        cx.notify();
                    }))
            })
            // "TIDE only" (ignored while "This workspace" is on, which is narrower)
            .child({
                let on = self.proc_only_tide;
                let dimmed = self.proc_workspace_only;
                div()
                    .id("proc-tide")
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on && !dimmed, |d| d.bg(rgb(ACCENT)).text_color(rgb(SEL_TEXT)))
                    .when(!(on && !dimmed), |d| d.text_color(rgb(MUTED)).hover(|s| s.bg(rgb(HOVER))))
                    .child("TIDE only")
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.proc_only_tide = !this.proc_only_tide;
                        this.proc_anchor = None;
                        cx.notify();
                    }))
            })
            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(if sel_count > 0 {
                format!("{} selected · {} processes", sel_count, total)
            } else {
                format!("{} processes", total)
            }));

        // column header
        let col_head = div()
            .h(px(24.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .border_b_1()
            .border_color(rgb(BORDER))
            .text_size(px(11.))
            .text_color(rgb(MUTED))
            .child(div().flex_grow(1.0).child("Process"))
            .child(div().w(px(90.)).flex().justify_end().child("Memory"))
            .child(div().w(px(64.)).flex().justify_end().child("PID"))
            .child(div().w(px(90.)).child("User"));

        let mut list = div().id("proc-list").flex().flex_col().flex_grow(1.0).min_h(px(0.)).overflow_y_scroll();
        for (i, p) in rows.iter().enumerate().take(800) {
            let selected = self.proc_selected.contains(&p.pid);
            let text = if selected { SEL_TEXT } else { TEXT };
            let mem = fmt_mem(p.rss_kb);
            list = list.child(
                div()
                    .id(("proc", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(24.))
                    .px_3()
                    .when(selected, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!selected, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .cursor_pointer()
                    .text_size(px(12.))
                    .child(div().flex_grow(1.0).truncate().text_color(rgb(text)).child(p.name.clone()))
                    .child(div().w(px(90.)).flex().justify_end().text_color(rgb(if selected { SEL_TEXT } else { MUTED })).child(mem))
                    .child(div().w(px(64.)).flex().justify_end().text_color(rgb(if selected { SEL_TEXT } else { MUTED })).child(p.pid.to_string()))
                    .child(div().w(px(90.)).truncate().text_color(rgb(if selected { SEL_TEXT } else { MUTED })).child(p.user.clone()))
                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _w, cx| {
                        let m = ev.modifiers();
                        this.proc_select(i, m.shift, m.platform, cx);
                    })),
            );
        }

        // footer: Kill button + shortcuts hint
        let footer = div()
            .h(px(46.))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .flex_shrink_0()
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(div().flex_grow(1.0).text_size(px(11.)).text_color(rgb(MUTED)).child("⌘/⇧-click multi-select  ·  ⌘R refresh  ·  ⌘⌫ kill"))
            // Kill All: kills every process currently shown (after the filters)
            .child({
                let mut b = div()
                    .id("proc-kill-all")
                    .px_3()
                    .py_1()
                    .rounded_md()
                    .text_size(px(12.))
                    .border_1()
                    .border_color(rgb(if total > 0 { GIT_DELETED } else { BORDER }))
                    .text_color(rgb(if total > 0 { GIT_DELETED } else { MUTED }))
                    .child(format!("Kill All ({total})"));
                if total > 0 {
                    b = b.cursor_pointer().hover(|s| s.bg(rgb(HOVER))).on_click(
                        cx.listener(|this, _e, _w, cx| this.proc_kill_all(cx)),
                    );
                }
                b
            })
            .child({
                let kill_label = if sel_count > 0 { format!("Kill ({})", sel_count) } else { "Kill".to_string() };
                let mut b = div()
                    .id("proc-kill")
                    .px_4()
                    .py_1()
                    .rounded_md()
                    .text_size(px(12.))
                    .border_1()
                    .border_color(rgb(if sel_count > 0 { GIT_DELETED } else { BORDER }))
                    .text_color(rgb(if sel_count > 0 { SEL_TEXT } else { MUTED }))
                    .child(kill_label);
                if sel_count > 0 {
                    b = b.bg(rgb(GIT_DELETED)).cursor_pointer().on_click(
                        cx.listener(|this, _e, _w, cx| this.proc_kill_selected(cx)),
                    );
                }
                b
            });

        // backdrop + centered panel
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .child(
                div().absolute().inset_0().bg(rgba(0x00000088)).id("proc-backdrop").on_click(
                    cx.listener(|this, _e, _w, cx| {
                        this.proc_open = false;
                        cx.notify();
                    }),
                ),
            )
            .child(
                div()
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(px(w))
                    .h(px(h))
                    .bg(rgb(PANEL_BG))
                    .border_1()
                    .border_color(rgb(ACCENT))
                    .rounded_lg()
                    .shadow_lg()
                    .flex()
                    .flex_col()
                    .track_focus(&self.proc_focus)
                    .on_key_down(cx.listener(Self::proc_key))
                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                    .child(header)
                    .child(col_head)
                    .child(list)
                    .child(footer),
            )
    }

    /// Confirmation dialog before deleting a file/folder from the project tree.
    /// The "Push Commits" dialog (cmd+shift+k): commits + changed files that
    /// would be pushed to the branch's upstream, with diff + push actions.
    fn render_push_dialog(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 960.0_f32;
        let h = 600.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let top = ((self.win_height - h) / 2.0).max(40.);
        let target = self.push_target.strip_prefix("origin/").unwrap_or(&self.push_target).to_string();

        // build the changed-file tree
        let mut root_node = ChangeDir::default();
        for (p, s) in &self.push_files {
            let rel = self.rel(p);
            let comps: Vec<&str> = rel.split('/').filter(|c| !c.is_empty()).collect();
            root_node.insert(&comps, p.clone(), *s);
        }
        let mut rows = Vec::new();
        flatten_changes(&root_node, Path::new(""), 0, &self.push_collapsed, &mut rows);
        let file_count = self.push_files.len();

        // ── left: branch + commits ───────────────────────────────────────
        let all_sel = self.push_commit_sel.is_none();
        let mut commits = div().id("push-commits").flex().flex_col().w(px(280.)).flex_shrink_0().min_h(px(0.)).overflow_y_scroll()
            .border_r_1().border_color(rgb(BORDER))
            .child(
                // branch header doubles as "show all commits" (clears the filter)
                div().id("push-branch").flex().flex_row().items_center().gap_2().h(px(26.)).px_3().cursor_pointer()
                    .when(all_sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!all_sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .on_click(cx.listener(|this, _e, _w, cx| this.push_select_commit(None, cx)))
                    .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(ACCENT)).child(IC_BRANCH))
                    .child(div().text_size(px(12.)).text_color(rgb(if all_sel { SEL_TEXT } else { TEXT })).truncate().child(self.push_branch.clone())),
            );
        for (i, (hash, subject)) in self.push_commits.iter().enumerate() {
            let sel = self.push_commit_sel.as_deref() == Some(hash.as_str());
            let sha = hash.clone();
            commits = commits.child(
                div().id(("push-commit", i)).flex().flex_row().items_center().gap_2().h(px(22.)).pl(px(28.)).pr_3().cursor_pointer()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .on_click(cx.listener(move |this, _e, _w, cx| this.push_select_commit(Some(sha.clone()), cx)))
                    .child(div().flex_grow(1.0).text_size(px(12.)).text_color(rgb(if sel { SEL_TEXT } else { TEXT })).truncate().child(subject.clone()))
                    .child(div().text_size(px(11.)).text_color(rgb(if sel { SEL_TEXT } else { MUTED })).child(hash.clone())),
            );
        }
        if self.push_commits.is_empty() {
            commits = commits.child(div().px_3().py_2().text_size(px(12.)).text_color(rgb(MUTED)).child("No commits ahead"));
        }

        // ── right: changed-files tree ────────────────────────────────────
        let mut tree = div().id("push-tree").flex().flex_col().flex_grow(1.0).min_h(px(0.)).overflow_y_scroll()
            .child(
                div().flex().flex_row().items_center().gap_2().h(px(26.)).px_3()
                    .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(FOLDER_ICON)).child(IC_FOLDER))
                    .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child(target.clone()))
                    .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(format!("{} files", file_count))),
            );
        for (i, row) in rows.into_iter().enumerate() {
            match row {
                CommitRow::Dir { depth, key, label, .. } => {
                    let collapsed = self.push_collapsed.contains(&key);
                    let key_toggle = key.clone();
                    tree = tree.child(
                        div().id(("push-dir", i)).flex().flex_row().items_center().gap_1().h(px(22.))
                            .pl(px(20. + depth as f32 * 14.)).pr_2().cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                if !this.push_collapsed.remove(&key_toggle) {
                                    this.push_collapsed.insert(key_toggle.clone());
                                }
                                cx.notify();
                            }))
                            .child(div().w(px(14.)).flex().justify_center().text_size(px(12.)).text_color(rgb(DIR)).child(if collapsed { "▸" } else { "▾" }))
                            .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(FOLDER_ICON)).child(IC_FOLDER))
                            .child(div().text_size(px(12.)).text_color(rgb(DIR)).child(label)),
                    );
                }
                CommitRow::File { depth, path, name, state } => {
                    let is_sel = self.push_selected.as_ref() == Some(&path);
                    let color = match state {
                        GitState::New => GIT_NEW,
                        GitState::Modified => GIT_MODIFIED,
                        GitState::Deleted => GIT_DELETED,
                    };
                    let (badge, badge_color) = ext_badge(&path);
                    let path_click = path.clone();
                    tree = tree.child(
                        div().id(("push-file", i)).flex().flex_row().items_center().gap_1().h(px(22.))
                            .pl(px(20. + depth as f32 * 14.)).pr_2().cursor_pointer()
                            .when(is_sel, |d| d.bg(rgb(SELECTED_BG)))
                            .when(!is_sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                            .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _w, cx| {
                                this.push_selected = Some(path_click.clone());
                                if ev.click_count() >= 2 {
                                    this.push_open_diff(path_click.clone(), cx);
                                }
                                cx.notify();
                            }))
                            .child(div().w(px(14.)))
                            .child(div().w(px(16.)).flex().justify_center().text_size(px(9.)).text_color(rgb(if is_sel { SEL_TEXT } else { badge_color })).child(badge))
                            .child(div().text_size(px(12.)).text_color(rgb(if is_sel { SEL_TEXT } else { color })).child(name)),
                    );
                }
            }
        }

        // ── footer ───────────────────────────────────────────────────────
        let push_tags = self.push_tags;
        let footer = div().flex().flex_row().items_center().gap_3().h(px(48.)).px_4()
            .border_t_1().border_color(rgb(BORDER))
            .child(
                div().id("push-tags").flex().flex_row().items_center().gap_2().cursor_pointer()
                    .on_click(cx.listener(|this, _e, _w, cx| { this.push_tags = !this.push_tags; cx.notify(); }))
                    .child(
                        div().size(px(14.)).flex().items_center().justify_center().rounded_sm().border_1()
                            .border_color(rgb(if push_tags { ACCENT } else { MUTED }))
                            .when(push_tags, |d| d.bg(rgb(ACCENT)))
                            .text_size(px(10.)).text_color(rgb(SEL_TEXT))
                            .child(if push_tags { "✓" } else { "" }),
                    )
                    .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child("Push tags")),
            )
            .child(div().flex_1())
            .child(
                div().id("push-cancel").px_4().py_1().rounded_md().border_1().border_color(rgb(BORDER))
                    .text_color(rgb(TEXT)).cursor_pointer().hover(|s| s.bg(rgb(HOVER)))
                    .child("Cancel")
                    .on_click(cx.listener(|this, _e, _w, cx| { this.push_open = false; cx.notify(); })),
            )
            .child(
                div().id("push-go").px_5().py_1().rounded_md().bg(rgb(ACCENT)).text_color(rgb(SEL_TEXT)).cursor_pointer()
                    .child("Push")
                    .on_click(cx.listener(|this, _e, _w, cx| { this.do_push(cx); })),
            );

        // backdrop + centered panel
        div().absolute().inset_0().flex().items_start().justify_center()
            .child(
                div().absolute().inset_0().bg(rgba(0x00000088))
                    .id("push-backdrop")
                    .on_click(cx.listener(|this, _e, _w, cx| { this.push_open = false; cx.notify(); })),
            )
            .child(
                div().absolute().left(px(left)).top(px(top)).w(px(w)).h(px(h))
                    .bg(rgb(PANEL_BG)).border_1().border_color(rgb(BORDER)).rounded_lg().shadow_lg()
                    .flex().flex_col()
                    .track_focus(&self.push_focus)
                    .on_key_down(cx.listener(Self::push_key))
                    // clicking inside the panel must not reach the dismiss backdrop
                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                    .child(
                        div().flex().items_center().justify_center().h(px(38.)).flex_shrink_0()
                            .border_b_1().border_color(rgb(BORDER))
                            .text_size(px(13.)).text_color(rgb(TEXT))
                            .child(format!("Push Commits to {target}")),
                    )
                    .child(div().flex().flex_row().flex_grow(1.0).min_h(px(0.)).child(commits).child(tree))
                    .child(footer),
            )
    }

    /// Editor right-click context menu, anchored at the click position. A
    /// full-window transparent backdrop dismisses it on any outside click.
    /// The editor context-menu actions, in display order. Shared by the menu
    /// renderer and the keyboard handler so the numbers always line up.
    fn editor_ctx_actions() -> [(&'static str, fn(&mut Self, &mut Window, &mut Context<Self>)); 8] {
        [
            ("Reveal in Dir Tree", |this, window, cx| this.reveal_active_in_tree(window, cx)),
            ("Reveal in Changes", |this, window, cx| this.reveal_active_in_changes(window, cx)),
            ("Reveal in PR Review", |this, window, cx| this.reveal_active_in_pr(window, cx)),
            ("Copy Full Path", |this, _w, cx| {
                if let Some(p) = this.active_path().cloned() {
                    cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().to_string()));
                    this.show_flash("Full path copied", cx);
                }
            }),
            ("Copy Path", |this, _w, cx| {
                if let Some(p) = this.active_path().cloned() {
                    this.copy_reference(&p, cx); // repo-relative path
                }
            }),
            ("Revert File", |this, window, cx| {
                if let Some(p) = this.active_path().cloned().filter(|p| p.is_file()) {
                    this.confirm_revert = Some(p);
                    window.focus(&this.confirm_focus, cx);
                }
            }),
            ("Comment on This Line", |this, window, cx| this.open_comment_prompt(window, cx)),
            ("Close Tab", |this, window, cx| this.close_tab(this.active, window, cx)),
        ]
    }

    /// Run editor-context-menu action `idx` (0-based) and close the menu.
    fn editor_ctx_run(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let actions = Self::editor_ctx_actions();
        if let Some((_, run)) = actions.get(idx) {
            self.editor_ctx = None;
            // default focus back to the editor; actions that move focus
            // (e.g. Reveal → tree) override this afterwards
            self.focus_active(window, cx);
            run(self, window, cx);
            cx.notify();
        }
    }

    /// Keys while the context menu is open: Esc closes it, 1-9 invoke an action.
    fn editor_ctx_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        if key == "escape" {
            self.editor_ctx = None;
            self.focus_active(window, cx);
            cx.notify();
            return;
        }
        if let Ok(n) = key.parse::<usize>() {
            if n >= 1 {
                self.editor_ctx_run(n - 1, window, cx);
            }
        }
    }

    fn render_editor_ctx_menu(&self, pos: (f32, f32), cx: &mut Context<Self>) -> impl IntoElement {
        let actions = Self::editor_ctx_actions();
        let menu_w = 220.0_f32;
        let row_h = 28.0_f32;
        let menu_h = actions.len() as f32 * row_h + 10.0; // padding
        let left = pos.0.min((self.win_width - menu_w - 8.0).max(0.));
        let top = pos.1.min((self.win_height - menu_h - 8.0).max(0.));

        let mut menu = div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(menu_w))
            .py_1()
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            // pressing inside the menu must not reach the dismiss backdrop
            .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation());
        for (i, (label, _)) in actions.into_iter().enumerate() {
            menu = menu.child(
                div()
                    .id(("ctx", i))
                    .h(px(row_h))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    // the number you can press to invoke the action
                    .child(div().w(px(12.)).text_color(rgb(MUTED)).child(format!("{}", i + 1)))
                    .child(label)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.editor_ctx_run(i, window, cx);
                    })),
            );
        }

        div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .w(px(self.win_width))
            .h(px(self.win_height))
            .track_focus(&self.editor_ctx_focus)
            .on_key_down(cx.listener(Self::editor_ctx_key))
            // backdrop: any click outside the menu closes it
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.editor_ctx = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _e, _w, cx| {
                    this.editor_ctx = None;
                    cx.notify();
                }),
            )
            .child(menu)
    }

    /// Right-click actions for the open menu. Operate on `tree_ctx_path`.
    fn tree_ctx_actions(kind: CtxKind) -> Vec<(&'static str, fn(&mut Self, &mut Window, &mut Context<Self>))> {
        if kind == CtxKind::AgentSel {
            return vec![
                ("Comment on Selection", |this, window, cx| {
                    this.quote_agent_selection(window, cx)
                }),
                ("Copy", |this, _w, cx| {
                    if let Some(t) = this.agent_selected_text(cx) {
                        cx.write_to_clipboard(ClipboardItem::new_string(t));
                        this.show_flash("Copied", cx);
                    }
                }),
                ("Clear Selection", |this, _w, cx| {
                    this.agent_sel = None;
                    cx.notify();
                }),
            ];
        }
        if kind == CtxKind::ReviewFile {
            return vec![("Comment on Whole File", |this, window, cx| {
                if let Some(p) = this.tree_ctx_path.clone() {
                    this.begin_comment(p, 0, 0, window, cx);
                }
            })];
        }
        vec![
            ("New File", |this, window, cx| this.open_new_file_prompt(window, cx)),
            ("Reveal in Finder", |this, _w, _cx| {
                if let Some(p) = this.tree_ctx_path.clone() {
                    let _ = Command::new("open").arg("-R").arg(&p).spawn();
                }
            }),
            ("Copy Full Path", |this, _w, cx| {
                if let Some(p) = this.tree_ctx_path.clone() {
                    cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().to_string()));
                    this.show_flash("Full path copied", cx);
                }
            }),
            ("Copy Path", |this, _w, cx| {
                if let Some(p) = this.tree_ctx_path.clone() {
                    this.copy_reference(&p, cx); // repo-relative path
                }
            }),
            ("Refresh", |this, _w, _cx| {
                // re-read the filesystem so created/deleted files show up; expand
                // the targeted folder so its current contents are visible
                if let Some(p) = this.tree_ctx_path.clone() {
                    if p.is_dir() {
                        this.expanded.insert(p);
                    }
                }
                this.rebuild();
            }),
            ("Revert", |this, window, cx| {
                if let Some(p) = this.tree_ctx_path.clone().filter(|p| p.is_file()) {
                    this.confirm_revert = Some(p);
                    window.focus(&this.confirm_focus, cx);
                }
            }),
            ("Delete", |this, window, cx| {
                if let Some(p) = this.tree_ctx_path.clone() {
                    this.confirm_delete = Some(p);
                    window.focus(&this.confirm_focus, cx);
                }
            }),
        ]
    }

    /// Open the file-level context menu for `path` at the click position.
    fn open_review_ctx(&mut self, path: PathBuf, pos: (f32, f32), window: &mut Window, cx: &mut Context<Self>) {
        self.tree_ctx = Some(pos);
        self.tree_ctx_path = Some(path);
        self.tree_ctx_kind = CtxKind::ReviewFile;
        window.focus(&self.tree_ctx_focus, cx);
        cx.notify();
    }

    fn tree_ctx_run(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let actions = Self::tree_ctx_actions(self.tree_ctx_kind);
        if let Some((_, run)) = actions.get(idx) {
            self.tree_ctx = None;
            self.tree_ctx_kind = CtxKind::Tree;
            run(self, window, cx);
            self.tree_ctx_path = None;
            cx.notify();
        }
    }

    fn tree_ctx_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        if key == "escape" {
            self.tree_ctx = None;
            self.tree_ctx_path = None;
            self.tree_ctx_kind = CtxKind::Tree;
            cx.notify();
            return;
        }
        if let Ok(n) = key.parse::<usize>() {
            if n >= 1 {
                self.tree_ctx_run(n - 1, _window, cx);
            }
        }
    }

    fn render_tree_ctx_menu(&self, pos: (f32, f32), cx: &mut Context<Self>) -> impl IntoElement {
        let actions = Self::tree_ctx_actions(self.tree_ctx_kind);
        let menu_w = match self.tree_ctx_kind {
            CtxKind::ReviewFile => 220.0_f32,
            CtxKind::AgentSel => 210.0_f32,
            CtxKind::Tree => 200.0_f32,
        };
        let row_h = 28.0_f32;
        let menu_h = actions.len() as f32 * row_h + 10.0;
        let left = pos.0.min((self.win_width - menu_w - 8.0).max(0.));
        let top = pos.1.min((self.win_height - menu_h - 8.0).max(0.));

        let mut menu = div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(menu_w))
            .py_1()
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation());
        for (i, (label, _)) in actions.into_iter().enumerate() {
            menu = menu.child(
                div()
                    .id(("tree-ctx", i))
                    .h(px(row_h))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(div().w(px(12.)).text_color(rgb(MUTED)).child(format!("{}", i + 1)))
                    .child(label)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.tree_ctx_run(i, window, cx);
                    })),
            );
        }

        div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .w(px(self.win_width))
            .h(px(self.win_height))
            .track_focus(&self.tree_ctx_focus)
            .on_key_down(cx.listener(Self::tree_ctx_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.tree_ctx = None;
                    this.tree_ctx_path = None;
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _e, _w, cx| {
                    this.tree_ctx = None;
                    this.tree_ctx_path = None;
                    cx.notify();
                }),
            )
            .child(menu)
    }

    fn render_confirm_delete(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.confirm_delete.clone().unwrap_or_default();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let is_dir = path.is_dir();
        let kind = if is_dir { "folder" } else { "file" };
        let body = if is_dir {
            format!("“{}” and all its contents will be permanently deleted.", name)
        } else {
            format!("“{}” will be permanently deleted.", name)
        };
        let w = 380.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        div()
            .absolute()
            .top(px(160.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.confirm_focus)
            .on_key_down(cx.listener(Self::confirm_key))
            .child(
                div()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .text_size(px(13.))
                    .text_color(rgb(TEXT))
                    .child(format!("Delete {}?", kind)),
            )
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(body),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .px_4()
                    .pb_3()
                    .child(
                        div()
                            .id("confirm-cancel")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .text_color(rgb(TEXT))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.confirm_delete = None;
                                window.focus(&this.tree_focus, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("confirm-delete")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(GIT_DELETED))
                            .text_color(rgb(SEL_TEXT))
                            .cursor_pointer()
                            .child("Delete")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.do_delete(window, cx);
                            })),
                    ),
            )
    }

    fn render_confirm_revert(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.confirm_revert.clone().unwrap_or_default();
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let w = 400.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        div()
            .absolute()
            .top(px(160.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.confirm_focus)
            .on_key_down(cx.listener(Self::revert_key))
            .child(div().px_4().pt_3().pb_1().text_size(px(13.)).text_color(rgb(TEXT)).child("Revert file?"))
            .child(
                div()
                    .px_4()
                    .pb_3()
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(format!("“{}” will be restored to HEAD. Uncommitted changes will be lost.", name)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .px_4()
                    .pb_3()
                    .child(
                        div()
                            .id("revert-cancel")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .text_color(rgb(TEXT))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                this.confirm_revert = None;
                                window.focus(&this.tree_focus, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("revert-go")
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .bg(rgb(GIT_DELETED))
                            .text_color(rgb(SEL_TEXT))
                            .cursor_pointer()
                            .child("Revert")
                            .on_click(cx.listener(|this, _ev, window, cx| this.do_revert(window, cx))),
                    ),
            )
    }

    fn render_branch_prompt(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 360.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        div()
            .absolute()
            .top(px(120.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.br_focus)
            .on_key_down(cx.listener(Self::br_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("New branch name"),
            )
            .child(
                div()
                    .mx_3()
                    .mb_2()
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .text_color(rgb(TEXT))
                    .child(self.br_query.render(self.caret(), SELECTION)),
            )
            // "also create a PR" checkbox (Tab toggles, or click)
            .child({
                let on = self.br_make_pr;
                div()
                    .id("br-make-pr")
                    .mx_3()
                    .mb_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.br_make_pr = !this.br_make_pr;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(14.))
                            .h(px(14.))
                            .rounded_sm()
                            .border_1()
                            .border_color(rgb(if on { ACCENT } else { MUTED }))
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(on, |d| d.bg(rgb(ACCENT)))
                            .when(on, |d| {
                                d.child(div().font_family(ICON_FONT).text_size(px(10.)).text_color(rgb(SEL_TEXT)).child(IC_CHECK))
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(if on { TEXT } else { MUTED }))
                            .child("Create a pull request too"),
                    )
            })
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child("Enter to create · Tab toggles PR · Esc to cancel"),
            )
    }

    fn render_pr_create_prompt(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 360.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        div()
            .absolute()
            .top(px(120.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.prc_focus)
            .on_key_down(cx.listener(Self::prc_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("Create pull request — milestone (optional)"),
            )
            .child(
                div()
                    .mx_3()
                    .mb_1()
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .text_size(px(13.))
                    // show a muted "6.4.3" placeholder until something is typed
                    .child(if self.prc_milestone.is_empty() {
                        div()
                            .text_color(rgb(MUTED))
                            .child(format!("6.4.3{}", self.caret()))
                    } else {
                        div()
                            .text_color(rgb(TEXT))
                            .child(self.prc_milestone.render(self.caret(), SELECTION))
                    }),
            )
            .children({
                // autocomplete list of matching open milestones
                let sugg = self.prc_suggestions();
                (!sugg.is_empty()).then(|| {
                    let mut list = div().id("prc-ms").mx_3().mb_1().flex().flex_col().max_h(px(160.)).overflow_y_scroll();
                    for (i, m) in sugg.iter().enumerate().take(50) {
                        let selected = i == self.prc_sel;
                        let title = m.clone();
                        list = list.child(
                            div()
                                .id(("prc-ms-row", i))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .text_size(px(12.))
                                .cursor_pointer()
                                .when(selected, |d| d.bg(rgb(SELECTED_BG)).text_color(rgb(SEL_TEXT)))
                                .when(!selected, |d| d.text_color(rgb(TEXT)).hover(|s| s.bg(rgb(HOVER))))
                                .child(title.clone())
                                .on_click(cx.listener(move |this, _e, _w, cx| {
                                    this.prc_open = false;
                                    save_last_milestone(&title); // remember for next time
                                    this.run_command(format!("pr --ms {}", title), cx);
                                })),
                        );
                    }
                    list
                })
            })
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child(if self.prc_milestones.is_empty() {
                        "Enter to create · Esc to cancel"
                    } else {
                        "↑↓ to pick · Tab to fill · Enter to create · Esc to cancel"
                    }),
            )
    }

    fn render_run_prompt(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 520.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        div()
            .absolute()
            .top(px(120.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.runc_focus)
            .on_key_down(cx.listener(Self::runc_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("Run command"),
            )
            .child(
                div()
                    .mx_3()
                    .mb_3()
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .child(if self.runc_query.is_empty() {
                        div().text_color(rgb(MUTED)).child(format!("{}e.g. wippp --ms 6.5.4", self.caret()))
                    } else {
                        div().child(self.runc_query.render(self.caret(), SELECTION))
                    }),
            )
    }

    fn render_new_project(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 560.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        // the typed path is valid when it points at an existing folder
        let raw = self.newproj_path.text.trim();
        let valid = !raw.is_empty()
            && PathBuf::from(if let Some(r) = raw.strip_prefix("~/") {
                std::env::var("HOME").map(|h| format!("{h}/{r}")).unwrap_or_else(|_| raw.to_string())
            } else {
                raw.to_string()
            })
            .is_dir();

        // text input + "Choose…" button on one row
        let input_row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .mx_3()
            .child(
                div()
                    .flex_grow(1.0)
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .child(if self.newproj_path.is_empty() {
                        div()
                            .text_color(rgb(MUTED))
                            .child(format!("{}/path/to/folder  or  a GitHub URL", self.caret()))
                    } else {
                        div().child(self.newproj_path.render(self.caret(), SELECTION))
                    }),
            )
            .child(
                div()
                    .id("newproj-choose")
                    .px_3()
                    .py_1()
                    .flex_shrink_0()
                    .bg(rgb(PANEL_BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child("Choose…")
                    .on_click(cx.listener(|this, _e, _w, cx| this.newproj_choose(cx))),
            );

        // Open button (enabled only when the path is an existing folder)
        let open_btn = div()
            .id("newproj-open")
            .px_3()
            .py_1()
            .rounded_md()
            .text_size(px(12.))
            .border_1()
            .border_color(rgb(if valid { ACCENT } else { BORDER }))
            .text_color(rgb(if valid { SEL_TEXT } else { MUTED }))
            .when(valid, |d| {
                d.bg(rgb(ACCENT)).cursor_pointer().hover(|s| s.bg(rgb(ACCENT))).on_click(
                    cx.listener(|this, _e, _w, cx| this.newproj_submit(cx)),
                )
            })
            .child("Open");

        // recent projects (most recent first, filtered by the typed text) — click
        // or arrow+enter to open; capped at 20
        let filtered = self.newproj_filtered();
        let mut recents = div().id("newproj-recents").flex().flex_col().max_h(px(480.)).overflow_y_scroll().pb_2();
        if !self.newproj_recents.is_empty() {
            recents = recents.child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(if self.newproj_path.text.trim().is_empty() {
                        "Recent".to_string()
                    } else {
                        format!("Recent · {} match(es)", filtered.len())
                    }),
            );
            for (i, p) in filtered.iter().enumerate() {
                let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let dir = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
                let path = p.clone();
                let sel = i == self.newproj_sel;
                // compare canonicalised, so ~/dev/x and a symlinked path match
                let canon = |q: &Path| std::fs::canonicalize(q).unwrap_or_else(|_| q.to_path_buf());
                let already_open = self.ws_roots.iter().any(|r| canon(r) == canon(p));
                let key = p.to_string_lossy().to_string();
                let live = self
                    .newproj_titles
                    .get(&key)
                    .filter(|t| !t.trim().is_empty())
                    .cloned();
                // a reset workspace keeps no title; show the last one it had in
                // grey so the folder is still recognisable
                let title = match live {
                    Some(t) => Some((t, true)),
                    None => self
                        .newproj_last_titles
                        .get(&key)
                        .filter(|t| !t.trim().is_empty())
                        .cloned()
                        .map(|t| (t, false)),
                };
                recents = recents.child(
                    div()
                        .id(("newproj-recent", i))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .h(px(26.))
                        .px_3()
                        .cursor_pointer()
                        .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                        .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                        .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(FOLDER_ICON)).child(IC_FOLDER))
                        .child(div().flex_shrink_0().text_size(px(12.)).text_color(rgb(if sel { SEL_TEXT } else { TEXT })).child(name))
                        // the workspace title tells you which piece of work lives
                        // in this folder — the whole point of picking one
                        .when_some(title, |d, (t, live)| {
                            let color = match (sel, live) {
                                (true, _) => SEL_TEXT,
                                (false, true) => ACCENT,
                                (false, false) => MUTED,
                            };
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .max_w(px(240.))
                                    .text_size(px(11.))
                                    .truncate()
                                    .text_color(rgb(color))
                                    .child(t),
                            )
                        })
                        .child(div().flex_grow(1.0).text_size(px(11.)).text_color(rgb(MUTED)).truncate().child(dir))
                        // already one of the open workspaces → picking it just
                        // switches to it rather than opening a second copy
                        .when(already_open, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .px_1p5()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                                    .text_size(px(9.))
                                    .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                                    .child("already open"),
                            )
                        })
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.newproj_open = false;
                            cx.emit(ProjectNav::OpenPath(path.clone()));
                            cx.notify();
                        })),
                );
            }
        }

        div()
            .absolute()
            .top(px(120.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.newproj_focus)
            .on_key_down(cx.listener(Self::newproj_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("Open Project — folder path, or a repo to clone into ~/dev"),
            )
            .child(input_row)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .child(open_btn),
            )
            .child(recents)
    }
}

/// A tiny hover-tooltip view.
struct TooltipView {
    text: SharedString,
}
impl Render for TooltipView {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .text_size(px(11.))
            .text_color(rgb(POPUP_FG))
            .child(self.text.clone())
    }
}

fn tip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_w, cx| cx.new(|_| TooltipView { text: text.into() }).into()
}

// ── native agent panel (ACP) ────────────────────────────────────────────────
impl Storm {
    fn act_toggle_agent(&mut self, _: &ToggleAgent, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_agent(window, cx);
    }

    /// ⌘⇧A. The pane is always up, so this is "put me in it" — the toggle it
    /// used to be only ever got in the way of the surface you work in.
    fn toggle_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !AGENT_PANEL {
            return; // feature parked; flip AGENT_PANEL to re-enable
        }
        self.focus_agent(window, cx);
    }

    /// On the panel's first open, pick up where the last session left off: ask
    /// for the project's sessions and load the newest one when the answer lands.
    /// The adapter's handshake already opened a fresh session, which stands if
    /// there's nothing to resume.
    fn autoresume_agent(&mut self, cx: &mut Context<Self>) {
        if self.agent_resumed || self.agent_autoresume {
            return;
        }
        // started over and never prompted since: the handshake's own new session
        // is the one that belongs here, not whatever came before the reset
        if self.agent_fresh {
            self.agent_resumed = true;
            return;
        }
        if let Some(acp) = self.acp.clone() {
            self.agent_autoresume = true;
            acp.list_sessions(&self.root);
            cx.notify();
        }
    }

    /// Ask the adapter what's resumable and show the list.
    fn open_agent_picker(&mut self, cx: &mut Context<Self>) {
        self.ensure_acp(cx);
        if let Some(acp) = self.acp.clone() {
            acp.list_sessions(&self.root);
        }
        self.agent_picker = true;
        cx.notify();
    }

    /// Drop the current conversation and start a fresh session.
    fn new_agent_session(&mut self, cx: &mut Context<Self>) {
        self.ensure_acp(cx);
        self.agent_resumed = true; // an explicit new session outranks auto-resume
        self.agent_autoresume = false;
        self.mark_agent_fresh(true);
        self.reset_agent_transcript();
        if let Some(acp) = self.acp.clone() {
            acp.new_session(&self.root);
        }
        cx.notify();
    }

    /// Remember (on disk, per workspace) whether the agent here was started over
    /// and not yet prompted.
    fn mark_agent_fresh(&mut self, fresh: bool) {
        if self.agent_fresh == fresh {
            return;
        }
        self.agent_fresh = fresh;
        save_ws_fresh(&self.root, fresh);
    }

    fn reset_agent_transcript(&mut self) {
        self.agent_briefed = false; // a new session hasn't been told anything yet
        self.acp_state = None;
        self.agent_ghost = None;
        self.agent_jump_open = false;
        self.agent_queue.clear();
        self.agent_imgs.clear();
        self.agent_img_seq = 0;
        self.agent_pastes.clear();
        self.agent_list.scroll_to_end(); // a replayed session opens at its end
        self.agent_picker = false;
        self.agent_msgs.clear();
        self.agent_cur = None;
        self.agent_cur_thought = None;
        self.agent_ctx = None;
        self.agent_title = None;
        self.agent_busy = false;
    }

    /// Resume a session: the transcript is rebuilt from the adapter's replay.
    fn resume_agent_session(&mut self, id: String, cx: &mut Context<Self>) {
        self.mark_agent_fresh(false); // resuming by hand is a choice to keep it
        self.reset_agent_transcript();
        if let Some(acp) = self.acp.clone() {
            acp.load_session(&self.root, &id);
        }
        cx.notify();
    }

    /// Spawn the ACP adapter on first use and start the event drain poll.
    fn ensure_acp(&mut self, cx: &mut Context<Self>) {
        if self.acp.is_none() {
            self.acp = acp::Acp::new(&self.root, self.rpc_env());
            if self.acp.is_none() {
                self.agent_msgs.push(AgentMsg::Tool {
                    id: String::new(),
                    title: "could not start agent — is `npx` (Node 22+) on PATH?".into(),
                    status: "failed".into(),
                    detail: String::new(),
                    started: std::time::Instant::now(),
                });
            }
        }
        if self.acp.is_some() && !self.acp_polling {
            self.acp_polling = true;
            self.start_acp_poll(cx);
        }
    }

    /// Drain agent events ~12x/s and fold them into the transcript.
    fn start_acp_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(80)).await;
            let alive = this.update(cx, |this, cx| {
                let Some(acp) = this.acp.clone() else { return false };
                if acp.dirty() {
                    for ev in acp.drain() {
                        this.apply_acp_event(ev);
                    }
                    cx.notify();
                }
                true
            });
            if !matches!(alive, Ok(true)) {
                break;
            }
        })
        .detach();
    }

    fn apply_acp_event(&mut self, ev: acp::AcpEvent) {
        use acp::AcpEvent::*;
        match ev {
            Ready => {} // session ready; input was already accepting text
            Usage { used, size } => self.agent_ctx = Some((used, size)),
            // a replayed turn from session/load
            User(t) => {
                self.agent_cur = None;
                self.agent_cur_thought = None;
                self.agent_msgs.push(AgentMsg::User { text: t, imgs: Vec::new() });
            }
            Sessions(list) => {
                self.agent_sessions = list;
                // this answer was for the auto-resume, not the picker
                if std::mem::take(&mut self.agent_autoresume) {
                    self.agent_resumed = true;
                    if let Some((id, _)) = self.agent_sessions.first().cloned() {
                        self.reset_agent_transcript();
                        if let Some(acp) = self.acp.clone() {
                            acp.load_session(&self.root, &id);
                        }
                    }
                }
            }
            Title(t) => self.agent_title = session_title(&t),
            Text(t) => {
                self.agent_cur_thought = None; // real answer began → close the reasoning bubble
                match self.agent_cur {
                    Some(i) => {
                        if let Some(AgentMsg::Agent(s)) = self.agent_msgs.get_mut(i) {
                            s.push_str(&t);
                        }
                    }
                    None => {
                        self.agent_msgs.push(AgentMsg::Agent(t));
                        self.agent_cur = Some(self.agent_msgs.len() - 1);
                    }
                }
            }
            Thought(t) => match self.agent_cur_thought {
                Some(i) => {
                    if let Some(AgentMsg::Thought(s)) = self.agent_msgs.get_mut(i) {
                        s.push_str(&t);
                    }
                }
                None => {
                    self.agent_msgs.push(AgentMsg::Thought(t));
                    self.agent_cur_thought = Some(self.agent_msgs.len() - 1);
                }
            },
            Tool { id, title, status, detail } => {
                let detail = tool_note(&detail);
                self.agent_cur = None; // text after a tool call starts a fresh bubble
                self.agent_cur_thought = None;
                self.agent_msgs.push(AgentMsg::Tool {
                    id,
                    title,
                    status,
                    detail,
                    started: std::time::Instant::now(),
                });
            }
            ToolUpdate { id, title, status, detail } => {
                for m in self.agent_msgs.iter_mut() {
                    if let AgentMsg::Tool { id: mid, title: t, status: st, detail: d, .. } = m {
                        if *mid != id {
                            continue;
                        }
                        if let Some(status) = &status {
                            *st = status.clone();
                        }
                        // the announcement is made before the tool's input has
                        // finished streaming, so this is usually the first time
                        // we learn what the call actually is
                        if let Some(title) = &title {
                            *t = title.clone();
                        }
                        if let Some(detail) = &detail {
                            *d = tool_note(detail);
                        }
                    }
                }
            }
            TurnEnd => {
                self.agent_busy = false;
                self.agent_turn_start = None;
                self.acp_state = Some(AgentState::Done);
                self.agent_cur = None;
                self.agent_cur_thought = None;
                self.agent_next_queued();
            }
            Error(e) => {
                // a failed turn drops the queue: those prompts assumed the turn
                // they were typed behind actually happened
                self.agent_queue.clear();
                self.agent_busy = false;
                self.acp_state = Some(AgentState::Failed);
                self.agent_cur = None;
                self.agent_cur_thought = None;
                self.agent_msgs.push(AgentMsg::Tool {
                    id: String::new(),
                    title: e,
                    status: "failed".into(),
                    detail: String::new(),
                    started: std::time::Instant::now(),
                });
            }
        }
    }

    fn agent_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // pane navigation belongs to the dock, not to the message box
        let ks = &ev.keystroke;
        if self.agent_picker && ks.key == "escape" {
            self.agent_picker = false;
            cx.notify();
            return;
        }
        if ks.modifiers.platform {
            match ks.key.as_str() {
                // the dock stacks vertically, so only ⌘↑/⌘↓ move between panes —
                // ⌘←/⌘→ belong to the composer (line start/end, ⇧ to select)
                "down" | "up" => {
                    let forward = ks.key == "down";
                    self.move_pane_focus(false, forward, window, cx);
                    return;
                }
                "enter" if ks.modifiers.shift => {
                    self.agent_zoomed = !self.agent_zoomed;
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        let ks = &ev.keystroke;
        // terminal muscle memory: ⌃J adds a line, ⌃C abandons what you're typing
        // (and the turn in flight), ⌃N starts a fresh session
        if ks.modifiers.control {
            match ks.key.as_str() {
                "j" => {
                    self.agent_input.newline();
                    cx.notify();
                    return;
                }
                "c" => {
                    self.interrupt_agent(cx);
                    self.agent_input.clear();
                    self.agent_ghost = None;
                    cx.notify();
                    return;
                }
                "n" => {
                    self.new_agent_session(cx);
                    return;
                }
                // every other control combo is inert here rather than falling
                // through to the field, where it could edit the text
                _ => return,
            }
        }
        // ⌘V with an image on the clipboard attaches it to the next prompt
        if ks.key == "v" && ks.modifiers.platform && !ks.modifiers.alt {
            if self.attach_clipboard_image(cx) {
                return;
            }
            if self.paste_collapsed(cx) {
                return;
            }
        }
        // ⌥C quotes the selected agent text into the composer; ⌘C copies it
        if ks.key == "c" && ks.modifiers.alt && !ks.modifiers.platform {
            self.quote_agent_selection(window, cx);
            return;
        }
        if ks.key == "c" && ks.modifiers.platform && self.agent_sel.is_some() {
            if let Some(t) = self.agent_selected_text(cx) {
                cx.write_to_clipboard(ClipboardItem::new_string(t));
                self.show_flash("Copied", cx);
                return;
            }
        }
        // `[Image #1]` is one thing: arrows step over it and backspace takes the
        // whole token (and its attachment) rather than a character of it
        if !ks.modifiers.platform && !ks.modifiers.alt && !ks.modifiers.control {
            let (text, caret) = (self.agent_input.text.clone(), self.agent_input.caret);
            match ks.key.as_str() {
                "left" if !ks.modifiers.shift => {
                    if let Some((start, _, _)) = image_token_before(&text, caret) {
                        self.agent_input.move_to(start);
                        cx.notify();
                        return;
                    }
                }
                "right" if !ks.modifiers.shift => {
                    if let Some((_, end)) = image_token_after(&text, caret) {
                        self.agent_input.move_to(end);
                        cx.notify();
                        return;
                    }
                }
                "backspace" => {
                    if let Some((start, end, label)) = image_token_before(&text, caret) {
                        self.agent_input.splice(start..end, "");
                        self.agent_imgs.retain(|i| i.label != label);
                        if let Some(id) = label
                            .strip_prefix("Pasted text #")
                            .and_then(|r| r.split_whitespace().next())
                            .and_then(|n| n.parse::<usize>().ok())
                        {
                            self.agent_pastes.remove(&id);
                        }
                        cx.notify();
                        return;
                    }
                }
                _ => {}
            }
        }
        // word motions would otherwise land inside a token
        if ks.modifiers.alt && !ks.modifiers.platform && matches!(ks.key.as_str(), "left" | "right") {
            let forward = ks.key == "right";
            Self::field_input(&mut self.agent_input, ks, cx, |_| true);
            let (text, caret) = (self.agent_input.text.clone(), self.agent_input.caret);
            if let Some((start, end)) = image_token_around(&text, caret) {
                self.agent_input.move_to(if forward { end } else { start });
            }
            cx.notify();
            return;
        }
        match ks.key.as_str() {
            // the `@` list owns these keys while it's up
            "escape" if self.at_open => {
                self.at_open = false;
                self.at_items.clear();
            }
            "up" | "down" if self.at_open => {
                let last = self.at_items.len().saturating_sub(1);
                self.at_sel = if ks.key == "down" {
                    if self.at_sel >= last { 0 } else { self.at_sel + 1 }
                } else if self.at_sel == 0 {
                    last
                } else {
                    self.at_sel - 1
                };
                cx.notify();
                return;
            }
            "tab" | "enter" if self.at_open && !ks.modifiers.shift && !ks.modifiers.platform => {
                self.accept_mention();
                cx.notify();
                return;
            }
            "escape" if self.agent_jump_open => {
                self.agent_jump_open = false;
                cx.notify();
            }
            // a selection is what escape drops first
            "escape" if self.agent_sel.is_some() => {
                self.agent_sel = None;
                cx.notify();
            }
            // escape drops the jump list or a selection (above) and nothing
            // else: the pane isn't closable, so there's nothing left to close
            // ⇥ takes the suggestion
            "tab" if self.agent_ghost.is_some() => {
                if let Some(rest) = self.agent_ghost.take() {
                    self.agent_input.insert_text(&rest);
                }
            }
            // ↩ sends; the field's own newline handling is bypassed here
            "enter" if !ks.modifiers.shift => {
                let shown = self.agent_input.text.trim().to_string();
                if shown.is_empty() && self.agent_imgs.is_empty() {
                    return;
                }
                let full = self.expand_mentions(&self.expand_pastes(&shown));
                self.agent_input.clear();
                self.agent_ghost = None;
                self.agent_pastes.clear();
                self.agent_mentions.clear();
                self.at_open = false;
                self.at_items.clear();
                // the transcript shows what you typed (tokens and all); the
                // agent gets the pasted content spliced back in
                let label = (full != shown).then_some(shown);
                self.send_agent_prompt_as(full, label, cx);
            }
            "enter" => {
                self.agent_input.newline(); // ⇧↩ too, since it costs nothing
            }
            _ => {
                // printable text arrives through the input handler instead
                if !Self::text_comes_via_input_handler(ks) {
                    Self::field_input(&mut self.agent_input, ks, cx, |_| true);
                }
            }
        }
        self.recompute_agent_ghost();
        self.recompute_mentions(cx);
        cx.notify();
    }

    /// Stop the turn in flight (ctrl+c / the Stop button). The adapter ends the
    /// turn on its side; we drop the busy state right away so the UI responds.
    fn interrupt_agent(&mut self, cx: &mut Context<Self>) {
        if !self.agent_busy {
            return;
        }
        if let Some(acp) = self.acp.clone() {
            acp.cancel();
        }
        let dropped = self.agent_queue.len();
        self.agent_queue.clear();
        if dropped > 0 {
            self.show_flash(&format!("Interrupted — dropped {dropped} queued message(s)"), cx);
        }
        self.agent_busy = false;
        self.agent_turn_start = None;
        self.acp_state = None;
        self.agent_cur = None;
        self.agent_cur_thought = None;
        if dropped == 0 {
            self.show_flash("Interrupted", cx);
        }
        cx.notify();
    }

    /// What the agent is told about TIDE, once per session, ahead of the first
    /// prompt. It never appears in the transcript — the label carries what you
    /// actually typed.
    fn tide_briefing(&self) -> String {
        // The repo's own instructions come first, and they're named explicitly:
        // the adapter picks up CLAUDE.md on its own, but AGENTS.md is the file
        // this repo actually keeps its house rules in, and nothing loads it.
        let mut house = String::new();
        for name in ["AGENTS.md", "CLAUDE.md"] {
            if self.root.join(name).is_file() {
                if !house.is_empty() {
                    house.push_str(" and ");
                }
                house.push_str(name);
            }
        }
        let house = if house.is_empty() {
            String::new()
        } else {
            format!(
                "Read {house} at the root of this repo before you start, and work \
                 the way it says. Those are the house rules for this codebase and \
                 they outrank anything below.\n\n"
            )
        };
        format!("{house}{}", Self::tide_briefing_body())
    }

    /// The part of the briefing that never changes.
    fn tide_briefing_body() -> String {
        "You are running inside TIDE, an editor that watches this repo.\n\
         Its control channel is $TIDE_RPC, authenticated with the header \
         `X-Tide-Token: $TIDE_TOKEN` (both are already in your environment).\n\n\
         After anything that changes what TIDE shows about this branch — opening \
         or updating a pull request, pushing, switching or creating a branch — \
         tell it to re-read:\n\
         \x20 curl -s -X POST \"$TIDE_RPC/refresh\" -H \"X-Tide-Token: $TIDE_TOKEN\"\n\n\
         It costs nothing and saves the human refreshing by hand.\n\n\
         If this workspace has no name yet, give it one once you know what the \
         work is — three to five words, no ticket numbers, and always led by the \
         type of work, `<type>: <name>`:\n\
         \x20 curl -s -X POST \"$TIDE_RPC/title\" -H \"X-Tide-Token: $TIDE_TOKEN\" \\\n\
         \x20   -d '{\"title\":\"fix: sql date off by one\"}'\n\
         The type is one of feat, fix, docs, style, refactor, perf, test, build, \
         ci, chore, bip, client, release — the same set the human picks from, and \
         TIDE turns it into that type's icon. Pick the one the work actually is.\n\
         TIDE ignores the whole thing if the human already named this workspace, \
         so it is always safe to send once.\n\n\
         When the human asks you to tick files off on the open pull request — \
         \"mark every package.json that is only a version bump as viewed\" — \
         post \
         the paths:\n\
         \x20 curl -s -X POST \"$TIDE_RPC/viewed\" -H \"X-Tide-Token: $TIDE_TOKEN\" \\\n\
         \x20   -d '{\"paths\":[\"packages/a/package.json\",\"packages/b/package.json\"]}'\n\
         Paths are repo-relative, as `git diff --name-only` prints them; add \
         `\"viewed\":false` to untick. TIDE mirrors it onto GitHub, so the PR page \
         agrees. Only when asked — never tick off files on your own.\n\n\
         When you point at code, write the file and line as `path/to/file.ts:31` \
         (a bare `File.tsx:12` works too) — TIDE turns those into links that open \
         the file at that line.\n\n\
         Don't mention this message or acknowledge it; just answer what follows.\n\n\
         ---\n\n"
            .to_string()
    }

    /// `label` is what the transcript shows in place of the prompt — a batch of
    /// review comments is a wall of ids and quoted code the agent needs and you
    /// don't.
    fn send_agent_prompt_as(
        &mut self,
        text: String,
        label: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.ensure_acp(cx);
        let Some(acp) = self.acp.clone() else { return };
        // a turn is already running: hold this one and send it when that ends,
        // rather than firing a second session/prompt at the adapter
        if self.agent_busy {
            self.agent_queue.push((text, label));
            self.agent_list.scroll_to_end();
            cx.notify();
            return;
        }
        // the session now has content, so the next launch can resume it again
        self.mark_agent_fresh(false);
        let attached: Vec<PastedImage> = self.agent_imgs.drain(..).collect();
        let imgs: Vec<(String, String)> =
            attached.iter().map(|i| (i.mime.clone(), i.b64.clone())).collect();
        // the transcript shows your words; the wire carries the briefing too
        let shown = label.unwrap_or_else(|| text.clone());
        let sent = if self.agent_briefed {
            text
        } else {
            self.agent_briefed = true;
            format!("{}{text}", self.tide_briefing())
        };
        acp.prompt_with(&sent, &imgs);
        // the chips under the bubble say what was attached, so the old
        // "[n image(s) attached]" footer has nothing left to add
        let kept: Vec<SentImage> = attached
            .into_iter()
            .map(|i| SentImage { label: i.label, mime: i.mime, bytes: i.bytes, image: i.image })
            .collect();
        self.agent_msgs.push(AgentMsg::User { text: shown, imgs: kept });
        self.agent_turn_start = Some(std::time::Instant::now());
        self.start_agent_spinner(cx);
        // same signal a terminal agent's UserPromptSubmit hook sends
        self.acp_state = Some(AgentState::Working);
        if self.ws_idle {
            self.ws_idle = false;
            save_ws_idle(&self.root, false);
        }
        self.agent_list.scroll_to_end(); // your own turn always scrolls into view
        self.agent_cur = None;
        self.agent_cur_thought = None;
        self.agent_busy = true;
        cx.notify();
    }

    /// An agent message, rendered row by row so a row's text can be selected and
    /// quoted back at the agent. Fences and tables fall through to the plain
    /// renderer — selecting inside a code block is what the editor is for.
    fn render_agent_md(
        &mut self,
        msg: usize,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let rows = md_prepare(text);
        let cpath = self.agent_comment_path(msg);
        // rows that already carry a comment (1-based spans), and the row the open
        // box belongs under
        let commented: Vec<(usize, usize, bool)> = self
            .review
            .iter()
            .filter(|c| c.path == cpath)
            .map(|c| (c.line, c.end, !c.replies.is_empty()))
            .collect();
        let box_after = self
            .rc_open
            .then(|| self.rc_target.clone())
            .flatten()
            .filter(|(p, _, _)| *p == cpath)
            .map(|(_, _, b)| b.saturating_sub(1));
        let sel = self.agent_sel.filter(|(m, _, _)| *m == msg).map(|(_, a, b)| (a.min(b), a.max(b)));
        let selected = |i: usize| sel.is_some_and(|(a, b)| i >= a && i <= b);
        // one wrapper per row: press anchors the selection, moving over a row
        // while dragging extends to it
        let wrap = |i: usize, inner: AnyElement, cx: &mut Context<Self>| {
            div()
                .child(inner)
                // A press only *arms* the selection; the range appears once the
                // pointer actually moves. Selecting on mouse-down swallowed
                // clicks on file links, since the row re-rendered underneath the
                // click.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _w, cx| {
                        this.agent_sel_armed = Some((msg, i));
                        this.agent_sel_drag = true;
                        this.agent_sel_moved = false;
                        this.agent_sel_pos =
                            Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                        let _ = cx;
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        // right-clicking outside the current selection selects
                        // just this row, so the menu always has a target
                        let inside = this
                            .agent_sel
                            .is_some_and(|(m, a, b)| m == msg && i >= a.min(b) && i <= a.max(b));
                        if !inside {
                            this.agent_sel = Some((msg, i, i));
                            this.agent_sel_armed = None;
                        }
                        this.tree_ctx = Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                        this.tree_ctx_path = None;
                        this.tree_ctx_kind = CtxKind::AgentSel;
                        window.focus(&this.tree_ctx_focus, cx);
                        cx.notify();
                    }),
                )
                .on_mouse_move(cx.listener(move |this, ev: &MouseMoveEvent, _w, cx| {
                    if !this.agent_sel_drag {
                        return;
                    }
                    // first movement after the press starts the selection
                    if let Some((m, anchor)) = this.agent_sel_armed.take() {
                        this.agent_sel = Some((m, anchor, i));
                        this.agent_sel_moved = true;
                        this.agent_sel_pos =
                            Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                        cx.notify();
                        return;
                    }
                    if let Some((m, a, b)) = this.agent_sel {
                        if m == msg && b != i {
                            this.agent_sel = Some((m, a, i));
                            this.agent_sel_moved = true;
                            this.agent_sel_pos =
                                Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                            cx.notify();
                        }
                    }
                }))
                .into_any_element()
        };
        // build every row first, then stitch: selected runs go inside one
        // container so the purple bar down their left edge is continuous
        let mut built: Vec<(usize, AnyElement)> = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            if rows[i].cells.is_some() {
                let first = i;
                let mut grid = div()
                    .my_1()
                    .flex()
                    .flex_col()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .overflow_hidden();
                while i < rows.len() && rows[i].cells.is_some() {
                    grid = grid.child(md_row_el(&rows[i], selected(first)));
                    i += 1;
                }
                built.push((first, wrap(first, grid.into_any_element(), cx)));
                continue;
            }
            let row = &rows[i];
            // file references in the answer are links: `Icon.tsx:14` opens the
            // file at that line
            // two kinds of link in one line: a file reference opens the file,
            // a URL opens the browser. Sorted and non-overlapping, since that's
            // what the highlighter and the click ranges both need.
            let mut links: Vec<(std::ops::Range<usize>, LinkTarget)> = file_refs(&row.text)
                .into_iter()
                .filter(|r| self.file_ref_target(&r.path, cx).is_some())
                .map(|r| (r.range, LinkTarget::File(r.path, r.line)))
                .chain(
                    url_spans(&row.text)
                        .into_iter()
                        .map(|r| (r.clone(), LinkTarget::Url(row.text[r].to_string()))),
                )
                .collect();
            links.sort_by_key(|(r, _)| r.start);
            links.dedup_by(|b, a| b.0.start < a.0.end);
            // the marker sits in the reserved left gutter, once per span, exactly
            // like the editor's gutter icon — click it to open that thread
            let span = commented.iter().find(|(a, b, _)| i + 1 >= *a && i + 1 <= *b).copied();
            let gutter = match span {
                Some((a, b, answered)) if a == i + 1 => {
                    let p = cpath.clone();
                    div()
                        .id(("agent-comment-mark", msg * 10_000 + i))
                        .w(px(16.))
                        .flex_shrink_0()
                        .font_family(ICON_FONT)
                        .text_size(px(11.))
                        .cursor_pointer()
                        .text_color(rgb(if answered { GIT_NEW } else { COMMENT_MARK }))
                        .hover(|s| s.text_color(rgb(TEXT)))
                        .child(IC_COMMENT)
                        .tooltip(tip("Open this thread"))
                        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _e, window, cx| {
                            this.begin_comment(p.clone(), a, b, window, cx);
                        }))
                        .into_any_element()
                }
                // every row reserves the column, so nothing shifts sideways
                _ => div().w(px(16.)).flex_shrink_0().into_any_element(),
            };
            let inner = div()
                .flex()
                .flex_row()
                .items_start()
                .child(gutter)
                .child(
                    div()
                        .flex_grow(1.0)
                        .min_w(px(0.))
                        .text_size(px(row.size.max(1.0)))
                        .child(if links.is_empty() {
                            md_row_el(row, selected(i))
                        } else {
                            let ranges: Vec<std::ops::Range<usize>> =
                                links.iter().map(|(r, _)| r.clone()).collect();
                            let targets: Vec<LinkTarget> =
                                links.iter().map(|(_, t)| t.clone()).collect();
                            let styled = md_styled_links(row, row.heading, selected(i), &ranges);
                            let me = cx.entity().downgrade();
                            gpui::InteractiveText::new(("md-link", msg * 10_000 + i), styled)
                                .on_click(ranges, move |ix, window, cx| {
                                    match targets.get(ix) {
                                        Some(LinkTarget::Url(u)) => open_browser(u),
                                        Some(LinkTarget::File(path, line)) => {
                                            let (path, line) = (path.clone(), *line);
                                            if let Some(me) = me.upgrade() {
                                                me.update(cx, |this, cx| {
                                                    this.open_file_ref(&path, line, window, cx)
                                                });
                                            }
                                        }
                                        None => {}
                                    }
                                })
                                .into_any_element()
                        }),
                )
                .into_any_element();
            built.push((i, wrap(i, inner, cx)));
            i += 1;
        }
        // a row's left bar: purple while it's selected or carries a comment, and
        // tinted only while selected. The 2px border is always laid out, so
        // nothing moves sideways when it lights up.
        let has_comment =
            |i: usize| commented.iter().any(|(a, b, _)| i + 1 >= *a && i + 1 <= *b);
        let bar = |i: usize| (selected(i), has_comment(i));
        let mut col = div().flex().flex_col().gap(px(4.)).text_color(rgb(TEXT));
        let mut k = 0;
        while k < built.len() {
            let (on, marked) = bar(built[k].0);
            let mut group = div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .pl_1()
                .border_l_2()
                .when(on, |d| d.bg(rgba(COMMENT_TINT)))
                .border_color(rgb(COMMENT_MARK))
                .when(!on && !marked, |d| d.border_color(rgba(0x00000000)));
            while k < built.len() && bar(built[k].0) == (on, marked) {
                let ix = built[k].0;
                let (_, el) = std::mem::replace(&mut built[k], (usize::MAX, div().into_any_element()));
                group = group.child(el);
                if box_after == Some(ix) {
                    let focused = self.rc_focus.is_focused(window);
                    group = group.child(self.render_comment_prompt(focused, true, cx).into_any_element());
                }
                k += 1;
            }
            col = col.child(group);
        }
        col.into_any_element()
    }

    /// Where a comment on message `msg` is filed. The message index rides in the
    /// path so two answers can both have a commented row 3.
    fn agent_comment_path(&self, msg: usize) -> PathBuf {
        self.root.join(AGENT_SRC).join(msg.to_string())
    }

    /// The currently selected agent text: the selected rows, in order.
    fn agent_selected_text(&self, cx: &App) -> Option<String> {
        let (msg, a, b) = self.agent_sel?;
        let _ = cx;
        let text = match self.agent_msgs.get(msg)? {
            AgentMsg::Agent(t) | AgentMsg::Thought(t) => t.clone(),
            _ => return None,
        };
        let rows = md_prepare(&text);
        let (a, b) = (a.min(b), a.max(b).min(rows.len().saturating_sub(1)));
        let picked: Vec<String> = rows[a..=b]
            .iter()
            .filter(|r| !r.gap)
            .map(|r| match &r.cells {
                // a table row reads back as its cells, tab-separated
                Some(cells) => {
                    cells.iter().map(|c| c.text.to_string()).collect::<Vec<_>>().join("\t")
                }
                None => r.text.to_string(),
            })
            .collect();
        let out = picked.join("\n");
        (!out.trim().is_empty()).then_some(out)
    }

    fn act_quote_agent_sel(
        &mut self,
        _: &QuoteAgentSel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.quote_agent_selection(window, cx);
    }

    /// Pull an image off the clipboard into the pending attachments. Returns
    /// false when the clipboard holds no image, so the caller can paste text.
    fn attach_clipboard_image(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else { return false };
        let Some(img) = item.entries().iter().find_map(|e| match e {
            gpui::ClipboardEntry::Image(i) => Some(i.clone()),
            _ => None,
        }) else {
            return false;
        };
        // the adapter takes base64 in the prompt itself, so a huge paste would
        // land in the transcript as megabytes of text
        const MAX: usize = 6 * 1024 * 1024;
        if img.bytes.len() > MAX {
            self.show_flash("That image is too large to attach (over 6 MB)", cx);
            return true;
        }
        self.agent_img_seq += 1;
        let label = format!("Image #{}", self.agent_img_seq);
        // the token goes into the text at the caret, so the question can point
        // at a specific picture ("what's wrong in [Image #2]?")
        self.agent_input.insert_text(&format!("[{label}] "));
        self.agent_imgs.push(PastedImage {
            mime: img.format.mime_type().to_string(),
            b64: acp::base64(&img.bytes),
            bytes: img.bytes.len(),
            image: Arc::new(img),
            label,
        });
        cx.notify();
        true
    }

    /// A long clipboard paste lands as `[Pasted text #3 +216 lines]` rather than
    /// burying the composer. Pasting the same text again expands that token in
    /// place, which is the escape hatch when you do want to see it.
    /// Returns false when the clipboard isn't worth collapsing.
    fn paste_collapsed(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) else { return false };
        // already collapsed once: the second paste reveals it
        if let Some((id, _)) = self.agent_pastes.iter().find(|(_, t)| **t == text) {
            let id = *id;
            let token = self.paste_token(id, &text);
            if let Some(at) = self.agent_input.text.find(&token) {
                self.agent_input.splice(at..at + token.len(), &text);
                self.agent_pastes.remove(&id);
                cx.notify();
                return true;
            }
        }
        let lines = text.lines().count();
        if lines < 6 && text.len() < 400 {
            return false; // short enough to read in the box
        }
        self.agent_paste_seq += 1;
        let id = self.agent_paste_seq;
        self.agent_pastes.insert(id, text.clone());
        let token = self.paste_token(id, &text);
        self.agent_input.insert_text(&format!("{token} "));
        self.show_flash("Pasted text collapsed — paste again to expand it", cx);
        cx.notify();
        true
    }

    fn paste_token(&self, id: usize, text: &str) -> String {
        format!("[Pasted text #{id} +{} lines]", text.lines().count())
    }

    /// Swap every `[Pasted text #N …]` token back for what was pasted. The agent
    /// needs the content; only the composer and the transcript show the token.
    fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (id, full) in &self.agent_pastes {
            let token = self.paste_token(*id, full);
            out = out.replace(&format!("{token} "), full).replace(&token, full);
        }
        out
    }

    /// ⌥C in the agent pane: comment on the selected answer text. Opens the same
    /// box the editor uses and files the note into the review queue, so it goes
    /// back to the agent with everything else.
    fn quote_agent_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.agent_selected_text(cx) else {
            self.show_flash("Select some of the agent's text first", cx);
            return;
        };
        let Some((msg, a, b)) = self.agent_sel else { return };
        self.agent_sel = None;
        // the box's snippet is the quote; there's no file line to read it from
        self.rc_snippet = Some(text);
        // rows are 1-based here, so 0 stays free for "the whole answer"
        let (first, last) = (a.min(b) + 1, a.max(b) + 1);
        let path = self.agent_comment_path(msg);
        self.begin_comment(path, first, last, window, cx);
    }

    /// Animate the "thinking" row while a turn is in flight. Runs only for the
    /// duration of the turn, so nothing ticks when the agent is idle.
    fn start_agent_spinner(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(120)).await;
            let Ok(busy) = this.update(cx, |this, cx| {
                if this.agent_busy {
                    this.agent_spin = this.agent_spin.wrapping_add(1);
                    cx.notify();
                }
                this.agent_busy
            }) else {
                break;
            };
            if !busy {
                break;
            }
        })
        .detach();
    }

    /// Turn a written reference into a real file. An absolute-ish path is taken
    /// as repo-relative; a bare `Icon.tsx` is looked up in the finder's index,
    /// preferring a unique match and otherwise the shortest path (the least
    /// nested one is nearly always what the agent meant).
    fn resolve_file_ref(&self, path: &str) -> Option<PathBuf> {
        let direct = self.root.join(path);
        if direct.is_file() {
            return Some(direct);
        }
        if path.contains('/') {
            // a partial path: match on the suffix
            let mut hits: Vec<&PathBuf> =
                self.all_files.iter().filter(|p| p.to_string_lossy().ends_with(path)).collect();
            hits.sort_by_key(|p| p.to_string_lossy().len());
            return hits.first().map(|p| (*p).clone());
        }
        let mut hits: Vec<&PathBuf> = self
            .all_files
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n == path))
            .collect();
        hits.sort_by_key(|p| p.to_string_lossy().len());
        hits.first().map(|p| (*p).clone())
    }

    /// [`resolve_file_ref`] memoized, and the reason the file index gets built
    /// even when the finder was never opened: a reference is only drawn as a
    /// link if it resolves, so this runs while rendering the transcript.
    fn file_ref_target(&mut self, path: &str, cx: &mut Context<Self>) -> Option<PathBuf> {
        if let Some(hit) = self.ref_hits.get(path) {
            return hit.clone();
        }
        self.index_files(cx);
        let hit = self.resolve_file_ref(path);
        self.ref_hits.insert(path.to_string(), hit.clone());
        hit
    }

    /// Scan the project's paths once, in the background. The finder does its own
    /// (debounced, repeatable) scan; this is the first-use one for everything
    /// else, and it never runs twice.
    fn index_files(&mut self, cx: &mut Context<Self>) {
        if self.files_scanned {
            return;
        }
        self.files_scanned = true;
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let (files, dirs) = cx
                .background_executor()
                .spawn(async move { collect_paths(&root) })
                .await;
            this.update(cx, |this, cx| {
                this.all_files = files;
                this.all_dirs = dirs;
                this.ref_hits.clear();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open a file reference from the transcript at its line.
    fn open_file_ref(&mut self, path: &str, line: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.file_ref_target(path, cx) else {
            self.show_flash(&format!("Can't find {path} in this project"), cx);
            return;
        };
        self.open_file(p, window, cx);
        if line > 0 {
            if let Some(tab) = self.tabs.get(self.active) {
                tab.editor.update(cx, |e, cx| e.goto(line, 1, cx));
            }
        }
    }

    /// The `@` list, sitting on top of the composer. Rows are clickable, ↑/↓
    /// move, ⇥ or ↩ takes the highlighted one.
    fn render_mentions(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.at_open || self.at_items.is_empty() {
            return None;
        }
        let mut panel = div()
            .absolute()
            .left_0()
            .right_0()
            // bottom: 100% — the list grows upwards from the composer's top edge
            .bottom(relative(1.))
            .mb_1()
            .max_h(px(240.))
            .overflow_hidden()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .py_1();
        for (i, m) in self.at_items.iter().enumerate() {
            let sel = i == self.at_sel;
            panel = panel.child(
                div()
                    .id(("agent-mention", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .hover(|s| s.bg(rgb(if sel { SELECTED_BG } else { HOVER })))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(12.))
                            .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                            .child(m.token.clone()),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .child(m.detail.clone()),
                    )
                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.at_sel = i;
                        this.accept_mention();
                        window.focus(&this.agent_focus, cx);
                        cx.notify();
                    })),
            );
        }
        Some(panel)
    }

    /// Recompute the `@` completion from the composer's text and caret. Driven
    /// off the text rather than off keystrokes, so typing, backspacing, pasting
    /// and moving the caret all end up in the same place.
    fn recompute_mentions(&mut self, cx: &mut Context<Self>) {
        let text = self.agent_input.text.clone();
        let caret = self.agent_input.caret_safe();
        let Some((i, query)) = mention_span(&text, caret) else {
            self.at_open = false;
            self.at_items.clear();
            return;
        };
        self.at_start = i;
        self.at_items = self.mention_matches(&query, cx);
        self.at_open = !self.at_items.is_empty();
        self.at_sel = self.at_sel.min(self.at_items.len().saturating_sub(1));
    }

    /// What `@query` can mean: any open workspace, then any file in this one.
    /// Workspaces come first because that's the thing you can't otherwise say —
    /// the agent already knows its own repo.
    fn mention_matches(&mut self, query: &str, cx: &mut Context<Self>) -> Vec<Mention> {
        let q = query.to_lowercase();
        let mut out: Vec<Mention> = Vec::new();
        for (i, root) in self.ws_roots.iter().enumerate() {
            let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let title = self.ws_names.get(i).cloned().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            if !q.is_empty()
                && !name.to_lowercase().contains(&q)
                && !title.to_lowercase().contains(&q)
            {
                continue;
            }
            let detail = if title.is_empty() || title == name {
                root.to_string_lossy().to_string()
            } else {
                format!("{title}  ·  {}", root.to_string_lossy())
            };
            out.push(Mention {
                token: format!("@{name}"),
                detail,
                // the agent needs the path, not the nickname
                expand: Some(format!("{name} ({})", root.to_string_lossy())),
            });
        }
        // files need something to go on: every path matches an empty query
        if q.len() >= 2 {
            self.index_files(cx);
            let mut hits: Vec<String> = self
                .all_files
                .iter()
                .map(|p| self.rel(p))
                .filter(|rel| rel.to_lowercase().contains(&q))
                .collect();
            hits.sort_by_key(|r| r.len());
            for rel in hits.into_iter().take(8) {
                let folder = rel.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
                // a path says everything already: it goes in as itself, no `@`
                out.push(Mention { token: rel, detail: folder, expand: None });
            }
        }
        out.truncate(12);
        out
    }

    /// Put the highlighted candidate into the composer.
    fn accept_mention(&mut self) {
        let Some(m) = self.at_items.get(self.at_sel).cloned() else { return };
        let caret = self.agent_input.caret_safe();
        self.agent_input.splice(self.at_start..caret, &format!("{} ", m.token));
        if let Some(expand) = m.expand {
            self.agent_mentions.insert(m.token, expand);
        }
        self.at_open = false;
        self.at_items.clear();
        self.at_sel = 0;
    }

    /// `@wby-next6` → `wby-next6 (/Users/adrian/dev/wby-next6)`. The composer
    /// keeps the short form; the agent gets the path it can actually open.
    fn expand_mentions(&self, text: &str) -> String {
        let mut toks: Vec<(&String, &String)> = self.agent_mentions.iter().collect();
        // longest first: `@wby` must not eat its way into `@wby-next6`
        toks.sort_by_key(|(t, _)| std::cmp::Reverse(t.len()));
        let mut out = text.to_string();
        for (token, expansion) in toks {
            out = out.replace(token.as_str(), expansion);
        }
        out
    }

    /// The rest of the newest message that starts with what's typed, fish-style.
    /// History only — no model call, so it costs nothing and never surprises.
    fn recompute_agent_ghost(&mut self) {
        let typed = self.agent_input.text.clone();
        self.agent_ghost = None;
        // only for a single line, and only once there's enough to match on
        if typed.trim().len() < 3 || typed.contains('\n') {
            return;
        }
        let lower = typed.to_lowercase();
        self.agent_ghost = self
            .agent_msgs
            .iter()
            .rev()
            .filter_map(|m| match m {
                AgentMsg::User { text, .. } => Some(text),
                _ => None,
            })
            .filter(|t| !t.contains('\n'))
            .find(|t| t.len() > typed.len() && t.to_lowercase().starts_with(&lower))
            .map(|t| t[typed.len()..].to_string());
    }

    /// Keys while the jump list is open: esc closes it, ↩ jumps to the newest
    /// match, everything else filters.
    fn agent_jump_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.agent_jump_open = false;
                self.agent_jump_query.clear();
                window.focus(&self.agent_focus, cx);
            }
            "enter" => {
                let q = self.agent_jump_query.text.to_lowercase();
                let hit = self.agent_msgs.iter().enumerate().rev().find(|(_, m)| match m {
                    AgentMsg::User { text, .. } => q.is_empty() || text.to_lowercase().contains(&q),
                    _ => false,
                });
                if let Some((ix, _)) = hit {
                    self.agent_list.scroll_to_reveal_item(ix);
                    self.agent_jump_open = false;
                    self.agent_jump_query.clear();
                    window.focus(&self.agent_focus, cx);
                }
            }
            _ => {
                // printable text arrives through the input handler instead
                if !Self::text_comes_via_input_handler(ks) {
                    Self::field_input(&mut self.agent_jump_query, ks, cx, |_| true);
                }
            }
        }
        cx.notify();
    }

    /// Send the next queued prompt, if any. Called when a turn ends.
    fn agent_next_queued(&mut self) {
        if self.agent_queue.is_empty() {
            return;
        }
        let (text, label) = self.agent_queue.remove(0);
        if let Some(acp) = self.acp.clone() {
            let sent = if self.agent_briefed {
                text.clone()
            } else {
                self.agent_briefed = true;
                format!("{}{text}", self.tide_briefing())
            };
            acp.prompt(&sent);
            self.agent_msgs
                .push(AgentMsg::User { text: label.unwrap_or(text), imgs: Vec::new() });
            self.agent_turn_start = Some(std::time::Instant::now());
            self.acp_state = Some(AgentState::Working);
            self.agent_cur = None;
            self.agent_cur_thought = None;
            self.agent_busy = true;
            self.agent_list.scroll_to_end();
        }
    }

    /// The briefing, on screen: the same text every new session is handed
    /// before your first prompt. Read-only, copyable, closed by clicking away.
    fn render_agent_brief(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.agent_brief_open {
            return None;
        }
        let text = self.tide_briefing();
        let copy = text.clone();
        Some(
            div()
                .absolute()
                .inset_0()
                .id("agent-brief-backdrop")
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.agent_brief_open = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .absolute()
                        .top(px(38.))
                        .left(px(8.))
                        .right(px(8.))
                        .max_h(px(420.))
                        .bg(rgb(POPUP_BG))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded_md()
                        .shadow_lg()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        // clicks inside are for the panel, not the backdrop
                        .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_1p5()
                                .border_b_1()
                                .border_color(rgb(BORDER))
                                .text_size(px(11.))
                                .text_color(rgb(MUTED))
                                .child(
                                    div()
                                        .flex_grow(1.0)
                                        .text_color(rgb(TEXT))
                                        .child("Sent to every new session, before your first message"),
                                )
                                .child(
                                    div()
                                        .id("agent-brief-copy")
                                        .px_1p5()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                        .child("copy")
                                        .on_click(cx.listener(move |this, _e, _w, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                            this.show_flash("Briefing copied", cx);
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    div()
                                        .id("agent-brief-close")
                                        .px_1p5()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                        .child("✕")
                                        .on_click(cx.listener(|this, _e, _w, cx| {
                                            this.agent_brief_open = false;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("agent-brief-body")
                                .min_h(px(0.))
                                .overflow_y_scroll()
                                .px_3()
                                .py_2()
                                .font_family("Menlo")
                                .text_size(px(11.))
                                .line_height(px(16.))
                                .text_color(rgb(TEXT))
                                // the trailing `---` separator is machinery, not
                                // instruction: the panel shows what it says
                                .child(text.trim_end_matches(['-', '\n', ' ']).to_string()),
                        ),
                ),
        )
    }

    fn render_agent(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The pane is open from startup, so nobody had to toggle it — start the
        // adapter and resume the last session the first time it's actually drawn.
        // Lazy on purpose: only the workspace you're looking at spawns an adapter.
        if self.acp.is_none() {
            self.ensure_acp(cx);
            self.autoresume_agent(cx);
        }
        // header
        let header = div()
            .h(px(34.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    // the title is the adapter's, and can be a paragraph: clip the
                    // whole group so it can't shove the buttons out of the header
                    .flex_grow(1.0)
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(div().flex_shrink_0().font_family(ICON_FONT).text_size(px(14.)).text_color(rgb(ICON)).child(IC_AGENT))
                    .child(div().flex_shrink_0().text_size(px(12.)).text_color(rgb(TEXT)).child("Agent"))
                    // the adapter's own title for the session, once it has one
                    .when_some(self.agent_title.clone(), |d, t| {
                        d.child(
                            div()
                                .min_w(px(0.))
                                .truncate()
                                .text_size(px(11.))
                                .text_color(rgb(MUTED))
                                .child(t),
                        )
                    })
                    // context bar straight from ACP's usage_update
                    .when_some(self.agent_ctx, |d, (used, size)| {
                        let pct = if size > 0 { (used * 100 / size).min(100) } else { 0 };
                        d.child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(if pct >= 80 { GIT_DELETED } else { LINE_NUMBER }))
                                .child(format!("ctx {} / {} · {pct}%", usage::short(used), usage::short(size))),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0() // the actions keep their room, always
                    // resume an earlier session for this project (ACP session/list
                    // + session/load; Claude Code's own /resume isn't available
                    // over the protocol)
                    // jump list of your own turns, for walking back through a
                    // long conversation
                    .when(!self.agent_msgs.is_empty(), |d| {
                        let mine = self
                            .agent_msgs
                            .iter()
                            .filter(|m| matches!(m, AgentMsg::User { .. }))
                            .count();
                        let open = self.agent_jump_open;
                        d.child(
                            div()
                                .id("agent-jump")
                                .px_1p5()
                                .rounded_md()
                                .cursor_pointer()
                                .text_size(px(11.))
                                .when(open, |d| d.bg(rgb(HOVER)))
                                .text_color(rgb(MUTED))
                                .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                .child(format!("☰ {mine}"))
                                .tooltip(tip("Your messages — click to jump"))
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    this.agent_jump_open = !this.agent_jump_open;
                                    this.agent_jump_query.clear();
                                    if this.agent_jump_open {
                                        window.focus(&this.agent_jump_focus, cx);
                                    }
                                    cx.notify();
                                })),
                        )
                    })
                    // what TIDE says to the agent before your first word: not a
                    // secret, just hidden, so there's a way to read it
                    .child(
                        div()
                            .id("agent-brief")
                            .px_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .font_family(ICON_FONT)
                            .text_size(px(12.))
                            .when(self.agent_brief_open, |d| d.bg(rgb(HOVER)))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                            .child(IC_GEAR)
                            .tooltip(tip("The instructions TIDE gives this agent"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.agent_brief_open = !this.agent_brief_open;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("agent-resume")
                            .px_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                            .child("↻ Resume")
                            .tooltip(tip("Resume an earlier session in this project"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.open_agent_picker(cx))),
                    )
                    .child(
                        div()
                            .id("agent-new")
                            .px_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                            .child("+ New")
                            .tooltip(tip("Start a fresh session  (⌃N)"))
                            .on_click(cx.listener(|this, _e, _w, cx| this.new_agent_session(cx))),
                    )
                    .child(
                        div()
                            .id("agent-close")
                            .px_1()
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.text_color(rgb(TEXT)))
                            .child("✕")
                            .on_click(cx.listener(|this, _e, window, cx| this.toggle_agent(window, cx))),
                    ),
            );

        // transcript
        // Virtualized: only the messages on screen are built, so a long session
        // costs the same to render as a short one. `ListAlignment::Bottom` is the
        // chat-log behaviour — it follows the tail until you scroll away.
        // rows = messages + the "thinking" row + one row per queued prompt
        let count = self.agent_msgs.len() + usize::from(self.agent_busy) + self.agent_queue.len();
        if count != self.agent_list_len {
            if count > self.agent_list_len {
                // appended: splice keeps the scroll where it is, and a reader
                // parked at the end keeps following the tail
                self.agent_list.splice(self.agent_list_len..self.agent_list_len, count - self.agent_list_len);
            } else {
                self.agent_list.reset(count); // a new or replayed session
            }
            self.agent_list_len = count;
        }
        // which of my messages the answer on screen belongs to: the last User
        // message at or above the first visible item, pinned as a header once it
        // has scrolled out of view
        let top_ix = self.agent_list.logical_scroll_top().item_ix;
        let pinned = (top_ix > 0)
            .then(|| {
                self.agent_msgs[..top_ix.min(self.agent_msgs.len())]
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(i, m)| match m {
                        AgentMsg::User { text, .. } => Some((i, text.clone())),
                        _ => None,
                    })
            })
            .flatten();
        let list = if count == 0 {
            let starting = self.acp.as_ref().map(|a| !a.is_ready()).unwrap_or(false);
            let hint = if starting {
                "Starting agent…"
            } else {
                "Ask Claude about this project. It can read and edit files in the working tree."
            };
            div()
                .flex_grow(1.0)
                .min_h(px(0.))
                .p_3()
                .text_size(px(12.))
                .text_color(rgb(MUTED))
                .child(hint)
                .into_any_element()
        } else {
            gpui::list(
                self.agent_list.clone(),
                cx.processor(|this: &mut Self, ix: usize, window: &mut Window, cx: &mut Context<Self>| {
                    if let Some(m) = this.agent_msgs.get(ix) {
                        // agent prose is selectable, so it can be quoted back
                        if let AgentMsg::Agent(t) = m {
                            let t = t.clone();
                            return div()
                                .px_3()
                                .py_1()
                                .child(this.render_agent_md(ix, &t, window, cx))
                                .into_any_element();
                        }
                        return div().px_3().py_1().child(render_agent_msg(m, ix)).into_any_element();
                    }
                    let after = ix - this.agent_msgs.len();
                    // the "thinking" row sits between the transcript and the queue
                    if this.agent_busy && after == 0 {
                        const SPIN: [&str; 10] =
                            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                        let secs = this
                            .agent_turn_start
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let elapsed = if secs >= 60 {
                            format!("{}m {}s", secs / 60, secs % 60)
                        } else {
                            format!("{secs}s")
                        };
                        return div()
                            .px_3()
                            .py_1()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .text_size(px(12.))
                            .child(
                                div()
                                    .text_color(rgb(ACCENT))
                                    .child(SPIN[this.agent_spin % SPIN.len()]),
                            )
                            .child(div().text_color(rgb(MUTED)).child("thinking…"))
                            .child(div().text_color(rgb(LINE_NUMBER)).child(elapsed))
                            .into_any_element();
                    }
                    let qi = after - usize::from(this.agent_busy);
                    match this.agent_queue.get(qi).map(|(t, l)| l.clone().unwrap_or_else(|| t.clone())) {
                        Some(text) => div()
                            .px_3()
                            .py_1()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED))
                                    .child("You  ·  queued")
                                    // a queued message hasn't been sent yet, so
                                    // changing your mind has to be possible
                                    .child(
                                        div()
                                            .id(("agent-queue-drop", qi))
                                            .px_1()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(GIT_DELETED)))
                                            .child("✕ remove")
                                            .tooltip(tip("Don't send this one"))
                                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                                if qi < this.agent_queue.len() {
                                                    this.agent_queue.remove(qi);
                                                    cx.notify();
                                                }
                                            })),
                                    )
                                    // and putting it back in the box to edit is
                                    // the other half of that
                                    .child(
                                        div()
                                            .id(("agent-queue-edit", qi))
                                            .px_1()
                                            .rounded_md()
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                            .child("edit")
                                            .tooltip(tip("Move it back into the composer"))
                                            .on_click(cx.listener(move |this, _e, window, cx| {
                                                if qi < this.agent_queue.len() {
                                                    let (t, _) = this.agent_queue.remove(qi);
                                                    this.agent_input.set(t);
                                                    window.focus(&this.agent_focus, cx);
                                                    cx.notify();
                                                }
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(rgb(SELECTED_BG))
                                    .text_size(px(14.5))
                                    .text_color(rgb(MUTED))
                                    .child(text),
                            )
                            .into_any_element(),
                        None => div().into_any_element(),
                    }
                }),
            )
            .flex_grow(1.0)
            .min_h(px(0.))
            .into_any_element()
        };
        // The pinned "You" row is a real row above the list, not an overlay: a
        // `List` needs a definite height from its parent, and wrapping it in a
        // relative box collapsed it to nothing.
        let pinned_row = pinned.map(|(ix, text)| {
            let one_line = text.replace('\n', " ");
            div()
                .id("agent-pinned")
                .flex_shrink_0()
                .px_3()
                .py_1()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .bg(rgb(SELECTED_BG))
                .border_b_1()
                .border_color(rgb(ACCENT))
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(rgb(SEL_TEXT))
                .child(div().flex_shrink_0().text_color(rgb(MUTED)).child("You"))
                .child(div().flex_grow(1.0).min_w(px(0.)).truncate().child(one_line))
                .tooltip(tip("Jump back to this message"))
                // click it to go back to where that turn started
                .on_click(cx.listener(move |this, _e, _w, cx| {
                    this.agent_list.scroll_to_reveal_item(ix);
                    cx.notify();
                }))
                .into_any_element()
        });

        // input: a text area (grows with the prompt, capped), with the turn's
        // Stop button beside it while the agent is working
        let focused = self.agent_focus.is_focused(window);
        // 4 rows minimum: the box shouldn't read as a one-line field pinned to
        // the very bottom of the pane
        let rows = self.agent_input.text.split('\n').count().clamp(4, 14) as f32;
        let busy = self.agent_busy;
        let pending_comments =
            self.review.iter().filter(|c| !c.draft && !c.sent && !c.resolved).count();
        // one box, full width: the text on top, controls along its bottom edge
        // (the way Claude's own composer does it)
        let input = div()
            .flex_shrink_0()
            .relative()
            .child(input_sink(self.agent_focus.clone(), cx.entity()))
            .children(self.render_mentions(cx))
            .mx_2()
            .mt_2()
            .mb_3()
            .px_2()
            .py_1()
            .bg(rgb(BG))
            .border_1()
            .border_color(rgb(if focused { ACCENT } else { BORDER }))
            .rounded_md()
            .flex()
            .flex_col()
            // pasted images waiting to go with the next message: a thumbnail
            // chip each, hover for a bigger look
            .when(!self.agent_imgs.is_empty(), |d| {
                let mut row = div().mb_1p5().flex().flex_row().flex_wrap().gap_2();
                for (i, pi) in self.agent_imgs.iter().enumerate() {
                    let kind = pi.mime.rsplit('/').next().unwrap_or("image").to_uppercase();
                    let kb = (pi.bytes as f32 / 1024.0).round() as usize;
                    let preview = pi.image.clone();
                    row = row.child(
                        div()
                            .id(("agent-img", i))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .pl_0p5()
                            .pr_1()
                            .rounded_md()
                            .bg(rgb(CURRENT_LINE))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(
                                gpui::img(pi.image.clone())
                                    .h(px(104.))
                                    .max_w(px(192.))
                                    .rounded_sm(),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(div().text_color(rgb(COMMENT_MARK)).child(pi.label.clone()))
                                    .child(format!("{kind}  ·  {kb} KB")),
                            )
                            .tooltip(move |_w, cx| {
                                cx.new(|_| ImagePreview { image: preview.clone() }).into()
                            })
                            .child(
                                div()
                                    .id(("agent-img-drop", i))
                                    .px_1()
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(rgb(GIT_DELETED)))
                                    .child("✕")
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        if i < this.agent_imgs.len() {
                                            let gone = this.agent_imgs.remove(i);
                                            let token = format!("[{}] ", gone.label);
                                            let text = this
                                                .agent_input
                                                .text
                                                .replace(&token, "")
                                                .replace(&format!("[{}]", gone.label), "");
                                            this.agent_input.set(text);
                                        }
                                        cx.notify();
                                    })),
                            ),
                    );
                }
                d.child(row)
            })
            .child(
                div()
                    .min_h(px(rows * 20.0))
                    .max_h(px(14.0 * 20.0))
                    .overflow_hidden()
                    .line_height(px(20.))
                    .text_size(px(14.))
                    .text_color(rgb(TEXT))
                    // clicking anywhere in the box starts typing
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, window, cx| {
                            window.focus(&this.agent_focus, cx);
                            cx.notify();
                        }),
                    )
                    .child(if self.agent_input.text.is_empty() {
                        div()
                            .text_color(rgb(MUTED))
                            .child(format!("Message the agent…{}", self.caret_if(focused)))
                    } else {
                        // the ghost sits after the text, dimmed, like a shell's
                        // history suggestion — ⇥ takes it
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_start()
                            // min_w(0) lets the text shrink below its content
                            // width, which is what makes a long line wrap instead
                            // of running off the edge of the box
                            .child(
                                div()
                                    .min_w(px(0.))
                                    .child(self.agent_input.render(self.caret_if(focused), SELECTION)),
                            )
                            .when_some(self.agent_ghost.clone(), |d, rest| {
                                d.child(
                                    div()
                                        .min_w(px(0.))
                                        .truncate()
                                        .text_color(rgb(LINE_NUMBER))
                                        .child(rest),
                                )
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .ml_2()
                                        .text_size(px(9.))
                                        .text_color(rgb(LINE_NUMBER))
                                        .child("⇥"),
                                )
                            })
                    }),
            )
            // bottom row inside the box: the key legend, and Stop while a turn runs
            .child(
                div()
                    .h(px(20.))
                    .flex_shrink_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(LINE_NUMBER))
                            .child("↩ send  ·  @ reference  ·  ⌃J newline  ·  ⌃N new  ·  ⌘V paste image  ·  ⌥C comment  ·  ⌃C clear"),
                    )
                    .child(div().flex_grow(1.0))
                    // pending review comments are usually written while reading an
                    // answer right here — the send button belongs next to the box
                    // you're already looking at, not only in the review pane
                    .when(pending_comments > 0, |d| {
                        d.child(
                            div()
                                .id("agent-send-comments")
                                .flex_shrink_0()
                                .mr_2()
                                .px_1p5()
                                .rounded_md()
                                .cursor_pointer()
                                .text_size(px(11.))
                                .border_1()
                                .border_color(rgb(COMMENT_MARK))
                                .text_color(rgb(COMMENT_MARK))
                                .hover(|s| s.bg(rgb(HOVER)))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                // the codicon needs its own font family
                                .child(div().font_family(ICON_FONT).text_size(px(10.)).child(IC_COMMENT))
                                .child(format!(
                                    "Send {pending_comments} comment{}",
                                    if pending_comments == 1 { "" } else { "s" }
                                ))
                                .tooltip(tip("Send the pending review comments  (⌥↩)"))
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    this.send_review(window, cx)
                                })),
                        )
                    })
                    .when(busy, |d| {
                        d.child(
                            div()
                                .id("agent-stop")
                                .flex_shrink_0()
                                .px_1p5()
                                .rounded_md()
                                .cursor_pointer()
                                .text_size(px(11.))
                                .border_1()
                                .border_color(rgb(GIT_DELETED))
                                .text_color(rgb(GIT_DELETED))
                                .hover(|s| s.bg(rgb(HOVER)))
                                .child("■ Stop")
                                .tooltip(tip("Interrupt the agent  (⌃C, which also clears the box)"))
                                .on_click(cx.listener(|this, _e, _w, cx| this.interrupt_agent(cx))),
                        )
                    }),
            );

        // the resume list replaces the transcript while it's open
        let picker = self.agent_picker.then(|| {
            let mut col = div()
                .id("agent-sessions")
                .flex_grow(1.0)
                .min_h(px(0.))
                .overflow_y_scroll()
                .flex()
                .flex_col();
            col = col.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(if self.agent_sessions.is_empty() {
                        "Loading sessions…".to_string()
                    } else {
                        format!("{} session(s) in this project", self.agent_sessions.len())
                    }),
            );
            for (i, (id, title)) in self.agent_sessions.iter().enumerate() {
                let sid = id.clone();
                col = col.child(
                    div()
                        .id(("agent-session", i))
                        .px_3()
                        .py_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(HOVER)))
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(TEXT))
                                .truncate()
                                .child(title.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(LINE_NUMBER))
                                .child(id.chars().take(8).collect::<String>()),
                        )
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.resume_agent_session(sid.clone(), cx);
                        })),
                );
            }
            col
        });

        // sized by the dock that hosts it (a share of the dock's height)
        div()
            .size_full()
            .flex()
            .flex_col()
            .min_h(px(0.))
            .bg(rgb(PANEL_BG))
            .track_focus(&self.agent_focus)
            .key_context("Agent")
            .on_key_down(cx.listener(Self::agent_key))
            .relative()
            .child(header)
            // the pinned row only shows with the transcript, never over the picker
            .children(if picker.is_some() { None } else { pinned_row })
            .child(match picker {
                Some(p) => p.into_any_element(),
                None => list.into_any_element(),
            })
            .child(input)
            .children(self.render_agent_brief(cx))
            // last child, so it paints over the transcript instead of under it
            .when(self.agent_jump_open, |d| {
                let jump_focused = self.agent_jump_focus.is_focused(window);
                let q = self.agent_jump_query.text.to_lowercase();
                let mut panel = div()
                    .id("agent-jump-list")
                    .absolute()
                    .top(px(34.))
                    .left(px(8.))
                    .right(px(8.))
                    .max_h(px(420.))
                    .overflow_y_scroll()
                    .pb_1()
                    .bg(rgb(POPUP_BG))
                    .border_1()
                    .border_color(rgb(ACCENT))
                    .rounded_md()
                    .shadow_lg()
                    .occlude()
                    .track_focus(&self.agent_jump_focus)
                    .on_key_down(cx.listener(Self::agent_jump_key))
                    .child(input_sink(self.agent_jump_focus.clone(), cx.entity()))
                    .flex()
                    .flex_col()
                    // filter: a long conversation is easier to search than to scroll
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .text_size(px(11.))
                            .child(div().font_family(ICON_FONT).text_color(rgb(MUTED)).child(IC_SEARCH))
                            .child(if self.agent_jump_query.is_empty() {
                                div()
                                    .flex_grow(1.0)
                                    .text_color(rgb(MUTED))
                                    .child(format!("Filter your messages…{}", self.caret_if(jump_focused)))
                            } else {
                                div().flex_grow(1.0).text_color(rgb(TEXT)).child(
                                    self.agent_jump_query.render(self.caret_if(jump_focused), SELECTION),
                                )
                            }),
                    );
                // newest first: the turn you want is nearly always a recent one
                let mine: Vec<(usize, &String)> = self
                    .agent_msgs
                    .iter()
                    .enumerate()
                    .filter_map(|(ix, m)| match m {
                        AgentMsg::User { text, .. } => Some((ix, text)),
                        _ => None,
                    })
                    .collect();
                let total = mine.len();
                for (pos, (ix, text)) in mine.into_iter().rev().enumerate() {
                    // keep the conversation's own numbering, counted from the top
                    let n = total - pos;
                    if !q.is_empty() && !text.to_lowercase().contains(&q) {
                        continue;
                    }
                    let one_line = text.replace('\n', " ");
                    let label = if one_line.chars().count() > 110 {
                        format!("{}…", one_line.chars().take(110).collect::<String>())
                    } else {
                        one_line
                    };
                    panel = panel.child(
                        div()
                            .id(("agent-jump-row", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .text_size(px(11.))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .w(px(20.))
                                    .text_color(rgb(LINE_NUMBER))
                                    .child(format!("{n}")),
                            )
                            .child(
                                div()
                                    .flex_grow(1.0)
                                    .min_w(px(0.))
                                    .truncate()
                                    .text_color(rgb(TEXT))
                                    .child(label),
                            )
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.agent_list.scroll_to_reveal_item(ix);
                                this.agent_jump_open = false;
                                this.agent_jump_query.clear();
                                cx.notify();
                            })),
                    );
                }
                d.child(panel)
            })
    }
}

/// Render one transcript entry (user bubble, assistant text, or a tool card).
/// A tool call's text kept for display only: the first few lines, clipped. The
/// full thing can be a whole file (a Read's result) and the row shows two lines,
/// so there's no reason to hold on to the rest for the life of the session.
fn tool_note(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .map(|l| l.chars().take(200).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_agent_msg(m: &AgentMsg, ix: usize) -> impl IntoElement {
    match m {
        AgentMsg::User { text: t, imgs } => div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_size(px(10.)).text_color(rgb(MUTED)).child("You"))
            .child(
                div()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(rgb(SELECTED_BG))
                    // same size as the agent's prose (md_doc's body size)
                    .text_size(px(14.5))
                    .text_color(rgb(SEL_TEXT))
                    // a URL you pasted is a link here too: underlined rather
                    // than recoloured, since accent-on-selection is unreadable
                    .child({
                        let spans = url_spans(t);
                        if spans.is_empty() {
                            div().child(t.clone()).into_any_element()
                        } else {
                            let urls: Vec<String> =
                                spans.iter().map(|r| t[r.clone()].to_string()).collect();
                            let hl: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = spans
                                .iter()
                                .map(|r| {
                                    (
                                        r.clone(),
                                        gpui::HighlightStyle {
                                            underline: Some(gpui::UnderlineStyle {
                                                thickness: px(1.),
                                                color: Some(rgb(SEL_TEXT).into()),
                                                wavy: false,
                                            }),
                                            ..Default::default()
                                        },
                                    )
                                })
                                .collect();
                            let styled = StyledText::new(t.clone()).with_highlights(hl);
                            gpui::InteractiveText::new(("you-link", ix), styled)
                                .on_click(spans, move |i, _w, _cx| {
                                    if let Some(u) = urls.get(i) {
                                        open_browser(u);
                                    }
                                })
                                .into_any_element()
                        }
                    })
                    // what went out with it: the same chips the composer showed
                    // while you were writing, so `[Image #1]` still has a picture
                    // attached to it after the fact
                    .when(!imgs.is_empty(), |d| {
                        let mut row = div().mt_1p5().flex().flex_row().flex_wrap().gap_2();
                        for (n, pi) in imgs.iter().enumerate() {
                            let kind = pi.mime.rsplit('/').next().unwrap_or("image").to_uppercase();
                            let kb = (pi.bytes as f32 / 1024.0).round() as usize;
                            let preview = pi.image.clone();
                            row = row.child(
                                div()
                                    .id(("you-img", ix * 100 + n))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .pl_0p5()
                                    .pr_1()
                                    .rounded_md()
                                    .bg(rgb(CURRENT_LINE))
                                    .border_1()
                                    .border_color(rgb(BORDER))
                                    .text_size(px(10.))
                                    .text_color(rgb(MUTED))
                                    .child(
                                        gpui::img(pi.image.clone())
                                            .h(px(88.))
                                            .max_w(px(160.))
                                            .rounded_sm(),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .text_color(rgb(COMMENT_MARK))
                                                    .child(pi.label.clone()),
                                            )
                                            .child(format!("{kind}  ·  {kb} KB")),
                                    )
                                    .tooltip(move |_w, cx| {
                                        cx.new(|_| ImagePreview { image: preview.clone() }).into()
                                    }),
                            );
                        }
                        d.child(row)
                    }),
            )
            .into_any_element(),
        // the model answers in markdown — headings, `code`, and bullets included
        AgentMsg::Agent(t) => md_doc(t).into_any_element(),
        AgentMsg::Thought(t) => div()
            .pl_2()
            .border_l_2()
            .border_color(rgb(BORDER))
            .child(md_text(t, 12.0, MUTED))
            .into_any_element(),
        AgentMsg::Tool { title, status, detail, started, .. } => {
            let running = matches!(status.as_str(), "pending" | "in_progress");
            let (glyph, color) = match status.as_str() {
                "completed" => (IC_CHECK, GIT_NEW),
                "failed" | "cancelled" => ("✕", GIT_DELETED),
                // a running call is the one thing here worth looking at
                _ => ("●", ACCENT),
            };
            // for a Bash call the title *is* the command, so it can be long and
            // can carry newlines: the row shows the first line, the tooltip the
            // whole of it
            let mut head = title.lines().next().unwrap_or_default().to_string();
            if title.lines().count() > 1 {
                head.push_str(" …");
            }
            let secs = started.elapsed().as_secs();
            let elapsed = if secs >= 60 {
                format!("{}m {}s", secs / 60, secs % 60)
            } else {
                format!("{secs}s")
            };
            // while it runs, `detail` says what the call is for; when it failed,
            // it's the error the model saw. On a call that worked it's noise.
            let note = detail
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(2)
                .collect::<Vec<_>>()
                .join("  ")
                .trim()
                .to_string();
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .px_2()
                .py_1()
                .rounded_md()
                .bg(rgb(BG))
                .border_1()
                .border_color(rgb(if running { ACCENT } else { BORDER }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .font_family(ICON_FONT)
                                .text_size(px(11.))
                                .text_color(rgb(color))
                                .flex_shrink_0()
                                .child(glyph),
                        )
                        .child(
                            div()
                                .id(("agent-tool", ix))
                                .flex_grow(1.0)
                                .min_w(px(0.))
                                .truncate()
                                .font_family("Menlo")
                                .text_size(px(11.5))
                                .text_color(rgb(if running { TEXT } else { MUTED }))
                                .child(head)
                                .tooltip({
                                    let full = title.clone();
                                    move |_w, cx| {
                                        cx.new(|_| TooltipView { text: full.clone().into() }).into()
                                    }
                                }),
                        )
                        // how long it has been going: the whole point of the row
                        // when a command takes minutes
                        .when(running, |d| {
                            d.child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(11.))
                                    .text_color(rgb(LINE_NUMBER))
                                    .child(elapsed),
                            )
                        }),
                )
                .when(!note.is_empty() && (running || status == "failed"), |d| {
                    d.child(
                        div()
                            .pl(px(19.))
                            .text_size(px(11.))
                            .text_color(rgb(if status == "failed" { GIT_DELETED } else { MUTED }))
                            .child(note),
                    )
                })
                .into_any_element()
        }
    }
}

/// One visible row in the Git panel's branch tree.
struct GitBranchRow {
    depth: usize,
    label: String,
    branch: Option<String>, // Some → a leaf branch (its full name, e.g. "adrian/foo")
    folder: Option<String>, // Some → a folder row (its path key), toggles collapse
}

// ── Git log panel ────────────────────────────────────────────────────────────
impl Storm {
    fn act_git_log(&mut self, _: &GitLog, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_git_log(window, cx);
    }

    fn toggle_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.git_open = !self.git_open;
        if self.git_open {
            self.run_open = false; // Git + Run console share the bottom; only one at a time
            self.load_git_log(cx);
            window.focus(&self.git_focus, cx);
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// Toggle the Run console; it and the Git panel are mutually exclusive
    /// (both are bottom docks).
    fn toggle_run(&mut self, cx: &mut Context<Self>) {
        self.run_open = !self.run_open;
        if self.run_open {
            self.git_open = false;
        }
        cx.notify();
    }

    /// (Re)load the branch list + commit log for the current rev, off-thread.
    fn load_git_log(&mut self, cx: &mut Context<Self>) {
        self.git_gen += 1;
        let gen = self.git_gen;
        let root = self.root.clone();
        let rev = self.git_rev.clone();
        cx.spawn(async move |this, cx| {
            let (branches, commits) = cx
                .background_executor()
                .spawn(async move { (git_branches(&root), git_log_commits(&root, &rev, 500)) })
                .await;
            this.update(cx, |this, cx| {
                if this.git_gen != gen {
                    return;
                }
                this.git_branches_list = branches;
                this.git_commits = commits;
                this.git_sel = None;
                this.git_details_msg.clear();
                this.git_details_files.clear();
                if !this.git_commits.is_empty() {
                    this.select_commit(0, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Show a different branch/rev in the log (branch-tree click).
    fn git_show_rev(&mut self, rev: String, cx: &mut Context<Self>) {
        self.git_rev = rev;
        self.load_git_log(cx);
    }

    /// Select commit `idx` and load its message + changed files off-thread.
    fn select_commit(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(c) = self.git_commits.get(idx) else { return };
        self.git_sel = Some(idx);
        let hash = c.hash.clone();
        let root = self.root.clone();
        let gen = self.git_gen;
        cx.spawn(async move |this, cx| {
            let (msg, files) = cx
                .background_executor()
                .spawn(async move { (git_commit_message(&root, &hash), git_commit_files(&root, &hash)) })
                .await;
            this.update(cx, |this, cx| {
                if this.git_gen == gen {
                    this.git_details_msg = msg;
                    this.git_details_files = files;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Indices into `git_commits` matching the filter (subject/hash/author).
    fn git_visible(&self) -> Vec<usize> {
        let q = self.git_query.text.trim().to_lowercase();
        (0..self.git_commits.len())
            .filter(|&i| {
                if q.is_empty() {
                    return true;
                }
                let c = &self.git_commits[i];
                c.subject.to_lowercase().contains(&q)
                    || c.short.to_lowercase().contains(&q)
                    || c.hash.to_lowercase().contains(&q)
                    || c.author.to_lowercase().contains(&q)
            })
            .collect()
    }

    /// Flatten branch names into a collapsible tree (split on '/').
    fn git_branch_rows(&self) -> Vec<GitBranchRow> {
        #[derive(Default)]
        struct Node {
            dirs: std::collections::BTreeMap<String, Node>,
            leaves: std::collections::BTreeMap<String, String>, // label → full branch name
        }
        let mut root = Node::default();
        for name in &self.git_branches_list {
            let comps: Vec<&str> = name.split('/').collect();
            let mut node = &mut root;
            for c in &comps[..comps.len().saturating_sub(1)] {
                node = node.dirs.entry(c.to_string()).or_default();
            }
            if let Some(leaf) = comps.last() {
                node.leaves.insert(leaf.to_string(), name.clone());
            }
        }
        let mut out = Vec::new();
        fn walk(node: &Node, depth: usize, prefix: &str, collapsed: &HashSet<String>, out: &mut Vec<GitBranchRow>) {
            for (name, child) in &node.dirs {
                let key = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                out.push(GitBranchRow { depth, label: name.clone(), branch: None, folder: Some(key.clone()) });
                if !collapsed.contains(&key) {
                    walk(child, depth + 1, &key, collapsed, out);
                }
            }
            for (label, full) in &node.leaves {
                out.push(GitBranchRow { depth, label: label.clone(), branch: Some(full.clone()), folder: None });
            }
        }
        walk(&root, 1, "", &self.git_collapsed, &mut out);
        out
    }

    /// Open the diff of `path` at the selected commit (commit^ vs commit).
    fn open_commit_diff(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(sel) = self.git_sel else { return };
        let Some(c) = self.git_commits.get(sel) else { return };
        let files: Vec<PathBuf> = self.git_details_files.iter().map(|(p, _)| p.clone()).collect();
        let Some(idx) = files.iter().position(|p| p == &path) else { return };
        let old = Some(format!("{}^", c.hash));
        let new_rev = Some(c.hash.clone());
        self.open_diff_window(files, idx, old, new_rev, false, String::new(), PrViewFilter::All, cx);
    }

    fn git_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // the filter input is nested in the same focus scope; ignore its keys here
        if self.git_filter_focus.is_focused(window) {
            return;
        }
        let key = ev.keystroke.key.as_str();
        let vis = self.git_visible();
        let cur = self.git_sel.and_then(|s| vis.iter().position(|&i| i == s)).unwrap_or(0);
        match key {
            "escape" => {
                self.git_open = false;
                self.focus_active(window, cx);
                cx.notify();
            }
            "down" => {
                if let Some(&i) = vis.get((cur + 1).min(vis.len().saturating_sub(1))) {
                    self.select_commit(i, cx);
                    cx.notify();
                }
            }
            "up" => {
                if let Some(&i) = vis.get(cur.saturating_sub(1)) {
                    self.select_commit(i, cx);
                    cx.notify();
                }
            }
            _ => {}
        }
    }

    fn git_filter_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key == "escape" {
            self.git_query.clear();
        } else {
            Self::field_input(&mut self.git_query, &ev.keystroke, cx, |_| true);
        }
        cx.notify();
    }

    fn render_git_log(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(ICON)).child(IC_BRANCH))
            .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child("Git"))
            .child(div().flex_1())
            .child(
                div()
                    .id("git-close")
                    .px_1()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(TEXT)))
                    .child("✕")
                    .on_click(cx.listener(|this, _e, window, cx| this.toggle_git_log(window, cx))),
            );

        // left: branch tree
        let cur_branch = self.branch.clone();
        let mut branches = div()
            .id("git-branches")
            .w(px(240.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .py_1()
            .child(
                // HEAD (current branch) — clicking shows HEAD
                div()
                    .id("git-head")
                    .px_2()
                    .py_1()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .when(self.git_rev == "HEAD", |d| d.bg(rgb(SELECTED_BG)))
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(div().font_family(ICON_FONT).text_size(px(11.)).text_color(rgb(ACCENT)).child(IC_HOME))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(TEXT))
                            .child(format!("HEAD  ·  {}", if cur_branch.is_empty() { "detached".into() } else { cur_branch.clone() })),
                    )
                    .on_click(cx.listener(|this, _e, _w, cx| this.git_show_rev("HEAD".into(), cx))),
            );
        for row in self.git_branch_rows() {
            let indent = 8.0 + row.depth as f32 * 12.0;
            if let Some(folder) = row.folder.clone() {
                let collapsed = self.git_collapsed.contains(&folder);
                branches = branches.child(
                    div()
                        .id(SharedString::from(format!("git-fold-{folder}")))
                        .pl(px(indent))
                        .pr_2()
                        .py(px(2.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(HOVER)))
                        .child(div().text_size(px(10.)).text_color(rgb(MUTED)).child(if collapsed { "▸" } else { "▾" }))
                        .child(div().font_family(ICON_FONT).text_size(px(11.)).text_color(rgb(FOLDER_ICON)).child(IC_FOLDER))
                        .child(div().text_size(px(12.)).text_color(rgb(DIR)).child(row.label.clone()))
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            if !this.git_collapsed.remove(&folder) {
                                this.git_collapsed.insert(folder.clone());
                            }
                            cx.notify();
                        })),
                );
            } else if let Some(branch) = row.branch.clone() {
                let sel = self.git_rev == branch;
                let is_cur = branch == cur_branch;
                branches = branches.child(
                    div()
                        .id(SharedString::from(format!("git-br-{branch}")))
                        .pl(px(indent + 12.0))
                        .pr_2()
                        .py(px(2.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                        .hover(|s| s.bg(rgb(HOVER)))
                        .child(div().font_family(ICON_FONT).text_size(px(10.)).text_color(rgb(if is_cur { ACCENT } else { MUTED })).child(IC_BRANCH))
                        .child(
                            div()
                                .text_size(px(12.))
                                .truncate()
                                .text_color(rgb(if is_cur { ACCENT } else { TEXT }))
                                .child(row.label.clone()),
                        )
                        .on_click(cx.listener(move |this, _e, _w, cx| this.git_show_rev(branch.clone(), cx))),
                );
            }
        }

        // center: filter bar + commit list
        let vis = self.git_visible();
        let filter_bar = div()
            .h(px(34.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex_grow(1.0)
                    .min_w(px(0.))
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(if self.git_filter_focus.is_focused(window) { ACCENT } else { BORDER }))
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .track_focus(&self.git_filter_focus)
                    .on_key_down(cx.listener(Self::git_filter_key))
                    .child(if self.git_query.text.is_empty() {
                        div().text_color(rgb(MUTED)).child("Text or hash").into_any_element()
                    } else {
                        self.git_query.render(self.caret(), SELECTION)
                    }),
            )
            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(format!("⎇ {}", self.git_rev)))
            .child(
                div()
                    .id("git-reload")
                    .px_2()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(TEXT)))
                    .child("↻")
                    .tooltip(tip("Reload"))
                    .on_click(cx.listener(|this, _e, _w, cx| this.load_git_log(cx))),
            );

        let mut list = div()
            .id("git-commits")
            .flex_grow(1.0)
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.git_scroll)
            .flex()
            .flex_col();
        if self.git_commits.is_empty() {
            list = list.child(div().px_3().py_2().text_size(px(12.)).text_color(rgb(MUTED)).child("No commits"));
        }
        for &i in &vis {
            let c = &self.git_commits[i];
            let sel = self.git_sel == Some(i);
            list = list.child(
                div()
                    .id(("git-c", i))
                    .h(px(24.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .cursor_pointer()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                    .child(div().flex_shrink_0().text_size(px(9.)).text_color(rgb(ACCENT)).child("●"))
                    .child(
                        div()
                            .flex_grow(1.0)
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(12.))
                            .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                            .child(c.subject.clone()),
                    )
                    .child(div().flex_shrink_0().w(px(120.)).truncate().text_size(px(11.)).text_color(rgb(MUTED)).child(c.author.clone()))
                    .child(div().flex_shrink_0().text_size(px(11.)).text_color(rgb(MUTED)).child(c.date.clone()))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.select_commit(i, cx);
                        cx.notify();
                    })),
            );
        }
        let center = div().flex_grow(1.0).min_w(px(0.)).h_full().flex().flex_col().child(filter_bar).child(list);

        // right: commit details
        let details = self.render_git_details(cx);

        div()
            .w_full()
            .h(px(self.git_height))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .border_t_1()
            .border_color(rgb(BORDER))
            .track_focus(&self.git_focus)
            .on_key_down(cx.listener(Self::git_key))
            .child(header)
            .child(div().flex_grow(1.0).min_h(px(0.)).flex().flex_row().child(branches).child(center).child(details))
    }

    fn render_git_details(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut panel = div()
            .id("git-details")
            .w(px(360.))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            .p_3()
            .gap_2();
        let Some(sel) = self.git_sel else {
            return panel.child(div().text_size(px(12.)).text_color(rgb(MUTED)).child("Select commit to view changes"));
        };
        let Some(c) = self.git_commits.get(sel) else {
            return panel.child(div());
        };
        panel = panel
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(11.)).text_color(rgb(ACCENT)).child(c.short.clone()))
                    .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(format!("{}  ·  {}", c.author, c.date))),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .child(if self.git_details_msg.is_empty() { c.subject.clone() } else { self.git_details_msg.clone() }),
            )
            .child(div().h(px(1.)).bg(rgb(BORDER)))
            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(format!("{} file(s) changed", self.git_details_files.len())));
        for (path, state) in &self.git_details_files {
            let rel = self.rel(path);
            let color = match state {
                GitState::New => GIT_NEW,
                GitState::Deleted => GIT_DELETED,
                _ => GIT_MODIFIED,
            };
            let p = path.clone();
            panel = panel.child(
                div()
                    .id(SharedString::from(format!("git-df-{}", rel)))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .py(px(1.))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .child(div().w(px(6.)).flex_shrink_0().text_size(px(11.)).text_color(rgb(color)).child("●"))
                    .child(div().min_w(px(0.)).truncate().text_size(px(12.)).text_color(rgb(TEXT)).child(rel))
                    .on_click(cx.listener(move |this, _e, _w, cx| this.open_commit_diff(p.clone(), cx))),
            );
        }
        panel
    }
}

// ── in-pane browser dock ─────────────────────────────────────────────────────
impl Storm {
    /// The right-dock browser: an address bar + nav controls (gpui), with the
    /// embedded WebView floating over the region below the bar. Computes that
    /// region's rect from the current layout and (re)positions the WebView.
    fn render_browser_dock(&mut self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let addr_h = 36.0_f32;
        let dock_w = self.term_width;
        // column sits just left of the 44px right activity bar
        let region_x = self.win_width - 44.0 - dock_w;
        let region_y = 44.0 + addr_h; // below topbar + address bar
        let bottom = (if self.run_open { 260.0 } else { 0.0 })
            + (if self.git_open { self.git_height + 4.0 } else { 0.0 });
        let region_h = (self.win_height - region_y - bottom).max(0.0);
        let region_w = dock_w;

        // create the WebView on first open; afterwards just reposition + show it
        if self.browser.is_none() {
            let url = normalize_url(self.browser_url.text.trim());
            self.browser = browser::Browser::new(window, &url, region_x, region_y, region_w, region_h);
        } else if let Some(b) = &self.browser {
            b.set_bounds(region_x, region_y, region_w, region_h);
            b.set_visible(true);
        }

        let nav = |id: &'static str, glyph: &'static str, enabled: bool| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .size(px(22.))
                .rounded_md()
                .text_size(px(13.))
                .text_color(rgb(if enabled { MUTED } else { LINE_NUMBER }))
                .when(enabled, |d| d.cursor_pointer().hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT))))
                .child(glyph)
        };

        let focused = self.browser_focus.is_focused(window);
        let bar = div()
            .h(px(addr_h))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_2()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(nav("br-back", "‹", true).on_click(cx.listener(|this, _e, _w, _cx| {
                if let Some(b) = &this.browser { b.back(); }
            })))
            .child(nav("br-fwd", "›", true).on_click(cx.listener(|this, _e, _w, _cx| {
                if let Some(b) = &this.browser { b.forward(); }
            })))
            .child(nav("br-reload", "↻", true).on_click(cx.listener(|this, _e, _w, _cx| {
                if let Some(b) = &this.browser { b.reload(); }
            })))
            .child(
                div()
                    .flex_grow(1.0)
                    .min_w(px(0.))
                    .px_2()
                    .py(px(3.))
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(if focused { ACCENT } else { BORDER }))
                    .rounded_md()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .track_focus(&self.browser_focus)
                    .on_key_down(cx.listener(Self::browser_key))
                    .child(self.browser_url.render(self.caret(), SELECTION)),
            )
            .child(nav("br-ext", "⧉", true).on_click(cx.listener(|this, _e, _w, _cx| {
                open_browser(&normalize_url(this.browser_url.text.trim()));
            })))
            .child(nav("br-close", "✕", true).on_click(cx.listener(|this, _e, window, cx| {
                this.toggle_browser(window, cx);
            })));

        // the region below the bar is left empty — the native WebView floats over it
        div()
            .w(px(dock_w))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .child(bar)
            .child(div().flex_grow(1.0).min_h(px(0.)))
    }
}

/// The Webiny admin URL for a `wby-next{N}` project (`https://admin{N}.localhost`),
/// or None for any other project.
fn admin_url(root: &Path) -> Option<String> {
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let n = name.strip_prefix("wby-next")?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| format!("https://admin{n}.localhost"))
}

/// Is the admin URL reachable? Any HTTP response (even 4xx/5xx) counts as up;
/// a connection failure ("000") is down. `-k` since local certs are self-signed.
fn admin_reachable(url: &str) -> bool {
    Command::new("curl")
        .args(["-sk", "--max-time", "3", "-o", "/dev/null", "-w", "%{http_code}", url])
        .output()
        .map(|o| {
            let code = String::from_utf8_lossy(&o.stdout).trim().parse::<u32>().unwrap_or(0);
            // Only a 2xx/3xx means the app is actually serving. The portless proxy
            // always answers on these hosts, with its own 404 ("No app registered
            // for adminN.localhost") when nothing is running and 502 when the
            // target crashed — both are "down", so any-response-is-up was wrong.
            (200..400).contains(&code)
        })
        .unwrap_or(false)
}

/// Default browser URL for a project. `wby-next{N}` → `https://admin{N}.localhost`
/// (Webiny multi-workspace convention); everything else → localhost:3000.
fn default_browser_url(root: &Path) -> String {
    let name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(n) = name.strip_prefix("wby-next") {
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) {
            return format!("https://admin{n}.localhost");
        }
    }
    "http://localhost:3000".into()
}

/// Add a scheme to a bare address-bar entry. Loopback/port-only hosts default
/// to http; everything else to https.
fn normalize_url(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return "about:blank".into();
    }
    if s.contains("://") {
        return s.to_string();
    }
    let host = s.split(['/', ':']).next().unwrap_or(s);
    if host == "localhost" || host == "127.0.0.1" || host.parse::<std::net::Ipv4Addr>().is_ok() {
        format!("http://{s}")
    } else {
        format!("https://{s}")
    }
}

/// Open `url` in the user's default browser.
fn open_browser(url: &str) {
    // open in the user's default browser (respects Brave/Chrome/etc. preference)
    let _ = Command::new("open").arg(url).spawn();
}

/// A vertical-activity-bar icon button.
fn activity_icon(
    id: &'static str,
    glyph: &'static str,
    tooltip: &'static str,
    active: bool,
    badge: usize,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .relative()
        .w(px(34.))
        .h(px(34.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .font_family(ICON_FONT)
        .text_size(px(18.))
        .text_color(rgb(ICON)) // same glyph color resting + active
        .when(active, |d| d.bg(rgb(ICON_SELECTED_BG)))
        .when(!active, |d| d.hover(|s| s.bg(rgb(HOVER))))
        .cursor_pointer()
        .child(glyph)
        // count badge (e.g. uncommitted files) in the top-right corner
        .when(badge > 0, |d| {
            let label = if badge > 99 { "99+".to_string() } else { badge.to_string() };
            d.child(
                div()
                    .absolute()
                    .top(px(-2.))
                    .right(px(-2.))
                    .min_w(px(15.))
                    .h(px(15.))
                    .px(px(3.))
                    .rounded_full()
                    .bg(rgb(ACCENT))
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family("Inter") // digits, not the icon font
                    .text_size(px(9.))
                    .text_color(rgb(SEL_TEXT))
                    .child(label),
            )
        })
        .tooltip(tip(tooltip))
        .on_click(on_click)
}

impl Storm {
    fn render_goto(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 320.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        div()
            .absolute()
            .top(px(120.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.goto_focus)
            .on_key_down(cx.listener(Self::goto_key))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("Go to line  [line] or [line:column]"),
            )
            .child(
                div()
                    .mx_3()
                    .mb_3()
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .text_color(rgb(TEXT))
                    .child(self.goto_query.render(self.caret(), SELECTION)),
            )
    }

    fn render_finder(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let w = 680.0_f32;
        let left = ((self.win_width - w) / 2.0).max(0.);
        let root = self.root.clone();

        let mut panel = div()
            .absolute()
            .top(px(64.))
            .left(px(left))
            .w(px(w))
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .track_focus(&self.finder_focus)
            .on_key_down(cx.listener(Self::finder_key));

        // search row
        panel = panel.child(
            div()
                .h(px(40.))
                .px_3()
                .flex()
                .items_center()
                .border_b_1()
                .border_color(rgb(BORDER))
                .text_color(rgb(TEXT))
                .child(if self.finder_query.is_empty() {
                    div().child(format!("  Search files…{}", self.caret())).into_any_element()
                } else {
                    div()
                        .flex()
                        .flex_row()
                        .child("  ")
                        .child(self.finder_query.render(self.caret(), SELECTION))
                        .into_any_element()
                }),
        );

        // results — capped height + scrollable so the panel never grows past ~600px
        let mut results = div()
            .id("finder-results")
            .flex()
            .flex_col()
            .max_h(px(560.))
            .overflow_y_scroll();

        let dirs_mode = self.finder_dirs_mode;
        for (i, p) in self.finder_results.iter().enumerate() {
            let selected = i == self.finder_selected;
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let dir = p
                .parent()
                .and_then(|d| d.strip_prefix(&root).ok())
                .map(|d| d.to_string_lossy().to_string())
                .unwrap_or_default();
            let path = p.clone();
            // folder icon in dirs mode, ext badge otherwise
            let (badge, badge_color, badge_size) =
                if dirs_mode { (IC_FOLDER.to_string(), FOLDER_ICON, 13.) } else { let (b, c) = ext_badge(p); (b, c, 10.) };

            results = results.child(
                div()
                    .id(("finder", i))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(26.))
                    .px_3()
                    .when(selected, |d| d.bg(rgb(SELECTED_BG)))
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(20.))
                            .flex()
                            .justify_center()
                            .text_size(px(badge_size))
                            .when(dirs_mode, |d| d.font_family(ICON_FONT))
                            .text_color(rgb(if selected { SEL_TEXT } else { badge_color }))
                            .child(badge),
                    )
                    .child(
                        div()
                            .text_color(rgb(if selected { SEL_TEXT } else if dirs_mode { DIR } else { TEXT }))
                            .child(if dirs_mode { format!("{}/", name) } else { name }),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(if selected { SEL_TEXT } else { MUTED }))
                            .child(dir),
                    )
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.open_finder_result(path.clone(), window, cx);
                    })),
            );
        }

        panel.child(results)
    }
}

impl Storm {
    fn render_divider(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(4.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(if self.resizing { ACCENT } else { BORDER }))
            .cursor(CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _window, cx| {
                    this.resizing = true;
                    cx.notify();
                }),
            )
    }

    /// Small "collapse" button for a left-panel header — hides the whole left
    /// sidebar (same as toggling its activity-bar icon off).
    /// Header button that copies all given file paths (repo-relative, one per
    /// line) to the clipboard — handy to paste a changed-file list into an LLM.
    fn copy_paths_btn(&self, id: &'static str, paths: Vec<PathBuf>, cx: &mut Context<Self>) -> impl IntoElement {
        let n = paths.len();
        div()
            .id(id)
            .px_1()
            .flex_shrink_0()
            .font_family(ICON_FONT)
            .text_size(px(12.))
            .text_color(rgb(MUTED))
            .hover(|s| s.text_color(rgb(TEXT)))
            .cursor_pointer()
            .child(IC_COPY)
            .tooltip(tip("Copy all changed paths"))
            .on_click(cx.listener(move |this, _e, _w, cx| {
                let list = paths.iter().map(|p| this.rel(p)).collect::<Vec<_>>().join("\n");
                cx.write_to_clipboard(ClipboardItem::new_string(list));
                this.show_flash(&format!("Copied {n} paths"), cx);
            }))
    }

    /// "collapse all" / "expand all" for a file tree pane.
    fn tree_fold_btns(
        &self,
        id_collapse: &'static str,
        id_expand: &'static str,
        collapse: impl Fn(&mut Self, bool, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expand = collapse.clone();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id(id_collapse)
                    .px_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                    .child("⇤")
                    .tooltip(tip("Collapse all"))
                    .on_click(cx.listener(move |this, _e, _w, cx| collapse(this, true, cx))),
            )
            .child(
                div()
                    .id(id_expand)
                    .px_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                    .child("⇥")
                    .tooltip(tip("Expand all"))
                    .on_click(cx.listener(move |this, _e, _w, cx| expand(this, false, cx))),
            )
    }

    fn collapse_left_btn(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("panel-collapse")
            .px_1()
            .flex_shrink_0()
            .cursor_pointer()
            .text_size(px(15.))
            .text_color(rgb(MUTED))
            .hover(|s| s.text_color(rgb(TEXT)))
            .child("–")
            .tooltip(|_w, cx| cx.new(|_| TooltipView { text: "Collapse panel".into() }).into())
            .on_click(cx.listener(|this, _e, window, cx| {
                this.show_left = false;
                this.focus_active(window, cx);
                cx.notify();
            }))
    }

    fn render_tree(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.entries.len();
        let width = px(self.tree_width);

        div()
            .flex()
            .flex_col()
            .w(width)
            .h_full()
            .bg(rgb(BG))
            .track_focus(&self.tree_focus)
            .key_context("Tree")
            .on_key_down(cx.listener(Self::tree_key))
            .child(
                div()
                    .h(px(32.))
                    .px_3()
                    .flex()
                    .items_center()
                    .text_color(rgb(ACCENT))
                    .text_size(px(12.))
                    .child("PROJECT")
                    .child(div().flex_grow(1.0))
                    .child(self.collapse_left_btn(cx)),
            )
            .child(
                uniform_list(
                    "tree",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                        range
                            .map(|ix| {
                                let entry = &this.entries[ix];
                                let indent = (entry.depth as f32) * 14.;
                                let is_dir = entry.is_dir;
                                let expanded = this.expanded.contains(&entry.path);
                                let is_open = this.is_tree_selected(&entry.path);
                                let git = this.git_status.get(&entry.path).copied();
                                let name = entry.name.clone();
                                let dimmed = entry.ignored; // git-ignored → faded
                                let comments = if is_dir { 0 } else { this.comment_count(&entry.path) };
                                let ctx_path = entry.path.clone(); // for the right-click menu
                                let fg = if is_open {
                                    rgb(SEL_TEXT)
                                } else if is_dir {
                                    rgb(DIR)
                                } else {
                                    match git {
                                        Some(GitState::New) => rgb(GIT_NEW),
                                        Some(GitState::Modified) => rgb(GIT_MODIFIED),
                                        Some(GitState::Deleted) => rgb(GIT_DELETED),
                                        None => rgb(TEXT),
                                    }
                                };
                                // disclosure arrow (dirs only) in its own column
                                let chevron = if is_dir {
                                    if expanded { "▾" } else { "▸" }
                                } else {
                                    ""
                                };
                                let chevron_color = if is_open { SEL_TEXT } else { DIR };
                                // icon column: folder glyph for dirs, ext badge for files
                                let (icon, icon_color, icon_is_font, icon_size) = if is_dir {
                                    (IC_FOLDER.to_string(), if is_open { SEL_TEXT } else { FOLDER_ICON }, true, 13.)
                                } else {
                                    let (b, c) = ext_badge(&entry.path);
                                    (b, if is_open { SEL_TEXT } else { c }, false, 9.)
                                };

                                div()
                                    .id(ix)
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .h(px(22.))
                                    .w_full()
                                    .pl(px(8. + indent))
                                    .pr_2()
                                    .when(is_open, |d| d.bg(rgb(SELECTED_BG)))
                                    .when(!is_open, |d| d.hover(|s| s.bg(rgb(HOVER))))
                                    .when(dimmed, |d| d.opacity(0.5)) // git-ignored → faded
                                    .cursor_pointer()
                                    .child(
                                        div()
                                            .w(px(12.))
                                            .flex()
                                            .justify_center()
                                            .text_size(px(11.))
                                            .text_color(rgb(chevron_color))
                                            .child(chevron),
                                    )
                                    .child(
                                        div()
                                            .w(px(16.))
                                            .flex()
                                            .justify_center()
                                            .text_size(px(icon_size))
                                            .when(icon_is_font, |d| d.font_family(ICON_FONT))
                                            .text_color(rgb(icon_color))
                                            .child(icon),
                                    )
                                    .child(div().flex_grow(1.0).truncate().text_color(fg).child(name))
                                    // review comments pinned in this file
                                    .when(comments > 0, |d| d.child(comment_marker(comments, 11.)))
                                    .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
                                        window.focus(&this.tree_focus, cx); // route cmd+c/v to the tree
                                        if ev.click_count() >= 2 {
                                            this.on_entry(ix, window, cx);
                                        } else {
                                            let m = ev.modifiers();
                                            this.select_entry(ix, m.shift, m.platform, cx);
                                        }
                                    }))
                                    // right-click → context menu (Refresh, …)
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                            this.tree_ctx = Some((f32::from(ev.position.x), f32::from(ev.position.y)));
                                            this.tree_ctx_path = Some(ctx_path.clone());
                                            window.focus(&this.tree_ctx_focus, cx);
                                            cx.notify();
                                        }),
                                    )
                            })
                            .collect()
                    }),
                )
                .track_scroll(&self.tree_scroll)
                .flex_grow(1.0),
            )
    }

    fn render_editor(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_tabs = !self.tabs.is_empty();

        let mut col = div().flex().flex_col().flex_grow(1.0).h_full();

        // tab bar
        col = col.child(self.render_tabs(cx));

        if has_tabs {
            // breadcrumb bar above the file (Zed-style): rel path + copy button
            let path = self.tabs[self.active].path.clone();
            let scratch = self.tabs[self.active].scratch;
            let rel = if scratch {
                path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
            } else {
                self.rel(&path)
            };
            let path_click = path.clone();
            let crumb = div()
                .h(px(34.))
                .flex_shrink_0()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .bg(rgb(BG))
                .border_b_1()
                .border_color(rgb(BORDER))
                .text_size(px(14.))
                // path sizes to content so the copy button sits right after it;
                // clicking the path (not just the icon) copies it too
                .child(
                    div()
                        .id("crumb-path")
                        .min_w(px(0.))
                        .truncate()
                        .cursor_pointer()
                        .text_color(rgb(MUTED))
                        .hover(|s| s.text_color(rgb(TEXT)))
                        .child(rel.clone())
                        .when(!scratch, |d| {
                            d.on_click(cx.listener(move |this, _e, _w, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(this.rel(&path_click)));
                                this.show_flash("Path copied", cx);
                            }))
                        }),
                )
                .when(!scratch, |d| {
                    d.child(
                        div()
                            .id("crumb-copy")
                            .flex_shrink_0()
                            .font_family(ICON_FONT)
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.text_color(rgb(TEXT)))
                            .cursor_pointer()
                            .child(IC_COPY)
                            .tooltip(tip("Copy path"))
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(this.rel(&path)));
                                this.show_flash("Path copied", cx);
                            })),
                    )
                })
                .child(div().flex_grow(1.0)) // spacer keeps path + copy left-aligned
                // markdown: raw source or rendered, per tab
                .when(is_markdown(&self.tabs[self.active].path), |d| {
                    let preview = self.tabs[self.active].preview;
                    d.child(
                        div()
                            .id("crumb-md")
                            .flex_shrink_0()
                            .px_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(11.))
                            .border_1()
                            .border_color(rgb(if preview { COMMENT_MARK } else { BORDER }))
                            .text_color(rgb(if preview { COMMENT_MARK } else { MUTED }))
                            .hover(|s| s.bg(rgb(HOVER)))
                            .child(if preview { "Rendered" } else { "Raw" })
                            .tooltip(tip("Markdown: rendered / raw  (⌘⇧V)"))
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.toggle_md_preview(window, cx)
                            })),
                    )
                });
            col = col.child(crumb);
            let tab = &self.tabs[self.active];
            if tab.preview && is_markdown(&tab.path) {
                // rendered view reads the buffer, so unsaved edits show up too
                let body = tab.editor.read(cx).text();
                col = col.child(
                    div()
                        .id("md-preview")
                        .flex_grow(1.0)
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .bg(rgb(BG))
                        .child(md_doc(&body).px_4().py_3()),
                );
            } else {
                let editor = tab.editor.clone();
                col = col.child(div().flex_grow(1.0).min_h(px(0.)).child(editor));
            }
        } else {
            col = col.child(self.render_status(cx));
        }

        col
    }

    /// Empty-pane dashboard: a snapshot of the current work — branch, PR (with
    /// target + status, click to open), uncommitted count, and a refresh button.
    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.project_name();
        let branch = self.branch_label();
        let changed = self.git_status.len();

        // a labeled row: leading icon glyph + content
        let row = |icon: &'static str, icon_color: u32| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(div().w(px(16.)).flex_shrink_0().font_family(ICON_FONT).text_size(px(13.)).text_color(rgb(icon_color)).child(icon))
        };

        let mut card = div()
            .w(px(460.))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(18.))
                                    .text_color(rgb(TEXT))
                                    .child(project),
                            )
                            // grayed folder path — a custom title otherwise hides it
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(11.))
                                    .text_color(rgb(LINE_NUMBER))
                                    .child(tilde_path(&self.root)),
                            ),
                    )
                    .child(
                        div()
                            .id("status-refresh")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(rgb(MUTED))
                            .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                            .child("↻ Refresh")
                            .tooltip(tip("Refresh branch + PR status"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.refresh_pr_link(cx);
                                this.check_admin_now(cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(div().h(px(1.)).bg(rgb(BORDER)))
            // current branch
            .child(
                row(IC_BRANCH, ACCENT)
                    .child(div().text_size(px(13.)).text_color(rgb(MUTED)).child("On branch"))
                    .child(branch_name_el(&branch).text_size(px(13.))),
            );

        // pull request
        if let Some((num, url, status, base, _head, state)) = self.pr_link.clone() {
            let (status_text, status_color) = match status {
                PrStatus::Passing => ("checks passing", status.color()),
                PrStatus::Pending => ("checks running", status.color()),
                PrStatus::Failing => ("checks failing", status.color()),
                PrStatus::None => ("no checks", MUTED),
            };
            let open_url = url.clone();
            card = card.child(
                row(IC_PR, GIT_NEW)
                    .child(
                        div()
                            .id("status-pr")
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(rgb(ACCENT))
                            .hover(|s| s.underline())
                            .child(format!("PR #{num}"))
                            .tooltip(tip("Open on GitHub"))
                            .on_click(cx.listener(move |_t, _e, _w, _cx| {
                                let _ = Command::new("open").arg(&open_url).spawn();
                            })),
                    )
                    // lifecycle state: draft / open / merged / closed
                    .child(pr_state_pill(state, 11.))
                    .child(div().text_size(px(13.)).text_color(rgb(MUTED)).child("→"))
                    .child(branch_name_el(&base).text_size(px(13.)))
                    .child(div().w(px(6.)).flex_shrink_0().text_size(px(12.)).text_color(rgb(status_color)).child("●"))
                    .child(div().text_size(px(12.)).text_color(rgb(status_color)).child(status_text)),
            );
        } else {
            let mut r = row(IC_PR, MUTED)
                .child(div().text_size(px(13.)).text_color(rgb(MUTED)).child("No open PR for this branch"));
            if let Some(parent) = self.branch_parent.clone() {
                r = r
                    .child(div().text_size(px(13.)).text_color(rgb(MUTED)).child("·  from"))
                    .child(div().text_size(px(13.)).text_color(rgb(TEXT)).child(parent));
            }
            card = card.child(r);
        }

        // uncommitted changes — click to open the Changes pane
        card = card.child(
            row(IC_SCM, if changed > 0 { GIT_MODIFIED } else { MUTED })
                .child(
                    div()
                        .id("status-changes")
                        .cursor_pointer()
                        .text_size(px(13.))
                        .text_color(rgb(if changed > 0 { TEXT } else { MUTED }))
                        .hover(|s| s.text_color(rgb(ACCENT)))
                        .child(if changed == 0 {
                            "Working tree clean".to_string()
                        } else {
                            format!("{changed} uncommitted file{}", if changed == 1 { "" } else { "s" })
                        })
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.left_view = LeftView::Changes;
                            this.show_left = true;
                            cx.notify();
                        })),
                ),
        );

        // admin (wby-next* only): clickable host + the same ▶ the topbar has.
        // The button's colour already says up/down, so no separate dot.
        if let Some(url) = admin_url(&self.root) {
            let host = url.trim_start_matches("https://").trim_start_matches("http://").to_string();
            let open = url.clone();
            card = card.child(
                row(IC_BROWSER, ACCENT)
                    .child({
                        let up = self.admin_up == Some(true);
                        div()
                            .id("status-admin")
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(rgb(if up { GIT_NEW } else { ACCENT }))
                            .hover(|s| s.underline())
                            .child(host)
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                if up {
                                    open_browser(&open);
                                } else if this.admin_terminal(cx).is_some() {
                                    this.show_admin_log(window, cx);
                                } else {
                                    this.start_admin(window, cx);
                                }
                            }))
                    })
                    .child(self.admin_play_btn("status-admin-run", cx)),
            );
        }

        // who the work is for
        if let Some(client) = self.ws_client.clone() {
            card = card.child(
                row(IC_HOME, FOLDER_ICON)
                    .child(div().text_size(px(13.)).text_color(rgb(MUTED)).child("Client"))
                    .child(div().text_size(px(13.)).text_color(rgb(TEXT)).child(client)),
            );
        }

        // pending review comments (nothing pending → no row)
        if !self.review.is_empty() {
            let ready = self.review.iter().filter(|c| !c.draft && !c.resolved).count();
            let drafts = self.review.len() - ready;
            let label = match (ready, drafts) {
                (r, 0) => format!("{r} pending comment{}", if r == 1 { "" } else { "s" }),
                (0, d) => format!("{d} draft comment{}", if d == 1 { "" } else { "s" }),
                (r, d) => format!("{r} pending · {d} draft"),
            };
            card = card.child(
                row(IC_COMMENT, COMMENT_MARK)
                    .child(
                        div()
                            .id("status-review")
                            .cursor_pointer()
                            .text_size(px(13.))
                            .text_color(rgb(COMMENT_MARK))
                            .hover(|s| s.underline())
                            .child(label)
                            .tooltip(tip("Open the review-comments pane"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.left_view = LeftView::Review;
                                this.show_left = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(MUTED))
                            .child(if ready > 0 { "⌥↩ sends them" } else { "open one to make it ready" }),
                    ),
            );
        }

        // agent state reported from a terminal pane (nothing reported → no row)
        if let Some((state, msg)) = self.agent_rollup() {
            let color = agent_color(state);
            let mut r = row(IC_TERMINAL, color);
            if state.is_idle() {
                // same solid badge as the switcher — and the point is nobody is
                // on this workspace, not that some agent stopped
                r = r
                    .child(idle_badge(10.))
                    .child(div().text_size(px(13.)).text_color(rgb(MUTED)).child("no work assigned"));
            } else {
                r = r
                    .child(div().w(px(6.)).flex_shrink_0().text_size(px(12.)).text_color(rgb(color)).child("●"))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(rgb(color))
                            .child(format!("Agent {}", agent_label(state))),
                    );
                if !msg.is_empty() {
                    r = r.child(div().text_size(px(12.)).text_color(rgb(MUTED)).truncate().child(msg));
                }
            }
            card = card.child(r);
        }

        div()
            .flex_grow(1.0)
            .w_full()
            .h_full()
            .min_h(px(0.))
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .child(card)
    }

    fn render_tabs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = div()
            .h(px(34.))
            .flex()
            .flex_row()
            .items_center()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .overflow_hidden();

        for (ix, tab) in self.tabs.iter().enumerate() {
            let active = ix == self.active;
            let dirty = tab.editor.read(cx).is_dirty();
            let name = tab
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // color the filename by its git status (added/modified/deleted),
            // falling back to the normal active/inactive text colors
            let label_color = match self.git_status.get(&tab.path) {
                Some(GitState::New) => GIT_NEW,
                Some(GitState::Modified) => GIT_MODIFIED,
                Some(GitState::Deleted) => GIT_DELETED,
                None if active => TEXT,
                None => MUTED,
            };

            let chip = div()
                .flex()
                .flex_row()
                .items_center()
                .h_full()
                .px_3()
                .gap_2()
                .border_r_1()
                .border_color(rgb(BORDER))
                .when(active, |d| d.bg(rgb(BG)))
                .when(!active, |d| d.hover(|s| s.bg(rgb(HOVER))))
                .child(
                    // label — click switches tab
                    div()
                        .id(("tab", ix))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(rgb(label_color))
                        .child(if dirty { format!("● {}", name) } else { name })
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.switch_tab(ix, window, cx);
                        })),
                )
                .child(
                    // ✕ — click closes tab
                    div()
                        .id(("close", ix))
                        .px_1()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(rgb(MUTED))
                        .hover(|s| s.text_color(rgb(0xf7768e)))
                        .child("✕")
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.close_tab(ix, window, cx);
                        })),
                );

            bar = bar.child(chip);
        }

        bar
    }
}

/// Holds every open project as a live `Storm` view and renders the active one.
/// Background projects keep their editors and running terminals alive.
/// Drag payload for reordering project rows in the switcher (dragged index).
struct DraggedProject(usize);

/// Chip rendered under the cursor while dragging a project card.
struct DragChip {
    label: String,
}
impl Render for DragChip {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(ICON_SELECTED_BG))
            .text_color(rgb(SEL_TEXT))
            .text_size(px(12.))
            .shadow_lg()
            .child(self.label.clone())
    }
}

struct Workspace {
    projects: Vec<Entity<Storm>>,
    active: usize,
    focus_pending: bool,
    focus: FocusHandle,
    // project-switcher dialog
    switcher_open: bool,
    switcher_sel: usize, // index into the *filtered* list
    switcher_focus: FocusHandle,
    switcher_query: Field, // type-to-filter text (full input: caret, selection, word motion)
    // opened via alt-tab (cmd+tab style): keep advancing on tab; commit on opt release
    switcher_quick: bool,
    /// The switcher was opened from a child view, which has no window handle —
    /// the next render claims focus for it.
    switcher_focus_pending: bool,
    /// Project the mouse is over while the ⌥Tab switcher is up: releasing ⌥
    /// commits this rather than the keyboard selection.
    switcher_hover: Option<usize>,
    // blinking caret for the switcher's filter input
    caret_on: bool,
    switcher_scroll: ScrollHandle, // the row list scrolls when it outgrows the panel
    /// Project indices whose agent is mid-turn when ⌘Q was pressed; `Some` shows
    /// the "are you sure" panel instead of quitting.
    quit_busy: Option<Vec<usize>>,
    quit_focus: FocusHandle,
}

impl Workspace {
    fn new(roots: Vec<PathBuf>, cx: &mut Context<Self>) -> Self {
        let mut ws = Self {
            projects: Vec::new(),
            active: 0,
            focus_pending: true,
            focus: cx.focus_handle(),
            switcher_open: false,
            switcher_sel: 0,
            switcher_focus: cx.focus_handle(),
            switcher_query: Field::default(),
            switcher_quick: false,
            switcher_focus_pending: false,
            switcher_hover: None,
            caret_on: true,
            switcher_scroll: ScrollHandle::new(),
            quit_busy: None,
            quit_focus: cx.focus_handle(),
        };
        // open every passed root as its own workspace
        for root in roots {
            ws.add_project(root, cx);
        }
        // start on the first workspace that isn't parked; a paused one is only
        // the landing spot when every workspace is paused
        ws.active = ws
            .projects
            .iter()
            .position(|p| !p.read(cx).ws_paused)
            .unwrap_or(0);
        // Slow repaint so per-project topbar state (branch names, attention dots)
        // pushed up from the child views stays current.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        })
        .detach();
        // caret blink for the switcher's filter input; only repaints while the
        // dialog is actually open
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(530)).await;
            let ok = this
                .update(cx, |this, cx| {
                    this.caret_on = !this.caret_on;
                    if this.switcher_open {
                        cx.notify();
                    }
                })
                .is_ok();
            if !ok {
                break;
            }
        })
        .detach();
        ws
    }

    fn add_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        // already open? focus the existing project instead of opening a duplicate
        let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let target = canon(&root);
        if let Some(i) = self.projects.iter().position(|p| canon(&p.read(cx).root) == target) {
            self.active = i;
            self.focus_pending = true;
            cx.notify();
            return;
        }
        push_recent_project(&root); // remember it for the New Project dialog
        let storm = cx.new(|cx| Storm::new(root, cx));
        cx.subscribe(&storm, |this, _s, ev: &ProjectNav, cx| match ev {
            ProjectNav::Switch(i) => {
                if *i < this.projects.len() {
                    this.active = *i;
                    this.focus_pending = true;
                    cx.notify();
                }
            }
            ProjectNav::Switcher => this.open_switcher(cx),
            ProjectNav::Open => this.open_project(cx),
            ProjectNav::OpenPath(p) => this.add_project(p.clone(), cx),
            ProjectNav::Remove(i) => this.remove_project(*i, cx),
        })
        .detach();
        self.projects.push(storm);
        self.active = self.projects.len() - 1;
        self.focus_pending = true;
        self.persist_session(cx);
        cx.notify();
    }

    /// Remember the open workspaces (in order) so a bare `tide` reopens them.
    fn persist_session(&self, cx: &App) {
        let roots: Vec<PathBuf> = self.projects.iter().map(|p| p.read(cx).root.clone()).collect();
        save_session(&roots);
    }

    /// Close the workspace at `idx`. Keeps at least one open (the window would
    /// be empty otherwise). Adjusts the active + switcher selection to stay valid.
    fn remove_project(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx >= self.projects.len() || self.projects.len() <= 1 {
            return;
        }
        self.projects.remove(idx);
        // keep `active` pointing at a sensible project
        if self.active > idx || self.active >= self.projects.len() {
            self.active = self.active.saturating_sub(1);
        }
        self.switcher_sel = self.switcher_sel.min(self.projects.len() - 1);
        self.focus_pending = true;
        self.persist_session(cx);
        cx.notify();
    }

    /// Drag-reorder: move project `from` to sit before `to`. Keeps `active`
    /// pointing at the same project (by identity) and follows the moved card
    /// with the switcher selection.
    fn reorder_projects(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let n = self.projects.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let active_id = self.projects[self.active].entity_id();
        let item = self.projects.remove(from);
        let dst = if from < to { to - 1 } else { to };
        self.projects.insert(dst, item);
        self.active = self
            .projects
            .iter()
            .position(|p| p.entity_id() == active_id)
            .unwrap_or(0);
        self.switcher_sel = dst;
        self.persist_session(cx);
        cx.notify();
    }

    fn next_project(&mut self, _: &NextProject, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher_step(1, window, cx);
    }

    fn prev_project(&mut self, _: &PrevProject, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher_step(-1, window, cx);
    }

    /// alt-tab (cmd+tab style): open the switcher in "quick" mode and advance the
    /// highlight by `dir`; each further alt-tab advances again. Releasing opt
    /// commits the highlighted project (see `switcher_modifiers`).
    fn switcher_step(&mut self, dir: i32, window: &mut Window, cx: &mut Context<Self>) {
        if self.projects.len() <= 1 {
            return;
        }
        self.switcher_hover = None;
        if !self.switcher_open {
            self.switcher_open = true;
            self.switcher_query.clear();
            self.switcher_hover = None; // stale hover from a previous open
            self.switcher_quick = true;
            window.focus(&self.switcher_focus, cx);
            // quick-switch only walks unpaused workspaces; start from the one
            // after (or before) the active one within that list
            let rotation = self.switcher_matches(cx);
            let here = rotation.iter().position(|&i| i == self.active);
            let len = rotation.len().max(1) as i32;
            self.switcher_sel = match here {
                Some(pos) => ((pos as i32 + dir).rem_euclid(len)) as usize,
                // the active workspace is paused, so it isn't in the rotation
                None => if dir >= 0 { 0 } else { (len - 1) as usize },
            };
        } else {
            let len = self.switcher_matches(cx).len().max(1);
            self.switcher_sel = ((self.switcher_sel as i32 + dir).rem_euclid(len as i32)) as usize;
        }
        self.switcher_scroll.scroll_to_item(self.switcher_sel);
        cx.notify();
    }

    /// Commit the quick-switcher pick when the opt/alt modifier is released.
    fn switcher_modifiers(&mut self, ev: &ModifiersChangedEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if self.switcher_open && self.switcher_quick && !ev.modifiers.alt {
            let filtered = self.switcher_matches(cx);
            // the row under the cursor wins: if you went looking with the mouse,
            // that's the one you meant, not wherever ⌥Tab left the selection
            let target = self
                .switcher_hover
                .filter(|i| filtered.contains(i))
                .or_else(|| filtered.get(self.switcher_sel).copied());
            self.switcher_open = false;
            self.switcher_quick = false;
            self.switcher_hover = None;
            if let Some(pi) = target {
                self.switch_to(pi, cx);
            }
            cx.notify();
        }
    }

    fn open_project_action(&mut self, _: &OpenProject, _w: &mut Window, cx: &mut Context<Self>) {
        self.open_project(cx);
    }

    fn switch_to(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.projects.len() {
            self.active = i;
            self.focus_pending = true;
            cx.notify();
        }
    }

    /// cmd+shift+e: open the project switcher dialog.
    fn show_projects(&mut self, _: &ShowProjects, window: &mut Window, cx: &mut Context<Self>) {
        self.switcher_open = true;
        self.switcher_quick = false;
        self.switcher_query.clear();
        self.switcher_sel = self.active;
        self.switcher_hover = None;
        self.switcher_scroll.scroll_to_item(self.switcher_sel);
        window.focus(&self.switcher_focus, cx);
        cx.notify();
    }

    /// ⌘K from anywhere in the window: open the active project's palette.
    fn act_palette(&mut self, _: &CommandPalette, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.projects.get(self.active).cloned() else { return };
        p.update(cx, |s, cx| s.act_command_palette(&CommandPalette, window, cx));
    }

    /// Same dialog, opened from a child view (the topbar's workspace tally).
    /// Focus is claimed on the next render, since there's no window here.
    fn open_switcher(&mut self, cx: &mut Context<Self>) {
        self.switcher_open = true;
        self.switcher_quick = false;
        self.switcher_query.clear();
        self.switcher_sel = self.active;
        self.switcher_scroll.scroll_to_item(self.switcher_sel);
        self.switcher_focus_pending = true;
        cx.notify();
    }

    /// Project indices matching the type-to-filter query (name or branch).
    fn switcher_matches(&self, cx: &App) -> Vec<usize> {
        let q = self.switcher_query.text.to_lowercase();
        (0..self.projects.len())
            .filter(|&i| {
                // ⌥Tab's rotation excludes paused workspaces entirely; ⌘E lists
                // them (marked, and skipped by ↑↓) so they can be resumed
                if self.switcher_quick && self.projects[i].read(cx).ws_paused {
                    return false;
                }
                if q.is_empty() {
                    return true;
                }
                let s = self.projects[i].read(cx);
                // match the folder too: untitled workspaces have no name to type
                s.project_name().to_lowercase().contains(&q)
                    || s.folder_name().to_lowercase().contains(&q)
                    || s.branch.to_lowercase().contains(&q)
            })
            .collect()
    }

    fn switcher_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        // typing or arrowing means the keyboard is choosing again
        self.switcher_hover = None;
        let filtered = self.switcher_matches(cx);
        match key {
            "escape" => {
                if self.switcher_query.is_empty() {
                    self.switcher_open = false;
                    self.switcher_quick = false;
                    self.focus_pending = true;
                } else {
                    self.switcher_query.clear();
                    self.switcher_sel = 0;
                }
                cx.notify();
            }
            "enter" => {
                if let Some(&pi) = filtered.get(self.switcher_sel) {
                    self.switcher_open = false;
                    self.switcher_quick = false;
                    self.switch_to(pi, cx);
                }
            }
            // ↑↓ walk every row, paused or not — ⌘E is where you go to pick one
            // deliberately; only ⌥Tab's rotation skips paused workspaces.
            // ←→ belong to the text field (caret motion)
            "down" => {
                let n = filtered.len();
                if n > 0 {
                    self.switcher_sel = (self.switcher_sel + 1) % n; // wrap to first
                }
                self.switcher_scroll.scroll_to_item(self.switcher_sel);
                cx.notify();
            }
            "up" => {
                let n = filtered.len();
                if n > 0 {
                    self.switcher_sel = (self.switcher_sel + n - 1) % n; // wrap to last
                }
                self.switcher_scroll.scroll_to_item(self.switcher_sel);
                cx.notify();
            }
            _ => {
                // quick-jump when the filter is empty: 1-9, then 0, -, = (10, 11, 12)
                let m = &ev.keystroke.modifiers;
                if self.switcher_query.is_empty() && !m.platform && !m.control && !m.alt {
                    let target = match key {
                        "0" => Some(9),
                        "-" => Some(10),
                        "=" => Some(11),
                        _ => key.parse::<usize>().ok().filter(|d| (1..=9).contains(d)).map(|d| d - 1),
                    };
                    if let Some(t) = target {
                        if t < self.projects.len() {
                            self.switcher_open = false;
                            self.switch_to(t, cx);
                        }
                        return;
                    }
                }
                // everything else is ordinary text editing — the shared Field
                // handles space, opt+arrows, ⌘A/C/V/X, ⌘Z, word deletes, …
                let clip = cx.read_from_clipboard().and_then(|i| i.text());
                let edit = self.switcher_query.key(&ev.keystroke, clip, |_| true);
                if let Some(text) = self.switcher_query.take_copy() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                if edit == Edit::Changed {
                    self.switcher_sel = 0; // a new filter starts at the top row
                }
                if edit != Edit::None {
                    cx.notify();
                }
            }
        }
    }

    /// Show the macOS folder picker and open the chosen directory as a project.
    fn open_project(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let path = cx
                .background_executor()
                .spawn(async {
                    let out = Command::new("osascript")
                        .arg("-e")
                        .arg("POSIX path of (choose folder with prompt \"Open Project\")")
                        .output()
                        .ok()?;
                    if !out.status.success() {
                        return None;
                    }
                    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    (!p.is_empty()).then(|| PathBuf::from(p))
                })
                .await;
            if let Some(root) = path {
                this.update(cx, |this, cx| this.add_project(root, cx)).ok();
            }
        })
        .detach();
    }
}

impl Workspace {
    /// Workspaces whose agent is mid-turn — a terminal pane that reported
    /// `working`, or the ACP panel with a prompt in flight.
    fn busy_workspaces(&self, cx: &App) -> Vec<usize> {
        self.projects
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                let s = p.read(cx);
                s.agent_busy || matches!(s.agent_rollup(), Some((AgentState::Working, _)))
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// ⌘Q: save the tab in front, then quit — unless an agent is still working,
    /// in which case ask first. Comments, titles, and the session list are
    /// already written as they change, so there's nothing else to flush.
    fn act_quit(&mut self, _: &QuitApp, window: &mut Window, cx: &mut Context<Self>) {
        let busy = self.busy_workspaces(cx);
        if busy.is_empty() {
            self.quit_now(cx);
            return;
        }
        self.quit_busy = Some(busy);
        window.focus(&self.quit_focus, cx);
        cx.notify();
    }

    fn quit_now(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = self.projects.get(self.active) {
            p.update(cx, |s, cx| s.save_tab(s.active, cx));
        }
        cx.quit();
    }

    /// Keys while the quit prompt is up: ↩ quits, esc keeps working.
    fn quit_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "enter" => self.quit_now(cx),
            "escape" => {
                self.quit_busy = None;
                self.focus_pending = true;
                window.focus(&self.focus, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    /// One row per still-working workspace, in the same shape ⌘E uses: number
    /// key-cap, state dot, folder icon, title + path, branch line.
    fn render_quit_confirm(&self, busy: &[usize], cx: &mut Context<Self>) -> impl IntoElement {
        let n = busy.len();
        let mut rows = div().flex().flex_col().gap_1();
        for &i in busy {
            let Some(p) = self.projects.get(i) else { continue };
            let (name, branch, root, state, target) = {
                let s = p.read(cx);
                let target = s
                    .pr_link
                    .as_ref()
                    .map(|(_, _, _, b, _, _)| b.clone())
                    .filter(|b| !b.is_empty())
                    .or_else(|| s.branch_parent.clone());
                let state = s.agent_rollup().map(|(st, _)| st).unwrap_or(AgentState::Working);
                (s.project_name(), s.branch.clone(), s.root.clone(), state, target)
            };
            rows = rows.child(
                div()
                    .id(("quit-busy", i))
                    .w_full()
                    .py_2()
                    .px_3()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x34373c)).border_color(rgb(ACCENT)))
                    // click a row to go look at what it's doing, dialog dismissed
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.quit_busy = None;
                        this.active = i;
                        this.focus_pending = true;
                        window.focus(&this.focus, cx);
                        cx.notify();
                    }))
                    .child(key_cap(format!("{}", i + 1), true))
                    .child(
                        div()
                            .w(px(7.))
                            .h(px(7.))
                            .rounded_full()
                            .flex_shrink_0()
                            .bg(rgb(agent_color(state))),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .font_family(ICON_FONT)
                            .text_size(px(13.))
                            .text_color(rgb(FOLDER_ICON))
                            .child(IC_FOLDER),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .min_w(px(0.))
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(13.))
                                            .text_color(rgb(TEXT))
                                            .child(name),
                                    )
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(10.))
                                            .text_color(rgb(LINE_NUMBER))
                                            .child(tilde_path(&root)),
                                    ),
                            )
                            .when(!branch.is_empty(), |d| {
                                d.child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_1()
                                        .min_w(px(0.))
                                        .overflow_hidden()
                                        .text_size(px(10.))
                                        .when_some(target, |d, t| {
                                            d.child(branch_name_el(&t).flex_shrink_0()).child(
                                                div().flex_shrink_0().text_color(rgb(LINE_NUMBER)).child("←"),
                                            )
                                        })
                                        .child(branch_name_el(&branch).flex_shrink_0()),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(10.))
                            .text_color(rgb(agent_color(state)))
                            .child(agent_label(state)),
                    ),
            );
        }
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000088))
            .child(
                div()
                    .w(px(560.))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .bg(rgb(PANEL_BG))
                    .border_1()
                    .border_color(rgb(GIT_MODIFIED))
                    .rounded_lg()
                    .shadow_lg()
                    .track_focus(&self.quit_focus)
                    .on_key_down(cx.listener(Self::quit_key))
                    .child(div().text_size(px(14.)).text_color(rgb(TEXT)).child(format!(
                        "{n} workspace{} still working",
                        if n == 1 { "" } else { "s" }
                    )))
                    .child(rows)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(div().flex_grow(1.0))
                            .child(rc_button("quit-cancel", "Keep working", false, false).on_click(
                                cx.listener(|this, _e, window, cx| {
                                    this.quit_busy = None;
                                    window.focus(&this.focus, cx);
                                    cx.notify();
                                }),
                            ))
                            .child(rc_button("quit-ok", "Quit anyway", true, false).on_click(
                                cx.listener(|this, _e, _w, cx| this.quit_now(cx)),
                            )),
                    ),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        // push the project list into the active view so its dropdown can show it
        // the ▾ dropdown has no path column, so an untitled project shows its
        // folder there rather than a wall of "Untitled" rows
        let names: Vec<String> = self
            .projects
            .iter()
            .map(|p| {
                let s = p.read(cx);
                match s.ws_title.as_ref().filter(|t| !t.trim().is_empty()) {
                    Some(t) => t.clone(),
                    None => s.folder_name(),
                }
            })
            .collect();
        let branches: Vec<String> = self.projects.iter().map(|p| p.read(cx).branch.clone()).collect();
        let roots: Vec<PathBuf> = self.projects.iter().map(|p| p.read(cx).root.clone()).collect();
        let states: Vec<Option<AgentState>> = self
            .projects
            .iter()
            .map(|p| p.read(cx).agent_rollup().map(|(s, _)| s))
            .collect();
        self.projects[active].update(cx, |s, _| {
            s.ws_names = names;
            s.ws_roots = roots;
            s.ws_branches = branches;
            s.ws_states = states;
            s.ws_active = active;
        });
        // hide every background project's native WebView so it doesn't float
        // over the active workspace (the active one re-shows its own on render)
        for (i, p) in self.projects.iter().enumerate() {
            if i != active {
                p.update(cx, |s, _| s.park_browser());
            }
        }
        // focus the active project after a switch (render has the Window)
        if self.focus_pending {
            self.focus_pending = false;
            let storm = self.projects[active].clone();
            storm.update(cx, |s, cx| s.focus_active(window, cx));
        } else if window.focused(cx).is_none() || self.focus.is_focused(window) {
            // Self-heal focus: when nothing is focused (e.g. after a dialog closes
            // or the window regains focus) or it landed on the bare workspace root,
            // global shortcuts like cmd+shift+p wouldn't route (they're handled in
            // the project view). Push focus back into the active project so they
            // work without first clicking the editor.
            let storm = self.projects[active].clone();
            storm.update(cx, |s, cx| s.focus_active(window, cx));
        }

        let mut root = div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_modifiers_changed(cx.listener(Self::switcher_modifiers))
            .on_action(cx.listener(Self::next_project))
            .on_action(cx.listener(Self::prev_project))
            .on_action(cx.listener(Self::open_project_action))
            .on_action(cx.listener(Self::show_projects))
            .on_action(cx.listener(Self::act_quit))
            // Actions only travel *up* from the focused element, so a handler on
            // the project view is unreachable whenever focus sits on this root
            // (right after the switcher closes, say) — which is why ⌘K sometimes
            // did nothing. Handle it here too and forward it down.
            .on_action(cx.listener(Self::act_palette))
            .child(self.projects[active].clone());

        if let Some(busy) = self.quit_busy.clone() {
            root = root.child(self.render_quit_confirm(&busy, cx));
        }

        if self.switcher_open {
            if std::mem::take(&mut self.switcher_focus_pending) {
                window.focus(&self.switcher_focus, cx);
            }
            let win = window.viewport_size();
            let w = 520.0_f32;
            let filtered = self.switcher_matches(cx);
            // The panel is anchored (fixed top, so the input never moves under the
            // cursor while typing) and sizes itself to its content up to a cap —
            // no row-height estimate to get wrong, and the list only scrolls when
            // it genuinely doesn't fit.
            let left = ((f32::from(win.width) - w) / 2.0).max(0.);
            let top = (f32::from(win.height) * 0.12).max(56.0);
            let max_h = (f32::from(win.height) - top - 56.0).max(200.0);
            // vertical list, one full-width row per matching project
            let mut grid = div().flex().flex_col().gap_1().px_2().pb_2();
            for (pos, &i) in filtered.iter().enumerate() {
                let paused = self.projects[i].read(cx).ws_paused;
                let client = self.projects[i].read(cx).ws_client.clone();
                let (name, branch, root, agent, target) = {
                    let s = self.projects[i].read(cx);
                    // same target as the topbar: the PR's base branch, else the
                    // reflog's best guess at what this branch came off
                    let target = s
                        .pr_link
                        .as_ref()
                        .map(|(_, _, _, b, _, _)| b.clone())
                        .filter(|b| !b.is_empty())
                        .or_else(|| s.branch_parent.clone());
                    (s.project_name(), s.branch.clone(), s.root.clone(), s.agent_rollup(), target)
                };
                let sel = pos == self.switcher_sel;
                let multi = self.projects.len() > 1;
                let idx = i;
                // quick-jump key label: 1-9, then 0, -, = for rows 10-12
                let num = match pos {
                    0..=8 => (pos + 1).to_string(),
                    9 => "0".to_string(),
                    10 => "-".to_string(),
                    11 => "=".to_string(),
                    _ => String::new(),
                };
                let free = agent.as_ref().is_some_and(|(s, _)| s.is_idle());
                let label_drag = name.clone();
                grid = grid.child(
                    div()
                        .id(("ws-switch", i))
                        .relative()
                        .w_full()
                        .on_mouse_move(cx.listener(move |this, _e: &MouseMoveEvent, _w, cx| {
                            if this.switcher_hover != Some(i) {
                                this.switcher_hover = Some(i);
                                cx.notify();
                            }
                        }))
                        // min, not fixed: a row with an agent-state line is taller,
                        // and a fixed height clipped it into the row below
                        .min_h(px(64.))
                        .py_2()
                        .px_3()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(if sel { ACCENT } else { BORDER }))
                        // a free workspace is the thing you open this list to
                        // find: give the whole row the badge's colour, faintly
                        // the wash alone: a cyan border as well read as loud as
                        // the selected row
                        .when(free && !sel, |d| d.bg(rgba(FREE_TINT)))
                        .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                        // HOVER is nearly identical to POPUP_BG here → use a clearly
                        // lighter tint + accent border so hover is actually visible
                        .when(!sel, |d| d.hover(|s| s.bg(rgb(0x34373c)).border_color(rgb(ACCENT))))
                        .cursor_pointer()
                        // drag to reorder: drop onto another row to move before it
                        .on_drag(DraggedProject(i), move |_, _, _, cx| {
                            cx.new(|_| DragChip { label: label_drag.clone() })
                        })
                        .drag_over::<DraggedProject>(|s, _, _, _| s.border_color(rgb(ACCENT)).bg(rgb(HOVER)))
                        .on_drop(cx.listener(move |this, d: &DraggedProject, _w, cx| {
                            this.reorder_projects(d.0, idx, cx);
                        }))
                        // quick-jump key (sequential over the filtered list) — same
                        // key-cap as the workspace number in the top-left topbar
                        .child(key_cap(num.clone(), !sel))
                        // agent state reported from this project's terminal panes
                        // (idle gets the loud pill on the right instead of a dot)
                        .when_some(agent.clone().filter(|(s, _)| !s.is_idle()), |d, (s, _)| {
                            d.child(
                                div()
                                    .w(px(7.))
                                    .h(px(7.))
                                    .rounded_full()
                                    .flex_shrink_0()
                                    .bg(rgb(agent_color(s))),
                            )
                        })
                        // folder icon
                        .child(
                            div()
                                .flex_shrink_0()
                                .font_family(ICON_FONT)
                                .text_size(px(13.))
                                .text_color(rgb(if sel { SEL_TEXT } else { FOLDER_ICON }))
                                .child(IC_FOLDER),
                        )
                        // name + branch, taking the remaining width
                        .child(
                            div()
                                .flex_grow(1.0)
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap(px(2.))
                                // title + the real folder path (grayed), since a
                                // custom workspace title hides which folder it is
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .gap_2()
                                        .min_w(px(0.))
                                        // clip on the row, not per child: a `truncate()`
                                        // child inside an items_center flex row collapses
                                        // to zero width here
                                        .overflow_hidden()
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_size(px(13.))
                                                .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                                                .child(name),
                                        )
                                        .when_some(client, |d, c| {
                                            d.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .px_1()
                                                    .rounded_md()
                                                    .bg(rgb(HOVER))
                                                    .text_size(px(9.))
                                                    .text_color(rgb(FOLDER_ICON))
                                                    .child(c),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_shrink_0()
                                                .text_size(px(10.))
                                                .text_color(rgb(if sel { SEL_TEXT } else { LINE_NUMBER }))
                                                .child(tilde_path(&root)),
                                        ),
                                )
                                // a free workspace is a clean slot: its branch is
                                // whatever `next` left and it has no live session,
                                // so both lines are noise
                                .when(!branch.is_empty() && !free, |d| {
                                    d.child(
                                        div()
                                            .flex()
                                            .flex_row()
                                            .items_center()
                                            .gap_1()
                                            .min_w(px(0.))
                                            .overflow_hidden()
                                            .text_size(px(10.))
                                            // base ← head, GitHub's order, same as
                                            // the topbar shows for the open project
                                            .when_some(target, |d, t| {
                                                d.child(branch_name_el(&t).flex_shrink_0())
                                                    .child(
                                                        div()
                                                            .flex_shrink_0()
                                                            .text_color(rgb(LINE_NUMBER))
                                                            .child("←"),
                                                    )
                                            })
                                            .child(div().flex_shrink_0().text_color(rgb(MUTED)).child("⎇"))
                                            .child(
                                                branch_name_el(&branch)
                                                    .flex_shrink_0(),
                                            ),
                                    )
                                })
                                // what the agent last reported, so the dot's colour
                                // isn't the only thing you have to go on
                                .when_some(agent.clone().filter(|(s, _)| !s.is_idle()), |d, (st, msg)| {
                                    let text = if msg.is_empty() {
                                        format!("● {}", agent_label(st))
                                    } else {
                                        format!("● {} · {msg}", agent_label(st))
                                    };
                                    d.child(
                                        div()
                                            .text_size(px(10.))
                                            .truncate()
                                            .text_color(rgb(agent_color(st)))
                                            .child(text),
                                    )
                                }),
                        )
                        // idle = free to grab. A grey word doesn't carry that, so
                        // it gets a solid badge at the end of the row instead.
                        .when(free, |d| d.child(idle_badge(9.)))
                        // paused: skipped by ⌥Tab and by ↑↓ here. Click the badge
                        // to resume it without opening the workspace first.
                        .when(paused, |d| {
                            d.child(
                                div()
                                    .id(("ws-resume", i))
                                    .flex_shrink_0()
                                    .px_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .border_1()
                                    .border_color(rgb(GIT_MODIFIED))
                                    .text_size(px(9.))
                                    .text_color(rgb(GIT_MODIFIED))
                                    .hover(|s| s.bg(rgb(GIT_MODIFIED)).text_color(rgb(BG)))
                                    .child("PAUSED")
                                    .tooltip(tip("Paused — click to resume"))
                                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.projects[i].update(cx, |s, cx| s.toggle_pause(cx));
                                        cx.notify();
                                    })),
                            )
                        })
                        // close button (hidden with one project left)
                        .when(multi, |d| {
                            d.child(
                                div()
                                    .id(("ws-close", i))
                                    .flex_shrink_0()
                                    .px_1()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .hover(|s| s.text_color(rgb(0xf7768e)))
                                    .cursor_pointer()
                                    .child("✕")
                                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                    .on_click(cx.listener(move |this, _e, window, cx| {
                                        this.remove_project(idx, cx);
                                        // keep the switcher focused so Esc still closes it
                                        // (remove_project set focus_pending, which would
                                        // otherwise hand focus to a project view)
                                        if this.projects.len() <= 1 {
                                            this.switcher_open = false;
                                            this.focus_pending = true;
                                        } else {
                                            this.focus_pending = false;
                                            window.focus(&this.switcher_focus, cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                        })
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.switcher_open = false;
                            this.switcher_quick = false;
                            this.switch_to(idx, cx);
                        })),
                );
            }
            // tally across every project (not just the filtered rows) so the
            // header answers "what's the state of my fleet" at a glance
            let mut tally: Vec<(AgentState, usize)> = Vec::new();
            for p in &self.projects {
                if let Some((s, _)) = p.read(cx).agent_rollup() {
                    match tally.iter_mut().find(|(t, _)| *t == s) {
                        Some((_, n)) => *n += 1,
                        None => tally.push((s, 1)),
                    }
                }
            }
            tally.sort_by_key(|(s, _)| std::cmp::Reverse(s.priority()));
            let mut counts = div().flex().flex_row().items_center().gap_2();
            for (s, n) in tally {
                counts = counts.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .w(px(6.))
                                .h(px(6.))
                                .rounded_full()
                                .flex_shrink_0()
                                .bg(rgb(agent_color(s))),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(rgb(agent_color(s)))
                                .child(format!("{n} {}", agent_label(s))),
                        ),
                );
            }
            let empty = self.switcher_query.is_empty();
            let header = div()
                .flex()
                .flex_col()
                .gap_1()
                .px_3()
                .py_1()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        // a real input box: caret, selection, word motion, ⌘A/V…
                        .child(
                            div()
                                .flex_grow(1.0)
                                .min_w(px(0.))
                                .px_2()
                                .py_1()
                                .bg(rgb(BG))
                                .border_1()
                                .border_color(rgb(BORDER))
                                .rounded_md()
                                .text_size(px(12.))
                                .text_color(rgb(TEXT))
                                .child(
                                    self.switcher_query
                                        .render(if self.caret_on { "▏" } else { " " }, SELECTION),
                                ),
                        )
                        .child(counts),
                )
                // always rendered (dimmer once you're typing) — a hint line that
                // appears and disappears would resize the header under the caret
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(rgb(if empty { MUTED } else { LINE_NUMBER }))
                        .child("type to filter  ·  ↑↓ move  ·  ↩ open  ·  1-9 jump"),
                );
            let panel = div()
                .absolute()
                .top(px(top))
                .left(px(left))
                .w(px(w))
                .bg(rgb(POPUP_BG))
                .border_1()
                .border_color(rgb(ACCENT))
                .rounded_md()
                .shadow_lg()
                // grows with its content, never past the window
                .max_h(px(max_h))
                .flex()
                .flex_col()
                .py_1()
                .overflow_hidden()
                .track_focus(&self.switcher_focus)
                .on_key_down(cx.listener(Self::switcher_key))
                .child(header)
                .child(
                    div()
                        .id("ws-switch-list")
                        // shrinkable, not growable: the list takes the height it
                        // needs and only scrolls once the panel hits its cap
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .track_scroll(&self.switcher_scroll)
                        .child(grid),
                );
            root = root.child(panel);
        }

        root
    }
}

/// A standalone window showing one file's diff, with prev/next over the file set.
struct DiffWindow {
    root: PathBuf,
    files: Vec<PathBuf>,
    idx: usize,
    // diff sources: `old` rev (None = working diff vs disk), `new_rev` (None = HEAD)
    old: Option<String>,
    new_rev: Option<String>,
    rows: Vec<DiffRow>,
    focus: FocusHandle,
    focused: bool,
    left_scroll: ScrollHandle,
    right_scroll: ScrollHandle,
    // set when the file (re)loads → next render scrolls to the first change
    pending_scroll: bool,
    sel: Option<DiffSel>,
    dragging: bool,
    char_w: f32,
    hl: Highlighter,
    left_styles: Vec<Vec<Run>>,
    right_styles: Vec<Vec<Run>>,
    // blinking text caret (side, row, col) — read-only but clickable
    caret: Option<(DiffSide, usize, usize)>,
    caret_on: bool,
    // cmd+f search
    search_open: bool,
    search: Field,
    search_focus: FocusHandle,
    matches: Vec<DiffMatch>,
    cur_match: usize,
    // the main app window + entity, so F4 can open the file there
    storm: WeakEntity<Storm>,
    main_window: Option<AnyWindowHandle>,
    // collapsed directories in the file tree sidebar (relative dir paths)
    tree_collapsed: HashSet<PathBuf>,
    // sidebar file filter (substring over each file's relative path)
    filter: Field,
    filter_focus: FocusHandle,
    // fraction of the diff area given to the left pane (draggable divider)
    pane_split: f32,
    resizing_pane: bool,
    // file-tree sidebar width, and whether its divider is being dragged
    sidebar_w: f32,
    resizing_sidebar: bool,
    // PR-review mode: sidebar gains viewed checkboxes, progress, viewed-filter
    pr_mode: bool,
    pr_filter: PrViewFilter,
    pr_hidden: HashSet<usize>, // file indices filtered out by pr_filter (this render)
    pr_viewed: HashSet<PathBuf>, // local snapshot (avoids reading Storm mid-update)
    states: HashMap<PathBuf, GitState>, // git status per file, for name colours
    comment_counts: HashMap<PathBuf, usize>, // review comments per file (sidebar)
    // review comments on the file being shown: (line, text), pulled from Storm
    comments: Vec<(usize, String)>,
    rc_open: bool,           // inline comment input under a row
    rc_row: usize,           // the diff row it's attached to
    rc_line: usize,          // that row's file line
    rc_query: Field,
    rc_focus: FocusHandle,
    rc_replies: Vec<ReviewReply>, // thread of the comment the box is editing
}

/// One visible row in the Diff window's file-tree sidebar.
struct DiffTreeRow {
    depth: usize,
    label: String,
    dir: Option<PathBuf>,   // Some → a directory row (its relative path), toggles collapse
    file_idx: Option<usize>, // Some → a file row (index into `files`)
}

impl DiffWindow {
    fn new(
        root: PathBuf,
        files: Vec<PathBuf>,
        idx: usize,
        old: Option<String>,
        new_rev: Option<String>,
        pr_mode: bool,
        pr_viewed: HashSet<PathBuf>,
        states: HashMap<PathBuf, GitState>,
        filter_text: String,
        pr_view: PrViewFilter,
        storm: WeakEntity<Storm>,
        main_window: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let hl = Highlighter::new();
        // seed the sidebar filter from the source pane so cmd+d keeps the view
        let mut filter = Field::default();
        if !filter_text.trim().is_empty() {
            filter.set(filter_text);
        }
        let (rows, left_styles, right_styles) = compute_diff(&root, &files[idx], &old, &new_rev, &hl);
        // blink the text caret
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(530)).await;
            let ok = this
                .update(cx, |this: &mut DiffWindow, cx| {
                    if this.caret.is_some() {
                        this.caret_on = !this.caret_on;
                        cx.notify();
                    }
                    // keep the inline notes in step with the main window, where a
                    // comment may have been added or sent while this window is open
                    let before = this.comments.len();
                    this.load_comments(cx);
                    if this.comments.len() != before {
                        cx.notify();
                    }
                })
                .is_ok();
            if !ok {
                break;
            }
        })
        .detach();
        Self {
            root,
            files,
            idx,
            old,
            new_rev,
            rows,
            focus: cx.focus_handle(),
            focused: false,
            left_scroll: ScrollHandle::new(),
            right_scroll: ScrollHandle::new(),
            pending_scroll: true,
            sel: None,
            dragging: false,
            char_w: 8.0,
            hl,
            left_styles,
            right_styles,
            caret: None,
            caret_on: true,
            search_open: false,
            search: Field::default(),
            search_focus: cx.focus_handle(),
            matches: Vec::new(),
            cur_match: 0,
            storm,
            main_window,
            tree_collapsed: HashSet::new(),
            filter,
            filter_focus: cx.focus_handle(),
            pane_split: 0.5,
            resizing_pane: false,
            // wide enough for deep monorepo paths without dragging every time
            sidebar_w: 420.,
            resizing_sidebar: false,
            pr_mode,
            pr_filter: pr_view,
            pr_hidden: HashSet::new(),
            pr_viewed,
            states,
            comment_counts: HashMap::new(),
            comments: Vec::new(),
            rc_open: false,
            rc_row: 0,
            rc_line: 0,
            rc_query: Field::default(),
            rc_focus: cx.focus_handle(),
            rc_replies: Vec::new(),
        }
    }

    /// Pull this file's review comments from the main window, so they can be
    /// drawn under their rows. Cheap: a read of an in-memory Vec.
    fn load_comments(&mut self, cx: &mut Context<Self>) {
        let path = self.files[self.idx].clone();
        let (list, counts) = self
            .storm
            .read_with(cx, |s, _| {
                let list = s
                    .review
                    .iter()
                    .filter(|c| c.path == path)
                    .map(|c| (c.end, c.text.clone()))
                    .collect::<Vec<_>>();
                // counts for every file, so the sidebar can mark them
                let mut counts: HashMap<PathBuf, usize> = HashMap::new();
                for c in &s.review {
                    *counts.entry(c.path.clone()).or_insert(0) += 1;
                }
                (list, counts)
            })
            .unwrap_or_default();
        self.comments = list;
        self.comment_counts = counts;
    }

    /// Open the inline comment box under the caret's row.
    fn open_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        // with no caret yet (nothing clicked since the window opened), comment on
        // the first row in view rather than doing nothing
        let row = self
            .caret
            .map(|(_, r, _)| r)
            .or(self.sel.as_ref().map(|s| s.head.0))
            .unwrap_or_else(|| self.right_scroll.top_item().min(self.rows.len() - 1));
        if self.caret.is_none() {
            self.caret = Some((DiffSide::Right, row, 0));
        }
        let (line, _) = self.cursor_file_pos();
        self.rc_row = row;
        self.rc_line = line;
        let existing = self
            .comments
            .iter()
            .find(|(l, _)| *l == line)
            .map(|(_, t)| t.clone())
            .unwrap_or_default();
        let path = self.files[self.idx].clone();
        self.rc_replies = self
            .storm
            .read_with(cx, |s, _| {
                s.review
                    .iter()
                    .find(|c| c.path == path && c.end == line)
                    .map(|c| c.replies.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        self.rc_query.multiline = true; // ↩ newline, ⌘↩ save
        self.rc_query.set(existing);
        self.rc_open = true;
        // same rule as the editor's box: an existing thread opens for reading,
        // a fresh note takes the caret straight away
        if self.rc_replies.is_empty() {
            window.focus(&self.rc_focus, cx);
        }
        cx.notify();
    }

    /// Hand the note to the main window's review list (the diff window owns no
    /// state of its own here — Storm is where comments live and get sent from).
    fn save_comment(&mut self, cx: &mut Context<Self>) {
        let path = self.files[self.idx].clone();
        let line = self.rc_line;
        let text = self.rc_query.text.trim().to_string();
        let snippet = self
            .rows
            .get(self.rc_row)
            .map(|r| diff_side_text(r, DiffSide::Right))
            .filter(|t| !t.trim().is_empty())
            .or_else(|| self.rows.get(self.rc_row).map(|r| diff_side_text(r, DiffSide::Left)))
            .unwrap_or_default();
        let _ = self.storm.update(cx, |s, cx| {
            s.set_comment(path, line, line, snippet, text, false, cx);
        });
        self.load_comments(cx);
    }

    /// The inline comment input, rendered as a row inside the right pane.
    fn render_diff_comment_box(&mut self, rc_focused: bool, cx: &mut Context<Self>) -> gpui::AnyElement {
        let body_h = rc_lines(&self.rc_query.text) as f32 * RC_LINE_H;
        let replies = self.rc_replies.clone();
        let thread_h = rc_thread_rows(&replies) as f32 * RC_LINE_H;
        let has_note = self.comments.iter().any(|(l, _)| *l == self.rc_line);
        div()
            .min_h(px(rc_box_h(&self.rc_query.text, &replies)))
            .w_full()
            .mx(px(RC_BOX_PAD))
            .my(px(RC_BOX_PAD))
            .px_2()
            .py_1()
            .bg(rgb(PANEL_BG))
            .border_1()
            .border_color(rgb(COMMENT_MARK))
            .rounded_md()
            .flex()
            .flex_col()
            .gap(px(2.))
            .overflow_hidden()
            .track_focus(&self.rc_focus)
            .on_key_down(cx.listener(Self::rc_key))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .text_size(px(10.))
                    .child(div().text_color(rgb(COMMENT_MARK)).child(format!("comment on line {}", self.rc_line)))
                    .child(div().flex_grow(1.0))
                    .child(div().text_color(rgb(LINE_NUMBER)).child("⌘↩ save · ↩ newline · esc cancel")),
            )
            .when(!replies.is_empty(), |d| {
                d.child(
                    div()
                        .id("diff-rc-thread")
                        .h(px(thread_h))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .children(replies.iter().map(|r| {
                            let agent = r.who == "agent";
                            div()
                                .flex()
                                .flex_row()
                                .gap_1()
                                .pl_1()
                                .border_l_2()
                                .border_color(rgb(if agent { GIT_NEW } else { COMMENT_MARK }))
                                .text_size(px(12.))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_color(rgb(if agent { GIT_NEW } else { COMMENT_MARK }))
                                        .child(if agent { "agent" } else { "you" }),
                                )
                                .child(md_text(&r.text, 12.0, TEXT).flex_grow(1.0))
                        })),
                )
            })
            .child(
                div()
                    .px_2()
                    .py_1()
                    // grow with the text (wrapping included) instead of clipping;
                    // the cap is what makes a long note scroll inside the box
                    .min_h(px(body_h + 8.0))
                    .max_h(px(RC_MAX_LINES as f32 * RC_LINE_H + 8.0))
                    .line_height(px(RC_LINE_H))
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .overflow_hidden()
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    // opens unfocused on an existing thread: click here to type
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this: &mut DiffWindow, _e, window, cx| {
                            window.focus(&this.rc_focus, cx);
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .mt(px(-rc_scroll_off(&self.rc_query)))
                            .child(self.rc_query.render(if rc_focused { "▏" } else { "" }, SELECTION)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .when(has_note, |d| {
                        d.child(rc_button("diff-rc-delete", "Delete", false, true).on_click(
                            cx.listener(|this, _e, window, cx| {
                                this.rc_query.set(String::new());
                                this.save_comment(cx); // empty text removes it
                                this.rc_open = false;
                                window.focus(&this.focus, cx);
                                cx.notify();
                            }),
                        ))
                    })
                    .child(div().flex_grow(1.0))
                    .child(rc_button("diff-rc-cancel", "Cancel", false, false).on_click(cx.listener(
                        |this, _e, window, cx| {
                            this.rc_open = false;
                            window.focus(&this.focus, cx);
                            cx.notify();
                        },
                    )))
                    .child(
                        rc_button("diff-rc-save", if has_note { "Update" } else { "Comment" }, true, false)
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.save_comment(cx);
                                this.rc_open = false;
                                window.focus(&this.focus, cx);
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn rc_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev.keystroke.key.as_str() {
            "escape" => {
                self.rc_open = false;
                window.focus(&self.focus, cx);
            }
            "enter" if ev.keystroke.modifiers.platform => {
                self.save_comment(cx);
                self.rc_open = false;
                window.focus(&self.focus, cx);
            }
            _ => {
                let clip = cx.read_from_clipboard().and_then(|i| i.text());
                self.rc_query.key(&ev.keystroke, clip, |_| true);
            }
        }
        cx.notify();
    }

    /// Flatten `files` into a directory tree (respecting collapsed dirs) for the
    /// sidebar — dirs first then files at each level, both alphabetical.
    fn tree_rows(&self) -> Vec<DiffTreeRow> {
        #[derive(Default)]
        struct Node {
            dirs: std::collections::BTreeMap<String, Node>,
            files: std::collections::BTreeMap<String, usize>,
        }
        let q = self.filter.text.trim().to_lowercase();
        let filtering = !q.is_empty();
        let mut root = Node::default();
        for (i, f) in self.files.iter().enumerate() {
            if self.pr_hidden.contains(&i) {
                continue; // filtered out by the viewed-state filter (PR mode)
            }
            let rel = f.strip_prefix(&self.root).unwrap_or(f);
            // filter on the whole relative path so "core/req" or ".test" both work
            if filtering && !rel.to_string_lossy().to_lowercase().contains(&q) {
                continue;
            }
            let comps: Vec<String> =
                rel.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
            let mut node = &mut root;
            for c in &comps[..comps.len().saturating_sub(1)] {
                node = node.dirs.entry(c.clone()).or_default();
            }
            if let Some(name) = comps.last() {
                node.files.insert(name.clone(), i);
            }
        }
        let mut out = Vec::new();
        // while filtering, ignore collapse state so every match is visible
        fn walk(node: &Node, depth: usize, prefix: &Path, collapsed: &HashSet<PathBuf>, filtering: bool, out: &mut Vec<DiffTreeRow>) {
            for (name, child) in &node.dirs {
                let path = prefix.join(name);
                out.push(DiffTreeRow { depth, label: name.clone(), dir: Some(path.clone()), file_idx: None });
                if filtering || !collapsed.contains(&path) {
                    walk(child, depth + 1, &path, collapsed, filtering, out);
                }
            }
            for (name, idx) in &node.files {
                out.push(DiffTreeRow { depth, label: name.clone(), dir: None, file_idx: Some(*idx) });
            }
        }
        walk(&root, 0, Path::new(""), &self.tree_collapsed, filtering, &mut out);
        out
    }

    /// Keystrokes while the sidebar filter input is focused. Esc clears/close,
    /// Enter jumps to the first matching file.
    fn filter_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // ⌥C belongs to the diff, not to the filter text
        if ev.keystroke.modifiers.alt && ev.keystroke.key == "c" {
            self.open_comment(window, cx);
            return;
        }
        let ks = &ev.keystroke;
        if ks.key == "escape" {
            if self.filter.is_empty() {
                window.focus(&self.focus, cx); // nothing to clear → hand focus back to the diff
            } else {
                self.filter.clear();
            }
            cx.notify();
            return;
        }
        if ks.key == "enter" {
            if let Some(idx) = self.tree_rows().iter().find_map(|r| r.file_idx) {
                self.goto(idx, cx);
            }
            return;
        }
        let clip = cx.read_from_clipboard().and_then(|c| c.text());
        self.filter.key(ks, clip, |_| true);
        if let Some(text) = self.filter.take_copy() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        cx.notify();
    }

    /// Recompute search matches across both sides for the current query.
    fn recompute_matches(&mut self) {
        self.matches.clear();
        let q: Vec<char> = self.search.text.to_lowercase().chars().collect();
        if q.is_empty() {
            self.cur_match = 0;
            return;
        }
        for (i, row) in self.rows.iter().enumerate() {
            for side in [DiffSide::Left, DiffSide::Right] {
                let chars: Vec<char> = diff_side_text(row, side).to_lowercase().chars().collect();
                if chars.len() < q.len() {
                    continue;
                }
                let mut s = 0;
                while s + q.len() <= chars.len() {
                    if chars[s..s + q.len()] == q[..] {
                        self.matches.push((side, i, s, s + q.len()));
                        s += q.len();
                    } else {
                        s += 1;
                    }
                }
            }
        }
        if self.cur_match >= self.matches.len() {
            self.cur_match = 0;
        }
    }

    /// Scroll the current match into view on its side.
    fn scroll_to_match(&mut self) {
        let Some(&(side, row, col, _)) = self.matches.get(self.cur_match) else { return };
        let handle = match side {
            DiffSide::Left => &self.left_scroll,
            DiffSide::Right => &self.right_scroll,
        };
        let vh = f32::from(handle.bounds().size.height).max(1.0);
        let vw = f32::from(handle.bounds().size.width).max(1.0);
        let max = handle.max_offset();
        let target_y = (row as f32 * 18.0 - vh / 2.0).max(0.0).min(f32::from(max.y).max(0.0));
        let target_x = (52.0 + col as f32 * self.char_w - vw / 2.0).max(0.0).min(f32::from(max.x).max(0.0));
        handle.set_offset(gpui::point(px(-target_x), px(-target_y)));
        // place the caret at the match too
        self.caret = Some((side, row, col));
        self.caret_on = true;
    }

    fn goto(&mut self, idx: usize, cx: &mut Context<Self>) {
        if idx < self.files.len() {
            self.idx = idx;
            let (rows, ls, rs) = compute_diff(&self.root, &self.files[idx], &self.old, &self.new_rev, &self.hl);
            self.rows = rows;
            self.left_styles = ls;
            self.right_styles = rs;
            self.sel = None;
            self.caret = None;
            self.pending_scroll = true; // jump to the first change in the new file
            self.rc_open = false;
            self.load_comments(cx); // notes follow the file being shown
            self.recompute_matches();
            cx.notify();
        }
    }

    /// File indices currently visible in the sidebar (respecting the viewed
    /// filter + text filter), in natural order. Drives prev/next so they walk
    /// the active list only.
    fn visible_files(&self) -> Vec<usize> {
        let q = self.filter.text.trim().to_lowercase();
        (0..self.files.len())
            .filter(|i| {
                if self.pr_hidden.contains(i) {
                    return false;
                }
                if !q.is_empty() {
                    let f = &self.files[*i];
                    let rel = f.strip_prefix(&self.root).unwrap_or(f).to_string_lossy().to_lowercase();
                    if !rel.contains(&q) {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    /// Step to the next/previous visible file (skips filtered-out ones).
    fn goto_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        let vis = self.visible_files();
        if vis.is_empty() {
            return;
        }
        let target = match vis.iter().position(|&i| i == self.idx) {
            Some(p) if forward => vis.get(p + 1).copied(),
            Some(p) => p.checked_sub(1).and_then(|q| vis.get(q).copied()),
            // current file is filtered out → jump to the nearest visible one
            None if forward => vis.iter().find(|&&i| i > self.idx).copied(),
            None => vis.iter().rev().find(|&&i| i < self.idx).copied(),
        };
        if let Some(t) = target {
            self.goto(t, cx);
        }
    }

    /// Row index of the first changed (non-Equal) line, if any.
    fn first_change_row(&self) -> Option<usize> {
        self.rows.iter().position(|r| r.kind != DiffKind::Equal)
    }

    /// Move to the next/previous search match and scroll it into view.
    fn step_match(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        let n = self.matches.len();
        self.cur_match = if forward {
            (self.cur_match + 1) % n
        } else {
            (self.cur_match + n - 1) % n
        };
        self.scroll_to_match();
    }

    fn search_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // cmd+f again, or escape, closes search and returns focus to the diff
        if ks.key == "escape" || (ks.modifiers.platform && ks.key == "f") {
            self.search_open = false;
            window.focus(&self.focus, cx);
            cx.notify();
            return;
        }
        if ks.key == "enter" {
            self.step_match(!ks.modifiers.shift);
            cx.notify();
            return;
        }
        let clip = cx.read_from_clipboard().and_then(|c| c.text());
        let edit = self.search.key(ks, clip, |_| true);
        if let Some(text) = self.search.take_copy() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        if edit == Edit::Changed {
            self.recompute_matches();
            self.cur_match = 0;
            if !self.matches.is_empty() {
                self.scroll_to_match();
            }
        }
        cx.notify();
    }

    /// Map a window-space position to (side, row, col) within the diff content.
    fn cell_at(&self, pos: gpui::Point<gpui::Pixels>) -> Option<(DiffSide, usize, usize)> {
        let lb = self.left_scroll.bounds();
        // pick the side by which pane the cursor is over
        let (side, b, off) = if f32::from(pos.x) < f32::from(lb.right()) {
            (DiffSide::Left, lb, self.left_scroll.offset())
        } else {
            (DiffSide::Right, self.right_scroll.bounds(), self.right_scroll.offset())
        };
        let gutter = 44.0 + 8.0; // line-number cell + text left padding
        let cx_ = f32::from(pos.x) - f32::from(b.left()) - f32::from(off.x);
        let cy = f32::from(pos.y) - f32::from(b.top()) - f32::from(off.y);
        if cx_ < 0.0 || cy < 0.0 {
            return None;
        }
        let row = (cy / 18.0).floor() as usize;
        if row >= self.rows.len() {
            return None;
        }
        let col = (((cx_ - gutter) / self.char_w).floor()).max(0.0) as usize;
        let len = diff_side_text(&self.rows[row], side).chars().count();
        Some((side, row, col.min(len)))
    }

    /// Text of the current selection (joined by newlines).
    fn selected_text(&self) -> Option<String> {
        let s = self.sel.as_ref()?;
        let (a, b) = s.range();
        let mut out = Vec::new();
        for r in a.0..=b.0 {
            let line = diff_side_text(&self.rows[r], s.side);
            let ch: Vec<char> = line.chars().collect();
            let n = ch.len();
            let cs = if r == a.0 { a.1 } else { 0 }.min(n);
            let ce = if r == b.0 { b.1 } else { n }.min(n);
            out.push(ch[cs..ce].iter().collect::<String>());
        }
        Some(out.join("\n"))
    }

    /// Map the diff's caret (or selection head) to a 1-based (line, col) in the
    /// real file, so F4 can open the editor at the same spot. Prefers the side's
    /// own line number, falling back to the other side for added/removed rows.
    fn cursor_file_pos(&self) -> (usize, usize) {
        let (side, row, col) = if let Some((s, r, c)) = self.caret {
            (s, r, c)
        } else if let Some(sel) = &self.sel {
            (sel.side, sel.head.0, sel.head.1)
        } else {
            (DiffSide::Right, 0, 0)
        };
        let line = self
            .rows
            .get(row)
            .and_then(|dr| match side {
                DiffSide::Left => dr.left_no.or(dr.right_no),
                DiffSide::Right => dr.right_no.or(dr.left_no),
            })
            .unwrap_or(1);
        (line, col + 1)
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // alt+c comments on the caret's line — checked before the input-focus
        // bail-out below, or it would never fire while the file filter has focus
        // (which it usually does after opening the window). Not while the comment
        // box itself has focus: there ⌥C is just text.
        if ks.modifiers.alt && ks.key == "c" && !(self.rc_open && self.rc_focus.is_focused(window)) {
            self.open_comment(window, cx);
            return;
        }
        // when one of the inputs has focus, let its own handler own the keystroke
        // (events bubble up to this root otherwise, so ⌘A would select the whole
        // file behind the comment box instead of the text being typed)
        if (self.search_open && self.search_focus.is_focused(window))
            || self.filter_focus.is_focused(window)
            || (self.rc_open && self.rc_focus.is_focused(window))
        {
            return;
        }
        // cmd+f opens the search bar
        if ks.modifiers.platform && ks.key == "f" {
            self.search_open = true;
            window.focus(&self.search_focus, cx);
            cx.notify();
            return;
        }
        // cmd+g / cmd+shift+g step through matches
        if ks.modifiers.platform && ks.key == "g" {
            self.step_match(!ks.modifiers.shift);
            cx.notify();
            return;
        }
        // cmd+a selects all text on the active side (left or right)
        if ks.modifiers.platform && ks.key == "a" {
            if !self.rows.is_empty() {
                // pick the side you're working in (caret → selection → right)
                let side = self
                    .caret
                    .map(|(s, _, _)| s)
                    .or(self.sel.as_ref().map(|s| s.side))
                    .unwrap_or(DiffSide::Right);
                let last = self.rows.len() - 1;
                let last_len = diff_side_text(&self.rows[last], side).chars().count();
                self.sel = Some(DiffSel { side, anchor: (0, 0), head: (last, last_len) });
                cx.notify();
            }
            return;
        }
        // cmd+c copies the current selection
        if ks.modifiers.platform && ks.key == "c" {
            if let Some(text) = self.selected_text() {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            return;
        }
        // F4: close the diff and open the real file in the main app window, at
        // the same line/column the caret was on here (centered in view)
        if ks.key == "f4" {
            let path = self.files[self.idx].clone();
            let (line, col) = self.cursor_file_pos();
            window.remove_window();
            if path.is_file() {
                if let Some(handle) = self.main_window {
                    let storm = self.storm.clone();
                    cx.update_window(handle, move |_, window, cx| {
                        window.activate_window();
                        storm
                            .update(cx, |s, cx| {
                                s.open_file(path, window, cx);
                                if let Some(tab) = s.tabs.get(s.active) {
                                    tab.editor.update(cx, |e, cx| e.goto(line, col, cx));
                                }
                            })
                            .ok();
                    })
                    .ok();
                }
            }
            return;
        }
        match ks.key.as_str() {
            "escape" => {
                if self.search_open {
                    self.search_open = false;
                    cx.notify();
                } else {
                    window.remove_window();
                }
            }
            "down" | "right" => self.goto_step(true, cx),
            "up" | "left" => self.goto_step(false, cx),
            _ => {}
        }
    }

    /// The cmd+f search bar (below the nav bar): query input + match counter.
    fn render_search_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = if self.matches.is_empty() {
            "No results".to_string()
        } else {
            format!("{}/{}", self.cur_match + 1, self.matches.len())
        };
        div()
            .h(px(32.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .track_focus(&self.search_focus)
            .on_key_down(cx.listener(Self::search_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e, window, cx| {
                    window.focus(&this.search_focus, cx);
                    cx.notify();
                }),
            )
            .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(MUTED)).child(IC_SEARCH))
            .child(
                div()
                    .flex_grow(1.0)
                    .font_family("Menlo")
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .child(if self.search.is_empty() {
                        div().text_color(rgb(MUTED)).child("Find in diff…▏").into_any_element()
                    } else {
                        self.search.render("▏", SELECTION)
                    }),
            )
            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(count))
            .child(
                div()
                    .id("dw-find-prev")
                    .px_2()
                    .cursor_pointer()
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(ACCENT)))
                    .child("‹")
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.step_match(false);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("dw-find-next")
                    .px_2()
                    .cursor_pointer()
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(ACCENT)))
                    .child("›")
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.step_match(true);
                        cx.notify();
                    })),
            )
    }
}

impl Render for DiffWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            window.focus(&self.focus, cx);
            window.activate_window(); // become the key window so scroll works without a click
        }
        // measure the real monospace advance so columns + selection are precise
        let run = TextRun {
            len: 1,
            font: font("Menlo"),
            color: rgb(TEXT).into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        self.char_w = f32::from(window.text_system().shape_line("0".into(), px(13.), &[run], None).width);
        let path = &self.files[self.idx];
        let rel = path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().to_string();
        // position within the currently-visible (filtered) list
        let vis = self.visible_files();
        let vpos = vis.iter().position(|&i| i == self.idx);
        let pos = format!("{}/{}", vpos.map(|p| p + 1).unwrap_or(0), vis.len());
        let has_prev = vpos.map(|p| p > 0).unwrap_or(!vis.is_empty());
        let has_next = vpos.map(|p| p + 1 < vis.len()).unwrap_or(!vis.is_empty());
        // count changed hunks (contiguous non-Equal runs), shown like WebStorm
        let diffs = {
            let mut n = 0usize;
            let mut in_hunk = false;
            for row in &self.rows {
                let changed = row.kind != DiffKind::Equal;
                if changed && !in_hunk {
                    n += 1;
                }
                in_hunk = changed;
            }
            n
        };

        let bar = div()
            .h(px(34.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("dw-prev")
                    .px_2()
                    .cursor_pointer()
                    .text_color(rgb(if has_prev { TEXT } else { MUTED }))
                    .hover(|s| s.text_color(rgb(ACCENT)))
                    .child("‹")
                    .on_click(cx.listener(|this, _e, _w, cx| this.goto_step(false, cx))),
            )
            .child(
                div()
                    .id("dw-next")
                    .px_2()
                    .cursor_pointer()
                    .text_color(rgb(if has_next { TEXT } else { MUTED }))
                    .hover(|s| s.text_color(rgb(ACCENT)))
                    .child("›")
                    .on_click(cx.listener(|this, _e, _w, cx| this.goto_step(true, cx))),
            )
            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(pos))
            .child(div().flex_grow(1.0).text_color(rgb(TEXT)).text_size(px(12.)).child(rel))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child(format!("{} difference{}", diffs, if diffs == 1 { "" } else { "s" })),
            )
            .child(
                // the review-comment affordance isn't discoverable otherwise
                div()
                    .text_size(px(11.))
                    .text_color(rgb(if self.comments.is_empty() { LINE_NUMBER } else { COMMENT_MARK }))
                    .child(match self.comments.len() {
                        0 => "⌥C comment".to_string(),
                        n => format!("{n} comments · ⌥C comment"),
                    }),
            )
            .child(
                div()
                    .id("dw-close")
                    .px_2()
                    .cursor_pointer()
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(GIT_DELETED)))
                    .child("✕")
                    .on_click(cx.listener(|_this, _e, window, _cx| window.remove_window())),
            );

        let col = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .font_family("Inter")
            .text_size(px(13.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_scroll_wheel(cx.listener(|_this, _e, _w, cx| cx.notify()))
            // click to place caret / drag to select; double-click selects a word
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                    if let Some((side, r, c)) = this.cell_at(ev.position) {
                        if ev.click_count >= 2 {
                            let line = this.rows.get(r).map(|row| diff_side_text(row, side)).unwrap_or_default();
                            let (s, e) = word_range(&line, c);
                            this.sel = Some(DiffSel { side, anchor: (r, s), head: (r, e) });
                            this.caret = Some((side, r, e));
                            this.dragging = false;
                        } else {
                            this.sel = Some(DiffSel { side, anchor: (r, c), head: (r, c) });
                            this.caret = Some((side, r, c));
                            this.dragging = true;
                        }
                        this.caret_on = true;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, window, cx| {
                if this.resizing_sidebar {
                    this.sidebar_w = f32::from(ev.position.x).clamp(160., 640.);
                    cx.notify();
                    return;
                }
                if this.resizing_pane {
                    // drag the divider: split = cursor x within the diff area
                    // (which starts after the sidebar, when one is shown)
                    let sidebar_w = if this.files.len() > 1 { this.sidebar_w + 4.0 } else { 0.0 };
                    let area_w = (f32::from(window.viewport_size().width) - sidebar_w).max(1.0);
                    let x = f32::from(ev.position.x) - sidebar_w;
                    this.pane_split = (x / area_w).clamp(0.15, 0.85);
                    cx.notify();
                    return;
                }
                if this.dragging {
                    if let Some((side, r, c)) = this.cell_at(ev.position) {
                        if let Some(s) = &mut this.sel {
                            if s.side == side {
                                s.head = (r, c);
                                cx.notify();
                            }
                        }
                    }
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _e, _w, cx| {
                this.dragging = false;
                if this.resizing_pane || this.resizing_sidebar {
                    this.resizing_pane = false;
                    this.resizing_sidebar = false;
                    cx.notify();
                }
            }))
            .child(bar)
            .when(self.search_open, |d| d.child(self.render_search_bar(cx)));

        // on (re)load, snap both panes to the first change with a little context
        // above it, so the diff opens on the change instead of the file top
        if self.pending_scroll {
            self.pending_scroll = false;
            if let Some(row) = self.first_change_row() {
                let y = -(row.saturating_sub(3) as f32 * 18.0);
                self.left_scroll.set_offset(gpui::point(px(0.), px(y)));
                self.right_scroll.set_offset(gpui::point(px(0.), px(y)));
            }
        }

        // two panes with a draggable divider between them (pane_split = left share)
        let split = self.pane_split;
        // visible width of each pane: comment widgets are sized to this so they
        // stay fully on screen instead of extending into the scrollable width
        let sidebar = if self.files.len() > 1 { self.sidebar_w + 4.0 } else { 0.0 };
        let area_w = (f32::from(window.viewport_size().width) - sidebar - 4.0).max(120.0);
        let (left_vis, right_vis) = (area_w * split - 10.0, area_w * (1.0 - split) - 10.0);
        // the inline comment box lives in the right pane, under its row
        let comment_box = self
            .rc_open
            .then(|| {
                let replies = self.rc_replies.clone();
                let focused = self.rc_focus.is_focused(window);
                (self.rc_row, rc_gap_h(&self.rc_query.text, &replies), self.render_diff_comment_box(focused, cx))
            });
        let left_pane = diff_pane(&self.rows, DiffSide::Left, &self.left_scroll, self.char_w, self.sel.as_ref(), &self.left_styles, &self.matches, self.cur_match, self.caret, self.caret_on, &[], None, left_vis.max(120.0));
        let right_pane = diff_pane(&self.rows, DiffSide::Right, &self.right_scroll, self.char_w, self.sel.as_ref(), &self.right_styles, &self.matches, self.cur_match, self.caret, self.caret_on, &self.comments, comment_box, right_vis.max(120.0));
        let divider = div()
            .w(px(4.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(if self.resizing_pane { ACCENT } else { BORDER }))
            .cursor(CursorStyle::ResizeLeftRight)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                    this.resizing_pane = true;
                    cx.stop_propagation(); // don't also start a text selection
                    cx.notify();
                }),
            );
        // explicit pixel widths (flex_grow split was unreliable — a pane's content
        // min-width leaked through and skewed the ratio). Compute from the window:
        let win_w = f32::from(window.viewport_size().width);
        let sidebar_w = if self.files.len() > 1 { 260.0 } else { 0.0 };
        let avail = (win_w - sidebar_w - 4.0).max(2.0); // minus the 4px divider
        let lw = (avail * split).max(1.0);
        let rw = (avail - lw).max(1.0);
        let body = div()
            .flex()
            .flex_row()
            .flex_grow(1.0)
            .min_h(px(0.))
            .child(div().w(px(lw)).h_full().flex_shrink_0().overflow_hidden().child(left_pane))
            .child(divider)
            .child(div().w(px(rw)).h_full().flex_shrink_0().overflow_hidden().child(right_pane));

        // file-tree sidebar (only for a multi-file diff): collapsible directories,
        // click a file to jump to it, current file highlighted
        if self.files.len() > 1 {
            // PR mode: use the local viewed snapshot (reading Storm here can
            // reentrantly panic — cmd+d opens this window mid-Storm-update)
            let viewed = self.pr_viewed.clone();
            if self.pr_mode {
                self.pr_hidden = self
                    .files
                    .iter()
                    .enumerate()
                    .filter_map(|(i, f)| {
                        let v = viewed.contains(f);
                        let hide = match self.pr_filter {
                            PrViewFilter::All => false,
                            PrViewFilter::Unviewed => v,
                            PrViewFilter::Viewed => !v,
                        };
                        hide.then_some(i)
                    })
                    .collect();
            } else {
                self.pr_hidden.clear();
            }
            let viewed_count = self.files.iter().filter(|f| viewed.contains(*f)).count();
            let total = self.files.len();
            let pct = if total == 0 { 0 } else { viewed_count * 100 / total };
            // fixed filter input (top of the sidebar)
            let filter_row = div()
                .id("dw-filter")
                .h(px(28.))
                .px_3()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .border_b_1()
                .border_color(rgb(BORDER))
                .track_focus(&self.filter_focus)
                .on_key_down(cx.listener(Self::filter_key))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e, window, cx| {
                        window.focus(&this.filter_focus, cx);
                        cx.notify();
                    }),
                )
                .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(MUTED)).child(IC_SEARCH))
                .child(if self.filter.is_empty() {
                    div().text_size(px(12.)).text_color(rgb(MUTED)).child("Filter files…")
                } else {
                    div().text_size(px(12.)).text_color(rgb(TEXT)).child(self.filter.render("▏", SELECTION))
                });
            // the scrolling row list
            let mut list = div()
                .id("dw-files")
                .flex_grow(1.0)
                .min_h(px(0.))
                .flex()
                .flex_col()
                .overflow_y_scroll();
            // All/Unviewed/Viewed chips (fixed, PR mode)
            let chips = self.pr_mode.then(|| {
                let chip = |label: &'static str, f: PrViewFilter| {
                    let on = self.pr_filter == f;
                    div()
                        .id(label)
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .text_size(px(11.))
                        .cursor_pointer()
                        .when(on, |d| d.bg(rgb(ACCENT)).text_color(rgb(SEL_TEXT)))
                        .when(!on, |d| d.text_color(rgb(MUTED)).hover(|s| s.bg(rgb(HOVER))))
                        .child(label)
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.pr_filter = f;
                            cx.notify();
                        }))
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(chip("All", PrViewFilter::All))
                    .child(chip("Unviewed", PrViewFilter::Unviewed))
                    .child(chip("Viewed", PrViewFilter::Viewed))
            });
            // progress bar (viewed / total) — pinned at the bottom (PR mode)
            let progress = self.pr_mode.then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(format!("{viewed_count}/{total} viewed"))
                            .child(format!("{pct}%")),
                    )
                    .child({
                        let bar_w = 236.0_f32; // ~sidebar width minus padding
                        div()
                            .w(px(bar_w))
                            .h(px(4.))
                            .rounded_sm()
                            .bg(rgb(HOVER))
                            .child(
                                div()
                                    .w(px(bar_w * pct as f32 / 100.0))
                                    .h_full()
                                    .rounded_sm()
                                    .bg(rgb(GIT_NEW)),
                            )
                    })
            });
            for (ri, row) in self.tree_rows().into_iter().enumerate() {
                let indent = 8.0 + row.depth as f32 * 14.0;
                let sel = row.file_idx == Some(self.idx);
                let is_dir = row.dir.is_some();
                let collapsed = row.dir.as_ref().is_some_and(|p| self.tree_collapsed.contains(p));
                let dir_path = row.dir.clone();
                let file_idx = row.file_idx;
                let file_path = file_idx.map(|i| self.files[i].clone());
                let is_viewed = file_path.as_ref().is_some_and(|p| viewed.contains(p));
                let show_check = self.pr_mode && file_path.is_some();
                let check_path = file_path.clone();
                // file-type badge (files) / file-count (dirs), like the PR tree
                let badge = file_path.as_ref().map(|p| ext_badge(p));
                let dir_files: Vec<PathBuf> = dir_path
                    .as_ref()
                    .map(|dp| {
                        self.files
                            .iter()
                            .filter(|f| f.strip_prefix(&self.root).map(|r| r.starts_with(dp)).unwrap_or(false))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                let dir_count = dir_path.as_ref().map(|_| dir_files.len());
                let dir_viewed = !dir_files.is_empty() && dir_files.iter().all(|p| viewed.contains(p));
                let show_dir_check = self.pr_mode && is_dir;
                list = list.child(
                    div()
                        .id(("dw-row", ri))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .h(px(22.))
                        .pl(px(indent))
                        .pr_2()
                        .flex_shrink_0()
                        .cursor_pointer()
                        .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                        .when(!sel, |d| d.hover(|s| s.bg(rgb(HOVER))))
                        .child(
                            div()
                                .w(px(12.))
                                .flex()
                                .justify_center()
                                .text_size(px(11.))
                                .text_color(rgb(DIR))
                                .child(if is_dir { if collapsed { "▸" } else { "▾" } } else { "" }),
                        )
                        // folder viewed checkbox (PR mode) — marks all files under it
                        .when(show_dir_check, |d| {
                            let files = dir_files.clone();
                            d.child(
                                div()
                                    .id(("dw-dcheck", ri))
                                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        let all = !files.is_empty() && files.iter().all(|p| this.pr_viewed.contains(p));
                                        for p in &files {
                                            if all {
                                                this.pr_viewed.remove(p);
                                            } else {
                                                this.pr_viewed.insert(p.clone());
                                            }
                                        }
                                        if let Some(storm) = this.storm.upgrade() {
                                            let files = files.clone();
                                            storm.update(cx, |s, _| {
                                                // Storm::toggle_pr_viewed_all flips based on its own
                                                // state; set each explicitly to match our target
                                                for p in &files {
                                                    let has = s.pr_viewed.contains(p);
                                                    if all && has || !all && !has {
                                                        s.toggle_pr_viewed(p.clone());
                                                    }
                                                }
                                            });
                                        }
                                        cx.notify();
                                    }))
                                    .child(check_box(dir_viewed)),
                            )
                        })
                        // viewed checkbox (PR mode, file rows) — toggles + syncs to gh
                        .when(show_check, |d| {
                            d.child(
                                div()
                                    .id(("dw-check", ri))
                                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        if let Some(p) = check_path.clone() {
                                            // update local snapshot + push to the main window (gh sync)
                                            if !this.pr_viewed.remove(&p) {
                                                this.pr_viewed.insert(p.clone());
                                            }
                                            if let Some(storm) = this.storm.upgrade() {
                                                storm.update(cx, |s, _| s.toggle_pr_viewed(p));
                                            }
                                            cx.notify();
                                        }
                                    }))
                                    .child(check_box(is_viewed)),
                            )
                        })
                        .when(is_dir, |d| {
                            d.child(
                                div()
                                    .font_family(ICON_FONT)
                                    .text_size(px(12.))
                                    .text_color(rgb(FOLDER_ICON))
                                    .child(IC_FOLDER),
                            )
                        })
                        // file-type badge (colored, like the PR tree)
                        .when_some(badge, |d, (b, c)| {
                            d.child(
                                div()
                                    .w(px(16.))
                                    .flex()
                                    .justify_center()
                                    .text_size(px(9.))
                                    .text_color(rgb(if sel { SEL_TEXT } else { c }))
                                    .child(b),
                            )
                        })
                        .child(
                            div()
                                .flex_grow(1.0)
                                .truncate()
                                .text_size(px(13.))
                                // colour file names by git status, like the trees
                                // in the main window
                                .text_color(rgb(if sel {
                                    SEL_TEXT
                                } else if is_dir {
                                    DIR
                                } else {
                                    match row.file_idx.and_then(|i| self.files.get(i)).and_then(|p| self.states.get(p)) {
                                        Some(GitState::New) => GIT_NEW,
                                        Some(GitState::Modified) => GIT_MODIFIED,
                                        Some(GitState::Deleted) => GIT_DELETED,
                                        None => TEXT,
                                    }
                                }))
                                .child(row.label),
                        )
                        // review comments on this file
                        .when_some(
                            row.file_idx
                                .and_then(|i| self.files.get(i))
                                .map(|p| self.comment_counts.get(p).copied().unwrap_or(0))
                                .filter(|n| *n > 0),
                            |d, n| d.child(comment_marker(n, 11.)),
                        )
                        // dir file count on the right
                        .when_some(dir_count, |d, n| {
                            d.child(div().text_size(px(10.)).text_color(rgb(MUTED)).child(format!("{n}")))
                        })
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            if let Some(p) = &dir_path {
                                if !this.tree_collapsed.remove(p) {
                                    this.tree_collapsed.insert(p.clone());
                                }
                                cx.notify();
                            } else if let Some(i) = file_idx {
                                this.goto(i, cx);
                            }
                        })),
                );
            }
            // sidebar: fixed filter + chips on top, scrolling list, progress pinned
            // at the bottom
            let sidebar = div()
                .w(px(self.sidebar_w))
                .flex_shrink_0()
                .h_full()
                .flex()
                .flex_col()
                .bg(rgb(PANEL_BG))
                .border_r_1()
                .border_color(rgb(BORDER))
                .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                .child(filter_row)
                .children(chips)
                .child(list)
                .children(progress);
            // draggable edge between the sidebar and the two diff panes
            let sidebar_divider = div()
                .w(px(4.))
                .h_full()
                .flex_shrink_0()
                .bg(rgb(if self.resizing_sidebar { ACCENT } else { BORDER }))
                .cursor(CursorStyle::ResizeLeftRight)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _e: &MouseDownEvent, _w, cx| {
                        this.resizing_sidebar = true;
                        cx.stop_propagation();
                        cx.notify();
                    }),
                );
            // body added directly as a flex child so it keeps a bounded height
            // (scroll panes need that); a non-flex wrapper here breaks scrolling
            let content = div()
                .flex()
                .flex_row()
                .flex_grow(1.0)
                .min_h(px(0.))
                .child(sidebar)
                .child(sidebar_divider)
                .child(body);
            col.child(content)
        } else {
            col.child(body)
        }
    }
}

/// 3-way merge editor in its own window: Yours | Result (editable) | Theirs.
struct MergeWindow {
    root: PathBuf,
    path: PathBuf,
    ours: Entity<Editor>,
    result: Entity<Editor>,
    theirs: Entity<Editor>,
    ours_text: String,
    theirs_text: String,
    focus: FocusHandle,
    focused: bool,
    storm: WeakEntity<Storm>,
    main_window: Option<AnyWindowHandle>,
}

impl MergeWindow {
    #[allow(clippy::too_many_arguments)]
    fn new(
        root: PathBuf,
        path: PathBuf,
        ours_text: String,
        theirs_text: String,
        result_text: String,
        storm: WeakEntity<Storm>,
        main_window: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let ours = cx.new(|c| Editor::new(None, c));
        let theirs = cx.new(|c| Editor::new(None, c));
        let result = cx.new(|c| Editor::new(None, c));
        ours.update(cx, |e, c| e.set_log(ours_text.clone(), c));
        theirs.update(cx, |e, c| e.set_log(theirs_text.clone(), c));
        result.update(cx, |e, c| {
            e.set_read_only(false); // center pane is hand-editable
            e.set_log(result_text, c);
        });
        Self {
            root,
            path,
            ours,
            result,
            theirs,
            ours_text,
            theirs_text,
            focus: cx.focus_handle(),
            focused: false,
            storm,
            main_window,
        }
    }

    /// Write the result to disk, stage it, tell the main window to re-scan
    /// conflicts, and close this window.
    fn resolve(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.result.read(cx).text();
        let _ = std::fs::write(&self.path, text);
        let rel = self.path.strip_prefix(&self.root).unwrap_or(&self.path).to_string_lossy().to_string();
        let _ = Command::new("git").args(["add", "--", &rel]).current_dir(&self.root).output();
        if let Some(storm) = self.storm.upgrade() {
            storm.update(cx, |s, cx| s.mc_reload(cx));
        }
        if let Some(main) = self.main_window {
            let _ = main.update(cx, |_, window, _| window.activate_window());
        }
        window.remove_window();
    }
}

impl Render for MergeWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused {
            self.focused = true;
            window.activate_window();
        }
        let btn = |id: &'static str, label: &'static str, accent: bool| {
            div()
                .id(id)
                .px_3()
                .py_1()
                .rounded_md()
                .text_size(px(12.))
                .cursor_pointer()
                .border_1()
                .border_color(rgb(if accent { ACCENT } else { BORDER }))
                .text_color(rgb(if accent { SEL_TEXT } else { TEXT }))
                .when(accent, |d| d.bg(rgb(ICON_SELECTED_BG)))
                .hover(|s| s.bg(rgb(HOVER)))
                .child(label)
        };
        let col = |title: &'static str, editor: Entity<Editor>| {
            div()
                .flex()
                .flex_grow(1.0)
                .min_w(px(0.))
                .flex_col()
                .border_r_1()
                .border_color(rgb(BORDER))
                .child(
                    div()
                        .h(px(24.))
                        .px_2()
                        .flex()
                        .items_center()
                        .flex_shrink_0()
                        .bg(rgb(PANEL_BG))
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .text_size(px(11.))
                        .text_color(rgb(MUTED))
                        .child(title),
                )
                .child(div().flex_grow(1.0).min_h(px(0.)).child(editor))
        };

        let ours_text = self.ours_text.clone();
        let theirs_text = self.theirs_text.clone();
        let toolbar = div()
            .h(px(34.))
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(btn("mw-ours", "◀ Take all Yours", false).on_click(cx.listener(move |this, _e, _w, cx| {
                let t = ours_text.clone();
                this.result.update(cx, |e, cx| e.set_log(t, cx));
            })))
            .child(btn("mw-theirs", "Take all Theirs ▶", false).on_click(cx.listener(move |this, _e, _w, cx| {
                let t = theirs_text.clone();
                this.result.update(cx, |e, cx| e.set_log(t, cx));
            })))
            .child(div().flex_grow(1.0))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .child("edit the middle pane freely · <<<< markers mark conflicts"),
            )
            .child(btn("mw-resolve", "Resolve", true).on_click(cx.listener(|this, _e, window, cx| {
                this.resolve(window, cx);
            })))
            .child(
                div()
                    .id("mw-close")
                    .px_2()
                    .cursor_pointer()
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(GIT_DELETED)))
                    .child("✕")
                    .on_click(cx.listener(|_this, _e, window, _cx| window.remove_window())),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .font_family("Inter")
            .text_size(px(13.))
            .track_focus(&self.focus)
            .child(toolbar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_grow(1.0)
                    .min_h(px(0.))
                    .child(col("Yours", self.ours.clone()))
                    .child(col("Result (editable)", self.result.clone()))
                    .child(col("Theirs", self.theirs.clone())),
            )
    }
}

/// Order paths the way a person reads them: `wby-next2` before `wby-next10`.
/// A plain sort (and shell glob expansion) puts 10 right after 1, which then
/// shows up as the workspace numbering in the switcher.
fn natural_key(p: &Path) -> Vec<(u64, String)> {
    let s = p.to_string_lossy().to_string();
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    let mut text = String::new();
    while let Some(c) = chars.next() {
        if c.is_ascii_digit() {
            let mut n = c.to_digit(10).unwrap() as u64;
            while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                n = n.saturating_mul(10).saturating_add(d as u64);
                chars.next();
            }
            out.push((n, std::mem::take(&mut text)));
        } else {
            text.push(c);
        }
    }
    if !text.is_empty() {
        out.push((u64::MAX, text)); // trailing text sorts after any number run
    }
    out
}

/// Resolve one CLI argument to project directories. `~` is expanded, and a
/// `*`/`?` pattern is matched against its parent directory's entries — so
/// `tide 'wby-next*'` works even when the shell didn't expand it (quoted, or
/// run from a directory where nothing matched).
fn resolve_arg(arg: &str) -> Vec<PathBuf> {
    let expanded = match arg.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME").map(|h| PathBuf::from(h).join(rest)).unwrap_or_else(|_| PathBuf::from(arg)),
        None => PathBuf::from(arg),
    };
    let as_str = expanded.to_string_lossy().to_string();
    if !(as_str.contains('*') || as_str.contains('?')) {
        return std::fs::canonicalize(&expanded).ok().filter(|p| p.is_dir()).into_iter().collect();
    }
    let parent = expanded.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf());
    let dir = parent.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let pattern = expanded.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .map(|n| glob_match(&pattern, &n.to_string_lossy()))
                .unwrap_or(false)
        })
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    hits.sort_by_key(|p| natural_key(p));
    hits
}

fn main() {
    // Each CLI arg is a project root (globs allowed) opened as its own
    // workspace. With no args, reopen last session's workspaces; if there is no
    // session yet, fall back to the current directory.
    let roots: Vec<PathBuf> = {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut v: Vec<PathBuf> = args.iter().flat_map(|a| resolve_arg(a)).collect();
        // a single glob arg is already naturally sorted; a shell-expanded list
        // isn't (wby-next10 lands after wby-next1), so sort when it looks like one
        if args.len() > 1 {
            v.sort_by_key(|p| natural_key(p));
        }
        v.dedup();
        if v.is_empty() && args.is_empty() {
            v = load_session();
        }
        if v.is_empty() {
            v.push(std::env::current_dir().unwrap());
        }
        v
    };

    application().run(move |cx: &mut App| {
        // bundle the Codicon icon font + Inter (UI font)
        cx.text_system()
            .add_fonts(vec![
                std::borrow::Cow::Borrowed(include_bytes!("../assets/codicon.ttf").as_slice()),
                std::borrow::Cow::Borrowed(include_bytes!("../assets/Inter-Regular.otf").as_slice()),
                std::borrow::Cow::Borrowed(include_bytes!("../assets/Inter-Medium.otf").as_slice()),
                std::borrow::Cow::Borrowed(include_bytes!("../assets/Inter-SemiBold.otf").as_slice()),
                std::borrow::Cow::Borrowed(include_bytes!("../assets/Inter-Bold.otf").as_slice()),
            ])
            .ok();

        cx.bind_keys([
            KeyBinding::new("backspace", Backspace, Some("Editor")),
            KeyBinding::new("delete", Delete, Some("Editor")),
            KeyBinding::new("left", MoveLeft, Some("Editor")),
            KeyBinding::new("right", MoveRight, Some("Editor")),
            KeyBinding::new("up", MoveUp, Some("Editor")),
            KeyBinding::new("down", MoveDown, Some("Editor")),
            KeyBinding::new("home", Home, Some("Editor")),
            KeyBinding::new("end", End, Some("Editor")),
            KeyBinding::new("cmd-left", Home, Some("Editor")),
            KeyBinding::new("cmd-right", End, Some("Editor")),
            KeyBinding::new("enter", Newline, Some("Editor")),
            KeyBinding::new("tab", Indent, Some("Editor")),
            KeyBinding::new("cmd-s", Save, Some("Editor")),
            // completion
            KeyBinding::new("ctrl-space", CompTrigger, Some("Editor")),
            KeyBinding::new("escape", CompDismiss, Some("Editor")),
            // go to definition
            KeyBinding::new("cmd-down", GotoDef, Some("Editor")),
            KeyBinding::new("cmd-b", GotoDef, Some("Editor")),
            KeyBinding::new("cmd-enter", EnableEdit, Some("Editor")),
            // in-file search
            KeyBinding::new("cmd-f", SearchOpen, Some("Editor")),
            // copy reference (relpath:line)
            KeyBinding::new("cmd-shift-c", CopyReference, Some("Editor")),
            KeyBinding::new("cmd-shift-alt-c", CopyReferenceLine, Some("Editor")),
            KeyBinding::new("cmd-w", CloseTab, Some("Editor")),
            KeyBinding::new("cmd-shift-w", CloseOthers, Some("Editor")),
            // selection
            KeyBinding::new("shift-left", SelectLeft, Some("Editor")),
            KeyBinding::new("shift-right", SelectRight, Some("Editor")),
            KeyBinding::new("shift-up", SelectUp, Some("Editor")),
            KeyBinding::new("shift-down", SelectDown, Some("Editor")),
            KeyBinding::new("cmd-shift-left", SelectHome, Some("Editor")),
            KeyBinding::new("cmd-shift-right", SelectEnd, Some("Editor")),
            KeyBinding::new("cmd-a", SelectAll, Some("Editor")),
            // clipboard
            KeyBinding::new("cmd-c", Copy, Some("Editor")),
            KeyBinding::new("cmd-v", Paste, Some("Editor")),
            KeyBinding::new("cmd-x", Cut, Some("Editor")),
            // word movement
            KeyBinding::new("alt-left", WordLeft, Some("Editor")),
            KeyBinding::new("alt-right", WordRight, Some("Editor")),
            KeyBinding::new("alt-shift-left", SelectWordLeft, Some("Editor")),
            KeyBinding::new("alt-shift-right", SelectWordRight, Some("Editor")),
            KeyBinding::new("alt-backspace", DeleteWordLeft, Some("Editor")),
            KeyBinding::new("alt-delete", DeleteWordRight, Some("Editor")),
            // undo / redo
            KeyBinding::new("cmd-z", Undo, Some("Editor")),
            KeyBinding::new("cmd-shift-z", Redo, Some("Editor")),
            // line ops
            KeyBinding::new("cmd-backspace", DeleteLine, Some("Editor")),
            // duplicate line / toggle comment (Editor beats the global cmd-d)
            KeyBinding::new("cmd-d", DupLine, Some("Editor")),
            KeyBinding::new("cmd-/", ToggleComment, Some("Editor")),
            KeyBinding::new("alt-shift-up", MoveLineUp, Some("Editor")),
            KeyBinding::new("alt-shift-down", MoveLineDown, Some("Editor")),
            // terminal toggle (global)
            KeyBinding::new("alt-f12", ToggleTerminal, None),
            // terminal tab management (when terminal focused)
            // global, not Terminal-scoped: the first ⌘T is what opens the thing
            KeyBinding::new("cmd-t", NewTerminal, None),
            KeyBinding::new("cmd-w", CloseTerminalTab, Some("Terminal")),
            KeyBinding::new("cmd-shift-w", CloseOtherTerminals, Some("Terminal")),
            // fuzzy file finder (global)
            KeyBinding::new("cmd-shift-o", OpenFinder, None),
            // go to line (global)
            KeyBinding::new("cmd-l", GotoLine, None),
            // the commit pane is a palette command now — ⌘K opens the palette
            KeyBinding::new("cmd-shift-k", PushDialog, None),
            KeyBinding::new("cmd-shift-t", SetWsTitle, None),
            // find in files (global)
            KeyBinding::new("cmd-shift-f", FindInFiles, None),
            // git branches/actions popup (global)
            KeyBinding::new("alt-b", GitPopup, None),
            KeyBinding::new("alt-f", FetchRemotes, None),
            KeyBinding::new("cmd-shift-m", WipPush, None),
            KeyBinding::new("cmd-shift-b", RunBuild, None),
            KeyBinding::new("cmd-shift-a", ToggleAgent, None),
            KeyBinding::new("cmd-9", GitLog, None),
            KeyBinding::new("alt-h", GitLog, None),
            KeyBinding::new("alt-l", PullRemote, None),
            // command palette (global)
            KeyBinding::new("cmd-k", CommandPalette, None),
            KeyBinding::new("cmd-shift-h", OpenOnGithub, None),
            // multi-project workspace (global)
            KeyBinding::new("alt-tab", NextProject, None),
            KeyBinding::new("alt-shift-tab", PrevProject, None),
            KeyBinding::new("cmd-o", NewProject, None),
            KeyBinding::new("cmd-shift-n", NewScratch, None),
            KeyBinding::new("cmd-e", ShowProjects, None),
            // review comments: pin one to a line, list them, send the batch
            KeyBinding::new("alt-c", AddComment, None),
            // the agent pane's ⌥C quotes the selected answer text instead of
            // starting a review comment (deeper context wins)
            KeyBinding::new("alt-c", QuoteAgentSel, Some("Agent")),
            KeyBinding::new("alt-shift-c", ShowComments, None),
            KeyBinding::new("alt-enter", SendComments, None),
            KeyBinding::new("cmd-f12", SendComments, None),
            KeyBinding::new("cmd-q", QuitApp, None),
            KeyBinding::new("cmd-shift-v", ToggleMdPreview, None),
            // refresh PR status + admin health (topbar chip and status screen)
            KeyBinding::new("cmd-r", RefreshPr, None),
            // diff (opens in its own window)
            KeyBinding::new("cmd-d", ShowDiff, None),
            // terminal pane splits — listed after the global cmd-d so they win
            // while a terminal has focus (later binding takes precedence)
            KeyBinding::new("cmd-d", SplitTermRight, Some("Terminal")),
            KeyBinding::new("cmd-shift-d", SplitTermDown, Some("Terminal")),
            // zoom the focused pane to the whole dock
            KeyBinding::new("cmd-shift-enter", ToggleTermZoom, Some("Terminal")),
            // pane navigation (while zoomed these cycle prev/next instead)
            KeyBinding::new("cmd-left", FocusPaneLeft, Some("Terminal")),
            KeyBinding::new("cmd-right", FocusPaneRight, Some("Terminal")),
            KeyBinding::new("cmd-up", FocusPaneUp, Some("Terminal")),
            KeyBinding::new("cmd-down", FocusPaneDown, Some("Terminal")),
        ]);

        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("tide".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| Workspace::new(roots.clone(), cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}

#[cfg(test)]
mod short_cmd_tests {
    use super::*;

    #[test]
    fn interpreters_and_paths_are_stripped() {
        assert_eq!(short_cmd("node /usr/local/bin/yarn webiny-server watch admin"), "yarn webiny-server watch admin");
        assert_eq!(short_cmd("/Users/adrian/.local/bin/claude --dangerously-skip-permissions --continue"), "claude");
        assert_eq!(short_cmd("/bin/zsh"), "zsh"); // a bare shell keeps its name
        assert_eq!(short_cmd("npm exec webiny-mcp serve"), "exec webiny-mcp serve");
    }

    #[test]
    fn long_commands_are_truncated_to_fit_a_badge() {
        // flags are dropped, so the length has to come from real arguments
        let out = short_cmd("yarn webiny watch api website admin graphql headless-cms file-manager");
        assert!(out.chars().count() <= 38, "{out}");
        assert!(out.ends_with('…'), "{out}");
    }
}

#[cfg(test)]
mod highlight_merge_tests {
    use super::*;

    fn style(color: u32) -> gpui::HighlightStyle {
        gpui::HighlightStyle { color: Some(rgb(color).into()), ..Default::default() }
    }

    /// gpui walks runs in order, so the output has to be sorted and disjoint
    /// whatever order the callers pushed their spans in.
    #[test]
    fn overlapping_spans_come_out_sorted_and_disjoint() {
        let out = merged_highlights(vec![(10..20, style(0x111111)), (0..5, style(0x222222))]);
        let starts: Vec<usize> = out.iter().map(|(r, _)| r.start).collect();
        assert_eq!(starts, vec![0, 10]);
        // an overlap is split, and the later span wins on the shared stretch
        let out = merged_highlights(vec![(0..10, style(0x111111)), (4..6, style(0x222222))]);
        assert_eq!(
            out.iter().map(|(r, _)| (r.start, r.end)).collect::<Vec<_>>(),
            vec![(0, 4), (4, 6), (6, 10)]
        );
        assert_eq!(out[1].1.color, style(0x222222).color);
        assert_eq!(out[0].1.color, style(0x111111).color);
    }

    /// A run split only by boundaries, with the same style either side, is one
    /// run again — fewer runs to shape.
    #[test]
    fn identical_neighbours_are_joined() {
        let out = merged_highlights(vec![(0..4, style(0x111111)), (4..8, style(0x111111))]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].0.start, out[0].0.end), (0, 8));
    }
}

#[cfg(test)]
mod file_ref_tests {
    use super::*;

    #[test]
    fn a_basename_with_a_line_is_a_reference() {
        let t = "fill-inherit (Icon.tsx:14,25) and InputPrimitive.tsx:12 too";
        let refs = file_refs(t);
        assert_eq!(refs.len(), 2);
        assert_eq!(&t[refs[0].range.clone()], "Icon.tsx:14,25");
        assert_eq!((refs[0].path.as_str(), refs[0].line), ("Icon.tsx", 14));
        assert_eq!(&t[refs[1].range.clone()], "InputPrimitive.tsx:12");
        assert_eq!(refs[1].line, 12);
    }

    #[test]
    fn full_paths_and_extensionless_lines_work() {
        let refs = file_refs("see packages/api-aco/src/x.ts:31 for the fix");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "packages/api-aco/src/x.ts");
        assert_eq!(refs[0].line, 31);
        // no line number is still a file
        let refs = file_refs("touch Cargo.toml and move on");
        assert_eq!((refs[0].path.as_str(), refs[0].line), ("Cargo.toml", 0));
    }

    /// Prose is full of numbers and dots; only real-looking files may match.
    #[test]
    fn prose_is_not_mistaken_for_a_path() {
        assert!(file_refs("11 call sites, 3 files, at 10:30").is_empty());
        assert!(file_refs("version 6.4.9 shipped").is_empty());
        assert!(file_refs("fill-neutral-xstrong on ghost").is_empty());
        // a trailing sentence dot isn't part of the name
        let refs = file_refs("in Icon.tsx.");
        assert_eq!(refs[0].path, "Icon.tsx");
    }
}

#[cfg(test)]
mod session_title_tests {
    use super::*;

    /// The adapter names a session after its first prompt, and the first prompt
    /// of a session carries the hidden briefing — that isn't a title.
    #[test]
    fn the_briefing_is_not_a_title() {
        assert_eq!(
            session_title("You are running inside TIDE, an editor that watches this repo."),
            None
        );
        // with house rules, the briefing opens with those instead
        assert_eq!(session_title("Read AGENTS.md at the root of this repo before you start"), None);
        assert_eq!(session_title("   "), None);
    }

    #[test]
    fn a_real_title_keeps_its_first_line_clipped() {
        assert_eq!(session_title("Fix the SQL date bug\nand more"), Some("Fix the SQL date bug".into()));
        let long = "x".repeat(200);
        let out = session_title(&long).unwrap();
        assert_eq!(out.chars().count(), 71, "70 chars plus the ellipsis");
        assert!(out.ends_with('…'));
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    fn urls(t: &str) -> Vec<String> {
        url_spans(t).into_iter().map(|r| t[r].to_string()).collect()
    }

    #[test]
    fn a_pasted_link_is_found_whole() {
        assert_eq!(
            urls("does it have anything to do with https://github.com/webiny/webiny-js/pull/5647"),
            ["https://github.com/webiny/webiny-js/pull/5647"]
        );
        // two on one line, and http as well as https
        assert_eq!(
            urls("see https://a.com/x and http://b.dev/y?q=1#z now"),
            ["https://a.com/x", "http://b.dev/y?q=1#z"]
        );
    }

    /// The punctuation that ends the sentence isn't part of the address, but a
    /// bracket the URL opened itself is.
    #[test]
    fn sentence_punctuation_is_left_out() {
        assert_eq!(urls("read https://x.com/a."), ["https://x.com/a"]);
        assert_eq!(urls("(https://x.com/a)"), ["https://x.com/a"]);
        assert_eq!(urls("<https://x.com/a>,"), ["https://x.com/a"]);
        assert_eq!(urls("wiki https://x.com/a_(b)"), ["https://x.com/a_(b)"]);
    }

    #[test]
    fn prose_without_a_scheme_is_not_a_link() {
        assert!(urls("github.com/webiny/webiny-js").is_empty());
        assert!(urls("the https:// prefix").is_empty());
    }
}

#[cfg(test)]
mod mention_tests {
    use super::*;

    #[test]
    fn an_at_in_front_of_the_caret_opens_a_mention() {
        assert_eq!(mention_span("@", 1), Some((0, String::new())));
        assert_eq!(mention_span("write in @wby", 13), Some((9, "wby".into())));
        // the caret, not the end of the text, decides what's being typed
        assert_eq!(mention_span("write in @wby next6", 13), Some((9, "wby".into())));
    }

    #[test]
    fn everything_else_is_plain_text() {
        // an e-mail isn't a mention
        assert_eq!(mention_span("mail a@b.com", 12), None);
        // a space closes it: the name has been typed and the sentence goes on
        assert_eq!(mention_span("@wby-next6 and", 14), None);
        assert_eq!(mention_span("no at sign here", 15), None);
        // a caret before the `@` sees nothing
        assert_eq!(mention_span("in @wby", 2), None);
    }
}

#[cfg(test)]
mod typed_title_tests {
    use super::*;

    /// An agent posts `fix: …`; the workspace ends up looking exactly as if the
    /// type had been picked in the dialog.
    #[test]
    fn a_type_prefix_becomes_that_types_emoji() {
        assert_eq!(typed_title("fix: sql date off by one"), "🐛 sql date off by one");
        assert_eq!(typed_title("  FEAT:  brand colors "), "✨ brand colors");
        // conventional-commit scopes are fine and don't survive
        assert_eq!(typed_title("refactor(api): drop the shim"), "♻️ drop the shim");
    }

    /// Anything that isn't one of our types is left alone: a colon in a title is
    /// far more likely to be prose than a mistyped type.
    #[test]
    fn other_titles_pass_through() {
        assert_eq!(typed_title("wire up: the new dock"), "wire up: the new dock");
        assert_eq!(typed_title("sql date bug"), "sql date bug");
        assert_eq!(typed_title("fix:"), "fix:");
        // already carrying a type (a resent title) stays as it is
        assert_eq!(typed_title("🐛 sql date bug"), "🐛 sql date bug");
    }
}

#[cfg(test)]
mod repo_ref_tests {
    use super::*;

    #[test]
    fn github_urls_become_a_clone_url_and_a_folder_name() {
        assert_eq!(
            parse_repo_ref("https://github.com/webiny/webiny-js"),
            Some(("https://github.com/webiny/webiny-js.git".into(), "webiny-js".into()))
        );
        // a link copied from a file view still names the repo
        assert_eq!(
            parse_repo_ref("https://github.com/webiny/webiny-js/tree/next/packages"),
            Some(("https://github.com/webiny/webiny-js.git".into(), "webiny-js".into()))
        );
        assert_eq!(
            parse_repo_ref("git@github.com:webiny/webiny-js.git"),
            Some(("git@github.com:webiny/webiny-js.git".into(), "webiny-js".into()))
        );
        assert_eq!(
            parse_repo_ref("webiny/webiny-js"),
            Some(("https://github.com/webiny/webiny-js.git".into(), "webiny-js".into()))
        );
    }

    /// Paths must keep working — the same field opens folders.
    #[test]
    fn paths_are_not_mistaken_for_repos() {
        assert!(parse_repo_ref("~/dev/tide").is_none());
        assert!(parse_repo_ref("./src").is_none());
        assert!(parse_repo_ref("/Users/adrian/dev").is_none());
        assert!(parse_repo_ref("packages/api-aco/src").is_none());
        assert!(parse_repo_ref("").is_none());
        assert!(parse_repo_ref("two words/repo").is_none());
    }
}

#[cfg(test)]
mod image_token_tests {
    use super::*;

    #[test]
    fn backspace_after_a_token_takes_the_whole_token() {
        let t = "look at [Image #1] here";
        let (s, e, label) = image_token_before(t, 19).expect("token");
        assert_eq!(&t[s..e], "[Image #1] ");
        assert_eq!(label, "Image #1");
        // and right after the bracket, once the space is gone
        let t2 = "look at [Image #12]";
        let (s2, e2, l2) = image_token_before(t2, t2.len()).expect("token");
        assert_eq!(&t2[s2..e2], "[Image #12]");
        assert_eq!(l2, "Image #12");
    }

    /// Only image tokens are atomic; other bracketed text stays ordinary.
    #[test]
    fn other_brackets_are_left_alone() {
        assert!(image_token_before("see [note] ", 11).is_none());
        assert!(image_token_before("plain text", 10).is_none());
        assert!(image_token_after("[note] x", 0).is_none());
    }

    /// ⌥← / ⌥→ walk by word, which lands inside `[Image #2]`; the caret is
    /// snapped back out to the nearest edge.
    #[test]
    fn a_word_motion_inside_a_token_is_detected() {
        let t = "a [Image #2] b";
        assert_eq!(image_token_around(t, 4), Some((2, 13)));
        assert_eq!(image_token_around(t, 9), Some((2, 13)));
        // the edges themselves are outside
        assert!(image_token_around(t, 2).is_none());
        assert!(image_token_around(t, 13).is_none());
        // and a second token is found too
        let t2 = "[Image #1] [Image #2] x";
        assert_eq!(image_token_around(t2, 15), Some((11, 22)));
    }

    /// A collapsed paste is atomic in the same way an image token is.
    #[test]
    fn a_pasted_text_token_is_atomic_too() {
        let t = "see [Pasted text #3 +216 lines] ok";
        let (s, e, label) = image_token_before(t, 32).expect("token");
        assert_eq!(&t[s..e], "[Pasted text #3 +216 lines] ");
        assert_eq!(label, "Pasted text #3 +216 lines");
        assert_eq!(image_token_after(t, 4), Some((4, 32)));
        assert_eq!(image_token_around(t, 10), Some((4, 32)));
    }

    #[test]
    fn the_right_arrow_steps_over_a_token() {
        let t = "a [Image #2] b";
        let (s, e) = image_token_after(t, 2).expect("token");
        assert_eq!(&t[s..e], "[Image #2] ");
        // mid-token there's nothing to step over
        assert!(image_token_after(t, 3).is_none());
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;

    /// One comment, no general note — the common shape in these tests.
    fn format_review_one(item: ReviewItem) -> String {
        format_review(&[item], "")
    }

    #[test]
    fn message_numbers_items_and_quotes_the_code() {
        let items = vec![
            ReviewItem {
                id: "ab12c".into(),
                reference: "src/foo.ts:42".into(),
                snippet: "const x = doThing()".into(),
                note: "memoize this".into(),
                thread: Vec::new(),
            },
            ReviewItem {
                id: "de34f".into(),
                reference: "src/bar.rs:10-11".into(),
                snippet: "fn a() {\n    b();\n}".into(),
                note: "extract".into(),
                thread: Vec::new(),
            },
        ];
        let msg = format_review(&items, "");
        assert!(msg.starts_with("Review comments from TIDE"), "{msg}");
        assert!(msg.contains("1. [ab12c] src/foo.ts:42\n   | const x = doThing()\n   -> memoize this"), "{msg}");
        // the id is what lets an answer find its way back to this thread
        assert!(msg.contains("$TIDE_RPC/comment"), "{msg}");
        // every snippet line is quoted, so the agent can't confuse code with notes
        assert!(msg.contains("2. [de34f] src/bar.rs:10-11\n   | fn a() {\n   |     b();\n   | }\n   -> extract"), "{msg}");
    }

    /// A follow-up has to carry what was already said, or the agent answers the
    /// original note again instead of the new question.
    #[test]
    fn a_thread_is_included_so_a_follow_up_reads_as_a_conversation() {
        let msg = format_review_one(ReviewItem {
            id: "t1".into(),
            reference: "a.tsx:45".into(),
            snippet: "value: String(option.value),".into(),
            note: "what about undefined?".into(),
            thread: vec![
                ("agent".into(), "!= null covers both".into()),
                ("you".into(), "then why the extra guard?".into()),
            ],
        });
        assert!(msg.contains("   you (agent): != null covers both"), "{msg}");
        assert!(msg.contains("   me: then why the extra guard?"), "{msg}");
    }

    #[test]
    fn a_long_snippet_is_capped() {
        let snippet = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let msg = format_review_one(ReviewItem {
            id: "x9".into(),
            reference: "f.rs:1-40".into(),
            snippet,
            note: "too big".into(),
            thread: Vec::new(),
        });
        assert_eq!(msg.lines().filter(|l| l.starts_with("   | ")).count(), 12);
    }
}

#[cfg(test)]
mod review_persist_tests {
    use super::*;

    fn sample(root: &Path) -> Vec<ReviewComment> {
        vec![
            ReviewComment {
                id: "aaa11".into(),
                path: root.join("src/main.rs"),
                line: 42,
                end: 42,
                snippet: "let x = 1;".into(),
                text: "memoize".into(),
                draft: false,
                sent: true,
                replies: vec![ReviewReply { who: "agent".into(), text: "done, memoized".into() }],
                resolved: true,
            },
            ReviewComment {
                id: "bbb22".into(),
                path: root.join("src/lib.rs"),
                line: 7,
                end: 9,
                snippet: "fn a() {}".into(),
                text: "half-written".into(),
                draft: true,
                sent: false,
                replies: Vec::new(),
                resolved: false,
            },
        ]
    }

    #[test]
    fn round_trips_through_json() {
        let root = Path::new("/Users/x/dev/proj");
        let json = review_to_json(root, &sample(root));
        assert_eq!(review_from_json(root, &json), sample(root));
    }

    #[test]
    fn paths_are_stored_relative_so_a_moved_checkout_still_matches() {
        let json = review_to_json(Path::new("/old/place"), &sample(Path::new("/old/place")));
        assert!(!json.contains("/old/place"), "{json}");
        let moved = review_from_json(Path::new("/new/place"), &json);
        assert_eq!(moved[0].path, Path::new("/new/place/src/main.rs"));
        assert!(moved[1].draft);
    }

    #[test]
    fn junk_and_empty_notes_are_dropped_not_fatal() {
        let root = Path::new("/p");
        assert!(review_from_json(root, "not json").is_empty());
        let json = r#"[{"path":"a.rs","line":1,"text":"  "},{"path":"b.rs","line":2,"text":"keep"}]"#;
        let out = review_from_json(root, json);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "keep");
        assert_eq!(out[0].end, 2); // end defaults to line
    }
}
