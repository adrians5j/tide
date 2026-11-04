#!/bin/sh
# Keep the build cache from eating the disk.
#
# `cargo build` throws nothing away. Every edit leaves another set of object
# files under target/debug/deps beside the ones before it, and after a few
# weeks of that the folder held 218,623 files and 70 GB. This drops what the
# current build no longer needs and keeps the rest, so the next build is still
# incremental rather than starting from nothing.
#
#   sh scripts/prune-build-cache.sh            # what the weekly job runs
#   DAYS=3 sh scripts/prune-build-cache.sh     # drop anything older than 3 days
#   CAP_GB=10 sh scripts/prune-build-cache.sh  # hold it under 10 GB
#
# Needs cargo-sweep (`cargo install cargo-sweep`), which knows cargo's layout
# well enough to take each artifact's fingerprint with it. Without that the
# only safe move is a full clean, and a surprise ten-minute rebuild is its own
# kind of annoying, so the job says so in the log and leaves the folder alone.
#
# The weekly run is a launchd job, com.tide.prune-build-cache, in
# ~/Library/LaunchAgents. Its log is ~/.config/tide/prune-build-cache.log.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
days=${DAYS:-7}
cap_gb=${CAP_GB:-25}
stamp=$(date '+%Y-%m-%d %H:%M')

# apparent size, which is what cargo and the Finder both report. `du` reads
# allocated blocks and came out at a third of the real figure on this disk.
size_gb() {
    find "$repo/target" -type f -exec stat -f '%z' {} + 2>/dev/null |
        awk '{ s += $1 } END { printf "%.1f", s / 1073741824 }'
}

[ -d "$repo/target" ] || { echo "$stamp  nothing to prune"; exit 0; }

# a build in flight holds this; pruning under one is a race
lock="$repo/target/debug/deps/.cargo-lock"
if [ -e "$lock" ] && /usr/sbin/lsof "$lock" >/dev/null 2>&1; then
    echo "$stamp  a build is running, left alone"
    exit 0
fi

if ! command -v cargo-sweep >/dev/null 2>&1; then
    echo "$stamp  cargo-sweep is not installed — nothing pruned ($(size_gb) GB)"
    exit 0
fi

before=$(size_gb)
cargo sweep --time "$days" "$repo" >/dev/null 2>&1 || true
# and a ceiling, for a week that built more than most
cargo sweep --maxsize "${cap_gb}GB" "$repo" >/dev/null 2>&1 || true
echo "$stamp  $(printf '%s' "$before") GB -> $(size_gb) GB"
