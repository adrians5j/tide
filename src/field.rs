//! Shared single-line text-input buffer with caret, selection, undo/redo and
//! the standard editing keys. Used by every chrome input and the editor search.
use gpui::{div, prelude::*, rgb, Div, Keystroke};

fn is_word_sep(c: char) -> bool {
    c.is_whitespace() || matches!(c, '/' | '.' | '_' | '-')
}

/// Byte offset of the char boundary just left of `i`.
fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map(|(b, _)| b).unwrap_or(0)
}

/// Byte offset of the char boundary just right of `i`.
fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].char_indices().nth(1).map(|(b, _)| i + b).unwrap_or_else(|| s.len())
}

/// Word boundary to the left of `i` (skips separators, then a run of word chars).
fn word_left(s: &str, mut i: usize) -> usize {
    while i > 0 && s[..i].chars().next_back().is_some_and(is_word_sep) {
        i = prev_boundary(s, i);
    }
    while i > 0 && s[..i].chars().next_back().is_some_and(|c| !is_word_sep(c)) {
        i = prev_boundary(s, i);
    }
    i
}

/// Word boundary to the right of `i`.
fn word_right(s: &str, mut i: usize) -> usize {
    let len = s.len();
    while i < len && s[i..].chars().next().is_some_and(is_word_sep) {
        i = next_boundary(s, i);
    }
    while i < len && s[i..].chars().next().is_some_and(|c| !is_word_sep(c)) {
        i = next_boundary(s, i);
    }
    i
}

/// What a keystroke did to a [`Field`]; callers re-filter results on `Changed`.
#[derive(PartialEq)]
pub enum Edit {
    None,
    Moved,
    Changed,
}

/// A single-line text buffer with caret + selection, undo/redo, and the usual
/// editing keys. Shared by every chrome input so they all behave the same:
/// arrow / opt-arrow / cmd-arrow navigation, shift-selection, cmd+a select-all,
/// cmd+c/x/v clipboard, cmd+z/cmd+shift+z undo/redo, and word/line deletes.
#[derive(Default)]
pub struct Field {
    pub text: String,
    pub caret: usize,
    anchor: Option<usize>,        // selection anchor; selection spans anchor..caret
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    typing: bool,                 // coalesce a run of typed chars into one undo step
    copy_out: Option<String>,     // text the caller should put on the clipboard
}

impl Field {
    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
        self.anchor = None;
        self.undo.clear();
        self.redo.clear();
        self.typing = false;
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Replace the whole buffer, caret at the end.
    pub fn set(&mut self, s: String) {
        self.caret = s.len();
        self.text = s;
        self.anchor = None;
    }

    fn insert(&mut self, s: &str) {
        self.text.insert_str(self.caret, s);
        self.caret += s.len();
    }

