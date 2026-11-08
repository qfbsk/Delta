//! 群聊：一个窗口，同时是一个容器（*"一个工作空间就是一个群"*）。

use std::path::PathBuf;

use crate::ids::{ActorId, GroupId, WindowId};

/// 法则 23：软件里没有"工作区"这一层。`root` 就是群聊开启时选中的那个根路径，
/// 它同时是成员 AI 的 scope root（ACP `session/new` 的 cwd）与文件树的根。
///
/// 法则 17：群组是个特殊窗口，没有这些状态——成员与表决归它，`agent`/`share` 不归它。
#[derive(Debug)]
pub struct Group {
	pub id: GroupId,
	pub root: PathBuf,
	pub window: WindowId,
	pub members: Vec<ActorId>,
	pub open: bool,
}
