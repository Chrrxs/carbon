# Carbon usage and project format

This document covers the operational details intentionally omitted from the
project README.

## Project ownership

A Carbon project has two explicit source domains:

- filesystem-authoritative mappings for complete Folder and script subtrees;
- a strict binary artifact for Studio-authored state outside those mappings.

Mapped roots and their descendants never enter the Studio artifact. Carbon
composes both domains for builds and managed Studio sessions. An existing
binary place can be converted with `carbon migrate`; Carbon does not read the
legacy manifest store or silently fall back to another format.

Mapped directories need portable file content to exist below a mapped root.
Carbon ignores bare empty directories because Git cannot preserve them across
worktrees or clones. To author an intentionally empty `Folder`, place an empty
JSON object in its `meta.json`; `carbon migrate` writes this marker
automatically for empty extracted Folders.

`$path` values are relative to the project file. They may use `.` and `..`, so
places in sibling directories can map the same shared source directory.
Absolute paths remain unsupported.

## Commands

```sh
# Convert an existing place and extract mapped script source.
carbon migrate existing.rbxl --output game.carbon.json

# Create a project, Studio-state artifact, and starter source.
carbon init --output game.carbon.json --name Game

# Compose a permanent Roblox place.
carbon build game.carbon.json --output game.rbxl

# Build a disposable place, launch Studio, and serve mapped source.
carbon serve game.carbon.json

# Focus the Studio managed for a worktree, instance ID, or endpoint port.
carbon focus --worktree .
carbon focus 'anon:550e8400-e29b-41d4-a716-446655440000'
carbon focus --port 8000

# Capture Studio-owned state using the instance ID printed by `serve`.
carbon capture 'anon:550e8400-e29b-41d4-a716-446655440000'

# Capture, then stop the server and its managed Studio process.
carbon stop 'anon:550e8400-e29b-41d4-a716-446655440000'

# Compare binary parity.
carbon diff open-place.rbxl current-source-build.rbxl
```

`serve` prefers loopback port 8000 by default and atomically leases the next
available port when another Carbon session is already using it. `--port` keeps
strict single-port behavior. The selected endpoint is embedded in the
disposable place and printed during startup, so managed sessions from separate
Git worktrees can build, launch, synchronize, capture, and stop concurrently.
Each endpoint accepts one Studio client.

`carbon focus` activates the exact native Studio process launched for the
selected serve session. A worktree target may be the repository root or any
path inside it. Carbon never falls back to window-title matching; if more than
one session is registered for the same worktree, select it by instance ID or
port instead. Serve sessions started by an older Carbon version must be
restarted once so their Studio process and worktree identity are registered.

When `robloxstudio-mcp` owns the Studio lifecycle, the connected message prints
the instance ID reported by `manage_instance`; pass that ID to `carbon capture`
or `carbon stop`. Both commands also accept `--port` for explicit endpoint
selection. During startup, `serve` reports managed-place building, Studio
launch, and connection waiting separately. The waiting message omits the ID
until Studio has connected and MCP has assigned it. In automatic lifecycle
mode, including on Linux/WSL, Carbon delegates launch ownership to a compatible
broker. The broker's process-identity launch remains suspended until Carbon has
prepared exact-process RML injection and sends authorization, then retains
ownership until Carbon attests the injected runtime and completes the launch.
Carbon uses direct launch when no compatible broker is available or when it can
prove a selected broker failed before dispatch. If a broker request may have
been dispatched, Carbon withholds direct fallback to avoid a duplicate or
unowned Studio process and reports the broker endpoint, failure stage, complete
cause chain, and recovery command. After inspecting or restarting the broker,
explicitly select direct lifecycle for a subsequent attempt with
`CARBON_STUDIO_LIFECYCLE=direct carbon serve`.

Mapping topology is frozen
for the session, so restart `serve` after changing the project file. Filesystem
edits beneath a mapping reconcile authoritatively; Studio edits beneath a
mapping never write back to source.

## Canonical files

- `*.carbon.json` contains a strict Rojo-shaped mapping tree.
- `*.carbon.data/state.carbon` contains the Studio-owned complement in a
  checksummed `CARBONRB` version 1 envelope.
- Large values live in `*.carbon.data/blobs/<blake3>.zst` and are committed by
  kind, length, and digest from the artifact.

The artifact includes normalized Roblox binary plus compact MessagePack data
for stable identities, references, metadata, and external values. The reader
fails closed on unsupported versions, corruption, missing blobs, unknown
flags, or trailing bytes. Repository readability without Carbon is not a
format goal; deterministic bytes, bounded reads, semantic identity, and
mergeability are.

## Capture behavior

Capture acquires a fresh native hierarchy and reference lease, validates a
complete sequence of bounded chunks, and promotes the Studio artifact and any
new mapped identity metadata atomically. A failed or blocked capture preserves
the previous canonical state.

Carbon blocks capture when persistent state cannot be represented safely,
including scripts outside mappings and mapped-owned references to Studio-owned
objects. Studio-owned references may target stable mapped identities.

Closing or disconnecting Studio does not capture. `carbon stop` deliberately
captures before ending the served session.

## Semantic Git merges

Carbon merges artifacts by stable instance identity. Independent additions,
deletions, renames, reparents, metadata edits, and property edits can converge
through an ordinary three-way merge. Conflicting edits to the same semantic
field remain explicit conflicts.

`carbon serve` installs the repository-local merge driver and attribute rule
without dirtying the worktree. To publish the attribute rule before anyone
runs `serve`, commit:

```gitattributes
*.carbon merge=carbon -diff
```

Resolve conflicts through Carbon's structured plan:

```sh
carbon conflicts --json > carbon-conflicts.json
# Add one decision for each conflict ID.
carbon resolve --plan carbon-conflicts.json
git merge --continue
```

Decisions can take the base, current, or incoming value, set a typed property
value, or remove an eligible property or metadata value. Carbon validates and
stages the result but leaves the final Git operation to the user.

## Embedded Studio components

Release executables embed their matching Studio plugin and RML package. Before
launching Studio, Carbon verifies those bytes, replaces a missing or outdated
plugin, and materializes RML without replacing unrelated mods or user
configuration.

The full installed-stack qualification and release contract is documented in
[`qualification/README.md`](qualification/README.md).
