use actix_msgpack::{MsgPack, MsgPackResponseBuilder};
use actix_web::{error::BlockingError, post, web, web::Data, HttpResponse, Responder};
use log::trace;
use std::sync::Arc;

use crate::{
	core::{queue::Queue, Core},
	server::AuthRequest,
};

async fn receive(queue: Arc<Queue>, id: u32) -> Result<anyhow::Result<Option<crate::server::Message>>, BlockingError> {
	web::block(move || queue.get_timeout(id)).await
}

#[post("/read")]
async fn main(request: MsgPack<AuthRequest>, core: Data<Arc<Core>>) -> impl Responder {
	trace!("Received request: read");

	let id = request.client_id;
	let queue = core.queue();

	if !queue.is_subscribed(id) {
		return HttpResponse::Unauthorized().body("Not subscribed");
	}

	match receive(queue, id).await {
		Ok(Ok(message)) => HttpResponse::Ok().msgpack(message),
		Ok(Err(error)) => HttpResponse::InternalServerError().body(error.to_string()),
		Err(error) => HttpResponse::InternalServerError().body(error.to_string()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{Duration, Instant};

	#[actix_web::test]
	async fn long_poll_does_not_block_the_http_runtime() {
		let queue = Arc::new(Queue::new());
		queue.subscribe(7, "test", None).unwrap();
		let release_queue = queue.clone();
		let release = std::thread::spawn(move || {
			std::thread::sleep(Duration::from_millis(100));
			release_queue
				.push(
					crate::server::Disconnect {
						message: "release pending read".to_owned(),
					},
					Some(7),
				)
				.unwrap();
		});

		let started = Instant::now();
		let pending_read = actix_web::rt::spawn(receive(queue, 7));
		actix_web::rt::time::sleep(Duration::from_millis(10)).await;
		assert!(
			started.elapsed() < Duration::from_millis(50),
			"a blocking queue read occupied the HTTP runtime"
		);
		let message = pending_read.await.unwrap().unwrap().unwrap();
		assert!(message.is_some());
		release.join().unwrap();
	}
}
