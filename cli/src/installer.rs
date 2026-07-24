use anyhow::Result;
use std::env;

use crate::ext::PathExt;

pub fn is_managed() -> bool {
	let path = match env::current_exe() {
		Ok(path) => path,
		Err(_) => return false,
	};
	!path.contains(&[".carbon", "bin"]) && (path.contains(&["bin"]) || path.contains(&["tool-storage"]))
}

/// Installation is owned by the qualification and release scripts. Running a
/// development binary must never copy itself into a second user installation
/// or mutate shell startup files.
pub fn verify(_is_managed: bool) -> Result<()> {
	Ok(())
}
