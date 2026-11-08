//! 控制平面本体：对象、绑定与法则的可执行形式。
//!
//! 没有窗口、没有 PTY、没有 UI——这些全是法则 1–25 的机器可读版本，
//! 因为 headless 是硬要求：AI 能做的一切都必须能在没有窗口的情况下做。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::actor::{Actor, ActorKind};
use crate::event::{Event, EventState, Subject};
use crate::group::Group;
use crate::ids::{ActorId, EditId, EventId, GroupId, Ids, WindowId};
use crate::keyboard::{Esc, InputMode, Keyboard};
use crate::record::{LedgerRow, NullRecorder, OpKind, Recorder, RowSubject};
use crate::window::{CloseDenial, KeymapOwner, OwnershipMode, Window, WindowKind};

/// 法则 4：接管已存在的窗口要不要申请。读取永远不会走到这里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Takeover {
	/// 法则 3：窗口存在，AI 就接管。
	Granted,
	/// 人类以前台非 agent/非 share 的形式打开过，所以要问一次。
	NeedsConsent { window: WindowId, ai: ActorId },
	/// 法则 17：群聊窗口没有归属模式，接管这个动作对它不成立。
	NotTakeoverable(WindowId),
}

/// 法则 22：群聊关闭就是 AI 关闭。这里返回它究竟停了谁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupClosed {
	pub group: GroupId,
	pub stopped: Vec<EventId>,
	pub ai_closed: Vec<ActorId>,
}

/// 法则 8：无窗口的事件不存在。唯一能触发的原因是那个 actor 没有窗口可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoOutputWindow(pub ActorId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEdit {
	pub ai: ActorId,
	pub window: WindowId,
	pub path: PathBuf,
}

pub struct Delta {
	ids: Ids,
	pub actors: HashMap<ActorId, Actor>,
	pub windows: HashMap<WindowId, Window>,
	pub events: HashMap<EventId, Event>,
	pub groups: HashMap<GroupId, Group>,
	pub edits: HashMap<EditId, LiveEdit>,
	pub keyboard: Keyboard,
	/// 法则 9：z-order，最后一项在最上面。下面的先不画。
	pub z_order: Vec<WindowId>,
	/// 有序事件日志的序号。真正的 ledger 属于后面那一层（见 `record`）。
	pub tick: u64,
	recorder: Box<dyn Recorder>,
}

impl Default for Delta {
	fn default() -> Self {
		Self::null()
	}
}

impl Delta {
	pub fn new(recorder: Box<dyn Recorder>) -> Self {
		Self {
			ids: Ids::default(),
			actors: HashMap::new(),
			windows: HashMap::new(),
			events: HashMap::new(),
			groups: HashMap::new(),
			edits: HashMap::new(),
			keyboard: Keyboard::default(),
			z_order: Vec::new(),
			tick: 0,
			recorder,
		}
	}

	/// 本期没有数据库，所以默认用空实现。
	pub fn null() -> Self {
		Self::new(Box::<NullRecorder>::default())
	}

	fn row(&mut self, actor: ActorId, op: OpKind, subject: RowSubject) {
		self.tick += 1;
		self.recorder.append(LedgerRow {
			at: self.tick,
			actor,
			op,
			subject,
		});
	}

	/// 法则 21：每个 AI 都有自己的一个窗口，调度器和它的实时流都在里面。
	pub fn add_ai(&mut self) -> ActorId {
		let ai = self.ids.actor();
		let console = self.push_window(WindowKind::AgentConsole { ai });
		self.actors.insert(
			ai,
			Actor {
				id: ai,
				kind: ActorKind::Agent,
				console: Some(console),
				closed: false,
			},
		);
		ai
	}

	pub fn add_human(&mut self) -> ActorId {
		let human = self.ids.actor();
		self.actors.insert(
			human,
			Actor {
				id: human,
				kind: ActorKind::Human,
				console: None,
				closed: false,
			},
		);
		human
	}

