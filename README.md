# Carbon

Carbon is a hybrid source workflow for whole Roblox places. You choose which
Folder and script subtrees are owned by local files; everything else remains
authored in Studio and is captured as a deterministic, mergeable artifact.

Mapped source synchronizes live in one direction. Studio-owned state changes
only when you explicitly capture it, so ownership never depends on which copy
changed last.

> [!IMPORTANT]
> Carbon is pre-1.0. Releases support Roblox Studio on x86_64 Windows both
> natively and through WSL2, and project formats may still change before 1.0.

## Who Carbon is for

Carbon is for Roblox developers and teams who want external editors and Git for
code without reconstructing every model, UI, or authored object as filesystem
source. It is especially useful for existing, Studio-heavy places that need
repeatable builds, whole-place versioning, and meaningful Git merges.

Carbon is not the simplest choice for a filesystem-only project, a reusable
package, or a workflow where Studio should automatically overwrite local code.

## Carbon vs. Rojo

Carbon's main advantage is instance-aware source control for the complete
place. Its artifact gives instances stable identities, so Carbon can merge
independent renames, reparents, property edits, additions, and deletions by
their meaning instead of treating the place as an opaque binary file.

| Tool | Best fit |
| --- | --- |
| [Carbon](https://github.com/Chrrxs/carbon) | Selected trees belong to the filesystem, the rest belongs to Studio, and instance changes across the complete place must merge semantically. |
| [Rojo](https://github.com/rojo-rbx/rojo) | The filesystem is the primary source of truth and you want the established Rojo ecosystem. |

Choose Carbon over Rojo when you need an explicit filesystem/Studio ownership
split and semantic conflict handling for Studio-authored instances. When two
branches edit the same instance field incompatibly, Carbon reports a structured
conflict with instance context and resolves it through an explicit plan.

## Platform support

- Windows 10 or 11 on an x86_64 machine
- Roblox Studio installed on Windows
- Native Windows or WSL2 for the Carbon CLI
- [Rokit](https://github.com/rojo-rbx/rokit) installed in the same environment
  as the Carbon CLI

## Install

```sh
rokit add Chrrxs/carbon
```

The executable contains its matching RML runtime and Studio plugin. Carbon
installs or updates both automatically when `serve` or `studio` starts.
Rokit selects the native Windows x86_64 build in Windows and the Linux x86_64
build in WSL2.

## Start a project

```sh
# Convert an existing place.
carbon migrate existing.rbxl --output game.carbon.json

# Or create a new project.
carbon init --output game.carbon.json --name Game

# Start live mapped-source sync and Studio.
carbon serve game.carbon.json

# Focus the Studio managed for this Git worktree.
carbon focus --worktree .

# Save Studio-owned state using the instance ID printed by `serve`.
carbon capture 'anon:550e8400-e29b-41d4-a716-446655440000'

# Produce a place file.
carbon build game.carbon.json --output game.rbxl
```

## More

- [Usage and project format](USAGE.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)
- [MIT license](LICENSE.md)
