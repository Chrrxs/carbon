use std::{
	env, fs,
	path::{Path, PathBuf},
	process::Command,
};

const STUDIO_PLUGIN_BUNDLE_ENV: &str = "CARBON_STUDIO_PLUGIN_BUNDLE";
const RML_BUNDLE_ENV: &str = "CARBON_RML_BUNDLE";
const RML_GENERATED_SOURCE: &str = "carbon_rml_bundle.rs";

fn resolve_bash() -> PathBuf {
	#[cfg(windows)]
	if let Ok(output) = Command::new("where.exe").arg("git.exe").output() {
		if output.status.success() {
			for line in String::from_utf8_lossy(&output.stdout).lines() {
				let git = Path::new(line.trim());
				let Some(root) = git.parent().and_then(Path::parent) else {
					continue;
				};
				let candidate = root.join("bin").join("bash.exe");
				if candidate.is_file() {
					return candidate;
				}
			}
		}
	}

	PathBuf::from("bash")
}

fn main() {
	let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
	let repository = manifest_dir.parent().expect("the CLI lives beneath the monorepo root");
	let version_script = repository.join("scripts/build-version");
	let identity_script = repository.join("scripts/build-identity");

	println!("cargo:rerun-if-env-changed=CARBON_BUILD_VERSION");
	println!("cargo:rerun-if-env-changed=CARBON_BUILD_IDENTITY");
	println!("cargo:rerun-if-env-changed={STUDIO_PLUGIN_BUNDLE_ENV}");
	println!("cargo:rerun-if-env-changed={RML_BUNDLE_ENV}");
	println!("cargo:rustc-check-cfg=cfg(carbon_bundled_studio_plugin)");
	println!("cargo:rustc-check-cfg=cfg(carbon_bundled_rml)");
	// The embedded component identity is derived from the product tree below.
	// Watching `.git/HEAD` is both redundant and invalid in linked worktrees,
	// where `.git` is a file and Cargo would rebuild this script forever.
	for input in [
		"Cargo.toml",
		"Cargo.lock",
		"cli",
		"rml",
		"studio-plugin",
		"qualification",
		"scripts",
	] {
		println!("cargo:rerun-if-changed={}", repository.join(input).display());
	}

	let version = match env::var("CARBON_BUILD_VERSION") {
		Ok(version) => version,
		Err(_) => {
			let output = Command::new(resolve_bash())
				.arg(&version_script)
				.current_dir(repository)
				.output()
				.expect("failed to execute scripts/build-version");
			assert!(
				output.status.success(),
				"scripts/build-version failed: {}",
				String::from_utf8_lossy(&output.stderr)
			);
			String::from_utf8(output.stdout)
				.expect("build version was not UTF-8")
				.trim()
				.to_owned()
		}
	};

	assert!(!version.is_empty(), "build version must not be empty");
	println!("cargo:rustc-env=CARBON_BUILD_VERSION={version}");
	let identity = match env::var("CARBON_BUILD_IDENTITY") {
		Ok(identity) => identity,
		Err(_) => {
			let output = Command::new(resolve_bash())
				.arg(&identity_script)
				.current_dir(repository)
				.output()
				.expect("failed to execute scripts/build-identity");
			assert!(
				output.status.success(),
				"scripts/build-identity failed: {}",
				String::from_utf8_lossy(&output.stderr)
			);
			String::from_utf8(output.stdout)
				.expect("build identity was not UTF-8")
				.trim()
				.to_owned()
		}
	};
	assert!(!identity.is_empty(), "build identity must not be empty");
	println!("cargo:rustc-env=CARBON_BUILD_IDENTITY={identity}");

	let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
	let profile = env::var("PROFILE").unwrap_or_default();
	let bundled_plugin = out_dir.join("Carbon.rbxm");
	if let Some(source) = env::var_os(STUDIO_PLUGIN_BUNDLE_ENV) {
		let source = PathBuf::from(source);
		println!("cargo:rerun-if-changed={}", source.display());
		let metadata = fs::metadata(&source).unwrap_or_else(|error| {
			panic!(
				"{STUDIO_PLUGIN_BUNDLE_ENV} does not identify a readable Carbon.rbxm at {}: {error}",
				source.display()
			)
		});
		assert!(metadata.is_file(), "{} is not a file", source.display());
		assert!(metadata.len() > 0, "{} is empty", source.display());
		fs::copy(&source, &bundled_plugin)
			.unwrap_or_else(|error| panic!("failed to bundle Carbon Studio plugin {}: {error}", source.display()));
		println!("cargo:rustc-cfg=carbon_bundled_studio_plugin");
	} else {
		assert!(
			profile != "release",
			"release builds require {STUDIO_PLUGIN_BUNDLE_ENV} to point to the matching Carbon.rbxm"
		);
		fs::write(&bundled_plugin, []).expect("failed to stage the empty development plugin bundle");
	}

	stage_rml_bundle(&out_dir, &profile);
}

