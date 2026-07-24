use actix_msgpack::MsgPack;
use actix_web::{post, web::Data, HttpResponse, Responder};
use log::trace;
use std::sync::Arc;

use crate::{
	core::Core,
	server::{privileged::purge_hidden_apply_preflights, AuthRequest},
};

#[post("/unsubscribe")]
async fn main(request: MsgPack<AuthRequest>, core: Data<Arc<Core>>) -> impl Responder {
	trace!("Received request: unsubscribe");

	let bridge_id = core
		.queue()
		.studio_route(request.client_id)
		.and_then(|route| route.bridge_id);
	let unsubscribed = core.queue().unsubscribe(request.client_id);

	if unsubscribed.is_ok() {
		purge_hidden_apply_preflights(request.client_id, bridge_id.as_deref());
		HttpResponse::Ok().body("Unsubscribed successfully")
	} else {
		HttpResponse::BadRequest().body("Not subscribed")
	}
}
