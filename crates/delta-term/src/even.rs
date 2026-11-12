//! even: group chat. One group is one workspace; members are AI sessions on a
//! shared opencode server. `@` hands a pane's task to a member; AI mentions of
//! other members are recorded as suggestions, never executed.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use unicode_width::UnicodeWidthChar;

use crate::canvas::{self, Canvas};
use crate::layout::Rect;
use crate::opencode::{Client, DEFAULT_MODEL, MODEL_CHOICES};

const CODENAMES: [&str; 12] =
	["sam", "apple", "pine", "lake", "moss", "river", "stone", "cloud", "ember", "frost", "reef", "dune"];

#[derive(Clone, Debug)]
pub struct Member {
	pub name: String,
	pub model: String,
	pub role: Option<String>,
	pub session: String,
	pub busy: bool,
	pub error: Option<String>,
	pub last_cost: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
	Human,
	Reply,
	Suggestion,
	System,
	Error,
}

#[derive(Clone, Debug)]
pub struct Record {
	pub kind: RecordKind,
	pub member: Option<String>,
	pub to: Option<String>,
	pub text: String,
	pub cost: Option<f64>,
	pub seconds: Option<f64>,
}

#[derive(Clone, Debug)]
pub enum EvenEvent {
	OpenPage { name: String, session: String },
	ClosePage { name: String },
}

struct Task {
	member: String,
	started: Instant,
}

impl Clone for Task {
	fn clone(&self) -> Self {
		Self { member: self.member.clone(), started: self.started }
	}
}

struct Group {
	members: Vec<Member>,
	records: Vec<Record>,
	tasks: Vec<Task>,
	input: Vec<char>,
	cursor: usize,
	scroll: usize,
	seen: HashSet<String>,
	total_cost: f64,
}

impl Group {
	fn new() -> Self {
		Self {
			members: Vec::new(),
			records: Vec::new(),
			tasks: Vec::new(),
			input: Vec::new(),
			cursor: 0,
			scroll: 0,
			seen: HashSet::new(),
			total_cost: 0.0,
		}
	}

	fn push(&mut self, kind: RecordKind, member: Option<&str>, to: Option<&str>, text: String) {
		self.records.push(Record {
			kind,
			member: member.map(String::from),
			to: to.map(String::from),
			text,
			cost: None,
			seconds: None,
		});
		self.scroll = 0;
	}

	fn next_name(&self) -> String {
		for name in CODENAMES {
			if !self.members.iter().any(|m| m.name == name) {
				return String::from(name);
			}
		}
		format!("member{}", self.members.len() + 1)
	}
}

pub struct GroupPane {
	pub name: String,
	pub root: String,
	inner: Arc<Mutex<Group>>,
	client: Option<Client>,
	running: Arc<AtomicBool>,
	events: Arc<Mutex<Vec<EvenEvent>>>,
	dirty: Arc<AtomicBool>,
}

impl GroupPane {
	pub fn new(name: &str, root: &str, client: Option<Client>) -> Self {
		let inner = Arc::new(Mutex::new(Group::new()));
		let running = Arc::new(AtomicBool::new(true));
		let events = Arc::new(Mutex::new(Vec::new()));
		let dirty = Arc::new(AtomicBool::new(true));
		if let Some(client) = client.clone() {
			let data = inner.clone();
			let run = running.clone();
			let dirty2 = dirty.clone();
			thread::spawn(move || {
				while run.load(Ordering::Relaxed) {
					poll_once(&data, &client, &dirty2);
					thread::sleep(Duration::from_millis(700));
				}
			});
			if let Ok(mut group) = inner.lock() {
				group.push(
					RecordKind::System,
					None,
					None,
					String::from(":add 添加成员；@代号 交代任务；:help 看命令。"),
				);
			}
		} else if let Ok(mut group) = inner.lock() {
			group.push(
				RecordKind::Error,
				None,
				None,
				String::from("opencode server 没有起来；even 只能当记事本用。检查 D:\\OPENCODE\\opencode.exe。"),
			);
		}
		Self {
			name: String::from(name),
			root: String::from(root),
			inner,
			client,
			running,
			events,
			dirty,
		}
	}

