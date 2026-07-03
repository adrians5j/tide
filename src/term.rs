use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor, Processor, StdSyncHandler};
use gpui::{
    App, Bounds, ClipboardItem, Context, DispatchPhase, Element, ElementId, Entity, FocusHandle, Focusable,
    GlobalElementId, InspectorElementId, KeyDownEvent, Keystroke, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollWheelEvent, ShapedLine, Style,
    TextRun, Window, div, fill, font, point, prelude::*, px, relative, rgb, size,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::theme::*;

const FONT: &str = "JetBrainsMono Nerd Font";
const FONT_SIZE: f32 = 13.0;
// Tight line height so box-drawing glyphs (│ ─ etc.) connect between rows
// instead of looking dashed. ≈ Menlo's natural ascent+descent at 13px.
const LINE_HEIGHT: f32 = 16.0;

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;
type SharedTerm = Arc<Mutex<Term<EventProxy>>>;

/// Minimal `Dimensions` impl (alacritty's `TermSize` is test-only).
struct TermDims {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// A text selection over the visible grid, in 0-based (row, col) cell
/// coordinates. Both endpoints are *inclusive* of the cell they name.
#[derive(Clone, Copy, PartialEq)]
struct Sel {
    anchor: (usize, usize),
    head: (usize, usize),
}

impl Sel {
    /// (start, end) ordered top-left → bottom-right. Tuple ordering compares
    /// row first, then column — exactly the reading order we want.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

// ── event proxy: handles writes-back and repaint signals ───────────────────

#[derive(Clone)]
pub struct EventProxy {
    writer: Writer,
    dirty: Arc<AtomicBool>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => {
                if let Ok(mut w) = self.writer.lock() {
                    let _ = w.write_all(s.as_bytes());
                }
            }
            Event::Wakeup | Event::MouseCursorDirty => {
                self.dirty.store(true, Ordering::Relaxed);
            }
            _ => {}
        }
    }
}

/// Scroll dampening factor (lines emitted per line-height of trackpad travel).
/// 1.0 keeps the raw speed; lower is calmer. Set `TIDE_SCROLL_SPEED` to tune
/// without a rebuild; defaults to a gentler-than-native 0.4.
fn scroll_speed() -> f32 {
    use std::sync::OnceLock;
    static SPEED: OnceLock<f32> = OnceLock::new();
    *SPEED.get_or_init(|| {
        std::env::var("TIDE_SCROLL_SPEED")
            .ok()
            .and_then(|v| v.trim().parse::<f32>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(0.4)
    })
}

// ── Terminal entity ────────────────────────────────────────────────────────

pub struct Terminal {
    pub focus_handle: FocusHandle,
    term: SharedTerm,
    writer: Writer,
    master: Box<dyn MasterPty + Send>,
    dirty: Arc<AtomicBool>,
    cols: usize,
    rows: usize,
    /// per-visible-row cache: (content hash, shaped line) — avoids re-shaping
    /// unchanged rows every frame while a TUI floods output
    shaped_cache: Vec<(u64, ShapedLine)>,
    last_bounds: Option<Bounds<Pixels>>,
    last_char_width: Pixels,
    /// while the user drags the pane divider, skip live PTY resizes (which make
    /// apps like zellij redraw fully and flood output) — resize once on release
    pub defer_resize: bool,
    cursor_on: bool,
    /// fractional scroll lines banked between wheel events, so trackpad
    /// pixel-deltas emit whole lines at the damped speed (see on_scroll)
    scroll_accum: f32,
    /// current text selection (word, drag, or select-all), in grid cells
    selection: Option<Sel>,
    /// true while a left-drag selection is in progress
    selecting: bool,
    /// text of the current selection, for cmd+c
    sel_text: Option<String>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Terminal {
    pub fn new(working_dir: std::path::PathBuf, cx: &mut Context<Self>) -> Self {
        let cols = 80usize;
        let rows = 24usize;

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows as u16,
                cols: cols as u16,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut cmd = CommandBuilder::new(shell);
        cmd.cwd(working_dir);
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd).expect("spawn shell");
        drop(pair.slave);

        let writer: Writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
        let dirty = Arc::new(AtomicBool::new(true));
        let proxy = EventProxy { writer: writer.clone(), dirty: dirty.clone() };

        let term = Term::new(Config::default(), &TermDims { columns: cols, screen_lines: rows }, proxy);
        let term: SharedTerm = Arc::new(Mutex::new(term));

        // reader thread: feed PTY output into the VT parser
        {
            let mut reader = pair.master.try_clone_reader().expect("reader");
            let term = term.clone();
            let dirty = dirty.clone();
            std::thread::spawn(move || {
                let mut parser: Processor<StdSyncHandler> = Processor::new();
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if let Ok(mut t) = term.lock() {
                                parser.advance(&mut *t, &buf[..n]);
                            }
                            dirty.store(true, Ordering::Relaxed);
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // repaint pump: ~60fps, only notifies when the grid changed
        {
            let dirty = dirty.clone();
            cx.spawn(async move |this, cx| loop {
                cx.background_executor().timer(Duration::from_millis(16)).await;
                if dirty.swap(false, Ordering::Relaxed) {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }

        // cursor blink (~530ms)
        cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_millis(530)).await;
            if this.update(cx, |t, cx| { t.cursor_on = !t.cursor_on; cx.notify(); }).is_err() {
                break;
            }
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            term,
            writer,
            master: pair.master,
            dirty,
            cols,
            rows,
            shaped_cache: Vec::new(),
            cursor_on: true,
            scroll_accum: 0.0,
            last_bounds: None,
            last_char_width: px(8.),
            defer_resize: false,
            selection: None,
            selecting: false,
            sel_text: None,
            _child: child,
        }
    }

    /// Map a window-space position to a 0-based (col, row) display cell.
    fn cell_at0(&self, pos: gpui::Point<Pixels>) -> Option<(usize, usize)> {
        let b = self.last_bounds?;
        let cw = f32::from(self.last_char_width).max(1.0);
        let col = (f32::from(pos.x - b.left()) / cw).floor().max(0.) as usize;
        let row = (f32::from(pos.y - b.top()) / LINE_HEIGHT).floor().max(0.) as usize;
        Some((col, row))
    }

    /// Select the whole word under `pos` (like a double-click in the editor).
    fn select_word_at(&mut self, pos: gpui::Point<Pixels>) {
        let Some((col, row)) = self.cell_at0(pos) else { return };
        // read the clicked display row's characters
        let chars: Vec<char> = {
            let Ok(guard) = self.term.lock() else { return };
            let content = guard.renderable_content();
            let mut chars = vec![' '; self.cols];
            for indexed in content.display_iter {
                if indexed.point.line.0 == row as i32 {
                    let c = indexed.point.column.0;
                    if c < chars.len() {
                        chars[c] = if indexed.cell.c == '\0' { ' ' } else { indexed.cell.c };
                    }
                }
            }
            chars
        };
        if col >= chars.len() {
            return;
        }
        let is_word = |c: char| c.is_alphanumeric() || "_-./@:~".contains(c);
        if !is_word(chars[col]) {
            return;
        }
        let mut start = col;
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        let mut end = col + 1;
        while end < chars.len() && is_word(chars[end]) {
            end += 1;
        }
        self.sel_text = Some(chars[start..end].iter().collect());
        // store inclusive of the last word cell (end is one-past)
        self.selection = Some(Sel { anchor: (row, start), head: (row, end - 1) });
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        self.selecting = false;
        self.sel_text = None;
    }

    /// Map a window position to a clamped 0-based (row, col) grid cell.
    fn sel_cell(&self, pos: gpui::Point<Pixels>) -> Option<(usize, usize)> {
        let (col, row) = self.cell_at0(pos)?;
        Some((row.min(self.rows.saturating_sub(1)), col.min(self.cols.saturating_sub(1))))
    }

    /// True while a TUI (vim, zellij, …) has mouse reporting enabled, in which
    /// case clicks are forwarded to it instead of starting a text selection.
    fn mouse_reporting(&self) -> bool {
        self.term
            .lock()
            .map(|g| g.mode().intersects(TermMode::MOUSE_MODE))
            .unwrap_or(false)
    }

    /// Snapshot the visible grid as rows of chars (blanks for empty cells).
    fn grid_chars(&self) -> Vec<Vec<char>> {
        let Ok(guard) = self.term.lock() else { return Vec::new() };
        let content = guard.renderable_content();
        let mut grid = vec![vec![' '; self.cols]; self.rows];
        for indexed in content.display_iter {
            let r = indexed.point.line.0;
            let c = indexed.point.column.0;
            if r >= 0 && (r as usize) < self.rows && c < self.cols {
                grid[r as usize][c] = if indexed.cell.c == '\0' { ' ' } else { indexed.cell.c };
            }
        }
        grid
    }

    /// Extract the selected text: each row's column span, trailing whitespace
    /// trimmed, joined by newlines (with trailing blank lines dropped).
    fn extract_selection(&self, sel: Sel) -> String {
        let (start, end) = sel.ordered();
        let grid = self.grid_chars();
        let mut out = String::new();
        for row in start.0..=end.0 {
            let Some(line) = grid.get(row) else { continue };
            let from = if row == start.0 { start.1 } else { 0 }.min(self.cols.saturating_sub(1));
            let to = if row == end.0 { end.1 } else { self.cols.saturating_sub(1) };
            let to = to.min(self.cols.saturating_sub(1));
            if from <= to {
                let s: String = line[from..=to].iter().collect();
                out.push_str(s.trim_end());
            }
            if row != end.0 {
                out.push('\n');
            }
        }
        while out.ends_with('\n') {
            out.pop();
        }
        out
    }

    /// Select everything in the visible grid (cmd+a).
    fn select_all(&mut self) {
        if self.cols == 0 || self.rows == 0 {
            return;
        }
        let sel = Sel { anchor: (0, 0), head: (self.rows - 1, self.cols - 1) };
        self.sel_text = Some(self.extract_selection(sel));
        self.selection = Some(sel);
        self.selecting = false;
    }

    /// Map a window-space position to a 1-based (col, row) terminal cell.
    fn cell_at(&self, pos: gpui::Point<Pixels>) -> Option<(i64, i64)> {
        let b = self.last_bounds?;
        let cw = f32::from(self.last_char_width).max(1.0);
        let col = ((f32::from(pos.x - b.left()) / cw).floor() as i64 + 1).max(1);
        let row = ((f32::from(pos.y - b.top()) / LINE_HEIGHT).floor() as i64 + 1).max(1);
        Some((col, row))
    }

    /// Forward a mouse event to the PTY as an SGR sequence when the running
    /// app has mouse reporting enabled (zellij, vim, htop, …).
    fn forward_mouse(&self, button: i64, pressed: bool, pos: gpui::Point<Pixels>) {
        let (report, sgr) = {
            let Ok(g) = self.term.lock() else { return };
            let m = g.mode();
            (m.intersects(TermMode::MOUSE_MODE), m.contains(TermMode::SGR_MOUSE))
        };
        if !report {
            return;
        }
        let Some((col, row)) = self.cell_at(pos) else { return };
        let seq = if sgr {
            format!("\x1b[<{};{};{}{}", button, col, row, if pressed { 'M' } else { 'm' })
        } else {
            // legacy X10: button byte + 32, coords + 32
            let cb = (button + 32) as u8;
            let cx = (col + 32).min(255) as u8;
            let cy = (row + 32).min(255) as u8;
            let mut v = vec![0x1b, b'[', b'M', cb, cx, cy];
            if !pressed {
                v[3] = 3 + 32; // release button code
            }
            self.write(&v);
            return;
        };
        self.write(seq.as_bytes());
    }

    fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Type text into the shell (used to run zsh aliases / git commands from
    /// toolbar buttons — output shows in the terminal pane).
    pub fn send_text(&self, s: &str) {
        self.write(s.as_bytes());
    }

    /// Kill the child shell (when closing a terminal tab).
    pub fn kill(&mut self) {
        let _ = self._child.kill();
    }

    /// PID of the shell this terminal spawned (root of its process subtree).
    pub fn child_pid(&self) -> Option<u32> {
        self._child.process_id()
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        if cols == 0 || rows == 0 {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        let _ = self.master.resize(PtySize {
            rows: rows as u16,
            cols: cols as u16,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut t) = self.term.lock() {
            t.resize(TermDims { columns: cols, screen_lines: rows });
        }
        self.shaped_cache.clear();
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        // keep the cursor solid while actively typing
        self.cursor_on = true;
        // cmd+c copies the current selection (don't forward it to the shell).
        // Compute from the live selection so it works for drag, double-click and
        // select-all alike — not just whatever last set `sel_text`.
        if ks.modifiers.platform && ks.key == "c" {
            if let Some(sel) = self.selection {
                let text = self.extract_selection(sel);
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }
            return;
        }
        // cmd+a selects everything in the visible grid (don't forward)
        if ks.modifiers.platform && ks.key == "a" {
            self.select_all();
            cx.notify();
            return;
        }
        // any other key clears the selection highlight
        if self.selection.is_some() {
            self.clear_selection();
        }
        // cmd+v → paste clipboard into the shell (bracketed if the app asked for it)
        if ks.modifiers.platform && ks.key == "v" {
            self.scroll_to_bottom();
            if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                let bracketed = self
                    .term
                    .lock()
                    .map(|g| g.mode().contains(TermMode::BRACKETED_PASTE))
                    .unwrap_or(false);
                // send the whole paste as ONE write — splitting the bracketed
                // markers from the body across separate flushes can leave zsh's
                // bracketed-paste widget mid-read, so it buffers the text without
                // redrawing the line (paste looks invisible until the next key)
                let mut buf = Vec::with_capacity(text.len() + 12);
                if bracketed {
                    buf.extend_from_slice(b"\x1b[200~");
                    buf.extend_from_slice(text.as_bytes());
                    buf.extend_from_slice(b"\x1b[201~");
                } else {
                    buf.extend_from_slice(text.as_bytes());
                }
                self.write(&buf);
            }
            return;
        }
        if let Some(bytes) = keystroke_to_bytes(ks) {
            self.scroll_to_bottom(); // typing jumps back to the prompt
            self.write(&bytes);
        }
    }

    fn on_mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        if ev.button == MouseButton::Left {
            // double-click selects the word under the cursor (like the editor)
            if ev.click_count >= 2 {
                self.select_word_at(ev.position);
                self.selecting = false;
                cx.notify();
                return;
            }
            // begin a tentative selection; mouse-up decides drag (select) vs a
            // plain click (forwarded to a mouse-reporting app like zellij)
            if let Some(cell) = self.sel_cell(ev.position) {
                self.selection = Some(Sel { anchor: cell, head: cell });
                self.selecting = true;
                self.sel_text = None;
                cx.notify();
            }
            return;
        }
        // other buttons: clear any selection + forward to the app
        if self.selection.is_some() {
            self.clear_selection();
            cx.notify();
        }
        let button = match ev.button {
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        self.forward_mouse(button, true, ev.position);
    }

    fn on_mouse_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let _ = cx;
        // left-button release (selection finalize / click-forward) is handled by
        // the window-level listener in paint; here we only forward other buttons
        if ev.button == MouseButton::Left {
            return;
        }
        if !self.mouse_reporting() {
            return;
        }
        let button = match ev.button {
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
            _ => return,
        };
        self.forward_mouse(button, false, ev.position);
    }

    fn on_scroll(&mut self, ev: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let report = self.term.lock().map(|g| g.mode().intersects(TermMode::MOUSE_MODE)).unwrap_or(false);
        let dy = f32::from(ev.delta.pixel_delta(px(LINE_HEIGHT)).y);
        // Bank fractional lines and apply a speed factor so a macOS trackpad's
        // large, momentum-driven pixel deltas don't overscroll. TIDE_SCROLL_SPEED
        // tunes it (1.0 = one line per line-height of travel; lower = calmer).
        self.scroll_accum += (dy / LINE_HEIGHT) * scroll_speed();
        let lines = self.scroll_accum.trunc() as i32;
        if lines == 0 {
            return; // keep banking until a whole line accrues
        }
        self.scroll_accum -= lines as f32;
        // a mouse-reporting app (vim, claude code, …) wants the wheel itself
        if report {
            let button = if lines > 0 { 64 } else { 65 };
            for _ in 0..lines.unsigned_abs().min(5) {
                self.forward_mouse(button, true, ev.position);
            }
            return;
        }
        // otherwise scroll our own scrollback buffer (positive dy = up = history)
        if let Ok(mut g) = self.term.lock() {
            g.scroll_display(Scroll::Delta(lines));
        }
        self.dirty.store(true, Ordering::Relaxed);
        cx.notify();
    }

    /// Snap the viewport back to the prompt (used when the user types).
    fn scroll_to_bottom(&self) {
        if let Ok(mut g) = self.term.lock() {
            g.scroll_display(Scroll::Bottom);
        }
    }
}

impl Focusable for Terminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Terminal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .size_full()
            .pl_2()
            .pt_1()
            .bg(rgb(BG))
            .text_size(px(FONT_SIZE))
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TerminalElement { term: cx.entity() })
    }
}

// ── translate a GPUI keystroke into terminal bytes ─────────────────────────

fn keystroke_to_bytes(ks: &Keystroke) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let key = ks.key.as_str();

    // Ctrl + letter → control byte
    if m.control && key.len() == 1 {
        let c = key.chars().next().unwrap().to_ascii_lowercase();
        if c.is_ascii_alphabetic() {
            return Some(vec![(c as u8) - b'a' + 1]);
        }
    }

    // Option + Left/Right → word motion (ESC b / ESC f), matching Ghostty's
    // default so zsh/readline word-jump works out of the box. (The plain
    // xterm "alt+arrow" CSI form below isn't bound to word motion by shells.)
    if m.alt && !m.control && !m.shift {
        match key {
            "left" => return Some(vec![0x1b, b'b']),
            "right" => return Some(vec![0x1b, b'f']),
            _ => {}
        }
    }

    // xterm modifier code: 1 + shift(1) + alt(2) + ctrl(4). Encoded into CSI
    // sequences so apps like zellij see opt/shift/ctrl + arrow.
    let mod_code = 1 + (m.shift as u8) + ((m.alt as u8) * 2) + ((m.control as u8) * 4);
    let modified = mod_code > 1;

    // Cursor keys: ESC[<letter> plain, ESC[1;<mod><letter> when modified.
    if let Some(letter) = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    } {
        let s = if modified {
            format!("\x1b[1;{mod_code}{letter}")
        } else {
            format!("\x1b[{letter}")
        };
        return Some(s.into_bytes());
    }

    // Tilde-style keys: ESC[<n>~ plain, ESC[<n>;<mod>~ when modified.
    if let Some(n) = match key {
        "pageup" => Some(5),
        "pagedown" => Some(6),
        "delete" => Some(3),
        _ => None,
    } {
        let s = if modified {
            format!("\x1b[{n};{mod_code}~")
        } else {
            format!("\x1b[{n}~")
        };
        return Some(s.into_bytes());
    }

    let seq: &[u8] = match key {
        "enter" => b"\r",
        "backspace" => b"\x7f",
        "tab" => b"\t",
        "escape" => b"\x1b",
        _ => {
            if let Some(kc) = &ks.key_char {
                // Alt prefixes with ESC (meta)
                if m.alt {
                    let mut v = vec![0x1b];
                    v.extend_from_slice(kc.as_bytes());
                    return Some(v);
                }
                return Some(kc.as_bytes().to_vec());
            }
            return None;
        }
    };
    Some(seq.to_vec())
}

// ── color resolution (standard xterm-256 palette + Tokyo Night defaults) ────

fn named_rgb(c: NamedColor) -> (u8, u8, u8) {
    use NamedColor::*;
    match c {
        Background => (0x1f, 0x1f, 0x1f),
        Foreground => (0xd4, 0xd4, 0xd4),
        Black => (0x00, 0x00, 0x00),
        Red => (0xcd, 0x31, 0x31),
        Green => (0x0d, 0xbc, 0x79),
        Yellow => (0xe5, 0xe5, 0x10),
        Blue => (0x24, 0x72, 0xc8),
        Magenta => (0xbc, 0x3f, 0xbc),
        Cyan => (0x11, 0xa8, 0xcd),
        White => (0xe5, 0xe5, 0xe5),
        BrightBlack => (0x66, 0x66, 0x66),
        BrightRed => (0xf1, 0x4c, 0x4c),
        BrightGreen => (0x23, 0xd1, 0x8b),
        BrightYellow => (0xf5, 0xf5, 0x43),
        BrightBlue => (0x3b, 0x8e, 0xea),
        BrightMagenta => (0xd6, 0x70, 0xd6),
        BrightCyan => (0x29, 0xb8, 0xdb),
        BrightWhite => (0xe5, 0xe5, 0xe5),
        _ => (0xd4, 0xd4, 0xd4),
    }
}

fn indexed_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => {
            const NC: [NamedColor; 16] = [
                NamedColor::Black, NamedColor::Red, NamedColor::Green, NamedColor::Yellow,
                NamedColor::Blue, NamedColor::Magenta, NamedColor::Cyan, NamedColor::White,
                NamedColor::BrightBlack, NamedColor::BrightRed, NamedColor::BrightGreen,
                NamedColor::BrightYellow, NamedColor::BrightBlue, NamedColor::BrightMagenta,
                NamedColor::BrightCyan, NamedColor::BrightWhite,
            ];
            named_rgb(NC[i as usize])
        }
        16..=231 => {
            let i = i - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let conv = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (conv(r), conv(g), conv(b))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

fn color_u32(c: VteColor) -> u32 {
    let (r, g, b) = match c {
        VteColor::Named(n) => named_rgb(n),
        VteColor::Indexed(i) => indexed_rgb(i),
        VteColor::Spec(rgb) => (rgb.r, rgb.g, rgb.b),
    };
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

// ── custom element: paints the terminal grid ───────────────────────────────

pub struct TerminalElement {
    term: Entity<Terminal>,
}

pub struct TermPrepaint {
    rows: Vec<(Pixels, ShapedLine)>,
    bgs: Vec<gpui::PaintQuad>,
    decorations: Vec<gpui::PaintQuad>,
    cursor: Option<gpui::PaintQuad>,
    scrollbar: Option<gpui::PaintQuad>,
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TermPrepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _i: Option<&InspectorElementId>,
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
        _i: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> TermPrepaint {
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

        // resize the pty/term to fit the element
        let cols = (f32::from(bounds.size.width) / f32::from(char_width)).floor().max(1.) as usize;
        let rows = (f32::from(bounds.size.height) / LINE_HEIGHT).floor().max(1.) as usize;
        if !self.term.read(cx).defer_resize {
            self.term.update(cx, |t, _| t.resize(cols, rows));
        }

        // Pass 1 (under lock): build per-row text+runs+hash, bg quads, and
        // custom-drawn box/block glyphs.
        type RawRow = (Pixels, String, Vec<TextRun>, u64);
        let (raw_rows, mut bgs, decorations, cursor_point): (
            Vec<RawRow>,
            Vec<gpui::PaintQuad>,
            Vec<gpui::PaintQuad>,
            _,
        ) = {
            let terminal = self.term.read(cx);
            let guard = terminal.term.lock().unwrap();
            let content = guard.renderable_content();

            let mut raw_rows: Vec<RawRow> = Vec::new();
            let mut bgs = Vec::new();
            let mut decorations = Vec::new();
            let mut cur_line: i32 = i32::MIN;
            let mut line_str = String::new();
            let mut runs: Vec<TextRun> = Vec::new();
            let mut row_hash: u64 = 0;
            // color of the run currently being accumulated (sentinel = none yet)
            let mut run_color: u32 = u32::MAX;

            for indexed in content.display_iter {
                let cell = indexed.cell;
                let line = indexed.point.line.0;
                let col = indexed.point.column.0;

                if line != cur_line {
                    if cur_line != i32::MIN {
                        let y = bounds.top() + line_height * (cur_line as f32);
                        raw_rows.push((
                            y,
                            std::mem::take(&mut line_str),
                            std::mem::take(&mut runs),
                            row_hash,
                        ));
                    }
                    cur_line = line;
                    row_hash = 0;
                    run_color = u32::MAX;
                }

                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let cell_x = bounds.left() + char_width * (col as f32);
                let cell_y = bounds.top() + line_height * (line as f32);

                let bg = cell.bg;
                if !matches!(bg, VteColor::Named(NamedColor::Background)) {
                    bgs.push(fill(
                        Bounds::new(point(cell_x, cell_y), size(char_width, line_height)),
                        rgb(color_u32(bg)),
                    ));
                }

                let ch = if cell.c == '\0' { ' ' } else { cell.c };
                let mut fg = cell.fg;
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut { bg });
                }
                let fg_rgb = color_u32(fg);
                row_hash = row_hash
                    .wrapping_mul(1099511628211)
                    .wrapping_add(ch as u64)
                    .wrapping_mul(1099511628211)
                    .wrapping_add(fg_rgb as u64);

                // Box-drawing & block glyphs → draw as rects so they connect
                // perfectly. Keep a space in the text to preserve column width.
                let draw_char = if let Some(quads) =
                    glyph_quads(ch, cell_x, cell_y, char_width, line_height, fg_rgb)
                {
                    decorations.extend(quads);
                    ' '
                } else {
                    ch
                };

                // Coalesce adjacent cells of the same color into one TextRun.
                // shape_line cost scales with run count, so a 200-col row that
                // was 200 runs collapses to a handful — the big win for TUIs.
                let len = draw_char.len_utf8();
                if fg_rgb == run_color {
                    if let Some(last) = runs.last_mut() {
                        last.len += len;
                    }
                } else {
                    run_color = fg_rgb;
                    runs.push(TextRun {
                        len,
                        font: font(FONT),
                        color: rgb(fg_rgb).into(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                }
                line_str.push(draw_char);
            }
            if cur_line != i32::MIN {
                let y = bounds.top() + line_height * (cur_line as f32);
                raw_rows.push((y, line_str, runs, row_hash));
            }

            (raw_rows, bgs, decorations, content.cursor.point)
        };

        // selection highlight (drawn over cell bgs, under text) — one quad per
        // row in the span, inclusive of the head/anchor cells.
        if let Some(sel) = self.term.read(cx).selection {
            let cols = self.term.read(cx).cols;
            let (s, e) = sel.ordered();
            for row in s.0..=e.0 {
                let from = if row == s.0 { s.1 } else { 0 };
                let to = if row == e.0 { e.1 } else { cols.saturating_sub(1) };
                if to < from {
                    continue;
                }
                let y = bounds.top() + line_height * (row as f32);
                let x0 = bounds.left() + char_width * (from as f32);
                let x1 = bounds.left() + char_width * ((to + 1) as f32);
                bgs.push(fill(
                    Bounds::new(point(x0, y), size(x1 - x0, line_height)),
                    rgb(SELECTION),
                ));
            }
        }

        // Pass 2: reuse cached shaped lines whose content hash is unchanged.
        // Move the cache out (not clone) so only re-used lines get cloned.
        let old_cache = self.term.update(cx, |t, _| std::mem::take(&mut t.shaped_cache));
        let mut rows_out: Vec<(Pixels, ShapedLine)> = Vec::with_capacity(raw_rows.len());
        let mut new_cache: Vec<(u64, ShapedLine)> = Vec::with_capacity(raw_rows.len());
        for (i, (y, text, runs, hash)) in raw_rows.into_iter().enumerate() {
            let shaped = match old_cache.get(i) {
                Some((old_hash, old_line)) if *old_hash == hash => old_line.clone(),
                _ => text_sys.shape_line(text.into(), font_size, &runs, None),
            };
            new_cache.push((hash, shaped.clone()));
            rows_out.push((y, shaped));
        }
        self.term.update(cx, |t, _| {
            t.shaped_cache = new_cache;
            t.last_bounds = Some(bounds);
            t.last_char_width = char_width;
        });

        // cursor (blinking)
        let cursor = if self.term.read(cx).cursor_on {
            let cx_pos = bounds.left() + char_width * (cursor_point.column.0 as f32);
            let cy = bounds.top() + line_height * (cursor_point.line.0 as f32);
            Some(fill(
                Bounds::new(point(cx_pos, cy), size(char_width, line_height)),
                rgb(ACCENT),
            ))
        } else {
            None
        };

        // vertical scrollbar reflecting the scrollback position. Only the normal
        // screen has history; alt-screen apps (which own their own scroll) report
        // history_size == 0, so the bar simply doesn't show for them.
        let scrollbar = {
            let (offset, history) = self
                .term
                .read(cx)
                .term
                .lock()
                .map(|g| (g.grid().display_offset(), g.grid().history_size()))
                .unwrap_or((0, 0));
            if history == 0 || rows == 0 {
                None
            } else {
                let track_h = f32::from(bounds.size.height);
                let total = (history + rows) as f32;
                let thumb_h = (track_h * rows as f32 / total).clamp(24.0, track_h);
                // lines scrolled above the viewport top → thumb position
                let content_top = (history - offset) as f32;
                let max_top = (track_h - thumb_h).max(0.0);
                let thumb_top = (track_h * content_top / total).clamp(0.0, max_top);
                let sb_w = 5.0;
                let x = bounds.right() - px(sb_w + 1.0);
                let y = bounds.top() + px(thumb_top);
                Some(fill(
                    Bounds::new(point(x, y), size(px(sb_w), px(thumb_h))),
                    gpui::rgba(0x9aa0a6cc),
                ))
            }
        };

        TermPrepaint { rows: rows_out, bgs, decorations, cursor, scrollbar }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _i: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _layout: &mut (),
        prepaint: &mut TermPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        for bg in prepaint.bgs.drain(..) {
            window.paint_quad(bg);
        }
        let focused = self.term.read(cx).focus_handle.is_focused(window);
        if focused {
            if let Some(cur) = prepaint.cursor.take() {
                window.paint_quad(cur);
            }
        }
        for (y, line) in prepaint.rows.drain(..) {
            line.paint(point(_bounds.left(), y), px(LINE_HEIGHT), gpui::TextAlign::Left, None, window, cx).ok();
        }
        // box-drawing / block glyphs on top
        for d in prepaint.decorations.drain(..) {
            window.paint_quad(d);
        }
        // scrollbar thumb, on top of everything
        if let Some(sb) = prepaint.scrollbar.take() {
            window.paint_quad(sb);
        }

        // Window-level drag handlers: element-scoped `on_mouse_move` is gated to
        // the hover phase and stops firing once a button is held, so a press +
        // drag never extends the selection. Registering on the window (fresh
        // each paint) guarantees we keep getting move/up events mid-drag.
        let term = self.term.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase: DispatchPhase, window: &mut Window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let changed = term.update(cx, |t, cx| {
                if !t.selecting || ev.pressed_button != Some(MouseButton::Left) {
                    return false;
                }
                if let Some(cell) = t.sel_cell(ev.position) {
                    if let Some(sel) = t.selection.as_mut() {
                        if sel.head != cell {
                            sel.head = cell;
                            cx.notify();
                            // also flag the async repaint pump (the proven-live
                            // path used for streaming output)
                            t.dirty.store(true, Ordering::Relaxed);
                            return true;
                        }
                    }
                }
                false
            });
            // a plain notify from inside the drag's event dispatch doesn't paint
            // until the gesture ends; force the frame so the highlight tracks live
            if changed {
                window.refresh();
            }
        });
        let term_up = self.term.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase: DispatchPhase, _window, cx| {
            if phase != DispatchPhase::Bubble || ev.button != MouseButton::Left {
                return;
            }
            term_up.update(cx, |t, cx| {
                if !t.selecting {
                    return;
                }
                t.selecting = false;
                match t.selection {
                    // a real drag → keep the selection (ready for cmd+c)
                    Some(sel) if sel.anchor != sel.head => {
                        t.sel_text = Some(t.extract_selection(sel));
                    }
                    // a plain click (no drag) → forward it to a mouse-reporting
                    // app so zellij/vim still get clicks; otherwise just clear
                    _ => {
                        t.clear_selection();
                        if t.mouse_reporting() {
                            t.forward_mouse(0, true, ev.position);
                            t.forward_mouse(0, false, ev.position);
                        }
                    }
                }
                cx.notify();
            });
        });
    }
}

/// Render box-drawing (U+2500–257F) and block elements (U+2580–259F) as solid
/// rects so they connect cleanly, like a real terminal. Returns None for
/// ordinary glyphs (rendered via the font).
fn glyph_quads(c: char, x: Pixels, y: Pixels, w: Pixels, h: Pixels, color: u32) -> Option<Vec<gpui::PaintQuad>> {
    let xf = f32::from(x);
    let yf = f32::from(y);
    let wf = f32::from(w);
    let hf = f32::from(h);
    let t = (hf / 9.0).max(1.5); // line thickness
    let midx = xf + (wf - t) / 2.0;
    let midy = yf + (hf - t) / 2.0;

    let rect = |x: f32, y: f32, w: f32, h: f32| {
        fill(
            Bounds::new(point(px(x), px(y)), size(px(w), px(h))),
            rgb(color),
        )
    };
    let shade = |alpha: f32| {
        fill(
            Bounds::new(point(x, y), size(w, h)),
            gpui::Rgba {
                r: ((color >> 16) & 0xff) as f32 / 255.0,
                g: ((color >> 8) & 0xff) as f32 / 255.0,
                b: (color & 0xff) as f32 / 255.0,
                a: alpha,
            },
        )
    };

    let h_full = || rect(xf, midy, wf, t);
    let v_full = || rect(midx, yf, t, hf);
    let h_left = || rect(xf, midy, (wf + t) / 2.0, t);
    let h_right = || rect(midx, midy, (wf + t) / 2.0, t);
    let v_top = || rect(midx, yf, t, (hf + t) / 2.0);
    let v_bot = || rect(midx, midy, t, (hf + t) / 2.0);

    let quads: Vec<gpui::PaintQuad> = match c {
        // lines
        '─' | '━' => vec![h_full()],
        '│' | '┃' => vec![v_full()],
        // corners (sharp + rounded)
        '┌' | '┏' | '╭' => vec![h_right(), v_bot()],
        '┐' | '┓' | '╮' => vec![h_left(), v_bot()],
        '└' | '┗' | '╰' => vec![h_right(), v_top()],
        '┘' | '┛' | '╯' => vec![h_left(), v_top()],
        // tees
        '├' | '┣' => vec![v_full(), h_right()],
        '┤' | '┫' => vec![v_full(), h_left()],
        '┬' | '┳' => vec![h_full(), v_bot()],
        '┴' | '┻' => vec![h_full(), v_top()],
        '┼' | '╋' => vec![h_full(), v_full()],
        // block elements
        '█' => vec![rect(xf, yf, wf, hf)],
        '▀' => vec![rect(xf, yf, wf, hf / 2.0)],
        '▄' => vec![rect(xf, yf + hf / 2.0, wf, hf / 2.0)],
        '▌' => vec![rect(xf, yf, wf / 2.0, hf)],
        '▐' => vec![rect(xf + wf / 2.0, yf, wf / 2.0, hf)],
        '▖' => vec![rect(xf, yf + hf / 2.0, wf / 2.0, hf / 2.0)],
        '▗' => vec![rect(xf + wf / 2.0, yf + hf / 2.0, wf / 2.0, hf / 2.0)],
        '▘' => vec![rect(xf, yf, wf / 2.0, hf / 2.0)],
        '▝' => vec![rect(xf + wf / 2.0, yf, wf / 2.0, hf / 2.0)],
        '▙' => vec![rect(xf, yf, wf / 2.0, hf), rect(xf, yf + hf / 2.0, wf, hf / 2.0)],
        '▟' => vec![rect(xf + wf / 2.0, yf, wf / 2.0, hf), rect(xf, yf + hf / 2.0, wf, hf / 2.0)],
        '▛' => vec![rect(xf, yf, wf, hf / 2.0), rect(xf, yf, wf / 2.0, hf)],
        '▜' => vec![rect(xf, yf, wf, hf / 2.0), rect(xf + wf / 2.0, yf, wf / 2.0, hf)],
        '░' => vec![shade(0.25)],
        '▒' => vec![shade(0.5)],
        '▓' => vec![shade(0.75)],
        _ => return None,
    };
    Some(quads)
}
