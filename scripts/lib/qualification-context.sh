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

qualification_http_is_healthy() {
	local url="$1"
	curl --silent --fail --connect-timeout 0.2 --max-time 0.5 "$url" >/dev/null 2>&1
}

qualification_mcp_server_can_be_reused() {
	local studio_plugin_updated="$1"
	local mcp_server_rebuilt="$2"
	local mcp_server_healthy="$3"
	((studio_plugin_updated == 0 && mcp_server_rebuilt == 0 && mcp_server_healthy == 1))
}

qualification_process_belongs_to_repository() {
	local process_id="$1"
	local repository="$2"
	local repository_path
	local process_cwd
	local process_command

	[[ "$process_id" =~ ^[0-9]+$ && -r "/proc/${process_id}/cmdline" ]] || return 1
	repository_path="$(realpath "$repository" 2>/dev/null)" || return 1
	process_cwd="$(realpath "/proc/${process_id}/cwd" 2>/dev/null || true)"
	if [[ "$process_cwd" == "$repository_path" || "$process_cwd" == "${repository_path}/"* ]]; then
		return 0
	fi
	process_command="$(tr '\0' ' ' < "/proc/${process_id}/cmdline")"
	[[ "$process_command" == *"$repository_path"* ]]
}

qualification_process_descends_from() {
	local process_id="$1"
	local ancestor_id="$2"
	local parent_id
	local depth

	[[ "$process_id" =~ ^[0-9]+$ && "$ancestor_id" =~ ^[0-9]+$ ]] || return 1
	for ((depth = 0; depth < 64; depth += 1)); do
		[[ "$process_id" == "$ancestor_id" ]] && return 0
		[[ -r "/proc/${process_id}/status" ]] || return 1
		parent_id="$(ps -o ppid= -p "$process_id" 2>/dev/null | tr -d ' ')"
		[[ "$parent_id" =~ ^[0-9]+$ && "$parent_id" != "$process_id" && "$parent_id" != 0 ]] || return 1
		process_id="$parent_id"
	done
	return 1
}

qualification_stop_process_tree() {
	local root_id="$1"
	local listener_id="$2"
	local process_group
	local attempt

	[[ "$root_id" =~ ^[0-9]+$ && "$listener_id" =~ ^[0-9]+$ ]] || return 1
	process_group="$(ps -o pgid= -p "$root_id" 2>/dev/null | tr -d ' ')"
	if [[ "$process_group" == "$root_id" ]]; then
		kill -TERM -- "-${process_group}" 2>/dev/null || true
	else
		kill -TERM "$listener_id" 2>/dev/null || true
		if [[ "$root_id" != "$listener_id" ]]; then
			kill -TERM "$root_id" 2>/dev/null || true
		fi
	fi

	for ((attempt = 0; attempt < 100; attempt += 1)); do
		if ! kill -0 "$root_id" 2>/dev/null && ! kill -0 "$listener_id" 2>/dev/null; then
			return 0
		fi
		sleep 0.05
	done
	return 1
}

qualification_wait_for_http_stability() {
	local url="$1"
	local required_successes="${2:-30}"
	local max_attempts="${3:-300}"
	local delay_seconds="${4:-0.1}"
	local attempt
	local consecutive_successes=0

	for ((attempt = 1; attempt <= max_attempts; attempt += 1)); do
		if qualification_http_is_healthy "$url"; then
			consecutive_successes=$((consecutive_successes + 1))
			if ((consecutive_successes >= required_successes)); then
				return 0
			fi
		else
			consecutive_successes=0
		fi
		if ((attempt < max_attempts)); then
			sleep "$delay_seconds"
		fi
	done
	return 1
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
