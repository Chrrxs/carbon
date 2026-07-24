#!/usr/bin/env bash

# Hash the release-relevant working-tree contents independently of whether the
# same bytes are committed, staged, or untracked. This lets qualification run
# before the release-input commit without weakening the later identity check.
release_source_fingerprint() {
	local root="$1"
	shift
	local temporary_directory
	local temporary_index
	local tree
	temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/carbon-release-index.XXXXXXXX")"
	temporary_index="${temporary_directory}/index"
	if ! (
		cd "$root"
		GIT_INDEX_FILE="$temporary_index" git read-tree --empty
		GIT_INDEX_FILE="$temporary_index" git add -A -- "$@"
		GIT_INDEX_FILE="$temporary_index" git write-tree
	) > "${temporary_directory}/tree"; then
		rm -rf -- "$temporary_directory"
		return 1
	fi
	tree="$(tail -n 1 "${temporary_directory}/tree")"
	rm -rf -- "$temporary_directory"
	printf '%s\n' "$tree" | sha256sum | cut -d ' ' -f 1
}

# Hash every mergeable byte in the current repository state. Git-ignored build
# outputs and qualification evidence are intentionally outside the tree.
repository_tree_fingerprint() {
	release_source_fingerprint "$1" .
}

# Compute the same fingerprint for a committed revision without checking it
# out. This is the identity used by the qualified fast-forward merge gate.
revision_tree_fingerprint() {
	local root="$1"
	local revision="$2"
	local tree
	tree="$(git -C "$root" rev-parse "${revision}^{tree}")"
	printf '%s\n' "$tree" | sha256sum | cut -d ' ' -f 1
}
