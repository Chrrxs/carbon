use anyhow::{ensure, Context, Result};
use log::{debug, trace, warn};
use serde::{Deserialize, Serialize};
use std::{
	collections::HashMap,
	fs::{self, File, OpenOptions},
	io::Write,
	path::{Path, PathBuf},
	process::{self, Command},
	thread,
};

use crate::{artifact_store, util};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Session {
	pub pid: u32,
	pub host: Option<String>,
	pub port: Option<u16>,
	#[serde(default)]
	pub studio_pid: Option<u32>,
	#[serde(default)]
	pub worktree: Option<PathBuf>,
}

impl Session {
	pub fn get_address(&self) -> Option<String> {
		if let Some(host) = &self.host {
			if let Some(port) = self.port {
				return Some(format!("http://{host}:{port}"));
			}
		}

		None
	}
}

#[derive(Serialize, Deserialize, Debug)]
struct Sessions {
	last_session: String,
	active_sessions: HashMap<String, Session>,
}

fn read_sessions(path: &Path) -> Result<Sessions> {
	if path.exists() {
		match toml::from_str(&fs::read_to_string(path)?) {
			Ok(sessions) => return Ok(sessions),
			Err(_) => warn!("Session data file is corrupted! Creating new one.."),
		}
	}

	Ok(Sessions {
		last_session: String::new(),
		active_sessions: HashMap::new(),
	})
}

fn registry_lock(directory: &Path) -> Result<File> {
	fs::create_dir_all(directory)?;
	let path = directory.join("sessions.lock");
	OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(&path)
		.with_context(|| format!("failed to open Carbon session lock {}", path.display()))
}

fn set_sessions_in(directory: &Path, sessions: &Sessions) -> Result<()> {
	let path = directory.join("sessions.toml");
	let temporary = directory.join(format!(".sessions-{}.tmp", uuid::Uuid::new_v4().simple()));
	let result = (|| -> Result<()> {
		let mut output = OpenOptions::new().create_new(true).write(true).open(&temporary)?;
		output.write_all(toml::to_string(sessions)?.as_bytes())?;
		output.sync_all()?;
		drop(output);
		artifact_store::install_artifact_file(&temporary, &path)?;
		#[cfg(unix)]
		File::open(directory)?.sync_all()?;
		Ok(())
	})();
	let _ = fs::remove_file(&temporary);
	result
}

fn get_sessions_in(directory: &Path) -> Result<Sessions> {
	let lock = registry_lock(directory)?;
	lock.lock_shared()
		.context("failed to lock Carbon sessions for reading")?;
	read_sessions(&directory.join("sessions.toml"))
}

fn mutate_sessions<T>(directory: &Path, mutate: impl FnOnce(&mut Sessions) -> Result<T>) -> Result<T> {
	let lock = registry_lock(directory)?;
	lock.lock().context("failed to lock Carbon sessions for update")?;
	let mut sessions = read_sessions(&directory.join("sessions.toml"))?;
	let result = mutate(&mut sessions)?;
	set_sessions_in(directory, &sessions)?;
	Ok(result)
}

fn get_sessions() -> Result<Sessions> {
	get_sessions_in(&util::get_carbon_dir()?)
}

fn register_in(directory: &Path, id: Option<String>, session: Session) -> Result<String> {
	mutate_sessions(directory, |sessions| {
		let id = id.unwrap_or_else(|| generate_id(sessions));
		sessions.last_session.clone_from(&id);
		sessions.active_sessions.insert(id.clone(), session);
		Ok(id)
	})
}

pub fn add(id: Option<String>, session: Session, run_async: bool) -> Result<String> {
	let id = register_in(&util::get_carbon_dir()?, id, session.clone())?;

	if !run_async {
		ctrlc::set_handler(move || {
			match remove(&session) {
				Ok(()) => trace!("Session entry removed"),
				Err(err) => warn!("Failed to remove session entry: {err}"),
			}

			process::exit(0);
		})?;
	}

	// Schedule manual cleanup of old sessions
	// as ctrlc handler does not work on Windows,
	// on UNIX cleanup will remove crashed sessions
	thread::spawn(move || match cleanup() {
		Ok(()) => debug!("Session cleanup completed"),
		Err(err) => warn!("Failed to cleanup sessions: {err}"),
	});

	Ok(id)
}

