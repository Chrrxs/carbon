use actix_web::{get, post, web, web::Data, HttpResponse, Responder};
use serde::Deserialize;
use std::sync::Arc;

use crate::core::Core;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureOptions {
	#[serde(default)]
	managed_reload_transition_id: Option<String>,
}

#[post("/capture/request")]
pub(crate) async fn initiate(options: web::Query<CaptureOptions>, core: Data<Arc<Core>>) -> impl Responder {
	match core.begin_manifest_capture_mode_transition(options.managed_reload_transition_id.clone()) {
		Ok(capture_status) => HttpResponse::Ok().json(capture_status),
		Err(error) => capture_request_error(error),
	}
}

#[post("/capture/automatic")]
pub(crate) async fn automatic(core: Data<Arc<Core>>) -> impl Responder {
	match core.start_automatic_capture_monitor() {
		Ok(started) => HttpResponse::Ok().json(serde_json::json!({ "started": started })),
		Err(error) => capture_request_error(error),
	}
}

fn capture_request_error(error: anyhow::Error) -> HttpResponse {
	HttpResponse::Conflict().body(format!("{error:#}"))
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn genuine_capture_conflicts_remain_conflicts() {
		let response = capture_request_error(anyhow::anyhow!("another Capture Manifest operation is already running"));
		assert_eq!(response.status(), actix_web::http::StatusCode::CONFLICT);
	}
}