	pub fn take_events(&self) -> Vec<EvenEvent> {
		match self.events.lock() {
			Ok(mut list) => std::mem::take(&mut *list),
			Err(_) => Vec::new(),
		}
	}

	pub fn member_sessions(&self) -> Vec<(String, String)> {
		match self.inner.lock() {
			Ok(group) => group.members.iter().map(|m| (m.name.clone(), m.session.clone())).collect(),
			Err(_) => Vec::new(),
		}
	}

	pub fn take_dirty(&self) -> bool {
		self.dirty.swap(false, Ordering::Relaxed)
	}

	pub fn busy_members(&self) -> Vec<String> {
		match self.inner.lock() {
			Ok(group) => group.members.iter().filter(|m| m.busy).map(|m| m.name.clone()).collect(),
			Err(_) => Vec::new(),
		}
	}

	pub fn member_names(&self) -> Vec<String> {
		match self.inner.lock() {
			Ok(group) => group.members.iter().map(|m| m.name.clone()).collect(),
			Err(_) => Vec::new(),
		}
	}

	pub fn transcript(&self) -> String {
		let Ok(group) = self.inner.lock() else { return String::new() };
		let mut out = format!("even {} ({})\n", self.name, self.root);
		for member in &group.members {
			out.push_str(&format!(
				"@{} {} {}{}\n",
				member.name,
				member.model,
				if member.busy { "busy" } else { "idle" },
				member.error.as_deref().map(|e| format!(" error={e}")).unwrap_or_default()
			));
		}
		for record in &group.records {
			out.push_str(&format!(
				"{}{}: {}\n",
				match record.kind {
					RecordKind::Human => "you",
					RecordKind::Reply => record.member.as_deref().unwrap_or("ai"),
					RecordKind::Suggestion => "suggestion",
					RecordKind::System => "·",
					RecordKind::Error => "!",
				},
				record.to.as_deref().map(|t| format!(" →{t}")).unwrap_or_default(),
				record.text
			));
		}
		out
	}

	pub fn insert_char(&self, ch: char) {
		if let Ok(mut group) = self.inner.lock() {
			let cursor = group.cursor;
			group.input.insert(cursor, ch);
			group.cursor = cursor + 1;
		}
		self.dirty.store(true, Ordering::Relaxed);
	}

	pub fn insert_text(&self, text: &str) {
		if let Ok(mut group) = self.inner.lock() {
			for ch in text.chars() {
				if ch == '\r' {
					continue;
				}
				let ch = if ch == '\n' { ' ' } else { ch };
				let cursor = group.cursor;
				group.input.insert(cursor, ch);
				group.cursor = cursor + 1;
			}
		}
		self.dirty.store(true, Ordering::Relaxed);
	}

	pub fn backspace(&self) {
		if let Ok(mut group) = self.inner.lock() {
			if group.cursor > 0 {
				let cursor = group.cursor - 1;
				group.cursor = cursor;
				group.input.remove(cursor);
			}
		}
		self.dirty.store(true, Ordering::Relaxed);
	}

	pub fn delete(&self) {
		if let Ok(mut group) = self.inner.lock() {
			let cursor = group.cursor;
			if cursor < group.input.len() {
				group.input.remove(cursor);
			}
		}
		self.dirty.store(true, Ordering::Relaxed);
	}

	pub fn move_input(&self, delta: i32) {
		if let Ok(mut group) = self.inner.lock() {
			if delta < 0 {
				group.cursor = group.cursor.saturating_sub((-delta) as usize);
			} else {
				group.cursor = (group.cursor + delta as usize).min(group.input.len());
			}
		}
	}

	pub fn input_home(&self) {
		if let Ok(mut group) = self.inner.lock() {
			group.cursor = 0;
		}
	}

