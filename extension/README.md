# Talk to TIDE

A Chrome extension for the other half of the conversation: you're reading a
page, and you want to tell an agent something about it. There's a small `tide`
tab on the right edge of every page. Click it and a composer slides out: pick
the workspace, type, send.

The release notes case it was built for: highlight an entry, click the tab,
type "this one looks good, mark it as completed", send. The agent gets the
sentence, the heading you highlighted and the page URL. Take a screenshot
yourself (⌃⇧⌘4) and ⌘V it into the box to send that too.

## Install

1. `chrome://extensions` → turn on Developer mode → **Load unpacked** → pick this
   folder.
2. Open the extension's options (⋮ → Options on its card).
3. In TIDE: `⌘K → Copy the Phone Link`. Paste the whole link into **Host**; the
   token is split out of it for you. Save.

That link is `http://<host>:7420/m?t=<token>` — the same hub the phone page uses.
The extension talks to the same two routes: `GET /thread` for the workspace list
and `POST /say` to send.

## Using it

- The `tide` tab sits on the right edge of the page. Click it, or press
  **⌥⇧T**, or click the toolbar icon. **Esc** or ✕ puts it away.
- The dropdown lists the live workspaces, marked `· working` while the agent is
  busy. The **Active / All** pill beside it brings in the parked and idle ones;
  the choice sticks. The one you're on is always listed, parked or not. It
  opens on the one you last talked to.
- **⌘V** in the box attaches an image from the clipboard, so any screenshot you
  take yourself goes with the message. Several can be attached; click a
  thumbnail's ✕ to drop one.
- The page link always goes along. Anything you had highlighted goes with it,
  quoted under the message and shown under the box; click that line to leave it
  out.
- **⌘↩** sends. **Thread** opens TIDE's own web view in a tab, which is where
  the answer shows up.

## Notes

- Nothing is sent anywhere but your own machine. The host is loopback by
  default, and every call carries the token TIDE generated; without it the hub
  answers 403.
- Pasted images are scaled to 1800px and re-encoded, so a retina capture costs
  a few hundred KB instead of several MB. You pay for it twice otherwise: once
  on the wire and again in the agent's prompt.
- The panel is in a shadow root, so the page's CSS can't reach it and its own
  can't reach the page.
- The icons come from `../assets/tide-icon.svg`, rendered to PNG. Change the
  SVG and re-render if you want a different mark.
- `chrome://` pages and the Web Store don't run extensions at all, so the tab
  doesn't appear there; the toolbar icon shows a dash to say why.
- Host permissions are loopback, `*.local` and `*.ts.net`. If you run TIDE's hub
  on a LAN IP, add that host to `manifest.json` and reload the extension.
