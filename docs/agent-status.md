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

Send it early: the agent is told to name the workspace in its first reply rather
than at the end of the work.

TIDE takes the name when the workspace has none, and when the current name was
written by an agent — so an agent can improve its own first guess as the work
becomes clearer, which is what makes naming early safe. A name the human typed is
theirs: TIDE ignores an agent's title then, so the call is always safe to make.

To rename a name the human chose, add `"force": true`. That is for a rename they
asked for, not something to send on your own initiative. A named workspace also
stops being marked free or paused.

## Working from a phone

The `phone` chip in the agent pane copies a link:
`http://<your-ip>:<port>/m?t=<token>`. Open it on a phone on the same wifi and
you get every open workspace, one tap apart, each with its transcript and a
composer. Anything typed there arrives as if it were typed in that workspace's
pane.

The listener starts with TIDE and stays up. It costs a bound socket: the only
real work, building a transcript, happens while a phone is actually polling, and
only for the workspace it is reading. The link carries the token, so treat it the
way you would any other key — anyone on that network who has it can type into
your sessions.

## Pull requests, and where work lives

Open the PR yourself when the work is worth one — a feature that holds together,
a fix with its tests, a round of changes worth reviewing. Don't stop to ask
first: a PR is cheaper to close than to wait for.

A PR body is about the code, not about who said what. No names, no "the client",
no "as discussed", nothing about who noticed or reported it. Say what was wrong,
what it affected and what changed: "Sven noticed the dropdown closes on scroll"
is "the dropdown closed on scroll". The conversation that led to the work is
private to it; the PR outlives it and is read by people who weren't there.

Pictures in a PR body live in the repo: commit them under
`.github/pr-assets/<pr number>/` and link the raw URL pinned to the commit that
added them, `raw.githubusercontent.com/<owner>/<repo>/<sha>/...`. A link through
the branch name dies when the branch is deleted after the merge, which is when
someone is most likely reading the PR again.

GitHub's own uploader is for comments. A body you rewrite loses its images every
time, and `gh` has no way to upload to one: that endpoint belongs to the web UI,
not the API.

Never use git worktrees. Each TIDE workspace is already its own checkout, and a
worktree inside one puts the work somewhere nothing in TIDE is looking — not the
file tree, not the PR pane, not the reviewers. Branch in place.

## Saying what you did

When TIDE hands over a job — review comments to address, a branch to bring up to
date, a conflict to sort out — finish by posting what happened:

```
POST $TIDE_RPC/summary
X-Tide-Token: $TIDE_TOKEN

{"title": "Merged origin/next",
 "text": "Resolved 3 conflicts in CommandPalette.tsx, kept both sides. yyb
clean, 24 tests pass."}
```

`title` is two to four words naming the job; it heads the card so a transcript
full of them can be scanned. Without one the card calls itself "Summary".

It renders as a card in the transcript, set apart from the turn around it, which
is where the human looks for "so what happened" after a long run of tool calls.
Markdown, with its line breaks kept — a short paragraph or a few bullets: what
changed, what was decided, anything still open.

Post it once, at the end, and don't repeat it in the reply. An empty summary is
a bad request rather than an empty card.

## Where the work stands

One workspace usually holds several stretches of work — the UI, then the backend,
then the syncing. The title is the whole of it and stays put; the subtitle says
which stretch is happening now.

```
POST $TIDE_RPC/title
X-Tide-Token: $TIDE_TOKEN

{"subtitle": "UI auto syncing"}
```

Two or three words, and no title in the same call — the two move independently. A
title alone leaves the subtitle where it was; an empty subtitle clears it, which
is what to send when a stretch is done and the next hasn't started.

Unlike the title, the subtitle is the agent's to keep current: there is no force
and no permission, because it describes what is happening rather than what the
work is called. Move it when the work moves — after a merge, when the next piece
is picked up. The human reads it in the topbar as `AI Chat (UI auto syncing)`,
which is how they tell at a glance where a workspace was left. `GET
$TIDE_RPC/workspace` reports it back under `subtitle`.

