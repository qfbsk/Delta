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
use crate::clipboard;
use crate::input::Key;
use crate::layout::Rect;
use crate::opencode::{Client, DEFAULT_MODEL, MODEL_CHOICES};

const CODENAMES: [&str; 12] = [
	"sam", "apple", "pine", "lake", "moss", "river", "stone", "cloud", "ember", "frost", "reef",
	"dune",
];

/// AI-to-AI handoffs allowed after one human message. Members may wake each other and talk
/// back and forth; the limit only stops the exchange drifting on without the human.
const AI_EXCHANGE_LIMIT: u32 = 6;

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
	Stopping { name: String },
	Stopped { name: String },
	ClosePage { name: String },
}

#[derive(Clone)]
struct Task {
	session: String,
	started: Instant,
}

/// A text position in even. The conversation is selectable but not editable;
/// the input line is both. Variant order is the on-screen order, so a range
/// compares transcript positions before input ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EvenPos {
	Transcript { line: usize, col: usize },
	Input { col: usize },
}

struct Group {
	members: Vec<Member>,
	records: Vec<Record>,
	tasks: Vec<Task>,
	input: Vec<char>,
	cursor: usize,
	scroll: usize,
	/// Anchor and moving end of the current selection, in text order.
	selection: Option<(EvenPos, EvenPos)>,
	undo: Vec<(Vec<char>, usize)>,
	redo: Vec<(Vec<char>, usize)>,
	/// Width used by the last render; transcript selection is measured in it.
	cols: usize,
	seen: HashSet<String>,
	adding: HashSet<String>,
	stopping: HashSet<String>,
	// A remove requested during stop shares the same interrupt confirmation.
	removing: HashSet<String>,
	total_cost: f64,
	// AI-to-AI handoffs since the human last spoke, and what was already handed over in this
	// chain so the same sentence is not delivered twice.
	ai_round: u32,
	ai_said: HashSet<(String, String, String)>,
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
			selection: None,
			undo: Vec::new(),
			redo: Vec::new(),
			cols: 80,
			seen: HashSet::new(),
			adding: HashSet::new(),
			stopping: HashSet::new(),
			removing: HashSet::new(),
			total_cost: 0.0,
			ai_round: 0,
			ai_said: HashSet::new(),
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

	fn checkpoint(&mut self) {
		self.undo.push((self.input.clone(), self.cursor));
		if self.undo.len() > 200 {
			self.undo.remove(0);
		}
		self.redo.clear();
	}

	fn undo_input(&mut self) -> bool {
		let Some((input, cursor)) = self.undo.pop() else {
			return false;
		};
		let now = std::mem::replace(&mut self.input, input);
		let at = std::mem::replace(&mut self.cursor, cursor);
		self.redo.push((now, at));
		self.selection = None;
		true
	}

	fn redo_input(&mut self) -> bool {
		let Some((input, cursor)) = self.redo.pop() else {
			return false;
		};
		let now = std::mem::replace(&mut self.input, input);
		let at = std::mem::replace(&mut self.cursor, cursor);
		self.undo.push((now, at));
		self.selection = None;
		true
	}

	fn input_selection(&self) -> Option<(usize, usize)> {
		let (a, b) = self.selection?;
		let (a, b) = if a <= b { (a, b) } else { (b, a) };
		match (a, b) {
			(EvenPos::Input { col: from }, EvenPos::Input { col: to }) => {
				let len = self.input.len();
				Some((from.min(len), to.min(len).max(from.min(len))))
			}
			_ => None,
		}
	}

	fn delete_input_selection(&mut self) -> bool {
		let Some((a, b)) = self.input_selection() else {
			return false;
		};
		self.input.drain(a..b);
		self.cursor = a;
		self.selection = None;
		true
	}

	/// The selection's text. The conversation is selectable and not editable;
	/// the input line is both, so the two join at the input prompt.
	fn selected_text(&self) -> String {
		let Some((a, b)) = self.selection else {
			return String::new();
		};
		let (a, b) = if a <= b { (a, b) } else { (b, a) };
		match (a, b) {
			(EvenPos::Input { col: from }, EvenPos::Input { col: to }) => {
				let len = self.input.len();
				self.input[from.min(len)..to.min(len).max(from.min(len))]
					.iter()
					.collect()
			}
			(
				EvenPos::Transcript { line: al, col: ac },
				EvenPos::Transcript { line: bl, col: bc },
			) => {
				let rows = transcript_layout(self, self.cols).rows;
				let mut out = String::new();
				for line in al..=bl {
					let Some((text, _)) = rows.get(line) else {
						break;
					};
					let chars: Vec<char> = text.chars().collect();
					let from = if line == al { ac } else { 0 }.min(chars.len());
					let to = if line == bl { bc } else { chars.len() }
						.min(chars.len())
						.max(from);
					out.extend(&chars[from..to]);
					if line != bl {
						out.push('\n');
					}
				}
				out
			}
			_ => String::new(),
		}
	}

	fn input_index_at(&self, cols: usize, cell: usize) -> usize {
		let avail = cols.saturating_sub(2).max(1);
		let widths: Vec<usize> = self.input.iter().map(|c| c.width().unwrap_or(1)).collect();
		let mut col = widths[..self.cursor.min(widths.len())]
			.iter()
			.sum::<usize>();
		let mut start_idx = 0usize;
		while col >= avail && start_idx < self.cursor {
			col -= widths[start_idx];
			start_idx += 1;
		}
		if cell <= 2 {
			return start_idx;
		}
		let target = cell - 2;
		let mut seen = 0usize;
		for (i, width) in widths.iter().enumerate().skip(start_idx) {
			if seen + width > target {
				return i;
			}
			seen += width;
		}
		self.input.len()
	}

