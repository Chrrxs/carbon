# Carbon Roblox

Filesystem-authoritative Roblox development with explicit place capture.

The Carbon Studio plugin connects one Studio session to a `carbon serve` process. It live-syncs only the Folder/script mappings established when the server started. After reconciliation, Studio-owned state outside mappings is captured continuously from Studio auto-recovery saves.

The plugin requires a matching Carbon CLI build. Release CLIs embed that exact plugin and automatically install it before `carbon serve` or `carbon studio` launches Studio, replacing a missing or byte-different local copy.

## Connection contract

`carbon serve` prefers port 8000, leases the next available loopback port for concurrent managed sessions, or uses a strict explicit `--port`. The plugin handshake verifies:

- protocol version;
- target project;
- served mapping topology;
- mapped source generation; and
- Studio session identity.

The server accepts one active Studio client. A second client is rejected until the first disconnects. The server remains running after Studio closes and ends only on Ctrl-C or explicit Stop.

A managed launch place is already composed from current source. Plugin startup trusts the endpoint/project/session/generation handoff and attaches the established mapped identities without replaying, traversing, or verifying the manifest hierarchy.

An arbitrary open place may attach when no other Studio client is active. The plugin shows the target project and requires explicit confirmation before Carbon applies filesystem mappings authoritatively.

## Mapping synchronization

Mapped source is one-way and filesystem-authoritative:

- valid mapped file, directory, source, property, and attribute changes flow into Studio;
- Studio drift beneath a mapping never writes back and is overwritten by the next valid filesystem update;
- that reconciliation removes invalid Studio-only descendants and reports each removal explicitly;
- invalid mapped source leaves the last valid realization active; and
- a project-file change reconnects automatically, applies the replacement mapping topology, and is acknowledged only after a transition-bound recovery capture proves it.

The plugin does not write Studio changes into mapped files or project mappings.

## Capture Manifest

`serve` continuously waits for the next Studio auto-recovery save and atomically commits each verified result. **Capture Manifest** waits for the currently active automatic capture cycle and displays its progress.

To import a place saved manually through Studio's **File > Save to File** command, use:

```sh
carbon capture game.carbon.json manually-saved.rbxl
```

Automatic capture verifies the exact project, worktree, and Studio session. Offline manual capture verifies the embedded project identity against the explicit project argument. Both validate that mapped state remains filesystem-authoritative and block:

- scripts outside mappings;
- mapped-owned instance references targeting manifest-owned state;
- ambiguous manifest identity reconciliation; and
- persistent state Carbon cannot represent safely.

Manifest-owned `Ref` properties may target any mapped instance. Capture restores filesystem-authoritative mapped roots before committing the Studio-owned complement, so Studio drift beneath a mapping never writes back to source. `carbon stop` and the first Ctrl+C in the serve terminal both wait for the next auto-recovery or a manual save over the temporary served `.rbxl`, whichever arrives first.

Studio auto-recovery must be enabled. For managed `serve`, Carbon installs the
matching plugin before authorizing an exact-process `robloxstudio-mcp` launch;
the broker remains lifecycle owner through final instance association and
shutdown. Carbon does not inject native code or modify Studio binaries.

## Links

- [Carbon CLI and project documentation](https://github.com/Chrrxs/carbon)
- [Carbon Studio plugin source](.)
