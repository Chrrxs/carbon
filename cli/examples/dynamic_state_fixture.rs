use std::{env, fs::File, path::PathBuf};

use rbx_dom_weak::{types::Vector3, InstanceBuilder, WeakDom};

fn main() -> Result<(), Box<dyn std::error::Error>> {
	let output = env::args_os()
		.nth(1)
		.map(PathBuf::from)
		.ok_or("usage: cargo run --example dynamic_state_fixture -- <output.rbxl>")?;
	let dom = WeakDom::new(
		InstanceBuilder::new("DataModel")
			.with_name("Carbon dynamic-state evidence")
			.with_child(
				InstanceBuilder::new("Workspace")
					.with_name("Workspace")
					.with_child(
						InstanceBuilder::new("Part")
							.with_name("AuthoredRotVelocity")
							.with_property("Anchored", true)
							.with_property("RotVelocity", Vector3::new(1.25, -2.5, 3.75)),
					)
					.with_child(
						InstanceBuilder::new("UnionOperation")
							.with_name("AuthoredTriangleCount")
							.with_property("Anchored", true)
							.with_property("TriangleCount", 12_345_i32),
					),
			),
	);
	rbx_binary::to_writer(File::create(&output)?, &dom, dom.root().children())?;
	println!("{}", output.display());
	Ok(())
}