	/// Where a pane cell lands: the conversation body, the input line, or nothing
	/// (the header and member rows are fixed labels).
	fn side_pos(&self, cols: usize, rows: usize, row: usize, col: usize) -> Option<EvenPos> {
		if row + 1 == rows {
			return Some(EvenPos::Input {
				col: self.input_index_at(cols, col),
			});
		}
		if row < 2 {
			return None;
		}
		let body_rows = rows.saturating_sub(4);
		let rendered = transcript_layout(self, cols).rows;
		let scroll = self.scroll.min(rendered.len().saturating_sub(body_rows));
		let start = rendered
			.len()
			.saturating_sub(body_rows)
			.saturating_sub(scroll);
		let line = start + (row - 2);
		let (text, _) = rendered.get(line)?;
		let mut cell = 0usize;
		let mut index = text.chars().count();
		for (i, ch) in text.chars().enumerate() {
			let width = ch.width().unwrap_or(1);
			if cell + width > col {
				index = i;
				break;
			}
			cell += width;
		}
		Some(EvenPos::Transcript { line, col: index })
	}

	fn next_name(&self) -> String {
		for name in CODENAMES {
			if !self.adding.contains(name) && !self.members.iter().any(|m| m.name == name) {
				return String::from(name);
			}
		}
		(13..)
			.map(|i| format!("member{i}"))
			.find(|name| {
				!self.adding.contains(name) && !self.members.iter().any(|m| m.name == *name)
			})
			.unwrap()
	}

	fn session_can_act(&self, session: &str) -> bool {
		!self.stopping.contains(session) && self.members.iter().any(|m| m.session == session)
	}

	fn stop(
		&mut self,
		name: Option<&str>,
		remove: bool,
		events: &mut Vec<EvenEvent>,
	) -> Vec<String> {
		let mut sessions = Vec::new();
		for member in &self.members {
			if name.is_some_and(|name| member.name != name) {
				continue;
			}
			if remove {
				self.removing.insert(member.session.clone());
			}
			if self.stopping.insert(member.session.clone()) {
				events.push(EvenEvent::Stopping {
					name: member.name.clone(),
				});
				sessions.push(member.session.clone());
			}
		}
		sessions
	}

	fn interrupted(
		&mut self,
		session: &str,
		result: Result<(), String>,
		events: &mut Vec<EvenEvent>,
	) {
		if !self.stopping.remove(session) {
			return;
		}
		let remove = self.removing.remove(session);
		let Some(index) = self.members.iter().position(|m| m.session == session) else {
			return;
		};
		let name = self.members[index].name.clone();
		match result {
			Ok(()) => {
				self.tasks.retain(|task| task.session != session);
				self.members[index].busy = false;
				self.members[index].error = None;
				events.push(EvenEvent::Stopped { name: name.clone() });
				if remove {
					self.members.remove(index);
					events.push(EvenEvent::ClosePage { name: name.clone() });
				}
				self.push(
					RecordKind::System,
					Some(&name),
					None,
					if remove {
						format!("移除 @{name}，它的任务一起没了。")
					} else {
						format!("@{name}：已中断。")
					},
				);
			}
			Err(error) => {
				self.members[index].error = Some(error.clone());
				self.push(
					RecordKind::Error,
					Some(&name),
					None,
					format!("@{name}：中断失败：{error}"),
				);
			}
		}
	}

