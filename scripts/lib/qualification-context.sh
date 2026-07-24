#!/usr/bin/env bash

# One qualification run owns only the values exported by this module. Global
# installed components remain outside the context and are coordinated by the
# caller's stack lock.

qualification_worktree_key() {
	local repository="$1"
	printf '%s\0' "$(realpath "$repository")" | sha256sum | cut -c 1-16
}

qualification_rml_build_dir() {
	local repository="$1"
	local windows_cache_root="${2:-/mnt/c/cq}"
	printf '%s/carbon-rml-builds/%s/carbon-rml-build\n' \
		"${windows_cache_root%/}" "$(qualification_worktree_key "$repository")"
}

qualification_context_init() {
	local state_root="$1"
	local repository="$2"
	local requested_port="${3:-auto}"
	local uuid

	uuid="$(tr -d '-' < /proc/sys/kernel/random/uuid 2>/dev/null || true)"
	if [[ -z "$uuid" ]]; then
		uuid="$(date -u +%s%N)-$$-${RANDOM}"
	fi
	QUALIFICATION_RUN_ID="${uuid:0:20}"
	QUALIFICATION_WORKTREE_KEY="$(qualification_worktree_key "$repository")"
	QUALIFICATION_WORKTREE_STATE="${state_root}/worktrees/${QUALIFICATION_WORKTREE_KEY}"
	QUALIFICATION_OWNER_PID="$BASHPID"
	QUALIFICATION_PORT_LEASE=""
	QUALIFICATION_PORT=""
	mkdir -p "${state_root}/ports" "$QUALIFICATION_WORKTREE_STATE"

	local first_port
	local attempts
	if [[ -n "$requested_port" && "$requested_port" != "auto" && "$requested_port" != "0" ]]; then
		[[ "$requested_port" =~ ^[0-9]+$ ]] && ((requested_port > 0 && requested_port < 65536)) || return 2
		first_port="$requested_port"
		attempts=1
	else
		# The hash distributes simultaneous worktrees across the dynamic/private
		# range. Atomic lease directories resolve collisions without a coordinator.
		first_port=$((49152 + 0x${QUALIFICATION_RUN_ID:0:4} % 16384))
		attempts=16384
	fi

	local offset
	local port
	local lease
	local owner_pid
	local stale_lease
	for ((offset = 0; offset < attempts; offset += 1)); do
		port=$((49152 + (first_port - 49152 + offset) % 16384))
		if ((attempts == 1)); then
			port="$first_port"
		fi
		lease="${state_root}/ports/${port}.lease"
		if ! mkdir "$lease" 2>/dev/null; then
			# SIGKILL cannot run the normal trap. Reclaim only a lease whose owning
			# process is definitely gone; the atomic rename lets one contender win.
			owner_pid=""
			[[ -f "${lease}/owner" ]] && IFS= read -r owner_pid < "${lease}/owner"
			if [[ -n "$owner_pid" ]] && ! kill -0 "$owner_pid" 2>/dev/null; then
				stale_lease="${lease}.stale-${QUALIFICATION_RUN_ID}"
				if mv "$lease" "$stale_lease" 2>/dev/null; then
					rm -f -- "${stale_lease}/owner"
					rmdir -- "$stale_lease"
					mkdir "$lease" 2>/dev/null || continue
				else
					continue
				fi
			else
				continue
			fi
		fi
		printf '%s\n%s\n%s\n' "$QUALIFICATION_OWNER_PID" "$QUALIFICATION_RUN_ID" "$(realpath "$repository")" > "${lease}/owner"
		if lsof -nP -iTCP:"$port" -sTCP:LISTEN -t 2>/dev/null | grep -q .; then
			rm -f -- "${lease}/owner"
			rmdir -- "$lease"
			continue
		fi
		QUALIFICATION_PORT="$port"
		QUALIFICATION_PORT_LEASE="$lease"
		export QUALIFICATION_RUN_ID QUALIFICATION_WORKTREE_KEY QUALIFICATION_WORKTREE_STATE QUALIFICATION_OWNER_PID
		export QUALIFICATION_PORT QUALIFICATION_PORT_LEASE
		return 0
	done
	return 1
}

qualification_context_release() {
	local lease="${QUALIFICATION_PORT_LEASE:-}"
	[[ -n "$lease" && -d "$lease" ]] || return 0
	local owner_pid=""
	local owner_run=""
	if [[ -f "${lease}/owner" ]]; then
		{
			IFS= read -r owner_pid
			IFS= read -r owner_run
		} < "${lease}/owner"
	fi
	[[ "$owner_pid" == "${QUALIFICATION_OWNER_PID:-$BASHPID}" && "$owner_run" == "${QUALIFICATION_RUN_ID:-}" ]] || return 1
	rm -f -- "${lease}/owner"
	rmdir -- "$lease"
	QUALIFICATION_PORT_LEASE=""
}
