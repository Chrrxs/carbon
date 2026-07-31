# Carbon Roblox

Filesystem-authoritative Roblox development with explicit place capture.

The Carbon Studio plugin connects one Studio session to an `carbon serve` process. It live-syncs only the Folder/script mappings established when the server started. Studio-owned state outside mappings is never synchronized continuously; the user saves it explicitly with **Capture Manifest**.

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
- a project-file change reconnects automatically, applies the replacement mapping topology, and is acknowledged only after a transition-bound full capture proves it.

The plugin does not write Studio changes into mapped files or project mappings.

## Capture Manifest

**Capture Manifest** invokes the same server operation as:

```sh
carbon capture --port 8000
```

Capture performs a fresh full native RML scan only on demand, reports progress, supports cancellation, and transfers full chunks whenever equality with the prior manifest cannot be proven. The plugin only requests the operation and displays its status; it never traverses the place, records manifest changes, or constructs capture payloads. Capture validates that mapped state still matches the served generation and blocks:

- scripts outside mappings;
- mapped-owned instance references targeting manifest-owned state;
- ambiguous manifest identity reconciliation; and
- persistent state Carbon cannot represent safely.

Manifest-owned `Ref` properties may target any mapped instance. Capture atomically persists a missing mapped target ID in directory `meta.json` or project `$id` together with the manifest; this identity-only write is not Studio syncback. Capture never writes other mapped state or changes ownership topology. Disconnect, Studio close, and Ctrl-C do not capture automatically; `carbon stop` deliberately captures before ending the served session.

## Privileged serialized properties

With the monorepo RML build and `Carbon.RmlBridge`, the CLI can broker serialized properties that ordinary Studio APIs cannot read during explicit capture. The bridge is authenticated and bound to the exact Studio process; its bearer token is never disclosed to the plugin. Missing or incompatible elevation leaves the safe reflection path active and turns unreadable persistent authored state into an actionable capture blocker.

## Links

- [Carbon CLI and project documentation](https://github.com/Chrrxs/carbon)
- [Carbon Studio plugin source](.)
- [Carbon RML bridge source](../rml/code/dotnet/CarbonBridge)
