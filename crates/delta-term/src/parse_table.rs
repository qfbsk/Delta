//! 转写自 `vendor/ghostty-1.4.0/src/terminal/parse_table.zig`（钉住 v1.4.0）。
//!
//! 与上游一样，这张表是生成的：先铺"任意状态"的规则，再铺每个状态自己的规则，
//! 没被覆盖的格子 = 留在原状态、动作 none。上游的重复写入检查在编译期是注释掉的，
//! 这里同样不做运行时检查。

use std::sync::OnceLock;

use crate::parser::{State, TransitionAction};

pub const STATE_COUNT: usize = 14;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Transition {
	pub state: State,
	pub action: TransitionAction,
}

pub fn table() -> &'static [[Transition; STATE_COUNT]; 256] {
	static TABLE: OnceLock<[[Transition; STATE_COUNT]; 256]> = OnceLock::new();
	TABLE.get_or_init(build)
}

fn build() -> [[Transition; STATE_COUNT]; 256] {
	// 默认：留在原状态，动作 none（对应上游 `transition(@fromBackingInt(j), .none)`）。
	let mut result = [[Transition {
		state: State::Ground,
		action: TransitionAction::None,
	}; STATE_COUNT]; 256];
	for (c, row) in result.iter_mut().enumerate() {
		let _ = c;
		for (s, cell) in row.iter_mut().enumerate() {
			cell.state = State::from_index(s);
		}
	}

	// anywhere 规则
	for source in State::ALL {
		single(
			&mut result,
			0x18,
			source,
			State::Ground,
			TransitionAction::Execute,
		);
		single(
			&mut result,
			0x1a,
			source,
			State::Ground,
			TransitionAction::Execute,
		);
		range(
			&mut result,
			0x80,
			0x8f,
			source,
			State::Ground,
			TransitionAction::Execute,
		);
		range(
			&mut result,
			0x91,
			0x97,
			source,
			State::Ground,
			TransitionAction::Execute,
		);
		single(
			&mut result,
			0x99,
			source,
			State::Ground,
			TransitionAction::Execute,
		);
		single(
			&mut result,
			0x9a,
			source,
			State::Ground,
			TransitionAction::Execute,
		);
		single(
			&mut result,
			0x9c,
			source,
			State::Ground,
			TransitionAction::None,
		);

		single(
			&mut result,
			0x1b,
			source,
			State::Escape,
			TransitionAction::None,
		);

		single(
			&mut result,
			0x98,
			source,
			State::SosPmApcString,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x9e,
			source,
			State::SosPmApcString,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x9f,
			source,
			State::SosPmApcString,
			TransitionAction::None,
		);

		single(
			&mut result,
			0x9b,
			source,
			State::CsiEntry,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x90,
			source,
			State::DcsEntry,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x9d,
			source,
			State::OscString,
			TransitionAction::None,
		);
	}

	// ground
	{
		let s = State::Ground;
		single(
			&mut result,
			0x19,
			s,
			State::Ground,
			TransitionAction::Execute,
		);
		range(
			&mut result,
			0x00,
			0x17,
			s,
			State::Ground,
			TransitionAction::Execute,
		);
		range(
			&mut result,
			0x1c,
			0x1f,
			s,
			State::Ground,
			TransitionAction::Execute,
		);
		range(
			&mut result,
			0x20,
			0x7f,
			s,
			State::Ground,
			TransitionAction::Print,
		);
	}

	// escape_intermediate
	{
		let s = State::EscapeIntermediate;
		single(&mut result, 0x19, s, s, TransitionAction::Execute);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Execute);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Execute);
		range(&mut result, 0x20, 0x2f, s, s, TransitionAction::Collect);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);
		range(
			&mut result,
			0x30,
			0x7e,
			s,
			State::Ground,
			TransitionAction::EscDispatch,
		);
	}

	// sos_pm_apc_string
	{
		let s = State::SosPmApcString;
		single(&mut result, 0x19, s, s, TransitionAction::ApcPut);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::ApcPut);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::ApcPut);
		range(&mut result, 0x20, 0x7f, s, s, TransitionAction::ApcPut);
	}

	// escape
	{
		let s = State::Escape;
		single(&mut result, 0x19, s, s, TransitionAction::Execute);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Execute);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Execute);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);

		range(
			&mut result,
			0x30,
			0x4f,
			s,
			State::Ground,
			TransitionAction::EscDispatch,
		);
		range(
			&mut result,
			0x51,
			0x57,
			s,
			State::Ground,
			TransitionAction::EscDispatch,
		);
		range(
			&mut result,
			0x60,
			0x7e,
			s,
			State::Ground,
			TransitionAction::EscDispatch,
		);
		single(
			&mut result,
			0x59,
			s,
			State::Ground,
			TransitionAction::EscDispatch,
		);
		single(
			&mut result,
			0x5a,
			s,
			State::Ground,
			TransitionAction::EscDispatch,
		);
		single(
			&mut result,
			0x5c,
			s,
			State::Ground,
			TransitionAction::EscDispatch,
		);

		range(
			&mut result,
			0x20,
			0x2f,
			s,
			State::EscapeIntermediate,
			TransitionAction::Collect,
		);

		single(
			&mut result,
			0x58,
			s,
			State::SosPmApcString,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x5e,
			s,
			State::SosPmApcString,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x5f,
			s,
			State::SosPmApcString,
			TransitionAction::None,
		);

		single(
			&mut result,
			0x50,
			s,
			State::DcsEntry,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x5b,
			s,
			State::CsiEntry,
			TransitionAction::None,
		);
		single(
			&mut result,
			0x5d,
			s,
			State::OscString,
			TransitionAction::None,
		);
	}

	// dcs_entry
	{
		let s = State::DcsEntry;
		single(&mut result, 0x19, s, s, TransitionAction::Ignore);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Ignore);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Ignore);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);

		range(
			&mut result,
			0x20,
			0x2f,
			s,
			State::DcsIntermediate,
			TransitionAction::Collect,
		);
		single(
			&mut result,
			0x3a,
			s,
			State::DcsIgnore,
			TransitionAction::None,
		);
		range(
			&mut result,
			0x30,
			0x39,
			s,
			State::DcsParam,
			TransitionAction::Param,
		);
		single(
			&mut result,
			0x3b,
			s,
			State::DcsParam,
			TransitionAction::Param,
		);
		range(
			&mut result,
			0x3c,
			0x3f,
			s,
			State::DcsParam,
			TransitionAction::Collect,
		);
		range(
			&mut result,
			0x40,
			0x7e,
			s,
			State::DcsPassthrough,
			TransitionAction::None,
		);
	}

	// dcs_intermediate
	{
		let s = State::DcsIntermediate;
		single(&mut result, 0x19, s, s, TransitionAction::Ignore);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Ignore);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Ignore);
		range(&mut result, 0x20, 0x2f, s, s, TransitionAction::Collect);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);

		range(
			&mut result,
			0x30,
			0x3f,
			s,
			State::DcsIgnore,
			TransitionAction::None,
		);
		range(
			&mut result,
			0x40,
			0x7e,
			s,
			State::DcsPassthrough,
			TransitionAction::None,
		);
	}

	// dcs_ignore
	{
		let s = State::DcsIgnore;
		single(&mut result, 0x19, s, s, TransitionAction::Ignore);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Ignore);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Ignore);
		// 高位字节是载荷，不能触发"任意状态"的 C1 转换（上游注释：0x9B 会中途开一个 CSI）。
		range(&mut result, 0x80, 0xff, s, s, TransitionAction::Ignore);
	}

	// dcs_param
	{
		let s = State::DcsParam;
		single(&mut result, 0x19, s, s, TransitionAction::Ignore);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Ignore);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Ignore);
		range(&mut result, 0x30, 0x39, s, s, TransitionAction::Param);
		single(&mut result, 0x3b, s, s, TransitionAction::Param);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);

		single(
			&mut result,
			0x3a,
			s,
			State::DcsIgnore,
			TransitionAction::None,
		);
		range(
			&mut result,
			0x3c,
			0x3f,
			s,
			State::DcsIgnore,
			TransitionAction::None,
		);
		range(
			&mut result,
			0x20,
			0x2f,
			s,
			State::DcsIntermediate,
			TransitionAction::Collect,
		);
		range(
			&mut result,
			0x40,
			0x7e,
			s,
			State::DcsPassthrough,
			TransitionAction::None,
		);
	}

	// dcs_passthrough
	{
		let s = State::DcsPassthrough;
		single(&mut result, 0x19, s, s, TransitionAction::Put);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Put);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Put);
		range(&mut result, 0x20, 0x7e, s, s, TransitionAction::Put);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);
		// 高位字节是载荷（含 0x9C）：与 osc_string 一致，不能让 C1 规则截断字符串。
		range(&mut result, 0x80, 0xff, s, s, TransitionAction::Put);
	}

	// csi_param
	{
		let s = State::CsiParam;
		single(&mut result, 0x19, s, s, TransitionAction::Execute);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Execute);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Execute);
		range(&mut result, 0x30, 0x39, s, s, TransitionAction::Param);
		single(&mut result, 0x3a, s, s, TransitionAction::Param);
		single(&mut result, 0x3b, s, s, TransitionAction::Param);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);

		range(
			&mut result,
			0x40,
			0x7e,
			s,
			State::Ground,
			TransitionAction::CsiDispatch,
		);
		range(
			&mut result,
			0x3c,
			0x3f,
			s,
			State::CsiIgnore,
			TransitionAction::None,
		);
		range(
			&mut result,
			0x20,
			0x2f,
			s,
			State::CsiIntermediate,
			TransitionAction::Collect,
		);
	}

	// csi_ignore
	{
		let s = State::CsiIgnore;
		single(&mut result, 0x19, s, s, TransitionAction::Execute);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Execute);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Execute);
		range(&mut result, 0x20, 0x3f, s, s, TransitionAction::Ignore);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);
		range(
			&mut result,
			0x40,
			0x7e,
			s,
			State::Ground,
			TransitionAction::None,
		);
	}

	// csi_intermediate
	{
		let s = State::CsiIntermediate;
		single(&mut result, 0x19, s, s, TransitionAction::Execute);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Execute);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Execute);
		range(&mut result, 0x20, 0x2f, s, s, TransitionAction::Collect);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);

		range(
			&mut result,
			0x40,
			0x7e,
			s,
			State::Ground,
			TransitionAction::CsiDispatch,
		);
		range(
			&mut result,
			0x30,
			0x3f,
			s,
			State::CsiIgnore,
			TransitionAction::None,
		);
	}

	// csi_entry
	{
		let s = State::CsiEntry;
		single(&mut result, 0x19, s, s, TransitionAction::Execute);
		range(&mut result, 0x00, 0x17, s, s, TransitionAction::Execute);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Execute);
		single(&mut result, 0x7f, s, s, TransitionAction::Ignore);

		range(
			&mut result,
			0x40,
			0x7e,
			s,
			State::Ground,
			TransitionAction::CsiDispatch,
		);
		single(
			&mut result,
			0x3a,
			s,
			State::CsiIgnore,
			TransitionAction::None,
		);
		range(
			&mut result,
			0x20,
			0x2f,
			s,
			State::CsiIntermediate,
			TransitionAction::Collect,
		);
		range(
			&mut result,
			0x30,
			0x39,
			s,
			State::CsiParam,
			TransitionAction::Param,
		);
		single(
			&mut result,
			0x3b,
			s,
			State::CsiParam,
			TransitionAction::Param,
		);
		range(
			&mut result,
			0x3c,
			0x3f,
			s,
			State::CsiParam,
			TransitionAction::Collect,
		);
	}

	// osc_string
	{
		let s = State::OscString;
		single(&mut result, 0x19, s, s, TransitionAction::Ignore);
		range(&mut result, 0x00, 0x06, s, s, TransitionAction::Ignore);
		range(&mut result, 0x08, 0x17, s, s, TransitionAction::Ignore);
		range(&mut result, 0x1c, 0x1f, s, s, TransitionAction::Ignore);
		range(&mut result, 0x20, 0xff, s, s, TransitionAction::OscPut);
		// BEL 也能结束 OSC（xterm 两者都收）。
		single(&mut result, 0x07, s, State::Ground, TransitionAction::None);
	}

	result
}

