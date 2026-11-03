# Shows Remote

Control Shows on the other TV from a terminal. The target must already have
Shows open; the remote never starts a local player.

```powershell
shows-remote
```

Open **apps → shows remote** in the at menu, or run the command above. The fzt
picker checks the computers in the shared home inventory, lists available Shows
instances first, and explains unavailable PCs. Select an instance to control it;
**Refresh instances** checks again. No player starts as a side effect.

Press `/` in the controller for its fzt command palette: playback controls,
play a show from the current round, reconnect, change instance, or quit.
Arrows select a show; Enter plays it. Space sets play/pause, `s` plays, `p` pauses,
`n` skips **and marks watched**, `b` goes back, `d` defers without marking watched,
left/right seek 10 seconds, and `+`/`-` change volume. `q` leaves Shows running;
`r` reconnects after a disconnect.

```powershell
shows-remote upstairspc status
shows-remote upstairspc queue
shows-remote upstairspc play
shows-remote upstairspc pause
shows-remote upstairspc play "The Simpsons"
shows-remote upstairspc seek -30
shows-remote upstairspc volume 50
shows-remote upstairspc status --json
```

An explicit target skips the picker. One-shot commands require a target or
`--local`; there is no default computer. `--local` controls this PC without SSH;
`--channel dev` selects the target's installed dev channel. Failures exit nonzero
and print to stderr. The interactive remote names the target, shows its alerts
and disables controls while disconnected or waiting for a reply. A timed-out
command may already have applied; inspect status before retrying.

## Installation

Publish the CI release using `scripts/publish-shows-channel.ps1 -Channel stable`
and use the existing launcher to install it on both PCs. Run
`scripts/install-shows-launcher.ps1` to install the terminal command. It adds
`%USERPROFILE%\bin` to user PATH (already on PATH on the home PCs).
The instance picker and command palette use the managed fzt executable at
`%ProgramFiles%\at-home\fzt\current\fzt.exe`, installed by home-sync. A missing
fzt installation explains how to install it and fails explicitly.

Discovery reads `\\NelsonNAS\files\shell-config-profile-home\machine\state.json`.
`SHOWS_REMOTE_INVENTORY` can select another inventory file with the same
`machines` shape for an isolated test or another installation. Names come from
that inventory, never a hardcoded list. This PC connects over loopback; other
PCs use their existing SSH configuration. Discovery probes concurrently and
uses the normal bounded status request, closing each probe's owned agent.

SSH must already work with `ssh -o BatchMode=yes upstairspc hostname`, including
known-host trust. Shows creates no SSH keys and exposes no HTTP listener on the
LAN. The loopback port is 45831; leave `SHOWS_CONTROL_PORT` unset normally.

Release target requests log to `%APPDATA%\shows\shows.log`. The remote stores no
library, history, playback state or secrets.

## Home-service checklist applicability

- Tickets, source, CI and releases belong with Shows so both apps ship together.
- Installation reuses the existing hash-verified Shows channel publisher.
- SSH reuses existing PC access. No new hosted service, browser login, identity
  issuer, secret store, DNS record, TLS listener or homepage URL is introduced.
- No new durable business state needs backup; the NAS database stays authoritative.
- The app is on demand. Connection state and exit status report unavailability;
  an always-on down alert would flag a normally closed TV app as a fault.

See the [remote-control contract](feature-contracts/remote-control.md).
