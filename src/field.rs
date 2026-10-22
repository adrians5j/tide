//! Shared single-line text-input buffer with caret, selection, undo/redo and
//! the standard editing keys. Used by every chrome input and the editor search.
use gpui::{
    fill, prelude::*, px, rgb, AnyElement, App, Bounds, DispatchPhase, ElementId, GlobalElementId,
    HighlightStyle, InspectorElementId, Keystroke, LayoutId, MouseButton, MouseDownEvent, Pixels,
    StyledText, Window,
};
use std::rc::Rc;

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
    /// When set, Enter inserts a newline and pasted newlines survive — for a
    /// comment body rather than a one-line filter. The submit key is the
    /// caller's business (⌘↩ where this is used).
    pub multiline: bool,
    pub text: String,
    pub caret: usize,
    anchor: Option<usize>,        // selection anchor; selection spans anchor..caret
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    typing: bool,                 // coalesce a run of typed chars into one undo step
    copy_out: Option<String>,     // text the caller should put on the clipboard
}

impl Field {
    /// A field whose ⇧↩/⌘↩ insert line breaks and whose pastes keep newlines.
    pub fn multiline() -> Self {
        Self { multiline: true, ..Self::default() }
    }

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

    /// Move the caret (clearing any selection) — for a host that treats some run
    /// of text as one unit, like the composer's `[Image #1]` tokens.
    pub fn move_to(&mut self, pos: usize) {
        let mut p = pos.min(self.text.len());
        while p > 0 && !self.text.is_char_boundary(p) {
            p -= 1;
        }
        self.move_caret(p, false);
    }

    /// Replace `range` with `with`, caret after it. One undo step.
    pub fn splice(&mut self, range: std::ops::Range<usize>, with: &str) {
        let (s, e) = (range.start.min(self.text.len()), range.end.min(self.text.len()));
        if s > e || !self.text.is_char_boundary(s) || !self.text.is_char_boundary(e) {
            return;
        }
        self.push_undo();
        self.text.replace_range(s..e, with);
        self.caret = s + with.len();
        self.anchor = None;
        self.typing = false;
    }

