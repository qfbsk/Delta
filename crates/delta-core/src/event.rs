//! 事件：法则 8 的"无窗口的事件不存在"，法则 10 的"output 只绑一个窗口"。

use std::path::PathBuf;

use crate::ids::{ActorId, EventId, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventState {
	Running,
	Finished,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
	/// 一次编译、一条 `gcc -o` 之类：跑在某个 pane 里（推导"delta 只承认窗口里的执行"）。
	Program { argv: Vec<String>, cwd: PathBuf },
	/// AI 正在做的事，按它自己的说法记一条标签；权威不来自这句话。
	/// 法则：pin 数的是事件行，不是 `done`。
	Task { label: String },
}

#[derive(Debug)]
pub struct Event {
	pub id: EventId,
	pub ai: ActorId,
	pub output: WindowId,
	pub state: EventState,
	pub subject: Subject,
}

impl Event {
	pub fn is_live(&self) -> bool {
		self.state == EventState::Running
	}
}