	fn push_window(&mut self, kind: WindowKind) -> WindowId {
		let id = self.ids.window();
		self.windows.insert(id, Window::new(id, kind));
		self.z_order.push(id);
		id
	}

	/// 法则 23：工作区不是界面的层级，只是群聊开启时选中的根路径。
	pub fn open_group(&mut self, human: ActorId, root: PathBuf) -> GroupId {
		let group = self.ids.group();
		let window = self.push_window(WindowKind::Group { group });
		self.groups.insert(
			group,
			Group {
				id: group,
				root,
				window,
				members: vec![human],
				open: true,
			},
		);
		group
	}

	pub fn group_add_ai(&mut self, group: GroupId, ai: ActorId) -> bool {
		let Some(g) = self.groups.get_mut(&group) else {
			return false;
		};
		if !g.open || g.members.contains(&ai) {
			return false;
		}
		g.members.push(ai);
		true
	}

	/// 人类打开一个 shell pane（默认 profile 是 solid，法则 30 of 清单）。
	pub fn open_terminal(&mut self, cwd: PathBuf) -> WindowId {
		self.push_window(WindowKind::Terminal { cwd })
	}

	/// 人类打开一个 miku 窗口。人开的东西归属就是人，并且这一步就是"在前台打开过"。
	pub fn open_editor(&mut self, by: ActorId, path: PathBuf) -> WindowId {
		let window = self.push_window(WindowKind::Editor { path });
		if let Some(w) = self.windows.get_mut(&window) {
			w.owner = Some(by);
		}
		self.focus(by, window);
		window
	}

	/// 法则 4：读取不申请。这条路径上没有 Takeover，也没有 Consent。
	pub fn peek(&self, _ai: ActorId, window: WindowId) -> Option<&Window> {
		self.windows.get(&window)
	}

	pub fn group_window(&self, group: GroupId) -> WindowId {
		self.groups[&group].window
	}

	pub fn group_root(&self, group: GroupId) -> PathBuf {
		self.groups[&group].root.clone()
	}

	/// 法则 19：全屏程序原地开，所以键映射换了主人；程序自己退出后窗口还在。
	pub fn set_keymap(&mut self, window: WindowId, keymap: KeymapOwner) -> bool {
		let Some(w) = self.windows.get_mut(&window) else {
			return false;
		};
		w.keymap = keymap;
		true
	}

	pub fn window(&self, id: WindowId) -> Option<&Window> {
		self.windows.get(&id)
	}

	/// 法则 21：那个 AI 的窗口（调度器 + 实时流）。
	pub fn console(&self, ai: ActorId) -> Option<WindowId> {
		self.actors.get(&ai)?.console
	}

	pub fn drawn(&self) -> Option<WindowId> {
		self.z_order.last().copied()
	}

	/// 法则 9：睡眠不是关闭——进程、事件与 pin 都还在，只是不画。
	pub fn sleep(&mut self, window: WindowId) -> bool {
		let Some(at) = self.z_order.iter().position(|w| *w == window) else {
			return false;
		};
		let id = self.z_order.remove(at);
		self.z_order.insert(0, id);
		true
	}

	pub fn raise(&mut self, window: WindowId) -> bool {
		let Some(at) = self.z_order.iter().position(|w| *w == window) else {
			return false;
		};
		let id = self.z_order.remove(at);
		self.z_order.push(id);
		true
	}

	/// 焦点不等于所有权。人类把键盘移过去，就是在这一刻把"在前台看过"记进历史。
	pub fn focus(&mut self, by: ActorId, window: WindowId) -> bool {
		let Some(w) = self.windows.get_mut(&window) else {
			return false;
		};
		let human = self.actors.get(&by).map(Actor::is_human).unwrap_or(false);
		if human
			&& w.ownership == OwnershipMode::Normal
			&& !matches!(w.kind, WindowKind::Group { .. })
		{
			w.note_human_foreground();
		}
		self.keyboard.focus(window);
		true
	}

