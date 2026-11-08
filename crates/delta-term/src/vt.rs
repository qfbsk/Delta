//! 把解析出来的动作落到网格上。
//!
//! 这一版只覆盖"跑通 cmd / pwsh / 一个全屏程序"所需的最小子集：
//! 光标移动、擦除、自动换行、滚动。**还没做**：备用屏（1049/47/1047）、滚动区域、
//! SGR 属性、制表位设置。没有备用屏就跑不起 nvim，所以它是下一块，不是可以忘的一部分。

use crate::grid::Grid;
use crate::parser::{Action, Csi, Parser};

#[derive(Debug)]
pub struct Vt {
	parser: Parser,
	grid: Grid,
}

impl Vt {
	pub fn new(cols: usize, rows: usize) -> Self {
		Self::with_limits(cols, rows, crate::grid::DEFAULT_SCROLLBACK_BYTES)
	}

	pub fn with_limits(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
		Vt {
			parser: Parser::new(),
			grid: Grid::with_limits(cols, rows, scrollback_limit),
		}
	}

	pub fn grid(&self) -> &Grid {
		&self.grid
	}

	pub fn feed(&mut self, bytes: &[u8]) {
		let mut actions = Vec::new();
		self.parser.feed(bytes, &mut actions);
		for action in actions {
			self.apply(action);
		}
	}

	fn apply(&mut self, action: Action) {
		match action {
			Action::Print(ch) => self.grid.put_char(ch),
			Action::C0(byte) => match byte {
				0x08 => self.grid.backspace(),
				0x09 => self.grid.tab(),
				0x0a | 0x0b | 0x0c => self.grid.line_feed(),
				0x0d => self.grid.carriage_return(),
				_ => {}
			},
			Action::Csi(csi) => self.apply_csi(csi),
			// OSC 0/2 是窗口标题，属于 chrome 的事实，不归网格。
			Action::Osc(_) => {}
		}
	}

	fn apply_csi(&mut self, csi: Csi) {
		let n = |index: usize| -> usize { param(&csi, index) as usize };
		match csi.final_byte {
			b'H' | b'f' => self.grid.move_to(n(0) - 1, n(1) - 1),
			b'A' => self.grid.cursor_up(n(0)),
			b'B' => self.grid.cursor_down(n(0)),
			b'C' => self.grid.cursor_right(n(0)),
			b'D' => self.grid.cursor_left(n(0)),
			b'E' => {
				self.grid.cursor_down(n(0));
				self.grid.carriage_return();
			}
			b'F' => {
				self.grid.cursor_up(n(0));
				self.grid.carriage_return();
			}
			b'G' => self.grid.move_to(self.grid.cursor.row, n(0) - 1),
			b'd' => self.grid.move_to(n(0) - 1, self.grid.cursor.col),
			b'J' => self.grid.erase_display(mode(&csi)),
			b'K' => self.grid.erase_line(mode(&csi)),
			// SGR 只解析不落格子：颜色、粗体等属性属于后面的切片。
			b'm' => {}
			// `CSI ? 1049 h/l` 等私有模式需要备用屏，下一块做；现在明确吞掉而不是误做。
			b'h' | b'l' => {}
			_ => {}
		}
	}
}

/// 光标类命令的默认值是 1，`0` 也按 1 读（xterm 的行为）。
fn param(csi: &Csi, index: usize) -> u16 {
	match csi.params.get(index) {
		Some(0) | None => 1,
		Some(value) => *value,
	}
}

/// 擦除类命令的默认值是 0，`0` 是有效值。
fn mode(csi: &Csi) -> u16 {
	csi.params.first().copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn plain_text_lands_in_the_grid() {
		let mut vt = Vt::new(10, 3);
		vt.feed(b"hi");
		assert_eq!(vt.grid().view(), vec!["hi"]);
	}

	#[test]
	fn cr_lf_moves_to_a_new_line() {
		let mut vt = Vt::new(10, 3);
		vt.feed(b"ab\r\ncd");
		assert_eq!(vt.grid().view(), vec!["ab", "cd"]);
	}

	#[test]
	fn wrap_happens_on_the_next_character_not_at_the_edge() {
		let mut vt = Vt::new(3, 2);
		vt.feed(b"abc");
		assert_eq!(vt.grid().view(), vec!["abc"]);
		vt.feed(b"d");
		assert_eq!(vt.grid().view(), vec!["abc", "d"]);
	}

	#[test]
	fn a_sequence_split_across_reads_is_still_one_sequence() {
		let mut vt = Vt::new(6, 2);
		vt.feed(b"\x1b[");
		vt.feed(b"1;3");
		vt.feed(b"Hx");
		assert_eq!(vt.grid().view(), vec!["  x"]);
	}

	#[test]
	fn utf8_split_across_reads_prints_once() {
		let mut vt = Vt::new(6, 2);
		let bytes = "界".as_bytes();
		vt.feed(&bytes[..2]);
		vt.feed(&bytes[2..]);
		assert_eq!(vt.grid().view(), vec!["界"]);
	}

	#[test]
	fn a_wide_char_at_the_last_column_wraps_before_printing() {
		let mut vt = Vt::new(3, 2);
		vt.feed(b"ab");
		vt.feed("界".as_bytes());
		assert_eq!(vt.grid().view(), vec!["ab", "界"]);
	}

	#[test]
	fn erase_display_two_clears_the_screen_and_leaves_the_cursor_where_it_was() {
		let mut vt = Vt::new(6, 2);
		vt.feed(b"abc\r\ndef\x1b[2J");
		assert!(vt.grid().view().is_empty(), "{:?}", vt.grid().view());
		vt.feed(b"\x1b[Hx");
		assert_eq!(vt.grid().view(), vec!["x"]);
	}

	#[test]
	fn erase_line_zero_clears_from_the_cursor() {
		let mut vt = Vt::new(6, 1);
		vt.feed(b"abcdef\x1b[3G\x1b[K");
		assert_eq!(vt.grid().view(), vec!["ab"]);
	}

	#[test]
	fn scrolling_at_the_bottom_pushes_a_line_into_scrollback() {
		let mut vt = Vt::new(3, 2);
		vt.feed(b"abc\r\ndef\r\nghi");
		assert_eq!(vt.grid().view(), vec!["def", "ghi"]);
		assert_eq!(vt.grid().scrollback_len(), 1);
		assert_eq!(vt.grid().scrollback_row(0).unwrap().text(), "abc");
	}

	#[test]
	fn scrollback_respects_its_byte_budget() {
		// 每行 "abc" + 换行 = 4 字节，预算 10 字节，所以只留最后两行。
		let mut vt = Vt::with_limits(3, 1, 10);
		vt.feed(b"aaa\r\nbbb\r\nccc\r\nddd\r\n");
		assert!(vt.grid().scrollback_bytes() <= 10);
		assert_eq!(vt.grid().scrollback_len(), 2);
		assert_eq!(vt.grid().scrollback_row(0).unwrap().text(), "ccc");
		assert_eq!(vt.grid().scrollback_row(1).unwrap().text(), "ddd");
	}

	#[test]
	fn osc_is_collected_and_not_drawn() {
		let mut vt = Vt::new(6, 1);
		vt.feed(b"\x1b]0;a title\x07x");
		assert_eq!(vt.grid().view(), vec!["x"]);
	}
}