	pub fn input_end(&self) {
		if let Ok(mut group) = self.inner.lock() {
			group.cursor = group.input.len();
		}
	}

	pub fn scroll(&self, delta: i32) {
		if let Ok(mut group) = self.inner.lock() {
			if delta > 0 {
				group.scroll = (group.scroll + delta as usize).min(group.records.len());
			} else {
				group.scroll = group.scroll.saturating_sub((-delta) as usize);
			}
		}
	}

	pub fn enter(&self) {
		let line = match self.inner.lock() {
			Ok(mut group) => {
				let text: String = group.input.iter().collect();
				group.input.clear();
				group.cursor = 0;
				text
			}
			Err(_) => return,
		};
		let line = line.trim().to_string();
		if line.is_empty() {
			return;
		}
		self.dirty.store(true, Ordering::Relaxed);
		if line.starts_with(':') {
			self.command(&line);
			return;
		}
		let (names, members): (Vec<String>, Vec<String>) = match self.inner.lock() {
			Ok(group) => (
				group.members.iter().map(|m| m.name.clone()).collect(),
				group.members.iter().map(|m| m.name.clone()).collect(),
			),
			Err(_) => return,
		};
		let targets = mentions(&line);
		if targets.is_empty() {
			if let Ok(mut group) = self.inner.lock() {
				group.push(
					RecordKind::System,
					None,
					None,
					format!("没有 @ 到人。成员：{}", names.join(", ")),
				);
			}
			return;
		}
		let mut unknown = Vec::new();
		for target in &targets {
			if !members.contains(target) {
				unknown.push(target.clone());
			}
		}
		if !unknown.is_empty() {
			if let Ok(mut group) = self.inner.lock() {
				group.push(
					RecordKind::Error,
					None,
					None,
					format!("没有这个成员：{}", unknown.join(", ")),
				);
			}
			return;
		}
		let Some(client) = self.client.clone() else {
			return;
		};
		for target in &targets {
			let session = {
				let Ok(mut group) = self.inner.lock() else { return };
				let Some(index) = group.members.iter().position(|m| m.name == *target) else { continue };
				group.tasks.retain(|t| t.member != *target);
				group.tasks.push(Task { member: target.clone(), started: Instant::now() });
				let member = &mut group.members[index];
				member.busy = true;
				member.error = None;
				member.session.clone()
			};
			if let Ok(mut group) = self.inner.lock() {
				group.push(RecordKind::Human, Some("you"), Some(target), line.clone());
			}
			let inner = self.inner.clone();
			let dirty = self.dirty.clone();
			let target = target.clone();
			let client = client.clone();
			let line = line.clone();
			thread::spawn(move || {
				let result = client.prompt(&session, &line);
				if let Err(err) = result {
					if let Ok(mut group) = inner.lock() {
						group.tasks.retain(|t| t.member != target);
						if let Some(member) = group.members.iter_mut().find(|m| m.name == target) {
							member.busy = false;
							member.error = Some(err.clone());
						}
						group.push(RecordKind::Error, Some(&target), None, err);
					}
					dirty.store(true, Ordering::Relaxed);
				}
			});
		}
	}

