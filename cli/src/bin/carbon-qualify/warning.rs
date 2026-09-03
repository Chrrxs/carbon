use serde_json::Value;

pub fn warning_lines(text: &str) -> Vec<String> {
	text.lines()
		.filter(|line| is_warning_line(line))
		.map(str::to_owned)
		.collect()
}

pub fn runtime_log_failures(payload: &Value) -> Vec<String> {
	let direct_entries = payload.get("entries").and_then(Value::as_array).into_iter().flatten();
	let instances = payload.get("instances").and_then(Value::as_array);
	let multiplayer_entries = instances
		.into_iter()
		.flatten()
		.flat_map(|instance| instance.get("entries").and_then(Value::as_array).into_iter().flatten());
	let mut failures = direct_entries
		.chain(multiplayer_entries)
		.filter_map(|entry| {
			let level = entry
				.get("level")
				.and_then(Value::as_str)
				.unwrap_or_default()
				.to_ascii_uppercase();
			let message = entry
				.get("message")
				.or_else(|| entry.get("text"))
				.and_then(Value::as_str)
				.unwrap_or_default();
			if matches!(level.as_str(), "WARN" | "WARNING" | "ERROR" | "FATAL") || is_warning_line(message) {
				Some(format!("[{level}] {message}"))
			} else {
				None
			}
		})
		.collect();
	append_runtime_log_errors(&mut failures, payload);
	for instance in instances.into_iter().flatten() {
		append_runtime_log_errors(&mut failures, instance);
	}
	failures
}

fn append_runtime_log_errors(failures: &mut Vec<String>, result: &Value) {
	let instance_id = result
		.get("instanceId")
		.and_then(Value::as_str)
		.unwrap_or("unknown instance");
	if let Some(error) = result.get("error").and_then(Value::as_str) {
		failures.push(format!("[ERROR] {instance_id}: {error}"));
	}
	for peer_error in result.get("peerErrors").and_then(Value::as_array).into_iter().flatten() {
		let Some(error) = peer_error.get("error").and_then(Value::as_str) else {
			continue;
		};
		let peer_id = peer_error
			.get("peerId")
			.and_then(Value::as_str)
			.unwrap_or("unknown peer");
		let role = peer_error.get("role").and_then(Value::as_str).unwrap_or("unknown role");
		failures.push(format!("[ERROR] {instance_id} {role} ({peer_id}): {error}"));
	}
}

fn is_warning_line(line: &str) -> bool {
	let mut normalized = line.to_ascii_lowercase();
	for clean in [
		"0 warnings",
		"0 warning",
		"no warnings",
		"no warning",
		"0 errors",
		"0 error",
		"no errors",
		"no error",
	] {
		normalized = normalized.replace(clean, "");
	}
	if normalized.contains("promise.error(") {
		return true;
	}
	normalized
		.split(|character: char| !character.is_ascii_alphanumeric())
		.any(|word| matches!(word, "warn" | "warning" | "warnings" | "error" | "errors" | "fatal"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn clean_summaries_are_not_failures() {
		assert!(warning_lines("Results: 0 errors, 0 warnings").is_empty());
		assert_eq!(
			warning_lines("WARNING: something happened"),
			["WARNING: something happened"]
		);
	}

	#[test]
	fn all_runtime_warning_encodings_fail() {
		let payload = serde_json::json!({"entries": [
			{"level": "WARN", "message": "plain"},
			{"level": "OUT", "message": "Promise.Error(synthetic)"},
			{"level": "OUT", "message": "Results: 0 errors, 0 warnings"}
		]});
		assert_eq!(runtime_log_failures(&payload).len(), 2);
	}

	#[test]
	fn multiplayer_runtime_failures_are_reported() {
		let payload = serde_json::json!({
			"multiplayerGroupId": "multiplayer:qualification",
			"instances": [
				{
					"instanceId": "instance:qualification-server",
					"entries": [{"level": "OUT", "message": "server ready"}],
					"peerErrors": [{
						"peerId": "peer:qualification-server",
						"role": "server",
						"error": "server peer disconnected",
					}],
				},
				{
					"instanceId": "instance:qualification-client",
					"error": "Every connected Peer failed to read its runtime log buffer.",
					"peerErrors": [{
						"peerId": "peer:qualification-client",
						"role": "client-1",
						"error": "client disconnected",
					}],
				},
				{
					"instanceId": "instance:qualification-other-client",
					"entries": [{"level": "ERROR", "message": "client failed"}],
				},
			],
			"nextCursorByInstance": {},
		});

		let failures = runtime_log_failures(&payload);
		assert_eq!(failures.len(), 4);
		for expected in [
			"client failed",
			"server peer disconnected",
			"Every connected Peer failed",
			"client disconnected",
		] {
			assert!(failures.iter().any(|failure| failure.contains(expected)));
		}
	}

	#[test]
	fn direct_runtime_peer_read_errors_are_reported() {
		let payload = serde_json::json!({
			"instanceId": "instance:qualification-edit",
			"entries": [],
			"peerErrors": [{
				"peerId": "peer:qualification-edit",
				"role": "edit",
				"error": "edit peer disconnected",
			}],
		});

		assert_eq!(
			runtime_log_failures(&payload),
			["[ERROR] instance:qualification-edit edit (peer:qualification-edit): edit peer disconnected"]
		);
	}
}
