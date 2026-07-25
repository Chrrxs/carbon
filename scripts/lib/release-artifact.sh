#!/usr/bin/env bash

qualified_release_asset_name() {
	(($# == 1)) || {
		echo "qualified_release_asset_name: expected VERSION" >&2
		return 2
	}
	local version="$1"
	[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.([1-9][0-9]*)$ ]] || {
		echo "qualified_release_asset_name: invalid version: ${version}" >&2
		return 2
	}
	printf 'carbon-%s-linux-x86_64.gz\n' "$version"
}

windows_release_asset_name() {
	(($# == 1)) || {
		echo "windows_release_asset_name: expected VERSION" >&2
		return 2
	}
	local version="$1"
	[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.([1-9][0-9]*)$ ]] || {
		echo "windows_release_asset_name: invalid version: ${version}" >&2
		return 2
	}
	printf 'carbon-%s-windows-x86_64.zip\n' "$version"
}

prepare_qualified_release_asset() {
	(($# == 3)) || {
		echo "prepare_qualified_release_asset: expected CANDIDATE RECEIPT OUTPUT" >&2
		return 2
	}
	local candidate="$1"
	local receipt="$2"
	local output_dir="$3"
	local asset_name
	local output
	local expected_candidate_sha256
	local candidate_sha256
	local candidate_version
	local receipt_version
	local candidate_target

	[[ -x "$candidate" ]] || {
		echo "prepare_qualified_release_asset: candidate is not executable: ${candidate}" >&2
		return 2
	}
	[[ -f "$receipt" ]] || {
		echo "prepare_qualified_release_asset: receipt is unavailable: ${receipt}" >&2
		return 2
	}
	command -v gzip >/dev/null 2>&1 || {
		echo "prepare_qualified_release_asset: gzip is required" >&2
		return 2
	}

	expected_candidate_sha256="$(jq -er '.candidate.sha256' "$receipt")" || return 2
	candidate_sha256="$(sha256sum "$candidate" | cut -d ' ' -f 1)"
	[[ "$candidate_sha256" == "$expected_candidate_sha256" ]] || {
		echo "prepare_qualified_release_asset: candidate bytes do not match the qualification receipt" >&2
		return 2
	}
	candidate_target="$(jq -er '.candidate.target' "$receipt")" || return 2
	[[ "$candidate_target" == "x86_64-unknown-linux-gnu" ]] || {
		echo "prepare_qualified_release_asset: unsupported candidate target: ${candidate_target}" >&2
		return 2
	}
	candidate_version="$("$candidate" --color never --version | awk '{print $NF}')" || return 2
	receipt_version="$(jq -er '.candidate.version' "$receipt")" || return 2
	[[ "$candidate_version" == "$receipt_version" ]] || {
		echo "prepare_qualified_release_asset: candidate version does not match the qualification receipt" >&2
		return 2
	}
	asset_name="$(qualified_release_asset_name "$candidate_version")" || return 2
	output="${output_dir}/${asset_name}"

	mkdir -p "$output_dir"
	rm -f -- "$output"
	gzip --no-name --best --stdout -- "$candidate" > "$output"
	chmod 0644 "$output"
	gzip --test -- "$output" || {
		echo "prepare_qualified_release_asset: staged gzip is invalid" >&2
		return 2
	}
	gzip --decompress --stdout -- "$output" | cmp -s - "$candidate" || {
		echo "prepare_qualified_release_asset: staged gzip does not contain the qualified candidate" >&2
		return 2
	}
}