	fn command(&self, line: &str) {
		let mut parts = line[1..].splitn(2, char::is_whitespace);
		let cmd = parts.next().unwrap_or("");
		let rest = parts.next().unwrap_or("").trim();
		match cmd {
			"add" => {
				let mut pieces = rest.split_whitespace();
				let first = pieces.next().filter(|piece| !piece.is_empty());
				let model = match first {
					None => DEFAULT_MODEL.to_string(),
					Some(token) => match MODEL_CHOICES.iter().find(|(id, label)| *id == token || *label == token) {
						Some((id, _)) => id.to_string(),
						None => {
							let choices: Vec<&str> = MODEL_CHOICES.iter().map(|(id, _)| *id).collect();
							if let Ok(mut group) = self.inner.lock() {
								group.push(
									RecordKind::Error,
									None,
									None,
									format!(
										"没有这个模型：{token}。模型只能是 {}；成员代号自动生成，不用起名。",
										choices.join(" / ")
									),
								);
							}
							return;
						}
					},
				};
				let role = pieces.next().map(String::from);
				self.add_member(&model, role.as_deref());
			}
			"remove" => self.remove_member(rest),
			"members" => {
				if let Ok(group) = self.inner.lock() {
					let names: Vec<String> = group
						.members
						.iter()
						.map(|m| match &m.role {
							Some(role) => format!("@{} {} ({role})", m.name, m.model),
							None => format!("@{} {}", m.name, m.model),
						})
						.collect();
					drop(group);
					if let Ok(mut group) = self.inner.lock() {
						group.push(
							RecordKind::System,
							None,
							None,
							if names.is_empty() {
								String::from("还没有成员。:add 添加。")
							} else {
								names.join("  ")
							},
						);
					}
				}
			}
			"open" => {
				if rest.is_empty() {
					if let Ok(mut group) = self.inner.lock() {
						group.push(RecordKind::System, None, None, String::from(":open <代号>"));
					}
					return;
				}
				if let Ok(group) = self.inner.lock() {
					if let Some(member) = group.members.iter().find(|m| m.name == rest) {
						if let Ok(mut events) = self.events.lock() {
							events.push(EvenEvent::OpenPage {
								name: member.name.clone(),
								session: member.session.clone(),
							});
						}
					}
				}
			}
			"stop" => self.stop(if rest.is_empty() { None } else { Some(rest) }),
			"clear" => {
				if let Ok(mut group) = self.inner.lock() {
					group.records.clear();
					group.scroll = 0;
				}
			}
			"help" => {
				if let Ok(mut group) = self.inner.lock() {
					group.push(
						RecordKind::System,
						None,
						None,
						String::from(
							":add [模型] [角色]（模型：deepseek-flash / deepseek-v4-pro；代号自动生成）  :remove 代号  :members  :open 代号  :stop [代号]  :clear  :help",
						),
					);
				}
			}
			"" => {}
			other => {
				if let Ok(mut group) = self.inner.lock() {
					group.push(RecordKind::Error, None, None, format!("未知命令：:{other}"));
				}
			}
		}
	}

	fn add_member(&self, model: &str, role: Option<&str>) {
		let Some(client) = self.client.clone() else { return };
		let model = if MODEL_CHOICES.iter().any(|(id, _)| *id == model) {
			model.to_string()
		} else if let Some((id, _)) = MODEL_CHOICES.iter().find(|(_, label)| *label == model) {
			id.to_string()
		} else {
			model.to_string()
		};
		let root = self.root.clone();
		let inner = self.inner.clone();
		let dirty = self.dirty.clone();
		let role = role.map(String::from);
		thread::spawn(move || {
			let name = match inner.lock() {
				Ok(group) => group.next_name(),
				Err(_) => return,
			};
			let title = format!("even-{name}");
			match client.create_session(&root, &title, &model) {
				Ok(session) => {
					if let Ok(mut group) = inner.lock() {
						group.members.push(Member {
							name: name.clone(),
							model: model.clone(),
							role: role.clone(),
							session,
							busy: false,
							error: None,
							last_cost: None,
						});
						group.push(
							RecordKind::System,
							None,
							None,
							format!("加入 @{name}（{model}）。"),
						);
					}
					dirty.store(true, Ordering::Relaxed);
				}
				Err(err) => {
					if let Ok(mut group) = inner.lock() {
						group.push(RecordKind::Error, None, None, format!("建会话失败：{err}"));
					}
					dirty.store(true, Ordering::Relaxed);
				}
			}
		});
	}

