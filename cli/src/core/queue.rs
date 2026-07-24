use anyhow::{bail, Result};
use colored::Colorize;
use crossbeam_channel::{Receiver, Sender};
use std::{
	collections::HashMap,
	sync::{Mutex, RwLock},
};

use crate::{
	carbon_warn,
	config::Config,
	constants::QUEUE_TIMEOUT,
	server::{self, Message},
};

macro_rules! read {
	($rwlock:expr) => {
		$rwlock.read().unwrap()
	};
}

macro_rules! write {
	($rwlock:expr) => {
		$rwlock.write().unwrap()
	};
}

#[derive(Debug, Clone)]
struct Listener {
	pub id: u32,
	pub name: String,
	pub studio_route: Option<StudioRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudioRoute {
	pub studio_session_id: String,
	pub instance_id: String,
	pub bridge_id: Option<String>,
	pub manifest_identities_authoritative: bool,
}

#[derive(Debug)]
struct Channel {
	sender: Sender<Message>,
	receiver: Receiver<Message>,
}

pub struct Queue {
	queues: RwLock<HashMap<u32, Channel>>,
	listeners: RwLock<Vec<Listener>>,
	unsynced_changes: RwLock<usize>,
	first_subscribe: Mutex<Option<FirstSubscribe>>,
}

type FirstSubscribe = Box<dyn FnOnce(&str, Option<&StudioRoute>) + Send + 'static>;

impl Queue {
	const CHANNEL_CAPACITY: usize = 64;

	pub fn new() -> Self {
		Self {
			queues: RwLock::new(HashMap::new()),
			listeners: RwLock::new(Vec::new()),
			unsynced_changes: RwLock::new(0),
			first_subscribe: Mutex::new(None),
		}
	}

	pub fn on_first_subscribe<F>(&self, callback: F)
	where
		F: FnOnce(&str, Option<&StudioRoute>) + Send + 'static,
	{
		*self.first_subscribe.lock().unwrap() = Some(Box::new(callback));
	}

	pub fn push<M>(&self, message: M, id: Option<u32>) -> Result<()>
	where
		M: Into<Message>,
	{
		if let Some(id) = id {
			if !self.is_subscribed(id) {
				bail!("Not subscribed")
			}

			let queues = read!(self.queues);
			let sender = queues.get(&id).unwrap().sender.clone();
			drop(queues);

			sender.send(message.into())?;

			return Ok(());
		}

		let message: Message = message.into();
		let mut did_push = false;

		let senders: Vec<_> = {
			let listeners = read!(self.listeners);
			let queues = read!(self.queues);
			listeners
				.iter()
				.filter_map(|listener| queues.get(&listener.id).map(|channel| channel.sender.clone()))
				.collect()
		};
		for sender in senders {
			sender.send(message.clone())?;
			did_push = true;
		}

		if !did_push {
			let max_unsynced_changes = Config::new().max_unsynced_changes;
			let mut unsynced_changes = write!(self.unsynced_changes);

			*unsynced_changes += 1;

			if max_unsynced_changes > 0 && *unsynced_changes >= max_unsynced_changes {
				carbon_warn!(
					"There are {} unsynced changes. Connect at least one client to this server or increase max_unsynced_changes setting to suppress this warning",
					unsynced_changes.to_string().bold()
				);
			}
		}

		Ok(())
	}

	pub fn get_timeout(&self, id: u32) -> Result<Option<Message>> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		let queues = read!(self.queues);
		let receiver = queues.get(&id).unwrap().receiver.clone();

		drop(queues);

		Ok(receiver.recv_timeout(QUEUE_TIMEOUT).ok())
	}

	pub fn subscribe(&self, id: u32, name: &str, studio_route: Option<StudioRoute>) -> Result<()> {
		if self.is_subscribed(id) {
			bail!("Already subscribed")
		}
		if !read!(self.listeners).is_empty() {
			bail!("another Studio client is already connected")
		}

		let (sender, receiver) = crossbeam_channel::bounded(Self::CHANNEL_CAPACITY);
		let channel = Channel { sender, receiver };

		let callback_route = studio_route.clone();
		let listener = Listener {
			id,
			name: name.to_owned(),
			studio_route,
		};

		write!(self.listeners).push(listener);
		write!(self.queues).insert(id.to_owned(), channel);
		let callback = self.first_subscribe.lock().unwrap().take();
		if let Some(callback) = callback {
			callback(name, callback_route.as_ref());
		}

		Ok(())
	}

	pub fn unsubscribe(&self, id: u32) -> Result<()> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		write!(self.listeners).retain(|listener| listener.id != id);
		write!(self.queues).remove(&id);

