//! 网格：一个 pane 的字符层。
//!
//! 这一层是 `laws/delta-reasoning.md` 的 *First runnable proof* 段说的那个地基：
//! cmd / pwsh / vim 要在分屏里跑通、每个键和 IME 都对，之后才轮到 AI 层。
//! 所以这里只做字节到格子的还原，不认识窗口、事件、群聊。

/// 一个格子。`width == 0` 是宽字符占掉的第二格。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
	pub ch: char,
	pub width: u8,
}

impl Cell {
	pub const BLANK: Cell = Cell { ch: ' ', width: 1 };

	fn glyph(ch: char, width: u8) -> Self {
		Cell { ch, width }
	}

	fn spacer() -> Self {
		Cell { ch: ' ', width: 0 }
	}
}

/// 一个变长字体的格子宽度：0 个组合符、1 个普通字符、2 个全角/emoji。
///
/// 这份区间表是第一版，conformance 语料（`references/` 里的 vttest / esctest2 方向）
/// 会在它上面挑错——写在这里是为了将来能改，而不是假装它已经完整。
pub fn char_width(ch: char) -> u8 {
	let c = ch as u32;
	if ch.is_control() {
		return 0;
	}
	if (0x0300..=0x036F).contains(&c)
		|| (0x1AB0..=0x1AFF).contains(&c)
		|| (0x20D0..=0x20FF).contains(&c)
	{
		return 0;
	}
	let wide = (0x1100..=0x115F).contains(&c)
		|| (0x2E80..=0x303E).contains(&c)
		|| (0x3041..=0x33FF).contains(&c)
		|| (0x3400..=0x4DBF).contains(&c)
		|| (0x4E00..=0x9FFF).contains(&c)
		|| (0xA000..=0xA4CF).contains(&c)
		|| (0xAC00..=0xD7A3).contains(&c)
		|| (0xF900..=0xFAFF).contains(&c)
		|| (0xFE30..=0xFE4F).contains(&c)
		|| (0xFF00..=0xFF60).contains(&c)
		|| (0xFFE0..=0xFFE6).contains(&c)
		|| (0x1F300..=0x1FAFF).contains(&c)
		|| (0x20000..=0x3FFFD).contains(&c);
	if wide { 2 } else { 1 }
}

/// herdr 的 `scrollback_limit_bytes = 10000000`，注释写明 *"Matches Ghostty's default
/// scrollback-limit behavior"*。一个字节预算同时管住"不画面上的后台 pane"和窗口睡眠时
/// 还在长的 scrollback（`laws/window-ownership.md` 的推导需要这个上限）。
pub const DEFAULT_SCROLLBACK_BYTES: usize = 10_000_000;

#[derive(Debug, Clone)]
pub struct Row {
	cells: Vec<Cell>,
}

impl Row {
	fn blank(cols: usize) -> Self {
		Row {
			cells: vec![Cell::BLANK; cols],
		}
	}

	pub fn len(&self) -> usize {
		self.cells.len()
	}

	pub fn is_empty(&self) -> bool {
		self.cells.is_empty()
	}

	pub fn cell(&self, col: usize) -> Option<&Cell> {
		self.cells.get(col)
	}

	/// 去掉行尾空白后的文本——测试与"给 AI 的有界视图"都要它。
	pub fn text(&self) -> String {
		let mut out = String::new();
		for cell in &self.cells {
			if cell.width == 0 {
				continue;
			}
			if cell.ch == '\0' {
				continue;
			}
			out.push(cell.ch);
		}
		out.trim_end().to_string()
	}

	fn bytes(&self) -> usize {
		self.cells
			.iter()
			.map(|c| if c.width == 0 { 0 } else { c.ch.len_utf8() })
			.sum()
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
	pub row: usize,
	pub col: usize,
}

#[derive(Debug)]
pub struct Grid {
	cols: usize,
	rows: usize,
	lines: Vec<Row>,
	/// 顶部滚出去的历史行，越新越靠后。
	scrollback: Vec<Row>,
	scrollback_bytes: usize,
	scrollback_limit: usize,
	pub cursor: Cursor,
	/// VT 的"待换行"：走到最后一列时并不立刻换行，下一个字符才换。
	wrap_pending: bool,
}

impl Grid {
	pub fn new(cols: usize, rows: usize) -> Self {
		Self::with_limits(cols, rows, DEFAULT_SCROLLBACK_BYTES)
	}

	pub fn with_limits(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
		assert!(cols > 0 && rows > 0);
		Grid {
			cols,
			rows,
			lines: vec![Row::blank(cols); rows],
			scrollback: Vec::new(),
			scrollback_bytes: 0,
			scrollback_limit,
			cursor: Cursor { row: 0, col: 0 },
			wrap_pending: false,
		}
	}

	pub fn cols(&self) -> usize {
		self.cols
	}

	pub fn rows(&self) -> usize {
		self.rows
	}

	pub fn row(&self, row: usize) -> Option<&Row> {
		self.lines.get(row)
	}

