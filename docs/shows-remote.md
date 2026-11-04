# Shows remote

The terminal remote and SSH agent now live in their own repository:
[shows-remote](https://forgejo.romaine.life/nelsong6/shows-remote).
Its [usage](https://forgejo.romaine.life/nelsong6/shows-remote/src/branch/main/docs/usage.md)
and [client contract](https://forgejo.romaine.life/nelsong6/shows-remote/src/branch/main/docs/remote-control.md)
are maintained there. The existing `shows-remote` command and at-menu entry
launch the independently installed native client.

The desktop owns playback, NAS state, GPU preview capture and the protocol-1
loopback HTTP/SSE contract. It keeps the `shows-control` crate, which the remote
pins by immutable Git revision. Desktop releases contain the player and libmpv;
remote releases contain the client and signed installers. Neither release
requires rebuilding the other application.