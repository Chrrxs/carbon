use actix_files::NamedFile;
use actix_msgpack::MsgPack;
use actix_web::{post, web, web::Data, Either, HttpRequest, HttpResponse, Responder};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use std::{
	fs::{self, File},
	io::Read,
	sync::Arc,
	thread,
	time::{Duration, Instant, SystemTime},
};

use crate::{
	carbon_warn,
	core::{Core, QualificationExport},
	privileged_bridge::{Bridge, Capabilities},
};

const MAX_WARNINGS_PER_REPORT: usize = 4_096;
const MAX_WARNING_BYTES: usize = 16 * 1024;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Request {
	client_id: u32,
	warnings: Vec<String>,
}

#[post("/diagnostics/warnings")]
async fn warnings(request: MsgPack<Request>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	if !core.queue().is_subscribed(request.client_id) {
		return HttpResponse::Unauthorized().body("Not subscribed");
	}
	if request.warnings.len() > MAX_WARNINGS_PER_REPORT {
		return HttpResponse::PayloadTooLarge().body("too many Studio warnings");
	}
	if request
		.warnings
		.iter()
		.any(|warning| warning.is_empty() || warning.len() > MAX_WARNING_BYTES)
	{
		return HttpResponse::BadRequest().body("Studio warning is empty or too large");
	}

	for warning in request.warnings {
		carbon_warn!("Studio: {warning}");
	}
	HttpResponse::Ok().finish()
}

#[derive(Deserialize)]
struct LocalSaveQueued {
	queued: bool,
	#[serde(rename = "engineGeneration")]
	engine_generation: u64,
}

fn bearer_token(request: &HttpRequest) -> Option<&str> {
	request
		.headers()
		.get("authorization")?
		.to_str()
		.ok()?
		.strip_prefix("Bearer ")
}

fn valid_place_header(path: &std::path::Path) -> Result<()> {
	let mut header = [0_u8; 8];
	File::open(path)
		.with_context(|| format!("failed to open Studio-saved place {}", path.display()))?
		.read_exact(&mut header)
		.context("Studio-saved place is truncated")?;
	ensure!(
		header == *b"<roblox!" || header.starts_with(b"<roblox"),
		"Studio local save did not produce an RBXL document"
	);
	Ok(())
}

fn export_local_place(core: &Core, export: QualificationExport) -> Result<std::path::PathBuf> {
	let client_id = core.queue().single_listener_id()?;
	let route = core
		.queue()
		.studio_route(client_id)
		.context("connected Studio route is unavailable")?;
	let bridge_id = route.bridge_id.context("connected Studio RML bridge is not bound")?;
	let bridge = Bridge::discover(&bridge_id)?;
	let before_capabilities: Capabilities = bridge.get("v1/capabilities")?;
	ensure!(
		before_capabilities.local_place_save_diagnostic,
		"installed RML bridge does not support local place serialization"
	);
	ensure!(before_capabilities.engine_ready, "Studio edit DataModel is not ready");

	let before = fs::metadata(&export.path)
		.with_context(|| format!("qualification launch place is unavailable: {}", export.path.display()))?;
	let before_modified = before.modified().unwrap_or(SystemTime::UNIX_EPOCH);
	let before_len = before.len();
	let queued: LocalSaveQueued = bridge.post("v1/diagnostics/save-local-place", &serde_json::json!({}))?;
	ensure!(queued.queued, "RML did not queue Studio's local place save");
	ensure!(
		queued.engine_generation == before_capabilities.engine_generation,
		"edit DataModel changed while local place save was queued"
	);

	let deadline = Instant::now() + Duration::from_secs(60);
	let mut stable_since = None;
	let mut last_state = None;
	loop {
		ensure!(
			Instant::now() < deadline,
			"timed out waiting for Studio's local place save"
		);
		if let Ok(metadata) = fs::metadata(&export.path) {
			let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
			let state = (metadata.len(), modified);
			let changed = metadata.len() != before_len || modified != before_modified;
			if changed {
				if last_state == Some(state) {
					if stable_since.is_some_and(|started: Instant| started.elapsed() >= Duration::from_millis(300)) {
						valid_place_header(&export.path)?;
						let after_capabilities: Capabilities = bridge.get("v1/capabilities")?;
						ensure!(
							after_capabilities.process_id == before_capabilities.process_id
								&& after_capabilities.engine_generation == before_capabilities.engine_generation
								&& after_capabilities.engine_ready,
							"Studio edit DataModel changed during local place serialization"
						);
						return Ok(export.path);
					}
				} else {
					last_state = Some(state);
					stable_since = Some(Instant::now());
				}
			}
		}
		thread::sleep(Duration::from_millis(25));
	}
}

#[post("/diagnostics/qualification/export-place")]
pub(crate) async fn export_place(request: HttpRequest, core: Data<Arc<Core>>) -> Either<NamedFile, HttpResponse> {
	let Some(token) = bearer_token(&request) else {
		return Either::Right(HttpResponse::Unauthorized().body("qualification token is required"));
	};
	let export = match core.authorize_qualification_export(token) {
		Ok(export) => export,
		Err(error) => return Either::Right(HttpResponse::Unauthorized().body(error.to_string())),
	};
	let core = core.into_inner();
	match web::block(move || export_local_place(&core, export)).await {
		Ok(Ok(path)) => match NamedFile::open_async(path).await {
			Ok(file) => Either::Left(file),
			Err(error) => Either::Right(HttpResponse::InternalServerError().body(error.to_string())),
		},
		Ok(Err(error)) => Either::Right(HttpResponse::Conflict().body(format!("{error:#}"))),
		Err(error) => Either::Right(HttpResponse::InternalServerError().body(error.to_string())),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn warning_report_bounds_are_explicit() {
		assert_eq!(MAX_WARNINGS_PER_REPORT, 4_096);
		assert_eq!(MAX_WARNING_BYTES, 16 * 1024);
	}

	#[test]
	fn place_headers_accept_binary_and_xml_rbxl() {
		let root = std::env::temp_dir().join(format!("carbon-place-header-{}", uuid::Uuid::new_v4()));
		fs::write(&root, b"<roblox!payload").unwrap();
		assert!(valid_place_header(&root).is_ok());
		fs::write(&root, b"<roblox version").unwrap();
		assert!(valid_place_header(&root).is_ok());
		fs::write(&root, b"not-rbxl").unwrap();
		assert!(valid_place_header(&root).is_err());
		fs::remove_file(root).unwrap();
	}
}
