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

Generated reflection data must remain reproducible. Do not hand-edit
`studio-plugin/src/Lib/Dom/database.luau`; use the generator documented by the
plugin toolchain.

## Pull requests

Keep each pull request focused. Explain the user-visible behavior, call out
format or compatibility effects, and list the exact checks you ran. Update the
relevant documentation in the same change.

By submitting a contribution, you agree that it is licensed under the
repository's [MIT license](LICENSE.md).
