use actix_web::{post, web::Data, HttpResponse, Responder};
use anyhow::Result;
use log::{error, info, trace};
use std::{sync::Arc, time::Duration};

use crate::{core::Core, server::StopHandle};

fn settle_stop<Capture>(capture: Capture) -> Result<String>
where
	Capture: FnOnce() -> Result<String>,
{
	capture()
}

#[post("/stop")]
async fn main(core: Data<Arc<Core>>, stop_handle: Data<StopHandle>) -> impl Responder {
	trace!("Received request: stop");
	let Some(stop_handle) = stop_handle.get().cloned() else {
		return HttpResponse::InternalServerError().body("Carbon server stop handle is unavailable");
	};

	info!("Carbon stop requested; capturing the connected Studio place before shutdown");
	let shutdown_core = Arc::clone(core.get_ref());
	let capture =
		actix_web::rt::task::spawn_blocking(move || settle_stop(|| shutdown_core.capture_before_shutdown())).await;

	actix_web::rt::spawn(async move {
		// Let the response reach `carbon stop` before ending the HTTP workers.
		actix_web::rt::time::sleep(Duration::from_millis(50)).await;
		stop_handle.stop(false).await;
	});

	match capture {
		Ok(Ok(message)) => {
			info!("Automatic Capture Manifest completed before Carbon stop: {message}");
			HttpResponse::Ok().body(format!(
				"Capture Manifest completed: {message}. Carbon stopped successfully"
			))
		}
		Ok(Err(capture_error)) => {
			error!("automatic Capture Manifest before Carbon stop failed: {capture_error:#}");
			HttpResponse::InternalServerError().body(format!(
				"Automatic Capture Manifest before Carbon stop failed: {capture_error:#}. Carbon stopped without a successful final capture"
			))
		}
		Err(join_error) => {
			error!("automatic Capture Manifest worker before Carbon stop failed: {join_error}");
			HttpResponse::InternalServerError().body(format!(
				"Automatic Capture Manifest worker before Carbon stop failed: {join_error}. Carbon stopped without a successful final capture"
			))
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::cell::Cell;

	#[test]
	fn stop_waits_for_capture() {
		let capture_calls = Cell::new(0);
		let message = settle_stop(|| {
			capture_calls.set(capture_calls.get() + 1);
			Ok("Studio artifact committed".to_owned())
		})
		.unwrap();

		assert_eq!(message, "Studio artifact committed");
		assert_eq!(capture_calls.get(), 1);
	}

	#[test]
	fn stop_propagates_capture_failure() {
		let capture_calls = Cell::new(0);
		let error = settle_stop(|| {
			capture_calls.set(capture_calls.get() + 1);
			anyhow::bail!("capture failed")
		})
		.unwrap_err();

		assert_eq!(error.to_string(), "capture failed");
		assert_eq!(capture_calls.get(), 1);
	}
}
