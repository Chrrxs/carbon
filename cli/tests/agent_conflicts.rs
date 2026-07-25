use carbon::{artifact_store, core::snapshot::Snapshot, resolution::UnresolvedValue};
use rbx_dom_weak::{
	types::{Attributes, Ref, Variant},
	Ustr, UstrMap,
};
use serde_json::{json, Value as JsonValue};
use std::{
	ffi::OsStr,
	fs,
	path::{Path, PathBuf},
	process::{Command, Output},
};
use uuid::Uuid;

fn temporary_repository() -> PathBuf {
	let path = std::env::temp_dir().join(format!("carbon-agent-conflict-test-{}", Uuid::new_v4().simple()));
	fs::create_dir(&path).unwrap();
	path
}

fn run<I, S>(program: &Path, cwd: &Path, arguments: I) -> Output
where
	I: IntoIterator<Item = S>,
	S: AsRef<OsStr>,
{
	Command::new(program)
		.args(arguments)
		.current_dir(cwd)
		.env("CARBON_TEST_BUNDLED_REFLECTION", "1")
		.output()
		.unwrap()
}

fn git<I, S>(repository: &Path, arguments: I)
where
	I: IntoIterator<Item = S>,
	S: AsRef<OsStr>,
{
	let output = run(Path::new("git"), repository, arguments);
	assert!(
		output.status.success(),
		"Git failed:\nstdout: {}\nstderr: {}",
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr)
	);
}

fn write_artifact(
	path: &Path,
	root: Ref,
	child: Ref,
	child_name: &str,
	name_tag: &str,
	flavor: &str,
	archivable: bool,
) {
	let snapshot = Snapshot::new()
		.with_id(root)
		.with_name("Game")
		.with_class("DataModel")
		.with_children(vec![Snapshot::new()
			.with_id(child)
			.with_name(child_name)
			.with_class("Folder")
			.with_properties(UstrMap::from_iter([
				(Ustr::from("NameTag"), Variant::String(name_tag.to_owned())),
				(Ustr::from("Flavor"), Variant::String(flavor.to_owned())),
				(Ustr::from("Archivable"), Variant::Bool(archivable)),
			]))]);
	artifact_store::extract_snapshot(snapshot, "Game".to_owned(), path).unwrap();
}

fn write_nested_artifact(path: &Path, root: Ref, parent: Ref, child: Ref, child_value: Option<&str>) {
	let mut attributes = Attributes::new();
	if let Some(value) = child_value {
		attributes.insert("Shared".to_owned(), Variant::String(value.to_owned()));
	}
	let children = child_value
		.map(|_| {
			vec![Snapshot::new()
				.with_id(parent)
				.with_name("ParentDeleteTarget")
				.with_class("Folder")
				.with_children(vec![Snapshot::new()
					.with_id(child)
					.with_name("DescendantTarget")
					.with_class("Folder")
					.with_properties(UstrMap::from_iter([(
						Ustr::from("Attributes"),
						Variant::Attributes(attributes),
					)]))])]
		})
		.unwrap_or_default();
	let snapshot = Snapshot::new()
		.with_id(root)
		.with_name("Game")
		.with_class("DataModel")
		.with_children(children);
	artifact_store::extract_snapshot(snapshot, "Game".to_owned(), path).unwrap();
}

fn initialize_repository(repository: &Path, carbon: &Path) {
	git(repository, ["init", "-b", "main"]);
	git(repository, ["config", "user.name", "Carbon Test"]);
	git(repository, ["config", "user.email", "carbon@example.invalid"]);
	let driver = format!("{} merge-artifact %O %A %B %P", carbon.display());
	git(
		repository,
		["config", "merge.carbon.name", "Carbon semantic artifact merge"],
	);
	git(repository, ["config", "merge.carbon.driver", &driver]);
	fs::write(repository.join(".gitattributes"), "*.carbon merge=carbon -diff\n").unwrap();
}

