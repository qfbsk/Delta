//! 法则 16 的另一半主语：输入模式属于键盘，不属于窗口。

use crate::ids::WindowId;
use crate::window::KeymapOwner;

/// `normal` 是快捷模式（法则 13）：键盘在 chrome 手里。
/// `insert` 是"键原样交给 pane"，`replace` 就是 insert + overtype。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
	/// 默认与快捷模式（法则 13）。
	#[default]
	Normal,
	Insert,
	Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Esc {
	/// 键盘本来就在 chrome 手里，ESC 无事可做。
	AlreadyNormal,
	/// delta 自己的键映射（miku / chrome）：ESC 回到 normal。
	ToNormal,
	/// 键已经交给外来全屏程序：ESC 是那个程序的字节，chrome 的退出键在那里是 prefix。
	GivenToApp,
}

#[derive(Debug, Default)]
pub struct Keyboard {
	pub focused: Option<WindowId>,
	pub input: InputMode,
}

impl Keyboard {
	/// ESC 的判据来自"谁的键映射"，不需要等待计时（`escape-time` 不抄）。
	pub fn esc(&mut self, keymap: KeymapOwner) -> Esc {
		if self.input == InputMode::Normal {
			return Esc::AlreadyNormal;
		}
		match keymap {
			KeymapOwner::Delta => {
				self.input = InputMode::Normal;
				Esc::ToNormal
			}
			KeymapOwner::ForeignApp => Esc::GivenToApp,
		}
	}

	pub fn focus(&mut self, window: WindowId) {
		self.focused = Some(window);
	}
}
