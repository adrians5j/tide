# Showcase recording

A shot list for a 90-second video of TIDE. Each shot says what to do and what
the viewer should notice, so it can be recorded in one take with a few retries
rather than edited together.

Record with QuickTime (File, New Screen Recording) or
`screencapture -v -V 120 ~/Desktop/tide.mov`. Either one needs Screen Recording
permission for the app you start it from.

## Before recording

- Open two workspaces, one with an open PR that has conflicts or is behind. The
  conflict chips are the shot that lands hardest.
- Have one workspace mid-conversation with the agent, so the transcript is not
  empty on camera.
- Clear the todo list of anything you would not want read aloud.
- Window at 1600x1000 or wider. Smaller and the three columns fight.

## Shots

**1. Launch (0:00 to 0:08).** Start TIDE cold. Every workspace's agent comes up
on its own, spinner in each pane. This is the one moment that shows TIDE holds
several checkouts at once, and it only happens at launch.

**2. ⌥Tab between workspaces (0:08 to 0:15).** Hold ⌥ and tab through the ring.
Each card shows the branch, the PR, and whether an agent is mid-turn. Release on
a workspace with an open PR.

**3. The topbar (0:15 to 0:25).** Slow pan across it. Title and checkpoint,
branch, PR number with the checks dot, `↓4 behind next`, `⚠ PR conflicts`, the
local host, the watch button. Click `↓4 behind next` and let the handoff land in
the agent pane. Say that one click sends the merge, the conflict list and the
build command.

**4. The PR body as a tab (0:25 to 0:40).** Click the PR chip. The body opens as
a markdown tab with the state pill, branches, author, file counts and CI, and
the title in the tab bar. Scroll once. Click Merge to open the confirmation,
fold a folder, then press esc. Do not merge on camera.

**5. Review comments (0:40 to 0:55).** Open a diff, ⌥C on a line, type a short
comment, send the batch. The agent answers in the thread and posts a summary
card. The card is the payoff, so hold on it.

**6. Reviewers (0:55 to 1:10).** Start the reviewers. The card shows both
sessions reading with their own timers, and the composer says it is waiting.
Cut to the sectioned summary when they finish. This is a long wait in real
time, so cut it.

**7. Todos and ⌘E (1:10 to 1:25).** ⌘⇧L for the list, show a task the agent
filed with its details, hit ▶ to hand it back. Then ⌘E to jump between files.

**8. Close (1:25 to 1:30).** Back to the topbar with everything green.

## What to leave out

- The terminal. Every editor has one.
- Settings, themes, the file tree. Nobody watches a tree.
- Anything that needs an explanation longer than the shot.

## Credentials

`~/.config/tide/agent-notes.md` holds the local login and the API key, and the
briefing panel (the gear in the agent pane) prints them verbatim. Don't open
that panel on camera.
