use actix_msgpack::MsgPack;
use actix_web::{post, web::Data, HttpResponse, Responder};
use log::trace;
use serde::Deserialize;
use std::sync::Arc;

use crate::core::{queue::StudioRoute, Core};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Request {
	client_id: u32,
	name: String,
	#[serde(default)]
	studio_session_id: Option<String>,
	#[serde(default)]
	instance_id: Option<String>,
}

#[post("/subscribe")]
async fn main(request: MsgPack<Request>, core: Data<Arc<Core>>) -> impl Responder {
	trace!("Received request: subscribe");
	let request = request.0;
	if core.requires_hard_restart() {
		return HttpResponse::Conflict()
			.body("game.carbon.json changed; hard restart carbon serve before reconnecting Studio");
	}

	let studio_route = match (request.studio_session_id, request.instance_id) {
		(Some(studio_session_id), Some(instance_id)) => Some(StudioRoute {
			studio_session_id,
			instance_id,
		}),
		_ => None,
	};
	let subscribed = core.queue().subscribe(request.client_id, &request.name, studio_route);

	if subscribed.is_ok() {
		HttpResponse::Ok().body("Subscribed successfully")
	} else {
		HttpResponse::BadRequest().body("Already subscribed")
	}
}