pub fn get(id: Option<String>, host: Option<String>, port: Option<u16>) -> Result<Option<Session>> {
	let sessions = get_sessions()?;
	Ok(find_session(&sessions, id.as_deref(), host.as_deref(), port))
}

pub fn detect_worktree(path: &Path) -> Result<Option<PathBuf>> {
	let directory = if path.is_file() {
		path.parent()
			.with_context(|| format!("{} does not have a parent directory", path.display()))?
	} else {
		path
	};
	let output = Command::new("git")
		.args(["rev-parse", "--show-toplevel"])
		.current_dir(directory)
		.output()
		.with_context(|| format!("failed to inspect the Git worktree containing {}", path.display()))?;
	if !output.status.success() {
		return Ok(None);
	}
	let root = String::from_utf8(output.stdout).context("Git returned a non-UTF-8 worktree path")?;
	let root = root.trim();
	ensure!(!root.is_empty(), "Git returned an empty worktree path");
	Ok(Some(fs::canonicalize(root).with_context(|| {
		format!("failed to canonicalize Git worktree {root}")
	})?))
}

pub fn get_by_worktree(path: &Path) -> Result<Option<Session>> {
	let worktree =
		detect_worktree(path)?.with_context(|| format!("{} is not inside a Git worktree", path.display()))?;
	find_worktree_session(&get_sessions()?, &worktree)
}

fn find_session(sessions: &Sessions, id: Option<&str>, host: Option<&str>, port: Option<u16>) -> Option<Session> {
	if id.is_none() && host.is_none() && port.is_none() {
		return sessions.active_sessions.get(&sessions.last_session).cloned();
	} else if let Some(id) = id {
		return sessions.active_sessions.get(id).cloned();
	}

	sessions
		.active_sessions
		.values()
		.find(|session| {
			host.is_none_or(|host| session.host.as_deref() == Some(host))
				&& port.is_none_or(|port| session.port == Some(port))
		})
		.cloned()
}

fn find_worktree_session(sessions: &Sessions, worktree: &Path) -> Result<Option<Session>> {
	let matches = sessions
		.active_sessions
		.values()
		.filter(|session| session.worktree.as_deref() == Some(worktree))
		.collect::<Vec<_>>();
	ensure!(
		matches.len() <= 1,
		"multiple running Carbon serve sessions are registered for worktree {}; use an instance ID or --port",
		worktree.display()
	);
	Ok(matches.first().map(|session| (*session).clone()))
}

fn replace_id_in(directory: &Path, session: &Session, id: String) -> Result<()> {
	ensure!(!id.is_empty(), "Session ID cannot be empty");
	mutate_sessions(directory, |sessions| {
		let previous_id = sessions
			.active_sessions
			.iter()
			.find_map(|(id, candidate)| (candidate == session).then(|| id.clone()))
			.context("Session not found")?;
		if previous_id == id {
			return Ok(());
		}
		ensure!(
			!sessions.active_sessions.contains_key(&id),
			"Session ID {id} is already registered"
		);

		sessions.active_sessions.remove(&previous_id);
		sessions.active_sessions.insert(id.clone(), session.clone());
		if sessions.last_session == previous_id {
			sessions.last_session = id.clone();
		}
		Ok(())
	})
}

pub fn replace_id(session: &Session, id: String) -> Result<()> {
	replace_id_in(&util::get_carbon_dir()?, session, id)
}

pub fn get_multiple(ids: &Vec<String>) -> Result<HashMap<String, Session>> {
	let sessions = get_sessions()?;

	let mut result = HashMap::new();

	for id in ids {
		if let Some(session) = sessions.active_sessions.get(id) {
			result.insert(id.to_owned(), session.to_owned());
		}
	}

	Ok(result)
}

pub fn get_all() -> Result<HashMap<String, Session>> {
	Ok(get_sessions()?.active_sessions)
}

