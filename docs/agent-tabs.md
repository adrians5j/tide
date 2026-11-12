# Agent tabs

One agent pane, several conversations. A tab per conversation, a tab per
reviewer, and two buttons at the end of the strip for opening another one.

```
Main   Reading the merge…  ● reviewer-1        + Tab   + with context
```

`+ Tab` opens an empty conversation. `+ with context` opens one carrying the
last conversation's bearings: the workspace, the branch, the pull request, the
last three questions you asked and the answer to the last of them. ⌘T and ⌥⌘T
do the same thing with the agent pane focused; ⌘T elsewhere still opens a
terminal tab.

A tab keeps everything that belongs to it: its transcript, its scroll position,
whatever is half-typed in its composer, its queue, and the turn it has running.
Switching tabs changes none of that. The dot on a tab means a turn is running
in it, so a long answer in one tab is a dot while you work in another.

## Why a second conversation and not a second question

Every request re-sends the whole conversation, so a turn costs about what the
conversation weighs, whatever it asks. Two unrelated pieces of work in one
session means each turn of the second one pays for the first one as well.

That is what `+ with context` is for. The handoff is a few hundred tokens: the
repo is right there to be read, and the three questions are what make a
follow-up land without the transcript behind them. It is the same handoff a
session gets when a turn ends over 250k and the next message starts a new one
(see [token-usage.md](token-usage.md)), with the difference that the
conversation you left is still open in the tab beside it.

A tab that carried the handoff says so on its first line, with what came
across.

## What a tab costs

A conversation is one ACP connection, and a connection carries one session, so
a tab is a second `npx @agentclientprotocol/claude-agent-acp` process — the
same thing a reviewer is. It starts on the tab's first open and is shut down
when the tab closes. The last tab doesn't close; a pane with no conversation
in it has nothing to show.

## In the code

`Conv` (src/main.rs) holds one conversation's state. The one on screen is not
in a `Conv`: it lives in Storm's own `agent_*` fields, the way it did when
there was only ever one, and its slot in `convs` is empty. `swap_conv(i)`
exchanges the two, and calling it twice puts everything back.

That is also how a background tab's events reach the right transcript. The
drain poll stands in each tab in turn — swap in, apply its events, swap out —
so `apply_acp_event` and the two hundred other places that read `agent_msgs`
never had to learn that tabs exist.