#[test]
fn incoming_descendant_resolution_restores_implicitly_deleted_ancestor() {
	let repository = temporary_repository();
	let carbon = Path::new(env!("CARGO_BIN_EXE_carbon"));
	initialize_repository(&repository, carbon);
	let artifact = repository.join("game.carbon.data/state.carbon");
	let root = Ref::new();
	let parent = Ref::new();
	let child = Ref::new();
	write_nested_artifact(&artifact, root, parent, child, Some("base"));
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "base"]);

	git(&repository, ["checkout", "-b", "incoming"]);
	write_nested_artifact(&artifact, root, parent, child, Some("B-descendant-modified"));
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "incoming descendant edit"]);

	git(&repository, ["checkout", "main"]);
	write_nested_artifact(&artifact, root, parent, child, None);
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "current subtree deletion"]);

	let merge = run(Path::new("git"), &repository, ["merge", "incoming"]);
	assert!(!merge.status.success());
	let discovery = run(carbon, &repository, ["conflicts", "--json"]);
	assert!(
		discovery.status.success(),
		"{}",
		String::from_utf8_lossy(&discovery.stderr)
	);
	let mut document: JsonValue = serde_json::from_slice(&discovery.stdout).unwrap();
	let conflicts = document["conflicts"].as_array().unwrap();
	assert_eq!(conflicts.len(), 1);
	let conflict = &conflicts[0];
	assert_eq!(conflict["details"]["identity"], child.to_string());
	assert_eq!(conflict["details"]["field"]["kind"], "existence");
	assert!(conflict["details"]["allowed"]
		.as_array()
		.unwrap()
		.iter()
		.any(|choice| choice == "incoming"));
	document["decisions"] = json!([{
		"conflict": conflict["id"],
		"action": "take",
		"side": "incoming"
	}]);
	let plan = repository.join("resolution.json");
	fs::write(&plan, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

	let resolved = run(carbon, &repository, ["resolve", "--plan", plan.to_str().unwrap()]);
	assert!(
		resolved.status.success(),
		"stdout: {}\nstderr: {}",
		String::from_utf8_lossy(&resolved.stdout),
		String::from_utf8_lossy(&resolved.stderr)
	);
	let loaded = artifact_store::load_tree(&artifact).unwrap();
	assert_eq!(loaded.tree.get_instance(parent).unwrap().parent(), root);
	let descendant = loaded.tree.get_instance(child).unwrap();
	assert_eq!(descendant.parent(), parent);
	let Variant::Attributes(attributes) = &descendant.properties[&Ustr::from("Attributes")] else {
		panic!("descendant attributes were not preserved");
	};
	let Some(Variant::BinaryString(value)) = attributes.get("Shared") else {
		panic!("descendant attribute edit was not preserved");
	};
	let value: &[u8] = value.as_ref();
	assert_eq!(value, b"B-descendant-modified");
	fs::remove_dir_all(repository).unwrap();
}