pub fn remove(session: &Session) -> Result<()> {
	mutate_sessions(&util::get_carbon_dir()?, |sessions| {
		let id = sessions
			.active_sessions
			.iter()
			.find_map(|(i, s)| if s == session { Some(i.clone()) } else { None })
			.context("Session not found")?;

		sessions.active_sessions.remove(&id);

		if sessions.last_session == id {
			sessions.last_session = sessions.active_sessions.keys().next().cloned().unwrap_or_default();
		}
		Ok(())
	})
}

pub fn remove_multiple(ids: &Vec<String>) -> Result<()> {
	mutate_sessions(&util::get_carbon_dir()?, |sessions| {
		for id in ids {
			sessions.active_sessions.remove(id);
		}

		sessions.last_session = sessions.active_sessions.keys().next().cloned().unwrap_or_default();
		Ok(())
	})
}

pub fn remove_all() -> Result<()> {
	mutate_sessions(&util::get_carbon_dir()?, |sessions| {
		sessions.last_session.clear();
		sessions.active_sessions.clear();
		Ok(())
	})
}

fn remove_matching_in(directory: &Path, expected: &HashMap<String, Session>) -> Result<()> {
	mutate_sessions(directory, |sessions| {
		for (id, session) in expected {
			if sessions.active_sessions.get(id) == Some(session) {
				sessions.active_sessions.remove(id);
			}
		}
		if !sessions.active_sessions.contains_key(&sessions.last_session) {
			sessions.last_session = sessions.active_sessions.keys().next().cloned().unwrap_or_default();
		}
		Ok(())
	})
}

pub fn remove_matching(expected: &HashMap<String, Session>) -> Result<()> {
	remove_matching_in(&util::get_carbon_dir()?, expected)
}

fn cleanup() -> Result<()> {
	mutate_sessions(&util::get_carbon_dir()?, |sessions| {
		for (id, session) in sessions.active_sessions.clone() {
			if !util::process_exists(session.pid) {
				sessions.active_sessions.remove(&id);
			}
		}
		if !sessions.active_sessions.contains_key(&sessions.last_session) {
			sessions.last_session = sessions.active_sessions.keys().next().cloned().unwrap_or_default();
		}
		Ok(())
	})
}