	pub fn set_input(&mut self, mode: InputMode) {
		self.keyboard.input = mode;
	}

	/// 法则 13 + 推导：ESC 只在 delta 自己的键映射上有意义。
	pub fn esc(&mut self) -> Option<Esc> {
		let keymap = self.keymap_of_focused()?;
		Some(self.keyboard.esc(keymap))
	}

	fn keymap_of_focused(&self) -> Option<KeymapOwner> {
		Some(self.windows.get(&self.keyboard.focused?)?.keymap)
	}

	/// 法则 3、4：AI 要接管一个已存在的窗口。
	pub fn request_takeover(&mut self, ai: ActorId, window: WindowId) -> Takeover {
		let Some(w) = self.windows.get(&window) else {
			return Takeover::NotTakeoverable(window);
		};
		if matches!(w.kind, WindowKind::Group { .. }) {
			return Takeover::NotTakeoverable(window);
		}
		if w.needs_consent(ai) {
			return Takeover::NeedsConsent { window, ai };
		}
		self.enter_agent(ai, window);
		Takeover::Granted
	}

	/// 法则 4：同意即自动保存并进入 agent；"人类在前台看过"这条记录随之清掉。
	pub fn grant_takeover(&mut self, ai: ActorId, window: WindowId) -> bool {
		if !self.windows.contains_key(&window) {
			return false;
		}
		{
			let w = self.windows.get_mut(&window).unwrap();
			w.grant_consent(ai);
		}
		self.row(ai, OpKind::Consent, RowSubject::Window(window));
		self.enter_agent(ai, window);
		true
	}

	/// 法则 14：`agent` 的进出只由 AI 决定。
	pub fn enter_agent(&mut self, ai: ActorId, window: WindowId) -> bool {
		let Some(w) = self.windows.get_mut(&window) else {
			return false;
		};
		if w.ownership.holder().is_some_and(|other| other != ai) {
			return false;
		}
		w.ownership = OwnershipMode::Agent(ai);
		w.owner = Some(ai);
		self.row(ai, OpKind::Takeover, RowSubject::Window(window));
		true
	}

	pub fn release_agent(&mut self, ai: ActorId, window: WindowId) -> bool {
		let Some(w) = self.windows.get_mut(&window) else {
			return false;
		};
		if w.ownership.holder() != Some(ai) {
			return false;
		}
		w.ownership = OwnershipMode::Normal;
		true
	}

	/// 法则 15：share 是 AI 把人类请求进来的状态。人类不能关、不能退。
	pub fn ask_share(&mut self, ai: ActorId, window: WindowId) -> bool {
		let Some(w) = self.windows.get_mut(&window) else {
			return false;
		};
		if w.ownership.holder() != Some(ai) {
			return false;
		}
		w.ownership = OwnershipMode::Share(ai);
		true
	}

	/// 法则 15：*"你在群组跟他说好了，他就会立刻切换为 agent 模式"*。
	pub fn share_resolved_in_group(&mut self, window: WindowId) -> bool {
		let Some(w) = self.windows.get_mut(&window) else {
			return false;
		};
		let Some(ai) = w.ownership.holder() else {
			return false;
		};
		if !matches!(w.ownership, OwnershipMode::Share(_)) {
			return false;
		}
		w.ownership = OwnershipMode::Agent(ai);
		true
	}

	/// 法则 2 与 11：AI 编辑什么，什么就有窗口；已有窗口就接管而不是复制（按路径幂等）。
	/// 没有窗口时开出来的那个不抢焦点——看见不等于焦点。
	pub fn begin_edit(&mut self, ai: ActorId, path: PathBuf) -> Result<EditId, Takeover> {
		let existing = self
			.windows
			.iter()
			.find(|(_, w)| {
				w.bound_path() == Some(&path) && matches!(w.kind, WindowKind::Editor { .. })
			})
			.map(|(id, _)| *id);
		let window = match existing {
			Some(window) => match self.request_takeover(ai, window) {
				Takeover::Granted => window,
				ask => return Err(ask),
			},
			None => self.push_window(WindowKind::Editor { path: path.clone() }),
		};
		let token = self.ids.edit();
		self.edits.insert(
			token,
			LiveEdit {
				ai,
				window,
				path: path.clone(),
			},
		);
		let w = self.windows.get_mut(&window).unwrap();
		w.add_edit(token);
		self.row(ai, OpKind::Edit, RowSubject::Path(path));
		Ok(token)
	}

