use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use unicode_width::UnicodeWidthChar;

use crate::keys::*;
use crate::opencode::{Client, MODEL_CHOICES};
use crate::record::Event;

const CODENAMES: [&str; 8] = ["sam", "apple", "pine", "lake", "moss", "river", "stone", "cloud"];

#[derive(Clone, Debug)]
pub struct Member {
	pub name: String,
	pub model: String,
	pub role: Option<String>,
	pub session: String,
	seen: HashSet<String>,
	pub busy: bool,
	pub error: Option<String>,
	pub last_cost: Option<f64>,
}

#[derive(Clone, Debug)]
struct Task {
	id: u64,
	member: String,
	started: Instant,
	text: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordKind {
	Human,
	Reply,
	Suggestion,
	System,
	Error,
	Stopped,
}

#[derive(Clone, Debug)]
struct Record {
	kind: RecordKind,
	member: Option<String>,
	to: Option<String>,
	task: Option<u64>,
	text: String,
	at: u64,
	cost: Option<f64>,
	seconds: Option<f64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
	Model,
	Mention,
}

#[derive(Clone)]
struct PickerItem {
	value: String,
	label: String,
}

struct Picker {
	kind: PickerKind,
	items: Vec<PickerItem>,
	selected: usize,
}

struct Group {
	members: Vec<Member>,
	records: Vec<Record>,
	tasks: Vec<Task>,
	input: Vec<char>,
	cursor: usize,
	selected: usize,
	scroll: usize,
	picker: Option<Picker>,
	next_task: u64,
	total_cost: f64,
	cwd: String,
}

impl Group {
	fn new(cwd: String) -> Self {
		Self {
			members: Vec::new(),
			records: Vec::new(),
			tasks: Vec::new(),
			input: Vec::new(),
			cursor: 0,
			selected: 0,
			scroll: 0,
			picker: None,
			next_task: 1,
			total_cost: 0.0,
			cwd,
		}
	}

	fn push(&mut self, kind: RecordKind, text: String) {
		self.records.push(Record {
			kind,
			member: None,
			to: None,
			task: None,
			text,
			at: now(),
			cost: None,
			seconds: None,
		});
	}

	fn next_name(&self) -> String {
		for name in CODENAMES {
			if !self.members.iter().any(|member| member.name == name) {
				return String::from(name);
			}
		}
		format!("member{}", self.members.len() + 1)
	}
}

pub enum GroupAction {
	Open { name: String, session: String },
	Close { session: String },
}

pub struct GroupPane {
	data: Arc<Mutex<Group>>,
	client: Option<Client>,
	running: Arc<AtomicBool>,
	events: Arc<Mutex<Vec<Event>>>,
}

impl GroupPane {
	pub fn new(client: Option<Client>, cwd: String) -> Self {
		let data = Arc::new(Mutex::new(Group::new(cwd)));
		let running = Arc::new(AtomicBool::new(true));
		let events = Arc::new(Mutex::new(Vec::new()));
		if let Some(client) = client.clone() {
			let thread_data = data.clone();
			let thread_running = running.clone();
			thread::spawn(move || {
				while thread_running.load(Ordering::Relaxed) {
					poll_once(&thread_data, &client);
					thread::sleep(Duration::from_millis(600));
				}
			});
		} else if let Ok(mut group) = data.lock() {
			group.push(
				RecordKind::Error,
				String::from("opencode server unavailable; members cannot be added"),
			);
		}
		Self {
			data,
			client,
			running,
			events,
		}
	}

	pub fn take_events(&self) -> Vec<Event> {
		match self.events.lock() {
			Ok(mut events) => std::mem::take(&mut *events),
			Err(_) => Vec::new(),
		}
	}

	pub fn sessions(&self) -> Vec<(String, String)> {
		match self.data.lock() {
			Ok(group) => group
				.members
				.iter()
				.map(|member| (member.name.clone(), member.session.clone()))
				.collect(),
			Err(_) => Vec::new(),
		}
	}

	pub fn open_by_index(&self, index: usize) -> Option<GroupAction> {
		let groups = self.data.lock().ok()?;
		let member = groups.members.get(index)?;
		Some(GroupAction::Open {
			name: member.name.clone(),
			session: member.session.clone(),
		})
	}