fn stage_rml_bundle(out_dir: &Path, profile: &str) {
	let generated = out_dir.join(RML_GENERATED_SOURCE);
	let Some(source) = env::var_os(RML_BUNDLE_ENV).map(PathBuf::from) else {
		assert!(
			profile != "release",
			"release builds require {RML_BUNDLE_ENV} to point to the matching RML package directory"
		);
		fs::write(
			generated,
			"pub(super) const EMBEDDED_RML_FILES: &[(&str, &[u8])] = &[];\n",
		)
		.expect("failed to stage the empty development RML bundle");
		return;
	};

	assert!(
		source.is_dir(),
		"{RML_BUNDLE_ENV} is not a directory: {}",
		source.display()
	);
	let mut files = Vec::new();
	collect_rml_files(&source, &source, &mut files);
	assert!(
		!files.is_empty(),
		"{RML_BUNDLE_ENV} contains no files: {}",
		source.display()
	);
	files.sort_by(|left, right| left.0.cmp(&right.0));

	let mut rust = String::from("pub(super) const EMBEDDED_RML_FILES: &[(&str, &[u8])] = &[\n");
	for (relative, absolute) in files {
		println!("cargo:rerun-if-changed={}", absolute.display());
		rust.push_str(&format!(
			"\t({relative:?}, include_bytes!({absolute:?}) as &[u8]),\n",
			absolute = absolute.to_string_lossy()
		));
	}
	rust.push_str("];\n");
	fs::write(generated, rust).expect("failed to generate the embedded RML file table");
	println!("cargo:rustc-cfg=carbon_bundled_rml");
}

fn collect_rml_files(root: &Path, directory: &Path, files: &mut Vec<(String, PathBuf)>) {
	let mut entries = fs::read_dir(directory)
		.unwrap_or_else(|error| panic!("failed to read RML bundle directory {}: {error}", directory.display()))
		.collect::<Result<Vec<_>, _>>()
		.unwrap_or_else(|error| {
			panic!(
				"failed to enumerate RML bundle directory {}: {error}",
				directory.display()
			)
		});
	entries.sort_by_key(|entry| entry.file_name());
	for entry in entries {
		let path = entry.path();
		let metadata = fs::symlink_metadata(&path)
			.unwrap_or_else(|error| panic!("failed to inspect RML bundle entry {}: {error}", path.display()));
		assert!(
			!metadata.file_type().is_symlink(),
			"RML bundle contains a symlink: {}",
			path.display()
		);
		let relative = path.strip_prefix(root).expect("RML entry is beneath its bundle root");
		if relative == Path::new("RobloxModLoader/logs") {
			continue;
		}
		if metadata.is_dir() {
			collect_rml_files(root, &path, files);
		} else if metadata.is_file() {
			let relative = relative
				.to_str()
				.unwrap_or_else(|| panic!("RML bundle path is not UTF-8: {}", relative.display()))
				.replace('\\', "/");
			assert!(!relative.is_empty(), "RML bundle contains an empty path");
			files.push((relative, path));
		}
	}
}
