# Agent status channel

TIDE runs a loopback HTTP server per project window. Anything running inside a
terminal pane can report what it is doing; TIDE shows it as a chip in that pane's
corner and a dot on the terminal tab.

## What a pane gets

Every pane shell is spawned with:

| var | value |
|-----|-------|
| `TIDE_RPC` | `http://127.0.0.1:<ephemeral port>` |
| `TIDE_TOKEN` | per-window token, required on every request |
| `TIDE_PANE` | this pane's id — pass it back so the report lands on the right pane |
| `TIDE_ROOT` | the project root the window was opened on |

Children inherit them, so Claude Code, its hooks, and any script started in that
pane all have what they need. Nothing has to guess which window or pane it is in.

## Endpoint

```
POST $TIDE_RPC/agent
X-Tide-Token: $TIDE_TOKEN

{"pane": "$TIDE_PANE", "state": "working|blocked|done|failed", "summary": "optional, ≤200 chars"}
```

Aliases accepted for `state`: `running`/`start` → working, `needs_input`/`waiting`/`question`
→ blocked, `finished`/`stop` → done, `error` → failed.

`GET $TIDE_RPC/health` needs no token — use it to check a cached port is still a
live TIDE.

Omit `pane` and the report lands on the active pane of the active tab. Only UI
state is written: the channel reads no files and runs nothing.

## Wiring Claude Code hooks

Hooks make the lifecycle states automatic — the model doesn't have to remember
to report. One reporter script, three hooks that call it.

`~/.claude/tide-report.sh` (chmod +x):

```sh
#!/bin/sh
# Usage: tide-report.sh working|blocked|done|failed [summary]
[ -n "$TIDE_RPC" ] || exit 0
curl -s -m 2 -X POST "$TIDE_RPC/agent" \
  -H "X-Tide-Token: $TIDE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"pane\":\"$TIDE_PANE\",\"state\":\"${1:-working}\",\"summary\":\"$2\"}" \
  >/dev/null 2>&1
exit 0
```

The `TIDE_RPC` guard keeps it silent outside TIDE; `exit 0` means a failed curl
can never block a turn.

Then in `~/.claude/settings.json` — **append** to the existing arrays for these
events, don't replace them:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "$HOME/.claude/tide-report.sh working" }] }
    ],
    "Notification": [
      { "hooks": [{ "type": "command", "command": "$HOME/.claude/tide-report.sh blocked" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "$HOME/.claude/tide-report.sh done" }] }
    ]
  }
}
```

Hook commands run through a shell, so `$HOME` expands. Claude Code re-reads
settings when `/hooks` is opened — until then a running session keeps its old
config.

## Telling TIDE to re-read git state

Opening a PR, pushing, or switching branches changes what TIDE's topbar should
say, and it has no way to notice until its next poll (branch changes) or a ⌘R
(PR link). One request fixes that:

```
POST $TIDE_RPC/refresh
X-Tide-Token: $TIDE_TOKEN
```

No body. TIDE re-queries the PR for the current branch, reloads the PR file
list, and re-checks admin health. Agents in the ACP pane are told about this
automatically: TIDE prepends a short briefing to the first prompt of each
session, which never shows in the transcript. The gear in the agent pane's
header shows that briefing verbatim, and copies it. A terminal agent needs the
same instructions in `CLAUDE.md`, next to the `/agent` ones.

The briefing opens by pointing at the repo's own rules. When `AGENTS.md` or
`CLAUDE.md` sits at the repo root, the agent is told to read it first and that
it outranks everything TIDE says. Neither file is mentioned when neither
exists.

## Naming the workspace

```
POST $TIDE_RPC/title
X-Tide-Token: $TIDE_TOKEN

{"title": "fix: sql date off by one"}
```

Lead the name with the type of work, `<type>: <name>`. The types are the ones the
title dialog offers (feat, fix, docs, style, refactor, perf, test, build, ci,
chore, bip, client, release), and TIDE replaces the prefix with that type's icon,
so an agent-named workspace reads the same as a hand-named one. A scope is
accepted and dropped (`fix(api): …`). A title with no recognized prefix is stored
as sent.

TIDE takes the name only when the workspace has none — a name the human typed is
never overwritten, so an agent can send one once without checking first. A named
workspace also stops being marked free.

## Marking PR files as viewed

```
POST $TIDE_RPC/viewed
X-Tide-Token: $TIDE_TOKEN

{"paths": ["packages/a/package.json", "packages/b/package.json"]}
```

The same checkbox the human ticks in the PR pane. Paths are repo-relative, the
way `git diff --name-only` prints them; absolute and `./`-prefixed paths work
too, and a single `"path": "..."` string is accepted. Add `"viewed": false` to
untick. TIDE applies it to the loaded PR and mirrors it onto GitHub, so the PR
page agrees, then says in a toast how many files matched. Paths the PR doesn't
touch are counted and reported, never guessed at.

This exists for requests like "mark every package.json that is only a version
bump as viewed": the agent works out the list, TIDE does the ticking. An agent
should not tick files off unasked.

## Answering review comments

TIDE's review comments are threads. When a batch is sent to a pane, each comment
carries a short id in brackets, and the message tells the agent how to answer:

```
POST $TIDE_RPC/comment
X-Tide-Token: $TIDE_TOKEN

{"id": "<id from the brackets>", "text": "what you did"}
```

The reply lands in that comment's thread in the review pane (labelled `agent`)
and is saved with the comment, so it survives a restart. Comments stay in the
list after sending — marked `sent`, then `answered` once a reply arrives —
instead of disappearing the moment they're handed over.

Threads are a conversation: a follow-up written in TIDE puts the comment back in
the send queue, and the next batch includes everything already said —

```
1. [t1] a.tsx:45
   | value: String(option.value),
   -> what about undefined?
   you (agent): != null covers both
   me: then why the extra guard?
```

so the next answer continues the discussion instead of restating the first one.
The human resolves the thread when they're satisfied; nothing auto-closes.

## Reporting on purpose

For anything the model decides to say (rather than a lifecycle event), tell it
the endpoint in `CLAUDE.md` and let it use Bash:

```
curl -s -X POST "$TIDE_RPC/agent" -H "X-Tide-Token: $TIDE_TOKEN" \
  -d "{\"pane\":\"$TIDE_PANE\",\"state\":\"blocked\",\"summary\":\"need the migration order\"}"
```

Allowlist `Bash(curl -s -X POST http://127.0.0.1:*)` to avoid a prompt each time.
If prose instructions turn out to be unreliable, the same route can be wrapped in
an MCP tool later — the contract doesn't change.

## In the UI

- Pane corner chip: colour + state word + summary. Click it to acknowledge (clears).
- Pane border takes the state colour while a state is set (focus still wins).
- Terminal tab dot: the most attention-hungry state across that tab's panes
  (blocked > failed > done > working), so a background tab can still call you.