	pub fn click(&self, row: usize) -> Option<GroupAction> {
		let mut group = self.data.lock().ok()?;
		if row < 2 {
			return None;
		}
		let index = row - 2;
		if index >= group.members.len() {
			return None;
		}
		group.selected = index;
		let member = &group.members[index];
		Some(GroupAction::Open {
			name: member.name.clone(),
			session: member.session.clone(),
		})
	}

	pub fn insert_str(&self, text: &str) {
		let Ok(mut group) = self.data.lock() else {
			return;
		};
		for ch in text.chars() {
			if ch == '\r' {
				continue;
			}
			let ch = if ch == '\n' { ' ' } else { ch };
			if (ch as u32) >= 0x20 && ch != '\u{7f}' {
				let cursor = group.cursor;
				group.input.insert(cursor, ch);
				group.cursor += 1;
			}
		}
		group.mention_picker();
	}

	pub fn on_key(&self, vk: u32, ctrl: bool, _shift: bool) -> Vec<GroupAction> {
		let mut actions = Vec::new();
		let Ok(mut group) = self.data.lock() else {
			return actions;
		};
		if group.picker.is_some() {
			match vk {
				VK_UP => {
					if let Some(picker) = group.picker.as_mut() {
						picker.selected = picker.selected.saturating_sub(1);
					}
					return actions;
				}
				VK_DOWN => {
					if let Some(picker) = group.picker.as_mut() {
						if picker.selected + 1 < picker.items.len() {
							picker.selected += 1;
						}
					}
					return actions;
				}
				VK_RETURN => {
					let picker = group.picker.take();
					if let Some(picker) = picker {
						match picker.kind {
							PickerKind::Model => {
								drop(group);
								if let Some(item) = picker.items.get(picker.selected) {
									if let Err(err) = self.add_member(&item.value, None) {
										if let Ok(mut group) = self.data.lock() {
											group.push(RecordKind::Error, err);
										}
									}
								}
								return actions;
							}
							PickerKind::Mention => {
								if let Some(item) = picker.items.get(picker.selected) {
									insert_at_cursor(&mut group, &item.value);
								}
							}
						}
					}
					return actions;
				}
				VK_ESCAPE => {
					group.picker = None;
					return actions;
				}
				_ => {}
			}
		}
		match vk {
			VK_RETURN => {
				drop(group);
				actions = self.send_input();
			}
			VK_BACK => {
				if group.cursor > 0 {
					group.cursor -= 1;
					let cursor = group.cursor;
					group.input.remove(cursor);
					group.mention_picker();
				}
			}
			VK_DELETE => {
				if group.cursor < group.input.len() {
					let cursor = group.cursor;
					group.input.remove(cursor);
					group.mention_picker();
				}
			}
			VK_LEFT => {
				if ctrl {
					group.cursor = 0;
				} else {
					group.cursor = group.cursor.saturating_sub(1);
				}
			}
			VK_RIGHT => {
				if ctrl {
					group.cursor = group.input.len();
				} else {
					group.cursor = (group.cursor + 1).min(group.input.len());
				}
			}
			VK_HOME => group.cursor = 0,
			VK_END => group.cursor = group.input.len(),
			VK_PRIOR => group.scroll += 8,
			VK_NEXT => group.scroll = group.scroll.saturating_sub(8),
			VK_UP => group.scroll += 1,
			VK_DOWN => group.scroll = group.scroll.saturating_sub(1),
			VK_TAB => {
				if !complete_mention(&mut group) {
					group.mention_picker();
				}
			}
			VK_ESCAPE => {
				group.input.clear();
				group.cursor = 0;
				group.picker = None;
			}
			VK_V if ctrl => {}
			_ => {}
		}
		actions
	}

	pub fn stop_all(&self) {
		self.running.store(false, Ordering::Relaxed);
		let sessions = self.sessions();
		if let Some(client) = self.client.clone() {
			thread::spawn(move || {
				for (_, session) in sessions {
					let _ = client.interrupt(&session);
				}
			});
		}
		if let Ok(mut group) = self.data.lock() {
			group.tasks.clear();
			for member in &mut group.members {
				member.busy = false;
			}
		}
	}