	fn remove_member(&self, name: &str) {
		let name = name.trim();
		if name.is_empty() {
			return;
		}
		let removed = match self.inner.lock() {
			Ok(mut group) => {
				let index = group.members.iter().position(|m| m.name == name);
				index.map(|i| group.members.remove(i))
			}
			Err(_) => None,
		};
		if let Some(member) = removed {
			if let Some(client) = self.client.clone() {
				let session = member.session.clone();
				thread::spawn(move || {
					let _ = client.interrupt(&session);
				});
			}
			if let Ok(mut events) = self.events.lock() {
				events.push(EvenEvent::ClosePage { name: String::from(name) });
			}
			if let Ok(mut group) = self.inner.lock() {
				group.tasks.retain(|t| t.member != name);
				group.push(RecordKind::System, None, None, format!("移除 @{name}，它的任务一起没了。"));
			}
		}
	}

	pub fn stop(&self, name: Option<&str>) {
		let targets: Vec<(String, String)> = match self.inner.lock() {
			Ok(mut group) => {
				let targets: Vec<(String, String)> = group
					.members
					.iter()
					.filter(|m| name.map(|n| m.name == n).unwrap_or(true))
					.map(|m| (m.name.clone(), m.session.clone()))
					.collect();
				group.tasks.retain(|t| !targets.iter().any(|(n, _)| *n == t.member));
				for member in &mut group.members {
					if targets.iter().any(|(n, _)| *n == member.name) {
						member.busy = false;
					}
				}
				targets
			}
			Err(_) => Vec::new(),
		};
		if let Some(client) = self.client.clone() {
			thread::spawn(move || {
				for (_, session) in targets {
					let _ = client.interrupt(&session);
				}
			});
		}
	}

	pub fn render(&self, canvas: &Canvas, area: Rect, focused: bool) {
		let cw = canvas.cell_w;
		let ch = canvas.cell_h;
		let cols = (area.width() / cw).max(1) as usize;
		let rows = (area.height() / ch).max(1) as usize;
		let bg = canvas::rgb(canvas::DEFAULT_BG.0, canvas::DEFAULT_BG.1, canvas::DEFAULT_BG.2);
		canvas.fill(area.left, area.top, area.left + cols as i32 * cw, area.top + rows as i32 * ch, bg);
		let saved = canvas.clip((area.left, area.top, area.left + cols as i32 * cw, area.top + rows as i32 * ch));
		let Ok(group) = self.inner.lock() else {
			canvas.unclip(saved);
			return;
		};
		let dim = canvas::rgb(130, 136, 150);
		let accent = canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2);
		let green = canvas::rgb(150, 205, 160);
		let red = canvas::rgb(230, 120, 120);
		let plain = canvas::rgb(canvas::DEFAULT_FG.0, canvas::DEFAULT_FG.1, canvas::DEFAULT_FG.2);
		let y0 = area.top;
		canvas.text(area.left, y0, &format!("even {}  {}", self.name, self.root), accent);
		let members_line = if group.members.is_empty() {
			String::from("(no members) :add [model] [role]")
		} else {
			group
				.members
				.iter()
				.map(|m| {
					let mark = if m.busy { "*" } else { "" };
					format!("@{}{mark} ", m.name)
				})
				.collect::<String>()
		};
		canvas.text(area.left, y0 + ch, &truncate(&members_line, cols), dim);

