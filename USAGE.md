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
carbon focus --worktree . --restore

# Move one managed Studio back to its configured parking desktop without
# focusing another Studio.
carbon park --worktree .
carbon park 'anon:550e8400-e29b-41d4-a716-446655440000'
carbon park --port 8000

# Import a manually saved binary place directly into a project.
carbon capture game.carbon.json manually-saved.rbxl

# Wait for the next auto-recovery capture, then stop serve and Studio.
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

To keep Studio windows on a custom-named Windows 11 parking desktop, add this
workspace setting to `carbon.toml` beside the project:

```toml
studio_desktop = "Studios"
```

The setting applies to both managed `carbon serve` launches and the standalone
`carbon studio` command. An empty value disables desktop placement and focus
routing. Carbon resolves the name case-insensitively before launch, moves only
the exact Studio process it started, verifies the resulting desktop, and does
not switch the active desktop. Placement currently requires Windows 11 24H2 or
newer (including when Carbon runs through WSL); a missing, duplicate, or
unsupported desktop fails the launch and cleans up the new Studio process.
Parking also guards Studio audio by default; there is no separate audio setting.
Carbon mutes shared-mode render sessions belonging to the exact parked Studio
process on every active output device and keeps a Windows Core Audio guardian
alive for streams created later. Focusing that Studio restores only mute states
that Carbon changed, so a session that was already user-muted stays muted.
Carbon carries that ownership through Windows' persisted replacement audio
sessions, including a changed session identifier or a replacement Studio process
at the same executable path, and verifies that no Carbon-owned mute remains
before focus reports success.

Parking also keeps an exact-process Windows activation guardian alive. The
guardian installs a CBT veto only on the parked Studio's UI threads, so Studio
cannot make itself foreground when playtest state changes or another internal
action requests activation. Mouse activation and Alt+Tab remain user-controlled.
Carbon removes every veto before it focuses or unparks that Studio. The guardian
revalidates the process executable and creation time throughout its lifetime and
exits with the exact Studio process generation.

`carbon focus` leaves the exact native Studio process launched for the selected
serve session in the foreground. When `studio_desktop` is configured, focus
first captures the currently active Windows desktop, moves the selected Studio
there, and moves other running Carbon Studio sessions from the same Git
repository back to the parking desktop each session recorded at launch. Run
`carbon focus` from the desktop where you want to work; that desktop does not
need a configured name. Routing is serialized so concurrent focus commands do
not interleave. Moving the selected Studio is strict, while a stale sibling is
reported as a warning and does not block focus.

`carbon park` moves only the selected exact managed Studio back to the
`studio_desktop` recorded when its serve session started. It does not activate
Studio, switch desktops, or park repository siblings. This is useful after
working in one Studio when no other Studio needs to be focused. Both explicit
and automatic parking guard programmatic focus, mute Carbon-owned audio, and
clear taskbar attention from every top-level window
owned by the verified Studio process, including an off-desktop modal; Carbon
never dismisses or answers that dialog. `carbon focus` repeats the parked
sibling attention pass after activating the selected Studio because Windows
can relatch a shared taskbar group during activation.

If Studio owns an active modal dialog, Carbon focuses that dialog instead of the
disabled main window. Pass `--restore` to verify Studio activation and then
return to the previously foreground window. A worktree target may be the
repository root or any path inside it. Carbon never falls back to process-name
or window-title matching, and sessions from different Git repositories are
left untouched. If more than one session is registered for the same worktree,
select it by instance ID or port instead. Serve sessions started by an older
Carbon version must be restarted once so their exact process, repository, and
launch-time parking desktop are registered.

After Carbon successfully validates an exact-session Studio auto-recovery and
commits it atomically, it atomically moves that consumed `.rbxl` into the
`.carbon-consumed` directory beside Roblox's AutoSaves. This keeps consumed
Carbon recoveries out of Studio's recovery scan while preserving the original
bytes. Carbon does not archive manual saves, rejected recoveries, unknown
files, or evidence from a failed capture; an archive failure leaves the source
in place and emits a warning.

