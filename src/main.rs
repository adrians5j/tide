use gpui::{
    AnyElement, AnyView, AnyWindowHandle, App, Bounds, ClipboardItem, Context, CursorStyle, Div, Entity, EventEmitter,
    FocusHandle, KeyBinding, WeakEntity,
    KeyDownEvent, Keystroke, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ScrollStrategy, SharedString,
    StyledText, TextRun, Window, WindowBounds, WindowOptions, actions, div, font, prelude::*, px,
    relative, rgb, rgba, size, uniform_list, ScrollHandle, UniformListScrollHandle,
};
use gpui_platform::application;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

mod theme;
use theme::*;
mod field;
use field::{Edit, Field};
mod syntax;
mod editor;
mod term;
mod diff;
mod lsp;
use diff::{DiffKind, DiffRow};
use syntax::{Highlighter, Run};
use lsp::Lsp;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use term::Terminal;
use editor::{
    Backspace, CompDismiss, CompTrigger, Copy, Cut, Delete, DeleteLine, Editor, End, GotoDef, Home,
    Indent, MoveDown, MoveLeft, MoveLineDown, MoveLineUp, MoveRight, MoveUp, Newline, OpenLocation,
    Paste, Redo, Save, SearchOpen, SelectAll, SelectDown, SelectEnd, SelectHome, SelectLeft,
    SelectRight, SelectUp, SelectWordLeft, SelectWordRight, Undo, WordLeft, WordRight,
};

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
const IC_CHEVRON_LEFT: &str = "\u{eab5}";
const IC_CHEVRON_RIGHT: &str = "\u{eab6}";
const IC_CLOSE: &str = "\u{ea76}";
const IC_FOLDER: &str = "\u{ea83}";
const IC_COPY: &str = "\u{ebcc}";
// The codicon font is renamed to "Segoe Fluent Icons" because GPUI refuses to
// load any font lacking an 'm' glyph — except that one specially-cased name.
const ICON_FONT: &str = "Segoe Fluent Icons";

