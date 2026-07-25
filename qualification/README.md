# Carbon production-readiness qualification

`carbon-qualify` is the Rust-native, blocking release harness for the installed Carbon stack. It owns Carbon commands, Studio MCP calls, background processes, fixed-point filesystem assertions, cleanup, evidence, and release policy in one run.

The harness is deliberately separate from `carbon`. A release tests the exact candidate copied into an installed `bin` location while the runner remains outside the shipped archive.

## Release contract

A release is qualified only when all configured scenarios pass. The runner:

- executes scenarios sequentially within a run while concurrent runs share the routed MCP endpoint safely;
- fails successful commands that emit unexpected warning, error, fatal, or `Promise.Error` diagnostics;
- fails every warning or error returned by `get_runtime_logs`;
- executes cleanup after both success and failure and treats non-allowed cleanup failures as release failures;
- kills any registered background process still alive when the runner exits;
- supports selecting one matching element from an MCP array, so a run captures its own `instanceId` without depending on array order;
- checks exact file and directory content, with optional per-entry modification-time stability;
- writes `report.json`, `junit.xml`, command streams, MCP payloads, profiler output, and images to one artifact directory; and
- returns a nonzero exit status unless the complete suite passes.

The release command verifies a passing receipt before it creates the
SemVer-compatible tag that starts `.github/workflows/release.yml`. The unpadded
Eastern timestamp is Carbon's product version; a separate content-derived
identity keeps internal components exact.

## Host setup

Qualification runs from the configured WSL/Windows development host through
the single feature workflow documented in [`WORKFLOW.md`](../WORKFLOW.md). The
host must provide Roblox Studio, robloxstudio-mcp, the Studio profiler and
EditableImage capabilities, and permission to read RML logs and crash dumps.
Carbon builds and installs its own Studio plugin and matching RML package.

Configure the disposable fixture and the robloxstudio-mcp checkout once for the
host:

```sh
export CARBON_QUALIFY_PROJECT=/path/to/fixture/game.carbon.json
export CARBON_QUALIFY_MCP_REPO=/path/to/robloxstudio-mcp
```

The fixture's sibling `game.carbon.data` directory must exist. Other host
locations can be configured with the `CARBON_QUALIFY_*` environment variables;
feature authors do not pass qualification options.

## Feature qualification

After recording red and implementing the change, run:

```sh
./scripts/change qualify
```

This is the only supported qualification entrypoint. It runs formatting,
linting, a RustSec advisory scan, every Rust and managed test, workflow tests,
plugin validation, release builds, and this installed-stack suite before
issuing a full-tree receipt. Cached build outputs remain incremental, but tests
run for every qualified tree.

The managed-Studio scenario validates the complete saved place, not only the
capture protocol. Once two explicit captures prove a filesystem fixed point,
RML queues Studio's native local **File > Save to File** action against the
disposable place opened by `serve`. The runner downloads that RBXL as
`studio-saved.rbxl`, builds the captured source as `capture-rebuilt.rbxl`, and
requires `carbon diff` to find zero gameplay-affecting or unexplained
differences. The export route is available only when `serve` receives the
runner's generated qualification token; it has no destination-path argument
and does not call Roblox's cloud save API.

The managed-Studio scenario also creates a live manifest-owned `ObjectValue`
reference to the mapped `ReplicatedStorage.Shared` directory, captures it,
verifies that capture created identity metadata, and requires the rebuilt place
to match Studio. An isolated clone then removes that ID and must fail its build
with a diagnostic telling the user to modify the relevant manifest reference.
The live Studio reference is cleared and recaptured; a clone of that corrected
manifest must build without the ID and retain native-save parity. The clones
keep those intentionally invalid source edits out of the active serve session.

The same managed session exercises Roblox `ProceduralModel.Generator` with a
generator `ModuleScript` inside the mapped `ReplicatedStorage.Shared` source.
Two procedural models share that generator, a third keeps the default `nil`
generator, and generation is awaited before capture. Capture must append the
generator's ID without replacing its existing `meta.json` attributes, preserve
both references, and retain native-save parity. Isolated clones prove that a
missing generator ID blocks builds while either reference remains. The live
models then clear their generators one at a time: generated contents must be
preserved, the first clear must still leave the build blocked without the ID,
and clearing the final reference must make the ID unnecessary. These checks
follow Roblox's documented generator assignment, asynchronous generation, and
clear-preserves-content contracts.

Warm runs from multiple worktrees may execute simultaneously. Each run leases
an unused Carbon port, gives its staged DataModel a unique name, runs immutable
private copies of its Carbon CLI and qualifier, and selects that DataModel from
`get_connected_instances` before routing every Studio probe by `instance_id`.
The MCP listener and installed Studio stack are shared: warm runs hold shared
leases, while stale plugin/MCP/RML updates take a short exclusive lease. Studio
only needs to be closed when RobloxModLoader's loaded binaries actually require
replacement. The conventional `~/.local/bin/carbon` is still updated atomically
for other projects, but changing it cannot alter a qualification already in
flight. Teardown is equally scoped: qualification forces the advertised MCP
lifecycle protocol, verifies its launch through both `instance_id` and the
captured `launch_id`, and requires `carbon stop --port ...` to capture before
closing that exact managed launch. No title matching, direct fallback, or global
Studio shutdown is involved.

