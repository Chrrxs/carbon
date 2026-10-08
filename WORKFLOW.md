# Carbon feature workflow

Every feature follows one state machine:

```text
RED RECORDED -> IMPLEMENT -> QUALIFIED TREE -> COMMIT SAME TREE -> FF-ONLY MERGE
```

The only interface for merging is `./scripts/change`.

Qualification is the merge gate into `main`, not a prerequisite for trying a
change. While a feature is in progress, build and run an unqualified binary
from the feature worktree (see [Local unqualified builds](#local-unqualified-builds)).
Run `./scripts/change qualify` only when the tree is ready to merge.

## 1. Create a feature worktree

Start from the current local `main`:

```bash
git worktree add ../carbon-my-feature -b codex/my-feature main
cd ../carbon-my-feature
```

Keep one branch in one worktree. Do not implement directly on `main`.

## 2. Record red

Write the smallest regression test that describes the change. Run it through
the workflow and require it to fail:

```bash
./scripts/change red -- cargo test --locked -p carbon test_name -- --exact
```

Use the natural focused command for non-Rust code. `red` refuses a passing
command and records the command, nonzero exit code, tree identity, and complete
output. Do this before implementing the behavior.

## 3. Implement

Make the production change. Iterate with the focused test until it passes.
Do not remove or weaken the regression test that established red.

### Local unqualified builds

To try the change against a real project or Studio session before merging,
build a release binary from the feature worktree. Release builds embed the
Studio plugin, so build that first with the same version:

```bash
export CARBON_BUILD_VERSION="$(./scripts/build-version)"
./scripts/build-studio-plugin target/local/Carbon.rbxm
CARBON_STUDIO_PLUGIN_BUNDLE="$PWD/target/local/Carbon.rbxm" \
	cargo build --locked --release --bin carbon
./target/release/carbon --version
```

Run it by path, for example `./target/release/carbon serve game.carbon.json`.
The binary is unqualified: it needs no receipt and produces none, and it never
replaces the installed CLI. Only `./scripts/update-release` updates the
installed CLI and plugin, and it accepts only qualified artifacts. Build and
test like this as often as you need; qualification does not run here.

## 4. Qualify the complete tree before merging

Qualify once the feature is finished and ready to merge into `main`. Run one
command:

```bash
./scripts/change qualify
```

Qualification first reruns the recorded regression and then requires every
phase to pass:

1. Rust formatting
2. Rust linting with warnings denied
3. RustSec advisory scanning with the pinned `cargo-audit` release
4. All locked Rust targets and tests
5. Workflow, installer, and isolation tests
6. Studio-plugin reflection validation, linting, and formatting
7. Release CLI and Studio plugin builds
8. The installed-stack Studio qualification suite, including auto-recovery capture

The repository is hashed before and after qualification. Any changed byte
invalidates the run. A passing receipt is stored by the complete Git-tree
fingerprint under:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/carbon/qualification/receipts/
```

Build outputs and qualification evidence are Git-ignored; every mergeable byte,
including documentation and workflows, is covered by the receipt.

Inspect the current state at any time:

```bash
./scripts/change status
```

## 5. Commit the same tree

After qualification, do not edit files:

```bash
git add -A
git commit -m "Describe the feature"
./scripts/change status HEAD
```

Committing identical bytes preserves the tree fingerprint and its receipt.

## 6. Fast-forward merge

Return to the clean `main` worktree and merge through the workflow:

```bash
cd /path/to/carbon
./scripts/change merge codex/my-feature
```

`merge` refuses unless:

- it is running from clean local `main`;
- the feature worktree has no uncommitted changes;
- the feature branch is a fast-forward of `main`;
- the branch's complete tree has a valid passing receipt;
- the red record and every required phase log still match their hashes.

The merge is `--ff-only` and receives a `carbon-qualification` Git note that
identifies the exact receipt. If `main` advanced, rebase the feature branch onto
`main`, rerun `./scripts/change qualify`, commit the resulting tree, and merge
again.

Never use `git merge` directly for Carbon feature branches.

## Release

Feature merging and release publication are separate. After qualified changes
are on clean, pushed `main`, publish a stable version without rebuilding or
changing the local installation:

```bash
./scripts/release
```

The qualified executable already carries its Eastern `YY.M.DHHMM` build
timestamp, such as `26.7.231935`. The command verifies the complete receipt and
remote `main`, pushes the matching `v26.7.231935` tag, and publishes that exact
installed-stack-qualified executable as the Rokit-compatible
`carbon-26.7.231935-linux-x86_64.gz` asset. The publication workflow also
builds `carbon-26.7.231935-windows-x86_64.zip` from the immutable tag on a
native Windows runner, verifies its embedded version, and then verifies both
published assets. Pull requests compile and test every Rust target on both
Linux and Windows.
