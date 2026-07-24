use anyhow::{ensure, Context, Result};
use clap::Parser;
use colored::Colorize;
use std::path::PathBuf;

use crate::{carbon_info, ext::PathExt, project};

/// Migrate an existing binary Roblox place into a canonical Carbon project.
#[derive(Parser)]
pub struct Migrate {
	/// Existing binary .rbxl place.
	#[arg()]
	input: PathBuf,

	/// Destination strict *.carbon.json project.
	#[arg(short, long)]
	output: PathBuf,
}

impl Migrate {
	pub fn main(self) -> Result<()> {
		let input = self.input.resolve()?;
		ensure!(input.get_ext() == "rbxl", "input must be a binary .rbxl place");
		ensure!(input.is_file(), "input place does not exist: {}", input.display());

		let output = self.output.resolve()?;
		ensure!(
			project::is_project_path(&output),
			"output must be a .carbon.json project"
		);
		let report = project::extract_binary(&input, &output)
			.with_context(|| format!("failed to migrate {}", input.display()))?;
		carbon_info!(
			"Migrated {} into {} with {} Studio-owned instances, {} properties, and {} external blobs in {} artifact",
			input.to_string().bold(),
			output.to_string().bold(),
			report.instances,
			report.properties,
			report.blobs,
			report.artifacts,
		);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::place_diff;
	use rbx_dom_weak::{InstanceBuilder, WeakDom};
	use std::{fs, fs::File};
	use uuid::Uuid;

	#[test]
	fn migrate_creates_a_rebuildable_project_from_a_binary_place() {
		let root = std::env::temp_dir().join(format!("carbon-migrate-{}", Uuid::new_v4()));
		fs::create_dir_all(&root).unwrap();
		let input = root.join("existing.rbxl");
		let output = root.join("game.carbon.json");
		let rebuilt = root.join("rebuilt.rbxl");
		let dom = WeakDom::new(
			InstanceBuilder::new("DataModel")
				.with_child(
					InstanceBuilder::new("Workspace")
						.with_name("Workspace")
						.with_child(InstanceBuilder::new("Part").with_name("MigratedPart")),
				)
				.with_child(
					InstanceBuilder::new("ServerScriptService")
						.with_name("ServerScriptService")
						.with_child(
							InstanceBuilder::new("Script")
								.with_name("Main")
								.with_property("Source", "print('migrated')\n"),
						)
						.with_child(
							InstanceBuilder::new("Folder")
								.with_name("Container")
								.with_child(InstanceBuilder::new("Folder").with_name("Empty"))
								.with_child(
									InstanceBuilder::new("ModuleScript")
										.with_name("Library")
										.with_property("Source", "return {}\n"),
								),
						),
				),
		);
		rbx_binary::to_writer(File::create(&input).unwrap(), &dom, dom.root().children()).unwrap();

		Migrate {
			input: input.clone(),
			output: output.clone(),
		}
		.main()
		.unwrap();

		assert!(output.is_file());
		assert!(root.join("game.carbon.data/state.carbon").is_file());
		assert_eq!(
			fs::read_to_string(root.join("src/ServerScriptService/Main.server.luau")).unwrap(),
			"print('migrated')\n"
		);
		assert_eq!(
			serde_json::from_slice::<serde_json::Value>(
				&fs::read(root.join("src/ServerScriptService/Container/Empty/meta.json")).unwrap()
			)
			.unwrap(),
			serde_json::json!({})
		);
		project::compile(&output, &rebuilt, None).unwrap();
		assert_eq!(
			place_diff::compare(&input, &rebuilt, 100).unwrap().blocking_differences,
			0
		);

		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn migrate_keeps_unrepresentable_script_subtrees_in_carbon_state() {
		let root = std::env::temp_dir().join(format!("carbon-migrate-{}", Uuid::new_v4()));
		fs::create_dir_all(&root).unwrap();
		let input = root.join("existing.rbxl");
		let output = root.join("game.carbon.json");
		let rebuilt = root.join("rebuilt.rbxl");
		let dom = WeakDom::new(
			InstanceBuilder::new("DataModel").with_child(
				InstanceBuilder::new("Workspace").with_name("Workspace").with_child(
					InstanceBuilder::new("Model").with_name("OpaqueModel").with_child(
						InstanceBuilder::new("Script")
							.with_name("Embedded")
							.with_property("Source", "print('embedded')\n"),
					),
				),
			),
		);
		rbx_binary::to_writer(File::create(&input).unwrap(), &dom, dom.root().children()).unwrap();

		Migrate {
			input: input.clone(),
			output: output.clone(),
		}
		.main()
		.unwrap();

		assert!(output.is_file());
		assert!(root.join("game.carbon.data/state.carbon").is_file());
		assert!(!root.join("src/Workspace/OpaqueModel").exists());
		project::compile(&output, &rebuilt, None).unwrap();
		assert_eq!(
			place_diff::compare(&input, &rebuilt, 100).unwrap().blocking_differences,
			0
		);

		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn migrate_rejects_non_binary_place_inputs() {
		let error = Migrate {
			input: PathBuf::from("existing.rbxlx"),
			output: PathBuf::from("game.carbon.json"),
		}
		.main()
		.unwrap_err();
		assert!(error.to_string().contains("binary .rbxl"));
	}
}
