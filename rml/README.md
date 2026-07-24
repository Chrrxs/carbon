<p align="center">
  <p align="center">
	<img width="150" height="150" src="./assets/logo.png" alt="Logo">
  </p>
  <h1 align="center"><b>Roblox ModLoader</b></h1>
  <p align="center">
    A modding framework for Roblox Studio, enabling native, C#, and internal Luau script mods.
  </p>
</p>

<div align="center">

![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/revolutionxk/roblox-modloader/build.yml?style=for-the-badge&branch=develop&logo=github&label=develop%20build)
![GitHub Workflow Status](https://img.shields.io/github/actions/workflow/status/revolutionxk/roblox-modloader/build.yml?style=for-the-badge&branch=main&logo=github&label=main%20build)
[![GitHub License](https://img.shields.io/github/license/revolutionxk/roblox-modloader?style=for-the-badge)](LICENSE)

</div>

<div align="center">

[![Discord](https://img.shields.io/discord/1405678257545936916?style=for-the-badge&logo=discord&logoColor=white)](https://robloxmodloader.com/)

</div>

> [!NOTE]
> This project is still in development and may contain bugs or incomplete features.

> [!NOTE]
> This directory is Carbon's integrated RML module and contains its
> authenticated, serialized-property-only bridge.

> [!WARNING]
> Roblox changed (shuffled) the internal layout of Luau’s structs a few months ago, so the in-memory structures we
> relied on no longer line up. Because of that I’m building a
> static-analysis [dumper](https://github.com/revolutionxk/roblox-modloader/tree/develop/dumper) to reconstruct the
> correct structs and offsets so scripting support can work again. Luau/internal scripting is temporarily disabled while
> I
> finish that—native C++/C# mods keep working normally. I’ll re-enable scripting once
> the [dumper](https://github.com/revolutionxk/roblox-modloader/tree/develop/dumper) produces a stable, reliable
> mapping.

## Cross-Platform Support

- [x] Windows
- [x] Linux Vinegar
- [ ] macOS (planned)

## Quick Start

### Installation

RML targets your local Roblox **Studio** installation (typically under
`%LOCALAPPDATA%\Roblox\Versions\<version>\`).

RML is built and versioned with the rest of the Carbon monorepo. `carbon serve`
and `carbon studio` install the bundled build automatically when the latest
Studio version has no RML marker or has an older one. Use `carbon rml status`
or `carbon rml ensure` to inspect or trigger the same operation explicitly.
Do not install an independently versioned RML release into a Carbon-managed
Studio installation.

The release archive is already laid out so a plain extract lands everything in the right place:

```
<Studio directory>/
├── RobloxStudioBeta.exe
├── dwmapi.dll                    proxy, loaded by Studio
└── RobloxModLoader/
    ├── roblox_modloader.dll      native core
    ├── config.toml               created with defaults if missing
    ├── runtime/                  the .NET host and bundled runtime
    └── mods/                     your mods, one folder each
        └── your-mod/
            ├── native/           native C++ mod DLLs
            ├── dotnet/           .NET mod assemblies
            └── scripts/          Luau scripts (temporarily disabled)
```

## Writing a mod

The quickest way to start is to copy one of the [examples](#examples) and
adapt it.

## Examples

| Example                                             | Surface    | What it shows                                                     |
|-----------------------------------------------------|------------|-------------------------------------------------------------------|
| [`basic-mod`](examples/basic-mod)                   | C++        | Minimal native mod skeleton and the hooking entry points          |
| [`internal_developer`](examples/internal_developer) | C++        | Enables Studio's internal developer tools                         |
| [`discord_rpc`](examples/discord_rpc)               | C++ / Luau | Discord Rich Presence, native + script bridge                     |
| [`example_dotnet`](examples/example_dotnet)         | C#         | Services, instances, properties, and events through the typed API |
| [`discord_rpc_dotnet`](examples/discord_rpc_dotnet) | C#         | A managed Discord Rich Presence integration                       |

## Building from source

### Prerequisites

- Windows with Visual Studio 2022 (MSVC, x64)
- CMake 3.22.1 or newer
- .NET 10 SDK (for the managed runtime and mods)
- Git

### Steps

From the Carbon repository root, use the supported product build and runtime
gate. It derives the build identity automatically, retains the Windows-local
native and .NET caches, and packages RML for the CLI installer:

```sh
./scripts/change qualify
```

For focused CMake development, use `rml/` as the source directory. Product
packages must still go through `scripts/build-rml.ps1` so the same derived
identity is injected into native and managed artifacts.

### Build options

| Option                                   | Description                          | Default |
|------------------------------------------|--------------------------------------|---------|
| `ROBLOX_MODLOADER_BUILD_PROXY_GENERATOR` | Build the proxy generator tool       | ON      |
| `ROBLOX_MODLOADER_BUILD_PROXY_DLL`       | Auto-generate the `dwmapi.dll` proxy | ON      |
| `ROBLOX_MODLOADER_BUILD_EXAMPLES`        | Build the example mods               | ON      |

## Roadmap

Current focus areas:

- Finish the [dumper](https://github.com/revolutionxk/roblox-modloader/tree/develop/dumper)
- Re-enable Luau/internal scripting support
- Add macOS support (long-term goal)
- Finish .NET modding support (Better API, async support, etc.)
- Add a mod browser and installer

## Contributing

Contributions are welcome. Please open an issue to discuss substantial changes
first, keep pull requests focused, follow the existing code style, add tests
where it makes sense, and follow the repository's
[contribution guide](../CONTRIBUTING.md).

## License

Released under the MIT License. See [LICENSE](LICENSE).

## Disclaimer

This project is provided for educational and research purposes. You are responsible for complying
with Roblox's Terms of Service and any applicable laws. It is not affiliated with or endorsed by
Roblox Corporation.
