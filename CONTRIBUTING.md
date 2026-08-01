# Contributing to Carbon

Thanks for helping improve Carbon. Bug reports, focused proposals,
documentation fixes, and tested code changes are welcome.

## Before opening a change

- Search the existing issues first.
- Use the issue forms for bugs and feature proposals.
- Discuss large behavior or format changes before investing in an
  implementation.
- Never include private places, credentials, authentication tokens, or
  qualification artifacts in an issue or pull request.

Use the repository's `.local/` directory for personal notes, experiments, and
other contributor-only working files. Git ignores the entire directory.

## Development environment

The complete release stack requires Windows 10 or 11, x86_64 WSL2, Roblox
Studio, Rust, .NET, Node.js, Rokit, and `cargo-audit` 0.22.2. Install the
security and plugin toolchains with:

```sh
cargo install cargo-audit --locked --version 0.22.2
cd studio-plugin
rokit install
```

The ordinary Rust checks are available on every supported Rust host and run in
CI on both Linux and native Windows:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo audit
cargo test --locked --all-targets
```

### Incremental RML builds

During development, reuse one preconfigured CMake build directory through
`CARBON_RML_CMAKE_BUILD_DIR`. Preserve that directory's generator, dependency
cache, and object files. Build only the target affected by the current change:

Use the guarded wrapper, which validates the existing cache and runs only the
equivalent target build:

```sh
CARBON_RML_CMAKE_BUILD_DIR='C:\path\to\the\existing\build' ./scripts/build-rml-target <target>
```

The wrapper executes:

```powershell
cmake --build $env:CARBON_RML_CMAKE_BUILD_DIR --config Release --target <target>
```

Do not reconfigure, clean, remap, or replace that build directory unless a
changed input requires CMake regeneration. Do not create per-test build
directories. Run the clean package build only once, as part of final
`./scripts/change qualify`.

## Required change workflow

Carbon does not merge unqualified feature bytes. Every behavior change starts
with a focused failing regression and uses the repository workflow:

```sh
./scripts/change red -- <focused-test-command>
# Implement the change and make the focused test pass.
./scripts/change qualify
```

Read [`WORKFLOW.md`](WORKFLOW.md) for the full state machine and
[`qualification/README.md`](qualification/README.md) for one-time host setup.
Full qualification launches the installed Studio/RML stack and therefore needs
a configured Windows/WSL2 host. If you do not have that host, open a draft pull
request with the focused test and local results; a maintainer must run the full
gate before merge.

Reflection comes from the current Roblox API dump at runtime. Run
`./scripts/update-reflection --check` to verify that Carbon can build and apply
the current schema and its version-independent serialization policy.

### Updating the pinned Studio ABI

RML only enables its private native-layout profile for the exact Roblox Studio
executable recorded in
`rml/code/roblox_modloader/src/roblox/pinned_internals_profile.cpp`. When Studio
updates, derive and review the complete replacement profile with the offline ABI
analyzers, then update the version, file size, SHA-256 digest, offsets, and RVAs
together. Do not ship a partial profile or restore live-process layout
discovery. The replacement must pass `./scripts/test-pinned-rml-abi` and the full
`./scripts/change qualify` Studio run.

## Pull requests

Keep each pull request focused. Explain the user-visible behavior, call out
format or compatibility effects, and list the exact checks you ran. Update the
relevant documentation in the same change.

By submitting a contribution, you agree that it is licensed under the
repository's [MIT license](LICENSE.md).