		let input_rows = 1;
		let body_rows = rows.saturating_sub(3 + input_rows);
		let mut lines: Vec<(String, windows_sys::Win32::Foundation::COLORREF)> = Vec::new();
		for record in &group.records {
			let (prefix, color) = match record.kind {
				RecordKind::Human => {
					let to = record.to.as_deref().unwrap_or("");
					(format!("you →{to}: "), accent)
				}
				RecordKind::Reply => (format!("{}: ", record.member.as_deref().unwrap_or("ai")), green),
				RecordKind::Suggestion => (
					format!("{} →{} (suggestion): ", record.member.as_deref().unwrap_or("ai"), record.to.as_deref().unwrap_or("")),
					dim,
				),
				RecordKind::System => (String::from("· "), dim),
				RecordKind::Error => (String::from("! "), red),
			};
			let mut first = true;
			for wrap in wrap_text(&record.text, cols.saturating_sub(prefix_width(&prefix))) {
				if first {
					lines.push((format!("{prefix}{wrap}"), color));
					first = false;
				} else {
					lines.push((format!("{}{wrap}", " ".repeat(prefix_width(&prefix))), color));
				}
			}
			if record.cost.is_some() || record.seconds.is_some() {
				let mut meta = String::new();
				if let Some(seconds) = record.seconds {
					meta.push_str(&format!("  {:.1}s", seconds));
				}
				if let Some(cost) = record.cost {
					meta.push_str(&format!("  ${cost:.4}"));
				}
				if let Some(last) = lines.last_mut() {
					last.0.push_str(&meta);
				}
			}
		}
		let total = lines.len();
		let start = total.saturating_sub(body_rows).saturating_sub(group.scroll.min(total.saturating_sub(body_rows)));
		for (i, (line, color)) in lines.iter().skip(start).take(body_rows).enumerate() {
			canvas.text(area.left, y0 + (2 + i as i32) * ch, &truncate(line, cols), *color);
		}
		if group.scroll > 0 {
			canvas.text(
				area.left + (cols as i32 - 4).max(0) * cw,
				y0 + 2 * ch,
				"↑pg",
				dim,
			);
		}
		let input_y = area.top + (rows as i32 - 1) * ch;
		let prompt = String::from("> ");
		canvas.text(area.left, input_y, &prompt, accent);
		let avail = cols.saturating_sub(2);
		let widths: Vec<usize> = group.input.iter().map(|c| c.width().unwrap_or(1)).collect();
		let mut col = widths[..group.cursor].iter().sum::<usize>();
		let mut start_idx = 0usize;
		while col >= avail && start_idx < group.cursor {
			col -= widths[start_idx];
			start_idx += 1;
		}
		let shown: String = group.input[start_idx..].iter().collect();
		canvas.text(area.left + 2 * cw, input_y, &truncate(&shown, avail), plain);
		if focused {
			let x = area.left + ((2 + col.min(avail)) as i32) * cw;
			if x < area.left + cols as i32 * cw {
				canvas.fill(x, input_y, x + cw, input_y + ch, accent);
			}
		}
		canvas.unclip(saved);
	}
}

impl Drop for GroupPane {
	fn drop(&mut self) {
		self.running.store(false, Ordering::Relaxed);
		if let Some(client) = self.client.clone() {
			let sessions = self.member_sessions();
			thread::spawn(move || {
				for (_, session) in sessions {
					let _ = client.interrupt(&session);
				}
			});
		}
	}
}