    /// Insert text at the caret, replacing any selection. This is the path
    /// dictation and IME take: macOS delivers those through `insertText:`, not as
    /// key events, so they never reach `key`.
    pub fn insert_text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let s = if self.multiline { s.replace('\r', "\n") } else { s.replace(['\n', '\r'], " ") };
        self.push_undo();
        self.delete_selection();
        self.insert(&s);
        self.typing = false;
    }

    /// Insert a line break at the caret (a host binds its own key to this —
    /// the agent box uses ctrl+j, since ↩ sends).
    pub fn newline(&mut self) {
        self.push_undo();
        self.delete_selection();
        self.insert("\n");
        self.typing = false;
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
            let c = self.caret_safe();
            let mut a = a.min(self.text.len());
            while a > 0 && !self.text.is_char_boundary(a) {
                a -= 1;
            }
            (a != c).then(|| (a.min(c), a.max(c)))
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

    /// Caret snapped into the text and onto a char boundary. Every index that
    /// reaches a slice goes through here: a stray mid-character caret (an IME
    /// commit, a host that hands us byte offsets) must degrade, never panic.
    pub fn caret_safe(&self) -> usize {
        let mut i = self.caret.min(self.text.len());
        while i > 0 && !self.text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    /// One line drawn twice: the real string in flow, which is what decides the
    /// layout, and on top of it the same string with the caret glyph spliced in
    /// and every other character painted transparent. The caret lands exactly
    /// where the text puts it while taking no width of its own, so moving it
    /// can't push the rest of the line sideways.
    /// Render the text, with the selection highlighted and a caret at the
    /// insertion point. `glyph` is the blinking caret string; blank means hidden
    /// (an unfocused field, or the off half of the blink).
    pub fn render(&self, glyph: &str, sel_bg: u32) -> AnyElement {
        // an empty buffer still needs a line box, or the row collapses to nothing
        let base = if self.text.is_empty() { " ".to_string() } else { self.text.clone() };
        let hl: Vec<(std::ops::Range<usize>, HighlightStyle)> = self
            .selection()
            .map(|(s, e)| {
                (s..e, HighlightStyle { background_color: Some(rgb(sel_bg).into()), ..Default::default() })
            })
            .into_iter()
            .collect();
        CaretText {
            text: StyledText::new(base).with_highlights(hl),
            caret: (!glyph.trim().is_empty()).then(|| self.caret_safe()),
            on_click: None,
        }
        .into_any_element()
    }

    /// The same field, but clicking in the text puts the caret where you
    /// clicked. `place` is handed the byte offset; it is the caller's because
    /// only the caller knows which field this is and how to redraw.
    pub fn render_clickable(
        &self,
        glyph: &str,
        sel_bg: u32,
        place: impl Fn(usize, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let base = if self.text.is_empty() { " ".to_string() } else { self.text.clone() };
        let hl: Vec<(std::ops::Range<usize>, HighlightStyle)> = self
            .selection()
            .map(|(s, e)| {
                (s..e, HighlightStyle { background_color: Some(rgb(sel_bg).into()), ..Default::default() })
            })
            .into_iter()
            .collect();
        CaretText {
            text: StyledText::new(base).with_highlights(hl),
            caret: (!glyph.trim().is_empty()).then(|| self.caret_safe()),
            on_click: Some(Rc::new(place)),
        }
        .into_any_element()
    }

    /// Where the caret lands moving one line `down` (or up). The column is
    /// counted in characters, not bytes, so it survives anything non-ASCII on
    /// the way past.
    fn vertical(&self, down: bool) -> usize {
        let at = self.caret_safe();
        let line_start = self.text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let column = self.text[line_start..at].chars().count();
        if down {
            let Some(rel) = self.text[at..].find('\n') else {
                return self.text.len(); // last line already
            };
            let next = at + rel + 1;
            let end = self.text[next..].find('\n').map(|i| next + i).unwrap_or(self.text.len());
            step_chars(&self.text, next, end, column)
        } else {
            if line_start == 0 {
                return 0; // first line already
            }
            let prev = self.text[..line_start - 1].rfind('\n').map(|i| i + 1).unwrap_or(0);
            step_chars(&self.text, prev, line_start - 1, column)
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
            // ⌘←/⌘→: line start/end in a text area, buffer start/end in a
            // one-line field (where they're the same thing). ⇧ extends, since
            // every motion here runs through `move_caret`.
            "left" if m.platform => Some(if self.multiline {
                self.text[..self.caret_safe()].rfind('\n').map(|i| i + 1).unwrap_or(0)
            } else {
                0
            }),
            "right" if m.platform => Some(if self.multiline {
                let at = self.caret_safe();
                self.text[at..].find('\n').map(|i| at + i).unwrap_or_else(|| self.text.len())
            } else {
                self.text.len()
            }),
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
            // ↑/↓ keep the column and change the line. On the first line ↑ goes
            // to the start and on the last ↓ goes to the end, which is what a
            // one-line field wants too — there, they simply are home and end.
            "up" => Some(self.vertical(false)),
            "down" => Some(self.vertical(true)),
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
            // ↩ adds the line break; ⌘↩ is the form's submit key
            "enter" if self.multiline && !m.platform && !m.control => {
                self.push_undo();
                self.delete_selection();
                self.insert("\n");
                self.typing = false;
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
                    let pasted =
                        if self.multiline { t.replace('\r', "\n") } else { t.replace(['\n', '\r'], " ") };
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

/// Text with a caret drawn at a byte offset. gpui lays the text out (wrapping,
/// line breaks, selection backgrounds and all); the caret is then painted as a
/// thin quad at the position that layout reports for the offset. It therefore
/// takes no width of its own, which is the whole point: a caret spliced into the
/// string as a glyph shoves every character after it sideways on every keypress.
struct CaretText {
    text: StyledText,
    caret: Option<usize>,
    /// Told where in the string a click landed. The field itself can't hear the
    /// mouse — it isn't an entity — so whoever renders it says what to do with
    /// the offset.
    on_click: Option<Rc<dyn Fn(usize, &mut Window, &mut App)>>,
}

impl Element for CaretText {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        self.text.request_layout(id, inspector, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text.prepaint(id, inspector, bounds, &mut (), window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let color = window.text_style().color;
        self.text.paint(id, inspector, bounds, &mut (), &mut (), window, cx);
        // a press inside the text moves the caret there. `index_for_position`
        // is the same layout the caret is drawn from, so the two can't disagree
        // about where a character is.
        if let Some(place) = self.on_click.clone() {
            let layout = self.text.layout().clone();
            window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || ev.button != MouseButton::Left
                    || !bounds.contains(&ev.position)
                {
                    return;
                }
                // past the end of a line, `index_for_position` reports the
                // nearest offset instead of nothing, which is what clicking in
                // the empty space to the right should do anyway
                let ix = match layout.index_for_position(ev.position) {
                    Ok(ix) => ix,
                    Err(ix) => ix,
                };
                place(ix, window, cx);
            });
        }
        let Some(ix) = self.caret else { return };
        let layout = self.text.layout();
        if let Some(origin) = layout.position_for_index(ix) {
            window.paint_quad(fill(
                Bounds::new(origin, gpui::size(px(1.5), layout.line_height())),
                color,
            ));
        }
    }
}

impl IntoElement for CaretText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// `column` characters into `start..end`, clamped to the end of that line — so
/// moving from a long line onto a short one lands at the short line's end
/// rather than overshooting into the next.
fn step_chars(text: &str, start: usize, end: usize, column: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(column)
        .map(|(i, _)| start + i)
        .unwrap_or(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_and_down_keep_the_column() {
        let mut f = field("alpha\nbravo\ncharlie", 0);
        f.caret = 8; // "br|avo", column 2
        assert_eq!(f.vertical(false), 2, "same column on the line above");
        assert_eq!(f.vertical(true), 14, "and on the line below");
    }

    #[test]
    fn a_short_line_takes_the_caret_to_its_end() {
        // column 6 on a line that is only 2 long lands at its end, not past it
        let mut f = field("alpha!\nno\nomega!", 0);
        f.caret = 6; // end of "alpha!"
        assert_eq!(f.vertical(true), 9, "the end of \"no\", not into \"omega\"");
        f.caret = 16; // end of "omega!"
        assert_eq!(f.vertical(false), 9);
    }

    #[test]
    fn the_first_and_last_lines_go_to_the_ends() {
        let mut f = field("alpha\nbravo", 0);
        f.caret = 3;
        assert_eq!(f.vertical(false), 0, "↑ on the first line is home");
        f.caret = 8;
        assert_eq!(f.vertical(true), 11, "↓ on the last line is end");
        // a single-line field has nowhere to go but the ends
        let mut one = field("just this", 4);
        one.multiline = false;
        assert_eq!(one.vertical(false), 0);
        assert_eq!(one.vertical(true), 9);
    }

    #[test]
    fn a_multibyte_line_counts_characters_not_bytes() {
        let mut f = field("héllo wörld\nsecond line", 0);
        f.caret = "héllo wö".len(); // column 8 in characters
        let landed = f.vertical(true);
        assert!(f.text.is_char_boundary(landed), "never mid-character");
        // "héllo wö" is 8 characters, so it lands 8 characters into the next
        // line — the 'i' of "line", not the byte at offset 8
        assert_eq!(&f.text[landed..landed + 1], "i");
    }

    fn field(text: &str, caret: usize) -> Field {
        let mut f = Field::multiline();
        f.set(text.to_string());
        f.caret = caret;
        f
    }

    fn press(f: &mut Field, key: &str, platform: bool, shift: bool) {
        let ks = Keystroke {
            modifiers: gpui::Modifiers { platform, shift, ..Default::default() },
            key: key.into(),
            key_char: None,
        };
        f.key(&ks, None, |_| true);
    }

    /// ⌘←/⌘→ work on the caret's own line, not the whole buffer — a text area
    /// with three paragraphs would otherwise jump to the very top or bottom.
    #[test]
    fn cmd_arrows_go_to_the_line_edges() {
        let mut f = field("one\ntwo\nthree", 5); // inside "two"
        press(&mut f, "left", true, false);
        assert_eq!(f.caret, 4, "start of the middle line");
        press(&mut f, "right", true, false);
        assert_eq!(f.caret, 7, "end of the middle line");
    }

    /// ⇧ turns the same motions into a selection.
    #[test]
    fn shift_cmd_arrows_select_to_the_line_edge() {
        let mut f = field("one\ntwo\nthree", 5);
        press(&mut f, "right", true, true);
        assert_eq!(f.selection(), Some((5, 7)));
        let mut f = field("one\ntwo\nthree", 5);
        press(&mut f, "left", true, true);
        assert_eq!(f.selection(), Some((4, 5)));
    }

    /// A one-line field keeps buffer start/end.
    #[test]
    fn a_single_line_field_still_jumps_to_the_ends() {
        let mut f = Field::default();
        f.set("hello".into());
        f.caret = 2;
        press(&mut f, "right", true, false);
        assert_eq!(f.caret, 5);
        press(&mut f, "left", true, false);
        assert_eq!(f.caret, 0);
    }
}
