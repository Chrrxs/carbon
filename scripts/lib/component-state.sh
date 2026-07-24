#!/usr/bin/env bash

fingerprint_tree() {
	local root="$1"
	shift
	local git_root=""
	git_root="$(git -C "$root" rev-parse --show-toplevel 2>/dev/null || true)"
	# A directory below a repository can still resolve its parent's HEAD. Only
	# use Git's tracked/untracked fast path when the directory being fingerprinted
	# is the repository root itself. Artifact trees nested under an ignored build
	# directory must be hashed from their actual files.
	if [[ -n "$git_root" && "$(realpath "$root")" == "$(realpath "$git_root")" ]]; then
		(
			cd "$root"
			{
				# The committed blob IDs represent every clean input without
				# rereading its contents. Diffs and untracked files keep the
				# fingerprint content-sensitive in a dirty working tree.
				git ls-tree -r HEAD -- "$@"
				git diff --no-ext-diff --binary HEAD -- "$@"
				git ls-files --others --exclude-standard -z -- "$@" |
					sort -z |
					while IFS= read -r -d '' file; do
						printf '%s\0' "$file"
						sha256sum "$file" | cut -d ' ' -f 1
					done
			}
		) | sha256sum | cut -d ' ' -f 1
		return
	fi

	(
		cd "$root"
		{
			find "$@" -type f \
				! -path '*/.git/*' \
				! -path '*/node_modules/*' \
				! -path '*/target/*' \
				! -path '*/build/*' \
				! -path '*/dist/*' \
				! -path '*/qualification-artifacts/*' \
				-print0 2>/dev/null || true
		} |
			sort -z |
			while IFS= read -r -d '' file; do
				printf '%s\0' "$file"
				sha256sum "$file" | cut -d ' ' -f 1
			done
	) | sha256sum | cut -d ' ' -f 1
}

component_is_current() {
	local name="$1"
	local fingerprint="$2"
	shift 2
	local stamp="${state_dir}/${name}.stamp"
	local -a stamp_values
	[[ -f "$stamp" ]] || return 1
	mapfile -t stamp_values < "$stamp"
	((${#stamp_values[@]} == $# + 1)) || return 1
	[[ "${stamp_values[0]}" == "$fingerprint" ]] || return 1
	local index=1
	local file
	for file in "$@"; do
		[[ -f "$file" ]] || return 1
		[[ "${stamp_values[$index]}" == "$(sha256sum "$file" | cut -d ' ' -f 1)" ]] || return 1
		((index += 1))
	done
}

write_component_stamp() {
	local name="$1"
	local fingerprint="$2"
	shift 2
	local temporary_stamp
	mkdir -p "$state_dir"
	temporary_stamp="$(mktemp "${state_dir}/${name}.XXXXXX")"
	{
		printf '%s\n' "$fingerprint"
		local file
		for file in "$@"; do
			sha256sum "$file" | cut -d ' ' -f 1
		done
	} > "$temporary_stamp"
	mv -f "$temporary_stamp" "${state_dir}/${name}.stamp"
}

component_status() {
	if (($1 == 0)); then
		printf 'up to date'
	else
		printf 'update required'
	fi
}

qualified_component_needs_update() {
	local artifact="$1"
	local target="$2"
	[[ -f "$target" ]] || return 0
	[[ "$(sha256sum "$artifact" | cut -d ' ' -f 1)" != "$(sha256sum "$target" | cut -d ' ' -f 1)" ]]
}

install_qualified_component() {
	local artifact="$1"
	local target="$2"
	local mode="$3"
	if ! qualified_component_needs_update "$artifact" "$target"; then
		printf 'up to date'
		return
	fi

	local parent
	local temporary
	parent="$(dirname "$target")"
	mkdir -p "$parent"
	temporary="$(mktemp "${parent}/.$(basename "$target").release.XXXXXX")"
	trap 'rm -f -- "$temporary"' RETURN
	install -m "$mode" "$artifact" "$temporary"
	mv -f "$temporary" "$target"
	trap - RETURN
	printf 'updated'
}

qualified_tree_needs_update() {
	local artifact="$1"
	local target="$2"
	local expected_fingerprint="$3"
	[[ -d "$target" ]] || return 0
	[[ "$(fingerprint_tree "$target" .)" != "$expected_fingerprint" ]]
}

install_qualified_tree() {
	local artifact="$1"
	local target="$2"
	local expected_fingerprint="$3"
	if ! qualified_tree_needs_update "$artifact" "$target" "$expected_fingerprint"; then
		printf 'up to date'
		return
	fi

	local parent
	local temporary
	local previous
	parent="$(dirname "$target")"
	mkdir -p "$parent"
	temporary="$(mktemp -d "${parent}/.$(basename "$target").release.XXXXXX")"
	previous="${target}.previous.$$"
	trap 'rm -rf -- "$temporary"' RETURN
	cp -a "${artifact}/." "$temporary/"
	[[ "$(fingerprint_tree "$temporary" .)" == "$expected_fingerprint" ]] || {
		echo "qualified tree copy failed verification" >&2
		return 1
	}
	if [[ -e "$target" ]]; then
		mv "$target" "$previous"
	fi
	if ! mv "$temporary" "$target"; then
		[[ ! -e "$previous" ]] || mv "$previous" "$target"
		return 1
	fi
	[[ ! -e "$previous" ]] || rm -rf -- "$previous"
	trap - RETURN
	printf 'updated'
}