	pub fn view(&self) -> Vec<String> {
		let mut out: Vec<String> = self.lines.iter().map(Row::text).collect();
		while out.last().is_some_and(|line| line.is_empty()) {
			out.pop();
		}
		out
	}

	pub fn scrollback_len(&self) -> usize {
		self.scrollback.len()
	}

	pub fn scrollback_bytes(&self) -> usize {
		self.scrollback_bytes
	}

	pub fn scrollback_row(&self, index: usize) -> Option<&Row> {
		self.scrollback.get(index)
	}

	fn blank_row(&self) -> Row {
		Row::blank(self.cols)
	}

	fn clear_wrap(&mut self) {
		self.wrap_pending = false;
	}

	pub fn put_char(&mut self, ch: char) {
		let width = char_width(ch);
		if width == 0 {
			return;
		}
		if self.wrap_pending {
			self.carriage_return();
			self.line_feed();
		}
		// 宽字符在最后一列放不下：先换行，与 xterm 的自动换行一致。
		if width as usize > self.cols - self.cursor.col {
			self.carriage_return();
			self.line_feed();
		}
		let col = self.cursor.col;
		let row = self.cursor.row;
		let width = width.min((self.cols - col) as u8).max(1);
		self.lines[row].cells[col] = Cell::glyph(ch, width);
		if width == 2 {
			self.lines[row].cells[col + 1] = Cell::spacer();
		}
		let next = col + width as usize;
		if next >= self.cols {
			self.cursor.col = self.cols - 1;
			self.wrap_pending = true;
		} else {
			self.cursor.col = next;
		}
	}

	pub fn carriage_return(&mut self) {
		self.cursor.col = 0;
		self.clear_wrap();
	}

	/// 换行；在最后一行时把顶行推进 scrollback（带字节预算）。
	pub fn line_feed(&mut self) {
		self.clear_wrap();
		if self.cursor.row + 1 < self.rows {
			self.cursor.row += 1;
			return;
		}
		let top = self.lines.remove(0);
		let blank = self.blank_row();
		self.lines.push(blank);
		self.push_scrollback(top);
	}

	fn push_scrollback(&mut self, row: Row) {
		let bytes = row.bytes() + 1; // 行本身 + 换行
		self.scrollback_bytes += bytes;
		self.scrollback.push(row);
		while self.scrollback_bytes > self.scrollback_limit && !self.scrollback.is_empty() {
			let dropped = self.scrollback[0].bytes() + 1;
			self.scrollback_bytes = self.scrollback_bytes.saturating_sub(dropped);
			self.scrollback.remove(0);
		}
	}

	pub fn backspace(&mut self) {
		self.clear_wrap();
		if self.cursor.col > 0 {
			self.cursor.col -= 1;
		}
	}

	/// 制表位每 8 列；最后停在本行最后一列。
	pub fn tab(&mut self) {
		self.clear_wrap();
		let next = (self.cursor.col / 8 + 1) * 8;
		self.cursor.col = next.min(self.cols - 1);
	}

	pub fn move_to(&mut self, row: usize, col: usize) {
		self.clear_wrap();
		self.cursor.row = row.min(self.rows - 1);
		self.cursor.col = col.min(self.cols - 1);
	}

	pub fn cursor_up(&mut self, n: usize) {
		self.clear_wrap();
		self.cursor.row = self.cursor.row.saturating_sub(n);
	}

	pub fn cursor_down(&mut self, n: usize) {
		self.clear_wrap();
		self.cursor.row = (self.cursor.row + n).min(self.rows - 1);
	}

	pub fn cursor_right(&mut self, n: usize) {
		self.clear_wrap();
		self.cursor.col = (self.cursor.col + n).min(self.cols - 1);
	}

	pub fn cursor_left(&mut self, n: usize) {
		self.clear_wrap();
		self.cursor.col = self.cursor.col.saturating_sub(n);
	}

	/// `CSI J`
	pub fn erase_display(&mut self, mode: u16) {
		self.clear_wrap();
		let row = self.cursor.row;
		match mode {
			0 => {
				self.erase_line(0);
				for r in row + 1..self.rows {
					self.lines[r] = self.blank_row();
				}
			}
			1 => {
				self.erase_line(1);
				for r in 0..row {
					self.lines[r] = self.blank_row();
				}
			}
			// 3 是"连 scrollback 一起清"，这里只清屏；清 scrollback 由调用方决定。
			_ => {
				for r in 0..self.rows {
					self.lines[r] = self.blank_row();
				}
			}
		}
	}

	pub fn clear_scrollback(&mut self) {
		self.scrollback.clear();
		self.scrollback_bytes = 0;
	}

	/// `CSI K`
	pub fn erase_line(&mut self, mode: u16) {
		self.clear_wrap();
		let (row, col) = (self.cursor.row, self.cursor.col);
		match mode {
			0 => {
				for c in col..self.cols {
					self.lines[row].cells[c] = Cell::BLANK;
				}
			}
			1 => {
				for c in 0..=col {
					self.lines[row].cells[c] = Cell::BLANK;
				}
			}
			_ => {
				self.lines[row] = self.blank_row();
			}
		}
	}
}