fn single(
	table: &mut [[Transition; STATE_COUNT]; 256],
	c: u8,
	from: State,
	to: State,
	action: TransitionAction,
) {
	table[c as usize][from.index()] = Transition { state: to, action };
}

fn range(
	table: &mut [[Transition; STATE_COUNT]; 256],
	from_byte: u8,
	to_byte: u8,
	from: State,
	to: State,
	action: TransitionAction,
) {
	let mut c = from_byte;
	loop {
		single(table, c, from, to, action);
		if c == to_byte {
			break;
		}
		c += 1;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	// 三条都是上游 parse_table.zig 的自带测试（译写）。
	#[test]
	fn dcs_passthrough_high_bytes_are_payload() {
		for c in 0x80..=0xffu8 {
			let entry = table()[c as usize][State::DcsPassthrough.index()];
			assert_eq!(entry.state, State::DcsPassthrough);
			assert_eq!(entry.action, TransitionAction::Put);
		}
	}

	#[test]
	fn dcs_ignore_high_bytes_are_ignored_payload() {
		for c in 0x80..=0xffu8 {
			let entry = table()[c as usize][State::DcsIgnore.index()];
			assert_eq!(entry.state, State::DcsIgnore);
			assert_eq!(entry.action, TransitionAction::Ignore);
		}
	}

	#[test]
	fn dcs_passthrough_still_exits_on_esc_can_and_sub() {
		let esc = table()[0x1b][State::DcsPassthrough.index()];
		assert_eq!(esc.state, State::Escape);
		let can = table()[0x18][State::DcsPassthrough.index()];
		assert_eq!(can.state, State::Ground);
		let sub = table()[0x1a][State::DcsPassthrough.index()];
		assert_eq!(sub.state, State::Ground);
	}
}