	fn add_member(&self, model: &str, role: Option<&str>) -> Result<String, String> {
		let client = self
			.client
			.as_ref()
			.ok_or_else(|| String::from("opencode server unavailable"))?;
		if !MODEL_CHOICES.iter().any(|(id, _)| *id == model) {
			return Err(format!("unknown model {model}"));
		}
		let (name, cwd) = {
			let group = self.data.lock().map_err(|_| String::from("group busy"))?;
			(group.next_name(), group.cwd.clone())
		};
		let session = client.create_session(&cwd, &format!("even/{name}"), model)?;
		let mut group = self.data.lock().map_err(|_| String::from("group busy"))?;
		let label = match role {
			Some(role) if !role.is_empty() => format!("{name} on {model} ({role})"),
			_ => format!("{name} on {model}"),
		};
		group.push(RecordKind::System, format!("added {label}"));
		group.members.push(Member {
			name: name.clone(),
			model: String::from(model),
			role: role.filter(|role| !role.is_empty()).map(String::from),
			session,
			seen: HashSet::new(),
			busy: false,
			error: None,
			last_cost: None,
		});
		group.selected = group.members.len() - 1;
		Ok(name)
	}

	fn send_input(&self) -> Vec<GroupAction> {
		let actions = Vec::new();
		let text = {
			let Ok(mut group) = self.data.lock() else {
				return actions;
			};
			let text: String = group.input.iter().collect();
			group.input.clear();
			group.cursor = 0;
			group.picker = None;
			text.trim().to_string()
		};
		if text.is_empty() {
			return actions;
		}
		if text.starts_with('/') {
			return self.command(&text);
		}
		let targets = {
			let group = match self.data.lock() {
				Ok(group) => group,
				Err(_) => return actions,
			};
			mentions(&text, &group.members)
		};
		if targets.is_empty() {
			if let Ok(mut group) = self.data.lock() {
				group.push(RecordKind::Human, text);
				group.scroll = 0;
			}
			return actions;
		}
		let client = match self.client.as_ref() {
			Some(client) => client.clone(),
			None => {
				if let Ok(mut group) = self.data.lock() {
					group.push(RecordKind::Error, String::from("opencode server unavailable"));
				}
				return actions;
			}
		};
		for name in targets {
			let (task_id, session) = {
				let Ok(mut group) = self.data.lock() else {
					continue;
				};
				let Some(index) = group
					.members
					.iter()
					.position(|member| member.name == name)
				else {
					continue;
				};
				let task_id = group.next_task;
				group.next_task += 1;
				let session = {
					let member = &mut group.members[index];
					member.busy = true;
					member.error = None;
					member.session.clone()
				};
				group.records.push(Record {
					kind: RecordKind::Human,
					member: Some(name.clone()),
					to: Some(name.clone()),
					task: Some(task_id),
					text: text.clone(),
					at: now(),
					cost: None,
					seconds: None,
				});
				group.tasks.push(Task {
					id: task_id,
					member: name.clone(),
					started: Instant::now(),
					text: text.clone(),
				});
				group.scroll = 0;
				(task_id, session)
			};
			if let Ok(mut events) = self.events.lock() {
				events.push(Event::AiTask {
					member: name.clone(),
					task: task_id,
					session: session.clone(),
					text: text.clone(),
				});
			}
			let prompt = text.clone();
			let thread_client = client.clone();
			let thread_data = self.data.clone();
			let thread_name = name.clone();
			thread::spawn(move || {
				if let Err(err) = thread_client.prompt(&session, &prompt) {
					if let Ok(mut group) = thread_data.lock() {
						group.tasks.retain(|task| task.id != task_id);
						if let Some(member) = group
							.members
							.iter_mut()
							.find(|member| member.name == thread_name)
						{
							member.busy = false;
						}
						group.push(
							RecordKind::Error,
							format!("{thread_name} task {task_id} failed: {err}"),
						);
					}
				}
			});
		}
		actions
	}

