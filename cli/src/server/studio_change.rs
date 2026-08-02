use actix_msgpack::MsgPack;
use actix_web::{post, web::Data, HttpResponse, Responder};
use serde::Deserialize;
use std::sync::Arc;

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
