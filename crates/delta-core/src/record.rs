//! 法则 5 的空位：*无论 AI 还是人类，操作都自动 GIT、自动数据库*。
//!
//! 本期"数据库，git，备份权限管理，留好空白。以后再做"，所以这里**只有接口和调用点**：
//! `Delta` 在每个该记账的地方都 append 一行，实现留空。删除之所以可以不显示窗口
//! （法则 46）而仍然安全，靠的就是这一层将来被真正实现——备份先行是硬要求。

use std::path::PathBuf;

use crate::ids::{ActorId, EventId, GroupId, WindowId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
	Edit,
	Delete,
	Takeover,
	Consent,
	EventStart,
	EventStop,
	GroupClose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowSubject {
	Path(PathBuf),
	Window(WindowId),
	Event(EventId),
	Group(GroupId),
}

/// 一行意图记录。真正的 ledger（去重 chunk store、pre/post hash、类别、状态）属于后面那一层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
	pub at: u64,
	pub actor: ActorId,
	pub op: OpKind,
	pub subject: RowSubject,
}

pub trait Recorder {
	fn append(&mut self, row: LedgerRow);
}

/// 本期实现：什么都不做。
#[derive(Debug, Default)]
pub struct NullRecorder;

impl Recorder for NullRecorder {
	fn append(&mut self, _row: LedgerRow) {}
}
