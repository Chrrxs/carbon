#![allow(clippy::new_without_default)]

use rbx_dom_weak::{types::Variant, UstrMap};

pub(crate) mod artifact_resolution;
pub mod artifact_store;
pub(crate) mod capture_artifact;
pub mod capture_provider;
pub(crate) mod capture_store;
pub mod cli;
pub mod config;
pub mod constants;
pub mod core;
pub mod crash_handler;
pub mod ext;
pub mod installer;
pub mod logger;
pub(crate) mod manifest_identity;
pub mod place_diff;
pub mod privileged_bridge;
pub mod program;
pub mod project;
pub mod resolution;
pub mod rml;
pub mod server;
pub mod sessions;
pub mod source;
mod source_wire;
pub mod studio;
pub(crate) mod studio_plugin;
pub mod studio_windows;
pub mod util;

/// Global type for snapshot and instance properties
pub type Properties = UstrMap<Variant>;

/// A shorter way to lock the Mutex
#[macro_export]
macro_rules! lock {
	($mutex:expr) => {
		$mutex.lock().expect("Tried to lock Mutex that panicked!")
	};
}
