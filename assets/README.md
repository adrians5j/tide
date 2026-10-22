# Icon

`tide-icon.svg` is the source: a line of text that turned out to be water, with
the caret still sitting at the end of it.

The PNGs beside it are rendered from it (16 … 1024). `tide-512.png` is compiled
into the binary and set as the dock icon at launch — run from a terminal, a Rust
binary has no bundle and would otherwise show the generic one. The 16/32/48/128
copies are also in `extension/icons/` for the Chrome extension.

To change the mark: edit the SVG, then rasterise it at those sizes (any
converter will do; the ones here were drawn into a canvas in a browser and saved
out as PNG).

## The app bundle

`scripts/make-app.sh` wraps the release binary in `~/Applications/TIDE.app`,
with `TIDE.icns` built from these PNGs. That is what makes TIDE findable from
Raycast, Spotlight and the app switcher, and what gives it its name in the menu
bar.

The bundle holds a copy of the binary, so run the script again after a build you
want the launcher to pick up. Its executable is a two-line shell script that
runs the binary through a login shell: a GUI launch otherwise inherits a bare
PATH, and TIDE shells out to git, gh, npx and agent-browser.