After a passing run, immutable evidence remains below
`${XDG_STATE_HOME:-$HOME/.local/state}/carbon/qualification`. The receipt is
addressed by the complete Git tree, so another worktree can verify the same
committed bytes. It binds the red failure, every phase log, release artifacts,
suite, report, target triple, product fingerprint, and automatic
`YY.M.DHHMM` version and its hidden content-derived identity. Commit without
editing, then use
`./scripts/change merge <branch>` from clean `main`.

After qualified bytes are merged and pushed, release them with one command:

```sh
./scripts/release
```

`release` never calls Cargo, CMake, dotnet, Wally, or Rojo and never changes the
local installation. The final checkout must be clean, match remote `main`, and
have the exact content-addressed receipt. It takes the version from the retained
WSL/Linux x86_64 candidate, pushes the matching tag, and publishes the exact
executable as the deterministic Rokit-compatible
`carbon-<version>-linux-x86_64.gz` asset. That executable embeds the exact RML
package and Studio plugin exercised by qualification. The
publication-triggered workflow builds a native
`carbon-<version>-windows-x86_64.zip` from the same immutable tag, reruns the
RML build's native and managed checks, verifies the Windows executable version,
and then verifies both published assets. `update-release` is an optional,
separate local installer for the retained qualified artifacts.

The MCP token is loaded from `ROBLOX_STUDIO_AUTH_TOKEN`, the compatibility override `ROBLOX_STUDIO_MCP_AUTH_TOKEN`, `ROBLOX_STUDIO_MCP_AUTH_TOKEN_FILE`, or the standard `~/.robloxstudio-mcp/auth-token` path. Tokens are never written to reports.

## Suite format

Suites are versioned JSON documents. Every scenario is required; there is no release-mode skip mechanism.

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
          "timeout_seconds": 60,
          "select": {
            "pointer": "/instances",
            "checks": [
              {"pointer": "/dataModelName", "op": "equals", "value": "${studio_data_model}"},
              {"pointer": "/role", "op": "equals", "value": "edit"}
            ]
          },
          "capture": {
            "studio_instance": "/instanceId"
          }
        }
      ],
      "cleanup": []
    }
  ]
}
```

Supported step kinds are:

- `command`: run a foreground command with timeout, exit-code and stream assertions; expected failing commands may allow only explicitly matched diagnostic lines through `allowed_diagnostic_contains`, while every unrelated warning or error remains fatal;
- `spawn`: start a named background process with output redirected into evidence;
- `wait_process`: require a named background process to exit with one of its configured `expected_exit_codes` (default `[0]`);
- `terminate_process`: idempotently kill a residual named process;
- `mcp`: call any robloxstudio-mcp tool, optionally poll JSON checks, select exactly one matching array element, and capture values;
- `export_studio_place`: authenticate to a qualification-enabled `serve`, queue Studio's native local save, and stream the resulting disposable RBXL into evidence;
- `snapshot_path`: hash a file or directory tree and record every entry's mtime;
- `assert_path_unchanged`: compare content and, by default, mtimes with a prior snapshot;
- `start_crash_watch` / `assert_no_crash`: fail on new RobloxModLoader dumps and retain matching exception/module/address/stack evidence;
- `assert_numeric_delta`: enforce a numeric growth budget between two values captured from prior MCP results;
- `sleep`: a bounded delay for the rare case where no observable readiness state exists.

JSON checks support `exists`, `absent`, `equals`, `not_equals`, `contains`, `less_than_or_equal`, and `greater_than_or_equal` against RFC 6901 JSON pointers. An MCP `select` block points to an array and applies its checks to each element; exactly one must match, and captures are then relative to that selected element. Check values may contain suite variables.

Variables use `${name}`. Exact placeholders preserve JSON types; embedded placeholders render as strings. `${env:NAME}` reads a required environment variable. Values captured by MCP steps are scoped to that scenario iteration. Built-ins are `${suite_dir}`, `${artifact_dir}`, `${scenario}`, and `${iteration}`.

## Extending coverage

Add scenarios as vertical runtime slices: construct or select a fixture, perform the user-visible operation, probe server/client state, collect logs and resource evidence, then restore or close everything in `cleanup`. Prefer MCP-observable readiness conditions over sleeps.

Every new release-critical area must be represented by a tag listed in `policy.required_tags`. Removing the last scenario carrying such a tag makes suite validation fail before Studio launches.

Studio qualification is necessary but cannot prove Roblox cloud behavior. DataStore, MemoryStore, MessagingService, real server allocation, production throttling, and regional networking still require a private published-universe canary. That canary should publish its own blocking evidence before the GitHub release workflow is authorized.
