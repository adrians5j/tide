# Where the tokens went

The plan bars say how much of the window is gone. They don't say what spent it.
This does: tokens by hour, by workspace and by session, over the last day or the
last week.

Open it from the palette, "Where the Tokens Went…", or print the same reading
without opening TIDE at all:

```
tide --tokens        # today, since midnight
tide --tokens day    # the last 24 hours, rolling
tide --tokens week   # the last 7 days, a bar to the day
```

The panel has the same three. Today is the clock's day, not the last 24 hours:
at nine in the morning those are different questions, and the first one is the
one you are asking when a five-hour window keeps running out.

A workspace row, and any session row, opens that workspace when you click it.

## Where the numbers come from

Claude Code writes one JSONL file per session under
`~/.claude/projects/<slug>/<session-id>.jsonl`, and every assistant line in it
carries the usage the API reported for that request:

```json
{"type":"assistant","timestamp":"2026-11-05T11:16:01.291Z","cwd":"/Users/you/dev/app",
 "message":{"usage":{"input_tokens":2,"cache_creation_input_tokens":27756,
 "cache_read_input_tokens":13398,"output_tokens":174}}}
```

`cwd` is the workspace, the file name is the session id, and the timestamp puts
the request in an hour. Nothing is sent anywhere and nothing is instrumented;
this is a read of a log Claude Code was already writing.

TIDE knows which of those sessions it opened itself and what for, because
`spawned-sessions.json` records every one (that is the same list the resume
picker hides behind a toggle). So a reviewer round shows up as four named
sessions rather than as unexplained spend, and each workspace row says how much
of its total was reviewers.

## The weighting

The panel sorts and draws by one number, at the published price ratios:

```
weighted = input + 1.25 × cache written + 0.1 × cache read + 5 × output
```

A million cache reads is not a million tokens of work, and output costs several
times what input does, so a straight token count ranks the wrong things first.
The four raw numbers are in the header if you want them. This is an estimate of
what a stretch of work was worth, not a bill, and not what the plan's own meter
counts.

## The scan

Session logs run to gigabytes. The scan remembers how many bytes of each file it
has already read, in `~/.config/tide/token-scan.json`, along with that file's
totals per hour. The files only ever grow, so the next scan reads the tail.

A first scan of 1.7GB takes about five seconds, off the UI thread; the ones
after it read a few megabytes and take about twenty milliseconds. Files nothing
has touched in thirty days are dropped from the cache and never reopened.
