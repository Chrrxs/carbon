//! Agent-facing semantic conflict resolution for Carbon artifacts.
//!
//! Git and the filesystem are adapters around one typed artifact merge plan.
//! Discovery materializes the three index stages, reports semantic fields, and
//! fingerprints the exact index state. Application reconstructs that plan and
//! accepts decisions only when the fingerprint and conflict descriptions still
//! match.

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::{
	collections::{BTreeMap, HashMap},
	env,
	ffi::OsString,
	fs::{self, File, OpenOptions},
	io::Write,
	path::{Path, PathBuf},
	process::{Command, Output},
};
use uuid::Uuid;

use crate::artifact_store::{
	self, artifact_blob_names, install_artifact_file, plan_artifact_merge, ArtifactMergePlan, PlannedConflict,
	ResolutionSide,
};
use crate::project;

pub(crate) const RESOLUTION_SCHEMA: &str = "carbon-conflict-resolution-v1";

const MERGE_DRIVER_NAME: &str = "Carbon semantic artifact merge";
const MERGE_DRIVER_COMMAND: &str = "carbon merge-artifact %O %A %B %P";
const MERGE_DRIVER_RECURSIVE: &str = "binary";
const MERGE_ATTRIBUTE_RULE: &str = "*.carbon merge=carbon -diff";

