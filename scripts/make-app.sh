#!/bin/sh
# Wrap the binary in a TIDE.app so Raycast, Spotlight, ⌘Tab and the Dock can all
# find it. Run it after a build you want the launcher to use:
#
#   sh scripts/make-app.sh            # release build, into ~/Applications
#   DEST=/Applications sh scripts/make-app.sh
#
# The bundle holds a copy of the binary, so rebuilding TIDE does not change what
# the launcher runs until this is run again.
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
dest=${DEST:-$HOME/Applications}
app="$dest/TIDE.app"
bin="$here/target/release/tide"

echo "building…"
(cd "$here" && cargo build --release >/dev/null)
[ -x "$bin" ] || { echo "no binary at $bin" >&2; exit 1; }

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/tide-bin"

# The launcher runs the binary through a login shell. A GUI launch inherits a
# bare PATH (/usr/bin:/bin:/usr/sbin:/sbin), and TIDE shells out to git, gh,
# npx and agent-browser — none of which are on it. `exec` keeps the process
# inside the bundle, so the icon and the name stay TIDE's.
cat > "$app/Contents/MacOS/TIDE" <<'LAUNCH'
#!/bin/sh
exec /bin/zsh -lc 'exec "$0" "$@"' "$(dirname "$0")/tide-bin" "$@"
LAUNCH
chmod +x "$app/Contents/MacOS/TIDE"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>TIDE</string>
  <key>CFBundleDisplayName</key><string>TIDE</string>
  <key>CFBundleExecutable</key><string>TIDE</string>
  <key>CFBundleIdentifier</key><string>dev.adrian.tide</string>
  <key>CFBundleIconFile</key><string>TIDE</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# the icon, from the same PNGs the extension and the dock use
set=$(mktemp -d)/TIDE.iconset
mkdir -p "$set"
cp "$here/assets/tide-16.png"   "$set/icon_16x16.png"
cp "$here/assets/tide-32.png"   "$set/icon_16x16@2x.png"
cp "$here/assets/tide-32.png"   "$set/icon_32x32.png"
cp "$here/assets/tide-64.png"   "$set/icon_32x32@2x.png"
cp "$here/assets/tide-128.png"  "$set/icon_128x128.png"
cp "$here/assets/tide-256.png"  "$set/icon_128x128@2x.png"
cp "$here/assets/tide-256.png"  "$set/icon_256x256.png"
cp "$here/assets/tide-512.png"  "$set/icon_256x256@2x.png"
cp "$here/assets/tide-512.png"  "$set/icon_512x512.png"
cp "$here/assets/tide-1024.png" "$set/icon_512x512@2x.png"
iconutil -c icns "$set" -o "$app/Contents/Resources/TIDE.icns"

# tell LaunchServices it exists, so Spotlight (and Raycast) see it now rather
# than whenever it next gets round to the folder
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
  -f "$app" 2>/dev/null || true
touch "$app"

echo "built $app"