	fn command(&self, text: &str) -> Vec<GroupAction> {
		let mut actions = Vec::new();
		let mut parts = text.splitn(2, char::is_whitespace);
		let command = parts.next().unwrap_or("");
		let rest = parts.next().unwrap_or("").trim();
		match command {
			"/add" => {
				if rest.is_empty() {
					if let Ok(mut group) = self.data.lock() {
						group.picker = Some(Picker {
							kind: PickerKind::Model,
							items: MODEL_CHOICES
								.iter()
								.map(|(id, label)| PickerItem {
									value: String::from(*id),
									label: format!("{id}  {label}"),
								})
								.collect(),
							selected: 0,
						});
					}
				} else {
					let mut pieces = rest.splitn(2, char::is_whitespace);
					let model = pieces.next().unwrap_or("");
					let role = pieces.next().unwrap_or("").trim();
					if let Err(err) = self.add_member(model, if role.is_empty() { None } else { Some(role) }) {
						if let Ok(mut group) = self.data.lock() {
							group.push(RecordKind::Error, err);
						}
					}
				}
			}
			"/models" => {
				if let Ok(mut group) = self.data.lock() {
					let text = MODEL_CHOICES
						.iter()
						.map(|(id, label)| format!("{id}  {label}"))
						.collect::<Vec<_>>()
						.join("\n");
					group.push(RecordKind::System, text);
				}
			}
			"/members" => {
				if let Ok(mut group) = self.data.lock() {
					if group.members.is_empty() {
						group.push(RecordKind::System, String::from("no members yet; /add introduces one"));
					} else {
						let text = group
							.members
							.iter()
							.map(|member| match &member.role {
								Some(role) => format!("{}  {}  {role}", member.name, member.model),
								None => format!("{}  {}", member.name, member.model),
							})
							.collect::<Vec<_>>()
							.join("\n");
						group.push(RecordKind::System, text);
					}
				}
			}
			"/tasks" => {
				if let Ok(mut group) = self.data.lock() {
					if group.tasks.is_empty() {
						group.push(RecordKind::System, String::from("no running tasks"));
					} else {
						let text = group
							.tasks
							.iter()
							.map(|task| {
								format!(
									"task {} {} {:.1}s  {}",
									task.id,
									task.member,
									task.started.elapsed().as_secs_f64(),
									task.text
								)
							})
							.collect::<Vec<_>>()
							.join("\n");
						group.push(RecordKind::System, text);
					}
				}
			}
			"/stop" => {
				let stopped = self.stop_target(rest);
				if let Ok(mut group) = self.data.lock() {
					if stopped.is_empty() {
						group.push(RecordKind::Error, String::from("no such task or member"));
					} else {
						group.push(
							RecordKind::Stopped,
							format!("stopped {}; window stays", stopped.join(", ")),
						);
					}
				}
			}
			"/remove" => {
				let name = rest;
				let removed = {
					let Ok(mut group) = self.data.lock() else {
						return actions;
					};
					match group.members.iter().position(|member| member.name == name) {
						Some(index) => {
							let member = group.members.remove(index);
							group.tasks.retain(|task| task.member != member.name);
							group.push(
								RecordKind::System,
								format!("removed {}; its task stopped", member.name),
							);
							group.selected = group.selected.min(group.members.len().saturating_sub(1));
							if let Ok(mut events) = self.events.lock() {
								events.push(Event::MemberRemoved {
									member: member.name.clone(),
									session: member.session.clone(),
								});
							}
							Some(member)
						}
						None => {
							group.push(RecordKind::Error, format!("no member {name}"));
							None
						}
					}
				};
				if let Some(member) = removed {
					actions.push(GroupAction::Close {
						session: member.session.clone(),
					});
					if let Some(client) = self.client.clone() {
						let session = member.session.clone();
						thread::spawn(move || {
							let _ = client.interrupt(&session);
						});
					}
				}
			}
			"/open" => {
				if let Some(action) = self.open_by_name(rest) {
					actions.push(action);
				} else if let Ok(mut group) = self.data.lock() {
					group.push(RecordKind::Error, format!("no member {rest}"));
				}
			}
			"/clear" => {
				if let Ok(mut group) = self.data.lock() {
					group.records.clear();
					group.scroll = 0;
				}
			}
			"/help" => {
				if let Ok(mut group) = self.data.lock() {
					group.push(
						RecordKind::System,
						String::from(
							"@name task   /add [model [role]]   /models   /members   /tasks   /stop name|id   /remove name   /open name   /clear",
						),
					);
				}
			}
			_ => {
				if let Ok(mut group) = self.data.lock() {
					group.push(RecordKind::Error, format!("unknown command {command}"));
				}
			}
		}
		actions
	}

