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
pub const TAB_INACTIVE: u32 = 0x1f2122;
pub const BORDER: u32 = 0x2b2d30; // subtle separators
pub const SPLIT_BORDER: u32 = 0x1e1f21;
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

// ── popups / autocomplete ──────────────────────────────────────────────────
pub const POPUP_BG: u32 = 0x2b2d30;
pub const POPUP_FG: u32 = 0xbcbec4;
pub const POPUP_SELECTED: u32 = 0x2e436e;

// ── git / diff ─────────────────────────────────────────────────────────────
pub const GIT_NEW: u32 = 0x6aaf6a; // added / untracked
pub const GIT_MODIFIED: u32 = 0x5b9bd5; // modified (WebStorm blue)
pub const GIT_DELETED: u32 = 0xc75450; // deleted
pub const DIFF_ADD_BG: u32 = 0x26331f;
pub const DIFF_REMOVE_BG: u32 = 0x3a2526;
pub const SEARCH_MATCH_BG: u32 = 0x4d4023;
pub const SEARCH_CURRENT_BG: u32 = 0x3a4768;