pub(crate) const CONFLICT_GUIDANCE: &str = "Next: from the repository, run `carbon conflicts --json > carbon-conflicts.json`, add one decision for every conflict, then run `carbon resolve --plan carbon-conflicts.json`.\nAfter Carbon stages the resolved artifact, run `git merge --continue` or `git rebase --continue`.\nHelp: `carbon conflicts --help` describes the conflict document; `carbon resolve --help` describes every decision action.";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GitStage {
	pub mode: String,
	pub oid: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GitStages {
	pub base: GitStage,
	pub current: GitStage,
	pub incoming: GitStage,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ResolutionConflict {
	pub id: String,
	pub details: PlannedConflict,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ResolutionGuidance {
	pub workflow: Vec<String>,
	pub help: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "action", deny_unknown_fields)]
pub(crate) enum ResolutionDecision {
	Take { conflict: String, side: ResolutionSide },
	Set { conflict: String, value: JsonValue },
	Remove { conflict: String },
}

impl ResolutionDecision {
	fn conflict(&self) -> &str {
		match self {
			Self::Take { conflict, .. } | Self::Set { conflict, .. } | Self::Remove { conflict } => conflict,
		}
	}
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ResolutionDocument {
	pub schema: String,
	pub path: String,
	pub token: String,
	pub stages: GitStages,
	pub conflicts: Vec<ResolutionConflict>,
	pub decisions: Vec<ResolutionDecision>,
	pub guidance: ResolutionGuidance,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyReport {
	pub path: String,
	pub resolved_conflicts: usize,
	pub instances: u64,
	pub properties: u64,
	pub staged: Vec<String>,
	pub next: String,
	pub help: Vec<String>,
}

#[derive(Clone, Debug)]
struct IndexEntry {
	mode: String,
	oid: String,
}

#[derive(Clone, Debug, Default)]
struct UnmergedPath {
	stages: [Option<IndexEntry>; 3],
}

struct TemporaryDirectory {
	path: PathBuf,
}

impl TemporaryDirectory {
	fn new() -> Result<Self> {
		let path = env::temp_dir().join(format!("carbon-conflict-{}", Uuid::new_v4().simple()));
		fs::create_dir(&path).with_context(|| format!("failed to create conflict workspace {}", path.display()))?;
		Ok(Self { path })
	}
}

impl Drop for TemporaryDirectory {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.path);
	}
}

struct PreparedConflict {
	root: PathBuf,
	path: String,
	stages: GitStages,
	temporary: TemporaryDirectory,
	stage_artifacts: [PathBuf; 3],
	plan: ArtifactMergePlan,
	document: ResolutionDocument,
}

pub(crate) struct GitMergeInputs {
	_temporary: TemporaryDirectory,
	pub base: PathBuf,
	pub current: PathBuf,
	pub incoming: PathBuf,
}

fn guidance() -> ResolutionGuidance {
	ResolutionGuidance {
		workflow: vec![
			"Add exactly one decision for every conflict ID in this document.".to_owned(),
			"Run `carbon resolve --plan <plan.json>`; Carbon will reject stale or incomplete plans.".to_owned(),
			"Run `git merge --continue` or `git rebase --continue` after Carbon stages the result.".to_owned(),
		],
		help: vec!["carbon conflicts --help".to_owned(), "carbon resolve --help".to_owned()],
	}
}

fn git_output(cwd: &Path, arguments: &[OsString]) -> Result<Output> {
	Command::new("git")
		.args(arguments)
		.current_dir(cwd)
		.output()
		.context("failed to execute Git")
}

fn git_checked(cwd: &Path, arguments: &[OsString]) -> Result<Vec<u8>> {
	let output = git_output(cwd, arguments)?;
	if !output.status.success() {
		let command = arguments
			.iter()
			.map(|argument| argument.to_string_lossy())
			.collect::<Vec<_>>()
			.join(" ");
		let stderr = String::from_utf8_lossy(&output.stderr);
		bail!("Git command `git {command}` failed: {}", stderr.trim());
	}
	Ok(output.stdout)
}

fn git_try(cwd: &Path, arguments: &[OsString]) -> Result<Option<Vec<u8>>> {
	let output = git_output(cwd, arguments)?;
	Ok(output.status.success().then_some(output.stdout))
}

fn find_repository() -> Result<PathBuf> {
	let current = env::current_dir().context("failed to determine current directory")?;
	let output = git_checked(
		&current,
		&[OsString::from("rev-parse"), OsString::from("--show-toplevel")],
	)
	.context("Carbon conflict commands must run inside the conflicted Git repository")?;
	let root = String::from_utf8(output).context("Git repository path is not UTF-8")?;
	fs::canonicalize(root.trim()).context("failed to resolve Git repository root")
}

fn repository_for(path: &Path) -> Result<Option<PathBuf>> {
	let directory = path.parent().unwrap_or(path);
	let has_git_marker = directory.ancestors().any(|ancestor| ancestor.join(".git").exists());
	let output = match Command::new("git")
		.args(["rev-parse", "--show-toplevel"])
		.current_dir(directory)
		.output()
	{
		Ok(output) => output,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound && !has_git_marker => return Ok(None),
		Err(error) => return Err(error).context("failed to execute Git while configuring Carbon semantic merges"),
	};
	if !output.status.success() {
		if !has_git_marker {
			return Ok(None);
		}
		bail!(
			"failed to locate the Git repository for {}: {}",
			path.display(),
			String::from_utf8_lossy(&output.stderr).trim()
		);
	}
	let root = String::from_utf8(output.stdout).context("Git repository path is not UTF-8")?;
	Ok(Some(
		fs::canonicalize(root.trim()).context("failed to resolve Git repository root")?,
	))
}

fn git_common_dir(root: &Path) -> Result<PathBuf> {
	let output = git_checked(root, &[OsString::from("rev-parse"), OsString::from("--git-common-dir")])?;
	let path = String::from_utf8(output).context("Git common directory is not UTF-8")?;
	let path = PathBuf::from(path.trim());
	Ok(if path.is_absolute() { path } else { root.join(path) })
}

#[cfg(target_os = "linux")]
fn is_windows_drive_path(path: &str) -> bool {
	let bytes = path.as_bytes();
	bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'\\' | b'/')
}

#[cfg(target_os = "linux")]
fn windows_git_common_dir(root: &Path) -> Result<OsString> {
	ensure!(
		env::var_os("WSL_DISTRO_NAME").is_some(),
		"Windows Git fallback is available only from WSL"
	);
	let common_dir = git_common_dir(root)?;
	let output = Command::new("wslpath")
		.arg("-w")
		.arg(&common_dir)
		.output()
		.with_context(|| format!("failed to translate Git common directory {}", common_dir.display()))?;
	ensure!(
		output.status.success(),
		"wslpath -w failed for Git common directory {}: {}",
		common_dir.display(),
		String::from_utf8_lossy(&output.stderr).trim()
	);
	let windows_path = String::from_utf8(output.stdout)
		.context("Windows Git common directory is not UTF-8")?
		.trim()
		.to_owned();
	ensure!(
		is_windows_drive_path(&windows_path),
		"Git common directory {} is not on a Windows drive",
		common_dir.display()
	);
	Ok(windows_path.into())
}

#[cfg(target_os = "linux")]
fn set_local_config_with_windows_git(root: &Path, arguments: &[OsString]) -> Result<()> {
	let git_dir = windows_git_common_dir(root)?;
	let output = Command::new("git.exe")
		.arg("--git-dir")
		.arg(git_dir)
		.args(arguments)
		.output()
		.context("failed to execute Windows Git from WSL")?;
	ensure!(
		output.status.success(),
		"Windows Git configuration fallback failed: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	);
	Ok(())
}

fn local_config_matches(root: &Path, key: &str, expected: &str) -> Result<bool> {
	let output = git_output(
		root,
		&[
			OsString::from("config"),
			OsString::from("--local"),
			OsString::from("--get-all"),
			OsString::from(key),
		],
	)?;
	if output.status.success() {
		let values = String::from_utf8(output.stdout).context("Git configuration value is not UTF-8")?;
		return Ok(values.lines().collect::<Vec<_>>() == [expected]);
	}
	if output.status.code() == Some(1) {
		return Ok(false);
	}
	bail!(
		"failed to inspect local Git configuration `{key}`: {}",
		String::from_utf8_lossy(&output.stderr).trim()
	)
}

fn set_local_config(root: &Path, key: &str, value: &str) -> Result<()> {
	let arguments = [
		OsString::from("config"),
		OsString::from("--local"),
		OsString::from("--replace-all"),
		OsString::from(key),
		OsString::from(value),
	];
	match git_checked(root, &arguments) {
		Ok(_) => Ok(()),
		Err(error) => {
			#[cfg(target_os = "linux")]
			if env::var_os("WSL_DISTRO_NAME").is_some() {
				return set_local_config_with_windows_git(root, &arguments).with_context(|| {
					format!("native Git could not update local configuration: {error:#}; Windows Git fallback failed")
				});
			}
			Err(error)
		}
	}
}

fn merge_attributes_match(root: &Path, artifact: &str) -> Result<bool> {
	let output = git_checked(
		root,
		&[
			OsString::from("check-attr"),
			OsString::from("-z"),
			OsString::from("merge"),
			OsString::from("diff"),
			OsString::from("--"),
			OsString::from(artifact),
		],
	)?;
	let fields = output
		.split(|byte| *byte == 0)
		.filter(|field| !field.is_empty())
		.collect::<Vec<_>>();
	ensure!(fields.len() == 6, "Git returned malformed attribute output");
	let mut attributes = HashMap::new();
	for field in fields.chunks_exact(3) {
		let name = std::str::from_utf8(field[1]).context("Git attribute name is not UTF-8")?;
		let value = std::str::from_utf8(field[2]).context("Git attribute value is not UTF-8")?;
		attributes.insert(name, value);
	}
	Ok(attributes.get("merge") == Some(&"carbon") && attributes.get("diff") == Some(&"unset"))
}

fn append_local_attributes(root: &Path) -> Result<()> {
	let output = git_checked(
		root,
		&[
			OsString::from("rev-parse"),
			OsString::from("--git-path"),
			OsString::from("info/attributes"),
		],
	)?;
	let path = String::from_utf8(output).context("Git attributes path is not UTF-8")?;
	let path = PathBuf::from(path.trim());
	let path = if path.is_absolute() { path } else { root.join(path) };
	let parent = path.parent().context("Git attributes path has no parent")?;
	fs::create_dir_all(parent)?;
	let mut contents = if path.is_file() { fs::read(&path)? } else { Vec::new() };
	if !contents.is_empty() && !contents.ends_with(b"\n") {
		contents.push(b'\n');
	}
	contents.extend_from_slice(b"# Carbon semantic artifact merge (managed by carbon serve)\n");
	contents.extend_from_slice(MERGE_ATTRIBUTE_RULE.as_bytes());
	contents.push(b'\n');
	let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(&path)?;
	file.write_all(&contents)?;
	file.sync_all()?;
	Ok(())
}

fn repository_setup_lock(root: &Path) -> Result<File> {
	let path = git_common_dir(root)?;
	let lock_path = path.join("carbon-serve.lock");
	let lock = OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(&lock_path)
		.with_context(|| format!("failed to open repository setup lock {}", lock_path.display()))?;
	lock.lock()
		.with_context(|| format!("failed to lock repository setup {}", lock_path.display()))?;
	Ok(lock)
}

/// Ensure the repository containing a served Carbon project has effective,
/// local semantic-merge attributes and the exact driver command. Returns true
/// only when Git metadata changed; projects outside Git remain supported.
pub(crate) fn configure_repository(project_path: &Path) -> Result<bool> {
	let Some(root) = repository_for(project_path)? else {
		return Ok(false);
	};
	let _setup_lock = repository_setup_lock(&root)?;
	let project_path = fs::canonicalize(project_path)
		.with_context(|| format!("failed to resolve Carbon project {}", project_path.display()))?;
	let artifact = project::data_artifact(&project_path)?;
	let artifact = artifact
		.strip_prefix(&root)
		.with_context(|| {
			format!(
				"Carbon project is outside its Git repository: {}",
				project_path.display()
			)
		})?
		.to_str()
		.context("Carbon artifact path is not UTF-8")?
		.replace('\\', "/");
	let mut changed = false;
	if !merge_attributes_match(&root, &artifact)? {
		append_local_attributes(&root)?;
		ensure!(
			merge_attributes_match(&root, &artifact)?,
			"Git did not apply Carbon's local merge attributes to {artifact}"
		);
		changed = true;
	}
	for (key, value) in [
		("merge.carbon.name", MERGE_DRIVER_NAME),
		("merge.carbon.driver", MERGE_DRIVER_COMMAND),
		("merge.carbon.recursive", MERGE_DRIVER_RECURSIVE),
	] {
		if !local_config_matches(&root, key, value)? {
			set_local_config(&root, key, value)?;
			changed = true;
		}
	}
	Ok(changed)
}

fn parse_unmerged(root: &Path) -> Result<BTreeMap<String, UnmergedPath>> {
	let bytes = git_checked(
		root,
		&[
			OsString::from("ls-files"),
			OsString::from("-u"),
			OsString::from("-z"),
			OsString::from("--full-name"),
		],
	)?;
	let mut paths = BTreeMap::<String, UnmergedPath>::new();
	for record in bytes.split(|byte| *byte == 0).filter(|record| !record.is_empty()) {
		let record = std::str::from_utf8(record).context("an unmerged Git path is not UTF-8")?;
		let (metadata, path) = record
			.split_once('\t')
			.context("Git returned a malformed unmerged index entry")?;
		let mut metadata = metadata.split_whitespace();
		let mode = metadata.next().context("unmerged index entry has no mode")?;
		let oid = metadata.next().context("unmerged index entry has no object ID")?;
		let stage: usize = metadata
			.next()
			.context("unmerged index entry has no stage")?
			.parse()
			.context("unmerged index entry has an invalid stage")?;
		ensure!((1..=3).contains(&stage), "unmerged index stage is outside 1..=3");
		ensure!(metadata.next().is_none(), "unmerged index entry has trailing metadata");
		let entry = &mut paths.entry(path.to_owned()).or_default().stages[stage - 1];
		ensure!(entry.is_none(), "Git returned duplicate stage {stage} for {path}");
		*entry = Some(IndexEntry {
			mode: mode.to_owned(),
			oid: oid.to_owned(),
		});
	}
	Ok(paths)
}

fn user_path(root: &Path, path: &Path) -> Result<String> {
	let absolute = if path.is_absolute() {
		path.to_owned()
	} else {
		env::current_dir()?.join(path)
	};
	let absolute = absolute
		.canonicalize()
		.with_context(|| format!("conflicted artifact does not exist: {}", absolute.display()))?;
	let relative = absolute
		.strip_prefix(root)
		.with_context(|| format!("conflicted artifact is outside repository: {}", absolute.display()))?;
	let relative = relative
		.to_str()
		.context("conflicted artifact path is not UTF-8")?
		.replace('\\', "/");
	ensure!(!relative.is_empty(), "repository root is not a Carbon artifact");
	Ok(relative)
}

fn select_conflict(
	root: &Path,
	requested: Option<&Path>,
	paths: &BTreeMap<String, UnmergedPath>,
) -> Result<(String, GitStages)> {
	let candidates = paths
		.iter()
		.filter(|(path, _)| path.ends_with(".carbon"))
		.collect::<Vec<_>>();
	let (path, unmerged) = if let Some(requested) = requested {
		let requested = user_path(root, requested)?;
		let unmerged = paths
			.get(&requested)
			.with_context(|| format!("{requested} is not an unmerged Git path"))?;
		ensure!(requested.ends_with(".carbon"), "{requested} is not a Carbon artifact");
		(requested, unmerged)
	} else {
		match candidates.as_slice() {
			[] => bail!(
				"no conflicted .carbon artifact exists in the Git index\nHelp: `carbon conflicts --help` explains when this command applies"
			),
			[(path, unmerged)] => ((*path).clone(), *unmerged),
			_ => {
				let paths = candidates
					.iter()
					.map(|(path, _)| format!("  {path}"))
					.collect::<Vec<_>>()
					.join("\n");
				bail!("multiple Carbon artifacts are conflicted; pass one path explicitly:\n{paths}")
			}
		}
	};
	let stage = |index: usize, name: &str| -> Result<GitStage> {
		let entry =
			unmerged.stages[index].as_ref().with_context(|| {
				format!("{path} has no {name} index stage; semantic resolution requires base, current, and incoming artifacts")
			})?;
		Ok(GitStage {
			mode: entry.mode.clone(),
			oid: entry.oid.clone(),
		})
	};
	let stages = GitStages {
		base: stage(0, "base")?,
		current: stage(1, "current")?,
		incoming: stage(2, "incoming")?,
	};
	Ok((path, stages))
}

fn object_bytes(root: &Path, object: &str) -> Result<Vec<u8>> {
	git_checked(
		root,
		&[
			OsString::from("cat-file"),
			OsString::from("blob"),
			OsString::from(object),
		],
	)
}

fn try_object_bytes(root: &Path, object: &str) -> Result<Option<Vec<u8>>> {
	git_try(
		root,
		&[
			OsString::from("cat-file"),
			OsString::from("blob"),
			OsString::from(object),
		],
	)
}

fn blob_repository_path(artifact_path: &str, name: &str) -> Result<String> {
	let parent = Path::new(artifact_path)
		.parent()
		.context("conflicted artifact has no data directory")?;
	Ok(parent
		.join("blobs")
		.join(name)
		.to_str()
		.context("Carbon blob path is not UTF-8")?
		.replace('\\', "/"))
}

fn recover_blob(root: &Path, path: &str, stage: usize) -> Result<Vec<u8>> {
	for object in [format!(":{stage}:{path}"), format!(":{path}")] {
		if let Some(bytes) = try_object_bytes(root, &object)? {
			return Ok(bytes);
		}
	}
	let working = root.join(path);
	if working.is_file() {
		return fs::read(&working).with_context(|| format!("failed to read Carbon blob {}", working.display()));
	}
	for revision in ["HEAD", "MERGE_HEAD", "REBASE_HEAD", "ORIG_HEAD"] {
		if let Some(bytes) = try_object_bytes(root, &format!("{revision}:{path}"))? {
			return Ok(bytes);
		}
	}
	let commits = git_try(
		root,
		&[
			OsString::from("log"),
			OsString::from("--all"),
			OsString::from("--format=%H"),
			OsString::from("--"),
			OsString::from(path),
		],
	)?;
	if let Some(commits) = commits {
		let commits = String::from_utf8(commits).context("Git commit IDs are not UTF-8")?;
		for commit in commits.lines().filter(|commit| !commit.is_empty()) {
			if let Some(bytes) = try_object_bytes(root, &format!("{commit}:{path}"))? {
				return Ok(bytes);
			}
		}
	}
	bail!("required Carbon blob {path} is unavailable from the index, working tree, and reachable history")
}

fn materialize_stage(
	root: &Path,
	temporary: &Path,
	artifact_path: &str,
	stage: usize,
	stage_name: &str,
	object: &GitStage,
) -> Result<PathBuf> {
	let directory = temporary.join(stage_name);
	fs::create_dir(&directory)?;
	let artifact = directory.join("state.carbon");
	fs::write(&artifact, object_bytes(root, &object.oid)?)?;
	for name in artifact_blob_names(&artifact)? {
		let repository_path = blob_repository_path(artifact_path, &name)?;
		let destination = directory.join("blobs").join(&name);
		fs::create_dir_all(destination.parent().expect("blob path has a parent"))?;
		fs::write(&destination, recover_blob(root, &repository_path, stage)?)?;
	}
	Ok(artifact)
}

fn materialize_driver_artifact(
	root: &Path,
	temporary: &Path,
	artifact_path: &str,
	stage: usize,
	stage_name: &str,
	source: &Path,
	hydrate: bool,
) -> Result<PathBuf> {
	let directory = temporary.join(stage_name);
	fs::create_dir(&directory)?;
	let artifact = directory.join("state.carbon");
	fs::copy(source, &artifact)?;
	if hydrate {
		for name in artifact_blob_names(&artifact)? {
			let repository_path = blob_repository_path(artifact_path, &name)?;
			let destination = directory.join("blobs").join(&name);
			fs::create_dir_all(destination.parent().expect("blob path has a parent"))?;
			fs::write(&destination, recover_blob(root, &repository_path, stage)?)?;
		}
	}
	Ok(artifact)
}

pub(crate) fn materialize_git_merge_inputs(
	base: &Path,
	current: &Path,
	incoming: &Path,
	worktree: &Path,
) -> Result<GitMergeInputs> {
	let root = find_repository()?;
	let path = user_path(&root, worktree)?;
	let temporary = TemporaryDirectory::new()?;
	let base = materialize_driver_artifact(&root, &temporary.path, &path, 1, "base", base, false)?;
	let current = materialize_driver_artifact(&root, &temporary.path, &path, 2, "current", current, true)?;
	let incoming = materialize_driver_artifact(&root, &temporary.path, &path, 3, "incoming", incoming, true)?;
	Ok(GitMergeInputs {
		_temporary: temporary,
		base,
		current,
		incoming,
	})
}

fn conflict_id(path: &str, stages: &GitStages, details: &PlannedConflict) -> Result<String> {
	let mut hasher = blake3::Hasher::new();
	hasher.update(RESOLUTION_SCHEMA.as_bytes());
	hasher.update(path.as_bytes());
	hasher.update(&serde_json::to_vec(stages)?);
	hasher.update(&serde_json::to_vec(details)?);
	Ok(format!("c_{}", hasher.finalize().to_hex()))
}

fn plan_token(path: &str, stages: &GitStages, conflicts: &[ResolutionConflict]) -> Result<String> {
	let mut hasher = blake3::Hasher::new();
	hasher.update(RESOLUTION_SCHEMA.as_bytes());
	hasher.update(path.as_bytes());
	hasher.update(&serde_json::to_vec(stages)?);
	for conflict in conflicts {
		hasher.update(conflict.id.as_bytes());
	}
	Ok(hasher.finalize().to_hex().to_string())
}

fn prepare(requested: Option<&Path>) -> Result<PreparedConflict> {
	let root = find_repository()?;
	let paths = parse_unmerged(&root)?;
	let (path, stages) = select_conflict(&root, requested, &paths)?;
	let temporary = TemporaryDirectory::new()?;
	let stage_artifacts = [
		materialize_stage(&root, &temporary.path, &path, 1, "base", &stages.base)?,
		materialize_stage(&root, &temporary.path, &path, 2, "current", &stages.current)?,
		materialize_stage(&root, &temporary.path, &path, 3, "incoming", &stages.incoming)?,
	];
	let plan = plan_artifact_merge(
		&stage_artifacts[0],
		&stage_artifacts[0],
		&stage_artifacts[1],
		&stage_artifacts[1],
		&stage_artifacts[2],
		&stage_artifacts[2],
	)?;
	let details = plan.conflicts()?;
	let conflicts = details
		.into_iter()
		.map(|details| {
			Ok(ResolutionConflict {
				id: conflict_id(&path, &stages, &details)?,
				details,
			})
		})
		.collect::<Result<Vec<_>>>()?;
	let document = ResolutionDocument {
		schema: RESOLUTION_SCHEMA.to_owned(),
		path: path.clone(),
		token: plan_token(&path, &stages, &conflicts)?,
		stages: stages.clone(),
		conflicts,
		decisions: Vec::new(),
		guidance: guidance(),
	};
	ensure!(
		plan.conflict_count() == document.conflicts.len(),
		"semantic conflict document lost a planned conflict"
	);
	Ok(PreparedConflict {
		root,
		path,
		stages,
		temporary,
		stage_artifacts,
		plan,
		document,
	})
}

pub(crate) fn discover(path: Option<&Path>) -> Result<ResolutionDocument> {
	Ok(prepare(path)?.document)
}

fn decision_index(reference: &str, conflicts: &[ResolutionConflict]) -> Result<usize> {
	if let Some(index) = conflicts.iter().position(|conflict| conflict.id == reference) {
		return Ok(index);
	}
	ensure!(
		reference.len() >= 8,
		"conflict reference `{reference}` is neither a full ID nor an 8-character prefix"
	);
	let matches = conflicts
		.iter()
		.enumerate()
		.filter(|(_, conflict)| conflict.id.starts_with(reference))
		.map(|(index, _)| index)
		.collect::<Vec<_>>();
	match matches.as_slice() {
		[index] => Ok(*index),
		[] => bail!("decision refers to unknown conflict `{reference}`"),
		_ => bail!("conflict prefix `{reference}` is ambiguous"),
	}
}

fn copy_new_file(source: &Path, destination: &Path) -> Result<bool> {
	if destination.exists() {
		return Ok(false);
	}
	let parent = destination.parent().context("destination has no parent")?;
	fs::create_dir_all(parent)?;
	let temporary = parent.join(format!(".carbon-resolve-{}.tmp", Uuid::new_v4().simple()));
	let result = (|| {
		fs::copy(source, &temporary)?;
		OpenOptions::new().write(true).open(&temporary)?.sync_all()?;
		fs::rename(&temporary, destination)?;
		Ok(true)
	})();
	if result.is_err() {
		let _ = fs::remove_file(&temporary);
	}
	result
}

fn copy_synced(source: &Path, destination: &Path) -> Result<()> {
	fs::copy(source, destination)?;
	OpenOptions::new().write(true).open(destination)?.sync_all()?;
	Ok(())
}

fn stage_paths(root: &Path, paths: &[String]) -> Result<()> {
	let mut arguments = vec![OsString::from("add"), OsString::from("--")];
	arguments.extend(paths.iter().map(OsString::from));
	git_checked(root, &arguments)?;
	Ok(())
}

fn install_candidate(prepared: &PreparedConflict, candidate: &Path, candidate_blobs: &[String]) -> Result<Vec<String>> {
	let destination = prepared.root.join(&prepared.path);
	let current_bytes = fs::read(&prepared.stage_artifacts[1])?;
	ensure!(
		fs::read(&destination).with_context(|| format!("failed to read {}", destination.display()))? == current_bytes,
		"{} no longer matches Git's current stage; rerun `carbon conflicts` before resolving",
		prepared.path
	);
	let parent = destination.parent().context("conflicted artifact has no parent")?;
	let mut created_blobs = Vec::new();
	let mut staged = vec![prepared.path.clone()];
	for name in candidate_blobs {
		let source = candidate
			.parent()
			.expect("candidate has a parent")
			.join("blobs")
			.join(name);
		let repository_path = blob_repository_path(&prepared.path, name)?;
		let target = prepared.root.join(&repository_path);
		if copy_new_file(&source, &target)? {
			created_blobs.push(target);
		}
		staged.push(repository_path);
	}
	let candidate_copy = parent.join(format!(".carbon-resolve-candidate-{}.tmp", Uuid::new_v4().simple()));
	let recovery = parent.join(format!(".carbon-resolve-recovery-{}.tmp", Uuid::new_v4().simple()));
	let result = (|| {
		copy_synced(candidate, &candidate_copy)?;
		artifact_store::load_tree(&candidate_copy).context("resolved candidate failed validation")?;
		copy_synced(&destination, &recovery)?;
		install_artifact_file(&candidate_copy, &destination)?;
		if let Err(error) = stage_paths(&prepared.root, &staged) {
			install_artifact_file(&recovery, &destination)
				.context("Git staging failed and Carbon could not restore the current artifact")?;
			return Err(error).context("resolved artifact was restored because Git staging failed");
		}
		Ok(())
	})();
	let _ = fs::remove_file(&candidate_copy);
	let _ = fs::remove_file(&recovery);
	if result.is_err() {
		for blob in created_blobs {
			let _ = fs::remove_file(blob);
		}
	}
	result?;
	staged.sort();
	staged.dedup();
	Ok(staged)
}

pub(crate) fn apply(document: ResolutionDocument) -> Result<ApplyReport> {
	ensure!(
		document.schema == RESOLUTION_SCHEMA,
		"unsupported resolution schema `{}`; rerun `carbon conflicts`",
		document.schema
	);
	let root = find_repository()?;
	let requested = root.join(&document.path);
	let mut prepared = prepare(Some(&requested))?;
	ensure!(
		document.path == prepared.document.path
			&& document.stages == prepared.stages
			&& document.token == prepared.document.token
			&& document.conflicts == prepared.document.conflicts,
		"resolution plan is stale or its conflict descriptions were modified; rerun `carbon conflicts`"
	);
	let mut decisions = HashMap::<usize, ResolutionDecision>::new();
	for decision in document.decisions {
		let index = decision_index(decision.conflict(), &prepared.document.conflicts)?;
		ensure!(
			decisions.insert(index, decision).is_none(),
			"conflict {} has more than one decision",
			prepared.document.conflicts[index].id
		);
	}
	let missing = prepared
		.document
		.conflicts
		.iter()
		.enumerate()
		.filter(|(index, _)| !decisions.contains_key(index))
		.map(|(_, conflict)| conflict.id.clone())
		.collect::<Vec<_>>();
	ensure!(
		missing.is_empty(),
		"resolution plan is incomplete; missing decisions for: {}\nHelp: `carbon resolve --help` describes take, set, and remove actions",
		missing.join(", ")
	);
	let mut ordered = decisions
		.into_iter()
		.map(|(index, decision)| Ok((prepared.plan.conflict_rank(index)?, index, decision)))
		.collect::<Result<Vec<_>>>()?;
	ordered.sort_by_key(|(rank, index, _)| (*rank, *index));
	for (_, index, decision) in ordered {
		match decision {
			ResolutionDecision::Take { side, .. } => prepared.plan.take(index, side)?,
			ResolutionDecision::Set { value, .. } => prepared.plan.set(index, value)?,
			ResolutionDecision::Remove { .. } => prepared.plan.remove(index)?,
		}
	}
	let candidate = prepared.temporary.path.join("candidate/state.carbon");
	let report = prepared.plan.finish(&candidate)?;
	let blobs = artifact_blob_names(&candidate)?;
	let staged = install_candidate(&prepared, &candidate, &blobs)?;
	Ok(ApplyReport {
		path: prepared.path,
		resolved_conflicts: prepared.document.conflicts.len(),
		instances: report.instances,
		properties: report.properties,
		staged,
		next: "Run `git merge --continue` or `git rebase --continue`.".to_owned(),
		help: vec!["carbon conflicts --help".to_owned(), "carbon resolve --help".to_owned()],
	})
}

pub(crate) fn parse_document(bytes: &[u8]) -> Result<ResolutionDocument> {
	let mut deserializer = serde_json::Deserializer::from_slice(bytes);
	let document = ResolutionDocument::deserialize(&mut deserializer).context("invalid Carbon resolution document")?;
	deserializer
		.end()
		.context("resolution document contains trailing data")?;
	Ok(document)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{
		ffi::OsStr,
		sync::{Arc, Barrier},
	};

	fn git<I, S>(repository: &Path, arguments: I) -> Vec<u8>
	where
		I: IntoIterator<Item = S>,
		S: AsRef<OsStr>,
	{
		let output = Command::new("git")
			.args(arguments)
			.current_dir(repository)
			.output()
			.unwrap();
		assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
		output.stdout
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn windows_git_fallback_requires_a_drive_mapped_common_directory() {
		assert!(is_windows_drive_path(r"C:\Users\carbon\repository\.git"));
		assert!(is_windows_drive_path("D:/repository/.git"));
		assert!(!is_windows_drive_path(r"\\wsl.localhost\Ubuntu\home\carbon\.git"));
		assert!(!is_windows_drive_path("/home/carbon/repository/.git"));
	}

	#[test]
	fn linked_worktree_resolves_the_shared_git_common_directory() {
		let directory = env::temp_dir().join(format!("carbon-linked-git-{}", Uuid::new_v4().simple()));
		let repository = directory.join("repository");
		let worktree = directory.join("worktree");
		fs::create_dir_all(&repository).unwrap();
		git(&repository, ["init", "-b", "main"]);
		git(&repository, ["config", "user.name", "Carbon Test"]);
		git(&repository, ["config", "user.email", "carbon@example.invalid"]);
		fs::write(repository.join("tracked.txt"), "tracked\n").unwrap();
		git(&repository, ["add", "tracked.txt"]);
		git(&repository, ["commit", "-m", "tracked"]);
		git(
			&repository,
			[
				OsString::from("worktree"),
				OsString::from("add"),
				OsString::from("-b"),
				OsString::from("linked"),
				worktree.as_os_str().to_owned(),
			],
		);

		assert_eq!(
			fs::canonicalize(git_common_dir(&worktree).unwrap()).unwrap(),
			fs::canonicalize(repository.join(".git")).unwrap()
		);

		git(
			&repository,
			[
				OsString::from("worktree"),
				OsString::from("remove"),
				OsString::from("--force"),
				worktree.as_os_str().to_owned(),
			],
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn conflict_guidance_names_discovery_resolution_and_help() {
		assert!(CONFLICT_GUIDANCE.contains("carbon conflicts --json"));
		assert!(CONFLICT_GUIDANCE.contains("carbon resolve --plan"));
		assert!(CONFLICT_GUIDANCE.contains("carbon conflicts --help"));
		assert!(CONFLICT_GUIDANCE.contains("carbon resolve --help"));
	}

	#[test]
	fn serve_setup_configures_semantic_merges_without_dirtying_the_worktree() {
		let repository = env::temp_dir().join(format!("carbon-git-setup-{}", Uuid::new_v4().simple()));
		fs::create_dir(&repository).unwrap();
		git(&repository, ["init", "-b", "main"]);
		git(&repository, ["config", "user.name", "Carbon Test"]);
		git(&repository, ["config", "user.email", "carbon@example.invalid"]);
		let project = repository.join("game.carbon.json");
		fs::write(&project, "{}\n").unwrap();
		git(&repository, ["add", "game.carbon.json"]);
		git(&repository, ["commit", "-m", "project"]);

		assert!(configure_repository(&project).unwrap());
		assert_eq!(
			String::from_utf8(git(&repository, ["config", "--local", "--get", "merge.carbon.driver"]))
				.unwrap()
				.trim(),
			"carbon merge-artifact %O %A %B %P"
		);
		assert_eq!(
			String::from_utf8(git(
				&repository,
				["check-attr", "merge", "diff", "--", "game.carbon.data/state.carbon",]
			))
			.unwrap(),
			"game.carbon.data/state.carbon: merge: carbon\ngame.carbon.data/state.carbon: diff: unset\n"
		);
		assert!(git(&repository, ["status", "--porcelain"]).is_empty());
		assert!(!repository.join(".gitattributes").exists());
		let config = fs::read(repository.join(".git/config")).unwrap();
		let attributes = fs::read(repository.join(".git/info/attributes")).unwrap();

		assert!(!configure_repository(&project).unwrap());
		assert_eq!(fs::read(repository.join(".git/config")).unwrap(), config);
		assert_eq!(fs::read(repository.join(".git/info/attributes")).unwrap(), attributes);
		fs::remove_dir_all(repository).unwrap();
	}

	#[test]
	fn serve_setup_keeps_projects_outside_git_supported() {
		let directory = env::temp_dir().join(format!("carbon-no-git-{}", Uuid::new_v4().simple()));
		fs::create_dir(&directory).unwrap();
		let project = directory.join("game.carbon.json");
		fs::write(&project, "{}\n").unwrap();
		assert!(!configure_repository(&project).unwrap());
		assert!(!directory.join(".git").exists());
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn parallel_serve_serializes_shared_repository_setup() {
		let repository = env::temp_dir().join(format!("carbon-parallel-git-setup-{}", Uuid::new_v4().simple()));
		fs::create_dir(&repository).unwrap();
		git(&repository, ["init", "-b", "main"]);
		git(&repository, ["config", "user.name", "Carbon Test"]);
		git(&repository, ["config", "user.email", "carbon@example.invalid"]);
		let project = repository.join("game.carbon.json");
		fs::write(&project, "{}\n").unwrap();
		git(&repository, ["add", "game.carbon.json"]);
		git(&repository, ["commit", "-m", "project"]);
		let workers = 12;
		let barrier = Arc::new(Barrier::new(workers));
		let threads = (0..workers)
			.map(|_| {
				let barrier = Arc::clone(&barrier);
				let project = project.clone();
				std::thread::spawn(move || {
					barrier.wait();
					configure_repository(&project)
				})
			})
			.collect::<Vec<_>>();

		for thread in threads {
			thread.join().unwrap().unwrap();
		}

		let attributes = fs::read_to_string(repository.join(".git/info/attributes")).unwrap();
		assert_eq!(attributes.matches(MERGE_ATTRIBUTE_RULE).count(), 1);
		assert!(git(&repository, ["status", "--porcelain"]).is_empty());
		fs::remove_dir_all(repository).unwrap();
	}
}
