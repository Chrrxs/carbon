use actix_web::{post, web::Data, HttpResponse, Responder};
use anyhow::Result;
use log::{error, info, trace};
use std::{
	sync::{atomic::Ordering, Arc},
	time::Duration,
};

use crate::{
	core::Core,
	server::{StopHandle, StopRequested},
};

const DISCONNECTED_STOP_MESSAGE: &str = "Studio disconnected; latest committed manifest retained";
const PENDING_RELOAD_STOP_MESSAGE: &str = "Synchronization reload incomplete; latest committed manifest retained";

fn settle_stop<Pending, Connected, Capture>(pending: Pending, connected: Connected, capture: Capture) -> Result<String>
where
	Pending: Fn() -> bool,
	Connected: Fn() -> bool,
	Capture: FnOnce() -> Result<String>,
{
	if pending() {
		return Ok(PENDING_RELOAD_STOP_MESSAGE.to_owned());
	}
	if !connected() {
		return Ok(DISCONNECTED_STOP_MESSAGE.to_owned());
	}
	match capture() {
		Ok(message) => Ok(message),
		Err(_) if pending() => Ok(PENDING_RELOAD_STOP_MESSAGE.to_owned()),
		Err(_) if !connected() => Ok(DISCONNECTED_STOP_MESSAGE.to_owned()),
		Err(error) => Err(error),
	}
}

#[post("/stop")]
async fn main(
	core: Data<Arc<Core>>,
	stop_handle: Data<StopHandle>,
	stop_requested: Data<StopRequested>,
) -> impl Responder {
	trace!("Received request: stop");
	let Some(stop_handle) = stop_handle.get().cloned() else {
		return HttpResponse::InternalServerError().body("Carbon server stop handle is unavailable");
	};
	stop_requested.store(true, Ordering::Release);

	info!("Carbon stop requested; capturing the connected Studio place before shutdown");
	let shutdown_core = Arc::clone(core.get_ref());
	let capture = actix_web::rt::task::spawn_blocking(move || {
		settle_stop(
			|| shutdown_core.has_pending_managed_reload(),
			|| shutdown_core.queue().has_subscribers(),
			|| shutdown_core.capture_before_shutdown(),
		)
	})
	.await;

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
		let message = settle_stop(
			|| false,
			|| true,
			|| {
				capture_calls.set(capture_calls.get() + 1);
				Ok("Studio artifact committed".to_owned())
			},
		)
		.unwrap();

		assert_eq!(message, "Studio artifact committed");
		assert_eq!(capture_calls.get(), 1);
	}

	#[test]
	fn stop_propagates_capture_failure() {
		let capture_calls = Cell::new(0);
		let error = settle_stop(
			|| false,
			|| true,
			|| {
				capture_calls.set(capture_calls.get() + 1);
				anyhow::bail!("capture failed")
			},
		)
		.unwrap_err();

		assert_eq!(error.to_string(), "capture failed");
		assert_eq!(capture_calls.get(), 1);
	}

	#[test]
	fn stop_retains_the_latest_capture_when_studio_is_already_disconnected() {
		let capture_calls = Cell::new(0);
		let message = settle_stop(
			|| false,
			|| false,
			|| {
				capture_calls.set(capture_calls.get() + 1);
				anyhow::bail!("capture must not run")
			},
		)
		.unwrap();

		assert_eq!(message, "Studio disconnected; latest committed manifest retained");
		assert_eq!(capture_calls.get(), 0);
	}

	#[test]
	fn stop_tolerates_a_disconnect_while_capture_starts() {
		let connected = Cell::new(true);
		let message = settle_stop(
			|| false,
			|| connected.get(),
			|| {
				connected.set(false);
				anyhow::bail!("Capture Manifest requires one connected Studio client")
			},
		)
		.unwrap();

		assert_eq!(message, "Studio disconnected; latest committed manifest retained");
	}

	#[test]
	fn stop_retains_the_latest_capture_during_managed_reload() {
		let capture_calls = Cell::new(0);
		let message = settle_stop(
			|| true,
			|| true,
			|| {
				capture_calls.set(capture_calls.get() + 1);
				anyhow::bail!("capture must not run")
			},
		)
		.unwrap();

		assert_eq!(message, PENDING_RELOAD_STOP_MESSAGE);
		assert_eq!(capture_calls.get(), 0);
	}
}
