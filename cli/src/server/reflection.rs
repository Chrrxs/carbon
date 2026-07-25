use actix_msgpack::MsgPackResponseBuilder;
use actix_web::{get, HttpResponse, Responder};
use serde::Serialize;

use crate::util;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReflectionResponse<'a> {
	version: &'a [u32; 4],
	api_dump: &'a str,
}

#[get("/reflection")]
pub(crate) async fn main() -> impl Responder {
	let snapshot = util::get_reflection_snapshot();
	HttpResponse::Ok().msgpack(ReflectionResponse {
		version: &snapshot.version,
		api_dump: &snapshot.api_dump,
	})
}