	fn stop_target(&self, target: &str) -> Vec<String> {
		let (labels, sessions) = {
			let Ok(mut group) = self.data.lock() else {
				return Vec::new();
			};
			let by_id = target.parse::<u64>().ok();
			let mut labels: Vec<String> = Vec::new();
			let mut names: HashSet<String> = HashSet::new();
			group.tasks.retain(|task| {
				let hit = by_id.map(|id| id == task.id).unwrap_or(task.member == target);
				if hit {
					labels.push(format!("task {}", task.id));
					names.insert(task.member.clone());
					false
				} else {
					true
				}
			});
			if by_id.is_none() {
				if group.members.iter().any(|member| member.name == target) {
					names.insert(String::from(target));
					if labels.is_empty() {
						labels.push(String::from(target));
					}
				}
			}
			for name in &names {
				if let Some(member) = group.members.iter_mut().find(|member| member.name == *name) {
					member.busy = false;
				}
			}
			let sessions: Vec<String> = names
				.iter()
				.filter_map(|name| {
					group
						.members
						.iter()
						.find(|member| member.name == *name)
						.map(|member| member.session.clone())
				})
				.collect();
			if let Ok(mut events) = self.events.lock() {
				for name in &names {
					if let Some(member) = group.members.iter().find(|member| member.name == *name) {
						events.push(Event::AiStopped {
							member: member.name.clone(),
							session: member.session.clone(),
						});
					}
				}
			}
			(labels, sessions)
		};
		if let Some(client) = self.client.clone() {
			thread::spawn(move || {
				for session in sessions {
					let _ = client.interrupt(&session);
				}
			});
		}
		labels
	}

	fn open_by_name(&self, name: &str) -> Option<GroupAction> {
		let group = self.data.lock().ok()?;
		let member = group.members.iter().find(|member| member.name == name)?;
		Some(GroupAction::Open {
			name: member.name.clone(),
			session: member.session.clone(),
		})
	}

