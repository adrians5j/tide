use gpui::{
    App, Bounds, Context, DispatchPhase, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    CursorStyle, FocusHandle, Focusable, GlobalElementId, InspectorElementId, KeyDownEvent, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollWheelEvent,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window, actions, div, fill, font,
    point, prelude::*, px, relative, rgb, size, ClipboardItem,
};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::field::{Edit, Field};
use crate::lsp::{parse_completions, CompItem, Lsp};
use crate::syntax::Highlighter;
use crate::theme::*;

#[derive(Clone)]
struct Snapshot {
    content: String,
    cursor: usize,
}

actions!(
    editor,
    [
        Backspace, Delete, MoveLeft, MoveRight, MoveUp, MoveDown, Home, End, Newline, Indent, Save,
        SelectLeft, SelectRight, SelectUp, SelectDown, SelectHome, SelectEnd, SelectAll, Copy,
        Paste, Cut, WordLeft, WordRight, SelectWordLeft, SelectWordRight, Undo, Redo, DeleteLine,
        MoveLineUp, MoveLineDown, CompTrigger, CompDismiss, GotoDef, SearchOpen
    ]
);

/// Emitted when the editor wants the workspace to open a file at a position
/// (e.g. go-to-definition). The workspace handles opening + cursor placement.
pub struct OpenLocation {
    pub path: PathBuf,
    pub line: usize, // 1-based
    pub col: usize,  // 1-based
}

impl gpui::EventEmitter<OpenLocation> for Editor {}

const FONT: &str = "Menlo";
const FONT_SIZE: f32 = 13.0;
const LINE_HEIGHT: f32 = 20.0;
const GUTTER_PAD: f32 = 16.0; // px between gutter numbers and code

pub struct Editor {
    pub focus_handle: FocusHandle,
    pub path: Option<PathBuf>,
    content: String,
    /// cursor/selection as byte offsets into `content`; cursor is `end` (or `start` if reversed)
    selected_range: Range<usize>,
    selection_reversed: bool,
    /// per-line syntect runs: (byte_len, 0xRRGGBB)
    styles: Vec<Vec<(usize, u32)>>,
    scroll_y: Pixels,
    scroll_x: Pixels,
    hl: Highlighter,
    dirty: bool,
    read_only: bool, // when true, all edits are blocked (nav/select/copy still work)
    ro_hint: bool,   // briefly show a "read-only" hint at the cursor on a blocked edit
    ro_hint_gen: u64,
    blink_on: bool,
    blink_epoch: usize,
    undo_stack: Vec<Snapshot>,
    redo_stack: Vec<Snapshot>,
    typing_run: bool,
    /// true while a left-drag text selection is in progress
    selecting: bool,

    // last-seen on-disk mtime, to detect external edits (another tool changing
    // the file while it's open here) and reload instead of showing stale content
    disk_mtime: Option<SystemTime>,
    // the file changed on disk while we had unsaved edits → show the reload prompt
    conflict: bool,

    // layout cache (filled during paint) for mouse mapping
    last_bounds: Option<Bounds<Pixels>>,
    last_char_width: Pixels,
    last_gutter_width: Pixels,

    // LSP
    lsp: Option<Arc<Lsp>>,
    uri: String,
    version: i64,
    comp: Vec<CompItem>,
    comp_sel: usize,
    comp_open: bool,
    hover: Option<(String, f32, f32)>, // (text, x, y)
    hover_gen: u64,
    link: Option<Range<usize>>, // token shown as a cmd-clickable link

    // in-file search (cmd+f)
    search_open: bool,
    search_focus: FocusHandle,
    search_query: Field,
    search_matches: Vec<Range<usize>>,
    search_idx: usize,
    hl_gen: u64,
    // content changed since the last didChange we sent the language server
    lsp_dirty: bool,
    // generation counter for the debounced LSP sync
    sync_gen: u64,
}

impl Editor {
    pub fn new(lsp: Option<Arc<Lsp>>, cx: &mut Context<Self>) -> Self {
        Self {
            lsp,
            uri: String::new(),
            version: 0,
            comp: Vec::new(),
            comp_sel: 0,
            comp_open: false,
            hover: None,
            hover_gen: 0,
            link: None,
            search_open: false,
            search_focus: cx.focus_handle(),
            search_query: Field::default(),
            search_matches: Vec::new(),
            search_idx: 0,
            hl_gen: 0,
            lsp_dirty: false,
            sync_gen: 0,
            focus_handle: cx.focus_handle(),
            path: None,
            content: String::new(),
            selected_range: 0..0,
            selection_reversed: false,
            styles: Vec::new(),
            scroll_y: px(0.),
            scroll_x: px(0.),
            hl: Highlighter::new(),
            dirty: false,
            read_only: true, // read-only by default; the workspace sets it per editor
            ro_hint: false,
            ro_hint_gen: 0,
            blink_on: true,
            blink_epoch: 0,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            typing_run: false,
            selecting: false,
            disk_mtime: None,
            conflict: false,
            last_bounds: None,
            last_char_width: px(8.),
            last_gutter_width: px(56.),
        }
    }

