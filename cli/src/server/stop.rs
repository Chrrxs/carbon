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
	fn concurrent_shutdown_signal_and_stop_convergence() {
		use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

		let coordinator = Arc::new(crate::core::ShutdownCoordinator::new());
		let capture_calls = Arc::new(AtomicUsize::new(0));
		let cleanup_started = Arc::new(AtomicBool::new(false));

		let mut handles = Vec::new();
		for _ in 0..10 {
			let coordinator = Arc::clone(&coordinator);
			let capture_calls = Arc::clone(&capture_calls);
			let cleanup_started = Arc::clone(&cleanup_started);
			handles.push(std::thread::spawn(move || {
				let result = coordinator.execute_or_await(|| {
					capture_calls.fetch_add(1, Ordering::SeqCst);
					std::thread::sleep(Duration::from_millis(50));
					Ok("Capture Manifest completed".to_owned())
				});
				assert!(
					!cleanup_started.load(Ordering::SeqCst),
					"cleanup occurred before capture settled"
				);
				result
			}));
		}

		for handle in handles {
			let message = handle.join().unwrap().unwrap();
			assert_eq!(message, "Capture Manifest completed");
		}

		assert_eq!(capture_calls.load(Ordering::SeqCst), 1);
		cleanup_started.store(true, Ordering::SeqCst);

		let idempotent = coordinator
			.execute_or_await(|| {
				capture_calls.fetch_add(1, Ordering::SeqCst);
				Ok("Re-run should not happen".to_owned())
			})
			.unwrap();
		assert_eq!(idempotent, "Capture Manifest completed");
		assert_eq!(capture_calls.load(Ordering::SeqCst), 1);
	}

	#[test]
	fn disconnected_with_last_manifest_success() {
		let directory = std::env::temp_dir().join(format!("carbon-stop-disconnected-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest_path = directory.join("place.carbon");
		let tree = crate::core::tree::Tree::new(
			crate::core::snapshot::Snapshot::new()
				.with_id(rbx_dom_weak::types::Ref::new())
				.with_class("DataModel")
				.with_name("DisconnectedTest"),
		);
		crate::artifact_store::extract_tree(&tree, "DisconnectedTest".to_owned(), &manifest_path).unwrap();

		let core = Arc::new(Core::new_artifact(&manifest_path).unwrap());
		assert!(core.queue().single_listener_id().is_err());

		let result = core.capture_before_shutdown().unwrap();
		assert!(result.contains("Studio is disconnected; retained valid manifest"));
		assert!(result.contains("place.carbon"));

		std::fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn disconnected_no_manifest_failure() {
		let directory = std::env::temp_dir().join(format!("carbon-stop-no-manifest-{}", uuid::Uuid::new_v4()));
		std::fs::create_dir_all(&directory).unwrap();
		let manifest_path = directory.join("place.carbon");
		let tree = crate::core::tree::Tree::new(
			crate::core::snapshot::Snapshot::new()
				.with_id(rbx_dom_weak::types::Ref::new())
				.with_class("DataModel")
				.with_name("NoManifestTest"),
		);
		crate::artifact_store::extract_tree(&tree, "NoManifestTest".to_owned(), &manifest_path).unwrap();

		let core = Arc::new(Core::new_artifact(&manifest_path).unwrap());
		assert!(core.queue().single_listener_id().is_err());

		std::fs::remove_file(&manifest_path).unwrap();

		let error = core.capture_before_shutdown().unwrap_err();
		assert!(error.to_string().contains("active served project"), "{error:#}");

		std::fs::remove_dir_all(directory).unwrap();
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