	pub fn view(&self, cols: usize, rows: usize) -> GroupView {
		let group = match self.data.lock() {
			Ok(group) => group,
			Err(_) => return GroupView::empty(),
		};
		let left = 24usize.min(cols / 3).max(12);
		let width = cols.saturating_sub(left + 3).max(8);
		let stream_rows = rows.saturating_sub(5).max(1);
		let mut lines: Vec<ViewLine> = Vec::new();
		for record in &group.records {
			match record.kind {
				RecordKind::Human => {
					let task = record
						.task
						.map(|id| format!(" task {id}"))
						.unwrap_or_default();
					let to = record.to.clone().unwrap_or_default();
					lines.push(ViewLine {
						text: format!("{} you -> {to}{task} (authoritative)", clock(record.at)),
						kind: RecordKind::Human,
					});
					for line in wrap(&record.text, width.saturating_sub(2)) {
						lines.push(ViewLine {
							text: format!("  {line}"),
							kind: RecordKind::Human,
						});
					}
				}
				RecordKind::Reply => {
					let task = record
						.task
						.map(|id| format!(" task {id}"))
						.unwrap_or_default();
					let mut meta = String::new();
					if let Some(seconds) = record.seconds {
						meta.push_str(&format!(" {seconds:.1}s"));
					}
					if let Some(cost) = record.cost {
						meta.push_str(&format!(" ${cost:.6}"));
					}
					lines.push(ViewLine {
						text: format!(
							"{} {}{task}{meta}",
							clock(record.at),
							record.member.clone().unwrap_or_default()
						),
						kind: RecordKind::Reply,
					});
					for line in wrap(&record.text, width.saturating_sub(2)) {
						lines.push(ViewLine {
							text: format!("  {line}"),
							kind: RecordKind::Reply,
						});
					}
				}
				RecordKind::Suggestion => {
					let text = format!(
						"{} -> {} (suggestion): {}",
						record.member.clone().unwrap_or_default(),
						record.to.clone().unwrap_or_default(),
						record.text
					);
					for line in wrap(&text, width) {
						lines.push(ViewLine {
							text: line,
							kind: RecordKind::Suggestion,
						});
					}
				}
				RecordKind::System => {
					for line in wrap(&record.text, width.saturating_sub(2)) {
						lines.push(ViewLine {
							text: format!("* {line}"),
							kind: RecordKind::System,
						});
					}
				}
				RecordKind::Error => {
					for line in wrap(&record.text, width.saturating_sub(2)) {
						lines.push(ViewLine {
							text: format!("! {line}"),
							kind: RecordKind::Error,
						});
					}
				}
				RecordKind::Stopped => {
					for line in wrap(&record.text, width.saturating_sub(2)) {
						lines.push(ViewLine {
							text: format!("* {line}"),
							kind: RecordKind::Stopped,
						});
					}
				}
			}
			lines.push(ViewLine {
				text: String::new(),
				kind: RecordKind::System,
			});
		}
		let total = lines.len();
		let start = total.saturating_sub(stream_rows + group.scroll);
		let visible: Vec<ViewLine> = lines
			.into_iter()
			.skip(start)
			.take(stream_rows)
			.collect();
		let running = group.tasks.len();
		let title = format!(
			"even   {} members   {running} running   ${:.6}",
			group.members.len(),
			group.total_cost
		);
		let members = group
			.members
			.iter()
			.enumerate()
			.map(|(index, member)| {
				let marker = if member.error.is_some() {
					"!"
				} else if member.busy {
					"*"
				} else {
					" "
				};
				let role = member
					.role
					.as_ref()
					.map(|role| format!(" {role}"))
					.unwrap_or_default();
				MemberView {
					text: format!("{marker} {}  {}{role}", member.name, short_model(&member.model)),
					selected: index == group.selected,
					busy: member.busy,
				}
			})
			.collect();
		let hint = if group.picker.is_some() {
			String::from("up/down choose, Enter confirm, Esc cancel")
		} else if running > 0 {
			let names = group
				.tasks
				.iter()
				.map(|task| task.member.clone())
				.collect::<Vec<_>>()
				.join(", ");
			format!("running: {names}   /stop name   /tasks")
		} else {
			String::from("type @name task   /add member   /stop   /remove   /open")
		};
		let picker = group.picker.as_ref().map(|picker| PickerView {
			items: picker
				.items
				.iter()
				.enumerate()
				.map(|(index, item)| PickerLine {
					text: item.label.clone(),
					selected: index == picker.selected,
				})
				.collect(),
		});
		GroupView {
			title,
			left,
			members,
			lines: visible,
			start,
			total,
			input: group.input.iter().collect(),
			cursor: group.cursor,
			hint,
			picker,
			total_cost: group.total_cost,
		}
	}
}

impl Drop for GroupPane {
	fn drop(&mut self) {
		self.running.store(false, Ordering::Relaxed);
		let sessions = self.sessions();
		if let Some(client) = self.client.clone() {
			thread::spawn(move || {
				for (_, session) in sessions {
					let _ = client.interrupt(&session);
				}
			});
		}
	}
}

fn poll_once(data: &Arc<Mutex<Group>>, client: &Client) {
	let members: Vec<(String, String)> = match data.lock() {
		Ok(group) => group
			.members
			.iter()
			.map(|member| (member.name.clone(), member.session.clone()))
			.collect(),
		Err(_) => return,
	};
	for (name, session) in members {
		let messages = match client.messages(&session) {
			Ok(messages) => messages,
			Err(err) => {
				if let Ok(mut group) = data.lock() {
					if let Some(member) = group.members.iter_mut().find(|member| member.name == name) {
						if member.error.as_deref() != Some(err.as_str()) {
							member.error = Some(err);
						}
					}
				}
				continue;
			}
		};
		let mut replies = Vec::new();
		{
			let group = match data.lock() {
				Ok(group) => group,
				Err(_) => return,
			};
			let member = match group.members.iter().find(|member| member.name == name) {
				Some(member) => member,
				None => continue,
			};
			for message in &messages {
				let id = message["id"].as_str().unwrap_or("").to_string();
				if id.is_empty() || member.seen.contains(&id) {
					continue;
				}
				if let Some(reply) = assistant_reply(message) {
					replies.push((id, reply));
				}
			}
		}
		if replies.is_empty() {
			continue;
		}
		let Ok(mut group) = data.lock() else {
			return;
		};
		let names: Vec<String> = group.members.iter().map(|member| member.name.clone()).collect();
		for (id, reply) in replies {
			let task = group
				.tasks
				.iter()
				.find(|task| task.member == name)
				.cloned();
			let member = match group.members.iter_mut().find(|member| member.name == name) {
				Some(member) => member,
				None => break,
			};
			if member.seen.contains(&id) {
				continue;
			}
			member.seen.insert(id);
			member.busy = false;
			member.error = None;
			match reply {
				Ok(reply) => {
					member.last_cost = reply.cost;
					if let Some(cost) = reply.cost {
						group.total_cost += cost;
					}
					let seconds = task.as_ref().map(|task| task.started.elapsed().as_secs_f64());
					group.tasks.retain(|task| task.member != name);
					group.records.push(Record {
						kind: RecordKind::Reply,
						member: Some(name.clone()),
						to: None,
						task: task.as_ref().map(|task| task.id),
						text: reply.text.clone(),
						at: now(),
						cost: reply.cost,
						seconds,
					});
					for (to, line) in suggestions(&name, &reply.text, &names) {
						group.records.push(Record {
							kind: RecordKind::Suggestion,
							member: Some(name.clone()),
							to: Some(to),
							task: task.as_ref().map(|task| task.id),
							text: line,
							at: now(),
							cost: reply.cost,
							seconds: None,
						});
					}
				}
				Err(error) => {
					group.tasks.retain(|task| task.member != name);
					group.records.push(Record {
						kind: RecordKind::Error,
						member: Some(name.clone()),
						to: None,
						task: task.as_ref().map(|task| task.id),
						text: format!("{name}: {error}"),
						at: now(),
						cost: None,
						seconds: None,
					});
				}
			}
		}
		group.scroll = group.scroll.min(group.records.len());
	}
}

struct Reply {
	text: String,
	cost: Option<f64>,
}

fn assistant_reply(message: &Value) -> Option<Result<Reply, String>> {
	if message["type"].as_str() != Some("assistant") {
		return None;
	}
	let finished =
		message.get("finish").is_some() || message["time"]["completed"].as_u64().is_some();
	if !finished {
		return None;
	}
	if let Some(error) = message.get("error") {
		if !error.is_null() {
			let text = error["message"]
				.as_str()
				.or_else(|| error["type"].as_str())
				.unwrap_or("unknown error");
			return Some(Err(text.to_string()));
		}
	}
	let mut out = String::new();
	if let Some(content) = message["content"].as_array() {
		for part in content {
			if part["type"].as_str() == Some("text") {
				if let Some(text) = part["text"].as_str() {
					out.push_str(text);
					if !text.ends_with('\n') {
						out.push('\n');
					}
				}
			}
		}
	}
	if out.trim().is_empty() {
		out.push_str("(no text)");
	}
	Some(Ok(Reply {
		text: out.trim_end().to_string(),
		cost: message["cost"].as_f64(),
	}))
}

fn suggestions(from: &str, text: &str, names: &[String]) -> Vec<(String, String)> {
	let mut out = Vec::new();
	for line in text.lines() {
		for token in line.split_whitespace() {
			let Some(raw) = token.strip_prefix('@') else {
				continue;
			};
			let name = raw.trim_end_matches(|ch: char| !ch.is_alphanumeric() && ch != '_');
			if name.is_empty() || name == from || !names.iter().any(|known| known == name) {
				continue;
			}
			out.push((String::from(name), line.trim().to_string()));
		}
	}
	out
}

fn mentions(text: &str, members: &[Member]) -> Vec<String> {
	let mut out = Vec::new();
	for token in text.split_whitespace() {
		let Some(raw) = token.strip_prefix('@') else {
			continue;
		};
		let name = raw.trim_end_matches(|ch: char| !ch.is_alphanumeric() && ch != '_');
		if members.iter().any(|member| member.name == name) && !out.iter().any(|seen| seen == name) {
			out.push(String::from(name));
		}
	}
	out
}

fn complete_mention(group: &mut Group) -> bool {
	let mut start = group.cursor;
	while start > 0 && group.input[start - 1] != ' ' {
		start -= 1;
	}
	let word: String = group.input[start..group.cursor].iter().collect();
	let Some(prefix) = word.strip_prefix('@') else {
		return false;
	};
	if let Some(member) = group
		.members
		.iter()
		.find(|member| member.name.starts_with(prefix))
	{
		group.input.drain(start..);
		for ch in format!("@{} ", member.name).chars() {
			group.input.push(ch);
		}
		group.cursor = group.input.len();
		return true;
	}
	false
}

fn insert_at_cursor(group: &mut Group, text: &str) {
	for ch in text.chars() {
		group.input.insert(group.cursor, ch);
		group.cursor += 1;
	}
	if group.cursor == group.input.len() {
		group.input.push(' ');
		group.cursor += 1;
	}
}

impl Group {
	fn mention_picker(&mut self) {
		let mut start = self.cursor;
		while start > 0 && self.input[start - 1] != ' ' {
			start -= 1;
		}
		let word: String = self.input[start..self.cursor].iter().collect();
		let Some(prefix) = word.strip_prefix('@') else {
			if matches!(self.picker.as_ref().map(|picker| picker.kind), Some(PickerKind::Mention)) {
				self.picker = None;
			}
			return;
		};
		let items: Vec<PickerItem> = self
			.members
			.iter()
			.filter(|member| member.name.starts_with(prefix))
			.map(|member| PickerItem {
				value: format!("@{}", member.name),
				label: format!("@{}  {}", member.name, short_model(&member.model)),
			})
			.collect();
		if items.is_empty() {
			self.picker = None;
		} else {
			self.picker = Some(Picker {
				kind: PickerKind::Mention,
				items,
				selected: 0,
			});
		}
	}
}

fn short_model(model: &str) -> &str {
	match model {
		"deepseek-flash" => "flash",
		"deepseek-v4-pro" => "v4-pro",
		other => other,
	}
}

fn now() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| duration.as_secs())
		.unwrap_or(0)
}