		Ok(())
	}

	pub fn disconnect(&self, message: &str, id: u32) -> Result<()> {
		if !self.is_subscribed(id) {
			bail!("Not subscribed")
		}

		self.push(
			server::Disconnect {
				message: message.to_owned(),
			},
			Some(id),
		)?;

		Ok(())
	}

	pub fn is_subscribed(&self, id: u32) -> bool {
		read!(self.listeners).iter().any(|listener| listener.id == id)
	}

	pub fn single_listener_id(&self) -> Result<u32> {
		let listeners = read!(self.listeners);
		match listeners.as_slice() {
			[listener] => Ok(listener.id),
			[] => bail!("Capture Manifest requires one connected Studio client"),
			_ => bail!("Capture Manifest requires exactly one connected Studio client"),
		}
	}

	pub fn studio_route(&self, id: u32) -> Option<StudioRoute> {
		read!(self.listeners)
			.iter()
			.find(|listener| listener.id == id)
			.and_then(|listener| listener.studio_route.clone())
	}

	pub fn bind_studio_bridge(&self, id: u32, bridge_id: &str) -> Result<()> {
		let mut listeners = write!(self.listeners);
		let listener = listeners.iter_mut().find(|listener| listener.id == id);
		let Some(route) = listener.and_then(|listener| listener.studio_route.as_mut()) else {
			bail!("Studio route is unavailable")
		};
		route.bridge_id = Some(bridge_id.to_owned());
		Ok(())
	}

	pub fn mark_manifest_identities_authoritative(&self, id: u32) -> Result<()> {
		self.set_manifest_identities_authoritative(id, true)
	}

	pub fn set_manifest_identities_authoritative(&self, id: u32, authoritative: bool) -> Result<()> {
		let mut listeners = write!(self.listeners);
		let route = listeners
			.iter_mut()
			.find(|listener| listener.id == id)
			.and_then(|listener| listener.studio_route.as_mut())
			.ok_or_else(|| anyhow::anyhow!("Studio route is unavailable"))?;
		route.manifest_identities_authoritative = authoritative;
		Ok(())
	}

	pub fn get_first_non_internal_listener_name(&self) -> Option<String> {
		read!(self.listeners)
			.iter()
			.next()
			.map(|listener| listener.name.to_owned())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::{Arc, Mutex};

	fn route(session: &str, instance: &str, bridge: &str) -> StudioRoute {
		StudioRoute {
			studio_session_id: session.to_owned(),
			instance_id: instance.to_owned(),
			bridge_id: Some(bridge.to_owned()),
			manifest_identities_authoritative: false,
		}
	}

	#[test]
	fn studio_route_is_replaced_only_after_the_client_disconnects() {
		let queue = Queue::new();
		let first = route("plugin-session-a", "place:123", "0123456789abcdef0123456789abcdef");
		let second = route("plugin-session-b", "place:123", "fedcba9876543210fedcba9876543210");

		queue.subscribe(101, "same-place", Some(first.clone())).unwrap();
		assert_eq!(queue.studio_route(101), Some(first));
		assert!(queue.subscribe(202, "same-place", Some(second.clone())).is_err());

		queue.unsubscribe(101).unwrap();
		queue.subscribe(202, "same-place", Some(second.clone())).unwrap();
		assert_eq!(queue.studio_route(101), None);
		assert_eq!(queue.studio_route(202), Some(second));
	}

	#[test]
	fn capability_negotiation_binds_only_the_calling_studio_route() {
		let queue = Queue::new();
		let unbound = StudioRoute {
			studio_session_id: "plugin-session-a".to_owned(),
			instance_id: "anon:place-a".to_owned(),
			bridge_id: None,
			manifest_identities_authoritative: false,
		};
		queue.subscribe(101, "place-a", Some(unbound)).unwrap();

		queue
			.bind_studio_bridge(101, "0123456789abcdef0123456789abcdef")
			.unwrap();

		assert_eq!(
			queue.studio_route(101).and_then(|route| route.bridge_id),
			Some("0123456789abcdef0123456789abcdef".to_owned())
		);
		assert!(!queue.studio_route(101).unwrap().manifest_identities_authoritative);
		queue.mark_manifest_identities_authoritative(101).unwrap();
		assert!(queue.studio_route(101).unwrap().manifest_identities_authoritative);
		queue.set_manifest_identities_authoritative(101, false).unwrap();
		assert!(!queue.studio_route(101).unwrap().manifest_identities_authoritative);
	}

	#[test]
	fn only_one_studio_client_may_subscribe() {
		let queue = Queue::new();
		queue.subscribe(101, "first", None).unwrap();
		let error = queue.subscribe(202, "second", None).unwrap_err().to_string();
		assert!(error.contains("another Studio client"));
		queue.unsubscribe(101).unwrap();
		queue.subscribe(202, "second", None).unwrap();
		assert_eq!(queue.single_listener_id().unwrap(), 202);
	}

	#[test]
	fn first_successful_subscription_reports_connection_once() {
		let queue = Queue::new();
		let connections = Arc::new(Mutex::new(Vec::new()));
		let observed = Arc::clone(&connections);
		queue.on_first_subscribe(move |name, route| {
			observed
				.lock()
				.unwrap()
				.push((name.to_owned(), route.map(|route| route.instance_id.clone())))
		});

		queue
			.subscribe(101, "first", Some(route("studio-session", "anon:first", "bridge")))
			.unwrap();
		queue.unsubscribe(101).unwrap();
		queue.subscribe(202, "second", None).unwrap();

		assert_eq!(
			*connections.lock().unwrap(),
			[("first".to_owned(), Some("anon:first".to_owned()))]
		);
	}
}
