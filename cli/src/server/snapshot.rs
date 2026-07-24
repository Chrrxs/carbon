use actix_msgpack::{MsgPack, MsgPackResponseBuilder};
use actix_web::{post, web::Data, HttpResponse, Responder};
use log::trace;
use rbx_dom_weak::types::Ref;
use serde::Deserialize;
use std::sync::Arc;

use crate::{
	artifact_store::{MappingSourcePage, SourceCursor},
	core::Core,
};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PageRequest {
	instance: Ref,
	#[serde(default)]
	cursor: Vec<Ref>,
	max_instances: Option<usize>,
	max_bytes: Option<usize>,
}

/// Stream hierarchy only. Properties are intentionally excluded and arrive
/// through `/snapshot/source-page` without ever being retained in Core.
#[post("/snapshot/page")]
pub(crate) async fn page(request: MsgPack<PageRequest>, core: Data<Arc<Core>>) -> impl Responder {
	trace!("Received request: snapshot page");
	let max_instances = request.max_instances.unwrap_or(512).clamp(1, 65_536);
	let max_bytes = request
		.max_bytes
		.unwrap_or(8 * 1024 * 1024)
		.clamp(64 * 1024, 16 * 1024 * 1024);
	match core.snapshot_page(request.instance, request.cursor.clone(), max_instances, max_bytes) {
		Ok(Some(page)) => HttpResponse::Ok().msgpack(page),
		Ok(None) => HttpResponse::NotFound().body("Snapshot root not found"),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourcePageRequest {
	cursor: Option<SourceCursor>,
	max_instances: Option<usize>,
	max_bytes: Option<usize>,
	#[serde(default)]
	metadata_only: bool,
	#[serde(default)]
	mapping_values: bool,
}

/// Stream canonical property/reference/blob payloads directly from the artifact.
#[post("/snapshot/source-page")]
pub(crate) async fn source_page(request: MsgPack<SourcePageRequest>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	let max_instances = request.max_instances.unwrap_or(4096).clamp(1, 4096);
	let max_bytes = request
		.max_bytes
		.unwrap_or(8 * 1024 * 1024)
		.clamp(64 * 1024, 16 * 1024 * 1024);
	match core.source_page(request.cursor, max_instances, max_bytes, request.metadata_only) {
		Ok(source) if request.mapping_values => HttpResponse::Ok().msgpack(MappingSourcePage(&source)),
		Ok(source) => HttpResponse::Ok().msgpack(source),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}
