# Carbon

Carbon puts complete Roblox places in source control without forcing every
instance into the filesystem.

Local files own selected Folder and script subtrees. Studio owns everything
else. Carbon captures Studio-owned state in a deterministic artifact and uses
stable instance identities to merge independent changes.

Mapped source syncs from the filesystem to Studio. Run a capture to save
Studio-owned changes.

> [!IMPORTANT]
> Carbon is pre-1.0. It supports Roblox Studio on x86_64 Windows, either
> natively or through WSL2. The project format can change before 1.0.

## Source ownership

- **Mapped source:** Local files own the mapped instance and every descendant.
- **Studio-owned state:** Studio owns every instance outside a mapping.

Each instance has one source domain. Carbon removes Studio-only changes below
a mapping during source reconciliation.

## Select Carbon or Rojo

Use Carbon when local source and Studio-authored content must coexist in one
place, and the complete place must support semantic Git merges.

Use [Rojo](https://github.com/rojo-rbx/rojo) when the filesystem is the primary
source of truth or you need the full Rojo project format and ecosystem.

## Architectural differences from Rojo

Carbon uses a Rojo-shaped project tree, but its ownership model is stricter.

### Use narrow mappings

A `$path` owns its root and every descendant. A mapping on an engine service
owns all contents of that service. Carbon cannot retain a new Studio-owned
instance in that service.

Map only the folders and scripts that the filesystem must own. Unmapped
siblings can remain Studio-owned.

### Do not mix source domains below a mapping

Create mapped instances in local source. Create Studio-owned instances under
Studio-owned parents. Removing a mapping does not transfer its old subtree to
Studio-owned state.

The project root must be a `DataModel` and cannot have `$path`. Its direct
children must be Roblox services. Mappings can be engine anchors, direct
children of services, or supported containers below `StarterPlayer`. Carbon
cannot route a mapping through an arbitrary Studio-owned instance.

Carbon project nodes support `$className`, `$path`, `$id`, `$properties`, and
`$attributes`. Carbon rejects other special fields, including Rojo
`$ignoreUnknownInstances` behavior.

Carbon permits these classes below a mapping:

- `Folder`
- `Script`
- `LocalScript`
- `ModuleScript`

Mapped directories can contain scripts, child directories, and one optional
`meta.json`. Carbon does not load `.rbxl` or `.rbxm` files below mappings.

Studio-owned instances can refer to mapped instances. Mapped instances cannot
refer to Studio-owned instances.

See [Usage and project format](USAGE.md) for the full mapping and capture rules.

## System requirements

- Windows 10 or 11 on an x86_64 machine
- Roblox Studio on Windows
- Carbon CLI on native Windows or WSL2
- [Rokit](https://github.com/rojo-rbx/rokit) in the CLI environment

## Install

```sh
rokit add Chrrxs/carbon
```

The executable includes its matching RML runtime and Studio plugin. Carbon
installs or updates them when `serve` or `studio` starts.

## Start

```sh
# Convert an existing place.
carbon migrate existing.rbxl --output game.carbon.json

# Or create a project.
carbon init --output game.carbon.json --name Game

# Start live sync and Studio.
carbon serve game.carbon.json

# Save Studio-owned state with the instance ID from serve.
carbon capture 'anon:550e8400-e29b-41d4-a716-446655440000'

# Build a place file.
carbon build game.carbon.json --output game.rbxl
```

## More

- [Usage and project format](USAGE.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)
- [MIT license](LICENSE.md)
