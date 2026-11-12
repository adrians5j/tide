# Where TIDE's own tokens go

`tide --tokens` answers "which workspace, which session, which hour". It cannot
answer "which part of TIDE", because a question typed in the agent pane, a
question asked under a code tour stop and a brief TIDE requested all land in the
same session log, in the same hour, under the same workspace. The log has no
idea which button was pressed.

TIDE does. Every prompt it sends is written down as it goes out, with the name
of the thing that sent it and the byte the session log had reached at that
moment. When the turn ends, everything the log has grown since that byte is the
turn, and the usage lines in it are what the turn cost.

```
tide --spend        # the last five hours
tide --spend today  # since local midnight
tide --spend day    # the last 24 hours
tide --spend week   # the last 7 days
```

```
Where TIDE's tokens went  ·  the last 7 days  ·  214 turns  ·  weighted 41.2M

  by what sent it
      24.8M   60%   118 turns   1402 req    182.4k ctx/req   pane
       7.1M   17%    31 turns    402 req    221.0k ctx/req   job
       4.9M   12%    38 turns    291 req     96.2k ctx/req   stop-ask
       2.6M    6%    14 turns    120 req    140.1k ctx/req   comment
```

## The kinds

| kind | what it is |
|---|---|
| `pane` | typed in the agent pane |
| `stop-ask` | asked under a stop on a code tour |
| `comment` | a batch of review comments sent out of the editor |
| `slide-ask` | asked under a slide of a brief |
| `phone` | typed on the phone |
| `todo` | a to-do handed to the agent |
| `job` | TIDE's own doing: a brief, a tour, a merge note, a PR body check |

`by job` splits that last row by the name the pane already shows, so "Brief of
#6176" and "Question on stop 8" are their own lines.

## What is in the file

One line per turn, in `~/.config/tide/spend.jsonl`:

```json
{"at":1794512345,"secs":42,"kind":"stop-ask","job":"Question on stop 8",
 "tide":false,"root":"/Users/you/dev/app","sid":"fc9eec45","in":2,
 "cw":27756,"cr":13398,"out":174,"calls":9}
```

`in`, `cw`, `cr` and `out` are the four raw numbers Claude Code reported for the
requests in that turn; `calls` is how many requests the turn took. The weighting
the report sorts by is the same one the token panel uses, written out in
[token-usage.md](token-usage.md).

The file is append-only and loses its older half past 4MB, which is tens of
thousands of turns.

## Why the byte offset

A turn's reading is the few kilobytes it appended, not a scan of a log that runs
to megabytes. The offset also makes it exact: a session with a day of requests
in it cannot be billed to the question you just asked, because the question's
mark says where its own turn started.

The measurement happens three seconds after the adapter says the turn ended,
because the last request's usage reaches the log a moment after that, and it
happens on a thread of its own, because the pane has a repaint to do.

## What it does not see

Turns TIDE sent, and nothing else. The reviewers run in sessions of their own,
and `tide --tokens` already breaks those out per workspace. A `claude` running
in a terminal pane is not TIDE's doing at all. A message steered into a turn
that is already running has no line of its own either: it joins that turn, and
what it costs counts against whatever started it.