    pub fn load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        self.styles = self.hl.highlight(&content, &path);
        // close the previously-open document on the server
        if let Some(lsp) = &self.lsp {
            if !self.uri.is_empty() {
                lsp.did_close(&self.uri);
            }
        }
        self.uri = format!("file://{}", path.display());
        self.version = 1;
        if let Some(lsp) = &self.lsp {
            lsp.did_open(&self.uri, lang_id(&path), self.version, &content);
        }
        self.content = content;
        self.path = Some(path);
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.selecting = false;
        self.scroll_y = px(0.);
        self.scroll_x = px(0.);
        self.dirty = false;
        self.comp_open = false;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.typing_run = false;
        self.disk_mtime = self.path.as_deref().and_then(file_mtime);
        self.restart_blink(cx);
        cx.notify();
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        // Only write when there are unsaved edits. Saving a clean buffer would
        // rewrite the file with our in-memory copy — which clobbers any change
        // another tool (e.g. Claude Code) made on disk while it was open here.
        if !self.dirty {
            return;
        }
        if let Some(path) = &self.path {
            if std::fs::write(path, &self.content).is_ok() {
                self.dirty = false;
                self.conflict = false; // our version is now on disk
                self.disk_mtime = file_mtime(path);
                cx.notify();
            }
        }
    }

    /// Replace the buffer with `content` and resync highlighting + the LSP.
    fn reload_from(&mut self, content: String, disk: Option<SystemTime>, path: &Path, cx: &mut Context<Self>) {
        self.content = content;
        self.styles = self.hl.highlight(&self.content, path);
        // keep the cursor in range; drop selection/undo from the old contents
        let caret = self.cursor_offset().min(self.content.len());
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.dirty = false;
        self.conflict = false;
        self.disk_mtime = disk;
        if let Some(lsp) = &self.lsp {
            if !self.uri.is_empty() {
                self.version += 1;
                lsp.did_change(&self.uri, self.version, &self.content);
            }
        }
        cx.notify();
    }

    /// React to an on-disk change. With no unsaved edits, reload silently; with
    /// unsaved edits, raise a conflict so the user can choose (see the banner).
    pub fn reload_if_changed(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else { return };
        let Some(disk) = file_mtime(&path) else { return };
        if Some(disk) == self.disk_mtime {
            return; // unchanged since we last read it
        }
        let Ok(content) = std::fs::read_to_string(&path) else { return };
        if content == self.content {
            self.disk_mtime = Some(disk); // mtime moved but bytes are identical
            return;
        }
        if self.dirty {
            // external change collides with unsaved local edits → ask the user
            if !self.conflict {
                self.conflict = true;
                cx.notify();
            }
            return;
        }
        self.reload_from(content, Some(disk), &path, cx);
    }

    /// Conflict resolution: discard local edits and take the on-disk version.
    pub fn force_reload(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else { return };
        let Ok(content) = std::fs::read_to_string(&path) else { return };
        let disk = file_mtime(&path);
        self.reload_from(content, disk, &path, cx);
    }

    /// Conflict resolution: keep local edits; stop flagging (a later save will
    /// overwrite the on-disk version).
    pub fn keep_local(&mut self, cx: &mut Context<Self>) {
        self.disk_mtime = self.path.as_deref().and_then(file_mtime);
        self.conflict = false;
        cx.notify();
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn set_read_only(&mut self, ro: bool) {
        self.read_only = ro;
        if !ro {
            self.ro_hint = false;
        }
    }

    /// Flash a "read-only" hint at the cursor (when a blocked edit is attempted).
    fn show_ro_hint(&mut self, cx: &mut Context<Self>) {
        self.ro_hint = true;
        self.ro_hint_gen = self.ro_hint_gen.wrapping_add(1);
        let gen = self.ro_hint_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(1600)).await;
            this.update(cx, |this, cx| {
                if this.ro_hint_gen == gen {
                    this.ro_hint = false;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// FNV-1a hash of the buffer — cheap change-detection for idle tracking.
    pub fn content_hash(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in self.content.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    /// 1-based line of the cursor (for "copy reference").
    pub fn cursor_line(&self) -> usize {
        self.offset_to_pos(self.cursor_offset()).0 + 1
    }

    fn rehighlight(&mut self) {
        if let Some(path) = &self.path {
            self.styles = self.hl.highlight(&self.content, path);
        }
    }

    /// Re-highlight after a short idle, coalescing bursts of typing into one
    /// pass (full-file syntect highlighting is too heavy to run per keystroke).
    fn schedule_rehighlight(&mut self, cx: &mut Context<Self>) {
        self.hl_gen += 1;
        let gen = self.hl_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(60)).await;
            this.update(cx, |this, cx| {
                if this.hl_gen == gen {
                    this.rehighlight();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ── line/offset helpers ────────────────────────────────────────────────

    /// Byte offset of the start of each line (len = line count).
    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0usize];
        for (i, b) in self.content.bytes().enumerate() {
            if b == b'\n' {
                starts.push(i + 1);
            }
        }
        starts
    }

    fn line_count(&self) -> usize {
        self.content.bytes().filter(|b| *b == b'\n').count() + 1
    }

    /// (row, byte offset within line) for a global byte offset.
    fn offset_to_pos(&self, offset: usize) -> (usize, usize) {
        let starts = self.line_starts();
        let mut row = 0;
        for (i, &s) in starts.iter().enumerate() {
            if s > offset {
                break;
            }
            row = i;
        }
        (row, offset - starts[row])
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let o = offset.min(self.content.len());
        self.selected_range = o..o;
        self.typing_run = false;
        self.comp_open = false;
        self.restart_blink(cx);
        cx.notify();
    }

    /// Jump to a 1-based line and column, centering it in the viewport.
    pub fn goto(&mut self, line1: usize, col1: usize, cx: &mut Context<Self>) {
        let starts = self.line_starts();
        if starts.is_empty() {
            return;
        }
        let row = line1.saturating_sub(1).min(starts.len() - 1);
        let line_start = starts[row];
        let line_end = if row + 1 < starts.len() {
            starts[row + 1] - 1
        } else {
            self.content.len()
        };
        let off = (line_start + col1.saturating_sub(1)).min(line_end);
        self.move_to(off, cx);

        // center the target row in the viewport
        let vh = self.last_bounds.map(|b| f32::from(b.size.height)).unwrap_or(600.);
        let target = (row as f32 * LINE_HEIGHT) - vh / 2.0;
        self.scroll_y = px(target.max(0.));
        cx.notify();
    }

    /// Make the cursor solid now, then resume blinking on a fresh ~530ms cycle.
    /// Bumping the epoch retires any previously-running blink loop.
    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_on = true;
        self.blink_epoch += 1;
        let epoch = self.blink_epoch;
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(530)).await;
            let keep_going = this
                .update(cx, |this, cx| {
                    if this.blink_epoch != epoch {
                        return false;
                    }
                    this.blink_on = !this.blink_on;
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        })
        .detach();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.content.len());
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.restart_blink(cx);
        cx.notify();
    }

    /// Byte offset one line up/down from the cursor, keeping the column.
    fn vertical_target(&self, dir: i32) -> Option<usize> {
        let (row, col) = self.offset_to_pos(self.cursor_offset());
        let starts = self.line_starts();
        let target = row as i32 + dir;
        if target < 0 || target as usize >= starts.len() {
            return None;
        }
        let target = target as usize;
        let line_start = starts[target];
        let line_end = if target + 1 < starts.len() {
            starts[target + 1] - 1
        } else {
            self.content.len()
        };
        Some(line_start + col.min(line_end - line_start))
    }

    fn prev_boundary(&self, offset: usize) -> usize {
        self.content[..offset]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content[offset..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| offset + i)
            .unwrap_or(self.content.len())
    }

    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    /// Word boundary to the left: skip whitespace, then a run of same-class chars.
    fn prev_word(&self, offset: usize) -> usize {
        let mut i = offset;
        let prev = |i: usize| self.content[..i].char_indices().next_back();
        while let Some((st, c)) = prev(i) {
            if c.is_whitespace() { i = st } else { break }
        }
        if let Some((_, c0)) = prev(i) {
            let w = Self::is_word(c0);
            while let Some((st, c)) = prev(i) {
                if !c.is_whitespace() && Self::is_word(c) == w { i = st } else { break }
            }
        }
        i
    }

    /// Word boundary to the right.
    fn next_word(&self, offset: usize) -> usize {
        let mut i = offset;
        let cur = |i: usize| self.content[i..].chars().next().map(|c| (c, i + c.len_utf8()));
        while let Some((c, nx)) = cur(i) {
            if c.is_whitespace() { i = nx } else { break }
        }
        if let Some((c0, _)) = cur(i) {
            let w = Self::is_word(c0);
            while let Some((c, nx)) = cur(i) {
                if !c.is_whitespace() && Self::is_word(c) == w { i = nx } else { break }
            }
        }
        i
    }

    // ── action handlers (bound to keys in main) ─────────────────────────────

    fn act_backspace(&mut self, _: &Backspace, w: &mut Window, cx: &mut Context<Self>) { self.on_key("backspace", w, cx); }
    fn act_delete(&mut self, _: &Delete, w: &mut Window, cx: &mut Context<Self>) { self.on_key("delete", w, cx); }
    fn act_left(&mut self, _: &MoveLeft, w: &mut Window, cx: &mut Context<Self>) { self.on_key("left", w, cx); }
    fn act_right(&mut self, _: &MoveRight, w: &mut Window, cx: &mut Context<Self>) { self.on_key("right", w, cx); }
    fn act_up(&mut self, _: &MoveUp, w: &mut Window, cx: &mut Context<Self>) { self.on_key("up", w, cx); }
    fn act_down(&mut self, _: &MoveDown, w: &mut Window, cx: &mut Context<Self>) { self.on_key("down", w, cx); }
    fn act_home(&mut self, _: &Home, w: &mut Window, cx: &mut Context<Self>) { self.on_key("home", w, cx); }
    fn act_end(&mut self, _: &End, w: &mut Window, cx: &mut Context<Self>) { self.on_key("end", w, cx); }
    fn act_newline(&mut self, _: &Newline, w: &mut Window, cx: &mut Context<Self>) { self.on_key("enter", w, cx); }
    fn act_indent(&mut self, _: &Indent, w: &mut Window, cx: &mut Context<Self>) { self.on_key("tab", w, cx); }
    fn act_save(&mut self, _: &Save, _w: &mut Window, cx: &mut Context<Self>) { self.save(cx); }
    fn act_sel_left(&mut self, _: &SelectLeft, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-left", w, cx); }
    fn act_sel_right(&mut self, _: &SelectRight, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-right", w, cx); }
    fn act_sel_up(&mut self, _: &SelectUp, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-up", w, cx); }
    fn act_sel_down(&mut self, _: &SelectDown, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-down", w, cx); }
    fn act_sel_home(&mut self, _: &SelectHome, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-home", w, cx); }
    fn act_sel_end(&mut self, _: &SelectEnd, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-end", w, cx); }
    fn act_select_all(&mut self, _: &SelectAll, _w: &mut Window, cx: &mut Context<Self>) { self.select_all(cx); }
    fn act_copy(&mut self, _: &Copy, _w: &mut Window, cx: &mut Context<Self>) { self.copy(cx); }
    fn act_paste(&mut self, _: &Paste, _w: &mut Window, cx: &mut Context<Self>) { self.paste(cx); }
    fn act_cut(&mut self, _: &Cut, _w: &mut Window, cx: &mut Context<Self>) { self.cut(cx); }
    fn act_word_left(&mut self, _: &WordLeft, w: &mut Window, cx: &mut Context<Self>) { self.on_key("word-left", w, cx); }
    fn act_word_right(&mut self, _: &WordRight, w: &mut Window, cx: &mut Context<Self>) { self.on_key("word-right", w, cx); }
    fn act_sel_word_left(&mut self, _: &SelectWordLeft, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-word-left", w, cx); }
    fn act_sel_word_right(&mut self, _: &SelectWordRight, w: &mut Window, cx: &mut Context<Self>) { self.on_key("select-word-right", w, cx); }
    fn act_undo(&mut self, _: &Undo, _w: &mut Window, cx: &mut Context<Self>) { self.undo(cx); }
    fn act_redo(&mut self, _: &Redo, _w: &mut Window, cx: &mut Context<Self>) { self.redo(cx); }
    fn act_delete_line(&mut self, _: &DeleteLine, _w: &mut Window, cx: &mut Context<Self>) { self.delete_line(cx); }
    fn act_move_line_up(&mut self, _: &MoveLineUp, _w: &mut Window, cx: &mut Context<Self>) { self.move_line(-1, cx); }
    fn act_move_line_down(&mut self, _: &MoveLineDown, _w: &mut Window, cx: &mut Context<Self>) { self.move_line(1, cx); }
    fn act_comp_trigger(&mut self, _: &CompTrigger, _w: &mut Window, cx: &mut Context<Self>) { self.request_completion(cx); }
    fn act_goto_def(&mut self, _: &GotoDef, _w: &mut Window, cx: &mut Context<Self>) {
        let (row, col) = self.offset_to_pos(self.cursor_offset());
        self.go_to_definition(row, col, cx);
    }
    fn act_comp_dismiss(&mut self, _: &CompDismiss, w: &mut Window, cx: &mut Context<Self>) { self.on_key("escape", w, cx); }

    /// Replace the whole buffer (one undo step) and place the cursor.
    fn set_content(&mut self, new_content: String, cursor: usize, cx: &mut Context<Self>) {
        if self.read_only {
            self.show_ro_hint(cx);
            return;
        }
        self.undo_stack.push(Snapshot {
            content: self.content.clone(),
            cursor: self.cursor_offset(),
        });
        if self.undo_stack.len() > 1000 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
        self.typing_run = false;
        self.content = new_content;
        let c = cursor.min(self.content.len());
        self.selected_range = c..c;
        self.selection_reversed = false;
        self.dirty = true;
        self.rehighlight();
        self.restart_blink(cx);
        cx.notify();
    }

    fn delete_line(&mut self, cx: &mut Context<Self>) {
        let mut v: Vec<String> = self.content.split('\n').map(|s| s.to_string()).collect();
        let (row, _) = self.offset_to_pos(self.cursor_offset());
        if v.len() <= 1 {
            self.set_content(String::new(), 0, cx);
            return;
        }
        v.remove(row);
        let new_content = v.join("\n");
        let nr = row.min(v.len() - 1);
        let cur: usize = v[..nr].iter().map(|s| s.len() + 1).sum();
        self.set_content(new_content, cur, cx);
    }

    fn move_line(&mut self, dir: i32, cx: &mut Context<Self>) {
        let mut v: Vec<String> = self.content.split('\n').map(|s| s.to_string()).collect();
        let (row, col) = self.offset_to_pos(self.cursor_offset());
        let target = row as i32 + dir;
        if target < 0 || target as usize >= v.len() {
            return;
        }
        let target = target as usize;
        v.swap(row, target);
        let new_content = v.join("\n");
        let line_start: usize = v[..target].iter().map(|s| s.len() + 1).sum();
        let cur = line_start + col.min(v[target].len());
        self.set_content(new_content, cur, cx);
    }

    /// Byte range of the cursor's line, including its trailing newline (so a
    /// no-selection copy/paste round-trips as a whole line).
    fn current_line_range(&self) -> Range<usize> {
        let starts = self.line_starts();
        let (row, _) = self.offset_to_pos(self.cursor_offset());
        let start = starts[row];
        let end = starts.get(row + 1).copied().unwrap_or(self.content.len());
        start..end
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        // with no selection, copy the whole current line (VS Code / JetBrains style)
        let range = if self.selected_range.is_empty() {
            self.current_line_range()
        } else {
            self.selected_range.clone()
        };
        let text = self.content[range].to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
            self.replace(None, &text, cx);
        }
    }

    fn cut(&mut self, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            self.copy(cx);
            self.replace(None, "", cx);
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        self.restart_blink(cx);
        cx.notify();
    }

    // ── editing core ─────────────────────────────────────────────────────────

    fn on_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.hover = None;
        self.link = None;
        // completion popup intercepts navigation/accept/dismiss keys
        if self.comp_open {
            match key {
                "up" => {
                    self.comp_sel = self.comp_sel.saturating_sub(1);
                    cx.notify();
                    return;
                }
                "down" => {
                    self.comp_sel = (self.comp_sel + 1).min(self.comp.len().saturating_sub(1));
                    cx.notify();
                    return;
                }
                "enter" | "tab" => {
                    self.accept_completion(cx);
                    return;
                }
                "escape" => {
                    self.comp_open = false;
                    cx.notify();
                    return;
                }
                _ => {}
            }
        }
        match key {
            "left" => {
                let o = if self.selected_range.is_empty() {
                    self.prev_boundary(self.cursor_offset())
                } else {
                    self.selected_range.start
                };
                self.move_to(o, cx);
            }
            "right" => {
                let o = if self.selected_range.is_empty() {
                    self.next_boundary(self.cursor_offset())
                } else {
                    self.selected_range.end
                };
                self.move_to(o, cx);
            }
            "up" => self.move_vertical(-1, cx),
            "down" => self.move_vertical(1, cx),
            "home" => {
                let (row, _) = self.offset_to_pos(self.cursor_offset());
                let start = self.line_starts()[row];
                self.move_to(start, cx);
            }
            "end" => {
                let (row, _) = self.offset_to_pos(self.cursor_offset());
                let starts = self.line_starts();
                let end = if row + 1 < starts.len() {
                    starts[row + 1] - 1
                } else {
                    self.content.len()
                };
                self.move_to(end, cx);
            }
            "select-left" => {
                let p = self.prev_boundary(self.cursor_offset());
                self.select_to(p, cx);
            }
            "select-right" => {
                let n = self.next_boundary(self.cursor_offset());
                self.select_to(n, cx);
            }
            "select-up" => {
                if let Some(t) = self.vertical_target(-1) {
                    self.select_to(t, cx);
                }
            }
            "select-down" => {
                if let Some(t) = self.vertical_target(1) {
                    self.select_to(t, cx);
                }
            }
            "select-home" => {
                let (row, _) = self.offset_to_pos(self.cursor_offset());
                let start = self.line_starts()[row];
                self.select_to(start, cx);
            }
            "select-end" => {
                let (row, _) = self.offset_to_pos(self.cursor_offset());
                let starts = self.line_starts();
                let end = if row + 1 < starts.len() {
                    starts[row + 1] - 1
                } else {
                    self.content.len()
                };
                self.select_to(end, cx);
            }
            "word-left" => {
                let p = self.prev_word(self.cursor_offset());
                self.move_to(p, cx);
            }
            "word-right" => {
                let n = self.next_word(self.cursor_offset());
                self.move_to(n, cx);
            }
            "select-word-left" => {
                let p = self.prev_word(self.cursor_offset());
                self.select_to(p, cx);
            }
            "select-word-right" => {
                let n = self.next_word(self.cursor_offset());
                self.select_to(n, cx);
            }
            "backspace" => {
                if self.selected_range.is_empty() {
                    let prev = self.prev_boundary(self.cursor_offset());
                    if prev == self.cursor_offset() {
                        return;
                    }
                    self.selected_range = prev..self.cursor_offset();
                }
                self.replace(None, "", cx);
            }
            "delete" => {
                if self.selected_range.is_empty() {
                    let next = self.next_boundary(self.cursor_offset());
                    if next == self.cursor_offset() {
                        return;
                    }
                    self.selected_range = self.cursor_offset()..next;
                }
                self.replace(None, "", cx);
            }
            "enter" => {
                // auto-indent: carry the current line's leading whitespace to the new line
                let (row, _) = self.offset_to_pos(self.cursor_offset());
                let line_start = self.line_starts()[row];
                let indent: String = self.content[line_start..]
                    .chars()
                    .take_while(|c| *c == ' ' || *c == '\t')
                    .collect();
                self.replace(None, &format!("\n{}", indent), cx);
            }
            "tab" => self.replace(None, "  ", cx),
            _ => {}
        }
        self.scroll_cursor_into_view(window);
    }

    fn move_vertical(&mut self, dir: i32, cx: &mut Context<Self>) {
        if let Some(t) = self.vertical_target(dir) {
            self.move_to(t, cx);
        }
    }

    fn replace(&mut self, range: Option<Range<usize>>, text: &str, cx: &mut Context<Self>) {
        if self.read_only {
            self.show_ro_hint(cx);
            return;
        }
        let range = range.unwrap_or_else(|| self.selected_range.clone());

        // Coalesce a run of plain single-char typing into one undo step:
        // push a snapshot only when starting a new run (or for any non-typing edit).
        let is_typing = text.len() == 1 && text != "\n" && range.is_empty();
        if !is_typing || !self.typing_run {
            self.undo_stack.push(Snapshot {
                content: self.content.clone(),
                cursor: self.cursor_offset(),
            });
            if self.undo_stack.len() > 1000 {
                self.undo_stack.remove(0);
            }
        }
        self.typing_run = is_typing;
        self.redo_stack.clear();

        let new_content = format!(
            "{}{}{}",
            &self.content[..range.start],
            text,
            &self.content[range.end..]
        );
        self.content = new_content;
        let new_cursor = range.start + text.len();
        self.selected_range = new_cursor..new_cursor;
        self.selection_reversed = false;
        self.dirty = true;
        self.schedule_rehighlight(cx); // debounced — keep typing snappy
        self.restart_blink(cx);

        // sync to the language server + completion, both debounced (see below).
        // Only show completions while typing identifier characters / after '.'.
        let want_completion =
            is_typing && matches!(text.chars().next(), Some(c) if c.is_alphanumeric() || c == '_' || c == '.');
        if is_typing && !want_completion {
            self.comp_open = false; // close instantly on a non-identifier char
        }
        self.lsp_dirty = true;
        self.schedule_lsp(want_completion, cx);
        cx.notify();
    }

    /// Send a full-document didChange to the server if the buffer changed since
    /// the last one. Idempotent, so it's safe to call as a "flush".
    fn flush_lsp(&mut self) {
        if !self.lsp_dirty {
            return;
        }
        self.lsp_dirty = false;
        if let Some(lsp) = &self.lsp {
            if !self.uri.is_empty() {
                self.version += 1;
                lsp.did_change(&self.uri, self.version, &self.content);
            }
        }
    }

    /// Debounce the LSP sync (and optional completion) so a burst of keystrokes
    /// sends at most one didChange — serializing the whole document on every
    /// key was the editor's per-keystroke cost.
    fn schedule_lsp(&mut self, want_completion: bool, cx: &mut Context<Self>) {
        if self.lsp.is_none() || self.uri.is_empty() {
            return;
        }
        self.sync_gen += 1;
        let gen = self.sync_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(120)).await;
            this.update(cx, |this, cx| {
                if this.sync_gen != gen {
                    return; // superseded by a newer keystroke
                }
                this.flush_lsp();
                if want_completion {
                    this.request_completion(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask the server for completions at the cursor.
    fn request_completion(&mut self, cx: &mut Context<Self>) {
        // make sure the server has the latest buffer before we ask
        self.flush_lsp();
        let Some(lsp) = self.lsp.clone() else { return };
        if self.uri.is_empty() {
            return;
        }
        let (row, col) = self.offset_to_pos(self.cursor_offset());
        let rx = lsp.completion(&self.uri, row, col);
        cx.spawn(async move |this, cx| {
            if let Ok(val) = rx.await {
                let items = parse_completions(&val);
                this.update(cx, |this, cx| {
                    if items.is_empty() {
                        this.comp_open = false;
                    } else {
                        this.comp = items;
                        this.comp_sel = 0;
                        this.comp_open = true;
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Go to the definition of the symbol at (row, col); emits OpenLocation.
    fn go_to_definition(&mut self, row: usize, col: usize, cx: &mut Context<Self>) {
        let Some(lsp) = self.lsp.clone() else { return };
        if self.uri.is_empty() {
            return;
        }
        let rx = lsp.definition(&self.uri, row, col);
        cx.spawn(async move |this, cx| {
            if let Ok(val) = rx.await {
                if let Some((uri, line0, ch0)) = crate::lsp::parse_location(&val) {
                    let path = std::path::PathBuf::from(uri.trim_start_matches("file://"));
                    this.update(cx, |_this, cx| {
                        cx.emit(OpenLocation { path, line: line0 + 1, col: ch0 + 1 });
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// If `offset` sits inside a quoted relative import path, resolve it (with
    /// the ESM `.js`→`.ts` rewrite) and open it in a tab. Returns true on a hit.
    fn open_path_at(&mut self, offset: usize, cx: &mut Context<Self>) -> bool {
        let Some(spec) = self.string_literal_at(offset) else { return false };
        // only relative specifiers; bare package imports need node resolution
        if !(spec.starts_with("./") || spec.starts_with("../")) {
            return false;
        }
        let Some(dir) = self.path.as_deref().and_then(Path::parent) else { return false };
        let Some(target) = resolve_import(dir, &spec) else { return false };
        cx.emit(OpenLocation { path: target, line: 1, col: 1 });
        true
    }

    /// The contents of the quote-delimited string spanning `offset` on its line,
    /// or None if the offset isn't inside one.
    fn string_literal_at(&self, offset: usize) -> Option<String> {
        let starts = self.line_starts();
        let row = match starts.binary_search(&offset) {
            Ok(r) => r,
            Err(r) => r.saturating_sub(1),
        };
        let line_start = *starts.get(row)?;
        let line_end = if row + 1 < starts.len() { starts[row + 1] - 1 } else { self.content.len() };
        let line = &self.content[line_start..line_end];
        let local = offset - line_start;
        let mut i = 0;
        while i < line.len() {
            let c = line.as_bytes()[i];
            if c == b'"' || c == b'\'' || c == b'`' {
                if let Some(rel_end) = line[i + 1..].find(c as char) {
                    let (cs, ce) = (i + 1, i + 1 + rel_end);
                    if local >= cs && local <= ce {
                        return Some(line[cs..ce].to_string());
                    }
                    i = ce + 1;
                    continue;
                }
            }
            i += 1;
        }
        None
    }

    // ── in-file search ──────────────────────────────────────────────────────

    fn act_search(&mut self, _: &SearchOpen, window: &mut Window, cx: &mut Context<Self>) {
        self.search_open = true;
        // prefill with the current single-line selection
        if !self.selected_range.is_empty() {
            let s = &self.content[self.selected_range.clone()];
            if !s.contains('\n') {
                self.search_query.set(s.to_string());
            }
        }
        self.recompute_search();
        if !self.search_matches.is_empty() {
            self.search_navigate(0, cx);
        }
        window.focus(&self.search_focus, cx);
        cx.notify();
    }

    fn recompute_search(&mut self) {
        self.search_matches.clear();
        self.search_idx = 0;
        if self.search_query.is_empty() {
            return;
        }
        let q = self.search_query.text.to_lowercase();
        let hay = self.content.to_lowercase();
        let mut start = 0;
        while let Some(pos) = hay[start..].find(&q) {
            let abs = start + pos;
            self.search_matches.push(abs..abs + q.len());
            start = abs + q.len().max(1);
        }
    }

    fn search_navigate(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.search_matches.is_empty() {
            return;
        }
        let n = self.search_matches.len() as i32;
        self.search_idx = (((self.search_idx as i32 + delta) % n + n) % n) as usize;
        let m = self.search_matches[self.search_idx].clone();
        self.selected_range = m.clone();
        self.selection_reversed = false;
        let (row, _) = self.offset_to_pos(m.start);
        let vh = self.last_bounds.map(|b| f32::from(b.size.height)).unwrap_or(600.);
        self.scroll_y = px(((row as f32) * LINE_HEIGHT - vh / 2.0).max(0.));
        cx.notify();
    }

    fn search_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        match ks.key.as_str() {
            "escape" => {
                self.search_open = false;
                self.search_matches.clear();
                window.focus(&self.focus_handle, cx);
            }
            "enter" => {
                if ks.modifiers.shift {
                    self.search_navigate(-1, cx);
                } else {
                    self.search_navigate(1, cx);
                }
            }
            "down" => self.search_navigate(1, cx),
            "up" => self.search_navigate(-1, cx),
            _ => {
                // full text editing via the shared Field (caret, word/line motion,
                // selection, cmd+a, cmd+c/x/v, undo/redo)
                let clip = cx.read_from_clipboard().and_then(|i| i.text());
                let edit = self.search_query.key(ks, clip, |_| true);
                if let Some(text) = self.search_query.take_copy() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                if edit == Edit::Changed {
                    self.recompute_search();
                    if !self.search_matches.is_empty() {
                        self.search_navigate(0, cx);
                    }
                }
            }
        }
        cx.notify();
    }

    /// Accept the selected completion, replacing the identifier prefix.
    fn accept_completion(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.comp.get(self.comp_sel).cloned() else { return };
        let cursor = self.cursor_offset();
        // walk back over identifier chars to find the prefix start
        let mut start = cursor;
        while let Some((i, c)) = self.content[..start].char_indices().next_back() {
            if c.is_alphanumeric() || c == '_' {
                start = i;
            } else {
                break;
            }
        }
        self.comp_open = false;
        self.selected_range = start..cursor;
        self.replace(None, &item.insert, cx);
    }

    fn undo(&mut self, cx: &mut Context<Self>) {
        if let Some(snap) = self.undo_stack.pop() {
            self.redo_stack.push(Snapshot {
                content: self.content.clone(),
                cursor: self.cursor_offset(),
            });
            self.content = snap.content;
            self.selected_range = snap.cursor..snap.cursor;
            self.selection_reversed = false;
            self.dirty = true;
            self.typing_run = false;
            self.rehighlight();
            self.restart_blink(cx);
            self.lsp_dirty = true;
            self.schedule_lsp(false, cx);
            cx.notify();
        }
    }

    fn redo(&mut self, cx: &mut Context<Self>) {
        if let Some(snap) = self.redo_stack.pop() {
            self.undo_stack.push(Snapshot {
                content: self.content.clone(),
                cursor: self.cursor_offset(),
            });
            self.content = snap.content;
            self.selected_range = snap.cursor..snap.cursor;
            self.selection_reversed = false;
            self.dirty = true;
            self.typing_run = false;
            self.rehighlight();
            self.restart_blink(cx);
            self.lsp_dirty = true;
            self.schedule_lsp(false, cx);
            cx.notify();
        }
    }

    fn scroll_cursor_into_view(&mut self, _window: &mut Window) {
        let (row, _) = self.offset_to_pos(self.cursor_offset());
        let line_h = px(LINE_HEIGHT);
        let cursor_top = line_h * (row as f32);
        if cursor_top < self.scroll_y {
            self.scroll_y = cursor_top;
        }
        // bottom clamp handled loosely; refined once we know viewport height
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ── EntityInputHandler: routes typed text (and IME) into the buffer ────────

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = range_utf16.start.min(self.content.len())..range_utf16.end.min(self.content.len());
        *actual = Some(range.clone());
        Some(self.content.get(range)?.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.selected_range.clone(),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range, new_text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace(range, new_text, cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

// ── the custom multi-line element ──────────────────────────────────────────

pub struct EditorElement {
    pub editor: Entity<Editor>,
}

pub struct EditorPrepaint {
    current_line: Option<PaintQuad>,
    gutter: Vec<(Pixels, ShapedLine)>,
    code: Vec<(Pixels, ShapedLine)>,
    cursor: Option<PaintQuad>,
    selections: Vec<PaintQuad>,
    link: Option<PaintQuad>,
    search: Vec<PaintQuad>,
}

impl IntoElement for EditorElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = EditorPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> EditorPrepaint {
        let editor = self.editor.read(cx);
        let content = editor.content.clone();
        let styles = editor.styles.clone();
        let scroll_y = editor.scroll_y;
        let scroll_x = editor.scroll_x;
        let selected = editor.selected_range.clone();
        let cursor_offset = editor.cursor_offset();
        let blink_on = editor.blink_on;
        let link_range = editor.link.clone();
        let search_matches = editor.search_matches.clone();
        let search_idx = editor.search_idx;

        let line_height = px(LINE_HEIGHT);
        let font_size = px(FONT_SIZE);
        let text_sys = window.text_system();

        // monospace advance width
        let probe = text_sys.shape_line(
            "0".into(),
            font_size,
            &[TextRun {
                len: 1,
                font: font(FONT),
                color: rgb(TEXT).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let char_width = probe.width;

        let lines: Vec<&str> = content.split('\n').collect();
        let total = lines.len();
        let digits = total.to_string().len().max(2);
        let gutter_width = char_width * (digits as f32) + px(GUTTER_PAD);
        let code_x = bounds.left() + gutter_width;

        // visible line range
        let first = (f32::from(scroll_y) / LINE_HEIGHT).floor().max(0.) as usize;
        let visible = (f32::from(bounds.size.height) / LINE_HEIGHT).ceil() as usize + 2;
        let last = (first + visible).min(total);

        let mut line_starts = vec![0usize];
        for (i, b) in content.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }

        // current-line highlight (full width, behind everything)
        let cursor_row = {
            let mut r = 0;
            for (i, &s) in line_starts.iter().enumerate() {
                if s > cursor_offset {
                    break;
                }
                r = i;
            }
            r
        };
        let current_line = if cursor_row >= first && cursor_row < last {
            let y = bounds.top() + line_height * (cursor_row as f32) - scroll_y;
            Some(fill(
                Bounds::new(point(bounds.left(), y), size(bounds.size.width, line_height)),
                rgb(CURRENT_LINE),
            ))
        } else {
            None
        };

        // resolve the link token to (row, start_col, end_col) on its line
        let link_pos = link_range.as_ref().map(|r| {
            let mut row = 0;
            for (i, &s) in line_starts.iter().enumerate() {
                if s > r.start {
                    break;
                }
                row = i;
            }
            let rs = line_starts[row];
            (row, r.start - rs, r.end - rs)
        });

        let mut gutter = Vec::new();
        let mut code = Vec::new();
        let mut selections = Vec::new();
        let mut cursor = None;
        let mut link = None;
        let mut search = Vec::new();

        for row in first..last {
            let y = bounds.top() + line_height * (row as f32) - scroll_y;
            let line = lines[row];

            // gutter number (right-aligned)
            let num = format!("{}", row + 1);
            let num_run = TextRun {
                len: num.len(),
                font: font(FONT),
                color: rgb(LINE_NUMBER).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let num_line = text_sys.shape_line(num.into(), font_size, &[num_run], None);
            gutter.push((y, num_line));

            // code line with syntect runs
            let runs = build_runs(line, styles.get(row));
            let shaped = text_sys.shape_line(SharedString::from(line.to_string()), font_size, &runs, None);

            // selection highlight on this row
            let row_start = line_starts[row];
            let row_end = row_start + line.len();
            if !selected.is_empty() && selected.start < row_end + 1 && selected.end > row_start {
                let sel_start = selected.start.saturating_sub(row_start).min(line.len());
                let sel_end = selected.end.saturating_sub(row_start).min(line.len());
                let x0 = code_x + shaped.x_for_index(sel_start) - scroll_x;
                let x1 = code_x + shaped.x_for_index(sel_end) - scroll_x;
                // extend full line when selection spans the newline
                let x1 = if selected.end > row_end { x1 + char_width } else { x1 };
                selections.push(fill(
                    Bounds::from_corners(point(x0, y), point(x1, y + line_height)),
                    rgb(SELECTION),
                ));
            }

            // cursor on this row (only when in the "on" phase of the blink)
            if blink_on && cursor_offset >= row_start && cursor_offset <= row_end {
                let col = cursor_offset - row_start;
                let cx_pos = code_x + shaped.x_for_index(col) - scroll_x;
                cursor = Some(fill(
                    Bounds::new(point(cx_pos, y), size(px(2.), line_height)),
                    rgb(CURSOR),
                ));
            }

            // search match highlights on this row
            for (mi, m) in search_matches.iter().enumerate() {
                if m.start < row_end + 1 && m.end > row_start {
                    let s0 = m.start.saturating_sub(row_start).min(line.len());
                    let s1 = m.end.saturating_sub(row_start).min(line.len());
                    let x0 = code_x + shaped.x_for_index(s0) - scroll_x;
                    let x1 = code_x + shaped.x_for_index(s1) - scroll_x;
                    let bg = if mi == search_idx { SEARCH_CURRENT_BG } else { SEARCH_MATCH_BG };
                    search.push(fill(
                        Bounds::from_corners(point(x0, y), point(x1, y + line_height)),
                        rgb(bg),
                    ));
                }
            }

            // cmd-hover link underline on this row
            if let Some((lrow, c0, c1)) = link_pos {
                if row == lrow && c1 <= line.len() {
                    let x0 = code_x + shaped.x_for_index(c0) - scroll_x;
                    let x1 = code_x + shaped.x_for_index(c1) - scroll_x;
                    let uy = y + line_height - px(2.);
                    link = Some(fill(
                        Bounds::from_corners(point(x0, uy), point(x1, uy + px(1.5))),
                        rgb(ACCENT),
                    ));
                }
            }

            code.push((y, shaped));
        }

        // stash layout info for mouse mapping
        self.editor.update(cx, |e, _| {
            e.last_bounds = Some(bounds);
            e.last_char_width = char_width;
            e.last_gutter_width = gutter_width;
        });

        EditorPrepaint {
            current_line,
            gutter,
            code,
            cursor,
            selections,
            link,
            search,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        prepaint: &mut EditorPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        // current-line highlight (behind everything, full width)
        if let Some(cl) = prepaint.current_line.take() {
            window.paint_quad(cl);
        }

        let editor = self.editor.read(cx);
        let gutter_width = editor.last_gutter_width;
        let scroll_x = editor.scroll_x;
        let code_x = bounds.left() + gutter_width;

        // Everything in the code region is clipped so horizontally-scrolled
        // content can't bleed over the gutter.
        let code_mask = gpui::ContentMask {
            bounds: Bounds::from_corners(
                point(code_x, bounds.top()),
                point(bounds.right(), bounds.bottom()),
            ),
        };
        let focused = focus_handle.is_focused(window);
        window.with_content_mask(Some(code_mask), |window| {
            for s in prepaint.search.drain(..) {
                window.paint_quad(s);
            }
            for sel in prepaint.selections.drain(..) {
                window.paint_quad(sel);
            }
            for (y, line) in prepaint.code.drain(..) {
                line.paint(point(code_x - scroll_x, y), px(LINE_HEIGHT), gpui::TextAlign::Left, None, window, cx).ok();
            }
            if let Some(link) = prepaint.link.take() {
                window.paint_quad(link);
            }
            if focused {
                if let Some(cursor) = prepaint.cursor.take() {
                    window.paint_quad(cursor);
                }
            }
        });

        // gutter numbers (fixed column, clipped to the editor's own bounds so a
        // partially-scrolled top line can't bleed up over the tab bar)
        let gutter_left = bounds.left() + px(8.);
        let gutter_mask = gpui::ContentMask {
            bounds: Bounds::from_corners(
                point(bounds.left(), bounds.top()),
                point(code_x, bounds.bottom()),
            ),
        };
        window.with_content_mask(Some(gutter_mask), |window| {
            for (y, line) in prepaint.gutter.drain(..) {
                line.paint(point(gutter_left, y), px(LINE_HEIGHT), gpui::TextAlign::Left, None, window, cx).ok();
            }
        });

        // Window-level drag handlers: element `on_mouse_move` is hover-phase only
        // and stops once a button is held, so a press + drag never extends the
        // selection. Registering on the window (re-registered each paint) keeps
        // move/up events flowing for the duration of the drag.
        let ed = self.editor.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase: DispatchPhase, window: &mut Window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let changed = ed.update(cx, |e, cx| {
                if !e.selecting || ev.pressed_button != Some(MouseButton::Left) {
                    return false;
                }
                match e.offset_at(ev.position) {
                    Some(off) => {
                        e.select_to(off, cx);
                        true
                    }
                    None => false,
                }
            });
            // force the frame so the selection tracks the cursor live during drag
            if changed {
                window.refresh();
            }
        });
        let ed_up = self.editor.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase: DispatchPhase, _window, cx| {
            if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                return;
            }
            ed_up.update(cx, |e, _cx| e.selecting = false);
        });
    }
}

/// The file's last-modified time, or None if it can't be stat'd.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Resolve a relative ESM import specifier to an on-disk file. TypeScript ESM
/// writes `.js` in import paths that actually point at `.ts` sources, so we try
/// the TS siblings first, then the literal path, then extensionless + index
/// resolution. Returns the first candidate that exists.
fn resolve_import(dir: &Path, spec: &str) -> Option<PathBuf> {
    let base = dir.join(spec);
    let mut cands: Vec<PathBuf> = Vec::new();
    let mut push = |v: PathBuf| {
        if !cands.contains(&v) {
            cands.push(v);
        }
    };
    match base.extension().and_then(|e| e.to_str()) {
        Some("js") | Some("jsx") => {
            let stem = base.with_extension("");
            for e in ["ts", "tsx", "d.ts", "js", "jsx"] {
                push(stem.with_extension(e));
            }
        }
        Some("mjs") => {
            let stem = base.with_extension("");
            for e in ["mts", "ts", "mjs"] {
                push(stem.with_extension(e));
            }
        }
        Some("cjs") => {
            let stem = base.with_extension("");
            for e in ["cts", "ts", "cjs"] {
                push(stem.with_extension(e));
            }
        }
        // .ts/.tsx/.json/etc — already concrete
        Some(_) => push(base.clone()),
        // extensionless: try appending an ext, then a directory index
        None => {
            for e in ["ts", "tsx", "js", "jsx", "mts", "cts"] {
                push(base.with_extension(e));
            }
            for e in ["ts", "tsx", "js", "jsx"] {
                push(base.join(format!("index.{e}")));
            }
        }
    }
    cands.into_iter().find(|p| p.is_file())
}

fn build_runs(line: &str, styles: Option<&Vec<(usize, u32)>>) -> Vec<TextRun> {
    let total: usize = styles.map(|s| s.iter().map(|(l, _)| *l).sum()).unwrap_or(0);
    if let Some(styles) = styles {
        if total == line.len() && !styles.is_empty() {
            return styles
                .iter()
                .map(|(len, color)| TextRun {
                    len: *len,
                    font: font(FONT),
                    color: rgb(*color).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                })
                .collect();
        }
    }
    // fallback: single plain run
    vec![TextRun {
        len: line.len(),
        font: font(FONT),
        color: rgb(TEXT).into(),
        background_color: None,
        underline: None,
        strikethrough: None,
    }]
}

// ── mouse + scroll handling, rendered via Render ───────────────────────────

impl Editor {
    /// Hover: after the pointer rests ~350ms, ask the server for hover info.
    fn on_mouse_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // moving hides any shown hover
        if self.hover.is_some() {
            self.hover = None;
            cx.notify();
        }
        let Some(b) = self.last_bounds else { return };
        let Some(lsp) = self.lsp.clone() else { return };
        if self.uri.is_empty() {
            return;
        }
        let pos = ev.position;
        let cw = f32::from(self.last_char_width);
        let gw = f32::from(self.last_gutter_width);
        let relx = f32::from(pos.x - b.left()) - gw;
        if relx < 0.0 {
            return; // over the gutter
        }
        let row = ((f32::from(pos.y - b.top()) + f32::from(self.scroll_y)) / LINE_HEIGHT)
            .floor()
            .max(0.) as usize;
        let col = ((relx + f32::from(self.scroll_x)) / cw).floor().max(0.) as usize;

        // cmd held → show the hovered identifier as a clickable link
        {
            let starts = self.line_starts();
            let new_link = if ev.modifiers.platform && row < starts.len() {
                let line_start = starts[row];
                let line_end = if row + 1 < starts.len() { starts[row + 1] - 1 } else { self.content.len() };
                let offset = (line_start + col).min(line_end);
                let r = self.word_at(offset);
                if r.is_empty() { None } else { Some(r) }
            } else {
                None
            };
            if new_link != self.link {
                self.link = new_link;
                cx.notify();
            }
        }

        // popup coords are relative to the editor div (not the window)
        let px_x = gw + (col as f32) * cw - f32::from(self.scroll_x);
        let px_y = ((row + 1) as f32) * LINE_HEIGHT - f32::from(self.scroll_y);

        self.hover_gen += 1;
        let gen = self.hover_gen;
        let uri = self.uri.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(350)).await;
            // bail if the pointer moved again since
            if this.update(cx, |this, _| this.hover_gen != gen).unwrap_or(true) {
                return;
            }
            if let Ok(val) = lsp.hover(&uri, row, col).await {
                if let Some(text) = crate::lsp::parse_hover(&val) {
                    this.update(cx, |this, cx| {
                        if this.hover_gen == gen {
                            this.hover = Some((text, px_x, px_y));
                            cx.notify();
                        }
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// Map a window-space position to a byte offset into `content`, clamped to
    /// the clicked line. Used by click placement and drag-selection.
    fn offset_at(&self, pos: Point<Pixels>) -> Option<usize> {
        let bounds = self.last_bounds?;
        let rel_y = f32::from(pos.y - bounds.top()) + f32::from(self.scroll_y);
        let row = (rel_y / LINE_HEIGHT).floor().max(0.) as usize;
        let starts = self.line_starts();
        if row >= starts.len() {
            return Some(self.content.len());
        }
        let line_start = starts[row];
        let line_end = if row + 1 < starts.len() {
            starts[row + 1] - 1
        } else {
            self.content.len()
        };
        let rel_x = f32::from(pos.x - bounds.left() - self.last_gutter_width) + f32::from(self.scroll_x);
        let col = (rel_x / f32::from(self.last_char_width)).round().max(0.) as usize;
        Some((line_start + col).min(line_end))
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.hover = None;
        let Some(offset) = self.offset_at(event.position) else { return };

        // cmd+click → follow an import path, else go to definition at the symbol
        if event.modifiers.platform {
            if self.open_path_at(offset, cx) {
                return;
            }
            let (r, c) = self.offset_to_pos(offset);
            self.go_to_definition(r, c, cx);
            return;
        }

        if event.click_count >= 2 {
            let range = self.word_at(offset);
            self.selected_range = range;
            self.selection_reversed = false;
            self.typing_run = false;
            self.restart_blink(cx);
            cx.notify();
        } else {
            // single click places the caret and begins a drag-selection; the
            // window-level listener (in the element's paint) extends it
            self.move_to(offset, cx);
            self.selecting = true;
        }
    }

    /// Range of the word (or run of same-class chars) containing `offset`.
    fn word_at(&self, offset: usize) -> Range<usize> {
        let len = self.content.len();
        if len == 0 {
            return 0..0;
        }
        let off = offset.min(len);
        let class = |c: char| {
            if c.is_whitespace() {
                0
            } else if Self::is_word(c) {
                1
            } else {
                2
            }
        };
        let here = self.content[off..]
            .chars()
            .next()
            .or_else(|| self.content[..off].chars().next_back());
        let Some(hc) = here else { return off..off };
        let target = class(hc);
        if target == 0 {
            return off..off; // whitespace → no word
        }
        let mut start = off;
        while let Some((i, c)) = self.content[..start].char_indices().next_back() {
            if class(c) == target {
                start = i;
            } else {
                break;
            }
        }
        let mut end = off;
        while let Some(c) = self.content[end..].chars().next() {
            if class(c) == target {
                end += c.len_utf8();
            } else {
                break;
            }
        }
        start..end
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        self.hover = None;
        self.link = None;
        let delta = event.delta.pixel_delta(px(LINE_HEIGHT));

        // vertical
        let new_y = f32::from(self.scroll_y) - f32::from(delta.y);
        let max_y = (self.line_count() as f32 * LINE_HEIGHT - 100.).max(0.);
        self.scroll_y = px(new_y.clamp(0., max_y));

        // horizontal
        let longest = self.content.split('\n').map(|l| l.chars().count()).max().unwrap_or(0);
        let viewport_w = self
            .last_bounds
            .map(|b| f32::from(b.size.width) - f32::from(self.last_gutter_width))
            .unwrap_or(0.);
        let content_w = longest as f32 * f32::from(self.last_char_width);
        let max_x = (content_w - viewport_w + f32::from(self.last_char_width) * 2.).max(0.);
        let new_x = f32::from(self.scroll_x) - f32::from(delta.x);
        self.scroll_x = px(new_x.clamp(0., max_x));

        cx.notify();
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let base = div()
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(rgb(BG))
            .text_size(px(FONT_SIZE))
            .when(self.link.is_some(), |d| d.cursor(CursorStyle::PointingHand))
            .on_action(cx.listener(Self::act_backspace))
            .on_action(cx.listener(Self::act_delete))
            .on_action(cx.listener(Self::act_left))
            .on_action(cx.listener(Self::act_right))
            .on_action(cx.listener(Self::act_up))
            .on_action(cx.listener(Self::act_down))
            .on_action(cx.listener(Self::act_home))
            .on_action(cx.listener(Self::act_end))
            .on_action(cx.listener(Self::act_newline))
            .on_action(cx.listener(Self::act_indent))
            .on_action(cx.listener(Self::act_save))
            .on_action(cx.listener(Self::act_sel_left))
            .on_action(cx.listener(Self::act_sel_right))
            .on_action(cx.listener(Self::act_sel_up))
            .on_action(cx.listener(Self::act_sel_down))
            .on_action(cx.listener(Self::act_sel_home))
            .on_action(cx.listener(Self::act_sel_end))
            .on_action(cx.listener(Self::act_select_all))
            .on_action(cx.listener(Self::act_copy))
            .on_action(cx.listener(Self::act_paste))
            .on_action(cx.listener(Self::act_cut))
            .on_action(cx.listener(Self::act_word_left))
            .on_action(cx.listener(Self::act_word_right))
            .on_action(cx.listener(Self::act_sel_word_left))
            .on_action(cx.listener(Self::act_sel_word_right))
            .on_action(cx.listener(Self::act_comp_trigger))
            .on_action(cx.listener(Self::act_comp_dismiss))
            .on_action(cx.listener(Self::act_goto_def))
            .on_action(cx.listener(Self::act_search))
            .on_action(cx.listener(Self::act_undo))
            .on_action(cx.listener(Self::act_redo))
            .on_action(cx.listener(Self::act_delete_line))
            .on_action(cx.listener(Self::act_move_line_up))
            .on_action(cx.listener(Self::act_move_line_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(EditorElement { editor: cx.entity() });

        // overlays live OUTSIDE the "Editor" key context so a focused search
        // input gets its own keys (escape, arrows, cmd+arrows) instead of the
        // editor's bindings intercepting them.
        let mut el = div().size_full().relative().child(base);
        if self.conflict {
            el = el.child(self.render_conflict_banner(cx));
        }
        if self.search_open {
            el = el.child(self.render_search_bar(cx));
        }
        if self.comp_open && !self.comp.is_empty() {
            el = el.child(self.render_completion());
        }
        if let Some((text, x, y)) = &self.hover {
            el = el.child(self.render_hover(text, *x, *y));
        }
        if self.ro_hint {
            el = el.child(self.render_ro_hint());
        }
        el
    }
}

impl Editor {
    /// Banner shown when the file changed on disk while we had unsaved edits.
    /// Lets the user reload (discard local) or keep their edits.
    fn render_conflict_banner(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let btn = |id: &'static str, label: &'static str, accent: bool| {
            div()
                .id(id)
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(rgb(BORDER))
                .when(accent, |d| d.bg(rgb(ACCENT)).text_color(rgb(SEL_TEXT)))
                .when(!accent, |d| d.text_color(rgb(TEXT)).hover(|s| s.bg(rgb(HOVER))))
                .cursor_pointer()
                .child(label)
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(34.))
            .px_3()
            .bg(rgb(PANEL_BG))
            .border_b_1()
            .border_color(rgb(ACCENT))
            .text_size(px(12.))
            .child(
                div()
                    .flex_grow(1.0)
                    .text_color(rgb(0xe2b340))
                    .child("⚠ This file changed on disk — you have unsaved edits."),
            )
            .child(btn("conflict-reload", "Reload (discard mine)", false).on_click(
                cx.listener(|this, _e, _w, cx| this.force_reload(cx)),
            ))
            .child(btn("conflict-keep", "Keep mine", true).on_click(
                cx.listener(|this, _e, _w, cx| this.keep_local(cx)),
            ))
    }

    fn render_search_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = if self.search_matches.is_empty() {
            "No results".to_string()
        } else {
            format!("{} of {}", self.search_idx + 1, self.search_matches.len())
        };
        div()
            .absolute()
            .top(px(6.))
            .right(px(16.))
            .w(px(420.))
            .h(px(32.))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .track_focus(&self.search_focus)
            .on_key_down(cx.listener(Self::search_key))
            .child(
                div()
                    .flex_grow(1.0)
                    .text_size(px(12.))
                    .text_color(rgb(TEXT))
                    .child(self.search_query.render("▏", SELECTION)),
            )
            .child(div().text_size(px(11.)).text_color(rgb(MUTED)).child(count))
            .child(
                div()
                    .id("search-prev")
                    .px_1()
                    .cursor_pointer()
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(TEXT)))
                    .child("↑")
                    .on_click(cx.listener(|this, _e, _w, cx| this.search_navigate(-1, cx))),
            )
            .child(
                div()
                    .id("search-next")
                    .px_1()
                    .cursor_pointer()
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(TEXT)))
                    .child("↓")
                    .on_click(cx.listener(|this, _e, _w, cx| this.search_navigate(1, cx))),
            )
            .child(
                div()
                    .id("search-close")
                    .px_1()
                    .cursor_pointer()
                    .text_color(rgb(MUTED))
                    .hover(|s| s.text_color(rgb(GIT_DELETED)))
                    .child("✕")
                    .on_click(cx.listener(|this, _e, window, cx| {
                        this.search_open = false;
                        this.search_matches.clear();
                        window.focus(&this.focus_handle, cx);
                        cx.notify();
                    })),
            )
    }
}

impl Editor {
    /// Small "read-only" badge floated just below the cursor (mirrors the
    /// completion-popup placement) when a hand edit is attempted while locked.
    fn render_ro_hint(&self) -> impl IntoElement {
        let (row, col) = self.offset_to_pos(self.cursor_offset());
        let cw = f32::from(self.last_char_width);
        let gw = f32::from(self.last_gutter_width);
        let x = gw + (col as f32) * cw - f32::from(self.scroll_x);
        let y = ((row + 1) as f32) * LINE_HEIGHT - f32::from(self.scroll_y);
        div()
            .absolute()
            .left(px(x))
            .top(px(y + 2.0))
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(GIT_MODIFIED))
            .shadow_lg()
            .text_size(px(11.))
            .text_color(rgb(GIT_MODIFIED))
            .child("🔒 Read-only — toggle EDIT to type")
    }

    fn render_completion(&self) -> impl IntoElement {
        let (row, col) = self.offset_to_pos(self.cursor_offset());
        let cw = f32::from(self.last_char_width);
        let gw = f32::from(self.last_gutter_width);
        // coords relative to the editor div (popup is its child)
        let x = gw + (col as f32) * cw - f32::from(self.scroll_x);
        let y = ((row + 1) as f32) * LINE_HEIGHT - f32::from(self.scroll_y);

        // window the list so the selected item is visible
        let start = if self.comp_sel >= 12 { self.comp_sel - 11 } else { 0 };
        let end = (start + 12).min(self.comp.len());

        let mut panel = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(380.))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(ACCENT))
            .rounded_md()
            .shadow_lg()
            .flex()
            .flex_col()
            .overflow_hidden();

        for i in start..end {
            let item = &self.comp[i];
            let sel = i == self.comp_sel;
            let detail: String = item.detail.chars().take(32).collect();
            panel = panel.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .h(px(20.))
                    .px_2()
                    .when(sel, |d| d.bg(rgb(SELECTED_BG)))
                    .child(
                        div()
                            .w(px(16.))
                            .text_color(rgb(MUTED))
                            .child(kind_icon(item.kind)),
                    )
                    .child(
                        div()
                            .flex_grow(1.0)
                            .text_color(rgb(if sel { SEL_TEXT } else { TEXT }))
                            .child(item.label.clone()),
                    )
                    .child(div().text_size(px(10.)).text_color(rgb(MUTED)).child(detail)),
            );
        }
        panel
    }
}

impl Editor {
    fn render_hover(&self, text: &str, x: f32, y: f32) -> impl IntoElement {
        let mut panel = div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .max_w(px(560.))
            .bg(rgb(POPUP_BG))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_md()
            .shadow_lg()
            .p_2()
            .flex()
            .flex_col();
        for line in text.lines().take(20) {
            panel = panel.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(POPUP_FG))
                    .child(line.to_string()),
            );
        }
        panel
    }
}

/// LSP languageId for a path.
fn lang_id(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        _ => "plaintext",
    }
}

/// A short tag for an LSP CompletionItemKind.
fn kind_icon(kind: u8) -> &'static str {
    match kind {
        2 | 3 => "ƒ",       // method / function
        5 => "›",           // field
        6 => "x",           // variable
        7 => "C",           // class
        8 => "I",           // interface
        9 => "M",           // module
        10 => "p",          // property
        13 => "e",          // enum
        14 => "K",          // keyword
        21 => "k",          // constant
        _ => "·",
    }
}
