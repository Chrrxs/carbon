use serde_json::Value;

pub fn warning_lines(text: &str) -> Vec<String> {
	text.lines()
		.filter(|line| is_warning_line(line))
		.map(str::to_owned)
		.collect()
}

pub fn runtime_log_failures(payload: &Value) -> Vec<String> {
	payload
		.get("entries")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
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
		.collect()
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
}