	/// 法则 2：编辑完成的瞬间 pin 就解除。机器开的窗口随后沉到图层底部，不删除。
	pub fn end_edit(&mut self, token: EditId) -> bool {
		let Some(live) = self.edits.remove(&token) else {
			return false;
		};
		let sink = if let Some(w) = self.windows.get_mut(&live.window) {
			w.remove_edit(token);
			w.pin_count() == 0 && w.owner == None && matches!(w.kind, WindowKind::Editor { .. })
		} else {
			false
		};
		if sink {
			self.sleep(live.window);
		}
		true
	}

	/// 法则 46：*"AI删除就删除呗，不显示"*。不开窗口、不显示，唯一痕迹是记录层那一行。
	pub fn ai_delete(&mut self, ai: ActorId, path: PathBuf) {
		self.row(ai, OpKind::Delete, RowSubject::Path(path));
	}

	/// 法则 8 与 10：事件必绑窗口，一个事件的 output 只有一个窗口。
	/// 默认绑到那个 AI 自己的窗口（法则 21）。
	pub fn start_event(
		&mut self,
		ai: ActorId,
		subject: Subject,
	) -> Result<EventId, NoOutputWindow> {
		let Some(console) = self
			.actors
			.get(&ai)
			.and_then(|a| (!a.closed).then_some(a.console).flatten())
		else {
			return Err(NoOutputWindow(ai));
		};
		self.start_event_in(ai, console, subject)
	}

	/// 法则 7：终端窗口一样交接，所以事件的 output 也可以是一个人类看得见的 pane。
	pub fn start_event_in(
		&mut self,
		ai: ActorId,
		output: WindowId,
		subject: Subject,
	) -> Result<EventId, NoOutputWindow> {
		if !self.windows.contains_key(&output) {
			return Err(NoOutputWindow(ai));
		}
		let id = self.ids.event();
		self.events.insert(
			id,
			Event {
				id,
				ai,
				output,
				state: EventState::Running,
				subject,
			},
		);
		self.windows.get_mut(&output).unwrap().add_event(id);
		self.row(ai, OpKind::EventStart, RowSubject::Event(id));
		Ok(id)
	}

	/// 推导：pin 数的是活着的事件行（进程没退出），不是任何人口里的 `done`。
	pub fn finish_event(&mut self, id: EventId) -> bool {
		self.unbind(id)
	}

	/// 法则 8：停止在事件调度器上，任何人随时可用；不藏在关闭键里。
	/// 推导：停止必须把 `agent`/`share` 一起清掉，否则挂住的 AI 留下一个既关不掉也退不出的窗口。
	pub fn stop_event(&mut self, id: EventId) -> bool {
		let Some(event) = self.events.get(&id) else {
			return false;
		};
		let (ai, output) = (event.ai, event.output);
		let stopped = self.unbind(id);
		if stopped {
			self.row(ai, OpKind::EventStop, RowSubject::Event(id));
			let w = self.windows.get_mut(&output).unwrap();
			if w.ownership.holder() == Some(ai) {
				w.ownership = OwnershipMode::Normal;
			}
		}
		stopped
	}

	fn unbind(&mut self, id: EventId) -> bool {
		let Some(event) = self.events.get_mut(&id) else {
			return false;
		};
		if event.state == EventState::Finished {
			return false;
		}
		event.state = EventState::Finished;
		if let Some(w) = self.windows.get_mut(&event.output) {
			w.remove_event(id);
		}
		true
	}