## Your own standing instructions

Anything in `~/.config/tide/agent-notes.md` is appended to the briefing verbatim,
so every new session gets it. That file is the place for things that are true of
your machine rather than of TIDE: local sign-in details, an API key for a local
instance, house conventions you repeat often. It sits outside the repo, so
nothing there can end up in a commit, and editing it needs no rebuild — the gear
in the agent pane shows the result.

## Showing what you saw in a browser

```
POST $TIDE_RPC/shot
X-Tide-Token: $TIDE_TOKEN

{"path": "/tmp/shot.png", "note": "admin after saving the flag"}
```

TIDE reads the file and puts the picture in the transcript, with the note above
it. A relative path is relative to the workspace. PNG, JPEG, GIF and WebP, up to
12 MB.

Pair it with `agent-browser` (installed; `agent-browser skills get core` is its
own guide): open the page, click through it, `agent-browser screenshot
/tmp/shot.png`, then post it. Use it when you checked something in a browser and
the human would otherwise have to take your word for it. `agent-browser console`
and `agent-browser errors` are worth reading before claiming a page works.

## Filing something on the todo list

```
POST $TIDE_RPC/todo
X-Tide-Token: $TIDE_TOKEN

{"todos": [{"text": "drop the WCP shim",
            "details": "GetFeatureFlagsWithLicense only needs it for the 6.4 backport;\nonce that ships the shim is dead weight.",
            "category": "refactor", "scope": "api"},
           "chase the flaky test"],
 "group": "license gated flags"}
```

For "note that down", "park it", "backlog this". `text` is the one-line version and the only required
field; a bare string in the array is just the text. `details` is the long half:
the reasoning, what was decided, what to watch out for. `category` is one of the type
words the workspace title uses (feat, fix, docs, …), `status` is todo, doing or
done, and `link`, `scope` and `client` are optional. One call files the whole batch.

The same path reads the list back:

```
GET $TIDE_RPC/todo
X-Tide-Token: $TIDE_TOKEN

{"todos": [{"id": "…", "text": "…", "details": "…", "status": "todo", …}]}
```

That is the whole list, every workspace's, straight off disk, so it is current
even while the UI has it open. Use it to check what was filed, or to answer
"what's on my list".

## Changing one

The same POST edits, when the item carries the `id` that the read gave you:

```
POST $TIDE_RPC/todo
X-Tide-Token: $TIDE_TOKEN

{"todos": [{"id": "58w65", "status": "done"},
           {"id": "eg3m7", "delete": true}]}
```

Only the fields the request mentions are touched, so marking something done
needn't repeat its text, and leaving `details` out keeps the details. `delete`
takes the line off the list.

Without an `id` a post always files a new task — that is the one difference
between filing and editing, and an edit that forgets the id becomes a duplicate
instead. An `id` TIDE doesn't recognise is dropped rather than filed as
something new; the human sees how many were ignored. A batch's top-level `group`
applies to the tasks it files, not to the ones it edits, which keep the group
they have unless the item names a different one.

`group` keeps a batch together under one heading in TIDE's list. Send one, or let
TIDE fall back to the workspace's title. Don't send the workspace itself: TIDE
records the folder each task was filed from, so a line can always be traced back
to the checkout it came out of. That's provenance, not ownership. The list is one
list, shared by every window.

## Which workspace is this?

```
GET $TIDE_RPC/workspace
X-Tide-Token: $TIDE_TOKEN

{"folder": "wby-next4", "title": "Event Van Handler",
 "root": "/Users/adrian/dev/wby-next4", "branch": "adrian/task-definition-split",
 "base": "next"}
```

The folder is the checkout on disk; the title is what the human (or `/title`)
named the workspace, and it can change while a session runs, so read this at the
moment you need it. `base` is the branch this one merges into (the PR's base, or
the reflog's guess), which is what a local review diffs against. Agents are told to put a `Workspace: <folder> · <title>` line
in every PR body they open, which is what traces a PR back to the checkout it was
written in.

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
