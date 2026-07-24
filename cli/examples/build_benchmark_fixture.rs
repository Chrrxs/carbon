use anyhow::{ensure, Context, Result};
use carbon::{artifact_store, core::snapshot::Snapshot, Properties};
use rbx_dom_weak::{
	types::{CFrame, Color3, Enum, Matrix3, Ref, Variant, Vector3},
	Ustr,
};
use serde_json::json;
use std::{env, fs, path::PathBuf, time::Instant};

fn stable_ref(ordinal: usize) -> Ref {
	let block = ordinal / (u16::MAX as usize + 1) + 1;
	let counter = ordinal % (u16::MAX as usize + 1);
	Ref::some(((block as u128) << 16) | counter as u128)
}

fn fixture_snapshot(instances: usize, shape: &str) -> Snapshot {
	let mut children = Vec::with_capacity(instances - 2);
	for ordinal in 2..instances {
		let name = format!("Node{}", ordinal % 1024);
		let class = match shape {
			"lean" => "Folder",
			"valued" => "BoolValue",
			"part" => "Part",
			_ => unreachable!(),
		};
		let mut snapshot = Snapshot::new()
			.with_id(stable_ref(ordinal))
			.with_name(&name)
			.with_class(class);
		if shape == "valued" {
			let mut properties = Properties::default();
			properties.insert(Ustr::from("Value"), Variant::Bool(ordinal % 2 == 0));
			snapshot = snapshot.with_properties(properties);
		} else if shape == "part" {
			let varying = (ordinal % 1024) as f32;
			let mut properties = Properties::default();
			properties.insert(Ustr::from("Anchored"), Variant::Bool(ordinal % 2 == 0));
			properties.insert(Ustr::from("CanCollide"), Variant::Bool(ordinal % 3 != 0));
			properties.insert(Ustr::from("CanQuery"), Variant::Bool(ordinal % 5 != 0));
			properties.insert(Ustr::from("CanTouch"), Variant::Bool(ordinal % 7 != 0));
			properties.insert(Ustr::from("CastShadow"), Variant::Bool(ordinal % 11 != 0));
			properties.insert(Ustr::from("Massless"), Variant::Bool(ordinal % 13 == 0));
			properties.insert(
				Ustr::from("Transparency"),
				Variant::Float32((ordinal % 10) as f32 / 10.0),
			);
			properties.insert(Ustr::from("Reflectance"), Variant::Float32((ordinal % 5) as f32 / 10.0));
			properties.insert(
				Ustr::from("Color"),
				Variant::Color3(Color3::new(varying / 1024.0, 0.5, 1.0 - varying / 1024.0)),
			);
			properties.insert(Ustr::from("Size"), Variant::Vector3(Vector3::new(4.0, 1.0, 2.0)));
			properties.insert(
				Ustr::from("CFrame"),
				Variant::CFrame(CFrame::new(
					Vector3::new(varying, 0.0, varying / 2.0),
					Matrix3::identity(),
				)),
			);
			properties.insert(Ustr::from("Material"), Variant::Enum(Enum::from_u32(256)));
			snapshot = snapshot.with_properties(properties);
		}
		children.push(snapshot);
	}
	Snapshot::new()
		.with_id(stable_ref(0))
		.with_name("DataModel")
		.with_class("DataModel")
		.with_children(vec![Snapshot::new()
			.with_id(stable_ref(1))
			.with_name("Workspace")
			.with_class("Workspace")
			.with_children(children)])
}

fn main() -> Result<()> {
	let mut args = env::args().skip(1);
	let root = PathBuf::from(
		args.next()
			.context("usage: build_benchmark_fixture <output-dir> [instances] [lean|valued|part]")?,
	);
	let instances = args
		.next()
		.as_deref()
		.unwrap_or("300000")
		.parse::<usize>()
		.context("instance count must be an integer")?;
	ensure!(instances >= 2, "fixture needs at least two instances");
	let shape = args.next().unwrap_or_else(|| "valued".to_owned());
	ensure!(
		matches!(shape.as_str(), "lean" | "valued" | "part"),
		"shape must be lean, valued, or part"
	);
	ensure!(args.next().is_none(), "too many arguments");

	fs::create_dir_all(&root)?;
	let started = Instant::now();
	let snapshot = fixture_snapshot(instances, &shape);
	eprintln!(
		"constructed {instances} {shape} instances in {:.3}s",
		started.elapsed().as_secs_f64()
	);

	let studio = root.join("studio-only");
	let hybrid = root.join("hybrid");
	fs::create_dir_all(studio.join("game.carbon.data"))?;
	fs::create_dir_all(hybrid.join("game.carbon.data"))?;
	let studio_artifact = studio.join("game.carbon.data/state.carbon");
	let started = Instant::now();
	let report = artifact_store::extract_snapshot(snapshot, "BuildBenchmark".to_owned(), &studio_artifact)?;
	eprintln!(
		"wrote source artifact ({} instances, {} properties) in {:.3}s",
		report.instances,
		report.properties,
		started.elapsed().as_secs_f64()
	);
	fs::copy(&studio_artifact, hybrid.join("game.carbon.data/state.carbon"))?;

	fs::write(
		studio.join("game.carbon.json"),
		serde_json::to_vec_pretty(&json!({
			"name": "BuildBenchmark",
			"tree": {"$className": "DataModel"}
		}))?,
	)?;
	fs::write(
		hybrid.join("game.carbon.json"),
		serde_json::to_vec_pretty(&json!({
			"name": "BuildBenchmark",
			"tree": {
				"$className": "DataModel",
				"ServerScriptService": {
					"Mapped": {"$path": "Mapped.server.luau"}
				}
			}
		}))?,
	)?;
	fs::write(hybrid.join("Mapped.server.luau"), "return true\n")?;

	println!("{}", studio.join("game.carbon.json").display());
	println!("{}", hybrid.join("game.carbon.json").display());
	Ok(())
}
