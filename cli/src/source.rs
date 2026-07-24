use anyhow::{bail, ensure, Context, Result};
use rbx_dom_weak::types::Ref;
use serde::Serialize;
use std::{fs, path::PathBuf};

use crate::{ext::PathExt, project};

/// Resolve the one canonical whole-place source manifest.
pub fn resolve(path: PathBuf) -> Result<PathBuf> {
	let path = path.resolve()?;
	if path.is_file() {
		ensure!(
			project::is_project_path(&path),
			"unsupported source {}; expected a strict .carbon.json project",
			path.display()
		);
		return Ok(path);
	}

	let directory = if path.exists() {
		ensure!(
			path.is_dir(),
			"source path is not a file or directory: {}",
			path.display()
		);
		path
	} else {
		bail!("source path does not exist: {}", path.display());
	};

	let mut manifests = fs::read_dir(&directory)
		.with_context(|| format!("failed to read source directory {}", directory.display()))?
		.filter_map(|entry| entry.ok().map(|entry| entry.path()))
		.filter(|path| path.is_file() && project::is_project_path(path))
		.collect::<Vec<_>>();
	manifests.sort();
	match manifests.len() {
		1 => Ok(manifests.pop().unwrap()),
		0 => bail!("no .carbon.json project found in {}", directory.display()),
		count => bail!(
			"found {count} .carbon.json projects in {}; pass one explicitly",
			directory.display()
		),
	}
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDetails {
	version: String,
	protocol_version: u32,
	name: String,
	root_refs: Vec<Ref>,
	mapped_root_refs: Vec<Ref>,
	is_place: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	source_root_ref: Option<Ref>,
	#[serde(skip_serializing_if = "Option::is_none")]
	source_generation: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	worktree_id: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	session_token: Option<String>,
}

impl SourceDetails {
	pub fn new(name: String, root_refs: Vec<Ref>, is_place: bool) -> Self {
		Self {
			version: env!("CARBON_BUILD_VERSION").to_owned(),
			protocol_version: 4,
			name,
			root_refs,
			mapped_root_refs: Vec::new(),
			is_place,
			source_root_ref: None,
			source_generation: None,
			worktree_id: None,
			session_token: None,
		}
	}

	pub fn with_source_root_ref(mut self, source_root_ref: Ref) -> Self {
		self.source_root_ref = Some(source_root_ref);
		self
	}

	pub fn with_mapped_root_refs(mut self, mapped_root_refs: Vec<Ref>) -> Self {
		self.mapped_root_refs = mapped_root_refs;
		self
	}

	pub fn with_source_generation(mut self, generation: String) -> Self {
		self.source_generation = Some(generation);
		self
	}

	pub fn with_worktree(mut self, worktree_id: String, session_token: String) -> Self {
		self.worktree_id = Some(worktree_id);
		self.session_token = Some(session_token);
		self
	}

	pub fn with_session_token(mut self, session_token: String) -> Self {
		self.session_token = Some(session_token);
		self
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{SystemTime, UNIX_EPOCH};

	fn temporary_directory(name: &str) -> PathBuf {
		let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		std::env::temp_dir().join(format!("carbon-{name}-{unique}"))
	}

	#[test]
	fn source_details_publish_protocol_four_mapping_barriers() {
		let mapped = vec![Ref::new(), Ref::new()];
		let details = SourceDetails::new("Mapped".to_owned(), vec![], true).with_mapped_root_refs(mapped.clone());
		assert_eq!(details.protocol_version, 4);
		assert_eq!(details.mapped_root_refs, mapped);
	}

	#[test]
	fn live_session_details_authenticate_without_claiming_a_managed_worktree() {
		let root = Ref::new();
		let details = SourceDetails::new("Captured Place".to_owned(), vec![], true)
			.with_source_root_ref(root)
			.with_source_generation("generation".to_owned())
			.with_session_token("session".to_owned());
		let value = serde_json::to_value(details).unwrap();
		assert_eq!(value["name"], "Captured Place");
		assert_eq!(value["sourceGeneration"], "generation");
		assert_eq!(value["sessionToken"], "session");
		assert_eq!(value["sourceRootRef"], serde_json::to_value(root).unwrap());
		assert!(value.get("worktreeId").is_none());
	}

	#[test]
	fn resolver_rejects_non_manifest_json() {
		let directory = temporary_directory("resolver-rejection");
		fs::create_dir_all(&directory).unwrap();
		let unsupported = directory.join("default.project.json");
		fs::write(&unsupported, "{}").unwrap();
		let error = resolve(unsupported).unwrap_err().to_string();
		assert!(error.contains("expected a strict .carbon.json project"));
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn resolver_requires_one_unambiguous_manifest() {
		let directory = temporary_directory("manifest-resolver");
		fs::create_dir_all(&directory).unwrap();
		fs::write(directory.join("a.carbon.json"), "{}").unwrap();
		assert_eq!(resolve(directory.clone()).unwrap(), directory.join("a.carbon.json"));
		fs::write(directory.join("b.carbon.json"), "{}").unwrap();
		assert!(resolve(directory.clone()).unwrap_err().to_string().contains("found 2"));
		fs::remove_dir_all(directory).unwrap();
	}
}
