//! 转写自 `vendor/ghostty-1.4.0/src/terminal/Parser.zig`（钉住 v1.4.0）。
//!
//! 上游头注释：*"This is implemented directly as the state machine described on
//! vt100.net: https://vt100.net/emu/dec_ansi_parser"*。这里逐条对照转写：
//! `next()` 仍然一次返回三个动作（退出旧状态 / 转换动作 / 进入新状态）。
//!
//! **一处有意偏离**：上游在解析器内部用 `osc.Parser` 把 OSC 字符串解析成带类型的命令
//! （`osc.zig`，1320 行），这一版只把原始载荷交出去，`osc.zig` 的转写是下一片。

use crate::parse_table::table;

pub const MAX_INTERMEDIATE: usize = 4;
pub const MAX_PARAMS: usize = 24;

/// 状态机（顺序与上游枚举一致）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
	Ground,
	Escape,
	EscapeIntermediate,
	CsiEntry,
	CsiIntermediate,
	CsiParam,
	CsiIgnore,
	DcsEntry,
	DcsParam,
	DcsIntermediate,
	DcsPassthrough,
	DcsIgnore,
	OscString,
	SosPmApcString,
}

impl State {
	pub const ALL: [State; 14] = [
		State::Ground,
		State::Escape,
		State::EscapeIntermediate,
		State::CsiEntry,
		State::CsiIntermediate,
		State::CsiParam,
		State::CsiIgnore,
		State::DcsEntry,
		State::DcsParam,
		State::DcsIntermediate,
		State::DcsPassthrough,
		State::DcsIgnore,
		State::OscString,
		State::SosPmApcString,
	];

	pub fn index(self) -> usize {
		self as usize
	}

	pub fn from_index(index: usize) -> State {
		State::ALL[index]
	}
}

/// 表里的转换动作：解析器内部用，不交给调用方。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransitionAction {
	None,
	Ignore,
	Print,
	Execute,
	Collect,
	Param,
	EscDispatch,
	CsiDispatch,
	Put,
	OscPut,
	ApcPut,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Csi {
	pub intermediates: [u8; MAX_INTERMEDIATE],
	pub intermediates_len: u8,
	pub params: [u16; MAX_PARAMS],
	pub params_len: u8,
	/// 第 i 位为 1 表示第 i 个参数后面跟的是 `:`（SGR 的颜色子参数）。
	pub params_sep: u32,
	pub final_byte: u8,
}

impl Csi {
	pub fn intermediates(&self) -> &[u8] {
		&self.intermediates[..self.intermediates_len as usize]
	}

	pub fn params(&self) -> &[u16] {
		&self.params[..self.params_len as usize]
	}