fn generate_id(sessions: &Sessions) -> String {
	let mut index = 0;

	loop {
		let id = index.to_string();

		if !sessions.active_sessions.contains_key(&id) {
			return id;
		}

		index += 1;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::{
		sync::{Arc, Barrier},
		time::{SystemTime, UNIX_EPOCH},
	};

	#[test]
	fn session_registration_returns_its_instance_id() {
		type RegisterSession = fn(Option<String>, Session, bool) -> Result<String>;
		let _: RegisterSession = add;
	}

	#[test]
	fn merge_stress_regression_mcp_instance_id_replaces_direct_session_selector() {
		let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		let directory = std::env::temp_dir().join(format!("carbon-direct-session-id-{unique}"));
		fs::create_dir_all(&directory).unwrap();
		let session = Session {
			pid: 10,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8000),
			studio_pid: Some(20),
			worktree: Some(PathBuf::from("/tmp/direct-worktree")),
		};
		assert_eq!(register_in(&directory, None, session.clone()).unwrap(), "0");

		replace_id_in(&directory, &session, "anon:direct-carbon".to_owned()).unwrap();

		let sessions = get_sessions_in(&directory).unwrap();
		assert_eq!(
			find_session(&sessions, Some("anon:direct-carbon"), None, None),
			Some(session)
		);
		assert!(find_session(&sessions, Some("0"), None, None).is_none());
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn parallel_serve_session_registrations_are_lossless() {
		let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		let directory = std::env::temp_dir().join(format!("carbon-parallel-sessions-{unique}"));
		fs::create_dir_all(&directory).unwrap();
		let workers = 24;
		let barrier = Arc::new(Barrier::new(workers));
		let threads = (0..workers)
			.map(|index| {
				let directory = directory.clone();
				let barrier = Arc::clone(&barrier);
				thread::spawn(move || {
					barrier.wait();
					register_in(
						&directory,
						None,
						Session {
							pid: 10_000 + index as u32,
							host: Some("127.0.0.1".to_owned()),
							port: Some(8100 + index as u16),
							studio_pid: Some(20_000 + index as u32),
							worktree: None,
						},
					)
					.unwrap()
				})
			})
			.collect::<Vec<_>>();
		let ids = threads
			.into_iter()
			.map(|thread| thread.join().unwrap())
			.collect::<Vec<_>>();

		let sessions = get_sessions_in(&directory).unwrap();
		assert_eq!(sessions.active_sessions.len(), workers);
		assert!(ids.iter().all(|id| sessions.active_sessions.contains_key(id)));
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn parallel_serve_target_filters_must_all_match_one_session() {
		let first = Session {
			pid: 1,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8000),
			studio_pid: Some(10),
			worktree: Some(PathBuf::from("/tmp/first")),
		};
		let second = Session {
			pid: 2,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8001),
			studio_pid: Some(11),
			worktree: Some(PathBuf::from("/tmp/second")),
		};
		let sessions = Sessions {
			last_session: "first".to_owned(),
			active_sessions: HashMap::from([("first".to_owned(), first), ("second".to_owned(), second.clone())]),
		};

		assert_eq!(
			find_session(&sessions, None, Some("127.0.0.1"), Some(8001)),
			Some(second)
		);
		assert_eq!(find_session(&sessions, None, Some("localhost"), Some(8001)), None);
	}

	#[test]
	fn parallel_serve_snapshot_removal_preserves_reused_session_ids() {
		let unique = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
		let directory = std::env::temp_dir().join(format!("carbon-parallel-session-removal-{unique}"));
		fs::create_dir_all(&directory).unwrap();
		let old = Session {
			pid: 10,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8000),
			studio_pid: Some(20),
			worktree: Some(PathBuf::from("/tmp/old")),
		};
		register_in(&directory, Some("0".to_owned()), old.clone()).unwrap();
		let snapshot = HashMap::from([("0".to_owned(), old)]);
		let replacement = Session {
			pid: 11,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8001),
			studio_pid: Some(21),
			worktree: Some(PathBuf::from("/tmp/replacement")),
		};
		register_in(&directory, Some("0".to_owned()), replacement.clone()).unwrap();

		remove_matching_in(&directory, &snapshot).unwrap();

		assert_eq!(
			get_sessions_in(&directory).unwrap().active_sessions.get("0"),
			Some(&replacement)
		);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn legacy_sessions_default_focus_metadata() {
		let session: Session = toml::from_str(
			r#"
pid = 41
host = "127.0.0.1"
port = 8000
"#,
		)
		.unwrap();

		assert_eq!(session.studio_pid, None);
		assert_eq!(session.worktree, None);
	}

	#[test]
	fn worktree_focus_selects_only_the_exact_registered_session() {
		let first = Session {
			pid: 1,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8000),
			studio_pid: Some(101),
			worktree: Some(PathBuf::from("/tmp/carbon-first")),
		};
		let second = Session {
			pid: 2,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8001),
			studio_pid: Some(102),
			worktree: Some(PathBuf::from("/tmp/carbon-second")),
		};
		let sessions = Sessions {
			last_session: "first".to_owned(),
			active_sessions: HashMap::from([("first".to_owned(), first), ("second".to_owned(), second.clone())]),
		};

		assert_eq!(
			find_worktree_session(&sessions, Path::new("/tmp/carbon-second")).unwrap(),
			Some(second)
		);
		assert!(find_worktree_session(&sessions, Path::new("/tmp/carbon-other"))
			.unwrap()
			.is_none());
	}

	#[test]
	fn duplicate_worktree_sessions_require_an_unambiguous_target() {
		let session = Session {
			pid: 1,
			host: Some("127.0.0.1".to_owned()),
			port: Some(8000),
			studio_pid: Some(101),
			worktree: Some(PathBuf::from("/tmp/carbon-duplicate")),
		};
		let sessions = Sessions {
			last_session: "first".to_owned(),
			active_sessions: HashMap::from([
				("first".to_owned(), session.clone()),
				(
					"second".to_owned(),
					Session {
						port: Some(8001),
						..session
					},
				),
			]),
		};

		let error = find_worktree_session(&sessions, Path::new("/tmp/carbon-duplicate")).unwrap_err();
		assert!(error.to_string().contains("multiple running Carbon serve sessions"));
	}
}
