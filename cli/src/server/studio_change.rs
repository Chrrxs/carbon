use actix_msgpack::MsgPack;
use actix_web::{
	post,
	web::{Data, Json},
	HttpResponse, Responder,
};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

use crate::core::Core;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Acknowledgement {
	client_id: u32,
	request_id: String,
	change_generation: String,
}

#[post("/studio/change-generation")]
pub(crate) async fn acknowledge(request: MsgPack<Acknowledgement>, core: Data<Arc<Core>>) -> impl Responder {
	let request = request.0;
	match core.acknowledge_studio_change_generation(request.client_id, &request.request_id, request.change_generation) {
		Ok(()) => HttpResponse::Ok().body("Studio change generation acknowledged"),
		Err(error) => HttpResponse::Conflict().body(format!("{error:#}")),
	}
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QuickSaveFailure {
	client_id: u32,
	token: String,
	message: String,
}

#[post("/studio/quick-save/failure")]
pub(crate) async fn report_quick_save_failure(
	request: MsgPack<QuickSaveFailure>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	let request = request.0;
	match core.report_studio_quick_save_failure(request.client_id, &request.token, request.message) {
		Ok(()) => HttpResponse::Ok().body("Studio quick-save failure recorded"),
		Err(error) => HttpResponse::Conflict().body(format!("{error:#}")),
	}
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaytestStopRequest {
	client_id: u32,
}

/// The edit plugin asks the running playtest to end so it can quick-save.
#[post("/studio/playtest/stop")]
pub(crate) async fn request_playtest_stop(
	request: MsgPack<PlaytestStopRequest>,
	core: Data<Arc<Core>>,
) -> impl Responder {
	match core.request_playtest_stop(request.0.client_id) {
		Ok(()) => HttpResponse::Ok().body("Studio playtest stop requested"),
		Err(error) => HttpResponse::Conflict().body(format!("{error:#}")),
	}
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaytestStopWait {
	session_token: String,
}

/// Studio's HTTP requests time out at 30 seconds.
const PLAYTEST_STOP_POLL: Duration = Duration::from_secs(20);

/// A play server long-polls here; `stop` tells it to end its playtest.
#[post("/studio/playtest/await-stop")]
pub(crate) async fn await_playtest_stop(request: Json<PlaytestStopWait>, core: Data<Arc<Core>>) -> impl Responder {
	let core = Arc::clone(core.get_ref());
	let session_token = request.into_inner().session_token;
	let waited =
		actix_web::rt::task::spawn_blocking(move || core.await_playtest_stop(&session_token, PLAYTEST_STOP_POLL)).await;
	match waited {
		Ok(Ok(stop)) => HttpResponse::Ok().json(serde_json::json!({ "stop": stop })),
		Ok(Err(error)) => HttpResponse::Forbidden().body(format!("{error:#}")),
		Err(error) => HttpResponse::InternalServerError().body(format!("Studio playtest stop worker failed: {error}")),
	}
}
