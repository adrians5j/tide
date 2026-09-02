# Plan usage bars

The topbar chip shows how much of your Claude.ai plan the current session has
spent: the 5-hour window and the weekly one, each with the time it resets. Same
numbers `/usage` prints, read the cheap way.

## Where the numbers come from

Claude Code pipes a JSON payload to whatever `statusLine` command you configure,
and for Claude.ai subscriptions that payload carries the plan windows:

```json
{
  "rate_limits": {
    "five_hour": { "used_percentage": 54, "resets_at": 1738425600 },
    "seven_day": { "used_percentage": 69, "resets_at": 1738857600 }
  }
}
```

`resets_at` is Unix epoch seconds. Both windows are independently optional: they
appear only for Pro/Max sessions, and only after the session's first API
response. TIDE treats a missing window as *unknown*, never as 0%.

## Wiring it up

Run **Install Plan Usage Bars** from the command palette. It writes
`~/.claude/tide-statusline.sh` and points `statusLine` in
`~/.claude/settings.json` at it, passing your previous status command through as
an argument — so an existing status line keeps working:

```json
"statusLine": {
  "type": "command",
  "command": "\"$HOME/.claude/tide-statusline.sh\" bash \"…/caveman-statusline.sh\""
}
```

The shim keeps a copy of the payload and hands it on:

```sh
input=$(cat)
printf '%s' "$input" > "$dir/usage.json.tmp" && mv "$dir/usage.json.tmp" "$dir/usage.json"
[ "$#" -gt 0 ] && printf '%s' "$input" | "$@"
```

The write is atomic (temp + rename), so TIDE never reads a half-written file.
Restart Claude Code afterwards — a running session keeps its old settings.

## In the UI

TIDE re-reads `~/.config/tide/usage.json` every 15s and only repaints when the
numbers move. The chip shows the *worse* of the two windows — green under 60%,
amber past it, red from 90% — and clicking it opens both bars with their reset
times. No chip means nothing has reported yet: the shim isn't installed, Claude
Code hasn't restarted, or the session is on an API key rather than a plan.

Every window in the file is global to your account, not per project — the same
numbers show in every TIDE window.