	pub fn sep_is_colon(&self, index: usize) -> bool {
		self.params_sep & (1 << index) != 0
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Esc {
	pub intermediates: [u8; MAX_INTERMEDIATE],
	pub intermediates_len: u8,
	pub final_byte: u8,
}

impl Esc {
	pub fn intermediates(&self) -> &[u8] {
		&self.intermediates[..self.intermediates_len as usize]
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DcsHook {
	pub intermediates: [u8; MAX_INTERMEDIATE],
	pub intermediates_len: u8,
	pub params: [u16; MAX_PARAMS],
	pub params_len: u8,
	pub final_byte: u8,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
	/// 一个字符（调用方已经做过 UTF-8 解码，见 `utf8.rs` / `stream.rs`）。
	Print(char),
	/// C0 或 C1 控制字符。
	Execute(u8),
	CsiDispatch(Csi),
	EscDispatch(Esc),
	OscDispatch(Vec<u8>),
	DcsHook(DcsHook),
	DcsPut(u8),
	DcsUnhook,
	ApcStart,
	ApcPut(u8),
	ApcEnd,
}

#[derive(Debug)]
pub struct Parser {
	state: State,
	intermediates: [u8; MAX_INTERMEDIATE],
	intermediates_idx: u8,
	params: [u16; MAX_PARAMS],
	params_sep: u32,
	params_idx: u8,
	param_acc: u16,
	param_acc_idx: u8,
	/// 见模块头：这一版只收原始 OSC 载荷。
	osc: Vec<u8>,
}

impl Default for Parser {
	fn default() -> Self {
		Self::new()
	}
}

impl Parser {
	pub fn new() -> Self {
		Parser {
			state: State::Ground,
			intermediates: [0; MAX_INTERMEDIATE],
			intermediates_idx: 0,
			params: [0; MAX_PARAMS],
			params_sep: 0,
			params_idx: 0,
			param_acc: 0,
			param_acc_idx: 0,
			osc: Vec::new(),
		}
	}

	pub fn state(&self) -> State {
		self.state
	}

	/// 与上游 `next` 同形：最多三个动作，按顺序执行。
	pub fn next(&mut self, c: u8) -> [Option<Action>; 3] {
		let effect = table()[c as usize][self.state.index()];
		let next_state = effect.state;

		let exit = if self.state == next_state {
			None
		} else {
			match self.state {
				State::OscString => Some(Action::OscDispatch(std::mem::take(&mut self.osc))),
				State::DcsPassthrough => Some(Action::DcsUnhook),
				State::SosPmApcString => Some(Action::ApcEnd),
				_ => None,
			}
		};

		let transition = self.do_action(effect.action, c);

		let entry = if self.state == next_state {
			None
		} else {
			match next_state {
				State::Escape | State::DcsEntry | State::CsiEntry => {
					self.clear();
					None
				}
				State::OscString => {
					self.osc.clear();
					None
				}
				State::DcsPassthrough => {
					if self.params_idx as usize >= MAX_PARAMS {
						None
					} else {
						if self.param_acc_idx > 0 {
							self.params[self.params_idx as usize] = self.param_acc;
							self.params_idx += 1;
						}
						Some(Action::DcsHook(DcsHook {
							intermediates: self.intermediates,
							intermediates_len: self.intermediates_idx,
							params: self.params,
							params_len: self.params_idx,
							final_byte: c,
						}))
					}
				}
				State::SosPmApcString => Some(Action::ApcStart),
				_ => None,
			}
		};

		self.state = next_state;
		[exit, transition, entry]
	}

	fn collect(&mut self, c: u8) {
		if self.intermediates_idx as usize >= MAX_INTERMEDIATE {
			return;
		}
		self.intermediates[self.intermediates_idx as usize] = c;
		self.intermediates_idx += 1;
	}

	fn do_action(&mut self, action: TransitionAction, c: u8) -> Option<Action> {
		match action {
			TransitionAction::None | TransitionAction::Ignore => None,
			TransitionAction::Print => Some(Action::Print(c as char)),
			TransitionAction::Execute => Some(Action::Execute(c)),
			TransitionAction::Collect => {
				self.collect(c);
				None
			}
			TransitionAction::Param => {
				if c == b';' || c == b':' {
					if self.params_idx as usize >= MAX_PARAMS {
						return None;
					}
					self.params[self.params_idx as usize] = self.param_acc;
					if c == b':' {
						self.params_sep |= 1 << self.params_idx;
					}
					self.params_idx += 1;
					self.param_acc = 0;
					self.param_acc_idx = 0;
					return None;
				}
				self.param_acc = self
					.param_acc
					.saturating_mul(10)
					.saturating_add((c - b'0') as u16);
				self.param_acc_idx = self.param_acc_idx.saturating_add(1);
				None
			}
			TransitionAction::OscPut => {
				if self.osc.len() < 4096 {
					self.osc.push(c);
				}
				None
			}
			TransitionAction::CsiDispatch => {
				if self.params_idx as usize >= MAX_PARAMS {
					return None;
				}
				if self.param_acc_idx > 0 {
					self.params[self.params_idx as usize] = self.param_acc;
					self.params_idx += 1;
				}
				let dispatch = Csi {
					intermediates: self.intermediates,
					intermediates_len: self.intermediates_idx,
					params: self.params,
					params_len: self.params_idx,
					params_sep: self.params_sep,
					final_byte: c,
				};
				// 上游：只有 'm' 允许冒号/混合分隔符。
				if c != b'm' && self.params_sep != 0 {
					return None;
				}
				Some(Action::CsiDispatch(dispatch))
			}
			TransitionAction::EscDispatch => Some(Action::EscDispatch(Esc {
				intermediates: self.intermediates,
				intermediates_len: self.intermediates_idx,
				final_byte: c,
			})),
			TransitionAction::Put => Some(Action::DcsPut(c)),
			TransitionAction::ApcPut => Some(Action::ApcPut(c)),
		}
	}

	pub fn clear(&mut self) {
		self.intermediates_idx = 0;
		self.params_idx = 0;
		self.params_sep = 0;
		self.param_acc = 0;
		self.param_acc_idx = 0;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// 以下测试转写自 Parser.zig 同名的 `test "..."` 块。

	#[test]
	fn sos_pm_apc_string_is_entered_and_left() {
		let mut p = Parser::new();
		let _ = p.next(0x9e);
		assert_eq!(p.state(), State::SosPmApcString);
		let _ = p.next(0x9c);
		assert_eq!(p.state(), State::Ground);

		let a = p.next(b'a');
		assert_eq!(p.state(), State::Ground);
		assert!(a[0].is_none());
		assert_eq!(a[1], Some(Action::Print('a')));
		assert!(a[2].is_none());

		let a = p.next(0x19);
		assert_eq!(p.state(), State::Ground);
		assert!(a[0].is_none());
		assert_eq!(a[1], Some(Action::Execute(0x19)));
		assert!(a[2].is_none());
	}

	#[test]
	fn esc_open_paren_b() {
		let mut p = Parser::new();
		let _ = p.next(0x1b);
		let _ = p.next(b'(');
		let a = p.next(b'B');
		assert_eq!(p.state(), State::Ground);
		match &a[1] {
			Some(Action::EscDispatch(d)) => {
				assert_eq!(d.final_byte, b'B');
				assert_eq!(d.intermediates(), b"(");
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn csi_h_without_params() {
		let mut p = Parser::new();
		let _ = p.next(0x1b);
		let _ = p.next(b'[');
		let a = p.next(b'H');
		assert_eq!(p.state(), State::Ground);
		match &a[1] {
			Some(Action::CsiDispatch(d)) => {
				assert_eq!(d.final_byte, b'H');
				assert!(d.params().is_empty());
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn csi_one_semicolon_four_h() {
		let mut p = Parser::new();
		for c in b"\x1b[1;4" {
			let _ = p.next(*c);
		}
		let a = p.next(b'H');
		match &a[1] {
			Some(Action::CsiDispatch(d)) => {
				assert_eq!(d.params(), &[1, 4]);
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn sgr_38_colon_2_m_keeps_the_colon_separator() {
		let mut p = Parser::new();
		for c in b"\x1b[38:2" {
			let _ = p.next(*c);
		}
		let a = p.next(b'm');
		match &a[1] {
			Some(Action::CsiDispatch(d)) => {
				assert_eq!(d.params(), &[38, 2]);
				assert!(d.sep_is_colon(0));
				assert!(!d.sep_is_colon(1));
			}
			other => panic!("{other:?}"),
		}
	}

	#[test]
	fn colon_separators_are_dropped_for_non_m_finals() {
		let mut p = Parser::new();
		for c in b"\x1b[48:2" {
			let _ = p.next(*c);
		}
		let a = p.next(b'm');
		assert!(matches!(a[1], Some(Action::CsiDispatch(_))));

		let mut p = Parser::new();
		for c in b"\x1b[1:2" {
			let _ = p.next(*c);
		}
		let a = p.next(b'H');
		assert!(a[1].is_none(), "非 m 的 final 不允许冒号分隔：{a:?}");
	}

	#[test]
	fn esc_restarts_a_sequence_instead_of_leaking_it() {
		let mut p = Parser::new();
		for c in b"\x1b[1" {
			let _ = p.next(*c);
		}
		let _ = p.next(0x1b);
		let _ = p.next(b'[');
		let a = p.next(b'K');
		assert!(matches!(a[1], Some(Action::CsiDispatch(_))));
	}
}