#[test]
fn agent_plan_resolves_a_real_git_artifact_conflict() {
	let repository = temporary_repository();
	let carbon = Path::new(env!("CARGO_BIN_EXE_carbon"));
	initialize_repository(&repository, carbon);
	let artifact = repository.join("game.carbon.data/state.carbon");
	let root = Ref::new();
	let child = Ref::new();
	let base_value = "base".repeat(5_000);
	let current_value = "current".repeat(3_000);
	let incoming_value = "incoming".repeat(3_000);
	let base_flavor = "base-flavor".repeat(2_000);
	let current_flavor = "current-flavor".repeat(2_000);
	let incoming_flavor = "incoming-flavor".repeat(2_000);
	let custom_flavor = "custom-flavor".repeat(2_000);
	write_artifact(&artifact, root, child, "Child", &base_value, &base_flavor, true);
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "base"]);

	git(&repository, ["checkout", "-b", "incoming"]);
	write_artifact(
		&artifact,
		root,
		child,
		"IncomingChild",
		&incoming_value,
		&incoming_flavor,
		true,
	);
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "incoming edit"]);

	git(&repository, ["checkout", "main"]);
	write_artifact(
		&artifact,
		root,
		child,
		"CurrentChild",
		&current_value,
		&current_flavor,
		false,
	);
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "current edits"]);

	let merge = run(Path::new("git"), &repository, ["merge", "incoming"]);
	assert!(!merge.status.success());
	let merge_error = String::from_utf8_lossy(&merge.stderr);
	assert!(merge_error.contains("carbon conflicts --json"), "{merge_error}");
	assert!(merge_error.contains("carbon resolve --plan"), "{merge_error}");
	assert!(merge_error.contains("carbon conflicts --help"), "{merge_error}");
	assert!(merge_error.contains("carbon resolve --help"), "{merge_error}");

	let discovery = run(carbon, &repository, ["conflicts", "--json"]);
	assert!(
		discovery.status.success(),
		"{}",
		String::from_utf8_lossy(&discovery.stderr)
	);
	let mut document: JsonValue = serde_json::from_slice(&discovery.stdout).unwrap();
	assert_eq!(document["schema"], "carbon-conflict-resolution-v1");
	assert_eq!(document["path"], "game.carbon.data/state.carbon");
	let conflicts = document["conflicts"].as_array().unwrap();
	assert_eq!(conflicts.len(), 3);
	let property_conflict = conflicts
		.iter()
		.find(|conflict| conflict["details"]["field"]["name"] == "NameTag")
		.unwrap();
	assert_eq!(property_conflict["details"]["field"]["name"], "NameTag");
	assert_eq!(
		property_conflict["details"]["context"]["current"]["path"],
		json!(["CurrentChild"])
	);
	assert_eq!(
		property_conflict["details"]["context"]["incoming"]["path"],
		json!(["IncomingChild"])
	);
	assert_eq!(
		property_conflict["details"]["incoming"]["value"]["$type"],
		"ExternalValue"
	);
	assert!(
		property_conflict["details"]["incoming"]["value"]["preview"]
			.as_str()
			.unwrap()
			.len() <= 160
	);
	let property_id = property_conflict["id"].as_str().unwrap().to_owned();
	let flavor_id = conflicts
		.iter()
		.find(|conflict| conflict["details"]["field"]["name"] == "Flavor")
		.unwrap()["id"]
		.as_str()
		.unwrap()
		.to_owned();
	let name_id = conflicts
		.iter()
		.find(|conflict| conflict["details"]["field"]["kind"] == "name")
		.unwrap()["id"]
		.as_str()
		.unwrap()
		.to_owned();
	assert_eq!(document["guidance"]["help"].as_array().unwrap().len(), 2);
	let before = fs::read(&artifact).unwrap();

	let incomplete = repository.join("incomplete.json");
	fs::write(&incomplete, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
	let rejected = run(carbon, &repository, ["resolve", "--plan", incomplete.to_str().unwrap()]);
	assert!(!rejected.status.success());
	assert!(String::from_utf8_lossy(&rejected.stderr).contains("missing decisions"));
	assert_eq!(fs::read(&artifact).unwrap(), before);

	let custom_value =
		serde_json::to_value(UnresolvedValue::FullyQualified(Variant::String(custom_flavor.clone()))).unwrap();
	document["decisions"] = json!([
		{"conflict": property_id, "action": "take", "side": "incoming"},
		{"conflict": flavor_id, "action": "set", "value": custom_value},
		{"conflict": name_id, "action": "set", "value": "CustomChild"}
	]);
	let stale = repository.join("stale.json");
	let original_token = document["token"].as_str().unwrap().to_owned();
	document["token"] = json!(format!("{original_token}stale"));
	fs::write(&stale, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
	let rejected = run(carbon, &repository, ["resolve", "--plan", stale.to_str().unwrap()]);
	assert!(!rejected.status.success());
	assert!(String::from_utf8_lossy(&rejected.stderr).contains("stale"));
	assert_eq!(fs::read(&artifact).unwrap(), before);

	document["token"] = json!(original_token);
	let plan = repository.join("resolution.json");
	fs::write(&plan, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
	let resolved = run(carbon, &repository, ["resolve", "--plan", plan.to_str().unwrap()]);
	assert!(
		resolved.status.success(),
		"{}",
		String::from_utf8_lossy(&resolved.stderr)
	);
	let output = String::from_utf8_lossy(&resolved.stdout);
	assert!(output.contains("git merge --continue"), "{output}");
	assert!(output.contains("carbon resolve --help"), "{output}");
	let unmerged = run(Path::new("git"), &repository, ["ls-files", "-u"]);
	assert!(unmerged.status.success());
	assert!(unmerged.stdout.is_empty());

	let loaded = artifact_store::load_tree(&artifact).unwrap();
	let instance = loaded.tree.get_instance(child).unwrap();
	assert_eq!(instance.name, "CustomChild");
	let properties = &instance.properties;
	assert_eq!(properties[&Ustr::from("Archivable")], Variant::Bool(false));
	assert_eq!(properties[&Ustr::from("NameTag")], Variant::String(incoming_value));
	assert_eq!(properties[&Ustr::from("Flavor")], Variant::String(custom_flavor));
	fs::remove_dir_all(repository).unwrap();
}

#[test]
fn git_driver_auto_merges_external_values_through_the_canonical_path() {
	let repository = temporary_repository();
	let carbon = Path::new(env!("CARGO_BIN_EXE_carbon"));
	initialize_repository(&repository, carbon);
	let artifact = repository.join("game.carbon.data/state.carbon");
	let root = Ref::new();
	let child = Ref::new();
	let base_value = "base".repeat(5_000);
	let incoming_value = "incoming".repeat(3_000);
	write_artifact(&artifact, root, child, "Child", &base_value, "same", true);
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "base"]);

	git(&repository, ["checkout", "-b", "incoming"]);
	write_artifact(&artifact, root, child, "Child", &incoming_value, "same", true);
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "incoming property"]);

	git(&repository, ["checkout", "main"]);
	write_artifact(&artifact, root, child, "Child", &base_value, "same", false);
	git(&repository, ["add", "-A"]);
	git(&repository, ["commit", "-m", "current property"]);

	let merge = run(Path::new("git"), &repository, ["merge", "incoming"]);
	assert!(
		merge.status.success(),
		"stdout: {}\nstderr: {}",
		String::from_utf8_lossy(&merge.stdout),
		String::from_utf8_lossy(&merge.stderr)
	);
	let loaded = artifact_store::load_tree(&artifact).unwrap();
	let properties = &loaded.tree.get_instance(child).unwrap().properties;
	assert_eq!(properties[&Ustr::from("Archivable")], Variant::Bool(false));
	assert_eq!(properties[&Ustr::from("NameTag")], Variant::String(incoming_value));
	let status = run(Path::new("git"), &repository, ["status", "--porcelain"]);
	assert!(status.status.success());
	assert!(status.stdout.is_empty(), "{}", String::from_utf8_lossy(&status.stdout));
	fs::remove_dir_all(repository).unwrap();
}
