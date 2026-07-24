#!/usr/bin/env bash

release_version_from_build() {
	local version="${1:-}"
	[[ "$version" =~ ^(0|[1-9][0-9]?)\.(0|[1-9][0-9]?)\.([1-9][0-9]{4,5})$ ]] || return 1
	local short_year=$((10#${BASH_REMATCH[1]}))
	local year=$((2000 + short_year))
	local month=$((10#${BASH_REMATCH[2]}))
	local stamp="${BASH_REMATCH[3]}"
	local day_length=$((${#stamp} - 4))
	local day=$((10#${stamp:0:day_length}))
	local hour=$((10#${stamp:day_length:2}))
	local minute=$((10#${stamp:day_length + 2:2}))
	((month >= 1 && month <= 12 && day >= 1 && day <= 31 && hour <= 23 && minute <= 59)) || return 1
	local observed
	observed="$(
		TZ=America/New_York date -d \
			"$(printf '%04d-%02d-%02dT%02d:%02d' "$year" "$month" "$day" "$hour" "$minute")" \
			'+%Y-%m-%dT%H:%M' 2>/dev/null
	)" || return 1
	[[ "$observed" == "$(printf '%04d-%02d-%02dT%02d:%02d' "$year" "$month" "$day" "$hour" "$minute")" ]] ||
		return 1
	local canonical
	canonical="$(printf '%d.%d.%d%02d%02d' "$short_year" "$month" "$day" "$hour" "$minute")"
	[[ "$canonical" == "$version" ]] || return 1
	printf '%s\n' "$version"
}

canonical_release_tag() {
	local version="${1:-}"
	release_version_from_build "$version" >/dev/null || return 1
	printf 'v%s\n' "$version"
}

release_version_from_tag() {
	local tag="${1:-}"
	[[ "$tag" == v* ]] || return 1
	release_version_from_build "${tag#v}"
}
