# Carbon production-readiness qualification

`carbon-qualify` is the blocking release harness for the installed Carbon
stack. It owns Carbon commands, Studio MCP calls, background processes,
filesystem assertions, cleanup, and evidence collection in one run.

The harness is separate from `carbon`. Qualification copies the exact candidate
into an installed binary location while the runner remains outside the shipped
archive.

## Release contract

A release is qualified only when every configured scenario passes. The runner:

- executes scenarios sequentially and routes every Studio call to the selected
  managed instance
- fails successful commands that emit unexpected warning or error diagnostics
- fails every warning or error returned by Studio runtime logs
- executes cleanup after success or failure
- kills registered background processes that remain alive
- compares file and directory content, with optional modification-time checks
- retains command streams, MCP payloads, reports, and captured artifacts
- returns a nonzero exit status unless the complete suite passes

The release command verifies the resulting content-addressed receipt before it
creates the timestamped release tag.

## Host setup

Qualification runs on the configured Windows or WSL development host. It
requires Roblox Studio, a running `robloxstudio-mcp` that advertises lifecycle
protocol v3 with exact process identity, the Studio plugin toolchain, and Studio
auto-recovery enabled.

Configure the disposable fixture and MCP checkout once:

```sh
export CARBON_QUALIFY_PROJECT=/path/to/fixture/game.carbon.json
export CARBON_QUALIFY_MCP_REPO=/path/to/robloxstudio-mcp
```

The fixture's sibling `game.carbon.data` directory must exist. Other host
locations can be configured with the `CARBON_QUALIFY_*` environment variables.

## Feature qualification

After recording the focused failing regression and implementing the change, run:

```sh
./scripts/change qualify
```

This entrypoint runs Rust formatting, linting, advisory scanning and tests,
workflow tests, Studio-plugin validation, release builds, and the installed
Studio suite. A passing run issues a receipt bound to every byte in the Git
tree.

The installed suite proves three release-critical slices:

1. The exact installed candidate starts and reports its version.
2. Repeated builds produce identical place bytes without changing the canonical
   data directory.
3. A broker-managed Studio session imports an explicit binary place through
   `carbon capture`, proves Carbon can focus it through the broker-final MCP
   instance ID, authors a live change, then `carbon stop` waits for the
   continuously monitored next auto-recovery file and a rebuild retains that
   change.

The stop step has a 390-second harness timeout around Carbon's six-minute
recovery deadline. No native code is loaded into Studio and no Studio binary is
modified.

Concurrent worktrees lease separate Carbon ports and run state. Each run gives
its staged DataModel a unique name and unique managed launch, then routes focus,
all MCP probes, and shutdown by the final instance ID returned for that launch.
The opaque launch ID is retained separately as lifecycle evidence and is never
used as a Studio tool-routing ID.

Evidence and the passing receipt remain below
`${XDG_STATE_HOME:-$HOME/.local/state}/carbon/qualification`. Commit without
editing, then merge from clean `main` with:

```sh
./scripts/change merge <branch>
```

After qualified bytes are merged and pushed, publish them with:

```sh
./scripts/release
```

The release contains the Carbon executable with its embedded Studio plugin.
`update-release` can install the retained qualified CLI and plugin locally
without rebuilding them.

## Suite format

Suites are versioned JSON documents. Every scenario is required and there is no
release-mode skip mechanism.

```json
{
  "schema_version": 1,
  "name": "example",
  "policy": {
    "fail_on_command_warnings": true,
    "fail_on_runtime_warnings": true,
    "minimum_scenarios": 1,
    "required_tags": ["runtime"]
  },
  "scenarios": [
    {
      "name": "server-probe",
      "tags": ["runtime"],
      "steps": [
        {
          "name": "wait-for-studio",
          "kind": "mcp",
          "tool": "get_connected_instances",
          "poll_interval_ms": 100,
          "timeout_seconds": 60
        }
      ],
      "cleanup": []
    }
  ]
}
```

Supported step kinds are:

- `command`: run a foreground command with timeout and stream assertions
- `spawn`: start a named background process with evidence capture
- `wait_process`: wait for a named process and validate its exit code
- `terminate_process`: idempotently stop a named process
- `mcp`: call an MCP tool, poll JSON checks, select one result, and capture values
- `snapshot_path`: hash a file or directory tree
- `assert_path_unchanged`: compare a path with a prior snapshot
- `assert_numeric_delta`: enforce a numeric growth budget
- `assert_place_instance`: decode an RBXL and verify an exact instance path, class, properties, and attributes
- `sleep`: wait for a bounded interval when no observable readiness exists

JSON checks support `exists`, `absent`, `equals`, `not_equals`, `contains`,
`less_than_or_equal`, and `greater_than_or_equal` against RFC 6901 pointers.

Variables use `${name}`. Exact placeholders preserve JSON types and embedded
placeholders render as strings. `${env:NAME}` reads a required environment
variable. Built-ins include `${suite_dir}`, `${artifact_dir}`,
`${scenario}`, and `${iteration}`.

## Extending coverage

Add scenarios as vertical runtime slices: create a fixture state, perform the
user-visible operation, probe the result, retain useful evidence, and restore or
close everything in cleanup. Prefer observable readiness checks over sleeps.

Every release-critical area must have a tag listed in
`policy.required_tags`. Removing the last scenario carrying a required tag
makes validation fail before Studio launches.