fn poll_once(data: &Arc<Mutex<Group>>, client: &Client, dirty: &Arc<AtomicBool>) {
	let members: Vec<(String, String)> = match data.lock() {
		Ok(group) => group.members.iter().map(|m| (m.name.clone(), m.session.clone())).collect(),
		Err(_) => return,
	};
	for (name, session) in members {
		let messages = match client.messages(&session) {
			Ok(messages) => messages,
			Err(err) => {
				if let Ok(mut group) = data.lock() {
					if let Some(member) = group.members.iter_mut().find(|m| m.name == name) {
						if member.error.as_deref() != Some(err.as_str()) {
							member.error = Some(err);
							dirty.store(true, Ordering::Relaxed);
						}
					}
				}
				continue;
			}
		};
		let mut replies = Vec::new();
		let mut idles: Vec<(String, String, Option<String>)> = Vec::new();
		{
			let Ok(group) = data.lock() else { return };
			for message in &messages {
				let id = message["id"].as_str().unwrap_or("").to_string();
				if id.is_empty() || group.seen.contains(&id) {
					continue;
				}
				if message["type"].as_str() == Some("idle") {
					let outcome = message["outcome"].as_str().unwrap_or("").to_string();
					let error = message["error"]["message"]
						.as_str()
						.or_else(|| message["error"]["type"].as_str())
						.map(String::from);
					idles.push((id, outcome, error));
					continue;
				}
				if let Some(reply) = assistant_reply(message) {
					replies.push((id, reply));
				}
			}
		}
		if replies.is_empty() && idles.is_empty() {
			continue;
		}
		let Ok(mut group) = data.lock() else { return };
		let names: Vec<String> = group.members.iter().map(|m| m.name.clone()).collect();
		for (id, reply) in replies {
			if group.seen.contains(&id) {
				continue;
			}
			group.seen.insert(id);
			let task = group.tasks.iter().find(|t| t.member == name).cloned();
			let seconds = task.as_ref().map(|t| t.started.elapsed().as_secs_f64());
			group.tasks.retain(|t| t.member != name);
			{
				let Some(member) = group.members.iter_mut().find(|m| m.name == name) else { break };
				member.busy = false;
			}
			match reply {
				Ok(reply) => {
					let cost = reply.cost;
					if let Some(member) = group.members.iter_mut().find(|m| m.name == name) {
						member.error = None;
						member.last_cost = cost;
					}
					if let Some(cost) = cost {
						group.total_cost += cost;
					}
					group.records.push(Record {
						kind: RecordKind::Reply,
						member: Some(name.clone()),
						to: None,
						text: reply.text.clone(),
						cost,
						seconds,
					});
					for (to, line) in suggestions(&name, &reply.text, &names) {
						group.records.push(Record {
							kind: RecordKind::Suggestion,
							member: Some(name.clone()),
							to: Some(to),
							text: line,
							cost: None,
							seconds: None,
						});
					}
				}
				Err(error) => {
					group.records.push(Record {
						kind: RecordKind::Error,
						member: Some(name.clone()),
						to: None,
						text: format!("{name}: {error}"),
						cost: None,
						seconds,
					});
				}
			}
		}
		for (id, outcome, error) in idles {
			if group.seen.contains(&id) {
				continue;
			}
			group.seen.insert(id);
			if let Some(member) = group.members.iter_mut().find(|m| m.name == name) {
				member.busy = false;
			}
			if outcome == "failed" || outcome == "interrupted" {
				let text = if outcome == "interrupted" {
					format!("{name}: 已中断")
				} else {
					format!(
						"{name}: 会话失败{}",
						error.map(|err| format!("：{err}")).unwrap_or_default()
					)
				};
				group.records.push(Record {
					kind: RecordKind::Error,
					member: Some(name.clone()),
					to: None,
					text,
					cost: None,
					seconds: None,
				});
			}
		}
		group.scroll = group.scroll.min(group.records.len());
		dirty.store(true, Ordering::Relaxed);
	}
}

#[derive(Debug)]
struct Reply {
	text: String,
	cost: Option<f64>,
}

fn assistant_reply(message: &Value) -> Option<Result<Reply, String>> {
	let role = message["type"].as_str().or_else(|| message["info"]["role"].as_str())?;
	if role != "assistant" && role != "model" {
		return None;
	}
	let finish = message["finish"]
		.as_str()
		.or_else(|| message["info"]["finish"].as_str());
	// A step that only requested tools is not a reply; the run continues.
	if matches!(finish, Some("tool-calls") | Some("tool_calls") | Some("continue")) {
		return None;
	}
	let finished = finish.is_some()
		|| message["time"]["completed"].as_u64().is_some()
		|| message["info"]["time"]["completed"].as_u64().is_some();
	if !finished {
		return None;
	}
	if let Some(error) = message.get("error") {
		if !error.is_null() {
			let text =
				error["message"].as_str().or_else(|| error["type"].as_str()).unwrap_or("unknown error");
			return Some(Err(text.to_string()));
		}
	}
	if finish == Some("error") {
		return Some(Err(String::from("opencode 返回 error")));
	}
	let mut out = String::new();
	if let Some(content) = message["content"].as_array().or_else(|| message["parts"].as_array()) {
		for part in content {
			let kind = part["type"].as_str().unwrap_or("");
			if kind == "text" {
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
		cost: message["cost"].as_f64().or_else(|| message["info"]["cost"].as_f64()),
	}))
}

