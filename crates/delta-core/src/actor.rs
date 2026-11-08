//! 人类与 AI 是同一种表面对象的两种客户端。

use crate::ids::{ActorId, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorKind {
	Human,
	/// 一个 ACP 会话 = 一个子进程 = 一个 actor（法则之外的既定结论）。
	Agent,
}

#[derive(Debug)]
pub struct Actor {
	pub id: ActorId,
	pub kind: ActorKind,
	/// 法则 21：每个 AI 有且只有一个"调度器 + 实时流式信息"的窗口。
	pub console: Option<WindowId>,
	/// 法则 22：群聊关闭就是 AI 关闭。
	pub closed: bool,
}

impl Actor {
	pub fn is_human(&self) -> bool {
		self.kind == ActorKind::Human
	}
}
