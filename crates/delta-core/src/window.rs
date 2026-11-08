//! 法则 1–4、9、10、15–18 里的"窗口"这个对象。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::ids::{ActorId, EventId, GroupId, WindowId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowKind {
	/// 一个 shell pane。法则 7：终端窗口和编辑器窗口一样交接。
	Terminal { cwd: PathBuf },
	/// miku 显示一个文件。法则 2/11：AI 编辑什么，就有一个这样的窗口。
	Editor { path: PathBuf },
	/// 法则 21：调度器与这个 AI 的实时流式信息是同一个窗口。
	AgentConsole { ai: ActorId },
	/// 法则 22：群聊也是一个窗口。
	Group { group: GroupId },
}

/// 归属模式：记在窗口上，无视焦点，决定能不能关（法则 16）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipMode {
	Normal,
	/// 法则 18：人有眼睛没键盘。
	Agent(ActorId),
	/// 法则 15：人能输入，不能关，不能退；AI 实时看输入。
	Share(ActorId),
}

impl OwnershipMode {
	/// 法则 16：归属模式决定能不能关，与焦点无关。`Normal` 没有 holder。
	pub fn holder(self) -> Option<ActorId> {
		match self {
			OwnershipMode::Normal => None,
			OwnershipMode::Agent(ai) | OwnershipMode::Share(ai) => Some(ai),
		}
	}
}

/// 键盘在这个窗口里撞见的是谁的键映射。
///
/// 这不是被作废的 DECSET-1049 提案：这里说的是"哪个程序拿着这个 pane"（法则 19 原地开），
/// 不是从转义序列猜布局。法则 13 的 ESC 只对自己那侧有意义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapOwner {
	/// chrome 自己，或 miku（键位不学 vim，所以 ESC 在这里不歧义）。
	Delta,
	/// 外来全屏程序（`nvim`、`less`…）。
	ForeignApp,
}

#[derive(Debug)]
pub struct Window {
	pub id: WindowId,
	pub kind: WindowKind,
	pub ownership: OwnershipMode,
	pub keymap: KeymapOwner,
	/// 法则 4 那条"谁是最终归属"的历史落在这里。
	pub owner: Option<ActorId>,
	/// 法则 10：绑定的 output 事件，一个窗口可以同时绑好几个。
	pub events: HashSet<EventId>,
	/// 法则 2：只在这次编辑在飞期间的绑定。
	live_edits: HashSet<crate::ids::EditId>,
	/// 法则 4：人类以非 agent、非 share 的形式在前台打开过几次。
	pub foreground_epoch: u64,
	/// (窗口, AI) 各自的水位线：授权过就等于把这条记录清掉（清单第 49 条）。
	consented_at: HashMap<ActorId, u64>,
}

impl Window {
	pub(crate) fn new(id: WindowId, kind: WindowKind) -> Self {
		Self {
			id,
			kind,
			ownership: OwnershipMode::Normal,
			keymap: KeymapOwner::Delta,
			owner: None,
			events: HashSet::new(),
			live_edits: HashSet::new(),
			foreground_epoch: 0,
			consented_at: HashMap::new(),
		}
	}

	/// 法则 1 与法则 10："不能关"是按绑定的东西计数，不是布尔开关。
	pub fn pin_count(&self) -> usize {
		self.events.len() + self.live_edits.len()
	}

	/// 法则 4：读取不申请；接管需要申请，判据是人类有没有把它在前台打开过。
	pub fn needs_consent(&self, ai: ActorId) -> bool {
		self.foreground_epoch > 0
			&& self.consented_at.get(&ai).copied().unwrap_or(0) < self.foreground_epoch
	}

	pub fn note_human_foreground(&mut self) {
		self.foreground_epoch += 1;
	}

	/// 同意即自动保存并进入 agent（法则 4），并清掉"人类在前台看过"这条待办的记录。
	pub fn grant_consent(&mut self, ai: ActorId) {
		self.consented_at.insert(ai, self.foreground_epoch);
	}

	pub(crate) fn add_event(&mut self, id: EventId) -> bool {
		self.events.insert(id)
	}

	pub(crate) fn remove_event(&mut self, id: EventId) -> bool {
		self.events.remove(&id)
	}

	pub(crate) fn add_edit(&mut self, token: crate::ids::EditId) -> bool {
		self.live_edits.insert(token)
	}

	pub(crate) fn remove_edit(&mut self, token: crate::ids::EditId) -> bool {
		self.live_edits.remove(&token)
	}

	pub fn bound_path(&self) -> Option<&PathBuf> {
		match &self.kind {
			WindowKind::Editor { path } => Some(path),
			WindowKind::Terminal { cwd } => Some(cwd),
			_ => None,
		}
	}
}

/// 法则 1 的"不能关"：拒绝必须给出原因列表，每个原因都能跳到事件调度器那一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseDenial {
	/// 法则 1 + 推导"拒绝关闭要列出每一条事件"：返回的是列表，不是一个"不行"。
	/// 每一项的跳转目标是事件调度器里的那一条（法则 8）。
	Pinned {
		window: WindowId,
		events: Vec<EventId>,
	},
	/// 法则 15/16：`agent`、`share` 之下人类关不掉，哪怕自己的键盘在 normal。
	Owned {
		window: WindowId,
		ai: ActorId,
		mode: OwnershipMode,
	},
	/// 法则 8 + 法则 22：关闭永远不含停止。群聊的关闭是另一个动作（`close_group`），
	/// 它明确地是"停止它的成员"。
	GroupCloseIsStopAction { window: WindowId, group: GroupId },
}
