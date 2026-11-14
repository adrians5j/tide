# The phone link

TIDE serves the agent panes to a phone: the workspace list, the open
transcript, pictures, and a box to type into. One listener on port 7420 for the
whole window, one token, one bookmark.

## Where the link points

In order of preference:

1. **A tunnel**, when you have opened one. `https://xxxx.ngrok.app/m?t=…`
2. **Tailscale**, `machine.tailxxxx.ts.net`. Works from anywhere both ends are
   on the tailnet, and inside the house too, so one bookmark covers both.
3. **mDNS**, `machine.local`. Survives a new DHCP lease.
4. **The LAN address**, `192.168.1.114`. Always works, never memorable, wrong
   after the router hands out a different one.

"Copy the Phone Link" in the palette gives you the best one there is. "by IP"
forces the fourth, for a phone that will not resolve `.local`.

## The tunnel

"Phone Link over the Internet" starts `ngrok http 7420` and reads the address
back off ngrok's own JSON log. The link is copied when it arrives. Running the
command again closes it, and so does quitting TIDE: a public address on your
machine should not outlive the window that opened it.

It needs ngrok once:

```
brew install ngrok
ngrok config add-authtoken <token from dashboard.ngrok.com>
```

Without the authtoken ngrok exits instead of retrying, and TIDE says so rather
than leaving you waiting for a link.

On the free tier the first visit from a browser hits ngrok's own interstitial
page and you click through to get to TIDE. A paid plan or a reserved domain
removes it.

## What a public link means

Tailscale is private: both ends are on your tailnet and nothing else can reach
the port. A tunnel is not. The address is on the public internet, and the token
in the query string is the only thing between a stranger and this window.

What that gets whoever has it: every workspace's name and branch, the open
transcript, the pictures in it, and a box that sends messages to your agent.
The token is 32 random characters and the address is random too, so it is not
going to be guessed, but it is one copied link away from being shared. Close
the tunnel when you are done.
