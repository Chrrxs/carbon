use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct Variables {
	values: BTreeMap<String, Value>,
}

impl Variables {
	pub fn new(values: BTreeMap<String, Value>) -> Self {
		Self { values }
	}

	pub fn insert(&mut self, name: impl Into<String>, value: Value) {
		self.values.insert(name.into(), value);
	}

	pub fn get(&self, name: &str) -> Option<&Value> {
		self.values.get(name)
	}

	pub fn resolve_string(&self, input: &str) -> Result<String> {
		let value = self.resolve_string_value(input, &mut BTreeSet::new())?;
		value_to_string(&value)
	}

	pub fn resolve_value(&self, input: &Value) -> Result<Value> {
		self.resolve_value_inner(input, &mut BTreeSet::new())
	}

	fn resolve_value_inner(&self, input: &Value, stack: &mut BTreeSet<String>) -> Result<Value> {
		match input {
			Value::String(value) => self.resolve_string_value(value, stack),
			Value::Array(values) => values
				.iter()
				.map(|value| self.resolve_value_inner(value, stack))
				.collect(),
			Value::Object(values) => values
				.iter()
				.map(|(key, value)| Ok((key.clone(), self.resolve_value_inner(value, stack)?)))
				.collect(),
			_ => Ok(input.clone()),
		}
	}

	fn resolve_string_value(&self, input: &str, stack: &mut BTreeSet<String>) -> Result<Value> {
		if let Some(name) = exact_placeholder(input) {
			return self.resolve_placeholder(name, stack);
		}

		let mut output = String::new();
		let mut rest = input;
		while let Some(start) = rest.find("${") {
			output.push_str(&rest[..start]);
			let placeholder = &rest[start + 2..];
			let Some(end) = placeholder.find('}') else {
				bail!("unterminated template placeholder in {input:?}");
			};
			let name = &placeholder[..end];
			let value = self.resolve_placeholder(name, stack)?;
			output.push_str(&value_to_string(&value)?);
			rest = &placeholder[end + 1..];
		}
		output.push_str(rest);
		Ok(Value::String(output))
	}

	fn resolve_placeholder(&self, name: &str, stack: &mut BTreeSet<String>) -> Result<Value> {
		if let Some(environment_name) = name.strip_prefix("env:") {
			let value = std::env::var(environment_name)
				.with_context(|| format!("required environment variable {environment_name:?} is not set"))?;
			return Ok(Value::String(value));
		}

		if !stack.insert(name.to_owned()) {
			bail!("cyclic template variable reference involving {name:?}");
		}
		let value = self
			.values
			.get(name)
			.with_context(|| format!("unresolved template variable {name:?}"))?;
		let resolved = self.resolve_value_inner(value, stack);
		stack.remove(name);
		resolved
	}
}

fn exact_placeholder(input: &str) -> Option<&str> {
	if input.starts_with("${") && input.ends_with('}') && !input[2..input.len() - 1].contains("${") {
		Some(&input[2..input.len() - 1])
	} else {
		None
	}
}

fn value_to_string(value: &Value) -> Result<String> {
	match value {
		Value::Null => Ok("null".to_owned()),
		Value::Bool(value) => Ok(value.to_string()),
		Value::Number(value) => Ok(value.to_string()),
		Value::String(value) => Ok(value.clone()),
		Value::Array(_) | Value::Object(_) => Ok(serde_json::to_string(value)?),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn exact_placeholders_preserve_json_types() {
		let mut values = Variables::default();
		values.insert("count", Value::from(3));
		assert_eq!(
			values.resolve_value(&Value::String("${count}".into())).unwrap(),
			Value::from(3)
		);
		assert_eq!(values.resolve_string("count=${count}").unwrap(), "count=3");
	}

	#[test]
	fn nested_variables_resolve_and_cycles_fail() {
		let mut values = Variables::default();
		values.insert("root", Value::String("/tmp/repo".into()));
		values.insert("project", Value::String("${root}/game.json".into()));
		assert_eq!(values.resolve_string("${project}").unwrap(), "/tmp/repo/game.json");
		values.insert("root", Value::String("${project}".into()));
		assert!(values
			.resolve_string("${project}")
			.unwrap_err()
			.to_string()
			.contains("cyclic"));
	}
}