	/// 法则 21：那个 AI 的窗口就是调度器，"这个可以更好的关闭"。
	pub fn stop_ai(&mut self, ai: ActorId) -> Vec<EventId> {
		let ids: Vec<EventId> = self
			.events
			.iter()
			.filter(|(_, e)| e.ai == ai && e.is_live())
			.map(|(id, _)| *id)
			.collect();
		for id in &ids {
			self.stop_event(*id);
		}
		ids
	}

	pub fn scheduler_view(&self, ai: ActorId) -> Vec<&Event> {
		let mut live: Vec<&Event> = self
			.events
			.values()
			.filter(|e| e.ai == ai && e.is_live())
			.collect();
		live.sort_by_key(|e| e.id.0);
		live
	}

	/// 法则 1 与 10：拒绝关闭给出的是一份清单（每个跳转目标是调度器里的那一条事件）。
	pub fn request_close(&mut self, window: WindowId) -> Result<(), CloseDenial> {
		let Some(w) = self.windows.get(&window) else {
			return Ok(());
		};
		if let WindowKind::Group { group } = &w.kind {
			let group = *group;
			return Err(CloseDenial::GroupCloseIsStopAction { window, group });
		}
		if w.pin_count() > 0 {
			let mut events: Vec<EventId> = w.events.iter().copied().collect();
			events.sort_by_key(|e| e.0);
			return Err(CloseDenial::Pinned { window, events });
		}
		if let Some(ai) = w.ownership.holder() {
			return Err(CloseDenial::Owned {
				window,
				ai,
				mode: w.ownership,
			});
		}
		Ok(())
	}

	pub fn close(&mut self, window: WindowId) -> Result<(), CloseDenial> {
		self.request_close(window)?;
		self.windows.remove(&window);
		if let Some(at) = self.z_order.iter().position(|w| *w == window) {
			self.z_order.remove(at);
		}
		if self.keyboard.focused == Some(window) {
			self.keyboard.focused = None;
		}
		Ok(())
	}

	/// 法则 22：群聊关闭就是 AI 关闭。这是一个"停止"动作，不是绕过 pin 的关闭，
	/// 所以它由人类发起并记在自己的账上（法则 8：停止与关闭是两个面）。
	pub fn close_group(&mut self, by: ActorId, group: GroupId) -> GroupClosed {
		let mut stopped = Vec::new();
		let mut ai_closed = Vec::new();
		let (members, window) = match self.groups.get(&group) {
			Some(g) if g.open => (g.members.clone(), g.window),
			_ => {
				return GroupClosed {
					group,
					stopped,
					ai_closed,
				};
			}
		};
		for member in members {
			stopped.extend(self.stop_ai(member));
			if self.actors.get(&member).map(|a| a.kind) == Some(ActorKind::Agent) {
				if let Some(a) = self.actors.get_mut(&member) {
					a.closed = true;
				}
				ai_closed.push(member);
			}
		}
		self.row(by, OpKind::GroupClose, RowSubject::Group(group));
		if let Some(g) = self.groups.get_mut(&group) {
			g.open = false;
		}
		self.windows.remove(&window);
		if let Some(at) = self.z_order.iter().position(|w| *w == window) {
			self.z_order.remove(at);
		}
		GroupClosed {
			group,
			stopped,
			ai_closed,
		}
	}

	pub fn windows_for_path(&self, path: &Path) -> Vec<WindowId> {
		let mut ids: Vec<WindowId> = self
			.windows
			.values()
			.filter(|w| w.bound_path().is_some_and(|p| p.as_path() == path))
			.map(|w| w.id)
			.collect();
		ids.sort_by_key(|w| w.0);
		ids
	}

	/// 法则 9 + 推导：只画最前面那一个；下面的存在、被计数、随时可切回。
	pub fn visible_window_ids(&self) -> HashSet<WindowId> {
		self.drawn().into_iter().collect()
	}

	pub fn counted_but_hidden(&self) -> usize {
		self.windows
			.len()
			.saturating_sub(self.visible_window_ids().len())
	}
}
