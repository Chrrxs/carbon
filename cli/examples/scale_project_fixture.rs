use anyhow::{bail, Context, Result};
use carbon::{artifact_store, core::snapshot::Snapshot};
use rbx_dom_weak::types::Ref;
use serde_json::json;
use std::{
	fs,
	path::{Path, PathBuf},
};

fn id(ordinal: u128) -> Ref {
	format!("{ordinal:032x}")
		.parse()
		.expect("128-bit hex is a valid Roblox referent")
}

fn generate(root: &Path, children: usize) -> Result<()> {
	if root.exists() {
		bail!("fixture output already exists: {}", root.display());
	}
	let data = root.join("game.carbon.data");
	fs::create_dir_all(&data)?;
	fs::write(
		root.join("game.carbon.json"),
		serde_json::to_vec_pretty(&json!({
			"name": format!("CarbonScale{children}"),
			"tree": { "$className": "DataModel" }
		}))?,
	)?;
	let nodes = (0..children)
		.map(|index| {
			Snapshot::new()
				.with_id(id(index as u128 + 3))
				.with_name(&format!("Node{index:07}"))
				.with_class("Folder")
		})
		.collect();
	let snapshot = Snapshot::new()
		.with_id(id(1))
		.with_name("DataModel")
		.with_class("DataModel")
		.with_children(vec![Snapshot::new()
			.with_id(id(2))
			.with_name("Workspace")
			.with_class("Workspace")
			.with_children(nodes)]);
	artifact_store::extract_snapshot(snapshot, format!("CarbonScale{children}"), &data.join("state.carbon"))?;
	Ok(())
}

fn main() -> Result<()> {
	let mut args = std::env::args_os().skip(1);
	let output = PathBuf::from(
		args.next()
			.context("usage: scale_project_fixture <output-dir> <children>")?,
	);
	let children = args
		.next()
		.context("usage: scale_project_fixture <output-dir> <children>")?
		.to_string_lossy()
		.parse::<usize>()?;
	if args.next().is_some() {
		bail!("usage: scale_project_fixture <output-dir> <children>");
	}
	generate(&output, children)
}