	fn prompt_failed(&mut self, session: &str, started: Instant, error: String) {
		let Some(index) = self.members.iter().position(|m| m.session == session) else {
			return;
		};
		// A late prompt response must not clear a newer task or an ongoing stop.
		if !self
			.tasks
			.iter()
			.any(|task| task.session == session && task.started == started)
		{
			return;
		}
		if !self.stopping.contains(session) {
			self.tasks.retain(|task| task.session != session);
			self.members[index].busy = false;
		}
		self.members[index].error = Some(error.clone());
		let name = self.members[index].name.clone();
		self.push(RecordKind::Error, Some(&name), None, error);
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
				String::from(
					"opencode server 没有起来，当前不能添加成员或发送任务。检查 DELTA_OPENCODE 指向的程序。",
				),
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
			Ok(group) => group
				.members
				.iter()
				.map(|m| (m.name.clone(), m.session.clone()))
				.collect(),
			Err(_) => Vec::new(),
		}
	}

	pub fn take_dirty(&self) -> bool {
		self.dirty.swap(false, Ordering::Relaxed)
	}

	pub fn busy_members(&self) -> Vec<String> {
		match self.inner.lock() {
			Ok(group) => group
				.members
				.iter()
				.filter(|m| m.busy)
				.map(|m| m.name.clone())
				.collect(),
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
		let Ok(group) = self.inner.lock() else {
			return String::new();
		};
		let mut out = format!("even {} ({})\n", self.name, self.root);
		for member in &group.members {
			out.push_str(&format!(
				"@{} {} {}{}\n",
				member.name,
				member.model,
				if member.busy { "busy" } else { "idle" },
				member
					.error
					.as_deref()
					.map(|e| format!(" error={e}"))
					.unwrap_or_default()
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
				record
					.to
					.as_deref()
					.map(|t| format!(" →{t}"))
					.unwrap_or_default(),
				record.text
			));
		}
		out
	}

	pub fn insert_char(&self, ch: char) {
		if let Ok(mut group) = self.inner.lock() {
			group.checkpoint();
			group.delete_input_selection();
			let cursor = group.cursor;
			group.input.insert(cursor, ch);
			group.cursor = cursor + 1;
		}
		self.dirty.store(true, Ordering::Relaxed);
	}

	pub fn insert_text(&self, text: &str) {
		if let Ok(mut group) = self.inner.lock() {
			group.checkpoint();
			group.delete_input_selection();
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

	/// Text and images share the same paste shortcut. An image is written to
	/// a temporary PNG and enters the prompt as Markdown image syntax.
	pub fn paste(&self) {
		let Some(content) = clipboard::paste() else {
			return;
		};
		match content {
			clipboard::Paste::Text(text) => self.insert_text(&text),
			clipboard::Paste::Image(png) => {
				let Some(path) = clipboard::save_image(&png) else {
					return;
				};
				self.insert_text(&format!("![]({})", path.display()));
			}
		}
	}

	pub fn backspace(&self) {
		if let Ok(mut group) = self.inner.lock() {
			group.checkpoint();
			if !group.delete_input_selection() && group.cursor > 0 {
				let cursor = group.cursor - 1;
				group.cursor = cursor;
				group.input.remove(cursor);
			}
		}
		self.dirty.store(true, Ordering::Relaxed);
	}

	pub fn delete(&self) {
		if let Ok(mut group) = self.inner.lock() {
			group.checkpoint();
			if !group.delete_input_selection() {
				let cursor = group.cursor;
				if cursor < group.input.len() {
					group.input.remove(cursor);
				}
			}
		}
		self.dirty.store(true, Ordering::Relaxed);
	}

	pub fn move_input(&self, delta: i32) {
		if let Ok(mut group) = self.inner.lock() {
			group.selection = None;
			if delta < 0 {
				group.cursor = group.cursor.saturating_sub((-delta) as usize);
			} else {
				group.cursor = (group.cursor + delta as usize).min(group.input.len());
			}
		}
	}

	pub fn input_home(&self) {
		if let Ok(mut group) = self.inner.lock() {
			group.selection = None;
			group.cursor = 0;
		}
	}

	pub fn input_end(&self) {
		if let Ok(mut group) = self.inner.lock() {
			group.selection = None;
			group.cursor = group.input.len();
		}
	}

	pub fn scroll(&self, delta: i32) {
		if let Ok(mut group) = self.inner.lock() {
			if delta > 0 {
				group.scroll = group.scroll.saturating_add(delta as usize);
			} else {
				group.scroll = group.scroll.saturating_sub((-delta) as usize);
			}
		}
	}

	pub fn wheel(&self, delta: i32) {
		self.scroll(delta * 3);
	}

	/// even's own key map. Delta only hands over the key.
	pub fn key(&self, key: &Key) -> bool {
		if !key.down {
			return true;
		}
		let ctrl = key.ctrl && !key.alt;
		let Ok(mut group) = self.inner.lock() else {
			return true;
		};
		match key.vk {
			0x41 if ctrl => {
				group.selection = Some((
					EvenPos::Input { col: 0 },
					EvenPos::Input {
						col: group.input.len(),
					},
				));
			}
			0x43 if ctrl => {
				let text = group.selected_text();
				if !text.is_empty() {
					clipboard::set(&text);
				}
			}
			0x58 if ctrl => {
				// The conversation cannot be edited, so only an input selection can be cut.
				if let Some((from, to)) = group.input_selection() {
					group.checkpoint();
					let text: String = group.input[from..to].iter().collect();
					group.input.drain(from..to);
					group.cursor = from;
					group.selection = None;
					if !text.is_empty() {
						clipboard::set(&text);
					}
				}
			}
			0x56 if ctrl => {
				drop(group);
				self.paste();
				return true;
			}
			0x5A if ctrl => {
				if key.shift {
					group.redo_input();
				} else {
					group.undo_input();
				}
			}
			0x59 if ctrl => {
				group.redo_input();
			}
			0x0D => {
				drop(group);
				self.enter();
				return true;
			}
			0x08 => {
				drop(group);
				self.backspace();
				return true;
			}
			0x2E => {
				drop(group);
				self.delete();
				return true;
			}
			0x09 => {
				group.checkpoint();
				group.delete_input_selection();
				let cursor = group.cursor;
				group.input.insert(cursor, '\t');
				group.cursor = cursor + 1;
			}
			0x1B => {}
			0x25 => {
				group.selection = None;
				group.cursor = group.cursor.saturating_sub(1);
			}
			0x27 => {
				group.selection = None;
				group.cursor = (group.cursor + 1).min(group.input.len());
			}
			0x26 => group.scroll = group.scroll.saturating_add(1),
			0x28 => group.scroll = group.scroll.saturating_sub(1),
			0x24 => {
				group.selection = None;
				group.cursor = 0;
			}
			0x23 => {
				group.selection = None;
				group.cursor = group.input.len();
			}
			0x21 => group.scroll = group.scroll.saturating_add(10),
			0x22 => group.scroll = group.scroll.saturating_sub(10),
			_ => {
				if let Some(ch) = key.ch {
					if !ch.is_control() && (!key.ctrl && !key.alt || key.right_alt) {
						group.checkpoint();
						group.delete_input_selection();
						let cursor = group.cursor;
						group.input.insert(cursor, ch);
						group.cursor = cursor + 1;
					}
				}
			}
		}
		self.dirty.store(true, Ordering::Relaxed);
		true
	}

	/// The selected text, empty when nothing is selected.
	pub fn selected_text(&self) -> String {
		self.inner
			.lock()
			.map(|group| group.selected_text())
			.unwrap_or_default()
	}

	/// Mouse press in a pane cell. The conversation starts selecting; the input
	/// line also moves its caret.
	pub fn press(&self, cols: usize, rows: usize, row: usize, col: usize) {
		let Ok(mut group) = self.inner.lock() else {
			return;
		};
		group.cols = cols;
		let Some(pos) = group.side_pos(cols, rows, row, col) else {
			return;
		};
		if let EvenPos::Input { col: at } = pos {
			group.cursor = at;
		}
		group.selection = Some((pos, pos));
	}

	/// Ctrl+click on an image path or a URL: hand it to the system's default
	/// program. Nothing else in the pane reacts to the key.
	pub fn open_at(&self, cols: usize, rows: usize, row: usize, col: usize) -> bool {
		let Ok(group) = self.inner.lock() else {
			return false;
		};
		let Some(pos) = group.side_pos(cols, rows, row, col) else {
			return false;
		};
		let (text, index) = match pos {
			EvenPos::Transcript { line, col } => {
				let layout = transcript_layout(&group, cols);
				let Some((text, _)) = layout.rows.get(line) else {
					return false;
				};
				(text.clone(), col)
			}
			EvenPos::Input { col } => (group.input.iter().collect::<String>(), col),
		};
		let Some(target) = link_at(&text, index) else {
			return false;
		};
		// The shell handshake may wait on the program it starts; never hold up the window.
		thread::spawn(move || {
			open_with_system(&target);
		});
		true
	}

	pub fn drag(&self, cols: usize, rows: usize, row: usize, col: usize) {
		let Ok(mut group) = self.inner.lock() else {
			return;
		};
		group.cols = cols;
		let Some(pos) = group.side_pos(cols, rows, row, col) else {
			return;
		};
		let Some((anchor, _)) = group.selection else {
			group.selection = Some((pos, pos));
			return;
		};
		// A selection stays inside one side: dragging from the conversation into
		// the prompt should not silently include (or cut) the typed line.
		let pos = match anchor {
			EvenPos::Transcript { .. } => match pos {
				EvenPos::Transcript { .. } => pos,
				EvenPos::Input { .. } => {
					let rows = transcript_layout(&group, cols).rows;
					let line = rows.len().saturating_sub(1);
					let col = rows
						.get(line)
						.map(|(text, _)| text.chars().count())
						.unwrap_or(0);
					EvenPos::Transcript { line, col }
				}
			},
			EvenPos::Input { .. } => EvenPos::Input {
				col: group.input_index_at(cols, col),
			},
		};
		group.selection = Some((anchor, pos));
	}

	pub fn enter(&self) {
		let line = match self.inner.lock() {
			Ok(mut group) => {
				let text: String = group.input.iter().collect();
				group.input.clear();
				group.cursor = 0;
				group.selection = None;
				group.undo.clear();
				group.redo.clear();
				text
			}
			Err(_) => return,
		};
		let line = line.trim().to_string();
		let line = half_width(&line);
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
		if message_body(&line).is_empty() {
			if let Ok(mut group) = self.inner.lock() {
				group.push(
					RecordKind::System,
					None,
					None,
					String::from("在 @代号 后写要说的话，例如 @sam 你好。"),
				);
			}
			return;
		}
		let Some(client) = self.client.clone() else {
			return;
		};
		// A human message starts a new chain; AI handoffs from the previous one no longer count.
		if let Ok(mut group) = self.inner.lock() {
			group.ai_round = 0;
			group.ai_said.clear();
		}
		for target in &targets {
			let started = Instant::now();
			let session = {
				let Ok(mut group) = self.inner.lock() else {
					return;
				};
				let Some(index) = group.members.iter().position(|m| m.name == *target) else {
					continue;
				};
				let session = group.members[index].session.clone();
				if !group.session_can_act(&session) {
					group.push(
						RecordKind::Error,
						Some(target),
						None,
						String::from("正在等待中断确认，尚未发送新任务。"),
					);
					continue;
				}
				group.tasks.retain(|t| t.session != session);
				group.tasks.push(Task {
					session: session.clone(),
					started,
				});
				let member = &mut group.members[index];
				member.busy = true;
				member.error = None;
				session
			};
			if let Ok(mut group) = self.inner.lock() {
				group.push(RecordKind::Human, Some("you"), Some(target), line.clone());
			}
			let inner = self.inner.clone();
			let dirty = self.dirty.clone();
			let client = client.clone();
			let line = message_body(&line).to_string();
			thread::spawn(move || {
				let result = client.prompt(&session, &line);
				if let Err(err) = result {
					if let Ok(mut group) = inner.lock() {
						group.prompt_failed(&session, started, err);
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
					Some(token) => match MODEL_CHOICES
						.iter()
						.find(|(id, label)| *id == token || *label == token)
					{
						Some((id, _)) => id.to_string(),
						None => {
							let choices: Vec<&str> =
								MODEL_CHOICES.iter().map(|(id, _)| *id).collect();
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
				let role = rest
					.split_once(char::is_whitespace)
					.map(|(_, role)| role.trim())
					.filter(|role| !role.is_empty());
				self.add_member(&model, role);
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
		let Some(client) = self.client.clone() else {
			return;
		};
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
		let name = match inner.lock() {
			Ok(mut group) => {
				let name = group.next_name();
				group.adding.insert(name.clone());
				name
			}
			Err(_) => return,
		};
		let identity = format!("{}:{name}", self.name);
		let running = self.running.clone();
		thread::spawn(move || {
			let title = format!("even-{name}");
			match client.create_session(&root, &title, &model, &identity, role.as_deref()) {
				Ok(session) => {
					if !running.load(Ordering::Relaxed) {
						let _ = client.interrupt(&session);
						return;
					}
					if let Ok(mut group) = inner.lock() {
						group.adding.remove(&name);
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
						group.adding.remove(&name);
						group.push(RecordKind::Error, None, None, format!("建会话失败：{err}"));
					}
					dirty.store(true, Ordering::Relaxed);
				}
			}
		});
	}

	fn remove_member(&self, name: &str) {
		if !name.trim().is_empty() {
			self.interrupt(Some(name.trim()), true);
		}
	}

	pub fn stop(&self, name: Option<&str>) {
		self.interrupt(name, false);
	}

	pub fn session_can_act(&self, session: &str) -> bool {
		self.inner
			.lock()
			.is_ok_and(|group| group.session_can_act(session))
	}

	fn interrupt(&self, name: Option<&str>, remove: bool) {
		let Some(client) = self.client.clone() else {
			return;
		};
		let sessions = {
			let Ok(mut group) = self.inner.lock() else {
				return;
			};
			let Ok(mut events) = self.events.lock() else {
				return;
			};
			group.stop(name, remove, &mut events)
		};
		let inner = self.inner.clone();
		let events = self.events.clone();
		let dirty = self.dirty.clone();
		thread::spawn(move || {
			for session in sessions {
				let result = client.interrupt(&session);
				if let Ok(mut group) = inner.lock() {
					if let Ok(mut events) = events.lock() {
						group.interrupted(&session, result, &mut events);
					}
				}
				dirty.store(true, Ordering::Relaxed);
			}
		});
	}

	pub fn render(&self, canvas: &Canvas, area: Rect, focused: bool) {
		let cw = canvas.cell_w;
		let ch = canvas.cell_h;
		let cols = (area.width() / cw).max(1) as usize;
		let rows = (area.height() / ch).max(1) as usize;
		let bg = canvas::rgb(
			canvas::DEFAULT_BG.0,
			canvas::DEFAULT_BG.1,
			canvas::DEFAULT_BG.2,
		);
		canvas.fill(
			area.left,
			area.top,
			area.left + cols as i32 * cw,
			area.top + rows as i32 * ch,
			bg,
		);
		let saved = canvas.clip((
			area.left,
			area.top,
			area.left + cols as i32 * cw,
			area.top + rows as i32 * ch,
		));
		let Ok(mut group) = self.inner.lock() else {
			canvas.unclip(saved);
			return;
		};
		let dim = canvas::rgb(130, 136, 150);
		let accent = canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2);
		let plain = canvas::rgb(
			canvas::DEFAULT_FG.0,
			canvas::DEFAULT_FG.1,
			canvas::DEFAULT_FG.2,
		);
		let y0 = area.top;
		canvas.text(
			area.left,
			y0,
			&format!("even {}  {}", self.name, self.root),
			accent,
		);
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
		group.cols = cols;
		let layout = transcript_layout(&group, cols);
		let lines = layout.rows;
		let selection = group
			.selection
			.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
		let sel_bg = canvas::rgb(
			canvas::SELECTION_BG.0,
			canvas::SELECTION_BG.1,
			canvas::SELECTION_BG.2,
		);
		let transcript_span = |line_idx: usize, text: &str| -> Option<(usize, usize)> {
			let (a, b) = selection?;
			let (
				EvenPos::Transcript {
					line: first,
					col: from,
				},
				EvenPos::Transcript {
					line: last,
					col: to,
				},
			) = (a, b)
			else {
				return None;
			};
			if line_idx < first || line_idx > last {
				return None;
			}
			let len = text.chars().count();
			let from = if line_idx == first { from } else { 0 };
			let to = if line_idx == last { to } else { len };
			if from >= to {
				return None;
			}
			Some(cell_span(text, from, to))
		};
		let total = lines.len();
		group.scroll = group.scroll.min(total.saturating_sub(body_rows));
		let start = total.saturating_sub(body_rows).saturating_sub(group.scroll);
		for (i, (line, color)) in lines.iter().skip(start).take(body_rows).enumerate() {
			let y = y0 + (2 + i as i32) * ch;
			if let Some((from, to)) = transcript_span(start + i, line) {
				let from = from.min(cols);
				let to = to.min(cols);
				if to > from {
					canvas.fill(
						area.left + from as i32 * cw,
						y,
						area.left + to as i32 * cw,
						y + ch,
						sel_bg,
					);
				}
			}
			canvas.text(area.left, y, &truncate(line, cols), *color);
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
		if let Some((EvenPos::Input { col: from }, EvenPos::Input { col: to })) = selection {
			let cell_at =
				|index: usize| -> usize { widths[..index.min(widths.len())].iter().sum::<usize>() };
			let left = cell_at(start_idx);
			let from = (2 + cell_at(from).saturating_sub(left)).max(2).min(cols);
			let to = (2 + cell_at(to).saturating_sub(left)).max(2).min(cols);
			if to > from {
				canvas.fill(
					area.left + from as i32 * cw,
					input_y,
					area.left + to as i32 * cw,
					input_y + ch,
					sel_bg,
				);
			}
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

	/// 输入行光标所在的格坐标（相对面板左上角），与 render 的画法一致。
	pub fn caret_cell(&self, cols: usize, rows: usize) -> (usize, usize) {
		let Ok(group) = self.inner.lock() else {
			return (2, rows.saturating_sub(1));
		};
		let avail = cols.saturating_sub(2);
		let widths: Vec<usize> = group.input.iter().map(|c| c.width().unwrap_or(1)).collect();
		let mut col = widths[..group.cursor.min(widths.len())]
			.iter()
			.sum::<usize>();
		let mut start_idx = 0usize;
		while col >= avail && start_idx < group.cursor {
			col -= widths[start_idx];
			start_idx += 1;
		}
		(2 + col.min(avail), rows.saturating_sub(1))
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
		Ok(group) => group
			.members
			.iter()
			.map(|m| (m.name.clone(), m.session.clone()))
			.collect(),
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
		let mut handoffs: Vec<(String, String)> = Vec::new();
		for (id, reply) in replies {
			if group.seen.contains(&id) {
				continue;
			}
			group.seen.insert(id);
			let task = group.tasks.iter().find(|t| t.session == session).cloned();
			let seconds = task.as_ref().map(|t| t.started.elapsed().as_secs_f64());
			if !group.stopping.contains(&session) {
				group.tasks.retain(|t| t.session != session);
				let Some(member) = group.members.iter_mut().find(|m| m.session == session) else {
					break;
				};
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
							to: Some(to.clone()),
							text: line.clone(),
							cost: None,
							seconds: None,
						});
						let body = message_body(&line).to_string();
						if body.is_empty() {
							continue;
						}
						if group.ai_round >= AI_EXCHANGE_LIMIT {
							group.push(
								RecordKind::System,
								None,
								None,
								format!(
									"AI 之间已经来回 {AI_EXCHANGE_LIMIT} 次，先停下；你说一句再继续。"
								),
							);
							break;
						}
						let Some(member) = group.members.iter().find(|m| m.name == to) else {
							continue;
						};
						if member.busy {
							group.push(
								RecordKind::System,
								None,
								None,
								format!("@{to} 正在忙，这条 @ 没有转过去。"),
							);
							continue;
						}
						let target_session = member.session.clone();
						if !group.session_can_act(&target_session) {
							group.push(
								RecordKind::System,
								None,
								None,
								format!("@{to} 正在等中断确认，这条 @ 没有转过去。"),
							);
							continue;
						}
						if !group
							.ai_said
							.insert((name.clone(), to.clone(), body.clone()))
						{
							group.push(
								RecordKind::System,
								None,
								None,
								format!("同样的话已经对 @{to} 说过，没有重复转。"),
							);
							continue;
						}
						group.ai_round += 1;
						group.tasks.retain(|t| t.session != target_session);
						group.tasks.push(Task {
							session: target_session.clone(),
							started: Instant::now(),
						});
						if let Some(member) = group.members.iter_mut().find(|m| m.name == to) {
							member.busy = true;
							member.error = None;
						}
						group.push(
							RecordKind::System,
							Some(&name),
							Some(&to),
							format!("@{name} 的话转给了 @{to}。"),
						);
						handoffs.push((target_session, body));
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
		for (session, body) in handoffs {
			let inner = data.clone();
			let client = client.clone();
			let dirty = dirty.clone();
			let started = Instant::now();
			thread::spawn(move || {
				if let Err(err) = client.prompt(&session, &body) {
					if let Ok(mut group) = inner.lock() {
						group.prompt_failed(&session, started, err);
					}
					dirty.store(true, Ordering::Relaxed);
				}
			});
		}
		for (id, outcome, error) in idles {
			if group.seen.contains(&id) {
				continue;
			}
			group.seen.insert(id);
			if !group.stopping.contains(&session) {
				group.tasks.retain(|t| t.session != session);
				if let Some(member) = group.members.iter_mut().find(|m| m.session == session) {
					member.busy = false;
				}
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
		dirty.store(true, Ordering::Relaxed);
	}
}

#[derive(Debug)]
struct Reply {
	text: String,
	cost: Option<f64>,
}

fn assistant_reply(message: &Value) -> Option<Result<Reply, String>> {
	let role = message["type"]
		.as_str()
		.or_else(|| message["info"]["role"].as_str())?;
	if role != "assistant" && role != "model" {
		return None;
	}
	let finish = message["finish"]
		.as_str()
		.or_else(|| message["info"]["finish"].as_str());
	// A step that only requested tools is not a reply; the run continues.
	if matches!(
		finish,
		Some("tool-calls") | Some("tool_calls") | Some("continue")
	) {
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
			let text = error["message"]
				.as_str()
				.or_else(|| error["type"].as_str())
				.unwrap_or("unknown error");
			return Some(Err(text.to_string()));
		}
	}
	if finish == Some("error") {
		return Some(Err(String::from("opencode 返回 error")));
	}
	let mut out = String::new();
	if let Some(content) = message["content"]
		.as_array()
		.or_else(|| message["parts"].as_array())
	{
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
		cost: message["cost"]
			.as_f64()
			.or_else(|| message["info"]["cost"].as_f64()),
	}))
}

/// 中文输入法会提交全角冒号和全角 @；命令和 @ 按半角解释。
fn half_width(text: &str) -> String {
	if text.contains(['：', '＠']) {
		text.replace('：', ":").replace('＠', "@")
	} else {
		text.to_string()
	}
}

fn message_body(text: &str) -> &str {
	let mut body = text.trim_start();
	while body.starts_with('@') {
		let end = body.find(char::is_whitespace).unwrap_or(body.len());
		body = body[end..].trim_start();
	}
	body
}

fn mentions(text: &str) -> Vec<String> {
	let mut out = Vec::new();
	for token in text.split_whitespace() {
		let Some(raw) = token.strip_prefix('@') else {
			continue;
		};
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
			let Some(raw) = token.strip_prefix('@') else {
				continue;
			};
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
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

fn prefix_width(prefix: &str) -> usize {
	prefix.chars().map(|c| c.width().unwrap_or(1)).sum()
}

/// The conversation as it is laid out for a given width. Selection and copy
/// use the same rows the render draws.
struct TranscriptLayout {
	rows: Vec<(String, windows_sys::Win32::Foundation::COLORREF)>,
}

fn transcript_layout(group: &Group, cols: usize) -> TranscriptLayout {
	let dim = canvas::rgb(130, 136, 150);
	let accent = canvas::rgb(canvas::ACCENT.0, canvas::ACCENT.1, canvas::ACCENT.2);
	let green = canvas::rgb(150, 205, 160);
	let red = canvas::rgb(230, 120, 120);
	let mut rows = Vec::new();
	for record in group.records.iter() {
		let (prefix, color) = match record.kind {
			RecordKind::Human => {
				let to = record.to.as_deref().unwrap_or("");
				(format!("you →{to}: "), accent)
			}
			RecordKind::Reply => (
				format!("{}: ", record.member.as_deref().unwrap_or("ai")),
				green,
			),
			RecordKind::Suggestion => (
				format!(
					"{} →{} (suggestion): ",
					record.member.as_deref().unwrap_or("ai"),
					record.to.as_deref().unwrap_or("")
				),
				dim,
			),
			RecordKind::System => (String::from("· "), dim),
			RecordKind::Error => (String::from("! "), red),
		};
		let mut first = true;
		for wrap in wrap_text(&record.text, cols.saturating_sub(prefix_width(&prefix))) {
			if first {
				rows.push((format!("{prefix}{wrap}"), color));
				first = false;
			} else {
				rows.push((
					format!("{}{wrap}", " ".repeat(prefix_width(&prefix))),
					color,
				));
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
			if let Some(last) = rows.last_mut() {
				last.0.push_str(&meta);
			}
		}
	}
	TranscriptLayout { rows }
}

/// The image path or URL under a character index of one rendered row.
/// `![](path)` yields `path`; a bare path or http(s) link is returned whole.
fn link_at(text: &str, index: usize) -> Option<String> {
	let chars: Vec<char> = text.chars().collect();
	if chars.is_empty() {
		return None;
	}
	let index = index.min(chars.len() - 1);
	if chars[index].is_whitespace() {
		return None;
	}
	let mut lo = index;
	while lo > 0 && !chars[lo - 1].is_whitespace() {
		lo -= 1;
	}
	let mut hi = index;
	while hi + 1 < chars.len() && !chars[hi + 1].is_whitespace() {
		hi += 1;
	}
	let token: String = chars[lo..=hi].iter().collect();
	let target = match token.split_once("![](") {
		Some((_, rest)) => rest.trim_end_matches(')').to_string(),
		None => {
			let trim = |c: char| "()[]<>\"'`,;:".contains(c);
			let mut s = token.trim_matches(trim).to_string();
			while s.ends_with(['.', '。', '，', '、', '；']) {
				s.pop();
			}
			s
		}
	};
	if target.is_empty() {
		return None;
	}
	if target.starts_with("http://") || target.starts_with("https://") {
		return Some(target);
	}
	if target.contains('\\') || target.contains('/') {
		return Some(target);
	}
	None
}

/// Open a path or URL with whatever the system registered for it.
fn open_with_system(target: &str) -> bool {
	use windows_sys::Win32::UI::Shell::ShellExecuteW;
	use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
	let file = if target.starts_with("http://") || target.starts_with("https://") {
		target.to_string()
	} else {
		let path = target.strip_prefix("file:///").unwrap_or(target);
		let path = std::path::Path::new(path);
		if !path.exists() {
			return false;
		}
		path.display().to_string()
	};
	let file: Vec<u16> = file.encode_utf16().chain(std::iter::once(0)).collect();
	let verb: Vec<u16> = "open\0".encode_utf16().collect();
	let result = unsafe {
		ShellExecuteW(
			std::ptr::null_mut(),
			verb.as_ptr(),
			file.as_ptr(),
			std::ptr::null(),
			std::ptr::null(),
			SW_SHOWNORMAL,
		)
	};
	result as isize > 32
}

/// Display cells between two character indices of a rendered row.
fn cell_span(text: &str, from: usize, to: usize) -> (usize, usize) {	let chars: Vec<char> = text.chars().collect();
	let from = from.min(chars.len());
	let to = to.min(chars.len()).max(from);
	let start: usize = chars[..from].iter().map(|c| c.width().unwrap_or(1)).sum();
	let width: usize = chars[from..to].iter().map(|c| c.width().unwrap_or(1)).sum();
	(start, start + width)
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
	let width = width.max(1);
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
	fn routing_prefix_is_not_an_opencode_mention() {
		assert_eq!(
			message_body("@sam hello ,who are you"),
			"hello ,who are you"
		);
		assert_eq!(
			message_body("@sam @apple\t请核对 @lake 的建议"),
			"请核对 @lake 的建议"
		);
		assert_eq!(message_body("@sam"), "");
		assert_eq!(message_body("  原样保留  空格"), "原样保留  空格");
	}

	#[test]
	fn pending_members_reserve_distinct_names() {
		let mut group = Group::new();
		assert_eq!(group.next_name(), "sam");
		group.adding.insert("sam".into());
		assert_eq!(group.next_name(), "apple");
	}

	#[test]
	fn caret_follows_input_and_scrolls_with_the_text() {
		let pane = GroupPane::new("even1", "E:/Delta", None);
		pane.insert_text("：add");
		assert_eq!(pane.caret_cell(40, 10), (7, 9));
		pane.insert_text(&"a".repeat(50));
		assert_eq!(pane.caret_cell(20, 10), (19, 9));
	}

	#[test]
	fn full_width_command_markers_are_folded() {
		assert_eq!(half_width("：add"), ":add");
		assert_eq!(half_width("＠sam 你好"), "@sam 你好");
		assert_eq!(half_width("Hello，世界"), "Hello，世界");
		assert_eq!(mentions(&half_width("＠sam 你好")), vec!["sam".to_string()]);
	}

	#[test]
	fn stop_requires_confirmation_and_failure_keeps_task() {
		let mut group = Group::new();
		group.members.push(Member {
			name: "sam".into(),
			model: "test".into(),
			role: None,
			session: "s1".into(),
			busy: true,
			error: None,
			last_cost: None,
		});
		group.tasks.push(Task {
			session: "s1".into(),
			started: Instant::now(),
		});
		let mut events = Vec::new();
		assert_eq!(group.stop(Some("sam"), false, &mut events), vec!["s1"]);
		assert!(!group.session_can_act("s1"));
		assert!(group.members[0].busy);
		assert_eq!(group.tasks.len(), 1);
		assert!(group.stop(Some("sam"), false, &mut events).is_empty());
		group.interrupted("s1", Err("offline".into()), &mut events);
		assert!(group.members[0].busy);
		assert_eq!(group.tasks.len(), 1);
		assert!(group.session_can_act("s1"));
		assert!(
			!events
				.iter()
				.any(|event| matches!(event, EvenEvent::Stopped { .. }))
		);
		group.stop(Some("sam"), true, &mut events);
		assert_eq!(group.members.len(), 1);
		group.interrupted("s1", Ok(()), &mut events);
		assert!(group.members.is_empty());
		assert!(group.tasks.is_empty());
		assert!(!group.session_can_act("s1"));
		assert!(
			events
				.iter()
				.any(|event| matches!(event, EvenEvent::ClosePage { name } if name == "sam"))
		);
	}

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
	fn transcript_positions_sort_before_input() {
		assert!(
			EvenPos::Transcript { line: 99, col: 99 } < EvenPos::Input { col: 0 },
			"the conversation must never sort after the prompt"
		);
	}

	#[test]
	fn input_selection_copies_and_cuts_only_the_prompt() {
		let mut group = Group::new();
		group.input = "hello".chars().collect();
		group.cursor = 5;
		group.selection = Some((EvenPos::Input { col: 1 }, EvenPos::Input { col: 4 }));
		assert_eq!(group.selected_text(), "ell");
		assert_eq!(group.input_selection(), Some((1, 4)));
		assert!(group.delete_input_selection());
		assert_eq!(group.input.iter().collect::<String>(), "ho");
		assert_eq!(group.cursor, 1);
		assert!(group.selection.is_none());
	}

	#[test]
	fn conversation_selection_copies_but_never_cuts_the_prompt() {
		let mut group = Group::new();
		group.input = "typed".chars().collect();
		group.push(RecordKind::Human, None, None, "hello".into());
		let rows = transcript_layout(&group, 80).rows;
		assert!(rows.len() >= 1);
		let line = 0;
		let len = rows[line].0.chars().count();
		group.selection = Some((
			EvenPos::Transcript { line, col: 0 },
			EvenPos::Transcript { line, col: len },
		));
		assert_eq!(group.selected_text(), rows[line].0);
		assert!(group.input_selection().is_none());
		assert!(!group.delete_input_selection());
		assert_eq!(group.input.iter().collect::<String>(), "typed");
	}

	#[test]
	fn markdown_image_and_url_tokens_are_found_under_the_pointer() {
		assert_eq!(
			link_at("see ![](C:\\Temp\\a b.png) now", 8).as_deref(),
			Some("C:\\Temp\\a")
		);
		assert_eq!(
			link_at("![](C:\\Temp\\a.png)", 3).as_deref(),
			Some("C:\\Temp\\a.png")
		);
		assert_eq!(
			link_at("open https://x.test/a.png please", 8).as_deref(),
			Some("https://x.test/a.png")
		);
		assert_eq!(link_at("plain words", 3), None);
	}

	#[test]
	fn input_undo_and_redo_restore_the_prompt() {
		let mut group = Group::new();
		group.checkpoint();
		group.input = "one".chars().collect();
		group.cursor = 3;
		group.checkpoint();
		group.input = "one two".chars().collect();
		group.cursor = 7;
		assert!(group.undo_input());
		assert_eq!(group.input.iter().collect::<String>(), "one");
		assert!(group.undo_input());
		assert_eq!(group.input.iter().collect::<String>(), "");
		assert!(group.redo_input());
		assert_eq!(group.input.iter().collect::<String>(), "one");
		assert!(group.redo_input());
		assert_eq!(group.input.iter().collect::<String>(), "one two");
		assert!(group.selection.is_none());
	}

	#[test]
	fn side_pos_maps_input_row_and_rejects_header_rows() {
		let mut group = Group::new();
		group.input = "abc".chars().collect();
		group.cols = 80;
		assert_eq!(group.side_pos(80, 10, 0, 0), None);
		assert_eq!(group.side_pos(80, 10, 1, 0), None);
		assert_eq!(
			group.side_pos(80, 10, 9, 4),
			Some(EvenPos::Input { col: 2 })
		);
	}

	#[test]
	fn reports_errors_and_empty_text() {
		let failed = json!({"type": "assistant", "finish": "error", "error": {"message": "quota"}});
		assert_eq!(assistant_reply(&failed).unwrap().unwrap_err(), "quota");
		let empty = json!({"type": "assistant", "time": {"completed": 5}, "content": []});
		assert_eq!(assistant_reply(&empty).unwrap().unwrap().text, "(no text)");
	}
}
