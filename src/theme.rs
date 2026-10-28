//! Centralized theme tokens — WebStorm-style (JetBrains new UI dark).
//! All UI/editor/syntax/git colors live here; call sites reference tokens only.
//! (Syntax highlighting itself comes from the `TwoDark` syntect theme.)

// ── core editor ────────────────────────────────────────────────────────────
pub const BG: u32 = 0x18191a; // editor pane background
pub const TEXT: u32 = 0xbcbec4; // editor.foreground
pub const LINE_NUMBER: u32 = 0x4b4d51;
pub const SELECTION: u32 = 0x2e436e; // selection.background
pub const CURRENT_LINE: u32 = 0x1f2122; // cursorLine.background
pub const CURSOR: u32 = 0xced0d6;

// ── UI chrome (sidebar, panels, tabs, bars) ────────────────────────────────
pub const PANEL_BG: u32 = 0x222426; // sidebar / top + bottom bar / tab bar
pub const BORDER: u32 = 0x2b2d30; // subtle separators
/// A rule drawn inside a surface: the dividers in the top bar, in a menu,
/// between the halves of a diff. Brighter than BORDER, which edges a panel
/// and reads fine at low contrast because the two sides differ in colour.
pub const SEPARATOR: u32 = 0x3c3f43;
pub const MUTED: u32 = 0x7a7e85; // muted text
pub const ICON: u32 = 0xa9adb5; // sidebar/activity icons (resting + active)
pub const DIR: u32 = 0xbcbec4; // folder names render as default text
pub const FOLDER_ICON: u32 = 0xc9a26a; // warm tan folder glyph
pub const HOVER: u32 = 0x2b2e31;

// ── accents / selection ────────────────────────────────────────────────────
pub const ACCENT: u32 = 0x3574f0; // JetBrains blue (cursor-line gutter, active icon, dividers, dialog border)
pub const ICON_SELECTED_BG: u32 = 0x2f68ee; // background of a selected sidebar icon
pub const SELECTED_BG: u32 = 0x2e436e; // selected list row background
pub const SEL_TEXT: u32 = 0xffffff; // text on a selected row
/// Your own messages in the agent transcript. Darker than SELECTION on purpose:
/// selecting text inside one has to stay visible, and SELECTION is what paints
/// it.
pub const USER_BUBBLE: u32 = 0x233350;

// ── popups / autocomplete ──────────────────────────────────────────────────
pub const POPUP_BG: u32 = 0x2b2d30;
pub const POPUP_FG: u32 = 0xbcbec4;

// ── git / diff ─────────────────────────────────────────────────────────────
pub const GIT_NEW: u32 = 0x6aaf6a; // added / untracked
pub const GIT_MODIFIED: u32 = 0x5b9bd5; // modified (WebStorm blue)
pub const GIT_DELETED: u32 = 0xc75450; // deleted
pub const DIFF_ADD_BG: u32 = 0x26331f;
pub const DIFF_REMOVE_BG: u32 = 0x3a2526;
pub const SEARCH_MATCH_BG: u32 = 0x4d4023;
pub const SEARCH_CURRENT_BG: u32 = 0x3a4768;

// ── review comments (gutter marker, inline notes, boxes, counts) ───────────
// Purple: amber means "agent needs you", green is added/passing, blue is the UI
// accent, and the free-workspace badge moved to cyan so this hue is unshared.
/// 7% cyan wash behind a free workspace's row in the switcher: enough to pick
/// the row out while scanning a list of a dozen, not enough to compete with the
/// selected row or with the FREE badge itself.
pub const FREE_TINT: u32 = 0x56c8d812;

pub const COMMENT_MARK: u32 = 0xbb9af7;
pub const COMMENT_TINT: u32 = 0xbb9af722; // 13% wash behind inline notes
pub const COMMENT_TINT_HOVER: u32 = 0xbb9af733;

// ── same-word occurrence highlight (selecting a word marks its twins) ────────
pub const WORD_MATCH_BG: u32 = 0x33414d;

// ── code tour (a walk the agent prepared, stepped through like a debugger) ──
/// The stop you are standing on: a bar down the gutter and the line numbers.
/// Amber, because the two hues already spoken for in the editor are review
/// purple and diff green/red, and a tour is read against a diff often enough
/// that it can't borrow either.
pub const TOUR_MARK: u32 = 0xe0af68;
/// 11% wash across the stop's lines — enough to find after a jump, faint
/// enough to read code through.
pub const TOUR_TINT: u32 = 0xe0af681c;
