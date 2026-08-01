#!/usr/bin/env bash

CHANGE_REQUIRED_PHASES=(
	focused-regression
	rust-format
	rust-clippy
	rust-advisories
	rust-tests
	rbx-binary-tests
	workflow-tests
	studio-plugin-quality
	production-readiness
)

change_state_root() {
	printf '%s\n' "${CARBON_CHANGE_STATE_DIR:-${XDG_STATE_HOME:-${HOME}/.local/state}/carbon/qualification}"
}

change_worktree_key() {
	printf '%s\0' "$(realpath "$1")" | sha256sum | cut -c 1-16
}

change_worktree_state() {
	printf '%s/worktrees/%s\n' "$(change_state_root)" "$(change_worktree_key "$1")"
}

change_red_record() {
	printf '%s/red.json\n' "$(change_worktree_state "$1")"
}

change_receipt_path() {
	printf '%s/receipts/%s.json\n' "$(change_state_root)" "$1"
}

change_plan_fingerprint() {
	printf '%s\0' "${CHANGE_REQUIRED_PHASES[@]}" | sha256sum | cut -d ' ' -f 1
}

change_required_phases_json() {
	printf '%s\0' "${CHANGE_REQUIRED_PHASES[@]}" | jq -Rs 'split("\u0000")[:-1]'
}

verify_hashed_file() {
	local path="$1"
	local expected_sha256="$2"
	[[ -f "$path" ]] || return 1
	[[ "$(sha256sum "$path" | cut -d ' ' -f 1)" == "$expected_sha256" ]]
}

verify_change_receipt() {
	local receipt="$1"
	local expected_fingerprint="$2"
	local required_phases
	local log_path
	local log_sha256

	[[ -f "$receipt" ]] || return 1
	required_phases="$(change_required_phases_json)"
	jq -e \
		--arg fingerprint "$expected_fingerprint" \
		--arg plan_sha256 "$(change_plan_fingerprint)" \
		--argjson required_phases "$required_phases" '
		.schema_version == 5 and
		.outcome == "pass" and
		(.qualified_at | type == "string") and
		.tree_fingerprint.kind == "git-tree-content-v1" and
		.tree_fingerprint.sha256 == $fingerprint and
		(.source_fingerprint.sha256 | type == "string") and
		(.suite.path | type == "string") and
		(.suite.sha256 | type == "string") and
		(.candidate.path | type == "string") and
		(.candidate.sha256 | type == "string") and
		(.candidate.version | type == "string") and
		(.candidate.target | type == "string") and
		.components.carbon_cli.artifact_path == .candidate.path and
		.components.carbon_cli.artifact_sha256 == .candidate.sha256 and
		(.components.carbon_cli.qualification_runner_path | type == "string") and
		(.components.carbon_cli.qualification_runner_sha256 | type == "string") and
		.components.studio_plugin.source_path == "studio-plugin" and
		(.components.studio_plugin.artifact_path | type == "string") and
		(.components.studio_plugin.artifact_sha256 | type == "string") and
		(.report.path | type == "string") and
		(.report.sha256 | type == "string") and
		.workflow.plan_sha256 == $plan_sha256 and
		.workflow.red.outcome == "expected-failure" and
		(.workflow.red.exit_code | type == "number" and . != 0) and
		([.workflow.phases[].id] == $required_phases) and
		all(.workflow.phases[]; .outcome == "pass")
	' "$receipt" >/dev/null || return 1

	log_path="$(jq -er '.workflow.red.log.path' "$receipt")" || return 1
	log_sha256="$(jq -er '.workflow.red.log.sha256' "$receipt")" || return 1
	verify_hashed_file "$log_path" "$log_sha256" || return 1

	while IFS=$'\t' read -r log_path log_sha256; do
		verify_hashed_file "$log_path" "$log_sha256" || return 1
	done < <(jq -r '[
		[.suite.path, .suite.sha256],
		[.candidate.path, .candidate.sha256],
		[.components.carbon_cli.qualification_runner_path, .components.carbon_cli.qualification_runner_sha256],
		[.components.studio_plugin.artifact_path, .components.studio_plugin.artifact_sha256],
		[.report.path, .report.sha256]
	] | .[] | @tsv' "$receipt")
	[[ "$(jq -er '.outcome' "$(jq -er '.report.path' "$receipt")")" == "pass" ]] || return 1

	while IFS=$'\t' read -r log_path log_sha256; do
		verify_hashed_file "$log_path" "$log_sha256" || return 1
	done < <(jq -r '.workflow.phases[] | [.log.path, .log.sha256] | @tsv' "$receipt")
}

verify_red_record() {
	local record="$1"
	local log_path
	local log_sha256
	[[ -f "$record" ]] || return 1
	jq -e '
		.schema_version == 1 and
		.outcome == "expected-failure" and
		(.base_commit | type == "string") and
		(.tree_fingerprint | type == "string") and
		(.command | type == "array" and length > 0) and
		(.exit_code | type == "number" and . != 0)
	' "$record" >/dev/null || return 1
	log_path="$(jq -er '.log.path' "$record")" || return 1
	log_sha256="$(jq -er '.log.sha256' "$record")" || return 1
	verify_hashed_file "$log_path" "$log_sha256"
}