actions!(
    workspace,
    [
        CloseTab, CloseOthers, ToggleTerminal, OpenFinder, GotoLine, NewTerminal, CloseTerminalTab,
        CloseOtherTerminals, GotoCommit, ShowDiff, FindInFiles,
        GitPopup, CommandPalette, CopyReference, OpenOnGithub, NextProject, PrevProject, OpenProject,
        ShowProjects, PushDialog, RunCommand, NewProject, FetchRemotes, PullRemote
    ]
);

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
/// Height above the find dialog's results: title 24 + search 42 + scope 22.
const FIND_HEAD_H: f32 = 88.0;

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
fn parse_search_query(q: &str) -> (String, Vec<String>) {
    let mut include = Vec::new();
    let mut excludes = Vec::new();
    for tok in q.split_whitespace() {
        if tok.len() > 1 && tok.starts_with('-') {
            excludes.push(tok[1..].to_string());
        } else {
            include.push(tok);
        }
    }
    (include.join(" "), excludes)
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
    let (include, excludes) = parse_search_query(query);
    if include.len() < 2 || scopes.is_empty() {
        return Vec::new();
    }
    let mut results = search_ripgrep(&include, scopes, case_sensitive)
        .unwrap_or_else(|| search_rust(&include, scopes, case_sensitive));
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
fn search_ripgrep(query: &str, scopes: &[PathBuf], case_sensitive: bool) -> Option<Vec<FindResult>> {
    let bin = rg_bin()?;
    let mut cmd = Command::new(bin);
    cmd.arg("--line-number")
        .arg("--no-heading")
        .arg("--color=never")
        // case-sensitive on demand; otherwise smart-case (insensitive if all-lowercase)
        .arg(if case_sensitive { "--case-sensitive" } else { "--smart-case" })
        .arg("--max-count=50") // per-file cap
        .arg("--fixed-strings") // literal, not regex
        .arg("--")
        .arg(query);
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
fn search_rust(query: &str, scopes: &[PathBuf], case_sensitive: bool) -> Vec<FindResult> {
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
    for path in files {
        if out.len() >= FIND_CAP {
            break;
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

/// File holding the recent-projects list (most-recent first, one path per line).
fn recents_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".config/tide/recent-projects.txt"))
}

/// Recently-opened project paths, most recent first; only existing dirs.
fn load_recent_projects() -> Vec<PathBuf> {
    let Some(file) = recents_path() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(&file) else { return Vec::new() };
    text.lines().map(|l| PathBuf::from(l.trim())).filter(|p| p.is_dir()).collect()
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
        area = area.child(
            line.child(
                div()
                    .w(px(44.))
                    .pr_2()
                    .flex()
                    .justify_end()
                    .text_color(rgb(LINE_NUMBER))
                    .child(no.map(|n| n.to_string()).unwrap_or_default()),
            )
            .child(div().flex_grow(1.0).px_2().child(StyledText::new(text).with_runs(runs))),
        );
    }
    div()
        .relative()
        .w_1_2()
        .h_full()
        .overflow_hidden()
        .child(area)
        .child(v_scrollbar(handle))
        .child(h_scrollbar(handle))
}

#[allow(clippy::too_many_arguments)]
fn diff_body<'a>(
    rows: &[DiffRow],
    left_sb: &ScrollHandle,
    right_sb: &ScrollHandle,
    char_w: f32,
    sel: Option<&DiffSel>,
    left_styles: &'a [Vec<Run>],
    right_styles: &'a [Vec<Run>],
    matches: &[DiffMatch],
    cur_match: usize,
    caret: Option<(DiffSide, usize, usize)>,
    caret_on: bool,
) -> impl IntoElement {
    // two independent 2D-scroll panes (wheel = vertical, shift+wheel = horizontal)
    div()
        .flex()
        .flex_row()
        .flex_grow(1.0)
        .min_h(px(0.))
        .child(diff_pane(rows, DiffSide::Left, left_sb, char_w, sel, left_styles, matches, cur_match, caret, caret_on))
        .child(div().w(px(1.)).h_full().bg(rgb(BORDER)))
        .child(diff_pane(rows, DiffSide::Right, right_sb, char_w, sel, right_styles, matches, cur_match, caret, caret_on))
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

#[derive(Clone, Copy, PartialEq)]
enum LeftView {
    Files,
    Changes,
    PullRequest,
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
    for cand in ["main", "master", "develop"] {
        let ok = Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", cand])
            .current_dir(root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return cand.to_string();
        }
    }
    "main".to_string()
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
fn gh_set_file_viewed(root: &Path, pr_id: &str, path: &str, viewed: bool) {
    let field = if viewed { "markFileAsViewed" } else { "unmarkFileAsViewed" };
    let query = format!(
        "mutation($id:ID!,$path:String!){{ {field}(input:{{pullRequestId:$id, path:$path}}){{ clientMutationId }} }}"
    );
    let _ = Command::new("gh")
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
        .output();
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
}

/// Per-branch actions, shown in a submenu when you press enter on a branch in
/// the git popup.
#[derive(Clone, Copy, PartialEq)]
enum BranchAction {
    Checkout,
    Merge,
}

/// (action, label, icon) — the submenu entries, in display order. The label is
/// a fallback; render builds a dynamic one (e.g. "Merge 'x' into 'y'").
const BRANCH_ACTIONS: &[(BranchAction, &'static str, &'static str)] = &[
    (BranchAction::Checkout, "Checkout", IC_HOME),
    (BranchAction::Merge, "Merge", IC_BRANCH),
];

/// Commands available in the command palette (cmd+shift+p).
#[derive(Clone, Copy, PartialEq)]
enum Cmd {
    WipCommit,
    WipPush,
    OpenPr,
    CreatePr,
    Build,
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
    ProcessManager,
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
    (Cmd::Build, "Build", IC_TOOLS, ""),
    (Cmd::GitAdd, "Git Add", IC_ADD, ""),
    (Cmd::Pull, "Pull", IC_HOME, ""),
    (Cmd::Fetch, "Fetch", IC_HOME, ""),
    (Cmd::NewBranch, "New Branch", IC_BRANCH, ""),
    (Cmd::Commit, "Commit…", IC_COMMIT, "⌘K"),
    (Cmd::GitPopup, "Branches & Git Actions", IC_BRANCH, "⌥B"),
    (Cmd::ShowDiff, "Show Diff", IC_PR, "⌘D"),
    (Cmd::ToggleTerminal, "Toggle Terminal", IC_TERMINAL, "⌥F12"),
    (Cmd::FindInFiles, "Find in Files", IC_SEARCH, "⌘⇧F"),
    (Cmd::GoToFile, "Go to File", IC_FILES, "⌘⇧O"),
    (Cmd::GoToLine, "Go to Line", IC_HOME, "⌘L"),
    (Cmd::CopyBranch, "Copy Branch Name", IC_BRANCH, ""),
    (Cmd::ProcessManager, "Process Manager", IC_TOOLS, ""),
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

fn fetch_pr_link(root: &Path, branch: &str) -> Option<(u64, String, PrStatus)> {
    if branch.is_empty() {
        return None;
    }
    let out = Command::new("gh")
        .args(["pr", "view", branch, "--json", "number,url,statusCheckRollup"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let number = v.get("number")?.as_u64()?;
    let url = v.get("url")?.as_str()?.to_string();
    Some((number, url, rollup_status(&v)))
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
    // type-to-filter buffer for the dir tree; typing while the tree is focused
    // fuzzy-filters the entries (nvim-explorer style)
    tree_filter: Field,
    // total file/dir count scanned for the current filter (the "/N" in "5/N")
    tree_total: usize,
    // path copied with cmd+c in the tree, to be pasted with cmd+v
    tree_clipboard: Option<PathBuf>,
    // pending delete confirmation: the tree path awaiting yes/no
    confirm_delete: Option<PathBuf>,
    confirm_focus: FocusHandle,
    terminals: Vec<Entity<Terminal>>,
    active_term: usize,
    show_terminal: bool,
    term_width: f32,
    resizing_term: bool,
    // read-only Run console (command output)
    run_open: bool,
    run_cmd: String,
    run_lines: Vec<String>,
    run_running: bool,
    run_active: bool, // a command is running or just finished (toast visible)
    run_failed: bool,
    run_spin: usize, // spinner frame
    run_gen: u64,
    run_scroll: UniformListScrollHandle,
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
    tree_ctx_focus: FocusHandle,
    git_status: HashMap<PathBuf, GitState>,
    // pull-request review
    pr_files: Vec<(PathBuf, GitState)>,
    pr_viewed: HashSet<PathBuf>,
    pr_collapsed: HashSet<PathBuf>,
    pr_base: String,
    pr_selected: Option<PathBuf>,
    pr_focus: FocusHandle,
    pr_gen: u64,
    pr_loading: bool,
    pr_node_id: Option<String>, // PR GraphQL node id, for marking files viewed
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
    // go-to-line dialog
    goto_open: bool,
    goto_focus: FocusHandle,
    goto_query: Field,
    // root focus so global shortcuts dispatch even with no file/tab open
    focus: FocusHandle,
    inited: bool,
    // chrome
    branch: String,
    // open PR for the current branch, if any: (number, url). Refetched when the
    // branch changes; `pr_link_branch` is the branch it was last queried for.
    pr_link: Option<(u64, String, PrStatus)>,
    pr_link_branch: String,
    mem_mb: u64,
    left_view: LeftView,
    show_left: bool,
    // branch-name prompt (for the `br` alias)
    br_open: bool,
    br_focus: FocusHandle,
    br_query: Field,
    // create-PR prompt: optional milestone for `gh pr create`
    prc_open: bool,
    prc_focus: FocusHandle,
    prc_milestone: Field,
    // run-arbitrary-command prompt (cmd+shift+t)
    runc_open: bool,
    runc_focus: FocusHandle,
    runc_query: Field,
    // new-project dialog (cmd+shift+n): type or choose a folder path to open
    newproj_open: bool,
    newproj_focus: FocusHandle,
    newproj_path: Field,
    newproj_recents: Vec<PathBuf>, // recent projects, loaded when the dialog opens
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
    gitp_action_sel: usize,
    // command palette
    palette_open: bool,
    palette_focus: FocusHandle,
    palette_query: Field,
    palette_sel: usize,
    palette_results: Vec<(Cmd, &'static str, &'static str, &'static str)>,
    palette_gen: u64,
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
    ws_idle: Vec<f32>,  // seconds since each project was last viewed/worked on
    ws_pulse: Vec<f32>, // seconds since each project's last detected change (big if none)
    ws_active: usize,
    ws_open: bool, // project-switcher dropdown expanded
    // idle tracking: reset while this project is the on-screen one. Drives the
    // topbar icon's fade-to-red (a project you're not looking at slowly reddens).
    last_active: Instant,
    // change detection: when the fingerprint changes, `pulse_at` is stamped and
    // the topbar icon plays a brief pulse animation. None until the first poll
    // establishes a baseline (so loading a project doesn't fire a pulse).
    prev_fp: Option<u64>,
    pulse_at: Option<Instant>,
}

/// Navigation requests a project view sends up to the workspace.
enum ProjectNav {
    Switch(usize),
    Open,
    OpenPath(PathBuf), // open a project at a specific folder path (from the new-project dialog)
    Remove(usize),
    Activity, // a change was detected → workspace should pulse this icon
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
        let mut s = Self {
            root,
            expanded,
            entries: Vec::new(),
            tabs: Vec::new(),
            active: 0,
            tree_width: 280.,
            resizing: false,
            tree_scroll: UniformListScrollHandle::new(),
            tree_focus: cx.focus_handle(),
            tree_filter: Field::default(),
            tree_total: 0,
            tree_clipboard: None,
            confirm_delete: None,
            confirm_focus: cx.focus_handle(),
            pr_files: Vec::new(),
            pr_viewed: HashSet::new(),
            pr_collapsed: HashSet::new(),
            pr_base: String::new(),
            pr_selected: None,
            pr_focus: cx.focus_handle(),
            pr_gen: 0,
            pr_loading: false,
            pr_node_id: None,
            pr_filter: Field::default(),
            pr_filter_focus: cx.focus_handle(),
            pr_view_filter: PrViewFilter::All,
            pr_rows: Vec::new(),
            pr_shown_files: Vec::new(),
            pr_scroll: UniformListScrollHandle::new(),
            terminals: Vec::new(),
            active_term: 0,
            show_terminal: true,
            term_width: 900.,
            run_open: false,
            run_cmd: String::new(),
            run_lines: Vec::new(),
            run_running: false,
            run_active: false,
            run_failed: false,
            run_spin: 0,
            run_gen: 0,
            run_scroll: UniformListScrollHandle::new(),
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
            all_dirs: Vec::new(),
            goto_open: false,
            goto_focus: cx.focus_handle(),
            goto_query: Field::default(),
            focus: cx.focus_handle(),
            inited: false,
            branch: String::new(),
            pr_link: None,
            pr_link_branch: String::new(),
            mem_mb: 0,
            left_view: LeftView::Files,
            show_left: true,
            br_open: false,
            br_focus: cx.focus_handle(),
            br_query: Field::default(),
            prc_open: false,
            prc_focus: cx.focus_handle(),
            prc_milestone: Field::default(),
            runc_open: false,
            runc_focus: cx.focus_handle(),
            runc_query: Field::default(),
            newproj_open: false,
            newproj_focus: cx.focus_handle(),
            newproj_path: Field::default(),
            newproj_recents: Vec::new(),
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
            gitp_action_sel: 0,
            palette_open: false,
            palette_focus: cx.focus_handle(),
            palette_query: Field::default(),
            palette_sel: 0,
            palette_results: Vec::new(),
            palette_gen: 0,
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
            ws_branches: Vec::new(),
            ws_idle: Vec::new(),
            ws_pulse: Vec::new(),
            ws_active: 0,
            ws_open: false,
            // start "idle" (colorless) — a workspace only goes green once it's
            // actually viewed or sees a change, not just because it was opened
            last_active: Instant::now()
                .checked_sub(Duration::from_secs(ACTIVE_FADE_SECS as u64 + 1))
                .unwrap_or_else(Instant::now),
            prev_fp: None,
            pulse_at: None,
        };
        s.ignored = git_ignored_paths(&s.root); // hide git-ignored paths from the tree
        s.expanded.insert(s.root.clone()); // the root node starts expanded
        s.rebuild();
        s.start_git_poll(cx);
        s.start_caret_blink(cx);
        s.lsp = Lsp::new(&s.root);
        let root = s.root.clone();
        let term = cx.new(|cx| Terminal::new(root, cx));
        // optional startup command run in the default terminal (e.g. set
        // TIDE_TERM_CMD=z to auto-launch zellij). The shell reads this after
        // sourcing its rc file, so aliases/functions like `z` are defined.
        if let Ok(cmd) = std::env::var("TIDE_TERM_CMD") {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                term.read(cx).send_text(&format!("{cmd}\n"));
            }
        }
        s.terminals.push(term);
        s
    }

    /// True when some chrome text input is on screen (so we only repaint for
    /// the blink when a caret is actually visible).
    fn input_visible(&self) -> bool {
        self.finder_open
            || self.goto_open
            || self.br_open
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
        self.terminals.get(self.active_term)
    }

    /// Ensure at least one terminal exists; returns the active one's handle.
    fn ensure_terminal(&mut self, cx: &mut Context<Self>) {
        if self.terminals.is_empty() {
            let root = self.root.clone();
            self.terminals.push(cx.new(|cx| Terminal::new(root, cx)));
            self.active_term = 0;
        }
    }

    fn new_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = self.root.clone();
        self.terminals.push(cx.new(|cx| Terminal::new(root, cx)));
        self.active_term = self.terminals.len() - 1;
        self.show_terminal = true;
        let fh = self.terminals[self.active_term].read(cx).focus_handle.clone();
        window.focus(&fh, cx);
        cx.notify();
    }

    fn act_new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal(window, cx);
    }

    fn act_close_terminal(&mut self, _: &CloseTerminalTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.terminals.is_empty() {
            self.close_terminal(self.active_term, window, cx);
        }
    }

    fn act_close_other_terminals(&mut self, _: &CloseOtherTerminals, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminals.is_empty() {
            return;
        }
        let keep = self.terminals.remove(self.active_term);
        for t in &self.terminals {
            t.update(cx, |t, _| t.kill());
        }
        self.terminals.clear();
        self.terminals.push(keep);
        self.active_term = 0;
        let fh = self.terminals[0].read(cx).focus_handle.clone();
        window.focus(&fh, cx);
        cx.notify();
    }

    fn switch_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.terminals.len() {
            self.active_term = ix;
            let fh = self.terminals[ix].read(cx).focus_handle.clone();
            window.focus(&fh, cx);
            cx.notify();
        }
    }

    fn close_terminal(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.terminals.len() {
            return;
        }
        self.terminals[ix].update(cx, |t, _| t.kill());
        self.terminals.remove(ix);
        if self.active_term >= self.terminals.len() && !self.terminals.is_empty() {
            self.active_term = self.terminals.len() - 1;
        } else if ix < self.active_term {
            self.active_term = self.active_term.saturating_sub(1);
        }
        if self.terminals.is_empty() {
            self.show_terminal = false;
            self.focus_active(window, cx);
        } else {
            let fh = self.terminals[self.active_term].read(cx).focus_handle.clone();
            window.focus(&fh, cx);
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
            let (status, branch, mem, ignored) = cx
                .background_executor()
                .spawn(async move {
                    (compute_git(&root2), git_branch(&root2), process_mem_mb(), git_ignored_paths(&root2))
                })
                .await;
            // refetch the PR link only when the branch actually changed — the
            // gh call hits the network, so we don't want it on every 2s tick
            let pr_update = if branch != prev_pr_branch {
                let (root3, br) = (root.clone(), branch.clone());
                Some(cx.background_executor().spawn(async move { fetch_pr_link(&root3, &br) }).await)
            } else {
                None
            };
            if this
                .update(cx, |this, cx| {
                    this.git_status = status;
                    this.branch = branch.clone();
                    this.mem_mb = mem;
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
                    // fingerprint mutable state (git working tree + open buffers);
                    // if it changed since last tick, this project saw activity
                    let mut fp: u64 = 0;
                    for (p, s) in &this.git_status {
                        let mut e: u64 = 0xcbf29ce484222325;
                        for b in p.to_string_lossy().as_bytes() {
                            e ^= *b as u64;
                            e = e.wrapping_mul(0x100000001b3);
                        }
                        let code = match s {
                            GitState::New => 1u64,
                            GitState::Modified => 2,
                            GitState::Deleted => 3,
                        };
                        // fold in the file's mtime so repeated edits to an
                        // already-"modified" file (even if not open in a tab) count
                        let mtime = std::fs::metadata(p)
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        fp ^= e.wrapping_add(code).wrapping_add(mtime.rotate_left(17)); // xor: order-independent
                    }
                    for ed in &editors {
                        fp = fp.rotate_left(7) ^ ed.read(cx).content_hash();
                    }
                    // a change since the last poll → pulse this project's icon
                    // (the first poll just records a baseline, no pulse)
                    if this.prev_fp.is_some_and(|prev| prev != fp) {
                        this.pulse_at = Some(Instant::now());
                        // detected work also counts as freshness: a changing
                        // workspace stays lit (not idle-faded) even unwatched,
                        // so you can tell it's still doing something at a glance
                        this.last_active = Instant::now();
                        cx.emit(ProjectNav::Activity); // workspace animates the flash
                    }
                    this.prev_fp = Some(fp);
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
        let query = self.tree_filter.text.trim().to_string();
        if query.is_empty() {
            if self.expanded.contains(&root) {
                self.walk(&root, 1, false, &mut out);
            }
            self.tree_total = out.len();
        } else {
            // filtered view: scan the whole tree (ignoring the expanded set),
            // keeping matching entries plus the ancestor dirs that reach them.
            let mut total = 0;
            self.walk_filtered(&root, 1, false, &query, &mut out, &mut total);
            self.tree_total = total;
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

    /// Walk the entire tree keeping only entries whose name fuzzy-matches
    /// `query`, plus the ancestor directories needed to reach them. Bumps
    /// `total` for every file/dir visited so the caller can show a matched/total
    /// count. Returns true if anything at or under `dir` matched.
    fn walk_filtered(
        &self,
        dir: &Path,
        depth: usize,
        ignored_ancestor: bool,
        query: &str,
        out: &mut Vec<Entry>,
        total: &mut usize,
    ) -> bool {
        let mut any = false;
        for (name, path, is_dir) in Self::read_children(dir) {
            *total += 1;
            let ignored = ignored_ancestor || self.ignored.contains(&path);
            if is_dir {
                let mark = out.len();
                out.push(Entry { name: name.clone(), path: path.clone(), is_dir: true, depth, ignored });
                let child = self.walk_filtered(&path, depth + 1, ignored, query, out, total);
                if child || fuzzy_score(query, &name).is_some() {
                    any = true;
                } else {
                    out.truncate(mark); // no match under (or in) this dir → drop it
                }
            } else if fuzzy_score(query, &name).is_some() {
                out.push(Entry { name, path, is_dir: false, depth, ignored });
                any = true;
            }
        }
        any
    }

    fn active_path(&self) -> Option<&PathBuf> {
        self.tabs.get(self.active).map(|t| &t.path)
    }

    fn project_name(&self) -> String {
        self.root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "tide".into())
    }

    /// Seconds since this project was last viewed/worked on (for the idle fade).
    fn idle_secs(&self) -> f32 {
        self.last_active.elapsed().as_secs_f32()
    }

    /// Seconds since this project's last detected change, or a large number if
    /// there hasn't been one (so it reads as "not pulsing").
    fn pulse_secs(&self) -> f32 {
        self.pulse_at.map(|t| t.elapsed().as_secs_f32()).unwrap_or(1e9)
    }

    /// Root-level key handling: escape closes any open popup. Fires for the
    /// whole focus path, so it works even if the popup didn't grab focus.
    fn on_root_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.key != "escape" {
            return;
        }
        if self.editor_ctx.take().is_some() {
            cx.notify();
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
            || self.prc_open
            || self.runc_open
            || self.newproj_open
            || self.find_open
            || self.gitp_open
            || self.ws_open;
        if any_open {
            self.palette_open = false;
            self.finder_open = false;
            self.goto_open = false;
            self.br_open = false;
            self.prc_open = false;
            self.runc_open = false;
            self.newproj_open = false;
            self.find_open = false;
            self.gitp_open = false;
            self.ws_open = false;
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
                        self.tree_clipboard = Some(p);
                        cx.notify();
                    }
                }
                "v" => self.paste_into_selected(cx),
                _ => {}
            }
            return;
        }
        // escape clears an active filter
        if ks.key == "escape" {
            if !self.tree_filter.is_empty() {
                self.tree_filter.clear();
                self.rebuild();
                cx.notify();
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
        // backspace with an empty filter falls back to delete-selected-entry
        if ks.key == "backspace" && self.tree_filter.is_empty() {
            if let Some(p) = self.tree_selected.clone() {
                self.confirm_delete = Some(p);
                window.focus(&self.confirm_focus, cx);
                cx.notify();
            }
            return;
        }
        // anything else feeds the type-to-filter buffer
        if Self::field_input(&mut self.tree_filter, ks, cx, |_| true) == Edit::Changed {
            self.rebuild();
            if !self.entries.is_empty() {
                self.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
        }
        cx.notify();
    }

    /// Paste the copied file/folder into the selected folder (recursively for
    /// directories), avoiding clobbering by adding a " copy" suffix on collision.
    fn paste_into_selected(&mut self, cx: &mut Context<Self>) {
        let Some(src) = self.tree_clipboard.clone() else { return };
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
        if copy_path(&src, &dest).is_err() {
            return;
        }
        self.expanded.insert(target_dir);
        self.rebuild();
        self.tree_selected = Some(dest.clone());
        self.reveal_in_tree(&dest);
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
            if self.tree_clipboard.as_ref() == Some(&path) {
                self.tree_clipboard = None;
            }
            self.rebuild();
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
        self.run_cmd = cmd.clone();
        self.run_running = true;
        self.run_active = true; // show the bottom-right toast (not the console)
        self.run_failed = false;
        self.run_spin = 0;
        self.run_lines.clear();
        self.run_lines.push(format!("$ {}", cmd));

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
                        let mut new = this
                            .run_buf
                            .lock()
                            .map(|mut b| std::mem::take(&mut *b))
                            .unwrap_or_default();
                        this.run_lines.append(&mut new);
                        let n = this.run_lines.len();
                        if n > 5000 {
                            this.run_lines.drain(0..n - 5000);
                        }
                        this.run_scroll
                            .scroll_to_item(this.run_lines.len().saturating_sub(1), ScrollStrategy::Top);
                    }
                    if this.run_done.load(Ordering::Relaxed) {
                        this.run_running = false;
                        let success = this.run_ok.load(Ordering::Relaxed);
                        this.run_failed = !success;
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
                            // surface the console on error; replace the toast
                            this.run_open = true;
                            this.run_active = false;
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
            tab.editor.update(cx, |e, cx| e.save(cx));
        }
    }

    fn open_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // leaving the current tab → auto-save it
        self.save_tab(self.active, cx);
        // already open? just switch (and pick up any external edits)
        if let Some(i) = self.tabs.iter().position(|t| t.path == path) {
            self.active = i;
            let ed = self.tabs[i].editor.clone();
            ed.update(cx, |e, cx| e.reload_if_changed(cx));
        } else {
            let lsp = self.lsp.clone();
            let editor = cx.new(|cx| Editor::new(lsp, cx));
            editor.update(cx, |e, cx| e.load(path.clone(), cx));
            // go-to-definition: open the target file at the position
            cx.subscribe_in(&editor, window, |this, _ed, ev: &OpenLocation, window, cx| {
                this.open_file(ev.path.clone(), window, cx);
                if let Some(tab) = this.tabs.get(this.active) {
                    let (line, col) = (ev.line, ev.col);
                    tab.editor.update(cx, |e, cx| e.goto(line, col, cx));
                }
            })
            .detach();
            self.tabs.push(Tab { path: path.clone(), editor });
            self.active = self.tabs.len() - 1;
        }
        self.tree_selected = Some(path.clone());
        self.reveal_in_tree(&path);
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
        // a live type-to-filter would hide the target, so drop it first
        if !self.tree_filter.is_empty() {
            self.tree_filter.clear();
        }
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
        self.show_terminal = !self.show_terminal;
        if self.show_terminal {
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

    fn act_open_finder(&mut self, _: &OpenFinder, window: &mut Window, cx: &mut Context<Self>) {
        self.save_tab(self.active, cx);
        self.finder_open = true;
        self.finder_query.clear();
        self.finder_selected = 0;
        self.update_finder(); // show whatever we already have (instant)
        window.focus(&self.finder_focus, cx);
        cx.notify();

        // (re)scan the file list off the main thread so opening never blocks
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let (files, dirs) = cx
                .background_executor()
                .spawn(async move { collect_paths(&root) })
                .await;
            this.update(cx, |this, cx| {
                this.all_files = files;
                this.all_dirs = dirs;
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
                fuzzy_score(&query, &rel).map(|sc| (sc, p.clone()))
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

    // ── toolbar alias actions (run zsh aliases in the terminal) ─────────────

    fn open_branch_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.br_open = true;
        self.br_query.clear();
        window.focus(&self.br_focus, cx);
        cx.notify();
    }

    fn br_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.br_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                let name = self.br_query.text.trim().to_string();
                self.br_open = false;
                if !name.is_empty() {
                    self.run_command(format!("br {}", name), cx);
                }
            }
            _ => {
                Self::field_input(&mut self.br_query, ks, cx, |_| true);
            }
        }
        cx.notify();
    }

    /// Open the create-PR prompt: an optional milestone, then `gh pr create`.
    fn open_pr_create_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.prc_open = true;
        self.prc_milestone.clear();
        window.focus(&self.prc_focus, cx);
        cx.notify();
    }

    fn prc_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.prc_open = false;
                self.focus_active(window, cx);
            }
            "enter" => {
                let milestone = self.prc_milestone.text.trim().to_string();
                self.prc_open = false;
                // runs the `pr` zsh function; milestone is optional (--ms <ver>)
                let cmd = if milestone.is_empty() {
                    "pr".to_string()
                } else {
                    format!("pr --ms {}", milestone)
                };
                self.run_command(cmd, cx);
            }
            _ => {
                Self::field_input(&mut self.prc_milestone, ks, cx, |_| true);
            }
        }
        cx.notify();
    }

    /// cmd+shift+t: prompt for an arbitrary shell command to run.
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

    /// opt+l: pull the current branch (quick shortcut for the palette's Pull).
    fn act_pull(&mut self, _: &PullRemote, _window: &mut Window, cx: &mut Context<Self>) {
        self.run_command("git pull".into(), cx);
    }

    /// cmd+shift+n: open the "New Project" dialog (type a folder path or pick one).
    fn act_new_project(&mut self, _: &NewProject, window: &mut Window, cx: &mut Context<Self>) {
        self.newproj_open = true;
        self.newproj_path.clear();
        self.newproj_recents = load_recent_projects();
        window.focus(&self.newproj_focus, cx);
        cx.notify();
    }

    fn newproj_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.newproj_open = false;
                self.focus_active(window, cx);
            }
            "enter" => self.newproj_submit(cx),
            _ => {
                Self::field_input(&mut self.newproj_path, ks, cx, |_| true);
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
        }
        // not a folder → leave the dialog open so the user can fix the path
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
        // otherwise: working-tree diff of the changed files, in a separate window
        let active = self.active_path().cloned();
        self.open_working_diff(active, cx);
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
        self.open_diff_window(files, idx, None, None, cx);
    }

    /// Open a diff in its own window. `old` Some → committed diff of `old` vs
    /// `new_rev` (default HEAD); `old` None → working-tree diff (HEAD vs disk).
    fn open_diff_window(
        &mut self,
        files: Vec<PathBuf>,
        idx: usize,
        old: Option<String>,
        new_rev: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let root = self.root.clone();
        let storm = cx.entity().downgrade();
        let main = cx.active_window();
        let bounds = Bounds::centered(None, size(px(1660.), px(820.)), cx);
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
            move |_, cx| cx.new(|cx| DiffWindow::new(root, files, idx, old, new_rev, storm, main, cx)),
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
            let (base, files, node_id, viewed) = cx
                .background_executor()
                .spawn(async move {
                    let base = git_pr_base(&root);
                    let files = git_pr_files(&root, &base);
                    let node_id = gh_pr_node_id(&root);
                    // restore the "viewed" state from the actual PR on GitHub
                    let viewed = gh_pr_viewed_files(&root);
                    (base, files, node_id, viewed)
                })
                .await;
            this.update(cx, |this, cx| {
                if this.pr_gen == gen {
                    this.pr_base = base;
                    this.pr_files = files;
                    this.pr_node_id = node_id;
                    this.pr_viewed = viewed;
                    this.pr_loading = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn toggle_pr_viewed(&mut self, path: PathBuf) {
        if !self.pr_viewed.remove(&path) {
            self.pr_viewed.insert(path.clone());
        }
        self.push_pr_viewed(&[path]);
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
        // (repo-relative path, viewed-now) for each file
        let items: Vec<(String, bool)> = paths
            .iter()
            .map(|p| (self.rel(p), self.pr_viewed.contains(p)))
            .collect();
        std::thread::spawn(move || {
            for (path, viewed) in items {
                gh_set_file_viewed(&root, &pr_id, &path, viewed);
            }
        });
    }

    /// Open the base..HEAD diff for a PR file in a separate window.
    fn open_pr_diff(&mut self, path: PathBuf, _window: &mut Window, cx: &mut Context<Self>) {
        let files: Vec<PathBuf> = self.pr_files.iter().map(|(p, _)| p.clone()).collect();
        let Some(idx) = files.iter().position(|p| p == &path) else { return };
        let old = Some(diff_base_rev(&self.root, &self.pr_base));
        self.open_diff_window(files, idx, old, None, cx);
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
        // cmd+shift+c: copy the selected file/folder's repo-relative reference
        if ks.modifiers.platform && ks.modifiers.shift && ks.key == "c" {
            if let Some(p) = self.pr_selected.clone() {
                self.copy_reference(&p, cx);
            }
            return;
        }
        // cmd+shift+s: mark the selected file viewed, or all files under a
        // selected folder
        if ks.modifiers.platform && ks.modifiers.shift && ks.key == "s" {
            if let Some(sel) = self.pr_selected.clone() {
                let targets: Vec<PathBuf> = if self.pr_files.iter().any(|(p, _)| p == &sel) {
                    vec![sel]
                } else {
                    self.pr_files
                        .iter()
                        .filter(|(p, _)| p.starts_with(&sel))
                        .map(|(p, _)| p.clone())
                        .collect()
                };
                self.toggle_pr_viewed_all(&targets);
                cx.notify();
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
        self.open_diff_window(files, idx, old, new_rev, cx);
    }

    fn do_push(&mut self, cx: &mut Context<Self>) {
        self.push_open = false;
        let cmd = if self.push_tags { "git push --tags && git push" } else { "git push" };
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

    fn gitp_execute(&mut self, item: GitItem, window: &mut Window, cx: &mut Context<Self>) {
        // a branch doesn't act directly — it opens the per-branch action submenu
        if let GitItem::Branch(name) = item {
            self.gitp_action_branch = Some(name);
            self.gitp_action_sel = 0;
            cx.notify();
            return;
        }
        self.gitp_open = false;
        match item {
            GitItem::Action(GitAction::Update, ..) => self.run_command("git pull".into(), cx),
            GitItem::Action(GitAction::Commit, ..) => {
                self.left_view = LeftView::Changes;
                self.show_left = true;
                window.focus(&self.commit_focus, cx);
                cx.notify();
            }
            GitItem::Action(GitAction::Push, ..) => self.run_command("git push".into(), cx),
            GitItem::Action(GitAction::Pr, ..) => self.run_command("pro".into(), cx),
            GitItem::Action(GitAction::CreatePr, ..) => self.open_pr_create_prompt(window, cx),
            GitItem::Action(GitAction::NewBranch, ..) => self.open_branch_prompt(window, cx),
            GitItem::Branch(_) => {}
        }
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
        let _ = window;
        match action {
            BranchAction::Checkout => self.run_command(format!("git checkout {}", branch), cx),
            BranchAction::Merge => self.run_command(format!("git merge {}", branch), cx),
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
                    if let Some(&(action, ..)) = BRANCH_ACTIONS.get(self.gitp_action_sel) {
                        self.gitp_run_branch_action(action, window, cx);
                    }
                }
                "down" => {
                    self.gitp_action_sel =
                        (self.gitp_action_sel + 1).min(BRANCH_ACTIONS.len().saturating_sub(1));
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
    fn act_copy_reference(&mut self, _: &CopyReference, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get(self.active) {
            let line = tab.editor.read(cx).cursor_line();
            let reference = format!("{}:{}", self.rel(&tab.path), line);
            cx.write_to_clipboard(ClipboardItem::new_string(reference));
            self.show_flash("Reference copied", cx);
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
    fn changes_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        if ks.modifiers.platform && ks.modifiers.shift && ks.key == "c" {
            if let Some(p) = self.commit_selected.clone() {
                self.copy_reference(&p, cx);
            }
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
    fn palette_items(&self) -> Vec<(Cmd, &'static str, &'static str, &'static str)> {
        let mut scored: Vec<(i32, (Cmd, &str, &str, &str))> = PALETTE
            .iter()
            .filter_map(|&(c, label, icon, hint)| {
                fuzzy_score(&self.palette_query.text, label).map(|s| (s, (c, label, icon, hint)))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, it)| it).collect()
    }

    fn palette_execute(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        match cmd {
            Cmd::WipCommit => self.run_command("wip".into(), cx),
            Cmd::WipPush => self.run_command("wipp".into(), cx),
            Cmd::OpenPr => self.run_command("pro".into(), cx),
            Cmd::CreatePr => self.open_pr_create_prompt(window, cx),
            Cmd::Build => self.run_command("yyb".into(), cx),
            Cmd::GitAdd => self.run_command("gaa".into(), cx),
            Cmd::Pull => self.run_command("git pull".into(), cx),
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
            Cmd::ProcessManager => self.open_process_manager(window, cx),
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
        self.proc_ws_pids = self.terminals.iter().filter_map(|t| t.read(cx).child_pid()).collect();
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
            // cmd+a selects all of the preview code; cmd+c copies the selection
            "a" if ks.modifiers.platform => {
                if let Some(pv) = &self.find_preview {
                    if !pv.lines.is_empty() {
                        let last = pv.lines.len() - 1;
                        let last_len = pv.lines[last].chars().count();
                        self.find_psel = Some((0, 0, last, last_len));
                    }
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
            format!("{} && git commit -m \"{}\" && git push", add, safe)
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
            self.term_width = (self.win_width - f32::from(ev.position.x)).clamp(200., 1125.);
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
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.resizing
            || self.resizing_term
            || self.find_resize.is_some()
            || self.find_moving
            || self.find_split_dragging
        {
            self.resizing = false;
            self.resizing_term = false;
            self.find_resize = None;
            self.find_moving = false;
            self.find_split_dragging = false;
            for t in &self.terminals {
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

// ── render ───────────────────────────────────────────────────────────────

impl Render for Storm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let win = window.viewport_size();
        self.win_width = f32::from(win.width);
        self.win_height = f32::from(win.height);
        let show_term = self.show_terminal && !self.terminals.is_empty();

        // Focus the root once so global shortcuts (cmd+shift+o, cmd+l, opt+f12)
        // dispatch even before any file/tab is open.
        if !self.inited {
            self.inited = true;
            window.focus(&self.focus, cx);
        }

        // WebStorm-style title: "<project> - <open file>"
        let project = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "tide".into());
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
        let bottom = self.render_bottom(cx);
        let left_panel = if self.show_left {
            Some(match self.left_view {
                LeftView::Files => self.render_tree(window, cx).into_any_element(),
                LeftView::Changes => self.render_changes(window, cx).into_any_element(),
                LeftView::PullRequest => self.render_pr(window, cx).into_any_element(),
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

        if show_term {
            let term = self.terminals[self.active_term].clone();
            let term_tabs = self.render_term_tabs(cx);
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
                                for t in &this.terminals {
                                    t.update(cx, |t, _| t.defer_resize = true);
                                }
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        .w(px(self.term_width))
                        .h_full()
                        .flex_shrink_0()
                        .flex()
                        .flex_col()
                        .bg(rgb(PANEL_BG))
                        .child(term_tabs)
                        .child(div().flex_grow(1.0).child(term)),
                );
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
            .on_action(cx.listener(Self::act_goto_commit))
            .on_action(cx.listener(Self::act_show_diff))
            .on_action(cx.listener(Self::act_find_in_files))
            .on_action(cx.listener(Self::act_git_popup))
            .on_action(cx.listener(Self::act_command_palette))
            .on_action(cx.listener(Self::act_copy_reference))
            .on_action(cx.listener(Self::act_open_on_github))
            .on_action(cx.listener(Self::act_push_dialog))
            .on_action(cx.listener(Self::act_run_command))
            .on_action(cx.listener(Self::act_new_project))
            .on_action(cx.listener(Self::act_fetch))
            .on_action(cx.listener(Self::act_pull))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .child(topbar)
            .child(middle)
            .when_some(run_dock, |d, dock| d.child(dock))
            .child(bottom);

        if self.finder_open {
            root = root.child(self.render_finder(cx));
        }
        if self.goto_open {
            root = root.child(self.render_goto(cx));
        }
        if self.br_open {
            root = root.child(self.render_branch_prompt(cx));
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
        if self.ws_open {
            root = root.child(self.render_project_dropdown(cx));
        }
        if self.run_active && !self.run_open {
            root = root.child(self.render_run_toast(cx));
        }
        if let Some(msg) = self.flash.clone() {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(34.))
                    .right(px(16.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(rgb(POPUP_BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .shadow_lg()
                    .child(div().text_color(rgb(GIT_NEW)).child("✓"))
                    .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child(msg)),
            );
        }

        root
    }
}

impl Storm {
    fn render_editor_wrap(&self, editor: impl IntoElement, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_grow(1.0)
            .min_w(px(0.))
            .h_full()
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

    fn render_topbar(&self, project: String, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .child(div().text_color(rgb(MUTED)).text_size(px(9.)).child("▾"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.ws_open = !this.ws_open;
                                cx.notify();
                            })),
                    )
                    .when(!self.branch.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .text_color(rgb(MUTED))
                                .text_size(px(12.))
                                .child(format!("⎇ {}", self.branch))
                                // copy-to-clipboard button for the branch name
                                .child(
                                    div()
                                        .id("copy-branch")
                                        .font_family(ICON_FONT)
                                        .text_size(px(11.))
                                        .px_1()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(rgb(MUTED))
                                        .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                        .child(IC_COPY)
                                        .tooltip(|_w, cx| {
                                            cx.new(|_| TooltipView { text: "Copy branch name".into() }).into()
                                        })
                                        .on_click(cx.listener(|this, _e, _w, cx| {
                                            if !this.branch.is_empty() {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    this.branch.clone(),
                                                ));
                                                this.show_flash("Branch name copied", cx);
                                            }
                                        })),
                                ),
                        )
                    })
                    // PR link for the current branch, if one exists → opens it
                    .when_some(self.pr_link.clone(), |d, (num, url, status)| {
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
                                .on_click(cx.listener(move |_this, _e, _w, _cx| {
                                    let _ = Command::new("open").arg(&url).spawn();
                                })),
                        )
                    })
                    // refresh the PR link + checks status (otherwise only updates
                    // on branch change / cmd+R)
                    .when(!self.branch.is_empty(), |d| {
                        d.child(
                            div()
                                .id("pr-refresh")
                                .px_1()
                                .rounded_md()
                                .cursor_pointer()
                                .text_size(px(12.))
                                .text_color(rgb(MUTED))
                                .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                                .child("↻")
                                .tooltip(|_w, cx| {
                                    cx.new(|_| TooltipView { text: "Refresh PR status".into() }).into()
                                })
                                .on_click(cx.listener(|this, _e, _w, cx| this.refresh_pr_link(cx))),
                        )
                    }),
            );
        // project quick-switch icons (2+ projects only), centered in the bar.
        // Each icon rests colorless when idle and tints green as it sees activity.
        if self.ws_names.len() > 1 {
            let mut strip = div().flex().flex_row().items_center().gap_1();
            for (i, name) in self.ws_names.iter().enumerate() {
                let active = i == self.ws_active;
                let idle = self.ws_idle.get(i).copied().unwrap_or(0.0);
                // freshness: 1.0 right after activity, fading to 0 as it goes idle
                let fresh = (1.0 - idle / ACTIVE_FADE_SECS).clamp(0.0, 1.0);
                // the viewed project keeps its selected (blue) bg; others rest at
                // the bar color (invisible when dead) and tint green when active
                let base_bg = if active {
                    ICON_SELECTED_BG
                } else {
                    lerp_rgb(PANEL_BG, ACTIVE_GREEN, fresh)
                };
                // change-pulse: a quick flash that decays over PULSE_SECS, layered
                // over the base color (ease-out so it pops then fades)
                let pulse = self.ws_pulse.get(i).copied().unwrap_or(1e9);
                let pint = (1.0 - (pulse / PULSE_SECS)).clamp(0.0, 1.0);
                let pint = pint * pint; // ease-out
                let bg = lerp_rgb(base_bg, PULSE_COLOR, pint);
                let ring_alpha = (pint * 255.0) as u32 & 0xff;
                let text = if active || fresh > 0.4 || pint > 0.3 { SEL_TEXT } else { MUTED };
                let label = project_icon_label(name);
                let nm = name.clone();
                let idx = i;
                strip = strip.child(
                    div()
                        .id(("topbar-proj", i))
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .text_size(px(10.))
                        // 2px ring, always present but transparent when not pulsing
                        // (keeps the box size stable — no layout jump on pulse)
                        .border_2()
                        .border_color(gpui::rgba((PULSE_COLOR << 8) | ring_alpha))
                        .bg(rgb(bg))
                        .text_color(rgb(text))
                        .cursor_pointer()
                        .child(label)
                        .tooltip(move |_w, cx| cx.new(|_| TooltipView { text: nm.clone().into() }).into())
                        .on_click(cx.listener(move |_this, _e, _w, cx| {
                            cx.emit(ProjectNav::Switch(idx));
                            cx.notify();
                        })),
                );
            }
            // strip in the middle + an equal-grow spacer on the right → centered
            bar = bar.child(strip).child(div().flex_1());
        } else {
            // keep the left group left-aligned when there's no strip
            bar = bar.child(div().flex_1());
        }
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
            .child(activity_icon("act-term", IC_TERMINAL, "Terminal  (⌥F12)", self.show_terminal, 0, cx.listener(|this, _ev, window, cx| {
                this.toggle_terminal(window, cx);
            })))
            .child(activity_icon("act-run", IC_RUN, "Run console", self.run_open, 0, cx.listener(|this, _ev, _w, cx| {
                this.run_open = !this.run_open;
                cx.notify();
            })))
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

        for ix in 0..self.terminals.len() {
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
                    .child(
                        div()
                            .id(("term-tab", ix))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(if active { TEXT } else { MUTED }))
                            .child(format!("Terminal {}", ix + 1))
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
        bar.child(
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
        )
    }

    /// Bottom-right status toast for a running/finished command. Click to open
    /// the full Run console.
    fn render_run_toast(&self, cx: &mut Context<Self>) -> impl IntoElement {
        const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let (glyph, color, msg) = if self.run_running {
            (SPIN[self.run_spin % SPIN.len()].to_string(), ACCENT, format!("Running  {}", self.run_cmd))
        } else if self.run_failed {
            ("✕".to_string(), GIT_DELETED, format!("{} failed", self.run_cmd))
        } else {
            ("✓".to_string(), GIT_NEW, format!("{} done", self.run_cmd))
        };
        div()
            .id("run-toast")
            .absolute()
            .bottom(px(34.))
            .right(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .cursor_pointer()
            .child(div().text_color(rgb(color)).child(glyph))
            .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child(msg))
            .on_click(cx.listener(|this, _e, _w, cx| {
                this.run_open = true;
                this.run_active = false;
                cx.notify();
            }))
    }

    /// The read-only Run console: a bottom dock streaming command output.
    fn render_run(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = self.run_running;
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
            .border_color(rgb(BORDER))
            .child(
                div()
                    .font_family(ICON_FONT)
                    .text_size(px(12.))
                    .text_color(rgb(if running { ACCENT } else { MUTED }))
                    .child(IC_RUN),
            )
            .child(div().text_size(px(12.)).text_color(rgb(TEXT)).child(self.run_cmd.clone()))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(if running { ACCENT } else { MUTED }))
                    .child(if running { "running…" } else { "done" }),
            )
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

        let list = uniform_list(
            "run-log",
            self.run_lines.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _w, _cx| {
                range
                    .map(|i| div().px_3().child(this.run_lines[i].clone()).into_any_element())
                    .collect()
            }),
        )
        .track_scroll(&self.run_scroll)
        .flex_grow(1.0)
        .font_family("Menlo")
        .text_size(px(12.))
        .text_color(rgb(TEXT))
        .bg(rgb(BG));

        div()
            .h(px(260.))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(header)
            .child(list)
    }

    fn render_bottom(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.active_path().map(|p| self.rel(p)).unwrap_or_default();
        div()
            .h(px(24.))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_t_1()
            .border_color(rgb(BORDER))
            .text_size(px(12.))
            .child(div().text_color(rgb(MUTED)).child(path))
            .child(div().text_color(rgb(MUTED)).child(format!("{} MB", self.mem_mb)))
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
                    let is_open = active_path.as_ref() == Some(&path);
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
                                    .on_click(cx.listener(move |this, _e, window, cx| {
                                        this.open_file(path_open.clone(), window, cx);
                                        cx.notify();
                                    })),
                            ),
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
                    .child(format!("COMMIT  ·  {} changed", count)),
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
                        div().text_size(px(12.)).text_color(rgb(MUTED)).child(format!("Filter files…{}", self.caret_if(filter_focused)))
                    } else {
                        div().text_size(px(12.)).text_color(rgb(TEXT)).child(self.commit_filter.render(self.caret_if(filter_focused), SELECTION))
                    }),
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
                    }),
            )
            // filter bar
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
                        div().text_size(px(12.)).text_color(rgb(MUTED)).child(format!("Filter files…{}", self.caret_if(filter_focused)))
                    } else {
                        div().text_size(px(12.)).text_color(rgb(TEXT)).child(self.pr_filter.render(self.caret_if(filter_focused), SELECTION))
                    }),
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
        let (_, excludes) = parse_search_query(&self.find_query.text);

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
                        div().text_color(rgb(MUTED)).child(format!("Search in files…{}", self.caret()))
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
            .when(!excludes.is_empty(), |d| {
                d.child(
                    div()
                        .text_color(rgb(GIT_DELETED))
                        .truncate()
                        .child(format!("· excluding {}", excludes.join(", "))),
                )
            });

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
                    .on_click(cx.listener(move |this, _ev, _w, cx| {
                        this.find_selected = i;
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
                let styles = highlighter().highlight(&content, &r.path);
                let lines = content.split('\n').map(|s| s.to_string()).collect();
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
                            .child(*label),
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

        for (i, &(action, label, icon)) in BRANCH_ACTIONS.iter().enumerate() {
            let sel = i == self.gitp_action_sel;
            // dynamic, WebStorm-style label (e.g. "Merge 'next' into 'mine'")
            let display = match action {
                BranchAction::Merge => format!("Merge '{}' into '{}'", branch, self.branch),
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

    /// Process Manager dialog: filterable list of running processes with
    /// multi-select (cmd/shift-click) and kill.
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
    fn editor_ctx_actions() -> [(&'static str, fn(&mut Self, &mut Window, &mut Context<Self>)); 4] {
        [
            ("Reveal in Dir Tree", |this, window, cx| this.reveal_active_in_tree(window, cx)),
            ("Copy Path", |this, _w, cx| {
                if let Some(p) = this.active_path().cloned() {
                    cx.write_to_clipboard(ClipboardItem::new_string(p.to_string_lossy().to_string()));
                    this.show_flash("Path copied", cx);
                }
            }),
            ("Copy Reference", |this, _w, cx| {
                if let Some(p) = this.active_path().cloned() {
                    this.copy_reference(&p, cx);
                }
            }),
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

    /// Dir-tree right-click actions. Operate on `tree_ctx_path`.
    fn tree_ctx_actions() -> [(&'static str, fn(&mut Self, &mut Window, &mut Context<Self>)); 2] {
        [
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
            ("Delete", |this, window, cx| {
                if let Some(p) = this.tree_ctx_path.clone() {
                    this.confirm_delete = Some(p);
                    window.focus(&this.confirm_focus, cx);
                }
            }),
        ]
    }

    fn tree_ctx_run(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        let actions = Self::tree_ctx_actions();
        if let Some((_, run)) = actions.get(idx) {
            run(self, window, cx);
            self.tree_ctx = None;
            self.tree_ctx_path = None;
            cx.notify();
        }
    }

    fn tree_ctx_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let key = ev.keystroke.key.as_str();
        if key == "escape" {
            self.tree_ctx = None;
            self.tree_ctx_path = None;
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
        let actions = Self::tree_ctx_actions();
        let menu_w = 200.0_f32;
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
                    .mb_3()
                    .px_2()
                    .py_1()
                    .bg(rgb(BG))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .rounded_md()
                    .text_color(rgb(TEXT))
                    .child(self.br_query.render(self.caret(), SELECTION)),
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
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child("Enter to create · Esc to cancel"),
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
                        div().text_color(rgb(MUTED)).child(format!("{}/path/to/folder", self.caret()))
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

        // recent projects (most recent first) — click to open
        let mut recents = div().id("newproj-recents").flex().flex_col().max_h(px(280.)).overflow_y_scroll().pb_2();
        if !self.newproj_recents.is_empty() {
            recents = recents.child(
                div()
                    .px_3()
                    .py_1()
                    .text_size(px(11.))
                    .text_color(rgb(MUTED))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child("Recent"),
            );
            for (i, p) in self.newproj_recents.iter().take(10).enumerate() {
                let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                let dir = p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default();
                let path = p.clone();
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
                        .hover(|s| s.bg(rgb(HOVER)))
                        .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(FOLDER_ICON)).child(IC_FOLDER))
                        .child(div().flex_shrink_0().text_size(px(12.)).text_color(rgb(TEXT)).child(name))
                        .child(div().flex_grow(1.0).text_size(px(11.)).text_color(rgb(MUTED)).truncate().child(dir))
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
                    .child("Open Project — folder path"),
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

/// Build a tooltip closure for a static label.
/// Seconds since the last activity at which a project icon fades back to its
/// resting (colorless) state. (1800.0 = 30 min; currently 5 min.)
const ACTIVE_FADE_SECS: f32 = 300.0;
/// The "active" color icons tint toward right after activity (fades as it idles).
const ACTIVE_GREEN: u32 = 0x6aaf6a;
/// How long a change-pulse animation lasts, and its flash color.
const PULSE_SECS: f32 = 0.7;
const PULSE_COLOR: u32 = 0x4ec9b0;

/// Linear blend between two 0xRRGGBB colors (t clamped to 0..1).
fn lerp_rgb(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let chan = |sh: u32| {
        let ca = ((a >> sh) & 0xff) as f32;
        let cb = ((b >> sh) & 0xff) as f32;
        (ca + (cb - ca) * t).round() as u32
    };
    (chan(16) << 16) | (chan(8) << 8) | chan(0)
}

/// Two-letter badge for a project icon: first + last non-space char, uppercased
/// (e.g. "wby-next1" → "W1", "wcp" → "WP", "a" → "A").
fn project_icon_label(name: &str) -> String {
    let chars: Vec<char> = name.chars().filter(|c| !c.is_whitespace()).collect();
    match chars.as_slice() {
        [] => "?".to_string(),
        [only] => only.to_uppercase().to_string(),
        [first, .., last] => format!("{}{}", first.to_uppercase(), last.to_uppercase()),
    }
}

fn tip(text: &'static str) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_w, cx| cx.new(|_| TooltipView { text: text.into() }).into()
}

/// A centered icon button (top bar) rendered with the Codicon font.
fn toolbar_btn(
    id: &'static str,
    glyph: &'static str,
    tooltip: &'static str,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .font_family(ICON_FONT)
        .text_size(px(16.))
        .text_color(rgb(MUTED))
        .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
        .cursor_pointer()
        .child(glyph)
        .tooltip(tip(tooltip))
        .on_click(on_click)
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

    fn render_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.entries.len();
        let width = px(self.tree_width);
        let focused = self.tree_focus.is_focused(window);
        let filtering = !self.tree_filter.is_empty();
        // "5/N" while filtering, just the visible row count otherwise
        let count_label =
            if filtering { format!("{}/{}", count, self.tree_total) } else { format!("{}", count) };

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
                    .child("PROJECT"),
            )
            // type-to-filter bar: only shown while a filter is active (typing in
            // the focused tree), so the empty bar doesn't take up space
            .when(filtering, |d| {
                d.child(
                    div()
                        .id("tree-filter")
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .h(px(28.))
                        .px_3()
                        .border_b_1()
                        .border_color(rgb(BORDER))
                        .child(div().font_family(ICON_FONT).text_size(px(12.)).text_color(rgb(MUTED)).child(IC_SEARCH))
                        .child(
                            div()
                                .flex_grow(1.0)
                                .text_size(px(12.))
                                .text_color(rgb(TEXT))
                                .child(self.tree_filter.render(self.caret_if(focused), SELECTION)),
                        )
                        .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(count_label)),
                )
            })
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
                                    .child(div().text_color(fg).child(name))
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
            let editor = self.tabs[self.active].editor.clone();
            col = col.child(div().flex_grow(1.0).child(editor));
        } else {
            col = col.child(
                div()
                    .flex_grow(1.0)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(MUTED))
                    .child("Select a file"),
            );
        }

        col
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
struct Workspace {
    projects: Vec<Entity<Storm>>,
    active: usize,
    focus_pending: bool,
    focus: FocusHandle,
    // project-switcher dialog
    switcher_open: bool,
    switcher_sel: usize,
    switcher_focus: FocusHandle,
    // idle reset: which project has been on-screen, and since when. Once the
    // active project has been viewed ≥5s, its idle timer keeps resetting.
    prev_active: usize,
    active_since: Instant,
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
            prev_active: 0,
            active_since: Instant::now(),
        };
        // open every passed root as its own workspace; first one is active
        for root in roots {
            ws.add_project(root, cx);
        }
        ws.active = 0;
        // Slow repaint so the idle fade advances over time. Pulses are animated
        // separately (event-driven, see pulse_anim) so they don't depend on this.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(3)).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        })
        .detach();
        ws
    }

    /// Smoothly animate the topbar for the duration of a change-pulse: notify at
    /// ~30fps for PULSE_SECS so the flash decays smoothly, then stop.
    fn pulse_anim(&self, cx: &mut Context<Self>) {
        cx.notify(); // show the peak immediately
        cx.spawn(async move |this, cx| {
            let frames = (PULSE_SECS * 30.0) as u32 + 2;
            for _ in 0..frames {
                cx.background_executor().timer(Duration::from_millis(33)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn add_project(&mut self, root: PathBuf, cx: &mut Context<Self>) {
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
            ProjectNav::Open => this.open_project(cx),
            ProjectNav::OpenPath(p) => this.add_project(p.clone(), cx),
            ProjectNav::Remove(i) => this.remove_project(*i, cx),
            ProjectNav::Activity => this.pulse_anim(cx),
        })
        .detach();
        self.projects.push(storm);
        self.active = self.projects.len() - 1;
        self.focus_pending = true;
        cx.notify();
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
        cx.notify();
    }

    fn next_project(&mut self, _: &NextProject, _w: &mut Window, cx: &mut Context<Self>) {
        let n = self.projects.len();
        if n > 1 {
            self.active = (self.active + 1) % n;
            self.focus_pending = true;
            cx.notify();
        }
    }

    fn prev_project(&mut self, _: &PrevProject, _w: &mut Window, cx: &mut Context<Self>) {
        let n = self.projects.len();
        if n > 1 {
            self.active = (self.active + n - 1) % n;
            self.focus_pending = true;
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
        self.switcher_sel = self.active;
        window.focus(&self.switcher_focus, cx);
        cx.notify();
    }

    fn switcher_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let n = self.projects.len();
        let key = ev.keystroke.key.as_str();
        // press a digit to jump straight to that workspace
        if let Some(d) = key.parse::<usize>().ok().filter(|d| *d >= 1 && *d <= n) {
            self.switcher_open = false;
            self.switch_to(d - 1, cx);
            return;
        }
        match key {
            // x / delete / backspace closes the highlighted workspace (stays open
            // so you can close several); closing the last one is a no-op
            "x" | "delete" | "backspace" => {
                self.remove_project(self.switcher_sel, cx);
                if self.projects.len() <= 1 {
                    self.switcher_open = false;
                    self.focus_pending = true;
                } else {
                    // stay in the switcher; keep focus here (remove_project set
                    // focus_pending, which would otherwise jump to a project)
                    self.focus_pending = false;
                }
                cx.notify();
            }
            "escape" => {
                self.switcher_open = false;
                self.focus_pending = true;
                cx.notify();
            }
            "down" => {
                self.switcher_sel = (self.switcher_sel + 1).min(n.saturating_sub(1));
                cx.notify();
            }
            "up" => {
                self.switcher_sel = self.switcher_sel.saturating_sub(1);
                cx.notify();
            }
            "enter" => {
                self.switcher_open = false;
                self.switch_to(self.switcher_sel, cx); // focus_pending re-focuses the project
            }
            _ => {}
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

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let active = self.active;
        // track how long the active project has been on screen; once it's been
        // viewed ≥5s, keep resetting its idle timer so it stays "fresh"
        if active != self.prev_active {
            self.prev_active = active;
            self.active_since = Instant::now();
        }
        if self.active_since.elapsed() >= Duration::from_secs(5) {
            self.projects[active].update(cx, |s, _| s.last_active = Instant::now());
        }

        // push the project list into the active view so its dropdown can show it
        let names: Vec<String> = self.projects.iter().map(|p| p.read(cx).project_name()).collect();
        let branches: Vec<String> = self.projects.iter().map(|p| p.read(cx).branch.clone()).collect();
        let idle: Vec<f32> = self.projects.iter().map(|p| p.read(cx).idle_secs()).collect();
        let pulse: Vec<f32> = self.projects.iter().map(|p| p.read(cx).pulse_secs()).collect();
        self.projects[active].update(cx, |s, _| {
            s.ws_names = names;
            s.ws_branches = branches;
            s.ws_idle = idle;
            s.ws_pulse = pulse;
            s.ws_active = active;
        });
        // focus the active project after a switch (render has the Window)
        if self.focus_pending {
            self.focus_pending = false;
            let storm = self.projects[active].clone();
            storm.update(cx, |s, cx| s.focus_active(window, cx));
        }

        let mut root = div()
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::next_project))
            .on_action(cx.listener(Self::prev_project))
            .on_action(cx.listener(Self::open_project_action))
            .on_action(cx.listener(Self::show_projects))
            .child(self.projects[active].clone());

        if self.switcher_open {
            let win = window.viewport_size();
            let w = 560.0_f32;
            let left = ((f32::from(win.width) - w) / 2.0).max(0.);
            let mut panel = div()
                .absolute()
                .top(px(120.))
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
                .track_focus(&self.switcher_focus)
                .on_key_down(cx.listener(Self::switcher_key))
                .child(
                    div()
                        .px_3()
                        .py_1()
                        .text_size(px(11.))
                        .text_color(rgb(MUTED))
                        .child("Switch Project  ·  press a number  ·  x to close"),
                );
            for i in 0..self.projects.len() {
                let p = &self.projects[i];
                let (name, branch) = {
                    let s = p.read(cx);
                    (s.project_name(), s.branch.clone())
                };
                let sel = i == self.switcher_sel;
                let is_active = i == active;
                let idx = i;
                panel = panel.child(
                    div()
                        .id(("ws-switch", i))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .h(px(46.))
                        .px_3()
                        // number shortcut badge
                        .child(
                            div()
                                .w(px(18.))
                                .flex()
                                .justify_center()
                                .text_size(px(12.))
                                .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                                .child(format!("{}", i + 1)),
                        )
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
                            // name + branch stacked
                            div()
                                .flex()
                                .flex_col()
                                .flex_grow(1.0)
                                .child(
                                    div()
                                        .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                                        .child(name),
                                )
                                .when(!branch.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(if sel { SEL_TEXT } else { MUTED }))
                                            .child(format!("⎇ {}", branch)),
                                    )
                                }),
                        )
                        .when(is_active, |d| {
                            d.child(div().text_size(px(11.)).text_color(rgb(ACCENT)).child("●"))
                        })
                        // close button (hidden when only one project is left)
                        .when(self.projects.len() > 1, |d| {
                            d.child(
                                div()
                                    .id(("ws-close", i))
                                    .px_1()
                                    .text_size(px(12.))
                                    .text_color(rgb(MUTED))
                                    .hover(|s| s.text_color(rgb(0xf7768e)))
                                    .cursor_pointer()
                                    .child("✕")
                                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                                    .on_click(cx.listener(move |this, _e, _w, cx| {
                                        this.remove_project(idx, cx);
                                    })),
                            )
                        })
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            this.switcher_open = false;
                            this.switch_to(idx, cx);
                        })),
                );
            }
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
}

impl DiffWindow {
    fn new(
        root: PathBuf,
        files: Vec<PathBuf>,
        idx: usize,
        old: Option<String>,
        new_rev: Option<String>,
        storm: WeakEntity<Storm>,
        main_window: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        let hl = Highlighter::new();
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
        }
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
            self.recompute_matches();
            cx.notify();
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
        // when the search input has focus, let search_key own the keystroke
        // (events bubble up to this root handler otherwise)
        if self.search_open && self.search_focus.is_focused(window) {
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
            "down" | "right" => {
                if self.idx + 1 < self.files.len() {
                    self.goto(self.idx + 1, cx);
                }
            }
            "up" | "left" => {
                if self.idx > 0 {
                    self.goto(self.idx - 1, cx);
                }
            }
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
                        div().text_color(rgb(MUTED)).child("Find in diff…▏")
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
        let pos = format!("{}/{}", self.idx + 1, self.files.len());
        let has_prev = self.idx > 0;
        let has_next = self.idx + 1 < self.files.len();
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
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        if this.idx > 0 {
                            this.goto(this.idx - 1, cx);
                        }
                    })),
            )
            .child(
                div()
                    .id("dw-next")
                    .px_2()
                    .cursor_pointer()
                    .text_color(rgb(if has_next { TEXT } else { MUTED }))
                    .hover(|s| s.text_color(rgb(ACCENT)))
                    .child("›")
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        if this.idx + 1 < this.files.len() {
                            this.goto(this.idx + 1, cx);
                        }
                    })),
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
            // drag to select text
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                    if let Some((side, r, c)) = this.cell_at(ev.position) {
                        this.sel = Some(DiffSel { side, anchor: (r, c), head: (r, c) });
                        this.caret = Some((side, r, c));
                        this.caret_on = true;
                        this.dragging = true;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
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
            .on_mouse_up(MouseButton::Left, cx.listener(|this, _e, _w, _cx| this.dragging = false))
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

        col.child(diff_body(
                &self.rows,
                &self.left_scroll,
                &self.right_scroll,
                self.char_w,
                self.sel.as_ref(),
                &self.left_styles,
                &self.right_styles,
                &self.matches,
                self.cur_match,
                self.caret,
                self.caret_on,
            ))
    }
}

fn main() {
    // each CLI arg is a project root opened as its own workspace; default to cwd
    let roots: Vec<PathBuf> = {
        let mut v: Vec<PathBuf> = std::env::args()
            .skip(1)
            .filter_map(|a| std::fs::canonicalize(&a).ok())
            .filter(|p| p.is_dir())
            .collect();
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
            // in-file search
            KeyBinding::new("cmd-f", SearchOpen, Some("Editor")),
            // copy reference (relpath:line)
            KeyBinding::new("cmd-shift-c", CopyReference, Some("Editor")),
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
            // undo / redo
            KeyBinding::new("cmd-z", Undo, Some("Editor")),
            KeyBinding::new("cmd-shift-z", Redo, Some("Editor")),
            // line ops
            KeyBinding::new("cmd-backspace", DeleteLine, Some("Editor")),
            KeyBinding::new("alt-shift-up", MoveLineUp, Some("Editor")),
            KeyBinding::new("alt-shift-down", MoveLineDown, Some("Editor")),
            // terminal toggle (global)
            KeyBinding::new("alt-f12", ToggleTerminal, None),
            // terminal tab management (when terminal focused)
            KeyBinding::new("cmd-t", NewTerminal, Some("Terminal")),
            KeyBinding::new("cmd-w", CloseTerminalTab, Some("Terminal")),
            KeyBinding::new("cmd-shift-w", CloseOtherTerminals, Some("Terminal")),
            // fuzzy file finder (global)
            KeyBinding::new("cmd-shift-o", OpenFinder, None),
            // go to line (global)
            KeyBinding::new("cmd-l", GotoLine, None),
            // jump to commit pane with the active file checked (global)
            KeyBinding::new("cmd-k", GotoCommit, None),
            KeyBinding::new("cmd-shift-k", PushDialog, None),
            KeyBinding::new("cmd-shift-t", RunCommand, None),
            // find in files (global)
            KeyBinding::new("cmd-shift-f", FindInFiles, None),
            // git branches/actions popup (global)
            KeyBinding::new("alt-b", GitPopup, None),
            KeyBinding::new("alt-f", FetchRemotes, None),
            KeyBinding::new("alt-l", PullRemote, None),
            // command palette (global)
            KeyBinding::new("cmd-shift-p", CommandPalette, None),
            KeyBinding::new("cmd-shift-h", OpenOnGithub, None),
            // multi-project workspace (global)
            KeyBinding::new("alt-tab", NextProject, None),
            KeyBinding::new("alt-shift-tab", PrevProject, None),
            KeyBinding::new("cmd-shift-n", NewProject, None),
            KeyBinding::new("cmd-e", ShowProjects, None),
            // diff (opens in its own window)
            KeyBinding::new("cmd-d", ShowDiff, None),
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
