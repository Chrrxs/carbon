use std::{env, fs::File, io, io::BufReader, io::Read, path::PathBuf};

use rbx_dom_weak::{
	types::{Content, Variant},
	InstanceBuilder, Ustr, WeakDom,
};

fn inspect(path: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
	let dom = rbx_binary::from_reader(BufReader::new(File::open(path)?))?;
	for owner_name in [
		"NoneOwner",
		"UriOwner",
		"VisibleOwner",
		"CrossRootOwner",
		"PersistentCrossRootOwner",
		"RootTargetOwner",
	] {
		let owner = dom
			.descendants()
			.find(|instance| instance.name == owner_name)
			.ok_or("content owner is missing")?;
		let Variant::Content(content) = owner
			.properties
			.get(&Ustr::from("FallbackImageContent"))
			.ok_or("content property is missing")?
		else {
			return Err("content property has the wrong type".into());
		};
		let target = content
			.as_object()
			.and_then(|target| dom.get_by_ref(target))
			.map(|instance| instance.name.as_str())
			.unwrap_or("<none-or-uri>");
		println!("{owner_name}\t{target}");
	}
	Ok(())
}

fn inspect_all(reader: impl Read) -> Result<(), Box<dyn std::error::Error>> {
	let dom = rbx_binary::from_reader(reader)?;
	for instance in dom.descendants() {
		for (property_name, value) in &instance.properties {
			let Variant::Content(content) = value else {
				continue;
			};
			let value = if let Some(uri) = content.as_uri() {
				format!("uri:{uri}")
			} else if let Some(target) = content.as_object() {
				let target = dom
					.get_by_ref(target)
					.map(|target| target.name.as_str())
					.unwrap_or("<missing-object>");
				format!("object:{target}")
			} else {
				"none".to_owned()
			};
			println!("{}\t{}\t{}", instance.name, property_name, value);
		}
	}
	Ok(())
}

fn inspect_prefix(reader: impl Read, prefix: &str) -> Result<(), Box<dyn std::error::Error>> {
	let dom = rbx_binary::from_reader(reader)?;
	for instance in dom.descendants().filter(|instance| instance.name.starts_with(prefix)) {
		for (property_name, value) in &instance.properties {
			println!("{}\t{}\t{:?}", instance.name, property_name, value);
		}
	}
	Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args = env::args_os().skip(1).collect::<Vec<_>>();
	if let [flag, path] = args.as_slice() {
		if flag == "--inspect" {
			return inspect(&PathBuf::from(path));
		}
		if flag == "--inspect-all" {
			if path == "-" {
				return inspect_all(BufReader::new(io::stdin().lock()));
			}
			return inspect_all(BufReader::new(File::open(path)?));
		}
	}
	if let [flag, prefix, path] = args.as_slice() {
		if flag == "--inspect-prefix" {
			if path == "-" {
				return inspect_prefix(BufReader::new(io::stdin().lock()), &prefix.to_string_lossy());
			}
			return inspect_prefix(BufReader::new(File::open(path)?), &prefix.to_string_lossy());
		}
	}
	let output = args
		.first()
		.map(PathBuf::from)
		.ok_or("usage: cargo run --example content_object_fixture -- [--inspect] <output.rbxl>")?;

	let visible_target = InstanceBuilder::new("Folder").with_name("VisibleTarget");
	let visible_target_ref = visible_target.referent();
	let cross_root_target = InstanceBuilder::new("Folder").with_name("CrossRootTarget");
	let cross_root_target_ref = cross_root_target.referent();
	let persistent_cross_root_target = InstanceBuilder::new("Folder").with_name("PersistentCrossRootTarget");
	let persistent_cross_root_target_ref = persistent_cross_root_target.referent();
	let persistent_root_target = InstanceBuilder::new("ServerStorage")
		.with_name("ServerStorage")
		.with_child(persistent_cross_root_target);
	let persistent_root_target_ref = persistent_root_target.referent();
	let dom = WeakDom::new(
		InstanceBuilder::new("DataModel")
			.with_name("Carbon Content.Object evidence")
			.with_child(
				InstanceBuilder::new("Workspace")
					.with_name("Workspace")
					.with_child(visible_target)
					.with_child(
						InstanceBuilder::new("AdGui")
							.with_name("NoneOwner")
							.with_property("FallbackImageContent", Content::none()),
					)
					.with_child(
						InstanceBuilder::new("AdGui")
							.with_name("UriOwner")
							.with_property("FallbackImageContent", Content::from_uri("rbxassetid://1")),
					)
					.with_child(
						InstanceBuilder::new("AdGui")
							.with_name("VisibleOwner")
							.with_property("FallbackImageContent", Content::from_referent(visible_target_ref)),
					)
					.with_child(
						InstanceBuilder::new("AdGui")
							.with_name("CrossRootOwner")
							.with_property("FallbackImageContent", Content::from_referent(cross_root_target_ref)),
					)
					.with_child(
						InstanceBuilder::new("AdGui")
							.with_name("PersistentCrossRootOwner")
							.with_property(
								"FallbackImageContent",
								Content::from_referent(persistent_cross_root_target_ref),
							),
					)
					.with_child(
						InstanceBuilder::new("AdGui")
							.with_name("RootTargetOwner")
							.with_property(
								"FallbackImageContent",
								Content::from_referent(persistent_root_target_ref),
							),
					),
			)
			.with_child(
				InstanceBuilder::new("CoreGui")
					.with_name("CoreGui")
					.with_child(cross_root_target),
			)
			.with_child(persistent_root_target),
	);

	rbx_binary::to_writer(File::create(&output)?, &dom, dom.root().children())?;
	println!("{}", output.display());
	Ok(())
}