fn clock(at: u64) -> String {
	let seconds = at % 86_400;
	format!(
		"{:02}:{:02}:{:02}",
		seconds / 3600,
		(seconds % 3600) / 60,
		seconds % 60
	)
}

fn wrap(text: &str, width: usize) -> Vec<String> {
	let width = width.max(1);
	let mut out = Vec::new();
	for raw in text.split('\n') {
		if raw.is_empty() {
			out.push(String::new());
			continue;
		}
		let mut line = String::new();
		let mut used = 0;
		for ch in raw.chars() {
			let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
			if used + ch_width > width && used > 0 {
				out.push(std::mem::take(&mut line));
				used = 0;
			}
			line.push(ch);
			used += ch_width;
		}
		if !line.is_empty() {
			out.push(line);
		}
	}
	if out.is_empty() {
		out.push(String::new());
	}
	out
}

pub struct GroupView {
	pub title: String,
	pub left: usize,
	pub members: Vec<MemberView>,
	pub lines: Vec<ViewLine>,
	pub start: usize,
	pub total: usize,
	pub input: String,
	pub cursor: usize,
	pub hint: String,
	pub picker: Option<PickerView>,
	pub total_cost: f64,
}

impl GroupView {
	fn empty() -> Self {
		Self {
			title: String::from("even"),
			left: 24,
			members: Vec::new(),
			lines: Vec::new(),
			start: 0,
			total: 0,
			input: String::new(),
			cursor: 0,
			hint: String::new(),
			picker: None,
			total_cost: 0.0,
		}
	}
}