    /// Selection as a (start, end) byte range, or None when empty.
    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        if a == self.caret {
            None
        } else {
            Some((a.min(self.caret), a.max(self.caret)))
        }
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((s, e)) = self.selection() {
            self.text.replace_range(s..e, "");
            self.caret = s;
            self.anchor = None;
            true
        } else {
            false
        }
    }

    fn push_undo(&mut self) {
        self.undo.push((self.text.clone(), self.caret));
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn undo_op(&mut self) {
        if let Some((t, c)) = self.undo.pop() {
            self.redo.push((self.text.clone(), self.caret));
            self.text = t;
            self.caret = c.min(self.text.len());
            self.anchor = None;
        }
    }

    fn redo_op(&mut self) {
        if let Some((t, c)) = self.redo.pop() {
            self.undo.push((self.text.clone(), self.caret));
            self.text = t;
            self.caret = c.min(self.text.len());
            self.anchor = None;
        }
    }

    /// Pop any text the last keystroke wants copied to the system clipboard.
    pub fn take_copy(&mut self) -> Option<String> {
        self.copy_out.take()
    }

    /// Move the caret to `pos`, extending the selection when `extend` (shift).
    fn move_caret(&mut self, pos: usize, extend: bool) {
        if extend {
            if self.anchor.is_none() {
                self.anchor = Some(self.caret);
            }
        } else {
            self.anchor = None;
        }
        self.caret = pos;
        self.typing = false;
    }

    /// The text with `glyph` inserted at the caret (caret-only rendering).
    pub fn display(&self, glyph: &str) -> String {
        let (a, b) = self.text.split_at(self.caret);
        format!("{a}{glyph}{b}")
    }

    /// Render the text with the selection highlighted (and caret when there's
    /// no selection). `glyph` is the blinking caret string.
    pub fn render(&self, glyph: &str, sel_bg: u32) -> Div {
        if let Some((s, e)) = self.selection() {
            div()
                .flex()
                .flex_row()
                .child(div().child(self.text[..s].to_string()))
                .child(div().bg(rgb(sel_bg)).child(self.text[s..e].to_string()))
                .child(div().child(self.text[e..].to_string()))
        } else {
            div().child(self.display(glyph))
        }
    }

    /// Apply a navigation/editing keystroke. `clip` supplies cmd+v text; `accept`
    /// gates which inserted strings are allowed (e.g. digits-only for goto).
    pub fn key(&mut self, ks: &Keystroke, clip: Option<String>, accept: impl Fn(&str) -> bool) -> Edit {
        let m = &ks.modifiers;
        let shift = m.shift;
        let key = ks.key.as_str();

        // ── cmd combos ───────────────────────────────────────────────────
        if m.platform && !m.alt && !m.control {
            match key {
                "a" => {
                    self.anchor = Some(0);
                    self.caret = self.text.len();
                    self.typing = false;
                    return Edit::Moved;
                }
                "c" | "x" => {
                    if let Some((s, e)) = self.selection() {
                        self.copy_out = Some(self.text[s..e].to_string());
                        if key == "x" {
                            self.push_undo();
                            self.delete_selection();
                            self.typing = false;
                            return Edit::Changed;
                        }
                    }
                    return Edit::Moved;
                }
                "z" => {
                    if shift {
                        self.redo_op();
                    } else {
                        self.undo_op();
                    }
                    self.typing = false;
                    return Edit::Changed;
                }
                _ => {}
            }
        }

        // ── caret movement (shift extends the selection) ──────────────────
        let moved = match key {
            "left" if m.alt => Some(word_left(&self.text, self.caret)),
            "right" if m.alt => Some(word_right(&self.text, self.caret)),
            "left" if m.platform => Some(0),
            "right" if m.platform => Some(self.text.len()),
            "left" => {
                if !shift {
                    if let Some((s, _)) = self.selection() {
                        self.move_caret(s, false);
                        return Edit::Moved;
                    }
                }
                Some(prev_boundary(&self.text, self.caret))
            }
            "right" => {
                if !shift {
                    if let Some((_, e)) = self.selection() {
                        self.move_caret(e, false);
                        return Edit::Moved;
                    }
                }
                Some(next_boundary(&self.text, self.caret))
            }
            "home" => Some(0),
            "end" => Some(self.text.len()),
            _ => None,
        };
        if let Some(pos) = moved {
            self.move_caret(pos, shift);
            return Edit::Moved;
        }

        // ── editing ───────────────────────────────────────────────────────
        match key {
            "backspace" => {
                self.push_undo();
                self.typing = false;
                if self.delete_selection() {
                    return Edit::Changed;
                }
                if m.alt {
                    let start = word_left(&self.text, self.caret);
                    self.text.replace_range(start..self.caret, "");
                    self.caret = start;
                } else if m.platform {
                    self.text.replace_range(0..self.caret, "");
                    self.caret = 0;
                } else if self.caret > 0 {
                    let prev = prev_boundary(&self.text, self.caret);
                    self.text.replace_range(prev..self.caret, "");
                    self.caret = prev;
                }
                Edit::Changed
            }
            "delete" => {
                self.push_undo();
                self.typing = false;
                if self.delete_selection() {
                    return Edit::Changed;
                }
                if self.caret < self.text.len() {
                    let next = next_boundary(&self.text, self.caret);
                    self.text.replace_range(self.caret..next, "");
                }
                Edit::Changed
            }
            "v" if m.platform => {
                if let Some(t) = clip {
                    let pasted = t.replace(['\n', '\r'], " ");
                    let pasted = pasted.trim();
                    if !pasted.is_empty() && accept(pasted) {
                        self.push_undo();
                        self.delete_selection();
                        self.insert(pasted);
                    }
                }
                self.typing = false;
                Edit::Changed
            }
            _ => {
                if !m.platform && !m.control && !m.alt {
                    if let Some(kc) = &ks.key_char {
                        if accept(kc) {
                            if !self.typing {
                                self.push_undo();
                                self.typing = true;
                            }
                            self.delete_selection();
                            self.insert(kc);
                            return Edit::Changed;
                        }
                    }
                }
                Edit::None
            }
        }
    }
}