fn mentions(text: &str) -> Vec<String> {
	let mut out = Vec::new();
	for token in text.split_whitespace() {
		let Some(raw) = token.strip_prefix('@') else { continue };
		let name = raw.trim_end_matches(|ch: char| !ch.is_alphanumeric() && ch != '_');
		if name.is_empty() || out.iter().any(|n| n == name) {
			continue;
		}
		out.push(String::from(name));
	}
	out
}

fn suggestions(from: &str, text: &str, names: &[String]) -> Vec<(String, String)> {
	let mut out = Vec::new();
	for line in text.lines() {
		for token in line.split_whitespace() {
			let Some(raw) = token.strip_prefix('@') else { continue };
			let name = raw.trim_end_matches(|ch: char| !ch.is_alphanumeric() && ch != '_');
			if name.is_empty() || name == from || !names.iter().any(|known| known == name) {
				continue;
			}
			if !out.iter().any(|(to, _): &(String, String)| to == name) {
				out.push((String::from(name), line.trim().to_string()));
			}
		}
	}
	out
}

fn now() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn prefix_width(prefix: &str) -> usize {
	prefix.chars().map(|c| c.width().unwrap_or(1)).sum()
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
	let width = width.max(8);
	let mut out = Vec::new();
	let mut line = String::new();
	let mut width_now = 0usize;
	for ch in text.chars() {
		if ch == '\n' {
			out.push(std::mem::take(&mut line));
			width_now = 0;
			continue;
		}
		let w = ch.width().unwrap_or(1);
		if width_now + w > width {
			out.push(std::mem::take(&mut line));
			width_now = 0;
		}
		line.push(ch);
		width_now += w;
	}
	if !line.is_empty() {
		out.push(line);
	}
	if out.is_empty() {
		out.push(String::new());
	}
	out
}

fn truncate(text: &str, cols: usize) -> String {
	let mut out = String::new();
	let mut width = 0usize;
	for ch in text.chars() {
		let w = ch.width().unwrap_or(1);
		if width + w > cols {
			break;
		}
		out.push(ch);
		width += w;
	}
	out
}

#[allow(dead_code)]
fn unused_now() -> u64 {
	now()
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn parses_flat_assistant_message() {
		let message = json!({
			"id": "msg_1",
			"time": {"created": 1, "completed": 2},
			"type": "assistant",
			"content": [{"type": "text", "text": "Hi!"}],
			"finish": "stop",
			"cost": 0.000047082
		});
		let reply = assistant_reply(&message).expect("assistant").expect("ok");
		assert_eq!(reply.text, "Hi!");
		assert!((reply.cost.unwrap() - 0.000047082).abs() < 1e-12);
	}

	#[test]
	fn ignores_unfinished_and_user_messages() {
		let unfinished = json!({"type": "assistant", "content": [{"type": "text", "text": "Hey"}]});
		assert!(assistant_reply(&unfinished).is_none());
		let user = json!({"type": "user", "text": "hi"});
		assert!(assistant_reply(&user).is_none());
	}

	#[test]
	fn ignores_tool_call_steps() {
		let step = json!({
			"type": "assistant",
			"time": {"created": 1, "completed": 2},
			"content": [{"type": "text", "text": "running"}],
			"finish": "tool-calls"
		});
		assert!(assistant_reply(&step).is_none());
		let bare_error = json!({"type": "assistant", "time": {"completed": 2}, "finish": "error"});
		assert!(assistant_reply(&bare_error).is_some_and(|reply| reply.is_err()));
	}

	#[test]
	fn reports_errors_and_empty_text() {
		let failed = json!({"type": "assistant", "finish": "error", "error": {"message": "quota"}});
		assert_eq!(assistant_reply(&failed).unwrap().unwrap_err(), "quota");
		let empty = json!({"type": "assistant", "time": {"completed": 5}, "content": []});
		assert_eq!(assistant_reply(&empty).unwrap().unwrap().text, "(no text)");
	}
}
