use actix_msgpack::MsgPack;
use actix_web::{post, web::Data, HttpResponse, Responder};
use serde::Deserialize;
use std::sync::Arc;

use crate::{carbon_warn, core::Core};

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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn warning_report_bounds_are_explicit() {
		assert_eq!(MAX_WARNINGS_PER_REPORT, 4_096);
		assert_eq!(MAX_WARNING_BYTES, 16 * 1024);
	}
}
