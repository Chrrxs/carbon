use actix_web::{get, post, web, web::Data, HttpResponse, Responder};
use serde::Deserialize;
use std::sync::Arc;

use crate::core::Core;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureOptions {
	#[serde(default)]
	force_full: bool,
}

#[post("/capture/request")]
pub(crate) async fn initiate(options: web::Query<CaptureOptions>, core: Data<Arc<Core>>) -> impl Responder {
	match core.begin_manifest_capture_mode(options.force_full) {
		Ok(capture_status) => HttpResponse::Ok().json(capture_status),
		Err(error) => HttpResponse::Conflict().body(format!("{error:#}")),
	}
}

#[get("/capture/status/{request_id}")]
pub(crate) async fn get_status(request_id: web::Path<String>, core: Data<Arc<Core>>) -> impl Responder {
	match core.manifest_capture_status(&request_id) {
		Ok(capture_status) => HttpResponse::Ok().json(capture_status),
		Err(error) => HttpResponse::NotFound().body(format!("{error:#}")),
	}
}

#[post("/capture/cancel/{request_id}")]
pub(crate) async fn cancel(request_id: web::Path<String>, core: Data<Arc<Core>>) -> impl Responder {
	match core.cancel_manifest_capture(&request_id) {
		Ok(capture_status) => HttpResponse::Ok().json(capture_status),
		Err(error) => HttpResponse::Conflict().body(format!("{error:#}")),
	}
}
