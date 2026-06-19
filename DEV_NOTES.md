# Dev notes

Architecture, conventions, and the non-obvious decisions behind `tide`. Read
this before touching the editor/terminal rendering — several things look wrong
but are deliberate workarounds for GPUI behavior.

## Layout

Single binary, a handful of modules under `src/`:

| File | What lives here |
|------|-----------------|
| `main.rs` | The app shell. `Storm` is the per-project view (tree, tabs, git, PR, terminal docks, all the dialogs/overlays); `Workspace` holds multiple `Storm`s and switches between them. Most chrome + git/PR/find/commit logic is here. |
| `editor.rs` | The text editor: `Editor` entity + `EditorElement` (custom GPUI `Element` doing layout/paint), selection, undo, LSP wiring, search, completion, hover. |
| `term.rs` | Terminal: `Terminal` entity + `TerminalElement`. Wraps `alacritty_terminal` over a `portable-pty`. Handles the grid → quads/glyphs render, mouse/selection, scrollback. |
| `lsp.rs` | Minimal LSP client (spawns a language server, did_open/did_change/definition/hover/completion). |
| `diff.rs` | Diff model (via `similar`) for the diff window. |
| `field.rs` | `Field`: a shared single-line text input (caret, selection, undo, clipboard) used by every chrome input — finder, goto, filter bars, commit message, etc. |
| `syntax.rs` | Syntax highlighting (`syntect` + `two-face`). |
| `theme.rs` | Colors + icon-font glyph constants. |

> Naming: the core type is still `Storm` (the project's original name). The repo/
> binary are `tide`; the type name just never got renamed and it isn't worth the
> churn. Treat `Storm` == "the app".

## GPUI gotchas (the important part)

These bit us and the fixes are easy to "simplify" back into bugs:

- **Drag-select uses *window-level* mouse listeners, not element `on_mouse_move`.**
  An element's `on_mouse_move` is hover-phase only — it stops firing the moment a
  button is held, so a press-and-drag never extends a selection. Both the editor
  and terminal register move/up handlers via `window.on_mouse_event(...)` inside
  their element `paint()` (re-registered every frame). This is also why panel
  resizing is handled on the *root* div, not on the tiny divider.

- **A `cx.notify()` from inside a drag's event dispatch doesn't repaint until the
  gesture ends.** During a drag we call `window.refresh()` to force the frame so
  the selection highlight tracks live. (Async-task `notify()`s — cursor blink,
  the terminal repaint pump — *do* paint live, which is why those work.)

- **Custom elements cache shaped lines.** `EditorElement`/`TerminalElement` shape
  text in `prepaint` and cache by content hash to avoid re-shaping unchanged rows
  every frame (matters when a TUI floods the terminal). Background quads (incl.
  selection highlight, terminal scrollbar) are rebuilt each frame.

- **Nested focus + bubbling key handlers double-fire.** The PR pane's filter input
  is nested inside the pane's own key-handled container, so a keystroke hit both
  handlers and inserted twice. Pattern: a parent key handler must bail when a
  nested input owns focus (`if self.x_filter_focus.is_focused(window) { return }`).

## Files: no VFS

There is **no virtual filesystem**. Deliberately the inverse trade-off from
IntelliJ/WebStorm (which mirror the project in RAM and flush async):

- Files are read lazily, **one `String` buffer per open tab** (`Editor::load`),
  and written **straight to disk** (`Editor::save`) — no background flush.
- The tree walks the **real FS on demand** (`fs::read_dir`, expanded dirs only).
- **No file watcher.** External changes (Claude Code, git, etc.) are detected by
  a **2 s mtime poll** (`start_git_poll`) plus a check on tab switch / reopen.
- **`save()` only writes when the buffer is dirty.** Critical: auto-save fires on
  every tab switch/focus change, so saving a *clean* buffer would rewrite the file
  with our stale copy and **clobber external edits**. Don't remove that guard.
- Conflict handling: clean buffer + on-disk change → silent reload; **dirty**
  buffer + on-disk change → a per-tab banner (Reload / Keep mine), never a silent
  clobber.

## Terminal notes

- PTY columns are resized to fit the pane width, so lines wrap — there's no
  horizontal overflow (hence no horizontal scrollbar). Scrollback is the alacritty
  default (10k lines); the wheel scrolls it when no mouse-reporting app is active,
  otherwise the wheel is forwarded to the app.
- `Option`+`Left`/`Right` emit `ESC b` / `ESC f` (word motion), matching Ghostty —
  not the xterm modified-arrow form, which shells don't bind to word motion.
- Powerline/Nerd glyphs (zellij status bar, etc.) need a Nerd Font; the font is
  `JetBrainsMono Nerd Font` (see README). Menlo renders them as tofu.

## Git / PR

All git and GitHub interaction is shelling out — `git status --porcelain` (polled
every 2 s) and the `gh` CLI (`gh pr view`, `gh pr create`, etc.). "Create PR" runs
the user's `pr` zsh function (via `zsh -ic`, so aliases/functions resolve). The
title-bar PR link is refetched only when the branch changes (and on ⌘R) to avoid
hitting the network every tick.

## Build

- `Cargo.toml` pins **Zed's patched forks** of `async-process` / `async-task` —
  required for `gpui_macos` to build. GPUI itself tracks the Zed `main` branch, so
  builds can break when Zed changes; pin a rev if it gets noisy.
- Dev profile compiles **dependencies at `opt-level = 3`** (our crate stays
  unoptimized for fast incremental builds). Font shaping, the VT parser, and
  syntect are hot paths — at opt-level 0 the editor/terminal feel laggy.

## Conventions

- Chrome inputs all go through `Field` so they behave identically (arrows,
  opt/cmd-arrows, selection, clipboard, undo).
- Overlays/dialogs are added as absolutely-positioned children of the root in
  `Storm::render`, gated on a `*_open` flag, with their own focus handle +
  key handler; `escape` closes via `on_root_key` and/or the dialog's handler.
- Colors and icon glyphs come from `theme.rs` — don't hardcode hex unless it's a
  one-off (e.g. a translucent scrollbar).