Managed `serve` requires a loopback `robloxstudio-mcp` that advertises lifecycle
protocol v3 with exact process identity. The default MCP URL is
`http://127.0.0.1:58741`; set `CARBON_STUDIO_MCP_URL` to another loopback URL.
Carbon uses the broker's `ROBLOX_STUDIO_AUTH_TOKEN`, explicit no-auth setting,
or default `~/.robloxstudio-mcp/auth-token` file.

Startup reports two different identifiers. The **launch ID** is the broker's
opaque ownership handle and is usable for startup status and cleanup before a
plugin connects. It is not a Studio tool-routing ID. `serve` reports ready only
after the broker associates that launch with its final **instance ID**. The
connected message prints both and the instance ID can be passed directly to
Roblox Studio MCP tools, `carbon focus`, or `carbon stop`. `carbon stop --list`
shows `Pending` in the Instance ID column until association completes and keeps
the Launch ID in its own column.

`carbon capture` instead takes a project and a saved `.rbxl` and never contacts
the serve endpoint. Carbon never starts a second, independently owned Studio if
broker discovery, authorization, ownership completion, or association fails.
The broker retains the exact native PID and process-creation identity for close;
Carbon keeps the same identity only as an emergency close fallback if the
broker becomes unreachable.

This is a managed-serve compatibility change: `CARBON_STUDIO_LIFECYCLE` no
longer selects a direct `serve` launch. The standalone `carbon studio` command
remains an unmanaged convenience launch. Restart serve sessions created by an
older Carbon so their registry entries gain a distinct launch ID; legacy
entries remain readable and show `-` in the Launch ID column.

Project-file changes reload automatically in the existing `serve` process.
Carbon first captures authored Studio state, then reconnects the plugin to the
replacement mapping contract; the reload is complete only after Studio applies
and proves that topology with a transition-bound capture. Filesystem edits
beneath a mapping reconcile authoritatively, while Studio edits beneath a
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

After the plugin finishes its initial mapping reconciliation, `carbon serve`
continuously waits for new Studio auto-recovery files. Each stable binary
recovery is verified against the exact served project, worktree, and Studio
session, its filesystem-authoritative mapped roots are restored, and the new
Studio artifact is promoted atomically. Carbon immediately starts waiting for
the following recovery after a successful commit.

`carbon capture game.carbon.json manually-saved.rbxl` is the explicit offline
path. It validates and commits that existing binary place immediately without
requiring an instance ID, port, running server, or connected Studio. The
place's embedded Carbon project identity must match the explicit project.

Studio auto-recovery must be enabled. On Windows Carbon watches
`%LOCALAPPDATA%\Roblox\RobloxStudio\AutoSaves`; from WSL it watches the same
Windows directory through `wslpath`. Tests and custom environments may set
`CARBON_STUDIO_AUTOSAVES_DIR` to an explicit directory. Only new or changed
`.rbxl` files created after the active automatic wait began are eligible. Each
wait is bounded to six minutes and then restarts while the serve session remains
connected. A failed, cancelled, or timed-out attempt preserves the previous
canonical state.

Carbon blocks capture when persistent state cannot be represented safely,
including scripts outside mappings and mapped-owned references to Studio-owned
objects. Studio-owned references may target stable mapped identities.

`carbon stop` races the next eligible auto-recovery against a manual save over
the temporary `carbon-serve-*.rbxl` launch place, then asks
`robloxstudio-mcp manage_instance` to close the exact launch as soon as either
verified file arrives.
Pressing Ctrl+C in the `carbon serve` terminal follows the same default
shutdown path. A second Ctrl+C forces cleanup.

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

## Embedded Studio component

Release executables embed their matching Studio plugin. Before authorizing a
managed broker launch, Carbon verifies those bytes and replaces a missing or
outdated plugin. No native mod-loader, DLL injection, or Studio binary
manipulation is used.

The full installed-stack qualification and release contract is documented in
[`qualification/README.md`](qualification/README.md).
