use actix_web::{get, post, web, web::Data, HttpResponse, Responder};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::core::{Core, ManifestCaptureStatus};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CaptureOptions {
	#[serde(default)]
	managed_reload_transition_id: Option<String>,
	/// The requesting plugin starts a Studio quick save when the response
	/// carries a token.
	#[serde(default)]
	quick_save: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureRequestResponse {
	#[serde(flatten)]
	status: ManifestCaptureStatus,
	#[serde(skip_serializing_if = "Option::is_none")]
	quick_save_token: Option<String>,
}

#[post("/capture/request")]
pub(crate) async fn initiate(options: web::Query<CaptureOptions>, core: Data<Arc<Core>>) -> impl Responder {
	let options = options.into_inner();
	let core = Arc::clone(core.get_ref());
	// Arming a quick save can wait for another Carbon session's quick save.
	let started = actix_web::rt::task::spawn_blocking(move || {
		let status = core.begin_manifest_capture_mode_transition(options.managed_reload_transition_id)?;
		let quick_save_token = if options.quick_save {
			core.arm_plugin_studio_quick_save()
		} else {
			None
		};
		Ok::<_, anyhow::Error>(CaptureRequestResponse {
			status,
			quick_save_token,
		})
	})
	.await;
	match started {
		Ok(Ok(response)) => HttpResponse::Ok().json(response),
		Ok(Err(error)) => capture_request_error(error),
		Err(error) => {
			HttpResponse::InternalServerError().body(format!("Capture Manifest request worker failed: {error}"))
		}
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

	#[test]
	fn capture_request_response_adds_the_quick_save_token_beside_the_status() {
		let status = ManifestCaptureStatus {
			request_id: "capture".to_owned(),
			state: "running".to_owned(),
			source_generation: "generation".to_owned(),
			message: None,
		};
		let with_token = serde_json::to_value(CaptureRequestResponse {
			status: status.clone(),
			quick_save_token: Some("token".to_owned()),
		})
		.unwrap();
		assert_eq!(with_token["requestId"], "capture");
		assert_eq!(with_token["state"], "running");
		assert_eq!(with_token["quickSaveToken"], "token");

		let without_token = serde_json::to_value(CaptureRequestResponse {
			status,
			quick_save_token: None,
		})
		.unwrap();
		assert!(without_token.get("quickSaveToken").is_none());
	}

	#[test]
	fn capture_requests_opt_in_to_plugin_quick_saves() {
		let options = web::Query::<CaptureOptions>::from_query("quickSave=true").unwrap();
		assert!(options.quick_save);
		let options = web::Query::<CaptureOptions>::from_query("managedReloadTransitionId=reload").unwrap();
		assert!(!options.quick_save);
		assert_eq!(options.managed_reload_transition_id.as_deref(), Some("reload"));
	}
}