pub struct MemberView {
	pub text: String,
	pub selected: bool,
	pub busy: bool,
}

pub struct ViewLine {
	pub text: String,
	pub kind: RecordKind,
}

pub struct PickerView {
	pub items: Vec<PickerLine>,
}

pub struct PickerLine {
	pub text: String,
	pub selected: bool,
}

#[cfg(test)]
mod tests {
	use super::*;

	fn members() -> Vec<Member> {
		vec![
			Member {
				name: String::from("sam"),
				model: String::from("deepseek-flash"),
				role: None,
				session: String::from("a"),
				seen: HashSet::new(),
				busy: false,
				error: None,
				last_cost: None,
			},
			Member {
				name: String::from("apple"),
				model: String::from("deepseek-flash"),
				role: Some(String::from("reviewer")),
				session: String::from("b"),
				seen: HashSet::new(),
				busy: false,
				error: None,
				last_cost: None,
			},
		]
	}

	#[test]
	fn routes_only_known_members() {
		let list = members();
		assert_eq!(mentions("@sam review @nobody @apple", &list), vec!["sam", "apple"]);
	}

	#[test]
	fn ai_mentions_are_suggestions_not_commands() {
		let list = members();
		let found = suggestions("sam", "ask @apple to look", &list.iter().map(|m| m.name.clone()).collect::<Vec<_>>());
		assert_eq!(found.len(), 1);
		assert_eq!(found[0].0, "apple");
	}

	#[test]
	fn wraps_by_display_width() {
		let lines = wrap("abcdef", 3);
		assert_eq!(lines, vec!["abc", "def"]);
	}
}
